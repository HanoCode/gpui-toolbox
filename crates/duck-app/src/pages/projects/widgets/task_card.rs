//! 看板任务卡。
//!
//! 结构逐段对齐原型 `cardHTML`（`docs/projects-mock.html:1170-1192`）：
//!
//! ```text
//! ┃ ← 状态竖条（.card::before，仅进行中 / 阻塞）
//! ┃  .ctop   项目色点 · 首个标签 · 象限徽标 · 置顶星 ······ ⋯
//! ┃  .ctitle 标题（已完成：删除线 + 降为三级字色）
//! ┃  .pbar   进度条（仅未完成；填色按 80 / 40 / 0 三档换色）
//! ┗  .pmeta  优先级点+P{n} · 子任务 n/N · 阻塞 · 日历+截止 ······ {prog}%
//! ```
//!
//! # 与原型的差异（都在这里写死，不留「看起来一样但行为不同」）
//!
//! 1. **悬停不抬升**：原型 `.card:hover{transform:translateY(-2px)}`，本项目 `Div`
//!    没有 transform，改为「边框转强调色 + 底色转悬停薄层」—— 与全站悬停约定同源。
//! 2. **标题不限两行**：原型 `-webkit-line-clamp:2`，GPUI 没有行数截断，这里让它自然
//!    换行。长标题会把卡片撑高，但比截断丢字可接受（卡片在 268px 宽的列里）。
//! 3. **标签片用带色点的中性片**：原型 `.tagchip` 是强调色薄底无圆点；列表 / 详情 /
//!    卡片三处统一走 [`super::tag_chip`]，因为标签色是用户数据，不该在卡片上被丢掉。
//! 4. **卡片只放首个标签**（原型 `t.tags[0]`）：其余标签在详情面板里看。
//! 5. **不带时刻**：原型卡片的截止只有日期（`dueLabel`）；有时刻的那份在列表「截止」
//!    列与完整编辑器里看。
//!
//! 单击选中、**双击开完整编辑器**（原型在一个全局 `dblclick` 处理器里对任何带
//! `data-card` 的元素做这件事，所以卡片与列表行同一个手势，判据共用
//! [`super::is_double_click`]）。

use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, Context, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_component::{
    Icon,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _},
};

use duck_components::DuckIcon;
use duck_components::content::tag;
use duck_components::notify::Tone;
use duck_components::selectable_text;
use duck_project::{CalendarDate, Project, Task, TaskStatus};
use duck_theme::{active_theme, color_from_hex, tr};

use super::super::ProjectsPage;
use super::TaskDrag;
use super::priority_dot::{priority_color, priority_dot};
use super::progress_bar::task_progress_bar;
use super::quadrant_badge::quadrant_badge;
use super::tag_chip::tag_chip;
use super::task_menu::task_menu;

/// 状态竖条的宽度（原型 3px）。视觉标记，不是间距步进，故留裸值。
const STRIPE_WIDTH: f32 = 3.;

/// 一张看板卡片。
///
/// `projects` 与 `today` 由调用方给：选中态、项目表、「今天」都是页面状态，卡片自己
/// 不查（同一帧里几十张卡片，各自去查一遍是重复劳动）。
pub(crate) fn task_card(
    task: &Task,
    projects: &[Project],
    today: CalendarDate,
    selected: bool,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let done = task.status == TaskStatus::Done;
    let project = task
        .project_id
        .and_then(|id| projects.iter().find(|project| project.id == id));

    let mut card = div()
        .id(SharedString::from(format!("project-task-{}", task.id)))
        // 状态竖条要绝对定位在自己里面，所以卡片得先立成定位上下文。
        .relative()
        .w_full()
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
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
        // 色点只有几像素、完成态只有一条删除线，读屏都拿不到信息 ⇒ 名字里必须
        // 把状态与优先级写全。
        .aria_label(SharedString::from(format!(
            "{} · {} · {}",
            task.title,
            super::status_label(task.status, cx),
            super::priority_dot::priority_label(task.priority, cx)
        )))
        .on_click({
            let view = view.clone();
            let id = task.id;
            let status = task.status;
            move |event: &ClickEvent, window, cx| {
                // 双击开完整编辑器（原型全局 `dblclick` → `openTaskEditor`）：卡片与
                // 列表行同一个手势，实现见 [`super::is_double_click`]。
                if super::is_double_click(event) {
                    let _ = view.update(cx, |page, cx| {
                        super::super::dialogs::open_task_editor(
                            page,
                            Some(id),
                            None,
                            status,
                            None,
                            window,
                            cx,
                        );
                    });
                    return;
                }
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
        });

    if let Some(stripe) = status_stripe(task.status, cx) {
        card = card.child(stripe);
    }

    // ── `.ctop` ─────────────────────────────────────────────────────────
    card = card.child(
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                // 项目色点（原型 `.pdot` 带 `title`）：色点本身读屏拿不到信息，
                // 名字挂在卡片的 aria_label 上（项目名在详情里看）。
                div()
                    .flex_none()
                    .size(extended.icon.sm / 2.)
                    .rounded_full()
                    .bg(project.map_or(tokens.colors.muted_foreground, |project| {
                        color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground)
                    })),
            )
            // 只放首个标签（原型 `t.tags[0]`）。
            .children(task.tags.first().map(|tag| tag_chip(tag, cx)))
            .child(quadrant_badge(task.quadrant, cx))
            .when(task.starred, |row| {
                row.child(
                    Icon::new(DuckIcon::Star)
                        .size(extended.icon.sm)
                        .flex_none()
                        .text_color(extended.colors.warning),
                )
            })
            .child(div().flex_1())
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
                        Button::new(SharedString::from(format!("project-task-more-{}", task.id)))
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
                                    task_menu(menu, &task, &projects, &columns, &view, window, cx)
                                }
                            }),
                    ),
            ),
    );

    // ── `.ctitle` ───────────────────────────────────────────────────────
    card = card.child(
        div()
            .w_full()
            .min_w_0()
            .text_size(tokens.typography.sm.size)
            .line_height(tokens.typography.sm.line_height)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(if done {
                extended.colors.text_tertiary
            } else {
                tokens.colors.foreground
            })
            // 已完成的标题划掉（原型 `.card.st-done .ctitle`）：状态列之外再给一道
            // 记号，扫一眼就知道这条不用管了。
            .when(done, |title| title.line_through())
            .child(selectable_text(SharedString::from(task.title.clone()))),
    );

    // ── `.pbar`：仅未完成 ────────────────────────────────────────────────
    if !done {
        card = card.child(div().w_full().child(task_progress_bar(
            SharedString::from(format!("project-task-progress-{}", task.id)),
            task.progress,
            cx,
        )));
    }

    // ── `.pmeta` ────────────────────────────────────────────────────────
    //
    // 排成「左组（可换行）+ 右侧读数」两段，而不是一行人里塞一个弹性占位：原型给
    // `.pct` 挂的是 `margin-left:auto`，而换行 + 弹性占位在窄卡上会让占位自己占掉
    // 一整行、把百分比顶到第三行去。`justify_between` 的分法没有这个坑。
    let (due_text, due_color) = super::due_label(task, today, cx);
    card = card.child(
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .flex_wrap()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(tokens.spacing.xxs)
                            .child(priority_dot(task.priority, cx))
                            .child(
                                div()
                                    .text_size(tokens.typography.xs.size)
                                    .line_height(tokens.typography.xs.line_height)
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(priority_color(task.priority, cx))
                                    .child(SharedString::from(format!(
                                        "P{}",
                                        task.priority.as_i64()
                                    ))),
                            ),
                    )
                    .when(task.subtasks.total > 0, |row| {
                        row.child(meta_icon_text(
                            DuckIcon::ListOrdered,
                            SharedString::from(format!(
                                "{}/{}",
                                task.subtasks.done, task.subtasks.total
                            )),
                            extended.colors.text_tertiary,
                            cx,
                        ))
                    })
                    .when(task.status == TaskStatus::Blocked, |row| {
                        row.child(tag(tr(cx, "阻塞"), Tone::Warning, cx))
                    })
                    .child(meta_icon_text(DuckIcon::Calendar, due_text, due_color, cx)),
            )
            .when(!done, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(format!("{}%", task.progress))),
                )
            }),
    );

    card.into_any_element()
}

/// 卡片左侧的状态竖条（原型 `.card::before`，`docs/projects-mock.html:236-238`）。
///
/// 只在「进行中 / 阻塞」两态上出现：这两态是**正在占用人**的，被埋在一列卡片里也该
/// 一眼看得见。待办与已完成不画 —— 一屏几十张卡都挂一条彩条，等于一条都没挂。
fn status_stripe(status: TaskStatus, cx: &App) -> Option<AnyElement> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let color = match status {
        TaskStatus::Doing => tokens.colors.primary,
        TaskStatus::Blocked => theme.extended().colors.warning,
        _ => return None,
    };
    Some(
        div()
            .absolute()
            .left(px(0.))
            // 上下各留出卡片内边距那一段（原型 `top/bottom:8px`）：顶到圆角上会跟
            // 圆角打架，看起来像描边画歪了。
            .top(tokens.spacing.xs)
            .bottom(tokens.spacing.xs)
            .w(px(STRIPE_WIDTH))
            .rounded(px(2.))
            .bg(color)
            .into_any_element(),
    )
}

/// `.pmeta` 里「小图标 + 文字」那一格（原型 `.pmeta .m`）。
fn meta_icon_text(icon: DuckIcon, text: SharedString, color: Hsla, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .flex_none()
        .gap(tokens.spacing.xxs)
        .child(
            Icon::new(icon)
                .size(theme.extended().icon.sm)
                .text_color(color),
        )
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(color)
                .child(selectable_text(text)),
        )
        .into_any_element()
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
