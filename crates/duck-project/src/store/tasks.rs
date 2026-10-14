//! 任务（含一层子任务）。
//!
//! 三条被反复用到的规则在本文件里落地，改动前先读一遍：
//!
//! 1. **`completed_at` 由状态唯一决定**：进入 `done` 写一次（已经有了就不重写，
//!    所以反复把卡片拖进「已完成」不会刷新完成时刻），离开 `done` 清空。
//! 2. **`sort_order` 是「同项目同状态一列」内的序号**，不是全局序号 —— 看板列内
//!    拖拽改的就是它，跨列拖拽同时改 `status` 与 `sort_order`。
//! 3. **子任务只有一层**：`parent_id` 指向的父任务自己不能再有 `parent_id`，
//!    否则卡片上的 `2/5` 会算不清、左栏计数也会重复算。

use rusqlite::types::Value;

use crate::error::{ProjectError, Result};
use crate::models::{NewTask, Task, TaskPatch, TaskStatus};
use crate::query::TaskQuery;
use crate::store::{
    OrderScope, ProjectStore, SetBuilder, TASK_COLUMNS, hydrate, not_found, require_non_empty,
    task_from_row,
};

impl ProjectStore {
    /// 按查询取任务（含标签与子任务计数）。
    pub fn list_tasks(&self, query: &TaskQuery) -> Result<Vec<Task>> {
        let (tail, args) = query.build()?;
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t{tail}");
        let mut tasks = {
            let mut statement = self.conn.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(args.iter()))?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(task_from_row(row)?);
            }
            out
        };
        hydrate(&self.conn, &mut tasks)?;
        Ok(tasks)
    }

    /// 单个任务（含已归档的；「已归档」看 `archived_at`）。
    pub fn get_task(&self, id: i64) -> Result<Option<Task>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.id = ?1");
        let mut task = {
            let mut statement = self.conn.prepare(&sql)?;
            let mut rows = statement.query([id])?;
            match rows.next()? {
                Some(row) => Some(task_from_row(row)?),
                None => None,
            }
        };
        if let Some(task) = &mut task {
            // 单条也要补派生字段：详情面板要用 `2/5` 与标签片。
            hydrate(&self.conn, std::slice::from_mut(task))?;
        }
        Ok(task)
    }

    /// 某个父任务的子任务（未归档，按手动顺序）。
    pub fn list_subtasks(&self, parent_id: i64) -> Result<Vec<Task>> {
        self.list_tasks(&TaskQuery::new().children_of(parent_id))
    }

    /// 新建任务，返回新 id。标题必填；`sort_order` 缺省时追加到同项目同列的末尾。
    pub fn create_task(&self, new: NewTask) -> Result<i64> {
        let title = new.title.trim();
        require_non_empty("task title", title)?;
        self.check_parent(new.parent_id, None)?;
        if let Some(project_id) = new.project_id
            && self.get_project(project_id)?.is_none()
        {
            return Err(not_found("project"));
        }

        let sort_order = match new.sort_order {
            Some(value) => value,
            None => self.next_sort_order(OrderScope::TaskColumn {
                project_id: new.project_id,
                status: new.status,
            })?,
        };
        let now = crate::time::now_seconds();
        // 建的时候就是「已完成」也要有完成时刻，否则报表里的完成趋势会漏掉它。
        let completed_at = (new.status == TaskStatus::Done).then_some(now);

        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO tasks(project_id, parent_id, title, description_md, status, priority, \
             quadrant, start_date, due_date, due_time, completed_at, progress, estimate_hours, \
             actual_hours, sort_order, created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
            rusqlite::params![
                new.project_id,
                new.parent_id,
                title,
                new.description_md,
                new.status.as_str(),
                new.priority.as_i64(),
                new.quadrant.as_str(),
                new.start_date.map(|date| date.to_iso()),
                new.due_date.map(|date| date.to_iso()),
                new.due_time,
                completed_at,
                i64::from(new.progress),
                new.estimate_hours,
                new.actual_hours,
                sort_order,
                now,
            ],
        )?;
        let id = transaction.last_insert_rowid();
        replace_tags(&transaction, id, &new.tag_ids)?;
        transaction.commit()?;
        Ok(id)
    }

    /// 局部更新。patch 全空时不发语句（但仍校验对象存在）。
    pub fn update_task(&self, id: i64, patch: TaskPatch) -> Result<()> {
        let existing = self.get_task(id)?.ok_or_else(|| not_found("task"))?;
        // 先整体取出各字段：后面 `set.push_optional` 会逐个把它们移走，
        // 而标签要留到最后和状态改动放进同一个事务。
        let TaskPatch {
            title,
            project_id,
            parent_id,
            description_md,
            status,
            priority,
            quadrant,
            start_date,
            due_date,
            due_time,
            progress,
            estimate_hours,
            actual_hours,
            sort_order,
            tag_ids,
        } = patch;

        if let Some(title) = &title {
            require_non_empty("task title", title)?;
        }
        self.check_parent(parent_id.flatten(), Some(id))?;
        if let Some(project_id) = project_id.flatten()
            && self.get_project(project_id)?.is_none()
        {
            return Err(not_found("project"));
        }
        if let Some(status) = status
            && !existing.status.can_transition_to(status)
        {
            return Err(ProjectError::Invalid(format!(
                "illegal task transition {} -> {}",
                existing.status.as_str(),
                status.as_str()
            )));
        }

        let now = crate::time::now_seconds();
        let mut set = SetBuilder::new();
        set.push_optional("title", title, |title| Value::Text(title.trim().to_owned()));
        set.push_optional("project_id", project_id, |project| {
            project.map_or(Value::Null, Value::Integer)
        });
        set.push_optional("parent_id", parent_id, |parent| {
            parent.map_or(Value::Null, Value::Integer)
        });
        set.push_optional("description_md", description_md, Value::Text);
        set.push_optional("priority", priority, |priority| {
            Value::Integer(priority.as_i64())
        });
        set.push_optional("quadrant", quadrant, |quadrant| {
            Value::Text(quadrant.as_str().to_owned())
        });
        set.push_optional("start_date", start_date, crate::store::date_value);
        set.push_optional("due_date", due_date, crate::store::date_value);
        set.push_optional("due_time", due_time, |due_time| {
            due_time.map_or(Value::Null, Value::Text)
        });
        set.push_optional("progress", progress, |progress| {
            Value::Integer(i64::from(progress))
        });
        set.push_optional("estimate_hours", estimate_hours, |hours| {
            hours.map_or(Value::Null, Value::Real)
        });
        set.push_optional("actual_hours", actual_hours, |hours| {
            hours.map_or(Value::Null, Value::Real)
        });
        set.push_optional("sort_order", sort_order, Value::Integer);

        // 状态不走 `push_optional`：它还牵着 `completed_at` 与 `progress`，
        // 三者必须在同一条语句里落地 —— 否则「拖到已完成」会有一瞬间
        // 「状态是 done 但没有完成时刻」的中间态，列表刷新时就看到了。
        if let Some(status) = status {
            set.push("status", Value::Text(status.as_str().to_owned()));
            if status == TaskStatus::Done {
                set.push(
                    "completed_at",
                    Value::Integer(existing.completed_at.unwrap_or(now)),
                );
                set.push("progress", Value::Integer(100));
            } else if existing.status == TaskStatus::Done {
                // 只有「离开已完成」才清空；本来就不是 done 就别碰它。
                set.push("completed_at", Value::Null);
            }
        }

        let assignments = if set.is_empty() {
            None
        } else {
            set.push("updated_at", Value::Integer(now));
            Some(set.finish())
        };

        let transaction = self.conn.unchecked_transaction()?;
        if let Some((assignments, mut args)) = assignments {
            args.push(Value::Integer(id));
            let statement = format!("UPDATE tasks SET {assignments} WHERE id = ?");
            let affected =
                transaction.execute(&statement, rusqlite::params_from_iter(args.iter()))?;
            if affected == 0 {
                return Err(not_found("task"));
            }
        }
        if let Some(tag_ids) = tag_ids {
            replace_tags(&transaction, id, &tag_ids)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 改状态（看板拖拽的落点、详情面板的状态按钮都走它）。
    pub fn set_task_status(&self, id: i64, status: TaskStatus) -> Result<()> {
        self.update_task(id, TaskPatch::new().status(status))
    }

    /// 跨列拖拽：改状态 + 落到指定位置。单事务。
    pub fn move_task(&self, id: i64, status: TaskStatus, sort_order: i64) -> Result<()> {
        let existing = self.get_task(id)?.ok_or_else(|| not_found("task"))?;
        if !existing.status.can_transition_to(status) {
            return Err(ProjectError::Invalid(format!(
                "illegal task transition {} -> {}",
                existing.status.as_str(),
                status.as_str()
            )));
        }
        let project_id = existing.project_id;
        let now = crate::time::now_seconds();
        // 进入 / 离开「已完成」时同步 `completed_at` 与 `progress`（同 `update_task`）。
        let (completed_at, progress) = if status == TaskStatus::Done {
            (
                Value::Integer(existing.completed_at.unwrap_or(now)),
                Value::Integer(100),
            )
        } else {
            (Value::Null, Value::Integer(i64::from(existing.progress)))
        };

        let transaction = self.conn.unchecked_transaction()?;
        // 先把目标列里该位置及其后的整体后移一格，再把这张卡插进去 ——
        // 直接按给定值写会与目标列已有的序号撞车，列内顺序就随 SQLite 心情了。
        transaction.execute(
            "UPDATE tasks SET sort_order = sort_order + 1 \
             WHERE archived_at IS NULL AND status = ?1 AND sort_order >= ?2 \
             AND id <> ?3 AND project_id IS ?4",
            rusqlite::params![status.as_str(), sort_order, id, project_id],
        )?;
        transaction.execute(
            "UPDATE tasks SET status = ?1, sort_order = ?2, updated_at = ?3, \
             completed_at = ?4, progress = ?5 WHERE id = ?6",
            rusqlite::params![status.as_str(), sort_order, now, completed_at, progress, id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 列内重排（数组下标即新的 `sort_order`）。单事务。
    pub fn reorder_tasks(&self, ids: &[i64]) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement =
                transaction.prepare("UPDATE tasks SET sort_order = ?1 WHERE id = ?2")?;
            for (index, id) in ids.iter().enumerate() {
                statement.execute(rusqlite::params![index as i64, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 整体替换标签集合。单事务（先删后插，不会出现「删了没插上」）。
    pub fn set_task_tags(&self, id: i64, tag_ids: &[i64]) -> Result<()> {
        if self.get_task(id)?.is_none() {
            return Err(not_found("task"));
        }
        let transaction = self.conn.unchecked_transaction()?;
        replace_tags(&transaction, id, tag_ids)?;
        transaction.commit()?;
        Ok(())
    }

    /// 物理删除任务（连同子任务与里程碑，走 `ON DELETE CASCADE`）。
    ///
    /// 界面上只在「归档中心」的彻底删除里调，且必须二次确认。
    pub fn delete_task(&self, id: i64) -> Result<()> {
        let affected = self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        if affected == 0 {
            return Err(not_found("task"));
        }
        Ok(())
    }

    /// 各状态下的顶层任务数（看板列头计数用）。**排除归档与子任务**。
    pub fn task_counts_by_status(
        &self,
        project_ids: &[i64],
    ) -> Result<std::collections::HashMap<TaskStatus, u32>> {
        let mut sql = String::from(
            "SELECT status, COUNT(*) FROM tasks WHERE archived_at IS NULL AND parent_id IS NULL",
        );
        if !project_ids.is_empty() {
            sql.push_str(&format!(
                " AND project_id IN ({})",
                crate::store::query_placeholders(project_ids.len())
            ));
        }
        sql.push_str(" GROUP BY status");
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(project_ids.iter()))?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            if let Some(status) = TaskStatus::parse(&row.get::<_, String>(0)?) {
                out.insert(status, row.get::<_, i64>(1)?.max(0) as u32);
            }
        }
        Ok(out)
    }

    /// 父任务校验：不能指向自己，也不能挂在一个已经有父任务的子任务下（只许一层）。
    fn check_parent(&self, parent_id: Option<i64>, child_id: Option<i64>) -> Result<()> {
        let Some(parent_id) = parent_id else {
            return Ok(());
        };
        if child_id == Some(parent_id) {
            return Err(ProjectError::Invalid(
                "a task cannot be its own parent".to_owned(),
            ));
        }
        let parent = self.get_task(parent_id)?.ok_or_else(|| not_found("task"))?;
        if parent.parent_id.is_some() {
            return Err(ProjectError::Invalid(
                "subtasks are limited to a single level".to_owned(),
            ));
        }
        Ok(())
    }
}

/// 替换某任务的标签集合；忽略不存在的 tag id（界面上的标签可能在别处被删了）。
fn replace_tags(
    transaction: &rusqlite::Transaction<'_>,
    task_id: i64,
    tag_ids: &[i64],
) -> Result<()> {
    transaction.execute("DELETE FROM task_tags WHERE task_id = ?1", [task_id])?;
    if tag_ids.is_empty() {
        return Ok(());
    }
    let mut statement = transaction.prepare(
        "INSERT OR IGNORE INTO task_tags(task_id, tag_id) SELECT ?1, id FROM tags WHERE id = ?2",
    )?;
    for tag_id in tag_ids {
        statement.execute(rusqlite::params![task_id, tag_id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::error::{ProjectError, Result};
    use crate::models::{NewProject, NewTag, NewTask, Priority, Quadrant, TaskPatch, TaskStatus};
    use crate::query::{ArchiveScope, DueFilter, TaskQuery, TaskSort};
    use crate::time::CalendarDate;

    fn date(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("合法日期")
    }

    fn today() -> CalendarDate {
        date(2026, 10, 14)
    }

    #[test]
    fn create_read_update_round_trip() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let id = store.create_task(
            NewTask::new("  实现连接池复用  ")
                .project(project)
                .description("## 要点\n- 复用")
                .priority(Priority::P0)
                .quadrant(Quadrant::Q1)
                .dates(Some(date(2026, 10, 10)), Some(date(2026, 10, 18)))
                .due_time("18:00")
                .progress(45)
                .hours(Some(8.0), Some(3.5)),
        )?;

        let task = store.get_task(id)?.expect("刚建的任务");
        assert_eq!(task.title, "实现连接池复用", "标题应去首尾空白");
        assert_eq!(task.project_id, Some(project));
        assert_eq!(task.parent_id, None);
        assert_eq!(task.status, TaskStatus::Todo);
        assert_eq!(task.priority, Priority::P0);
        assert_eq!(task.quadrant, Quadrant::Q1);
        assert_eq!(task.due_date, Some(date(2026, 10, 18)));
        assert_eq!(task.due_time.as_deref(), Some("18:00"));
        assert_eq!(task.progress, 45);
        assert_eq!(task.estimate_hours, Some(8.0));
        assert_eq!(task.actual_hours, Some(3.5));
        assert_eq!(task.completed_at, None);
        assert!(!task.is_archived());
        assert!(!task.is_overdue(today()), "10-18 还没到期");

        store.update_task(
            id,
            TaskPatch::new()
                .title("实现连接池")
                .priority(Priority::P1)
                .progress(60)
                .due_time(None),
        )?;
        let task = store.get_task(id)?.expect("更新后仍存在");
        assert_eq!(task.title, "实现连接池");
        assert_eq!(task.priority, Priority::P1);
        assert_eq!(task.progress, 60);
        assert_eq!(task.due_time, None, "due_time 传 None 应写 NULL");
        Ok(())
    }

    #[test]
    fn overdue_depends_on_today_and_status() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let yesterday =
            store.create_task(NewTask::new("y").dates(None, Some(date(2026, 10, 13))))?;
        let done = store.create_task(
            NewTask::new("d")
                .status(TaskStatus::Done)
                .dates(None, Some(date(2026, 10, 1))),
        )?;
        let no_due = store.create_task(NewTask::new("n"))?;
        assert!(store.get_task(yesterday)?.unwrap().is_overdue(today()));
        assert!(
            !store.get_task(done)?.unwrap().is_overdue(today()),
            "完成的不算逾期"
        );
        assert!(
            !store.get_task(no_due)?.unwrap().is_overdue(today()),
            "没截止日不算逾期"
        );
        Ok(())
    }

    #[test]
    fn status_transitions_maintain_completed_at_and_progress() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_task(NewTask::new("t").progress(30))?;
        assert_eq!(store.get_task(id)?.unwrap().completed_at, None);

        // 待办 → 已完成：允许（看板拖拽不该有禁区），并写上完成时刻、进度补到 100。
        store.set_task_status(id, TaskStatus::Done)?;
        let task = store.get_task(id)?.unwrap();
        assert!(task.completed_at.is_some());
        assert_eq!(task.progress, 100);
        let first_completed = task.completed_at;

        // 重复标记完成不刷新完成时刻。
        store.set_task_status(id, TaskStatus::Done)?;
        assert_eq!(store.get_task(id)?.unwrap().completed_at, first_completed);

        // 离开已完成：清空完成时刻，进度保持（由用户自己调）。
        store.set_task_status(id, TaskStatus::Doing)?;
        let task = store.get_task(id)?.unwrap();
        assert_eq!(task.completed_at, None);
        assert_eq!(task.progress, 100);
        Ok(())
    }

    #[test]
    fn cancelled_must_return_to_todo_first() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_task(NewTask::new("t"))?;
        store.set_task_status(id, TaskStatus::Cancelled)?;
        let error = store
            .set_task_status(id, TaskStatus::Done)
            .expect_err("取消是终态，不能直接变完成");
        assert!(
            matches!(error, ProjectError::Invalid(ref detail) if detail.contains("cancelled -> done")),
            "错误应点明非法迁移，实际：{error}"
        );
        store.set_task_status(id, TaskStatus::Todo)?;
        store.set_task_status(id, TaskStatus::Done)?;
        assert_eq!(store.get_task(id)?.unwrap().status, TaskStatus::Done);
        Ok(())
    }

    #[test]
    fn creating_a_done_task_gets_a_completion_time() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_task(NewTask::new("t").status(TaskStatus::Done))?;
        assert!(store.get_task(id)?.unwrap().completed_at.is_some());
        Ok(())
    }

    #[test]
    fn blank_title_is_rejected() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        assert!(matches!(
            store.create_task(NewTask::new("  ")),
            Err(ProjectError::Invalid(_))
        ));
        let id = store.create_task(NewTask::new("t"))?;
        assert!(matches!(
            store.update_task(id, TaskPatch::new().title(" ")),
            Err(ProjectError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn subtasks_are_limited_to_one_level() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let parent = store.create_task(NewTask::new("父"))?;
        let child = store.create_task(NewTask::new("子").parent(parent))?;
        let error = store
            .create_task(NewTask::new("孙").parent(child))
            .expect_err("只允许一层子任务");
        assert!(matches!(error, ProjectError::Invalid(_)));
        assert!(matches!(
            store.create_task(NewTask::new("自环").parent(9_999)),
            Err(ProjectError::NotFound("task"))
        ));
        let alone = store.create_task(NewTask::new("x"))?;
        assert!(matches!(
            store.update_task(alone, TaskPatch::new().parent(alone)),
            Err(ProjectError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn subtask_progress_is_counted_live_only() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let parent = store.create_task(NewTask::new("父"))?;
        store.create_task(NewTask::new("a").parent(parent).status(TaskStatus::Done))?;
        store.create_task(NewTask::new("b").parent(parent))?;
        let gone = store.create_task(NewTask::new("c").parent(parent))?;
        store.archive_task(gone)?;

        let task = store.get_task(parent)?.unwrap();
        assert_eq!(task.subtasks.total, 2, "归档的子任务不计入 2/5");
        assert_eq!(task.subtasks.done, 1);
        assert_eq!(store.list_subtasks(parent)?.len(), 2);
        Ok(())
    }

    #[test]
    fn sort_order_appends_within_the_same_column() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("A"))?;
        let a = store.create_task(NewTask::new("a").project(project))?;
        let b = store.create_task(NewTask::new("b").project(project))?;
        let c = store.create_task(NewTask::new("c").project(project).status(TaskStatus::Done))?;
        // 同列内递增。
        assert!(store.get_task(a)?.unwrap().sort_order < store.get_task(b)?.unwrap().sort_order);
        // 不同列各自从 0 开始。
        assert_eq!(store.get_task(c)?.unwrap().sort_order, 0);
        Ok(())
    }

    #[test]
    fn list_tasks_honours_archive_scope_and_due_filters() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("A"))?;
        // 逾期的那条故意放在**上一周**（今天 10-14 周三，本周是 10-12 ~ 10-18）——
        // 「本周」口径是「截止日落在本周」，逾期但仍在周内的会被它包含，这是设计口径。
        let overdue = store.create_task(
            NewTask::new("overdue")
                .project(project)
                .dates(None, Some(today().add_days(-5))),
        )?;
        store.create_task(
            NewTask::new("today")
                .project(project)
                .dates(None, Some(today())),
        )?;
        store.create_task(
            NewTask::new("this-week")
                .project(project)
                .dates(None, Some(today().add_days(2))),
        )?;
        store.create_task(
            NewTask::new("next-week")
                .project(project)
                .dates(None, Some(today().add_days(9))),
        )?;
        store.create_task(NewTask::new("unscheduled").project(project))?;
        let archived = store.create_task(NewTask::new("archived").project(project))?;
        store.archive_task(archived)?;

        let live = store.list_tasks(&TaskQuery::new())?;
        assert_eq!(live.len(), 5, "默认查询不含归档项");
        assert_eq!(
            store
                .list_tasks(&TaskQuery::new().archive(ArchiveScope::Archived))?
                .len(),
            1
        );

        let overdue_hits =
            store.list_tasks(&TaskQuery::new().due(DueFilter::Overdue).today(today()))?;
        assert_eq!(overdue_hits.len(), 1);
        assert_eq!(overdue_hits[0].id, overdue);
        assert_eq!(
            store
                .list_tasks(&TaskQuery::new().due(DueFilter::Today).today(today()))?
                .len(),
            1
        );
        assert_eq!(
            store
                .list_tasks(&TaskQuery::new().due(DueFilter::ThisWeek).today(today()))?
                .len(),
            2,
            "本周 = 周三到周日：今天 + 两天后各一条（上一周那条不算）"
        );
        assert_eq!(
            store
                .list_tasks(&TaskQuery::new().due(DueFilter::Unscheduled).today(today()))?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn top_level_only_hides_children() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let parent = store.create_task(NewTask::new("父"))?;
        store.create_task(NewTask::new("子").parent(parent))?;
        assert_eq!(store.list_tasks(&TaskQuery::new())?.len(), 2);
        assert_eq!(
            store.list_tasks(&TaskQuery::new().top_level_only())?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn search_matches_title_and_description_only() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let tag = store.create_tag(NewTag::new("后端"))?;
        store.create_task(NewTask::new("修复登录").description("超时问题"))?;
        store.create_task(NewTask::new("随便").tags([tag]))?;
        let hits = store.list_tasks(&TaskQuery::new().search("登录"))?;
        assert_eq!(hits.len(), 1);
        assert_eq!(store.list_tasks(&TaskQuery::new().search("超时"))?.len(), 1);
        assert_eq!(
            store
                .list_tasks(&TaskQuery::new().search("不存在的词"))?
                .len(),
            0
        );
        Ok(())
    }

    #[test]
    fn tags_are_replaced_atomically_and_filter_works() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let front = store.create_tag(NewTag::new("前端"))?;
        let back = store.create_tag(NewTag::new("后端"))?;
        let id = store.create_task(NewTask::new("t").tags([front, back]))?;
        let task = store.get_task(id)?.unwrap();
        assert_eq!(task.tags.len(), 2);
        // 中文按 UTF-8 字节序（= 码点序）：「前端」在「后端」之前。
        assert_eq!(task.tags[0].name, "前端");

        store.set_task_tags(id, &[back])?;
        assert_eq!(store.get_task(id)?.unwrap().tags.len(), 1);
        // 不存在的 tag id 被忽略，不报错。
        store.set_task_tags(id, &[front, 9_999])?;
        assert_eq!(store.get_task(id)?.unwrap().tags.len(), 1);
        assert_eq!(store.list_tasks(&TaskQuery::new().tags([front]))?.len(), 1);
        assert_eq!(
            store.list_tasks(&TaskQuery::new().tags([back]))?.len(),
            0,
            "整体替换：上一轮的标签应已摘掉"
        );
        Ok(())
    }

    #[test]
    fn empty_patch_keeps_updated_at_and_checks_existence() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_task(NewTask::new("t"))?;
        let before = store.get_task(id)?.unwrap();
        store.update_task(id, TaskPatch::new())?;
        assert_eq!(store.get_task(id)?.unwrap().updated_at, before.updated_at);
        assert!(matches!(
            store.update_task(9_999, TaskPatch::new()),
            Err(ProjectError::NotFound("task"))
        ));
        Ok(())
    }

    #[test]
    fn moving_a_task_shifts_the_target_column() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("A"))?;
        let a = store.create_task(NewTask::new("a").project(project).status(TaskStatus::Doing))?;
        let b = store.create_task(NewTask::new("b").project(project).status(TaskStatus::Doing))?;
        let moving = store.create_task(NewTask::new("m").project(project))?;

        // 插到 doing 列的第 0 位。
        store.move_task(moving, TaskStatus::Doing, 0)?;
        let moved = store.get_task(moving)?.unwrap();
        assert_eq!(moved.status, TaskStatus::Doing);
        assert_eq!(moved.sort_order, 0);
        assert!(
            store.get_task(a)?.unwrap().sort_order >= 1,
            "原第 0 位应被挤后"
        );
        assert!(store.get_task(b)?.unwrap().sort_order >= 2);

        // 列内顺序按 sort_order 读出。
        let column = store.list_tasks(
            &TaskQuery::new()
                .statuses([TaskStatus::Doing])
                .sort(TaskSort::Manual),
        )?;
        assert_eq!(column[0].id, moving);
        assert_eq!(column.len(), 3);
        Ok(())
    }

    #[test]
    fn reorder_tasks_sets_manual_order() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let a = store.create_task(NewTask::new("a"))?;
        let b = store.create_task(NewTask::new("b"))?;
        let c = store.create_task(NewTask::new("c"))?;
        store.reorder_tasks(&[c, a, b])?;
        let ids: Vec<i64> = store
            .list_tasks(&TaskQuery::new())?
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, [c, a, b]);
        Ok(())
    }

    #[test]
    fn deleting_a_parent_cascades_to_children() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let parent = store.create_task(NewTask::new("父"))?;
        let child = store.create_task(NewTask::new("子").parent(parent))?;
        store.delete_task(parent)?;
        assert!(store.get_task(child)?.is_none(), "删父任务要级联删子任务");
        assert!(matches!(
            store.delete_task(parent),
            Err(ProjectError::NotFound("task"))
        ));
        Ok(())
    }

    #[test]
    fn counts_by_status_ignore_children_and_archived() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("A"))?;
        let parent = store.create_task(NewTask::new("p").project(project))?;
        store.create_task(NewTask::new("c").parent(parent))?;
        store.create_task(NewTask::new("d").project(project).status(TaskStatus::Doing))?;
        let archived = store.create_task(NewTask::new("x").project(project))?;
        store.archive_task(archived)?;

        let counts = store.task_counts_by_status(&[project])?;
        assert_eq!(counts.get(&TaskStatus::Todo), Some(&1));
        assert_eq!(counts.get(&TaskStatus::Doing), Some(&1));
        assert_eq!(counts.values().sum::<u32>(), 2);
        assert_eq!(store.task_counts_by_status(&[])?.values().sum::<u32>(), 2);
        Ok(())
    }
}
