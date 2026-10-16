//! spec → SQL（**纯函数**：不碰 gpui、不碰连接、不碰 I/O）。
//!
//! 拆出来单放的理由与 `charts.rs` / 计划树一致：**把「拼串」这件事做成可单测的纯计算**，
//! 面板那边只负责收集输入。生成的语句一律走 `duck_db::quote_identifier` /
//! `quote_qualified`（与结果集导出、对象树「生成 SELECT」同一套引号规则）——
//! 界面层**不许**再写第二份引号逻辑。
//!
//! 与 DBFlux 的一处**故意差异**：DBFlux 的预览是**参数化占位符**（值不下发，运行期绑定）；
//! 本仓的编辑器没有参数绑定通道（跑的是**文本** SQL），所以这里按引擎写成**字面量** ——
//! 预览与「填进编辑器直接执行」的内容必须逐字节一致，否则用户看到的不是他要跑的东西。

use super::*;

/// 目标引擎的 SQL 口味（构建器只关心这三件事）。
///
/// `quote` 直接取 [`DatabaseKind::identifier_quote`]；`None` 的引擎（Mongo / Elasticsearch）
/// **没有 SQL 这一层**，`dialect_for` 返回 `None`，界面据此不给入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BuilderDialect {
    /// 标识符引号字符（`"` / `` ` `` / `[`）。
    ///
    /// ⚠️ SQL Server 的 `[` 只需要**左**括号闭合用的 `]`，引号规则由
    /// [`duck_db::quote_identifier`] 说了算（它按 `quote` 字符成对包），这里只透传。
    pub quote: char,
    /// 布尔字面量的写法：`1` / `0` 还是 `TRUE` / `FALSE`。
    pub bool_literal: BoolLiteral,
    /// 分页子句的写法。
    pub limit: LimitStyle,
}

/// 布尔字面量口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoolLiteral {
    /// `1` / `0` —— SQLite 与 SQL Server 没有真正的布尔类型。
    OneZero,
    /// `TRUE` / `FALSE`。
    TrueFalse,
}

/// 分页子句口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LimitStyle {
    /// `LIMIT n OFFSET m`（SQLite / PostgreSQL / MySQL / ClickHouse）。
    LimitOffset,
    /// `OFFSET m ROWS FETCH NEXT n ROWS ONLY`（SQL Server；**必须**有 `ORDER BY`）。
    OffsetFetch,
}

/// 取引擎的构建器口味；`None` = 该引擎没有 SQL（Mongo / Elasticsearch）。
pub(super) fn dialect_for(engine: DatabaseKind) -> Option<BuilderDialect> {
    let quote = engine.identifier_quote()?;
    Some(match engine {
        DatabaseKind::Sqlite => BuilderDialect {
            quote,
            bool_literal: BoolLiteral::OneZero,
            limit: LimitStyle::LimitOffset,
        },
        DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::ClickHouse => BuilderDialect {
            quote,
            bool_literal: BoolLiteral::TrueFalse,
            limit: LimitStyle::LimitOffset,
        },
        DatabaseKind::SqlServer => BuilderDialect {
            quote,
            bool_literal: BoolLiteral::OneZero,
            limit: LimitStyle::OffsetFetch,
        },
        // `identifier_quote()` 已经挡掉这两个，分支写全只为让编译器替我们守穷尽性。
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => return None,
    })
}

/// 渲染结果：SQL 文本 + **没进 SQL 的行数**（半填完的条件）。
///
/// 半填完的条件（例如选了列没填值）**不拼进 SQL**，数量交给面板显示 —— 静默拼一个
/// `= ''` 会让界面上的条件与真正跑的语句不一致，比不拼更坏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenderedSql {
    pub sql: String,
    /// 未填完而未进 SQL 的条件数 / 未选列的条件数。
    pub incomplete: usize,
}

/// 把 spec 渲染成 `SELECT` / `UPDATE` / `DELETE`。
///
/// `None` = 还没选目标表（面板显示指引空态，不该出现在 `Some("SELECT * FROM ")` 这种
/// 半截语句上）。
pub(super) fn render(spec: &BuilderSpec, dialect: BuilderDialect) -> Option<RenderedSql> {
    let table = spec.table.as_ref()?;
    let qualified = quote_qualified(table, dialect.quote).ok()?;
    let from = match spec.alias.trim() {
        // 别名与表名相同时不写 `AS`：默认别名就是表名，写出来只是噪音。
        alias if !alias.is_empty() && alias != table.name => {
            format!("{qualified} AS {}", quote_ident(alias, dialect.quote).ok()?)
        }
        _ => qualified,
    };

    let (mut sql, incomplete) = match spec.mode {
        BuilderMode::Select => (
            render_select(spec, &from, dialect)?,
            render_where(spec, dialect),
        ),
        BuilderMode::Update => (
            render_update(spec, &from, dialect)?,
            render_where(spec, dialect),
        ),
        BuilderMode::Delete => (format!("DELETE FROM {from}"), render_where(spec, dialect)),
    };

    let (where_sql, incomplete_where) = incomplete;
    if !where_sql.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_sql);
    }

    if spec.mode == BuilderMode::Select {
        if !spec.sorts.is_empty() {
            let entries: Vec<String> = spec
                .sorts
                .iter()
                .filter(|row| !row.column.trim().is_empty())
                .map(|row| {
                    let column = quote_ident(row.column.trim(), dialect.quote)
                        .unwrap_or_else(|_| row.column.trim().to_owned());
                    format!("{column} {}", if row.desc { "DESC" } else { "ASC" })
                })
                .collect();
            if !entries.is_empty() {
                sql.push_str(" ORDER BY ");
                sql.push_str(&entries.join(", "));
            }
        }
        sql.push_str(&render_paging(spec, dialect));
    }

    Some(RenderedSql {
        sql: format!("{sql};"),
        incomplete: incomplete_where,
    })
}

/// `SELECT` 部分（投影 + `FROM`）。
fn render_select(spec: &BuilderSpec, from: &str, dialect: BuilderDialect) -> Option<String> {
    let projection = match spec.projection {
        ProjectionMode::All => "*".to_owned(),
        ProjectionMode::Explicit => {
            let columns: Vec<String> = spec
                .columns
                .iter()
                .filter(|row| !row.column.trim().is_empty())
                .map(|row| {
                    let column = quote_ident(row.column.trim(), dialect.quote)
                        .unwrap_or_else(|_| row.column.trim().to_owned());
                    match row.alias.trim() {
                        "" => column,
                        alias => format!(
                            "{column} AS {}",
                            quote_ident(alias, dialect.quote).unwrap_or_else(|_| alias.to_owned())
                        ),
                    }
                })
                .collect();
            if columns.is_empty() {
                // 选了「指定列」却一条都没填：给 `*` 而不是 `SELECT `（半截语句跑不了，
                // 而且用户看不出哪里错了）——面板同时会有「一条列都没选」的读数。
                "*".to_owned()
            } else {
                columns.join(", ")
            }
        }
    };
    Some(format!("SELECT {projection} FROM {from}"))
}

/// `UPDATE ... SET ...` 部分（一条赋值都没有时返回 `None`：`UPDATE t SET` 不是语句）。
fn render_update(spec: &BuilderSpec, from: &str, dialect: BuilderDialect) -> Option<String> {
    let assignments: Vec<String> = spec
        .assignments
        .iter()
        .filter(|row| !row.column.trim().is_empty())
        .map(|row| {
            let column = quote_ident(row.column.trim(), dialect.quote)
                .unwrap_or_else(|_| row.column.trim().to_owned());
            format!(
                "{column} = {}",
                literal_or_expression(row.value.trim(), row.raw, dialect)
            )
        })
        .collect();
    if assignments.is_empty() {
        return None;
    }
    Some(format!("UPDATE {from} SET {}", assignments.join(", ")))
}

/// `WHERE` 子句（不含 `WHERE` 关键字）；返回 `(子句, 未填完行数)`。
fn render_where(spec: &BuilderSpec, dialect: BuilderDialect) -> (String, usize) {
    let mut parts: Vec<String> = Vec::new();
    let mut incomplete = 0usize;
    for row in &spec.predicates {
        let column = row.column.trim();
        if column.is_empty() {
            incomplete += 1;
            continue;
        }
        let column = quote_ident(column, dialect.quote).unwrap_or_else(|_| column.to_owned());
        if !row.op.needs_value() {
            parts.push(format!("{column} {}", null_operator(row.op)));
            continue;
        }
        let value = row.value.trim();
        if value.is_empty() {
            incomplete += 1;
            continue;
        }
        parts.push(format!(
            "{column} {} {}",
            row.op.sql_operator().unwrap_or("="),
            literal_or_expression(value, row.raw_value, dialect)
        ));
    }
    let joiner = match spec.bool_op {
        BoolOp::And => " AND ",
        BoolOp::Or => " OR ",
    };
    // 多条件一律加括号：`a OR b AND c` 的优先级极易读错，括号把「我拼的和你想的一样」
    // 变成肉眼可见（单条时不加，省得满屏括号）。
    let clause = if parts.len() > 1 {
        format!("({})", parts.join(joiner))
    } else {
        parts.join(joiner)
    };
    (clause, incomplete)
}

/// 分页子句。
///
/// ⚠️ **`OFFSET` 只在有 `LIMIT` 时下发**：SQLite 的 `OFFSET` 必须跟 `LIMIT`，
/// MySQL 同理，SQL Server 要 `ORDER BY` + `OFFSET/FETCH`。与其为每条引擎各写一套
/// 「没上限时的 offset 写法」（`LIMIT -1` / `LIMIT 18446744073709551615` …），不如
/// **统一不下发**，由 [`offset_is_ignored`] 告诉面板提示用户补上限 —— 少一套方言分歧，
/// 也少一类「预览看着对、跑起来是语法错」。
fn render_paging(spec: &BuilderSpec, dialect: BuilderDialect) -> String {
    let offset = spec.offset;
    match dialect.limit {
        LimitStyle::LimitOffset => match spec.limit {
            Some(limit) if offset > 0 => format!(" LIMIT {limit} OFFSET {offset}"),
            Some(limit) => format!(" LIMIT {limit}"),
            None => String::new(),
        },
        LimitStyle::OffsetFetch => {
            // 没有 `ORDER BY` 时 `OFFSET/FETCH` 不合法（SQL Server 硬要求）。
            if !has_order_by(spec) {
                return String::new();
            }
            let fetch = spec.limit.map_or_else(String::new, |limit| {
                format!(" FETCH NEXT {limit} ROWS ONLY")
            });
            let skip = if offset > 0 { offset } else { 0 };
            format!(" OFFSET {skip} ROWS{fetch}")
        }
    }
}

/// 排序里有没有真填了列（空行不算）。
fn has_order_by(spec: &BuilderSpec) -> bool {
    spec.sorts.iter().any(|row| !row.column.trim().is_empty())
}

/// 用户填了偏移、但它**不会进 SQL** —— 面板据此显示一句提示（而不是默默吞掉）。
pub(super) fn offset_is_ignored(spec: &BuilderSpec, dialect: BuilderDialect) -> bool {
    if spec.offset == 0 {
        return false;
    }
    match dialect.limit {
        LimitStyle::LimitOffset => spec.limit.is_none(),
        LimitStyle::OffsetFetch => !has_order_by(spec),
    }
}

/// 值：`raw` 为真时按**原样表达式**下发（用户要写函数 / 算术），否则当字面量。
fn literal_or_expression(value: &str, raw: bool, dialect: BuilderDialect) -> String {
    if raw {
        return value.to_owned();
    }
    literal(value, dialect)
}

/// 「不吃值」的两档算子（`IS NULL` / `IS NOT NULL`）的 SQL 文本。
///
/// ⚠️ **不走 `FilterOp::sql_operator()`**：内核那边它对这两档**刻意返回 `None`**
/// （`IS NULL` 在 `filter_conditions` 里单独拼，因为值走绑定参数），于是
/// `sql_operator().unwrap_or("IS NULL")` 会把 **`IS NOT NULL` 静默降级成 `IS NULL`** ——
/// 界面显示「不为空」、跑出来是「为空」，一整类错行且不报错。这里的单测
/// `null_predicates_take_no_value` 就是为它写的（该 bug 正是被它抓出来的）。
fn null_operator(op: FilterOp) -> &'static str {
    match op {
        FilterOp::IsNotNull => "IS NOT NULL",
        _ => "IS NULL",
    }
}

/// 把输入框里的一段文本渲染成 SQL 字面量。
///
/// 判据只有一条：**能不能一眼看出是「别的东西」** —— `NULL`、整/小数、布尔、`'引号串'`
/// 原样；其余一律当文本串加引号并转义（`'` → `''`）。宁可把数字包成字符串，也不要把
/// 用户输的 `abc` 拼成裸标识符（那是语法错误，甚至可能是注入面）。
pub(super) fn literal(value: &str, dialect: BuilderDialect) -> String {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return "NULL".to_owned();
    }
    if trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("false") {
        let flag = trimmed.eq_ignore_ascii_case("true");
        return match dialect.bool_literal {
            BoolLiteral::OneZero => if flag { "1" } else { "0" }.to_owned(),
            BoolLiteral::TrueFalse => if flag { "TRUE" } else { "FALSE" }.to_owned(),
        };
    }
    if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        return trimmed.to_owned();
    }
    // 用户已经自己带了引号：原样（他要的就是这个串）。
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        return trimmed.to_owned();
    }
    format!("'{}'", trimmed.replace('\'', "''"))
}

/// 标识符引号（失败＝名字里有引号字符，交给调用方回落原名）。
fn quote_ident(name: &str, quote: char) -> Result<String, DbError> {
    quote_identifier(name, quote)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQLITE: BuilderDialect = BuilderDialect {
        quote: '"',
        bool_literal: BoolLiteral::OneZero,
        limit: LimitStyle::LimitOffset,
    };

    fn spec_with(table: &str) -> BuilderSpec {
        BuilderSpec {
            table: Some(TableRef::bare(table)),
            alias: table.to_owned(),
            ..BuilderSpec::default()
        }
    }

    fn render_sqlite(spec: &BuilderSpec) -> RenderedSql {
        render(spec, SQLITE).expect("选了表就该出 SQL")
    }

    #[test]
    fn empty_spec_is_star_select() {
        let sql = render_sqlite(&spec_with("users")).sql;
        assert_eq!(sql, "SELECT * FROM \"users\";");
    }

    #[test]
    fn no_table_renders_nothing() {
        assert!(render(&BuilderSpec::default(), SQLITE).is_none());
    }

    #[test]
    fn alias_is_only_emitted_when_it_differs_from_the_table() {
        let mut spec = spec_with("users");
        assert_eq!(render_sqlite(&spec).sql, "SELECT * FROM \"users\";");
        spec.alias = "u".to_owned();
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" AS \"u\";"
        );
    }

    #[test]
    fn explicit_projection_with_alias() {
        let mut spec = spec_with("users");
        spec.projection = ProjectionMode::Explicit;
        spec.columns = vec![
            ColRow {
                column: "id".to_owned(),
                alias: String::new(),
            },
            ColRow {
                column: "name".to_owned(),
                alias: "n".to_owned(),
            },
        ];
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT \"id\", \"name\" AS \"n\" FROM \"users\";"
        );
    }

    #[test]
    fn explicit_projection_without_columns_falls_back_to_star() {
        let mut spec = spec_with("users");
        spec.projection = ProjectionMode::Explicit;
        assert_eq!(render_sqlite(&spec).sql, "SELECT * FROM \"users\";");
    }

    #[test]
    fn predicates_join_with_the_top_level_operator_and_get_parenthesised() {
        let mut spec = spec_with("users");
        spec.predicates = vec![
            PredicateRow {
                column: "age".to_owned(),
                op: FilterOp::Gte,
                value: "18".to_owned(),
                raw_value: false,
            },
            PredicateRow {
                column: "name".to_owned(),
                op: FilterOp::Like,
                value: "%a%".to_owned(),
                raw_value: false,
            },
        ];
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" WHERE (\"age\" >= 18 AND \"name\" LIKE '%a%');"
        );
        spec.bool_op = BoolOp::Or;
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" WHERE (\"age\" >= 18 OR \"name\" LIKE '%a%');"
        );
    }

    #[test]
    fn null_predicates_take_no_value() {
        let mut spec = spec_with("users");
        spec.predicates = vec![
            PredicateRow {
                column: "deleted_at".to_owned(),
                op: FilterOp::IsNull,
                value: String::new(),
                raw_value: false,
            },
            PredicateRow {
                column: "email".to_owned(),
                op: FilterOp::IsNotNull,
                value: String::new(),
                raw_value: false,
            },
        ];
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" WHERE (\"deleted_at\" IS NULL AND \"email\" IS NOT NULL);"
        );
    }

    #[test]
    fn half_filled_predicates_are_counted_not_emitted() {
        let mut spec = spec_with("users");
        spec.predicates = vec![
            PredicateRow {
                column: "age".to_owned(),
                op: FilterOp::Eq,
                value: String::new(),
                raw_value: false,
            },
            PredicateRow {
                column: "  ".to_owned(),
                op: FilterOp::Eq,
                value: "1".to_owned(),
                raw_value: false,
            },
        ];
        let rendered = render_sqlite(&spec);
        assert_eq!(rendered.sql, "SELECT * FROM \"users\";");
        assert_eq!(rendered.incomplete, 2);
    }

    #[test]
    fn sort_and_paging_follow_the_dialect() {
        let mut spec = spec_with("users");
        spec.sorts = vec![
            SortRow {
                column: "created_at".to_owned(),
                desc: true,
            },
            SortRow {
                column: String::new(),
                desc: false,
            },
        ];
        spec.limit = Some(50);
        spec.offset = 100;
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" ORDER BY \"created_at\" DESC LIMIT 50 OFFSET 100;"
        );
    }

    #[test]
    fn offset_without_limit_is_ignored_and_reported() {
        let mut spec = spec_with("users");
        spec.offset = 10;
        assert_eq!(render_sqlite(&spec).sql, "SELECT * FROM \"users\";");
        assert!(offset_is_ignored(&spec, SQLITE), "面板要据此提示补上限");
        spec.limit = Some(5);
        assert!(!offset_is_ignored(&spec, SQLITE));
        assert_eq!(
            render_sqlite(&spec).sql,
            "SELECT * FROM \"users\" LIMIT 5 OFFSET 10;"
        );
    }

    #[test]
    fn sqlserver_uses_offset_fetch_only_when_ordered() {
        let mssql = dialect_for(DatabaseKind::SqlServer).expect("SQL Server 有 SQL");
        let mut spec = spec_with("users");
        spec.limit = Some(10);
        // 没排序：OFFSET/FETCH 不合法 ⇒ 不拼分页（面板会提示补 ORDER BY）。
        assert_eq!(
            render(&spec, mssql).expect("有表").sql,
            "SELECT * FROM \"users\";"
        );
        spec.sorts = vec![SortRow {
            column: "id".to_owned(),
            desc: false,
        }];
        spec.offset = 20;
        assert_eq!(
            render(&spec, mssql).expect("有表").sql,
            "SELECT * FROM \"users\" ORDER BY \"id\" ASC OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY;"
        );
    }

    #[test]
    fn literals_are_typed_by_shape() {
        assert_eq!(literal("42", SQLITE), "42");
        assert_eq!(literal("-1.5", SQLITE), "-1.5");
        assert_eq!(literal("NULL", SQLITE), "NULL");
        assert_eq!(literal("true", SQLITE), "1");
        assert_eq!(literal("o'brien", SQLITE), "'o''brien'");
        assert_eq!(literal("'already'", SQLITE), "'already'");
        let pg = dialect_for(DatabaseKind::Postgres).expect("PG 有 SQL");
        assert_eq!(literal("true", pg), "TRUE");
        assert_eq!(literal("false", pg), "FALSE");
    }

    #[test]
    fn update_and_delete_reuse_the_same_where() {
        let mut spec = spec_with("users");
        spec.mode = BuilderMode::Update;
        spec.assignments = vec![AssignRow {
            column: "archived".to_owned(),
            value: "true".to_owned(),
            raw: false,
        }];
        spec.predicates = vec![PredicateRow {
            column: "id".to_owned(),
            op: FilterOp::Eq,
            value: "7".to_owned(),
            raw_value: false,
        }];
        assert_eq!(
            render_sqlite(&spec).sql,
            "UPDATE \"users\" SET \"archived\" = 1 WHERE \"id\" = 7;"
        );

        spec.mode = BuilderMode::Delete;
        assert_eq!(
            render_sqlite(&spec).sql,
            "DELETE FROM \"users\" WHERE \"id\" = 7;"
        );
    }

    #[test]
    fn update_without_assignments_renders_nothing() {
        let mut spec = spec_with("users");
        spec.mode = BuilderMode::Update;
        assert!(render(&spec, SQLITE).is_none());
    }

    #[test]
    fn mongo_and_elasticsearch_have_no_dialect() {
        assert!(dialect_for(DatabaseKind::Mongo).is_none());
        assert!(dialect_for(DatabaseKind::Elasticsearch).is_none());
    }

    #[test]
    fn identifier_quote_follows_the_engine() {
        let mysql = dialect_for(DatabaseKind::Mysql).expect("MySQL 有 SQL");
        let mut spec = spec_with("users");
        spec.alias = "u".to_owned();
        assert_eq!(
            render(&spec, mysql).expect("有表").sql,
            "SELECT * FROM `users` AS `u`;"
        );
    }
}
