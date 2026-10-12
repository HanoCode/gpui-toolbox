//! 驱动分发：`Connection` 枚举 + 统一连接面。
//!
//! 分发用 enum 而不是 trait object（dbdelve / dbx 的共同经验）：引擎数量有限、
//! 分发点集中在这一个 `impl` 里，类型安全且零动态分派；跨界出来的全是
//! [`crate::result`] 的具体值。每引擎一个子模块（连接 / 执行 / 内省三件事）。
//!
//! # 限定名（[`TableRef`]）
//!
//! 表 / 视图这些目录对象的引用一律走 [`TableRef`]（`schema` + `name`），不再用裸
//! `&str`。原因：PG 的多 schema、MySQL 的「schema 即库」都需要限定名，而把限定名
//! 拼成 `"schema"."name"` 再回传字符串，会让**引号与转义规则**漏到调用方去。
//! 这里由 [`quote_qualified`] 逐段加引号，各驱动只管调它。

pub mod clickhouse;
pub mod elasticsearch;
pub mod mongo;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

use std::fmt;

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, DatabaseKind};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet};

/// 一个目录对象的限定名：命名空间（可选）+ 名字。
///
/// - PostgreSQL：`schema` = `public` 之类；`None` 表示用 `current_schema()`。
/// - MySQL：`schema` = 库名（MySQL 里 schema 与 database 同义）；`None` = `DATABASE()`。
/// - SQLite：`schema` = `main` / `temp` / attached 名；`None` = `main`。
///
/// `schema` 为 `None` 时，「限定名」退化成裸名字 —— 与限定的写法**语义等价**
/// （都在当前命名空间里找），所以调用方拿不到 schema 时不必编造一个。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableRef {
    /// 命名空间；`None` = 当前（`current_schema()` / `DATABASE()` / `main`）。
    pub schema: Option<String>,
    /// 对象名（未加引号的原名）。
    pub name: String,
}

impl TableRef {
    /// 当前命名空间里的一个对象。
    #[must_use]
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    /// 指定命名空间里的一个对象。
    #[must_use]
    pub fn in_schema(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    /// 名字是否为空（没有选中对象）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// 展示文本（`schema.name`；无 schema 时就是裸名）。
    #[must_use]
    pub fn display(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// 限定名的各段（schema / name，空段已剔除）——供 [`quote_qualified`] 逐段加引号。
    #[must_use]
    pub fn parts(&self) -> Vec<&str> {
        let mut parts = Vec::with_capacity(2);
        if let Some(schema) = &self.schema
            && !schema.is_empty()
        {
            parts.push(schema.as_str());
        }
        if !self.name.is_empty() {
            parts.push(self.name.as_str());
        }
        parts
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

impl From<&str> for TableRef {
    fn from(name: &str) -> Self {
        Self::bare(name)
    }
}

impl From<String> for TableRef {
    fn from(name: String) -> Self {
        Self::bare(name)
    }
}

/// 目录里的一个对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTable {
    /// 名称（含引号转义前的原名）。
    pub name: String,
    /// 对象种类。
    pub kind: TableKind,
    /// 所属命名空间（见 [`TableRef::schema`]；SQLite 的 `main` 也照实给出）。
    pub schema: Option<String>,
}

impl CatalogTable {
    /// 转成限定名引用。
    #[must_use]
    pub fn table_ref(&self) -> TableRef {
        TableRef {
            schema: self.schema.clone(),
            name: self.name.clone(),
        }
    }
}

/// 一格编辑的目标行定位：主键列名 → 该行原值（展示文本）。
pub type PkValues = Vec<(String, String)>;

/// 一次批量改格里的一格：定位（主键原值）+ 目标列 + 新值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdit {
    /// 主键列名 → 该行原值（展示文本）；空表示定位不了，调用方先拒。
    pub pk: PkValues,
    /// 目标列名。
    pub column: String,
    /// 新值（展示文本；各引擎按列类型亲和转换）。`None` = 写入 `NULL`。
    pub value: Option<String>,
}

/// 一条待插入的行：列清单 + 对应值。
///
/// 列清单是**逐行给**的（不是一张表统一的列集）：新增行时用户只填一部分列，
/// 没填的列应当落到数据库的 `DEFAULT`，而不是被显式写成 `NULL`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertRow {
    /// 本次插入要写的列（顺序即 `values` 的顺序）。
    pub columns: Vec<String>,
    /// 与 `columns` 等长的值；`None` = 写 `NULL`（不是 DEFAULT）。
    pub values: Vec<Option<String>>,
}

/// [`Connection::apply_batch`] 的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// 改动的行数（命中数之和）。
    pub updated: u64,
    /// 插入的行数。
    pub inserted: u64,
}

impl BatchOutcome {
    /// 总影响行数。
    #[must_use]
    pub const fn total(self) -> u64 {
        self.updated + self.inserted
    }
}

/// 目录对象的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// 表。
    Table,
    /// 视图。
    View,
    /// 函数 / 存储过程（PG `information_schema.routines`、MySQL 同）。
    Routine,
    /// 序列（PG 独有；MySQL / SQLite 无此概念）。
    Sequence,
}

impl TableKind {
    /// 是否可当作表来浏览 / 编辑（表：可以；视图：只读浏览；函数 / 序列：不可以）。
    #[must_use]
    pub const fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View)
    }
}

/// 一张表上的一个索引（右栏「索引」卡的数据源）。
///
/// 三引擎的取值口径：SQLite 走 `pragma_index_list` / `pragma_index_info`
/// （含 `sqlite_autoindex_*` 这类约束隐式建的索引），MySQL 走
/// `information_schema.STATISTICS`，PG 走 `pg_index` + `pg_attribute`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIndex {
    /// 索引名。
    pub name: String,
    /// 组成列（按索引内的顺序；表达式列在 PG 下为空串）。
    pub columns: Vec<String>,
    /// 是否唯一索引。
    pub unique: bool,
    /// 是否主键索引。
    pub primary: bool,
}

/// 一张表的行数**口径**：`COUNT(*)` 是精确值，系统统计是估算值。
///
/// 两者展示文案不同（「1 234 行」vs「约 1 234 行」），所以类型上就分开，
/// 免得 UI 层去猜这个数字可不可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCount {
    /// 精确计数（SQLite：没有系统统计信息，只能 `COUNT(*)`）。
    Exact(u64),
    /// 统计信息估算（PostgreSQL `pg_class.reltuples` / MySQL `information_schema.TABLES.TABLE_ROWS`）。
    Estimate(u64),
}

impl RowCount {
    /// 数值本身（两种口径同形取用）。
    #[must_use]
    pub const fn value(self) -> u64 {
        match self {
            Self::Exact(value) | Self::Estimate(value) => value,
        }
    }

    /// 是否只是估算值。
    #[must_use]
    pub const fn is_estimate(self) -> bool {
        matches!(self, Self::Estimate(_))
    }
}

/// 一条已建立的连接。**全部方法阻塞**，调用方负责放到后台线程。
pub enum Connection {
    /// SQLite（本地文件）。
    Sqlite(sqlite::SqliteConnection),
    /// PostgreSQL。
    Postgres(postgres::PostgresConnection),
    /// MySQL / MariaDB。
    Mysql(mysql::MysqlConnection),
    /// ClickHouse（HTTP 接口）。
    ClickHouse(clickhouse::ClickHouseConnection),
    /// Microsoft SQL Server（`tiberius`，驱动内部自管 runtime）。
    ///
    /// **Box 起来是有意的**：它自带一个 tokio runtime，整个结构体 ~1 KB，
    /// 而次大的变体不到 300 B——不装箱会让整个枚举按 1 KB 对齐、到处搬运。
    SqlServer(Box<mssql::MssqlConnection>),
    /// MongoDB（`mongodb`，同样是 async-only 驱动内部自管 runtime）。
    ///
    /// 不装箱：它只有 `Client` 句柄 + runtime + 几个字符串，与其它变体同量级。
    Mongo(mongo::MongoConnection),
    /// Elasticsearch（HTTP REST 接口，见 `drivers/elasticsearch.rs`）。
    ///
    /// 不装箱：两条 HTTP 引擎（本变体与 ClickHouse）都只有端点 + agent + 几个字符串。
    Elasticsearch(elasticsearch::ElasticsearchConnection),
}

impl Connection {
    /// 按档案建连；网络引擎带入口令（可空——有些库允许无密认证）。
    pub fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        cfg.validate()?;
        match cfg.kind {
            DatabaseKind::Sqlite => sqlite::SqliteConnection::connect(cfg).map(Self::Sqlite),
            DatabaseKind::Postgres => {
                postgres::PostgresConnection::connect(cfg, password).map(Self::Postgres)
            }
            DatabaseKind::Mysql => mysql::MysqlConnection::connect(cfg, password).map(Self::Mysql),
            DatabaseKind::ClickHouse => {
                clickhouse::ClickHouseConnection::connect(cfg, password).map(Self::ClickHouse)
            }
            DatabaseKind::SqlServer => mssql::MssqlConnection::connect(cfg, password)
                .map(|conn| Self::SqlServer(Box::new(conn))),
            DatabaseKind::Mongo => mongo::MongoConnection::connect(cfg, password).map(Self::Mongo),
            DatabaseKind::Elasticsearch => {
                elasticsearch::ElasticsearchConnection::connect(cfg, password)
                    .map(Self::Elasticsearch)
            }
        }
    }

    /// 引擎种类。
    #[must_use]
    pub const fn kind(&self) -> DatabaseKind {
        match self {
            Self::Sqlite(_) => DatabaseKind::Sqlite,
            Self::Postgres(_) => DatabaseKind::Postgres,
            Self::Mysql(_) => DatabaseKind::Mysql,
            Self::ClickHouse(_) => DatabaseKind::ClickHouse,
            Self::SqlServer(_) => DatabaseKind::SqlServer,
            Self::Mongo(_) => DatabaseKind::Mongo,
            Self::Elasticsearch(_) => DatabaseKind::Elasticsearch,
        }
    }

    /// 带外取消句柄（见 [`crate::cancel`]）：与连接锁无关，查询跑着时也能送「停」。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            Self::Sqlite(conn) => conn.cancel_handle(),
            Self::Postgres(conn) => conn.cancel_handle(),
            Self::Mysql(conn) => conn.cancel_handle(),
            Self::ClickHouse(conn) => conn.cancel_handle(),
            Self::SqlServer(conn) => conn.cancel_handle(),
            Self::Mongo(conn) => conn.cancel_handle(),
            Self::Elasticsearch(conn) => conn.cancel_handle(),
        }
    }

    /// 活性探测（每引擎一条最便宜的语句）。
    pub fn ping(&mut self) -> Result<(), DbError> {
        match self {
            Self::Sqlite(conn) => conn.ping(),
            Self::Postgres(conn) => conn.ping(),
            Self::Mysql(conn) => conn.ping(),
            Self::ClickHouse(conn) => conn.ping(),
            Self::SqlServer(conn) => conn.ping(),
            Self::Mongo(conn) => conn.ping(),
            Self::Elasticsearch(conn) => conn.ping(),
        }
    }

    /// 服务器版本描述（连接详情展示用）。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        match self {
            Self::Sqlite(conn) => conn.server_version(),
            Self::Postgres(conn) => conn.server_version(),
            Self::Mysql(conn) => conn.server_version(),
            Self::ClickHouse(conn) => conn.server_version(),
            Self::SqlServer(conn) => conn.server_version(),
            Self::Mongo(conn) => conn.server_version(),
            Self::Elasticsearch(conn) => conn.server_version(),
        }
    }

    /// 把这条会话切成**只读 / 可写**（连接级只读档，`docs/database-design.md` §4.8）。
    ///
    /// 前四引擎由**服务器**兜底拒绝写入，不是界面上的软拦截：
    /// SQLite `PRAGMA query_only`、PostgreSQL `default_transaction_read_only`、
    /// MySQL `SESSION TRANSACTION READ ONLY`、ClickHouse 每个请求带 `readonly=1`
    /// （HTTP 无会话，所以它是个**每请求都带上**的状态）。
    /// ⚠️ **SQL Server / MongoDB / Elasticsearch 是例外**：它们都没有会话级只读开关，
    /// 只读档由驱动在写语句**进引擎之前**拦下（见各驱动模块文档）。那一层同样不会漏放
    /// （未知操作一律当写，默认拒绝），但它不是服务器兜底——真要防住生产库仍然应该用
    /// 只读账号。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        match self {
            Self::Sqlite(conn) => conn.set_read_only(read_only),
            Self::Postgres(conn) => conn.set_read_only(read_only),
            Self::Mysql(conn) => conn.set_read_only(read_only),
            Self::ClickHouse(conn) => conn.set_read_only(read_only),
            Self::SqlServer(conn) => conn.set_read_only(read_only),
            Self::Mongo(conn) => conn.set_read_only(read_only),
            Self::Elasticsearch(conn) => conn.set_read_only(read_only),
        }
    }

    /// 记住界面选中的命名空间（左栏的库 / schema 下拉换项时调）。
    ///
    /// **只有 MongoDB 真的做点什么**：SQL 引擎的「当前库 / schema」是**服务端**状态
    /// （`search_path`、当前库），驱动侧无从也无需改写——编辑器里的 SQL 该怎么解析由
    /// 服务端决定，换下拉不会改写已写好的语句（其它工具同理）。而 MongoDB 的「当前库」
    /// 是**客户端概念**，不告诉驱动，编辑器里的命令（`{"find": "users"}`）就会落在旧库
    /// 或「未选定」上。其余引擎是空操作。
    pub fn set_namespace(&mut self, schema: Option<&str>) {
        if let Self::Mongo(conn) = self {
            conn.set_namespace(schema);
        }
    }

    /// 执行一段 SQL（可含多条语句），读语句的行在 `max_rows` 处截断。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        match self {
            Self::Sqlite(conn) => conn.execute(sql, max_rows),
            Self::Postgres(conn) => conn.execute(sql, max_rows),
            Self::Mysql(conn) => conn.execute(sql, max_rows),
            Self::ClickHouse(conn) => conn.execute(sql, max_rows),
            Self::SqlServer(conn) => conn.execute(sql, max_rows),
            Self::Mongo(conn) => conn.execute(sql, max_rows),
            Self::Elasticsearch(conn) => conn.execute(sql, max_rows),
        }
    }

    /// 可选的命名空间清单（左栏「库 / schema」下拉的数据源）。
    ///
    /// 各引擎口径：SQLite 给 `pragma database_list`（`main` / `temp` / 附加库），
    /// PG 给 `pg_namespace`（剔除系统 schema），MySQL 给 `information_schema.schemata`
    /// （剔除 `mysql` / `performance_schema` / `sys` / `information_schema`），
    /// ClickHouse 给 `system.databases`（剔重复的 `INFORMATION_SCHEMA`，保留 `system`
    /// —— 它是排障的一等公民），SQL Server 给 `sys.schemas`（剔内置角色 schema），
    /// MongoDB 给 `listDatabases`。
    /// ⚠️ **Elasticsearch 恒空**：索引是平铺的，没有库 / schema 这一层（见
    /// `drivers/elasticsearch.rs`）。
    /// 一律剔空名字、按名排序。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.list_schemas(),
            Self::Postgres(conn) => conn.list_schemas(),
            Self::Mysql(conn) => conn.list_schemas(),
            Self::ClickHouse(conn) => conn.list_schemas(),
            Self::SqlServer(conn) => conn.list_schemas(),
            Self::Mongo(conn) => conn.list_schemas(),
            Self::Elasticsearch(conn) => conn.list_schemas(),
        }
    }

    /// 一个命名空间里的目录对象；`schema` 为 `None` 时取当前命名空间。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.catalog(schema),
            Self::Postgres(conn) => conn.catalog(schema),
            Self::Mysql(conn) => conn.catalog(schema),
            Self::ClickHouse(conn) => conn.catalog(schema),
            Self::SqlServer(conn) => conn.catalog(schema),
            Self::Mongo(conn) => conn.catalog(schema),
            Self::Elasticsearch(conn) => conn.catalog(schema),
        }
    }

    /// 一张表的列（名称 + 类型）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_columns(table),
            Self::Postgres(conn) => conn.table_columns(table),
            Self::Mysql(conn) => conn.table_columns(table),
            Self::ClickHouse(conn) => conn.table_columns(table),
            Self::SqlServer(conn) => conn.table_columns(table),
            Self::Mongo(conn) => conn.table_columns(table),
            Self::Elasticsearch(conn) => conn.table_columns(table),
        }
    }

    /// 一张表的分页浏览（`limit` 行，`offset` 起始；行上限即 `limit`）。
    ///
    /// `order_by` 是**稳定序**的列（调用方传主键；空则不加 `ORDER BY`）。
    /// 不加稳定序的分页在 PG / MySQL 上会翻页重复或漏行——见
    /// `docs/database-design.md` §4.5。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
    ) -> Result<ResultSet, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_page(table, limit, offset, order_by),
            Self::Postgres(conn) => conn.table_page(table, limit, offset, order_by),
            Self::Mysql(conn) => conn.table_page(table, limit, offset, order_by),
            Self::ClickHouse(conn) => conn.table_page(table, limit, offset, order_by),
            Self::SqlServer(conn) => conn.table_page(table, limit, offset, order_by),
            Self::Mongo(conn) => conn.table_page(table, limit, offset, order_by),
            Self::Elasticsearch(conn) => conn.table_page(table, limit, offset, order_by),
        }
    }

    /// 一张表的主键列名（按声明顺序；无主键表返回空 `Vec`，编辑层据此判只读）。
    ///
    /// ClickHouse 恒返回空：它的 `PRIMARY KEY` 是排序表达式、`UPDATE` 是异步
    /// mutation，没有可供网格编辑定位的行主键（见 `drivers/clickhouse.rs`）。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_primary_key(table),
            Self::Postgres(conn) => conn.table_primary_key(table),
            Self::Mysql(conn) => conn.table_primary_key(table),
            Self::ClickHouse(conn) => conn.table_primary_key(table),
            Self::SqlServer(conn) => conn.table_primary_key(table),
            Self::Mongo(conn) => conn.table_primary_key(table),
            Self::Elasticsearch(conn) => conn.table_primary_key(table),
        }
    }

    /// 改一格：按主键定位行，把 `column` 更新为文本 `value`（`None` = 写 `NULL`）。
    ///
    /// 主键值按「展示文本」比较（各引擎 `CAST(pk AS TEXT)`），浮点展示格式差异
    /// 可能匹配不到行——命中 0 行返回 `Ok(0)`，由调用方提示用户刷新重试。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        if pk.is_empty() {
            return Err(DbError::Query("table has no primary key".into()));
        }
        match self {
            Self::Sqlite(conn) => conn.update_cell(table, pk, column, value),
            Self::Postgres(conn) => conn.update_cell(table, pk, column, value),
            Self::Mysql(conn) => conn.update_cell(table, pk, column, value),
            Self::ClickHouse(conn) => conn.update_cell(table, pk, column, value),
            Self::SqlServer(conn) => conn.update_cell(table, pk, column, value),
            Self::Mongo(conn) => conn.update_cell(table, pk, column, value),
            Self::Elasticsearch(conn) => conn.update_cell(table, pk, column, value),
        }
    }

    /// 按主键删行（表浏览的删除按钮走这里）。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &PkValues) -> Result<u64, DbError> {
        if pk.is_empty() {
            return Err(DbError::Query("table has no primary key".into()));
        }
        match self {
            Self::Sqlite(conn) => conn.delete_rows(table, pk),
            Self::Postgres(conn) => conn.delete_rows(table, pk),
            Self::Mysql(conn) => conn.delete_rows(table, pk),
            Self::ClickHouse(conn) => conn.delete_rows(table, pk),
            Self::SqlServer(conn) => conn.delete_rows(table, pk),
            Self::Mongo(conn) => conn.delete_rows(table, pk),
            Self::Elasticsearch(conn) => conn.delete_rows(table, pk),
        }
    }

    /// **一个事务**里按主键删多行（结果网格多选删除走这里）：要么全删、要么一行都不动。
    ///
    /// 空 `keys` 直接 `Ok(0)`（不白开事务）；任一行的主键为空则整批拒绝。
    pub fn delete_row_set(&mut self, table: &TableRef, keys: &[PkValues]) -> Result<u64, DbError> {
        if keys.is_empty() {
            return Ok(0);
        }
        if keys.iter().any(|pk| pk.is_empty()) {
            return Err(DbError::Query("table has no primary key".into()));
        }
        match self {
            Self::Sqlite(conn) => conn.delete_row_set(table, keys),
            Self::Postgres(conn) => conn.delete_row_set(table, keys),
            Self::Mysql(conn) => conn.delete_row_set(table, keys),
            Self::ClickHouse(conn) => conn.delete_row_set(table, keys),
            Self::SqlServer(conn) => conn.delete_row_set(table, keys),
            Self::Mongo(conn) => conn.delete_row_set(table, keys),
            Self::Elasticsearch(conn) => conn.delete_row_set(table, keys),
        }
    }

    /// **一个事务**里改多格（暂存式编辑的提交路径）：要么全成、要么全回滚。
    ///
    /// 空 `edits` 直接 `Ok(0)`（不白开事务）；任一格定位不到行时返回的 `affected`
    /// 会小一格——命中行数的差额由调用方核对并提示刷新。
    pub fn update_cells(&mut self, table: &TableRef, edits: &[CellEdit]) -> Result<u64, DbError> {
        if edits.is_empty() {
            return Ok(0);
        }
        if edits.iter().any(|edit| edit.pk.is_empty()) {
            return Err(DbError::Query("table has no primary key".into()));
        }
        match self {
            Self::Sqlite(conn) => conn.update_cells(table, edits),
            Self::Postgres(conn) => conn.update_cells(table, edits),
            Self::Mysql(conn) => conn.update_cells(table, edits),
            Self::ClickHouse(conn) => conn.update_cells(table, edits),
            Self::SqlServer(conn) => conn.update_cells(table, edits),
            Self::Mongo(conn) => conn.update_cells(table, edits),
            Self::Elasticsearch(conn) => conn.update_cells(table, edits),
        }
    }

    /// **一个事务**里提交一次暂存式编辑（改格 + 新增行混排）：要么全成、要么全回滚。
    ///
    /// 这是「暂存 → 提交」模型的落地点（`docs/database-design.md` §4.5）：界面上攒下的
    /// `pending_edits` / `pending_inserts` 一次交过来，**一个事务**跑完。拆成
    /// [`Self::update_cells`] + [`Self::insert_rows`] 两次调用会变成两个事务，
    /// 插入失败时改动已经落了库——所以合成一个入口。
    ///
    /// 两边都为空时不白开事务；任一格的定位主键为空则整批拒绝（一个都不动）。
    pub fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[CellEdit],
        inserts: &[InsertRow],
    ) -> Result<BatchOutcome, DbError> {
        if edits.is_empty() && inserts.is_empty() {
            return Ok(BatchOutcome::default());
        }
        if edits.iter().any(|edit| edit.pk.is_empty()) {
            return Err(DbError::Query("table has no primary key".into()));
        }
        for row in inserts {
            if row.columns.is_empty() {
                return Err(DbError::Query("no columns to insert".into()));
            }
            if row.columns.len() != row.values.len() {
                return Err(DbError::Query(
                    "row width does not match column list".into(),
                ));
            }
        }
        match self {
            Self::Sqlite(conn) => conn.apply_batch(table, edits, inserts),
            Self::Postgres(conn) => conn.apply_batch(table, edits, inserts),
            Self::Mysql(conn) => conn.apply_batch(table, edits, inserts),
            Self::ClickHouse(conn) => conn.apply_batch(table, edits, inserts),
            Self::SqlServer(conn) => conn.apply_batch(table, edits, inserts),
            Self::Mongo(conn) => conn.apply_batch(table, edits, inserts),
            Self::Elasticsearch(conn) => conn.apply_batch(table, edits, inserts),
        }
    }

    /// 一张表的建表 DDL（SQLite / MySQL / ClickHouse 返回引擎原文，PG 由目录重构近似版）。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_ddl(table),
            Self::Postgres(conn) => conn.table_ddl(table),
            Self::Mysql(conn) => conn.table_ddl(table),
            Self::ClickHouse(conn) => conn.table_ddl(table),
            Self::SqlServer(conn) => conn.table_ddl(table),
            Self::Mongo(conn) => conn.table_ddl(table),
            Self::Elasticsearch(conn) => conn.table_ddl(table),
        }
    }

    /// 一张表上的索引（右栏「索引」卡）。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_indexes(table),
            Self::Postgres(conn) => conn.table_indexes(table),
            Self::Mysql(conn) => conn.table_indexes(table),
            Self::ClickHouse(conn) => conn.table_indexes(table),
            Self::SqlServer(conn) => conn.table_indexes(table),
            Self::Mongo(conn) => conn.table_indexes(table),
            Self::Elasticsearch(conn) => conn.table_indexes(table),
        }
    }

    /// 一张表的行数（口径见 [`RowCount`]）。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_row_count(table),
            Self::Postgres(conn) => conn.table_row_count(table),
            Self::Mysql(conn) => conn.table_row_count(table),
            Self::ClickHouse(conn) => conn.table_row_count(table),
            Self::SqlServer(conn) => conn.table_row_count(table),
            Self::Mongo(conn) => conn.table_row_count(table),
            Self::Elasticsearch(conn) => conn.table_row_count(table),
        }
    }

    /// 批量参数化插入（CSV 导入与「新增行」都走这里）：`columns` 是目标列，
    /// `rows` 每行等宽，`None` 单元格写 `NULL`。全部文本参数，按引擎的类型亲和落地。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        if columns.is_empty() {
            return Err(DbError::Query("no columns to insert".into()));
        }
        for row in rows {
            if row.len() != columns.len() {
                return Err(DbError::Query(
                    "row width does not match column list".into(),
                ));
            }
        }
        match self {
            Self::Sqlite(conn) => conn.insert_rows(table, columns, rows),
            Self::Postgres(conn) => conn.insert_rows(table, columns, rows),
            Self::Mysql(conn) => conn.insert_rows(table, columns, rows),
            Self::ClickHouse(conn) => conn.insert_rows(table, columns, rows),
            Self::SqlServer(conn) => conn.insert_rows(table, columns, rows),
            Self::Mongo(conn) => conn.insert_rows(table, columns, rows),
            Self::Elasticsearch(conn) => conn.insert_rows(table, columns, rows),
        }
    }

    /// 关闭连接（各驱动随自身 drop；这里给调用方一个显式时机）。
    pub fn close(self) {}
}

/// URL 查询串 / 路径成分编码（unreserved 原样，其余百分号编码）。
///
/// 走 HTTP 的两条引擎（ClickHouse 的查询串、Elasticsearch 的索引名与路径）都靠它：
/// 值来自用户输入或目录内省，**必须编码**——否则一个含 `&` 的库名就能往查询串里
/// 塞进别的参数，含 `/` 的索引名能把路径拐到别的端点上。
pub(crate) fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 表名进 SQL 文本时的引号包裹（按引擎的引号字符）。
/// 名字本身来自目录内省，不做 fuzzy 匹配；包含引擎引号字符的名字不合法，
/// 直接拒绝——比生成被拆散的 SQL 强。
pub(crate) fn quote_identifier(name: &str, quote: char) -> Result<String, DbError> {
    if name.contains(quote) || name.contains('\0') {
        return Err(DbError::Config(format!(
            "identifier contains quote char {quote}: {name}"
        )));
    }
    Ok(format!("{quote}{name}{quote}"))
}

/// 限定名的引号包裹：逐段加引号再点连接（`"schema"."name"`）。
///
/// 只给 `schema` 加引号而不给 `name` 加，是 SQL 注入面；整串一起加又会让
/// `schema.name` 被当成**一个**标识符。逐段加是唯一正确的写法。
pub(crate) fn quote_qualified(table: &TableRef, quote: char) -> Result<String, DbError> {
    let parts = table.parts();
    if parts.is_empty() {
        return Err(DbError::Config("table name is empty".into()));
    }
    parts
        .into_iter()
        .map(|part| quote_identifier(part, quote))
        .collect::<Result<Vec<_>, _>>()
        .map(|quoted| quoted.join("."))
}

/// 分页 / 浏览的一档排序：列 + 方向。
///
/// 第一档通常是用户点的列头，其后跟主键——**主键那一档是稳定性的来源**：
/// 只按用户列排时同值行的相对顺序仍未定义，翻页照样会重复 / 漏行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// 列名。
    pub column: String,
    /// 是否降序。
    pub descending: bool,
}

impl OrderBy {
    /// 升序一档。
    #[must_use]
    pub fn asc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: false,
        }
    }

    /// 降序一档。
    #[must_use]
    pub fn desc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: true,
        }
    }
}

/// 拼 `ORDER BY` 子句（空列表 → 空串）。
///
/// 列名来自主键 / 列内省，仍逐段加引号——内省结果理论上一定合法，但这里**不假设**
/// 上游可信（与 [`quote_identifier`] 的取向一致）。
pub(crate) fn order_by_clause(order_by: &[OrderBy], quote: char) -> Result<String, DbError> {
    if order_by.is_empty() {
        return Ok(String::new());
    }
    let columns = order_by
        .iter()
        .map(|entry| {
            let quoted = quote_identifier(&entry.column, quote)?;
            Ok(if entry.descending {
                format!("{quoted} DESC")
            } else {
                quoted
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    Ok(format!(" ORDER BY {}", columns.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 引号包裹：合法名字成对包裹，含引号字符的名字直接拒绝。
    #[test]
    fn identifier_quoting() {
        assert_eq!(quote_identifier("users", '"').expect("合法"), "\"users\"");
        assert_eq!(
            quote_identifier("订单明细", '`').expect("合法"),
            "`订单明细`"
        );
        assert!(quote_identifier("we\"ird", '"').is_err());
        assert!(quote_identifier("we`ird", '`').is_err());
    }

    /// 限定名：逐段加引号，空段剔除，全空拒绝。
    #[test]
    fn qualified_quoting() {
        let bare = TableRef::bare("users");
        assert_eq!(quote_qualified(&bare, '"').expect("合法"), "\"users\"");

        let qualified = TableRef::in_schema("public", "users");
        assert_eq!(
            quote_qualified(&qualified, '"').expect("合法"),
            "\"public\".\"users\""
        );
        assert_eq!(qualified.display(), "public.users");

        // 空 schema 段等于没限定（不是 `""."users"`）。
        let blank = TableRef {
            schema: Some(String::new()),
            name: "users".into(),
        };
        assert_eq!(quote_qualified(&blank, '"').expect("合法"), "\"users\"");

        // 全空直接拒绝，不生成 `""` 这种会被解析成别的对象的 SQL。
        assert!(quote_qualified(&TableRef::default(), '"').is_err());
        // 任一段含引号字符 → 拒绝（不能只检查最后一段）。
        let bad = TableRef::in_schema("we\"ird", "users");
        assert!(quote_qualified(&bad, '"').is_err());
    }

    /// `ORDER BY` 子句：空列表不产生子句；多档按序逗号连接、逐列加引号、降序带 `DESC`。
    #[test]
    fn order_by_building() {
        assert_eq!(order_by_clause(&[], '"').expect("空"), "");
        let columns = vec![OrderBy::desc("a"), OrderBy::asc("b c")];
        assert_eq!(
            order_by_clause(&columns, '"').expect("合法"),
            " ORDER BY \"a\" DESC, \"b c\""
        );
        let bad = vec![OrderBy::asc("we\"ird")];
        assert!(order_by_clause(&bad, '"').is_err());
    }

    /// URL 成分编码：unreserved 原样，`&` / `=` / 空格 / 非 ASCII 必须编码，
    /// 否则一个含 `&` 的库名就能往查询串里塞参数。
    #[test]
    fn url_components_are_encoded() {
        assert_eq!(url_encode("default"), "default");
        assert_eq!(url_encode("logs-2026.10"), "logs-2026.10");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("我的库"), "%E6%88%91%E7%9A%84%E5%BA%93");
    }
}
