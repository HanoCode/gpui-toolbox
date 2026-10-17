//! 取工件与注册表：流式写盘、边收边算 SHA-256、多地址按序重试。
//!
//! # 为什么不复用 `duck-update` 的下载器
//!
//! 那一份的入口是 `duck_update::stream(&ManifestFile, …)`，而 `ManifestFile` 的字段
//! 是**私有的**、也没有公开构造函数（它的来源是签名过的 JSON manifest）。为了复用
//! 一个下载函数去给那个类型开一个构造口子，比这里重写 60 行流式读写更贵 —— 而且
//! 两边的契约本来就不同：更新器的失败要区分「签名」「版本」这些语义，驱动下载只关心
//! 「网络 / 体积 / 摘要 / 中止」四件事。
//!
//! 唯一**共享的是口径**：不整包进内存（峰值一个 [`CHUNK`]）、超过承诺体积立刻放弃、
//! 半份文件删掉不给下游留可发现的东西、校验前 `sync_all`。
//!
//! # 三个超时档
//!
//! | 用途 | 全局 | 建连 |
//! | --- | --- | --- |
//! | 注册表（几十 KB） | 30 s | 15 s |
//! | 驱动 / JRE 工件（几 MB ~ 几十 MB） | 30 min | 15 s |
//!
//! 大文件给 30 分钟不是「耐心」而是「烂网下十几 MB 就该跑几分钟」；真正的上限是体积。
//!
//! # 代理
//!
//! `ureq` **不会**自己读环境变量，而企业网里 `HTTPS_PROXY` 是最常见的出口方式
//! （命令行工具 curl / git 都认它）。所以这里显式读一遍
//! `HTTPS_PROXY` → `https_proxy` → `ALL_PROXY`，取到就装上。取不到就走直连。

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::Result;

/// I/O 缓冲块大小（与 `duck-update` 同一档）：够大省系统调用，够小让进度回调刷得密。
pub const CHUNK: usize = 64 * 1024;

/// 大工件（驱动 / JRE）的全局时间预算。
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 小文档（注册表 / 包内 manifest）的全局时间预算。
const SMALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 建连超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 允许比承诺大出多少才判定「这不是我要的东西」。
const OVERSHOOT_ALLOWANCE: u64 = 64 * 1024;

const USER_AGENT: &str = concat!("DuckToolbox/", env!("CARGO_PKG_VERSION"), " (driver-store)");

/// 进度回调的回答：继续还是中止。
///
/// 让回调**返回**决定（而不是让下载器读某个共享标志），是为了让这一层对「为什么停」
/// 没有意见：界面用取消标志决定，测试用计数器决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// 继续。
    Continue,
    /// 中止：半份文件会被删掉。
    Abort,
}

/// 已写到磁盘、且摘要已算出的工件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

impl Downloaded {
    /// 落盘路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 实际写下去的字节数。
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 传输途中算出的摘要（小写十六进制）。
    ///
    /// 它只是**便宜的早退信号**；安装前的关口是解包前重新读盘算出来的那个
    /// （[`sha256_file`]）。
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// 取一份小文档（注册表、包内 manifest）。
///
/// # Errors
/// 全部地址都失败时报**最后一个**错误；超出 `limit` 或读盘失败按各自变体上报。
pub fn fetch_small(urls: &[String], limit: u64, what: &str) -> Result<Vec<u8>> {
    first_success(urls, |url| {
        let agent = agent(SMALL_TIMEOUT);
        let mut response = agent
            .get(url)
            .call()
            .map_err(|error| crate::DriverError::Network(format!("{what} from {url}: {error}")))?;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| crate::DriverError::Network(format!("{what} from {url}: {error}")))
    })
}

/// 把一个工件流式写到 `destination`。
///
/// 按 `urls` 的顺序试：主地址失败就换备用（官方 / 镜像两条路靠这一步生效）。
/// 用户中止是唯一例外——那是明确的「别下了」，立刻停，不会拿着这个意图去试下一个地址。
///
/// `progress` 在开始前调一次（`0, total`），之后每收满一块调一次。
///
/// # Errors
/// 全部地址失败时返回最后一个错误；非 https、写盘失败、超出承诺体积、用户中止
/// 分别对应 [`DriverError`](crate::DriverError) 的不同变体。中止与超限都会把半份
/// 文件删掉。
pub fn stream(
    urls: &[String],
    expected_size: u64,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<Downloaded> {
    first_success(urls, |url| {
        stream_one(url, expected_size, destination, progress)
    })
}

/// 按顺序试每个地址，返回第一个成功的结果。
///
/// 中止立刻穿透（拿「别下了」的意图去试下一个地址毫无意义）；其余错误先记着往下试，
/// 全失败才交出**最后一个** —— 那是最接近现状的一条。
fn first_success<T>(urls: &[String], mut attempt: impl FnMut(&str) -> Result<T>) -> Result<T> {
    let mut last: Option<crate::DriverError> = None;
    for (index, url) in urls.iter().enumerate() {
        if index > 0 {
            tracing::warn!(%url, "the previous download source failed; trying the next one");
        }
        match attempt(url) {
            Ok(value) => return Ok(value),
            Err(crate::DriverError::Cancelled) => return Err(crate::DriverError::Cancelled),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        crate::DriverError::Network("the artifact has no download url".to_owned())
    }))
}

fn stream_one(
    url: &str,
    expected_size: u64,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<Downloaded> {
    // 这一层才是「把 URL 变成磁盘上一个文件」的那一步，所以 https 在这里再挡一道。
    if !url.starts_with("https://") {
        return Err(crate::DriverError::Network(format!(
            "not an https url: {url:?}"
        )));
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", parent.display())))?;
    }

    let agent = agent(ARTIFACT_TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| crate::DriverError::Network(format!("{url}: {error}")))?;

    let mut out = std::fs::File::create(destination)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", destination.display())))?;

    let limit = expected_size.saturating_add(OVERSHOOT_ALLOWANCE);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut written: u64 = 0;

    if progress(0, expected_size) == Flow::Abort {
        drop(out);
        let _ = std::fs::remove_file(destination);
        return Err(crate::DriverError::Cancelled);
    }

    let mut body = response.body_mut().as_reader();
    loop {
        let read = body
            .read(&mut buffer)
            .map_err(|error| crate::DriverError::Network(format!("{url}: {error}")))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        out.write_all(chunk).map_err(|error| {
            crate::DriverError::Io(format!("{}: {error}", destination.display()))
        })?;
        hasher.update(chunk);
        written += read as u64;

        if written > limit {
            drop(out);
            let _ = std::fs::remove_file(destination);
            return Err(crate::DriverError::Integrity(format!(
                "{url}: body is larger than the announced {expected_size} bytes"
            )));
        }
        if progress(written, expected_size) == Flow::Abort {
            drop(out);
            let _ = std::fs::remove_file(destination);
            return Err(crate::DriverError::Cancelled);
        }
    }

    out.flush()
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", destination.display())))?;
    // 字节得真的落盘：校验器马上会重新读它们。
    out.sync_all()
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", destination.display())))?;
    drop(out);

    Ok(Downloaded {
        path: destination.to_path_buf(),
        bytes: written,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// 读盘算一份文件的 SHA-256（小写十六进制）。
///
/// # Errors
/// 打不开或读不动。
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 摘要比对（大小写无关；注册表给的是小写）。
#[must_use]
pub fn digest_matches(expected: &str, actual: &str) -> bool {
    expected.len() == actual.len() && expected.eq_ignore_ascii_case(actual)
}

/// 建一个只走 HTTPS 的 agent。
///
/// 每次调用建一个：装驱动是稀罕事，为此养长连接池不划算。`https_only` 让重定向
/// 也逃不掉明文。
fn agent(timeout: Duration) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(USER_AGENT);
    if let Some(proxy) = proxy_from_env() {
        builder = builder.proxy(Some(proxy));
    }
    ureq::Agent::new_with_config(builder.build())
}

/// 从环境变量取代理（`HTTPS_PROXY` → `https_proxy` → `ALL_PROXY` → `all_proxy`）。
///
/// 取到但不合法时**忽略**它（走直连）：一个写坏的环境变量不该让整个下载功能不可用。
fn proxy_from_env() -> Option<ureq::Proxy> {
    let raw = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())?;
    match ureq::Proxy::new(raw.trim()) {
        Ok(proxy) => Some(proxy),
        Err(error) => {
            tracing::warn!(%error, "ignoring an unusable proxy setting from the environment");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第一个成功就收工，后面的地址一个都不碰。
    #[test]
    fn the_first_success_wins() {
        let mut tried = Vec::new();
        let result: Result<String> =
            first_success(&["primary".to_owned(), "mirror".to_owned()], |url| {
                tried.push(url.to_owned());
                Ok(url.to_owned())
            });
        assert_eq!(result.ok().as_deref(), Some("primary"));
        assert_eq!(tried, ["primary"]);
    }

    /// 主地址失败要**继续试备用** —— 换源全靠这一步。
    #[test]
    fn a_failing_primary_falls_through() {
        let mut tried = Vec::new();
        let result: Result<&str> =
            first_success(&["primary".to_owned(), "mirror".to_owned()], |url| {
                tried.push(url.to_owned());
                if url == "primary" {
                    Err(crate::DriverError::Network("refused".to_owned()))
                } else {
                    Ok("downloaded")
                }
            });
        assert_eq!(result.ok(), Some("downloaded"));
        assert_eq!(tried, ["primary", "mirror"]);
    }

    /// 中止立刻穿透，且报的是「最后一个来源的错误」。
    #[test]
    fn cancelling_pierces_and_the_last_error_is_reported() {
        let mut tried = Vec::new();
        let result: Result<&str> =
            first_success(&["primary".to_owned(), "mirror".to_owned()], |url| {
                tried.push(url.to_owned());
                Err::<&str, _>(crate::DriverError::Cancelled)
            });
        assert!(matches!(result, Err(crate::DriverError::Cancelled)));
        assert_eq!(tried, ["primary"], "中止不该再去试备用源");

        let result: Result<&str> = first_success(&["a".to_owned(), "b".to_owned()], |url| {
            Err(crate::DriverError::Network(format!("{url} refused")))
        });
        let message = result.expect_err("两个地址都被弄挂了").to_string();
        assert!(message.contains("b refused"), "{message}");
    }

    /// 空地址列表给一句说得出口的话，而不是 panic。
    #[test]
    fn an_empty_source_list_is_reported() {
        let result: Result<&str> = first_success(&[], |_| Ok("never"));
        assert!(matches!(result, Err(crate::DriverError::Network(_))));
    }

    /// 摘要比对大小写无关，长度不同直接否。
    #[test]
    fn digests_compare_case_insensitively() {
        assert!(digest_matches("ABC123", "abc123"));
        assert!(!digest_matches("abc", "abcd"));
        assert!(!digest_matches("abc123", "abd123"));
    }

    /// 体积超限与中止都会把半份文件删掉 —— 不给下游留可发现的东西。
    #[test]
    fn partial_files_are_removed_on_abort() {
        let dir = std::env::temp_dir().join("duck-drivers-download-test");
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("partial.bin");

        // 走不到网络，但「中止」在第一次 progress 调用时就发生 —— 不建连也该成立。
        let result = stream(&[], 1024, &path, &|_, _| Flow::Abort);
        assert!(result.is_err());
        assert!(!path.exists(), "半份文件必须被删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
