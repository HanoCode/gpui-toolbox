//! 取归档（以及 manifest 本身）：走 HTTPS，边收边写边算摘要。
//!
//! # 为什么是阻塞式客户端
//!
//! 调用点只会在后台任务里跑它，永远不在 UI 线程上。异步客户端（或者流式读的异步版本）
//! 要拖进一个运行时，为一个更新下载再开一个是错的取舍。阻塞版本一样能流式读：
//! 归档**从不整包进内存**，峰值就是这里的 64 KiB 缓冲。
//!
//! # 超时按「一次没人盯着的传输」定
//!
//! 全局超时给到 30 分钟：十几 MB 的包在烂网下就该跑几分钟，这不是有人盯着看的请求。
//! 真正的上限是**体积**而不是耐心——超过了 manifest 承诺的大小就立刻放弃。

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::CHUNK;
use crate::error::UpdateError;
use crate::manifest::ManifestFile;
use crate::sha256::Hasher;

/// 允许比承诺大出多少才判定「这不是我要的东西」。
///
/// 留这一点余量只是为了让错误说的是「和承诺的不一样」，而不是恰好在边界上截断。
/// 完全不设上限的话，一个跳到巨大文件的重定向会在摘要来得及反对之前把磁盘填满。
const OVERSHOOT_ALLOWANCE: u64 = 64 * 1024;

/// 整次传输的时间预算。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 建连超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// manifest 只有几 KB，给它一个短得多的时间预算。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 上限：manifest 再大也不该超过这个数（防止一个错误的重定向让我们读进一个巨大的 body）。
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;

const USER_AGENT: &str = concat!("DuckToolbox/", env!("CARGO_PKG_VERSION"), " (updater)");

/// 进度回调告诉下载器下一步做什么。
///
/// 让回调**返回**一个决定，而不是让下载器自己去读某个共享标志，是为了让这一层
/// 对「为什么会停」没有任何意见——应用拿一个原子标志决定，测试拿一个计数器决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// 继续。
    Continue,
    /// 中止：半份归档会被删掉，比一份都没有更糟。
    Abort,
}

/// 已经写到磁盘、但**还没被信任**的归档。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Downloaded {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

impl Downloaded {
    /// 归档落在哪。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 实际写下去的字节数。
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 传输途中算出的摘要。
    ///
    /// 只是个**便宜的早退信号**；安装前的关口是
    /// [`verify`](crate::verify::verify) 重新读盘算出来的那个。
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// 取一份小文档（manifest）的原始字节。
///
/// # Errors
/// 网络层失败、非 2xx 状态码、或超出 [`MANIFEST_LIMIT`] 都返回 [`UpdateError::Network`]。
pub(crate) fn fetch(url: &str) -> Result<Vec<u8>, UpdateError> {
    if !url.starts_with("https://") {
        return Err(UpdateError::Network(format!("not an https url: {url:?}")));
    }

    let agent = agent(MANIFEST_TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| UpdateError::Network(error.to_string()))?;

    response
        .body_mut()
        .with_config()
        .limit(MANIFEST_LIMIT)
        .read_to_vec()
        .map_err(|error| UpdateError::Network(error.to_string()))
}

/// 把 `file` 流式写到 `destination`。
///
/// 按 [`ManifestFile::urls`] 的顺序试：主地址失败就换一个备用地址（加速通道的备用
/// 那一半就是靠这个生效的）。**全部失败**才报错，报的是**最后一个**地址的错误——
/// 那是最接近「现在到底怎么了」的一条。
///
/// 用户中止是唯一例外：那是明确的「别下了」，立刻停，不会拿着这个意图去试下一个地址。
///
/// `progress` 在开始前调一次（0），之后每收满一块调一次，它的回答决定是否继续。
///
/// # Errors
/// 全部地址都失败时返回最后一个错误；非 https 地址、写盘失败、超出承诺大小、
/// 用户中止，分别对应 [`UpdateError`] 的不同变体。中止与超限都会把半份文件删掉，
/// 不给下游留下可发现的东西。
pub fn stream(
    file: &ManifestFile,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<Downloaded, UpdateError> {
    first_success(file.urls(), |url| {
        stream_one(url, file.size(), destination, progress)
    })
}

/// 按顺序试每一个来源，返回**第一个**成功的结果。
///
/// 中止（[`UpdateError::Cancelled`]）是唯一会立刻穿透出来的错误：那是用户说「别下了」，
/// 拿这个意图去试下一个来源毫无意义。其余错误都先记着往下试，全部失败才把**最后
/// 一个**交出去——那是最接近「现在到底怎么了」的一条。
///
/// 抽成独立函数是为了它能在没有网络的情况下被测到：来源列表与尝试动作都是参数。
fn first_success<'a, T>(
    urls: impl Iterator<Item = &'a str>,
    mut attempt: impl FnMut(&str) -> Result<T, UpdateError>,
) -> Result<T, UpdateError> {
    let mut last: Option<UpdateError> = None;

    for (index, url) in urls.enumerate() {
        if index > 0 {
            tracing::warn!(%url, "the previous download source failed; trying the next one");
        }
        match attempt(url) {
            Ok(value) => return Ok(value),
            Err(UpdateError::Cancelled) => return Err(UpdateError::Cancelled),
            Err(error) => last = Some(error),
        }
    }

    // `urls()` 至少有一个元素（主地址），所以这里不会落空；写兜底而不是 `expect`，
    // 是为了将来真把主地址改成可选时不会炸在这里。
    Err(last.unwrap_or_else(|| UpdateError::Download("the archive has no download url".to_owned())))
}

/// 从**一个**地址流式写到 `destination`。
///
/// # Errors
/// 见 [`stream`]。
fn stream_one(
    url: &str,
    expected_size: u64,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<Downloaded, UpdateError> {
    // `manifest::parse` 已经拒过非 https 的地址；这里是那条腰带的背带——
    // 因为这个函数才是把 URL 变成用户磁盘上一个文件的那一步。
    if !url.starts_with("https://") {
        return Err(UpdateError::Download(format!("not an https url: {url:?}")));
    }

    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| UpdateError::Io(format!("{}: {error}", parent.display())))?;
    }

    let agent = agent(DOWNLOAD_TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| UpdateError::Download(error.to_string()))?;

    let mut out = File::create(destination)
        .map_err(|error| UpdateError::Io(format!("{}: {error}", destination.display())))?;

    let total = expected_size;
    let limit = total.saturating_add(OVERSHOOT_ALLOWANCE);
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut written: u64 = 0;

    if progress(0, total) == Flow::Abort {
        drop(out);
        let _ = std::fs::remove_file(destination);
        return Err(UpdateError::Cancelled);
    }

    let mut body = response.body_mut().as_reader();
    loop {
        let read = body
            .read(&mut buffer)
            .map_err(|error| UpdateError::Download(error.to_string()))?;
        if read == 0 {
            break;
        }

        let chunk = &buffer[..read];
        out.write_all(chunk)
            .map_err(|error| UpdateError::Io(format!("{}: {error}", destination.display())))?;
        hasher.update(chunk);
        written += read as u64;

        if written > limit {
            // 放弃，而不是把磁盘写满。半份文件一起删掉：下游不该能找到它。
            drop(out);
            let _ = std::fs::remove_file(destination);
            return Err(UpdateError::SizeMismatch {
                expected: total,
                actual: written,
            });
        }

        // 中止每块检查一次，所以取消一个下载会在一块之内停下，而不是等到结束。
        if progress(written, total) == Flow::Abort {
            drop(out);
            let _ = std::fs::remove_file(destination);
            return Err(UpdateError::Cancelled);
        }
    }

    out.flush()
        .map_err(|error| UpdateError::Io(format!("{}: {error}", destination.display())))?;
    // 字节得真的落在盘上，而不是停在页缓存里——校验器马上会重新读它们。
    out.sync_all()
        .map_err(|error| UpdateError::Io(format!("{}: {error}", destination.display())))?;
    drop(out);

    Ok(Downloaded {
        path: destination.to_path_buf(),
        bytes: written,
        sha256: hasher.finalize_hex(),
    })
}

/// 建一个只走 HTTPS 的 agent。
///
/// 每次调用建一个：更新下载是稀罕事，为此养一个长连接池不划算，也就没有共享状态
/// 需要管。`https_only` 让重定向也逃不掉。
fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(USER_AGENT)
        .build();
    ureq::Agent::new_with_config(config)
}

#[cfg(test)]
mod tests {
    use super::first_success;
    use crate::error::UpdateError;

    /// 第一个成功就收工，后面的来源一个都不该被碰到。
    #[test]
    fn the_first_success_wins_and_no_further_source_is_touched() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_owned());
            Ok::<_, UpdateError>(url.to_owned())
        });

        assert_eq!(result.ok().as_deref(), Some("primary"));
        assert_eq!(tried, ["primary"], "it must stop at the first success");
    }

    /// 主地址失败要**继续试备用地址**——加速通道的备用那一半全靠这一步。
    #[test]
    fn a_failing_primary_falls_through_to_the_next_source() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_owned());
            if url == "primary" {
                Err::<&str, _>(UpdateError::Download("refused".to_owned()))
            } else {
                Ok("downloaded")
            }
        });

        assert_eq!(result.ok(), Some("downloaded"));
        assert_eq!(tried, ["primary", "mirror"]);
    }

    /// 全失败时报的是**最后**一个来源的错误——那是最接近现状的一条。
    #[test]
    fn when_every_source_fails_the_last_error_is_reported() {
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            Err::<&str, _>(UpdateError::Download(format!("{url} refused")))
        });

        let error = result.expect_err("both sources were made to fail");
        assert!(error.to_string().contains("mirror refused"), "{error}");
    }

    /// 中止立刻穿透：用户说了别下，就不该再去试备用源。
    #[test]
    fn cancelling_pierces_instead_of_trying_the_next_source() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_owned());
            Err::<&str, _>(UpdateError::Cancelled)
        });

        assert_eq!(result, Err(UpdateError::Cancelled));
        assert_eq!(
            tried,
            ["primary"],
            "an abort is not a reason to try elsewhere"
        );
    }

    /// 一个来源都没有时给一句说得出口的话，而不是让兜底表达式旁边留个 panic。
    #[test]
    fn an_empty_source_list_is_reported_rather_than_panicking() {
        let empty: [&str; 0] = [];
        let result = first_success(empty.into_iter(), |_| Ok::<_, UpdateError>("never"));

        assert!(matches!(result, Err(UpdateError::Download(_))));
    }
}
