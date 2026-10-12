//! 错误模型：把客户端、凭据与档案校验的失败归一成可显示的枚举。
//!
//! `Display` 是英文（本仓日志与错误约定），界面文案由调用方用 `tr` 组织。

use std::fmt;

use redis::ErrorKind;

/// Redis 操作失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedisError {
    /// 连接档案不完整或取值非法（缺主机、端口越界、分隔符为空等）。
    Config(String),
    /// 凭据存取失败（凭据文件读写出错、加解密失败等）。
    Secret(String),
    /// 建立连接失败（网络、认证、TLS 协商）。
    Connect(String),
    /// 命令执行失败（服务端返回错误、响应类型不符）。
    Command(String),
    /// 当前连接 / 服务端不支持请求的能力。
    Unsupported(String),
}

impl fmt::Display for RedisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(detail) => write!(f, "invalid connection config: {detail}"),
            Self::Secret(detail) => write!(f, "secure storage failed: {detail}"),
            Self::Connect(detail) => write!(f, "connection failed: {detail}"),
            Self::Command(detail) => write!(f, "command failed: {detail}"),
            Self::Unsupported(detail) => write!(f, "not supported: {detail}"),
        }
    }
}

impl std::error::Error for RedisError {}

impl From<duck_platform::secret::SecretError> for RedisError {
    fn from(err: duck_platform::secret::SecretError) -> Self {
        Self::Secret(err.to_string())
    }
}

impl From<redis::RedisError> for RedisError {
    /// 按 redis-rs 的 [`ErrorKind`] 归类。
    ///
    /// 建连与命令共用同一个错误类型，所以这里按 kind 粗分：配置 / 认证 / IO
    /// 三类归 `Config` 与 `Connect`，其余（服务端应答错误、类型不符、脚本失败）归 `Command`。
    /// 文案保留原样，排查时仍能看到服务端的原始应答。
    ///
    /// `ErrorKind` 是 `#[non_exhaustive]`（上游会继续加变体），所以末尾必须有通配分支。
    fn from(err: redis::RedisError) -> Self {
        let text = err.to_string();
        match err.kind() {
            ErrorKind::InvalidClientConfig => Self::Config(text),
            ErrorKind::AuthenticationFailed | ErrorKind::Io => Self::Connect(text),
            _ => {
                // 连接类失败有时以服务端应答的形态出现（代理 / 网关回一段错误文本），
                // 所以再用谓词与关键词兜一层，避免把「连不上」报成「命令失败」。
                if err.is_io_error()
                    || err.is_connection_refusal()
                    || err.is_connection_dropped()
                    || err.is_timeout()
                    || is_network_text(&text)
                {
                    Self::Connect(text)
                } else {
                    Self::Command(text)
                }
            }
        }
    }
}

/// 报错文本是否像网络 / 认证层面的失败。
///
/// 只看小写化后的几个稳定关键词——redis-rs 的 IO 错误里嵌的是操作系统原文，
/// 不同平台措辞不同，这里不求全，只把最常见的一批捞出来。
fn is_network_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "connection refused",
        "failed to connect",
        "connection reset",
        "timed out",
        "timeout",
        "broken pipe",
        "no route to host",
        "name or service not known",
        "nodename nor servname",
        "dns",
        "wrongpass",
        "invalid password",
        "without any password",
        "auth",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误文本分类：网络 / 认证类归 `Connect`，服务端应答类归 `Command`。
    ///
    /// `Display` 文案是英文（本仓日志约定），只断言前缀，不锁整句。
    #[test]
    fn error_display_prefixes() {
        assert!(
            RedisError::Config("x".into())
                .to_string()
                .starts_with("invalid connection config")
        );
        assert!(
            RedisError::Secret("x".into())
                .to_string()
                .starts_with("secure storage failed")
        );
        assert!(
            RedisError::Connect("x".into())
                .to_string()
                .starts_with("connection failed")
        );
        assert!(
            RedisError::Command("x".into())
                .to_string()
                .starts_with("command failed")
        );
        assert!(
            RedisError::Unsupported("x".into())
                .to_string()
                .starts_with("not supported")
        );
    }

    /// 网络关键词识别（不依赖 redis-rs 的构造，直接测谓词）。
    #[test]
    fn network_text_detection() {
        assert!(is_network_text("Connection refused (os error 61)"));
        assert!(is_network_text("Operation timed out"));
        assert!(is_network_text("WRONGPASS invalid username-password pair"));
        assert!(!is_network_text("ERR unknown command 'FOO'"));
        assert!(!is_network_text(
            "WRONGTYPE Operation against a key holding the wrong kind of value"
        ));
    }
}
