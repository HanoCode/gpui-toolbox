//! Elasticsearch 驱动：**REST 接口**直连（`ureq`，零新增依赖）。
//!
//! # 为什么走 REST 而不是 `elasticsearch` crate
//!
//! `docs/database-design.md` §5 的选型就是 `ureq` 直连 REST，与 ClickHouse 同一条路子。
//! 两条理由：① Elasticsearch 的 REST 接口就是它的**一等入口**（官方文档、Kibana、
//! 所有 SDK 都建立在它之上），没有"更适合程序化访问"的私有协议；② `ureq` 已在依赖
//! 图内（更新器 / 插件出网 / AI 厂商 / ClickHouse），所以这一条引擎也是**零新增依赖**。
//!
//! # 编辑器里的「查询语言」是 Kibana Dev Tools 的请求格式
//!
//! 一个纯 REST 引擎没有 SQL 可写，必须自定一门输入语法。这里选**Kibana 控制台**的
//! 那一套（`<METHOD> <PATH>` + 换行 + JSON 体，空行分隔多条）：
//!
//! ```text
//! GET /logs-*/_search
//! {"query": {"match_all": {}}, "size": 3}
//! ```
//!
//! 理由与 MongoDB 那条相同——**不自造壳语法**。Kibana 控制台格式已经是事实标准
//! （官方文档里到处是可直接粘贴的片段），复用它等于复用别人的用户教育成本；自造一门
//! 糖衣语法则等于自己维护一门方言。`#` 开头的行当注释跳过。
//!
//! # 与「网格契约」的映射
//!
//! - **库 / schema ↔ 没有这一层**：Elasticsearch 的索引是**平铺**的，没有 database
//!   或 schema 容器。`list_schemas()` 因此返回空，界面的命名空间下拉自然不出现
//!   （它只在清单长度 > 1 时才渲染），不必给界面加引擎特判。
//! - **表 ↔ 索引（index）**，**视图 ↔ 别名（alias）**：别名是"指向一个或多个索引的
//!   虚拟名字"，与视图语义最接近。两者一次 `GET /<pattern>/_alias` 就都拿到了
//!   （不用 `_cat/indices`——那条要 `monitor` 权限，而 `_alias` 只要索引元数据权限）。
//! - **行 ↔ 文档**，**主键恒 `_id`**：`_id` 是每个文档的地址，也是 `_update` /
//!   `_doc/<id>` 唯一可靠的定位方式。⚠️ 它是**虚拟列**——不在 `_source` 里，所以由本
//!   驱动注入到结果集首位（不注入的话编辑层按列名找不到主键，整张表会被判成不可编辑）。
//! - **列 ↔ mapping 里的字段路径**：`properties` 递归展平（`user.name`），**多字段
//!   （`fields`）跳过**——`title.keyword` 不是 `_source` 里独立的一格，把它列成列会让
//!   用户以为能单独编辑。runtime 字段同理跳过（不在 `_source`，列出来永远空白）。
//!
//! # 排序与分页
//!
//! ⚠️ **`_id` 不能排序**（服务端会以 `Fielddata access on the _id field is disallowed`
//! 拒绝），而调用方给的 `order_by` 恰好就是主键。所以排序档先按**列类型**过滤一遍：
//! 只有 keyword 族 / 数值 / 日期 / 布尔 / ip 可排；过滤后为空就退回 **`_doc`**
//! （段内物理序，是"没有排序"时的确定性落点）。`text` 字段不可排（默认没开 fielddata），
//! 也一并被类型过滤挡在构造子句之前，用户拿不到一条服务端报错。
//!
//! 分页走 `from` + `size`，因此受 `index.max_result_window`（缺省 10000）限制；
//! 翻过头服务端会明确报 `Result window is too large`——本条驱动**原样透出**，不吞。
//!
//! # 过滤：查询 DSL，不是 `WHERE`
//!
//! 表浏览的过滤条件（[`TableFilter`]）在这里翻译成 ES 的查询 DSL：等值 `term`、
//! 不等 `bool.must_not` + `term`、比较 `range`、`LIKE` → `wildcard`（`%`→`*`、`_`→`?`，
//! 字面量里的 `*` `?` `\` 先转义）、`IS [NOT] NULL` → `exists` 的正反；多条之间取 `AND`
//! （`bool.must`）。值一律走 JSON 编码（见 [`encode`]），不存在字符串拼接，注入无从谈起。
//!
//! ⚠️ **不做"类型不可查就悄悄丢掉"的过滤**。排序那条能静默按类型过滤，是因为 `order_by`
//! 由调用方按主键自动生成——丢掉是自己修自己的问题；而过滤条件是用户一条一条点出来的，
//! 丢掉等于把条件吃掉、结果与看到的条件对不上话。所以照实发出去：`text` 字段上的
//! `wildcard` / `term`（默认没开 fielddata）会由服务端报错，这条驱动原样透出。
//!
//! # 只读档是客户端拦截
//!
//! Elasticsearch 没有"会话只读"开关（与 SQL Server 同理），所以只读是驱动在请求进网
//! **之前**拦下：`GET` / `HEAD` 一律放行（REST 语义保证数据变更都走
//! `POST`/`PUT`/`DELETE`/`PATCH`），`POST` 只在路径段命中**只读命令白名单**时放行
//! （`_search` / `_count` / `_msearch` / `_mget` / `_sql` …——搜、数、聚合都是 POST），
//! 其余一律拒；**未知命令一律当写**，安全方向必须是默认拒绝。真要防住生产集群仍然
//! 应该用只读角色，这一层只是多一道闸。
//!
//! # 没有事务
//!
//! Elasticsearch **没有跨文档事务**（`_bulk` 里每条 action 各自原子提交），所以批量
//! 改格 / 删行不是"要么全成要么全回滚"——与 MongoDB 那条的边界完全同构。部分失败时
//! 驱动会**吵出来**并报出已生效的条数（见 [`Endpoint`] 的 `bulk_send`），调用方按
//! 返回的影响行数核对并刷新。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Map, Value as Json};

use crate::cancel::CancelHandle;
use crate::config::{ConnectionConfig, TlsMode};
use crate::drivers::{
    BatchOutcome, CatalogTable, CellEdit, FilterOp, InsertRow, OrderBy, PkValues, RowCount,
    TableFilter, TableIndex, TableKind, TableRef, url_encode,
};
use crate::error::DbError;
use crate::result::{Column, ExecOutcome, ResultSet, Value};

/// 连接兜底超时：与 ClickHouse 同口径（真正的"停"由 [`crate::cancel::CancelTimer`] 负责）。
const HARD_TIMEOUT_SECS: u64 = 3600;

/// TCP 连接超时（比兜底短得多：连不上要快速失败）。
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// 缺省 REST 端口。
const DEFAULT_PORT: u16 = 9200;

/// 主键列名（虚拟列，见模块文档）。
const ID_COLUMN: &str = "_id";

/// 主键列的类型名（展示用；提示这是一条由驱动合成的列）。
const ID_TYPE: &str = "_id";

/// 只读档下允许的 `POST` 端点标记（路径里**任一段**命中即算读）。
///
/// `GET` / `HEAD` 不在表里——它们本来就全放行。列在这里的都是"必须用 POST 提交请求体
/// 的读操作"：搜、数、多搜索、批量取、字段能力、分析器、解释、校验、滚动续取。
const READ_ONLY_MARKERS: &[&str] = &[
    "_search",
    "_count",
    "_msearch",
    "_mget",
    "_field_caps",
    "_analyze",
    "_explain",
    "_validate",
    "_search_shards",
    "_terms_enum",
    "_termvectors",
    "_mtermvectors",
    "_rank_eval",
    "_knn_search",
    "_sql",
    "scroll",
];

/// 可以安全地用作排序键的字段类型。
///
/// ⚠️ 故意**不含** `text` / `match_only_text` / `annotated_text`：它们默认没开
/// fielddata，排序会被服务端拒绝。也不含 `object` / `nested`（不是叶子，排不了）。
const SORTABLE_TYPES: &[&str] = &[
    "keyword",
    "constant_keyword",
    "wildcard",
    "long",
    "integer",
    "short",
    "byte",
    "double",
    "float",
    "half_float",
    "scaled_float",
    "unsigned_long",
    "date",
    "date_nanos",
    "boolean",
    "ip",
    "version",
];

/// 索引配置里**易变**的键：每次重建集群都会变，进「DDL」只会把差异噪音灌满界面。
const VOLATILE_SETTING_KEYS: &[&str] = &[
    "creation_date",
    "creation_date_string",
    "uuid",
    "version",
    "provided_name",
    "resize",
    "history",
];

/// 一条解析出来的 REST 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    /// 大写方法（`GET` / `HEAD` / `POST` / `PUT` / `PATCH` / `DELETE`）。
    method: String,
    /// 以 `/` 开头的路径（可带查询串）。
    path: String,
    /// 请求体原文（`None` = 不带体）。**不做 JSON 校验**：`_bulk` 的体是 NDJSON，
    /// 校验反而会误伤；语法错误交给服务端给出准确原因。
    body: Option<String>,
}

/// 发一条请求所需的全部内容。
///
/// 单独成型是因为**取消通道还要拿一份**（`_tasks` 发现与 `_cancel` 必须另发请求，
/// 不能占着主连接）。口令只留在内存，不落盘、不进日志。
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// `http(s)://host:port`，不含路径。
    base: String,
    /// 缺省索引 / 索引模式（可空 = 不限）。
    default_index: String,
    /// `Basic …` 认证头的值；未配用户名时为 `None`（有些单机集群没开安全）。
    auth: Option<String>,
    agent: ureq::Agent,
}

/// 一条响应（取消通道只把响应丢掉，故只需要正文）。
pub(crate) struct Reply {
    body: String,
}

impl Reply {
    /// 正文解析成 JSON；空正文当 `Null`。
    ///
    /// `pub(crate)` 是给取消通道用的：它要读 `_tasks` 的响应来认领任务。
    pub(crate) fn json(&self) -> Result<Json, DbError> {
        parse_json(&self.body)
    }
}

/// 正文 → JSON（空正文当 `Null`）。
fn parse_json(body: &str) -> Result<Json, DbError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(Json::Null);
    }
    serde_json::from_str(trimmed)
        .map_err(|error| DbError::Query(format!("response is not JSON: {error}")))
}

impl Endpoint {
    /// 发一条请求；非 2xx 归成可读错误。
    ///
    /// `body` 有值时用 `send`（会自己补 `Content-Length`），无值时用 `call()`——后者按
    /// 规范不带长度头，正好对上 `GET` / `HEAD` / `DELETE`。
    pub(crate) fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        opaque: Option<&str>,
    ) -> Result<Reply, DbError> {
        let url = format!("{}{}", self.base, path);
        let mut headers: Vec<(&str, String)> = vec![("Accept", "application/json".to_owned())];
        if let Some(auth) = &self.auth {
            headers.push(("Authorization", auth.clone()));
        }
        if let Some(id) = opaque {
            // `X-Opaque-Id` 会被服务端回显在 `_tasks` 的 `headers` 里——取消通道靠它把
            // 「我们发的这一条」从集群里一堆任务中认出来（见 `cancel.rs` 的取消表）。
            headers.push(("X-Opaque-Id", id.to_owned()));
        }

        let mut response = match method {
            // 无体方法：`call()` 不带 `Content-Length`。
            "GET" | "HEAD" | "DELETE" => {
                let builder = match method {
                    "GET" => self.agent.get(&url),
                    "HEAD" => self.agent.head(&url),
                    _ => self.agent.delete(&url),
                };
                headers
                    .iter()
                    .fold(builder, |acc, (key, value)| {
                        acc.header(*key, value.as_str())
                    })
                    .call()
            }
            // 带体方法。
            "POST" | "PUT" | "PATCH" => {
                let builder = match method {
                    "POST" => self.agent.post(&url),
                    "PUT" => self.agent.put(&url),
                    _ => self.agent.patch(&url),
                };
                headers
                    .iter()
                    .fold(builder, |acc, (key, value)| {
                        acc.header(*key, value.as_str())
                    })
                    .send(body.unwrap_or_default())
            }
            other => {
                return Err(DbError::Config(format!(
                    "Elasticsearch supports GET/HEAD/POST/PUT/PATCH/DELETE, not {other}"
                )));
            }
        }
        .map_err(map_transport)?;

        let status = response.status().as_u16();
        // 不用 `Body::read_to_string()`：它自带上限，大结果集会被截成半截 JSON
        // 而报出一个与真实原因无关的解析错误（与 ClickHouse 同一个坑）。
        let mut text = String::new();
        response
            .body_mut()
            .as_reader()
            .read_to_string(&mut text)
            .map_err(|error| DbError::Query(format!("response read failed: {error}")))?;

        if !(200..300).contains(&status) {
            return Err(DbError::Query(error_text(&text, status)));
        }
        Ok(Reply { body: text })
    }

    /// 发一条 `_bulk` 并汇总每条 action 的结果。
    ///
    /// ⚠️ **部分失败要吵**：`_bulk` 逐条独立提交，所以「3 条成功、2 条失败」时既不能报
    /// 全成、也不能报全败。这里把已生效的条数如实写进错误消息，让用户知道现在是
    /// **中间状态**（需要刷新核对），而不是被一句"失败"误导成什么都没发生。
    fn bulk_send(&self, ndjson: &str, opaque: Option<&str>) -> Result<(u64, u64, u64), DbError> {
        let value = self
            .request("POST", "/_bulk", Some(ndjson), opaque)?
            .json()?;
        let (updated, created, deleted, errors) = bulk_counts(&value);
        if let Some(first) = errors.first() {
            return Err(DbError::Query(format!(
                "{} action(s) applied ({} updated, {created} created, {deleted} deleted), \
                 {} failed — first: {first}",
                updated + created + deleted,
                updated,
                errors.len()
            )));
        }
        Ok((updated, created, deleted))
    }
}

/// 传输层错误归类：连不上归 `Connect`，超时归 `Cancelled`，其余归 `Query`。
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

/// 错误正文归一。
///
/// Elasticsearch 的错误是结构化 JSON（`{"error":{"type","reason"},"status":404}`），
/// 剥出 `type: reason` 比把整段 JSON 灌进界面有用得多；剥不出就退回截断的原文
/// （别把整段栈灌进界面）。
fn error_text(body: &str, status: u16) -> String {
    if let Some(detail) = structured_error(body) {
        return format!("HTTP {status}: {detail}");
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("HTTP {status}");
    }
    let clipped: String = trimmed.chars().take(600).collect();
    format!("HTTP {status}: {clipped}")
}

/// 从结构化错误里剥出 `type: reason`（不是那个形状就返回 `None`）。
fn structured_error(body: &str) -> Option<String> {
    let value: Json = serde_json::from_str(body.trim()).ok()?;
    let error = value.get("error")?;
    let reason = error.get("reason")?.as_str()?;
    let kind = error.get("type").and_then(Json::as_str).unwrap_or_default();
    if reason.is_empty() {
        return None;
    }
    Some(if kind.is_empty() {
        reason.to_owned()
    } else {
        format!("{kind}: {reason}")
    })
}

/// 拆「方法 + 路径」的请求行。
///
/// 只认前两个 token（第三个及以后当 HTTP 版本之类的杂尾忽略，与 `curl` 的宽容度一致）；
/// 只有一个 token 时按 `GET` 处理（Kibana 控制台也允许省略方法），路径补上前导 `/`。
fn parse_request_line(line: &str) -> Result<Request, DbError> {
    let mut tokens = line.split_whitespace();
    let first = tokens.next().unwrap_or_default();
    let (method, path) = match tokens.next() {
        Some(second) => (first.to_uppercase(), second.to_owned()),
        None => ("GET".to_owned(), first.to_owned()),
    };
    if path.is_empty() {
        return Err(DbError::Query(format!("request line has no path: {line}")));
    }
    Ok(Request {
        method,
        path: normalize_path(&path),
        body: None,
    })
}

/// 路径归一：补上前导 `/`。
///
/// 用户很可能只写 `logs-*/_search`（不带斜杠），补一下比报错友好；但**不**做别的猜测
/// ——路径原样透传，服务端才是语法的裁判。
fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// 把编辑器里的整段文本拆成若干条请求。
///
/// 规则（Kibana 控制台同构）：**空行分段**、`#` 开头的行是注释、每段首行是请求行、
/// 其余行原样拼成请求体。空段（只有注释 / 空白）会被丢掉。
fn parse_requests(text: &str) -> Result<Vec<Request>, DbError> {
    let mut requests = Vec::new();
    let mut current: Option<Request> = None;
    let mut body_lines: Vec<&str> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            archive(&mut requests, &mut current, &mut body_lines);
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        match current.as_mut() {
            Some(_) => body_lines.push(line),
            None => current = Some(parse_request_line(line.trim())?),
        }
    }
    archive(&mut requests, &mut current, &mut body_lines);
    Ok(requests)
}

/// 收下当前这一段（请求行 + 已攒的体行）。
fn archive(requests: &mut Vec<Request>, current: &mut Option<Request>, body_lines: &mut Vec<&str>) {
    if let Some(mut request) = current.take() {
        let joined = body_lines.join("\n");
        let trimmed = joined.trim();
        if !trimmed.is_empty() {
            request.body = Some(trimmed.to_owned());
        }
        requests.push(request);
    }
    body_lines.clear();
}

/// 路径段（去掉查询串 / 锚点后按 `/` 切）——只读判定用它。
fn path_segments(path: &str) -> Vec<&str> {
    let without_query = path.split(['?', '#']).next().unwrap_or(path);
    without_query
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// 只读判定（见模块文档「只读档是客户端拦截」）。
fn is_read_only_request(method: &str, path: &str) -> bool {
    match method {
        "GET" | "HEAD" => true,
        "POST" => path_segments(path)
            .iter()
            .any(|segment| READ_ONLY_MARKERS.contains(segment)),
        _ => false,
    }
}

/// 一条 Elasticsearch 连接。
pub struct ElasticsearchConnection {
    endpoint: Endpoint,
    /// 只读档：请求进网前客户端拦截。**不是**服务端兜底（见模块文档）。
    read_only: bool,
    /// 正在跑的请求的 `X-Opaque-Id`；取消通道靠它寻址。空闲时是 `None`。
    inflight: Arc<Mutex<Option<String>>>,
    /// `GET /` 拿到的版本号（连接时取一次）。
    version: String,
}

impl ElasticsearchConnection {
    /// 建连：拼端点 + 立刻验证一次（REST 没有握手，「能连上」必须自己证）。
    pub(crate) fn connect(cfg: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let endpoint = build_endpoint(cfg, password);
        // `GET /` 既证明连得上、认证过得去，又顺带把版本拿到手（省一次往返）。
        let reply = endpoint.request("GET", "/", None, None)?;
        let version = version_of(&reply.json()?).unwrap_or_else(|| "unknown".to_owned());
        tracing::info!(host = %cfg.host, index = %cfg.database, "elasticsearch connected");
        Ok(Self {
            endpoint,
            read_only: cfg.read_only,
            inflight: Arc::new(Mutex::new(None)),
            version,
        })
    }

    /// 带外取消句柄（`X-Opaque-Id` 寻址 → `_tasks/<id>/_cancel`）。
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::elasticsearch(self.endpoint.clone(), self.inflight.clone())
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), DbError> {
        self.endpoint.request("GET", "/", None, None)?;
        Ok(())
    }

    /// 版本描述。
    pub fn server_version(&mut self) -> Result<String, DbError> {
        Ok(format!("{} (Elasticsearch)", self.version))
    }

    /// 连接级只读：记下来，之后用户发来的请求逐条过闸。
    pub fn set_read_only(&mut self, read_only: bool) -> Result<(), DbError> {
        self.read_only = read_only;
        Ok(())
    }

    /// 执行一段请求文本（可含多条，空行分隔）；最后一条带结果集的胜出。
    pub fn execute(&mut self, text: &str, max_rows: usize) -> Result<ExecOutcome, DbError> {
        let requests = parse_requests(text)?;
        if requests.is_empty() {
            return Err(DbError::Query("no request to execute".into()));
        }
        let mut outcome = ExecOutcome::default();
        for request in requests {
            if self.read_only && !is_read_only_request(&request.method, &request.path) {
                return Err(read_only_error(&request));
            }
            let reply =
                self.send_with_opaque(&request.method, &request.path, request.body.as_deref())?;
            let value = reply.json()?;
            let (affected, result) = outcome_of(&value, max_rows)?;
            outcome.affected += affected;
            if let Some(set) = result {
                outcome.result = Some(set);
            }
            outcome.statements_run += 1;
        }
        Ok(outcome)
    }

    /// 可选命名空间清单：**恒空**。
    ///
    /// Elasticsearch 的索引是平铺的，没有 database / schema 这一层（见模块文档）。
    /// 返回空列表而不是编造一个假名字：界面的命名空间下拉只在条目 > 1 时才渲染，
    /// 空清单正好让它消失。
    /// Elasticsearch 没有「库」这一层：索引名就是顶层命名。恒空。
    ///
    /// 返回空而不是造一个假名字：上层据此不画「数据库」节点（见 `Session.databases`
    /// 的用法），界面就不会出现一个点了没反应的空下拉。
    pub fn list_databases(&mut self) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    /// 无库可切：索引名直接写在语句里。空操作。
    pub fn use_database(&mut self, _name: &str) -> Result<(), DbError> {
        Ok(())
    }

    pub fn list_schemas(&mut self) -> Result<Vec<String>, DbError> {
        Ok(Vec::new())
    }

    /// 索引 / 别名的目录。
    ///
    /// 一次 `GET /<pattern>/_alias` 同时拿到索引与别名（不用 `_cat/indices`——那条要
    /// `monitor` 权限）。`expand_wildcards=all` 让隐藏索引（`.` 开头，`.security` /
    /// `.kibana` 之类）也列出来——与 ClickHouse 保留 `system`、MongoDB 保留
    /// `admin/config/local` 同一取向：排障时挡住的正是最常要看的东西。
    ///
    /// `schema` 为 `None` 时用档案里的缺省索引 / 模式；两者都空 = 不限（全部索引）。
    pub fn catalog(&mut self, schema: Option<&str>) -> Result<Vec<CatalogTable>, DbError> {
        let pattern = resolve_pattern(schema, &self.endpoint.default_index);
        let path = format!("{}/_alias?expand_wildcards=all", pattern_path(pattern));
        let value = self.endpoint.request("GET", &path, None, None)?.json()?;
        Ok(catalog_of(&value))
    }

    /// 索引的列：mapping 里的字段路径递归展平，**首位是虚拟列 `_id`**。
    pub fn table_columns(&mut self, table: &TableRef) -> Result<Vec<Column>, DbError> {
        let path = format!("/{}/_mapping", url_encode(&index_of(table)?));
        let value = self.endpoint.request("GET", &path, None, None)?.json()?;
        Ok(columns_with_id(&flatten_mappings(&value)))
    }

    /// 一页文档。
    ///
    /// 列清单来自 mapping（与 [`Self::table_columns`] 同一份），所以网格的列是稳定的；
    /// 排序按列类型过滤（`_id` 不可排，见模块文档），过滤后为空退回 `_doc`。
    /// 多取一条探测「还有下一页」，而不是靠「正好 limit 条」猜。
    ///
    /// `filters` 非空时换成真正的查询 DSL（见模块文档「过滤」），不再恒发 `match_all`；
    /// 命中的是**过滤后**的文档集合，`from` / `size` 随之作用于该集合。
    pub fn table_page(
        &mut self,
        table: &TableRef,
        limit: usize,
        offset: u64,
        order_by: &[OrderBy],
        filters: &[TableFilter],
    ) -> Result<ResultSet, DbError> {
        let columns = self.table_columns(table)?;
        let types = self.column_types(table);
        let body = serde_json::json!({
            "query": filter_clause(filters, &types)?,
            "from": offset,
            "size": limit.saturating_add(1),
            "sort": sort_clause(order_by, &columns),
            // 关掉总数统计：网格只要一页，精确总数是白花的开销。
            "track_total_hits": false,
        });
        let text = encode(&body)?;
        let path = format!("/{}/_search", url_encode(&index_of(table)?));
        let value = self.send_with_opaque("POST", &path, Some(&text))?.json()?;
        Ok(page_of(hits_of(&value), &columns, limit))
    }

    /// 主键：**恒 `_id`**（见模块文档）。
    pub fn table_primary_key(&mut self, _table: &TableRef) -> Result<Vec<String>, DbError> {
        Ok(vec![ID_COLUMN.to_owned()])
    }

    /// 改一格：`POST /<index>/_update/<id>` + `{"doc": {列: 值}}`。
    ///
    /// 返回 1 / 0 = 命中 / 没找到。`noop`（值本来就是这个）**也计命中**——否则界面会把
    /// 「改成功但没变化」误报成「行没找到，请刷新」（与 MongoDB 那条同口径）。
    ///
    /// 值按列类型亲和转换（见 [`bind_value`]）。⚠️ `_id` 是虚拟列，改它要重建文档，
    /// 本条驱动直接拒绝。
    pub fn update_cell(
        &mut self,
        table: &TableRef,
        pk: &PkValues,
        column: &str,
        value: Option<&str>,
    ) -> Result<u64, DbError> {
        self.ensure_writable()?;
        if column == ID_COLUMN {
            return Err(id_is_not_editable());
        }
        let id = id_of(pk)?;
        let types = self.column_types(table);
        let bound = bind_value(value, types.get(column).map(String::as_str))?;
        let body = Json::Object(Map::from_iter([(
            "doc".to_owned(),
            Json::Object(Map::from_iter([(column.to_owned(), bound)])),
        )]));
        let text = encode(&body)?;
        let path = format!(
            "/{}/_update/{}?retry_on_conflict=3",
            url_encode(&index_of(table)?),
            url_encode(&id)
        );
        let value = self.send_with_opaque("POST", &path, Some(&text))?.json()?;
        Ok(hit_of_update(&value))
    }

    /// 按 `_id` 删一行（`DELETE /<index>/_doc/<id>`）。
    pub fn delete_rows(&mut self, table: &TableRef, pk: &PkValues) -> Result<u64, DbError> {
        self.ensure_writable()?;
        let path = format!(
            "/{}/_doc/{}",
            url_encode(&index_of(table)?),
            url_encode(&id_of(pk)?)
        );
        let value = self.endpoint.request("DELETE", &path, None, None)?.json()?;
        Ok(match result_of(&value) {
            // 有些版本只回 `_version` / `result` 组合，取不到就按「删掉了」计——
            // 服务端已经 2xx，说没删反而更容易误导。
            Some("not_found") => 0,
            _ => 1,
        })
    }

    /// 删多行：**一条** `_bulk`（每条 action 各自原子，见模块文档「没有事务」）。
    pub fn delete_row_set(&mut self, table: &TableRef, keys: &[PkValues]) -> Result<u64, DbError> {
        self.ensure_writable()?;
        if keys.is_empty() {
            return Ok(0);
        }
        let index = index_of(table)?;
        let lines = delete_lines(&index, keys)?;
        Ok(self.bulk(&lines)?.2)
    }

    /// 改多格：**一条** `_bulk` 的 `update` action。
    ///
    /// 每格一个 action（不是按（列, 值）分组）：ES 每格的新值本来就各不相同，分组省不下
    /// 多少，而 `_update_by_query` 要开脚本（有些集群禁了）。见模块文档「没有事务」。
    pub fn update_cells(&mut self, table: &TableRef, edits: &[CellEdit]) -> Result<u64, DbError> {
        self.ensure_writable()?;
        if edits.is_empty() {
            return Ok(0);
        }
        let index = index_of(table)?;
        let types = self.column_types(table);
        let lines = edit_lines(&index, &types, edits)?;
        Ok(self.bulk(&lines)?.0)
    }

    /// 一次提交「改格 + 新增行」：合成**一条** `_bulk`。
    ///
    /// ⚠️ **不是事务**：`_bulk` 逐条 action 独立提交，中途失败时前面已经生效（与
    /// MongoDB 那条的边界同构）。调用方按返回的影响行数核对刷新。
    pub fn apply_batch(
        &mut self,
        table: &TableRef,
        edits: &[CellEdit],
        inserts: &[InsertRow],
    ) -> Result<BatchOutcome, DbError> {
        self.ensure_writable()?;
        if edits.is_empty() && inserts.is_empty() {
            return Ok(BatchOutcome::default());
        }
        let index = index_of(table)?;
        let types = self.column_types(table);
        let mut lines = edit_lines(&index, &types, edits)?;
        lines.push_str(&insert_lines(&index, &types, inserts)?);
        let (updated, created, _) = self.bulk(&lines)?;
        Ok(BatchOutcome {
            updated,
            inserted: created,
        })
    }

    /// 批量插入：一条 `_bulk` 的 `index` action。
    ///
    /// 列里有 `_id` 且填了值 → 用它当文档 id（这是「导入一份带 id 的数据」的自然期待）；
    /// 否则交给服务端生成（`index` action 不带 `_id` 就会自动生成）。
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
        // 共享列清单 → 逐行 `InsertRow`（[`insert_lines`] 的输入形状，与暂存式编辑一致）。
        let per_row: Vec<InsertRow> = rows
            .iter()
            .map(|row| InsertRow {
                columns: columns.to_vec(),
                values: row.clone(),
            })
            .collect();
        let index = index_of(table)?;
        let types = self.column_types(table);
        let lines = insert_lines(&index, &types, &per_row)?;
        Ok(self.bulk(&lines)?.1)
    }

    /// 索引的「DDL」：Elasticsearch 没有原文出口，这里由 settings + mapping
    /// **重构**一条等价的建索引请求（`PUT /<索引>`）。
    ///
    /// ⚠️ 首行注释**用英文**是有意的：内核不吐成品中文（`duck-db` 不参与 i18n，
    /// 中文一律由界面层 `tr(...)` 给），而这段文本是要**粘进编辑器**的草稿。
    pub fn table_ddl(&mut self, table: &TableRef) -> Result<String, DbError> {
        let index = index_of(table)?;
        let path = format!("/{}", url_encode(&index));
        let value = self.endpoint.request("GET", &path, None, None)?.json()?;
        Ok(ddl_of(&index, &value))
    }

    /// 索引上的「索引」：**恒空**。
    ///
    /// Elasticsearch 没有独立的索引对象——**mapping 本身就是索引**（每个字段按类型建
    /// 自己的倒排表）。硬把「字段清单」塞进这个卡片会与旁边的「列」卡重复且误导，所以
    /// 老实返回空：界面显示「没有索引」，这在搜索库里是**正确**的答案。想看字段能不能
    /// 排序 / 聚合，右栏的「列」卡已经给了类型。
    pub fn table_indexes(&mut self, _table: &TableRef) -> Result<Vec<TableIndex>, DbError> {
        Ok(Vec::new())
    }

    /// 文档数：走 `_count`（精确计数；不带查询时服务端直接用段内文档数，不扫全量）。
    pub fn table_row_count(&mut self, table: &TableRef) -> Result<RowCount, DbError> {
        let path = format!("/{}/_count", url_encode(&index_of(table)?));
        let value = self.endpoint.request("GET", &path, None, None)?.json()?;
        let count = value.get("count").and_then(Json::as_u64).unwrap_or(0);
        Ok(RowCount::Exact(count))
    }

    // ---- 内部 ----

    /// 发一条请求，同时登记 `X-Opaque-Id` 供取消寻址；返回后立刻撤销登记。
    fn send_with_opaque(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Reply, DbError> {
        let opaque = next_opaque_id();
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = Some(opaque.clone());
        }
        let reply = self.endpoint.request(method, path, body, Some(&opaque));
        // 无论成败都要撤销：留着一个已结束的 id 会让下一次取消打到空气上。
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = None;
        }
        reply
    }

    /// 发一条 `_bulk`（与单条请求一样登记 `X-Opaque-Id`，这样批量写也能被「停止」打到）。
    fn bulk(&self, lines: &str) -> Result<(u64, u64, u64), DbError> {
        let opaque = next_opaque_id();
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = Some(opaque.clone());
        }
        let outcome = self.endpoint.bulk_send(lines, Some(&opaque));
        if let Ok(mut slot) = self.inflight.lock() {
            *slot = None;
        }
        outcome
    }

    /// 列的 JSON 类型（`列 → mapping 类型`）；取不到时是空表（[`bind_value`] 走启发式）。
    fn column_types(&self, table: &TableRef) -> HashMap<String, String> {
        // 这里刻意**吞掉错误**：类型拿不到只影响「值怎么绑定」，不该让一次改格提前失败。
        // 真正的失败会在随后那条 `_update` / `_bulk` 上如实报出来。
        let Ok(index) = index_of(table) else {
            return HashMap::new();
        };
        let path = format!("/{}/_mapping", url_encode(&index));
        let Ok(reply) = self.endpoint.request("GET", &path, None, None) else {
            return HashMap::new();
        };
        let Ok(value) = reply.json() else {
            return HashMap::new();
        };
        flatten_mappings(&value)
            .into_iter()
            .map(|column| (column.name, column.type_name))
            .collect()
    }

    /// 只读档的软拦截（typed 方法走这里；用户发来的请求走 [`Self::execute`] 的闸）。
    fn ensure_writable(&self) -> Result<(), DbError> {
        if self.read_only {
            return Err(DbError::Unsupported(
                "this connection is read-only; writes are blocked before reaching the cluster"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// 统一的「只读档拦下了这条请求」文案。
fn read_only_error(request: &Request) -> DbError {
    DbError::Unsupported(format!(
        "this connection is read-only; {} {} is a write request",
        request.method, request.path
    ))
}

/// 统一的「文档 id 不能原地改」文案。
fn id_is_not_editable() -> DbError {
    DbError::Unsupported(
        "the document id cannot be edited in place; delete and re-index the document instead"
            .into(),
    )
}

/// 拼端点 + 建 agent。
fn build_endpoint(cfg: &ConnectionConfig, password: Option<&str>) -> Endpoint {
    let port = cfg.port.unwrap_or(DEFAULT_PORT);
    let (scheme, insecure) = match cfg.tls {
        TlsMode::Disable => ("http", false),
        TlsMode::Trust => ("https", false),
        TlsMode::DangerAcceptInvalid => ("https", true),
    };
    let host = cfg.host.trim();
    // IPv6 字面量要带方括号，否则 `[::1]:9200` 拼出来是歧义的。
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };

    let user = cfg.user.trim();
    // 有些单机 / 开发集群没开安全 → 不填用户名就不发认证头（发了反而会被拒）。
    let auth = (!user.is_empty()).then(|| {
        let raw = format!("{user}:{}", password.unwrap_or_default());
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        format!("Basic {encoded}")
    });

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

    Endpoint {
        base: format!("{scheme}://{authority}"),
        default_index: cfg.database.trim().to_owned(),
        auth,
        agent: ureq::Agent::new_with_config(builder.build()),
    }
}

/// 缺省索引 / 索引模式的回落（**纯函数**，脱开连接单测）：
/// 调用方给的 → 档案里配的 → `None`（= 不限，列全部索引）。
///
/// 空串与纯空白一律当「没给」（「填了空格」不该变成一个叫 `" "` 的模式）。
fn resolve_pattern<'a>(schema: Option<&'a str>, configured: &'a str) -> Option<&'a str> {
    let clean = |value: &'a str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    schema.and_then(clean).or_else(|| clean(configured))
}

/// 索引模式 → 路径段：`None` 时是 `_all`（Elasticsearch 的「全部索引」名字）。
///
/// ⚠️ `_all` 与空段不能混：`//_alias` 是坏路径，而 `/_alias` 的语义在版本间不完全一致，
/// 不如显式写 `_all` 稳。
fn pattern_path(pattern: Option<&str>) -> String {
    match pattern {
        Some(text) => format!("/{}", encode_pattern(text)),
        None => "/_all".to_owned(),
    }
}

/// 索引模式的成分编码：**只编码字面部分，通配符原样留下**。
///
/// `logs-*` 里的 `*` / `?` / `,`（多索引列表）是 Elasticsearch 的**模式语法**，不是数据，
/// 编码成 `%2A` 就算服务端解回来，也等于把语义押在中间层的解码行为上。所以按模式语法
/// 切开、逐段走 [`url_encode`]，元字符自己保留。
fn encode_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut literal = String::new();
    for ch in pattern.chars() {
        if matches!(ch, '*' | '?' | ',') {
            out.push_str(&url_encode(&literal));
            literal.clear();
            out.push(ch);
        } else {
            literal.push(ch);
        }
    }
    out.push_str(&url_encode(&literal));
    out
}

/// 从 [`TableRef`] 取索引名。
///
/// `schema` 若有值优先（界面传下来的落点），否则用 `name`——ES 没有命名空间层，
/// 目录给的 `CatalogTable.schema` 恒为 `None`，所以实际走的是 `name`。
fn index_of(table: &TableRef) -> Result<String, DbError> {
    let index = table
        .schema
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| table.name.trim());
    if index.is_empty() {
        return Err(DbError::Config("index name is empty".into()));
    }
    Ok(index.to_owned())
}

/// `_id` 从主键值里取出来。
fn id_of(pk: &PkValues) -> Result<String, DbError> {
    let Some((_, value)) = pk.iter().find(|(name, _)| name == ID_COLUMN) else {
        return Err(DbError::Query(
            "document id is missing from the primary key".into(),
        ));
    };
    if value.is_empty() {
        return Err(DbError::Query("document id is empty".into()));
    }
    Ok(value.clone())
}

/// 把一个 JSON 值编码成请求体（编码失败归成 `Query`，不 unwrap）。
fn encode(value: &Json) -> Result<String, DbError> {
    serde_json::to_string(value)
        .map_err(|error| DbError::Query(format!("request encode failed: {error}")))
}

/// 生成一个可寻址的 `X-Opaque-Id`。
///
/// 只要进程内不重复即可（取消只在同一进程的同一连接里寻址），所以用进程 id + 单调计数：
/// 比引入随机数依赖划算（与 ClickHouse 的 `query_id` 同法）。
fn next_opaque_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("duck-{}-{seq}", std::process::id())
}

/// `GET /` 的响应里取版本号。
fn version_of(value: &Json) -> Option<String> {
    value
        .get("version")
        .and_then(|version| version.get("number"))
        .and_then(Json::as_str)
        .map(str::to_owned)
}

/// 从 `GET /_tasks` 的响应里按 `X-Opaque-Id` 认领我们那条任务。
///
/// 这是取消通道的寻址步骤（见 `cancel.rs` 的取消表）：Elasticsearch 的 `_cancel` 要
/// **点名 `nodeId:taskId`**，而那个 id 只在任务列表里——所以先列一遍、按我们发请求时
/// 带上的 `X-Opaque-Id` 找回自己。返回 `None` = 那条请求已经跑完（取消本就该幂等）。
///
/// 响应形状：
/// `{"nodes": {"<nodeId>": {"tasks": {"<taskId>": {"headers": {"X-Opaque-Id": "…"}}}}}}`
pub(crate) fn find_task_by_opaque(value: &Json, opaque: &str) -> Option<String> {
    let nodes = value.get("nodes")?.as_object()?;
    for (node_id, node) in nodes {
        let Some(tasks) = node.get("tasks").and_then(Json::as_object) else {
            continue;
        };
        for (task_id, task) in tasks {
            let Some(headers) = task.get("headers").and_then(Json::as_object) else {
                continue;
            };
            // 头名大小写不敏感（HTTP 头本来就该如此）。
            let matched = headers.iter().any(|(name, header)| {
                name.eq_ignore_ascii_case("x-opaque-id")
                    && header.as_str().is_some_and(|text| text == opaque)
            });
            if matched {
                return Some(format!("{node_id}:{task_id}"));
            }
        }
    }
    None
}

/// `GET /<pattern>/_alias` 的响应 → 目录对象。
///
/// 形状：`{"idx": {"aliases": {"alias-name": {}}}}`。索引是表，别名是视图（见模块文档）。
fn catalog_of(value: &Json) -> Vec<CatalogTable> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let mut tables = Vec::new();
    let mut aliases = BTreeSet::new();
    for (index, entry) in map {
        if index.trim().is_empty() {
            continue;
        }
        tables.push(CatalogTable {
            name: index.clone(),
            kind: TableKind::Table,
            // ES 没有命名空间层：schema 恒为 `None`，`TableRef` 于是退化成裸索引名。
            schema: None,
        });
        if let Some(names) = entry.get("aliases").and_then(Json::as_object) {
            for name in names.keys() {
                if !name.trim().is_empty() {
                    aliases.insert(name.clone());
                }
            }
        }
    }
    for name in aliases {
        tables.push(CatalogTable {
            name,
            kind: TableKind::View,
            schema: None,
        });
    }
    tables.sort_by(|left, right| left.name.cmp(&right.name));
    tables
}

/// 列清单首位补上虚拟列 `_id`（见模块文档）。
fn columns_with_id(columns: &[Column]) -> Vec<Column> {
    let mut all = Vec::with_capacity(columns.len() + 1);
    all.push(Column {
        name: ID_COLUMN.to_owned(),
        type_name: ID_TYPE.to_owned(),
    });
    all.extend_from_slice(columns);
    all
}

/// `_mapping` 响应 → 列清单（**按路径排序**，不含虚拟列 `_id`）。
///
/// 展平规则见模块文档：`properties` 递归（`user.name`）、`fields` 多字段跳过、
/// `runtime` 字段跳过。多个索引（通配模式）时取并集——同一路径不同类型时**先到者胜**
/// （通配到类型冲突的索引本来就该让用户改用具体索引）。
fn flatten_mappings(value: &Json) -> Vec<Column> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    for entry in map.values() {
        let Some(properties) = entry.get("mappings").and_then(|m| m.get("properties")) else {
            continue;
        };
        flatten_properties(properties, "", &mut found);
    }
    found
        .into_iter()
        .map(|(name, type_name)| Column { name, type_name })
        .collect()
}

/// 递归展平 `properties`。
fn flatten_properties(properties: &Json, prefix: &str, out: &mut BTreeMap<String, String>) {
    let Some(map) = properties.as_object() else {
        return;
    };
    for (name, spec) in map {
        if name.trim().is_empty() {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        // `object` / `nested`：继续往下钻，叶子才是 `_source` 里可展示的一格。
        if let Some(inner) = spec.get("properties") {
            flatten_properties(inner, &path, out);
            continue;
        }
        let type_name = spec.get("type").and_then(Json::as_str).unwrap_or("object");
        out.entry(path).or_insert_with(|| type_name.to_owned());
    }
}

/// `_search` 响应里的 `hits.hits` 数组。
fn hits_of(value: &Json) -> &[Json] {
    value
        .get("hits")
        .and_then(|hits| hits.get("hits"))
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// 搜索命中 → 一页结果（首列是 `_id`，其后按 `columns` 从 `_source` 取路径值）。
fn page_of(hits: &[Json], columns: &[Column], limit: usize) -> ResultSet {
    let mut rows = Vec::with_capacity(hits.len().min(limit));
    let mut truncated = false;
    for hit in hits {
        if rows.len() >= limit {
            truncated = true;
            break;
        }
        let source = hit.get("_source").unwrap_or(&Json::Null);
        rows.push(
            columns
                .iter()
                .map(|column| {
                    if column.name == ID_COLUMN {
                        let id = hit.get("_id").and_then(Json::as_str).unwrap_or_default();
                        return Value::Text(id.to_owned());
                    }
                    match lookup_path(source, &column.name) {
                        Some(found) => map_json(&found, &column.type_name),
                        None => Value::Null,
                    }
                })
                .collect(),
        );
    }
    ResultSet {
        columns: columns.to_vec(),
        rows,
        truncated,
    }
}

/// 按点分路径取值（支持数组：数组元素上继续按名字找，结果收成一个 JSON 数组）。
fn lookup_path(value: &Json, path: &str) -> Option<Json> {
    let parts: Vec<&str> = path.split('.').collect();
    walk(value, &parts)
}

/// [`lookup_path`] 的递归实现。
fn walk(value: &Json, parts: &[&str]) -> Option<Json> {
    let Some((head, rest)) = parts.split_first() else {
        return Some(value.clone());
    };
    match value {
        Json::Object(map) => map.get(*head).and_then(|next| walk(next, rest)),
        // 数组：拿**同一条剩余路径**去每个元素上找（元素才是对象），收集成数组。
        Json::Array(items) => {
            let collected: Vec<Json> = items.iter().filter_map(|item| walk(item, parts)).collect();
            (!collected.is_empty()).then_some(Json::Array(collected))
        }
        _ => None,
    }
}

/// 排序子句（**纯函数**，脱开连接单测）。
///
/// 只收「类型可排」的档；`_id` 被类型过滤自动挡下（它的 `type_name` 是 `_id`，不在
/// [`SORTABLE_TYPES`] 里）；过滤后为空退回 `_doc`（见模块文档「排序与分页」）。
fn sort_clause(order_by: &[OrderBy], columns: &[Column]) -> Json {
    let sortable = |column: &str| -> bool {
        columns
            .iter()
            .find(|entry| entry.name == column)
            .is_some_and(|entry| SORTABLE_TYPES.contains(&entry.type_name.as_str()))
    };
    let entries: Vec<Json> = order_by
        .iter()
        .filter(|entry| sortable(&entry.column))
        .map(|entry| {
            let direction = if entry.descending { "desc" } else { "asc" };
            Json::Object(Map::from_iter([(
                entry.column.clone(),
                Json::String(direction.to_owned()),
            )]))
        })
        .collect();
    if entries.is_empty() {
        serde_json::json!([{ "_doc": "asc" }])
    } else {
        Json::Array(entries)
    }
}

/// 过滤子句（**纯函数**，脱开连接单测）。
///
/// ES 没有 SQL 的 `WHERE`，改翻成查询 DSL（见模块文档「过滤」）。没有条件时仍返回
/// `match_all`——让调用方少一个分支，也保持「恒有一个 `query`」的请求形状。
fn filter_clause(
    filters: &[TableFilter],
    types: &HashMap<String, String>,
) -> Result<Json, DbError> {
    let mut must: Vec<Json> = Vec::with_capacity(filters.len());
    for filter in filters {
        // 列名去空白后必须非空：空列名会拼出 `term: {"": …}` 这种无意义的子句，
        // 服务端多半按「零命中」处理——那是最难发现的一类错，就地拒掉。
        let column = filter.column.trim();
        if column.is_empty() {
            return Err(DbError::Query("filter column must not be empty".into()));
        }
        let type_name = types.get(column).map(String::as_str);
        must.push(match filter.op {
            FilterOp::Eq => term_clause(column, filter_value(filter, type_name)?),
            // 不等 = 「不是这个值」，用 must_not 包一层（`term` 本身只有正面语义）。
            FilterOp::NotEq => not_clause(term_clause(column, filter_value(filter, type_name)?)),
            FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => {
                let key = match filter.op {
                    FilterOp::Gt => "gt",
                    FilterOp::Gte => "gte",
                    FilterOp::Lt => "lt",
                    _ => "lte",
                };
                let bounds = Json::Object(Map::from_iter([(
                    key.to_owned(),
                    filter_value(filter, type_name)?,
                )]));
                Json::Object(Map::from_iter([(
                    "range".to_owned(),
                    Json::Object(Map::from_iter([(column.to_owned(), bounds)])),
                )]))
            }
            FilterOp::Like => {
                let pattern = filter
                    .value
                    .as_deref()
                    .ok_or_else(|| missing_value(filter))?;
                let spec = Json::Object(Map::from_iter([(
                    "value".to_owned(),
                    Json::String(like_to_wildcard(pattern)),
                )]));
                Json::Object(Map::from_iter([(
                    "wildcard".to_owned(),
                    Json::Object(Map::from_iter([(column.to_owned(), spec)])),
                )]))
            }
            // `IS NULL` 在 ES 里的对应物是「这个字段没有值」——`exists` 的反面。
            // 注意 ES 把显式的 `null` 也当作「不存在」（不索引 null），语义正好对上。
            FilterOp::IsNull => not_clause(exists_clause(column)),
            FilterOp::IsNotNull => exists_clause(column),
        });
    }
    if must.is_empty() {
        return Ok(serde_json::json!({ "match_all": {} }));
    }
    Ok(Json::Object(Map::from_iter([(
        "bool".to_owned(),
        Json::Object(Map::from_iter([("must".to_owned(), Json::Array(must))])),
    )])))
}

/// 单个字段的 `term` 子句。
fn term_clause(column: &str, value: Json) -> Json {
    Json::Object(Map::from_iter([(
        "term".to_owned(),
        Json::Object(Map::from_iter([(column.to_owned(), value)])),
    )]))
}

/// 单个字段的 `exists` 子句。
fn exists_clause(column: &str) -> Json {
    Json::Object(Map::from_iter([(
        "exists".to_owned(),
        Json::Object(Map::from_iter([(
            "field".to_owned(),
            Json::String(column.to_owned()),
        )])),
    )]))
}

/// 把一个子句取反（`bool.must_not`，单个元素）。
fn not_clause(clause: Json) -> Json {
    Json::Object(Map::from_iter([(
        "bool".to_owned(),
        Json::Object(Map::from_iter([(
            "must_not".to_owned(),
            Json::Array(vec![clause]),
        )])),
    )]))
}

/// 过滤条件的值 → JSON 值（缺值时给出带列名的报错）。
fn filter_value(filter: &TableFilter, type_name: Option<&str>) -> Result<Json, DbError> {
    let value = filter
        .value
        .as_deref()
        .ok_or_else(|| missing_value(filter))?;
    bind_value(Some(value), type_name)
}

/// 「这个操作符需要一个值，但没给」的统一报错。
fn missing_value(filter: &TableFilter) -> DbError {
    DbError::Query(format!("filter on {:?} needs a value", filter.column))
}

/// SQL `LIKE` 模式 → ES `wildcard` 模式。
///
/// `%` → `*`（任意长度）、`_` → `?`（单个字符），与 SQL 的语义逐字对应；用户模式里
/// **本来就是通配符的 `*` `?` `\` 先转义**，否则一个字面量星号会变成通配、把过滤结果
/// 悄悄放宽。`REGEX_ESCAPES` 那种「正则元字符」清单在这里不适用——wildcard 只有三个
/// 元字符，多转义会有反效果。
fn like_to_wildcard(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        match ch {
            '%' => out.push('*'),
            '_' => out.push('?'),
            '*' | '?' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// 一个 JSON 单元格 → 统一值模型（按 mapping 类型名分派）。
///
/// 纪律：**浮点只用于本来就是二进制的类型**（`double` / `float` / `half_float`）；
/// `scaled_float` 走文本（它是有精度的定点数，本仓不让十进制过 `f64`）。
/// ⚠️ **类型未知时按 JSON 自己的形状来**（整数给整数、小数给浮点）——那些值来自
/// 控制台查询或 `_cat` 之类的响应，本来就没有 mapping 可依，把它一律降成文本只会让
/// 网格失去数值语义。
fn map_json(value: &Json, type_name: &str) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Bool(*flag),
        Json::String(text) => match type_name {
            "date" | "date_nanos" => Value::DateTime(text.clone()),
            _ => Value::Text(text.clone()),
        },
        Json::Number(number) => match type_name {
            "long" | "integer" | "short" | "byte" => match number.as_i64() {
                Some(parsed) => Value::Int(parsed),
                None => Value::Text(number.to_string()),
            },
            "unsigned_long" => match number.as_u64() {
                Some(parsed) => Value::UInt(parsed),
                None => Value::Text(number.to_string()),
            },
            "double" | "float" | "half_float" => match number.as_f64() {
                Some(parsed) => Value::Float(parsed),
                None => Value::Text(number.to_string()),
            },
            // 定点数（scaled_float，以及 mapping 里未来的定点类型）：不进 f64。
            "scaled_float" => Value::Text(number.to_string()),
            // 类型未知：JSON 已经把整数与小数分开了，照抄它的判断。
            _ => match (number.as_i64(), number.as_u64(), number.as_f64()) {
                (Some(parsed), _, _) => Value::Int(parsed),
                (None, Some(parsed), _) => Value::UInt(parsed),
                (None, None, Some(parsed)) => Value::Float(parsed),
                (None, None, None) => Value::Text(number.to_string()),
            },
        },
        // 对象 / 数组（keyword 数组、object 字段…）：落 JSON 文本，信息不丢。
        other => match serde_json::to_string(other) {
            Ok(text) => Value::Json(text),
            Err(_) => Value::Null,
        },
    }
}

/// 文本 → JSON 值（写路径的类型亲和转换）。
///
/// `None` = 写 `null`（ES 允许 `_source` 里存 null）。类型来自 mapping；取不到类型时走
/// 启发式（[`untyped_bind`]）。
///
/// ⚠️ **类型已知时不做宽容回落**：`long` 列上填 `abc` 直接报错，而不是偷偷写成字符串
/// ——后者会在服务端引发 `mapper_parsing_exception`，错误信息离用户的操作更远。
fn bind_value(text: Option<&str>, type_name: Option<&str>) -> Result<Json, DbError> {
    let Some(raw) = text else {
        return Ok(Json::Null);
    };
    let trimmed = raw.trim();
    let Some(type_name) = type_name else {
        return Ok(untyped_bind(trimmed));
    };
    match type_name {
        "boolean" => match trimmed {
            "true" | "1" => Ok(Json::Bool(true)),
            "false" | "0" => Ok(Json::Bool(false)),
            other => Err(DbError::Query(format!(
                "expected a boolean (true/false), got {other:?}"
            ))),
        },
        "long" | "integer" | "short" | "byte" | "unsigned_long" => trimmed
            .parse::<i64>()
            .map(Json::from)
            .or_else(|_| trimmed.parse::<u64>().map(Json::from))
            .map_err(|_| DbError::Query(format!("expected an integer, got {trimmed:?}"))),
        "double" => trimmed
            .parse::<f64>()
            .map(Json::from)
            .map_err(|_| DbError::Query(format!("expected a number, got {trimmed:?}"))),
        // date / keyword / text / ip / 以及其它一切：原样当字符串（日期交给 ES 解析）。
        _ => Ok(Json::String(raw.to_owned())),
    }
}

/// 类型未知时的启发式绑定（见 [`bind_value`]）。
fn untyped_bind(trimmed: &str) -> Json {
    match trimmed {
        "true" => return Json::Bool(true),
        "false" => return Json::Bool(false),
        _ => {}
    }
    if let Ok(parsed) = trimmed.parse::<i64>() {
        return Json::from(parsed);
    }
    if let Ok(parsed) = trimmed.parse::<f64>() {
        return Json::from(parsed);
    }
    // 看着像 JSON 才试着解析（`.ok()` 兜住失败，不用 let-chain）。
    let looks_like_json = trimmed.starts_with('{') || trimmed.starts_with('[');
    let parsed = looks_like_json
        .then(|| serde_json::from_str::<Json>(trimmed).ok())
        .flatten();
    if let Some(value) = parsed {
        return value;
    }
    Json::String(trimmed.to_owned())
}

/// `POST /<index>/_update/<id>` 的响应 → 命中数。
///
/// `updated` 与 `noop` 都算命中（值本来就是这个也报成功，与 MongoDB 的 `matched_count`
/// 同口径）；`not_found` / 缺字段算 0。
fn hit_of_update(value: &Json) -> u64 {
    match result_of(value) {
        Some("updated" | "noop") => 1,
        _ => 0,
    }
}

/// 响应里的 `result` 字段。
fn result_of(value: &Json) -> Option<&str> {
    value.get("result").and_then(Json::as_str)
}

/// `_bulk` 响应汇总：改 / 增 / 删各自计数 + 失败原因。
fn bulk_counts(value: &Json) -> (u64, u64, u64, Vec<String>) {
    let mut updated = 0_u64;
    let mut created = 0_u64;
    let mut deleted = 0_u64;
    let mut errors = Vec::new();
    let Some(items) = value.get("items").and_then(Json::as_array) else {
        return (0, 0, 0, errors);
    };
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        // 每条 item 恰好一个键（action 名），值是结果对象。
        let Some((action, outcome)) = map.iter().next() else {
            continue;
        };
        if let Some(error) = outcome.get("error") {
            let reason = error
                .get("reason")
                .and_then(Json::as_str)
                .or_else(|| error.get("type").and_then(Json::as_str))
                .unwrap_or("unknown error");
            errors.push(reason.to_owned());
            continue;
        }
        match action.as_str() {
            "update" => updated += 1,
            "index" | "create" => created += 1,
            "delete" => deleted += 1,
            _ => {}
        }
    }
    (updated, created, deleted, errors)
}

/// 一条响应 → （影响行数, 结果集）。三种形状见模块文档。
fn outcome_of(value: &Json, max_rows: usize) -> Result<(u64, Option<ResultSet>), DbError> {
    match value {
        Json::Null => Ok((0, None)),
        Json::Array(items) => Ok((0, Some(rows_of_array(items, max_rows)))),
        Json::Object(_) => {
            if value.get("hits").is_some() {
                let (columns, rows, truncated) = rows_of_hits(hits_of(value), max_rows);
                return Ok((
                    0,
                    Some(ResultSet {
                        columns,
                        rows,
                        truncated,
                    }),
                ));
            }
            // `_bulk` 这类"只有计数"的响应：给出影响行数，不给结果集。
            if value.get("items").is_some() {
                let (updated, created, deleted, errors) = bulk_counts(value);
                if let Some(first) = errors.first() {
                    return Err(DbError::Query(format!(
                        "{} action(s) applied, {} failed — first: {first}",
                        updated + created + deleted,
                        errors.len()
                    )));
                }
                return Ok((updated + created + deleted, None));
            }
            Ok((0, Some(single_row(value))))
        }
        // 标量（很少见）：一列一行。
        other => Ok((
            0,
            Some(ResultSet {
                columns: vec![Column {
                    name: "value".to_owned(),
                    type_name: String::new(),
                }],
                rows: vec![vec![map_json(other, "")]],
                truncated: false,
            }),
        )),
    }
}

/// 对象数组 → 结果集（列取并集，顺序按**首次出现**，这样 `_cat` 的列序是服务端给的序）。
fn rows_of_array(items: &[Json], max_rows: usize) -> ResultSet {
    let mut columns: Vec<Column> = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(items.len().min(max_rows));
    let mut truncated = false;
    for item in items {
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut row: Vec<Value> = Vec::with_capacity(columns.len());
        match item {
            Json::Object(map) => {
                for (name, cell) in map {
                    let index = match columns.iter().position(|column| &column.name == name) {
                        Some(index) => index,
                        None => {
                            columns.push(Column {
                                name: name.clone(),
                                type_name: String::new(),
                            });
                            columns.len() - 1
                        }
                    };
                    if row.len() <= index {
                        row.resize(index + 1, Value::Null);
                    }
                    row[index] = map_json(cell, "");
                }
            }
            other => {
                if columns.is_empty() {
                    columns.push(Column {
                        name: "value".to_owned(),
                        type_name: String::new(),
                    });
                }
                row = vec![map_json(other, "")];
            }
        }
        rows.push(row);
    }
    // 后出现的列会把前面的行"变短"——统一补齐，否则按下标取值会越界。
    pad_rows(&mut rows, columns.len());
    ResultSet {
        columns,
        rows,
        truncated,
    }
}

/// 搜索命中 → 结果集。列 = `_id` + 各命中 `_source` 顶层键的并集（首次出现序）。
fn rows_of_hits(hits: &[Json], max_rows: usize) -> (Vec<Column>, Vec<Vec<Value>>, bool) {
    let mut columns = vec![Column {
        name: ID_COLUMN.to_owned(),
        type_name: ID_TYPE.to_owned(),
    }];
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(hits.len().min(max_rows));
    let mut truncated = false;
    for hit in hits {
        if rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut row: Vec<Value> = vec![Value::Text(
            hit.get("_id")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
        )];
        if let Some(map) = hit.get("_source").and_then(Json::as_object) {
            for (name, cell) in map {
                let index = match columns.iter().position(|column| &column.name == name) {
                    Some(index) => index,
                    None => {
                        columns.push(Column {
                            name: name.clone(),
                            type_name: String::new(),
                        });
                        columns.len() - 1
                    }
                };
                if row.len() <= index {
                    row.resize(index + 1, Value::Null);
                }
                row[index] = map_json(cell, "");
            }
        }
        rows.push(row);
    }
    // 见 [`rows_of_array`] 的同名说明。
    pad_rows(&mut rows, columns.len());
    (columns, rows, truncated)
}

/// 把所有行补齐到 `width` 列（后出现的列会让先前的行"变短"）。
fn pad_rows(rows: &mut [Vec<Value>], width: usize) {
    for row in rows.iter_mut() {
        if row.len() < width {
            row.resize(width, Value::Null);
        }
    }
}

/// 普通对象 → 单行结果集（键当列）——`GET /`、`_count`、`_mapping` 这类都落这里。
fn single_row(value: &Json) -> ResultSet {
    let Some(map) = value.as_object() else {
        return ResultSet::default();
    };
    let columns: Vec<Column> = map
        .keys()
        .map(|name| Column {
            name: name.clone(),
            type_name: String::new(),
        })
        .collect();
    let row: Vec<Value> = map.values().map(|cell| map_json(cell, "")).collect();
    ResultSet {
        columns,
        rows: vec![row],
        truncated: false,
    }
}

/// 一条插入行的文档体（`_id` 单独拿出来做 action 的 `_id`）。
fn document_of(
    row: &InsertRow,
    types: &HashMap<String, String>,
) -> Result<(Json, Option<String>), DbError> {
    if row.columns.len() != row.values.len() {
        return Err(DbError::Query(
            "row width does not match column list".into(),
        ));
    }
    let mut document = Map::new();
    let mut explicit_id: Option<String> = None;
    for (column, cell) in row.columns.iter().zip(row.values.iter()) {
        if column == ID_COLUMN {
            explicit_id = cell.clone().filter(|text| !text.trim().is_empty());
            continue;
        }
        document.insert(
            column.clone(),
            bind_value(cell.as_deref(), types.get(column).map(String::as_str))?,
        );
    }
    Ok((Json::Object(document), explicit_id))
}

/// 拼 `_bulk` 的 NDJSON 行（**纯函数**，脱开连接单测）。
///
/// 每行一个 JSON 文档：action 行 + （除了 `delete`）源文档行。
fn push_bulk_line(lines: &mut String, value: &Json) -> Result<(), DbError> {
    lines.push_str(&encode(value)?);
    lines.push('\n');
    Ok(())
}

/// 删除 action 的 NDJSON。
fn delete_lines(index: &str, keys: &[PkValues]) -> Result<String, DbError> {
    let mut lines = String::new();
    for pk in keys {
        let action = serde_json::json!({ "delete": { "_index": index, "_id": id_of(pk)? } });
        push_bulk_line(&mut lines, &action)?;
    }
    Ok(lines)
}

/// 更新 action 的 NDJSON（每格一个 action）。
fn edit_lines(
    index: &str,
    types: &HashMap<String, String>,
    edits: &[CellEdit],
) -> Result<String, DbError> {
    let mut lines = String::new();
    for edit in edits {
        if edit.column == ID_COLUMN {
            return Err(id_is_not_editable());
        }
        let action = serde_json::json!({
            "update": { "_index": index, "_id": id_of(&edit.pk)? }
        });
        let bound = bind_value(
            edit.value.as_deref(),
            types.get(&edit.column).map(String::as_str),
        )?;
        let doc = Json::Object(Map::from_iter([(
            "doc".to_owned(),
            Json::Object(Map::from_iter([(edit.column.clone(), bound)])),
        )]));
        push_bulk_line(&mut lines, &action)?;
        push_bulk_line(&mut lines, &doc)?;
    }
    Ok(lines)
}

/// 索引 action 的 NDJSON（列里有 `_id` 且填了值就用它当文档 id）。
fn insert_lines(
    index: &str,
    types: &HashMap<String, String>,
    rows: &[InsertRow],
) -> Result<String, DbError> {
    let mut lines = String::new();
    for row in rows {
        if row.columns.is_empty() {
            return Err(DbError::Query("no columns to insert".into()));
        }
        let (document, explicit_id) = document_of(row, types)?;
        let action = match &explicit_id {
            Some(id) => serde_json::json!({ "index": { "_index": index, "_id": id } }),
            None => serde_json::json!({ "index": { "_index": index } }),
        };
        push_bulk_line(&mut lines, &action)?;
        push_bulk_line(&mut lines, &document)?;
    }
    Ok(lines)
}

/// 索引的「DDL」文本（见 [`ElasticsearchConnection::table_ddl`]）。
///
/// 输出是一段可直接粘回编辑器的请求（`PUT /<索引>` + settings + mappings），首行英文
/// 注释说明它是重构品而不是原文。
fn ddl_of(index: &str, value: &Json) -> String {
    let mut out = String::from(
        "// Elasticsearch has no DDL source text; the request below is reconstructed from the index settings and mapping.\n",
    );
    // 通配时响应可能有多个键 → 优先按索引名取，取不到就用第一个。
    let entry = value
        .get(index)
        .or_else(|| value.as_object().and_then(|map| map.values().next()));

    let Some(entry) = entry else {
        out.push_str(&format!("PUT /{index}\n"));
        return out;
    };

    let mut body = Map::new();
    if let Some(settings) = entry.get("settings").and_then(|value| value.get("index")) {
        let mut cleaned = Map::new();
        if let Some(map) = settings.as_object() {
            for (key, setting) in map {
                if !VOLATILE_SETTING_KEYS.contains(&key.as_str()) {
                    cleaned.insert(key.clone(), setting.clone());
                }
            }
        }
        body.insert("settings".to_owned(), Json::Object(cleaned));
    }
    if let Some(mappings) = entry.get("mappings") {
        body.insert("mappings".to_owned(), mappings.clone());
    }

    out.push_str(&format!("PUT /{index}\n"));
    match serde_json::to_string_pretty(&Json::Object(body)) {
        Ok(pretty) => out.push_str(&pretty),
        Err(_) => out.push_str("{}"),
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求行解析：方法省略时按 GET；路径补前导 `/`；多余 token 忽略（curl 同法）。
    #[test]
    fn request_line_is_lenient_but_explicit() {
        let get = parse_request_line("GET /logs-*/_search").expect("合法");
        assert_eq!(get.method, "GET");
        assert_eq!(get.path, "/logs-*/_search");

        let bare = parse_request_line("/_cat/indices").expect("省略方法");
        assert_eq!(bare.method, "GET");
        assert_eq!(bare.path, "/_cat/indices");

        // 不带前导斜杠也收（补上），小写方法归一成大写，但不做别的猜测。
        let loose = parse_request_line("post my-index/_update/1").expect("小写方法 + 无斜杠");
        assert_eq!(loose.method, "POST");
        assert_eq!(loose.path, "/my-index/_update/1");

        assert!(parse_request_line("").is_err());
    }

    /// 多请求解析：空行分段、`#` 注释跳过、体原样保留。
    #[test]
    fn multiple_requests_are_split_on_blank_lines() {
        let text = "# 先看版本\nGET /\n\n\
                    POST /my-index/_search\n\
                    {\n  \"query\": {\"match_all\": {}}\n}\n\n\
                    # 只有注释的段要丢掉\n";
        let requests = parse_requests(text).expect("可解析");
        assert_eq!(requests.len(), 2, "注释段不该变成请求");

        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/");
        assert_eq!(requests[0].body, None, "没有体");

        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/my-index/_search");
        assert_eq!(
            requests[1].body.as_deref(),
            Some("{\n  \"query\": {\"match_all\": {}}\n}"),
            "体要原样保留（含缩进）"
        );

        // 纯注释 / 纯空白 → 空清单（调用方据此报「没有可执行的请求」）。
        assert!(
            parse_requests("# 什么也没有\n\n")
                .expect("可解析")
                .is_empty()
        );
    }

    /// 只读闸：GET/HEAD 全放行；POST 只在白名单端点放行；其余与未知一律当写。
    #[test]
    fn read_only_gate_defaults_to_refusing() {
        // 读：GET 与带体的搜索型 POST。
        assert!(is_read_only_request("GET", "/_cat/indices"));
        assert!(is_read_only_request("HEAD", "/my-index"));
        assert!(is_read_only_request("POST", "/my-index/_search"));
        assert!(is_read_only_request("POST", "/_search/scroll"));
        assert!(is_read_only_request("POST", "/_msearch"));
        // 查询串不该干扰判定。
        assert!(is_read_only_request("POST", "/my-index/_count?q=*"));

        // 写：POST 的写端点，以及一切非 GET/HEAD 的带体方法。
        assert!(!is_read_only_request("POST", "/my-index/_doc"));
        assert!(!is_read_only_request("POST", "/_bulk"));
        assert!(!is_read_only_request("POST", "/my-index/_update/1"));
        assert!(!is_read_only_request("PUT", "/my-index"));
        assert!(!is_read_only_request("DELETE", "/my-index/_doc/1"));
        assert!(!is_read_only_request("PATCH", "/my-index/_doc/1"));
        // 未知命令默认当写（安全方向必须是默认拒绝）。
        assert!(!is_read_only_request("POST", "/my-index/_whatever"));
    }

    /// 索引模式回落：调用方给的 → 档案配的 → 不限（`_all`）。
    #[test]
    fn index_pattern_falls_back_to_all() {
        assert_eq!(resolve_pattern(Some("logs-*"), "other"), Some("logs-*"));
        assert_eq!(resolve_pattern(None, "logs-*"), Some("logs-*"));
        assert_eq!(resolve_pattern(None, ""), None);
        assert_eq!(resolve_pattern(None, "   "), None);
        assert_eq!(resolve_pattern(Some("  "), ""), None);
        assert_eq!(pattern_path(None), "/_all");
        // ⚠️ 通配符是**模式语法**，必须原样留在路径里（不能编码成 `%2A`）。
        assert_eq!(pattern_path(Some("logs-*")), "/logs-*");
        assert_eq!(encode_pattern("logs-*"), "logs-*");
        assert_eq!(encode_pattern("a,b?"), "a,b?");
        // 字面部分照样编码（空格 / 非 ASCII / 保留字符）。
        assert_eq!(encode_pattern("我的 库"), "%E6%88%91%E7%9A%84%20%E5%BA%93");
        assert_eq!(encode_pattern("a b*"), "a%20b*");
    }

    /// 虚拟列：`_id` 永远排在首位。
    #[test]
    fn the_id_column_is_prepended() {
        let columns = columns_with_id(&[Column {
            name: "title".to_owned(),
            type_name: "text".to_owned(),
        }]);
        assert_eq!(columns[0].name, "_id");
        assert_eq!(columns[1].name, "title");
    }

    /// mapping 展平：`properties` 递归成点分路径、`fields` 多字段与 `runtime` 跳过。
    #[test]
    fn mapping_flattens_nested_paths_and_skips_multi_fields() {
        let value = serde_json::json!({
            "my-index": {
                "mappings": {
                    "properties": {
                        "title": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
                        "price": {"type": "double"},
                        "user": {"properties": {
                            "name": {"type": "keyword"},
                            "age": {"type": "integer"}
                        }},
                        "tags": {"type": "keyword"},
                        "empty_object": {"properties": {}}
                    },
                    "runtime": {"day": {"type": "keyword"}}
                }
            }
        });
        let columns = flatten_mappings(&value);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["price", "tags", "title", "user.age", "user.name"],
            "按路径排序；多字段 title.keyword 与 runtime 字段都不该出现，\
             `properties: {{}}` 的空对象没有叶子也不该出列"
        );
        assert_eq!(columns[0].type_name, "double");
        assert_eq!(
            columns
                .iter()
                .find(|c| c.name == "user.name")
                .expect("有")
                .type_name,
            "keyword"
        );
    }

    /// 目录：索引是表、别名是视图，一次 `_alias` 响应两类都给。
    #[test]
    fn catalog_maps_indices_to_tables_and_aliases_to_views() {
        let value = serde_json::json!({
            "logs-1": {"aliases": {"logs": {}}},
            "logs-2": {"aliases": {"logs": {}}},
            "other": {"aliases": {}}
        });
        let catalog = catalog_of(&value);
        let tables: Vec<&str> = catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .map(|entry| entry.name.as_str())
            .collect();
        let views: Vec<&str> = catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::View)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(tables, vec!["logs-1", "logs-2", "other"]);
        assert_eq!(views, vec!["logs"], "别名指向两个索引也只出现一次");
        assert!(
            catalog.iter().all(|entry| entry.schema.is_none()),
            "ES 没有命名空间层"
        );
    }

    /// 排序：`_id` 与 text 列被类型过滤挡下，全被挡下时退回 `_doc`。
    #[test]
    fn sort_clause_filters_unsortable_columns() {
        let columns = vec![
            Column {
                name: "_id".to_owned(),
                type_name: ID_TYPE.to_owned(),
            },
            Column {
                name: "title".to_owned(),
                type_name: "text".to_owned(),
            },
            Column {
                name: "ts".to_owned(),
                type_name: "date".to_owned(),
            },
            Column {
                name: "tag".to_owned(),
                type_name: "keyword".to_owned(),
            },
        ];
        // 调用方给的 order_by 恰好是主键（`_id`）→ 全被挡下 → 退回 `_doc`。
        let fallback = sort_clause(&[OrderBy::asc("_id")], &columns);
        assert_eq!(fallback, serde_json::json!([{"_doc": "asc"}]));

        // text 不可排，date / keyword 可排。
        let mixed = sort_clause(
            &[
                OrderBy::asc("title"),
                OrderBy::desc("ts"),
                OrderBy::asc("tag"),
            ],
            &columns,
        );
        assert_eq!(mixed, serde_json::json!([{"ts": "desc"}, {"tag": "asc"}]));
    }

    /// 过滤：无条件退回 `match_all`，有条件拼成 `bool.must` 且**值走 JSON 编码**。
    #[test]
    fn filters_become_query_dsl() {
        let types = HashMap::from([
            ("tag".to_owned(), "keyword".to_owned()),
            ("score".to_owned(), "long".to_owned()),
            ("ts".to_owned(), "date".to_owned()),
        ]);

        // 无条件是 `match_all`（保持「恒有一个 query」的请求形状）。
        assert_eq!(
            filter_clause(&[], &types).expect("无条件"),
            serde_json::json!({ "match_all": {} })
        );

        // 等值 / 不等 / 比较 / LIKE / IS [NOT] NULL 各一档，顺序即 `must` 的顺序。
        let clause = filter_clause(
            &[
                TableFilter::new("tag", FilterOp::Eq, "rust"),
                TableFilter::new("score", FilterOp::Gte, "9"),
                TableFilter::new("tag", FilterOp::Like, "a%b_"),
                TableFilter::null("ts", FilterOp::IsNotNull),
            ],
            &types,
        )
        .expect("合法过滤");
        assert_eq!(
            clause,
            serde_json::json!({
                "bool": {
                    "must": [
                        {"term": {"tag": "rust"}},
                        {"range": {"score": {"gte": 9}}},
                        {"wildcard": {"tag": {"value": "a*b?"}}},
                        {"exists": {"field": "ts"}},
                    ]
                }
            })
        );

        // 不等取反；IS NULL 是 exists 的反面。
        let negated = filter_clause(
            &[
                TableFilter::new("tag", FilterOp::NotEq, "old"),
                TableFilter::null("ts", FilterOp::IsNull),
            ],
            &types,
        )
        .expect("合法过滤");
        assert_eq!(
            negated,
            serde_json::json!({
                "bool": {
                    "must": [
                        {"bool": {"must_not": [{"term": {"tag": "old"}}]}},
                        {"bool": {"must_not": [{"exists": {"field": "ts"}}]}},
                    ]
                }
            })
        );
    }

    /// 过滤的注入面：值是用户输入，必须原样落在 JSON 字符串里，不进 DSL 结构。
    #[test]
    fn filter_values_are_never_interpreted_as_dsl() {
        let types = HashMap::from([("tag".to_owned(), "keyword".to_owned())]);
        let clause = filter_clause(
            &[TableFilter::new(
                "tag",
                FilterOp::Eq,
                "\"}],\"match_all\":{},\"x\":{\"a\":\"",
            )],
            &types,
        )
        .expect("合法过滤");
        // 整串仍是一个 `term` 的值——没有任何一层被顶开。
        let must = clause
            .get("bool")
            .and_then(|value| value.get("must"))
            .and_then(Json::as_array)
            .expect("bool.must");
        assert_eq!(must.len(), 1);
        assert_eq!(
            must[0]
                .get("term")
                .and_then(|value| value.get("tag"))
                .and_then(Json::as_str),
            Some("\"}],\"match_all\":{},\"x\":{\"a\":\"")
        );
    }

    /// 过滤的输入校验：空列名、缺值、以及 `LIKE` 的缺值都要**当场报错**。
    #[test]
    fn filters_reject_empty_columns_and_missing_values() {
        let types = HashMap::new();
        assert!(filter_clause(&[TableFilter::new("  ", FilterOp::Eq, "x")], &types).is_err());
        assert!(filter_clause(&[TableFilter::null("a", FilterOp::Eq)], &types).is_err());
        assert!(filter_clause(&[TableFilter::null("a", FilterOp::Like)], &types).is_err());
        // `IS NULL` / `IS NOT NULL` 不需要值。
        assert!(filter_clause(&[TableFilter::null("a", FilterOp::IsNull)], &types).is_ok());
    }

    /// `LIKE` → `wildcard`：`%`/`_` 是通配，用户自己的元字符先转义。
    #[test]
    fn like_patterns_become_wildcards() {
        assert_eq!(like_to_wildcard("a%b"), "a*b");
        assert_eq!(like_to_wildcard("a_b"), "a?b");
        // 用户输入的字面星号 / 问号 / 反斜杠不能变成通配。
        assert_eq!(like_to_wildcard("100*"), "100\\*");
        assert_eq!(like_to_wildcard("a?b"), "a\\?b");
        assert_eq!(like_to_wildcard("a\\b"), "a\\\\b");
        assert_eq!(like_to_wildcard("no wildcards"), "no wildcards");
    }

    /// 路径取值：嵌套对象、数组、缺字段。
    #[test]
    fn lookup_path_walks_objects_and_arrays() {
        let source = serde_json::json!({
            "user": {"name": "duck"},
            "items": [{"sku": "a"}, {"sku": "b"}],
            "tags": ["x", "y"],
            "empty": null
        });
        assert_eq!(
            lookup_path(&source, "user.name"),
            Some(serde_json::json!("duck"))
        );
        assert_eq!(
            lookup_path(&source, "tags"),
            Some(serde_json::json!(["x", "y"]))
        );
        assert_eq!(
            lookup_path(&source, "items.sku"),
            Some(serde_json::json!(["a", "b"])),
            "数组元素上继续按名字找"
        );
        assert_eq!(lookup_path(&source, "nope"), None);
        assert_eq!(lookup_path(&source, "user.nope"), None);
    }

    /// 值映射：按 mapping 类型分派；`scaled_float` 与未知类型走文本（不经 f64）。
    #[test]
    fn values_map_by_declared_type() {
        let json = |raw: &str| serde_json::from_str::<Json>(raw).expect("json");

        assert_eq!(map_json(&json("7"), "long"), Value::Int(7));
        assert_eq!(map_json(&json("7"), "unsigned_long"), Value::UInt(7));
        assert_eq!(map_json(&json("1.5"), "double"), Value::Float(1.5));
        assert_eq!(map_json(&json("1.5"), "float"), Value::Float(1.5));
        assert_eq!(map_json(&json("true"), "boolean"), Value::Bool(true));
        assert_eq!(
            map_json(&json("\"2026-10-12T00:00:00Z\""), "date"),
            Value::DateTime("2026-10-12T00:00:00Z".into())
        );
        assert_eq!(
            map_json(&json("\"abc\""), "keyword"),
            Value::Text("abc".into())
        );
        // ⚠️ 定点数不进 f64（scaled_float 是有精度的定点数）。
        assert_eq!(
            map_json(&json("12.34"), "scaled_float"),
            Value::Text("12.34".into())
        );
        // ⚠️ 类型未知时按 JSON 自己的形状来：整数给整数、小数给浮点——
        // 控制台查询与 `_cat` 之类的响应本来就没有 mapping 可依。
        assert_eq!(map_json(&json("12"), ""), Value::Int(12));
        assert_eq!(map_json(&json("12.34"), ""), Value::Float(12.34));
        assert_eq!(map_json(&json("null"), "long"), Value::Null);
        assert_eq!(
            map_json(&json("{\"a\":1}"), "object"),
            Value::Json("{\"a\":1}".into())
        );
    }

    /// 写值绑定：类型已知时严格（不偷偷降级），未知时启发式。
    #[test]
    fn bind_value_is_strict_when_the_type_is_known() {
        assert_eq!(bind_value(None, Some("long")), Ok(Json::Null));
        assert_eq!(
            bind_value(Some("42"), Some("long")),
            Ok(serde_json::json!(42))
        );
        assert_eq!(
            bind_value(Some(" true "), Some("boolean")),
            Ok(serde_json::json!(true))
        );
        assert_eq!(
            bind_value(Some("1.25"), Some("double")),
            Ok(serde_json::json!(1.25))
        );
        // 日期原样当字符串交给服务端解析。
        assert_eq!(
            bind_value(Some("2026-10-12"), Some("date")),
            Ok(serde_json::json!("2026-10-12"))
        );
        // 类型已知但值不合 → 直接报错（不偷偷写成字符串）。
        assert!(bind_value(Some("abc"), Some("long")).is_err());
        assert!(bind_value(Some("maybe"), Some("boolean")).is_err());

        // 类型未知 → 启发式。
        assert_eq!(bind_value(Some("42"), None), Ok(serde_json::json!(42)));
        assert_eq!(bind_value(Some("true"), None), Ok(serde_json::json!(true)));
        assert_eq!(bind_value(Some("hi"), None), Ok(serde_json::json!("hi")));
        assert_eq!(
            bind_value(Some("{\"a\":1}"), None),
            Ok(serde_json::json!({"a": 1}))
        );
    }

    /// 主键取值：`_id` 必须在，且不能是空串。
    #[test]
    fn document_id_comes_from_the_primary_key() {
        let pk = vec![("_id".to_owned(), "abc".to_owned())];
        assert_eq!(id_of(&pk), Ok("abc".to_owned()));
        assert!(id_of(&Vec::new()).is_err());
        assert!(id_of(&vec![("other".to_owned(), "x".to_owned())]).is_err());
        assert!(id_of(&vec![("_id".to_owned(), String::new())]).is_err());
    }

    /// `_bulk` 汇总：改 / 增 / 删各自计数，失败原因原样带出（部分成功要说清）。
    #[test]
    fn bulk_counts_splits_by_action() {
        let ok = serde_json::json!({
            "errors": false,
            "items": [
                {"update": {"_index": "i", "_id": "1", "result": "updated", "status": 200}},
                {"update": {"_index": "i", "_id": "2", "result": "noop", "status": 200}},
                {"index": {"_index": "i", "_id": "3", "result": "created", "status": 201}},
                {"delete": {"_index": "i", "_id": "4", "result": "deleted", "status": 200}}
            ]
        });
        assert_eq!(bulk_counts(&ok), (2, 1, 1, Vec::new()));

        let partial = serde_json::json!({
            "errors": true,
            "items": [
                {"index": {"_index": "i", "_id": "1", "result": "created", "status": 201}},
                {"index": {"_index": "i", "error": {"type": "mapper_parsing_exception", "reason": "failed to parse"}, "status": 400}}
            ]
        });
        let (updated, created, deleted, errors) = bulk_counts(&partial);
        assert_eq!((updated, created, deleted), (0, 1, 0));
        assert_eq!(errors, vec!["failed to parse".to_owned()]);
    }

    /// NDJSON 拼装：delete 只有 action 行；update / index 各带一行源文档；`_id` 可指定。
    #[test]
    fn bulk_lines_are_ndjson() {
        let types = HashMap::new();

        let deletes = delete_lines("i", &[vec![("_id".to_owned(), "1".to_owned())]]).expect("可拼");
        let lines: Vec<&str> = deletes.lines().collect();
        assert_eq!(lines.len(), 1, "delete 没有源文档行");
        // 逐字段比对而不是比字符串：`serde_json::Map` 的键序取决于 `preserve_order`
        // 这个 feature（本仓没开，但别把测试押在 feature 上）。
        let action: Json = serde_json::from_str(lines[0]).expect("是 JSON");
        assert_eq!(action["delete"]["_index"], serde_json::json!("i"));
        assert_eq!(action["delete"]["_id"], serde_json::json!("1"));

        let edits = edit_lines(
            "i",
            &types,
            &[CellEdit {
                pk: vec![("_id".to_owned(), "9".to_owned())],
                column: "title".to_owned(),
                value: Some("x".to_owned()),
            }],
        )
        .expect("可拼");
        let lines: Vec<&str> = edits.lines().collect();
        assert_eq!(lines.len(), 2, "action 行 + 源文档行");
        assert!(lines[0].contains(r#""update":"#));
        assert_eq!(lines[1], r#"{"doc":{"title":"x"}}"#);

        // 带 `_id` 的插入用它当文档 id；不带则交给服务端生成。
        let with_id = insert_lines(
            "i",
            &types,
            &[InsertRow {
                columns: vec!["_id".to_owned(), "title".to_owned()],
                values: vec![Some("42".to_owned()), Some("hi".to_owned())],
            }],
        )
        .expect("可拼");
        assert!(with_id.contains(r#""_id":"42""#), "{with_id}");
        assert!(
            with_id.contains(r#"{"title":"hi"}"#),
            "`_id` 不进文档体：{with_id}"
        );

        let auto_id = insert_lines(
            "i",
            &types,
            &[InsertRow {
                columns: vec!["title".to_owned()],
                values: vec![Some("hi".to_owned())],
            }],
        )
        .expect("可拼");
        assert!(
            !auto_id.contains("\"_id\""),
            "不带 id 就不该写 id：{auto_id}"
        );

        // 宽度不匹配要拒。
        assert!(
            insert_lines(
                "i",
                &types,
                &[InsertRow {
                    columns: vec!["a".to_owned(), "b".to_owned()],
                    values: vec![Some("1".to_owned())],
                }],
            )
            .is_err()
        );
    }

    /// 响应 → （影响行数, 结果集）三种形状。
    #[test]
    fn responses_map_to_result_sets() {
        // ① 搜索响应：每条命中一行，首列是 `_id`，`_source` 的键按首次出现补列。
        let search = serde_json::json!({
            "hits": {"hits": [
                {"_id": "1", "_source": {"title": "a"}},
                {"_id": "2", "_source": {"title": "b", "extra": 1}}
            ]}
        });
        let (_, result) = outcome_of(&search, 10).expect("解析");
        let result = result.expect("有结果集");
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "title", "extra"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Text("1".into()));
        // 第一行没有 extra 列 → 补 Null，列数不塌。
        assert_eq!(result.rows[0][2], Value::Null);
        assert_eq!(result.rows[1][2], Value::Int(1));

        // ② 对象数组：每个元素一行。⚠️ 列序取决于 `serde_json` 的 `preserve_order`
        // 这个 feature（是别的依赖开启的，不归本 crate 管）→ 按下标找列名，不押列序。
        let array = serde_json::json!([{"index": "a", "docs.count": "3"}, {"index": "b"}]);
        let (_, result) = outcome_of(&array, 10).expect("解析");
        let result = result.expect("有结果集");
        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.rows.len(), 2);
        let at = |name: &str| {
            result
                .columns
                .iter()
                .position(|column| column.name == name)
                .unwrap_or_else(|| panic!("缺列 {name}"))
        };
        assert_eq!(result.rows[0][at("index")], Value::Text("a".into()));
        assert_eq!(result.rows[0][at("docs.count")], Value::Text("3".into()));
        assert_eq!(result.rows[1][at("docs.count")], Value::Null, "短行要补齐");

        // ③ 普通对象：单行，键当列。
        let object = serde_json::json!({"count": 12});
        let (_, result) = outcome_of(&object, 10).expect("解析");
        let result = result.expect("有结果集");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0][0],
            Value::Int(12),
            "类型未知的整数按 JSON 形状给整数"
        );

        // ④ `_bulk` 响应：只给影响行数，不给结果集。
        let bulk = serde_json::json!({"errors": false, "items": [
            {"delete": {"_index": "i", "_id": "1", "result": "deleted", "status": 200}},
            {"delete": {"_index": "i", "_id": "2", "result": "not_found", "status": 404}}
        ]});
        assert_eq!(outcome_of(&bulk, 10), Ok((2, None)));

        // 空响应：没有结果集。
        assert_eq!(outcome_of(&Json::Null, 10), Ok((0, None)));
    }

    /// 行数截断在 Rust 侧做（`max_rows` 处停并置 `truncated`）。
    #[test]
    fn result_sets_are_truncated_at_the_cap() {
        let array = serde_json::json!([{"a": 1}, {"a": 2}, {"a": 3}]);
        let (_, result) = outcome_of(&array, 2).expect("解析");
        let result = result.expect("有结果集");
        assert_eq!(result.rows.len(), 2);
        assert!(result.truncated);

        let hits = [
            serde_json::json!({"_id": "1"}),
            serde_json::json!({"_id": "2"}),
        ];
        let (_, rows, truncated) = rows_of_hits(&hits, 5);
        assert_eq!(rows.len(), 2);
        assert!(!truncated, "没到上限就不该说截断");
    }

    /// 网格分页：列固定取自 mapping（不随命中漂移），多取一条探测下一页。
    #[test]
    fn the_grid_page_uses_the_mapping_columns() {
        let columns = columns_with_id(&[
            Column {
                name: "title".to_owned(),
                type_name: "text".to_owned(),
            },
            Column {
                name: "ts".to_owned(),
                type_name: "date".to_owned(),
            },
        ]);
        // 三道命中、limit=2：探测到还有下一页。
        let hits = [
            serde_json::json!({"_id": "1", "_source": {"title": "a"}}),
            serde_json::json!({"_id": "2", "_source": {"title": "b"}}),
            serde_json::json!({"_id": "3", "_source": {"title": "c"}}),
        ];
        let page = page_of(&hits, &columns, 2);
        assert_eq!(page.rows.len(), 2);
        assert!(page.truncated);
        assert_eq!(page.columns.len(), 3, "列由 mapping 决定，不随命中漂移");
        // 缺 `ts` → Null（而不是少一格）。
        assert_eq!(page.rows[0][2], Value::Null);
        assert_eq!(page.rows[0][0], Value::Text("1".into()));
    }

    /// 更新响应：`updated` / `noop` 都算命中，`not_found` 不算。
    #[test]
    fn update_hits_count_noop_as_success() {
        assert_eq!(hit_of_update(&serde_json::json!({"result": "updated"})), 1);
        assert_eq!(hit_of_update(&serde_json::json!({"result": "noop"})), 1);
        assert_eq!(
            hit_of_update(&serde_json::json!({"result": "not_found"})),
            0
        );
        assert_eq!(hit_of_update(&serde_json::json!({})), 0);
    }

    /// 错误正文归一：结构化错误剥出 `type: reason`，非 JSON 退回截断原文。
    #[test]
    fn error_text_prefers_the_structured_reason() {
        let structured = r#"{"error":{"type":"index_not_found_exception","reason":"no such index [x]"},"status":404}"#;
        assert_eq!(
            error_text(structured, 404),
            "HTTP 404: index_not_found_exception: no such index [x]"
        );
        // 只有 reason 也要能用。
        assert_eq!(
            error_text(r#"{"error":{"reason":"boom"}}"#, 500),
            "HTTP 500: boom"
        );
        assert_eq!(error_text("", 502), "HTTP 502");
        assert_eq!(error_text("plain text", 500), "HTTP 500: plain text");
    }

    /// 「DDL」重构：剥掉易变设置、保留 mapping，首行是英文注释（内核不吐成品中文）。
    #[test]
    fn ddl_is_reconstructed_without_volatile_settings() {
        let value = serde_json::json!({
            "my-index": {
                "settings": {"index": {
                    "creation_date": "1699999999999",
                    "uuid": "abc",
                    "version": {"created": "8500000"},
                    "provided_name": "my-index",
                    "number_of_shards": "1",
                    "number_of_replicas": "1"
                }},
                "mappings": {"properties": {"title": {"type": "text"}}}
            }
        });
        let text = ddl_of("my-index", &value);
        assert!(text.starts_with("// Elasticsearch has no DDL"), "{text}");
        assert!(text.contains("PUT /my-index"));
        assert!(text.contains("number_of_shards"), "{text}");
        assert!(!text.contains("creation_date"), "易变设置要剥掉：{text}");
        assert!(!text.contains("uuid"), "{text}");
        assert!(text.contains("properties"), "{text}");

        // 索引不存在 → 只给一行 `PUT`，不编造结构。
        let empty = ddl_of("gone", &serde_json::json!({}));
        assert!(empty.contains("PUT /gone"));
    }

    /// 端点拼装：scheme 跟 TLS 档位，端口缺省 9200，IPv6 带方括号，认证头按用户名有无。
    #[test]
    fn endpoint_follows_tls_and_credentials() {
        let base = ConnectionConfig {
            id: "conn-es".into(),
            kind: crate::config::DatabaseKind::Elasticsearch,
            host: "es.local".into(),
            user: "elastic".into(),
            ..ConnectionConfig::default()
        };
        let endpoint = build_endpoint(&base, Some("hunter2"));
        assert!(
            endpoint.base.starts_with("http://es.local:9200"),
            "{}",
            endpoint.base
        );
        // Basic 头是 base64("elastic:hunter2")。
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("elastic:hunter2")
        );
        assert_eq!(endpoint.auth.as_deref(), Some(expected.as_str()));

        let tls = ConnectionConfig {
            tls: TlsMode::Trust,
            ..base.clone()
        };
        assert!(
            build_endpoint(&tls, None)
                .base
                .starts_with("https://es.local:9200")
        );

        // 不填用户名 → 不发认证头（没开安全的单机集群）。
        let anonymous = ConnectionConfig {
            user: String::new(),
            ..base.clone()
        };
        assert!(build_endpoint(&anonymous, None).auth.is_none());

        let v6 = ConnectionConfig {
            host: "::1".into(),
            ..base
        };
        assert!(
            build_endpoint(&v6, None)
                .base
                .starts_with("http://[::1]:9200"),
            "IPv6 字面量要带方括号"
        );
    }

    /// 索引名解析：schema 优先，其次 name；两者都空要拒。
    #[test]
    fn index_name_resolution() {
        assert_eq!(index_of(&TableRef::bare("logs-1")).expect("有名"), "logs-1");
        assert_eq!(
            index_of(&TableRef::in_schema("alias-name", "logs-1")).expect("schema 优先"),
            "alias-name"
        );
        assert!(index_of(&TableRef::default()).is_err());
    }

    /// 取消寻址：按 `X-Opaque-Id` 从 `_tasks` 里认出自己那条任务（`nodeId:taskId`）。
    #[test]
    fn cancel_finds_the_task_by_opaque_id() {
        let tasks = serde_json::json!({
            "nodes": {
                "node-a": {"tasks": {
                    "12": {"action": "indices:data/read/search",
                           "headers": {"X-Opaque-Id": "duck-111-0"}}
                }},
                "node-b": {"tasks": {
                    // 头名大小写不敏感。
                    "34": {"action": "indices:data/read/search",
                           "headers": {"x-opaque-id": "duck-111-1"}}
                }}
            }
        });
        assert_eq!(
            find_task_by_opaque(&tasks, "duck-111-1"),
            Some("node-b:34".to_owned())
        );
        assert_eq!(
            find_task_by_opaque(&tasks, "duck-111-0"),
            Some("node-a:12".to_owned())
        );
        // 没在跑 → 空操作（取消本就该幂等）。
        assert_eq!(find_task_by_opaque(&tasks, "duck-999-9"), None);
        // 形状不对也算找不到，不 panic。
        assert_eq!(find_task_by_opaque(&serde_json::json!({}), "x"), None);
        assert_eq!(
            find_task_by_opaque(&serde_json::json!({"nodes": {"n": {}}}), "x"),
            None
        );
    }
}
