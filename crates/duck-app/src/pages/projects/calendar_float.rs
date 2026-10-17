//! 日历浮窗 —— 项目管理页的第三个窗口形态（`docs/projects-design.md` §4.3 ②）。
//!
//! 与「随手看」同一条路子（见 [`super::float`] 的模块头）：独立无边框小窗、**置顶**、
//! **不抢焦点**、不占 Dock、自己开一条 `duck-project` 连接、订阅 [`super::bus`] 跟着重读。
//! 区别只在一件事上：这一窗回答的是「**哪天有什么**」—— 月历格子 + 选中那天的任务清单。
//!
//! # 刻意的取舍
//!
//! 1. **周从周一开始**（ISO 周）：页内日历的周视图、周计划档都按 ISO 周（`start_of_week`
//!    是周一），浮窗若是「周日开头」就会与主窗排出两套不同的行，同一份任务在两个窗里
//!    落在不同的列。
//! 2. **格子外的日期画成空的**，不画上 / 下月的溢出日：300pt 宽的格子塞不下
//!    「10 月 31 日 / 11 月 1 日」两行任务；要看的月份切过去就是。
//! 3. **不接写动作**：任务清单只读，改状态 / 编辑都回主窗（与「随手看」详情的口径一致
//!    —— 小窗只做「瞄一眼」；写入口多一处就要多养一套回执与失败提示）。
//!
//! 几何落 `windows` 表（`settings::restore_calendar_float`），键位 `⌥⌘C`（`HotkeySlot::CalendarFloat`）。

use std::collections::HashMap;

use gpui::{
    App, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, point, px, size,
};
use gpui_component::{Icon, v_flex};

use duck_components::DuckIcon;
use duck_components::selectable_text;
use duck_project::{CalendarDate, Project, ProjectStore, Task, TaskQuery, Weekday};
use duck_theme::{ExtendedTokens, active_theme, color_from_hex, tr, trf};

use super::bus;
use super::state::local_today;

/// 窗口尺寸：6 行 7 列的月历格 + 底部清单。不可缩放（照「随手看」的口径）。
const CALENDAR_FLOAT_SIZE: gpui::Size<gpui::Pixels> = size(px(320.), px(430.));
/// 首次出现时距离屏幕边缘的留白（与「随手看」一致）。
const SCREEN_MARGIN: f32 = 24.;
/// 顶部抓取条高度（macOS 隐形标题栏盖住它，见 `float::float_window_options`）。
const GRIP_HEIGHT: f32 = 26.;
/// 选中日的清单最多列几条；剩下的折成一句「还有 N 条」。
const DAY_LIST_PREVIEW: usize = 4;

/// 窗口选项：与「随手看」同一套 —— `PopUp`、透明底、不抢焦点、可拖动。
#[must_use]
pub(crate) fn calendar_float_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(calendar_float_bounds(cx)),
        titlebar: None,
        kind: gpui::WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        is_movable: true,
        app_owns_titlebar_drag: false,
        window_background: WindowBackgroundAppearance::Transparent,
        focus: false,
        show: true,
        ..WindowOptions::default()
    }
}

/// 几何：上次待的地方，没有存档就落在主屏右上角（「随手看」的下方一点）。
#[must_use]
pub(crate) fn calendar_float_bounds(cx: &App) -> WindowBounds {
    if let Some(origin) = crate::settings::restore_calendar_float(cx) {
        return WindowBounds::Windowed(Bounds::new(origin, CALENDAR_FLOAT_SIZE));
    }
    let Some(display) = cx.primary_display() else {
        return WindowBounds::centered(CALENDAR_FLOAT_SIZE, cx);
    };
    let screen = display.bounds();
    WindowBounds::Windowed(Bounds::new(
        point(
            screen.origin.x + screen.size.width - CALENDAR_FLOAT_SIZE.width - px(SCREEN_MARGIN),
            screen.origin.y + px(SCREEN_MARGIN) + px(GRIP_HEIGHT + 300.),
        ),
        CALENDAR_FLOAT_SIZE,
    ))
}

/// 月历网格：**周一起头**的 6×7 = 42 格，本月之外的格子是 `None`。
///
/// 纯函数（可单测）：`2026-10-01` 是周四 ⇒ 头三个格子是 `None`。
#[must_use]
pub(crate) fn month_grid(year: i32, month: u32) -> Vec<Option<CalendarDate>> {
    let first = CalendarDate::new(year, month, 1);
    let Some(first) = first else {
        return Vec::new();
    };
    let offset = first.weekday().index_from_monday();
    let grid_start = first.add_days(-offset);
    (0..42)
        .map(|index| {
            let date = grid_start.add_days(index);
            (date.year() == year && date.month() == month).then_some(date)
        })
        .collect()
}

/// 月份步进：`1..12` 之外由 `CalendarDate::new` 的校验兜底（调用方只传合法月份）。
#[must_use]
pub(crate) fn step_month(year: i32, month: u32, step: i32) -> (i32, u32) {
    let total = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(step);
    let new_year = total.div_euclid(12);
    let new_month = total.rem_euclid(12) as u32 + 1;
    (i32::try_from(new_year).unwrap_or(year), new_month)
}

/// 浮窗的数据。不实现 `Render`（三件套纪律）。
pub(crate) struct CalendarFloatState {
    /// 内核。打不开时为 `None`，此时 [`Self::error`] 有值。
    store: Option<ProjectStore>,
    /// 打开内核失败的原因（英文，直接来自内核）。
    error: Option<String>,
    /// 正在看的月份（锚定日只存年 / 月，日永远取 1）。
    year: i32,
    month: u32,
    /// 选中看清单的那天。
    selected: Option<CalendarDate>,
    /// 有截止日的顶层未归档任务，按截止日归组（键 = `days_since_epoch`）。
    by_date: HashMap<i64, Vec<Task>>,
    /// 项目表（清单行显示项目色点）。
    projects: Vec<Project>,
    /// 算「今天」用的本地日期。
    today: CalendarDate,
}

impl CalendarFloatState {
    /// 建状态并读第一屏。锚定在**今天**所在的月份。
    pub(crate) fn new() -> Self {
        let (store, error) = Self::open_store();
        let today = local_today();
        let mut state = Self {
            store,
            error,
            year: today.year(),
            month: today.month(),
            selected: Some(today),
            by_date: HashMap::new(),
            projects: Vec::new(),
            today,
        };
        state.reload();
        state
    }

    /// 打开内核。失败时退到内存库（界面仍可用，但绝不静默）—— 与「随手看」同一条口径。
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

    /// 重读：把**全部带截止日的顶层未归档任务**拉回来按日归组。
    ///
    /// 一次读全部而不是按月查：个人库的量级撑得住，换来切月份不用再打库
    /// （月份切换是纯内存操作，见 [`Self::step`]）。
    pub(crate) fn reload(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        self.today = local_today();
        match store.list_projects(duck_project::ArchiveScope::Live) {
            Ok(projects) => self.projects = projects,
            Err(err) => return self.fail(err),
        }
        let tasks = store.list_tasks(&TaskQuery::new().top_level_only());
        let tasks = match tasks {
            Ok(tasks) => tasks,
            Err(err) => return self.fail(err),
        };
        let mut by_date: HashMap<i64, Vec<Task>> = HashMap::new();
        for task in tasks {
            let Some(due) = task.due_date else {
                continue;
            };
            by_date
                .entry(due.days_since_epoch())
                .or_default()
                .push(task);
        }
        self.by_date = by_date;
        // 选中的那天可能已经不在当前月（切换了月份），保留它没问题 —— 清单按
        // 「选中的日期」取数，与网格哪个高亮无关。
    }

    /// 收敛一次读失败。
    fn fail(&mut self, error: duck_project::ProjectError) {
        tracing::warn!(%error, "calendar float could not read the project store");
        self.error = Some(error.to_string());
    }

    /// 当前月份标签的年 / 月。
    pub(crate) const fn month_anchor(&self) -> (i32, u32) {
        (self.year, self.month)
    }

    /// 当前网格（42 格）。
    pub(crate) fn grid(&self) -> Vec<Option<CalendarDate>> {
        month_grid(self.year, self.month)
    }

    /// 选中的那天。
    pub(crate) const fn selected(&self) -> Option<CalendarDate> {
        self.selected
    }

    /// 今天。
    pub(crate) const fn today(&self) -> CalendarDate {
        self.today
    }

    /// 读错误（英文原文；界面文案由调用方组）。
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 某天的任务（按截止日升序排列，同日的按内核给的顺序）。
    pub(crate) fn tasks_on(&self, date: CalendarDate) -> Vec<&Task> {
        self.by_date
            .get(&date.days_since_epoch())
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .collect()
    }

    /// 选中某天（再点同一天是取消选中）。
    pub(crate) fn select(&mut self, date: CalendarDate) {
        self.selected = if self.selected == Some(date) {
            None
        } else {
            Some(date)
        };
    }

    /// 切月份：`step` 为 `-1` / `1`。月份变了选中日跟着清掉 —— 留着会让人以为
    /// 「格子高亮的还是我点的那天」，其实那天的清单已经换月了。
    pub(crate) fn step(&mut self, step: i32) {
        let (year, month) = step_month(self.year, self.month, step);
        self.year = year;
        self.month = month;
        if self
            .selected
            .is_some_and(|date| date.year() != year || date.month() != month)
        {
            self.selected = None;
        }
    }

    /// 回到今天所在月份并选中今天。
    pub(crate) fn jump_to_today(&mut self) {
        self.year = self.today.year();
        self.month = self.today.month();
        self.selected = Some(self.today);
    }

    /// 某条任务所属项目。
    pub(crate) fn project_of(&self, task: &Task) -> Option<&Project> {
        let id = task.project_id?;
        self.projects.iter().find(|project| project.id == id)
    }
}

/// 日历浮窗视图。
pub struct CalendarFloatWindow {
    state: CalendarFloatState,
    /// 窗口位置观察（首帧挂，见 `Render::render`）。
    bounds_watch: Option<Subscription>,
    /// 变更广播的订阅。
    bus_sub: Option<Subscription>,
}

impl CalendarFloatWindow {
    /// 建窗。
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: CalendarFloatState::new(),
            bounds_watch: None,
            bus_sub: None,
        };
        // 与「随手看」同一条：自己 emit 的也会收到，多读一遍库换「不必区分谁改的」。
        let bus = bus::handle(cx);
        this.bus_sub = Some(cx.subscribe(
            &bus,
            |this: &mut Self, _bus, _event: &bus::ProjectChanged, cx| {
                this.state.reload();
                cx.notify();
            },
        ));
        this
    }
}

impl Render for CalendarFloatWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.bounds_watch.is_none() {
            self.bounds_watch = Some(cx.observe_window_bounds(window, |_this, window, cx| {
                crate::settings::save_calendar_float_bounds(window, cx);
            }));
        }

        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let radius = theme.components().dialog_radius;
        let view = cx.entity().downgrade();

        let (year, month) = self.state.month_anchor();
        let (year_text, month_text) = (year.to_string(), month.to_string());

        let mut root = v_flex()
            .size_full()
            .overflow_hidden()
            .rounded(radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            .child(grip(extended));

        if let Some(error) = self.state.error() {
            root = root.child(
                div()
                    .flex_none()
                    .w_full()
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(tokens.colors.destructive)
                    .child(selectable_text(trf(
                        cx,
                        "打不开项目库，下面显示的是临时数据：{}",
                        &[error],
                    ))),
            );
        }

        // ── 月头：上一个月 / 年月 / 回到今天 / 下一个月 / 关闭 ──────────────
        let head = div()
            .flex_none()
            .w_full()
            .px(tokens.spacing.sm)
            .pb(tokens.spacing.xs)
            .flex()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                nav_button("calendar-float-prev", DuckIcon::ChevronLeft, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.state.step(-1);
                            cx.notify();
                        });
                    }
                }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .line_height(tokens.typography.sm.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(selectable_text(trf(
                        cx,
                        "{}年{}月",
                        &[&year_text, &month_text],
                    ))),
            )
            .child(
                nav_button("calendar-float-today", DuckIcon::Calendar, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.state.jump_to_today();
                            cx.notify();
                        });
                    }
                }),
            )
            .child(
                nav_button("calendar-float-next", DuckIcon::ChevronRight, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.state.step(1);
                            cx.notify();
                        });
                    }
                }),
            )
            .child(
                nav_button("calendar-float-close", DuckIcon::X, cx).on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |_this, cx| {
                            crate::app::close_calendar_float(cx);
                        });
                    }
                }),
            );

        // ── 星期表头（周一起头，与页内周视图同一条口径）────────────────────
        let weekday_row = div()
            .flex_none()
            .w_full()
            .px(tokens.spacing.xs)
            .flex()
            .children(
                ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
                    .iter()
                    .map(|label| {
                        div()
                            .flex_1()
                            .text_center()
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(tr(cx, label)))
                    }),
            );

        // ── 月历格 ─────────────────────────────────────────────────────────
        let today = self.state.today();
        let grid = v_flex()
            .flex_none()
            .w_full()
            .px(tokens.spacing.xs)
            .gap(tokens.spacing.xxs)
            .children(
                self.state
                    .grid()
                    .chunks(7)
                    .map(|week| {
                        div()
                            .w_full()
                            .flex()
                            .gap(tokens.spacing.xxs)
                            .children(week.iter().map(|cell| {
                                day_cell(
                                    self,
                                    &view,
                                    *cell,
                                    today,
                                    today.weekday() == Weekday::Saturday
                                        || today.weekday() == Weekday::Sunday,
                                    cx,
                                )
                            }))
                    })
                    .collect::<Vec<_>>(),
            );

        // ── 选中日的清单 ───────────────────────────────────────────────────
        let list = day_list(self, cx);

        root.child(head)
            .child(weekday_row)
            .child(grid)
            .child(div().flex_1().min_h_0())
            .child(list)
    }
}

// ── 渲染块 ──────────────────────────────────────────────────────────────────

/// 顶部抓取条（与「随手看」同款：不接事件，拖动归 macOS 隐形标题栏）。
fn grip(extended: &ExtendedTokens) -> gpui::AnyElement {
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

/// 月头上的小图标按钮。
fn nav_button(id: &'static str, icon: DuckIcon, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hover = extended.colors.hover_tint;
    div()
        .id(id)
        .size(tokens.spacing.xl)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme.components().button_radius)
        .text_color(tokens.colors.muted_foreground)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(icon_label(icon, cx)))
        .hover(move |style| style.bg(hover))
        .child(Icon::new(icon).size(extended.icon.sm))
}

/// 导航按钮的读屏名（图标本身带不走语义）。
fn icon_label(icon: DuckIcon, cx: &App) -> &'static str {
    match icon {
        DuckIcon::ChevronLeft => tr(cx, "上一个月"),
        DuckIcon::ChevronRight => tr(cx, "下一个月"),
        DuckIcon::Calendar => tr(cx, "回到今天"),
        DuckIcon::X => tr(cx, "关闭"),
        _ => "",
    }
}

/// 一格日期。空格（本月之外）不接事件。
fn day_cell(
    this: &CalendarFloatWindow,
    view: &WeakEntity<CalendarFloatWindow>,
    date: Option<CalendarDate>,
    today: CalendarDate,
    _today_is_weekend: bool,
    cx: &App,
) -> gpui::AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let Some(date) = date else {
        return div().flex_1().min_h(px(38.)).into_any_element();
    };
    let selected = this.state.selected() == Some(date);
    let is_today = date == today;
    let tasks = this.state.tasks_on(date);
    let done_count = tasks.iter().filter(|task| task.status.is_settled()).count();

    // 点数条：最多画 3 个，先画未完成的（这一窗回答「哪天还有什么」）。
    let mut dots = div().flex().items_center().justify_center().gap(px(2.));
    for task in tasks.iter().take(3 - done_count.min(2)) {
        let color = match task.priority {
            duck_project::Priority::P0 | duck_project::Priority::P1 => tokens.colors.destructive,
            _ => extended.colors.info,
        };
        dots = dots.child(div().size(px(4.)).rounded_full().bg(color));
    }

    let mut cell = div()
        .flex_1()
        .min_h(px(38.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(2.))
        .py(px(2.))
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
        });

    if date == today {
        // 今天的日子号用主题色描一圈（选中态是整格反色，两者不冲突）。
        cell = cell
            .border(extended.stroke.strong)
            .border_color(tokens.colors.ring);
    }

    let day_text = date.day().to_string();
    cell = cell
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(if is_today {
                    tokens.colors.foreground
                } else {
                    tokens.colors.muted_foreground
                })
                .child(SharedString::from(day_text)),
        )
        .child(dots);

    let id = SharedString::from(format!("calendar-float-day-{}", date.to_iso()));
    div()
        .id(id)
        .flex_1()
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(format!(
            "{}{}",
            date.to_iso(),
            if tasks.is_empty() {
                String::new()
            } else {
                trf(cx, "，{} 条任务", &[&tasks.len().to_string()])
            }
        )))
        .child(cell)
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.state.select(date);
                    cx.notify();
                });
            }
        })
        .into_any_element()
}

/// 选中那天的任务清单（只读；没有选中就给一句说明）。
fn day_list(this: &CalendarFloatWindow, cx: &App) -> gpui::AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let Some(selected) = this.state.selected() else {
        return div()
            .flex_none()
            .w_full()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .text_size(tokens.typography.xs.size)
            .line_height(tokens.typography.xs.line_height)
            .text_color(tokens.colors.muted_foreground)
            .child(selectable_text(tr(cx, "点一个月历格看那天的任务。")))
            .into_any_element();
    };

    let tasks = this.state.tasks_on(selected);
    let date_text = selected.to_iso();
    let mut rows = v_flex()
        .id("calendar-float-day-list")
        .flex_none()
        .w_full()
        .max_h(px(120.))
        .overflow_y_scroll()
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .gap(tokens.spacing.xxs)
        .border_t(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(trf(
                    cx,
                    "{} · {} 条任务",
                    &[&date_text, &tasks.len().to_string()],
                ))),
        );
    if tasks.is_empty() {
        rows = rows.child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(tr(cx, "这天没有截止的任务。"))),
        );
    }
    for task in tasks.iter().take(DAY_LIST_PREVIEW) {
        let project = this.state.project_of(task);
        let color = project
            .and_then(|project| color_from_hex(&project.color))
            .unwrap_or(tokens.colors.muted_foreground);
        let title = if task.status.is_settled() {
            trf(cx, "✓ {}", &[&task.title])
        } else {
            task.title.clone()
        };
        rows = rows.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(div().size(px(5.)).flex_none().rounded_full().bg(color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if task.status.is_settled() {
                            tokens.colors.muted_foreground
                        } else {
                            tokens.colors.foreground
                        })
                        .child(selectable_text(SharedString::from(title))),
                ),
        );
    }
    let hidden = tasks.len().saturating_sub(DAY_LIST_PREVIEW);
    if hidden > 0 {
        let hidden_text = hidden.to_string();
        rows = rows.child(
            div()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(trf(cx, "还有 {} 条……", &[&hidden_text]))),
        );
    }
    rows.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> CalendarDate {
        CalendarDate::parse(text).expect("合法日期")
    }

    /// 网格的形状与对齐：42 格、第 0 列永远是周一、本月的每一天恰好出现一次。
    #[test]
    fn the_grid_is_monday_first_and_covers_the_month_once() {
        let grid = month_grid(2026, 10);
        assert_eq!(grid.len(), 42);
        // 2026-10-01 是周四 ⇒ 头三格是空（周一二三），第 3 格（周四）起才是 10 月 1 日。
        assert_eq!(grid[0], None);
        assert_eq!(grid[2], None);
        assert_eq!(grid[3], Some(date("2026-10-01")));
        // 本月 31 天都在，且各自只出现一次。
        for day in 1..=31 {
            let needle = CalendarDate::new(2026, 10, day).expect("合法日期");
            assert_eq!(
                grid.iter().filter(|cell| **cell == Some(needle)).count(),
                1,
                "10 月 {day} 日恰好出现一次"
            );
        }
        // 尾部也补满整周：11 月的溢出日一律画成空格。
        assert_eq!(grid[41], None);
    }

    /// 月首就是周一（2026-06-01）时，第 0 格**不是** None，也没有多余的前置空格。
    #[test]
    fn a_month_starting_on_monday_leaves_no_leading_blanks() {
        let grid = month_grid(2026, 6);
        assert_eq!(grid[0], Some(date("2026-06-01")));
        assert_eq!(grid[0].expect("第一格").weekday().index_from_monday(), 0);
    }

    /// 月份步进跨年：12 月 +1 → 明年 1 月；1 月 -1 → 去年 12 月。
    #[test]
    fn stepping_months_crosses_year_boundaries() {
        assert_eq!(step_month(2026, 12, 1), (2027, 1));
        assert_eq!(step_month(2026, 1, -1), (2025, 12));
        assert_eq!(step_month(2026, 10, 0), (2026, 10));
    }

    /// 选中日与网格是**独立**的两件事：切月份清掉选中（否则清单还挂着上个月的那天，
    /// 界面上却找不到高亮）。
    #[test]
    fn stepping_months_drops_a_selection_from_another_month() {
        let mut state = CalendarFloatState {
            store: None,
            error: None,
            year: 2026,
            month: 10,
            selected: Some(date("2026-10-14")),
            by_date: HashMap::new(),
            projects: Vec::new(),
            today: date("2026-10-14"),
        };
        state.step(1);
        assert_eq!(state.month_anchor(), (2026, 11));
        assert_eq!(state.selected(), None, "跨月后旧选中日该被放下");

        // 同月内的选中不受影响。
        state.select(date("2026-11-20"));
        state.step(-1);
        state.step(1);
        assert_eq!(
            state.selected(),
            None,
            "来回切换同样清掉（那天已不在网格里）"
        );
    }
}
