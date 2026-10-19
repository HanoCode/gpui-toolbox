//! 个性化 SQL 改写（参考仓 `ifm_ai_tools/src-tauri/src/sql_rewriter.rs`，386 行，
//! **逐字复刻**）。
//!
//! 谁在调它：`duck-merge-core::see_merge` 的「个性化 SQL」阶段——解压出
//! `database/<dbType>/个性化脚本/<客户号>/` 后按客户号重写 `show_config` 系列语句
//! （参考仓也是 `crate::sql_rewriter::rewrite_sql_dir`）。
//!
//! ⚠️ 它**不是**「SQL 改写」那个界面的后端：参考仓 UI 的 `SqlReplace` / `SqlReExec`
//! 是两份纯前端算法，已移植进 `plugins/sql-rewrite`（JS 轨）。所以别指望这里能找到
//! 那两种模式。
//!
//! 依赖 `crate::sql_safety`（标识符校验）与 `crate::error::AppError`（两类拒绝理由），
//! 两者同批复刻。
//!
//! 复刻件的 clippy 偏差逐条列名：`unused_imports` / `dead_code` 覆盖参考仓留着但
//! 当前路径用不到的辅助函数与 `use`，保持逐行可 `diff`。
#![allow(
    clippy::let_and_return,
    clippy::redundant_pattern_matching,
    dead_code,
    unused_imports,
    unused_variables
)]

use std::path::Path;
use regex::Regex;
use crate::sql_safety::{validate_identifier, escape_identifier, SqlDialect};
use crate::error::AppError;

/// Validates and escapes SQL string literals to prevent injection.
fn escape_sql_string(input: &str) -> String {
    // Escape single quotes by doubling them (SQL standard)
    format!("'{}'", input.replace('\'', "''"))
}

/// Validates that a value is safe for SQL interpolation (basic alphanumeric check for simple values)
fn validate_sql_value(value: &str) -> Result<(), String> {
    // Allow alphanumeric, spaces, basic punctuation for values
    if value.chars().all(|c| c.is_alphanumeric() || c.is_whitespace() || ".,-_@:;/".contains(c)) {
        Ok(())
    } else {
        Err(format!("Invalid SQL value: '{}'", value))
    }
}

/// Java SqlParamParser.extractParameters:
/// Extract comma-separated parameters from a function call, respecting quotes and nested parens.
pub fn extract_params(input: &str) -> Vec<String> {
    let start = input.find('(');
    let end = input.rfind(')');
    let (s, e) = match (start, end) { (Some(s), Some(e)) if e > s => (s, e), _ => return vec![] };
    let content = &input[s + 1..e].trim();
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut quote_char = '\0';
    let mut last = 0usize;

    for (i, c) in content.char_indices() {
        if (c == '\'' || c == '"') && (!in_quote || c == quote_char) {
            in_quote = !in_quote;
            if in_quote { quote_char = c; }
        } else if !in_quote {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    result.push(content[last..i].trim().to_string());
                    last = i + 1;
                }
                _ => {}
            }
        }
    }
    if last < content.len() { result.push(content[last..].trim().to_string()); }
    result
}

/// Java replaceSqlParams: replace ${key} placeholders with values from the map
fn replace_params(template: &str, params: &[(&str, &str)]) -> String {
    let mut s = template.to_string();
    for (k, v) in params {
        s = s.replace(&format!("${{{}}}", k), v);
    }
    s
}

// Java F_SHOW_CONFIG_FIELD_REGEX: call f_show_config_field(...)
// → translates to INSERT / DELETE / UPDATE on OT_TSHOWCONFIGFIELD
fn rewrite_show_config_field(content: &str, re: &Regex, op_type: &str) -> String {
    let mut result = String::new();
    let mut last_end = 0usize;
    for cap in re.find_iter(content) {
        let start = cap.start();
        result.push_str(&content[last_end..start]);
        let call = cap.as_str();
        let params = extract_params(call);
        if params.len() < 3 { result.push_str(call); last_end = cap.end(); continue; }

        let opt = params[0].trim().trim_matches('\'');
        let vc_config_code = params.get(2).map(|s| s.trim()).unwrap_or("");
        let vc_name = params.get(3).map(|s| s.trim()).unwrap_or("");
        let vc_label = params.get(4).map(|s| s.trim()).unwrap_or("");
        let vc_type = params.get(5).map(|s| s.trim()).unwrap_or("");
        let vc_field_mode = params.get(6).map(|s| s.trim()).unwrap_or("");
        let vc_format = params.get(7).map(|s| s.trim()).unwrap_or("");
        let vc_sort = params.get(8).map(|s| s.trim()).unwrap_or("");
        let vc_width = params.get(9).map(|s| s.trim()).unwrap_or("");
        let vc_height = params.get(10).map(|s| s.trim()).unwrap_or("");
        let vc_category = params.get(11).map(|s| s.trim()).unwrap_or("");
        let vc_editable = params.get(12).map(|s| s.trim()).unwrap_or("");
        let vc_orderable = params.get(13).map(|s| s.trim()).unwrap_or("");
        let vc_visible = params.get(14).map(|s| s.trim()).unwrap_or("");
        let vc_fixable = params.get(15).map(|s| s.trim()).unwrap_or("");
        let vc_filter_type = params.get(16).map(|s| s.trim()).unwrap_or("");
        let vc_groupable = params.get(17).map(|s| s.trim()).unwrap_or("");
        let vc_value = params.get(18).map(|s| s.trim()).unwrap_or("");
        let vc_load_type = params.get(19).map(|s| s.trim()).unwrap_or("");
        let vc_default_value = params.get(20).map(|s| s.trim()).unwrap_or("");
        let vc_rules = params.get(21).map(|s| s.trim()).unwrap_or("");
        let vc_messages = params.get(22).map(|s| s.trim()).unwrap_or("");
        let vc_value_type = params.get(23).map(|s| s.trim().trim_end_matches(&[')', ';'][..])).unwrap_or("");

        let vc_group_code = params.get(1).map(|s| s.trim()).unwrap_or("");

        // Validate identifiers to prevent SQL injection
        if let Err(e) = validate_identifier(vc_group_code) {
            result.push_str(call); // Skip unsafe calls
            last_end = cap.end();
            continue;
        }
        if let Err(_) = validate_identifier(vc_config_code) {
            result.push_str(call);
            last_end = cap.end();
            continue;
        }
        if let Err(_) = validate_identifier(vc_name) {
            result.push_str(call);
            last_end = cap.end();
            continue;
        }

        match op_type {
            "change" => {
                let escaped_group_code = escape_sql_string(vc_group_code);
                let escaped_config_code = escape_sql_string(vc_config_code);
                let escaped_name = escape_sql_string(vc_name);
                let escaped_label = escape_sql_string(vc_label);
                let escaped_type = escape_sql_string(vc_type);
                let escaped_field_mode = escape_sql_string(vc_field_mode);
                let escaped_format = escape_sql_string(vc_format);
                let escaped_sort = escape_sql_string(vc_sort);
                let escaped_width = escape_sql_string(vc_width);
                let escaped_height = escape_sql_string(vc_height);
                let escaped_category = escape_sql_string(vc_category);
                let escaped_editable = escape_sql_string(vc_editable);
                let escaped_orderable = escape_sql_string(vc_orderable);
                let escaped_visible = escape_sql_string(vc_visible);
                let escaped_fixable = escape_sql_string(vc_fixable);
                let escaped_filter_type = escape_sql_string(vc_filter_type);
                let escaped_groupable = escape_sql_string(vc_groupable);
                let escaped_value = escape_sql_string(vc_value);
                let escaped_load_type = escape_sql_string(vc_load_type);
                let escaped_default_value = escape_sql_string(vc_default_value);
                let escaped_rules = escape_sql_string(vc_rules);
                let escaped_messages = escape_sql_string(vc_messages);
                let escaped_value_type = escape_sql_string(vc_value_type);

                let t_group_id = format!("(SELECT t.VC_SC_GROUP_ID FROM OT_TSHOWCONFIGGROUP t WHERE t.VC_SC_GROUP_CODE = {})", escaped_group_code);
                let t_config_id = format!("(SELECT t.VC_SC_ID FROM OT_TSHOWCONFIG t WHERE t.VC_SC_GROUP_ID = {} AND t.VC_SC_CODE = {})", t_group_id, escaped_config_code);
                if opt.ends_with('1') {
                    let sql = format!("INSERT INTO OT_TSHOWCONFIGFIELD VALUES(null,{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{});",
                        t_config_id, escaped_name, escaped_label, escaped_type, escaped_field_mode, escaped_format, escaped_sort, escaped_width, escaped_height,
                        escaped_category, escaped_editable, escaped_orderable, escaped_visible, escaped_fixable, escaped_filter_type, escaped_groupable,
                        escaped_value, escaped_load_type, escaped_default_value, escaped_rules, escaped_messages, escaped_value_type);
                    result.push_str(&sql);
                } else if opt.ends_with('2') {
                    let t_id = format!("(SELECT t.VC_SC_FIELD_ID FROM OT_TSHOWCONFIGFIELD t WHERE t.VC_SC_ID = ({}) AND t.VC_SC_FIELD_NAME = {})", t_config_id, escaped_name);
                    let sql = format!("DELETE FROM OT_TSHOWCONFIGFIELD WHERE VC_SC_FIELD_ID = (SELECT * FROM ({}) AS tmp);", t_id);
                    result.push_str(&sql);
                } else if opt.ends_with('3') {
                    let t_id = format!("(SELECT t.VC_SC_GROUP_ID FROM OT_TSHOWCONFIGGROUP t WHERE t.VC_SC_GROUP_CODE = {})", escaped_group_code);
                    let sql = format!("UPDATE OT_TSHOWCONFIGFIELD SET VC_SC_FIELD_LABEL={},VC_SC_FIELD_TYPE={},VC_SC_FIELD_MODE={},VC_SC_FIELD_FORMAT={},EN_SC_FIELD_SORT={},EN_SC_FIELD_WIDTH={},EN_SC_FIELD_HEIGHT={},VC_SC_FIELD_CATEGORY={},C_SC_FIELD_EDITABLE={},C_SC_FIELD_ORDERABLE={},C_SC_FIELD_VISIBLE={},C_SC_FIELD_FIXABLE={},C_SC_FIELD_FILTER_TYPE={},C_SC_FIELD_GROUPABLE={},VC_SC_FIELD_VALUE_FROM={},C_SC_FIELD_LOAD_TYPE={},VC_SC_FIELD_DEFAULT_VALUE={},VC_SC_FIELD_RULES={},C_SC_FIELD_MESSAGE={},C_SC_FIELD_VALUE_TYPE={} WHERE VC_SC_FIELD_ID = {} ;",
                        escaped_label, escaped_type, escaped_field_mode, escaped_format, escaped_sort, escaped_width, escaped_height, escaped_category,
                        escaped_editable, escaped_orderable, escaped_visible, escaped_fixable, escaped_filter_type, escaped_groupable,
                        escaped_value, escaped_load_type, escaped_default_value, escaped_rules, escaped_messages, escaped_value_type, t_id);
                    result.push_str(&sql);
                } else {
                    result.push_str(call);
                }
            }
            _ => { result.push_str(call); }
        }
        last_end = cap.end();
    }
    result.push_str(&content[last_end..]);
    result
}

/// Java F_SHOW_CONFIG_REGEX: call f_show_config(...) → INSERT/UPDATE/DELETE on OT_TSHOWCONFIG
fn rewrite_show_config(content: &str, re: &Regex) -> String {
    let mut result = String::new();
    let mut last_end = 0usize;
    for cap in re.find_iter(content) {
        let start = cap.start();
        result.push_str(&content[last_end..start]);
        let call = cap.as_str();
        let params = extract_params(call);
        if params.len() < 8 { result.push_str(call); last_end = cap.end(); continue; }

        let opt = params[0].trim().trim_matches('\'');
        let vc_group_code = params.get(1).map(|s| s.trim()).unwrap_or("");
        let vc_config_code = params.get(2).map(|s| s.trim()).unwrap_or("");
        let vc_name = params.get(3).map(|s| s.trim()).unwrap_or("");
        let vc_version = params.get(4).map(|s| s.trim()).unwrap_or("");
        let vc_services_type = params.get(5).map(|s| s.trim()).unwrap_or("");
        let vc_services = params.get(6).map(|s| s.trim()).unwrap_or("");
        let vc_type = params.get(7).map(|s| s.trim()).unwrap_or("");

        // Validate identifiers to prevent SQL injection
        if let Err(_) = validate_identifier(vc_group_code) {
            result.push_str(call);
            last_end = cap.end();
            continue;
        }
        if let Err(_) = validate_identifier(vc_config_code) {
            result.push_str(call);
            last_end = cap.end();
            continue;
        }

        let escaped_group_code = escape_sql_string(vc_group_code);
        let escaped_config_code = escape_sql_string(vc_config_code);
        let escaped_name = escape_sql_string(vc_name);
        let escaped_version = escape_sql_string(vc_version);
        let escaped_services_type = escape_sql_string(vc_services_type);
        let escaped_services = escape_sql_string(vc_services);
        let escaped_type = escape_sql_string(vc_type);

        let t_group_id = format!("(SELECT t.VC_SC_GROUP_ID FROM OT_TSHOWCONFIGGROUP t WHERE t.VC_SC_GROUP_CODE = {})", escaped_group_code);
        match opt {
            "1" => {
                let sql = format!("INSERT INTO OT_TSHOWCONFIG VALUES(null,{},{},{},{},{},{},{});", t_group_id, escaped_config_code, escaped_name, escaped_version, escaped_services_type, escaped_services, escaped_type);
                result.push_str(&sql);
            }
            "2" => {
                let t_config_id = format!("(SELECT t.VC_SC_ID FROM OT_TSHOWCONFIG t WHERE t.VC_SC_GROUP_ID = {} AND t.VC_SC_CODE = {});", t_group_id, escaped_config_code);
                let sql = format!("DELETE FROM OT_TSHOWCONFIG WHERE VC_SC_ID = {};\nDELETE FROM OT_TSHOWCONFIGFIELD WHERE VC_SC_ID = {};", t_config_id, t_config_id);
                result.push_str(&sql);
            }
            "3" => {
                let t_config_id = format!("(SELECT t.VC_SC_ID FROM OT_TSHOWCONFIG t WHERE t.VC_SC_GROUP_ID = {} AND t.VC_SC_CODE = {})", t_group_id, escaped_config_code);
                let sql = format!("UPDATE OT_TSHOWCONFIG t SET VC_SC_NAME={},VC_SC_VERSION={},C_SC_SERVICE_TYPE={},VC_SC_SERVICES={},VC_SC_TYPE={} WHERE t.VC_SC_ID={};",
                    escaped_name, escaped_version, escaped_services_type, escaped_services, escaped_type, t_config_id);
                result.push_str(&sql);
            }
            _ => { result.push_str(call); }
        }
        last_end = cap.end();
    }
    result.push_str(&content[last_end..]);
    result
}

/// Java F_SHOW_CONFIG_GROUP_REGEX: call f_show_config_group(...) → INSERT/UPDATE/DELETE on OT_TSHOWCONFIGGROUP
fn rewrite_show_config_group(content: &str, re: &Regex) -> String {
    let mut result = String::new();
    let mut last_end = 0usize;
    for cap in re.find_iter(content) {
        let start = cap.start();
        result.push_str(&content[last_end..start]);
        let call = cap.as_str();
        let params = extract_params(call);
        if params.len() < 2 { result.push_str(call); last_end = cap.end(); continue; }

        let opt = params[0].trim().trim_matches('\'');
        let vc_code = params.get(1).map(|s| s.trim()).unwrap_or("");
        let vc_name = params.get(2).map(|s| s.trim()).unwrap_or("");
        let vc_version = params.get(3).map(|s| s.trim()).unwrap_or("");
        let vc_tenant_id = "'10000'";

        match opt {
            "1" => {
                let sql = format!("INSERT INTO OT_TSHOWCONFIGGROUP VALUES(null,{},{},{},{});", vc_code, vc_name, vc_version, vc_tenant_id);
                result.push_str(&sql);
            }
            "2" => {
                let t_id = format!("(SELECT t.VC_SC_GROUP_ID FROM OT_TSHOWCONFIGGROUP t WHERE t.VC_SC_GROUP_CODE = {})", vc_code);
                let sql = format!("DELETE FROM OT_TSHOWCONFIGGROUP WHERE VC_SC_GROUP_ID={};\nDELETE FROM OT_TSHOWCONFIGFIELD WHERE VC_SC_ID IN (SELECT VC_SC_ID FROM OT_TSHOWCONFIG t WHERE t.VC_SC_GROUP_ID = {});\nDELETE FROM OT_TSHOWCONFIG WHERE VC_SC_GROUP_ID = {};", t_id, t_id, t_id);
                result.push_str(&sql);
            }
            "3" => {
                let t_id = format!("(SELECT t.VC_SC_GROUP_ID FROM OT_TSHOWCONFIGGROUP t WHERE t.VC_SC_GROUP_CODE = {})", vc_code);
                let sql = format!("UPDATE OT_TSHOWCONFIGGROUP SET VC_SC_GROUP_CODE={},VC_SC_GROUP_NAME={},VC_SC_GROUP_VERSION={},VC_TENANT_ID={} WHERE VC_SC_GROUP_ID={};", vc_code, vc_name, vc_version, vc_tenant_id, t_id);
                result.push_str(&sql);
            }
            _ => { result.push_str(call); }
        }
        last_end = cap.end();
    }
    result.push_str(&content[last_end..]);
    result
}

/// Java UPDATE_SHOW_CONFIG_FIELD_REGEX: call update_show_config_field(...) → UPDATE on OT_TSHOWCONFIGFIELD
fn rewrite_update_show_config_field(content: &str, re: &Regex) -> String {
    let mut result = String::new();
    let mut last_end = 0usize;
    for cap in re.find_iter(content) {
        let start = cap.start();
        result.push_str(&content[last_end..start]);
        let call = cap.as_str();
        let params = extract_params(call);
        if params.len() < 4 { result.push_str(call); last_end = cap.end(); continue; }
        let sc_code = params[0].trim();
        let fd_name = params[1].trim();
        let col_name = params[2].trim().trim_matches('\'').to_lowercase();
        let col_val = params[3].trim();
        let sql = format!("UPDATE ot_tshowconfigfield SET {}= {} WHERE vc_sc_field_name = {} AND vc_sc_id = (SELECT vc_sc_id FROM ot_tshowconfig WHERE vc_sc_code = {});", col_name, col_val, fd_name, sc_code);
        result.push_str(&sql);
        last_end = cap.end();
    }
    result.push_str(&content[last_end..]);
    result
}

/// Java DEL_SHOW_CONFIG_FIELD_REGEX: call del_show_config_field(...) → DELETE on OT_TSHOWCONFIGFIELD
fn rewrite_del_show_config_field(content: &str, re: &Regex) -> String {
    let mut result = String::new();
    let mut last_end = 0usize;
    for cap in re.find_iter(content) {
        let start = cap.start();
        result.push_str(&content[last_end..start]);
        let call = cap.as_str();
        let params = extract_params(call);
        if params.len() < 2 { result.push_str(call); last_end = cap.end(); continue; }
        let sc_code = params[0].trim();
        let fd_name = params[1].trim();
        let sql = format!("DELETE FROM ot_tshowconfigfield WHERE vc_sc_field_name = {} AND vc_sc_id = (SELECT vc_sc_id FROM ot_tshowconfig WHERE vc_sc_code = {});", fd_name, sc_code);
        result.push_str(&sql);
        last_end = cap.end();
    }
    result.push_str(&content[last_end..]);
    result
}

// Lazy static regexes
fn re_show_config_field() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)call\s+f_show_config_field\s*\([^;]*?;").expect("valid regex"))
}
fn re_update_show_config_field() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)call\s+update_show_config_field\s*\([^;]*?;").expect("valid regex"))
}
fn re_del_show_config_field() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)call\s+del_show_config_field\s*\([^;]*?;").expect("valid regex"))
}
fn re_show_config_group() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)call\s+f_show_config_group\s*\([^;]*?;").expect("valid regex"))
}
fn re_show_config() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)call\s+f_show_config\s*\([^;]*?;").expect("valid regex"))
}

/// Main entry: apply all SQL rewrites to a file's content (Java findSqlFiles equivalent)
pub fn rewrite_sql_file_content(content: &str) -> String {
    let s = rewrite_show_config_field(content, re_show_config_field(), "change");
    let s = rewrite_show_config(&s, re_show_config());
    let s = rewrite_show_config_group(&s, re_show_config_group());
    let s = rewrite_update_show_config_field(&s, re_update_show_config_field());
    let s = rewrite_del_show_config_field(&s, re_del_show_config_field());
    s
}

/// Apply SQL rewrites to a single file (in-place)
pub fn rewrite_sql_file(path: &Path) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("读取SQL文件失败 {}: {}", path.display(), e))?;
    let rewritten = rewrite_sql_file_content(&content);
    if rewritten != content {
        std::fs::write(path, &rewritten)
            .map_err(|e| format!("写入SQL文件失败 {}: {}", path.display(), e))?;
    }
    Ok(())
}

/// Find and rewrite all .sql files in a directory tree (Java findSqlFiles)
pub fn rewrite_sql_dir(dir: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    if !dir.exists() { return Ok(0); }
    fn walk(d: &Path, count: &mut usize) -> Result<(), String> {
        if let Ok(entries) = std::fs::read_dir(d) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() { walk(&p, count)?; }
                else if p.extension().map(|x| x == "sql").unwrap_or(false) {
                    rewrite_sql_file(&p)?;
                    *count += 1;
                }
            }
        }
        Ok(())
    }
    walk(dir, &mut count)?;
    Ok(count)
}
