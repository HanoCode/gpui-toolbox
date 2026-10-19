//! 结构化表格正文：`[io.output] kind = "table"` 的载荷与文本回退解析。
//!
//! 为什么结果表不能只当一段文本发：宿主的结果表要能排序、按列对齐、一键导出
//! CSV（参考项目 `ClassCompare.vue` / `SQLChecker.vue` 的结果表都是这样）。纯文本
//! 里数一列的位置只能靠猜，猜错就是错行——所以进程轨多一条通道（`body` JSON），
//! 把 `{columns, rows}` 送过来。
//!
//! 两条兼容路径：
//!
//! 1. **结构化**（首选）：插件发 `body`，字段就是本模块的 [`TableDoc`]；
//! 2. **Markdown 表**（回退）：插件只发 `output` 文本时，宿主用
//!    [`TableDoc::from_markdown`] 解析 GFM 管道表。`dir-compare` / `config-compare`
//!    这类工具本来就在拼 Markdown 表，零改动即可变成真表格。
//!
//! 两条都不成立时渲染器退回纯文本——表格是增强，不是前置条件。
//! `output` 文本始终保留一份（复制按钮复制的是它）。

use serde::{Deserialize, Serialize};

/// 宿主把表格编辑结果塞进 `params` 的这个键，再跑一次插件。
///
/// 语义是**前置修改**：插件收到它应当先把改动落进自己的数据，再照常执行
/// 原本的动作。这样「改几格 → 重跑」复用的仍是同一个动作，不需要为编辑
/// 另开一条双向协议（宿主 → 插件的 dispatch 是 v3 才有的事）。
///
/// 下划线开头是刻意的：它不可能和清单里的参数 id 撞车（参数 id 是标识符）。
pub const EDIT_PARAM: &str = "__edits";

/// 一条单元格改动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellEdit {
    /// 行标识。插件发 `row_ids` 时是它，没发时退化成行下标（`"0"`、`"1"`…）。
    ///
    /// ⚠️ 下标是**不稳定**的：重跑后行序一变，改动就落到别的行上。所以真正
    /// 要支持编辑的输出必须给 `row_ids`（见 [`TableDoc::row_ids`]）。
    pub row_id: String,
    /// 列名（与 [`TableDoc::columns`] 里的写法一致）。
    pub column: String,
    /// 新值（空串表示清空，不是「不改」）。
    pub value: String,
}

impl CellEdit {
    /// 从 `params[EDIT_PARAM]` 解出一批改动；没有 / 形状不对都当「无改动」。
    ///
    /// 宽容解而不是报错：老宿主不会发这个键，插件不该为此失败。
    #[must_use]
    pub fn decode_list(params: &serde_json::Value) -> Vec<Self> {
        params
            .get(EDIT_PARAM)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
}

/// 一份结果表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDoc {
    /// 表头（按显示顺序）。
    #[serde(default)]
    pub columns: Vec<String>,
    /// 行；每行按 `columns` 顺序给单元格。
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
    /// 行标识（与 `rows` 一一对应，可省略）。
    ///
    /// 只有它能让「编辑第 3 行」在重跑后依然指向同一行。省略时宿主按行下标
    /// 兜底（能用，但重排就错位）。长度不足的补空串，多余的忽略。
    #[serde(default)]
    pub row_ids: Vec<String>,
}

impl TableDoc {
    /// 构造一份表。
    #[must_use]
    pub fn new(columns: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self {
            columns,
            rows,
            row_ids: Vec::new(),
        }
    }

    /// 带行标识构造（可编辑输出的正路）。
    #[must_use]
    pub fn with_row_ids(
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        row_ids: Vec<String>,
    ) -> Self {
        Self {
            columns,
            rows,
            row_ids,
        }
    }

    /// 第 `index` 行的身份：有 `row_ids` 用它，没有就退化成下标。
    ///
    /// 空串也算「没有」——插件给了长度但没填内容时不该产生一个空 id。
    #[must_use]
    pub fn row_id(&self, index: usize) -> String {
        self.row_ids
            .get(index)
            .filter(|id| !id.is_empty())
            .cloned()
            .unwrap_or_else(|| index.to_string())
    }

    /// 是否空表。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() || self.rows.is_empty()
    }

    /// 行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 从 `body` 里解一份表；解不出来返回 `None`。
    ///
    /// 接受的形状：
    ///
    /// ```json
    /// {"columns": ["包名", "版本"], "rows": [["a.zip", "1.0"]]}
    /// {"columns": ["name", "ver"], "rows": [{"name": "a.zip", "ver": "1.0"}]}
    /// ```
    ///
    /// 行是对象时按列名取值（键既可以是列名、也可以是列 id）；缺列补空串。
    /// `fallback_columns` 是清单里 `[io.output] columns` 声明的表头：插件只给
    /// `rows` 时用它。两条都没有表头 ⇒ 认为不是表格（列名没法凭空造）。
    #[must_use]
    pub fn decode(body: Option<&serde_json::Value>, fallback_columns: &[String]) -> Option<Self> {
        let body = body?;
        let columns: Vec<String> = body
            .get("columns")
            .and_then(|value| value.as_array())
            .map(|items| items.iter().map(json_cell).collect())
            .filter(|columns: &Vec<String>| !columns.is_empty())
            .unwrap_or_else(|| fallback_columns.to_vec());
        if columns.is_empty() {
            return None;
        }
        let raw_rows = body.get("rows")?.as_array()?;
        let mut rows = Vec::with_capacity(raw_rows.len());
        for row in raw_rows {
            match row {
                serde_json::Value::Array(cells) => {
                    let mut line: Vec<String> = cells.iter().map(json_cell).collect();
                    line.resize(columns.len(), String::new());
                    rows.push(line);
                }
                serde_json::Value::Object(map) => {
                    rows.push(
                        columns
                            .iter()
                            .map(|column| map.get(column).map(json_cell).unwrap_or_default())
                            .collect(),
                    );
                }
                other => {
                    // 一行只有一个标量：当成单列表。
                    rows.push(vec![json_cell(other)]);
                }
            }
        }
        // 行标识可有可无；给了就按行对齐，长度不齐的由 `row_id()` 兜底。
        let mut row_ids: Vec<String> = body
            .get("row_ids")
            .and_then(|value| value.as_array())
            .map(|items| items.iter().map(json_cell).collect())
            .unwrap_or_default();
        row_ids.resize(rows.len(), String::new());
        Some(Self {
            columns,
            rows,
            row_ids,
        })
    }

    /// 把 Markdown 管道表解析成结构化表格；没有表头分隔行时返回 `None`。
    ///
    /// 判据是 **第二行必须是 `|---|:--:|` 这种分隔行**——只看到 `|` 还不能算
    /// 表格（正文里的 `a | b` 太常见），必须有分隔行才认。
    #[must_use]
    pub fn from_markdown(text: &str) -> Option<Self> {
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('|') && line.ends_with('|'));
        let header = lines.next()?;
        let divider = lines.next()?;
        if !is_divider(divider) {
            return None;
        }
        let columns = split_row(header);
        if columns.is_empty() {
            return None;
        }
        let mut rows = Vec::new();
        for line in lines {
            let mut cells = split_row(line);
            if cells.is_empty() {
                continue;
            }
            cells.resize(columns.len(), String::new());
            rows.push(cells);
        }
        // Markdown 里没有行标识可言——靠它的表天然不适合编辑（宿主会按下标兜底）。
        Some(Self {
            columns,
            rows,
            row_ids: Vec::new(),
        })
    }

    /// 导出 CSV（RFC 4180：含逗号 / 引号 / 换行的单元格加引号）。
    ///
    /// 前缀一个 UTF-8 BOM：Excel 在中文 Windows 上不认无 BOM 的 UTF-8，
    /// 双击打开就是乱码——而「导出 CSV 再双击」正是这个按钮的用法。
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = String::from("\u{feff}");
        out.push_str(&csv_line(&self.columns));
        for row in &self.rows {
            out.push_str(&csv_line(row));
        }
        out
    }

    /// 导出 Markdown 表。
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push('|');
        for column in &self.columns {
            out.push(' ');
            out.push_str(&column.replace('|', "\\|"));
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
            for index in 0..self.columns.len() {
                out.push(' ');
                out.push_str(
                    &row.get(index)
                        .cloned()
                        .unwrap_or_default()
                        .replace('|', "\\|"),
                );
                out.push_str(" |");
            }
            out.push('\n');
        }
        out
    }
}

/// JSON 标量 → 单元格文本（对象 / 数组按 JSON 原样给，不猜结构）。
fn json_cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Markdown 的一行 `| a | b |` → `["a", "b"]`（支持 `\|` 转义）。
fn split_row(line: &str) -> Vec<String> {
    let body = line
        .strip_prefix('|')
        .and_then(|rest| rest.strip_suffix('|'))
        .unwrap_or(line);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in body.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '|' => {
                cells.push(current.trim().to_owned());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    cells.push(current.trim().to_owned());
    cells
}

/// `|---|:--:|---:|` 形态的分隔行。
fn is_divider(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let cell = cell.trim_matches(':');
            !cell.is_empty() && cell.chars().all(|ch| ch == '-')
        })
}

/// CSV 一行（含 CRLF：RFC 4180 与 Excel 都吃这一口）。
fn csv_line(cells: &[String]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        if cell.contains([',', '"', '\n', '\r']) {
            line.push('"');
            line.push_str(&cell.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(cell);
        }
    }
    line.push_str("\r\n");
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_rows_as_arrays_pad_to_column_count() {
        let body = json!({
            "columns": ["包名", "版本"],
            "rows": [["a.zip"], ["b.zip", "2.0"]]
        });
        let doc = TableDoc::decode(Some(&body), &[]).expect("a table");
        assert_eq!(doc.columns, ["包名", "版本"]);
        assert_eq!(doc.rows[0], ["a.zip", ""]);
        assert_eq!(doc.rows[1], ["b.zip", "2.0"]);
    }

    #[test]
    fn body_rows_as_objects_use_column_keys() {
        let body = json!({
            "columns": ["name", "ver"],
            "rows": [{"name": "a.zip", "ver": "1.0"}]
        });
        let doc = TableDoc::decode(Some(&body), &[]).expect("a table");
        assert_eq!(doc.rows[0], ["a.zip", "1.0"]);
    }

    #[test]
    fn manifest_columns_fill_in_when_body_omits_them() {
        let body = json!({ "rows": [["x", "y"]] });
        let doc = TableDoc::decode(Some(&body), &["A".into(), "B".into()]).expect("a table");
        assert_eq!(doc.columns, ["A", "B"]);
        assert_eq!(doc.rows[0], ["x", "y"]);
    }

    #[test]
    fn markdown_table_is_recognised_only_with_a_divider() {
        let table = "\
| 文件 | 状态 |
| --- | :--: |
| a.txt | 新增 |
| b.txt | 相同 |
";
        let doc = TableDoc::from_markdown(table).expect("a markdown table");
        assert_eq!(doc.columns, ["文件", "状态"]);
        assert_eq!(doc.rows.len(), 2);
        assert_eq!(doc.rows[1], ["b.txt", "相同"]);

        // 没有分隔行：正文里带竖线不算表。
        assert!(TableDoc::from_markdown("| a | b |\n| c | d |").is_none());
    }

    #[test]
    fn csv_quotes_cells_and_starts_with_a_bom() {
        let doc = TableDoc::new(
            vec!["名称".into(), "备注".into()],
            vec![vec!["a,b".into(), "带\"引号\"".into()]],
        );
        let csv = doc.to_csv();
        assert!(csv.starts_with('\u{feff}'));
        assert!(csv.contains("\"a,b\",\"带\"\"引号\"\"\"\r\n"));
    }

    #[test]
    fn markdown_export_escapes_pipes() {
        let doc = TableDoc::new(vec!["列".into()], vec![vec!["a|b".into()]]);
        let md = doc.to_markdown();
        assert!(md.contains("a\\|b"));
        assert!(md.contains("| --- |"));
    }

    #[test]
    fn row_ids_travel_with_the_body_and_pad_to_row_count() {
        let body = json!({
            "columns": ["字段"],
            "rows": [["a"], ["b"], ["c"]],
            "row_ids": ["f1", "f2"]
        });
        let doc = TableDoc::decode(Some(&body), &[]).expect("a table");
        assert_eq!(doc.row_id(0), "f1");
        assert_eq!(doc.row_id(1), "f2");
        // 少给的那个按下标兜底，不 panic 也不串到上一行。
        assert_eq!(doc.row_id(2), "2");
    }

    #[test]
    fn row_id_falls_back_to_the_index_without_ids() {
        let doc = TableDoc::new(vec!["A".into()], vec![vec!["x".into()]]);
        assert_eq!(doc.row_id(0), "0");
        // 空串也算「没给」——长度给了但内容是空，不该产生空 id。
        let blank = TableDoc::with_row_ids(
            vec!["A".into()],
            vec![vec!["x".into()]],
            vec![String::new()],
        );
        assert_eq!(blank.row_id(0), "0");
    }

    #[test]
    fn edits_decode_from_the_reserved_param_key() {
        let params = json!({
            EDIT_PARAM: [
                {"row_id": "f1/tradeDate", "column": "含义", "value": "交易日"},
                {"row_id": "f1/amount", "column": "类型", "value": "amount"}
            ],
            "bank_code": "hzcb"
        });
        let edits = CellEdit::decode_list(&params);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].column, "含义");
        assert_eq!(edits[1].value, "amount");
        // 老宿主不发这个键：空的，不是错误。
        assert!(CellEdit::decode_list(&json!({"bank_code": "hzcb"})).is_empty());
        // 形状不对也只当没有（插件不该为宿主的 bug 失败）。
        assert!(CellEdit::decode_list(&json!({ EDIT_PARAM: "oops" })).is_empty());
    }
}
