//! SMTP 客户端（**发信方向**）：`EHLO` / `STARTTLS` / `AUTH` / `MAIL` / `RCPT` / `DATA`。
//!
//! # 为什么手写而不引 `lettre`
//!
//! 两条：① 引它要同时动 `Cargo.toml` 与 `THIRD_PARTY_LICENSES.md` 两道门禁
//! （`sbin/licenses.py --check` 是发布门禁之一），而本批次只需要 RFC 5321 的一小片；
//! ② 手写把「多行响应解析 / 点填充 / 能力位协商」变成**可对着脚本化服务端跑真对话**
//! 的代码——本文件的测试起一个本地假服务端，逐条断言客户端发出去的东西。
//!
//! 有意不做的：`DSN`、`SMTPUTF8`、`BINARYMIME`、`PIPELINING` 的派发、`CHUNKING`
//! （`BDAT`）。这些都属于「知道服务端支持，但本批次不用」——能力位已经解析出来了，
//! 用到时不必重新协商。
//!
//! # 传输层口径
//!
//! `Plain`（明文）**直接拒绝**，与 [`crate::proto::imap::ImapSession::connect`] 同口径：
//! SMTP 的 `AUTH` 会把口令明文发出去。支持 `Tls`（隐式，465）与 `StartTls`（587 / 25）；
//! `StartTls` 档若服务端不声明 `STARTTLS`，**拒绝降级**而不是明文发信。
//!
//! 信任链与 IMAP 共用同一份 `rustls` 配置（[`tls_config`]），全仓一份根集。

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{MailError, Result};
use crate::models::{AuthKind, ConnectionSecurity};
use crate::oauth::sasl_xoauth2_base64;
use crate::proto::imap::tls_config;

/// 会话底下的字节流。
///
/// 之所以需要这个 trait：会话在 TLS 之后持有的具体类型是
/// `tokio_rustls::client::TlsStream<TcpStream>`，而它是 `pub(crate)` 的实现细节——
/// 装箱成 `Box<dyn Transport>` 之后，[`SmtpSession`] 的字段类型不必暴露 `tokio` 派生
/// 类型（「tokio 不外泄」这条纪律在 crate 内的延续）。
///
/// `Unpin` 与 `Send` 是超级约束：前者是 `BufReader` 与 `AsyncReadExt` 的要求，
/// 后者是因为会话要被搬到自管的 runtime 上跑。
trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> Transport for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// SMTP 建连参数。**含口令**——只在建连那一瞬间存在于内存里。
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// 主机。
    pub host: String,
    /// 端口（隐式 TLS 常见 465，STARTTLS 常见 587 / 25）。
    pub port: u16,
    /// 加密方式；[`ConnectionSecurity::Plain`] 会被拒绝。
    pub security: ConnectionSecurity,
    /// 登录名。**留空则不发 `AUTH`**（给不需要认证的中继用）。
    pub username: String,
    /// 凭据。口令档是口令，[`AuthKind::OAuth2`] 档是**访问令牌**。
    pub password: String,
    /// 认证方式：决定走 `AUTH PLAIN` / `AUTH LOGIN` 还是 `AUTH XOAUTH2`。
    pub auth: AuthKind,
    /// `EHLO` 里自报的域名。空值会回落到 `localhost`。
    pub client_name: String,
}

impl SmtpConfig {
    /// 建连 / 握手的超时。
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// 单条命令的超时。比 IMAP 的宽：`DATA` 之后要传完整封信，
    /// 大附件（下一批）会让这一步明显变慢。
    pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(180);
}

/// 服务端在 `EHLO` 里声明的能力位。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpCaps {
    /// 支持 `STARTTLS`。
    pub starttls: bool,
    /// 支持 `AUTH PLAIN`。
    pub auth_plain: bool,
    /// 支持 `AUTH LOGIN`。
    pub auth_login: bool,
    /// 支持 `AUTH XOAUTH2`（OAuth2 账户用它，见 [`crate::oauth`]）。
    pub auth_xoauth2: bool,
    /// 支持 `8BITMIME`（本批次发的是 UTF-8 正文，靠它才不用做 quoted-printable）。
    pub eight_bit_mime: bool,
    /// 服务端接受的报文上限（字节）。`None` = 未声明或声明为 0（= 不限制）。
    pub size_limit: Option<u64>,
    /// 支持增强状态码（`2.1.0` 这类前缀）。
    pub enhanced_status: bool,
    /// 支持 `PIPELINING`（本批次不用，但值得记下来）。
    pub pipelining: bool,
}

/// 一条（可能多行的）服务端响应。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    /// 状态码（取最后一行的；RFC 5321 要求多行响应各行同码）。
    code: u16,
    /// 各行的正文（已去掉状态码与前导分隔符）。
    lines: Vec<String>,
}

/// 一条已连上（且多半已认证）的 SMTP 会话。
pub struct SmtpSession {
    io: BufReader<Box<dyn Transport>>,
    caps: SmtpCaps,
}

impl SmtpSession {
    /// 建连（含可选的 STARTTLS 升级）并按需认证。
    pub async fn connect(cfg: &SmtpConfig) -> Result<Self> {
        if cfg.security == ConnectionSecurity::Plain {
            return Err(MailError::Unsupported(
                "cleartext SMTP is not wired up in this build; use TLS or STARTTLS".into(),
            ));
        }
        let host = cfg.host.trim();
        if host.is_empty() {
            return Err(MailError::Config("smtp host is empty".into()));
        }

        let tcp = tokio::time::timeout(
            SmtpConfig::CONNECT_TIMEOUT,
            TcpStream::connect((host, cfg.port)),
        )
        .await
        .map_err(|_| MailError::Timeout(format!("tcp connect to {host}:{}", cfg.port)))?
        .map_err(|e| MailError::Connect(format!("{host}:{}: {e}", cfg.port)))?;
        let _ = tcp.set_nodelay(true);

        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|e| MailError::Connect(format!("invalid host {host}: {e}")))?;
        let connector = TlsConnector::from(tls_config());

        // 隐式 TLS：先握手，再读问候语。
        let (mut io, caps) = if cfg.security == ConnectionSecurity::Tls {
            let tls = upgrade(connector.clone(), server_name.clone(), tcp).await?;
            let mut io = reader(tls);
            expect_code(
                &read_reply_with_timeout(&mut io, "greeting").await?,
                &[220],
                "greeting",
            )?;
            let caps = ehlo(&mut io, &cfg.client_name).await?;
            (io, caps)
        } else {
            // STARTTLS：先在明文上读问候语与能力位。
            let mut plain: BufReader<TcpStream> = BufReader::new(tcp);
            expect_code(
                &read_reply_with_timeout(&mut plain, "greeting").await?,
                &[220],
                "greeting",
            )?;
            let announced = ehlo(&mut plain, &cfg.client_name).await?;
            if !announced.starttls {
                return Err(MailError::Unsupported(
                    "the server does not advertise STARTTLS; refusing to send credentials in the clear"
                        .into(),
                ));
            }
            command(&mut plain, "STARTTLS", &[220], "STARTTLS").await?;
            // ⚠️ 交接：RFC 3207 禁止服务端在 `220` 之后继续发明文数据。
            // `into_inner()` 会**丢弃**缓冲里剩下的字节，所以先断言它是空的——
            // 真有残留说明对端违约，此时静默丢掉会让我们读到错位的 TLS 记录。
            if !plain.buffer().is_empty() {
                return Err(MailError::Protocol(
                    "server sent plaintext bytes after the STARTTLS reply".into(),
                ));
            }
            let tls = upgrade(connector.clone(), server_name.clone(), plain.into_inner()).await?;
            let mut io = reader(tls);
            // STARTTLS 之后必须重新 `EHLO`（RFC 3207），能力位以这一次为准。
            let caps = ehlo(&mut io, &cfg.client_name).await?;
            (io, caps)
        };

        if !cfg.username.is_empty() {
            authenticate(&mut io, &caps, &cfg.username, &cfg.password, cfg.auth).await?;
        }
        tracing::debug!(caps = ?caps, "smtp session ready");

        Ok(Self { io, caps })
    }

    /// 服务端能力位（建连时探测一次）。
    #[must_use]
    pub fn caps(&self) -> &SmtpCaps {
        &self.caps
    }

    /// 发一封信。
    ///
    /// `from` 是**信封发件人**（`MAIL FROM`，裸地址不带尖括号——本方法自己补）；
    /// `recipients` 是**信封收件人**（`RCPT TO`，应来自
    /// [`crate::compose::envelope_recipients`]，即含密送）；`message` 是
    /// [`crate::compose::build_message`] 装好的整封（含头部）。
    pub async fn send(&mut self, from: &str, recipients: &[String], message: &str) -> Result<()> {
        if recipients.is_empty() {
            return Err(MailError::Config("no recipients to send to".into()));
        }
        let from = from.trim();
        if from.is_empty() {
            return Err(MailError::Config("envelope sender is empty".into()));
        }

        // 服务端声明了上限就**先拒**：与其把几 MB 推上去再收 552，不如在本地报错。
        // 用 `filter` 而非嵌套 `if let`（本项目 MSRV 1.85 没有 let-chain）。
        let size = message.len() as u64;
        if let Some(limit) = self.caps.size_limit.filter(|limit| size > *limit) {
            return Err(MailError::Config(format!(
                "the message is {size} bytes; the server accepts at most {limit}"
            )));
        }

        // `SIZE=` 只在服务端声明 `SIZE` 时才带（没声明的服务端会把未知参数当语法错）。
        let mail_from = if self.caps.size_limit.is_some() {
            format!("MAIL FROM:<{from}> SIZE={size}")
        } else {
            format!("MAIL FROM:<{from}>")
        };
        command(&mut self.io, &mail_from, &[250], "MAIL FROM").await?;

        for recipient in recipients {
            let recipient = recipient.trim();
            if recipient.is_empty() {
                continue;
            }
            // 251 是「不是本地用户，但会转发」，属于成功。
            command(
                &mut self.io,
                &format!("RCPT TO:<{recipient}>"),
                &[250, 251],
                "RCPT TO",
            )
            .await?;
        }

        command(&mut self.io, "DATA", &[354], "DATA").await?;
        let payload = smtp_data_payload(message);
        self.io.write_all(payload.as_bytes()).await?;
        self.io.flush().await?;
        expect_code(
            &read_reply_with_timeout(&mut self.io, "message body").await?,
            &[250],
            "message body",
        )
    }

    /// 道别。**不留返回值**：连接即将关闭，`QUIT` 的响应没有可用的后续动作，
    /// 失败也只记一条 debug——把「关连接失败」当错误上报只会制造噪音。
    pub async fn quit(mut self) {
        if let Err(error) = write_line(&mut self.io, "QUIT").await {
            tracing::debug!(%error, "smtp QUIT could not be written");
            return;
        }
        match read_reply_with_timeout(&mut self.io, "QUIT").await {
            Ok(reply) if reply.code == 221 => {}
            Ok(reply) => tracing::debug!(
                code = reply.code,
                "smtp server did not answer QUIT with 221"
            ),
            Err(error) => tracing::debug!(%error, "smtp QUIT had no clean reply"),
        }
    }
}

/// `DATA` 阶段的报文体：CRLF 规整 + 点填充 + 结束行。
///
/// 点填充是**必须**的：正文里以 `.` 开头的一行会被服务端当成「报文结束」。
/// 规则是行首的单个 `.` 变成两个（`\r\n.` → `\r\n..`），正文首字符是 `.` 时同样要填。
/// 结尾必须是 `\r\n.\r\n`——漏掉那个 CRLF 会让最后一行与服务端认的结束符粘连。
#[must_use]
pub fn smtp_data_payload(message: &str) -> String {
    let normalized = crate::compose::normalize_crlf(message);
    let mut out = String::with_capacity(normalized.len() + 8);
    let mut at_line_start = true;
    for ch in normalized.chars() {
        if at_line_start && ch == '.' {
            out.push('.');
        }
        out.push(ch);
        at_line_start = ch == '\n';
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// 把明文流换成 TLS，并**再套一层缓冲**给后续的按行读。
fn reader(tls: tokio_rustls::client::TlsStream<TcpStream>) -> BufReader<Box<dyn Transport>> {
    BufReader::new(Box::new(tls) as Box<dyn Transport>)
}

/// TLS 握手（两处调用：隐式 TLS 与 STARTTLS），超时与错误口径一致。
async fn upgrade(
    connector: TlsConnector,
    server_name: rustls::pki_types::ServerName<'static>,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    tokio::time::timeout(
        SmtpConfig::CONNECT_TIMEOUT,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|_| MailError::Timeout("tls handshake".into()))?
    .map_err(|e| MailError::Connect(format!("tls handshake failed: {e}")))
}

/// 读一条响应，带命令超时。
async fn read_reply_with_timeout<S>(io: &mut BufReader<S>, label: &str) -> Result<Reply>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(SmtpConfig::COMMAND_TIMEOUT, read_reply(io)).await {
        Ok(result) => result,
        Err(_) => Err(MailError::Timeout(format!("smtp {label}"))),
    }
}

/// 读一条（可能多行的）响应。
///
/// 多行响应的续行规则：第 4 个字符是 `-` 表示还有下一行，是空格表示结束。
/// 状态码**按字节**取前 3 位——服务端理论上可能回非 ASCII 文本，用字符串切片
/// 有 panic 风险。
async fn read_reply<S>(io: &mut BufReader<S>) -> Result<Reply>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut lines = Vec::new();
    // 不给初值：`unused_assignments` 会把「初值立刻被覆盖」当死代码报警，
    // 而延迟初始化（`let mut code;`）既满足「循环后一定有值」又不会报警。
    let mut code;
    loop {
        let mut raw = String::new();
        let read = io.read_line(&mut raw).await?;
        if read == 0 {
            return Err(MailError::Connect(
                "the smtp server closed the connection".into(),
            ));
        }
        let line = raw.trim_end_matches(['\r', '\n']);
        let bytes = line.as_bytes();
        if bytes.len() < 3 {
            return Err(MailError::Protocol(format!(
                "malformed smtp reply: {line:?}"
            )));
        }
        code = std::str::from_utf8(&bytes[..3])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                MailError::Protocol(format!("smtp reply has no status code: {line:?}"))
            })?;
        let more = bytes.get(3) == Some(&b'-');
        lines.push(
            std::str::from_utf8(&bytes[4.min(bytes.len())..])
                .unwrap_or_default()
                .to_owned(),
        );
        if !more {
            break;
        }
        if lines.len() > 64 {
            return Err(MailError::Protocol("smtp reply is implausibly long".into()));
        }
    }
    Ok(Reply { code, lines })
}

/// 状态码不符合预期就报错，并把「哪种错」分好类（认证 / 连接 / 协议）。
fn expect_code(reply: &Reply, expected: &[u16], label: &str) -> Result<()> {
    if expected.contains(&reply.code) {
        return Ok(());
    }
    let detail = reply.lines.last().cloned().unwrap_or_default();
    Err(match reply.code {
        // 421 是「服务不可用，正在关闭连接」——属于连接层，值得重试。
        421 => MailError::Connect(format!("smtp {label}: server is closing: {detail}")),
        530 | 534 | 535 | 538 => MailError::Auth(format!("smtp {label}: {detail}")),
        code => MailError::Protocol(format!("smtp {label}: unexpected status {code} ({detail})")),
    })
}

/// 写一行（自动补 CRLF 并 flush）。
async fn write_line<S>(io: &mut BufReader<S>, line: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    io.write_all(line.as_bytes()).await?;
    io.write_all(b"\r\n").await?;
    io.flush().await?;
    Ok(())
}

/// 发一条命令并校验响应码。
async fn command<S>(
    io: &mut BufReader<S>,
    line: &str,
    expected: &[u16],
    label: &str,
) -> Result<Reply>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_line(io, line).await?;
    let reply = read_reply_with_timeout(io, label).await?;
    expect_code(&reply, expected, label)?;
    Ok(reply)
}

/// `EHLO`，失败时按需回退 `HELO`。
///
/// 回退只针对 **5xx**：那说明服务端不认 `EHLO`（RFC 5321 之前的实现）。
/// 4xx 是临时拒绝（如反垃圾限流），回退只会把问题掩盖成「能力位为空」，
/// 所以直接报错。
async fn ehlo<S>(io: &mut BufReader<S>, client_name: &str) -> Result<SmtpCaps>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = match client_name.trim() {
        "" => "localhost",
        trimmed => trimmed,
    };
    write_line(io, &format!("EHLO {name}")).await?;
    let reply = read_reply_with_timeout(io, "EHLO").await?;
    if reply.code == 250 {
        return Ok(parse_caps(&reply.lines));
    }
    if !(500..600).contains(&reply.code) {
        expect_code(&reply, &[250], "EHLO")?;
    }
    // 老服务器：`HELO` 没有扩展，能力位只能全空（于是 STARTTLS 也拿不到，
    // 那种服务器上 STARTTLS 档会正确地拒绝发信）。
    command(io, &format!("HELO {name}"), &[250], "HELO").await?;
    Ok(SmtpCaps::default())
}

/// 解析 `EHLO` 的能力位列表。
///
/// 第一条是服务端的域名（不是能力），要跳过。`AUTH` 有两种历史写法：
/// `AUTH PLAIN LOGIN`（现代）与 `AUTH=PLAIN LOGIN`（老式，RFC 4954 之前）。
fn parse_caps(lines: &[String]) -> SmtpCaps {
    let mut caps = SmtpCaps::default();
    for line in lines.iter().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        let upper = keyword.to_ascii_uppercase();
        match upper.as_str() {
            "STARTTLS" => caps.starttls = true,
            "8BITMIME" => caps.eight_bit_mime = true,
            "ENHANCEDSTATUSCODES" => caps.enhanced_status = true,
            "PIPELINING" => caps.pipelining = true,
            // `SIZE 0` 按 RFC 1870 是「不限制」，所以归一到 `None`，
            // 免得后面拿 0 去比大小把每封信都拒了。
            "SIZE" => {
                caps.size_limit = parts
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|limit| *limit > 0);
            }
            _ => {}
        }
        // `AUTH PLAIN LOGIN` 与 `AUTH=PLAIN LOGIN` 的区别只在第一个 token 里。
        let mechanisms = if upper == "AUTH" {
            parts
                .map(|part| part.to_ascii_uppercase())
                .collect::<Vec<_>>()
        } else if let Some(rest) = upper.strip_prefix("AUTH=") {
            let mut all = vec![rest.to_owned()];
            all.extend(parts.map(|part| part.to_ascii_uppercase()));
            all
        } else {
            continue;
        };
        for mechanism in mechanisms {
            match mechanism.as_str() {
                "PLAIN" => caps.auth_plain = true,
                "LOGIN" => caps.auth_login = true,
                "XOAUTH2" => caps.auth_xoauth2 = true,
                _ => {}
            }
        }
    }
    caps
}

/// 认证。
///
/// OAuth2 账户走 `XOAUTH2`（`password` 是访问令牌）；口令账户优先 `PLAIN`（一次往返），
/// 否则 `LOGIN`（三次往返）。
///
/// 两条口径：
/// - OAuth2 账户**绝不**回落到口令机制 —— 把访问令牌当口令发出去只会失败，
///   而且服务端的报错看不出根因。服务端没声明 `XOAUTH2` 就明确报「不支持」。
/// - 口令账户两种机制都没有就**报错**而不是裸发：有的中继允许无认证发信，但那是
///   [`SmtpConfig::username`] 留空时走的分支，不该在这里静默退化。
async fn authenticate<S>(
    io: &mut BufReader<S>,
    caps: &SmtpCaps,
    username: &str,
    password: &str,
    auth: AuthKind,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if auth == AuthKind::OAuth2 {
        if !caps.auth_xoauth2 {
            return Err(MailError::Unsupported(
                "the server does not advertise XOAUTH2; an oauth2 account cannot authenticate here"
                    .into(),
            ));
        }
        let token = sasl_xoauth2_base64(username, password);
        command(io, &format!("AUTH XOAUTH2 {token}"), &[235], "AUTH XOAUTH2").await?;
        return Ok(());
    }
    if caps.auth_plain {
        let token = base64_encode(format!("\0{username}\0{password}").as_bytes());
        command(io, &format!("AUTH PLAIN {token}"), &[235], "AUTH PLAIN").await?;
        return Ok(());
    }
    if caps.auth_login {
        command(io, "AUTH LOGIN", &[334], "AUTH LOGIN").await?;
        command(
            io,
            &base64_encode(username.as_bytes()),
            &[334],
            "AUTH LOGIN username",
        )
        .await?;
        command(
            io,
            &base64_encode(password.as_bytes()),
            &[235],
            "AUTH LOGIN password",
        )
        .await?;
        return Ok(());
    }
    Err(MailError::Unsupported(
        "the server advertises no supported AUTH mechanism (need PLAIN or LOGIN)".into(),
    ))
}

/// base64（与 `mime.rs` 同一实现来源，避免两份口径）。
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// 假服务端对 `EHLO` 的标准应答：**多行**（`250-` 续行、最后一行用空格），
    /// 顺带把「能力位声明」的解析口径钉在真实报文形状上。
    const EHLO_REPLY: &str = "250-fake.example.com\r\n250-SIZE 35882577\r\n250-STARTTLS\r\n\
                              250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 PIPELINING\r\n";

    /// 一条应答规则。
    enum Rule {
        /// 收到以 `prefix` 开头的行 → 回 `reply`。
        Reply {
            prefix: &'static str,
            reply: &'static str,
        },
        /// 读掉报文直到单独一行 `.`，然后回 `reply`。
        Body { reply: &'static str },
    }

    /// 标准 `EHLO` 规则（多数用例都以它开头）。
    fn ehlo_rule() -> Rule {
        Rule::Reply {
            prefix: "EHLO ",
            reply: EHLO_REPLY,
        }
    }

    /// 从假服务端侧读一行；对端提前断开返回 `None`（**不是** panic——用例提前
    /// 返回时连接会被丢掉，那是正常路径，不该在任务里造一次假失败）。
    async fn next_line(io: &mut BufReader<TcpStream>) -> Option<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(5), io.read_line(&mut line))
            .await
            .expect("客户端应在 5 秒内发下一行")
            .expect("读取不应失败");
        (read > 0).then(|| line.trim_end_matches(['\r', '\n']).to_owned())
    }

    async fn send_all(io: &mut BufReader<TcpStream>, text: &str) {
        io.write_all(text.as_bytes()).await.expect("write");
        io.flush().await.expect("flush");
    }

    /// 脚本化服务端：按规则逐条应答，返回**被规则消费掉的**行（去掉行尾 CRLF）。
    ///
    /// 注意返回的是「消费掉的」而不是「收到的」：规则跑完就收工，之后的 `QUIT`
    /// 一类的行不会被记录。断言时按这个口径写。
    async fn serve(listener: TcpListener, rules: Vec<Rule>) -> Vec<String> {
        let (tcp, _) = listener.accept().await.expect("accept");
        let _ = tcp.set_nodelay(true);
        let mut io = BufReader::new(tcp);
        let mut seen: Vec<String> = Vec::new();
        send_all(&mut io, "220 fake ready\r\n").await;

        for rule in rules {
            match rule {
                Rule::Reply { prefix, reply } => {
                    let Some(line) = next_line(&mut io).await else {
                        return seen;
                    };
                    assert!(
                        line.starts_with(prefix),
                        "期待以 {prefix:?} 开头的行，实得 {line:?}"
                    );
                    seen.push(line);
                    send_all(&mut io, reply).await;
                }
                Rule::Body { reply } => {
                    loop {
                        let Some(line) = next_line(&mut io).await else {
                            return seen;
                        };
                        let done = line == ".";
                        seen.push(line);
                        if done {
                            break;
                        }
                    }
                    send_all(&mut io, reply).await;
                }
            }
        }
        seen
    }

    /// 起一个脚本化服务端并连上它，返回 (会话, 服务端任务)。
    ///
    /// ⚠️ `rules` 的**第一条要留给 `EHLO`**（用 [`ehlo_rule`]）：本函数读完问候语
    /// 之后会真的走一次 `EHLO`，那一步正是被第一条规则应答的。
    async fn session_with_server(
        rules: Vec<Rule>,
    ) -> (SmtpSession, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(serve(listener, rules));

        let tcp = TcpStream::connect(addr).await.expect("connect");
        let _ = tcp.set_nodelay(true);
        // 直接构造会话，**不走** `connect`：`connect` 会强制 TLS，而 TLS 握手那条
        // 路径与 IMAP 共用同一份实现、已被验证。这里要测的是**对话逻辑本身**。
        let mut io: BufReader<Box<dyn Transport>> = BufReader::new(Box::new(tcp));
        expect_code(
            &read_reply_with_timeout(&mut io, "greeting")
                .await
                .expect("greeting"),
            &[220],
            "greeting",
        )
        .expect("问候语应是 220");
        let caps = ehlo(&mut io, "test.local").await.expect("EHLO");
        (SmtpSession { io, caps }, server)
    }

    /// `DATA` 阶段的点填充与结束行。
    #[test]
    fn data_payload_stuffs_leading_dots_and_terminates() {
        assert_eq!(
            smtp_data_payload("line one\n.hidden\nline two"),
            "line one\r\n..hidden\r\nline two\r\n.\r\n"
        );
        // 正文首个字符就是 `.` 也要填。
        assert_eq!(smtp_data_payload(".x"), "..x\r\n.\r\n");
        // 已经以 CRLF 结尾时不重复补空行。
        assert_eq!(smtp_data_payload("a\r\n"), "a\r\n.\r\n");
        // 行中间的 `.` 不动。
        assert_eq!(smtp_data_payload("a.b"), "a.b\r\n.\r\n");
        // `\r\n.\r\n` 是唯一的结束符，所以 `..` 行不会被误判成结束。
        assert!(smtp_data_payload(".\n.\n").ends_with("..\r\n..\r\n.\r\n"));
    }

    /// 能力位解析：`AUTH` 两种历史写法、`SIZE 0` 归一到「不限制」、未知扩展不影响已知项。
    #[test]
    fn capability_parsing_handles_both_auth_spellings() {
        let lines = |raw: &[&str]| raw.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        let modern = parse_caps(&lines(&[
            "mail.example.com at your service",
            "SIZE 35882577",
            "8BITMIME",
            "STARTTLS",
            "AUTH PLAIN LOGIN",
            "ENHANCEDSTATUSCODES",
            "PIPELINING",
        ]));
        assert_eq!(
            modern,
            SmtpCaps {
                starttls: true,
                auth_plain: true,
                auth_login: true,
                auth_xoauth2: false,
                eight_bit_mime: true,
                size_limit: Some(35_882_577),
                enhanced_status: true,
                pipelining: true,
            }
        );

        // 老式 `AUTH=` 写法（RFC 4954 之前）。
        let legacy = parse_caps(&lines(&["host", "AUTH=PLAIN LOGIN"]));
        assert!(legacy.auth_plain && legacy.auth_login);
        assert!(!legacy.starttls);

        // `SIZE 0` 按 RFC 1870 是「不限制」，必须归一到 `None`——否则后面拿 0 比大小
        // 会把每一封信都拒掉。
        assert_eq!(parse_caps(&lines(&["host", "SIZE 0"])).size_limit, None);
        // 只有 `SIZE` 没有数值：同样按「未知」处理。
        assert_eq!(parse_caps(&lines(&["host", "SIZE"])).size_limit, None);
        // 不认识的扩展被忽略。
        assert_eq!(
            parse_caps(&lines(&["host", "XWEIRD=1 2", "DSN"])),
            SmtpCaps::default()
        );
        // 只有第一条（服务端域名）时，能力位全空。
        assert_eq!(parse_caps(&lines(&["host"])), SmtpCaps::default());
        assert_eq!(parse_caps(&[]), SmtpCaps::default());
    }

    /// 状态码分类：认证类 / 连接类 / 其它协议错，并据此决定「值不值得重试」。
    #[test]
    fn unexpected_status_codes_are_classified() {
        let reply = |code: u16, text: &str| Reply {
            code,
            lines: vec![text.to_owned()],
        };
        assert!(expect_code(&reply(250, "ok"), &[250], "MAIL FROM").is_ok());
        assert!(expect_code(&reply(251, "forward"), &[250, 251], "RCPT TO").is_ok());

        let closing = expect_code(&reply(421, "too many connections"), &[250], "x").unwrap_err();
        assert_eq!(closing.code(), "connect", "421 是连接层，值得重试");
        assert!(closing.is_retryable());

        let auth = expect_code(&reply(535, "bad credentials"), &[250], "AUTH").unwrap_err();
        assert_eq!(auth.code(), "auth");
        assert!(!auth.is_retryable(), "口令错重试多少次都一样");

        let protocol = expect_code(&reply(552, "message too big"), &[250], "DATA").unwrap_err();
        assert_eq!(protocol.code(), "protocol");
        assert!(protocol.to_string().contains("552"), "{protocol}");
        assert!(
            protocol.to_string().contains("message too big"),
            "{protocol}"
        );
    }

    /// 真对话（走本地假服务端）：`EHLO` → `AUTH PLAIN` → `MAIL`/`RCPT`/`DATA` → 正文 → `QUIT`。
    #[tokio::test]
    async fn full_dialogue_sends_the_message_and_stuffs_dots() {
        let message = "Subject: hi\r\n\r\nline one\r\n.hidden\r\n";
        let (mut session, server) = session_with_server(vec![
            ehlo_rule(),
            Rule::Reply {
                prefix: "AUTH PLAIN ",
                reply: "235 2.7.0 accepted\r\n",
            },
            Rule::Reply {
                prefix: "MAIL FROM:<alice@example.com>",
                reply: "250 2.1.0 ok\r\n",
            },
            Rule::Reply {
                prefix: "RCPT TO:<bob@example.com>",
                reply: "250 2.1.5 ok\r\n",
            },
            Rule::Reply {
                prefix: "DATA",
                reply: "354 end with <CRLF>.<CRLF>\r\n",
            },
            Rule::Body {
                reply: "250 2.0.0 queued as 12345\r\n",
            },
            Rule::Reply {
                prefix: "QUIT",
                reply: "221 2.0.0 bye\r\n",
            },
        ])
        .await;
        // 多行 EHLO 应答应被完整解析出来。
        assert_eq!(
            session.caps(),
            &SmtpCaps {
                starttls: true,
                auth_plain: true,
                auth_login: true,
                auth_xoauth2: false,
                eight_bit_mime: true,
                size_limit: Some(35_882_577),
                enhanced_status: false,
                pipelining: true,
            }
        );

        // 会话是直接构造的（没有走 `connect` 的认证分支），所以先手动认证一次。
        authenticate(
            &mut session.io,
            &session.caps,
            "alice@example.com",
            "secret",
            AuthKind::Password,
        )
        .await
        .expect("AUTH PLAIN");
        session
            .send(
                "alice@example.com",
                &["bob@example.com".to_owned()],
                message,
            )
            .await
            .expect("send");
        session.quit().await;

        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec![
                "EHLO test.local".to_owned(),
                // PLAIN 的载荷是 `authzid \0 authcid \0 passwd`，authzid 留空。
                "AUTH PLAIN AGFsaWNlQGV4YW1wbGUuY29tAHNlY3JldA==".to_owned(),
                // 服务端声明了 SIZE，所以信封带 `SIZE=<字节数>`。
                format!("MAIL FROM:<alice@example.com> SIZE={}", message.len()),
                "RCPT TO:<bob@example.com>".to_owned(),
                "DATA".to_owned(),
                // 正文：行首 `.` 已填充，以单行 `.` 结束。
                "Subject: hi".to_owned(),
                String::new(),
                "line one".to_owned(),
                "..hidden".to_owned(),
                ".".to_owned(),
                "QUIT".to_owned(),
            ]
        );
    }

    /// 服务端只认 `HELO`（对 `EHLO` 回 5xx）时回退，且回退后能力位为空。
    #[tokio::test]
    async fn ehlo_falls_back_to_helo_on_a_5xx() {
        let (session, server) = session_with_server(vec![
            Rule::Reply {
                prefix: "EHLO ",
                reply: "502 command not implemented\r\n",
            },
            Rule::Reply {
                prefix: "HELO ",
                reply: "250 hello\r\n",
            },
        ])
        .await;
        assert_eq!(
            session.caps(),
            &SmtpCaps::default(),
            "HELO 没有扩展，能力位只能全空"
        );
        session.quit().await;
        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec!["EHLO test.local".to_owned(), "HELO test.local".to_owned()]
        );
    }

    /// 一个收件人都没有时**不发任何信封信令**（否则会给服务端留一个半开的信封）。
    #[tokio::test]
    async fn send_without_recipients_is_rejected_before_any_command() {
        let (mut session, server) = session_with_server(vec![ehlo_rule()]).await;
        let error = session
            .send("alice@example.com", &[], "Subject: x\r\n\r\nbody")
            .await
            .expect_err("空收件人必须被拒");
        assert_eq!(error.code(), "config");
        session.quit().await;

        // `QUIT` 没有被任何规则消费，所以不会出现在 `seen` 里。
        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec!["EHLO test.local".to_owned()],
            "被拒的那次不该在协议上留下任何信封痕迹"
        );
    }

    /// 超出服务端声明的 `SIZE` 时在**本地**拒发（不推上去再收 552）。
    #[tokio::test]
    async fn oversized_message_is_rejected_locally() {
        let (mut session, server) = session_with_server(vec![ehlo_rule()]).await;
        assert_eq!(session.caps().size_limit, Some(35_882_577));
        // 真造一封 34 MB 的信太浪费，所以把上限压到 4 字节后跑同一段逻辑。
        session.caps.size_limit = Some(4);
        let error = session
            .send(
                "alice@example.com",
                &["bob@example.com".to_owned()],
                "way too long",
            )
            .await
            .expect_err("超限必须被拒");
        assert_eq!(error.code(), "config");
        assert!(error.to_string().contains("accepts at most 4"), "{error}");
        session.quit().await;

        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec!["EHLO test.local".to_owned()],
            "被拒的那次不该发出 MAIL FROM"
        );
    }

    /// 只声明 `AUTH LOGIN` 时走三步往返；一个机制都没有时明确报「不支持」。
    #[tokio::test]
    async fn auth_login_is_used_when_plain_is_absent() {
        let (mut session, server) = session_with_server(vec![
            Rule::Reply {
                prefix: "EHLO ",
                reply: "250-fake.example.com\r\n250-AUTH LOGIN\r\n250 OK\r\n",
            },
            Rule::Reply {
                prefix: "AUTH LOGIN",
                reply: "334 VXNlcm5hbWU6\r\n",
            },
            Rule::Reply {
                prefix: "YWxpY2VAZXhhbXBsZS5jb20=",
                reply: "334 UGFzc3dvcmQ6\r\n",
            },
            Rule::Reply {
                prefix: "c2VjcmV0",
                reply: "235 2.7.0 accepted\r\n",
            },
            Rule::Reply {
                prefix: "QUIT",
                reply: "221 bye\r\n",
            },
        ])
        .await;
        assert!(session.caps().auth_login && !session.caps().auth_plain);
        authenticate(
            &mut session.io,
            &session.caps,
            "alice@example.com",
            "secret",
            AuthKind::Password,
        )
        .await
        .expect("AUTH LOGIN 三步");
        session.quit().await;

        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec![
                "EHLO test.local".to_owned(),
                "AUTH LOGIN".to_owned(),
                // base64("alice@example.com") / base64("secret")，由外部权威实现算出。
                "YWxpY2VAZXhhbXBsZS5jb20=".to_owned(),
                "c2VjcmV0".to_owned(),
                "QUIT".to_owned(),
            ]
        );
    }

    /// OAuth2 账户走 `AUTH XOAUTH2`：SASL 初始响应的明文形状由本测试**独立**钉住
    /// （不调用被测的 `sasl_xoauth2_base64`，免得自证自明）。
    #[tokio::test]
    async fn oauth2_accounts_authenticate_with_xoauth2() {
        use base64::Engine as _;

        let (mut session, server) = session_with_server(vec![
            Rule::Reply {
                prefix: "EHLO ",
                reply: "250-fake.example.com\r\n250-AUTH XOAUTH2 PLAIN\r\n250 OK\r\n",
            },
            Rule::Reply {
                prefix: "AUTH XOAUTH2 ",
                reply: "235 2.7.0 accepted\r\n",
            },
            Rule::Reply {
                prefix: "QUIT",
                reply: "221 bye\r\n",
            },
        ])
        .await;
        assert!(session.caps().auth_xoauth2 && session.caps().auth_plain);
        authenticate(
            &mut session.io,
            &session.caps,
            "alice@example.com",
            "token-1",
            AuthKind::OAuth2,
        )
        .await
        .expect("AUTH XOAUTH2");
        session.quit().await;

        // `^A` = U+0001；明文与 base64 都在这里独立写出。
        let expected_plain = "user=alice@example.com\u{1}auth=Bearer token-1\u{1}\u{1}";
        let expected_line = format!(
            "AUTH XOAUTH2 {}",
            base64::engine::general_purpose::STANDARD.encode(expected_plain)
        );
        let seen = server.await.expect("服务端任务不应 panic");
        assert_eq!(
            seen,
            vec![
                "EHLO test.local".to_owned(),
                expected_line,
                "QUIT".to_owned(),
            ]
        );
    }

    /// OAuth2 账户遇到不声明 `XOAUTH2` 的服务端：明确报「不支持」，
    /// **绝不**回落到 `AUTH PLAIN`（把访问令牌当口令发出去只会得到看不懂的失败）。
    #[tokio::test]
    async fn oauth2_never_falls_back_to_password_mechanisms() {
        let mut io: BufReader<Box<dyn Transport>> = BufReader::new(Box::new(DeadStream));
        let caps = SmtpCaps {
            auth_plain: true,
            auth_login: true,
            ..SmtpCaps::default()
        };
        let error = authenticate(
            &mut io,
            &caps,
            "alice@example.com",
            "token-1",
            AuthKind::OAuth2,
        )
        .await
        .expect_err("没有 XOAUTH2 必须报错");
        assert_eq!(error.code(), "unsupported");
        assert!(error.to_string().contains("XOAUTH2"), "{error}");
    }

    /// 一个机制都不声明时：报 `Unsupported`，**绝不**裸发口令。
    #[tokio::test]
    async fn missing_auth_mechanisms_are_reported_without_touching_the_socket() {
        // 这个「流」什么都不做：能过就证明这条分支根本没写 socket。
        let mut io: BufReader<Box<dyn Transport>> = BufReader::new(Box::new(DeadStream));
        let error = authenticate(&mut io, &SmtpCaps::default(), "u", "p", AuthKind::Password)
            .await
            .expect_err("没有可用机制必须报错");
        assert_eq!(error.code(), "unsupported");
        assert!(error.to_string().contains("AUTH"), "{error}");
    }

    /// 只用来验证「没有可用 AUTH 机制」这条分支不会去碰 IO。
    struct DeadStream;

    impl AsyncRead for DeadStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for DeadStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
}
