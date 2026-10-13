//! `pending_actions` 的读写（D1：动作队列）。
//!
//! # 状态机（严格对齐 DDL 的 5 态）
//!
//! ```text
//!             cancel()
//! pending ──────────────► cancelled
//!    │  ▲
//!    │  │ mark_failed(not_before=Some)   （可重试：留在 pending，等退避到点）
//!    ▼  │
//! inflight ──┬─ mark_done() ──► done
//!            └─ mark_failed(not_before=None) ──► failed   （终态，不再重试）
//! ```
//!
//! **只暴露这四条迁移**：没有「从 done 改回 pending」这类后门。`cancel` 只允许从
//! `pending` 转（已 `inflight` / `done` 的动作不能再撤销——它可能已经在服务端生效）。
//!
//! # 顺序语义
//!
//! [`due`] 按 `created_at, id` **升序**返回：动作必须按用户操作顺序补发，否则
//! 「先移动后打标签」会变成「先打标签后移动」，标签落到错的文件夹上（`UID STORE`
//! 要求先 SELECT 对邮箱）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};

/// 动作的生命周期状态（对应 `pending_actions.state` 的 CHECK 集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionState {
    /// 已入队，等待补发。
    Pending,
    /// 正在执行（已从队列取出、还没落结论）。
    Inflight,
    /// 已完成。
    Done,
    /// 终态失败：重试次数超限，不再补发。
    Failed,
    /// 用户撤销 / 主动取消。
    Cancelled,
}

impl ActionState {
    /// 落盘值（稳定，别改）。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Inflight => "inflight",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 从落盘值恢复。DDL 的 CHECK 保证只会是这 5 个之一；真出现未知值时按
    /// [`ActionState::Pending`] 处理（`due()` 仍按 `state='pending'` 过滤，不会误发）。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "inflight" => Self::Inflight,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

/// 入队一条动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAction {
    /// 账户。
    pub account_id: i64,
    /// 动作种类（`queue::ActionKind` 的落盘值；本层不解释它）。
    pub kind: String,
    /// 目标邮件（可空：如将来的「全部标已读」）。
    pub message_id: Option<i64>,
    /// 目标线程（M1 无会话合并，留空）。
    pub thread_id: Option<i64>,
    /// 载荷（JSON 文本；本层不解释它）。
    pub payload: String,
    /// 最早可执行时间（`None` = 立即可执行）。撤销宽限期就用它表达。
    pub not_before: Option<i64>,
}

/// 从库里读出的一条动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAction {
    /// 主键。
    pub id: i64,
    /// 账户。
    pub account_id: i64,
    /// 动作种类。
    pub kind: String,
    /// 目标邮件。
    pub message_id: Option<i64>,
    /// 目标线程。
    pub thread_id: Option<i64>,
    /// 载荷（JSON 文本）。
    pub payload: String,
    /// 状态。
    pub state: ActionState,
    /// 已尝试次数。
    pub attempts: i64,
    /// 最早可执行时间。
    pub not_before: Option<i64>,
    /// 入队时间（排序键）。
    pub created_at: i64,
    /// 结束时间（`done` / `failed` / `cancelled` 时非空）。
    pub finished_at: Option<i64>,
    /// 最近一次失败原因。
    pub last_error: Option<String>,
}

/// 当前 Unix 秒。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 入队（`state='pending'`，`created_at=now`），返回新主键。
pub fn enqueue(conn: &Connection, action: &NewAction) -> Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
           (account_id, kind, message_id, thread_id, payload, state, attempts, not_before, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?7)",
        params![
            action.account_id,
            action.kind,
            action.message_id,
            action.thread_id,
            action.payload,
            action.not_before,
            now_secs(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 到点可执行的动作（`pending` 且 `not_before` 为空或已到），按入队顺序升序。
pub fn due(conn: &Connection, account_id: i64, limit: i64) -> Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, kind, message_id, thread_id, payload, state, attempts,
                not_before, created_at, finished_at, last_error
         FROM pending_actions
         WHERE account_id = ?1 AND state = 'pending'
           AND (not_before IS NULL OR not_before <= ?3)
         ORDER BY created_at, id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![account_id, limit, now_secs()], row_to_action)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 按主键取一条。
pub fn get(conn: &Connection, id: i64) -> Result<PendingAction> {
    conn.query_row(
        "SELECT id, account_id, kind, message_id, thread_id, payload, state, attempts,
                not_before, created_at, finished_at, last_error
         FROM pending_actions WHERE id = ?1",
        [id],
        row_to_action,
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("pending action {id}")))
}

/// 列某账户的动作（可选按状态过滤），按入队顺序升序。
pub fn list(
    conn: &Connection,
    account_id: i64,
    state: Option<ActionState>,
) -> Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, kind, message_id, thread_id, payload, state, attempts,
                not_before, created_at, finished_at, last_error
         FROM pending_actions
         WHERE account_id = ?1 AND (?2 IS NULL OR state = ?2)
         ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(
        params![account_id, state.map(ActionState::wire_name)],
        row_to_action,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// `pending` → `inflight`，**条件认领**：返回 `true` 表示本次调用真的抢到了这一行。
///
/// 为什么必须是条件更新（`AND state = 'pending'`）而且要看返回值：两条
/// [`crate::queue::flush_pending`] 可以真并发（它是 `runtime.spawn(...).wait()`）。
/// 时序——[A] 已把 `id=1` 认领成 `inflight` 正在网络往返 → [B] 进门先跑崩溃恢复
/// [`requeue_inflight`]，把 A 正在飞的那行**打回 `pending`** → [B] 的 [`due`] 又把它取到
/// → 若 [B] 不检查返回值就继续，这条动作会被**重复执行**。
///
/// 幂等动作（已读 / 旗标 / 标签）重复一次无害；`move` / `delete` **不幂等**：第二次
/// `UID MOVE` 会因 UID 已不在源文件夹而失败，留下一行假 `failed`，用户还会看到一次
/// 「同步操作失败」。所以认领失败的一方**必须跳过**（见 `queue::flush_inner`）。
pub fn mark_inflight(conn: &Connection, id: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE pending_actions SET state = 'inflight' WHERE id = ?1 AND state = 'pending'",
        [id],
    )?;
    Ok(changed > 0)
}

/// `inflight` → `done`。
pub fn mark_done(conn: &Connection, id: i64, finished_at: i64) -> Result<()> {
    conn.execute(
        "UPDATE pending_actions SET state = 'done', finished_at = ?2, last_error = NULL
         WHERE id = ?1",
        params![id, finished_at],
    )?;
    Ok(())
}

/// 记一次失败。
///
/// - `not_before = Some(t)`：**可重试**——留在 `pending`，写 `attempts` / `last_error`，
///   退避到 `t` 之后再由 [`due`] 取出；
/// - `not_before = None`：**放弃**——转 `failed`（终态），并把 `finished_at` 记上。
///
/// `attempts` 由调用方给（通常是旧值 + 1），本层不自己加——调用方才知道「第几次」。
pub fn mark_failed(
    conn: &Connection,
    id: i64,
    attempts: i64,
    last_error: &str,
    not_before: Option<i64>,
) -> Result<()> {
    match not_before {
        Some(t) => conn.execute(
            "UPDATE pending_actions
             SET state = 'pending', attempts = ?2, last_error = ?3, not_before = ?4
             WHERE id = ?1",
            params![id, attempts, last_error, t],
        )?,
        None => conn.execute(
            "UPDATE pending_actions
             SET state = 'failed', attempts = ?2, last_error = ?3, finished_at = ?4
             WHERE id = ?1",
            params![id, attempts, last_error, now_secs()],
        )?,
    };
    Ok(())
}

/// 崩溃恢复：把某账户卡在 `inflight` 的动作放回 `pending`，返回改了几条。
///
/// 为什么要它：补发过程若在 `mark_inflight` 之后中断（进程被杀 / 断电），这行会**永远
/// 停在 `inflight`**——[`due`] 只取 `pending`，它就再也不会被补发。所以每次开跑前把残留的
/// `inflight` 归位。
///
/// ⚠️ **前提不是「本进程唯一的执行者」**（早期注释这么写，是错的）：`flush_pending` 是
/// `runtime.spawn(...).wait()`，同账户的两条调用可以真并发。本函数无条件抢占该账户所有
/// `inflight`，会把**另一条正在网络往返的动作**也打回 `pending`。安全性由两处共同保证：
///
/// 1. 进程内闸门 [`crate::queue::try_begin_flush`] 让同账户的补发**互斥**（正常路径下
///    本函数进门时不可能有别的补发在飞）；
/// 2. [`mark_inflight`] 是**条件认领**——万一闸门失效（如跨进程写了同一个库），抢不到
///    的那一方会跳过而不是重复执行。
///
/// 两条都在时，本函数「多改了几行」的代价只是让那一方多做一次无谓的 `due()` 查询。
pub fn requeue_inflight(conn: &Connection, account_id: i64) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE pending_actions SET state = 'pending'
         WHERE account_id = ?1 AND state = 'inflight'",
        [account_id],
    )?;
    Ok(changed)
}

/// 撤销 / 取消：**只允许从 `pending` 转 `cancelled`**。返回是否真的取消了
/// （已 `inflight` / `done` / `failed` 的返回 `false`）。
pub fn cancel(conn: &Connection, id: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE pending_actions SET state = 'cancelled', finished_at = ?2
         WHERE id = ?1 AND state = 'pending'",
        params![id, now_secs()],
    )?;
    Ok(changed > 0)
}

fn row_to_action(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingAction> {
    let state: String = row.get(6)?;
    Ok(PendingAction {
        id: row.get(0)?,
        account_id: row.get(1)?,
        kind: row.get(2)?,
        message_id: row.get(3)?,
        thread_id: row.get(4)?,
        payload: row.get(5)?,
        state: ActionState::from_wire(&state),
        attempts: row.get(7)?,
        not_before: row.get(8)?,
        created_at: row.get(9)?,
        finished_at: row.get(10)?,
        last_error: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::accounts;
    use crate::models::{ConnectionSecurity, NewAccount};

    fn mem() -> (Connection, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let acct = accounts::insert(
            &conn,
            &NewAccount {
                email: "a@example.com".into(),
                display_name: None,
                username: "a@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        (conn, acct)
    }

    fn action(acct: i64, kind: &str) -> NewAction {
        NewAction {
            account_id: acct,
            kind: kind.into(),
            message_id: None,
            thread_id: None,
            payload: "{}".into(),
            not_before: None,
        }
    }

    #[test]
    fn enqueue_then_due_returns_it_in_order() {
        let (conn, acct) = mem();
        let first = enqueue(&conn, &action(acct, "star")).unwrap();
        let second = enqueue(&conn, &action(acct, "move")).unwrap();
        let due = due(&conn, acct, 10).unwrap();
        assert_eq!(
            due.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![first, second],
            "必须按入队顺序补发"
        );
    }

    #[test]
    fn due_respects_not_before() {
        let (conn, acct) = mem();
        let mut future = action(acct, "move");
        future.not_before = Some(now_secs() + 3600);
        enqueue(&conn, &future).unwrap();
        assert!(
            due(&conn, acct, 10).unwrap().is_empty(),
            "未到点的动作不该被取出"
        );

        let mut past = action(acct, "star");
        past.not_before = Some(now_secs() - 1);
        enqueue(&conn, &past).unwrap();
        assert_eq!(due(&conn, acct, 10).unwrap().len(), 1);
    }

    #[test]
    fn full_state_machine_pending_inflight_done() {
        let (conn, acct) = mem();
        let id = enqueue(&conn, &action(acct, "star")).unwrap();
        assert_eq!(get(&conn, id).unwrap().state, ActionState::Pending);

        mark_inflight(&conn, id).unwrap();
        assert_eq!(get(&conn, id).unwrap().state, ActionState::Inflight);
        // inflight 的不再出现在 due 里。
        assert!(due(&conn, acct, 10).unwrap().is_empty());

        mark_done(&conn, id, now_secs()).unwrap();
        let done = get(&conn, id).unwrap();
        assert_eq!(done.state, ActionState::Done);
        assert!(done.finished_at.is_some());
    }

    #[test]
    fn mark_failed_reschedules_or_gives_up() {
        let (conn, acct) = mem();
        let id = enqueue(&conn, &action(acct, "move")).unwrap();

        // 可重试：留在 pending，退避到未来。
        mark_failed(&conn, id, 1, "timeout", Some(now_secs() + 3600)).unwrap();
        let retry = get(&conn, id).unwrap();
        assert_eq!(retry.state, ActionState::Pending);
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.last_error.as_deref(), Some("timeout"));
        assert!(due(&conn, acct, 10).unwrap().is_empty());

        // 放弃：终态 failed。
        mark_failed(&conn, id, 5, "still broken", None).unwrap();
        let dead = get(&conn, id).unwrap();
        assert_eq!(dead.state, ActionState::Failed);
        assert!(dead.finished_at.is_some());
        assert!(due(&conn, acct, 10).unwrap().is_empty());
    }

    #[test]
    fn cancel_only_applies_to_pending() {
        let (conn, acct) = mem();
        let id = enqueue(&conn, &action(acct, "delete")).unwrap();
        assert!(cancel(&conn, id).unwrap(), "pending 可撤销");
        assert_eq!(get(&conn, id).unwrap().state, ActionState::Cancelled);
        // 再撤一次：已经不在 pending，返回 false。
        assert!(!cancel(&conn, id).unwrap());

        // inflight 的不可撤销。
        let other = enqueue(&conn, &action(acct, "star")).unwrap();
        mark_inflight(&conn, other).unwrap();
        assert!(!cancel(&conn, other).unwrap());
        assert_eq!(get(&conn, other).unwrap().state, ActionState::Inflight);
    }

    #[test]
    fn list_filters_by_state() {
        let (conn, acct) = mem();
        let a = enqueue(&conn, &action(acct, "star")).unwrap();
        let b = enqueue(&conn, &action(acct, "move")).unwrap();
        cancel(&conn, b).unwrap();
        assert_eq!(list(&conn, acct, None).unwrap().len(), 2);
        let pending = list(&conn, acct, Some(ActionState::Pending)).unwrap();
        assert_eq!(pending.iter().map(|a| a.id).collect::<Vec<_>>(), vec![a]);
        assert_eq!(
            list(&conn, acct, Some(ActionState::Cancelled))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn missing_action_is_not_found() {
        let (conn, _) = mem();
        assert!(matches!(get(&conn, 999), Err(MailError::NotFound(_))));
    }
}
