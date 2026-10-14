//! 错误类型。**错误详情一律英文**（i18n 守卫扫描整个 `crates/` 的中文字面量；
//! 面向用户的文案由 `duck-app` 侧取词包裹）。

use std::fmt;

/// 内核层错误。
#[derive(Debug)]
pub enum ProjectError {
    /// 数据目录创建 / 文件操作失败。
    Io(std::io::Error),
    /// SQL 执行失败。
    Sqlite(rusqlite::Error),
    /// 输入不合法（状态串 / 优先级越界 / 日期串解析失败 / 进度越界等）。
    Invalid(String),
    /// 目标行不存在（更新、删除、恢复时按主键找不到）。
    NotFound(&'static str),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "project I/O failed: {error}"),
            Self::Sqlite(error) => write!(f, "project sqlite failed: {error}"),
            Self::Invalid(detail) => write!(f, "invalid input: {detail}"),
            Self::NotFound(what) => write!(f, "{what} not found"),
        }
    }
}

impl std::error::Error for ProjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Invalid(_) | Self::NotFound(_) => None,
        }
    }
}

impl From<std::io::Error> for ProjectError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for ProjectError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// 本 crate 的 `Result` 别名。
pub type Result<T> = std::result::Result<T, ProjectError>;
