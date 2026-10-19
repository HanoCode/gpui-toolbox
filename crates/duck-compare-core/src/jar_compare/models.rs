//! jar 比对的数据结构。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/jar_compare/models.rs`。三处裁剪：
//!
//! - `JarCompareResult` 去掉 `compare_key` / `compare_time` / `diff_desc`：前两个是
//!   SQLite 行的标识（这里不落库，见 [`super`] 的模块头），最后一个是界面文案（见
//!   [`super::super::class_compare::models::CompareResult`] 的同一段说明）。
//! - `ScanProgress` / `JarCompareHistory` 不进内核：进度是 [`super::Progress`]，
//!   历史归宿主层。

use serde::{Deserialize, Serialize};

use crate::CompareVerdict;

/// JAR 内一个 `.class` 的版本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JarClassInfo {
    pub class_path: String,
    pub version: String,
    pub class_name: String,
    pub package_name: String,
}

/// 一个 JAR 的完整画像（Phase 2 打开 ZIP 扫完才有的东西）。
#[derive(Debug, Clone)]
pub struct JarInfo {
    pub env_name: String,
    pub jar_name: String,
    pub jar_path: String,
    /// 相对扫描根的路径，例如 `lib/foo.jar`、`server/lcpt-xxx/lib/bar.jar`。
    /// **配对靠它**，不靠文件名——同名 JAR 在不同服务下是完全不同的东西。
    pub jar_rel_path: String,
    /// 从路径推断的服务名（`lib` 或 `server/<name>` 里的 `<name>`）。
    pub server_name: String,
    pub file_size: u64,
    pub classes: Vec<JarClassInfo>,
    /// JAR 级版本号：内部 `.class` 版本里出现次数最多的那个（见
    /// [`super::scanner::JarScanner`]）。
    pub jar_version: String,
}

/// 一条比对结论。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JarCompareResult {
    pub jar_name: String,
    pub server_name: String,
    pub jar_path_base: String,
    pub jar_path_target: String,
    pub class_count_base: usize,
    pub class_count_target: usize,
    pub version_base: String,
    pub version_target: String,
    pub compare_type: CompareVerdict,
    /// 口径同 class 比对：只有版本回退算异常。
    pub is_abnormal: bool,
}

/// 一次 jar 比对的全部产出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JarCompareReport {
    pub results: Vec<JarCompareResult>,
    /// 基准侧扫到的 JAR 数。
    pub base_listed: usize,
    pub target_listed: usize,
}

impl JarCompareReport {
    pub fn count(&self, verdict: CompareVerdict) -> usize {
        self.results
            .iter()
            .filter(|result| result.compare_type == verdict)
            .count()
    }

    pub fn abnormal(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.is_abnormal)
            .count()
    }
}
