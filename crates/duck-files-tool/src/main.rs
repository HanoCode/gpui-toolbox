//! 文件批处理族（`duck.files.*`）的 **process 轨入口**。
//!
//! 与 SQL / 合并 / 比对三族同一个骨架：一个二进制服务整族工具，按 `request.tool`
//! 分派到 [`duck_files_core`]。新增一个工具＝加一条 `match` 分支 + 一份 `plugin.toml`。
//!
//! 本文件只做四件事（逻辑全在内核里，见各模块头）：
//!
//! 1. 把 `[io]` 参数摊平成内核入参；
//! 2. 给长任务发进度（遍历 / 转换都是「按文件推进」的，进度条才有意义）；
//! 3. 把内核的产出排成**可复制的文本报告**（宿主的结果表渲染器还没做，见计划 §8）；
//! 4. 把内核的「稳定码」错误翻成当前语言的一句话。
//!
//! ## 撤销日志落在哪
//!
//! 批量改写要在**下一次运行**里还能撤，所以日志必须落盘。插件进程拿不到宿主的
//! 数据目录（协议里没有这个字段），但二进制就在 `<plugin>/bin/run`——从
//! `current_exe()` 往上两级就是插件目录，日志放 `<plugin>/data/undo/`。
//! `DUCK_PLUGIN_DATA` 可覆盖（测试与排障用）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use duck_files_core::{
    AddressCountry, AddressOptions, BatchMode, ChartOptions, ChartPalette, ChartType,
    CustomerOptions, DataOutput, DateTimeKind, ErrorKind, FilesError, Gender, HolderType,
    InternetKind, Lang, NameOptions, NumberKind, OutFormat, RenameRule, ReplaceRule, SearchMode,
    SearchOptions, TreeOptions, WalkOptions, apply_rename, apply_replace, convert_file,
    delimiter_from_value, list_undo, names as random_names, parse_columns, parse_delimited,
    parse_fixed, parse_names, parse_table, parse_with_delimiter, plan_rename, preview_replace,
    search, to_svg_chart, undo_last,
};
use duck_plugin::PluginLocales;
use duck_plugin_process::{PluginOutput, RunRequest, serve};

mod i18n;
mod report;

/// 预览表最多列多少行（再多用户也读不过来，真正要的是「先扫一眼对不对」）。
const MAX_PREVIEW_ROWS: usize = 200;
/// 批量转换的报告最多列多少行。
const MAX_CONVERT_ROWS: usize = 60;
/// 内容搜索每个文件最多报几处（与参考仓 `locate_matches(.., 5)` 同口径）。
const MAX_HITS_PER_FILE: usize = 5;
/// 内容搜索跳过的单文件大小上限。
const MAX_CONTENT_BYTES: u64 = 2 * 1024 * 1024;

/// 参数区（与 `duck-compare-tool` 同一套：宿主把表单值序列化成字符串键值对）。
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

    /// 默认**开**的开关：键缺席时返回 `fallback`。
    ///
    /// `flag()` 把「键不在 params 里」当成 false，而 `[io.params]` 的 `default = "true"`
    /// 是**宿主渲染表单时**才填进去的——直接构造请求（CLI、测试、将来的脚本调用）时
    /// 键是缺席的，用 `flag()` 会把「首行为表头」静默翻成 false，整张表错位一行。
    fn flag_or(&self, key: &str, fallback: bool) -> bool {
        if self.0.get(key).is_none() {
            return fallback;
        }
        self.flag(key)
    }

    /// 数值参数：空串 / 解析失败 → `fallback`（不报错：留空就是「用默认」）。
    fn number(&self, key: &str, fallback: usize) -> usize {
        self.text(key).parse().unwrap_or(fallback)
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
        // `DUCK_LOCALES_DIR` 已经是 `locales/` 这一级：用 `load` 会再拼一次目录。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(std::path::Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    fn get(&self, source: &str) -> String {
        self.locales.text(&self.lang, source).to_owned()
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
        "duck.files.search" => search_run(request, out, &text),
        "duck.files.tree" => tree_run(request, out, &text),
        "duck.files.batch-edit" => batch_run(request, out, &text),
        "duck.files.csv-tools" => csv_run(request, out, &text),
        "duck.files.doc-convert" => convert_run(request, out, &text),
        "duck.text.data-format" => data_format_run(request, out, &text),
        "duck.text.random-gen" => random_gen_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-files-tool: unknown tool {other}"
        ))),
    }
}

/// 读输入：选了文件就读文件（UTF-8），没选就用输入框的文本。
///
/// `csv_run` 是内联写的同一个判断，这里抽出来给两个新工具用——它的输入字段叫
/// `input`、路径参数叫 `file`，与 CSV 工具的 `data` 不同，所以没有回头改 `csv_run`。
fn read_pasted_or_file(
    request: &RunRequest,
    params: &Params,
    field: &str,
    path_key: &str,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let path = params.text(path_key);
    if path.is_empty() {
        let pasted = params.text(field);
        return Ok(if pasted.is_empty() {
            request.input.clone()
        } else {
            pasted
        });
    }
    let file = duck_files_core::require_file(&path).map_err(|error| fail(text, &error))?;
    std::fs::read_to_string(&file).map_err(|error| fail(text, &FilesError::io(error)))
}

/// 固定分隔符段的「自定义」分隔符解析（参考页 `resolveDelimiter`）。
///
/// 下拉里选的是 `comma` / `tab` / … 这类代号，`custom` 才去读文本框；文本框空着
/// 时参考页回落 `|`，照抄。
fn resolve_delimiter(params: &Params) -> String {
    let value = params.text("delim_delimiter");
    if value == "custom" {
        let custom = params.text("delim_custom");
        if custom.is_empty() {
            "|".to_owned()
        } else {
            custom
        }
    } else {
        delimiter_from_value(&value)
            .map(|delimiter| delimiter.to_string())
            .unwrap_or_else(|| ",".to_owned())
    }
}

/// 数据格式转换（`duck.text.data-format`）：CSV / 定长文件 / 固定分隔符三段。
///
/// 与参考页 `DataFormat.vue` 的三段一一对应；`mode` 段决定解析器，输出格式由
/// `csv_output` / `fixed_output` / `delim_output` 三个参数各自的选项集决定
/// （参考页也是三套选项，因为每段都有一个「原样返回」的格式）。
fn data_format_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let mode = {
        let value = params.text("mode");
        if value.is_empty() {
            "csv".to_owned()
        } else {
            value
        }
    };
    let raw = read_pasted_or_file(request, &params, "input", "file", text)?;
    if raw.trim().is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }
    let prefix = text.get("列");

    let mut columns: Vec<duck_files_core::Column> = Vec::new();
    let mut delimiter_text = String::new();
    let grid = match mode.as_str() {
        "fixed" => {
            columns = parse_columns(&params.text("fixed_columns"), &prefix)
                .map_err(|error| fail(text, &error))?;
            parse_fixed(&raw, &columns, params.flag_or("fixed_trim", true))
        }
        "delim" => {
            delimiter_text = resolve_delimiter(&params);
            let names = parse_names(&params.text("delim_columns"));
            parse_with_delimiter(
                &raw,
                &delimiter_text,
                params.flag_or("delim_has_header", true),
                &names,
                params.flag_or("delim_trim", true),
                &prefix,
            )
        }
        _ => parse_delimited(
            &raw,
            delimiter_from_value(&params.text("csv_delimiter")),
            params.flag_or("csv_has_header", true),
            &prefix,
        ),
    };

    if grid.rows.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }

    let (output_key, fallback) = match mode.as_str() {
        "fixed" => ("fixed_output", DataOutput::Fixed),
        "delim" => ("delim_output", DataOutput::Delimited),
        _ => ("csv_output", DataOutput::Csv),
    };
    let output = DataOutput::parse(&params.text(output_key)).unwrap_or(fallback);
    let table_name = params.text(&format!("{mode}_table_name"));
    let body = output.render(&grid, &table_name, &delimiter_text, &columns);

    out.progress(
        100,
        Some(100),
        Some(&fill(
            &text.get("{} 行 × {} 列"),
            &[
                &grid.row_count().to_string(),
                &grid.column_count().to_string(),
            ],
        )),
    );
    Ok(body)
}

/// 随机文本生成（`duck.text.random-gen`）：六类字段。
///
/// `category` 对应参考页的六个标签页；每类自己的旋钮用 `visible_when` 控制显隐，
/// 隐藏的参数不会进 `request.params`。
fn random_gen_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let category = params.text("category");
    let count = params.number("count", 10).clamp(1, 500);
    let mut rng = duck_files_core::Rng::seeded();

    let body = match category.as_str() {
        "address" => {
            let options = AddressOptions {
                country: AddressCountry::parse(&params.text("address_country")),
                with_postal_code: params.flag_or("address_postcode", true),
                with_coordinate: params.flag("address_coordinate"),
            };
            duck_files_core::addresses(&mut rng, options, count).join("\n")
        }
        "internet" => duck_files_core::internet(
            &mut rng,
            InternetKind::parse(&params.text("internet_type")),
            count,
        )
        .join("\n"),
        "datetime" => duck_files_core::datetime(
            &mut rng,
            DateTimeKind::parse(&params.text("datetime_type")),
            count,
        )
        .join("\n"),
        "number" => duck_files_core::numbers(
            &mut rng,
            NumberKind::parse(&params.text("number_type")),
            count,
        )
        .join("\n"),
        "customer" => {
            let country = {
                let value = params.text("customer_country");
                if value.is_empty() {
                    "CN".to_owned()
                } else {
                    value
                }
            };
            let options = CustomerOptions {
                country,
                holder_type: HolderType::parse(&params.text("customer_holder")),
                non_resident_only: params.flag("customer_non_resident"),
            };
            let get = |source: &str| text.get(source);
            let labels = report::Labels { get: &get };
            (0..count)
                .map(|_| report::render(&duck_files_core::customer(&mut rng, &options), &labels))
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        _ => {
            let options = NameOptions {
                lang: Lang::parse(&params.text("name_lang")),
                gender: Gender::parse(&params.text("name_gender")),
                with_company: params.flag("name_company"),
            };
            random_names(&mut rng, options, count).join("\n")
        }
    };

    out.progress(
        100,
        Some(100),
        Some(&fill(&text.get("已生成 {} 条"), &[&count.to_string()])),
    );
    Ok(body)
}

/// 内核错误（稳定码 + 细节）→ 当前语言的一句话。
///
/// 细节（路径、`regex` 原文）不翻译：那是数据，翻译它没有意义。
fn fail(text: &Text, error: &FilesError) -> duck_plugin::PluginError {
    let base = text.get(match error.kind() {
        ErrorKind::PathMissing => "路径不存在",
        ErrorKind::NotADirectory => "路径不是目录",
        ErrorKind::NotAFile => "路径不是文件",
        ErrorKind::IllegalPath => "路径包含非法字符 '..'，拒绝访问",
        ErrorKind::BlockedPath => "拒绝访问系统路径",
        ErrorKind::NotUtf8 => "文件不是 UTF-8 编码，无法处理",
        ErrorKind::BadPattern => "正则表达式写错了",
        ErrorKind::EmptyInput => "输入是空的，没有可处理的内容",
        ErrorKind::Unsupported => "这种格式不支持",
        ErrorKind::TooLarge => "文件太大",
        ErrorKind::Conflict => "目标已存在，改名会覆盖",
        ErrorKind::UndoUnavailable => "没有可撤销的记录",
        ErrorKind::BadConfig => "配置写错了，检查一下再试",
        ErrorKind::Io => "读写文件失败",
    });
    match error.detail() {
        Some(detail) => duck_plugin::PluginError::Execution(format!("{base}：{detail}")),
        None => duck_plugin::PluginError::Execution(base),
    }
}

/// 逗号（中英文）、空白、换行分隔的模式串。
fn patterns(raw: &str) -> Vec<String> {
    raw.split([',', '，', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 插件自己的数据目录（撤销日志落这里）。
fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DUCK_PLUGIN_DATA") {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(|plugin| plugin.join("data")))
        .unwrap_or_else(|| std::env::temp_dir().join("duck-files-data"))
}

fn undo_dir() -> PathBuf {
    data_dir().join("undo")
}

// ── 报告排版 ───────────────────────────────────────────────────────────

/// 终端显示宽度：东亚宽字符算 2 列。
///
/// 结果区是等宽字体，「命中」按 `chars().count()` 只有 2，视觉上占 4 列，
/// 一补空格整列就歪。不引 `unicode-width`：本工具只会遇到中日韩文字与全角标点。
fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD
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

/// `fill` 的薄封装：第一个参数是按 `{}` 分段的模板。
fn fill(template: &str, args: &[&str]) -> String {
    i18n::fill(template, args)
}

// ── 文件搜索 ───────────────────────────────────────────────────────────

fn search_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir = params.text("dir");
    if dir.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }
    let mode = match params.text("mode").as_str() {
        "filename" => SearchMode::Filename,
        _ => SearchMode::Content,
    };

    let options = SearchOptions {
        mode,
        pattern: params.text("pattern"),
        is_regex: params.flag("is_regex"),
        case_sensitive: params.flag("case_sensitive"),
        whole_word: params.flag("whole_word"),
        max_results: params.number("max_results", 500).clamp(1, 5_000),
        max_hits_per_file: MAX_HITS_PER_FILE,
        max_file_bytes: MAX_CONTENT_BYTES,
        walk: WalkOptions {
            max_depth: params.number("max_depth", 0),
            include_hidden: params.flag("show_hidden"),
            include: patterns(&params.text("include")),
            exclude: patterns(&params.text("exclude")),
            max_entries: 200_000,
            follow_links: false,
        },
    };

    out.progress(10, Some(100), Some(&text.get("正在遍历目录…")));
    let outcome = search(Path::new(&dir), &options).map_err(|error| fail(text, &error))?;
    out.progress(100, Some(100), Some(&text.get("搜索完成")));
    Ok(search_report(text, &dir, &options, &outcome))
}

fn search_report(
    text: &Text,
    dir: &str,
    options: &SearchOptions,
    outcome: &duck_files_core::SearchOutcome,
) -> String {
    let mut report = String::new();
    report.push_str(&fill(&text.get("搜索范围：{}"), &[dir]));
    report.push('\n');
    report.push_str(&format!(
        "{} · {} · {}",
        fill(
            &text.get("关键词：{}"),
            &[&format!("\"{}\"", options.pattern)]
        ),
        text.get(match options.mode {
            SearchMode::Filename => "按文件名",
            SearchMode::Content => "按内容",
        }),
        fill(&text.get("命中 {} 处"), &[&outcome.hits.len().to_string()],)
    ));
    if options.whole_word {
        // 只在开了才提：报表头越长，真正要看的命中行越靠下。
        report.push_str(" · ");
        report.push_str(&text.get("全词"));
    }
    report.push('\n');
    let mut counts = fill(
        &text.get("扫描 {} 个文件"),
        &[&outcome.files_seen.to_string()],
    );
    if outcome.files_skipped > 0 {
        counts.push_str(" · ");
        counts.push_str(&fill(
            &text.get("跳过 {} 个（二进制或过大）"),
            &[&outcome.files_skipped.to_string()],
        ));
    }
    report.push_str(&counts);
    report.push('\n');
    if outcome.truncated {
        report.push_str(&fill(
            &text.get("结果已截断（只显示前 {} 处）"),
            &[&options.max_results.to_string()],
        ));
        report.push('\n');
    }
    report.push('\n');

    if outcome.hits.is_empty() {
        report.push_str(&text.get(match options.mode {
            SearchMode::Filename => "没有匹配的文件",
            SearchMode::Content => "没有匹配的内容",
        }));
        report.push('\n');
        return report;
    }

    if options.mode == SearchMode::Filename {
        for hit in &outcome.hits {
            report.push_str(&hit.rel);
            report.push('\n');
        }
        return report;
    }

    // 内容模式：路径对齐后再跟 `行:列`，扫读时眼睛能沿一列往下走。
    let width = outcome
        .hits
        .iter()
        .map(|hit| display_width(&hit.rel))
        .max()
        .unwrap_or(0)
        .min(60);
    for hit in &outcome.hits {
        let mut line = format!(
            "{}  {}:{}  {}",
            pad(&hit.rel, width),
            hit.line,
            hit.column,
            hit.text
        );
        if hit.file_hits > MAX_HITS_PER_FILE {
            line.push_str(&format!(
                "   {}",
                fill(
                    &text.get("（该文件共 {} 处）"),
                    &[&hit.file_hits.to_string()]
                )
            ));
        }
        report.push_str(line.trim_end());
        report.push('\n');
    }
    report
}

// ── 目录树 ─────────────────────────────────────────────────────────────

fn tree_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir = params.text("dir");
    let options = TreeOptions {
        max_depth: params.number("max_depth", 3).max(1),
        include_hidden: params.flag("show_hidden"),
        show_size: params.flag("show_size"),
        dirs_only: params.flag("dirs_only"),
        max_entries: 5_000,
    };
    let markdown = params.text("format") == "markdown";

    let (body, stats) = if markdown {
        duck_files_core::render_markdown(Path::new(&dir), &options)
    } else {
        duck_files_core::render_text(Path::new(&dir), &options)
    }
    .map_err(|error| fail(text, &error))?;

    out.progress(100, Some(100), Some(&text.get("目录读取完成")));
    let mut report = String::new();
    report.push_str(&fill(
        &text.get("{} 个目录 · {} 个文件 · 共 {}"),
        &[
            &stats.dirs.to_string(),
            &stats.files.to_string(),
            &duck_files_core::human_size(stats.bytes),
        ],
    ));
    report.push('\n');
    if stats.truncated {
        report.push_str(&fill(
            &text.get("已截断（深度上限 {}，或条目超过 {}）"),
            &[
                &options.max_depth.to_string(),
                &options.max_entries.to_string(),
            ],
        ));
        report.push('\n');
    }
    report.push('\n');
    report.push_str(&body);
    Ok(report)
}

// ── 批量改写 ───────────────────────────────────────────────────────────

fn batch_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let action = params.text("action");
    if action == "undo" {
        return undo_run(out, text);
    }
    if action == "undo-list" {
        let entries = list_undo(&undo_dir()).map_err(|error| fail(text, &error))?;
        return Ok(undo_list_report(text, &entries));
    }

    let dir = params.text("dir");
    if dir.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }
    let root = duck_files_core::require_dir(&dir).map_err(|error| fail(text, &error))?;
    let walk = WalkOptions {
        max_depth: params.number("max_depth", 0),
        include_hidden: params.flag("show_hidden"),
        include: patterns(&params.text("include")),
        exclude: vec!["*.bak".to_string()],
        max_entries: 20_000,
        follow_links: false,
    };

    let mode = if params.text("mode") == "replace" {
        BatchMode::Replace
    } else {
        BatchMode::Rename
    };
    out.progress(10, Some(100), Some(&text.get("正在读取文件清单…")));
    let (files, walk_outcome) =
        duck_files_core::collect_files(&root, &walk).map_err(|error| fail(text, &error))?;
    if files.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }

    let apply = action == "apply";
    match mode {
        BatchMode::Rename => rename_branch(text, &root, &files, &walk_outcome, &params, apply),
        BatchMode::Replace => {
            replace_branch(out, text, &root, &files, &walk_outcome, &params, apply)
        }
    }
}

fn rename_branch(
    text: &Text,
    root: &Path,
    files: &[duck_files_core::WalkEntry],
    walk_outcome: &duck_files_core::WalkOutcome,
    params: &Params,
    apply: bool,
) -> Result<String, duck_plugin::PluginError> {
    let rule = RenameRule {
        pattern: params.text("pattern"),
        replacement: params.text("replacement"),
        is_regex: params.flag("is_regex"),
        case_sensitive: params.flag("case_sensitive"),
        prefix: params.text("prefix"),
        suffix: params.text("suffix"),
    };
    let plan = plan_rename(root, files, &rule).map_err(|error| fail(text, &error))?;
    let changed: Vec<&duck_files_core::RenamePlanRow> =
        plan.iter().filter(|row| row.from != row.to).collect();
    let blocked: Vec<&duck_files_core::RenamePlanRow> = plan
        .iter()
        .filter(|row| row.ok && row.from != row.to)
        .collect();

    let mut report = String::new();
    report.push_str(&fill(
        &text.get("扫描 {} 个文件 · 需要改名 {} 个"),
        &[&files.len().to_string(), &changed.len().to_string()],
    ));
    report.push('\n');
    if walk_outcome.truncated {
        report.push_str(&text.get("文件清单已截断（达到条目上限）"));
        report.push('\n');
    }

    if !apply {
        report.push('\n');
        if changed.is_empty() {
            report.push_str(&text.get("没有需要改名的文件"));
            report.push('\n');
            return Ok(report);
        }
        let width = changed
            .iter()
            .map(|row| display_width(&row.from.display().to_string()))
            .max()
            .unwrap_or(0)
            .min(48);
        for row in changed.iter().take(MAX_PREVIEW_ROWS) {
            let from = row.from.display().to_string();
            let to = row.to.display().to_string();
            let mark = if row.ok {
                text.get("将改名")
            } else {
                format!("{}：{}", text.get("跳过"), reason_text(text, row.reason))
            };
            report.push_str(&format!("{}  →  {}  {}\n", pad(&from, width), to, mark));
        }
        if changed.len() > MAX_PREVIEW_ROWS {
            report.push_str(&fill(
                &text.get("……其余 {} 行未显示"),
                &[&(changed.len() - MAX_PREVIEW_ROWS).to_string()],
            ));
            report.push('\n');
        }
        report.push('\n');
        report.push_str(&text.get("确认无误后，把「动作」改成「应用」再运行一次"));
        report.push('\n');
        return Ok(report);
    }

    if blocked.is_empty() {
        report.push_str(&text.get("没有可应用的改名"));
        report.push('\n');
        return Ok(report);
    }
    let outcome =
        apply_rename(root, &plan, Some(&undo_dir())).map_err(|error| fail(text, &error))?;
    report.push('\n');
    report.push_str(&fill(
        &text.get("已改名 {} 个文件"),
        &[&outcome.applied.to_string()],
    ));
    report.push('\n');
    if !outcome.failed.is_empty() {
        report.push_str(&fill(
            &text.get("{} 个失败"),
            &[&outcome.failed.len().to_string()],
        ));
        report.push('\n');
        for (path, error) in outcome.failed.iter().take(MAX_PREVIEW_ROWS) {
            report.push_str(&format!("  {path}  {error}\n"));
        }
    }
    if let Some(id) = &outcome.undo_id {
        report.push_str(&fill(&text.get("撤销记录：{}"), &[id]));
        report.push('\n');
        report.push_str(&text.get("要回滚，把「动作」改成「撤销」再运行一次"));
        report.push('\n');
    }
    Ok(report)
}

fn reason_text(text: &Text, reason: Option<&'static str>) -> String {
    match reason {
        Some("bad_name") => text.get("新名字不合法（空或含路径分隔符）"),
        Some("duplicate_target") => text.get("本批里有文件改到同一个名字"),
        Some("target_exists") => text.get("目标文件已存在"),
        _ => text.get("未知原因"),
    }
}

fn replace_branch(
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
    root: &Path,
    files: &[duck_files_core::WalkEntry],
    walk_outcome: &duck_files_core::WalkOutcome,
    params: &Params,
    apply: bool,
) -> Result<String, duck_plugin::PluginError> {
    let rule = ReplaceRule {
        pattern: params.text("pattern"),
        replacement: params.text("replacement"),
        is_regex: params.flag("is_regex"),
        case_sensitive: params.flag("case_sensitive"),
    };
    if rule.pattern.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }
    let paths: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();

    let mut report = String::new();
    if apply {
        out.progress(40, Some(100), Some(&text.get("正在替换…")));
        let (results, undo_id) =
            apply_replace(&paths, &rule, Some(&undo_dir())).map_err(|error| fail(text, &error))?;
        let replaced: usize = results.iter().map(|row| row.replaced_count).sum();
        let touched = results.iter().filter(|row| row.replaced_count > 0).count();
        let failed: Vec<&duck_files_core::ReplaceResult> =
            results.iter().filter(|row| !row.ok).collect();
        report.push_str(&fill(
            &text.get("替换 {} 处，涉及 {} 个文件"),
            &[&replaced.to_string(), &touched.to_string()],
        ));
        report.push('\n');
        if !failed.is_empty() {
            report.push_str(&fill(
                &text.get("{} 个文件处理失败"),
                &[&failed.len().to_string()],
            ));
            report.push('\n');
            for row in failed.iter().take(MAX_PREVIEW_ROWS) {
                report.push_str(&format!(
                    "  {}  {}\n",
                    row.path,
                    row.error.clone().unwrap_or_default()
                ));
            }
        }
        if let Some(id) = &undo_id {
            report.push_str(&fill(&text.get("撤销记录：{}"), &[id]));
            report.push('\n');
            report.push_str(&text.get("要回滚，把「动作」改成「撤销」再运行一次"));
            report.push('\n');
        }
        return Ok(report);
    }

    let previews =
        preview_replace(&paths, &rule, MAX_HITS_PER_FILE).map_err(|error| fail(text, &error))?;
    let matched: Vec<&duck_files_core::ReplacePreview> =
        previews.iter().filter(|row| row.match_count > 0).collect();
    let total: usize = previews.iter().map(|row| row.match_count).sum();
    let unreadable = previews.iter().filter(|row| row.error.is_some()).count();

    report.push_str(&fill(
        &text.get("扫描 {} 个文件 · 命中 {} 处 · 涉及 {} 个文件"),
        &[
            &files.len().to_string(),
            &total.to_string(),
            &matched.len().to_string(),
        ],
    ));
    report.push('\n');
    if walk_outcome.truncated {
        report.push_str(&text.get("文件清单已截断（达到条目上限）"));
        report.push('\n');
    }
    if unreadable > 0 {
        report.push_str(&fill(
            &text.get("{} 个文件读不了（非 UTF-8 等）"),
            &[&unreadable.to_string()],
        ));
        report.push('\n');
    }
    report.push('\n');
    if matched.is_empty() {
        report.push_str(&text.get("没有命中任何内容"));
        report.push('\n');
        return Ok(report);
    }
    let width = matched
        .iter()
        .map(|row| display_width(&short_path(root, &row.path)))
        .max()
        .unwrap_or(0)
        .min(48);
    for row in matched.iter().take(MAX_PREVIEW_ROWS) {
        report.push_str(&format!(
            "{}  {}\n",
            pad(&short_path(root, &row.path), width),
            fill(&text.get("{} 处"), &[&row.match_count.to_string()])
        ));
        for hit in &row.matches {
            report.push_str(&format!(
                "    {}:{}  {}\n",
                hit.line,
                hit.column,
                hit.context.trim()
            ));
        }
    }
    if matched.len() > MAX_PREVIEW_ROWS {
        report.push_str(&fill(
            &text.get("……其余 {} 个文件未显示"),
            &[&(matched.len() - MAX_PREVIEW_ROWS).to_string()],
        ));
        report.push('\n');
    }
    report.push('\n');
    report.push_str(&text.get("确认无误后，把「动作」改成「应用」再运行一次"));
    report.push('\n');
    Ok(report)
}

/// 报告里把绝对路径缩成相对目录的路径（前缀太长会把行挤爆）。
fn short_path(root: &Path, path: &str) -> String {
    let path = Path::new(path);
    match path.strip_prefix(root) {
        Ok(relative) => duck_files_core::slash(relative),
        Err(_) => path.display().to_string(),
    }
}

fn undo_run(
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    out.progress(50, Some(100), Some(&text.get("正在撤销…")));
    let report = match undo_last(&undo_dir()) {
        Ok(report) => report,
        Err(error) if error.kind() == ErrorKind::UndoUnavailable => {
            return Ok(format!("{}\n", text.get("没有可撤销的记录")));
        }
        Err(error) => return Err(fail(text, &error)),
    };
    let mut out_text = String::new();
    out_text.push_str(&fill(
        &text.get("已恢复 {} 个文件"),
        &[&report.restored.to_string()],
    ));
    out_text.push('\n');
    out_text.push_str(&fill(&text.get("撤销记录：{}"), &[&report.id]));
    out_text.push('\n');
    if !report.failed.is_empty() {
        out_text.push_str(&fill(
            &text.get("{} 项恢复失败（记录已保留，可修好路径再撤）"),
            &[&report.failed.len().to_string()],
        ));
        out_text.push('\n');
        for (path, error) in report.failed.iter().take(MAX_PREVIEW_ROWS) {
            out_text.push_str(&format!("  {path}  {error}\n"));
        }
    }
    Ok(out_text)
}

fn undo_list_report(text: &Text, entries: &[duck_files_core::UndoEntryInfo]) -> String {
    if entries.is_empty() {
        return format!("{}\n", text.get("没有可撤销的记录"));
    }
    let mut report = fill(
        &text.get("可撤销记录：{} 条（新的在前）"),
        &[&entries.len().to_string()],
    );
    report.push('\n');
    report.push('\n');
    for entry in entries {
        report.push_str(&format!(
            "{}  {}  {}\n",
            pad(&entry.id, 18),
            pad(
                &text.get(match entry.kind.as_str() {
                    "rename" => "改名",
                    _ => "替换",
                }),
                8
            ),
            fill(&text.get("{} 个文件"), &[&entry.items.to_string()])
        ));
    }
    report
}

// ── CSV 工具 ───────────────────────────────────────────────────────────

fn csv_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let path = params.text("file");
    let raw = if path.is_empty() {
        // 没选文件就读输入框（`[io.fields]` 的 `data`）。
        let pasted = Params::new(&request.params).text("data");
        if pasted.is_empty() {
            request.input.clone()
        } else {
            pasted
        }
    } else {
        let file = duck_files_core::require_file(&path).map_err(|error| fail(text, &error))?;
        std::fs::read_to_string(&file).map_err(|error| fail(text, &FilesError::io(error)))?
    };
    if raw.trim().is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }

    let delimiter = match params.text("delimiter").as_str() {
        "comma" => Some(','),
        "tab" => Some('\t'),
        "semicolon" => Some(';'),
        "pipe" => Some('|'),
        _ => None,
    };
    let table = parse_table(&raw, delimiter).map_err(|error| fail(text, &error))?;

    if params.text("mode") == "chart" {
        return csv_chart(out, text, &params, &table, &path);
    }

    let export = params.text("format");
    let body = match export.as_str() {
        "json" => table.to_json().map_err(|error| fail(text, &error))?,
        "insert" => table.to_insert(&params.text("table_name")),
        _ => table.to_markdown(),
    };
    out.progress(
        100,
        Some(100),
        Some(&fill(
            &text.get("{} 行 × {} 列"),
            &[
                &table.row_count().to_string(),
                &table.column_count().to_string(),
            ],
        )),
    );
    Ok(body)
}

fn csv_chart(
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
    params: &Params,
    table: &duck_files_core::CsvTable,
    source_path: &str,
) -> Result<String, duck_plugin::PluginError> {
    let kind = match params.text("chart").as_str() {
        "line" => ChartType::Line,
        "pie" => ChartType::Pie,
        "doughnut" => ChartType::Doughnut,
        _ => ChartType::Bar,
    };
    let palette = match params.text("palette").as_str() {
        "soft" => ChartPalette::Soft,
        "vivid" => ChartPalette::Vivid,
        "mono" => ChartPalette::Mono,
        _ => ChartPalette::Default,
    };
    let value_columns: Vec<usize> = patterns(&params.text("value_columns"))
        .iter()
        .filter_map(|part| part.parse::<usize>().ok())
        .map(|index| index.saturating_sub(1))
        .collect();

    let options = ChartOptions {
        kind,
        label_column: None,
        value_columns,
        palette,
        show_legend: !params.flag("hide_legend"),
        show_grid: !params.flag("hide_grid"),
    };
    let svg = to_svg_chart(table, &options).map_err(|error| fail(text, &error))?;

    let dir = params.text("output_dir");
    let target = if dir.is_empty() {
        let base = if source_path.is_empty() {
            data_dir().join("charts")
        } else {
            Path::new(source_path)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| data_dir().join("charts"))
        };
        base.join(format!("{}.svg", chart_file_stem(source_path)))
    } else {
        let dir = duck_files_core::require_dir(&dir).map_err(|error| fail(text, &error))?;
        dir.join(format!("{}.svg", chart_file_stem(source_path)))
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| fail(text, &FilesError::io(error)))?;
    }
    std::fs::write(&target, svg).map_err(|error| fail(text, &FilesError::io(error)))?;

    // 预览通道：宿主 `kind = "preview"` 按 body 里的路径内联渲染 SVG
    // （gpui 图片管线对 SVG 走内置 resvg 光栅化，全彩）。路径必须是已落盘
    // 的文件——上面刚写完，成立。文本报告照旧发，复制 / 导出按钮用它。
    out.body(&serde_json::json!({ "preview": target.display().to_string() }).to_string());
    out.progress(100, Some(100), Some(&text.get("图表已生成")));
    let mut report = String::new();
    report.push_str(&fill(
        &text.get("已生成图表：{}"),
        &[&target.display().to_string()],
    ));
    report.push('\n');
    report.push_str(&fill(
        &text.get("{} 行 × {} 列 · 数值列 {}"),
        &[
            &table.row_count().to_string(),
            &table.column_count().to_string(),
            &if options.value_columns.is_empty() {
                text.get("全部")
            } else {
                options
                    .value_columns
                    .iter()
                    .map(|index| (index + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ],
    ));
    report.push('\n');
    report.push_str(&text.get("用浏览器或图片查看器打开即可（SVG 是文本，也能贴进文档）"));
    Ok(report)
}

/// 图表文件名：输入文件名（去扩展名），没有输入就叫 `chart`。
fn chart_file_stem(source_path: &str) -> String {
    Path::new(source_path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "chart".to_string())
}

// ── 格式转换 ───────────────────────────────────────────────────────────

fn convert_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let raw_format = params.text("format");
    let format = OutFormat::parse(&raw_format).ok_or_else(|| {
        fail(
            text,
            &FilesError::with_detail(ErrorKind::Unsupported, &raw_format),
        )
    })?;
    let output_dir = params.text("output_dir");
    let overwrite = params.flag("overwrite");

    if params.text("mode") == "batch" {
        return convert_batch(out, text, &params, format, &output_dir, overwrite);
    }

    let input = params.text("input_file");
    let path = duck_files_core::require_file(&input).map_err(|error| fail(text, &error))?;
    let content = convert_file(&path, format).map_err(|error| fail(text, &error))?;
    out.progress(100, Some(100), Some(&text.get("转换完成")));

    if output_dir.is_empty() {
        // 没选输出目录 = 只要预览（结果区可选中、可复制）。
        return Ok(content);
    }
    let dir = duck_files_core::require_dir(&output_dir).map_err(|error| fail(text, &error))?;
    let target = dir.join(format!(
        "{}.{}",
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string()),
        format.extension()
    ));
    if target.exists() && !overwrite {
        return Err(fail(
            text,
            &FilesError::with_detail(ErrorKind::Conflict, target.display()),
        ));
    }
    std::fs::write(&target, &content).map_err(|error| fail(text, &FilesError::io(error)))?;
    Ok(convert_one_report(text, &path, &target, &content))
}

fn convert_one_report(text: &Text, source: &Path, target: &Path, content: &str) -> String {
    let mut report = String::new();
    report.push_str(&fill(
        &text.get("已转换：{} → {}"),
        &[
            &source.file_name().unwrap_or_default().to_string_lossy(),
            &target.display().to_string(),
        ],
    ));
    report.push('\n');
    report.push_str(&fill(
        &text.get("输出 {} 个字符 · {} 行"),
        &[
            &content.chars().count().to_string(),
            &content.lines().count().to_string(),
        ],
    ));
    report.push('\n');
    report.push('\n');
    // 附前几行预览：用户最常问的就是「转出来对不对」。
    for line in content.lines().take(12) {
        report.push_str(line);
        report.push('\n');
    }
    if content.lines().count() > 12 {
        report.push_str(&text.get("……（预览只显示前 12 行，完整内容见输出文件）"));
        report.push('\n');
    }
    report
}

fn convert_batch(
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
    params: &Params,
    format: OutFormat,
    output_dir: &str,
    overwrite: bool,
) -> Result<String, duck_plugin::PluginError> {
    let input_dir = params.text("input_dir");
    if output_dir.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }
    let root = duck_files_core::require_dir(&input_dir).map_err(|error| fail(text, &error))?;
    let target_root =
        duck_files_core::require_dir(output_dir).map_err(|error| fail(text, &error))?;

    let walk = WalkOptions {
        max_depth: 0,
        include_hidden: false,
        include: SUPPORTED_EXTENSIONS
            .iter()
            .map(|ext| format!("*.{ext}"))
            .collect(),
        exclude: Vec::new(),
        max_entries: 5_000,
        follow_links: false,
    };
    let (files, _) =
        duck_files_core::collect_files(&root, &walk).map_err(|error| fail(text, &error))?;
    if files.is_empty() {
        return Err(fail(text, &FilesError::new(ErrorKind::EmptyInput)));
    }

    let total = files.len();
    let mut succeeded = 0usize;
    let mut skipped = 0usize;
    let mut rows: Vec<String> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for (index, file) in files.iter().enumerate() {
        out.progress(
            index as u64,
            Some(total as u64),
            Some(&fill(
                &text.get("正在转换 {}/{}"),
                &[&(index + 1).to_string(), &total.to_string()],
            )),
        );
        let relative = file.rel.with_extension(format.extension());
        let target = target_root.join(&relative);
        if target.exists() && !overwrite {
            skipped += 1;
            rows.push(format!(
                "{}  {}",
                file.rel_slash(),
                text.get("已存在，跳过")
            ));
            continue;
        }
        if let Some(error) = ensure_parent(&target) {
            failures.push((file.rel_slash(), error));
            continue;
        }
        match convert_file(&file.path, format) {
            Ok(content) => match std::fs::write(&target, &content) {
                Ok(()) => {
                    succeeded += 1;
                    rows.push(format!("{}  →  {}", file.rel_slash(), relative.display()));
                }
                Err(error) => failures.push((file.rel_slash(), error.to_string())),
            },
            Err(error) => failures.push((file.rel_slash(), error.to_string())),
        }
    }

    out.progress(
        total as u64,
        Some(total as u64),
        Some(&text.get("转换完成")),
    );
    let mut report = String::new();
    report.push_str(&fill(
        &text.get("共 {} 个文件 · 成功 {} 个 · 跳过 {} 个 · 失败 {} 个"),
        &[
            &total.to_string(),
            &succeeded.to_string(),
            &skipped.to_string(),
            &failures.len().to_string(),
        ],
    ));
    report.push('\n');
    report.push('\n');
    for row in rows.iter().take(MAX_CONVERT_ROWS) {
        report.push_str(row);
        report.push('\n');
    }
    if rows.len() > MAX_CONVERT_ROWS {
        report.push_str(&fill(
            &text.get("……其余 {} 个未显示"),
            &[&(rows.len() - MAX_CONVERT_ROWS).to_string()],
        ));
        report.push('\n');
    }
    if !failures.is_empty() {
        report.push('\n');
        report.push_str(&fill(
            &text.get("{} 个失败"),
            &[&failures.len().to_string()],
        ));
        report.push('\n');
        for (path, error) in failures.iter().take(MAX_CONVERT_ROWS) {
            report.push_str(&format!("  {path}  {error}\n"));
        }
    }
    Ok(report)
}

/// 批量转换：给目标文件建父目录，失败返回错误文本。
///
/// 抽成函数是为了让调用点只有一个 `if let`：写成 `if let Some(p) = .. { if let Err(e) = .. }`
/// 会触发 `clippy::collapsible_if`，而它建议的 `if a && let Some(x) = b` 需要
/// `let_chains`（Rust 1.88），本仓 MSRV 是 1.85。
fn ensure_parent(target: &Path) -> Option<String> {
    let parent = target.parent()?;
    std::fs::create_dir_all(parent)
        .err()
        .map(|error| error.to_string())
}

/// 批量转换会处理的扩展名（与 [`Format::from_extension`] 对齐）。
const SUPPORTED_EXTENSIONS: [&str; 21] = [
    "md",
    "markdown",
    "html",
    "htm",
    "txt",
    "json",
    "csv",
    "tsv",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "xml",
    "docx",
    "pptx",
    "xlsx",
    "xlsm",
    "pdf",
];
