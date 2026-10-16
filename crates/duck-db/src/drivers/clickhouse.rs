//! ClickHouse 驱动：**HTTP 接口**直连（`ureq`，零新增依赖）。
//!
//! # 为什么走 HTTP 而不是原生协议
//!
//! `docs/database-design.md` §5 的选型就是 `ureq` 直连 REST。两条理由：
//! ① `clickhouse` crate 的最新**可用**版本停在 0.13.3（0.14/0.15 起 MSRV 顶到 1.89，
//! 顶破本仓的 1.85 硬门），落后两个大版本；② HTTP 接口（缺省 8123）本就是
//! ClickHouse 对外推荐的一等入口，`ureq` 已在依赖图内（更新器 / 插件出网 / AI 厂商），
//! 所以这一条引擎是**零新增依赖**。
//!
//! # 会话是无状态的，所以状态必须逐请求带上
//!
//! HTTP 每次请求都是一个新会话，「建连时 `SET` 一次」在这里是无效的。因此：
//! - **只读档**：每个请求带 URL 参数 `readonly=1`（服务端自己拒写）；
//! - **参数化**：URL 参数 `param_x=v` 配 SQL 里的 `{x:String}` 占位（官方方式，
//!   所以这条驱动**没有字符串拼接出来的注入面**）；
//! - **取消**：请求带 `query_id`，取消时另发 `KILL QUERY WHERE query_id = '…'`。
//!
//! # 结果集用 `JSONCompact`
//!
//! `default_format=JSONCompact` 一次给出 `meta`（列名 + ClickHouse 类型名）与
//! `data`，不需要自己解析 TSV 的转义规则。
//!
//! ⚠️ **行数截断在 Rust 侧做**，不靠 URL 的 `max_result_rows`：后者是**块级**
//! `overflow_mode=break`，返回的行数落在块边界上而不是恰好 N 行，拿它做「还有更多」
//! 的探测会时灵时不灵。

use std::io::Read as _;
use std::sync::{Arc, Mutex};

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, TlsMode};
use crate::drivers::{
    CatalogTable, FilterOp, OrderBy, PkValues, RowCount, TableColumnMeta, TableFilter, TableIndex,
    TableKind, TableRef, filter_conditions, order_by_clause, quote_identifier, quote_qualified,
};
// URL 成分编码的实现住在 `drivers/mod.rs`：Elasticsearch 的路径段要用同一份，
// 编码规则不能有两处口径。
use super::url_encode;
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};
use crate::sqlintel::{StmtKind, classify, prove_read_only, split_statements};

/// 连接兜底超时：真正的「停」由 `CancelTimer` → `KILL QUERY` 负责，这里只是
/// 不让一条请求把后台线程永久勾住（3600 秒与 `duck-app` 的 `MAX_TIMEOUT_SECS` 同口径）。
const HARD_TIMEOUT_SECS: u64 = 3600;

/// TCP 连接超时（比兜底短得多：连不上要快速失败，而不是等一小时）。
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// 库名留空时用的惯例库。
///
/// 请求里不带 `database` 参数时，服务端的 `currentDatabase()` 本来就是 `default`；
/// 这里显式对齐，否则 `WHERE database = ''` 一条都查不出来。
const DEFAULT_DATABASE: &str = "default";

/// 库名回落（**纯函数**，脱开连接单测）：调用方给的 → 档案里配的 → [`DEFAULT_DATABASE`]。
///
/// 空串与纯空白一律当「没给」（「填了空格」不该变成一个叫 `" "` 的库名）。
fn pick_database<'a>(schema: Option<&'a str>, configured: &'a str) -> &'a str {
    let clean = |value: &'a str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    schema
        .and_then(clean)
        .or_else(|| clean(configured))
        .unwrap_or(DEFAULT_DATABASE)
}

/// 列类型是不是 ClickHouse 的**数值族**（`UInt*` / `Int*` / `Float*` / `Decimal*` / `Bool`）。
///
/// **纯函数**，脱开连接单测。先剥掉 `Nullable(...)` / `LowCardinality(...)` 包装
/// （可叠加，如 `Nullable(LowCardinality(Int64))`），再判前缀。
///
/// 刻意**不**把 `Date` / `DateTime` / `DateTime64` 算进来：它们的 `>` 走字符串序才是
/// 对的（ISO 串的字典序就是时间序），而且 `toFloat64OrNull` 未必接受它们。
/// `String` / `FixedString` / `Enum*` / `UUID` / `IPv4` / `Array(...)` 同理不算。
fn is_numeric_type(type_name: &str) -> bool {
    let mut inner = type_name.trim();
    loop {
        let stripped = ["Nullable(", "LowCardinality("].iter().find_map(|prefix| {
            inner
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
        });
        match stripped {
            Some(rest) => inner = rest.trim(),
            None => break,
        }
    }
    inner.starts_with("UInt")
        || inner.starts_with("Int")
        || inner.starts_with("Float")
        || inner.starts_with("Decimal")
        || inner == "Bool"
}

/// 该档过滤是否应走**数值序**比较：仅「顺序算子**且**列是数值族」。
///
/// **纯函数**，且是这两个判定的**单一真源**：`table_page` 的 `column_expr` 与
/// `value_expr` 两个闭包都调它。两边的判定一旦不一致，就会生成「列转 Float64、
/// 值还是 String」这种必然报类型错的 SQL，所以不允许各写一份。
fn numeric_ordering(types: &[Column], column: &str, op: FilterOp) -> bool {
    matches!(
        op,
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte
    ) && types
        .iter()
        .find(|candidate| candidate.name.as_str() == column)
        .is_some_and(|candidate| is_numeric_type(&candidate.type_name))
}

/// 发一条请求所需的全部内容。
///
/// 单独成型是因为**取消通道还要拿一份**（`KILL QUERY` 必须另发一条请求，而且
/// 不能占着主连接——与 MySQL 的 `opts` 同一个道理）。口令留在内存，不落盘、不进日志。
///
/// `pub(crate)` 而不是 `pub`：它是实现细节（`ureq::Agent` + 认证头），
/// 不该出现在 `duck-db` 的公开 API 面上。
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// `http(s)://host:port`，不含路径。
    base: String,
    /// 请求缺省落到哪个库（URL 参数 `database`）。
    database: String,
    user: String,
    password: String,
    agent: ureq::Agent,
}

/// 一条响应：正文 + 服务端自报的影响行数。
///
/// `pub(crate)` 是给取消通道用的：它只把响应丢掉（`KILL QUERY` 没有结果要看），
/// 但 `post` 的返回类型必须对 `pub(crate)` 的调用方可见。
pub(crate) struct Reply {
    body: String,
    written_rows: u64,
}

impl Endpoint {
    /// 发一条语句。`params` 是 `param_<名>=<值>`（配 SQL 里的 `{名:String}`）。
    pub(crate) fn post(
        &self,
        sql: &str,
        query_id: Option<&str>,
        read_only: bool,
        params: &[(&str, &str)],
    ) -> Result<Reply, DbError> {
        let mut url = String::with_capacity(self.base.len() + 96);
        url.push_str(&self.base);
        url.push_str("/?default_format=JSONCompact");
        if !self.database.is_empty() {
            url.push_str("&database=");
            url.push_str(&url_encode(&self.database));
        }
        if read_only {
            url.push_str("&readonly=1");
        }
        if let Some(id) = query_id {
            url.push_str("&query_id=");
            url.push_str(&url_encode(id));
        }
        for (key, value) in params {
            url.push_str("&param_");
            url.push_str(key);
            url.push('=');
            url.push_str(&url_encode(value));
        }

        let mut response = self
            .agent
            .post(&url)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .send(sql)
            .map_err(map_transport)?;

        let status = response.status();
        let written_rows = summary_written_rows(&response);
        // 不用 `Body::read_to_string()`：它自带上限，大结果集会被截成半截 JSON
        // 而报出一个与真实原因无关的解析错误。走 reader 自己读，长度由我们兜。
        let mut body = String::new();
        response
            .body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .map_err(|error| DbError::Query(format!("response read failed: {error}")))?;

        if !status.is_success() {
            // ClickHouse 把 `Code: N. DB::Exception: …` 放在正文里，原样带给用户。
            return Err(DbError::Query(error_text(&body, status.as_u16())));
        }
        Ok(Reply { body, written_rows })
    }
}

/// `X-ClickHouse-Summary` 里的 `written_rows`（分片写不是 0；读语句是 0）。
fn summary_written_rows<B>(response: &ureq::http::Response<B>) -> u64 {
    let Some(raw) = response
        .headers()
        .get("X-ClickHouse-Summary")
        .and_then(|value| value.to_str().ok())
    else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            let field = value.get("written_rows")?;
            // 该字段是**字符串**（ClickHouse 用字符串传 64 位整数）。
            match field {
                serde_json::Value::String(text) => text.parse().ok(),
                serde_json::Value::Number(number) => number.as_u64(),
                _ => None,
            }
        })
        .unwrap_or(0)
}

/// 错误正文归一：空正文兜一个 `HTTP <码>`，超长截断（别把整段栈灌进界面）。
fn error_text(body: &str, status: u16) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("HTTP {status}");
    }
    trimmed.chars().take(600).collect()
}

/// 传输层错误归类：连不上归 `Connect`，超时归 `Cancelled`（与「被中止」同义），其余归 `Query`。
fn map_transport(error: ureq::Error) -> DbError {
    let timeout = matches!(error, ureq::Error::Timeout(_));
    let unreachable = matches!(
        error,
        ureq::Error::HostNotFound
            | ureq::Error::Io(_)
            | ureq::Error::BadUri(_)
            | ureq::Error::InvalidProxyUrl
    );
    let text = error.to_string();
    if timeout {
        DbError::Cancelled(text)
    } else if unreachable {
        DbError::Connect(text)
    } else {
        DbError::Query(text)
    }
}

/// 一条 ClickHouse 连接。
pub struct ClickHouseConnection {
    endpoint: Endpoint,
    /// 只读档：每个请求带 `readonly=1`。**不是**界面上的软拦截——服务端自己拒写。
    read_only: bool,
    /// 正在跑的语句的 `query_id`；取消通道靠它寻址。空闲时是 `None`。
    inflight: Arc<Mutex<Option<String>>>,
    /// `SELECT version()` 的结果（连接时取一次，`server_version()` 直接复用）。
    version: String,
}

impl ClickHouseConnection {
    /// 建连：拼端点 + 立刻验证一次（HTTP 没有握手，「能连上」必须自己证）。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let endpoint = build_endpoint(cfg, password)?;
        // 用 `SELECT version()` 一并把版本拿到手：既证明连得上、认证过得去，
        // 又省掉一次往返。
        let reply = endpoint.post("SELECT version()", None, false, &[])?;
        let version = first_cell_text(&reply.body).unwrap_or_else(|| "unknown".to_owned());
        tracing::info!(host = %cfg.host, database = %cfg.database, "clickhouse connected");
        Ok(Self {
            endpoint,
            read_only: cfg.read_only,
            inflight: Arc::new(Mutex::new(None)),
            version,
        })
    }

    /// 带外取消句柄（`KILL QUERY WHERE query_id = '…'`；另发一条请求，不占主连接）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::clickhouse(self.endpoint.clone(), self.inflight.clone())
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.endpoint.post("SELECT 1", None, self.read_only, &[])?;
        Ok(())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        Ok(format!("{} (ClickHouse)", self.version))
    }

    /// 连接级只读：记下来，之后每个请求都带 `readonly=1`。
    ///
    /// ⚠️ HTTP 无会话，「设置」不是一个动作而是一个**每请求都带上**的状态。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 执行一段 SQL（多条语句逐条跑），读语句在 `max_rows` 处截断。
    pub fn execute(&mut self, sql: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let statements = split_statements(sql);
        if statements.is_empty() {
            return Err(DbError::Query("no statement to execute".into()));
        }
        let mut outcome = ExecOutcome::default();
        for stmt in statements {
            let read = prove_read_only(&stmt) || classify(&stmt) == StmtKind::Read;
            if read {
                let reply = self.send_with_id(&stmt)?;
                outcome.result = Some(result_set(&reply.body, max_rows)?);
            } else {
                let reply = self.send_with_id(&stmt)?;
                // 写路径没有结果集；`written_rows` 只在 INSERT 一类语句上有值。
                outcome.affected += reply.written_rows;
            }
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 分页读取（结果区「加载更多」走这里）：把语句包一层 `LIMIT/OFFSET`，
    /// 从第 `offset` 行起取 `limit` 行；多取一行探测「还有下一页」。
    ///
    /// 语义与边界见 [`crate::drivers::Connection::execute_paged`]。
    pub fn execute_paged(
        &mut self,
        sql: &str,
        offset: u64,
        limit: usize,
    ) -> Result<ResultSet, DbError> {
        let inner = super::paged_inner_sql(sql)?;
        let wrapped = super::wrap_paged_sql(&inner, offset, limit, '`')?;
        self.query_one_with(&wrapped, &[], limit)
    }

    /// 跑一条语句，同时登记 `query_id` 供取消寻址；返回后立刻撤销登记。
    fn send_with_id(&self, sql: &str) -> Result<Reply, DbError> {
        self.send_with_id_params(sql, &[])
    }

    /// 同 [`Self::send_with_id`]，但带 `param_<名>=<值>`（配 SQL 里的 `{名:String}`）。
    fn send_with_id_params(&self, sql: &str, params: &[(&str, &str)]) -> Result<Reply, DbError> {
        let query_id = next_query_id();
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = Some(query_id.clone());
        }
        let reply = self
            .endpoint
            .post(sql, Some(&query_id), self.read_only, params);
        // 无论成败都要撤销：留着一个已结束的 id 会让下一次取消打到空气上。
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = None;
        }
        reply
    }

    /// 跑一条**带参数**的读语句（表浏览过滤用；值是 URL 上的 `param_<名>`）。
    fn query_one_with(
        &self,
        stmt: &str,
        params: &[(&str, &str)],
        max_rows: usize,
    ) -> Result<ResultSet, DbError> {
        let reply = self.send_with_id_params(stmt, params)?;
        result_set(&reply.body, max_rows)
    }

    /// **切换会话的当前库**。
    ///
    /// ClickHouse 走 HTTP，没有「会话」可切 —— 「当前库」是**每个请求**上的
    /// `database` URL 参数（见 `Endpoint::database`）。所以切库就是改端点上的
    /// 这个字段：此后所有请求（含用户手写的 SQL 与内省语句）都落在新库上。
    /// 这就等价于原生命令行里 `USE <db>` 的效果。
    pub fn use_database(&mut self, name: &str) {
        self.endpoint.database = name.to_owned();
    }

    /// **全部**库（含 `INFORMATION_SCHEMA`）。
    ///
    /// 与 [`Self::list_schemas`] 的差别只在 `INFORMATION_SCHEMA`：浏览对象时它
    /// 与 `system` 重复、剔掉更干净；但「这条连接上有哪些库」这个问题应当如实回答。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        self.strings("SELECT name FROM system.databases ORDER BY name", &[])
    }

    /// 可选库清单。
    ///
    /// 剔 `INFORMATION_SCHEMA`（它与 `system` 内容重复），但**保留 `system`**：
    /// ClickHouse 的 `system.query_log` / `system.parts` / `system.replicas` 是运维
    /// 排障的一等公民，剔掉反而挡住本引擎最常用的真实用途。
    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        let names = self.strings(
            "SELECT name FROM system.databases \
             WHERE name NOT IN ('INFORMATION_SCHEMA', 'information_schema') \
             ORDER BY name",
            &[],
        )?;
        Ok(names)
    }

    /// 一个库里的表 / 视图（ClickHouse 没有用户级序列）。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let database = self.resolve_schema(schema);
        let reply = self.endpoint.post(
            "SELECT name, engine FROM system.tables WHERE database = {db:String} ORDER BY name",
            None,
            self.read_only,
            &[("db", database.as_str())],
        )?;
        let parsed = parse_compact(&reply.body)?;
        let mut tables = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            let Some(name) = row.first().and_then(json_text) else {
                continue;
            };
            let engine = row.get(1).and_then(json_text).unwrap_or_default();
            tables.push(CatalogTable {
                name,
                kind: table_kind_of_engine(&engine),
                schema: Some(database.clone()),
            });
        }
        Ok(tables)
    }

    /// 一张表的列（走 `system.columns`，`position` 升序即声明序）。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        self.column_types(table)
    }

    /// 一张表的列级元数据（`system.columns`：默认值 / 注释拿得到；可空从类型文本的
    /// `Nullable(` 前缀推导——ClickHouse 把可空编码进类型本身；字符集没有）。
    pub fn table_column_meta(&mut self, table: &TableRef) -> Result<Vec<TableColumnMeta>, DbError> {
        let database = self.resolve_schema(table.schema.as_deref());
        let reply = self.endpoint.post(
            "SELECT name, type, default_expression, comment FROM system.columns \
             WHERE database = {db:String} AND table = {tbl:String} ORDER BY position",
            None,
            self.read_only,
            &[("db", database.as_str()), ("tbl", table.name.as_str())],
        )?;
        let parsed = parse_compact(&reply.body)?;
        let mut columns = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            let Some(name) = row.first().and_then(json_text) else {
                continue;
            };
            let type_name = row.get(1).and_then(json_text).unwrap_or_default();
            columns.push(TableColumnMeta {
                nullable: Some(type_name.starts_with("Nullable(")),
                default: row
                    .get(2)
                    .and_then(json_text)
                    .filter(|text| !text.is_empty()),
                comment: row.get(3).and_then(json_text).unwrap_or_default(),
                character_set: None,
                collation: None,
                name,
                type_name,
            });
        }
        Ok(columns)
    }

    /// [`Self::table_columns`] 的 `&self` 版本。
    ///
    /// 为什么要单独一个有：`table_page` 是 `&self`，而它的过滤需要知道每一列的**声明
    /// 类型**才能决定顺序比较走数值序还是字典序（见 [`Self::table_page`]）。只读查询
    /// 本来就不需要 `&mut self`——`table_columns` 收 `&mut` 只是沿用了同一 impl 的写法。
    fn column_types(&self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let database = self.resolve_schema(table.schema.as_deref());
        let reply = self.endpoint.post(
            "SELECT name, type FROM system.columns \
             WHERE database = {db:String} AND table = {tbl:String} ORDER BY position",
            None,
            self.read_only,
            &[("db", database.as_str()), ("tbl", table.name.as_str())],
        )?;
        let parsed = parse_compact(&reply.body)?;
        let mut columns = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            let Some(name) = row.first().and_then(json_text) else {
                continue;
            };
            columns.push(Column {
                name,
                type_name: row.get(1).and_then(json_text).unwrap_or_default(),
                comment: String::new(),
            });
        }
        Ok(columns)
    }

    /// 一张表的分页数据（标识符用反引号；`order_by` 为空时依赖引擎自身的序）。
    ///
    /// 过滤值走官方的 `param_<名>=<值>` + `{名:String}` 占位（**没有字符串拼接**）。
    ///
    /// # ⚠️ 顺序比较必须按列的声明类型分叉
    ///
    /// ClickHouse 不允许 Number 与 String 直接比较（`FunctionsComparison.h`：numbers /
    /// strings / dates 各自成组，「within each group, but not from different groups」）。
    /// 而 `{fN:String}` 参数**就是 String 类型**，所以两侧必须有一侧被转。转到哪一边
    /// 决定了语义，两种都要：
    ///
    /// - **数值列 + `>` / `>=` / `<` / `<=`** → 两侧都过 `toFloat64OrNull` → **数值序**。
    ///   这修掉了一个真实缺陷：此前一律 `toString(列) op {fN:String}`，比的是字典序，
    ///   `10 > 9` 会被判成**假**（`'1' < '9'`），数值列的 `>` / `<` 因此是错的。
    /// - **其余全部**（相等 / 不等 / `LIKE` / 文本列与日期列的顺序比较）→
    ///   `toString(列) op {fN:String}` → **字典序**。对 ISO 日期串这正是时间序
    ///   （`'2024-01-01' < '2024-02-01'`），对文本也正是期望语义；而且 `Date` /
    ///   `DateTime` / `Enum` 这些列不能过 `toFloat64OrNull`，硬转会报类型错。
    ///
    /// 已知代价：数值序那一档走 Float64，`|整数| > 2^53` 会丢精度。要在「无服务端类型
    /// 推断、且一条 SQL 生成路径同时服务文本列与数值列」的前提下同时拿到两种语义，
    /// 这是最简的可用解（要精确就得按列类型生成不同的转型表达式，那是更大的改造）。
    pub fn table_page(
        &self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let quoted = quote_qualified(table, '`')?;
        let order = order_by_clause(order_by, '`')?;
        // 过滤才需要列类型（多一次 `system.columns`，无过滤时不查）。
        // 取不到类型时退化为空表 —— 于是所有顺序比较都走字典序，也就是**改动前的一贯
        // 行为**：过滤不该因为一次元数据查询失败而整页报错。
        let types = if filters.is_empty() {
            Vec::new()
        } else {
            self.column_types(table).unwrap_or_default()
        };
        let (conditions, values) = filter_conditions(
            filters,
            |name, op| {
                let quoted_column = quote_identifier(name, '`')?;
                Ok(if numeric_ordering(&types, name, op) {
                    format!("toFloat64OrNull({quoted_column})")
                } else if op.needs_value() {
                    format!("toString({quoted_column})")
                } else {
                    quoted_column
                })
            },
            |index, filter| {
                let placeholder = format!("{{f{index}:String}}");
                // 与上面那条分支**必须同源**（都问 `numeric_ordering`）：一旦判定不一致，
                // 就会生成「列转 Float64、值还是 String」这类必然报类型错的 SQL。
                if numeric_ordering(&types, &filter.column, filter.op) {
                    format!("toFloat64OrNull({placeholder})")
                } else {
                    placeholder
                }
            },
        )?;
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sql =
            format!("SELECT * FROM {quoted}{where_clause}{order} LIMIT {limit} OFFSET {offset}");
        let names: Vec<String> = (1..=values.len())
            .map(|index| format!("f{index}"))
            .collect();
        let params: Vec<(&str, &str)> = names
            .iter()
            .zip(values.iter())
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        self.query_one_with(&sql, &params, limit)
    }

    /// **总是空**：ClickHouse 没有可供网格编辑定位的「行主键」。
    ///
    /// 它的 `PRIMARY KEY` 是 `ORDER BY` 的**排序表达式**（可以不是列，例如
    /// `cityHash64(id)`），而且 `UPDATE` / `DELETE` 都是异步 mutation（`ALTER TABLE …
    /// UPDATE`，落在后台的 mutation 队列里，返回的是「已排队」而不是「已改 N 行」）。
    /// 所以返回空 —— 编辑层据此把 ClickHouse 的表浏览判为只读，这是**诚实**的结果，
    /// 而不是给一个点了会失败的按钮。
    pub fn table_primary_key(&mut self, _table: &TableRef) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    /// 改一格：不支持（见 [`Self::table_primary_key`]）。
    pub fn update_cell(
        &mut self,
        _table: &TableRef,
        _pk: &PkValues,
        _column: &str,
        _value: Option<&str>,
    ) -> Result<u64, DbError> {
        Err(unsupported_mutation())
    }

    /// 按主键删行：不支持（见 [`Self::table_primary_key`]）。
    pub fn delete_rows(&mut self, _table: &TableRef, _pk: &PkValues) -> Result<u64, DbError> {
        Err(unsupported_mutation())
    }

    /// 批量删行：不支持。
    pub fn delete_row_set(
        &mut self,
        _table: &TableRef,
        _keys: &[PkValues],
    ) -> Result<u64, DbError> {
        Err(unsupported_mutation())
    }

    /// 批量改格：不支持。
    pub fn update_cells(
        &mut self,
        _table: &TableRef,
        _edits: &[crate::drivers::CellEdit],
    ) -> Result<u64, DbError> {
        Err(unsupported_mutation())
    }

    /// 暂存式编辑提交：不支持。
    pub fn apply_batch(
        &mut self,
        _table: &TableRef,
        _edits: &[crate::drivers::CellEdit],
        _inserts: &[crate::drivers::InsertRow],
    ) -> Result<crate::drivers::BatchOutcome, DbError> {
        Err(unsupported_mutation())
    }

    /// 一张表的建表 DDL（`SHOW CREATE TABLE` 原文）。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let quoted = quote_qualified(table, '`')?;
        let reply = self.endpoint.post(
            &format!("SHOW CREATE TABLE {quoted}"),
            None,
            self.read_only,
            &[],
        )?;
        first_cell_text(&reply.body)
            .ok_or_else(|| DbError::Query(format!("table not found: {table}")))
    }

    /// 一张表上的**跳数索引**（data skipping index，`system.data_skipping_indices`）。
    ///
    /// 它们不是唯一索引（`unique = false`，ClickHouse 没有唯一约束），也算不上
    /// 主键（`primary = false`）—— 但对「这条查询为什么快 / 慢」很有用，所以照实给。
    pub fn table_indexes(&mut self, table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        let database = self.resolve_schema(table.schema.as_deref());
        let reply = self.endpoint.post(
            "SELECT name, expr, type FROM system.data_skipping_indices \
             WHERE database = {db:String} AND table = {tbl:String} ORDER BY name",
            None,
            self.read_only,
            &[("db", database.as_str()), ("tbl", table.name.as_str())],
        )?;
        let parsed = parse_compact(&reply.body)?;
        let mut indexes = Vec::with_capacity(parsed.rows.len());
        for row in &parsed.rows {
            let Some(name) = row.first().and_then(json_text) else {
                continue;
            };
            // `expr` 是索引表达式、`type` 是算法名（minmax / bloom_filter / set …）；
            // 表达式为空时退而展示算法名，至少让用户知道这里有东西。
            let expr = row.get(1).and_then(json_text).unwrap_or_default();
            let algorithm = row.get(2).and_then(json_text).unwrap_or_default();
            let detail = if expr.is_empty() { algorithm } else { expr };
            indexes.push(TableIndex {
                name,
                columns: if detail.is_empty() {
                    Vec::new()
                } else {
                    vec![detail]
                },
                unique: false,
                primary: false,
            });
        }
        Ok(indexes)
    }

    /// 行数：读 `system.tables.total_rows`。
    ///
    /// 口径标成**估算**：MergeTree 下它由元数据维护、基本精确，但 Distributed /
    /// 视图这类表给的是「已知的那部分」，所以不承诺精确。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let database = self.resolve_schema(table.schema.as_deref());
        let reply = self.endpoint.post(
            "SELECT total_rows FROM system.tables \
             WHERE database = {db:String} AND name = {tbl:String}",
            None,
            self.read_only,
            &[("db", database.as_str()), ("tbl", table.name.as_str())],
        )?;
        let parsed = parse_compact(&reply.body)?;
        let Some(row) = parsed.rows.first() else {
            return Err(DbError::Query(format!("table not found: {table}")));
        };
        let estimate = match row.first() {
            Some(serde_json::Value::Number(number)) => number.as_u64().unwrap_or(0),
            Some(serde_json::Value::String(text)) => text.parse().unwrap_or(0),
            _ => 0,
        };
        Ok(RowCount::Estimate(estimate))
    }

    /// 批量插入：一条 `INSERT … VALUES (…), (…)` 多行语句。
    ///
    /// ClickHouse 没有「事务」可言（Atomic 库引擎只保证单条语句的原子可见性），
    /// 所以合成**一条**语句就是这里能做到的最强原子性——比逐行 INSERT 强。
    pub fn insert_rows(
        &mut self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Option<String>>],
    ) -> Result<u64, DbError> {
        let sql = insert_sql(table, columns, rows)?;
        let reply = self.send_with_id(&sql)?;
        // 老版本不带 `X-ClickHouse-Summary`，那时回落到「按提交的行数计」。
        Ok(if reply.written_rows > 0 {
            reply.written_rows
        } else {
            rows.len() as u64
        })
    }

    /// 把 `None` 解析成具体的库名：调用方给的 → 建连时那个 → [`DEFAULT_DATABASE`]。
    fn resolve_schema(&self, schema: Option<&str>) -> String {
        pick_database(schema, &self.endpoint.database).to_owned()
    }

    /// 跑一条只返回单列文本的语句。
    fn strings(&self, sql: &str, params: &[(&str, &str)]) -> Result<Vec<String>, DbError> {
        let reply = self.endpoint.post(sql, None, self.read_only, params)?;
        let parsed = parse_compact(&reply.body)?;
        Ok(parsed
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(json_text))
            .collect())
    }
}

/// 统一的「这个引擎不做网格编辑」文案。
fn unsupported_mutation() -> DbError {
    DbError::Unsupported(
        "ClickHouse has no in-place row editing (UPDATE/DELETE are asynchronous mutations)".into(),
    )
}

/// 拼端点（scheme 由 TLS 档位决定）+ 建 agent。
fn build_endpoint(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Endpoint, DbError> {
    let port = cfg.port.unwrap_or_else(|| {
        cfg.kind
            .default_port()
            .expect("ClickHouse has a default port")
    });
    let (scheme, insecure) = match cfg.tls {
        TlsMode::Disable => ("http", false),
        TlsMode::Trust => ("https", false),
        TlsMode::DangerAcceptInvalid => ("https", true),
    };
    let host = cfg.host.trim();
    // IPv6 字面量要带方括号，否则 `[::1]:8123` 拼出来是歧义的。
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let base = format!("{scheme}://{authority}");

    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
        .timeout_global(Some(std::time::Duration::from_secs(HARD_TIMEOUT_SECS)));
    if insecure {
        builder = builder.tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(true)
                .build(),
        );
    }

    Ok(Endpoint {
        base,
        database: cfg.database.trim().to_owned(),
        user: cfg.user.trim().to_owned(),
        password: password.unwrap_or_default().to_owned(),
        agent: ureq::Agent::new_with_config(builder.build()),
    })
}

/// `engine` 名 → 目录对象种类。
///
/// ClickHouse 的视图引擎有 `View` / `MaterializedView` / `LiveView` / `WindowView`，
/// 一律带 `View` 后缀；其余（MergeTree 族、Log、Kafka、Distributed …）都是表。
fn table_kind_of_engine(engine: &str) -> TableKind {
    if engine.ends_with("View") {
        TableKind::View
    } else {
        TableKind::Table
    }
}

/// 生成一个可寻址的 `query_id`。
///
/// 只要进程内不重复即可（取消只在同一进程的同一连接里寻址），所以用进程 id +
/// 单调计数：比引入一个随机数依赖划算。
fn next_query_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("duck-{}-{seq}", std::process::id())
}

/// `JSONCompact` 解析结果：列元信息 + 行。
struct Compact {
    columns: Vec<Column>,
    rows: Vec<Vec<serde_json::Value>>,
}

/// 解析 `FORMAT JSONCompact` 的响应。
fn parse_compact(body: &str) -> Result<Compact, DbError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Compact {
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|error| DbError::Query(format!("response is not JSONCompact: {error}")))?;

    let columns = value
        .get("meta")
        .and_then(serde_json::Value::as_array)
        .map(|meta| {
            meta.iter()
                .map(|entry| Column {
                    name: entry
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    type_name: entry
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    // ClickHouse 的列注释要另发 DESCRIBE 才拿得到，结果集元数据里没有。
                    comment: String::new(),
                })
                .collect()
        })
        .unwrap_or_default();

    let rows = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|data| {
            data.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| cells.to_vec())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Compact { columns, rows })
}

/// 响应 → [`ResultSet`]（`max_rows` 处截断并置 `truncated`）。
fn result_set(body: &str, max_rows: usize) -> Result<ResultSet, DbError> {
    let parsed = parse_compact(body)?;
    let types: Vec<String> = parsed.columns.iter().map(|c| c.type_name.clone()).collect();
    let mut rows = Vec::with_capacity(parsed.rows.len().min(max_rows));
    let mut truncated = false;
    for row in parsed.rows {
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(row.len());
        for (index, cell) in row.iter().enumerate() {
            let type_name = types.get(index).map(String::as_str).unwrap_or_default();
            cells.push(map_json(cell, type_name));
        }
        rows.push(cells);
    }
    Ok(ResultSet {
        columns: parsed.columns,
        rows,
        truncated,
    })
}

/// 取第一行第一列的文本（`SELECT version()` / `SHOW CREATE TABLE` 这类单值响应）。
fn first_cell_text(body: &str) -> Option<String> {
    let parsed = parse_compact(body).ok()?;
    parsed.rows.first()?.first().and_then(json_text)
}

/// JSON 标量 → 文本（数字与字符串都给；数组 / 对象给紧凑 JSON）。
fn json_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        serde_json::Value::Null => None,
        other => serde_json::to_string(other).ok(),
    }
}

/// 剥掉 `Nullable(...)` / `LowCardinality(...)` 这些**包装**，露出基底类型名。
fn base_type(raw: &str) -> &str {
    let mut current = raw.trim();
    loop {
        let (head, inner) = match current.split_once('(') {
            Some((head, rest)) => (head.trim(), rest.strip_suffix(')').unwrap_or(rest).trim()),
            None => return current,
        };
        if matches!(head, "Nullable" | "LowCardinality") && !inner.is_empty() {
            current = inner;
        } else {
            return current;
        }
    }
}

/// 一个 JSON 单元格 → 统一值模型（按 ClickHouse 类型名分派）。
fn map_json(value: &serde_json::Value, type_name: &str) -> Value {
    use serde_json::Value as Json;

    if value.is_null() {
        return Value::Null;
    }
    // `Nullable(X)` 的 null 已经在上面拦掉；这里按基底类型走。
    let base = base_type(type_name);
    let numeric = || match value {
        Json::Number(number) => Some(number),
        _ => None,
    };
    match base {
        "Bool" => match value {
            Json::Bool(flag) => Value::Bool(*flag),
            other => Value::Bool(json_text(other).as_deref() == Some("true")),
        },
        "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256" => match numeric() {
            Some(number) => match number.as_i64() {
                Some(parsed) => Value::Int(parsed),
                // 128/256 位整数超出 i64：JSONCompact 给的是数字（可能被 serde_json
                // 归到 u64 或 f64），拿不到整数就退回文本形态。
                None => Value::Text(json_text(value).unwrap_or_default()),
            },
            // 超出 64 位时 ClickHouse 会把它当字符串发出来。
            None => Value::Text(json_text(value).unwrap_or_default()),
        },
        "UInt8" | "UInt16" | "UInt32" | "UInt64" | "UInt128" | "UInt256" => match numeric() {
            Some(number) => match number.as_u64() {
                Some(parsed) => Value::UInt(parsed),
                None => Value::Text(json_text(value).unwrap_or_default()),
            },
            None => Value::Text(json_text(value).unwrap_or_default()),
        },
        "Float32" | "Float64" => match numeric().and_then(serde_json::Number::as_f64) {
            Some(parsed) => Value::Float(parsed),
            None => Value::Text(json_text(value).unwrap_or_default()),
        },
        // `Decimal(18, 4)` / `Decimal64(4)`：**绝不走 f64**（本仓统一值模型的硬纪律）。
        base if base.starts_with("Decimal") => Value::Text(json_text(value).unwrap_or_default()),
        "Date" | "Date32" => Value::Date(json_text(value).unwrap_or_default()),
        "DateTime" | "DateTime64" => Value::DateTime(json_text(value).unwrap_or_default()),
        // String / FixedString / Enum8 / Enum16 / UUID / IPv4 / IPv6 …：一律文本。
        "String" | "FixedString" | "Enum8" | "Enum16" | "UUID" | "IPv4" | "IPv6" => {
            Value::Text(json_text(value).unwrap_or_default())
        }
        // Array / Tuple / Map / Nested / AggregateFunction …：没有对应的标量模型，
        // 落成 JSON 文本（界面按文本渲染，至少信息不丢）。
        _ => match value {
            Json::String(text) => Value::Text(text.clone()),
            other => match serde_json::to_string(other) {
                Ok(text) => Value::Json(text),
                Err(_) => Value::Text(json_text(other).unwrap_or_default()),
            },
        },
    }
}

/// 拼多行 INSERT（字符串字面量手工转义 —— ClickHouse 的 `VALUES` 不走 URL 参数）。
fn insert_sql(
    table: &TableRef,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> Result<String, DbError> {
    let quoted = quote_qualified(table, '`')?;
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, '`'))
        .collect::<Result<_, _>>()?;
    let mut sql = format!("INSERT INTO {quoted} ({}) VALUES ", quoted_cols.join(", "));
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        let cells: Vec<String> = row
            .iter()
            .map(|cell| match cell {
                Some(text) => format!("'{}'", escape_string(text)),
                None => "NULL".to_owned(),
            })
            .collect();
        sql.push('(');
        sql.push_str(&cells.join(", "));
        sql.push(')');
    }
    Ok(sql)
}

/// ClickHouse 字符串字面量转义：反斜杠与单引号都要转（ClickHouse 认 `\'` 也认 `''`，
/// 这里用反斜杠形式——它同时压住了 `\n` / `\t` 这类会被当控制字符解释的序列）。
fn escape_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端点拼装：TLS 档位决定 scheme，IPv6 字面量要带方括号，缺省端口是 8123。
    #[test]
    fn endpoint_scheme_and_port_follow_tls_and_kind() {
        let base = ConnectionConfig {
            id: "conn-1".into(),
            kind: crate::config::DatabaseKind::ClickHouse,
            host: "ch.local".into(),
            database: "default".into(),
            ..ConnectionConfig::default()
        };
        let plain = build_endpoint(&base, None).expect("不含 TLS 也要能拼出端点");
        assert!(
            plain.base.starts_with("http://ch.local:8123"),
            "{}",
            plain.base
        );

        let tls = ConnectionConfig {
            tls: TlsMode::Trust,
            ..base.clone()
        };
        assert!(
            build_endpoint(&tls, None)
                .expect("TLS 端点")
                .base
                .starts_with("https://ch.local:8123")
        );

        let v6 = ConnectionConfig {
            host: "::1".into(),
            ..base
        };
        assert!(
            build_endpoint(&v6, None)
                .expect("IPv6 端点")
                .base
                .starts_with("http://[::1]:8123")
        );
    }

    /// 库名可留空：调用方给的 → 档案里配的 → `default`（与请求不带 `database` 参数时
    /// 服务端的 `currentDatabase()` 对齐）。
    #[test]
    fn blank_database_falls_back_to_default() {
        assert_eq!(pick_database(Some("analytics"), "default"), "analytics");
        assert_eq!(pick_database(None, "analytics"), "analytics");
        assert_eq!(pick_database(None, ""), DEFAULT_DATABASE);
        assert_eq!(pick_database(None, "   "), DEFAULT_DATABASE);
        assert_eq!(pick_database(Some("  "), "  "), DEFAULT_DATABASE);
    }

    /// `JSONCompact` 解析：列元信息 + 行，空正文当作无结果。
    #[test]
    fn compact_payload_is_parsed() {
        let body = r#"{"meta":[{"name":"a","type":"UInt8"},{"name":"b","type":"String"}],
            "data":[[1,"x"],[2,"y"]],"rows":2}"#;
        let parsed = parse_compact(body).expect("合法 JSONCompact");
        assert_eq!(parsed.columns.len(), 2);
        assert_eq!(parsed.columns[0].name, "a");
        assert_eq!(parsed.columns[1].type_name, "String");
        assert_eq!(parsed.rows.len(), 2);

        assert!(parse_compact("").expect("空正文").rows.is_empty());
        assert!(parse_compact("Code: 60. DB::Exception").is_err());
    }

    /// 值映射：整数 / 小数 / 日期 / Nullable 包装 / 复合类型各归各位，Decimal 不走 f64。
    #[test]
    fn values_map_by_declared_type() {
        let json = |raw: &str| serde_json::from_str::<serde_json::Value>(raw).expect("json");

        assert_eq!(map_json(&json("7"), "UInt8"), Value::UInt(7));
        assert_eq!(map_json(&json("-7"), "Int16"), Value::Int(-7));
        assert_eq!(map_json(&json("1.5"), "Float64"), Value::Float(1.5));
        // Decimal 一律文本：绝不经过 f64。
        assert_eq!(
            map_json(&json("\"12.3400\""), "Decimal(18, 4)"),
            Value::Text("12.3400".into())
        );
        // Nullable 包装要剥掉，按基底类型走。
        assert_eq!(map_json(&json("3"), "Nullable(UInt8)"), Value::UInt(3));
        assert_eq!(
            map_json(&json("null"), "Nullable(UInt8)"),
            Value::Null,
            "NULL 不按基底类型解释"
        );
        assert_eq!(
            map_json(&json("\"2026-10-12\""), "Date32"),
            Value::Date("2026-10-12".into())
        );
        // 复合类型落 JSON 文本，信息不丢。
        assert_eq!(
            map_json(&json("[1,2]"), "Array(UInt8)"),
            Value::Json("[1,2]".into())
        );
    }

    /// 行数截断在 Rust 侧做：超出 `max_rows` 置 `truncated`，且不吞掉列元信息。
    #[test]
    fn rows_are_truncated_at_the_cap() {
        let body = r#"{"meta":[{"name":"a","type":"UInt8"}],"data":[[1],[2],[3]],"rows":3}"#;
        let short = result_set(body, 2).expect("解析成功");
        assert_eq!(short.rows.len(), 2);
        assert!(short.truncated, "被截断时要置位");
        assert_eq!(short.columns.len(), 1);

        let full = result_set(body, 9).expect("解析成功");
        assert_eq!(full.rows.len(), 3);
        assert!(!full.truncated, "没到上限就不该说截断");
    }

    /// INSERT 转义：单引号 / 反斜杠 / 换行都要压住，`None` 落 NULL。
    #[test]
    fn insert_escapes_string_literals() {
        let table = TableRef::bare("events");
        let columns = vec!["name".to_owned(), "note".to_owned()];
        let rows = vec![vec![Some("O'Brien\\x".to_owned()), None]];
        let sql = insert_sql(&table, &columns, &rows).expect("可拼");
        assert_eq!(
            sql,
            "INSERT INTO `events` (`name`, `note`) VALUES ('O\\'Brien\\\\x', NULL)"
        );
    }

    /// 视图引擎名归一：带 `View` 后缀的是视图，其余是表。
    #[test]
    fn view_engines_are_recognised() {
        assert_eq!(table_kind_of_engine("View"), TableKind::View);
        assert_eq!(table_kind_of_engine("MaterializedView"), TableKind::View);
        assert_eq!(table_kind_of_engine("MergeTree"), TableKind::Table);
        assert_eq!(table_kind_of_engine("Distributed"), TableKind::Table);
    }

    /// 目录对象种类不再受「只有表 / 视图」限制：函数与序列也能表达。
    #[test]
    fn relation_kinds_stay_explicit() {
        assert!(TableKind::Table.is_relation());
        assert!(TableKind::View.is_relation());
        assert!(!TableKind::Procedure.is_relation());
        assert!(!TableKind::Function.is_relation());
        assert!(!TableKind::Sequence.is_relation());
    }

    /// 数值族判定：数值算、日期/文本/枚举/复合**不算**（它们不能过 `toFloat64OrNull`，
    /// 而且字典序本来就是它们的正确语义）。
    #[test]
    fn numeric_type_detection_covers_the_number_family_only() {
        for numeric in [
            "UInt8",
            "UInt64",
            "UInt256",
            "Int32",
            "Int128",
            "Float32",
            "Float64",
            "Decimal(18, 4)",
            "Decimal256(10)",
            "Bool",
        ] {
            assert!(is_numeric_type(numeric), "{numeric} 应判为数值族");
        }

        // 包装要剥掉（含叠加）。
        assert!(is_numeric_type("Nullable(UInt64)"));
        assert!(is_numeric_type("LowCardinality(Int32)"));
        assert!(is_numeric_type("Nullable(LowCardinality(Int64))"));
        assert!(is_numeric_type("  Nullable(Float64)  "), "首尾空白要容忍");
        assert!(!is_numeric_type("Nullable(String)"), "剥掉包装后是文本");

        for non_numeric in [
            "String",
            "FixedString(8)",
            "Date",
            "Date32",
            "DateTime",
            "DateTime64(3)",
            "UUID",
            "IPv4",
            "IPv6",
            "Enum8('a' = 1, 'b' = 2)",
            "Array(UInt8)",
            "Map(String, UInt8)",
            "Tuple(UInt8, String)",
            "Nothing",
        ] {
            assert!(
                !is_numeric_type(non_numeric),
                "{non_numeric} 不该判为数值族"
            );
        }
    }

    /// 决策函数：**只有**「顺序算子 + 数值列」才走数值序。这条判定是
    /// `table_page` 里 `column_expr` / `value_expr` 两个闭包的共同来源，口径必须一致。
    #[test]
    fn numeric_ordering_needs_both_an_ordering_op_and_a_numeric_column() {
        let columns = |raw: &[(&str, &str)]| {
            raw.iter()
                .map(|(name, type_name)| Column {
                    name: (*name).to_owned(),
                    type_name: (*type_name).to_owned(),
                    comment: String::new(),
                })
                .collect::<Vec<_>>()
        };
        let types = columns(&[("amount", "UInt64"), ("name", "String"), ("at", "DateTime")]);

        for op in [FilterOp::Gt, FilterOp::Gte, FilterOp::Lt, FilterOp::Lte] {
            assert!(
                numeric_ordering(&types, "amount", op),
                "数值列 {op:?} 走数值序"
            );
            assert!(
                !numeric_ordering(&types, "name", op),
                "文本列 {op:?} 走字典序"
            );
            assert!(
                !numeric_ordering(&types, "at", op),
                "日期列 {op:?} 走字典序"
            );
            assert!(
                !numeric_ordering(&types, "missing", op),
                "列名对不上时保守走字典序"
            );
        }

        // 相等 / 不等 / LIKE 一律文本比较（数值列也走 toString，做精确文本匹配）。
        for op in [FilterOp::Eq, FilterOp::NotEq, FilterOp::Like] {
            assert!(
                !numeric_ordering(&types, "amount", op),
                "{op:?} 不该走数值序"
            );
        }
        // 无值算子同理（它们本来就不进 `filter_conditions` 的值分支）。
        for op in [FilterOp::IsNull, FilterOp::IsNotNull] {
            assert!(
                !numeric_ordering(&types, "amount", op),
                "{op:?} 不该走数值序"
            );
        }

        // 类型未知（元数据查询失败时的退化路径）→ 走字典序，即改动前的一贯行为。
        assert!(!numeric_ordering(&[], "amount", FilterOp::Gt));
    }
}
