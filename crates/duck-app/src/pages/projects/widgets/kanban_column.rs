//! 看板列：列头（名称 / 计数 / WIP 提示）+ 卡片栈，整列是投放目标。

use gpui::{
    AnyElement, App, FontWeight, InteractiveElement as _, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{Icon, h_flex, v_flex};

use duck_components::content::{neutral_tag, tag};
use duck_components::notify::Tone;
use duck_components::{ButtonVariant, DuckIcon, icon_button};
use duck_project::{BoardColumn, CalendarDate, Project, Task, TaskStatus};
use duck_theme::{active_theme, tr, trf};

use super::super::ProjectsPage;
use super::super::view::status_color;
use super::TaskDrag;
use super::column_title;
use super::task_card::task_card;

/// 列宽。
///
/// 是布局几何量（不是间距阶梯、也不是字号），所以留裸值并在此写明来由：
/// 236 是左栏默认宽，列宽取 268 让「左栏 + 两列」在 1280 宽的窗口里正好排开，
/// 再多一列就得横向滚动 —— 与参考项目的 `repeat(N, 1fr)` 不同，这里用定宽是为了
/// 让每列宽度不随列数变化，拖动卡片时列不会跳宽。
///
/// `pub(crate)`：拖拽幽灵卡（`task_card::TaskDragGhost`）要与真卡片同宽，
/// 同一个数不该在两个文件里各写一份。
pub(crate) const COLUMN_WIDTH: f32 = 268.;

/// 待办列底部那段「已搁置 N」。
///
/// 为什么挂在待办列而不是另开第五列：搁置不是并列的第五种「正在发生的事」，
/// 它是「这条我先不做」——语义上紧挨着待办。另开一列会让看板默认变成五列，
/// 1280 宽的窗口放不下（见 [`COLUMN_WIDTH`] 的来由），而且用户没要过这一列。
///
/// 折叠时只留一行入口：**完全不给入口**等于这些任务在界面上不存在（它们不在
/// `DEFAULT_BOARD` 的四列里），既选不中也拖不出来。
#[allow(clippy::too_many_arguments)]
fn deferred_section(
    deferred: &[&Task],
    open: bool,
    projects: &[Project],
    today: CalendarDate,
    selected: Option<i64>,
    checked: &[i64],
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hover_tint = extended.colors.hover_tint;
    let label = trf(cx, "已搁置 {}", &[&deferred.len().to_string()]);

    let mut section = v_flex().w_full().flex_none().gap(tokens.spacing.xs).child(
        h_flex()
            .id("project-column-deferred-head")
            .w_full()
            .flex_none()
            .h(theme.density().control)
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.xs)
            .rounded(theme.components().card_radius)
            .cursor_pointer()
            .text_size(tokens.typography.xs.size)
            .line_height(tokens.typography.xs.line_height)
            .text_color(extended.colors.text_tertiary)
            .role(Role::Button)
            .aria_label(SharedString::from(label.clone()))
            .hover(move |style| style.bg(hover_tint))
            .child(
                Icon::new(if open {
                    DuckIcon::ChevronDown
                } else {
                    DuckIcon::ChevronRight
                })
                .size(extended.icon.sm),
            )
            .child(div().truncate().child(SharedString::from(label)))
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.toggle_deferred(cx));
                }
            }),
    );

    if open {
        // 整段整体压暗一档：搁置卡与待办卡同形，只靠位置区分不够 —— 拖到别处之后
        // 就再也看不出哪张是搁置过的。
        section = section.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .opacity(extended.opacity.dim)
                .children(deferred.iter().map(|task| {
                    task_card(
                        task,
                        projects,
                        today,
                        selected == Some(task.id),
                        checked.contains(&task.id),
                        view,
                        cx,
                    )
                })),
        );
    }

    section.into_any_element()
}

/// 一列。
#[allow(clippy::too_many_arguments)]
pub(crate) fn kanban_column(
    column: &BoardColumn,
    tasks: &[&Task],
    deferred: &[&Task],
    deferred_open: bool,
    projects: &[Project],
    today: CalendarDate,
    selected: Option<i64>,
    checked: &[i64],
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
    //
    // 读数按原型写成「WIP 5」/「超 WIP 2」而不是笼统的「超出 WIP」：只告诉用户
    // 「超了」不告诉超了几个，他还得自己数一遍。超限才用警示色 —— 常态挂一个
    // 橙色徽标会让人以为每一列都有问题。
    let wip = column.wip_limit.map(|limit| {
        let over = count as i64 - limit;
        if over > 0 {
            tag(
                trf(cx, "超 WIP {}", &[&over.to_string()]),
                Tone::Warning,
                cx,
            )
        } else {
            neutral_tag(
                SharedString::from(trf(cx, "WIP {}", &[&limit.to_string()])),
                cx,
            )
        }
    });

    let cards: Vec<AnyElement> = tasks
        .iter()
        .map(|task| {
            task_card(
                task,
                projects,
                today,
                selected == Some(task.id),
                checked.contains(&task.id),
                view,
                cx,
            )
        })
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
        // 凹陷区底色（浅色下比 chrome 深一整档）：列要和画布分得开、白卡片要浮得起来，
        // 见 `duck-theme::ExtendedColors::well`。
        .bg(extended.colors.well)
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
                        // 列色点（原型 `.colhead .cdot`）：与卡片上的状态点同源取色。
                        .child(
                            div()
                                .flex_none()
                                .size(px(8.))
                                .rounded_full()
                                .bg(status_color(status, cx)),
                        )
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
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .children(wip)
                        // 「列设置」入口（原型 `data-col-edit`）：列名、WIP 上限、增删
                        // 列都收在同一个弹窗里 —— 放在列头上，改的就是眼前这一列。
                        .child(
                            icon_button(
                                SharedString::from(format!("project-column-settings-{status_key}")),
                                tr(cx, "列设置"),
                                Icon::new(DuckIcon::Ellipsis)
                                    .size(extended.icon.sm)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |page, cx| {
                                        page.open_column_settings(window, cx);
                                    });
                                }
                            }),
                        ),
                ),
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
                .when(status == TaskStatus::Todo && !deferred.is_empty(), |body| {
                    body.child(deferred_section(
                        deferred,
                        deferred_open,
                        projects,
                        today,
                        selected,
                        checked,
                        view,
                        cx,
                    ))
                })
                .when(tasks.is_empty() && deferred.is_empty(), |body| {
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
