//! DuckDB：跑**官方 sidecar worker**（`dbx-agent-duckdb-*`）的引擎。
//!
//! # 为什么是这个形状
//!
//! 三条路都试过，这是唯一真走得通的：
//!
//! 1. **内嵌 `duckdb` crate**：编译一次要几分钟、`libduckdb-sys` 进主依赖图，
//!    而它换来的只是「一个文件库」——不值当（本仓连 `xlsx` 都不肯引，见
//!    `docs/projects-design.md`）。
//! 2. **自己写一个 worker**：能说本仓的 JSON-RPC，但**没有任何地方能发布我们的工件**
//!    （驱动商店的下载源是 dbx 的 release，见 `duck-drivers::source`）⇒ 用户点了
//!    「安装」会去下一条不存在的 URL。
//! 3. **说 dbx 的 sidecar 协议**（本文件）：工件是官方发布的六平台原生件，装上即用；
//!    代价是**两套封套**——`duck-db-agent` 里那个 [`WireDialect::DbxSidecar`] 就是它。
//!
//! 换句话说：这一档不是「另一种 agent」，而是**协议不同、行为也不同**的一条独立引擎，
//! 只是借了同一套「起子进程 + 每行一条 JSON」的传输层。
//!
//! # 与 agent 轨的三处不同（都影响正确性，别照 agent.rs 抄）
//!
//! 1. **没有会话**。agent 轨有 `open_session` / `agentSessionId` / 服务端游标；
//!    worker 只有一条连接、`session_id` 恒 `null`。所以这里没有会话簿记，`cancel`
//!    也不需要点名（worker 自己知道哪条查询在跑）。
//! 2. **`execute` 是异步回报的**。worker 把查询丢进 `tokio::spawn` 接着读下一行 ——
//!    于是跑着查询时再发一条 `execute` 会得到 `duckdb_worker_busy`（不是排队）。
//!    本引擎的每个方法都是「发一条、等一条」的阻塞调用，不会自己撞上这条；但
//!    **取消**正是靠这个机制插进去的（见 [`DuckDbConnection::cancel_handle`]）。
//! 3. **中毒要自己收拾**。个别失败（语法错触发 duckdb-rs 的连接毒化）worker 会回
//!    `duckdb_worker_poisoned` 并**等着父进程杀它**——dbx 的做法是父进程 `kill` 掉、
//!    下一条请求时换一个新 worker。本引擎照做（见 [`DuckDbConnection::call`]）：
//!    收到这个码就杀进程、丢掉句柄，下次用时重新起 + 重连。
//!
//! # 只做读（以及用户自己写的 SQL）
//!
//! 与 agent 轨同一条纪律，理由也一样：sidecar 协议**没有参数绑定**
//! （`DuckDbWorkerExecuteParams` 只有 `sql` / `database` / `maxRows`），
//! 我们替用户拼的写语句只能把值当字面量塞进 SQL。所以：
//!
//! - 读路径（目录浏览 / 表页 / 查询）全通；
//! - 改格 / 删行 / 插行 / 批量提交一律 [`DbError::Unsupported`]，要写就在 SQL
//!   编辑器里自己写（那条路是用户写的 SQL，不是我们替他拼的 UPDATE）。

use std::sync::Arc;
use std::time::Duration;

use duck_db_agent::{AgentError, AgentHandle, AgentProcess, WireDialect};
use duck_drivers::{DriverEntry, DriverStore};
use serde_json::{Value, json};

use super::agent::{table_select_sql, unsupported};
use super::{
    CatalogTable, OrderBy, RowCount, TableColumnMeta, TableFilter, TableForeignKey, TableIndex,
    TableKind, TableRef, TableTrigger, quote_identifier, wrap_paged_sql,
};
use crate::cancel::CancelHandle;
use crate::config::ConnectionConfig;
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value as Cell};

/// 建连超时（起进程 + 打开文件 / 建 WAL 都可能慢）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 元数据请求超时。
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);
/// 语句执行超时下限（真正的上限由档案的 `default_timeout` 给）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// 收尾宽限。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// DuckDB 的标识符引号。worker 没有「报告引号」这一步，而 DuckDB 就是 `"`。
const QUOTE: char = '"';

/// worker 的方法名（`DuckDbWorkerMethod` 的 snake_case 拼法，**照抄对端**）。
mod method {
    pub(super) const CONNECT: &str = "connect";
    pub(super) const EXECUTE: &str = "execute";
    pub(super) const LIST_DATABASES: &str = "list_databases";
    pub(super) const LIST_SCHEMAS: &str = "list_schemas";
    pub(super) const LIST_TABLES: &str = "list_tables";
    pub(super) const LIST_COLUMNS: &str = "list_columns";
    pub(super) const GET_TABLE_DDL: &str = "get_table_ddl";
    pub(super) const CANCEL: &str = "cancel";
    pub(super) const SHUTDOWN: &str = "shutdown";
}

/// 取消当前那条查询的方法名（[`crate::cancel`] 代发）。
///
/// worker 的 `cancel` **不用点名**：它只有一条连接、一个正在跑的查询，自己知道打断谁
/// —— 这与 agent 轨的 `cancel_session`（要带 `agentSessionId`）是两套语义。
pub(crate) const CANCEL: &str = method::CANCEL;

/// 「这个连接已经烂了，把我杀了重开」——worker 自报的错误码。
///
/// 收到它必须**在父进程侧杀**：worker 自己不能 `exit`（那会和父进程的下一条请求
/// 抢窗口，请求可能写进一个正在死的进程）。
const POISONED: &str = "duckdb_worker_poisoned";

/// 驱动目录里的键（档案的 `driver` 就是这个值，也是「走这条引擎」的判据）。
pub(crate) const DRIVER_KEY: &str = "duckdb";

/// 一条 DuckDB 连接：一个 worker 进程 + 一条连接（worker 没有会话层）。
pub struct DuckDbConnection {
    /// 驱动进程句柄。`None` = 需要（重新）起一个 —— 见 [`Self::call`] 的中毒分支。
    handle: Option<Arc<AgentHandle>>,
    /// 起进程用的**配方**（中毒重起时原样再用一次）。
    spec: AgentProcess,
    /// **数据库文件路径**（`:memory:` 允许）。中毒重起时要拿它再 `connect` 一次 ——
    /// 不能用 `database`（那是 worker 报的主库名，`:memory:` 下叫 `main`）。
    path: String,
    /// 目录条目（展示名与「没装驱动」的提示都靠它）。
    entry: &'static DriverEntry,
    /// 主库名（worker 报的第一个 catalog，如 `main`）。
    database: String,
    /// schema 参数（只影响我们拼给 worker 的参数）。
    schema: Option<String>,
    /// 连接级只读档：写语句在下发之前拦下（与 agent 轨同一条口径）。
    read_only: bool,
    /// 单条语句的超时上限。
    timeout: Duration,
}

impl DuckDbConnection {
    /// 建连：找已安装的 worker 工件、起进程、`connect` 到文件（或 `:memory:`）。
    ///
    /// `cfg.database` 就是 **DuckDB 文件路径**（`:memory:` 允许）—— 与 SQLite 同一条
    /// 口径：文件引擎的「库名」那一格放的是路径。
    ///
    /// # Errors
    /// - 档案里没有驱动键 / 键不在目录里（[`DbError::Config`]）；
    /// - 工件没装或坏了（[`DbError::Connect`]，界面据此提示「去驱动管理里装」）；
    /// - 文件打不开（worker 回 `duckdb_connect_failed` 之类）；
    /// - 没填文件（[`DbError::Config`]）——`:memory:` 要用户自己写出来。
    pub(crate) fn connect(
        cfg: &ConnectionConfig,
        _password: Option<&str>,
    ) -> Result<Self, DbError> {
        cfg.validate()?;
        let entry = cfg.driver_entry().ok_or_else(|| {
            DbError::Config(format!(
                "connection {} has no known agent driver key",
                cfg.id
            ))
        })?;
        let path = cfg.database.trim();
        if path.is_empty() {
            return Err(DbError::Config(
                "a DuckDB connection needs a database file path (use `:memory:` for an in-memory one)"
                    .into(),
            ));
        }

        let store = DriverStore::new();
        let launch = store.launch_spec(entry.key).map_err(|error| {
            DbError::Connect(format!(
                "the {} driver is not ready to run ({error}); install or update it in the driver manager",
                entry.label
            ))
        })?;
        let spec = AgentProcess {
            program: launch.program.clone(),
            args: launch.args.clone(),
            working_dir: Some(launch.working_dir.clone()),
            env: Vec::new(),
        };

        let mut connection = Self {
            handle: None,
            spec,
            path: path.to_owned(),
            entry,
            database: String::new(),
            schema: None,
            read_only: cfg.read_only,
            timeout: cfg
                .default_timeout()
                .map_or(QUERY_TIMEOUT, Duration::from_secs),
        };
        // 起进程 + `connect` 一起做：这条路上任何一步失败都是「连不上」。
        connection.start(path)?;
        Ok(connection)
    }

    /// 起一个新 worker 并 `connect` 到 `path`（建连与中毒重起共用）。
    fn start(&mut self, path: &str) -> Result<(), DbError> {
        let handle =
            AgentHandle::spawn_with(&self.spec, WireDialect::DbxSidecar).map_err(|error| {
                DbError::Connect(format!(
                    "cannot start the {} driver process: {error}",
                    self.entry.label
                ))
            })?;
        let handle = Arc::new(handle);
        // ⚠️ **没有握手**：sidecar 的第一步业务请求就是 `connect`（见
        // `AgentHandle::handshake` 的说明）。
        call_raw(
            &handle,
            method::CONNECT,
            json!({ "path": path }),
            CONNECT_TIMEOUT,
        )
        .map_err(|error| describe(self.entry, "connect", error).into_connect())?;

        // 主库名：列表的第一条（`:memory:` 是 `main`，文件库也是文件名）。
        // 拿不到就用 `main` 兜底 —— 它只用于后续元数据请求的参数，猜错会有明确的错报出来。
        self.database = list_databases_raw(&handle)
            .ok()
            .and_then(|names| names.into_iter().next())
            .unwrap_or_else(|| "main".to_owned());
        self.handle = Some(handle);
        Ok(())
    }

    /// 当前句柄（没有就说明上一次中了毒，这里顺手重起 + 重连）。
    ///
    /// 重连用的路径是**建连时那个**：`self.database` 记的是 worker 报的主库名
    /// （`:memory:` 下是 `main`），不是文件路径，所以路径另存一份。
    fn worker(&mut self) -> Result<Arc<AgentHandle>, DbError> {
        match self.handle.clone() {
            Some(handle) => Ok(handle),
            None => {
                let path = self.path.clone();
                self.start(&path)?;
                self.handle
                    .clone()
                    .ok_or_else(|| DbError::Connect("the DuckDB worker did not start".into()))
            }
        }
    }

    /// 发一条请求，并把失败翻成 [`DbError`]。
    ///
    /// **中毒自愈就在这里**：收到 [`POISONED`] 时杀掉进程、把句柄置空（下一次请求
    /// 会重新起一个并重连），然后把错误如实抛给用户 —— 那一次调用已经失败，只是
    /// 连接不会从此报废。
    fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, DbError> {
        let handle = self.worker()?;
        match call_raw(&handle, method, params, timeout) {
            Ok(value) => Ok(value),
            Err(AgentError::Remote { message, data, .. })
                if data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_str)
                    == Some(POISONED) =>
            {
                // 杀 + 标记：`shutdown` 是幂等的，之后 `Drop` 也跟着做一遍。
                handle.shutdown();
                self.handle = None;
                Err(DbError::Query(format!(
                    "the DuckDB worker hit an unrecoverable error and was restarted: {message}"
                )))
            }
            Err(error) => Err(describe(self.entry, method, error)),
        }
    }

    // ── Connection 门面（名字与签名对齐其它引擎） ──────────────────────────

    /// 目录条目。
    pub(crate) fn entry(&self) -> &'static DriverEntry {
        self.entry
    }

    /// 标识符引号：DuckDB 恒为 `"`（worker 没有「报告引号」这一步）。
    pub(crate) const fn identifier_quote(&self) -> Option<char> {
        Some(QUOTE)
    }

    /// 支持分页续拉：本引擎把语句包进外层 `LIMIT/OFFSET`（DuckDB 支持这套语法）。
    pub(crate) const fn supports_paged_read(&self) -> bool {
        true
    }

    /// 带外取消句柄（worker 的 `cancel` 不需要点名）。
    pub(crate) fn cancel_handle(&self) -> CancelHandle {
        match self.handle.clone() {
            Some(handle) => CancelHandle::duckdb(handle),
            // 没有句柄（中毒待重起）时给一个「什么都取消不了」的句柄：
            // 取消的语义本来就是「尽力而为」，这里如实空转。
            None => CancelHandle::none(),
        }
    }

    /// 活性探测：worker 没有 `validate_session`，跑一条最便宜的 `SELECT 1`。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.execute("SELECT 1", 1).map(|_| ())
    }

    /// 引擎版本（`SELECT version()`）。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        let outcome = self.execute("SELECT version()", 1)?;
        outcome
            .result
            .and_then(|result| result.rows.first().and_then(|row| row.first().cloned()))
            .map(|value| value.render())
            .ok_or_else(|| DbError::Query("the DuckDB worker did not report a version".into()))
    }

    /// 只读档（在下发之前拦写语句，见 [`Self::refuse_writes`]）。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 当前 schema（只影响拼给 worker 的参数）。
    pub fn set_namespace(&mut self, schema: Option<&str>) {
        self.schema = schema.map(str::to_owned);
    }

    /// 切库：DuckDB 的 `USE` 能切到已 attach 的 catalog（同一进程内，不必重连）。
    pub fn switch_database(&mut self, name: &str) -> Result<super::SwitchOutcome, DbError> {
        let statement = format!("USE {}", quote_identifier(name, QUOTE)?);
        self.execute(&statement, 1)?;
        self.database = name.to_owned();
        Ok(super::SwitchOutcome::Applied)
    }

    /// 库清单（DuckDB 的 catalog：主库 + 各个 `ATTACH` 进来的库）。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        let value = self.call(method::LIST_DATABASES, json!({}), METADATA_TIMEOUT)?;
        Ok(database_names(&value))
    }

    /// 命名空间清单（当前库里的 schema）。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let value = self.call(
            method::LIST_SCHEMAS,
            json!({ "database": self.database }),
            METADATA_TIMEOUT,
        )?;
        let mut names: Vec<String> = value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        names.retain(|name| !name.trim().is_empty());
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// 一个命名空间里的表与视图。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let schema = schema
            .map(str::to_owned)
            .or_else(|| self.schema.clone())
            .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
        let value = self.call(
            method::LIST_TABLES,
            json!({ "database": self.database, "schema": schema }),
            METADATA_TIMEOUT,
        )?;
        let tables = value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_owned();
                        let kind = item
                            .get("table_type")
                            .and_then(Value::as_str)
                            .map_or(TableKind::Table, table_kind);
                        Some(CatalogTable {
                            name,
                            kind,
                            schema: Some(schema.clone()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tables)
    }

    /// 一张表的列（名称 + 类型 + 注释）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        Ok(self
            .columns_of(table)?
            .into_iter()
            .map(|column| Column {
                name: column.name,
                type_name: column.type_name,
                comment: column.comment,
            })
            .collect())
    }

    /// 一张表的列级元数据（表结构编辑器用）。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        Ok(self
            .columns_of(table)?
            .into_iter()
            .map(|column| TableColumnMeta {
                name: column.name,
                type_name: column.type_name,
                comment: column.comment,
                nullable: column.nullable,
                default: column.default,
                // DuckDB 没有列级字符集 / 排序规则（库级 collation 在建库时定）。
                character_set: None,
                collation: None,
            })
            .collect())
    }

    /// 一张表的主键列名（按 `list_columns` 的声明顺序）。
    ///
    /// ⚠️ 本档**不做行编辑**（协议没有参数绑定，见模块文档），主键只用于
    /// [`Self::table_page`] 的稳定序 —— 没有它翻页可能重复 / 漏行。
    pub fn table_primary_key(&mut self, table: &TableRef) -> Result<Vec<String>, DbError> {
        Ok(self
            .columns_of(table)?
            .into_iter()
            .filter(|column| column.primary_key)
            .map(|column| column.name)
            .collect())
    }

    /// 表页：`SELECT * FROM … [WHERE …] [ORDER BY …]` + 外层 `LIMIT/OFFSET`。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let sql = table_select_sql(table, order_by, filters, QUOTE)?;
        self.execute_paged(&sql, offset, limit)
    }

    /// 提交一段 SQL（可含多条语句），读语句的行在 `max_rows` 处截断。
    ///
    /// 多语句**一次送给 worker**：它自己按顺序执行并只回最后一条读语句的结果集、
    /// 累加写语句的影响行数（实测 `CREATE TABLE …; INSERT …;` 回 `affected_rows: 2`），
    /// 这与本方法的语义正好一致。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let statements = crate::sqlintel::split_statements(sql);
        if statements.is_empty() {
            return Ok(ExecOutcome::default());
        }
        if self.read_only {
            self.refuse_writes(&statements)?;
        }
        let params = json!({
            "sql": sql,
            // `0` 对 worker 是「不设上限」——界面传 0（不该发生）时换成 1，别把整张表拉回来。
            "maxRows": max_rows.max(1),
            "database": self.database,
        });
        let result = self.call(method::EXECUTE, params, self.timeout)?;
        let mut outcome = ExecOutcome {
            result: query_result(&result),
            affected: result
                .get("affected_rows")
                .and_then(Value::as_i64)
                .and_then(|rows| u64::try_from(rows).ok())
                .unwrap_or(0),
            statements_run: statements.len(),
        };
        // worker 的 `truncated` / `has_more` 都表示「还有更多行没回来」。
        if let Some(result_set) = outcome.result.as_mut() {
            result_set.truncated |= result
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
        Ok(outcome)
    }

    /// 分页读一段 SQL：包一层 `LIMIT/OFFSET`（多取一行探测「还有下一页」）。
    pub fn execute_paged(
        &mut self,
        sql: &str,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet, DbError> {
        let inner = super::paged_inner_sql(sql)?;
        let wrapped = wrap_paged_sql(&inner, offset, limit, QUOTE)?;
        let outcome = self.execute(&wrapped, super::paged_probe(limit))?;
        let mut result = outcome.result.unwrap_or_default();
        // 探到的那一行不算结果：扔掉并标 `truncated`。
        if result.rows.len() > limit {
            result.rows.truncate(limit);
            result.truncated = true;
        }
        Ok(result)
    }

    /// 建表 DDL（worker 的 `get_table_ddl` 给的是引擎原文）。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let schema = table
            .schema
            .clone()
            .or_else(|| self.schema.clone())
            .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
        let value = self.call(
            method::GET_TABLE_DDL,
            json!({ "database": self.database, "schema": schema, "table": table.name }),
            METADATA_TIMEOUT,
        )?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| DbError::Query("the DuckDB worker returned a non-text table DDL".into()))
    }

    /// 行数：DuckDB 没有可信的系统统计，走 `COUNT(*)` ⇒ 精确值。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let qualified = super::quote_qualified(table, QUOTE)?;
        let outcome = self.execute(&format!("SELECT count(*) FROM {qualified}"), 1)?;
        let count = outcome
            .result
            .and_then(|result| result.rows.first().and_then(|row| row.first().cloned()))
            .map(|value| value.render().parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        Ok(RowCount::Exact(count))
    }

    /// 不做的那些（与 agent 轨同一句话）。
    pub(crate) fn unsupported(&self, action: &str) -> DbError {
        unsupported(action, Some(self.entry))
    }

    // ── 协议里没有的这几样（与 agent 轨同一条理由，见 [`super::agent`]）──────
    //
    // worker 的方法表只有 `list_databases / list_schemas / list_tables / list_columns /
    // get_table_ddl`，索引 / 外键 / 触发器一个都没有。左栏据此不画那三档
    // （`list_triggers`/`list_indexes` 同样恒空），右栏的表级入口照旧按「空清单」显示。

    /// 索引：协议没有这项 ⇒ 恒空（不是「查了没查到」）。
    pub fn table_indexes(&mut self, _table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        Ok(Vec::new())
    }

    /// 外键：同上。
    pub fn table_foreign_keys(
        &mut self,
        _table: &TableRef,
    ) -> Result<Vec<TableForeignKey>, DbError> {
        Ok(Vec::new())
    }

    /// 触发器：同上。
    pub fn table_triggers(&mut self, _table: &TableRef) -> Result<Vec<TableTrigger>, DbError> {
        Ok(Vec::new())
    }

    /// 收尾：请 worker 自己退（它会先 checkpoint 再回 ack）。
    pub fn close(self) {
        if let Some(handle) = self.handle {
            // `SHUTDOWN`：worker 关掉连接（把 WAL 落干净）之后再回 `{"shutdown":true}`。
            let _ = call_raw(&handle, method::SHUTDOWN, json!({}), CLOSE_TIMEOUT);
            handle.shutdown();
        }
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /// 列的原始信息（`list_columns` 一次拿全，两个门面各取所需）。
    fn columns_of(&mut self, table: &TableRef) -> Result<Vec<RawColumn>, DbError> {
        let schema = table
            .schema
            .clone()
            .or_else(|| self.schema.clone())
            .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned());
        let value = self.call(
            method::LIST_COLUMNS,
            json!({ "database": self.database, "schema": schema, "table": table.name }),
            METADATA_TIMEOUT,
        )?;
        Ok(value
            .as_array()
            .map(|items| items.iter().map(RawColumn::from_json).collect())
            .unwrap_or_default())
    }

    /// 只读档下拒写（与 `agent.rs::refuse_writes` 同一条口径：**下发之前**拦）。
    fn refuse_writes(&self, statements: &[String]) -> Result<(), DbError> {
        for statement in statements {
            if crate::sqlintel::classify(statement) != crate::sqlintel::StmtKind::Read {
                return Err(DbError::Unsupported(format!(
                    "this connection is read-only; refused a statement that is not a read: {statement}"
                )));
            }
        }
        Ok(())
    }
}

/// 新建库时 worker 用的默认 schema（DuckDB 的 `main`）。
const DEFAULT_SCHEMA: &str = "main";

/// `list_columns` 的原始一行。
struct RawColumn {
    name: String,
    type_name: String,
    comment: String,
    nullable: Option<bool>,
    default: Option<String>,
    primary_key: bool,
}

impl RawColumn {
    fn from_json(item: &Value) -> Self {
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Self {
            name: text("name"),
            // `data_type` 是 DuckDB 的类型名原文（`INTEGER` / `VARCHAR` / `DECIMAL(18,3)`）。
            type_name: text("data_type"),
            comment: text("comment"),
            nullable: item.get("is_nullable").and_then(Value::as_bool),
            default: item
                .get("column_default")
                .and_then(Value::as_str)
                .map(str::to_owned),
            primary_key: item
                .get("is_primary_key")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// 发一条请求（不碰 [`DuckDbConnection`] 的状态，重连路径也要用）。
fn call_raw(
    handle: &AgentHandle,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, AgentError> {
    handle.call(method, params, timeout)
}

/// 库清单（建连时也要用一次，所以单独拆出来）。
fn list_databases_raw(handle: &AgentHandle) -> Result<Vec<String>, AgentError> {
    call_raw(handle, method::LIST_DATABASES, json!({}), METADATA_TIMEOUT)
        .map(|value| database_names(&value))
}

/// `list_databases` 的回应 → 名字清单（`[{"name":"main"}]`）。
fn database_names(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    names.retain(|name| !name.trim().is_empty());
    names.sort();
    names.dedup();
    names
}

/// worker 的 `table_type` → 我们的 [`TableKind`]。
///
/// worker 报的是 SQL 标准的 `BASE TABLE` / `VIEW`（大小写不一定），认不出的按表处理
/// ——这是 [`TableKind::is_relation`] 最宽的那一档，宁可让用户在树上看见它。
fn table_kind(table_type: &str) -> TableKind {
    match table_type.trim().to_ascii_uppercase().as_str() {
        "VIEW" => TableKind::View,
        _ => TableKind::Table,
    }
}

/// 执行结果 → [`ResultSet`]（没有结果集的写语句返回 `None`）。
fn query_result(value: &Value) -> Option<ResultSet> {
    let columns = value.get("columns")?.as_array()?;
    if columns.is_empty() {
        return None;
    }
    let types = value.get("column_types").and_then(Value::as_array);
    let names: Vec<Column> = columns
        .iter()
        .enumerate()
        .map(|(index, name)| Column {
            name: name.as_str().unwrap_or_default().to_owned(),
            type_name: types
                .and_then(|types| types.get(index))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            // 查询结果的列常常是表达式 / 别名，没有可信的注释来源（与 agent 轨一致）。
            comment: String::new(),
        })
        .collect();
    let rows = value
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| cells.iter().map(cell_value).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ResultSet {
        columns: names,
        rows,
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 一个 JSON 单元格 → 内核的 [`Cell`]。
///
/// 整数优先按 `i64` / `u64` 取值（精确），拿不到才落到 `f64`；
/// 数组与对象（DuckDB 的 `LIST` / `STRUCT` / `MAP`）按 JSON 文本承载。
fn cell_value(value: &Value) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Bool(flag) => Cell::Bool(*flag),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Cell::Int(int)
            } else if let Some(uint) = number.as_u64() {
                Cell::UInt(uint)
            } else {
                number.as_f64().map_or(Cell::Null, Cell::Float)
            }
        }
        Value::String(text) => Cell::Text(text.clone()),
        Value::Array(_) | Value::Object(_) => Cell::Json(value.to_string()),
    }
}

/// 一条失败 → 带引擎名与动作的说明（与 agent 轨同一口径，英文原文）。
fn describe(entry: &DriverEntry, action: &str, error: AgentError) -> DbError {
    match error {
        AgentError::Remote { message, .. } => DbError::Query(format!("{}: {message}", entry.label)),
        AgentError::Timeout { waited, .. } => DbError::Query(format!(
            "the {} driver did not answer {action} within {waited:?}",
            entry.label
        )),
        AgentError::Closed => {
            DbError::Connect(format!("the {} driver process is gone", entry.label))
        }
        other => DbError::Query(format!(
            "the {} driver failed during {action}: {other}",
            entry.label
        )),
    }
}

/// 建连失败时用 [`DbError::Connect`]，其余用 [`DbError::Query`]。
trait IntoConnect {
    fn into_connect(self) -> DbError;
}

impl IntoConnect for DbError {
    fn into_connect(self) -> DbError {
        match self {
            DbError::Query(message) => DbError::Connect(message),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 库清单：`[{"name":"main"}]` → 名字，剔空、排序、去重。
    #[test]
    fn database_names_are_cleaned_up() {
        let value = json!([{ "name": "b" }, { "name": "a" }, { "name": "" }, { "name": "a" }]);
        assert_eq!(database_names(&value), vec!["a".to_owned(), "b".to_owned()]);
        assert!(database_names(&json!(null)).is_empty());
    }

    /// 表类型：`VIEW` 认成视图，其余（含大小写与未知值）都按表 —— 宁可在树上多画。
    #[test]
    fn table_kinds_map_leniently() {
        assert_eq!(table_kind("VIEW"), TableKind::View);
        assert_eq!(table_kind(" view "), TableKind::View);
        assert_eq!(table_kind("BASE TABLE"), TableKind::Table);
        assert_eq!(table_kind("whatever"), TableKind::Table);
    }

    /// 单元格映射：整数精确、浮点走 `Float`、复杂类型落 JSON 文本、NULL 是 NULL。
    #[test]
    fn cell_values_keep_integers_exact() {
        assert_eq!(cell_value(&json!(null)), Cell::Null);
        assert_eq!(cell_value(&json!(true)), Cell::Bool(true));
        assert_eq!(cell_value(&json!(-7)), Cell::Int(-7));
        assert_eq!(cell_value(&json!(u64::MAX)), Cell::UInt(u64::MAX));
        assert_eq!(cell_value(&json!(1.5)), Cell::Float(1.5));
        assert_eq!(cell_value(&json!("x")), Cell::Text("x".to_owned()));
        // LIST / STRUCT 按 JSON 文本承载（别把结构悄悄摊成字符串）。
        assert_eq!(cell_value(&json!([1, 2])), Cell::Json("[1,2]".to_owned()));
    }

    /// 结果集：列名 / 类型按位取，没有列就是「没有结果集」（写语句）。
    #[test]
    fn query_results_need_columns() {
        let result = query_result(&json!({
            "columns": ["a", "b"],
            "column_types": ["INTEGER", "VARCHAR"],
            "rows": [[1, "x"], [2, null]],
            "truncated": true,
        }))
        .expect("应当是结果集");
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[1].type_name, "VARCHAR");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[1][1], Cell::Null);
        assert!(result.truncated);

        assert!(query_result(&json!({ "columns": [], "rows": [] })).is_none());
        assert!(query_result(&json!({ "affected_rows": 2 })).is_none());
    }

    /// 只读档拒写：读放行、写在下发之前就拒（一条都不发出去）。
    #[test]
    fn read_only_refuses_writes_before_they_are_sent() {
        let connection = DuckDbConnection {
            handle: None,
            spec: AgentProcess {
                program: "/nonexistent".into(),
                args: Vec::new(),
                working_dir: None,
                env: Vec::new(),
            },
            entry: duck_drivers::entry("duckdb").expect("duckdb 在驱动目录里"),
            path: ":memory:".to_owned(),
            database: "main".to_owned(),
            schema: None,
            read_only: true,
            timeout: QUERY_TIMEOUT,
        };
        assert!(connection.refuse_writes(&["SELECT 1".to_owned()]).is_ok());
        let error = connection
            .refuse_writes(&["DELETE FROM t".to_owned()])
            .expect_err("写语句应当被拒");
        match error {
            DbError::Unsupported(message) => assert!(message.contains("read-only")),
            other => panic!("应当是 Unsupported，实际 {other:?}"),
        }
    }

    /// **真机对线**：起真 worker、建表、写、查、读目录、翻一页、收尾。
    ///
    /// 默认 `#[ignore]`：它要的是**已安装的驱动工件**（28 MB，从 dbx 注册表下），
    /// CI 里没有、也不该为一个单测去下一个。手跑：
    ///
    /// ```text
    /// cargo test -p duck-db --locked -- --ignored duckdb_worker_round_trip
    /// ```
    ///
    /// 单测里那些解析函数是拿**手抄的回应**喂的，只有这条能证明「真 worker 真的这么回」——
    /// 协议里最容易错的两处（`ok` 与 `result` 并列、列类型在 `column_types`）都在这条路上。
    #[test]
    #[ignore = "needs the duckdb driver artifact installed in the driver store"]
    fn duckdb_worker_round_trip() {
        use crate::config::DatabaseKind;

        let cfg = ConnectionConfig {
            id: "duckdb-smoke".to_owned(),
            name: "duckdb-smoke".to_owned(),
            kind: DatabaseKind::Agent,
            driver: Some("duckdb".to_owned()),
            database: ":memory:".to_owned(),
            ..ConnectionConfig::default()
        };
        let mut connection =
            crate::drivers::Connection::connect(&cfg, None).expect("该起得来 worker 并连上");

        // 建 + 写（多语句一次下发；worker 自己按分号切）。
        let written = connection
            .execute(
                "CREATE TABLE t(a INTEGER, b VARCHAR); \
                 INSERT INTO t VALUES (1, 'x'), (2, NULL);",
                100,
            )
            .expect("建表与写入该成功");
        assert_eq!(written.affected, 2, "INSERT 的影响行数该按 worker 的回报算");

        // 读：列名 / 类型 / 值（INTEGER 要原样是整数，别退化成浮点）。
        let read = connection
            .execute("SELECT a, b FROM t ORDER BY a", 100)
            .expect("SELECT 该成功");
        let result = read.result.expect("SELECT 该带结果集");
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.columns[0].name, "a");
        assert_eq!(result.columns[1].name, "b");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Cell::Int(1));
        assert_eq!(result.rows[1][1], Cell::Null);

        // 目录：库 → 命名空间 → 表 → 列（左栏树的三层都走真 worker）。
        let databases = connection.list_databases().expect("库清单该拿得到");
        assert!(!databases.is_empty(), "至少该有一个主库：{databases:?}");
        let schemas = connection.list_schemas().expect("命名空间清单该拿得到");
        assert!(
            schemas.iter().any(|s| s == "main"),
            "该有 main：{schemas:?}"
        );
        let tables = connection.catalog(Some("main")).expect("表清单该拿得到");
        let table = tables
            .iter()
            .find(|t| t.name == "t")
            .expect("刚建的表该在清单里");
        assert_eq!(table.schema.as_deref(), Some("main"));

        let columns = connection
            .table_columns(&table_ref("main", "t"))
            .expect("列清单该拿得到");
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[1].name, "b");

        // 表页：包一层 LIMIT/OFFSET（这条同时验证 `supports_paged_read` 不是空话）。
        let page = connection
            .table_page(&table_ref("main", "t"), 1, 1, &[], &[])
            .expect("表页该拿得到");
        assert_eq!(page.rows.len(), 1, "limit=1 只该回一行");

        // 行数（走 COUNT(*)：DuckDB 没有可信的统计行数）。
        let count = connection
            .table_row_count(&table_ref("main", "t"))
            .expect("行数该拿得到");
        assert_eq!(count, RowCount::Exact(2));

        // DDL 与版本：worker 的 `get_table_ddl` 给引擎原文，`version()` 给三段式。
        let ddl = connection
            .table_ddl(&table_ref("main", "t"))
            .expect("DDL 该拿得到");
        assert!(ddl.contains("CREATE TABLE"), "DDL 该是建表语句：{ddl}");
        let version = connection.server_version().expect("版本该拿得到");
        assert!(version.contains('.'), "版本该像 `1.1.3`：{version}");

        // 写路径仍是「一律不做」（协议没有参数绑定）。
        let refused = connection
            .delete_rows(
                &table_ref("main", "t"),
                &vec![("a".to_owned(), "1".to_owned())],
            )
            .expect_err("工具替用户拼的删除该被拒");
        assert!(matches!(refused, DbError::Unsupported(_)));

        connection.close();
    }

    fn table_ref(schema: &str, name: &str) -> TableRef {
        TableRef {
            schema: Some(schema.to_owned()),
            name: name.to_owned(),
        }
    }
}
