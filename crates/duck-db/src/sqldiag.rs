//! SQL 语义诊断（dbx 的 `useQueryEditorDiagnostics` 同款功能，内核层）。
//!
//! dbx 在编辑器里做的是「语义级」检查：解析报错 + 引用的表在目录里不存在
//! （未知对象高亮）。本模块是同一件事的**纯函数内核**：输入 SQL 文本 + 引擎
//! 种类 + 会话目录（[`CatalogTable`]），输出一串带行列号的诊断；零 I/O、不碰
//! 连接，可以被单测钉死。
//!
//! 与 dbx 的差距（刻意取舍，见 `docs/database-design.md` §4.18）：
//! - dbx 还校验**列**引用（懒加载每张表的列清单）。我们的目录只有表名
//!   （[`CatalogTable`] 无列清单），列校验没有数据源，**不做**——宁缺毋滥，
//!   校验个寂寞只会把用户往错误的修改方向推。
//! - dbx 的诊断画在编辑器里（CodeMirror 装饰）。gpui-base 编辑器没有装饰
//!   通道，UI 层把结果摆成编辑器下方的**问题条**（见 db 页）。
//! - 解析失败时 sqlparser 的报错文本自带 ` at Line: N, Column: M` 后缀
//!   （`Location` 的 Display），直接从消息里解出来；解不出就 0（UI 显示
//!   时按「未知位置」处理）。

use sqlparser::ast::{Query, TableFactor, Visit, Visitor};
use sqlparser::dialect::{
    ClickHouseDialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;
use std::ops::ControlFlow;

use crate::config::DatabaseKind;
use crate::drivers::CatalogTable;

/// 一条诊断的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 解析失败 —— 语句执行不了。
    Error,
    /// 引用了目录里不存在的表 —— 可能是写错名字，也可能是目录还没刷新。
    Warning,
}

/// 一条语义诊断。
///
/// `line` / `column` 从 1 起；`0` = 定位不了（UI 按「未知位置」显示）。
/// `message` 是**英文**（内核层惯例，界面层用 `trf` 包中文外壳）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlDiagnostic {
    /// 1 起的行号；0 = 未知。
    pub line: u64,
    /// 1 起的列号；0 = 未知。
    pub column: u64,
    /// 严重程度。
    pub severity: Severity,
    /// 英文描述。
    pub message: String,
}

/// 单条诊断上限（防夸张输入刷屏：一个故意写坏的文件能产出几百条）。
const DIAGNOSTICS_CAP: usize = 50;

/// 引擎的解析方言；不认识或非 SQL 的引擎回落 `GenericDialect`。
fn dialect_for(kind: DatabaseKind) -> &'static dyn sqlparser::dialect::Dialect {
    match kind {
        DatabaseKind::Sqlite => &SQLiteDialect {},
        DatabaseKind::Postgres => &PostgreSqlDialect {},
        DatabaseKind::Mysql => &MySqlDialect {},
        DatabaseKind::ClickHouse => &ClickHouseDialect {},
        DatabaseKind::SqlServer => &MsSqlDialect {},
        // MongoDB / Elasticsearch 的编辑器内容不是 SQL（JSON 命令文档 / 控制台
        // 语法），语义诊断对它们没有定义——空名单，UI 层据此隐藏问题条。
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => &GenericDialect {},
    }
}

/// 对一段 SQL 做语义诊断。
///
/// - 非 SQL 引擎（MongoDB / Elasticsearch）返回空；
/// - 解析报错：每条 `Error` 级诊断（sqlparser 报第一个错就停）；
/// - 目录**非空**时校验表引用：引用了目录（与 CTE）里都没有的表 → `Warning`。
///   目录为空（元数据没加载 / 加载失败）时**跳过**表校验——空目录报「全是未知
///   表」是误报洪水，不是诊断。
///
/// 名字匹配**不区分大小写**：PostgreSQL 未加引号折叠为小写、MySQL 在部分平台
/// 区分大小写、SQLite 又不同——诊断是提示不是编译器，按最宽松的口径避免把
/// 正确的 SQL 标成问题。
#[must_use]
pub fn diagnose_sql(kind: DatabaseKind, sql: &str, tables: &[CatalogTable]) -> Vec<SqlDiagnostic> {
    if matches!(kind, DatabaseKind::Mongo | DatabaseKind::Elasticsearch) {
        return Vec::new();
    }
    let mut out = Vec::new();

    let parsed = Parser::parse_sql(dialect_for(kind), sql);
    let statements = match parsed {
        Ok(statements) => statements,
        Err(error) => {
            let (line, column, message) = split_error_location(&error.to_string());
            out.push(SqlDiagnostic {
                line,
                column,
                severity: Severity::Error,
                message,
            });
            return out;
        }
    };

    if tables.is_empty() {
        return out;
    }

    // 目录检索集：裸名 + `schema.name` 限定名，全小写。
    let bare: std::collections::HashSet<String> = tables
        .iter()
        .map(|table| table.name.to_lowercase())
        .collect();
    let qualified: std::collections::HashSet<String> = tables
        .iter()
        .filter_map(|table| {
            let schema = table.schema.as_ref()?;
            Some(format!(
                "{}.{}",
                schema.to_lowercase(),
                table.name.to_lowercase()
            ))
        })
        .collect();

    for statement in &statements {
        let mut collector = RelationCollector::default();
        let _ = statement.visit(&mut collector);
        for name in collector.relations {
            // CTE 名收的是小写；比对对象是引用的**末段**裸名。
            let cte_hit = name
                .0
                .last()
                .is_some_and(|ident| collector.ctes.contains(&ident.value.to_lowercase()));
            if cte_hit {
                continue;
            }
            let display = name.to_string();
            let parts = name.0;
            let Some(last) = parts.last() else {
                continue;
            };
            // 匹配口径按**引用的段数**走：裸名（1 段）对目录裸名；限定名（≥2 段）
            // 取末两段对 `schema.name` 限定集 —— `other.users` 不能因为目录里有裸
            // 的 `users` 就放行（在 PostgreSQL 那是另一个 schema 的表）。
            let hit = if parts.len() == 1 {
                bare.contains(&last.value.to_lowercase())
            } else {
                let schema = &parts[parts.len() - 2];
                qualified.contains(&format!(
                    "{}.{}",
                    schema.value.to_lowercase(),
                    last.value.to_lowercase()
                ))
            };
            if hit {
                continue;
            }
            let (line, column) = locate_identifier(sql, &last.value);
            out.push(SqlDiagnostic {
                line,
                column,
                severity: Severity::Warning,
                message: format!("Unknown table \"{display}\""),
            });
            if out.len() >= DIAGNOSTICS_CAP {
                return out;
            }
        }
    }
    out
}

/// 把 sqlparser 报错文本拆成（行, 列, 去掉位置后缀的消息）。
///
/// 报错消息形如 `Expected: ..., found: ... at Line: 3, Column: 12`
/// （`Location` 的 Display，见 sqlparser tokenizer.rs）。找不到后缀就整体
/// 当消息、位置记 0——**宁可少给位置，不能给错位置**。
fn split_error_location(message: &str) -> (u64, u64, String) {
    const MARKER: &str = " at Line: ";
    let Some(start) = message.rfind(MARKER) else {
        return (0, 0, message.to_owned());
    };
    let tail = &message[start + MARKER.len()..];
    // 尾巴形如 `3, Column: 12`；任何一段解不动就整条放弃定位。
    let Some((line_part, column_part)) = tail.split_once(", Column: ") else {
        return (0, 0, message.to_owned());
    };
    let (Ok(line), Ok(column)) = (
        line_part.trim().parse::<u64>(),
        column_part.trim().parse::<u64>(),
    ) else {
        return (0, 0, message.to_owned());
    };
    (line, column, message[..start].to_owned())
}

/// 在 SQL 文本里找一个标识符的（行, 列）——**第一次整词出现**的位置。
///
/// 简单线性扫描而不是维护 token 位置表：诊断只在编辑器内容变化时跑一次，
/// 文本量级（几 KB～几十 KB）下一次扫描远快过给每条诊断建索引。前后邻字符
/// 是标识符字符（字母数字 / 下划线）则不算 —— 避免 `users` 撞上
/// `user_settings` 的前缀。
fn locate_identifier(sql: &str, needle: &str) -> (u64, u64) {
    let bytes = sql.as_bytes();
    let target = needle.as_bytes();
    if target.is_empty() {
        return (0, 0);
    }
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut line: u64 = 1;
    let mut column: u64 = 1;
    for index in 0..bytes.len() {
        let before_ok = index == 0 || !is_word(bytes[index - 1]);
        let after_ok = bytes
            .get(index + target.len())
            .is_none_or(|byte| !is_word(*byte));
        if before_ok && after_ok && bytes[index..].starts_with(target) {
            return (line, column);
        }
        if bytes[index] == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (0, 0)
}

/// 收集一条语句里的表引用与 CTE 名。
///
/// - `pre_visit_query`：每层查询的 `WITH` 子句里的 CTE 别名都收进来（嵌套
///   子查询里的 CTE 也算 —— 它们的作用域覆盖整条内层查询）；
/// - `pre_visit_table_factor`：只收**真表**（`args: None`）；表值函数
///   （`generate_series(1, 10)` 之类）与 `UNNEST` 不是目录对象，收进来全是误报。
#[derive(Default)]
struct RelationCollector {
    relations: Vec<sqlparser::ast::ObjectName>,
    ctes: Vec<String>,
}

impl Visitor for RelationCollector {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                self.ctes.push(cte.alias.name.value.to_lowercase());
            }
        }
        ControlFlow::<()>::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            name, args: None, ..
        } = factor
        {
            self.relations.push(name.clone());
        }
        ControlFlow::<()>::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(entries: &[(&str, Option<&str>)]) -> Vec<CatalogTable> {
        entries
            .iter()
            .map(|(name, schema)| CatalogTable {
                name: (*name).to_owned(),
                kind: crate::drivers::TableKind::Table,
                schema: schema.map(|schema| schema.to_owned()),
            })
            .collect()
    }

    #[test]
    fn mongo_and_elasticsearch_yield_nothing() {
        let tables = catalog(&[("users", None)]);
        assert!(diagnose_sql(DatabaseKind::Mongo, "db.users.find()", &tables).is_empty());
        assert!(diagnose_sql(DatabaseKind::Elasticsearch, "GET users/_search", &tables).is_empty());
    }

    #[test]
    fn parse_error_reports_line_and_column() {
        let out = diagnose_sql(DatabaseKind::Sqlite, "SELECT FROM WHERE", &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, Severity::Error);
        assert!(out[0].line >= 1, "行号应解出：{:?}", out[0]);
        assert!(
            !out[0].message.contains("at Line"),
            "消息里不该再带位置后缀"
        );
    }

    #[test]
    fn unknown_table_is_flagged_with_position() {
        let tables = catalog(&[("users", None)]);
        let out = diagnose_sql(DatabaseKind::Sqlite, "SELECT *\nFROM usres", &tables);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[0].line, 2, "未知表在第二行");
        assert!(out[0].message.contains("usres"));
    }

    #[test]
    fn cte_and_table_function_are_not_flagged() {
        let tables = catalog(&[("orders", None)]);
        let sql = "WITH big AS (SELECT * FROM orders) \
                   SELECT * FROM big CROSS JOIN generate_series(1, 3)";
        let out = diagnose_sql(DatabaseKind::Postgres, sql, &tables);
        assert!(out.is_empty(), "CTE 与表函数不该报：{out:?}");
    }

    #[test]
    fn empty_catalog_skips_table_check_but_keeps_parse_errors() {
        let out = diagnose_sql(DatabaseKind::Mysql, "SELECT * FROM anything", &[]);
        assert!(out.is_empty(), "空目录不报未知表：{out:?}");
        let out = diagnose_sql(DatabaseKind::Mysql, "SELECT ]]", &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, Severity::Error);
    }

    #[test]
    fn schema_qualified_reference_matches() {
        let tables = catalog(&[("users", Some("public"))]);
        let out = diagnose_sql(
            DatabaseKind::Postgres,
            "SELECT * FROM public.users",
            &tables,
        );
        assert!(out.is_empty(), "{out:?}");
        let out = diagnose_sql(DatabaseKind::Postgres, "SELECT * FROM other.users", &tables);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn diagnostics_are_capped() {
        let tables = catalog(&[("users", None)]);
        // 51 张未知表 → 截到上限。
        let sql: String = (0..51)
            .map(|index| format!("SELECT * FROM missing_{index};\n"))
            .collect();
        let out = diagnose_sql(DatabaseKind::Sqlite, &sql, &tables);
        assert_eq!(out.len(), DIAGNOSTICS_CAP);
    }
}
