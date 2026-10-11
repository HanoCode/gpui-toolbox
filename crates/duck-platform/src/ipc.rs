//! 单实例转发通道：让第二个实例把手上的请求交给正在运行的那个。
//!
//! [`crate::lock`] 只回答「有没有别的实例在跑」，然后让位者直接退出。退出是对的，
//! 但它手里那份「用户到底要我打开什么」就丢了——`duck duck://tool/http.replay`
//! 在应用已运行时应该**跳到那个工具**，而不是什么都不发生。
//!
//! 本模块补的就是这一段：一条同一台机器内的短连接，送一批 [`OpenRequest`] 过去。
//!
//! # 两端
//!
//! - Unix：`<应用数据目录>/duck.ipc` 上的 `UnixListener`，socket 文件权限收紧到 `0600`；
//! - 其它（Windows）：绑定 `127.0.0.1` 的**随机端口**，把端口号写进 `<应用数据目录>/duck.ipc`。
//!
//! Windows 上不用 Unix socket 是因为 `std` 没有；不用固定端口是因为它可能被别的程序占，
//! 那时会把「端口被占」误判成「已有实例在跑」。随机端口 + 落盘的实际端口没有这个问题。
//!
//! # 为什么这不是第二个单实例锁
//!
//! 它**不替代** `flock`：谁是「正主」仍由 [`crate::lock`] 判定。本模块只负责送信。
//! 唯一的例外是 Windows——那里 `flock` 退化成无锁，于是「端口能不能连上」顺带成了
//! 该平台上的单实例判据。
//!
//! # 协议
//!
//! 纯文本行，不引 serde：内容只有「一批 URL / 路径」，为它拉进一个 JSON 依赖不划算。
//! 见 [`encode`] / [`decode`]。首行是魔数，认不出来的一律丢掉——宁可什么都不做，
//! 也不要把别的程序连上来时发的字节当成用户意图。

use std::io;
use std::path::PathBuf;

use crate::open::OpenRequest;

/// 协议魔数（含版本号，格式变了就一起变）。
const HEADER: &str = "duck-open/1";
/// 单条 URL 记录的标签。
const TAG_URL: &str = "U ";
/// 单条路径记录的标签。
const TAG_PATH: &str = "F ";
/// 一次转发最多读这么多字节。
///
/// 对端是同一台机器上的自家客户端，正常情况下只有几百字节；设上限是因为
/// 「有人往这个 socket 灌 1 GB」不该把我们自己的内存一起吃掉。
const MAX_PAYLOAD: u64 = 1024 * 1024;
/// 读写超时：连不上、发不出去都要**快**失败——这是一条位于退出路径上的调用，
/// 卡住就等于应用关不掉。
const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);

/// 转发通道失败的原因。
///
/// `Display` 是英文（日志与错误约定）；界面文案由调用方组织。
#[derive(Debug)]
pub enum IpcError {
    /// 已经有实例在监听——第二个实例应当让位。
    AlreadyListening,
    /// 没有实例在监听，没有可送的对象。
    NoListener,
    /// 通道建不出来 / 连不上 / 写不进去。
    Unavailable(io::Error),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyListening => f.write_str("another instance is already listening"),
            Self::NoListener => f.write_str("no running instance is listening"),
            Self::Unavailable(error) => write!(f, "the hand-off channel is unusable: {error}"),
        }
    }
}

impl std::error::Error for IpcError {}

/// 监听句柄：它活着的期间，后台线程一直受理转发。
///
/// 丢弃时删掉落点文件（Unix 的 socket 文件 / Windows 的端口号文件）。进程被 `kill -9`
/// 时不会走到这里，那种情况留下的陈旧文件由下一次 [`listen_for_requests`] 清掉。
#[derive(Debug)]
pub struct IpcListener {
    endpoint: PathBuf,
}

impl IpcListener {
    /// 落点位置（日志与排查用）。
    #[must_use]
    pub fn endpoint(&self) -> &std::path::Path {
        &self.endpoint
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

/// 转发通道的落点：Unix 是 socket 文件，Windows 是写着端口号的文件。
#[must_use]
pub fn endpoint_path() -> PathBuf {
    crate::paths::app_data_dir().join("duck.ipc")
}

/// 开始受理转发。
///
/// `handler` 在**后台线程**上被调用，所以它必须是 `Send`——这也意味着它不能直接碰 gpui
/// 的状态（`App` 不是 `Send`）。调用方的正确姿势是往一个通道里投递，让主线程去取：
///
/// ```ignore
/// let (sender, receiver) = futures_channel::mpsc::unbounded();
/// let listener = duck_platform::listen_for_requests(move |requests| {
///     let _ = sender.unbounded_send(requests);
/// })?;
/// // 主线程：receiver 交给 `cx.spawn` 出来的任务消费。
/// ```
///
/// # Errors
/// - [`IpcError::AlreadyListening`]：已经有一个实例在监听（调用方应当让位）。
/// - [`IpcError::Unavailable`]：目录建不出来、socket 建不出来、端口写不进去。
pub fn listen_for_requests(
    handler: impl Fn(Vec<OpenRequest>) + Send + 'static,
) -> Result<IpcListener, IpcError> {
    listen_at(endpoint_path(), handler)
}

/// 把请求送给正在运行的实例。
///
/// # Errors
/// - [`IpcError::NoListener`]：没有实例在监听（正常情况：这次启动自己就是第一个）。
/// - [`IpcError::Unavailable`]：连上了但送不出去。
pub fn forward_to_running_instance(requests: &[OpenRequest]) -> Result<(), IpcError> {
    forward_at(endpoint_path(), requests)
}

// ── 协议（纯函数，无 IO）─────────────────────────────────────────────────

/// 把一批请求编码成一条转发消息。
///
/// 含换行的条目会被**跳过**：协议是行式的，一条含换行的路径会把后面的字节变成
/// 另一条记录。路径里有换行在实践中几乎不存在，丢掉它比发一条会被对端误读的消息好。
#[must_use]
pub fn encode(requests: &[OpenRequest]) -> String {
    let mut payload = String::from(HEADER);
    for request in requests {
        let line = match request {
            OpenRequest::Url(url) => format!("{TAG_URL}{url}"),
            OpenRequest::Path(path) => format!("{TAG_PATH}{}", path.display()),
        };
        if line.contains(['\n', '\r']) {
            tracing::debug!("dropping a hand-off entry that contains a line break");
            continue;
        }
        payload.push('\n');
        payload.push_str(&line);
    }
    payload.push('\n');
    payload
}

/// 解码一条转发消息；不是我们的协议时返回空表。
///
/// 认不出的行**逐行忽略**而不是整条放弃：这样一来，新版本加一种标签之后，
/// 老版本收到它仍然能处理自己认识的那几条。
#[must_use]
pub fn decode(payload: &str) -> Vec<OpenRequest> {
    let mut lines = payload.lines();
    if lines.next() != Some(HEADER) {
        return Vec::new();
    }
    let mut requests = Vec::new();
    for line in lines {
        // 空值不要：`U ` 后面什么都没有的条目是噪声，收下来只会变成一个打不开的 URL。
        if let Some(url) = line.strip_prefix(TAG_URL) {
            if !url.is_empty() {
                requests.push(OpenRequest::Url(url.to_owned()));
            }
            // 标签匹配过就不再往下看：两个标签互斥，再查一次前缀没有意义。
            continue;
        }
        if let Some(path) = line.strip_prefix(TAG_PATH)
            && !path.is_empty()
        {
            requests.push(OpenRequest::Path(PathBuf::from(path)));
        }
    }
    requests
}

// ── Unix：Unix socket ────────────────────────────────────────────────────
#[cfg(unix)]
fn listen_at(
    endpoint: PathBuf,
    handler: impl Fn(Vec<OpenRequest>) + Send + 'static,
) -> Result<IpcListener, IpcError> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    // 落点已被占：连得上说明有活着的实例，连不上说明是上次没退干净留下的空壳。
    // 不区分这两者就会出现「应用崩过一次之后再也收不到转发」。
    if endpoint.exists() {
        if UnixStream::connect(&endpoint).is_ok() {
            return Err(IpcError::AlreadyListening);
        }
        let _ = std::fs::remove_file(&endpoint);
    }
    if let Some(parent) = endpoint.parent() {
        crate::paths::ensure_dir(parent).map_err(IpcError::Unavailable)?;
    }

    let listener = UnixListener::bind(&endpoint).map_err(IpcError::Unavailable)?;
    // socket 文件默认按 umask 给权限；收紧到「只有本人」——它是一条能驱动界面的入口。
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))
        .map_err(IpcError::Unavailable)?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                // accept 失败多半是 fd 耗尽：立刻重试只会把 CPU 空转满，
                // 让出一下时间片，等别的连接先释放掉。
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            };
            let _ = stream.set_read_timeout(Some(TIMEOUT));
            let mut payload = String::new();
            // 借用 `&stream` 加限长读；直接 `stream.take(..)` 会拿走所有权，之后就写不了应答。
            let _ = (&stream).take(MAX_PAYLOAD).read_to_string(&mut payload);
            let requests = decode(&payload);
            if !requests.is_empty() {
                handler(requests);
            }
            // 应答让转发方知道「送到了」，不必靠超时反推。
            let _ = stream.write_all(b"ok\n");
        }
    });

    Ok(IpcListener { endpoint })
}

#[cfg(unix)]
fn forward_at(endpoint: PathBuf, requests: &[OpenRequest]) -> Result<(), IpcError> {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(&endpoint).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            IpcError::NoListener
        } else {
            IpcError::Unavailable(error)
        }
    })?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(IpcError::Unavailable)?;
    stream
        .write_all(encode(requests).as_bytes())
        .map_err(IpcError::Unavailable)?;
    // 关掉写端：对端读到 EOF 才会停止等待，否则它要耗到读超时。
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

// ── 其它（Windows）：loopback TCP + 端口文件 ─────────────────────────────
#[cfg(not(unix))]
fn listen_at(
    endpoint: PathBuf,
    handler: impl Fn(Vec<OpenRequest>) + Send + 'static,
) -> Result<IpcListener, IpcError> {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};

    if let Some(port) = read_port(&endpoint)
        && TcpStream::connect(("127.0.0.1", port)).is_ok()
    {
        return Err(IpcError::AlreadyListening);
    }
    if let Some(parent) = endpoint.parent() {
        crate::paths::ensure_dir(parent).map_err(IpcError::Unavailable)?;
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(IpcError::Unavailable)?;
    let port = listener.local_addr().map_err(IpcError::Unavailable)?.port();
    std::fs::write(&endpoint, port.to_string()).map_err(IpcError::Unavailable)?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                // accept 失败多半是 fd 耗尽：立刻重试只会把 CPU 空转满，
                // 让出一下时间片，等别的连接先释放掉。
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            };
            let _ = stream.set_read_timeout(Some(TIMEOUT));
            let mut payload = String::new();
            let _ = (&stream).take(MAX_PAYLOAD).read_to_string(&mut payload);
            let requests = decode(&payload);
            if !requests.is_empty() {
                handler(requests);
            }
            let _ = stream.write_all(b"ok\n");
        }
    });

    Ok(IpcListener { endpoint })
}

#[cfg(not(unix))]
fn forward_at(endpoint: PathBuf, requests: &[OpenRequest]) -> Result<(), IpcError> {
    use std::io::Write as _;
    use std::net::TcpStream;

    let Some(port) = read_port(&endpoint) else {
        return Err(IpcError::NoListener);
    };
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(IpcError::Unavailable)?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(IpcError::Unavailable)?;
    stream
        .write_all(encode(requests).as_bytes())
        .map_err(IpcError::Unavailable)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// 读端口号文件；读不出来就是不认识（陈旧文件、被人改坏、还没建）。
#[cfg(not(unix))]
fn read_port(endpoint: &std::path::Path) -> Option<u16> {
    std::fs::read_to_string(endpoint).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{IpcError, decode, encode, forward_at, listen_at};
    use crate::open::OpenRequest;
    use std::path::PathBuf;
    use std::sync::mpsc;

    /// 短路径：macOS 的 `sun_path` 只有 104 字节，而 `temp_dir()` 在那边是
    /// `/var/folders/…/T/`（约 50 字符），加长文件名会越界，所以 Unix 上固定走 `/tmp`。
    fn endpoint(tag: &str) -> PathBuf {
        let root = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        root.join(format!("duck-ipc-{}-{tag}", std::process::id()))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_round_trip_preserves_every_request() {
        let requests = vec![
            OpenRequest::Url("duck://tool/http.replay".to_owned()),
            OpenRequest::Path(PathBuf::from("/tmp/带空格 的目录/a.txt")),
        ];
        assert_eq!(decode(&encode(&requests)), requests);
    }

    #[test]
    fn a_foreign_payload_decodes_to_nothing() {
        // 别的程序连上来发什么都行；只要魔数不对，我们就当没听见。
        assert!(decode("").is_empty());
        assert!(decode("GET / HTTP/1.1\r\n\r\n").is_empty());
        assert!(decode("duck-open/0\nU duck://x\n").is_empty());
    }

    #[test]
    fn unknown_tags_are_ignored_line_by_line() {
        // 前向兼容：新版本加的标签，老版本认不出，但不该连累它认识的那几条。
        let payload = "duck-open/1\nS something-new\nU duck://tool/x\n";
        assert_eq!(
            decode(payload),
            vec![OpenRequest::Url("duck://tool/x".to_owned())]
        );
    }

    #[test]
    fn entries_containing_a_line_break_are_dropped_rather_than_misread() {
        // 含换行的路径会把后面的字节变成另一条记录；宁可丢它，也不要发一条会被误读的消息。
        let requests = vec![
            OpenRequest::Path(PathBuf::from("/tmp/has\nnewline")),
            OpenRequest::Url("duck://tool/kept".to_owned()),
        ];
        assert_eq!(
            decode(&encode(&requests)),
            vec![OpenRequest::Url("duck://tool/kept".to_owned())]
        );
    }

    #[test]
    fn an_empty_batch_still_carries_the_header() {
        assert_eq!(encode(&[]), format!("{}\n", super::HEADER));
        assert!(decode(&encode(&[])).is_empty());
    }

    #[test]
    fn a_forwarded_batch_reaches_the_listener() {
        let path = endpoint("forward");
        cleanup(&path);
        let (sender, receiver) = mpsc::channel();
        let listener = listen_at(path.clone(), move |requests| {
            let _ = sender.send(requests);
        })
        .expect("first listener should bind");

        let sent = vec![
            OpenRequest::Url("duck://tool/image.convert".to_owned()),
            OpenRequest::Path(PathBuf::from("/tmp/a b.png")),
        ];
        forward_at(path.clone(), &sent).expect("forwarding should succeed");

        let received = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the listener should have been called");
        assert_eq!(received, sent);

        drop(listener);
        cleanup(&path);
    }

    #[test]
    fn a_second_listener_is_turned_away() {
        let path = endpoint("busy");
        cleanup(&path);
        let first = listen_at(path.clone(), |_| {}).expect("first listener should bind");

        match listen_at(path.clone(), |_| {}) {
            Err(IpcError::AlreadyListening) => {}
            other => panic!("expected AlreadyListening, got {other:?}"),
        }

        drop(first);
        cleanup(&path);
    }

    #[test]
    fn a_leftover_endpoint_is_reclaimed() {
        // 上次 `kill -9` 留下的空壳：没人监听，就该被清掉重新绑定，
        // 否则应用崩过一次之后就再也收不到转发了。
        let path = endpoint("stale");
        cleanup(&path);
        std::fs::write(&path, b"leftover").expect("should be able to write a leftover file");

        let listener =
            listen_at(path.clone(), |_| {}).expect("a stale endpoint should be reclaimable");

        drop(listener);
        cleanup(&path);
    }

    #[test]
    fn forwarding_without_a_listener_says_so() {
        let path = endpoint("nobody");
        cleanup(&path);
        match forward_at(path.clone(), &[OpenRequest::Url("duck://x".to_owned())]) {
            Err(IpcError::NoListener) => {}
            other => panic!("expected NoListener, got {other:?}"),
        }
    }

    #[test]
    fn dropping_the_listener_removes_the_endpoint() {
        let path = endpoint("cleanup");
        cleanup(&path);
        let listener = listen_at(path.clone(), |_| {}).expect("listener should bind");
        assert!(path.exists(), "the endpoint should exist while listening");
        drop(listener);
        assert!(
            !path.exists(),
            "dropping the listener should take the endpoint with it"
        );
    }
}
