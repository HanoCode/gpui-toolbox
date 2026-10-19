//! 比对族（`duck.compare.*`）的 **process 轨入口**。
//!
//! 与 SQL / 合并两族同一个骨架：一个二进制服务整族工具，按 `request.tool` 分派到
//! [`duck_compare_core`]。新增一个比对工具＝加一条 `match` 分支 + 一份 `plugin.toml`。
//!
//! 本文件只做三件事（逻辑全在内核里，见各模块头）：
//!
//! 1. 把 `[io]` 参数摊平成内核要的入参；
//! 2. 把内核的产出映射成宿主的 [`duck_plugin::DiffDoc`]（`kind = "diff"` 的载荷）；
//! 3. 写事件：结构化差异走 `body`（分片），可复制的纯文本走 `result`。
//!
//! ⚠️ **`output` 与 `body` 的分工**是这一族的关键：`body` 只负责画（A/B 双栏、
//! 行内高亮），`output` 是用户按「复制」拿走的东西（unified diff / 清单）。
//! 两者都由同一份行数据生成，不会对不上。

use std::process::ExitCode;

use duck_compare_core::{
    CompareVerdict, Stage, class_compare, config_diff, dir_compare, jar_compare, text_diff,
};
use duck_plugin::PluginLocales;
use duck_plugin_process::{PluginOutput, RunRequest, serve};

mod i18n;

use i18n::Renderer;

/// 参数区（与 `duck-sql-tool` 同一套：宿主把表单值序列化成字符串键值对）。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    fn text(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.trim().to_owned(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    fn flag(&self, key: &str) -> bool {
        self.text(key).eq_ignore_ascii_case("true")
    }

    /// 取布尔，键**缺席**时用 `fallback`。
    ///
    /// 与 [`Self::flag`] 的区别只在缺省：`flag` 把「没给」当 `false`，而有些开关的
    /// 默认值是开着的（参考仓 `忽略大小写`），缺失时按 `false` 处理就翻了语义。
    fn flag_or(&self, key: &str, fallback: bool) -> bool {
        match self.0.get(key) {
            Some(serde_json::Value::Bool(value)) => *value,
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
        // `DUCK_LOCALES_DIR` 已经是 `locales/` 这一级：用 `load` 会再拼一次目录，
        // 结果一条译文都装不上（英文界面静默显示中文）。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(std::path::Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    fn get(&self, source: &str) -> String {
        self.locales.text(&self.lang, source).to_owned()
    }

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
        "duck.compare.text-diff" => text_diff_run(request, out, &text),
        "duck.compare.dir-compare" => dir_compare_run(request, out, &text),
        "duck.compare.config-compare" => config_compare_run(request, out, &text),
        "duck.compare.class-compare" => class_compare_run(request, out, &text),
        "duck.compare.jar-compare" => jar_compare_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-compare-tool: unknown tool {other}"
        ))),
    }
}

/// 内核错误是中文（复刻件原样），这里翻成当前语言再交给宿主。
fn fail(text: &Text, message: &str) -> duck_plugin::PluginError {
    duck_plugin::PluginError::Execution(text.renderer().translate(message))
}

// ── 文本差异 ────────────────────────────────────────────────────────────

/// 文本差异：两个输入框 → A/B 双栏 + 行内高亮。
///
/// 输入来自 `[io.fields]`（`text_a` / `text_b`，两个 textarea）——这是唯一一个
/// **用满两个字段**的工具，也正是 `[io]` 允许 0~2 个字段的原因。
fn text_diff_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let ignore_whitespace = params.flag("ignore_whitespace");
    // 参考仓 `TextDiff.vue` 的「忽略大小写」**默认开着**，缺省值必须跟着它走。
    let ignore_case = params.flag_or("ignore_case", true);
    let a = field(request, "text_a");
    let b = field(request, "text_b");

    // 两侧都空：不是错误，但也别画一块空的双栏——给一句提示更诚实。
    if a.is_empty() && b.is_empty() {
        return Err(fail(text, "两侧都是空的，没有可比的内容"));
    }

    let result = text_diff::compute_diff(a.clone(), b.clone(), ignore_whitespace, ignore_case)
        .map_err(|error| fail(text, &error))?;
    let doc = to_diff_doc(&result);

    // 结构化载荷：宿主据此画 A/B 双栏（`body` 只负责画）。
    out.body(&serde_json::to_string(&doc).map_err(|error| {
        duck_plugin::PluginError::Execution(format!("serialize diff body: {error}"))
    })?);

    // 纯文本镜像：复制按钮复制的就是它（unified diff，能直接贴进邮件 / 缺陷单）。
    Ok(text_diff::unified_diff(
        &a,
        &b,
        "A",
        "B",
        ignore_whitespace,
        ignore_case,
    ))
}

/// `[io.fields]` 的值：宿主直接把字段正文放在 `request.input`？不——两个字段时
/// 宿主按 id 放进 `params`（见 `tool_run.rs::params_json` 的 fields 分支），
/// 这里两个来源都读一遍，兼容手工调用。
fn field(request: &RunRequest, id: &str) -> String {
    let from_params = Params::new(&request.params).text(id);
    if !from_params.is_empty() {
        return from_params;
    }
    if id == "text_a" {
        request.input.clone()
    } else {
        String::new()
    }
}

/// 内核的行结构 → 宿主的 `DiffDoc`。
fn to_diff_doc(result: &text_diff::DiffResult) -> duck_plugin::DiffDoc {
    let rows = result
        .rows
        .iter()
        .map(|row| duck_plugin::DiffRow {
            kind: match row.kind.as_str() {
                "delete" => duck_plugin::DiffRowKind::Delete,
                "insert" => duck_plugin::DiffRowKind::Insert,
                "change" => duck_plugin::DiffRowKind::Change,
                _ => duck_plugin::DiffRowKind::Equal,
            },
            old_ln: row.old_ln,
            new_ln: row.new_ln,
            old_text: row.old_text.clone(),
            new_text: row.new_text.clone(),
            old_spans: spans(&row.old_changes),
            new_spans: spans(&row.new_changes),
        })
        .collect();
    duck_plugin::DiffDoc {
        files: vec![duck_plugin::DiffFile {
            title_a: "A".to_owned(),
            title_b: "B".to_owned(),
            rows,
        }],
    }
}

fn spans(changes: &[text_diff::ChangeSpan]) -> Vec<duck_plugin::DiffSpan> {
    // 参考仓的 `inline_diff` 走 `from_chars`，**一个字符一个 span**：一行 200 字就是
    // 200 个片段，渲染成 200 个元素，滚动时全是开销，而相邻同类片段在视觉上本来就是
    // 一整块。这里把相邻同类合并——渲染结果一模一样，节点数掉一个量级。
    let mut merged: Vec<duck_plugin::DiffSpan> = Vec::new();
    for change in changes {
        let hit = change.kind != "equal";
        match merged.last_mut() {
            Some(last) if last.hit == hit => last.text.push_str(&change.text),
            _ => merged.push(duck_plugin::DiffSpan {
                hit,
                text: change.text.clone(),
            }),
        }
    }
    merged
}

// ── 目录比对 ────────────────────────────────────────────────────────────

/// 目录比对：两个目录 → 差异清单（新增 / 删除 / 修改 / 未变）。
///
/// 判定只看**文件名、大小、修改时间**（源仓口径）：大目录逐文件读内容不现实，
/// 真要逐字比对请用「文本差异」或「包比对」。
///
/// 三个开关对应参考页目录会话里**真有**的那三件东西：
///
/// - `view`：顶部「全部 / 差异 / 相同」。参考页只筛树的显示，工具没有树，
///   所以筛在输出上。
/// - `format`：参考页「导出报告」给了 HTML / 文本两种形态（目录会话下它导出的是
///   文本 diff 的行记录，是空文件——参考仓的 bug），这里按它的**意图**补成
///   清单 / Markdown 表 / HTML 三种。
/// - `exclude`：参考页只硬编码跳过 `.` 开头的隐藏项，本仓额外给了剪枝规则。
fn dir_compare_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir_a = params.text("dir_a");
    let dir_b = params.text("dir_b");
    require_dirs(text, &dir_a, &dir_b)?;

    let excludes = patterns(&params.text("exclude"));
    let view = dir_compare::View::parse(&params.text("view"));
    let format = params.text("format");

    let result =
        dir_compare::compare_directories_excluding(dir_a.clone(), dir_b.clone(), &excludes)
            .map_err(|error| fail(text, &error))?;
    out.progress(0, None, Some(&text.get("比对完成")));

    let stats = &result.stats;
    let mut head = fill(
        &text.get("差异 · 新增 {} 个 · 删除 {} 个 · 修改 {} 个 · 未变 {} 个"),
        &[
            &stats.added.to_string(),
            &stats.removed.to_string(),
            &stats.modified.to_string(),
            &stats.unchanged.to_string(),
        ],
    );
    if !excludes.is_empty() {
        head.push('\n');
        head.push_str(&fill(
            &text.get("已按规则跳过: {}"),
            &[&excludes.join(", ")],
        ));
    }

    let rows: Vec<&dir_compare::DirDiffEntry> = result
        .entries
        .iter()
        .filter(|entry| view.keeps(&entry.kind))
        .collect();

    let body = match format.as_str() {
        "table" => render_table(text, &rows),
        "html" => render_html(text, &head, &rows),
        _ => render_list(text, &rows),
    };

    if rows.is_empty() && format != "html" {
        // 头部照给（统计是汇总口径，不随筛选变），再补一句「为什么是空的」。
        return Ok(format!("{head}\n\n{}", empty_hint(text, view)));
    }
    if format == "html" {
        return Ok(body);
    }
    Ok(format!("{head}\n\n{body}"))
}

/// 筛选后一条不剩时的说明：不给的话用户会以为工具坏了。
fn empty_hint(text: &Text, view: dir_compare::View) -> String {
    match view {
        dir_compare::View::Diff => text.get("两边完全一致"),
        dir_compare::View::Same => text.get("没有未变的条目"),
        dir_compare::View::All => text.get("两边都是空目录"),
    }
}

/// 状态标记：清单形态用单字符前缀（参考页树的 `+新 / -删 / ~改` 语义）。
fn kind_mark(kind: &str) -> &'static str {
    match kind {
        "added" => "+ ",
        "removed" => "- ",
        "modified" => "~ ",
        _ => "= ",
    }
}

/// 状态词：表格 / HTML 形态用整词（参考页 `kindLabel`）。
///
/// 返回中文源串，调用方过 [`Text::get`] 翻——这样三种形态共用同一份词表。
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "added" => "新增",
        "removed" => "删除",
        "modified" => "修改",
        _ => "未变",
    }
}

/// 一条的大小描述：改过的给「A → B」，只在一侧存在的给存在的那侧的大小。
fn size_note(entry: &dir_compare::DirDiffEntry, text: &Text) -> String {
    if entry.is_dir {
        return text.get("目录");
    }
    match entry.kind.as_str() {
        "modified" => {
            if entry.size_a != entry.size_b {
                format!("{} → {}", bytes(entry.size_a), bytes(entry.size_b))
            } else {
                // 大小一样还被判改：只能是 mtime 变了，说出来，别让用户猜。
                format!("{} · {}", bytes(entry.size_a), text.get("修改时间不同"))
            }
        }
        _ => bytes(entry.size_a.max(entry.size_b)),
    }
}

/// 清单形态（默认）：一行一条，前缀就是标记。
fn render_list(text: &Text, rows: &[&dir_compare::DirDiffEntry]) -> String {
    let mut report = String::new();
    for entry in rows {
        report.push_str(kind_mark(&entry.kind));
        report.push_str(&entry.rel_path);
        if entry.is_dir {
            // 目录没有大小，`(目录)` 是废话，只留尾斜杠。
            report.push('/');
        } else {
            report.push_str("  (");
            report.push_str(&size_note(entry, text));
            report.push(')');
        }
        report.push('\n');
    }
    report.trim_end().to_string()
}

/// 表格形态：Markdown，贴进周报 / 缺陷单不用再排。
fn render_table(text: &Text, rows: &[&dir_compare::DirDiffEntry]) -> String {
    let mut report = String::new();
    report.push_str(&format!(
        "| {} | {} | {} |\n|---|---|---|\n",
        text.get("状态"),
        text.get("路径"),
        text.get("大小"),
    ));
    for entry in rows {
        report.push_str(&format!(
            "| {} | `{}` | {} |\n",
            text.get(kind_label(&entry.kind)),
            escape_md(&entry.rel_path),
            escape_md(&size_note(entry, text)),
        ));
    }
    report.trim_end().to_string()
}

/// HTML 形态：自带底色的完整文档，双击就能看（参考页「导出报告 → HTML」）。
fn render_html(text: &Text, head: &str, rows: &[&dir_compare::DirDiffEntry]) -> String {
    let mut body = String::new();
    for entry in rows {
        let color = match entry.kind.as_str() {
            "added" => "#22c55e",
            "removed" => "#ef4444",
            "modified" => "#f59e0b",
            _ => "#94a3b8",
        };
        body.push_str(&format!(
            "<tr><td style=\"color:{color}\">{}</td><td><code>{}</code></td><td>{}</td></tr>\n",
            escape_html(kind_label(&entry.kind)),
            escape_html(&entry.rel_path),
            escape_html(&size_note(entry, text)),
        ));
    }
    format!(
        "<!DOCTYPE html>\n<html lang=\"{lang}\">\n<head><meta charset=\"utf-8\"><title>{title}</title>\n\
<style>\nbody{{font:13px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:24px;color:#1f2937}}\n\
h1{{font-size:16px;margin:0 0 4px}}\np{{color:#6b7280;margin:0 0 16px;white-space:pre-line}}\n\
table{{border-collapse:collapse;width:100%}}\nth,td{{text-align:left;padding:5px 8px;border-bottom:1px solid #e5e7eb}}\n\
th{{color:#6b7280;font-weight:500}}\ncode{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}}\n</style>\n\
</head>\n<body>\n<h1>{title}</h1>\n<p>{head}</p>\n<table>\n\
<thead><tr><th>{c_status}</th><th>{c_path}</th><th>{c_size}</th></tr></thead>\n<tbody>\n{body}</tbody>\n</table>\n</body>\n</html>",
        title = escape_html(&text.get("差异报告")),
        // `lang` 跟界面语言走：英文界面导出的报告不该自称中文。
        lang = escape_html(&text.lang),
        head = escape_html(head),
        c_status = escape_html(&text.get("状态")),
        c_path = escape_html(&text.get("路径")),
        c_size = escape_html(&text.get("大小")),
    )
}

/// Markdown 表格里的 `|` 会把列切断。
fn escape_md(value: &str) -> String {
    value.replace('|', "\\|")
}

/// HTML 转义：路径里 `<` `&` 不算罕见（Java 泛型、URL 参数）。
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── 配置比对 ────────────────────────────────────────────────────────────

/// 配置比对：properties / YAML / JSON 三路输入 → 拍平成键值 → 逐键判档。
///
/// 参考页的做法（`DirCompare.vue` 的 config 会话）是「先把两侧都转成 JSON、
/// 再拍平、再按键比」，所以同一份配置写成 properties 还是 YAML 不影响结果。
///
/// 两个开关：
/// - `view`：参考页只有「仅差异 / 全部」两档（没有「仅相同」）。
/// - `format`：清单 / Markdown 表。参考页这一段没有导出按钮，两种形态是为
///   「复制出去贴进缺陷单」补的，不是照抄。
fn config_compare_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let a = field(request, "config_a");
    let b = field(request, "config_b");
    if a.trim().is_empty() || b.trim().is_empty() {
        return Err(fail(text, "两侧都要有内容"));
    }

    let diff = config_diff::compare(&a, &b).map_err(|error| fail(text, &error))?;
    out.progress(0, None, Some(&text.get("比对完成")));

    let stats = &diff.stats;
    let mut head = fill(
        &text.get("配置差异 · 修改 {} 个 · 仅左 {} 个 · 仅右 {} 个 · 相同 {} 个"),
        &[
            &stats.modified.to_string(),
            &stats.left_only.to_string(),
            &stats.right_only.to_string(),
            &stats.same.to_string(),
        ],
    );
    head.push('\n');
    head.push_str(&format!(
        "{} vs {}",
        diff.left_format.label(),
        diff.right_format.label()
    ));

    // 参考页只有「仅差异 / 全部」：`all` 才是全部，其余一律只看差异。
    let show_all = params.text("view") == "all";
    let rows: Vec<&config_diff::Entry> = diff
        .entries
        .iter()
        .filter(|entry| show_all || entry.status != config_diff::Status::Same)
        .collect();

    if rows.is_empty() {
        let hint = if diff.entries.is_empty() {
            text.get("两侧都没读到配置项")
        } else {
            text.get("两侧配置完全一致")
        };
        return Ok(format!("{head}\n\n{hint}"));
    }

    let body = match params.text("format").as_str() {
        "table" => render_config_table(text, &rows),
        _ => render_config_list(&rows),
    };
    Ok(format!("{head}\n\n{body}"))
}

/// 清单：`~ key  旧 → 新` / `- key  (旧)` / `+ key  (新)` / `= key  (值)`。
fn render_config_list(rows: &[&config_diff::Entry]) -> String {
    let mut report = String::new();
    for entry in rows {
        report.push_str(kind_mark(entry.status.kind()));
        report.push_str(&entry.key);
        report.push_str("  (");
        match entry.status {
            config_diff::Status::Modified => {
                report.push_str(entry.left.as_deref().unwrap_or_default());
                report.push_str(" → ");
                report.push_str(entry.right.as_deref().unwrap_or_default());
            }
            config_diff::Status::LeftOnly => {
                report.push_str(entry.left.as_deref().unwrap_or_default());
            }
            config_diff::Status::RightOnly => {
                report.push_str(entry.right.as_deref().unwrap_or_default());
            }
            config_diff::Status::Same => {
                report.push_str(entry.left.as_deref().unwrap_or_default());
            }
        }
        report.push_str(")\n");
    }
    report.trim_end().to_string()
}

/// 配置比对的状态词：**不用**目录那一套「新增 / 删除」——配置里没有「新增」的
/// 概念，只有「相对另一侧多/少了这个键」，参考页也一直写「仅左 / 仅右」。
fn config_status_label(status: config_diff::Status) -> &'static str {
    match status {
        config_diff::Status::Same => "相同",
        config_diff::Status::Modified => "修改",
        config_diff::Status::LeftOnly => "仅左",
        config_diff::Status::RightOnly => "仅右",
    }
}

/// 表：`状态 | Key | 左侧值 | 右侧值`——参考页那张表的列顺序，前面加一列状态
/// （终端里没有行底色，状态只能靠文字）。
fn render_config_table(text: &Text, rows: &[&config_diff::Entry]) -> String {
    let mut report = String::new();
    report.push_str(&format!(
        "| {} | Key | {} | {} |\n|---|---|---|---|\n",
        text.get("状态"),
        text.get("左侧值"),
        text.get("右侧值"),
    ));
    for entry in rows {
        report.push_str(&format!(
            "| {} | `{}` | {} | {} |\n",
            text.get(config_status_label(entry.status)),
            escape_md(&entry.key),
            escape_md(entry.left.as_deref().unwrap_or("—")),
            escape_md(entry.right.as_deref().unwrap_or("—")),
        ));
    }
    report.trim_end().to_string()
}

/// 文件大小：B / KB / MB / GB（保留一位小数）。
fn bytes(size: u64) -> String {
    const KB: f64 = 1024.;
    let value = size as f64;
    if value < KB {
        return format!("{size} B");
    }
    if value < KB * KB {
        return format!("{:.1} KB", value / KB);
    }
    if value < KB * KB * KB {
        return format!("{:.1} MB", value / (KB * KB));
    }
    format!("{:.1} GB", value / (KB * KB * KB))
}

/// `{}` 占位符替换（与 `duck_plugin_process::fill` 同语义，但允许模板来自译文）。
fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    for arg in args {
        match rest.split_once("{}") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(arg);
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

// ── class 比对 / jar 比对 ───────────────────────────────────────────────

/// 解析逗号（或空白）分隔的排除模式串。
fn patterns(raw: &str) -> Vec<String> {
    raw.split([',', '，', ' ', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 表单里「自定义」那个选项的哨兵值。
///
/// `select` 的选项不许是空串（装载期校验），所以「自定义」占一个值，真名在旁边的
/// 文本框里——参考页 `baseCustomEnv` 也是这么分的。
const CUSTOM_ENV: &str = "custom";

/// 环境名：预设下拉 + 自定义框，两者都没有就用译文的「基准」「对比」。
///
/// 它只影响报告措辞与 CSV 文件名，不影响判定。
fn env_name(params: &Params, key: &str, custom_key: &str, fallback: &str, text: &Text) -> String {
    let preset = params.text(key);
    let value = if preset == CUSTOM_ENV {
        params.text(custom_key)
    } else {
        preset
    };
    if value.is_empty() {
        text.get(fallback)
    } else {
        value
    }
}

/// 校验两侧目录。
fn require_dirs(text: &Text, dir_a: &str, dir_b: &str) -> Result<(), duck_plugin::PluginError> {
    for dir in [dir_a, dir_b] {
        if dir.is_empty() {
            return Err(fail(text, "两侧目录都要选"));
        }
        if !std::path::Path::new(dir).is_dir() {
            return Err(fail(text, "路径不是有效目录"));
        }
    }
    Ok(())
}

/// class 比对：两边应用目录里的同名 `.class`，逐个比 `getVersion()`。
fn class_compare_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir_a = params.text("dir_a");
    let dir_b = params.text("dir_b");
    require_dirs(text, &dir_a, &dir_b)?;

    let spec = class_compare::CompareSpec {
        base_env: env_name(&params, "env_a", "env_a_custom", "基准", text),
        target_env: env_name(&params, "env_b", "env_b_custom", "对比", text),
        base_dir: dir_a.into(),
        target_dir: dir_b.into(),
        // 键缺席时按「不勾」处理：lib 条目的版本是 `jar:大小:时间` 指纹，两个包
        // 打包时间不同就会让每个 lib JAR 都变成一条差异（见 plugin.toml 的说明）。
        compare_lib: params.flag_or("compare_lib", false),
        jar_exclude: patterns(&params.text("jar_exclude")),
    };

    let mut report_progress = |progress: class_compare::Progress| {
        out.progress(
            u64::from(progress.percent),
            Some(100),
            Some(&class_stage(text, &progress.stage)),
        );
    };
    let report = class_compare::scan_and_compare(&spec, &mut report_progress)
        .map_err(|error| fail(text, &error))?;
    out.progress(100, Some(100), Some(&text.get("比对完成")));

    let mut body = class_report(text, &spec, &report, View::parse(&params.text("view")));
    if let Some(note) = export_class_csv(text, &spec, &report, &params.text("csv_dir"))? {
        body.push('\n');
        body.push_str(&note);
        body.push('\n');
    }
    Ok(body)
}

/// 进度阶段 → 一句话（class 版：单位「个类」）。
fn class_stage(text: &Text, stage: &Stage) -> String {
    match stage {
        Stage::BaseListing => text.get("扫描基准环境…"),
        Stage::BaseListed { count } => {
            fill(&text.get("基准环境清单：{} 个类"), &[&count.to_string()])
        }
        Stage::TargetListed { count } => {
            fill(&text.get("对比环境清单：{} 个类"), &[&count.to_string()])
        }
        Stage::BaseVersions {
            batch,
            batches,
            scanned,
        } => fill(
            &text.get("基准环境第 {}/{} 批：已扫 {} 个类"),
            &[
                &batch.to_string(),
                &batches.to_string(),
                &scanned.to_string(),
            ],
        ),
        Stage::TargetVersions {
            batch,
            batches,
            scanned,
        } => fill(
            &text.get("对比环境第 {}/{} 批：已扫 {} 个类"),
            &[
                &batch.to_string(),
                &batches.to_string(),
                &scanned.to_string(),
            ],
        ),
        Stage::Comparing => text.get("比对分析…"),
    }
}

/// 进度阶段 → 一句话（jar 版：单位「个包」）。
fn jar_stage(text: &Text, stage: &Stage) -> String {
    match stage {
        Stage::BaseListing => text.get("扫描基准环境…"),
        Stage::BaseListed { count } => {
            fill(&text.get("基准环境清单：{} 个包"), &[&count.to_string()])
        }
        Stage::TargetListed { count } => {
            fill(&text.get("对比环境清单：{} 个包"), &[&count.to_string()])
        }
        Stage::BaseVersions {
            batch,
            batches,
            scanned,
        } => fill(
            &text.get("基准环境第 {}/{} 批：已扫 {} 个包"),
            &[
                &batch.to_string(),
                &batches.to_string(),
                &scanned.to_string(),
            ],
        ),
        Stage::TargetVersions {
            batch,
            batches,
            scanned,
        } => fill(
            &text.get("对比环境第 {}/{} 批：已扫 {} 个包"),
            &[
                &batch.to_string(),
                &batches.to_string(),
                &scanned.to_string(),
            ],
        ),
        Stage::Comparing => text.get("比对分析…"),
    }
}

/// 终端显示宽度：东亚宽字符算 2 列。
///
/// 结果区的字体是等宽的，「新增」按 `chars().count()` 算只有 2，视觉上却占 4 列，
/// 一补空格整列就歪。这里用一段够用的区间判定（不引 `unicode-width`：
/// 本工具只会遇到中日韩文字与全角标点）。
fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F      // 谚文字母
        | 0x2E80..=0x303E     // 中日韩部首、标点
        | 0x3041..=0x33FF     // 假名、注音、兼容字符
        | 0x3400..=0x4DBF     // 扩展 A
        | 0x4E00..=0x9FFF     // 基本汉字
        | 0xA000..=0xA4CF     // 彝文
        | 0xAC00..=0xD7A3     // 谚文音节
        | 0xF900..=0xFAFF     // 兼容汉字
        | 0xFE30..=0xFE6F     // 兼容形式
        | 0xFF00..=0xFF60     // 全角
        | 0xFFE0..=0xFFE6     // 全角符号
        | 0x20000..=0x3FFFD   // 扩展 B 及以上
    )
}

/// 右侧补空格到指定**显示宽度**。
fn pad(value: &str, width: usize) -> String {
    let used = display_width(value);
    if used >= width {
        return value.to_string();
    }
    format!("{value}{}", " ".repeat(width - used))
}

/// 超宽内容截断成 `…` 结尾。
///
/// `WEB-INF/lib` 下 JAR 条目的「版本」是 `jar:<大小>:<时间>` 指纹，能到 30 多列；
/// 不截断的话一个长指纹就会把整张表推歪（`pad` 只补空格、不裁）。
fn clip(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let step = if is_wide(ch) { 2 } else { 1 };
        if used + step + 1 > width {
            break;
        }
        out.push(ch);
        used += step;
    }
    out.push('…');
    out
}

/// 一个单元格：先按列宽截断，再补空格。
fn cell(value: &str, width: usize) -> String {
    pad(&clip(value, width), width)
}

/// 列宽 = max(表头, 本列最长内容)，封顶 `cap`。
///
/// 表头本身不给截断的余地：`基准版本` 是 8 列，若只按值为 `1.2.0.3`（7 列）算宽度，
/// 表头会被裁成「基准版…」——一张看不懂表头的表还不如没有列名。
fn column_width(widths: impl Iterator<Item = usize>, header: &str, cap: usize) -> usize {
    let header_w = display_width(header);
    widths
        .max()
        .unwrap_or(0)
        .max(header_w)
        .min(cap.max(header_w))
}

/// 报告筛选：对应参考页那排筛选按钮（全部 / 仅异常 / 升级 / 回退 / 新增 / 缺失）。
///
/// `all` **不含「相同」**：源页的表格把相同项一起列出来，可几百条同版本内容会把
/// 真正要看的那几十条埋掉。要看相同请显式选 `same`——于是「全部」在终端里
/// 始终等于「全部差异」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    Abnormal,
    Update,
    Rollback,
    New,
    Missing,
    Same,
}

impl View {
    fn parse(raw: &str) -> Self {
        match raw {
            "abnormal" => Self::Abnormal,
            "update" => Self::Update,
            "rollback" => Self::Rollback,
            "new" => Self::New,
            "missing" => Self::Missing,
            "same" => Self::Same,
            _ => Self::All,
        }
    }

    /// 这一组要不要列出来。`abnormal` 落到回退组：异常只由版本回退产生。
    fn shows(self, verdict: CompareVerdict) -> bool {
        match self {
            Self::All => verdict != CompareVerdict::Same,
            Self::Abnormal | Self::Rollback => verdict == CompareVerdict::Rollback,
            Self::Update => verdict == CompareVerdict::Update,
            Self::New => verdict == CompareVerdict::New,
            Self::Missing => verdict == CompareVerdict::Missing,
            Self::Same => verdict == CompareVerdict::Same,
        }
    }

    /// 筛选线用的名字（`all` 不出线，省一行）。
    fn label(self, text: &Text) -> Option<String> {
        match self {
            Self::All => None,
            Self::Abnormal => Some(text.get("仅异常")),
            Self::Update => Some(text.get("升级")),
            Self::Rollback => Some(text.get("回退")),
            Self::New => Some(text.get("新增")),
            Self::Missing => Some(text.get("缺失")),
            Self::Same => Some(text.get("相同")),
        }
    }
}

/// 空值画成一个破折号：空白单元格在等宽表里分不清「没有」和「少了一列」。
fn dash(value: &str) -> &str {
    if value.is_empty() { "—" } else { value }
}

/// 一条结论的文案（CSV 的「状态」列，也是筛选标签）。
///
/// 口径照参考页：`is_abnormal` 先判——回退行在页面上显示的是「异常」，不是「回退」。
fn verdict_label(text: &Text, verdict: CompareVerdict, abnormal: bool) -> String {
    if abnormal {
        return text.get("异常");
    }
    match verdict {
        CompareVerdict::Update => text.get("升级"),
        CompareVerdict::Same => text.get("相同"),
        CompareVerdict::Rollback => text.get("回退"),
        CompareVerdict::New => text.get("新增"),
        CompareVerdict::Missing => text.get("缺失"),
    }
}

/// class 比对的文本报告。
///
/// 分组顺序是**按重要性**排的，不是按枚举序：回退（要人立刻处理）→ 升级 →
/// 新增。「相同」只在 `view = same` 时列出——几百条同版本内容会把真正要看的那
/// 几十条埋掉（这一点与参考页表格不同，见 [`View`] 的文档）。
fn class_report(
    text: &Text,
    spec: &class_compare::CompareSpec,
    report: &class_compare::ClassCompareReport,
    view: View,
) -> String {
    let mut out = String::new();
    // 这行是纯结构（两个环境名并列），不走译文表：任何语言里都是「A · B」。
    out.push_str(&format!("{} · {}\n", spec.base_env, spec.target_env));
    out.push_str(&fill(
        &text.get("基准 {} 个类 · 对比 {} 个类 · 交集 {} 个"),
        &[
            &report.base_listed.to_string(),
            &report.target_listed.to_string(),
            &report.compared.to_string(),
        ],
    ));
    out.push('\n');

    out.push_str(&fill(
        &text.get("新增 {} · 升级 {} · 回退 {} · 异常 {} · 相同 {} · 总计 {}"),
        &[
            &report.count(CompareVerdict::New).to_string(),
            &report.count(CompareVerdict::Update).to_string(),
            &report.count(CompareVerdict::Rollback).to_string(),
            &report.abnormal().to_string(),
            &report.count(CompareVerdict::Same).to_string(),
            &report.results.len().to_string(),
        ],
    ));
    out.push('\n');

    if let Some(label) = view.label(text) {
        out.push_str(&fill(&text.get("筛选：{}"), &[&label]));
        out.push('\n');
    }

    let mut listed = 0usize;
    for (verdict, title) in [
        (CompareVerdict::Rollback, text.get("回退 {} 条")),
        (CompareVerdict::Update, text.get("升级 {} 条")),
        (CompareVerdict::New, text.get("新增 {} 条")),
        (CompareVerdict::Same, text.get("相同 {} 条")),
    ] {
        if !view.shows(verdict) {
            continue;
        }
        let rows: Vec<&class_compare::CompareResult> = report
            .results
            .iter()
            .filter(|result| result.compare_type == verdict)
            .collect();
        if rows.is_empty() {
            continue;
        }
        listed += rows.len();
        out.push('\n');
        out.push_str(&fill(&title, &[&rows.len().to_string()]));
        out.push('\n');
        out.push_str(&class_rows(text, &rows));
    }

    if listed == 0 {
        out.push('\n');
        out.push_str(&text.get(if view == View::All {
            "两边没有差异"
        } else {
            "没有符合筛选条件的记录"
        }));
        out.push('\n');
    }
    out
}

/// 一组 class 结论的逐行明细。
///
/// 列**按参考页那张表**给：服务 / 类名 / 包名 / 基准版本 / 对比版本 / 级差。
/// 页面上还有「差异描述」（一整句）与「源码差异」——前者由本行的版本与级差
/// 表达（分组已经说明了升级还是回退），后者要反编译（见 `class_compare` 模块头）。
fn class_rows(text: &Text, rows: &[&class_compare::CompareResult]) -> String {
    let headers = [
        text.get("服务"),
        text.get("类名"),
        text.get("包名"),
        text.get("基准版本"),
        text.get("对比版本"),
        text.get("级差"),
    ];
    let server_w = column_width(
        rows.iter().map(|row| display_width(&row.server_name)),
        &headers[0],
        24,
    );
    let class_w = column_width(
        rows.iter().map(|row| display_width(&row.class_name)),
        &headers[1],
        30,
    );
    let pkg_w = column_width(
        rows.iter().map(|row| display_width(&row.package_name)),
        &headers[2],
        40,
    );
    let base_w = column_width(
        rows.iter()
            .map(|row| display_width(dash(&row.version_base))),
        &headers[3],
        16,
    );
    let target_w = column_width(
        rows.iter()
            .map(|row| display_width(dash(&row.version_target))),
        &headers[4],
        16,
    );

    let mut out = String::new();
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}  {}\n",
        cell(&headers[0], server_w),
        cell(&headers[1], class_w),
        cell(&headers[2], pkg_w),
        cell(&headers[3], base_w),
        cell(&headers[4], target_w),
        headers[5],
    ));
    for row in rows {
        // 新增的行没有对比侧：版本与级差都画破折号，而不是留空或写 0。
        let level = if row.compare_type == CompareVerdict::New {
            "—".to_string()
        } else {
            row.diff_version_num.to_string()
        };
        let line = format!(
            "  {}  {}  {}  {}  {}  {}",
            cell(&row.server_name, server_w),
            cell(&row.class_name, class_w),
            cell(&row.package_name, pkg_w),
            cell(dash(&row.version_base), base_w),
            cell(dash(&row.version_target), target_w),
            level,
        );
        // 最后一列补过空格会留下一串尾随空白，复制出去全是看不见的字符。
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// jar 比对：两边应用目录里的同名 JAR，比 JAR 级版本（内部 class 版本的众数）。
fn jar_compare_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir_a = params.text("dir_a");
    let dir_b = params.text("dir_b");
    require_dirs(text, &dir_a, &dir_b)?;

    let spec = jar_compare::JarCompareSpec {
        base_env: env_name(&params, "env_a", "env_a_custom", "基准", text),
        target_env: env_name(&params, "env_b", "env_b_custom", "对比", text),
        base_dir: dir_a.into(),
        target_dir: dir_b.into(),
        // 键缺席时按「勾上」处理：参考页 `compareLib` 默认 true（放开扫描范围）。
        scan_all: params.flag_or("compare_lib", true),
        jar_exclude: patterns(&params.text("jar_exclude")),
    };

    let mut report_progress = |progress: class_compare::Progress| {
        out.progress(
            u64::from(progress.percent),
            Some(100),
            Some(&jar_stage(text, &progress.stage)),
        );
    };
    let report = jar_compare::compare_jars(&spec, &mut report_progress)
        .map_err(|error| fail(text, &error))?;
    out.progress(100, Some(100), Some(&text.get("比对完成")));

    let mut body = jar_report(text, &spec, &report, View::parse(&params.text("view")));
    if let Some(note) = export_jar_csv(text, &spec, &report, &params.text("csv_dir"))? {
        body.push('\n');
        body.push_str(&note);
        body.push('\n');
    }
    Ok(body)
}

/// jar 比对的文本报告。分组与 class 一致（回退 → 升级 → 新增 → 缺失）。
fn jar_report(
    text: &Text,
    spec: &jar_compare::JarCompareSpec,
    report: &jar_compare::JarCompareReport,
    view: View,
) -> String {
    let mut out = String::new();
    // 这行是纯结构（两个环境名并列），不走译文表：任何语言里都是「A · B」。
    out.push_str(&format!("{} · {}\n", spec.base_env, spec.target_env));
    out.push_str(&fill(
        &text.get("基准 {} 个包 · 对比 {} 个包"),
        &[
            &report.base_listed.to_string(),
            &report.target_listed.to_string(),
        ],
    ));
    out.push('\n');

    out.push_str(&fill(
        &text.get("新增 {} · 升级 {} · 回退 {} · 缺失 {} · 异常 {} · 相同 {} · 总计 {}"),
        &[
            &report.count(CompareVerdict::New).to_string(),
            &report.count(CompareVerdict::Update).to_string(),
            &report.count(CompareVerdict::Rollback).to_string(),
            &report.count(CompareVerdict::Missing).to_string(),
            &report.abnormal().to_string(),
            &report.count(CompareVerdict::Same).to_string(),
            &report.results.len().to_string(),
        ],
    ));
    out.push('\n');

    if let Some(label) = view.label(text) {
        out.push_str(&fill(&text.get("筛选：{}"), &[&label]));
        out.push('\n');
    }

    let mut listed = 0usize;
    for (verdict, title) in [
        (CompareVerdict::Rollback, text.get("回退 {} 条")),
        (CompareVerdict::Update, text.get("升级 {} 条")),
        (CompareVerdict::New, text.get("新增 {} 条")),
        (CompareVerdict::Missing, text.get("缺失 {} 条")),
        (CompareVerdict::Same, text.get("相同 {} 条")),
    ] {
        if !view.shows(verdict) {
            continue;
        }
        let rows: Vec<&jar_compare::JarCompareResult> = report
            .results
            .iter()
            .filter(|result| result.compare_type == verdict)
            .collect();
        if rows.is_empty() {
            continue;
        }
        listed += rows.len();
        out.push('\n');
        out.push_str(&fill(&title, &[&rows.len().to_string()]));
        out.push('\n');
        out.push_str(&jar_rows(text, &rows));
    }

    if listed == 0 {
        out.push('\n');
        out.push_str(&text.get(if view == View::All {
            "两边没有差异"
        } else {
            "没有符合筛选条件的记录"
        }));
        out.push('\n');
    }
    out
}

/// 一组 jar 结论的逐行明细（服务 / 路径 / 两个版本 / 两边类数）。
fn jar_rows(text: &Text, rows: &[&jar_compare::JarCompareResult]) -> String {
    let headers = [
        text.get("服务"),
        text.get("JAR 路径"),
        text.get("基准版本"),
        text.get("对比版本"),
        text.get("类数（基准/对比）"),
    ];
    let server_w = column_width(
        rows.iter().map(|row| display_width(&row.server_name)),
        &headers[0],
        20,
    );
    let path_w = column_width(
        rows.iter().map(|row| display_width(&row.jar_name)),
        &headers[1],
        52,
    );
    let base_w = column_width(
        rows.iter()
            .map(|row| display_width(dash(&row.version_base))),
        &headers[2],
        16,
    );
    let target_w = column_width(
        rows.iter()
            .map(|row| display_width(dash(&row.version_target))),
        &headers[3],
        16,
    );
    let counts: Vec<String> = rows
        .iter()
        .map(|row| format!("{}/{}", row.class_count_base, row.class_count_target))
        .collect();
    let count_w = column_width(
        counts.iter().map(|text| display_width(text)),
        &headers[4],
        20,
    );

    let mut out = String::new();
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}\n",
        cell(&headers[0], server_w),
        cell(&headers[1], path_w),
        cell(&headers[2], base_w),
        cell(&headers[3], target_w),
        cell(&headers[4], count_w),
    ));
    for (row, count) in rows.iter().zip(&counts) {
        let line = format!(
            "  {}  {}  {}  {}  {}",
            cell(&row.server_name, server_w),
            cell(&row.jar_name, path_w),
            cell(dash(&row.version_base), base_w),
            cell(dash(&row.version_target), target_w),
            cell(count, count_w),
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 环境名进文件名：路径分隔符、空格之类一律换成 `-`。
///
/// 环境名来自表单（可能是「上线包 / 财务」这种带斜杠的写法），直接拼进路径会
/// 在用户没想到的地方建出一层目录。
fn file_stem(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "compare".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 导出顺序：回退在前、相同在后。
///
/// 表格里可以按列排序，导出的 CSV 打开就是最终形态：`results` 的原序是「先交集
/// 后基准独有」，异常会散落在中间，几百行里找一条回退比什么都费劲。
fn severity(verdict: CompareVerdict) -> u8 {
    match verdict {
        CompareVerdict::Rollback => 0,
        CompareVerdict::Update => 1,
        CompareVerdict::New => 2,
        CompareVerdict::Missing => 3,
        CompareVerdict::Same => 4,
    }
}

/// 按 [`severity`] 排好的结果（导出用）。
fn ordered<T>(results: &[T], verdict: impl Fn(&T) -> CompareVerdict) -> Vec<&T> {
    let mut rows: Vec<&T> = results.iter().collect();
    rows.sort_by_key(|row| severity(verdict(row)));
    rows
}

/// 一行的 CSV 单元格：带逗号/引号/换行的加引号（RFC 4180 口径）。
fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// 把表格写成 CSV（UTF-8 BOM 开头，Excel 双击打开不乱码），返回报告末尾那一行。
///
/// 参考页那两个按钮（CSV / Excel）**都是 CSV**：`exportExcel` 用的也是
/// `write_text_file` + `.csv` 后缀，只是引号风格不同。这里只留一份正确的。
fn write_csv(
    text: &Text,
    dir: &str,
    name: &str,
    header: &[String],
    rows: &[Vec<String>],
) -> Result<Option<String>, duck_plugin::PluginError> {
    if dir.is_empty() {
        return Ok(None);
    }
    let folder = std::path::Path::new(dir);
    if !folder.is_dir() {
        return Err(fail(text, "CSV 导出目录不是有效目录"));
    }
    let mut body = String::from("\u{feff}");
    body.push_str(
        &header
            .iter()
            .map(|cell| csv_cell(cell))
            .collect::<Vec<_>>()
            .join(","),
    );
    body.push('\n');
    for row in rows {
        body.push_str(
            &row.iter()
                .map(|cell| csv_cell(cell))
                .collect::<Vec<_>>()
                .join(","),
        );
        body.push('\n');
    }
    let path = folder.join(name);
    std::fs::write(&path, body).map_err(|error| {
        duck_plugin::PluginError::Execution(fill(
            &text.get("写入 CSV 失败: {}"),
            &[&error.to_string()],
        ))
    })?;
    Ok(Some(fill(
        &text.get("CSV：{}（{} 行）"),
        &[&path.to_string_lossy(), &rows.len().to_string()],
    )))
}

/// 写 class 比对的 CSV 明细（列与参考页导出一致，状态列已本地化）。
fn export_class_csv(
    text: &Text,
    spec: &class_compare::CompareSpec,
    report: &class_compare::ClassCompareReport,
    dir: &str,
) -> Result<Option<String>, duck_plugin::PluginError> {
    let header: Vec<String> = [
        "状态",
        "服务",
        "类名",
        "包名",
        "基准版本",
        "对比版本",
        "级差",
        "基准文件",
        "对比文件",
    ]
    .iter()
    .map(|key| text.get(key))
    .collect();
    let rows: Vec<Vec<String>> = ordered(&report.results, |row| row.compare_type)
        .into_iter()
        .map(|row| {
            vec![
                verdict_label(text, row.compare_type, row.is_abnormal),
                row.server_name.clone(),
                row.class_name.clone(),
                row.package_name.clone(),
                row.version_base.clone(),
                row.version_target.clone(),
                row.diff_version_num.to_string(),
                row.file_path_base.clone(),
                row.file_path_target.clone(),
            ]
        })
        .collect();
    let name = format!(
        "class-compare-{}-vs-{}.csv",
        file_stem(&spec.base_env),
        file_stem(&spec.target_env)
    );
    write_csv(text, dir, &name, &header, &rows)
}

/// 写 jar 比对的 CSV 明细。
///
/// 比参考页多两列：`对比类型`（原样英文，给脚本用）与 `是否异常`——页面上这两项
/// 只体现在颜色与图标里，导出成表就丢了，脚本再判一次颜色是没法判的。
fn export_jar_csv(
    text: &Text,
    spec: &jar_compare::JarCompareSpec,
    report: &jar_compare::JarCompareReport,
    dir: &str,
) -> Result<Option<String>, duck_plugin::PluginError> {
    let header: Vec<String> = [
        "状态",
        "JAR 包名",
        "服务",
        "基准版本",
        "对比版本",
        "基准类数",
        "对比类数",
        "对比类型",
        "是否异常",
        "基准文件",
        "对比文件",
    ]
    .iter()
    .map(|key| text.get(key))
    .collect();
    let rows: Vec<Vec<String>> = ordered(&report.results, |row| row.compare_type)
        .into_iter()
        .map(|row| {
            vec![
                verdict_label(text, row.compare_type, row.is_abnormal),
                row.jar_name.clone(),
                row.server_name.clone(),
                row.version_base.clone(),
                row.version_target.clone(),
                row.class_count_base.to_string(),
                row.class_count_target.to_string(),
                row.compare_type.as_str().to_string(),
                text.get(if row.is_abnormal { "是" } else { "否" }),
                row.jar_path_base.clone(),
                row.jar_path_target.clone(),
            ]
        })
        .collect();
    let name = format!(
        "jar-compare-{}-vs-{}.csv",
        file_stem(&spec.base_env),
        file_stem(&spec.target_env)
    );
    write_csv(text, dir, &name, &header, &rows)
}
