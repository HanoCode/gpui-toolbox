//! 任务查询：筛选口径、排序与归档可见性（纯逻辑，可无环境依赖地单测）。
//!
//! 这里是**口径的唯一真源**：界面上的「今天 / 本周 / 逾期 / 未排期」四个快捷筛选、
//! 侧栏计数、报表统计都调它，不各自写一遍 `WHERE`。
//!
//! # 归档可见性（最重要的一条）
//!
//! **所有常规视图一律附加 `archived_at IS NULL`**，这不是调用方的自觉，而是
//! [`TaskQuery`] 的默认值（[`ArchiveScope::Live`]）。要看归档项必须显式改成
//! [`ArchiveScope::Archived`]，这只有「归档中心」会做。默认值选对方向 ——
//! 忘记加条件的后果是「归档项漏出来」，那比「归档项少显示一条」严重得多。
//!
//! # 「今天」由调用方传入
//!
//! 内核不知道用户在哪（`crate::time` 的模块注释）。用到 [`DueFilter`] 时才需要
//! [`TaskQuery::today`]，缺失就是 [`ProjectError::Invalid`] —— 宁可报错也不要悄悄
//! 退回 UTC 今天（那会在 UTC+8 的早上 8 点前把所有「今天到期」判成「明天到期」）。

use rusqlite::types::Value;

use crate::error::{ProjectError, Result};
use crate::models::{Priority, Quadrant, TaskStatus};
use crate::time::CalendarDate;

/// 归档可见性。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArchiveScope {
    /// 只看未归档（**默认**，所有常规视图）。
    #[default]
    Live,
    /// 只看已归档（「归档中心」）。
    Archived,
    /// 都看（统计的「含归档」口径、排重）。
    Any,
}

impl ArchiveScope {
    /// 是否为「常规视图」口径。
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// 日期快捷筛选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueFilter {
    /// 今天到期且未完成。
    Today,
    /// 本周（周一至周日）到期且未完成。
    ThisWeek,
    /// 已逾期（截止日早于今天，且未完成 / 未取消）。
    Overdue,
    /// 未排期（没有截止日，且未完成 / 未取消）。
    Unscheduled,
}

impl DueFilter {
    /// 全部取值，界面按这个顺序排 chips。
    pub const ALL: [Self; 4] = [
        Self::Today,
        Self::ThisWeek,
        Self::Overdue,
        Self::Unscheduled,
    ];

    /// 该筛选是否需要 [`TaskQuery::today`]。
    #[must_use]
    pub const fn needs_today(self) -> bool {
        matches!(self, Self::Today | Self::ThisWeek | Self::Overdue)
    }
}

/// 排序口径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskSort {
    /// 手动顺序（列内拖拽的结果）。
    #[default]
    Manual,
    /// 截止日升序，无截止日排最后。
    DueAsc,
    /// 截止日降序，无截止日排最后。
    DueDesc,
    /// 优先级（P0 最前），同级按手动顺序。
    PriorityFirst,
    /// 进度高者在前，同级按手动顺序。
    ProgressDesc,
    /// 最近创建优先。
    CreatedDesc,
    /// 标题升序。
    TitleAsc,
}

impl TaskSort {
    /// 全部取值。
    pub const ALL: [Self; 7] = [
        Self::Manual,
        Self::DueAsc,
        Self::DueDesc,
        Self::PriorityFirst,
        Self::ProgressDesc,
        Self::CreatedDesc,
        Self::TitleAsc,
    ];

    /// SQL 的 `ORDER BY` 片段。
    ///
    /// 每条都带 `t.id` 作为末位键：没有它，`sort_order` / `due_date` 相同的行在两次
    /// 查询里可能以不同顺序返回，界面会看到卡片「自己换位置」。
    #[must_use]
    const fn order_by(self) -> &'static str {
        match self {
            Self::Manual => "t.sort_order ASC, t.id ASC",
            // `IS NULL` 先排，保证「没排期的」沉到最后（SQLite 里 NULL 是排在最前的）。
            Self::DueAsc => "t.due_date IS NULL ASC, t.due_date ASC, t.id ASC",
            Self::DueDesc => "t.due_date IS NULL ASC, t.due_date DESC, t.id DESC",
            Self::PriorityFirst => "t.priority ASC, t.sort_order ASC, t.id ASC",
            Self::ProgressDesc => "t.progress DESC, t.sort_order ASC, t.id ASC",
            Self::CreatedDesc => "t.created_at DESC, t.id DESC",
            Self::TitleAsc => "t.title COLLATE NOCASE ASC, t.id ASC",
        }
    }
}

/// 任务查询（构建器）。
#[derive(Debug, Clone)]
pub struct TaskQuery {
    archive: ArchiveScope,
    project_ids: Vec<i64>,
    inbox_only: bool,
    statuses: Vec<TaskStatus>,
    priorities: Vec<Priority>,
    quadrants: Vec<Quadrant>,
    tag_ids: Vec<i64>,
    parent_id: Option<i64>,
    top_level_only: bool,
    due: Option<DueFilter>,
    search: Option<String>,
    today: Option<CalendarDate>,
    sort: TaskSort,
    limit: Option<u32>,
}

impl Default for TaskQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskQuery {
    /// 空查询：未归档 + 手动顺序 + 不限量。
    #[must_use]
    pub fn new() -> Self {
        Self {
            archive: ArchiveScope::default(),
            project_ids: Vec::new(),
            inbox_only: false,
            statuses: Vec::new(),
            priorities: Vec::new(),
            quadrants: Vec::new(),
            tag_ids: Vec::new(),
            parent_id: None,
            top_level_only: false,
            due: None,
            search: None,
            today: None,
            sort: TaskSort::default(),
            limit: None,
        }
    }

    /// 归档可见性。
    #[must_use]
    pub fn archive(mut self, scope: ArchiveScope) -> Self {
        self.archive = scope;
        self
    }

    /// 限定项目集合（空 = 不限）。
    #[must_use]
    pub fn projects(mut self, project_ids: impl IntoIterator<Item = i64>) -> Self {
        self.project_ids = project_ids.into_iter().collect();
        self
    }

    /// 只看收集箱（`project_id IS NULL`）。
    #[must_use]
    pub const fn inbox(mut self) -> Self {
        self.inbox_only = true;
        self
    }

    /// 限定状态集合（空 = 不限）。
    #[must_use]
    pub fn statuses(mut self, statuses: impl IntoIterator<Item = TaskStatus>) -> Self {
        self.statuses = statuses.into_iter().collect();
        self
    }

    /// 限定优先级集合（空 = 不限）。
    #[must_use]
    pub fn priorities(mut self, priorities: impl IntoIterator<Item = Priority>) -> Self {
        self.priorities = priorities.into_iter().collect();
        self
    }

    /// 限定四象限集合（空 = 不限）。
    #[must_use]
    pub fn quadrants(mut self, quadrants: impl IntoIterator<Item = Quadrant>) -> Self {
        self.quadrants = quadrants.into_iter().collect();
        self
    }

    /// 限定标签（命中**任一**即通过，空 = 不限）。
    #[must_use]
    pub fn tags(mut self, tag_ids: impl IntoIterator<Item = i64>) -> Self {
        self.tag_ids = tag_ids.into_iter().collect();
        self
    }

    /// 只看某个父任务下的子任务。
    #[must_use]
    pub const fn children_of(mut self, parent_id: i64) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// 只看顶层任务（看板与列表的默认口径：子任务跟着父卡片显示）。
    #[must_use]
    pub const fn top_level_only(mut self) -> Self {
        self.top_level_only = true;
        self
    }

    /// 日期快捷筛选。
    #[must_use]
    pub const fn due(mut self, due: DueFilter) -> Self {
        self.due = Some(due);
        self
    }

    /// 关键词（匹配标题与描述，大小写不敏感；`%`/`_`/`\` 会被转义）。
    #[must_use]
    pub fn search(mut self, term: impl Into<String>) -> Self {
        let term = term.into();
        if !term.trim().is_empty() {
            self.search = Some(term);
        }
        self
    }

    /// 本地「今天」（用到 [`DueFilter`] 时必需）。
    #[must_use]
    pub const fn today(mut self, today: CalendarDate) -> Self {
        self.today = Some(today);
        self
    }

    /// 排序。
    #[must_use]
    pub const fn sort(mut self, sort: TaskSort) -> Self {
        self.sort = sort;
        self
    }

    /// 结果上限。
    #[must_use]
    pub const fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// 归档可见性（供 store 决定要不要顺带查「随项目归档」的行）。
    #[must_use]
    pub const fn archive_scope(&self) -> ArchiveScope {
        self.archive
    }

    /// 取「今天」；调用方没给就报错（只有 [`DueFilter::Unscheduled`] 不需要它）。
    fn require_today(&self, due: DueFilter) -> Result<CalendarDate> {
        self.today.ok_or_else(|| {
            ProjectError::Invalid(format!(
                "due filter {} requires TaskQuery::today",
                due_name(due)
            ))
        })
    }

    /// 生成 `WHERE` 片段与参数。
    ///
    /// 返回的是**不含** `SELECT` 的片段（`WHERE … ORDER BY … LIMIT …`），由 store
    /// 拼到固定的列清单后面；参数顺序与占位符出现顺序严格一致。
    pub(crate) fn build(&self) -> Result<(String, Vec<Value>)> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();

        match self.archive {
            ArchiveScope::Live => clauses.push("t.archived_at IS NULL".to_owned()),
            ArchiveScope::Archived => clauses.push("t.archived_at IS NOT NULL".to_owned()),
            ArchiveScope::Any => {}
        }
        if self.inbox_only {
            clauses.push("t.project_id IS NULL".to_owned());
        }
        if let Some(parent_id) = self.parent_id {
            clauses.push("t.parent_id = ?".to_owned());
            args.push(Value::Integer(parent_id));
        } else if self.top_level_only {
            clauses.push("t.parent_id IS NULL".to_owned());
        }

        push_in(
            &mut clauses,
            &mut args,
            "t.project_id",
            self.project_ids.iter().map(|id| Value::Integer(*id)),
        );
        push_in(
            &mut clauses,
            &mut args,
            "t.status",
            self.statuses
                .iter()
                .map(|status| Value::Text(status.as_str().to_owned())),
        );
        push_in(
            &mut clauses,
            &mut args,
            "t.priority",
            self.priorities.iter().map(|p| Value::Integer(p.as_i64())),
        );
        push_in(
            &mut clauses,
            &mut args,
            "t.quadrant",
            self.quadrants
                .iter()
                .map(|q| Value::Text(q.as_str().to_owned())),
        );

        // 标签是「命中任一」的语义 → EXISTS 子查询，而不是 JOIN：
        // JOIN 会让一个挂 3 个标签的任务出现 3 行，调用方还得自己去重。
        if !self.tag_ids.is_empty() {
            let placeholders = placeholders(self.tag_ids.len());
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM task_tags tt \
                 WHERE tt.task_id = t.id AND tt.tag_id IN ({placeholders}))"
            ));
            args.extend(self.tag_ids.iter().map(|id| Value::Integer(*id)));
        }

        if let Some(due) = self.due {
            match due {
                DueFilter::Today => {
                    let today = self.require_today(due)?;
                    clauses.push(format!(
                        "t.due_date = ? AND t.status NOT IN ({})",
                        SETTLED_STATUSES_SQL
                    ));
                    args.push(Value::Text(today.to_iso()));
                }
                DueFilter::ThisWeek => {
                    let today = self.require_today(due)?;
                    clauses.push(format!(
                        "t.due_date BETWEEN ? AND ? AND t.status NOT IN ({})",
                        SETTLED_STATUSES_SQL
                    ));
                    args.push(Value::Text(today.start_of_week().to_iso()));
                    args.push(Value::Text(today.end_of_week().to_iso()));
                }
                DueFilter::Overdue => {
                    let today = self.require_today(due)?;
                    clauses.push(format!(
                        "t.due_date IS NOT NULL AND t.due_date < ? AND t.status NOT IN ({})",
                        SETTLED_STATUSES_SQL
                    ));
                    args.push(Value::Text(today.to_iso()));
                }
                DueFilter::Unscheduled => {
                    clauses.push(format!(
                        "t.due_date IS NULL AND t.status NOT IN ({})",
                        SETTLED_STATUSES_SQL
                    ));
                }
            }
        }

        if let Some(term) = &self.search {
            let pattern = format!("%{}%", escape_like(term));
            // `ESCAPE '\'` 必须显式写：否则用户搜 `100%` 会被当成通配符。
            clauses.push(
                r"(t.title LIKE ? ESCAPE '\' OR t.description_md LIKE ? ESCAPE '\')".to_owned(),
            );
            args.push(Value::Text(pattern.clone()));
            args.push(Value::Text(pattern));
        }

        let mut sql = String::new();
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(self.sort.order_by());
        if let Some(limit) = self.limit {
            sql.push_str(" LIMIT ?");
            args.push(Value::Integer(i64::from(limit)));
        }
        Ok((sql, args))
    }
}

/// 已完成 / 已取消的稳定键，拼进 `NOT IN (…)`。
const SETTLED_STATUSES_SQL: &str = "'done', 'cancelled'";

/// `DueFilter` 的英文名（错误详情用；错误详情一律英文）。
const fn due_name(due: DueFilter) -> &'static str {
    match due {
        DueFilter::Today => "today",
        DueFilter::ThisWeek => "this_week",
        DueFilter::Overdue => "overdue",
        DueFilter::Unscheduled => "unscheduled",
    }
}

/// `?` 占位符列表。
fn placeholders(count: usize) -> String {
    let mut out = String::with_capacity(count * 3);
    for index in 0..count {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('?');
    }
    out
}

/// 空集合 = 不加条件（而不是「匹配不到任何行」）—— 界面上「没选项目」的语义是「不限」。
fn push_in(
    clauses: &mut Vec<String>,
    args: &mut Vec<Value>,
    column: &str,
    values: impl Iterator<Item = Value>,
) {
    let values: Vec<Value> = values.collect();
    if values.is_empty() {
        return;
    }
    clauses.push(format!("{column} IN ({})", placeholders(values.len())));
    args.extend(values);
}

/// 转义 `LIKE` 的元字符（`\` 自己也要转，否则搜 `\` 会吃掉后面的字符）。
///
/// `pub(crate)`：归档中心的「只搜归档区」也要用同一套转义 —— 两份实现早晚会分叉。
pub(crate) fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ArchiveScope, DueFilter, TaskQuery, TaskSort, escape_like};
    use crate::error::ProjectError;
    use crate::models::{Priority, TaskStatus};
    use crate::time::CalendarDate;
    use rusqlite::types::Value;

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    #[test]
    fn default_query_hides_archived() {
        let (sql, args) = TaskQuery::new().build().expect("构建 SQL");
        assert!(
            sql.contains("t.archived_at IS NULL"),
            "默认必须排除归档项，实际：{sql}"
        );
        assert!(args.is_empty());
        assert!(sql.contains("ORDER BY t.sort_order ASC, t.id ASC"));
        assert!(TaskQuery::new().archive_scope().is_live());
    }

    #[test]
    fn archive_scope_switches_the_clause() {
        let (archived, _) = TaskQuery::new()
            .archive(ArchiveScope::Archived)
            .build()
            .expect("构建 SQL");
        assert!(archived.contains("t.archived_at IS NOT NULL"));
        assert!(!archived.contains("IS NULL"));
        let (any, _) = TaskQuery::new()
            .archive(ArchiveScope::Any)
            .build()
            .expect("构建 SQL");
        assert!(!any.contains("WHERE"), "Any 不带任何条件：{any}");
    }

    #[test]
    fn today_filter_uses_the_given_day_not_utc() {
        let (sql, args) = TaskQuery::new()
            .due(DueFilter::Today)
            .today(today())
            .build()
            .expect("构建 SQL");
        assert!(sql.contains("t.due_date = ?"));
        assert!(sql.contains("t.status NOT IN ('done', 'cancelled')"));
        assert_eq!(args, vec![Value::Text("2026-10-14".to_owned())]);
    }

    #[test]
    fn this_week_spans_monday_to_sunday() {
        // 2026-10-14 是周三 → 本周是 10-12（周一）到 10-18（周日）。
        let (sql, args) = TaskQuery::new()
            .due(DueFilter::ThisWeek)
            .today(today())
            .build()
            .expect("构建 SQL");
        assert!(sql.contains("t.due_date BETWEEN ? AND ?"));
        assert_eq!(
            args,
            vec![
                Value::Text("2026-10-12".to_owned()),
                Value::Text("2026-10-18".to_owned()),
            ]
        );
    }

    #[test]
    fn overdue_excludes_settled_and_unscheduled() {
        let (sql, args) = TaskQuery::new()
            .due(DueFilter::Overdue)
            .today(today())
            .build()
            .expect("构建 SQL");
        assert!(sql.contains("t.due_date IS NOT NULL AND t.due_date < ?"));
        assert!(sql.contains("t.status NOT IN ('done', 'cancelled')"));
        assert_eq!(args, vec![Value::Text("2026-10-14".to_owned())]);

        let (unscheduled, args) = TaskQuery::new()
            .due(DueFilter::Unscheduled)
            .today(today())
            .build()
            .expect("构建 SQL");
        assert!(unscheduled.contains("t.due_date IS NULL"));
        assert!(args.is_empty(), "未排期不需要绑日期参数");
    }

    #[test]
    fn date_filter_without_today_is_rejected() {
        let error = TaskQuery::new()
            .due(DueFilter::Today)
            .build()
            .expect_err("缺 today 必须报错");
        assert!(
            matches!(error, ProjectError::Invalid(ref detail) if detail.contains("today")),
            "错误应点明缺 today，实际：{error}"
        );
        // 未排期不需要 today（它根本不比日期），所以不该报错。
        assert!(TaskQuery::new().due(DueFilter::Unscheduled).build().is_ok());
    }

    #[test]
    fn empty_collections_mean_unlimited() {
        let (sql, args) = TaskQuery::new().projects([]).statuses([]).build().unwrap();
        assert!(!sql.contains("t.project_id IN"), "空集合不应加条件：{sql}");
        assert!(!sql.contains("t.status IN"), "空集合不应加条件：{sql}");
        assert!(args.is_empty());
    }

    #[test]
    fn multi_value_filters_use_one_placeholder_each() {
        let (sql, args) = TaskQuery::new()
            .projects([1, 2, 3])
            .statuses([TaskStatus::Todo, TaskStatus::Doing])
            .priorities([Priority::P0, Priority::P1])
            .build()
            .expect("构建 SQL");
        assert!(sql.contains("t.project_id IN (?, ?, ?)"));
        assert!(sql.contains("t.status IN (?, ?)"));
        assert!(sql.contains("t.priority IN (?, ?)"));
        assert_eq!(args.len(), 3 + 2 + 2);
        assert_eq!(args[0], Value::Integer(1));
        assert_eq!(args[3], Value::Text("todo".to_owned()));
        assert_eq!(args[5], Value::Integer(0), "P0 落库为 0");
    }

    #[test]
    fn tag_filter_uses_exists_not_join() {
        let (sql, _) = TaskQuery::new().tags([7, 8]).build().expect("构建 SQL");
        assert!(sql.contains("EXISTS (SELECT 1 FROM task_tags tt"), "{sql}");
        assert!(!sql.contains("JOIN"), "JOIN 会让多标签任务重复出行：{sql}");
    }

    #[test]
    fn search_escapes_like_metacharacters() {
        assert_eq!(escape_like("100%"), r"100\%");
        assert_eq!(escape_like("a_b"), r"a\_b");
        assert_eq!(escape_like(r"c:\x"), r"c:\\x");
        let (sql, args) = TaskQuery::new().search("100%").build().expect("构建 SQL");
        assert!(sql.contains(r"ESCAPE '\'"));
        assert_eq!(args[0], Value::Text(r"%100\%%".to_owned()));
        // 纯空白关键词等于没给。
        let (sql, args) = TaskQuery::new().search("   ").build().expect("构建 SQL");
        assert!(!sql.contains("LIKE"), "{sql}");
        assert!(args.is_empty());
    }

    #[test]
    fn top_level_only_and_children_are_mutually_exclusive() {
        let (sql, _) = TaskQuery::new().top_level_only().build().unwrap();
        assert!(sql.contains("t.parent_id IS NULL"));
        // `children_of` 优先：它比「只看顶层」更具体。
        let (sql, args) = TaskQuery::new()
            .top_level_only()
            .children_of(42)
            .build()
            .unwrap();
        assert!(sql.contains("t.parent_id = ?"));
        assert!(!sql.contains("t.parent_id IS NULL"));
        assert_eq!(args, vec![Value::Integer(42)]);
    }

    #[test]
    fn due_sorts_push_null_deadlines_last() {
        for sort in [TaskSort::DueAsc, TaskSort::DueDesc] {
            let (sql, _) = TaskQuery::new().sort(sort).build().unwrap();
            assert!(
                sql.contains("t.due_date IS NULL ASC"),
                "无截止日的应沉到最后：{sql}"
            );
        }
    }

    #[test]
    fn every_sort_has_a_stable_tiebreaker() {
        for sort in TaskSort::ALL {
            let (sql, _) = TaskQuery::new().sort(sort).build().unwrap();
            let order = sql.split("ORDER BY ").nth(1).expect("含 ORDER BY");
            assert!(order.contains("t.id"), "排序缺 id 兜底键：{order}");
        }
    }

    #[test]
    fn limit_appends_last_argument() {
        let (sql, args) = TaskQuery::new()
            .sort(TaskSort::Manual)
            .limit(20)
            .projects([3])
            .build()
            .expect("构建 SQL");
        assert!(sql.ends_with("LIMIT ?"));
        assert_eq!(args.last(), Some(&Value::Integer(20)));
    }
}
