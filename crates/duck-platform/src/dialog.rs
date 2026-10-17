//! 原生文件与消息对话框（rfd）。
//!
//! 都是模态阻塞调用——退出码由平台事件循环在对话框关闭后返回，因此**不要在 render 里调用**，
//! 只在动作处理器（点击、菜单、快捷键）里调用。
//!
//! ⚠️ **文件选择（`open_file` / `open_files` / `pick_folder` / `save_file`）与消息框
//!（`alert` / `confirm`）一律不许从 gpui 的 `App::update` 里调**：macOS 上 rfd 用的是
//! `NSApp` 的**嵌套模态循环**（文件面板 `runModal`、消息框 `NSAlert runModal`），循环一开
//! AppKit 就把事件派发重入进来，而此刻 App 的 `RefCell` 还借在外面 —— 重入期间任何前台
//! 任务走到 `AsyncApp::update_entity` 的 `borrow_mut()` 就直接撞成
//! 「RefCell already borrowed」并 abort（`try_borrow_mut` 那一路则静默丢事件）。
//!
//! 界面上能触发的动作**全都**在 `App::update` 里，所以界面代码请走：
//!
//! - 文件面板 → `duck-app` 的 `file_dialogs` 模块（gpui 自己的 `prompt_for_paths` /
//!   `prompt_for_new_path`，`beginWithCompletionHandler` **真异步**，不重入事件循环）；
//! - 确认框 → `duck-app` 的 `confirm::open_confirm`（主题弹窗，异步）。
//!
//! 本模块这几个函数留给**没有 gpui 上下文**的调用方（启动期、后台线程、CLI）。
//!
//! 实证（同一条链上的两次闪退）：
//!
//! - `crash-2026-10-15.log` 12:46：`import_csv → duck_platform::dialog::open_file →
//!   rfd pick_file → run_modal`；
//! - `crash-2026-10-16.log` 21:25：`DatabasePage::export_connections →
//!   duck_platform::dialog::save_file → NSSavePanel runModal` → 重入期间邮件页的事件
//!   轮询器被调度 → `AsyncApp::update_entity` → panic → abort。
//

use std::path::PathBuf;

use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

/// 选择单个文件。
#[must_use]
pub fn open_file() -> Option<PathBuf> {
    rfd::FileDialog::new().pick_file()
}

/// 选择多个文件；用户取消或选了空集合时返回 `None`。
#[must_use]
pub fn open_files() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .pick_files()
        .filter(|files| !files.is_empty())
}

/// 选择目录。
#[must_use]
pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().pick_folder()
}

/// 「另存为」；`default_name` 是预填文件名。
#[must_use]
pub fn save_file(default_name: impl AsRef<str>) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_file_name(default_name.as_ref())
        .save_file()
}

/// 消息框语义级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Info,
    Warning,
    Error,
}

/// 只读消息框（单个「好」按钮）。
pub fn alert(title: &str, description: &str, kind: AlertKind) {
    let level = match kind {
        AlertKind::Info => MessageLevel::Info,
        AlertKind::Warning => MessageLevel::Warning,
        AlertKind::Error => MessageLevel::Error,
    };
    MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(level)
        .set_buttons(MessageButtons::Ok)
        .show();
}

/// 确认框；用户点「是」返回 `true`。
///
/// 破坏性操作调用前，请把默认答案放在安全侧（见 `duck-components::dialog` 的约定）。
#[must_use]
pub fn confirm(title: &str, description: &str) -> bool {
    matches!(
        MessageDialog::new()
            .set_title(title)
            .set_description(description)
            .set_level(MessageLevel::Warning)
            .set_buttons(MessageButtons::YesNo)
            .show(),
        MessageDialogResult::Yes
    )
}
