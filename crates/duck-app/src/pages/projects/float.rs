//! 「随手看」悬浮窗 —— 项目管理页的第二个窗口形态（`docs/projects-design.md` §4.3 ⑤）。
//!
//! 独立无边框小窗（300×550）、**置顶**、**不抢焦点**、不占 Dock。两段列表：进行中
//! （全部）在前、待办（前 5 条 + 「展开剩余 N 个任务」）在后；点卡片就地展开详情。
//!
//! # 与主窗的关系
//!
//! 它**自己**打开一个 `duck-project` 连接（同一个库文件），而不是借主窗那个 ——
//! 因为主窗关掉之后它还得能用（「随手看」的价值正在这里）。代价是两处视图不会自动
//! 互知，所以走 [`super::bus`] 的变更广播：谁落了库就广播一声，两边各自重读。
//!
//! # 三处刻意与原型不同
//!
//! 1. **不抢焦点**（`focus: false`）。原型 §4.3 ⑤ 只写「不占 Dock」，但「随手看」的
//!    用途是瞄一眼 —— 抢走前台会把「瞥一下」变成「切走一次」。窗口本身是
//!    `WindowKind::PopUp`（macOS 上是 `NSWindowStyleMaskNonactivatingPanel` 的
//!    `NSPanel`），`focus: false` 只影响**刚弹出那一下**：之后点它照样能拿键盘
//!    （gpui 的 `acceptsFirstMouse` 返回 YES，第一下点击就送进内容区）。代价是
//!    `Esc` 收不起来（它压根不是 key window），收起只有再按一次全局键、点 ✕ 两条路。
//! 2. **内联详情不放开「编辑」按钮**：完整任务编辑器属 S5，摆一个点了没反应的按钮
//!    比不摆更糟（P3-6）。详情里给的是「开始 / 暂停 / 完成 / 返回」四个都真会动数据的键。
//! 3. **「新增」是窗口内联的一行输入**，不是主窗那个新建弹窗：300pt 宽的窗里再套一层
//!    对话框会把可用面积压到没法看，而输入框只有一行。

use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    Icon,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use duck_components::DuckIcon;
use duck_components::content::neutral_tag;
use duck_project::{
    ArchiveScope, CalendarDate, NewTask, Project, ProjectStore, Task, TaskQuery, TaskSort,
    TaskStatus,
};
use duck_theme::{ExtendedTokens, active_theme, color_from_hex, tr, trf, trn};

use super::bus;
use super::state::local_today;
use super::widgets::priority_dot::priority_dot;
use super::widgets::priority_dot::priority_label;
use super::widgets::progress_bar::task_progress;

/// 窗口尺寸（照参考实现 `tauri.conf.json:73-90`：300×550，不可缩放）。
const FLOAT_SIZE: gpui::Size<gpui::Pixels> = size(px(300.), px(550.));
/// 首次出现时距离屏幕边缘的留白。
const SCREEN_MARGIN: f32 = 24.;
/// 顶部抓取条的高度（原型 `.fhead` 的 26px）。
const GRIP_HEIGHT: f32 = 26.;
/// 进行中段的最大高度（原型 `.fmid` 的 `max-height:206px`）：超出的部分自己滚，
/// 把剩下的高度留给待办 —— 待办通常比进行中长得多。
const DOING_MAX_HEIGHT: f32 = 206.;
/// 待办段默认展示几条（原型 `maxDisplayTodo = 5`）。
const TODO_PREVIEW: usize = 5;
/// 内联详情的描述最多显示多少字符（小窗里没有滚动到天边的余地）。
const DETAIL_CHARS: usize = 160;
/// 动作回执的存活时长。
const HINT_LIFETIME: Duration = Duration::from_millis(2000);

/// 悬浮窗窗口选项。
///
/// `is_movable: true` 且**不**声明 `app_owns_titlebar_drag` —— 于是 macOS 上 AppKit
/// 保留顶部约 28pt 的隐形标题栏当系统拖拽区，正好盖住原型那条 26px 抓取条
/// （它本来也不接点击）。命令面板那边正相反（`app_owns_titlebar_drag: true`），
/// 因为它的整窗都是可点内容。
#[must_use]
pub(crate) fn float_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(float_window_bounds(cx)),
        // 无标题栏：窗口内容就是那一整块卡片。
        titlebar: None,
        kind: gpui::WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        is_movable: true,
        // 显式写 `false` 而不是靠 `WindowOptions::default()`：这条正是本窗的**设计点**
        // （见函数文档），把它交给默认值等于把一个有意决策埋在别处。
        app_owns_titlebar_drag: false,
        // 透明底才能露出卡片自己的圆角与系统阴影。
        window_background: WindowBackgroundAppearance::Transparent,
        // 关键：弹出时不抢焦点。「瞄一眼」不该把当前工作切走。
        focus: false,
        show: true,
        ..WindowOptions::default()
    }
}

/// 悬浮窗的几何：上次待的地方，没有存档就落在主屏右上角。
///
/// 与命令面板一样**不跟随鼠标所在屏**：唤起键是键盘操作，鼠标可能停在任意位置。
#[must_use]
pub(crate) fn float_window_bounds(cx: &App) -> WindowBounds {
    if let Some(origin) = crate::settings::restore_task_float(cx) {
        return WindowBounds::Windowed(Bounds::new(origin, FLOAT_SIZE));
    }
    let Some(display) = cx.primary_display() else {
        return WindowBounds::centered(FLOAT_SIZE, cx);
    };
    let screen = display.bounds();
    WindowBounds::Windowed(Bounds::new(
        point(
            screen.origin.x + screen.size.width - FLOAT_SIZE.width - px(SCREEN_MARGIN),
            screen.origin.y + px(SCREEN_MARGIN),
        ),
        FLOAT_SIZE,
    ))
}

/// 悬浮窗的数据。
///
/// 不实现 `Render`（三件套纪律）：它只认数据，所以能单测。
pub(crate) struct FloatState {
    /// 内核。打不开时为 `None`，此时 [`Self::error`] 有值。
    store: Option<ProjectStore>,
    /// 打开内核失败的原因（英文，直接来自内核）。
    error: Option<String>,
    /// 进行中（全部，顶层任务）。
    doing: Vec<Task>,
    /// 待办（未展开时只有前 [`TODO_PREVIEW`] 条）。
    todos: Vec<Task>,
    /// 待办总数 —— **顶层待办的条数**（与 [`Self::todos`] 同源，算「展开剩余 N 个任务」用）。
    ///
    /// ⚠️ 别改用 `board_stats().todo`：那个口径只按 `archived_at` 过滤、**含子任务**，
    /// 会把「展开剩余」算多（展开后一条不多、那一行自己消失）。
    todos_total: u32,
    /// 待办段是否已展开。
    ///
    /// **单向**：展开后不再给「收起」—— 这一窗本来就小，收起只会让人再点一次，
    /// 而原始形态就是「5 条 + 展开剩余」。
    expanded: bool,
    /// 就地展开详情的那条任务。
    open: Option<i64>,
    /// 项目表（卡片上显示项目名与色点）。
    projects: Vec<Project>,
    /// 算「逾期」用的今天。
    today: CalendarDate,
}

impl FloatState {
    /// 建状态并读第一屏。
    pub(crate) fn new() -> Self {
        let (store, error) = Self::open_store();
        let mut state = Self {
            store,
            error,
            doing: Vec::new(),
            todos: Vec::new(),
            todos_total: 0,
            expanded: false,
            open: None,
            projects: Vec::new(),
            today: local_today(),
        };
        state.reload();
        state
    }

    /// 打开内核。失败时退到内存库（界面仍可用，但绝不静默）。
    ///
    /// 与 `ProjectsState::open_store` 同一条口径；两处各写一遍是因为它们各自的
    /// 「退到内存库」表现不同（那边整页换错误态，这边只是多一行提示 + 列表空）。
    fn open_store() -> (Option<ProjectStore>, Option<String>) {
        match ProjectStore::open_default() {
            Ok(store) => (Some(store), None),
            Err(err) => {
                let message = err.to_string();
                match ProjectStore::open_in_memory() {
                    Ok(store) => (Some(store), Some(message)),
                    Err(fallback) => (
                        None,
                        Some(format!("{message}; in-memory fallback failed: {fallback}")),
                    ),
                }
            }
        }
    }

    /// 重读两段列表与项目表。
    pub(crate) fn reload(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        self.today = local_today();
        match store.list_projects(ArchiveScope::Live) {
            Ok(projects) => self.projects = projects,
            Err(err) => return self.fail(err),
        }
        // 两段都按优先级排：这一窗存在的意义就是「我该干什么」，手动顺序留给看板。
        let doing = store.list_tasks(
            &TaskQuery::new()
                .top_level_only()
                .statuses([TaskStatus::Doing])
                .sort(TaskSort::PriorityFirst),
        );
        match doing {
            Ok(tasks) => self.doing = tasks,
            Err(err) => return self.fail(err),
        }
        // 待办：**一次把全部顶层待办读回来**，再按需截前几条。
        //
        // ⚠️ 总数别拿 `board_stats().todo` 顶替：那个口径只按 `archived_at` 过滤，
        // **含子任务**；而这一段只列顶层（`top_level_only`）。两者一混，只要有待办
        // 子任务，「展开剩余 N 个任务」就会多算 —— 点下去一条不多，那一行自己消失。
        // 与其在两个口径之间换算，不如让**总数与列表同源**。
        let todos = store.list_tasks(
            &TaskQuery::new()
                .top_level_only()
                .statuses([TaskStatus::Todo])
                .sort(TaskSort::PriorityFirst),
        );
        let todos = match todos {
            Ok(tasks) => tasks,
            Err(err) => return self.fail(err),
        };
        self.todos_total = todos.len() as u32;
        self.todos = if self.expanded {
            todos
        } else {
            todos.into_iter().take(TODO_PREVIEW).collect()
        };
        // 展开着的那条可能刚被改到别的状态（或删了），这一遍就把它放下 ——
        // 否则详情会一直挂在一条已经不在列表里的任务上。
        if let Some(id) = self.open
            && !self.doing.iter().any(|task| task.id == id)
            && !self.todos.iter().any(|task| task.id == id)
        {
            self.open = None;
        }
    }

    /// 收敛一次读失败。
    fn fail(&mut self, error: duck_project::ProjectError) {
        tracing::warn!(%error, "task float could not read the project store");
        self.error = Some(error.to_string());
    }

    /// 进行中段。
    pub(crate) fn doing(&self) -> &[Task] {
        &self.doing
    }

    /// 待办段（未展开时是前几条）。
    pub(crate) fn todos(&self) -> &[Task] {
        &self.todos
    }

    /// 待办还有几条没显示（0 = 没有「展开」这一行）。
    pub(crate) fn todos_collapsed(&self) -> u32 {
        if self.expanded {
            return 0;
        }
        self.todos_total
            .saturating_sub(u32::try_from(self.todos.len()).unwrap_or(u32::MAX))
    }

    /// 就地展开的那条任务 id。
    pub(crate) fn open(&self) -> Option<i64> {
        self.open
    }

    /// 今天（算逾期）。
    pub(crate) fn today(&self) -> CalendarDate {
        self.today
    }

    /// 读错误（英文原文；界面文案由调用方组）。
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 某条任务所属项目。
    pub(crate) fn project_of(&self, task: &Task) -> Option<&Project> {
        let id = task.project_id?;
        self.projects.iter().find(|project| project.id == id)
    }

    /// 展开待办。
    pub(crate) fn expand_todos(&mut self) {
        if self.expanded {
            return;
        }
        self.expanded = true;
        self.reload();
    }

    /// 就地展开 / 收起一条任务的详情。
    pub(crate) fn toggle_open(&mut self, id: i64) {
        self.open = if self.open == Some(id) {
            None
        } else {
            Some(id)
        };
    }

    /// 收起详情。
    pub(crate) fn close_detail(&mut self) {
        self.open = None;
    }

    /// 改一条任务的状态（详情里的按钮用）。
    ///
    /// 内核自己会校验状态迁移；非法迁移原样把错误交回调用方去提示。
    pub(crate) fn set_status(&mut self, id: i64, status: TaskStatus) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store.move_task(id, status, 0)?;
        self.reload();
        Ok(())
    }

    /// 在「待办」里新建一条任务（悬浮窗的新增行）。
    pub(crate) fn create(&mut self, title: &str) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store.create_task(NewTask::new(title))?;
        self.reload();
        Ok(())
    }
}

/// 悬浮窗视图。
pub struct FloatWindow {
    state: FloatState,
    /// 新增行是否打开着。打开时底部多一行输入框。
    capturing: bool,
    /// 新增行的输入框（`capturing` 为真时才有值）。
    draft: Option<Entity<InputState>>,
    /// 新增行回车的订阅。
    draft_sub: Option<Subscription>,
    /// 窗口位置观察（首帧挂，见 `Render::render`）。
    bounds_watch: Option<Subscription>,
    /// 变更广播的订阅（主窗改了库，这一窗跟着重读）。
    bus_sub: Option<Subscription>,
    /// 底部那行回执（文案 / 是不是错误）。
    hint: Option<(SharedString, bool)>,
    /// 回执的序号源（定时器守卫）。
    hint_seq: u64,
}

impl FloatWindow {
    /// 建窗。
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: FloatState::new(),
            capturing: false,
            draft: None,
            draft_sub: None,
            bounds_watch: None,
            bus_sub: None,
            hint: None,
            hint_seq: 0,
        };
        this.follow_project_changes(cx);
        this
    }

    /// 订阅 [`bus`]：主窗（或另一个悬浮窗）改了库，这一边重读一遍。
    ///
    /// 自己 emit 的那次也会收到（自己发自己收）—— 多读一遍库，换来的是**不必区分
    /// 「谁改的」**：写入路径只有一条，这里少一处会忘记同步的地方。
    fn follow_project_changes(&mut self, cx: &mut Context<Self>) {
        if self.bus_sub.is_some() {
            return;
        }
        let bus = bus::handle(cx);
        self.bus_sub = Some(cx.subscribe(
            &bus,
            |this: &mut Self, _bus, _event: &bus::ProjectChanged, cx| {
                this.state.reload();
                cx.notify();
            },
        ));
    }

    /// 弹一条底部回执。
    fn show_hint(
        &mut self,
        message: impl Into<SharedString>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) {
        self.hint_seq += 1;
        let seq = self.hint_seq;
        self.hint = Some((message.into(), is_error));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HINT_LIFETIME).await;
            let _ = this.update(cx, |this, cx| {
                // 序号守卫照 `pages/db` 的 toast：连点两次时第一条的定时器不该把
                // 第二条一起抹掉。
                if this.hint_seq != seq {
                    return;
                }
                this.hint = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开新增行（已经开着就只把焦点送进去）。
    fn begin_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = &self.draft {
            let draft = draft.clone();
            // `Focusable` 在作用域里才能这样取（`InputState` 是 `InputBaseState` 的别名）。
            window.focus(&draft.focus_handle(cx), cx);
            return;
        }
        self.capturing = true;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "任务标题")));
        self.draft_sub = Some(cx.subscribe(
            &input,
            |_this: &mut Self, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let entity = cx.entity();
                    crate::update::on_notify_window(cx, move |window, cx| {
                        entity.update(cx, |this, cx| this.submit_draft(window, cx));
                    });
                }
            },
        ));
        self.draft = Some(input.clone());
        // 焦点要在下一轮 effect 之后给（渲染期直接改焦点会被 gpui 挡下）。
        cx.defer_in(window, move |_this, window, cx| {
            window.focus(&input.focus_handle(cx), cx);
        });
        cx.notify();
    }

    /// 提交新增行。
    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .draft
            .as_ref()
            .map(|draft| draft.read(cx).value().trim().to_owned())
        else {
            return;
        };
        if title.is_empty() {
            return;
        }
        match self.state.create(&title) {
            Ok(()) => {
                if let Some(draft) = &self.draft {
                    draft.update(cx, |draft, cx| draft.set_value("", window, cx));
                }
                bus::notify(cx);
                self.show_hint(trf(cx, "已新建「{}」。", &[&title]), false, cx);
            }
            Err(error) => {
                self.show_hint(trf(cx, "新建失败：{}", &[&error.to_string()]), true, cx);
            }
        }
        cx.notify();
    }

    /// 收起新增行。
    fn end_capture(&mut self, cx: &mut Context<Self>) {
        self.capturing = false;
        self.draft = None;
        self.draft_sub = None;
        cx.notify();
    }

    /// 走一条状态变更（并广播）。
    fn apply_status(&mut self, id: i64, status: TaskStatus, cx: &mut Context<Self>) {
        match self.state.set_status(id, status) {
            Ok(()) => {
                // 广播给其它视图（主窗那一份）。自己也会收到一次并重读 —— 无害。
                bus::notify(cx);
            }
            Err(error) => {
                self.show_hint(trf(cx, "修改失败：{}", &[&error.to_string()]), true, cx);
            }
        }
        cx.notify();
    }

    /// 隐藏窗口。
    ///
    /// 是**隐藏**而不是销毁：窗口自己关自己（`remove_window`），下次唤起重新开一个
    /// —— 与命令面板同一条路。这样句柄会变陈旧，所以 `app::float_window_is_open`
    /// 一律用 `update(..).is_ok()` 判活（见 `app::Desktop::float_window` 的注释）。
    fn hide(&mut self, cx: &mut Context<Self>) {
        crate::app::close_task_float(cx);
    }
}

impl Render for FloatWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.bounds_watch.is_none() {
            // 拖过之后下次回到原位。`observe_window_bounds` 需要窗口，所以只能首帧挂
            // （与 `AuxiliaryWindowView` 挂几何观察、`PaletteWindow` 挂激活观察同一个理由）。
            self.bounds_watch = Some(cx.observe_window_bounds(window, |_this, window, cx| {
                crate::settings::save_task_float_bounds(window, cx);
            }));
        }

        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 浮窗自成一块卡片：用对话框圆角（比卡片大一档），四个角才看得出来。
        let radius = theme.components().dialog_radius;
        // 视图弱引用：渲染块函数只拿得到 `&App`，而 `App` 上没有 `entity()`。
        let view = cx.entity().downgrade();

        let head_doing = section_head(
            tr(cx, "进行中"),
            u32::try_from(self.state.doing().len()).unwrap_or(u32::MAX),
            extended.colors.info,
            cx,
        );
        let head_todo = section_head(
            tr(cx, "待办"),
            self.state.todos_total,
            tokens.colors.muted_foreground,
            cx,
        );

        let mut root = v_flex()
            .size_full()
            .overflow_hidden()
            .rounded(radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .child(grip(extended));

        if let Some(error) = self.state.error() {
            // 「绝不静默」：库打不开时退到内存库仍可用，但必须让用户看见这件事。
            root = root.child(error_line(error, cx));
        }

        root.child(head_doing)
            .child(
                div()
                    .flex_none()
                    .max_h(px(DOING_MAX_HEIGHT))
                    .id("task-float-doing")
                    .overflow_y_scroll()
                    .child(render_doing(self, &view, cx)),
            )
            .child(head_todo)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id("task-float-todo")
                    .overflow_y_scroll()
                    .child(render_todos(self, &view, cx)),
            )
            .children(self.capturing.then(|| draft_row(self, &view, cx)))
            .child(footer(self, &view, cx))
    }
}

// ── 渲染块 ──────────────────────────────────────────────────────────────────

/// 顶部抓取条。
///
/// 不接任何鼠标事件：它落在 macOS 隐形标题栏（约 28pt）的范围内，拖动由系统处理
/// （见 [`float_window_options`]）。这里只画那根 36×2 的小横条。
fn grip(extended: &ExtendedTokens) -> AnyElement {
    div()
        .flex_none()
        .h(px(GRIP_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(36.))
                .h(extended.stroke.strong)
                .rounded_full()
                .bg(extended.colors.hairline),
        )
        .into_any_element()
}

/// 库打不开时的提示行。
fn error_line(error: &str, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    div()
        .flex_none()
        .w_full()
        .px(tokens.spacing.sm)
        .pb(tokens.spacing.xs)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.destructive)
        .child(trf(cx, "打不开项目库，下面显示的是临时数据：{}", &[error]))
        .into_any_element()
}

/// 段头：状态点 + 名字 + 计数。
fn section_head(label: &'static str, count: u32, dot: gpui::Hsla, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex_none()
        .w_full()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .flex()
        .items_center()
        .gap(tokens.spacing.xs)
        // 尺寸从图标档位取半档，不写裸 px（档位随「设置 → 外观 → 字号」缩放）。
        .child(
            div()
                .size(extended.icon.sm / 2.)
                .flex_none()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .px(tokens.spacing.xs)
                .rounded_full()
                .bg(extended.colors.chrome)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(count.to_string())),
        )
        .into_any_element()
}

/// 「进行中」段。
fn render_doing(this: &FloatWindow, view: &WeakEntity<FloatWindow>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    if this.state.doing().is_empty() {
        return empty_line(tr(cx, "没有进行中的任务。"), cx);
    }
    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.sm)
        .pb(tokens.spacing.xs)
        .children(
            this.state
                .doing()
                .iter()
                .map(|task| float_card(this, task, view, cx)),
        )
        .into_any_element()
}

/// 「待办」段（含「展开剩余 N 个任务」与空态）。
fn render_todos(this: &FloatWindow, view: &WeakEntity<FloatWindow>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let mut list = v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .px(tokens.spacing.sm)
        .pb(tokens.spacing.sm)
        .children(
            this.state
                .todos()
                .iter()
                .map(|task| float_card(this, task, view, cx)),
        );
    let hidden = this.state.todos_collapsed();
    if hidden > 0 {
        // 虚线框：原型里这一行就是「还没展开」的视觉标记，实线会被当成一条任务。
        list = list.child(
            div()
                .id("task-float-expand")
                .w_full()
                .py(tokens.spacing.xs)
                .flex()
                .justify_center()
                .rounded(theme.components().card_radius)
                .border(extended.stroke.thin)
                .border_dashed()
                .border_color(extended.colors.hairline)
                .cursor_pointer()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .role(Role::Button)
                .aria_label(SharedString::from(trn(
                    cx,
                    "展开剩余 {} 个任务",
                    i64::from(hidden),
                )))
                .hover({
                    let hover = extended.colors.hover_tint;
                    move |style| style.bg(hover)
                })
                .child(SharedString::from(trn(
                    cx,
                    "展开剩余 {} 个任务",
                    i64::from(hidden),
                )))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.state.expand_todos();
                            cx.notify();
                        });
                    }
                }),
        );
    }
    if this.state.todos_total == 0 {
        list = list.child(empty_line(tr(cx, "没有待办任务。"), cx));
    }
    list.into_any_element()
}

/// 一行空态。
fn empty_line(message: &'static str, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .w_full()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.sm)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(tokens.colors.muted_foreground)
        .child(SharedString::from(message))
        .into_any_element()
}

/// 一张卡片；是当前展开项时改画详情。
fn float_card(
    this: &FloatWindow,
    task: &Task,
    view: &WeakEntity<FloatWindow>,
    cx: &App,
) -> AnyElement {
    if this.state.open() == Some(task.id) {
        return detail_card(this, task, view, cx);
    }
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let overdue = task.is_overdue(this.state.today());

    div()
        .id(SharedString::from(format!("task-float-card-{}", task.id)))
        .w_full()
        // `v_flex()` 是 cross-axis stretch，正好当整宽卡片用（`h_flex()` 会是居中）。
        .flex()
        .flex_col()
        .gap(tokens.spacing.xs)
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.surface)
        .cursor_pointer()
        .role(Role::Button)
        // 色点只有像素大小，读屏拿不到信息 ⇒ 名字里必须带上优先级。
        .aria_label(SharedString::from(format!(
            "{} · {}",
            task.title,
            priority_label(task.priority, cx)
        )))
        .hover({
            let hover = extended.colors.hover_tint;
            move |style| style.bg(hover)
        })
        .on_click({
            let view = view.clone();
            let id = task.id;
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.state.toggle_open(id);
                    cx.notify();
                });
            }
        })
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(task.title.clone())),
        )
        .when(task.progress > 0, |card| {
            card.child(task_progress(
                SharedString::from(format!("task-float-progress-{}", task.id)),
                task.progress,
                cx,
            ))
        })
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(priority_dot(task.priority, cx))
                .child(meta_text(
                    SharedString::from(format!("P{}", task.priority.as_i64())),
                    tokens.colors.muted_foreground,
                ))
                .when(task.status == TaskStatus::Blocked, |row| {
                    row.child(meta_text(tr(cx, "阻塞"), tokens.colors.destructive))
                })
                .when(overdue, |row| {
                    row.child(meta_text(tr(cx, "逾期"), tokens.colors.destructive))
                })
                .when_some(task.due_date, |row, due| {
                    row.child(meta_text(
                        SharedString::from(due.to_iso()),
                        tokens.colors.muted_foreground,
                    ))
                })
                .when(
                    task.due_date.is_none() && task.status != TaskStatus::Blocked && !overdue,
                    |row| row.child(meta_text(tr(cx, "未排期"), tokens.colors.muted_foreground)),
                ),
        )
        .into_any_element()
}

/// 就地展开的详情卡（原型画面 7 的第二屏）。
fn detail_card(
    this: &FloatWindow,
    task: &Task,
    view: &WeakEntity<FloatWindow>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let project = this.state.project_of(task);
    let mut card = div()
        .id(SharedString::from(format!("task-float-detail-{}", task.id)))
        .w_full()
        .flex()
        .flex_col()
        .gap(tokens.spacing.xs)
        .p(tokens.spacing.sm)
        .rounded(theme.components().card_radius)
        // 用焦点环色描边标记「这条是展开的」——与看板卡片的选中态同一个口径。
        .border(extended.stroke.strong)
        .border_color(tokens.colors.ring)
        .bg(tokens.colors.surface)
        .child(
            div()
                .w_full()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(task.title.clone())),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(meta_text(
                    SharedString::from(format!(
                        "P{} {}",
                        task.priority.as_i64(),
                        priority_label(task.priority, cx)
                    )),
                    tokens.colors.foreground,
                ))
                .child(neutral_tag(SharedString::from(task.quadrant.as_str()), cx))
                .children(project.map(|project| {
                    // 颜色是用户填的 hex，可能格式不对 ⇒ 回落到三级文字色（与
                    // `tag_chip` / 看板卡片同一条口径）。
                    let color =
                        color_from_hex(&project.color).unwrap_or(tokens.colors.muted_foreground);
                    div()
                        .flex()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .child(div().size(extended.icon.sm / 2.).rounded_full().bg(color))
                        .child(meta_text(
                            SharedString::from(project.name.clone()),
                            tokens.colors.muted_foreground,
                        ))
                })),
        );
    if task.progress > 0 {
        card = card.child(task_progress(
            SharedString::from(format!("task-float-detail-progress-{}", task.id)),
            task.progress,
            cx,
        ));
    }
    if !task.description_md.trim().is_empty() {
        card = card.child(
            div()
                .w_full()
                .pt(tokens.spacing.xs)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(SharedString::from(clip_detail(&task.description_md))),
        );
    }
    if task.subtasks.total > 0 {
        card = card.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(meta_text(
                    SharedString::from(format!("{}/{}", task.subtasks.done, task.subtasks.total)),
                    tokens.colors.muted_foreground,
                ))
                .child(meta_text(tr(cx, "子任务"), tokens.colors.muted_foreground)),
        );
    }
    card.child(action_row(task, view, cx)).into_any_element()
}

/// 详情里的动作行：开始 / 暂停 · 完成 · 返回。
///
/// 不发「编辑」：完整编辑器属 S5，摆一个点了没反应的按钮比不摆更糟（P3-6）。
fn action_row(task: &Task, view: &WeakEntity<FloatWindow>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    let id = task.id;
    let view = view.clone();
    // 推进的方向由当前状态决定：待办 / 搁置 → 「开始」，其余（进行中 / 阻塞）→ 「暂停」。
    let (advance_label, advance_status) = match task.status {
        TaskStatus::Todo | TaskStatus::Deferred => (tr(cx, "开始"), TaskStatus::Doing),
        _ => (tr(cx, "暂停"), TaskStatus::Todo),
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(tokens.spacing.xs)
        .child(
            small_button(
                "task-float-advance",
                advance_label,
                DuckIcon::Play,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.apply_status(id, advance_status, cx));
                }
            })
            .into_any_element(),
        )
        .child(
            small_button("task-float-done", tr(cx, "完成"), DuckIcon::Check, true, cx).on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.apply_status(id, TaskStatus::Done, cx));
                }
            }),
        )
        .child(
            small_button(
                "task-float-back",
                tr(cx, "返回"),
                DuckIcon::ChevronLeft,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.state.close_detail();
                        cx.notify();
                    });
                }
            }),
        )
        .into_any_element()
}

/// 底部操作条：新增 / 主窗口 / 刷新 / 隐藏。
fn footer(this: &FloatWindow, view: &WeakEntity<FloatWindow>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let mut bar = div()
        .flex_none()
        .w_full()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .flex()
        .items_center()
        .gap(tokens.spacing.xs)
        .border_t(extended.stroke.thin)
        .border_color(extended.colors.hairline);

    // 有回执时它占满一条（四个按钮退到右边）；没有就放一个撑开的占位。
    match &this.hint {
        Some((message, is_error)) => {
            let color = if *is_error {
                tokens.colors.destructive
            } else {
                extended.colors.success
            };
            bar = bar.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(color)
                    .child(message.clone()),
            );
        }
        None => bar = bar.child(div().flex_1()),
    }

    bar.child(
        footer_button("task-float-new", tr(cx, "新增"), DuckIcon::Plus, cx)
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.begin_capture(window, cx));
                }
            })
            .into_any_element(),
    )
    .child(
        footer_button(
            "task-float-main",
            tr(cx, "主窗口"),
            DuckIcon::ExternalLink,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                // 抬主窗 + 跳到项目管理页：从「随手看」点过去，目的就是看那一条。
                let _ = view.update(cx, |this, cx| {
                    crate::app::reveal_main_window(cx);
                    // 主窗还没开时 `reveal_main_window` 会**同步**建好视图，
                    // 所以紧接着的跳转能落到地方（见 `app::open_main_window`）。
                    crate::app::navigate_to(cx, crate::pages::PROJECTS_ROUTE);
                    // 自己收起来 —— 主窗已经把这一页摊开了，留着小窗只会挡路。
                    this.hide(cx);
                });
            }
        })
        .into_any_element(),
    )
    .child(
        footer_button("task-float-refresh", tr(cx, "刷新"), DuckIcon::RotateCw, cx)
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.state.reload();
                        cx.notify();
                    });
                }
            })
            .into_any_element(),
    )
    .child(
        footer_button("task-float-hide", tr(cx, "隐藏"), DuckIcon::X, cx)
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.hide(cx));
                }
            })
            .into_any_element(),
    )
    .into_any_element()
}

/// 新增行：一行输入 + 「取消」。
fn draft_row(this: &FloatWindow, view: &WeakEntity<FloatWindow>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let Some(draft) = &this.draft else {
        return div().into_any_element();
    };
    div()
        .flex_none()
        .w_full()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .flex()
        .items_center()
        .gap(tokens.spacing.xs)
        .border_t(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(draft).appearance(false).bordered(false).w_full()),
        )
        .child(
            footer_button("task-float-cancel", tr(cx, "取消"), DuckIcon::X, cx).on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.end_capture(cx));
                }
            }),
        )
        .into_any_element()
}

// ── 小构件 ──────────────────────────────────────────────────────────────────

/// 卡片元信息里的一段文字（11px）。字号小于 `typography.xs`(12) 一档 ——
/// 与 `duck_components::kit::meta_text` 的取舍相同，但这里要按语义换色。
fn meta_text(text: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .text_size(px(11.))
        .line_height(px(14.))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// 底条上的按钮（一律非首选样式）。
fn footer_button(
    id: &'static str,
    label: &'static str,
    icon: DuckIcon,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    small_button(id, label, icon, false, cx)
}

/// 紧凑按钮的公共骨架。
///
/// 高度取 `spacing.xl`(24) 而不是密度档 `control`：一行里塞四个按钮，
/// 用控件高度会把底条撑到窗口的 1/10。
fn small_button(
    id: &'static str,
    label: &'static str,
    icon: DuckIcon,
    primary: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let (bg, fg) = if primary {
        (tokens.colors.primary, tokens.colors.primary_foreground)
    } else {
        (extended.colors.chrome, tokens.colors.foreground)
    };
    let hover = extended.colors.hover_tint;
    div()
        .id(id)
        .flex_1()
        .h(tokens.spacing.xl)
        .flex()
        .items_center()
        .justify_center()
        .gap(tokens.spacing.xxs)
        .rounded(theme.components().button_radius)
        .bg(bg)
        .text_size(tokens.typography.xs.size)
        .line_height(tokens.typography.xs.line_height)
        .text_color(fg)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(label))
        .hover(move |style| style.bg(hover))
        .child(Icon::new(icon).size(extended.icon.sm))
        .child(SharedString::from(label))
}

/// 详情里的描述：压平空白 + 按**字符**截断。
///
/// 按字符而不是字节：中文一个字三字节，按字节切会把一个字劈成两个乱码
/// （由 `clipping_the_detail_counts_characters_not_bytes` 守着）。
fn clip_detail(markdown: &str) -> String {
    // 300pt 宽的窗里没有排版 markdown 的余地，先把换行与连续空白压成单个空格。
    let text: String = markdown.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= DETAIL_CHARS {
        return text;
    }
    let head: String = text.chars().take(DETAIL_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    /// 一个只认数据的空状态（`store: None`）。
    fn state(todos_total: u32) -> FloatState {
        FloatState {
            store: None,
            error: None,
            doing: Vec::new(),
            todos: Vec::new(),
            todos_total,
            expanded: false,
            open: None,
            projects: Vec::new(),
            today: today(),
        }
    }

    /// 待办折叠的算术：显示了 `todos.len()` 条、总数 `todos_total`，剩下的才是「展开剩余 N」。
    ///
    /// 这条容易写成 `todos_total - TODO_PREVIEW`（错的）：总数是**库里顶层待办的条数**，
    /// 折叠只是"没全取回来"，只有减「实际查回来的条数」才对得上屏上看到的东西
    /// （总数为什么不能改用 `board_stats().todo` 见 [`the_todo_total_counts_top_level_tasks_only`]）。
    #[test]
    fn the_collapsed_count_tracks_what_is_left_to_show() {
        let mut state = state(9);
        assert_eq!(state.todos_collapsed(), 9, "一条都还没显示");
        // 查回来 5 条 → 还剩 4（原型「展开剩余 4 个任务」）。
        state.todos = vec![task(1), task(2), task(3), task(4), task(5)];
        assert_eq!(state.todos_collapsed(), 4);
        // 展开了就没有「剩余」这一行（哪怕总数更大）。
        state.expanded = true;
        assert_eq!(state.todos_collapsed(), 0);
        // 总数比显示的少（另一处刚改完库、还没重读）时也不能下溢。
        state.expanded = false;
        state.todos_total = 3;
        assert_eq!(state.todos_collapsed(), 0);
    }

    /// 展开态是**单向**的：`expand_todos` 再调一次不该把状态翻回去
    /// （翻回去等于每次重读都掉回 5 条）。
    #[test]
    fn expanding_todos_is_one_way() {
        let mut state = state(0);
        state.expand_todos();
        assert!(state.expanded, "展开是单向的，点一次就该立住");
        assert_eq!(state.todos_collapsed(), 0);
    }

    /// 详情只挂在「还在列表里」的那条任务上 —— 真库跑一遍：
    /// 建一条 → 展开它 → 改成「已完成」→ 它离开待办段，展开态必须跟着放下。
    #[test]
    fn the_open_detail_is_dropped_when_its_task_leaves_the_list() {
        let store = ProjectStore::open_in_memory().expect("in-memory sqlite");
        let mut state = FloatState {
            store: Some(store),
            error: None,
            doing: Vec::new(),
            todos: Vec::new(),
            todos_total: 0,
            expanded: false,
            open: None,
            projects: Vec::new(),
            today: today(),
        };
        state.reload();
        state.create("写周报").expect("建任务");
        let id = state.todos().first().expect("刚落进待办").id;
        assert_eq!(state.todos_collapsed(), 0, "只有一条，没有剩余");
        state.toggle_open(id);
        assert_eq!(state.open(), Some(id));
        // 完成它 → 离开待办段 → `reload` 收尾那一段把它放下。
        state.set_status(id, TaskStatus::Done).expect("改状态");
        assert!(
            !state.todos().iter().any(|task| task.id == id),
            "已完成的任务不该还在待办段"
        );
        assert_eq!(state.open(), None, "详情该跟着收起");
    }

    /// 「展开剩余 N」的总数**必须与列表同源**（都是顶层任务）。
    ///
    /// 反例：拿 `board_stats().todo` 当总数 —— 那个口径只按 `archived_at` 过滤、
    /// **含子任务**。于是有待办子任务时总数偏大，「展开剩余 N」多算，点下去一条不多、
    /// 那一行自己消失。真库跑一遍：6 条顶层待办 + 2 条待办子任务。
    #[test]
    fn the_todo_total_counts_top_level_tasks_only() {
        let store = ProjectStore::open_in_memory().expect("in-memory sqlite");
        let mut parent = 0;
        for index in 1..=6 {
            let id = store
                .create_task(NewTask::new(format!("顶层 {index}")))
                .expect("建顶层任务");
            parent = parent.max(id);
        }
        for index in 1..=2 {
            store
                .create_task(NewTask::new(format!("子任务 {index}")).parent(parent))
                .expect("建子任务");
        }
        let mut state = FloatState {
            store: Some(store),
            error: None,
            doing: Vec::new(),
            todos: Vec::new(),
            todos_total: 0,
            expanded: false,
            open: None,
            projects: Vec::new(),
            today: today(),
        };
        state.reload();
        assert_eq!(state.todos_total, 6, "子任务不计入（这一段只列顶层）");
        assert_eq!(state.todos().len(), TODO_PREVIEW, "折叠时只显示前几条");
        assert_eq!(state.todos_collapsed(), 1, "点开确实还会多出一条");
        state.expand_todos();
        state.reload();
        assert_eq!(state.todos().len(), 6, "展开后 6 条顶层全在");
        assert_eq!(state.todos_collapsed(), 0, "一条不剩，那一行该消失");
    }

    /// 描述截断按**字符**而不是字节（中文按字节切会把一个字劈成两个乱码）。
    #[test]
    fn clipping_the_detail_counts_characters_not_bytes() {
        let short = "短描述";
        assert_eq!(clip_detail(short), short, "短文本原样返回");

        let long = "描".repeat(DETAIL_CHARS + 10);
        let clipped = clip_detail(&long);
        assert_eq!(
            clipped.chars().count(),
            DETAIL_CHARS + 1,
            "截到 DETAIL_CHARS 个字符再加一个省略号"
        );
        assert!(clipped.ends_with('…'));
        // 按字节切会得到 DETAIL_CHARS 个**字节**，字符数只剩三分之一。
        assert!(clipped.chars().all(|ch| ch == '描' || ch == '…'));
    }

    /// 换行与连续空白要压成单个空格：详情卡里没有排版 markdown 的余地。
    #[test]
    fn clipping_the_detail_flattens_whitespace() {
        assert_eq!(
            clip_detail("第一行\n\n  第二行\t第三行"),
            "第一行 第二行 第三行"
        );
    }

    fn task(id: i64) -> Task {
        Task {
            id,
            project_id: None,
            parent_id: None,
            title: format!("任务 {id}"),
            description_md: String::new(),
            status: TaskStatus::Todo,
            priority: duck_project::Priority::P2,
            quadrant: duck_project::Quadrant::Q2,
            start_date: None,
            due_date: None,
            due_time: None,
            completed_at: None,
            progress: 0,
            estimate_hours: None,
            actual_hours: None,
            sort_order: 0,
            created_at: 0,
            updated_at: 0,
            archived_at: None,
            archived_root: None,
            tags: Vec::new(),
            subtasks: duck_project::SubtaskProgress::default(),
            starred: false,
            block_reason: String::new(),
            note: String::new(),
        }
    }
}
