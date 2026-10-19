//! 数据格式转换：**CSV / 定长文件 / 固定分隔符**三段解析 + 六种导出。
//!
//! 逐条移植参考仓 `src/tools/DataFormat.vue`（1,449 行，纯前端，无 `invoke`）。
//! 与原页的对应关系：
//!
//! | 参考页 | 这里 |
//! |---|---|
//! | `detectDelimiter()` | [`detect_delimiter`] |
//! | `parseDelimited()` | [`parse_delimited`] |
//! | `parseFixed()`（按 `widths` 切片） | [`parse_fixed`] |
//! | `parseDelim()`（按配置分隔符切） | [`parse_with_delimiter`] |
//! | `formatOutput()` 的五个分支 | [`Output::render`] |
//! | `convertCsv()` 的 `csv` 分支 | [`to_csv`] |
//!
//! ## 三处必须知道的「照抄」
//!
//! 1. **`parseDelimited` 没有引号状态机**。参考页是裸 `split(delimiter)`，字段里
//!    带引号或换行就会散架。这里不动它——CSV 的正规解析在 `csv_tools.rs`（`csv`
//!    crate），那是同一个参考仓**另一个页面**的口径；本页要的是原样的「快速分列」。
//! 2. **`json-array` 输出的是二维数组**，不是对象数组。参考页 `JSON.stringify(rows)`，
//!    对象形态是另一个选项 `json-objects`。不少人第一次用会以为「JSON 数组」= 对象数组。
//! 3. **`text-table` 按 UTF-16 码元数补齐**（参考页 `s.length`）。所以中文表格不会真
//!    对齐（等宽字体下一个汉字占两列）。改成 `chars().count()` 只会更不对齐，照抄即可。
//!
//! ## 一处主动差异
//!
//! 参考页有「配置管理」弹窗 + `getSetting` 持久化（`fixed_width_configs` /
//! `fixed_delimiter_configs`）。`process` 轨插件拿不到宿主的 kv，所以配置改成**一行
//! 文本参数**（[`parse_columns`] 的 `10:姓名,8:城市,4:年龄` 语法）——内容等价，
//! 用户可复制存档、可贴到团队文档里，比「存在本机 localStorage」更适合交接。
//!
//! 内核**不产出**整句文案：错误只有稳定码 + 细节，默认列名前缀由调用方传入。

use crate::error::{ErrorKind, FilesError, FilesResult};

/// 列定义：宽度（字符数）+ 列名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub width: usize,
    pub name: String,
}

/// 解析出来的表。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grid {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// 实际使用的分隔符（定长段为空）。
    pub delimiter: Option<char>,
}

impl Grid {
    /// 列数。
    pub fn column_count(&self) -> usize {
        self.headers.len()
    }

    /// 数据行数（不含表头）。
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 各列中最长的表头/单元格的字符宽度（`text-table` 用）。
    fn widths(&self) -> Vec<usize> {
        self.headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                self.rows
                    .iter()
                    .map(|row| row.get(index).map_or(0, |cell| utf16_len(cell)))
                    .max()
                    .unwrap_or(0)
                    .max(utf16_len(header))
            })
            .collect()
    }
}

/// 输出格式（对应参考页 `outputFormatOptions` + 每段一个「原样」格式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// CSV 段的原样格式：标准 CSV 转义。
    Csv,
    /// 定长段的原样格式：按列宽补齐 + 截断。
    Fixed,
    /// 分隔符段的原样格式：用配置里的分隔符 join。
    Delimited,
    /// 二维数组（**不是**对象数组）。
    JsonArray,
    /// 对象数组。
    JsonObjects,
    /// Markdown 表格。
    Markdown,
    /// 单条多行 `INSERT`。
    SqlInsert,
    /// 等宽文本表。
    TextTable,
}

impl Output {
    /// 从参数的 `value` 解析；认不出返回 `None`（调用方做主备）。
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "csv" => Self::Csv,
            "fixed-format" => Self::Fixed,
            "delim-format" => Self::Delimited,
            "json-array" => Self::JsonArray,
            "json-objects" => Self::JsonObjects,
            "markdown" => Self::Markdown,
            "sql-insert" => Self::SqlInsert,
            "text-table" => Self::TextTable,
            _ => return None,
        })
    }

    /// 是否需要表名（只有 `INSERT` 要）。
    pub fn needs_table_name(self) -> bool {
        matches!(self, Self::SqlInsert)
    }

    /// 渲染。`delimiter` / `columns` 只被 `Delimited` / `Fixed` 用到。
    pub fn render(
        self,
        grid: &Grid,
        table_name: &str,
        delimiter: &str,
        columns: &[Column],
    ) -> String {
        match self {
            Self::Csv => to_csv(grid),
            Self::Fixed => grid.render_fixed(columns),
            Self::Delimited => {
                let mut lines = Vec::with_capacity(grid.rows.len() + 1);
                if !grid.headers.is_empty() {
                    lines.push(grid.headers.join(delimiter));
                }
                lines.extend(grid.rows.iter().map(|row| row.join(delimiter)));
                lines.join("\n")
            }
            Self::JsonArray => {
                let rows: Vec<serde_json::Value> = grid
                    .rows
                    .iter()
                    .map(|row| {
                        serde_json::Value::Array(
                            row.iter()
                                .map(|cell| serde_json::Value::String(cell.clone()))
                                .collect(),
                        )
                    })
                    .collect();
                pretty(&serde_json::Value::Array(rows))
            }
            Self::JsonObjects => {
                let rows: Vec<serde_json::Value> = grid
                    .rows
                    .iter()
                    .map(|row| {
                        let mut object = serde_json::Map::new();
                        for (index, header) in grid.headers.iter().enumerate() {
                            object.insert(
                                header.clone(),
                                serde_json::Value::String(
                                    row.get(index).cloned().unwrap_or_default(),
                                ),
                            );
                        }
                        serde_json::Value::Object(object)
                    })
                    .collect();
                pretty(&serde_json::Value::Array(rows))
            }
            Self::Markdown => {
                let mut out = String::new();
                out.push_str("| ");
                out.push_str(&grid.headers.join(" | "));
                out.push_str(" |\n|");
                out.push_str(
                    &grid
                        .headers
                        .iter()
                        .map(|_| " --- ")
                        .collect::<Vec<_>>()
                        .join("|"),
                );
                out.push_str("|\n");
                for row in &grid.rows {
                    out.push_str("| ");
                    out.push_str(
                        &row.iter()
                            .map(|cell| cell.replace('|', "\\|"))
                            .collect::<Vec<_>>()
                            .join(" | "),
                    );
                    out.push_str(" |\n");
                }
                out
            }
            Self::SqlInsert => {
                let table = if table_name.trim().is_empty() {
                    "data"
                } else {
                    table_name.trim()
                };
                let columns = grid
                    .headers
                    .iter()
                    .map(|header| format!("`{header}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                grid.rows
                    .iter()
                    .map(|row| {
                        let values = row
                            .iter()
                            .map(|cell| format!("'{}'", cell.replace('\'', "''")))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("INSERT INTO `{table}` ({columns}) VALUES ({values});")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::TextTable => {
                let widths = grid.widths();
                let mut out = String::new();
                out.push_str("| ");
                out.push_str(
                    &grid
                        .headers
                        .iter()
                        .enumerate()
                        .map(|(index, header)| pad_to(header, widths[index]))
                        .collect::<Vec<_>>()
                        .join(" | "),
                );
                out.push_str(" |\n|-");
                out.push_str(
                    &widths
                        .iter()
                        .map(|width| "-".repeat(*width))
                        .collect::<Vec<_>>()
                        .join("-|-"),
                );
                out.push_str("-|\n");
                for row in &grid.rows {
                    out.push_str("| ");
                    out.push_str(
                        &(0..grid.headers.len())
                            .map(|index| {
                                pad_to(row.get(index).map_or("", String::as_str), widths[index])
                            })
                            .collect::<Vec<_>>()
                            .join(" | "),
                    );
                    out.push_str(" |\n");
                }
                out
            }
        }
    }
}

/// 定长段「原样」输出需要列宽，直接走 [`Grid::render_fixed`]。
impl Grid {
    /// 定长输出（`Output::Fixed` 的真身）：按 `columns` 的宽度补齐并截断。
    ///
    /// 参考页 `cell.padEnd(w, ' ').substring(0, w)`：**先补齐再截断**，所以超宽单元格
    /// 会被切掉，而不是撑开整列。
    pub fn render_fixed(&self, columns: &[Column]) -> String {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(index, cell)| {
                        pad_to(cell, columns.get(index).map_or(10, |column| column.width))
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// JSON 美化（2 空格缩进，与 `JSON.stringify(x, null, 2)` 对齐）。
fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// 补空格到 `width` 个 UTF-16 码元，超出则截断（`padEnd().substring()` 的等价物）。
///
/// 按**字符**前进、按 UTF-16 码元计宽：`substring` 会把代理对切成半个字符，
/// 这里不做那种破坏——代价是 emoji 只占一位时不补空格。
fn pad_to(text: &str, width: usize) -> String {
    let mut current = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cost = ch.len_utf16();
        if used + cost > width {
            break;
        }
        used += cost;
        current.push(ch);
    }
    if used < width {
        current.push_str(&" ".repeat(width - used));
    }
    current
}

/// 参考页 `s.length`：UTF-16 码元数。
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 标准 CSV 转义（参考页 `convertCsv` 的 `esc`）：含 `"` `,` 换行时加引号、`"` 翻倍。
pub fn to_csv(grid: &Grid) -> String {
    let escape = |cell: &str| {
        if cell.contains(['"', ',', '\n', '\r']) {
            format!("\"{}\"", cell.replace('"', "\"\""))
        } else {
            cell.to_owned()
        }
    };
    let mut lines = Vec::with_capacity(grid.rows.len() + 1);
    if !grid.headers.is_empty() {
        lines.push(
            grid.headers
                .iter()
                .map(|header| escape(header))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    lines.extend(grid.rows.iter().map(|row| {
        row.iter()
            .map(|cell| escape(cell))
            .collect::<Vec<_>>()
            .join(",")
    }));
    lines.join("\n")
}

/// 分隔符探测（参考页 `detectDelimiter`）：候选 `,` `\t` `;` `|`，
/// 按「非零行出现次数的均值 − 方差 × 0.5」打分取最高；全无命中回落到 `,`。
pub fn detect_delimiter(text: &str) -> char {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return ',';
    }
    let mut best: Option<(char, f64)> = None;
    for candidate in [',', '\t', ';', '|'] {
        let counts: Vec<f64> = lines
            .iter()
            .map(|line| line.matches(candidate).count() as f64)
            .filter(|count| *count > 0.0)
            .collect();
        if counts.is_empty() {
            continue;
        }
        let average = counts.iter().sum::<f64>() / counts.len() as f64;
        let variance = counts
            .iter()
            .map(|count| (count - average).powi(2))
            .sum::<f64>()
            / counts.len() as f64;
        let score = average - variance * 0.5;
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((candidate, score));
        }
    }
    best.map_or(',', |(candidate, _)| candidate)
}

/// 分隔符选项 `value` → 字符。`auto` 返回 `None`（交给 [`detect_delimiter`]）。
pub fn delimiter_from_value(value: &str) -> Option<char> {
    match value {
        "comma" => Some(','),
        "tab" => Some('\t'),
        "semicolon" => Some(';'),
        "pipe" => Some('|'),
        "hash" => Some('#'),
        "dollar" => Some('$'),
        "tilde" => Some('~'),
        _ => None,
    }
}

/// CSV / TSV 段解析：裸 `split` + 逐格 `trim`（**不处理引号**，见模块头第 1 条）。
pub fn parse_delimited(
    text: &str,
    delimiter: Option<char>,
    has_header: bool,
    default_prefix: &str,
) -> Grid {
    let data_lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if data_lines.is_empty() {
        return Grid::default();
    }
    let actual = delimiter.unwrap_or_else(|| detect_delimiter(text));
    let split = |line: &str| -> Vec<String> {
        line.split(actual)
            .map(|cell| cell.trim().to_owned())
            .collect()
    };
    let mut headers: Vec<String> = Vec::new();
    if has_header {
        headers = split(data_lines[0]);
    }
    let rows: Vec<Vec<String>> = data_lines
        .iter()
        .skip(usize::from(has_header))
        .map(|line| split(line))
        .collect();
    if !has_header {
        headers = default_headers(&rows, default_prefix);
    }
    Grid {
        headers,
        rows,
        delimiter: Some(actual),
    }
}

/// 定长文件段解析：按 `columns` 的宽度逐段切片。
pub fn parse_fixed(text: &str, columns: &[Column], trim: bool) -> Grid {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let rows: Vec<Vec<String>> = lines
        .iter()
        .map(|line| {
            let mut cursor = 0usize;
            columns
                .iter()
                .map(|column| {
                    let cell = slice_chars(line, cursor, column.width);
                    cursor += column.width;
                    if trim { cell.trim().to_owned() } else { cell }
                })
                .collect()
        })
        .collect();
    Grid {
        headers: columns.iter().map(|column| column.name.clone()).collect(),
        rows,
        delimiter: None,
    }
}

/// 固定分隔符段解析：用配置里的**字符串**分隔符切（可以是多字符）。
pub fn parse_with_delimiter(
    text: &str,
    delimiter: &str,
    has_header: bool,
    names: &[String],
    trim: bool,
    default_prefix: &str,
) -> Grid {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Grid::default();
    }
    let separator = if delimiter.is_empty() { "|" } else { delimiter };
    let clean = |cell: &str| -> String {
        if trim {
            cell.trim().to_owned()
        } else {
            cell.to_owned()
        }
    };
    let (mut headers, data_lines): (Vec<String>, &[&str]) = if has_header {
        (lines[0].split(separator).map(clean).collect(), &lines[1..])
    } else {
        (names.to_vec(), &lines[..])
    };
    let rows: Vec<Vec<String>> = data_lines
        .iter()
        .map(|line| line.split(separator).map(clean).collect())
        .collect();
    if headers.is_empty() {
        headers = default_headers(&rows, default_prefix);
    }
    Grid {
        headers,
        rows,
        delimiter: separator.chars().next(),
    }
}

/// 无表头时的默认列名：`{prefix}1` `{prefix}2` …
fn default_headers(rows: &[Vec<String>], prefix: &str) -> Vec<String> {
    let count = rows.iter().map(Vec::len).max().unwrap_or(0);
    (1..=count)
        .map(|index| format!("{prefix}{index}"))
        .collect()
}

/// 按**字符**切片（不是字节，也不是 UTF-16 码元）。
///
/// 定长文件的列宽在业务上按「字」算（一个汉字一格），所以这里用 `chars()`。
/// JavaScript 的 `substring` 是 UTF-16 码元；对 BMP 内的中英混排两者等价，
/// 只有 emoji（代理对）会差一位——定长文件里不会有 emoji，取更符合直觉的那个。
fn slice_chars(line: &str, start: usize, width: usize) -> String {
    line.chars().skip(start).take(width).collect()
}

/// 「配置管理」弹窗里那张表的一行文本等价物：`10:姓名,8:城市,4:年龄`。
///
/// 也接受只有宽度的写法（`10,8,4`），列名按 `{prefix}1..n` 生成。
/// 参考页把配置存进 `localStorage`，这里改成一行文本：可复制、可进版本库、
/// 可贴到交接文档，比「只存在于本机设置」更耐用。
pub fn parse_columns(spec: &str, default_prefix: &str) -> FilesResult<Vec<Column>> {
    let mut columns = Vec::new();
    for (index, part) in spec.split([',', ';']).enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (width_text, name) = match part.split_once(':') {
            Some((width, name)) => (width.trim(), name.trim()),
            None => match part.split_once('=') {
                Some((width, name)) => (width.trim(), name.trim()),
                None => (part, ""),
            },
        };
        let width: usize = width_text.parse().map_err(|_| {
            FilesError::with_detail(
                ErrorKind::BadConfig,
                format!("column #{} width `{width_text}`", index + 1),
            )
        })?;
        if width == 0 {
            return Err(FilesError::with_detail(
                ErrorKind::BadConfig,
                format!("column #{} width is 0", index + 1),
            ));
        }
        let name = if name.is_empty() {
            format!("{default_prefix}{}", index + 1)
        } else {
            name.to_owned()
        };
        columns.push(Column { width, name });
    }
    if columns.is_empty() {
        return Err(FilesError::new(ErrorKind::BadConfig));
    }
    Ok(columns)
}

/// 解析配置里的列名（固定分隔符段的「列名（可选）」）：`姓名,城市,年龄`。
pub fn parse_names(spec: &str) -> Vec<String> {
    spec.split([',', ';'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(headers: &[&str], rows: &[&[&str]]) -> Grid {
        Grid {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|c| (*c).to_owned()).collect())
                .collect(),
            delimiter: Some(','),
        }
    }

    #[test]
    fn detects_comma_against_noise() {
        // 三行都是 2 个逗号、0 个制表符 → 逗号胜出。
        assert_eq!(detect_delimiter("a,b\nc,d\ne,f\n"), ',');
    }

    #[test]
    fn detects_tab_for_excel_paste() {
        assert_eq!(detect_delimiter("a\tb\tc\n1\t2\t3\n"), '\t');
    }

    #[test]
    fn variance_penalty_prefers_consistent_delimiter() {
        // `|` 每行都是 2 个（方差 0，得分 2）；`,` 一行 1 个、一行 8 个
        // （均值 4.5、方差 12.25，扣掉一半方差后是负数）。参考页就是这么打分的。
        let text = "a|b|c,x\n1|2|3,2,3,4,5,6,7,8,9\n";
        assert_eq!(detect_delimiter(text), '|');
    }

    #[test]
    fn no_delimiter_falls_back_to_comma() {
        assert_eq!(detect_delimiter("hello\nworld\n"), ',');
        assert_eq!(detect_delimiter(""), ',');
    }

    #[test]
    fn parses_delimited_with_header() {
        let parsed = parse_delimited("name,age\n张三,28\n李四,35\n", None, true, "col");
        assert_eq!(parsed.headers, vec!["name", "age"]);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0], vec!["张三", "28"]);
    }

    #[test]
    fn parses_delimited_without_header_uses_prefix() {
        let parsed = parse_delimited("a,1\nb,2\n", Some(','), false, "列");
        assert_eq!(parsed.headers, vec!["列1", "列2"]);
        assert_eq!(parsed.rows.len(), 2);
    }

    #[test]
    fn blank_lines_are_skipped() {
        let parsed = parse_delimited("a,b\n\n1,2\n\n3,4\n", Some(','), true, "c");
        assert_eq!(parsed.rows.len(), 2);
    }

    #[test]
    fn fixed_width_slices_by_character() {
        let columns = parse_columns("4:姓名,4:城市,2:年龄", "列").expect("配置");
        let parsed = parse_fixed("张三  北京  28\n李四  上海  35", &columns, true);
        assert_eq!(parsed.headers, vec!["姓名", "城市", "年龄"]);
        assert_eq!(parsed.rows[0], vec!["张三", "北京", "28"]);
        assert_eq!(parsed.rows[1], vec!["李四", "上海", "35"]);
    }

    #[test]
    fn fixed_width_keeps_padding_when_trim_off() {
        let columns = parse_columns("4,4", "列").expect("配置");
        let parsed = parse_fixed("张三  北京", &columns, false);
        assert_eq!(parsed.rows[0][0], "张三  ");
    }

    #[test]
    fn column_spec_rejects_zero_and_garbage() {
        assert!(parse_columns("0:坏", "列").is_err());
        assert!(parse_columns("abc:坏", "列").is_err());
        assert!(parse_columns("   ", "列").is_err());
    }

    #[test]
    fn column_spec_accepts_width_only() {
        let columns = parse_columns("10,8", "列").expect("配置");
        assert_eq!(columns[0].name, "列1");
        assert_eq!(columns[1].width, 8);
    }

    #[test]
    fn multi_char_delimiter_works() {
        let parsed = parse_with_delimiter("a||b\n1||2", "||", true, &[], true, "列");
        assert_eq!(parsed.headers, vec!["a", "b"]);
        assert_eq!(parsed.rows[0], vec!["1", "2"]);
    }

    #[test]
    fn delimiter_names_are_used_when_no_header() {
        let names = parse_names("姓名,城市");
        let parsed = parse_with_delimiter("张三|北京", "|", false, &names, true, "列");
        assert_eq!(parsed.headers, vec!["姓名", "城市"]);
    }

    #[test]
    fn json_array_is_two_dimensional() {
        let table = grid(&["a", "b"], &[&["1", "2"]]);
        let text = Output::JsonArray.render(&table, "", ",", &[]);
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(value, serde_json::json!([["1", "2"]]));
    }

    #[test]
    fn json_objects_use_headers_as_keys() {
        let table = grid(&["a", "b"], &[&["1", "2"]]);
        let text = Output::JsonObjects.render(&table, "", ",", &[]);
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(value, serde_json::json!([{"a": "1", "b": "2"}]));
    }

    #[test]
    fn json_objects_fill_missing_cells() {
        let table = Grid {
            headers: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into()]],
            delimiter: Some(','),
        };
        let text = Output::JsonObjects.render(&table, "", ",", &[]);
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(value, serde_json::json!([{"a": "1", "b": ""}]));
    }

    #[test]
    fn markdown_escapes_pipes() {
        let table = grid(&["a"], &[&["x|y"]]);
        let text = Output::Markdown.render(&table, "", ",", &[]);
        assert!(text.contains("x\\|y"), "{text}");
    }

    #[test]
    fn insert_escapes_quotes_and_backticks_table() {
        let table = grid(&["name"], &[&["O'Brien"]]);
        let text = Output::SqlInsert.render(&table, "t_user", ",", &[]);
        assert_eq!(text, "INSERT INTO `t_user` (`name`) VALUES ('O''Brien');");
    }

    #[test]
    fn insert_defaults_table_name_to_data() {
        let table = grid(&["a"], &[&["1"]]);
        assert!(
            Output::SqlInsert
                .render(&table, "  ", ",", &[])
                .contains("`data`")
        );
    }

    #[test]
    fn text_table_pads_columns() {
        let table = grid(&["name", "n"], &[&["ab", "1234"]]);
        let text = Output::TextTable.render(&table, "", ",", &[]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "| name | n    |");
        assert_eq!(lines[1], "|------|------|");
        assert_eq!(lines[2], "| ab   | 1234 |");
    }

    #[test]
    fn csv_escapes_quotes_commas_and_newlines() {
        let table = grid(&["a", "b"], &[&["x,y", "say \"hi\""]]);
        let text = to_csv(&table);
        assert_eq!(text, "a,b\n\"x,y\",\"say \"\"hi\"\"\"");
    }

    #[test]
    fn csv_keeps_plain_cells_unquoted() {
        let table = grid(&["a"], &[&["plain"]]);
        assert_eq!(to_csv(&table), "a\nplain");
    }

    #[test]
    fn fixed_render_pads_and_truncates() {
        let table = grid(&["姓名", "城市"], &[&["张三丰", "北京市朝阳区"]]);
        let columns = parse_columns("4:姓名,4:城市", "列").expect("配置");
        // 参考页是 `padEnd(w).substring(0, w)`：先补齐再截断，所以第一列留着补的那个空格。
        let text = table.render_fixed(&columns);
        assert_eq!(text, "张三丰 北京市朝");
    }

    #[test]
    fn delimited_render_joins_with_given_separator() {
        let table = grid(&["a", "b"], &[&["1", "2"]]);
        assert_eq!(
            Output::Delimited.render(&table, "", "||", &[]),
            "a||b\n1||2"
        );
    }

    #[test]
    fn output_names_match_reference_page() {
        assert_eq!(Output::parse("json-array"), Some(Output::JsonArray));
        assert_eq!(Output::parse("json-objects"), Some(Output::JsonObjects));
        assert_eq!(Output::parse("fixed-format"), Some(Output::Fixed));
        assert_eq!(Output::parse("delim-format"), Some(Output::Delimited));
        assert_eq!(Output::parse("text-table"), Some(Output::TextTable));
        assert_eq!(Output::parse("nope"), None);
    }
}
