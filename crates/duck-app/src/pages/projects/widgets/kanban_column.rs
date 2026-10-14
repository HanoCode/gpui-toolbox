//! 看板列：列头（名称 / 计数 / WIP 提示）+ 卡片栈，整列是投放目标。

use gpui::{
    AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{Icon, h_flex, v_flex};

use duck_components::DuckIcon;
use duck_components::content::neutral_tag;
use duck_project::{BoardColumn, CalendarDate, Task};
use duck_theme::{active_theme, tr};

use super::super::ProjectsPage;
use super::TaskDrag;
use super::column_title;
use super::task_card::task_card;

/// 列宽。
///
/// 是布局几何量（不是间距阶梯、也不是字号），所以留裸值并在此写明来由：
/// 236 是左栏默认宽，列宽取 268 让「左栏 + 两列」在 1280 宽的窗口里正好排开，
/// 再多一列就得横向滚动 —— 与参考项目的 `repeat(N, 1fr)` 不同，这里用定宽是为了
/// 让每列宽度不随列数变化，拖动卡片时列不会跳宽。
const COLUMN_WIDTH: f32 = 268.;

/// 一列。
pub(crate) fn kanban_column(
    column: &BoardColumn,
    tasks: &[&Task],
    today: CalendarDate,
    selected: Option<i64>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let status = column.status_key;
    let status_key = status.as_str();

    // 列名优先用用户改过的；没改过（内核里是 `NULL`）才回落内置文案。
    // 与卡片菜单的「状态…」子菜单共用一份（见 `super::column_title`）。
    let title = column_title(column, cx);
    let count = tasks.len();
    // 超出 WIP 只提示、不阻断：看板是给人看的，不是写闸门。
    let over_wip = column.wip_limit.is_some_and(|limit| count as i64 > limit);

    let cards: Vec<AnyElement> = tasks
        .iter()
        .map(|task| task_card(task, today, selected == Some(task.id), view, cx))
        .collect();

    // `drag_over` / `on_drop` / `hover` 的闭包必须是 `'static`，而 `extended` 借的是
    // `cx` —— 颜色是 `Copy` 的，先取出来再 move 进去。
    let drop_target = extended.colors.drop_target;
    let hover_tint = extended.colors.hover_tint;

    v_flex()
        .flex_none()
        .w(px(COLUMN_WIDTH))
        .h_full()
        .min_h_0()
        .rounded(theme.components().card_radius)
        .bg(extended.colors.chrome)
        // 投放高亮：`drag_over` 只在**兼容载荷**悬停时才回调，天然就是判据，
        // 不需要自己在页面上记一个「现在悬在哪一列」。
        .drag_over::<TaskDrag>(move |style, _, _, _| style.bg(drop_target))
        .on_drop::<TaskDrag>({
            let view = view.clone();
            move |drag: &TaskDrag, _window, cx| {
                let id = drag.task_id;
                let _ = view.update(cx, |page, cx| page.move_task_to(id, status, cx));
            }
        })
        .child(
            h_flex()
                .w_full()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .child(
                    h_flex()
                        .min_w_0()
                        .gap(tokens.spacing.xs)
                        .child(
                            div()
                                .truncate()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(tokens.colors.foreground)
                                .child(title),
                        )
                        .child(neutral_tag(SharedString::from(count.to_string()), cx)),
                )
                .children(over_wip.then(|| neutral_tag(tr(cx, "超出 WIP"), cx))),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "project-column-body-{status_key}"
                )))
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.xs)
                .pb(tokens.spacing.xs)
                .overflow_y_scroll()
                .children(cards)
                .when(tasks.is_empty(), |body| {
                    body.child(
                        div()
                            .w_full()
                            .py(tokens.spacing.lg)
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .text_size(tokens.typography.xs.size)
                                    .line_height(tokens.typography.xs.line_height)
                                    .text_color(extended.colors.text_tertiary)
                                    .child(tr(cx, "拖一张卡片到这里")),
                            ),
                    )
                }),
        )
        // 列底「+ 添加任务」（原型 `.addcard`：虚线框，悬停转实线）。
        // 挂在列本体上而不是滚动区里：它不跟着卡片一起滚，永远在手边。
        .child(
            h_flex()
                .id(SharedString::from(format!(
                    "project-column-add-{status_key}"
                )))
                .flex_none()
                .mx(tokens.spacing.sm)
                .mb(tokens.spacing.sm)
                .h(theme.density().control)
                .justify_center()
                .gap(tokens.spacing.xs)
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_dashed()
                .border_color(extended.colors.hairline)
                .cursor_pointer()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                // 自绘的可点行对读屏不存在，角色与名字都得显式给。
                .role(Role::Button)
                .aria_label(tr(cx, "添加任务"))
                .hover(move |style| style.bg(hover_tint))
                .child(Icon::new(DuckIcon::Plus).size(extended.icon.sm))
                .child(tr(cx, "添加任务"))
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ =
                            view.update(cx, |page, cx| page.add_task_to_column(status, window, cx));
                    }
                }),
        )
        .into_any_element()
}
