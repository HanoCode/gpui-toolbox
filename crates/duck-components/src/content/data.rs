//! 数据类内容件：徽标 / 标签 / 状态单元格 / 描述列表 / 分页 / 状态栏。

use std::rc::Rc;

use duck_theme::{active_theme, tr};
use gpui::{
    AnyElement, App, DefiniteLength, Div, ElementId, Hsla, IntoElement, ParentElement as _,
    SharedString, Styled, Window, div, percentage, px,
};
use gpui_component::{
    Icon, Sizable as _, Size,
    badge::Badge,
    description_list::{DescriptionItem, DescriptionList},
    status_bar::StatusBar,
    tag::{Tag, TagVariant},
};

use crate::{Button, ButtonVariant, DuckIcon, icon_button, notify::Tone};

/// 分页的翻页回调（`current` / `page_step_button` 共享同一份 `Rc`）。
type PageSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// 计数徽标：叠在宿主元素右上角（调用方把宿主作为子元素传入）。
///
/// 上游 [`Badge`] 是「宿主 + 绝对定位角标」的复合件，`count` 为 0 时整体隐藏。
/// 独立的小胶囊用 [`tag`]。
pub fn badge_count(count: usize, max: usize, tone: Tone, cx: &App) -> Badge {
    Badge::new()
        .count(count)
        .max(max)
        .color(tone.color(cx))
        .with_size(Size::Small)
}

/// 圆点徽标：只需「有新内容」的信号，不报数字。
pub fn badge_dot(tone: Tone, cx: &App) -> Badge {
    Badge::new().dot().color(tone.color(cx))
}

/// 语义标签：独立小胶囊（状态、BETA、分类等）。`tone` 与 toast 共用 [`Tone`]。
pub fn tag(text: impl Into<SharedString>, tone: Tone, cx: &App) -> Tag {
    let variant = match tone {
        Tone::Info => TagVariant::Info,
        Tone::Success => TagVariant::Success,
        Tone::Warning => TagVariant::Warning,
        Tone::Error => TagVariant::Danger,
    };
    Tag::new()
        .with_variant(variant)
        .rounded_full()
        .text_size(active_theme(cx).tokens().typography.xs.size)
        .child(text.into())
}

/// 中性标签：跟随主题的灰阶胶囊（关键词等非语义内容）。
///
/// [`tag`] 的四个 tone 都是语义色（info 固定走上游主题的 `info` 青），不适合
/// 「无语义、只想融入界面」的内容；这里走 `TagVariant::Secondary`——
/// 底色 / 文字 / 描边全部取自 duck-theme 映射（surface2 / text_primary / border），明暗自动跟随。
pub fn neutral_tag(text: impl Into<SharedString>, cx: &App) -> Tag {
    Tag::new()
        .with_variant(TagVariant::Secondary)
        .rounded_full()
        .text_size(active_theme(cx).tokens().typography.xs.size)
        .child(text.into())
}

/// 强调色标签：薄强调底 + 强调色文字/描边的胶囊。
///
/// 上游 [`tag`] 的语义色（info / success / warning）是固定色相，不随用户选的强调色走；
/// 同屏出现蓝 / 绿 / 橙 / 灰多套色时界面会花。这里用上游 `TagVariant::Custom` 把配色
/// 锁死到强调色上——与全应用「强调色薄底」的悬停约定同源（底 `opacity.tint`、
/// 文字强调色、描边实强调色，同筛选 chip 的选中态），明暗与自定义强调色自动跟随。
pub fn accent_tag(text: impl Into<SharedString>, cx: &App) -> Tag {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let opacity = theme.extended().opacity;
    let accent = tokens.colors.primary;

    Tag::custom(accent.opacity(opacity.tint), accent, accent)
        .rounded_full()
        .text_size(tokens.typography.xs.size)
        .child(text.into())
}

/// 状态单元格：6px 状态点 + `xs` 文字（列表「状态」列）。
pub fn status_cell(color: Hsla, text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let xs = tokens.typography.xs;
    div()
        .flex()
        .items_center()
        .gap(tokens.spacing.xxs)
        // 状态色点直径：视觉标记（不是间距步进），阶梯里没有对应档，保留裸值。
        .child(div().size(px(6.)).flex_none().rounded_full().bg(color))
        .child(
            div()
                .text_size(xs.size)
                .line_height(xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(text.into()),
        )
}

/// 描述列表的一行：标签 + 值（值可以是文本，也可以是任意元素）。
pub fn description_row(label: impl Into<SharedString>, value: impl IntoElement) -> DescriptionItem {
    DescriptionItem::new(label.into()).value(value.into_any_element())
}

/// 键值描述列表：竖排、无描边、单列。
///
/// 上游默认是「横排 + 三列 + 描边表格」，更接近属性表；工具箱里多是「字段名 → 值」
/// 的窄列表，所以这里给一档常用配置。需要表格形态时直接用上游 `DescriptionList::new()`。
pub fn description_list(items: impl IntoIterator<Item = DescriptionItem>) -> DescriptionList {
    DescriptionList::vertical()
        .bordered(false)
        .columns(1)
        .label_width(DefiniteLength::from(px(120.)))
        .with_size(Size::Small)
        .children(items)
}

/// 分页控件：上一页 / 页码（首尾固定，当前页左右各一，断档处折叠成省略号）/ 下一页。
///
/// 不复用上游 `Pagination`：那个件内部用 `rust_i18n` 取「上一页 / 下一页」等标签，
/// 与本项目「源串即键 + `EN` 表」是两套 i18n，切语言时它不会跟着变；这里按本项目
/// 按钮原语与文案重画。`current_page` 从 1 起。
pub fn pagination(
    id: impl Into<ElementId>,
    current_page: usize,
    total_pages: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let id = id.into();
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let xs = tokens.typography.xs;
    let on_select: PageSelect = Rc::new(on_select);
    let total = total_pages.max(1);
    let current = current_page.clamp(1, total);

    let mut row = div()
        .flex()
        .items_center()
        .gap(tokens.spacing.xs)
        .child(page_step_button(
            &id,
            true,
            (current > 1).then(|| current - 1),
            on_select.clone(),
            cx,
        ));

    for (index, slot) in page_slots(current, total).into_iter().enumerate() {
        let key = ElementId::NamedInteger(SharedString::from(format!("{id}-page")), index as u64);
        match slot {
            None => {
                row = row.child(
                    div()
                        .w(theme.density().control)
                        .flex()
                        .justify_center()
                        .text_size(xs.size)
                        .line_height(xs.line_height)
                        .text_color(theme.extended().colors.text_tertiary)
                        .child("…"),
                );
            }
            Some(page) => {
                let variant = if page == current {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                };
                let on_select = Rc::clone(&on_select);
                row = row.child(
                    crate::button(key, page.to_string(), variant, cx)
                        .on_click(move |_, window, cx| on_select(page, window, cx)),
                );
            }
        }
    }

    row.child(page_step_button(
        &id,
        false,
        (current < total).then(|| current + 1),
        on_select,
        cx,
    ))
}

/// 页码槽位：首尾固定，当前页左右各一；中间断档处填 `None`（渲染成省略号）。
fn page_slots(current: usize, total: usize) -> Vec<Option<usize>> {
    let mut pages = vec![1, total];
    let start = current.saturating_sub(1).max(1);
    let end = (current + 1).min(total);
    pages.extend(start..=end);
    pages.sort_unstable();
    pages.dedup();

    let mut slots = Vec::with_capacity(pages.len() + 2);
    let mut previous = 0;
    for page in pages {
        if previous != 0 && page != previous + 1 {
            slots.push(None);
        }
        slots.push(Some(page));
        previous = page;
    }
    slots
}

/// 翻页按钮：`flipped` 为真时读作「上一页」并把图标转 180°（复用同一枚 `chevron-right`）。
///
/// `target` 为 `None` 表示这一侧已经到头（首页 / 末页），按钮置灰。
fn page_step_button(
    id: &ElementId,
    flipped: bool,
    target: Option<usize>,
    on_select: PageSelect,
    cx: &App,
) -> Button {
    let theme = active_theme(cx);
    let key = ElementId::NamedInteger(SharedString::from(format!("{id}-step")), u64::from(flipped));
    let label = if flipped {
        tr(cx, "上一页")
    } else {
        tr(cx, "下一页")
    };
    let mut glyph = Icon::new(DuckIcon::ChevronRight).size(theme.extended().icon.md);
    if flipped {
        glyph = glyph.rotate(percentage(0.5));
    }
    let button = icon_button(key, label, glyph, ButtonVariant::Ghost, cx);
    match target {
        Some(page) => button.on_click(move |_, window, cx| on_select(page, window, cx)),
        // 到底了：**真正置灰**（`disabled`），而不是只把不透明度调低。只降不透明的按钮在
        // Tab 顺序与辅助技术里仍然是个「能按」的按钮——键盘用户会停在它上面，读屏会念
        // 「下一页，按钮」，实际按下去什么都不会发生。
        None => button
            .disabled(true)
            .opacity(theme.extended().opacity.faint)
            .cursor_default(),
    }
}

/// 底部状态栏：左右两组内容，左侧贴左、右侧贴右，中间留白。
pub fn status_bar(
    left: impl IntoIterator<Item = AnyElement>,
    right: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> StatusBar {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let xs = tokens.typography.xs;
    let mut bar = StatusBar::new()
        .h(theme.density().status_bar)
        .px(tokens.spacing.sm)
        .py_0()
        .gap(tokens.spacing.xs)
        .text_size(xs.size)
        .line_height(xs.line_height);
    for item in left {
        bar = bar.left(item);
    }
    for item in right {
        bar = bar.right(item);
    }
    bar
}
