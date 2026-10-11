//! 把应用从「看不见」的状态恢复出来。
//!
//! 全局快捷键按下时要能唤起主窗，而「看不见」在 macOS 上有两种：**应用被隐藏**
//! （`⌘H`，或我们自己为了 toggle 调的 `App::hide`）与**窗口被最小化**（`⌘M`、
//! 黄点，或系统偏好里的双击标题栏动作）。gpui 对这两种状态都**没有**对应的恢复
//! 入口，所以这里自己补上：
//!
//! - `App::hide` 有，`unhide` 没有 —— gpui 只暴露了 `hide_other_apps` /
//!   `unhide_other_apps`（操作**别的**应用）。
//! - `Window::minimize_window` 有，还原没有 —— `activate_window` 走的是
//!   `makeKeyAndOrderFront:`，而 AppKit 里 `orderFront:` **不会**解除最小化。
//!   （Windows 那边正好相反：gpui 的 `PlatformWindow::activate` 自己会
//!   `IsIconic` → `SW_RESTORE`，所以那个平台不需要本模块。）
//!
//! 只有 macOS 需要这条补丁，于是只有 macOS 有真实实现；其它平台是空操作。
//!
//! # 为什么敢直接发 AppKit 消息
//!
//! `App::hide` 在 macOS 上发的就是 `-[NSApplication hide:]`（见
//! `gpui_macos/src/platform.rs`），这里发的是同一对消息里的另一半 ——
//! 不是「绕过 gpui 改状态」，是补上它没暴露的那半。`objc2` / `objc2-app-kit`
//! 的版本与 gpui-fast 锁定的快照一致（`0.6` / `0.3.2`），**不新增下载**。

/// 解除应用隐藏，并把所有被最小化的窗口还原。
///
/// 还原是「全部窗口」而不是「只还原主窗」：gpui 不交出 `NSWindow` 指针
/// （`Window` 内部是 `Box<dyn PlatformWindow>`，没有公开的原生句柄），而
/// 用户按下唤起键时想要的本来就是「把 Duck 叫回来」。代价是被用户刻意最小化的
/// 辅助窗口也会一起回来 —— 那是用户自己最小化的窗口，一起还原不算意外。
pub fn restore_visible() {
    #[cfg(target_os = "macos")]
    macos::restore_visible();

    #[cfg(not(target_os = "macos"))]
    {
        // Windows / Linux：`Window::activate_window` 会自己把最小化的窗口还原
        // （Windows 的 `PlatformWindow::activate` 里有 `IsIconic` → `SW_RESTORE`；
        // X11 / Wayland 的窗口管理器自己处理）。这里没有额外要做的事。
    }
}

/// 取消「隐藏其他应用」的效果，把所有被隐藏的应用重新显示出来。
///
/// 与 [`restore_visible`] 同一族的补丁：gpui 只暴露了 `hide_other_apps`，
/// 没有反向的 `unhide_all_apps`。macOS 的「显示全部」是应用菜单里的标准三项之一
/// （隐藏 / 隐藏其他 / 显示全部），缺了它这一组菜单就是残的。
pub fn show_all_apps() {
    #[cfg(target_os = "macos")]
    macos::show_all_apps();

    #[cfg(not(target_os = "macos"))]
    {
        // 其它平台没有对应的全局隐藏语义，空操作。
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    pub fn restore_visible() {
        // AppKit 的这几条消息都只能在主线程发。我们是主线程（这条路径从
        // gpui 的窗口/快捷键回调进来），但 `MainThreadMarker` 是唯一能证明
        // 这一点的东西 —— 拿不到就不要猜着发，那是在别的线程上碰 AppKit。
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("restore_visible was called off the main thread; ignoring");
            return;
        };
        // 安全性：`sharedApplication` 只在主线程有效（上面刚校验过），
        // 之后几条都是对已有窗口的幂等操作（已显示 / 已还原的窗口不受影响）。
        let app = NSApplication::sharedApplication(mtm);
        if app.isHidden() {
            app.unhide(None);
        }
        let mut restored = 0usize;
        for window in app.windows().iter() {
            if window.isMiniaturized() {
                window.deminiaturize(None);
                restored += 1;
            }
        }
        if restored > 0 {
            tracing::debug!(count = restored, "restored miniaturised windows");
        }
    }

    /// 与 `restore_visible` 同一条主线：`MainThreadMarker` 证明在主线程，
    /// `unhideAllApplications:` 是对全系统的幂等操作（没有隐藏的应用时无副作用）。
    pub fn show_all_apps() {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("show_all_apps was called off the main thread; ignoring");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        app.unhideAllApplications(None);
    }
}
