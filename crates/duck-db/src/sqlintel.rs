//! SQL 智能层（纯函数，零 I/O）：语句拆分、分类、风险分级与只读证明。
//!
//! 取向来自 dbx 的 `dbx-sql-core`：这层**只碰 SQL 文本，不碰数据库**，
//! 因此可以被单测钉死，也能被未来的 AI 闸门直接复用——模型产出的每条语句
//! 都必须过 [`prove_read_only`]，写操作必须走确认流程。

use sqlparser::ast::{
    Expr, GroupByExpr, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::config::DatabaseKind;

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

/// 一条完整语句及其在原文中的起始字节偏移。
///
/// `text` 与 [`split_statements`] 的产出同源（trim 过的语句文本）；`start` 指向
/// 语句的**第一个非空白字符**——界面据此把「逐条执行」的图标钉在语句首行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// 语句文本（与 [`split_statements`] 同口径：trim 过）。
    pub text: String,
    /// 语句首字符在原文里的字节偏移。
    pub start: usize,
}

/// [`split_statements`] 的带位置版本：除了语句文本，还回每条语句的起始偏移。
///
/// 扫描逻辑与 [`split_statements`] **逐字对应**（引号 / 两类注释 / 分号切分）——
/// 两份实现必须同改：界面上「第几行有 ▶」与「执行的是哪段文本」必须来自同一次切分，
/// 漂了就是图标指到别的语句上。
#[must_use]
pub fn split_statement_spans(sql: &str) -> Vec<StatementSpan> {
    let mut statements = Vec::new();
    let mut current = String::new();
    // `Some(偏移)` = 当前语句的第一个非空白字符位置；`None` = 还没吃到任何字符。
    let mut current_start: Option<usize> = None;
    let mut chars = sql.char_indices().peekable();
    #[derive(PartialEq)]
    enum In {
        None,
        Single,
        Double,
        Backtick,
        BracketComment,
    }
    let mut state = In::None;

    let mut flush = |current: &mut String, current_start: &mut Option<usize>| {
        if !current.trim().is_empty() {
            let start = current_start.unwrap_or(0);
            let text = current.trim().to_owned();
            statements.push(StatementSpan { text, start });
        }
        current.clear();
        *current_start = None;
    };

    while let Some((offset, c)) = chars.next() {
        match state {
            In::None => {
                if c.is_whitespace() && current_start.is_none() {
                    continue;
                }
                if current_start.is_none() {
                    current_start = Some(offset);
                }
                match c {
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
                    '-' if chars.peek() == Some(&(offset + 1, '-')) => {
                        // 行注释：吃掉直到换行（换行本身保留，让错误定位的行号不漂移）。
                        current.push(c);
                        while let Some(&(_, n)) = chars.peek() {
                            chars.next();
                            current.push(n);
                            if n == '\n' {
                                break;
                            }
                        }
                    }
                    '/' if chars.peek() == Some(&(offset + 1, '*')) => {
                        state = In::BracketComment;
                        current.push(c);
                    }
                    ';' => {
                        flush(&mut current, &mut current_start);
                    }
                    _ => current.push(c),
                }
            }
            In::Single | In::Double | In::Backtick => {
                current.push(c);
                if c == '\\' && state == In::Single {
                    // MySQL 风格的反斜杠转义：原样带过下一个字符。
                    if let Some(&(_, n)) = chars.peek() {
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
                    if chars.peek() == Some(&(offset + 1, closing)) {
                        current.push(chars.next().expect("peeked char must exist").1);
                    } else {
                        state = In::None;
                    }
                }
            }
            In::BracketComment => {
                current.push(c);
                if c == '*' && chars.peek() == Some(&(offset + 1, '/')) {
                    current.push(chars.next().expect("peeked char must exist").1);
                    state = In::None;
                }
            }
        }
    }
    flush(&mut current, &mut current_start);
    statements
}

/// 解析一条语句；解析失败按 `Other` 处理（调用方走保守路径）。
fn parse_one(stmt: &str) -> Option<Statement> {
    Parser::parse_sql(&GenericDialect {}, stmt)
        .ok()
        .and_then(|mut ast| if ast.len() == 1 { ast.pop() } else { None })
}

/// 解析失败路径兜底用的词法标记：只区分「词」与「左括号」，标点折叠成 `Other`。
///
/// 目的是把注释与**字符串字面量**排除在外（见 [`tokenize`]）——`SELECT 'DELETE' FROM t`
/// 里的 `DELETE` 是数据，不是关键字。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// 一段 ASCII 字母 / 数字 / 下划线（已大写）。
    Word(String),
    /// `(`（用来判「名字后面紧跟左括号」= 函数调用）。
    LParen,
    /// 其它标点（会打断「名字 + 左括号」的相邻关系）。
    Other,
}

/// 把语句切成词法标记：跳过 `'…'` / `"…"` / `` `…` `` 字面量与 `--` / `/* */` 注释。
///
/// 只做**词法**，不做语法——解析器在这条路上已经失败，兜底只能用最粗的信号。
/// 逐字节扫描对 UTF-8 安全：多字节字符的任何字节都不会等于 ASCII 的引号 / 注释符号。
fn tokenize(stmt: &str) -> Vec<Tok> {
    let bytes = stmt.as_bytes();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut i = 0;
    // 词缓冲落的收尾统一走它。
    macro_rules! flush {
        () => {
            if !word.is_empty() {
                tokens.push(Tok::Word(std::mem::take(&mut word)));
            }
        };
    }
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' | b'`' => {
                flush!();
                i = skip_quoted(bytes, i, b);
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                flush!();
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                flush!();
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b if b.is_ascii_alphanumeric() || b == b'_' => {
                word.push((b as char).to_ascii_uppercase());
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                flush!();
                i += 1;
            }
            b'(' => {
                flush!();
                tokens.push(Tok::LParen);
                i += 1;
            }
            _ => {
                flush!();
                tokens.push(Tok::Other);
                i += 1;
            }
        }
    }
    flush!();
    tokens
}

/// 跳过一段引号字面量，返回**下一个**字节的下标。
///
/// 规则与 [`split_statements`] 同源：单引号里 `\x` 是转义；双写引号（`''` / `""`）
/// 也是转义，字面量不在这里结束。
fn skip_quoted(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && quote == b'\'' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// 判定写 / DDL 的关键字（大写比较）。出现在语句**任意位置**即视为写信号——
/// 兜底路径不做语法，只看词法。
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "UPSERT", "TRUNCATE", "DROP", "ALTER",
    "CREATE", "RENAME", "GRANT", "REVOKE", "VACUUM", "REINDEX", "LOAD", "CALL", "COPY", "ATTACH",
    "DETACH",
];

/// 标记 `FOR [NO KEY | KEY] UPDATE | FOR SHARE` 锁定子句占用的词。
///
/// 这些词里的 `UPDATE` / `SHARE` **不是** DML 写：`SELECT … FOR UPDATE` 只是取行锁。
/// 剔除它们，兜底的写关键字扫描才不会把取锁的 SELECT 误判成写。返回与 `tokens` 等长的位图。
fn locking_mask(tokens: &[Tok]) -> Vec<bool> {
    let mut mask = vec![false; tokens.len()];
    for i in 0..tokens.len() {
        if !matches!(&tokens[i], Tok::Word(w) if w == "FOR") {
            continue;
        }
        let mut k = i + 1;
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "NO") {
            k += 1;
        }
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "KEY") {
            k += 1;
        }
        if matches!(tokens.get(k), Some(Tok::Word(w)) if w == "UPDATE" || w == "SHARE") {
            mask[i..=k].fill(true);
        }
    }
    mask
}

/// 取语句的词序列（大写），**剔除锁定子句里的 `UPDATE` / `SHARE`**（见 [`locking_mask`]）。
fn scan_words(stmt: &str) -> Vec<String> {
    let tokens = tokenize(stmt);
    let locked = locking_mask(&tokens);
    tokens
        .into_iter()
        .enumerate()
        .filter_map(|(i, tok)| match tok {
            Tok::Word(w) if !locked[i] => Some(w),
            _ => None,
        })
        .collect()
}

/// 语句是否含 `SELECT … FOR UPDATE / SHARE` 之类的**取锁**子句（词法判定，覆盖
/// `FOR NO KEY UPDATE` / `FOR KEY SHARE` —— 这两种 GenericDialect 解析不了）。
fn has_locking_clause(tokens: &[Tok]) -> bool {
    locking_mask(tokens).into_iter().any(|locked| locked)
}

/// 解析失败路径的兜底风险：扫描整条的词法标记（跳过字符串 / 注释、剔除取锁子句）。
///
/// - 扫不到任何写关键字 → `None`（交给上层，`Other` 仍按保守处理）；
/// - 出现 `DROP` / `TRUNCATE` → `Dangerous`；
/// - 出现**没有 `WHERE` 的** `DELETE` / `UPDATE` → `Dangerous`（全表清改）；
/// - 其余出现写关键字的形态 → `Write`。
///
/// ⚠️ 这是**词法**兜底，不做语法：`WHERE 1 = 1` 这类恒真条件识别不了（会被当有 WHERE）。
/// 宁可多报危险（fail-closed），不可漏报。
fn keyword_risk(stmt: &str) -> Option<SqlRisk> {
    let words = scan_words(stmt);
    if !words.iter().any(|w| WRITE_KEYWORDS.contains(&w.as_str())) {
        return None;
    }
    if words.iter().any(|w| w == "DROP" || w == "TRUNCATE") {
        return Some(SqlRisk::Dangerous);
    }
    for dml in ["DELETE", "UPDATE"] {
        if let Some(pos) = words.iter().position(|w| w == dml) {
            // 该 DML 之后没有 WHERE → 全表清改（更靠前的 WHERE 是别处的条件，不算）。
            if !words[pos..].iter().any(|w| w == "WHERE") {
                return Some(SqlRisk::Dangerous);
            }
        }
    }
    Some(SqlRisk::Write)
}

/// 按首关键字猜测类别；给解析器覆盖不到的方言词（MySQL 的 `LOCK TABLES` 之类）兜底。
///
/// ⚠️ **这里只做「解析失败时的保守兜底」**：`SELECT` / `WITH` 头一旦解析失败**绝不**
/// 直接判读——`WITH x AS (...) DELETE …`、`SELECT … INTO OUTFILE` 这类写构造正是解析
/// 不了的形态。改为**扫描整条的词法标记**：扫到写关键字（`DELETE` / `UPDATE` / `INSERT`
/// / `TRUNCATE` / `DROP` …）就至少算写。PRAGMA 另走 [`pragma_fallback_kind`]。
fn keyword_kind(stmt: &str) -> StmtKind {
    let words = scan_words(stmt);
    let head = words.first().map(String::as_str).unwrap_or_default();
    match head {
        // 只保留「形态上就是读」的词：SHOW / DESCRIBE / USE 不出写；PRAGMA 单独判。
        "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "USE" => StmtKind::Read,
        "PRAGMA" => pragma_fallback_kind(stmt),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "UPSERT" | "LOAD" | "CALL"
        | "COPY" => StmtKind::Write,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "VACUUM"
        | "ANALYZE" | "REINDEX" | "LOCK" | "UNLOCK" | "SET" | "BEGIN" | "START" | "COMMIT"
        | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => StmtKind::Ddl,
        _ => {
            // 头部不是写词（`WITH` / `SELECT` / 乱码）：扫整条，扫到写关键字即算写。
            if words.iter().any(|w| WRITE_KEYWORDS.contains(&w.as_str())) {
                StmtKind::Write
            } else {
                StmtKind::Other
            }
        }
    }
}

/// 「带参数也只读」的 PRAGMA 白名单（小写比较）。
///
/// 只放**取信息**型：`table_info` / `index_list` 这类；像 `journal_mode` / `synchronous`
/// / `encoding` 这类「有参即写」的**不放**——它们带参时会被判写。
const PRAGMA_READERS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "collation_list",
    "compile_options",
    "pragma_list",
    "function_list",
    "module_list",
    "integrity_check",
    "quick_check",
    "page_count",
    "freelist_count",
];

/// 「**无值时**只读」的 PRAGMA 白名单（小写比较）。
///
/// 覆盖 SQLite 常见的「无值取读、有值判写」型（`journal_mode` / `synchronous` / `encoding`
/// 等）。⚠️ 显式**不含**维护 / 写入型——`wal_checkpoint`、`optimize`、`shrink_memory`、
/// `incremental_vacuum` 这类**无值也写**，必须留白发证明。宁可少放（fail-closed）。
const PRAGMA_BARE_READERS: &[&str] = &[
    "user_version",
    "schema_version",
    "application_id",
    "journal_mode",
    "synchronous",
    "encoding",
    "page_size",
    "cache_size",
    "mmap_size",
    "auto_vacuum",
    "locking_mode",
    "foreign_keys",
    "recursive_triggers",
    "temp_store",
    "busy_timeout",
    "read_uncommitted",
    "secure_delete",
    "wal_autocheckpoint",
    "journal_size_limit",
    "cache_spill",
    "max_page_count",
    "soft_heap_limit",
    "threads",
    "data_version",
    "legacy_file_format",
    "defer_foreign_keys",
    "count_changes",
    "reverse_unordered_selects",
    "ignore_check_constraints",
    "trusted_schema",
    "query_only",
    "cell_size_check",
    "checkpoint_fullfsync",
    "fullfsync",
];

/// PRAGMA 名（取 `schema.pragma` 里的最后一段，小写）。
fn pragma_last_name(name: &sqlparser::ast::ObjectName) -> String {
    name.0
        .last()
        .map(|ident| ident.value.to_ascii_lowercase())
        .unwrap_or_default()
}

/// PRAGMA 名是否属于「无值只读」白名单（[`PRAGMA_READERS`] ∪ [`PRAGMA_BARE_READERS`]）。
fn pragma_name_reads(name: &str) -> bool {
    PRAGMA_READERS.contains(&name) || PRAGMA_BARE_READERS.contains(&name)
}

/// 解析器认不出的 PRAGMA（参数是标识符的形态，如 `PRAGMA table_info(t)` /
/// `PRAGMA journal_mode = WAL`）的保守分类。
///
/// 规则（fail-closed）：`=` 赋值形态一律写；带参形态只有落在 [`PRAGMA_READERS`] 白名单里
/// 才判读（其余如 `PRAGMA journal_mode(WAL)` 判写）；无参形态放宽到 [`pragma_name_reads`]。
/// 可解析的 `PRAGMA name [= v | (v)]` 走 [`classify`] 里的 `Statement::Pragma` 分支。
fn pragma_fallback_kind(stmt: &str) -> StmtKind {
    let upper = stmt.trim_start().to_ascii_uppercase();
    let rest = upper.strip_prefix("PRAGMA").unwrap_or(&upper).trim_start();
    if rest.contains('=') {
        return StmtKind::Write;
    }
    let name_end = rest
        .find(['(', ' ', '\t', '\n', '\r'])
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_ascii_lowercase();
    let reads = if rest.contains('(') {
        PRAGMA_READERS.contains(&name.as_str())
    } else {
        pragma_name_reads(&name)
    };
    if reads {
        StmtKind::Read
    } else {
        StmtKind::Write
    }
}

/// 一条 `SELECT` / `Query`（含 CTE、集合运算、派生表）是否**可证明只读**。
///
/// 数据修改型 CTE（PG 的 `WITH x AS (INSERT/UPDATE … RETURNING *) …`）、
/// `SELECT … INTO new_table` / `INTO OUTFILE`、以及派生表里嵌的 DML，全部不放行。
fn query_is_read_only(query: &Query) -> bool {
    let ctes_read_only = query.with.as_ref().is_none_or(|with| {
        with.cte_tables
            .iter()
            .all(|cte| query_is_read_only(&cte.query))
    });
    ctes_read_only && set_expr_is_read_only(&query.body)
}

/// `SetExpr`（SELECT / 括号子查询 / 集合运算 / `VALUES` / DML 体）是否只读。
fn set_expr_is_read_only(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select_is_read_only(select),
        SetExpr::Query(inner) => query_is_read_only(inner),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_is_read_only(left) && set_expr_is_read_only(right)
        }
        SetExpr::Values(_) | SetExpr::Table(_) => true,
        // 数据修改型 CTE 体：`WITH x AS (INSERT/UPDATE … RETURNING *) …`。
        SetExpr::Insert(_) | SetExpr::Update(_) => false,
    }
}

/// 一条 `SELECT` 本体是否只读：`INTO` 目标、以及 `FROM` 里的派生表都不能藏写。
fn select_is_read_only(select: &Select) -> bool {
    // `SELECT … INTO new_table` / `INTO OUTFILE`：结果被搬去别处，不是纯读。
    select.into.is_none() && select.from.iter().all(table_with_joins_is_read_only)
}

/// 一个 `FROM` 项（表 + JOIN）及 JOIN 的表因子是否只读。
fn table_with_joins_is_read_only(item: &TableWithJoins) -> bool {
    table_factor_is_read_only(&item.relation)
        && item
            .joins
            .iter()
            .all(|join| table_factor_is_read_only(&join.relation))
}

/// 一个表因子是否只读：派生表 / 嵌套 JOIN / PIVOT 壳里的子查询要递归检查，
/// 普通表名、表值函数、`UNNEST` 等不带内嵌 DML，直接放行。
fn table_factor_is_read_only(factor: &TableFactor) -> bool {
    match factor {
        TableFactor::Derived { subquery, .. } => query_is_read_only(subquery),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => table_with_joins_is_read_only(table_with_joins),
        TableFactor::Pivot { table, .. }
        | TableFactor::Unpivot { table, .. }
        | TableFactor::MatchRecognize { table, .. } => table_factor_is_read_only(table),
        _ => true,
    }
}

/// 语句是否**可证明只读**（AST 层面的判据，[`classify`] 与 [`prove_read_only`] 共用）。
///
/// `EXPLAIN` 的口径是 fail-closed：只有**内层语句本身可证明只读**才放行（`EXPLAIN
/// SELECT` / `EXPLAIN ANALYZE SELECT` 通过；`EXPLAIN DELETE` / `EXPLAIN ANALYZE
/// DELETE` 不通过）。带 `ANALYZE` 会真的执行内层，必须拦；不带 `ANALYZE` 时是否执行
/// 随引擎/方言漂移，一并从严。
fn statement_is_read_only(stmt: &Statement) -> bool {
    match stmt {
        Statement::Query(query) => query_is_read_only(query),
        // `EXPLAIN ANALYZE <stmt>` 会**真的执行**内层语句；而不带 `ANALYZE` 时是否执行
        // 由引擎决定（Postgres/MySQL/SQLite 不执行，但语法与行为随方言漂移）。这里取
        // fail-closed 口径：**只有内层本身可证明只读**的 `EXPLAIN` 才放行——`EXPLAIN
        // DELETE` 一律不自动执行（交人工），`EXPLAIN SELECT` / `EXPLAIN ANALYZE SELECT`
        // 照常放行。
        Statement::Explain { statement, .. } => statement_is_read_only(statement),
        // 赋值 / 带值形态（`PRAGMA n = v` / `PRAGMA n(v)`）：只有「带参也只读」的白名单
        // 才放行（其余判写）；无值形态也要过白名单——`wal_checkpoint` / `optimize` /
        // `shrink_memory` 这些**无值也写**，不能一律放行。
        Statement::Pragma { name, value, .. } => {
            let name = pragma_last_name(name);
            if value.is_some() {
                PRAGMA_READERS.contains(&name.as_str())
            } else {
                pragma_name_reads(&name)
            }
        }
        Statement::ExplainTable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowViews { .. } => true,
        _ => false,
    }
}

/// 分类一条语句。
#[must_use]
pub fn classify(stmt: &str) -> StmtKind {
    match parse_one(stmt) {
        // 读 / 写由 AST 全量判定：CTE 里的 DML、`SELECT … INTO`、`EXPLAIN ANALYZE <写>`
        // 都会被 [`statement_is_read_only`] 判成非只读，落到 Write。
        Some(
            parsed @ (Statement::Query(_) | Statement::Explain { .. } | Statement::Pragma { .. }),
        ) => {
            if statement_is_read_only(&parsed) {
                StmtKind::Read
            } else {
                StmtKind::Write
            }
        }
        Some(Statement::ExplainTable { .. })
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

/// 只读证明：语句**必须解析成功**且 AST 属于读语句族才放行。
/// 解析失败一律不给证明（宁可多弹一次确认，不可漏一次写库）。
#[must_use]
pub fn prove_read_only(stmt: &str) -> bool {
    parse_one(stmt).is_some_and(|parsed| statement_is_read_only(&parsed))
}

/// 已知会**产生副作用**的函数（大写比较；与 [`tokenize`] 的输出一致）。
///
/// ⚠️ **这是可扩充的「已知集合」，不是完备证明**：这里挡住的是最常被拿来做「借只读之
/// 名行写库之实」的一批（序列推进、跨库、会话 / 后端控制、大对象、配置写入、取锁）。
/// 未列出的函数若有副作用，`allow_auto_execute` 仍会放行——它的定位是「常规 SELECT 的
/// 自动执行闸门」，不是「无副作用的形式化证明」。要更严就继续往这张表里加。
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    // 序列推进 / 回置（`nextval` 一调即持久改序列状态）。
    "NEXTVAL",
    "SETVAL",
    // 跨库 / 远程执行。
    "DBLINK",
    "DBLINK_EXEC",
    "DBLINK_CONNECT",
    "DBLINK_CONNECT_U",
    "DBLINK_DISCONNECT",
    "DBLINK_GET_CONNECTION",
    "DBLINK_GET_RESULT",
    "DBLINK_OPEN",
    "DBLINK_FETCH",
    "DBLINK_CLOSE",
    "DBLINK_CANCEL",
    // 后端 / 会话控制。
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_LOG_BACKEND_MEMORY_CONTEXTS",
    "SET_CONFIG",
    // 大对象（写入 / 删除服务器端对象）。
    "LO_UNLINK",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_PUT",
    "LO_TRUNCATE",
    "LO_CREAT",
    "LO_CREATE",
    // 建议锁（取锁同样阻塞他人）。
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_LOCK_SHARED",
    "PG_ADVISORY_XACT_LOCK",
    "PG_ADVISORY_XACT_LOCK_SHARED",
    "PG_TRY_ADVISORY_LOCK",
    "PG_TRY_ADVISORY_LOCK_SHARED",
    "PG_TRY_ADVISORY_XACT_LOCK",
    "PG_TRY_ADVISORY_XACT_LOCK_SHARED",
    "GET_LOCK",
    "RELEASE_LOCK",
];

/// 语句是否带**已知副作用**：命中 [`SIDE_EFFECT_FUNCTIONS`]（名字后紧跟 `(`），
/// 或含 `FOR UPDATE / SHARE` 之类的取锁子句。
///
/// 词法判定（不做语法）：`allow_auto_execute` 与内核 `execute_read_only` 共用的
/// 「读形状但别自动跑」判据。解析不了的语句也不会误放行——它们本就过不了只读证明。
#[must_use]
pub fn has_write_side_effects(stmt: &str) -> bool {
    let tokens = tokenize(stmt);
    let calls_side_effect_fn = tokens.windows(2).any(|pair| match &pair[0] {
        Tok::Word(word) => SIDE_EFFECT_FUNCTIONS.contains(&word.as_str()) && pair[1] == Tok::LParen,
        _ => false,
    });
    calls_side_effect_fn || has_locking_clause(&tokens)
}

/// 「这段 SQL 能否**自动执行**」的唯一判据（`db.rs` 的 AI 闸门与内核
/// [`crate::drivers::Connection::execute_read_only`] 同源）：**恰好一条**语句、
/// 能被证明只读、且**不带已知副作用**（[`has_write_side_effects`]）。
///
/// 抽成纯函数是为了可单测：越权构造（数据修改型 CTE、`SELECT … INTO`、
/// `EXPLAIN ANALYZE <写>`、赋值型 PRAGMA / `INTO OUTFILE`）、带副作用的读
/// （`nextval` / `setval` / `dblink*` / `pg_terminate_backend` / `SET_CONFIG` …）、
/// 以及取锁读（`SELECT … FOR UPDATE / SHARE`）都必须为 `false`。
///
/// ⚠️ **边界（如实声明）**：副作用判定只覆盖 [`SIDE_EFFECT_FUNCTIONS`] 这张**已知集合**，
/// 以及取锁子句。**未知函数的副作用不在此列**——所以它是「常规 SELECT 的自动执行闸门」，
/// 不是「这条语句绝对无副作用」的完备证明。要收紧就扩充那张表。
#[must_use]
pub fn allow_auto_execute(sql: &str) -> bool {
    let statements = split_statements(sql);
    statements.len() == 1
        && prove_read_only(&statements[0])
        && !has_write_side_effects(&statements[0])
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
                Some(Statement::Update { selection, .. }) => {
                    if selection.as_ref().is_none_or(always_true) {
                        SqlRisk::Dangerous
                    } else {
                        SqlRisk::Write
                    }
                }
                Some(Statement::Delete(del)) => {
                    if del.selection.as_ref().is_none_or(always_true) {
                        SqlRisk::Dangerous
                    } else {
                        SqlRisk::Write
                    }
                }
                // 解析不了（`WITH x AS (…) DELETE …` 之流）：词法兜底——扫到无 WHERE 的
                // DML 就判危险，别让 CTE 包裹的全表删躲过「执行危险 SQL」确认。
                _ => keyword_risk(stmt).unwrap_or(SqlRisk::Write),
            }
        }
        StmtKind::Ddl => {
            let head = scan_words(stmt);
            let head = head.first().map(String::as_str).unwrap_or_default();
            if head == "DROP" || head == "TRUNCATE" {
                SqlRisk::Dangerous
            } else {
                keyword_risk(stmt).unwrap_or(SqlRisk::Write)
            }
        }
        // 解析失败的形态：词法兜底（`WITH … DELETE` 落这里）。
        StmtKind::Other => keyword_risk(stmt).unwrap_or(SqlRisk::Write),
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
    explain_target(sql).map(|parsed| format!("EXPLAIN {parsed}"))
}

/// 按**引擎**包一条能拿到结构化输出的执行计划语句。
///
/// 与 [`explain_sql`] 的分工：那条给的是"各引擎都认的通用形态"（`EXPLAIN <stmt>`，
/// 输出是一坨文本）；这条按引擎挑出**能结构化**的形态，交给 [`crate::plan::parse_plan`]
/// 去解析：
///
/// - SQLite：`EXPLAIN QUERY PLAN`（四列 `id/parent/detail`，层级是真的）
/// - PostgreSQL：`EXPLAIN (FORMAT JSON)`（`Plan` / `Plans` 嵌套）
/// - MySQL：`EXPLAIN FORMAT=JSON`（`query_block` / `table` 嵌套）
/// - ClickHouse：`EXPLAIN`（缩进文本；`json = 1` 只有较新版本认，不拿它赌）
///
/// 其余三条引擎（SQL Server / MongoDB / Elasticsearch）没有这些形态，退回通用写法——
/// 由数据库自己报错，胜过在这里编一个不存在的语法。
///
/// ⚠️ 语法正确性由**语句本身**保证（校验与 [`explain_sql`] 完全同一条路：单条 + 属于
/// 读 / 写族），这里只改前缀。
pub fn explain_sql_for(kind: DatabaseKind, sql: &str) -> Result<String, ExplainSkip> {
    let parsed = explain_target(sql)?;
    Ok(match kind {
        DatabaseKind::Sqlite => format!("EXPLAIN QUERY PLAN {parsed}"),
        DatabaseKind::Postgres => format!("EXPLAIN (FORMAT JSON) {parsed}"),
        DatabaseKind::Mysql => format!("EXPLAIN FORMAT=JSON {parsed}"),
        DatabaseKind::ClickHouse
        | DatabaseKind::SqlServer
        | DatabaseKind::Mongo
        | DatabaseKind::Elasticsearch => format!("EXPLAIN {parsed}"),
    })
}

/// 归一化输出缓冲：负责「连续空白只留一个」与「引号外一律折叠为小写」两条规则。
///
/// 抽成一个小结构体而不是在扫描循环里内联：吐字符的地方有六七处，每处都手写一遍
/// 「先补空格再收字符」迟早会漏一处，而漏了只表现为「指纹偶尔对不上」——最难查的那种。
struct FingerprintBuilder {
    out: String,
    /// 上一个被扫描到的字符是空白 / 注释结束：下一个字符前要先补一个空格。
    pending_space: bool,
}

impl FingerprintBuilder {
    fn new(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            pending_space: false,
        }
    }

    /// 原样收（引号**内**的标识符：`"MyCol"` 与 `"mycol"` 是两列，不能合并）。
    fn verbatim(&mut self, c: char) {
        self.flush_space();
        self.out.push(c);
    }

    /// 折叠大小写后收（引号外的标识符与运算符：关键字大小写是风格，不是语义）。
    fn folded(&mut self, c: char) {
        self.flush_space();
        for lower in c.to_lowercase() {
            self.out.push(lower);
        }
    }

    /// 记一个「这里是空白」的信号（不立刻吐空格，等真有字符要吐时再补）。
    fn note_space(&mut self) {
        self.pending_space = true;
    }

    fn flush_space(&mut self) {
        if self.pending_space && !self.out.is_empty() {
            self.out.push(' ');
        }
        self.pending_space = false;
    }

    /// 收工：去掉尾部空白（`trim_end` 不能直接用在 `String` 的 `self` 上，这里显式做）。
    fn finish(mut self) -> String {
        self.flush_space();
        self.out.trim_end().to_owned()
    }
}

/// 语句的**归一化文本**：把「每次跑都不一样的部分」抹平，留下语句的形状。
///
/// 抹掉的东西：字面量（`'…'`）、数字、注释、多余空白、以及引号外的大小写。
/// 留下的东西：**双引号 / 反引号里的标识符原样保留**（`"MyCol"` 与 `"mycol"` 是两列，
/// 不能合并），以及运算符与括号结构。
///
/// 为什么值得做：审计里「同一类语句跑了多少次」比「每条 SQL 的原文」有用得多，
/// 而把字面量换成 `?` 之后，`WHERE id = 1` 与 `WHERE id = 2` 才算同一类。
///
/// ⚠️ **刻意不做的三件事**：① 不做语法解析（审计要能记下写坏的 SQL，解析器会整段失败）；
/// ② 不展开 `IN (1,2,3)` 与 `IN (1,2)` 的差别（元素个数留在形状里 —— 它们确实是两种计划）；
/// ③ 双引号按**标识符**而不是字符串处理（PG 里 `"x"` 是列名；MySQL 的字符串一律走单引号）。
#[must_use]
pub fn normalize_for_fingerprint(sql: &str) -> String {
    /// 扫描状态。`Single`/`Double`/`Backtick` 是三种引号，`Line`/`Block` 是两类注释，
    /// `Number` 是数字字面量（连续吃掉，整体只记一个 `?`）。
    #[derive(PartialEq)]
    enum In {
        None,
        Single,
        Double,
        Backtick,
        Line,
        Block,
        Number,
    }

    let mut built = FingerprintBuilder::new(sql.len());
    let mut chars = sql.chars().peekable();
    let mut state = In::None;

    while let Some(c) = chars.next() {
        match state {
            In::None => match c {
                '\'' => {
                    built.folded('?');
                    state = In::Single;
                }
                // 双引号 / 反引号里的内容**原样收**（标识符），所以连引号一起吐。
                '"' | '`' => {
                    built.verbatim(c);
                    state = if c == '"' { In::Double } else { In::Backtick };
                }
                '-' if chars.peek() == Some(&'-') => {
                    chars.next();
                    state = In::Line;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = In::Block;
                }
                c if c.is_ascii_digit() => {
                    built.folded('?');
                    state = In::Number;
                }
                c if c.is_whitespace() => built.note_space(),
                c => built.folded(c),
            },
            In::Single => {
                // `''` 是转义的单引号，不是字符串结束。
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        state = In::None;
                    }
                }
            }
            In::Double | In::Backtick => {
                built.verbatim(c);
                let closer = if state == In::Double { '"' } else { '`' };
                if c == closer {
                    // 连着两个同样的引号 = 转义，仍在引号内。
                    if chars.peek() == Some(&closer) {
                        if let Some(next) = chars.next() {
                            built.verbatim(next);
                        }
                    } else {
                        state = In::None;
                    }
                }
            }
            In::Line => {
                if c == '\n' {
                    // 换行结束注释，也是**词与词之间的分隔**：`a--x\nb` 是 `a b`。
                    built.note_space();
                    state = In::None;
                }
            }
            In::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    // 注释替换成一个空白，免得把两边粘成一个词（`a/*x*/b` 是 `a b`）。
                    built.note_space();
                    state = In::None;
                }
            }
            In::Number => {
                if c.is_ascii_alphanumeric() || c == '.' {
                    // `1e5` / `1.5` / `0x1f` 都还是一个数字，继续吃。
                } else if c.is_whitespace() {
                    built.note_space();
                    state = In::None;
                } else {
                    // 数字后面紧跟运算符（`1+1` → `?+?`）：运算符照常收。
                    state = In::None;
                    built.folded(c);
                }
            }
        }
    }
    built.finish()
}

/// 语句指纹：归一化文本的 64 位 FNV-1a，输出 16 位十六进制。
///
/// 空串（或全是注释 / 空白）得空串 —— 「没有语句形状」不该与「某个形状」撞成一个值。
#[must_use]
pub fn fingerprint(sql: &str) -> String {
    let normalized = normalize_for_fingerprint(sql);
    if normalized.is_empty() {
        return String::new();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// [`explain_sql`] / [`explain_sql_for`] 共用的闸门：单条 + 出得了计划的语句。
fn explain_target(sql: &str) -> Result<Statement, ExplainSkip> {
    let statements = split_statements(sql);
    if statements.len() != 1 {
        return Err(ExplainSkip::NotASingleStatement);
    }
    let statement = &statements[0];
    let Some(parsed) = parse_one(statement) else {
        return Err(ExplainSkip::NoPlanForThisStatement);
    };
    match classify(statement) {
        StmtKind::Read | StmtKind::Write => Ok(parsed),
        StmtKind::Ddl | StmtKind::Other => Err(ExplainSkip::NoPlanForThisStatement),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 归一化：抹掉字面量 / 数字 / 注释 / 空白 / 引号外大小写，**保留**标识符原文。
    #[test]
    fn fingerprint_normalization_keeps_the_shape_only() {
        assert_eq!(
            normalize_for_fingerprint("SELECT * FROM t WHERE id = 1"),
            "select * from t where id = ?"
        );
        // 字符串字面量（含转义的单引号）整体变一个 `?`。
        assert_eq!(
            normalize_for_fingerprint("UPDATE t SET a = 'it''s 1' WHERE id = 42"),
            "update t set a = ? where id = ?"
        );
        // 缩进 / 换行 / 块注释不影响形状。
        assert_eq!(
            normalize_for_fingerprint("SELECT\n    a,\n    b\n  FROM t /* 说明 */ WHERE a = 3"),
            "select a, b from t where a = ?"
        );
        // 行注释到行尾为止，且后面的词还在。
        assert_eq!(
            normalize_for_fingerprint("SELECT 1 -- 随手记\nFROM t"),
            "select ? from t"
        );
        // 注释把两边隔开（`a/*x*/b` 是 `a b`，不是 `ab`）。
        assert_eq!(normalize_for_fingerprint("a/*x*/b"), "a b");
        assert_eq!(normalize_for_fingerprint("a--x\nb"), "a b");
        // 数字字面量的各种写法都吃成一个 `?`。
        assert_eq!(
            normalize_for_fingerprint("SELECT 1e5, 1.5, 0x1f"),
            "select ?, ?, ?"
        );
        // 数字紧跟运算符：运算符留在形状里。
        assert_eq!(normalize_for_fingerprint("SELECT 1+2"), "select ?+?");
        // 标识符原样保留大小写（两列就是两列），关键字则折叠。
        assert_eq!(
            normalize_for_fingerprint("SELECT \"MyCol\" FROM \"MyTable\""),
            "select \"MyCol\" from \"MyTable\""
        );
        assert_eq!(
            normalize_for_fingerprint("SELECT `MyCol`"),
            "select `MyCol`"
        );
        // 转义的双引号不结束标识符。
        assert_eq!(
            normalize_for_fingerprint("SELECT \"a\"\"b\" FROM t"),
            "select \"a\"\"b\" from t"
        );
        // 引号里的内容不会被当成分隔符 / 注释。
        assert_eq!(
            normalize_for_fingerprint("SELECT 'a -- b', 'c /* d */' FROM t"),
            "select ?, ? from t"
        );
        assert_eq!(normalize_for_fingerprint("   \n  "), "");
        assert_eq!(normalize_for_fingerprint("-- 只有注释"), "");
    }

    /// 指纹：同一类语句同值、结构不同则不同、空输入给空串。
    #[test]
    fn fingerprints_group_the_same_statement_shape() {
        let select_one = fingerprint("SELECT * FROM t WHERE id = 1");
        assert_eq!(select_one, fingerprint("select * from t where id = 2"));
        assert_eq!(
            select_one,
            fingerprint("\n  SELECT *\n  FROM t\n  WHERE id = 999999  ")
        );
        assert_eq!(select_one.len(), 16, "指纹是 16 位十六进制");
        assert!(select_one.chars().all(|c| c.is_ascii_hexdigit()));

        // 结构不同 → 不同（表格不同、列不同、语句种类不同）。
        assert_ne!(select_one, fingerprint("SELECT * FROM other WHERE id = 1"));
        assert_ne!(select_one, fingerprint("SELECT * FROM t WHERE name = 'x'"));
        assert_ne!(select_one, fingerprint("DELETE FROM t WHERE id = 1"));
        // 标识符大小写是**两个不同的对象**，不该被并成一条。
        assert_ne!(
            fingerprint("SELECT \"A\" FROM t"),
            fingerprint("SELECT \"a\" FROM t")
        );
        // `IN` 的元素个数留在形状里（确实是两种计划）。
        assert_ne!(
            fingerprint("SELECT * FROM t WHERE id IN (1,2,3)"),
            fingerprint("SELECT * FROM t WHERE id IN (1,2)")
        );

        // 空 / 只有注释 → 空串（「没有形状」不是一个形状）。
        assert_eq!(fingerprint(""), "");
        assert_eq!(fingerprint("/* nothing */"), "");
    }

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

    /// 带位置切分：文本与 [`split_statements`] 同口径，偏移落在语句首字符。
    #[test]
    fn split_spans_match_texts_and_offsets() {
        let sql =
            "\n  SELECT 1;\n\n  update t set a = 'x;y' where id = 1;\n\n-- 注释; 分号\nSELECT 2";
        let spans = split_statement_spans(sql);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "SELECT 1");
        assert_eq!(&sql[spans[0].start..spans[0].start + 8], "SELECT 1");
        assert_eq!(spans[1].text, "update t set a = 'x;y' where id = 1");
        assert_eq!(spans[2].text, "-- 注释; 分号\nSELECT 2");
        // 偏移必须能对回原文（截出去的片段以语句文本开头）。
        for span in &spans {
            let tail = &sql[span.start..];
            assert!(tail.starts_with(&span.text) || tail.starts_with("--"));
        }
        // 与无位置版本互证：文本清单必须完全一致。
        let plain = split_statements(sql);
        let from_spans: Vec<String> = spans.into_iter().map(|span| span.text).collect();
        assert_eq!(plain, from_spans);
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

    /// 四类「写形状的查询」绝不能判读：数据修改型 CTE、`SELECT … INTO`、
    /// `EXPLAIN ANALYZE <写>`、赋值型 PRAGMA / `INTO OUTFILE`。
    #[test]
    fn write_shaped_queries_are_never_read() {
        // 第 1 类：数据修改型 CTE / 解析失败的 WITH / 派生表里嵌 DML。
        for stmt in [
            "WITH x AS (SELECT 1) DELETE FROM users",
            "WITH x AS (DELETE FROM users RETURNING *) SELECT * FROM x",
            "WITH x AS (INSERT INTO users VALUES (1) RETURNING *) SELECT * FROM x",
            "WITH x AS (UPDATE users SET a = 1 RETURNING *) SELECT * FROM x",
            "WITH x AS (SELECT 1) INSERT INTO users VALUES (1)",
            "WITH x AS (SELECT 1) UPDATE users SET a = 1",
            "WITH x AS (SELEC bad) DELETE FROM users",
            "SELECT * FROM (INSERT INTO users VALUES (1) RETURNING *) x",
            "SELECT * FROM (DELETE FROM users RETURNING *) x",
        ] {
            assert_ne!(classify(stmt), StmtKind::Read, "不得判 Read：{stmt}");
            assert!(!prove_read_only(stmt), "不得给只读证明：{stmt}");
        }
        // 第 2 类：`SELECT … INTO`。
        assert_ne!(classify("SELECT * INTO newtbl FROM users"), StmtKind::Read);
        assert!(!prove_read_only("SELECT * INTO newtbl FROM users"));
        // 第 3 类：内层为写的 EXPLAIN 一律不放行（`ANALYZE` 会真的执行内层）。
        assert_ne!(
            classify("EXPLAIN ANALYZE DELETE FROM users"),
            StmtKind::Read
        );
        assert!(!prove_read_only("EXPLAIN ANALYZE DELETE FROM users"));
        assert!(!prove_read_only("EXPLAIN DELETE FROM users"));
        // 第 4 类：赋值型 PRAGMA / `INTO OUTFILE`。
        assert_ne!(classify("PRAGMA user_version = 5"), StmtKind::Read);
        assert_ne!(classify("PRAGMA journal_mode = WAL"), StmtKind::Read);
        assert_ne!(classify("PRAGMA journal_mode(WAL)"), StmtKind::Read);
        assert_ne!(
            classify("SELECT * FROM users INTO OUTFILE '/tmp/x'"),
            StmtKind::Read
        );
        assert!(!prove_read_only(
            "SELECT * FROM users INTO OUTFILE '/tmp/x'"
        ));
    }

    /// 安全形态不回归：普通只读查询仍判 Read（可证明只读的仍是子集）。
    #[test]
    fn plain_reads_stay_read() {
        for stmt in [
            "SELECT * FROM users",
            "select id, name from users where id = 1 order by id limit 10",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "SHOW TABLES",
            "PRAGMA table_info(t)",
            "PRAGMA user_version",
            "-- 前缀注释\nSELECT * FROM users",
        ] {
            assert_eq!(classify(stmt), StmtKind::Read, "应判 Read：{stmt}");
        }
        // `PRAGMA table_info(t)` 参数是标识符、解析器不认，故不给证明，但分类仍是 Read。
        for stmt in [
            "SELECT * FROM users",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "SHOW TABLES",
        ] {
            assert!(prove_read_only(stmt), "应给只读证明：{stmt}");
        }
    }

    /// 自动执行闸门（纯函数）：恰好一条 + 可证明只读才为真。
    #[test]
    fn allow_auto_execute_gate() {
        // 放行：单条只读（含大小写 / 前置注释）。
        assert!(allow_auto_execute("SELECT * FROM users"));
        assert!(allow_auto_execute("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(allow_auto_execute("  SELECT 1  "));
        assert!(allow_auto_execute("-- 说明\nselect * from t"));
        // 不放行：四类越权构造 + 普通写。
        for stmt in [
            "WITH x AS (SELECT 1) DELETE FROM users",
            "WITH x AS (INSERT INTO users VALUES (1) RETURNING *) SELECT * FROM x",
            "SELECT * INTO newtbl FROM users",
            "EXPLAIN ANALYZE DELETE FROM users",
            "PRAGMA user_version = 5",
            "SELECT * FROM users INTO OUTFILE '/tmp/x'",
            "INSERT INTO users VALUES (1)",
            "UPDATE users SET a = 1 WHERE id = 1",
            "DROP TABLE users",
            "-- 说明\ninsert into t values (1)",
        ] {
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        // 不放行：多条（哪怕每条都只读）/ 空 / 只有注释。
        assert!(!allow_auto_execute("SELECT 1; SELECT 2"));
        assert!(!allow_auto_execute(""));
        assert!(!allow_auto_execute("   "));
        assert!(!allow_auto_execute("-- 只有注释"));
    }

    /// RES-1：解析失败时的兜底要**扫描整条**的词法标记——CTE 包裹的全表删 / 全表改
    /// 必须判 `Dangerous`（触发「执行危险 SQL」确认），不能只落 `Write`。
    #[test]
    fn fallback_scan_flags_cte_wrapped_writes() {
        // 解析失败的 WITH + DML：词法兜底判危险（无 WHERE）。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users"),
            SqlRisk::Dangerous
        );
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) UPDATE users SET a = 1"),
            SqlRisk::Dangerous
        );
        assert_eq!(
            risk_of("WITH x AS (DELETE FROM users RETURNING *) SELECT * FROM x"),
            SqlRisk::Dangerous
        );
        // 带 WHERE 的 CTE 包裹 DML：写，但不升危险。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users WHERE id = 1"),
            SqlRisk::Write
        );
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) UPDATE users SET a = 1 WHERE id = 2"),
            SqlRisk::Write
        );
        // 字符串里的 `delete` 不算关键字（不因它多出一条 DML）。
        assert_eq!(
            risk_of("WITH x AS (SELECT 1) DELETE FROM users WHERE msg = 'delete'"),
            SqlRisk::Write
        );
        // 对照：裸 DML 本来就是 Dangerous（不能被改动破坏）。
        assert_eq!(risk_of("DELETE FROM users"), SqlRisk::Dangerous);
        assert_eq!(risk_of("UPDATE users SET a = 1"), SqlRisk::Dangerous);
        // 不误判：字面量 / 注释里的写关键字不触发。
        assert_eq!(risk_of("SELECT 'delete' FROM t"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("SELECT 'DROP TABLE t' FROM t"), SqlRisk::ReadOnly);
        assert_eq!(risk_of("-- DELETE FROM users\nSELECT 1"), SqlRisk::ReadOnly);
    }

    /// RES-2：命中已知副作用函数（`nextval` / `setval` / `dblink*` / 后端控制 …）的读
    /// **不自动执行**；名字不带括号（列名 / 表名）、或只在字面量里出现，不算命中。
    #[test]
    fn side_effect_functions_are_not_auto_executed() {
        for stmt in [
            "SELECT nextval('s')",
            "SELECT setval('s', 1)",
            "SELECT dblink('host=h', 'SELECT 1')",
            "SELECT pg_terminate_backend(123)",
            "SELECT pg_cancel_backend(123)",
            "SELECT set_config('a', 'b', false)",
            "SELECT lo_unlink(1)",
            "SELECT pg_advisory_lock(1)",
        ] {
            assert!(has_write_side_effects(stmt), "应识别副作用函数：{stmt}");
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        // 名字不带括号（当列名 / 表名用）不算函数调用。
        assert!(!has_write_side_effects("SELECT nextval FROM t"));
        // 只在字面量里出现，不算调用。
        assert!(!has_write_side_effects("SELECT 'nextval(1)' FROM t"));
        // 分类仍判读（副作用不改变语句种类），只是闸门不放行。
        assert_eq!(classify("SELECT nextval('s')"), StmtKind::Read);
        assert!(prove_read_only("SELECT nextval('s')"));
        // 普通聚合读照常放行。
        assert!(allow_auto_execute("SELECT count(*) FROM t"));
    }

    /// RES-3：可解析的**无值** PRAGMA 也要过白名单——维护型（`wal_checkpoint` /
    /// `optimize` / `shrink_memory`）无值也写，不给只读证明。
    #[test]
    fn bare_pragmas_need_the_whitelist() {
        assert!(prove_read_only("PRAGMA user_version"));
        assert!(prove_read_only("PRAGMA journal_mode"));
        assert_eq!(classify("PRAGMA user_version"), StmtKind::Read);
        assert_eq!(classify("PRAGMA journal_mode"), StmtKind::Read);
        for stmt in [
            "PRAGMA wal_checkpoint",
            "PRAGMA optimize",
            "PRAGMA shrink_memory",
        ] {
            assert!(
                !prove_read_only(stmt),
                "无值维护型 PRAGMA 不得判只读：{stmt}"
            );
            assert_eq!(classify(stmt), StmtKind::Write, "应判写：{stmt}");
            assert!(!allow_auto_execute(stmt), "不得自动执行：{stmt}");
        }
        assert!(allow_auto_execute("PRAGMA user_version"));
        // 带参的取信息型（解析器不认、走兜底白名单）仍判读。
        assert_eq!(classify("PRAGMA table_info(t)"), StmtKind::Read);
    }

    /// RES-4：`SELECT … FOR UPDATE / SHARE`（含 `FOR NO KEY UPDATE` / `FOR KEY SHARE`）
    /// 取行锁、会阻塞他人——判读可以，但**不自动执行**。
    #[test]
    fn locking_selects_are_not_auto_executed() {
        // 可解析的两种：分类仍是 Read、仍给只读证明，只是闸门不放行。
        assert_eq!(classify("SELECT * FROM t FOR UPDATE"), StmtKind::Read);
        assert!(prove_read_only("SELECT * FROM t FOR UPDATE"));
        for stmt in [
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "SELECT * FROM t FOR KEY SHARE",
        ] {
            assert!(has_write_side_effects(stmt), "应识别取锁子句：{stmt}");
            assert!(
                !allow_auto_execute(stmt),
                "取锁 SELECT 不得自动执行：{stmt}"
            );
        }
        // 对照：同一条 SELECT 去掉取锁子句就放行。
        assert!(allow_auto_execute("SELECT * FROM t"));
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

    /// 按引擎挑「能结构化」的形态：四条 SQL 引擎各有各的写法，其余三条退回通用。
    #[test]
    fn per_engine_explain_shapes() {
        assert_eq!(
            explain_sql_for(DatabaseKind::Sqlite, "select 1").as_deref(),
            Ok("EXPLAIN QUERY PLAN SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::Postgres, "select 1").as_deref(),
            Ok("EXPLAIN (FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::Mysql, "select 1").as_deref(),
            Ok("EXPLAIN FORMAT=JSON SELECT 1")
        );
        assert_eq!(
            explain_sql_for(DatabaseKind::ClickHouse, "select 1").as_deref(),
            Ok("EXPLAIN SELECT 1")
        );
        // 没有结构化形态的三条：与 `explain_sql` 同形（由数据库自己报错，不编语法）。
        for kind in [
            DatabaseKind::SqlServer,
            DatabaseKind::Mongo,
            DatabaseKind::Elasticsearch,
        ] {
            assert_eq!(
                explain_sql_for(kind, "select 1"),
                explain_sql("select 1"),
                "{kind:?} 应当退回通用形态"
            );
        }
        // 闸门是同一条：不合格的语句在任何引擎下都拿不到计划语句。
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
            DatabaseKind::ClickHouse,
        ] {
            assert_eq!(
                explain_sql_for(kind, "drop table t"),
                Err(ExplainSkip::NoPlanForThisStatement)
            );
            assert_eq!(
                explain_sql_for(kind, "select 1; select 2"),
                Err(ExplainSkip::NotASingleStatement)
            );
        }
    }

    /// 生成的计划语句必须**本身**被认成读语句：否则驱动会把它当写语句走 `execute`，
    /// 结果集就没了（各驱动的读 / 写分流靠 `prove_read_only` + `classify`）。
    #[test]
    fn generated_plan_statements_count_as_reads() {
        for kind in [
            DatabaseKind::Sqlite,
            DatabaseKind::Postgres,
            DatabaseKind::Mysql,
            DatabaseKind::ClickHouse,
        ] {
            let sql = explain_sql_for(kind, "select 1").expect("包装成功");
            assert!(
                prove_read_only(&sql) || classify(&sql) == StmtKind::Read,
                "{kind:?} 的计划语句被当成写语句了：{sql}"
            );
        }
    }
}
