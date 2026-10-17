//! 标签（全局共享，跨项目复用）。

use crate::error::{ProjectError, Result};
use crate::models::{NewTag, Tag};
use crate::store::{ProjectStore, not_found, require_non_empty, tag_from_row};

/// 标签 + 使用次数（「标签管理」页要按使用次数排序 / 显示徽标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagUsage {
    /// 标签本身。
    pub tag: Tag,
    /// 挂了多少个**未归档**任务。
    pub usage: u32,
}

impl ProjectStore {
    /// 全部标签，按名称升序；`usage_count` 现算（不在表里缓存 —— 缓存列会随
    /// 归档 / 删除 / 级联失配，而这张表本来就很小）。
    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        Ok(self
            .tag_usage()?
            .into_iter()
            .map(|entry| Tag {
                usage_count: entry.usage,
                ..entry.tag
            })
            .collect())
    }

    /// 新建标签，返回新 id。名称去首尾空白；重名报 [`ProjectError::Invalid`]
    /// （要「有就复用」用 [`Self::ensure_tag`]）。
    pub fn create_tag(&self, tag: NewTag) -> Result<i64> {
        let name = tag.name.trim();
        require_non_empty("tag name", name)?;
        if self.find_tag_by_name(name)?.is_some() {
            return Err(ProjectError::Invalid(format!(
                "tag name already exists: {name}"
            )));
        }
        self.conn.execute(
            "INSERT INTO tags(name, color) VALUES(?1, ?2)",
            rusqlite::params![name, tag.color],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 有就复用、没有就建（快速新建里的 `#标签` 走它：同一个词反复输入不该建出多个标签）。
    pub fn ensure_tag(&self, name: &str, color: &str) -> Result<i64> {
        let name = name.trim();
        require_non_empty("tag name", name)?;
        if let Some(existing) = self.find_tag_by_name(name)? {
            return Ok(existing.id);
        }
        self.conn.execute(
            "INSERT INTO tags(name, color) VALUES(?1, ?2)",
            rusqlite::params![name, color],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 按名称精确查（重名不敏感由 `UNIQUE` 保证，这里的比较是字节相等）。
    pub fn find_tag_by_name(&self, name: &str) -> Result<Option<Tag>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, name, color, is_favorite FROM tags WHERE name = ?1")?;
        let mut rows = statement.query([name])?;
        match rows.next()? {
            Some(row) => Ok(Some(tag_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 改名称 / 改颜色（`None` = 不改）。
    pub fn update_tag(&self, id: i64, name: Option<&str>, color: Option<&str>) -> Result<()> {
        if let Some(name) = name {
            let name = name.trim();
            require_non_empty("tag name", name)?;
            if let Some(existing) = self.find_tag_by_name(name)?
                && existing.id != id
            {
                return Err(ProjectError::Invalid(format!(
                    "tag name already exists: {name}"
                )));
            }
        }
        let affected = self.conn.execute(
            "UPDATE tags SET name = IFNULL(?1, name), color = IFNULL(?2, color) WHERE id = ?3",
            rusqlite::params![name.map(str::trim), color, id],
        )?;
        if affected == 0 {
            return Err(not_found("tag"));
        }
        Ok(())
    }

    /// 删除标签（`task_tags` 走 `ON DELETE CASCADE`，任务本身不受影响）。
    pub fn delete_tag(&self, id: i64) -> Result<()> {
        let affected = self.conn.execute("DELETE FROM tags WHERE id = ?1", [id])?;
        if affected == 0 {
            return Err(not_found("tag"));
        }
        Ok(())
    }

    /// 改收藏（左栏标签段「收藏在前」的开关；排序口径在 [`Self::tag_usage`] 的
    /// `ORDER BY` 里，界面只拨这个开关、不自己排）。
    pub fn set_tag_favorite(&self, id: i64, favorite: bool) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE tags SET is_favorite = ?1 WHERE id = ?2",
            rusqlite::params![i64::from(favorite), id],
        )?;
        if affected == 0 {
            return Err(not_found("tag"));
        }
        Ok(())
    }

    /// 合并：把 `from` 上的关系全部搬到 `into`，再删掉 `from`。单事务。
    ///
    /// 返回搬走的关系条数（同一任务原本两个标签都有时只算一条 —— `INSERT OR IGNORE`）。
    pub fn merge_tags(&self, from: &[i64], into: i64) -> Result<u32> {
        if self.find_tag(into)?.is_none() {
            return Err(not_found("tag"));
        }
        let sources: Vec<i64> = from.iter().copied().filter(|id| *id != into).collect();
        if sources.is_empty() {
            return Ok(0);
        }
        let transaction = self.conn.unchecked_transaction()?;
        let mut moved = 0_u32;
        {
            let mut insert = transaction.prepare(
                "INSERT OR IGNORE INTO task_tags(task_id, tag_id) \
                 SELECT task_id, ?1 FROM task_tags WHERE tag_id = ?2",
            )?;
            let mut delete_source = transaction.prepare("DELETE FROM tags WHERE id = ?1")?;
            for source in sources {
                insert.execute(rusqlite::params![into, source])?;
                // `changes()` 是「本连接上一条语句影响的行数」：紧跟在 INSERT 后面读，
                // 拿到的就是真正插进去、没被 OR IGNORE 忽略掉的条数。
                moved += transaction.changes() as u32;
                delete_source.execute([source])?;
            }
        }
        transaction.commit()?;
        Ok(moved)
    }

    /// 某个任务的标签（按名称升序）。
    pub fn tags_of_task(&self, task_id: i64) -> Result<Vec<Tag>> {
        let mut statement = self.conn.prepare(
            "SELECT g.id, g.name, g.color, g.is_favorite FROM task_tags tt JOIN tags g ON g.id = tt.tag_id \
             WHERE tt.task_id = ?1 ORDER BY g.name COLLATE NOCASE ASC",
        )?;
        let mut rows = statement.query([task_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(tag_from_row(row)?);
        }
        Ok(out)
    }

    fn find_tag(&self, id: i64) -> Result<Option<Tag>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, name, color, is_favorite FROM tags WHERE id = ?1")?;
        let mut rows = statement.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(tag_from_row(row)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectStore;
    use crate::error::{ProjectError, Result};
    use crate::models::{NewTag, NewTask, TaskStatus};

    #[test]
    fn favorite_orders_first_and_persists() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.create_tag(NewTag::new("后端"))?;
        let frontend = store.create_tag(NewTag::new("前端"))?;
        // 默认都按名称排（前端在后端之后？不 —— 按码点序「前端」<「后端」）。
        assert_eq!(store.list_tags()?[0].name, "前端");
        assert!(!store.list_tags()?[0].is_favorite);

        store.set_tag_favorite(frontend, true)?;
        // 收藏的排前面；`ORDER BY` 里 DESC 优先于名称。
        let tags = store.list_tags()?;
        assert_eq!(tags[0].name, "前端");
        assert!(tags[0].is_favorite);
        assert!(!tags[1].is_favorite);

        // 取消收藏后回到纯名称序；关掉不存在的标签照样报 NotFound。
        store.set_tag_favorite(frontend, false)?;
        assert!(!store.list_tags()?[0].is_favorite);
        assert!(matches!(
            store.set_tag_favorite(9999, true),
            Err(ProjectError::NotFound("tag"))
        ));
        Ok(())
    }

    #[test]
    fn favorite_survives_merge_and_list_tags_default() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let from = store.create_tag(NewTag::new("旧名"))?;
        let into = store.create_tag(NewTag::new("新名"))?;
        store.set_tag_favorite(from, true)?;
        let task = store.create_task(NewTask::new("a").tags([from]))?;
        store.merge_tags(&[from], into)?;
        // 合并后 `from` 没了，收藏标记随行消失 —— 收藏是**标签**的属性，不搬到目标。
        let tags = store.list_tags()?;
        assert_eq!(tags.len(), 1);
        assert!(!tags[0].is_favorite);
        assert_eq!(store.tags_of_task(task)?[0].name, "新名");
        Ok(())
    }

    #[test]
    fn create_list_update_delete() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let front = store.create_tag(NewTag::new("  前端  ").color("#FF0000"))?;
        store.create_tag(NewTag::new("后端"))?;
        let tags = store.list_tags()?;
        assert_eq!(tags.len(), 2);
        // 中文按 **UTF-8 字节序**（= 码点序）排：「前端」(U+524D) 在「后端」(U+540E) 之前。
        // SQLite 的 NOCASE 只折叠 ASCII，对中文就是二进制比较。
        assert_eq!(tags[0].name, "前端");
        assert_eq!(tags[1].name, "后端");
        assert_eq!(tags[0].color, "#FF0000");

        store.update_tag(front, Some("前端开发"), None)?;
        assert_eq!(store.list_tags()?[0].name, "前端开发");
        store.delete_tag(front)?;
        assert_eq!(store.list_tags()?.len(), 1);
        assert!(matches!(
            store.delete_tag(front),
            Err(ProjectError::NotFound("tag"))
        ));
        Ok(())
    }

    #[test]
    fn duplicate_names_are_rejected_but_ensure_tag_reuses() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let first = store.create_tag(NewTag::new("后端"))?;
        assert!(matches!(
            store.create_tag(NewTag::new("后端")),
            Err(ProjectError::Invalid(_))
        ));
        let again = store.ensure_tag("后端", "#000000")?;
        assert_eq!(again, first, "ensure_tag 应复用已有标签而不是新建");
        assert_eq!(store.list_tags()?.len(), 1);
        // 改名撞车也要拦下。
        let other = store.create_tag(NewTag::new("前端"))?;
        assert!(matches!(
            store.update_tag(other, Some("后端"), None),
            Err(ProjectError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn usage_counts_only_live_tasks() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let tag = store.create_tag(NewTag::new("后端"))?;
        store.create_task(NewTask::new("a").tags([tag]))?;
        let archived = store.create_task(NewTask::new("b").tags([tag]))?;
        assert_eq!(store.list_tags()?[0].usage_count, 2);
        store.archive_task(archived)?;
        assert_eq!(
            store.list_tags()?[0].usage_count,
            1,
            "归档的任务不该再计入标签使用次数"
        );
        assert_eq!(
            store
                .list_tasks(&crate::query::TaskQuery::new().tags([tag]))?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn merging_moves_relations_and_drops_sources() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let front = store.create_tag(NewTag::new("前端"))?;
        let back = store.create_tag(NewTag::new("后端"))?;
        let target = store.create_tag(NewTag::new("工程"))?;
        let a = store.create_task(NewTask::new("a").tags([front, target]))?;
        let b = store.create_task(NewTask::new("b").tags([back]))?;

        store.merge_tags(&[front, back], target)?;
        let tags = store.list_tags()?;
        assert_eq!(tags.len(), 1, "被合并的标签应被删除");
        assert_eq!(
            store.get_task(a)?.unwrap().tags.len(),
            1,
            "本来就有目标标签的不重复"
        );
        assert_eq!(store.get_task(b)?.unwrap().tags[0].name, "工程");
        store.delete_tag(target)?;
        assert_eq!(store.get_task(a)?.unwrap().tags.len(), 0);
        assert_eq!(store.get_task(b)?.unwrap().tags.len(), 0);
        Ok(())
    }

    #[test]
    fn merging_into_a_missing_tag_is_not_found() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let tag = store.create_tag(NewTag::new("后端"))?;
        assert!(matches!(
            store.merge_tags(&[tag], 9_999),
            Err(ProjectError::NotFound("tag"))
        ));
        Ok(())
    }

    #[test]
    fn completed_tasks_still_count_as_tag_usage() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let tag = store.create_tag(NewTag::new("后端"))?;
        store.create_task(NewTask::new("a").tags([tag]).status(TaskStatus::Done))?;
        assert_eq!(store.list_tags()?[0].usage_count, 1, "完成 ≠ 归档，仍计次");
        Ok(())
    }
}
