//! 应用标准目录。全部位于用户级目录下，不写系统路径。

use std::path::{Path, PathBuf};

/// 应用目录名（macOS: `~/Library/Application Support/DuckToolbox`）。
const APP_DIR: &str = "DuckToolbox";

/// 用户数据目录：数据库、日志等需要长期保留的东西。
#[must_use]
pub fn app_data_dir() -> PathBuf {
    base(dirs::data_dir())
}

/// 配置目录：可手动编辑、可被用户删除重建的配置。
#[must_use]
pub fn config_dir() -> PathBuf {
    base(dirs::config_dir())
}

/// 缓存目录：可随时清空。
#[must_use]
pub fn cache_dir() -> PathBuf {
    base(dirs::cache_dir())
}

/// 日志目录。
#[must_use]
pub fn log_dir() -> PathBuf {
    app_data_dir().join("logs")
}

/// SQLite 数据库文件路径。
#[must_use]
pub fn db_path() -> PathBuf {
    app_data_dir().join("duck.sqlite3")
}

/// 单实例锁文件路径（`<应用数据目录>/<name>.lock`）。
#[must_use]
pub fn lock_path(name: &str) -> PathBuf {
    app_data_dir().join(format!("{name}.lock"))
}

/// 递归创建目录（已存在视为成功）。
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// 在平台标准目录下拼出应用目录；平台目录缺失时退到系统临时目录。
fn base(root: Option<PathBuf>) -> PathBuf {
    root.unwrap_or_else(std::env::temp_dir).join(APP_DIR)
}
