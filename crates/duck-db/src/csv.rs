//! CSV 读侧解析（导出走各驱动的 `to_csv`，这里管导入）。
//!
//! 语义约定：未加引号的空单元格解析为 `None`（导入时写 `NULL`），
//! 带引号的空串 `""` 保留为空字符串——与常见导入工具一致。

/// 把 CSV 文本解析为行。首行通常为表头（由调用方自行取舍）。
///
/// 支持：引号字段内的逗号 / 换行 / `""` 转义；CRLF 与 LF 行尾。
/// 尾随换行不产生空行；行宽不齐是合法输入，由调用方校验。
#[must_use]
pub fn parse_csv(text: &str) -> Vec<Vec<Option<String>>> {
    fn finish_field(row: &mut Vec<Option<String>>, field: &mut String, quoted: bool) {
        if quoted || !field.is_empty() {
            row.push(Some(std::mem::take(field)));
        } else {
            row.push(None);
        }
    }

    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut row: Vec<Option<String>> = Vec::new();
    let mut field = String::new();
    let mut quoted = false; // 当前字段带过引号（决定空值语义）
    let mut in_quotes = false; // 正处于引号内
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' if field.is_empty() => {
                    in_quotes = true;
                    quoted = true;
                }
                ',' => {
                    finish_field(&mut row, &mut field, quoted);
                    quoted = false;
                }
                '\n' => {
                    finish_field(&mut row, &mut field, quoted);
                    quoted = false;
                    rows.push(std::mem::take(&mut row));
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    finish_field(&mut row, &mut field, quoted);
                    quoted = false;
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    // 末尾无换行的残行。
    if !field.is_empty() || quoted || !row.is_empty() {
        finish_field(&mut row, &mut field, quoted);
        rows.push(row);
    }
    // 完全空白的行（单个未加引号的空字段）跳过——与常见导入工具一致。
    rows.retain(|row| !(row.len() == 1 && row[0].is_none()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_rows() {
        let rows = parse_csv("a,b,c\n1,2,3\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_deref(), Some("a"));
        assert_eq!(rows[1][2].as_deref(), Some("3"));
    }

    #[test]
    fn empty_is_null_quoted_empty_is_text() {
        let rows = parse_csv("x,,\"\"\n");
        assert_eq!(rows[0][0].as_deref(), Some("x"));
        assert_eq!(rows[0][1], None);
        assert_eq!(rows[0][2].as_deref(), Some(""));
    }

    #[test]
    fn quoted_comma_newline_and_escape() {
        let rows = parse_csv("\"a,b\",\"line1\nline2\",\"say \"\"hi\"\"\"\n");
        assert_eq!(rows[0][0].as_deref(), Some("a,b"));
        assert_eq!(rows[0][1].as_deref(), Some("line1\nline2"));
        assert_eq!(rows[0][2].as_deref(), Some("say \"hi\""));
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let rows = parse_csv("a,b\r\n1,2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1].as_deref(), Some("2"));
    }

    #[test]
    fn ragged_rows_allowed() {
        let rows = parse_csv("a,b\n1\n");
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[1].len(), 1);
    }

    #[test]
    fn empty_input() {
        assert!(parse_csv("").is_empty());
        assert!(parse_csv("\n").is_empty());
        assert!(parse_csv("\r\n\r\n").is_empty());
    }

    #[test]
    fn blank_lines_skipped() {
        let rows = parse_csv("a,b\n\n1,2\n");
        assert_eq!(rows.len(), 2);
    }
}
