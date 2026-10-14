//! 项目管理页的弹窗族 —— 原型 `open*` 系列（`docs/projects-mock.html:1758-2669`）
//! 的逐个对应。
//!
//! 每个弹窗都走同一条骨架：`open_dialog` + `modal_drag`/`modal_offset`（量尺在
//! prepaint，`.opacity(offset.opacity)` 与 `.close_button(false)` 两条不能省，
//! 见页面 `confirm` 的说明）+ 一个挂在 `Entity` 上的表单状态。表单状态与
//! `redis/mod.rs` 的 `EditForm` 同一理由：`open_dialog` 的内容闭包是 `Fn`
//! （重排会重跑），拿不到 `&mut ProjectsPage`，但可以克隆 `Entity` 自由读写。
//!
//! # 字段口径
//!
//! - 日期一律 `YYYY-MM-DD` 文本输入（空 = 没排期）。gpui-component 没有日期
//!   选择控件，自造一个日历弹层的成本远大于「看着 ISO 文本打字」；
//! - 工时是小时数文本（空 / 非法 = 不设）；
//! - 进度用「− 10 / + 10」步进 + 百分比读数，写入口径与内核一致（越界钳 0–100）；
//! - 标签管理弹窗的清单**不拍快照**：删除 / 换色后要立刻反映，所以渲染时从页面
//!   状态现读（弹窗内容闭包每帧重跑，现读零成本）。

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled, WeakEntity,
    Window, div, px,
};
use gpui_component::{
    WindowExt as _,
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    v_flex,
};

use duck_components::{
    ButtonVariant, button, dialog_top_offset, dropdown_select, field_hint, form_field, form_row,
    modal_drag, modal_drag_area, modal_offset, modal_title_row,
};
use duck_project::{
    BoardColumn, CalendarDate, ColumnSpec, LogKind, NewProject, NewTask, Priority, Project,
    ProjectPatch, Quadrant, Tag, Task, TaskPatch, TaskStatus,
};
use duck_theme::{active_theme, color_from_hex, tr, trf};

use super::ProjectsPage;
use super::quickparse::{self, QuickParse};
use super::widgets::status_label;

/// 项目色板（原型 `PJCOLORS`）：8 个高区分度颜色，新建 / 编辑项目共用。
pub(crate) const PROJECT_COLORS: [&str; 8] = [
    "#3B82F6", "#8B5CF6", "#EC4899", "#EF4444", "#F59E0B", "#10B981", "#14B8A6", "#6B7280",
];

/// 任务编辑弹窗宽（双列大表单）。布局几何量，留裸值并写明来由。
const TASK_EDITOR_WIDTH: f32 = 760.;
/// 其余单列表单的宽度（比确认框略宽，容下「标签 + 计数」一行）。
const FORM_WIDTH: f32 = 520.;

// ---------------------------------------------------------------------------
// 共用小件
// ---------------------------------------------------------------------------

/// 单行输入的现值（去首尾空白）。
pub(crate) fn input_value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

fn textarea_value(input: &Entity<TextareaState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

/// 日期输入 → [`CalendarDate`]；空串 / 非法文本都按「没排期」。
fn parse_date_input(input: &Entity<InputState>, cx: &App) -> Option<CalendarDate> {
    let text = input_value(input, cx);
    (!text.is_empty())
        .then(|| CalendarDate::parse(&text))
        .flatten()
}

/// 工时输入 → 小时数；空串 / 非法 / 非正数都按「不设」。
fn parse_hours_input(input: &Entity<InputState>, cx: &App) -> Option<f64> {
    input_value(input, cx)
        .parse::<f64>()
        .ok()
        .filter(|hours| *hours > 0.)
}

/// 单行输入框（与页面 `input_box` 同一套 token）。
fn line_input(input: &Entity<InputState>, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .w_full()
        .h(theme.density().control)
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(Input::new(input).appearance(false).bordered(false).w_full())
}

/// 多行输入框（描述 / 备注 / 日志正文）。
///
/// 高度按**行数 × 正文行高**算，不写死像素：行高是字阶 token 的一部分，跟着
/// 「缩放」档一起变；写死一个 `px` 会在放大字号后把正文压成一条缝。
fn area_input(
    input: &Entity<TextareaState>,
    min_rows: usize,
    max_rows: usize,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let row = tokens.typography.sm.line_height;
    div()
        .w_full()
        .h(row * min_rows as f32)
        .max_h(row * max_rows as f32)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(
            Textarea::new(input)
                .appearance(false)
                .bordered(false)
                .w_full()
                .h_full(),
        )
}

fn new_input(window: &mut Window, cx: &mut App, placeholder: &str) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(SharedString::from(placeholder)))
}

fn new_area(window: &mut Window, cx: &mut App, placeholder: &str) -> Entity<TextareaState> {
    cx.new(|cx| TextareaState::new(window, cx).placeholder(SharedString::from(placeholder)))
}

/// 回填一个单行输入框（编辑模式打开弹窗时用）。
///
/// 写成自由函数而不是 `from_task` 里的闭包：两个闭包同时借 `cx` / `window` 会撞
/// `E0524`（闭包捕获的是同一个 `&mut`）。函数各自拿一次借用，互不相干。
fn fill_input(input: &Entity<InputState>, value: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |state, cx| {
        state.set_value(SharedString::from(value.to_owned()), window, cx)
    });
}

/// 回填一个多行输入框。
fn fill_area(input: &Entity<TextareaState>, value: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |state, cx| {
        state.set_value(SharedString::from(value.to_owned()), window, cx)
    });
}

// ---------------------------------------------------------------------------
// 任务编辑大表单（原型 openTaskEditor）
// ---------------------------------------------------------------------------

/// 任务编辑表单的状态（新建与编辑共用；`task_id` 区分两者）。
pub(crate) struct TaskEditorForm {
    /// `Some(id)` = 编辑既有任务；`None` = 新建。
    pub(crate) task_id: Option<i64>,
    title: Entity<InputState>,
    desc: Entity<TextareaState>,
    start: Entity<InputState>,
    due: Entity<InputState>,
    estimate: Entity<InputState>,
    actual: Entity<InputState>,
    block: Entity<InputState>,
    note: Entity<TextareaState>,
    project: Option<i64>,
    status: TaskStatus,
    priority: Priority,
    quadrant: Quadrant,
    progress: u8,
    tags: Vec<i64>,
    /// 打开那一刻的项目与标签清单（弹窗生命周期内不会改它们，不用订阅）。
    projects: Vec<Project>,
    available_tags: Vec<Tag>,
}

impl TaskEditorForm {
    /// 新建模式。
    fn new(
        window: &mut Window,
        cx: &mut App,
        preset_project: Option<i64>,
        preset_status: TaskStatus,
        projects: Vec<Project>,
        available_tags: Vec<Tag>,
    ) -> Self {
        Self {
            task_id: None,
            title: new_input(window, cx, "一句话说清要做什么"),
            desc: new_area(window, cx, "背景、验收标准、参考实现…"),
            start: new_input(window, cx, "YYYY-MM-DD"),
            due: new_input(window, cx, "YYYY-MM-DD"),
            estimate: new_input(window, cx, "0"),
            actual: new_input(window, cx, "0"),
            block: new_input(window, cx, "例如：等上游开权限"),
            note: new_area(window, cx, "只给自己看的提示…"),
            project: preset_project,
            status: preset_status,
            priority: Priority::P2,
            quadrant: Quadrant::Q2,
            progress: 0,
            tags: Vec::new(),
            projects,
            available_tags,
        }
    }

    /// 编辑模式：从既有任务回填。
    fn from_task(
        window: &mut Window,
        cx: &mut App,
        task: &Task,
        projects: Vec<Project>,
        available_tags: Vec<Tag>,
    ) -> Self {
        let mut form = Self::new(
            window,
            cx,
            task.project_id,
            task.status,
            projects,
            available_tags,
        );
        form.task_id = Some(task.id);
        form.priority = task.priority;
        form.quadrant = task.quadrant;
        form.progress = task.progress;
        form.tags = task.tags.iter().map(|tag| tag.id).collect();
        fill_input(&form.title, &task.title, window, cx);
        fill_area(&form.desc, &task.description_md, window, cx);
        if let Some(date) = task.start_date {
            fill_input(&form.start, &date.to_iso(), window, cx);
        }
        if let Some(date) = task.due_date {
            fill_input(&form.due, &date.to_iso(), window, cx);
        }
        if let Some(hours) = task.estimate_hours {
            fill_input(&form.estimate, &hours.to_string(), window, cx);
        }
        if let Some(hours) = task.actual_hours {
            fill_input(&form.actual, &hours.to_string(), window, cx);
        }
        fill_input(&form.block, &task.block_reason, window, cx);
        fill_area(&form.note, &task.note, window, cx);
        form
    }

    /// 标题是否为空（空标题不给保存 —— 与内核 `require_non_empty` 同口径）。
    fn title_is_empty(&self, cx: &App) -> bool {
        input_value(&self.title, cx).is_empty()
    }

    /// 步进进度（−10 / +10，越界钳 0–100，与内核写入口径一致）。
    fn step_progress(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = (i32::from(self.progress) + delta).clamp(0, 100) as u8;
        if next != self.progress {
            self.progress = next;
            cx.notify();
        }
    }

    /// 开关一个标签。
    fn toggle_tag(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(index) = self.tags.iter().position(|known| *known == id) {
            self.tags.remove(index);
        } else {
            self.tags.push(id);
        }
        cx.notify();
    }

    /// 读出全部字段（「保存」那一刻用）。
    fn collect(&self, cx: &App) -> CollectedTask {
        CollectedTask {
            title: input_value(&self.title, cx),
            description: textarea_value(&self.desc, cx),
            start: parse_date_input(&self.start, cx),
            due: parse_date_input(&self.due, cx),
            estimate: parse_hours_input(&self.estimate, cx),
            actual: parse_hours_input(&self.actual, cx),
            block: input_value(&self.block, cx),
            note: textarea_value(&self.note, cx),
            project: self.project,
            status: self.status,
            priority: self.priority,
            quadrant: self.quadrant,
            progress: self.progress,
            tags: self.tags.clone(),
        }
    }
}

/// [`TaskEditorForm::collect`] 的结果 —— 纯数据，校验与落地两步分开。
pub(crate) struct CollectedTask {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) start: Option<CalendarDate>,
    pub(crate) due: Option<CalendarDate>,
    pub(crate) estimate: Option<f64>,
    pub(crate) actual: Option<f64>,
    pub(crate) block: String,
    pub(crate) note: String,
    pub(crate) project: Option<i64>,
    pub(crate) status: TaskStatus,
    pub(crate) priority: Priority,
    pub(crate) quadrant: Quadrant,
    pub(crate) progress: u8,
    pub(crate) tags: Vec<i64>,
}

impl CollectedTask {
    /// 转成内核的新建输入。
    pub(crate) fn into_new_task(self) -> NewTask {
        let mut new = NewTask::new(self.title)
            .description(self.description)
            .status(self.status)
            .priority(self.priority)
            .quadrant(self.quadrant)
            .dates(self.start, self.due)
            .hours(self.estimate, self.actual)
            .block_reason(self.block)
            .note(self.note)
            .progress(self.progress)
            .tags(self.tags);
        if let Some(project_id) = self.project {
            new = new.project(project_id);
        }
        new
    }

    /// 转成内核的局部更新（编辑模式：整表单覆盖所有字段）。
    pub(crate) fn into_patch(self) -> TaskPatch {
        let mut patch = TaskPatch::new()
            .title(self.title)
            .description(self.description)
            .priority(self.priority)
            .quadrant(self.quadrant)
            .dates(self.start, self.due)
            .hours(self.estimate, self.actual)
            .block_reason(self.block)
            .note(self.note)
            .progress(self.progress)
            .tags(self.tags);
        patch = match self.project {
            Some(project_id) => patch.project(project_id),
            None => patch.clear_project(),
        };
        patch
    }
}

/// 打开任务编辑弹窗（新建或编辑）。
pub(crate) fn open_task_editor(
    page: &mut ProjectsPage,
    task_id: Option<i64>,
    preset_project: Option<i64>,
    preset_status: TaskStatus,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let (existing, projects, available_tags) = {
        let state = page.state.read(cx);
        (
            task_id.and_then(|id| state.tasks().iter().find(|task| task.id == id).cloned()),
            state.projects().to_vec(),
            state.tags().to_vec(),
        )
    };
    let form = match &existing {
        Some(task) => {
            cx.new(|cx| TaskEditorForm::from_task(window, cx, task, projects, available_tags))
        }
        None => cx.new(|cx| {
            TaskEditorForm::new(
                window,
                cx,
                preset_project,
                preset_status,
                projects,
                available_tags,
            )
        }),
    };

    let heading = match task_id {
        Some(_) => tr(cx, "编辑任务"),
        None => tr(cx, "新建任务"),
    };
    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();
        let heading_for_content = heading.clone();

        dialog
            .w(px(TASK_EDITOR_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "保存修改"))
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    task_editor_body(
                        &heading_for_content,
                        &drag_for_content,
                        &form_for_content,
                        cx,
                    ),
                ))
            })
            .on_ok(move |_, _, cx| {
                let collected = form_for_ok.read(cx).collect(cx);
                if collected.title.is_empty() {
                    // 空标题不给保存；返回 `false` 让弹窗留着，用户继续填。
                    return false;
                }
                let task_id = form_for_ok.read(cx).task_id;
                let _ = view_for_ok.update(cx, |page, cx| {
                    page.submit_task_editor(task_id, collected, cx)
                });
                true
            })
    });
}

/// 任务编辑弹窗的正文：双列（原型 `ed-*` 的左右两栏）。
fn task_editor_body(
    heading: &str,
    drag: &Entity<duck_components::ModalDrag>,
    form: &Entity<TaskEditorForm>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);

    // ---- 左列：标题 / 描述 / 日期 / 工时 / 进度 / 阻塞 ----
    let left = v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.sm)
        .child(form_field(
            tr(cx, "标题"),
            line_input(&read.title, cx),
            read.title_is_empty(cx)
                .then(|| SharedString::from(tr(cx, "标题不能为空。"))),
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "描述"),
            area_input(&read.desc, 3, 8, cx),
            None,
            None,
            cx,
        ))
        .child(form_row(
            [
                form_field(
                    tr(cx, "开始日期"),
                    line_input(&read.start, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "截止日期"),
                    line_input(&read.due, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_row(
            [
                form_field(
                    tr(cx, "预估工时（小时）"),
                    line_input(&read.estimate, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "实际工时（小时）"),
                    line_input(&read.actual, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_field(
            tr(cx, "进度"),
            progress_step_control(form, read.progress, cx),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "阻塞原因"),
            line_input(&read.block, cx),
            None,
            Some(SharedString::from(tr(
                cx,
                "状态为「阻塞」时显示在卡片与右栏。",
            ))),
            cx,
        ));

    // ---- 右列：项目 / 状态 / 优先级 / 四象限 / 标签 / 备注 ----
    let project_options: Vec<(SharedString, SharedString)> = std::iter::once((
        SharedString::from("none"),
        SharedString::from(tr(cx, "未归入项目")),
    ))
    .chain(read.projects.iter().map(|project| {
        (
            SharedString::from(project.id.to_string()),
            SharedString::from(project.name.clone()),
        )
    }))
    .collect();
    let current_project = SharedString::from(
        read.project
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_owned()),
    );
    let form_for_project = form.clone();
    let project_control = dropdown_select(
        "projects-task-editor-project",
        project_options,
        current_project,
        move |value, _, cx| {
            let project = if value.as_ref() == "none" {
                None
            } else {
                value.as_ref().parse::<i64>().ok()
            };
            form_for_project.update(cx, |form, cx| {
                form.project = project;
                cx.notify();
            });
        },
        cx,
    );

    let status_options: Vec<(SharedString, SharedString)> = TaskStatus::ALL
        .iter()
        .map(|status| {
            (
                SharedString::from(status.as_str()),
                SharedString::from(status_label(*status, cx)),
            )
        })
        .collect();
    let form_for_status = form.clone();
    let status_control = dropdown_select(
        "projects-task-editor-status",
        status_options,
        SharedString::from(read.status.as_str()),
        move |value, _, cx| {
            if let Some(status) = TaskStatus::parse(value.as_ref()) {
                form_for_status.update(cx, |form, cx| {
                    form.status = status;
                    cx.notify();
                });
            }
        },
        cx,
    );

    let priority_options: Vec<(SharedString, SharedString)> = Priority::ALL
        .iter()
        .map(|priority| {
            let key = SharedString::from(format!("P{}", priority.as_i64()));
            (key.clone(), key)
        })
        .collect();
    let form_for_priority = form.clone();
    let priority_control = dropdown_select(
        "projects-task-editor-priority",
        priority_options,
        SharedString::from(format!("P{}", read.priority.as_i64())),
        move |value, _, cx| {
            if let Some(priority) = Priority::parse(value.as_ref()) {
                form_for_priority.update(cx, |form, cx| {
                    form.priority = priority;
                    cx.notify();
                });
            }
        },
        cx,
    );

    let quadrant_options: Vec<(SharedString, SharedString)> = Quadrant::ALL
        .iter()
        .map(|quadrant| {
            (
                SharedString::from(quadrant.as_str()),
                SharedString::from(quadrant_short_label(*quadrant, cx)),
            )
        })
        .collect();
    let form_for_quadrant = form.clone();
    let quadrant_control = dropdown_select(
        "projects-task-editor-quadrant",
        quadrant_options,
        SharedString::from(read.quadrant.as_str()),
        move |value, _, cx| {
            if let Some(quadrant) = Quadrant::parse(value.as_ref()) {
                form_for_quadrant.update(cx, |form, cx| {
                    form.quadrant = quadrant;
                    cx.notify();
                });
            }
        },
        cx,
    );

    let right = v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.sm)
        .child(form_field(
            tr(cx, "所属项目"),
            project_control,
            None,
            None,
            cx,
        ))
        .child(form_field(tr(cx, "状态"), status_control, None, None, cx))
        .child(form_field(
            tr(cx, "优先级"),
            priority_control,
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "四象限"),
            quadrant_control,
            None,
            None,
            cx,
        ))
        .child(tag_picker(
            form,
            read.tags.as_slice(),
            &read.available_tags,
            cx,
        ))
        .child(form_field(
            tr(cx, "备注"),
            area_input(&read.note, 2, 5, cx),
            None,
            None,
            cx,
        ));

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-task-editor-title",
            drag,
            SharedString::from(heading),
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(tokens.spacing.md)
                .child(left)
                .child(right),
        )
}

/// 进度步进控件：− / 百分比 / +。
fn progress_step_control(
    form: &Entity<TaskEditorForm>,
    progress: u8,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let minus = form.clone();
    let plus = form.clone();
    h_flex()
        .gap(tokens.spacing.xs)
        .child(
            button(
                "projects-task-progress-minus",
                "−",
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                minus.update(cx, |form, cx| form.step_progress(-10, cx));
            }),
        )
        .child(
            div()
                .min_w(px(52.))
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                // `gpui` 的 `Div` 没有 `text_align_center`；百分比读数要居中，
                // 用一个占满宽度的 flex 容器把文字推到中间（比手算 padding 稳）。
                .flex()
                .justify_center()
                .child(SharedString::from(trf(cx, "{}%", &[&progress.to_string()]))),
        )
        .child(
            button(
                "projects-task-progress-plus",
                "+",
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                plus.update(cx, |form, cx| form.step_progress(10, cx));
            }),
        )
}

/// 标签多选片：选中的用标签本色，未选中的中性描边。
fn tag_picker(
    form: &Entity<TaskEditorForm>,
    selected: &[i64],
    available: &[Tag],
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut row = h_flex().w_full().flex_wrap().gap(tokens.spacing.xxs);
    if available.is_empty() {
        row = row.child(duck_components::body_text(
            tr(cx, "还没有标签，可在左栏「管理标签」里新建。"),
            cx,
        ));
        return row;
    }
    for tag in available {
        let picked = selected.contains(&tag.id);
        let color = color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground);
        let form_for_tag = form.clone();
        let id = tag.id;
        row = row.child(
            h_flex()
                .id(SharedString::from(format!("projects-task-editor-tag-{id}")))
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                .rounded_full()
                .border(theme.extended().stroke.thin)
                .border_color(if picked {
                    color
                } else {
                    theme.extended().colors.hairline
                })
                .bg(if picked {
                    color.opacity(theme.extended().opacity.tint)
                } else {
                    tokens.colors.surface
                })
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if picked {
                    color
                } else {
                    tokens.colors.muted_foreground
                })
                .cursor_pointer()
                .hover(move |style| style.bg(theme.extended().colors.hover_tint))
                .role(Role::Button)
                .aria_label(SharedString::from(tag.name.clone()))
                .child(SharedString::from(tag.name.clone()))
                .on_click(move |_, _, cx| {
                    form_for_tag.update(cx, |form, cx| form.toggle_tag(id, cx));
                }),
        );
    }
    row
}

/// 四象限的短名（弹窗下拉里用；卡片徽标只给 `Q1` 字面量）。
fn quadrant_short_label(quadrant: Quadrant, cx: &App) -> &'static str {
    match quadrant {
        Quadrant::Q1 => tr(cx, "Q1 重要且紧急"),
        Quadrant::Q2 => tr(cx, "Q2 重要不紧急"),
        Quadrant::Q3 => tr(cx, "Q3 紧急不重要"),
        Quadrant::Q4 => tr(cx, "Q4 都不紧急"),
    }
}

// ---------------------------------------------------------------------------
// 项目编辑弹窗（原型 openProjectEditor）
// ---------------------------------------------------------------------------

/// 项目编辑表单。
pub(crate) struct ProjectEditorForm {
    /// `Some(id)` = 编辑；`None` = 新建。
    pub(crate) project_id: Option<i64>,
    name: Entity<InputState>,
    desc: Entity<TextareaState>,
    start: Entity<InputState>,
    due: Entity<InputState>,
    color: usize,
}

impl ProjectEditorForm {
    /// 「保存」那一刻读出的字段。
    fn collect(&self, cx: &App) -> CollectedProject {
        CollectedProject {
            name: input_value(&self.name, cx),
            description: textarea_value(&self.desc, cx),
            start: parse_date_input(&self.start, cx),
            due: parse_date_input(&self.due, cx),
            color: PROJECT_COLORS
                .get(self.color)
                .copied()
                .unwrap_or(PROJECT_COLORS[0])
                .to_owned(),
        }
    }
}

/// [`ProjectEditorForm::collect`] 的结果。
pub(crate) struct CollectedProject {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) start: Option<CalendarDate>,
    pub(crate) due: Option<CalendarDate>,
    pub(crate) color: String,
}

impl CollectedProject {
    /// 转成内核的新建输入。
    pub(crate) fn into_new_project(self) -> NewProject {
        NewProject::new(self.name)
            .description(self.description)
            .color(self.color)
            .dates(self.start, self.due)
    }

    /// 转成内核的局部更新（编辑模式：整表单覆盖）。
    ///
    /// 日期用 `dates` 而不是「有值才设」：表单里清空日期就是要清空 —— 用
    /// `Option::Some(None)` 的写法交给内核，才不会被当成「没填就不改」。
    pub(crate) fn into_patch(self) -> ProjectPatch {
        ProjectPatch::new()
            .name(self.name)
            .description(self.description)
            .color(self.color)
            .dates(self.start, self.due)
    }
}

/// 打开项目编辑弹窗。`project_id` 为 `None` 时是「新建项目」。
pub(crate) fn open_project_editor(
    page: &mut ProjectsPage,
    project_id: Option<i64>,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let existing = {
        let state = page.state.read(cx);
        project_id.and_then(|id| state.projects().iter().find(|p| p.id == id).cloned())
    };
    let default_color = existing
        .as_ref()
        .map(|project| project.color.clone())
        .unwrap_or_else(|| PROJECT_COLORS[0].to_owned());

    let form = cx.new(|cx| {
        let form = ProjectEditorForm {
            project_id,
            name: new_input(window, cx, "例如：Alpha 网关重构"),
            desc: new_area(window, cx, "这个项目要达成什么"),
            start: new_input(window, cx, "YYYY-MM-DD"),
            due: new_input(window, cx, "YYYY-MM-DD"),
            color: PROJECT_COLORS
                .iter()
                .position(|candidate| *candidate == default_color)
                .unwrap_or(0),
        };
        if let Some(project) = &existing {
            form.name.update(cx, |input, cx| {
                input.set_value(SharedString::from(project.name.clone()), window, cx)
            });
            form.desc.update(cx, |input, cx| {
                input.set_value(SharedString::from(project.description.clone()), window, cx)
            });
            if let Some(date) = project.start_date {
                form.start.update(cx, |input, cx| {
                    input.set_value(SharedString::from(date.to_iso()), window, cx)
                });
            }
            if let Some(date) = project.due_date {
                form.due.update(cx, |input, cx| {
                    input.set_value(SharedString::from(date.to_iso()), window, cx)
                });
            }
        }
        form
    });

    let heading = match project_id {
        Some(_) => tr(cx, "项目设置"),
        None => tr(cx, "新建项目"),
    };
    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();
        let view_for_delete = view.clone();
        let heading_for_content = heading.clone();

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(if project_id.is_some() {
                        SharedString::from(tr(cx, "保存"))
                    } else {
                        SharedString::from(tr(cx, "创建"))
                    })
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    project_editor_body(
                        &heading_for_content,
                        &drag_for_content,
                        &form_for_content,
                        project_id,
                        &view_for_delete,
                        cx,
                    ),
                ))
            })
            .on_ok(move |_, _, cx| {
                let collected = form_for_ok.read(cx).collect(cx);
                if collected.name.is_empty() {
                    return false;
                }
                let project_id = form_for_ok.read(cx).project_id;
                let _ = view_for_ok.update(cx, |page, cx| {
                    page.submit_project_editor(project_id, collected, cx)
                });
                true
            })
    });
}

/// 项目编辑弹窗正文。
fn project_editor_body(
    heading: &str,
    drag: &Entity<duck_components::ModalDrag>,
    form: &Entity<ProjectEditorForm>,
    project_id: Option<i64>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);

    // 色板：8 个圆点，选中的加描边。
    let mut swatches = h_flex().w_full().flex_wrap().gap(tokens.spacing.sm);
    for (index, candidate) in PROJECT_COLORS.iter().enumerate() {
        let color = color_from_hex(candidate).unwrap_or(tokens.colors.muted_foreground);
        let picked = read.color == index;
        // 每一轮都要**新克隆一份** `Entity`：`on_click` 是 `move` 闭包，
        // 循环里直接捕获外层的那个会被第一次迭代移走（E0382）。
        let form_for_color = form.clone();
        swatches = swatches.child(
            div()
                .id(SharedString::from(format!(
                    "projects-project-color-{index}"
                )))
                .size(tokens.spacing.lg)
                .rounded_full()
                .bg(color)
                .border(theme.extended().stroke.thin)
                .border_color(if picked {
                    tokens.colors.foreground
                } else {
                    theme.extended().colors.hairline
                })
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(SharedString::from(*candidate))
                .on_click(move |_, _, cx| {
                    form_for_color.update(cx, |form, cx| {
                        form.color = index;
                        cx.notify();
                    });
                }),
        );
    }

    let mut body = v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-project-editor-title",
            drag,
            SharedString::from(heading),
            cx,
        ))
        .child(form_field(
            tr(cx, "名称"),
            line_input(&read.name, cx),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "一句话说明"),
            area_input(&read.desc, 2, 4, cx),
            None,
            None,
            cx,
        ))
        .child(form_field(tr(cx, "颜色"), swatches, None, None, cx))
        .child(form_row(
            [
                form_field(
                    tr(cx, "开始日期"),
                    line_input(&read.start, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "目标完成日"),
                    line_input(&read.due, cx),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ));

    // 编辑模式才有「删除项目」：不可逆动作另走二次确认（页面 `confirm_delete_project`）。
    if let Some(id) = project_id {
        let view_for_delete = view.clone();
        body = body.child(
            button(
                "projects-project-editor-delete",
                tr(cx, "删除项目"),
                ButtonVariant::Destructive,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = view_for_delete
                    .update(cx, |page, cx| page.confirm_delete_project(id, window, cx));
            }),
        );
    }
    body
}

// ---------------------------------------------------------------------------
// 标签管理弹窗（原型 openTagManager）
// ---------------------------------------------------------------------------

/// 打开标签管理弹窗：清单（换色 / 删除就地生效）+ 底部新建输入。
pub(crate) fn open_tag_manager(
    page: &mut ProjectsPage,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let new_name = new_input(window, cx, "新标签名");
    let form = cx.new(|_| TagManagerForm { new_name });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_add = form.clone();
        let view_for_content = view.clone();
        let view_for_add = view.clone();

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "完成"))
                    .cancel_text(tr(cx, "关闭"))
                    .show_cancel(false),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    tag_manager_body(&form_for_content, &drag_for_content, &view_for_content, cx),
                ))
            })
            .on_ok(move |_, _, cx| {
                let name = input_value(&form_for_add.read(cx).new_name, cx);
                if !name.is_empty() {
                    let _ = view_for_add.update(cx, |page, cx| page.create_tag(&name, cx));
                }
                true
            })
    });
}

struct TagManagerForm {
    new_name: Entity<InputState>,
}

fn tag_manager_body(
    form: &Entity<TagManagerForm>,
    drag: &Entity<duck_components::ModalDrag>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    // 清单**现读**：删除 / 换色后下一帧就是新清单（理由见模块头）。
    let tags: Vec<Tag> = view
        .read_with(cx, |page, cx| page.state.read(cx).tags().to_vec())
        .unwrap_or_default();

    let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
    if tags.is_empty() {
        list = list.child(duck_components::body_text(tr(cx, "还没有标签。"), cx));
    }
    for tag in &tags {
        let color = color_from_hex(&tag.color).unwrap_or(tokens.colors.muted_foreground);
        let view_for_cycle = view.clone();
        let view_for_delete = view.clone();
        let tag_id = tag.id;
        list = list.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                // 色点点击 = 换下一个色板色（就地保存）。
                .child(
                    div()
                        .id(SharedString::from(format!("projects-tag-color-{tag_id}")))
                        .size(tokens.spacing.sm)
                        .rounded_full()
                        .bg(color)
                        .border(theme.extended().stroke.thin)
                        .border_color(theme.extended().colors.hairline)
                        .cursor_pointer()
                        .role(Role::Button)
                        .aria_label(tr(cx, "换一个颜色"))
                        .on_click(move |_, _, cx| {
                            let _ = view_for_cycle
                                .update(cx, |page, cx| page.cycle_tag_color(tag_id, cx));
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .child(SharedString::from(tag.name.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(theme.extended().colors.text_tertiary)
                        .child(SharedString::from(trf(
                            cx,
                            "{} 项任务",
                            &[&tag.usage_count.to_string()],
                        ))),
                )
                .child(
                    button(
                        SharedString::from(format!("projects-tag-delete-{tag_id}")),
                        tr(cx, "删除"),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view_for_delete.update(cx, |page, cx| page.delete_tag(tag_id, cx));
                    }),
                ),
        );
    }

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-tag-manager-title",
            drag,
            tr(cx, "管理标签"),
            cx,
        ))
        .child(list)
        .child(form_field(
            tr(cx, "新建标签"),
            line_input(&form.read(cx).new_name, cx),
            None,
            Some(SharedString::from(tr(
                cx,
                "按「完成」创建；颜色取自项目色板。",
            ))),
            cx,
        ))
}

// ---------------------------------------------------------------------------
// 看板列设置弹窗（原型 openColumnSettings）
// ---------------------------------------------------------------------------

/// 打开列设置弹窗：每行（列名 + WIP），「新增一列」补一个还没有列的状态。
pub(crate) fn open_column_settings(
    page: &mut ProjectsPage,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let columns: Vec<BoardColumn> = page.state.read(cx).columns().to_vec();
    let mut rows: Vec<ColumnRow> = Vec::new();
    for column in &columns {
        // 先把占位文案取出来：`new_input` 要 `&mut cx`，而 `status_label` 要 `&cx`
        // —— 同一个表达式里交错会撞 `E0502`。
        let placeholder = status_label(column.status_key, cx);
        let name = new_input(window, cx, placeholder);
        if let Some(name_text) = &column.name {
            name.update(cx, |input, cx| {
                input.set_value(SharedString::from(name_text.clone()), window, cx)
            });
        }
        let wip = new_input(window, cx, "不限");
        if let Some(limit) = column.wip_limit {
            wip.update(cx, |input, cx| {
                input.set_value(SharedString::from(limit.to_string()), window, cx)
            });
        }
        rows.push(ColumnRow {
            status_key: column.status_key,
            name,
            wip,
        });
    }
    let form = cx.new(|_| ColumnSettingsForm {
        rows,
        extra_status: None,
        extra_name: None,
    });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "保存"))
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    column_settings_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .on_ok(move |_, _, cx| {
                let specs = form_for_ok.read(cx).collect(cx);
                let _ = view_for_ok.update(cx, |page, cx| page.submit_columns(specs, cx));
                true
            })
    });
}

struct ColumnRow {
    status_key: TaskStatus,
    name: Entity<InputState>,
    wip: Entity<InputState>,
}

struct ColumnSettingsForm {
    rows: Vec<ColumnRow>,
    /// 「新增一列」选中的状态；`None` = 不加。一次保存最多补一列（一个状态一列
    /// 是本页看板的既有口径，多列同状态需要拖拽排序的完整设计，不在这里做）。
    extra_status: Option<TaskStatus>,
    extra_name: Option<Entity<InputState>>,
}

impl ColumnSettingsForm {
    fn collect(&self, cx: &App) -> Vec<ColumnSpec> {
        let mut specs: Vec<ColumnSpec> = Vec::new();
        for row in &self.rows {
            let mut spec = ColumnSpec::new(row.status_key);
            let name = input_value(&row.name, cx);
            if !name.is_empty() {
                spec = spec.name(name);
            }
            if let Some(limit) = input_value(&row.wip, cx)
                .parse::<i64>()
                .ok()
                .filter(|limit| *limit > 0)
            {
                spec = spec.wip_limit(limit);
            }
            specs.push(spec);
        }
        if let Some(status) = self.extra_status {
            let mut spec = ColumnSpec::new(status);
            if let Some(name) = &self.extra_name {
                let name = input_value(name, cx);
                if !name.is_empty() {
                    spec = spec.name(name);
                }
            }
            specs.push(spec);
        }
        specs
    }
}

fn column_settings_body(
    form: &Entity<ColumnSettingsForm>,
    drag: &Entity<duck_components::ModalDrag>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);

    let mut list = v_flex().w_full().gap(tokens.spacing.sm);
    for row in &read.rows {
        list = list.child(form_row(
            [
                form_field(
                    SharedString::from(status_label(row.status_key, cx)),
                    line_input(&row.name, cx),
                    None,
                    Some(SharedString::from(tr(cx, "留空 = 用默认列名"))),
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "WIP 上限"),
                    line_input(&row.wip, cx),
                    None,
                    Some(SharedString::from(tr(cx, "留空 = 不限"))),
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ));
    }

    // 「新增一列」：把一个还没有列的状态补上。
    let missing: Vec<TaskStatus> = TaskStatus::ALL
        .iter()
        .filter(|status| !read.rows.iter().any(|row| row.status_key == **status))
        .copied()
        .collect();
    if !missing.is_empty() {
        let form_for_toggle = form.clone();
        let extra_label = match read.extra_status {
            Some(status) => {
                SharedString::from(trf(cx, "将新增「{}」列", &[status_label(status, cx)]))
            }
            None => SharedString::from(tr(cx, "新增一列")),
        };
        list = list.child(
            button(
                "projects-column-add",
                extra_label,
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                form_for_toggle.update(cx, |form, cx| {
                    form.extra_status = match form.extra_status {
                        Some(_) => None,
                        None => {
                            // 取第一个还没有列的状态作为默认新列。
                            TaskStatus::ALL
                                .iter()
                                .find(|status| {
                                    !form.rows.iter().any(|row| row.status_key == **status)
                                })
                                .copied()
                        }
                    };
                    cx.notify();
                });
            }),
        );
    }

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-column-settings-title",
            drag,
            tr(cx, "看板列设置"),
            cx,
        ))
        .child(list)
        .child(field_hint(tr(cx, "改动在按「保存」后整体生效。"), cx))
}

// ---------------------------------------------------------------------------
// 快速新建 Spotlight（原型 openSpotlight + parseQuick）
// ---------------------------------------------------------------------------

/// 打开快速新建弹窗：一句话 + 实时 chips 预览。
pub(crate) fn open_spotlight(
    page: &mut ProjectsPage,
    preset_project: Option<i64>,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let (projects, tags, today, scope_project) = {
        let state = page.state.read(cx);
        (
            state.projects().to_vec(),
            state.tags().to_vec(),
            state.today(),
            match state.scope() {
                super::state::BoardScope::Project(id) => Some(id),
                _ => None,
            },
        )
    };
    let input = new_input(window, cx, "修复登录超时 #后端 @Alpha !P0 明天 18:00");
    let form = cx.new(|_| SpotlightForm {
        input,
        projects,
        tags,
        today,
        preset_project: preset_project.or(scope_project),
    });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "创建"))
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    spotlight_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .on_ok(move |_, _, cx| {
                let text = input_value(&form_for_ok.read(cx).input, cx);
                if text.is_empty() {
                    return false;
                }
                let _ = view_for_ok.update(cx, |page, cx| page.submit_spotlight(&text, cx));
                true
            })
    });
}

struct SpotlightForm {
    input: Entity<InputState>,
    projects: Vec<Project>,
    tags: Vec<Tag>,
    today: CalendarDate,
    preset_project: Option<i64>,
}

fn spotlight_body(
    form: &Entity<SpotlightForm>,
    drag: &Entity<duck_components::ModalDrag>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);
    let text = input_value(&read.input, cx);
    let parsed = quickparse::parse(&text, read.today);

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-spotlight-title",
            drag,
            tr(cx, "快速新建"),
            cx,
        ))
        .child(line_input(&read.input, cx))
        .child(spotlight_preview(
            &parsed,
            &read.projects,
            &read.tags,
            read.preset_project,
            cx,
        ))
}

/// 解析结果的 chips 预览（原型 `sp-res`）：一眼看出这句话会被拆成什么。
fn spotlight_preview(
    parsed: &QuickParse,
    projects: &[Project],
    tags: &[Tag],
    preset_project: Option<i64>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut row = h_flex().w_full().flex_wrap().gap(tokens.spacing.xxs);

    if !parsed.has_markers() && preset_project.is_none() && parsed.title.is_empty() {
        row = row.child(duck_components::body_text(
            tr(cx, "#标签 @项目 !P0 日期/今天/明天 HH:MM 都能识别。"),
            cx,
        ));
        return row;
    }

    if parsed.title.is_empty() {
        row = row.child(duck_components::body_text(
            tr(cx, "写一句标题，剩下的交给记号。"),
            cx,
        ));
    } else {
        row = row.child(neutral_chip(SharedString::from(parsed.title.clone()), cx));
    }
    if let Some(name) = &parsed.project {
        row = row.child(neutral_chip(
            SharedString::from(trf(cx, "@{}", &[name])),
            cx,
        ));
    } else if let Some(project) =
        preset_project.and_then(|id| projects.iter().find(|project| project.id == id))
    {
        row = row.child(neutral_chip(
            SharedString::from(trf(cx, "@{}", &[&project.name])),
            cx,
        ));
    }
    for name in &parsed.tags {
        let exists = tags.iter().any(|tag| &tag.name == name);
        let label = if exists {
            SharedString::from(trf(cx, "#{}", &[name]))
        } else {
            SharedString::from(trf(cx, "#{}（新建）", &[name]))
        };
        row = row.child(neutral_chip(label, cx));
    }
    if let Some(priority) = parsed.priority {
        row = row.child(neutral_chip(
            SharedString::from(trf(cx, "!P{}", &[&priority.as_i64().to_string()])),
            cx,
        ));
    }
    if let Some(due) = parsed.due {
        row = row.child(neutral_chip(SharedString::from(due.to_iso()), cx));
    }
    if let Some(time) = &parsed.due_time {
        row = row.child(neutral_chip(SharedString::from(time.clone()), cx));
    }
    row
}

fn neutral_chip(label: SharedString, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xxs)
        .rounded_full()
        .border(theme.extended().stroke.thin)
        .border_color(theme.extended().colors.hairline)
        .bg(tokens.colors.background)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.foreground)
        .child(label)
}

// ---------------------------------------------------------------------------
// 工作日志编辑弹窗
// ---------------------------------------------------------------------------

/// 打开某天（某一档）日志的编辑弹窗。
pub(crate) fn open_log_editor(
    page: &mut ProjectsPage,
    date: CalendarDate,
    kind: LogKind,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let existing = {
        let state = page.state.read(cx);
        state
            .log_of(date, kind)
            .map(|log| log.content_md.clone())
            .unwrap_or_default()
    };
    let body = cx.new(|cx| {
        // 直接构造 + 回填：`TextareaState` 是**状态本身**（不是 `Entity`），
        // 拿得到 `&mut` 就不必再绕一层 `Entity::update`。
        let mut state = TextareaState::new(window, cx);
        state.set_value(SharedString::from(existing), window, cx);
        state
    });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    let date_iso = date.to_iso();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let body_for_content = body.clone();
        let body_for_ok = body.clone();
        let view_for_ok = view.clone();
        let date_for_ok = date;
        let kind_for_ok = kind;
        let heading =
            SharedString::from(trf(cx, "{} 的{}", &[&date_iso, log_kind_label(kind, cx)]));

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr(cx, "保存"))
                    .cancel_text(tr(cx, "取消"))
                    .show_cancel(true),
            )
            .content(move |content, _, cx| {
                let theme = active_theme(cx);
                let tokens = theme.tokens();
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(modal_title_row(
                            "projects-log-editor-title",
                            &drag_for_content,
                            heading.clone(),
                            cx,
                        ))
                        .child(area_input(&body_for_content, 6, 14, cx)),
                ))
            })
            .on_ok(move |_, _, cx| {
                let text = textarea_value(&body_for_ok, cx);
                let _ = view_for_ok.update(cx, |page, cx| {
                    page.submit_log(date_for_ok, kind_for_ok, &text, cx)
                });
                true
            })
    });
}

/// 日志档位的中文名（弹窗标题与日志视图的 tab 共用一处真源）。
pub(crate) fn log_kind_label(kind: LogKind, cx: &App) -> &'static str {
    match kind {
        LogKind::Daily => tr(cx, "日报"),
        LogKind::Weekly => tr(cx, "周报"),
        LogKind::Plan => tr(cx, "周计划"),
    }
}
