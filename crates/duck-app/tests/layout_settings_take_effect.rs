//! 「设置 → 外观 → 布局」改动必须真的生效（无头回归守卫）。
//!
//! # 为什么守这里
//!
//! 2026-10-11 用户实测报告「设置里的侧栏调整不生效」。逐环核查后确认：写入链
//! （`dropdown_select` → `update_appearance` → `set_global`）与持久化链
//! （`observe_global` → `save_appearance`）都在，真正的疑点是**传播链**——
//! 工具箱侧栏这类视图只调 `active_theme(cx)` 读全局、从不 `cx.notify()`，全靠
//! gpui-fast 的「全局读追踪」（`note_global_read` + `mark_changed_retained_views_dirty`）
//! 在外观变化后自动标脏重渲。这条链一旦上游行为变化，表现就是「设置改了、界面纹丝不动」，
//! 单测不会红、clippy 不会说话。
//!
//! 所以这里守两件可观测的事：
//! 1. **传播链**：一个只读 `active_theme(cx).appearance()` 的视图，在外观变化、
//!    且自己未被通知的前提下，下一帧必须拿到新值；
//! 2. **交互链**：真的用鼠标点开「右栏位置」下拉、用键盘选中一项，全局外观必须变。
//!
//! # 效果作用面（不是 bug，是设计）
//!
//! 「侧栏项间距」只作用于**工具箱页**的侧栏条目；「右栏位置」只在**有右栏内容的路由**
//! （目前仅工具箱）且右栏**展开**时才能看出变化。主窗停在别的路由上改这两项，
//! 画面确实不会有任何动静——那是作用面窄，不是这条链断了。

use std::cell::Cell;
use std::rc::Rc;

use duck_components::{dropdown_select, option_row};
use duck_theme::{SidebarSide, active_theme, update_appearance};
use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, Modifiers, ParentElement as _,
    Render, SharedString, Styled as _, TestAppContext, VisualTestContext, Window, div, point,
};
use gpui_component::{Root, init as component_init, v_flex};

/// 下拉控件的 debug 选择器（定位点击坐标用）。
const CONTROL: &str = "layout-side-control";

/// 被测视图：按 `pages/settings.rs` 的同款方式渲染一行「右栏位置」，
/// 并在渲染时把当前外观的 `nav_gap` 与侧别记进探针。
///
/// 视图本身**从不** `cx.notify()`——工具箱侧栏就是这个形态，传播链守的就是它。
struct Harness {
    seen_nav_gap: Rc<Cell<f32>>,
    /// 1.0 = 左，0.0 = 右。
    seen_side: Rc<Cell<f32>>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = active_theme(cx).appearance();
        self.seen_nav_gap.set(appearance.nav_gap);
        self.seen_side.set(match appearance.right_sidebar_side {
            SidebarSide::Left => 1.,
            SidebarSide::Right => 0.,
        });

        let side_options: [(SharedString, SharedString); 2] = [
            (
                SharedString::from(SidebarSide::Left.wire_name()),
                SharedString::from("左侧"),
            ),
            (
                SharedString::from(SidebarSide::Right.wire_name()),
                SharedString::from("右侧"),
            ),
        ];
        let current = SharedString::from(appearance.right_sidebar_side.wire_name());
        let control = div()
            .debug_selector(|| CONTROL.to_string())
            .child(dropdown_select(
                "layout-side-dropdown",
                side_options,
                current,
                |value, _, cx| {
                    let side = SidebarSide::from_wire_name(&value);
                    update_appearance(cx, move |appearance| appearance.right_sidebar_side = side);
                },
                cx,
            ));

        v_flex()
            .size_full()
            .child(option_row("右栏位置", None, control, cx))
    }
}

fn harness_window(
    cx: &mut TestAppContext,
    seen_nav_gap: Rc<Cell<f32>>,
    seen_side: Rc<Cell<f32>>,
) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let content = cx.new(|_| Harness {
            seen_nav_gap,
            seen_side,
        });
        Root::new(content, window, cx)
    });
    draw(cx);
    cx
}

/// 强制绘一帧：外观变化后的重渲要真的画出来，探针才会被写入。
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 传播链：只读主题的视图在 `update_appearance` 之后**未被通知**也必须拿到新值。
///
/// 这条守的是 gpui-fast 全局读追踪的自动标脏——它若失效，所有「设置改了但界面
/// 不动」类缺陷都会从这里长出来。
#[gpui::test]
fn layout_appearance_changes_reach_views_that_only_read_the_theme(cx: &mut TestAppContext) {
    let seen_nav_gap = Rc::new(Cell::new(f32::NAN));
    let seen_side = Rc::new(Cell::new(-1.));
    let cx = harness_window(cx, seen_nav_gap.clone(), seen_side.clone());

    // 初值：nav_gap 缺省 4，侧栏在右。
    assert!(
        (seen_nav_gap.get() - 4.).abs() < f32::EPSILON,
        "首帧应读到缺省间距 4，实得 {}",
        seen_nav_gap.get()
    );
    assert!(
        (seen_side.get() - 0.).abs() < f32::EPSILON,
        "首帧应读到缺省侧别「右」，实得 {}",
        seen_side.get()
    );

    cx.update(|_, cx| {
        update_appearance(cx, |appearance| {
            appearance.nav_gap = 20.;
            appearance.right_sidebar_side = SidebarSide::Left;
        });
    });
    draw(cx);

    assert!(
        (seen_nav_gap.get() - 20.).abs() < f32::EPSILON,
        "外观变化后，只读主题、未被通知的视图必须在下一帧拿到新间距，实得 {}",
        seen_nav_gap.get()
    );
    assert!(
        (seen_side.get() - 1.).abs() < f32::EPSILON,
        "外观变化后，只读主题、未被通知的视图必须在下一帧拿到新侧别，实得 {}",
        seen_side.get()
    );
    cx.update(|_, cx| {
        assert_eq!(
            active_theme(cx).appearance().nav_gap,
            20.,
            "全局外观本身必须已经改到"
        );
    });
}

/// 交互链：点开下拉 → 键盘选中「左侧」→ 全局外观真的变。
///
/// 菜单打开时自己拿焦点（`dropdown_menu.rs:140`），`down` 从未选中态落到第一项
/// （`popup_menu.rs:934`），`enter` 确认并触发 `on_select`。
#[gpui::test]
fn clicking_the_sidebar_side_dropdown_updates_the_appearance(cx: &mut TestAppContext) {
    let seen_nav_gap = Rc::new(Cell::new(f32::NAN));
    let seen_side = Rc::new(Cell::new(-1.));
    let cx = harness_window(cx, seen_nav_gap.clone(), seen_side.clone());

    let bounds = cx
        .debug_bounds(CONTROL)
        .expect("下拉控件应当排进布局（选择器没命中说明它没被渲染）");
    cx.simulate_click(
        point(bounds.center().x, bounds.center().y),
        Modifiers::none(),
    );
    draw(cx);

    // down：从未选中态选到第一项「左侧」；enter：确认。
    cx.simulate_keystrokes("down");
    draw(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);

    cx.update(|_, cx| {
        assert!(
            matches!(
                active_theme(cx).appearance().right_sidebar_side,
                SidebarSide::Left
            ),
            "点选「左侧」后全局外观必须变；还是「右」说明交互链断了"
        );
    });
    assert!(
        (seen_side.get() - 1.).abs() < f32::EPSILON,
        "选中「左侧」后，只读主题的视图必须在下一帧重渲出新侧别，实得 {}",
        seen_side.get()
    );
}
