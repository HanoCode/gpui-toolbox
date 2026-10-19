//! SQL 安全工具模块
//!
//! 提供防止 SQL 注入的工具函数，特别是针对标识符的转义和验证
//! 支持多种 SQL 方言：MySQL, PostgreSQL, SQLite, SQL Server
//!
//! 参考仓 `ifm_ai_tools/src-tauri/src/sql_safety.rs`（289 行，**逐字复刻**），
//! 目前由 `crate::sql_rewriter` 使用（个性化 SQL 改写前的标识符校验）。
//!
//! `unused_imports`：参考仓引入了 `std::fmt` 却全文用 `fmt::` 全限定路径，保持原样。
#![allow(unused_imports)]

use crate::error::AppError;
use std::fmt;

/// SQL 方言标识符，用于正确的转义
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    MySql,
    Postgres,
    Sqlite,
    SqlServer,
}

/// 验证标识符是否为安全的 SQL 标识符
///
/// 允许的字符：`[A-Za-z_][A-Za-z0-9_]*`
///
/// 返回 `Ok(())` 表示成功，如果名称包含非法字符、为空或以数字开头，
/// 则返回 `Err(AppError::InvalidIdentifier)`
pub fn validate_identifier(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::InvalidIdentifier("标识符不能为空".to_string()));
    }
    
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return Err(AppError::InvalidIdentifier("标识符不能为空".to_string())),
    };
    
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(AppError::InvalidIdentifier(
            format!("标识符必须以字母或下划线开头: '{}'", name)
        ));
    }
    
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(AppError::InvalidIdentifier(
                format!("标识符包含非法字符 '{}': '{}'", c, name)
            ));
        }
    }
    
    Ok(())
}

/// 如果标识符通过 [`validate_identifier`] 验证，则返回原标识符
/// 这是为需要返回已验证字符串的调用点提供的便捷包装器
pub fn checked_identifier(name: &str) -> Result<&str, AppError> {
    validate_identifier(name)?;
    Ok(name)
}

/// 转义已验证的 SQL 标识符以安全地插入到 SQL 字符串中
/// 此函数假设标识符已经通过 [`validate_identifier`] 验证
/// 
/// - MySQL/MariaDB: 使用反引号包裹并转义内部反引号
/// - PostgreSQL: 使用双引号包裹并转义内部双引号
/// - SQLite: 使用双引号包裹（或反引号以兼容）
/// - SQL Server: 使用方括号包裹并转义内部方括号
pub fn escape_identifier(name: &str, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::MySql => format!("`{}`", name.replace('`', "``")),
        SqlDialect::Postgres => format!("\"{}\"", name.replace('"', "\"\"")),
        SqlDialect::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        SqlDialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
    }
}

/// SQLite 专用的标识符转义（向后兼容）
pub fn escape_identifier_sqlite(identifier: &str) -> Result<String, AppError> {
    validate_identifier(identifier)?;
    Ok(escape_identifier(identifier, SqlDialect::Sqlite))
}

/// 验证并转义多个 SQL 标识符
pub fn escape_identifiers(identifiers: &[&str], dialect: SqlDialect) -> Result<Vec<String>, AppError> {
    identifiers.iter().map(|id| {
        validate_identifier(id)?;
        Ok(escape_identifier(id, dialect))
    }).collect()
}

/// 构建安全的 ORDER BY 子句
pub fn build_order_by(column: &str, direction: Option<&str>, dialect: SqlDialect) -> Result<String, AppError> {
    validate_identifier(column)?;
    let safe_column = escape_identifier(column, dialect);
    
    let safe_direction = match direction {
        Some(d) => {
            let upper = d.to_uppercase();
            match upper.as_str() {
                "DESC" => "DESC",
                "ASC" => "ASC",
                other => return Err(AppError::InvalidIdentifier(
                    format!("无效的排序方向: {}", other)
                )),
            }
        }
        None => "ASC",
    };

    Ok(format!("{} {}", safe_column, safe_direction))
}

/// 验证表名是否安全
pub fn validate_table_name(table_name: &str) -> Result<(), AppError> {
    validate_identifier(table_name)
}

/// 验证列名是否安全
pub fn validate_column_name(column_name: &str) -> Result<(), AppError> {
    validate_identifier(column_name)
}

/// 转义 SQL 字符串字面量以防止注入
pub fn escape_sql_string(input: &str) -> String {
    // 通过双引号转义单引号（SQL 标准）
    format!("'{}'", input.replace('\'', "''"))
}

/// 验证值是否对 SQL 插值安全（基本字母数字检查，用于简单值）
pub fn validate_sql_value(value: &str) -> Result<(), AppError> {
    // 允许字母数字、空格、基本标点符号
    if value.chars().all(|c| c.is_alphanumeric() || c.is_whitespace() || ".,-_@:;/".contains(c)) {
        Ok(())
    } else {
        Err(AppError::InvalidIdentifier(
            format!("无效的 SQL 值: '{}'", value)
        ))
    }
}

/// 危险 SQL 关键字列表 — 用于前端查询工具的写操作拦截
const DANGEROUS_SQL_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "TRUNCATE",
    "REPLACE", "GRANT", "REVOKE", "RENAME", "CALL", "EXEC", "EXECUTE",
    "MERGE", "UPSERT", "LOAD", "INTO", "SET", "COMMIT", "ROLLBACK",
];

/// 验证 SQL 是否对前端安全（只允许只读查询）
///
/// 此函数阻止来自前端查询工具的写操作，防止意外数据修改。
/// 内部工具（部署、合并等）应绕过此检查。
///
/// 允许的语句：SELECT, SHOW, DESCRIBE, EXPLAIN, WITH (CTE)
pub fn validate_safe_for_frontend(sql: &str) -> Result<(), AppError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(AppError::SqlInjectionRisk("SQL 语句不能为空".to_string()));
    }

    // 去除注释前缀 (-- 或 /*) 和前导空白
    let upper = trimmed.to_uppercase();

    // 允许的只读语句前缀
    let read_prefixes = ["SELECT", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "WITH"];
    for prefix in &read_prefixes {
        if upper.starts_with(prefix) {
            // 确保不是 SELECT ... INTO 写操作
            if upper.contains(" INTO ") {
                return Err(AppError::SqlInjectionRisk(
                    "SELECT INTO 写操作不被允许".to_string()
                ));
            }
            // 检查多语句（分号分隔）
            if trimmed.contains(';') {
                return Err(AppError::SqlInjectionRisk(
                    "多语句执行不被允许".to_string()
                ));
            }
            return Ok(());
        }
    }

    // 检查危险关键字
    for keyword in DANGEROUS_SQL_KEYWORDS {
        if upper.starts_with(keyword) || upper.contains(&format!(" {} ", keyword)) {
            return Err(AppError::SqlInjectionRisk(
                format!("写操作 '{}' 不被允许，请使用只读查询", keyword)
            ));
        }
    }

    Err(AppError::SqlInjectionRisk(
        "不支持的 SQL 语句类型，请使用 SELECT/SHOW/DESCRIBE/EXPLAIN".to_string()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_identifiers() {
        assert!(validate_identifier("users").is_ok());
        assert!(validate_identifier("_private").is_ok());
        assert!(validate_identifier("Table123").is_ok());
        assert!(validate_identifier("my_schema").is_ok());
    }

    #[test]
    fn test_invalid_identifiers() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("123table").is_err());
        assert!(validate_identifier("my-table").is_err());
        assert!(validate_identifier("my table").is_err());
        assert!(validate_identifier("users; DROP TABLE").is_err());
        assert!(validate_identifier("it's").is_err());
    }

    #[test]
    fn test_escape_mysql_identifier() {
        assert_eq!(escape_identifier("users", SqlDialect::MySql), "`users`");
        assert_eq!(escape_identifier("my_table", SqlDialect::MySql), "`my_table`");
    }

    #[test]
    fn test_escape_postgres_identifier() {
        assert_eq!(escape_identifier("users", SqlDialect::Postgres), "\"users\"");
        assert_eq!(escape_identifier("my_table", SqlDialect::Postgres), "\"my_table\"");
    }

    #[test]
    fn test_escape_sqlite_identifier() {
        assert_eq!(escape_identifier("users", SqlDialect::Sqlite), "\"users\"");
        assert_eq!(escape_identifier("my_table", SqlDialect::Sqlite), "\"my_table\"");
    }

    #[test]
    fn test_escape_sql_server_identifier() {
        assert_eq!(escape_identifier("users", SqlDialect::SqlServer), "[users]");
        assert_eq!(escape_identifier("my_table", SqlDialect::SqlServer), "[my_table]");
    }

    #[test]
    fn test_build_order_by() {
        assert_eq!(
            build_order_by("id", None, SqlDialect::Sqlite).unwrap(),
            "\"id\" ASC"
        );
        assert_eq!(
            build_order_by("name", Some("DESC"), SqlDialect::Sqlite).unwrap(),
            "\"name\" DESC"
        );
        assert!(build_order_by("id", Some("INVALID"), SqlDialect::Sqlite).is_err());
    }

    #[test]
    fn test_escape_sql_string() {
        assert_eq!(escape_sql_string("normal"), "'normal'");
        assert_eq!(escape_sql_string("it's"), "'it''s'");
        assert_eq!(escape_sql_string("O'Reilly"), "'O''Reilly'");
    }

    #[test]
    fn test_validate_safe_for_frontend_read() {
        assert!(validate_safe_for_frontend("SELECT * FROM users").is_ok());
        assert!(validate_safe_for_frontend("select id from orders").is_ok());
        assert!(validate_safe_for_frontend("SHOW DATABASES").is_ok());
        assert!(validate_safe_for_frontend("SHOW TABLES").is_ok());
        assert!(validate_safe_for_frontend("DESCRIBE users").is_ok());
        assert!(validate_safe_for_frontend("EXPLAIN SELECT 1").is_ok());
        assert!(validate_safe_for_frontend("WITH cte AS (SELECT 1) SELECT * FROM cte").is_ok());
    }

    #[test]
    fn test_validate_safe_for_frontend_write_blocked() {
        assert!(validate_safe_for_frontend("INSERT INTO users VALUES (1)").is_err());
        assert!(validate_safe_for_frontend("UPDATE users SET name='x'").is_err());
        assert!(validate_safe_for_frontend("DELETE FROM users").is_err());
        assert!(validate_safe_for_frontend("DROP TABLE users").is_err());
        assert!(validate_safe_for_frontend("ALTER TABLE users ADD col INT").is_err());
        assert!(validate_safe_for_frontend("TRUNCATE TABLE users").is_err());
        assert!(validate_safe_for_frontend("CREATE TABLE test (id INT)").is_err());
        assert!(validate_safe_for_frontend("GRANT ALL ON *.* TO 'user'").is_err());
    }

    #[test]
    fn test_validate_safe_for_frontend_injection_attempts() {
        assert!(validate_safe_for_frontend("SELECT 1; DROP TABLE users").is_err());
        assert!(validate_safe_for_frontend("SELECT * INTO outfile '/tmp/x'").is_err());
        assert!(validate_safe_for_frontend("  DROP TABLE users").is_err());
        assert!(validate_safe_for_frontend("").is_err());
    }
}