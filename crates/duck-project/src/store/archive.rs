//! 归档：任务与项目**都可以归档**，归档后从一切常规视图与计数里消失，只在归档中心可见。
//!
//! 语义边界（设计文档 §2.3）：
//!
//! | | 归档 | 软删（`cancelled`） | 物理删除 |
//! | --- | --- | --- | --- |
//! | 数据 | 全量保留 | 保留 | 级联删除 |
//! | 原地恢复 | ✅ 一键 | ✅ 改回待办 | ❌ |
//! | 二次确认 | ❌（可逆且不丢数据） | ❌ | ✅ 必须 |
//!
//! # 级联归属（`archived_root`）
//!
//! - **归档项目** → 项目下所有未归档任务（含它们的子任务）记
//!   `archived_root = 'project:<id>'`。
//! - **归档顶层任务** → 它自己与它的子任务记 `archived_root = 'task:<id>'`。
//! - **归档中心 · 任务段** 只列「自己就是根」的那些（`archived_root = 'task:' || t.id`），
//!   所以随项目归档的任务不会在任务段重复出现。
//! - **恢复**只复活挂在同一个根下的行；独立归档过的任务不会被项目恢复连带复活
//!   （它的根是 `task:<自己的 id>`，与项目根不同）。

use rusqlite::types::Value;

use crate::error::{ProjectError, Result};
use crate::models::{ArchiveRoot, Project, Task};
use crate::query::escape_like;
use crate::store::{
    ProjectStore, TASK_COLUMNS, hydrate, not_found, project_from_row, task_from_row,
};

/// 归档中心的计件数（左栏入口的徽标）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveCounts {
    /// 「任务段」的条数（自己就是归档根的任务）。
    pub tasks: u32,
    /// 已归档项目数。
    pub projects: u32,
}

impl ArchiveCounts {
    /// 徽标上显示的总数。
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.tasks + self.projects
    }

    /// 是否为空（空则不显示左栏入口的徽标）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// 归档中心「任务段」的一行。
#[derive(Debug, Clone)]
pub struct ArchivedTask {
    /// 任务本身（已归档）。
    pub task: Task,
    /// 所属项目名；未归属项目时为 `None`。
    pub project_name: Option<String>,
    /// 随这条一起归档的子任务数。
    pub cascaded_subtasks: u32,
}

/// 归档中心「项目段」的一行。
#[derive(Debug, Clone)]
pub struct ArchivedProject {
    /// 项目本身（已归档）。
    pub project: Project,
    /// 随它一起归档的任务数（含子任务）。
    pub cascaded_tasks: u32,
}

/// 「清空归档」的范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScope {
    /// 只清任务段。
    Tasks,
    /// 只清项目段（连同随项目归档的任务）。
    Projects,
    /// 全清。
    All,
}

impl ProjectStore {
    /// 归档一个顶层任务（连同它的子任务）。已归档时是幂等空操作。
    pub fn archive_task(&self, id: i64) -> Result<()> {
        let task = self.get_task(id)?.ok_or_else(|| not_found("task"))?;
        if task.archived_at.is_some() {
            return Ok(());
        }
        let now = crate::time::now_seconds();
        let root = ArchiveRoot::Task(id).to_wire();
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "UPDATE tasks SET archived_at = ?1, archived_root = ?2, updated_at = ?1 WHERE id = ?3",
            rusqlite::params![now, root, id],
        )?;
        // 子任务跟着一起归档：父卡片都看不见了，子任务单独留在列表里只会让人困惑。
        transaction.execute(
            "UPDATE tasks SET archived_at = ?1, archived_root = ?2, updated_at = ?1 \
             WHERE parent_id = ?3 AND archived_at IS NULL",
            rusqlite::params![now, root, id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 恢复一个顶层任务（连同随它归档的子任务）。
    ///
    /// 若这条是「随项目归档」的，直接拒绝并提示先恢复项目 —— 否则会出现
    /// 「项目还在归档中、里面某个任务冒出来」的撕裂状态。
    pub fn restore_task(&self, id: i64) -> Result<()> {
        let task = self.get_task(id)?.ok_or_else(|| not_found("task"))?;
        let Some(root) = task.archived_root else {
            return Ok(()); // 本来就没归档
        };
        if !root.is_task() {
            return Err(ProjectError::Invalid(
                "task is archived with its project; restore the project instead".to_owned(),
            ));
        }
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "UPDATE tasks SET archived_at = NULL, archived_root = NULL \
             WHERE archived_root = ?1",
            [root.to_wire()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 归档项目，连同它名下所有未归档的任务（含子任务）。返回随它归档的任务数。
    pub fn archive_project(&self, id: i64) -> Result<u32> {
        let project = self.get_project(id)?.ok_or_else(|| not_found("project"))?;
        if project.archived_at.is_some() {
            return Ok(0);
        }
        let now = crate::time::now_seconds();
        let root = ArchiveRoot::Project(id).to_wire();
        let transaction = self.conn.unchecked_transaction()?;
        let cascaded = transaction.execute(
            "UPDATE tasks SET archived_at = ?1, archived_root = ?2, updated_at = ?1 \
             WHERE project_id = ?3 AND archived_at IS NULL",
            rusqlite::params![now, root, id],
        )?;
        transaction.execute(
            "UPDATE projects SET archived_at = ?1, updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        transaction.commit()?;
        Ok(cascaded as u32)
    }

    /// 恢复项目，连同**随它一起归档**的任务（独立归档过的任务不连带恢复，
    /// 避免用户「东西自己冒出来」）。返回一起恢复的任务数。
    pub fn restore_project(&self, id: i64) -> Result<u32> {
        let project = self.get_project(id)?.ok_or_else(|| not_found("project"))?;
        if project.archived_at.is_none() {
            return Ok(0);
        }
        let root = ArchiveRoot::Project(id).to_wire();
        let transaction = self.conn.unchecked_transaction()?;
        let restored = transaction.execute(
            "UPDATE tasks SET archived_at = NULL, archived_root = NULL WHERE archived_root = ?1",
            [root.as_str()],
        )?;
        transaction.execute(
            "UPDATE projects SET archived_at = NULL, updated_at = ?1 WHERE id = ?2",
            rusqlite::params![crate::time::now_seconds(), id],
        )?;
        transaction.commit()?;
        Ok(restored as u32)
    }

    /// 彻底删除归档任务（物理删，级联子任务与里程碑）。
    ///
    /// 只允许删**已归档**的：让「删」必须经过「归档」这一步，界面上就永远不会有
    /// 「手滑点了某处，东西直接没了」的入口。
    pub fn delete_archived_task(&self, id: i64) -> Result<()> {
        let task = self.get_task(id)?.ok_or_else(|| not_found("task"))?;
        if task.archived_at.is_none() {
            return Err(ProjectError::Invalid(
                "only archived tasks can be deleted here; archive it first".to_owned(),
            ));
        }
        self.delete_task(id)
    }

    /// 彻底删除归档项目（物理删，级联它的任务与里程碑）。
    pub fn delete_archived_project(&self, id: i64) -> Result<()> {
        let project = self.get_project(id)?.ok_or_else(|| not_found("project"))?;
        if project.archived_at.is_none() {
            return Err(ProjectError::Invalid(
                "only archived projects can be deleted here; archive it first".to_owned(),
            ));
        }
        self.delete_project(id)
    }

    /// 清空归档。返回清掉的**归档中心条目数**（任务段行数 + 项目段行数），
    /// 与 [`Self::archive_counts`] 的口径一致 —— 子任务跟着父任务走，不单独计数。
    ///
    /// 界面必须二次确认。
    pub fn clear_archive(&self, scope: ClearScope) -> Result<u32> {
        let before = self.archive_counts()?;
        let transaction = self.conn.unchecked_transaction()?;
        if matches!(scope, ClearScope::Tasks | ClearScope::All) {
            // 只删「自己就是根」的任务：子任务走 `parent_id` 级联。
            // 随项目归档的任务**不在这里删** —— 它们的项目还在，删了会留下空项目。
            transaction.execute(
                "DELETE FROM tasks WHERE archived_at IS NOT NULL \
                 AND archived_root = 'task:' || id",
                [],
            )?;
        }
        if matches!(scope, ClearScope::Projects | ClearScope::All) {
            // 删项目连带它名下任务（`ON DELETE CASCADE`）。
            transaction.execute("DELETE FROM projects WHERE archived_at IS NOT NULL", [])?;
        }
        transaction.commit()?;
        Ok(match scope {
            ClearScope::Tasks => before.tasks,
            ClearScope::Projects => before.projects,
            ClearScope::All => before.total(),
        })
    }

    /// 归档中心「任务段」：自己就是归档根的已归档任务，按归档时间倒序。
    ///
    /// `search` 为 `Some` 时按标题 / 描述过滤（只搜归档区）。
    pub fn list_archived_tasks(&self, search: Option<&str>) -> Result<Vec<ArchivedTask>> {
        let mut sql = format!(
            "SELECT {TASK_COLUMNS}, p.name, \
             (SELECT COUNT(*) FROM tasks c WHERE c.parent_id = t.id AND c.archived_at IS NOT NULL) \
             FROM tasks t LEFT JOIN projects p ON p.id = t.project_id \
             WHERE t.archived_at IS NOT NULL AND t.archived_root = 'task:' || t.id"
        );
        let term = search.map(str::trim).filter(|term| !term.is_empty());
        if term.is_some() {
            sql.push_str(
                r" AND (t.title LIKE ?1 ESCAPE '\' OR t.description_md LIKE ?1 ESCAPE '\')",
            );
        }
        sql.push_str(" ORDER BY t.archived_at DESC, t.id DESC");

        let args: Vec<Value> = match term {
            Some(term) => vec![Value::Text(format!("%{}%", escape_like(term)))],
            None => Vec::new(),
        };
        let mut rows_out: Vec<(Task, Option<String>, u32)> = Vec::new();
        {
            let mut statement = self.conn.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(args.iter()))?;
            while let Some(row) = rows.next()? {
                rows_out.push((
                    task_from_row(row)?,
                    row.get(20)?,
                    row.get::<_, i64>(21)?.max(0) as u32,
                ));
            }
        }
        let mut tasks: Vec<Task> = rows_out.iter().map(|(task, _, _)| task.clone()).collect();
        hydrate(&self.conn, &mut tasks)?;
        Ok(rows_out
            .into_iter()
            .zip(tasks)
            .map(|((_, project_name, cascaded), task)| ArchivedTask {
                task,
                project_name,
                cascaded_subtasks: cascaded,
            })
            .collect())
    }

    /// 归档中心「项目段」：已归档项目 + 随它归档的任务数，按归档时间倒序。
    pub fn list_archived_projects(&self, search: Option<&str>) -> Result<Vec<ArchivedProject>> {
        let mut sql = String::from(
            "SELECT p.id, p.group_id, p.name, p.description, p.color, p.icon, p.status, \
             p.start_date, p.due_date, p.sort_order, p.created_at, p.updated_at, p.archived_at, \
             (SELECT COUNT(*) FROM tasks t WHERE t.archived_root = 'project:' || p.id) \
             FROM projects p WHERE p.archived_at IS NOT NULL",
        );
        let term = search.map(str::trim).filter(|term| !term.is_empty());
        if term.is_some() {
            sql.push_str(r" AND (p.name LIKE ?1 ESCAPE '\' OR p.description LIKE ?1 ESCAPE '\')");
        }
        sql.push_str(" ORDER BY p.archived_at DESC, p.id DESC");

        let args: Vec<Value> = match term {
            Some(term) => vec![Value::Text(format!("%{}%", escape_like(term)))],
            None => Vec::new(),
        };
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(args.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ArchivedProject {
                project: project_from_row(row)?,
                cascaded_tasks: row.get::<_, i64>(13)?.max(0) as u32,
            });
        }
        Ok(out)
    }

    /// 归档中心的计件数（口径与两个 `list_*` 严格一致）。
    pub fn archive_counts(&self) -> Result<ArchiveCounts> {
        let tasks: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE archived_at IS NOT NULL \
             AND archived_root = 'task:' || id",
            [],
            |row| row.get(0),
        )?;
        let projects: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM projects WHERE archived_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(ArchiveCounts {
            tasks: tasks.max(0) as u32,
            projects: projects.max(0) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClearScope, ProjectStore};
    use crate::error::{ProjectError, Result};
    use crate::models::{NewProject, NewTask, ProjectPatch, TaskStatus};
    use crate::query::{ArchiveScope, TaskQuery};

    #[test]
    fn archiving_a_task_hides_it_everywhere_but_the_archive_center() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let keep = store.create_task(NewTask::new("keep").project(project))?;
        let gone = store.create_task(NewTask::new("gone").project(project))?;
        store.archive_task(gone)?;

        assert_eq!(store.list_tasks(&TaskQuery::new())?.len(), 1);
        assert_eq!(
            store.project_task_counts()?.get(&project).map(|c| c.total),
            Some(1)
        );
        assert_eq!(store.task_counts(&[project])?.total, 1);
        // 全局统计也不含它。
        let stats = store.board_stats(crate::time::CalendarDate::new(2026, 10, 14).unwrap())?;
        assert_eq!(stats.total, 1);
        assert_eq!(stats.archived_tasks, 1);
        // 归档中心能看到。
        let archived = store.list_archived_tasks(None)?;
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].task.id, gone);
        assert_eq!(archived[0].project_name.as_deref(), Some("Alpha"));
        assert_eq!(
            archived[0].task.archived_root,
            Some(crate::models::ArchiveRoot::Task(gone))
        );
        assert_eq!(
            store.archive_counts()?,
            super::ArchiveCounts {
                tasks: 1,
                projects: 0
            }
        );
        assert_eq!(store.get_task(keep)?.unwrap().archived_at, None);
        Ok(())
    }

    #[test]
    fn archiving_a_task_also_archives_its_subtasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let parent = store.create_task(NewTask::new("父"))?;
        store.create_task(NewTask::new("子1").parent(parent).status(TaskStatus::Done))?;
        store.create_task(NewTask::new("子2").parent(parent))?;
        store.archive_task(parent)?;

        assert!(store.list_subtasks(parent)?.is_empty());
        let archived = store.list_archived_tasks(None)?;
        assert_eq!(archived.len(), 1, "子任务不单独进任务段");
        assert_eq!(archived[0].cascaded_subtasks, 2);
        assert_eq!(store.archive_counts()?.tasks, 1);

        // 恢复一条就全回来。
        store.restore_task(parent)?;
        assert_eq!(store.list_subtasks(parent)?.len(), 2);
        assert!(store.list_archived_tasks(None)?.is_empty());
        Ok(())
    }

    #[test]
    fn archiving_a_project_cascades_and_leaves_other_archives_alone() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let alpha = store.create_project(NewProject::new("Alpha"))?;
        let beta = store.create_project(NewProject::new("Beta"))?;
        store.create_task(NewTask::new("a1").project(alpha))?;
        let a2 = store.create_task(NewTask::new("a2").project(alpha))?;
        store.create_task(NewTask::new("b1").project(beta))?;
        // 先独立归档一条，它不该被项目归档「吞掉」。
        store.archive_task(a2)?;

        let cascaded = store.archive_project(alpha)?;
        assert_eq!(cascaded, 1, "只归档当时还没归档的那条");
        assert_eq!(store.list_projects(ArchiveScope::Live)?.len(), 1);
        assert_eq!(store.task_counts(&[])?.total, 1, "只剩 Beta 的那条");
        assert_eq!(
            store.archive_counts()?,
            super::ArchiveCounts {
                tasks: 1,
                projects: 1
            }
        );

        // 任务段里只有那条独立归档的（随项目归档的不在任务段）。
        let archived = store.list_archived_tasks(None)?;
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].task.id, a2);
        assert_eq!(store.list_archived_projects(None)?[0].cascaded_tasks, 1);
        Ok(())
    }

    #[test]
    fn restoring_a_project_does_not_resurrect_independently_archived_tasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let a1 = store.create_task(NewTask::new("a1").project(project))?;
        let a2 = store.create_task(NewTask::new("a2").project(project))?;
        store.archive_task(a2)?;
        let _ = store.archive_project(project)?;

        let restored = store.restore_project(project)?;
        assert_eq!(restored, 1, "只恢复随项目归档的那条");
        assert_eq!(store.get_project(project)?.unwrap().archived_at, None);
        assert_eq!(store.get_task(a1)?.unwrap().archived_at, None);
        assert!(
            store.get_task(a2)?.unwrap().archived_at.is_some(),
            "独立归档过的任务不该被项目恢复连带复活"
        );
        assert_eq!(store.archive_counts()?.tasks, 1);
        Ok(())
    }

    #[test]
    fn restoring_a_cascade_archived_task_directly_is_refused() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let task = store.create_task(NewTask::new("t").project(project))?;
        let _ = store.archive_project(project)?;
        let error = store
            .restore_task(task)
            .expect_err("随项目归档的任务不能单独恢复");
        assert!(
            matches!(error, ProjectError::Invalid(ref detail) if detail.contains("project")),
            "错误应提示先恢复项目，实际：{error}"
        );
        Ok(())
    }

    #[test]
    fn archiving_is_idempotent() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let task = store.create_task(NewTask::new("t").project(project))?;
        store.archive_task(task)?;
        let first = store.get_task(task)?.unwrap().archived_at;
        store.archive_task(task)?;
        assert_eq!(store.get_task(task)?.unwrap().archived_at, first);
        assert_eq!(store.archive_counts()?.tasks, 1, "重复归档不该产生第二条");

        let _ = store.archive_project(project)?;
        let stamp = store.get_project(project)?.unwrap().archived_at;
        assert_eq!(store.archive_project(project)?, 0);
        assert_eq!(store.get_project(project)?.unwrap().archived_at, stamp);
        // 未归档的恢复也是空操作。
        let live = store.create_task(NewTask::new("live"))?;
        store.restore_task(live)?;
        assert_eq!(store.get_task(live)?.unwrap().archived_at, None);
        Ok(())
    }

    #[test]
    fn physical_delete_requires_prior_archiving() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let task = store.create_task(NewTask::new("t"))?;
        assert!(matches!(
            store.delete_archived_task(task),
            Err(ProjectError::Invalid(_))
        ));
        assert!(store.get_task(task)?.is_some(), "拒绝时不能真删掉");

        store.archive_task(task)?;
        store.delete_archived_task(task)?;
        assert!(store.get_task(task)?.is_none());
        assert!(matches!(
            store.delete_archived_task(task),
            Err(ProjectError::NotFound("task"))
        ));
        Ok(())
    }

    #[test]
    fn deleting_an_archived_project_cascades_to_its_archived_tasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let task = store.create_task(NewTask::new("t").project(project))?;
        let _ = store.archive_project(project)?;
        store.delete_archived_project(project)?;
        assert!(store.get_project(project)?.is_none());
        assert!(store.get_task(task)?.is_none(), "任务应随项目级联删除");
        assert_eq!(store.archive_counts()?, super::ArchiveCounts::default());
        Ok(())
    }

    #[test]
    fn search_only_sees_the_archive_area() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let gone = store.create_task(NewTask::new("修复登录超时"))?;
        store.create_task(NewTask::new("修复登录白屏"))?;
        store.archive_task(gone)?;
        assert_eq!(store.list_archived_tasks(Some("登录"))?.len(), 1);
        assert_eq!(store.list_archived_tasks(Some("白屏"))?.len(), 0);
        assert_eq!(
            store.list_archived_tasks(Some("   "))?.len(),
            1,
            "空白关键词=不过滤"
        );
        // LIKE 元字符要被转义，不能当通配符。
        store.archive_task(store.create_task(NewTask::new("100% 覆盖"))?)?;
        assert_eq!(store.list_archived_tasks(Some("100%"))?.len(), 1);
        assert_eq!(store.list_archived_tasks(Some("1_0%"))?.len(), 0);

        let project = store.create_project(NewProject::new("重构"))?;
        let _ = store.archive_project(project)?;
        assert_eq!(store.list_archived_projects(Some("重构"))?.len(), 1);
        assert_eq!(store.list_archived_projects(Some("别的"))?.len(), 0);
        Ok(())
    }

    #[test]
    fn clear_archive_covers_both_sections() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        store.create_task(NewTask::new("inside").project(project))?;
        let standalone = store.create_task(NewTask::new("standalone"))?;
        store.archive_task(standalone)?;
        let _ = store.archive_project(project)?;
        assert_eq!(
            store.archive_counts()?,
            super::ArchiveCounts {
                tasks: 1,
                projects: 1
            }
        );
        assert_eq!(store.list_archived_tasks(None)?.len(), 1);

        // 只清任务段。
        assert_eq!(store.clear_archive(ClearScope::Tasks)?, 1);
        assert_eq!(
            store.archive_counts()?,
            super::ArchiveCounts {
                tasks: 0,
                projects: 1
            }
        );
        assert_eq!(
            store.list_projects(ArchiveScope::Live)?.len(),
            0,
            "项目还在归档中，不该被「清任务段」顺手删掉"
        );

        // 再清项目段：项目 + 随它归档的任务一起走。
        assert_eq!(store.clear_archive(ClearScope::Projects)?, 1);
        assert_eq!(store.archive_counts()?, super::ArchiveCounts::default());
        assert_eq!(store.task_counts(&[])?.total, 0);
        assert!(store.list_projects(ArchiveScope::Any)?.is_empty());
        Ok(())
    }

    #[test]
    fn archiving_does_not_touch_project_status() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        store.update_project(
            project,
            ProjectPatch::new().status(crate::models::ProjectStatus::Paused),
        )?;
        let _ = store.archive_project(project)?;
        let archived = &store.list_archived_projects(None)?[0].project;
        assert_eq!(
            archived.status,
            crate::models::ProjectStatus::Paused,
            "归档是正交维度，不该改写项目状态"
        );
        Ok(())
    }
}
