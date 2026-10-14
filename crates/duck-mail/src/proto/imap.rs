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
use async_imap::extensions::idle::IdleResponse;
use async_imap::types::{Capabilities, Capability, NameAttribute};
use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{MailError, Result};
use crate::models::{AuthKind, ConnectionSecurity, ConnectionTestResult};
use crate::oauth::sasl_xoauth2;
use crate::proto::{ImapConfig, ProtocolCaps};

/// 加密后的 IMAP 流。
///
/// `pub(crate)`：它是 `tokio_rustls` / `tokio` 的派生类型，「tokio 不出公共 API」
/// （`docs/email-design.md` §4.1 纪律 2）这条要求把它挡在 crate 内。
pub(crate) type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

/// 一封邮件在服务端上的常见旗标（写方向用）。
///
/// 只给「本批次会写」的三个：已读 / 旗标 / 删除。`\Answered` / `\Draft` 留给后续批次。
pub const FLAG_SEEN: &str = "\\Seen";
/// 旗标（星标）。
pub const FLAG_FLAGGED: &str = "\\Flagged";
/// 删除标记（配 `EXPUNGE` 才真正销毁）。
pub const FLAG_DELETED: &str = "\\Deleted";

/// 一条已登录的 IMAP 会话（TLS）。
pub struct ImapSession {
    session: Session<TlsStream>,
    /// 建连时探测到的服务端能力位。写方向（MOVE 回退、UIDPLUS）按它决策。
    caps: ProtocolCaps,
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

/// `LIST` 属性 → IMAP **线上写法**（带前导反斜杠，如 `\Sent`）。
///
/// ⚠️ `imap-proto` 把属性解析成**枚举**（`\Sent` → [`NameAttribute::Sent`]），所以
/// `format!("{a:?}")` 出来的是 `Sent`——**反斜杠没了**。而下游
/// [`crate::models::FolderRole::from_imap`] 比对的是 `\Sent`，于是旗标判定永远不成立，
/// 所有文件夹落 `Other`，同步**一个都不跑**（表现为「同步完成：0 个文件夹」）。
/// 163 / QQ 这类带旗标的服务端也照样中招，所以必须在这儿还原成线格式。
///
/// `Extension` 的载荷**已含**前导反斜杠（解析器捕获的是整个 `\Foo`），原样透传。
fn attribute_wire_form(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\Noinferiors".to_string(),
        NameAttribute::NoSelect => "\\Noselect".to_string(),
        NameAttribute::Marked => "\\Marked".to_string(),
        NameAttribute::Unmarked => "\\Unmarked".to_string(),
        NameAttribute::All => "\\All".to_string(),
        NameAttribute::Archive => "\\Archive".to_string(),
        NameAttribute::Drafts => "\\Drafts".to_string(),
        NameAttribute::Flagged => "\\Flagged".to_string(),
        NameAttribute::Junk => "\\Junk".to_string(),
        NameAttribute::Sent => "\\Sent".to_string(),
        NameAttribute::Trash => "\\Trash".to_string(),
        NameAttribute::Extension(name) => name.to_string(),
        // `NameAttribute` 是 `#[non_exhaustive]`：上游将来加了新变体也不能把属性丢掉。
        other => format!("{other:?}"),
    }
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

/// `UID FETCH (UID FLAGS)` 回来的一条（旗标对账用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchedFlags {
    /// IMAP UID。
    pub uid: u32,
    /// 是否 `\Seen`。
    pub seen: bool,
    /// 是否 `\Flagged`。
    pub flagged: bool,
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

/// XOAUTH2 的 `async-imap::Authenticator`。
///
/// `async-imap` 的握手会**自己**把 [`Self::process`] 的返回值做 base64，所以这里回的是
/// **明文**形式（见 [`sasl_xoauth2`]）。服务端拒绝时会在 `+ ` 挑战里塞一段 JSON 错误，
/// 这时回**空串**让握手尽快收尾 —— 协议规定空响应即中止，继续回内容只会让服务端
/// 再发一轮挑战。
struct XOAuth2 {
    user: String,
    token: String,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.sent {
            String::new()
        } else {
            self.sent = true;
            sasl_xoauth2(&self.user, &self.token)
        }
    }
}

impl ImapSession {
    /// 建连 + 登录。
    ///
    /// 加密档一律走 TLS，**没有明文回退**：
    /// - [`ConnectionSecurity::Tls`]：TCP 建连后直接隐式 TLS 握手（993）；
    /// - [`ConnectionSecurity::StartTls`]：TCP 建连后明文发 `STARTTLS`，成功后把
    ///   同一条连接升级为 TLS（143）——服务端拒绝则建连失败，不会退化成明文；
    /// - [`ConnectionSecurity::Plain`]：直接拒绝（本构建不提供明文 IMAP）。
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

        // `StartTls` 档：先在**明文**连接上发 `STARTTLS`，再就地把同一条 TCP 升级成
        // TLS（RFC 2595）。库没有封装好的 `starttls()`：公开路径是 `Client`（经 `Deref`
        // 到 `Connection`）发 `run_command_and_check_ok("STARTTLS", None)`，
        // 再用 `Client::into_inner()` 取回底层流（见 crate 内 `lib.rs` 的 STARTTLS 段）。
        // `Tls` 档则直接对 TCP 做 TLS 握手。两者之后是同一条隐式-TLS 路径。
        let tcp = if cfg.security == ConnectionSecurity::StartTls {
            let mut plain = async_imap::Client::new(tcp);
            tokio::time::timeout(
                ImapConfig::COMMAND_TIMEOUT,
                plain.run_command_and_check_ok("STARTTLS", None),
            )
            .await
            .map_err(|_| MailError::Timeout("STARTTLS command".into()))?
            .map_err(|e| MailError::Connect(format!("STARTTLS rejected: {e}")))?;
            // 服务端接受后（tagged `OK`），连接上不再有 IMAP 数据，取回 TCP 交给 rustls。
            plain.into_inner()
        } else {
            tcp
        };

        let tls = tokio::time::timeout(
            ImapConfig::CONNECT_TIMEOUT,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| MailError::Timeout("tls handshake".into()))?
        .map_err(|e| MailError::Connect(format!("tls handshake failed: {e}")))?;

        let client = async_imap::Client::new(tls);
        // `login_with_capabilities` 的 `Err` 是 `(imap::Error, Client<..>)` 元组（它要把
        // client 交还调用方以便重试），不实现 `Display`，所以不能走 `with_timeout` ——
        // 这里单独展开，把元组里的错误取出来。用它而不是 `login`：登录响应里常带
        // `CAPABILITY` 响应码，那样就不必再单独发一条 `CAPABILITY`。
        // 认证分两条：OAuth2 账户走 `AUTHENTICATE XOAUTH2`（口令位放的是访问令牌），
        // 其余走 `LOGIN`。两条路的失败都是 `(Error, Client)` 元组（库要把 client 交还
        // 调用方以便重试），都不实现 `Display`，所以各自展开一次超时。
        let (session, announced) = if cfg.auth == AuthKind::OAuth2 {
            let authenticator = XOAuth2 {
                user: cfg.username.clone(),
                token: cfg.password.clone(),
                sent: false,
            };
            match tokio::time::timeout(
                ImapConfig::CONNECT_TIMEOUT,
                client.authenticate("XOAUTH2", authenticator),
            )
            .await
            {
                // `authenticate` 不回传登录响应里的 CAPABILITY（那只有 `LOGIN` 才有），
                // 所以这里给 `None`，下面会显式问一次。
                Ok(Ok(session)) => (session, None),
                Ok(Err((e, _client))) => return Err(MailError::Auth(e.to_string())),
                Err(_) => {
                    return Err(MailError::Timeout(format!(
                        "XOAUTH2 exceeded {}s",
                        ImapConfig::CONNECT_TIMEOUT.as_secs()
                    )));
                }
            }
        } else {
            let login = client.login_with_capabilities(cfg.username.clone(), cfg.password.clone());
            match tokio::time::timeout(ImapConfig::CONNECT_TIMEOUT, login).await {
                Ok(Ok(parts)) => parts,
                Ok(Err((e, _client))) => return Err(MailError::Auth(e.to_string())),
                Err(_) => {
                    return Err(MailError::Timeout(format!(
                        "login exceeded {}s",
                        ImapConfig::CONNECT_TIMEOUT.as_secs()
                    )));
                }
            }
        };

        let mut this = Self {
            session,
            caps: ProtocolCaps::default(),
        };
        // 登录响应没带 CAPABILITY 就显式问一次；再失败也不致命——能力位退回默认
        // （全 false），写方向会走保守的 COPY + \Deleted + UID EXPUNGE 回退路径。
        //
        // 这一步（以及 `login_with_capabilities` 里的 CAPABILITY 响应码）都在**升级之后**
        // 的会话上发生：`StartTls` 档升级前不会读任何能力位，所以不会拿明文阶段的
        // caps 当准（RFC 2595 明确 STARTTLS 后能力集可能变化）。
        let atoms = match announced {
            Some(caps) => capability_atoms(&caps),
            None => match this.capability_atoms().await {
                Ok(atoms) => atoms,
                Err(e) => {
                    tracing::debug!(error = %e, "CAPABILITY probe failed; assuming no extensions");
                    Vec::new()
                }
            },
        };
        this.caps = caps_from_atoms(&atoms);
        tracing::debug!(caps = ?this.caps, "imap capabilities");

        // QQ / 163 一类国内服务端要求登录后先亮身份（RFC 2971 `ID`），否则后续
        // `SELECT` / `APPEND` 会被拒（报「Unsafe Login. Please contact kefu@…」——
        // `LIST` 不受限，所以「测试连接」看起来是通的，一同步就全军覆没）。
        // 尽力而为：不认 `ID` 的服务端拒绝就算了，记条 debug 日志继续。
        let id_command = format!(
            "ID (\"name\" \"Duck\" \"version\" \"{}\")",
            env!("CARGO_PKG_VERSION")
        );
        if let Err(e) = this.session.run_command_and_check_ok(&id_command).await {
            tracing::debug!(error = %e, "IMAP ID command rejected; continuing without it");
        }
        Ok(this)
    }

    /// 服务端能力位（建连时探测一次）。
    pub fn caps(&self) -> &ProtocolCaps {
        &self.caps
    }

    /// `CAPABILITY` 命令 → 原子名列表。
    async fn capability_atoms(&mut self) -> Result<Vec<String>> {
        let caps = with_timeout(
            "capability",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.capabilities(),
        )
        .await?;
        Ok(capability_atoms(&caps))
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
                name.attributes().iter().map(attribute_wire_form).collect();
            let selectable = !attributes
                .iter()
                .any(|a| a.eq_ignore_ascii_case("\\Noselect"));
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
    ///
    /// ⚠️ **返回集恒满足 `uid >= uid_from`**。服务端会用 `n:*` 的范围语义回吐越界
    /// 信封（见 [`uid_in_requested_range`]），调用方不必自己再挡一遍——
    /// 「拉回几封」与「新增几封」是两回事，计数要另算。
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
            // 越界回吐必须就地丢掉，理由见 [`uid_in_requested_range`]：本地已经追平
            // 服务端时（`uid_from` = 最大 UID + 1），`n:*` 会把**最大 UID 那封**回吐
            // 给客户端，照单全收就会每轮上报一封并不存在的新邮件。
            if !uid_in_requested_range(uid, uid_from) {
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

    /// 拉一段 UID 区间的旗标（`UID FETCH <set> (UID FLAGS)`）。
    ///
    /// `set` 走 IMAP 的 UID 集合语法（`a:b` 或 `a,b,c`）；同步引擎用它做**窗口内**
    /// 的旗标对账——只比对最近这批，不整箱拉（`docs/email-design.md` §4.6 第 4 步）。
    pub async fn fetch_flags(&mut self, set: &str) -> Result<Vec<FetchedFlags>> {
        let stream = with_timeout(
            "uid fetch flags",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_fetch(set, "(UID FLAGS)"),
        )
        .await?;

        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let fetch = item.map_err(|e| MailError::Protocol(format!("FETCH FLAGS: {e}")))?;
            let Some(uid) = fetch.uid else { continue };
            if uid == 0 {
                continue;
            }
            let (seen, flagged) = flags_of(&fetch);
            out.push(FetchedFlags { uid, seen, flagged });
        }
        Ok(out)
    }

    // ---- 写方向（M1.5：动作队列要落的 IMAP 命令） ----
    //
    // 三条纪律：
    // ① `UID STORE` / `UID EXPUNGE` 返回的是 `impl Stream`，**必须消费到底**
    //    （`.SILENT` 也不保证一条不回），否则未标记响应留在管道里 → 协议脱同步；
    // ② 命令一律走 **UID**（非 UID 的 `store` / `copy` / `mv` 不用），UID 在文件夹内
    //    稳定，序号会随 EXPUNGE 漂移；
    // ③ 这些命令都要求邮箱**已 `SELECT`**，调用方负责先选好（见 `sync` / `queue`）。

    /// `UID STORE <set> +FLAGS.SILENT (…)`：加旗标。
    pub async fn add_flags(&mut self, uids: &[u32], flags: &[&str]) -> Result<()> {
        self.store_flags(uids, flags, StoreMode::Add).await
    }

    /// `UID STORE <set> -FLAGS.SILENT (…)`：去旗标。
    pub async fn remove_flags(&mut self, uids: &[u32], flags: &[&str]) -> Result<()> {
        self.store_flags(uids, flags, StoreMode::Remove).await
    }

    /// `UID STORE <set> FLAGS.SILENT (…)`：整体替换。
    pub async fn set_flags(&mut self, uids: &[u32], flags: &[&str]) -> Result<()> {
        self.store_flags(uids, flags, StoreMode::Set).await
    }

    /// `UID STORE` 的公共实现。空集合 / 空旗标是 no-op（`+FLAGS.SILENT ()` 是非法命令）。
    async fn store_flags(&mut self, uids: &[u32], flags: &[&str], mode: StoreMode) -> Result<()> {
        if uids.is_empty() || flags.is_empty() {
            return Ok(());
        }
        let set = uid_set(uids);
        let query = store_query(flags, mode);
        let stream = with_timeout(
            "uid store",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_store(set, query),
        )
        .await?;
        // ⚠️ 必须消费到底。见本段的纪律 ①。
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            item.map_err(|e| MailError::Protocol(format!("UID STORE: {e}")))?;
        }
        Ok(())
    }

    /// 把一组 UID 移到 `folder`。
    ///
    /// 有 `MOVE` 扩展（RFC 6851）→ 一条原子的 `UID MOVE`；否则回退
    /// `UID COPY` + `+FLAGS \Deleted` + `UID EXPUNGE`（[`move_plan`] 是纯函数，
    /// 决策被单测钉住；本环境没有真实 IMAP 服务器，网络路径无法自动化验证）。
    ///
    /// 三步回退**不是**原子的：中断后消息可能「已 COPY 但没删」，重试会产生副本。
    /// 这是无 MOVE 服务器上的已知代价（设计文档 §4.6 / §M1.5 明写接受）。
    pub async fn move_messages(&mut self, uids: &[u32], folder: &str) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        match move_plan(&self.caps) {
            MovePlan::Move => {
                let set = uid_set(uids);
                with_timeout(
                    "uid move",
                    ImapConfig::COMMAND_TIMEOUT,
                    self.session.uid_mv(set, folder),
                )
                .await?;
                Ok(())
            }
            MovePlan::CopyThenDelete => {
                let set = uid_set(uids);
                with_timeout(
                    "uid copy",
                    ImapConfig::COMMAND_TIMEOUT,
                    self.session.uid_copy(set, folder),
                )
                .await?;
                self.add_flags(uids, &[FLAG_DELETED]).await?;
                self.expunge_uids(uids).await
            }
        }
    }

    // ---- 发信写回：APPEND ----

    /// `APPEND <mailbox> (\Seen) {<n>}`：把一整封**原始报文**追加到指定邮箱。
    ///
    /// 发信之后靠它把副本写进服务端「已发送」（RFC 3501 §6.3.11）。这一步不能省：
    /// SMTP 只负责投递，**副本要发信方自己塞**——服务端不会把发出去的信替你托管进
    /// `\Sent`（QQ / 163 / Gmail / Outlook 都不做）。缺了它，用户发完信在「已发送」
    /// 里找不着自己，换台设备打开更是完全没有。调用方见 `duck-mail/src/lib.rs` 的
    /// `write_back_sent`。
    ///
    /// ⚠️ 与本节其它写命令不同，**它不需要先 `SELECT`**：`APPEND` 作用于未选中的
    /// 邮箱，直接指定目标即可（`add_flags` / `move_messages` 则必须选好再发）。
    ///
    /// `\Seen`：自己发出去的信自然是读过的，服务端也据此算未读计数（不给的话，
    /// 「已发送」上会挂出一个假的未读角标）。
    /// `INTERNALDATE` 交给服务端打（`None`）——列表按报文里的 `Date` 头排序，那个由
    /// `compose::build_message` 写进头部，与 IMAP 的到达时间无关。
    ///
    /// 邮箱名交给 `async-imap` 的 `validate_str`（恒加双引号并转义），所以带空格或
    /// 引号的名字（`Sent Items`）也能直接传；本模块传的是服务端 `LIST` 回的**原始名**，
    /// 与 `SELECT` 用的是同一个串（含 Modified UTF-7 形态，见 [`crate::proto::utf7`]）。
    pub async fn append_message(&mut self, mailbox: &str, message: &[u8]) -> Result<()> {
        with_timeout(
            "append",
            ImapConfig::COMMAND_TIMEOUT,
            self.session
                .append(mailbox, Some("(\\Seen)"), None, message),
        )
        .await?;
        Ok(())
    }

    /// 删除一组 UID：有垃圾箱就移进垃圾箱，否则就地 `\Deleted` + `UID EXPUNGE`。
    pub async fn delete_messages(&mut self, uids: &[u32], trash: Option<&str>) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        match delete_plan(trash) {
            DeletePlan::ToTrash => {
                // `delete_plan` 只在 `trash` 非空时才返回 `ToTrash`。
                let trash = trash.unwrap_or_default();
                self.move_messages(uids, trash).await
            }
            DeletePlan::FlagAndExpunge => {
                self.add_flags(uids, &[FLAG_DELETED]).await?;
                self.expunge_uids(uids).await
            }
        }
    }

    /// `UID EXPUNGE <set>`（RFC 4315，需 `UIDPLUS`）：只销毁集合内的 `\Deleted`。
    ///
    /// 比无参 `EXPUNGE` 安全：别的设备标了 `\Deleted`、还没被清掉的消息不会连带销毁。
    pub async fn expunge_uids(&mut self, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let set = uid_set(uids);
        let stream = with_timeout(
            "uid expunge",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_expunge(set),
        )
        .await?;
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            item.map_err(|e| MailError::Protocol(format!("UID EXPUNGE: {e}")))?;
        }
        Ok(())
    }

    /// 无参 `EXPUNGE`：清掉当前邮箱里**全部** `\Deleted`。
    ///
    /// 只在明确要「就地清空」时用；日常删除走 [`Self::expunge_uids`]（范围受限）。
    pub async fn expunge(&mut self) -> Result<()> {
        let stream = with_timeout(
            "expunge",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.expunge(),
        )
        .await?;
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            item.map_err(|e| MailError::Protocol(format!("EXPUNGE: {e}")))?;
        }
        Ok(())
    }

    /// `UID SEARCH ALL` —— 该文件夹在服务端的全部 UID（删除对账用）。
    ///
    /// 必须在 `SELECT` 之后调用。结果排序后返回，便于与本地集合做差。
    pub async fn search_all_uids(&mut self) -> Result<Vec<u32>> {
        let mut uids: Vec<u32> = with_timeout(
            "uid search",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_search("ALL"),
        )
        .await?
        .into_iter()
        .collect();
        uids.sort_unstable();
        Ok(uids)
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

    // ---- 按节取回（附件按需下载） ----

    /// 取回一个 MIME 部件的**原始**（未解码）内容，连同它的 MIME 头。
    ///
    /// 发一条 `UID FETCH <uid> (UID BODY.PEEK[<section>.MIME] BODY.PEEK[<section>])`：
    /// `BODY.PEEK` 而非 `BODY`——后者会把邮件标成 `\Seen`，下一个附件不该顺手把整封
    /// 邮件变成「已读」。两段一起取是因为单靠正文段拿不到 `Content-Transfer-Encoding`，
    /// 调用方要把它们拼成一封单部件报文交给 MIME 解析器才能解码（见
    /// [`crate::mime::decode_attachment_part`]）。段号必须是 IMAP 语法（`2` / `1.2`）。
    pub async fn fetch_section(&mut self, uid: u32, section: &str) -> Result<FetchedSection> {
        use async_imap::imap_proto::types::{MessageSection, SectionPath};

        let path = parse_section_path(section)?;
        let mime_path = SectionPath::Part(path.clone(), Some(MessageSection::Mime));
        let body_path = SectionPath::Part(path, None);
        let query = format!("(UID BODY.PEEK[{section}.MIME] BODY.PEEK[{section}])");
        let stream = with_timeout(
            "uid fetch section",
            ImapConfig::COMMAND_TIMEOUT,
            self.session.uid_fetch(uid.to_string(), &query),
        )
        .await?;

        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let fetch = item.map_err(|e| MailError::Protocol(format!("FETCH section: {e}")))?;
            if fetch.uid != Some(uid) {
                continue;
            }
            let mime_headers = fetch
                .section(&mime_path)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            if let Some(body) = fetch.section(&body_path) {
                return Ok(FetchedSection {
                    mime_headers,
                    body: body.to_vec(),
                });
            }
        }
        Err(MailError::NotFound(format!(
            "section {section} of uid {uid}"
        )))
    }

    // ---- IDLE（收信推送） ----

    /// 进入 `IDLE` 等待一次变更，最多等 `max_idle`；退出时发 `DONE` 并把会话还回来。
    ///
    /// 返回值里的 [`ImapSession`] **可以继续用**（`DONE` 之后服务器从 IDLE 状态退出，
    /// 连接仍是已登录、已 `SELECT` 的会话），调用方拿到后就能直接跑一轮增量同步。
    ///
    /// 三个工程细节（照 `flectar-mail-core/src/imap/mod.rs` 的裁决）：
    /// - **`DONE` 包超时**：`Handle::done` 自身不带超时，一台半死的服务器会让它永久
    ///   挂住；超时即返回错误，调用方断开重连（绝不能拿一条脱同步的连接继续用）。
    /// - **不支持 IDLE**：返回 [`IdleOutcome::Unsupported`]，调用方回落 `NOOP` 轮询；
    ///   这条路径**不消耗连接**，会话原样还回。
    /// - **max-idle 到点**：返回 [`IdleOutcome::Timeout`]，调用方据此重入 IDLE 或跑一次
    ///   兜底同步。
    pub async fn idle_once(self, max_idle: Duration) -> Result<(Self, IdleOutcome)> {
        if !self.caps.idle {
            return Ok((self, IdleOutcome::Unsupported));
        }
        let Self { session, caps } = self;
        let mut handle = session.idle();
        // `IDLE` 的进入等 continuation（`+ idling`），同样要有超时。
        with_timeout("idle init", ImapConfig::COMMAND_TIMEOUT, handle.init()).await?;

        let outcome = {
            let (wait, stop) = handle.wait_with_timeout(max_idle);
            // `wait_with_timeout` 自己带 `max_idle` 超时；外面再兜一层，避免库的
            // 内部计时器在极端情况下不触发。
            let result = tokio::time::timeout(max_idle + IDLE_DONE_TIMEOUT, wait).await;
            // `stop` 只是打断句柄（本函数不主动打断），显式丢弃，别让它在 `done` 期间
            // 触发一次多余的中断。
            drop(stop);
            match result {
                Ok(Ok(IdleResponse::NewData(_))) => IdleOutcome::Changed,
                // `ManualInterrupt` 只在外部触发打断时出现；本模块不触发，真遇到了
                // 就按「没消息」处理，安全地重入 IDLE。
                Ok(Ok(IdleResponse::ManualInterrupt | IdleResponse::Timeout)) => {
                    IdleOutcome::Timeout
                }
                Ok(Err(e)) => return Err(MailError::Protocol(format!("idle wait: {e}"))),
                Err(_) => IdleOutcome::Timeout,
            }
        };

        // `done` 无内建超时 → 必须包一层。超时/出错都返回 `Err`：调用方会断开重连。
        let session = match tokio::time::timeout(IDLE_DONE_TIMEOUT, handle.done()).await {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => return Err(MailError::Protocol(format!("idle done: {e}"))),
            Err(_) => {
                return Err(MailError::Timeout(format!(
                    "idle done exceeded {}s",
                    IDLE_DONE_TIMEOUT.as_secs()
                )));
            }
        };
        Ok((Self { session, caps }, outcome))
    }
}

/// `DONE` 之后等服务端 `OK` 的超时。见 [`ImapSession::idle_once`]。
const IDLE_DONE_TIMEOUT: Duration = Duration::from_secs(10);

/// 一次 `IDLE` 等待的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    /// 服务端推送了变更（至少「有动静」）。
    Changed,
    /// 等到 `max_idle` 都没有消息（到点应重入 IDLE / 跑一次兜底同步）。
    Timeout,
    /// 服务端没有 `IDLE` 能力位（调用方回落 `NOOP` 轮询）。
    Unsupported,
}

/// 按节取回的一段内容（原始、未解码）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FetchedSection {
    /// `BODY[<section>.MIME]`——该部件的 MIME 头（可能为空：服务端没回这一项）。
    pub mime_headers: Vec<u8>,
    /// `BODY[<section>]`——该部件的正文（仍带传输编码）。
    pub body: Vec<u8>,
}

/// 纯函数：把 `"2"` / `"1.2"` 这样的 IMAP 节号解析成数字路径。
///
/// 非法（空、含非数字段）时返回 `Protocol` 错误——段号是本模块自己从 MIME 结构里
/// 推出来的，不该把非法值发到服务器上。
pub fn parse_section_path(section: &str) -> Result<Vec<u32>> {
    if section.is_empty() {
        return Err(MailError::Protocol("empty imap section".into()));
    }
    section
        .split('.')
        .map(|part| {
            part.parse::<u32>()
                .map_err(|_| MailError::Protocol(format!("invalid imap section {section:?}")))
        })
        .collect()
}

/// `UID STORE` 的模式（对应 `+FLAGS` / `-FLAGS` / `FLAGS` 三种查询语法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    /// `+FLAGS`：加旗标（已存在则保持）。
    Add,
    /// `-FLAGS`：去旗标。
    Remove,
    /// `FLAGS`：整体替换。
    Set,
}

impl StoreMode {
    /// 落进查询串的关键字。
    const fn keyword(self) -> &'static str {
        match self {
            Self::Add => "+FLAGS",
            Self::Remove => "-FLAGS",
            Self::Set => "FLAGS",
        }
    }
}

/// 纯函数：拼 `UID STORE` 的查询串，如 `+FLAGS.SILENT (\Seen \Flagged)`。
///
/// `.SILENT` 让服务端**不必回报**每条消息的新旗标：本模块不读它，少一批无标记响应
/// 就少一轮解析。⚠️ 但 `.SILENT` **不保证一条不回**，返回的流仍必须消费到底。
pub fn store_query(flags: &[&str], mode: StoreMode) -> String {
    format!("{}.SILENT ({})", mode.keyword(), flags.join(" "))
}

/// 纯函数：把一组 UID 编成 IMAP 集合串（去重、升序、连续段压成 `a:b`）。
///
/// 取舍：连续 **≥ 3** 个才压成区间——`1:2` 与 `1,2` 同长，压了无益反而难读。区间压缩
/// 只为缩短命令（大文件夹上批量移动 / 删除时命令可能上万字符），对服务端语义等价。
/// 空输入回空串；调用方必须先判空（`UID STORE "" …` 是非法命令）。
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        // 尽量向后吸附连续 UID。用 `checked_add` 防 `u32::MAX` 处溢出。
        while let Some(next) = sorted.get(index + 1).copied() {
            if end.checked_add(1) != Some(next) {
                break;
            }
            index += 1;
            end = next;
        }
        let span = end - start;
        if span >= 2 {
            parts.push(format!("{start}:{end}"));
        } else if span == 1 {
            // 连续两个：写两个单值。
            parts.push(start.to_string());
            parts.push(end.to_string());
        } else {
            parts.push(start.to_string());
        }
        index += 1;
    }
    parts.join(",")
}

/// 移动要走哪条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePlan {
    /// 服务端有 `MOVE` 扩展：一条原子的 `UID MOVE`。
    Move,
    /// 无扩展：`UID COPY` + `+FLAGS \Deleted` + `UID EXPUNGE` 三步。
    CopyThenDelete,
}

/// 纯函数：按能力位决定移动路径。
pub fn move_plan(caps: &ProtocolCaps) -> MovePlan {
    if caps.move_ {
        MovePlan::Move
    } else {
        MovePlan::CopyThenDelete
    }
}

/// 删除要走哪条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletePlan {
    /// 有垃圾箱：删除 = 移进垃圾箱。
    ToTrash,
    /// 没有垃圾箱：`+FLAGS \Deleted` + `UID EXPUNGE` 就地销毁。
    FlagAndExpunge,
}

/// 纯函数：有非空垃圾箱名就移进垃圾箱，否则就地销毁。
pub fn delete_plan(trash: Option<&str>) -> DeletePlan {
    match trash {
        Some(name) if !name.trim().is_empty() => DeletePlan::ToTrash,
        _ => DeletePlan::FlagAndExpunge,
    }
}

/// 纯函数：CAPABILITY 原子名 → 能力位。
///
/// `server_search` 取 `IMAP4rev1` 或显式 `SEARCH`：`SEARCH` 是 IMAP4rev1 的必备能力，
/// 多数服务端**不会**在 CAPABILITY 里单独列它，所以只认 `SEARCH` 会把几乎所有服务器
/// 都判成「不支持服务端搜索」。
pub fn caps_from_atoms(atoms: &[String]) -> ProtocolCaps {
    let has = |needle: &str| atoms.iter().any(|a| a.eq_ignore_ascii_case(needle));
    ProtocolCaps {
        idle: has("IDLE"),
        move_: has("MOVE"),
        uidplus: has("UIDPLUS"),
        special_use: has("SPECIAL-USE"),
        server_search: has("IMAP4rev1") || has("SEARCH"),
    }
}

/// `Capabilities` → 原子名列表。
fn capability_atoms(caps: &Capabilities) -> Vec<String> {
    caps.iter()
        .map(|c| match c {
            Capability::Imap4rev1 => "IMAP4rev1".to_string(),
            Capability::Auth(name) => format!("AUTH={name}"),
            Capability::Atom(name) => name.clone(),
        })
        .collect()
}

/// 服务端回的信封是否真的落在请求区间 `uid_from:*` 内。
///
/// 两条过滤合成一个判据，**两条都必要**：
///
/// - `uid == 0`：空文件夹上 `UID FETCH 1:*` 会回一条 UID 为 0 的**伪响应**，它不是邮件；
/// - `uid < uid_from`：**RFC 3501 的 `seq-range` 语义取 `min..max`**——最大 UID 小于
///   `n` 时 `n:*` 等价于 `n:max`，服务端会把**最大 UID 那封原样回吐**（请求 `96:*`、
///   最大 UID 95 → 回 95）。本地一旦追平（`uid_from` = 最大 UID + 1），回吐的正是
///   「最后一封」，照单全收就会让增量同步**每轮上报一封并不存在的新邮件**
///   ——「一直提示有新邮件、其实什么都没有」的协议层成因。
///
/// 提到模块级是因为它是个**纯函数**：越界回吐取决于服务端实现，在真机上极难复现，
/// 只能靠单测把语义钉住（见本文件 `tests`）。
fn uid_in_requested_range(uid: u32, uid_from: u32) -> bool {
    // `uid_from` 为 0（`0:*` 即「全部」）时只剩伪响应过滤这一条。
    uid != 0 && uid >= uid_from
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
///
/// `pub(crate)`：SMTP（[`crate::proto::smtp`]）要用**同一份**配置——全仓一份根集，
/// 两条通道的信任链不能各造一份。
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
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

    /// `UID FETCH n:*` 的越界回吐与空文件夹伪响应都必须被挡在「新邮件」判定之外。
    ///
    /// 真机上要复现得恰好卡在「本地追平服务端」那一刻，且行为还随服务端实现而变；
    /// 这里把 RFC 3501 的 `seq-range` 语义（取 `min..max`）钉死：请求 `96:*` 而服务端
    /// 回了 95（最后一封），必须判为**不在区间内**——否则增量同步每轮都会报出一条
    /// 并不存在的新邮件（「一直提示有新邮件、其实什么都没有」）。
    #[test]
    fn envelope_outside_the_requested_uid_range_is_filtered_out() {
        // 越界回吐：请求 96 起，服务端把最大 UID 95 原样回了过来（`96:*` ≡ `96:95`）。
        assert!(!uid_in_requested_range(95, 96));
        // 正常增量：起点本身就是新邮件，必须留下。
        assert!(uid_in_requested_range(96, 96));
        assert!(uid_in_requested_range(1000, 96));
        // 空文件夹上 `1:*` 的伪响应（UID 0）不是邮件。
        assert!(!uid_in_requested_range(0, 1));
        // `0:*`（整箱）只剩伪响应过滤这一条：`uid_from` 为 0 不该把什么都挡掉。
        assert!(uid_in_requested_range(1, 0));
    }

    #[test]
    fn cleartext_is_refused_before_touching_the_network() {
        // 明文档在这个版本里没接线：必须**在连网之前**就拒绝，而不是发出明文口令。
        let cfg = ImapConfig {
            host: "127.0.0.1".into(),
            port: 143,
            security: ConnectionSecurity::Plain,
            username: "u".into(),
            password: "p".into(),
            auth: AuthKind::Password,
        };
        let rt = crate::runtime::MailRuntime::new().unwrap();
        let res = rt.block_on(ImapSession::connect(&cfg));
        assert!(matches!(res, Err(MailError::Unsupported(_))));
    }

    #[test]
    fn xoauth2_authenticator_sends_the_sasl_string_exactly_once() {
        use async_imap::Authenticator as _;

        let mut auth = XOAuth2 {
            user: "alice@example.com".into(),
            token: "token-1".into(),
            sent: false,
        };
        // `async-imap` 会自己 base64，所以这里回的必须是**明文** SASL 初始响应。
        assert_eq!(
            auth.process(b""),
            "user=alice@example.com\u{1}auth=Bearer token-1\u{1}\u{1}"
        );
        // 服务端再发一次挑战（失败时里面是 JSON 错误）→ 回空串中止握手，
        // 继续回内容只会让服务端再发一轮挑战。
        assert_eq!(auth.process(br#"{"status":"401"}"#), "");
    }

    #[test]
    fn tls_config_is_built_once_with_webpki_roots() {
        let a = tls_config();
        let b = tls_config();
        assert!(Arc::ptr_eq(&a, &b), "tls config should be shared");
    }

    #[test]
    fn store_query_carries_the_mode_and_flags() {
        // `.SILENT` 让服务端不回报旗标；模式关键字决定加 / 去 / 替换。
        assert_eq!(
            store_query(&[FLAG_SEEN], StoreMode::Add),
            "+FLAGS.SILENT (\\Seen)"
        );
        assert_eq!(
            store_query(&[FLAG_FLAGGED], StoreMode::Remove),
            "-FLAGS.SILENT (\\Flagged)"
        );
        assert_eq!(
            store_query(&[FLAG_SEEN, FLAG_FLAGGED], StoreMode::Set),
            "FLAGS.SILENT (\\Seen \\Flagged)"
        );
    }

    #[test]
    fn uid_set_sorts_dedups_and_compresses_long_runs() {
        // 乱序 + 重复 → 升序去重。
        assert_eq!(uid_set(&[3, 1, 2, 2]), "1:3");
        // 连续 3 个压成区间；连续 2 个写两个单值（`1:2` 与 `1,2` 同长）。
        assert_eq!(uid_set(&[1, 2]), "1,2");
        assert_eq!(uid_set(&[1, 2, 3, 5, 6, 9]), "1:3,5,6,9");
        assert_eq!(uid_set(&[7, 9, 10, 11, 12, 20]), "7,9:12,20");
        // 单值 / 空集合。
        assert_eq!(uid_set(&[42]), "42");
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn move_plan_only_uses_move_when_the_server_advertises_it() {
        let mut caps = ProtocolCaps::default();
        assert_eq!(move_plan(&caps), MovePlan::CopyThenDelete);
        caps.move_ = true;
        assert_eq!(move_plan(&caps), MovePlan::Move);
    }

    #[test]
    fn delete_plan_routes_to_trash_only_with_a_real_name() {
        assert_eq!(delete_plan(Some("Trash")), DeletePlan::ToTrash);
        // 空 / 全空白不是「有垃圾箱」，退回就地销毁。
        assert_eq!(delete_plan(Some("   ")), DeletePlan::FlagAndExpunge);
        assert_eq!(delete_plan(Some("")), DeletePlan::FlagAndExpunge);
        assert_eq!(delete_plan(None), DeletePlan::FlagAndExpunge);
    }

    #[test]
    fn caps_from_atoms_is_case_insensitive_and_search_defaults_true() {
        let atoms = vec![
            "IMAP4rev1".to_string(),
            "move".to_string(),
            "UIDPLUS".to_string(),
            "Special-Use".to_string(),
        ];
        let caps = caps_from_atoms(&atoms);
        assert!(caps.move_);
        assert!(caps.uidplus);
        assert!(caps.special_use);
        // `SEARCH` 是 IMAP4rev1 的必备能力，多数服务端不单列 → 靠 IMAP4rev1 判定。
        assert!(caps.server_search);
        // 没声明 IDLE 就是没有。
        assert!(!caps.idle);

        let bare = caps_from_atoms(&["IDLE".to_string()]);
        assert!(bare.idle);
        assert!(!bare.move_);
        assert!(!bare.server_search, "无 IMAP4rev1 / SEARCH 时应为 false");
    }

    #[test]
    fn list_attributes_keep_their_backslash() {
        // ⚠️ 这条守卫的是「同步 0 个文件夹」那个 bug：`imap-proto` 把 `\Sent` 解析成
        // 枚举变体，若用 `{:?}` 当字符串就会退化成 `Sent`，与
        // `FolderRole::from_imap` 的 `\Sent` 比对永失配 → 全部落 `Other` → 不同步。
        use async_imap::types::NameAttribute as A;
        // 旧实现用 `{:?}` 当属性串，这就是根因：枚举变体名**不带反斜杠**。
        assert_eq!(format!("{:?}", A::Sent), "Sent");
        assert_eq!(attribute_wire_form(&A::Sent), "\\Sent");
        assert_eq!(attribute_wire_form(&A::Drafts), "\\Drafts");
        assert_eq!(attribute_wire_form(&A::Trash), "\\Trash");
        assert_eq!(attribute_wire_form(&A::Junk), "\\Junk");
        assert_eq!(attribute_wire_form(&A::Archive), "\\Archive");
        assert_eq!(attribute_wire_form(&A::All), "\\All");
        assert_eq!(attribute_wire_form(&A::Flagged), "\\Flagged");
        // `\Noselect` 是 `selectable` 判据，丢了反斜杠会让不可选容器混进界面。
        assert_eq!(attribute_wire_form(&A::NoSelect), "\\Noselect");
        assert_eq!(attribute_wire_form(&A::NoInferiors), "\\Noinferiors");
        // 未知属性（`Extension`）的载荷**已含**反斜杠，原样透传且不重复加。
        assert_eq!(
            attribute_wire_form(&A::Extension("\\X-MyFlag".into())),
            "\\X-MyFlag"
        );
        // 端到端：还原出的属性串必须能被角色判定认出来。
        assert_eq!(
            crate::models::FolderRole::from_imap("&XfJT0ZAB-", &[attribute_wire_form(&A::Sent)]),
            crate::models::FolderRole::Sent
        );
    }

    #[test]
    fn parse_section_path_accepts_dotted_sections_only() {
        assert_eq!(parse_section_path("2").unwrap(), vec![2]);
        assert_eq!(parse_section_path("1.2").unwrap(), vec![1, 2]);
        assert_eq!(parse_section_path("3.1.4").unwrap(), vec![3, 1, 4]);
        // 空 / 非数字段一律拒绝——不把可疑值发给服务器。
        assert!(parse_section_path("").is_err());
        assert!(parse_section_path("1..2").is_err());
        assert!(parse_section_path("a").is_err());
        assert!(parse_section_path("1.2.x").is_err());
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
            auth: AuthKind::Password,
        };
        let rt = crate::runtime::MailRuntime::new().unwrap();
        let report = rt.block_on(probe(cfg));
        assert!(!report.ok);
        assert!(report.error.is_some());
        assert_eq!(report.folder_count, 0);
    }
}
