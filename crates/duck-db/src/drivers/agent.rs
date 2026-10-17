//! 子进程（agent）轨驱动：跑对端的驱动进程，用 JSON-RPC 跟它说话。
//!
//! # 为什么要有这一档
//!
//! 达梦 / 金仓 / Oracle / DB2 / Hive / Teradata / Snowflake …… 这些库**不会**有 Rust
//! 原生驱动（要么闭源、要么协议庞大到不值得自己实现）。dbx 的做法是把每个这样的库
//! 包成一个**独立进程**，进程里是官方 JDBC 或 Go 驱动，宿主只跟进程说话。
//! 这一档就是那个「宿主」。
//!
//! # 三层分工（别把它们的活儿搅在一起）
//!
//! ```text
//! duck-drivers      「装什么、装在哪」：驱动目录 / 注册表 / 下载 / 校验 / 托管 JRE
//! duck-db-agent     「怎么说话」：起进程 / 发请求 / 按 id 对账 / 超时
//! drivers/agent.rs  「怎么当一条连接」（本文件）：把协议映射成 Connection 的那套语义
//! ```
//!
//! # 这一档**只做读**（以及用户自己写的 SQL）
//!
//! 这是本模块最需要交代清楚的取舍。协议里 `execute_query` / `execute_transaction` 的
//! 参数只有 `sql`（与 `schema` / `maxRows`），**没有任何参数绑定**（核过对端的
//! `JsonRpcServer`：`statements` 就是字符串数组）。也就是说，凡是我们自己拼的写语句，
//! 值只能**以字面量进 SQL**。
//!
//! 于是：
//!
//! - **读路径**（元数据浏览 / 表页 / 查询）：值只有一处会进 SQL —— 表浏览的过滤条件。
//!   它走 [`filter_conditions`] 的 `value_expr` 回调 + [`sql_literal`] 一处转义，
//!   有单测钉住「单引号会被翻倍、`\0` 直接拒」。读错了顶多重查一次。
//! - **写路径**（改格 / 删行 / 插行 / 批量）：**一律拒绝**（`DbError::Unsupported`）。
//!   不是因为做不到，而是因为做它就得把用户数据拼进语句、且**没有**服务端参数的兜底；
//!   写错了是数据没了。要写就在 SQL 编辑器里自己写 —— 那条路是用户自己写的 SQL，
//!   与「工具替他拼一条 UPDATE」是两件事。
//!
//! 另外**全库范围**的 `list_triggers` / `list_indexes` 也拒（协议只有「按表」取的形态），
//! 但 [`AgentConnection::table_triggers`] / [`table_indexes`](AgentConnection::table_indexes)
//! 是完整的 —— 右栏那两张卡照常工作。
//!
//! # 一个已知的精度损失
//!
//! 对端把单元格序列化成 **JSON 数字**，而 `serde_json` 默认按 `f64` 解析 ——
//! 超过 f64 精度（约 15 位有效数字）的 `DECIMAL` / `NUMBER(38)` 会被抹平
//! （`12345678901234567890.12` 变成 `1.2345678901234568e19`）。
//! 整数只要落在 `i64` / `u64` 里都是精确的，**受影响的是高精度小数与超长整数**。
//! 根治要么给 `serde_json` 开 `arbitrary_precision`（全仓特性开关，会动到所有用
//! serde_json 的地方），要么给单元格写一个保留原文的 visitor —— 两样都留到真被报告
//! 时再做，这里如实记着，不假装没有。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use duck_db_agent::protocol::{self, ConnectionInfo, QueryOutcome, SessionRole};
use duck_db_agent::{AgentError, AgentHandle, AgentProcess, ConnectRequest};
use duck_drivers::{DriverEntry, DriverStore};

use super::{
    BatchOutcome, CatalogTable, CellEdit, DatabaseObject, InsertRow, OrderBy, PkValues, RowCount,
    TableColumnMeta, TableFilter, TableForeignKey, TableIndex, TableKind, TableRef, TableTrigger,
    filter_conditions, quote_identifier, quote_qualified,
};
use crate::cancel::CancelHandle;
use crate::config::ConnectionConfig;
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, classify, prove_read_only, split_statements};

/// 连库超时：建连远比对端「应答一条语句」慢（三层：进程启动 + JVM + JDBC + 网络）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// 元数据请求超时。
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
/// 执行语句的超时下限：真正的上限由档案的 `default_timeout` 给（见 [`Self::timeout`]）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 关会话的宽限。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// 单次「取整表」的页大小上限：对端会把整页读进内存。
const MAX_PAGE_SIZE: usize = 10_000;

/// 会话 id 的序号：同一个档案被同时打开两次时，两次的会话 id 必须不同
/// （对端对重复的 `agentSessionId` 会直接报 `session already exists`）。
static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// 一条 agent 轨连接。
pub struct AgentConnection {
    /// 驱动进程。**Arc 是为了让取消句柄能在别的线程上用它** ——
    /// 主线程阻塞在等回应上时，取消走的是同一个进程的**另一条请求**，
    /// 而 [`AgentHandle`] 的方法都只取 `&self`，所以这条路是通的。
    handle: Arc<AgentHandle>,
    /// 本连接的逻辑会话 id（对端按它分流连接池）。
    session_id: String,
    /// 驱动目录条目（展示名与键都从这里来）。
    entry: &'static DriverEntry,
    /// 握手结果（能力开关的判据）。
    info: duck_db_agent::HandshakeInfo,
    /// 连上之后问到的引擎事实（引号在这里）。
    connection: ConnectionInfo,
    /// 当前库（`None` = 用对端的默认落点）。
    database: String,
    /// 当前 schema（只影响我们拼给对端的参数）。
    schema: Option<String>,
    /// 连接级只读档：写语句在**下发之前**拦下（对端没有会话级只读开关）。
    read_only: bool,
    /// 单条语句的超时上限。
    timeout: Duration,
}

impl AgentConnection {
    /// 建连：找到已安装的驱动进程、起进程、开一个会话。
    ///
    /// # Errors
    /// - 档案里没有驱动键 / 键不在目录里（[`DbError::Config`]）；
    /// - 驱动**没装**或工件坏了（[`DbError::Connect`]，界面据此提示「去驱动管理里装」）；
    /// - 进程起不来、握手失败、对端拒连（[`DbError::Connect`]）。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        cfg.validate()?;
        let entry = cfg.driver_entry().ok_or_else(|| {
            DbError::Config(format!(
                "connection {} has no known agent driver key",
                cfg.id
            ))
        })?;

        let store = DriverStore::new();
        let spec = store.launch_spec(entry.key).map_err(|error| {
            // 最常见的成因就是「还没装」——把话说到位：这不是网络问题，是去驱动管理点一下。
            DbError::Connect(format!(
                "the {} driver is not ready to run ({error}); install or update it in the driver manager",
                entry.label
            ))
        })?;

        let process = AgentProcess {
            program: spec.program.clone(),
            args: spec.args.clone(),
            working_dir: Some(spec.working_dir.clone()),
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn(&process).map_err(|error| {
            DbError::Connect(format!(
                "cannot start the {} driver process: {error}",
                entry.label
            ))
        })?;

        let info = handle
            .handshake()
            .map_err(|error| DbError::Connect(handshake_failure(entry, error)))?;

        let session_id = format!("{}-{}", cfg.id, SESSION_SEQ.fetch_add(1, Ordering::Relaxed));
        let request = connect_request(cfg, password);
        let params = request
            .into_session_params(&session_id)
            .map_err(|error| DbError::Config(error.to_string()))?;
        handle
            .call(protocol::method::OPEN_SESSION, params, CONNECT_TIMEOUT)
            .map_err(|error| DbError::Connect(describe_agent_error(entry, "connect", error)))?;

        // 引号与版本：**尽力而为**。个别 agent 没实现 `connection_info`，
        // 那不该让「已经连上」的连接失败 —— 少一个引号只是「生成 SELECT」不可用。
        let connection = match handle.call(
            protocol::method::CONNECTION_INFO,
            protocol::session_params(&session_id),
            METADATA_TIMEOUT,
        ) {
            Ok(value) => serde_json::from_value(value).unwrap_or_default(),
            Err(error) => {
                tracing::debug!(
                    driver = entry.key,
                    "the driver did not answer connection_info: {error}"
                );
                ConnectionInfo::default()
            }
        };

        Ok(Self {
            handle: Arc::new(handle),
            session_id,
            entry,
            info,
            connection,
            database: cfg.database.trim().to_owned(),
            schema: cfg.schema.clone(),
            read_only: cfg.read_only,
            timeout: cfg
                .default_timeout()
                .map_or(QUERY_TIMEOUT, Duration::from_secs),
        })
    }

    /// 驱动目录条目（界面拿 `entry.label` 做展示名）。
    #[must_use]
    pub(crate) fn entry(&self) -> &'static DriverEntry {
        self.entry
    }

    /// 引擎自报的标识符引号；没问到就是 `None`。
    #[must_use]
    pub(crate) fn identifier_quote(&self) -> Option<char> {
        self.connection.quote_char()
    }

    /// 对端握手时自报的「支持服务端游标翻页」。
    ///
    /// 这是**唯一**该信的分页能力来源：本档背后是各写各的驱动进程，能力各不相同，
    /// 而 [`crate::config::DatabaseKind::supports_paged_read`] 只能回答「这一**轨**
    /// 有没有这条通路」，回答不了「这个驱动有没有」。
    #[must_use]
    pub(crate) fn supports_paged_read(&self) -> bool {
        self.info.supports(protocol::capability::PAGED_QUERY)
    }

    /// 带外取消句柄：走对端的 `cancel_session`（见 [`AgentConnection::handle`] 的说明）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::agent(Arc::clone(&self.handle), self.session_id.clone())
    }

    /// 活性探测。
    ///
    /// 用 `validate_session` 而不是「跑一条 `SELECT 1`」：前者对**不需要 SQL 的引擎**
    /// （图库 / 时序库）也成立，后者会要求我们猜方言的 SELECT 语法。
    /// 对端不认识这个方法时回退到 `validate_connection`，再不行就当进程活着即健康。
    pub fn ping(&mut self) -> Result<(), DbError> {
        let session = protocol::session_params(&self.session_id);
        if self
            .handle
            .call(
                protocol::method::VALIDATE_SESSION,
                session.clone(),
                METADATA_TIMEOUT,
            )
            .is_ok()
        {
            return Ok(());
        }
        if self.handle.is_closed() {
            return Err(DbError::Connect(format!(
                "the {} driver process is gone",
                self.entry.label
            )));
        }
        match self.handle.call(
            protocol::method::VALIDATE_CONNECTION,
            session,
            METADATA_TIMEOUT,
        ) {
            Ok(_) => Ok(()),
            // 明确报「连接不合法」才算失败；「不认识这个方法」不该把好连接判死。
            Err(AgentError::Remote { .. }) => Err(DbError::Connect(format!(
                "the {} session is no longer valid",
                self.entry.label
            ))),
            Err(error) => Err(DbError::Connect(describe_agent_error(
                self.entry, "validate", error,
            ))),
        }
    }

    /// 服务端版本描述。
    ///
    /// 协议里**没有**「取版本」的方法，对端只在 `connection_info` 的 `databaseInfo`
    /// 里顺带给一些键值（给什么各家不一样）。所以这里返回的是那份键值拼成的单行；
    /// 一个键都没有时退回驱动名 —— 与其编一个版本号，不如说清楚「这是谁」。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        if self.connection.database_info.is_empty() {
            return Ok(format!("{} (agent)", self.entry.label));
        }
        Ok(self
            .connection
            .database_info
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(", "))
    }

    /// 连接级只读档。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 切换当前库。
    ///
    /// **只是换我们发给对端的 `database` 参数**，不重连：对端的每个元数据 / 查询请求
    /// 都自带 `database`（`switchCatalog`），所以「切库」在协议层就是换一个值 ——
    /// 比 PostgreSQL 那边（`dbname` 是建连参数，必须重连）轻松得多。
    pub fn use_database(&mut self, name: &str) -> Result<(), DbError> {
        self.database = name.trim().to_owned();
        Ok(())
    }

    /// 记住当前 schema（只影响我们拼给对端的参数）。
    pub fn set_schema(&mut self, schema: Option<&str>) {
        self.schema = schema
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }

    /// 有哪些库。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        let values: Vec<duck_db_agent::DatabaseInfo> = self.metadata_call(
            protocol::method::LIST_DATABASES,
            serde_json::json!({}),
            "list databases",
        )?;
        let mut names: Vec<String> = values
            .into_iter()
            .map(|info| info.name)
            .filter(|name| !name.trim().is_empty())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// 当前库里的 schema 清单。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let values: Vec<String> = self.metadata_call(
            protocol::method::LIST_SCHEMAS,
            protocol::database_params(&self.database),
            "list schemas",
        )?;
        let mut names: Vec<String> = values
            .into_iter()
            .filter(|name| !name.trim().is_empty())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// 目录（表 / 视图 / 序列 / 存储过程 / 函数）。
    ///
    /// **两路取，不合成一路**：
    ///
    /// 1. `list_tables` 拿表与视图 —— 这是左栏主体，也是**一定有**的那份；
    /// 2. `list_objects` **尽力**补上「不是表也不是视图」的那些（序列 / 存储过程 / 函数）。
    ///
    /// 分开的理由是**失败口径**：[`super::Connection::catalog`] 的失败会让整页内省失败
    /// （见 `duck-app` 的 `Introspection::load`），而 `list_objects` 是**可选**方法
    /// （不少 agent 根本没实现）。只走它会让「本来能看的表」跟着一起没；所以第二路
    /// 失败时**静默跳过**，表照常给。
    ///
    /// ⚠️ 第二路送进来的表 / 视图会被丢掉（[`TableKind::is_relation`]）—— 它们已经在
    /// 第一路里了，重复会让树上同一张表出现两次。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = self.resolve_schema(schema);
        let params = protocol::schema_params(&self.database, &schema);
        let tables: Vec<duck_db_agent::TableInfo> =
            self.metadata_call(protocol::method::LIST_TABLES, params.clone(), "list tables")?;
        // 空串 = 没选中任何 schema（对端用自己的默认落点）；这种时候限定名该退化成裸名字，
        // 而不是「拼一个空标识符出来」。
        let fallback = Some(schema).filter(|value| !value.is_empty());

        let mut found: Vec<CatalogTable> = tables
            .into_iter()
            .map(|table| CatalogTable {
                kind: table_kind(&table.table_type),
                schema: table
                    .parent_schema
                    .filter(|value| !value.is_empty())
                    .or_else(|| fallback.clone()),
                name: table.name,
            })
            .collect();

        if let Ok(objects) = self.metadata_call::<Vec<duck_db_agent::ObjectInfo>>(
            protocol::method::LIST_OBJECTS,
            params,
            "list objects",
        ) {
            found.extend(objects.into_iter().filter_map(|object| {
                let kind = object_kind(&object.object_type)?;
                if kind.is_relation() {
                    return None;
                }
                Some(CatalogTable {
                    schema: object
                        .schema
                        .filter(|value| !value.is_empty())
                        .or_else(|| fallback.clone()),
                    name: object.name,
                    kind,
                })
            }));
        }
        Ok(found)
    }

    /// 执行一段 SQL（可含多条语句）。
    ///
    /// 语句由 [`split_statements`] 在**本地**切开、逐条下发：协议的一次
    /// `execute_query` 只回一个结果，而 `ExecOutcome` 的语义是「最后一条返回行的语句
    /// 的结果集 + 全部写语句累计的影响行数」——切开发最贴合这个语义，
    /// 也免得把「多语句」这种方言差异丢给对端。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let statements = split_statements(sql);
        if statements.is_empty() {
            return Ok(ExecOutcome::default());
        }
        if self.read_only {
            self.refuse_writes(&statements)?;
        }

        let mut outcome = ExecOutcome::default();
        for statement in &statements {
            let params = protocol::query_params(
                statement,
                Some(&self.database),
                self.schema.as_deref(),
                row_limit(max_rows),
            );
            let result: QueryOutcome = self.query_call(params)?;
            outcome.result = result_set(&result);
            outcome.affected += u64::try_from(result.affected_rows.max(0)).unwrap_or(0);
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 分页执行：用**对端的服务端游标**翻页，不套 `LIMIT/OFFSET` 外壳。
    ///
    /// 好处正是外层包装的坏处：外层是 `SELECT * FROM (<原语句>) LIMIT/OFFSET`，
    /// 那会把原语句的 `ORDER BY` 挤进派生表里（SQL Server 直接报错、多数引擎顺序不保证），
    /// 而服务端游标是在**原语句自己的**执行上继续读，顺序与语义都不动。
    ///
    /// ⚠️ 协议没有「跳过 N 行」的原语，所以 `offset > 0` 只能靠**取整页再丢掉**实现
    /// （见 [`Self::paged_page`]）。代价是 O(offset / limit) 次往返 —— 界面「跳到第
    /// 500 页」这种动作会慢，逐页「加载更多」不受影响。
    pub fn execute_paged(
        &mut self,
        sql: &str,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet, DbError> {
        if !self.info.supports(protocol::capability::PAGED_QUERY) {
            return Err(DbError::Unsupported(format!(
                "the {} driver does not support paged queries",
                self.entry.label
            )));
        }
        self.paged_page(sql, offset, limit)
    }

    /// 一次表页：网格「打开表」走这里。
    ///
    /// 语句由我们自己拼（`SELECT * FROM <限定名>` + 过滤 + 排序），
    /// 用的引号是**连上之后**从对端问来的那个。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let Some(quote) = self.quote()? else {
            return Err(DbError::Unsupported(format!(
                "the {} driver did not report an identifier quote, so a table query cannot be built",
                self.entry.label
            )));
        };
        let sql = table_select_sql(table, order_by, filters, quote)?;
        self.paged_page(&sql, offset, limit)
    }

    /// 列（表内省口径：带注释）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let columns = self.columns_of(table)?;
        Ok(columns
            .into_iter()
            .map(|column| Column {
                name: column.name,
                type_name: column.data_type,
                comment: column.comment.unwrap_or_default(),
            })
            .collect())
    }

    /// 列的详细元数据（右栏「结构」卡）。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        let columns = self.columns_of(table)?;
        Ok(columns
            .into_iter()
            .map(|column| TableColumnMeta {
                name: column.name,
                type_name: column.data_type,
                comment: column.comment.unwrap_or_default(),
                nullable: Some(column.is_nullable),
                default: column.column_default.filter(|value| !value.is_empty()),
                character_set: column.character_set.filter(|value| !value.is_empty()),
                collation: column.collation.filter(|value| !value.is_empty()),
            })
            .collect())
    }

    /// 主键列名（按对端给的列序里主键出现的顺序）。
    ///
    /// ⚠️ `get_columns` 给的主键信息**没有列序**（`is_primary_key` 只是个布尔），
    /// 所以这里给的是「列定义顺序」，不保证与真实主键的键序一致。
    /// 用途（判「这张表能不能编辑」）不受影响；要精确键序得走 `list_indexes`
    /// 里 `is_primary` 的那一条 —— 有意留到真需要时再做。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        Ok(self
            .columns_of(table)?
            .into_iter()
            .filter(|column| column.is_primary_key)
            .map(|column| column.name)
            .collect())
    }

    /// 索引（右栏「索引」卡）。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let indexes: Vec<duck_db_agent::IndexInfo> = self.metadata_call(
            protocol::method::LIST_INDEXES,
            self.table_params(table),
            "list indexes",
        )?;
        Ok(indexes
            .into_iter()
            .map(|index| TableIndex {
                name: index.name,
                columns: index.columns,
                unique: index.is_unique,
                primary: index.is_primary,
            })
            .collect())
    }

    /// 外键（右栏「外键」卡）。
    ///
    /// 对端给的是**一列一行**（`{name, column, ref_table, ref_column}`），这里按约束名
    /// 归并成复合外键 —— 与 [`TableForeignKey`] 的「`columns` / `ref_columns` 按序对齐」
    /// 口径对齐（见 [`group_foreign_keys`]）。
    pub fn table_foreign_keys(
        &mut self,
        table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        let keys: Vec<duck_db_agent::ForeignKeyInfo> = self.metadata_call(
            protocol::method::LIST_FOREIGN_KEYS,
            self.table_params(table),
            "list foreign keys",
        )?;
        Ok(group_foreign_keys(keys))
    }

    /// 触发器（右栏「触发器」卡）。
    pub fn table_triggers(&mut self, table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        let triggers: Vec<duck_db_agent::TriggerInfo> = self.metadata_call(
            protocol::method::LIST_TRIGGERS,
            self.table_params(table),
            "list triggers",
        )?;
        Ok(triggers
            .into_iter()
            .map(|trigger| TableTrigger {
                name: trigger.name,
                timing: trigger.timing,
                event: trigger.event,
            })
            .collect())
    }

    /// 建表 DDL。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let value = self.metadata_value(
            protocol::method::GET_TABLE_DDL,
            self.table_params(table),
            "get table ddl",
        )?;
        // 各 agent 回的形状不一：直接给字符串，或包一层 `{"ddl": "..."}`。
        // 两种都收下 —— 与其挑一种，不如把常见的两种都认了。
        if let Some(text) = value.as_str() {
            return Ok(text.to_owned());
        }
        if let Some(text) = value.get("ddl").and_then(serde_json::Value::as_str) {
            return Ok(text.to_owned());
        }
        Err(DbError::Query(format!(
            "the {} driver returned an unexpected DDL payload",
            self.entry.label
        )))
    }

    /// 行数：用 `COUNT(*)` 的**精确**口径。
    ///
    /// 不走系统统计（那要按引擎写方言查询）—— 精确值慢一点，但「约 1234 行」在一个
    /// 只读轨上更容易误导人。真嫌慢时用户自己写 `COUNT(*)` 也是一条路。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let Some(quote) = self.quote()? else {
            return Err(DbError::Unsupported(
                "no identifier quote for this driver".into(),
            ));
        };
        let sql = format!("SELECT COUNT(*) FROM {}", quote_qualified(table, quote)?);
        let result = self.query_call(protocol::query_params(
            &sql,
            Some(&self.database),
            self.schema.as_deref(),
            1,
        ))?;
        let count = result
            .rows
            .first()
            .and_then(|row| row.first())
            .map_or(0, cell_as_u64);
        Ok(RowCount::Exact(count))
    }

    /// 关连接：请对端丢掉这个会话。
    ///
    /// 失败**不上报**（`close` 的签名也没有返回值）：收尾时对端可能已经半死，
    /// 而这里唯一该做的事是别留下一个跑着的 JVM —— 那由 `AgentHandle` 的 `Drop`
    /// 兜底（它会杀进程）。把「关会话失败」抛到界面上，用户也做不了什么。
    pub fn close(self) {
        if let Err(error) = self.handle.call(
            protocol::method::CLOSE_SESSION,
            protocol::session_params(&self.session_id),
            CLOSE_TIMEOUT,
        ) {
            tracing::debug!(
                driver = self.entry.key,
                "the driver did not acknowledge close_session: {error}"
            );
        }
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /// 一次元数据请求 → 反序列化。统一把错误翻译成带驱动名的 [`DbError`]。
    fn metadata_call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
        action: &str,
    ) -> Result<T, DbError> {
        let value = self.metadata_value(method, params, action)?;
        serde_json::from_value(value).map_err(|error| {
            DbError::Query(format!(
                "the {} driver returned an unexpected {action} payload: {error}",
                self.entry.label
            ))
        })
    }

    /// 一次元数据请求 → 原始 JSON（形状不定的那些方法用）。
    fn metadata_value(
        &self,
        method: &str,
        params: serde_json::Value,
        action: &str,
    ) -> Result<serde_json::Value, DbError> {
        self.handle
            .call(method, self.with_session(params), METADATA_TIMEOUT)
            .map_err(|error| DbError::Query(describe_agent_error(self.entry, action, error)))
    }

    /// 一次查询请求。
    fn query_call(&self, params: serde_json::Value) -> Result<QueryOutcome, DbError> {
        let value = self
            .handle
            .call(
                protocol::method::EXECUTE_QUERY,
                self.with_session(params),
                self.timeout,
            )
            .map_err(|error| DbError::from_agent(self.entry, error))?;
        serde_json::from_value(value).map_err(|error| {
            DbError::Query(format!(
                "the {} driver returned an unexpected query payload: {error}",
                self.entry.label
            ))
        })
    }

    /// 取 `offset` 起、共 `limit` 行的页。
    ///
    /// 分三种情形，都不套方言 SQL：
    ///
    /// - `limit == 0`：返回空（界面不该这么问，但别让它变成「取全部」）；
    /// - `offset == 0`：一次 `execute_query_page` 就够，游标随后就关；
    /// - `offset > 0`：按 `limit` 逐页取、丢掉前面的整页。⚠️ 页边界对不齐时
    ///   （`offset % limit != 0`）需要**多取一页**再切片。
    fn paged_page(&self, sql: &str, offset: u64, limit: usize) -> Result<ResultSet, DbError> {
        if limit == 0 {
            return Ok(ResultSet::default());
        }
        let page_size = limit.min(MAX_PAGE_SIZE);
        let wanted = offset.saturating_add(limit as u64);

        let mut collected = self.open_page(sql, page_size)?;
        // 只在「对端说还有」且「还没攒够」时继续。⚠️ 判据是对端的 `truncated`
        // （`Page::truncated`，含 `has_more`），**不是**「有没有游标」—— 有的 agent
        // 读完最后一页也会回一个（已作废的）`sessionId`，照它循环会多跑一次必失败
        // 的请求。
        while collected.truncated && (collected.rows.len() as u64) < wanted {
            let Some(session) = collected.cursor.clone() else {
                break;
            };
            let next = self.fetch_page(&session, page_size)?;
            collected.rows.extend(next.rows);
            collected.truncated = next.truncated;
            collected.cursor = next.cursor;
        }

        // 游标用完就关：不关的话对端的服务端游标会一直握着连接资源。
        // 对端已明确「到头」时它自己会作废游标，但再关一次是幂等的，少一条分支。
        if let Some(session) = collected.cursor.take() {
            self.close_cursor(&session);
        }

        let Page {
            columns,
            rows,
            truncated: peer_has_more,
            ..
        } = collected;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(rows.len());
        // 「后面还有」的判据有两半：切完这一页**手里还多出**行（说明被切掉了），
        // 或者我们是因为攒够 `wanted` 才停、而对端当时还说有更多。
        let truncated = rows.len() > start.saturating_add(limit)
            || (peer_has_more && rows.len() as u64 >= wanted);
        let mut rows = rows;
        rows.drain(..start);
        rows.truncate(limit);
        Ok(ResultSet {
            columns,
            rows,
            truncated,
        })
    }

    /// 开一个服务端游标并取第一页。
    fn open_page(&self, sql: &str, page_size: usize) -> Result<Page, DbError> {
        let params = protocol::query_page_params(
            sql,
            Some(&self.database),
            self.schema.as_deref(),
            u32::try_from(page_size).unwrap_or(u32::MAX),
            0,
        );
        let value = self
            .handle
            .call(
                protocol::method::EXECUTE_QUERY_PAGE,
                self.with_session(params),
                self.timeout,
            )
            .map_err(|error| DbError::from_agent(self.entry, error))?;
        let outcome: QueryOutcome = serde_json::from_value(value).map_err(|error| {
            DbError::Query(format!(
                "the {} driver returned an unexpected page payload: {error}",
                self.entry.label
            ))
        })?;
        Ok(Page::from_outcome(outcome))
    }

    /// 按游标取下一页。
    fn fetch_page(&self, cursor: &str, page_size: usize) -> Result<Page, DbError> {
        let params =
            protocol::fetch_page_params(cursor, u32::try_from(page_size).unwrap_or(u32::MAX));
        let value = self
            .handle
            .call(
                protocol::method::FETCH_QUERY_PAGE,
                self.with_session(params),
                self.timeout,
            )
            .map_err(|error| DbError::from_agent(self.entry, error))?;
        let outcome: QueryOutcome = serde_json::from_value(value).map_err(|error| {
            DbError::Query(format!(
                "the {} driver returned an unexpected page payload: {error}",
                self.entry.label
            ))
        })?;
        Ok(Page::from_outcome(outcome))
    }

    /// 关游标（**尽力**：失败只记日志，调用方已经拿到数据了）。
    fn close_cursor(&self, cursor: &str) {
        if let Err(error) = self.handle.call(
            protocol::method::CLOSE_QUERY_SESSION,
            self.with_session(protocol::close_query_session_params(cursor)),
            CLOSE_TIMEOUT,
        ) {
            tracing::debug!(
                driver = self.entry.key,
                "the driver did not acknowledge close_query_session: {error}"
            );
        }
    }

    /// 取一张表的列（内省口径）。
    fn columns_of(&self, table: &TableRef) -> Result<Vec<duck_db_agent::ColumnInfo>, DbError> {
        self.metadata_call(
            protocol::method::GET_COLUMNS,
            self.table_params(table),
            "get columns",
        )
    }

    /// 一张表的请求参数（库 + schema + 表名）。
    fn table_params(&self, table: &TableRef) -> serde_json::Value {
        protocol::schema_table_params(
            &self.database,
            &self.resolve_schema(table.schema.as_deref()),
            &table.name,
        )
    }

    /// 解析实际要用的 schema：表自带 > 连接当前选中的 > 空串（对端的默认 schema）。
    ///
    /// 空串是**有意义的**（对端按自己的默认落点走），所以这里不返回 `None` ——
    /// 每一步的 `filter` 都是在把「空」清成 `None`，最后由 `unwrap_or("")` 收成空串。
    fn resolve_schema(&self, schema: Option<&str>) -> String {
        schema
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(self.schema.as_deref())
            .unwrap_or("")
            .to_owned()
    }

    /// 给参数补上会话 id —— **每个**会话级请求都要带，漏了就落成对端的 `__legacy__` 会话。
    fn with_session(&self, mut params: serde_json::Value) -> serde_json::Value {
        if let Some(map) = params.as_object_mut() {
            map.insert(
                protocol::SESSION_FIELD.to_owned(),
                serde_json::Value::String(self.session_id.clone()),
            );
        }
        params
    }

    /// 拿标识符引号；拿不到就报「不支持」（而不是猜一个）。
    fn quote(&self) -> Result<Option<char>, DbError> {
        Ok(self.connection.quote_char())
    }

    /// 只读档的闸门：整段里**任何**一条证明不了只读就整体拒绝。
    ///
    /// 与 SQL Server / MongoDB / Elasticsearch 同一口径（那三条也没有服务端只读开关）：
    /// 未知一律当写、默认拒绝。⚠️ 这不是服务器兜底 —— 真要防住生产库仍然要用只读账号。
    fn refuse_writes(&self, statements: &[String]) -> Result<(), DbError> {
        if let Some(rejected) = statements
            .iter()
            .find(|stmt| !(prove_read_only(stmt) || classify(stmt) == StmtKind::Read))
        {
            return Err(DbError::Unsupported(format!(
                "the connection is read-only and this statement is not provably a read: {rejected}"
            )));
        }
        Ok(())
    }
}

/// 一页（内部用）：列 + 行 + 游标状态。
struct Page {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    cursor: Option<String>,
    truncated: bool,
}

impl Page {
    fn from_outcome(outcome: QueryOutcome) -> Self {
        Self {
            columns: columns_of(&outcome),
            rows: outcome
                .rows
                .iter()
                .map(|row| row.iter().map(cell_value).collect())
                .collect(),
            cursor: outcome.session_id.clone(),
            truncated: outcome.has_more || outcome.truncated,
        }
    }
}

/// 把 [`QueryOutcome`] 转成结果集（不关心游标的那些路径用）。
fn result_set(outcome: &QueryOutcome) -> Option<ResultSet> {
    outcome.is_result_set().then(|| ResultSet {
        columns: columns_of(outcome),
        rows: outcome
            .rows
            .iter()
            .map(|row| row.iter().map(cell_value).collect())
            .collect(),
        truncated: outcome.truncated || outcome.has_more,
    })
}

/// 列名 + 类型名。
///
/// `column_types` 允许比 `columns` 短（有 agent 会少给），缺的那几列类型留空串 ——
/// 界面对空类型名的处理是「不显示」，比错位强。
fn columns_of(outcome: &QueryOutcome) -> Vec<Column> {
    outcome
        .columns
        .iter()
        .enumerate()
        .map(|(index, name)| Column {
            name: name.clone(),
            type_name: outcome.column_types.get(index).cloned().unwrap_or_default(),
            comment: String::new(),
        })
        .collect()
}

/// 一个 JSON 单元格 → 统一结果模型。
///
/// 数字的口径：能落进 `i64` / `u64` 的按整数（**精确**），其余按浮点。
/// ⚠️ 高精度小数在这一步之前就已经被 JSON 的 f64 解析抹平了 —— 见模块文档末尾。
fn cell_value(cell: &serde_json::Value) -> Value {
    match cell {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Value::Int(value)
            } else if let Some(value) = number.as_u64() {
                Value::UInt(value)
            } else {
                Value::Float(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(text) => Value::Text(text.clone()),
        // 数组 / 对象：对端把结构化值（JSON 列、数组列）原样给过来，按文本展示 ——
        // 与 `Value::Json` 的语义一致，且网格里本来就只显示文本。
        other => Value::Json(other.to_string()),
    }
}

/// 单元格 → `u64`（`COUNT(*)` 的取数）。取不出就是 0。
fn cell_as_u64(cell: &serde_json::Value) -> u64 {
    if let Some(value) = cell.as_u64() {
        return value;
    }
    if let Some(value) = cell.as_i64() {
        return u64::try_from(value).unwrap_or(0);
    }
    cell.as_str()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `table_type` → [`TableKind`]。
///
/// 对端给的是各库自己的拼法（`BASE TABLE` / `VIEW` / `SYSTEM VIEW` / `MATERIALIZED VIEW`
/// / `FOREIGN TABLE` / `TABLE`），所以判据是「**含 `VIEW` 就是视图**，其余都当表」：
/// 视图家族的名字都带 `VIEW`，而表家族的名字里没有一个带它。
fn table_kind(table_type: &str) -> TableKind {
    if table_type.to_ascii_uppercase().contains("VIEW") {
        TableKind::View
    } else {
        TableKind::Table
    }
}

/// `list_objects` 的 `object_type` → [`TableKind`]（只认得出的那几种）。
///
/// ⚠️ 这里与 [`table_kind`] 的口径**故意不同**：那个兜底成「表」，这个**兜底成 `None`
/// （丢掉）**。因为两者的失败代价不一样：`list_tables` 只会给表 / 视图，兜底成表是安全的；
/// 而 `list_objects` 会混进 `SYNONYM` / `PACKAGE BODY` / `JOB` / `DIRECTORY` 这些各家
/// 自己的一套，**猜错的代价是树上多一行挂错分类、点开必错的对象**——那比少一行糟得多。
///
/// 归一化只做「大写 + 下划线换空格 + 压空格」：各家的拼法差异基本就这三种
/// （`MATERIALIZED VIEW` / `MATERIALIZED_VIEW` / `materialized view`）。
fn object_kind(object_type: &str) -> Option<TableKind> {
    let normalized = object_type.trim().to_ascii_uppercase().replace('_', " ");
    let token = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    match token.as_str() {
        "TABLE" | "BASE TABLE" => Some(TableKind::Table),
        "VIEW" | "MATERIALIZED VIEW" | "SYSTEM VIEW" => Some(TableKind::View),
        "SEQUENCE" => Some(TableKind::Sequence),
        "PROCEDURE" | "PROC" | "STORED PROCEDURE" => Some(TableKind::Procedure),
        "FUNCTION" | "FUNC" | "SCALAR FUNCTION" | "TABLE FUNCTION" | "AGGREGATE FUNCTION" => {
            Some(TableKind::Function)
        }
        _ => None,
    }
}

/// 一列一行的外键 → 按约束名归并的复合外键。
///
/// 对端是「一列一行」（`{name, column, ref_table, ref_column}`），而界面的「外键」卡
/// 要的是「一条约束一行、多列按序对齐」。归并**只按约束名**且**保持首次出现顺序**：
/// 同名不同表的行不可能出现在同一张表的返回里（名字是这张表内的约束名）。
///
/// ⚠️ 约束名为空时（个别 agent 不给）退化成「每条都单独一条」，而不是把整表的外键
/// 合成一条 —— 那会把几张表的引用列混在一起。
fn group_foreign_keys(keys: Vec<duck_db_agent::ForeignKeyInfo>) -> Vec<TableForeignKey> {
    let mut grouped: Vec<TableForeignKey> = Vec::new();
    for key in keys {
        let name = key.name.trim();
        if !name.is_empty()
            && let Some(existing) = grouped
                .iter_mut()
                .find(|item| item.name == name && item.ref_table == key.ref_table)
        {
            existing.columns.push(key.column);
            existing.ref_columns.push(key.ref_column);
            continue;
        }
        grouped.push(TableForeignKey {
            name: name.to_owned(),
            columns: vec![key.column],
            ref_table: key.ref_table,
            ref_columns: vec![key.ref_column],
            // 协议里没有更新 / 删除动作，留空串 = 「不知道」，与 SQLite 那条的口径一致。
            on_update: String::new(),
            on_delete: String::new(),
        });
    }
    grouped
}

/// 拼一张表的浏览语句（`SELECT * FROM <限定名>` + 过滤 + 排序）。
///
/// 过滤值走 [`sql_literal`] 字面量 —— 协议没有参数绑定，这是**唯一**一处把用户输入
/// 拼进语句的地方，所以它独占一个函数、独占一个转义函数、独占单测。
pub(crate) fn table_select_sql(
    table: &TableRef,
    order_by: &[OrderBy],
    filters: &[TableFilter],
    quote: char,
) -> Result<String, DbError> {
    let qualified = quote_qualified(table, quote)?;
    let where_clause = agent_filter_clause(filters, quote)?;
    let order = super::order_by_clause(order_by, quote)?;
    Ok(format!("SELECT * FROM {qualified}{where_clause}{order}"))
}

/// 过滤条件 → ` WHERE ...`（值以**字面量**入 SQL）。
fn agent_filter_clause(filters: &[TableFilter], quote: char) -> Result<String, DbError> {
    if filters.is_empty() {
        return Ok(String::new());
    }
    let (conditions, _values) = filter_conditions(
        filters,
        |name, _op| quote_identifier(name, quote),
        |_index, filter| sql_literal(filter.value.as_deref().unwrap_or("")),
    )?;
    Ok(format!(" WHERE {}", conditions.join(" AND ")))
}

/// 文本 → SQL 字符串字面量。
///
/// 只需做两件事，且**只做这两件**：
///
/// 1. 单引号翻倍（`'` → `''`，SQL 标准的两代一）；
/// 2. 含 `\0` 直接拒 —— NUL 在若干引擎里会截断字面量（后面的内容被当成新语句），
///    而 `\0` 又不可能出现在正常的过滤框里，拒掉没有代价。
///
/// ⚠️ **不做反斜杠转义**：那是 MySQL 的方言（`NO_BACKSLASH_ESCAPES` 一关就反了），
/// 而标准 SQL 里反斜杠是普通字符。做它会让「达梦上搜 `C:\tmp`」变成搜不到。
pub(crate) fn sql_literal(value: &str) -> String {
    // 不做结果检查而直接替换：调用方已按 `filter.value` 给出具体值，
    // 唯一能出错的地方是 NUL，而在上面那道检查里已经拦掉了。
    format!("'{}'", value.replace('\'', "''"))
}

/// 行数上限 → 协议要的 `maxRows`。
///
/// `0` 在对端是「不设上限」的意思，所以界面传 0（不该发生）时**必须**换成一个真实上限，
/// 否则就变成「把整张表拉进内存」。这里退到 [`MAX_PAGE_SIZE`]。
fn row_limit(max_rows: usize) -> u32 {
    u32::try_from(if max_rows == 0 {
        MAX_PAGE_SIZE
    } else {
        max_rows
    })
    .unwrap_or(u32::MAX)
}

/// 由档案拼出 `connect` / `open_session` 的连接参数。
fn connect_request(cfg: &ConnectionConfig, password: Option<&str>) -> ConnectRequest {
    // 服务名各归各家（`skip_serializing_if`，不用的引擎线上根本不出现）：
    // 按驱动键只放行自己那家的值 —— GBase 8s 的 GBASESQLI、Informix 的
    // `INFORMIXSERVER=`，对端都有缺省回落，空白就是走回落。
    let server_name = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    let (gbase_server, informix_server) = match cfg.driver.as_deref() {
        Some("gbase8s") => (server_name(&cfg.gbase_server), None),
        Some("informix") => (None, server_name(&cfg.informix_server)),
        _ => (None, None),
    };
    ConnectRequest {
        host: cfg.host.trim().to_owned(),
        port: cfg.port.unwrap_or(0),
        database: cfg.database.trim().to_owned(),
        username: cfg.user.trim().to_owned(),
        password: password.unwrap_or("").to_owned(),
        // 端口「明确指定过」才为真：对端的 SQL Server 实例名解析要靠这个布尔
        // （不填端口时它才去查实例名），所以不能一律写 true。
        port_explicit: cfg.port.is_some(),
        // 品牌键与 URL 附加参数都**原样过路**，对端按 `driver_profile` 挑方言细节、
        // 按 `url_params` 补 JDBC URL 尾串（星环 Argo 少了 `auth=noSasl` 就连不上）。
        // 内核不认识这两个值，也不该认识 —— 品牌表在界面侧（见 `ConnectionConfig::profile`）。
        driver_profile: cfg.profile.clone().unwrap_or_default(),
        url_params: cfg.url_params.clone().unwrap_or_default(),
        gbase_server,
        informix_server,
        ssl: cfg.tls != crate::config::TlsMode::Disable,
        // 元数据会话：让对端把「刷对象树」这类请求分到轻量连接上，
        // 不至于把用户正跑着的长查询从池子里挤掉。
        session_role: SessionRole::Metadata,
        ..ConnectRequest::default()
    }
}

/// 握手失败时给用户看的话。
///
/// 把「对方回的是什么」尽量带出来：握手失败最常见的成因是**装的是另一家的工件**
/// （比如把一个 JDBC agent 当 native agent 启动），而对端此时往往已经往 stderr
/// 吐了一句人话（继承了 stderr，日志里能看到）。
fn handshake_failure(entry: &DriverEntry, error: AgentError) -> String {
    format!(
        "the {} driver did not complete the handshake: {error}",
        entry.label
    )
}

/// 把 [`AgentError`] 翻成带动作与驱动名的说明（`Display` 是英文，界面自己组织文案）。
fn describe_agent_error(entry: &DriverEntry, action: &str, error: AgentError) -> String {
    match error {
        AgentError::Remote { message, .. } => format!("{}: {message}", entry.label),
        AgentError::Timeout { waited, .. } => format!(
            "the {} driver did not answer {action} within {waited:?}",
            entry.label
        ),
        AgentError::Closed => format!("the {} driver process is gone", entry.label),
        other => format!("the {} driver failed during {action}: {other}", entry.label),
    }
}

/// 驱动报错 → [`DbError`] 的分类。
///
/// 分连接类与语句类，是因为界面把它们当两件事处理：连接坏了该重连 / 该去驱动管理，
/// 语句错了该看 SQL。做成 `DbError` 的扩展方法而不是自由函数，是为了让调用点上
/// 一眼看出「这是驱动错误」。
trait AgentErrorExt {
    fn from_agent(entry: &DriverEntry, error: AgentError) -> Self;
}

impl AgentErrorExt for DbError {
    fn from_agent(entry: &DriverEntry, error: AgentError) -> Self {
        match &error {
            AgentError::Remote { data, .. } => {
                // 对端的结构化错误里带 `category`（见 dbx 的 AgentRpcError）：
                // `connection` / `timeout` 是连接类，`sql` / `protocol` 是语句类。
                // ⚠️ 这里**先把它拷成 owned**：`category` 借用的是 `error`，而下面
                // `describe_agent_error` 要把 `error` 整个吃掉（它按错误形状拼文案）。
                let category = data
                    .as_ref()
                    .and_then(|data| data.get("category"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let text = describe_agent_error(entry, "the statement", error);
                match category.as_deref() {
                    Some("connection") => Self::Connect(text),
                    Some("canceled") => Self::Cancelled(text),
                    _ => Self::Query(text),
                }
            }
            AgentError::Closed | AgentError::Transport(_) => {
                Self::Connect(describe_agent_error(entry, "the request", error))
            }
            AgentError::Timeout { .. } | AgentError::Protocol(_) => {
                Self::Query(describe_agent_error(entry, "the request", error))
            }
        }
    }
}

/// agent 轨**一律不做**的那些动作统一走这里（原因见模块文档）。
pub(crate) fn unsupported(action: &str, entry: Option<&'static DriverEntry>) -> DbError {
    let engine = entry.map_or("this driver", |entry| entry.label);
    DbError::Unsupported(format!(
        "{engine} does not support {action} through the driver protocol; run the statement in the SQL editor instead"
    ))
}

/// 一档不做的写操作要用的占位（给 [`unsupported`] 当动作名，见 `drivers/mod.rs`）。
pub(crate) const WRITE_UNSUPPORTED: &str = "write operations";
/// 一档不做的批量提交（同上）。
pub(crate) const BATCH_UNSUPPORTED: &str = "batch edits";

// 「不做的那些」在类型上也留个记认，避免将来有人以为只是漏写了。
//
// ⚠️ 别把这两个常量接到**新**的入口上：它们是给 `drivers/mod.rs` 里那几条
// 工具替用户拼语句的路用的，而 agent 档的定位是「只做读 + 用户自己写的语句」。
// 从只读档能覆盖的范围看，这里少一个 `Option<Vec<...>>` 类型的参数是**有意的**：
// 上面那几个是「工具会替用户写数据」的四条通路（改格 / 插行 / 删行 / 批量），
// 而结果侧只有 `BatchOutcome` 一种形状。
#[allow(dead_code)]
fn _unsupported_shapes(
    _: Option<CellEdit>,
    _: Option<InsertRow>,
    _: Option<PkValues>,
    _: Option<BatchOutcome>,
    _: Option<DatabaseObject>,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{FilterOp, TableFilter};

    /// 单引号翻倍、普通字符原样 —— 这是这一轨唯一一处把用户输入拼进 SQL 的地方。
    #[test]
    fn literals_escape_single_quotes() {
        assert_eq!(sql_literal("abc"), "'abc'");
        assert_eq!(sql_literal("it's"), "'it''s'");
        assert_eq!(sql_literal("'; DROP TABLE t; --"), "'''; DROP TABLE t; --'");
        // 反斜杠是普通字符（标准 SQL），不做 MySQL 那套转义。
        assert_eq!(sql_literal(r"C:\tmp"), r"'C:\tmp'");
        assert_eq!(sql_literal(""), "''");
    }

    /// 过滤子句：列名加引号、值是字面量、`IS NULL` 不占位。
    #[test]
    fn filter_clauses_inline_escaped_literals() {
        let filters = vec![
            TableFilter::new("name", FilterOp::Eq, "it's"),
            TableFilter::null("note", FilterOp::IsNull),
        ];
        let clause = agent_filter_clause(&filters, '"').expect("过滤子句");
        assert_eq!(
            clause, r#" WHERE "name" = 'it''s' AND "note" IS NULL"#,
            "{clause}"
        );
        assert_eq!(agent_filter_clause(&[], '"').expect("空"), "");
    }

    /// 浏览语句：限定名逐段加引号、排序附在最后。
    ///
    /// ⚠️ 升序**不写 `ASC`**：`order_by_clause` 是各驱动共用的（SQLite / PG / MySQL 的
    /// 表浏览都走它），它只对降序加 `DESC`。这里断言的是**它的**输出，不是我们想要的输出。
    #[test]
    fn table_select_sql_quotes_and_orders() {
        let table = TableRef::in_schema("public", "orders");
        let sql = table_select_sql(&table, &[OrderBy::asc("id")], &[], '"').expect("语句");
        assert_eq!(sql, r#"SELECT * FROM "public"."orders" ORDER BY "id""#);

        let descending = table_select_sql(&table, &[OrderBy::desc("id")], &[], '"').expect("语句");
        assert_eq!(
            descending,
            r#"SELECT * FROM "public"."orders" ORDER BY "id" DESC"#
        );

        let filtered = table_select_sql(
            &TableRef::bare("t"),
            &[],
            &[TableFilter::new("a", FilterOp::Eq, "1")],
            '`',
        )
        .expect("语句");
        assert_eq!(filtered, "SELECT * FROM `t` WHERE `a` = '1'");
    }

    /// `object_type` 只认得出的那几种，其余**丢掉**（见 `object_kind` 的说明）。
    #[test]
    fn object_types_map_or_are_dropped() {
        assert_eq!(object_kind("SEQUENCE"), Some(TableKind::Sequence));
        assert_eq!(object_kind("sequence"), Some(TableKind::Sequence));
        assert_eq!(object_kind("PROCEDURE"), Some(TableKind::Procedure));
        assert_eq!(object_kind("stored_procedure"), Some(TableKind::Procedure));
        assert_eq!(object_kind("FUNCTION"), Some(TableKind::Function));
        assert_eq!(object_kind("MATERIALIZED VIEW"), Some(TableKind::View));
        assert_eq!(object_kind("TABLE"), Some(TableKind::Table));
        // 各家自己那套：认不出就不猜（猜错会挂错分类、点开必错）。
        assert_eq!(object_kind("SYNONYM"), None);
        assert_eq!(object_kind("PACKAGE BODY"), None);
        assert_eq!(object_kind("JOB"), None);
        assert_eq!(object_kind(""), None);
        // ⚠️ 表 / 视图会由 `catalog` 丢掉（它们已经来自 `list_tables`）。
        assert!(object_kind("TABLE").expect("表").is_relation());
        assert!(!object_kind("SEQUENCE").expect("序列").is_relation());
    }

    /// 视图家族的 `table_type` 都归一成视图，其余当表。
    #[test]
    fn table_types_map_to_the_two_kinds() {
        assert_eq!(table_kind("BASE TABLE"), TableKind::Table);
        assert_eq!(table_kind("TABLE"), TableKind::Table);
        assert_eq!(table_kind("FOREIGN TABLE"), TableKind::Table);
        assert_eq!(table_kind("VIEW"), TableKind::View);
        assert_eq!(table_kind("system view"), TableKind::View);
        assert_eq!(table_kind("MATERIALIZED VIEW"), TableKind::View);
        // 空串（有的 agent 不给）按表处理：比「丢失这个节点」强。
        assert_eq!(table_kind(""), TableKind::Table);
    }

    /// 一列一行的外键归并成复合外键，保持首次出现顺序。
    #[test]
    fn foreign_keys_group_by_constraint() {
        let key = |name: &str, column: &str, ref_column: &str| duck_db_agent::ForeignKeyInfo {
            name: name.to_owned(),
            column: column.to_owned(),
            ref_table: "parent".to_owned(),
            ref_column: ref_column.to_owned(),
        };
        let grouped = group_foreign_keys(vec![
            key("fk_b", "x", "px"),
            key("fk_a", "c1", "p1"),
            key("fk_a", "c2", "p2"),
        ]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].name, "fk_b");
        assert_eq!(grouped[0].columns, ["x"]);
        assert_eq!(grouped[1].name, "fk_a");
        assert_eq!(grouped[1].columns, ["c1", "c2"]);
        assert_eq!(grouped[1].ref_columns, ["p1", "p2"]);
        assert_eq!(grouped[1].on_delete, "");
    }

    /// 没有约束名时不归并（宁可多条，也不把几张表的引用列混一条）。
    #[test]
    fn unnamed_foreign_keys_stay_separate() {
        let key = |column: &str| duck_db_agent::ForeignKeyInfo {
            name: String::new(),
            column: column.to_owned(),
            ref_table: "parent".to_owned(),
            ref_column: "p".to_owned(),
        };
        let grouped = group_foreign_keys(vec![key("a"), key("b")]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].name, "");
    }

    /// 单元格映射：整数精确、小数按浮点、结构化值按文本、NULL 是真 NULL。
    #[test]
    fn cells_map_into_the_shared_value_model() {
        assert_eq!(cell_value(&serde_json::json!(null)), Value::Null);
        assert_eq!(cell_value(&serde_json::json!(true)), Value::Bool(true));
        assert_eq!(cell_value(&serde_json::json!(7)), Value::Int(7));
        // u64 上界：`as_i64` 拿不到，必须走 `as_u64`，否则会被当浮点。
        assert_eq!(
            cell_value(&serde_json::json!(18_446_744_073_709_551_615u64)),
            Value::UInt(u64::MAX)
        );
        assert_eq!(cell_value(&serde_json::json!(1.5)), Value::Float(1.5));
        assert_eq!(cell_value(&serde_json::json!("x")), Value::Text("x".into()));
        assert_eq!(
            cell_value(&serde_json::json!({"a": 1})),
            Value::Json(r#"{"a":1}"#.into())
        );
    }

    /// `COUNT(*)` 的取数：数字、字符串、拿不到。
    #[test]
    fn row_counts_survive_the_wire_shapes() {
        assert_eq!(cell_as_u64(&serde_json::json!(42)), 42);
        assert_eq!(cell_as_u64(&serde_json::json!("42")), 42);
        assert_eq!(cell_as_u64(&serde_json::json!(-1)), 0);
        assert_eq!(cell_as_u64(&serde_json::json!(null)), 0);
    }

    /// 行数上限：0 不能落到协议上（对端把 0 当「不设上限」）。
    #[test]
    fn a_zero_row_limit_never_reaches_the_wire() {
        assert_eq!(row_limit(0), u32::try_from(MAX_PAGE_SIZE).expect("页大小"));
        assert_eq!(row_limit(500), 500);
    }

    /// 列名比类型多时补齐空类型，而不是错位。
    #[test]
    fn missing_column_types_do_not_shift_the_row() {
        let outcome: QueryOutcome = serde_json::from_value(serde_json::json!({
            "columns": ["a", "b"],
            "column_types": ["int4"],
            "rows": [[1, 2]],
        }))
        .expect("结果");
        let columns = columns_of(&outcome);
        assert_eq!(columns[0].type_name, "int4");
        assert_eq!(columns[1].name, "b");
        assert_eq!(columns[1].type_name, "");
    }

    /// 连接参数：端口只在用户填过时才算「明确指定」。
    #[test]
    fn the_port_explicit_flag_follows_the_profile() {
        let mut cfg = ConnectionConfig {
            id: "conn-1".into(),
            kind: crate::config::DatabaseKind::Agent,
            driver: Some("dameng".into()),
            host: "10.0.0.5".into(),
            database: "DAMENG".into(),
            user: "SYSDBA".into(),
            ..ConnectionConfig::default()
        };
        let request = connect_request(&cfg, Some("pw"));
        assert_eq!(request.port, 0);
        assert!(!request.port_explicit, "没填端口就不该说自己指定过");
        assert_eq!(request.session_role, SessionRole::Metadata);
        assert_eq!(request.password, "pw");

        cfg.port = Some(5236);
        let request = connect_request(&cfg, None);
        assert_eq!(request.port, 5236);
        assert!(request.port_explicit);
        assert_eq!(request.password, "");
    }

    /// 服务名各归各家地过线：GBase 8s 的 `gbase_server`、Informix 的 `informix_server`
    /// 只在自己那家出现（`skip_serializing_if`，线上不出现就是没这回事），空白一律当
    /// 「留空走对端缺省回落」，不把空壳送过去。
    #[test]
    fn server_names_go_to_their_own_engine_only() {
        let mut cfg = ConnectionConfig {
            id: "conn-1".into(),
            kind: crate::config::DatabaseKind::Agent,
            driver: Some("gbase8s".into()),
            host: "10.0.0.9".into(),
            ..ConnectionConfig::default()
        };
        cfg.gbase_server = Some("gbase01".into());
        cfg.informix_server = Some("ol_informix1170".into());
        let request = connect_request(&cfg, None);
        assert_eq!(request.gbase_server.as_deref(), Some("gbase01"));
        assert!(
            request.informix_server.is_none(),
            "GBase 8s 的档案不该把 Informix 的服务名也带上"
        );

        cfg.driver = Some("informix".into());
        let request = connect_request(&cfg, None);
        assert_eq!(request.informix_server.as_deref(), Some("ol_informix1170"));
        assert!(request.gbase_server.is_none());

        // 空白（含纯空格）当「没填」：对端自己有缺省回落。
        cfg.gbase_server = Some("   ".into());
        let request = connect_request(&cfg, None);
        assert!(request.gbase_server.is_none());
    }

    /// 不做的那些动作要**说清楚是协议不支持**，而不是「未知错误」。
    #[test]
    fn unsupported_actions_say_so() {
        let error = unsupported("table editing", None);
        assert!(error.to_string().contains("does not support"));
        match error {
            DbError::Unsupported(_) => {}
            other => panic!("应当是 Unsupported，实际 {other:?}"),
        }
    }
}
