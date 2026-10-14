//! 看板列的自定义（拍板 Q2：可改名、可新增、可调 WIP 上限、可排序）。
//!
//! # 空表 = 用内置默认列
//!
//! 默认四列（待办 / 进行中 / 阻塞 / 已完成）**不落库**。理由有两条，都不是洁癖：
//!
//! 1. 内核里不出现界面文案 —— 默认列名由 `duck-app` 按当前语言取词，落库就等于把
//!    中文写进内核（i18n 守卫会拦下，而且英文用户会看到四个中文列名）。
//! 2. 「没改过」与「改回了原样」是两回事：前者应当跟着语言走，后者是用户的明确选择。
//!
//! 所以：表里有行 = 用户定制过（`name` 为 `NULL` 的那几列仍走内置文案），表为空 =
//! 全用内置默认。`reset_columns` 就是把这个表清空。

use crate::error::{ProjectError, Result};
use crate::models::{BoardColumn, ColumnSpec, TaskStatus};
use crate::store::{ProjectStore, column_from_row};

impl ProjectStore {
    /// 看板列。表为空时回落 [`ProjectStore::default_columns`]。
    pub fn list_columns(&self) -> Result<Vec<BoardColumn>> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, status_key, sort_order, wip_limit FROM board_columns \
             ORDER BY sort_order ASC, id ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            // 状态键认不出来的行直接跳过（旧库野值不该让整块看板打不开）。
            if let Some(column) = column_from_row(row)? {
                out.push(column);
            }
        }
        if out.is_empty() {
            return Ok(Self::default_columns());
        }
        Ok(out)
    }

    /// 整表替换（列设置弹窗保存时调）。单事务：先清后写，同事务 ——
    /// 分两次写的话，中间失败就会把看板列全清空。
    ///
    /// 返回落库的列数。传空数组等价于 [`Self::reset_columns`]。
    pub fn replace_columns(&self, columns: &[ColumnSpec]) -> Result<u32> {
        for (index, column) in columns.iter().enumerate() {
            if let Some(limit) = column.wip_limit
                && limit < 0
            {
                return Err(ProjectError::Invalid(format!(
                    "column {index} has a negative wip_limit"
                )));
            }
        }
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute("DELETE FROM board_columns", [])?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO board_columns(name, status_key, sort_order, wip_limit) \
                 VALUES(?1, ?2, ?3, ?4)",
            )?;
            for (index, column) in columns.iter().enumerate() {
                statement.execute(rusqlite::params![
                    column.name,
                    column.status_key.as_str(),
                    index as i64,
                    column.wip_limit,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(columns.len() as u32)
    }

    /// 回到内置默认四列（清空定制）。
    pub fn reset_columns(&self) -> Result<()> {
        self.conn.execute("DELETE FROM board_columns", [])?;
        Ok(())
    }

    /// 当前是否处于「未定制」状态。
    pub fn columns_are_default(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM board_columns", [], |row| row.get(0))?;
        Ok(count == 0)
    }
}

/// 由内置状态顺序造一份默认列定义。
#[must_use]
pub fn default_column_specs() -> Vec<ColumnSpec> {
    TaskStatus::DEFAULT_BOARD
        .iter()
        .map(|status| ColumnSpec::new(*status))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ProjectStore, default_column_specs};
    use crate::error::{ProjectError, Result};
    use crate::models::{ColumnSpec, TaskStatus};

    #[test]
    fn empty_table_falls_back_to_built_in_defaults() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        assert!(store.columns_are_default()?);
        let columns = store.list_columns()?;
        let keys: Vec<&str> = columns.iter().map(|c| c.status_key.as_str()).collect();
        assert_eq!(keys, ["todo", "doing", "blocked", "done"]);
        assert!(columns.iter().all(|c| c.wip_limit.is_none()));
        assert_eq!(store.list_columns()?, ProjectStore::default_columns());
        Ok(())
    }

    #[test]
    fn replace_then_read_back() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let specs = vec![
            ColumnSpec::new(TaskStatus::Todo)
                .name("待处理")
                .wip_limit(5),
            ColumnSpec::new(TaskStatus::Doing).name("做着呢"),
            ColumnSpec::new(TaskStatus::Done).name("搞定了"),
        ];
        assert_eq!(store.replace_columns(&specs)?, 3);
        assert!(!store.columns_are_default()?);

        let columns = store.list_columns()?;
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].name.as_deref(), Some("待处理"));
        assert_eq!(columns[0].wip_limit, Some(5));
        assert_eq!(columns[2].status_key, TaskStatus::Done);
        assert!(columns[0].sort_order < columns[1].sort_order);
        Ok(())
    }

    #[test]
    fn replace_is_a_full_swap_not_a_merge() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.replace_columns(&default_column_specs())?;
        assert_eq!(store.list_columns()?.len(), 4);
        // 第二次只给两列：第一次的四列必须全消失，否则会留下孤儿列。
        store.replace_columns(&[
            ColumnSpec::new(TaskStatus::Todo),
            ColumnSpec::new(TaskStatus::Done),
        ])?;
        assert_eq!(store.list_columns()?.len(), 2);
        Ok(())
    }

    #[test]
    fn reset_brings_back_defaults() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.replace_columns(&[ColumnSpec::new(TaskStatus::Doing)])?;
        assert_eq!(store.list_columns()?.len(), 1);
        store.reset_columns()?;
        assert!(store.columns_are_default()?);
        assert_eq!(store.list_columns()?.len(), 4);
        // 空数组 = 重置。
        store.replace_columns(&[ColumnSpec::new(TaskStatus::Doing)])?;
        store.replace_columns(&[])?;
        assert_eq!(store.list_columns()?.len(), 4);
        Ok(())
    }

    #[test]
    fn a_column_may_keep_the_built_in_name() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        // 只改 WIP 上限、不改名：`name` 留 NULL，界面继续用内置文案。
        store.replace_columns(&[
            ColumnSpec::new(TaskStatus::Todo).wip_limit(3),
            ColumnSpec::new(TaskStatus::Doing),
        ])?;
        let columns = store.list_columns()?;
        assert_eq!(columns[0].name, None);
        assert_eq!(columns[0].wip_limit, Some(3));
        Ok(())
    }

    #[test]
    fn negative_wip_limit_is_rejected() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        let error = store
            .replace_columns(&[ColumnSpec::new(TaskStatus::Todo).wip_limit(-1)])
            .expect_err("负的 WIP 上限应报错");
        assert!(matches!(error, ProjectError::Invalid(_)));
        assert!(store.columns_are_default()?, "校验失败不应留下半套列");
        Ok(())
    }

    #[test]
    fn unknown_status_keys_are_skipped_not_fatal() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.replace_columns(&[ColumnSpec::new(TaskStatus::Todo)])?;
        // 模拟外部工具塞了一行认不出来的状态键。
        store.conn.execute(
            "INSERT INTO board_columns(name, status_key, sort_order) VALUES('x', 'nope', 9)",
            [],
        )?;
        let columns = store.list_columns()?;
        assert_eq!(columns.len(), 1, "野值行跳过，不该整块看板打不开");
        assert_eq!(columns[0].status_key, TaskStatus::Todo);
        Ok(())
    }
}
