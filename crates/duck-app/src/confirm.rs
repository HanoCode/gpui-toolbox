//! 破坏性动作的**主题确认框**（与页面无关的通用版）。
//!
//! ## 为什么不用 `duck_platform::confirm`
//!
//! 那是 rfd 的**同步系统模态**（macOS 上 `NSAlert runModal`），三个问题：
//!
//! 1. 外观与本应用的主题 / 字号 / 深浅色是两套；
//! 2. 它把 UI 线程钉在原地等用户点，弹窗期间整页不重绘；
//! 3. ⚠️ **最要命的一条**：从 gpui 的 `App::update` 里弹系统模态会把 AppKit 的事件
//!    派发**重入**回 gpui —— 重入期间任何前台任务跑到 `AsyncApp::update_entity`，
//!    都会撞上「RefCell already borrowed」并 abort。
//!
//! 第 3 条有实证：2026-10-16 21:25 的闪退，调用链是「导出连接 →
//! `duck_platform::save_file` → `NSSavePanel runModal` → 重入期间邮件页的事件轮询器
//! 被调度 → `AsyncApp::update_entity` 的 `borrow_mut()` 失败」（`crash-2026-10-16.log`）。
//! 文件面板与确认框同属这一类，界面里一律禁用。
//!
//! ## 本模块是异步的
//!
//! 函数**立刻返回**，真正要做的事整段搬进 `on_ok`：用户什么时候点「确定」都行，
//! 期间界面照常重绘、别的动作照常派发。
//!
//! 标题 / 正文 / 按钮文案在**调用时**就取成 `SharedString`：`tr` 要 `&App`，而弹层
//! 重排的闭包是每次重排都重跑的 `Fn`，在里面取不到「当前语言」那一刻。
//!
//! ## 两个入口
//!
//! | 调用点手里有的东西 | 用哪个 | 回调拿到 |
//! | --- | --- | --- |
//! | `&mut Window` + `&mut App`（裸动作 / 全局动作） | [`open_confirm`] | `(&mut Window, &mut App)` |
//! | `&mut Window` + `&mut Context<T>`（页面方法） | [`open_confirm_in`] | `(&mut T, &mut Window, &mut Context<T>)` |
//!
//! 「裸 `&mut App`、连窗口都没有」的那几处（插件市场、更新器）先借
//! [`crate::update::on_notify_window`] 找到当前窗口，再走 [`open_confirm`]。
//!
//! `pages/db/helpers.rs` 里那份 `open_confirm` 是本模块出现之前的 db 专用副本
//! （多挂了 db 的居中量尺口径）；后续可折叠到本模块，避免两份实现漂移。

use std::rc::Rc;

use duck_components::{
    ButtonVariant, body_text, dialog_footer, dialog_top_offset, modal_drag, modal_drag_area,
    modal_offset, modal_title_row,
};
use gpui::{App, Context, ParentElement as _, SharedString, Styled as _, Window, px};
use gpui_component::{WindowExt as _, v_flex};

/// 确认框宽度。与 `pages::db` / `pages::redis` 的本地常量同值（设计稿的弹窗宽度）。
const CONFIRM_WIDTH: f32 = 440.;

/// 确认框「同意」回调。
///
/// 用 `Rc<dyn Fn>` 而不是泛型 `FnOnce`：弹层的构建闭包是 `Fn`（每一轮重排都会重跑），
/// 要在里面再克隆一份给按钮回调，`FnOnce` 拿不出来。
type ConfirmHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 弹一个主题确认框；用户点「确定」后执行 `on_ok`，点「取消」/ 关窗则什么都不做。
///
/// 破坏性操作调用前，请把默认答案放在安全侧：按钮文案写清楚**会丢掉什么**
/// （「删除」「导入」），别用「确定」。
pub fn open_confirm(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    ok_text: impl Into<SharedString>,
    on_ok: ConfirmHandler,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = title.into();
    let message: SharedString = message.into();
    let ok_text: SharedString = ok_text.into();
    let drag = modal_drag(cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
        let drag_for_content = drag.clone();
        let title_for_content = title.clone();
        let message_for_content = message.clone();
        let on_ok_for_ok = on_ok.clone();
        let on_ok_for_footer = on_ok.clone();
        dialog
            .w(px(CONFIRM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .child(modal_title_row(
                            "app-confirm-title",
                            &drag_for_content,
                            title_for_content.clone(),
                            cx,
                        ))
                        .child(body_text(message_for_content.clone(), cx)),
                ))
            })
            // ⚠️ 可见按钮必须显式挂 footer：`button_props` 只登记回调，
            // `Dialog::render` 只渲染 `.footer(...)`，不挂就是「只有一个 ✕」的空壳。
            .footer(dialog_footer(
                "app-confirm",
                ok_text.clone(),
                ButtonVariant::Primary,
                Rc::new(move |window, cx| {
                    on_ok_for_footer(window, cx);
                    true
                }),
                cx,
            ))
            // 键盘 ⏎ 走 base 的 `Confirm` action → 这里；鼠标点按钮走上面 footer。
            .on_ok(move |_, window, cx| {
                // 确认之后才真正动手；视图已销毁也照样关弹窗，不让一个点不掉的
                // 确认框挂在屏幕上。
                on_ok_for_ok(window, cx);
                true
            })
    });
}

/// 确认框的**页面实体**版本：`on_ok` 拿得到 `&mut T` 与 `Context<T>`。
///
/// 页面上的破坏性动作几乎都要改自己的状态（删标签、清列表、发提示），而
/// [`open_confirm`] 的回调只有 `&mut App`，够不着 `&mut T`。这里把「弹层替调用方
/// 握着 `Entity<T>`」这件事包进来，调用方写普通的方法逻辑即可 —— 代价是 `on_ok`
/// 必须是 `Fn`（弹层闭包每一轮重排都会重跑），要捕获的数据请先在调用前克隆好。
///
/// 页面在用户点确定之前就被关掉时，`update` 会失败：静默跳过，只关弹层。
pub fn open_confirm_in<T, F>(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    ok_text: impl Into<SharedString>,
    on_ok: F,
    window: &mut Window,
    cx: &mut Context<T>,
) where
    T: 'static,
    F: Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
{
    let entity = cx.entity();
    open_confirm(
        title,
        message,
        ok_text,
        Rc::new(move |window, cx| {
            let _ = entity.update(cx, |this, cx| on_ok(this, window, cx));
        }),
        window,
        cx,
    );
}
