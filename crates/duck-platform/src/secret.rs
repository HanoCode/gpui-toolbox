//! 安全凭据存储：把「不该落进 SQLite」的值交给操作系统的钥匙串。
//!
//! 与持久层的分工（**这条分工是硬规则，不是建议**）：
//! - **非敏感配置走 `duck-store` 的 SQLite**（明文，但导出 / 备份 / 迁移都方便）；
//! - **敏感值只走这里**，SQLite 里**只存键名**，绝不存值。
//!
//! 为什么值得单独一层：`duck-store` 的 `kv` 表是明文的，而「数据导出」会把
//! `settings/kv.json` 打进归档——一旦 token 落进 `kv`，它就会跟着用户随手
//! 分享的备份包一起流出去，而且**不可撤回**。把敏感值收口到系统钥匙串，
//! 导出侧只需要在 `manifest` 里写明「包里没有凭据」，这件事就闭死了。
//!
//! # 不写日志
//!
//! 本模块**不产生任何日志行**。运行日志会落盘，而且会被「数据导出」整包打走
//! （`logs/` 是可选项），所以这里经手的值一行都不能进日志。失败原因一律通过
//! [`SecretError`] 交给调用方：调用方可以记录「存储不可用」这个**事实**，
//! 但不能记录值本身。
//!
//! # 不做明文回落
//!
//! 系统后端拿不到时**不降级**成明文或内存存储，而是让 [`available`] 返回
//! `false`、让操作返回 [`SecretError::UnsupportedPlatform`]，由调用方
//! **禁用依赖它的功能**。降级为明文等于把「安全存储」这个前提悄悄抽掉，
//! 而用户以为自己受保护——这比直接报错危险得多。
//!
//! # 预期中的系统授权弹窗（不是 bug）
//!
//! macOS 会给每个钥匙串条目记 ACL。**读第一条不是本进程创建、或创建者签名不同的
//! 条目时，系统会弹一次「… 想要使用您钥匙串中的机密信息」**。所以：
//! - 未签名的 dev 构建与已签名的发布版之间互读会各弹一次，属既定行为；
//! - 本仓**没有 entitlements 文件、也不开沙箱**，因此不需要 `keychain-access-groups`，
//!   打包链路（`sbin/package.sh` / `package.ps1`）**不需要为此改动**。
//!
//! # Linux
//!
//! 本仓不编译 Linux 后端，因此在 Linux 上 [`backend`] 是 [`Backend::None`]。
//! 原因是两条路都不成立：`linux-native` 用的是内核钥匙串，**会话级、重启即丢**
//! （等于换个方式静默丢数据）；`sync-secret-service` 要拖进 `dbus-secret-service`
//! / `zbus` 等四十来个包，且 `libdbus-sys` 依赖系统装 `libdbus-1-dev`——
//! 与本仓「终端用户零系统依赖」的既有取向（rusqlite `bundled`、rustls 替 OpenSSL）冲突。
//! 将来若真要支持，开 `sync-secret-service` + `crypto-rust`，并给 [`Backend`] 加分支。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

/// 写进系统钥匙串的服务名。用户在「钥匙串访问」里按这个名字找到本应用的全部条目。
pub const SERVICE: &str = "com.duck.toolbox";

/// 键名长度上限（字节）。
///
/// 128 是给「`<插件id>.<工具名>`」这类复合键留的余量，同时也远低于各平台
/// 自己的属性长度限制——把上限收在我们的这一侧，是为了让错误在**写之前**出现，
/// 而不是写到一半被平台拒绝（macOS 的 `TooLong` 粒度是按属性的，报错信息很难懂）。
const MAX_KEY_BYTES: usize = 128;

/// 探测用的键名。刻意**永不写入**，所以它不会在用户的钥匙串里留下任何痕迹。
const PROBE_KEY: &str = "probe";

/// 凭据存储操作失败的原因。
///
/// `Display` 是英文（本仓日志与错误约定）；界面文案由调用方用 `tr` 组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// 当前平台没有编译进系统后端（见模块文档「Linux」一节）。
    UnsupportedPlatform,
    /// 后端存在但此刻够不着：钥匙串被锁、Secret Service 没起来、被策略拒绝。
    Unavailable(String),
    /// 后端拒绝了这次调用：键名或值不合法、超出平台长度上限。
    Rejected(String),
    /// 其余后端故障（编码不对、同名条目冲突等）。
    Failed(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("secure storage is not available on this platform")
            }
            Self::Unavailable(detail) => write!(f, "secure storage is unreachable: {detail}"),
            Self::Rejected(detail) => write!(f, "the request was rejected: {detail}"),
            Self::Failed(detail) => write!(f, "the secure storage call failed: {detail}"),
        }
    }
}

impl std::error::Error for SecretError {}

/// 本平台编译进来的系统后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// macOS 钥匙串（Keychain）。
    Keychain,
    /// Windows 凭据管理器（Credential Manager）。
    CredentialManager,
    /// 本平台没有编译进后端。
    None,
}

impl Backend {
    /// 给日志字段用的稳定短名（`tracing` 的 `backend = ...`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keychain => "keychain",
            Self::CredentialManager => "credential-manager",
            Self::None => "none",
        }
    }
}

/// 后端抽象。存在的理由有两个：让上层逻辑能对着内存实现测，以及让「注入一个假后端」
/// 在未来成为可能（例如把凭据托管到企业的密钥服务）。
///
/// 契约：**「键不存在」是 `Ok(None)`，不是错误**。各后端自己负责把「缺失」这个
/// 平台特异的错误码归一掉，调用方不需要知道 `errSecItemNotFound` 存在。
pub trait SecretStore: Send + Sync {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    fn delete(&self, key: &str) -> Result<(), SecretError>;

    /// 默认走 [`SecretStore::get`]。后端若有更便宜的「存在性」查询可自行覆盖。
    fn exists(&self, key: &str) -> Result<bool, SecretError> {
        Ok(self.get(key)?.is_some())
    }
}

/// 内存实现。
///
/// **它不是回落方案**：进程退出即丢，所以没有系统后端的平台上 [`available`] 返回
/// `false`，调用方应当禁用相关功能，而不是改用这里。它的用途只有一个——
/// 让凭据相关的上层逻辑能在没有钥匙串的环境里被测。
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 中毒的锁说明有别的线程 panic 了。这里**恢复数据继续用**，而不是跟着 panic：
    /// 一个存储层的锁不该把一次 panic 传染成第二次。
    fn lock(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretStore for MemoryStore {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.lock().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        Ok(self.lock().get(key).cloned())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.lock().remove(key);
        Ok(())
    }
}

/// 本平台编译进来的系统后端实例；没有则 `None`。
///
/// 这里的 `#[cfg]` 必须与 `Cargo.toml` 里开 `apple-native` / `windows-native`
/// 的 target 谓词**逐字一致**——两个文件、同一个谓词，改一个就得改另一个。
///
/// 漏开后端特性的后果值得写在这：`keyring` 那时**不报错**，而是把默认后端
/// 静默换成进程内的内存 mock（见其 `src/lib.rs` 的 `pub use mock as default;` 分支），
/// 表现是「写入成功、重启全丢」。所以本文件末尾放了两条**编译期**断言守着它：
/// `keyring::macos` / `keyring::windows` 这两个模块本身被同一个特性门控，
/// 漏开特性时这里**编译不过**，而不是跑起来悄悄丢数据。
fn system_store() -> Option<&'static dyn SecretStore> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        Some(&native::SystemStore)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// 本平台编译进来的系统后端。
#[must_use]
pub fn backend() -> Backend {
    #[cfg(target_os = "macos")]
    {
        Backend::Keychain
    }
    #[cfg(target_os = "windows")]
    {
        Backend::CredentialManager
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Backend::None
    }
}

/// 系统安全存储**此刻**是否可用。
///
/// 与 [`backend`] 的分工：那里回答「有没有编译进来」（编译期事实），
/// 这里回答「现在能不能用」（运行期事实——钥匙串锁了、服务没起来都会让它变成 `false`）。
///
/// 实现上会真的读一次存储（一个**从不写入**的探针键，只读，不会留痕）。
/// 调用方若要在渲染路径上反复问，**自己缓存结果**。
#[must_use]
pub fn available() -> bool {
    probe().is_ok()
}

/// 不可用时的具体原因；可用时返回 `Ok(())`。
///
/// 给调用方用来区分「本平台没后端」和「有后端但此刻够不着」——两者的界面措辞
/// 不该一样（前者是永久事实，后者可能锁一下钥匙串就好了）。
pub fn probe() -> Result<(), SecretError> {
    let store = system_store().ok_or(SecretError::UnsupportedPlatform)?;
    // `Ok(None)`（没有这条记录）同样是「存储通」的证据，所以只要不是 `Err` 就算通过。
    store.get(PROBE_KEY).map(|_| ())
}

/// 写入一条凭据（新建或覆盖）。
pub fn set(key: &str, value: &str) -> Result<(), SecretError> {
    set_with(store()?, key, value)
}

/// 读取一条凭据；键不存在时是 `Ok(None)`。
pub fn get(key: &str) -> Result<Option<String>, SecretError> {
    get_with(store()?, key)
}

/// 删除一条凭据。**幂等**：删一个不存在的键算成功。
pub fn delete(key: &str) -> Result<(), SecretError> {
    delete_with(store()?, key)
}

/// 键是否存在。
pub fn exists(key: &str) -> Result<bool, SecretError> {
    exists_with(store()?, key)
}

fn store() -> Result<&'static dyn SecretStore, SecretError> {
    system_store().ok_or(SecretError::UnsupportedPlatform)
}

fn set_with(store: &dyn SecretStore, key: &str, value: &str) -> Result<(), SecretError> {
    validate_key(key)?;
    store.set(key, value)
}

fn get_with(store: &dyn SecretStore, key: &str) -> Result<Option<String>, SecretError> {
    validate_key(key)?;
    store.get(key)
}

fn delete_with(store: &dyn SecretStore, key: &str) -> Result<(), SecretError> {
    validate_key(key)?;
    store.delete(key)
}

fn exists_with(store: &dyn SecretStore, key: &str) -> Result<bool, SecretError> {
    validate_key(key)?;
    store.exists(key)
}

/// 键名规则：非空、不超过 [`MAX_KEY_BYTES`] 字节、且只含 [`is_key_char`] 允许的字符。
///
/// 为什么要在**写之前**自己校验：钥匙串里的条目用户是删不干净的（macOS 的
/// 「钥匙串访问」里一堆条目根本不知道谁写的），键名一旦写歪就是永久垃圾；
/// 而且把「空格开头」「带换行」这类键名拦在门外，也顺手挡掉了「拼错键名导致
/// 读不到自己刚写的值」这个最常见的自伤。
fn validate_key(key: &str) -> Result<(), SecretError> {
    if key.is_empty() {
        return Err(SecretError::Rejected("the key is empty".to_owned()));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(SecretError::Rejected(format!(
            "the key is longer than {MAX_KEY_BYTES} bytes"
        )));
    }
    if let Some(bad) = key.chars().find(|c| !is_key_char(*c)) {
        return Err(SecretError::Rejected(format!(
            "the key contains {bad:?}, which is outside the allowed set"
        )));
    }
    Ok(())
}

/// 键名允许的字符：ASCII 字母数字加 `.` `-` `_` `:`。
///
/// 刻意收得很紧。这套字符集足够表达 `<插件id>.<工具名>` 与 `provider.<名字>.token`
/// 这类分层键名，同时天然排除了控制字符、空白与各类容易看混的同形字符。
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')
}

// ── 平台后端 ───────────────────────────────────────────────────────────
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native {
    use super::{SERVICE, SecretError, SecretStore};

    /// 系统钥匙串 / 凭据管理器。无状态——每次调用现开一个 `Entry`，
    /// 因为平台后端自己会在进程内做缓存与序列化。
    pub(super) struct SystemStore;

    impl SecretStore for SystemStore {
        fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
            entry(key)?.set_password(value).map_err(classify)
        }

        fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
            match entry(key)?.get_password() {
                Ok(value) => Ok(Some(value)),
                // 「没有这一条」不是错误：读一个还没设过的键是完全正常的路径
                // （首启时所有凭据都还没配）。归一成 `None`，不向上冒错。
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(other) => Err(classify(other)),
            }
        }

        fn delete(&self, key: &str) -> Result<(), SecretError> {
            match entry(key)?.delete_credential() {
                Ok(()) => Ok(()),
                // 删一个不存在的条目按「已经没了」处理，让 `delete` 幂等——
                // 否则「重装后清理残留」这种调用会莫名其妙地失败。
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(other) => Err(classify(other)),
            }
        }
    }

    fn entry(key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, key).map_err(classify)
    }

    /// 把 `keyring` 的错误归一成三档。
    ///
    /// 最要紧的一条：`NoStorageAccess` / `PlatformFailure` **绝不能**被折叠成
    /// 「当作没有值」。那会让调用方以为「用户还没配」，而真相是「这台机器上读不出来」
    /// ——一个环境故障被伪装成空配置，是这类代码最难查的一类 bug。
    pub(super) fn classify(error: keyring::Error) -> SecretError {
        match error {
            keyring::Error::NoStorageAccess(detail) | keyring::Error::PlatformFailure(detail) => {
                SecretError::Unavailable(detail.to_string())
            }
            keyring::Error::TooLong(field, limit) => SecretError::Rejected(format!(
                "the `{field}` attribute exceeds the platform limit of {limit} characters"
            )),
            keyring::Error::Invalid(field, reason) => {
                SecretError::Rejected(format!("the `{field}` attribute is invalid: {reason}"))
            }
            keyring::Error::BadEncoding(_) => {
                SecretError::Failed("the stored value is not valid UTF-8".to_owned())
            }
            keyring::Error::Ambiguous(items) => SecretError::Failed(format!(
                "{} credentials in the store match this key",
                items.len()
            )),
            // `keyring::Error` 是 `#[non_exhaustive]`，上游随时可能加变体。
            // 落到这里说明上面漏了分支（`NoEntry` 在上游已被各调用点先行拦掉），
            // 记成失败而不是悄悄吞掉。
            other => SecretError::Failed(other.to_string()),
        }
    }
}

// ── 编译期守卫：后端特性真的开了 ────────────────────────────────────────
//
// `keyring::macos` / `keyring::windows` 这两个模块被
// `#[cfg(all(target_os = …, feature = "…-native"))]` 门控，所以引用它们就等于
// 断言「特性开着」。一旦 `Cargo.toml` 里的 target 依赖表漏了 `features = [...]`，
// **这里直接编译失败**——而不是编译通过、跑起来用内存 mock，
// 让用户的数据在每次重启后静静消失。
#[cfg(target_os = "macos")]
const _: fn() -> Box<keyring::CredentialBuilder> = keyring::macos::default_credential_builder;

#[cfg(target_os = "windows")]
const _: fn() -> Box<keyring::CredentialBuilder> = keyring::windows::default_credential_builder;

#[cfg(test)]
mod tests {
    use super::{
        Backend, MAX_KEY_BYTES, MemoryStore, SecretError, backend, delete_with, exists_with,
        get_with, set_with, validate_key,
    };

    /// 一条合法的键名，供各用例复用。
    const KEY: &str = "provider.example.token";

    // ── 内存实现的四态 ────────────────────────────────────────────────

    #[test]
    fn a_missing_key_reads_as_none_rather_than_an_error() {
        let store = MemoryStore::new();
        assert_eq!(get_with(&store, KEY).expect("读不存在的键"), None);
        assert!(!exists_with(&store, KEY).expect("探不存在的键"));
    }

    #[test]
    fn a_written_key_round_trips() {
        let store = MemoryStore::new();
        set_with(&store, KEY, "s3cr3t").expect("写入");
        assert_eq!(
            get_with(&store, KEY).expect("读回").as_deref(),
            Some("s3cr3t")
        );
        assert!(exists_with(&store, KEY).expect("存在性"));
    }

    #[test]
    fn writing_the_same_key_twice_overwrites_rather_than_duplicates() {
        let store = MemoryStore::new();
        set_with(&store, KEY, "old").expect("首次写入");
        set_with(&store, KEY, "new").expect("覆盖写入");
        assert_eq!(get_with(&store, KEY).expect("读回").as_deref(), Some("new"));
    }

    #[test]
    fn a_deleted_key_is_gone() {
        let store = MemoryStore::new();
        set_with(&store, KEY, "s3cr3t").expect("写入");
        delete_with(&store, KEY).expect("删除");
        assert_eq!(get_with(&store, KEY).expect("读回"), None);
        assert!(!exists_with(&store, KEY).expect("存在性"));
    }

    /// `delete` 的契约是幂等：删一个不存在的键算成功，不该报错。
    #[test]
    fn deleting_a_missing_key_is_still_a_success() {
        let store = MemoryStore::new();
        delete_with(&store, KEY).expect("删不存在的键应当成功");
        delete_with(&store, KEY).expect("再删一次也应当成功");
    }

    /// 键名互不串门——这条挡的是「哈希算错」这类会同时污染多条凭据的 bug。
    #[test]
    fn different_keys_hold_different_values() {
        let store = MemoryStore::new();
        set_with(&store, "a", "1").expect("写 a");
        set_with(&store, "b", "2").expect("写 b");
        assert_eq!(get_with(&store, "a").expect("读 a").as_deref(), Some("1"));
        assert_eq!(get_with(&store, "b").expect("读 b").as_deref(), Some("2"));
    }

    /// 值是空串与含非 ASCII 的字符串时同样要原样往返（真实钥匙串存的是 UTF-8 字节）。
    #[test]
    fn odd_but_legal_values_survive_the_round_trip() {
        let store = MemoryStore::new();
        for value in ["", "  ", "换行\n制表\t", "emoji 🔐 与空格"] {
            set_with(&store, KEY, value).expect("写入");
            assert_eq!(get_with(&store, KEY).expect("读回").as_deref(), Some(value));
        }
    }

    // ── 键名校验 ──────────────────────────────────────────────────────

    #[test]
    fn an_empty_key_is_rejected_before_reaching_the_store() {
        assert!(matches!(validate_key(""), Err(SecretError::Rejected(_))));
    }

    #[test]
    fn keys_with_whitespace_or_control_characters_are_rejected() {
        for bad in [" leading", "trailing ", "two words", "new\nline", "tab\t"] {
            assert!(
                matches!(validate_key(bad), Err(SecretError::Rejected(_))),
                "{bad:?} 不该被接受"
            );
        }
    }

    #[test]
    fn non_ascii_keys_are_rejected() {
        assert!(matches!(
            validate_key("凭据"),
            Err(SecretError::Rejected(_))
        ));
    }

    #[test]
    fn the_allowed_separators_are_accepted() {
        for good in ["a.b", "a-b", "a_b", "a:b", "Plugin-1.tool_2:slot"] {
            assert!(validate_key(good).is_ok(), "{good:?} 应当被接受");
        }
    }

    /// 长度上限按**字节**算：刚好压线通过，多一个字节就被拒。
    #[test]
    fn the_length_limit_is_measured_in_bytes() {
        let exact = "a".repeat(MAX_KEY_BYTES);
        assert!(validate_key(&exact).is_ok(), "压线的键应当通过");
        let over = "a".repeat(MAX_KEY_BYTES + 1);
        assert!(
            matches!(validate_key(&over), Err(SecretError::Rejected(_))),
            "超一个字节就该被拒"
        );
    }

    /// 校验必须发生在**动存储之前**：非法键名不该在钥匙串里留下任何条目。
    #[test]
    fn a_rejected_key_never_touches_the_store() {
        let store = MemoryStore::new();
        assert!(set_with(&store, "bad key", "value").is_err());
        assert_eq!(get_with(&store, "bad").expect("读"), None);
        assert_eq!(store.lock().len(), 0, "不该留下任何条目");
    }

    // ── 后端判定 ──────────────────────────────────────────────────────

    /// `backend()` 与 `system_store()` 用的是同一个 `#[cfg]` 谓词，
    /// 这条断言它们不会各自漂移（例如只改了其中一个）。
    #[test]
    fn the_reported_backend_agrees_with_whether_a_store_exists() {
        let has_store = super::system_store().is_some();
        assert_eq!(
            backend() == Backend::None,
            !has_store,
            "后端判定与 store 的存在性不一致"
        );
    }

    /// 本仓只在这两个平台开后端特性（见 `Cargo.toml` 的 target 依赖表）。
    #[test]
    fn the_backend_matches_the_platform_we_compile_for() {
        let expected = if cfg!(target_os = "macos") {
            Backend::Keychain
        } else if cfg!(target_os = "windows") {
            Backend::CredentialManager
        } else {
            Backend::None
        };
        assert_eq!(backend(), expected);
        assert_eq!(backend().as_str() == "none", backend() == Backend::None);
    }

    /// 真往系统钥匙串里写一条再读回来。
    ///
    /// **默认 `#[ignore]`**：它会短暂写进用户的真实钥匙串（跑完即删），
    /// 不适合跟着 `cargo test` 一起跑。但它是「后端不是内存 mock」的唯一硬证据，
    /// 所以留着，按需手动跑：
    ///
    /// ```text
    /// cargo test -p duck-platform -- --ignored --nocapture
    /// security find-generic-password -s com.duck.toolbox   # 应能看到条目
    /// ```
    #[test]
    #[ignore = "会写入真实的系统钥匙串，按需手动跑"]
    fn a_real_round_trip_lands_in_the_system_keychain() {
        let key = "round-trip-probe";
        assert!(super::available(), "系统安全存储不可用，先解决环境再谈往返");

        super::set(key, "round-trip-value").expect("写入系统钥匙串");
        assert_eq!(
            super::get(key).expect("读回").as_deref(),
            Some("round-trip-value")
        );
        assert!(super::exists(key).expect("存在性"));

        super::delete(key).expect("删除");
        assert_eq!(super::get(key).expect("再读应为空"), None);
        // 幂等：删完再删一次也应当是成功
        super::delete(key).expect("重复删除");
    }
}
