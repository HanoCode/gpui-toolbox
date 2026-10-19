//! 格式转换：文档 / 配置 / 表格 → Markdown / HTML / 纯文本 / JSON / CSV。
//!
//! 参考仓 `converter/` 是一套 `ConvertOptions` + 十几个 `convert_*` 分支，主体逻辑
//! （`json_to_markdown` / `format_val` / `csv_to_markdown` / `csv_to_json` /
//! `toml_value_to_json` / `strip_html_tags` / `strip_markdown` / `markdown_to_html`）
//! 逐字复刻。三处**不得不**偏离，原因都在依赖上：
//!
//! 1. **`kreuzberg` 不能引**：参考仓锁的 4.10.2 声明 `rust-version = "1.91"`，
//!    而本仓硬门是 1.85。所以 docx / pptx 改成「ZIP 容器 + quick-xml 取文本运行」，
//!    xlsx 改用 `calamine`（参考仓的 `kreuzberg::excel` 底下也是它）。
//!    抽出来的正文与参考仓一致（段落一行、表格单元格制表符分隔），少了元数据与
//!    图片——本工具从来只输出正文。
//! 2. **PDF 只抽文字**：`pdf-extract` 与参考仓同版本。扫描件抽不出内容时按参考仓
//!    口径报错（"结果为空"），不做 OCR。
//! 3. **不出 PDF**：参考仓的 `pdf_gen` 用 `printpdf` + 内置 Helvetica——中文字形
//!    一个都没有，导出的中文 PDF 是空白。真要出 PDF 得内嵌一份 CJK 字体（10 MB+
//!    且带授权问题），不属于「格式转换」这个工具的职责。输出格式里因此没有 PDF，
//!    选不到就不会得到一个打不开的文件。
//!
//! ## 输入 → 输出矩阵（与参考仓 `list_supported_formats` 对齐，去掉 PDF 输出）
//!
//! | 输入 | md | html | txt | json | csv |
//! | --- | --- | --- | --- | --- | --- |
//! | docx / pptx / xlsx / pdf | ✓ | ✓ | ✓ | | |
//! | html | ✓ | ✓ | ✓ | | |
//! | json / yaml / toml | ✓ | | ✓ | ✓ | |
//! | csv | ✓ | | ✓ | ✓ | |
//! | ini / xml | ✓（围栏） | | ✓ | | |
//! | md | ✓ | ✓ | ✓ | | |
//! | txt | ✓ | | ✓ | | |

use std::path::Path;

use crate::error::{ErrorKind, FilesError, FilesResult};

/// 输入格式（按扩展名判定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
    Text,
    Json,
    Csv,
    Yaml,
    Toml,
    Ini,
    Xml,
    Docx,
    Pptx,
    Xlsx,
    Pdf,
    /// 不认识的扩展名（保留原文，便于报错时带上）。
    Unknown(String),
}

impl Format {
    /// 扩展名 → 格式（小写比较；`yml` 与 `yaml`、`htm` 与 `html` 同族）。
    pub fn from_extension(ext: &str) -> Self {
        match ext.trim_start_matches('.').to_lowercase().as_str() {
            "md" | "markdown" => Self::Markdown,
            "html" | "htm" => Self::Html,
            "txt" | "text" | "log" => Self::Text,
            "json" => Self::Json,
            "csv" | "tsv" => Self::Csv,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "ini" | "cfg" | "conf" | "properties" => Self::Ini,
            "xml" => Self::Xml,
            "docx" => Self::Docx,
            "pptx" => Self::Pptx,
            "xlsx" | "xlsm" => Self::Xlsx,
            "pdf" => Self::Pdf,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// 是不是「不是文本、要按二进制解析」的那一类。
    pub fn is_binary_container(&self) -> bool {
        matches!(self, Self::Docx | Self::Pptx | Self::Xlsx | Self::Pdf)
    }
}

/// 输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutFormat {
    Markdown,
    Html,
    Text,
    Json,
    Csv,
}

impl OutFormat {
    /// `md` / `html` / `txt` / `json` / `csv`（大小写不敏感，`markdown` 也认）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "md" | "markdown" => Some(Self::Markdown),
            "html" | "htm" => Some(Self::Html),
            "txt" | "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Text => "txt",
            Self::Json => "json",
            Self::Csv => "csv",
        }
    }
}

/// 按扩展名判定输入格式。
pub fn detect_format(path: &Path) -> Format {
    let ext = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    Format::from_extension(&ext)
}

/// 把文件转成目标格式的**文本**（不落盘：落盘由插件层决定，便于「先预览再保存」）。
pub fn convert_file(path: &Path, to: OutFormat) -> FilesResult<String> {
    if !path.exists() {
        return Err(FilesError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    if !path.is_file() {
        return Err(FilesError::with_detail(ErrorKind::NotAFile, path.display()));
    }
    let format = detect_format(path);
    match format {
        Format::Docx => render_office(&docx_text(path)?, to),
        Format::Pptx => render_office(&pptx_text(path)?, to),
        Format::Xlsx => render_office(&xlsx_text(path)?, to),
        Format::Pdf => render_office(&pdf_text(path)?, to),
        Format::Unknown(ext) => Err(FilesError::with_detail(ErrorKind::Unsupported, ext)),
        other => {
            let text = std::fs::read_to_string(path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    FilesError::new(ErrorKind::NotUtf8)
                } else {
                    FilesError::io(error)
                }
            })?;
            convert_text(&text, &other, to)
        }
    }
}

/// 文本 → 目标格式（纯函数，测试的主力入口）。
pub fn convert_text(text: &str, from: &Format, to: OutFormat) -> FilesResult<String> {
    match from {
        Format::Markdown => match to {
            OutFormat::Markdown | OutFormat::Text => Ok(if to == OutFormat::Markdown {
                text.to_string()
            } else {
                strip_markdown(text)
            }),
            OutFormat::Html => Ok(markdown_to_html(text)),
            _ => Err(unsupported(format!("markdown → {to:?}"))),
        },
        Format::Html => match to {
            OutFormat::Html => Ok(text.to_string()),
            OutFormat::Text => Ok(strip_html_tags(text)),
            OutFormat::Markdown => Ok(html_to_markdown(text)),
            _ => Err(unsupported(format!("html → {to:?}"))),
        },
        Format::Json => match to {
            OutFormat::Json | OutFormat::Text => Ok(text.to_string()),
            OutFormat::Markdown => json_to_markdown(text),
            _ => Err(unsupported(format!("json → {to:?}"))),
        },
        Format::Csv => match to {
            OutFormat::Csv | OutFormat::Text => Ok(text.to_string()),
            OutFormat::Markdown => csv_to_markdown(text),
            OutFormat::Json => csv_to_json(text),
            OutFormat::Html => Err(unsupported(format!("csv → {to:?}"))),
        },
        Format::Yaml => match to {
            OutFormat::Text => Ok(text.to_string()),
            OutFormat::Json => yaml_to_json(text),
            OutFormat::Markdown => yaml_to_json(text).and_then(|json| json_to_markdown(&json)),
            OutFormat::Html | OutFormat::Csv => Err(unsupported(format!("yaml → {to:?}"))),
        },
        Format::Toml => match to {
            OutFormat::Text => Ok(text.to_string()),
            OutFormat::Json => toml_to_json(text),
            OutFormat::Markdown => toml_to_json(text).and_then(|json| json_to_markdown(&json)),
            OutFormat::Html | OutFormat::Csv => Err(unsupported(format!("toml → {to:?}"))),
        },
        Format::Ini | Format::Xml => match to {
            OutFormat::Text => Ok(text.to_string()),
            // 结构与参考仓一致：包一层带语言标记的围栏（粘进文档能高亮）。
            OutFormat::Markdown => Ok(format!(
                "```{}\n{}\n```",
                if matches!(from, Format::Ini) {
                    "ini"
                } else {
                    "xml"
                },
                text.trim_end()
            )),
            _ => Err(unsupported(format!("{from:?} → {to:?}"))),
        },
        Format::Text => match to {
            OutFormat::Text | OutFormat::Markdown => Ok(text.to_string()),
            _ => Err(unsupported(format!("text → {to:?}"))),
        },
        Format::Docx | Format::Pptx | Format::Xlsx | Format::Pdf => Err(FilesError::with_detail(
            ErrorKind::Unsupported,
            "binary input needs a file path",
        )),
        Format::Unknown(ext) => Err(FilesError::with_detail(ErrorKind::Unsupported, ext)),
    }
}

fn unsupported(detail: String) -> FilesError {
    FilesError::with_detail(ErrorKind::Unsupported, detail)
}

/// Office / PDF 抽出来的正文一律是 Markdown 形态（段落 + 表格），再按目标格式收尾。
fn render_office(text: &str, to: OutFormat) -> FilesResult<String> {
    match to {
        OutFormat::Markdown => Ok(text.to_string()),
        OutFormat::Text => Ok(strip_markdown(text)),
        OutFormat::Html => Ok(markdown_to_html(text)),
        OutFormat::Json | OutFormat::Csv => Err(unsupported(format!("office → {to:?}"))),
    }
}

// ── 二进制容器 ──────────────────────────────────────────────────────────

/// 读 ZIP 里的一个条目。
fn read_zip_entry(path: &Path, name: &str) -> FilesResult<String> {
    let file = std::fs::File::open(path).map_err(FilesError::io)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("zip: {error}"))
    })?;
    let mut entry = archive.by_name(name).map_err(|_| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("missing entry: {name}"))
    })?;
    let mut text = String::new();
    use std::io::Read;
    entry.read_to_string(&mut text).map_err(FilesError::io)?;
    Ok(text)
}

/// ZIP 里所有以 `prefix` 开头、以 `suffix` 结尾的条目名，按其中的数字排序。
///
/// 幻灯片必须按序号排：`slide1.xml` < `slide10.xml` < `slide2.xml` 是字典序的坑，
/// 而汇报材料的页序恰恰全靠文件名。
fn zip_entries_matching(path: &Path, prefix: &str, suffix: &str) -> FilesResult<Vec<String>> {
    let file = std::fs::File::open(path).map_err(FilesError::io)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("zip: {error}"))
    })?;
    let mut names: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        let name = entry.name().to_string();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            names.push(name);
        }
    }
    names.sort_by_key(|name| numeric_suffix(name));
    Ok(names)
}

/// 文件名里最后一个连续数字（`ppt/slides/slide12.xml` → 12）。
fn numeric_suffix(name: &str) -> u64 {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let digits: String = stem
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().unwrap_or(0)
}

/// Word：`word/document.xml` 的正文。
fn docx_text(path: &Path) -> FilesResult<String> {
    let xml = read_zip_entry(path, "word/document.xml")?;
    extract_runs(&xml, "w:t", "w:p", Some("w:tc"), Some("w:tr"))
}

/// PowerPoint：每页一段，页间用 `---` 分隔（贴进 Markdown 就是分页）。
fn pptx_text(path: &Path) -> FilesResult<String> {
    let slides = zip_entries_matching(path, "ppt/slides/slide", ".xml")?;
    if slides.is_empty() {
        return Err(FilesError::with_detail(ErrorKind::Unsupported, "no slides"));
    }
    let mut out = String::new();
    for (index, slide) in slides.iter().enumerate() {
        let xml = read_zip_entry(path, slide)?;
        let text = extract_runs(&xml, "a:t", "a:p", None, None)?;
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if index > 0 {
            out.push_str("\n---\n\n");
        }
        out.push_str(text);
        out.push('\n');
    }
    Ok(out)
}

/// Excel：每个 sheet 一张 Markdown 表，标题 `=== 名称 ===`（与参考仓同形）。
fn xlsx_text(path: &Path) -> FilesResult<String> {
    use calamine::{Reader, open_workbook_auto};
    let mut workbook = open_workbook_auto(path).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("xlsx: {error}"))
    })?;
    let sheets = workbook.sheet_names().to_vec();
    let mut out = String::new();
    for (index, name) in sheets.iter().enumerate() {
        let Ok(range) = workbook.worksheet_range(name) else {
            continue;
        };
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("=== {name} ===\n"));
        let mut rows = range.rows();
        let Some(header) = rows.next() else {
            continue;
        };
        let width = header.len();
        out.push_str(&format!(
            "| {} |\n",
            header.iter().map(cell_text).collect::<Vec<_>>().join(" | ")
        ));
        out.push_str(&format!("|{}|\n", " --- |".repeat(width)));
        for row in rows {
            let cells: Vec<String> = row.iter().map(cell_text).collect();
            if cells.iter().all(|cell| cell.is_empty()) {
                continue;
            }
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
    }
    Ok(out)
}

fn cell_text(cell: &calamine::Data) -> String {
    let text = match cell {
        calamine::Data::Empty => String::new(),
        calamine::Data::String(value) => value.clone(),
        calamine::Data::Float(value) => {
            if value.fract().abs() < f64::EPSILON {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        }
        calamine::Data::Int(value) => value.to_string(),
        calamine::Data::Bool(value) => value.to_string(),
        calamine::Data::DateTime(value) => value.to_string(),
        calamine::Data::DateTimeIso(value) => value.clone(),
        calamine::Data::DurationIso(value) => value.clone(),
        calamine::Data::Error(error) => format!("#{error:?}"),
    };
    text.replace('|', "\\|").replace('\n', " ")
}

/// PDF：只抽文字（扫描件没有文字层，会走「结果为空」那条错误）。
fn pdf_text(path: &Path) -> FilesResult<String> {
    let text = pdf_extract::extract_text(path).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("pdf: {error}"))
    })?;
    if text.trim().is_empty() {
        return Err(FilesError::with_detail(
            ErrorKind::Unsupported,
            "empty pdf text",
        ));
    }
    Ok(text)
}

/// 从 OOXML 里取文本运行。
///
/// `text_tag`（`w:t` / `a:t`）里的字是正文，`para_tag` 收尾时换行；给了
/// `cell_tag` / `row_tag` 时按单元格制表符、按行换行（表格粘进 Excel 能自动分列）。
fn extract_runs(
    xml: &str,
    text_tag: &str,
    para_tag: &str,
    cell_tag: Option<&str>,
    row_tag: Option<&str>,
) -> FilesResult<String> {
    use quick_xml::events::Event;
    use quick_xml::events::Event::{CData, Empty, End, Start, Text};

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut in_text = false;
    // 段落里一行都没写过时不要凭空多一个空行（Word 里空段落很常见）。
    let mut paragraph_dirty = false;
    // 在表格单元格里：段落结束**不换行**（换行交给单元格的 `\t` 与行的 `\n`），
    // 否则一个单元格里的段落会把自己的内容挤到下一行，表格对不齐。
    let mut cell_depth = 0usize;

    loop {
        match reader.read_event() {
            Ok(Start(element)) => {
                let name = element.name();
                if name.as_ref() == text_tag.as_bytes() {
                    in_text = true;
                } else if Some(name.as_ref()) == cell_tag.map(str::as_bytes) {
                    cell_depth += 1;
                }
            }
            Ok(Empty(element)) => {
                let name = element.name();
                // `<w:tab/>` 是制表符，`<w:br/>` 是软换行；这两者都不带文本内容。
                match name.as_ref() {
                    b"w:tab" => {
                        out.push('\t');
                        paragraph_dirty = true;
                    }
                    b"w:br" => {
                        out.push('\n');
                        paragraph_dirty = true;
                    }
                    _ => {}
                }
            }
            Ok(Text(text)) => {
                if in_text {
                    let value = text.unescape().map_err(|error| {
                        FilesError::with_detail(ErrorKind::Unsupported, error.to_string())
                    })?;
                    out.push_str(&value);
                    paragraph_dirty = true;
                }
            }
            Ok(CData(data)) => {
                if in_text {
                    out.push_str(&String::from_utf8_lossy(&data));
                    paragraph_dirty = true;
                }
            }
            Ok(End(element)) => {
                let name = element.name();
                if name.as_ref() == text_tag.as_bytes() {
                    in_text = false;
                } else if name.as_ref() == para_tag.as_bytes() {
                    if paragraph_dirty && cell_depth == 0 {
                        out.push('\n');
                        paragraph_dirty = false;
                    }
                } else if Some(name.as_ref()) == cell_tag.map(str::as_bytes) {
                    cell_depth = cell_depth.saturating_sub(1);
                    out.push('\t');
                    paragraph_dirty = true;
                } else if Some(name.as_ref()) == row_tag.map(str::as_bytes) {
                    out.push('\n');
                    paragraph_dirty = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(FilesError::with_detail(
                    ErrorKind::Unsupported,
                    error.to_string(),
                ));
            }
        }
    }
    // 收尾：去掉每行尾随的制表符 / 空格，合并连续空行。
    let mut cleaned = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
            cleaned.push('\n');
        } else {
            blank = 0;
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    Ok(cleaned.trim().to_string())
}

// ── 结构化数据（复刻 data.rs）───────────────────────────────────────────

/// JSON → Markdown：数组按对象转表、对象转两列表，标量包围栏。
fn json_to_markdown(json_str: &str) -> FilesResult<String> {
    let value: serde_json::Value = serde_json::from_str(json_str).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("json: {error}"))
    })?;
    let mut markdown = String::new();
    match value {
        serde_json::Value::Array(array) => {
            if array.is_empty() {
                return Ok("_空数组_\n".to_string());
            }
            if let Some(serde_json::Value::Object(first)) = array.first() {
                let fields: Vec<String> = first.keys().cloned().collect();
                markdown.push_str(&format!("| {} |\n", fields.join(" | ")));
                markdown.push_str(&format!("|{}|\n", " --- |".repeat(fields.len())));
                for item in &array {
                    if let serde_json::Value::Object(object) = item {
                        let row: Vec<String> = fields
                            .iter()
                            .map(|field| {
                                object
                                    .get(field)
                                    .map(format_value)
                                    .unwrap_or_default()
                                    .replace('|', "\\|")
                            })
                            .collect();
                        markdown.push_str(&format!("| {} |\n", row.join(" | ")));
                    }
                }
            } else {
                for item in &array {
                    markdown.push_str(&format!("- {}\n", format_value(item)));
                }
            }
        }
        serde_json::Value::Object(object) => {
            markdown.push_str("| 键 | 值 |\n");
            markdown.push_str("| --- | --- |\n");
            for (key, value) in object {
                markdown.push_str(&format!(
                    "| {} | {} |\n",
                    key,
                    format_value(&value).replace('|', "\\|")
                ));
            }
        }
        _ => markdown = format!("```json\n{}\n```", json_str.trim()),
    }
    Ok(markdown)
}

/// 值的单行表示（复刻 `format_val`）。
fn format_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Array(array) => {
            let items: Vec<String> = array.iter().map(format_value).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(object) => {
            let items: Vec<String> = object
                .iter()
                .map(|(key, value)| format!("{key}: {}", format_value(value)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// CSV → Markdown 表。
///
/// 与「CSV 工具」共用同一个解析器（`csv_tools::parse_table`）：同一份 CSV 在两个
/// 工具里得到同一张表，分隔符探测与引号处理不会各说各话。
fn csv_to_markdown(text: &str) -> FilesResult<String> {
    Ok(crate::csv_tools::parse_table(text, None)?.to_markdown())
}

/// CSV → JSON 数组（复刻 `csv_to_json`）。
fn csv_to_json(text: &str) -> FilesResult<String> {
    crate::csv_tools::parse_table(text, None)?.to_json()
}

fn yaml_to_json(text: &str) -> FilesResult<String> {
    let value: serde_json::Value = serde_yaml::from_str(text).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("yaml: {error}"))
    })?;
    serde_json::to_string_pretty(&value)
        .map_err(|error| FilesError::with_detail(ErrorKind::Unsupported, error.to_string()))
}

fn toml_to_json(text: &str) -> FilesResult<String> {
    let value: toml::Value = toml::from_str(text).map_err(|error| {
        FilesError::with_detail(ErrorKind::Unsupported, format!("toml: {error}"))
    })?;
    serde_json::to_string_pretty(&toml_value_to_json(&value))
        .map_err(|error| FilesError::with_detail(ErrorKind::Unsupported, error.to_string()))
}

/// TOML 值 → JSON 值（复刻 `toml_value_to_json`）。
fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => serde_json::Value::String(text.clone()),
        toml::Value::Integer(number) => serde_json::Value::Number((*number).into()),
        toml::Value::Float(number) => serde_json::Number::from_f64(*number)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        toml::Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        toml::Value::Array(array) => {
            serde_json::Value::Array(array.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => {
            let mut map = serde_json::Map::new();
            for (key, value) in table {
                map.insert(key.clone(), toml_value_to_json(value));
            }
            serde_json::Value::Object(map)
        }
    }
}

// ── HTML / Markdown ────────────────────────────────────────────────────

/// Markdown → HTML。
///
/// 参考仓用 comrak 并开了十几个扩展；这里换成 pulldown-cmark（理由见根 `Cargo.toml`），
/// 保留其中最常用的五项：表格、删除线、任务列表、脚注、智能标点。
/// 缺的两项是**上标**与**描述列表**——前者在交付文档里几乎不出现，后者是 Pandoc
/// 方言（comrak 自己也不默认开）。
fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    let parser = Parser::new_ext(markdown, options);
    let mut html_out = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut html_out, parser);
    html_out
}

/// 去掉标签只留文字（复刻 `strip_html_tags`：逐行 trim、去空行、`\n` 连接）。
///
/// 两处比参考仓多做的（都是「导出纯文本」这个用途的基本要求）：
/// 1. 先删掉 `<script>` / `<style>` 整段——参考仓会把 CSS 当正文导出来；
/// 2. 块级标签收尾时补换行——真实网页常常整篇在一行里，不补换行会挤成一坨。
fn strip_html_tags(html: &str) -> String {
    let cleaned = strip_script_style(html);
    let mut result = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in cleaned.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                let name = tag
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                let closing = tag.trim_start().starts_with('/');
                let block = matches!(
                    name.as_str(),
                    "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                );
                if block && (closing || name == "br") {
                    result.push('\n');
                }
            }
            _ if in_tag => tag.push(ch),
            _ => result.push(ch),
        }
    }
    result
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 删掉 `<script>` / `<style>` 整段（连内容一起）。
fn strip_script_style(html: &str) -> String {
    let mut text = html.to_string();
    for tag in ["script", "style"] {
        loop {
            let lower = text.to_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else {
                break;
            };
            let Some(close) = lower[start..].find(&format!("</{tag}>")) else {
                break;
            };
            let end = start + close + tag.len() + 3;
            text.replace_range(start..end, "");
        }
    }
    text
}

/// Markdown → 纯文本（复刻 `strip_md_to_text`：去 `#` / `*` / `-` / `>` / `|`，跳代码围栏）。
fn strip_markdown(markdown: &str) -> String {
    let mut result = String::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            result.push('\n');
            continue;
        }
        if trimmed.starts_with("```") {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches(['#', '*', '-', '>', ' ', '|'])
            .trim_end_matches(['*', ' ', '|'])
            // 参考仓只 trim 两侧的 `*`，`**粗**` 会剩半截 `**粗`；这里把成对的强调标记
            // 整个删掉（纯文本导出里保留半截标记比没有标记更糟）。
            .replace("**", "")
            .replace("__", "");
        result.push_str(&cleaned);
        result.push('\n');
    }
    result.trim().to_string()
}

/// HTML → Markdown。
///
/// 参考仓走 `kreuzberg::extraction::html::convert_html_to_markdown`，那个 crate 用不了
/// （MSRV 1.91），所以这里按**最常见的六类块级元素**自己转：标题、段落、列表、
/// 链接、强调、代码。不是通用 HTML 解析器——转不出来的标签一律当文本剥掉，
/// 与参考仓「尽力而为」的定位一致。
fn html_to_markdown(html: &str) -> String {
    // script / style 的内容不是正文，整段删掉（否则会混进一大坨 CSS）。
    let text = strip_script_style(html);

    let entity = |input: String| {
        input
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    };

    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(lt) = rest.find('<') {
        // 标签前的文字原样留下。
        out.push_str(&entity(rest[..lt].to_string()));
        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag_text = rest[lt + 1..lt + gt].trim().to_string();
        let lower = tag_text.to_lowercase();
        let name: String = lower
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        let closing = lower.starts_with('/');

        match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if closing {
                    out.push_str("\n\n");
                } else {
                    out.push_str("\n\n");
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
            }
            "p" | "div" | "section" | "article" | "tr" => {
                if closing || lower.ends_with('/') {
                    out.push_str("\n\n");
                }
            }
            "br" => out.push('\n'),
            "li" => {
                if closing {
                    out.push('\n');
                } else {
                    out.push_str("\n- ");
                }
            }
            "strong" | "b" => out.push_str("**"),
            "em" | "i" => out.push('*'),
            "code" => out.push('`'),
            "td" | "th" if closing => out.push_str(" | "),
            _ => {}
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(&entity(rest.to_string()));

    let cleaned = entity(out)
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    // 三段以上空行压成两段（HTML 里换行几乎都是排版噪声）。
    let mut squeezed = String::with_capacity(cleaned.len());
    let mut blanks = 0;
    for line in cleaned.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        squeezed.push_str(line);
        squeezed.push('\n');
    }
    squeezed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{temp_dir, write, write_bytes, write_zip};

    #[test]
    fn markdown_to_html_then_back_to_text() {
        let html = convert_text(
            "# 标题\n\n- 一条\n- 两条\n",
            &Format::Markdown,
            OutFormat::Html,
        )
        .expect("html");
        assert!(html.contains("<h1>"), "{html}");
        assert!(html.contains("<li>一条</li>"), "{html}");

        let text = convert_text(
            "# 标题\n\n正文 **粗**\n",
            &Format::Markdown,
            OutFormat::Text,
        )
        .expect("text");
        // `**` 整对删掉（参考仓会留下半截 `**粗`）。
        assert_eq!(text, "标题\n\n正文 粗");
    }

    #[test]
    fn json_array_of_objects_becomes_a_table() {
        let json = r#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#;
        let markdown = convert_text(json, &Format::Json, OutFormat::Markdown).expect("md");
        assert!(markdown.starts_with("| a | b |\n"), "{markdown}");
        assert!(markdown.contains("| 1 | x |"), "{markdown}");

        let objects = r#"{"k":"v","n":3}"#;
        let markdown = convert_text(objects, &Format::Json, OutFormat::Markdown).expect("md");
        assert!(markdown.contains("| 键 | 值 |"), "{markdown}");
        assert!(markdown.contains("| k | v |"), "{markdown}");
    }

    #[test]
    fn yaml_and_toml_convert_to_json() {
        let json =
            convert_text("name: demo\nport: 8080\n", &Format::Yaml, OutFormat::Json).expect("yaml");
        assert!(json.contains("\"name\": \"demo\""), "{json}");

        let json = convert_text(
            "name = \"demo\"\nport = 8080\n",
            &Format::Toml,
            OutFormat::Json,
        )
        .expect("toml");
        assert!(json.contains("\"port\": 8080"), "{json}");
    }

    #[test]
    fn csv_to_markdown_and_json() {
        let csv = "a,b\n1,2\n3,4\n";
        let markdown = convert_text(csv, &Format::Csv, OutFormat::Markdown).expect("md");
        assert!(markdown.starts_with("| a | b |\n"), "{markdown}");
        assert!(markdown.contains("| 3 | 4 |"), "{markdown}");

        let json = convert_text(csv, &Format::Csv, OutFormat::Json).expect("json");
        assert!(json.contains("\"a\": \"1\""), "{json}");
    }

    #[test]
    fn ini_and_xml_are_fenced_not_parsed() {
        let markdown = convert_text("[a]\nk=1\n", &Format::Ini, OutFormat::Markdown).expect("ini");
        assert_eq!(markdown, "```ini\n[a]\nk=1\n```");

        let markdown = convert_text("<a/>", &Format::Xml, OutFormat::Markdown).expect("xml");
        assert_eq!(markdown, "```xml\n<a/>\n```");
    }

    #[test]
    fn html_to_text_drops_tags_and_script_blocks() {
        let html = "<html><head><style>body{color:red}</style></head><body><h1>标题</h1><p>一段 <b>正文</b></p></body></html>";
        let text = convert_text(html, &Format::Html, OutFormat::Text).expect("text");
        assert_eq!(text, "标题\n一段 正文");
        assert!(!text.contains("color"), "{text}");
    }

    #[test]
    fn html_to_markdown_maps_headings_lists_and_emphasis() {
        let html = "<h2>二级</h2><p>前 <strong>粗</strong> 后</p><ul><li>一</li><li>二</li></ul>";
        let markdown = convert_text(html, &Format::Html, OutFormat::Markdown).expect("md");
        assert!(markdown.contains("## 二级"), "{markdown}");
        assert!(markdown.contains("**粗**"), "{markdown}");
        assert!(markdown.contains("- 一"), "{markdown}");
    }

    #[test]
    fn unsupported_combinations_are_reported_not_silently_empty() {
        let error = convert_text("x", &Format::Json, OutFormat::Csv).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        let error =
            convert_text("x", &Format::Unknown("doc".to_string()), OutFormat::Text).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn extension_detection_covers_the_matrix() {
        assert_eq!(detect_format(Path::new("a.MD")), Format::Markdown);
        assert_eq!(detect_format(Path::new("a.yml")), Format::Yaml);
        assert_eq!(detect_format(Path::new("a.properties")), Format::Ini);
        assert_eq!(
            detect_format(Path::new("a.doc")),
            Format::Unknown("doc".to_string())
        );
    }

    #[test]
    fn extracts_runs_from_a_minimal_docx() {
        let root = temp_dir("convert-docx");
        let path = root.join("a.docx");
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>第一段</w:t></w:r></w:p><w:p><w:r><w:t>第二</w:t></w:r><w:r><w:t>段</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
        write_zip(&path, &[("word/document.xml", xml.as_bytes())]);

        let text = convert_file(&path, OutFormat::Markdown).expect("docx");
        assert!(text.contains("第一段"), "{text}");
        assert!(
            text.contains("第二段"),
            "同一段里的多个 run 要拼起来: {text}"
        );
        assert!(text.contains("A\t"), "表格单元格用制表符分隔: {text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pptx_slides_are_ordered_numerically_and_split() {
        let root = temp_dir("convert-pptx");
        let path = root.join("a.pptx");
        // 造 10 页，专门验证 slide2 排在 slide10 之前。
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for index in 1..=10 {
            let xml = format!(
                r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>第{index}页</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            );
            entries.push((format!("ppt/slides/slide{index}.xml"), xml.into_bytes()));
        }
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect();
        write_zip(&path, &refs);

        let text = convert_file(&path, OutFormat::Markdown).expect("pptx");
        let second = text.find("第2页").expect("第 2 页");
        let tenth = text.find("第10页").expect("第 10 页");
        assert!(second < tenth, "字典序会把 10 排到 2 前面: {text}");
        assert!(text.contains("---"), "页间要有分隔: {text}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_file_and_non_office_zip_are_typed_errors() {
        let root = temp_dir("convert-errors");
        let missing = root.join("nope.docx");
        assert_eq!(
            convert_file(&missing, OutFormat::Text).unwrap_err().kind(),
            ErrorKind::PathMissing
        );

        let broken = root.join("broken.docx");
        write_bytes(&root, "broken.docx", b"not a zip at all");
        assert_eq!(
            convert_file(&broken, OutFormat::Text).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn json_markdown_table_escapes_pipes() {
        let json = r#"[{"a":"x|y"}]"#;
        let markdown = convert_text(json, &Format::Json, OutFormat::Markdown).expect("md");
        assert!(markdown.contains("x\\|y"), "{markdown}");
    }

    #[test]
    fn ini_file_round_trip_via_path() {
        let root = temp_dir("convert-ini");
        write(&root, "app.ini", "[db]\nhost=127.0.0.1\n");
        let text = convert_file(&root.join("app.ini"), OutFormat::Markdown).expect("ini");
        assert!(text.starts_with("```ini"), "{text}");
        assert!(text.contains("host=127.0.0.1"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }
}
