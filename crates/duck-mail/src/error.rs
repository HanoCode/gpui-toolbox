//! 邮件内核的错误模型。
//!
//! 与 `duck-db::DbError` 同口径：**手写枚举 + 英文 `Display`**（日志与内部调试用），
//! **不吐成品中文**——界面文案由 `duck-app` 用 `tr` 组织，或由内核给出「源串 +
//! 参数」交宿主翻译（照 `SshError::template()` 的范式，见 AGENTS §3.3）。
//!
//! [`MailError::code`] 给出稳定的、语言无关的短标识，宿主可据此选本地化文案，
//! 不必匹配 `Display` 的字符串。
//!
//! ⚠️ **这条约定不是靠自觉**：`duck-theme` 的守卫
//! `every_chinese_literal_is_translated` 会扫本 crate 的每一个字符串字面量，本 crate
//! 里没有 EN 表可登记，所以任何中文详情都会让 `cargo test -p duck-theme` 直接红。
//! 唯一豁免的是 [`crate::models`] 的 IMAP 文件夹别名表（`NON_UI_SOURCES`，理由是
//! 「一个字节都不会渲染」）；错误详情**不在**豁免范围——它会经
//! `error.to_string()` 进通知条。

use std::fmt;

/// 邮箱操作失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    /// 账户档案不完整或取值非法（缺主机、端口为 0、邮箱格式错等）。
    Config(String),
    /// 本地 SQLite 库读写失败。
    Db(String),
    /// 文件系统失败（附件目录不可写等）。
    Io(String),
    /// 凭据存取失败（保管箱不可用、密文损坏）。
    Secret(String),
    /// 建立连接失败（网络不通、TLS 协商失败、端口拒绝）。
    Connect(String),
    /// 认证失败（口令错、OAuth 令牌过期且刷新失败）。
    Auth(String),
    /// 协议层失败（服务端拒绝命令、响应不可解析）。
    Protocol(String),
    /// 协议操作超时。**带外取消不走这条**（那是 [`MailError::Other`] 里的取消语义）。
    Timeout(String),
    /// MIME 解析失败（结构损坏、字符集未知）。
    Mime(String),
    /// 服务端不支持请求的能力（如不支持 `IDLE` / `MOVE`）。
    Unsupported(String),
    /// 找不到对象（账户 / 文件夹 / 邮件已被删除）。
    NotFound(String),
    /// 账户需要重新认证（令牌失效）。
    NeedsReauth,
    /// 当前离线，动作已排队。
    Offline,
    /// 兜底。
    Other(String),
}

impl MailError {
    /// 稳定的、语言无关的错误标识。宿主用它选本地化文案。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Db(_) => "db",
            Self::Io(_) => "io",
            Self::Secret(_) => "secret",
            Self::Connect(_) => "connect",
            Self::Auth(_) => "auth",
            Self::Protocol(_) => "protocol",
            Self::Timeout(_) => "timeout",
            Self::Mime(_) => "mime",
            Self::Unsupported(_) => "unsupported",
            Self::NotFound(_) => "not_found",
            Self::NeedsReauth => "needs_reauth",
            Self::Offline => "offline",
            Self::Other(_) => "other",
        }
    }

    /// 该失败是否值得重试（网络类可重试，配置/认证类不可）。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Timeout(_) | Self::Offline)
    }
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(d) => write!(f, "invalid mail config: {d}"),
            Self::Db(d) => write!(f, "mail database error: {d}"),
            Self::Io(d) => write!(f, "mail io error: {d}"),
            Self::Secret(d) => write!(f, "credential storage failed: {d}"),
            Self::Connect(d) => write!(f, "connection failed: {d}"),
            Self::Auth(d) => write!(f, "authentication failed: {d}"),
            Self::Protocol(d) => write!(f, "imap protocol error: {d}"),
            Self::Timeout(d) => write!(f, "operation timed out: {d}"),
            Self::Mime(d) => write!(f, "mime parse error: {d}"),
            Self::Unsupported(d) => write!(f, "not supported: {d}"),
            Self::NotFound(d) => write!(f, "not found: {d}"),
            Self::NeedsReauth => write!(f, "account needs re-authentication"),
            Self::Offline => write!(f, "offline"),
            Self::Other(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for MailError {}

impl From<rusqlite::Error> for MailError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Db(err.to_string())
    }
}

impl From<std::io::Error> for MailError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<duck_platform::secret::SecretError> for MailError {
    fn from(err: duck_platform::secret::SecretError) -> Self {
        Self::Secret(err.to_string())
    }
}

/// 本 crate 的统一结果类型。
pub type Result<T> = std::result::Result<T, MailError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_stable_and_language_neutral() {
        assert_eq!(MailError::Auth("x".into()).code(), "auth");
        assert_eq!(MailError::NeedsReauth.code(), "needs_reauth");
        // code 只含 ASCII，宿主可直接拿去做文案键。
        assert!(MailError::Protocol("x".into()).code().is_ascii());
    }

    #[test]
    fn only_transport_failures_are_retryable() {
        assert!(MailError::Connect("timeout".into()).is_retryable());
        assert!(MailError::Timeout("idle".into()).is_retryable());
        // 认证与配置错误重试多少次都一样，不该进重试队列。
        assert!(!MailError::Auth("bad password".into()).is_retryable());
        assert!(!MailError::Config("missing host".into()).is_retryable());
    }

    #[test]
    fn display_is_english_for_logs() {
        let msg = MailError::Connect("connection refused".into()).to_string();
        assert!(msg.contains("connection failed"));
        assert!(!msg.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)));
    }
}
