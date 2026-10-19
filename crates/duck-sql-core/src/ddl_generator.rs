//! DDL 生成 / 分表 / UNION VIEW / 占位符替换。
//!
//! **本文件逐字复刻**自参考仓 `ifm_ai_tools/src-tauri/src/db/ddl_generator.rs`
//! （已在生产使用中验证）。移植原则见 `docs/ifm-plugin-port-plan.md` §6.4 与
//! 用户口径「复刻不复研」：只允许加这个模块头，**函数体一字不改**——
//! 改了就要重新验证行为，而这正是移植要避免的成本。

use regex::Regex;

/// Extract table name from a CREATE TABLE statement
pub fn extract_table_name(ddl: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?i)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:`|'|"|)(\w+)(?:`|'|"|)\s*\("#,
    )
    .ok()?;
    re.captures(ddl)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

/// Extract column names from a CREATE TABLE statement body.
/// Returns an empty vec if no columns can be parsed.
pub fn extract_columns(ddl: &str) -> Vec<String> {
    let open = match ddl.find('(') {
        Some(i) => i,
        None => return vec![],
    };
    let close = match ddl.rfind(')') {
        Some(i) if i > open => i,
        _ => return vec![],
    };
    let body = &ddl[open + 1..close];

    let mut columns = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let first = match line.split_whitespace().next() {
            Some(t) => t.trim_end_matches(','),
            None => continue,
        };
        let name = first.trim_matches('`').trim_matches('\'').trim_matches('"');
        if name.is_empty() {
            continue;
        }
        let upper = name.to_uppercase();
        if matches!(
            upper.as_str(),
            "PRIMARY"
                | "UNIQUE"
                | "KEY"
                | "CONSTRAINT"
                | "FOREIGN"
                | "CHECK"
                | "INDEX"
                | "FULLTEXT"
                | "SPATIAL"
        ) {
            continue;
        }
        columns.push(name.to_string());
    }
    columns
}

/// Generate N copies of a DDL with incrementing suffix (table, table1, table2, ...)
/// 若表名已以 _数字 结尾（如 tb_0），则替换末尾数字而非追加，避免 tb_0 -> tb_01
pub fn generate_sharded_ddls(original_ddl: &str, count: usize) -> Vec<String> {
    let table_name = match extract_table_name(original_ddl) {
        Some(name) => name,
        None => return vec![original_ddl.to_string()],
    };

    let mut results = Vec::with_capacity(count);
    for i in 1..=count {
        let new_name = sharded_table_name(&table_name, i);
        let new_ddl = original_ddl.replace(&table_name, &new_name);
        results.push(new_ddl);
    }
    results
}

// 参考仓原样保留 `if let` 里再套一层 `if`：合并成 let-chains 需要 Rust 1.88，
// 本仓 MSRV 是 1.85；而「复刻不复研」又不允许改写结构，于是就地豁免这一条。
#[allow(clippy::collapsible_if)]
fn sharded_table_name(table_name: &str, index: usize) -> String {
    if let Some(pos) = table_name.rfind('_') {
        if pos > 0 && table_name[pos + 1..].chars().all(|c| c.is_ascii_digit()) {
            let base = &table_name[..pos];
            return format!("{}_{}", base, index);
        }
    }
    format!("{}{}", table_name, index)
}

/// Generate sharded DDLs with TA code prefix (table + taCode + index)
pub fn generate_ta_sharded_ddls(original_ddl: &str, ta_code: &str, count: usize) -> Vec<String> {
    let table_name = match extract_table_name(original_ddl) {
        Some(name) => name,
        None => return vec![original_ddl.to_string()],
    };

    let base = format!("{}{}", table_name, ta_code);
    let mut results = Vec::with_capacity(count);
    for i in 1..=count {
        let new_name = sharded_table_name(&base, i);
        let new_ddl = original_ddl.replace(&table_name, &new_name);
        results.push(new_ddl);
    }
    results
}

/// Generate UNION ALL views for sharded tables
/// `with_seq` controls whether each SELECT includes a `table_seq` column
pub fn generate_union_views(original_ddl: &str, count: usize, with_seq: bool) -> Vec<String> {
    let table_name = match extract_table_name(original_ddl) {
        Some(name) => name,
        None => return vec![],
    };
    let columns = extract_columns(original_ddl);
    let col_list = if columns.is_empty() {
        "*".to_string()
    } else {
        columns.join(", ")
    };

    let mut selects: Vec<String> = Vec::with_capacity(count);
    for i in 1..=count {
        if with_seq {
            selects.push(format!(
                "SELECT {}, {} AS table_seq FROM {}{}",
                col_list, i, table_name, i
            ));
        } else {
            selects.push(format!("SELECT {} FROM {}{}", col_list, table_name, i));
        }
    }

    let union_query = selects.join("\nUNION ALL\n");
    let view_name = format!("view_{}", table_name);
    vec![format!("CREATE VIEW {} AS\n{};", view_name, union_query)]
}

/// Generate UNION ALL views with TA code
/// `with_seq` controls whether each SELECT includes a `table_seq` column
pub fn generate_ta_union_views(
    original_ddl: &str,
    ta_code: &str,
    count: usize,
    with_seq: bool,
) -> Vec<String> {
    let table_name = match extract_table_name(original_ddl) {
        Some(name) => name,
        None => return vec![],
    };
    let columns = extract_columns(original_ddl);
    let col_list = if columns.is_empty() {
        "*".to_string()
    } else {
        columns.join(", ")
    };

    let base = format!("{}{}", table_name, ta_code);
    let mut selects: Vec<String> = Vec::with_capacity(count);
    for i in 1..=count {
        if with_seq {
            selects.push(format!(
                "SELECT {}, {} AS table_seq FROM {}{}",
                col_list, i, base, i
            ));
        } else {
            selects.push(format!("SELECT {} FROM {}{}", col_list, base, i));
        }
    }

    let union_query = selects.join("\nUNION ALL\n");
    let view_name = format!("view_{}", base);
    vec![format!("CREATE VIEW {} AS\n{};", view_name, union_query)]
}

/// Replace `${num}` and `${ta_code}` / `${TACODE}` placeholders in SQL (大小写不敏感，支持有/无下划线)
pub fn replace_sql_placeholders(sql: &str, table_index: u32, ta_code: &str) -> String {
    replace_sql_placeholders_with_patterns(sql, table_index, ta_code, None, None)
}

/// 支持自定义占位符文本；若传入 None/空 则回退到大小写不敏感的默认匹配
pub fn replace_sql_placeholders_with_patterns(
    sql: &str,
    table_index: u32,
    ta_code: &str,
    num_placeholder: Option<&str>,
    ta_placeholder: Option<&str>,
) -> String {
    let mut result = sql.to_string();

    // 数值占位符：自定义文本优先，否则 (?i)${num}
    if let Some(pat) = num_placeholder.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t) }
    }) {
        result = result.replace(pat, &table_index.to_string());
    } else {
        let re_num = Regex::new(r"(?i)\$\{num\}").unwrap();
        result = re_num
            .replace_all(&result, table_index.to_string())
            .to_string();
    }

    // TA 占位符：自定义文本优先，否则 (?i)${ta_?code}
    if let Some(pat) = ta_placeholder.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t) }
    }) {
        result = result.replace(pat, ta_code);
    } else {
        let re_ta = Regex::new(r"(?i)\$\{ta_?code\}").unwrap();
        result = re_ta.replace_all(&result, ta_code).to_string();
    }

    result
}

/// Format DDL statements for display
pub fn format_ddls(ddls: &[String]) -> String {
    ddls.join("\n\n")
}

/// Count occurrences of a table name pattern
pub fn count_occurrences(sql: &str, table_name: &str) -> usize {
    sql.matches(table_name).count()
}
