//! 工作日志（`work_logs` 表）。
//!
//! 表在 v1 就建好（`UNIQUE (date, kind)`），读写到这里才落地 —— 工作日志视图是它的
//! 第一个调用方。口径：
//!
//! - **每天一条日报**（`kind = 'daily'`）：重复保存走 `upsert`，不会堆出两行；
//! - `linked_task_ids` 是 JSON 数组文本（v1 schema 钉死的形态），读出来解析失败按
//!   空表处理 —— 外部工具改坏不该让整页打不开；
//! - `tags` 是 v3 加的**自由标签**列（逗号分隔，见 `store::join_tags`）；
//! - `ai_generated` 只是记录来源标记，界面据实展示，不影响任何行为。
//!
//! ⚠️ `kind = 'plan'` 这一档**已废弃**（v3 起周计划独立成表，见 `store::plans`）。
//! 旧库里已有的行仍读得出来并按原样显示，本文件不做迁移 —— 迁移要猜「这条 plan
//! 属于哪一周」，猜错会把用户的承诺挪到别的周。

use crate::error::Result;
use crate::models::{LogKind, WorkLog};
use crate::store::{
    ProjectStore, date_column, id_list_to_wire, join_tags, normalize_tags, parse_id_list,
    split_tags,
};
use crate::time::CalendarDate;

impl ProjectStore {
    /// 某个日期区间内的日志（含端点），按日期倒序（最近的在前）。
    pub fn list_logs_between(&self, from: CalendarDate, to: CalendarDate) -> Result<Vec<WorkLog>> {
        let sql = "SELECT id, date, kind, content_md, ai_generated, linked_task_ids, tags \
             FROM work_logs WHERE date >= ?1 AND date <= ?2 \
             ORDER BY date DESC, kind ASC";
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query(rusqlite::params![from.to_iso(), to.to_iso()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(log_from_row(row)?);
        }
        Ok(out)
    }

    /// 某天某类日志；没有就是 `None`。
    pub fn get_log(&self, date: CalendarDate, kind: LogKind) -> Result<Option<WorkLog>> {
        let sql = "SELECT id, date, kind, content_md, ai_generated, linked_task_ids, tags \
             FROM work_logs WHERE date = ?1 AND kind = ?2";
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query(rusqlite::params![date.to_iso(), kind.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(log_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 写某天某类的日志（存在就整体覆盖，`UNIQUE (date, kind)` 兜底）。
    ///
    /// `linked_task_ids` 由调用方给（AI 小结里勾过的任务）；手工日志传空表。
    /// `tags` 走 [`normalize_tags`] 清洗（丢空项、丢含逗号的项、去重）。
    pub fn upsert_log(
        &self,
        date: CalendarDate,
        kind: LogKind,
        content_md: &str,
        ai_generated: bool,
        linked_task_ids: &[i64],
        tags: &[String],
    ) -> Result<()> {
        let linked = id_list_to_wire(linked_task_ids);
        let tags = join_tags(&normalize_tags(tags));
        self.conn.execute(
            "INSERT INTO work_logs(date, kind, content_md, ai_generated, linked_task_ids, tags) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(date, kind) DO UPDATE SET \
             content_md = excluded.content_md, \
             ai_generated = excluded.ai_generated, \
             linked_task_ids = excluded.linked_task_ids, \
             tags = excluded.tags",
            rusqlite::params![
                date.to_iso(),
                kind.as_str(),
                content_md,
                i64::from(ai_generated),
                linked,
                tags,
            ],
        )?;
        Ok(())
    }

    /// 删除某天某类的日志。
    pub fn delete_log(&self, date: CalendarDate, kind: LogKind) -> Result<()> {
        self.conn.execute(
            "DELETE FROM work_logs WHERE date = ?1 AND kind = ?2",
            rusqlite::params![date.to_iso(), kind.as_str()],
        )?;
        Ok(())
    }
}

/// 一行 `work_logs` → [`WorkLog`]。
fn log_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkLog> {
    let kind_raw: String = row.get(2)?;
    let linked_raw: String = row.get(5)?;
    let tags_raw: String = row.get(6)?;
    Ok(WorkLog {
        id: row.get(0)?,
        date: date_column(Some(row.get(1)?))
            .unwrap_or_else(|| CalendarDate::from_days_since_epoch(0)),
        kind: LogKind::parse(&kind_raw).unwrap_or(LogKind::Daily),
        content_md: row.get(3)?,
        ai_generated: row.get::<_, i64>(4)? != 0,
        linked_task_ids: parse_id_list(&linked_raw),
        tags: split_tags(&tags_raw),
    })
}

#[cfg(test)]
mod tests {
    use crate::error::Result;
    use crate::models::LogKind;
    use crate::store::ProjectStore;
    use crate::time::CalendarDate;

    fn date(day: u32) -> CalendarDate {
        CalendarDate::new(2026, 10, day).expect("合法日期")
    }

    #[test]
    fn upsert_then_read_round_trips() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_log(
            date(14),
            LogKind::Daily,
            "今天修了网关",
            false,
            &[3, 7],
            &["线上事故".to_owned()],
        )?;
        let log = store
            .get_log(date(14), LogKind::Daily)?
            .expect("刚写的日志");
        assert_eq!(log.content_md, "今天修了网关");
        assert!(!log.ai_generated);
        assert_eq!(log.linked_task_ids, vec![3, 7]);
        assert_eq!(log.tags, ["线上事故"]);

        // 同天再写是覆盖，不是新增一行。
        store.upsert_log(
            date(14),
            LogKind::Daily,
            "改口：修了两个网关",
            true,
            &[],
            &[],
        )?;
        let log = store.get_log(date(14), LogKind::Daily)?.expect("仍在");
        assert_eq!(log.content_md, "改口：修了两个网关");
        assert!(log.ai_generated);
        assert!(log.linked_task_ids.is_empty());
        assert!(log.tags.is_empty(), "覆盖写要把标签一起换掉");
        Ok(())
    }

    #[test]
    fn list_between_orders_by_date_desc() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_log(date(12), LogKind::Daily, "周二", false, &[], &[])?;
        store.upsert_log(date(14), LogKind::Daily, "周四", false, &[], &[])?;
        store.upsert_log(date(13), LogKind::Weekly, "周报", false, &[], &[])?;
        let logs = store.list_logs_between(date(12), date(14))?;
        let dates: Vec<String> = logs.iter().map(|log| log.date.to_iso()).collect();
        assert_eq!(dates, ["2026-10-14", "2026-10-13", "2026-10-12"]);
        Ok(())
    }

    #[test]
    fn delete_removes_only_that_day_and_kind() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_log(date(14), LogKind::Daily, "日报", false, &[], &[])?;
        store.upsert_log(date(14), LogKind::Plan, "计划", false, &[], &[])?;
        store.delete_log(date(14), LogKind::Daily)?;
        assert!(store.get_log(date(14), LogKind::Daily)?.is_none());
        assert!(store.get_log(date(14), LogKind::Plan)?.is_some());
        Ok(())
    }

    /// 标签在入口就被清洗：空项、含逗号的项、重复项都进不了库。
    #[test]
    fn tags_are_normalized_before_landing() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_log(
            date(14),
            LogKind::Daily,
            "正文",
            false,
            &[],
            &[
                "  复盘  ".to_owned(),
                String::new(),
                "   ".to_owned(),
                // 逗号会破坏「逗号分隔」这条不变量，整项丢掉而不是转义。
                "a,b".to_owned(),
                "复盘".to_owned(),
                "线上".to_owned(),
            ],
        )?;
        let log = store.get_log(date(14), LogKind::Daily)?.expect("写入成功");
        assert_eq!(log.tags, ["复盘", "线上"]);
        Ok(())
    }
}
