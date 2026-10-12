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
}
