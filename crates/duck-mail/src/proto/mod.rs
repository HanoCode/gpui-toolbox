//! 协议层：IMAP（收信，M1）/ SMTP（发信）/ JMAP（M2）/ Gmail REST（M2）。
//!
//! # 为什么是枚举分发而不是 trait
//!
//! `melib` 用一个 19 方法的 `MailBackend` trait，因为要同时支持 maildir / mbox /
//! notmuch / IMAP / JMAP 等**多种本地与远端后端**。本模块只有「远端账号」一种形态，
//! 且 M1 只有 IMAP 一条通道——所以用枚举 + 能力位（照 `duck-db` §4.2 的裁决）：
//! 调用点 `match protocol { … }` 是穷尽的，加了新协议编译器会指出所有要改的地方，
//! 而 trait 会静默地被 `Box<dyn>` 藏起来。

pub mod imap;
pub mod smtp;
pub mod utf7;

use crate::models::{AuthKind, ConnectionSecurity};

/// 邮箱访问协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProtocol {
    /// IMAP4rev1（M1）。
    Imap,
    /// JMAP（M2，`feature = "jmap"`）。
    Jmap,
    /// Gmail 专用 REST（M2，`feature = "gmail"`）。
    Gmail,
}

/// 服务端能力位。同步引擎按这些位决定走哪条路径（如不支持 `MOVE` 就用
/// `COPY` + `STORE \Deleted` + `EXPUNGE` 三步走）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtocolCaps {
    /// 支持 `IDLE`（收信推送）。
    pub idle: bool,
    /// 支持 `MOVE` 扩展（RFC 6851）。
    pub move_: bool,
    /// 支持 `UIDPLUS`（`APPEND` 能回 UID）。
    pub uidplus: bool,
    /// 支持 `SPECIAL-USE`（文件夹带 `\Sent` 等旗标）。
    pub special_use: bool,
    /// 支持服务端 `SEARCH`（否则本地过滤）。
    pub server_search: bool,
}

/// IMAP 建连参数。
///
/// **含口令**——它只在「建连的那一瞬间」存在于内存里，不落任何持久化结构。
#[derive(Debug, Clone)]
pub struct ImapConfig {
    /// 主机。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 加密方式。
    pub security: ConnectionSecurity,
    /// 登录名。
    pub username: String,
    /// 凭据。口令档是口令，[`AuthKind::OAuth2`] 档是**访问令牌**。
    pub password: String,
    /// 认证方式：决定 `password` 是口令还是访问令牌，以及走 `LOGIN` 还是 `XOAUTH2`。
    pub auth: AuthKind,
}

impl ImapConfig {
    /// 常驻连接的超时（连接 / 登录 / 命令）。
    pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    /// 单条命令的超时。**必须有界**：把同步 actor 卡死在一台上吊的服务器上，
    /// 比同步失败严重得多（设计文档 §4.6）。
    pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
}
