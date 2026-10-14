//! 分组（左栏一级节点：工作 / 个人 …）。

use crate::error::Result;
use crate::models::{Group, NewGroup};
use crate::store::{OrderScope, ProjectStore, group_from_row, not_found, require_non_empty};

impl ProjectStore {
    /// 全部分组，按 `sort_order` 再按 id。
    pub fn list_groups(&self) -> Result<Vec<Group>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, name, icon, sort_order, collapsed, created_at FROM project_groups ORDER BY sort_order ASC, id ASC")?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(group_from_row(row)?);
        }
        Ok(out)
    }

    /// 新建分组，返回新 id。名称去首尾空白，空名报错。
    pub fn create_group(&self, group: NewGroup) -> Result<i64> {
        let name = group.name.trim();
        require_non_empty("group name", name)?;
        let sort_order = match group.sort_order {
            Some(value) => value,
            None => self.next_sort_order(OrderScope::Groups)?,
        };
        self.conn.execute(
            "INSERT INTO project_groups(name, icon, sort_order, collapsed, created_at) \
             VALUES(?1, ?2, ?3, 0, ?4)",
            rusqlite::params![name, group.icon, sort_order, crate::time::now_seconds()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 改名。
    pub fn rename_group(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        require_non_empty("group name", name)?;
        let affected = self.conn.execute(
            "UPDATE project_groups SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        if affected == 0 {
            return Err(not_found("group"));
        }
        Ok(())
    }

    /// 改图标（`None` = 清空）。
    pub fn set_group_icon(&self, id: i64, icon: Option<&str>) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE project_groups SET icon = ?1 WHERE id = ?2",
            rusqlite::params![icon, id],
        )?;
        if affected == 0 {
            return Err(not_found("group"));
        }
        Ok(())
    }

    /// 记录左栏折叠态。
    pub fn set_group_collapsed(&self, id: i64, collapsed: bool) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE project_groups SET collapsed = ?1 WHERE id = ?2",
            rusqlite::params![i64::from(collapsed), id],
        )?;
        if affected == 0 {
            return Err(not_found("group"));
        }
        Ok(())
    }

    /// 按给定顺序重排（数组下标即新的 `sort_order`）。
    ///
    /// 单事务：中途失败不会留下「一半新序、一半旧序」的排列。
    pub fn reorder_groups(&self, ids: &[i64]) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        {
            let mut statement =
                transaction.prepare("UPDATE project_groups SET sort_order = ?1 WHERE id = ?2")?;
            for (index, id) in ids.iter().enumerate() {
                statement.execute(rusqlite::params![index as i64, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 删除分组。**不删项目**：组内项目落回「未分组」（`ON DELETE SET NULL`）。
    pub fn delete_group(&self, id: i64) -> Result<()> {
        let affected = self
            .conn
            .execute("DELETE FROM project_groups WHERE id = ?1", [id])?;
        if affected == 0 {
            return Err(not_found("group"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::error::{ProjectError, Result};
    use crate::models::{NewGroup, NewProject};

    #[test]
    fn create_list_and_rename() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let work = store.create_group(NewGroup::new("工作").icon("briefcase"))?;
        store.create_group(NewGroup::new("个人"))?;
        let groups = store.list_groups()?;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].id, work);
        assert_eq!(groups[0].icon.as_deref(), Some("briefcase"));
        assert!(!groups[0].collapsed);
        // 未给 sort_order 时追加到末尾。
        assert!(groups[1].sort_order > groups[0].sort_order);

        store.rename_group(work, "  客户端项目  ")?;
        assert_eq!(store.list_groups()?[0].name, "客户端项目");
        store.set_group_collapsed(work, true)?;
        assert!(store.list_groups()?[0].collapsed);
        Ok(())
    }

    #[test]
    fn empty_name_is_rejected() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let error = store
            .create_group(NewGroup::new("   "))
            .expect_err("空名应报错");
        assert!(matches!(error, ProjectError::Invalid(_)));
        let group = store.create_group(NewGroup::new("工作"))?;
        assert!(matches!(
            store.rename_group(group, " "),
            Err(ProjectError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn reorder_is_transactional_and_ordered() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let a = store.create_group(NewGroup::new("A"))?;
        let b = store.create_group(NewGroup::new("B"))?;
        let c = store.create_group(NewGroup::new("C"))?;
        store.reorder_groups(&[c, a, b])?;
        let names: Vec<String> = store.list_groups()?.into_iter().map(|g| g.name).collect();
        assert_eq!(names, ["C", "A", "B"]);
        Ok(())
    }

    #[test]
    fn deleting_a_group_keeps_its_projects() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let group = store.create_group(NewGroup::new("工作"))?;
        let project = store.create_project(NewProject::new("Alpha").group(group))?;
        store.delete_group(group)?;
        assert!(store.list_groups()?.is_empty());
        // 项目还在，只是落到「未分组」。
        let project = store.get_project(project)?.expect("项目应保留");
        assert_eq!(project.group_id, None);
        Ok(())
    }

    #[test]
    fn updating_a_missing_group_is_not_found() {
        let store = ProjectStore::open_in_memory().expect("打开库");
        assert!(matches!(
            store.rename_group(9_999, "x"),
            Err(ProjectError::NotFound("group"))
        ));
        assert!(matches!(
            store.delete_group(9_999),
            Err(ProjectError::NotFound("group"))
        ));
    }
}
