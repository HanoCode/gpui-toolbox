//! 差异渲染器：A/B 双栏逐行对齐 + 统计（`[io.output] kind = "diff"` 的宿主侧）。
//!
//! 布局对齐参考项目 `DiffView.vue`（`docs/ifm-ui-layout-analysis.md` 的 diff 原型）：
//! 一行两栏、每栏「行号槽 + 正文」，删除在左、新增在右、改动两侧同时上色；行内改动
//! 部分（`DiffSegment.hit`）用更浓的一档底色标出「具体改了哪几个字」。
//!
//! 三个硬约束：
//!
//! 1. **行高固定** —— 走 `uniform_list` 虚拟滚动。一次比对两万行是常态，全量布局
//!    会把滚动拖死（参考项目在 5,000 行左右就开始卡）。
//! 2. **不依赖插件框架** —— 本 crate 只认这里的 view-model（[`DiffSection`] /
//!    [`DiffLine`]），插件的 `duck_plugin::diff` 由调用方映射过来。否则 UI 组件库
//!    就得反向依赖插件框架，数据库/日志这些非插件页面也用不了它。
//! 3. **颜色全部取 token** —— 增绿 / 删红 / 改黄走 `success` / `destructive` /
//!    `warning`，衬底透明度取 `extended.opacity`（不写字面量 0.x）。
//!
//! 语义色方向与 A 股行情相反是有意的：这里表达的是「新增 / 删除」，与 git / IDE
//! 一致；行情页的红涨绿跌是另一套语义，两者不通用。

use std::rc::Rc;

use duck_theme::active_theme;
use gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _,
    UniformListScrollHandle, div, px, uniform_list,
};

/// 一行的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// 两侧相同。
    Equal,
    /// 只在左侧（删除）。
    Delete,
    /// 只在右侧（新增）。
    Insert,
    /// 两侧不同（行内 `hit` 片段标出改动）。
    Change,
    /// 不连续标记（hunk 之间）。
    Gap,
}

/// 行内片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSegment {
    /// 是否属于改动部分。
    pub hit: bool,
    /// 片段文本。
    pub text: String,
}

impl DiffSegment {
    /// 未改动的片段。
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            hit: false,
            text: text.into(),
        }
    }

    /// 改动了的片段。
    #[must_use]
    pub fn changed(text: impl Into<String>) -> Self {
        Self {
            hit: true,
            text: text.into(),
        }
    }
}

/// 一行差异（两栏内容在同一条记录里 —— 对齐关系由产出一方决定，渲染器不再二次配对）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// 行种类。
    pub kind: DiffLineKind,
    /// 左侧行号。
    pub old_no: Option<u32>,
    /// 右侧行号。
    pub new_no: Option<u32>,
    /// 左侧片段。
    pub old: Vec<DiffSegment>,
    /// 右侧片段。
    pub new: Vec<DiffSegment>,
}

impl DiffLine {
    /// 不连续标记行。
    #[must_use]
    pub fn gap() -> Self {
        Self {
            kind: DiffLineKind::Gap,
            old_no: None,
            new_no: None,
            old: Vec::new(),
            new: Vec::new(),
        }
    }
}

/// 一份文件的差异段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSection {
    /// 左侧标题（文件名 / 「版本 A」）。
    pub title_a: String,
    /// 右侧标题。
    pub title_b: String,
    /// 逐行差异。
    pub lines: Vec<DiffLine>,
}

/// 统计（画在结果头部的 `+N −M ~K`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffSummary {
    /// 新增行。
    pub added: usize,
    /// 删除行。
    pub removed: usize,
    /// 改动行。
    pub modified: usize,
}

/// 行号槽宽度按「5 位行号」估（等宽字体下 1 字符 ≈ 0.6 em）。
const GUTTER_CHARS: f32 = 3.4;

/// 统计条：`+12  −3  ~2`。
///
/// 三个数都可能是 0；0 也照画，用户才能确认「这一类真的是 0」而不是没统计。
#[must_use]
pub fn diff_summary(summary: DiffSummary, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let extended = theme.extended();
    let tokens = theme.tokens();
    let mono = tokens.typography.mono_md;
    let row = [
        (format!("+{}", summary.added), extended.colors.success),
        (format!("−{}", summary.removed), tokens.colors.destructive),
        (format!("~{}", summary.modified), extended.colors.warning),
    ];
    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens.spacing.sm);
    for (text, color) in row {
        bar = bar.child(
            div()
                .text_size(mono.size)
                .line_height(mono.line_height)
                .text_color(color)
                .child(SharedString::from(text)),
        );
    }
    bar.into_any_element()
}

/// 差异视图（固定行高 + 虚拟滚动）。
///
/// `id_prefix` 用于 `uniform_list` 的元素 id（同窗多个差异区不能重名）。
/// `scroll` 由调用方持有：切走再切回时滚动位置要保留在原地。
pub fn diff_view(
    id_prefix: SharedString,
    sections: Rc<Vec<DiffSection>>,
    scroll: UniformListScrollHandle,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 段标题只在「多段」或「有标题」时画：单段无标题的常见情形（两段文本比对）
    // 多画一行「 → 」纯属噪声。
    let with_headers = sections.len() > 1
        || sections
            .iter()
            .any(|section| !section.title_a.is_empty() || !section.title_b.is_empty());
    let items = Rc::new(flatten(&sections, with_headers));
    let rows = sections;
    let count = items.len();
    let mono = tokens.typography.mono_md;
    let palette = Palette {
        row_h: mono.line_height + tokens.spacing.xxs,
        gutter: mono.size * GUTTER_CHARS,
        font: mono.size,
        line_height: mono.line_height,
        gap: tokens.spacing.xs,
        hairline: extended.colors.hairline,
        tertiary: extended.colors.text_tertiary,
        muted: tokens.colors.muted_foreground,
        foreground: tokens.colors.foreground,
        chrome: extended.colors.chrome,
        success: extended.colors.success,
        warning: extended.colors.warning,
        danger: tokens.colors.destructive,
        // 整行衬底与行内高亮各取一档：`tint + subtle` 才比背景明显（0.20 上下）。
        row_tint: extended.opacity.subtle,
        hit_tint: extended.opacity.tint + extended.opacity.subtle,
        faint: extended.opacity.muted,
    };
    let surface = tokens.colors.surface;
    let thin = extended.stroke.thin;
    let radius = tokens.radius.md;
    let border = tokens.colors.border;

    div()
        .w_full()
        .h_full()
        .min_h_0()
        .overflow_hidden()
        .rounded(radius)
        .border(thin)
        .border_color(border)
        .bg(surface)
        .child(
            uniform_list(
                SharedString::from(format!("{id_prefix}-diff")),
                count,
                move |range, _, _| {
                    range
                        .map(|index| match items[index] {
                            Item::Header(section) => {
                                let section = &rows[section];
                                header_row(&section.title_a, &section.title_b, &palette)
                            }
                            Item::Line(section, line) => {
                                line_row(&rows[section].lines[line], &palette)
                            }
                        })
                        .collect()
                },
            )
            .track_scroll(&scroll),
        )
        .into_any_element()
}

/// 渲染一套差异要用的尺寸与颜色（一次取好，逐行复用）。
///
/// 参数包成一个结构而不是十几个实参：`uniform_list` 的闭包要对**每一行**调用，
/// 逐参数传会让签名长到读不懂，也容易把两个相邻的 `Hsla` 传反（编译期发现不了）。
struct Palette {
    row_h: gpui::Pixels,
    gutter: gpui::Pixels,
    font: gpui::Pixels,
    line_height: gpui::Pixels,
    gap: gpui::Pixels,
    hairline: Hsla,
    tertiary: Hsla,
    muted: Hsla,
    foreground: Hsla,
    chrome: Hsla,
    success: Hsla,
    warning: Hsla,
    danger: Hsla,
    row_tint: f32,
    hit_tint: f32,
    faint: f32,
}

/// 拍平后的条目：只存**下标**，不存行内容。
///
/// 每帧都要重算这份索引（`uniform_list` 的闭包要求 `'static`，数据得搬进去）；
/// 存下标的话一次搬家就是几万个 `usize`，存行内容则是几万个 `String` 的深拷贝——
/// 滚动两万行的差异时这两者的差别就是「跟手」和「卡」。
enum Item {
    /// 段标题行（第 n 段）。
    Header(usize),
    /// 差异行（第 n 段第 m 行）。
    Line(usize, usize),
}

fn flatten(sections: &[DiffSection], with_headers: bool) -> Vec<Item> {
    let mut items = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if with_headers {
            items.push(Item::Header(index));
        }
        items.extend((0..section.lines.len()).map(|line| Item::Line(index, line)));
    }
    items
}

/// 段标题：左「A → B」（两侧同名时只写一次）。
fn header_row(title_a: &str, title_b: &str, palette: &Palette) -> AnyElement {
    let label = if title_a == title_b || title_b.is_empty() {
        title_a.to_owned()
    } else if title_a.is_empty() {
        title_b.to_owned()
    } else {
        format!("{title_a} → {title_b}")
    };
    div()
        .w_full()
        .h(palette.row_h)
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .bg(palette.chrome)
        .pl(palette.gutter)
        .pr(palette.gap)
        .text_size(palette.font)
        .line_height(palette.line_height)
        .text_color(palette.foreground)
        .truncate()
        .child(SharedString::from(label))
        .into_any_element()
}

/// 一行：`[行号][左侧] │ [行号][右侧]`。
fn line_row(line: &DiffLine, palette: &Palette) -> AnyElement {
    if line.kind == DiffLineKind::Gap {
        return div()
            .w_full()
            .h(palette.row_h)
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .text_size(palette.font)
            .line_height(palette.line_height)
            .text_color(palette.tertiary)
            .child(SharedString::from("⋯"))
            .into_any_element();
    }

    // 两侧底色：删除只染左、新增只染右、改动两侧都染。
    let (left_bg, right_bg) = match line.kind {
        DiffLineKind::Delete => (Some(tint(palette.danger, palette.row_tint)), None),
        DiffLineKind::Insert => (None, Some(tint(palette.success, palette.row_tint))),
        DiffLineKind::Change => (
            Some(tint(palette.warning, palette.row_tint)),
            Some(tint(palette.warning, palette.row_tint)),
        ),
        DiffLineKind::Equal | DiffLineKind::Gap => (None, None),
    };
    let left_hit = match line.kind {
        DiffLineKind::Delete | DiffLineKind::Change => tint(palette.danger, palette.hit_tint),
        _ => tint(palette.warning, palette.hit_tint),
    };
    let right_hit = match line.kind {
        DiffLineKind::Insert | DiffLineKind::Change => tint(palette.success, palette.hit_tint),
        _ => tint(palette.warning, palette.hit_tint),
    };

    div()
        .w_full()
        .h(palette.row_h)
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .text_size(palette.font)
        .line_height(palette.line_height)
        .child(cell(
            line.old_no,
            &line.old,
            left_bg,
            left_hit,
            palette,
            false,
        ))
        .child(div().w(px(1.)).h_full().flex_none().bg(palette.hairline))
        .child(cell(
            line.new_no,
            &line.new,
            right_bg,
            right_hit,
            palette,
            true,
        ))
        .into_any_element()
}

/// 一栏：行号槽 + 片段。
fn cell(
    number: Option<u32>,
    segments: &[DiffSegment],
    background: Option<Hsla>,
    hit_background: Hsla,
    palette: &Palette,
    is_right: bool,
) -> AnyElement {
    let mut column = div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden();
    if let Some(background) = background {
        column = column.bg(background);
    }
    let label = number.map_or_else(String::new, |value| value.to_string());
    column = column.child(
        div()
            .w(palette.gutter)
            .flex_none()
            .text_color(palette.tertiary)
            .text_right()
            .pr(palette.gap)
            .child(SharedString::from(label)),
    );

    if segments.iter().all(|segment| segment.text.is_empty()) {
        // 这一侧没有内容（对侧新增 / 删除）：留一个极淡的占位，避免看起来像被裁掉了。
        let mark = if is_right { "＋" } else { "−" };
        return column
            .child(
                div()
                    .text_color(tint(palette.muted, palette.faint))
                    .child(SharedString::from(mark)),
            )
            .into_any_element();
    }

    let mut body = div().flex_1().min_w_0().overflow_hidden();
    for segment in segments {
        let mut piece = div()
            .flex_none()
            .text_color(palette.foreground)
            .child(SharedString::from(segment.text.clone()));
        if segment.hit {
            piece = piece.bg(hit_background);
        }
        body = body.child(piece);
    }
    column.child(body).into_any_element()
}

/// 给颜色加一档透明度（衬底用）。
fn tint(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}
