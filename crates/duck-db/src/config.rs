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
}

impl DatabaseKind {
    /// 全部档位。
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
            _ => Self::Sqlite,
        }
    }

    /// 引擎的 TCP 缺省端口；SQLite 无网络端口，返回 `None`。
    ///
    /// ClickHouse 取 **HTTP 接口**的 8123（不是原生协议的 9000）——本条驱动走 HTTP。
    /// Elasticsearch 取 REST 的 9200（不是节点间通信的 9300）。
    #[must_use]
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Self::Sqlite => None,
            Self::Postgres => Some(5432),
            Self::Mysql => Some(3306),
            Self::ClickHouse => Some(8123),
            Self::SqlServer => Some(1433),
            Self::Mongo => Some(27017),
            Self::Elasticsearch => Some(9200),
        }
    }

    /// 是否需要 TCP 主机名（SQLite 走本地文件，不填）。
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
    #[must_use]
    pub const fn identifier_quote(self) -> Option<char> {
        match self {
            Self::Sqlite | Self::Postgres | Self::SqlServer => Some('"'),
            Self::Mysql | Self::ClickHouse => Some('`'),
            Self::Mongo | Self::Elasticsearch => None,
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
    #[must_use]
    pub const fn supports_paged_read(self) -> bool {
        matches!(
            self,
            Self::Sqlite | Self::Postgres | Self::Mysql | Self::ClickHouse
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
            | DatabaseKind::Elasticsearch => {
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

    /// 该连接的默认查询超时（秒）：`None` / `0` 都算「未设置」（用全局默认）。
    #[must_use]
    pub fn default_timeout(&self) -> Option<u64> {
        self.default_timeout_secs.filter(|secs| *secs > 0)
    }

    /// 凭据是否缺位：网络引擎必须有主机名；SQLite 无要求。
    ///
    /// **库名只有 SQLite 必填**（那里装的是文件路径）。网络引擎留空一律放行——
    /// 口径见 [`DatabaseKind::needs_database`]，各驱动的落点实现见其模块文档。
    pub fn validate(&self) -> Result<(), crate::error::DbError> {
        if self.id.trim().is_empty() {
            return Err(crate::error::DbError::Config("id is empty".into()));
        }
        if self.kind.needs_host() && self.host.trim().is_empty() {
            return Err(crate::error::DbError::Config("host is empty".into()));
        }
        if self.kind.needs_database() && self.database.trim().is_empty() {
            return Err(crate::error::DbError::Config("file path is empty".into()));
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
            env_tag: EnvTag::Prod,
            group: Some("默认".into()),
            ssh_tunnel: Some(SshTunnelConfig {
                host: "bastion.example.com".into(),
                port: Some(2222),
                user: "duck".into(),
                remote_host: "db.internal".into(),
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

    /// 标识符引号：SQL 族分两档（反引号 / 双引号），两条非 SQL 引擎没有这个概念。
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
    }

    /// 分页续拉能力：只有四条 SQL 引擎支持；SQL Server（顺序不定）/ MongoDB /
    /// Elasticsearch 一律不支持——界面与 `execute_paged` 都查这一个判据。
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
    }
}
