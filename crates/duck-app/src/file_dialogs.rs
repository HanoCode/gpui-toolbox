//! 原生文件对话框的**异步**入口（gpui 自己的面板）。
//!
//! ## 为什么不用 `duck_platform::save_file` / `open_file`
//!
//! 那两个走 `rfd`，在 macOS 上是**阻塞的嵌套模态循环**（`NSApp runModal`）。
//! 而界面里能触发它们的动作全都在 gpui 的 `App::update` 里面 —— 模态循环一开，
//! AppKit 就把 gpui 的事件派发重入进来，`AsyncApp::update_entity` 再去
//! `borrow_mut()` 撞上「RefCell already borrowed」并 abort（`try_borrow_mut`
//! 那一路则静默丢事件）。实证（同一条链上的两次闪退）：`crash-2026-10-15.log`
//! 12:46 `import_csv → duck_platform::dialog::open_file → rfd pick_file → run_modal`；
//! `crash-2026-10-16.log` 21:25 `DatabasePage::export_connections →
//! duck_platform::dialog::save_file → NSSavePanel runModal`（重入期间邮件页的事件
//! 轮询器被调度，`AsyncApp::update_entity` 直接 panic）。
//!
//! gpui 自己的 `prompt_for_new_path` / `prompt_for_paths` 走
//! `NSSavePanel::beginWithCompletionHandler`（真异步回调，**不重入**事件循环），
//! 结果经 oneshot channel 送回 —— 所以这里是界面代码唯一正确的那条路。
//! 确认框同理走 [`crate::confirm::open_confirm`]（主题弹窗）。
//!
//! ## 两套签名：有没有页面实体
//!
//! | 调用点手里有的东西 | 用哪个 | 回调拿到 |
//! | --- | --- | --- |
//! | `&mut Context<T>`（页面方法） | [`save_file`] / [`open_file`] / [`open_files`] / [`pick_folder`] | `(路径, &mut T, &mut Context<T>)` |
//! | 同上，且回调里要弹提示 | [`save_file_in_window`] | 上面那套 + `&mut Window` |
//! | 只有 `&mut Window, &mut App`（`on_click` 里的裸 App） | [`save_file_in_app`] / [`open_file_in_app`] / [`pick_folder_in_app`] | `(路径, &mut Window, &mut App)` |
//!
//! ## 用之前先记住：对话框是异步的
//!
//! 用户可能开着面板去泡杯茶。回调跑起来时页面状态早就可能变了（翻页 / 切标签 /
//! 换了连接 / 结果被新查询顶掉）。凡是「导出我现在看着的这份结果」这类语义，
//! 内容必须在**打开对话框之前**就固化好，回调里**不许再回读页面**。
//!
//! ## 取消与出错都不回调
//!
//! 用户按「取消」、面板打不开（部分 Linux 环境）、通道断掉 —— 三种情况一律**不调
//! `on_pick`**。调用方因此不需要在回调里再判一次「是不是真选了」（静默返回与
//! 「路径为空」这两件事在类型上已经分开了）。

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, PathPromptOptions, Window};

/// 面板选项：`files` / `directories` / `multiple` 三种组合，`prompt` 是面板上的提示语。
fn prompt_options(
    files: bool,
    directories: bool,
    multiple: bool,
    prompt: &str,
) -> PathPromptOptions {
    PathPromptOptions {
        files,
        directories,
        multiple,
        prompt: Some(prompt.to_owned().into()),
    }
}

/// 取出「选中项的集合」里唯一那个；空集合按取消处理。
fn only(mut paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.pop()
}

/// 「另存为」：用户选定落点后回调 `(路径, 页面, 上下文)`。
///
/// `default_name` 是面板里预填的文件名（含扩展名），起始目录见
/// [`duck_platform::default_dialog_dir`]。
pub fn save_file<T, F>(cx: &mut Context<T>, default_name: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(PathBuf, &mut T, &mut Context<T>) + 'static,
{
    let dir = duck_platform::default_dialog_dir();
    let receiver = cx.prompt_for_new_path(&dir, Some(default_name));
    cx.spawn(async move |this, cx| {
        // 取消（`Ok(None)`）与失败（`Err`）都不回调 —— 见模块注释。
        let Ok(Ok(Some(path))) = receiver.await else {
            return;
        };
        let _ = this.update(cx, |this, cx| on_pick(path, this, cx));
    })
    .detach();
}

/// [`save_file`] 的**带窗口**版本：回调额外拿到 `&mut Window`。
///
/// 需要在回调里弹提示（`notify::*` 全都要 `&mut Window`）的调用方用这个。窗口按
/// `crate::update::on_notify_window` 的口径在**回调那一刻**挑（活跃窗口优先）——
/// 与 `spawn_db` / `spawn_local` 的 `done` 拿到窗口走的是同一条路，不额外造一套口径。
pub fn save_file_in_window<T, F>(cx: &mut Context<T>, default_name: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(PathBuf, &mut Window, &mut T, &mut Context<T>) + 'static,
{
    let dir = duck_platform::default_dialog_dir();
    let receiver = cx.prompt_for_new_path(&dir, Some(default_name));
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(path))) = receiver.await else {
            return;
        };
        cx.update(move |cx| {
            crate::update::on_notify_window(cx, move |window, cx| {
                let _ = this.update(cx, |this, cx| on_pick(path, window, this, cx));
            });
        });
    })
    .detach();
}

/// 「打开文件」（单选）：用户选定后回调 `(路径, 页面, 上下文)`。
///
/// `prompt` 是面板上的提示语（macOS 显示在标题区）。面板给的是「选中项的集合」，
/// 这里取唯一那个 —— 单选面板本来也只可能给一个。
pub fn open_file<T, F>(cx: &mut Context<T>, prompt: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(PathBuf, &mut T, &mut Context<T>) + 'static,
{
    let receiver = cx.prompt_for_paths(prompt_options(true, false, false, prompt));
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        // 面板给的是「选中项的集合」：单选下取唯一那个；空集合按取消处理。
        let Some(path) = only(paths) else {
            return;
        };
        let _ = this.update(cx, |this, cx| on_pick(path, this, cx));
    })
    .detach();
}

/// 「选目录」：用户选定后回调 `(路径, 页面, 上下文)`。
pub fn pick_folder<T, F>(cx: &mut Context<T>, prompt: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(PathBuf, &mut T, &mut Context<T>) + 'static,
{
    let receiver = cx.prompt_for_paths(prompt_options(false, true, false, prompt));
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = only(paths) else {
            return;
        };
        let _ = this.update(cx, |this, cx| on_pick(path, this, cx));
    })
    .detach();
}

/// 「打开文件」（多选）：用户选定后回调 `(路径集合, 页面, 上下文)`。
///
/// 空集合按取消处理（「选了个空」在语义上就是没选）。
pub fn open_files<T, F>(cx: &mut Context<T>, prompt: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(Vec<PathBuf>, &mut T, &mut Context<T>) + 'static,
{
    let receiver = cx.prompt_for_paths(prompt_options(true, false, true, prompt));
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let _ = this.update(cx, |this, cx| on_pick(paths, this, cx));
    })
    .detach();
}

/// 「另存为」的**无页面实体**版本：回调拿 `(路径, 窗口, App)`。
///
/// 给「点击处理器里只有 `&mut App`」的调用方（`data.rs` 的导入导出、`terminal_dialogs`
/// 的选择按钮）。窗口句柄在**开面板那一刻**就取下来，回调里再用 `App::update_window`
/// 还原 `&mut Window` —— 用户可能在面板上停留很久，也可能顺手把这个窗口关了，
/// 所以还原失败（`Err`）时静默丢弃，不 panic。
pub fn save_file_in_app<F>(window: &mut Window, cx: &mut App, default_name: &str, on_pick: F)
where
    F: FnOnce(PathBuf, &mut Window, &mut App) + 'static,
{
    let handle = window.window_handle();
    let dir = duck_platform::default_dialog_dir();
    let receiver = cx.prompt_for_new_path(&dir, Some(default_name));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(path))) = receiver.await else {
            return;
        };
        let _ = cx.update_window(handle, |_, window, cx| on_pick(path, window, cx));
    })
    .detach();
}

/// 「打开文件」（单选）的**无页面实体**版本，语义同 [`save_file_in_app`]。
pub fn open_file_in_app<F>(window: &mut Window, cx: &mut App, prompt: &str, on_pick: F)
where
    F: FnOnce(PathBuf, &mut Window, &mut App) + 'static,
{
    let handle = window.window_handle();
    let receiver = cx.prompt_for_paths(prompt_options(true, false, false, prompt));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = only(paths) else {
            return;
        };
        let _ = cx.update_window(handle, |_, window, cx| on_pick(path, window, cx));
    })
    .detach();
}

/// 「选目录」的**无页面实体**版本，语义同 [`save_file_in_app`]。
pub fn pick_folder_in_app<F>(window: &mut Window, cx: &mut App, prompt: &str, on_pick: F)
where
    F: FnOnce(PathBuf, &mut Window, &mut App) + 'static,
{
    let handle = window.window_handle();
    let receiver = cx.prompt_for_paths(prompt_options(false, true, false, prompt));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = receiver.await else {
            return;
        };
        let Some(path) = only(paths) else {
            return;
        };
        let _ = cx.update_window(handle, |_, window, cx| on_pick(path, window, cx));
    })
    .detach();
}
