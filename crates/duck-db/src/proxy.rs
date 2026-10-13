//! 代理隧道：SOCKS5 / HTTP CONNECT，**进程内**转发（零新增依赖）。
//!
//! 与 [`crate::tunnel`]（SSH）的分工：SSH 那条路把工作交给系统 `ssh`（用户自己的
//! `~/.ssh/config` 说了算），代理这条路没有对应的「系统工具」可用（`socat`/`nc` 不是
//! macOS 标配，也不能保证支持 CONNECT 的认证），所以握手与字节中转都在本进程内做。
//!
//! 形态与 SSH 隧道对齐：**对内只暴露一个本机监听端口**（[`ProxyTunnel::local_port`]），
//! 驱动照旧连 `127.0.0.1:{port}`，两边谁也不知道对端是谁。
//!
//! 两条协议都按 RFC / 规范原文实现，不用第三方 crate：
//!
//! - **SOCKS5**：RFC 1928（握手 + CONNECT 请求）+ RFC 1929（用户名 / 口令认证）。
//! - **HTTP CONNECT**：RFC 9110 §9.3.6（`CONNECT host:port HTTP/1.1`），
//!   认证走 `Proxy-Authorization: Basic`（RFC 7617）。
//!
//! 刻意**不**做的事：
//! - 不解析目标主机的 IP 类型：CONNECT 请求一律用 `ATYP=0x03`（域名）——
//!   代理侧本来就要按名字解析（否则分流规则失效），而且省掉 IPv4/IPv6 判定。
//! - 不代理 UDP（SOCKS5 的 ASSOCIATE）：数据库连接全是 TCP。
//! - 不做认证方式协商的降级：要求认证就发认证，**绝不在用户配了口令时退回匿名**。

use std::io::{BufRead as _, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::DbError;

/// 单次握手的超时（连代理 + 认证 + CONNECT 全程）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// 代理协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    /// SOCKS5（RFC 1928）。
    #[default]
    Socks5,
    /// HTTP CONNECT 隧道。
    HttpConnect,
}

impl ProxyKind {
    /// 两档（界面展示顺序）。
    pub const ALL: [Self; 2] = [Self::Socks5, Self::HttpConnect];

    /// 稳定键（持久化 / 元素 id 用，**不随界面语言变**）。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::HttpConnect => "http_connect",
        }
    }

    /// 从稳定键还原。
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// 协议惯用端口（SOCKS5 1080 / HTTP 代理 8080）。
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Socks5 => 1080,
            Self::HttpConnect => 8080,
        }
    }
}

/// 代理配置（进连接档案；口令不入 kv，见 [`ProxyConfig::password`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 协议。
    #[serde(default)]
    pub kind: ProxyKind,
    /// 代理主机。
    #[serde(default)]
    pub host: String,
    /// 代理端口；`None` 用协议默认端口。
    #[serde(default)]
    pub port: Option<u16>,
    /// 认证用户名（留空 = 匿名）。
    #[serde(default)]
    pub username: String,
    /// 认证口令。
    ///
    /// **`#[serde(skip)]`**：与 SSH 口令同一条口径——口令只经凭据文件进内存，
    /// 绝不随档案落进 kv。序列化出来永远没有它，反序列化也不去读。
    #[serde(skip)]
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 实际使用的端口。
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.kind.default_port())
    }

    /// 认证信息（用户名非空才算「要认证」）。
    #[must_use]
    pub fn credentials(&self) -> Option<(&str, &str)> {
        let user = self.username.trim();
        if user.is_empty() {
            return None;
        }
        Some((user, self.password.as_deref().unwrap_or("")))
    }
}

/// 开一条代理隧道的全部参数。
pub struct ProxyTunnelSpec {
    /// 代理本身。
    pub proxy: ProxyConfig,
    /// 最终要连到的主机（数据库主机；由代理侧解析）。
    pub target_host: String,
    /// 最终要连到的端口（数据库端口）。
    pub target_port: u16,
}

/// 一条代理隧道：本机监听 + 每条连接各自握手并双向中转。
#[derive(Debug)]
pub struct ProxyTunnel {
    /// 本机监听端口。
    local_port: u16,
    /// 收工信号（accept 循环每轮先看它）。
    shutdown: Arc<AtomicBool>,
    /// 转发线程句柄（Drop 时 join，避免线程还在用已释放的资源）。
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProxyTunnel {
    /// 起一条隧道：绑定本机随机端口，起一个 accept 循环。
    pub fn open(spec: ProxyTunnelSpec) -> Result<Self, DbError> {
        let host = spec.proxy.host.trim().to_owned();
        if host.is_empty() {
            return Err(DbError::Config("proxy host is empty".into()));
        }
        if spec.target_host.trim().is_empty() {
            return Err(DbError::Config("proxy target host is empty".into()));
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|err| DbError::Connect(format!("bind local proxy port: {err}")))?;
        let local_port = listener
            .local_addr()
            .map_err(|err| DbError::Connect(format!("local proxy addr: {err}")))?
            .port();

        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let shutdown = Arc::clone(&shutdown);
            // 线程里只带「要连去哪儿」的朴素数据：`ProxyConfig` 含口令，但这条线程本就要
            // 用它去做认证，没有额外扩散面（它不进 kv、不进日志）。
            let proxy = spec.proxy.clone();
            let target_host = spec.target_host.trim().to_owned();
            let target_port = spec.target_port;
            std::thread::Builder::new()
                .name("duck-proxy-tunnel".into())
                .spawn(move || {
                    accept_loop(listener, shutdown, proxy, target_host, target_port);
                })
                .map_err(|err| DbError::Connect(format!("spawn proxy tunnel: {err}")))?
        };
        tracing::info!(
            local_port,
            proxy = %host,
            port = spec.proxy.effective_port(),
            target = %spec.target_host,
            "proxy tunnel listening"
        );
        Ok(Self {
            local_port,
            shutdown,
            worker: Some(worker),
        })
    }

    /// 本机监听端口（数据库连接指向 `127.0.0.1:{local_port}`）。
    #[must_use]
    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    /// 隧道是否还在（转发线程没退出）。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
}

impl Drop for ProxyTunnel {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // 唤醒阻塞在 `accept` 上的线程：自连一次即可（比轮询省事，也不引入超时精度问题）。
        let _ = TcpStream::connect(("127.0.0.1", self.local_port));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// accept 循环：每条进来的连接单起一个线程去握手 + 中转。
///
/// 为什么每条连接一个线程：数据库驱动会开多条 TCP（连接池），串行处理会让第二条
/// 一直等第一条断开——表现是「连上了但一查询就没反应」。
fn accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    proxy: ProxyConfig,
    target_host: String,
    target_port: u16,
) {
    loop {
        match listener.accept() {
            Ok((client, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let proxy = proxy.clone();
                let target_host = target_host.clone();
                std::thread::Builder::new()
                    .name("duck-proxy-conn".into())
                    .spawn(move || {
                        if let Err(error) = serve(client, &proxy, &target_host, target_port) {
                            // 内核一律英文：这条只进日志，界面看到的是驱动那边「连接被重置」。
                            tracing::warn!(%error, "proxy tunnel connection failed");
                        }
                    })
                    .ok();
            }
            Err(error) => {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                tracing::warn!(%error, "proxy tunnel accept failed");
                // 短暂退让，避免 accept 持续失败时把 CPU 打满。
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 单条连接：连代理 → 握手 → 双向中转。
fn serve(
    mut client: TcpStream,
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()> {
    let address = (proxy.host.trim(), proxy.effective_port());
    let resolved = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("proxy address did not resolve"))?;
    let upstream = TcpStream::connect_timeout(&resolved, HANDSHAKE_TIMEOUT)?;
    upstream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    upstream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut upstream = upstream;
    match proxy.kind {
        ProxyKind::Socks5 => {
            socks5_connect(&mut upstream, proxy.credentials(), target_host, target_port)?
        }
        ProxyKind::HttpConnect => {
            http_connect(&mut upstream, proxy.credentials(), target_host, target_port)?
        }
    }
    // 握手完就把超时撤掉：隧道跑起来之后是长连接（查询可能跑几分钟，不能有读超时）。
    upstream.set_read_timeout(None)?;
    upstream.set_write_timeout(None)?;

    // `client` 与 `upstream` 互拷到 EOF。两个方向各一个线程，谁先结束都可能
    // （数据库单方面关闭 / 客户端断开），所以两边都 `shutdown` 一次把对面叫醒。
    let mut client_read = client.try_clone()?;
    let mut upstream_read = upstream.try_clone()?;
    let pump_up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream);
        let _ = upstream.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream_read, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = pump_up.join();
    Ok(())
}

/// SOCKS5：方法协商 →（必要时）用户名口令认证 → CONNECT 请求。
///
/// 抽成对 `Read + Write` 的纯函数，测试里用内存假流就能把每条分支走一遍，
/// 不需要真的起代理。
pub fn socks5_connect<S: Read + Write>(
    stream: &mut S,
    credentials: Option<(&str, &str)>,
    host: &str,
    port: u16,
) -> std::io::Result<()> {
    // ① 方法协商：认证与否给出**同一套**候选，由代理挑。
    let mut greeting = vec![0x05u8, 0x01];
    greeting.push(if credentials.is_some() { 0x02 } else { 0x00 });
    stream.write_all(&greeting)?;
    stream.flush()?;
    let mut picked = [0u8; 2];
    stream.read_exact(&mut picked)?;
    if picked[0] != 0x05 {
        return Err(std::io::Error::other("socks5: not a socks5 proxy"));
    }
    match picked[1] {
        0x00 => {}
        0x02 => {
            // ② 用户名 / 口令（RFC 1929）：**要求认证却没配口令**时如实报错，
            //    绝不退回匿名（那等于把「我以为走的是认证通道」变成裸奔）。
            let (user, password) = credentials.ok_or_else(|| {
                std::io::Error::other("socks5: proxy requires authentication but no credentials")
            })?;
            let user = user.as_bytes();
            let password = password.as_bytes();
            if user.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
                return Err(std::io::Error::other("socks5: credentials too long"));
            }
            let mut auth = vec![0x01u8, user.len() as u8];
            auth.extend_from_slice(user);
            auth.push(password.len() as u8);
            auth.extend_from_slice(password);
            stream.write_all(&auth)?;
            stream.flush()?;
            let mut reply = [0u8; 2];
            stream.read_exact(&mut reply)?;
            if reply[0] != 0x01 || reply[1] != 0x00 {
                return Err(std::io::Error::other("socks5: authentication rejected"));
            }
        }
        0xff => return Err(std::io::Error::other("socks5: no acceptable method")),
        other => {
            return Err(std::io::Error::other(format!(
                "socks5: unsupported auth method {other:#04x}"
            )));
        }
    }

    // ③ CONNECT：ATYP=0x03（域名）+ 大端端口。
    let host_bytes = host.as_bytes();
    if host_bytes.len() > u8::MAX as usize {
        return Err(std::io::Error::other("socks5: host name too long"));
    }
    let mut request = vec![0x05u8, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    request.extend_from_slice(host_bytes);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;
    stream.flush()?;

    // ④ 应答：VER REP RSV ATYP BND.ADDR BND.PORT —— 后面这段长度随 ATYP 变，
    //    但隧道已经建好了，把它们读掉丢弃即可（不读会污染后面的数据库流量）。
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    if head[1] != 0x00 {
        return Err(std::io::Error::other(format!(
            "socks5: connect rejected ({})",
            socks5_reply_text(head[1])
        )));
    }
    let mut discard = match head[3] {
        0x01 => vec![0u8; 4 + 2],
        0x04 => vec![0u8; 16 + 2],
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            vec![0u8; len[0] as usize + 2]
        }
        other => {
            return Err(std::io::Error::other(format!(
                "socks5: unknown address type {other:#04x}"
            )));
        }
    };
    stream.read_exact(&mut discard)?;
    Ok(())
}

/// SOCKS5 应答码的英文说明（RFC 1928 §6；只进日志，界面不展示）。
fn socks5_reply_text(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

/// HTTP CONNECT：发请求 → 读状态行与头 → 要求 2xx。
pub fn http_connect<S: Read + Write>(
    stream: &mut S,
    credentials: Option<(&str, &str)>,
    host: &str,
    port: u16,
) -> std::io::Result<()> {
    let authority = format!("{host}:{port}");
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n"
    );
    if let Some((user, password)) = credentials {
        let token = crate::result::base64_encode(format!("{user}:{password}").as_bytes());
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    // 逐行读到空行为止；只解析状态行，其余头丢弃（`Proxy-Agent` 之类没用）。
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if status.is_empty() {
        return Err(std::io::Error::other(
            "http connect: proxy closed the connection",
        ));
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    if !http_status_is_success(&status) {
        return Err(std::io::Error::other(format!(
            "http connect: proxy refused ({})",
            status.trim()
        )));
    }
    Ok(())
}

/// 状态行是不是 2xx（`HTTP/1.1 200 Connection established`）。
fn http_status_is_success(status_line: &str) -> bool {
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存假流：写进去的记在 `written`，要从 `readable` 里取。
    ///
    /// 这样握手的每一条分支（成功 / 拒绝 / 要求认证）都能走一遍，不必真起代理。
    struct FakeStream {
        to_peer: Vec<u8>,
        readable: std::io::Cursor<Vec<u8>>,
    }

    impl FakeStream {
        fn replying(bytes: &[u8]) -> Self {
            Self {
                to_peer: Vec::new(),
                readable: std::io::Cursor::new(bytes.to_vec()),
            }
        }
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.readable.read(buf)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.to_peer.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 稳定键往返 + 两档的默认端口不同（同一个下拉里不能有两项同值）。
    #[test]
    fn proxy_kind_keys_and_ports_are_distinct() {
        for kind in ProxyKind::ALL {
            assert_eq!(ProxyKind::from_key(kind.key()), Some(kind));
        }
        assert_eq!(ProxyKind::from_key("bogus"), None);
        let ports: std::collections::BTreeSet<u16> = ProxyKind::ALL
            .iter()
            .map(|kind| kind.default_port())
            .collect();
        assert_eq!(ports.len(), ProxyKind::ALL.len());
    }

    /// 没填端口走协议默认；用户名留空就是匿名（不产生凭据）。
    #[test]
    fn proxy_config_falls_back_to_protocol_defaults() {
        let mut cfg = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            ..ProxyConfig::default()
        };
        assert_eq!(cfg.effective_port(), 1080);
        assert_eq!(cfg.credentials(), None);
        cfg.kind = ProxyKind::HttpConnect;
        assert_eq!(cfg.effective_port(), 8080);
        cfg.port = Some(3128);
        assert_eq!(cfg.effective_port(), 3128);
        cfg.username = "  proxy-user  ".into();
        assert_eq!(cfg.credentials(), Some(("proxy-user", "")));
    }

    /// ⚠️ 口令**不随档案落 kv**：序列化出来必须没有它。
    #[test]
    fn proxy_password_never_reaches_the_serialized_form() {
        let cfg = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "proxy.internal".into(),
            port: Some(1080),
            username: "u".into(),
            password: Some("hunter2".into()),
        };
        let json = serde_json::to_string(&cfg).expect("可序列化");
        assert!(!json.contains("hunter2"), "{json}");
        let back: ProxyConfig = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back.password, None);
        assert_eq!(back.username, "u");
    }

    /// SOCKS5 匿名：问候语只报「无认证」，请求用域名 + 大端端口。
    #[test]
    fn socks5_anonymous_negotiation_and_connect() {
        // 应答：方法协商选 0x00，CONNECT 成功（ATYP=1，4 字节地址 + 2 字节端口）。
        let mut stream = FakeStream::replying(&[
            0x05, 0x00, // 方法协商
            0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0x1f, 0x90, // CONNECT 应答
        ]);
        socks5_connect(&mut stream, None, "db.internal", 5432).expect("匿名握手应成功");
        let sent = stream.to_peer;
        assert_eq!(
            &sent[..3],
            &[0x05, 0x01, 0x00],
            "问候语：SOCKS5 + 1 种方法 + 无认证"
        );
        // 请求：VER CMD RSV ATYP LEN "db.internal" PORT(0x15 0x38)
        assert_eq!(&sent[3..8], &[0x05, 0x01, 0x00, 0x03, 11]);
        assert_eq!(&sent[8..19], b"db.internal");
        assert_eq!(&sent[19..21], &5432u16.to_be_bytes());
    }

    /// SOCKS5 带认证：问候语报 0x02，代理挑 0x02 时发用户名口令。
    #[test]
    fn socks5_username_password_authentication() {
        let mut stream = FakeStream::replying(&[
            0x05, 0x02, // 方法协商 → 用户名口令
            0x01, 0x00, // 认证成功
            0x05, 0x00, 0x00, 0x03, 0x00, 0x00,
            0x00, // CONNECT 成功（域名长度 0 + 端口）
        ]);
        socks5_connect(&mut stream, Some(("alice", "s3cret")), "h", 1).expect("认证握手应成功");
        let sent = stream.to_peer;
        assert_eq!(
            &sent[..3],
            &[0x05, 0x01, 0x02],
            "配了口令就报「用户名口令」"
        );
        assert_eq!(&sent[3..4], &[0x01], "RFC 1929 版本号");
        assert_eq!(sent[4], 5, "用户名长度");
        assert_eq!(&sent[5..10], b"alice");
        assert_eq!(sent[10], 6, "口令长度");
        assert_eq!(&sent[11..17], b"s3cret");
    }

    /// ⚠️ 代理要求认证而用户没配口令：**报错，不退回匿名**。
    #[test]
    fn socks5_refuses_to_fall_back_to_anonymous() {
        let mut stream = FakeStream::replying(&[0x05, 0x02]);
        let error = socks5_connect(&mut stream, None, "h", 1).unwrap_err();
        assert!(
            error.to_string().contains("requires authentication"),
            "{error}"
        );
    }

    /// SOCKS5 拒绝与「无可用方法」都要如实报错。
    #[test]
    fn socks5_errors_are_reported_verbatim() {
        let mut rejected =
            FakeStream::replying(&[0x05, 0x00, 0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        let error = socks5_connect(&mut rejected, None, "h", 1).unwrap_err();
        assert!(error.to_string().contains("connection refused"), "{error}");

        let mut none = FakeStream::replying(&[0x05, 0xff]);
        let error = socks5_connect(&mut none, None, "h", 1).unwrap_err();
        assert!(
            error.to_string().contains("no acceptable method"),
            "{error}"
        );

        // 认证被拒。
        let mut bad = FakeStream::replying(&[0x05, 0x02, 0x01, 0x01]);
        let error = socks5_connect(&mut bad, Some(("u", "p")), "h", 1).unwrap_err();
        assert!(
            error.to_string().contains("authentication rejected"),
            "{error}"
        );
    }

    /// HTTP CONNECT：请求行 / Host / Basic 认证都要对，2xx 才算成功。
    #[test]
    fn http_connect_sends_the_expected_request() {
        let mut stream = FakeStream::replying(
            b"HTTP/1.1 200 Connection established\r\nProxy-Agent: test\r\n\r\n",
        );
        http_connect(&mut stream, Some(("alice", "s3cret")), "db.internal", 5432)
            .expect("200 应放行");
        let sent = String::from_utf8(stream.to_peer).expect("请求是文本");
        assert!(
            sent.starts_with("CONNECT db.internal:5432 HTTP/1.1\r\n"),
            "{sent}"
        );
        assert!(sent.contains("Host: db.internal:5432\r\n"), "{sent}");
        // base64("alice:s3cret") = YWxpY2U6czNjcmV0
        assert!(
            sent.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n"),
            "{sent}"
        );
        assert!(sent.ends_with("\r\n\r\n"), "{sent}");
    }

    /// 匿名请求不带 `Proxy-Authorization`；非 2xx 一律拒绝。
    #[test]
    fn http_connect_without_credentials_and_on_failure() {
        let mut anonymous = FakeStream::replying(b"HTTP/1.1 200 OK\r\n\r\n");
        http_connect(&mut anonymous, None, "h", 1).expect("200 应放行");
        let sent = String::from_utf8(anonymous.to_peer).expect("请求是文本");
        assert!(!sent.contains("Proxy-Authorization"), "{sent}");

        let mut refused =
            FakeStream::replying(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n");
        let error = http_connect(&mut refused, None, "h", 1).unwrap_err();
        assert!(error.to_string().contains("407"), "{error}");

        // 代理直接断开（没有状态行）。
        let mut closed = FakeStream::replying(b"");
        let error = http_connect(&mut closed, None, "h", 1).unwrap_err();
        assert!(error.to_string().contains("closed"), "{error}");
    }

    /// 状态行解析只认 2xx（`HTTP/1.1 200` / `HTTP/1.0 204` 通过，其余放行不了）。
    #[test]
    fn only_two_hundred_class_status_lines_pass() {
        assert!(http_status_is_success(
            "HTTP/1.1 200 Connection established"
        ));
        assert!(http_status_is_success("HTTP/1.0 204 No Content"));
        assert!(!http_status_is_success(
            "HTTP/1.1 407 Proxy Authentication Required"
        ));
        assert!(!http_status_is_success("HTTP/1.1 302 Found"));
        assert!(!http_status_is_success("garbage"));
    }

    /// 参数不全时**不拉线程**，直接报配置错误。
    #[test]
    fn rejects_incomplete_specs() {
        let error = ProxyTunnel::open(ProxyTunnelSpec {
            proxy: ProxyConfig::default(),
            target_host: "db".into(),
            target_port: 5432,
        })
        .unwrap_err();
        assert!(matches!(error, DbError::Config(_)));

        let error = ProxyTunnel::open(ProxyTunnelSpec {
            proxy: ProxyConfig {
                host: "127.0.0.1".into(),
                ..ProxyConfig::default()
            },
            target_host: "  ".into(),
            target_port: 5432,
        })
        .unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
    }

    /// 真的起一条隧道：端口被监听，drop 之后线程收工（不泄漏）。
    #[test]
    fn a_tunnel_listens_and_stops_on_drop() {
        let tunnel = ProxyTunnel::open(ProxyTunnelSpec {
            proxy: ProxyConfig {
                kind: ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                // 指向一个没人听的端口：只有握手会失败，accept 循环本身照常。
                port: Some(9),
                ..ProxyConfig::default()
            },
            target_host: "db.internal".into(),
            target_port: 5432,
        })
        .expect("应能起隧道");
        assert!(tunnel.is_alive());
        let port = tunnel.local_port();
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "端口应已监听"
        );
        drop(tunnel);
        // drop 之后线程已 join，端口随之释放（给 OS 一点回收时间）。
        for _ in 0..50 {
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("隧道 drop 后端口 {port} 仍被占用");
    }
}
