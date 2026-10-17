//! 周计划（`weekly_plans` 表，v3 建的）。
//!
//! 与工作日志分开的原因见 `crate::db` 的 v3 迁移注释（键不同 / 多两列语义 / 有状态机）。
//! 本文件只落三条口径：
//!
//! 1. **一行一周**。`week_key`（`YYYY-Www`）是唯一键，写入口一律 upsert ——
//!    「同一周改两次」在库里就只有一行。写入前校验周键，非法键直接报 `Invalid`：
//!    让它落库的话，那一行永远没有任何日期能对上（界面上就是「计划凭空消失」）。
//! 2. **`original_task_ids` 首次定型后不动**。第一次以**非空**任务集保存时把它定成
//!    当时的 `task_ids`，此后再加任务只改 `task_ids`。两者之差就是「计划 vs 实际」的
//!    「后来加的」那一栏（[`WeeklyPlan::added_task_ids`]）。
//!
//!    为什么不是「第一次保存就定型」：用户完全可能先点开周计划、写两句正文就保存，
//!    此时任务集还是空的。若那时定型，之后把这一周的活排进去会**全部**被标成
//!    「后来加的」—— 对比栏就没有意义了。
//! 3. **`status` 是独立动作**（[`ProjectStore::set_plan_status`]），不跟着内容保存走。
//!    「确认」表达的是对那一周内容的承诺，写正文时顺手把它改回 `draft` 会让确认
//!    变成一次性的。

use crate::error::{ProjectError, Result};
use crate::models::{WeeklyPlan, WeeklyPlanStatus};
use crate::store::{ProjectStore, id_list_to_wire, not_found, parse_id_list};
use crate::time::now_seconds;

/// `weekly_plans` 的列清单，顺序与 [`plan_from_row`] 的下标严格对应。
const PLAN_COLUMNS: &str = "id, week_key, content_md, task_ids, original_task_ids, status, \
     created_at, updated_at";

impl ProjectStore {
    /// 某一周的计划；没写过就是 `None`。
    pub fn get_plan(&self, week_key: &str) -> Result<Option<WeeklyPlan>> {
        let sql = format!("SELECT {PLAN_COLUMNS} FROM weekly_plans WHERE week_key = ?1");
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query([week_key])?;
        match rows.next()? {
            Some(row) => Ok(Some(plan_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 最近写过的若干周计划（按周键倒序 = 时间倒序）。
    ///
    /// `limit` 传 0 时返回空表（**不是**「不限」）：这个方法服务的是「周计划清单」，
    /// 界面上总有个「看最近几周」的上限，返回全表只会在用了几年之后变成一次几万行读。
    /// 要全表就传一个大数。
    pub fn list_plans(&self, limit: usize) -> Result<Vec<WeeklyPlan>> {
        let sql =
            format!("SELECT {PLAN_COLUMNS} FROM weekly_plans ORDER BY week_key DESC LIMIT ?1");
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query([i64::try_from(limit).unwrap_or(i64::MAX)])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(plan_from_row(row)?);
        }
        Ok(out)
    }

    /// 写某一周的计划（存在就整体覆盖）。
    ///
    /// `original_task_ids` 的定型规则见模块注释第 2 条 —— 用 SQL 的 `CASE` 在一条语句里
    /// 表达，而不是「先读一行、判断、再写」：后者在两次调用之间留了一个窗口，
    /// 而这里正是「一周一行」的写入口。
    ///
    /// `week_key` 必须是 [`crate::time::CalendarDate::from_week_key`] 认得的形态。
    pub fn upsert_plan(
        &self,
        week_key: &str,
        content_md: &str,
        task_ids: &[i64],
        status: WeeklyPlanStatus,
    ) -> Result<()> {
        check_week_key(week_key)?;
        let now = now_seconds();
        let tasks = id_list_to_wire(task_ids);
        self.conn.execute(
            "INSERT INTO weekly_plans(week_key, content_md, task_ids, original_task_ids, status, \
             created_at, updated_at) \
             VALUES(?1, ?2, ?3, ?3, ?4, ?5, ?5) \
             ON CONFLICT(week_key) DO UPDATE SET \
             content_md = excluded.content_md, \
             task_ids = excluded.task_ids, \
             original_task_ids = CASE \
                 WHEN weekly_plans.original_task_ids = '[]' AND excluded.task_ids <> '[]' \
                 THEN excluded.task_ids \
                 ELSE weekly_plans.original_task_ids END, \
             status = excluded.status, \
             updated_at = excluded.updated_at",
            rusqlite::params![week_key, content_md, tasks, status.as_str(), now],
        )?;
        Ok(())
    }

    /// 只改状态（「确认」/「改回草稿」），不动内容与任务集。
    ///
    /// 那一周还没写过计划时报 [`ProjectError::NotFound`]：先有内容才谈得上承诺，
    /// 静默成功会让界面上的状态徽章指向一份不存在的计划。
    pub fn set_plan_status(&self, week_key: &str, status: WeeklyPlanStatus) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE weekly_plans SET status = ?1, updated_at = ?2 WHERE week_key = ?3",
            rusqlite::params![status.as_str(), now_seconds(), week_key],
        )?;
        if affected == 0 {
            return Err(not_found("weekly plan"));
        }
        Ok(())
    }

    /// 删掉某一周的计划。与 [`Self::delete_task`] 同口径：删不存在的行报 `NotFound`。
    pub fn delete_plan(&self, week_key: &str) -> Result<()> {
        let affected = self
            .conn
            .execute("DELETE FROM weekly_plans WHERE week_key = ?1", [week_key])?;
        if affected == 0 {
            return Err(not_found("weekly plan"));
        }
        Ok(())
    }
}

/// 写入前的周键校验。
///
/// 走 [`crate::time::CalendarDate::from_week_key`] 而不是「正则看着像」：那个函数会
/// 回环校验（`2025-W53` 语法合法但 2025 只有 52 周），是唯一能挡住「年周不存在」的地方。
fn check_week_key(week_key: &str) -> Result<()> {
    if crate::time::CalendarDate::from_week_key(week_key).is_none() {
        return Err(ProjectError::Invalid(format!(
            "week key must look like YYYY-Www and exist: {week_key}"
        )));
    }
    Ok(())
}

/// 一行 `weekly_plans` → [`WeeklyPlan`]。
fn plan_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WeeklyPlan> {
    let status_raw: String = row.get(5)?;
    let tasks_raw: String = row.get(3)?;
    let original_raw: String = row.get(4)?;
    Ok(WeeklyPlan {
        id: row.get(0)?,
        week_key: row.get(1)?,
        content_md: row.get(2)?,
        task_ids: parse_id_list(&tasks_raw),
        original_task_ids: parse_id_list(&original_raw),
        // 状态列被外部改成野值：按 `draft` 处理（未确认）比按 `confirmed` 安全 ——
        // 界面上少一个「已确认」徽章不会让人误判，多一个才会。
        status: WeeklyPlanStatus::parse(&status_raw).unwrap_or_default(),
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::error::Result;
    use crate::models::WeeklyPlanStatus;
    use crate::store::ProjectStore;

    #[test]
    fn missing_plan_is_none_and_write_then_read_round_trips() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        assert!(store.get_plan("2026-W42")?.is_none(), "没写过就是 None");

        store.upsert_plan(
            "2026-W42",
            "本周把网关切完",
            &[11, 12],
            WeeklyPlanStatus::Draft,
        )?;
        let plan = store.get_plan("2026-W42")?.expect("刚写的计划");
        assert_eq!(plan.week_key, "2026-W42");
        assert_eq!(plan.content_md, "本周把网关切完");
        assert_eq!(plan.task_ids, vec![11, 12]);
        assert_eq!(
            plan.original_task_ids,
            vec![11, 12],
            "首次以非空集保存即定型"
        );
        assert_eq!(plan.status, WeeklyPlanStatus::Draft);
        assert!(plan.created_at > 0);
        assert!(plan.updated_at >= plan.created_at);
        Ok(())
    }

    /// 同周再写是覆盖，且 `original_task_ids` 定住不动 —— 「后来加的」靠它算出来。
    #[test]
    fn rewriting_keeps_the_original_task_ids() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_plan("2026-W42", "初稿", &[11, 12], WeeklyPlanStatus::Draft)?;
        store.upsert_plan(
            "2026-W42",
            "加了 13",
            &[11, 12, 13],
            WeeklyPlanStatus::Confirmed,
        )?;

        let plan = store.get_plan("2026-W42")?.expect("仍在");
        assert_eq!(plan.content_md, "加了 13");
        assert_eq!(plan.task_ids, vec![11, 12, 13]);
        assert_eq!(plan.original_task_ids, vec![11, 12], "定型之后不再回头改");
        assert_eq!(plan.added_task_ids(), vec![13]);
        assert_eq!(plan.planned_task_ids(), vec![11, 12]);
        assert!(plan.is_confirmed());

        // 只留下一行。
        assert_eq!(store.list_plans(10)?.len(), 1);
        Ok(())
    }

    /// 先存空任务集的正文不会把「定型」浪费掉（模块注释第 2 条的正面用例）。
    #[test]
    fn an_empty_task_set_does_not_freeze_the_original_ids() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_plan("2026-W42", "先写两句", &[], WeeklyPlanStatus::Draft)?;
        assert!(
            store
                .get_plan("2026-W42")?
                .expect("在")
                .original_task_ids
                .is_empty()
        );
        store.upsert_plan("2026-W42", "排活", &[7, 8], WeeklyPlanStatus::Draft)?;
        let plan = store.get_plan("2026-W42")?.expect("在");
        assert_eq!(
            plan.original_task_ids,
            vec![7, 8],
            "第一次拿到非空任务集时定型"
        );
        assert!(plan.added_task_ids().is_empty(), "它们不算「后来加的」");
        Ok(())
    }

    #[test]
    fn list_plans_is_week_key_desc_and_respects_the_limit() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        for key in ["2026-W40", "2026-W42", "2026-W41"] {
            store.upsert_plan(key, "", &[], WeeklyPlanStatus::Draft)?;
        }
        let keys: Vec<String> = store
            .list_plans(10)?
            .into_iter()
            .map(|plan| plan.week_key)
            .collect();
        assert_eq!(keys, ["2026-W42", "2026-W41", "2026-W40"]);
        assert_eq!(store.list_plans(2)?.len(), 2, "最近两周");
        assert!(store.list_plans(0)?.is_empty(), "0 是空表而不是不限");
        Ok(())
    }

    #[test]
    fn status_can_be_set_apart_from_content() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_plan("2026-W42", "正文", &[1], WeeklyPlanStatus::Draft)?;
        store.set_plan_status("2026-W42", WeeklyPlanStatus::Confirmed)?;
        let plan = store.get_plan("2026-W42")?.expect("在");
        assert!(plan.is_confirmed());
        assert_eq!(plan.content_md, "正文", "改状态不动内容");
        assert_eq!(plan.task_ids, vec![1], "也不动任务集");

        store.set_plan_status("2026-W42", WeeklyPlanStatus::Draft)?;
        assert!(!store.get_plan("2026-W42")?.expect("在").is_confirmed());

        // 没有那一周时报错，而不是静默成功。
        assert!(
            store
                .set_plan_status("2026-W43", WeeklyPlanStatus::Confirmed)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn delete_removes_the_week_and_reports_a_missing_one() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_plan("2026-W42", "正文", &[1], WeeklyPlanStatus::Draft)?;
        store.delete_plan("2026-W42")?;
        assert!(store.get_plan("2026-W42")?.is_none());
        assert!(store.delete_plan("2026-W42").is_err(), "删不存在的行要报错");
        Ok(())
    }

    /// 非法周键必须在**写入前**被拦下：落进去的行永远没有日期能对上。
    #[test]
    fn malformed_week_keys_are_rejected() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        for junk in ["", "2026W42", "2026-w42", "2026-W00", "2026-W54"] {
            assert!(
                store
                    .upsert_plan(junk, "", &[], WeeklyPlanStatus::Draft)
                    .is_err(),
                "{junk:?} 不该被写进去"
            );
        }
        // 语法对但年周不存在：2025 年只有 52 周。
        assert!(
            store
                .upsert_plan("2025-W53", "", &[], WeeklyPlanStatus::Draft)
                .is_err()
        );
        assert!(store.list_plans(10)?.is_empty(), "一条都没落库");
        Ok(())
    }

    /// 坏行不该让整份清单打不开：状态列被外部改成野值按「未确认」读。
    ///
    /// 走 [`ProjectStore::exec_raw`] 伪造外部写入 —— 常规写口的 `status` 一定是
    /// 合法键，这条容错分支只能这么测。
    #[test]
    fn unknown_status_reads_as_draft() -> Result<()> {
        let store = ProjectStore::open_in_memory()?;
        store.upsert_plan("2026-W42", "正文", &[1, 2], WeeklyPlanStatus::Confirmed)?;
        store.exec_raw("UPDATE weekly_plans SET status = 'wat', task_ids = '[1,oops,2]'")?;

        let plan = store.get_plan("2026-W42")?.expect("仍在");
        assert_eq!(plan.status, WeeklyPlanStatus::Draft, "野值按未确认读");
        assert_eq!(plan.task_ids, vec![1, 2], "半截 JSON 里能读出来的照样读");
        assert_eq!(plan.content_md, "正文", "别的列不受影响");
        Ok(())
    }
}
