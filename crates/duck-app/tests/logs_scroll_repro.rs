//! 「设置 → 日志」竖向滚动失效的无头复现。
//!
//! 结构逐层对照真实代码：`SettingsWindowView::render`（`pages/settings.rs`）的
//! 窗口根与内容行、`render_content` 的标题块 + 日志页专用壳（`flex_1().min_h_0()`，
//! **不套滚动容器**），以及壳里独立 entity 的 `LogView::render`（工具条 / 正文 /
//! 状态栏，正文 `overflow_scrollbar` 双向滚动）。每层都是同一个布局约束的复制品，
//! 只有装饰（颜色、字号、真实文案）被换成带 `debug_selector` 的定高行。
//!
//! 预期：在正文上派发竖向滚轮事件，正文内容必须移动。若本测试失败，则滚动失效
//! 是布局链问题；若通过，则问题在真实窗口的运行时路径（渲染器 / scroll layers）。

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseMoveEvent,
    ParentElement, Render, ScrollDelta, ScrollWheelEvent, Styled, TestAppContext, TouchPhase,
    VisualTestContext, Window, div, point, px,
};
use gpui_component::{h_flex, init as component_init, scroll::ScrollableElement as _, v_flex};

/// 滚轮事件 + 重绘。gpui-fast 的滚轮处理要求 `hitbox.should_handle_scroll`，
/// 即滚轮位置必须已在 `window.mouse_hit_test` 命中链里——所以先把指针移过去。
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

fn row(selector: &'static str, height: f32) -> gpui::Div {
    div()
        .h(px(height))
        .flex_shrink_0()
        .debug_selector(move || selector.to_string())
}

/// 独立 entity 的日志查看器复制品：逐层对照 `LogView::render`（`pages/logs.rs`）。
struct EmbeddedLogReplica;

impl Render for EmbeddedLogReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            // 工具条：`flex_none`（真实代码里 `.flex_none()` 在 `render_toolbar`）。
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .h(px(40.))
                    .child(row("log-toolbar", 40.)),
            )
            .child(Self::render_body())
            // 状态栏：`flex_none`。
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .h(px(28.))
                    .child(row("log-status", 28.)),
            )
    }
}

impl EmbeddedLogReplica {
    /// 逐层对照 `LogView::render_body`：`flex_1().min_h_0().w_full()` +
    /// `overflow_scrollbar()`（双向）+ `self_start()`（修复：取消 Scrollable
    /// flex-row 交叉轴对内容块的拉伸，否则纵向溢出消失、竖向滚不动），
    /// 内容是不换行的长文本（这里用 60 条定高行代替，纵向溢出等价）。
    fn render_body() -> gpui::AnyElement {
        div()
            .id("logs-text")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_scrollbar()
            .self_start()
            .child(div().whitespace_nowrap().children((0..60).map(|ix| {
                let selector = match ix {
                    0 => Some("log-first-row"),
                    59 => Some("log-last-row"),
                    _ => None,
                };
                let row = row("log-row", 20.);
                match selector {
                    Some(name) => row.debug_selector(move || name.to_string()),
                    None => row,
                }
            })))
            .into_any_element()
    }
}

/// 设置窗口复制品：`SettingsWindowView::render` 的窗口根 + 内容行 +
/// `render_content` 的 LOGS_PAGE 分支（标题块固定、日志壳不套滚动容器）。
struct SettingsLogsPageReplica {
    log_view: Entity<EmbeddedLogReplica>,
}

impl Render for SettingsLogsPageReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().min_w_0().min_h_0().child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .items_stretch()
                .child(Self::render_content(self.log_view.clone())),
        )
    }
}

impl SettingsLogsPageReplica {
    /// 逐层对照 `render_content` 的标题块（`flex_none`）与日志页壳
    /// （`div().flex_1().min_h_0().w_full()`，直接装查看器 entity）。
    fn render_content(log_view: Entity<EmbeddedLogReplica>) -> gpui::AnyElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .h(px(60.))
                    .child(row("page-title", 60.)),
            )
            .child(div().flex_1().min_h_0().w_full().child(log_view))
            .into_any_element()
    }
}

/// 设置页里的日志正文必须能竖向滚动。
#[gpui::test]
fn settings_embedded_log_body_scrolls_vertically(cx: &mut TestAppContext) {
    cx.update(component_init);
    let (_, cx) = cx.add_window_view(|_, cx| SettingsLogsPageReplica {
        log_view: cx.new(|_| EmbeddedLogReplica),
    });
    let cx: &mut VisualTestContext = cx;
    draw(cx);

    let initial = cx.debug_bounds("log-last-row").unwrap().origin.y;
    // 滚轮落在正文中部（窗口中部偏下），向上滚 80px。
    scroll(cx, 400., 400., 0., -80.);

    let after = cx.debug_bounds("log-last-row").unwrap().origin.y;
    assert!(
        after < initial,
        "滚轮之后正文应上移：initial={initial:?}, after={after:?}"
    );
}

/// 对照组：普通设置页（外层 `overflow_y_scrollbar`）在同一窗口结构下能滚——
/// 证明测试窗口、事件派发与断言本身是有效的。
#[gpui::test]
fn settings_regular_page_body_scrolls_vertically(cx: &mut TestAppContext) {
    cx.update(component_init);

    struct RegularPageReplica;

    impl Render for RegularPageReplica {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().min_w_0().min_h_0().child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_stretch()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                v_flex()
                                    .w_full()
                                    .flex_none()
                                    .h(px(60.))
                                    .child(row("page-title", 60.)),
                            )
                            // 普通页分支：`overflow_y_scrollbar` 的 body。
                            .child(
                                div()
                                    .id("settings-body")
                                    .flex_1()
                                    .min_h_0()
                                    .w_full()
                                    .overflow_y_scrollbar()
                                    // 测试窗口是 1920×1080：100 行 × 20px 才能保证
                                    // 真实溢出（40 行只有 800px，本就不该滚）。
                                    .children((0..100).map(|ix| {
                                        let selector = match ix {
                                            0 => Some("page-first-row"),
                                            99 => Some("page-last-row"),
                                            _ => None,
                                        };
                                        let row = row("page-row", 20.);
                                        match selector {
                                            Some(name) => {
                                                row.debug_selector(move || name.to_string())
                                            }
                                            None => row,
                                        }
                                    })),
                            ),
                    ),
            )
        }
    }

    let (_, cx) = cx.add_window_view(|_, _| RegularPageReplica);
    let cx: &mut VisualTestContext = cx;
    draw(cx);

    let initial = cx.debug_bounds("page-last-row").unwrap().origin.y;
    scroll(cx, 400., 400., 0., -80.);

    let after = cx.debug_bounds("page-last-row").unwrap().origin.y;
    assert!(
        after < initial,
        "对照组必须能滚：initial={initial:?}, after={after:?}"
    );
}
/// 环境裁决：逐字复制 gpui-component `scrollable.rs` 自带的
/// `OverflowScrollbarVerticalTest`（100×100 容器 + 5 行，**不带** `self_start`）。
/// 断言与上游一致：只验证双向 `overflow_scrollbar` 保留调用方的 gap。
///
/// 上游对这一形态只测过 gap 与横向滚动，从未测过纵向滚动；下面的
/// ignored 用例存档了原因——flex-row 交叉轴拉伸让纵向溢出消失（上游缺陷，
/// 本项目以 `self_start` 规避，见上方各复刻用例）。
#[gpui::test]
fn upstream_style_plain_scrollbar_preserves_gap(cx: &mut TestAppContext) {
    cx.update(component_init);

    struct PlainScrollableTest;

    impl Render for PlainScrollableTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .w(px(100.))
                .h(px(100.))
                .gap(px(10.))
                .overflow_scrollbar()
                .child(row("plain-first-row", 20.))
                .child(row("plain-second-row", 20.))
                .child(row("plain-third-row", 20.))
                .child(row("plain-fourth-row", 20.))
                .child(row("plain-last-row", 20.))
        }
    }

    let (_, cx) = cx.add_window_view(|_, _| PlainScrollableTest);
    let cx: &mut VisualTestContext = cx;
    draw(cx);

    let first = cx.debug_bounds("plain-first-row").unwrap();
    let second = cx.debug_bounds("plain-second-row").unwrap();
    assert_eq!(second.top() - first.bottom(), px(10.));
}

/// 存档上游缺陷：不带 `self_start` 的双向 `overflow_scrollbar` 纵向滚不动。
/// 上游修复（`Scrollable` 内容块取消交叉轴拉伸）落地后可移除 `#[ignore]`。
#[gpui::test]
#[ignore = "上游 gpui-component `Scrollable`（Both 轴）的交叉轴拉伸缺陷"]
fn upstream_style_plain_scrollbar_cannot_scroll_vertically(cx: &mut TestAppContext) {
    cx.update(component_init);

    struct PlainScrollableTest;

    impl Render for PlainScrollableTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .w(px(100.))
                .h(px(100.))
                .overflow_scrollbar()
                .child(row("plain-first-row", 20.))
                .child(row("plain-last-row", 20.))
        }
    }

    let (_, cx) = cx.add_window_view(|_, _| PlainScrollableTest);
    let cx: &mut VisualTestContext = cx;
    draw(cx);

    let initial = cx.debug_bounds("plain-last-row").unwrap().origin.y;
    scroll(cx, 10., 10., 0., -50.);
    let after = cx.debug_bounds("plain-last-row").unwrap().origin.y;
    assert!(
        after >= initial,
        "上游缺陷未修复前，不带 self_start 的双向滚动区纵向应推不动"
    );
}

/// 独立日志窗口复制品：`AuxiliaryWindowView` 的 chrome 壳 + 同一个查看器。
/// 用于区分「设置页独有」与「两处宿主共有」。
#[gpui::test]
fn standalone_log_window_body_scrolls_vertically(cx: &mut TestAppContext) {
    cx.update(component_init);

    struct StandaloneLogReplica {
        log_view: Entity<EmbeddedLogReplica>,
    }

    impl Render for StandaloneLogReplica {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // 对照 `aux.rs` chrome：窗口根是 flex **列**（标题条 + 内容壳），
            // 内容壳 `flex_1().min_h_0()` 才能从列布局里分到窗口高度——外壳若是
            // Block 布局，`flex_1` 失效，查看器会撑成内容高度而失去可滚的约束。
            v_flex()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(row("window-titlebar", 28.))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(self.log_view.clone()),
                )
        }
    }

    let (_, cx) = cx.add_window_view(|_, cx| StandaloneLogReplica {
        log_view: cx.new(|_| EmbeddedLogReplica),
    });
    let cx: &mut VisualTestContext = cx;
    draw(cx);

    let initial = cx.debug_bounds("log-last-row").unwrap().origin.y;
    scroll(cx, 400., 400., 0., -80.);

    let after = cx.debug_bounds("log-last-row").unwrap().origin.y;
    assert!(
        after < initial,
        "独立窗口的日志正文应上移：initial={initial:?}, after={after:?}"
    );
}
