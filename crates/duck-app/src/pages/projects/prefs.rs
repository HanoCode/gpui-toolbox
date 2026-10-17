//! 项目管理页的 UI 偏好（设计文档 §6.2 的 `kv: projects_ui`）与日志草稿
//! （`kv: projects_log_draft`）。
//!
//! 三条约定照抄全仓其它偏好（`pages/data.rs` 的 `set_flag` 是同一套）：
//!
//! 1. **单键 JSON**：整份偏好住一个 kv 键，不散成一堆键 —— 少一个键就少一处
//!    「写了这个忘了那个」的机会；
//! 2. **全字段 `Option` + `#[serde(default)]`**：缺字段回落**代码里的默认值**，
//!    默认值只有一处真源（[`ProjectsPrefs::view_mode`] / [`ProjectsPrefs::task_sort`]）；
//! 3. **写完 `flush`**：`kv_put` 是 400 ms 去抖的，写完立刻重绘会读回旧值、
//!    把界面弹回去。偏好是「用户点一下就该记住」的东西，等不起去抖窗口。
//!
//! # 为什么草稿不塞进 `projects_ui`
//!
//! 偏好与草稿的**生命周期不同**：偏好一旦写下就一直在（换视图、换排序都只改它的一
//! 部分字段），草稿是「正在打、还没保存的那几个字」—— 用掉（预填进编辑器）或丢掉，
//! 两者都不该留下痕迹。挤进同一个键的话，「清草稿」就变成「改偏好」，而两者都走
//! 「整份 JSON 覆盖写」的路径：弹窗里每敲一个字都要把偏好也重序列化一遍，
//! 任何一处序列化失败都会连偏好一起丢。
//!
//! # 为什么内核枚举不进 JSON
//!
//! [`TaskSort`] / [`ViewMode`] 都不做 serde 派生。内核（`duck-project`）不该为了
//! 界面偏好引入序列化依赖，界面枚举也不该把「枚举定义的形状」变成存档格式。
//! 所以 wire 形式由本模块钉成 `&'static str`：换枚举顺序、加变体、甚至换语言重写
//! 这一层，旧存档都不会读歪 —— 认不出的键一律回落默认值，**不 panic、不清档**。

use duck_project::{LogKind, TaskSort};
use gpui::App;

/// 整份偏好的存储键。
pub(crate) const KEY_PROJECTS_UI: &str = "projects_ui";

/// 日志草稿的存储键。
pub(crate) const KEY_LOG_DRAFT: &str = "projects_log_draft";

/// 主区显示哪一块。
///
/// `Board` / `List` / `Calendar` / `Timeline` / `Report` / `Log` 是**同一批任务的
/// 六个画法**（共用筛选 / 排序 / 选中，与原型 `VIEWS` 一一对应），`Archive` 是
/// 另一块内容（归档记录）。用 `is_archive()` 而不是逐个 `==` 比较：「不是归档」
/// 的分支自动覆盖全部常规视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ViewMode {
    /// 看板（默认）。
    #[default]
    Board,
    /// 列表（表格式，列可排序）。
    List,
    /// 日历（按截止日的月视图）。
    Calendar,
    /// 时间线（按周分段的截止排期）。
    Timeline,
    /// 报表（KPI + 趋势 + 分布 + 逾期清单）。
    Report,
    /// 工作日志（日报）。
    Log,
    /// 归档中心。
    Archive,
}

impl ViewMode {
    /// 常规视图的取值，按工具条里 segmented tabs 的显示顺序（原型 `VIEWS` 同序）。
    pub(crate) const TABS: [Self; 6] = [
        Self::Board,
        Self::List,
        Self::Calendar,
        Self::Timeline,
        Self::Report,
        Self::Log,
    ];

    /// wire 键。
    const fn key(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::List => "list",
            Self::Calendar => "calendar",
            Self::Timeline => "timeline",
            Self::Report => "report",
            Self::Log => "log",
            Self::Archive => "archive",
        }
    }

    /// 解析 wire 键；未知值返回 `None`（调用方回落默认值）。
    fn from_key(text: &str) -> Option<Self> {
        match text {
            "board" => Some(Self::Board),
            "list" => Some(Self::List),
            "calendar" => Some(Self::Calendar),
            "timeline" => Some(Self::Timeline),
            "report" => Some(Self::Report),
            "log" => Some(Self::Log),
            "archive" => Some(Self::Archive),
            _ => None,
        }
    }

    /// 是不是归档中心。
    ///
    /// 看板与列表共用「选中 / 归档 / 删除 / 完成态」这一整族动作，归档中心不共用
    /// —— 判据收成一个方法，免得散落的 `!= ViewMode::Board` 在加了列表之后逐个漏改。
    pub(crate) const fn is_archive(self) -> bool {
        matches!(self, Self::Archive)
    }
}

/// [`TaskSort`] 的 wire 键。
///
/// 与 `TaskSort::ALL` **逐一对应**，由 `sort_keys_cover_every_variant` 钉住：
/// 加了变体却忘了给键，那条测试立刻红。
pub(crate) const fn sort_key(sort: TaskSort) -> &'static str {
    match sort {
        TaskSort::Manual => "manual",
        TaskSort::DueAsc => "due_asc",
        TaskSort::DueDesc => "due_desc",
        TaskSort::PriorityFirst => "priority_first",
        TaskSort::ProgressDesc => "progress_desc",
        TaskSort::CreatedDesc => "created_desc",
        TaskSort::TitleAsc => "title_asc",
    }
}

/// 解析 [`TaskSort`] 的 wire 键；未知值返回 `None`。
pub(crate) fn sort_from_key(text: &str) -> Option<TaskSort> {
    TaskSort::ALL
        .into_iter()
        .find(|sort| sort_key(*sort) == text)
}

/// 列表视图的分组轴：同一批任务按什么「摆成几摞」。
///
/// 与 [`ViewMode`] / [`TaskSort`] 同一个待遇 —— 内核不认识它（分组是纯界面概念，
/// `duck-project` 只负责把任务查出来），wire 形式由本模块钉住：认不出的键一律回落
/// 默认值，**不 panic、不清档**。
///
/// 只作用于列表视图：看板的「分组」就是列本身，日历 / 时间线 / 报表各有自己的
/// 骨架，拿一个轴去套它们只会多出一层按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GroupMode {
    /// 不分组（原型 `none`）：一整张表，只有表头。
    None,
    /// 按优先级分组（默认 —— 与原型 `S.group` 的初值 `priority` 一致）。
    #[default]
    Priority,
    /// 按项目分组；未归入项目的合成**最后一组**（原型同款）。
    Project,
    /// 按状态分组（按看板列序）；**空组也画**，它顺带把列名当成了分组表头。
    Status,
}

impl GroupMode {
    /// 全部取值，按原型菜单的排列顺序。
    pub(crate) const ALL: [Self; 4] = [Self::Priority, Self::Project, Self::Status, Self::None];
}

/// [`GroupMode`] 的 wire 键。与 [`GroupMode::ALL`] **逐一对应**，由
/// `group_keys_cover_every_variant` 钉住：加了变体却忘了给键，那条测试立刻红。
pub(crate) const fn group_key(mode: GroupMode) -> &'static str {
    match mode {
        GroupMode::None => "none",
        GroupMode::Priority => "priority",
        GroupMode::Project => "project",
        GroupMode::Status => "status",
    }
}

/// 解析 [`GroupMode`] 的 wire 键；未知值返回 `None`。
pub(crate) fn group_from_key(text: &str) -> Option<GroupMode> {
    GroupMode::ALL
        .into_iter()
        .find(|mode| group_key(*mode) == text)
}

/// 日历的画法：月网格 / 周网格（设计文档 §5.3 的「月 / 周双模」）。
///
/// **落盘**（与视图 / 排序 / 分组 / 报表范围同待遇）：它是「我习惯按周排期」这一类的
/// 长期口径，不是「这一屏我想看看别的」——后者是报表的状态 / 标签筛选那种一次性视角。
///
/// 两档的差别**不只是**格数：月视图 6×7 恒 42 格、每格 2 条 + `+N`；周视图一行 7 列、
/// 撑满可用高度、每格能列更多条。几何与容量一起由 `view::CalendarLayout` 从它推出来，
/// 免得「周视图按 42 格算」这种偏差散在渲染代码里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CalendarMode {
    /// 月网格（默认，与参考实现的初值一致）。
    #[default]
    Month,
    /// 周网格（一行 7 列，看板式的列头）。
    Week,
}

impl CalendarMode {
    /// 全部取值，按切换条上的顺序。
    pub(crate) const ALL: [Self; 2] = [Self::Month, Self::Week];

    /// wire 键。
    const fn key(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Week => "week",
        }
    }

    /// 解析 wire 键；未知值返回 `None`（调用方回落默认值）。
    fn from_key(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == text)
    }
}

/// 报表的时间范围档（`report.rs` 用它决定趋势桶数与「范围内完成」的口径）。
///
/// 三档而不是「自定义起止日」：报表上的时间只用在两处（完成趋势、完成明细），这两处
/// 要的是「最近一段」而不是一个精确区间；三档一次点击就能换，日期选择器要点四下。
///
/// **落盘**（与 [`ViewMode`] / 排序 / 分组同待遇）：换一档是「我平时就这么看报表」，
/// 下次进来仍该是那一档。报表上的状态 / 标签筛选则相反 —— 那是「我现在想看看这批」，
/// 属于一次性视角，不落盘（见 `report::ReportStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReportRange {
    /// 近 7 天（默认，与报表原本那一版一致）。
    #[default]
    Days7,
    /// 近 30 天。
    Days30,
    /// 近 90 天（按周分桶，13 根柱）。
    Days90,
}

impl ReportRange {
    /// 全部取值，按筛选条上的顺序。
    pub(crate) const ALL: [Self; 3] = [Self::Days7, Self::Days30, Self::Days90];

    /// 名义天数（界面上的「近 N 天」）。
    ///
    /// 实际取数窗口见 `report::window_days`：按周分桶的 90 天会取整到 91 天。
    pub(crate) const fn days(self) -> i64 {
        match self {
            Self::Days7 => 7,
            Self::Days30 => 30,
            Self::Days90 => 90,
        }
    }

    /// 趋势图每根柱跨几天（1 = 按日，7 = 按周）。
    pub(crate) const fn bucket_days(self) -> i64 {
        match self {
            Self::Days7 | Self::Days30 => 1,
            Self::Days90 => 7,
        }
    }

    /// wire 键。
    const fn key(self) -> &'static str {
        match self {
            Self::Days7 => "7d",
            Self::Days30 => "30d",
            Self::Days90 => "90d",
        }
    }

    /// 解析 wire 键；未知值返回 `None`。
    fn from_key(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|range| range.key() == text)
    }
}

/// 落盘的偏好快照。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ProjectsPrefs {
    /// 主区视图（[`ViewMode`] 的 wire 键）。
    pub(crate) view: Option<String>,
    /// 任务排序口径（[`sort_key`] 的 wire 键）。
    pub(crate) sort: Option<String>,
    /// 列表分组轴（[`group_key`] 的 wire 键）。
    pub(crate) group: Option<String>,
    /// 日历画法（[`CalendarMode`] 的 wire 键）。
    pub(crate) calendar_mode: Option<String>,
    /// 报表时间范围档（[`ReportRange`] 的 wire 键）。
    pub(crate) report_range: Option<String>,
}

impl ProjectsPrefs {
    /// 读盘；没写过或解析失败时是**默认偏好**（不报错、不挡页面）。
    pub(crate) fn load(cx: &App) -> Self {
        let Some(json) = duck_store::kv_get(cx, KEY_PROJECTS_UI) else {
            return Self::default();
        };
        match serde_json::from_str(&json) {
            Ok(prefs) => prefs,
            Err(error) => {
                tracing::warn!(%error, "failed to parse projects prefs, falling back to defaults");
                Self::default()
            }
        }
    }

    /// 写盘并**立刻落盘**（见模块头第 3 条）。
    pub(crate) fn save(cx: &App, prefs: &Self) {
        let Ok(json) = serde_json::to_string(prefs) else {
            // 序列化失败只可能是存储层坏了；不写半截 JSON 覆盖掉用户的偏好。
            tracing::warn!("failed to serialize projects prefs");
            return;
        };
        duck_store::kv_put(cx, KEY_PROJECTS_UI, json);
        duck_store::flush(cx);
    }

    /// 主区视图；缺字段或认不出时回落到看板。
    pub(crate) fn view_mode(&self) -> ViewMode {
        self.view
            .as_deref()
            .and_then(ViewMode::from_key)
            .unwrap_or_default()
    }

    /// 排序口径；缺字段或认不出时回落到手动顺序（内核的 `TaskSort` 默认值）。
    pub(crate) fn task_sort(&self) -> TaskSort {
        self.sort
            .as_deref()
            .and_then(sort_from_key)
            .unwrap_or_default()
    }

    /// 列表分组轴；缺字段或认不出时回落到按优先级（原型的初值）。
    pub(crate) fn group_mode(&self) -> GroupMode {
        self.group
            .as_deref()
            .and_then(group_from_key)
            .unwrap_or_default()
    }

    /// 报表时间范围档；缺字段或认不出时回落到近 7 天。
    pub(crate) fn report(&self) -> ReportRange {
        self.report_range
            .as_deref()
            .and_then(ReportRange::from_key)
            .unwrap_or_default()
    }

    /// 日历画法；缺字段或认不出时回落到月视图。
    pub(crate) fn calendar(&self) -> CalendarMode {
        self.calendar_mode
            .as_deref()
            .and_then(CalendarMode::from_key)
            .unwrap_or_default()
    }

    /// 记下视图。
    pub(crate) fn with_view(mut self, mode: ViewMode) -> Self {
        self.view = Some(mode.key().to_owned());
        self
    }

    /// 记下排序口径。
    pub(crate) fn with_sort(mut self, sort: TaskSort) -> Self {
        self.sort = Some(sort_key(sort).to_owned());
        self
    }

    /// 记下列表分组轴。
    pub(crate) fn with_group(mut self, mode: GroupMode) -> Self {
        self.group = Some(group_key(mode).to_owned());
        self
    }

    /// 记下报表时间范围档。
    pub(crate) fn with_report(mut self, range: ReportRange) -> Self {
        self.report_range = Some(range.key().to_owned());
        self
    }

    /// 记下日历画法。
    pub(crate) fn with_calendar(mut self, mode: CalendarMode) -> Self {
        self.calendar_mode = Some(mode.key().to_owned());
        self
    }
}

/// 一篇「正在打、还没保存」的日志。
///
/// 为什么需要它：日志是**弹窗里**写的（`dialogs::open_log_editor`），而弹窗里的
/// 「取消 / 点外面关掉」与「保存」走的是两条路 —— 前者不会经过任何写库调用。没有草稿，
/// 关掉弹窗就等于把刚打的字扔掉，而用户点「取消」的意图通常是「先放放」。
///
/// 生命周期（三条，代码里由 `dialog::open_log_editor` 与 `submit_log` 落实）：
///
/// - **打开弹窗**：库里那一篇为空、且草稿正好属于这一天这一档 → 预填草稿；
///   库里已经有正文时**不**预填（那篇日志已经保存过了，草稿属于更早的一次编辑）。
/// - **边打边存**：输入框每次变化都写一遍（`kv_put` 自己是 400 ms 去抖的，
///   所以不需要在这里再做一层）。
/// - **保存成功**：删掉草稿（`kv_delete`，不是写空串 —— 见 `duck_store::Store::kv_delete`
///   的注释：写空串会留下一行空值，读回来是 `Some("")` 而不是 `None`）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct LogDraft {
    /// 归属日期（`YYYY-MM-DD`）。
    pub(crate) date: Option<String>,
    /// 档位（[`duck_project::LogKind::as_str`] 的稳定键）。
    pub(crate) kind: Option<String>,
    /// 正文。
    pub(crate) content: Option<String>,
    /// 标签。
    pub(crate) tags: Option<Vec<String>>,
    /// 记下草稿的时刻（Unix 秒）。只用于「这是什么时候留下的」，不参与任何判断。
    pub(crate) saved_at: Option<i64>,
}

impl LogDraft {
    /// 记一份草稿。
    pub(crate) fn new(
        date: duck_project::CalendarDate,
        kind: LogKind,
        content: String,
        tags: Vec<String>,
    ) -> Self {
        Self {
            date: Some(date.to_iso()),
            kind: Some(kind.as_str().to_owned()),
            content: Some(content),
            tags: Some(tags),
            saved_at: Some(duck_project::now_seconds()),
        }
    }

    /// 读盘；没写过或解析失败时是 `None`（读不出草稿只是少了个便利，不该挡任何路径）。
    pub(crate) fn load(cx: &App) -> Option<Self> {
        let json = duck_store::kv_get(cx, KEY_LOG_DRAFT)?;
        match serde_json::from_str(&json) {
            Ok(draft) => Some(draft),
            Err(error) => {
                tracing::warn!(%error, "failed to parse the log draft, ignoring it");
                None
            }
        }
    }

    /// 写盘。**不 flush**：草稿是去抖写里最典型的「丢了也不致命」的一类，
    /// 而每次敲键都 flush 会把设置落盘路径也拖慢（见模块头第 3 条的反面）。
    pub(crate) fn save(&self, cx: &App) {
        let Ok(json) = serde_json::to_string(self) else {
            tracing::warn!("failed to serialize the log draft");
            return;
        };
        duck_store::kv_put(cx, KEY_LOG_DRAFT, json);
    }

    /// 丢掉草稿（保存成功之后）。
    pub(crate) fn clear(cx: &App) {
        duck_store::kv_delete(cx, KEY_LOG_DRAFT);
    }

    /// 这份草稿是不是「这一天、这一档」的。
    ///
    /// 两档共用一份草稿：只按日期判断的话，在周报里打到一半去写日报会看到周报的字。
    pub(crate) fn belongs_to(&self, date: duck_project::CalendarDate, kind: LogKind) -> bool {
        self.date.as_deref() == Some(date.to_iso().as_str())
            && self.kind.as_deref() == Some(kind.as_str())
    }

    /// 正文。
    pub(crate) fn content(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }

    /// 标签。
    pub(crate) fn tags(&self) -> Vec<String> {
        self.tags.clone().unwrap_or_default()
    }

    /// 什么都没打（只是打开过弹窗）—— 这种草稿不值得预填，也不值得留着。
    pub(crate) fn is_empty(&self) -> bool {
        self.content().trim().is_empty() && self.tags().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_keys_cover_every_variant() {
        for sort in TaskSort::ALL {
            let key = sort_key(sort);
            assert_eq!(sort_from_key(key), Some(sort), "{key} 应能还原");
        }
        assert_eq!(sort_from_key("nope"), None);
    }

    #[test]
    fn group_keys_cover_every_variant() {
        for mode in GroupMode::ALL {
            let key = group_key(mode);
            assert_eq!(group_from_key(key), Some(mode), "{key} 应能还原");
        }
        assert_eq!(group_from_key("nope"), None);
    }

    #[test]
    fn group_menu_lists_none_last() {
        // 原型菜单把「不分组」摆在最后（前三条都是「按……分组」，最后一条是不分组）。
        assert_eq!(GroupMode::ALL[3], GroupMode::None);
    }

    #[test]
    fn report_range_keys_cover_every_variant() {
        for range in ReportRange::ALL {
            assert_eq!(
                ReportRange::from_key(match range {
                    ReportRange::Days7 => "7d",
                    ReportRange::Days30 => "30d",
                    ReportRange::Days90 => "90d",
                }),
                Some(range),
                "{range:?}"
            );
        }
        assert_eq!(ReportRange::from_key("1y"), None);
    }

    #[test]
    fn report_ranges_widen_and_bucket_by_week_at_ninety() {
        // 天数与分桶一起钉：换档时趋势图的桶数由这两条算出（`report::window_days`）。
        assert_eq!(ReportRange::ALL[0], ReportRange::Days7);
        assert!(ReportRange::Days7.days() < ReportRange::Days30.days());
        assert!(ReportRange::Days30.days() < ReportRange::Days90.days());
        assert_eq!(ReportRange::Days7.bucket_days(), 1);
        assert_eq!(ReportRange::Days30.bucket_days(), 1);
        assert_eq!(ReportRange::Days90.bucket_days(), 7, "90 根柱画不清，按周");
        assert_eq!(ReportRange::default(), ReportRange::Days7);
    }

    #[test]
    fn calendar_mode_keys_cover_every_variant() {
        // 与视图 / 排序 / 分组 / 报表范围同一个待遇：加了变体却忘了给键，这条立刻红。
        for mode in CalendarMode::ALL {
            let key = match mode {
                CalendarMode::Month => "month",
                CalendarMode::Week => "week",
            };
            assert_eq!(CalendarMode::from_key(key), Some(mode), "{mode:?}");
        }
        assert_eq!(CalendarMode::from_key("quarter"), None);
        assert_eq!(CalendarMode::default(), CalendarMode::Month);
    }

    #[test]
    fn missing_fields_fall_back_to_the_defaults() {
        let prefs: ProjectsPrefs = serde_json::from_str("{}").expect("空对象可解析");
        assert_eq!(prefs.view_mode(), ViewMode::Board);
        assert_eq!(prefs.task_sort(), TaskSort::Manual);
        assert_eq!(prefs.group_mode(), GroupMode::Priority);
        assert_eq!(prefs.report(), ReportRange::Days7);
        assert_eq!(prefs.calendar(), CalendarMode::Month);
    }

    #[test]
    fn unknown_keys_fall_back_to_the_defaults_instead_of_failing() {
        // 旧版本写下、或被手改坏的 JSON 都不该让偏好读不出来。
        let prefs: ProjectsPrefs = serde_json::from_str(
            r#"{"view":"gantt","sort":"by_vibes","group":"by_vibes","report_range":"forever","calendar_mode":"quarter"}"#,
        )
        .expect("仍可解析");
        assert_eq!(prefs.view_mode(), ViewMode::Board);
        assert_eq!(prefs.task_sort(), TaskSort::Manual);
        assert_eq!(prefs.group_mode(), GroupMode::Priority);
        assert_eq!(prefs.report(), ReportRange::Days7);
        assert_eq!(prefs.calendar(), CalendarMode::Month);
    }

    #[test]
    fn every_view_mode_round_trips_through_its_wire_key() {
        // 七个变体各走一遍 key → from_key；加了变体却忘了给键，这条立刻红。
        for mode in [
            ViewMode::Board,
            ViewMode::List,
            ViewMode::Calendar,
            ViewMode::Timeline,
            ViewMode::Report,
            ViewMode::Log,
            ViewMode::Archive,
        ] {
            assert_eq!(ViewMode::from_key(mode.key()), Some(mode), "{mode:?}");
        }
        assert_eq!(ViewMode::from_key("kanban"), None);
    }

    #[test]
    fn view_tabs_are_the_regular_views_in_order() {
        // 工具条上的 segmented tabs 只该出现常规视图，且不含归档中心（它有左栏入口）。
        assert_eq!(
            ViewMode::TABS,
            [
                ViewMode::Board,
                ViewMode::List,
                ViewMode::Calendar,
                ViewMode::Timeline,
                ViewMode::Report,
                ViewMode::Log,
            ]
        );
        assert!(!ViewMode::TABS.iter().any(|mode| mode.is_archive()));
        assert!(ViewMode::Archive.is_archive());
        assert!(!ViewMode::Board.is_archive());
        assert!(!ViewMode::List.is_archive());
    }

    #[test]
    fn the_list_view_is_persisted_like_the_board() {
        let prefs = ProjectsPrefs::default().with_view(ViewMode::List);
        let json = serde_json::to_string(&prefs).expect("可序列化");
        assert!(json.contains("\"list\""), "wire 键要稳定：{json}");
        let back: ProjectsPrefs = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back.view_mode(), ViewMode::List);
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let prefs = ProjectsPrefs::default()
            .with_view(ViewMode::Archive)
            .with_sort(TaskSort::DueAsc)
            .with_group(GroupMode::Status)
            .with_calendar(CalendarMode::Week)
            .with_report(ReportRange::Days90);
        let json = serde_json::to_string(&prefs).expect("可序列化");
        let back: ProjectsPrefs = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, prefs);
        assert_eq!(back.view_mode(), ViewMode::Archive);
        assert_eq!(back.task_sort(), TaskSort::DueAsc);
        assert_eq!(back.group_mode(), GroupMode::Status);
        assert_eq!(back.calendar(), CalendarMode::Week);
        assert_eq!(back.report(), ReportRange::Days90);
        assert!(json.contains("\"90d\""), "wire 键要稳定：{json}");
        assert!(json.contains("\"week\""), "wire 键要稳定：{json}");
    }

    /// 草稿按「日期 + 档位」认领：两档共用一份草稿，光比日期会让周报读到日报的字。
    #[test]
    fn a_draft_only_belongs_to_its_own_day_and_kind() {
        let today = duck_project::CalendarDate::new(2026, 10, 14).expect("合法日期");
        let draft = LogDraft::new(
            today,
            LogKind::Weekly,
            "周三写了一半".to_owned(),
            vec!["复盘".to_owned()],
        );
        assert!(draft.belongs_to(today, LogKind::Weekly));
        assert!(!draft.belongs_to(today, LogKind::Daily), "档位不同不该认领");
        assert!(
            !draft.belongs_to(today.add_days(1), LogKind::Weekly),
            "日期不同"
        );
        assert_eq!(draft.content(), "周三写了一半");
        assert_eq!(draft.tags(), ["复盘"]);
        assert!(!draft.is_empty());
        assert!(draft.saved_at.is_some());
    }

    #[test]
    fn an_empty_draft_is_recognised_and_missing_fields_do_not_panic() {
        assert!(LogDraft::default().is_empty(), "没打过字就不算草稿");
        // 手改坏的存档：认不出的字段一律走缺省，不 panic。
        let draft: LogDraft =
            serde_json::from_str(r#"{"date":"2026-10-14","kind":"nope"}"#).expect("仍可解析");
        assert!(draft.content().is_empty());
        assert!(draft.tags().is_empty());
        assert!(draft.is_empty());
        assert!(!draft.belongs_to(
            duck_project::CalendarDate::new(2026, 10, 14).expect("合法"),
            LogKind::Daily
        ));
    }

    #[test]
    fn a_draft_round_trips_through_json() {
        let today = duck_project::CalendarDate::new(2026, 10, 14).expect("合法日期");
        let draft = LogDraft::new(
            today,
            LogKind::Daily,
            "正文".to_owned(),
            vec!["线上事故".to_owned(), "复盘".to_owned()],
        );
        let json = serde_json::to_string(&draft).expect("可序列化");
        let back: LogDraft = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, draft);
        assert!(json.contains("2026-10-14"), "日期要可读：{json}");
        assert!(json.contains("\"daily\""), "档位用内核的稳定键：{json}");
    }
}
