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

use std::collections::{BTreeSet, HashMap};

use duck_project::{
    ArchiveCounts, ArchivedProject, ArchivedTask, BoardColumn, BoardStats, CalendarDate,
    ClearScope, ColumnSpec, DueFilter, Group, LogKind, Milestone, NewMilestone, Project,
    ProjectPatch, ProjectStore, Tag, Task, TaskCounts, TaskPatch, TaskQuery, TaskSort, TaskStatus,
    WeeklyPlan, WeeklyPlanStatus, WorkLog, now_seconds,
};
use gpui::App;

use super::prefs::{self, CalendarMode, GroupMode, ReportRange, ViewMode};
use super::report::{ReportFilters, ReportStatus};

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
    /// 撤销「改状态」→ 改回原来那一档。
    ///
    /// 只有**弹回执**的那条路径（卡片菜单「状态…」与右栏下拉）才压它：拖拽落列走
    /// [`ProjectsState::move_task`]，那条路径不弹回执（见 `mod.rs` 的写动作收口说明），
    /// 拿不到提示条也就没有「撤销」按钮可挂。
    RevertStatus {
        /// 任务。
        id: i64,
        /// 改之前的状态。
        status: TaskStatus,
    },
    /// 撤销「建了一条」→ 把刚建出来的那条删掉（复制任务）。
    ///
    /// 内核里这是一次硬删（不是归档），但删的是**本次刚建**的那条：它除了这次创建之外
    /// 没有任何来路，删掉就是精确地退回上一步。
    RemoveCreated(i64),
}

impl UndoAction {
    /// 这条动作指向的那条**任务**；不指向任务时 `None`。
    ///
    /// 用来在任务被删掉时把没用的撤销槽放下。`RestoreProject` **不**参与：项目与任务
    /// 的 id 来自两张表，同一个数字不是同一样东西，按数字模糊匹配会误伤。
    pub(crate) const fn task_id(self) -> Option<i64> {
        match self {
            Self::RestoreTask(id) | Self::RemoveCreated(id) => Some(id),
            Self::RevertStatus { id, .. } => Some(id),
            Self::RestoreProject(_) => None,
        }
    }
}

/// 撤销栈最多留几步。
///
/// 32 不是算出来的：批量操作一次最多也就勾十几条，32 足够退完一整批还多；再深既不
/// 会用到，也会让「按了十次 `⌘Z` 还没到底」变成一种困惑。
const UNDO_DEPTH: usize = 32;

/// 一条撤销槽的存活时长（秒）。
///
/// 与提示条那 7 秒（`mod.rs` 的 `UNDO_WINDOW`）是**两件事**：那个决定「撤销」按钮在
/// 屏幕上待多久，这个决定「这一步还能不能退」。按钮消失了不该把历史也一起抹掉 ——
/// 批量归档 5 条要看回执时早就过 7 秒了。60 秒的取值见
/// [`ProjectsState::push_undo`] 上方的说明（要够退完一批，又不能长到让人按 `⌘Z`
/// 退进一个早已忘记的状态）。
const UNDO_RETENTION: i64 = 60;

/// 撤销栈里的一格（设计文档 §5.6 的撤销通道）。
///
/// 归档可逆且不丢数据，所以归档后给一条带「撤销」的提示条；`seq` 是提示条定时器的
/// 守卫 —— 到点时只有「还是自己那一条」才动手，否则连做两次归档时，第一条的定时器
/// 会把第二条一起抹掉（`pages/db.rs` 的 `toast_seq` 是同一套）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UndoSlot {
    /// 序号，单调递增。
    pub(crate) seq: u64,
    /// 撤销要做的动作。
    pub(crate) action: UndoAction,
    /// 连带影响的数量：归档项目时是随它归档的任务数，归档任务时是 0。
    pub(crate) cascaded: u32,
    /// 过了这一刻（Unix 秒）这一步就退不动了。
    pub(crate) expires_at: i64,
}

/// 用户眼中的「今天」。
///
/// 时刻来自内核（Unix 秒），时区偏移来自平台层 —— 两步都在这里合起来，内核与
/// 平台层各自保持单一职责。
pub(crate) fn local_today() -> CalendarDate {
    local_date(now_seconds())
}

/// 把一个 Unix 秒换算成**用户眼中**的日历日（日历视图按完成日归位时用）。
pub(crate) fn local_date(seconds: i64) -> CalendarDate {
    CalendarDate::from_local_unix_seconds(seconds, duck_platform::local_utc_offset_seconds(seconds))
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
    ///
    /// 由 [`Self::clock`] 在每次 `reload()` 时刷新，别处不要单独改它。
    today: CalendarDate,
    /// 「今天」怎么来。
    ///
    /// 生产里就是 [`local_today`]；单测注入一个固定日期。做成一格而不是让测试
    /// 「读完再把 `today` 改回来」：`reload()` 有近三十个调用点（每个写操作收尾都
    /// 重读一遍），事后改字段撑不过下一次写操作 —— 而 `stats` 又是内核按传进去的
    /// 那一天算的，两半一旦取到不同的日期，逾期 / 今天到期 / 未排期三格就对不上。
    clock: fn() -> CalendarDate,
    /// 排序口径。
    sort: TaskSort,
    /// 列表视图的分组轴（见 [`GroupMode`]；与排序是**两条独立的轴** ——
    /// 排序决定组**内**的顺序，分组决定任务被摆成几摞）。
    group: GroupMode,
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
    /// 日历视图锚定的那一天（月视图取它所在月、周视图取它所在周；`None` = 本地今天）。
    ///
    /// 存的是**整日**而不是「年 + 月」：周视图的锚是「某一周」，月份对它是过粗的粒度。
    /// 月视图从它取 `year()` / `month()`，翻月时把锚定日夹回当月 1 号（见
    /// `ProjectsPage::step_calendar`）。
    calendar_anchor: Option<CalendarDate>,
    /// 日历的画法（月网格 / 周网格）。**落盘**，见 [`CalendarMode`]。
    calendar_mode: CalendarMode,
    /// 工作日志视图最近一段的日志（读口径见 [`Self::reload_logs`]）。
    logs: Vec<WorkLog>,
    /// 工作日志视图当前看的是哪一档（日报 / 周报 / 周计划）。
    log_kind: LogKind,
    /// 工作日志视图选中的那一篇（`None` = 落该档最近的一篇）。
    log_selected: Option<i64>,
    /// 周计划视图锚定的那一周（存的是该周内的某一天；`None` = 本地今天所在周）。
    ///
    /// **不落盘**（与日历锚同一个待遇）：周计划是「我现在要排这一周」，下次进来回到
    /// 当周才是对的 —— 记住上个月看的那一周只会让人以为计划没了。
    plan_week: Option<CalendarDate>,
    /// 最近写过的周计划（读口径见 [`Self::reload_plans`]），按周键倒序。
    plans: Vec<WeeklyPlan>,
    /// 当前这一周计划里那些任务的**现状**（顺序跟随计划里的 `task_ids`）。
    ///
    /// 与 [`Self::tasks`] 是两回事：那一份是「当前看板筛选下的任务」，这一份只认
    /// 计划里记的 id（含已归档的、也含当前筛选之外的 —— 计划是历史事实）。
    plan_tasks: Vec<Task>,
    /// 当前这一周里到期的任务（「＋ 加入计划」的候选池，**排除已完成 / 已取消**）。
    week_tasks: Vec<Task>,
    /// 报表的时间范围档（`report.rs` 用它决定趋势桶数和「范围内完成」的口径）。
    ///
    /// **落盘**（见 `prefs::ReportRange`）：它表达的是「我平时就这么看报表」。
    report_range: ReportRange,
    /// 报表的状态筛选档（**只在内存里活着、不落盘**）：与工具条那个「显示已完成」同一类
    /// 一次性视角，不是长期偏好。
    report_status: ReportStatus,
    /// 报表的标签筛选（多选 OR；空 = 不限）。同样**不落盘**。
    ///
    /// 用 `Vec` 而不是 `BTreeSet`：它是「勾了哪几个」的一张小列表，行数是个位数，
    /// 界面按标签本身的名字顺序画 chips，不依赖这里的顺序。
    report_tags: Vec<i64>,
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
    /// **可撤销的变更，最新的在末尾**（`⌘Z` 一路往回退）。
    ///
    /// 早先只有一个槽（撤完就空）。改成栈是因为「批量归档 5 条，连按 5 次 `⌘Z`」是真实
    /// 用法，单槽只能退回最后一次。两重上界：
    ///
    /// - 深度 [`UNDO_DEPTH`]（超出丢最旧的）；
    /// - 时限 [`UNDO_RETENTION`]（每条自带截止时刻，过了就当没有）。
    ///
    /// ⚠️ 时限不是可选装饰：这个栈只记**四类动作**（归档 / 恢复项目 / 改状态 / 复制），
    /// 不记编辑正文、拖拽落点这类路径。留着一条几分钟前的归档槽，会让用户「改完标题顺手
    /// 按 `⌘Z`」退到一个他早就忘了的状态 —— 撤销是「刚才那几下」的退路，不是无限历史。
    undo_stack: Vec<UndoSlot>,
    /// 撤销槽的序号源。
    undo_seq: u64,
    /// 列表视图里被勾选的任务（原型 `S.checked`）。
    ///
    /// **只在内存里活着、不落盘**：勾选表达的是「我现在要对这几条下手」这个临时意图，
    /// 重启后还留着一批勾选只会让人误以为它们是某种持久状态。
    ///
    /// 用 `BTreeSet` 而不是 `HashSet`：批量动作要按确定顺序写库（回执里报的条数、
    /// 出错时停在哪一条都得以同一顺序复现），`BTreeSet` 的遍历顺序天然稳定。
    checked: BTreeSet<i64>,
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
            clock: local_today,
            sort: prefs.task_sort(),
            group: prefs.group_mode(),
            smart: None,
            tag: None,
            tags: Vec::new(),
            show_done: true,
            projects_collapsed: false,
            sidebar: SidebarCounts::default(),
            search: String::new(),
            view: prefs.view_mode(),
            calendar_anchor: None,
            calendar_mode: prefs.calendar(),
            logs: Vec::new(),
            log_kind: LogKind::Daily,
            log_selected: None,
            plan_week: None,
            plans: Vec::new(),
            plan_tasks: Vec::new(),
            week_tasks: Vec::new(),
            report_range: prefs.report(),
            report_status: ReportStatus::default(),
            report_tags: Vec::new(),
            archive_search: String::new(),
            archive_tasks: Vec::new(),
            archive_projects: Vec::new(),
            archive_counts: ArchiveCounts::default(),
            archive_selected: None,
            undo_stack: Vec::new(),
            undo_seq: 0,
            checked: BTreeSet::new(),
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
    ///
    /// 「今天」在**读之前**先定下来（取 [`Self::clock`]）：`stats` 是内核按这一天算的
    /// —— 逾期 / 今天到期 / 未排期三格都吃它，左栏的「本周」也跟着走。先按一个日期读、
    /// 再改字段，只会让统计与其余几格的口径互相打架。
    pub(crate) fn reload(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        self.today = (self.clock)();
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
        self.reload_logs();
        // 撤销栈的时限在这里落地（每条自带截止时刻）。放在收口处而不是各个写口：
        // 「读一次能不能撤」永远发生在 `reload()` 之后，剪在这里就不会漏。
        // ⚠️ 不做「按归档区清单剪」那一版：`archive_tasks` **只在归档中心可见时才读**
        // （见 [`Self::reload_archive`]），拿它当判据会在看板上把栈整个剪空。归档区被
        // 清空走的是 [`Self::clear_archive`] 那条路（整个栈一起清）。
        self.prune_undo(now_seconds());
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
                // 勾选同理：列表里已经没有的行不能再算进「已选中 N 项」——
                // 否则切个范围回来，批量条会报着一个屏幕上看不到的数字。
                let visible: std::collections::HashSet<i64> =
                    self.tasks.iter().map(|task| task.id).collect();
                self.checked.retain(|id| visible.contains(id));
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

    /// 库里**全部**顶层任务（绕过范围 / 筛选 / 分档），导出的「全部任务」那一档用。
    ///
    /// 现查而不是常驻缓存：它只在导出弹窗打开时被读一次，而常驻一份「全量」等于每次
    /// 刷新都多跑一趟查询、多占一份内存 —— 为一年用两次的功能不划算。
    ///
    /// 错误原样交回调用方（不在这一层吞掉）：调用方是弹窗的开门动作，有地方弹回执。
    pub(crate) fn all_tasks(&self) -> duck_project::Result<Vec<Task>> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.list_tasks(&TaskQuery::new().top_level_only())
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

    /// 当前列表分组轴。
    pub(crate) const fn group(&self) -> GroupMode {
        self.group
    }

    /// 当前智能列表档位（`None` = 不限）。
    pub(crate) const fn smart(&self) -> Option<SmartFilter> {
        self.smart
    }

    /// 左栏点中的标签（`None` = 不限）。
    pub(crate) const fn tag(&self) -> Option<i64> {
        self.tag
    }

    /// 报表的时间范围档。
    pub(crate) const fn report_range(&self) -> ReportRange {
        self.report_range
    }

    /// 报表的状态筛选档。
    pub(crate) const fn report_status(&self) -> ReportStatus {
        self.report_status
    }

    /// 报表的标签筛选（空 = 不限）。
    pub(crate) fn report_tags(&self) -> &[i64] {
        &self.report_tags
    }

    /// 报表当前的全部筛选条件（`report::build` 的入参）。
    ///
    /// 现拼一份而不是在状态里养一个 [`ReportFilters`] 字段：三格筛选的**真源**是状态里
    /// 那三个字段，再存一份拼好的就成了第二处真源 —— 改了一处忘了另一处，报表就会跟
    /// 筛选条上亮着的 chips 对不上。标签那一列是个位数的列表，每帧克隆一次的代价可以忽略。
    pub(crate) fn report_filters(&self) -> ReportFilters {
        ReportFilters {
            range: self.report_range,
            status: self.report_status,
            tags: self.report_tags.clone(),
        }
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

    /// 撤销栈顶那一步（提示条的「撤销」按钮与 `⌘Z` 都作用在它上面）；空栈 = `None`。
    ///
    /// ⚠️ 读它之前不过期检查：这里只回答「栈顶是谁」，而「还能不能退」由 [`Self::undo`]
    /// 自己先剪一遍再答（剪枝要读时钟，读口保持纯粹更省心）。
    pub(crate) fn undo_slot(&self) -> Option<UndoSlot> {
        self.undo_stack.last().copied()
    }

    /// 还能退几步（提示条上写「还可撤销 N 步」）。
    pub(crate) fn undo_depth(&self) -> usize {
        self.undo_stack.len()
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

    /// 改列表分组轴。
    ///
    /// **不重读任务**：分组只是把已经查出来的那一批换一种摆法，筛选与排序都没动，
    /// 重读一趟数据库纯属白付（[`Self::set_show_done`] 要重读，是因为它真在筛）。
    pub(crate) fn set_group(&mut self, group: GroupMode) {
        self.group = group;
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

    /// 换报表的时间范围档。
    ///
    /// **不重读任务**：范围只是换一种分桶方式去算已经查出来的那一批，筛选与排序都没动
    /// （理由同 [`Self::set_group`]）。
    pub(crate) fn set_report_range(&mut self, range: ReportRange) {
        self.report_range = range;
    }

    /// 换报表的状态筛选档（纯界面态，同样不重读）。
    pub(crate) fn set_report_status(&mut self, status: ReportStatus) {
        self.report_status = status;
    }

    /// 报表的标签筛选：已勾中就取消，否则加上（**多选**）。
    pub(crate) fn toggle_report_tag(&mut self, id: i64) {
        match self.report_tags.iter().position(|current| *current == id) {
            Some(index) => {
                self.report_tags.remove(index);
            }
            None => self.report_tags.push(id),
        }
    }

    /// 清掉报表的全部标签筛选。
    pub(crate) fn clear_report_tags(&mut self) {
        self.report_tags.clear();
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
        self.reload_logs();
        self.reload_plans();
    }

    /// 读工作日志（只在日志视图可见时读；口径 = 最近 30 天，含今天）。
    fn reload_logs(&mut self) {
        if self.view != ViewMode::Log {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let to = self.today;
        let from = to.add_days(-29);
        match store.list_logs_between(from, to) {
            Ok(logs) => self.logs = logs,
            Err(err) => self.fail(err),
        }
    }

    /// 读周计划（只在日志视图的「周计划」档可见时读）。
    ///
    /// 一次读三份：最近写过的计划（左清单）、当前这一周计划里任务的**现状**、
    /// 当前这一周到期的任务（候选池）。分三次查而不是拼一条大 SQL：
    /// 三份的来源与过滤条件都不同，粘在一起只会得到一条谁都改不动的语句。
    fn reload_plans(&mut self) {
        if self.view != ViewMode::Log || self.log_kind != LogKind::Plan {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let anchor = self.plan_week.unwrap_or(self.today);
        let plans = match store.list_plans(PLAN_LIST_LIMIT) {
            Ok(plans) => plans,
            Err(err) => {
                self.fail(err);
                return;
            }
        };
        let task_ids = plans
            .iter()
            .find(|plan| plan.week_key == anchor.week_key())
            .map(|plan| plan.task_ids.clone())
            .unwrap_or_default();
        match store.list_tasks_by_ids(&task_ids) {
            Ok(tasks) => self.plan_tasks = tasks,
            Err(err) => {
                self.fail(err);
                return;
            }
        }
        // 候选池复用内核对「本周到期」的口径（`DueFilter::ThisWeek` 自带「未完成 /
        // 未取消」），不在这里另写一条 `BETWEEN`。
        match store.list_tasks(&TaskQuery::new().due(DueFilter::ThisWeek).today(anchor)) {
            Ok(tasks) => self.week_tasks = tasks,
            Err(err) => {
                self.fail(err);
                return;
            }
        }
        self.plans = plans;
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
            // 日历 / 时间线 / 报表 / 日志四个视图没有「视觉顺序的选中」概念：
            // 它们的主体是网格 / 分段 / 统计卡，不是一张张可选中的卡片。
            ViewMode::Calendar | ViewMode::Timeline | ViewMode::Report | ViewMode::Log => {}
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

    /// 压一条撤销槽。
    ///
    /// 两件事顺手做掉（而不是留给读口）：序号自增（提示条定时器的守卫）、盖上截止时刻。
    ///
    /// 压之前**先剪一遍**：截断到 [`UNDO_DEPTH`]、丢掉过期的。放在这里而不是只放在
    /// `undo()` 里，是因为批量操作会在一个循环里连压十几条 —— 先剪能让栈长有个上界。
    ///
    /// 60 秒（[`UNDO_RETENTION`]）够不够：批量归档 5 条之后一条条按 `⌘Z` 退回来，
    /// 手速再慢也在十秒内；而不设时限的话，一次「改完标题顺手按 `⌘Z`」会退到一个用户
    /// 早就忘了的归档动作上（见字段 `undo_stack` 的注记）。
    fn push_undo(&mut self, action: UndoAction, cascaded: u32) {
        let now = now_seconds();
        self.prune_undo(now);
        self.undo_seq += 1;
        self.undo_stack.push(UndoSlot {
            seq: self.undo_seq,
            action,
            cascaded,
            expires_at: now + UNDO_RETENTION,
        });
        let overflow = self.undo_stack.len().saturating_sub(UNDO_DEPTH);
        if overflow > 0 {
            self.undo_stack.drain(..overflow);
        }
    }

    /// 把过期的撤销槽剪掉。
    ///
    /// `now` 由调用方给（而不是这里读时钟）：单测要能把「六十秒后」这一步直接喂进来，
    /// 不必真的睡一分钟。
    fn prune_undo(&mut self, now: i64) {
        self.undo_stack.retain(|slot| slot.expires_at > now);
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
        self.forget_undo_for_tasks(&[id]);
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
        // 归档区整个清空了，栈里任何「撤销归档」都不可能再成功 —— 一次清干净，
        // 而不是留给 `undo()` 一条条去撞「记录不存在」。
        self.undo_stack.clear();
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
        // 删掉的任务若正是撤销槽指的那条（先归档、撤销回来、再删），槽跟着作废 ——
        // 判据按 `task_id()` 而不是单个变体：改状态 / 复制任务留下的槽同样指向一条任务。
        self.forget_undo_for_tasks(&[id]);
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

    // ------------------------------------------------------------------
    // 列表批量勾选（原型 `S.checked`）
    // ------------------------------------------------------------------

    /// 勾选 / 取消勾选一条（列表行最左边的勾）。
    pub(crate) fn toggle_checked(&mut self, id: i64) {
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
    }

    /// 表头那个勾：这一屏已经全勾上了就整批取消，否则把这一屏全勾上。
    ///
    /// 用的是「当前 `tasks`」而不是「库里所有任务」：勾选是给**看得见的行**用的，
    /// 把屏幕外的任务也勾上，批量条报的数字就对不上了。
    pub(crate) fn toggle_check_all(&mut self) {
        if self.all_checked() {
            self.checked.clear();
            return;
        }
        self.checked.extend(self.tasks.iter().map(|task| task.id));
    }

    /// 批量条上的「取消选择」。
    pub(crate) fn clear_checked(&mut self) {
        self.checked.clear();
    }

    /// 已勾选的条数（批量条上的「N 项已选中」）。
    pub(crate) fn checked_count(&self) -> usize {
        self.checked.len()
    }

    /// 这一条勾了没有（列表行画勾用）。
    pub(crate) fn is_checked(&self, id: i64) -> bool {
        self.checked.contains(&id)
    }

    /// 当前这一屏是不是已经全部勾上（表头勾的选中态）。
    ///
    /// 空列表一律 `false`：一屏什么都没有时表头勾不该是「已全选」的样子。
    pub(crate) fn all_checked(&self) -> bool {
        !self.tasks.is_empty()
            && self
                .tasks
                .iter()
                .all(|task| self.checked.contains(&task.id))
    }

    /// 已勾选的 id（升序 —— 批量动作的写库顺序必须可复现，出错时才知道停在哪）。
    pub(crate) fn checked_ids(&self) -> Vec<i64> {
        self.checked.iter().copied().collect()
    }

    // ------------------------------------------------------------------
    // 批量动作
    // ------------------------------------------------------------------

    /// 批量改状态（批量条上的「标记完成」就是 `Done` 这一档的特例）。
    ///
    /// 走与单条同一个内核入口（`store.move_task`）：状态迁移的联动（`completed_at`、
    /// 进度）只在内核里有一份，批量另写一条迟早分叉。逐条写、一条失败就停，并把
    /// **已经改掉的条数**如实交回调用方 —— 中途失败不能把前面成功的当成没发生。
    pub(crate) fn bulk_set_status(
        &mut self,
        ids: &[i64],
        status: TaskStatus,
    ) -> duck_project::Result<usize> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let mut done = 0;
        for id in ids {
            store.move_task(*id, status, 0)?;
            done += 1;
        }
        self.reload();
        Ok(done)
    }

    /// 批量改字段（优先级 / 移入项目）。
    ///
    /// 收成一个方法而不是每条各开一个：内核那边都是同一个 `update_task`，
    /// 区别只在 `patch` 怎么组，而 `patch` 是调用方最清楚的事。
    pub(crate) fn bulk_patch(
        &mut self,
        ids: &[i64],
        patch: &TaskPatch,
    ) -> duck_project::Result<usize> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let mut done = 0;
        for id in ids {
            store.update_task(*id, patch.clone())?;
            done += 1;
        }
        self.reload_tasks();
        Ok(done)
    }

    /// 批量删除（**物理删除**，调用方必须先弹二次确认 —— 与单条删除同一条纪律）。
    pub(crate) fn bulk_delete(&mut self, ids: &[i64]) -> duck_project::Result<usize> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let mut done = 0;
        for id in ids {
            store.delete_task(*id)?;
            done += 1;
        }
        self.forget_undo_for_tasks(ids);
        self.reload();
        Ok(done)
    }

    /// 改一条任务的状态（卡片菜单 / 右栏下拉），错误**原样交给调用方**。
    ///
    /// 与拖拽走的 [`Self::move_task`] 分开：拖拽的落点是自己人（列头就摆在那儿），
    /// 失败在状态里记一笔就够；菜单要弹一条即时回执说明「为什么没改成」——比如从
    /// 「已取消」出发去别的列，内核的状态机会拒（`cancelled` 只许回 `todo`），
    /// 这时候必须把原因说出来，而不是静默什么都不发生。
    ///
    /// 成功时压一条撤销槽（原型的「已移动到「X」」提示条带「撤销」）：改状态是可逆的
    /// —— 再写一次原状态就回去了。**改之前那一档必须先读**，`move_task` 之后再读就只剩
    /// 新值了。
    pub(crate) fn set_task_status(
        &mut self,
        id: i64,
        status: TaskStatus,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let previous = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.status);
        store.move_task(id, status, 0)?;
        if let Some(previous) = previous {
            self.push_undo(
                UndoAction::RevertStatus {
                    id,
                    status: previous,
                },
                0,
            );
        }
        self.reload();
        Ok(())
    }

    /// 撤销**栈顶**那一步（§5.6 的撤销通道）。连按 `⌘Z` 就是一路往回退。
    ///
    /// - 没有可撤的（空栈，或栈里的都过期了）→ `Ok(None)`（按 `⌘Z` 是空操作，不算错）；
    /// - 槽位指向的对象已经不在归档里（被别处恢复了 / 被彻底删了）→ `Err`，同时把
    ///   这条没用的槽放下 —— 留着一个按下去必然失败的按钮比没有按钮更糟。
    ///   只放下**这一条**：栈里更早的那些该留着，它们指向别的对象。
    /// - 库没打开 → `Err` 且**把槽放回去**：这一次撤不动不是「这一步撤不了」，
    ///   白吃一步会让 `⌘Z` 莫名其妙地少退一层。
    ///
    /// 撤销**本身不再压新槽**：撤销就是「把上一步退回去」，再按一次不该又走一遍
    /// （也因此连按就是逐步往回退，不会原地打转）。
    pub(crate) fn undo(&mut self) -> duck_project::Result<Option<UndoAction>> {
        // 先剪再取：过期的那一步当它不存在，而不是取出来再报「撤销失败」。
        self.prune_undo(now_seconds());
        let Some(slot) = self.undo_stack.pop() else {
            return Ok(None);
        };
        let action = slot.action;
        let Some(store) = &self.store else {
            self.undo_stack.push(slot);
            return Err(store_not_open());
        };
        let outcome: duck_project::Result<u32> = match action {
            UndoAction::RestoreTask(id) => store.restore_task(id).map(|()| 0),
            UndoAction::RestoreProject(id) => store.restore_project(id),
            // 落点写 0 = 列首：与 `move_task` 那条路径同口径，位置由内核重排，这里不猜。
            UndoAction::RevertStatus { id, status } => store.move_task(id, status, 0).map(|()| 0),
            UndoAction::RemoveCreated(id) => store.delete_task(id).map(|()| 0),
        };
        match outcome {
            Ok(_) => {
                self.reload();
                Ok(Some(action))
            }
            // 这条已经弹出去了（见文档注释）：失败即作废，不再放回。
            Err(err) => Err(err),
        }
    }

    /// 这条撤销槽已经被别处兑现 / 作废 ⇒ 从栈里摘掉**所有**同动作的条目。
    ///
    /// 判据只认「同一个动作」：恢复任务 3 不该顺手把「撤销归档项目 7」也清掉。
    /// 是 `retain` 而不是「只看栈顶」：栈里可能存着同一动作的多份（归档 → 撤销 →
    /// 再归档），它们指向同一个对象，一条被兑现了其余几份同样按不动。
    fn forget_undo(&mut self, action: UndoAction) {
        self.undo_stack.retain(|slot| slot.action != action);
    }

    /// 刚删掉一批任务 ⇒ 指向其中任何一条的撤销条目都作废（见 [`UndoAction::task_id`]）。
    ///
    /// 收口成一批而不是逐条：`bulk_delete` 要在**循环里**删（`store` 的借用出不了循环），
    /// 而借还没还完就动 `undo_stack` 会被借用检查拦下。
    fn forget_undo_for_tasks(&mut self, ids: &[i64]) {
        self.undo_stack
            .retain(|slot| slot.action.task_id().is_none_or(|id| !ids.contains(&id)));
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

    // ------------------------------------------------------------------
    // S5 已落地：完整表单 / 置顶 / 复制 / 里程碑 / 看板列 / 标签管理 / 工作日志
    // （复活弹窗、重命名、复制任务都在这一段里；未做的项见设计文档 §9「S5 复核」）
    // ------------------------------------------------------------------

    /// 用完整表单的字段建项目（项目编辑弹窗「创建」）。
    ///
    /// 与 [`Self::create_project`]（只给名字、自动去重的老路径）并存：后者是左栏
    /// 「点一下就多一个项目」的快捷入口，这里是带名字 / 描述 / 色板 / 日期的完整表单。
    pub(crate) fn create_project_full(&mut self, new: duck_project::NewProject) -> Option<i64> {
        let store = self.store.as_ref()?;
        match store.create_project(new) {
            Ok(id) => {
                self.reload();
                self.scope = BoardScope::Project(id);
                Some(id)
            }
            Err(err) => {
                self.fail(err);
                None
            }
        }
    }

    /// 用完整表单的字段建任务（编辑弹窗「创建」与快速新建 Spotlight 共用）。
    pub(crate) fn create_task_full(&mut self, new: duck_project::NewTask) -> Option<i64> {
        let store = self.store.as_ref()?;
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

    /// 快速新建（Spotlight）：把自然语言解析结果落成任务。
    ///
    /// `#标签` 里**不存在的标签直接创建**（内核 `ensure_tag` 的语义，颜色取默认
    /// 项目色）；`@项目` 只认名字完全一致（ASCII 不敏感）的项目，认不出就落收集箱
    /// —— 静默把任务挂到一个猜出来的项目上比落收集箱更危险。
    pub(crate) fn create_task_from_quick(
        &mut self,
        parsed: &super::quickparse::QuickParse,
        preset_project: Option<i64>,
    ) -> Option<i64> {
        let store = self.store.as_ref()?;
        let project_id = parsed
            .project
            .as_ref()
            .and_then(|name| {
                self.projects
                    .iter()
                    .find(|project| project.name.eq_ignore_ascii_case(name))
                    .map(|project| project.id)
            })
            .or(preset_project);
        let mut tag_ids = Vec::new();
        // 内层收口：`store` 是对 `self` 的不可变借用，`self.fail` 是可变借用 ——
        // 两者不能在同一个循环里交错，先把可能失败的整段跑完再统一处理错误。
        let ensured: duck_project::Result<Vec<i64>> = (|| {
            tag_ids.clear();
            for name in &parsed.tags {
                tag_ids.push(store.ensure_tag(name, duck_project::DEFAULT_PROJECT_COLOR)?);
            }
            Ok(std::mem::take(&mut tag_ids))
        })();
        let tag_ids = match ensured {
            Ok(ids) => ids,
            Err(err) => {
                self.fail(err);
                return None;
            }
        };
        let mut new = duck_project::NewTask::new(parsed.title.clone()).tags(tag_ids);
        if let Some(project_id) = project_id {
            new = new.project(project_id);
        }
        if let Some(priority) = parsed.priority {
            new = new.priority(priority);
        }
        if parsed.due.is_some() {
            new = new.dates(None, parsed.due);
        }
        if let Some(time) = &parsed.due_time {
            new = new.due_time(time.clone());
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

    /// 「从文件 / 文本导入」：把解析好的每一行落成任务。
    ///
    /// `fallback_project` 是「没写 `@项目`」时的落点（调用方给当前范围）—— 与快速新建
    /// 同一条口径（见 `ProjectsPage::submit_spotlight`），比原型更贴手：在某个项目下
    /// 粘一串任务，八成就是想把它们放进这个项目。
    ///
    /// `skip_duplicates` 打开时按**标题**跳过库里已有的（以及本批里重复的）——
    /// 比对键由 [`super::import::title_key`] 定（trim + 小写），批内去重也在那里。
    /// 「导一份自己刚导过的文件」是最常见的误操作，默认开。
    ///
    /// 状态与进度直接来自行（`Row::resolved_*`）：`- [x]` 与 `status=done` 建出来就是
    /// 「已完成 + 100%」，不给卡片留「已完成但 0%」这种自相矛盾的读数。
    ///
    /// ⚠️ 一条失败就**整体停下**并把错误原样交回调用方：半截导入比没导入更糟
    /// （用户不知道从第几行重来）。已经建成的那些**不回滚** —— 内核的 `create_task`
    /// 每条一个事务，删掉它们会连用户自己刚建的条目一起误伤。
    pub(crate) fn import_lines(
        &mut self,
        rows: &[super::import::Row],
        fallback_project: Option<i64>,
        skip_duplicates: bool,
    ) -> duck_project::Result<ImportOutcome> {
        let (rows, skipped) = if skip_duplicates {
            super::import::partition(rows, &self.existing_title_keys())
        } else {
            (rows.to_vec(), 0)
        };
        // 收成一个闭包：`store` 是对 `self` 的不可变借用，而收尾的 `self.reload()`
        // 要可变借用 —— 借用不能跨过终点，先把整段写入跑完再落收尾。
        let outcome = (|| -> duck_project::Result<usize> {
            if rows.is_empty() {
                return Ok(0);
            }
            let store = self.store.as_ref().ok_or_else(store_not_open)?;
            let mut created = 0;
            for row in &rows {
                let project_id = row
                    .project
                    .as_ref()
                    .and_then(|name| {
                        self.projects
                            .iter()
                            .find(|project| project.name.eq_ignore_ascii_case(name))
                            .map(|project| project.id)
                    })
                    .or(fallback_project);
                let mut tag_ids = Vec::new();
                for name in &row.tags {
                    tag_ids.push(store.ensure_tag(name, duck_project::DEFAULT_PROJECT_COLOR)?);
                }
                let status = row.resolved_status();
                let mut new = duck_project::NewTask::new(row.title.clone())
                    .status(status)
                    .progress(row.resolved_progress())
                    .starred(row.starred)
                    .tags(tag_ids);
                if let Some(project_id) = project_id {
                    new = new.project(project_id);
                }
                if let Some(priority) = row.priority {
                    new = new.priority(priority);
                }
                if let Some(quadrant) = row.quadrant {
                    new = new.quadrant(quadrant);
                }
                if row.start.is_some() || row.due.is_some() {
                    new = new.dates(row.start, row.due);
                }
                if let Some(time) = &row.due_time {
                    new = new.due_time(time.clone());
                }
                if row.estimate_hours.is_some() || row.actual_hours.is_some() {
                    new = new.hours(row.estimate_hours, row.actual_hours);
                }
                if let Some(description) = &row.description {
                    new = new.description(description.clone());
                }
                if let Some(note) = &row.note {
                    new = new.note(note.clone());
                }
                store.create_task(new)?;
                created += 1;
            }
            Ok(created)
        })();
        // 成败都重读：失败前可能已经建成前几条，界面得反映真实库存。
        self.reload();
        outcome.map(|created| ImportOutcome { created, skipped })
    }

    /// 库里**全部**未归档顶层任务的标题比对键，给导入去重用。
    ///
    /// 走 `TaskQuery::top_level_only()`（与 [`Self::all_tasks`] 同一条查询）：
    /// 子任务也去重只会把「修登录（子任务）」挡在门外，而用户根本看不到它。
    /// 查询失败时返回空表 —— 去重是**便利**，查不到就不去重，不能因此把导入整个拦下。
    pub(crate) fn existing_title_keys(&self) -> Vec<String> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        store
            .list_tasks(&TaskQuery::new().top_level_only())
            .map(|tasks| {
                tasks
                    .iter()
                    .map(|task| super::import::title_key(&task.title))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 复制任务：新建一条字段相同（含标签 / 优先级 / 四象限 / 日期 / 工时 / 进度）
    /// 的任务，标题加「（副本）」。子任务与里程碑**不**复制（原型同口径：
    /// `duplicateTask` 只拷贝卡片本身的字段）。
    pub(crate) fn duplicate_task(&mut self, id: i64) -> duck_project::Result<Option<i64>> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let Some(source) = store.get_task(id)? else {
            return Err(duck_project::ProjectError::NotFound("task"));
        };
        let mut new = duck_project::NewTask::new(format!("{}（副本）", source.title))
            .description(source.description_md.clone())
            .status(TaskStatus::Todo)
            .priority(source.priority)
            .quadrant(source.quadrant)
            .dates(source.start_date, source.due_date)
            .hours(source.estimate_hours, source.actual_hours)
            .starred(source.starred)
            .block_reason(source.block_reason.clone())
            .note(source.note.clone())
            .tags(source.tags.iter().map(|tag| tag.id));
        if let Some(project_id) = source.project_id {
            new = new.project(project_id);
        }
        if source.due_time.is_some() {
            new = new.due_time(source.due_time.clone().unwrap_or_default());
        }
        let created = store.create_task(new)?;
        // 撤销槽指向刚建出来的那一条（原型「已复制为「X」」带「撤销」）。
        self.push_undo(UndoAction::RemoveCreated(created), 0);
        self.reload_tasks();
        Ok(Some(created))
    }

    /// 给任务添加一个里程碑（详情面板「添加里程碑」）。
    pub(crate) fn add_milestone(&mut self, new: NewMilestone) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.create_milestone(new)?;
        self.reload();
        Ok(())
    }

    /// 删除一个里程碑。
    pub(crate) fn remove_milestone(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_milestone(id)?;
        self.reload();
        Ok(())
    }

    /// 达成 / 取消达成一个里程碑。
    pub(crate) fn toggle_milestone_reached(
        &mut self,
        id: i64,
        reached: bool,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        if reached {
            store.reach_milestone(id)?;
        } else {
            store.unreach_milestone(id)?;
        }
        self.reload();
        Ok(())
    }

    /// 某个任务的里程碑（详情面板用）。
    pub(crate) fn task_milestones(&self, task_id: i64) -> Vec<Milestone> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        store.list_task_milestones(task_id).unwrap_or_default()
    }

    /// 整体替换看板列（列设置弹窗的「保存」）。
    pub(crate) fn replace_columns(&mut self, columns: &[ColumnSpec]) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.replace_columns(columns)?;
        self.reload();
        Ok(())
    }

    /// 新建 / 重命名 / 改色标签；返回新 id（重命名与改色返回 `None`）。
    pub(crate) fn create_tag(&mut self, name: &str, color: &str) -> duck_project::Result<i64> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        let id = store.create_tag(duck_project::NewTag::new(name).color(color))?;
        self.reload();
        Ok(id)
    }

    pub(crate) fn update_tag(
        &mut self,
        id: i64,
        name: Option<&str>,
        color: Option<&str>,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.update_tag(id, name, color)?;
        self.reload();
        Ok(())
    }

    pub(crate) fn delete_tag(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_tag(id)?;
        // 选中的标签刚被删掉，筛选跟着放下（`reload` 里的守卫也会兜一道）。
        if self.tag == Some(id) {
            self.tag = None;
        }
        self.reload();
        Ok(())
    }

    /// 收藏 / 取消收藏标签。排序口径在内核（`list_tags` 的 `ORDER BY
    /// is_favorite DESC, name`），这里只拨开关、重载后顺序自动就位。
    pub(crate) fn set_tag_favorite(&mut self, id: i64, favorite: bool) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.set_tag_favorite(id, favorite)?;
        self.reload();
        Ok(())
    }

    /// 更新项目（项目编辑弹窗「保存」）。
    pub(crate) fn update_project(
        &mut self,
        id: i64,
        patch: ProjectPatch,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.update_project(id, patch)?;
        self.reload();
        Ok(())
    }

    /// 删除项目（项目编辑弹窗的「删除项目」——**不可逆**，调用方先二次确认）。
    pub(crate) fn delete_project(&mut self, id: i64) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_project(id)?;
        if self.scope.is_project(id) {
            self.scope = BoardScope::All;
        }
        self.reload();
        Ok(())
    }

    /// 日历视图锚定的那一天；`None` = 本地今天。
    pub(crate) fn calendar_anchor(&self) -> Option<CalendarDate> {
        self.calendar_anchor
    }

    /// 换日历视图的锚定日（翻页 / 回到今天 / 右键「查看这周」）。
    pub(crate) fn set_calendar_anchor(&mut self, anchor: Option<CalendarDate>) {
        self.calendar_anchor = anchor;
    }

    /// 日历的画法（月 / 周）。
    pub(crate) const fn calendar_mode(&self) -> CalendarMode {
        self.calendar_mode
    }

    /// 换日历的画法。**只改画法、不动锚定日**：锚是整日，两种画法共用同一个锚
    /// ——月 ↔ 周互切时停在原地，而不是跳回今天。
    pub(crate) fn set_calendar_mode(&mut self, mode: CalendarMode) {
        self.calendar_mode = mode;
    }

    /// 工作日志视图当前档位。
    pub(crate) const fn log_kind(&self) -> LogKind {
        self.log_kind
    }

    /// 换工作日志的档位（日报 / 周报 / 周计划）。
    ///
    /// 换档时**放下选中项**：选中态是「某档里的一篇」，跨档沿用会指到一篇不在清单里的
    /// 日志上（右栏就会空着，看着像坏了）。
    ///
    /// 进「周计划」档要顺手读一次周计划：那一档的右栏不是日志正文，而是计划面板，
    /// 数据源也不同（`weekly_plans` 表）。不在这里读的话，切过去只会看到上一次留下的
    /// 空面板。
    pub(crate) fn set_log_kind(&mut self, kind: LogKind) {
        if self.log_kind == kind {
            return;
        }
        self.log_kind = kind;
        self.log_selected = None;
        if kind == LogKind::Plan {
            self.reload_plans();
        }
    }

    /// 当前档位下的日志，按日期倒序。
    pub(crate) fn logs_of_kind(&self, kind: LogKind) -> Vec<&WorkLog> {
        let mut rows: Vec<&WorkLog> = self.logs.iter().filter(|log| log.kind == kind).collect();
        rows.sort_by_key(|log| std::cmp::Reverse(log.date.days_since_epoch()));
        rows
    }

    /// 某天某档的日志。
    pub(crate) fn log_of(&self, date: CalendarDate, kind: LogKind) -> Option<&WorkLog> {
        self.logs
            .iter()
            .find(|log| log.date == date && log.kind == kind)
    }

    /// 工作日志视图右侧展开的是哪一篇。
    ///
    /// 选中的那篇被删掉（或还没选）时回落到该档最近的一篇 —— 空着右栏会让人以为
    /// 「日志丢了」，而不是「还没选」。
    pub(crate) fn selected_log(&self) -> Option<&WorkLog> {
        let kind = self.log_kind;
        if let Some(id) = self.log_selected {
            // ⚠️ 两个条件合成**一条谓词**，不是两层 `if`：分开写会被 clippy 判
            // `collapsible_if`，而它建议的 `let` 链到 1.88 才稳定（本仓 MSRV 1.85）。
            if let Some(log) = self
                .logs
                .iter()
                .find(|log| log.id == id && log.kind == kind)
            {
                return Some(log);
            }
        }
        self.logs
            .iter()
            .filter(|log| log.kind == kind)
            .max_by_key(|log| log.date.days_since_epoch())
    }

    /// 选中一篇日志。
    pub(crate) fn select_log(&mut self, id: i64) {
        self.log_selected = Some(id);
    }

    /// 写某天某档的日志（工作日志编辑弹窗「保存」），写完重读日志清单。
    ///
    /// `ai_generated` 只记**来源**：弹窗里点过「AI 生成」就是 `true`。它不影响任何
    /// 行为（内核那列的注释也这么写），界面据实展示而已 —— 一句「这段是 AI 写的」
    /// 比事后猜有用。
    pub(crate) fn save_log(
        &mut self,
        date: CalendarDate,
        kind: LogKind,
        content: &str,
        tags: &[String],
        ai_generated: bool,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.upsert_log(date, kind, content, ai_generated, &[], tags)?;
        self.reload_logs();
        Ok(())
    }
}

/// 周计划左清单最多列几周（约一年）。
///
/// 与「本周 / 上周」这种随手翻不同，左清单是「我以前排过什么」的入口；一年足够回溯，
/// 再多就该去报表看了（那里有真正的区间统计）。
const PLAN_LIST_LIMIT: usize = 52;

/// 「计划 vs 实际」的计数（面板顶部那条摘要）。
///
/// 口径都在这里，界面不自己数：一旦界面各数一遍，「后加」是按 `original_task_ids`
/// 还是按「今天才创建的」这种分歧就会长得不一样。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlanStats {
    /// 计划里现在有几项。
    pub(crate) total: u32,
    /// 其中已完成的。
    pub(crate) done: u32,
    /// 其中是**原计划**（首次定型时就在里面）。
    pub(crate) planned: u32,
    /// 其中是后来加的。
    pub(crate) added: u32,
}

/// 一次导入的结果。
///
/// 两个数分开报：只说「已导入 N 个」会让「文件里 20 条、其实 18 条重复」看起来
/// 像丢了数据。界面把 `skipped` 拼在同一句里。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ImportOutcome {
    /// 真正建成的条数。
    pub(crate) created: usize,
    /// 因标题重复被跳过的条数（去重关掉时恒为 0）。
    pub(crate) skipped: usize,
}

impl PlanStats {
    /// 完成率（0–100）。没有任务时返回 0（与 `BoardStats::completion_percent` 同口径）。
    #[must_use]
    pub(crate) fn completion_percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        ((u64::from(self.done) * 100 + u64::from(self.total) / 2) / u64::from(self.total)) as u32
    }
}

impl ProjectsState {
    /// 周计划视图锚定的那一周（存的是该周内的某一天；`None` = 本地今天所在周）。
    pub(crate) const fn plan_week(&self) -> Option<CalendarDate> {
        self.plan_week
    }

    /// 当前锚定周（`None` 时回落到今天 —— 「本周」是这一档的默认视角）。
    pub(crate) fn plan_anchor(&self) -> CalendarDate {
        self.plan_week.unwrap_or(self.today)
    }

    /// 当前锚定周的周键（`2026-W42`）。周计划的所有读写都以它为准。
    pub(crate) fn plan_week_key(&self) -> String {
        self.plan_anchor().week_key()
    }

    /// 换周计划看的哪一周（`None` = 回到本周）；换完重读三份数据。
    pub(crate) fn set_plan_week(&mut self, week: Option<CalendarDate>) {
        if self.plan_week == week {
            return;
        }
        self.plan_week = week;
        self.reload_plans();
    }

    /// 最近写过的周计划（左清单）。
    pub(crate) fn plans(&self) -> &[WeeklyPlan] {
        &self.plans
    }

    /// 当前锚定周的那份计划；没写过就是 `None`（界面据此显示「这一周还没排」）。
    pub(crate) fn current_plan(&self) -> Option<&WeeklyPlan> {
        let key = self.plan_week_key();
        self.plans.iter().find(|plan| plan.week_key == key)
    }

    /// 当前计划里那些任务的现状（顺序跟随计划里的 `task_ids`）。
    pub(crate) fn plan_tasks(&self) -> &[Task] {
        &self.plan_tasks
    }

    /// 候选池里**还没进计划**的那些（「＋ 加入计划」要列的就是它们）。
    ///
    /// 这是候选池唯一对外的读口：要「全部候选」和「还没加进去的」都从这里出去，
    /// 免得两个读口各有一套过滤口径（周键改判据时只改一处）。
    pub(crate) fn addable_tasks(&self) -> Vec<&Task> {
        match self.current_plan() {
            Some(plan) => self
                .week_tasks
                .iter()
                .filter(|task| !plan.task_ids.contains(&task.id))
                .collect(),
            None => self.week_tasks.iter().collect(),
        }
    }

    /// 「计划 vs 实际」的计数。
    pub(crate) fn plan_stats(&self) -> PlanStats {
        let total = self.plan_tasks.len() as u32;
        let done = self
            .plan_tasks
            .iter()
            .filter(|task| task.status == duck_project::TaskStatus::Done)
            .count() as u32;
        let (planned, added) = match self.current_plan() {
            Some(plan) => (
                plan.planned_task_ids().len() as u32,
                plan.added_task_ids().len() as u32,
            ),
            None => (0, 0),
        };
        PlanStats {
            total,
            done,
            planned,
            added,
        }
    }

    /// 这一周写过计划吗（没写过时界面把「任务集」当作空表，但**不**显示状态徽章）。
    pub(crate) fn has_plan(&self) -> bool {
        self.current_plan().is_some()
    }

    /// 写当前这一周的计划（正文 + 任务集 + 状态）。
    ///
    /// 全量写：正文与任务集由调用方一起给。`original_task_ids` 的定型在内核里
    /// （见 `duck_project::store::plans`），这里不重复实现一遍。
    pub(crate) fn save_plan(
        &mut self,
        content: &str,
        task_ids: &[i64],
        status: WeeklyPlanStatus,
    ) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.upsert_plan(&self.plan_week_key(), content, task_ids, status)?;
        self.reload_plans();
        Ok(())
    }

    /// 只改当前这一周计划的状态（「确认」/「改回草稿」）。
    pub(crate) fn set_plan_status(&mut self, status: WeeklyPlanStatus) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.set_plan_status(&self.plan_week_key(), status)?;
        self.reload_plans();
        Ok(())
    }

    /// 删掉当前这一周的计划。
    pub(crate) fn delete_plan(&mut self) -> duck_project::Result<()> {
        let Some(store) = &self.store else {
            return Err(store_not_open());
        };
        store.delete_plan(&self.plan_week_key())?;
        self.reload_plans();
        Ok(())
    }

    /// 把一条任务加进当前这一周的计划（已在里面就什么都不做）。
    ///
    /// 「什么都不做」而不是报错：界面上的「＋ 加入计划」与右键菜单可能同时到达
    /// （菜单点完清单还没重绘），把它当成幂等动作比弹一个「已经在计划里」更有用。
    pub(crate) fn add_task_to_plan(&mut self, task_id: i64) -> duck_project::Result<()> {
        self.edit_plan_tasks(|ids| {
            if !ids.contains(&task_id) {
                ids.push(task_id);
            }
        })
    }

    /// 把一条任务移出当前这一周的计划。
    pub(crate) fn remove_task_from_plan(&mut self, task_id: i64) -> duck_project::Result<()> {
        self.edit_plan_tasks(|ids| ids.retain(|id| *id != task_id))
    }

    /// 改计划里的任务集（其余字段原样写回）。
    ///
    /// 正文与状态**照现在的值原样回写**，不在这里改它们：这个方法的调用方是
    /// 「加一条 / 去一条」，顺手动正文会让「点一下加号」变成一次看不见的内容改写。
    fn edit_plan_tasks(&mut self, edit: impl FnOnce(&mut Vec<i64>)) -> duck_project::Result<()> {
        let (mut ids, content, status) = match self.current_plan() {
            Some(plan) => (plan.task_ids.clone(), plan.content_md.clone(), plan.status),
            // 这一周还没写过：从空表与草稿状态起一份。
            None => (Vec::new(), String::new(), WeeklyPlanStatus::Draft),
        };
        edit(&mut ids);
        self.save_plan(&content, &ids, status)
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

    /// 用内存库造一个脱离了页面的状态：正是「状态层不碰 gpui」的用处。
    ///
    /// 「今天」钉在 [`fixture_today`] 上（`clock` 一格）：左栏计数、逾期判定、日期
    /// 分档都带「今天」口径，不钉住的话换一天跑测试就红。
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
            clock: fixture_today,
            sort: TaskSort::Manual,
            group: GroupMode::Priority,
            smart: None,
            tag: None,
            tags: Vec::new(),
            show_done: true,
            projects_collapsed: false,
            sidebar: SidebarCounts::default(),
            search: String::new(),
            view: ViewMode::Board,
            calendar_anchor: None,
            calendar_mode: CalendarMode::Month,
            logs: Vec::new(),
            log_kind: LogKind::Daily,
            log_selected: None,
            plan_week: None,
            plans: Vec::new(),
            plan_tasks: Vec::new(),
            week_tasks: Vec::new(),
            report_range: ReportRange::Days7,
            report_status: ReportStatus::All,
            report_tags: Vec::new(),
            archive_search: String::new(),
            archive_tasks: Vec::new(),
            archive_projects: Vec::new(),
            archive_counts: ArchiveCounts::default(),
            archive_selected: None,
            undo_stack: Vec::new(),
            undo_seq: 0,
            checked: BTreeSet::new(),
        };
        state.reload();
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
        state.reload();
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
        state.reload();
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
    fn changing_the_status_leaves_an_undo_that_puts_it_back() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");

        state.set_task_status(id, TaskStatus::Doing).unwrap();
        assert_eq!(state.tasks_in_status(TaskStatus::Doing).len(), 1);
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RevertStatus {
                id,
                status: TaskStatus::Todo,
            }),
            "改状态要压一条「改回**原来那一档**」的槽 —— 存的是改之前的值，不是新值"
        );

        state.undo().unwrap();
        assert_eq!(
            state.tasks_in_status(TaskStatus::Todo).len(),
            1,
            "撤销后回到待办"
        );
        assert_eq!(state.tasks_in_status(TaskStatus::Doing).len(), 0);
        assert_eq!(state.undo_slot(), None, "撤销槽用完即清");
    }

    #[test]
    fn duplicating_leaves_an_undo_that_deletes_the_copy() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        let copy = state.duplicate_task(id).unwrap().unwrap();
        assert_eq!(state.task_total(), 2, "复制出来就是两条");

        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RemoveCreated(copy)),
            "复制要压一条「把刚建的那条删掉」的槽，且指向**副本**而不是原件"
        );

        state.undo().unwrap();
        assert_eq!(state.task_total(), 1, "撤销后只剩原件");
    }

    #[test]
    fn deleting_the_task_the_undo_points_at_drops_the_slot() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.set_task_status(id, TaskStatus::Doing).unwrap();
        assert!(state.undo_slot().is_some());

        state.delete_task(id).unwrap();
        assert_eq!(state.undo_slot(), None, "槽指向的任务没了 ⇒ 槽跟着作废");
    }

    /// `⌘Z` 一次退一步，最新的先退（不是「一次性全还原」）。
    #[test]
    fn undo_walks_back_one_step_at_a_time() {
        let mut state = memory_state();
        let first = add_task(&mut state, "one");
        let second = add_task(&mut state, "two");

        state.archive_task(first).unwrap();
        state.archive_task(second).unwrap();
        assert_eq!(state.task_total(), 0);
        assert_eq!(state.undo_depth(), 2, "两次归档 = 两步历史");

        assert_eq!(state.undo().unwrap(), Some(UndoAction::RestoreTask(second)));
        assert_eq!(state.task_total(), 1, "只回来一条");
        assert_eq!(state.undo_depth(), 1);

        assert_eq!(state.undo().unwrap(), Some(UndoAction::RestoreTask(first)));
        assert_eq!(state.task_total(), 2);
        assert_eq!(state.undo().unwrap(), None, "退到底了");
        assert_eq!(state.undo_depth(), 0);
    }

    /// 栈深封顶，超出时丢**最旧的**（撤销是「刚才那几下」的退路，不是无限历史）。
    #[test]
    fn the_undo_stack_keeps_only_the_newest_steps() {
        let mut state = memory_state();
        let pushes = UNDO_DEPTH as i64 + 8;
        for id in 1..=pushes {
            state.push_undo(UndoAction::RemoveCreated(id), 0);
        }

        assert_eq!(state.undo_depth(), UNDO_DEPTH, "封顶");
        assert_eq!(
            state.undo_slot().map(|slot| slot.action),
            Some(UndoAction::RemoveCreated(pushes)),
            "栈顶永远是最新压的那条"
        );
        assert_eq!(
            state.undo_stack.first().map(|slot| slot.action),
            Some(UndoAction::RemoveCreated(pushes - UNDO_DEPTH as i64 + 1)),
            "被挤掉的是最早那几条"
        );
    }

    /// 过了时效的那一步当它不存在 —— `⌘Z` 答「没有可撤销的操作」，不是「撤销失败」。
    #[test]
    fn undo_steps_expire_after_the_retention_window() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        assert!(state.undo_slot().is_some());

        // 不真的等一分钟：把「六十秒后」直接喂进剪枝口。
        state.prune_undo(now_seconds() + UNDO_RETENTION + 1);
        assert_eq!(state.undo_slot(), None);
        assert_eq!(state.undo().unwrap(), None);
        assert_eq!(state.task_total(), 0, "没真撤销：任务还在归档里");
    }

    /// 归档区整个清空 ⇒ 栈里那些「撤销归档」全都不可能成功。
    #[test]
    fn clearing_the_archive_empties_the_undo_stack() {
        let mut state = memory_state();
        let id = add_task(&mut state, "t");
        state.archive_task(id).unwrap();
        assert_eq!(state.undo_depth(), 1);

        state.clear_archive(ClearScope::All).unwrap();
        assert_eq!(state.undo_depth(), 0);
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
        state.reload();
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
        state.reload();
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
        state.reload();
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

    #[test]
    fn the_report_filters_are_wired_straight_into_the_build_input() {
        // 三格筛选没有第二份真源：`report_filters()` 现拼出来的必须与三个字段一致。
        let mut state = memory_state();
        state.set_report_range(ReportRange::Days90);
        state.set_report_status(ReportStatus::Open);
        state.toggle_report_tag(7);

        let filters = state.report_filters();
        assert_eq!(filters.range, ReportRange::Days90);
        assert_eq!(filters.status, ReportStatus::Open);
        assert_eq!(filters.tags, vec![7]);
    }

    #[test]
    fn report_tags_are_a_multi_select_that_toggles_off() {
        let mut state = memory_state();
        state.toggle_report_tag(7);
        state.toggle_report_tag(8);
        assert_eq!(state.report_tags(), &[7, 8], "多选：两个都留着");

        state.toggle_report_tag(7);
        assert_eq!(state.report_tags(), &[8], "再点一次就取消那一个");

        state.clear_report_tags();
        assert!(state.report_tags().is_empty());
    }

    #[test]
    fn the_report_range_defaults_to_seven_days_and_reads_back() {
        let state = memory_state();
        assert_eq!(state.report_range(), ReportRange::Days7);
        assert_eq!(state.report_status(), ReportStatus::All);
    }

    #[test]
    fn the_calendar_defaults_to_the_month_grid_without_an_anchor() {
        let state = memory_state();
        assert_eq!(state.calendar_anchor(), None, "None = 跟着今天走");
        assert_eq!(state.calendar_mode(), CalendarMode::Month);
    }

    #[test]
    fn switching_the_calendar_mode_keeps_the_anchor_in_place() {
        // 月 ↔ 周共享同一个锚：切画法该停在原地，而不是跳回今天（那是「今天」按钮的事）。
        let mut state = memory_state();
        let anchor = CalendarDate::new(2026, 11, 3).expect("合法日期");
        state.set_calendar_anchor(Some(anchor));
        state.set_calendar_mode(CalendarMode::Week);
        assert_eq!(state.calendar_mode(), CalendarMode::Week);
        assert_eq!(state.calendar_anchor(), Some(anchor));
        state.set_calendar_mode(CalendarMode::Month);
        assert_eq!(state.calendar_anchor(), Some(anchor));
    }

    // -----------------------------------------------------------------------
    // 周计划
    // -----------------------------------------------------------------------

    /// 周计划档的状态：视图要在日志档、档位要在周计划，否则 `reload_plans` 直接返回
    /// （那是刻意的 —— 没显示这一档时读三份数据纯属浪费）。
    fn plan_state() -> ProjectsState {
        let mut state = memory_state();
        state.set_view(ViewMode::Log);
        state.set_log_kind(LogKind::Plan);
        state
    }

    /// 建一条**本周内到期**的任务，并重读周计划的三份数据。
    ///
    /// 候选池的口径是内核的 `DueFilter::ThisWeek`（2026-10-12 ~ 10-18 到期且未完成），
    /// 所以 `day` 要落在 12–18 里。
    fn add_due_this_week(state: &mut ProjectsState, title: &str, day: u32) -> i64 {
        let due = CalendarDate::new(2026, 10, day).expect("2026-10 的合法日子");
        let id = state
            .store
            .as_ref()
            .unwrap()
            .create_task(NewTask::new(title).dates(None, Some(due)))
            .unwrap();
        state.reload_plans();
        id
    }

    /// 锚`None` = 跟着今天走；换成别的周只影响周键，不动任务筛选。
    #[test]
    fn the_plan_week_defaults_to_today_and_follows_the_anchor() {
        let mut state = plan_state();
        assert_eq!(state.plan_week(), None, "None = 跟着今天走");
        assert_eq!(state.plan_anchor(), fixture_today());
        assert_eq!(state.plan_week_key(), "2026-W42", "2026-10-14 落在第 42 周");

        // 往前一周：锚是整日，周键跟着走。
        state.set_plan_week(Some(fixture_today().add_days(-7)));
        assert_eq!(state.plan_week_key(), "2026-W41");
        // 同一周里换一天不该换周键（锚是「那一周」，不是「那一天」）。
        state.set_plan_week(Some(fixture_today().add_days(-5)));
        assert_eq!(state.plan_week_key(), "2026-W41");
        // 回本周。
        state.set_plan_week(None);
        assert_eq!(state.plan_week_key(), "2026-W42");
    }

    /// 候选池 = 本周到期且未完成；已完成的、下周的不进来。
    #[test]
    fn the_candidate_pool_is_this_weeks_open_tasks_only() {
        let mut state = plan_state();
        let monday = add_due_this_week(&mut state, "周一的事", 12);
        let sunday = add_due_this_week(&mut state, "周日的事", 18);
        // 下周与今天之后很远的都不算。
        add_due_this_week(&mut state, "下周一的事", 19);
        let done = add_due_this_week(&mut state, "已经做完的", 13);
        state
            .store
            .as_ref()
            .unwrap()
            .set_task_status(done, TaskStatus::Done)
            .unwrap();
        state.reload_plans();

        // 顺序与过滤直接读字段：`addable_tasks` 是给界面的读口（计划为空时两者相同，
        // 见下面那条断言），而这条要钉的是**池子本身**的形态。
        let ids: Vec<i64> = state.week_tasks.iter().map(|task| task.id).collect();
        assert_eq!(ids, vec![monday, sunday], "按截止日升序，且只有未完成的");

        // 还没进计划时，候选池 = 全部可加项。
        let addable: Vec<i64> = state.addable_tasks().iter().map(|task| task.id).collect();
        assert_eq!(addable, vec![monday, sunday]);
    }

    /// 空的一周没有计划；把任务加进去就自动起一份（草稿态、空正文）。
    #[test]
    fn an_empty_week_has_no_plan_until_a_task_is_added() {
        let mut state = plan_state();
        assert!(!state.has_plan());
        assert_eq!(state.plan_stats(), PlanStats::default());
        assert!(state.plans().is_empty());

        let task = add_due_this_week(&mut state, "写周报", 14);
        state.add_task_to_plan(task).expect("写入成功");
        assert!(state.has_plan());
        let plan = state.current_plan().expect("刚起的计划");
        assert_eq!(plan.task_ids, vec![task]);
        assert_eq!(plan.status, WeeklyPlanStatus::Draft, "自动起的是草稿");
        assert!(plan.content_md.is_empty());

        // 加第二次是幂等的（界面上的按钮与右键菜单可能同时到达）。
        state.add_task_to_plan(task).expect("再写一次也不报错");
        assert_eq!(
            state.current_plan().expect("在").task_ids,
            vec![task],
            "不该出现两条一样的 id"
        );

        // 加进来之后候选池里就没有它了。
        assert!(state.addable_tasks().is_empty());
        assert_eq!(state.plan_tasks().len(), 1, "计划内任务的现状读出来了");

        // 移出之后计划还在（只剩空任务集），候选池又回来。
        state.remove_task_from_plan(task).expect("写入成功");
        assert!(state.current_plan().expect("在").task_ids.is_empty());
        assert_eq!(state.addable_tasks().len(), 1);
    }

    /// 计划 vs 实际：定型时的算「原计划」，之后的算「后加」；完成数按当前状态数。
    #[test]
    fn added_tasks_are_split_from_the_original_ones() {
        let mut state = plan_state();
        let a = add_due_this_week(&mut state, "A", 12);
        let b = add_due_this_week(&mut state, "B", 13);
        // 第一次以非空任务集保存 → 定型。
        state
            .save_plan("这周先把 A、B 收掉", &[a, b], WeeklyPlanStatus::Draft)
            .expect("写入成功");
        // 之后再加一条。
        let c = add_due_this_week(&mut state, "C", 14);
        state.add_task_to_plan(c).expect("写入成功");

        let stats = state.plan_stats();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.planned, 2, "A、B 是定型时的原计划");
        assert_eq!(stats.added, 1, "C 是后来加的");
        assert_eq!(stats.done, 0);
        assert_eq!(stats.completion_percent(), 0);

        // 完成 A：完成数与完成率跟着走，但「原计划 / 后加」的分档不动。
        state
            .store
            .as_ref()
            .unwrap()
            .set_task_status(a, TaskStatus::Done)
            .unwrap();
        state.reload_plans();
        let stats = state.plan_stats();
        assert_eq!(stats.done, 1);
        assert_eq!(stats.planned, 2);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.completion_percent(), 33);
    }

    /// 保存正文**不动任务集与状态**（`submit_plan` 靠这条把两者一起带回）。
    #[test]
    fn saving_the_body_keeps_the_task_set() {
        let mut state = plan_state();
        let task = add_due_this_week(&mut state, "只改正文", 14);
        state.add_task_to_plan(task).expect("写入成功");
        state
            .set_plan_status(WeeklyPlanStatus::Confirmed)
            .expect("确认成功");

        state
            .save_plan("改了正文", &[task], WeeklyPlanStatus::Confirmed)
            .expect("写入成功");
        let plan = state.current_plan().expect("在");
        assert_eq!(plan.content_md, "改了正文");
        assert_eq!(plan.task_ids, vec![task]);
        assert!(plan.is_confirmed());
    }

    /// 删计划只删计划，任务本身留着。
    #[test]
    fn deleting_the_plan_leaves_the_tasks_alone() {
        let mut state = plan_state();
        let task = add_due_this_week(&mut state, "留下我", 14);
        state.add_task_to_plan(task).expect("写入成功");
        state.delete_plan().expect("删除成功");
        assert!(!state.has_plan());
        assert!(state.plans().is_empty());
        assert!(state.plan_tasks().is_empty());
        assert_eq!(state.addable_tasks().len(), 1, "任务还在候选池里");
        assert!(
            state
                .store
                .as_ref()
                .unwrap()
                .get_task(task)
                .unwrap()
                .is_some()
        );
    }

    /// 完成率四舍五入到整数，空计划不除零。
    #[test]
    fn plan_completion_percent_rounds_and_guards_against_zero() {
        let stats = |total, done| PlanStats {
            total,
            done,
            planned: total,
            added: 0,
        };
        assert_eq!(PlanStats::default().completion_percent(), 0, "空计划不除零");
        assert_eq!(stats(3, 1).completion_percent(), 33);
        assert_eq!(stats(3, 2).completion_percent(), 67, "四舍五入");
        assert_eq!(stats(1, 1).completion_percent(), 100);
        assert_eq!(stats(7, 6).completion_percent(), 86);
    }
}
