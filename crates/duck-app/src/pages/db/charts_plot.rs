//! 自绘图表层：散点 / 堆叠柱。
//!
//! 这两档 `gpui-component` 的现成组件画不出来 —— `LineChart` 的圆点填充用的就是 `stroke`
//! 那个颜色（`line_chart.rs` 的 `dot_fill(stroke)`），把线设透明会让点一起消失；`BarChart`
//! 没暴露堆叠模式。而 **DBFlux 用的就是同一套 GPUI 原语、它把这两档自绘出来了**，所以本文件
//! 照它的引擎实现：`gpui::canvas` 里直接画圆与矩形。
//!
//! # 来源
//!
//! 绘制手法移植自 **DBFlux**（Apache-2.0，<https://github.com/0xErwin1/dbflux>）：
//!
//! | 本文件 | 上游（`crates/dbflux_components/src/chart/`） |
//! |---|---|
//! | [`paint_dot`] | `engine.rs::paint_filled_circle` |
//! | [`scatter_element`] | `engine.rs::paint_scatter` |
//! | [`stack_columns`] | `engine.rs::stack_columns`（含 `stack_x_key`） |
//! | [`paint_stack_columns`] | `engine.rs::paint_stacked_bars` |
//! | [`paint_gridlines`] | `engine.rs::paint_gridlines` |
//! | [`nice_step`] / [`ticks`] | `axis.rs::nice_step` / `ticks_numeric` |
//!
//! # 接法上的三处刻意不同（都不是笔误）
//!
//! 1. **横轴键用标签字符串，不用 `f64` 数据坐标**：本仓的横轴本来就有「分类」这一档
//!    （[`ChartAxisKind::Categorical`]），标签才是它的身份；上游只吃数值轴，所以能拿
//!    `f64` 当键。
//! 2. **刻度数字沿用本仓已有的 [`compact_number`]**（`helpers.rs`），不引入第二套数字
//!    格式化。上游的 `format_numeric` 是同样意图的另一份实现。
//! 3. **刻度文字不进 canvas，用绝对定位的元素画**：本仓没有 canvas 内 shaping 的先例，而
//!    文字走元素管线还能吃到主题字体与字号令牌。两侧**共用下面这组纯函数**把位置换算成
//!    0~1 的比例，所以对齐是结构性的，不是「两处各算一遍、凑巧一样」。

use super::*;
use gpui::{Bounds, PathBuilder, Pixels, canvas, point};

// ── 画框 ──────────────────────────────────────────────────────────────────────

/// 左边留给 Y 刻度文字的空档。
///
/// 上游会**量**一遍刻度文字再定这个留白（`effective_x_label_padding`），本仓固定：量宽要
/// 进 canvas 的 shaping 通路，而这里的刻度文字由 [`compact_number`] 生成、宽度本就有界
/// （`1.2k` / `-3.4M` 这种），固定留白够用，且网格与文字天然共用一套换算。
const PLOT_GUTTER_LEFT: f32 = 52.;
/// 右侧留白：最后一条刻度文字不掉出面板。
const PLOT_GUTTER_RIGHT: f32 = 14.;
/// 顶部留白：最高那根柱 / 最上面那个点不贴着面板边。
const PLOT_GUTTER_TOP: f32 = 10.;
/// 底部留白：放横轴文字。
const PLOT_GUTTER_BOTTOM: f32 = 22.;

/// 散点半径（上游 `paint_scatter` 的 3.5）。
const DOT_RADIUS: f32 = 3.5;
/// 网格线粗细。
const GRID_LINE: f32 = 1.;
/// 堆叠柱宽占一格的比例（上游 `paint_stacked_bars` 的 0.8）。
const STACK_BAR_RATIO: f32 = 0.8;
/// 柱 / 点顶部留的余量比例（上游 `bar_layout_params` 的 8%）。
const PLOT_HEADROOM: f64 = 0.08;
/// 散点的顶部余量：点有半径，不想到框线上去。
const DOT_HEADROOM: f64 = 0.05;
/// 横轴刻度**目标条数**。
///
/// 上游按宽度算（`clamp(plot_w / 120, 3, 16)`），本仓拿不到宽度（建元素时还没排版），
/// 固定 5：刻度文字盒 [`X_LABEL_WIDTH`] 宽，5 条正好铺满一屏两张那种宽度。
const X_LABEL_TARGET: usize = 5;
/// 横轴刻度文字盒宽（居中定位要减半个盒宽）。
const X_LABEL_WIDTH: f32 = 64.;
/// 文字盒高相对字号的倍数（纵向居中要减半个盒高）。
const LABEL_BOX_LINES: f32 = 1.2;

/// 自绘图要用的主题色与字号。
///
/// 渲染期从 tokens 取一次带进来：canvas 的 paint 闭包在 `'static` 里跑，那里拿不到 `cx`。
pub(super) struct PlotStyle {
    /// 数值色（散点、只有一条序列时的柱）。
    pub(super) accent: Hsla,
    /// 网格线色。
    pub(super) grid: Hsla,
    /// 刻度文字色。
    pub(super) tick: Hsla,
    /// 刻度文字字号。来自 `tokens.typography.xs.size` —— **不从裸 `px` 取**。
    pub(super) text_size: Pixels,
}

// ── 纯函数层（可单测，不碰 gpui）─────────────────────────────────────────────

/// 一张图用的数值范围。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PlotRange {
    /// 下界（画在框底）。
    pub(super) min: f64,
    /// 上界（画在框顶）。
    pub(super) max: f64,
}

impl PlotRange {
    /// 从一组值收范围；非有限值直接丢掉。
    ///
    /// **空集（或全是非有限值）给 `0..=1`**：调用方已经先判过「没点就不画」，这里不 panic。
    pub(super) fn of(values: impl IntoIterator<Item = f64>) -> Self {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for value in values {
            if value.is_finite() {
                min = min.min(value);
                max = max.max(value);
            }
        }
        if min > max {
            return Self { min: 0., max: 1. };
        }
        Self { min, max }
    }

    /// 顶端加 `pad` 比例的余量（`0.08` = 8%）。
    ///
    /// 只加上界、不动下界 —— 与上游 `bar_layout_params` 同口径：柱 / 点的**读数**就是它的
    /// 高度，下界一动，「0 在哪儿」就跟着漂。
    pub(super) fn headroom(self, pad: f64) -> Self {
        Self {
            min: self.min,
            max: self.max + (self.max - self.min) * pad,
        }
    }

    /// 值的跨度；退化（只有一个值 / 全是同一个值）时给 1，免得后面除零。
    fn span(self) -> f64 {
        let span = self.max - self.min;
        if span.abs() < f64::MIN_POSITIVE {
            1.
        } else {
            span
        }
    }

    /// 值 → 从底往上的比例（0 = 框底，1 = 框顶）。
    pub(super) fn fraction(self, value: f64) -> f32 {
        ((value - self.min) / self.span()) as f32
    }
}

/// 把步长圆整到 `{1, 2, 5, 10} × 10ⁿ` —— 移植 DBFlux `axis.rs::nice_step`。
pub(super) fn nice_step(raw: f64) -> f64 {
    if raw <= 0. {
        return 1.;
    }
    let magnitude = 10f64.powf(raw.log10().floor());
    let fraction = raw / magnitude;
    let nice = if fraction <= 1. {
        1.
    } else if fraction <= 2. {
        2.
    } else if fraction <= 5. {
        5.
    } else {
        10.
    };
    nice * magnitude
}

/// 取落在范围里的「整数感」刻度 —— 移植 DBFlux `axis.rs::ticks_numeric`。
///
/// 只取**位置**，文案由调用方用 [`compact_number`] 渲染（见文件头的第 2 条刻意不同）。
/// 两端各留 `step * 1e-9` 的容差：浮点累加出来的 `0.30000000000000004` 不该被漏掉。
pub(super) fn ticks(range: PlotRange, target: usize) -> Vec<f64> {
    let target = target.max(1);
    if range.max <= range.min {
        return vec![range.min];
    }
    let step = nice_step((range.max - range.min) / target as f64);
    let mut value = (range.min / step).ceil() * step;
    let mut out = Vec::new();
    while value <= range.max + step * 1e-9 {
        if value >= range.min - step * 1e-9 {
            out.push(value);
        }
        value += step;
    }
    out
}

/// 等距抽 `target` 个横轴位置当刻度（**首尾必取**）。
///
/// DBFlux 的横轴刻度是**另算**的（按数值间隔生成 `ticks_numeric` / `ticks_time`），本仓的
/// 横轴标签本来就来自数据（时间已按紧凑格式改写），所以直接抽真标签：不会出现「刻度算得出、
/// 却对不上任何一行」的情形。
pub(super) fn sample_axis(items: &[(f64, String)], target: usize) -> Vec<(f64, String)> {
    if items.is_empty() {
        return Vec::new();
    }
    let count = target.clamp(1, items.len());
    if count == 1 {
        return vec![items[0].clone()];
    }
    let step = (items.len() - 1) as f64 / (count - 1) as f64;
    let mut out: Vec<(f64, String)> = Vec::with_capacity(count);
    for slot in 0..count {
        let index = (slot as f64 * step).round() as usize;
        let Some(item) = items.get(index) else {
            continue;
        };
        // 抽稀后的下标可能撞在一起（item 数少于目标条数时不会，但取整会），去重。
        if out.last().is_some_and(|last| last.1 == item.1) {
            continue;
        }
        out.push(item.clone());
    }
    out
}

/// 堆叠里的一根柱。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StackColumn {
    /// 横轴位置：该标签下的 `axis`（同一标签的 `axis` 本就一样）。
    pub(super) axis: f64,
    /// 标签（横轴显示名）。
    pub(super) label: String,
    /// `(序列下标, 值)`，从基线往上排。
    pub(super) segments: Vec<(usize, f64)>,
}

impl StackColumn {
    /// 这一柱各分段之和。
    ///
    /// 绘制不经过它（[`paint_stack_columns`] 直接按 `segments` 逐段垒），只在断言里用来说
    /// 「几段叠起来是多少」，所以标 `cfg(test)` —— 不然非测试构建里它就是个死方法。
    #[cfg(test)]
    pub(super) fn total(&self) -> f64 {
        self.segments.iter().map(|(_, value)| *value).sum()
    }
}

/// 把多条序列按**横轴标签**对齐成一列列堆叠 —— 移植 DBFlux `engine.rs::stack_columns`。
///
/// 为什么按标签而不是按点序：序列之间的 X 不一定齐（有的行某一列没值），按点序垒会**错位**。
/// 一条序列在同一个 X 上有多个点时**累加**（上游同口径）；某条序列在这个 X 上没有值（或值
/// 加起来正好是 0）时**不出段** —— 也就是在那一柱里算 0，而不是补一条 0 高的缝。
///
/// 标签为空串的点直接跳过：那与「分类是 NULL」是同一件事（[`chart_series`] 的口径），
/// 横轴上不该多出一根没名字的柱。
pub(super) fn stack_columns(series: &[Vec<ChartPoint>]) -> Vec<StackColumn> {
    // 先一趟把「(序列, 标签) → 累加值」摊平。
    //
    // ⚠️ 别写成「逐柱再扫一遍各序列」：那是 O(柱 × 序列 × 点)，而这里是**渲染路径**
    // （每帧都跑）。上限放宽看是 400 点 × 8 列，实际能到百万级字符串比较 —— 一帧几毫秒
    // 就没了。上游同样是一趟摊平（每序列一张 `HashMap`），只是它是数值键、本仓是标签键。
    let mut sums: HashMap<(usize, &str), f64> = HashMap::new();
    let mut labels: BTreeSet<&str> = BTreeSet::new();
    let mut positions: Vec<(f64, String)> = Vec::new();
    for (index, points) in series.iter().enumerate() {
        for point in points {
            if point.label.is_empty() || !point.value.is_finite() {
                continue;
            }
            *sums.entry((index, point.label.as_str())).or_insert(0.) += point.value;
            // 同一个标签只登记一次横轴位置（各序列的 `axis` 本就一样，取先见到的那个）。
            if labels.insert(point.label.as_str()) {
                positions.push((point.axis, point.label.clone()));
            }
        }
    }
    // 横轴顺序 = `axis` 升序（时间 / 数值 / 分类三档的 `axis` 都是可比的，同 [`ChartPoint`]）。
    positions.sort_by(|left, right| left.0.total_cmp(&right.0));

    positions
        .into_iter()
        .map(|(axis, label)| {
            let segments = series
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    // 没有值 / 值加起来正好是 0 ⇒ 不出段（在那一柱里算 0，不是补一条 0 高的缝）。
                    let sum = sums.get(&(index, label.as_str())).copied().unwrap_or(0.);
                    (sum != 0.).then_some((index, sum))
                })
                .collect();
            StackColumn {
                axis,
                label,
                segments,
            }
        })
        .collect()
}

/// 堆叠柱的值域：**从 0 起**，到所有分段累积值的最高点。
///
/// 取的是每个分段的上界 / 下界，不只是各柱的总和：有负值段时它会掉到基线下面，只按总和取
/// 范围会把那一段画到框外。
pub(super) fn stack_range(columns: &[StackColumn]) -> PlotRange {
    let mut min = 0.0_f64;
    let mut max = 0.0_f64;
    for column in columns {
        let mut cumulative = 0.0;
        for (_, value) in &column.segments {
            cumulative += value;
            min = min.min(cumulative);
            max = max.max(cumulative);
        }
    }
    PlotRange { min, max }.headroom(PLOT_HEADROOM)
}

// ── 绘制层 ────────────────────────────────────────────────────────────────────

/// 一块画布里的**绘图区**（面板坐标去掉四条留白）。
#[derive(Debug, Clone, Copy)]
struct PlotRect {
    x0: f32,
    y0: f32,
    w: f32,
    h: f32,
}

/// 画布尺寸 → 绘图区。与 [`plot_shell`] 里两个刻度层的定位用的是同一组常量。
fn plot_rect(bounds: Bounds<Pixels>) -> PlotRect {
    PlotRect {
        x0: f32::from(bounds.origin.x) + PLOT_GUTTER_LEFT,
        y0: f32::from(bounds.origin.y) + PLOT_GUTTER_TOP,
        w: (f32::from(bounds.size.width) - PLOT_GUTTER_LEFT - PLOT_GUTTER_RIGHT).max(1.),
        h: (f32::from(bounds.size.height) - PLOT_GUTTER_TOP - PLOT_GUTTER_BOTTOM).max(1.),
    }
}

/// 在比例 `frac`（从框底往上 0~1）处画一条横网格线 —— 移植 DBFlux `paint_gridlines`。
///
/// 只画横线（上游也没有竖线：它靠悬停十字线标横轴位置，本仓的图没有十字线）。
/// `- GRID_LINE / 2` 是让 1px 的线落在像素格上，不然会被抗锯齿糊成 2px 的灰边。
fn paint_gridlines(window: &mut Window, fracs: &[f32], rect: PlotRect, color: Hsla) {
    for frac in fracs {
        let sy = rect.y0 + (1. - frac) * rect.h;
        window.paint_quad(gpui::fill(
            gpui::Bounds {
                origin: point(px(rect.x0), px(sy - GRID_LINE / 2.)),
                size: gpui::size(px(rect.w), px(GRID_LINE)),
            },
            color,
        ));
    }
}

/// 画一个实心圆 —— 移植 DBFlux `engine.rs::paint_filled_circle`。
///
/// GPUI 的 `PathBuilder` 没有「圆」这个原语，上游用**两段半圆**拼（右 → 左、左 → 右）。
/// `LineChart` 自己画的点绑死在 `stroke` 色上，所以「只有点、没有线」在那边做不出来 ——
/// 这十五行就是散点图缺的那块。
fn paint_dot(window: &mut Window, cx: f32, cy: f32, r: f32, color: Hsla) {
    let radii = point(px(r), px(r));
    let right = point(px(cx + r), px(cy));
    let left = point(px(cx - r), px(cy));

    let mut builder = PathBuilder::fill();
    builder.move_to(right);
    builder.arc_to(radii, px(0.), false, true, left);
    builder.arc_to(radii, px(0.), false, true, right);
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// 画堆叠柱 —— 移植 DBFlux `engine.rs::paint_stacked_bars`。
///
/// 每根柱**占满一格**（序列是叠起来的，不是并排的），逐段从基线往上垒。值域必须已经算到
/// 「最高的那一柱」（见 [`stack_range`]），否则柱顶出框。
///
/// 首尾各让半个格（上游 `bar_x_inset_fraction`）：不让的话第一根 / 最后一根柱有一半在框外。
fn paint_stack_columns(
    window: &mut Window,
    columns: &[StackColumn],
    axis: PlotRange,
    range: PlotRange,
    rect: PlotRect,
    palette: &[Hsla],
    fallback: Hsla,
) {
    let slots = columns.len().max(1) as f32;
    let inset = rect.w * 0.5 / slots;
    let usable = (rect.w - 2. * inset).max(1.);
    let bar_w = (usable / slots * STACK_BAR_RATIO).max(1.);

    for column in columns {
        let center = rect.x0 + inset + axis.fraction(column.axis) * usable;
        let left = center - bar_w / 2.;
        // 分段按序列顺序从基线往上垒，与 [`stack_range`] 的累加顺序**必须一致**，否则柱顶
        // 与值域对不上（一个往上数、一个往下数）。
        let mut cumulative = 0.0_f64;
        for (index, value) in &column.segments {
            let bottom = rect.y0 + (1. - range.fraction(cumulative)) * rect.h;
            let top = rect.y0 + (1. - range.fraction(cumulative + value)) * rect.h;
            cumulative += value;
            let (y, h) = if top <= bottom {
                (top, bottom - top)
            } else {
                (bottom, top - bottom)
            };
            let color = palette.get(*index).copied().unwrap_or(fallback);
            window.paint_quad(gpui::fill(
                gpui::Bounds {
                    origin: point(px(left), px(y)),
                    size: gpui::size(px(bar_w), px(h.max(1.))),
                },
                color,
            ));
        }
    }
}

/// 图的骨架：一块 canvas（网格 + 标记）+ 两片绝对定位的刻度文字。
///
/// 三个子层的几何**由同一组常量推出**（[`plot_rect`] 给 canvas，`top/left/right/bottom` 给
/// 文字层），所以两边不需要互相对表；刻度位置一律先换算成 0~1 的比例再交给各自的定位。
///
/// `marks` 拿的是整块画布的范围，自己去 [`plot_rect`] 取绘图区。
fn plot_shell(
    style: &PlotStyle,
    x_ticks: &[(f32, String)],
    y_ticks: &[(f32, String)],
    marks: impl Fn(Bounds<Pixels>, &mut Window) + 'static,
) -> AnyElement {
    let size = style.text_size;
    let box_h = size * LABEL_BOX_LINES;
    let mut root = div().relative().w_full().h_full().overflow_hidden();

    // 网格 + 标记：铺满整块面板，绘图区在 paint 里算。
    root = root.child(
        canvas(
            |bounds, _, _| bounds,
            move |bounds, _state, window, _cx| marks(bounds, window),
        )
        .absolute()
        .inset_0(),
    );

    // Y 刻度：贴在左边留白里、右对齐；纵向**居中在刻度线上**。
    let mut y_layer = div()
        .absolute()
        .top(px(PLOT_GUTTER_TOP))
        .left(px(0.))
        .w(px(PLOT_GUTTER_LEFT))
        .bottom(px(PLOT_GUTTER_BOTTOM));
    for (frac, label) in y_ticks {
        y_layer = y_layer.child(
            div()
                .absolute()
                .top(relative(1. - *frac))
                .left(px(0.))
                .w_full()
                .h(box_h)
                .mt(size * -0.5)
                .pr(px(6.))
                .text_right()
                .text_size(size)
                .text_color(style.tick)
                .child(SharedString::from(label.clone())),
        );
    }
    root = root.child(y_layer);

    // X 刻度：落在绘图区正下方，**水平居中在刻度位置上**（减半个盒宽）。
    let mut x_layer = div()
        .absolute()
        .top(px(PLOT_GUTTER_TOP))
        .left(px(PLOT_GUTTER_LEFT))
        .right(px(PLOT_GUTTER_RIGHT))
        .bottom(px(PLOT_GUTTER_BOTTOM));
    for (frac, label) in x_ticks {
        x_layer = x_layer.child(
            div()
                .absolute()
                .top(relative(1.))
                .left(relative(*frac))
                .w(px(X_LABEL_WIDTH))
                .ml(px(-X_LABEL_WIDTH / 2.))
                .mt(px(2.))
                .truncate()
                .text_center()
                .text_size(size)
                .text_color(style.tick)
                .child(SharedString::from(label.clone())),
        );
    }
    root.child(x_layer).into_any_element()
}

/// 一张**散点**图（一条数值列一张，与折线 / 柱状同为「小倍数」口径）。
pub(super) fn scatter_element(points: Vec<ChartPoint>, style: &PlotStyle) -> AnyElement {
    let range = PlotRange::of(points.iter().map(|point| point.value)).headroom(DOT_HEADROOM);
    let axis = PlotRange::of(points.iter().map(|point| point.axis));
    let y_ticks = tick_marks(range);
    let x_ticks = axis_marks(
        &points
            .iter()
            .map(|point| (point.axis, point.label.clone()))
            .collect::<Vec<_>>(),
        axis,
    );
    let dots: Vec<(f32, f32)> = points
        .iter()
        .map(|point| (axis.fraction(point.axis), range.fraction(point.value)))
        .collect();

    let grid = style.grid;
    let accent = style.accent;
    let grid_fracs: Vec<f32> = y_ticks.iter().map(|(frac, _)| *frac).collect();
    plot_shell(style, &x_ticks, &y_ticks, move |bounds, window| {
        let rect = plot_rect(bounds);
        paint_gridlines(window, &grid_fracs, rect, grid);
        for (fx, fy) in &dots {
            paint_dot(
                window,
                rect.x0 + fx * rect.w,
                rect.y0 + (1. - fy) * rect.h,
                DOT_RADIUS,
                accent,
            );
        }
    })
}

/// 一张**堆叠柱**图：多条数值列共用一根柱（堆叠的意义就在这儿）。
///
/// 只有一条序列时它退化成「占满一格的柱状图」—— 这也正是单列调用（`chart_element` 那条
/// 一列一张的路）时的合理结果，不必另开一档。
pub(super) fn stacked_element(
    series: Vec<(String, Vec<ChartPoint>)>,
    style: &PlotStyle,
) -> AnyElement {
    let points: Vec<Vec<ChartPoint>> = series.into_iter().map(|(_, points)| points).collect();
    let columns = stack_columns(&points);
    let range = stack_range(&columns);
    let axis = PlotRange::of(columns.iter().map(|column| column.axis));
    let y_ticks = tick_marks(range);
    let x_ticks = axis_marks(
        &columns
            .iter()
            .map(|column| (column.axis, column.label.clone()))
            .collect::<Vec<_>>(),
        axis,
    );
    let palette = chart_palette(
        style.accent,
        columns.iter().map(|c| c.segments.len()).max().unwrap_or(1),
    );

    let grid = style.grid;
    let fallback = style.accent;
    let grid_fracs: Vec<f32> = y_ticks.iter().map(|(frac, _)| *frac).collect();
    plot_shell(style, &x_ticks, &y_ticks, move |bounds, window| {
        let rect = plot_rect(bounds);
        paint_gridlines(window, &grid_fracs, rect, grid);
        paint_stack_columns(window, &columns, axis, range, rect, &palette, fallback);
    })
}

/// 值域 → `(从底往上的比例, 文案)`，条数按面板高度不可知这个事实固定 [`Y_TICK_TARGET`]。
fn tick_marks(range: PlotRange) -> Vec<(f32, String)> {
    ticks(range, Y_TICK_TARGET)
        .into_iter()
        .map(|value| (range.fraction(value), compact_number(value)))
        .collect()
}

/// 横轴刻度：先抽真标签（[`sample_axis`]），再换算成绘图区里的**水平比例**。
fn axis_marks(items: &[(f64, String)], axis: PlotRange) -> Vec<(f32, String)> {
    sample_axis(items, X_LABEL_TARGET)
        .into_iter()
        .map(|(value, label)| (axis.fraction(value), label))
        .collect()
}

/// Y 轴刻度目标条数（上游按高度算 `clamp(plot_h / 60, 3, 12)`，这里同样拿不到高度）。
const Y_TICK_TARGET: usize = 5;
