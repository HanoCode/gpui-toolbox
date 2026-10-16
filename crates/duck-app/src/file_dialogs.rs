//! 原生文件对话框的**异步**入口（gpui 自己的面板）。
//!
//! ## 为什么不用 `duck_platform::save_file` / `open_file`
//!
//! 那两个走 `rfd`，在 macOS 上是**阻塞的嵌套模态循环**（`NSApp runModal`）。
//! 而界面里能触发它们的动作全都在 gpui 的 `App::update` 里面 —— 模态循环一开，
//! AppKit 就把 gpui 的事件派发重入进来，`AsyncApp::update_entity` 再去
//! `borrow_mut()` 撞上「RefCell already borrowed」并 abort（`try_borrow_mut`
//! 那一路则静默丢事件）。实证：`~/Library/Application Support/DuckToolbox/logs/
//! crash-2026-10-15.log` 12:46 那条，调用链
//! `import_csv → duck_platform::dialog::open_file → rfd pick_file → run_modal`。
//!
//! gpui 自己的 `prompt_for_new_path` / `prompt_for_paths` 走
//! `NSSavePanel::beginWithCompletionHandler`（真异步回调，**不重入**事件循环），
//! 结果经 oneshot channel 送回 —— 所以这里是界面代码唯一正确的那条路。
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

use gpui::{Context, PathPromptOptions};

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

/// 「打开文件」（单选）：用户选定后回调 `(路径, 页面, 上下文)`。
///
/// `prompt` 是面板上的提示语（macOS 显示在标题区）。面板给的是「选中项的集合」，
/// 这里取唯一那个 —— 单选面板本来也只可能给一个。
pub fn open_file<T, F>(cx: &mut Context<T>, prompt: &str, on_pick: F)
where
    T: 'static,
    F: FnOnce(PathBuf, &mut T, &mut Context<T>) + 'static,
{
    let options = PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(prompt.to_owned().into()),
    };
    let receiver = cx.prompt_for_paths(options);
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(mut paths))) = receiver.await else {
            return;
        };
        // 面板给的是「选中项的集合」：单选下取唯一那个；空集合按取消处理。
        let Some(path) = paths.pop() else {
            return;
        };
        let _ = this.update(cx, |this, cx| on_pick(path, this, cx));
    })
    .detach();
}
