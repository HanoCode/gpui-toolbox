//! SQL 插件族（`duck.sql.*`）的 **process 轨入口**。
//!
//! 一个二进制服务整族工具：宿主在请求里给 `tool`，这里按 id 分派到
//! [`duck_sql_core`]——所以族内新增一个工具只需要加一条 `match` 分支与一份
//! `plugin.toml`，不必再产一个新可执行文件。
//!
//! 逻辑全部来自参考仓已验证的实现（见 `duck-sql-core` 各模块头），本文件只做
//! 三件事：解析 `[io]` 参数、把前端那一层编排（例如「有 `${num}` 才分表」）
//! 照搬过来、把产出写成 JSONL 事件。
//!
//! 文案走插件自己的 `locales/`：宿主通过 [`duck_plugin::ENV_LOCALES_DIR`] 把
//! 目录传进来，与 JS 轨共用同一套「插件表 → 源串」降级，不额外发明一层。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use duck_plugin::PluginLocales;
use duck_plugin_process::{PluginOutput, RunRequest, serve};
use duck_sql_core::ddl_generator;
use duck_sql_core::sql_checker::{self, CheckConfig, CheckReport, Severity};
use duck_sql_core::sql_merge::{self, MergeReporter, SqlMergeOptions, SqlMergeResult};

mod i18n;

use i18n::Renderer;

/// 参数区：宿主把 `[io]` 表单收集到的值序列化成**字符串**键值对
/// （数值与开关也是字符串，见 `tool_run.rs::params_json`）。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    /// 取字符串；数字也接受（容错第三方调用方直接塞 JSON 数字）。
    fn text(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.trim().to_owned(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    fn number(&self, key: &str) -> Option<f64> {
        self.text(key).parse::<f64>().ok()
    }

    /// 开关：`"true"`（宿主）/ `true`（第三方 JSON）都算开。
    fn flag(&self, key: &str) -> bool {
        self.text(key).eq_ignore_ascii_case("true")
    }
}

/// 取词器：`locales/` 由宿主通过环境变量指路，缺失时原样返回中文源串。
struct Text {
    locales: PluginLocales,
    lang: String,
}

impl Text {
    fn load() -> Self {
        let lang = std::env::var(duck_plugin::ENV_LOCALE).unwrap_or_else(|_| "zh-Hans".to_owned());
        // 宿主给的 `DUCK_LOCALES_DIR` **就是** `locales/` 这一级：走 `load` 会把
        // `locales` 再拼一次，读不到任何文件（英文界面静默显示中文）。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    fn get(&self, source: &str) -> String {
        self.locales.text(&self.lang, source).to_owned()
    }

    /// 把 core 产出的**已插值**中文串翻回去（带 `{}` 的走模板反查）。
    fn renderer(&self) -> Renderer<'_> {
        Renderer::new(&self.locales, &self.lang)
    }
}

fn main() -> ExitCode {
    serve(handle)
}

fn handle(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
) -> Result<String, duck_plugin::PluginError> {
    let text = Text::load();
    match request.tool.as_str() {
        "duck.sql.ddl-generator" => ddl_generate(request, out, &text),
        "duck.sql.script-check" => script_check(request, out, &text),
        "duck.sql.sql-merge" => sql_merge_run(request, out, &text),
        "duck.sql.sql-classifier" => sql_classifier_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-sql-tool: unknown tool {other}"
        ))),
    }
}

// ── SQL 合并 ────────────────────────────────────────────────────────────

/// 合并过程回执：一路写插件事件流，一路攒成给人看的日志。
///
/// 两处重复是**有意**的：事件流是给宿主的 `log`/`progress` 渲染器用的（`duck-app`
/// 的 `ProgressSink` 还接着 `NullSink`，见 ADR-3 遗留），而这次的 `result` 正文
/// 必须自带过程日志——否则用户在「执行中…」之后什么也看不到。等宿主接上
/// log 渲染器，删掉 `lines` 这一份即可，事件那一份不用动。
struct MergeEvents<'a, 'b> {
    out: RefCell<&'a mut PluginOutput<std::io::Stdout>>,
    text: &'b Text,
    lines: RefCell<Vec<String>>,
}

impl MergeReporter for MergeEvents<'_, '_> {
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        current: usize,
        total: usize,
        _current_file: Option<String>,
    ) {
        let message = format!("{} {}%", stage_label(stage, self.text), percent);
        self.out.borrow_mut().progress(
            current as u64,
            if total > 0 { Some(total as u64) } else { None },
            Some(&message),
        );
    }

    fn log(&self, level: &str, message: &str, timestamp: String, duration_ms: Option<u64>) {
        // core 产出的日志是**已插值**中文，译文键是模板 ⇒ 走 Renderer 反查。
        let message = self.text.renderer().translate(message);
        let line = match duration_ms {
            Some(millis) => format!(
                "{timestamp} {} {message} ({millis} ms)",
                level.to_uppercase()
            ),
            None => format!("{timestamp} {} {message}", level.to_uppercase()),
        };
        self.out.borrow_mut().log(line.clone());
        self.lines.borrow_mut().push(line);
    }

    fn done(&self, _result: &SqlMergeResult) {
        // 结果由 `merge_sql` 直接返回给本文件，这里不再重复广播一份。
    }
}

/// 阶段名 → 界面文案（源仓 `SQLMerge.vue::stageLabel` 的同一张表）。
fn stage_label(stage: &str, text: &Text) -> String {
    let source = match stage {
        "scanning" => "扫描包目录…",
        "gathering" => "收集 SQL 文件…",
        "grouping" => "分组中…",
        "merging" => "合并中…",
        "scripts" => "复制脚本…",
        "done" => "合并完成",
        // 未知阶段原样透出：源仓前端也是 `map[stage] || stage`。
        other => return other.to_owned(),
    };
    text.get(source)
}

/// SQL 合并。
///
/// 编排照搬参考仓 `SQLMerge.vue::startMerge`：`src_dir` 必填，`customer_no` /
/// `db_type` 空串一律当「不过滤」（源仓传的是 `null`）。输出目录由 core 自己
/// 定在 `<源目录>/merge_sql_<时间戳>`（源仓行为，不改）。
fn sql_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    // 「只看使用说明」：参考页是标题栏上的帮助弹窗（四节长文），执行页没有弹窗
    // 容器，改成一个开关把同一份说明打进结果区——勾上时**不读目录、不写盘**。
    if params.flag("guide") {
        return Ok(merge_guide(text));
    }
    let src_dir = params.text("src_dir");
    if src_dir.is_empty() {
        return Err(fail(text.get("请选择要合并的源目录")));
    }
    let customer_no = params.text("customer_no");
    let db_type = params.text("db_type");
    let options = SqlMergeOptions {
        src_dir,
        // 源仓前端把空输入传成 `null`（= 不过滤），这里同样把空串折成 `None`。
        customer_no: optional(&customer_no).map(str::to_owned),
        db_type: optional(&db_type).map(str::to_owned),
    };
    let events = MergeEvents {
        out: RefCell::new(out),
        text,
        lines: RefCell::new(Vec::new()),
    };
    let result = sql_merge::merge_sql(&events, options)
        .map_err(|error| fail(text.renderer().translate(&error)))?;
    let log = events.lines.borrow().clone();
    Ok(merge_report(&result, text, &log))
}

/// 合并结果拼版：汇总 + 每组明细 + 过程日志（源仓的「统计卡 + 逐源文件表 + 日志面板」）。
fn merge_report(result: &SqlMergeResult, text: &Text, log: &[String]) -> String {
    let stats = &result.statistics;
    let mut out = i18n::fill(
        &text.get("合并完成：{} 个合并组 · {} 条 SQL · {} s"),
        &[
            &stats.merged_groups.len().to_string(),
            &stats.total_sql_count.to_string(),
            &format!("{:.1}", stats.total_duration_secs),
        ],
    );
    out.push('\n');
    out.push_str(&i18n::fill(
        &text.get("输出目录：{}"),
        &[&result.output_dir],
    ));
    out.push('\n');
    out.push_str(&i18n::fill(
        &text.get("扫描到 {} 个版本包"),
        &[&stats.packages.to_string()],
    ));

    for group in &stats.merged_groups {
        out.push_str("\n\n## ");
        out.push_str(&group.group_name);
        out.push('\n');
        out.push_str(&i18n::fill(
            &text.get("{} 个源文件 · {} 条 SQL"),
            &[
                &group.source_files.len().to_string(),
                &group.total_sql_count.to_string(),
            ],
        ));
        out.push('\n');
        out.push_str(&i18n::fill(&text.get("输出：{}"), &[&group.output_path]));
        out.push('\n');
        // 两档都明说：源仓只在失败时给黄标，读起来像「没校验」，而条数核对
        // 恰恰是这个工具唯一的自检手段。不写「= 各源文件之和」——DML 组末尾
        // 会自动补一条 `COMMIT;`，core 的判定允许它多 1 条（见 `match_check`）。
        out.push_str(&if group.match_check {
            i18n::fill(
                &text.get("条数校验：通过（合并后 {} 条）"),
                &[&group.total_sql_count.to_string()],
            )
        } else {
            text.get("条数校验：不一致，请人工核对")
        });
        if !group.source_files.is_empty() {
            out.push('\n');
            out.push_str(&source_table(group, text));
        }
    }

    if !log.is_empty() {
        out.push_str("\n\n");
        out.push_str(&i18n::fill(
            &text.get("处理日志（{} 行）"),
            &[&log.len().to_string()],
        ));
        for line in log {
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

/// 逐源文件明细（源仓右栏那张五列表，这里用 Markdown：贴进工单不用再排）。
fn source_table(group: &sql_merge::MergedGroupInfo, text: &Text) -> String {
    let mut out = format!(
        "| {} | {} | {} | {} | {} |\n|---|---|---|---:|---:|\n",
        text.get("来源包"),
        text.get("源文件"),
        text.get("合并到"),
        text.get("SQL 条数"),
        text.get("大小"),
    );
    for file in &group.source_files {
        out.push_str(&format!(
            "| {} | `{}` | `{}` | {} | {} |\n",
            file.source_package,
            file.source_path,
            file.target_path,
            file.sql_count,
            size_text(file.file_size),
        ));
    }
    out.trim_end().to_string()
}

/// 1024 进制的人读大小（源仓 `formatSize` 同口径：B / KB / MB，1 位小数）。
fn size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if bytes < 1024 * 1024 {
        return format!("{:.1} KB", bytes as f64 / 1024.0);
    }
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// 使用说明（源仓 `SQLMerge.vue` 那个帮助弹窗的四节，压成一段纯文本）。
///
/// 参考页把这份说明放在弹窗里；执行页没有弹窗容器，做成 `guide` 开关打进结果区——
/// 它讲的实质是「目录怎么摆」，能复制、能留存比弹一下就没更有用。
const GUIDE_LINES: &[&str] = &[
    "SQL 文件合并 · 使用说明",
    "",
    "一、它做什么",
    "把多个升级包里的 .sql 按「同一相对目录」分组，按包版本先后追加成一个文件；",
    "文件名含 ddl 的（不分大小写）单独归入 DDL 组。源文件内容原样追加，不做改写。",
    "",
    "二、怎么用",
    "1. 「源目录」选存放多个包的父目录（也可直接选 libs/spsql/ 或含 db-*.zip 的目录）；",
    "2. 「客户号过滤」「数据库类型过滤」可留空；填了就只保留路径命中该串的文件；",
    "3. 执行。结果落在源目录下的 merge_sql_<时间戳>/，同级的 merge.log 是完整日志。",
    "",
    "三、三种目录结构（自动识别，不用选）",
    "标准结构——包目录里有 database/：",
    "- packages/IFMS3.0V202501.08.028/database/spsql/<版本>/base/001_xxx.sql",
    "- packages/IFMS3.0V202503.15.033/database/spsql/<版本>/base/001_xxx.sql",
    "路径里 V202501.08.028 这样的版本段会被忽略，用来对齐不同版本的同一目录。",
    "",
    "SPSQL 库——libs/spsql/ 下每个版本目录：",
    "- libs/spsql/pub/IFMS6.0V202405.04.005/pub/004_xxx.sql",
    "顶层目录（pub/、trans/…）当容器，按「容器/子目录」分组。",
    "",
    "ZIP 库——目录里有 db-*.zip：",
    "- db.zip → pub/、trans/；db-HAXM.zip → HAXM/pub/、HAXM/trans/",
    "解压出的临时文件在合并结束后自动清理。",
    "",
    "四、输出长什么样",
    "- merge_sql_20261018_2340/spsql/base/IFMS3.0V202501.08.028～IFMS3.0V202503.15.033.sql",
    "文件名是「起始包～终止包」；每组都会核对「合并后条数 = 各源文件条数之和」，",
    "不一致时单独提示，交付前请人工核对。",
];

/// 渲染使用说明：逐行取词（空行不查表，直接留空）。
fn merge_guide(text: &Text) -> String {
    let mut out = String::new();
    for line in GUIDE_LINES {
        out.push_str(&text.get(line));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// SQL 分类抽取。
///
/// 编排照搬参考仓 `SqlClassifier.vue`：`pickRootDir` 之后先探 `pub` / `trans`
/// 两个子目录（有就只处理它们，没有就把根目录当成一个待处理目录），每个目录递归
/// 取 `*.sql`，分类结果累加后写进 `<目录>/exec/` 的六个文件（空文件跳过）。
///
/// 与源页面的差异只有两处，都是**呈现**：
/// ① 输出目录可在参数里改（默认仍是 `<根目录>/exec`）；
/// ② 结果里多报一行「未分类语句 N 条」——源页面把它们静默丢了，而设计稿要求
///    「无法归类的块不能静默丢」。
fn sql_classifier_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let root = params.text("root_dir");
    if root.is_empty() {
        return Err(fail(text.get("请选择要分类的源目录")));
    }
    let root_path = PathBuf::from(&root);
    if !root_path.is_dir() {
        return Err(fail(i18n::fill(&text.get("源目录不存在：{}"), &[&root])));
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    for name in ["pub", "trans"] {
        let candidate = root_path.join(name);
        if candidate.is_dir() {
            dirs.push(candidate);
        }
    }
    if dirs.is_empty() {
        dirs.push(root_path.clone());
    }

    let output_raw = params.text("output_dir");
    let output_dir = if output_raw.is_empty() {
        root_path.join("exec")
    } else {
        PathBuf::from(output_raw)
    };

    let date_suffix = chrono::Local::now().format("%Y%m%d").to_string();
    let mut file_count = 0_usize;
    let mut skipped = 0_usize;
    let mut unclassified = 0_usize;
    let mut all_create: Vec<String> = Vec::new();
    let mut all_column: Vec<String> = Vec::new();
    let mut all_dml: Vec<String> = Vec::new();
    let mut all_backup: Vec<String> = Vec::new();
    let mut all_rollback_ddl: Vec<String> = Vec::new();
    let mut all_rollback_data: Vec<String> = Vec::new();

    for dir in &dirs {
        let mut files = Vec::new();
        collect_sql_files(dir, dir, &mut files);
        // 源页面按 `rel_path` 字典序处理：同一个包内顺序稳定，产物可复现。
        files.sort();

        if files.is_empty() {
            out.log(i18n::fill(
                &text.get("{} 下没有 SQL 文件"),
                &[&dir.display().to_string()],
            ));
            continue;
        }

        for (abs, relative) in files {
            out.progress(file_count as u64 + 1, None, Some(&relative));
            match std::fs::read_to_string(&abs) {
                Ok(content) => {
                    let result =
                        duck_sql_core::sql_classifier::classify_sql_file(&content, &date_suffix);
                    unclassified += result.unclassified;
                    all_create.extend(result.create);
                    all_column.extend(result.column);
                    all_dml.extend(result.dml);
                    all_backup.extend(result.backup_lines);
                    all_rollback_ddl.extend(result.rollback_ddl);
                    all_rollback_data.extend(result.rollback_data);
                    file_count += 1;
                }
                Err(error) => {
                    // 源页面把读失败（多为非 UTF-8）计为「跳过」而不是整体失败。
                    skipped += 1;
                    out.log(i18n::fill(
                        &text.get("跳过 {}：{}"),
                        &[&relative, &error.to_string()],
                    ));
                }
            }
        }
    }

    if file_count == 0 {
        return Err(fail(text.get("没有找到可读取的 SQL 文件")));
    }

    std::fs::create_dir_all(&output_dir).map_err(|error| {
        fail(i18n::fill(
            &text.get("无法创建输出目录 {}：{}"),
            &[&output_dir.display().to_string(), &error.to_string()],
        ))
    })?;

    let mut backup = all_backup;
    backup.sort();
    backup.dedup();
    let outputs: [(&str, Vec<String>); 6] = [
        ("table_create_ddl.sql", all_create),
        ("table_column_ddl.sql", all_column),
        ("data_init_dml.sql", all_dml),
        ("backup_table.sql", backup),
        (
            "rollback_ddl.sql",
            all_rollback_ddl
                .into_iter()
                .filter(|line| !line.trim().is_empty())
                .collect(),
        ),
        (
            "rollback_data.sql",
            all_rollback_data
                .into_iter()
                .filter(|line| !line.trim().is_empty())
                .collect(),
        ),
    ];

    let mut written: Vec<(String, usize)> = Vec::new();
    for (name, lines) in outputs {
        if lines.is_empty() {
            continue;
        }
        let path = output_dir.join(name);
        std::fs::write(&path, lines.join("\n")).map_err(|error| {
            fail(i18n::fill(
                &text.get("写入 {} 失败：{}"),
                &[&path.display().to_string(), &error.to_string()],
            ))
        })?;
        written.push((name.to_owned(), lines.len()));
    }

    if written.is_empty() {
        return Err(fail(
            text.get("六个输出文件都是空的：这批脚本里没有可分类的语句"),
        ));
    }

    Ok(classify_report(
        text,
        &dirs,
        file_count,
        skipped,
        unclassified,
        &output_dir,
        &written,
    ))
}

/// 递归收集 `*.sql`，返回 `(绝对路径, 相对 dir 的路径)`，按相对路径排序。
fn collect_sql_files(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sql_files(&path, base, out);
            continue;
        }
        let is_sql = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"));
        if !is_sql {
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((path, relative));
    }
}

fn classify_report(
    text: &Text,
    dirs: &[PathBuf],
    file_count: usize,
    skipped: usize,
    unclassified: usize,
    output_dir: &Path,
    written: &[(String, usize)],
) -> String {
    let mut out = i18n::fill(
        &text.get("分类完成：{} 个目录 · {} 个文件"),
        &[&dirs.len().to_string(), &file_count.to_string()],
    );
    if skipped > 0 {
        out.push_str(&i18n::fill(
            &text.get("（跳过 {} 个无法读取的文件）"),
            &[&skipped.to_string()],
        ));
    }
    out.push('\n');
    out.push_str(&i18n::fill(
        &text.get("输出目录：{}"),
        &[&output_dir.display().to_string()],
    ));
    out.push_str("\n\n");
    out.push_str(&i18n::fill(
        &text.get("写入 {} 个文件"),
        &[&written.len().to_string()],
    ));
    for (name, lines) in written {
        out.push('\n');
        out.push_str(&i18n::fill(
            &text.get("{}（{} 行）"),
            &[name, &lines.to_string()],
        ));
    }
    if unclassified > 0 {
        out.push_str("\n\n");
        out.push_str(&i18n::fill(
            &text.get("未分类语句 {} 条：三类规则都没命中，未写入任何文件"),
            &[&unclassified.to_string()],
        ));
    }
    out
}

/// SQL 脚本检查。
///
/// 编排照搬参考仓 `SQLChecker.vue::runCheck`：`db_type` 取界面选择，其余
/// （`max_delete_rows` / `max_update_rows` = 10000、`allow_unconditional_delete`
/// = false、`check_timeout` = 30、表空间白名单 = 不过滤）就是前端写死的那组值。
/// 阈值不额外开放成参数——参考仓没给，多一个旋钮就多一种未验证组合。
fn script_check(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let sql = request.input.as_str();
    if sql.trim().is_empty() {
        return Err(fail(text.get("请粘贴要检查的 SQL 脚本")));
    }
    let params = Params::new(&request.params);
    let db_type = {
        let picked = params.text("db_type");
        if picked.is_empty() {
            "oracle".to_owned()
        } else {
            picked
        }
    };
    let config = CheckConfig {
        db_type,
        max_delete_rows: 10_000,
        max_update_rows: 10_000,
        allow_unconditional_delete: false,
        check_timeout: 30,
        allowed_tablespaces: None,
    };
    let report = sql_checker::check_sql(sql, &config);
    emit_check_table(out, &report, text);
    Ok(report_text(&report, text))
}

/// 结果表的 body（`[io.output] kind = "table"`）：宿主据此画
/// 「级别 / 位置 / 规则 / 说明」四列——问题清单本来就是表，压成文本之后
/// 「有几条错误、都在哪几行」得靠眼睛数。
///
/// 文本回执同时保留：复制按钮复制的是它，表格只影响**渲染**。
/// 说明与建议合成一格（`消息 → 建议`）：单独一列「建议」大多数行是空的。
fn emit_check_table(out: &mut PluginOutput<std::io::Stdout>, report: &CheckReport, text: &Text) {
    if report.results.is_empty() {
        return;
    }
    let translate = text.renderer();
    let columns = [
        text.get("级别"),
        text.get("位置"),
        text.get("规则"),
        text.get("说明"),
    ];
    let rows: Vec<Vec<String>> = report
        .results
        .iter()
        .map(|item| {
            let mut message = translate.translate(&item.message);
            if let Some(suggestion) = item.suggestion.as_deref() {
                message.push_str(" → ");
                message.push_str(&translate.translate(suggestion));
            }
            vec![
                severity_label(&item.severity, text),
                format!("L{}:{}", item.location.line, item.location.column),
                item.rule_id.clone(),
                message,
            ]
        })
        .collect();
    let doc = serde_json::json!({ "columns": columns, "rows": rows });
    out.body(&doc.to_string());
}

/// 报告拼版。
///
/// 参考仓把结果渲染成一张带级别色的小卡片列表，这里落成等价的纯文本：
/// 一行汇总 + 每条「级别 / 位置 / 规则号 / 规则名」+ 消息 + 建议。
/// 等宿主的 `table` 渲染器落地后再把 `[io.output] kind` 换过去，
/// 那时候这份文本仍然有用（复制、贴进工单）。
fn report_text(report: &CheckReport, text: &Text) -> String {
    let translate = text.renderer();
    let millis = report.execution_time_ms.to_string();
    if report.results.is_empty() {
        return i18n::fill(&text.get("SQL 检查通过，未发现问题（{} ms）"), &[&millis]);
    }
    let errors = report.total_errors.to_string();
    let warnings = report.total_warnings.to_string();
    let infos = report.total_info.to_string();
    let mut out = i18n::fill(
        &text.get("{} 错误 · {} 警告 · {} 信息（{} ms）"),
        &[&errors, &warnings, &infos, &millis],
    );
    for item in &report.results {
        out.push_str("\n\n");
        out.push_str(&format!(
            "[{}] L{}:{} {} {}",
            severity_label(&item.severity, text),
            item.location.line,
            item.location.column,
            item.rule_id,
            translate.translate(&item.rule_name)
        ));
        out.push_str("\n    ");
        out.push_str(&translate.translate(&item.message));
        if let Some(suggestion) = item.suggestion.as_deref() {
            // 箭头而不是「建议：」前缀：core 里的建议句自己就以「请 / 建议」开头，
            // 再套一层前缀会读成「建议：建议添加 LIMIT」。
            out.push_str("\n    → ");
            out.push_str(&translate.translate(suggestion));
        }
    }
    out
}

fn severity_label(severity: &Severity, text: &Text) -> String {
    let source = match severity {
        Severity::Error => "错误",
        Severity::Warning => "警告",
        Severity::Info => "信息",
    };
    text.get(source)
}

/// DDL 生成 / 分表。
///
/// 编排照搬参考仓 `DdlGenerator.vue::generateSharded` / `generateView`：
/// 含 `${num}`（或自定义索引占位符）才按数量循环展开，否则只做一次占位符替换；
/// 两者都没有时退回「表名追加序号」的老路。
fn ddl_generate(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let ddl = request.input.as_str();
    if ddl.trim().is_empty() {
        return Err(fail(text.get("请粘贴原始 CREATE TABLE DDL")));
    }
    let params = Params::new(&request.params);
    let mode = params.text("mode");
    // 界面上的范围是 1..512；这里再夹一次：process 轨没有执行页的即时校验，
    // 让 1e9 次循环把进程挂死不是「忠实复刻」的一部分。
    let count = params
        .number("shard_count")
        .unwrap_or(16.0)
        .clamp(1.0, 512.0) as usize;
    let ta_code = params.text("ta_code");
    let table_index = params.number("table_index").unwrap_or(1.0).max(0.0) as u32;
    let with_seq = params.flag("with_seq");
    let num_placeholder = params.text("num_placeholder");
    let ta_placeholder = params.text("ta_placeholder");

    if mode == "view" {
        let views = ddl_generator::generate_union_views(ddl, count, with_seq);
        if views.is_empty() {
            return Err(fail(text.get("输入里没有找到 CREATE TABLE 语句")));
        }
        return Ok(ddl_generator::format_ddls(&views));
    }

    let has_ta_placeholder = if ta_placeholder.is_empty() {
        ta_placeholder_present(ddl)
    } else {
        ddl.contains(ta_placeholder.as_str())
    };
    let has_num_placeholder = if num_placeholder.is_empty() {
        num_placeholder_present(ddl)
    } else {
        ddl.contains(num_placeholder.as_str())
    };

    if !has_num_placeholder {
        if has_ta_placeholder || !num_placeholder.is_empty() || !ta_placeholder.is_empty() {
            return Ok(ddl_generator::replace_sql_placeholders_with_patterns(
                ddl,
                table_index,
                &ta_code,
                optional(&num_placeholder),
                optional(&ta_placeholder),
            ));
        }
        let ddls = if ta_code.is_empty() {
            ddl_generator::generate_sharded_ddls(ddl, count)
        } else {
            ddl_generator::generate_ta_sharded_ddls(ddl, &ta_code, count)
        };
        return Ok(ddl_generator::format_ddls(&ddls));
    }

    let mut parts = Vec::with_capacity(count);
    for index in 1..=count {
        out.progress(index as u64, Some(count as u64), None);
        parts.push(ddl_generator::replace_sql_placeholders_with_patterns(
            ddl,
            index as u32,
            &ta_code,
            optional(&num_placeholder),
            optional(&ta_placeholder),
        ));
    }
    Ok(parts.join("\n\n"))
}

fn optional(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

fn fail(message: String) -> duck_plugin::PluginError {
    duck_plugin::PluginError::Execution(message)
}

/// `${num}`（大小写不敏感）是否出现——与 `DdlGenerator.vue` 的检测正则同一口径。
fn num_placeholder_present(ddl: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| regex::Regex::new(r"(?i)\$\{\s*num\s*\}").expect("static pattern"))
        .is_match(ddl)
}

/// `${ta_code}` / `${TACODE}` / `${taCode}` 变体是否出现。
fn ta_placeholder_present(ddl: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| regex::Regex::new(r"(?i)\$\{\s*ta_?code\s*\}").expect("static pattern"))
        .is_match(ddl)
}
