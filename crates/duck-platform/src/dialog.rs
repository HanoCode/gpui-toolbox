//! 原生文件与消息对话框（rfd）。
//!
//! 都是模态阻塞调用——退出码由平台事件循环在对话框关闭后返回，因此**不要在 render 里调用**，
//! 只在动作处理器（点击、菜单、快捷键）里调用。

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
