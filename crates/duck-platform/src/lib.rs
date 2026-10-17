//! Duck Toolbox 平台能力层：把「操作系统能做什么」收口到一处。
//!
//! 本 crate 不依赖 gpui —— 它是纯宿主能力（路径、日志、崩溃捕获、原生对话框、剪贴板、
//! 系统外壳、开机自启、安全凭据），因此既可以被 UI 层调用，也可以被后台任务、
//! 单元测试直接调用。
//!
//! 托盘图标与自动更新都已落地，但**都不在这一层**：前者在 `duck-app::tray`
//! （要 gpui 的 SVG 光栅化与全局状态），后者是独立的 `duck-update` crate
//! （纯逻辑 + 阻塞网络，同样不依赖 gpui）。平台层保持「只做宿主能力」的边界。

pub mod archive;
mod autostart;
mod clipboard;
mod crash;
mod dialog;
mod foreground;
mod handlers;
mod hotkey;
mod ipc;
mod lock;
mod logging;
mod open;
mod paths;
pub mod secret;
mod shell;
mod time;
pub mod vault;

pub use autostart::{LaunchAtLoginError, launch_at_login_enabled, set_launch_at_login};

pub use clipboard::{read_text, write_text};
pub use crash::{crash_count, crash_log_path, install_panic_hook};
pub use dialog::{AlertKind, alert, confirm, open_file, open_files, pick_folder, save_file};
pub use foreground::{restore_visible, show_all_apps};
pub use handlers::{PACKAGE_EXTENSION, register_open_handlers};
/// 全局热键。多个键按 [`Slot`] 分槽登记 —— 槽名由应用层定义，平台层只当不透明的键。
pub use hotkey::{
    CALENDAR_SPEC as CALENDAR_HOTKEY_SPEC, FLOAT_SPEC as FLOAT_HOTKEY_SPEC, Hotkey, HotkeyError,
    LAUNCHER_SPEC as LAUNCHER_HOTKEY_SPEC, ModifierState,
    QUICK_CAPTURE_SPEC as QUICK_CAPTURE_HOTKEY_SPEC, Slot, active_spec, bind, last_error, start,
};
pub use ipc::{IpcError, IpcListener, forward_to_running_instance, listen_for_requests};
/// IPC 协议层（line-based，duck-open/1）。给跨进程契约测试用，生产代码不必
/// 直接调——`listen_for_requests` / `forward_to_running_instance` 是更高层的接口。
pub use ipc::{decode, encode, endpoint_path as ipc_endpoint_path};
pub use lock::{InstanceLock, LockOutcome, acquire as acquire_instance_lock};
pub use logging::{
    LOG_DIR_MAX_BYTES, LOG_FILE_MAX_BYTES, LOG_RETENTION_DEFAULT_DAYS, LOG_RETENTION_FOREVER,
    LOG_SEGMENTS_PER_DAY, RollingWriter, enforce_size_cap, file_stamp, init_logging,
    log_file_segment, log_file_stamp, logs_summary, prune_logs,
};
pub use open::{OpenRequest, URL_SCHEME};
pub use paths::{
    app_data_dir, cache_dir, config_dir, db_path, default_dialog_dir, ensure_dir, lock_path,
    log_dir,
};
pub use shell::{open_url, reveal_in_finder};
pub use time::local_utc_offset_seconds;
