//! 项目（含左栏树所需的排序与归档可见性）。

use std::collections::HashMap;

use rusqlite::types::Value;

use crate::error::Result;
use crate::models::{NewProject, Project, ProjectPatch, TaskCounts};
use crate::query::ArchiveScope;
use crate::store::{
    OrderScope, PROJECT_COLUMNS, ProjectStore, SetBuilder, not_found, project_from_row,
    require_non_empty,
};

impl ProjectStore {
    /// 按归档可见性列项目。
    pub fn list_projects(&self, scope: ArchiveScope) -> Result<Vec<Project>> {
        let clause = match scope {
            ArchiveScope::Live => " WHERE p.archived_at IS NULL",
            ArchiveScope::Archived => " WHERE p.archived_at IS NOT NULL",
            ArchiveScope::Any => "",
        };
        let order = match scope {
            // 归档中心按归档时间倒序（最近归档的在最上面）。
            ArchiveScope::Archived => " ORDER BY p.archived_at DESC, p.id DESC",
            _ => " ORDER BY p.sort_order ASC, p.id ASC",
        };
        let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects p{clause}{order}");
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(project_from_row(row)?);
        }
        Ok(out)
    }

    /// 单个项目（含已归档的；「已归档」由调用方按 `archived_at` 判断）。
    pub fn get_project(&self, id: i64) -> Result<Option<Project>> {
        let sql = format!("SELECT {PROJECT_COLUMNS} FROM projects p WHERE p.id = ?1");
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(project_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 新建项目，返回新 id。
    pub fn create_project(&self, new: NewProject) -> Result<i64> {
        let name = new.name.trim();
        require_non_empty("project name", name)?;
        let sort_order = match new.sort_order {
            Some(value) => value,
            None => self.next_sort_order(OrderScope::Projects)?,
        };
        let now = crate::time::now_seconds();
        self.conn.execute(
            "INSERT INTO projects(group_id, name, description, color, icon, status, \
             start_date, due_date, sort_order, created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8, ?9, ?9)",
            rusqlite::params![
                new.group_id,
                name,
                new.description,
                new.color,
                new.icon,
                new.start_date.map(|date| date.to_iso()),
                new.due_date.map(|date| date.to_iso()),
                sort_order,
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 局部更新。patch 全空时不发语句也不报错（幂等空操作）。
    ///
    /// `updated_at` 只在**确实改了东西**时刷新。
    pub fn update_project(&self, id: i64, patch: ProjectPatch) -> Result<()> {
        // 名称若给了就必须非空 —— 否则项目会在左栏变成一个点不中的空行。
        // 在校验层先拦下，不要等 `SetBuilder` 拼好语句再回退。
        if let Some(name) = &patch.name {
            require_non_empty("project name", name)?;
        }

        let mut set = SetBuilder::new();
        set.push_optional("name", patch.name, |name| {
            Value::Text(name.trim().to_owned())
        });
        set.push_optional("group_id", patch.group_id, |group| {
            group.map_or(Value::Null, Value::Integer)
        });
        set.push_optional("description", patch.description, Value::Text);
        set.push_optional("color", patch.color, Value::Text);
        set.push_optional("icon", patch.icon, |icon| {
            icon.map_or(Value::Null, Value::Text)
        });
        set.push_optional("status", patch.status, |status| {
            Value::Text(status.as_str().to_owned())
        });
        set.push_optional("start_date", patch.start_date, crate::store::date_value);
        set.push_optional("due_date", patch.due_date, crate::store::date_value);
        set.push_optional("sort_order", patch.sort_order, Value::Integer);

        if set.is_empty() {
            // 空 patch：确认对象存在即可（不存在要报错，否则界面会以为「改成功了」）。
            if self.get_project(id)?.is_none() {
                return Err(not_found("project"));
            }
            return Ok(());
        }

        set.push("updated_at", Value::Integer(crate::time::now_seconds()));
        let (assignments, mut args) = set.finish();
        args.push(Value::Integer(id));
        let sql = format!("UPDATE projects SET {assignments} WHERE id = ?");
        let affected = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(args.iter()))?;
        if affected == 0 {
            return Err(not_found("project"));
        }
        Ok(())
    }

    /// 物理删除项目。**级联**删掉它的任务（`ON DELETE CASCADE`）与项目级里程碑。
    ///
    /// 与归档的分工：这是「这条不该存在」，归档是「不想再看见但留着」。界面只在
    /// 「归档中心」的彻底删除里调它，且必须二次确认。
    pub fn delete_project(&self, id: i64) -> Result<()> {
        let affected = self
            .conn
            .execute("DELETE FROM projects WHERE id = ?1", [id])?;
        if affected == 0 {
            return Err(not_found("project"));
        }
        Ok(())
    }

    /// 按给定顺序重排项目（数组下标即新的 `sort_order`）。单事务。
    pub fn reorder_projects(&self, ids: &[i64]) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement =
                transaction.prepare("UPDATE projects SET sort_order = ?1 WHERE id = ?2")?;
            for (index, id) in ids.iter().enumerate() {
                statement.execute(rusqlite::params![index as i64, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 把一批项目挂到某个分组下（`None` = 移出分组）。单事务。
    pub fn assign_projects_to_group(&self, ids: &[i64], group_id: Option<i64>) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement =
                transaction.prepare("UPDATE projects SET group_id = ?1 WHERE id = ?2")?;
            for id in ids {
                statement.execute(rusqlite::params![group_id, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 左栏树：分组 → 项目，附带每个项目的未完成计数。
    ///
    /// 一次查询取完全部计数（不是逐项目查），供左栏一次性渲染。
    pub fn project_tree(&self) -> Result<ProjectTree> {
        Ok(ProjectTree {
            groups: self.list_groups()?,
            projects: self.list_projects(ArchiveScope::Live)?,
            counts: self.project_task_counts()?,
        })
    }
}

/// 左栏树的一次性快照。
///
/// 三个字段必须**同一次读**取出来：分开读的话，两次读之间用户刚好归档了一个项目，
/// 左栏就会显示「项目消失了但计数还在」那一帧。
#[derive(Debug, Clone, Default)]
pub struct ProjectTree {
    /// 分组，按手动顺序。
    pub groups: Vec<crate::models::Group>,
    /// 未归档项目。
    pub projects: Vec<Project>,
    /// 项目 id → 未归档任务计数。
    pub counts: HashMap<i64, TaskCounts>,
}

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::error::{ProjectError, Result};
    use crate::models::{NewGroup, NewProject, NewTask, ProjectPatch, ProjectStatus};
    use crate::query::ArchiveScope;
    use crate::time::CalendarDate;

    fn date(year: i32, month: u32, day: u32) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("合法日期")
    }

    #[test]
    fn create_read_update_round_trip() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_project(
            NewProject::new("  Alpha  ")
                .description("第一个项目")
                .color("#FF0000")
                .icon("rocket")
                .dates(Some(date(2026, 10, 1)), Some(date(2026, 12, 31))),
        )?;
        let project = store.get_project(id)?.expect("刚建的项目");
        assert_eq!(project.name, "Alpha", "名称应去首尾空白");
        assert_eq!(project.color, "#FF0000");
        assert_eq!(project.icon.as_deref(), Some("rocket"));
        assert_eq!(project.status, ProjectStatus::Active);
        assert_eq!(project.start_date, Some(date(2026, 10, 1)));
        assert_eq!(project.due_date, Some(date(2026, 12, 31)));
        assert!(!project.is_archived());
        assert_eq!(project.created_at, project.updated_at);

        store.update_project(
            id,
            ProjectPatch::new()
                .name("Alpha 2")
                .status(ProjectStatus::Paused)
                .clear_icon()
                .dates(None, Some(date(2027, 1, 1))),
        )?;
        let project = store.get_project(id)?.expect("更新后仍存在");
        assert_eq!(project.name, "Alpha 2");
        assert_eq!(project.status, ProjectStatus::Paused);
        assert_eq!(project.icon, None, "clear_icon 应写 NULL");
        assert_eq!(project.start_date, None);
        assert_eq!(project.due_date, Some(date(2027, 1, 1)));
        Ok(())
    }

    #[test]
    fn empty_patch_is_a_no_op_but_still_checks_existence() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let id = store.create_project(NewProject::new("Alpha"))?;
        let before = store.get_project(id)?.expect("项目");
        store.update_project(id, ProjectPatch::new())?;
        assert_eq!(
            store.get_project(id)?.expect("项目").updated_at,
            before.updated_at,
            "空 patch 不该刷新 updated_at"
        );
        assert!(matches!(
            store.update_project(9_999, ProjectPatch::new()),
            Err(ProjectError::NotFound("project"))
        ));
        Ok(())
    }

    #[test]
    fn blank_name_is_rejected_on_create_and_update() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        assert!(matches!(
            store.create_project(NewProject::new("   ")),
            Err(ProjectError::Invalid(_))
        ));
        let id = store.create_project(NewProject::new("Alpha"))?;
        assert!(matches!(
            store.update_project(id, ProjectPatch::new().name(" ")),
            Err(ProjectError::Invalid(_))
        ));
        assert_eq!(store.get_project(id)?.expect("项目").name, "Alpha");
        Ok(())
    }

    #[test]
    fn list_projects_splits_live_and_archived_with_different_ordering() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let alpha = store.create_project(NewProject::new("Alpha"))?;
        let beta = store.create_project(NewProject::new("Beta"))?;
        store.create_project(NewProject::new("Gamma"))?;
        let _ = store.archive_project(alpha)?;
        let _ = store.archive_project(beta)?;

        let live = store.list_projects(ArchiveScope::Live)?;
        assert_eq!(
            live.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["Gamma"]
        );
        let archived = store.list_projects(ArchiveScope::Archived)?;
        // 归档中心按归档时间倒序。
        assert_eq!(archived.len(), 2);
        assert_eq!(archived[0].name, "Beta");
        assert_eq!(archived[1].name, "Alpha");
        assert_eq!(store.list_projects(ArchiveScope::Any)?.len(), 3);
        Ok(())
    }

    #[test]
    fn reorder_projects_sets_manual_order() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let a = store.create_project(NewProject::new("A"))?;
        let b = store.create_project(NewProject::new("B"))?;
        let c = store.create_project(NewProject::new("C"))?;
        store.reorder_projects(&[c, b, a])?;
        let names: Vec<String> = store
            .list_projects(ArchiveScope::Live)?
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["C", "B", "A"]);
        Ok(())
    }

    #[test]
    fn deleting_a_project_cascades_to_its_tasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let project = store.create_project(NewProject::new("Alpha"))?;
        let task = store.create_task(NewTask::new("t").project(project))?;
        store.delete_project(project)?;
        assert!(store.get_project(project)?.is_none());
        assert!(
            store.get_task(task)?.is_none(),
            "删项目必须级联删任务（foreign_keys 关了就是孤儿行）"
        );
        assert!(matches!(
            store.delete_project(project),
            Err(ProjectError::NotFound("project"))
        ));
        Ok(())
    }

    #[test]
    fn project_tree_bundles_groups_projects_and_counts() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let group = store.create_group(NewGroup::new("工作"))?;
        let alpha = store.create_project(NewProject::new("Alpha").group(group))?;
        store.create_project(NewProject::new("Beta"))?;
        store.create_task(NewTask::new("a1").project(alpha))?;
        store.create_task(NewTask::new("a2").project(alpha))?;

        let tree = store.project_tree()?;
        assert_eq!(tree.groups.len(), 1);
        assert_eq!(tree.projects.len(), 2);
        assert_eq!(tree.counts.get(&alpha).map(|c| c.total), Some(2));
        assert_eq!(tree.counts.get(&alpha).map(|c| c.open()), Some(2));
        Ok(())
    }

    #[test]
    fn assigning_projects_to_group_moves_and_clears() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let group = store.create_group(NewGroup::new("工作"))?;
        let a = store.create_project(NewProject::new("A"))?;
        let b = store.create_project(NewProject::new("B"))?;
        store.assign_projects_to_group(&[a, b], Some(group))?;
        assert_eq!(store.get_project(a)?.unwrap().group_id, Some(group));
        store.assign_projects_to_group(&[a], None)?;
        assert_eq!(store.get_project(a)?.unwrap().group_id, None);
        assert_eq!(store.get_project(b)?.unwrap().group_id, Some(group));
        Ok(())
    }
}
