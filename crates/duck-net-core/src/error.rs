//! 统一错误：**稳定 ASCII 码 + 可选细节**。
//!
//! 与 `duck-files-core::error` 同一套约定（那边有完整理由）：内核不产出给用户看的
//! 整句，只给稳定码（[`ErrorKind`]）与一段细节（主机:端口、HTTP 状态、响应原文）。
//! 插件层拿码查自己的译文表，再拼上细节 —— 译文表里只需要「连接失败」一句，
//! 不必为每个端点各登记一行。
//!
//! 参考仓是 `format!("FTP 连接 {}:{} 失败: {}", …)` 直接拼中文，前端拿到的就是成品
//! 字符串；本仓要过 i18n 这一关，所以只保留**码 + 细节**这一个偏离。

use std::fmt;

/// 错误种类。**码值是约定**：插件层按它查译文，改名等于改协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 输入为空（没给 URL / 主机 / 报文 / hosts 内容）。
    EmptyInput,
    /// URL 写错了（缺 scheme、缺 host、端口不是数字）。
    BadUrl,
    /// 请求头写错了（没有冒号、名字为空）。
    BadHeader,
    /// 键值行写错了（`query` / `vars` / `form` 每行必须 `键=值`）。
    BadPair,
    /// `{{变量}}` 没有对应的值。
    Variable,
    /// 不支持的协议 / 方法 / 编解码。
    Unsupported,
    /// 超时（连接、读、写，或整体）。
    Timeout,
    /// 域名解析不出来。
    Dns,
    /// TCP 连不上（拒绝、不可达、端口关闭）。
    Connect,
    /// TLS 握手或证书失败。
    Tls,
    /// 对端返回的 HTTP 状态是 4xx / 5xx（细节带状态码与状态行）。
    Http,
    /// 认证失败（FTP 530、S3 403、SFTP 认证不过）。
    Auth,
    /// 报文编解码失败（长度字段越界、头部不是 ASCII 数字、帧不完整）。
    Decode,
    /// 数据超过上限（响应体、报文、hosts 文件）。
    TooLarge,
    /// 文件系统错误（读 hosts、写产物）。
    Io,
    /// 路径不存在 / 不是文件（读 hosts 方案时）。
    PathMissing,
    /// 权限不足（写系统文件，S3 403 之外的 sudo 场景）。
    Elevation,
}

impl ErrorKind {
    /// 稳定码。插件层用它查译文；**不要**为了「更顺口」改这些字符串。
    pub fn code(self) -> &'static str {
        match self {
            Self::EmptyInput => "empty_input",
            Self::BadUrl => "bad_url",
            Self::BadHeader => "bad_header",
            Self::BadPair => "bad_pair",
            Self::Variable => "variable",
            Self::Unsupported => "unsupported",
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Http => "http",
            Self::Auth => "auth",
            Self::Decode => "decode",
            Self::TooLarge => "too_large",
            Self::Io => "io",
            Self::PathMissing => "path_missing",
            Self::Elevation => "elevation",
        }
    }
}

/// 内核错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError {
    kind: ErrorKind,
    detail: Option<String>,
}

impl NetError {
    pub fn new(kind: ErrorKind) -> Self {
        Self { kind, detail: None }
    }

    /// `detail` 收 [`std::fmt::Display`]：调用点最常用的是 `path.display()` /
    /// `format_args!`，它们只能 `to_string()`、不能 `into()`。
    pub fn with_detail(kind: ErrorKind, detail: impl fmt::Display) -> Self {
        Self {
            kind,
            detail: Some(detail.to_string()),
        }
    }

    /// I/O 错误的统一包装：细节带 `std::io::Error` 原文。
    pub fn io(error: std::io::Error) -> Self {
        Self::with_detail(ErrorKind::Io, error.to_string())
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl fmt::Display for NetError {
    /// 只用于日志与测试断言：`<code>: <detail>`。给人看的句子在插件层。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.kind.code()),
            None => write!(f, "{}", self.kind.code()),
        }
    }
}

impl std::error::Error for NetError {}

/// 内核的 `Result` 简写。
pub type NetResult<T> = Result<T, NetError>;

/// 用户给的文件路径 → 存在的文件（hosts 方案文件）。
pub fn require_file(path: &str) -> NetResult<std::path::PathBuf> {
    if path.trim().is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    let path = std::path::Path::new(path);
    if !path.exists() {
        return Err(NetError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    if !path.is_file() {
        return Err(NetError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_ascii() {
        for kind in [
            ErrorKind::EmptyInput,
            ErrorKind::BadUrl,
            ErrorKind::BadHeader,
            ErrorKind::BadPair,
            ErrorKind::Variable,
            ErrorKind::Unsupported,
            ErrorKind::Timeout,
            ErrorKind::Dns,
            ErrorKind::Connect,
            ErrorKind::Tls,
            ErrorKind::Http,
            ErrorKind::Auth,
            ErrorKind::Decode,
            ErrorKind::TooLarge,
            ErrorKind::Io,
            ErrorKind::PathMissing,
            ErrorKind::Elevation,
        ] {
            assert!(kind.code().is_ascii(), "{kind:?}");
        }
    }

    #[test]
    fn empty_path_is_empty_input_not_missing() {
        assert_eq!(
            require_file("  ").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            require_file("/definitely/not/here").unwrap_err().kind(),
            ErrorKind::PathMissing
        );
    }

    #[test]
    fn display_is_code_plus_detail_and_never_a_sentence() {
        let error = NetError::with_detail(ErrorKind::Connect, "10.0.0.1:80");
        assert_eq!(error.to_string(), "connect: 10.0.0.1:80");
        assert_eq!(NetError::new(ErrorKind::Timeout).to_string(), "timeout");
    }
}
