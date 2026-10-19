//! 上线包合并族（`duck.merge.*`）的 **process 轨入口**。
//!
//! 一个二进制服务整族工具：宿主在请求里给 `tool`，这里按 id 分派到
//! [`duck_merge_core`]——族内新增一个工具只需要加一条 `match` 分支与一份
//! `plugin.toml`，不必再产一个新可执行文件。
//!
//! 逻辑全部来自参考仓已验证的实现（见 `duck-merge-core` 各模块头），本文件只做
//! 三件事：解析 `[io]` 参数、把前端那一层编排照搬过来、把产出写成 JSONL 事件。
//!
//! 文案走插件自己的 `locales/`：宿主通过 [`duck_plugin::ENV_LOCALES_DIR`] 把
//! 目录传进来，与 JS 轨共用同一套「插件表 → 源串」降级。内核产出的是**已插值**
//! 的中文，所以还要过 [`duck_plugin_process::render::Renderer`] 做模板反查
//! （与 `duck-sql-tool` 同一套，见那个模块的文档）。

use duck_merge_core::MergeReporter;
use duck_merge_core::ifms6_merge::{
    self, Ifms6MergeOptions, Ifms6MergeResult, Ifms6NonSeeResult, NonSeeMergeOptions,
};
use duck_merge_core::manifest_extract::{self, ExtractOptions, ExtractReporter, ExtractResult};
use duck_merge_core::non_sp_merge::{self, NonSpMergeOptions, NonSpMergeResult};
use duck_merge_core::scc_merge::{self, SccMergeOptions, SccMergeResult};
use duck_merge_core::see_merge::{self, SeeMergeOptions, SeeMergeResult};
use duck_merge_core::sp_merge::{self, MergeOptions, MergeResult, PkgMergeReporter};
use duck_plugin::PluginLocales;
use duck_plugin_process::render::{Renderer, fill};
use duck_plugin_process::{PluginOutput, RunRequest, serve};
use std::cell::RefCell;
use std::path::Path;
use std::process::ExitCode;

/// 参数区：宿主把 `[io]` 表单收集到的值序列化成**字符串**键值对。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    /// 取字符串（数字 / 布尔也接受：第三方调用方可能直接塞 JSON 标量）。
    fn text(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.trim().to_owned(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    /// 取布尔（`"true"` / `"false"`；缺省或无法解析时用 `fallback`）。
    ///
    /// `[io]` 里 `switch` 的缺省值是字符串 `"true"` / `"false"`，但第三方调用方
    /// 直接塞 JSON 布尔也合理——两种都认。
    fn flag(&self, key: &str, fallback: bool) -> bool {
        match self.0.get(key) {
            Some(serde_json::Value::Bool(flag)) => *flag,
            Some(serde_json::Value::String(text)) => match text.trim() {
                "true" => true,
                "false" => false,
                _ => fallback,
            },
            _ => fallback,
        }
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

    /// 把内核产出的**已插值**中文串翻回去（带 `{}` 的走模板反查）。
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
    // 页内扫描（宿主 `[io] scan` 声明触发）：不是一次正常执行，先拦截。
    if request.action.as_deref() == Some("scan") {
        return page_scan(request, out, &text);
    }
    match request.tool.as_str() {
        "duck.merge.manifest-extract" => manifest_extract_run(request, out, &text),
        "duck.merge.pkg-merge" => pkg_merge_run(request, out, &text),
        "duck.merge.non-sp-merge" => non_sp_merge_run(request, out, &text),
        "duck.merge.ifms6-merge" => ifms6_merge_run(request, out, &text),
        "duck.merge.non-see-merge" => non_see_merge_run(request, out, &text),
        "duck.merge.scc-merge" => scc_merge_run(request, out, &text),
        "duck.merge.see-merge" => see_merge_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-merge-tool: unknown tool {other}"
        ))),
    }
}

/// 页内扫描：宿主以 `action = "scan"` 再拉一次本进程，这里只扫不合并。
///
/// 返回 `{"versions": ["a.zip", "b.zip", …]}` 形状的 body（按处理顺序），
/// 宿主把它回填进长任务页的只读展示参数。三个工具接了这条：
///
/// - `pkg-merge`：参考页面行为（`SpMerge.vue` 的 `scanPackages`）；
/// - `ifms6-merge` / `non-see-merge`：参考页面同样选完目录就回填版本清单，
///   内核共用 `ifms6_merge::scan_versions`。执行路径仍是「参数留空 = 按目录
///   实际版本合」，扫描只负责让用户**先看到**会合哪些——两处扫的是同一份目录，
///   不会出现「看到的和合的不一致」。
fn page_scan(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let versions = match request.tool.as_str() {
        "duck.merge.pkg-merge" => {
            sp_merge::scan_versions(&Params::new(&request.params).text("src_dir"))
        }
        "duck.merge.ifms6-merge" | "duck.merge.non-see-merge" => {
            ifms6_merge::scan_versions(&Params::new(&request.params).text("src_dir"))
        }
        _ => {
            return Err(duck_plugin::PluginError::Execution(
                "page scan is not supported by this tool".into(),
            ));
        }
    };
    let versions = versions.map_err(|error| fail(text.renderer().translate(&error)))?;
    let body = serde_json::json!({ "versions": versions });
    out.body(&body.to_string());
    Ok(String::new())
}

/// 把统计卡数据写成结构化 body（宿主长任务页据此渲染统计卡）。
///
/// 标签在这里就取好词（插件表 → 源串）：宿主不认识合并族的统计字段，
/// 拿到的就是「标签 + 值」的成品对。`output` 单独一个字段——它是「结果在哪个
/// 目录」，渲染成整行而不是格子。
fn emit_stats(
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
    entries: &[(&str, String)],
    output: &str,
) {
    let stats: serde_json::Value = serde_json::json!({
        "stats": entries
            .iter()
            .map(|(label, value)| serde_json::json!({
                "label": text.get(label),
                "value": value,
            }))
            .collect::<Vec<_>>(),
        "output": output,
    });
    out.body(&stats.to_string());
}

// ── 清单提取 ────────────────────────────────────────────────────────────

/// 过程回执：一路写插件事件流（宿主的进度条与日志窗），一路攒成结果里的日志块。
///
/// 两处重复是**有意**的：事件流只在运行中可见、跑完就停在最后一帧，而结果正文
/// 要能整段复制进工单——「哪一条没找到」正是复核清单。
struct ExtractEvents<'a, 'b> {
    out: RefCell<&'a mut PluginOutput<std::io::Stdout>>,
    text: &'b Text,
    lines: RefCell<Vec<String>>,
}

impl ExtractEvents<'_, '_> {
    fn lines(&self) -> Vec<String> {
        self.lines.borrow().clone()
    }
}

impl ExtractReporter for ExtractEvents<'_, '_> {
    fn progress(&self, current: usize, total: usize, current_file: &str) {
        // 事件流的 `message` 就是当前条目：界面顶部那行阶段说明直接可读。
        self.out.borrow_mut().progress(
            current as u64,
            Some(total as u64),
            Some(&truncate_path(current_file)),
        );
    }

    fn log(&self, level: &str, message: &str) {
        let translated = self.text.renderer().translate(message);
        // 级别随协议字段带出去（宿主日志窗按它着色），正文不再拼符号前缀。
        self.out.borrow_mut().log_with_level(level, &translated);
        self.lines.borrow_mut().push(translated);
    }

    fn done(&self, _result: &ExtractResult) {
        // 结果正文由调用方拼版（需要日志块，回执里没有）。
    }
}

fn manifest_extract_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let options = ExtractOptions {
        manifest_path: params.text("manifest_path"),
        src_dir: params.text("src_dir"),
        dst_dir: params.text("dst_dir"),
    };

    // 回执借用 `out`，跑完必须先把借用放开再拼结果。
    let (result, log) = {
        let events = ExtractEvents {
            out: RefCell::new(out),
            text,
            lines: RefCell::new(Vec::new()),
        };
        let result = manifest_extract::extract(&events, &options)
            .map_err(|error| fail(text.renderer().translate(&error)))?;
        (result, events.lines())
    };

    emit_stats(
        out,
        text,
        &[
            ("清单项", result.total.to_string()),
            ("找到", result.found.to_string()),
            ("未找到", result.not_found.to_string()),
            (
                "总耗时",
                format!("{:.1}", result.total_duration_ms as f64 / 1000.) + " s",
            ),
        ],
        &options.dst_dir,
    );
    Ok(report(&result, &options, text, &log))
}

/// 提取结果拼版：一行统计 + 未找到清单（可整段复制去补包）+ 过程日志。
///
/// 与 `ExtractByManifest.vue` 的「统计卡 + 未找到列表 + 日志面板」同构——
/// 只是落成纯文本：动作按钮（复制）归宿主，插件不碰剪贴板（零能力）。
fn report(result: &ExtractResult, options: &ExtractOptions, text: &Text, log: &[String]) -> String {
    let mut out = fill(
        &text.get("共 {} 条清单项 · 找到 {} · 未找到 {} · {} s"),
        &[
            &result.total.to_string(),
            &result.found.to_string(),
            &result.not_found.to_string(),
            &format!("{:.1}", result.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("目标目录：{}"), &[&options.dst_dir]));

    if result.not_found > 0 {
        out.push('\n');
        out.push('\n');
        out.push_str(&fill(
            &text.get("未找到的文件（{}）"),
            &[&result.not_found.to_string()],
        ));
        for entry in result.results.iter().filter(|entry| !entry.found) {
            out.push('\n');
            out.push_str(&entry.manifest_path);
        }
    }

    if !log.is_empty() {
        out.push('\n');
        out.push('\n');
        out.push_str(&fill(
            &text.get("处理日志（{} 行）"),
            &[&log.len().to_string()],
        ));
        for line in log {
            out.push('\n');
            out.push_str(line);
        }
    }

    if !result.success {
        out.push('\n');
        out.push('\n');
        out.push_str(&text.get("已取消。"));
    }
    out
}

/// 事件流里的当前条目截断：宿主那行只放得下一条，路径长了会把阶段说明挤没。
fn truncate_path(path: &str) -> String {
    const MAX: usize = 80;
    if path.chars().count() <= MAX {
        return path.to_owned();
    }
    let head: String = path.chars().take(MAX).collect();
    format!("{head}…")
}

// ── 上线包合并（SP / IFMS6）与需求单合并（非 SP） ─────────────────────
//
// 两条路径的进度/日志出口是**同一个**事件流，但内核侧的 trait 不同：
// `sp_merge` 的 `PkgMergeReporter` 用的是源仓原始载荷（带 `stage_duration_ms`，
// `current/total` 与 `current_file` 分列），其余三个模式共用一个简化的
// [`MergeReporter`]（只要「阶段 + 百分比 + 一句话」）。所以这里把「拼一行说明」
// 抽成 `MergeEvents` 的固有方法，两个 trait 实现都转调它——将来加模式也只加一个
// 实现块，不必再抄一遍取词与截断。

/// 内核的阶段名是英文（与源仓 Tauri 事件载荷一致），界面要中文：在这一层换标签。
/// `done` 不给文案——进度条走满就说明了。
fn stage_label(stage: &str) -> Option<&'static str> {
    match stage {
        "scanning" => Some("扫描包目录…"),
        "parsing" => Some("解析包名…"),
        // 只有需求单合并（非 SP）有这个阶段：先解压，边解压边按服务归并。
        "extracting" => Some("解压与合并中…"),
        "merging" => Some("合并中…"),
        "service_merge" => Some("服务目录合并…"),
        "sql_merge" => Some("SQL 脚本合并…"),
        "upd_merge" => Some("需求单说明合并…"),
        // SEE 包合并（`see_merge`）的五个阶段。
        "dedup" => Some("内层包去重…"),
        "slim_jars" => Some("Slim JAR 瘦身…"),
        "md5" => Some("生成 MD5 校验文件…"),
        "compressing" => Some("打包输出…"),
        // IFMS6 合包（`ifms6_merge`）。
        "db_merge" => Some("数据库包合并…"),
        "final_pack" => Some("最终打包…"),
        // SCC 合包（`scc_merge`）。
        "script_merge" => Some("脚本合并…"),
        "filtering" => Some("服务过滤…"),
        "stack_json" => Some("处理 Stack JSON…"),
        // 非 SEE 合包（`ifms6_merge::non_see_merge`）。
        "copy" => Some("复制版本包…"),
        "front" => Some("前端合并…"),
        "server" => Some("服务端合并…"),
        "db" => Some("数据库合并…"),
        "final" => Some("最终打包…"),
        _ => None,
    }
}

/// 过程回执：一路写插件事件流（宿主进度条 + 日志窗）。
///
/// **不攒日志块**：合并动辄几百行、几千行，塞进结果正文既没人读也拖慢渲染；
/// 宿主的日志窗本来就在，跑完仍能回看。清单提取那种「未找到几条，要整段复制去
/// 补包」的场景才需要把日志并进结果。
struct MergeEvents<'a, 'b> {
    out: RefCell<&'a mut PluginOutput<std::io::Stdout>>,
    text: &'b Text,
}

impl MergeEvents<'_, '_> {
    /// 一帧进度。`file` 是「当前正在处理的那一项」（可能没有）。
    fn emit_progress(&self, stage: &str, percent: u32, file: Option<String>) {
        // 进度条吃的是 `current/total`，而源仓的 `percent` 是**整体**百分比
        // （3 → 100，跨包、跨阶段早已算好）。所以把 percent 当成 0..100 的刻度
        // 直接喂进去——`current` / `total` 是「本组第几个包」，用它画条会在换组时
        // 倒退，与参考页面的观感不一致。
        // ⚠️ 先取词、再拼文件名：拼好之后整串去查表必然 miss（表里只有阶段标签本身），
        // 英文界面下会看到「合并中… · xxx.zip」这种半截翻译。
        let mut message = stage_label(stage)
            .map(|label| self.text.get(label))
            .unwrap_or_default();
        if let Some(file) = file {
            if !message.is_empty() {
                message.push_str(" · ");
            }
            message.push_str(&truncate_path(&file));
        }
        let message = if message.is_empty() {
            None
        } else {
            Some(message)
        };
        self.out
            .borrow_mut()
            .progress(u64::from(percent.min(100)), Some(100), message.as_deref());
    }

    /// 一行过程日志。级别随协议字段带出去（宿主日志窗按它着色），
    /// 正文不再拼符号前缀。
    fn emit_log(&self, level: &str, message: &str) {
        let translated = self.text.renderer().translate(message);
        self.out.borrow_mut().log_with_level(level, &translated);
    }
}

impl PkgMergeReporter for MergeEvents<'_, '_> {
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        _current: usize,
        _total: usize,
        current_file: Option<String>,
        _stage_duration_ms: Option<u64>,
    ) {
        self.emit_progress(stage, percent, current_file);
    }

    fn log(&self, level: &str, message: &str, _duration_ms: Option<u64>) {
        self.emit_log(level, message);
    }

    fn done(&self, _result: &MergeResult) {
        // 结果正文由调用方拼版（需要参数里的输出目录）。
    }
}

impl MergeReporter for MergeEvents<'_, '_> {
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        _current: usize,
        _total: usize,
        message: Option<String>,
    ) {
        self.emit_progress(stage, percent, message);
    }

    fn log(&self, level: &str, message: &str, _duration_ms: Option<u64>) {
        self.emit_log(level, message);
    }
}

fn pkg_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let options = MergeOptions {
        src_dir: params.text("src_dir"),
        dst_dir: params.text("dst_dir"),
        // 空 = 全选（与参考页面「全部」按钮同义）。
        version_filter: {
            let value = params.text("version_filter");
            if value.is_empty() {
                "all".to_owned()
            } else {
                value
            }
        },
        include_database: params.flag("include_database", true),
        include_documents: params.flag("include_documents", true),
        include_sql_merge: Some(params.flag("include_sql_merge", true)),
        include_service_merge: Some(params.flag("include_service_merge", true)),
        include_upd_merge: Some(params.flag("include_upd_merge", true)),
    };

    let result = {
        let events = MergeEvents {
            out: RefCell::new(out),
            text,
        };
        sp_merge::merge(&events, &options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("包数量", result.statistics.packages.to_string()),
            ("文件复制", result.statistics.files_copied.to_string()),
            ("文件覆盖", result.statistics.files_overwritten.to_string()),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_dir,
    );
    Ok(merge_report(&result, text))
}

/// 合并结果拼版：一行统计 + 输出目录 + 有回退清单时点出来。
///
/// 与 `SpMerge.vue` 的「成功徽标 + 统计卡」同构，只是落成纯文本：动作按钮
/// （复制 / 打开目录）归宿主，插件不碰剪贴板（零能力）。
fn merge_report(result: &MergeResult, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个包 · 新增 {} · 覆盖 {} · {} s"),
        &[
            &stats.packages.to_string(),
            &stats.files_copied.to_string(),
            &stats.files_overwritten.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出目录：{}"), &[&result.output_dir]));

    // `sp_merge` 只在**写失败**时记 warn，成功写完是静默的——所以这里探一下盘，
    // 有清单就点名，别让用户错过「这次合并把某个 .class 降级了」这件事。
    let regression = Path::new(&result.output_dir).join("版本回退核对清单.txt");
    if regression.is_file() {
        out.push('\n');
        out.push_str(&text.get("⚠️ 存在版本回退的 .class，详见输出目录的《版本回退核对清单.txt》"));
    }
    out
}

fn fail(message: String) -> duck_plugin::PluginError {
    duck_plugin::PluginError::Execution(message)
}

// ── 需求单合并（非 SP） ────────────────────────────────────────────────

fn non_sp_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let options = NonSpMergeOptions {
        src_dir: params.text("src_dir"),
        dst_dir: params.text("dst_dir"),
        // 参考页面这两个勾选框的初值都是**不勾**（`NonSpMerge.vue` 里
        // `ref(false)`），内核也是 `unwrap_or(false)`——默认值跟着源仓走，
        // 老用户的操作习惯不变。
        include_sql_merge: Some(params.flag("include_sql_merge", false)),
        include_upd_merge: Some(params.flag("include_upd_merge", false)),
    };

    let result = {
        let events = MergeEvents {
            out: RefCell::new(out),
            text,
        };
        non_sp_merge::merge(&events, &options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("需求单包", result.statistics.packages.to_string()),
            ("写入文件", result.statistics.files_copied.to_string()),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_dir,
    );
    Ok(non_sp_report(&result, &options, text))
}

/// 合并结果拼版：一行统计 + 输出目录 + 冲突报告 / upd 产物（存在才点名）。
///
/// 与 `NonSpMerge.vue` 的「成功徽标 + 统计卡 + 日志面板」同构，只是落成纯文本：
/// 动作按钮（复制 / 打开目录）归宿主，插件不碰剪贴板（零能力）。
fn non_sp_report(result: &NonSpMergeResult, options: &NonSpMergeOptions, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个需求单包 · 写入 {} 个文件 · {} s"),
        &[
            &stats.packages.to_string(),
            &stats.files_copied.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出目录：{}"), &[&result.output_dir]));

    // 冲突报告无条件生成（哪怕 0 条冲突也写「未发现冲突」），所以直接点名路径：
    // 用户最关心的就是「这次合并有没有把某个文件降级」，报告正文归文件。
    let report = Path::new(&options.dst_dir).join("合并检查结果.txt");
    if report.is_file() {
        out.push('\n');
        out.push_str(&text.get("同名文件冲突与裁决明细见《合并检查结果.txt》"));
    }

    if options.include_upd_merge.unwrap_or(false)
        && Path::new(&options.dst_dir).join("merge_upd.xml").is_file()
    {
        out.push('\n');
        out.push_str(&text.get("需求单说明已按包顺序追加为 merge_upd.xml"));
    }
    out
}

// ── IFMS6 合包 / 非 SEE 合包 / SCC 合包 / SEE 合包 ──────────────────────
//
// 这四个模块的 progress 载荷与 `sp_merge` 不同：`ifms6_merge`、`scc_merge`、
// `non_see_merge` 自己就带一句中文阶段说明（源仓载荷的 `message`），不再需要
// 上层拼「阶段标签 + 当前文件」。所以它们用 [`NoteEvents`]：有 `message` 就用它
// （过模板反查），没有才回落到阶段标签。

/// 回执：**以模块自己那句说明为准**。
///
/// 为什么不能复用 [`MergeEvents`]：那套是给「阶段 + 当前文件路径」用的（非 SEE
/// 需求单合并），会前置阶段标签。这四个模块的 `message` 本身就是阶段说明
/// （「数据库包合并...」），再接一次标签会变成
/// 「数据库包合并… · 数据库包合并...」。
struct NoteEvents<'a, 'b> {
    out: RefCell<&'a mut PluginOutput<std::io::Stdout>>,
    text: &'b Text,
}

impl NoteEvents<'_, '_> {
    fn emit_log(&self, level: &str, message: &str) {
        let translated = self.text.renderer().translate(message);
        self.out.borrow_mut().log_with_level(level, &translated);
    }
}

impl MergeReporter for NoteEvents<'_, '_> {
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        _current: usize,
        _total: usize,
        message: Option<String>,
    ) {
        let line = message
            .map(|line| self.text.renderer().translate(&line))
            .filter(|line| !line.trim().is_empty())
            .or_else(|| stage_label(stage).map(|label| self.text.get(label)));
        self.out
            .borrow_mut()
            .progress(u64::from(percent.min(100)), Some(100), line.as_deref());
    }

    fn log(&self, level: &str, message: &str, _duration_ms: Option<u64>) {
        self.emit_log(level, message);
    }
}

/// 版本列表：参数里给了就用参数（逗号分隔），留空则自己扫源目录。
///
/// 参考页面是「选目录 → 自动 scanVersions() → 回填文本框」，而 process 轨的插件
/// 进程不能被界面反向调用（源仓的 `ifms6_merge_scan_versions` 命令在这里不存在）。
/// 所以把扫描并进执行：留空即等于「按目录里的实际版本合」。
fn resolve_versions(
    src_dir: &str,
    raw: &str,
    events: &NoteEvents<'_, '_>,
    text: &Text,
) -> Result<Vec<String>, duck_plugin::PluginError> {
    let given: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect();
    if !given.is_empty() {
        return Ok(given);
    }
    let scanned = ifms6_merge::scan_versions(src_dir)
        .map_err(|error| fail(text.renderer().translate(&error)))?;
    if scanned.is_empty() {
        return Err(fail(
            text.get("源目录里没有找到版本包（IFMS6.0V* 目录或 .zip）"),
        ));
    }
    events.emit_log(
        "info",
        &fill(&text.get("扫描到 {} 个版本"), &[&scanned.len().to_string()]),
    );
    Ok(scanned)
}

/// 「✓ / —」两态：分块结果只在结果正文里点名，不占一行。
fn mark(flag: bool) -> String {
    if flag {
        "✓".to_owned()
    } else {
        "—".to_owned()
    }
}

fn ifms6_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let src_dir = params.text("src_dir");

    let result = {
        let events = NoteEvents {
            out: RefCell::new(out),
            text,
        };
        let versions = resolve_versions(&src_dir, &params.text("versions"), &events, text)?;
        let options = Ifms6MergeOptions {
            src_dir,
            dst_dir: params.text("dst_dir"),
            // 参考页面还有「客户名称 / 起始版本 / 截止版本」三格，后端内核**一个都不读**
            // （`Ifms6MergeOptions` 里三个字段全程没人用过）。做成输入框等于让人以为
            // 能过滤，改了却不生效——所以不声明，也就没有这三个值。
            customer_name: String::new(),
            all_merge_versions: versions,
            current_version: String::new(),
            target_version: String::new(),
            include_sql_merge: Some(params.flag("include_sql_merge", false)),
        };
        ifms6_merge::merge(&events, options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("版本数", result.statistics.versions.to_string()),
            ("前端", mark(result.statistics.front_merged)),
            ("服务端", mark(result.statistics.server_merged)),
            ("数据库", mark(result.statistics.db_merged)),
            ("ifmcounter", mark(result.statistics.ifmcounter_merged)),
            ("spconfig", mark(result.statistics.spconfig_merged)),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_path,
    );
    Ok(ifms6_report(&result, text))
}

fn ifms6_report(result: &Ifms6MergeResult, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个版本 · {} s"),
        &[
            &stats.versions.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出：{}"), &[&result.output_path]));
    out.push('\n');
    out.push_str(&fill(
        &text.get("分块结果：前端 {} · 服务端 {} · 数据库 {} · ifmcounter {} · spconfig {}"),
        &[
            &mark(stats.front_merged),
            &mark(stats.server_merged),
            &mark(stats.db_merged),
            &mark(stats.ifmcounter_merged),
            &mark(stats.spconfig_merged),
        ],
    ));

    // 内核只在**真的把某个 .class 降级**时写这份清单（见 `ifms6_merge` 模块头）。
    if Path::new(&result.output_path)
        .join("版本回退核对清单.txt")
        .is_file()
    {
        out.push('\n');
        out.push_str(&text.get("⚠️ 存在版本回退的 .class，详见输出目录的《版本回退核对清单.txt》"));
    }
    out
}

fn non_see_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let src_dir = params.text("src_dir");

    let result = {
        let events = NoteEvents {
            out: RefCell::new(out),
            text,
        };
        let versions = resolve_versions(&src_dir, &params.text("versions"), &events, text)?;
        let options = NonSeeMergeOptions {
            src_dir,
            dst_dir: params.text("dst_dir"),
            all_merge_versions: versions,
        };
        ifms6_merge::non_see_merge(&events, &options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("版本数", result.statistics.versions.to_string()),
            ("前端", mark(result.statistics.front_merged)),
            ("服务端", mark(result.statistics.server_merged)),
            ("数据库", mark(result.statistics.db_merged)),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_path,
    );
    Ok(non_see_report(&result, text))
}

fn non_see_report(result: &Ifms6NonSeeResult, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个版本 · {} s"),
        &[
            &stats.versions.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出：{}"), &[&result.output_path]));
    out.push('\n');
    out.push_str(&fill(
        &text.get("分块结果：前端 {} · 服务端 {} · 数据库 {}"),
        &[
            &mark(stats.front_merged),
            &mark(stats.server_merged),
            &mark(stats.db_merged),
        ],
    ));
    out
}

fn scc_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    // 微服务列表：参考页面是逗号分隔的一行，`tech_services` 由它**派生**
    // （`svc.filter(|s| s.contains("hswealth"))`，`SccMerge.vue:117`），不是独立输入。
    let services: Vec<String> = params
        .text("services")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect();
    let tech_services: Vec<String> = services
        .iter()
        .filter(|service| service.contains("hswealth"))
        .cloned()
        .collect();
    let empty_as_none = |value: String| {
        if value.is_empty() { None } else { Some(value) }
    };
    let options = SccMergeOptions {
        src_dir: params.text("src_dir"),
        dst_dir: params.text("dst_dir"),
        current_version: params.text("current_version"),
        target_version: params.text("target_version"),
        customer_no: params.text("customer_no"),
        // 参考页面默认 `db_type || 'oracle'`（`SccMerge.vue:120`）。
        db_type: {
            let value = params.text("db_type");
            if value.is_empty() {
                "oracle".to_owned()
            } else {
                value
            }
        },
        industry: empty_as_none(params.text("industry")),
        value_added: empty_as_none(params.text("value_added")),
        services,
        tech_services,
        stack_enabled: Some(params.flag("stack_enabled", false)),
        is_new_customer: Some(params.flag("is_new_customer", false)),
    };

    let result = {
        let events = NoteEvents {
            out: RefCell::new(out),
            text,
        };
        scc_merge::merge(&events, options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("包数量", result.statistics.packages.to_string()),
            ("服务数", result.statistics.services_merged.to_string()),
            // 参考页那行单行统计的第三格（`SccMerge.vue` 的「跨基线」）：
            // 状态是布尔，统计卡上以 ✓ / — 呈现。
            ("跨大版本", mark(result.statistics.cross_version)),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_path,
    );
    Ok(scc_report(&result, text))
}

fn scc_report(result: &SccMergeResult, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个包 · {} 个服务 · {} s"),
        &[
            &stats.packages.to_string(),
            &stats.services_merged.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出：{}"), &[&result.output_path]));
    if stats.cross_version {
        out.push('\n');
        out.push_str(&text.get("本次为跨大版本合包"));
    }
    out
}

fn see_merge_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let empty_as_none = |value: String| {
        if value.is_empty() { None } else { Some(value) }
    };
    let options = SeeMergeOptions {
        src_dir: params.text("src_dir"),
        dst_dir: params.text("dst_dir"),
        customer_no: empty_as_none(params.text("customer_no")),
        db_type: empty_as_none(params.text("db_type")),
        current_version: empty_as_none(params.text("current_version")),
        target_version: empty_as_none(params.text("target_version")),
        handle_custom_script: Some(params.flag("handle_custom_script", false)),
        slim_mode: Some(params.flag("slim_mode", false)),
    };

    let result = {
        let events = MergeEvents {
            out: RefCell::new(out),
            text,
        };
        see_merge::merge(&events, options)
            .map_err(|error| fail(text.renderer().translate(&error)))?
    };

    emit_stats(
        out,
        text,
        &[
            ("包数量", result.statistics.packages.to_string()),
            ("解压文件", result.statistics.files_extracted.to_string()),
            (
                "去重包",
                result.statistics.packages_deduplicated.to_string(),
            ),
            (
                "总耗时",
                format!(
                    "{:.1} s",
                    result.statistics.total_duration_ms as f64 / 1000.
                ),
            ),
        ],
        &result.output_path,
    );
    Ok(see_report(&result, text))
}

fn see_report(result: &SeeMergeResult, text: &Text) -> String {
    let stats = &result.statistics;
    let mut out = fill(
        &text.get("合并完成 · {} 个包 · 解压 {} 个文件 · 去重 {} 个 · {} s"),
        &[
            &stats.packages.to_string(),
            &stats.files_extracted.to_string(),
            &stats.packages_deduplicated.to_string(),
            &format!("{:.1}", stats.total_duration_ms as f64 / 1000.),
        ],
    );
    out.push('\n');
    out.push_str(&fill(&text.get("输出：{}"), &[&result.output_path]));

    // 三个「瘦身/个性化」计数只有真的发生时才由内核回填（`extended` 为 `None`
    // 表示一步都没触发），所以这一行是条件式的。
    if let Some(extended) = &result.extended {
        out.push('\n');
        out.push_str(&fill(
            &text.get("个性化 SQL：{} 个服务 · 删除客户 JAR {} 个 · Slim JAR 移除 {} 个"),
            &[
                &extended.personalized_sql_services.to_string(),
                &extended.customer_jars_deleted.to_string(),
                &extended.slim_jars_removed.to_string(),
            ],
        ));
    }
    out
}
