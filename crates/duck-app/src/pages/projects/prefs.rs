//! 项目管理页的 UI 偏好（设计文档 §6.2 的 `kv: projects_ui`）。
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
//! # 为什么内核枚举不进 JSON
//!
//! [`TaskSort`] / [`ViewMode`] 都不做 serde 派生。内核（`duck-project`）不该为了
//! 界面偏好引入序列化依赖，界面枚举也不该把「枚举定义的形状」变成存档格式。
//! 所以 wire 形式由本模块钉成 `&'static str`：换枚举顺序、加变体、甚至换语言重写
//! 这一层，旧存档都不会读歪 —— 认不出的键一律回落默认值，**不 panic、不清档**。

use duck_project::TaskSort;
use gpui::App;

/// 整份偏好的存储键。
pub(crate) const KEY_PROJECTS_UI: &str = "projects_ui";

/// 主区显示哪一块。
///
/// `Board` / `List` 是**同一批任务的两个画法**（共用筛选 / 排序 / 选中），`Archive`
/// 是另一块内容（归档记录），三者互斥。用 `is_archive()` 而不是逐个 `==` 比较：
/// 将来再加第三个常规视图（日历 / 时间线）时，「不是归档」的分支自动跟着对。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ViewMode {
    /// 看板（默认）。
    #[default]
    Board,
    /// 列表（表格式，列可排序）。
    List,
    /// 归档中心。
    Archive,
}

impl ViewMode {
    /// 常规视图（看板 / 列表）的取值，按工具条里 segmented tabs 的显示顺序。
    pub(crate) const TABS: [Self; 2] = [Self::Board, Self::List];

    /// wire 键。
    const fn key(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::List => "list",
            Self::Archive => "archive",
        }
    }

    /// 解析 wire 键；未知值返回 `None`（调用方回落默认值）。
    fn from_key(text: &str) -> Option<Self> {
        match text {
            "board" => Some(Self::Board),
            "list" => Some(Self::List),
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

/// 落盘的偏好快照。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ProjectsPrefs {
    /// 主区视图（[`ViewMode`] 的 wire 键）。
    pub(crate) view: Option<String>,
    /// 任务排序口径（[`sort_key`] 的 wire 键）。
    pub(crate) sort: Option<String>,
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
    fn missing_fields_fall_back_to_the_defaults() {
        let prefs: ProjectsPrefs = serde_json::from_str("{}").expect("空对象可解析");
        assert_eq!(prefs.view_mode(), ViewMode::Board);
        assert_eq!(prefs.task_sort(), TaskSort::Manual);
    }

    #[test]
    fn unknown_keys_fall_back_to_the_defaults_instead_of_failing() {
        // 旧版本写下、或被手改坏的 JSON 都不该让偏好读不出来。
        let prefs: ProjectsPrefs =
            serde_json::from_str(r#"{"view":"calendar","sort":"by_vibes"}"#).expect("仍可解析");
        assert_eq!(prefs.view_mode(), ViewMode::Board);
        assert_eq!(prefs.task_sort(), TaskSort::Manual);
    }

    #[test]
    fn every_view_mode_round_trips_through_its_wire_key() {
        // 三个变体各走一遍 key → from_key；加了变体却忘了给键，这条立刻红。
        for mode in [ViewMode::Board, ViewMode::List, ViewMode::Archive] {
            assert_eq!(ViewMode::from_key(mode.key()), Some(mode), "{mode:?}");
        }
        assert_eq!(ViewMode::from_key("kanban"), None);
    }

    #[test]
    fn view_tabs_are_the_regular_views_in_order() {
        // 工具条上的 segmented tabs 只该出现常规视图，且不含归档中心（它有左栏入口）。
        assert_eq!(ViewMode::TABS, [ViewMode::Board, ViewMode::List]);
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
    fn a_round_trip_keeps_both_fields() {
        let prefs = ProjectsPrefs::default()
            .with_view(ViewMode::Archive)
            .with_sort(TaskSort::DueAsc);
        let json = serde_json::to_string(&prefs).expect("可序列化");
        let back: ProjectsPrefs = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, prefs);
        assert_eq!(back.view_mode(), ViewMode::Archive);
        assert_eq!(back.task_sort(), TaskSort::DueAsc);
    }
}
