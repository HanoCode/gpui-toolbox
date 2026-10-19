//! 参考仓全局错误类型（`ifm_ai_tools/src-tauri/src/error.rs`，72 行）的**裁剪件**。
//!
//! 只保留本 crate 两个复刻件（`sql_safety` / `sql_rewriter`）实际用到的变体，
//! `Display` 文案与参考仓**逐字一致**（「无效的 SQL 标识符: {}」/「SQL 注入风险: {}」）。
//!
//! 为什么不全搬：参考仓那个枚举的其余变体挂在 `rusqlite` / `reqwest` /
//! `thiserror` 上（`#[from]` 依赖），本 crate 零数据库、零网络，把它们拉进来只是
//! 为了一个错误类型，等于让纯函数 crate 背上一整条 IO 依赖链。
//!
//! `From<AppError> for String` 照抄：参考仓所有 `#[tauri::command]` 都靠它把
//! 错误交给前端，本仓 `sql_rewriter` 的调用方也按同一口径取字符串。

/// SQL 安全相关的两类拒绝理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    /// 标识符（表名 / 列名 / 排序方向）不合法。
    InvalidIdentifier(String),
    /// 语句里出现了不能直接拼进 SQL 的内容。
    SqlInjectionRisk(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::InvalidIdentifier(message) => write!(f, "无效的 SQL 标识符: {message}"),
            AppError::SqlInjectionRisk(message) => write!(f, "SQL 注入风险: {message}"),
        }
    }
}

impl std::error::Error for AppError {}

/// 转换为字符串错误（参考仓同名实现：`impl From<AppError> for String`）。
impl From<AppError> for String {
    fn from(error: AppError) -> Self {
        error.to_string()
    }
}
