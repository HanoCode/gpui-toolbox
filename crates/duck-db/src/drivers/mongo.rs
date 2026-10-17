//! MongoDB 驱动：文档库，但**落到同一套网格契约**上。
//!
//! # 为什么一个文档库能接进网格
//!
//! `docs/database-design.md` §4.2 把驱动抽象成「SQL 网格 + 文档库」两族，但真要让
//! 用户在界面上用得起来，映射比抽象更重要。MongoDB 的层次**刚好同构**：
//!
//! | 网格概念 | MongoDB |
//! | --- | --- |
//! | 库 / schema | database（`list_databases`） |
//! | 表 | collection（`listCollections`；`type=view` 归视图） |
//! | 列 | 文档的**顶层字段**（无固定 schema，靠样本推） |
//! | 行 | 一个文档（`_id` 即这一行的地址） |
//! | 主键 | `_id`（恒存在、恒有唯一索引） |
//! | 分页 | `find + sort(_id) + skip + limit` |
//! | 改一格 | `updateOne({_id: …}, {$set: {f: v}})` |
//! | 建表 DDL | 没有原文出口 → 由 `listCollections` 规格 + 索引**重构** |
//!
//! 所以这条驱动不需要另立一套 UI，直接吃 `db.rs` 的 SQL 网格。**列的清单只能靠
//! 采样**（[`SCHEMA_SAMPLE`] 条文档的字段并集），这是文档库的固有性质，不是偷懒——
//! 空集合推不出列，界面应当老实显示「无样本」。
//!
//! # 查询语言是 JSON 命令
//!
//! 编辑器里输入的是**一个 JSON 文档**，即 `db.runCommand(<doc>)` 的参数
//! （`{"find": "users", "filter": {…}, "limit": 10}`、`{"count": "users"}`、
//! `{"aggregate": "users", "pipeline": […], "cursor": {}}`…）。走这一条而不是自己
//! 造 `db.coll.find({…})` 这种壳语法：**命令文档是服务端的稳定接口**，壳语法是
//! shell 的语法糖，解析它只会引入一个我们自己维护的方言。
//!
//! # 只读档是**客户端拦截**（与 SQL Server 同理）
//!
//! SQLite / PG / MySQL / ClickHouse 的只读都落在**服务端开关**上；MongoDB
//! **没有会话级只读开关**（只能靠只读账号或按库授只读角色）。所以这里由驱动在
//! 命令进引擎**之前**按首关键字拦下（[`command_is_write`]），且**未知命令一律当写**
//! ——安全方向必须是「默认拒绝」，不是「默认放行」。真要防住生产库仍然应该用
//! 只读账号。
//!
//! # 没有多文档事务（如实记录）
//!
//! MongoDB 的多文档事务**只在副本集 / 分片集群上可用**，standalone 直接报错。
//! 所以批处理（[`MongoConnection::apply_batch`] 一族）**不开事务**：要么发**一条**
//! 覆盖多行的命令（`delete_many({$or: […]})` / `update_many`），要么顺序发多条、
//! 按返回的命中数累计。中途失败时前面已生效——调用方按返回的影响行数核对刷新。
//! 这与 `drivers/clickhouse.rs` 的 `insert_rows`（无事务，退化成一条多行 INSERT）
//! 是同一条纪律：**给出引擎能做到的最强原子性，并把边界写在文档里**。
//!
//! # 库名可以留空
//!
//! MongoDB 的连接 URI **本来就不带库**（库是命令的一部分），所以档案里不填库名是
//! 完全合法的：连得上、`listDatabases` 能把所有库列出来，只是命令暂时没有落点。
//! 落点按三级回落（见 [`MongoConnection::resolve_database`]）：**命令里点名 → 档案
//! 记住的命名空间（界面上的下拉写回） → 档案里的默认库**；三级全空就是「未选定库」，
//! 界面给空目录 + 下拉可一键选。
//!
//! # tokio 不进公共 API
//!
//! `mongodb` 3.x 只有 async API。这里每条连接自管一个 current-thread runtime，
//! 所有 `await` 都在它上面 `block_on` —— 与 `drivers/mssql.rs` 同一套做法，
//! **一个 tokio 类型都不出现在本 crate 的公共面上**。

use std::collections::{HashMap, HashSet};
use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mongodb::bson::{Bson, Document, doc};
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection, Cursor, IndexModel};

use crate::cancel::CancelHandle;
use crate::config::{AuthMode, ConnectionConfig, TlsMode};
use crate::drivers::{
    BatchOutcome, CatalogTable, CellEdit, FilterOp, InsertRow, OrderBy, PkValues, RowCount,
    TableColumnMeta, TableFilter, TableIndex, TableKind, TableRef,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};

/// 采样条数：文档库没有固定 schema，列清单只能靠字段并集推。
///
/// 200 与 `db.rs` 的结果网格首屏同量级——够覆盖常规集合的字段，又不至于为了一次
/// 内省去扫一个大集合。
const SCHEMA_SAMPLE: i64 = 200;

/// 建连 / 选主超时。驱动缺省 30 秒太长：连不上要快速失败，而不是把后台线程占住。
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// 管理命令（`ping` / `buildInfo` / `killOp` / `$currentOp`）落在这个库上。
const ADMIN_DB: &str = "admin";

/// `appName` 的进程内序号：同一进程里开多条连接时保证名字不撞。
static APP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `read_only` 档下**允许**通过的命令首关键字（一律小写比较）。
///
/// 这份名单是「白名单」而不是「黑名单」（见 [`command_is_write`]）。收录口径：
/// 只读地读数据或读元数据的命令。`explain` 只解释不执行，`getLog` / `serverStatus`
/// 是运维只读面，都算。
const READ_COMMANDS: &[&str] = &[
    "aggregate",
    "buildinfo",
    "collstats",
    "connectionstatus",
    "count",
    "datasize",
    "dbstats",
    "distinct",
    "explain",
    "find",
    "getcmdlineopts",
    "getdefaultrwconcern",
    "getlog",
    "getparameter",
    "hello",
    "hostinfo",
    "ismaster",
    "listcollections",
    "listdatabases",
    "listindexes",
    "listsearchindexes",
    "ping",
    "replsetgetstatus",
    "serverstatus",
    "whatsmyuri",
];

/// 一条已建立的 MongoDB 连接。
///
/// 字段顺序是有意的：`client` 在 `runtime` **之前**声明，析构时就先掉 client
/// 再掉 runtime —— 反过来会让 client 在**已关闭的 runtime** 上收摊。
pub struct MongoConnection {
    /// 驱动客户端（内部带连接池；`Client` 是 `Clone` 的句柄）。
    client: Client,
    /// 自管的 current-thread runtime：`mongodb` 只有 async API，本 crate 是阻塞面。
    runtime: tokio::runtime::Runtime,
    /// 默认库（`ConnectionConfig.database`；表浏览 / 命令的落点）。**可留空**。
    database: String,
    /// 档案里记住的命名空间（`ConnectionConfig.schema`，界面上的命名空间下拉写回）。
    ///
    /// 它是「上次浏览的库」，比 `database` 更贴近用户当前想看的落点，所以解析库名时
    /// **优先于** `database`（见 [`MongoConnection::resolve_database`]）。
    namespace: Option<String>,
    /// 只读档：见模块文档（客户端拦截）。
    read_only: bool,
    /// `buildInfo.version`（连接时取一次；取不到就是 `unknown`）。
    version: String,
    /// 本条连接独有的 `appName`：取消时靠它点名「谁在跑」。
    app_name: String,
    /// 取消通道要另开一条客户端，故把 URI 留一份（**含口令，只在内存**）。
    uri: String,
}

impl MongoConnection {
    /// 建连：拼 URI（连接串模式整条直用）→ 起 runtime → 建客户端 → `ping` 一次（顺带取版本）。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let uri = match cfg.auth_mode {
            AuthMode::ConnectionString => cfg
                .connection_string
                .as_deref()
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .ok_or_else(|| DbError::Config("connection string is empty".into()))?
                .to_owned(),
            _ => build_uri(cfg, password)?,
        };
        let app_name = next_app_name(&cfg.id);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| DbError::Connect(format!("tokio runtime: {err}")))?;

        // `ClientOptions::parse` 也是个 action（`ParseConnectionString`），同样得转一次。
        // 解析失败归 `Config`：URI 拼错是**档案问题**，不是查询问题。
        let mut options = runtime
            .block_on(ClientOptions::parse(&uri).into_future())
            .map_err(map_options)?;
        options.app_name = Some(app_name.clone());
        options.connect_timeout = Some(Duration::from_secs(CONNECT_TIMEOUT_SECS));
        let client =
            Client::with_options(options).map_err(|err| DbError::Connect(err.to_string()))?;

        // `ping` 证明连得上、认证过得去（驱动是惰性连接，不探一下等于没连）。
        // `buildInfo` 只用来取版本号：**取不到不致命**（有些账号没有 clusterMonitor
        // 权限），所以它单独降级，不拖垮建连。
        let version = runtime.block_on(async {
            client
                .database(ADMIN_DB)
                .run_command(doc! { "ping": 1 })
                .await
                .map_err(map_mongo)?;
            Ok::<String, DbError>(
                match client
                    .database(ADMIN_DB)
                    .run_command(doc! { "buildInfo": 1 })
                    .await
                {
                    Ok(info) => info
                        .get_str("version")
                        .map_or_else(|_| "unknown".to_owned(), str::to_owned),
                    Err(_) => "unknown".to_owned(),
                },
            )
        })?;

        tracing::info!(host = %cfg.host, database = %cfg.database, "mongodb connected");
        Ok(Self {
            client,
            runtime,
            database: cfg.database.trim().to_owned(),
            // 库名可留空（见 `ConnectionConfig::validate`）；档案记住的命名空间是
            // 「上次浏览的库」，解析落点时排在 `database` 前面。
            namespace: cfg
                .schema
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            read_only: cfg.read_only,
            version,
            app_name,
            uri,
        })
    }

    /// 带外取消句柄（`killOp` 要另开客户端 + 自带 runtime，见 [`MongoCancelTarget`]）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::mongo(self.cancel_target())
    }

    /// 取消通道要的东西。
    fn cancel_target(&self) -> MongoCancelTarget {
        MongoCancelTarget {
            uri: self.uri.clone(),
            app_name: self.app_name.clone(),
        }
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.run(
            self.client
                .database(ADMIN_DB)
                .run_command(doc! { "ping": 1 }),
        )?;
        Ok(())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        Ok(format!("{} (MongoDB)", self.version))
    }

    /// 连接级只读：**只是记下来**（见模块文档——MongoDB 没有会话级只读开关）。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 记住界面上选中的库（**会话级「当前库」**）。
    ///
    /// MongoDB 的「当前库」是**客户端概念**（不像 SQL 引擎那样是服务端状态），所以
    /// 用户在下拉里换库之后必须告诉驱动一声，否则编辑器里的命令
    /// （`{"find": "users"}`）还会落在旧库、甚至落在「未选定」上。
    /// `None` / 空白 = 清掉落点，回到档案里配的那个。
    pub fn set_namespace(&mut self, schema: Option<&str>) {
        self.namespace = schema
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
    }

    /// 跑一条 JSON 命令。
    pub fn execute(&mut self, text: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let command = parse_command(text)?;
        // 命令（`{"find": "users"}`）必须落在某个库上；没选定库就给一句可操作的错误。
        let database = self.require_database(None, "this command")?;
        let reply = self.run_command(&database, command)?;
        Ok(reply_to_outcome(&reply, max_rows))
    }

    /// **切换会话的当前库** —— 与 [`Self::set_namespace`] 是同一件事。
    ///
    /// 单列一个方法只为跟其它引擎在 `Connection::switch_database` 里走同一个入口：
    /// Mongo 的「当前库」本来就是客户端状态，落点已经在这里了。
    pub fn use_database(&mut self, name: &str) {
        self.set_namespace(Some(name));
    }

    /// **全部**库（含 `admin` / `config` / `local`）。
    ///
    /// 与 [`Self::list_schemas`] 同源：Mongo 里「库」就是顶层容器，没有 schema 这一层。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        self.list_schemas()
    }

    /// 可选库清单。
    ///
    /// **不剔 `admin` / `config` / `local`**：它们是真实的库，`config` 里还有分片与
    /// 变更日志的元数据。口径与 ClickHouse 保留 `system` 一致——排障时挡住的正是
    /// 最常要看的东西。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let mut names = self.run(self.client.list_database_names())?;
        names.retain(|name| !name.trim().is_empty());
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// 一个库里的集合 / 视图。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        // 没选定库 → **空目录**（而不是报错）：连上没有默认库是合法状态，界面上还有
        // 命名空间下拉可以立刻选一个。报错会被调用方的 `unwrap_or_default` 吞掉，
        // 反而留下一条没人看的假失败。
        let Some(database) = self.resolve_database(schema)? else {
            return Ok(Vec::new());
        };
        let reply =
            self.run_command(&database, doc! { "listCollections": 1, "nameOnly": false })?;
        let mut tables = Vec::new();
        for entry in cursor_batch(&reply) {
            // `Document::get_str` 返回的是 `Result`（不是 `Option`）——取不到就是 Err。
            let Ok(name) = entry.get_str("name") else {
                continue;
            };
            // `type` 取值：`collection` / `view` / `timeseries`（4.2+）。
            let kind = match entry.get_str("type") {
                Ok("view") => TableKind::View,
                _ => TableKind::Table,
            };
            tables.push(CatalogTable {
                name: name.to_owned(),
                kind,
                schema: Some(database.clone()),
            });
        }
        tables.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(tables)
    }

    /// 集合的列：**样本字段并集**（文档库没有声明式 schema）。
    ///
    /// 空集合推不出任何列 → 返回空 `Vec`，界面据此显示「无样本」而不是编造列。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let docs = self.sample(table, SCHEMA_SAMPLE)?;
        Ok(columns_of_documents(&docs))
    }

    /// 一张集合的列级元数据：列只是**采样出来的字段名**，没有可空 / 默认值 / 注释
    /// 概念（文档库每条文档可以长不一样），五个属性槽位全空——调用方有啥画啥。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        Ok(self
            .table_columns(table)?
            .into_iter()
            .map(TableColumnMeta::from_column)
            .collect())
    }

    /// 一页文档。
    ///
    /// 稳定序：调用方给了 `order_by` 就用它，否则补 `_id` 升序——`_id` 上有唯一索引，
    /// 是文档库唯一天然稳定的排序键（`docs/database-design.md` §4.5 的翻页不重不漏）。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let collection = self.collection_for(table)?;
        let sort = sort_document(order_by);
        let filter = self.filter_document(table, filters)?;
        // 多取一条探测「还有下一页」，而不是靠「正好 limit 条」猜。
        let probe = i64::try_from(limit)
            .unwrap_or(i64::MAX - 1)
            .saturating_add(1);
        let cursor = self.run(collection.find(filter).sort(sort).skip(offset).limit(probe))?;
        let (mut docs, truncated) = self.drain_capped(cursor, limit)?;
        docs.truncate(limit);
        let mut set = documents_to_result_set(&docs);
        set.truncated = truncated;
        Ok(set)
    }

    /// 主键：**恒 `_id`**。
    ///
    /// 每个集合的 `_id` 上都有唯一索引（MongoDB 自己建的），它就是这一行的地址，
    /// 也是 `updateOne` / `deleteOne` 唯一可靠的定位方式。
    pub fn table_primary_key(&mut self, _table: &TableRef) -> Result<Vec<String>, DbError> {
        Ok(vec!["_id".to_owned()])
    }

    /// 改一格：`updateOne({_id: …}, {$set: {列: 值}})`。
    ///
    /// 返回的是 **`matched_count`** 而不是 `modified_count`：把「值本来就是这个」
    /// 也报成命中，否则界面会把「改成功但没变化」误报成「行没找到，请刷新」。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let collection = self.collection_for(table)?;
        check_name("field", column)?;
        let filter = pk_filter(pk)?;
        let bound = self.cell_to_bson(table, column, value)?;
        let update = doc! { "$set": { column: bound } };
        let result = self.run(collection.update_one(filter, update))?;
        Ok(result.matched_count)
    }

    /// 按 `_id` 删一行。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &PkValues) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let collection = self.collection_for(table)?;
        let filter = pk_filter(pk)?;
        let result = self.run(collection.delete_one(filter))?;
        Ok(result.deleted_count)
    }

    /// 删多行：合成**一条** `deleteMany({$or: […]})`。
    ///
    /// 一条命令而不是 N 条 `deleteOne`——Mongo 没有多文档事务（见模块文档），
    /// 那么「少的往返次数」就是能做到的最强保证。
    pub fn delete_row_set(&mut self, table: &TableRef, keys: &[PkValues]) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let collection = self.collection_for(table)?;
        let filters = keys.iter().map(pk_filter).collect::<Result<Vec<_>, _>>()?;
        let query = or_query(filters, "delete")?;
        let result = self.run(collection.delete_many(query))?;
        Ok(result.deleted_count)
    }

    /// 改多格：按 **（列, 新值）** 分组，每组发一条 `updateMany({$or: […]})`。
    ///
    /// 同一行改两列 = 两条命令（各自 `$set` 一个字段，服务端会合并），
    /// 同一列改同一值 = 一条命令。比「一格一条」的往返少一个量级。
    pub fn update_cells(&mut self, table: &TableRef, edits: &[CellEdit]) -> Result<u64, DbError> {
        self.ensure_writable()?;
        if edits.is_empty() {
            return Ok(0);
        }
        let collection = self.collection_for(table)?;
        let groups = self.group_edits(table, edits)?;
        let mut matched = 0_u64;
        for (column, value, filters) in groups {
            let query = or_query(filters, "update")?;
            let update = doc! { "$set": { column: value } };
            let result = self.run(collection.update_many(query, update))?;
            matched += result.matched_count;
        }
        Ok(matched)
    }

    /// 一次提交「改格 + 新增行」。
    ///
    /// ⚠️ **不是事务**（Mongo 的多文档事务只在副本集 / 分片集群上有）：改格走
    /// [`Self::update_cells`] 的分组命令，新增行走一条 `insertMany`；插入失败时
    /// 前面的改动已经生效，调用方按返回的影响行数核对刷新。
    pub fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[CellEdit],
        inserts: &[InsertRow],
    ) -> Result<BatchOutcome, DbError> {
        self.ensure_writable()?;
        let updated = if edits.is_empty() {
            0
        } else {
            self.update_cells(table, edits)?
        };
        let inserted = if inserts.is_empty() {
            0
        } else {
            self.insert_documents(table, inserts)?
        };
        Ok(BatchOutcome { updated, inserted })
    }

    /// 插入的核心：把 `InsertRow` 变成文档再 `insertMany`。
    ///
    /// 单元格按 `table_columns` 采到的类型做亲和转换，采样不到的列退回文本启发式
    /// （见 [`bind_text`]）。
    fn insert_documents(&self, table: &TableRef, rows: &[InsertRow]) -> Result<u64, DbError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let collection = self.collection_for(table)?;
        let types = self.column_types(table).unwrap_or_default();
        let mut documents = Vec::with_capacity(rows.len());
        for row in rows {
            if row.columns.is_empty() {
                return Err(DbError::Query("no columns to insert".into()));
            }
            if row.columns.len() != row.values.len() {
                return Err(DbError::Query(
                    "row width does not match column list".into(),
                ));
            }
            let mut document = Document::new();
            for (column, value) in row.columns.iter().zip(row.values.iter()) {
                check_name("field", column)?;
                let bound = bind_text(value.as_deref(), types.get(column.as_str()).copied())?;
                document.insert(column.clone(), bound);
            }
            documents.push(document);
        }
        let result = self.run(collection.insert_many(documents))?;
        Ok(result.inserted_ids.len() as u64)
    }

    /// 批量插入（**统一列清单**的入口：CSV 导入走这里）。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        if rows.is_empty() {
            return Ok(0);
        }
        if columns.is_empty() {
            return Err(DbError::Query("no columns to insert".into()));
        }
        let mut batch = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != columns.len() {
                return Err(DbError::Query(
                    "row width does not match column list".into(),
                ));
            }
            batch.push(InsertRow {
                columns: columns.to_vec(),
                values: row.clone(),
            });
        }
        self.insert_documents(table, &batch)
    }

    /// 集合的「DDL」：MongoDB 没有原文出口，这里由 `listCollections` 规格 + 索引
    /// **重构**一段等价描述（`db.createCollection` + `createIndex`）。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let database = self.require_database(table.schema.as_deref(), "this collection")?;
        check_name("collection", &table.name)?;
        let reply = self.run_command(
            &database,
            doc! { "listCollections": 1, "filter": { "name": table.name.as_str() } },
        )?;
        let spec = cursor_batch(&reply).into_iter().next();
        let indexes = self.table_indexes(table)?;

        // ⚠️ 这句注释**用英文**是有意的：内核不吐成品中文（`duck-db` 不参与 i18n，
        // 中文一律由界面层 `tr(...)` 给），而这段文本是**要粘进编辑器**的草稿。
        // 顺带说清它的性质：MongoDB 没有 DDL 原文，下面是重构的等价描述。
        let mut out = String::from(
            "// MongoDB has no DDL source text; the statements below are reconstructed from the collection spec and its indexes.\n",
        );
        let options = spec
            .as_ref()
            .and_then(|entry| entry.get_document("options").ok())
            .filter(|options| !options.is_empty());
        match options {
            Some(options) => {
                let pretty = pretty_json(&Bson::Document(options.clone()));
                out.push_str(&format!(
                    "db.createCollection({:?}, {pretty});\n",
                    table.name
                ));
            }
            None => out.push_str(&format!("db.createCollection({:?});\n", table.name)),
        }
        for index in &indexes {
            let keys = index
                .columns
                .iter()
                .fold(Document::new(), |mut acc, column| {
                    acc.insert(column.clone(), Bson::Int32(1));
                    acc
                });
            let mut opts = Document::new();
            opts.insert("name".to_owned(), Bson::String(index.name.clone()));
            if index.unique {
                opts.insert("unique".to_owned(), Bson::Boolean(true));
            }
            out.push_str(&format!(
                "db.{}.createIndex({}, {});\n",
                table.name,
                compact_json(&Bson::Document(keys)),
                compact_json(&Bson::Document(opts))
            ));
        }
        Ok(out)
    }

    /// 集合上的索引（右栏「索引」卡）。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let collection = self.collection_for(table)?;
        let cursor = self.run(collection.list_indexes())?;
        let models: Vec<IndexModel> = self.drain_any(cursor)?;
        let mut indexes = Vec::with_capacity(models.len());
        for model in models {
            let name = model
                .options
                .as_ref()
                .and_then(|options| options.name.clone())
                .unwrap_or_else(|| model.keys.keys().cloned().collect::<Vec<_>>().join("_"));
            let unique = model
                .options
                .as_ref()
                .and_then(|options| options.unique)
                .unwrap_or(false);
            indexes.push(TableIndex {
                primary: name == "_id_",
                name,
                columns: model.keys.keys().cloned().collect(),
                unique,
            });
        }
        Ok(indexes)
    }

    /// 文档数：走 **估算**（`estimated_document_count` 读集合元数据）。
    ///
    /// 不给 `Exact`：`countDocuments({})` 在千万级集合上是一次全扫，界面只是想显示
    /// 一个数字，不值得把后台线程占住。口径由 [`RowCount`] 带出去，界面自会写「约」。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let collection = self.collection_for(table)?;
        let count = self.run(collection.estimated_document_count())?;
        Ok(RowCount::Estimate(count))
    }

    // ---- 内部 ----

    /// 在自管 runtime 上跑一个驱动的 **action**，并把错误归一成 [`DbError`]。
    ///
    /// ⚠️ 收的是 action 而不是 future：`mongodb` 3.x 的 `Find` / `Update` /
    /// `Delete` / `RunCommand` / `ListIndexes` 这些结构体**只实现了 `IntoFuture`**
    /// （`Future` 实现落在它们 `into_future()` 产出的那个类型上），所以必须先转一次。
    /// 直接传 action 会报「`Find` is not a future」。
    fn run<A, T>(&self, action: A) -> Result<T, DbError>
    where
        A: IntoFuture<Output = mongodb::error::Result<T>>,
    {
        self.runtime
            .block_on(action.into_future())
            .map_err(map_mongo)
    }

    /// 拉干一个游标（不限行）。
    fn drain_any<T>(&self, mut cursor: Cursor<T>) -> Result<Vec<T>, DbError>
    where
        T: serde::de::DeserializeOwned + Send + Sync,
    {
        self.block_on(async move {
            let mut out = Vec::new();
            while cursor.advance().await.map_err(map_mongo)? {
                out.push(cursor.deserialize_current().map_err(map_mongo)?);
            }
            Ok(out)
        })
    }

    /// 拉一个游标，**最多 `cap` 条**；返回「是否还有更多」。
    ///
    /// 与 `drivers/clickhouse.rs` 同口径：截断在 Rust 侧做，并且**多读一条**来判断
    /// 「有更多」——只读满 `cap` 条就宣称截断，会把「正好 cap 条」误报成被截断。
    fn drain_capped(
        &self,
        mut cursor: Cursor<Document>,
        cap: usize,
    ) -> Result<(Vec<Document>, bool), DbError> {
        self.block_on(async move {
            let mut out = Vec::new();
            while cursor.advance().await.map_err(map_mongo)? {
                if out.len() >= cap {
                    return Ok((out, true));
                }
                out.push(cursor.deserialize_current().map_err(map_mongo)?);
            }
            Ok((out, false))
        })
    }

    /// 在 runtime 上 `block_on`（错误已在上游归一）。
    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// 解析库名，三级回落：**命令里点名 → 档案记住的命名空间 → 档案里的默认库**。
    ///
    /// 三级全空返回 `None`，即**没有选定库**——这在 MongoDB 上是**合法状态**（连得上、
    /// 也列得出所有库，只是命令没有落点）。库名本来就不必填，所以这里不能编一个出来：
    /// 调用方按「空目录」或「一句可操作的提示」处理。
    fn resolve_database(&self, schema: Option<&str>) -> Result<Option<String>, DbError> {
        let Some(name) = pick_database(schema, self.namespace.as_deref(), &self.database) else {
            return Ok(None);
        };
        check_name("database", name)?;
        Ok(Some(name.to_owned()))
    }

    /// 取库名；**没有选定库时给一句可操作的错误**（而不是拿空串去撞服务端）。
    fn require_database(&self, schema: Option<&str>, what: &str) -> Result<String, DbError> {
        self.resolve_database(schema)?.ok_or_else(|| {
            DbError::Config(format!(
                "no database selected for {what}: pick one in the namespace picker, \
                 or set a default database in the connection"
            ))
        })
    }

    /// 取一个集合句柄（库名 + 集合名都做了合法性检查）。
    fn collection_for(&self, table: &TableRef) -> Result<Collection<Document>, DbError> {
        let database = self.require_database(table.schema.as_deref(), "this collection")?;
        check_name("collection", &table.name)?;
        Ok(self
            .client
            .database(&database)
            .collection::<Document>(&table.name))
    }

    /// 跑一条命令，**只读档下先按首关键字做写拦截**。
    fn run_command(&self, database: &str, command: Document) -> Result<Document, DbError> {
        if self.read_only && command_is_write(&command) {
            return Err(read_only_error());
        }
        self.run(self.client.database(database).run_command(command))
    }

    /// 采集样本文档（列清单与类型亲和都靠它）。
    ///
    /// 逐条 `advance` 而不是把游标整条搬进 async：`advance` 才需要 await，
    /// `deserialize_current` 是**同步**的（返回 `Result` 而不是 future），借一下再克隆。
    fn sample(&self, table: &TableRef, size: i64) -> Result<Vec<Document>, DbError> {
        let collection = self.collection_for(table)?;
        let mut cursor = self.run(collection.find(doc! {}).limit(size))?;
        let mut out = Vec::new();
        while self.block_on(cursor.advance()).map_err(map_mongo)? {
            out.push(cursor.deserialize_current().map_err(map_mongo)?);
        }
        Ok(out)
    }

    /// 采样得到「列名 → BSON 类型别名」。
    fn column_types(&self, table: &TableRef) -> Result<HashMap<String, &'static str>, DbError> {
        let docs = self.sample(table, SCHEMA_SAMPLE)?;
        Ok(types_of_documents(&docs))
    }

    /// 把一格文本按**目标列的类型**绑成 BSON（类型未知时退回文本启发式）。
    fn cell_to_bson(
        &self,
        table: &TableRef,
        column: &str,
        value: Option<&str>,
    ) -> Result<Bson, DbError> {
        let types = self.column_types(table).unwrap_or_default();
        bind_text(value, types.get(column).copied())
    }

    /// 表浏览的过滤条件 → 查询文档。
    ///
    /// 每条条件各成一个文档，多条用 `$and` 串起来 —— **不用同一个文档的多键**：
    /// 同一列上两条条件（如 `age > 3` 且 `age < 9`）放进 `{age: …}` 里会互相覆盖。
    /// 值按目标列的**样本类型**绑成 BSON（与改一格同源，走 [`Self::cell_to_bson`]
    /// 用的那份 [`bind_text`]），`LIKE` 映射成 `$regex`（见 [`like_to_regex`]）。
    fn filter_document(
        &self,
        table: &TableRef,
        filters: &[TableFilter],
    ) -> Result<Document, DbError> {
        if filters.is_empty() {
            return Ok(Document::new());
        }
        let types = self.column_types(table).unwrap_or_default();
        let mut conditions = Vec::with_capacity(filters.len());
        for filter in filters {
            check_name("field", &filter.column)?;
            let type_name = types.get(&filter.column).copied();
            let value: Bson = match filter.op {
                FilterOp::IsNull => Bson::Null,
                FilterOp::IsNotNull => Bson::Document(doc! { "$ne": Bson::Null }),
                op => {
                    let bound = bind_text(filter.value.as_deref(), type_name)?;
                    match op {
                        FilterOp::Eq => bound,
                        FilterOp::NotEq => Bson::Document(doc! { "$ne": bound }),
                        FilterOp::Gt => Bson::Document(doc! { "$gt": bound }),
                        FilterOp::Gte => Bson::Document(doc! { "$gte": bound }),
                        FilterOp::Lt => Bson::Document(doc! { "$lt": bound }),
                        FilterOp::Lte => Bson::Document(doc! { "$lte": bound }),
                        FilterOp::Like => Bson::Document(
                            doc! { "$regex": like_to_regex(filter.value.as_deref().unwrap_or_default()) },
                        ),
                        FilterOp::IsNull | FilterOp::IsNotNull => unreachable!("matched above"),
                    }
                }
            };
            let mut single = Document::new();
            single.insert(filter.column.clone(), value);
            conditions.push(single);
        }
        if conditions.len() == 1 {
            Ok(conditions.pop().expect("length checked"))
        } else {
            Ok(doc! { "$and": Bson::Array(conditions.into_iter().map(Bson::Document).collect()) })
        }
    }

    /// 把多格编辑按 **（列, 新值）** 分组，值相同的行合并成一条 `$or` 查询。
    fn group_edits(
        &self,
        table: &TableRef,
        edits: &[CellEdit],
    ) -> Result<Vec<(String, Bson, Vec<Document>)>, DbError> {
        let types = self.column_types(table).unwrap_or_default();
        let mut groups: Vec<(String, Bson, Vec<Document>)> = Vec::new();
        for edit in edits {
            check_name("field", &edit.column)?;
            let filter = pk_filter(&edit.pk)?;
            let bound = bind_text(edit.value.as_deref(), types.get(&edit.column).copied())?;
            match groups
                .iter_mut()
                .find(|(column, value, _)| column == &edit.column && value == &bound)
            {
                Some((_, _, filters)) => filters.push(filter),
                None => groups.push((edit.column.clone(), bound, vec![filter])),
            }
        }
        Ok(groups)
    }

    /// 只读档下的写拦截（与 `drivers/mssql.rs` 的 `read_only_guard` 同款）。
    fn ensure_writable(&self) -> Result<(), DbError> {
        if self.read_only {
            return Err(read_only_error());
        }
        Ok(())
    }
}

// ---- 取消通道 ----

/// 取消一条正在跑的命令所需的全部内容。
///
/// MongoDB 的 `killOp` 要**点名 `opid`**，而 `opid` 只能从 `$currentOp` 里捞——
/// 所以这里带上 URI 与 `appName`：另开一条客户端，把「本进程这条连接在跑的操作」
/// 挑出来再杀。自带 runtime（取消是在**别的线程**上送的，不能蹭主连接那条）。
#[derive(Clone)]
pub(crate) struct MongoCancelTarget {
    /// 建连 URI（**含口令**；与连接本体同处内存，不落盘、不进日志）。
    uri: String,
    /// 本条连接独有的 `appName`。
    app_name: String,
}

impl MongoCancelTarget {
    /// 发一次 `killOp`；没有在跑的操作时是**空操作**（与 SQLite 的 interrupt 同语义）。
    pub(crate) fn kill(&self) -> Result<(), DbError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| DbError::Connect(format!("tokio runtime: {err}")))?;
        let mut options = runtime
            .block_on(ClientOptions::parse(&self.uri).into_future())
            .map_err(map_options)?;
        options.app_name = Some(self.app_name.clone());
        options.connect_timeout = Some(Duration::from_secs(CONNECT_TIMEOUT_SECS));
        let client =
            Client::with_options(options).map_err(|err| DbError::Connect(err.to_string()))?;

        let app_name = self.app_name.clone();
        runtime.block_on(async move {
            let admin = client.database(ADMIN_DB);
            // `$currentOp` 只列 `admin` 上的聚合阶段；按 appName 点名本连接的活跃操作。
            let reply = admin
                .run_command(doc! {
                    "aggregate": 1,
                    "pipeline": [
                        { "$currentOp": { "allUsers": true, "idleConnections": false } },
                        { "$match": { "appName": app_name.as_str(), "active": true } },
                        { "$project": { "opid": 1 } },
                        { "$limit": 1 },
                    ],
                    "cursor": {},
                })
                .await
                .map_err(map_mongo)?;
            let opid = cursor_batch(&reply)
                .into_iter()
                .find_map(|entry| bson_as_i64(entry.get("opid")));
            // 没有在跑的操作 → 空操作。这**不是**失败：取消本来就该幂等。
            let Some(opid) = opid else {
                return Ok(());
            };
            admin
                .run_command(doc! { "killOp": 1, "op": opid })
                .await
                .map_err(map_mongo)?;
            Ok(())
        })
    }
}

// ---- URI 与命名 ----

/// 建连 URI。
///
/// **用户名与口令走百分号编码**：一个含 `@` / `:` / `/` 的口令不编码就会把 URI
/// 解析成另一台主机（或另一个参数），这是实打实的注入面。
///
/// 不加 `authSource`：URI 路径上没有库名时，MongoDB 自己就把认证库定成 `admin`
/// ——显式写一遍只是把同一个行为说两遍，反而会误导「认证库可选」。
fn build_uri(cfg: &ConnectionConfig, password: Option<&str>) -> Result<String, DbError> {
    let host = cfg.host.trim();
    if host.is_empty() {
        return Err(DbError::Config("host is required".into()));
    }
    let port = cfg
        .port
        .unwrap_or_else(|| cfg.kind.default_port().expect("MongoDB has a default port"));
    // IPv6 字面量要带方括号，否则 `[::1]:27017` 拼出来是歧义的。
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };

    let mut uri = String::from("mongodb://");
    let user = cfg.user.trim();
    if !user.is_empty() {
        uri.push_str(&percent_encode(user));
        let password = password.unwrap_or_default();
        if !password.is_empty() {
            uri.push(':');
            uri.push_str(&percent_encode(password));
        }
        uri.push('@');
    }
    uri.push_str(&authority);
    uri.push_str(&format!(
        "/?connectTimeoutMS={}&serverSelectionTimeoutMS={}",
        CONNECT_TIMEOUT_SECS * 1_000,
        CONNECT_TIMEOUT_SECS * 1_000
    ));
    match cfg.tls {
        TlsMode::Disable => {}
        TlsMode::Trust => uri.push_str("&tls=true"),
        TlsMode::DangerAcceptInvalid => uri.push_str("&tls=true&tlsInsecure=true"),
    }
    Ok(uri)
}

/// unreserved 原样、其余百分号编码（与 `drivers/clickhouse.rs` 的 `url_encode` 同规则）。
fn percent_encode(value: &str) -> String {
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

/// 每条连接一个**进程内唯一**的 `appName`：取消时要靠它把「本条连接在跑的操作」
/// 从 `$currentOp` 里挑出来。
///
/// 带 `pid + 序号`是因为档案 id 会重复（导入连接、复制连接），而 appName 一旦撞了，
/// 取消就会打到别人的操作上。只保留 `[A-Za-z0-9_-]`——URI 查询串里最省心的字符集。
fn next_app_name(id: &str) -> String {
    let seq = APP_SEQ.fetch_add(1, Ordering::Relaxed);
    let sanitized: String = id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(48)
        .collect();
    format!("duck-{}-{seq}-{sanitized}", std::process::id())
}

/// 库名三级回落（**纯函数**，脱开连接单测）。
///
/// 顺序：**命令 / 表引用里点名 → 档案记住的命名空间 → 档案里的默认库**。空串与纯空白
/// 一律当「没给」，所以「填了空格」不会变成一个叫 `" "` 的库名。全空 → `None`。
fn pick_database<'a>(
    schema: Option<&'a str>,
    namespace: Option<&'a str>,
    configured: &'a str,
) -> Option<&'a str> {
    let clean = |value: &'a str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    schema
        .and_then(clean)
        .or_else(|| namespace.and_then(clean))
        .or_else(|| clean(configured))
}

/// 库名 / 集合名 / 字段名的合法性：MongoDB 只禁止**空名**与含 `\0` 的名。
///
/// 这里**不**逐段加引号（那是 SQL 的做法）：MongoDB 的对象名是**值**，不是要拼进
/// 语句文本的标识符——所以这条驱动压根没有字符串拼 SQL 的注入面。
fn check_name(what: &str, name: &str) -> Result<(), DbError> {
    if name.trim().is_empty() {
        return Err(DbError::Config(format!("{what} name is empty")));
    }
    if name.contains('\0') {
        return Err(DbError::Config(format!("{what} name contains NUL: {name}")));
    }
    Ok(())
}

// ---- 命令解析与只读判定 ----

/// 解析编辑器里的一段文本成命令文档。
///
/// 接受 JSON 文档；顺手吃掉结尾的分号（从 shell / 文档里粘过来常见）。
fn parse_command(text: &str) -> Result<Document, DbError> {
    let trimmed = text.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(DbError::Query("no command to execute".into()));
    }
    let json: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|err| DbError::Query(format!("command must be a JSON document: {err}")))?;
    match json_to_bson(json)? {
        Bson::Document(document) => Ok(document),
        _ => Err(DbError::Query(
            "command must be a JSON object, e.g. {\"find\": \"users\"}".into(),
        )),
    }
}

/// 这条命令**是不是写**？未知命令一律当写。
///
/// 方向不能反：把「读」误判成「写」只是只读档下多拒一条无害命令；把「写」误判成
/// 「读」就是只读档在生产库上真的写了。所以白名单 + 默认拒绝。
fn command_is_write(command: &Document) -> bool {
    let Some((key, _)) = command.iter().next() else {
        return true;
    };
    !READ_COMMANDS.contains(&key.to_ascii_lowercase().as_str())
}

/// 只读档下的统一错误文案。
fn read_only_error() -> DbError {
    DbError::Query(
        "connection is read-only (MongoDB has no server-side read-only session; \
         writes are refused by the driver)"
            .into(),
    )
}

// ---- BSON ⇄ 统一值模型 ----

/// BSON 值 → 统一值模型。
///
/// 口径：**可读优先，但可往返**。`ObjectId` / `DateTime` / `Decimal128` 都渲染成
/// 人眼友好的文本（而不是 `{"$oid": …}` 这种扩展 JSON），因为它们各自的文本形态
/// 都能被 [`bind_text`] 按列类型还原回去。
fn bson_to_value(value: &Bson) -> Value {
    match value {
        Bson::Double(number) => Value::Float(*number),
        Bson::String(text) => Value::Text(text.clone()),
        Bson::Array(_) | Bson::Document(_) => Value::Json(compact_json(value)),
        Bson::Boolean(flag) => Value::Bool(*flag),
        // `Undefined` 是废弃类型，现代服务端不会再写出它；按 NULL 展示。
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(number) => Value::Int(i64::from(*number)),
        Bson::Int64(number) => Value::Int(*number),
        Bson::Timestamp(stamp) => {
            Value::Text(format!("Timestamp({}, {})", stamp.time, stamp.increment))
        }
        Bson::Binary(binary) => Value::Bytes(binary.bytes.clone()),
        Bson::ObjectId(id) => Value::Text(id.to_hex()),
        Bson::DateTime(when) => Value::DateTime(format_datetime(*when)),
        Bson::Decimal128(number) => Value::Decimal(number.to_string()),
        Bson::RegularExpression(regex) => {
            Value::Text(format!("/{}/{}", regex.pattern, regex.options))
        }
        Bson::Symbol(text) | Bson::JavaScriptCode(text) => Value::Text(text.clone()),
        Bson::JavaScriptCodeWithScope(code) => Value::Text(code.code.clone()),
        // `DbPointer` 是废弃类型，且**两个字段都是 `pub(crate)`**（拿不到
        // `namespace` / `id`）—— 只能走 `Debug`。它本身已无人写入，不值得为它加依赖。
        Bson::DbPointer(pointer) => Value::Text(format!("{pointer:?}")),
        Bson::MinKey => Value::Text("MinKey".to_owned()),
        Bson::MaxKey => Value::Text("MaxKey".to_owned()),
    }
}

/// BSON 值 → 官方**类型别名**（`$type` 接受的那套字符串）。
///
/// 用它当 `Column.type_name`：界面拿它做展示，[`bind_text`] 拿它做写入时的类型亲和
/// ——两侧同一份词表，就不会出现「读了能显示、改了就存错」。
fn bson_type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// 把展示文本绑成 BSON：`None` → `Bson::Null`（契约：`None` 写 NULL，不是删字段）。
fn bind_text(value: Option<&str>, type_name: Option<&str>) -> Result<Bson, DbError> {
    let Some(text) = value else {
        return Ok(Bson::Null);
    };
    match type_name {
        None | Some("") => Ok(untyped_text_to_bson(text)),
        Some("string") => Ok(Bson::String(text.to_owned())),
        Some("null") => Ok(Bson::Null),
        Some("bool") => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(Bson::Boolean(true)),
            "false" | "0" => Ok(Bson::Boolean(false)),
            other => Err(DbError::Query(format!("not a boolean: {other}"))),
        },
        Some("int") => text
            .trim()
            .parse::<i32>()
            .map(Bson::Int32)
            .map_err(|err| DbError::Query(format!("not a 32-bit integer: {err}"))),
        Some("long") => text
            .trim()
            .parse::<i64>()
            .map(Bson::Int64)
            .map_err(|err| DbError::Query(format!("not a 64-bit integer: {err}"))),
        Some("double") => parse_double(text.trim())
            .map(Bson::Double)
            .ok_or_else(|| DbError::Query(format!("not a number: {text}"))),
        Some("decimal") => text
            .trim()
            .parse::<mongodb::bson::Decimal128>()
            .map(Bson::Decimal128)
            .map_err(|err| DbError::Query(format!("not a decimal128: {err}"))),
        Some("date") => parse_datetime(text.trim()),
        Some("objectId") => mongodb::bson::oid::ObjectId::parse_str(text.trim())
            .map(Bson::ObjectId)
            .map_err(|err| DbError::Query(format!("not an objectId: {err}"))),
        Some("object") | Some("array") => {
            let json: serde_json::Value = serde_json::from_str(text.trim()).map_err(|err| {
                DbError::Query(format!("cell of type {type_name:?} must be JSON: {err}"))
            })?;
            json_to_bson(json)
        }
        // 其余类型（binData / regex / timestamp / minKey …）没有可编辑的文本形态：
        // 老实报「不支持」，而不是悄悄写成一个字符串把数据弄坏。
        Some(other) => Err(DbError::Unsupported(format!("editing a {other} cell"))),
    }
}

/// 类型未知时的启发式：**先按扩展 JSON 解释，解释不了就当字符串**。
///
/// 这样 `123` → 整数、`"123"` → 字符串、`{"$oid": …}` → ObjectId，而 `abc` →
/// 字符串。用在 `_id` 定位与采样不到类型的列上。
fn untyped_text_to_bson(text: &str) -> Bson {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Bson::String(String::new());
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(json) => json_to_bson(json).unwrap_or_else(|_| Bson::String(text.to_owned())),
        Err(_) => Bson::String(text.to_owned()),
    }
}

/// 一行 `_id` 的展示文本 → 定位用的 BSON。
///
/// 与 [`untyped_text_to_bson`] 的差别只有一条前置规则：**24 位十六进制串按
/// `ObjectId` 解释**（`507f1f77bcf86cd799439011` 不是合法 JSON，否则会退成字符串，
/// 结果是「看得见却改不动」）。
fn parse_id_text(text: &str) -> Bson {
    let trimmed = text.trim();
    if is_object_id_hex(trimmed)
        && let Ok(id) = mongodb::bson::oid::ObjectId::parse_str(trimmed)
    {
        return Bson::ObjectId(id);
    }
    untyped_text_to_bson(text)
}

/// `LIKE` 模式 → 正则源串：`%` → `.*`、`_` → `.`，其余**正则元字符转义**
/// （用户的模式是 LIKE 模式，不是正则 —— 不转义的话 `a.b` 会意外匹配 `axb`）。
///
/// 首尾补 `^` / `$`：MongoDB 的 `$regex` 默认是「包含匹配」，而 SQL 的 `LIKE`
/// 要求整串匹配（通配符才放开对应位置）。
fn like_to_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    out.push('^');
    for ch in pattern.chars() {
        match ch {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            other => {
                if "^$.|?*+()[]{}".contains(other) {
                    out.push('\\');
                }
                out.push(other);
            }
        }
    }
    out.push('$');
    out
}

/// 24 位十六进制（ObjectId 的文本形态）。
fn is_object_id_hex(text: &str) -> bool {
    text.len() == 24 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 主键 → 定位文档。`_id` 走 [`parse_id_text`]，其余字段走无类型启发式。
fn pk_filter(pk: &PkValues) -> Result<Document, DbError> {
    let mut filter = Document::new();
    for (field, text) in pk {
        check_name("field", field)?;
        let bound = if field == "_id" {
            parse_id_text(text)
        } else {
            untyped_text_to_bson(text)
        };
        filter.insert(field.clone(), bound);
    }
    Ok(filter)
}

/// 把一组定位条件合成一条 `{ $or: […] }`（单条也要包——调用方不必分情况）。
fn or_query(filters: Vec<Document>, what: &str) -> Result<Document, DbError> {
    if filters.is_empty() {
        // `$or: []` 匹配不到任何行，会静默「改 0 行」；直接报错更诚实。
        return Err(DbError::Query(format!("no rows selected to {what}")));
    }
    if filters.len() == 1 {
        return Ok(filters.into_iter().next().expect("len == 1"));
    }
    Ok(doc! {
        "$or": filters.into_iter().map(Bson::Document).collect::<Vec<_>>(),
    })
}

/// 排序文档：调用方给了就用它，否则补 `_id` 升序（唯一天然稳定的排序键）。
fn sort_document(order_by: &[OrderBy]) -> Document {
    if order_by.is_empty() {
        return doc! { "_id": 1 };
    }
    let mut sort = Document::new();
    for entry in order_by {
        sort.insert(
            entry.column.clone(),
            Bson::Int32(if entry.descending { -1 } else { 1 }),
        );
    }
    sort
}

// ---- serde_json ⇄ BSON ----

/// JSON → BSON，认 MongoDB 的**扩展 JSON**（单键 `$xxx` 对象）。
///
/// 认这一套是因为它正是服务端与官方工具之间表达 `ObjectId` / `Long` / `Decimal128`
/// 的既定写法：用户在编辑器里写 `{"_id": {"$oid": "…"}}` 应当就是那个 ObjectId，
/// 而不是一个恰好叫 `$oid` 的嵌套对象。
fn json_to_bson(json: serde_json::Value) -> Result<Bson, DbError> {
    match json {
        serde_json::Value::Null => Ok(Bson::Null),
        serde_json::Value::Bool(flag) => Ok(Bson::Boolean(flag)),
        serde_json::Value::Number(number) => Ok(number_to_bson(&number)),
        serde_json::Value::String(text) => Ok(Bson::String(text)),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_bson(item)?);
            }
            Ok(Bson::Array(out))
        }
        serde_json::Value::Object(map) => {
            if map.len() == 1
                && let Some(entry) = map.iter().next()
                && let Some(extended) = extended_json_to_bson(entry.0, entry.1)?
            {
                return Ok(extended);
            }
            let mut document = Document::new();
            for (key, value) in map {
                document.insert(key, json_to_bson(value)?);
            }
            Ok(Bson::Document(document))
        }
    }
}

/// JSON 数字 → BSON：能落 `i32` 就 `Int32`，否则 `i64`，再不行才 `Double`。
fn number_to_bson(number: &serde_json::Number) -> Bson {
    if let Some(int) = number.as_i64() {
        return match i32::try_from(int) {
            Ok(narrow) => Bson::Int32(narrow),
            Err(_) => Bson::Int64(int),
        };
    }
    if let Some(uint) = number.as_u64() {
        return match i64::try_from(uint) {
            Ok(wide) => Bson::Int64(wide),
            Err(_) => Bson::Double(uint as f64),
        };
    }
    Bson::Double(number.as_f64().unwrap_or(f64::NAN))
}

/// 单键扩展 JSON → BSON；不是扩展 JSON 形态时返回 `None`（交给通用路径）。
///
/// 形态错（例如 `$oid` 不是 24 位十六进制）**报错而不是静默降级**：用户写错了要
/// 看得见，否则会写进一个结构错误的文档。
fn extended_json_to_bson(key: &str, value: &serde_json::Value) -> Result<Option<Bson>, DbError> {
    let text = || {
        value
            .as_str()
            .ok_or_else(|| DbError::Query(format!("{key} must be a string")))
    };
    let parsed = match key {
        "$oid" => Bson::ObjectId(
            mongodb::bson::oid::ObjectId::parse_str(text()?)
                .map_err(|err| DbError::Query(format!("bad $oid: {err}")))?,
        ),
        "$numberInt" => Bson::Int32(
            text()?
                .parse()
                .map_err(|err| DbError::Query(format!("bad $numberInt: {err}")))?,
        ),
        "$numberLong" => Bson::Int64(
            text()?
                .parse()
                .map_err(|err| DbError::Query(format!("bad $numberLong: {err}")))?,
        ),
        "$numberDouble" => Bson::Double(
            parse_double(text()?).ok_or_else(|| DbError::Query("bad $numberDouble".to_owned()))?,
        ),
        "$numberDecimal" => Bson::Decimal128(
            text()?
                .parse()
                .map_err(|err| DbError::Query(format!("bad $numberDecimal: {err}")))?,
        ),
        "$date" => {
            let raw = match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Object(map) => {
                    match map.get("$numberLong").and_then(|v| v.as_str()) {
                        Some(millis) => millis.to_owned(),
                        None => {
                            return Err(DbError::Query(
                                "$date must be an RFC3339 string or {\"$numberLong\": millis}"
                                    .into(),
                            ));
                        }
                    }
                }
                _ => {
                    return Err(DbError::Query(
                        "$date must be an RFC3339 string or {\"$numberLong\": millis}".into(),
                    ));
                }
            };
            parse_datetime(&raw)?
        }
        "$timestamp" => {
            let time = value
                .get("t")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| DbError::Query("$timestamp needs an integer 't'".into()))?;
            let increment = value
                .get("i")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| DbError::Query("$timestamp needs an integer 'i'".into()))?;
            Bson::Timestamp(mongodb::bson::Timestamp {
                time: u32::try_from(time)
                    .map_err(|_| DbError::Query("$timestamp 't' overflows u32".into()))?,
                increment: u32::try_from(increment)
                    .map_err(|_| DbError::Query("$timestamp 'i' overflows u32".into()))?,
            })
        }
        "$regularExpression" => {
            let pattern = value
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| DbError::Query("$regularExpression needs 'pattern'".into()))?;
            let options = value
                .get("options")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            Bson::RegularExpression(mongodb::bson::Regex {
                pattern: pattern.to_owned(),
                options: options.to_owned(),
            })
        }
        "$minKey" => Bson::MinKey,
        "$maxKey" => Bson::MaxKey,
        _ => return Ok(None),
    };
    Ok(Some(parsed))
}

/// BSON → **展示用** JSON（非扩展形态：ObjectId 出十六进制、日期出 RFC3339）。
///
/// 与 [`bson_to_value`] 同一口径——嵌套文档里也该是人眼能读的形态。
fn bson_to_json(value: &Bson) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Bson::Double(number) => serde_json::Number::from_f64(*number)
            .map_or_else(|| Json::String(format!("{number}")), Json::Number),
        Bson::String(text) => Json::String(text.clone()),
        Bson::Array(items) => Json::Array(items.iter().map(bson_to_json).collect()),
        Bson::Document(document) => Json::Object(
            document
                .iter()
                .map(|(key, value)| (key.clone(), bson_to_json(value)))
                .collect(),
        ),
        Bson::Boolean(flag) => Json::Bool(*flag),
        Bson::Null | Bson::Undefined => Json::Null,
        Bson::Int32(number) => Json::Number((*number).into()),
        Bson::Int64(number) => Json::Number((*number).into()),
        Bson::Timestamp(stamp) => {
            Json::String(format!("Timestamp({}, {})", stamp.time, stamp.increment))
        }
        Bson::Binary(binary) => Json::String(format!("0x{}", hex_encode(&binary.bytes))),
        Bson::ObjectId(id) => Json::String(id.to_hex()),
        Bson::DateTime(when) => Json::String(format_datetime(*when)),
        Bson::Decimal128(number) => Json::String(number.to_string()),
        Bson::RegularExpression(regex) => {
            Json::String(format!("/{}/{}", regex.pattern, regex.options))
        }
        Bson::Symbol(text) | Bson::JavaScriptCode(text) => Json::String(text.clone()),
        Bson::JavaScriptCodeWithScope(code) => Json::String(code.code.clone()),
        Bson::DbPointer(pointer) => Json::String(format!("{pointer:?}")),
        Bson::MinKey => Json::String("MinKey".to_owned()),
        Bson::MaxKey => Json::String("MaxKey".to_owned()),
    }
}

/// 紧凑 JSON 文本（人眼可读的兜底：序列化失败时退回 `null` 而不是 panic）。
fn compact_json(value: &Bson) -> String {
    serde_json::to_string(&bson_to_json(value)).unwrap_or_else(|_| "null".to_owned())
}

/// 缩进 JSON 文本（DDL 草稿用）。
fn pretty_json(value: &Bson) -> String {
    serde_json::to_string_pretty(&bson_to_json(value)).unwrap_or_else(|_| "null".to_owned())
}

/// 字节数组 → 十六进制（不引 hex crate：三行的事）。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 日期渲染：RFC3339（毫秒精度，`bson` 自己保证与 UTC 一致）。
fn format_datetime(when: mongodb::bson::DateTime) -> String {
    when.try_to_rfc3339_string()
        .unwrap_or_else(|_| format!("{}", when.timestamp_millis()))
}

/// 日期解析：RFC3339 优先，其次当作**毫秒时间戳**（整数）。
fn parse_datetime(text: &str) -> Result<Bson, DbError> {
    if let Ok(when) = mongodb::bson::DateTime::parse_rfc3339_str(text) {
        return Ok(Bson::DateTime(when));
    }
    if let Ok(millis) = text.parse::<i64>() {
        return Ok(Bson::DateTime(mongodb::bson::DateTime::from_millis(millis)));
    }
    Err(DbError::Query(format!(
        "not a date (RFC3339 or epoch millis): {text}"
    )))
}

/// 浮点解析：认 `NaN` / `Infinity` / `-Infinity`（`f64::from_str` 本就认这三档）。
fn parse_double(text: &str) -> Option<f64> {
    text.parse::<f64>().ok()
}

// ---- 文档 → 列与行 ----

/// 采样文档的**字段并集**（`_id` 提到最前，它是这一行的地址）。
///
/// 顺序 = 首次出现的顺序，而不是字典序：文档的字段顺序本身是有信息的
/// （用户建文档时怎么排的），字典序会把它抹平。
fn field_order(docs: &[Document]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for document in docs {
        for key in document.keys() {
            if seen.insert(key.as_str()) {
                names.push(key.clone());
            }
        }
    }
    if let Some(position) = names.iter().position(|name| name == "_id")
        && position != 0
    {
        let id = names.remove(position);
        names.insert(0, id);
    }
    names
}

/// 「字段 → BSON 类型别名」，取**第一个非空值**的类型（全空则不出现在表里）。
fn types_of_documents(docs: &[Document]) -> HashMap<String, &'static str> {
    let mut types = HashMap::new();
    for document in docs {
        for (key, value) in document {
            if types.contains_key(key) || matches!(value, Bson::Null | Bson::Undefined) {
                continue;
            }
            types.insert(key.clone(), bson_type_name(value));
        }
    }
    types
}

/// 列清单（名称 + 类型）。
fn columns_of_documents(docs: &[Document]) -> Vec<Column> {
    let types = types_of_documents(docs);
    field_order(docs)
        .into_iter()
        .map(|name| Column {
            type_name: types.get(&name).copied().unwrap_or_default().to_owned(),
            comment: String::new(),
            name,
        })
        .collect()
}

/// 文档 → 结果集：列是**整页字段的并集**，缺失的字段补 `NULL`。
///
/// 不能按第一条文档定列：文档库同一集合里字段本来就可以不一样，按第一行定列会让
/// 后面的字段**静默消失**。
fn documents_to_result_set(docs: &[Document]) -> ResultSet {
    let names = field_order(docs);
    let columns = names
        .iter()
        .map(|name| Column {
            name: name.clone(),
            type_name: String::new(),
            comment: String::new(),
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::with_capacity(docs.len());
    for document in docs {
        rows.push(
            names
                .iter()
                .map(|name| document.get(name).map_or(Value::Null, bson_to_value))
                .collect(),
        );
    }
    ResultSet {
        columns,
        rows,
        truncated: false,
    }
}

// ---- 命令应答 → 结果集 ----

/// 取应答文档里的游标批次（`cursor.firstBatch` / `nextBatch`）。
fn cursor_batch(reply: &Document) -> Vec<Document> {
    let Some(cursor) = reply.get_document("cursor").ok() else {
        return Vec::new();
    };
    let batch = cursor
        .get_array("firstBatch")
        .or_else(|_| cursor.get_array("nextBatch"))
        .ok();
    batch
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_document().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// 应答 → [`ExecOutcome`]：带游标的命令出结果集，其余出「一行应答表」+ 影响行数。
fn reply_to_outcome(reply: &Document, max_rows: usize) -> ExecOutcome {
    let affected = write_count(reply);
    let mut set = if reply.get_document("cursor").is_ok() {
        documents_to_result_set(&cursor_batch(reply))
    } else {
        // 非游标应答（`{ok: 1}`、`{n: 2, ok: 1}`、`{count: 12}`）：把应答本身当一行
        // 展示。用户要看的是「服务端回了什么」，原样给出比吞掉强。
        documents_to_result_set(std::slice::from_ref(reply))
    };
    if set.rows.len() > max_rows {
        set.rows.truncate(max_rows);
        set.truncated = true;
    }
    ExecOutcome {
        result: Some(set),
        affected,
        statements_run: 1,
    }
}

/// 应答里的影响行数：`n` 覆盖 insert / update / delete 三种（现代协议统一用它）；
/// 老形态的 `insertedIds` 兜一下。
fn write_count(reply: &Document) -> u64 {
    for key in ["n", "nModified", "deletedCount", "insertedCount"] {
        if let Some(count) = bson_as_i64(reply.get(key))
            && count > 0
        {
            return count.unsigned_abs();
        }
    }
    reply
        .get_document("insertedIds")
        .map_or(0, |ids| ids.len() as u64)
}

/// BSON → `i64`（整数族的宽取；`opid` 在不同服务端版本上可能是 `int` 或 `long`）。
fn bson_as_i64(value: Option<&Bson>) -> Option<i64> {
    match value? {
        Bson::Int32(number) => Some(i64::from(*number)),
        Bson::Int64(number) => Some(*number),
        Bson::Double(number) if number.is_finite() => Some(*number as i64),
        _ => None,
    }
}

// ---- 错误归一 ----

/// 归类结果：决定落到哪个 [`DbError`] 变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    /// 连不上 / 认证失败 / 选不到主 / TLS 失败。
    Connect,
    /// 被中断或 `maxTimeMS` 到点（与超时守护同义）。
    Cancelled,
    /// 引擎或服务端不支持该能力。
    Unsupported,
    /// 其余：语句或数据本身的问题。
    Query,
}

/// 归类决策（**纯函数**，脱开驱动的类型单测）。
///
/// ⚠️ 为什么要拆出来：`ErrorKind` 是 `#[non_exhaustive]`、`CommandError` 还带
/// `pub(crate)` 字段，**外部 crate 根本构造不出样本**——测试里造不出「被中断」这类
/// 错误。所以让 `map_mongo` 只负责从 `ErrorKind` 里取三个信号，决策本身收在这里，
/// 可以逐条断言。
fn classify(transport: bool, command_code: Option<i32>, unsupported: bool) -> ErrorClass {
    if unsupported {
        return ErrorClass::Unsupported;
    }
    // 11601 = Interrupted（`killOp` 之后服务端报的就是它）；50 = MaxTimeMSExpired。
    if command_code.is_some_and(|code| matches!(code, 11601 | 50)) {
        return ErrorClass::Cancelled;
    }
    if transport {
        ErrorClass::Connect
    } else {
        ErrorClass::Query
    }
}

/// 驱动错误 → [`DbError`]。
///
/// 归类口径与其它引擎一致：**连不上 / 认证失败 / 选不到主 → `Connect`**；
/// 被中断或 `maxTimeMS` 到点 → `Cancelled`（与超时守护同义）；其余 → `Query`。
fn map_mongo(error: mongodb::error::Error) -> DbError {
    use mongodb::error::ErrorKind;
    let text = error.to_string();
    let (transport, command_code, unsupported) = match error.kind.as_ref() {
        ErrorKind::Authentication { .. }
        | ErrorKind::ServerSelection { .. }
        | ErrorKind::DnsResolve { .. }
        | ErrorKind::Io(_)
        | ErrorKind::InvalidTlsConfig { .. }
        | ErrorKind::ConnectionPoolCleared { .. }
        | ErrorKind::IncompatibleServer { .. } => (true, None, false),
        ErrorKind::Command(command) => (false, Some(command.code), false),
        ErrorKind::SessionsNotSupported => (false, None, true),
        _ => (false, None, false),
    };
    match classify(transport, command_code, unsupported) {
        ErrorClass::Connect => DbError::Connect(text),
        ErrorClass::Cancelled => DbError::Cancelled(text),
        ErrorClass::Unsupported => DbError::Unsupported(text),
        ErrorClass::Query => DbError::Query(text),
    }
}

/// URI / 客户端选项解析失败：这是**档案写错了**，不是连不上。
fn map_options(error: mongodb::error::Error) -> DbError {
    DbError::Config(format!("invalid MongoDB URI: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseKind;

    fn config() -> ConnectionConfig {
        ConnectionConfig {
            kind: DatabaseKind::Mongo,
            host: "db.internal".into(),
            database: "shop".into(),
            ..ConnectionConfig::default()
        }
    }

    /// `LIKE` → `$regex`：通配符换成正则、正则元字符转义、首尾锚定。
    #[test]
    fn like_patterns_become_anchored_regexes() {
        assert_eq!(like_to_regex("ada"), "^ada$");
        assert_eq!(like_to_regex("a%"), "^a.*$");
        assert_eq!(like_to_regex("%a_"), "^.*a.$");
        // 正则元字符是**字面量**（用户写的是 LIKE 模式，不是正则）。
        assert_eq!(like_to_regex("a.b"), "^a\\.b$");
        assert_eq!(like_to_regex("(x)+"), "^\\(x\\)\\+$");
        assert!(!like_to_regex("100%").contains("\\"));
    }

    /// 库名三级回落：**命令点名 → 档案记住的命名空间 → 档案里的默认库**；全空就是
    /// 「没选定库」（库名本来就不必填，不能编一个出来）。
    #[test]
    fn database_falls_back_through_three_levels() {
        assert_eq!(
            pick_database(Some("cmd"), Some("picked"), "default"),
            Some("cmd")
        );
        assert_eq!(
            pick_database(None, Some("picked"), "default"),
            Some("picked")
        );
        assert_eq!(pick_database(None, None, "default"), Some("default"));
        assert_eq!(pick_database(None, None, ""), None);
        // 空白算「没给」，不该变成一个叫 `" "` 的库名。
        assert_eq!(pick_database(Some("  "), Some(" "), "  "), None);
    }

    /// 建连 URI **本来就不带库名**——这正是「库名可留空」在 MongoDB 上成立的原因。
    #[test]
    fn uri_never_carries_a_database_path() {
        let cfg = ConnectionConfig {
            database: "shop".into(),
            ..config()
        };
        let uri = build_uri(&cfg, Some("pw")).expect("拼 URI");
        assert!(!uri.contains("shop"), "库名不进 URI：{uri}");
        assert!(uri.contains("mongodb://"), "{uri}");
    }

    /// URI：口令里的 `@ : /` 必须被编码，否则会被解析成另一台主机。
    #[test]
    fn uri_percent_encodes_credentials() {
        let mut cfg = config();
        cfg.user = "duck".into();
        let uri = build_uri(&cfg, Some("p@ss:w/rd")).expect("uri");
        assert_eq!(
            uri,
            "mongodb://duck:p%40ss%3Aw%2Frd@db.internal:27017\
             /?connectTimeoutMS=20000&serverSelectionTimeoutMS=20000"
        );
        assert!(!uri.contains("p@ss:w/rd"), "明文口令不该出现在 URI 里");
    }

    /// URI：无口令时不写冒号；IPv6 字面量要加方括号；TLS 档位映射到 `tls=`。
    #[test]
    fn uri_handles_ipv6_and_tls() {
        let mut cfg = config();
        cfg.host = "::1".into();
        cfg.user = "duck".into();
        cfg.tls = TlsMode::Trust;
        let uri = build_uri(&cfg, None).expect("uri");
        assert!(uri.contains("@[::1]:27017/"), "{uri}");
        assert!(uri.ends_with("&tls=true"), "{uri}");

        cfg.tls = TlsMode::DangerAcceptInvalid;
        let insecure = build_uri(&cfg, None).expect("uri");
        assert!(
            insecure.ends_with("&tls=true&tlsInsecure=true"),
            "{insecure}"
        );

        cfg.tls = TlsMode::Disable;
        let plain = build_uri(&cfg, None).expect("uri");
        assert!(!plain.contains("tls"), "{plain}");
    }

    /// URI：缺主机直接拒绝（不要产出 `mongodb://:27017` 这种会被解析成 localhost 的串）。
    #[test]
    fn uri_requires_host() {
        let cfg = ConnectionConfig {
            kind: DatabaseKind::Mongo,
            host: "  ".into(),
            ..ConnectionConfig::default()
        };
        assert!(matches!(build_uri(&cfg, None), Err(DbError::Config(_))));
    }

    /// `appName` 进程内唯一，且只含 URI 查询串里省心的字符。
    #[test]
    fn app_names_are_unique_and_uri_safe() {
        let first = next_app_name("conn 1/2");
        let second = next_app_name("conn 1/2");
        assert_ne!(first, second);
        assert!(first.starts_with("duck-"), "{first}");
        assert!(
            first
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
            "{first}"
        );
    }

    /// BSON → 统一值模型：整数族、十进制、嵌套文档、二进制、ObjectId、日期。
    #[test]
    fn bson_cells_map_to_the_shared_value_model() {
        assert_eq!(bson_to_value(&Bson::Int32(7)), Value::Int(7));
        assert_eq!(bson_to_value(&Bson::Int64(-9)), Value::Int(-9));
        assert_eq!(bson_to_value(&Bson::Double(1.5)), Value::Float(1.5));
        assert_eq!(bson_to_value(&Bson::Boolean(true)), Value::Bool(true));
        assert_eq!(bson_to_value(&Bson::Null), Value::Null);
        assert_eq!(
            bson_to_value(&Bson::Decimal128("12.340".parse().expect("decimal"))),
            Value::Decimal("12.340".to_owned())
        );
        assert_eq!(
            bson_to_value(&Bson::Array(vec![Bson::Int32(1)])),
            Value::Json("[1]".to_owned())
        );
        assert_eq!(
            bson_to_value(&Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: vec![0xde, 0xad],
            })),
            Value::Bytes(vec![0xde, 0xad])
        );

        let id = mongodb::bson::oid::ObjectId::parse_str("507f1f77bcf86cd799439011").expect("oid");
        assert_eq!(
            bson_to_value(&Bson::ObjectId(id)),
            Value::Text("507f1f77bcf86cd799439011".to_owned())
        );

        let when =
            mongodb::bson::DateTime::parse_rfc3339_str("2026-10-12T09:30:00Z").expect("date");
        assert_eq!(
            bson_to_value(&Bson::DateTime(when)),
            Value::DateTime("2026-10-12T09:30:00Z".to_owned())
        );
    }

    /// 反向：一行 `_id` 的展示文本要能还原成 ObjectId，否则「看得见却改不动」。
    #[test]
    fn object_id_text_round_trips_through_the_primary_key() {
        let id = "507f1f77bcf86cd799439011";
        assert!(matches!(parse_id_text(id), Bson::ObjectId(_)));

        let filter = pk_filter(&vec![("_id".to_owned(), id.to_owned())]).expect("filter");
        assert_eq!(filter.get("_id"), Some(&parse_id_text(id)));
    }

    /// `_id` 也可能不是 ObjectId：数字、字符串、扩展 JSON 都要各归其类。
    #[test]
    fn primary_key_text_is_interpreted_by_shape() {
        assert_eq!(parse_id_text("42"), Bson::Int32(42));
        assert_eq!(parse_id_text(" 42 "), Bson::Int32(42));
        assert_eq!(parse_id_text("\"42\""), Bson::String("42".to_owned()));
        assert_eq!(parse_id_text("abc"), Bson::String("abc".to_owned()));
        assert_eq!(
            parse_id_text(r#"{"$numberLong": "9007199254740993"}"#),
            Bson::Int64(9_007_199_254_740_993)
        );
        // 23 位十六进制不是 ObjectId（长度不对），应当退成字符串。
        assert_eq!(
            parse_id_text("507f1f77bcf86cd79943901"),
            Bson::String("507f1f77bcf86cd79943901".to_owned())
        );
    }

    /// 类型亲和：已知列类型时按类型绑，未知时退回启发式（数字走数字、其余走文本）。
    #[test]
    fn typed_text_binds_by_column_type() {
        assert_eq!(bind_text(Some("42"), Some("int")), Ok(Bson::Int32(42)));
        assert_eq!(
            bind_text(Some("9007199254740993"), Some("long")),
            Ok(Bson::Int64(9_007_199_254_740_993))
        );
        assert_eq!(
            bind_text(Some("1.5"), Some("double")),
            Ok(Bson::Double(1.5))
        );
        assert_eq!(
            bind_text(Some("true"), Some("bool")),
            Ok(Bson::Boolean(true))
        );
        assert_eq!(
            bind_text(Some("42"), Some("string")),
            Ok(Bson::String("42".into()))
        );
        assert_eq!(bind_text(None, Some("string")), Ok(Bson::Null));
        assert_eq!(bind_text(Some("42"), None), Ok(Bson::Int32(42)));
        assert_eq!(bind_text(Some("abc"), None), Ok(Bson::String("abc".into())));
        assert_eq!(
            bind_text(Some(r#"{"a": 1}"#), Some("object")),
            Ok(Bson::Document(doc! { "a": 1 }))
        );
        assert_eq!(
            bind_text(Some("2026-10-12T09:30:00Z"), Some("date")),
            Ok(Bson::DateTime(
                mongodb::bson::DateTime::parse_rfc3339_str("2026-10-12T09:30:00Z").expect("date")
            ))
        );
    }

    /// 绑不上就报错，绝不悄悄写成字符串把数据搞坏。
    #[test]
    fn typed_text_rejects_bad_values() {
        assert!(bind_text(Some("abc"), Some("int")).is_err());
        assert!(bind_text(Some("maybe"), Some("bool")).is_err());
        assert!(bind_text(Some("not-a-date"), Some("date")).is_err());
        assert!(matches!(
            bind_text(Some("zz"), Some("binData")),
            Err(DbError::Unsupported(_))
        ));
    }

    /// 扩展 JSON：`$oid` / `$numberLong` / `$date` / `$minKey` 认，形态错则报错。
    #[test]
    fn extended_json_literals_map_to_bson_types() {
        let parse = |text: &str| json_to_bson(serde_json::from_str(text).expect("json"));

        assert!(matches!(
            parse(r#"{"$oid": "507f1f77bcf86cd799439011"}"#),
            Ok(Bson::ObjectId(_))
        ));
        assert_eq!(
            parse(r#"{"$numberLong": "9007199254740993"}"#),
            Ok(Bson::Int64(9_007_199_254_740_993))
        );
        assert_eq!(
            parse(r#"{"$numberDecimal": "12.340"}"#),
            Ok(Bson::Decimal128("12.340".parse().expect("decimal")))
        );
        assert_eq!(parse(r#"{"$minKey": 1}"#), Ok(Bson::MinKey));
        assert!(matches!(
            parse(r#"{"$date": "2026-10-12T09:30:00Z"}"#),
            Ok(Bson::DateTime(_))
        ));
        // `{"$date": {"$numberLong": "…"}}` 是官方毫秒形态。
        assert!(matches!(
            parse(r#"{"$date": {"$numberLong": "1760000000000"}}"#),
            Ok(Bson::DateTime(_))
        ));

        // 形态错 → 报错（不静默降级成一个叫 `$oid` 的嵌套对象）。
        assert!(parse(r#"{"$oid": "nope"}"#).is_err());
        assert!(parse(r#"{"$numberLong": "abc"}"#).is_err());
        assert!(parse(r#"{"$date": 5}"#).is_err());
    }

    /// 普通 JSON 的对象/数组/数字照常映射（不能被扩展 JSON 的分支吞掉）。
    #[test]
    fn plain_json_maps_without_extended_syntax() {
        assert_eq!(
            json_to_bson(serde_json::json!({"a": [1, true, null], "b": {"c": "x"}})),
            Ok(Bson::Document(doc! {
                "a": [1, true, null],
                "b": { "c": "x" },
            }))
        );
        // 两个键的对象即使键名以 `$` 开头也不是扩展 JSON 形态。
        assert_eq!(
            json_to_bson(serde_json::json!({"$oid": "x", "extra": 1})),
            Ok(Bson::Document(doc! { "$oid": "x", "extra": 1 }))
        );
    }

    /// 只读档的判定：白名单语义 + **未知命令一律当写**。
    #[test]
    fn unknown_commands_count_as_writes() {
        assert!(!command_is_write(&doc! { "find": "users" }));
        assert!(!command_is_write(
            &doc! { "aggregate": "users", "pipeline": [], "cursor": {} }
        ));
        assert!(!command_is_write(&doc! { "count": "users" }));
        assert!(!command_is_write(&doc! { "buildInfo": 1 }));

        assert!(command_is_write(
            &doc! { "insert": "users", "documents": [] }
        ));
        assert!(command_is_write(&doc! { "update": "users" }));
        assert!(command_is_write(&doc! { "delete": "users" }));
        assert!(command_is_write(&doc! { "drop": "users" }));
        assert!(command_is_write(&doc! { "createIndexes": "users" }));
        // 未知 / 空命令 → 当写（默认拒绝，方向不能反）。
        assert!(command_is_write(&doc! { "totallyNewCommand": 1 }));
        assert!(command_is_write(&Document::new()));
    }

    /// 只读档的错误文案与 mssql 同款（界面靠 `read-only` 这个词做提示）。
    #[test]
    fn read_only_error_is_recognisable() {
        assert!(matches!(
            read_only_error(),
            DbError::Query(ref detail) if detail.contains("read-only")
        ));
    }

    /// 命令解析：必须是一个 JSON 对象；结尾分号吃掉；空文本拒绝。
    #[test]
    fn command_text_must_be_a_json_object() {
        assert_eq!(
            parse_command(r#"{"find": "users"}"#).expect("doc"),
            doc! { "find": "users" }
        );
        assert_eq!(
            parse_command(r#"  {"count": "users"};  "#).expect("doc"),
            doc! { "count": "users" }
        );
        assert!(matches!(parse_command("[1, 2]"), Err(DbError::Query(_))));
        assert!(matches!(parse_command("   "), Err(DbError::Query(_))));
        assert!(matches!(
            parse_command("find users"),
            Err(DbError::Query(_))
        ));
    }

    /// 列清单：字段并集、`_id` 提到最前、缺失字段补 NULL、不全空才给类型。
    #[test]
    fn documents_flatten_with_union_columns_and_leading_id() {
        let docs = vec![
            doc! { "name": "a", "_id": 1, "extra": Bson::Null },
            doc! { "_id": 2, "age": 30 },
        ];
        let columns = columns_of_documents(&docs);
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "name", "extra", "age"]);
        assert_eq!(columns[0].type_name, "int");
        assert_eq!(columns[1].type_name, "string");
        // `extra` 只有 NULL 样本 → 推不出类型（空串），界面不会瞎标。
        assert_eq!(columns[2].type_name, "");
        assert_eq!(columns[3].type_name, "int");

        let set = documents_to_result_set(&docs);
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.rows[0][2], Value::Null);
        // 第二行没有 `name` → 补 NULL（而不是错位）。
        assert_eq!(set.rows[1][1], Value::Null);
        assert_eq!(set.rows[1][3], Value::Int(30));
    }

    /// 空集合：推不出任何列（文档库无 schema，不能编造）。
    #[test]
    fn empty_collection_yields_no_columns() {
        assert!(columns_of_documents(&[]).is_empty());
        assert!(field_order(&[]).is_empty());
    }

    /// 应答：游标批次出多行；非游标应答出「一行应答表」+ 影响行数；上限处截断。
    #[test]
    fn replies_become_rows_and_write_counts() {
        let cursor_reply = doc! {
            "cursor": { "firstBatch": [ { "_id": 1 }, { "_id": 2 } ], "id": 0_i64, "ns": "shop.users" },
            "ok": 1.0_f64,
        };
        let outcome = reply_to_outcome(&cursor_reply, 100);
        let set = outcome.result.expect("result");
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.rows[0][0], Value::Int(1));
        assert!(!set.truncated);
        assert_eq!(outcome.affected, 0);

        // 写应答：一行应答表 + `n` 计入影响行数。
        let write_reply = doc! { "n": 3_i32, "ok": 1.0_f64 };
        let outcome = reply_to_outcome(&write_reply, 100);
        assert_eq!(outcome.affected, 3);
        let set = outcome.result.expect("result");
        assert_eq!(set.rows.len(), 1);
        assert_eq!(set.columns.len(), 2);

        // 上限截断：读满 max_rows 且还有更多 → 标 truncated。
        let many = doc! {
            "cursor": { "firstBatch": [ { "_id": 1 }, { "_id": 2 }, { "_id": 3 } ], "id": 0_i64 },
        };
        let outcome = reply_to_outcome(&many, 2);
        let set = outcome.result.expect("result");
        assert_eq!(set.rows.len(), 2);
        assert!(set.truncated);
    }

    /// `insertedIds` 形态的老应答也能数出行数。
    #[test]
    fn insertion_counts_fall_back_to_inserted_ids() {
        let reply = doc! { "insertedIds": { "0": 1_i32, "1": 2_i32 }, "ok": 1.0_f64 };
        assert_eq!(write_count(&reply), 2);
        assert_eq!(write_count(&doc! { "ok": 1.0_f64 }), 0);
    }

    /// 排序文档：空 → `_id` 升序（唯一天然稳定的键）；给定 → 逐列按方向。
    #[test]
    fn sort_defaults_to_underscore_id() {
        assert_eq!(sort_document(&[]), doc! { "_id": 1 });
        assert_eq!(
            sort_document(&[OrderBy::desc("age"), OrderBy::asc("name")]),
            doc! { "age": -1, "name": 1 }
        );
    }

    /// `$or` 合成：空拒绝（静默改 0 行最坑人）、单条不包 `$or`、多条才包。
    #[test]
    fn or_query_rejects_empty_and_unwraps_single() {
        assert!(matches!(
            or_query(Vec::new(), "delete"),
            Err(DbError::Query(_))
        ));

        let single = or_query(vec![doc! { "_id": 1 }], "delete").expect("query");
        assert_eq!(single, doc! { "_id": 1 });

        let many = or_query(vec![doc! { "_id": 1 }, doc! { "_id": 2 }], "update").expect("query");
        assert_eq!(many, doc! { "$or": [ { "_id": 1 }, { "_id": 2 } ] });
    }

    /// 名字检查：空名与含 `\0` 的名拒绝——Mongo 的对象名不做 SQL 式引号包裹。
    #[test]
    fn names_are_validated_without_sql_quoting() {
        assert!(check_name("collection", "users").is_ok());
        assert!(check_name("collection", "we;ird name").is_ok());
        assert!(check_name("collection", "  ").is_err());
        assert!(check_name("collection", "we\0ird").is_err());
    }

    /// 错误归类：连不上 / 认证 → `Connect`；中断 → `Cancelled`；不支持 → `Unsupported`。
    ///
    /// 测的是决策本身（`classify`）而不是 `map_mongo`：`ErrorKind` 是
    /// `#[non_exhaustive]`、`CommandError` 还带 `pub(crate)` 字段，外部 crate
    /// **构造不出样本**，硬测只能靠 `Error::custom` 走 `_` 分支，等于没测。
    #[test]
    fn driver_errors_classify_by_kind() {
        assert_eq!(classify(true, None, false), ErrorClass::Connect);
        // 中断（`killOp` 后服务端回 11601）与 `maxTimeMS` 到点（50）都算取消。
        assert_eq!(classify(false, Some(11601), false), ErrorClass::Cancelled);
        assert_eq!(classify(false, Some(50), false), ErrorClass::Cancelled);
        // 普通命令错误码是查询问题。
        assert_eq!(classify(false, Some(2), false), ErrorClass::Query);
        assert_eq!(classify(false, None, false), ErrorClass::Query);
        // 不支持优先于其余判定（副本集/会话类能力缺失不该被报成查询错）。
        assert_eq!(classify(true, Some(50), true), ErrorClass::Unsupported);
    }
}
