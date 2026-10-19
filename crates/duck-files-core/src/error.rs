//! 统一错误：**稳定 ASCII 码 + 可选细节**。
//!
//! 内核不产出给用户看的整句（那会把中文句子焊进二进制），只给一个稳定码
//! （[`ErrorKind`]）与一段细节（文件路径、正则报错原文）。插件层拿码去查自己的
//! 译文表，再拼上细节 —— 这样 `locales/en.toml` 里只需要一句「路径不是目录」，
//! 不必为每个路径单独登记一行。
//!
//! 参考仓是直接 `format!("读取文件失败: {}", e)` 返回 `String`，插件层没有可翻译的
//! 抓手；这里是本族唯一一处**主动偏离**，理由就是 i18n（判定与文案内容不变）。

use std::fmt;

/// 错误种类。**码值是约定**：插件层按它查译文，改名等于改协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 路径不存在。
    PathMissing,
    /// 路径存在但不是目录。
    NotADirectory,
    /// 路径存在但不是文件。
    NotAFile,
    /// 路径含 `..`（参考仓 `validate_path` 的拒绝口径）。
    IllegalPath,
    /// 命中参考仓的禁访系统路径清单（`/etc/shadow` 等）。
    BlockedPath,
    /// 不是 UTF-8 文本。
    NotUtf8,
    /// 正则（或通配）写错了。
    BadPattern,
    /// 输入为空（没给目录 / 没给关键词 / 粘贴的 CSV 是空的）。
    EmptyInput,
    /// 格式不支持（扩展名不在矩阵内、输出格式组合没有实现）。
    Unsupported,
    /// 文件超过大小上限。
    TooLarge,
    /// 重名冲突（改名目标已存在 / 同一批里两个文件改到同一个名字）。
    Conflict,
    /// 文件系统错误（读、写、建目录）。
    Io,
    /// 撤销日志缺失、损坏，或要撤销的动作已经撤过了。
    UndoUnavailable,
    /// 结构化配置写错了（数据格式转换的列宽表、固定分隔符配置）。
    BadConfig,
}

impl ErrorKind {
    /// 稳定码。插件层用它查译文；**不要**为了「更顺口」改这些字符串。
    pub fn code(self) -> &'static str {
        match self {
            Self::PathMissing => "path_missing",
            Self::NotADirectory => "not_a_directory",
            Self::NotAFile => "not_a_file",
            Self::IllegalPath => "illegal_path",
            Self::BlockedPath => "blocked_path",
            Self::NotUtf8 => "not_utf8",
            Self::BadPattern => "bad_pattern",
            Self::EmptyInput => "empty_input",
            Self::Unsupported => "unsupported",
            Self::TooLarge => "too_large",
            Self::Conflict => "conflict",
            Self::Io => "io",
            Self::UndoUnavailable => "undo_unavailable",
            Self::BadConfig => "bad_config",
        }
    }
}

/// 内核错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesError {
    kind: ErrorKind,
    detail: Option<String>,
}

impl FilesError {
    pub fn new(kind: ErrorKind) -> Self {
        Self { kind, detail: None }
    }

    /// `detail` 收 [`std::fmt::Display`]：调用点最常用的是 `path.display()`，
    /// 它只能 `to_string()`、不能 `into()`。
    pub fn with_detail(kind: ErrorKind, detail: impl std::fmt::Display) -> Self {
        Self {
            kind,
            detail: Some(detail.to_string()),
        }
    }

    /// I/O 错误的统一包装：`kind` 保留语义（读失败 / 写失败都算 `Io`），
    /// 细节带上 `std::io::Error` 的原文。
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

impl fmt::Display for FilesError {
    /// 只用于日志与测试断言：`<code>: <detail>`。给人看的句子在插件层。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{}: {detail}", self.kind.code()),
            None => write!(f, "{}", self.kind.code()),
        }
    }
}

impl std::error::Error for FilesError {}

/// 内核的 `Result` 简写。
pub type FilesResult<T> = Result<T, FilesError>;

/// 参考仓 `file_ops::validate_path` / `batch_replace::validate_path` 的拒绝口径。
///
/// 两侧各有一份几乎相同的实现（`..` 与系统路径黑名单），这里合成一份：
/// - 反斜杠先归一到 `/`（Windows 路径也能被黑名单命中）；
/// - 拒绝任何 `..` 分量（不做「先规范化再判断」，规范化会把逃逸藏起来）。
pub fn validate_path(path: &str) -> FilesResult<()> {
    let normalized = path.replace('\\', "/");
    if normalized.contains("..") {
        return Err(FilesError::new(ErrorKind::IllegalPath));
    }
    const BLOCKED: [&str; 4] = ["/etc/shadow", "/etc/passwd", "/proc/", "/sys/"];
    for blocked in BLOCKED {
        if normalized.starts_with(blocked) {
            return Err(FilesError::with_detail(ErrorKind::BlockedPath, blocked));
        }
    }
    Ok(())
}

/// 用户给的路径 → 存在的目录。空串按 [`ErrorKind::EmptyInput`] 报。
pub fn require_dir(path: &str) -> FilesResult<std::path::PathBuf> {
    if path.trim().is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }
    validate_path(path)?;
    let path = std::path::Path::new(path);
    if !path.exists() {
        return Err(FilesError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    if !path.is_dir() {
        return Err(FilesError::with_detail(
            ErrorKind::NotADirectory,
            path.display(),
        ));
    }
    Ok(path.to_path_buf())
}

/// 用户给的文件路径 → 存在的文件。
pub fn require_file(path: &str) -> FilesResult<std::path::PathBuf> {
    if path.trim().is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }
    validate_path(path)?;
    let path = std::path::Path::new(path);
    if !path.exists() {
        return Err(FilesError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    if !path.is_file() {
        return Err(FilesError::with_detail(ErrorKind::NotAFile, path.display()));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape_and_system_paths() {
        assert_eq!(
            validate_path("/tmp/../etc/passwd").unwrap_err().kind(),
            ErrorKind::IllegalPath
        );
        assert_eq!(
            validate_path("C:\\Windows\\..\\x").unwrap_err().kind(),
            ErrorKind::IllegalPath
        );
        assert_eq!(
            validate_path("/etc/passwd").unwrap_err().kind(),
            ErrorKind::BlockedPath
        );
        assert!(validate_path("/tmp/ok").is_ok());
    }

    #[test]
    fn empty_path_is_empty_input_not_missing() {
        // 空串与「路径不存在」是两件事：前者用户没填，后者填错了。
        assert_eq!(require_dir("  ").unwrap_err().kind(), ErrorKind::EmptyInput);
        assert_eq!(
            require_dir("/definitely/not/here").unwrap_err().kind(),
            ErrorKind::PathMissing
        );
    }

    #[test]
    fn codes_are_stable_ascii() {
        for kind in [
            ErrorKind::PathMissing,
            ErrorKind::NotADirectory,
            ErrorKind::NotAFile,
            ErrorKind::IllegalPath,
            ErrorKind::BlockedPath,
            ErrorKind::NotUtf8,
            ErrorKind::BadPattern,
            ErrorKind::EmptyInput,
            ErrorKind::Unsupported,
            ErrorKind::TooLarge,
            ErrorKind::Conflict,
            ErrorKind::Io,
            ErrorKind::UndoUnavailable,
        ] {
            assert!(kind.code().is_ascii(), "{:?}", kind);
        }
    }
}
