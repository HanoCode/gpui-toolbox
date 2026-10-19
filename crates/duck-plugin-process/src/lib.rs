//! Duck Toolbox 插件独立进程执行轨（`process`）：stdio JSONL 协议 + runner。
//!
//! 与 JS 轨（[`duck_plugin_js`]）同层：零 gpui、零 tokio，纯 `std::process`
//! 加线程与 channel。协议与硬限制见 [`protocol`] 模块文档；线程模型与取消
//! 流程见 [`runner`] 模块文档。
//!
//! 分层：本 crate 只负责「进程跑完一次请求」——发现插件、能力仲裁、执行页
//! 都是宿主（`duck-app` / `duck-plugin`）的事。请求里的 `files`（per-run
//! 文件授权，ADR-3）与 `kv`（快照传入，ADR-4）由调用方组装好传进来。
//!
//! # Examples
//!
//! ```no_run
//! use duck_plugin_process::{CancelHandle, NullSink, RunRequest, run};
//! use std::path::Path;
//!
//! # fn example() -> Result<(), duck_plugin::PluginError> {
//! let request = RunRequest {
//!     tool: "dev.duck.sql.script-check".into(),
//!     input: String::new(),
//!     params: serde_json::json!({}),
//!     files: vec![],
//!     lang: "zh-Hans".into(),
//!     kv: Default::default(),
//!     action: None,
//! };
//! let outcome = run(
//!     Path::new("plugins/devkit.sql"),
//!     Path::new("plugins/devkit.sql/bin/script-check"),
//!     &request,
//!     &mut NullSink,
//!     &CancelHandle::new(),
//! )?;
//! println!("{}", outcome.output);
//! # Ok(())
//! # }
//! ```

pub mod protocol;
pub mod render;
pub mod runner;
pub mod server;

pub use protocol::{Event, IDLE_WARN, MAX_LINE_BYTES, MAX_TOTAL_BYTES, RunRequest};
pub use runner::{CANCEL_GRACE, CancelHandle, KILL_GRACE, NullSink, ProgressSink, RunOutcome};
pub use server::{PluginOutput, read_request, serve};

use std::path::Path;

use duck_plugin::PluginError;

/// 执行一个 process 轨插件工具（[`runner::run`] 的薄封装）。
///
/// # Errors
/// 见 [`runner::run`]。
pub fn run(
    plugin_dir: &Path,
    bin: &Path,
    request: &RunRequest,
    sink: &mut dyn ProgressSink,
    cancel: &CancelHandle,
) -> Result<RunOutcome, PluginError> {
    runner::run(plugin_dir, bin, request, sink, cancel)
}
