//! 任务详情面板（shell 级右栏）。
//!
//! 对齐原型 `rTaskDetail`（`docs/projects-mock.html:1530`）：头部动作 → 概览 → 属性
//! → 进度 → 子任务 → 里程碑 → 描述与备注 → 底部动作。分区顺序与原型逐条对应。
//!
//! # 与原型的差异（都在这里写死，不留「看起来一样但行为不同」）
//!
//! 1. **子任务只给汇总**：内核的 `Task::subtasks` 是「`parent_id` 指向本任务的未归档
//!    子项计数」，不是一份明细（`store/mod.rs` 的分组查询）。所以这里画「已完成/总数 +
//!    完成比例」，不画逐条勾选 —— 明细在列表 / 看板里以独立任务存在。
//! 2. **进度用档位推进而不是滑块**：原型是一个 `<input type=range min=prog>`，只能往前
//!    拖。本项目没有滑块控件，改成「推进到 25/50/75/100%」的下拉，**同样只可前进**
//!    （见 `ProjectsPage::advance_progress`）；要精确回退走「完整编辑器」。
//! 3. **里程碑达成是显式动作**：原型由 `prog >= snapshot` 自动判定；内核把 `reached_at`
//!    存在自己的列上（`reach_milestone` / `unreach_milestone`），所以点圆点手动切换，
//!    旁边仍按快照给出「还差 N%」的读数。
//! 4. **「更多」不放**：卡片右上角那个 `⋯` 与右键菜单已经是同一条 `task_menu` 的入口。

use gpui::{
    Anchor, AnyElement, App, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement, Role, SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window,
    div, prelude::FluentBuilder as _,
};
use gpui_component::{
    Icon, h_flex,
    menu::{PopupMenu, PopupMenuItem},
    v_flex,
};

use duck_components::notify::Tone;
use duck_components::{
    ButtonVariant, DuckIcon, body_text, button, caption_heading,
    content::{description_list, description_row, tag},
    icon_button, meta_text, toolbar_text_dropdown_button,
};
use duck_project::{CalendarDate, Milestone, Priority, Project, Quadrant, Task, TaskStatus};
use duck_theme::{active_theme, color_from_hex, tr, trf};

use super::super::ProjectsPage;
use super::priority_dot::priority_label;
use super::progress_bar::task_progress;
use super::quadrant_badge::quadrant_badge;
use super::tag_chip::tag_chip;
use super::{quadrant_label, status_label};

/// 「推进进度」的档位（原型是连续滑块；这里给四档，够用且不必新造控件）。
const PROGRESS_STEPS: [u8; 4] = [25, 50, 75, 100];

/// 选中任务的详情。
///
/// `projects` 与 `milestones` 由调用方（`view::render_right_sidebar`）取好：这一层是
/// 领域组件，不碰页面状态 —— 要读项目表 / 走库都得先拿到 `ProjectsState`。
pub(crate) fn task_detail_panel(
    task: &Task,
    projects: &[Project],
    milestones: &[Milestone],
    today: CalendarDate,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let overdue = task.is_overdue(today);
    let settled = task.status.is_settled();
    let project = task
        .project_id
        .and_then(|id| projects.iter().find(|project| project.id == id));

    let mut panel = v_flex()
        .size_full()
        .id("projects-right-sidebar")
        .gap(tokens.spacing.sm)
        .p(tokens.spacing.sm)
        .child(head_row(task, view, cx))
        .child(overview(task, project, cx));

    // ── 属性 ────────────────────────────────────────────────────────────
    let meta = vec![
        description_row(tr(cx, "状态"), status_picker(task, view, cx)),
        description_row(tr(cx, "优先级"), priority_picker(task, view, cx)),
        description_row(tr(cx, "四象限"), quadrant_picker(task, view, cx)),
        description_row(tr(cx, "所属项目"), project_picker(task, projects, view, cx)),
        description_row(
            tr(cx, "开始 / 截止"),
            h_flex()
                .gap(tokens.spacing.xxs)
                .child(body_text(date_text(task.start_date, cx), cx))
                .child(meta_text(SharedString::from("→"), cx))
                .child({
                    let due = body_text(date_text(task.due_date, cx), cx);
                    if overdue {
                        due.text_color(tokens.colors.destructive)
                    } else {
                        due
                    }
                })
                .into_any_element(),
        ),
        description_row(tr(cx, "工时"), body_text(hours_text(task), cx)),
    ];
    panel = panel.child(section(
        DuckIcon::SlidersHorizontal,
        tr(cx, "属性"),
        None,
        vec![description_list(meta).into_any_element()],
        cx,
    ));

    // ── 进度（「只可前进」；已完成不再显示，与原型 `if (t.status !== 'done')` 一致） ──
    if !settled {
        let mut rows = vec![
            task_progress(
                SharedString::from(format!("project-task-detail-progress-{}", task.id)),
                task.progress,
                cx,
            )
            .into_any_element(),
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(progress_picker(task, view, cx))
                .child(div().flex_1())
                .child(body_text(
                    SharedString::from(format!("{}%", task.progress)),
                    cx,
                ))
                .into_any_element(),
        ];
        if let Some(next) = PROGRESS_STEPS.iter().find(|step| **step > task.progress) {
            rows.push(
                meta_text(
                    SharedString::from(trf(cx, "下一档：{}%。", &[&next.to_string()])),
                    cx,
                )
                .into_any_element(),
            );
        }
        panel = panel.child(section(
            DuckIcon::Gauge,
            tr(cx, "进度"),
            Some(meta_text(tr(cx, "只可前进"), cx).into_any_element()),
            rows,
            cx,
        ));
    }

    // ── 子任务（只有汇总，见模块头第 1 条） ──────────────────────────────
    if task.subtasks.total > 0 {
        panel = panel.child(section(
            DuckIcon::ListOrdered,
            tr(cx, "子任务"),
            Some(
                meta_text(
                    SharedString::from(format!("{}/{}", task.subtasks.done, task.subtasks.total)),
                    cx,
                )
                .into_any_element(),
            ),
            vec![
                task_progress(
                    SharedString::from(format!("project-task-subtasks-{}", task.id)),
                    subtask_percent(task),
                    cx,
                )
                .into_any_element(),
                meta_text(tr(cx, "子任务是独立任务，这里只看汇总。"), cx).into_any_element(),
            ],
            cx,
        ));
    }

    // ── 里程碑 ──────────────────────────────────────────────────────────
    let mut rows: Vec<AnyElement> = milestones
        .iter()
        .map(|milestone| milestone_row("projects-milestone", milestone, task.progress, view, cx))
        .collect();
    if rows.is_empty() {
        rows.push(meta_text(tr(cx, "还没有里程碑。"), cx).into_any_element());
    }
    rows.push(
        button(
            "projects-milestone-add",
            tr(cx, "添加里程碑"),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click({
            let view = view.clone();
            let id = task.id;
            let progress = task.progress;
            move |_, window, cx| {
                let _ = view.update(cx, |page, cx| {
                    super::super::dialogs::open_milestone_editor(page, id, progress, window, cx)
                });
            }
        })
        .into_any_element(),
    );
    panel = panel.child(section(DuckIcon::Flag, tr(cx, "里程碑"), None, rows, cx));

    // ── 描述与备注 ──────────────────────────────────────────────────────
    let mut notes: Vec<AnyElement> = Vec::new();
    if !task.description_md.is_empty() {
        notes.push(
            note_block(
                "project-task-description",
                task.description_md.clone(),
                None,
                cx,
            )
            .into_any_element(),
        );
    }
    if !task.note.is_empty() {
        notes.push(
            note_block(
                "project-task-note",
                task.note.clone(),
                Some(extended.colors.hover_tint),
                cx,
            )
            .into_any_element(),
        );
    }
    if !task.block_reason.is_empty() {
        // 阻塞原因用警示色：它解释「为什么这条卡住了」，与备注不是一件事。
        notes.push(
            div()
                .w_full()
                .p(tokens.spacing.xs)
                .rounded(theme.components().input_radius)
                .bg(extended.colors.warning.opacity(extended.opacity.tint))
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(extended.colors.warning)
                .child(SharedString::from(trf(
                    cx,
                    "阻塞原因：{}",
                    &[&task.block_reason],
                )))
                .into_any_element(),
        );
    }
    if !notes.is_empty() {
        panel = panel.child(section(
            DuckIcon::FileText,
            tr(cx, "描述与备注"),
            None,
            notes,
            cx,
        ));
    }

    if overdue {
        panel = panel.child(meta_text(tr(cx, "已逾期。"), cx));
    }

    // ── 底部动作 ────────────────────────────────────────────────────────
    // 「重新打开」在已完成态出现（原型 `data-act="reopen"`），其余时候是「标记完成」。
    let primary = if settled {
        button(
            "project-task-reopen",
            tr(cx, "重新打开"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click({
            let view = view.clone();
            let id = task.id;
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| {
                    page.set_task_status(id, TaskStatus::Todo, cx);
                });
            }
        })
    } else {
        button(
            "project-task-complete",
            tr(cx, "标记完成"),
            ButtonVariant::Primary,
            cx,
        )
        .on_click({
            let view = view.clone();
            let id = task.id;
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.complete_task(id, cx));
            }
        })
    };
    panel = panel.child(primary).child(
        // **不用 `Destructive`**：归档是可逆的（§5.6 明确「可逆且数据不丢、不做二次
        // 确认」），涂危险色会让人以为点下去就没了；仓库里 `Destructive` 也只用于丢弃型
        // 动作（删除连接 / 全部清除 / 卸载 / 清空慢日志）。
        button(
            "project-task-archive",
            tr(cx, "归档任务"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click({
            let view = view.clone();
            let id = task.id;
            move |_, _, cx| {
                let _ = view.update(cx, |page, cx| page.archive_task(id, cx));
            }
        }),
    );

    panel.into_any_element()
}

/// 头部：标题 + 置顶 + 完整编辑。
fn head_row(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    let icon_size = active_theme(cx).extended().icon.sm;
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.xs)
        .child(caption_heading(tr(cx, "任务详情"), cx))
        .child(div().flex_1())
        .child(
            icon_button(
                "projects-detail-star",
                tr(
                    cx,
                    if task.starred {
                        "取消置顶"
                    } else {
                        "置顶"
                    },
                ),
                Icon::new(DuckIcon::Star).size(icon_size),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = task.id;
                let next = !task.starred;
                move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| page.set_starred(id, next, cx));
                }
            }),
        )
        .child(
            icon_button(
                "projects-detail-edit",
                tr(cx, "完整编辑"),
                Icon::new(DuckIcon::Pencil).size(icon_size),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = task.id;
                let status = task.status;
                move |_, window, cx| {
                    let _ = view.update(cx, |page, cx| {
                        super::super::dialogs::open_task_editor(
                            page,
                            Some(id),
                            None,
                            status,
                            window,
                            cx,
                        )
                    });
                }
            }),
        )
        .into_any_element()
}

/// 概览：项目色点 + 项目名 + 象限徽标 + 阻塞 / 置顶标签 + `#id` + 标题 + 标签片。
fn overview(task: &Task, project: Option<&Project>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let project_color = project.map_or(tokens.colors.muted_foreground, |project| {
        color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground)
    });
    let project_label = project.map_or_else(
        || SharedString::from(tr(cx, "未归入项目")),
        |project| SharedString::from(project.name.clone()),
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    // 色点只有几像素，读屏拿不到信息 ⇒ 项目名紧跟在它右边。
                    div()
                        .flex_none()
                        .size(extended.icon.sm * 0.5)
                        .rounded_full()
                        .bg(project_color),
                )
                .child(meta_text(project_label, cx))
                .child(quadrant_badge(task.quadrant, cx))
                .when(task.status == TaskStatus::Blocked, |row| {
                    row.child(tag(tr(cx, "阻塞"), Tone::Warning, cx))
                })
                .when(task.starred, |row| {
                    row.child(tag(tr(cx, "已置顶"), Tone::Info, cx))
                })
                .child(div().flex_1())
                .child(meta_text(SharedString::from(format!("#{}", task.id)), cx)),
        )
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.md.size)
                .line_height(tokens.typography.md.line_height)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(task.title.clone())),
        )
        .when(!task.tags.is_empty(), |panel| {
            panel.child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap(tokens.spacing.xxs)
                    .children(task.tags.iter().map(|tag| tag_chip(tag, cx))),
            )
        })
        .into_any_element()
}

/// 一个分区：标题行（图标 + 文字 + 可选的右侧读数）+ 若干行内容。
fn section(
    icon: DuckIcon,
    label: &'static str,
    trailing: Option<AnyElement>,
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    Icon::new(icon)
                        .size(extended.icon.sm)
                        .text_color(extended.colors.text_tertiary),
                )
                .child(caption_heading(SharedString::from(label), cx))
                .child(div().flex_1())
                .children(trailing),
        )
        .children(rows)
        .into_any_element()
}

/// 下拉选择：当前值写在按钮上（原型 `.vbtn`）。
///
/// 前导图标用的是这一项的**语义图标**（状态 = 滑杆 / 优先级 = 旗…），不是原型的尾部
/// caret —— 本项目的下拉按钮统一样式是「图标在前、无 caret」，与工具条那几个同源。
fn value_button(
    id: &'static str,
    icon: DuckIcon,
    value: SharedString,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    cx: &App,
) -> AnyElement {
    let icon_size = active_theme(cx).extended().icon.sm;
    toolbar_text_dropdown_button(
        id,
        value,
        Icon::new(icon).size(icon_size),
        Anchor::TopLeft,
        build,
        cx,
    )
}

fn status_picker(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let view = view.clone();
    let id = task.id;
    let current = task.status;
    let items: Vec<(TaskStatus, SharedString)> = TaskStatus::ALL
        .iter()
        .map(|status| (*status, SharedString::from(status_label(*status, cx))))
        .collect();
    value_button(
        "projects-detail-status",
        DuckIcon::SlidersHorizontal,
        SharedString::from(status_label(current, cx)),
        move |menu, _, _cx| {
            items.iter().fold(menu, |menu, (status, label)| {
                let status = *status;
                menu.item(PopupMenuItem::new(label.clone()).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.set_task_status(id, status, cx));
                    }
                }))
            })
        },
        cx,
    )
}

fn priority_picker(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let view = view.clone();
    let id = task.id;
    let current = task.priority;
    let items: Vec<(Priority, SharedString)> = Priority::ALL
        .iter()
        .map(|priority| {
            (
                *priority,
                SharedString::from(format!(
                    "P{} {}",
                    priority.as_i64(),
                    priority_label(*priority, cx)
                )),
            )
        })
        .collect();
    value_button(
        "projects-detail-priority",
        DuckIcon::Flag,
        SharedString::from(format!(
            "P{} {}",
            current.as_i64(),
            priority_label(current, cx)
        )),
        move |menu, _, _cx| {
            items.iter().fold(menu, |menu, (priority, label)| {
                let priority = *priority;
                menu.item(PopupMenuItem::new(label.clone()).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.set_priority(id, priority, cx));
                    }
                }))
            })
        },
        cx,
    )
}

fn quadrant_picker(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let view = view.clone();
    let id = task.id;
    let current = task.quadrant;
    let items: Vec<(Quadrant, SharedString)> = Quadrant::ALL
        .iter()
        .map(|quadrant| (*quadrant, SharedString::from(quadrant_label(*quadrant, cx))))
        .collect();
    value_button(
        "projects-detail-quadrant",
        DuckIcon::Layers,
        SharedString::from(quadrant_label(current, cx)),
        move |menu, _, _cx| {
            items.iter().fold(menu, |menu, (quadrant, label)| {
                let quadrant = *quadrant;
                menu.item(PopupMenuItem::new(label.clone()).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.set_quadrant(id, quadrant, cx));
                    }
                }))
            })
        },
        cx,
    )
}

fn project_picker(
    task: &Task,
    projects: &[Project],
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let view = view.clone();
    let id = task.id;
    // 「未归入」也得能选回来 —— 只列项目的话，移错了就再也回不到收集箱。
    let items: Vec<(Option<i64>, SharedString)> =
        std::iter::once((None, SharedString::from(tr(cx, "未归入项目"))))
            .chain(
                projects
                    .iter()
                    .map(|project| (Some(project.id), SharedString::from(project.name.clone()))),
            )
            .collect();
    let current = task
        .project_id
        .and_then(|wanted| projects.iter().find(|project| project.id == wanted))
        .map_or_else(
            || SharedString::from(tr(cx, "未归入项目")),
            |project| SharedString::from(project.name.clone()),
        );
    value_button(
        "projects-detail-project",
        DuckIcon::FolderOpen,
        current,
        move |menu, _, _cx| {
            items.iter().fold(menu, |menu, (target, label)| {
                let target = *target;
                menu.item(PopupMenuItem::new(label.clone()).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.set_task_project(id, target, cx));
                    }
                }))
            })
        },
        cx,
    )
}

/// 「推进进度」；已经到顶时下拉里没有可选档（按钮仍在，只是展开为空）。
fn progress_picker(task: &Task, view: &WeakEntity<ProjectsPage>, cx: &App) -> AnyElement {
    let view = view.clone();
    let id = task.id;
    let items: Vec<u8> = PROGRESS_STEPS
        .iter()
        .copied()
        .filter(|step| *step > task.progress)
        .collect();
    value_button(
        "projects-detail-progress",
        DuckIcon::Play,
        SharedString::from(tr(cx, "推进进度")),
        move |menu, _, _cx| {
            items.iter().fold(menu, |menu, step| {
                let step = *step;
                menu.item(
                    PopupMenuItem::new(SharedString::from(format!("{step}%"))).on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |page, cx| page.advance_progress(id, step, cx));
                        }
                    }),
                )
            })
        },
        cx,
    )
}

/// 一行里程碑：圆点（点击切换达成）+ 标题与读数 + 删除。
///
/// ⚠️ `id_prefix` **必须逐处不同**：右栏详情与任务编辑弹窗会**同时**渲染同一个任务的
/// 同一批里程碑（弹窗盖在页面上，页面照旧重绘），`ElementId` 只在**窗口**内唯一 ——
/// 两处都用 `projects-milestone` 会让圆点 / 删除键的命中状态互相串台。
pub(crate) fn milestone_row(
    id_prefix: &str,
    milestone: &Milestone,
    task_progress: u8,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let reached = milestone.is_reached();
    let color = if reached {
        extended.colors.success
    } else {
        tokens.colors.primary
    };

    let mut meta: Vec<AnyElement> = Vec::new();
    if let Some(date) = milestone.due_date {
        meta.push(meta_text(SharedString::from(date.to_iso()), cx).into_any_element());
    }
    if let Some(snapshot) = milestone.progress_snapshot {
        meta.push(
            meta_text(
                SharedString::from(trf(cx, "快照 {}%", &[&snapshot.to_string()])),
                cx,
            )
            .into_any_element(),
        );
        // 「还差多少」按快照算（内核不自动达成，见模块头第 3 条）。
        if !reached && snapshot > task_progress {
            meta.push(
                meta_text(
                    SharedString::from(trf(
                        cx,
                        "还差 {}%",
                        &[&(snapshot - task_progress).to_string()],
                    )),
                    cx,
                )
                .into_any_element(),
            );
        }
    }
    if reached {
        meta.push(
            meta_text(tr(cx, "已达成"), cx)
                .text_color(extended.colors.success)
                .into_any_element(),
        );
    }

    let label = if reached {
        trf(cx, "取消达成「{}」", &[&milestone.title])
    } else {
        trf(cx, "标记「{}」已达成", &[&milestone.title])
    };

    h_flex()
        .w_full()
        .items_start()
        .gap(tokens.spacing.xs)
        .child(
            div()
                .id(SharedString::from(format!(
                    "{id_prefix}-dot-{}",
                    milestone.id
                )))
                .flex_none()
                .mt(tokens.spacing.xs)
                .size(extended.icon.sm * 0.75)
                .rounded_full()
                .border(extended.stroke.strong)
                .border_color(color)
                .when(reached, |dot| dot.bg(color))
                .role(Role::Button)
                .aria_label(SharedString::from(label))
                .on_click({
                    let view = view.clone();
                    let id = milestone.id;
                    let next = !reached;
                    move |_, _, cx| {
                        let _ = view.update(cx, |page, cx| page.toggle_milestone(id, next, cx));
                    }
                }),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(body_text(SharedString::from(milestone.title.clone()), cx))
                .child(
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .children(meta),
                ),
        )
        .child(
            icon_button(
                SharedString::from(format!("{id_prefix}-remove-{}", milestone.id)),
                tr(cx, "删除里程碑"),
                Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                let id = milestone.id;
                move |_, _, cx| {
                    let _ = view.update(cx, |page, cx| page.remove_milestone(id, cx));
                }
            }),
        )
        .into_any_element()
}

/// 一段只读正文（描述 / 备注）。
///
/// `id` 必须逐块不同：两段都挂 `overflow_y_scroll`，同 id 会让滚动位置互相串。
fn note_block(id: &'static str, text: String, bg: Option<Hsla>, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .w_full()
        .max_h(px_detail_notes())
        .id(id)
        .overflow_y_scroll()
        .p(tokens.spacing.xs)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(bg.unwrap_or(tokens.colors.background))
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(tokens.colors.foreground)
        .child(SharedString::from(text))
}

/// 日期文案：没排期写「未设置」。
fn date_text(date: Option<CalendarDate>, cx: &App) -> SharedString {
    match date {
        Some(date) => SharedString::from(date.to_iso()),
        None => SharedString::from(tr(cx, "未设置")),
    }
}

/// 工时文案：原型是 `实际 / 预估 h`。
fn hours_text(task: &Task) -> SharedString {
    SharedString::from(format!(
        "{} / {} h",
        task.actual_hours.unwrap_or(0.),
        task.estimate_hours.unwrap_or(0.)
    ))
}

/// 子任务完成度（0–100）。
fn subtask_percent(task: &Task) -> u8 {
    let total = task.subtasks.total;
    if total == 0 {
        return 0;
    }
    u8::try_from(task.subtasks.done.saturating_mul(100) / total).unwrap_or(100)
}

/// 正文区的滚动上限。
///
/// 布局约束（撑住可视区），不属间距阶梯，故留裸值并在此写明来由。
fn px_detail_notes() -> gpui::Pixels {
    gpui::px(220.)
}
