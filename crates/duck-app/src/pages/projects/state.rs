//! 项目管理页的状态层。
//!
//! 本文件**不实现 `Render`** —— 项目树、看板列、任务集合、选中项与全部筛选 / 统计
//! 口径都住在这里，因此可以脱离窗口单测：同一份库喂进去，结果必须逐字一致。视图在
//! `view.rs`，装配在 `mod.rs`。
//!
//! 对 gpui 的接触面只有一处：[`ProjectsState::new`] 收一个 `&App` 去读 UI 偏好
//! （`kv: projects_ui`，见 [`prefs`]）—— 那是「读一次就够」的全局状态，单测里用
//! 内存库自建状态即可绕过（见测试里的 `memory_state`）。除此之外本文件不认识 gpui。
//!
//! 时间口径：内核的 [`CalendarDate`] 只认 UTC 秒（日期是日历事实，见
//! `docs/projects-design.md` §9 差异表第 7 条），所以「今天」必须在这里由
//! [`local_today`] 加上平台层给的时区偏移算出，内核不知道时区。

use std::collections::HashMap;

use duck_project::{
    ArchiveCounts, ArchivedProject, ArchivedTask, BoardColumn, BoardStats, CalendarDate,
    ClearScope, DueFilter, Group, Project, ProjectStore, Tag, Task, TaskCounts, TaskPatch,
    TaskQuery, TaskSort, TaskStatus, now_seconds,
};
use gpui::App;

use super::prefs::{self, ViewMode};

/// 左栏选中的范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BoardScope {
    /// 全部项目 + 收集箱。
    #[default]
    All,
    /// 只收集箱（未归属任何项目的任务）。
    Inbox,
    /// 单个项目。
    Project(i64),
}

impl BoardScope {
    /// 左栏高亮用：这个项目是不是当前选中项。
    pub(crate) fn is_project(self, id: i64) -> bool {
        matches!(self, Self::Project(current) if current == id)
    }
}

/// 左栏「智能列表」的四档（设计文档 §5.2 的口径表；顺序与原型左栏一致）。
///
/// **单选**，不是多选：内核的 [`TaskQuery::due`] 是单值（一次只能挂一个日期口径）。
/// 「今天」与「本周」在语义上是包含关系，多选只会多出几个等价组合 —— 既然组合要么
/// 编译不过、要么必然等价，界面上就不该给一个按下去看不出区别的选项。
///
/// 与左栏的 [`BoardScope`] 是 **AND** 关系：范围选「项目 Alpha」、智能列表选
/// 「逾期」，看的就是「Alpha 里逾期的那些」。
///
/// 与「显示已完成」是**两条独立的轴**（原型同款）：本枚举四档的 `WHERE` 全都带
/// `status NOT IN ('done','cancelled')`（见 `duck-project/src/query.rs`），
/// 「今天到期」叠「只看已完成」必然是空集 —— 所以「已完成」不做成第五档，而是交给
/// 工具条上那个独立开关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SmartFilter {
    /// 今天到期且未完成。
    Today,
    /// 本周（周一至周日）到期且未完成。
    ThisWeek,
    /// 已逾期且未完成 / 未取消。
    Overdue,
    /// 没排期且未完成 / 未取消。
    Unscheduled,
}

impl SmartFilter {
    /// 全部取值，界面按这个顺序排左栏。
    pub(crate) const ALL: [Self; 4] = [
        Self::Today,
        Self::ThisWeek,
        Self::Overdue,
        Self::Unscheduled,
    ];

    /// 对应的内核日期口径。
    pub(crate) const fn due(self) -> DueFilter {
        match self {
            Self::Today => DueFilter::Today,
            Self::ThisWeek => DueFilter::ThisWeek,
            Self::Overdue => DueFilter::Overdue,
            Self::Unscheduled => DueFilter::Unscheduled,
        }
    }
}

/// 左栏底部的三条计数与「智能列表」每档的条数（原型 `counts()` / `inSmart` 计数）。
///
/// **全局口径**：与当前范围 / 智能列表 / 搜索都无关 —— 它是「整个模块有多少」，
/// 不是「眼下这一屏有多少」。左栏那四条 `meta` 与底部那三格因此永远稳，不会随着
/// 点一下项目就跳。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SidebarCounts {
    /// 未归档任务总数。
    pub(crate) all: u32,
    /// 已完成。
    pub(crate) done: u32,
    /// 逾期（与 [`SmartFilter::Overdue`] 同一口径，两处共用一份）。
    pub(crate) overdue: u32,
    /// 今天到期。
    pub(crate) today: u32,
    /// 本周到期。
    pub(crate) week: u32,
    /// 未排期。
    pub(crate) unscheduled: u32,
}

impl SidebarCounts {
    /// 某一档智能列表的条数。
    pub(crate) const fn of(self, filter: SmartFilter) -> u32 {
        match filter {
            SmartFilter::Today => self.today,
            SmartFilter::ThisWeek => self.week,
            SmartFilter::Overdue => self.overdue,
            SmartFilter::Unscheduled => self.unscheduled,
        }
    }
}

/// 归档中心里的一行。
///
/// 两段（任务 / 项目）共用一个选中态，所以必须能区分「任务 12」与「项目 12」
/// —— 光存 `i64` 会让 `⌫` 删错东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveRow {
    /// 任务段的一行。
    Task(i64),
    /// 项目段的一行。
    Project(i64),
}

/// 撤销槽里记着的那个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UndoAction {
    /// 撤销「归档任务」→ 把它恢复回来。
    RestoreTask(i64),
    /// 撤销「归档项目」→ 把它（连同连带归档的任务）恢复回来。
    RestoreProject(i64),
}

/// 最近一次**可撤销**的变更（设计文档 §5.6 的撤销通道）。
///
/// 归档可逆且不丢数据，所以归档后给一条带「撤销」的提示条；`seq` 是定时器的守卫
/// —— 7 秒到点时只有「还是自己那一条」才清，否则连做两次归档时，第一条的定时器
/// 会把第二条一起抹掉（`pages/db.rs` 的 `toast_seq` 是同一套）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UndoSlot {
    /// 序号，单调递增。
    pub(crate) seq: u64,
    /// 撤销要做的动作。
    pub(crate) action: UndoAction,
    /// 连带影响的数量：归档项目时是随它归档的任务数，归档任务时是 0。
    pub(crate) cascaded: u32,
}

/// 用户眼中的「今天」。
///
/// 时刻来自内核（Unix 秒），时区偏移来自平台层 —— 两步都在这里合起来，内核与
/// 平台层各自保持单一职责。
pub(crate) fn local_today() -> CalendarDate {
    let now = now_seconds();
    CalendarDate::from_local_unix_seconds(now, duck_platform::local_utc_offset_seconds(now))
}

/// 项目管理页的全部状态。
pub(crate) struct ProjectsState {
    /// 内核。落盘库打不开时为 `None`，此时 [`Self::error`] 有值、界面渲染错误态。
    store: Option<ProjectStore>,
    /// 打开内核失败的原因（英文，直接来自 [`duck_project::ProjectError`]）。
    error: Option<String>,
    /// 左栏一级节点。
    groups: Vec<Group>,
    /// 左栏项目（含已归档的？否 —— 只有归档中心才看得到）。
    projects: Vec<Project>,
    /// 每个项目的任务计数（未归档）。
    counts: HashMap<i64, TaskCounts>,
    /// 看板列（表为空时回落内置四列）。
    columns: Vec<BoardColumn>,
    /// 当前范围下的顶层任务（看板用；子任务不占列）。
    tasks: Vec<Task>,
    /// 左栏选中范围。
    scope: BoardScope,
    /// 选中的任务（详情右栏用）。
    selected: Option<i64>,
    /// 全局统计（状态栏那条）。
    stats: BoardStats,
    /// 计算「逾期 / 今天到期」用的今天。
    today: CalendarDate,
    /// 排序口径。
    sort: TaskSort,
    /// 左栏「智能列表」的选中档；`None` = 不限。
    smart: Option<SmartFilter>,
    /// 左栏点标签后的筛选；`None` = 不限。多选留给后续增量 —— 内核
    /// [`TaskQuery::tags`] 收的是一个 id 列表（AND 语义），界面一次只挑一个最常用。
    tag: Option<i64>,
    /// 左栏「标签」段的数据（`Tag::usage_count` 由 `list_tags` 现算）。
    tags: Vec<Tag>,
    /// 工具条「显示已完成」开关。默认**开**（原型同款）：默认把库里的东西看全，
    /// 需要专注时再关掉，比「默认藏一半、自己发现」友好。
    show_done: bool,
    /// 左栏「全部项目」是否折叠（原型 `collapsed.work`）。
    projects_collapsed: bool,
    /// 左栏底部与智能列表的计数（全局口径）。
    sidebar: SidebarCounts,
    /// 主区搜索词（空串 = 不筛）。与 [`Self::archive_search`] 分开：一个筛在看板 /
    /// 列表上，一个只筛归档区，两处可能同屏。
    search: String,
    /// 主区显示哪一块（看板 / 列表 / 归档中心）。
    view: ViewMode,
    /// 归档中心的搜索词（空串 = 不筛）。
    ///
    /// 与左栏的项目过滤框**不是**同一份：左栏那个筛项目，归档中心的这个筛档案，
    /// 两处同时可见，共用一个词会互相打架。
    archive_search: String,
    /// 归档中心「任务段」的行。
    archive_tasks: Vec<ArchivedTask>,
    /// 归档中心「项目段」的行。
    archive_projects: Vec<ArchivedProject>,
    /// 归档中心的计件数（左栏入口的徽标，任何视图下都要准）。
    archive_counts: ArchiveCounts,
    /// 归档中心的选中行（`⌫` 与 J/K 用）。
    archive_selected: Option<ArchiveRow>,
    /// 最近一次可撤销的变更。
    undo: Option<UndoSlot>,
    /// 撤销槽的序号源。
    undo_seq: u64,
}

impl ProjectsState {
    /// 建状态：读偏好、打开内核、读第一屏数据。
    ///
    /// 偏好（`kv: projects_ui`）在这里读一次就够：之后每次改动都由页面负责落盘，
    /// 状态层不碰存储 —— 它只认「当前是什么」，不认「怎么存下来」。
    pub(crate) fn new(cx: &App) -> Self {
        let prefs = prefs::ProjectsPrefs::load(cx);
        let (store, error) = Self::open_store();
        let mut state = Self {
            store,
            error,
            groups: Vec::new(),
            projects: Vec::new(),
            counts: HashMap::new(),
            columns: Vec::new(),
            tasks: Vec::new(),
            scope: BoardScope::All,
            selected: None,
            stats: BoardStats::default(),
            today: local_today(),
            sort: prefs.task_sort(),
            smart: None,
            tag: None,
            tags: Vec::new(),
            show_done: true,
            projects_collapsed: false,
            sidebar: SidebarCounts::default(),
            search: String::new(),
            view: prefs.view_mode(),
            archive_search: String::new(),
            archive_tasks: Vec::new(),
            archive_projects: Vec::new(),
            archive_counts: ArchiveCounts::default(),
            archive_selected: None,
            undo: None,
            undo_seq: 0,
        };
        state.reload();
        state
    }

    /// 打开内核；落盘库打不开时退到内存库并把原因留在 [`Self::error`]。
    ///
    /// 退到内存库是为了「界面仍然可用」——但绝不静默：内存库不持久，
    /// 用户必须在界面上看到这条原因。
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

    /// 读一遍库（左栏 + 看板 + 统计）。全部读写失败都收敛到 [`Self::error`]。
    pub(crate) fn reload(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        self.today = local_today();
        match store.project_tree() {
            Ok(tree) => {
                self.groups = tree.groups;
                self.projects = tree.projects;
                self.counts = tree.counts;
            }
            Err(err) => return self.fail(err),
        }
        match store.list_columns() {
            Ok(columns) => self.columns = columns,
            Err(err) => return self.fail(err),
        }
        match store.board_stats(self.today) {
            Ok(stats) => self.stats = stats,
            Err(err) => return self.fail(err),
        }
        match store.list_tags() {
            Ok(tags) => self.tags = tags,
            Err(err) => return self.fail(err),
        }
        // 选中的标签可能刚被删 / 被合并掉，这一遍就把它放下 —— 否则查询会一直挂着一个
        // 不存在的 id，屏上显示「0 条」，用户却找不到是哪一项在筛。
        if let Some(id) = self.tag
            && !self.tags.iter().any(|tag| tag.id == id)
        {
            self.tag = None;
        }
        self.reload_sidebar_counts();
        self.reload_tasks();
        self.reload_archive();
    }

    /// 读左栏那两处计数（底部三格 + 智能列表四条）。
    ///
    /// 前三档与「今天」「未排期」直接取 [`BoardStats`]（同一次 `SELECT` 已经算好）；
    /// 只有「本周」内核没有现成字段，单开一条查询。为它给 [`BoardStats`] 加一个字段
    /// 会改动内核的公开结构体（所有构造点连同测试一起动），而这里一年也就读几次 ——
    /// 不值当。查询走 `top_level_only()`：与看板口径一致，子任务不单独计数。
    fn reload_sidebar_counts(&mut self) {
        let stats = self.stats;
        let mut counts = SidebarCounts {
            all: stats.total,
            done: stats.done,
            overdue: stats.overdue,
            today: stats.due_today,
            week: 0,
            unscheduled: stats.unscheduled,
        };
        if let Some(store) = &self.store {
            let query = TaskQuery::new()
                .top_level_only()
                .due(DueFilter::ThisWeek)
                .today(self.today);
            match store.list_tasks(&query) {
                Ok(tasks) => counts.week = tasks.len() as u32,
                Err(err) => return self.fail(err),
            }
        }
        self.sidebar = counts;
    }

    /// 读归档中心。
    ///
    /// 徽标计数每次都要准（左栏常驻、任何视图下都显示），所以无条件读；两份清单
    /// 只在归档中心可见时读 —— 它们要为每行现算标签与子任务进度（`hydrate`），
    /// 归档堆到几百条时不再零成本，而看板上根本用不到。计数只是一条 `COUNT`。
    fn reload_archive(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match store.archive_counts() {
            Ok(counts) => self.archive_counts = counts,
            Err(err) => return self.fail(err),
        }
        if self.view != ViewMode::Archive {
            return;
        }
        let search = self.archive_search.trim().to_owned();
        let term = (!search.is_empty()).then_some(search.as_str());
        match store.list_archived_tasks(term) {
            Ok(tasks) => self.archive_tasks = tasks,
            Err(err) => return self.fail(err),
        }
        match store.list_archived_projects(term) {
            Ok(projects) => self.archive_projects = projects,
            Err(err) => return self.fail(err),
        }
        // 选中行可能刚被恢复 / 彻底删除，这一帧就把它放下。
        if let Some(row) = self.archive_selected
            && !self.archive_row_exists(row)
        {
            self.archive_selected = None;
        }
    }

    /// 这一行还在归档中心里吗。
    fn archive_row_exists(&self, row: ArchiveRow) -> bool {
        match row {
            ArchiveRow::Task(id) => self.archive_tasks.iter().any(|entry| entry.task.id == id),
            ArchiveRow::Project(id) => self
                .archive_projects
                .iter()
                .any(|entry| entry.project.id == id),
        }
    }

    /// 只重读看板任务（拖拽 / 改状态后不必重读整棵树）。
    pub(crate) fn reload_tasks(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let query = self.query();
        match store.list_tasks(&query) {
            Ok(tasks) => {
                self.tasks = tasks;
                // 选中项可能已被归档 / 删除，这一帧就把它放下。
                if let Some(id) = self.selected
                    && !self.tasks.iter().any(|task| task.id == id)
                {
                    self.selected = None;
                }
            }
            Err(err) => self.fail(err),
        }
    }

    /// 当前范围 + 智能列表 + 标签 + 搜索词 + 已完成开关对应的查询。
    ///
    /// 四组条件是 **AND** 关系，且全部收在这一处 —— 看板、列表、状态栏计数读的都是
    /// 同一份 [`Self::tasks`]，所以几处的口径不可能分叉。视图（看板 / 列表）**不是**
    /// 条件之一：切视图不该改变看到的东西。
    fn query(&self) -> TaskQuery {
        let mut query = match self.scope {
            BoardScope::All => TaskQuery::new(),
            BoardScope::Inbox => TaskQuery::new().inbox(),
            BoardScope::Project(id) => TaskQuery::new().projects([id]),
        };
        // 看板与列表只画顶层任务；子任务在卡片上以「n/m」出现（`Task::subtasks`）。
        query = query.top_level_only().sort(self.sort);
        if let Some(id) = self.tag {
            query = query.tags([id]);
        }
        // 「显示已完成」关掉时只挡掉 `done`（原型同款）。已搁置 / 已取消照留：它们
        // 不是「已完成」，默认四列里也自然落不进任何一列，没有理由替用户藏起来。
        if !self.show_done {
            query = query.statuses([
                TaskStatus::Todo,
                TaskStatus::Doing,
                TaskStatus::Blocked,
                TaskStatus::Deferred,
                TaskStatus::Cancelled,
            ]);
        }
        if let Some(filter) = self.smart {
            // `DueFilter` 要用本地「今天」—— 内核不知道时区，缺了会直接报 `Invalid`
            // （宁可报错也不悄悄退回 UTC 今天，见 `duck-project/src/query.rs`）。
            query = query.due(filter.due()).today(self.today);
        }
        let term = self.search.trim();
        if !term.is_empty() {
            query = query.search(term.to_owned());
        }
        query
    }

    fn fail(&mut self, err: duck_project::ProjectError) {
        self.error = Some(err.to_string());
    }

    // ------------------------------------------------------------------
    // 读
    // ------------------------------------------------------------------

    /// 打开内核失败的原因（`None` = 一切正常）。
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub(crate) fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub(crate) fn columns(&self) -> &[BoardColumn] {
        &self.columns
    }

    pub(crate) fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub(crate) fn scope(&self) -> BoardScope {
        self.scope
    }

    pub(crate) const fn selected(&self) -> Option<i64> {
        self.selected
    }

    pub(crate) const fn stats(&self) -> BoardStats {
        self.stats
    }

    pub(crate) const fn today(&self) -> CalendarDate {
        self.today
    }

    /// 当前排序口径。
    pub(crate) const fn sort(&self) -> TaskSort {
        self.sort
    }

    /// 当前智能列表档位（`None` = 不限）。
    pub(crate) const fn smart(&self) -> Option<SmartFilter> {
        self.smart
    }

    /// 左栏点中的标签（`None` = 不限）。
    pub(crate) const fn tag(&self) -> Option<i64> {
        self.tag
    }

    /// 左栏「标签」段的数据（按名称升序，带使用次数）。
    pub(crate) fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// 「显示已完成」是不是开着。
    pub(crate) const fn show_done(&self) -> bool {
        self.show_done
    }

    /// 左栏「全部项目」是否折叠。
    pub(crate) const fn projects_collapsed(&self) -> bool {
        self.projects_collapsed
    }

    /// 左栏的全局计数。
    pub(crate) const fn sidebar_counts(&self) -> SidebarCounts {
        self.sidebar
    }

    /// 主区搜索词（原样，未 trim）。
    pub(crate) fn search(&self) -> &str {
        &self.search
    }

    /// 当前范围下的任务总数（工具条显示）。
    pub(crate) fn task_total(&self) -> u32 {
        self.tasks.len() as u32
    }

    /// 某个项目的计数；没读到按 0 算。
    pub(crate) fn project_counts(&self, id: i64) -> TaskCounts {
        self.counts.get(&id).copied().unwrap_or_default()
    }

    /// 选中任务（详情右栏用）。
    pub(crate) fn selected_task(&self) -> Option<&Task> {
        let id = self.selected?;
        self.tasks.iter().find(|task| task.id == id)
    }

    /// 某一列里的任务（看板按列渲染时用）。
    pub(crate) fn tasks_in_status(&self, status: TaskStatus) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| task.status == status)
            .collect()
    }

    /// 主区显示哪一块。
    pub(crate) const fn view(&self) -> ViewMode {
        self.view
    }

    /// 归档中心的计件数（左栏入口徽标）。
    pub(crate) const fn archive_counts(&self) -> ArchiveCounts {
        self.archive_counts
    }

    /// 归档中心「任务段」的行。
    pub(crate) fn archive_tasks(&self) -> &[ArchivedTask] {
        &self.archive_tasks
    }

    /// 归档中心「项目段」的行。
    pub(crate) fn archive_projects(&self) -> &[ArchivedProject] {
        &self.archive_projects
    }

    /// 归档中心的搜索词（原样，未 trim）。
    pub(crate) fn archive_search(&self) -> &str {
        &self.archive_search
    }

    /// 归档中心的选中行。
    pub(crate) const fn archive_selected(&self) -> Option<ArchiveRow> {
        self.archive_selected
    }

    /// 最近一次可撤销的变更。
    pub(crate) const fn undo_slot(&self) -> Option<UndoSlot> {
        self.undo
    }

    /// 选中的归档行是不是这一条。
    pub(crate) fn is_archive_selected(&self, row: ArchiveRow) -> bool {
        self.archive_selected == Some(row)
    }

    // ------------------------------------------------------------------
    // 写（全部经内核，写完立刻重读）
    // ------------------------------------------------------------------

    /// 换左栏范围。
    ///
    /// 范围与智能列表**互斥**（原型同款）：两者回答的都是「我现在在看哪一堆」，
    /// 同时亮着两处高亮只会让人说不清到底在看什么。点项目就放下智能列表，反之亦然。
    pub(crate) fn set_scope(&mut self, scope: BoardScope) {
        if self.scope == scope && self.smart.is_none() {
            return;
        }
        self.scope = scope;
        self.smart = None;
        self.reload_tasks();
    }

    /// 改排序口径。
    pub(crate) fn set_sort(&mut self, sort: TaskSort) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.reload_tasks();
    }

    /// 改智能列表档位（`None` = 清掉筛选）。
    ///
    /// 只重读任务，不重读整棵树：范围与项目计数都没变，而 `reload()` 会连标签与归档
    /// 计数一起读 —— 点一下左栏不值得付那一趟。
    pub(crate) fn set_smart(&mut self, smart: Option<SmartFilter>) {
        // 选上某一档就把范围收回到「全部任务」（两者互斥，见 [`Self::set_scope`]）；
        // 清掉某一档时**不动**范围，否则「取消筛选」会连带把项目也换了。
        let scope = if smart.is_some() {
            BoardScope::All
        } else {
            self.scope
        };
        if self.smart == smart && self.scope == scope {
            return;
        }
        self.smart = smart;
        self.scope = scope;
        self.reload_tasks();
    }

    /// 改「显示已完成」开关。
    pub(crate) fn set_show_done(&mut self, show: bool) {
        if self.show_done == show {
            return;
        }
        self.show_done = show;
        self.reload_tasks();
    }

    /// 左栏点标签：再点一次同一个 = 取消（与项目 / 智能列表同一套直觉）。
    pub(crate) fn toggle_tag(&mut self, id: i64) {
        let next = if self.tag == Some(id) { None } else { Some(id) };
        if self.tag == next {
            return;
        }
        self.tag = next;
        self.reload_tasks();
    }

    /// 折叠 / 展开左栏的「全部项目」。
    ///
    /// 纯界面态，不动数据 —— 折叠不该改变任何筛选口径。
    pub(crate) fn toggle_projects_collapsed(&mut self) {
        self.projects_collapsed = !self.projects_collapsed;
    }

    /// 改主区搜索词（边打边筛）。
    pub(crate) fn set_search(&mut self, term: &str) {
        if self.search == term {
            return;
        }
        self.search = term.to_owned();
        self.reload_tasks();
    }

    pub(crate) fn select(&mut self, id: Option<i64>) {
        self.selected = id;
    }

    /// 切主区视图。进归档中心时才读那两份清单（见 [`Self::reload_archive`]）。
    pub(crate) fn set_view(&mut self, view: ViewMode) {
        if self.view == view {
            return;
        }
        self.view = view;
        if view != ViewMode::Archive {
            // 离开归档中心就放下那行高亮：回来时按新数据重算，不留一个指向旧行的选中态。
            self.archive_selected = None;
        }
        self.reload_archive();
    }

    /// 改归档中心的搜索词。
    pub(crate) fn set_archive_search(&mut self, term: &str) {
        if self.archive_search == term {
            return;
        }
        self.archive_search = term.to_owned();
        self.reload_archive();
    }

    /// 选中 / 取消选中归档中心的一行。
    pub(crate) fn select_archive(&mut self, row: Option<ArchiveRow>) {
        self.archive_selected = row;
    }

    /// `J` / `K`：在当前视图的**视觉顺序**里前后移动选中。
    ///
    /// 看板按「列序 → 列内卡片序」展平，走位与肉眼从上到下、从左到右一致；
    /// 列表就是 [`Self::tasks`] 自己的排序顺序（它已经是 `sort` 口径出来的）；
    /// 归档中心按「任务段 → 项目段」展平。当前没有选中项时，往下走从头开始、
    /// 往上走从尾开始 —— 否则第一下按 `J` 会「什么都没发生」。
    pub(crate) fn select_next(&mut self, delta: i32) {
        match self.view {
            ViewMode::Board => {
                let order: Vec<i64> = self
                    .columns
                    .iter()
                    .flat_map(|column| self.tasks_in_status(column.status_key))
                    .map(|task| task.id)
                    .collect();
                self.selected = step(&order, self.selected, delta);
            }
            ViewMode::List => {
                let order: Vec<i64> = self.tasks.iter().map(|task| task.id).collect();
                self.selected = step(&order, self.selected, delta);
            }
            ViewMode::Archive => {
                let order: Vec<ArchiveRow> = self
                    .archive_tasks
                    .iter()
                    .map(|entry| ArchiveRow::Task(entry.task.id))
                    .chain(
                        self.archive_projects
                            .iter()
                            .map(|entry| ArchiveRow::Project(entry.project.id)),
                    )
                    .collect();
                self.archive_selected = step(&order, self.archive_selected, delta);
            }
        }
    }

    /// 改任务状态（看板拖拽落点 / 卡片上的快捷动作）。
    ///
    /// 走内核的 [`ProjectStore::move_task`]，它负责 `completed_at` 不变量与
    /// `sort_order` 重排。
    pub(crate) fn move_task(&mut self, id: i64, status: TaskStatus, sort_order: i64) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(err) = store.move_task(id, status, sort_order) {
            return self.fail(err);
        }
        self.reload();
    }

    /// 归档任务（连带子任务，由内核级联）。
    ///
    /// 归档后可撤销（§5.6），所以这里顺手压一条撤销槽 —— 撤销是**归档的一部分**，
    /// 不是页面的额外动作；放在页面里迟早会有一条归档路径忘了压。
    ///
    /// 错误**原样交回调用方**（理由见下面归档中心那一节）：一次归档失败不该在页顶
    /// 留一条擦不掉的横幅，页面拿 `Err` 弹一条即时回执就够。
    pub(crate) fn archive_task(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.archive_task(id)?;
        self.selected = None;
        self.push_undo(UndoAction::RestoreTask(id), 0);
        self.reload();
        Ok(())
    }

    /// 归档项目（连带它的全部任务，由内核级联）；返回连带归档的任务数。
    pub(crate) fn archive_project(&mut self, id: i64) -> duck_project::Result<u32> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let cascaded = store.archive_project(id)?;
        if self.scope.is_project(id) {
            self.scope = BoardScope::All;
        }
        self.selected = None;
        self.push_undo(UndoAction::RestoreProject(id), cascaded);
        self.reload();
        Ok(cascaded)
    }

    /// 压一条撤销槽（序号自增，供撤销条的定时器守卫比对）。
    fn push_undo(&mut self, action: UndoAction, cascaded: u32) {
        self.undo_seq += 1;
        self.undo = Some(UndoSlot {
            seq: self.undo_seq,
            action,
            cascaded,
        });
    }

    // ------------------------------------------------------------------
    // 归档中心：恢复 / 彻底删除 / 清空
    //
    // 这几个口把内核错误**原样交回调用方**，不塞进 [`Self::error`]：那条横幅是给
    // 「库打不开」这类**常驻**故障用的，一次「恢复失败」不该在页顶留一条擦不掉的
    // 警告。页面拿 `Err` 弹即时回执（回执是页面底部那条提示条），故障与否一眼可见。
    // ------------------------------------------------------------------

    /// 恢复一条归档任务。
    pub(crate) fn restore_task(&mut self, id: i64) -> duck_project::Result<u32> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.restore_task(id)?;
        // 这条归档已经被别处恢复了 ⇒ 挂着的那条「撤销归档」也跟着作废（它按下去
        // 只会失败）。见 [`Self::forget_undo`]。
        self.forget_undo(UndoAction::RestoreTask(id));
        self.reload();
        Ok(0)
    }

    /// 恢复一个归档项目（连同随它归档的任务）；返回连带恢复的任务数。
    pub(crate) fn restore_project(&mut self, id: i64) -> duck_project::Result<u32> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let restored = store.restore_project(id)?;
        self.forget_undo(UndoAction::RestoreProject(id));
        self.reload();
        Ok(restored)
    }

    /// 彻底删除一条归档任务（不可逆）。
    pub(crate) fn purge_task(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_archived_task(id)?;
        self.forget_undo(UndoAction::RestoreTask(id));
        self.reload();
        Ok(())
    }

    /// 彻底删除一个归档项目（连同随它归档的任务，不可逆）。
    pub(crate) fn purge_project(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_archived_project(id)?;
        self.forget_undo(UndoAction::RestoreProject(id));
        self.reload();
        Ok(())
    }

    /// 清空归档；返回值是被清掉的条数。
    pub(crate) fn clear_archive(&mut self, scope: ClearScope) -> duck_project::Result<u32> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let cleared = store.clear_archive(scope)?;
        self.archive_selected = None;
        // 归档区整个清空了，任何「撤销归档」都不可能再成功。
        self.undo = None;
        self.reload();
        Ok(cleared)
    }

    /// 物理删除一条**未归档**任务（看板 `⌫`，不可逆）。
    ///
    /// 与「彻底删除归档记录」分开：内核的 `delete_archived_task` 只收已归档对象
    /// （先归档再删是它的口径），未归档任务的物理删除走 `delete_task`。界面上这两条
    /// 都必须在二次确认之后才调得到（§5.6）。
    pub(crate) fn delete_task(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_task(id)?;
        // 删掉的任务若正是撤销槽指的那条（先归档、撤销回来、再删），槽跟着作废。
        self.forget_undo(UndoAction::RestoreTask(id));
        self.reload();
        Ok(())
    }

    /// 改一条任务的若干字段（`patch` 里只放要改的项）。
    ///
    /// 卡片菜单里的优先级 / 项目 / 四象限都走它 —— 一处收口「组 patch → 调内核 → 重读」，
    /// 省得每个动作各写一遍。写完只重读任务：范围与项目计数都没变，而 `reload()` 还要
    /// 连标签与归档一起读。
    pub(crate) fn patch_task(&mut self, id: i64, patch: TaskPatch) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.update_task(id, patch)?;
        self.reload_tasks();
        Ok(())
    }

    /// 把任务挪进另一个项目；`None` = 移出到「未归入项目」。
    ///
    /// 单独开一个口而不是让调用方拼 `TaskPatch`：`project` 与 `clear_project` 是两个
    /// 方法，拼错了会得到一个「什么都没改」的空 patch（内核照收，界面看着像没反应）。
    pub(crate) fn move_task_to_project(
        &mut self,
        id: i64,
        project_id: Option<i64>,
    ) -> duck_project::Result<()> {
        let patch = match project_id {
            Some(project_id) => TaskPatch::new().project(project_id),
            None => TaskPatch::new().clear_project(),
        };
        self.patch_task(id, patch)
    }

    /// 改一条任务的状态（卡片菜单），错误**原样交给调用方**。
    ///
    /// 与拖拽走的 [`Self::move_task`] 分开：拖拽的落点是自己人（列头就摆在那儿），
    /// 失败在状态里记一笔就够；菜单要弹一条即时回执说明「为什么没改成」——比如从
    /// 「已取消」出发去别的列，内核的状态机会拒（`cancelled` 只许回 `todo`），
    /// 这时候必须把原因说出来，而不是静默什么都不发生。
    pub(crate) fn set_task_status(
        &mut self,
        id: i64,
        status: TaskStatus,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.move_task(id, status, 0)?;
        self.reload();
        Ok(())
    }

    /// 撤销最近一次归档（§5.6 的撤销通道）。
    ///
    /// - 没有可撤的 → `Ok(None)`（按 `⌘Z` 是空操作，不算错）；
    /// - 槽位指向的对象已经不在归档里（被别处恢复了 / 被彻底删了）→ `Err`，同时把
    ///   这条没用的槽放下 —— 留着一个按下去必然失败的按钮比没有按钮更糟。
    ///
    /// 撤销**本身不再压新槽**：撤销就是「把归档退回去」，再按一次不该又归档一次。
    pub(crate) fn undo(&mut self) -> duck_project::Result<Option<UndoAction>> {
        let Some(action) = self.undo.as_ref().map(|slot| slot.action) else {
            return Ok(None);
        };
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let outcome: duck_project::Result<u32> = match action {
            UndoAction::RestoreTask(id) => store.restore_task(id).map(|()| 0),
            UndoAction::RestoreProject(id) => store.restore_project(id),
        };
        self.undo = None;
        match outcome {
            Ok(_) => {
                self.reload();
                Ok(Some(action))
            }
            Err(err) => Err(err),
        }
    }

    /// 撤销条超时：只有「还是自己那一条」才清。
    ///
    /// 少了这个守卫，7 秒内连做两次归档时，第一条的定时器会把第二条的撤销槽一起抹掉
    /// —— 第二条等于没给撤销的机会。
    pub(crate) fn expire_undo(&mut self, seq: u64) -> bool {
        if self.undo.as_ref().is_some_and(|slot| slot.seq == seq) {
            self.undo = None;
            return true;
        }
        false
    }

    /// 这条撤销槽已经被别处兑现 / 作废 ⇒ 放下它。
    ///
    /// 判据只认「同一个动作」：恢复任务 3 不该顺手把「撤销归档项目 7」也清掉。
    fn forget_undo(&mut self, action: UndoAction) {
        if self.undo.as_ref().is_some_and(|slot| slot.action == action) {
            self.undo = None;
        }
    }

    /// 切换一条任务的完成态（`Space`）。
    ///
    /// `cancelled` 是终态，要复活必须先回 `todo`（内核的状态机约束），所以取消态
    /// 按 `Space` 是「复活成待办」而不是「直接完成」。
    pub(crate) fn toggle_done(&mut self, id: i64) {
        let Some(status) = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.status)
        else {
            return;
        };
        let next = match status {
            TaskStatus::Done => TaskStatus::Todo,
            TaskStatus::Cancelled => TaskStatus::Todo,
            _ => TaskStatus::Done,
        };
        self.move_task(id, next, 0);
    }

    /// 新建任务；返回新 id。
    ///
    /// `title` 为空串时直接放弃（界面上按钮不该这么点得到，但这里兜一道）。
    pub(crate) fn create_task(&mut self, title: &str, project_id: Option<i64>) -> Option<i64> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        let store = self.store.as_ref()?;
        let mut new = duck_project::NewTask::new(title);
        if let Some(project_id) = project_id {
            new = new.project(project_id);
        }
        match store.create_task(new) {
            Ok(id) => {
                self.reload();
                self.selected = Some(id);
                Some(id)
            }
            Err(err) => {
                self.fail(err);
                None
            }
        }
    }

    /// 新建项目；返回新 id。
    pub(crate) fn create_project(&mut self, name: &str) -> Option<i64> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let store = self.store.as_ref()?;
        match store.create_project(duck_project::NewProject::new(name)) {
            Ok(id) => {
                self.reload();
                Some(id)
            }
            Err(err) => {
                self.fail(err);
                None
            }
        }
    }
}

/// 「内核没打开」时交回去的错误。
///
/// 正常路径上走不到（库打不开时页面渲染的是错误态、所有写口都点不着），但写口是
/// `pub(crate)` 的，测试与将来的入口都会调到 —— 静默返回 `Ok` 会让调用方以为写成了。
fn store_not_open() -> duck_project::ProjectError {
    duck_project::ProjectError::Invalid("project store is not open".to_owned())
}

/// 在一串有序项里挪一步。
///
/// 当前项不在里面（刚被删 / 刚换视图）时按方向落头或落尾；空列表返回 `None`。
fn step<T: PartialEq + Copy>(order: &[T], current: Option<T>, delta: i32) -> Option<T> {
    if order.is_empty() {
        return None;
    }
    let last = order.len() - 1;
    let index = current.and_then(|value| order.iter().position(|item| *item == value));
    let next = match (index, delta) {
        (Some(index), delta) => (index as i64 + i64::from(delta)).clamp(0, last as i64) as usize,
        (None, delta) if delta < 0 => last,
        (None, _) => 0,
    };
    Some(order[next])
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_project::{NewProject, NewTask, TaskStatus};

    /// 单测固定的「今天」。
    ///
    /// [`ProjectsState::reload`] 会把 `today` 刷成**真实**今天（`local_today()`）——
    /// 那是给生产用的；单测里必须钉死，否则「今天到期」这类断言会随跑测试的日期漂移
    /// （在别的日子跑就红了，而且红得莫名其妙）。
    fn fixture_today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    /// 重读库，并把「今天」钉回 [`fixture_today`]。
    ///
    /// `reload()` 会把「今天」刷成**真实**今天，而左栏计数与日期分档断言都建立在固定
    /// 日期上 —— 不钉住的话，换一天跑测试就红。钉完再重算一次左栏计数（那一份依赖
    /// `today`，见 [`ProjectsState::reload_sidebar_counts`]）。
    fn reload_and_pin(state: &mut ProjectsState) {
        state.reload();
        state.today = fixture_today();
        state.reload_sidebar_counts();
    }

    /// 用内存库造一个脱离了页面的状态：正是「状态层不碰 gpui」的用处。
    ///
    /// 这里**不走** `ProjectsState::new(cx)`：那条路要读 `kv`（偏好），而单测里没有
    /// `App`、也不该有 —— 默认值在 [`prefs`] 的测试里钉，这里只要一个干净的起点。
    fn memory_state() -> ProjectsState {
        let store = ProjectStore::open_in_memory().expect("in-memory sqlite");
        let mut state = ProjectsState {
            store: Some(store),
            error: None,
            groups: Vec::new(),
            projects: Vec::new(),
            counts: HashMap::new(),
            columns: Vec::new(),
            tasks: Vec::new(),
            scope: BoardScope::All,
            selected: None,
            stats: BoardStats::default(),
            today: fixture_today(),
            sort: TaskSort::Manual,
            smart: None,
            tag: None,
            tags: Vec::new(),
            show_done: true,
            projects_collapsed: false,
            sidebar: SidebarCounts::default(),
            search: String::new(),
            view: ViewMode::Board,
            archive_search: String::new(),
            archive_tasks: Vec::new(),
            archive_projects: Vec::new(),
            archive_counts: ArchiveCounts::default(),
            archive_selected: None,
            undo: None,
            undo_seq: 0,
        };
        reload_and_pin(&mut state);
        state
    }

    /// 建一条收集箱任务并返回 id。
    fn add_task(state: &mut ProjectsState, title: &str) -> i64 {
        let id = state
            .store
            .as_ref()
            .unwrap()
            .create_task(NewTask::new(title))
            .unwrap();
        reload_and_pin(state);
        id
    }

    /// 建一个项目 + 一条属于它的任务，返回 `(项目 id, 任务 id)`。
    fn add_project_with_task(state: &mut ProjectsState) -> (i64, i64) {
        let (project, task) = {
            let store = state.store.as_ref().unwrap();
            let project = store.create_project(NewProject::new("P")).unwrap();
            let task = store
                .create_task(NewTask::new("child").project(project))
                .unwrap();
            (project, task)
        };
        reload_and_pin(state);
        (project, task)
    }

    #[test]
    fn local_today_matches_the_injected_offset() {
        // 偏移 0 时必须与纯 UTC 解释一致；这条钉住「上层只加一次偏移」。
        let now = now_seconds();
        let utc = CalendarDate::from_unix_seconds(now);
        let local = local_today();
        assert!(
            local.days_until(utc).abs() <= 1,
            "本地日与 UTC 日不该差过一天"
        );
    }

    #[test]
    fn scope_selects_the_right_slice() {
        let mut state = memory_state();
        let store = state.store.as_ref().unwrap();
        let project = store.create_project(NewProject::new("P")).unwrap();
        store
            .create_task(NewTask::new("in project").project(project))
            .unwrap();
        store.create_task(NewTask::new("inbox one")).unwrap();

        state.reload();
        assert_eq!(state.task_total(), 2, "全部范围应看到两条顶层任务");

        state.set_scope(BoardScope::Project(project));
        assert_eq!(state.task_total(), 1);

        state.set_scope(BoardScope::Inbox);
        assert_eq!(state.task_total(), 1);

        state.set_scope(BoardScope::All);
        assert_eq!(state.task_total(), 2);
    }

    #[test]
    fn board_columns_fall_back_to_builtin_four() {
        let state = memory_state();
        let keys: Vec<TaskStatus> = state.columns().iter().map(|c| c.status_key).collect();
        assert_eq!(keys, TaskStatus::DEFAULT_BOARD.to_vec());
    }

    #[test]
    fn archiving_a_task_hides_it_and_drops_the_selection() {
        let mut state = memory_state();
        let id = {
            let store = state.store.as_ref().unwrap();
            store.create_task(NewTask::new("t")).unwrap()
        };
        state.reload();
        state.select(Some(id));
        assert_eq!(state.task_total(), 1);

        state.archive_task(id).unwrap();
        assert_eq!(state.task_total(), 0, "归档后必须立刻从看板消失");
        assert_eq!(state.selected(), None, "选中项跟着消失");
    }

    #[test]
    fn archiving_a_project_hides_its_tasks_and_resets_the_scope() {
        let mut state = memory_state();
        let project = {
            let store = state.store.as_ref().unwrap();
            let project = store.create_project(NewProject::new("P")).unwrap();
            store
                .create_task(NewTask::new("t").project(project))
                .unwrap();
            project
        };
        state.set_scope(BoardScope::Project(project));
        assert_eq!(state.task_total(), 1);

        state.archive_project(project).unwrap();
        // 范围内项目没了 ⇒ 回落「全部」，且它的任务已随项目归档。
        assert_eq!(state.scope(), BoardScope::All);
        assert_eq!(state.task_total(), 0);
    }

    #[test]
    fn empty_titles_are_refused() {
        let mut state = memory_state();
        assert_eq!(state.create_task("   ", None), None);
        assert_eq!(state.create_project(""), None);
        assert_eq!(state.task_total(), 0);
    }

    #[test]
    fn move_task_lands_in_the_target_column() {
        let mut state = memory_state();
        let id = {
            let store = state.store.as_ref().unwrap();
            store.create_task(NewTask::new("t")).unwrap()
        };
        state.reload();
        state.move_task(id, TaskStatus::Doing, 0);
        assert_eq!(state.tasks_in_status(TaskStatus::Doing).len(), 1);
        assert_eq!(state.tasks_in_status(TaskStatus::Todo).len(), 0);
    }

    // ── S3：归档中心 ────────────────────────────────────────────────────

    #[test]
    fn archiving_leaves_an_undo_that_brings_the_task_back() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");

        state.archive_task(id).unwrap();
        assert_eq!(state.task_total(), 0);
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RestoreTask(id)),
            "归档必须顺手压一条撤销槽"
        );

        assert_eq!(state.undo().unwrap(), Some(UndoAction::RestoreTask(id)));
        assert_eq!(state.task_total(), 1, "撤销后任务回到看板");
        assert_eq!(state.undo_slot(), None, "撤销槽用完即清");
    }

    #[test]
    fn a_stale_timer_does_not_clear_a_newer_undo() {
        let mut state = memory_state();
        let first = add_task(&mut state, "one");
        let second = add_task(&mut state, "two");

        state.archive_task(first).unwrap();
        let stale = state.undo_slot().unwrap().seq;
        state.archive_task(second).unwrap();

        assert!(!state.expire_undo(stale), "旧定时器不该清掉新槽");
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RestoreTask(second))
        );
        assert!(state.expire_undo(state.undo_slot().unwrap().seq));
        assert_eq!(state.undo_slot(), None);
    }

    #[test]
    fn archiving_a_project_records_the_cascade_for_the_undo() {
        let mut state = memory_state();
        let (project, _task) = add_project_with_task(&mut state);

        state.archive_project(project).unwrap();
        let slot = state.undo_slot().expect("归档项目要留撤销槽");
        assert_eq!(slot.action, UndoAction::RestoreProject(project));
        assert_eq!(slot.cascaded, 1, "连带归档的任务数要能被提示条读到");

        state.undo().unwrap();
        assert_eq!(state.task_total(), 1, "撤销后任务随项目一起回来");
    }

    #[test]
    fn a_task_that_rode_along_with_its_project_cannot_be_restored_alone() {
        let mut state = memory_state();
        let (project, task) = add_project_with_task(&mut state);
        state.archive_project(project).unwrap();

        assert!(
            state.restore_task(task).is_err(),
            "项目还在归档中，任务不能单独复活"
        );
        assert!(state.restore_project(project).is_ok());
    }

    #[test]
    fn the_task_section_lists_only_root_tasks() {
        let mut state = memory_state();
        let loose = add_task(&mut state, "loose");
        let (project, _task) = add_project_with_task(&mut state);

        state.archive_task(loose).unwrap();
        state.archive_project(project).unwrap();
        state.set_view(ViewMode::Archive);

        let titles: Vec<&str> = state
            .archive_tasks()
            .iter()
            .map(|entry| entry.task.title.as_str())
            .collect();
        assert_eq!(titles, vec!["loose"], "随项目归档的任务不在任务段重复出现");
        assert_eq!(state.archive_projects().len(), 1);
        let counts = state.archive_counts();
        assert_eq!((counts.tasks, counts.projects), (1, 1));
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn purging_is_refused_for_a_task_that_is_still_live() {
        let mut state = memory_state();
        let id = add_task(&mut state, "live");
        assert!(
            state.purge_task(id).is_err(),
            "彻底删除只对已归档对象开放（内核口径：先归档再删）"
        );
        assert_eq!(state.task_total(), 1, "被拒的删除不该留下痕迹");
    }

    #[test]
    fn clearing_the_archive_empties_both_sections_and_resets_the_selection() {
        let mut state = memory_state();
        let loose = add_task(&mut state, "loose");
        let (project, _task) = add_project_with_task(&mut state);
        state.archive_task(loose).unwrap();
        state.archive_project(project).unwrap();
        state.set_view(ViewMode::Archive);
        state.select_archive(Some(ArchiveRow::Task(loose)));

        let cleared = state.clear_archive(ClearScope::All).unwrap();
        assert_eq!(cleared, 2, "清掉的是归档中心的两行");
        assert!(state.archive_tasks().is_empty());
        assert!(state.archive_projects().is_empty());
        assert_eq!(state.archive_counts().total(), 0);
        assert_eq!(
            state.archive_selected(),
            None,
            "选中的行没了，选中态跟着放下"
        );
    }

    #[test]
    fn the_archive_search_narrows_both_sections() {
        let mut state = memory_state();
        let alpha = add_task(&mut state, "alpha");
        let beta = add_task(&mut state, "beta");
        let (project, _task) = add_project_with_task(&mut state);
        state.archive_task(alpha).unwrap();
        state.archive_task(beta).unwrap();
        state.archive_project(project).unwrap();
        state.set_view(ViewMode::Archive);
        assert_eq!(state.archive_tasks().len(), 2);

        state.set_archive_search("alpha");
        let titles: Vec<&str> = state
            .archive_tasks()
            .iter()
            .map(|entry| entry.task.title.as_str())
            .collect();
        assert_eq!(titles, vec!["alpha"]);
        assert!(
            state.archive_projects().is_empty(),
            "项目名 `P` 里没有 alpha，两段都该被筛掉"
        );

        // 空词 = 不筛；`reload()` 也要跟着搜索词走（每条写口都会调它）。
        state.set_archive_search("");
        state.reload();
        assert_eq!(state.archive_tasks().len(), 2);
        assert_eq!(state.archive_projects().len(), 1);
    }

    #[test]
    fn leaving_the_archive_center_drops_the_row_selection() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        state.set_view(ViewMode::Archive);
        state.select_archive(Some(ArchiveRow::Task(id)));

        state.set_view(ViewMode::Board);
        assert_eq!(state.archive_selected(), None);
    }

    #[test]
    fn the_archive_lists_are_only_read_while_the_center_is_visible() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        // 归档动作发生在看板上（不在归档中心）时，两份清单按需读 —— 这里不该有行。
        assert!(state.archive_tasks().is_empty(), "看板视图下不读归档清单");
        assert_eq!(state.archive_counts().total(), 1, "但徽标计数必须准");
    }

    // ── S3：键盘导航 ────────────────────────────────────────────────────

    #[test]
    fn select_next_walks_the_board_in_column_order() {
        let mut state = memory_state();
        let todo = add_task(&mut state, "todo");
        let doing = add_task(&mut state, "doing");
        state.move_task(doing, TaskStatus::Doing, 0);

        assert_eq!(state.selected(), None);
        state.select_next(1);
        assert_eq!(state.selected(), Some(todo), "第一下 J 落在第一条");
        state.select_next(1);
        assert_eq!(state.selected(), Some(doing), "列序：待办 → 进行中");
        state.select_next(1);
        assert_eq!(state.selected(), Some(doing), "到底就停住，不绕圈");
        state.select_next(-1);
        assert_eq!(state.selected(), Some(todo));
    }

    #[test]
    fn select_next_starts_from_the_tail_when_going_up() {
        let mut state = memory_state();
        let first = add_task(&mut state, "one");
        let last = add_task(&mut state, "two");

        state.select_next(-1);
        assert_eq!(state.selected(), Some(last), "往上走从最后一条进");
        state.select_next(-1);
        assert_eq!(state.selected(), Some(first));
    }

    #[test]
    fn select_next_follows_the_archive_sections() {
        let mut state = memory_state();
        let loose = add_task(&mut state, "loose");
        let (project, _task) = add_project_with_task(&mut state);
        state.archive_task(loose).unwrap();
        state.archive_project(project).unwrap();
        state.set_view(ViewMode::Archive);

        state.select_next(1);
        assert_eq!(state.archive_selected(), Some(ArchiveRow::Task(loose)));
        state.select_next(1);
        assert_eq!(
            state.archive_selected(),
            Some(ArchiveRow::Project(project)),
            "任务段走完接项目段"
        );
    }

    #[test]
    fn an_empty_view_has_nothing_to_select() {
        let mut state = memory_state();
        state.select_next(1);
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn toggling_done_bounces_between_done_and_todo() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");

        state.toggle_done(id);
        assert_eq!(state.tasks_in_status(TaskStatus::Done).len(), 1);
        state.toggle_done(id);
        assert_eq!(state.tasks_in_status(TaskStatus::Todo).len(), 1);
    }

    #[test]
    fn toggling_a_cancelled_task_revives_it_instead_of_completing_it() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.move_task(id, TaskStatus::Cancelled, 0);
        assert_eq!(state.tasks_in_status(TaskStatus::Cancelled).len(), 1);

        // `cancelled` 是终态：先回待办，不能直接跳完成。
        state.toggle_done(id);
        assert_eq!(state.tasks_in_status(TaskStatus::Todo).len(), 1);
        assert_eq!(state.tasks_in_status(TaskStatus::Done).len(), 0);
    }

    // ── S3-I：撤销槽与「未归档任务的物理删除」──────────────────────────────

    #[test]
    fn a_slot_that_was_already_honoured_elsewhere_is_dropped() {
        // 先归档、再在归档中心手动恢复：那条「撤销归档」按下去只会失败，
        // 所以必须在恢复的那一刻就作废（否则提示条上挂着一个按了报错的按钮）。
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        assert!(state.undo_slot().is_some());

        state.set_view(ViewMode::Archive);
        assert_eq!(state.restore_task(id).unwrap(), 0);
        assert_eq!(state.undo_slot(), None, "已被恢复的归档不再可撤销");
        assert_eq!(state.undo().unwrap(), None, "没有可撤的算空操作，不是错误");
    }

    #[test]
    fn a_slot_survives_an_unrelated_restore() {
        let mut state = memory_state();
        let first = add_task(&mut state, "one");
        let second = add_task(&mut state, "two");
        state.archive_task(first).unwrap();
        state.archive_task(second).unwrap();

        // 槽里是第二条；恢复第一条不该把它的撤销机会一起收走。
        state.restore_task(first).unwrap();
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RestoreTask(second))
        );
    }

    #[test]
    fn purging_the_archived_target_drops_its_undo_slot() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();

        state.purge_task(id).unwrap();
        assert_eq!(
            state.undo_slot(),
            None,
            "对象已经彻底没了，撤销槽必须跟着走"
        );
    }

    #[test]
    fn clearing_the_archive_also_drops_the_undo_slot() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();

        state.clear_archive(ClearScope::All).unwrap();
        assert_eq!(state.undo_slot(), None);
    }

    #[test]
    fn deleting_a_live_task_takes_it_off_the_board() {
        let mut state = memory_state();
        let id = add_task(&mut state, "live");
        state.select(Some(id));

        state.delete_task(id).unwrap();
        assert_eq!(state.task_total(), 0, "未归档任务也能被物理删除（看板 ⌫）");
        assert_eq!(state.selected(), None, "被删的那条不该还占着右栏");
        assert!(state.delete_task(id).is_err(), "删第二次应当报「找不到」");
    }

    #[test]
    fn deleting_a_task_under_its_own_archived_copy_only_drops_that_slot() {
        // 一条任务先归档、再撤销回来、然后被物理删除：撤销槽指的正是它。
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        state.undo().unwrap();
        assert_eq!(state.undo_slot(), None, "撤销之后槽已经空了");

        // 另一种次序：归档 A、再归档 B，然后删掉 A 的「已归档副本」。
        let other = add_task(&mut state, "other");
        state.archive_task(id).unwrap();
        state.archive_task(other).unwrap();
        state.purge_task(id).unwrap();
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RestoreTask(other)),
            "删的是 id，不该动 other 的撤销槽"
        );
    }

    #[test]
    fn a_stale_slot_reports_an_error_and_is_cleared() {
        // 绕过 `restore_task` 直接掏空归档区，制造「槽还在、对象没了」的状态：
        // 界面不该挂着一个按下去必然失败的撤销按钮。
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        state
            .store
            .as_ref()
            .unwrap()
            .delete_archived_task(id)
            .unwrap();

        assert!(state.undo().is_err(), "撤销一条已消失的记录应当失败");
        assert_eq!(state.undo_slot(), None, "失败的槽也不能留着");
    }

    // ── S3-II：列表视图 / 快捷筛选 / 主区搜索 ───────────────────────────

    /// 建一条带截止日的任务并返回 id。
    fn add_task_due(state: &mut ProjectsState, title: &str, due: CalendarDate) -> i64 {
        let id = state
            .store
            .as_ref()
            .unwrap()
            .create_task(NewTask::new(title).dates(None, Some(due)))
            .unwrap();
        reload_and_pin(state);
        id
    }

    /// 建一条带描述的任务（搜索要连描述一起匹配）。
    fn add_task_with_note(state: &mut ProjectsState, title: &str, note: &str) -> i64 {
        let id = state
            .store
            .as_ref()
            .unwrap()
            .create_task(NewTask::new(title).description(note))
            .unwrap();
        reload_and_pin(state);
        id
    }

    /// 当前结果集的 id，按界面看到的顺序。
    fn visible_ids(state: &ProjectsState) -> Vec<i64> {
        state.tasks().iter().map(|task| task.id).collect()
    }

    #[test]
    fn every_smart_filter_is_reachable_and_clears_again() {
        // 四档各点一遍：不 panic、也不留一个「清不掉」的状态（`due()` 的映射漏档
        // 会在这里现形）。
        let mut state = memory_state();
        for filter in SmartFilter::ALL {
            state.set_smart(Some(filter));
            assert_eq!(state.smart(), Some(filter), "{filter:?}");
        }
        state.set_smart(None);
        assert_eq!(state.smart(), None, "再点一次即回到不限");
    }

    #[test]
    fn the_today_filter_keeps_only_tasks_due_today() {
        let mut state = memory_state();
        let today = state.today();
        let due_today = add_task_due(&mut state, "today", today);
        add_task_due(&mut state, "later", today.add_days(5));
        add_task(&mut state, "no date");
        assert_eq!(state.task_total(), 3);

        state.set_smart(Some(SmartFilter::Today));
        assert_eq!(visible_ids(&state), vec![due_today], "只留今天到期的");
    }

    #[test]
    fn the_overdue_and_unscheduled_filters_split_on_the_due_date() {
        let mut state = memory_state();
        let today = state.today();
        let late = add_task_due(&mut state, "late", today.add_days(-2));
        let loose = add_task(&mut state, "loose");

        state.set_smart(Some(SmartFilter::Overdue));
        assert_eq!(visible_ids(&state), vec![late]);

        state.set_smart(Some(SmartFilter::Unscheduled));
        assert_eq!(visible_ids(&state), vec![loose]);
    }

    #[test]
    fn the_due_filters_never_show_settled_tasks() {
        // 内核的四档日期口径全都带 `status NOT IN ('done','cancelled')`，界面靠的就是
        // 这一条 —— 少了它会看到「今天到期的已完成任务」这种自相矛盾的行。
        let mut state = memory_state();
        let today = state.today();
        let done = add_task_due(&mut state, "done today", today);
        state.move_task(done, TaskStatus::Done, 0);

        state.set_smart(Some(SmartFilter::Today));
        assert_eq!(state.task_total(), 0, "已完成的今天到期项不该出现");
    }

    #[test]
    fn hiding_done_tasks_takes_them_out_of_the_view() {
        // 「已完成」不是智能列表的第五档，而是工具条上一个**独立开关**（原型同款）：
        // 关掉它，看板 / 列表里不再出现已完成；开回来又都在。
        let mut state = memory_state();
        let open = add_task(&mut state, "open");
        let done = add_task(&mut state, "done");
        state.move_task(done, TaskStatus::Done, 0);
        assert_eq!(state.task_total(), 2, "默认显示已完成");

        state.set_show_done(false);
        assert_eq!(visible_ids(&state), vec![open]);
        state.set_show_done(true);
        assert_eq!(state.task_total(), 2, "开回来两条都在");
    }

    #[test]
    fn the_smart_filter_and_the_scope_are_mutually_exclusive() {
        // 两者回答的都是「我在看哪一堆」，同屏亮两处高亮只会让人说不清在看什么；
        // 原型也是互斥的（点项目清智能列表，点智能列表清项目）。
        let mut state = memory_state();
        let today = state.today();
        let (project, _child) = add_project_with_task(&mut state);
        let in_project = {
            let store = state.store.as_ref().unwrap();
            store
                .create_task(NewTask::new("p").project(project).dates(None, Some(today)))
                .unwrap()
        };
        let inbox = add_task_due(&mut state, "i", today);

        state.set_scope(BoardScope::Project(project));
        state.set_smart(Some(SmartFilter::Today));
        assert_eq!(state.scope(), BoardScope::All, "选智能列表就把范围收回全部");
        let ids = visible_ids(&state);
        assert!(ids.contains(&in_project), "全部范围下项目里那条在");
        assert!(ids.contains(&inbox), "全部范围下收集箱那条也在");

        state.set_scope(BoardScope::Project(project));
        assert_eq!(state.smart(), None, "点项目就放下智能列表");
        let ids = visible_ids(&state);
        assert!(ids.contains(&in_project));
        assert!(!ids.contains(&inbox), "收集箱那条不属于当前范围");
    }

    #[test]
    fn the_sidebar_counts_describe_the_whole_module_not_the_current_view() {
        // 左栏那四条与底部三格是**全局**口径：点一下项目 / 挂一个搜索词都不该让它们跳
        // —— 跳了就没法拿它当「这个模块有多少」的参照。
        let mut state = memory_state();
        let today = state.today();
        add_task_due(&mut state, "today", today);
        add_task_due(&mut state, "late", today.add_days(-3));
        add_task_due(&mut state, "later this week", today.add_days(2));
        add_task_due(&mut state, "next week", today.add_days(20));
        add_task(&mut state, "no date");
        let done = add_task(&mut state, "done");
        state.move_task(done, TaskStatus::Done, 0);

        let counts = state.sidebar_counts();
        assert_eq!(counts.all, 6, "未归档任务总数");
        assert_eq!(counts.done, 1);
        assert_eq!(counts.today, 1);
        assert_eq!(counts.overdue, 1);
        assert_eq!(counts.unscheduled, 1);
        assert_eq!(counts.week, 2, "今天与「后天」都在本周（周一至周日）内");
        assert_eq!(
            counts.of(SmartFilter::Overdue),
            counts.overdue,
            "智能列表那条与底部那条必须是同一个数"
        );

        state.set_search("today");
        assert_eq!(state.task_total(), 1, "搜索确实把主区收窄了");
        assert_eq!(state.sidebar_counts(), counts, "但左栏给的是全局口径");
    }

    #[test]
    fn the_tag_filter_keeps_only_tagged_tasks_and_toggles_off() {
        let mut state = memory_state();
        let (tag, tagged) = {
            let store = state.store.as_ref().unwrap();
            let tag = store.ensure_tag("网关", "#3B82F6").unwrap();
            let task = store
                .create_task(NewTask::new("tagged").tags([tag]))
                .unwrap();
            (tag, task)
        };
        add_task(&mut state, "plain");

        assert_eq!(state.tags().len(), 1, "左栏「标签」段读到了这一个");
        assert_eq!(state.tags()[0].usage_count, 1);

        state.toggle_tag(tag);
        assert_eq!(state.tag(), Some(tag));
        assert_eq!(visible_ids(&state), vec![tagged]);

        state.toggle_tag(tag);
        assert_eq!(state.tag(), None, "再点一次取消");
        assert_eq!(state.task_total(), 2);
    }

    #[test]
    fn a_deleted_tag_is_dropped_from_the_filter() {
        // 标签被删 / 被合并掉之后，筛选不能继续挂着一个不存在的 id —— 那会显示
        // 「0 条」，而左栏里已经找不到哪一项在筛。
        let mut state = memory_state();
        let tag = {
            let store = state.store.as_ref().unwrap();
            let tag = store.ensure_tag("临时", "#F59E0B").unwrap();
            store.create_task(NewTask::new("t").tags([tag])).unwrap();
            tag
        };
        state.toggle_tag(tag);
        assert_eq!(state.tag(), Some(tag));
        assert_eq!(state.task_total(), 1);

        state.store.as_ref().unwrap().delete_tag(tag).unwrap();
        reload_and_pin(&mut state);
        assert_eq!(state.tag(), None, "标签没了，筛选也跟着放下");
        assert_eq!(state.task_total(), 1, "任务本身还在（只是没标签了）");
    }

    #[test]
    fn the_search_matches_title_and_description() {
        let mut state = memory_state();
        let by_title = add_task(&mut state, "连接池复用");
        add_task_with_note(&mut state, "别的", "顺手把连接池的超时也调了");
        add_task(&mut state, "无关");

        state.set_search("连接池");
        let ids = visible_ids(&state);
        assert!(ids.contains(&by_title), "标题命中");
        assert_eq!(ids.len(), 2, "描述命中也要算（内核同时匹配两列）");

        state.set_search("");
        assert_eq!(state.task_total(), 3, "清空搜索词 = 不筛");
    }

    #[test]
    fn the_search_escapes_like_metacharacters() {
        // 用户搜 `100%` 时 `%` 不该当通配符用（转义在内核，这里钉住界面确实走了那条路）。
        let mut state = memory_state();
        let exact = add_task(&mut state, "覆盖率 100%");
        add_task(&mut state, "覆盖率 1000");

        state.set_search("100%");
        assert_eq!(visible_ids(&state), vec![exact]);
    }

    #[test]
    fn the_list_view_walks_the_sorted_order_not_the_creation_order() {
        let mut state = memory_state();
        let task_b = add_task(&mut state, "b");
        let task_a = add_task(&mut state, "a");
        state.set_view(ViewMode::List);
        state.set_sort(TaskSort::TitleAsc);

        assert_eq!(state.selected(), None);
        state.select_next(1);
        assert_eq!(state.selected(), Some(task_a), "列表按排序口径走（a 在前）");
        state.select_next(1);
        assert_eq!(state.selected(), Some(task_b), "不是按创建顺序（b 先建）");
    }

    #[test]
    fn switching_to_the_list_view_does_not_read_the_archive_lists() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();

        state.set_view(ViewMode::List);
        assert!(state.archive_tasks().is_empty(), "列表视图不读归档清单");
        assert_eq!(state.archive_counts().total(), 1, "但徽标计数必须准");
    }

    #[test]
    fn the_card_menu_status_action_hands_the_refusal_back_to_the_page() {
        // 卡片菜单的「状态…」走 `set_task_status`，内核的拒绝**原样上抛** —— 页面靠
        // 拿到 `Err` 才能说清「为什么没改成」。用 `cancelled` 试：它只许回 `todo`。
        let mut state = memory_state();
        let id = add_task(&mut state, "t");

        state.set_task_status(id, TaskStatus::Doing).unwrap();
        assert_eq!(
            state.tasks_in_status(TaskStatus::Doing).len(),
            1,
            "合法迁移已落地"
        );

        state.set_task_status(id, TaskStatus::Cancelled).unwrap();
        assert!(
            state.set_task_status(id, TaskStatus::Doing).is_err(),
            "`cancelled` 去别的列必须被拒"
        );
        assert_eq!(
            state.tasks_in_status(TaskStatus::Cancelled).len(),
            1,
            "被拒之后状态原地不动"
        );
        assert!(
            state.set_task_status(id, TaskStatus::Todo).is_ok(),
            "回 `todo` 是状态机允许的那一条"
        );
    }

    #[test]
    fn a_task_created_for_a_column_is_moved_into_that_column() {
        // 看板列底「+ 添加任务」是两步：先建（落收集箱 / `todo`），再挪到点的那一列。
        // 这条钉住第二步 —— 挪完以后目标列得看到它、原列不再有它。
        let mut state = memory_state();
        let id = state.create_task("新任务", None).expect("建得出来");

        state.set_task_status(id, TaskStatus::Doing).unwrap();
        let doing = state.tasks_in_status(TaskStatus::Doing);
        assert_eq!(doing.len(), 1, "目标列里有它");
        assert_eq!(doing[0].id, id);
        assert_eq!(
            state.tasks_in_status(TaskStatus::Todo).len(),
            0,
            "待办列不再有它"
        );
    }
}
