//! 统一结果模型：所有引擎的值都拓宽进 [`Value`]，跨界即终态。
//!
//! 纪律（对齐 dbui / dbx 的经验）：数值**绝不经 `f64` 有损中转**——NUMERIC / DECIMAL
//! 进 [`Value::Decimal`]（字符串承载，保精度）；时间类型渲染成字符串；
//! 二进制保留字节，由界面决定展示截断。

use std::fmt;

/// 单元格值。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL。
    Null,
    /// 布尔。
    Bool(bool),
    /// 有符号整数。
    Int(i64),
    /// 无符号整数（MySQL 的 BIGINT UNSIGNED）。
    UInt(u64),
    /// 浮点。
    Float(f64),
    /// 精确小数（NUMERIC / DECIMAL），字符串承载。
    Decimal(String),
    /// 文本。
    Text(String),
    /// 二进制。
    Bytes(Vec<u8>),
    /// 日期（`YYYY-MM-DD`）。
    Date(String),
    /// 时间（`HH:MM:SS[.ffffff]`）。
    Time(String),
    /// 时间戳（`YYYY-MM-DD HH:MM:SS[.ffffff]`）。
    DateTime(String),
    /// JSON 文档（渲染为文本）。
    Json(String),
}

impl Value {
    /// 是否为 NULL。
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// 渲染成展示文本（NULL 渲染成 `NULL`）。
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Bool(v) => v.to_string(),
            Self::Int(v) => v.to_string(),
            Self::UInt(v) => v.to_string(),
            Self::Float(v) => {
                // 浮点保留最短可往返表示；NaN / 无穷按字面。
                if v.is_finite() {
                    format!("{v}")
                } else if v.is_nan() {
                    "NaN".to_owned()
                } else if *v > 0. {
                    "Infinity".to_owned()
                } else {
                    "-Infinity".to_owned()
                }
            }
            Self::Decimal(v)
            | Self::Text(v)
            | Self::Date(v)
            | Self::Time(v)
            | Self::DateTime(v)
            | Self::Json(v) => v.clone(),
            Self::Bytes(v) => format!("{} B", v.len()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// 结果集的一列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// 列名。
    pub name: String,
    /// 引擎报的类型名（展示用；拿不到时为空串）。
    pub type_name: String,
}

/// 一个结果集。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    /// 列（顺序即展示顺序）。
    pub columns: Vec<Column>,
    /// 行。
    pub rows: Vec<Vec<Value>>,
    /// 是否因行数上限截断（`max_rows` 探测到还有更多行）。
    pub truncated: bool,
}

impl ResultSet {
    /// 列数。
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// 行数（不含截断掉的部分）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 渲染成 CSV 文本（含表头行）。
    ///
    /// 这是**导出**的中性落点：与 [`crate::csv::parse_csv`] 是读写对偶，任何引擎的结果集
    /// 都走这里——导出格式不属于某一条驱动。
    #[must_use]
    pub fn to_csv(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            csv_field(&column.name, &mut out);
        }
        out.push('\n');
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                csv_field(&cell.render(), &mut out);
            }
            let _ = writeln!(out);
        }
        out
    }

    /// 渲染成 JSON 数组（每个对象一行，键为列名）。
    ///
    /// 类型按 [`Value`] 的真实形状给：数值 / 布尔给 JSON 字面量，`NULL` 给 `null`。
    /// 两处刻意**不**走数值字面量：`Decimal` 是定点数，本仓硬纪律「绝不进 `f64`」，
    /// 所以它以字符串承载（`"123.450"`）；非有限浮点（`NaN` / `Infinity`）在 JSON 里
    /// 没有字面量，同样降成字符串——否则会产出一份**语法非法**的 JSON。
    /// `Json` 格若本身是合法 JSON 就原样嵌入（导出的就是文档本身），否则按字符串给。
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("[\n");
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push_str(",\n");
            }
            out.push_str("  {");
            for (column_index, cell) in row.iter().enumerate() {
                if column_index > 0 {
                    out.push_str(", ");
                }
                let name = self
                    .columns
                    .get(column_index)
                    .map_or("", |column| column.name.as_str());
                out.push_str(&json_string(name));
                out.push_str(": ");
                out.push_str(&json_value(cell));
            }
            out.push('}');
        }
        out.push_str("\n]\n");
        out
    }

    /// 渲染成逐行一条的 `INSERT` 语句（含列清单）。
    ///
    /// `table` 是**调用方拼好的表引用文本**（按引擎的限定名与引号规则），列名与字符串
    /// 值用 `quote` 包裹——中性层不认识引擎，引号由调用方按 [`crate::config::DatabaseKind`]
    /// 给。逐行一条而不是一条多值 `VALUES`：多值写法 Oracle 之类不支持，逐行是最大公约数。
    #[must_use]
    pub fn to_insert_sql(&self, table: &str, quote: char) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| quote_name(&column.name, quote))
            .collect::<Vec<_>>()
            .join(", ");
        let mut out = String::new();
        for row in &self.rows {
            let values = row.iter().map(sql_literal).collect::<Vec<_>>().join(", ");
            out.push_str("INSERT INTO ");
            out.push_str(table);
            out.push_str(" (");
            out.push_str(&columns);
            out.push_str(") VALUES (");
            out.push_str(&values);
            out.push_str(");\n");
        }
        out
    }

    /// 渲染成 TSV 文本（含表头）。
    ///
    /// 用制表符而不是逗号：粘进 Excel / Numbers / 飞书表格会**直接分列**，不用再走一遍
    /// 「分列向导」——所以 TSV 是**复制到剪贴板**那条路的默认档。TSV 没有公认的转义
    /// 标准，这里按 CSV 的引号规则处理含制表符 / 换行 / 引号的单元格（主流表格软件
    /// 导入时认这一套）。
    ///
    /// 行式文本以换行收尾（与 [`Self::to_csv`] 同口径）。
    #[must_use]
    pub fn to_tsv(&self) -> String {
        let mut out = String::new();
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                out.push('\t');
            }
            out.push_str(&tsv_field(&column.name));
        }
        out.push('\n');
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    out.push('\t');
                }
                out.push_str(&tsv_field(&cell.render()));
            }
            out.push('\n');
        }
        out
    }

    /// 渲染成 Markdown 管道表格（GFM）。
    ///
    /// 表头 + 分隔行 + 数据行；单元格里的 `|` 转义成 `\|`、换行折成 `<br>`——这两条是
    /// 让**任意文本**塞进表格而不破坏列结构的唯一稳妥办法（Markdown 没有「转义块」的
    /// 概念，真换行会把一行切成两行）。表名之类的外壳由调用方加，这里只产出表格本体。
    ///
    /// 零列时返回空串：没有列就没有表头，分隔行也失去意义，硬凑出的一截反而**不是**
    /// 合法表格。
    #[must_use]
    pub fn to_markdown(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }

        let mut out = String::new();
        out.push('|');
        for column in &self.columns {
            out.push(' ');
            out.push_str(&markdown_cell(&column.name));
            out.push_str(" |");
        }
        out.push('\n');
        out.push('|');
        for _ in &self.columns {
            out.push_str(" --- |");
        }
        out.push('\n');
        for row in &self.rows {
            out.push('|');
            for cell in row {
                out.push(' ');
                out.push_str(&markdown_cell(&cell.render()));
                out.push_str(" |");
            }
            out.push('\n');
        }
        out
    }
    /// 渲染成**对齐的纯文本表**（列宽按显示宽度补空格）。
    ///
    /// 与 [`Self::to_tsv`] / [`Self::to_csv`] 的分工：那两档是**给程序读的**（分隔符 +
    /// 转义，导入表格软件），这一档是**给人看的**——粘进终端、工单、聊天窗口时列是对齐
    /// 的。因此单元格里的换行与制表符折成字面量 `\n` / `\t`（一个真换行会把后面所有行
    /// 顶偏，对齐也就无从谈起）。
    ///
    /// 宽度按**显示格数**算：CJK / 全角按两格，其余一格——只按 `chars().count()` 数的话
    /// 「表」与「a」同宽，中文列会窄掉一半。
    #[must_use]
    pub fn to_text(&self) -> String {
        let header: Vec<String> = self
            .columns
            .iter()
            .map(|column| flatten_cell(&column.name))
            .collect();
        let body: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| flatten_cell(&cell.render()))
                    .collect()
            })
            .collect();
        // 列宽取表头与所有单元格的最大值；零列时下面直接返回空串。
        let mut widths: Vec<usize> = header.iter().map(|cell| display_width(cell)).collect();
        for row in &body {
            for (index, cell) in row.iter().enumerate() {
                if let Some(width) = widths.get_mut(index) {
                    *width = (*width).max(display_width(cell));
                }
            }
        }
        let mut out = String::new();
        write_text_row(&mut out, &header, &widths);
        for row in &body {
            write_text_row(&mut out, row, &widths);
        }
        out
    }

    /// 渲染成**缩进完整**的 JSON（两空格缩进，一行一个字段）。
    ///
    /// 与 [`Self::to_json`] 的关系：那一档是「一条记录一行」——便于 `grep` / `diff` 与流式
    /// 消费；这一档是给人读与贴进文档的形态。两者都是合法 JSON，值的类型规则完全一致
    /// （共用 [`json_value`]，不会出现「美化后类型变了」这种事）。
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::from("[\n");
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push_str(",\n");
            }
            out.push_str("  {\n");
            for (column_index, cell) in row.iter().enumerate() {
                let name = self
                    .columns
                    .get(column_index)
                    .map_or("", |column| column.name.as_str());
                out.push_str("    ");
                out.push_str(&json_string(name));
                out.push_str(": ");
                out.push_str(&json_value(cell));
                if column_index + 1 < row.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("  }");
        }
        out.push_str("\n]\n");
        out
    }

    /// 渲染成**二进制转储**（每行一条记录，字节列按 [`BinaryEncoding`] 编码）。
    ///
    /// 非字节列照常给它们的展示文本、`NULL` 给 `NULL`：一份转储里常常是「id + 二进制
    /// 载荷」一起看，把非字节列丢掉反而要用户回去对表。
    #[must_use]
    pub fn to_binary_dump(&self, encoding: BinaryEncoding) -> String {
        let mut out = String::new();
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    out.push('\t');
                }
                match cell {
                    Value::Bytes(bytes) => out.push_str(&encoding.encode(bytes)),
                    other => out.push_str(&other.render()),
                }
            }
            out.push('\n');
        }
        out
    }

    /// 单列二进制结果的**原始字节**（落盘成真正的二进制文件）。
    ///
    /// 只有「恰好一列、且每一行都是 `Value::Bytes`」才给字节：这是「导出这张图 /
    /// 这个附件」那种场景唯一说得清的形态。混着文本列时不做「猜哪列是载荷」这种事
    /// ——猜错的代价是给用户一个看起来正常、实际错位的文件。空结果集返回 `None`
    /// （没有行就谈不上载荷）。
    #[must_use]
    pub fn raw_bytes(&self) -> Option<Vec<u8>> {
        if self.columns.len() != 1 || self.rows.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for row in &self.rows {
            match row.first() {
                Some(Value::Bytes(bytes)) => out.extend_from_slice(bytes),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// 二进制转储的编码档（见 [`ResultSet::to_binary_dump`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryEncoding {
    /// 小写十六进制、字节间不加分隔（可直接喂 `xxd -r -p`）。
    Hex,
    /// 标准 Base64（含 `=` 填充，不折行）。
    Base64,
}

impl BinaryEncoding {
    /// 编码一段字节。
    #[must_use]
    pub fn encode(self, bytes: &[u8]) -> String {
        match self {
            Self::Hex => hex_encode(bytes),
            Self::Base64 => base64_encode(bytes),
        }
    }
}

/// 把一格折成**单行**文本（换行 / 制表符变字面量），见 [`ResultSet::to_text`]。
fn flatten_cell(text: &str) -> String {
    text.replace("\r\n", "\\n")
        .replace(['\n', '\r'], "\\n")
        .replace('\t', "\\t")
}

/// 写一行对齐文本；末列不补尾空格（否则每行都会拖着看不见的空白）。
fn write_text_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let last = cells.len().saturating_sub(1);
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index < last {
            let width = widths.get(index).copied().unwrap_or_default();
            for _ in display_width(cell)..width {
                out.push(' ');
            }
        }
    }
    out.push('\n');
}

/// 文本的显示格数（CJK / 全角算两格）——只用于对齐，不追求与各终端逐像素一致。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| if is_wide_char(c) { 2 } else { 1 })
        .sum()
}

/// 全角 / CJK 判定（Unicode 常见宽字符区段）。
fn is_wide_char(c: char) -> bool {
    matches!(
        c as u32,
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
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    )
}

/// 小写十六进制（无分隔、无前缀）。
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// 标准 Base64（RFC 4648，含 `=` 填充）。
///
/// 手写而不是引 `base64`：这就是一张 64 字节的表 + 三次移位，为本仓**用处有限**
/// （二进制导出档 + HTTP 代理的 `Proxy-Authorization: Basic`）拉一个依赖不划算
/// （依赖准入纪律：能不新增就不新增）。
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(TABLE[((triple >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[((triple >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[(triple & 0x3f) as usize])
        } else {
            '='
        });
    }
    out
}

/// 一格的 TSV 文本（见 [`ResultSet::to_tsv`]）。
///
/// 公开是有意的：「复制整行」这类**不经过整个结果集**的路径也要用同一套转义规则，
/// 各写一份必然漂移（而漂移的那一份会把一行有换行的备注错开一整行）。
#[must_use]
pub fn tsv_field(cell: &str) -> String {
    if cell.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_owned()
    }
}

/// 一格的 Markdown 表格文本（见 [`ResultSet::to_markdown`]）。
fn markdown_cell(cell: &str) -> String {
    // 反斜杠必须**先**转义：数据里的 `\|` 若不先变成 `\\|`，后一步会把那个 `|`
    // 原样留下，表格就从这里被切列。
    cell.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\r', "")
        .replace('\n', "<br>")
}

/// 把值渲染成 CSV 字段（写侧需要引号转义时才加引号）。
fn csv_field(text: &str, out: &mut String) {
    if text.contains(',') || text.contains('"') || text.contains('\n') {
        out.push('"');
        for c in text.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(text);
    }
}

/// 一个字符串的 JSON 字面量（转义交给 `serde_json`，不手搓）。
fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// 一格值的 JSON 表示（见 [`ResultSet::to_json`] 的类型口径）。
fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Int(number) => number.to_string(),
        Value::UInt(number) => number.to_string(),
        Value::Float(number) if number.is_finite() => format!("{number}"),
        // JSON 没有 NaN / Infinity 字面量：降成字符串，别产出非法 JSON。
        Value::Float(number) => json_string(&number.to_string()),
        // 定点数绝不进 `f64`：字符串承载，保精度。
        Value::Decimal(text) => json_string(text),
        Value::Json(text) => {
            if serde_json::from_str::<serde_json::Value>(text).is_ok() {
                text.clone()
            } else {
                json_string(text)
            }
        }
        Value::Text(_) | Value::Date(_) | Value::Time(_) | Value::DateTime(_) | Value::Bytes(_) => {
            json_string(&value.render())
        }
    }
}

/// 一个标识符的引号包裹（引号字符自身双写转义）。
fn quote_name(name: &str, quote: char) -> String {
    let escaped = name.replace(quote, &format!("{quote}{quote}"));
    format!("{quote}{escaped}{quote}")
}

/// 一格值的 SQL 字面量（见 [`ResultSet::to_insert_sql`]）。
fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        // SQL 标准的布尔字面量（SQLite 3.23+ / PG / MySQL 8 都认）。
        Value::Bool(flag) => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
        Value::Int(number) => number.to_string(),
        Value::UInt(number) => number.to_string(),
        Value::Float(number) if number.is_finite() => format!("{number}"),
        // 非有限浮点与定点数都按**文本**给（前者的字面量各引擎不一，后者要保精度）。
        Value::Float(number) => sql_text(&number.to_string()),
        Value::Decimal(text) => text.clone(),
        Value::Bytes(_)
        | Value::Text(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::DateTime(_)
        | Value::Json(_) => sql_text(&value.render()),
    }
}

/// 单引号字符串字面量（`'` 双写转义）。
fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// 执行一段 SQL 的产出：一段文本可能含多条语句，逐条执行后合并成它。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecOutcome {
    /// 最后一条**返回行**的语句的结果集；整段都是写语句时为 `None`。
    pub result: Option<ResultSet>,
    /// 全部写语句累计的影响行数。
    pub affected: u64,
    /// 实际执行的语句条数。
    pub statements_run: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 渲染规则：NULL / 浮点 / 二进制。
    #[test]
    fn render_rules() {
        assert_eq!(Value::Null.render(), "NULL");
        assert!(Value::Null.is_null());
        assert_eq!(Value::Decimal("123.450".into()).render(), "123.450");
        assert_eq!(Value::Float(1.5).render(), "1.5");
        assert_eq!(Value::Float(f64::NAN).render(), "NaN");
        assert_eq!(Value::Bytes(vec![0u8; 3]).render(), "3 B");
        assert_eq!(Value::Json(r#"{"a":1}"#.into()).render(), r#"{"a":1}"#);
    }

    /// 纯文本档：列按显示宽度对齐，换行 / 制表符折成字面量。
    #[test]
    fn text_export_aligns_by_display_width() {
        let result = ResultSet {
            columns: vec![
                Column {
                    name: "名称".into(),
                    type_name: "text".into(),
                },
                Column {
                    name: "n".into(),
                    type_name: "integer".into(),
                },
            ],
            rows: vec![
                vec![Value::Text("表".into()), Value::Int(1)],
                vec![Value::Text("a\nb".into()), Value::Int(22)],
            ],
            truncated: false,
        };
        let text = result.to_text();
        // 「名称」两个全角字 = 4 格；第二行 `a\nb` 是 4 个 ASCII 字符 —— 正好对齐，
        // 所以两行的分隔都落在第 5 个空格之后。
        assert_eq!(text, "名称  n\n表    1\na\\nb  22\n");
        // 真换行不许漏进文本档：一个真换行会让后面所有行错位。
        assert!(!text.contains("a\nb"));
    }

    /// 十六进制 / Base64 编码正确，且非字节列照旧给展示文本。
    #[test]
    fn binary_dump_encodes_bytes_only() {
        let result = ResultSet {
            columns: vec![
                Column {
                    name: "id".into(),
                    type_name: "integer".into(),
                },
                Column {
                    name: "blob".into(),
                    type_name: "bytea".into(),
                },
            ],
            rows: vec![vec![Value::Int(7), Value::Bytes(vec![0x00, 0xff, 0x10])]],
            truncated: false,
        };
        assert_eq!(result.to_binary_dump(BinaryEncoding::Hex), "7\t00ff10\n");
        // `00 ff 10` 三字节 → `AP8Q`（无填充）。
        assert_eq!(result.to_binary_dump(BinaryEncoding::Base64), "7\tAP8Q\n");
        // 标准向量：`f`/`fo`/`foo` 的填充形态。
        assert_eq!(BinaryEncoding::Base64.encode(b"f"), "Zg==");
        assert_eq!(BinaryEncoding::Base64.encode(b"fo"), "Zm8=");
        assert_eq!(BinaryEncoding::Base64.encode(b"foo"), "Zm9v");
        assert_eq!(BinaryEncoding::Hex.encode(&[0xde, 0xad]), "dead");
    }

    /// 原始字节只在「单列且全是二进制」时给 —— 别的形态一律 `None`（不猜哪列是载荷）。
    #[test]
    fn raw_bytes_requires_a_single_binary_column() {
        let binary_only = ResultSet {
            columns: vec![Column {
                name: "blob".into(),
                type_name: "bytea".into(),
            }],
            rows: vec![vec![Value::Bytes(vec![1, 2])], vec![Value::Bytes(vec![3])]],
            truncated: false,
        };
        assert_eq!(binary_only.raw_bytes(), Some(vec![1, 2, 3]));

        // 混了文本列 → 不给。
        let mixed = ResultSet {
            columns: vec![
                Column {
                    name: "name".into(),
                    type_name: "text".into(),
                },
                Column {
                    name: "blob".into(),
                    type_name: "bytea".into(),
                },
            ],
            rows: vec![vec![Value::Text("a".into()), Value::Bytes(vec![1])]],
            truncated: false,
        };
        assert_eq!(mixed.raw_bytes(), None);

        // 单列但值是文本 → 不给（「导出附件」场景不含文本列）。
        let text_only = ResultSet {
            columns: vec![Column {
                name: "name".into(),
                type_name: "text".into(),
            }],
            rows: vec![vec![Value::Text("a".into())]],
            truncated: false,
        };
        assert_eq!(text_only.raw_bytes(), None);

        // 空结果集 → 不给。
        let empty = ResultSet {
            columns: vec![Column {
                name: "blob".into(),
                type_name: "bytea".into(),
            }],
            rows: Vec::new(),
            truncated: false,
        };
        assert_eq!(empty.raw_bytes(), None);
    }

    /// 美化 JSON 仍是合法 JSON，且与紧凑档类型规则一致。
    #[test]
    fn pretty_json_stays_parseable_and_agrees_with_compact() {
        let result = sample();
        let pretty: serde_json::Value =
            serde_json::from_str(&result.to_json_pretty()).expect("美化档必须是合法 JSON");
        let compact: serde_json::Value =
            serde_json::from_str(&result.to_json()).expect("紧凑档必须是合法 JSON");
        assert_eq!(pretty, compact);
        // 定点数一律字符串承载（本仓「绝不进 f64」硬纪律）。
        assert_eq!(pretty[0]["price"], serde_json::json!("1.50"));
        assert_eq!(pretty[0]["note"], serde_json::Value::Null);
    }

    /// 导出用的样例结果集（列名 ASCII，值覆盖各类型）。
    fn sample() -> ResultSet {
        ResultSet {
            columns: vec![
                Column {
                    name: "id".into(),
                    type_name: "integer".into(),
                },
                Column {
                    name: "name".into(),
                    type_name: "text".into(),
                },
                Column {
                    name: "price".into(),
                    type_name: "numeric".into(),
                },
                Column {
                    name: "note".into(),
                    type_name: "text".into(),
                },
            ],
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("a,b".into()),
                    Value::Decimal("1.50".into()),
                    Value::Null,
                ],
                vec![
                    Value::Int(2),
                    // 同时含单引号与双引号 + 换行：把 CSV 与 SQL 两种转义都逼出来。
                    Value::Text("it's \"quoted\"\nline".into()),
                    Value::Float(2.5),
                    Value::Bool(true),
                ],
            ],
            truncated: false,
        }
    }

    /// CSV：表头 + 分隔符 / 引号 / 换行的转义，NULL 按展示文本。
    #[test]
    fn to_csv_escapes_and_renders_header() {
        let csv = sample().to_csv();
        assert!(csv.starts_with("id,name,price,note\n"));
        assert!(csv.contains("1,\"a,b\",1.50,NULL\n"));
        // 引号双写 + 整体加引号（含换行的字段被引号包住）。
        assert!(csv.contains("2,\"it's \"\"quoted\"\"\nline\",2.5,true\n"));
    }

    /// JSON：数值 / 布尔给字面量，NULL 给 `null`，定点数与 `Json` 的口径见实现文档。
    #[test]
    fn to_json_keeps_types_and_never_loses_precision() {
        let json = sample().to_json();
        assert!(json.starts_with("[\n"));
        assert!(json.trim_end().ends_with(']'));
        assert!(json.contains("\"id\": 1"));
        assert!(json.contains("\"name\": \"a,b\""));
        // 定点数走字符串（绝不进 f64）。
        assert!(json.contains("\"price\": \"1.50\""));
        assert!(json.contains("\"note\": null"));
        assert!(json.contains("\"note\": true"));
        // 合法 JSON 的 `Json` 格原样嵌入，不套引号。
        let doc = ResultSet {
            columns: vec![Column {
                name: "doc".into(),
                type_name: "json".into(),
            }],
            rows: vec![vec![Value::Json(r#"{"a":1}"#.into())]],
            truncated: false,
        };
        assert!(doc.to_json().contains("\"doc\": {\"a\":1}"));
    }

    /// INSERT：列名按给定引号包裹、字符串双写单引号、NULL / 布尔 / 定点数的字面量口径。
    #[test]
    fn to_insert_sql_builds_literals() {
        let sql = sample().to_insert_sql("t", '"');
        assert!(sql.starts_with(
            "INSERT INTO t (\"id\", \"name\", \"price\", \"note\") VALUES (1, 'a,b', 1.50, NULL);\n"
        ));
        assert_eq!(sql.matches("INSERT INTO t ").count(), 2, "逐行一条");
        // 第二行的字符串值含真换行，字面量里也照样是换行（SQL 字符串允许）；
        // 单引号双写成 `''`，双引号在 SQL 单引号串里无需处理。
        assert!(sql.contains("VALUES (2, 'it''s \"quoted\"\nline', 2.5, TRUE);"));
        // 引号字符按引擎给（MySQL 反引号）。
        assert!(sample().to_insert_sql("t", '`').contains("(`id`, `name`"));
    }

    /// TSV：制表符分隔 + 表头，含制表符 / 换行 / 引号的单元格按 CSV 规矩包住。
    #[test]
    fn to_tsv_separates_with_tabs_and_quotes_only_when_needed() {
        let tsv = sample().to_tsv();
        assert!(tsv.starts_with("id\tname\tprice\tnote\n"));
        assert!(
            tsv.contains("1\ta,b\t1.50\tNULL\n"),
            "逗号在 TSV 里不是分隔符，不引"
        );
        // 换行与双引号被逼出来：整体包引号 + 内部引号双写。
        assert!(tsv.contains("2\t\"it's \"\"quoted\"\"\nline\"\t2.5\ttrue\n"));
        assert!(tsv.ends_with('\n'), "行式文本以换行收尾");

        // 制表符一定会被引号包住——漏掉这一步整张表就错列。
        let with_tab = ResultSet {
            columns: vec![Column {
                name: "a".into(),
                type_name: String::new(),
            }],
            rows: vec![vec![Value::Text("x\ty".into())]],
            truncated: false,
        };
        assert!(with_tab.to_tsv().contains("\"x\ty\""));

        // 单格转义与整集渲染同源（「复制整行」走的就是这条路）。
        assert_eq!(tsv_field("plain"), "plain");
        assert_eq!(tsv_field("a\tb"), "\"a\tb\"");
        assert_eq!(tsv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    /// Markdown：表头 + 分隔行 + 数据行；`|` 转义、换行折成 `<br>`、反斜杠先行转义。
    #[test]
    fn to_markdown_escapes_pipes_and_folds_newlines() {
        let md = sample().to_markdown();
        assert!(md.starts_with("| id | name | price | note |\n"));
        assert!(md.contains("| --- | --- | --- | --- |\n"), "GFM 分隔行");
        assert!(md.contains("| 1 | a,b | 1.50 | NULL |\n"));
        // 换行折成 <br>（Markdown 没有转义块，真换行会把一行切成两行）；
        // 引号在 Markdown 里没有语法意义，原样留。
        assert!(md.contains("| 2 | it's \"quoted\"<br>line | 2.5 | true |\n"));

        // 数据里的 `\|` 必须活到渲染后仍是一个 `|`：先转反斜杠再转竖线。
        let tricky = ResultSet {
            columns: vec![Column {
                name: "a".into(),
                type_name: String::new(),
            }],
            rows: vec![vec![Value::Text(r"a\|b".into())]],
            truncated: false,
        };
        assert!(tricky.to_markdown().contains(r"| a\\\|b |"));

        // 零列：给空串，而不是一截拼不成表的 `|`。
        assert!(ResultSet::default().to_markdown().is_empty());
        // 零行但有列：表头与分隔行照给（一张空表也是表）。
        let empty = ResultSet {
            columns: vec![Column {
                name: "only".into(),
                type_name: String::new(),
            }],
            rows: Vec::new(),
            truncated: false,
        };
        assert_eq!(empty.to_markdown(), "| only |\n| --- |\n");
    }

    /// 最小结果集（单列单行）在五种格式下的**逐字**输出。
    ///
    /// 界面的「复制为 TSV / CSV / Markdown / JSON / INSERT」菜单直接把这五份文本交给
    /// 剪贴板，所以这里钉死字面形状：改任何一处渲染规则，这里和界面的用例会一起红。
    #[test]
    fn a_minimal_result_set_renders_verbatim_in_every_format() {
        let result = ResultSet {
            columns: vec![Column {
                name: "id".into(),
                type_name: "INTEGER".into(),
            }],
            rows: vec![vec![Value::Int(1)]],
            truncated: false,
        };

        assert_eq!(result.to_tsv(), "id\n1\n");
        assert_eq!(result.to_csv(), "id\n1\n");
        assert_eq!(result.to_text(), "id\n1\n");
        assert_eq!(result.to_markdown(), "| id |\n| --- |\n| 1 |\n");
        assert_eq!(result.to_json(), "[\n  {\"id\": 1}\n]\n");
        assert_eq!(result.to_json_pretty(), "[\n  {\n    \"id\": 1\n  }\n]\n");
        assert_eq!(result.to_binary_dump(BinaryEncoding::Hex), "1\n");
        assert_eq!(
            result.to_insert_sql("t", '"'),
            "INSERT INTO t (\"id\") VALUES (1);\n"
        );
    }
}
