//! 行模式网格的**逐帧成本探针**（诊断用）。
//!
//! 为什么要有它：实机结论是「数据表数据一多就卡」，但改代码前得先知道钱花在哪。
//! 这里叠了两把尺子：
//!
//! 1. [`gpui::Window::layout_stats`]（`test-support` 下公开）—— 逐帧的
//!    `nodes_created` / `lines_shaped` / `layer_frames_composited` /
//!    `views_built` / `tiles_dirtied`。它一眼能分出两种截然不同的局面：
//!    **「这一帧把整屏可见行都重建了」** 与 **「只补了新露出来的那几行」**。
//! 2. 一个**分配计数器**（`CountingAllocator`）—— 分配次数与 profile 无关，
//!    是比墙钟更硬的指标。
//!
//! 帧协议与 `gpui_perf` 的 runner 一致：先跑「这一步」（滚动 / 改状态），
//! 再让平台把随之而来的那一帧画掉；一帧都没画就显式补一帧。
//!
//! ⚠️ 别用 `VisualTestContext::draw(origin, space, element)` 画帧 —— 那条路会
//! 把 `window.refreshing` 置真，**retained mode 与 scroll layers 全部失效**
//!（`gpui/src/fast/retained.rs::draw_outside_frames`）。本文件走
//! `cx.update(|window, cx| window.draw(cx))`，与实机同一条路。
//!
//! 跑法：
//! ```text
//! cargo test -p duck-components --test data_table_cost -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use duck_components::DuckIcon;
use duck_components::data_table::{
    CellKind, CellValue, ColumnSpec, DataTableHooks, DataTableLabels, DataTableState,
    DataTableView, RowData, TableModel, data_table,
};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PlatformInput, Render, ScrollDelta, ScrollStrategy, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, TestAppContext, TouchPhase,
    VisualTestContext, Window, div, point, px, rgb, size,
};
use gpui_component::{
    Icon, h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    tooltip::Tooltip,
    v_flex,
};

// ── 分配计数器 ────────────────────────────────────────────────────────

struct CountingAllocator;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}

// ── 夹具 ──────────────────────────────────────────────────────────────

/// 窗口尺寸（贴近实机数据库页）。
const WINDOW: (f32, f32) = (1400., 860.);

/// 造点像真的数据：短数字 / 英文名 / NULL / 长 JSON / 中英混排。
fn cell_text(row: usize, col: usize) -> SharedString {
    match col % 5 {
        0 => format!("{}", row * 31 + col).into(),
        1 => format!("name-{row}-{col}").into(),
        2 => "NULL".into(),
        3 => format!(
            "{{\"id\":{row},\"kind\":\"event\",\"payload\":\"{}\"}}",
            "x".repeat(24)
        )
        .into(),
        _ => format!("备注-{row}-{col}-中文单元格").into(),
    }
}

fn build_model(rows: usize, cols: usize) -> TableModel {
    TableModel::new(
        (0..cols)
            .map(|col| ColumnSpec::new(format!("column_{col}"), "TEXT"))
            .collect(),
        (0..rows)
            .map(|row| {
                RowData::new(
                    (0..cols)
                        .map(|col| CellValue::new(CellKind::Text, cell_text(row, col).to_string()))
                        .collect(),
                )
            })
            .collect(),
    )
}

/// 生产形状的钩子：单元格右键菜单 + 列头右键菜单 + 两个双击。
fn production_hooks() -> DataTableHooks {
    let cell_menu = Rc::new(
        |menu: PopupMenu, _: _, _: &mut Window, _: &mut Context<PopupMenu>| {
            menu.item(PopupMenuItem::new("copy"))
        },
    );
    let header_menu = Rc::new(
        |menu: PopupMenu, _: usize, _: &mut Window, _: &mut Context<PopupMenu>| {
            menu.item(PopupMenuItem::new("sort"))
        },
    );
    let cell_dbl = Rc::new(|_: _, _: &mut Window, _: &mut gpui::App| {});
    let row_dbl = Rc::new(|_: usize, _: &mut Window, _: &mut gpui::App| {});
    DataTableHooks {
        cell_menu: Some(cell_menu),
        header_menu: Some(header_menu),
        on_cell_double_click: Some(cell_dbl),
        on_row_double_click: Some(row_dbl),
    }
}

// ── 一帧的读数 ────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct Costs {
    allocs: u64,
    bytes: u64,
    wall_ms: f64,
    frames: u64,
    nodes_created: u64,
    nodes_reused: u64,
    lines_shaped: u64,
    views_built: u64,
    views_reused: u64,
    layers_composited: u64,
    layers_repainted: u64,
    tiles_dirtied: u64,
    build_ms: f64,
    prepaint_ms: f64,
    paint_ms: f64,
    forced: bool,
}

impl Costs {
    /// 读的是**自上一步清零以来**的累计值（`reset_layout_stats`）。
    fn read(cx: &mut VisualTestContext) -> Self {
        cx.update(|window, _| {
            let stats = window.layout_stats();
            Self {
                frames: stats.frames,
                nodes_created: stats.nodes_created,
                nodes_reused: stats.nodes_reused,
                lines_shaped: stats.lines_shaped,
                views_built: stats.views_built,
                views_reused: stats.views_reused,
                layers_composited: stats.layer_frames_composited,
                layers_repainted: stats.layer_frames_repainted,
                tiles_dirtied: stats.tiles_dirtied,
                build_ms: stats.build_time.as_secs_f64() * 1000.,
                prepaint_ms: stats.prepaint_time.as_secs_f64() * 1000.,
                paint_ms: stats.paint_time.as_secs_f64() * 1000.,
                ..Self::default()
            }
        })
    }
}

/// 走一帧：先跑「这一步」，再让平台把随之而来的那一帧画掉；一帧都没画就补一帧。
fn step_frame(cx: &mut VisualTestContext, step: impl FnOnce(&mut VisualTestContext)) -> Costs {
    let (allocs_before, bytes_before) = allocations();
    cx.update(|window, _| window.reset_layout_stats());
    let started = Instant::now();

    step(cx);
    cx.run_until_parked();
    let mut cost = Costs::read(cx);
    if cost.frames == 0 {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        cost = Costs::read(cx);
        cost.forced = true;
    }

    cost.wall_ms = started.elapsed().as_secs_f64() * 1000.;
    let (allocs_after, bytes_after) = allocations();
    cost.allocs = allocs_after - allocs_before;
    cost.bytes = bytes_after - bytes_before;
    cost
}

fn mean(costs: &[Costs], f: fn(&Costs) -> u64) -> f64 {
    costs.iter().map(f).sum::<u64>() as f64 / costs.len().max(1) as f64
}

fn mean_ms(costs: &[Costs], f: fn(&Costs) -> f64) -> f64 {
    costs.iter().map(f).sum::<f64>() / costs.len().max(1) as f64
}

fn report(label: &str, costs: &[Costs]) {
    eprintln!(
        "{label:<26} | 分配 {:>7.0} 次 {:>6.0} KB | 墙钟 {:>6.2} ms | 帧 {:>3.0} | 建节点 {:>6.0} 复用 {:>6.0} | 排版行 {:>5.0} | 视图 built/reuse {:>5.1}/{:<5.1} | 层合成 {:>3.0} 重绘 {:>3.0} | 脏块 {:>6.0} | build {:>5.2} prepaint {:>5.2} paint {:>5.2} ms",
        mean(costs, |c| c.allocs),
        mean(costs, |c| c.bytes) / 1024.,
        mean_ms(costs, |c| c.wall_ms),
        mean(costs, |c| c.frames),
        mean(costs, |c| c.nodes_created),
        mean(costs, |c| c.nodes_reused),
        mean(costs, |c| c.lines_shaped),
        mean(costs, |c| c.views_built),
        mean(costs, |c| c.views_reused),
        mean(costs, |c| c.layers_composited),
        mean(costs, |c| c.layers_repainted),
        mean(costs, |c| c.tiles_dirtied),
        mean_ms(costs, |c| c.build_ms),
        mean_ms(costs, |c| c.prepaint_ms),
        mean_ms(costs, |c| c.paint_ms),
    );
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
}

/// 预热：字型缓存、行层、布局树都要先捂热，头几帧的读数没有意义。
fn warmup(cx: &mut VisualTestContext, frames: usize) {
    for _ in 0..frames {
        step_frame(cx, |_| {});
    }
}

// ── ① 行模式网格：滚动 / 全量重绘 ────────────────────────────────────

struct Host {
    state: Entity<DataTableState>,
    labels: DataTableLabels,
    hooks: DataTableHooks,
    sortable: bool,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = DataTableView {
            sortable: self.sortable,
            ..Default::default()
        };
        div().size_full().flex().flex_col().child(data_table(
            "probe-grid",
            &self.state,
            &view,
            &self.labels,
            &self.hooks,
            cx,
        ))
    }
}

fn open_grid(
    cx: &mut TestAppContext,
    rows: usize,
    cols: usize,
    production: bool,
) -> (VisualTestContext, Entity<DataTableState>, Entity<Host>) {
    let state = cx.update(|cx| {
        let state = cx.new(|_| DataTableState::new(Arc::new(build_model(rows, cols))));
        state.update(cx, |state, cx| {
            state.set_view_rows((0..rows).collect(), 0, cx)
        });
        state
    });
    let hooks = if production {
        production_hooks()
    } else {
        DataTableHooks::default()
    };
    let labels = DataTableLabels::new("all", "row {}", "sort {}", "submit", "detail");
    let host_state = state.clone();
    let (host, ctx) = cx.add_window_view(move |_, _| Host {
        state: host_state,
        labels,
        hooks,
        sortable: production,
    });
    let mut ctx = ctx.clone();
    ctx.simulate_resize(size(px(WINDOW.0), px(WINDOW.1)));
    warmup(&mut ctx, 8);
    (ctx, state, host)
}

/// 滚动的路径：`scroll_to_item` 是**确定性**的（不依赖命中测试），滚到第 n 行。
fn scroll_to(cx: &mut VisualTestContext, state: &Entity<DataTableState>, row: usize) {
    let state = state.clone();
    cx.update(|_, cx| {
        state.update(cx, |state, _| {
            state.v_scroll().scroll_to_item(row, ScrollStrategy::Top);
        });
    });
}

/// 扇滚轮：把指针放在窗口中间，滚 `dy`。
///
/// 走 `PlatformInput::ScrollWheel`（与 fork 自己的列表层测试同款）：它是**输入
/// 事件**，帧不是因为 `refresh()` 变脏而画的 —— 这一点很关键，`Window::refresh`
/// 会把 `refreshing` 置真，而 retained mode 与 scroll layers 都在 `refreshing`
/// 时退化成「整窗重建」（`fast/retained.rs::reusable_retained`、`layers::active`）。
fn wheel(cx: &mut VisualTestContext, dy: f32) {
    cx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position: point(px(400.), px(400.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                modifiers: Default::default(),
                touch_phase: TouchPhase::Moved,
            }),
            cx,
        );
    });
}

// ── 裸 uniform_list 对照（无组件包装；另测「套一层横向滚动容器」）────────

struct PlainListHost {
    rows: usize,
    scroll: gpui::UniformListScrollHandle,
    /// 是否把列表塞进一个横向滚动容器里（复现组件的嵌套结构）。
    nested: bool,
}

impl Render for PlainListHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows;
        let list = gpui::uniform_list("plain-list", rows, move |range, _, _| {
            range
                .map(|row| {
                    h_flex()
                        .h(px(24.))
                        .w_full()
                        .flex_none()
                        .child(div().w(px(100.)).child(cell_text(row, 0)))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .track_scroll(&self.scroll);
        if self.nested {
            // ⚠️ 每一层都要**定高**：外层的 Block 布局 + 内层没有 flex 上下文时，
            // `uniform_list` 会塌成 0 高（视口 0 × 内容 24000），滚轮什么都不画 ——
            // 那样量出来的「分配 8 次」是假的（见本文件模块文档的性能口径警告）。
            div().size_full().flex().flex_col().child(
                div()
                    .id("plain-x")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .child(
                        div()
                            .flex_none()
                            .h_full()
                            .w(px(2200.))
                            .flex()
                            .flex_col()
                            .child(list),
                    ),
            )
        } else {
            div().size_full().flex().flex_col().child(list)
        }
    }
}

#[gpui::test]
fn row_mode_grid_per_frame_cost(cx: &mut TestAppContext) {
    init(cx);

    for (rows, cols) in [(1_000usize, 20usize), (1_000, 40)] {
        for production in [true, false] {
            let (mut ctx, state, host) = open_grid(cx, rows, cols, production);
            let label = if production {
                "组件·生产钩子"
            } else {
                "组件·无钩子"
            };
            eprintln!("── rows={rows} cols={cols} {label} ──");

            let scrollable = ctx.update(|_, cx| state.read(cx).v_scroll().is_scrollable());
            let item_size = ctx.update(|_, cx| {
                let handle = state.read(cx).v_scroll().clone();
                let borrowed = handle.0.borrow();
                borrowed
                    .last_item_size
                    .map(|size| (f32::from(size.item.height), f32::from(size.contents.height)))
            });
            eprintln!("   可滚动={scrollable} 视口/内容高={item_size:?}");

            // ⓪ 空转帧：什么都不改。这一帧若也要几十万字节的分配，说明有东西
            //    每帧都在把窗口变脏（repaint 自激）。
            let idles: Vec<Costs> = (0..4).map(|_| step_frame(&mut ctx, |_| {})).collect();
            report(&format!("{label} · 空转"), &idles);

            // ①a 滚轮（实机路径：输入事件驱动，retained/层可复用）
            let wheels: Vec<Costs> = (0..12)
                .map(|_| step_frame(&mut ctx, |cx| wheel(cx, 72.)))
                .collect();
            let top = ctx.update(|_, cx| state.read(cx).v_scroll().logical_scroll_top_index());
            eprintln!("   滚轮 12 次后停在逻辑行 {top}");
            report(&format!("{label} · 滚轮"), &wheels);

            // ①b 组件自带的 scroll_to_item + 刷新（宿主「跳转行」的路径）
            let mut jumps = Vec::new();
            for step in 1..=8usize {
                let state = state.clone();
                let target = step * 5;
                jumps.push(step_frame(&mut ctx, move |cx| {
                    scroll_to(cx, &state, target);
                    cx.update(|window, _| window.refresh());
                }));
            }
            let top = ctx.update(|_, cx| state.read(cx).v_scroll().logical_scroll_top_index());
            eprintln!("   跳转 8 次后停在逻辑行 {top}");
            report(&format!("{label} · 跳转+刷新"), &jumps);

            // ② 全量重绘：宿主 notify（不滚动）。
            let repaints: Vec<Costs> = (0..6)
                .map(|_| {
                    let host = host.clone();
                    step_frame(&mut ctx, |cx| {
                        cx.update(|_, cx| host.update(cx, |_, cx| cx.notify()));
                    })
                })
                .collect();
            report(&format!("{label} · 全量重绘"), &repaints);

            let forced = idles.iter().filter(|c| c.forced).count()
                + wheels.iter().filter(|c| c.forced).count()
                + jumps.iter().filter(|c| c.forced).count()
                + repaints.iter().filter(|c| c.forced).count();
            eprintln!("   强制补帧 {forced} 次");
        }
    }

    // ── 裸 uniform_list 对照 ──────────────────────────────────────────
    for (rows, nested) in [(1_000usize, false), (1_000, true)] {
        let (mut ctx, scroll, host) = {
            let scroll = gpui::UniformListScrollHandle::new();
            let handle = scroll.clone();
            let (host, ctx) = cx.add_window_view(move |_, _| PlainListHost {
                rows,
                scroll: handle,
                nested,
            });
            let ctx = ctx.clone();
            ctx.simulate_resize(size(px(WINDOW.0), px(WINDOW.1)));
            (ctx, scroll, host)
        };
        warmup(&mut ctx, 8);
        let tag = if nested {
            "裸列表·套横向容器"
        } else {
            "裸列表"
        };
        eprintln!(
            "── rows={rows} {tag}（对照组）可滚动={} 视口/内容={:?} ──",
            scroll.is_scrollable(),
            scroll
                .0
                .borrow()
                .last_item_size
                .map(|s| (f32::from(s.item.height), f32::from(s.contents.height)))
        );
        let idles: Vec<Costs> = (0..4).map(|_| step_frame(&mut ctx, |_| {})).collect();
        report(&format!("{tag} · 空转"), &idles);
        let wheels: Vec<Costs> = (0..12)
            .map(|_| step_frame(&mut ctx, |cx| wheel(cx, 72.)))
            .collect();
        eprintln!(
            "   滚轮 12 次后停在逻辑行 {}",
            scroll.logical_scroll_top_index()
        );
        report(&format!("{tag} · 滚轮"), &wheels);
        let repaints: Vec<Costs> = (0..6)
            .map(|_| {
                let host = host.clone();
                step_frame(&mut ctx, |cx| {
                    cx.update(|_, cx| host.update(cx, |_, cx| cx.notify()));
                })
            })
            .collect();
        report(&format!("{tag} · 全量重绘"), &repaints);
    }
}

// ── ② 按特性二分的**每格分配成本** ──────────────────────────────────

/// 单元格长相的开关组合（逐个加上去，看每一步各花多少分配）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Shape {
    text: bool,
    hover: bool,
    group: bool,
    tooltip: bool,
    menu: bool,
    icon: bool,
}

const fn shape(
    text: bool,
    hover: bool,
    group: bool,
    tooltip: bool,
    menu: bool,
    icon: bool,
) -> Shape {
    Shape {
        text,
        hover,
        group,
        tooltip,
        menu,
        icon,
    }
}

const SHAPES: &[(&str, Shape)] = &[
    ("仅空 div", shape(false, false, false, false, false, false)),
    ("+ 文本", shape(true, false, false, false, false, false)),
    ("+ 悬停底色", shape(true, true, false, false, false, false)),
    (
        "+ 组名（format!）",
        shape(true, true, true, false, false, false),
    ),
    ("+ 悬停图标 ⓘ", shape(true, true, true, false, false, true)),
    ("+ 提示 tooltip", shape(true, true, true, true, false, true)),
    (
        "+ 右键菜单（落库形状）",
        shape(true, true, true, true, true, true),
    ),
    (
        "落库形状 - 右键菜单",
        shape(true, true, true, true, false, true),
    ),
];

fn shape_cell(shape: Shape, row: usize, col: usize, _cx: &gpui::App) -> AnyElement {
    // 颜色与字号对「分配次数」没有影响（闭包捕获的是 `Copy` 值），写死即可。
    let hover_tint: gpui::Rgba = rgb(0x33_44_55);
    let tertiary: gpui::Rgba = rgb(0x99_99_99);
    let icon_sm = px(14.);

    let mut cell = div()
        .id(gpui::ElementId::Integer(((row as u64) << 20) | col as u64))
        .w(px(100.))
        .h(px(24.))
        .flex_none()
        .flex()
        .items_center()
        .overflow_hidden();
    if shape.hover {
        cell = cell.hover(move |style| style.bg(hover_tint));
    }
    if shape.text {
        cell = cell.child(div().truncate().child(cell_text(row, col)));
    }
    if shape.group || shape.icon {
        let name: SharedString = format!("g-{row}-{col}").into();
        cell = cell.group(name.clone()).relative();
        let mut info = div()
            .id(gpui::ElementId::Integer(
                (1 << 40) | ((row as u64) << 20) | col as u64,
            ))
            .absolute()
            .right(px(2.))
            .flex()
            .items_center()
            .opacity(0.)
            .group_hover(name, |style| style.opacity(1.));
        if shape.tooltip {
            info = info.tooltip(move |window, cx| Tooltip::new("detail").build(window, cx));
        }
        if shape.icon {
            info = info.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(18.))
                    .h(px(18.))
                    .child(Icon::new(DuckIcon::Info).size(icon_sm).text_color(tertiary)),
            );
        }
        cell = cell.child(info);
    }
    if shape.menu {
        // `context_menu` 把元素包成 `ContextMenu<E>`：与组件同款，最后再转 `AnyElement`。
        return cell
            .context_menu(move |menu, _, _| menu.item(PopupMenuItem::new("copy")))
            .into_any_element();
    }
    cell.into_any_element()
}

struct BisectHost {
    shape: Shape,
    rows: usize,
    cols: usize,
}

impl Render for BisectHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut grid = v_flex().flex_col();
        for row in 0..self.rows {
            let mut line = h_flex().h(px(24.)).w_full().flex_none();
            for col in 0..self.cols {
                line = line.child(shape_cell(self.shape, row, col, cx));
            }
            grid = grid.child(line);
        }
        div().size_full().overflow_hidden().child(grid)
    }
}

/// 一屏 40 行 × 20 列的**朴素网格**（不经 `uniform_list`，也不经组件），
/// 逐个特性加上去量「每格多少次分配」。
#[gpui::test]
fn per_cell_allocation_cost(cx: &mut TestAppContext) {
    init(cx);

    let (rows, cols) = (40usize, 20usize);
    let mut baseline: Option<f64> = None;
    eprintln!("── 40 行 × 20 列（800 格）全量重绘一帧 ──");
    for (label, shape) in SHAPES {
        let shape = *shape;
        let (host, ctx) = cx.add_window_view(move |_, _| BisectHost { shape, rows, cols });
        let mut ctx = ctx.clone();
        ctx.simulate_resize(size(px(WINDOW.0), px(WINDOW.1)));
        warmup(&mut ctx, 4);

        let costs: Vec<Costs> = (0..4)
            .map(|_| {
                let host = host.clone();
                step_frame(&mut ctx, |cx| {
                    cx.update(|_, cx| host.update(cx, |_, cx| cx.notify()));
                })
            })
            .collect();
        let allocs = mean(&costs, |c| c.allocs);
        let baseline = *baseline.get_or_insert(allocs);
        report(label, &costs);
        eprintln!(
            "       ^ 每格 {:.1} 次分配（较基线 +{:.1}）",
            allocs / (rows * cols) as f64,
            (allocs - baseline) / (rows * cols) as f64
        );
    }
}

// ── ③ 横向裁剪的**回归守卫** ────────────────────────────────────────

/// 一帧的成本只与**可视窗口**有关，与表有多宽无关。
///
/// 这是列虚拟化（`data_table::table` 里的 `RowSlot`）的**不变量**：没有它，表有
/// 几列就建几格 —— 60 列的一帧会比 20 列贵约三倍（实测：加裁剪前 1000×40 是
/// 77,809 次分配，而 1000×20 是 39,816 次，正好按列数线性）。
///
/// 钉住的是**比例**而不是绝对值：这样以后往单元格里加东西（钩子、图标、新样式）
/// 不会让守卫误报，但「又把横向视口外的列全建出来了」一定会红。
///
/// 分配次数在 dev 与 release 两档下**完全相同**（实测 28,090 vs 28,090），
/// 所以这个断言不受编译档影响。
#[gpui::test]
fn frame_cost_does_not_grow_with_table_width(cx: &mut TestAppContext) {
    init(cx);

    // 一帧「全量重绘」（`notify`）的分配次数：与窗口宽度内可见的列数成正比。
    let frame_allocs = |cx: &mut TestAppContext, cols: usize| -> f64 {
        let (mut ctx, _state, host) = open_grid(cx, 1_000, cols, true);
        let costs: Vec<Costs> = (0..4)
            .map(|_| {
                let host = host.clone();
                step_frame(&mut ctx, |cx| {
                    cx.update(|_, cx| host.update(cx, |_, cx| cx.notify()));
                })
            })
            .collect();
        mean(&costs, |c| c.allocs)
    };

    // 两档都**超出**窗口宽度（1400）：可视窗口里的列数相同，成本就该基本相同。
    let narrow = frame_allocs(cx, 20);
    let wide = frame_allocs(cx, 60);
    eprintln!(
        "横向裁剪守卫：20 列 {narrow:.0} 次 / 60 列 {wide:.0} 次 = {:.2}×（可视窗口相同）",
        wide / narrow
    );
    assert!(
        wide <= narrow * 1.6,
        "60 列的一帧是 20 列的 {:.2} 倍 —— 横向裁剪没生效（视口外的列又被建出来了）",
        wide / narrow
    );
}
