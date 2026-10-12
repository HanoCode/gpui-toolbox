//! SQL 智能层（纯函数，零 I/O）：语句拆分、分类、风险分级与只读证明。
//!
//! 取向来自 dbx 的 `dbx-sql-core`：这层**只碰 SQL 文本，不碰数据库**，
//! 因此可以被单测钉死，也能被未来的 AI 闸门直接复用——模型产出的每条语句
//! 都必须过 [`prove_read_only`]，写操作必须走确认流程。

use sqlparser::ast::{Expr, GroupByExpr, Select, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// 拆分后的单条语句的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtKind {
    /// 返回行的读语句（SELECT / SHOW / EXPLAIN / PRAGMA 视引擎而定）。
    Read,
    /// 改数据的写语句（INSERT / UPDATE / DELETE）。
    Write,
    /// 结构与权限类（CREATE / ALTER / DROP / GRANT / …）。
    Ddl,
    /// 无法判定（解析失败、非 SQL 等）。执行器按「可能返回行」的保守路径处理。
    Other,
}

/// 风险分级（从低到高）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqlRisk {
    /// 已证明只读：可以不经确认直接执行（AI 闸门的放行条件）。
    ReadOnly,
    /// 有写入，但不删结构。
    Write,
    /// 危险：删库删表或全表清改（无 WHERE 的 DELETE / UPDATE）。
    Dangerous,
}

/// 按引号 / 注释感知的方式把一段 SQL 拆成单条语句（不含尾分号）。
///
/// 自己写拆分而不是用解析器重建文本：拆分的输入常常**不是合法 SQL**
/// （用户写一半就来执行），解析器这条路会整段失败。这里只需要认识
/// `'…'` / `"…"` / `` `…` `` 三种引号与 `--` / `/* */` 两类注释。
#[must_use]
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    #[derive(PartialEq)]
    enum In {
        None,
        Single,
        Double,
        Backtick,
        BracketComment,
    }
    let mut state = In::None;

    while let Some(c) = chars.next() {
        match state {
            In::None => match c {
                '\'' => {
                    state = In::Single;
                    current.push(c);
                }
                '"' => {
                    state = In::Double;
                    current.push(c);
                }
                '`' => {
                    state = In::Backtick;
                    current.push(c);
                }
                '-' if chars.peek() == Some(&'-') => {
                    // 行注释：吃掉直到换行（换行本身保留，让错误定位的行号不漂移）。
                    current.push(c);
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        current.push(n);
                        if n == '\n' {
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    state = In::BracketComment;
                    current.push(c);
                }
                ';' => {
                    if !current.trim().is_empty() {
                        statements.push(current.trim().to_owned());
                    }
                    current.clear();
                }
                _ => current.push(c),
            },
            In::Single | In::Double | In::Backtick => {
                current.push(c);
                if c == '\\' && state == In::Single {
                    // MySQL 风格的反斜杠转义：原样带过下一个字符。
                    if let Some(&n) = chars.peek() {
                        chars.next();
                        current.push(n);
                    }
                    continue;
                }
                let closing = match state {
                    In::Single => '\'',
                    In::Double => '"',
                    _ => '`',
                };
                if c == closing {
                    // 双写引号（'' / ""）是转义，继续留在引号里。
                    if chars.peek() == Some(&closing) {
                        current.push(chars.next().expect("peeked char must exist"));
                    } else {
                        state = In::None;
                    }
                }
            }
            In::BracketComment => {
                current.push(c);
                if c == '*' && chars.peek() == Some(&'/') {
                    current.push(chars.next().expect("peeked char must exist"));
                    state = In::None;
                }
            }
        }
    }
    if !current.trim().is_empty() {
        statements.push(current.trim().to_owned());
    }
    statements
}

/// 解析一条语句；解析失败按 `Other` 处理（调用方走保守路径）。
fn parse_one(stmt: &str) -> Option<Statement> {
    Parser::parse_sql(&GenericDialect {}, stmt)
        .ok()
        .and_then(|mut ast| if ast.len() == 1 { ast.pop() } else { None })
}

/// 按首关键字猜测类别；给解析器覆盖不到的方言词（PRAGMA / MySQL 的
/// `LOCK TABLES` 之类）兜底。
fn keyword_kind(stmt: &str) -> StmtKind {
    let head = stmt
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    match head.as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "PRAGMA" | "WITH" | "USE" => {
            StmtKind::Read
        }
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "LOAD" | "CALL" => StmtKind::Write,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "VACUUM"
        | "ANALYZE" | "REINDEX" | "LOCK" | "UNLOCK" | "SET" | "BEGIN" | "START" | "COMMIT"
        | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => StmtKind::Ddl,
        _ => StmtKind::Other,
    }
}

/// 分类一条语句。
#[must_use]
pub fn classify(stmt: &str) -> StmtKind {
    match parse_one(stmt) {
        Some(Statement::Query(_)) => StmtKind::Read,
        Some(Statement::Explain { .. })
        | Some(Statement::ExplainTable { .. })
        | Some(Statement::ShowVariables { .. })
        | Some(Statement::ShowStatus { .. })
        | Some(Statement::ShowColumns { .. })
        | Some(Statement::ShowTables { .. })
        | Some(Statement::ShowDatabases { .. })
        | Some(Statement::ShowSchemas { .. })
        | Some(Statement::ShowFunctions { .. })
        | Some(Statement::ShowViews { .. }) => StmtKind::Read,
        Some(Statement::Insert(_))
        | Some(Statement::Update { .. })
        | Some(Statement::Delete(_)) => StmtKind::Write,
        Some(Statement::CreateTable(_))
        | Some(Statement::CreateView { .. })
        | Some(Statement::CreateIndex(_))
        | Some(Statement::AlterTable { .. })
        | Some(Statement::AlterView { .. })
        | Some(Statement::Drop { .. })
        | Some(Statement::DropFunction { .. }) => StmtKind::Ddl,
        Some(_) | None => keyword_kind(stmt),
    }
}

/// 一条语句对**显式事务**的影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEffect {
    /// 开启事务（`BEGIN` / `START TRANSACTION`）。
    Begin,
    /// 结束事务（`COMMIT` / `ROLLBACK` / `END`）。
    End,
}

/// 判定单条语句对显式事务的影响（`None` = 无关）。
///
/// 只看首关键字，**不做语法解析**——用户常写半截 SQL 就执行，解析器在这条路上会整段
/// 失败，而事务状态显示必须稳。三引擎的共同拼法：SQLite / PostgreSQL 的 `BEGIN`、
/// MySQL 的 `START TRANSACTION`；收尾是 `COMMIT` / `ROLLBACK` / `END`（PG 的 `END`
/// 即 `COMMIT`）。
///
/// 首关键字要**跳过注释**：[`split_statements`] 会把语句前的注释留在头部
/// （`-- 说明\nBEGIN` 是常态）。
#[must_use]
pub fn tx_effect(stmt: &str) -> Option<TxEffect> {
    match statement_head(stmt).as_str() {
        "BEGIN" | "START" => Some(TxEffect::Begin),
        "COMMIT" | "ROLLBACK" | "END" => Some(TxEffect::End),
        _ => None,
    }
}

/// 取语句的首个关键字（大写；跳过前导空白与 `--` / `/* */` 注释）。
fn statement_head(stmt: &str) -> String {
    let mut rest = stmt.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
        rest = rest.trim_start();
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// 只读证明：语句**必须解析成功**且 AST 属于读语句族才放行。
/// 解析失败一律不给证明（宁可多弹一次确认，不可漏一次写库）。
#[must_use]
pub fn prove_read_only(stmt: &str) -> bool {
    matches!(
        parse_one(stmt),
        Some(Statement::Query(_))
            | Some(Statement::Explain { .. })
            | Some(Statement::ExplainTable { .. })
            | Some(Statement::ShowVariables { .. })
            | Some(Statement::ShowStatus { .. })
            | Some(Statement::ShowColumns { .. })
            | Some(Statement::ShowTables { .. })
            | Some(Statement::ShowDatabases { .. })
            | Some(Statement::ShowSchemas { .. })
            | Some(Statement::ShowFunctions { .. })
            | Some(Statement::ShowViews { .. })
    )
}

/// 「恒真谓词」判定：`WHERE true` / `WHERE 1` 这类等价于没有 WHERE。
fn always_true(expr: &sqlparser::ast::Expr) -> bool {
    let text = expr.to_string();
    text.trim().eq_ignore_ascii_case("true")
        || text
            .trim()
            .parse::<f64>()
            .map(|v| v != 0.0)
            .unwrap_or(false)
}

/// 单条语句的风险分级。
#[must_use]
pub fn risk_of(stmt: &str) -> SqlRisk {
    match classify(stmt) {
        StmtKind::Read => SqlRisk::ReadOnly,
        StmtKind::Write => {
            // 无 WHERE 或恒真 WHERE 的 UPDATE / DELETE 是全表清改，升一级。
            match parse_one(stmt) {
                Some(Statement::Update { selection, .. }) => match selection {
                    None => SqlRisk::Dangerous,
                    Some(expr) if always_true(&expr) => SqlRisk::Dangerous,
                    Some(_) => SqlRisk::Write,
                },
                Some(Statement::Delete(del)) => match del.selection {
                    None => SqlRisk::Dangerous,
                    Some(expr) if always_true(&expr) => SqlRisk::Dangerous,
                    Some(_) => SqlRisk::Write,
                },
                _ => SqlRisk::Write,
            }
        }
        StmtKind::Ddl => {
            let head = stmt
                .trim_start()
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            if head == "DROP" || head == "TRUNCATE" {
                SqlRisk::Dangerous
            } else {
                SqlRisk::Write
            }
        }
        StmtKind::Other => SqlRisk::Write,
    }
}

/// 一段 SQL（多条语句）的整体风险：取最高档。
#[must_use]
pub fn risk_of_statements(stmts: &[String]) -> SqlRisk {
    stmts
        .iter()
        .map(|stmt| risk_of(stmt))
        .max()
        .unwrap_or(SqlRisk::ReadOnly)
}

/// 一条 SELECT 的「可编辑目标」：结果行与某张表的行是**一对一**时，给出那张表。
///
/// 判定的是「能不能把结果集当表来改」，不是「语句是不是 SELECT」。一条 SELECT 只有在
/// 下列条件全满足时才可编辑：
///
/// - 单表、**没有 JOIN**（多表连出来的行没有唯一归属的表）；
/// - 表因子就是一张**表名**（不是子查询 / 表值函数 / UNNEST / JSON_TABLE / PIVOT …）；
/// - 表**没有别名**（有别名时 `t.col` 里的列名与表列名对不上，改格定位会错表）；
/// - 没有 `DISTINCT` / `GROUP BY` / `HAVING` / `SELECT … INTO` —— 这些会让结果行
///   与表行不再是 1:1（一行结果可能是聚合了多行）；
/// - 投影只允许 `*` / `表.*` / 裸列名：出现函数、表达式、常量时，列名不再等于表列名，
///   按名字定位列会落空或落错。
///
/// 另外还要在**运行时**核一件事（这里给不出答案）：结果集里必须真的带上主键列，
/// 否则定位不了行——由调用方拿投影出来的列名对一遍主键。
///
/// `WITH`（CTE）与集合运算（`UNION` 等）一律不认：它们的行来源已经不可判定。
#[must_use]
pub fn edit_target(stmt: &str) -> Option<EditTarget> {
    let Some(Statement::Query(query)) = parse_one(stmt) else {
        return None;
    };
    if query.with.is_some() {
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if !is_plain_single_table(select) {
        return None;
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_ordinality,
        partitions,
        ..
    } = &select.from[0].relation
    else {
        return None;
    };
    // `args.is_some()` = 表值函数调用（`generate_series(...)` 之类），不是真表。
    if alias.is_some() || args.is_some() || *with_ordinality || !partitions.is_empty() {
        return None;
    }
    match name.0.as_slice() {
        // 裸表名：当前命名空间。
        [table] => Some(EditTarget {
            table: table.value.clone(),
            schema: None,
        }),
        // `schema.table`：带命名空间。
        [schema, table] => Some(EditTarget {
            table: table.value.clone(),
            schema: Some(schema.value.clone()),
        }),
        // `db.schema.table` 及以上：本仓三引擎都不支持（SQLite 是 attached 名 + 表名两段）。
        _ => None,
    }
}

/// [`edit_target`] 的返回值：结果集背后的那张表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    /// 表名（未加引号的原名）。
    pub table: String,
    /// 命名空间（`SELECT * FROM schema.t` 里的 `schema`）。
    pub schema: Option<String>,
}

/// `SELECT` 的「结果行 ↔ 表行 一对一」那一半判定（表因子与投影）。
fn is_plain_single_table(select: &Select) -> bool {
    // `SELECT … INTO new_table`：结果被搬去新表，与源表不再对应。
    if select.into.is_some() {
        return false;
    }
    // 单表、无 JOIN：`FROM a, b` 与 `FROM a JOIN b` 都会给出多个 `from` 项 / 非空 joins。
    if select.from.len() != 1 || !select.from[0].joins.is_empty() {
        return false;
    }
    if !is_ungrouped(select) {
        return false;
    }
    select.projection.iter().all(is_plain_projection)
}

/// 有没有会破坏「一行结果 = 一行表数据」的修饰（去重 / 分组 / 聚合）。
fn is_ungrouped(select: &Select) -> bool {
    if select.distinct.is_some() || select.having.is_some() {
        return false;
    }
    // `GROUP BY ALL` 与 `GROUP BY <expr>…` 都算分组；只有空 `Expressions` 是没分组。
    matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
}

/// 投影项是否是「列名就是表列名」的形态（`*` / `表.*` / 裸列名）。
fn is_plain_projection(item: &SelectItem) -> bool {
    matches!(
        item,
        SelectItem::Wildcard(_)
            | SelectItem::QualifiedWildcard(..)
            | SelectItem::UnnamedExpr(Expr::Identifier(_))
            | SelectItem::ExprWithAlias {
                expr: Expr::Identifier(_),
                ..
            }
    )
}

/// 投影出来的列名是否**覆盖**了定位所需的主键列。
///
/// 这是 [`edit_target`] 判定不了的最后一环：`SELECT name FROM users` 单表、无别名、
/// 无分组，但结果里没有主键列 —— 改了没法定位回哪一行。`SELECT *` 的列名要等结果集
/// 回来才知道，所以这一步必须在运行时按实际列名做。
#[must_use]
pub fn covers_columns(projected: &[String], needed: &[String]) -> bool {
    needed
        .iter()
        .all(|column| projected.iter().any(|name| name == column))
}

/// 重新排版后的 SQL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedSql {
    /// 排版后的文本：多语句之间隔一个空行，各句以 `;` 收尾。
    pub text: String,
    /// 解析不了、被**原样保留**的语句（界面要据此提示「有 N 段没动」）。
    pub kept_verbatim: Vec<String>,
}

/// 把一段 SQL 重新排版：按语句拆分 → 解析 → 换成解析树的规范输出。
///
/// 关键字大小写、空白、逗号与括号间距由 `Statement` 的 `Display` 统一 ——
/// 它同时也是「这段 SQL 能不能被理解」的证明：**解析不了的语句一律原样保留**
/// 并记进 [`FormattedSql::kept_verbatim`]。宁可不动，也不要把一段用户手上跑得通的
/// SQL 改成跑不了的（半截语句、方言特有语法都会走到这条路）。
#[must_use]
pub fn format_sql(sql: &str) -> FormattedSql {
    let mut formatted = Vec::new();
    let mut kept_verbatim = Vec::new();
    for statement in split_statements(sql) {
        match parse_one(&statement) {
            Some(parsed) => formatted.push(parsed.to_string()),
            None => {
                kept_verbatim.push(statement.clone());
                formatted.push(statement);
            }
        }
    }
    FormattedSql {
        text: formatted.join(";\n\n"),
        kept_verbatim,
    }
}

/// 一条语句「出不了执行计划」的原因。文案由界面层决定，内核只给分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainSkip {
    /// 编辑器里不是单条语句（空、多条，或拆出来是空的）。
    NotASingleStatement,
    /// 解析不了，或这个种类本来就没有执行计划（事务控制、`SET`、DDL 等）。
    NoPlanForThisStatement,
}

/// 把一段 SQL 包成一条 `EXPLAIN <stmt>`（SQLite / PostgreSQL / MySQL 的语法一致）。
///
/// 只接受**单条**出得了计划的语句：`EXPLAIN BEGIN` / `EXPLAIN DROP TABLE` 都不是合法
/// 用法。与其把它丢给数据库报一句方言各异的语法错，不如在闸门这一层先分类清楚。
pub fn explain_sql(sql: &str) -> Result<String, ExplainSkip> {
    let statements = split_statements(sql);
    if statements.len() != 1 {
        return Err(ExplainSkip::NotASingleStatement);
    }
    let statement = &statements[0];
    let Some(parsed) = parse_one(statement) else {
        return Err(ExplainSkip::NoPlanForThisStatement);
    };
    match classify(statement) {
        StmtKind::Read | StmtKind::Write => Ok(format!("EXPLAIN {parsed}")),
        StmtKind::Ddl | StmtKind::Other => Err(ExplainSkip::NoPlanForThisStatement),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 事务状态判定：三引擎的开关拼法、前置注释、大小写、以及「无关语句」。
    #[test]
    fn tx_effect_detection() {
        assert_eq!(tx_effect("BEGIN"), Some(TxEffect::Begin));
        assert_eq!(tx_effect("begin transaction"), Some(TxEffect::Begin));
        assert_eq!(
            tx_effect("START TRANSACTION WITH CONSISTENT SNAPSHOT"),
            Some(TxEffect::Begin)
        );
        assert_eq!(tx_effect("COMMIT"), Some(TxEffect::End));
        assert_eq!(tx_effect("  rollback ;"), Some(TxEffect::End));
        assert_eq!(tx_effect("END"), Some(TxEffect::End));
        // 前置注释（`split_statements` 会把它留在语句头部）。
        assert_eq!(tx_effect("-- 收尾\nCOMMIT"), Some(TxEffect::End));
        assert_eq!(tx_effect("/* 块注释 */ BEGIN"), Some(TxEffect::Begin));
        assert_eq!(tx_effect("-- 只有注释"), None);
        // 无关语句：含「以事务词开头」的干扰项。
        assert_eq!(tx_effect("SELECT 1"), None);
        assert_eq!(tx_effect("SAVEPOINT s1"), None);
        assert_eq!(tx_effect("UPDATE t SET a = 'COMMIT'"), None);
    }

    /// 拆分：普通分号、字符串里的分号、注释、结尾无分号。
    #[test]
    fn split_handles_quotes_and_comments() {
        let sql = "SELECT 1; -- 说明; 还有分号\nUPDATE t SET a = 'x;y' WHERE id = 1;\n/* 块;注释 */ INSERT INTO t(a) VALUES ('it''s'); SELECT 2";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "SELECT 1");
        assert!(parts[1].contains("'x;y'"));
        // 块注释保留在语句头部（注释跟着语句走）。
        assert!(parts[2].contains("INSERT INTO t"));
        assert_eq!(parts[3], "SELECT 2");
    }

    /// 分类：读 / 写 / DDL / 无法判定。
    #[test]
    fn classify_kinds() {
        assert_eq!(classify("SELECT * FROM t"), StmtKind::Read);
        assert_eq!(classify("INSERT INTO t VALUES (1)"), StmtKind::Write);
        assert_eq!(classify("UPDATE t SET a = 1 WHERE id = 1"), StmtKind::Write);
        assert_eq!(classify("CREATE TABLE t(id INT)"), StmtKind::Ddl);
        assert_eq!(classify("PRAGMA table_info(t)"), StmtKind::Read);
        assert_eq!(classify("SHOW TABLES"), StmtKind::Read);
    }

    /// 只读证明：SELECT 放行；写与解析失败都不放行。
    #[test]
    fn read_only_proof() {
        assert!(prove_read_only("SELECT * FROM t WHERE id = 1"));
        assert!(prove_read_only("EXPLAIN SELECT 1"));
        assert!(prove_read_only("SHOW TABLES"));
        assert!(!prove_read_only("UPDATE t SET a = 1"));
        assert!(!prove_read_only("DROP TABLE t"));
        // 解析失败（写一半的语句）必须不给证明。
        assert!(!prove_read_only("SELEC * FORM t"));
    }

    /// 风险分级：无 WHERE 升危险档，整体取最高。
    #[test]
    fn risk_grades() {
        assert_eq!(risk_of("SELECT 1"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("DELETE FROM t WHERE id = 1"), SqlRisk::Write);
        assert_eq!(risk_of("DELETE FROM t"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1 WHERE 1"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE t SET a = 1 WHERE id = 2"), SqlRisk::Write);
        assert_eq!(risk_of("DROP TABLE t"), SqlRisk::Dangerous);
        assert_eq!(risk_of("ALTER TABLE t ADD c INT"), SqlRisk::Write);

        let mixed = vec!["SELECT 1".to_owned(), "DROP TABLE t".to_owned()];
        assert_eq!(risk_of_statements(&mixed), SqlRisk::Dangerous);
    }

    /// 可编辑目标：单表裸 SELECT 认得出来；连表 / 聚合 / 别名 / 子查询一律不认。
    #[test]
    fn edit_target_only_for_plain_single_table_selects() {
        let target = |stmt: &str, table: &str, schema: Option<&str>| {
            assert_eq!(
                edit_target(stmt),
                Some(EditTarget {
                    table: table.to_owned(),
                    schema: schema.map(str::to_owned),
                }),
                "应认出可编辑目标：{stmt}"
            );
        };
        let none = |stmt: &str| {
            assert_eq!(edit_target(stmt), None, "不该认出可编辑目标：{stmt}");
        };

        // 认得出来的：通配、裸列名、带别名列（别名只影响展示）、WHERE / ORDER / LIMIT。
        target("SELECT * FROM users", "users", None);
        target("SELECT id, name FROM users WHERE id > 1", "users", None);
        target(
            "SELECT id AS 编号, name FROM users ORDER BY id LIMIT 10",
            "users",
            None,
        );
        target(
            "SELECT public.users.* FROM public.users",
            "users",
            Some("public"),
        );
        target("SELECT id FROM public.users", "users", Some("public"));

        // 连表：行没有唯一归属。
        none("SELECT * FROM a JOIN b ON a.id = b.id");
        none("SELECT * FROM a, b");
        none("SELECT * FROM a LEFT JOIN b ON a.id = b.id");
        // 表因子不是真表。
        none("SELECT * FROM (SELECT * FROM users) x");
        none("SELECT * FROM generate_series(1, 10)");
        none("SELECT * FROM users u");
        // 行数与表行不是一对一。
        none("SELECT DISTINCT name FROM users");
        none("SELECT count(*) FROM users");
        none("SELECT id, count(*) FROM users GROUP BY id");
        none("SELECT id FROM users HAVING id > 1");
        // 投影里有表达式 / 常量：列名不再等于表列名。
        none("SELECT id + 1 FROM users");
        none("SELECT upper(name) FROM users");
        none("SELECT 1 FROM users");
        // CTE / 集合运算 / 非 SELECT。
        none("WITH x AS (SELECT 1) SELECT * FROM x");
        none("SELECT * FROM a UNION SELECT * FROM b");
        none("UPDATE users SET name = 'x'");
        none("SELEC * FORM users");
    }

    /// 主键覆盖：结果集里必须真的带上定位所需的列（`SELECT name FROM t` 改不了）。
    #[test]
    fn edit_requires_the_primary_key_in_the_projection() {
        let projected = vec!["id".to_owned(), "name".to_owned()];
        assert!(covers_columns(&projected, &["id".to_owned()]));
        assert!(covers_columns(&projected, &[]));
        assert!(!covers_columns(&["name".to_owned()], &["id".to_owned()]));
        // 复合主键缺一不可（缺一个就定位不到唯一一行）。
        assert!(!covers_columns(
            &["a".to_owned(), "c".to_owned()],
            &["a".to_owned(), "b".to_owned()]
        ));
    }

    /// 排版只做规范化，绝不改语义：解析得了的换上规范形式，解析不了的**原样留着**。
    #[test]
    fn formatting_normalises_the_parsable_and_keeps_the_rest_verbatim() {
        let tidy = format_sql("select id,name from users where id=1");
        assert_eq!(tidy.text, "SELECT id, name FROM users WHERE id = 1");
        assert!(tidy.kept_verbatim.is_empty(), "这一句应该能解析");

        // 多语句用空行分隔，各自以 `;` 收尾。
        let two = format_sql("select 1;select 2");
        assert_eq!(two.text, "SELECT 1;\n\nSELECT 2");

        // 半截语句 / 方言语法：原样保留，并**列出来**让界面能提示。
        let broken = format_sql("select from where; this is not sql");
        assert_eq!(broken.kept_verbatim.len(), 2);
        assert!(broken.text.contains("this is not sql"));
        assert!(broken.text.contains("select from where"));

        // 空输入不 panic，也不产出内容。
        let empty = format_sql("   ");
        assert_eq!(empty.text, "");
        assert!(empty.kept_verbatim.is_empty());
    }

    /// 排版是幂等的：对已经规范化过的文本再跑一次不变（否则「点两次格式化」会一直抖）。
    #[test]
    fn formatting_is_idempotent() {
        let once = format_sql("select a,b from t where x=1 and y=2").text;
        let twice = format_sql(&once).text;
        assert_eq!(once, twice);
    }

    /// 执行计划只对「单条 + 有计划的语句」成立。
    #[test]
    fn explaining_needs_exactly_one_statement_that_has_a_plan() {
        assert_eq!(
            explain_sql("select 1").as_deref(),
            Ok("EXPLAIN SELECT 1"),
            "读语句应当能被包成 EXPLAIN"
        );
        assert_eq!(
            explain_sql("update t set a = 1").as_deref(),
            Ok("EXPLAIN UPDATE t SET a = 1"),
            "改数据的语句也有计划"
        );
        for (sql, expected) in [
            ("", ExplainSkip::NotASingleStatement),
            ("   ", ExplainSkip::NotASingleStatement),
            ("select 1; select 2", ExplainSkip::NotASingleStatement),
            ("drop table t", ExplainSkip::NoPlanForThisStatement),
            ("begin", ExplainSkip::NoPlanForThisStatement),
            ("this is not sql", ExplainSkip::NoPlanForThisStatement),
        ] {
            assert_eq!(explain_sql(sql), Err(expected), "`{sql}` 的判定不对");
        }
    }
}
