//! 各引擎的补全词表：**这个引擎该认识哪些词**。
//!
//! # 为什么在内核
//!
//! `sqlintel` 已经是「SQL 知识」的落点（拆分 / 分类 / 风险 / 格式化），词表与它
//! 同类：纯数据、零 gpui、零 IO。放内核还有一个实际好处 —— `duck-app` 的补全器
//! 与将来的语法高亮可以共用**同一份**词表，不会各写一份。
//!
//! ⚠️ 与 `sqlintel::WRITE_KEYWORDS` **不是一回事**，也不是第二真源：那张表只收
//! 「会写库的语句动词」（语义判定，用来证明只读），这张收**全部语法词**（给用户
//! 补全）。两张表之间没有「改一处另一处要跟着改」的耦合。
//!
//! # 选词口径
//!
//! 照抄 dbx 的 `DATABASE_SQL_KEYWORDS`（`apps/desktop/src/lib/sql/sqlCompletion.ts`）
//! 与 dbflux 的 `sql_keyword_candidates`（`dbflux_ui_document/src/code/completion.rs`）
//! 的分层：**通用核心 + 引擎增量**。
//!
//! - 通用核心（[`COMMON_KEYWORDS`] / [`COMMON_FUNCTIONS`]）只放**五个 SQL 引擎都
//!   认得、且名字语义都一致**的词。判不准的宁可不放：`GREATEST` / `LEAST` 在
//!   SQLite 与服务端 SQL Server 上不存在，于是一个进不了核心、只进 PostgreSQL /
//!   MySQL 的增量；反过来放进去的代价是**教人写错**（在 SQLite 里补出 `JSONB`、
//!   在 PostgreSQL 里补出 `AUTO_INCREMENT`，都是把用户往失败推）。
//! - 引擎增量只放「别的引擎没有、或名字/语义不同」的词。
//! - 词表是**核心**而不是全集：目标是「敲三个字母能想起名字」，不是把手册搬进来。
//! - 大小写：SQL 词一律大写（补全插入的是 label 原文，混进小写会让同一份 SQL 里
//!   两种写法并存）。唯一例外是 ClickHouse 的函数名，见 [`CLICKHOUSE_FUNCTIONS`]。
//!
//! # 非 SQL 引擎
//!
//! MongoDB 与 Elasticsearch 的查询语言不是 SQL，但它们**也有该认识的词**，而且是
//! 我们这两条驱动**真正接受**的那种输入：
//!
//! - [`mongo_words`]：`drivers/mongo.rs` 的编辑器吃的是**一个 JSON 命令文档**
//!   （`{"aggregate": "users", "pipeline": [...]}`，即 `db.runCommand(<doc>)` 的
//!   参数），所以收的是**服务端命令名** + 聚合阶段 + 查询/更新操作符。
//!   ⚠️ 这里**故意不收** `db.coll.find({})` 这种 mongo shell 外壳语法（dbflux 的
//!   `MONGO_METHODS` 收的就是它）—— 我们的驱动不吃壳语法，补出来就是让人白写。
//! - [`elasticsearch_words`]：`drivers/elasticsearch.rs` 的编辑器吃的是
//!   **Kibana 控制台**语法（`GET /logs-*/_search` + JSON 体），所以收的是请求动词、
//!   端点与检索 DSL 键。

use crate::config::DatabaseKind;

/// 一份补全词表：两桶，对应补全菜单里的两类说明文案。
///
/// 分两桶而不是打平：菜单里「关键字」与「函数」的图标和说明不同（`CompletionItemKind`
/// 也分 KEYWORD / FUNCTION），用户扫一眼就知道该往这儿写词还是写调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlWords {
    /// 「关键字」桶：语句 / 子句 / 谓词 / 类型名。
    ///
    /// MongoDB 收查询与聚合操作符；Elasticsearch 收请求动词、端点与 DSL 键。
    pub keywords: &'static [&'static str],
    /// 「函数」桶：标量 / 聚合 / 窗口函数。
    ///
    /// MongoDB 收服务端命令名（界面按 [`QueryLanguage::Mongo`] 标成「命令」）；
    /// Elasticsearch 是空的（控制台语法里没有函数调用这种东西）。
    pub functions: &'static [&'static str],
}

/// 该引擎的查询语言属于哪一家 —— 决定界面怎么给 [`SqlWords`] 的两桶贴标签。
///
/// 存在的理由很具体：`find` 是我们的 MongoDB 驱动接受的**命令名**，文案写成「函数」
/// 不算撒谎但也不准确；而 Elasticsearch 那一桶本来就只有关键字。判据只有一处，
/// 界面不要再自己按 `DatabaseKind` 分叉一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryLanguage {
    /// 五个 SQL 引擎：桶标「关键字 / 函数」。
    Sql,
    /// MongoDB：桶标「关键字 / 命令」。
    Mongo,
    /// Elasticsearch：桶标「关键字 / 函数」（函数桶为空）。
    Elasticsearch,
}

// ── 通用核心：五个 SQL 引擎共有、同名同义 ────────────────────────────────────

/// 通用关键字：语句、子句、谓词、类型名、事务与权限词。
const COMMON_KEYWORDS: &[&str] = &[
    "ADD COLUMN",
    "ALL",
    "ALTER COLUMN",
    "ALTER TABLE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BLOB",
    "CASCADE",
    "CASE",
    "CHAR",
    "CHECK",
    "COMMENT",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CREATE INDEX",
    "CREATE OR REPLACE VIEW",
    "CREATE SCHEMA",
    "CREATE SEQUENCE",
    "CREATE TABLE",
    "CREATE TRIGGER",
    "CREATE UNIQUE INDEX",
    "CREATE VIEW",
    "CROSS JOIN",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DECIMAL",
    "DEFAULT",
    "DELETE FROM",
    "DESC",
    "DISTINCT",
    "DOUBLE PRECISION",
    "DROP",
    "DROP COLUMN",
    "DROP INDEX",
    "DROP SCHEMA",
    "DROP TABLE",
    "DROP VIEW",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FETCH FIRST",
    "FLOAT",
    "FOR UPDATE",
    "FOREIGN KEY",
    "FROM",
    "FULL JOIN",
    "FULL OUTER JOIN",
    "GRANT",
    "GROUP BY",
    "HAVING",
    "IF EXISTS",
    "IF NOT EXISTS",
    "IN",
    "INNER JOIN",
    "INSERT INTO",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS NOT NULL",
    "IS NULL",
    "JOIN",
    "JSON",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "LIKE",
    "LIMIT",
    "MERGE",
    "NATURAL JOIN",
    "NOT",
    "NOT IN",
    "NOT LIKE",
    "NOT NULL",
    "NULL",
    "NULLS FIRST",
    "NULLS LAST",
    "NUMERIC",
    "OFFSET",
    "ON",
    "OR",
    "ORDER BY",
    "OVER",
    "PARTITION BY",
    "PRIMARY KEY",
    "RANGE",
    "REAL",
    "RECURSIVE",
    "REFERENCES",
    "RENAME TO",
    "RESTRICT",
    "REVOKE",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "ROLLBACK",
    "ROWS",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "SMALLINT",
    "SOME",
    "START TRANSACTION",
    "TEMPORARY",
    "TEXT",
    "THEN",
    "TIME",
    "TIMESTAMP",
    "TRUNCATE TABLE",
    "UNBOUNDED",
    "UNION",
    "UNION ALL",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VARCHAR",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHIN GROUP",
];

/// 通用函数：SQL 标准里就有、五个引擎同名同义的标量 / 聚合 / 窗口函数。
const COMMON_FUNCTIONS: &[&str] = &[
    "ABS",
    "AVG",
    "CAST",
    "CEIL",
    "CEILING",
    "CHAR_LENGTH",
    "COALESCE",
    "CONCAT",
    "COUNT",
    "COUNT(*)",
    "DENSE_RANK",
    "EXP",
    "EXTRACT",
    "FIRST_VALUE",
    "FLOOR",
    "LAG",
    "LAST_VALUE",
    "LEAD",
    "LENGTH",
    "LN",
    "LOG",
    "LOG10",
    "LOWER",
    "LTRIM",
    "MAX",
    "MIN",
    "MOD",
    "NTH_VALUE",
    "NTILE",
    "NULLIF",
    "OCTET_LENGTH",
    "POSITION",
    "POWER",
    "RANK",
    "REPLACE",
    "ROUND",
    "ROW_NUMBER",
    "RTRIM",
    "SQRT",
    "SUBSTRING",
    "SUM",
    "TRIM",
    "UPPER",
];

// ── SQLite ──────────────────────────────────────────────────────────────────

const SQLITE_KEYWORDS: &[&str] = &[
    "ATTACH DATABASE",
    "AUTOINCREMENT",
    "DETACH DATABASE",
    "GLOB",
    "INDEXED BY",
    "INSERT OR IGNORE",
    "INSERT OR REPLACE",
    "NOT INDEXED",
    "PRAGMA",
    "REINDEX",
    "STRICT",
    "VACUUM",
    "VIRTUAL TABLE",
    "WITHOUT ROWID",
];

const SQLITE_FUNCTIONS: &[&str] = &[
    "CHANGES",
    "DATE",
    "DATETIME",
    "IIF",
    "IFNULL",
    "INSTR",
    "JULIANDAY",
    "JSON_ARRAY",
    "JSON_EXTRACT",
    "JSON_OBJECT",
    "JSON_SET",
    "LAST_INSERT_ROWID",
    "LIKELY",
    "RANDOM",
    "RANDOMBLOB",
    "SQLITE_VERSION",
    "STRFTIME",
    "TOTAL",
    "TOTAL_CHANGES",
    "TYPEOF",
    "UNHEX",
    "UNIXEPOCH",
    "UNLIKELY",
    "ZEROBLOB",
];

// ── PostgreSQL ──────────────────────────────────────────────────────────────

const POSTGRES_KEYWORDS: &[&str] = &[
    "BIGSERIAL",
    "BYTEA",
    "CIDR",
    "DO NOTHING",
    "DO UPDATE",
    "GENERATED",
    "IDENTITY",
    "ILIKE",
    "INET",
    "JSONB",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "MACADDR",
    "MATERIALIZED VIEW",
    "ON CONFLICT",
    "RETURNING",
    "SERIAL",
    "SIMILAR TO",
    "SMALLSERIAL",
    "TABLESAMPLE",
    "TSQUERY",
    "TSVECTOR",
    "UUID",
];

const POSTGRES_FUNCTIONS: &[&str] = &[
    "AGE",
    "ARRAY_AGG",
    "ARRAY_LENGTH",
    "CURRENT_SETTING",
    "DATE_PART",
    "DATE_TRUNC",
    "GENERATE_SERIES",
    "GREATEST",
    "JSON_AGG",
    "JSONB_AGG",
    "JSONB_BUILD_OBJECT",
    "JSONB_SET",
    "LEAST",
    "NOW",
    "REGEXP_REPLACE",
    "STRING_AGG",
    "TO_CHAR",
    "TO_DATE",
    "TO_JSONB",
    "TO_NUMBER",
    "TO_TIMESTAMP",
    "UNNEST",
];

// ── MySQL / MariaDB ─────────────────────────────────────────────────────────

const MYSQL_KEYWORDS: &[&str] = &[
    "AUTO_INCREMENT",
    "BOOL",
    "BOOLEAN",
    "CALL",
    "CHARSET",
    "COLLATE",
    "DELIMITER",
    "DESCRIBE",
    "DISTINCTROW",
    "DUPLICATE KEY",
    "ENGINE",
    "ENUM",
    "FORCE INDEX",
    "HIGH_PRIORITY",
    "KILL",
    "LOCK TABLES",
    "LONGBLOB",
    "LONGTEXT",
    "MEDIUMBLOB",
    "MEDIUMTEXT",
    "ON DUPLICATE KEY UPDATE",
    "REPLACE INTO",
    "SCHEMA",
    "SET GLOBAL",
    "SET NAMES",
    "SET SESSION",
    "SHOW",
    "SHOW CREATE TABLE",
    "SHOW DATABASES",
    "SHOW INDEX",
    "SHOW TABLES",
    "SHOW VARIABLES",
    "STRAIGHT_JOIN",
    "TINYBLOB",
    "TINYTEXT",
    "UNLOCK TABLES",
    "UNSIGNED",
    "USE",
    "ZEROFILL",
];

const MYSQL_FUNCTIONS: &[&str] = &[
    "CONCAT_WS",
    "CURDATE",
    "CURTIME",
    "DATE_ADD",
    "DATE_FORMAT",
    "DATE_SUB",
    "DATEDIFF",
    "FIND_IN_SET",
    "FROM_UNIXTIME",
    "GROUP_CONCAT",
    "IF",
    "IFNULL",
    "JSON_EXTRACT",
    "JSON_OBJECT",
    "JSON_UNQUOTE",
    "LAST_INSERT_ID",
    "MD5",
    "NOW",
    "RAND",
    "SHA2",
    "SUBSTR",
    "TIMESTAMPDIFF",
    "UNIX_TIMESTAMP",
    "UUID",
    "YEARWEEK",
];

// ── ClickHouse ──────────────────────────────────────────────────────────────

/// ClickHouse 的子句 / 修饰词。函数名是**大小写敏感**的 camelCase，见下。
const CLICKHOUSE_KEYWORDS: &[&str] = &[
    "ALIAS",
    "ANTI JOIN",
    "ANY JOIN",
    "ARRAY",
    "ARRAY JOIN",
    "ASOF JOIN",
    "ATTACH",
    "CODEC",
    "CUBE",
    "DETACH",
    "ENGINE",
    "FINAL",
    "FORMAT",
    "GLOBAL",
    "GRANULARITY",
    "KILL",
    "MATERIALIZED",
    "OPTIMIZE",
    "OUTFILE",
    "PREWHERE",
    "PROJECTION",
    "ROLLUP",
    "SAMPLE",
    "SEMI JOIN",
    "SETTINGS",
    "SYSTEM",
    "TOTALS",
    "TTL",
    "TRUNCATE",
    "USE",
];

/// ⚠️ ClickHouse 的函数名**大小写规则分两档**（官方文档与实际行为都如此）：
///
/// - 常见**聚合与类型转换**函数（`sum` / `count` / `toDate` 那一类）注册为**大小写
///   不敏感**，写大写的 `SUM` 也认 —— 所以它们不需要单独收，[`COMMON_FUNCTIONS`]
///   里的大写写法在 ClickHouse 上同样有效。
/// - ClickHouse **专有**函数（`arrayJoin` / `multiIf` / `toStartOfDay`）**大小写敏感**，
///   必须照官方拼写；`tostartofday(...)` 这种写法是执行期报错。
///
/// 所以这一桶只收**专有函数 / 官方拼写就是小写或 camelCase 的函数**，通用聚合交给
/// 通用桶 —— 免得菜单里同时冒出 `SUM` 与 `sum` 两个候选（同一件事两份写法，用户不
/// 知道该挑哪个）。
const CLICKHOUSE_FUNCTIONS: &[&str] = &[
    "arrayJoin",
    "avgIf",
    "countIf",
    "dateDiff",
    "dictGet",
    "formatDateTime",
    "groupArray",
    "ifNull",
    "JSONExtractString",
    "maxIf",
    "minIf",
    "multiIf",
    "now",
    "quantile",
    "splitByChar",
    "sumIf",
    "toDate",
    "toDateTime",
    "toFloat64",
    "toInt32",
    "toStartOfDay",
    "toString",
    "toUInt64",
    "today",
    "uniq",
    "uniqExact",
    "yesterday",
];

// ── Microsoft SQL Server ────────────────────────────────────────────────────

const SQLSERVER_KEYWORDS: &[&str] = &[
    "BEGIN CATCH",
    "BEGIN TRY",
    "CROSS APPLY",
    "DBCC",
    "END CATCH",
    "END TRY",
    "EXEC",
    "EXECUTE",
    "GO",
    "IDENTITY",
    "IDENTITY_INSERT",
    "NCHAR",
    "NOLOCK",
    "NTEXT",
    "NVARCHAR",
    "OPTION",
    "OUTER APPLY",
    "PIVOT",
    "RAISERROR",
    "SELECT INTO",
    "SET NOCOUNT ON",
    "THROW",
    "TOP",
    "UNIQUEIDENTIFIER",
    "UNPIVOT",
];

const SQLSERVER_FUNCTIONS: &[&str] = &[
    "CHARINDEX",
    "CONVERT",
    "DATEADD",
    "DATEDIFF",
    "DATENAME",
    "DATEPART",
    "EOMONTH",
    "FORMAT",
    "GETDATE",
    "GETUTCDATE",
    "IIF",
    "ISNULL",
    "ISNUMERIC",
    "LEN",
    "NEWID",
    "OBJECT_ID",
    "PATINDEX",
    "REPLICATE",
    "SCOPE_IDENTITY",
    "STUFF",
    "SYSDATETIME",
    "SYSUTCDATETIME",
    "TRY_CAST",
    "TRY_CONVERT",
];

// ── MongoDB（JSON 命令文档）──────────────────────────────────────────────────

/// MongoDB 查询 / 更新操作符与聚合阶段（都以 `$` 开头，逐个照服务端文档拼写）。
const MONGO_OPERATORS: &[&str] = &[
    "$addFields",
    "$addToSet",
    "$all",
    "$and",
    "$bucket",
    "$collStats",
    "$count",
    "$currentDate",
    "$elemMatch",
    "$eq",
    "$exists",
    "$expr",
    "$facet",
    "$geoNear",
    "$graphLookup",
    "$group",
    "$gt",
    "$gte",
    "$in",
    "$inc",
    "$indexStats",
    "$jsonSchema",
    "$limit",
    "$lookup",
    "$lt",
    "$lte",
    "$match",
    "$merge",
    "$mod",
    "$mul",
    "$ne",
    "$nin",
    "$nor",
    "$not",
    "$or",
    "$out",
    "$pop",
    "$project",
    "$pull",
    "$push",
    "$regex",
    "$rename",
    "$replaceRoot",
    "$replaceWith",
    "$sample",
    "$search",
    "$set",
    "$setOnInsert",
    "$size",
    "$skip",
    "$sort",
    "$sortByCount",
    "$text",
    "$type",
    "$unset",
    "$unwind",
];

/// MongoDB 服务端命令名（命令文档的**第一个键**，也就是 `runCommand` 的命令名）。
const MONGO_COMMANDS: &[&str] = &[
    "aggregate",
    "buildInfo",
    "collStats",
    "commitTransaction",
    "connectionStatus",
    "count",
    "create",
    "createIndexes",
    "currentOp",
    "dbStats",
    "delete",
    "distinct",
    "drop",
    "dropDatabase",
    "endSessions",
    "explain",
    "find",
    "findAndModify",
    "getLog",
    "getParameter",
    "hello",
    "insert",
    "isMaster",
    "killCursors",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "mapReduce",
    "ping",
    "renameCollection",
    "serverStatus",
    "startTransaction",
    "update",
    "validate",
];

// ── Elasticsearch（Kibana 控制台语法）────────────────────────────────────────

/// 请求动词、端点与检索 DSL 键。Kibana 控制台里这三样是同一层的东西（都写在
/// 请求行或 JSON 体里），所以同桶。
const ELASTICSEARCH_KEYWORDS: &[&str] = &[
    "DELETE",
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "_aliases",
    "_analyze",
    "_bulk",
    "_cat/aliases",
    "_cat/health",
    "_cat/indices",
    "_cat/nodes",
    "_count",
    "_delete_by_query",
    "_doc",
    "_explain",
    "_field_caps",
    "_mapping",
    "_mget",
    "_msearch",
    "_refresh",
    "_search",
    "_settings",
    "_snapshot",
    "_source",
    "_sql",
    "_update",
    "_update_by_query",
    "aggs",
    "bool",
    "boost",
    "collapse",
    "docvalue_fields",
    "exists",
    "filter",
    "from",
    "fuzzy",
    "gt",
    "gte",
    "highlight",
    "lt",
    "lte",
    "match",
    "match_all",
    "match_phrase",
    "minimum_should_match",
    "must",
    "must_not",
    "nested",
    "operator",
    "pit",
    "prefix",
    "query",
    "range",
    "runtime_mappings",
    "script",
    "search_after",
    "should",
    "size",
    "sort",
    "suggest",
    "term",
    "terms",
    "timeout",
    "track_total_hits",
    "wildcard",
];

// ── 对外 API ────────────────────────────────────────────────────────────────

/// 该引擎的查询语言。
///
/// ⚠️ **agent 档按 `Sql` 处理**：它背后几十个驱动里绝大多数是 SQL 引擎
/// （Oracle / 达梦 / 金仓 / DB2 / Hive / Teradata / Snowflake …），少数不是
/// （etcd / Kafka 这类 KV、消息队列）但它们的语句也不是 JSON 命令文档，归到
/// Mongo / Elasticsearch 那两支只会更错。真正的分化在补全词表那一层（见 [`words`]）。
#[must_use]
pub const fn language(kind: DatabaseKind) -> QueryLanguage {
    match kind {
        DatabaseKind::Sqlite
        | DatabaseKind::Postgres
        | DatabaseKind::Mysql
        | DatabaseKind::ClickHouse
        | DatabaseKind::SqlServer
        | DatabaseKind::Agent => QueryLanguage::Sql,
        DatabaseKind::Mongo => QueryLanguage::Mongo,
        DatabaseKind::Elasticsearch => QueryLanguage::Elasticsearch,
    }
}

/// 该引擎的补全词表。
///
/// SQL 引擎拿到的是 `(通用核心, 引擎增量)`：调用方要**依次**遍历两段（增量排在
/// 核心之后，用户先看到常用词）。MongoDB 与 Elasticsearch 各自一份，核心那份是
/// 空的 —— 它们没有「通用 SQL 核心」这种中间层。
///
/// 返回两段切片而不是一个合并好的 `Vec`：合并要在每次补全时分配（每个按键都来
/// 一次）。两段**不重叠**由单测 `dialect_tables_do_not_repeat_the_core` 钉住，
/// 所以调用方不需要再去重。
#[must_use]
pub const fn words(kind: DatabaseKind) -> (SqlWords, SqlWords) {
    match kind {
        DatabaseKind::Sqlite => (
            common_words(),
            SqlWords {
                keywords: SQLITE_KEYWORDS,
                functions: SQLITE_FUNCTIONS,
            },
        ),
        DatabaseKind::Postgres => (
            common_words(),
            SqlWords {
                keywords: POSTGRES_KEYWORDS,
                functions: POSTGRES_FUNCTIONS,
            },
        ),
        DatabaseKind::Mysql => (
            common_words(),
            SqlWords {
                keywords: MYSQL_KEYWORDS,
                functions: MYSQL_FUNCTIONS,
            },
        ),
        DatabaseKind::ClickHouse => (
            common_words(),
            SqlWords {
                keywords: CLICKHOUSE_KEYWORDS,
                functions: CLICKHOUSE_FUNCTIONS,
            },
        ),
        DatabaseKind::SqlServer => (
            common_words(),
            SqlWords {
                keywords: SQLSERVER_KEYWORDS,
                functions: SQLSERVER_FUNCTIONS,
            },
        ),
        // 非 SQL 引擎没有「通用核心」这一层：它们的词表就是它们的全部。
        DatabaseKind::Mongo => (
            SqlWords {
                keywords: &[],
                functions: &[],
            },
            SqlWords {
                keywords: MONGO_OPERATORS,
                functions: MONGO_COMMANDS,
            },
        ),
        DatabaseKind::Elasticsearch => (
            SqlWords {
                keywords: &[],
                functions: &[],
            },
            SqlWords {
                keywords: ELASTICSEARCH_KEYWORDS,
                functions: &[],
            },
        ),
        // agent 档只给**通用核心**、增量留空：背后是几十个方言各异的引擎，而这里只拿得到
        // `DatabaseKind::Agent` 一个值。给通用核心是安全的（`SELECT` / `WHERE` / `JOIN` 这些
        // 各家都认），挑一份方言增量则会让 Oracle 用户看到一堆 MySQL 函数名。
        // 要做得更准，得先让 `Session` 把驱动键带下来，再按驱动取词表。
        DatabaseKind::Agent => (
            common_words(),
            SqlWords {
                keywords: &[],
                functions: &[],
            },
        ),
    }
}

/// 五个 SQL 引擎共有的那一段。
///
/// 单独公开是给「还没有活跃连接」的调用方兜底用的：那时补的是「SQL 长什么样」，
/// 不该替某个具体引擎说话，所以只给这一份、不给任何增量。
#[must_use]
pub const fn common_words() -> SqlWords {
    SqlWords {
        keywords: COMMON_KEYWORDS,
        functions: COMMON_FUNCTIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 词表内部不许重复：补全菜单里同一项出现两遍是肉眼可见的 bug。
    fn assert_no_duplicates(label: &str, words: &[&str]) {
        let mut seen = std::collections::HashSet::new();
        for word in words {
            assert!(seen.insert(*word), "{label} 里 {word} 重复");
        }
    }

    #[test]
    fn every_word_table_is_duplicate_free() {
        assert_no_duplicates("COMMON_KEYWORDS", COMMON_KEYWORDS);
        assert_no_duplicates("COMMON_FUNCTIONS", COMMON_FUNCTIONS);
        assert_no_duplicates("SQLITE_KEYWORDS", SQLITE_KEYWORDS);
        assert_no_duplicates("SQLITE_FUNCTIONS", SQLITE_FUNCTIONS);
        assert_no_duplicates("POSTGRES_KEYWORDS", POSTGRES_KEYWORDS);
        assert_no_duplicates("POSTGRES_FUNCTIONS", POSTGRES_FUNCTIONS);
        assert_no_duplicates("MYSQL_KEYWORDS", MYSQL_KEYWORDS);
        assert_no_duplicates("MYSQL_FUNCTIONS", MYSQL_FUNCTIONS);
        assert_no_duplicates("CLICKHOUSE_KEYWORDS", CLICKHOUSE_KEYWORDS);
        assert_no_duplicates("CLICKHOUSE_FUNCTIONS", CLICKHOUSE_FUNCTIONS);
        assert_no_duplicates("SQLSERVER_KEYWORDS", SQLSERVER_KEYWORDS);
        assert_no_duplicates("SQLSERVER_FUNCTIONS", SQLSERVER_FUNCTIONS);
        assert_no_duplicates("MONGO_OPERATORS", MONGO_OPERATORS);
        assert_no_duplicates("MONGO_COMMANDS", MONGO_COMMANDS);
        assert_no_duplicates("ELASTICSEARCH_KEYWORDS", ELASTICSEARCH_KEYWORDS);
    }

    /// 增量表不许重复通用核心：那等于同一项在菜单里出现两遍（调用方虽然有去重，
    /// 但表本身干净才是可维护的）。
    #[test]
    fn dialect_tables_do_not_repeat_the_core() {
        for table in [
            SQLITE_KEYWORDS,
            POSTGRES_KEYWORDS,
            MYSQL_KEYWORDS,
            CLICKHOUSE_KEYWORDS,
            SQLSERVER_KEYWORDS,
        ] {
            for word in table {
                assert!(
                    !COMMON_KEYWORDS.contains(word),
                    "关键字增量与通用核心撞车：{word}"
                );
            }
        }
        for table in [
            SQLITE_FUNCTIONS,
            POSTGRES_FUNCTIONS,
            MYSQL_FUNCTIONS,
            CLICKHOUSE_FUNCTIONS,
            SQLSERVER_FUNCTIONS,
        ] {
            for word in table {
                assert!(
                    !COMMON_FUNCTIONS.contains(word),
                    "函数增量与通用核心撞车：{word}"
                );
            }
        }
    }

    /// 每个引擎都得有词可补；SQL 引擎必须带通用核心。
    #[test]
    fn every_engine_has_words() {
        for kind in DatabaseKind::ALL {
            let (core, extra) = words(kind);
            assert!(
                !core.keywords.is_empty() || !extra.keywords.is_empty(),
                "{kind:?} 没有任何关键字"
            );
            if language(kind) == QueryLanguage::Sql {
                assert_eq!(core.keywords, COMMON_KEYWORDS, "{kind:?} 缺通用关键字");
                assert_eq!(core.functions, COMMON_FUNCTIONS, "{kind:?} 缺通用函数");
            }
        }
    }

    /// 每条 SQL 词都得是**大写**：补全插入的是 label 原文，混进小写会让同一份 SQL
    /// 里两种写法并存。ClickHouse 的函数名是已知例外（大小写敏感），单独跳过。
    #[test]
    fn sql_words_are_uppercase() {
        for table in [
            COMMON_KEYWORDS,
            COMMON_FUNCTIONS,
            SQLITE_KEYWORDS,
            SQLITE_FUNCTIONS,
            POSTGRES_KEYWORDS,
            POSTGRES_FUNCTIONS,
            MYSQL_KEYWORDS,
            MYSQL_FUNCTIONS,
            SQLSERVER_KEYWORDS,
            SQLSERVER_FUNCTIONS,
        ] {
            for word in table {
                assert_eq!(
                    *word,
                    word.to_uppercase(),
                    "SQL 词表里出现了非大写词：{word}"
                );
            }
        }
    }

    /// 这个引擎的词表**整体**（核心 + 增量）里有没有 `word` —— 也就是补全菜单会不会
    /// 给出它。用户看得到的那一层，测试也按这一层写。
    fn offers(kind: DatabaseKind, word: &str) -> bool {
        let (core, extra) = words(kind);
        core.keywords.contains(&word)
            || extra.keywords.contains(&word)
            || core.functions.contains(&word)
            || extra.functions.contains(&word)
    }

    /// **不串台**：每个引擎只补自己的词，别的引擎的专有写法一个字都不出。
    ///
    /// 这条是用户能直接踩到的：在 SQLite 上被补出 `AUTO_INCREMENT`、在 MySQL 上被
    /// 补出 `RETURNING`，都会写出一执行就报错的 SQL。
    #[test]
    fn engines_do_not_leak_each_others_words() {
        // (引擎, 该有的专有词, 不该有的别家专有词)
        let cases = [
            (
                DatabaseKind::Sqlite,
                &["AUTOINCREMENT", "PRAGMA", "IFNULL", "STRFTIME"][..],
                &[
                    "AUTO_INCREMENT",
                    "RETURNING",
                    "JSONB",
                    "PREWHERE",
                    "GETDATE",
                ][..],
            ),
            (
                DatabaseKind::Postgres,
                &["RETURNING", "JSONB", "ILIKE", "ON CONFLICT", "BIGSERIAL"][..],
                &["AUTOINCREMENT", "AUTO_INCREMENT", "PREWHERE", "GETDATE"][..],
            ),
            (
                DatabaseKind::Mysql,
                &["AUTO_INCREMENT", "SHOW TABLES", "UNSIGNED", "GROUP_CONCAT"][..],
                &["AUTOINCREMENT", "RETURNING", "JSONB", "PREWHERE"][..],
            ),
            (
                DatabaseKind::ClickHouse,
                &["PREWHERE", "FINAL", "SAMPLE", "arrayJoin", "toStartOfDay"][..],
                &["AUTOINCREMENT", "AUTO_INCREMENT", "RETURNING", "GETDATE"][..],
            ),
            (
                DatabaseKind::SqlServer,
                &["TOP", "GETDATE", "CROSS APPLY", "NVARCHAR", "NEWID"][..],
                &[
                    "AUTOINCREMENT",
                    "AUTO_INCREMENT",
                    "RETURNING",
                    "JSONB",
                    "PREWHERE",
                ][..],
            ),
            // 非 SQL 引擎连通用 SQL 核心都不该有：Mongo 里补出 `SELECT` 毫无意义。
            (
                DatabaseKind::Mongo,
                &["$match", "$group", "aggregate", "find", "createIndexes"][..],
                &["SELECT", "FROM", "WHERE", "AUTO_INCREMENT", "_search"][..],
            ),
            (
                DatabaseKind::Elasticsearch,
                &["_search", "_cat/indices", "match_all", "POST"][..],
                &["SELECT", "FROM", "WHERE", "AUTO_INCREMENT", "$match"][..],
            ),
        ];
        for (kind, expected, forbidden) in cases {
            for word in expected {
                assert!(offers(kind, word), "{kind:?} 应该补出 {word}，实际没有");
            }
            for word in forbidden {
                assert!(!offers(kind, word), "{kind:?} 不该补出 {word}，实际补了");
            }
        }
    }
}
