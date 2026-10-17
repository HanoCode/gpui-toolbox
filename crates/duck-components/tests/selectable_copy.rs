//! 内容区文字「拖选 + ⌘C 复制」的无头回归。
//!
//! 用户实机反馈：界面里大量文字（日志正文、许可全文、错误详情…）选不中、复制不了。
//! 根因：引擎（`gpui_base`）的文本选中是**元素主动报名**的 —— 只有把文本交给
//! `SelectableText`，它才会在 prepaint 阶段把自己的命中区注册进窗口级 `TextSelection`、
//! 在 paint 阶段注册文本 run；`gpui_base::Root` 里的 `TextSelectionLayer` 负责画选区，
//! `⌘C` 负责取文字。gpui 的普通文本子元素（`div().child("文字")`）**不报名**，
//! 因此天然不可选中 —— 这是框架行为，不是遗漏。
//!
//! 本仓入口：[`duck_components::selectable_text`]（id 取调用点）与
//! [`duck_components::selectable_text_with_id`]（id 由调用方给）。
//!
//! # 两条**尚未打通**的通路（都有断言钉住，别当成已支持）
//!
//! 1. **`uniform_list` 的行**（结果网格的单元格、各类虚拟列表行）：无论拖在哪一行
//!    哪一列都选不中（见 [`virtual_list_rows_are_not_selectable_yet`]）。所以
//!    `data_table` 只做了**列头**（列名 / 类型 / 注释），单元格正文没做。
//! 2. 若 [`selection_inside_a_scroll_container_works`] 变红，说明滚动容器也没打通，
//!    那么「日志正文 / 许可全文」这两处（都在滚动容器里）同样只是摆设。

use std::sync::Arc;

use duck_components::data_table::{
    CellKind, CellValue, ColumnSpec, DataTableHooks, DataTableLabels, DataTableState,
    DataTableView, RowData, TableModel, data_table,
};
use duck_components::{selectable_text, selectable_text_with_id};
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
    ParentElement as _, Pixels, Render, Size, Styled as _, TestAppContext, VisualTestContext,
    Window, div, point, px, size,
};
use gpui_base::TextSelection;
use gpui_component::Root;
use gpui_component::init as component_init;
use gpui_component::scroll::ScrollableElement as _;

/// 把内容视图套进 `gpui_component::Root` —— 与生产代码同一条装配路径，
/// 保证 `TextSelectionLayer` 在场、⌘C 有 `Copy` 可派发。
fn open_root<V: Render + 'static>(
    cx: &mut TestAppContext,
    window_size: Size<Pixels>,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V + 'static,
) -> VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let (_, ctx) = cx.add_window_view(move |window, cx| {
        let view = cx.new(|cx| build(window, cx));
        Root::new(view, window, cx)
    });
    let mut ctx = ctx.clone();
    ctx.simulate_resize(window_size);
    draw(&mut ctx);
    ctx
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 从 `from` 拖到 `to`（按住左键），松手后再画一帧。
fn drag(cx: &mut VisualTestContext, from: gpui::Point<Pixels>, to: gpui::Point<Pixels>) {
    cx.simulate_mouse_move(from, None, Modifiers::default());
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    draw(cx);
}

fn selected_text(cx: &mut VisualTestContext) -> String {
    cx.update(|window, cx| TextSelection::selected_text(window, cx))
}

// ── ① 两段独立文本：拖选跨段，且按 document_order 拼接 ─────────────────────
//
// 剪贴板那一跳（⌘C）不在这里断言：`⌘C → Copy → TextSelection::selected_text →
// write_to_clipboard` 整条都在 `gpui_base::Root` 里（`vendor/gpui-base/src/root.rs`），
// 本仓只负责让文字**进得了选区**。无头环境里窗口没有焦点，键事件派发不出去，
// 断言它只会测出测试环境的噪音。这里断言到「选区内容正确」为止。

struct TwoRuns;

impl Render for TwoRuns {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap(px(8.))
            .p(px(8.))
            .child(
                div()
                    .w(px(300.))
                    .h(px(32.))
                    .child(selectable_text("alpha").document_order(0)),
            )
            .child(
                div()
                    .w(px(300.))
                    .h(px(32.))
                    .child(selectable_text_with_id("second-run", "beta").document_order(1)),
            )
    }
}

#[gpui::test]
fn content_text_is_drag_selectable_and_copyable(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(600.), px(400.)), |_, _| TwoRuns);

    // 从第一段文字的左端拖到第二段文字上。
    drag(&mut cx, point(px(10.), px(20.)), point(px(120.), px(60.)));

    assert_eq!(
        selected_text(&mut cx),
        "alpha\nbeta",
        "拖选跨两段文字时应当两段都选中，并按 document_order 拼接"
    );
}

// ── ② 滚动容器里的文字 ────────────────────────────────────────────────────
//
// 日志正文、许可全文都在 gpui-component 的滚动容器里。这条决定那两处是不是真的可用。

struct ScrolledBody;

impl Render for ScrolledBody {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().size_full().child(
            div()
                .id("scrolled-body")
                .flex_1()
                .min_h_0()
                .w_full()
                .p(px(8.))
                .overflow_scrollbar()
                .child(selectable_text("scrolled body text")),
        )
    }
}

#[gpui::test]
fn selection_inside_a_scroll_container_works(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(600.), px(400.)), |_, _| ScrolledBody);
    drag(&mut cx, point(px(10.), px(16.)), point(px(300.), px(16.)));
    assert_eq!(
        selected_text(&mut cx),
        "scrolled body text",
        "滚动容器里的文字必须同样可选（日志正文 / 许可全文就落在这条通路上）"
    );
}

// ── ③ 负向对照：没有报名选区的普通文本，拖了也选不中 ─────────────────────
//
// 谁要是把内容区的文字改回 `div().child("文字")`，这里不会红，但 ①② 会红。
// 留它在这里是为了说明「选中链路不是自动的」：必须显式报名。

struct PlainText;

impl Render for PlainText {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p(px(8.))
            .child(div().w(px(300.)).h(px(32.)).child("plain text"))
    }
}

#[gpui::test]
fn plain_text_child_is_not_selectable(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(600.), px(400.)), |_, _| PlainText);
    drag(&mut cx, point(px(10.), px(20.)), point(px(120.), px(20.)));
    assert_eq!(
        selected_text(&mut cx),
        "",
        "裸文本子元素不参与选区 —— 这正是用户报的「选不中」"
    );
}

// ── ④ 已探明的引擎限制：`uniform_list` 的行选不中 ─────────────────────────
//
// 结果网格的单元格、各类虚拟列表的行都落在这里。断言按**现状**写：
// 谁把这条打通了（引擎侧修好），这条会红 —— 那时应当反转为正向断言，
// 并把 `data_table` 的单元格也接上 `selectable_text_with_id`。

struct VirtualListBody;

impl Render for VirtualListBody {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex_none()
                    .h(px(120.))
                    .w_full()
                    .child(gpui::uniform_list("probe-list", 1, |range, _, _| {
                        range
                            .map(|i| {
                                div()
                                    .h(px(24.))
                                    .w_full()
                                    .child(selectable_text_with_id(("probe-row", i), "alpha-cell"))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    })),
            )
    }
}

#[gpui::test]
fn virtual_list_rows_are_not_selectable_yet(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(600.), px(400.)), |_, _| VirtualListBody);
    let mut hits = 0;
    for y in [4., 12., 20., 28.] {
        drag(&mut cx, point(px(2.), px(y)), point(px(300.), px(y)));
        if !selected_text(&mut cx).is_empty() {
            hits += 1;
        }
    }
    assert_eq!(
        hits, 0,
        "现状：`uniform_list` 行内注册的可选中文本选不中（引擎侧限制）。\
         若这条开始失败，说明已打通 —— 请把断言反转，并给 data_table 的单元格接上可选中文本"
    );
}

// ── ⑤ data_table 的列头（已经接上的那一处） ───────────────────────────────

struct GridHost {
    state: Entity<DataTableState>,
    labels: DataTableLabels,
}

impl Render for GridHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = DataTableView::default();
        // ⚠️ 必须给 flex 列容器：`data_table` 的根节点是 `flex_1().min_h_0()`，
        // 落在块级父容器里高度会塌成 0，行区一行都不渲染。
        div().flex().flex_col().size_full().child(data_table(
            "selectable-head-grid",
            &self.state,
            &view,
            &self.labels,
            &DataTableHooks::default(),
            cx,
        ))
    }
}

#[gpui::test]
fn data_table_column_header_text_is_drag_selectable(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(900.), px(600.)), |_, cx| {
        let spec = ColumnSpec::new("opinion", "varchar(64)");
        let model = TableModel::new(
            vec![spec],
            vec![RowData::new(vec![CellValue::new(
                CellKind::Text,
                "alpha-cell".to_string(),
            )])],
        );
        let state = cx.new(|_| DataTableState::new(Arc::new(model)));
        state.update(cx, |state, cx| state.set_view_rows((0..1).collect(), 0, cx));
        let labels = DataTableLabels::new(
            "全选",
            "第 {} 行",
            "按 {} 排序",
            "提交",
            "单元格详情",
            "{} 的列操作",
            "列名",
            "类型",
            "注释",
            "复制详情",
            "：",
        );
        GridHost { state, labels }
    });

    // 列头第一行是列名（表头 56px 高：列名一行在上 ~12-28px，类型一行在下 ~32-44px）。
    // 起点落在列内 20px 处（命中的前提是**起点必须在文字命中区内**），终点往右拉出去即可。
    drag(&mut cx, point(px(98.), px(20.)), point(px(300.), px(20.)));
    let dragged = selected_text(&mut cx);
    assert!(
        !dragged.is_empty() && "opinion".ends_with(dragged.trim_end()),
        "拖选列头应当拖出列名的一段，实际拖到 {dragged:?}"
    );
}

// ── ⑥ 同一处代码渲染多份（循环）：自动 id 必须按文本区分 ───────────────────
//
// 这是整批改造的**安全前提**。引擎按 `TextSelectionHandle` 的**实体**归档参与者，
// 而 handle 是按**元素 id** 保留的 ⇒ 同一个 id 渲染出的两份文字会共用 handle，
// **后注册的覆盖先注册的**，先那份静默失去可选中（不 panic、不报错，最难查）。
// `selectable_text` 的 id 取「调用点 + 文本内容」正是为了这个场景：调用点相同、
// 文本不同 ⇒ id 不同 ⇒ 各自独立。
//
// 若哪天把 id 退回纯 `CodeLocation`（调用点），这条会红：拖第 2 行会得到空串
// （命中区落在最后注册的那一行上）或得到第 3 行的文字。

struct LoopRows;

impl Render for LoopRows {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col().size_full().p(px(8.));
        for text in ["row-one", "row-two", "row-three"] {
            column = column.child(
                div()
                    .w(px(300.))
                    .h(px(24.))
                    // ⚠️ 同一个调用点、三份不同文字。
                    .child(selectable_text(text)),
            );
        }
        column
    }
}

#[gpui::test]
fn repeated_text_runs_from_one_call_site_stay_independent(cx: &mut TestAppContext) {
    let mut cx = open_root(cx, size(px(600.), px(400.)), |_, _| LoopRows);

    // 行高 24 + 容器 padding 8：第 i 行的垂直中心 = 8 + 24i + 12。
    for (index, expected) in ["row-one", "row-two", "row-three"].into_iter().enumerate() {
        let y = 8. + 24. * index as f32 + 12.;
        drag(&mut cx, point(px(9.), px(y)), point(px(300.), px(y)));
        let dragged = selected_text(&mut cx);
        assert!(
            dragged.contains(expected),
            "拖第 {} 行应选中 {expected}，实际拖到 {dragged:?}",
            index + 1
        );
        for other in ["row-one", "row-two", "row-three"] {
            if other != expected {
                assert!(
                    !dragged.contains(other),
                    "拖第 {} 行串到了 {other}（同一调用点的多份文字共用了 handle）：{dragged:?}",
                    index + 1
                );
            }
        }
    }
}
