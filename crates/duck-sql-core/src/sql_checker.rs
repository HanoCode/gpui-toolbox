//! SQL 脚本静态检查（28 类规则：语法 / 事务 / 破坏性 / 一致性 / 规范 / 表空间 …）。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/sql_checker.rs`
//! （3,198 行，已在生产使用中验证）。移植原则见 `docs/ifm-plugin-port-plan.md` §6.4 与
//! 用户口径「复刻不复研」：只允许加这个模块头、去掉 Tauri 命令属性，
//! **函数体一字不改**——改了就要重新验证 28 条规则的行为，而这正是移植要避免的成本。
//!
//! 唯一的签名改动：`check_sql_command`（`#[tauri::command]` 包装）去掉属性后
//! 改名为 [`check_sql_text`]，逻辑仍然是 `check_sql(&sql, &config.unwrap_or_default())`。
#![allow(
    unused_parens,
    // 下面这批 clippy 建议**全部**落在复刻来的函数体里，改成建议写法就是重写逻辑。
    // 其中 `collapsible_if` / `collapsible_match` 的正确写法（let-chains）还要
    // Rust 1.88，而本仓 MSRV 是 1.85。逐条列名而不是 `allow(clippy::all)`：
    // 这样「源仓代码触发了哪些 lint」留在代码里，将来真要看 diff 一目了然。
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::trim_split_whitespace,
    clippy::explicit_counter_loop,
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::single_match,
    // 复刻文件自带的 `mod tests`（3 条）里用 `cfg.db_type = …` 改默认值。
    clippy::field_reassign_with_default
)]

use serde::{Deserialize, Serialize};
use sqlparser::ast::{Statement, ObjectType, Query, SetExpr, Value};
use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, MsSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConfig {
    pub db_type: String,
    pub max_delete_rows: usize,
    pub max_update_rows: usize,
    pub allow_unconditional_delete: bool,
    pub check_timeout: u64,
    /// 可配置的允许表空间白名单；None/空 表示不检查表空间
    pub allowed_tablespaces: Option<Vec<String>>,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            db_type: "oracle".to_string(),
            max_delete_rows: 10000,
            max_update_rows: 10000,
            allow_unconditional_delete: false,
            check_timeout: 30,
            allowed_tablespaces: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Severity {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "info")]
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleCategory {
    Syntax,
    Security,
    Transaction,
    Consistency,
    Performance,
    Convention,
    ErrorHandling,
    Idempotency,
    Environment,
    Integrity,
    Order,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeLocation {
    pub line: usize,
    pub column: usize,
    pub end_line: Option<usize>,
    pub end_column: Option<usize>,
    pub snippet: Option<String>,
    pub statement_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub rule_id: String,
    pub rule_name: String,
    pub severity: Severity,
    pub category: RuleCategory,
    pub location: CodeLocation,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    pub results: Vec<CheckResult>,
    pub total_errors: usize,
    pub total_warnings: usize,
    pub total_info: usize,
    pub execution_time_ms: u64,
}

/// 一条已成功解析的语句：(起始行号, 原始文本, AST)
type ParsedStmt = (usize, String, Statement);

fn get_dialect(db_type: &str) -> Box<dyn Dialect> {
    match db_type.to_lowercase().as_str() {
        "mysql" | "mariadb" | "tidb" | "oceanbase" => Box::new(MySqlDialect {}),
        "postgresql" | "postgres" | "gaussdb" | "kingbase" | "opengauss" | "highgo" | "vastbase" => Box::new(PostgreSqlDialect {}),
        "sqlserver" | "mssql" => Box::new(MsSqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

/// Oracle/DM/GaussDB/KingBase 等 Oracle 兼容方言
fn is_oracle_like(db_type: &str) -> bool {
    matches!(
        db_type.to_lowercase().as_str(),
        "oracle" | "dameng" | "dm" | "gaussdb" | "opengauss" | "kingbase" | "highgo" | "vastbase"
    )
}

/// 支持 DELETE/UPDATE ... LIMIT 的数据库
fn supports_limit(db_type: &str) -> bool {
    matches!(
        db_type.to_lowercase().as_str(),
        "mysql" | "mariadb" | "tidb" | "oceanbase" | "sqlite"
    )
}

/// 支持 CREATE TABLE/INDEX ... IF NOT EXISTS 的数据库
fn supports_if_not_exists(db_type: &str) -> bool {
    matches!(
        db_type.to_lowercase().as_str(),
        "mysql" | "mariadb" | "tidb" | "oceanbase" | "postgresql" | "postgres" | "sqlite"
            | "gaussdb" | "opengauss" | "kingbase" | "highgo" | "vastbase"
    )
}

fn get_line_snippet(lines: &[&str], line: usize) -> Option<String> {
    lines.get(line.saturating_sub(1)).map(|s| s.trim().to_string())
}

fn is_always_true_condition(expr: &sqlparser::ast::Expr) -> bool {
    match expr {
        sqlparser::ast::Expr::BinaryOp { left, op, right } => {
            if let sqlparser::ast::BinaryOperator::Eq = op {
                if let (sqlparser::ast::Expr::Value(Value::Number(l, _)), sqlparser::ast::Expr::Value(Value::Number(r, _))) = (left.as_ref(), right.as_ref()) {
                    return l == r;
                }
            }
            false
        }
        sqlparser::ast::Expr::Value(Value::Boolean(true)) => true,
        _ => false,
    }
}

fn has_select_star(query: &Query) -> bool {
    if let SetExpr::Select(select) = &*query.body {
        for projection in &select.projection {
            match projection {
                sqlparser::ast::SelectItem::Wildcard(_) => return true,
                sqlparser::ast::SelectItem::QualifiedWildcard(_, _) => return true,
                _ => {}
            }
        }
    }
    false
}

fn has_subquery(expr: &sqlparser::ast::Expr) -> bool {
    match expr {
        sqlparser::ast::Expr::Subquery(_) => true,
        sqlparser::ast::Expr::BinaryOp { left, right, .. } => has_subquery(left) || has_subquery(right),
        _ => false,
    }
}

fn preprocess_sql(sql: &str) -> String {
    let sql = sql.strip_prefix('\u{feff}').unwrap_or(sql);
    let mut result = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        result.push(ch);
        if ch == '-' {
            if chars.peek() == Some(&'-') {
                result.push('-');
                chars.next();
                let mut comment_ended_with_newline = false;
                let mut last_two = [char::default(); 2];
                let mut pos = 0;
                for c in chars.by_ref() {
                    last_two[pos % 2] = c;
                    pos += 1;
                    if c == '\n' {
                        result.push(c);
                        comment_ended_with_newline = true;
                        break;
                    }
                    result.push(c);
                }
                if !comment_ended_with_newline && !result.ends_with('\n') {
                    result.push('\n');
                }
            }
        }
    }
    result
}

/// 去掉行注释与块注释，保留字符串与换行（用于字符串级规则，避免注释干扰）
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let n = chars.len();
    let mut quote: Option<char> = None;
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                if i + 1 < n && chars[i + 1] == q {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            if i < n {
                out.push('\n');
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 剔除字符串字面量内容（保留结构、行号与长度），用于关键词模式扫描，
/// 避免 `'SELECT'`、`'LIKE %'` 等业务字符串被误判。
/// 支持标准转义（''/""）与 Oracle 替代引号（q'[...]' q'{...}' q'(...)' q'<...>'）。
fn strip_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let mut skipped_q = false;
            if c == '\''
                && i >= 1
                && (chars[i - 1] == 'q' || chars[i - 1] == 'Q')
                && (i == 1 || (!chars[i - 2].is_ascii_alphanumeric() && chars[i - 2] != '_'))
            {
                if let Some(delim) = chars.get(i + 1).copied() {
                    let closing = match delim {
                        '[' => Some(']'),
                        '{' => Some('}'),
                        '(' => Some(')'),
                        '<' => Some('>'),
                        _ => None,
                    };
                    if let Some(cl) = closing {
                        let mut j = i + 2;
                        while j < n {
                            if chars[j] == cl && chars.get(j + 1) == Some(&'\'') {
                                out.push(' ');
                                i = j + 2;
                                skipped_q = true;
                                break;
                            }
                            j += 1;
                        }
                    }
                }
            }
            if skipped_q {
                continue;
            }
            let q = c;
            out.push(' ');
            i += 1;
            while i < n {
                let ch = chars[i];
                if ch == q {
                    if chars.get(i + 1) == Some(&q) {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                        continue;
                    }
                    out.push(' ');
                    i += 1;
                    break;
                }
                if ch == '\n' {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 将 Oracle 替代引号（q'[..]' q'{..}' q'(..)' q'<..>'）归一化为普通字符串字面量，
/// 使 sqlparser 可以正常解析。仅在一次解析失败后的降级路径中调用。
fn normalize_q_strings(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '\''
            && i >= 1
            && (chars[i - 1] == 'q' || chars[i - 1] == 'Q')
            && (i == 1 || (!chars[i - 2].is_ascii_alphanumeric() && chars[i - 2] != '_'))
        {
            if let Some(delim) = chars.get(i + 1).copied() {
                let closing = match delim {
                    '[' => Some(']'),
                    '{' => Some('}'),
                    '(' => Some(')'),
                    '<' => Some('>'),
                    _ => None,
                };
                if let Some(cl) = closing {
                    let mut j = i + 2;
                    let mut found = false;
                    while j < n {
                        if chars[j] == cl && chars.get(j + 1) == Some(&'\'') {
                            found = true;
                            break;
                        }
                        j += 1;
                    }
                    if found {
                        out.pop();
                        out.push('\'');
                        for k in i + 2..j {
                            if chars[k] == '\'' {
                                out.push('\'');
                                out.push('\'');
                            } else {
                                out.push(chars[k]);
                            }
                        }
                        out.push('\'');
                        i = j + 2;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 按分号切分语句，返回 (起始行号, 语句文本, 是否以分号结束)
fn split_statements(sql: &str) -> Vec<(usize, String, bool)> {
    let mut chunks: Vec<(usize, String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut line = 1usize;
    let mut start_line = 1usize;
    let mut cur_has_content = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut quote: Option<char> = None;
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if in_line_comment {
            cur.push(c);
            if c == '\n' {
                in_line_comment = false;
                line += 1;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            cur.push(c);
            if c == '*' && next == Some('/') {
                cur.push('/');
                i += 1;
                in_block_comment = false;
            }
            if c == '\n' {
                line += 1;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                if next == Some(q) {
                    cur.push(q);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            if c == '\n' {
                line += 1;
            }
            i += 1;
            continue;
        }
        if c == '-' && next == Some('-') {
            in_line_comment = true;
            cur.push(c);
            cur.push(next.expect("next is Some('-')"));
            i += 2;
            continue;
        }
        if c == '/' && next == Some('*') {
            in_block_comment = true;
            cur.push(c);
            cur.push(next.expect("next is Some('*')"));
            i += 2;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
            if !cur_has_content {
                cur_has_content = true;
                start_line = line;
            }
            i += 1;
            continue;
        }
        if c == ';' {
            let trimmed = cur.trim();
            if has_code(trimmed) {
                chunks.push((start_line, trimmed.to_string(), true));
            }
            cur.clear();
            cur_has_content = false;
            i += 1;
            continue;
        }
        if c == '\n' {
            line += 1;
        }
        if !c.is_whitespace() && !cur_has_content {
            cur_has_content = true;
            start_line = line;
        }
        cur.push(c);
        i += 1;
    }
    let trimmed = cur.trim();
    if has_code(trimmed) {
        chunks.push((start_line, trimmed.to_string(), false));
    }
    chunks
}

/// 判断一段文本中是否包含真正的 SQL 代码（过滤纯注释）
fn has_code(text: &str) -> bool {
    let cleaned = strip_comments(text);
    cleaned.chars().any(|c| !c.is_whitespace())
}

/// 去除 Oracle 独有、GenericDialect 无法解析的语法（TABLESPACE 子句、多列 MODIFY）
fn sanitize_oracle_syntax(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let q = c;
            out.push(c);
            i += 1;
            while i < n {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < n && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == '$' && i + 1 < n && chars[i + 1] == '{' {
            let mut e = i + 2;
            while e < n && chars[e] != '}' {
                e += 1;
            }
            if e < n {
                let inner: String = chars[i + 2..e].iter().collect();
                if !inner.is_empty() {
                    out.push_str(&inner);
                } else {
                    out.push('X');
                }
                i = e + 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || (chars[i] == '$' && !(i + 1 < n && chars[i + 1] == '{'))) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word.eq_ignore_ascii_case("TABLESPACE") {
                while i < n && chars[i].is_whitespace() {
                    i += 1;
                }
                if i < n && (chars[i] == '\'' || chars[i] == '"') {
                    let q = chars[i];
                    i += 1;
                    while i < n && chars[i] != q {
                        i += 1;
                    }
                    if i < n {
                        i += 1;
                    }
                } else {
                    while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                        i += 1;
                    }
                }
                out.push(' ');
                continue;
            }
            if word.eq_ignore_ascii_case("CASCADE") {
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                let mut k = j;
                while k < n && (chars[k].is_ascii_alphabetic() || chars[k] == '_') {
                    k += 1;
                }
                let nxt: String = chars[j..k].iter().collect();
                if nxt.eq_ignore_ascii_case("DROP") {
                    let mut p = k;
                    while p < n && chars[p].is_whitespace() {
                        p += 1;
                    }
                    let mut q = p;
                    while q < n && (chars[q].is_ascii_alphabetic() || chars[q] == '_') {
                        q += 1;
                    }
                    let nxt2: String = chars[p..q].iter().collect();
                    if nxt2.eq_ignore_ascii_case("INDEX") {
                        out.push_str("CASCADE");
                        i = q;
                        continue;
                    }
                }
                if nxt.eq_ignore_ascii_case("CONSTRAINTS") {
                    out.push_str("CASCADE");
                    i = k;
                    continue;
                }
                out.push_str(&word);
                i = start + word.len();
                continue;
            }
            out.push_str(&word);
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 将 `MODIFY (col1 type, col2 type)` 折叠为首列，便于 GenericDialect 解析
fn collapse_alter_modify(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let q = c;
            out.push(c);
            i += 1;
            while i < n {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < n && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word.eq_ignore_ascii_case("MODIFY") {
                out.push_str("MODIFY");
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < n && chars[j] == '(' {
                    let mut depth = 0i32;
                    let mut k = j;
                    let mut end = None;
                    while k < n {
                        match chars[k] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = Some(k);
                                    break;
                                }
                            }
                            '\'' | '"' => {
                                let q = chars[k];
                                k += 1;
                                while k < n && chars[k] != q {
                                    k += 1;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    let end = end.unwrap_or(n);
                    let inner = &chars[j + 1..end];
                    let mut depth2 = 0i32;
                    let mut first_seg_end = inner.len();
                    for (idx, ch) in inner.iter().enumerate() {
                        match ch {
                            '(' => depth2 += 1,
                            ')' => depth2 -= 1,
                            ',' if depth2 == 0 => {
                                first_seg_end = idx;
                                break;
                            }
                            _ => {}
                        }
                    }
                    let seg: String = inner[..first_seg_end].iter().collect();
                    out.push(' ');
                    out.push_str(seg.trim());
                    i = end + 1;
                    continue;
                }
                i = start + word.len();
                continue;
            }
            out.push_str(&word);
            i = start + word.len();
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 将 Oracle 的 `ALTER TABLE t ADD (c1 ..., c2 ...)` 拆分为多条独立 ALTER 语句，
/// 保留所有列定义（sqlparser 不支持括号包裹的多列 ADD/MODIFY）。
fn expand_alter_column_lists(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c == '\'' || c == '"' {
            let q = c;
            out.push(c);
            i += 1;
            while i < n {
                out.push(chars[i]);
                if chars[i] == q {
                    if i + 1 < n && chars[i + 1] == q {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word.eq_ignore_ascii_case("ALTER") {
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                let mut k = j;
                while k < n && (chars[k].is_ascii_alphabetic() || chars[k] == '_') {
                    k += 1;
                }
                let kw: String = chars[j..k].iter().collect();
                if kw.eq_ignore_ascii_case("TABLE") {
                    while k < n && chars[k].is_whitespace() {
                        k += 1;
                    }
                    let mut m = k;
                    while m < n && !chars[m].is_whitespace() && chars[m] != '(' {
                        m += 1;
                    }
                    let table_name: String = chars[k..m].iter().collect();
                    if !table_name.is_empty() {
                        let mut p = m;
                        while p < n && chars[p].is_whitespace() {
                            p += 1;
                        }
                        let mut q2 = p;
                        while q2 < n && (chars[q2].is_ascii_alphabetic() || chars[q2] == '_') {
                            q2 += 1;
                        }
                        let op: String = chars[p..q2].iter().collect();
                        if (op.eq_ignore_ascii_case("ADD") || op.eq_ignore_ascii_case("MODIFY")) {
                            let mut r = q2;
                            while r < n && chars[r].is_whitespace() {
                                r += 1;
                            }
                            if r < n && chars[r] == '(' {
                                let mut depth = 0i32;
                                let mut e = r;
                                let mut end = None;
                                while e < n {
                                    match chars[e] {
                                        '(' => depth += 1,
                                        ')' => {
                                            depth -= 1;
                                            if depth == 0 {
                                                end = Some(e);
                                                break;
                                            }
                                        }
                                        '\'' | '"' => {
                                            let qq = chars[e];
                                            e += 1;
                                            while e < n && chars[e] != qq {
                                                e += 1;
                                            }
                                        }
                                        _ => {}
                                    }
                                    e += 1;
                                }
                                if let Some(end) = end {
                                    let inner: Vec<char> = chars[r + 1..end].to_vec();
                                    let segs = split_top_level_commas(&inner);
                                    if !segs.is_empty() {
                                        let mut expanded = String::new();
                                        for (idx, seg) in segs.iter().enumerate() {
                                            let seg_s: String = seg.iter().collect();
                                            if seg_s.trim().is_empty() {
                                                continue;
                                            }
                                            if idx > 0 {
                                                expanded.push(';');
                                            }
                                            expanded.push_str(&format!(
                                                "ALTER TABLE {} {} {}",
                                                table_name, op, seg_s.trim()
                                            ));
                                        }
                                        out.push_str(&expanded);
                                        i = end + 1;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            out.push_str(&word);
            i = start + word.len();
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 按顶层逗号切分（跳过括号与字符串），返回各段
fn split_top_level_commas(chars: &[char]) -> Vec<Vec<char>> {
    let mut segs = Vec::new();
    let mut cur = Vec::new();
    let mut depth = 0i32;
    let mut q: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(qq) = q {
            cur.push(ch);
            if ch == qq {
                if i + 1 < chars.len() && chars[i + 1] == qq {
                    cur.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                q = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            '\'' | '"' => {
                q = Some(ch);
                cur.push(ch);
            }
            ',' if depth == 0 => {
                segs.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
        i += 1;
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    segs
}

/// 解析单条语句；Oracle 兼容方言解析失败时做语法降级
fn parse_chunk(text: &str, dialect: &dyn Dialect, oracle_like: bool) -> Result<Vec<Statement>, String> {
    if let Ok(stmts) = Parser::parse_sql(dialect, text) {
        return Ok(stmts);
    }
    let normalized = normalize_q_strings(text);
    if normalized != text {
        if let Ok(stmts) = Parser::parse_sql(dialect, &normalized) {
            return Ok(stmts);
        }
    }
    if oracle_like {
        let code = strip_comments(text);
        let upper = code.trim_start().to_uppercase();
        if upper.starts_with("COMMENT ON") {
            return Ok(vec![]);
        }
        if upper.starts_with("CREATE OR REPLACE TYPE") || upper.starts_with("CREATE TYPE") {
            return Ok(vec![]);
        }
        let sanitized = sanitize_oracle_syntax(&normalized);
        let expanded = expand_alter_column_lists(&sanitized);
        let collapsed = collapse_alter_modify(&expanded);
        if let Ok(stmts) = Parser::parse_sql(dialect, &collapsed) {
            return Ok(stmts);
        }
    }
    match Parser::parse_sql(dialect, text) {
        Ok(stmts) => Ok(stmts),
        Err(e) => Err(e.to_string()),
    }
}

pub fn check_sql(sql: &str, config: &CheckConfig) -> CheckReport {
    let start = std::time::Instant::now();
    let mut results: Vec<CheckResult> = Vec::new();
    let dialect = get_dialect(&config.db_type);
    let processed_sql = preprocess_sql(sql);
    let lines: Vec<&str> = processed_sql.lines().collect();
    let code_only = strip_comments(&processed_sql);
    let code_nostr = strip_string_literals(&code_only);
    let nostr_lines: Vec<&str> = code_nostr.lines().collect();
    let upper_nostr = code_nostr.to_uppercase();
    let oracle_like = is_oracle_like(&config.db_type);

    // ---------- 解析：逐语句，失败只影响该语句 ----------
    let chunks = split_statements(&processed_sql);
    let mut parsed: Vec<ParsedStmt> = Vec::new();
    for (chunk_line, text, _) in &chunks {
        match parse_chunk(text, dialect.as_ref(), oracle_like) {
            Ok(stmts) => {
                for stmt in stmts {
                    parsed.push((*chunk_line, text.clone(), stmt));
                }
            }
            Err(msg) => {
                results.push(CheckResult {
                    rule_id: "S01".into(),
                    rule_name: "SQL语法错误".into(),
                    severity: Severity::Error,
                    category: RuleCategory::Syntax,
                    location: CodeLocation {
                        line: *chunk_line,
                        column: 1,
                        end_line: None,
                        end_column: None,
                        snippet: get_line_snippet(&lines, *chunk_line),
                        statement_index: None,
                    },
                    message: format!("SQL解析失败: {}", msg),
                    suggestion: Some("请检查SQL语法是否正确".into()),
                });
            }
        }
    }

    // S02: 关键字拼写错误（注释已剔除）
    let typos = [
        ("SELEC ", "SELECT"),
        ("UPDTE ", "UPDATE"),
        ("DELET ", "DELETE"),
        ("INERT ", "INSERT"),
        ("CREAT ", "CREATE"),
        ("ALERT ", "ALTER"),
        ("DRPO ", "DROP"),
        ("WHEER ", "WHERE"),
        ("WHRE ", "WHERE"),
    ];
    for (typo, correct) in &typos {
        if upper_nostr.contains(&typo.to_uppercase()) {
            let line = nostr_lines
                .iter()
                .position(|l| l.to_uppercase().contains(&typo.to_uppercase()))
                .map(|l| l + 1)
                .unwrap_or(1);
            results.push(CheckResult {
                rule_id: "S02".into(),
                rule_name: "关键字拼写错误".into(),
                severity: Severity::Error,
                category: RuleCategory::Syntax,
                location: CodeLocation {
                    line,
                    column: 1,
                    end_line: None,
                    end_column: None,
                    snippet: get_line_snippet(&lines, line),
                    statement_index: None,
                },
                message: format!("疑似关键字拼写错误: '{}'，正确写法: '{}'", typo.trim(), correct),
                suggestion: Some(format!("请将 '{}' 修改为 '{}'", typo.trim(), correct)),
            });
        }
    }

    // S03 / S04: 括号与引号（跳过注释与字符串转义）
    results.extend(check_brackets(&processed_sql, &lines));
    results.extend(check_quotes(&processed_sql, &lines));

    // S05: 末尾分号
    if !config.db_type.eq_ignore_ascii_case("sqlserver") {
        if let Some((line, _, ends_with_semi)) = chunks.last() {
            if !ends_with_semi {
                results.push(CheckResult {
                    rule_id: "S05".into(),
                    rule_name: "分号缺失".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Syntax,
                    location: CodeLocation {
                        line: *line,
                        column: 1,
                        end_line: None,
                        end_column: None,
                        snippet: get_line_snippet(&lines, *line),
                        statement_index: None,
                    },
                    message: "脚本末尾缺少分号".into(),
                    suggestion: Some("建议在脚本末尾添加分号".into()),
                });
            }
        }
    }

    // 事务（AST 级，避免列名/注释误命中）
    results.extend(check_transaction(&parsed, &lines));

    // 数据安全
    results.extend(check_unconditional_delete(&parsed, &lines, config));
    results.extend(check_unconditional_update(&parsed, &lines, config));
    results.extend(check_truncate_drop(&parsed, &lines));
    results.extend(check_batch_thresholds(&parsed, &lines, config));
    results.extend(check_select_star(&parsed, &lines));
    results.extend(check_select_without_where(&parsed, &lines));
    results.extend(check_ddl_autocommit(&parsed, &lines));
    results.extend(check_subquery(&parsed, &lines));
    results.extend(check_idempotency(&parsed, &lines, &config.db_type));
    results.extend(check_naming_convention(&parsed, &lines, &config.db_type));
    results.extend(check_reserved_words(&parsed, &lines));
    results.extend(check_duplicate_insert(&parsed, &lines, &config.db_type));
    results.extend(check_unique_constraint(&parsed, &lines));
    results.extend(check_not_null(&parsed, &lines));
    results.extend(check_type_mismatch(&parsed, &lines));
    results.extend(check_data_length(&parsed, &lines));
    results.extend(check_date_format(&parsed, &lines));
    results.extend(check_object_dependency(&parsed, &lines));
    results.extend(check_temp_table_order(&parsed, &lines));
    results.extend(check_data_consistency(&parsed, &lines));
    results.extend(check_delete_insert_condition(&parsed, &lines));
    results.extend(check_data_dependency(&parsed, &lines));
    results.extend(check_foreign_key(&parsed, &lines));
    results.extend(check_circular_dependency(&parsed, &lines));

    // D08: 危险操作备份提示（仅 TRUNCATE/DROP，信息级）
    if (upper_nostr.contains("TRUNCATE") || upper_nostr.contains("DROP TABLE"))
        && !upper_nostr.contains("BACKUP")
    {
        results.push(CheckResult {
            rule_id: "D08".into(),
            rule_name: "缺少备份建议".into(),
            severity: Severity::Info,
            category: RuleCategory::Security,
            location: CodeLocation {
                line: 1,
                column: 1,
                end_line: None,
                end_column: None,
                snippet: None,
                statement_index: None,
            },
            message: "脚本包含危险操作但未发现备份语句".into(),
            suggestion: Some("建议在执行危险操作前先备份数据".into()),
        });
    }

    // P03: LIKE 前导通配符（AST 级，避免字符串误报）
    results.extend(check_like_leading_wildcard(&parsed, &lines));

    // P04: IN 子查询
    if upper_nostr.contains("IN (SELECT") {
        let line = nostr_lines
            .iter()
            .position(|l| l.to_uppercase().contains("IN (SELECT"))
            .map(|l| l + 1)
            .unwrap_or(1);
        results.push(CheckResult {
            rule_id: "P04".into(),
            rule_name: "大数据量操作".into(),
            severity: Severity::Warning,
            category: RuleCategory::Performance,
            location: CodeLocation {
                line,
                column: 1,
                end_line: None,
                end_column: None,
                snippet: get_line_snippet(&lines, line),
                statement_index: None,
            },
            message: "IN子查询可能导致性能问题".into(),
            suggestion: Some("考虑使用EXISTS或JOIN".into()),
        });
    }

    // 环境/规范类
    results.extend(check_error_handling(&code_nostr, &lines, &config.db_type));
    results.extend(check_affected_rows(&code_nostr, &lines, &config.db_type));
    results.extend(check_environment(&code_only, &lines, &parsed));
    results.extend(check_keyword_case(&nostr_lines));
    results.extend(check_missing_comments(&lines));
    results.extend(check_unreplaced_placeholder(&code_only, &lines));
    results.extend(check_indentation(&lines));
    results.extend(check_error_continue(&code_nostr, &lines, &config.db_type));
    results.extend(check_version_control(&code_nostr, &lines));
    results.extend(check_permission(&code_nostr, &lines));
    results.extend(check_tablespace_compatibility(&parsed, &lines, &config.db_type, &config.allowed_tablespaces));
    results.extend(check_tablespace_specified(&parsed, &lines, &config.db_type));
    results.extend(check_index_tablespace(&parsed, &lines, &config.db_type));
    results.extend(check_partition_tablespace(&code_nostr, &lines));
    results.extend(check_charset_compatibility(&code_nostr, &lines));
    results.extend(check_version_syntax(&code_nostr, &lines, &config.db_type));
    results.extend(check_variable_order(&code_nostr, &lines, &config.db_type));

    let total_errors = results.iter().filter(|r| matches!(r.severity, Severity::Error)).count();
    let total_warnings = results.iter().filter(|r| matches!(r.severity, Severity::Warning)).count();
    let total_info = results.iter().filter(|r| matches!(r.severity, Severity::Info)).count();

    CheckReport {
        results,
        total_errors,
        total_warnings,
        total_info,
        execution_time_ms: start.elapsed().as_millis() as u64,
    }
}

fn loc(line: usize, lines: &[&str], stmt_idx: Option<usize>) -> CodeLocation {
    CodeLocation {
        line,
        column: 1,
        end_line: None,
        end_column: None,
        snippet: get_line_snippet(lines, line),
        statement_index: stmt_idx,
    }
}

fn check_brackets(sql: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut line_num = 1;
    let mut in_string = false;
    let mut string_char = ' ';
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
                line_num += 1;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if ch == '*' && next == Some('/') {
                in_block_comment = false;
                i += 1;
            }
            if ch == '\n' {
                line_num += 1;
            }
            i += 1;
            continue;
        }
        if in_string {
            if ch == string_char {
                if next == Some(string_char) {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            if ch == '\n' {
                line_num += 1;
            }
            i += 1;
            continue;
        }
        if ch == '-' && next == Some('-') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if ch == '/' && next == Some('*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        match ch {
            '\'' | '"' => {
                in_string = true;
                string_char = ch;
            }
            '(' | '[' | '{' => stack.push((ch, line_num)),
            ')' | ']' | '}' => {
                let expected = match ch {
                    ')' => '(',
                    ']' => '[',
                    '}' => '{',
                    _ => ' ',
                };
                if let Some((open, _)) = stack.last() {
                    if *open == expected {
                        stack.pop();
                    } else {
                        results.push(CheckResult {
                            rule_id: "S03".into(),
                            rule_name: "括号不匹配".into(),
                            severity: Severity::Error,
                            category: RuleCategory::Syntax,
                            location: loc(line_num, lines, None),
                            message: format!("括号 '{}' 与前面的 '{}' 不匹配", ch, open),
                            suggestion: Some("请检查括号是否正确配对".into()),
                        });
                    }
                } else {
                    results.push(CheckResult {
                        rule_id: "S03".into(),
                        rule_name: "括号不匹配".into(),
                        severity: Severity::Error,
                        category: RuleCategory::Syntax,
                        location: loc(line_num, lines, None),
                        message: format!("多余的闭括号 '{}'", ch),
                        suggestion: Some("请删除多余的括号".into()),
                    });
                }
            }
            _ => {}
        }
        if ch == '\n' {
            line_num += 1;
        }
        i += 1;
    }
    for (ch, open_line) in stack {
        results.push(CheckResult {
            rule_id: "S03".into(),
            rule_name: "括号不匹配".into(),
            severity: Severity::Error,
            category: RuleCategory::Syntax,
            location: loc(open_line, lines, None),
            message: format!("未闭合的括号 '{}'", ch),
            suggestion: Some("请添加对应的闭括号".into()),
        });
    }
    results
}

fn check_quotes(sql: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut single_count = 0;
    let mut double_count = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_string = false;
    let mut string_char = ' ';
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if ch == '*' && next == Some('/') {
                in_block_comment = false;
                i += 1;
            }
            i += 1;
            continue;
        }
        if in_string {
            if ch == string_char {
                if next == Some(string_char) {
                    i += 2;
                    continue;
                }
                in_string = false;
                if string_char == '\'' {
                    single_count += 1;
                } else {
                    double_count += 1;
                }
            }
            i += 1;
            continue;
        }
        if ch == '-' && next == Some('-') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if ch == '/' && next == Some('*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        match ch {
            '\'' => {
                let is_q = i >= 1
                    && (chars[i - 1] == 'q' || chars[i - 1] == 'Q')
                    && (i == 1 || (!chars[i - 2].is_ascii_alphanumeric() && chars[i - 2] != '_'));
                let closing = if is_q {
                    match chars.get(i + 1).copied() {
                        Some('[') => Some(']'),
                        Some('{') => Some('}'),
                        Some('(') => Some(')'),
                        Some('<') => Some('>'),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(cl) = closing {
                    let mut j = i + 2;
                    let mut matched = false;
                    while j < n {
                        if chars[j] == cl && chars.get(j + 1) == Some(&'\'') {
                            matched = true;
                            break;
                        }
                        j += 1;
                    }
                    if matched {
                        i = j + 2;
                        continue;
                    }
                }
                single_count += 1;
                in_string = true;
                string_char = '\'';
            }
            '"' => {
                double_count += 1;
                in_string = true;
                string_char = '"';
            }
            _ => {}
        }
        i += 1;
    }
    if single_count % 2 != 0 {
        results.push(CheckResult {
            rule_id: "S04".into(),
            rule_name: "引号不闭合".into(),
            severity: Severity::Error,
            category: RuleCategory::Syntax,
            location: loc(1, lines, None),
            message: "单引号数量不匹配".into(),
            suggestion: Some("请检查字符串是否正确闭合".into()),
        });
    }
    if double_count % 2 != 0 {
        results.push(CheckResult {
            rule_id: "S04".into(),
            rule_name: "引号不闭合".into(),
            severity: Severity::Error,
            category: RuleCategory::Syntax,
            location: loc(1, lines, None),
            message: "双引号数量不匹配".into(),
            suggestion: Some("请检查标识符是否正确闭合".into()),
        });
    }
    results
}

fn check_transaction(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut has_dml = false;
    let mut begin_count = 0usize;
    let mut end_count = 0usize;
    for (_, _, stmt) in parsed {
        match stmt {
            Statement::Insert(_)
            | Statement::Update { .. }
            | Statement::Delete(_)
            | Statement::Truncate { .. } => has_dml = true,
            Statement::StartTransaction { .. } => begin_count += 1,
            Statement::Commit { .. } | Statement::Rollback { .. } => end_count += 1,
            _ => {}
        }
    }
    if begin_count > 0 && end_count == 0 {
        results.push(CheckResult {
            rule_id: "T01".into(),
            rule_name: "事务未提交".into(),
            severity: Severity::Warning,
            category: RuleCategory::Transaction,
            location: loc(1, lines, None),
            message: "脚本显式开启了事务（BEGIN/START TRANSACTION）但未提交或回滚".into(),
            suggestion: Some("请在脚本末尾添加 COMMIT 或 ROLLBACK".into()),
        });
    } else if has_dml && end_count == 0 {
        results.push(CheckResult {
            rule_id: "T01".into(),
            rule_name: "缺少COMMIT".into(),
            severity: Severity::Info,
            category: RuleCategory::Transaction,
            location: loc(1, lines, None),
            message: "脚本包含DML语句但未发现COMMIT，请确认由执行环境统一提交".into(),
            suggestion: Some("若由执行工具统一提交可忽略此提示".into()),
        });
    }
    if begin_count > 0 && begin_count != end_count {
        results.push(CheckResult {
            rule_id: "T02".into(),
            rule_name: "事务不匹配".into(),
            severity: Severity::Warning,
            category: RuleCategory::Transaction,
            location: loc(1, lines, None),
            message: format!("BEGIN/COMMIT数量不匹配: BEGIN={}, COMMIT/ROLLBACK={}", begin_count, end_count),
            suggestion: Some("请确保每个事务都有对应的提交或回滚".into()),
        });
    }
    if begin_count > 1 {
        results.push(CheckResult {
            rule_id: "T04".into(),
            rule_name: "嵌套事务".into(),
            severity: Severity::Info,
            category: RuleCategory::Transaction,
            location: loc(1, lines, None),
            message: "检测到多个事务，部分数据库不支持嵌套事务".into(),
            suggestion: Some("请确认目标数据库是否支持嵌套事务".into()),
        });
    }
    results
}

fn check_unconditional_delete(parsed: &[ParsedStmt], lines: &[&str], config: &CheckConfig) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if config.allow_unconditional_delete {
        return results;
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Delete(delete) = stmt {
            if delete.selection.is_none() {
                results.push(CheckResult {
                    rule_id: "D01".into(),
                    rule_name: "无条件DELETE".into(),
                    severity: Severity::Error,
                    category: RuleCategory::Security,
                    location: loc(*line, lines, Some(idx)),
                    message: "DELETE语句缺少WHERE条件".into(),
                    suggestion: Some("请添加WHERE条件".into()),
                });
            } else if let Some(sel) = &delete.selection {
                if is_always_true_condition(sel) {
                    results.push(CheckResult {
                        rule_id: "D03".into(),
                        rule_name: "空条件DELETE".into(),
                        severity: Severity::Error,
                        category: RuleCategory::Security,
                        location: loc(*line, lines, Some(idx)),
                        message: "DELETE语句WHERE条件永真(如1=1)".into(),
                        suggestion: Some("请修改WHERE条件".into()),
                    });
                }
            }
        }
    }
    results
}

fn check_unconditional_update(parsed: &[ParsedStmt], lines: &[&str], config: &CheckConfig) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if config.allow_unconditional_delete {
        return results;
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Update { selection, .. } = stmt {
            if selection.is_none() {
                results.push(CheckResult {
                    rule_id: "D02".into(),
                    rule_name: "无条件UPDATE".into(),
                    severity: Severity::Error,
                    category: RuleCategory::Security,
                    location: loc(*line, lines, Some(idx)),
                    message: "UPDATE语句缺少WHERE条件".into(),
                    suggestion: Some("请添加WHERE条件".into()),
                });
            }
        }
    }
    results
}

fn check_truncate_drop(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        match stmt {
            Statement::Truncate { .. } => {
                results.push(CheckResult {
                    rule_id: "D06".into(),
                    rule_name: "TRUNCATE操作".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Security,
                    location: loc(*line, lines, Some(idx)),
                    message: "TRUNCATE操作会清空整表数据".into(),
                    suggestion: Some("请确保已备份数据".into()),
                });
            }
            Statement::Drop { object_type, .. } => {
                if *object_type == ObjectType::Table {
                    results.push(CheckResult {
                        rule_id: "D07".into(),
                        rule_name: "DROP操作".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Security,
                        location: loc(*line, lines, Some(idx)),
                        message: "DROP TABLE操作会删除表结构和数据".into(),
                        suggestion: Some("请确保已备份数据".into()),
                    });
                }
            }
            _ => {}
        }
    }
    results
}

fn check_batch_thresholds(parsed: &[ParsedStmt], lines: &[&str], config: &CheckConfig) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !supports_limit(&config.db_type) {
        return results;
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Delete(delete) = stmt {
            if delete.limit.is_none() {
                results.push(CheckResult {
                    rule_id: "D04".into(),
                    rule_name: "批量删除阈值".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Security,
                    location: loc(*line, lines, Some(idx)),
                    message: format!("DELETE没有LIMIT限制，可能超过{}条", config.max_delete_rows),
                    suggestion: Some("建议添加LIMIT子句".into()),
                });
            }
        }
    }
    results
}

fn check_select_star(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Query(query) = stmt {
            if has_select_star(query) {
                results.push(CheckResult {
                    rule_id: "P01".into(),
                    rule_name: "SELECT *使用".into(),
                    severity: Severity::Info,
                    category: RuleCategory::Performance,
                    location: loc(*line, lines, Some(idx)),
                    message: "使用SELECT *可能影响性能".into(),
                    suggestion: Some("建议明确指定字段".into()),
                });
            }
        }
    }
    results
}

fn check_select_without_where(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Query(query) = stmt {
            if let SetExpr::Select(select) = &*query.body {
                let only_dual = !select.from.is_empty()
                    && select.from.iter().all(|t| {
                        if let sqlparser::ast::TableFactor::Table { name, .. } = &t.relation {
                            name.to_string().to_lowercase() == "dual"
                        } else {
                            false
                        }
                    });
                if select.selection.is_none() && !only_dual {
                    results.push(CheckResult {
                        rule_id: "P02".into(),
                        rule_name: "缺少WHERE条件".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Performance,
                        location: loc(*line, lines, Some(idx)),
                        message: "SELECT语句缺少WHERE条件".into(),
                        suggestion: Some("请添加WHERE条件".into()),
                    });
                }
            }
        }
    }
    results
}

fn expr_has_like_leading(e: &sqlparser::ast::Expr) -> bool {
    use sqlparser::ast::{Expr, Value};
    match e {
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            let pat = if let Expr::Value(Value::SingleQuotedString(s)) = pattern.as_ref() {
                s.starts_with('%')
            } else {
                false
            };
            pat || expr_has_like_leading(expr) || expr_has_like_leading(pattern)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_like_leading(left) || expr_has_like_leading(right)
        }
        Expr::Nested(e) => expr_has_like_leading(e),
        Expr::InList { expr, list, .. } => {
            expr_has_like_leading(expr) || list.iter().any(expr_has_like_leading)
        }
        Expr::Between { expr, low, high, .. } => {
            expr_has_like_leading(expr) || expr_has_like_leading(low) || expr_has_like_leading(high)
        }
        Expr::IsNull(e) => expr_has_like_leading(e),
        Expr::UnaryOp { expr, .. } => expr_has_like_leading(expr),
        _ => false,
    }
}

fn query_has_like_leading(q: &Query) -> bool {
    if let SetExpr::Select(s) = &*q.body {
        if let Some(sel) = &s.selection {
            if expr_has_like_leading(sel) {
                return true;
            }
        }
        if let Some(h) = &s.having {
            if expr_has_like_leading(h) {
                return true;
            }
        }
    }
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            if query_has_like_leading(&cte.query) {
                return true;
            }
        }
    }
    false
}

fn check_like_leading_wildcard(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        let hit = match stmt {
            Statement::Query(q) => query_has_like_leading(q),
            Statement::Update { selection, .. } => {
                selection.as_ref().map(expr_has_like_leading).unwrap_or(false)
            }
            Statement::Delete(d) => d.selection.as_ref().map(expr_has_like_leading).unwrap_or(false),
            _ => false,
        };
        if hit {
            results.push(CheckResult {
                rule_id: "P03".into(),
                rule_name: "WHERE条件不用索引".into(),
                severity: Severity::Info,
                category: RuleCategory::Performance,
                location: loc(*line, lines, Some(idx)),
                message: "LIKE条件以%开头，可能无法使用索引".into(),
                suggestion: Some("避免使用前导通配符".into()),
            });
        }
    }
    results
}

fn check_ddl_autocommit(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut has_begin = false;
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        match stmt {
            Statement::StartTransaction { .. } => has_begin = true,
            Statement::CreateTable(_) | Statement::AlterTable { .. } | Statement::Drop { .. } => {
                if has_begin {
                    results.push(CheckResult {
                        rule_id: "T03".into(),
                        rule_name: "DDL自动提交".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Transaction,
                        location: loc(*line, lines, Some(idx)),
                        message: "DDL语句会自动提交事务".into(),
                        suggestion: Some("DDL会隐式提交事务，请注意".into()),
                    });
                }
            }
            _ => {}
        }
    }
    results
}

fn check_subquery(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Query(query) = stmt {
            if let SetExpr::Select(select) = &*query.body {
                if let Some(where_clause) = &select.selection {
                    if has_subquery(where_clause) {
                        results.push(CheckResult {
                            rule_id: "P05".into(),
                            rule_name: "子查询性能".into(),
                            severity: Severity::Info,
                            category: RuleCategory::Performance,
                            location: loc(*line, lines, Some(idx)),
                            message: "检测到子查询，可优化为JOIN".into(),
                            suggestion: Some("考虑将子查询改写为JOIN".into()),
                        });
                    }
                }
            }
        }
    }
    results
}

fn check_idempotency(parsed: &[ParsedStmt], lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !supports_if_not_exists(db_type) {
        return results;
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::CreateTable(_) = stmt {
            let upper = stmt.to_string().to_uppercase();
            if !upper.contains("IF NOT EXISTS") {
                results.push(CheckResult {
                    rule_id: "I01".into(),
                    rule_name: "可重复执行性".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Idempotency,
                    location: loc(*line, lines, Some(idx)),
                    message: "CREATE TABLE缺少IF NOT EXISTS".into(),
                    suggestion: Some("建议添加IF NOT EXISTS子句".into()),
                });
            }
        }
        if let Statement::CreateIndex(_) = stmt {
            let upper = stmt.to_string().to_uppercase();
            if !upper.contains("IF NOT EXISTS") {
                results.push(CheckResult {
                    rule_id: "I01".into(),
                    rule_name: "可重复执行性".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Idempotency,
                    location: loc(*line, lines, Some(idx)),
                    message: "CREATE INDEX缺少IF NOT EXISTS".into(),
                    suggestion: Some("建议添加IF NOT EXISTS子句".into()),
                });
            }
        }
    }
    results
}

fn check_naming_convention(parsed: &[ParsedStmt], lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if is_oracle_like(db_type) {
        return results;
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::CreateTable(ct) = stmt {
            if let Some(ident) = ct.name.0.last() {
                let table_name = &ident.value;
                if table_name.chars().any(|c| c.is_uppercase()) && table_name.contains('_') {
                    results.push(CheckResult {
                        rule_id: "N01".into(),
                        rule_name: "命名不规范".into(),
                        severity: Severity::Info,
                        category: RuleCategory::Convention,
                        location: loc(*line, lines, Some(idx)),
                        message: format!("表名 '{}' 建议使用小写+下划线", table_name),
                        suggestion: Some("建议使用 snake_case 命名".into()),
                    });
                }
            }
        }
    }
    results
}

fn check_error_handling(code_only: &str, lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    match db_type.to_lowercase().as_str() {
        d if is_oracle_like(d) => {
            if !upper.contains("WHENEVER SQLERROR") {
                results.push(CheckResult {
                    rule_id: "E01".into(),
                    rule_name: "缺少异常处理".into(),
                    severity: Severity::Info,
                    category: RuleCategory::ErrorHandling,
                    location: loc(1, lines, None),
                    message: "Oracle/达梦脚本未使用 WHENEVER SQLERROR".into(),
                    suggestion: Some("若由执行工具统一处理错误可忽略；脚本直跑建议添加 WHENEVER SQLERROR EXIT FAILURE".into()),
                });
            }
        }
        "sqlserver" | "mssql" => {
            if !upper.contains("TRY") && !upper.contains("CATCH") && (upper.contains("CREATE") || upper.contains("ALTER")) {
                results.push(CheckResult {
                    rule_id: "E02".into(),
                    rule_name: "缺少TRY...CATCH".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::ErrorHandling,
                    location: loc(1, lines, None),
                    message: "SQL Server脚本缺少TRY...CATCH".into(),
                    suggestion: Some("建议使用BEGIN TRY...END TRY".into()),
                });
            }
        }
        _ => {}
    }
    results
}

/// 收集 INSERT/UPDATE/DELETE 数据语句所覆盖的行号（用于排除业务数据误报）
fn data_statement_lines(parsed: &[ParsedStmt]) -> HashSet<usize> {
    let mut set = HashSet::new();
    for (line, text, stmt) in parsed {
        let is_data = matches!(
            stmt,
            Statement::Insert(_) | Statement::Update { .. } | Statement::Delete(_)
        );
        if is_data {
            let span = text.lines().count().max(1);
            for l in *line..*line + span {
                set.insert(l);
            }
        }
    }
    set
}

fn check_environment(code_only: &str, lines: &[&str], parsed: &[ParsedStmt]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let data_lines = data_statement_lines(parsed);
    let code_lines: Vec<&str> = code_only.lines().collect();

    // ENV01: 路径硬编码（排除数据语句中的业务值）
    for pattern in &["/HOME/", "/USR/", "/OPT/", "C:\\\\", "D:\\\\"] {
        for (i, l) in code_lines.iter().enumerate() {
            let ln = i + 1;
            if data_lines.contains(&ln) {
                continue;
            }
            if l.to_uppercase().contains(&pattern.to_uppercase()) {
                results.push(CheckResult {
                    rule_id: "ENV01".into(),
                    rule_name: "路径硬编码".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Environment,
                    location: loc(ln, lines, None),
                    message: "检测到硬编码的文件路径".into(),
                    suggestion: Some("建议使用相对路径或变量".into()),
                });
                break;
            }
        }
    }
    // ENV02: 主机名硬编码（排除数据语句）
    for pattern in &["@LOCALHOST", "@127.0.0.1", "@192.168.", "@10."] {
        for (i, l) in code_lines.iter().enumerate() {
            let ln = i + 1;
            if data_lines.contains(&ln) {
                continue;
            }
            if l.to_uppercase().contains(pattern) {
                results.push(CheckResult {
                    rule_id: "ENV02".into(),
                    rule_name: "主机名硬编码".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Environment,
                    location: loc(ln, lines, None),
                    message: "检测到硬编码的主机名".into(),
                    suggestion: Some("建议使用变量替代".into()),
                });
                break;
            }
        }
    }
    // ENV03: 密码明文
    let upper = code_only.to_uppercase();
    if upper.contains("IDENTIFIED BY") {
        for l in code_lines {
            if l.to_uppercase().contains("IDENTIFIED BY") {
                if let Some(after) = l.split("BY").nth(1) {
                    let pwd = after.trim().trim_end_matches(';');
                    if !pwd.is_empty() && !pwd.starts_with("PASSWORD") && !pwd.eq_ignore_ascii_case("NULL") {
                        results.push(CheckResult {
                            rule_id: "ENV03".into(),
                            rule_name: "密码明文".into(),
                            severity: Severity::Error,
                            category: RuleCategory::Environment,
                            location: loc(1, lines, None),
                            message: "检测到明文密码".into(),
                            suggestion: Some("禁止使用明文密码".into()),
                        });
                        break;
                    }
                }
            }
        }
    }
    results
}

fn check_keyword_case(code_lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let keywords = [
        "select", "from", "where", "insert", "update", "delete", "create", "alter", "drop",
        "and", "or", "not", "in", "values", "set", "into",
    ];
    let mut case_map: HashMap<String, (bool, bool)> = HashMap::new();
    for line in code_lines {
        for token in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let low = token.to_lowercase();
            if !keywords.contains(&low.as_str()) {
                continue;
            }
            let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
            let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
            let entry = case_map.entry(low).or_default();
            if has_upper {
                entry.0 = true;
            }
            if has_lower {
                entry.1 = true;
            }
        }
    }
    for (kw, (upper, lower)) in &case_map {
        if *upper && *lower {
            results.push(CheckResult {
                rule_id: "N03".into(),
                rule_name: "关键字大小写".into(),
                severity: Severity::Info,
                category: RuleCategory::Convention,
                location: loc(1, &[], None),
                message: format!("SQL关键字 '{}' 大小写使用不一致", kw),
                suggestion: Some("建议统一使用大写或小写".into()),
            });
            break;
        }
    }
    results
}

fn get_delete_table_name(delete: &sqlparser::ast::Delete) -> String {
    match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(tables) | sqlparser::ast::FromTable::WithoutKeyword(tables) => {
            tables.first().map(|t| t.relation.to_string().to_lowercase()).unwrap_or_default()
        }
    }
}

fn check_reserved_words(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let reserved: HashSet<&str> = [
        "select", "from", "where", "insert", "update", "delete", "create", "alter", "drop",
        "table", "index", "view", "grant", "revoke", "commit", "rollback", "begin", "end",
        "if", "else", "while", "loop", "cursor", "exception", "function", "procedure",
        "package", "trigger", "sequence", "synonym", "cluster", "database", "user", "role",
        "tablespace", "profile", "resource", "connect", "dba", "admin", "system", "sys",
        "public", "all", "any", "none", "null", "true", "false", "and", "or", "not", "in",
        "between", "like", "exists", "distinct", "as", "case", "when", "then", "else",
        "decode", "rownum", "rowid", "sysdate", "uid", "userenv", "dual",
    ]
    .into_iter()
    .collect();
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::CreateTable(ct) = stmt {
            if let Some(ident) = ct.name.0.last() {
                let name = ident.value.to_lowercase();
                if reserved.contains(name.as_str()) {
                    results.push(CheckResult {
                        rule_id: "S06".into(),
                        rule_name: "保留字使用".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Syntax,
                        location: loc(*line, lines, Some(idx)),
                        message: format!("表名 '{}' 是SQL保留字", ident.value),
                        suggestion: Some("建议使用双引号括起来或更换表名".into()),
                    });
                }
            }
        }
    }
    results
}

fn check_duplicate_insert(parsed: &[ParsedStmt], lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !supports_if_not_exists(db_type) {
        return results;
    }
    let mut deleted_before: HashSet<String> = HashSet::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            let upper = insert.to_string().to_uppercase();
            let has_conflict = upper.contains("ON CONFLICT")
                || upper.contains("ON DUPLICATE KEY")
                || upper.contains("REPLACE INTO")
                || upper.contains("MERGE INTO");
            let table = insert.table_name.to_string().to_lowercase();
            if !has_conflict && !deleted_before.contains(&table) {
                results.push(CheckResult {
                    rule_id: "I02".into(),
                    rule_name: "数据重复插入".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Idempotency,
                    location: loc(*line, lines, Some(idx)),
                    message: "INSERT语句缺少冲突处理机制，且脚本中未先DELETE该表".into(),
                    suggestion: Some("建议添加ON CONFLICT/ON DUPLICATE KEY处理，或先DELETE后INSERT".into()),
                });
            }
        }
        if let Statement::Delete(d) = stmt {
            deleted_before.insert(get_delete_table_name(d));
        }
    }
    results
}

fn check_unique_constraint(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut seen: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let mut deletes: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            if let Some(source) = &insert.source {
                if let SetExpr::Values(values) = &*source.body {
                    let cols: Vec<String> = insert.columns.iter().map(|c| c.to_string().to_lowercase()).collect();
                    let key = format!("{}|{}", cols.join(","), values);
                    let table = insert.table_name.to_string().to_lowercase();
                    let dup_idx = seen
                        .entry(table.clone())
                        .or_default()
                        .iter()
                        .rev()
                        .find(|(_, k)| k == &key)
                        .map(|(i, _)| *i);
                    if let Some(prev_idx) = dup_idx {
                        let has_delete_between = deletes
                            .entry(table.clone())
                            .or_default()
                            .iter()
                            .any(|&d| d > prev_idx && d < idx);
                        if !has_delete_between {
                            results.push(CheckResult {
                                rule_id: "IN02".into(),
                                rule_name: "唯一约束冲突".into(),
                                severity: Severity::Warning,
                                category: RuleCategory::Integrity,
                                location: loc(*line, lines, Some(idx)),
                                message: format!("表 '{}' 存在完全相同的重复INSERT", table),
                                suggestion: Some("请确认重复插入的数据是否有意为之".into()),
                            });
                        }
                    }
                    seen.entry(table).or_default().push((idx, key));
                }
            }
        }
        if let Statement::Delete(d) = stmt {
            deletes
                .entry(get_delete_table_name(d))
                .or_default()
                .push(idx);
        }
    }
    results
}

fn check_not_null(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            if let Some(source) = &insert.source {
                if let SetExpr::Values(values) = &*source.body {
                    for row in &values.rows {
                        for expr in row {
                            if let sqlparser::ast::Expr::Value(Value::Null) = expr {
                                results.push(CheckResult {
                                    rule_id: "IN03".into(),
                                    rule_name: "NOT NULL冲突".into(),
                                    severity: Severity::Info,
                                    category: RuleCategory::Integrity,
                                    location: loc(*line, lines, Some(idx)),
                                    message: "INSERT包含NULL值".into(),
                                    suggestion: Some("请确认对应字段允许NULL；如不允许请补充默认值".into()),
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    results
}

fn check_type_mismatch(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            if insert.columns.is_empty() {
                continue;
            }
            let col_count = insert.columns.len();
            if let Some(source) = &insert.source {
                if let SetExpr::Values(values) = &*source.body {
                    for row in &values.rows {
                        if row.len() != col_count {
                            results.push(CheckResult {
                                rule_id: "IN04".into(),
                                rule_name: "数据类型不匹配".into(),
                                severity: Severity::Error,
                                category: RuleCategory::Integrity,
                                location: loc(*line, lines, Some(idx)),
                                message: format!("列数不匹配：期望{}列，实际{}列", col_count, row.len()),
                                suggestion: Some("请检查INSERT语句的列数和值数是否一致".into()),
                            });
                            break;
                        }
                    }
                }
            }
        }
    }
    results
}

fn check_data_length(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            if let Some(source) = &insert.source {
                if let SetExpr::Values(values) = &*source.body {
                    for row in &values.rows {
                        for expr in row {
                            if let sqlparser::ast::Expr::Value(Value::SingleQuotedString(s)) = expr {
                                if s.len() > 4000 {
                                    results.push(CheckResult {
                                        rule_id: "IN05".into(),
                                        rule_name: "数据长度超限".into(),
                                        severity: Severity::Warning,
                                        category: RuleCategory::Integrity,
                                        location: loc(*line, lines, Some(idx)),
                                        message: format!("字符串长度{}超过4000字符", s.len()),
                                        suggestion: Some("请确认数据长度不超过字段定义".into()),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    results
}

/// 校验 YYYY-MM-DD / YYYY/MM/DD 是否为合法日期
fn valid_date_parts(s: &str) -> bool {
    if s.len() < 8 || s.len() > 10 {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '/') {
        return false;
    }
    let sep = if s.contains('-') { '-' } else { '/' };
    let parts: Vec<&str> = s.split(sep).collect();
    if parts.len() != 3 {
        return false;
    }
    let (y, m, d) = match (parts[0].parse::<u32>(), parts[1].parse::<u32>(), parts[2].parse::<u32>()) {
        (Ok(y), Ok(m), Ok(d)) => (y, m, d),
        _ => return false,
    };
    if m < 1 || m > 12 {
        return false;
    }
    let max_day = match m {
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    d >= 1 && d <= max_day
}

fn check_date_format(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        if let Statement::Insert(insert) = stmt {
            if let Some(source) = &insert.source {
                if let SetExpr::Values(values) = &*source.body {
                    for row in &values.rows {
                        for expr in row {
                            if let sqlparser::ast::Expr::Value(Value::SingleQuotedString(s)) = expr {
                                let dash_slash = s.chars().filter(|c| *c == '-' || *c == '/').count();
                                let looks_date = s.len() >= 8
                                    && s.len() <= 10
                                    && dash_slash >= 2
                                    && s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '/');
                                if looks_date && !valid_date_parts(s) {
                                    results.push(CheckResult {
                                        rule_id: "IN06".into(),
                                        rule_name: "日期格式错误".into(),
                                        severity: Severity::Warning,
                                        category: RuleCategory::Integrity,
                                        location: loc(*line, lines, Some(idx)),
                                        message: format!("日期 '{}' 格式无效", s),
                                        suggestion: Some("建议使用YYYY-MM-DD格式且为有效日期".into()),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    results
}

fn collect_refs(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Insert(i) => vec![i.table_name.to_string().to_lowercase()],
        Statement::Delete(d) => vec![get_delete_table_name(d)],
        Statement::Update { table, .. } => vec![table.relation.to_string().to_lowercase()],
        _ => vec![],
    }
}

fn check_object_dependency(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut created: HashMap<String, usize> = HashMap::new();
    for (line, _, stmt) in parsed.iter() {
        let name = match stmt {
            Statement::CreateTable(ct) => ct.name.0.last().map(|i| i.value.to_lowercase()),
            Statement::CreateIndex(ci) => ci.name.as_ref().map(|n| n.to_string().to_lowercase()),
            Statement::CreateView { name, .. } => name.0.last().map(|i| i.value.to_lowercase()),
            _ => None,
        };
        if let Some(n) = name {
            created.entry(n).or_insert(*line);
        }
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        for obj in collect_refs(stmt) {
            if let Some(&create_line) = created.get(&obj) {
                if *line < create_line {
                    results.push(CheckResult {
                        rule_id: "O01".into(),
                        rule_name: "对象依赖顺序".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Order,
                        location: loc(*line, lines, Some(idx)),
                        message: format!("对象 '{}' 在创建之前被使用", obj),
                        suggestion: Some("请调整语句顺序，先创建后使用".into()),
                    });
                }
            }
        }
    }
    results
}

fn temp_table_name(stmt: &Statement) -> Option<String> {
    match stmt {
        Statement::CreateTable(ct) => {
            let upper = stmt.to_string().to_uppercase();
            let name = ct.name.0.last().map(|i| i.value.to_lowercase())?;
            if upper.contains("TEMPORARY") || name.starts_with("tmp_") {
                Some(name)
            } else {
                None
            }
        }
        Statement::Drop { object_type, .. } => {
            if *object_type == ObjectType::Table {
                let name = stmt.to_string();
                let name = name
                    .split_whitespace()
                    .last()
                    .map(|s| s.trim_end_matches(';').to_lowercase());
                name.filter(|n| n.starts_with("tmp_"))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn collect_query_tables(stmt: &Statement) -> Vec<String> {
    let mut out = Vec::new();
    if let Statement::Query(query) = stmt {
        collect_tablefactor_tables(&query.body, &mut out);
    }
    out
}

fn collect_tablefactor_tables(set_expr: &sqlparser::ast::SetExpr, out: &mut Vec<String>) {
    match set_expr {
        SetExpr::Select(select) => {
            for item in &select.from {
                collect_relation(item.relation.clone(), out);
            }
        }
        _ => {}
    }
}

fn collect_relation(relation: sqlparser::ast::TableFactor, out: &mut Vec<String>) {
    match relation {
        sqlparser::ast::TableFactor::Table { name, .. } => {
            out.push(name.to_string().to_lowercase());
        }
        sqlparser::ast::TableFactor::NestedJoin { table_with_joins, .. } => {
            collect_relation(table_with_joins.relation.clone(), out);
        }
        _ => {}
    }
}

fn check_temp_table_order(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut created: HashMap<String, usize> = HashMap::new();
    for (line, _, stmt) in parsed.iter() {
        if let Some(name) = temp_table_name(stmt) {
            created.entry(name).or_insert(*line);
        }
    }
    for (idx, (line, _, stmt)) in parsed.iter().enumerate() {
        for table in collect_query_tables(stmt) {
            if table.starts_with("tmp_") {
                if let Some(&create_line) = created.get(&table) {
                    if *line < create_line {
                        results.push(CheckResult {
                            rule_id: "O02".into(),
                            rule_name: "临时表使用顺序".into(),
                            severity: Severity::Error,
                            category: RuleCategory::Order,
                            location: loc(*line, lines, Some(idx)),
                            message: format!("临时表 '{}' 在创建之前被使用", table),
                            suggestion: Some("请确保临时表先创建后使用".into()),
                        });
                    }
                }
            }
        }
    }
    results
}

fn check_unreplaced_placeholder(code_only: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    if upper.contains("%TA_CODE%") {
        let line = code_only
            .lines()
            .position(|l| l.to_uppercase().contains("%TA_CODE%"))
            .map(|l| l + 1)
            .unwrap_or(1);
        results.push(CheckResult {
            rule_id: "N05".into(),
            rule_name: "未替换的占位符".into(),
            severity: Severity::Warning,
            category: RuleCategory::Convention,
            location: loc(line, lines, None),
            message: "SQL中存在未替换的 %ta_code%，请注意替换".into(),
            suggestion: Some("请将 %ta_code% 替换为实际业务参数".into()),
        });
    }
    results
}

fn check_missing_comments(lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut stmt_count = 0;
    let mut comment_count = 0;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        if trimmed.to_uppercase().starts_with("CREATE")
            || trimmed.to_uppercase().starts_with("ALTER")
            || trimmed.to_uppercase().starts_with("DROP")
        {
            stmt_count += 1;
        }
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("--") && trimmed.len() > 3 {
            comment_count += 1;
        }
    }
    if stmt_count > 2 && comment_count == 0 {
        let line = lines
            .iter()
            .position(|l| !l.trim().is_empty() && !l.trim().starts_with("--"))
            .map(|l| l + 1)
            .unwrap_or(1);
        results.push(CheckResult {
            rule_id: "N02".into(),
            rule_name: "缺少注释".into(),
            severity: Severity::Info,
            category: RuleCategory::Convention,
            location: loc(line, lines, None),
            message: "SQL脚本缺少注释说明".into(),
            suggestion: Some("建议添加注释说明脚本用途".into()),
        });
    }
    results
}

fn check_indentation(lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut inconsistent = false;
    let mut uses_tabs = false;
    let mut uses_spaces = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            uses_tabs = true;
        } else if line.starts_with(' ') && line.chars().take_while(|&c| c == ' ').count() % 2 == 0 {
            uses_spaces = true;
        }
        if uses_tabs && uses_spaces {
            inconsistent = true;
            break;
        }
    }
    if inconsistent {
        results.push(CheckResult {
            rule_id: "N04".into(),
            rule_name: "缩进不规范".into(),
            severity: Severity::Info,
            category: RuleCategory::Convention,
            location: loc(1, lines, None),
            message: "SQL缩进风格不一致（混用Tab和空格）".into(),
            suggestion: Some("建议统一使用空格缩进".into()),
        });
    }
    results
}

fn check_error_continue(code_only: &str, lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    match db_type.to_lowercase().as_str() {
        d if is_oracle_like(d) => {
            if upper.contains("WHEN OTHERS") && upper.contains("CONTINUE") {
                let line = code_only
                    .lines()
                    .position(|l| l.to_uppercase().contains("WHEN OTHERS"))
                    .map(|l| l + 1)
                    .unwrap_or(1);
                results.push(CheckResult {
                    rule_id: "E03".into(),
                    rule_name: "错误后继续执行".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::ErrorHandling,
                    location: loc(line, lines, None),
                    message: "WHEN OTHERS后使用CONTINUE可能导致错误被忽略".into(),
                    suggestion: Some("建议记录错误日志后根据情况处理".into()),
                });
            }
        }
        "sqlserver" | "mssql" => {
            if upper.contains("CATCH") && upper.contains("CONTINUE") {
                let line = code_only
                    .lines()
                    .position(|l| l.to_uppercase().contains("CATCH"))
                    .map(|l| l + 1)
                    .unwrap_or(1);
                results.push(CheckResult {
                    rule_id: "E03".into(),
                    rule_name: "错误后继续执行".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::ErrorHandling,
                    location: loc(line, lines, None),
                    message: "CATCH块中使用CONTINUE可能导致错误被忽略".into(),
                    suggestion: Some("建议记录错误日志后根据情况处理".into()),
                });
            }
        }
        _ => {}
    }
    results
}

fn check_version_control(code_only: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    let has_version = upper.contains("VERSION") || upper.contains("-- V");
    if !has_version && (upper.contains("CREATE TABLE") || upper.contains("ALTER TABLE")) {
        let line = code_only
            .lines()
            .position(|l| l.to_uppercase().contains("CREATE TABLE") || l.to_uppercase().contains("ALTER TABLE"))
            .map(|l| l + 1)
            .unwrap_or(1);
        results.push(CheckResult {
            rule_id: "I03".into(),
            rule_name: "版本控制缺失".into(),
            severity: Severity::Info,
            category: RuleCategory::Idempotency,
            location: loc(line, lines, None),
            message: "DDL脚本缺少版本标记".into(),
            suggestion: Some("建议添加版本号注释便于追踪".into()),
        });
    }
    results
}

fn check_permission(code_only: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    let privileged_ops = [
        "GRANT",
        "REVOKE",
        "CREATE USER",
        "DROP USER",
        "ALTER USER",
        "CREATE ROLE",
        "DROP ROLE",
        "CREATE TABLESPACE",
        "DROP TABLESPACE",
    ];
    for op in &privileged_ops {
        if upper.contains(op) {
            let line = code_only
                .lines()
                .position(|l| l.to_uppercase().contains(op))
                .map(|l| l + 1)
                .unwrap_or(1);
            results.push(CheckResult {
                rule_id: "ENV04".into(),
                rule_name: "权限不足".into(),
                severity: Severity::Warning,
                category: RuleCategory::Environment,
                location: loc(line, lines, None),
                message: format!("操作 '{}' 需要DBA权限", op),
                suggestion: Some("请确认执行用户是否有足够权限".into()),
            });
            break;
        }
    }
    results
}

fn check_tablespace_compatibility(
    parsed: &[ParsedStmt],
    lines: &[&str],
    db_type: &str,
    allowed: &Option<Vec<String>>,
) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !is_oracle_like(db_type) {
        return results;
    }
    let allowed_list = match allowed {
        Some(v) if !v.is_empty() => v,
        _ => return results,
    };
    for (idx, (line, text, _)) in parsed.iter().enumerate() {
        let upper = text.to_uppercase();
        if upper.contains("TABLESPACE") {
            if let Some(ts) = upper.split("TABLESPACE").nth(1) {
                let ts_name = ts.trim().split_whitespace().next().unwrap_or("").trim_end_matches(';');
                let ts_name = ts_name.trim_matches('"').trim_matches('\'');
                if !ts_name.is_empty() && !allowed_list.iter().any(|a| a.eq_ignore_ascii_case(ts_name)) {
                    results.push(CheckResult {
                        rule_id: "B01".into(),
                        rule_name: "表空间不存在".into(),
                        severity: Severity::Error,
                        category: RuleCategory::Consistency,
                        location: loc(*line, lines, Some(idx)),
                        message: format!("表空间 '{}' 不在允许列表中", ts_name),
                        suggestion: Some("请确认表空间是否存在，或在配置中补充允许的表空间".into()),
                    });
                }
            }
        }
    }
    results
}

fn check_tablespace_specified(parsed: &[ParsedStmt], lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !is_oracle_like(db_type) {
        return results;
    }
    for (idx, (line, text, stmt)) in parsed.iter().enumerate() {
        if let Statement::CreateTable(_) = stmt {
            let upper = text.to_uppercase();
            if upper.contains(" AS SELECT") || upper.contains("AS SELECT") {
                continue;
            }
            if !upper.contains("TABLESPACE") {
                results.push(CheckResult {
                    rule_id: "B02".into(),
                    rule_name: "表空间未指定".into(),
                    severity: Severity::Info,
                    category: RuleCategory::Consistency,
                    location: loc(*line, lines, Some(idx)),
                    message: "CREATE TABLE未指定表空间，将使用默认表空间".into(),
                    suggestion: Some("建议显式指定TABLESPACE".into()),
                });
            }
        }
    }
    results
}

fn check_index_tablespace(parsed: &[ParsedStmt], lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    if !is_oracle_like(db_type) {
        return results;
    }
    for (idx, (line, text, stmt)) in parsed.iter().enumerate() {
        if let Statement::CreateIndex(_) = stmt {
            let upper = text.to_uppercase();
            if !upper.contains("TABLESPACE") {
                results.push(CheckResult {
                    rule_id: "B03".into(),
                    rule_name: "索引表空间".into(),
                    severity: Severity::Info,
                    category: RuleCategory::Consistency,
                    location: loc(*line, lines, Some(idx)),
                    message: "CREATE INDEX未指定TABLESPACE".into(),
                    suggestion: Some("建议为索引单独指定TABLESPACE".into()),
                });
            }
        }
    }
    results
}

fn check_partition_tablespace(code_only: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    if upper.contains("PARTITION") && upper.contains("TABLESPACE") {
        let mut ts_set: HashSet<String> = HashSet::new();
        for line in code_only.lines() {
            let upper_line = line.to_uppercase();
            if upper_line.contains("PARTITION") && upper_line.contains("TABLESPACE") {
                if let Some(ts) = upper_line.split("TABLESPACE").nth(1) {
                    let ts_name = ts.trim().split_whitespace().next().unwrap_or("").trim_end_matches(';').to_string();
                    if !ts_name.is_empty() {
                        ts_set.insert(ts_name);
                    }
                }
            }
        }
        if ts_set.len() > 1 {
            let line = code_only
                .lines()
                .position(|l| l.to_uppercase().contains("PARTITION"))
                .map(|l| l + 1)
                .unwrap_or(1);
            results.push(CheckResult {
                rule_id: "B04".into(),
                rule_name: "分区表空间".into(),
                severity: Severity::Warning,
                category: RuleCategory::Consistency,
                location: loc(line, lines, None),
                message: "分区使用了不同表空间".into(),
                suggestion: Some("请确认分区表空间配置".into()),
            });
        }
    }
    results
}

fn check_charset_compatibility(code_only: &str, lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    if upper.contains("CHARACTER SET") || upper.contains("CHARSET") {
        for line in code_only.lines() {
            let upper_line = line.to_uppercase();
            if upper_line.contains("CHARACTER SET") || upper_line.contains("CHARSET") {
                let charset = if upper_line.contains("CHARACTER SET") {
                    upper_line.split("CHARACTER SET").nth(1).unwrap_or("").trim().split_whitespace().next().unwrap_or("").trim_end_matches(';')
                } else {
                    upper_line.split("CHARSET=").nth(1).unwrap_or("").trim().split_whitespace().next().unwrap_or("").trim_end_matches(';')
                };
                if !charset.is_empty() && charset != "UTF8" && charset != "UTF-8" && charset != "AL32UTF8" && charset != "AL16UTF16" {
                    let line_num = code_only
                        .lines()
                        .position(|l| l.to_uppercase().contains("CHARACTER SET") || l.to_uppercase().contains("CHARSET"))
                        .map(|l| l + 1)
                        .unwrap_or(1);
                    results.push(CheckResult {
                        rule_id: "B05".into(),
                        rule_name: "字符集不兼容".into(),
                        severity: Severity::Error,
                        category: RuleCategory::Consistency,
                        location: loc(line_num, lines, None),
                        message: format!("字符集 '{}' 可能与数据库不一致", charset),
                        suggestion: Some("建议使用UTF-8字符集".into()),
                    });
                    break;
                }
            }
        }
    }
    results
}

fn check_version_syntax(code_only: &str, lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    match db_type.to_lowercase().as_str() {
        d if is_oracle_like(d) => {
            if upper.contains("FETCH FIRST") || upper.contains("FETCH NEXT") {
                let line = code_only
                    .lines()
                    .position(|l| l.to_uppercase().contains("FETCH FIRST") || l.to_uppercase().contains("FETCH NEXT"))
                    .map(|l| l + 1)
                    .unwrap_or(1);
                results.push(CheckResult {
                    rule_id: "B06".into(),
                    rule_name: "数据库版本语法".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Consistency,
                    location: loc(line, lines, None),
                    message: "FETCH FIRST语法需要Oracle 12c以上版本".into(),
                    suggestion: Some("请确认数据库版本是否支持此语法".into()),
                });
            }
        }
        "mysql" => {
            if upper.contains("JSON_") {
                let line = code_only
                    .lines()
                    .position(|l| l.to_uppercase().contains("JSON_"))
                    .map(|l| l + 1)
                    .unwrap_or(1);
                results.push(CheckResult {
                    rule_id: "B06".into(),
                    rule_name: "数据库版本语法".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Consistency,
                    location: loc(line, lines, None),
                    message: "JSON函数需要MySQL 5.7以上版本".into(),
                    suggestion: Some("请确认数据库版本是否支持此语法".into()),
                });
            }
        }
        _ => {}
    }
    results
}

fn check_variable_order(code_only: &str, lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = code_only.to_uppercase();
    match db_type.to_lowercase().as_str() {
        "sqlserver" | "mssql" => {
            let declare_pos = upper.find("DECLARE");
            let set_pos = upper.find("SET @");
            if let (Some(dp), Some(sp)) = (declare_pos, set_pos) {
                if sp < dp {
                    let line = code_only
                        .lines()
                        .position(|l| l.to_uppercase().contains("SET @"))
                        .map(|l| l + 1)
                        .unwrap_or(1);
                    results.push(CheckResult {
                        rule_id: "O03".into(),
                        rule_name: "变量定义顺序".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Order,
                        location: loc(line, lines, None),
                        message: "变量赋值SET发生在DECLARE之前".into(),
                        suggestion: Some("请先DECLARE变量再使用SET赋值".into()),
                    });
                }
            }
        }
        _ => {}
    }
    results
}

fn check_data_consistency(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut table_ops: HashMap<String, Vec<(usize, &str)>> = HashMap::new();
    for (line, _, stmt) in parsed {
        match stmt {
            Statement::Insert(insert) => {
                let name = insert.table_name.to_string();
                table_ops.entry(name).or_default().push((*line, "INSERT"));
            }
            Statement::Delete(delete) => {
                for t in &delete.tables {
                    let name = t.to_string();
                    table_ops.entry(name).or_default().push((*line, "DELETE"));
                }
            }
            _ => {}
        }
    }
    for (name, ops) in &table_ops {
        let inserts: Vec<&(usize, &str)> = ops.iter().filter(|(_, op)| *op == "INSERT").collect();
        let deletes: Vec<&(usize, &str)> = ops.iter().filter(|(_, op)| *op == "DELETE").collect();
        if !inserts.is_empty() && !deletes.is_empty() {
            let ins_idx = inserts[0].0;
            let del_idx = deletes[deletes.len() - 1].0;
            if ins_idx < del_idx {
                results.push(CheckResult {
                    rule_id: "C02".into(),
                    rule_name: "DELETE/INSERT顺序异常".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Consistency,
                    location: loc(ins_idx, lines, None),
                    message: format!("表 '{}' 先INSERT后DELETE", name),
                    suggestion: Some("建议先DELETE后INSERT".into()),
                });
            }
        }
        if deletes.len() > 1 && inserts.len() == 1 {
            let del_idx = deletes[0].0;
            results.push(CheckResult {
                rule_id: "C03".into(),
                rule_name: "多对多配对混乱".into(),
                severity: Severity::Warning,
                category: RuleCategory::Consistency,
                location: loc(del_idx, lines, None),
                message: format!("表 '{}' 有多个DELETE对应一个INSERT", name),
                suggestion: Some("请确认DELETE和INSERT的配对关系".into()),
            });
        }
    }
    results
}

fn check_delete_insert_condition(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut table_ops: HashMap<String, Vec<(usize, &Statement)>> = HashMap::new();
    for (line, _, stmt) in parsed {
        let table_name = match stmt {
            Statement::Delete(d) => Some(get_delete_table_name(d)),
            Statement::Insert(i) => Some(i.table_name.to_string().to_lowercase()),
            _ => None,
        };
        if let Some(name) = table_name {
            table_ops.entry(name).or_default().push((*line, stmt));
        }
    }
    for (name, ops) in &table_ops {
        let deletes: Vec<_> = ops.iter().filter(|(_, s)| matches!(s, Statement::Delete(_))).collect();
        let inserts: Vec<_> = ops.iter().filter(|(_, s)| matches!(s, Statement::Insert(_))).collect();
        if !deletes.is_empty() && !inserts.is_empty() {
            // 检查每个DELETE是否都有匹配的INSERT
            for (del_line, del_stmt) in &deletes {
                if let Statement::Delete(delete) = del_stmt {
                    // 提取DELETE WHERE的列名和值
                    let (del_cols, del_vals) = extract_delete_where_cols_vals(&delete.selection);
                    if del_cols.is_empty() {
                        // DELETE没有WHERE条件或无法提取，报警
                        results.push(CheckResult {
                            rule_id: "C01".into(),
                            rule_name: "DELETE/INSERT条件不一致".into(),
                            severity: Severity::Warning,
                            category: RuleCategory::Consistency,
                            location: loc(*del_line, lines, None),
                            message: format!("表 '{}' 的DELETE缺少WHERE条件", name),
                            suggestion: Some("请确保DELETE和INSERT的WHERE条件一致".into()),
                        });
                        continue;
                    }
                    // 查找匹配的INSERT（VALUES形式）
                    let has_matching_insert = inserts.iter().any(|(_, s)| {
                        if let Statement::Insert(insert) = s {
                            if !insert.columns.is_empty() {
                                if insert.source.is_none() {
                                    // source为None时是纯VALUES形式，但sqlparser可能将VALUES放在source中
                                }
                                return check_insert_matches_delete(insert, &del_cols, &del_vals);
                            }
                        }
                        false
                    });
                    if !has_matching_insert {
                        results.push(CheckResult {
                            rule_id: "C01".into(),
                            rule_name: "DELETE/INSERT条件不一致".into(),
                            severity: Severity::Warning,
                            category: RuleCategory::Consistency,
                            location: loc(*del_line, lines, None),
                            message: format!("表 '{}' 的DELETE条件与INSERT值不匹配", name),
                            suggestion: Some("请确保DELETE的WHERE条件对应INSERT的列值一致".into()),
                        });
                    }
                }
            }
        }
    }
    results
}

/// 从DELETE的WHERE条件中提取列名和值
fn extract_delete_where_cols_vals(selection: &Option<sqlparser::ast::Expr>) -> (Vec<String>, Vec<String>) {
    let mut cols = Vec::new();
    let mut vals = Vec::new();
    if let Some(expr) = selection {
        extract_eq_cols_vals(expr, &mut cols, &mut vals);
    }
    (cols, vals)
}

fn extract_eq_cols_vals(expr: &sqlparser::ast::Expr, cols: &mut Vec<String>, vals: &mut Vec<String>) {
    match expr {
        sqlparser::ast::Expr::BinaryOp { left, op, right } => {
            if let sqlparser::ast::BinaryOperator::Eq = op {
                match (left.as_ref(), right.as_ref()) {
                    (sqlparser::ast::Expr::Identifier(id), sqlparser::ast::Expr::Value(v)) => {
                        cols.push(id.value.to_lowercase());
                        vals.push(v.to_string().to_lowercase());
                    }
                    (sqlparser::ast::Expr::Value(v), sqlparser::ast::Expr::Identifier(id)) => {
                        cols.push(id.value.to_lowercase());
                        vals.push(v.to_string().to_lowercase());
                    }
                    _ => {}
                }
            } else if let sqlparser::ast::BinaryOperator::And = op {
                extract_eq_cols_vals(left, cols, vals);
                extract_eq_cols_vals(right, cols, vals);
            }
        }
        sqlparser::ast::Expr::InList { expr, list, .. } => {
            if let sqlparser::ast::Expr::Identifier(id) = expr.as_ref() {
                for item in list {
                    if let sqlparser::ast::Expr::Value(v) = item {
                        cols.push(id.value.to_lowercase());
                        vals.push(v.to_string().to_lowercase());
                    }
                }
            }
        }
        sqlparser::ast::Expr::Nested(e) => extract_eq_cols_vals(e, cols, vals),
        _ => {}
    }
}

/// 检查INSERT的列值是否匹配DELETE的条件
fn check_insert_matches_delete(
    insert: &sqlparser::ast::Insert,
    del_cols: &[String],
    del_vals: &[String],
) -> bool {
    let insert_cols = &insert.columns;
    if insert_cols.is_empty() {
        return false;
    }
    // 获取VALUES数据
    let values_rows = match &insert.source {
        Some(q) => {
            match &*q.body {
                sqlparser::ast::SetExpr::Values(v) => &v.rows,
                _ => return false,
            }
        }
        None => return false, // 没有source，无法检查
    };
    if values_rows.is_empty() {
        return false;
    }
    // 检查每个DELETE条件是否都能在INSERT中找到匹配
    for (del_col, del_val) in del_cols.iter().zip(del_vals.iter()) {
        // 找到DELETE的列在INSERT列中的位置
        if let Some(pos) = insert_cols.iter().position(|c| c.value.to_lowercase() == *del_col) {
            // 检查所有VALUES行中该位置的值
            let val_matched = values_rows.iter().any(|row| {
                if let Some(sqlparser::ast::Expr::Value(v)) = row.get(pos) {
                    let insert_val = v.to_string().to_lowercase();
                    let del_val_normalized = del_val.trim_matches('\'').trim_matches('"');
                    let insert_val_normalized = insert_val.trim_matches('\'').trim_matches('"');
                    del_val_normalized == insert_val_normalized
                } else {
                    false
                }
            });
            if !val_matched {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}



fn check_data_dependency(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut delete_tables: HashSet<String> = HashSet::new();
    for (line, _, stmt) in parsed {
        if let Statement::Delete(d) = stmt {
            delete_tables.insert(get_delete_table_name(d));
        }
        if let Statement::Insert(i) = stmt {
            let mut src_tables = Vec::new();
            if let Some(q) = &i.source {
                collect_tablefactor_tables(&q.body, &mut src_tables);
            }
            for src in src_tables {
                if delete_tables.contains(&src) {
                    results.push(CheckResult {
                        rule_id: "C04".into(),
                        rule_name: "数据依赖冲突".into(),
                        severity: Severity::Warning,
                        category: RuleCategory::Consistency,
                        location: loc(*line, lines, None),
                        message: format!("INSERT依赖表 '{}' 的DELETE后数据", src),
                        suggestion: Some("请确认DELETE和INSERT的数据依赖关系".into()),
                    });
                }
            }
        }
    }
    results
}

fn check_affected_rows(sql: &str, lines: &[&str], db_type: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let upper = sql.to_uppercase();
    let has_dml = upper.contains("INSERT") || upper.contains("UPDATE") || upper.contains("DELETE");
    let has_check = upper.contains("ROW_COUNT") || upper.contains("FOUND_ROWS") || upper.contains("@@ROWCOUNT") || upper.contains("SQL%ROWCOUNT");
    if has_dml && !has_check && db_type.to_lowercase() == "oracle" {
        let line = lines.iter().position(|l| l.to_uppercase().contains("INSERT") || l.to_uppercase().contains("UPDATE") || l.to_uppercase().contains("DELETE")).map(|l| l + 1).unwrap_or(1);
        results.push(CheckResult {
            rule_id: "E04".into(),
            rule_name: "影响行数未验证".into(),
            severity: Severity::Info,
            category: RuleCategory::ErrorHandling,
            location: loc(line, lines, None),
            message: "DML操作后未检查影响行数".into(),
            suggestion: Some("建议使用SQL%ROWCOUNT验证影响行数".into()),
        });
    }
    results
}

fn check_foreign_key(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let has_fk = parsed.iter().any(|(_, _, stmt)| {
        let s = stmt.to_string().to_uppercase();
        s.contains("REFERENCES") || s.contains("FOREIGN KEY")
    });
    if !has_fk {
        return results;
    }
    for (line, _, stmt) in parsed {
        if let Statement::Insert(insert) = stmt {
            let upper = insert.to_string().to_uppercase();
            if upper.contains("INSERT") && !upper.contains("IGNORE") && !upper.contains("ON CONFLICT") && !upper.contains("ON DUPLICATE KEY") {
                results.push(CheckResult {
                    rule_id: "IN01".into(),
                    rule_name: "外键约束冲突".into(),
                    severity: Severity::Warning,
                    category: RuleCategory::Integrity,
                    location: loc(*line, lines, None),
                    message: "INSERT可能违反外键约束".into(),
                    suggestion: Some("请确认插入的数据满足外键约束".into()),
                });
            }
        }
    }
    results
}

fn check_circular_dependency(parsed: &[ParsedStmt], lines: &[&str]) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let mut table_refs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut table_lines: HashMap<String, usize> = HashMap::new();
    for (line, _, stmt) in parsed {
        if let Statement::CreateTable(ct) = stmt {
            if let Some(ident) = ct.name.0.last() {
                let table_name = ident.value.to_lowercase();
                table_lines.entry(table_name.clone()).or_insert(*line);
                let mut refs = HashSet::new();
                let upper = ct.to_string().to_uppercase();
                if upper.contains("REFERENCES") {
                    for part in upper.split("REFERENCES") {
                        if let Some(ref_table) = part.trim().split_whitespace().next() {
                            let ref_name = ref_table.trim().trim_end_matches('(').to_lowercase();
                            if !ref_name.is_empty() && ref_name != table_name {
                                refs.insert(ref_name);
                            }
                        }
                    }
                }
                table_refs.insert(table_name, refs);
            }
        }
    }
    for (table, refs) in &table_refs {
        for ref_table in refs {
            if let Some(ref_refs) = table_refs.get(ref_table) {
                if ref_refs.contains(table) {
                    let line = table_lines.get(table).copied().unwrap_or(1);
                    results.push(CheckResult {
                        rule_id: "O04".into(),
                        rule_name: "循环依赖".into(),
                        severity: Severity::Error,
                        category: RuleCategory::Order,
                        location: loc(line, lines, None),
                        message: format!("表 '{}' 和 '{}' 存在循环依赖", table, ref_table),
                        suggestion: Some("请调整表结构消除循环依赖".into()),
                    });
                }
            }
        }
    }
    results
}

pub fn check_sql_text(sql: String, config: Option<CheckConfig>) -> Result<CheckReport, String> {
    Ok(check_sql(&sql, &config.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_sql_select_ok() {
        let report = check_sql("SELECT 1 FROM dual", &CheckConfig::default());
        assert_eq!(report.total_errors, 0, "{:?}", report.results);
    }

    #[test]
    fn check_sql_unconditional_delete_flagged() {
        let mut cfg = CheckConfig::default();
        cfg.db_type = "mysql".into();
        cfg.allow_unconditional_delete = false;
        let report = check_sql("DELETE FROM users", &cfg);
        assert!(
            report.total_errors + report.total_warnings > 0,
            "expected unconditional DELETE to be flagged: {:?}",
            report.results
        );
    }

    #[test]
    fn check_sql_syntax_error() {
        let report = check_sql("SELEC FROM nowhere", &CheckConfig::default());
        assert!(report.total_errors > 0);
        assert!(report.results.iter().any(|r| r.rule_id == "S01"));
    }
}
