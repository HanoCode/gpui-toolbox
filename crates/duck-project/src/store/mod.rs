//! 存储门面：一个 [`ProjectStore`] 拢住连接与全部读写方法。
//!
//! 按实体分文件（`groups` / `projects` / `tasks` / `tags` / `milestones` / `columns` /
//! `archive`），本文件只放**打开连接**、**跨实体的统计**与**共用的行映射**。
//!
//! # 为什么方法都收 `&self`（而不是 `&mut self`）
//!
//! 多语句写要用事务，而 `rusqlite::Transaction` 通常借 `&mut Connection`；这样一来
//! 每个写方法都变成 `&mut self`，GPUI 侧就得到处拿可变借用。这里统一走
//! `Connection::unchecked_transaction(&self)`（借用检查交给运行时，同一时刻只有一条
//! 事务在跑——本就是单连接单线程的用法），API 全部保持 `&self`，调用方轻松很多。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{ProjectError, Result};
use crate::models::{
    ArchiveRoot, BoardColumn, Group, Milestone, Priority, Project, ProjectStatus, Quadrant, Tag,
    Task, TaskContext, TaskCounts, TaskStatus,
};
use crate::time::CalendarDate;

mod archive;
mod columns;
mod groups;
mod logs;
mod milestones;
mod plans;
mod projects;
mod tags;
mod tasks;

pub use archive::{ArchiveCounts, ArchivedProject, ArchivedTask, ClearScope};
pub use columns::default_column_specs;
pub use projects::ProjectTree;
pub use tags::TagUsage;

/// 项目内核的数据库句柄。
///
/// 不是 `Send`（`rusqlite::Connection` 不跨线程共享）。`duck-app` 侧把它放进
/// `Entity<ProjectsState>`，所有调用在 UI 线程或 `background_executor` 的单一线程上完成。
pub struct ProjectStore {
    conn: Connection,
}

impl ProjectStore {
    /// 打开（或创建）指定路径的库。
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: crate::db::open(path)?,
        })
    }

    /// 打开应用数据目录下的默认库 `{app_data_dir}/projects.db`。
    pub fn open_default() -> Result<Self> {
        Self::open(&default_path())
    }

    /// 内存库（单测与「预览模式」用）。
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            conn: crate::db::open_in_memory()?,
        })
    }

    /// 当前 schema 版本（= 已应用的迁移条数）。
    pub fn schema_version(&self) -> Result<i64> {
        crate::db::schema_version(&self.conn)
    }

    /// 把 WAL 落回主库文件。
    ///
    /// 平时的写入已经在各自的事务里提交（WAL 模式下即时可见），此方法只是把日志
    /// 归并回主文件，供**应用退出**与**备份前**调用。失败不致命，故忽略返回值。
    pub fn flush(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    /// 库文件路径（默认库）。测试与「打开所在文件夹」用。
    #[must_use]
    pub fn default_path() -> PathBuf {
        default_path()
    }

    /// **测试专用**：直接对连接跑一段 SQL。
    ///
    /// 存在的理由只有一个 —— 单测要能伪造「外部工具把库改坏了」的场景（把状态列写成
    /// 野值、把 JSON 数组截半）。读端对这类数据必须容错，而容错分支是常规写口
    /// **造不出来**的，不用这个方法就只能靠注入。生产代码里没有调用点。
    #[cfg(test)]
    pub(crate) fn exec_raw(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }
}

/// `{app_data_dir}/projects.db`。
fn default_path() -> PathBuf {
    duck_platform::app_data_dir().join("projects.db")
}

// ---------------------------------------------------------------------------
// 共用行映射
// ---------------------------------------------------------------------------

/// `tasks` 的列清单，顺序与 [`task_from_row`] 的下标严格对应。
pub(crate) const TASK_COLUMNS: &str = "t.id, t.project_id, t.parent_id, t.title, t.description_md, \
     t.status, t.priority, t.quadrant, t.start_date, t.due_date, t.due_time, \
     t.completed_at, t.progress, t.estimate_hours, t.actual_hours, t.sort_order, \
     t.created_at, t.updated_at, t.archived_at, t.archived_root, \
     t.starred, t.block_reason, t.note, \
     t.git_branch, t.context_files, t.context_sql, t.context_tabs";

/// `projects` 的列清单，顺序与 [`project_from_row`] 的下标严格对应。
pub(crate) const PROJECT_COLUMNS: &str = "p.id, p.group_id, p.name, p.description, p.color, p.icon, \
     p.status, p.start_date, p.due_date, p.sort_order, p.created_at, p.updated_at, \
     p.archived_at";

/// 一行 `tasks` → [`Task`]。`tags` / `subtasks` 留默认值，由调用方补。
pub(crate) fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_id: row.get(1)?,
        parent_id: row.get(2)?,
        title: row.get(3)?,
        description_md: row.get(4)?,
        status: TaskStatus::parse(&row.get::<_, String>(5)?).unwrap_or_default(),
        priority: Priority::from_i64(row.get(6)?),
        quadrant: Quadrant::parse(&row.get::<_, String>(7)?).unwrap_or_default(),
        start_date: date_column(row.get(8)?),
        due_date: date_column(row.get(9)?),
        due_time: row.get(10)?,
        completed_at: row.get(11)?,
        progress: row.get::<_, i64>(12)?.clamp(0, 100) as u8,
        estimate_hours: row.get(13)?,
        actual_hours: row.get(14)?,
        sort_order: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        archived_at: row.get(18)?,
        archived_root: row
            .get::<_, Option<String>>(19)?
            .and_then(|text| ArchiveRoot::parse(&text)),
        starred: row.get::<_, i64>(20)? != 0,
        block_reason: row.get(21)?,
        note: row.get(22)?,
        git_branch: row.get(23)?,
        context: TaskContext {
            files: decode_files(&row.get::<_, String>(24)?),
            sql: row.get(25)?,
            tabs: decode_lines(&row.get::<_, String>(26)?),
        },
        tags: Vec::new(),
        subtasks: crate::models::SubtaskProgress::default(),
    })
}

/// 一行 `projects` → [`Project`]。
pub(crate) fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        group_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        color: row.get(4)?,
        icon: row.get(5)?,
        status: ProjectStatus::parse(&row.get::<_, String>(6)?).unwrap_or_default(),
        start_date: date_column(row.get(7)?),
        due_date: date_column(row.get(8)?),
        sort_order: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        archived_at: row.get(12)?,
    })
}

/// 一行 `project_groups` → [`Group`]。
pub(crate) fn group_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Group> {
    Ok(Group {
        id: row.get(0)?,
        name: row.get(1)?,
        icon: row.get(2)?,
        sort_order: row.get(3)?,
        collapsed: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

/// 一行 `milestones` → [`Milestone`]。
pub(crate) fn milestone_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Milestone> {
    Ok(Milestone {
        id: row.get(0)?,
        task_id: row.get(1)?,
        project_id: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        due_date: date_column(row.get(5)?),
        progress_snapshot: row.get::<_, Option<i64>>(6)?.map(|v| v.clamp(0, 100) as u8),
        reached_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// 一行 `board_columns` → [`BoardColumn`]。`status_key` 不认识时返回 `None`
/// （旧库里的野值不该让整页打不开）。
pub(crate) fn column_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<BoardColumn>> {
    let status_key: String = row.get(2)?;
    let Some(status_key) = TaskStatus::parse(&status_key) else {
        return Ok(None);
    };
    Ok(Some(BoardColumn {
        id: row.get(0)?,
        name: row.get(1)?,
        status_key,
        sort_order: row.get(3)?,
        wip_limit: row.get(4)?,
    }))
}

/// 一行 `tags` → [`Tag`]；`usage_count` 由调用方填。
///
/// 第 4 列固定是 `is_favorite`：所有取标签的查询（`find_tag_by_name` / `hydrate`
/// 的批量 / `tag_usage` 的聚合）都把这一列**放在第 4 位**，聚合查询把 `COUNT`
/// 挪到第 5 位 —— 位置是这份映射的契约。
pub(crate) fn tag_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tag> {
    Ok(Tag {
        id: row.get(0)?,
        name: row.get(1)?,
        color: row.get(2)?,
        is_favorite: row.get::<_, i64>(3)? != 0,
        usage_count: 0,
    })
}

/// 日期列：非法文本按「没设」处理（`CalendarDate::parse` 是严格的，写进去的一定合法，
/// 但外部工具改过的库不该让整个列表崩掉）。
pub(crate) fn date_column(value: Option<String>) -> Option<CalendarDate> {
    value.as_deref().and_then(CalendarDate::parse)
}

// ---------------------------------------------------------------------------
// 落盘的两种「轻量列表」形态（id 列表 / 标签列表）
// ---------------------------------------------------------------------------
//
// 本 crate 不引 serde（内核零序列化依赖的既有口径），`work_logs.linked_task_ids`
// 与 `weekly_plans.task_ids` 在 schema 里就是 JSON 数组文本，读写两端各用一个几行的
// 手写编解码 —— 形态简单到只有 `[1,2,3]` 与 `[]` 两种，手写比拉一个依赖划算。
//
// 两个函数是 `pub(crate)` 的**共用实现**：日志与周计划都走它。分叉出第二份手写解析
// 是最容易发生也最难发现的一类事故（一边能读懂另一边写的东西，只有边界值不同）。

/// `[1,2,3]` / `[]` → id 列表；任何不合法的形态都按空表处理。
pub(crate) fn parse_id_list(wire: &str) -> Vec<i64> {
    let inner = wire.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

/// id 列表 → `[1,2,3]`（与 schema 的 JSON 数组形态一致）。
pub(crate) fn id_list_to_wire(ids: &[i64]) -> String {
    let mut out = String::from("[");
    for (index, id) in ids.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out.push(']');
    out
}

/// 标签落盘形态：**逗号分隔**（`线上事故,复盘`），空列表 = 空串。
///
/// 为什么不用 JSON 数组：标签是用户手写的自由文本，JSON 就得处理引号/反斜杠的转义，
/// 手写编解码容易漏；而逗号分隔只需要一条不变量「标签里不含逗号」—— 那条由
/// [`normalize_tags`] 在**入口**保证（界面根本不让你把逗号敲进去），比在出口转义可靠。
pub(crate) fn join_tags(tags: &[String]) -> String {
    tags.join(",")
}

/// 逗号分隔 → 标签列表（去空白、丢空项；顺序保持）。
pub(crate) fn split_tags(wire: &str) -> Vec<String> {
    wire.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 工作上下文的**逐行**落盘形态（`context_tabs` 用）：一行一个值，空列表 = 空串。
///
/// 「含换行的项直接不要」是写端与读端共同的口径（见 `db.rs` v4 迁移注释）：
/// 链接里不该有换行，与其在库里留下一个回读不出来的字节，不如写时就丢掉。
pub(crate) fn encode_lines(items: &[String]) -> String {
    items.join("\n")
}

/// [`encode_lines`] 的逆：丢空行（尾巴上的换行是合法的落盘形态）。
pub(crate) fn decode_lines(wire: &str) -> Vec<String> {
    wire.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 文件引用的落盘形态（`context_files` 用）：一行一个 `行:列:路径`（[`FileRef::to_line`]）。
pub(crate) fn encode_files(files: &[crate::models::FileRef]) -> String {
    files
        .iter()
        .map(crate::models::FileRef::to_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`encode_files`] 的逆：形状不对的行**直接丢**（「宁可丢也不猜」，路径里带
/// 冒号 / Windows 盘符由 `from_line` 的「前两段必须是数字」判据兜住）。
pub(crate) fn decode_files(wire: &str) -> Vec<crate::models::FileRef> {
    wire.split('\n')
        .filter_map(crate::models::FileRef::from_line)
        .collect()
}

/// 清洗用户输入的标签：丢空项、**丢掉含逗号的项**、按出现顺序去重。
///
/// 入口处调用（写库前），让落盘形态的「标签里没有逗号」这条不变量在源头成立 ——
/// 出口再做转义等于承认入口可以写坏数据。
#[must_use]
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() || tag.contains(',') {
            continue;
        }
        if !out.iter().any(|existing| existing == tag) {
            out.push(tag.to_owned());
        }
    }
    out
}

/// 日期 → 可绑定的列值。
pub(crate) fn date_value(date: Option<CalendarDate>) -> rusqlite::types::Value {
    match date {
        Some(date) => rusqlite::types::Value::Text(date.to_iso()),
        None => rusqlite::types::Value::Null,
    }
}

/// 把查询命中的任务补上标签与子任务计数（两条批量查询，不做 N+1）。
pub(crate) fn hydrate(conn: &Connection, tasks: &mut [Task]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = tasks.iter().map(|task| task.id).collect();
    let placeholders = query_placeholders(ids.len());

    // 标签：一次查出所有命中任务的标签，按 task_id 归并。
    let mut by_task: HashMap<i64, Vec<Tag>> = HashMap::new();
    {
        let sql = format!(
            "SELECT tt.task_id, g.id, g.name, g.color, g.is_favorite FROM task_tags tt \
             JOIN tags g ON g.id = tt.tag_id WHERE tt.task_id IN ({placeholders}) \
             ORDER BY g.name COLLATE NOCASE ASC"
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(ids.iter()))?;
        while let Some(row) = rows.next()? {
            let task_id: i64 = row.get(0)?;
            by_task.entry(task_id).or_default().push(Tag {
                id: row.get(1)?,
                name: row.get(2)?,
                color: row.get(3)?,
                is_favorite: row.get::<_, i64>(4)? != 0,
                usage_count: 0,
            });
        }
    }

    // 子任务计数：一次分组查询。只数未归档的子任务（归档的子任务不该影响 `2/5`）。
    let mut counts: HashMap<i64, crate::models::SubtaskProgress> = HashMap::new();
    {
        let sql = format!(
            "SELECT parent_id, COUNT(*), SUM(status = 'done') FROM tasks \
             WHERE archived_at IS NULL AND parent_id IN ({placeholders}) GROUP BY parent_id"
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(ids.iter()))?;
        while let Some(row) = rows.next()? {
            let parent_id: i64 = row.get(0)?;
            counts.insert(
                parent_id,
                crate::models::SubtaskProgress {
                    total: row.get::<_, i64>(1)?.max(0) as u32,
                    done: row.get::<_, Option<i64>>(2)?.unwrap_or_default().max(0) as u32,
                },
            );
        }
    }

    for task in tasks.iter_mut() {
        task.tags = by_task.remove(&task.id).unwrap_or_default();
        task.subtasks = counts.get(&task.id).copied().unwrap_or_default();
    }
    Ok(())
}

/// `?` 占位符列表（`query::placeholders` 是私有的，这里给 store 用一份）。
fn query_placeholders(count: usize) -> String {
    let mut out = String::with_capacity(count * 3);
    for index in 0..count {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('?');
    }
    out
}

// ---------------------------------------------------------------------------
// 统计（口径都在这里，界面不自己算）
// ---------------------------------------------------------------------------

/// 全局统计（状态栏那条）。**一律排除归档项**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoardStats {
    /// 未归档任务总数。
    pub total: u32,
    /// 待办。
    pub todo: u32,
    /// 进行中。
    pub doing: u32,
    /// 受阻。
    pub blocked: u32,
    /// 已完成。
    pub done: u32,
    /// 已搁置。
    pub deferred: u32,
    /// 已取消。
    pub cancelled: u32,
    /// 逾期（截止日早于 `today` 且未完成 / 未取消）。
    pub overdue: u32,
    /// 今天到期。
    pub due_today: u32,
    /// 未排期（无截止日且未完成 / 未取消）。
    pub unscheduled: u32,
    /// 已归档任务数。
    pub archived_tasks: u32,
    /// 已归档项目数。
    pub archived_projects: u32,
}

impl BoardStats {
    /// 完成率（0–100）。没有任务时返回 0。
    #[must_use]
    pub fn completion_percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        // 用 u64 中转，避免大数字下 `u32 * 100` 溢出。
        ((u64::from(self.done) * 100 + u64::from(self.total) / 2) / u64::from(self.total)) as u32
    }

    /// 未完成数。
    #[must_use]
    pub const fn open(&self) -> u32 {
        self.total
            .saturating_sub(self.done)
            .saturating_sub(self.cancelled)
    }
}

impl ProjectStore {
    /// 全局统计。`today` 走调用方给的本地今天（见 `crate::time` 的模块注释）。
    pub fn board_stats(&self, today: CalendarDate) -> Result<BoardStats> {
        let mut stats = BoardStats::default();
        {
            let mut statement = self.conn.prepare(
                "SELECT status, COUNT(*) FROM tasks WHERE archived_at IS NULL GROUP BY status",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let status = TaskStatus::parse(&row.get::<_, String>(0)?);
                let count = row.get::<_, i64>(1)?.max(0) as u32;
                stats.total += count;
                match status {
                    Some(TaskStatus::Todo) => stats.todo += count,
                    Some(TaskStatus::Doing) => stats.doing += count,
                    Some(TaskStatus::Blocked) => stats.blocked += count,
                    Some(TaskStatus::Done) => stats.done += count,
                    Some(TaskStatus::Deferred) => stats.deferred += count,
                    Some(TaskStatus::Cancelled) => stats.cancelled += count,
                    // 状态列被外部改成了野值：计入总数但不落任何分档。
                    None => {}
                }
            }
        }
        let today_iso = today.to_iso();
        stats.overdue = self.count_tasks(
            "archived_at IS NULL AND due_date IS NOT NULL AND due_date < ? \
             AND status NOT IN ('done', 'cancelled')",
            std::slice::from_ref(&today_iso),
        )?;
        stats.due_today = self.count_tasks(
            "archived_at IS NULL AND due_date = ? AND status NOT IN ('done', 'cancelled')",
            &[today_iso],
        )?;
        stats.unscheduled = self.count_tasks(
            "archived_at IS NULL AND due_date IS NULL AND status NOT IN ('done', 'cancelled')",
            &[],
        )?;
        stats.archived_tasks = self.count_tasks("archived_at IS NOT NULL", &[])?;
        stats.archived_projects = self.count_rows(
            "SELECT COUNT(*) FROM projects WHERE archived_at IS NOT NULL",
            &[],
        )?;
        Ok(stats)
    }

    /// 每个项目的任务计数（左栏徽标用）。**排除归档项**。
    pub fn project_task_counts(&self) -> Result<HashMap<i64, TaskCounts>> {
        let mut out: HashMap<i64, TaskCounts> = HashMap::new();
        let mut statement = self.conn.prepare(
            "SELECT project_id, COUNT(*), SUM(status = 'done') FROM tasks \
             WHERE archived_at IS NULL AND project_id IS NOT NULL GROUP BY project_id",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            out.insert(
                row.get(0)?,
                TaskCounts {
                    total: row.get::<_, i64>(1)?.max(0) as u32,
                    done: row.get::<_, Option<i64>>(2)?.unwrap_or_default().max(0) as u32,
                },
            );
        }
        Ok(out)
    }

    /// 指定项目集合的任务计数（工具条上的「共 N · 完成 M」）。
    pub fn task_counts(&self, project_ids: &[i64]) -> Result<TaskCounts> {
        let (sql, args) = if project_ids.is_empty() {
            (
                "SELECT COUNT(*), SUM(status = 'done') FROM tasks WHERE archived_at IS NULL"
                    .to_owned(),
                Vec::new(),
            )
        } else {
            (
                format!(
                    "SELECT COUNT(*), SUM(status = 'done') FROM tasks WHERE archived_at IS NULL \
                     AND project_id IN ({})",
                    query_placeholders(project_ids.len())
                ),
                project_ids.to_vec(),
            )
        };
        let mut statement = self.conn.prepare(&sql)?;
        let counts = statement.query_row(rusqlite::params_from_iter(args.iter()), |row| {
            Ok(TaskCounts {
                total: row.get::<_, i64>(0)?.max(0) as u32,
                done: row.get::<_, Option<i64>>(1)?.unwrap_or_default().max(0) as u32,
            })
        })?;
        Ok(counts)
    }

    /// 收集箱（`project_id IS NULL`）的任务计数。
    pub fn inbox_task_counts(&self) -> Result<TaskCounts> {
        let mut statement = self.conn.prepare(
            "SELECT COUNT(*), SUM(status = 'done') FROM tasks \
             WHERE archived_at IS NULL AND project_id IS NULL",
        )?;
        let counts = statement.query_row([], |row| {
            Ok(TaskCounts {
                total: row.get::<_, i64>(0)?.max(0) as u32,
                done: row.get::<_, Option<i64>>(1)?.unwrap_or_default().max(0) as u32,
            })
        })?;
        Ok(counts)
    }

    /// 标签使用次数（`task_tags` 现算，不在 `tags` 表里缓存 —— 缓存列会随
    /// 归档 / 删除 / 级联失配，而这张表本来就很小）。
    ///
    /// 计数用 `COUNT(t.id)` 而不是 `COUNT(tt.task_id)`：后者会把「任务已被归档、
    /// 第二个 JOIN 没匹配上」的关系也数进去（`tt` 那个 JOIN 是无条件的）。
    pub fn tag_usage(&self) -> Result<Vec<TagUsage>> {
        // 排序口径放内核（`is_favorite DESC, name`）：界面（左栏标签段、悬浮窗）直接
        // 用这个顺序，不要各自再排一遍 —— 两处排序迟早排出两个结果。
        let mut statement = self.conn.prepare(
            "SELECT g.id, g.name, g.color, g.is_favorite, COUNT(t.id) FROM tags g \
             LEFT JOIN task_tags tt ON tt.tag_id = g.id \
             LEFT JOIN tasks t ON t.id = tt.task_id AND t.archived_at IS NULL \
             GROUP BY g.id, g.name, g.color, g.is_favorite \
             ORDER BY g.is_favorite DESC, g.name COLLATE NOCASE ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(TagUsage {
                tag: Tag {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    color: row.get(2)?,
                    is_favorite: row.get::<_, i64>(3)? != 0,
                    usage_count: 0,
                },
                usage: row.get::<_, i64>(4)?.max(0) as u32,
            });
        }
        Ok(out)
    }

    /// 默认看板列（表里没有行时界面用它）。
    ///
    /// 只给**状态键与 WIP 上限**，不给名称 —— 名称是界面文案，由 `duck-app` 按当前语言取词。
    #[must_use]
    pub fn default_columns() -> Vec<BoardColumn> {
        TaskStatus::DEFAULT_BOARD
            .iter()
            .enumerate()
            .map(|(index, status_key)| BoardColumn {
                id: 0,
                name: None,
                status_key: *status_key,
                sort_order: index as i64,
                wip_limit: None,
            })
            .collect()
    }

    /// `SELECT COUNT(*)` 的通用外壳（统计用）。
    fn count_tasks(&self, where_clause: &str, args: &[String]) -> Result<u32> {
        self.count_rows(
            &format!("SELECT COUNT(*) FROM tasks WHERE {where_clause}"),
            args,
        )
    }

    fn count_rows(&self, sql: &str, args: &[String]) -> Result<u32> {
        let mut statement = self.conn.prepare(sql)?;
        let count: i64 =
            statement.query_row(rusqlite::params_from_iter(args.iter()), |row| row.get(0))?;
        Ok(count.max(0) as u32)
    }
}

/// 供 `store::*` 内部把「找不到行」翻译成统一的错误。
pub(crate) fn not_found(what: &'static str) -> ProjectError {
    ProjectError::NotFound(what)
}

/// 「追加到末尾」时该用的 `sort_order` —— 作用域不同，末尾也不同。
pub(crate) enum OrderScope {
    /// 分组：全局一个序列。
    Groups,
    /// 项目：全局一个序列，**只数未归档的**（归档的项目不该把新项目顶到很后面）。
    Projects,
    /// 任务：同项目同状态一列一个序列（列内拖拽就是改它）。
    TaskColumn {
        /// 所属项目；`None` = 收集箱。
        project_id: Option<i64>,
        /// 看板列对应的状态。
        status: TaskStatus,
    },
}

impl ProjectStore {
    /// 见 [`OrderScope`]。`MAX(...) + 1`；空集合从 0 开始。
    pub(crate) fn next_sort_order(&self, scope: OrderScope) -> Result<i64> {
        let (sql, args): (String, Vec<rusqlite::types::Value>) = match scope {
            OrderScope::Groups => (
                "SELECT IFNULL(MAX(sort_order), -1) + 1 FROM project_groups".to_owned(),
                Vec::new(),
            ),
            OrderScope::Projects => (
                "SELECT IFNULL(MAX(sort_order), -1) + 1 FROM projects WHERE archived_at IS NULL"
                    .to_owned(),
                Vec::new(),
            ),
            OrderScope::TaskColumn { project_id, status } => {
                let mut sql = String::from(
                    "SELECT IFNULL(MAX(sort_order), -1) + 1 FROM tasks \
                     WHERE archived_at IS NULL AND status = ?",
                );
                let mut args = vec![rusqlite::types::Value::Text(status.as_str().to_owned())];
                match project_id {
                    Some(id) => {
                        sql.push_str(" AND project_id = ?");
                        args.push(rusqlite::types::Value::Integer(id));
                    }
                    None => sql.push_str(" AND project_id IS NULL"),
                }
                (sql, args)
            }
        };
        let mut statement = self.conn.prepare(&sql)?;
        let value: i64 = statement.query_row(rusqlite::params_from_iter(args), |row| row.get(0))?;
        Ok(value)
    }
}

/// 入库前的必填校验。
pub(crate) fn require_non_empty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ProjectError::Invalid(format!("{field} must not be empty")));
    }
    Ok(())
}

/// 动态 `SET` 子句的累加器。
///
/// 两种「设置」语义必须区分清楚，混起来就是数据事故：
///
/// - [`Self::push`]：**一定设成这个值**（包括显式的 `NULL`）。
/// - [`Self::push_optional`]：**给就设、不给就不碰**（局部更新 patch 的默认语义）。
///
/// 「设成 NULL」走 `push_optional` 传一个 `Some(None)`，在闭包里 `map_or(Value::Null, ..)` ——
/// 这样「不碰这一列」与「清空这一列」在类型上就分得开（`Option<Option<T>>` 的两层
/// 各代表一件事），不会再需要第三套接口。
pub(crate) struct SetBuilder {
    sets: Vec<String>,
    args: Vec<rusqlite::types::Value>,
}

impl SetBuilder {
    /// 空累加器。
    pub(crate) fn new() -> Self {
        Self {
            sets: Vec::new(),
            args: Vec::new(),
        }
    }

    /// 一定设成 `value`。
    pub(crate) fn push(&mut self, column: &str, value: rusqlite::types::Value) {
        self.sets.push(format!("{column} = ?"));
        self.args.push(value);
    }

    /// 给就设、不给就不碰。
    pub(crate) fn push_optional<T>(
        &mut self,
        column: &str,
        value: Option<T>,
        to_value: impl FnOnce(T) -> rusqlite::types::Value,
    ) {
        if let Some(value) = value {
            self.push(column, to_value(value));
        }
    }

    /// 是否什么都没设（此时不该发 `UPDATE`）。
    pub(crate) fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// 拆出 `"a = ?, b = ?"` 与参数列表。
    pub(crate) fn finish(self) -> (String, Vec<rusqlite::types::Value>) {
        (self.sets.join(", "), self.args)
    }
}

#[cfg(test)]
mod tests {
    use super::{BoardStats, ProjectStore};
    use crate::error::Result;
    use crate::models::{NewProject, NewTask, Priority, TaskStatus};
    use crate::time::CalendarDate;

    fn today() -> CalendarDate {
        CalendarDate::new(2026, 10, 14).expect("合法日期")
    }

    #[test]
    fn open_in_memory_reports_schema_version() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        assert!(store.schema_version()? >= 1);
        store.flush();
        Ok(())
    }

    #[test]
    fn default_path_lives_in_the_app_data_dir() -> Result<()> {
        let path = ProjectStore::default_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("projects.db")
        );
        assert!(path.starts_with(duck_platform::app_data_dir()));
        Ok(())
    }

    #[test]
    fn board_stats_exclude_archived_and_count_overdue() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let overdue = store.create_task(
            NewTask::new("overdue")
                .project(project)
                .dates(None, Some(today().add_days(-1))),
        )?;
        store.create_task(
            NewTask::new("today")
                .project(project)
                .dates(None, Some(today())),
        )?;
        store.create_task(NewTask::new("someday").project(project))?;
        store.create_task(
            NewTask::new("done")
                .project(project)
                .status(TaskStatus::Done)
                .dates(None, Some(today().add_days(-3))),
        )?;
        let archived = store.create_task(NewTask::new("archived").project(project))?;
        store.archive_task(archived)?;
        store.archive_task(overdue)?;

        let stats = store.board_stats(today())?;
        assert_eq!(stats.total, 3, "归档项不进总数");
        assert_eq!(stats.done, 1);
        assert_eq!(stats.overdue, 0, "归档的逾期任务不再计入逾期");
        assert_eq!(stats.due_today, 1);
        assert_eq!(stats.unscheduled, 1);
        assert_eq!(stats.archived_tasks, 2);
        assert_eq!(stats.archived_projects, 0);
        assert_eq!(stats.completion_percent(), 33);
        assert_eq!(stats.open(), 2);
        Ok(())
    }

    #[test]
    fn board_stats_with_no_tasks_is_all_zero() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let stats = store.board_stats(today())?;
        assert_eq!(stats, BoardStats::default());
        assert_eq!(stats.completion_percent(), 0, "空集不该除零");
        Ok(())
    }

    #[test]
    fn project_task_counts_are_live_only() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let alpha = store.create_project(NewProject::new("Alpha"))?;
        let beta = store.create_project(NewProject::new("Beta"))?;
        for index in 0..3 {
            store.create_task(NewTask::new(format!("a{index}")).project(alpha))?;
        }
        store.create_task(NewTask::new("b").project(beta).status(TaskStatus::Done))?;
        let archived = store.create_task(NewTask::new("c").project(beta))?;
        store.archive_task(archived)?;

        let counts = store.project_task_counts()?;
        assert_eq!(counts.get(&alpha).map(|c| c.total), Some(3));
        assert_eq!(counts.get(&beta).map(|c| c.total), Some(1));
        assert_eq!(counts.get(&beta).map(|c| c.done), Some(1));
        assert_eq!(store.task_counts(&[alpha, beta])?.total, 4);
        assert_eq!(store.task_counts(&[])?.total, 4, "空集合 = 全局");
        Ok(())
    }

    #[test]
    fn inbox_counts_untracked_tasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        store.create_task(NewTask::new("inbox-1"))?;
        store.create_task(NewTask::new("inbox-2").priority(Priority::P0))?;
        store.create_task(NewTask::new("in-project").project(project))?;
        assert_eq!(store.inbox_task_counts()?.total, 2);
        assert_eq!(store.task_counts(&[])?.total, 3);
        Ok(())
    }

    #[test]
    fn default_columns_carry_no_names() {
        let columns = ProjectStore::default_columns();
        assert_eq!(columns.len(), 4);
        assert!(
            columns.iter().all(|column| column.name.is_none()),
            "内核不含界面文案：默认列名由 duck-app 取词"
        );
        let keys: Vec<&str> = columns.iter().map(|c| c.status_key.as_str()).collect();
        assert_eq!(keys, ["todo", "doing", "blocked", "done"]);
    }
}
