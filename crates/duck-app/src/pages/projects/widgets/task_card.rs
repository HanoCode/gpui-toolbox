//! 看板任务卡。

use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    Icon,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _},
};

use duck_components::DuckIcon;
use duck_components::content::neutral_tag;
use duck_project::{CalendarDate, Task, TaskStatus};
use duck_theme::{active_theme, tr, trf};

use super::super::ProjectsPage;
use super::TaskDrag;
use super::priority_dot::{priority_dot, priority_label};
use super::tag_chip::tag_chip;
use super::task_menu::task_menu;

/// 卡片上最多显示几个标签片（其余折叠成「+n」）。
const TAGS_ON_CARD: usize = 3;

/// 一张看板卡片。
///
/// `selected` 由调用方给：选中态是页面状态，卡片自己不查（同一帧里多张卡片，
/// 各自去查一遍是重复劳动）。
pub(crate) fn task_card(
    task: &Task,
    today: CalendarDate,
    selected: bool,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let radius = theme.components().card_radius;
    let overdue = task.is_overdue(today);

    let mut card =
        div()
            .id(SharedString::from(format!("project-task-{}", task.id)))
            .w_full()
            .p(tokens.spacing.sm)
            .rounded(radius)
            .border(extended.stroke.thin)
            .bg(tokens.colors.surface)
            .cursor_pointer()
            .flex()
            .flex_col()
            .gap(tokens.spacing.xs)
            .role(Role::Button)
            .border_color(if selected {
                tokens.colors.ring
            } else {
                extended.colors.hairline
            })
            .hover(move |style| style.bg(extended.colors.hover_tint))
            // 色点只有像素大小，读屏拿不到任何信息 ⇒ 名字里必须带上优先级。
            .aria_label(SharedString::from(format!(
                "{} · {}",
                task.title,
                priority_label(task.priority, cx)
            )))
            .on_click({
                let view = view.clone();
                let id = task.id;
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.select_task(Some(id), cx));
                }
            })
            // 拖拽：整张卡片是可拖源，载荷只带 id（见 `TaskDrag`）。必须挂在
            // `context_menu` **之前** —— 后者返回 `ContextMenu` 包装，不再是可拖元素。
            .on_drag(TaskDrag { task_id: task.id }, {
                let title = SharedString::from(task.title.clone());
                move |_drag, _, _, cx| {
                    cx.new(|_| TaskDragGhost {
                        title: title.clone(),
                    })
                }
            })
            .context_menu({
                let view = view.clone();
                let id = task.id;
                move |menu, window, cx| {
                    // 菜单是**右键那一刻**才建的（`context_menu` 存的是闭包，不是菜单本体），
                    // 所以在这里现读「这条任务 + 项目表 + 看板列」：若改成渲染时捕获，
                    // 一屏几十张卡片每帧都要克隆一份全项目表。
                    let Some((task, projects, columns)) = view
                        .read_with(cx, |page, cx| page.menu_data(id, cx))
                        .ok()
                        .flatten()
                    else {
                        return menu;
                    };
                    task_menu(menu, &task, &projects, &columns, &view, window, cx)
                }
            })
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(tokens.spacing.xs)
                    .child(
                        div()
                            .flex_none()
                            .pt(tokens.spacing.xxs)
                            .child(priority_dot(task.priority, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(task.title.clone())),
                    )
                    // 四象限徽标（原型 `.qd`）：卡片菜单里刚改的四象限，得在卡上看得见 ——
                    // 否则「四象限…」就是一个存下去却看不见效果的动作。
                    .child(neutral_tag(SharedString::from(task.quadrant.as_str()), cx))
                    // 「阻塞」标记（原型 `projects-mock.html:1187`）：内核 `Task` **没有**
                    // `block`（阻塞原因）字段，所以只标状态、不给原因 —— 但至少要让人一眼
                    // 看出这张卡卡住了，而不是非得读状态才知道。
                    .when(task.status == TaskStatus::Blocked, |row| {
                        row.child(neutral_tag(tr(cx, "阻塞"), cx))
                    })
                    .when(overdue, |row| row.child(neutral_tag(tr(cx, "逾期"), cx)))
                    .when(task.subtasks.total > 0, |row| {
                        row.child(neutral_tag(
                            SharedString::from(format!(
                                "{}/{}",
                                task.subtasks.done, task.subtasks.total
                            )),
                            cx,
                        ))
                    })
                    .child(
                        // 「更多操作」显式入口（原型 `.more`，`projects-mock.html:1180`）：
                        // 右键菜单是熟手路径，但「右键」在不少场景下不直观 —— 原型两个都给，
                        // 本批照做，弹出的就是同一份 `task_menu`。
                        //
                        // 卡片整张可点（选中任务），而按钮在卡片**内部**：`stop_propagation`
                        // 只有在更深的元素上调用才拦得住祖先，故包一层。
                        div()
                            .flex_none()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new(SharedString::from(format!(
                                    "project-task-more-{}",
                                    task.id
                                )))
                                .ghost()
                                .icon(Icon::new(DuckIcon::Ellipsis).size(extended.icon.sm))
                                // 往下弹（同 `kit.rs` 对顶栏控件的取舍）：卡片贴着看板列顶部，
                                // 向上弹会出界；`BottomRight` 让菜单右缘对齐按钮右缘，正好落回列内。
                                .dropdown_menu_with_anchor(Anchor::BottomRight, {
                                    let view = view.clone();
                                    let id = task.id;
                                    move |menu, window, cx| {
                                        // 同卡片右键：菜单在**弹出那一刻**才建，理由见上。
                                        let Some((task, projects, columns)) = view
                                            .read_with(cx, |page, cx| page.menu_data(id, cx))
                                            .ok()
                                            .flatten()
                                        else {
                                            return menu;
                                        };
                                        task_menu(
                                            menu, &task, &projects, &columns, &view, window, cx,
                                        )
                                    }
                                }),
                            ),
                    ),
            );

    if !task.tags.is_empty() {
        let shown = task.tags.iter().take(TAGS_ON_CARD);
        let extra = task.tags.len().saturating_sub(TAGS_ON_CARD);
        card = card.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap(tokens.spacing.xxs)
                .children(shown.map(|tag| tag_chip(tag, cx)))
                .when(extra > 0, |row| {
                    row.child(neutral_tag(SharedString::from(format!("+{extra}")), cx))
                }),
        );
    }

    if task.progress > 0 {
        card = card.child(super::progress_bar::task_progress(
            SharedString::from(format!("project-task-progress-{}", task.id)),
            task.progress,
            cx,
        ));
    }

    if let Some(due) = task.due_date {
        card = card.child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if overdue {
                    tokens.colors.destructive
                } else {
                    extended.colors.text_tertiary
                })
                .child(due_text(due, task.due_time.as_deref(), cx)),
        );
    }

    card.into_any_element()
}

/// 截止日文案：有时刻就带上时刻。
fn due_text(due: CalendarDate, time: Option<&str>, cx: &App) -> SharedString {
    let iso = due.to_iso();
    match time {
        Some(time) => SharedString::from(trf(cx, "截止 {} {}", &[&iso, time])),
        None => SharedString::from(trf(cx, "截止 {}", &[&iso])),
    }
}

/// 跟着指针的幽灵卡：只显示标题。
///
/// `on_drag` 要求当场给一个可渲染视图；画整张卡片会带来「幽灵比原卡片复杂」的
/// 无谓开销，标题足够指明在拖什么。
struct TaskDragGhost {
    title: SharedString,
}

impl Render for TaskDragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        div()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(theme.components().card_radius)
            .border(theme.extended().stroke.thin)
            .border_color(theme.extended().colors.hairline)
            .bg(tokens.colors.surface)
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
            .text_color(tokens.colors.foreground)
            .child(self.title.clone())
    }
}
