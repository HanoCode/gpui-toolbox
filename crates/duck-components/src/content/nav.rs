//! 导航类内容件：标签页 / 折叠面板 / 树。

use duck_theme::active_theme;
use gpui::{
    AnyElement, App, Div, ElementId, Entity, FontWeight, ParentElement as _, SharedString, Styled,
    Window, div, px,
};
use gpui_component::{
    Icon, Sizable as _, Size,
    accordion::{Accordion, AccordionItem},
    h_flex,
    list::ListItem,
    tab::{Tab, TabBar},
    tree::{Tree, TreeState},
};

use crate::DuckIcon;

/// 标签页栏（下划线式）：内容区顶部的一档导航。
///
/// 与 [`crate::segmented_tabs`] 是两种语义 —— 分段控件用于「同一屏内切换视图模式」，
/// 下划线标签用于「页面级分档」。下划线样式交给上游 `TabBar::underline()`，它自带
/// 指示条滑动动画。
///
/// ⚠️ **无障碍上的已知缺口（未修，当前无调用方）**：上游 `TabBar` 的每个标签只带
/// `accessibility_label`（渲染成 `Role::Button`），**不报 `aria_selected`**，容器也没有
/// `Role::TabList` —— 读屏念得出标签名，却听不出当前在哪一档、也听不出「共几档」。
/// 上游没留注入缝，要补齐得自绘标签行（做法可参考 `kit::segmented_tabs`：
/// `Role::Tab` + `aria_selected` + `aria_position_in_set` / `aria_size_of_set`）。
pub fn tab_bar(
    id: impl Into<ElementId>,
    labels: impl IntoIterator<Item = SharedString>,
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> TabBar {
    let size = active_theme(cx).tokens().typography.sm.size;
    TabBar::new(id)
        .underline()
        .with_size(Size::Medium)
        .selected_index(selected)
        .text_size(size)
        .children(labels.into_iter().map(Tab::from))
        .on_click(move |index, window, cx| on_select(*index, window, cx))
}

/// 标签页栏 + 底部 hairline：外壳包一层，免得每个调用点各写一遍分隔线。
pub fn tabs(
    id: impl Into<ElementId>,
    labels: impl IntoIterator<Item = SharedString>,
    selected: usize,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let extended = theme.extended();
    div()
        .w_full()
        .flex_none()
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(tab_bar(id, labels, selected, on_select, cx))
}

/// 折叠面板：每节为 `(标题, 内容, 默认展开)`，可同时展开多节，无描边。
///
/// 「默认展开」是逐节给的：帮助 / 说明类内容通常全给 `true`（首屏即可读），
/// 高级选项类内容给 `false`（收起，按需展开）。
///
/// 上游 [`Accordion`] 的展开状态由它自己持有；调用方要持久化展开项时用
/// `Accordion::on_toggle_click`（本封装未暴露，需要时直接用上游件）。
pub fn accordion(
    id: impl Into<ElementId>,
    sections: impl IntoIterator<Item = (SharedString, AnyElement, bool)>,
    cx: &App,
) -> Accordion {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let body = tokens.typography.sm;
    let mut accordion = Accordion::new(id)
        .bordered(false)
        .multiple(true)
        .text_size(body.size);

    for (title, content, open) in sections {
        accordion = accordion.item(move |item: AccordionItem| {
            item.open(open)
                .title(
                    div()
                        .text_size(body.size)
                        .line_height(body.line_height)
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(content)
        });
    }
    accordion
}

/// 树：默认行渲染为「缩进 + 展开箭头 + 图标 + 文案」。
///
/// 树的展开 / 选中状态由 `state` 持有；行点击按 `ListEvent` 走上游，
/// 调用方订阅 [`Entity<TreeState>`] 的事件即可。数据（`TreeItem`）用
/// [`TreeState::items`] / `set_items` 灌入。
pub fn tree(state: &Entity<TreeState>) -> Tree {
    Tree::new(state, |_index, entry, selected, _window, cx| {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let indent = px(entry.depth() as f32 * 12.);
        let item = entry.item();

        let mut row = h_flex()
            .items_center()
            .gap(tokens.spacing.xs)
            .min_w_0()
            .pl(indent);
        if entry.is_folder() {
            let arrow = if entry.is_expanded() {
                DuckIcon::ChevronDown
            } else {
                DuckIcon::ChevronRight
            };
            row = row.child(
                Icon::new(arrow)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            );
        }
        row = row
            .child(
                Icon::new(if entry.is_folder() {
                    DuckIcon::FolderOpen
                } else {
                    DuckIcon::File
                })
                .size(extended.icon.md)
                .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .child(item.label.clone()),
            );

        ListItem::new(item.id.clone())
            .selected(selected)
            .disabled(entry.is_disabled())
            .child(row)
    })
}
