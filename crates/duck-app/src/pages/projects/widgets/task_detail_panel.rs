//! 任务详情面板（shell 级右栏）。

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, div,
};
use gpui_component::v_flex;

use duck_components::{
    ButtonVariant, body_text, button, caption_heading,
    content::{description_list, description_row, neutral_tag},
    meta_text,
};
use duck_project::{CalendarDate, Task};
use duck_theme::{active_theme, tr, trf};

use super::super::ProjectsPage;
use super::priority_dot::{priority_color, priority_label};
use super::progress_bar::task_progress;
use super::status_label;
use super::tag_chip::tag_chip;

/// 选中任务的详情。
pub(crate) fn task_detail_panel(
    task: &Task,
    today: CalendarDate,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let overdue = task.is_overdue(today);

    let date_text = |date: Option<CalendarDate>, cx: &App| -> SharedString {
        match date {
            Some(date) => SharedString::from(date.to_iso()),
            None => SharedString::from(tr(cx, "未设置")),
        }
    };

    let mut meta = vec![
        description_row(tr(cx, "状态"), body_text(status_label(task.status, cx), cx)),
        description_row(
            tr(cx, "优先级"),
            body_text(priority_label(task.priority, cx), cx),
        ),
        description_row(
            tr(cx, "开始日期"),
            body_text(date_text(task.start_date, cx), cx),
        ),
        description_row(
            tr(cx, "截止日期"),
            body_text(date_text(task.due_date, cx), cx),
        ),
    ];
    if let Some(hours) = task.estimate_hours {
        meta.push(description_row(
            tr(cx, "预估工时"),
            body_text(
                SharedString::from(trf(cx, "{} 小时", &[&hours.to_string()])),
                cx,
            ),
        ));
    }
    if let Some(hours) = task.actual_hours {
        meta.push(description_row(
            tr(cx, "实际工时"),
            body_text(
                SharedString::from(trf(cx, "{} 小时", &[&hours.to_string()])),
                cx,
            ),
        ));
    }

    let mut panel = v_flex()
        .size_full()
        .id("projects-right-sidebar")
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.sm)
        .child(caption_heading(tr(cx, "任务详情"), cx))
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.md.size)
                .line_height(tokens.typography.md.line_height)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(task.title.clone())),
        )
        .child(
            // 优先级色点是右栏里唯一靠颜色表意的地方，旁边配文字。
            v_flex().w_full().gap(tokens.spacing.xxs).child(
                div()
                    .w_full()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(priority_color(task.priority, cx))
                    .child(priority_label(task.priority, cx)),
            ),
        )
        .child(description_list(meta));

    if !task.description_md.is_empty() {
        panel = panel.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .child(caption_heading(tr(cx, "描述"), cx))
                .child(
                    div()
                        .w_full()
                        .max_h(px_detail_notes())
                        .id("project-task-description")
                        .overflow_y_scroll()
                        .p(tokens.spacing.xs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(tokens.colors.background)
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(task.description_md.clone())),
                ),
        );
    }

    panel = panel
        .child(caption_heading(tr(cx, "进度"), cx))
        .child(task_progress(
            SharedString::from(format!("project-task-detail-progress-{}", task.id)),
            task.progress,
            cx,
        ));

    if !task.tags.is_empty() {
        panel = panel.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(caption_heading(tr(cx, "标签"), cx))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap(tokens.spacing.xxs)
                        .children(task.tags.iter().map(|tag| tag_chip(tag, cx))),
                ),
        );
    }

    if task.subtasks.total > 0 {
        panel = panel.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(caption_heading(tr(cx, "子任务"), cx))
                .child(neutral_tag(
                    SharedString::from(format!("{}/{}", task.subtasks.done, task.subtasks.total)),
                    cx,
                )),
        );
    }

    if overdue {
        panel = panel.child(meta_text(tr(cx, "已逾期。"), cx));
    }

    // 动作区：完成与归档并列（归档是「从这里拿掉」，完成后仍留在已完成列）。
    panel = panel
        .child(
            button(
                "project-task-complete",
                tr(cx, "标记完成"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = task.id;
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.complete_task(id, cx));
                }
            }),
        )
        .child(
            // **不用 `Destructive`**：归档是可逆的（§5.6 明确「可逆且数据不丢、不做二次确认」），
            // 涂危险色会让人以为点下去就没了；仓库里 `Destructive` 也只用于丢弃型动作
            // （删除连接 / 全部清除 / 卸载 / 清空慢日志）。红色的位置留给 S3 的「彻底删除」（§4.3 ②）。
            button(
                "project-task-archive",
                tr(cx, "归档任务"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = task.id;
                move |_, _window, cx| {
                    let _ = view.update(cx, |page, cx| page.archive_task(id, cx));
                }
            }),
        );

    panel.into_any_element()
}

/// 描述区的滚动上限。
///
/// 布局约束（撑住可视区），不属间距阶梯，故留裸值并在此写明来由。
fn px_detail_notes() -> gpui::Pixels {
    gpui::px(220.)
}
