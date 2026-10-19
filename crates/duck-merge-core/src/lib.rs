//! 上线包合并族（`duck.merge.*`）的核心逻辑。
//!
//! 与 `duck-sql-core` 同一套移植规矩（**复刻不复研**）：参考仓
//! `ifm_ai_tools/src-tauri/src/` 里已经生产验证过的 Rust 实现整段复制进来，
//! 只改模块头与对外签名，**不许改逻辑、不许用 JS 重写**。
//!
//! 与 Tauri 的耦合只有三处：`AppHandle + Emitter` 的 `emit_*`、`std::thread::spawn`
//! 入口，以及 `CANCEL_FLAG` 的置位命令。移植时前两者换成
//! [`sp_merge::PkgMergeReporter`] 与同步函数（线程由插件进程自己开，见
//! `crates/duck-merge-tool`），取消改由宿主的进程管理负责（见 `sp_merge` 模块头）。
//!
//! 本 crate 零 gpui、零 tokio、全同步。
//!
//! 模块一览：
//! - [`manifest_extract`]：按清单提取文件（`extract_by_manifest.rs`，503 行）；
//! - [`merge_common`]：目录 / ZIP / 修改时间的共用基础能力（639 行）；
//! - [`upd_merge`]：需求单 `.upd` 说明文件追加合并（552 行）；
//! - [`sp_merge`]：上线包合并主线（SP 3.0/4.0、IFMS6，1,460 行）；
//! - [`non_sp_merge`]：非 SEE 上线包合并（1,936 行）；
//! - [`ifms6_merge`]：IFMS6.0 前端 / 服务端 / 库包分块合并（2,037 行）；
//! - [`scc_merge`]：SCC 客户包合并（555 行）。
//!
//! `merge_common` / `upd_merge` / `sp_merge` / `non_sp_merge` / `ifms6_merge` /
//! `scc_merge` 六个复刻件**不参与 rustfmt**：它们要能与参考仓逐行 `diff`（一次就
//! 知道有没有人偷改逻辑），重排两千多行只会让这份对比失效。`manifest_extract.rs`
//! 因为换了三处接线，本来就是新写的，照常格式化。

pub mod manifest_extract;
#[rustfmt::skip]
pub mod merge_common;
// `non_sp_merge` 判同名文件留哪份，靠的是 `duck-classfile` 里那两个复刻件
// （参考仓的 `class_compare/mod`）；本 crate 不再自留一份。
#[rustfmt::skip]
pub mod non_sp_merge;
// 与源仓 `mod tests` 同源的 12 条单测原样保留在文件末尾：它们是这批纯函数的
// 行为锁，等于免费继承上游的回归覆盖。
#[rustfmt::skip]
pub mod sp_merge;
#[rustfmt::skip]
pub mod upd_merge;
// 这两条路径与 `non_sp_merge` 共用同一个回执 trait（见下）：源仓的 progress 载荷
// 都只有「阶段 + 百分比 + 一句话」，没有 `sp_merge` 的 `stage_duration_ms`。
#[rustfmt::skip]
pub mod ifms6_merge;
#[rustfmt::skip]
pub mod scc_merge;
// SEE 分包合并：进度载荷与 `sp_merge` 六格全同，直接复用它的 `PkgMergeReporter`。
#[rustfmt::skip]
pub mod see_merge;

/// 合并过程的对外回执（源仓 `emit_progress` / `emit_log` 的替身）。
///
/// 与 [`sp_merge::PkgMergeReporter`] 的区别：那个是 `sp_merge` 的**原始载荷**
/// （带 `stage_duration_ms`，且 `current/total` 与 `current_file` 分列），这个给
/// `non_sp_merge` / `ifms6_merge` / `scc_merge` 用——三者的差异只在 progress 的
/// 说明字段：
///
/// | 模块 | 源仓 progress 载荷 |
/// | --- | --- |
/// | `non_sp_merge` | `stage / percent / current / total / current_file` |
/// | `ifms6_merge` | `stage / percent / message` |
/// | `scc_merge` | `stage / percent / message` |
///
/// 所以这里取并集：`non_sp_merge` 填 `current/total` 与 `message`（它的
/// `current_file` 就是那句话），后两者只填 `message`。落地端只认 `message`。
/// 三者都不需要 `done` 回执——结果直接当返回值返回，插件自己拼结果正文。
pub trait MergeReporter {
    /// 阶段进度。`stage` 是源仓的英文阶段名，`message` 是那一行的说明（可能没有）。
    fn progress(
        &self,
        stage: &str,
        percent: u32,
        current: usize,
        total: usize,
        message: Option<String>,
    );
    /// 一行过程日志。`level`：`info` / `warn` / `error` / `success`。
    fn log(&self, level: &str, message: &str, duration_ms: Option<u64>);
}
