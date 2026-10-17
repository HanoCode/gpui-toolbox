//! 连接档案：引擎种类、连接配置与凭据薄封装。
//!
//! 结构体本身即 wire（每字段 `#[serde(default)]`，旧存档缺字段可补默认值），
//! 由调用方序列化成 JSON 存进 `duck-store` 的 kv 单键。**口令不进 kv**：
//! 只经 [`store_password`] / [`load_password`] / [`delete_password`] 走
//! `duck-platform::secret`（应用自有凭据文件 `credentials.json`，值为 vault 密文）。

use serde::{Deserialize, Serialize};

use duck_platform::secret::SecretError;

/// 支持的引擎。`ALL` 的顺序即新建连接表单里的档位顺序。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DatabaseKind {
    /// SQLite（本地文件）。
    #[default]
    #[serde(rename = "sqlite")]
    Sqlite,
    /// PostgreSQL。
    #[serde(rename = "postgres")]
    Postgres,
    /// MySQL / MariaDB。
    #[serde(rename = "mysql")]
    Mysql,
    /// ClickHouse（走 HTTP 接口，见 `drivers/clickhouse.rs`）。
    #[serde(rename = "clickhouse")]
    ClickHouse,
    /// Microsoft SQL Server（见 `drivers/mssql.rs`）。
    #[serde(rename = "sqlserver")]
    SqlServer,
    /// MongoDB（文档库，见 `drivers/mongo.rs`）。
    #[serde(rename = "mongo")]
    Mongo,
    /// Elasticsearch（搜索 / 日志库，走 REST 接口，见 `drivers/elasticsearch.rs`）。
    #[serde(rename = "elasticsearch")]
    Elasticsearch,
    /// **子进程（agent）轨**：达梦 / 金仓 / Oracle / DB2 / Hive …… 这些库没有（也不会有）
    /// Rust 原生驱动，我们跑对端给的驱动进程、用 JSON-RPC 说话（见 `drivers/agent.rs`）。
    ///
    /// ⚠️ **「到底哪一条驱动」不在这个枚举里** —— 它是 [`ConnectionConfig::driver`] 里的
    /// 一个目录键（`dameng` / `oracle` / …），取值来自 `duck-drivers` 的目录表。
    ///
    /// 为什么不是一个引擎一个变体：那 55 条驱动的清单是**生成的**（每当上游注册表更新就
    /// 重生成），把它们抄成枚举变体等于第二份真源 —— 上游加一条，我们的枚举就漏一条，
    /// 而漏掉的表现是「界面里根本没有这个数据库」。放进数据里就没有这个问题。
    ///
    /// 代价是这一档的能力开关（端口 / 引号 / 分页）**问不了枚举本身**，得连驱动键一起问
    /// —— 所以那几个方法对 `Agent` 一律给保守答案，详见各自的文档。
    #[serde(rename = "agent")]
    Agent,
}

impl DatabaseKind {
    /// **内建**档位（宿主里编着驱动的那几条）。
    ///
    /// ⚠️ 不等于「界面能选的全部引擎」：agent 轨那几十条**是数据不是枚举**
    /// （见 [`Self::Agent`] 的文档），它们的清单在 `duck-drivers` 的目录表里。
    /// 界面里「新建连接」的引擎下拉要把两边合起来看。
    pub const ALL: [Self; 7] = [
        Self::Sqlite,
        Self::Postgres,
        Self::Mysql,
        Self::ClickHouse,
        Self::SqlServer,
        Self::Mongo,
        Self::Elasticsearch,
    ];

    /// wire 值（落盘用，稳定）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::ClickHouse => "clickhouse",
            Self::SqlServer => "sqlserver",
            Self::Mongo => "mongo",
            Self::Elasticsearch => "elasticsearch",
            Self::Agent => "agent",
        }
    }

    /// 解析 wire 值；未知值回落 SQLite（首个档位）。
    ///
    /// ⚠️ 别名只认**历史写法**：`es` 是本仓早期草稿里用过的短名，收下它以免旧档案
    /// 静默落成 SQLite；新档一律走 [`Self::wire_name`] 的全名。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "postgres" => Self::Postgres,
            "mysql" => Self::Mysql,
            "clickhouse" => Self::ClickHouse,
            "sqlserver" => Self::SqlServer,
            "mongo" => Self::Mongo,
            "elasticsearch" | "es" => Self::Elasticsearch,
            "agent" => Self::Agent,
            _ => Self::Sqlite,
        }
    }

    /// 是否是内建驱动（宿主里编着实现），即**不需要下载驱动**。
    ///
    /// ⚠️ 与 [`Self::needs_driver_install`] 不是一回事的反面也不能互推：`Agent` 一律
    /// 要装，而 `Sqlite` 这类内建的一个字节都不用下。
    #[must_use]
    pub const fn is_builtin(self) -> bool {
        !matches!(self, Self::Agent)
    }

    /// 建连前是否必须先装好一条外部驱动。
    #[must_use]
    pub const fn needs_driver_install(self) -> bool {
        matches!(self, Self::Agent)
    }

    /// 引擎的 TCP 缺省端口；SQLite 无网络端口，返回 `None`。
    ///
    /// ClickHouse 取 **HTTP 接口**的 8123（不是原生协议的 9000）——本条驱动走 HTTP。
    /// Elasticsearch 取 REST 的 9200（不是节点间通信的 9300）。
    ///
    /// ⚠️ `Agent` 返回 `None`，这**不代表它没有端口**（达梦就是 5236）：端口随驱动走，
    /// 而枚举里没有驱动键。要拿真值请用 [`ConnectionConfig::default_port`]，
    /// 它会去驱动目录里查这条驱动的 `defaultPort`。
    /// 这里的 `None` 是「问我不出答案」，界面不该把它当「不用填端口」。
    #[must_use]
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Self::Sqlite | Self::Agent => None,
            Self::Postgres => Some(5432),
            Self::Mysql => Some(3306),
            Self::ClickHouse => Some(8123),
            Self::SqlServer => Some(1433),
            Self::Mongo => Some(27017),
            Self::Elasticsearch => Some(9200),
        }
    }

    /// 是否需要 TCP 主机名（SQLite 走本地文件，不填）。
    ///
    /// `Agent` 走网络：对端是**独立进程**，进程与库之间才是网络（大多数情况同机直连），
    /// 但从我们的视角它终究要一个主机名。
    #[must_use]
    pub const fn needs_host(self) -> bool {
        !matches!(self, Self::Sqlite)
    }

    /// 库名是不是**必填**。
    ///
    /// ⚠️ 只有 SQLite 必填——那边 `database` 字段装的是**文件路径**，空路径根本无从
    /// 打开。其余六个引擎留空都是**合法**的：连接本身不需要先选定一个库，落点由引擎
    /// 自己决定。这一条曾经写成「网络引擎一律必填」，把 MySQL 这种协议上根本不要求
    /// 库名的引擎也拦在门外（其他客户端不填库名照样连）——是校验层比驱动更严。
    ///
    /// 留空时各自的落点**不在这份内核里成文**（内核不吐成品中文），由界面按
    /// `db.rs` 的引擎分支拼提示；驱动侧的实现见 `drivers/mysql.rs`（不选库）、
    /// `drivers/postgres.rs`（省略 `dbname`，服务端取用户名同名库）、
    /// `drivers/clickhouse.rs`（回落 `default`）、`drivers/mssql.rs`（登录名默认库）、
    /// `drivers/mongo.rs`（三级回落，全空即未选定）、`drivers/elasticsearch.rs`
    /// （留空 = 不限索引，目录列出全部索引）。
    #[must_use]
    pub const fn needs_database(self) -> bool {
        matches!(self, Self::Sqlite)
    }

    /// 该引擎包裹标识符（表名 / 列名）用的引号字符；`None` = 引擎没有 SQL 标识符概念
    /// （MongoDB 走 JSON 命令、Elasticsearch 走 REST 路径，都没有「带引号的表名」）。
    ///
    /// 与各驱动拼 SQL 时传给 `quote_identifier` 的字符**同源口径**：PostgreSQL / SQLite /
    /// SQL Server 用 `"`，MySQL 与 ClickHouse 用反引号（MySQL 默认 `ANSI_QUOTES` 关闭，
    /// 双引号会被当字符串字面量）。界面侧要拼「能直接执行的 SQL 草稿」时用它
    /// ——结果集的 INSERT 导出、对象树「生成 SELECT」骨架。
    ///
    /// ⚠️ `Agent` 返回 `None`：引号是**每条驱动不同**的事，而枚举里没有驱动键。
    /// 猜一个的代价很大 —— MySQL 系的双引号会被服务端当字符串字面量，拼出来的
    /// `SELECT * FROM "users"` 不报错，只是**查了另一张表**（或直接查不到）。
    /// 真值在连上之后由对端经 `connection_info` 的 `identifierQuote` 给出，
    /// 见 `drivers/agent.rs`；没连上之前宁可让「生成 SELECT」这条路不可用。
    #[must_use]
    pub const fn identifier_quote(self) -> Option<char> {
        match self {
            Self::Sqlite | Self::Postgres | Self::SqlServer => Some('"'),
            Self::Mysql | Self::ClickHouse => Some('`'),
            Self::Mongo | Self::Elasticsearch | Self::Agent => None,
        }
    }

    /// 该引擎是否支持结果集「分页续拉」（[`crate::drivers::Connection::execute_paged`]）。
    ///
    /// **这是「能不能续拉」的唯一判据**：内核的 `execute_paged` 与界面结果区的
    /// 「加载更多」都查它，两边口径不会再各写一份。
    ///
    /// - SQLite / PostgreSQL / MySQL / ClickHouse：外层包 `LIMIT/OFFSET` 可靠；
    /// - SQL Server：派生表不能带 `ORDER BY`，而外层又必须 `ORDER BY` 才能
    ///   `OFFSET/FETCH`——顺序不定会让翻页重复 / 漏行，故**不支持**（显式拒绝，不返回错页）；
    /// - MongoDB / Elasticsearch：没有 SQL 分页语法，同样不支持。
    /// - `Agent`：**支持，而且是真游标**——不走 `LIMIT/OFFSET` 的外层包装，而是对端的
    ///   `execute_query_page` / `fetch_query_page`（服务端游标，翻页不会重复/漏行，
    ///   也不会因为外层再包一层而丢掉 `ORDER BY`）。
    ///   ⚠️ 前提是对端握手时声明了 `paged_query`；没声明的会在
    ///   [`crate::drivers::Connection::execute_paged`] 上如实报「不支持」。
    ///
    /// ⚠️ **agent 档只有两层加起来才是完整答案**：这一层回答的是「这一**轨**有没有这条
    /// 通路」（有，所以界面别把它藏掉），而「**这个驱动**有没有」要问连接：
    /// [`crate::drivers::Connection::supports_paged_read`]。手上有连接就查后者。
    #[must_use]
    pub const fn supports_paged_read(self) -> bool {
        matches!(
            self,
            Self::Sqlite | Self::Postgres | Self::Mysql | Self::ClickHouse | Self::Agent
        )
    }

    /// 该引擎是否支持**实例级内省**（实例概览 / 实例指标 / 实例检查器）。
    ///
    /// **这是「要不要画这三个入口」的唯一判据**：内核的
    /// [`crate::drivers::Connection::instance_overview`] /
    /// [`crate::drivers::Connection::instance_table`] 与界面的左栏树都查它，
    /// 两边口径不会再各写一份。
    ///
    /// - SQLite / PostgreSQL / MySQL：有——SQLite 虽无服务端，但 PRAGMA 能给出
    ///   真实的页级体量与连接挂载状态；
    /// - SQL Server / ClickHouse / MongoDB / Elasticsearch：本轮未实作，界面**不画**
    ///   入口。宁可少一个节点，也不要给一个点开就报「不支持」的按钮（「不支持」与
    ///   「执行失败」必须分得开——后者用户能改，前者不能）。
    /// - `Agent`：**不画**。协议里只有「取执行计划」（`get_explain_info`），
    ///   没有实例级指标那一类方法，硬凑就得靠方言查询，而 agent 轨恰恰是
    ///   「方言最杂」的那一档（达梦 / Hive / Teradata 各写一套）。
    #[must_use]
    pub const fn supports_instance_insight(self) -> bool {
        matches!(self, Self::Sqlite | Self::Postgres | Self::Mysql)
    }
}

/// 环境标签：给连接列表分组着色用，防「把生产库当测试库」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EnvTag {
    /// 无标签。
    #[default]
    #[serde(rename = "none")]
    None,
    /// 开发。
    #[serde(rename = "dev")]
    Dev,
    /// 测试。
    #[serde(rename = "test")]
    Test,
    /// 预发布。
    #[serde(rename = "staging")]
    Staging,
    /// 生产。
    #[serde(rename = "prod")]
    Prod,
}

impl EnvTag {
    /// 全部档位（顺序即表单顺序）。
    pub const ALL: [Self; 5] = [Self::None, Self::Dev, Self::Test, Self::Staging, Self::Prod];

    /// wire 值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dev => "dev",
            Self::Test => "test",
            Self::Staging => "staging",
            Self::Prod => "prod",
        }
    }

    /// 解析 wire 值；未知值回落无标签。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "dev" => Self::Dev,
            "test" => Self::Test,
            "staging" => Self::Staging,
            "prod" => Self::Prod,
            _ => Self::None,
        }
    }
}

/// TLS 策略：`Disable` 明文、`Trust` 按系统信任链校验证书、`DangerAcceptInvalid`
/// 接受无效证书（自签测试库）。
///
/// **六条网络引擎均已接入 TLS**（PostgreSQL / MySQL / ClickHouse / SQL Server /
/// MongoDB 走 rustls 或各自的 TLS 开关，两条 HTTP 引擎走 https 端点；见
/// `drivers/*.rs`）。**SQLite 无网络，本档位对它无意义**——选了也不会有任何效果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsMode {
    /// 不使用 TLS。
    #[default]
    #[serde(rename = "disable")]
    Disable,
    /// 使用 TLS 并校验证书（默认信任链）。
    #[serde(rename = "trust")]
    Trust,
    /// 使用 TLS 但接受无效证书（自签测试库）。
    #[serde(rename = "danger-accept-invalid")]
    DangerAcceptInvalid,
}

impl TlsMode {
    /// 全部档位（顺序即表单顺序）。
    pub const ALL: [Self; 3] = [Self::Disable, Self::Trust, Self::DangerAcceptInvalid];

    /// wire 值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Trust => "trust",
            Self::DangerAcceptInvalid => "danger-accept-invalid",
        }
    }

    /// 解析 wire 值；未知值回落不启用。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "trust" => Self::Trust,
            "danger-accept-invalid" => Self::DangerAcceptInvalid,
            _ => Self::Disable,
        }
    }
}

/// 连接的**认证方式**（界面下拉；缺省 = 用户名 + 口令，兼容旧档案）。
///
/// 与 dbx 同一条取向：认证方式不是一个每引擎一个枚举的大集合，而是少数几个通用档位，
/// 每个引擎只认得其中几档（[`Self::available_for`]），界面按它显隐。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthMode {
    /// 用户名 + 口令（绝大多数引擎的默认形态）。
    #[default]
    #[serde(rename = "password")]
    Password,
    /// 无认证（PG 的 trust、未开安全的 MongoDB / Elasticsearch）。
    #[serde(rename = "none")]
    None_,
    /// 整条连接串（`postgresql://…` / `mysql://…` / `mongodb://…`）。
    #[serde(rename = "connection-string")]
    ConnectionString,
    /// API Key（Elasticsearch 的 `Authorization: ApiKey …`）。
    #[serde(rename = "api-key")]
    ApiKey,
}

impl AuthMode {
    /// 全部档位（顺序即表单下拉顺序）。
    pub const ALL: [Self; 4] = [
        Self::Password,
        Self::None_,
        Self::ConnectionString,
        Self::ApiKey,
    ];

    /// wire 值（落盘用，稳定）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::None_ => "none",
            Self::ConnectionString => "connection-string",
            Self::ApiKey => "api-key",
        }
    }

    /// 解析 wire 值；未知值回落口令认证。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "none" => Self::None_,
            "connection-string" => Self::ConnectionString,
            "api-key" => Self::ApiKey,
            _ => Self::Password,
        }
    }

    /// 该引擎是否认得这一档（界面下拉据它显隐；驱动侧的真正消费见 `drivers/*.rs`）。
    ///
    /// - 无认证：PG（trust）与未开安全的 MongoDB / Elasticsearch；MySQL 的账号模型
    ///   决定了「无认证」没有意义（账号必填），不给这一档。
    /// - 连接串：只有 URI 语义成熟的 PG / MySQL / MongoDB 三家。
    /// - API Key：只有 Elasticsearch（它的 REST 认证头原生吃这一档）。
    #[must_use]
    pub const fn available_for(self, kind: DatabaseKind) -> bool {
        match self {
            Self::Password => true,
            Self::None_ => matches!(
                kind,
                DatabaseKind::Postgres | DatabaseKind::Mongo | DatabaseKind::Elasticsearch
            ),
            Self::ConnectionString => matches!(
                kind,
                DatabaseKind::Postgres | DatabaseKind::Mysql | DatabaseKind::Mongo
            ),
            Self::ApiKey => matches!(kind, DatabaseKind::Elasticsearch),
        }
    }
}

/// SSH 隧道的**认证方式**（跳板机这一跳用哪个凭据）。
///
/// 缺省 = 口令（旧行为）；私钥口令不在这里 —— 与 SSH 口令同一条纪律，走凭据文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SshAuthMethod {
    /// 口令（`SSH_ASKPASS` 注入）。
    #[default]
    #[serde(rename = "password")]
    Password,
    /// 私钥文件（无口令）。
    #[serde(rename = "key")]
    Key,
    /// 私钥文件 + 私钥口令（加密私钥）。
    #[serde(rename = "key+password")]
    KeyAndPassword,
    /// 本机 ssh-agent（`SSH_AUTH_SOCK` 原样透传）。
    #[serde(rename = "agent")]
    Agent,
}

impl SshAuthMethod {
    /// 全部档位（顺序即表单下拉顺序）。
    pub const ALL: [Self; 4] = [Self::Password, Self::Key, Self::KeyAndPassword, Self::Agent];

    /// wire 值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Key => "key",
            Self::KeyAndPassword => "key+password",
            Self::Agent => "agent",
        }
    }

    /// 解析 wire 值；未知值回落口令。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "key" => Self::Key,
            "key+password" => Self::KeyAndPassword,
            "agent" => Self::Agent,
            _ => Self::Password,
        }
    }

    /// 这一档要不要拉起 askpass（口令 / 加密私钥的口令都经它注入）。
    #[must_use]
    pub const fn needs_askpass(self) -> bool {
        matches!(self, Self::Password | Self::KeyAndPassword)
    }

    /// 这一档要不要给 ssh 传 `-i <key>`。
    #[must_use]
    pub const fn needs_key(self) -> bool {
        matches!(self, Self::Key | Self::KeyAndPassword)
    }
}

/// 一条数据库连接档案。
///
/// 即 serde wire 结构：新增字段一律 `Option` + `#[serde(default)]`，
/// 保证旧存档逐字段可读（与 `AppearanceWire` 同一条纪律）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// 稳定 id（`conn-` + 随机后缀），凭据键名与档案关联都用它。
    #[serde(default)]
    pub id: String,
    /// 显示名（用户可改）。
    #[serde(default)]
    pub name: String,
    /// 引擎。
    #[serde(default)]
    pub kind: DatabaseKind,
    /// **要装哪条外部驱动**：`duck-drivers` 的目录键（`dameng` / `oracle` / `db2` / …）。
    ///
    /// 只有 [`DatabaseKind::Agent`] 会看它；内建引擎的档案里它是 `None`。
    /// `kind == Agent` 而它是 `None` / 空串，是一份**坏档案**（建连直接报 Config 错），
    /// 不是「随便挑一条」——猜错驱动会连到**另一家的库**上去，那比连不上危险得多。
    #[serde(default)]
    pub driver: Option<String>,
    /// **牌子**：`duck-app` 品牌表里的键（`mariadb` / `tidb` / `oceanbase` / `gbase8s` / …）。
    ///
    /// 一个线协议下面站着**十几个牌子**（`mysql` 协议就有 MySQL / MariaDB / TiDB /
    /// OceanBase / Doris / StarRocks … 12 个），档位表达不了这个粒度：
    /// 它们的缺省端口与惯用账号都不一样（3306 / 4000 / 2883 / 9030 …）。
    /// 这个字段就是那份「用户当年选的是哪个牌子」的记忆，dbx 同一个字段叫
    /// `driver_profile`。
    ///
    /// ⚠️ **内核只当字符串过路**（品牌表在界面侧，内核不认识它）：它不参与建连，
    /// 只在 agent 轨上原样转给对端当 `driver_profile`（对端按它挑方言细节）。
    /// 认不出的键不影响建连 —— 界面回落成档位自己的名字与图标。
    #[serde(default)]
    pub profile: Option<String>,
    /// **URL 附加参数**（JDBC URL 里 `?` 之后那截），品牌自己的缺省值由品牌表给。
    ///
    /// 这是「不填就连不上」的那一类：星环 Argo / Impala 要 `auth=noSasl`、
    /// Kyuubi 要 `auth=NONE`。只有 agent 轨消费它（内建驱动的连接串是各自拼的）。
    #[serde(default)]
    pub url_params: Option<String>,
    /// **GBase 8s 的服务名**（GBASESQLI 里的 `gbase_server`，dbx 同名）。
    ///
    /// 只有品牌键 `gbase8s` 用到它。对端 JDBC agent 有缺省回落（按 host 推一个），
    /// 所以留空合法 —— 但真实环境的服务名是管理员定的，回落猜错就 `908` 报错，
    /// 这是 GBase 8s 连不上的头号成因，值得给输入框。
    #[serde(default)]
    pub gbase_server: Option<String>,
    /// **Informix 的 server 名**（JDBC URL 里的 `INFORMIXSERVER=…`，dbx 同名）。
    ///
    /// 只有 Informix 档用到。与 [`Self::gbase_server`] 同理：对端有缺省回落，
    /// 留空合法，填了才准。
    #[serde(default)]
    pub informix_server: Option<String>,
    /// TCP 主机名（SQLite 忽略）。
    #[serde(default)]
    pub host: String,
    /// TCP 端口；`None` 用引擎缺省端口。
    #[serde(default)]
    pub port: Option<u16>,
    /// 用户名（SQLite 忽略）。
    #[serde(default)]
    pub user: String,
    /// 库名；SQLite 时是**数据库文件路径**（`:memory:` 允许）。
    ///
    /// **网络引擎可留空**（见 [`ConnectionConfig::validate`]）——留空即「不指定默认
    /// 库」，连接照建，落点由引擎自己决定（MySQL / MongoDB 甚至允许根本不选库）。
    #[serde(default)]
    pub database: String,
    /// TLS 策略。
    #[serde(default)]
    pub tls: TlsMode,
    /// 认证方式（缺省口令；旧档案没有这个字段，读进来就是口令认证）。
    #[serde(default)]
    pub auth_mode: AuthMode,
    /// **连接串模式**的连接串原文（`auth_mode == ConnectionString` 时生效）。
    ///
    /// 驱动侧拿它整条直连（host / port / user / password 等字段全部忽略）；
    /// 留空 + 连接串模式是一份坏档案（建连报 Config 错，见 [`Self::validate`]）。
    #[serde(default)]
    pub connection_string: Option<String>,
    /// 自签 CA 证书路径（PEM；`None` = 只用编译进二进制的公网根集）。
    #[serde(default)]
    pub ca_cert_path: Option<String>,
    /// 客户端证书路径（PEM；双向 TLS，目前 PostgreSQL / MySQL 驱动消费）。
    #[serde(default)]
    pub client_cert_path: Option<String>,
    /// 客户端私钥路径（PEM；口令不在这里 —— 走凭据文件 `dbconn:{id}:ssh_key`）。
    #[serde(default)]
    pub client_key_path: Option<String>,
    /// **侧栏可见库白名单**（`None` = 不过滤，全部显示）。
    ///
    /// 「选择显示数据库」把勾选结果存在这里；勾选恰好是全部时**清空**回 `None`
    /// ——白名单存死快照会让之后新建的库永远看不见。
    #[serde(default)]
    pub visible_databases: Option<Vec<String>>,
    /// 环境标签。
    #[serde(default)]
    pub env_tag: EnvTag,
    /// 自定义分组名（连接列表按它分组）。
    #[serde(default)]
    pub group: Option<String>,
    /// SSH 隧道（`None` = 直连）。
    #[serde(default)]
    pub ssh_tunnel: Option<SshTunnelConfig>,
    /// SOCKS5 / HTTP CONNECT 代理（`None` = 不走代理，见 [`crate::proxy`]）。
    ///
    /// 代理口令不在这里：与 SSH 口令同一条纪律，只走凭据文件
    /// `dbconn:{id}:proxy`（见 [`proxy_secret_key`]）。
    #[serde(default)]
    pub proxy: Option<crate::proxy::ProxyConfig>,
    /// AWS SSM 端口转发（`None` = 不用，见 [`crate::ssm`]）。
    ///
    /// 走用户本机已装的 `aws` CLI，本项目不引 AWS SDK、不存 AWS 凭据。
    #[serde(default)]
    pub ssm: Option<crate::ssm::SsmConfig>,
    /// 引用的「可复用传输档案」id（`None` = 用本档案自己的上面三档）。
    ///
    /// 档案本体由界面侧存 kv（内核不认识它），建连前由界面**展开**成上面三档 ——
    /// 内核看到的永远是一份填好的 [`ConnectionConfig`]，[`Self::transport`] 也随之生效。
    /// 这里只留一个引用，所以「一份公司 SOCKS5 出口被五条连接共用」是改一处、
    /// 五条连接下次建连全都生效。
    #[serde(default)]
    pub tunnel_profile: Option<String>,
    /// 该连接的**默认查询超时**（秒）；`None` / `0` = 用引擎无关的全局默认（30 秒）。
    ///
    /// 只作用于「可能很久」的读路径（SQL 执行、表页加载、AI 只读自动执行）；
    /// 单次覆盖由查询选项面板给，见 `docs/database-design.md` §4.4。
    #[serde(default)]
    pub default_timeout_secs: Option<u64>,
    /// 连接级**只读档**（`docs/database-design.md` §4.8）：开启后写语句会被拒绝。
    ///
    /// **前四引擎由服务器兜底**（SQLite `query_only` / PG `default_transaction_read_only` /
    /// MySQL `SESSION TRANSACTION READ ONLY` / ClickHouse 每请求带 `readonly=1`），
    /// 即便界面漏了一道拦截，写语句也会在服务端报错。
    /// ⚠️ **SQL Server / MongoDB / Elasticsearch 没有会话级只读开关**——只读档由驱动在
    /// 写语句进引擎之前拦下（未知操作一律当写，默认拒绝）：可信，但不是服务器兜底。
    /// 真要防住生产库，仍然应该用只读账号。生产库必开。
    #[serde(default)]
    pub read_only: bool,
    /// 上次浏览的 schema / 库（`None` = 引擎的当前命名空间）。
    ///
    /// 只影响界面的默认落点，不改变连接行为。
    #[serde(default)]
    pub schema: Option<String>,
    /// 连接钩子（建连 / 断开前后跑的本机命令，见 [`crate::hooks`]）。
    ///
    /// 缺省空集合：老档案读进来就是「没有钩子」，建连路径据此**不取快照、不起后台任务**。
    #[serde(default)]
    pub hooks: crate::hooks::ConnectionHooks,
}

/// SSH 隧道配置（跳板机信息；SSH 口令走凭据文件 `dbconn:{id}:ssh`，不入 kv）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    /// 跳板机主机。
    #[serde(default)]
    pub host: String,
    /// 跳板机端口；`None` = 22。
    #[serde(default)]
    pub port: Option<u16>,
    /// 跳板机用户。
    #[serde(default)]
    pub user: String,
    /// 转发目标主机（相对跳板机解析；空 = 跳板机本机）。
    #[serde(default)]
    pub remote_host: String,
    /// 认证方式（缺省口令；旧档案读进来就是口令认证）。
    #[serde(default)]
    pub auth_method: SshAuthMethod,
    /// 私钥文件路径（`auth_method` 为私钥两档时必填）。
    #[serde(default)]
    pub key_path: Option<String>,
}

impl ConnectionConfig {
    /// 连接显示名兜底：未命名时用「主机 / 库」拼一个。
    #[must_use]
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_owned();
        }
        match self.kind {
            DatabaseKind::Sqlite => {
                let path = self.database.trim();
                if path.is_empty() {
                    "SQLite".to_owned()
                } else {
                    std::path::Path::new(path)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_owned())
                }
            }
            DatabaseKind::Postgres
            | DatabaseKind::Mysql
            | DatabaseKind::ClickHouse
            | DatabaseKind::SqlServer
            | DatabaseKind::Mongo
            | DatabaseKind::Elasticsearch
            | DatabaseKind::Agent => {
                // 库名可留空（见 `validate`）→ 别拼出「host / 」这种半截名字。
                let database = self.database.trim();
                if database.is_empty() {
                    self.host.trim().to_owned()
                } else {
                    format!("{} / {database}", self.host.trim())
                }
            }
        }
    }

    /// 这条连接用的驱动目录条目（**只有 agent 轨有**，其余引擎返回 `None`）。
    ///
    /// 界面拿它取展示名（`entry.label` 是中文源串，过 `tr`）与缺省端口：
    /// 内核不吐成品文案，所以这里给的是**数据**，不是拼好的字符串。
    #[must_use]
    pub fn driver_entry(&self) -> Option<&'static duck_drivers::DriverEntry> {
        if self.kind != DatabaseKind::Agent {
            return None;
        }
        let key = self.driver.as_deref()?.trim();
        duck_drivers::catalog::entry(key)
    }

    /// 引擎缺省端口。
    ///
    /// 与 [`DatabaseKind::default_port`] 的区别只有一处、但很重要：agent 轨的端口
    /// **随驱动走**（达梦 5236、Oracle 1521、DB2 50000…），枚举答不出来，
    /// 得去驱动目录里查 [`DriverEntry::default_port`](duck_drivers::DriverEntry::default_port)。
    ///
    /// 查不到驱动（档案里没填 / 键写错了）时返回 `None`：界面按「请填端口」处理，
    /// 而不是悄悄回落成 0 或某个别的引擎的端口。
    #[must_use]
    pub fn default_port(&self) -> Option<u16> {
        match self.kind {
            DatabaseKind::Agent => self.driver_entry().and_then(|entry| entry.default_port),
            kind => kind.default_port(),
        }
    }

    /// 该连接的默认查询超时（秒）：`None` / `0` 都算「未设置」（用全局默认）。
    #[must_use]
    pub fn default_timeout(&self) -> Option<u64> {
        self.default_timeout_secs.filter(|secs| *secs > 0)
    }

    /// 凭据是否缺位：网络引擎必须有主机名；SQLite 无要求。
    ///
    /// **库名只有 SQLite 必填**（那里装的是文件路径）。网络引擎留空一律放行——
    /// 口径见 [`DatabaseKind::needs_database`]，各驱动的落点实现见其模块文档。
    /// 文件型档案：库里没有主机端口，`database` 那一格装的是**文件路径**。
    ///
    /// SQLite 是原生的文件引擎；DuckDB 只是**协议上**看起来像网络引擎（工件下载、
    /// 独立进程），连上之后打开的是一个本地文件。判据与界面同源：
    /// `kind == Sqlite` 或驱动键是 `duckdb`。
    #[must_use]
    fn is_file_backed(&self) -> bool {
        self.kind == DatabaseKind::Sqlite
            || self.driver.as_deref() == Some(crate::drivers::duckdb::DRIVER_KEY)
    }

    pub fn validate(&self) -> Result<(), crate::error::DbError> {
        if self.id.trim().is_empty() {
            return Err(crate::error::DbError::Config("id is empty".into()));
        }
        // ⚠️ 这两条都要问**文件型档案**：DuckDB 在档位上属 agent（工件从商店装），
        // 但对端接的是 `{path}` 而不是 host/port —— 拿 `kind` 一刀切会把它的
        // 「文件路径」当「库名」、又逼它填一个根本不存在的主机名。
        let file_backed = self.is_file_backed();
        if self.kind.needs_host() && !file_backed && self.host.trim().is_empty() {
            return Err(crate::error::DbError::Config("host is empty".into()));
        }
        if (self.kind.needs_database() || file_backed) && self.database.trim().is_empty() {
            return Err(crate::error::DbError::Config("file path is empty".into()));
        }
        // 连接串模式必须真的给了串：host / 端口 / 账号在那串里，空串等于没配。
        if self.auth_mode == AuthMode::ConnectionString {
            let raw = self.connection_string.as_deref().unwrap_or("").trim();
            if raw.is_empty() {
                return Err(crate::error::DbError::Config(
                    "connection string is empty".into(),
                ));
            }
        }
        // 私钥两档必须给了钥匙：ssh 没有钥匙又没有口令，只能靠 agent / 默认密钥兜底，
        // 而那两种都不该被这一档「看起来配了、其实空着」的档案悄悄代表。
        if let Some(tunnel) = &self.ssh_tunnel
            && tunnel.auth_method.needs_key()
            && tunnel.key_path.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(crate::error::DbError::Config(
                "ssh key path is empty".into(),
            ));
        }
        if self.kind == DatabaseKind::Agent {
            // 驱动键是 agent 轨的**必需项**：建连时靠它决定跑哪个进程、连哪家的库。
            let key = self.driver.as_deref().unwrap_or("").trim();
            if key.is_empty() {
                return Err(crate::error::DbError::Config(
                    "agent driver key is empty".into(),
                ));
            }
            // 键不在目录里就地拒掉，而不是留到下载时才发现：
            // 到了下载那一步，用户已经等过一次网络往返了。
            if duck_drivers::catalog::entry(key).is_none() {
                return Err(crate::error::DbError::Config(format!(
                    "unknown agent driver key: {key}"
                )));
            }
        }
        Ok(())
    }
}

/// 凭据键名（`dbconn:{id}:password`）。
#[must_use]
pub fn secret_key(id: &str) -> String {
    format!("dbconn:{id}:password")
}

/// 存一条口令（新建或覆盖）。
pub fn store_password(id: &str, password: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&secret_key(id), password)
}

/// 读一条口令；未存过是 `Ok(None)`。
pub fn load_password(id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&secret_key(id))
}

/// 删一条口令（幂等）。
pub fn delete_password(id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&secret_key(id))
}

/// SSH 口令键名（与数据库口令分开存）。
#[must_use]
pub fn ssh_secret_key(id: &str) -> String {
    format!("dbconn:{id}:ssh")
}

/// 存 SSH 口令（新建或覆盖）。
pub fn store_ssh_password(id: &str, password: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&ssh_secret_key(id), password)
}

/// 读 SSH 口令；未存过是 `Ok(None)`。
pub fn load_ssh_password(id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&ssh_secret_key(id))
}

/// 删 SSH 口令（幂等）。
pub fn delete_ssh_password(id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&ssh_secret_key(id))
}

/// SSH 私钥口令键名（加密私钥的 passphrase；与 SSH 登录口令分开存）。
#[must_use]
pub fn ssh_key_secret_key(id: &str) -> String {
    format!("dbconn:{id}:ssh_key")
}

/// 存 SSH 私钥口令（新建或覆盖）。
pub fn store_ssh_key_passphrase(id: &str, passphrase: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&ssh_key_secret_key(id), passphrase)
}

/// 读 SSH 私钥口令；未存过是 `Ok(None)`。
pub fn load_ssh_key_passphrase(id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&ssh_key_secret_key(id))
}

/// 删 SSH 私钥口令（幂等）。
pub fn delete_ssh_key_passphrase(id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&ssh_key_secret_key(id))
}

/// 传输档案的 SSH 私钥口令键名（与档案 SSH 口令同一条纪律：档案是独立凭据主体）。
#[must_use]
pub fn profile_ssh_key_secret_key(profile_id: &str) -> String {
    format!("dbprofile:{profile_id}:ssh_key")
}

/// 存传输档案的 SSH 私钥口令。
pub fn store_profile_ssh_key_passphrase(
    profile_id: &str,
    passphrase: &str,
) -> Result<(), SecretError> {
    duck_platform::secret::set(&profile_ssh_key_secret_key(profile_id), passphrase)
}

/// 读传输档案的 SSH 私钥口令；未存过是 `Ok(None)`。
pub fn load_profile_ssh_key_passphrase(profile_id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&profile_ssh_key_secret_key(profile_id))
}

/// 删传输档案的 SSH 私钥口令（幂等）。
pub fn delete_profile_ssh_key_passphrase(profile_id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&profile_ssh_key_secret_key(profile_id))
}

/// 代理口令键名（与数据库 / SSH 口令都分开存）。
#[must_use]
pub fn proxy_secret_key(id: &str) -> String {
    format!("dbconn:{id}:proxy")
}

/// 存代理口令（新建或覆盖）。
pub fn store_proxy_password(id: &str, password: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&proxy_secret_key(id), password)
}

/// 读代理口令；未存过是 `Ok(None)`。
pub fn load_proxy_password(id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&proxy_secret_key(id))
}

/// 删代理口令（幂等）。
pub fn delete_proxy_password(id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&proxy_secret_key(id))
}

/// 传输档案的 SSH 口令键名。
///
/// 档案是**独立凭据主体**：一份被五条连接引用的档案，口令只该存一份，
/// 而不是在五条连接上各存一遍（那样「轮换代理口令」就是五处手改）。
#[must_use]
pub fn profile_ssh_secret_key(profile_id: &str) -> String {
    format!("dbprofile:{profile_id}:ssh")
}

/// 存传输档案的 SSH 口令。
pub fn store_profile_ssh_password(profile_id: &str, password: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&profile_ssh_secret_key(profile_id), password)
}

/// 读传输档案的 SSH 口令；未存过是 `Ok(None)`。
pub fn load_profile_ssh_password(profile_id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&profile_ssh_secret_key(profile_id))
}

/// 删传输档案的 SSH 口令（幂等）。
pub fn delete_profile_ssh_password(profile_id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&profile_ssh_secret_key(profile_id))
}

/// 传输档案的代理口令键名。
#[must_use]
pub fn profile_proxy_secret_key(profile_id: &str) -> String {
    format!("dbprofile:{profile_id}:proxy")
}

/// 存传输档案的代理口令。
pub fn store_profile_proxy_password(profile_id: &str, password: &str) -> Result<(), SecretError> {
    duck_platform::secret::set(&profile_proxy_secret_key(profile_id), password)
}

/// 读传输档案的代理口令；未存过是 `Ok(None)`。
pub fn load_profile_proxy_password(profile_id: &str) -> Result<Option<String>, SecretError> {
    duck_platform::secret::get(&profile_proxy_secret_key(profile_id))
}

/// 删传输档案的代理口令（幂等）。
pub fn delete_profile_proxy_password(profile_id: &str) -> Result<(), SecretError> {
    duck_platform::secret::delete(&profile_proxy_secret_key(profile_id))
}

/// 连接实际走的传输方式。
///
/// ⚠️ **一次只用一条**：SSH 隧道 / 代理 / SSM 三者各自对内只暴露一个本机
/// `127.0.0.1` 端口，叠加没有确定的语义（真叠加要「让 ssh 子进程先经代理出网」
/// 这类链路改造）。因此这里按固定优先级挑一条，**规则只写在这一处**，
/// 界面上的分区顺序与提示文案都据它渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// 直连（无隧道 / 无代理）。
    Direct,
    /// `ssh -N -L` 本地端口转发。
    Ssh,
    /// SOCKS5 / HTTP CONNECT 代理。
    Proxy,
    /// AWS SSM 端口转发（本机 `aws` CLI）。
    Ssm,
}

impl TransportKind {
    /// 稳定键（i18n / 测试用）。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Ssh => "ssh",
            Self::Proxy => "proxy",
            Self::Ssm => "ssm",
        }
    }
}

impl ConnectionConfig {
    /// 生效的传输方式（优先级 **SSM > SSH > 代理 > 直连**）。
    ///
    /// 「配了但填不全」的档位**不算生效**（例如 SSM 没填实例 id、代理没填主机）——
    /// 否则界面会显示「走 SSM」而实际直连，最难查的那种错。
    #[must_use]
    pub fn transport(&self) -> TransportKind {
        if self
            .ssm
            .as_ref()
            .is_some_and(crate::ssm::SsmConfig::is_usable)
        {
            return TransportKind::Ssm;
        }
        if self
            .ssh_tunnel
            .as_ref()
            .is_some_and(|tunnel| !tunnel.host.trim().is_empty() && !tunnel.user.trim().is_empty())
        {
            return TransportKind::Ssh;
        }
        if self
            .proxy
            .as_ref()
            .is_some_and(|proxy| !proxy.host.trim().is_empty())
        {
            return TransportKind::Proxy;
        }
        TransportKind::Direct
    }

    /// 是否配了「填不全」的传输档位（界面据此提示，而不是静默直连）。
    #[must_use]
    pub fn has_incomplete_transport(&self) -> bool {
        let ssm_partial = self
            .ssm
            .as_ref()
            .is_some_and(|ssm| !crate::ssm::SsmConfig::is_usable(ssm));
        let ssh_partial = self
            .ssh_tunnel
            .as_ref()
            .is_some_and(|tunnel| tunnel.host.trim().is_empty() || tunnel.user.trim().is_empty());
        let proxy_partial = self
            .proxy
            .as_ref()
            .is_some_and(|proxy| proxy.host.trim().is_empty());
        ssm_partial || ssh_partial || proxy_partial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 往返：全字段 + 旧存档缺字段两类。
    #[test]
    fn wire_roundtrip_and_defaults() {
        let cfg = ConnectionConfig {
            id: "conn-1".into(),
            name: "本地库".into(),
            kind: DatabaseKind::Postgres,
            host: "127.0.0.1".into(),
            port: Some(5433),
            user: "duck".into(),
            database: "toolbox".into(),
            tls: TlsMode::Trust,
            auth_mode: AuthMode::ConnectionString,
            connection_string: Some("postgresql://duck@127.0.0.1/toolbox".into()),
            ca_cert_path: Some("/etc/ca.pem".into()),
            client_cert_path: Some("/etc/client.pem".into()),
            client_key_path: Some("/etc/client.key".into()),
            visible_databases: Some(vec!["toolbox".into(), "analytics".into()]),
            env_tag: EnvTag::Prod,
            group: Some("默认".into()),
            ssh_tunnel: Some(SshTunnelConfig {
                host: "bastion.example.com".into(),
                port: Some(2222),
                user: "duck".into(),
                remote_host: "db.internal".into(),
                auth_method: crate::config::SshAuthMethod::KeyAndPassword,
                key_path: Some("~/.ssh/id_ed25519".into()),
            }),
            default_timeout_secs: Some(120),
            read_only: true,
            schema: Some("analytics".into()),
            hooks: crate::hooks::ConnectionHooks {
                hooks: vec![crate::hooks::ConnectionHook {
                    id: "hook-1".into(),
                    phase: crate::hooks::HookPhase::PreConnect,
                    enabled: true,
                    action: crate::hooks::HookAction::Command {
                        line: "echo hi".into(),
                        cwd: None,
                    },
                    failure: crate::hooks::HookFailure::Warn,
                    timeout_secs: None,
                }],
            },
            proxy: Some(crate::proxy::ProxyConfig {
                kind: crate::proxy::ProxyKind::Socks5,
                host: "proxy.example.com".into(),
                port: Some(1080),
                username: "duck".into(),
                // ⚠️ `password` 是 `#[serde(skip)]`：留 `None` 才能让往返相等。
                password: None,
            }),
            ssm: Some(crate::ssm::SsmConfig {
                instance: "i-0123456789abcdef0".into(),
                remote_host: Some("db.internal".into()),
                region: Some("cn-north-1".into()),
                profile: Some("default".into()),
            }),
            tunnel_profile: Some("tunnel-1".into()),
            driver: Some("oracle".into()),
            profile: Some("oracle".into()),
            url_params: Some("auth=noSasl".into()),
            gbase_server: Some("gbase01".into()),
            informix_server: None,
        };
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: ConnectionConfig = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, cfg);

        // 旧存档：只有 id 与 kind，其余全走默认。
        let old: ConnectionConfig =
            serde_json::from_str(r#"{"id":"conn-2","kind":"mysql"}"#).expect("旧存档可读");
        assert_eq!(old.kind, DatabaseKind::Mysql);
        assert_eq!(old.tls, TlsMode::Disable);
        assert_eq!(old.env_tag, EnvTag::None);
        assert_eq!(old.port, None);
        // 新增字段必须对旧存档隐身（缺字段 → None / false，不报错）。
        assert_eq!(old.default_timeout_secs, None);
        assert_eq!(old.default_timeout(), None);
        assert!(!old.read_only, "旧存档默认必须是「可写」，不能凭空变只读");
        assert_eq!(old.schema, None);
        // 传输三档同样对旧存档隐身。
        assert_eq!(old.proxy, None);
        assert_eq!(old.ssm, None);
        assert_eq!(old.tunnel_profile, None);
        assert_eq!(old.transport(), TransportKind::Direct);
        assert!(!old.has_incomplete_transport(), "缺字段不算「填不全」");
        // 新一批字段（认证方式 / 连接串 / TLS 证书 / 库白名单）同样对旧存档隐身，
        // 且缺省必须是「口令认证 / 不过滤」——不能让旧连接悄悄换行为。
        assert_eq!(old.auth_mode, AuthMode::Password);
        assert_eq!(old.connection_string, None);
        assert_eq!(old.ca_cert_path, None);
        assert_eq!(old.client_cert_path, None);
        assert_eq!(old.client_key_path, None);
        assert_eq!(old.visible_databases, None);
        // 牌子与 URL 附参同样隐身：旧档案没有 `profile`，界面靠档位 + 驱动键自己推
        //（推得出来就有、推不出来就按档位显示），绝不能凭空给一个牌子。
        assert_eq!(old.profile, None);
        assert_eq!(old.url_params, None);
        let old_tunnel = old.ssh_tunnel.clone().unwrap_or_default();
        assert_eq!(
            old_tunnel.auth_method,
            crate::config::SshAuthMethod::Password
        );
        assert_eq!(old_tunnel.key_path, None);
        // `0` 与 `None` 同义（都表示「未设置」）。
        assert_eq!(
            ConnectionConfig {
                default_timeout_secs: Some(0),
                ..ConnectionConfig::default()
            }
            .default_timeout(),
            None
        );
    }

    /// 造一个只填了主机的代理档（其余留空）。
    fn proxy_at(host: &str) -> crate::proxy::ProxyConfig {
        crate::proxy::ProxyConfig {
            kind: crate::proxy::ProxyKind::Socks5,
            host: host.into(),
            port: None,
            username: String::new(),
            password: None,
        }
    }

    /// 造一个只填了实例 id 的 SSM 档。
    fn ssm_at(instance: &str) -> crate::ssm::SsmConfig {
        crate::ssm::SsmConfig {
            instance: instance.into(),
            remote_host: None,
            region: None,
            profile: None,
        }
    }

    /// 传输优先级：**SSM > SSH > 代理 > 直连**；「配了但填不全」必须让位而不是静默直连。
    #[test]
    fn transport_precedence_and_incomplete_inputs() {
        let mut cfg = ConnectionConfig {
            kind: DatabaseKind::Postgres,
            host: "db.local".into(),
            ..ConnectionConfig::default()
        };
        assert_eq!(cfg.transport(), TransportKind::Direct);
        assert!(!cfg.has_incomplete_transport());

        cfg.proxy = Some(proxy_at("proxy.local"));
        assert_eq!(cfg.transport(), TransportKind::Proxy);

        cfg.ssh_tunnel = Some(SshTunnelConfig {
            host: "bastion".into(),
            user: "duck".into(),
            ..SshTunnelConfig::default()
        });
        assert_eq!(cfg.transport(), TransportKind::Ssh);
        assert!(!cfg.has_incomplete_transport());

        cfg.ssm = Some(ssm_at("i-0123456789abcdef0"));
        assert_eq!(cfg.transport(), TransportKind::Ssm);

        // 逐个「填不全」：生效档位降级，同时界面能问出「有个档位没填完」。
        cfg.ssm = Some(ssm_at("  "));
        assert_eq!(cfg.transport(), TransportKind::Ssh);
        assert!(cfg.has_incomplete_transport());

        cfg.ssh_tunnel = Some(SshTunnelConfig::default());
        assert_eq!(cfg.transport(), TransportKind::Proxy);
        assert!(cfg.has_incomplete_transport());

        cfg.proxy = Some(proxy_at(" "));
        assert_eq!(cfg.transport(), TransportKind::Direct);
        assert!(cfg.has_incomplete_transport());
    }

    /// 显示名兜底与校验。
    #[test]
    fn display_name_and_validate() {
        let mut cfg = ConnectionConfig {
            id: "conn-3".into(),
            kind: DatabaseKind::Postgres,
            host: "db.local".into(),
            database: "shop".into(),
            ..ConnectionConfig::default()
        };
        assert_eq!(cfg.display_name(), "db.local / shop");
        cfg.validate().expect("网络引擎校验通过");

        cfg.kind = DatabaseKind::Sqlite;
        cfg.host.clear();
        cfg.database = "/tmp/demo.db".into();
        assert_eq!(cfg.display_name(), "demo");
        cfg.validate().expect("SQLite 只需要文件路径");

        cfg.database = String::new();
        assert!(cfg.validate().is_err(), "SQLite 缺文件路径应报错");
    }

    /// 引擎档位的 wire 值与缺省端口。
    ///
    /// 两条口径在这里钉死：缺省端口走**对外数据端口**（ClickHouse 8123 = HTTP 接口
    /// 而非原生 9000；Elasticsearch 9200 = REST 而非节点间 9300）；`es` 是历史短名，
    /// 必须仍能手解析回来，否则旧档案会静默落成 SQLite。
    #[test]
    fn engine_wire_names_and_ports() {
        assert_eq!(DatabaseKind::ALL.len(), 7);
        assert_eq!(
            DatabaseKind::Elasticsearch.wire_name(),
            "elasticsearch",
            "落盘名用全名"
        );
        assert_eq!(
            DatabaseKind::from_wire("elasticsearch"),
            DatabaseKind::Elasticsearch
        );
        assert_eq!(
            DatabaseKind::from_wire("es"),
            DatabaseKind::Elasticsearch,
            "历史短名要能解析回来，不能静默落成 SQLite"
        );
        assert_eq!(DatabaseKind::Elasticsearch.default_port(), Some(9200));
        assert_eq!(DatabaseKind::ClickHouse.default_port(), Some(8123));

        // 每个档位的 wire 名必须能原样解析回来（落盘 → 读回不失真）。
        for kind in DatabaseKind::ALL {
            assert_eq!(
                DatabaseKind::from_wire(kind.wire_name()),
                kind,
                "{} 的 wire 名不能自解析",
                kind.wire_name()
            );
        }
    }

    /// **库名只有 SQLite 必填**：六个网络引擎留空一律放行，而主机仍然必填。
    ///
    /// 这条回归是有来由的：校验层曾经写成「网络引擎库名一律必填」，把 MySQL 这种
    /// 协议上根本不要求库名的引擎也拦在门外（其余工具不填库名照样连）。
    #[test]
    fn database_is_optional_for_every_network_engine() {
        // 口径哨兵：必填库名的**只有** SQLite。将来加了新引擎忘了归类，会在这里被拦下。
        assert_eq!(
            DatabaseKind::ALL
                .iter()
                .filter(|kind| kind.needs_database())
                .count(),
            1,
            "只有 SQLite 需要必填库名（文件路径）"
        );
        for kind in DatabaseKind::ALL {
            let mut cfg = ConnectionConfig {
                id: format!("conn-{}", kind.wire_name()),
                kind,
                host: "db.local".into(),
                user: "duck".into(),
                database: String::new(),
                ..ConnectionConfig::default()
            };
            if kind.needs_database() {
                assert!(
                    cfg.validate().is_err(),
                    "{}: 库名（文件路径）应必填",
                    kind.wire_name()
                );
            } else {
                cfg.validate()
                    .unwrap_or_else(|error| panic!("{} 留空库名应放行：{error}", kind.wire_name()));
                // 留空时显示名不能是「host / 」这种半截。
                assert_eq!(cfg.display_name(), "db.local", "{}", kind.wire_name());
            }
            // SQLite 之外，主机名仍然必填。
            if kind.needs_host() {
                cfg.host = String::new();
                assert!(cfg.validate().is_err(), "{}: 主机应必填", kind.wire_name());
            }
        }
    }

    /// 认证方式的引擎可用性 + 连接串 / 私钥的校验口径。
    #[test]
    fn auth_modes_and_new_validation() {
        // 每档 wire 名自解析。
        for mode in AuthMode::ALL {
            assert_eq!(AuthMode::from_wire(mode.wire_name()), mode);
        }
        // 可用性口径：连接串三家、API Key 只 ES、无认证三家。
        assert!(AuthMode::Password.available_for(DatabaseKind::ClickHouse));
        assert!(!AuthMode::ConnectionString.available_for(DatabaseKind::ClickHouse));
        assert!(AuthMode::ConnectionString.available_for(DatabaseKind::Postgres));
        assert!(AuthMode::ConnectionString.available_for(DatabaseKind::Mongo));
        assert!(!AuthMode::ApiKey.available_for(DatabaseKind::Mysql));
        assert!(AuthMode::ApiKey.available_for(DatabaseKind::Elasticsearch));
        assert!(AuthMode::None_.available_for(DatabaseKind::Postgres));
        assert!(!AuthMode::None_.available_for(DatabaseKind::SqlServer));

        // 连接串模式缺串 → Config 错。
        let mut cfg = ConnectionConfig {
            id: "conn-cs".into(),
            kind: DatabaseKind::Postgres,
            host: "db.local".into(),
            auth_mode: AuthMode::ConnectionString,
            ..ConnectionConfig::default()
        };
        assert!(cfg.validate().is_err(), "空连接串应报错");
        cfg.connection_string = Some("postgresql://u@db.local/db".into());
        cfg.validate().expect("有串就放行");

        // SSH 私钥两档缺钥匙路径 → Config 错；口令档不查钥匙。
        cfg.auth_mode = AuthMode::Password;
        cfg.ssh_tunnel = Some(SshTunnelConfig {
            host: "bastion".into(),
            user: "duck".into(),
            auth_method: SshAuthMethod::Key,
            ..SshTunnelConfig::default()
        });
        assert!(cfg.validate().is_err(), "私钥档缺路径应报错");
        cfg.ssh_tunnel.as_mut().unwrap().key_path = Some("~/.ssh/id_ed25519".into());
        cfg.validate().expect("给了路径就放行");
        cfg.ssh_tunnel.as_mut().unwrap().auth_method = SshAuthMethod::Password;
        cfg.validate().expect("口令档不查钥匙");
    }

    /// 标识符引号：SQL 族分两档（反引号 / 双引号），两条非 SQL 引擎没有这个概念，
    /// agent 档**静态答不出**（要连上问对端，见 `Connection::identifier_quote`）。
    #[test]
    fn identifier_quote_is_per_engine() {
        assert_eq!(DatabaseKind::Mysql.identifier_quote(), Some('`'));
        assert_eq!(DatabaseKind::ClickHouse.identifier_quote(), Some('`'));
        assert_eq!(DatabaseKind::Postgres.identifier_quote(), Some('"'));
        assert_eq!(DatabaseKind::Sqlite.identifier_quote(), Some('"'));
        assert_eq!(DatabaseKind::SqlServer.identifier_quote(), Some('"'));
        // 非 SQL 引擎：界面上「导出 INSERT / 生成 SELECT」这类 SQL 草稿项要对它们禁用。
        assert_eq!(DatabaseKind::Mongo.identifier_quote(), None);
        assert_eq!(DatabaseKind::Elasticsearch.identifier_quote(), None);
        // ⚠️ agent 档也是 `None`，但那**不是**「非 SQL」的意思：它背后几十个引擎的引号
        // 各不相同（`"` / `` ` ``），只有连上问对端才知道。要引号请走
        // `Connection::identifier_quote()`（它对 agent 档会问真值）。
        // 这条断言钉的是「静态那一层不猜」——猜错了不报错、只查错对象。
        assert_eq!(DatabaseKind::Agent.identifier_quote(), None);
        assert!(DatabaseKind::Agent.needs_driver_install());
    }

    /// 分页续拉能力：四条 SQL 引擎 + agent 档；SQL Server（顺序不定）/ MongoDB /
    /// Elasticsearch 一律不支持——界面与 `execute_paged` 都查这一个判据。
    ///
    /// ⚠️ agent 档判「支持」是**乐观**的：这一层只知道「这一轨有真游标这条路」，具不具备
    /// 得连上问对端（[`crate::drivers::Connection::supports_paged_read`]）。乐观是对的——
    /// 保守会让支持游标的驱动白白丢掉「加载更多」；反过来多出一个按钮，点下去是一条
    /// 说清原因的错误。
    #[test]
    fn paged_read_is_only_for_four_sql_engines() {
        let supported: Vec<DatabaseKind> = DatabaseKind::ALL
            .into_iter()
            .filter(|kind| kind.supports_paged_read())
            .collect();
        assert_eq!(
            supported,
            vec![
                DatabaseKind::Sqlite,
                DatabaseKind::Postgres,
                DatabaseKind::Mysql,
                DatabaseKind::ClickHouse,
            ]
        );
        assert!(!DatabaseKind::SqlServer.supports_paged_read());
        assert!(!DatabaseKind::Mongo.supports_paged_read());
        assert!(!DatabaseKind::Elasticsearch.supports_paged_read());
        // agent 不在 `ALL`（那是**内建**七档）里，所以上面那份清单不含它——单独钉一句，
        // 免得将来给人「agent 不支持分页」的错觉。
        assert!(DatabaseKind::Agent.supports_paged_read());
        assert!(!DatabaseKind::ALL.contains(&DatabaseKind::Agent));
    }
}
