//! 连接档案：环境标签、连接配置与凭据薄封装。
//!
//! 结构体本身即 wire（每字段 `#[serde(default)]`，旧存档缺字段可补默认值），
//! 由调用方序列化成 JSON 存进 `duck-store` 的 kv 单键（`redis_connections`）。
//! **口令不进 kv**：只经 [`store_password`] / [`load_password`] / [`delete_password`]
//! 走 `duck-platform::secret`（应用自有凭据文件 `credentials.json`，值为 vault 密文）。

use serde::{Deserialize, Serialize};

use duck_platform::secret::SecretError;

use crate::error::RedisError;

/// Redis 缺省端口。
pub const DEFAULT_PORT: u16 = 6379;

/// Redis 逻辑库数量上限（`SELECT` 的合法范围是 0..=15）。
pub const MAX_DB: u32 = 15;

/// 环境标签：给连接列表分组着色用，防「把生产库当测试库」。
///
/// 与 `duck-db::config::EnvTag` 形状相同但**各自独立**——本 crate 不依赖 `duck-db`
/// （见 `docs/redis-design.md` §1.1 的取舍说明）。
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

    /// 是否为生产档（分级锁写的输入之一）。
    #[must_use]
    pub const fn is_production(self) -> bool {
        matches!(self, Self::Prod)
    }
}

/// 写入策略：安全 tab 的**单一单选轴**（对齐 zedis 的 writes 单选，ADR 口径：
/// 「跟随标签 / 允许 / 锁定 / 只读」一轴取代「只读开关 + 锁写开关」两个可能同时
/// 打开的布尔——后者无法表达「默认值跟随标签」这件事）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WritePolicy {
    /// 跟随环境标签：生产档默认锁写，其余档位可写。
    #[default]
    #[serde(rename = "follow-tag")]
    FollowTag,
    /// 显式允许写（压过生产档的默认锁写——用户明确知道自己在做什么）。
    #[serde(rename = "allow")]
    Allow,
    /// 显式锁写（无论环境标签；与生产锁一样是**限时可解锁**的）。
    #[serde(rename = "lock")]
    Lock,
    /// 只读：连接本身不执行任何写命令。
    #[serde(rename = "read-only")]
    ReadOnly,
}

impl WritePolicy {
    /// 全部档位（顺序即表单顺序）。
    pub const ALL: [Self; 4] = [Self::FollowTag, Self::Allow, Self::Lock, Self::ReadOnly];

    /// wire 值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::FollowTag => "follow-tag",
            Self::Allow => "allow",
            Self::Lock => "lock",
            Self::ReadOnly => "read-only",
        }
    }

    /// 解析 wire 值；未知值回落跟随标签。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "allow" => Self::Allow,
            "lock" => Self::Lock,
            "read-only" => Self::ReadOnly,
            _ => Self::FollowTag,
        }
    }

    /// 该策略下「连接本身只读」。
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    /// 该策略下「强制锁写」（限时可解锁的那一档）。
    #[must_use]
    pub const fn forces_lock(self) -> bool {
        matches!(self, Self::Lock)
    }

    /// 该策略下「生产档默认锁写是否生效」（显式允许 / 只读档都不需要它）。
    #[must_use]
    pub const fn follows_tag(self) -> bool {
        matches!(self, Self::FollowTag)
    }
}

/// TLS 策略。
///
/// M1 的 `redis` 依赖**未开任何 TLS 特性**（`docs/redis-design.md` §4.1），
/// 所以选了 [`TlsMode::Trust`] 或 [`TlsMode::DangerAcceptInvalid`] 时，
/// 建连会返回 [`RedisError::Unsupported`]——不会静默降级成明文。
/// 远端加密链路在 M1.5 之前由 SSH 隧道覆盖。
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

    /// 是否要求 TLS。
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Disable)
    }
}

/// SSH 隧道配置（跳板机信息；SSH 口令走凭据文件 `redisconn:{id}:ssh`，不入 kv）。
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

/// 一条 Redis 连接档案。
///
/// 即 serde wire 结构：新增字段一律 `Option` + `#[serde(default)]`，
/// 保证旧存档逐字段可读（与 `AppearanceWire`、`db_connections` 同一条纪律）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisConfig {
    /// 稳定 id（`redis-` + 随机后缀），凭据键名与档案关联都用它。
    #[serde(default)]
    pub id: String,
    /// 显示名（用户可改）。
    #[serde(default)]
    pub name: String,
    /// TCP 主机名。
    #[serde(default)]
    pub host: String,
    /// TCP 端口；`None` 用 [`DEFAULT_PORT`]。
    #[serde(default)]
    pub port: Option<u16>,
    /// 逻辑库序号（0..=15，建连时 `SELECT`）。
    #[serde(default)]
    pub db: u32,
    /// ACL 用户名；空 = 只用口令（Redis 6 之前的老式 AUTH）。
    #[serde(default)]
    pub acl_user: String,
    /// 描述（连接列表 / 编辑弹窗展示用；不影响连接）。
    #[serde(default)]
    pub description: Option<String>,
    /// TLS 策略（M1 仅 `Disable` 可用）。
    #[serde(default)]
    pub tls: TlsMode,
    /// 环境标签（分级锁写的输入之一）。
    #[serde(default)]
    pub env_tag: EnvTag,
    /// 写入策略（安全 tab 的单选轴；取代旧版的 `read_only` 布尔）。
    #[serde(default)]
    pub write_policy: WritePolicy,
    /// 每次 `SCAN` 的 `COUNT` 覆盖（`None` = 用界面默认值；上限由 `clamp_count` 收口）。
    #[serde(default)]
    pub scan_count: Option<u32>,
    /// 写命令是否逐条二次确认（破坏性确认在 M1.3 收尾接线，字段先持久化）。
    #[serde(default)]
    pub require_confirm_writes: bool,
    /// 命名空间分隔符（key 树分组用）。空串在 [`Self::validate`] 里被拒。
    #[serde(default = "default_separator")]
    pub key_separator: String,
    /// 键树最大层数；`None` = 内核缺省 [`crate::scan::MAX_TREE_DEPTH`]。
    /// 0 会被内核收敛为 1（至少切一层）。
    #[serde(default)]
    pub max_key_tree_depth: Option<u32>,
    /// 键树自动展开阈值：节点的子树键数不超过它时默认展开。
    /// `None` / 0 = 一律不自动展开（只铺顶层，用户自己点开）。
    #[serde(default)]
    pub auto_expand_threshold: Option<u32>,
    /// 自定义分组名（连接列表按它分组）。
    #[serde(default)]
    pub group: Option<String>,
    /// SSH 隧道（`None` = 直连）。
    #[serde(default)]
    pub ssh_tunnel: Option<SshTunnelConfig>,
}

/// `key_separator` 的 serde 缺省（`:`）。
fn default_separator() -> String {
    ":".to_owned()
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            host: String::new(),
            port: None,
            db: 0,
            acl_user: String::new(),
            description: None,
            tls: TlsMode::Disable,
            env_tag: EnvTag::None,
            write_policy: WritePolicy::default(),
            scan_count: None,
            require_confirm_writes: false,
            key_separator: default_separator(),
            max_key_tree_depth: None,
            auto_expand_threshold: None,
            group: None,
            ssh_tunnel: None,
        }
    }
}

impl RedisConfig {
    /// 生效端口（未填用缺省）。
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    /// 生效命名空间分隔符（为空时回落 `:`，供渲染路径安全调用）。
    #[must_use]
    pub fn effective_separator(&self) -> &str {
        if self.key_separator.is_empty() {
            ":"
        } else {
            &self.key_separator
        }
    }

    /// 生效键树最大层数（未填用内核缺省）。
    #[must_use]
    pub fn effective_tree_depth(&self) -> usize {
        self.max_key_tree_depth
            .and_then(|depth| usize::try_from(depth).ok())
            .unwrap_or(crate::scan::MAX_TREE_DEPTH)
    }

    /// 生效自动展开阈值；未填 / 0 一律返回 0（不自动展开）。
    #[must_use]
    pub fn effective_auto_expand(&self) -> usize {
        self.auto_expand_threshold
            .and_then(|threshold| usize::try_from(threshold).ok())
            .unwrap_or(0)
    }

    /// 连接显示名兜底：未命名时用「主机:端口/N」拼一个。
    #[must_use]
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_owned();
        }
        format!("{}:{}/{}", self.host.trim(), self.effective_port(), self.db)
    }

    /// 分组名兜底（未分组时返回 `None`，由 UI 决定「未分组」的呈现）。
    #[must_use]
    pub fn group_name(&self) -> Option<&str> {
        self.group
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
    }

    /// 档案校验：id / 主机非空、端口有效、db 在范围内、分隔符非空。
    pub fn validate(&self) -> Result<(), RedisError> {
        if self.id.trim().is_empty() {
            return Err(RedisError::Config("id is empty".into()));
        }
        if self.host.trim().is_empty() {
            return Err(RedisError::Config("host is empty".into()));
        }
        if self.port == Some(0) {
            return Err(RedisError::Config("port must not be 0".into()));
        }
        if self.db > MAX_DB {
            return Err(RedisError::Config(format!(
                "db must be within 0..={MAX_DB}, got {}",
                self.db
            )));
        }
        if self.key_separator.is_empty() {
            return Err(RedisError::Config("key separator is empty".into()));
        }
        Ok(())
    }

    /// 把用户名 / 口令 / 库序号拼成 redis-rs 能解析的连接 URL。
    ///
    /// 走 URL 而不是直接构造 `ConnectionInfo`：后者的字段是 `pub(crate)`，
    /// 外部无法逐字段装配（redis-rs 1.2.2 `connection.rs:244`）。
    ///
    /// 用户名与口令按 RFC 3986 的 userinfo 规则百分号编码——口令里的 `@` / `:` / `/`
    /// 若不编码会把 URL 拆坏，表现为「认证失败」这种极难排查的症状。
    #[must_use]
    pub fn connection_url(&self, password: Option<&str>) -> String {
        let mut url = String::from("redis://");
        let user = self.acl_user.trim();
        match (user.is_empty(), password) {
            // 只有口令（Redis 6 之前的老式 AUTH）。
            (true, Some(pw)) => {
                url.push(':');
                url.push_str(&percent_encode(pw));
                url.push('@');
            }
            // 用户名 + 口令。
            (false, Some(pw)) => {
                url.push_str(&percent_encode(user));
                url.push(':');
                url.push_str(&percent_encode(pw));
                url.push('@');
            }
            // 无口令：用户名单独出现没有意义，直接匿名连接。
            (_, None) => {}
        }
        url.push_str(self.host.trim());
        url.push(':');
        url.push_str(&self.effective_port().to_string());
        url.push('/');
        url.push_str(&self.db.to_string());
        url
    }
}

/// RFC 3986 userinfo 的百分号编码：只保留 `unreserved`（ALPHA / DIGIT / `-._~`），
/// 其余一律编码成 `%XX`（大写十六进制）。
///
/// 偏保守但绝对安全——多编码几个字符对服务端无影响，少编码一个就会拆坏 URL。
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// 凭据键名（`redisconn:{id}:password`）。
#[must_use]
pub fn secret_key(id: &str) -> String {
    format!("redisconn:{id}:password")
}

/// SSH 口令键名（与连接口令分开存）。
#[must_use]
pub fn ssh_secret_key(id: &str) -> String {
    format!("redisconn:{id}:ssh")
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
        let cfg = RedisConfig {
            id: "redis-1".into(),
            name: "本地缓存".into(),
            host: "127.0.0.1".into(),
            port: Some(6380),
            db: 3,
            acl_user: "duck".into(),
            description: Some("测试环境的主缓存".into()),
            tls: TlsMode::Trust,
            env_tag: EnvTag::Prod,
            write_policy: WritePolicy::ReadOnly,
            scan_count: Some(2_000),
            require_confirm_writes: true,
            key_separator: ".".into(),
            max_key_tree_depth: Some(4),
            auto_expand_threshold: Some(50),
            group: Some("默认".into()),
            ssh_tunnel: Some(SshTunnelConfig {
                host: "bastion.example.com".into(),
                port: Some(2222),
                user: "duck".into(),
                remote_host: "cache.internal".into(),
            }),
        };
        let json = serde_json::to_string(&cfg).expect("序列化");
        let back: RedisConfig = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, cfg);

        // 旧存档：只有 id 与 host，其余全走默认。
        let old: RedisConfig =
            serde_json::from_str(r#"{"id":"redis-2","host":"localhost"}"#).expect("旧存档可读");
        assert_eq!(old.db, 0);
        assert_eq!(old.port, None);
        assert_eq!(old.tls, TlsMode::Disable);
        assert_eq!(old.env_tag, EnvTag::None);
        assert_eq!(old.write_policy, WritePolicy::FollowTag);
        assert!(!old.write_policy.is_read_only());
        assert_eq!(old.scan_count, None);
        assert!(!old.require_confirm_writes);
        assert_eq!(old.key_separator, ":", "分隔符缺省应为冒号");
        // 键树两档缺省：深度用内核缺省、自动展开关闭（旧存档逐字段可读）。
        assert_eq!(old.max_key_tree_depth, None);
        assert_eq!(old.auto_expand_threshold, None);
        assert_eq!(old.effective_tree_depth(), crate::scan::MAX_TREE_DEPTH);
        assert_eq!(old.effective_auto_expand(), 0);
    }

    /// 键树两档的生效值：填了用填的，0 的语义由内核收口。
    #[test]
    fn tree_settings_effective_values() {
        let cfg = RedisConfig {
            id: "redis-7".into(),
            host: "h".into(),
            max_key_tree_depth: Some(3),
            auto_expand_threshold: Some(20),
            ..RedisConfig::default()
        };
        assert_eq!(cfg.effective_tree_depth(), 3);
        assert_eq!(cfg.effective_auto_expand(), 20);

        // 阈值显式填 0 = 不自动展开（与「未填」同一个可观察结果，但语义明确）。
        let off = RedisConfig {
            auto_expand_threshold: Some(0),
            ..cfg.clone()
        };
        assert_eq!(off.effective_auto_expand(), 0);
    }

    /// 旧档案迁移：2026-10-12 之前的 `read_only` 布尔 → `write_policy` 单选轴。
    /// 迁移本身在 UI 侧做 JSON 预处理（`load_connections`），这里钉住两端的形状。
    #[test]
    fn legacy_read_only_field_is_dropped_but_json_stays_parseable() {
        // 旧 wire：带 read_only 字段。serde 默认忽略未知字段，读到的是 FollowTag
        // ——真正的「true → read-only」换算由 UI 侧迁移完成，内核只保证不炸。
        let legacy: RedisConfig =
            serde_json::from_str(r#"{"id":"redis-9","host":"h","read_only":true}"#)
                .expect("旧字段不影响解析");
        assert_eq!(legacy.write_policy, WritePolicy::FollowTag);
        assert!(legacy.write_policy.follows_tag());
    }

    /// 显示名、端口与分隔符的兜底。
    #[test]
    fn display_name_and_effective_values() {
        let mut cfg = RedisConfig {
            id: "redis-3".into(),
            host: "cache.local".into(),
            ..RedisConfig::default()
        };
        assert_eq!(cfg.effective_port(), DEFAULT_PORT);
        assert_eq!(cfg.display_name(), "cache.local:6379/0");

        cfg.port = Some(6380);
        cfg.db = 5;
        assert_eq!(cfg.display_name(), "cache.local:6380/5");

        cfg.name = "  缓存  ".into();
        assert_eq!(cfg.display_name(), "缓存", "自定义名优先且去空白");

        cfg.key_separator = String::new();
        assert_eq!(cfg.effective_separator(), ":", "空分隔符渲染时回落冒号");
        assert!(cfg.validate().is_err(), "空分隔符应被校验拒绝");
    }

    /// 校验：id / host / 端口 / db 范围 / 分隔符五项。
    #[test]
    fn validate_rejects_bad_values() {
        let good = RedisConfig {
            id: "redis-4".into(),
            host: "127.0.0.1".into(),
            ..RedisConfig::default()
        };
        good.validate().expect("合法档案");

        let no_id = RedisConfig {
            id: "  ".into(),
            ..good.clone()
        };
        assert!(no_id.validate().is_err(), "空 id 应被拒");

        let no_host = RedisConfig {
            host: String::new(),
            ..good.clone()
        };
        assert!(no_host.validate().is_err(), "空主机应被拒");

        let zero_port = RedisConfig {
            port: Some(0),
            ..good.clone()
        };
        assert!(zero_port.validate().is_err(), "端口 0 应被拒");

        let bad_db = RedisConfig {
            db: MAX_DB + 1,
            ..good.clone()
        };
        assert!(bad_db.validate().is_err(), "db 越界应被拒");

        let max_db = RedisConfig { db: MAX_DB, ..good };
        max_db.validate().expect("db = 15 合法");
    }

    /// 连接 URL：匿名 / 只口令 / 用户名+口令三种形态。
    #[test]
    fn connection_url_shapes() {
        let base = RedisConfig {
            id: "redis-5".into(),
            host: "cache.local".into(),
            port: Some(6380),
            db: 2,
            ..RedisConfig::default()
        };

        assert_eq!(
            base.connection_url(None),
            "redis://cache.local:6380/2".to_owned()
        );

        // 只有口令：URL 里出现空用户名（`:pw@`）。
        let with_pw = base.connection_url(Some("secret"));
        assert_eq!(with_pw, "redis://:secret@cache.local:6380/2");

        // 用户名 + 口令。
        let named = RedisConfig {
            acl_user: "duck".into(),
            ..base.clone()
        };
        assert_eq!(
            named.connection_url(Some("secret")),
            "redis://duck:secret@cache.local:6380/2"
        );

        // 有用户名但没口令：不拼用户名字段（单独的用户名无意义）。
        assert_eq!(named.connection_url(None), "redis://cache.local:6380/2");
    }

    /// 口令里的 URL 元字符必须被编码，否则会把 URL 拆坏。
    #[test]
    fn connection_url_encodes_metacharacters() {
        let cfg = RedisConfig {
            id: "redis-6".into(),
            host: "cache.local".into(),
            acl_user: "duck".into(),
            ..RedisConfig::default()
        };
        // `p@ss:word/1?x#y` 里的 @ : / ? # 全部要编码。
        let url = cfg.connection_url(Some("p@ss:word/1?x#y"));
        assert_eq!(
            url,
            "redis://duck:p%40ss%3Aword%2F1%3Fx%23y@cache.local:6379/0"
        );
        // 编码后必须仍能被 redis-rs 解析成同一个连接目标。
        assert!(redis::parse_redis_url(&url).is_some(), "URL 应可解析");
    }

    /// 百分号编码谓词：非 ASCII 与保留字符一律编码，unreserved 原样保留。
    #[test]
    fn percent_encode_rules() {
        assert_eq!(percent_encode("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(percent_encode("中"), "%E4%B8%AD");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode(""), "");
    }
}
