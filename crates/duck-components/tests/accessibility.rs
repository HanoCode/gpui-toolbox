//! 无头键盘可达性测试（P2-7 第 2 条：「Tab 焦点顺序 + 可见焦点环」）。
//!
//! # 为什么这里不测 role / aria_label / aria_selected
//!
//! 计划里本来要断言这些无障碍属性，但**本工作区两条无头查询通路都不可用**，都已实测确认：
//!
//! 1. `gpui_base::test_support` 的 `find` / `snapshots` 查询面需要 `gpui-base/test-support`
//!    这个 feature，而开它会把 `.test_support()` 的返回类型换成 `Observed<E>`；
//!    `gpui-component 0.7.1` 在 `menu/popup_menu.rs:1469` 调了 `.test_support()` 之后
//!    仍继续链式调用 `ScrollableElement::vertical_scrollbar`（同文件 `:1508`），
//!    于是整条依赖链以 `E0599` 编译失败。上游没开过这个 feature，所以从未暴露。
//! 2. `Window::debug_a11y_tree_json()`（gpui 自己的 a11y 树导出）要求 a11y 适配器**已激活**
//!    （`window/a11y.rs:220` `is_active() == is_enabled() && active_flag`），而该标志只由
//!    平台适配器的 activation 回调置位；`TestPlatform`（`platform/test/platform.rs`）与
//!    `VisualTestPlatform`（`platform/visual_test.rs`）都**没有实现 `a11y_init`**，
//!    落到 `platform.rs:1351` 的空实现上 —— 无头环境里树永远不会被采集。
//!
//! 所以本文件只断言**确实可在无头环境观测**的那部分：控件进得了 Tab 序、能真的被 Tab /
//! Shift-Tab 走到、禁用控件被排除。role / 名字 / 选中态那部分由生产代码里的
//! `role(...)` / `aria_label(...)` / `aria_selected(...)` 直接表达，并靠
//! `duck-theme/tests/accessibility_contrast.rs` 守住其中可量化的对比度约束。
//!
//! 键位能生效的前提：`gpui_component::Root` 就是 `gpui_base::Root`
//! （`gpui-component/src/lib.rs:97`），它渲染时给 `#root` 挂 `key_context("Root")`，
//! 而 `gpui_component::init` 会调 `gpui_base::init` 把 `tab` / `shift-tab` 绑到
//! `window.focus_next` / `focus_prev`（`gpui-base/src/root.rs:14-22, 194-227`）。

use duck_components::{activity_button, sidebar_navigation_button, toolbar_action_button};
use gpui::{
    AppContext as _, Context, ElementId, FocusHandle, IntoElement, ParentElement as _, Render,
    Role, Styled as _, TestAppContext, VisualTestContext, Window, div, px,
};
use gpui_component::{Root, init as component_init, v_flex};
use std::collections::BTreeSet;

/// 被测控件的 id。两份 harness 共用这四个，这样「禁用项是否多出一个 tab stop」可以靠集合比较。
const SELECTED_ROUTE: &str = "a11y-selected-route";
const UNSELECTED_ROUTE: &str = "a11y-unselected-route";
const COMMAND: &str = "a11y-command";
const SIDEBAR_ROW: &str = "a11y-sidebar-row";
/// 只出现在「含禁用项」那份 harness 里。
const DISABLED_TOOLBAR: &str = "a11y-disabled-toolbar";

/// 探测 Tab 序时的最大取样次数，必须 ≥ 任何一份 harness 的可达控件数。
const MAX_PROBES: usize = 8;

/// 五（或四）个控件排成一列。`with_disabled` 控制是否带上那个禁用的工具条按钮。
struct Harness {
    with_disabled: bool,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = v_flex()
            .gap(px(8.))
            .child(activity_button(
                ElementId::from(SELECTED_ROUTE),
                "已选中的路由",
                div(),
                Role::Tab,
                true,
                px(32.),
                cx,
            ))
            .child(activity_button(
                ElementId::from(UNSELECTED_ROUTE),
                "未选中的路由",
                div(),
                Role::Tab,
                false,
                px(32.),
                cx,
            ))
            .child(activity_button(
                ElementId::from(COMMAND),
                "命令按钮",
                div(),
                Role::Button,
                false,
                px(32.),
                cx,
            ))
            .child(sidebar_navigation_button(
                ElementId::from(SIDEBAR_ROW),
                "侧栏导航项",
                div(),
                div(),
                true,
                cx,
            ));
        if self.with_disabled {
            // 禁用控件**不该**进 Tab 序：`Button::render` 只在 `!disabled && focusable`
            // 时才 `track_focus`，所以它连焦点句柄都不注册。
            row = row.child(toolbar_action_button(
                ElementId::from(DISABLED_TOOLBAR),
                "禁用的工具按钮",
                div(),
                false,
                true,
                cx,
            ));
        }
        row
    }
}

/// 按 `main.rs` 的顺序初始化，窗口根用 `Root`（Tab 键位生效的前提），并先绘一帧。
fn harness_window(cx: &mut TestAppContext, with_disabled: bool) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|_| Harness { with_disabled });
        Root::new(content, window, cx)
    });
    draw(cx);
    cx
}

/// 强制绘一帧：tab stops 在布局阶段收集，读完才能做焦点遍历。
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 当前持有键盘焦点的句柄。
fn focused(cx: &mut VisualTestContext) -> Option<FocusHandle> {
    cx.update(|window, cx| window.focused(cx))
}

/// 焦点句柄的稳定标识（`FocusHandle` 的 `Debug` 是 `FocusHandle(FocusId(n))`）。
fn handle_id(handle: &FocusHandle) -> String {
    format!("{handle:?}")
}

/// 走一遍 Tab 序，收集**可达**的焦点句柄标识集合。
///
/// 无论遍历是否循环都能收敛：循环时重复项被集合吃掉，不循环时越过末位后句柄不再变化。
/// 起点用 `window.focus_next` 建立 —— 无焦点时键位不落在任何节点上。
fn reachable_stops(cx: &mut VisualTestContext) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    cx.update(|window, cx| window.focus_next(cx));
    for _ in 0..MAX_PROBES {
        let Some(handle) = focused(cx) else {
            break;
        };
        assert!(
            handle.tab_stop,
            "能被 Tab 走到的句柄必须是 tab stop（{:?}）",
            handle_id(&handle)
        );
        seen.insert(handle_id(&handle));
        cx.simulate_keystrokes("tab");
        draw(cx);
    }
    seen
}

/// 焦点必须真能落在这批控件上 —— 此前它们连 `table_stop` 都没设，Tab 走不到。
#[gpui::test]
fn controls_are_reachable_by_keyboard(cx: &mut TestAppContext) {
    let cx = harness_window(cx, false);

    cx.update(|window, cx| window.focus_next(cx));
    let handle = focused(cx).expect("屏上必须有可聚焦控件，否则纯键盘用户什么都点不到");
    assert!(handle.tab_stop, "落到焦点上的控件应当是 tab stop");

    let stops = reachable_stops(cx);
    assert!(
        stops.len() >= 4,
        "四个可聚焦控件都该在 Tab 序里，实测只走到 {}：{stops:?}",
        stops.len()
    );
}

/// Tab 键本身必须驱动焦点前进，Shift-Tab 必须回退。
///
/// 这条是在验证「键位有没有接到焦点遍历上」——若哪天不再调 `gpui_component::init`、
/// 或窗口根不再是 `Root`，`tab` 就没人处理，这里会立刻变红。
#[gpui::test]
fn tab_and_shift_tab_move_focus(cx: &mut TestAppContext) {
    let cx = harness_window(cx, false);

    cx.update(|window, cx| window.focus_next(cx));
    let first = focused(cx).expect("起点应有焦点");
    let first_id = handle_id(&first);

    cx.simulate_keystrokes("tab");
    draw(cx);
    let second = focused(cx).expect("Tab 之后仍应有焦点");
    assert_ne!(
        handle_id(&second),
        first_id,
        "按 Tab 后焦点必须移动（键位确实接到了焦点遍历）"
    );

    cx.simulate_keystrokes("shift-tab");
    draw(cx);
    assert_eq!(
        focused(cx).map(|handle| handle_id(&handle)),
        Some(first_id),
        "Shift-Tab 应回到上一个控件"
    );
}

/// 禁用控件必须被排除在 Tab 序之外。
///
/// 做法是两份 harness 相减：五控件（其中一个禁用）与四控件（都不禁用）**可达数量必须相等**。
/// 这里只比数量、不比句柄标识 —— `FocusId` 是按窗口分配的自增号，同一次测试里的两个窗口
/// 天然拿到不同号段（实测 `2v1..5v1` vs `7v1..10v1`），比标识只会假红；而「多出来的那个
/// 控件若进了 Tab 序，数量就会 +1」正是这条要钉住的事。
#[gpui::test]
fn disabled_control_is_excluded_from_the_tab_order(cx: &mut TestAppContext) {
    let without = {
        let cx = harness_window(cx, false);
        reachable_stops(cx)
    };
    let with = {
        let cx = harness_window(cx, true);
        reachable_stops(cx)
    };

    assert_eq!(
        with.len(),
        without.len(),
        "多出来的那个控件是禁用的，不该多出一个 tab stop：含禁用项 {with:?} vs 不含 {without:?}"
    );
    assert_eq!(
        without.len(),
        4,
        "四个可聚焦控件都应可达、且环境里没有别的 tab stop，实测 {without:?}"
    );
}
