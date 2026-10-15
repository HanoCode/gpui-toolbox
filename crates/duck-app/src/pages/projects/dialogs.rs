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
//! - 日期用 gpui-component 的 [`DatePicker`]（日历弹层选择，可一键清空）——0.7.1 起
//!   库里有现成控件，不再让用户手打 ISO 文本（原型是 `<input type="date">`，语义
//!   就是「选」不是「填」）。存的是 [`DatePickerState`]，读值见 [`picker_date`]；
//! - 工时是小时数文本（空 / 非法 = 不设）。**预估工时**在「开始 / 截止」两头都选好
//!   之后会按天自动填（见 [`HOURS_PER_WORKDAY`] 与 [`TaskEditorForm::auto_hours`]）；
//!   **实际工时**永远手填 —— 它记的是已经花掉的量，与排期无关；
//! - 四象限用 2×2 格子（原型 `.qgrid2`），名字 + 处置建议一眼可读，比下拉少点一次；
//! - 进度用滑块 + 百分比读数（下限 = 打开时的进度），写入口径与内核一致（越界钳 0–100）；
//! - 标签管理弹窗的清单**不拍快照**：删除 / 换色后要立刻反映，所以渲染时从页面
//!   状态现读（弹窗内容闭包每帧重跑，现读零成本）。

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, Toggled, WeakEntity, Window, div, px,
};
use gpui_component::{
    Icon, Sizable as _, WindowExt as _,
    calendar::Date as PickerDate,
    date_picker::{DatePicker, DatePickerEvent, DatePickerState},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    slider::{Slider, SliderEvent, SliderState},
    v_flex,
};

use duck_components::{
    ButtonVariant, ConfirmHandler, DuckIcon, FooterAction, button, caption_heading, dialog_footer,
    dialog_footer_with_action, dialog_footer_with_hint, dialog_top_offset, dropdown_select,
    field_hint, form_field, form_row, input_with_action, meta_text, modal_drag, modal_drag_area,
    modal_offset, modal_title_row, notify::Tone, segmented_tabs,
};
use duck_project::{
    BoardColumn, CalendarDate, ColumnSpec, LogKind, NewProject, NewTask, Priority, Project,
    ProjectPatch, Quadrant, Tag, Task, TaskPatch, TaskStatus,
};
use duck_theme::{active_theme, color_from_hex, tr, trf};

use super::ProjectsPage;
use super::export::{ExportFormat, ExportScope};
use super::quickparse::{self, ImportLine, QuickParse};
use super::state::ProjectsState;
use super::widgets::progress_bar::task_progress_bar;
use super::widgets::status_label;
use super::widgets::task_detail_panel::milestone_row;

/// 项目色板（原型 `PJCOLORS`）：8 个高区分度颜色，新建 / 编辑项目共用。
pub(crate) const PROJECT_COLORS: [&str; 8] = [
    "#3B82F6", "#8B5CF6", "#EC4899", "#EF4444", "#F59E0B", "#10B981", "#14B8A6", "#6B7280",
];

/// 任务编辑弹窗宽（双列大表单）。布局几何量，留裸值并写明来由。
///
/// 940 是设计文档 §4.3 ③ 给的 `min(940px, 100vw - 32px)` ——窗口更窄时按
/// [`TASK_EDITOR_GUTTER`] 两侧各留一点，避免弹窗贴边 / 出界。
const TASK_EDITOR_WIDTH: f32 = 940.;
/// 窄窗口下任务编辑弹窗左右各留的空白（原型 `100vw - 32px`）。
const TASK_EDITOR_GUTTER: f32 = 32.;
/// 双列比例（设计文档 §4.3 ③ 照参考项目 `TaskBoard.vue:1556-1584` 的
/// `minmax(0,1.12fr) minmax(340px,.88fr)`）：左列「核心信息」略宽。
const TASK_EDITOR_LEFT_GROW: f32 = 1.12;
const TASK_EDITOR_RIGHT_GROW: f32 = 0.88;
/// 其余单列表单的宽度（比确认框略宽，容下「标签 + 计数」一行）。
const FORM_WIDTH: f32 = 520.;

/// 一个工作日按几小时算 —— 「预估工时随日期自动算」的口径。
///
/// 只参与**预估工时**：`(截止 - 开始 + 1) × 8`，**含首尾两天**
/// （10-01 → 10-02 = 2 天 = 16 小时）。实际工时永远手填。
///
/// 这个数字同时也是那句说明文案里的参数（经 [`trf`] 传进去），不写死在文案里 ——
/// 改这个常量，界面上的「每天 N 小时」跟着变。
const HOURS_PER_WORKDAY: u64 = 8;

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

/// 日期选择器 → [`CalendarDate`]；没选（清空）就是「没排期」。
///
/// 上游 [`DatePicker`] 的公开类型全踩在 `chrono` 的 `NaiveDate` 系上，而本仓的
/// 依赖准入结论是**不引 `chrono` / `time`**（理由见 `duck-project/src/time.rs`：
/// 历法自己手写）。所以两边都经 ISO 文本中转：读走 [`PickerDate::format`]
/// （上游公开方法，chrono 只在它内部出现），解析用内核的 [`CalendarDate::parse`]。
///
/// 档位仍是「单日期」：范围档会格式化成 `a - b`，parse 失败即当没选 ——
/// 与「清空 = 清除」同一个口径。
fn picker_date(state: &Entity<DatePickerState>, cx: &App) -> Option<CalendarDate> {
    let text = state.read(cx).date().format("%Y-%m-%d")?;
    CalendarDate::parse(&text)
}

/// 造一个空的（未选日期）单日期选择器状态。
///
/// `DatePickerState::new` 的默认档位就是「单日期」；范围模式（`range`）本仓用不上。
fn date_picker_state(window: &mut Window, cx: &mut App) -> Entity<DatePickerState> {
    cx.new(|cx| DatePickerState::new(window, cx))
}

/// 「开始」到「截止」一共几天（**含首尾**）。
///
/// 内核的 [`CalendarDate::days_until`] 口径是「另一个日期 − 自己」，所以
/// `start.days_until(due)` 已经是正的天数**差**（`2026-10-14 → 10-18` 得 4），
/// 含首尾再 `+1`。
///
/// 只排了单边、或截止早于开始，都当「没有跨度」（`None`）—— 后者该由保存时的
/// 校验去报错，这里只保证不给出 0 / 负数这种没有意义的跨度。
fn workdays_between(start: Option<CalendarDate>, due: Option<CalendarDate>) -> Option<u64> {
    let span = start?.days_until(due?) + 1;
    (span > 0).then_some(span as u64)
}

/// 把既有日期回填进选择器（`None` = 保持未选）。
///
/// 写入方向同样不点名 `chrono`：`PickerDate::Single` 的字段类型是 `Option<NaiveDate>`，
/// `parse()` 的目标类型由这个变体反推即可，声明里不出现 chrono 路径。
fn set_picker_date(
    state: &Entity<DatePickerState>,
    date: Option<CalendarDate>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(date) = date else { return };
    let value = date
        .to_iso()
        .parse()
        .ok()
        .map(|day| PickerDate::Single(Some(day)));
    if let Some(value) = value {
        state.update(cx, |state, cx| state.set_date(value, window, cx));
    }
}

/// 日期选择控件（日历弹层）。
///
/// `small()` 贴单行输入的档位；`cleanable(true)` 让「清掉排期」有一个明确入口 ——
/// 没有它，选错了只能选别的日期顶替。
fn date_picker(state: &Entity<DatePickerState>) -> impl IntoElement {
    DatePicker::new(state).small().cleanable(true)
}

// ---------------------------------------------------------------------------
// 进度控件（滑块 + 条 + 读数）
// ---------------------------------------------------------------------------

/// 进度控件：进度条 + 滑块 + 百分比读数。
///
/// **下限 = 打开弹窗那一刻的进度**（原型 `<input type="range" min="t.prog">`，
/// `docs/projects-mock.html:1787`）：进度是「已完成多少」的累积量，拖动只该往前 ——
/// 允许回退的话，改别的字段时手一滑就把进度抹回去了，而弹窗里没有撤销。
///
/// **为什么单独做成一个视图实体**：读数要跟着拖动**实时**变，而弹窗 `content`
/// 闭包不会因为滑块自己 `notify()` 而重跑（`duck-components::dialog` 的弹窗约定，
/// 同「正文要边打边过滤就得挂独立视图」那条）。这一小块自己订阅滑块、自己
/// `notify()`，与外面那层谁重绘无关。
struct TaskProgressSlider {
    slider: Entity<SliderState>,
    /// 订阅句柄：丢了这个 `Subscription`，拖动就只剩滑块自己在动、读数不动。
    _change: Subscription,
}

impl TaskProgressSlider {
    fn new(floor: u8, value: u8, cx: &mut Context<Self>) -> Self {
        let slider = cx.new(|_| {
            SliderState::new()
                .min(f32::from(floor))
                .max(100.)
                .step(1.)
                .default_value(f32::from(value))
        });
        let change = cx.subscribe(&slider, |_, _, _: &SliderEvent, cx| cx.notify());
        Self {
            slider,
            _change: change,
        }
    }

    /// 当前进度（0–100）。读数与落库都走这一个来源，免得两份状态各说各话。
    fn progress(&self, cx: &App) -> u8 {
        self.slider.read(cx).value().start().round().clamp(0., 100.) as u8
    }
}

impl Render for TaskProgressSlider {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = active_theme(cx).tokens();
        let progress = self.progress(cx);
        v_flex()
            .w_full()
            .gap(tokens.spacing.sm)
            .child(task_progress_bar(
                "projects-task-editor-progress-bar",
                progress,
                cx,
            ))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Slider::new(&self.slider).horizontal()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(38.))
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(format!("{progress}%"))),
                    ),
            )
    }
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
    start: Entity<DatePickerState>,
    due: Entity<DatePickerState>,
    estimate: Entity<InputState>,
    actual: Entity<InputState>,
    block: Entity<InputState>,
    note: Entity<TextareaState>,
    project: Option<i64>,
    /// 父任务（内核只许一层嵌套：候选清单见 [`TaskEditorForm::parent_candidates`]）。
    parent: Option<i64>,
    status: TaskStatus,
    priority: Priority,
    quadrant: Quadrant,
    /// 进度控件（滑块 + 读数）；下限 = 打开时的进度。
    progress: Entity<TaskProgressSlider>,
    tags: Vec<i64>,
    /// 打开那一刻的项目 / 标签 / 父任务候选清单（弹窗生命周期内不会改它们，不用订阅）。
    projects: Vec<Project>,
    available_tags: Vec<Tag>,
    /// 可选的父任务：**顶层任务**（内核 `check_parent` 只许一层，父自己不能再有父），
    /// 编辑态还要把任务自身去掉（不能当自己的父）。
    parent_candidates: Vec<Task>,
    /// 预估工时当前是不是「按日期自动算出来的」。
    ///
    /// - `true`：开始 / 截止任一变化就跟着重算（用户还没表达自己的意见）；
    /// - `false`：用户手填过，或编辑既有任务时带进来的旧值 —— 日期再改也不覆盖，
    ///   想拉回自动只能点「按日期重算」。
    ///
    /// 靠 `InputEvent::Change` 分辨「谁写的」：`InputState::set_value` **不**发这个
    /// 事件（gpui-base 的既有行为，`gpui-component` 自己也依赖它），所以程序化回填
    /// 不会把自己误标成「用户手填」。
    estimate_auto: bool,
    /// 日期选择器 / 工时输入框的订阅句柄；丢一个就断一条联动。
    _subs: Vec<Subscription>,
}

/// 打开弹窗那一刻取好的三份清单。
///
/// 打包成一个结构体而不是三个参数：`TaskEditorForm::new` 已经要收
/// 「预置项目 / 预置状态 / 初始进度」三件开门参数，再摊开三份清单就过了 clippy 的
/// 7 参线。三者的生命周期也一样（开门时快照、弹窗内不再订阅）。
struct EditorCatalogs {
    projects: Vec<Project>,
    available_tags: Vec<Tag>,
    parent_candidates: Vec<Task>,
}

impl TaskEditorForm {
    /// 新建模式。
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        preset_project: Option<i64>,
        preset_status: TaskStatus,
        progress: u8,
        catalogs: EditorCatalogs,
    ) -> Self {
        let mut form = Self {
            task_id: None,
            title: new_input(window, cx, "一句话说清要做什么"),
            desc: new_area(window, cx, "背景、验收标准、参考实现…"),
            start: date_picker_state(window, cx),
            due: date_picker_state(window, cx),
            estimate: new_input(window, cx, "0"),
            actual: new_input(window, cx, "0"),
            block: new_input(window, cx, "例如：等上游开权限"),
            note: new_area(window, cx, "只给自己看的提示…"),
            project: preset_project,
            parent: None,
            status: preset_status,
            priority: Priority::P2,
            quadrant: Quadrant::Q2,
            // 新建任务的进度下限就是 0：没有「已经完成的量」可回退。
            progress: cx.new(|cx| TaskProgressSlider::new(progress, progress, cx)),
            tags: Vec::new(),
            projects: catalogs.projects,
            available_tags: catalogs.available_tags,
            parent_candidates: catalogs.parent_candidates,
            // 空输入框 = 「听日期的话」，所以新建态就是自动态。
            estimate_auto: true,
            _subs: Vec::new(),
        };
        form.watch_dates(window, cx);
        form.watch_estimate(cx);
        form
    }

    /// 编辑模式：从既有任务回填。
    fn from_task(
        window: &mut Window,
        cx: &mut Context<Self>,
        task: &Task,
        catalogs: EditorCatalogs,
    ) -> Self {
        let mut form = Self::new(
            window,
            cx,
            task.project_id,
            task.status,
            task.progress,
            catalogs,
        );
        form.task_id = Some(task.id);
        form.priority = task.priority;
        form.quadrant = task.quadrant;
        form.parent = task.parent_id;
        form.tags = task.tags.iter().map(|tag| tag.id).collect();
        fill_input(&form.title, &task.title, window, cx);
        fill_area(&form.desc, &task.description_md, window, cx);
        set_picker_date(&form.start, task.start_date, window, cx);
        set_picker_date(&form.due, task.due_date, window, cx);
        if let Some(hours) = task.estimate_hours {
            fill_input(&form.estimate, &hours.to_string(), window, cx);
            // 带进来的旧工时算「用户自己的值」：日期再改不该悄悄把它算掉，
            // 想按新日期刷新得点「按日期重算」。
            form.estimate_auto = false;
        }
        if let Some(hours) = task.actual_hours {
            fill_input(&form.actual, &hours.to_string(), window, cx);
        }
        fill_input(&form.block, &task.block_reason, window, cx);
        fill_area(&form.note, &task.note, window, cx);
        // 两头都排了期、又没填过工时的任务：打开就按日期补一个（与新建态同一口径）。
        form.refresh_estimate(window, cx);
        form
    }

    /// 订阅「开始 / 截止」两个日期选择器：任一变化就按跨度重算预估工时。
    ///
    /// 用 `subscribe_in` 而不是 `subscribe` —— 回填输入框要 `&mut Window`
    /// （`InputState::set_value` 的签名），而普通订阅的回调不给窗口。
    fn watch_dates(&mut self, window: &Window, cx: &mut Context<Self>) {
        let pickers = [self.start.clone(), self.due.clone()];
        for picker in pickers {
            let sub = cx.subscribe_in(
                &picker,
                window,
                |form: &mut Self,
                 _: &Entity<DatePickerState>,
                 _: &DatePickerEvent,
                 window: &mut Window,
                 cx: &mut Context<Self>| {
                    form.refresh_estimate(window, cx);
                },
            );
            self._subs.push(sub);
        }
    }

    /// 订阅预估工时输入框：用户一动它就把「自动」标记摘掉。
    ///
    /// 之后日期再改也不会覆盖这个值 —— 这正是「手填优先」那条口径。程序化回填
    /// （`fill_input` 走 `set_value`）不发 `Change`，所以自动写进去的值不会把
    /// 自己误标成手填。
    ///
    /// 这里用 `subscribe`（第二个参数是**按值**的 `Entity<T2>`）而不是上面日期用的
    /// `subscribe_in`（给引用、且多一个 `Window`）：改标记用不上窗口。
    fn watch_estimate(&mut self, cx: &mut Context<Self>) {
        let sub = cx.subscribe(
            &self.estimate,
            |form: &mut Self, _: Entity<InputState>, event: &InputEvent, _: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    form.estimate_auto = false;
                }
            },
        );
        self._subs.push(sub);
    }

    /// 按日期跨度算出的预估工时（`开始 … 截止`，**含首尾**，每天
    /// [`HOURS_PER_WORKDAY`] 小时）。没排全期就没有跨度可算，返回 `None`。
    fn auto_hours(&self, cx: &App) -> Option<u64> {
        let days = workdays_between(picker_date(&self.start, cx), picker_date(&self.due, cx))?;
        Some(days * HOURS_PER_WORKDAY)
    }

    /// 按日期跨度刷新预估工时；返回是否真的写了。
    ///
    /// **只在「输入框为空」或「当前值就是自动算出来的」时才写** —— 用户手填过的值
    /// 不会被悄悄抹掉（要覆盖得点「按日期重算」）。日期没排全时什么也不做。
    fn refresh_estimate(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if !self.estimate_auto && !input_value(&self.estimate, cx).is_empty() {
            return false;
        }
        let Some(hours) = self.auto_hours(cx) else {
            return false;
        };
        let hours = hours.to_string();
        fill_input(&self.estimate, &hours, window, cx);
        self.estimate_auto = true;
        true
    }

    /// 「按日期重算」：无视「用户手填过」的标记，强制用日期跨度覆盖预估工时。
    /// 日期没排全时无动作（按钮的说明文字已写明这个前提）。
    fn recalc_estimate(&mut self, window: &mut Window, cx: &mut App) {
        self.estimate_auto = true;
        self.refresh_estimate(window, cx);
    }

    /// 标题是否为空（空标题不给保存 —— 与内核 `require_non_empty` 同口径）。
    fn title_is_empty(&self, cx: &App) -> bool {
        input_value(&self.title, cx).is_empty()
    }

    /// 当前进度（0–100）——来自滑块，落库与「里程碑默认快照」共用。
    fn progress(&self, cx: &App) -> u8 {
        self.progress.read(cx).progress(cx)
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
            start: picker_date(&self.start, cx),
            due: picker_date(&self.due, cx),
            estimate: parse_hours_input(&self.estimate, cx),
            actual: parse_hours_input(&self.actual, cx),
            block: input_value(&self.block, cx),
            note: textarea_value(&self.note, cx),
            project: self.project,
            parent: self.parent,
            status: self.status,
            priority: self.priority,
            quadrant: self.quadrant,
            progress: self.progress(cx),
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
    pub(crate) parent: Option<i64>,
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
        if let Some(parent_id) = self.parent {
            new = new.parent(parent_id);
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
        // 父任务同理：下拉选中了就给，选「顶层任务」就是**明确摘掉**父任务
        // （`clear_parent`），不能当成「不动」——不然改不回顶层。
        patch = match self.parent {
            Some(parent_id) => patch.parent(parent_id),
            None => patch.clear_parent(),
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
    let (existing, projects, available_tags, parent_candidates) = {
        let state = page.state.read(cx);
        // 父任务候选 = **顶层任务**（内核 `check_parent` 只许一层：父自己不能再有父），
        // 编辑态再去掉任务自身（不能当自己的父）。取错这一层会被内核拒掉，
        // 所以候选清单在界面侧就按同一条口径筛，而不是等保存失败。
        let candidates = state
            .tasks()
            .iter()
            .filter(|task| task.parent_id.is_none() && Some(task.id) != task_id)
            .cloned()
            .collect();
        (
            task_id.and_then(|id| state.tasks().iter().find(|task| task.id == id).cloned()),
            state.projects().to_vec(),
            state.tags().to_vec(),
            candidates,
        )
    };
    let catalogs = EditorCatalogs {
        projects,
        available_tags,
        parent_candidates,
    };
    let form = match &existing {
        Some(task) => cx.new(|cx| TaskEditorForm::from_task(window, cx, task, catalogs)),
        None => {
            cx.new(|cx| TaskEditorForm::new(window, cx, preset_project, preset_status, 0, catalogs))
        }
    };

    let heading = match task_id {
        Some(_) => tr(cx, "编辑任务"),
        None => tr(cx, "新建任务"),
    };
    // 确认按钮文案随模式分派（原型：新建「创建任务」/ 编辑「保存修改」）。
    let ok_text = match task_id {
        Some(_) => tr(cx, "保存修改"),
        None => tr(cx, "创建任务"),
    };
    let drag = modal_drag(cx);
    let view = page.view.clone();
    let state = page.state.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();
        let view_for_content = view.clone();
        let state_for_content = state.clone();
        let heading_for_content = heading;

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
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
        });
        let footer = match task_id {
            // 编辑态：左「删除任务」（点击关本窗、进二次确认），右「取消 / 保存修改」。
            Some(id) => {
                let view_for_delete = view.clone();
                dialog_footer_with_action(
                    "projects-task-editor",
                    FooterAction {
                        id: SharedString::from("projects-task-editor-delete"),
                        text: SharedString::from(tr(cx, "删除任务")),
                        on_click: std::rc::Rc::new(move |window, cx| {
                            window.close_dialog(cx);
                            let _ = view_for_delete
                                .update(cx, |page, cx| page.confirm_delete_task(id, window, cx));
                        }),
                    },
                    ok_text,
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                )
            }
            // 新建态：左侧给一句「按 ⏎ 直接保存」（原型那句写的是 ⌘↵，而本仓弹窗
            // 的提交键就是 ⏎ —— `gpui-base/src/dialog.rs:92`，照原型写会指错键）。
            None => dialog_footer_with_hint(
                "projects-task-editor",
                tr(cx, "按 ⏎ 直接保存"),
                ok_text,
                ButtonVariant::Primary,
                confirm,
                cx,
            ),
        };

        // 宽度按窗口内容区钳制（原型 `min(940px, 100vw - 32px)`）：窄窗口下与其
        // 往两侧出界，不如两列一起挤窄。
        let gutter = px(TASK_EDITOR_GUTTER);
        let width =
            px(TASK_EDITOR_WIDTH).min((window.viewport_size().width - gutter).max(px(320.)));

        dialog
            .w(width)
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    task_editor_body(
                        heading_for_content,
                        &drag_for_content,
                        &form_for_content,
                        task_id,
                        &state_for_content,
                        &view_for_content,
                        cx,
                    ),
                ))
            })
            .footer(footer)
    });
}

/// 任务编辑弹窗的正文：双列（原型 `ed-*` 的左右两栏）。
///
/// `task_id` / `state` / `view` 是**里程碑**那一块用的：清单要在**渲染期**从页面
/// 状态现读（弹窗自己开着时也能从下面那层「添加里程碑」弹窗里加一条，缓存一份就会
/// 显示旧的），而增设 / 达成 / 删除三个动作都落在页面上。新建态还没有 id，里程碑
/// 区块只给一句说明（内核的里程碑挂在 `task_id` 上，凭空造不了）。
fn task_editor_body(
    heading: &str,
    drag: &Entity<duck_components::ModalDrag>,
    form: &Entity<TaskEditorForm>,
    task_id: Option<i64>,
    state: &Entity<ProjectsState>,
    view: &WeakEntity<ProjectsPage>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);

    // 「按日期重算」按钮 + 那句说明（都用在下面「预估工时」那一格）。
    //
    // 按钮**不随「日期排全没有」切换禁用态**：弹窗正文的 `content` 闭包只跑一次
    // （见本文件头部说明），日期选择器自己 `notify` 到不了这里，禁用态一旦画上去
    // 就不会再更新。所以「先排好期」这个前提写进说明文字，跨度不成立时按钮点了
    // 自然什么都不做（`recalc_estimate` 的空跨度分支）。
    let form_for_recalc = form.clone();
    let recalc = button(
        "projects-task-editor-recalc-hours",
        tr(cx, "按日期重算"),
        ButtonVariant::Secondary,
        cx,
    )
    .on_click(move |_, window, cx| {
        form_for_recalc.update(cx, |form, cx| form.recalc_estimate(window, cx));
    });
    let estimate_hint = SharedString::from(trf(
        cx,
        "按开始 / 截止日期自动算，每天 {} 小时、含首尾。手填后不再覆盖，点右侧可重算。",
        &[&HOURS_PER_WORKDAY.to_string()],
    ));

    // ---- 左列：标题 / 描述 / 日期 / 工时 / 进度 / 阻塞 ----
    let left = v_flex()
        .flex_grow(TASK_EDITOR_LEFT_GROW)
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
                form_field(tr(cx, "开始日期"), date_picker(&read.start), None, None, cx)
                    .into_any_element(),
                form_field(tr(cx, "截止日期"), date_picker(&read.due), None, None, cx)
                    .into_any_element(),
            ],
            cx,
        ))
        .child(form_row(
            [
                form_field(
                    tr(cx, "预估工时（小时）"),
                    input_with_action(line_input(&read.estimate, cx), recalc, cx),
                    None,
                    Some(estimate_hint),
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
            // 进度控件是一个独立视图（`Entity<V: Render>` 也是元素）：读数要跟着拖动
            // 实时变，而弹窗 `content` 闭包不会因为滑块自己 notify 而重跑。
            read.progress.clone(),
            None,
            Some(SharedString::from(tr(cx, "只可前进，不可回退。"))),
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

    let quadrant_control = quadrant_grid(form, read.quadrant, cx);

    // 父任务：候选 = 顶层任务（内核 `check_parent` 只许一层），已在开门时按同一条
    // 口径筛好并去掉任务自身。「顶层任务」那一项是**明确的摘除**（编辑态走
    // `clear_parent`），不是「不动」—— 少了它，挂错父任务就撤不回来。
    let parent_options: Vec<(SharedString, SharedString)> = std::iter::once((
        SharedString::from("none"),
        SharedString::from(tr(cx, "顶层任务")),
    ))
    .chain(read.parent_candidates.iter().map(|task| {
        (
            SharedString::from(task.id.to_string()),
            SharedString::from(task.title.clone()),
        )
    }))
    .collect();
    let current_parent = SharedString::from(
        read.parent
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_owned()),
    );
    let form_for_parent = form.clone();
    let parent_control = dropdown_select(
        "projects-task-editor-parent",
        parent_options,
        current_parent,
        move |value, _, cx| {
            let parent = if value.as_ref() == "none" {
                None
            } else {
                value.as_ref().parse::<i64>().ok()
            };
            form_for_parent.update(cx, |form, cx| {
                form.parent = parent;
                cx.notify();
            });
        },
        cx,
    );

    // 里程碑：清单在渲染期现读（见函数头），行复用右栏那一份 `milestone_row`
    // —— 圆点切换达成、垃圾桶删除都在里面；id 前缀必须换一个，两处会同时画。
    let progress_now = read.progress(cx);
    let milestones = match task_id {
        Some(id) => state.read(cx).task_milestones(id),
        None => Vec::new(),
    };
    let mut milestone_rows: Vec<AnyElement> = milestones
        .iter()
        .map(|milestone| {
            milestone_row(
                "projects-task-editor-milestone",
                milestone,
                progress_now,
                view,
                cx,
            )
        })
        .collect();
    if milestone_rows.is_empty() {
        milestone_rows.push(meta_text(tr(cx, "还没有里程碑。"), cx).into_any_element());
    }
    let milestone_footer = match task_id {
        Some(id) => button(
            "projects-task-editor-milestone-add",
            tr(cx, "添加里程碑"),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |page, cx| {
                    open_milestone_editor(page, id, progress_now, window, cx)
                });
            }
        })
        .into_any_element(),
        // 新建态还没有 task_id：内核的里程碑挂在任务上，先别摆一个点了没用的入口。
        None => meta_text(tr(cx, "保存后就能在右栏详情里添加里程碑。"), cx).into_any_element(),
    };

    let right = v_flex()
        .flex_grow(TASK_EDITOR_RIGHT_GROW)
        .min_w_0()
        .gap(tokens.spacing.sm)
        .child(form_field(
            tr(cx, "所属项目"),
            project_control,
            None,
            None,
            cx,
        ))
        .child(form_row(
            [
                form_field(tr(cx, "状态"), status_control, None, None, cx).into_any_element(),
                form_field(tr(cx, "优先级"), priority_control, None, None, cx).into_any_element(),
            ],
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
        .child(form_field(tr(cx, "父任务"), parent_control, None, None, cx))
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(caption_heading(tr(cx, "里程碑"), cx))
                .children(milestone_rows)
                .child(milestone_footer),
        )
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

/// 四象限 2×2 格子（原型 `.qgrid2`，`docs/projects-mock.html:1782`）。
///
/// 每格两行：色点 + 短名（选中态描边走强调色、底色走强调薄底），下面一行处置建议
/// —— 只给「Q2」的话，用的人得先想起来 Q2 是「重要不紧急」才敢点。四个格子一次
/// 全部可见，比下拉少一次展开、也看得见「别的选项长什么样」。
fn quadrant_grid(form: &Entity<TaskEditorForm>, current: Quadrant, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let option = |quadrant: Quadrant, cx: &App| {
        let picked = quadrant == current;
        let color = super::widgets::quadrant_badge::quadrant_color(quadrant, cx);
        let form_for_quadrant = form.clone();
        h_flex()
            .id(SharedString::from(format!(
                "projects-task-editor-quad-{}",
                quadrant.as_str()
            )))
            .flex_1()
            .min_w_0()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(if picked {
                tokens.colors.primary
            } else {
                extended.colors.hairline
            })
            .bg(if picked {
                extended.colors.hover_tint
            } else {
                tokens.colors.background
            })
            .cursor_pointer()
            .hover(move |style| style.bg(extended.colors.hover_tint))
            .gap(tokens.spacing.xs)
            .items_start()
            .child(
                div()
                    .flex_none()
                    .size(tokens.spacing.sm)
                    .mt(tokens.spacing.xxs)
                    .rounded_full()
                    .bg(color),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(quadrant_short_label(quadrant, cx))),
                    )
                    .child(
                        div()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(quadrant_hint(quadrant, cx))),
                    ),
            )
            .on_click(move |_, _, cx| {
                form_for_quadrant.update(cx, |form, cx| {
                    form.quadrant = quadrant;
                    cx.notify();
                });
            })
    };

    v_flex()
        .w_full()
        .gap(tokens.spacing.xxs)
        .child(
            h_flex()
                .gap(tokens.spacing.xxs)
                .child(option(Quadrant::Q1, cx))
                .child(option(Quadrant::Q2, cx)),
        )
        .child(
            h_flex()
                .gap(tokens.spacing.xxs)
                .child(option(Quadrant::Q3, cx))
                .child(option(Quadrant::Q4, cx)),
        )
}

/// 四象限的短名（格子与下拉共用；卡片徽标只给 `Q1` 字面量）。
fn quadrant_short_label(quadrant: Quadrant, cx: &App) -> &'static str {
    match quadrant {
        Quadrant::Q1 => tr(cx, "Q1 重要且紧急"),
        Quadrant::Q2 => tr(cx, "Q2 重要不紧急"),
        Quadrant::Q3 => tr(cx, "Q3 紧急不重要"),
        Quadrant::Q4 => tr(cx, "Q4 都不紧急"),
    }
}

/// 四象限的处置建议（原型 `QUAD[q].s`，`projects-mock.html:948`）。
fn quadrant_hint(quadrant: Quadrant, cx: &App) -> &'static str {
    match quadrant {
        Quadrant::Q1 => tr(cx, "立即处理"),
        Quadrant::Q2 => tr(cx, "尽快安排或委派"),
        Quadrant::Q3 => tr(cx, "计划安排"),
        Quadrant::Q4 => tr(cx, "有空再处理"),
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
    start: Entity<DatePickerState>,
    due: Entity<DatePickerState>,
    color: usize,
}

impl ProjectEditorForm {
    /// 「保存」那一刻读出的字段。
    fn collect(&self, cx: &App) -> CollectedProject {
        CollectedProject {
            name: input_value(&self.name, cx),
            description: textarea_value(&self.desc, cx),
            start: picker_date(&self.start, cx),
            due: picker_date(&self.due, cx),
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
            start: date_picker_state(window, cx),
            due: date_picker_state(window, cx),
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
            set_picker_date(&form.start, project.start_date, window, cx);
            set_picker_date(&form.due, project.due_date, window, cx);
        }
        form
    });

    let heading = match project_id {
        Some(_) => tr(cx, "项目设置"),
        None => tr(cx, "新建项目"),
    };
    // 确认按钮文案随模式分派（原型：新建「创建」/ 编辑「保存」）。
    let ok_text = if project_id.is_some() {
        tr(cx, "保存")
    } else {
        tr(cx, "创建")
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
        let heading_for_content = heading;

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let collected = form_for_ok.read(cx).collect(cx);
            if collected.name.is_empty() {
                return false;
            }
            let project_id = form_for_ok.read(cx).project_id;
            let _ = view_for_ok.update(cx, |page, cx| {
                page.submit_project_editor(project_id, collected, cx)
            });
            true
        });
        // 删除入口在正文里（`project_editor_body` 的底部动作），底部操作区只放
        // 「取消 / 确认」——两处都摆「删除」会让人怀疑到底哪个是真的。
        let footer = dialog_footer(
            "projects-project-editor",
            ok_text,
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    project_editor_body(
                        heading_for_content,
                        &drag_for_content,
                        &form_for_content,
                        project_id,
                        &view_for_delete,
                        cx,
                    ),
                ))
            })
            .footer(footer)
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
                form_field(tr(cx, "开始日期"), date_picker(&read.start), None, None, cx)
                    .into_any_element(),
                form_field(tr(cx, "目标完成日"), date_picker(&read.due), None, None, cx)
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

        // 只有一个「完成」：这一窗是清单管理，输入框里的待建标签顺手带上、然后关窗。
        // 摆「取消」会让人以为「取消 = 刚才删掉的标签也退回来」——删除是即时生效的。
        let footer = DialogFooter::new().child(
            button(
                "projects-tag-manager-done",
                tr(cx, "完成"),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let name = input_value(&form_for_add.read(cx).new_name, cx);
                if !name.is_empty() {
                    let _ = view_for_add.update(cx, |page, cx| page.create_tag(&name, cx));
                }
                window.close_dialog(cx);
            }),
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    tag_manager_body(&form_for_content, &drag_for_content, &view_for_content, cx),
                ))
            })
            .footer(footer)
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

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let specs = form_for_ok.read(cx).collect(cx);
            let _ = view_for_ok.update(cx, |page, cx| page.submit_columns(specs, cx));
            true
        });
        let footer = dialog_footer(
            "projects-column-settings",
            tr(cx, "保存"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    column_settings_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .footer(footer)
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

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let text = input_value(&form_for_ok.read(cx).input, cx);
            if text.is_empty() {
                return false;
            }
            let _ = view_for_ok.update(cx, |page, cx| page.submit_spotlight(&text, cx));
            true
        });
        let footer = dialog_footer(
            "projects-spotlight",
            tr(cx, "创建"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    spotlight_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .footer(footer)
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

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let text = textarea_value(&body_for_ok, cx);
            let _ = view_for_ok.update(cx, |page, cx| {
                page.submit_log(date_for_ok, kind_for_ok, &text, cx)
            });
            true
        });
        let footer = dialog_footer(
            "projects-log-editor",
            tr(cx, "保存"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
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
            .footer(footer)
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

// ---------------------------------------------------------------------------
// 里程碑编辑弹窗（原型 openMilestoneEditor）
// ---------------------------------------------------------------------------

/// 里程碑快照的默认值：当前进度 + 20，封顶 100（原型 `Math.min(100, t.prog + 20)`）。
///
/// 提成自由函数是为了能单测：这是「打开弹窗那一刻」的取值，不算界面状态。
pub(crate) fn default_milestone_snapshot(progress: u8) -> u8 {
    progress.saturating_add(20).min(100)
}

/// 打开「添加里程碑」弹窗（右栏「里程碑」分区的 `+`）。
///
/// 只做**添加**，不做改名：原型的编辑入口在里程碑行上，而本项目右栏每行已经要挂
/// 「达成 / 删除」两个动作；再叠一个「点行改名」的弹窗，收益远小于多一套表单状态
/// 要维护。真要改名走「完整编辑器」那条路更顺。
pub(crate) fn open_milestone_editor(
    page: &mut ProjectsPage,
    task_id: i64,
    progress: u8,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let title = new_input(window, cx, "灰度上线");
    let snapshot = new_input(window, cx, "60");
    fill_input(
        &snapshot,
        &default_milestone_snapshot(progress).to_string(),
        window,
        cx,
    );

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let title_for_content = title.clone();
        let snapshot_for_content = snapshot.clone();
        let title_for_ok = title.clone();
        let snapshot_for_ok = snapshot.clone();
        let view_for_ok = view.clone();

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let title = input_value(&title_for_ok, cx);
            if title.is_empty() {
                return false;
            }
            let snapshot = input_value(&snapshot_for_ok, cx)
                .parse::<u8>()
                .unwrap_or(0)
                .min(100);
            let _ = view_for_ok.update(cx, |page, cx| {
                page.submit_milestone(task_id, &title, snapshot, cx)
            });
            true
        });
        let footer = dialog_footer(
            "projects-milestone-editor",
            tr(cx, "添加"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(FORM_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                let theme = active_theme(cx);
                let tokens = theme.tokens();
                content.child(modal_drag_area(
                    &drag_for_content,
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .child(modal_title_row(
                            "projects-milestone-editor-title",
                            &drag_for_content,
                            SharedString::from(tr(cx, "添加里程碑")),
                            cx,
                        ))
                        .child(form_field(
                            tr(cx, "名称"),
                            line_input(&title_for_content, cx),
                            None,
                            None,
                            cx,
                        ))
                        .child(form_field(
                            tr(cx, "进度快照"),
                            line_input(&snapshot_for_content, cx),
                            None,
                            // 内核在创建时若发现快照高于任务当前进度，会把任务进度一并
                            // 推到快照上 —— 这句得说清楚，否则像是「只写了个数字」。
                            Some(SharedString::from(tr(
                                cx,
                                "进度达到快照即视为达成；高于当前进度时会一并推进任务进度。",
                            ))),
                            cx,
                        )),
                ))
            })
            .footer(footer)
    });
}

// ---------------------------------------------------------------------------
// 导入 / 导出（原型 openImport + openExport）
// ---------------------------------------------------------------------------

/// 导入 / 导出弹窗宽。
///
/// 比单列表单（[`FORM_WIDTH`] = 520）宽一截：两个弹窗的主体都是一大段文本
/// （导入是粘贴框 + 解析预览，导出是输出预览），520 会把它们压成一条窄缝。
/// 布局几何量，留裸值并写明来由。
const TEXT_DIALOG_WIDTH: f32 = 880.;

/// 文本预览区的高度（行高的倍数）。
///
/// 按行高算而不是写死像素：行高是字阶 token 的一部分，跟着「缩放」档一起变；
/// 写死 px 会在放大字号后把预览压成一条缝。
const PREVIEW_ROWS: f32 = 13.;

/// 打开「导出任务」弹窗（原型 `openExport`）。
///
/// 导出**不落盘**：只生成一段文本，「复制到剪贴板」交出去（理由见 [`super::export`]）。
/// 任务在打开时**拍一次快照** —— 模态开着时底下的库不会变（用户在弹窗里改不了任务），
/// 而弹窗内容闭包每帧重跑，每帧去查一次库纯属白付。
pub(crate) fn open_export(
    page: &mut ProjectsPage,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    // 「全部任务」那一档要绕过筛选，得单独查一趟。查失败就别开弹窗了：开一个空预览
    // 比不开更让人怀疑数据丢了。
    let all_tasks = match page.state.read(cx).all_tasks() {
        Ok(tasks) => tasks,
        Err(err) => {
            page.show(
                Tone::Error,
                trf(cx, "读取任务失败：{}", &[&err.to_string()]),
                None,
                cx,
            );
            return;
        }
    };
    let (view_tasks, projects, today) = {
        let state = page.state.read(cx);
        (
            state.tasks().to_vec(),
            state.projects().to_vec(),
            state.today(),
        )
    };
    let form = cx.new(|_| ExportForm {
        scope: ExportScope::View,
        format: ExportFormat::Markdown,
        view_tasks,
        all_tasks,
        projects,
        today,
    });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();

        // 复制完就该走了：返回 `true` 让弹窗收掉（早先写成 `false`，那是「校验未过、
        // 弹窗留着」的语义，结果复制完弹窗还杵在原地）。
        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let read = form_for_ok.read(cx);
            let tasks = read.scope.tasks(&read.view_tasks, &read.all_tasks);
            let text = super::export::render(
                read.format,
                read.today,
                tr(cx, "任务导出"),
                tasks,
                |id| project_name_of(&read.projects, id),
            );
            let _ = view_for_ok.update(cx, |page, cx| page.copy_export(text, cx));
            true
        });
        let footer = dialog_footer(
            "projects-export",
            tr(cx, "复制到剪贴板"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(TEXT_DIALOG_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    export_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .footer(footer)
    });
}

/// 导出弹窗的表单状态。
struct ExportForm {
    scope: ExportScope,
    format: ExportFormat,
    /// 当前视图那一份（已应用筛选与排序）。
    view_tasks: Vec<Task>,
    /// 全部顶层任务（绕过筛选与分档）。
    all_tasks: Vec<Task>,
    /// 项目表：导出只要「id → 名字」，拍一份快照够用。
    projects: Vec<Project>,
    today: CalendarDate,
}

/// 按 `id` 查项目名；查不到（项目被删了）与收集箱都返回 `None`。
fn project_name_of(projects: &[Project], id: Option<i64>) -> Option<String> {
    let id = id?;
    projects
        .iter()
        .find(|project| project.id == id)
        .map(|project| project.name.clone())
}

fn export_body(
    form: &Entity<ExportForm>,
    drag: &Entity<duck_components::ModalDrag>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);
    let tasks = read.scope.tasks(&read.view_tasks, &read.all_tasks);
    let heading = tr(cx, "任务导出");
    let text = super::export::render(read.format, read.today, heading, tasks, |id| {
        project_name_of(&read.projects, id)
    });

    // 范围：两行单选（原型 `.radio`）。读数写在右端，一眼看出两档差多少条。
    let scope_labels: [(ExportScope, &'static str); 2] = [
        (ExportScope::View, tr(cx, "当前视图（已应用筛选与排序）")),
        (ExportScope::All, tr(cx, "全部任务")),
    ];
    let mut scope_col = v_flex()
        .w_full()
        .gap(tokens.spacing.xxs)
        .id("projects-export-scope")
        .role(Role::RadioGroup)
        .aria_label(tr(cx, "范围"));
    for (scope, label) in scope_labels {
        let count = scope.tasks(&read.view_tasks, &read.all_tasks).len();
        let form_for_click = form.clone();
        scope_col = scope_col.child(choice_row(
            match scope {
                ExportScope::View => "projects-export-scope-view",
                ExportScope::All => "projects-export-scope-all",
            },
            SharedString::from(label),
            SharedString::from(trf(cx, "{} 条", &[&count.to_string()])),
            read.scope == scope,
            move |_, _, cx| {
                form_for_click.update(cx, |form, cx| {
                    form.scope = scope;
                    cx.notify();
                });
            },
            cx,
        ));
    }

    // 格式：三档 segmented（原型 `#ex-fmt`）。三个名字都是格式专有名词，不译。
    let format_labels: Vec<SharedString> = ExportFormat::ALL
        .iter()
        .map(|format| SharedString::from(format.label()))
        .collect();
    let format_index = ExportFormat::ALL
        .iter()
        .position(|format| *format == read.format)
        .unwrap_or_default();
    let form_for_format = form.clone();
    let format_tabs = segmented_tabs(
        "projects-export-format",
        format_labels,
        format_index,
        move |index, _, cx| {
            let format = ExportFormat::ALL[index];
            form_for_format.update(cx, |form, cx| {
                form.format = format;
                cx.notify();
            });
        },
        cx,
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-export-title",
            drag,
            tr(cx, "导出任务"),
            cx,
        ))
        .child(form_field(tr(cx, "范围"), scope_col, None, None, cx))
        .child(form_field(tr(cx, "格式"), format_tabs, None, None, cx))
        .child(field_hint(
            trf(
                cx,
                "{} · {} 字符 · 导出不写入本地库，只产生文本",
                &[read.format.label(), &text.chars().count().to_string()],
            ),
            cx,
        ))
        .child(preview_box(text, cx))
}

/// 打开「从文本导入任务」弹窗（原型 `openImport`）。
///
/// 与原型的一处**刻意不同**：原型把示例文本**填进**输入框（`value=`），照抄的话
/// 用户什么都不做直接按「导入」就会真的把示例建进库里。这里改成占位文案
/// （`placeholder`）+ 一个「填入示例」按钮 —— 示例还在，但不会被动地变成数据。
pub(crate) fn open_import(
    page: &mut ProjectsPage,
    window: &mut Window,
    cx: &mut Context<ProjectsPage>,
) {
    let (projects, today) = {
        let state = page.state.read(cx);
        (state.projects().to_vec(), state.today())
    };
    // 先把占位文案算出来再建输入框：`new_area(window, cx, &import_sample(cx))` 会把
    // `cx` 同时按 `&mut`（建实体）和 `&`（取翻译）借两次，直接撞 E0502。
    let placeholder = import_sample(cx);
    let text = new_area(window, cx, &placeholder);
    let form = cx.new(|_| ImportForm {
        text,
        today,
        projects,
    });

    let drag = modal_drag(cx);
    let view = page.view.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let offset = modal_offset(&drag, cx);
        let drag_for_content = drag.clone();
        let form_for_content = form.clone();
        let form_for_ok = form.clone();
        let view_for_ok = view.clone();

        let confirm: ConfirmHandler = std::rc::Rc::new(move |_, cx| {
            let text = import_raw(&form_for_ok, cx);
            let _ = view_for_ok.update(cx, |page, cx| page.submit_import(&text, cx));
            true
        });
        let footer = dialog_footer(
            "projects-import",
            tr(cx, "导入"),
            ButtonVariant::Primary,
            confirm,
            cx,
        );

        dialog
            .w(px(TEXT_DIALOG_WIDTH))
            .ml(offset.x)
            .margin_top(dialog_top_offset(cx) + offset.y)
            .opacity(offset.opacity)
            .close_button(false)
            .content(move |content, _, cx| {
                content.child(modal_drag_area(
                    &drag_for_content,
                    import_body(&form_for_content, &drag_for_content, cx),
                ))
            })
            .footer(footer)
    });
}

/// 示例文本（原型的 `IMPORT_SAMPLE`）：占位文案与「填入示例」共用同一份真源。
///
/// 逐行给、用 `join("\n")` 拼，**不写成一行带 `\n` 的字符串**：i18n 守卫逐字面量
/// 按**源码原文**比对（`literals()` 把 `\"` 当转义但把 `\n` 原样留着），一行里塞
/// `\n` 会让整串变成一个「带反斜杠 n」的怪 key —— 翻译时得跟着写 `\\n`，谁少写一个
/// 反斜杠就静默漏翻。三行分开就都是干净的短句。
fn import_sample(cx: &App) -> String {
    [
        tr(cx, "- [ ] 整理需求 #产品 @Alpha !P1 明天"),
        tr(cx, "- [x] 提交周报"),
        tr(cx, "- 修登录超时 @Alpha"),
    ]
    .join("\n")
}

/// 导入弹窗的表单状态。
struct ImportForm {
    text: Entity<TextareaState>,
    today: CalendarDate,
    /// 项目表：预览里的项目色点要按名字查色。
    projects: Vec<Project>,
}

fn import_body(
    form: &Entity<ImportForm>,
    drag: &Entity<duck_components::ModalDrag>,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let read = form.read(cx);
    let text = import_raw(form, cx);
    let rows = quickparse::parse_lines(&text, read.today);

    let form_for_sample = form.clone();
    let form_for_clear = form.clone();
    let sample = import_sample(cx);
    let actions = h_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(
            button(
                "projects-import-sample",
                tr(cx, "填入示例"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, window, cx| {
                fill_import(&form_for_sample, &sample, window, cx);
            }),
        )
        .child(
            button(
                "projects-import-clear",
                tr(cx, "清空"),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click(move |_, window, cx| {
                fill_import(&form_for_clear, "", window, cx);
            }),
        );

    let left = v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .child(duck_components::body_text(tr(cx, "粘贴内容"), cx))
        .child(form_field(
            tr(cx, "支持 Markdown 清单，每行一个任务"),
            area_input(&read.text, 12, 20, cx),
            None,
            Some(SharedString::from(tr(cx, "语法与快速新建一致"))),
            cx,
        ))
        .child(actions);

    let right = v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .child(duck_components::body_text(
            trf(cx, "解析预览（{} 项）", &[&rows.len().to_string()]),
            cx,
        ))
        .child(import_preview(&rows, &read.projects, cx))
        .child(field_hint(tr(cx, "解析不出的部分原样作为标题。"), cx));

    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(modal_title_row(
            "projects-import-title",
            drag,
            tr(cx, "从文本导入任务"),
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

/// 解析预览（原型 `#im-prev`）：逐行画出将要建成的样子。
fn import_preview(rows: &[ImportLine], projects: &[Project], cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let xs = tokens.typography.xs;

    let mut list = v_flex()
        .w_full()
        .h(tokens.typography.sm.line_height * PREVIEW_ROWS)
        .id("projects-import-preview")
        .overflow_y_scroll()
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .p(tokens.spacing.xs);

    if rows.is_empty() {
        return list.child(duck_components::body_text(tr(cx, "没有可解析的行。"), cx));
    }

    for row in rows {
        let icon = if row.done {
            DuckIcon::CheckboxCheck
        } else {
            DuckIcon::Square
        };
        let title = if row.parsed.title.is_empty() {
            SharedString::from(tr(cx, "（空标题）"))
        } else {
            SharedString::from(row.parsed.title.clone())
        };
        let mut line = h_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .py(tokens.spacing.xxs)
            .text_size(xs.size)
            .line_height(xs.line_height)
            .child(Icon::new(icon).size(xs.size).text_color(if row.done {
                extended.colors.success
            } else {
                tokens.colors.muted_foreground
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(if row.done {
                        tokens.colors.muted_foreground
                    } else {
                        tokens.colors.foreground
                    })
                    .child(title),
            );
        if let Some(priority) = row.parsed.priority {
            line = line.child(
                div()
                    .flex_none()
                    .text_color(tokens.colors.muted_foreground)
                    .child(SharedString::from(format!("P{}", priority.as_i64()))),
            );
        }
        if let Some(due) = row.parsed.due {
            line = line.child(
                div()
                    .flex_none()
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(due.to_iso())),
            );
        }
        if let Some(color) = row
            .parsed
            .project
            .as_ref()
            .and_then(|name| {
                projects
                    .iter()
                    .find(|project| project.name.eq_ignore_ascii_case(name))
            })
            .map(|project| color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground))
        {
            line = line.child(div().flex_none().size(px(8.)).rounded_full().bg(color));
        }
        list = list.child(line);
    }
    list
}

/// 只读文本预览框（原型 `.ta.mono`）：可滚动，字数一多也不会把弹窗撑破。
///
/// 逐行画而不是把整段塞进一个文本元素：CSV / JSON 的换行必须**行行分明**（一行一个
/// 元素），否则一旦某个字段特别长，整段会按窗口宽度重排、看起来像错行。
fn preview_box(text: String, cx: &App) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let xs = tokens.typography.xs;
    div()
        .id("projects-export-preview")
        .w_full()
        .h(tokens.typography.sm.line_height * PREVIEW_ROWS)
        .overflow_y_scroll()
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .p(tokens.spacing.sm)
        .text_size(xs.size)
        .line_height(xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .children(
            text.lines()
                .map(|line| {
                    div()
                        .whitespace_nowrap()
                        .child(SharedString::from(line.to_owned()))
                })
                .collect::<Vec<_>>(),
        )
}

/// 单选行（原型 `.radio`）：圆点 + 文案 + 右端读数。
///
/// 圆点用「粗描边」而不是图标：原型 `.radio.on i{border-width:5px}` 是个甜甜圈
/// 画法 —— 15px 的圆、选中时描边 5px，中间自然留出 5px 的洞。`DuckIcon` 里没有
/// 空心圆（只有 `CircleCheck`），拿勾号当单选点会被误读成「已勾选的复选框」。
fn choice_row(
    id: &'static str,
    label: SharedString,
    detail: SharedString,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let accent = tokens.colors.primary;
    let hover_tint = extended.colors.hover_tint;

    let dot_size = extended.icon.md;
    let dot = div()
        .flex_none()
        .size(dot_size)
        .rounded_full()
        .border(if selected {
            // 1/3 直径：15 → 5px 的环，洞口约 5px，与原型一致。
            dot_size * (1. / 3.)
        } else {
            extended.stroke.thin
        })
        .border_color(if selected {
            accent
        } else {
            tokens.colors.border
        });

    div()
        .id(id)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(if selected {
            accent
        } else {
            extended.colors.hairline
        })
        .bg(if selected {
            accent.opacity(extended.opacity.tint)
        } else {
            tokens.colors.surface
        })
        .text_size(tokens.typography.sm.size)
        .line_height(tokens.typography.sm.line_height)
        .text_color(if selected {
            accent
        } else {
            tokens.colors.foreground
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_tint))
        .role(Role::RadioButton)
        .aria_label(label.clone())
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .on_click(on_click)
        .child(dot)
        .child(div().flex_1().min_w_0().child(label))
        .child(
            div()
                .flex_none()
                .text_color(extended.colors.text_tertiary)
                .child(detail),
        )
}

/// 导入框的原始值（**不去空白**）。
///
/// 不复用 `textarea_value`：那个会 `trim`。导入的每一行前后空白由解析层自己
/// `trim`，而多行文本一旦在读取时就被 `trim` 掉末尾换行，用户正在敲的空行会
/// 「自己消失」—— 编辑体验会变得很怪。
fn import_raw(form: &Entity<ImportForm>, cx: &App) -> String {
    form.read(cx).text.read(cx).value().to_string()
}

/// 把一段文本灌进导入框并重绘（「填入示例」/「清空」共用）。
///
/// 这里的 `cx.notify()` 不是可有可无：右侧的解析预览是**从 `ImportForm` 现读**
/// 的，光把值写进 `TextareaState` 只会重绘输入框本身，预览会停在旧文本上。
fn fill_import(form: &Entity<ImportForm>, value: &str, window: &mut Window, cx: &mut App) {
    form.update(cx, |form, cx| {
        form.text.update(cx, |state, cx| {
            state.set_value(SharedString::from(value.to_owned()), window, cx);
        });
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个必定的合法日期（测试里不该出现非法值，所以直接 `expect`）。
    fn day(month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(2026, month, day).expect("测试日期应当合法")
    }

    /// 预估工时的跨度**含首尾**：10-01 → 10-02 是 2 天，不是 1 天。
    ///
    /// 这条口径光写在 `workdays_between` 的注释里守不住 —— 「差一天」是最容易被后来者
    /// 顺手「修正」的地方，而它是用户在界面上能直接数出来的数（选 10-01 到 10-02，
    /// 预估工时应当是 16 而不是 8）。
    #[test]
    fn workdays_span_includes_both_ends() {
        assert_eq!(
            workdays_between(Some(day(10, 1)), Some(day(10, 1))),
            Some(1),
            "同一天算 1 天"
        );
        assert_eq!(
            workdays_between(Some(day(10, 1)), Some(day(10, 2))),
            Some(2)
        );
        assert_eq!(
            workdays_between(Some(day(10, 1)), Some(day(10, 31))),
            Some(31)
        );
        // 跨月也要对：别用「月份相减 × 30」那类近似，历法由内核算。
        assert_eq!(
            workdays_between(Some(day(9, 28)), Some(day(10, 2))),
            Some(5)
        );
    }

    /// 跨度不成立的情形一律 `None`：宁可让调用处「什么都不做」，也不要填 0 / 负数。
    #[test]
    fn workdays_span_missing_or_reversed_is_none() {
        assert_eq!(workdays_between(None, Some(day(10, 2))), None, "缺开始");
        assert_eq!(workdays_between(Some(day(10, 1)), None), None, "缺截止");
        assert_eq!(
            workdays_between(Some(day(10, 2)), Some(day(10, 1))),
            None,
            "截止早于开始"
        );
    }

    /// 工时 = 天数 × 每天小时数（口径的乘法那一半）。
    #[test]
    fn hours_are_days_times_workday_length() {
        let days = workdays_between(Some(day(10, 1)), Some(day(10, 2))).expect("两端都在");
        assert_eq!(
            days * HOURS_PER_WORKDAY,
            16,
            "10-01 → 10-02 = 2 天 × 8 小时"
        );
    }
}
