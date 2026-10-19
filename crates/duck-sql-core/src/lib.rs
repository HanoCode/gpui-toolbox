//! SQL 工具族的**核心逻辑**（零 gpui、零 tokio、全同步）。
//!
//! 与 `duck-db` / `duck-project` 同层：只放纯计算，界面与进程协议都在外面。
//! 每个模块都**逐字复刻**参考仓 `ifm_ai_tools` 的后端实现（已在生产验证），
//! 移植时只加模块头、不改函数体——见 `docs/ifm-plugin-port-plan.md` §6.4。
//!
//! 谁用它：`crates/duck-sql-tool`（process 轨的插件进程，stdio JSONL 入口）。

pub mod ddl_generator;
// 参考仓全局错误类型的裁剪件（只留 `sql_safety` / `sql_rewriter` 用到的两个变体）。
pub mod error;
// 三个复刻件**不参与 rustfmt**：它们要能与参考仓逐行对比（`diff` 一次就知道有没有
// 人偷偷改过逻辑）。重排 2,000+ 行会让这份对比彻底失效，而收益只是缩进好看。
// 新写的代码（含 `ddl_generator.rs` 与 `sql_classifier.rs`）照常走 `cargo fmt`。
#[rustfmt::skip]
pub mod sql_checker;
// `sql_rewriter` 的「个性化 SQL 改写」：见包合并（`duck-merge-core::see_merge`）
// 解压后按客户号重写 show_config 语句时调用它（参考仓也是 `crate::sql_rewriter`）。
// 它不是独立工具的后端 —— 参考仓的 UI「SQL 改写」页对应的是两份纯前端算法，
// 已移植进 `plugins/sql-rewrite`。
#[rustfmt::skip]
pub mod sql_rewriter;
// 标识符校验 / 方言转义：参考仓 `sql_safety.rs`，`sql_rewriter` 依赖它。
#[rustfmt::skip]
pub mod sql_safety;
#[rustfmt::skip]
pub mod sql_merge;
// 移植自参考仓**前端**（`src/services/sqlClassifier.ts`，那份代码没有 Rust 后端）：
// 逐函数翻译而非重新设计，所以照常参与 rustfmt（没有「与上游逐行 diff」这回事）。
pub mod sql_classifier;
