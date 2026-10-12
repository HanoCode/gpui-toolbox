//! 「终端里 Tab 补全失灵」的无头回归测试。
//!
//! # 根因（都有源码依据）
//!
//! gpui 的按键派发是 **action 绑定先于 key-down 监听器**（`window.rs::dispatch_key_event`：
//! 沿焦点链收集绑定并逐个派发，全部未命中才走 `finish_dispatch_key_event` 把
//! `KeyDownEvent` 交给 `on_key_down`）。而窗口根 `gpui_base::Root` 在 `"Root"` 上下文
//! 全局绑了 `"tab" → Tab`（焦点遍历，`root.rs:16`）—— 不在更深的 `"Terminal"` 上下文
//! 绑一个自己的 Tab 动作，终端画布的 `on_key_down` 永远收不到 Tab。
//!
//! # 本测试钉住的契约
//!
//! 画布（`key_context("Terminal")`）持有焦点时按 Tab：
//! 1. `"Terminal"` 上下文的 `SendTab` 动作被派发（画布把 `\t` 写进 PTY —— 由
//!    `duck-term::keymap` 的单测另钉）；
//! 2. `Root` 的焦点遍历 Tab **不**被触发 —— 焦点不挪窝。
//!
//! 结构复刻 `gpui-base` Root 的绑定方式（同键同上下文）与 `pages::terminal_canvas`
//! 的挂法（`key_context` + `on_action`），动作由真实键位表解析。
//!
//! # 覆盖面
//!
//! 钉的是 **gpui 的裁决规则**（「更深的上下文胜出、且胜出后 `Root` 的动作不再触发」），
//! 而不是某一条键位：`app::install` 里 `"Terminal"` 上下文的一整组绑定
//! （`ctrl-c` / 滚动族 / 行编辑族）都靠同一条规则生效 —— 这条规则若变，它们会一起坏，
//! 所以在这里守一次就够。键位表本身由编译期保证动作类型存在，键名字符串则由
//! `Keystroke::parse` 在启动时校验。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AppContext as _, InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, actions,
    div,
};

actions!(test_terminal, [RootTab, CanvasSendTab]);

/// 与 `gpui_base::Root` 相同的注册方式：`"Root"` 上下文绑 `tab`。
/// 直接用自家动作代替 `root::Tab`（那是个 pub(crate) 类型），派发语义一致：
/// 都是「该上下文里 tab 命中的动作」，谁被命中由 gpui 的上下文深度规则裁决。
fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("tab", RootTab, Some("Root")),
        KeyBinding::new("tab", CanvasSendTab, Some("Terminal")),
    ]);
}

struct CanvasReplica {
    /// `SendTab` 是否被派发过。
    sent: Rc<Cell<bool>>,
    focus: gpui::FocusHandle,
}

impl Render for CanvasReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let sent = self.sent.clone();
        div()
            .id("canvas")
            .size_full()
            // 与真实画布一致：上下文 + 动作 + 焦点。
            .key_context("Terminal")
            .track_focus(&self.focus)
            .tab_stop(true)
            .on_key_down(move |_event: &KeyDownEvent, _window, _cx| {
                // 只要有任何同键动作命中，这里就**不该**被走到 —— 用不动它来断言。
            })
            .on_action(move |_: &CanvasSendTab, _window, _cx| {
                sent.set(true);
            })
            .child("terminal")
    }
}

/// 「画布之外」的一个可聚焦元素。**必须真的挂到元素树上**：gpui 的派发路径由
/// 焦点句柄所在的元素链推导，未挂载的句柄给出空路径 —— 那样连 `"Root"` 的绑定
/// 都命中不了，测不出「焦点在画布外 » Tab 仍走焦点遍历」这条契约。
struct OtherReplica {
    focus: gpui::FocusHandle,
}

impl Render for OtherReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .id("other")
            .size_full()
            .track_focus(&self.focus)
            .tab_stop(true)
            .child("elsewhere")
    }
}

struct HostReplica {
    canvas: gpui::Entity<CanvasReplica>,
    /// 画布之外的可聚焦兄弟节点（焦点落它上面时，派发路径里没有 `"Terminal"`）。
    other: gpui::Entity<OtherReplica>,
    /// `RootTab`（焦点遍历替身）是否被派发过。
    root_fired: Rc<Cell<bool>>,
}

impl Render for HostReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let root_fired = self.root_fired.clone();
        div()
            .id("root")
            .size_full()
            .key_context("Root")
            .on_action(move |_: &RootTab, _window, _cx| {
                root_fired.set(true);
                // Root 的 Tab 处理器会挪焦点（`window.focus_next`）；这里动不动焦点
                // 都不影响断言，`root_fired` 本身就是「动作被触发」的证据。
            })
            .child(self.canvas.clone())
            .child(self.other.clone())
    }
}

#[gpui::test]
fn tab_reaches_the_terminal_action_while_the_canvas_is_focused(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let sent = Rc::new(Cell::new(false));
    let root_fired = Rc::new(Cell::new(false));

    let window = cx.add_window(|window, cx| {
        let canvas = cx.new(|cx| CanvasReplica {
            sent: sent.clone(),
            focus: cx.focus_handle(),
        });
        let other = cx.new(|cx| OtherReplica {
            focus: cx.focus_handle(),
        });
        // 聚焦到画布：按键派发路径 = canvas("Terminal") → root("Root")。
        let handle = canvas.read(cx).focus.clone();
        window.focus(&handle, cx);
        HostReplica {
            canvas,
            other,
            root_fired: root_fired.clone(),
        }
    });
    let mut visual = VisualTestContext::from_window(window.into(), cx);

    visual.simulate_keystrokes("tab");

    assert!(
        sent.get(),
        "画布聚焦时按 Tab 应派发 Terminal 上下文的 SendTab 动作"
    );
    assert!(
        !root_fired.get(),
        "Root 上下文的焦点遍历 Tab 不应再被触发（更深的 Terminal 绑定胜出）"
    );
}

/// 焦点不在画布上时，Tab 仍归焦点遍历管 —— 绑定收窄到 `"Terminal"` 上下文的意义所在。
#[gpui::test]
fn tab_still_traverses_focus_when_the_canvas_is_not_focused(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    let sent = Rc::new(Cell::new(false));
    let root_fired = Rc::new(Cell::new(false));

    let window = cx.add_window(|window, cx| {
        let canvas = cx.new(|cx| CanvasReplica {
            sent: sent.clone(),
            focus: cx.focus_handle(),
        });
        let other = cx.new(|cx| OtherReplica {
            focus: cx.focus_handle(),
        });
        // 聚焦到画布外的兄弟节点：派发路径里没有 `"Terminal"` 上下文，
        // Tab 应回落到 `"Root"` 的焦点遍历绑定。
        let other_focus = other.read(cx).focus.clone();
        window.focus(&other_focus, cx);
        HostReplica {
            canvas,
            other,
            root_fired: root_fired.clone(),
        }
    });
    let mut visual = VisualTestContext::from_window(window.into(), cx);

    visual.simulate_keystrokes("tab");

    assert!(!sent.get(), "焦点不在画布上时不应派发 SendTab");
    assert!(
        root_fired.get(),
        "焦点不在画布上时 Tab 应走 Root 的焦点遍历"
    );
}
