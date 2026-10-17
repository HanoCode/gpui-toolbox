//! 协议层：**方法名、参数形状、结果形状**。
//!
//! 这一层只做两件事，不做第三件：
//!
//! 1. 把「方法名 + 参数」拼成 JSON（`*_params` 函数族）；
//! 2. 把「结果 JSON」解析成 Rust 结构体（`serde`）。
//!
//! 它**不认识**连接、超时、进程 —— 那是 [`crate::AgentHandle`] 的事。
//!
//! # 命名口径（**逐字对齐，改一个字母就静默失效**）
//!
//! 对端是别人写好的二进制工件（见 `duck-drivers`），两边都**不做命名策略转换**：
//!
//! | 对端 | 序列化器 | 实际 JSON 键 |
//! |---|---|---|
//! | JDBC agent（Java） | Gson 默认 | **Java 字段名本身** |
//! | Go agent | `encoding/json` + tag | tag 值 |
//!
//! 两拨人写的是同一套名字，所以本模块的字段名是「**照抄对端**」而不是「Rust 该长什么样」：
//! 结果字段 **snake_case 为主，夹着 camelCase 少数派**（`sessionRole`、
//! `identifierQuote`），参数更是**两套混用**（`url_params` 是 snake_case、`maxRows` 是
//! camelCase、`agentSessionId` 又是 camelCase）。这里**不统一、不美化** ——
//! 统一了就得加一串 `#[serde(rename)]`，等于把「对端长什么样」藏起来，
//! 下次对不上号时更难查。
//!
//! # 只建模我们真正用的部分
//!
//! 协议有 41 个方法，本模块给出**全部方法名常量**（少一个常量就要在调用点手打字符串，
//! 打错的代价是一次 `-32601 method not found`），但只为**元数据 + 查询**这条主路
//! 建模结果类型。
//!
//! 几处故意留白，都不是疏忽：
//!
//! - `spatial_columns` / `spatial_values`：空间类型（PostGIS 几何等）的 WKB 预览用，
//!   主界面不渲染地图；反序列化**默认忽略未知字段**，所以不建模既不报错也不丢别的字段。
//! - `messages`：`RAISERROR` / `PRINT` 之类的信息性消息，同上。
//! - `execute_transaction` / `execute_batch` 的结果：形状是「结果数组」，等真做批量
//!   执行时再定 —— 现在猜一个形状反而会误导上层。
//!
//! # 权威来源
//!
//! 方法名与能力名的权威是 dbx 的 `agents/common/src/main/resources/agent-protocol-v2.json`
//! （v1 是它的子集）。本文件与它不一致时以它为准。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AgentError;

// ── 方法名 ───────────────────────────────────────────────────────────────────

/// 全部 41 个方法名。
///
/// 与 dbx 的 `AgentProtocol` 逐字一致；[`ALL`](self::ALL) 的长度即 41（有单测钉住），
/// 所以增删都能被发现。
pub mod method {
    /// 握手：拿协议版本与能力。
    pub const HANDSHAKE: &str = "handshake";
    /// 协议 v1 的单会话连接（老路径）。
    pub const CONNECT: &str = "connect";
    /// 协议 v2 的开会话：参数与 `connect` 相同，另带 `agentSessionId`。
    pub const OPEN_SESSION: &str = "open_session";
    /// 关会话。**有回应**（`{"ok":true}`），不是通知。
    pub const CLOSE_SESSION: &str = "close_session";
    /// 验会话（服务端问一句「连接还活着吗」）。
    pub const VALIDATE_SESSION: &str = "validate_session";
    /// 取消某会话正在跑的语句 / 游标。
    pub const CANCEL_SESSION: &str = "cancel_session";
    /// 试连：连上就算成功，不留会话。
    pub const TEST_CONNECTION: &str = "test_connection";
    /// 验连接（无会话形态）。
    pub const VALIDATE_CONNECTION: &str = "validate_connection";
    /// 连接信息：标识符引号、兼容模式。
    pub const CONNECTION_INFO: &str = "connection_info";
    /// 列数据库。
    pub const LIST_DATABASES: &str = "list_databases";
    /// 列 schema。
    pub const LIST_SCHEMAS: &str = "list_schemas";
    /// 列表 / 视图。
    pub const LIST_TABLES: &str = "list_tables";
    /// 列对象（表 / 视图 / 序列 / 函数……）。
    pub const LIST_OBJECTS: &str = "list_objects";
    /// 列数据类型。
    pub const LIST_DATA_TYPES: &str = "list_data_types";
    /// 补全助手（`completion_assistant_search_v1`）。
    pub const COMPLETION_ASSISTANT_SEARCH_V1: &str = "completion_assistant_search_v1";
    /// 取对象源码（函数 / 视图定义）。
    pub const GET_OBJECT_SOURCE: &str = "get_object_source";
    /// 取建表 DDL。
    pub const GET_TABLE_DDL: &str = "get_table_ddl";
    /// 取列。
    pub const GET_COLUMNS: &str = "get_columns";
    /// 列索引。
    pub const LIST_INDEXES: &str = "list_indexes";
    /// 列外键。
    pub const LIST_FOREIGN_KEYS: &str = "list_foreign_keys";
    /// 列触发器。
    pub const LIST_TRIGGERS: &str = "list_triggers";
    /// 列约束。
    pub const LIST_CONSTRAINTS: &str = "list_constraints";
    /// 列分区。
    pub const LIST_PARTITIONS: &str = "list_partitions";
    /// 列子分区。
    pub const LIST_SUBPARTITIONS: &str = "list_subpartitions";
    /// 取分区状态。
    pub const GET_TABLE_PARTITION_STATUS: &str = "get_table_partition_status";
    /// 取分区策略。
    pub const GET_TABLE_PARTITIONING: &str = "get_table_partitioning";
    /// 执行一条语句，一次拿完。
    pub const EXECUTE_QUERY: &str = "execute_query";
    /// 执行并只取第一页（返回游标 `sessionId`）。
    pub const EXECUTE_QUERY_PAGE: &str = "execute_query_page";
    /// 按游标取下一页。
    pub const FETCH_QUERY_PAGE: &str = "fetch_query_page";
    /// 关游标。
    pub const CLOSE_QUERY_SESSION: &str = "close_query_session";
    /// 开始按页读整表。
    pub const START_TABLE_READ: &str = "start_table_read";
    /// 按页读整表：取页。
    pub const FETCH_TABLE_READ_PAGE: &str = "fetch_table_read_page";
    /// 按页读整表：收尾。
    pub const CLOSE_TABLE_READ_SESSION: &str = "close_table_read_session";
    /// 取执行计划。
    pub const GET_EXPLAIN_INFO: &str = "get_explain_info";
    /// 批量执行（不保证事务）。
    pub const EXECUTE_BATCH: &str = "execute_batch";
    /// 一整批包在一个事务里。
    pub const EXECUTE_TRANSACTION: &str = "execute_transaction";
    /// 手动事务：开始。
    pub const BEGIN_MANUAL_TRANSACTION: &str = "begin_manual_transaction";
    /// 手动事务：提交。
    pub const COMMIT_MANUAL_TRANSACTION: &str = "commit_manual_transaction";
    /// 手动事务：回滚。
    pub const ROLLBACK_MANUAL_TRANSACTION: &str = "rollback_manual_transaction";
    /// 断开（v1 单会话路径的关连接）。
    pub const DISCONNECT: &str = "disconnect";
    /// 收尾：请 agent 自己退。
    pub const SHUTDOWN: &str = "shutdown";

    /// 全部方法名，供「拼错没拼错」的自检用。
    pub const ALL: [&str; 41] = [
        HANDSHAKE,
        CONNECT,
        OPEN_SESSION,
        CLOSE_SESSION,
        VALIDATE_SESSION,
        CANCEL_SESSION,
        TEST_CONNECTION,
        VALIDATE_CONNECTION,
        CONNECTION_INFO,
        LIST_DATABASES,
        LIST_SCHEMAS,
        LIST_TABLES,
        LIST_OBJECTS,
        LIST_DATA_TYPES,
        COMPLETION_ASSISTANT_SEARCH_V1,
        GET_OBJECT_SOURCE,
        GET_TABLE_DDL,
        GET_COLUMNS,
        LIST_INDEXES,
        LIST_FOREIGN_KEYS,
        LIST_TRIGGERS,
        LIST_CONSTRAINTS,
        LIST_PARTITIONS,
        LIST_SUBPARTITIONS,
        GET_TABLE_PARTITION_STATUS,
        GET_TABLE_PARTITIONING,
        EXECUTE_QUERY,
        EXECUTE_QUERY_PAGE,
        FETCH_QUERY_PAGE,
        CLOSE_QUERY_SESSION,
        START_TABLE_READ,
        FETCH_TABLE_READ_PAGE,
        CLOSE_TABLE_READ_SESSION,
        GET_EXPLAIN_INFO,
        EXECUTE_BATCH,
        EXECUTE_TRANSACTION,
        BEGIN_MANUAL_TRANSACTION,
        COMMIT_MANUAL_TRANSACTION,
        ROLLBACK_MANUAL_TRANSACTION,
        DISCONNECT,
        SHUTDOWN,
    ];
}

// ── 能力名 ───────────────────────────────────────────────────────────────────

/// 握手回包里 `capabilities` 的取值。
///
/// 只列**会用来做功能开关**的那几个：能力名的全集在 `agent-protocol-v2.json` 的
/// `allCapabilities`，其中大半是 etcd / MongoDB 专属，SQL 轨用不到。
///
/// ⚠️ 判能力一律用 [`HandshakeInfo::supports`]，**不要**在调用点上直接比对字面量：
/// 拼错的能力名不会报错，只会让那个开关**永远为假**（功能静默消失）。
pub mod capability {
    /// 能连能查（几乎所有 agent 都声明）。
    pub const CONNECT: &str = "connect";
    /// 能试连。
    pub const TEST_CONNECTION: &str = "test_connection";
    /// 能读元数据。
    pub const METADATA: &str = "metadata";
    /// 能执行任意语句。
    pub const QUERY: &str = "query";
    /// 支持「执行 + 游标翻页」（`execute_query_page` / `fetch_query_page`）。
    pub const PAGED_QUERY: &str = "paged_query";
    /// 支持事务。
    pub const TRANSACTION: &str = "transaction";
    /// 支持 DDL。
    pub const DDL: &str = "ddl";
    /// 协议 v2：一个进程可以服务多个会话。
    pub const MULTI_SESSION: &str = "multi_session";
    /// 错误回包里带结构化恢复数据（`category` / `retryable` / `sessionDisposition`…）。
    pub const STRUCTURED_ERROR_V1: &str = "structured_error_v1";
}

// ── 字段名 ───────────────────────────────────────────────────────────────────

/// 会话 id 的键名（协议 v2 的 `sessionField`）。
pub const SESSION_FIELD: &str = "agentSessionId";

/// 查询游标 id 的键名（协议 v2 的 `cursorSessionField`）。
///
/// ⚠️ **和 [`SESSION_FIELD`] 不是一个东西**，`fetch_query_page` / `close_query_session`
/// 用的是这个：前者标识「哪条连接」，后者标识「哪个还没读完的结果集」。
/// 混用不会报错 —— 对端会当成一个不存在的游标并回错，于是表现为「翻页永远失败」。
pub const CURSOR_SESSION_FIELD: &str = "sessionId";

// ── 参数拼装 ─────────────────────────────────────────────────────────────────

/// 只带会话 id 的参数：`close_session` / `cancel_session` / `validate_session` 用。
#[must_use]
pub fn session_params(agent_session_id: &str) -> Value {
    json!({ SESSION_FIELD: agent_session_id })
}

/// 只带库名的参数：`list_schemas` / `list_databases` 之外的按库取元数据。
#[must_use]
pub fn database_params(database: &str) -> Value {
    json!({ "database": database })
}

/// 带库 + schema 的参数：`list_tables` / `list_objects`。
#[must_use]
pub fn schema_params(database: &str, schema: &str) -> Value {
    json!({ "database": database, "schema": schema })
}

/// 带库 + schema + 表名的参数：`get_columns` / `list_indexes` / `list_foreign_keys` …
#[must_use]
pub fn schema_table_params(database: &str, schema: &str, table: &str) -> Value {
    json!({ "database": database, "schema": schema, "table": table })
}

/// `execute_query` 的参数。
///
/// `max_rows` 是**上限**，不是分页 —— 对端会读满这么多行然后置 `truncated`。
/// 传 `0` 表示「不设上限」（对端把 0 当成不限制），所以别拿 0 当「不要行」。
#[must_use]
pub fn query_params(
    sql: &str,
    database: Option<&str>,
    schema: Option<&str>,
    max_rows: u32,
) -> Value {
    let mut params = json!({ "sql": sql, "maxRows": max_rows });
    if let Some(map) = params.as_object_mut() {
        if let Some(database) = database.filter(|value| !value.is_empty()) {
            map.insert("database".to_owned(), Value::String(database.to_owned()));
        }
        if let Some(schema) = schema.filter(|value| !value.is_empty()) {
            map.insert("schema".to_owned(), Value::String(schema.to_owned()));
        }
    }
    params
}

/// `execute_query_page` 的参数：要 `pageSize`（每页多少行）**和** `maxRows`（总量上限）。
#[must_use]
pub fn query_page_params(
    sql: &str,
    database: Option<&str>,
    schema: Option<&str>,
    page_size: u32,
    max_rows: u32,
) -> Value {
    let mut params = query_params(sql, database, schema, max_rows);
    if let Some(map) = params.as_object_mut() {
        map.insert("pageSize".to_owned(), json!(page_size));
    }
    params
}

/// `fetch_query_page` 的参数。游标键是 [`CURSOR_SESSION_FIELD`]，不是会话 id。
#[must_use]
pub fn fetch_page_params(cursor_session_id: &str, page_size: u32) -> Value {
    json!({ CURSOR_SESSION_FIELD: cursor_session_id, "pageSize": page_size })
}

/// `close_query_session` 的参数。
#[must_use]
pub fn close_query_session_params(cursor_session_id: &str) -> Value {
    json!({ CURSOR_SESSION_FIELD: cursor_session_id })
}

// ── 会话角色 ─────────────────────────────────────────────────────────────────

/// `sessionRole`：这条会话是**干活的**还是**看元数据的**。
///
/// 对端在开池子时会按它分流 —— 元数据会话走轻量连接，避免「刷新一下对象树，
/// 顺手把跑着的长查询挤掉」。值不认识时对端按 [`Workload`](Self::Workload) 处理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRole {
    /// 跑用户语句。
    #[default]
    Workload,
    /// 读元数据。
    Metadata,
}

impl SessionRole {
    /// 线上取值。
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Workload => "workload",
            Self::Metadata => "metadata",
        }
    }
}

// ── 连接参数 ─────────────────────────────────────────────────────────────────

/// `connect` / `open_session` 的参数。
///
/// 字段与 dbx 的 `ConnectParams` 对齐（对端是 Gson，键就是 Java 字段名）。
/// 用 [`Default`] 起头再逐个填：只有 `host` / `port` 是真正必填，
/// 其余留空时对端各自有合理默认。
///
/// # 三个容易搞错的地方
///
/// - `port_explicit`：**端口是不是用户明确指定的**。SQL Server 的实例名解析要靠它 ——
///   用户写了 `host` 但没写端口时填 `false`，对端才会去查实例名。
/// - `mysql_compat_mode`：GBASE / 达梦这类「协议像 MySQL」的库用它切方言，
///   普通 MySQL 不要填。
/// - `connection_string`：**填了它就基本压过前面那些离散字段**（对端只在部分引擎上
///   认整串 URL）。所以要么走离散字段，要么走整串 —— 别两套都填一半。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectRequest {
    /// 主机。H2 / SQLite 这类文件库留空。
    pub host: String,
    /// 端口。文件库填 0。
    pub port: u16,
    /// 库名（Oracle 是 service name，SQL Server 是 database）。
    pub database: String,
    /// 用户名。
    pub username: String,
    /// 口令。落盘由上层负责（`duck-platform::secret`），本层只是过路。
    pub password: String,
    /// JDBC URL 的查询串（`?` 之后那截），不带问号。
    pub url_params: String,
    /// 整串连接串（JDBC URL / Mongo URI）。
    pub connection_string: String,
    /// 端口是否由用户明确指定。
    pub port_explicit: bool,
    /// 是否按 MySQL 兼容模式处理。
    pub mysql_compat_mode: bool,
    /// 是否启用 TLS。
    pub ssl: bool,
    /// 服务端 CA 证书路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    /// 客户端证书路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<String>,
    /// 客户端私钥路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<String>,
    /// 外部 JDBC 插件要用哪个驱动类。
    pub jdbc_driver_class: String,
    /// 外部 JDBC 插件的驱动 jar 路径。
    pub jdbc_driver_paths: Vec<String>,
    /// 驱动 profile（对端按它挑方言细节），通常留空。
    pub driver_profile: String,
    /// 会话角色。⚠️ 这个键是 **camelCase**，与前面那些 snake_case 混在一层里。
    #[serde(rename = "sessionRole")]
    pub session_role: SessionRole,
    /// GBASE 的服务名（仅 GBASE 用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gbase_server: Option<String>,
    /// Informix 的 server 名（仅 Informix 用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub informix_server: Option<String>,
}

impl ConnectRequest {
    /// 编码成线上参数。
    ///
    /// # Errors
    /// 理论上不会失败（全是标量），但不 `unwrap` —— 崩在协议层是最难查的一类，
    /// 而返回错误只是多一个 `?`。
    pub fn to_params(&self) -> crate::Result<Value> {
        serde_json::to_value(self).map_err(|error| {
            AgentError::Protocol(format!("cannot encode the connection parameters: {error}"))
        })
    }

    /// 组装 `open_session` 的参数：连接参数 + 会话 id。
    ///
    /// # Errors
    /// 同 [`to_params`](Self::to_params)。
    pub fn into_session_params(self, agent_session_id: &str) -> crate::Result<Value> {
        let mut params = self.to_params()?;
        let Some(map) = params.as_object_mut() else {
            return Err(AgentError::Protocol(
                "the connection parameters did not encode to an object".to_owned(),
            ));
        };
        map.insert(
            SESSION_FIELD.to_owned(),
            Value::String(agent_session_id.to_owned()),
        );
        Ok(params)
    }

    /// 组装 `connect` 的参数（协议 v1 的单会话老路径，不带会话 id）。
    ///
    /// 只在对方握手说「不支持 `multi_session`」时才该走这条 —— 老路径一个进程
    /// 只能服务一条连接，多开一个查询就得再起一个进程。
    ///
    /// # Errors
    /// 同 [`to_params`](Self::to_params)。
    pub fn into_connect_params(self) -> crate::Result<Value> {
        self.to_params()
    }
}

// ── 结果：握手 ───────────────────────────────────────────────────────────────

/// `handshake` 的结果。
///
/// 三个键都是 **camelCase**（对端 Java 的 `HandshakeResult` 就是这仨），
/// 所以这里用 `rename_all` 而不是逐个 `rename`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeInfo {
    /// 协议版本。`1` = 单会话老协议，`2` = 多会话；暂未见过更大的值。
    pub protocol_version: u32,
    /// agent 自己的协议版本。**通常与 `protocol_version` 相同**；
    /// 留 `#[serde(default)]` 是因为个别 agent 只回了前者（少了它就整个解析失败，
    /// 而这里失败等于「连不上任何库」）。
    #[serde(default)]
    pub agent_protocol_version: u32,
    /// 能力名单，取值见 [`capability`]。
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl HandshakeInfo {
    /// 对端是否声明了某项能力。
    ///
    /// **未声明 ≠ 不支持**（老 agent 的名单可能不全），所以调用点要么把它当
    /// 「优先走哪条路」的提示，要么在拿不到能力时准备一条回退路径 ——
    /// 别拿它当硬闸门，否则一个名单写漏的 agent 会「连得上但什么都做不了」。
    #[must_use]
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|value| value == capability)
    }

    /// 是否是多会话协议（一个进程服务多条连接）。
    #[must_use]
    pub fn is_multi_session(&self) -> bool {
        self.supports(capability::MULTI_SESSION)
    }
}

// ── 结果：连接信息 ───────────────────────────────────────────────────────────

/// `connection_info` 的结果：**连上之后才能问到的那些事实**。
///
/// 目前只有两样，但第一样很关键：`identifierQuote` 是**这个引擎包裹标识符用什么**。
/// 我们不在驱动目录里硬编码它（达梦用 `"`、GBase 用 `` ` ``，同族不同款），
/// 而是连上问一次 —— 猜错的代价是拼出**不报错但查错对象**的 SQL。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    /// 标识符引号（单字符；`""` = 这个引擎没有引号概念）。
    ///
    /// 用 [`quote_char`](Self::quote_char) 取，它会把「不是恰好一个字符」的情况挡掉。
    #[serde(default)]
    pub identifier_quote: String,
    /// 服务端自报的杂项键值（版本、兼容模式……各 agent 给的东西不一样）。
    ///
    /// ⚠️ **不保证有什么**：有的 agent 给一堆，有的给空表。别拿某个固定键当契约。
    #[serde(default)]
    pub database_info: std::collections::BTreeMap<String, String>,
}

impl ConnectionInfo {
    /// 标识符引号字符；给不出（空串 / 多字符 / 非 ASCII）时返回 `None`。
    ///
    /// 挡掉多字符是有意的：把 `""` 或 `[]` 这种两段式引号当单字符用，
    /// 拼出来的 SQL 会在**每个标识符上**错一次。
    #[must_use]
    pub fn quote_char(&self) -> Option<char> {
        let mut chars = self.identifier_quote.chars();
        match (chars.next(), chars.next()) {
            (Some(quote), None) if quote.is_ascii() => Some(quote),
            _ => None,
        }
    }
}

// ── 结果：元数据 ─────────────────────────────────────────────────────────────

/// `list_databases` 的一条。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseInfo {
    /// 库名。
    #[serde(default)]
    pub name: String,
}

/// `list_tables` / `list_objects` 的一条表（或视图）。
///
/// ⚠️ 键是 **snake_case** 的 `table_type`，不是 `tableType`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableInfo {
    /// 表名。
    #[serde(default)]
    pub name: String,
    /// 类型：`TABLE` / `VIEW` / `MATERIALIZED VIEW` / `FOREIGN TABLE` …（对端原样转达，
    /// 大小写随各库，**别拿它做大写比较之外的假设**）。
    #[serde(default)]
    pub table_type: String,
    /// 注释。
    #[serde(default)]
    pub comment: Option<String>,
    /// 所属 schema（分区表 / 子表这类会填）。
    #[serde(default)]
    pub parent_schema: Option<String>,
    /// 父对象名。
    #[serde(default)]
    pub parent_name: Option<String>,
    /// 是否有效（Oracle 的 `INVALID` 对象给 `false`；多数库不回这个键）。
    #[serde(default)]
    pub valid: Option<bool>,
}

/// `list_objects` 的一条对象（比 [`TableInfo`] 宽：还含序列 / 函数 / 触发器…）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// 对象名。
    #[serde(default)]
    pub name: String,
    /// 对象类型：`TABLE` / `VIEW` / `SEQUENCE` / `FUNCTION` / `PROCEDURE` / `INDEX` …
    #[serde(default)]
    pub object_type: String,
    /// 所属 schema。
    #[serde(default)]
    pub schema: Option<String>,
    /// 注释。
    #[serde(default)]
    pub comment: Option<String>,
    /// 是否有效。
    #[serde(default)]
    pub valid: Option<bool>,
    /// 父对象名（约束挂在表上、索引挂在表上）。
    #[serde(default)]
    pub parent_name: Option<String>,
}

/// `get_columns` 的一条。
///
/// `data_type` 是**对端给的展示用类型串**（`VARCHAR(64)` / `NUMBER(10,2)` 这种已经
/// 带长度精度的），不是 JDBC 的 `java.sql.Types`；要精度另外看 `numeric_*` 那几个。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// 列名。
    #[serde(default)]
    pub name: String,
    /// 展示用类型串。
    #[serde(default)]
    pub data_type: String,
    /// 可空。
    #[serde(default)]
    pub is_nullable: bool,
    /// 默认值表达式（没有就是 `None`，不是空串）。
    #[serde(default)]
    pub column_default: Option<String>,
    /// 是否主键之一。
    #[serde(default)]
    pub is_primary_key: bool,
    /// 附加信息：MySQL 的 `auto_increment`、PG 的 `generated`。
    #[serde(default)]
    pub extra: Option<String>,
    /// 注释。
    #[serde(default)]
    pub comment: Option<String>,
    /// 数值精度。
    #[serde(default)]
    pub numeric_precision: Option<i64>,
    /// 小数位。
    #[serde(default)]
    pub numeric_scale: Option<i64>,
    /// 字符长度上限（字节或字符随库而异）。
    #[serde(default)]
    pub character_maximum_length: Option<i64>,
    /// 字符集。
    #[serde(default)]
    pub character_set: Option<String>,
    /// 排序规则。
    #[serde(default)]
    pub collation: Option<String>,
}

/// `list_indexes` 的一条。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexInfo {
    /// 索引名。
    #[serde(default)]
    pub name: String,
    /// 索引列（按序）。
    #[serde(default)]
    pub columns: Vec<String>,
    /// 唯一索引。
    #[serde(default)]
    pub is_unique: bool,
    /// 主键索引。
    #[serde(default)]
    pub is_primary: bool,
    /// 部分索引的谓词（PG 的 `WHERE`）。
    #[serde(default)]
    pub filter: Option<String>,
    /// 索引类型（`BTREE` / `HASH` / `GIN` …）。
    #[serde(default)]
    pub index_type: Option<String>,
    /// 包含列（PG 的 `INCLUDE`、SQL Server 的含列）。
    #[serde(default)]
    pub included_columns: Option<Vec<String>>,
    /// 注释。
    #[serde(default)]
    pub comment: Option<String>,
    /// 每个键是不是「表达式」（表达式索引的键没有列名，`columns` 里是表达式文本）。
    /// 与 `columns` **按下标一一对应**，可能比 `columns` 短。
    #[serde(default)]
    pub key_is_expression: Vec<bool>,
    /// 是否由约束（主键 / 唯一约束）隐式创建 —— 这类索引**不能单独删**。
    #[serde(default)]
    pub constraint_backed: bool,
}

/// `list_foreign_keys` 的**一列一行**。
///
/// ⚠️ 注意这是**列级**不是约束级：一条复合外键会给多行（同一个 `name`、`ref_table`，
/// 每行一个 `column` / `ref_column`）。要还原成「一条约束多个列」，得按
/// `(name, ref_table)` 归并 —— 上游别指望这里已经归好了。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    /// 约束名（有些 agent 不给，落空串）。
    #[serde(default)]
    pub name: String,
    /// 本表的引用列。
    #[serde(default)]
    pub column: String,
    /// 被引用的表。
    #[serde(default)]
    pub ref_table: String,
    /// 被引用的列（与 `column` 同行一一对应）。
    #[serde(default)]
    pub ref_column: String,
}

/// `list_triggers` 的一条。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerInfo {
    /// 触发器名。
    #[serde(default)]
    pub name: String,
    /// 触发事件（`INSERT` / `UPDATE` / `DELETE`，可多个）。
    #[serde(default)]
    pub event: String,
    /// 触发时机（`BEFORE` / `AFTER` / `INSTEAD OF`）。
    #[serde(default)]
    pub timing: String,
}

// ── 结果：查询 ───────────────────────────────────────────────────────────────

/// `execute_query` / `execute_query_page` / `fetch_query_page` 的结果。
///
/// 三个方法共用一份形状（后两个多出 `session_id` / `has_more`），所以这里只有一个类型；
/// 靠 [`is_result_set`](Self::is_result_set) 区分「这是结果集」还是「这是影响行数」。
///
/// # 两个语义坑
///
/// 1. **`affected_rows` 对 SELECT 是 `0`，不是 `-1`** —— 所以「有没有结果集」只能看
///    `columns` 是否为空，不能看影响行数。反过来 `columns` 为空时 `rows` 一定也是空的。
/// 2. **`truncated` 只说明「被行数上限截断了」**，不代表还有游标可翻：不分页的
///    `execute_query` 也会置它（那时数据是真的拿不到了，得改走分页重跑）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryOutcome {
    /// 列名。**空数组 = 这条语句没有结果集**（INSERT/UPDATE/DDL）。
    #[serde(default)]
    pub columns: Vec<String>,
    /// 与 `columns` 按下标对齐的类型串。部分 agent 会少给几个，别假设等长。
    #[serde(default)]
    pub column_types: Vec<String>,
    /// 数据行。单元格是原样的 JSON 值（数字 / 字符串 / `null` / 嵌套对象）。
    #[serde(default)]
    pub rows: Vec<Vec<Value>>,
    /// 影响行数。DML 是真实行数，SELECT 是 `0`。
    #[serde(default)]
    pub affected_rows: i64,
    /// 对端自己统计的执行耗时（不含网络往返）。
    #[serde(default)]
    pub execution_time_ms: i64,
    /// 是否因为到达行数上限而截断。
    #[serde(default)]
    pub truncated: bool,
    /// 分页游标：只有 `execute_query_page` / `fetch_query_page` 会给。
    /// 拿去喂 [`fetch_page_params`] / [`close_query_session_params`]。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 后面还有没有页。同样是分页专有字段；`session_id` 在而它为 `false` 时，
    /// 游标已经无效，不用再关。
    #[serde(default)]
    pub has_more: bool,
}

impl QueryOutcome {
    /// 这条语句给的是结果集（而不是影响行数）。
    #[must_use]
    pub fn is_result_set(&self) -> bool {
        !self.columns.is_empty()
    }

    /// 列数。
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// 行数。
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 翻页游标（没有就是 `None`）。
    #[must_use]
    pub fn cursor(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 握手回包按线上真实形状解析（camelCase 三个键）。
    #[test]
    fn a_real_handshake_payload_parses() {
        let info: HandshakeInfo = serde_json::from_value(json!({
            "protocolVersion": 2,
            "agentProtocolVersion": 2,
            "capabilities": ["connect", "metadata", "query", "paged_query", "multi_session"],
        }))
        .expect("握手回包");
        assert_eq!(info.protocol_version, 2);
        assert_eq!(info.agent_protocol_version, 2);
        assert!(info.is_multi_session());
        assert!(info.supports(capability::PAGED_QUERY));
        assert!(!info.supports(capability::TRANSACTION));
        // ⚠️ camelCase 被写成了 snake_case 时**不该**解析出来。
        assert!(
            serde_json::from_value::<HandshakeInfo>(json!({ "protocol_version": 2 })).is_err(),
            "还原成 snake_case 就说明 rename_all 掉了"
        );
    }

    /// 少了 `agentProtocolVersion` 也能用（个别 agent 只回 `protocolVersion`）。
    #[test]
    fn a_handshake_without_the_agent_version_still_parses() {
        let info: HandshakeInfo =
            serde_json::from_value(json!({ "protocolVersion": 1 })).expect("只回一个版本号");
        assert_eq!(info.protocol_version, 1);
        assert_eq!(info.agent_protocol_version, 0);
        assert!(info.capabilities.is_empty());
        assert!(!info.is_multi_session());
    }

    /// 元数据字段是 snake_case —— 这条钉住「照抄对端」的口径（改成 camelCase 就红）。
    #[test]
    fn metadata_payloads_use_the_wire_names() {
        let table: TableInfo = serde_json::from_value(json!({
            "name": "orders",
            "table_type": "BASE TABLE",
            "comment": null,
            "parent_schema": "public",
            "valid": true,
        }))
        .expect("表信息");
        assert_eq!(table.table_type, "BASE TABLE");
        assert_eq!(table.parent_schema.as_deref(), Some("public"));
        assert_eq!(table.valid, Some(true));

        let column: ColumnInfo = serde_json::from_value(json!({
            "name": "amount",
            "data_type": "NUMBER(10,2)",
            "is_nullable": false,
            "column_default": "0",
            "is_primary_key": true,
            "numeric_precision": 10,
            "numeric_scale": 2,
            "character_maximum_length": null,
        }))
        .expect("列信息");
        assert!(!column.is_nullable);
        assert!(column.is_primary_key);
        assert_eq!(column.column_default.as_deref(), Some("0"));
        assert_eq!(column.numeric_precision, Some(10));
        assert_eq!(column.character_maximum_length, None);

        let index: IndexInfo = serde_json::from_value(json!({
            "name": "orders_pkey",
            "columns": ["id"],
            "is_unique": true,
            "is_primary": true,
            "constraint_backed": true,
        }))
        .expect("索引信息");
        assert!(index.is_unique);
        assert!(index.constraint_backed);
        assert!(index.included_columns.is_none());
        assert!(index.key_is_expression.is_empty());

        let object: ObjectInfo = serde_json::from_value(json!({
            "name": "orders_all",
            "object_type": "VIEW",
            "schema": "public",
        }))
        .expect("对象信息");
        assert_eq!(object.object_type, "VIEW");

        // `table_type` 才是线上键；`tableType` 是错的写法，解析出来会变成空串。
        let wrong: TableInfo =
            serde_json::from_value(json!({ "name": "orders", "tableType": "VIEW" }))
                .expect("忽略未知键");
        assert_eq!(wrong.table_type, "");
    }

    /// 一条语句的结果集与 DML 走同一个类型，靠 `columns` 区分。
    #[test]
    fn a_select_result_set_is_told_apart_from_a_dml_count() {
        let select: QueryOutcome = serde_json::from_value(json!({
            "columns": ["id", "name"],
            "column_types": ["int4", "text"],
            "rows": [[1, "a"], [2, null]],
            "affected_rows": 0,
            "execution_time_ms": 3,
            "truncated": false,
        }))
        .expect("查询结果");
        assert!(select.is_result_set());
        assert_eq!(select.column_count(), 2);
        assert_eq!(select.row_count(), 2);
        assert_eq!(select.cursor(), None);
        assert_eq!(select.rows[1][1], Value::Null);

        let update: QueryOutcome = serde_json::from_value(json!({
            "columns": [],
            "rows": [],
            "affected_rows": 7,
        }))
        .expect("DML 结果");
        assert!(!update.is_result_set());
        assert_eq!(update.affected_rows, 7);
    }

    /// 分页结果多出 `session_id` / `has_more`，游标要能取到。
    #[test]
    fn a_paged_result_carries_the_cursor() {
        let page: QueryOutcome = serde_json::from_value(json!({
            "columns": ["id"],
            "column_types": ["int4"],
            "rows": [[1]],
            "affected_rows": 0,
            "execution_time_ms": 1,
            "truncated": false,
            "session_id": "cursor-7",
            "has_more": true,
        }))
        .expect("分页结果");
        assert_eq!(page.cursor(), Some("cursor-7"));
        assert!(page.has_more);
    }

    /// 连接参数：`sessionRole` 是 camelCase、`url_params` 是 snake_case，
    /// `open_session` 还要把会话 id 塞进去。
    #[test]
    fn connection_parameters_keep_the_two_casing_conventions() {
        let request = ConnectRequest {
            host: "10.0.0.5".to_owned(),
            port: 5236,
            database: "DAMENG".to_owned(),
            username: "SYSDBA".to_owned(),
            password: "pw".to_owned(),
            url_params: "charset=UTF-8".to_owned(),
            session_role: SessionRole::Metadata,
            ..ConnectRequest::default()
        };
        let params = request
            .clone()
            .into_session_params("session-1")
            .expect("编码");
        let map = params.as_object().expect("对象");
        assert_eq!(map.get("sessionRole"), Some(&json!("metadata")));
        assert_eq!(map.get("url_params"), Some(&json!("charset=UTF-8")));
        assert_eq!(map.get(SESSION_FIELD), Some(&json!("session-1")));
        assert_eq!(map.get("port"), Some(&json!(5236)));
        // 没填的可选项不该以 `null` 出现在参数里。
        assert!(!map.contains_key("ca_cert_path"), "{map:?}");

        // 老路径（v1 单会话）没有会话 id。
        let legacy = request.into_connect_params().expect("编码");
        assert!(
            !legacy
                .as_object()
                .expect("对象")
                .contains_key(SESSION_FIELD)
        );

        // 角色默认是 workload。
        assert_eq!(SessionRole::default().wire_name(), "workload");
    }

    /// 查询参数：库 / schema 为空时**不出现在参数里**（对端把空串当「不切库」，
    /// 但老 agent 会因为多一个空键而报错，所以宁可省略）。
    #[test]
    fn query_parameters_omit_the_empty_scope() {
        let params = query_params("select 1", None, Some(""), 500);
        let map = params.as_object().expect("对象");
        assert_eq!(map.get("sql"), Some(&json!("select 1")));
        assert_eq!(map.get("maxRows"), Some(&json!(500)));
        assert!(!map.contains_key("database"));
        assert!(!map.contains_key("schema"));

        let scoped = query_params("select 1", Some("app"), Some("public"), 10);
        assert_eq!(scoped.get("database"), Some(&json!("app")));
        assert_eq!(scoped.get("schema"), Some(&json!("public")));

        let page = query_page_params("select 1", Some("app"), None, 200, 1000);
        assert_eq!(page.get("pageSize"), Some(&json!(200)));
        assert_eq!(page.get("maxRows"), Some(&json!(1000)));
    }

    /// 游标参数用 `sessionId`（**不是** `agentSessionId`）—— 两个键混用会表现为
    /// 「翻页永远失败」，所以单独钉一条。
    #[test]
    fn cursor_parameters_use_the_cursor_field() {
        let fetch = fetch_page_params("cursor-7", 250);
        assert_eq!(fetch.get(CURSOR_SESSION_FIELD), Some(&json!("cursor-7")));
        assert_eq!(fetch.get("pageSize"), Some(&json!(250)));
        assert!(fetch.get(SESSION_FIELD).is_none());

        let close = close_query_session_params("cursor-7");
        assert_eq!(close, json!({ "sessionId": "cursor-7" }));

        // 会话级参数反过来只能用 `agentSessionId`。
        let session = session_params("session-1");
        assert_eq!(session, json!({ "agentSessionId": "session-1" }));
        assert_ne!(SESSION_FIELD, CURSOR_SESSION_FIELD);
    }

    /// 连接信息：只认**恰好一个 ASCII 字符**的引号。
    #[test]
    fn the_identifier_quote_must_be_a_single_character() {
        let info: ConnectionInfo = serde_json::from_value(json!({
            "identifierQuote": "`",
            "databaseInfo": { "version": "8.1.3" },
        }))
        .expect("连接信息");
        assert_eq!(info.quote_char(), Some('`'));
        assert_eq!(
            info.database_info.get("version").map(String::as_str),
            Some("8.1.3")
        );

        let empty: ConnectionInfo =
            serde_json::from_value(json!({ "identifierQuote": "" })).expect("空引号");
        assert_eq!(empty.quote_char(), None);
        let wide: ConnectionInfo =
            serde_json::from_value(json!({ "identifierQuote": "[]" })).expect("两段式引号");
        assert_eq!(wide.quote_char(), None, "两段式引号不能当单字符用");
        let non_ascii: ConnectionInfo =
            serde_json::from_value(json!({ "identifierQuote": "「" })).expect("全角引号");
        assert_eq!(non_ascii.quote_char(), None);
    }

    /// 方法名清单没有重复、全是小写 + 下划线，且长度就是 41。
    #[test]
    fn the_method_catalogue_is_well_formed() {
        assert_eq!(method::ALL.len(), 41);
        let mut seen = method::ALL.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 41, "有重名方法");
        for name in method::ALL {
            assert!(!name.is_empty());
            assert!(
                name.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "方法名不该有大写或其他字符：{name}"
            );
            assert!(name.is_ascii(), "方法名必须是 ASCII：{name}");
        }
    }

    /// 能力名同样是 ASCII 小写；`supports` 不做大小写折叠，写错就是 `false`。
    #[test]
    fn capability_lookups_are_exact() {
        let info = HandshakeInfo {
            protocol_version: 2,
            agent_protocol_version: 2,
            capabilities: vec![capability::MULTI_SESSION.to_owned()],
        };
        assert!(info.supports(capability::MULTI_SESSION));
        assert!(!info.supports("Multi_Session"));
        assert!(!info.supports(""));
    }
}
