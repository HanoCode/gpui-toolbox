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

/// 切库（[`Connection::switch_database`]）的两种结局。
///
/// 分开两种而不是都回 `Ok(())`：PostgreSQL 的 `dbname` 是**建连参数**，在已建立的
/// 会话上根本切不了（服务端没有这条语句）。让驱动回 `NeedsReconnect`、由握着档案与
/// 口令的上层重连，好过在驱动侧假装修好了 —— 后者会留下一句「切了但 SQL 还打在旧库」
/// 的静默错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// 已在当前会话上生效。
    Applied,
    /// 本引擎只能在建连时决定库，调用方需要重建连接。
    NeedsReconnect,
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

/// 库级目录条目：不属于某张表的对象（触发器 / 事件 / 用户与权限）。
///
/// 统一用一个结构而不是三个：左栏树的分类节点只关心「名字 + 一行说明」，
/// 再多的字段对树展示没有意义（触发器正文 / 事件定义要看得见的话走 SQL 编辑器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseObject {
    /// 名称（触发器名 / 事件名 / 用户名）。
    pub name: String,
    /// 一行说明：触发器是**挂在哪张表**上，用户是**登录属性**（MySQL 的 host、
    /// PG 的 superuser / can login），事件是**调度状态**。给不出就 `None`。
    pub detail: Option<String>,
}

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
    /// 存储过程（MySQL `ROUTINE_TYPE='PROCEDURE'`、PG 同、SQL Server `type='P'`）。
    Procedure,
    /// 函数（MySQL `ROUTINE_TYPE='FUNCTION'`、PG 同、SQL Server `FN`/`IF`/`TF`）。
    Function,
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

/// 一张表上的一条外键（右栏「外键」卡的数据源）。
///
/// 复合外键（多列）把 `columns` / `ref_columns` 按声明顺序对齐存放。
/// SQLite 的外键**没有约束名**（`pragma foreign_key_list` 不给），
/// `name` 落空串，展示层以「指向哪张表」为主文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableForeignKey {
    /// 约束名（SQLite 恒空串）。
    pub name: String,
    /// 本表的引用列（按声明顺序）。
    pub columns: Vec<String>,
    /// 被引用的表名。
    pub ref_table: String,
    /// 被引用的列（与 `columns` 按序对齐）。
    pub ref_columns: Vec<String>,
    /// 更新动作（`CASCADE` / `SET NULL` / `NO ACTION` …）。
    pub on_update: String,
    /// 删除动作（同上）。
    pub on_delete: String,
}

/// 一张表上的一条触发器（右栏「触发器」卡的数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableTrigger {
    /// 触发器名。
    pub name: String,
    /// 触发时机（`BEFORE` / `AFTER` / `INSTEAD OF`；T-SQL 没有 `BEFORE`，只有后两者）。
    pub timing: String,
    /// 触发事件（`INSERT` / `UPDATE` / `DELETE` / `TRUNCATE`，可多个、逗号连接）。
    pub event: String,
}

/// 一张表的**列级元数据**（表结构编辑器的数据源；比 [`Column`] 富一档）。
///
/// 属性槽位是 `Option`：`None` = 引擎拿不到（SQLite 没注释、PG 没有字符集概念）。
/// `comment` 与 [`Column::comment`] 同约定：引擎没有就留空串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumnMeta {
    /// 列名。
    pub name: String,
    /// 引擎报的类型名（含长度修饰，口径同 [`Column::type_name`]）。
    pub type_name: String,
    /// 列注释（引擎没有就空串）。
    pub comment: String,
    /// 可空（拿不到为 `None`）。
    pub nullable: Option<bool>,
    /// 默认值原文（引擎报 `NULL` 也照抄；没有 / 拿不到为 `None`）。
    pub default: Option<String>,
    /// 字符集（仅 MySQL 有；拿不到为 `None`）。
    pub character_set: Option<String>,
    /// 排序规则（MySQL / PG 有；拿不到为 `None`）。
    pub collation: Option<String>,
}

impl TableColumnMeta {
    /// 从裸 [`Column`] 升格：五个属性全空（Mongo / ES 这类「列」只是名字的引擎用）。
    #[must_use]
    pub fn from_column(column: Column) -> Self {
        Self {
            name: column.name,
            type_name: column.type_name,
            comment: column.comment,
            nullable: None,
            default: None,
            character_set: None,
            collation: None,
        }
    }
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

/// 实例概览里的一项「名字 — 值」。
///
/// 引擎之间**共有的**那一小撮（版本、启动时间、当前库…）由
/// [`InstanceOverview::version`] 等字段直接承载；各引擎自己才有的那些
/// （字符集、页大小、日志模式…）装进 [`InstanceOverview::facts`]，界面照原样列出来
/// ——内核不替界面决定「哪些值得看」，也不做单位换算（换算要跟界面语言走）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceFact {
    /// 项目名（引擎口径的原文，如 `journal_mode` / `Uptime`）。
    pub name: String,
    /// 当前值（原样字符串）。
    pub value: String,
}

/// 实例概览：连的是哪台服务器、什么版本、跑了多久、有哪些库。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceOverview {
    /// 服务器版本描述。
    pub version: String,
    /// 当前库（没选定库的引擎为空串）。
    pub database: String,
    /// 当前登录身份。
    pub user: String,
    /// 已运行时长（引擎能报才有；SQLite 无服务端，恒 `None`）。
    pub uptime: Option<String>,
    /// 其余键值项（引擎相关，顺序即展示顺序）。
    pub facts: Vec<InstanceFact>,
}

/// 实例级内省要取的**哪张表**。
///
/// 两张表形态相同（表头 + 行），差别只在语义：指标是「数值」，检查器是「会话 / 对象」。
/// 内核用同一个出口是为了让**引擎只实现一次取值**，也便于界面用同一套表格渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceView {
    /// 实例指标：连接数、命中率、事务量、体量这类可比较的数字。
    Metrics,
    /// 实例检查器：服务端会话 / 连接现状（SQLite 为连接已挂载的数据库）。
    Processes,
}

impl InstanceView {
    /// 出错的上下文描述（内核错误一律英文，见 `AGENTS.md`）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Metrics => "instance metrics",
            Self::Processes => "instance processes",
        }
    }
}

/// 实例级内省取回的**通用表**：表头 + 行。
///
/// 为什么不是强类型（`Vec<ServerProcess>` 之类）：七引擎的列集差异太大（MySQL 的
/// `Command` / `Time`、PG 的 `wait_event`、SQLite 的 `file`），强类型要么每个引擎一个
/// 结构体（界面就得写七套渲染），要么把列砍到交集（那就丢信息）。表头 + 行既保真
/// 又只让界面写一套表格。
///
/// **不带「口径说明」这类散文**：内核不吐面向用户的文案（见 `AGENTS.md` 第 3 节：
/// 内核不出中文，界面要提示由界面自己 `trf`）。引擎侧的解释一律由界面按引擎拼。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceTable {
    /// 表头（与每行的列数等长）。
    pub columns: Vec<String>,
    /// 数据行。
    pub rows: Vec<Vec<String>>,
}

impl InstanceTable {
    /// 表里**数值型**的那些项（时间序列的纵轴来源）。
    ///
    /// 从同一张表里筛，不另开一份指标清单：加一项指标只改各驱动里那一处，时序与表格
    /// 永远同源。筛法是「值能不能解析成 `f64`」——`database_size`（`8493 kB`）、
    /// PG 命中率里的 `-` 这类**不是数**的项自然被排除：与其猜单位，不如让它们留在
    /// 明细表里（界面上那两处一个画图、一个列清单，正好互补）。
    ///
    /// 顺序即表格顺序（引擎已按稳定序输出，别在这里重排）；键与值都**去掉首尾空白**
    /// ——数值是 `parse` 前必须的，键跟着一起收口，免得同一个指标在序列里出现两个键。
    #[must_use]
    pub fn numeric(&self) -> Vec<InstanceMetric> {
        let Some(key_index) = self.columns.iter().position(|column| column == "metric") else {
            return Vec::new();
        };
        let Some(value_index) = self.columns.iter().position(|column| column == "value") else {
            return Vec::new();
        };
        self.rows
            .iter()
            .filter_map(|row| {
                let key = row.get(key_index)?.trim();
                let value = row.get(value_index)?.trim().parse::<f64>().ok()?;
                Some(InstanceMetric {
                    key: key.to_owned(),
                    value,
                })
            })
            .collect()
    }
}

/// 一条**数值型**实例指标（[`InstanceTable::numeric`] 的产出）。
///
/// 只有数值进来：时间序列的纵轴是数，字符串项（体量文案、字符集）没有坐标系可放。
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceMetric {
    /// 指标名（引擎口径原文，如 `Threads_connected` / `numbackends`）。
    pub key: String,
    /// 采样值。
    pub value: f64,
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

    /// 记住界面选中的命名空间（schema 下拉换项时调）。
    ///
    /// 只负责 **schema 这一层**（PG / MSSQL 库内的 `pg_namespace` / `sys.schemas`），
    /// 且**只有 MongoDB 真的做点什么**：Mongo 没有 schema 层，「当前库」是客户端
    /// 概念，不告诉驱动，编辑器里的命令（`{"find": "users"}`）就会落在旧库上。
    ///
    /// ⚠️ **「当前库」请走 [`Self::switch_database`]** —— 那个才真的改服务端会话。
    /// 早期版本只在这里记一笔，理由是「当前库是服务端状态、驱动无从改写」，那句话
    /// 对 `search_path` 成立、对「连上时没选库」的连接**不成立**：库名可留空建连的
    /// 情况下，用户点了库却不发 `USE`，之后每条语句都会被服务端回
    /// `No database selected` —— 界面「切了」、查询没切。
    pub fn set_namespace(&mut self, schema: Option<&str>) {
        if let Self::Mongo(conn) = self {
            conn.set_namespace(schema);
        }
    }

    /// 切换会话的**当前库**。
    ///
    /// - [`SwitchOutcome::Applied`]：已经在会话上生效，后续语句会落在这个库上；
    /// - [`SwitchOutcome::NeedsReconnect`]：本引擎在已建立的会话上切不了库
    ///   （PostgreSQL 的 `dbname` 是建连参数），**调用方必须重建连接**——
    ///   驱动侧拿不到口令，只能由上层带着档案重连。
    ///
    /// 库名不存在 / 无权限时不在这里兜底：交给服务端报错（MySQL `USE`、
    /// T-SQL `USE` 都会自己拒），客户端「猜」只会把真因盖住。
    pub fn switch_database(&mut self, name: &str) -> Result<SwitchOutcome, DbError> {
        match self {
            Self::Sqlite(conn) => {
                conn.use_database(name)?;
                Ok(SwitchOutcome::Applied)
            }
            // PG 的库是建连参数（`dbname=`），一个会话终身绑死，只能重连。
            Self::Postgres(_) => Ok(SwitchOutcome::NeedsReconnect),
            Self::Mysql(conn) => {
                conn.use_database(name)?;
                Ok(SwitchOutcome::Applied)
            }
            Self::ClickHouse(conn) => {
                conn.use_database(name);
                Ok(SwitchOutcome::Applied)
            }
            Self::SqlServer(conn) => {
                conn.use_database(name)?;
                Ok(SwitchOutcome::Applied)
            }
            Self::Mongo(conn) => {
                conn.use_database(name);
                Ok(SwitchOutcome::Applied)
            }
            Self::Elasticsearch(conn) => {
                conn.use_database(name)?;
                Ok(SwitchOutcome::Applied)
            }
        }
    }

    /// 这条连接上有**哪些库**（不是「有哪些 schema」——那是 [`Self::list_schemas`]）。
    ///
    /// 「连上了但什么都没看到」的根因就是这个问题的答案原先拿不到：库名可留空建连
    /// 时，`list_schemas()` 对 PG / MSSQL 给的是**库内**的 schema（服务端在有默认库
    /// 的前提下才给得出），对 MySQL 给的是剔过系统库的业务清单。用户最先想问的
    /// 「这台服务器上有哪些库」，得由这里回答。
    ///
    /// Elasticsearch 没有库这一层，恒空（上层据此不画库节点）。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.list_databases(),
            Self::Postgres(conn) => conn.list_databases(),
            Self::Mysql(conn) => conn.list_databases(),
            Self::ClickHouse(conn) => conn.list_databases(),
            Self::SqlServer(conn) => conn.list_databases(),
            Self::Mongo(conn) => conn.list_databases(),
            Self::Elasticsearch(conn) => conn.list_databases(),
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

    /// 执行一段 SQL，且**每一条语句都必须被证明只读**；否则整体拒绝，一条都不跑。
    ///
    /// 这是给「上层替用户跑 SQL」的场景准备的闸门（`docs/ai-design.md` §10.4 的
    /// `run_sql_readonly`）：AI 生成的语句不该只拦在界面上——界面可能被绕过、被改写，
    /// 而这一层与 `execute` 是**同一条**执行路径，拦截点紧贴真正下发的地方。
    ///
    /// 与 [`crate::sqlintel::prove_read_only`] 的关系：那边给的是"这条语句能否被证明
    /// 只读"（语法解析 + AST 判族），这里把它变成**执行前的一道硬闸**：
    ///
    /// - 一段里**任何**一条语句证明不了只读 → `Err(DbError::Unsupported)`，**一条都不执行**
    ///   （不做"能跑的跑、不能跑的跳过"：那种半执行状态最难收拾）；
    /// - 空串（或只有注释）同样按拒绝处理，不静默返回空结果；
    /// - **带已知副作用**的语句（[`crate::sqlintel::has_write_side_effects`]：`nextval` /
    ///   `setval` / `dblink*` / `pg_terminate_backend` / 取锁的 `SELECT … FOR UPDATE` 等）
    ///   同样拒绝——它们是「读形状」却会改状态 / 阻塞他人，不属自动执行的安全集合；
    /// - 证明只读的语句仍走 [`Connection::execute`]，因此行上限、截断、取消与超时
    ///   全部与手动执行一致。
    ///
    /// ⚠️ 证明是**保守**的：解析不了的语句一律拿不到证明（宁可多拒一次，不可漏放一次）。
    /// 需要"用户点头就能跑"的写操作请走 `execute` —— 那是另一条路，别放宽这里。
    pub fn execute_read_only(
        &mut self,
        sql: &str,
        max_rows: usize,
    ) -> Result<ExecOutcome, DbError> {
        let statements = crate::sqlintel::split_statements(sql);
        if statements.is_empty() {
            return Err(DbError::Unsupported("no statement to execute".into()));
        }
        if let Some(rejected) = statements
            .iter()
            .find(|stmt| !crate::sqlintel::prove_read_only(stmt))
        {
            return Err(DbError::Unsupported(format!(
                "read-only execution refused a statement that is not provably read-only: {rejected}"
            )));
        }
        // 第二道：只读形状但带副作用（改序列 / 取锁 / 后端控制…）也不自动跑。
        if let Some(rejected) = statements
            .iter()
            .find(|stmt| crate::sqlintel::has_write_side_effects(stmt))
        {
            return Err(DbError::Unsupported(format!(
                "read-only execution refused a statement with side effects: {rejected}"
            )));
        }
        self.execute(sql, max_rows)
    }

    /// 分页**读取**一段 SQL：从第 `offset` 行起取 `limit` 行（结果区「加载更多」的内核原语）。
    ///
    /// 实现口径：把用户那条语句包进外层 `LIMIT/OFFSET`（各引擎按自己的语法拼，见各驱动的
    /// `execute_paged`），并**多取一行**探测「还有没有下一页」——`truncated` = 本页之后
    /// 仍有更多行。调用方据此决定「加载更多」按钮的可见性，因此它同时是**行上限提示**
    /// 的判据：`truncated` 为真且有分页能力，才允许续拉。
    ///
    /// 三条边界（与 [`Self::execute`] 的分工）：
    /// - **只服务读路径**：写路径仍走单事务（[`Self::execute`] / [`Self::apply_batch`]），
    ///   本方法不参与任何写语义；
    /// - **只接受单条语句**：多语句批次没有「第 N 行起」的语义，也避免重跑批次把写语句
    ///   再执行一遍，直接 [`DbError::Unsupported`]；
    /// - **能力判定单一来源**：能不能续拉只看 [`DatabaseKind::supports_paged_read`]，
    ///   SQL Server / MongoDB / Elasticsearch 明确 [`DbError::Unsupported`]（界面据同一
    ///   判据不提供「加载更多」，两边不再各写一套）。
    ///
    /// SQL Server 被归为不支持的理由：其 `OFFSET/FETCH` 强制 `ORDER BY`，而派生表又不能
    /// 带 `ORDER BY`，外层只能 `ORDER BY (SELECT NULL)`——顺序不定会让翻页重复 / 漏行。
    /// 与其返回可能错的页，不如显式拒绝（见 [`crate::drivers::mssql`]）。
    ///
    /// ⚠️ 结果顺序只有在语句自带**确定序**（`ORDER BY` 唯一键）时才稳定；不带确定序时
    /// 翻页可能重复 / 漏行——与 [`Self::table_page`] 同一口径（`docs/database-design.md` §4.5）。
    pub fn execute_paged(
        &mut self,
        sql: &str,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet, DbError> {
        let kind = self.kind();
        if !kind.supports_paged_read() {
            return Err(paged_unsupported(kind.wire_name()));
        }
        match self {
            Self::Sqlite(conn) => conn.execute_paged(sql, offset, limit),
            Self::Postgres(conn) => conn.execute_paged(sql, offset, limit),
            Self::Mysql(conn) => conn.execute_paged(sql, offset, limit),
            Self::ClickHouse(conn) => conn.execute_paged(sql, offset, limit),
            // 这几条已被上面的 `supports_paged_read()` 排除；显式写全保证穷尽、
            // 且即便能力表与分发漂移也依然拒绝（fail-closed）。
            Self::SqlServer(_) | Self::Mongo(_) | Self::Elasticsearch(_) => {
                Err(paged_unsupported(kind.wire_name()))
            }
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

    /// 一个命名空间里的**触发器**清单（名字 + 挂在哪张表上）。
    ///
    /// 没有触发器概念的引擎（ClickHouse / Mongo / ES）返回空清单——「没有」
    /// 不是错误，左栏树直接不显示这个分类节点。
    pub fn list_triggers(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.list_triggers(schema),
            Self::Postgres(conn) => conn.list_triggers(schema),
            Self::Mysql(conn) => conn.list_triggers(schema),
            Self::SqlServer(conn) => conn.list_triggers(schema),
            Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => Ok(Vec::new()),
        }
    }

    /// 一个命名空间里的**索引**清单（名字 + 建在哪张表上）。
    ///
    /// 与 [`Self::table_indexes`] 的分工：这条给「库 → 索引」这一层（左栏树的分类
    /// 节点），那条给「表 → 索引」（右栏的索引卡）。没有命名空间级索引概念的引擎
    /// （ClickHouse / Mongo / ES）返回空清单——「没有」不是错误，树里不画这个节点。
    pub fn list_indexes(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.list_indexes(schema),
            Self::Postgres(conn) => conn.list_indexes(schema),
            Self::Mysql(conn) => conn.list_indexes(schema),
            Self::SqlServer(conn) => conn.list_indexes(schema),
            Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => Ok(Vec::new()),
        }
    }

    /// 一个命名空间里的**事件**清单（MySQL `CREATE EVENT` 的那套调度对象）。
    ///
    /// 只有 MySQL 有独立的事件调度器；其余引擎恒空（PG 的「事件」是
    /// LISTEN/NOTIFY，不是目录对象，不能混）。
    pub fn list_events(&mut self, schema: Option<&str>) -> Result<Vec<DatabaseObject>, DbError> {
        match self {
            Self::Mysql(conn) => conn.list_events(schema),
            Self::Sqlite(_)
            | Self::Postgres(_)
            | Self::SqlServer(_)
            | Self::ClickHouse(_)
            | Self::Mongo(_)
            | Self::Elasticsearch(_) => Ok(Vec::new()),
        }
    }

    /// 服务器级的**用户与登录**清单（名字 + 登录属性说明）。
    ///
    /// 立足服务器而不是某个库：MySQL 的 `mysql.user`、PG 的 `pg_roles`、
    /// SQL Server 的 `sys.server_principals` 都是服务器范围的。SQLite 没有用户
    /// 体系（文件级权限就是一切），恒空。
    pub fn list_users(&mut self) -> Result<Vec<DatabaseObject>, DbError> {
        match self {
            Self::Mysql(conn) => conn.list_users(),
            Self::Postgres(conn) => conn.list_users(),
            Self::SqlServer(conn) => conn.list_users(),
            Self::Sqlite(_) | Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => {
                Ok(Vec::new())
            }
        }
    }

    /// 实例概览：版本、当前库、登录身份、已运行时长，以及各引擎自己的键值项。
    ///
    /// 未实作引擎返回 `Unsupported`（不是空结果）——界面上这三个入口按
    /// [`DatabaseKind::supports_instance_insight`] 决定画不画，走到这里说明是**绕过界面**
    /// 的调用，如实报错比给一份空壳更有用。
    pub fn instance_overview(&mut self) -> Result<InstanceOverview, DbError> {
        match self {
            Self::Sqlite(conn) => conn.instance_overview(),
            Self::Postgres(conn) => conn.instance_overview(),
            Self::Mysql(conn) => conn.instance_overview(),
            Self::SqlServer(_) | Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => {
                Err(instance_unsupported(self.kind()))
            }
        }
    }

    /// 实例级内省的两张表之一（指标 / 检查器）。
    pub fn instance_table(&mut self, view: InstanceView) -> Result<InstanceTable, DbError> {
        match self {
            Self::Sqlite(conn) => conn.instance_table(view),
            Self::Postgres(conn) => conn.instance_table(view),
            Self::Mysql(conn) => conn.instance_table(view),
            Self::SqlServer(_) | Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => {
                Err(instance_unsupported(self.kind()))
            }
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

    /// 一张表的**列级元数据**（表结构编辑器的数据源；比 [`Column`] 富一档）。
    ///
    /// 与 [`Self::table_columns`] 的分工：那条是「画结果集表头」的最小口径，
    /// 这条是「编辑表结构」的完整口径——多出可空 / 默认值 / 字符集 / 排序规则，
    /// 注释与 [`Column::comment`] 同源。
    ///
    /// 属性槽位是 `Option`：`None` = 引擎拿不到（SQLite 没注释、PG 没有字符集
    /// 概念）。调用方**有啥画啥**：`None` 的属性画成不可编辑，不冒充。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_column_meta(table),
            Self::Postgres(conn) => conn.table_column_meta(table),
            Self::Mysql(conn) => conn.table_column_meta(table),
            Self::ClickHouse(conn) => conn.table_column_meta(table),
            Self::SqlServer(conn) => conn.table_column_meta(table),
            Self::Mongo(conn) => conn.table_column_meta(table),
            Self::Elasticsearch(conn) => conn.table_column_meta(table),
        }
    }

    /// 一张表的分页浏览（`limit` 行，`offset` 起始；行上限即 `limit`）。
    ///
    /// `order_by` 是**稳定序**的列（调用方传主键；空则不加 `ORDER BY`）。
    /// 不加稳定序的分页在 PG / MySQL 上会翻页重复或漏行——见
    /// `docs/database-design.md` §4.5。
    ///
    /// `filters` 是表浏览的过滤条件（空 = 不过滤）；值一律走绑定参数，见
    /// [`TableFilter`]。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::Postgres(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::Mysql(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::ClickHouse(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::SqlServer(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::Mongo(conn) => conn.table_page(table, limit, offset, order_by, filters),
            Self::Elasticsearch(conn) => conn.table_page(table, limit, offset, order_by, filters),
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

    /// 一张表上的外键（右栏「外键」卡）。
    ///
    /// 四条关系引擎（SQLite / PostgreSQL / MySQL / SQL Server）都给得出；
    /// ClickHouse 声明了外键语法但**从不执行**、MongoDB / Elasticsearch 没有这个
    /// 概念——三者恒返回空 `Vec`（「没有」而不是「不支持出错」，UI 直接不画卡）。
    pub fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_foreign_keys(table),
            Self::Postgres(conn) => conn.table_foreign_keys(table),
            Self::Mysql(conn) => conn.table_foreign_keys(table),
            Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => Ok(Vec::new()),
            Self::SqlServer(conn) => conn.table_foreign_keys(table),
        }
    }

    /// 一张表上的触发器（右栏「触发器」卡）。
    ///
    /// ClickHouse（物化视图不在此列）/ MongoDB / Elasticsearch 没有触发器概念，
    /// 恒返回空 `Vec`。
    pub fn table_triggers(&mut self, table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        match self {
            Self::Sqlite(conn) => conn.table_triggers(table),
            Self::Postgres(conn) => conn.table_triggers(table),
            Self::Mysql(conn) => conn.table_triggers(table),
            Self::ClickHouse(_) | Self::Mongo(_) | Self::Elasticsearch(_) => Ok(Vec::new()),
            Self::SqlServer(conn) => conn.table_triggers(table),
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

/// 分页包装用的子查询别名（加引号后不会与用户的结果列名冲突）。
pub(crate) const PAGE_ALIAS: &str = "__duck_page";

/// 取分页包装的**内层语句**：校验「恰好一条语句」并取出其纯文本。
///
/// [`crate::sqlintel::split_statements`] 已按分号切分并逐条 `trim`，这里再兜一层
/// 结尾分号（子查询里带分号是语法错误）。
pub(crate) fn paged_inner_sql(sql: &str) -> Result<String, DbError> {
    let statements = crate::sqlintel::split_statements(sql);
    if statements.len() != 1 {
        return Err(DbError::Unsupported(
            "paged execution needs exactly one statement".into(),
        ));
    }
    let inner = statements
        .into_iter()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_owned();
    if inner.is_empty() {
        return Err(DbError::Unsupported("no statement to page".into()));
    }
    Ok(inner)
}

/// 分页探测行数：多取一行判断「还有下一页」。
pub(crate) fn paged_probe(limit: usize) -> usize {
    limit.saturating_add(1)
}

/// 把内层语句包成外层分页查询（`SELECT * FROM (<inner>) AS <alias> LIMIT n OFFSET m`）。
///
/// SQLite / PostgreSQL / MySQL / ClickHouse 共用这一种形态；SQL Server 的
/// `OFFSET/FETCH` 语法不同（且强制要 `ORDER BY`），自己在驱动里拼。
pub(crate) fn wrap_paged_sql(
    inner: &str,
    offset: u64,
    limit: usize,
    quote: char,
) -> Result<String, DbError> {
    let alias = quote_identifier(PAGE_ALIAS, quote)?;
    let probe = paged_probe(limit);
    Ok(format!(
        "SELECT * FROM ({inner}) AS {alias} LIMIT {probe} OFFSET {offset}"
    ))
}

/// 没有 SQL 分页能力的引擎统一给出的拒绝（英文原文，便于日志检索）。
fn paged_unsupported(engine: &str) -> DbError {
    DbError::Unsupported(format!("{engine} does not support paged SQL execution"))
}

/// 没有实例级内省的引擎统一给出的拒绝（英文原文，便于日志检索）。
///
/// 与 [`paged_unsupported`] 同一口径：能力判据在
/// [`DatabaseKind::supports_instance_insight`]，界面据此不画入口；真被调到说明调用方
/// 没查判据，如实报错而不是返回空表——空表会被界面渲染成「这台服务器什么都没有」。
fn instance_unsupported(kind: DatabaseKind) -> DbError {
    DbError::Unsupported(format!(
        "{kind:?} does not support instance-level introspection"
    ))
}

/// 字节数转人类可读（`1.5 MB`）。单位用十进制（数据库工具惯例），与语言无关。
///
/// 放在内核而不是界面：SQLite 与 PostgreSQL 的指标行里都要它（PG 自己有
/// `pg_size_pretty`，但把两边的口径统一在这里，界面才不会出现「一个用 MB 一个用 MiB」）。
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 秒数转人类可读（`3d 4h 5m`）。与 [`human_bytes`] 同一口径：语言无关的紧凑形式。
pub(crate) fn human_duration_secs(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, secs) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
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
///
/// `pub` 是因为**界面层也要拼 SQL 草稿**（结果集导出 INSERT、对象树「生成 SELECT」）——
/// 那里的名字同样来自内省，必须走同一套引号规则，不能另写一份。
pub fn quote_identifier(name: &str, quote: char) -> Result<String, DbError> {
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
pub fn quote_qualified(table: &TableRef, quote: char) -> Result<String, DbError> {
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

/// 表浏览的一档过滤算符。
///
/// 这一组是**跨引擎的最大公约数**：`=` / `!=` / `>` / `>=` / `<` / `<=` / `LIKE`
/// 在五条 SQL 引擎上拼法一致（只差占位符风格），`IS NULL` / `IS NOT NULL` 不需要值。
/// 引擎侧的差异（值的参数化方式、列要不要包 `CAST` / `toString`）落在各驱动的
/// `table_page` 里，不在这层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `LIKE`（值里的 `%` / `_` 由引擎解释，本层不转义——那会让用户没法写通配）。
    Like,
    /// `IS NULL`（不需要值）。
    IsNull,
    /// `IS NOT NULL`（不需要值）。
    IsNotNull,
}

impl FilterOp {
    /// 全部档位（顺序即界面下拉的展示顺序）。
    pub const ALL: [Self; 9] = [
        Self::Eq,
        Self::NotEq,
        Self::Gt,
        Self::Gte,
        Self::Lt,
        Self::Lte,
        Self::Like,
        Self::IsNull,
        Self::IsNotNull,
    ];

    /// 是否需要值（`IS NULL` / `IS NOT NULL` 不需要）。
    #[must_use]
    pub const fn needs_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }

    /// 值型算子对应的 SQL 算子文本；`IS NULL` / `IS NOT NULL` 返回 `None`
    /// （它们由 [`filter_conditions`] 单独拼，不能写成 `col = ?`）。
    #[must_use]
    pub const fn sql_operator(self) -> Option<&'static str> {
        match self {
            Self::Eq => Some("="),
            Self::NotEq => Some("<>"),
            Self::Gt => Some(">"),
            Self::Gte => Some(">="),
            Self::Lt => Some("<"),
            Self::Lte => Some("<="),
            Self::Like => Some("LIKE"),
            Self::IsNull | Self::IsNotNull => None,
        }
    }
}

/// 表浏览的一档过滤条件：列 + 算符 + 值。
///
/// ⚠️ **值永远不被拼进 SQL 文本**：各驱动把它作为绑定参数送下去（SQLite/MySQL `?`、
/// PostgreSQL `$n`、SQL Server `@Pn`、ClickHouse 的 `{x:String}` URL 参数），
/// 需要 `NULL` 判定时用 [`FilterOp::IsNull`] / [`FilterOp::IsNotNull`]（此时 `value` 为
/// `None`），而不是把字面量 `NULL` 当值传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableFilter {
    /// 列名（未加引号的原名）。
    pub column: String,
    /// 算符。
    pub op: FilterOp,
    /// 值（展示文本）；`IS NULL` / `IS NOT NULL` 时为 `None`。
    pub value: Option<String>,
}

impl TableFilter {
    /// 一档带值的过滤。
    #[must_use]
    pub fn new(column: impl Into<String>, op: FilterOp, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op,
            value: Some(value.into()),
        }
    }

    /// 一档不带值的过滤（只对 `IS NULL` / `IS NOT NULL` 有意义）。
    #[must_use]
    pub fn null(column: impl Into<String>, op: FilterOp) -> Self {
        Self {
            column: column.into(),
            op,
            value: None,
        }
    }
}

/// 绑定占位符的三种写法（值**只**经占位符进 SQL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaceholderStyle {
    /// `?`（SQLite / MySQL，按出现顺序绑定）。
    Question,
    /// `$n`（PostgreSQL，从 1 起）。
    Dollar,
    /// `@Pn`（SQL Server，从 1 起）。
    At,
}

impl PlaceholderStyle {
    /// 第 `index` 个值参数（`index` 从 1 起）的写法。
    pub(crate) fn render(self, index: usize) -> String {
        match self {
            Self::Question => "?".to_owned(),
            Self::Dollar => format!("${index}"),
            Self::At => format!("@P{index}"),
        }
    }
}

/// 把一档档过滤条件拼成 `WHERE` 的各个条件片段与配套的绑定值。
///
/// 两个回调把「引擎差异」收进调用点，公共的那一半留在这里：
/// - **列名一律逐段加引号**（`column_expr` 的返回值必须包含引号结果，通常是
///   [`quote_identifier`] 的产物，个别引擎再包一层 `CAST(... AS TEXT)` / `toString(...)`）；
/// - **值一律走占位符**（`value_expr` 给出第 `n` 个值在 SQL 里的写法，个别引擎再补
///   `::text::<类型>` 这样的转型），返回值按出现顺序与占位符一一对应。
///
/// 返回 `(条件列表, 绑定值列表)`；`IS NULL` / `IS NOT NULL` 不占位、不进值列表，
/// 所以序号只按**带值**的条件递增。
pub(crate) fn filter_conditions(
    filters: &[TableFilter],
    column_expr: impl Fn(&str, FilterOp) -> Result<String, DbError>,
    value_expr: impl Fn(usize, &TableFilter) -> String,
) -> Result<(Vec<String>, Vec<String>), DbError> {
    let mut conditions = Vec::with_capacity(filters.len());
    let mut values = Vec::new();
    for filter in filters {
        if filter.column.trim().is_empty() {
            return Err(DbError::Config("filter column is empty".into()));
        }
        let column = column_expr(&filter.column, filter.op)?;
        match filter.op {
            FilterOp::IsNull => conditions.push(format!("{column} IS NULL")),
            FilterOp::IsNotNull => conditions.push(format!("{column} IS NOT NULL")),
            op => {
                let Some(value) = filter.value.as_deref() else {
                    return Err(DbError::Config(format!(
                        "filter on {} needs a value",
                        filter.column
                    )));
                };
                let index = values.len() + 1;
                let placeholder = value_expr(index, filter);
                let operator = op
                    .sql_operator()
                    .expect("value-bearing ops always have an operator");
                conditions.push(format!("{column} {operator} {placeholder}"));
                values.push(value.to_owned());
            }
        }
    }
    Ok((conditions, values))
}

/// 常见引擎的整段 `WHERE` 子句：列名按 `quote` 加引号、值按 `style` 给占位符。
///
/// 返回 `(" WHERE ...", 绑定值)`；过滤列表为空时返回 `("", 空)`（调用方据此不加子句）。
/// PostgreSQL / SQL Server / ClickHouse 的形态差异（列转型、`LIKE` 包装）走
/// [`filter_conditions`] 自己拼。
pub(crate) fn plain_filter_clause(
    filters: &[TableFilter],
    quote: char,
    style: PlaceholderStyle,
) -> Result<(String, Vec<String>), DbError> {
    if filters.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let (conditions, values) = filter_conditions(
        filters,
        |name, _op| quote_identifier(name, quote),
        |index, _| style.render(index),
    )?;
    Ok((format!(" WHERE {}", conditions.join(" AND ")), values))
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

    /// 指标数值筛选：只有能解析成数的项进序列，文案项（体量 / 命中率里的 `-`）留在表里。
    #[test]
    fn numeric_metrics_keep_only_parsable_values() {
        let table = InstanceTable {
            columns: vec!["metric".into(), "value".into()],
            rows: vec![
                vec!["Threads_connected".into(), "12".into()],
                vec!["Bytes_sent".into(), "1.5e6".into()],
                vec!["database_size".into(), "8493 kB".into()],
                vec!["hit_ratio".into(), "-".into()],
                vec!["  spaced  ".into(), " 7 ".into()],
            ],
        };
        let metrics = table.numeric();
        assert_eq!(
            metrics
                .iter()
                .map(|metric| (metric.key.as_str(), metric.value))
                .collect::<Vec<_>>(),
            vec![
                ("Threads_connected", 12.0),
                ("Bytes_sent", 1_500_000.0),
                ("spaced", 7.0),
            ]
        );
        // 列名对不上（别的形态的表）→ 空序列，而不是瞎猜哪列是值。
        let other = InstanceTable {
            columns: vec!["key".into(), "val".into()],
            rows: vec![vec!["a".into(), "1".into()]],
        };
        assert!(other.numeric().is_empty());
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

    /// 过滤子句：值只经占位符进 SQL，`IS NULL` 不占位、不递增序号，
    /// 列名逐段加引号（含引号字符的列名直接拒绝）。
    #[test]
    fn filter_clause_never_inlines_values() {
        let plain = plain_filter_clause(&[], '"', PlaceholderStyle::Question).expect("空");
        assert_eq!(plain.0, "");
        assert!(plain.1.is_empty());

        let filters = vec![
            TableFilter::new("name", FilterOp::Eq, "it's ' or 1=1 --"),
            TableFilter::null("note", FilterOp::IsNull),
            TableFilter::new("score", FilterOp::Gte, "9"),
        ];
        let (clause, values) =
            plain_filter_clause(&filters, '"', PlaceholderStyle::Question).expect("合法");
        assert_eq!(
            clause,
            " WHERE \"name\" = ? AND \"note\" IS NULL AND \"score\" >= ?"
        );
        // 值原样传给驱动绑定层，一个字符都没进 SQL 文本。
        assert_eq!(values, vec!["it's ' or 1=1 --", "9"]);
        assert!(!clause.contains("1=1"), "值绝不能出现在 SQL 文本里");

        // PG / SQL Server 的编号占位符只按带值条件递增。
        let (clause, _) =
            plain_filter_clause(&filters, '"', PlaceholderStyle::Dollar).expect("合法");
        assert!(clause.contains("\"name\" = $1"));
        assert!(clause.contains("\"score\" >= $2"), "{clause}");
        let (clause, _) = plain_filter_clause(&filters, '"', PlaceholderStyle::At).expect("合法");
        assert!(clause.contains("\"score\" >= @P2"), "{clause}");

        // 值型条件缺值 / 空列名都拒绝，不生成会被引擎当语法错拒的半截 SQL。
        assert!(
            plain_filter_clause(
                &[TableFilter {
                    column: "a".into(),
                    op: FilterOp::Eq,
                    value: None
                }],
                '"',
                PlaceholderStyle::Question
            )
            .is_err()
        );
        assert!(
            plain_filter_clause(
                &[TableFilter::new("  ", FilterOp::Eq, "x")],
                '"',
                PlaceholderStyle::Question
            )
            .is_err()
        );
        // 含引号字符的列名拒绝（与 `quote_identifier` 同口径）。
        assert!(
            plain_filter_clause(
                &[TableFilter::new("we\"ird", FilterOp::Eq, "x")],
                '"',
                PlaceholderStyle::Question
            )
            .is_err()
        );
    }

    /// 算符表：值型算子有 SQL 算子，`IS NULL` / `IS NOT NULL` 没有且不需要值。
    #[test]
    fn filter_ops_are_complete() {
        assert_eq!(FilterOp::ALL.len(), 9);
        for op in FilterOp::ALL {
            assert_eq!(op.needs_value(), op.sql_operator().is_some(), "{op:?}");
        }
        assert_eq!(FilterOp::Eq.sql_operator(), Some("="));
        assert_eq!(FilterOp::NotEq.sql_operator(), Some("<>"));
        assert_eq!(FilterOp::Like.sql_operator(), Some("LIKE"));
        assert_eq!(FilterOp::IsNull.sql_operator(), None);
        assert_eq!(FilterOp::IsNotNull.sql_operator(), None);
    }

    /// 分页包装：只接受单条语句、去掉结尾分号、外层 `LIMIT` 多取一行探测；
    /// 多语句 / 空串直接拒绝（不生成会被引擎解析成半截的 SQL）。
    #[test]
    fn paged_wrapping_is_single_statement_only() {
        let inner = paged_inner_sql("SELECT * FROM t;").expect("单条");
        assert_eq!(inner, "SELECT * FROM t");
        assert_eq!(paged_probe(10), 11);
        assert_eq!(
            wrap_paged_sql(&inner, 20, 10, '"').expect("合法"),
            "SELECT * FROM (SELECT * FROM t) AS \"__duck_page\" LIMIT 11 OFFSET 20"
        );
        // MySQL 反引号别名。
        assert!(
            wrap_paged_sql(&inner, 0, 5, '`')
                .expect("合法")
                .contains("AS `__duck_page` LIMIT 6 OFFSET 0")
        );
        // 多语句 / 空串都拒绝。
        assert!(paged_inner_sql("SELECT 1; SELECT 2").is_err());
        assert!(paged_inner_sql("   ").is_err());
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

    /// 人类可读换算：十进制单位（不是 1024 进位的 MiB），边界取整不丢档位。
    #[test]
    fn human_readable_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        // 超过最大单位后不再换档（TB 封顶，数值继续涨）。
        assert_eq!(human_bytes(2048_u64 * 1024 * 1024 * 1024), "2.0 TB");
        assert_eq!(
            human_bytes(5120_u64 * 1024 * 1024 * 1024 * 1024),
            "5120.0 TB"
        );

        assert_eq!(human_duration_secs(0), "0s");
        assert_eq!(human_duration_secs(59), "59s");
        assert_eq!(human_duration_secs(60), "1m 0s");
        assert_eq!(human_duration_secs(3_600), "1h 0m");
        assert_eq!(human_duration_secs(90_000), "1d 1h 0m");
    }
}
