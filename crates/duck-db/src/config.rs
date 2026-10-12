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

/// TLS 策略。PostgreSQL 在 M1 尚不支持 TLS（见 `docs/database-design.md` 依赖注记），
/// 选了非 `Disable` 档会在建连时得到 [`crate::error::DbError::Unsupported`]。
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
    /// 该连接的**默认查询超时**（秒）；`None` / `0` = 用引擎无关的全局默认（30 秒）。
    ///
    /// 只作用于「可能很久」的读路径（SQL 执行、表页加载、AI 只读自动执行）；
    /// 单次覆盖由查询选项面板给，见 `docs/database-design.md` §4.4。
    #[serde(default)]
    pub default_timeout_secs: Option<u64>,
    /// 连接级**只读档**（`docs/database-design.md` §4.8）：建连后由服务器自己拒绝写入。
    ///
    /// 三个引擎都落到引擎原生的只读开关上（SQLite `query_only` / PG
    /// `default_transaction_read_only` / MySQL `SESSION TRANSACTION READ ONLY`），
    /// 所以即便界面漏了一道拦截，写语句也会在服务器侧报错。生产库必开。
    #[serde(default)]
    pub read_only: bool,
    /// 上次浏览的 schema / 库（`None` = 引擎的当前命名空间）。
    ///
    /// 只影响界面的默认落点，不改变连接行为。
    #[serde(default)]
    pub schema: Option<String>,
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
}
