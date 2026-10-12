//! 凭据存储：把「不该跟着备份包流出去」的值收口到程序自己的数据目录。
//!
//! 与持久层的分工（**这条分工是硬规则，不是建议**）：
//! - **非敏感配置走 `duck-store` 的 SQLite**（明文，但导出 / 备份 / 迁移都方便）；
//! - **敏感值只走这里**，SQLite 里**只存键名**，绝不存值。
//!
//! 为什么值得单独一层：`duck-store` 的 `kv` 表是明文的，而「数据导出」会把
//! `settings/kv.json` 打进归档——一旦 token 落进 `kv`，它就会跟着用户随手
//! 分享的备份包一起流出去，而且**不可撤回**。把敏感值收口到本模块，导出侧
//! 只需要在 `manifest` 里写明「包里没有凭据」，这件事就闭死了。
//!
//! # 存在哪：程序自己的文件，不碰系统钥匙串
//!
//! 凭据落在 `<应用数据目录>/credentials.json`（macOS 上是
//! `~/Library/Application Support/DuckToolbox/credentials.json`），是一个
//! `{键名: 密文}` 的 JSON 对象。**刻意不使用操作系统的钥匙串 / 凭据管理器**：
//!
//! - 与系统绑定的存储会把「本应用的数据」拆成两半——备份、迁移、排查时都只
//!   搬得走一半，用户换台机器就得重新把每个口令输一遍；
//! - 系统后端本身有可用性差异（钥匙串锁着、Secret Service 没起来、Linux 上
//!   根本没有），于是同一份产品在不同机器上功能不一致；
//! - 它还会带来一堆与本应用无关的弹窗与授权路径。macOS 对「不是本进程创建」
//!   的条目会弹一次系统授权框，而本仓 dev 构建是 ad-hoc 签名、**每次重编译
//!   签名都变**，条目里的创建者随即失配 —— 那个框会每次都弹，
//!   连「始终允许」也救不了。
//!
//! # 值是密文，密钥另行落盘
//!
//! 每条值是 [`crate::vault::seal`] 的产出（AES-256-GCM，带随机 nonce），
//! 解密用的密钥在 `app_data_dir()/vault.key`（0600）。于是：
//! 这个文件单独被拷走、被 grep、被误分享出去，都读不出明文。
//! 换机器时**两个文件要一起带** —— 只带一个是读不出来的，
//! 这正是「与本机绑定」的含义。
//!
//! 文件权限仍然收着（Unix 下目录 `0700`、文件 `0600`）：加密不是把权限位
//! 变成可选项的理由，两层是叠加的。
//!
//! # 不写日志
//!
//! 本模块**不产生任何日志行**。运行日志会落盘，而且会被「数据导出」整包打走
//! （`logs/` 是可选项），所以这里经手的值一行都不能进日志。失败原因一律通过
//! [`SecretError`] 交给调用方：调用方可以记录「存储不可用」这个**事实**，
//! 但不能记录值本身。
//!
//! # 读不懂的文件是**硬错误**，不是空存储
//!
//! 文件不存在 = 还没存过任何凭据（首启的正常状态）。但文件**存在却解析不了**
//! （被手改坏了、写了一半断电了）一律报 [`SecretError::Failed`]，绝不回落成空表：
//! 回落的话，下一次写入会把这份读不懂的文件整体覆盖掉，用户其余凭据无声消失。
//!
//! # 写盘是原子的
//!
//! 先写同目录下的 `credentials.json.tmp`，再 `rename` 到位。`rename` 在同一
//! 文件系统内是原子的，所以不会出现「半截 JSON」——那种文件在下次启动时按上面的
//! 契约会让所有写入失败，用户就再也存不进凭据了。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::paths;
use crate::vault::{self, VaultError};

/// 凭据文件名，落在 [`paths::app_data_dir`] 下。
///
/// 与 `duck.sqlite3` 平级：**同一个目录里的两个文件**，用户的「搬走整个应用
/// 数据目录」这一个动作就能把设置与凭据一起带走。
pub const FILE_NAME: &str = "credentials.json";

/// 键名长度上限（字节）。
///
/// 128 是给「`<插件id>.<工具名>`」这类复合键留的余量。把上限收在我们的这一侧，
/// 是为了让错误在**写之前**出现，而不是写进文件之后才发现某个键名长得离谱。
const MAX_KEY_BYTES: usize = 128;

/// 新目录的权限位：只有属主能进（Unix）。
const DIR_MODE: u32 = 0o700;

/// 凭据文件的权限位：只有属主能读写（Unix）。
const FILE_MODE: u32 = 0o600;

/// 凭据存储操作失败的原因。
///
/// `Display` 是英文（本仓日志与错误约定）；界面文案由调用方用 `tr` 组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// 本机找不到一个可写的落点（连系统临时目录都拿不到）。
    ///
    /// 本仓的平台目录回落到 `std::env::temp_dir()`（见 [`paths`]），所以这条
    /// 事实上只会在环境被剥得极干净时出现。
    UnsupportedPlatform,
    /// 落点存在但此刻够不着：权限被拒、磁盘满、文件被别的进程占着。
    Unavailable(String),
    /// 请求被拒：键名不合法（空 / 超长 / 含白名单外的字符）。
    Rejected(String),
    /// 文件在，但内容不可用（不是合法 JSON、编码不对）。
    Failed(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("there is no writable location for the credential file")
            }
            Self::Unavailable(detail) => write!(f, "the credential file is unreachable: {detail}"),
            Self::Rejected(detail) => write!(f, "the request was rejected: {detail}"),
            Self::Failed(detail) => write!(f, "the credential file is unusable: {detail}"),
        }
    }
}

impl std::error::Error for SecretError {}

/// 后端抽象。存在的理由有两个：让上层逻辑能对着内存实现测，以及让「换一个落点」
/// 成为可能（例如测试里的临时目录，或将来把凭据托管到企业的密钥服务）。
///
/// 契约：**「键不存在」是 `Ok(None)`，不是错误**。存储自己负责把「缺失」归一掉，
/// 调用方不需要知道 `ENOENT` 存在。
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
/// **它不是回落方案**：进程退出即丢。它的用途只有一个——让凭据相关的上层逻辑
/// 能在不碰磁盘的前提下被测。
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

/// 文件后端：一份 JSON 对象，整读整写。
///
/// 用 `BTreeMap` 而不是 `HashMap` 有两个好处：键序稳定（同一份内容反复写出的
/// 字节完全一致，diff / 校验都省事），以及不依赖哈希随机种子。
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 凭据文件的路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 确保父目录存在。**只对本次新建的目录收紧权限**：已存在的目录是用户的，
    /// 它在被创建时就已经定好了权限，这里不该替用户改。
    fn prepare_dir(&self) -> Result<(), SecretError> {
        let Some(dir) = self.path.parent() else {
            return Err(SecretError::UnsupportedPlatform);
        };
        if dir.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(dir).map_err(|error| unreachable_path(dir, error))?;
        restrict(dir, DIR_MODE)?;
        Ok(())
    }

    /// 读出整份凭据表。文件不存在 = 空表（不是错误）。
    fn read_all(&self) -> Result<BTreeMap<String, String>, SecretError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(unreachable_path(&self.path, error)),
        };
        if text.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        // ⚠️ 解析失败**必须**冒错，绝不回落成空表：回落的话，下一次 `set` 会把
        // 这份读不懂的文件整体覆盖成「只有刚写进去的那一条」，用户其余凭据全部
        // 丢失，而且全程没有任何提示。报错至少能让写入停下来。
        serde_json::from_str(&text).map_err(|error| {
            SecretError::Failed(format!(
                "{} is not valid JSON: {error}",
                self.path.display()
            ))
        })
    }

    /// 整份写回。走「临时文件 + `rename`」，见模块文档「写盘是原子的」。
    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<(), SecretError> {
        self.prepare_dir()?;
        let json = serde_json::to_string_pretty(entries).map_err(|error| {
            SecretError::Failed(format!("could not encode the credential file: {error}"))
        })?;
        let temp = self.temp_path();
        std::fs::write(&temp, json.as_bytes()).map_err(|error| unreachable_path(&temp, error))?;
        // 先收紧再改名：改名之后才设权限的话，中间那一瞬文件是可读的。
        restrict(&temp, FILE_MODE)?;
        std::fs::rename(&temp, &self.path).map_err(|error| {
            // 改名失败就把临时文件收掉，别在用户的目录里留垃圾。
            let _ = std::fs::remove_file(&temp);
            unreachable_path(&self.path, error)
        })
    }

    /// 临时文件路径（与目标同目录，`rename` 才能保证是同一文件系统内的原子操作）。
    fn temp_path(&self) -> PathBuf {
        match self.path.file_name() {
            Some(name) => {
                let mut temp = name.to_os_string();
                temp.push(".tmp");
                self.path.with_file_name(temp)
            }
            None => self.path.with_extension("tmp"),
        }
    }
}

impl SecretStore for FileStore {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let _guard = lock();
        let mut entries = self.read_all()?;
        // 值没变就不写盘：设置页的「保存」是幂等的，用户点两次不该产生两次磁盘写。
        // 比的是**明文**——密文每次都不同（nonce 随机），拿密文比等于永远不相等。
        let unchanged = entries
            .get(key)
            .and_then(|sealed| vault::open(sealed).ok())
            .as_deref()
            == Some(value);
        if unchanged {
            return Ok(());
        }
        entries.insert(key.to_owned(), vault::seal(value).map_err(crypto_error)?);
        self.write_all(&entries)
    }

    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let _guard = lock();
        let entries = self.read_all()?;
        let Some(sealed) = entries.get(key) else {
            return Ok(None);
        };
        // 解不开时**冒错**，而不是当成「没配过」。两者对调用方的含义完全不同：
        // 一个是环境故障（换了机器、密钥文件被删、密文被改），一个是空配置。
        // 把前者伪装成后者，是这类代码里最难查的一类 bug。
        vault::open(sealed).map(Some).map_err(crypto_error)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let _guard = lock();
        let mut entries = self.read_all()?;
        // 幂等：删一个不存在的键算成功，而且要**在动磁盘之前**就返回——
        // 否则一次纯粹的删除会顺手把文件（甚至父目录）造出来。
        if entries.remove(key).is_none() {
            return Ok(());
        }
        self.write_all(&entries)
    }
}

/// 进程内的写串行化。
///
/// 读改写是三步（读整份 → 改一条 → 写整份），`duck-app` 又是**单进程多窗口**：
/// 两个窗口同时保存设置，没有这把锁就会各自基于同一份旧内容写回，丢掉对方那一条。
fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 默认凭据存储：`<应用数据目录>/credentials.json`。
///
/// 惰性建一次、全进程共用（路径只取决于平台目录，不会中途变）。
#[must_use]
pub fn default_store() -> &'static FileStore {
    static DEFAULT: OnceLock<FileStore> = OnceLock::new();
    DEFAULT.get_or_init(|| FileStore::new(paths::app_data_dir().join(FILE_NAME)))
}

/// 默认凭据文件的路径。
///
/// 给界面用：「凭据存在哪」不该是个谜——设置页直接把它显示出来，用户想自己
/// 备份、检查或删除的时候有地方可去。
#[must_use]
pub fn path() -> PathBuf {
    default_store().path().to_path_buf()
}

/// 凭据存储**此刻**能不能用。
///
/// 实现上会真的读一次文件（见 [`probe`]）。调用方若要在渲染路径上反复问，
/// **自己缓存结果**。
#[must_use]
pub fn available() -> bool {
    probe().is_ok()
}

/// 不可用时的具体原因；可用时返回 `Ok(())`。
///
/// 做两件事：确保父目录在、把现有文件读一遍。**不写入任何东西**——包括不做
/// 「写一个探针条目再删掉」那种测试，一次探测不该在用户的凭据文件里留痕。
///
/// 「能建出目录」就够代表「密钥也写得进去」：密钥文件在同一个目录里，
/// 目录写不了的时候它就是写不了的，所以这里不必再去碰一次 `vault.key`。
///
/// 文件存在但解析不了同样算「现在用不了」：该让界面提前把这件事说出来，
/// 而不是等用户填完密钥点了保存才报错。
pub fn probe() -> Result<(), SecretError> {
    let store = default_store();
    store.prepare_dir()?;
    store.read_all().map(|_| ())
}

/// 写入一条凭据（新建或覆盖）。
pub fn set(key: &str, value: &str) -> Result<(), SecretError> {
    set_with(default_store(), key, value)
}

/// 读取一条凭据；键不存在时是 `Ok(None)`。
pub fn get(key: &str) -> Result<Option<String>, SecretError> {
    get_with(default_store(), key)
}

/// 删除一条凭据。**幂等**：删一个不存在的键算成功。
pub fn delete(key: &str) -> Result<(), SecretError> {
    delete_with(default_store(), key)
}

/// 键是否存在。
pub fn exists(key: &str) -> Result<bool, SecretError> {
    exists_with(default_store(), key)
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
/// 为什么要在**写之前**自己校验：键名是这份 JSON 里的键，写歪了自己就读不回来
/// ——「拼错键名导致刚存的口令读不到」是这类代码最常见的自伤。白名单顺带把空格、
/// 换行、控制字符与非 ASCII 挡在门外：它们在文件里要么看不见、要么会被编辑器悄悄
/// 改写，排查起来全是噪音。
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

/// 收紧路径的权限位。
///
/// 只影响 Unix；Windows 上文件继承用户 profile 目录的 ACL，而那个目录默认已经
/// 只对本人开放，因此不做额外处理（`icacls` 那套不是本仓要背的负担）。
fn restrict(path: &Path, mode: u32) -> Result<(), SecretError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| unreachable_path(path, error))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// 把一次 IO 失败包成 [`SecretError::Unavailable`]，并把出错的路径带上。
///
/// 带路径是刻意的：这条错误最后会出现在界面提示里，用户看到具体文件才有下一步
/// （去看权限、去看磁盘）可走。
fn unreachable_path(path: &Path, error: std::io::Error) -> SecretError {
    SecretError::Unavailable(format!("{}: {error}", path.display()))
}

/// 把一次加解密失败归一成 [`SecretError`]。
///
/// 与 [`crate::vault::VaultError::Io`] 的差别值得留意：密钥文件读写不出来
/// 是「此刻够不着」（可能只是磁盘满了），而归到 `Unavailable`；
/// 密文本体不对完全是另一回事（数据坏了），归 `Failed`。
fn crypto_error(error: VaultError) -> SecretError {
    match error {
        VaultError::Io(detail) => SecretError::Unavailable(detail),
        VaultError::Crypto | VaultError::Invalid => SecretError::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FILE_NAME, FileStore, MAX_KEY_BYTES, MemoryStore, SecretError, delete_with, exists_with,
        get_with, set_with, validate_key,
    };
    use std::path::{Path, PathBuf};

    /// 一条合法的键名，供各用例复用。
    const KEY: &str = "provider.example.token";

    // ── 临时目录 ──────────────────────────────────────────────────────

    /// 一个只属于本用例的临时目录，`Drop` 时清掉。
    ///
    /// 目录名带进程 id 与用例标签：`cargo test` 在同一进程里并行跑用例，
    /// 共用一个目录名就会互相踩（症状是「明明刚写进去就读不到」）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("duck-secret-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        /// 目录里的凭据文件路径。目录本身此时**还不存在**——
        /// 正好顺带覆盖「父目录要由存储自己建出来」这条路径。
        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }

        fn dir(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // 有一个用例故意把「目录」的位置写成一个普通文件（见
            // `a_file_where_the_directory_belongs_is_reported`），所以两条路都要收。
            if self.0.is_dir() {
                let _ = std::fs::remove_dir_all(&self.0);
            } else {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    fn file_store(tag: &str) -> (TempDir, FileStore) {
        let temp = TempDir::new(tag);
        let store = FileStore::new(temp.file());
        (temp, store)
    }

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

    /// 值是空串与含非 ASCII 的字符串时同样要原样往返。
    #[test]
    fn odd_but_legal_values_survive_the_round_trip() {
        let store = MemoryStore::new();
        for value in ["", "  ", "换行\n制表\t", "emoji 🔐 与空格"] {
            set_with(&store, KEY, value).expect("写入");
            assert_eq!(get_with(&store, KEY).expect("读回").as_deref(), Some(value));
        }
    }

    // ── 文件后端的往返 ────────────────────────────────────────────────

    #[test]
    fn a_real_round_trip_lands_in_the_credential_file() {
        let (temp, store) = file_store("round-trip");
        // 父目录还不存在，存储得自己建出来。
        assert!(!temp.dir().exists(), "前提：临时目录是空的");

        set_with(&store, KEY, "round-trip-value").expect("写入凭据文件");
        assert!(temp.file().is_file(), "文件应当被创建出来");
        assert_eq!(
            get_with(&store, KEY).expect("读回").as_deref(),
            Some("round-trip-value")
        );
        assert!(exists_with(&store, KEY).expect("存在性"));

        delete_with(&store, KEY).expect("删除");
        assert_eq!(get_with(&store, KEY).expect("再读应为空"), None);
        // 幂等：删完再删一次也应当是成功
        delete_with(&store, KEY).expect("重复删除");
    }

    /// 写入必须是**读改写**：动一条不能把别的条目带走。
    #[test]
    fn writing_one_key_keeps_the_others() {
        let (_temp, store) = file_store("keep-others");
        set_with(&store, "a", "1").expect("写 a");
        set_with(&store, "b", "2").expect("写 b");
        set_with(&store, "a", "3").expect("改 a");
        assert_eq!(get_with(&store, "a").expect("读 a").as_deref(), Some("3"));
        assert_eq!(get_with(&store, "b").expect("读 b").as_deref(), Some("2"));
        delete_with(&store, "a").expect("删 a");
        assert_eq!(get_with(&store, "b").expect("读 b").as_deref(), Some("2"));
    }

    /// 文件是给人看的：键序稳定、内容是一眼能读懂的 JSON。
    #[test]
    fn the_file_is_stable_sorted_json() {
        let (temp, store) = file_store("stable-json");
        set_with(&store, "b", "2").expect("写 b");
        set_with(&store, "a", "1").expect("写 a");
        let first = std::fs::read_to_string(temp.file()).expect("读文件");
        set_with(&store, "c", "3").expect("写 c");
        let second = std::fs::read_to_string(temp.file()).expect("读文件");
        // `BTreeMap` 的键序：a 一定排在 b 前面。
        let a_at = first.find("\"a\"").expect("有 a");
        let b_at = first.find("\"b\"").expect("有 b");
        assert!(a_at < b_at, "键应当按字面序写出来：{first}");
        assert!(second.contains("\"a\""), "后写的条目不该洗掉旧的：{second}");
    }

    /// 值没变就不落盘——设置页「保存」是幂等的，连点两次不该产生两次磁盘写。
    ///
    /// 用 mtime 判定：这里要钉的正是「有没有真的写」这件事。
    #[test]
    fn writing_the_same_value_does_not_touch_the_file() {
        let (temp, store) = file_store("no-rewrite");
        set_with(&store, KEY, "same").expect("首次写入");
        let first = std::fs::metadata(temp.file())
            .expect("metadata")
            .modified()
            .expect("mtime");
        set_with(&store, KEY, "same").expect("再写同一个值");
        let second = std::fs::metadata(temp.file())
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert_eq!(first, second, "值没变就不该重写文件");
    }

    /// 删一个不存在的键，不该顺手把文件造出来。
    #[test]
    fn deleting_a_missing_key_does_not_create_the_file() {
        let (temp, store) = file_store("delete-no-create");
        delete_with(&store, KEY).expect("删不存在的键");
        assert!(!temp.file().exists(), "纯粹的删除不该凭空造出文件");
    }

    // ── 「读不懂」的契约 ──────────────────────────────────────────────

    /// 文件坏了要**报错**，不能回落成空存储。
    ///
    /// 这条是整个模块里最要紧的一条：回落的话，紧接着的一次写入会把读不懂的
    /// 文件整体覆盖，用户其余凭据无声丢失。
    #[test]
    fn a_corrupt_file_is_an_error_rather_than_an_empty_store() {
        let (temp, store) = file_store("corrupt");
        std::fs::create_dir_all(temp.dir()).expect("建目录");
        std::fs::write(temp.file(), b"{ this is not json").expect("写坏文件");

        assert!(matches!(get_with(&store, KEY), Err(SecretError::Failed(_))));
        assert!(matches!(
            set_with(&store, KEY, "value"),
            Err(SecretError::Failed(_))
        ));
        // 关键：写入失败之后，坏文件**原样还在**，没有被抹成空表。
        let after = std::fs::read_to_string(temp.file()).expect("读回");
        assert_eq!(after, "{ this is not json", "坏文件不该被覆盖");
    }

    /// 文件里存的必须是密文——「把这份 JSON 拷走 / grep / 误分享出去也读不出明文」
    /// 是这一层的卖点，所以把它钉住。
    #[test]
    fn the_file_does_not_contain_the_plaintext() {
        let (temp, store) = file_store("no-plaintext");
        set_with(&store, KEY, "sk-distinctive-token-0123456789").expect("写入");
        let raw = std::fs::read_to_string(temp.file()).expect("读文件");
        assert!(
            !raw.contains("sk-distinctive-token-0123456789"),
            "文件里出现了明文：{raw}"
        );
        // 键名保持明文：人打开这个文件时至少要看得懂「哪一条是哪一条」，
        // 而键名本身不是秘密（`ai:api_key`、`dbconn:<id>:password` 这类）。
        assert!(raw.contains(KEY), "键名应当可读：{raw}");
    }

    /// 密文坏了要**报错**，不能当成「没配过」。
    ///
    /// 两者对调用方的含义完全不同：前者是环境故障（换了机器、密钥文件被删、
    /// 密文被改），后者是空配置。把故障伪装成空配置会让界面说「还没配」，
    /// 而真相是「配了但读不出来」。
    #[test]
    fn a_broken_ciphertext_is_reported_rather_than_read_as_missing() {
        let (temp, store) = file_store("broken-cipher");
        std::fs::create_dir_all(temp.dir()).expect("建目录");
        std::fs::write(temp.file(), format!("{{\"{KEY}\":\"not-hex-at-all\"}}")).expect("写坏密文");
        assert!(matches!(get_with(&store, KEY), Err(SecretError::Failed(_))));
    }

    /// 空文件（或只有空白）视作「还没有凭据」，不是坏文件。
    #[test]
    fn an_empty_file_reads_as_no_credentials() {
        let (temp, store) = file_store("empty-file");
        std::fs::create_dir_all(temp.dir()).expect("建目录");
        std::fs::write(temp.file(), b"   \n").expect("写空白文件");
        assert_eq!(get_with(&store, KEY).expect("读空白文件"), None);
        set_with(&store, KEY, "value").expect("写进去");
        assert_eq!(
            get_with(&store, KEY).expect("读回").as_deref(),
            Some("value")
        );
    }

    /// 目录位置被一个普通文件占着时报 `Unavailable`，而不是 panic 或者静默失败。
    #[test]
    fn a_file_where_the_directory_belongs_is_reported() {
        let temp = TempDir::new("dir-is-a-file");
        std::fs::write(temp.dir(), b"not a directory").expect("占住目录名");
        let store = FileStore::new(temp.file());
        assert!(matches!(
            set_with(&store, KEY, "value"),
            Err(SecretError::Unavailable(_))
        ));
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

    /// 校验必须发生在**动存储之前**：非法键名不该让文件被创建出来。
    #[test]
    fn a_rejected_key_never_touches_the_store() {
        let (temp, store) = file_store("rejected-key");
        assert!(set_with(&store, "bad key", "value").is_err());
        assert_eq!(get_with(&store, "bad").expect("读"), None);
        assert!(!temp.file().exists(), "不该留下文件");
    }

    // ── 默认落点 ──────────────────────────────────────────────────────

    /// 默认凭据文件在应用数据目录下，与 `duck.sqlite3` 平级——
    /// 「搬走整个应用数据目录」这一个动作就能把设置与凭据一起带走。
    #[test]
    fn the_default_file_lives_in_the_app_data_directory() {
        let expected = crate::paths::app_data_dir().join(FILE_NAME);
        assert_eq!(super::path(), expected);
    }

    /// 权限位是这套方案唯一的保护，所以把它钉住。
    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (temp, store) = file_store("perms");
        set_with(&store, KEY, "value").expect("写入");

        let file = std::fs::metadata(temp.file())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600, "凭据文件应当只有属主可读写");
        let dir = std::fs::metadata(temp.dir())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(dir & 0o777, 0o700, "凭据目录应当只有属主可进");
    }
}
