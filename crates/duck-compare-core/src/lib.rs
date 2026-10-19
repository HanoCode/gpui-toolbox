//! 比对族内核（`crates/duck-compare-core`）：**只做比对，不碰界面**。
//!
//! 四块能力，各自对应参考仓一个已验证的实现：
//!
//! - [`text_diff`]：两段文本的逐行差异 + 行内字符级改动（`file_ops::compute_diff`）；
//! - [`dir_compare`]：两个目录的清单比对（`file_ops::compare_directories`），
//!   以及目录树读取（`list_directory` / `read_directory_tree`，`devkit.files` 族要用）；
//! - [`class_compare`]：两个应用目录里同名 `.class` 的版本高低（`class_compare/`）；
//! - [`jar_compare`]：同上，但比对单位是 JAR 包（`jar_compare/`）。
//!
//! 全部是**逐字复刻**：参考仓这些逻辑已经在真实交付里跑过，重写等于把「算得对不对」
//! 再验证一遍。本 crate 只做与宿主有关的裁剪——去掉 `#[tauri::command]`、去掉 emit、
//! 去掉 SQLite 落库（插件进程一次调用一次进程，落库没有读者），结构体字段改 `pub`。
//!
//! 两个跨族共用的小件：[`CompareVerdict`]（五档判决，插件层据此只写一份文案）与
//! [`progress`]（扫描阶段，结构化上报，见该模块头）。
//!
//! 后续的 `config_diff` 也进这个 crate——它比的是**配置文件**，但同样零 gpui、零 tauri。

pub mod class_compare;
pub mod config_diff;
pub mod dir_compare;
pub mod jar_compare;
pub mod progress;
pub mod text_diff;
pub mod verdict;

#[cfg(test)]
mod testkit;

pub use progress::{Progress, Stage};
pub use verdict::CompareVerdict;
