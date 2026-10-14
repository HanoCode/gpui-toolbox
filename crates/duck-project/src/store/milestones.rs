//! 里程碑（挂在任务上，或直接挂在项目上）。
//!
//! 与 `duck-db` 的「进度快照」约定一致（设计文档 §1 的照抄项）：创建里程碑时若它的
//! `progress_snapshot` **高于**任务当前进度，就把任务进度同步推进到快照值；
//! 达成时同理。反向不成立 —— 里程碑不会因为任务进度回退而降级，它已经达成就达成了。

use rusqlite::types::Value;

use crate::error::{ProjectError, Result};
use crate::models::{Milestone, NewMilestone};
use crate::store::{ProjectStore, milestone_from_row, not_found, require_non_empty};

impl ProjectStore {
    /// 某个任务的里程碑（按创建时间）。
    pub fn list_task_milestones(&self, task_id: i64) -> Result<Vec<Milestone>> {
        self.query_milestones("task_id = ?1", task_id)
    }

    /// 某个项目的项目级里程碑（`task_id IS NULL` 的那些）。
    pub fn list_project_milestones(&self, project_id: i64) -> Result<Vec<Milestone>> {
        self.query_milestones("project_id = ?1 AND task_id IS NULL", project_id)
    }

    /// 新建里程碑，返回新 id。
    ///
    /// 任务级里程碑若带 `progress_snapshot` 且高于任务当前进度，**同一事务**里把任务
    /// 进度推进上去 —— 分两条语句写的话，中间崩一次就会出现「里程碑在、进度没动」。
    pub fn create_milestone(&self, new: NewMilestone) -> Result<i64> {
        let title = new.title.trim();
        require_non_empty("milestone title", title)?;
        if new.task_id.is_none() && new.project_id.is_none() {
            return Err(ProjectError::Invalid(
                "a milestone must belong to a task or a project".to_owned(),
            ));
        }
        if let Some(task_id) = new.task_id
            && self.get_task(task_id)?.is_none()
        {
            return Err(not_found("task"));
        }
        if let Some(project_id) = new.project_id
            && self.get_project(project_id)?.is_none()
        {
            return Err(not_found("project"));
        }

        let now = crate::time::now_seconds();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO milestones(task_id, project_id, title, description, due_date, \
             progress_snapshot, created_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                new.task_id,
                new.project_id,
                title,
                new.description,
                new.due_date.map(|date| date.to_iso()),
                new.progress_snapshot.map(i64::from),
                now,
            ],
        )?;
        let id = transaction.last_insert_rowid();
        if let (Some(task_id), Some(snapshot)) = (new.task_id, new.progress_snapshot) {
            advance_progress(&transaction, task_id, snapshot, now)?;
        }
        transaction.commit()?;
        Ok(id)
    }

    /// 标记达成。任务级里程碑会顺带把任务进度推进到快照值（若快照更高）。
    pub fn reach_milestone(&self, id: i64) -> Result<()> {
        let milestone = self
            .get_milestone(id)?
            .ok_or_else(|| not_found("milestone"))?;
        let now = crate::time::now_seconds();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "UPDATE milestones SET reached_at = IFNULL(reached_at, ?1) WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        if let (Some(task_id), Some(snapshot)) = (milestone.task_id, milestone.progress_snapshot) {
            advance_progress(&transaction, task_id, snapshot, now)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 撤销达成（误点）。**不会**把任务进度退回 —— 用户可能已经在此基础上往前做了。
    pub fn unreach_milestone(&self, id: i64) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE milestones SET reached_at = NULL WHERE id = ?1",
            [id],
        )?;
        if affected == 0 {
            return Err(not_found("milestone"));
        }
        Ok(())
    }

    /// 改标题 / 描述 / 目标日期（`None` = 不改）。
    pub fn update_milestone(
        &self,
        id: i64,
        title: Option<&str>,
        description: Option<&str>,
        due_date: Option<Option<crate::time::CalendarDate>>,
    ) -> Result<()> {
        if let Some(title) = title {
            require_non_empty("milestone title", title)?;
        }
        let mut set = crate::store::SetBuilder::new();
        set.push_optional("title", title, |title| Value::Text(title.trim().to_owned()));
        set.push_optional("description", description, |description| {
            Value::Text(description.to_owned())
        });
        set.push_optional("due_date", due_date, crate::store::date_value);
        if set.is_empty() {
            if self.get_milestone(id)?.is_none() {
                return Err(not_found("milestone"));
            }
            return Ok(());
        }
        let (assignments, mut args) = set.finish();
        args.push(Value::Integer(id));
        let sql = format!("UPDATE milestones SET {assignments} WHERE id = ?");
        let affected = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(args.iter()))?;
        if affected == 0 {
            return Err(not_found("milestone"));
        }
        Ok(())
    }

    /// 删除里程碑。
    pub fn delete_milestone(&self, id: i64) -> Result<()> {
        let affected = self
            .conn
            .execute("DELETE FROM milestones WHERE id = ?1", [id])?;
        if affected == 0 {
            return Err(not_found("milestone"));
        }
        Ok(())
    }

    /// 单个里程碑。
    pub fn get_milestone(&self, id: i64) -> Result<Option<Milestone>> {
        let mut statement = self.conn.prepare(
            "SELECT id, task_id, project_id, title, description, due_date, \
             progress_snapshot, reached_at, created_at FROM milestones WHERE id = ?1",
        )?;
        let mut rows = statement.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(milestone_from_row(row)?)),
            None => Ok(None),
        }
    }

    fn query_milestones(&self, clause: &'static str, key: i64) -> Result<Vec<Milestone>> {
        let sql = format!(
            "SELECT id, task_id, project_id, title, description, due_date, \
             progress_snapshot, reached_at, created_at FROM milestones \
             WHERE {clause} ORDER BY reached_at IS NOT NULL ASC, created_at ASC, id ASC"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query([key])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(milestone_from_row(row)?);
        }
        Ok(out)
    }
}

/// 任务进度到了 `snapshot` 以下就推进到 `snapshot`；已经更高就不动
/// （里程碑是「至少到这里」，不是「覆盖成这里」）。
fn advance_progress(
    transaction: &rusqlite::Transaction<'_>,
    task_id: i64,
    snapshot: u8,
    now: i64,
) -> Result<()> {
    transaction.execute(
        "UPDATE tasks SET progress = ?1, updated_at = ?2 WHERE id = ?3 AND progress < ?1",
        rusqlite::params![i64::from(snapshot.min(100)), now, task_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::error::{ProjectError, Result};
    use crate::models::{NewMilestone, NewProject, NewTask, TaskPatch};
    use crate::time::CalendarDate;

    fn date(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("合法日期")
    }

    #[test]
    fn create_list_and_reach_task_milestone() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t").progress(20))?;
        let first = store.create_milestone(
            NewMilestone::for_task(task, "接口跑通")
                .due_date(date(2026, 10, 20))
                .progress_snapshot(40),
        )?;
        store.create_milestone(NewMilestone::for_task(task, "压测通过").progress_snapshot(30))?;

        let milestones = store.list_task_milestones(task)?;
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].title, "接口跑通");
        assert_eq!(milestones[0].due_date, Some(date(2026, 10, 20)));
        assert!(!milestones[0].is_reached());
        // 创建即推进进度（20 → 40），第二个快照更低所以不动。
        assert_eq!(store.get_task(task)?.unwrap().progress, 40);

        store.reach_milestone(first)?;
        assert!(store.get_milestone(first)?.unwrap().is_reached());
        // 达成后：已达成的排在未达成之后（列表顺序按可达性分档）。
        let milestones = store.list_task_milestones(task)?;
        assert_eq!(milestones[1].id, first);
        Ok(())
    }

    #[test]
    fn snapshot_never_lowers_progress() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t").progress(80))?;
        store.create_milestone(NewMilestone::for_task(task, "m").progress_snapshot(30))?;
        assert_eq!(
            store.get_task(task)?.unwrap().progress,
            80,
            "里程碑是「至少到这里」，不该把已经更高的进度拉回去"
        );
        // 反过来：用户手工把进度调低了，达成时再推上去。
        store.update_task(task, TaskPatch::new().progress(10))?;
        let milestone =
            store.create_milestone(NewMilestone::for_task(task, "m2").progress_snapshot(50))?;
        store.reach_milestone(milestone)?;
        assert_eq!(store.get_task(task)?.unwrap().progress, 50);
        Ok(())
    }

    #[test]
    fn reaching_twice_keeps_the_first_timestamp() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t"))?;
        let milestone = store.create_milestone(NewMilestone::for_task(task, "m"))?;
        store.reach_milestone(milestone)?;
        let first = store.get_milestone(milestone)?.unwrap().reached_at;
        store.reach_milestone(milestone)?;
        assert_eq!(store.get_milestone(milestone)?.unwrap().reached_at, first);
        store.unreach_milestone(milestone)?;
        assert!(!store.get_milestone(milestone)?.unwrap().is_reached());
        Ok(())
    }

    #[test]
    fn project_milestones_are_separate_from_task_ones() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let task = store.create_task(NewTask::new("t").project(project))?;
        store.create_milestone(NewMilestone::for_project(project, "M1 上线"))?;
        store.create_milestone(NewMilestone::for_task(task, "任务级"))?;

        let project_level = store.list_project_milestones(project)?;
        assert_eq!(project_level.len(), 1);
        assert_eq!(project_level[0].title, "M1 上线");
        assert_eq!(project_level[0].task_id, None);
        assert_eq!(store.list_task_milestones(task)?.len(), 1);
        Ok(())
    }

    #[test]
    fn invalid_targets_are_rejected() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        // 谁都不挂：
        let orphan = NewMilestone::for_task(1, "m");
        let orphan = NewMilestone {
            task_id: None,
            ..orphan
        };
        assert!(matches!(
            store.create_milestone(orphan),
            Err(ProjectError::Invalid(_))
        ));
        assert!(matches!(
            store.create_milestone(NewMilestone::for_task(9_999, "m")),
            Err(ProjectError::NotFound("task"))
        ));
        let project = store.create_project(NewProject::new("A"))?;
        assert!(matches!(
            store.create_milestone(NewMilestone::for_project(9_999, "m")),
            Err(ProjectError::NotFound("project"))
        ));
        let milestone = store.create_milestone(NewMilestone::for_project(project, "m"))?;
        assert!(matches!(store.delete_milestone(milestone), Ok(())));
        assert!(matches!(
            store.delete_milestone(milestone),
            Err(ProjectError::NotFound("milestone"))
        ));
        Ok(())
    }

    #[test]
    fn deleting_a_task_cascades_to_its_milestones() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t"))?;
        let milestone = store.create_milestone(NewMilestone::for_task(task, "m"))?;
        store.delete_task(task)?;
        assert!(store.get_milestone(milestone)?.is_none());
        Ok(())
    }

    #[test]
    fn updating_a_milestone_keeps_blank_titles_out() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t"))?;
        let milestone = store.create_milestone(NewMilestone::for_task(task, "m"))?;
        store.update_milestone(milestone, Some("改名"), Some("说明"), None)?;
        let updated = store.get_milestone(milestone)?.unwrap();
        assert_eq!(updated.title, "改名");
        assert_eq!(updated.description, "说明");
        assert!(matches!(
            store.update_milestone(milestone, Some("  "), None, None),
            Err(ProjectError::Invalid(_))
        ));
        // 全 None 是幂等空操作（对象存在即可）。
        store.update_milestone(milestone, None, None, None)?;
        assert!(matches!(
            store.update_milestone(9_999, None, None, None),
            Err(ProjectError::NotFound("milestone"))
        ));
        Ok(())
    }
}
