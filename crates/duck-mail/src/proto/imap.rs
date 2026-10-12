//! `async-imap` 薄封装。
//!
//! 同步引擎要的每件事都走 [`ImapSession`]，**`async-imap` 的 API 变动被关在这个
//! 文件里**（照 `flectar-mail-core/src/imap/mod.rs` 的组织方式）。
//!
//! v1 有意保持协议面小：不做 CONDSTORE / QRESYNC —— 增量用「新 UID 拉取 +
//! 窗口内旗标比对 + UID 集合对账」实现，这在**每一台**服务器上都正确
//! （`docs/email-design.md` §4.6）。

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_imap::Session;
use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{MailError, Result};
use crate::models::{ConnectionSecurity, ConnectionTestResult};
use crate::proto::ImapConfig;

/// 加密后的 IMAP 流。
pub type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
/// 明文流（仅本地测试服务器）。
pub type PlainStream = TcpStream;

/// 一条已登录的 IMAP 会话（TLS）。
pub struct ImapSession {
    session: Session<TlsStream>,
}

/// `LIST` 返回的一个文件夹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFolder {
    /// 原始名字。
    pub name: String,
    /// 层级分隔符。
    pub delimiter: Option<String>,
    /// `LIST` 属性（`\Sent` 等）。
    pub attributes: Vec<String>,
    /// 服务端是否声明它可选（`\Noselect` 的容器不该出现在界面里）。
    pub selectable: bool,
}

/// `SELECT` 之后的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectedState {
    /// UIDVALIDITY（变了说明本地缓存整体作废）。
    pub uid_validity: Option<u32>,
    /// UIDNEXT（水位上界）。
    pub uid_next: Option<u32>,
    /// 邮件总数。
    pub exists: u32,
}

/// `FETCH` 回来的一条信封（这里只拉了头部 + 旗标，正文按需再拉）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEnvelope {
    /// IMAP UID。
    pub uid: u32,
    /// 是否 `\Seen`。
    pub seen: bool,
    /// 是否 `\Flagged`。
    pub flagged: bool,
    /// `RFC822.SIZE`。
    pub size: Option<u32>,
    /// `BODY.PEEK[HEADER]` 的原始字节。
    pub header: Vec<u8>,
}

/// 只取「证明连得上」的信息：登录 + `LIST` 一次。
///
/// 失败一律**收成 [`ConnectionTestResult`]**而不是把错误抛出去——这个函数是给
/// 「测试连接」按钮用的，按钮要的是「成不成 + 为什么」，不是 `Result` 的传播。
pub async fn probe(cfg: ImapConfig) -> ConnectionTestResult {
    match probe_inner(cfg).await {
        Ok(count) => ConnectionTestResult {
            ok: true,
            folder_count: count,
            error: None,
        },
        Err(e) => ConnectionTestResult {
            ok: false,
            folder_count: 0,
            error: Some(e.to_string()),
        },
    }
}

async fn probe_inner(cfg: ImapConfig) -> Result<usize> {
    let mut session = ImapSession::connect(&cfg).await?;
    let folders = session.list_folders().await?;
    session.logout().await;
    Ok(folders.iter().filter(|f| f.selectable).count())
}

impl ImapSession {
    /// 建连 + 登录。
    pub async fn connect(cfg: &ImapConfig) -> Result<Self> {
        if cfg.security == ConnectionSecurity::Plain {
            return Err(MailError::Unsupported(
                "cleartext IMAP is not wired up in this build; use TLS or STARTTLS".into(),
            ));
        }

        let addr = (cfg.host.as_str(), cfg.port);
        let tcp = tokio::time::timeout(ImapConfig::CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| MailError::Timeout(format!("tcp connect to {}:{}", cfg.host, cfg.port)))?
            .map_err(|e| MailError::Connect(format!("{}:{}: {e}", cfg.host, cfg.port)))?;
        // 关掉 Nagle：IMAP 是小报文往返协议，攒包只会加延迟。
        let _ = tcp.set_nodelay(true);

        let connector = TlsConnector::from(tls_config());
        let server_name = rustls::pki_types::ServerName::try_from(cfg.host.clone())
            .map_err(|e| MailError::Connect(format!("invalid host {}: {e}", cfg.host)))?;

        let tls = tokio::time::timeout(
            ImapConfig::CONNECT_TIMEOUT,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| MailError::Timeout("tls handshake".into()))?
        .map_err(|e| MailError::Connect(format!("tls handshake failed: {e}")))?;

        let client = async_imap::Client::new(tls);
        // `Client::login` 的 `Err` 是 `(imap::Error, Client<..>)` 元组（它要把 client
        // 交还调用方以便重试），不实现 `Display`，所以不能走 `with_timeout` —— 这里
        // 单独展开，把元组里的错误取出来。
        let login = client.login(cfg.username.clone(), cfg.password.clone());
        let session = match tokio::time::timeout(ImapConfig::CONNECT_TIMEOUT, login).await {
            Ok(Ok(session)) => session,
            Ok(Err((e, _client))) => return Err(MailError::Auth(e.to_string())),
            Err(_) => {
                return Err(MailError::Timeout(format!(
                    "login exceeded {}s",
                    ImapConfig::CONNECT_TIMEOUT.as_secs()
                )));
            }
        };

        Ok(Self { session })
    }

    /// `LIST "" "*"`。
    pub async fn list_folders(&mut self) -> Result<Vec<ListedFolder>> {
        let stream = with_timeout(
            "list",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.list(None, Some("*")),
        )
        .await?;

        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let name = item.map_err(|e| MailError::Protocol(format!("LIST: {e}")))?;
            let attributes: Vec<String> =
                name.attributes().iter().map(|a| format!("{a:?}")).collect();
            let selectable = !attributes.iter().any(|a| a == "NoSelect");
            out.push(ListedFolder {
                name: name.name().to_string(),
                delimiter: name.delimiter().map(str::to_string),
                attributes,
                selectable,
            });
        }
        Ok(out)
    }

    /// `SELECT <mailbox>`，回过 `UIDVALIDITY` / `UIDNEXT` / 总数。
    pub async fn select(&mut self, mailbox: &str) -> Result<SelectedState> {
        let mb = with_timeout(
            "select",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.select(mailbox),
        )
        .await?;
        Ok(SelectedState {
            uid_validity: mb.uid_validity,
            uid_next: mb.uid_next,
            exists: mb.exists,
        })
    }

    /// 拉 `uid_from` 及以上（含）的新邮件信封。
    ///
    /// 只拉头部（`BODY.PEEK[HEADER]`）而不是整封：列表要在拿到正文前就出来，
    /// 正文按需再拉（`docs/email-design.md` §4.6 的「UI 不等全量完成」）。
    pub async fn fetch_envelopes(&mut self, uid_from: u32) -> Result<Vec<FetchedEnvelope>> {
        let range = format!("{uid_from}:*");
        let stream = with_timeout(
            "uid fetch headers",
            ImapConfig::COMMAND_TIMEOUT,
            self.session
                .uid_fetch(&range, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])"),
        )
        .await?;

        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let fetch = item.map_err(|e| MailError::Protocol(format!("FETCH: {e}")))?;
            let Some(uid) = fetch.uid else { continue };
            // `1:*` 在空文件夹上会回一条 UID 为 0 的伪响应，跳过。
            if uid == 0 {
                continue;
            }
            let (seen, flagged) = flags_of(&fetch);
            out.push(FetchedEnvelope {
                uid,
                seen,
                flagged,
                size: fetch.size,
                header: fetch.header().map(|h| h.to_vec()).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// 拉一整封的原始字节（`BODY.PEEK[]`，不会把它标成已读）。
    pub async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>> {
        let stream = with_timeout(
            "uid fetch body",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_fetch(uid.to_string(), "BODY.PEEK[]"),
        )
        .await?;
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let fetch = item.map_err(|e| MailError::Protocol(format!("FETCH body: {e}")))?;
            if let Some(body) = fetch.body() {
                return Ok(body.to_vec());
            }
        }
        Err(MailError::NotFound(format!("body of uid {uid}")))
    }

    /// `NOOP` —— 保活 + 让服务端有机会推送待发通知。
    pub async fn noop(&mut self) -> Result<()> {
        with_timeout("noop", ImapConfig::COMMAND_TIMEOUT, self.session.noop()).await?;
        Ok(())
    }

    /// 优雅登出。失败不重要（连接反正要关），故吞掉错误只记日志。
    pub async fn logout(mut self) {
        if let Err(e) = self.session.logout().await {
            tracing::debug!(error = %e, "imap logout failed (connection is being dropped anyway)");
        }
    }
}

fn flags_of(fetch: &async_imap::types::Fetch) -> (bool, bool) {
    use async_imap::types::Flag;
    let mut seen = false;
    let mut flagged = false;
    // `Fetch::flags()` 返回的是迭代器（不是 `Option`）——没请求旗标时它为空。
    for f in fetch.flags() {
        match f {
            Flag::Seen => seen = true,
            Flag::Flagged => flagged = true,
            _ => {}
        }
    }
    (seen, flagged)
}

/// 进程级共享一份 TLS 配置（信任链是编译进去的，每次重建没必要）。
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// 给单条 IMAP 命令加超时。
///
/// 包一层是因为 `async-imap` 自身不带超时——没有它，一台半死不活的服务器能把
/// 同步 actor 永久挂住。
async fn with_timeout<T, E, F>(label: &'static str, timeout: Duration, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(MailError::Protocol(format!("{label}: {e}"))),
        Err(_) => Err(MailError::Timeout(format!(
            "{label} exceeded {}s",
            timeout.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleartext_is_refused_before_touching_the_network() {
        // 明文档在这个版本里没接线：必须**在连网之前**就拒绝，而不是发出明文口令。
        let cfg = ImapConfig {
            host: "127.0.0.1".into(),
            port: 143,
            security: ConnectionSecurity::Plain,
            username: "u".into(),
            password: "p".into(),
        };
        let rt = crate::runtime::MailRuntime::new().unwrap();
        let res = rt.block_on(ImapSession::connect(&cfg));
        assert!(matches!(res, Err(MailError::Unsupported(_))));
    }

    #[test]
    fn tls_config_is_built_once_with_webpki_roots() {
        let a = tls_config();
        let b = tls_config();
        assert!(Arc::ptr_eq(&a, &b), "tls config should be shared");
    }

    #[test]
    fn probe_reports_failure_instead_of_propagating_it() {
        // 连一个必然连不上的地址：probe 必须返回 ok=false 而不是抛错。
        let cfg = ImapConfig {
            host: "127.0.0.1".into(),
            // 保留端口，几乎不可能有服务端在听。
            port: 1,
            security: ConnectionSecurity::Tls,
            username: "u".into(),
            password: "p".into(),
        };
        let rt = crate::runtime::MailRuntime::new().unwrap();
        let report = rt.block_on(probe(cfg));
        assert!(!report.ok);
        assert!(report.error.is_some());
        assert_eq!(report.folder_count, 0);
    }
}
