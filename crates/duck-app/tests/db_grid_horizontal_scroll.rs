//! 数据库结果网格**横向滚动**的无头回归。
//!
//! 结构逐层对照真实代码 `pages/db.rs::render_result` 的结果区：
//! `panel`（`v_flex().flex_1().min_h_0().min_w_0()`）→ 过滤条等固定条（`flex_none`）
//! → 横向滚动区：`flex_row` + `overflow_x_scroll` + `restrict_scroll_to_axis` +
//! `track_scroll`（持久 `ScrollHandle`），内容块 `flex_none().w_auto().min_w_full()`
//! （`header` + 行网格），常驻 `Scrollbar::horizontal(..).mode(ScrollbarMode::Always)`
//! 以 absolute 兄弟节点叠加；行区在真实代码里是 `uniform_list`
//! （`"db-result-grid"`），列头与行都是 `flex_none` 固定像素宽的单元格。
//!
//! 三条断言：
//! 1. 横向滚轮 → 内容左移（右侧列可见）——修复的直接目标；
//! 2. 竖向滚轮 → 行上移，且**横向偏移不变**（`restrict_scroll_to_axis` 锁轴，
//!    竖滚轮不得被映射成横滚）；
//! 3. 对照组：同一结构**不套**滚动区时，横向滚轮推不动内容（复现原缺陷——
//!    列宽之和超过视口后被整块裁掉）。

use gpui::{
    Context, InteractiveElement as _, IntoElement, MouseMoveEvent, ParentElement, Render,
    ScrollDelta, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement as _, Styled,
    TestAppContext, TouchPhase, UniformListScrollHandle, VisualTestContext, Window, div, point, px,
    uniform_list,
};
use gpui_component::{
    h_flex, init as component_init,
    scroll::{Scrollbar, ScrollbarMode},
    v_flex,
};

/// 结果网格的几何常量：与 `db.rs` 同量级（行高 24、多选列 36、列宽 200）。
const GRID_ROW_HEIGHT: f32 = 24.;
const SELECT_COLUMN_WIDTH: f32 = 36.;
const COLUMN_WIDTH: f32 = 200.;
/// 列数 × 列宽 = 2400 + 36 > 测试窗口宽 1920 → 必然横向溢出。
const COLUMN_COUNT: usize = 12;
/// 行数 × 行高 = 4800 > 测试窗口高 1080 → 必然纵向溢出。
const ROW_COUNT: usize = 200;
/// 探针行：必须落在首屏可见区间内。`uniform_list` 对屏外行**根本不渲染**，
/// 拿末行当探针会永远取不到 bounds（首版测试即栽在这里）。
const PROBE_ROW: usize = 20;

/// 滚轮事件 + 重绘（与 `tests/logs_scroll_repro.rs` 同源：gpui 的滚轮处理要求
/// `hitbox.should_handle_scroll`，指针必须先落进命中链）。
fn scroll(cx: &mut VisualTestContext, x: f32, y: f32, dx: f32, dy: f32) {
    cx.simulate_event(MouseMoveEvent {
        position: point(px(x), px(y)),
        ..Default::default()
    });
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(x), px(y)),
        delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    draw(cx);
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 定宽单元格；`selector` 只给「用于断言的探针单元格」。
fn cell(width: f32, selector: Option<&'static str>) -> gpui::Div {
    let cell = div().w(px(width)).flex_none().h(px(GRID_ROW_HEIGHT));
    match selector {
        Some(name) => cell.debug_selector(move || name.to_string()),
        None => cell,
    }
}

/// 一行：多选列 + `COLUMN_COUNT` 个定宽单元格（对照 `render_result` 的行结构）。
fn grid_row(row_index: usize) -> gpui::AnyElement {
    let probe = row_index == PROBE_ROW;
    h_flex()
        .h(px(GRID_ROW_HEIGHT))
        .w_full()
        .flex_none()
        .overflow_x_hidden()
        .child(cell(SELECT_COLUMN_WIDTH, None))
        .children((0..COLUMN_COUNT).map(|column_index| {
            let is_last_column = column_index + 1 == COLUMN_COUNT;
            // 探针列：末列用于横向断言（横滚时必须左移），首列用于纵向断言。
            let selector = match (probe, column_index) {
                (true, 0) => Some("db-probe-first-cell"),
                (true, _) if is_last_column => Some("db-probe-last-cell"),
                _ => None,
            };
            cell(COLUMN_WIDTH, selector)
        }))
        .into_any_element()
}

/// 行区：真实代码里的 `uniform_list("db-result-grid", ..).flex_1().min_h_0()`。
fn grid(scroll_handle: &UniformListScrollHandle) -> impl IntoElement {
    uniform_list("db-result-grid", ROW_COUNT, move |range, _window, _cx| {
        range
            .filter(|index| *index < ROW_COUNT)
            .map(grid_row)
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h_0()
    .track_scroll(scroll_handle)
}

/// 列头：固定宽单元格（对照 `render_result` 的 `header`）。
fn header() -> gpui::AnyElement {
    h_flex()
        .flex_none()
        .h(px(GRID_ROW_HEIGHT))
        .w_full()
        .overflow_x_hidden()
        .child(cell(SELECT_COLUMN_WIDTH, None))
        .children((0..COLUMN_COUNT).map(|_| cell(COLUMN_WIDTH, None)))
        .into_any_element()
}

/// 结果区复制品：`wrap` = true 时按修复后的形态把「列头 + 行网格」装进横向滚动区
/// 并叠加常驻滚动条，false 时对照修复前的形态（直接挂在 `panel` 下）。
struct ResultGridReplica {
    wrap: bool,
    scroll_handle: UniformListScrollHandle,
    /// 横向滚动句柄：必须**跨帧持久**（与真实代码的 `result_x_scroll` 同口径），
    /// 每帧新建会让滚轮写进来的偏移在下一帧丢掉。
    x_scroll: ScrollHandle,
}

impl Render for ResultGridReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = v_flex().flex_1().min_h_0().min_w_0().child(
            // 过滤条：固定条，留在滚动区外。
            h_flex().w_full().flex_none().h(px(40.)),
        );
        if self.wrap {
            // 内容块：`flex_none().w_auto().min_w_full()`，宽度由定宽列头撑出。
            let content = v_flex()
                .flex_none()
                .w_auto()
                .min_w_full()
                .child(header())
                .child(grid(&self.scroll_handle));
            let area = div()
                .id("db-result-x-area")
                .size_full()
                .flex()
                .flex_row()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(&self.x_scroll)
                .child(content);
            panel = panel.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .relative()
                    .child(area)
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::horizontal(&self.x_scroll)
                                .id("db-result-x-scrollbar")
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                    ),
            );
        } else {
            // 对照组 = 修复前的形态：列头 + 行网格直接挂在 `panel` 下，没有任何
            // 横向滚动容器（连滚动区都不建，否则对照组测的就不是原缺陷）。
            panel = panel.child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(header())
                    .child(grid(&self.scroll_handle)),
            );
        }
        v_flex().size_full().min_w_0().min_h_0().child(panel)
    }
}

fn add_grid_view(cx: &mut TestAppContext, wrap: bool) -> &mut VisualTestContext {
    cx.update(component_init);
    let (_, cx) = cx.add_window_view(|_, _| ResultGridReplica {
        wrap,
        scroll_handle: UniformListScrollHandle::new(),
        x_scroll: ScrollHandle::new(),
    });
    draw(cx);
    cx
}

/// 横向滚轮必须把内容推左，让最右侧的列进入视口。
#[gpui::test]
fn db_grid_scrolls_horizontally(cx: &mut TestAppContext) {
    let cx = add_grid_view(cx, true);

    let before = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;
    scroll(cx, 400., 400., -120., 0.);
    let after = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;

    assert!(
        after < before,
        "横向滚轮之后内容应左移：before={before:?}, after={after:?}"
    );
}

/// 竖向滚轮只驱动行列表纵向滚动，不得把列一起横移（锁轴）。
#[gpui::test]
fn db_grid_vertical_wheel_does_not_push_columns_sideways(cx: &mut TestAppContext) {
    let cx = add_grid_view(cx, true);

    let row_before = cx.debug_bounds("db-probe-first-cell").unwrap().origin.y;
    let column_before = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;
    scroll(cx, 400., 400., 0., -80.);
    let row_after = cx.debug_bounds("db-probe-first-cell").unwrap().origin.y;
    let column_after = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;

    assert!(
        row_after < row_before,
        "竖向滚轮之后行应上移：before={row_before:?}, after={row_after:?}"
    );
    assert_eq!(
        column_after, column_before,
        "竖向滚轮不得改变横向偏移：before={column_before:?}, after={column_after:?}"
    );
}

/// 对照组：修复前的形态（不套横向滚动区）横向滚轮推不动内容。
#[gpui::test]
fn db_grid_without_wrapper_cannot_scroll_horizontally(cx: &mut TestAppContext) {
    let cx = add_grid_view(cx, false);

    let before = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;
    scroll(cx, 400., 400., -120., 0.);
    let after = cx.debug_bounds("db-probe-last-cell").unwrap().origin.x;

    assert_eq!(
        after, before,
        "未修复形态下横向滚轮本就无效（这正是右侧列被裁掉的原因）：before={before:?}, after={after:?}"
    );
}
