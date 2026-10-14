//! 领域模型：枚举、记录与写入口的输入类型。
//!
//! 约定：
//!
//! - **记录**（[`Group`] / [`Project`] / [`Task`] / [`Tag`] / [`Milestone`] / [`BoardColumn`]）
//!   是「从库里读出来的一行」，字段 `pub`，与 `duck-ai::store` 的 `Conversation` 同风格。
//! - **输入**（[`NewTask`] / [`TaskPatch`] / [`TaskQuery`] 等）带不变量与可选语义，
//!   走构造器 + 链式设置，字段私有（`AGENTS.md` §4）。
//! - 所有枚举与 SQLite 之间的字面量都是**稳定英文键**（`doing` / `P0` → `0` / `Q2`）：
//!   界面文案按当次语言另译，换语言不会让历史数据失配。

use crate::time::CalendarDate;

/// 新建项目的默认颜色（与 `duck-theme` 的主色同色系）。
pub const DEFAULT_PROJECT_COLOR: &str = "#3B82F6";

/// 项目状态。
///
/// ⚠️ **没有 `Archived`**：归档是与状态正交的维度（`archived_at` 非空即已归档），
/// 把 `archived` 混进状态会让「归档一个已完成的项目」这种正常需求无处表达。
/// 详见 `docs/projects-design.md` §2.3。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ProjectStatus {
    /// 进行中。
    #[default]
    Active,
    /// 已搁置（仍在常规视图里，只是明确暂停）。
    Paused,
}

impl ProjectStatus {
    /// 全部取值。
    pub const ALL: [Self; 2] = [Self::Active, Self::Paused];

    /// SQLite 里的稳定键。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }

    /// 解析稳定键；未知值返回 `None`。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

/// 任务状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    /// 待办。
    #[default]
    Todo,
    /// 进行中。
    Doing,
    /// 受阻。
    Blocked,
    /// 已完成。
    Done,
    /// 已搁置。
    Deferred,
    /// 已取消（软删，可恢复为待办）。
    Cancelled,
}

impl TaskStatus {
    /// 全部取值，按看板的自然顺序。
    pub const ALL: [Self; 6] = [
        Self::Todo,
        Self::Doing,
        Self::Blocked,
        Self::Done,
        Self::Deferred,
        Self::Cancelled,
    ];

    /// 已归档之外、默认看板上的四列（`deferred` / `cancelled` 不占列）。
    pub const DEFAULT_BOARD: [Self; 4] = [Self::Todo, Self::Doing, Self::Blocked, Self::Done];

    /// SQLite 里的稳定键。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Deferred => "deferred",
            Self::Cancelled => "cancelled",
        }
    }

    /// 解析稳定键；未知值返回 `None`。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "todo" => Some(Self::Todo),
            "doing" => Some(Self::Doing),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            "deferred" => Some(Self::Deferred),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// 是否「不再需要跟进」（已完成或已取消）。逾期口径按它排除。
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// 状态迁移是否合法。
    ///
    /// 规则只有一条**硬**约束：`cancelled` 是终态，要复活必须先回到 `todo`
    /// （避免「取消 → 直接完成」把 `completed_at` 写成一个语义混乱的值）。
    /// 其余方向一律放行——看板拖拽本来就不该有禁区。
    ///
    /// > 设计文档 §2.2 里那条「`done` 只能由 `doing` / `blocked` 进入」在实现时**放宽**了：
    /// > 已批准的原型允许把卡片从「待办」直接拖到「已完成」。`completed_at` 的语义由
    /// > [`crate::store::ProjectStore::set_task_status`] 保证——进入 `done` 才写、离开就清，
    /// > 与「从哪来」无关。
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Cancelled => matches!(next, Self::Cancelled | Self::Todo),
            _ => true,
        }
    }
}

/// 优先级。数值越小越紧急（`P0` = 0），与 SQLite 里的 `INTEGER` 一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// 最高。
    P0,
    /// 高。
    P1,
    /// 中（默认）。
    #[default]
    P2,
    /// 低。
    P3,
}

impl Priority {
    /// 全部取值，由高到低。
    pub const ALL: [Self; 4] = [Self::P0, Self::P1, Self::P2, Self::P3];

    /// 落库数值。
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
            Self::P3 => 3,
        }
    }

    /// 从落库数值还原；越界钳到最近的合法档（不报错——旧库里的野值不该让整页打不开）。
    #[must_use]
    pub const fn from_i64(value: i64) -> Self {
        match value {
            i64::MIN..=0 => Self::P0,
            1 => Self::P1,
            2 => Self::P2,
            _ => Self::P3,
        }
    }

    /// 快捷新建里 `!P0` 的解析（大小写不敏感）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_uppercase().as_str() {
            "P0" => Some(Self::P0),
            "P1" => Some(Self::P1),
            "P2" => Some(Self::P2),
            "P3" => Some(Self::P3),
            _ => None,
        }
    }
}

/// 四象限（艾森豪威尔矩阵）。只是任务属性，**不是**看板布局。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Quadrant {
    /// 重要且紧急。
    Q1,
    /// 重要不紧急（默认）。
    #[default]
    Q2,
    /// 紧急不重要。
    Q3,
    /// 都不。
    Q4,
}

impl Quadrant {
    /// 全部取值。
    pub const ALL: [Self; 4] = [Self::Q1, Self::Q2, Self::Q3, Self::Q4];

    /// SQLite 里的稳定键。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Q1 => "Q1",
            Self::Q2 => "Q2",
            Self::Q3 => "Q3",
            Self::Q4 => "Q4",
        }
    }

    /// 解析稳定键（大小写不敏感）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_uppercase().as_str() {
            "Q1" => Some(Self::Q1),
            "Q2" => Some(Self::Q2),
            "Q3" => Some(Self::Q3),
            "Q4" => Some(Self::Q4),
            _ => None,
        }
    }

    /// 是否「重要」（Q1 / Q2）。
    #[must_use]
    pub const fn is_important(self) -> bool {
        matches!(self, Self::Q1 | Self::Q2)
    }

    /// 是否「紧急」（Q1 / Q3）。
    #[must_use]
    pub const fn is_urgent(self) -> bool {
        matches!(self, Self::Q1 | Self::Q3)
    }
}

/// 工作日志类型（表在 v1 就建好，读写属 M2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogKind {
    /// 日报。
    Daily,
    /// 周报。
    Weekly,
    /// 周计划。
    Plan,
}

impl LogKind {
    /// 全部取值。
    pub const ALL: [Self; 3] = [Self::Daily, Self::Weekly, Self::Plan];

    /// SQLite 里的稳定键。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Plan => "plan",
        }
    }

    /// 解析稳定键。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

/// 归档的**级联归属**：这条记录是被哪个根对象带着归档的。
///
/// 根对象自己也记自己（归档任务 12 → `Task(12)`；归档项目 7 → 项目行记 `archived_at`，
/// 它名下的任务记 `Project(7)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveRoot {
    /// 随任务归档（该 id 的任务就是根）。
    Task(i64),
    /// 随项目归档（该 id 的项目就是根）。
    Project(i64),
}

impl ArchiveRoot {
    /// 落库形式：`kind:id`。
    #[must_use]
    pub fn to_wire(self) -> String {
        match self {
            Self::Task(id) => format!("task:{id}"),
            Self::Project(id) => format!("project:{id}"),
        }
    }

    /// 解析落库形式；非法值返回 `None`（外部工具改过的库不该让整页打不开）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, id) = text.split_once(':')?;
        let id: i64 = id.parse().ok()?;
        match kind {
            "task" => Some(Self::Task(id)),
            "project" => Some(Self::Project(id)),
            _ => None,
        }
    }

    /// 根对象的 id。
    #[must_use]
    pub const fn id(self) -> i64 {
        match self {
            Self::Task(id) | Self::Project(id) => id,
        }
    }

    /// 根对象是任务还是项目。
    #[must_use]
    pub const fn is_task(self) -> bool {
        matches!(self, Self::Task(_))
    }
}

/// 子任务完成进度（**派生字段**，由查询顺手填好，写入时被忽略）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubtaskProgress {
    /// 子任务总数。
    pub total: u32,
    /// 其中已完成的数量。
    pub done: u32,
}

/// 左栏一级节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// 主键。
    pub id: i64,
    /// 名称。
    pub name: String,
    /// 图标名（`duck-components` 的图标标识，可选）。
    pub icon: Option<String>,
    /// 排序值，越小越靠前。
    pub sort_order: i64,
    /// 左栏是否折叠。
    pub collapsed: bool,
    /// 创建时刻（Unix 秒）。
    pub created_at: i64,
}

/// 项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// 主键。
    pub id: i64,
    /// 所属分组；`None` = 未分组。
    pub group_id: Option<i64>,
    /// 名称。
    pub name: String,
    /// 描述（纯文本，不渲染 Markdown）。
    pub description: String,
    /// 主题色（`#RRGGBB`），用于左栏色点与卡片左侧色条。
    pub color: String,
    /// 图标名。
    pub icon: Option<String>,
    /// 状态。
    pub status: ProjectStatus,
    /// 开始日期。
    pub start_date: Option<CalendarDate>,
    /// 截止日期。
    pub due_date: Option<CalendarDate>,
    /// 排序值。
    pub sort_order: i64,
    /// 创建时刻（Unix 秒）。
    pub created_at: i64,
    /// 最近更新时刻（Unix 秒）。
    pub updated_at: i64,
    /// 归档时刻；`None` = 未归档。
    pub archived_at: Option<i64>,
}

impl Project {
    /// 是否已归档。
    #[must_use]
    pub const fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// 任务。
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// 主键。
    pub id: i64,
    /// 所属项目；`None` = 收集箱（未归属任何项目）。
    pub project_id: Option<i64>,
    /// 父任务；`None` = 顶层任务。只支持一层嵌套。
    pub parent_id: Option<i64>,
    /// 标题（必填）。
    pub title: String,
    /// Markdown 描述。
    pub description_md: String,
    /// 状态。
    pub status: TaskStatus,
    /// 优先级。
    pub priority: Priority,
    /// 四象限。
    pub quadrant: Quadrant,
    /// 开始日期。
    pub start_date: Option<CalendarDate>,
    /// 截止日期。
    pub due_date: Option<CalendarDate>,
    /// 截止时刻 `HH:MM`；仅在 `due_date` 非空时有意义。
    pub due_time: Option<String>,
    /// 完成时刻（Unix 秒）；状态离开 `done` 时被清空。
    pub completed_at: Option<i64>,
    /// 进度（0–100）。与里程碑联动但**不相等**。
    pub progress: u8,
    /// 预估工时（小时）。
    pub estimate_hours: Option<f64>,
    /// 实际工时（小时）。
    pub actual_hours: Option<f64>,
    /// 排序值。
    pub sort_order: i64,
    /// 创建时刻（Unix 秒）。
    pub created_at: i64,
    /// 最近更新时刻（Unix 秒）。
    pub updated_at: i64,
    /// 归档时刻；`None` = 未归档。
    pub archived_at: Option<i64>,
    /// 归档的级联归属（见 [`ArchiveRoot`]）；未归档时为 `None`。
    pub archived_root: Option<ArchiveRoot>,
    /// 标签（派生字段，由查询填充）。
    pub tags: Vec<Tag>,
    /// 子任务进度（派生字段，由查询填充）。
    pub subtasks: SubtaskProgress,
}

impl Task {
    /// 是否已归档。
    #[must_use]
    pub const fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// 是否逾期（`today` 为调用方给的本地今天）。
    ///
    /// 已完成 / 已取消不算逾期；没有截止日不算逾期。
    #[must_use]
    pub fn is_overdue(&self, today: CalendarDate) -> bool {
        if self.status.is_settled() {
            return false;
        }
        self.due_date.is_some_and(|due| due < today)
    }
}

/// 标签。全局共享，跨项目复用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// 主键。
    pub id: i64,
    /// 名称（唯一）。
    pub name: String,
    /// 颜色（`#RRGGBB`）。
    pub color: String,
    /// 使用次数（**派生字段**：由 [`crate::store::ProjectStore::list_tags`] 现算，不缓存）。
    pub usage_count: u32,
}

/// 里程碑。`task_id` 为 `None` 时是挂在项目上的项目级里程碑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// 主键。
    pub id: i64,
    /// 所属任务；`None` = 项目级。
    pub task_id: Option<i64>,
    /// 所属项目。
    pub project_id: Option<i64>,
    /// 标题。
    pub title: String,
    /// 描述。
    pub description: String,
    /// 目标日期。
    pub due_date: Option<CalendarDate>,
    /// 创建时的任务进度快照（0–100）；项目级为 `None`。
    pub progress_snapshot: Option<u8>,
    /// 达成时刻（Unix 秒）；`None` = 未达成。
    pub reached_at: Option<i64>,
    /// 创建时刻（Unix 秒）。
    pub created_at: i64,
}

impl Milestone {
    /// 是否已达成。
    #[must_use]
    pub const fn is_reached(&self) -> bool {
        self.reached_at.is_some()
    }
}

/// 看板列。
///
/// `name` 为 `None` 表示「用内置默认文案」——内核不含界面文案（i18n 守卫会拦下
/// `crates/` 里的中文字面量），所以列的默认名称由 `duck-app` 侧按语言取词，
/// 只有用户改过名的那一列才把名字落库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardColumn {
    /// 主键。
    pub id: i64,
    /// 显示名；`None` = 用内置默认文案。
    pub name: Option<String>,
    /// 映射到的内置状态。
    pub status_key: TaskStatus,
    /// 排序值。
    pub sort_order: i64,
    /// WIP 上限（列内任务数超过它时列头给提示）；`None` = 不限。
    pub wip_limit: Option<i64>,
}

/// 工作日志（表结构在 v1，读写属 M2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkLog {
    /// 主键。
    pub id: i64,
    /// 归属日期。
    pub date: CalendarDate,
    /// 类型。
    pub kind: LogKind,
    /// Markdown 正文。
    pub content_md: String,
    /// 是否由 AI 生成。
    pub ai_generated: bool,
    /// 关联的任务 id（**归档不打断引用**：已归档任务仍留在日志里）。
    pub linked_task_ids: Vec<i64>,
}

/// 新建分组。
///
/// **不派生 `Default`**：名称是必填的，一个「空名分组」在左栏就是一行点不中的空白。
#[derive(Debug, Clone)]
pub struct NewGroup {
    /// 名称（必填，调用 [`Self::name`] 设置）。
    pub(crate) name: String,
    /// 图标名。
    pub(crate) icon: Option<String>,
    /// 排序值；`None` = 追加到末尾。
    pub(crate) sort_order: Option<i64>,
}

impl NewGroup {
    /// 名称（必填）。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            icon: None,
            sort_order: None,
        }
    }

    /// 图标名。
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// 排序值。
    #[must_use]
    pub const fn sort_order(mut self, sort_order: i64) -> Self {
        self.sort_order = Some(sort_order);
        self
    }
}

/// 新建项目。
#[derive(Debug, Clone)]
pub struct NewProject {
    pub(crate) name: String,
    pub(crate) group_id: Option<i64>,
    pub(crate) description: String,
    pub(crate) color: String,
    pub(crate) icon: Option<String>,
    pub(crate) start_date: Option<CalendarDate>,
    pub(crate) due_date: Option<CalendarDate>,
    pub(crate) sort_order: Option<i64>,
}

impl NewProject {
    /// 名称（必填）。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            group_id: None,
            description: String::new(),
            color: DEFAULT_PROJECT_COLOR.to_owned(),
            icon: None,
            start_date: None,
            due_date: None,
            sort_order: None,
        }
    }

    /// 所属分组。
    #[must_use]
    pub const fn group(mut self, group_id: i64) -> Self {
        self.group_id = Some(group_id);
        self
    }

    /// 描述。
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// 主题色。
    #[must_use]
    pub fn color(mut self, color: impl Into<String>) -> Self {
        self.color = color.into();
        self
    }

    /// 图标名。
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// 起止日期。
    #[must_use]
    pub const fn dates(mut self, start: Option<CalendarDate>, due: Option<CalendarDate>) -> Self {
        self.start_date = start;
        self.due_date = due;
        self
    }

    /// 排序值。
    #[must_use]
    pub const fn sort_order(mut self, sort_order: i64) -> Self {
        self.sort_order = Some(sort_order);
        self
    }
}

/// 项目局部更新。`None` = 不改这一项；要「清空」用对应的 `clear_*` 方法。
#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub(crate) name: Option<String>,
    pub(crate) group_id: Option<Option<i64>>,
    pub(crate) description: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) icon: Option<Option<String>>,
    pub(crate) status: Option<ProjectStatus>,
    pub(crate) start_date: Option<Option<CalendarDate>>,
    pub(crate) due_date: Option<Option<CalendarDate>>,
    pub(crate) sort_order: Option<i64>,
}

impl ProjectPatch {
    /// 空 patch（什么都不改）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 改名。
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 改归属分组。
    #[must_use]
    pub const fn group(mut self, group_id: i64) -> Self {
        self.group_id = Some(Some(group_id));
        self
    }

    /// 移出分组。
    #[must_use]
    pub const fn clear_group(mut self) -> Self {
        self.group_id = Some(None);
        self
    }

    /// 改描述。
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// 改颜色。
    #[must_use]
    pub fn color(mut self, color: impl Into<String>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// 改图标。
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(Some(icon.into()));
        self
    }

    /// 清图标。
    #[must_use]
    pub fn clear_icon(mut self) -> Self {
        self.icon = Some(None);
        self
    }

    /// 改状态。
    #[must_use]
    pub const fn status(mut self, status: ProjectStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// 改起止日期。
    #[must_use]
    pub const fn dates(mut self, start: Option<CalendarDate>, due: Option<CalendarDate>) -> Self {
        self.start_date = Some(start);
        self.due_date = Some(due);
        self
    }

    /// 改排序值。
    #[must_use]
    pub const fn sort_order(mut self, sort_order: i64) -> Self {
        self.sort_order = Some(sort_order);
        self
    }
}

/// 新建任务。
#[derive(Debug, Clone)]
pub struct NewTask {
    pub(crate) title: String,
    pub(crate) project_id: Option<i64>,
    pub(crate) parent_id: Option<i64>,
    pub(crate) description_md: String,
    pub(crate) status: TaskStatus,
    pub(crate) priority: Priority,
    pub(crate) quadrant: Quadrant,
    pub(crate) start_date: Option<CalendarDate>,
    pub(crate) due_date: Option<CalendarDate>,
    pub(crate) due_time: Option<String>,
    pub(crate) progress: u8,
    pub(crate) estimate_hours: Option<f64>,
    pub(crate) actual_hours: Option<f64>,
    pub(crate) tag_ids: Vec<i64>,
    pub(crate) sort_order: Option<i64>,
}

impl NewTask {
    /// 标题（必填）。
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            project_id: None,
            parent_id: None,
            description_md: String::new(),
            status: TaskStatus::Todo,
            priority: Priority::P2,
            quadrant: Quadrant::Q2,
            start_date: None,
            due_date: None,
            due_time: None,
            progress: 0,
            estimate_hours: None,
            actual_hours: None,
            tag_ids: Vec::new(),
            sort_order: None,
        }
    }

    /// 所属项目。
    #[must_use]
    pub const fn project(mut self, project_id: i64) -> Self {
        self.project_id = Some(project_id);
        self
    }

    /// 父任务（一层嵌套）。
    #[must_use]
    pub const fn parent(mut self, parent_id: i64) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Markdown 描述。
    #[must_use]
    pub fn description(mut self, description_md: impl Into<String>) -> Self {
        self.description_md = description_md.into();
        self
    }

    /// 状态。
    #[must_use]
    pub const fn status(mut self, status: TaskStatus) -> Self {
        self.status = status;
        self
    }

    /// 优先级。
    #[must_use]
    pub const fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// 四象限。
    #[must_use]
    pub const fn quadrant(mut self, quadrant: Quadrant) -> Self {
        self.quadrant = quadrant;
        self
    }

    /// 起止日期。
    #[must_use]
    pub const fn dates(mut self, start: Option<CalendarDate>, due: Option<CalendarDate>) -> Self {
        self.start_date = start;
        self.due_date = due;
        self
    }

    /// 截止时刻 `HH:MM`。
    #[must_use]
    pub fn due_time(mut self, due_time: impl Into<String>) -> Self {
        self.due_time = Some(due_time.into());
        self
    }

    /// 进度（越界会被钳到 0–100）。
    #[must_use]
    pub fn progress(mut self, progress: u8) -> Self {
        self.progress = progress.min(100);
        self
    }

    /// 预估 / 实际工时。
    #[must_use]
    pub const fn hours(mut self, estimate: Option<f64>, actual: Option<f64>) -> Self {
        self.estimate_hours = estimate;
        self.actual_hours = actual;
        self
    }

    /// 标签 id 列表（不存在的 id 会被忽略）。
    #[must_use]
    pub fn tags(mut self, tag_ids: impl IntoIterator<Item = i64>) -> Self {
        self.tag_ids = tag_ids.into_iter().collect();
        self
    }

    /// 排序值；`None` = 追加到同项目同列的末尾。
    #[must_use]
    pub const fn sort_order(mut self, sort_order: i64) -> Self {
        self.sort_order = Some(sort_order);
        self
    }
}

/// 任务局部更新。`None` = 不改这一项；要「清空」用对应的 `clear_*` 方法。
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub(crate) title: Option<String>,
    pub(crate) project_id: Option<Option<i64>>,
    pub(crate) parent_id: Option<Option<i64>>,
    pub(crate) description_md: Option<String>,
    pub(crate) status: Option<TaskStatus>,
    pub(crate) priority: Option<Priority>,
    pub(crate) quadrant: Option<Quadrant>,
    pub(crate) start_date: Option<Option<CalendarDate>>,
    pub(crate) due_date: Option<Option<CalendarDate>>,
    pub(crate) due_time: Option<Option<String>>,
    pub(crate) progress: Option<u8>,
    pub(crate) estimate_hours: Option<Option<f64>>,
    pub(crate) actual_hours: Option<Option<f64>>,
    pub(crate) sort_order: Option<i64>,
    pub(crate) tag_ids: Option<Vec<i64>>,
}

impl TaskPatch {
    /// 空 patch。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 改标题。
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// 改所属项目。
    #[must_use]
    pub const fn project(mut self, project_id: i64) -> Self {
        self.project_id = Some(Some(project_id));
        self
    }

    /// 移出项目（进收集箱）。
    #[must_use]
    pub const fn clear_project(mut self) -> Self {
        self.project_id = Some(None);
        self
    }

    /// 改父任务。
    #[must_use]
    pub const fn parent(mut self, parent_id: i64) -> Self {
        self.parent_id = Some(Some(parent_id));
        self
    }

    /// 提升为顶层任务。
    #[must_use]
    pub const fn clear_parent(mut self) -> Self {
        self.parent_id = Some(None);
        self
    }

    /// 改描述。
    #[must_use]
    pub fn description(mut self, description_md: impl Into<String>) -> Self {
        self.description_md = Some(description_md.into());
        self
    }

    /// 改状态。
    #[must_use]
    pub const fn status(mut self, status: TaskStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// 改优先级。
    #[must_use]
    pub const fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// 改四象限。
    #[must_use]
    pub const fn quadrant(mut self, quadrant: Quadrant) -> Self {
        self.quadrant = Some(quadrant);
        self
    }

    /// 改起止日期（`None` = 清空）。
    #[must_use]
    pub const fn dates(mut self, start: Option<CalendarDate>, due: Option<CalendarDate>) -> Self {
        self.start_date = Some(start);
        self.due_date = Some(due);
        self
    }

    /// 改截止时刻（`None` = 清空）。
    #[must_use]
    pub fn due_time(mut self, due_time: Option<String>) -> Self {
        self.due_time = Some(due_time);
        self
    }

    /// 改进度（越界钳到 0–100）。
    #[must_use]
    pub fn progress(mut self, progress: u8) -> Self {
        self.progress = Some(progress.min(100));
        self
    }

    /// 改工时。
    #[must_use]
    pub const fn hours(mut self, estimate: Option<f64>, actual: Option<f64>) -> Self {
        self.estimate_hours = Some(estimate);
        self.actual_hours = Some(actual);
        self
    }

    /// 整体替换标签集合。
    #[must_use]
    pub fn tags(mut self, tag_ids: impl IntoIterator<Item = i64>) -> Self {
        self.tag_ids = Some(tag_ids.into_iter().collect());
        self
    }

    /// 改排序值。
    #[must_use]
    pub const fn sort_order(mut self, sort_order: i64) -> Self {
        self.sort_order = Some(sort_order);
        self
    }
}

/// 新建标签。
#[derive(Debug, Clone)]
pub struct NewTag {
    /// 名称。
    pub(crate) name: String,
    /// 颜色。
    pub(crate) color: String,
}

impl NewTag {
    /// 名称（唯一）。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: DEFAULT_PROJECT_COLOR.to_owned(),
        }
    }

    /// 颜色。
    #[must_use]
    pub fn color(mut self, color: impl Into<String>) -> Self {
        self.color = color.into();
        self
    }
}

/// 新建里程碑。
#[derive(Debug, Clone)]
pub struct NewMilestone {
    pub(crate) task_id: Option<i64>,
    pub(crate) project_id: Option<i64>,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) due_date: Option<CalendarDate>,
    pub(crate) progress_snapshot: Option<u8>,
}

impl NewMilestone {
    /// 任务级里程碑。
    #[must_use]
    pub fn for_task(task_id: i64, title: impl Into<String>) -> Self {
        Self {
            task_id: Some(task_id),
            project_id: None,
            title: title.into(),
            description: String::new(),
            due_date: None,
            progress_snapshot: None,
        }
    }

    /// 项目级里程碑（不挂任务）。
    #[must_use]
    pub fn for_project(project_id: i64, title: impl Into<String>) -> Self {
        Self {
            task_id: None,
            project_id: Some(project_id),
            title: title.into(),
            description: String::new(),
            due_date: None,
            progress_snapshot: None,
        }
    }

    /// 描述。
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// 目标日期。
    #[must_use]
    pub const fn due_date(mut self, due_date: CalendarDate) -> Self {
        self.due_date = Some(due_date);
        self
    }

    /// 进度快照（0–100，钳位）。创建时若高于任务当前进度，任务进度会被同步推进。
    #[must_use]
    pub fn progress_snapshot(mut self, progress: u8) -> Self {
        self.progress_snapshot = Some(progress.min(100));
        self
    }
}

/// 看板列的整表替换项，见 [`crate::store::ProjectStore::replace_columns`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    /// 显示名；`None` = 用内置默认文案。
    pub name: Option<String>,
    /// 映射到的内置状态。
    pub status_key: TaskStatus,
    /// WIP 上限。
    pub wip_limit: Option<i64>,
}

impl ColumnSpec {
    /// 用内置名称的列。
    #[must_use]
    pub const fn new(status_key: TaskStatus) -> Self {
        Self {
            name: None,
            status_key,
            wip_limit: None,
        }
    }

    /// 改显示名。
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 设 WIP 上限。
    #[must_use]
    pub const fn wip_limit(mut self, limit: i64) -> Self {
        self.wip_limit = Some(limit);
        self
    }
}

/// 一个项目的任务计数（左栏徽标与统计条用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskCounts {
    /// 未归档任务总数。
    pub total: u32,
    /// 其中已完成。
    pub done: u32,
}

impl TaskCounts {
    /// 未完成数。
    #[must_use]
    pub const fn open(&self) -> u32 {
        self.total.saturating_sub(self.done)
    }
}

#[cfg(test)]
mod tests {
    use super::{Priority, ProjectStatus, Quadrant, TaskStatus};

    #[test]
    fn status_keys_round_trip() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(TaskStatus::parse("Done"), None, "稳定键大小写敏感");
        assert_eq!(TaskStatus::parse(""), None);
        for status in ProjectStatus::ALL {
            assert_eq!(ProjectStatus::parse(status.as_str()), Some(status));
        }
        for quadrant in Quadrant::ALL {
            assert_eq!(Quadrant::parse(quadrant.as_str()), Some(quadrant));
        }
        for priority in Priority::ALL {
            assert_eq!(Priority::from_i64(priority.as_i64()), priority);
        }
    }

    #[test]
    fn priority_clamps_out_of_range_values() {
        assert_eq!(Priority::from_i64(-5), Priority::P0);
        assert_eq!(Priority::from_i64(0), Priority::P0);
        assert_eq!(Priority::from_i64(3), Priority::P3);
        assert_eq!(Priority::from_i64(99), Priority::P3);
        // 排序：P0 最靠前（数值最小）。
        assert!(Priority::P0 < Priority::P3);
    }

    #[test]
    fn cancelled_is_terminal_until_reopened() {
        assert!(TaskStatus::Cancelled.can_transition_to(TaskStatus::Todo));
        assert!(TaskStatus::Cancelled.can_transition_to(TaskStatus::Cancelled));
        for next in [
            TaskStatus::Doing,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Deferred,
        ] {
            assert!(
                !TaskStatus::Cancelled.can_transition_to(next),
                "取消是终态，必须先回待办：{}",
                next.as_str()
            );
        }
        // 其余一律放行——看板拖拽不该有禁区（含待办直接拖到已完成）。
        for from in [
            TaskStatus::Todo,
            TaskStatus::Doing,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Deferred,
        ] {
            for to in TaskStatus::ALL {
                assert!(
                    from.can_transition_to(to),
                    "{} → {}",
                    from.as_str(),
                    to.as_str()
                );
            }
        }
    }

    #[test]
    fn settled_statuses_exclude_done_and_cancelled() {
        assert!(TaskStatus::Done.is_settled());
        assert!(TaskStatus::Cancelled.is_settled());
        assert!(!TaskStatus::Doing.is_settled());
        assert!(!TaskStatus::Blocked.is_settled());
    }

    #[test]
    fn default_board_columns_are_the_four_internal_statuses() {
        let keys: Vec<&str> = TaskStatus::DEFAULT_BOARD
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(keys, ["todo", "doing", "blocked", "done"]);
    }

    #[test]
    fn quadrants_map_to_importance_and_urgency() {
        assert!(Quadrant::Q1.is_important() && Quadrant::Q1.is_urgent());
        assert!(Quadrant::Q2.is_important() && !Quadrant::Q2.is_urgent());
        assert!(!Quadrant::Q3.is_important() && Quadrant::Q3.is_urgent());
        assert!(!Quadrant::Q4.is_important() && !Quadrant::Q4.is_urgent());
    }
}
