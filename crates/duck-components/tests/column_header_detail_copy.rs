//! 列头悬停详情卡上的「复制」：点击必须把**整份列详情**写进剪贴板（无头回归）。
//!
//! 用户实机反馈：悬停列头出现详情卡（列名 / 类型 / 注释），点右上角的复制，
//! 剪贴板里什么都没有。本文件把那条路径整条跑一遍：悬停列头 → 等 `HoverCard`
//! 的开启延迟 → 点卡上的复制 → 读**测试平台的剪贴板**断言内容。
//!
//! # 踩过的坑（照抄机制，别自己重新推）
//!
//! 1. 详情卡由 `gpui_base::HoverCard` 承载（原生 `.tooltip()` 鼠标一进弹层就
//!    算离开，按钮点不到），有 `open_delay`（默认 0.6s）；测试必须 `advance_clock`
//!    越过它，否则卡根本没开、`debug_bounds` 拿不到按钮。
//! 2. `debug_bounds` 只认 `debug_selector`（`gpui/src/app/test_context.rs:990`），
//!    不认元素 id —— 所以生产代码里给复制按钮挂了一枚 `debug_selector`。
//! 3. ⚠️ **`HoverCard` 的内容闭包每帧都会被调一遍**（宿主每帧重渲，见
//!    `ProbeHost::render` 埋点：每 `draw` 一次就多一条）。闭包里 `cx.new(...)`
//!    现造的子视图**身份每帧都变**，卡上按钮的 `InteractiveElementState`
//!    （承载 `pending_mouse_down`）就跟着每帧重建 ⇒ `on_click` 的「按下→抬起」
//!    配对永远配不上。症状极具误导性：**`on_hover` / `on_mouse_down` /
//!    `on_mouse_up` 全都正常触发，就是 `on_click` 一次都不来**（它们不依赖配对）。
//!    ⇒ 生产代码（`table.rs::column_detail_card`）因此把卡片做成**内联的
//!    自由函数**，内容闭包直接建元素、绝不 `cx.new(...)`。下面三条对照测试把
//!    这个不变量钉死。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use duck_components::data_table::{
    CellKind, CellValue, ColumnSpec, DataTableHooks, DataTableLabels, DataTableState,
    DataTableView, HEADER_HEIGHT, ROWNUM_WIDTH, RowData, SELECT_COLUMN_WIDTH, TableModel,
    data_table,
};
use gpui::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    Modifiers, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use gpui_component::init as component_init;

/// `HoverCard` 的开启延迟是 0.6s（`gpui_base::HoverCard::new`），多给一点余量。
const OPEN_DELAY: Duration = Duration::from_millis(800);

// ── 用户场景：data_table 的列头详情卡 ────────────────────────────────────

struct Host {
    state: Entity<DataTableState>,
    labels: DataTableLabels,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = DataTableView::default();
        div().size_full().child(data_table(
            "header-detail-grid",
            &self.state,
            &view,
            &self.labels,
            &DataTableHooks::default(),
            cx,
        ))
    }
}

/// 一列（列名 / 类型 / 注释齐全）+ 一行，窗口开够大。
fn open_grid(cx: &mut TestAppContext) -> VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let state = cx.update(|cx| {
        let spec = ColumnSpec::new("opinion", "varchar(500)").with_comment("意见");
        let model = TableModel::new(
            vec![spec],
            vec![RowData::new(vec![CellValue::new(
                CellKind::Text,
                "值".to_string(),
            )])],
        );
        let state = cx.new(|_| DataTableState::new(Arc::new(model)));
        state.update(cx, |state, cx| state.set_view_rows((0..1).collect(), 0, cx));
        state
    });
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
    let host_state = state.clone();
    let (_, ctx) = cx.add_window_view(move |_, _| Host {
        state: host_state,
        labels,
    });
    // `add_window_view` 借出去的是 `&mut VisualTestContext`：拷一份出来自己拿着，
    // 免得函数签名把 `cx` 的生命周期一起拖进返回类型（与 `data_table_cost.rs` 同款）。
    let mut ctx = ctx.clone();
    ctx.simulate_resize(size(px(900.), px(600.)));
    draw(&mut ctx);
    ctx
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
}

/// 悬停第一个数据列的表头格，并把详情卡等出来。
fn open_detail_card(cx: &mut VisualTestContext) {
    // 行号列 + 多选列之后才是第一个数据列；纵向落在表头行内。
    let header = point(
        px(ROWNUM_WIDTH + SELECT_COLUMN_WIDTH + 60.),
        px(HEADER_HEIGHT / 2.),
    );
    cx.simulate_mouse_move(header, None, Modifiers::default());
    cx.run_until_parked();
    cx.executor().advance_clock(OPEN_DELAY);
    cx.run_until_parked();
    draw(cx);
}

#[gpui::test]
fn hovering_a_column_header_opens_the_detail_card(cx: &mut TestAppContext) {
    let mut cx = open_grid(cx);
    open_detail_card(&mut cx);
    assert!(
        cx.debug_bounds("column-detail-copy").is_some(),
        "悬停列头 0.6s 之后详情卡应当展开（卡上有复制按钮）"
    );
}

#[gpui::test]
fn clicking_copy_writes_the_column_detail_to_the_clipboard(cx: &mut TestAppContext) {
    let mut cx = open_grid(cx);
    open_detail_card(&mut cx);
    let copy = cx
        .debug_bounds("column-detail-copy")
        .expect("详情卡已展开，复制按钮应当有几何");

    cx.simulate_click(copy.center(), Modifiers::default());
    cx.run_until_parked();

    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(
        copied.as_deref(),
        Some("列名：opinion\n类型：varchar(500)\n注释：意见"),
        "点复制应当把「列名 / 类型 / 注释」整份写进剪贴板"
    );
}

// ── 机制对照：内容闭包「每帧现造子视图」为什么会吃掉点击 ──────────────────
//
// 与 data_table 无关的最小组合：`HoverCard` + 一个带 `on_click` 的按钮，只换
// 按钮的**承载形态**。三条断言把 §3 那条不变量钉死：生产代码不许在内容闭包
// 里 `cx.new(...)`。

/// 事件日志：把每个回调按发生顺序记下来。比只读剪贴板更能说明「哪一环没跑」，
/// 也顺手记录「宿主每帧重渲」这个前提。
type Log = Rc<RefCell<Vec<String>>>;

/// 按钮本体：悬停 / 按下 / 点击各记一笔，点击另写剪贴板。**内联建**，不套视图。
fn probe_button(log: &Log) -> impl IntoElement {
    let hover_log = log.clone();
    let down_log = log.clone();
    let click_log = log.clone();
    div()
        .id("probe-button")
        .debug_selector(|| "probe-button".into())
        .w(px(40.))
        .h(px(40.))
        .on_hover(move |hovered, _, _| {
            hover_log.borrow_mut().push(format!("on_hover({hovered})"));
        })
        .on_mouse_down(gpui::MouseButton::Left, move |_, _, _| {
            down_log.borrow_mut().push("on_mouse_down".into());
        })
        .on_click(move |_, _, cx| {
            click_log.borrow_mut().push("on_click".into());
            cx.write_to_clipboard(ClipboardItem::new_string("clicked".to_string()));
        })
}

/// 同一个按钮，但装进独立实体（子视图）。只在 [`CardShape::StableChildView`]
/// 与 [`CardShape::FreshChildView`] 下用得到。
struct ProbeButton {
    log: Log,
}

impl Render for ProbeButton {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        probe_button(&self.log)
    }
}

/// 按钮在 `HoverCard` 内容里的三种承载形态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardShape {
    /// 内容闭包直接建元素 —— **生产代码用的形态**。
    InlineElements,
    /// 内容闭包里 `cx.new(...)` 现造子视图：每次渲染都是新实体 ⇒ 点击失效。
    FreshChildView,
    /// 子视图由宿主持有、只造一次 ⇒ 点击正常。
    StableChildView,
}

struct CardHost {
    shape: CardShape,
    log: Log,
    stable: Entity<ProbeButton>,
}

impl Render for CardHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let shape = self.shape;
        let log = self.log.clone();
        let stable = self.stable.clone();
        self.log.borrow_mut().push("CardHost::render".into());
        div().size_full().child(
            gpui_base::HoverCard::new("probe-card")
                .trigger(
                    div()
                        .id("probe-trigger")
                        .debug_selector(|| "probe-trigger".into())
                        .w(px(120.))
                        .h(px(30.)),
                )
                // ⚠️ 三个参数全部显式标注：内容构造闭包要装进 `Box<dyn FnOnce(..)>`
                // （higher-ranked），留空参推断器给不出 HRTB。
                .content(
                    move |_: &mut gpui_base::HoverCardState,
                          _: &mut Window,
                          cx: &mut Context<gpui_base::HoverCardState>| {
                        match shape {
                            CardShape::FreshChildView => {
                                let fresh = cx.new(|_| ProbeButton { log: log.clone() });
                                div().id("probe-body").child(fresh)
                            }
                            CardShape::StableChildView => {
                                div().id("probe-body").child(stable.clone())
                            }
                            CardShape::InlineElements => {
                                div().id("probe-body").child(probe_button(&log))
                            }
                        }
                    },
                ),
        )
    }
}

fn card_window(cx: &mut TestAppContext, shape: CardShape, log: Log) -> VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let host_log = log.clone();
    let (_, ctx) = cx.add_window_view(move |_, cx| {
        let stable = cx.new(|_| ProbeButton { log: host_log });
        CardHost { shape, log, stable }
    });
    let mut ctx = ctx.clone();
    ctx.simulate_resize(size(px(600.), px(400.)));
    draw(&mut ctx);
    ctx
}

/// 悬停触发元素 → 等卡开 → 移到按钮上 → 点一下，返回剪贴板内容。
fn card_click(cx: &mut VisualTestContext) -> Option<String> {
    let trigger = cx
        .debug_bounds("probe-trigger")
        .expect("触发元素应当有几何");
    cx.simulate_mouse_move(trigger.center(), None, Modifiers::default());
    cx.run_until_parked();
    cx.executor().advance_clock(OPEN_DELAY);
    cx.run_until_parked();
    draw(cx);
    let target = cx
        .debug_bounds("probe-button")
        .expect("弹层里的按钮应当有几何");
    cx.simulate_mouse_move(target.center(), None, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(target.center(), Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

#[gpui::test]
fn inline_card_content_receives_clicks(cx: &mut TestAppContext) {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let mut cx = card_window(cx, CardShape::InlineElements, log.clone());
    assert_eq!(
        card_click(&mut cx).as_deref(),
        Some("clicked"),
        "内容闭包直接建元素时，on_click 应当收得到点击（生产代码就是这个形态）"
    );
}

#[gpui::test]
fn stable_child_view_card_content_receives_clicks(cx: &mut TestAppContext) {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let mut cx = card_window(cx, CardShape::StableChildView, log.clone());
    assert_eq!(
        card_click(&mut cx).as_deref(),
        Some("clicked"),
        "子视图只要**身份稳定**，on_click 一样收得到点击"
    );
}

/// 负向对照：**这条记录的是坑本身**，不是期望行为。
///
/// 内容闭包里 `cx.new(...)` 现造子视图 ⇒ 每次渲染都是新实体 ⇒ 按钮的
/// `InteractiveElementState` 每帧重建 ⇒ `on_click` 的「按下→抬起」永远配不上。
/// 断言按症状写死：悬停、按下都照常，唯独点击不来。谁要是在 `table.rs` 的内容
/// 闭包里重新塞 `cx.new(...)`，这里不会红 —— 但 `table.rs` 里那条用户场景
/// （`clicking_copy_writes_the_column_detail_to_the_clipboard`）会红。
#[gpui::test]
fn per_frame_child_view_card_content_loses_clicks(cx: &mut TestAppContext) {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let mut cx = card_window(cx, CardShape::FreshChildView, log.clone());
    let copied = card_click(&mut cx);
    let events = log.borrow().clone();
    assert_eq!(
        copied, None,
        "每帧现造子视图时，on_click 收不到（这就是用户报的 bug 的机制）"
    );
    assert!(
        events.iter().any(|e| e == "on_hover(true)"),
        "同一条链上悬停是通的（所以这个 bug 才难查）：{events:?}"
    );
    assert!(
        events.iter().any(|e| e == "on_mouse_down"),
        "按下也是通的，唯独点击不来：{events:?}"
    );
}
