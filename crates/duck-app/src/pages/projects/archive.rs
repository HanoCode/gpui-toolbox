//! 归档中心的视图（设计文档 §5.6 / §4.3 ⑦）。
//!
//! 两段并列（**任务段** / **项目段**），不切页签：两段加起来才是「归档区」的全貌，
//! 一次滚完比来回点页签更接近「翻旧账」这个动作。
//!
//! 每行右侧给「恢复」与「彻底删除」两个图标动作（仓库惯例：chrome 区纯图标按钮走
//! [`toolbar_action_button`]，危险动作只把**图标**染成危险色、不铺红底 —— 铺红底会让
//! 一列行看起来全是警告）。行本身可点：点一下选中（`⌫` 与 `J` / `K` 作用在选中行上）。
//!
//! ⚠️ 行内的两个按钮必须 `stop_propagation`：否则点「恢复」会先把行选中，
//! 再被行的 `on_click` 当成一次普通点击 —— 少一次传播就在状态里多一次无谓的重读。
//!
//! **归档不可撤销标记**：本页只做显示与动作，口径全在 [`ProjectsState`]；
//! 「归档不做二次确认、彻底删除与清空必须二次确认」这条判据在 `mod.rs` 的弹窗里落。

use gpui::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement, ParentElement, Role,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, div, px,
};
use gpui_component::{
    Icon, h_flex,
    input::{Input, InputState},
    v_flex,
};

use duck_components::{
    ButtonVariant, DuckIcon, button, caption_heading, content::empty_state, meta_text,
    selectable_text, toolbar_action_button,
};
use duck_project::{ArchivedProject, ArchivedTask, now_seconds};
use duck_theme::{UnderMinute, active_theme, tr, tr_relative_time, trf, trn};

use super::ARCHIVE_KEY_CONTEXT;
use super::ProjectsPage;
use super::state::{ArchiveRow, ProjectsState};
use super::view::{chip, count_badge};

/// 归档中心搜索框的宽度（几何量，与工具条快速新建同源，故留裸值）。
const SEARCH_WIDTH: f32 = 220.;

/// 归档中心本体：标题行 + 搜索 + 清空 + 两段清单。
pub(crate) fn render_archive_center(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    search: &Entity<InputState>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let counts = state.archive_counts();

    let rows = v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .children(section(
            tr(cx, "任务"),
            state
                .archive_tasks()
                .iter()
                .map(|entry| task_row(entry, state, view, cx))
                .collect(),
            cx,
        ))
        .children(section(
            tr(cx, "项目"),
            state
                .archive_projects()
                .iter()
                .map(|entry| project_row(entry, state, view, cx))
                .collect(),
            cx,
        ));

    let body: AnyElement = if counts.is_empty() {
        empty_state(
            DuckIcon::Archive,
            tr(cx, "归档区是空的"),
            Some(SharedString::from(tr(
                cx,
                "归档的任务和项目会出现在这里，随时可以恢复。",
            ))),
            None,
            cx,
        )
        .into_any_element()
    } else if state.archive_tasks().is_empty() && state.archive_projects().is_empty() {
        // 有档案却一行都没读到 ⇒ 搜索词把两段都筛空了（口径见 §5.5）。
        empty_state(
            DuckIcon::Search,
            trf(
                cx,
                "归档区里没有匹配「{}」的条目",
                &[state.archive_search()],
            ),
            None,
            None,
            cx,
        )
        .into_any_element()
    } else {
        rows.into_any_element()
    };

    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        // 归档区自成一个键位上下文：`Esc` 在这一块里**无论焦点在不在搜索框**都能出去。
        // 页面级的 `esc` 绑定带 `!Input`（见 `KEY_CONTEXT_BARE`），焦点进了搜索框就
        // 不匹配 —— 而「翻旧账」这个动作十有八九是先点搜索框。返回按钮是鼠标出口，
        // 这一条是键盘出口，两条都得有（原先只有页面级那一条，等于没有）。
        .key_context(ARCHIVE_KEY_CONTEXT)
        .child(
            h_flex()
                .flex_none()
                .w_full()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    // 标题行最左的出口：归档中心是**整块换掉主区**的，不点左栏就没有
                    // 别的可见退路。放在标题左边而不是右上角，是因为它读作
                    // 「回到上一层」，不是「关闭这个面板」。
                    chip(
                        "projects-archive-back",
                        Some(
                            Icon::new(DuckIcon::ChevronLeft)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground)
                                .into_any_element(),
                        ),
                        SharedString::from(tr(cx, "返回")),
                        false,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, _window, cx| {
                            let _ = view.update(cx, |page, cx| page.leave_archive(cx));
                        }
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(tr(cx, "归档中心")),
                )
                .child(div().flex_1())
                .child(
                    div().w(px(SEARCH_WIDTH)).flex_none().child(
                        h_flex()
                            .h(theme.density().control)
                            .w_full()
                            .px(tokens.spacing.sm)
                            .rounded(theme.components().input_radius)
                            .border(extended.stroke.thin)
                            .border_color(extended.colors.hairline)
                            .bg(tokens.colors.background)
                            .child(
                                Input::new(search)
                                    .appearance(false)
                                    .bordered(false)
                                    .w_full(),
                            ),
                    ),
                )
                .child(
                    // 清空归档**不可逆**（要二次确认），按仓库惯例只在这类动作上用危险色。
                    button(
                        "projects-archive-clear",
                        tr(cx, "清空归档"),
                        ButtonVariant::Destructive,
                        cx,
                    )
                    .disabled(counts.is_empty())
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ =
                                view.update(cx, |page, cx| page.confirm_clear_archive(window, cx));
                        }
                    }),
                ),
        )
        .child(
            div()
                .id("projects-archive-scroll")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_y_scroll()
                .p(tokens.spacing.sm)
                .child(body),
        )
        .into_any_element()
}

/// 一段（标题 + 计数 + 若干行）；空段返回 `None` 的替代——这里返回空孩子，由外层拼。
fn section(title: &'static str, rows: Vec<AnyElement>, cx: &App) -> Option<AnyElement> {
    if rows.is_empty() {
        return None;
    }
    let tokens = active_theme(cx).tokens();
    Some(
        v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(
                h_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(SharedString::from(title), cx))
                    .child(count_badge(rows.len() as u32, cx)),
            )
            .children(rows)
            .into_any_element(),
    )
}

/// 归档中心「任务段」的一行。
fn task_row(
    entry: &ArchivedTask,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let mut meta = h_flex().flex_none().gap(tokens.spacing.sm);
    if let Some(name) = &entry.project_name {
        meta = meta.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(extended.colors.text_tertiary)
                .child(selectable_text(SharedString::from(name.clone()))),
        );
    }
    if entry.cascaded_subtasks > 0 {
        // 数量文案走 `trn` 而不是 `trf`：英文的单复数不同形（`1 subtask` / `2 subtasks`），
        // `trn` 会去 `EN_PLURAL` 取单数档，`trf` 只会写出 `1 subtasks`。
        meta = meta.child(meta_text(
            SharedString::from(trn(
                cx,
                "含 {} 个子任务",
                i64::from(entry.cascaded_subtasks),
            )),
            cx,
        ));
    }
    meta = meta.child(meta_text(archive_age(entry.task.archived_at, cx), cx));

    archive_row(
        SharedString::from(format!("projects-archive-task-{}", entry.task.id)),
        SharedString::from(entry.task.title.clone()),
        meta.into_any_element(),
        ArchiveRow::Task(entry.task.id),
        state.is_archive_selected(ArchiveRow::Task(entry.task.id)),
        view,
        cx,
    )
}

/// 归档中心「项目段」的一行。
fn project_row(
    entry: &ArchivedProject,
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();

    let meta = h_flex()
        .flex_none()
        .gap(tokens.spacing.sm)
        .child(meta_text(
            SharedString::from(trn(cx, "含 {} 个任务", i64::from(entry.cascaded_tasks))),
            cx,
        ))
        .child(meta_text(archive_age(entry.project.archived_at, cx), cx));

    archive_row(
        SharedString::from(format!("projects-archive-project-{}", entry.project.id)),
        SharedString::from(entry.project.name.clone()),
        meta.into_any_element(),
        ArchiveRow::Project(entry.project.id),
        state.is_archive_selected(ArchiveRow::Project(entry.project.id)),
        view,
        cx,
    )
}

/// 两段共用的一行：标题 + 元信息 + 恢复 + 彻底删除。
fn archive_row(
    id: SharedString,
    title: SharedString,
    meta: AnyElement,
    row: ArchiveRow,
    selected: bool,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    div()
        .id(id)
        .w_full()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xxs)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(if selected {
            tokens.colors.ring
        } else {
            extended.colors.hairline
        })
        .bg(if selected {
            extended.colors.nav_selected
        } else {
            tokens.colors.surface
        })
        // 满高行内并排：`h_flex()` 是 cross-axis 居中，行高由内容决定，这里正合适。
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens.spacing.sm)
        .role(Role::Button)
        .aria_label(title.clone())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(title)),
        )
        .child(meta)
        .child(
            toolbar_action_button(
                SharedString::from(format!("{}-restore", id_key(row))),
                tr(cx, "恢复"),
                Icon::new(DuckIcon::RotateCw)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.muted_foreground),
                false,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    // 内层动作必须先掐断传播：否则行还顺手被选中一次（见模块头）。
                    cx.stop_propagation();
                    let _ = view.update(cx, |page, cx| page.restore(row, window, cx));
                }
            }),
        )
        .child(
            toolbar_action_button(
                SharedString::from(format!("{}-purge", id_key(row))),
                tr(cx, "彻底删除"),
                Icon::new(DuckIcon::Trash2)
                    .size(extended.icon.md)
                    .text_color(tokens.colors.destructive),
                true,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    cx.stop_propagation();
                    let _ = view.update(cx, |page, cx| page.confirm_purge(row, window, cx));
                }
            }),
        )
        .on_click({
            let view = view.clone();
            move |_, _window, cx| {
                let _ = view.update(cx, |page, cx| page.select_archive(row, cx));
            }
        })
        .into_any_element()
}

/// 行的稳定 ASCII 键：元素 id 不用可见文案（切语言时 id 变化会被当成另一个元素）。
fn id_key(row: ArchiveRow) -> String {
    match row {
        ArchiveRow::Task(id) => format!("projects-archive-task-{id}"),
        ArchiveRow::Project(id) => format!("projects-archive-project-{id}"),
    }
}

/// 归档中心的右栏。
///
/// 归档行的详情与看板卡片不是一回事：卡片上的「标记完成 / 归档任务」对一条已归档的
/// 记录毫无意义，所以这里**不复用** `task_detail_panel`，只给这份记录本身的读数与
/// 两个本站特有的动作（恢复 / 彻底删除）。选中态因此不是白占的：右栏是它的详情。
pub(crate) fn render_archive_right_sidebar(
    state: &ProjectsState,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let Some(row) = state.archive_selected() else {
        return v_flex()
            .size_full()
            .id("projects-archive-right-sidebar")
            .items_center()
            .justify_center()
            .p(tokens.spacing.lg)
            .child(meta_text(tr(cx, "选中一条查看详情。"), cx))
            .into_any_element();
    };

    // 行可能刚被恢复 / 清空（右栏这一帧还没跟上），查不到就退回提示态。
    let (title, meta): (SharedString, Vec<(SharedString, SharedString)>) = match row {
        ArchiveRow::Task(id) => match state
            .archive_tasks()
            .iter()
            .find(|entry| entry.task.id == id)
        {
            Some(entry) => (
                SharedString::from(entry.task.title.clone()),
                vec![
                    (
                        SharedString::from(tr(cx, "项目")),
                        SharedString::from(
                            entry
                                .project_name
                                .clone()
                                .unwrap_or_else(|| tr(cx, "收集箱").to_owned()),
                        ),
                    ),
                    (
                        SharedString::from(tr(cx, "子任务")),
                        SharedString::from(entry.cascaded_subtasks.to_string()),
                    ),
                    (
                        SharedString::from(tr(cx, "归档时间")),
                        archive_age(entry.task.archived_at, cx),
                    ),
                ],
            ),
            None => return missing_row(cx),
        },
        ArchiveRow::Project(id) => match state
            .archive_projects()
            .iter()
            .find(|entry| entry.project.id == id)
        {
            Some(entry) => (
                SharedString::from(entry.project.name.clone()),
                vec![
                    (
                        SharedString::from(tr(cx, "连带任务")),
                        SharedString::from(entry.cascaded_tasks.to_string()),
                    ),
                    (
                        SharedString::from(tr(cx, "归档时间")),
                        archive_age(entry.project.archived_at, cx),
                    ),
                ],
            ),
            None => return missing_row(cx),
        },
    };

    let mut column = v_flex()
        .size_full()
        .id("projects-archive-right-sidebar")
        .min_h_0()
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.lg)
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(title)),
        );

    for (label, value) in meta {
        column = column.child(
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .child(
                    div()
                        .flex_none()
                        .w(px(LABEL_WIDTH))
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(selectable_text(value)),
                ),
        );
    }

    column
        .child(
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .pt(tokens.spacing.xs)
                .child(
                    button(
                        "projects-archive-detail-restore",
                        tr(cx, "恢复"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .flex_1()
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |page, cx| page.restore(row, window, cx));
                        }
                    }),
                )
                .child(
                    button(
                        "projects-archive-detail-purge",
                        tr(cx, "彻底删除"),
                        ButtonVariant::Destructive,
                        cx,
                    )
                    .flex_1()
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |page, cx| page.confirm_purge(row, window, cx));
                        }
                    }),
                ),
        )
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(extended.colors.text_tertiary)
                .child(selectable_text(tr(cx, "彻底删除后无法恢复。"))),
        )
        .into_any_element()
}

/// 右栏里的标签列宽（几何量，与详情面板同档）。
const LABEL_WIDTH: f32 = 64.;

/// 选中行在库里已经不存在时的右栏兜底。
fn missing_row(cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    v_flex()
        .size_full()
        .id("projects-archive-right-sidebar")
        .items_center()
        .justify_center()
        .p(tokens.spacing.lg)
        .child(meta_text(tr(cx, "这条记录已经不在归档区里了。"), cx))
        .into_any_element()
}

/// 「已归档 N 小时前」。
///
/// 分档（分钟 / 小时 / 天、英文单复数）一律交给 `duck-theme` 的格式化层，
/// 不在这里各写一遍 —— 全仓原先那三份手写分档都会写出 `1 minutes ago`。
fn archive_age(at: Option<i64>, cx: &App) -> SharedString {
    let Some(at) = at else {
        return SharedString::default();
    };
    let elapsed = now_seconds().saturating_sub(at);
    SharedString::from(trf(
        cx,
        "已归档 {}",
        &[&tr_relative_time(cx, elapsed, UnderMinute::JustNow)],
    ))
}
