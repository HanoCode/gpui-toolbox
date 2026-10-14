//! 本机加密保管箱：把需要**读回来用**的秘密加密后落进 SQLite。
//!
//! # 为什么 SSH 口令不进「键名 → 密文」凭据文件（决策）
//!
//! [`super::secret`] 的 `credentials.json` 是凭据的默认归宿，但 SSH 连接口令是
//! 一个例外 —— 它要跟档案行同生共死，两条硬需求把它顶了出来：
//!
//! 1. **自动登录要求读得回来**：口令要交给 askpass 助手去应答 ssh 的密码提示；
//! 2. **编辑连接要求回显**：用户改个端口不该被要求把口令重输一遍。
//!
//! 两条都意味着「应用要能随时无提示地读」，而 macOS 钥匙串的 ACL 恰好卡死这里：
//! 本仓 dev 构建是 ad-hoc 签名，**每次重编译签名都变**，条目 ACL 里的「创建者」
//! 随即失配 —— 每次读都弹一次系统授权框（用户看到的「点连接弹系统密码器」），
//! 且「始终允许」也救不了（授权跟着签名走，签名变了授权就作废）。开源 SSH
//! 客户端（PuTTY / Remmina / WindTerm）普遍因此把口令加密存进自己的本地库，
//! 本模块采用同一思路：
//!
//! - **密钥**：AES-256-GCM 密钥 32 字节，首次用时生成，落 `app_data_dir()/vault.key`
//!   （权限 0600）。密钥与本机绑定 —— 拷走数据库文件而没有密钥文件，读不出任何口令。
//! - **密文**：hex(`nonce(12B)` ‖ `AES-256-GCM(明文)`），存进档案记录
//!   （`duck-store` 的 SQLite）。SQLite 里 grep 不到明文，满足设计文档的验收线。
//! - **不做**：钥匙串迁移（读旧条目必弹框，正是要消灭的东西）；旧条目留给用户在
//!   「钥匙串访问」里自行删除（服务名 `com.duck.toolbox`）。
//!
//! # 与 [`super::secret`] 的分工
//!
//! [`super::secret`]（`credentials.json`）是凭据的**默认**归宿；连接口令类
//! （要读回自动登录 + 编辑回显）走本模块加密落库，多一层静态防护 ——
//! 拿到数据库文件而没有密钥文件，读不出任何口令。
//!
//! | 值 | 去处 |
//! |---|---|
//! | 连接口令（SSH / 数据库，要读回 + 回显） | 本模块（AES-256-GCM 加密落 SQLite） |
//! | 其余凭据 | [`super::secret`]（`credentials.json`，0600） |
//!
//! # 不写日志
//!
//! 与 [`super::secret`] 同一条规矩：明文与密钥都不进日志，错误只说**发生了什么**，
//! 不带值。

use std::fmt;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

/// 密钥文件名（在 [`crate::app_data_dir()`] 下）。
const KEY_FILE: &str = "vault.key";
/// AES-256 密钥长度（字节）。
const KEY_LEN: usize = 32;

/// 保管箱操作失败的原因。
///
/// `Display` 是英文（本仓日志与错误约定）；界面文案由调用方用 `tr` 组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// 密钥文件读不出来 / 写不进去（磁盘满、目录权限不对）。
    Io(String),
    /// 加解密本体失败（ring 的 `Unspecified` 不带原因，这是刻意的：错误细节
    /// 不该帮攻击者缩小范围）。
    Crypto,
    /// 密文格式不对（长度、hex、编码后的明文不是 UTF-8）。
    Invalid,
}

impl fmt::Display for VaultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(formatter, "the vault key file is unreachable: {detail}"),
            Self::Crypto => formatter.write_str("the cryptographic operation failed"),
            Self::Invalid => formatter.write_str("the sealed value is malformed"),
        }
    }
}

impl std::error::Error for VaultError {}

/// 进程内缓存的密钥。`OnceLock` 而不是每次读写都碰盘：
/// 连接与弹窗都会走这里，一次 IO 换之后全程内存操作。
static KEY: OnceLock<Result<[u8; KEY_LEN], VaultError>> = OnceLock::new();

/// 密钥文件的落点。
fn key_path() -> PathBuf {
    crate::app_data_dir().join(KEY_FILE)
}

/// 读出（或首次生成）密钥。
///
/// 用 `create_new` 做原子的「没有就建」：两个线程同时首用时，一个创建成功，
/// 另一个落到读取分支，不会互相覆盖。
///
/// ⚠️ 光靠 `create_new` **不够**：先创建的那个还没把 hex 写进去时，另一个线程
/// 就已经能读到那个空文件了 —— `unhex("")` 得到长度为 0 的 `Vec`，转不成
/// 32 字节，报 `Invalid`，于是「第一次保存凭据」偶发失败。所以这里另加一把
/// 进程内的锁把「首用」串起来，并在拿到锁之后**再查一次缓存**（等锁期间
/// 可能已经有线程把它填好了）。
///
/// 进程之间不需要考虑这件事：本仓有单实例锁，同时只会跑一个实例。
fn load_or_create_key() -> Result<[u8; KEY_LEN], VaultError> {
    if let Some(Ok(key)) = KEY.get() {
        return Ok(*key);
    }
    static FIRST_USE: Mutex<()> = Mutex::new(());
    let _guard = FIRST_USE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 双重检查：等锁的这段时间里，别的线程可能已经把密钥准备好了。
    if let Some(Ok(key)) = KEY.get() {
        return Ok(*key);
    }
    let result = load_or_create_key_uncached();
    // 缓存**成功值**；失败（磁盘满、目录只读）不缓存 —— 用户腾出空间后
    // 下一次调用应该真的重试，而不是把「坏了」这个结论记一辈子。
    if let Ok(key) = result {
        let _ = KEY.set(Ok(key));
    }
    result
}

fn load_or_create_key_uncached() -> Result<[u8; KEY_LEN], VaultError> {
    let path = key_path();
    if let Some(parent) = path.parent() {
        crate::ensure_dir(parent).map_err(|error| VaultError::Io(error.to_string()))?;
    }
    match create_new_key_file(&path) {
        Ok(mut created) => {
            // 新文件：生成 32 字节随机密钥，hex 落盘。
            let mut key = [0u8; KEY_LEN];
            SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| VaultError::Crypto)?;
            use std::io::Write as _;
            created
                .write_all(hex(&key).as_bytes())
                .map_err(|error| VaultError::Io(error.to_string()))?;
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // 已有文件：读回并校验长度。
            let text = std::fs::read_to_string(&path)
                .map_err(|error| VaultError::Io(error.to_string()))?;
            let bytes = unhex(text.trim()).ok_or(VaultError::Invalid)?;
            bytes.try_into().map_err(|_| VaultError::Invalid)
        }
        Err(error) => Err(VaultError::Io(error.to_string())),
    }
}

/// 以「已存在即失败」的方式开密钥文件（unix 下顺带压成 0600）。
fn create_new_key_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().write(true).create_new(true).open(path)
    }
}

/// 加密一段明文，返回可落库的密文串：hex(`nonce` ‖ `密文 + tag`)。
pub fn seal(plaintext: &str) -> Result<String, VaultError> {
    let key = load_or_create_key()?;
    let unbound = UnboundKey::new(&AES_256_GCM, &key).map_err(|_| VaultError::Crypto)?;
    let key = LessSafeKey::new(unbound);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| VaultError::Crypto)?;
    let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| VaultError::Crypto)?;

    // `seal_in_place_append_tag` 把 tag 追加在原地，出来就是「一次性给解密方」的完整包。
    let mut in_out = plaintext.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| VaultError::Crypto)?;

    let mut out = String::with_capacity(2 * (NONCE_LEN + in_out.len()));
    out.push_str(&hex(&nonce_bytes));
    out.push_str(&hex(&in_out));
    Ok(out)
}

/// 解开 [`seal`] 产出的密文；格式不对或密钥不匹配都是 [`VaultError::Invalid`]。
pub fn open(sealed: &str) -> Result<String, VaultError> {
    let key = load_or_create_key()?;
    let bytes = unhex(sealed.trim()).ok_or(VaultError::Invalid)?;
    if bytes.len() <= NONCE_LEN {
        return Err(VaultError::Invalid);
    }
    let unbound = UnboundKey::new(&AES_256_GCM, &key).map_err(|_| VaultError::Crypto)?;
    let key = LessSafeKey::new(unbound);
    let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| VaultError::Crypto)?;
    let mut in_out = ciphertext.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| VaultError::Crypto)?;
    String::from_utf8(plain.to_vec()).map_err(|_| VaultError::Invalid)
}

/// 小写 hex 编码（不引依赖；口令密文顶多几百字节，体积翻倍无所谓）。
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// hex 解码；奇数长度或非 hex 字符都是 `None`。
fn unhex(text: &str) -> Option<Vec<u8>> {
    let chars: Vec<char> = text.chars().collect();
    // `& 1` 而不是 `% 2`：`is_multiple_of` 是 1.87 的 API，本仓 MSRV 1.85。
    if chars.len() & 1 == 1 {
        return None;
    }
    let value = |c: char| -> Option<u8> { c.to_digit(16).map(|d| d as u8) };
    chars
        .chunks(2)
        .map(|pair| Some(value(pair[0])? << 4 | value(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{KEY_LEN, VaultError, hex, load_or_create_key_uncached, open, seal, unhex};

    /// 同一把密钥下，密封再打开必须原样往返（含非 ASCII 与空串）。
    #[test]
    fn sealed_values_round_trip() {
        // 测试进程与真实应用共用 `app_data_dir()`：这一条也会顺带验证
        // 「密钥文件不存在时自动创建」。真实磁盘 IO 是这里唯一的可信证据。
        for plaintext in ["correct horse", "口令🔐", ""] {
            let sealed = seal(plaintext).expect("加密");
            assert_ne!(sealed, plaintext, "密文不该等于明文");
            assert_eq!(open(&sealed).expect("解密"), plaintext);
        }
    }

    /// 每次加密用新 nonce：同一明文两次密文不同，但都能解回原值。
    #[test]
    fn sealing_twice_uses_fresh_nonces() {
        let first = seal("same").expect("第一次加密");
        let second = seal("same").expect("第二次加密");
        assert_ne!(first, second, "nonce 复用等于 GCM 报废");
        assert_eq!(open(&first).as_deref(), Ok("same"));
        assert_eq!(open(&second).as_deref(), Ok("same"));
    }

    /// 密文被动过一个字节就必须解不开 —— 这是 AEAD 的存在意义。
    ///
    /// 篡改落在**解码后的字节**上、最后再重新编码：早期写法直接在密文 hex 字符串的
    /// 末位做 `wrapping_add(1)`，而末位字符域是 `0-9a-f`，取到 `9` / `f` 时会变成
    /// `:` / `g` 这种**非法 hex**，于是命中的是 [`VaultError::Invalid`]（格式错）而
    /// 不是 [`VaultError::Crypto`]（认证失败）—— 2/16 的概率间歇性报红，与被测的
    /// 认证语义无关。
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let sealed = seal("integrity matters").expect("加密");
        let mut bytes = unhex(&sealed).expect("密文是 hex");
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert_eq!(open(&hex(&bytes)), Err(VaultError::Crypto));
    }

    /// 坏格式（奇数长度、非 hex、过短）一律 `Invalid`，不是恐慌。
    #[test]
    fn malformed_ciphertext_is_invalid_not_a_crash() {
        assert_eq!(open(""), Err(VaultError::Invalid));
        assert_eq!(open("abc"), Err(VaultError::Invalid));
        assert_eq!(open("zzzz"), Err(VaultError::Invalid));
        // 24 个 hex 字符 = 12 字节，恰好只有 nonce 没有密文体。
        assert_eq!(open(&"ab".repeat(12)), Err(VaultError::Invalid));
    }

    /// hex 编解码自洽（密钥文件与密文都靠它落盘）。
    #[test]
    fn hex_helpers_round_trip() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)), Some(bytes));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
    }

    /// 密钥文件：首次创建后落盘，第二次读回同一把（真实 IO）。
    #[test]
    fn key_file_is_created_once_and_stable() {
        let first = load_or_create_key_uncached().expect("首用生成");
        assert_eq!(first.len(), KEY_LEN);
        let second = load_or_create_key_uncached().expect("读回");
        assert_eq!(first, second, "密钥必须稳定，否则旧密文全部作废");
        // 落盘的是 hex（64 字符），且文件权限 0600（unix）。
        let path = crate::app_data_dir().join("vault.key");
        let text = std::fs::read_to_string(&path).expect("读密钥文件");
        assert_eq!(text.trim().len(), 2 * KEY_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("元数据")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "密钥文件不能对同机其他用户开放");
        }
    }
}
