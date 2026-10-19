//! Java `class` 文件解析与版本号提取。
//!
//! 本 crate 只做一件事：**在不反编译的前提下**读出 `.class` 里的版本号，并提供
//! 版本号比较。合并族（`duck-merge-core`）判「同名文件该留哪一份」时用的就是它：
//! 先看版本号、再看修改日期（见 `non_sp_merge::decide_conflict`）。
//!
//! 为什么单独一个 crate 而不是塞进合并核心里：比对族（`duck.compare.*` 的 class /
//! jar 比对）与「Java 反编译」工具将来都要用同一份解析器，塞进 `duck-merge-core`
//! 会让那两个工具的依赖关系变成「为了读一个 class 而依赖合并全家桶」。
//!
//! ## 移植口径（复刻不复研）
//!
//! 三个文件全部来自参考仓 `ifm_ai_tools/src-tauri/src/`，逐字复制：
//!
//! | 本 crate | 参考仓 | 行数 | 改动 |
//! | --- | --- | --- | --- |
//! | [`classfile`] | `decompiler.rs` 第 1–399 行 | 399 | 只取到 `parse_class_file` 结束 |
//! | [`version_compare`] | `class_compare/version_compare.rs` | 89 | 无（含 6 条上游单测） |
//! | [`version_extractor`] | `class_compare/version_extractor.rs` | 167 | `once_cell::Lazy` → `std::sync::LazyLock` |
//!
//! `classfile` 之所以只切前 399 行：反编译主体（`decompile_block_range` 等）依赖
//! `cfg_decompiler`，而 `decompile_class_enhanced` 还拉 `ristretto_classfile` 这个
//! 重依赖——版本提取一个都用不上。等「Java 反编译」工具真正落地时，把剩余部分
//! 整段搬进本 crate 即可，**已经进仓的 399 行不需要再动**。
//!
//! `once_cell` 换成 `std::sync::LazyLock` 是唯一的代码改动：本仓不引 `once_cell`
//! （MSRV 1.85 已有 `LazyLock`，1.80 稳定），语义完全相同。
//!
//! 本 crate 零 gpui、零 tokio、零 IO（只吃 `&[u8]`）。

pub mod version_compare;
pub mod version_extractor;

// 解析器是一次性整段照抄的：本 crate 目前只用到 `parse_class_file`，其余
// `read_*` / `ConstantPool` 辅助函数暂时无人调用。**不要**为了让告警消失去删它们
// ——它们正是反编译功能落地时要用的那一半。
#[allow(dead_code)]
pub mod classfile;

pub use version_compare::compare;
pub use version_extractor::VersionExtractor;
