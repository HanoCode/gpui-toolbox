//! 系统剪贴板（arboard）。文本读写用；图片留到需要时再加。
//!
//! **所有访问串行化**（见下方那把进程级访问锁）。这不是「顺手加个锁」——macOS 的
//! `NSPasteboard` 在**多线程并发访问**时会抛 Objective-C 异常，而 ObjC 异常跨
//! Rust FFI 边界的后果是 `Rust cannot catch foreign exceptions` → **进程直接
//! abort**（不是返回 `Err`，也没法 `catch_unwind`）。实测形状：8 个线程同时
//! `read_text()` 立刻 abort，单线程连续两次、或单个后台线程都正常。
//!
//! 而本应用**天然会出现并发**：插件工具在后台线程跑（`plugins.rs::run_plugin_tool`），
//! 插件可以通过 `duck.readClipboard()` 读剪贴板；同一时刻用户完全可能在主线程上
//! 走智能粘贴（也读剪贴板）或点「粘贴」写剪贴板。两者撞上就是一次无提示的闪退。
//!
//! 代价说明：锁是**进程级**的，一次剪贴板操作若阻塞（等另一个程序响应粘贴），
//! 其余调用者会一起等——但每个调用者本来就在同步阻塞地等，所以并没有把
//! 「一次卡住」变成「永久卡住」。取锁容忍中毒（前一个持锁者 panic 不该让剪贴板
//! 永久不可用）。

use std::sync::Mutex;

use arboard::Clipboard;

/// 剪贴板访问闸门：`Clipboard` 的构造与每一次读写都必须持锁。
///
/// `Mutex<()>` 而不是 `Mutex<Clipboard>`：`arboard::Clipboard` 在 macOS 上会起一个
/// 后台线程来「服务」剪贴板内容，把实例长期留着会让它在程序退出时与主线程抢跑；
/// 每次操作现建现弃更贴近它自己的文档建议（`wait()` 只在你需要长期持有实例时才有意义）。
static ACCESS: Mutex<()> = Mutex::new(());

/// 取剪贴板锁；中毒时取内部值继续（不把 `PoisonError` 当故障上报）。
fn lock() -> std::sync::MutexGuard<'static, ()> {
    ACCESS.lock().unwrap_or_else(|error| error.into_inner())
}

/// 写入纯文本。
///
/// # Errors
/// 剪贴板不可用（无显示服务 / 权限不足）时返回错误描述。
pub fn write_text(text: &str) -> Result<(), String> {
    let _guard = lock();
    let mut clipboard = Clipboard::new().map_err(|error| error.to_string())?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|error| error.to_string())
}

/// 读取纯文本；剪贴板不可用或无文本内容时返回 `None`。
#[must_use]
pub fn read_text() -> Option<String> {
    let _guard = lock();
    Clipboard::new().ok()?.get_text().ok()
}
