//! 错误模型：把三条驱动与凭据层的失败归一成可显示的枚举。
//!
//! `Display` 是英文（本仓日志与错误约定），界面文案由调用方用 `tr` 组织。

use std::fmt;

/// 数据库操作失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// 连接档案不完整或取值非法（缺主机、SQLite 路径为空等）。
    Config(String),
    /// 凭据存取失败（凭据文件读写出错、加解密失败等）。
    Secret(String),
    /// 建立连接失败（网络、认证、TLS 协商）。
    Connect(String),
    /// 语句执行失败。
    Query(String),
    /// 语句被取消（用户点「停止」或超时守护到点）。
    ///
    /// 引擎只能报「这条语句被中止了」，分不清是谁下的手——手动停还是超时由 UI 侧
    /// 按自己是否挂了 [`crate::cancel::CancelTimer`] 决定文案。
    Cancelled(String),
    /// 当前引擎不支持请求的能力（如 M1 的 PostgreSQL TLS）。
    Unsupported(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(detail) => write!(f, "invalid connection config: {detail}"),
            Self::Secret(detail) => write!(f, "secure storage failed: {detail}"),
            Self::Connect(detail) => write!(f, "connection failed: {detail}"),
            Self::Query(detail) => write!(f, "query failed: {detail}"),
            Self::Cancelled(detail) => write!(f, "query cancelled: {detail}"),
            Self::Unsupported(detail) => write!(f, "not supported: {detail}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<duck_platform::secret::SecretError> for DbError {
    fn from(err: duck_platform::secret::SecretError) -> Self {
        Self::Secret(err.to_string())
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(err: rusqlite::Error) -> Self {
        // `sqlite3_interrupt` 的落点是 `SQLITE_INTERRUPT`；它也是「超时守护」那条路径
        // 唯一的可观测信号，故单独归一（见 `cancel.rs`）。
        if let rusqlite::Error::SqliteFailure(code, _) = &err
            && code.code == rusqlite::ErrorCode::OperationInterrupted
        {
            return Self::Cancelled(err.to_string());
        }
        match &err {
            rusqlite::Error::SqliteFailure(..) | rusqlite::Error::SqlInputError { .. } => {
                Self::Query(err.to_string())
            }
            _ => Self::Query(err.to_string()),
        }
    }
}

impl From<postgres::Error> for DbError {
    fn from(err: postgres::Error) -> Self {
        // `cancel_query` 送达后服务器回 57014（query_canceled）。
        if err.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) {
            return Self::Cancelled(err.to_string());
        }
        // 建连与执行共用一个错误类型，按错误码的常见形态粗分：连接阶段失败
        // （DNS / 拒绝 / 认证）归 `Connect`，其余归 `Query`。文案保留原样供排查。
        let text = err.to_string();
        if text.contains("could not connect")
            || text.contains("authentication")
            || text.contains("password authentication")
            || text.contains("timeout")
        {
            Self::Connect(text)
        } else {
            Self::Query(text)
        }
    }
}

impl From<mysql::Error> for DbError {
    fn from(err: mysql::Error) -> Self {
        // `KILL QUERY` 的落点：1317 `ER_QUERY_INTERRUPTED`（被 kill）；
        // 3024 `ER_QUERY_TIMEOUT`（`max_execution_time` 到点）。两者都是「被中止」。
        if let mysql::Error::MySqlError(server) = &err
            && matches!(server.code, 1317 | 3024)
        {
            return Self::Cancelled(err.to_string());
        }
        let text = err.to_string();
        if text.contains("Connection refused")
            || text.contains("failed to connect")
            || text.contains("Access denied")
            || text.contains("timed out")
            || text.contains("Can't resolve")
        {
            Self::Connect(text)
        } else {
            Self::Query(text)
        }
    }
}
