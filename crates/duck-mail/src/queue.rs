//! 动作队列：动作模型 + 乐观更新 + 撤销 + 补发。
//!
//! # 为什么要队列
//!
//! 标记已读 / 旗标 / 移动 / 删除 / 打标签都是**远端动作**：本地立刻生效、远端随后
//! 补发（`docs/email-design.md` §4.11 的「离线优先」）。队列表是
//! `pending_actions`（DDL 5 态见 [`crate::db::repo::queue`]）。
//!
//! 一次用户动作拆成三件事，**在同一个写事务里**完成（[`enqueue`]）：
//!
//! 1. **记录「动作前是什么」**（读当前旗标 / 文件夹 / 标签贴附状态）→ 写进 payload；
//! 2. [`apply_local`]：把净效果写本地库，界面立刻反映（乐观更新）；
//! 3. 写一行 `pending_actions`，`not_before = now + UNDO_GRACE_SECS`。
//!
//! `not_before` 就是**撤销宽限期**：在它到点前 [`flush_pending`] 不会补发这条动作，
//! 用户随时可以 [`undo`]。宽限期一过，队列才把它推到服务器。
//!
//! # 撤销只抵消这一条动作的净效果
//!
//! [`revert_local`] **不是**「重新拉一遍服务器」，而是按 payload 里记下的「动作前
//! 状态」精确复原。所以 payload 必须带上 `was` / `fromFolderId` / `wasAttached`
//! 这类**前像**——只按动作种类反推（比如「MarkRead 的反面就是未读」）会在
//! 「本来已读又点了一次已读」这种幂等场景下把状态弄反。
//!
//! # 跨版本读回
//!
//! 数据库里可能留着**更旧 / 更新的程序**写的动作行。`kind` 不认识、或 payload 解析
//! 不出来时，[`decode`] 返回 `None`，[`flush_pending`] 把这种行直接判 `failed`——
//! **不 panic、也不反复重试**（重试一个永远读不懂的动作没有意义）。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::MailCore;
use crate::db::MailDb;
use crate::db::repo::labels::{self, NewLabel};
use crate::db::repo::{folders, messages, queue as queue_repo};
use crate::error::{MailError, Result};
use crate::models::FolderRole;
use crate::proto::ImapConfig;
use crate::proto::imap::{FLAG_FLAGGED, FLAG_SEEN, ImapSession};

/// 撤销宽限期（秒）。宽限内动作只在本地生效，用户可撤销；到点才补发远端。
pub const UNDO_GRACE_SECS: i64 = 8;

/// 失败重试上限（含首次）。到顶后转 `failed` 终态，不再补发。
pub const MAX_ATTEMPTS: i64 = 5;

/// 指数退避的基数（秒）：第 1 次失败退避 30s，随后 60 / 120 / 240…
pub const BACKOFF_BASE_SECS: i64 = 30;

/// 指数退避的上限（秒）。
pub const BACKOFF_MAX_SECS: i64 = 3600;

/// 一轮补发最多取多少条（防止一次拉爆内存 / 卡住一条连接太久）。
pub const FLUSH_BATCH: i64 = 100;

/// 纯函数：第 `attempts` 次失败后的退避秒数（`min(base * 2^(attempts-1), max)`）。
///
/// `attempts` 从 1 起；0 或负值按 1 处理（防御性，调用方不会传）。
pub fn backoff_secs(attempts: i64) -> i64 {
    let step = attempts.max(1) - 1;
    // 用 checked_shl 防大 attempts 溢出；溢出就直接取上限。
    let factor = 1i64.checked_shl(step.min(62) as u32).unwrap_or(i64::MAX);
    BACKOFF_BASE_SECS
        .saturating_mul(factor)
        .min(BACKOFF_MAX_SECS)
}

/// 动作种类。落盘值是稳定的 snake_case 串（见 [`ActionKind::wire_name`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// 标为已读。
    MarkRead,
    /// 标为未读。
    MarkUnread,
    /// 加旗标（星标）。
    Star,
    /// 去旗标。
    Unstar,
    /// 移动到另一个文件夹。
    Move,
    /// 删除（有垃圾箱则移入，否则就地销毁）。
    Delete,
    /// 打标签。
    LabelAdd,
    /// 去标签。
    LabelRemove,
}

impl ActionKind {
    /// 落盘值（稳定，别改）。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::MarkRead => "mark_read",
            Self::MarkUnread => "mark_unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
            Self::Move => "move",
            Self::Delete => "delete",
            Self::LabelAdd => "label_add",
            Self::LabelRemove => "label_remove",
        }
    }

    /// 从落盘值恢复。**认不出的返回 `None`**（老库里的动作不能瞎猜语义）。
    ///
    /// 额外接受两个**历史别名** `add_label` / `remove_label`：`007` 迁移里就直接引用
    /// 了这两个名字（Gmail 标签动作），说明早期版本用过它们。
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "mark_read" => Self::MarkRead,
            "mark_unread" => Self::MarkUnread,
            "star" => Self::Star,
            "unstar" => Self::Unstar,
            "move" => Self::Move,
            "delete" => Self::Delete,
            "label_add" | "add_label" => Self::LabelAdd,
            "label_remove" | "remove_label" => Self::LabelRemove,
            _ => return None,
        })
    }
}

/// 动作的类型化载荷（也定义 `pending_actions.payload` 的 JSON 结构）。
///
/// 字段名用 **camelCase**（`folderId` / `fromFolderId` / `labelId` / `wasAttached`）：
/// `007` 迁移的 SQL 按 `$.labelId` 读旧动作，json 键名要能对上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPayload {
    /// 已读 / 旗标：`was` 是动作前的状态（撤销用），`folder_id` 是动作前邮件所在文件夹。
    Flag {
        /// 动作前该位的值。
        was: bool,
        /// 动作前所在文件夹（远端执行时 SELECT 哪个邮箱）。
        folder_id: i64,
    },
    /// 移动：`from_folder_id` 是动作前的文件夹，`folder_id` 是目标。
    Move {
        /// 动作前所在文件夹。
        from_folder_id: i64,
        /// 目标文件夹。
        folder_id: i64,
    },
    /// 删除：`folder_id` 是动作前所在文件夹（远端要在这里 `\Deleted` + `EXPUNGE`）。
    Delete {
        /// 动作前所在文件夹。
        folder_id: i64,
    },
    /// 标签：`label_id` 是标签主键，`was_attached` 是动作前的贴附状态（撤销用）。
    Label {
        /// 标签主键。
        label_id: i64,
        /// 动作前是否已贴。
        was_attached: bool,
        /// 动作前所在文件夹。
        folder_id: i64,
    },
}

impl ActionPayload {
    /// 远端执行时要 SELECT 哪个文件夹。
    pub const fn source_folder_id(self) -> i64 {
        match self {
            Self::Flag { folder_id, .. }
            | Self::Move {
                from_folder_id: folder_id,
                ..
            }
            | Self::Delete { folder_id }
            | Self::Label { folder_id, .. } => folder_id,
        }
    }
}

/// 一条动作（已解出 `kind` + 类型化 payload）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// 目标邮件。
    pub message_id: i64,
    /// 动作种类。
    pub kind: ActionKind,
    /// 载荷。
    pub payload: ActionPayload,
}

/// 调用方想做什么（由 [`enqueue`] 补齐「动作前是什么」变成 [`Action`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// 标为已读。
    MarkRead,
    /// 标为未读。
    MarkUnread,
    /// 加旗标。
    Star,
    /// 去旗标。
    Unstar,
    /// 移动。
    Move {
        /// 目标文件夹。
        folder_id: i64,
    },
    /// 删除。
    Delete,
    /// 打标签。
    LabelAdd {
        /// 标签主键。
        label_id: i64,
    },
    /// 去标签。
    LabelRemove {
        /// 标签主键。
        label_id: i64,
    },
}

impl Request {
    /// 对应的动作种类。
    pub const fn kind(self) -> ActionKind {
        match self {
            Self::MarkRead => ActionKind::MarkRead,
            Self::MarkUnread => ActionKind::MarkUnread,
            Self::Star => ActionKind::Star,
            Self::Unstar => ActionKind::Unstar,
            Self::Move { .. } => ActionKind::Move,
            Self::Delete => ActionKind::Delete,
            Self::LabelAdd { .. } => ActionKind::LabelAdd,
            Self::LabelRemove { .. } => ActionKind::LabelRemove,
        }
    }
}

// ---- payload 的 JSON 编解码 ----

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlagJson {
    was: bool,
    folder_id: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveJson {
    from_folder_id: i64,
    folder_id: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteJson {
    folder_id: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelJson {
    label_id: i64,
    was_attached: bool,
    folder_id: i64,
}

/// 把载荷编成 JSON 文本。这些结构全是定长整数 / 布尔，序列化不可能失败。
fn payload_json(payload: ActionPayload) -> String {
    let value = match payload {
        ActionPayload::Flag { was, folder_id } => serde_json::to_value(FlagJson { was, folder_id }),
        ActionPayload::Move {
            from_folder_id,
            folder_id,
        } => serde_json::to_value(MoveJson {
            from_folder_id,
            folder_id,
        }),
        ActionPayload::Delete { folder_id } => serde_json::to_value(DeleteJson { folder_id }),
        ActionPayload::Label {
            label_id,
            was_attached,
            folder_id,
        } => serde_json::to_value(LabelJson {
            label_id,
            was_attached,
            folder_id,
        }),
    };
    match value {
        Ok(v) => v.to_string(),
        // 兜底：上面都是无字符串的纯值结构，不会走到这里；真走到了也给个合法 JSON。
        Err(_) => "{}".to_string(),
    }
}

/// 从库行解出动作。
///
/// 返回 `None` 的两种情形：`kind` 不认识（跨版本留的旧动作），或 payload 解析失败。
/// **调用方据此把该行判 `failed`**，不要重试、也不要 panic。
pub fn decode(kind: &str, message_id: Option<i64>, payload: &str) -> Option<Action> {
    let kind = ActionKind::from_wire(kind)?;
    let message_id = message_id?;
    let payload = match kind {
        ActionKind::MarkRead | ActionKind::MarkUnread | ActionKind::Star | ActionKind::Unstar => {
            let p: FlagJson = serde_json::from_str(payload).ok()?;
            ActionPayload::Flag {
                was: p.was,
                folder_id: p.folder_id,
            }
        }
        ActionKind::Move => {
            let p: MoveJson = serde_json::from_str(payload).ok()?;
            ActionPayload::Move {
                from_folder_id: p.from_folder_id,
                folder_id: p.folder_id,
            }
        }
        ActionKind::Delete => {
            let p: DeleteJson = serde_json::from_str(payload).ok()?;
            ActionPayload::Delete {
                folder_id: p.folder_id,
            }
        }
        ActionKind::LabelAdd | ActionKind::LabelRemove => {
            let p: LabelJson = serde_json::from_str(payload).ok()?;
            ActionPayload::Label {
                label_id: p.label_id,
                was_attached: p.was_attached,
                folder_id: p.folder_id,
            }
        }
    };
    Some(Action {
        message_id,
        kind,
        payload,
    })
}

/// 入队一个动作，返回动作主键（供撤销条用）。
///
/// **三步在一个事务里**（调用方给 `conn` 是 `write_tx` 的事务对象）：读前像 → 乐观更新
/// → 写队列。任何一步失败整条回滚，不会出现「本地改了但队列没记」。
pub fn enqueue(
    conn: &Connection,
    account_id: i64,
    message_id: i64,
    request: Request,
) -> Result<i64> {
    let location = messages::location(conn, message_id)?;
    // 邮件没有文件夹（`folder_id IS NULL`）时记 0：远端执行会据此判「无法定位」而放弃，
    // 不会误操作到别的文件夹。
    let folder_id = location.folder_id.unwrap_or(0);

    let payload = match request {
        Request::MarkRead | Request::MarkUnread => ActionPayload::Flag {
            was: location.is_read,
            folder_id,
        },
        Request::Star | Request::Unstar => ActionPayload::Flag {
            was: location.is_starred,
            folder_id,
        },
        Request::Move { folder_id: target } => ActionPayload::Move {
            from_folder_id: folder_id,
            folder_id: target,
        },
        Request::Delete => ActionPayload::Delete { folder_id },
        Request::LabelAdd { label_id } | Request::LabelRemove { label_id } => {
            ActionPayload::Label {
                label_id,
                was_attached: labels::is_attached(conn, message_id, label_id)?,
                folder_id,
            }
        }
    };

    let action = Action {
        message_id,
        kind: request.kind(),
        payload,
    };
    apply_local(conn, &action)?;
    queue_repo::enqueue(
        conn,
        &queue_repo::NewAction {
            account_id,
            kind: action.kind.wire_name().to_string(),
            message_id: Some(message_id),
            thread_id: None,
            payload: payload_json(payload),
            not_before: Some(queue_repo::now_secs() + UNDO_GRACE_SECS),
        },
    )
}

/// 乐观更新：把动作的净效果写进本地库。
pub fn apply_local(conn: &Connection, action: &Action) -> Result<()> {
    let id = action.message_id;
    match (action.kind, action.payload) {
        (ActionKind::MarkRead, _) => messages::set_read(conn, id, true),
        (ActionKind::MarkUnread, _) => messages::set_read(conn, id, false),
        (ActionKind::Star, _) => messages::set_starred(conn, id, true),
        (ActionKind::Unstar, _) => messages::set_starred(conn, id, false),
        (ActionKind::Move, ActionPayload::Move { folder_id, .. }) => {
            // 目标文件夹可能已被删除（列表是旧的）：`messages.folder_id` 有外键，
            // 硬挪会撞 FK。此时只排队、不改本地，等补发时报出真实原因。
            match folders::get(conn, folder_id) {
                Ok(_) => messages::set_folder(conn, id, folder_id),
                Err(MailError::NotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        }
        (ActionKind::Delete, _) => {
            // 软删除：本地挪进垃圾箱（列表立刻少一行）。没有垃圾箱就只排队，
            // 等同步把行收走——总比假装删了却没动要诚实。
            let location = messages::location(conn, id)?;
            match trash_folder_id(conn, location.account_id)? {
                Some(trash) if location.folder_id != Some(trash) => {
                    messages::set_folder(conn, id, trash)
                }
                _ => Ok(()),
            }
        }
        (ActionKind::LabelAdd, ActionPayload::Label { label_id, .. }) => {
            labels::attach(conn, id, label_id)
        }
        (ActionKind::LabelRemove, ActionPayload::Label { label_id, .. }) => {
            labels::detach(conn, id, label_id)
        }
        // payload 与 kind 不匹配只可能来自手写的坏行；`decode` 已保证不会，保守 no-op。
        _ => Ok(()),
    }
}

/// 撤销时的反向复原：**只抵消这一条动作的净效果**（按 payload 里的前像精确还原）。
pub fn revert_local(conn: &Connection, action: &Action) -> Result<()> {
    let id = action.message_id;
    match (action.kind, action.payload) {
        (ActionKind::MarkRead | ActionKind::MarkUnread, ActionPayload::Flag { was, .. }) => {
            messages::set_read(conn, id, was)
        }
        (ActionKind::Star | ActionKind::Unstar, ActionPayload::Flag { was, .. }) => {
            messages::set_starred(conn, id, was)
        }
        (ActionKind::Move, ActionPayload::Move { from_folder_id, .. }) => {
            messages::set_folder(conn, id, from_folder_id)
        }
        (ActionKind::Delete, ActionPayload::Delete { folder_id }) => {
            messages::set_folder(conn, id, folder_id)
        }
        // 加 / 去标签都按「动作前是否已贴」还原——两种动作的前像语义一致。
        (
            ActionKind::LabelAdd | ActionKind::LabelRemove,
            ActionPayload::Label {
                label_id,
                was_attached,
                ..
            },
        ) => {
            if was_attached {
                labels::attach(conn, id, label_id)
            } else {
                labels::detach(conn, id, label_id)
            }
        }
        _ => Ok(()),
    }
}

/// 撤销一条动作：反向复原本地 + `cancel` 队列行。
///
/// **只在 `state='pending'` 时生效**（已 `inflight` / `done` 的返回 `false`——
/// 它可能已经在服务端生效，本地擅自复原会和远端打架）。
pub fn undo(conn: &Connection, action_id: i64) -> Result<bool> {
    let row = queue_repo::get(conn, action_id)?;
    if row.state != queue_repo::ActionState::Pending {
        return Ok(false);
    }
    // 认不出的动作无法反向复原，但取消它总是对的（用户点了「撤销」）。
    if let Some(action) = decode(&row.kind, row.message_id, &row.payload) {
        revert_local(conn, &action)?;
    }
    queue_repo::cancel(conn, action_id)
}

/// 该账户的垃圾箱文件夹主键（没有则 `None`）。
fn trash_folder_id(conn: &Connection, account_id: i64) -> Result<Option<i64>> {
    Ok(folders::find_by_role(conn, account_id, FolderRole::Trash)?.map(|f| f.id))
}

// ---------------------------------------------------------------------------
// 补发（flush）
// ---------------------------------------------------------------------------

/// 进程内「该账户正在补发」的闸门：**同账户互斥，不同账户互不阻塞**。
///
/// 为什么必须有它：`flush_pending` 是 `runtime.spawn(...).wait()`，两个调用点（用户手动
/// 「立即同步」+ 后台定时器）可以**真并发**。没有闸门时会出现这个时序：
///
/// ```text
/// [A] flush_pending(acct) 进门 → mark_inflight(1) → 正在网络往返
/// [B] flush_pending(acct) 进门 → requeue_inflight 把 1 打回 pending
///                             → due() 又取到 1 → 重复执行
/// ```
///
/// 幂等动作（已读 / 旗标 / 标签）重复一次无害；`move` / `delete` **不幂等**——第二条
/// `UID MOVE` 失败，留下一行假 `failed`，用户看到一次「同步操作失败」。
///
/// 设计取舍：**没有等待队列**。抢不到的一方直接返回 `Ok(0)`，而不是排队等前者跑完——
/// 补发本身是「晚一点再做也一样」的重复劳动，排队只会把调用方（可能正占着 UI 线程的
/// `Task::wait()`）堵住，还增加死锁面。
fn flush_gate() -> &'static Mutex<HashSet<i64>> {
    static GATE: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 持闸守卫：`Drop` 里把账户放行。
///
/// 用 RAII 而不是「出门手动 remove」是为了**异常路径也不漏放**——`flush_inner` 返回
/// `Err`、或 `runtime()` 建不出来时，守卫照样析构。
struct FlushGuard {
    account_id: i64,
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        let gate = flush_gate();
        // ⚠️ 锁被 poison **必须忽略**（`into_inner`）而不是 `unwrap()`：守卫本身就在
        // drop 期，panic 会二次展开；更坏的是若这里放弃放行，该账户的补发会**永久失效**。
        // 本闸门保护的只有「集合增删」这一动作，poison 时集合本身仍然是一致的。
        let mut held = match gate.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.remove(&self.account_id);
    }
}

/// 尝试为本账户取得补发闸门；已有人在跑则返回 `None`（调用方应直接收工）。
fn try_begin_flush(account_id: i64) -> Option<FlushGuard> {
    let gate = flush_gate();
    let mut held = match gate.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    if held.insert(account_id) {
        Some(FlushGuard { account_id })
    } else {
        None
    }
}

/// 把某账户到点的动作按入队顺序推到服务器；返回成功执行的动作数（阻塞，调用方放后台线程）。
///
/// 形态与 `sync::sync_account` 一致：**被调用时同步跑一遍**，不是常驻 actor
/// （常驻 + IDLE 是下一批的事）。
///
/// **同账户重入直接返回 `Ok(0)`**（见 [`try_begin_flush`]），不是错误——调用方不需要
/// 区分「没人可发」和「已经有人在发」。
pub fn flush_pending(core: &MailCore, account_id: i64) -> Result<usize> {
    // ⚠️ 守卫必须在**这里**（阻塞等待的那个栈帧）持有，不能移进下面的 `async` 块：
    // 移进去它就得跨 `await` 存活，而它的生命周期应当精确覆盖「这次阻塞等待」。
    let Some(_guard) = try_begin_flush(account_id) else {
        tracing::debug!(
            account_id,
            "flush already in progress for this account; skipping"
        );
        return Ok(0);
    };
    let runtime = core.runtime()?;
    let core = core.clone();
    runtime
        .spawn(async move { flush_inner(&core, account_id).await })
        .wait()?
}

async fn flush_inner(core: &MailCore, account_id: i64) -> Result<usize> {
    let db = core.db().clone();

    // 崩溃恢复：上一轮若在本进程外中断，行会卡在 `inflight`，`due()` 永远取不到它。
    // ⚠️ 本函数**不假设自己是该账户的唯一执行者**：同账户并发由 [`try_begin_flush`] 的
    // 闸门挡住，这里的抢占若真撞上在飞的动作，靠下一步 `mark_inflight` 的条件认领兜底。
    let recovered = db.write_tx(|tx| queue_repo::requeue_inflight(tx, account_id))?;
    if recovered > 0 {
        tracing::warn!(
            account_id,
            recovered,
            "requeued inflight actions left by a crash"
        );
    }

    let actions = db.read(|conn| queue_repo::due(conn, account_id, FLUSH_BATCH))?;
    if actions.is_empty() {
        return Ok(0);
    }

    let (account, password) = crate::accounts::load_with_password(&db, core.secrets(), account_id)?;
    let cfg = ImapConfig {
        host: account.imap_host.clone(),
        port: account.imap_port,
        security: account.security,
        username: account.username.clone(),
        password,
        auth: account.auth_kind,
    };
    let mut session = ImapSession::connect(&cfg).await?;

    // 当前已 SELECT 的邮箱名。动作可能跨文件夹，切文件夹才重发 `SELECT`。
    let mut selected: Option<String> = None;
    let mut done = 0usize;

    for row in actions {
        // 认不出的动作直接判终态失败：没有语义可执行，重试多少次都一样。
        let Some(action) = decode(&row.kind, row.message_id, &row.payload) else {
            db.write_tx(|tx| {
                queue_repo::mark_failed(tx, row.id, row.attempts + 1, "unrecognized action", None)
            })?;
            continue;
        };

        // 条件认领：抢不到说明这行已被并发的另一轮补发领走（闸门失效时的兜底，
        // 见 [`try_begin_flush`]），跳过而**不是**重复执行——`move` / `delete` 不幂等。
        if !db.write_tx(|tx| queue_repo::mark_inflight(tx, row.id))? {
            tracing::debug!(
                action = row.id,
                "action already claimed elsewhere; skipping"
            );
            continue;
        }
        match execute_action(&db, &mut session, &mut selected, &action).await {
            Ok(()) => {
                db.write_tx(|tx| queue_repo::mark_done(tx, row.id, queue_repo::now_secs()))?;
                done += 1;
            }
            Err(error) => {
                let attempts = row.attempts + 1;
                // 网络类失败且没到上限 → 指数退避后重试；其余（或到顶）→ 终态失败。
                let retry_at = if error.is_retryable() && attempts < MAX_ATTEMPTS {
                    Some(queue_repo::now_secs() + backoff_secs(attempts))
                } else {
                    None
                };
                tracing::warn!(action = row.id, attempts, error = %error, "pending action failed");
                db.write_tx(|tx| {
                    queue_repo::mark_failed(tx, row.id, attempts, &error.to_string(), retry_at)
                })?;
            }
        }
    }

    session.logout().await;
    Ok(done)
}

/// 执行一条动作（假定邮件所在文件夹已可从库里解析出来）。
async fn execute_action(
    db: &MailDb,
    session: &mut ImapSession,
    selected: &mut Option<String>,
    action: &Action,
) -> Result<()> {
    let source_id = action.payload.source_folder_id();
    if source_id <= 0 {
        return Err(MailError::NotFound(format!(
            "action on message {} has no source folder",
            action.message_id
        )));
    }
    let uid = db
        .read(|conn| messages::location(conn, action.message_id))?
        .uid
        .ok_or_else(|| MailError::NotFound(format!("uid of message {}", action.message_id)))?;
    let uid = u32::try_from(uid)
        .map_err(|_| MailError::Protocol(format!("uid {uid} is out of range")))?;

    // `UID STORE` / `UID MOVE` / `UID EXPUNGE` 都要求邮箱已选中。
    let source = db.read(|conn| folders::get(conn, source_id))?;
    if selected.as_deref() != Some(source.name.as_str()) {
        session.select(&source.name).await?;
        *selected = Some(source.name.clone());
    }

    match action.kind {
        ActionKind::MarkRead => session.add_flags(&[uid], &[FLAG_SEEN]).await,
        ActionKind::MarkUnread => session.remove_flags(&[uid], &[FLAG_SEEN]).await,
        ActionKind::Star => session.add_flags(&[uid], &[FLAG_FLAGGED]).await,
        ActionKind::Unstar => session.remove_flags(&[uid], &[FLAG_FLAGGED]).await,
        ActionKind::Move => {
            let ActionPayload::Move { folder_id, .. } = action.payload else {
                return Err(MailError::Other(
                    "move action without a target folder".into(),
                ));
            };
            let target = db.read(|conn| folders::get(conn, folder_id))?;
            session.move_messages(&[uid], &target.name).await
        }
        ActionKind::Delete => {
            let trash_id = db.read(|conn| trash_folder_id(conn, source.account_id))?;
            let trash = match trash_id {
                Some(id) => Some(db.read(|conn| folders::get(conn, id))?.name),
                None => None,
            };
            session.delete_messages(&[uid], trash.as_deref()).await
        }
        ActionKind::LabelAdd => {
            let ActionPayload::Label { label_id, .. } = action.payload else {
                return Err(MailError::Other("label action without a label".into()));
            };
            let keyword = db.read(|conn| labels::get(conn, label_id))?.keyword;
            session.add_flags(&[uid], &[keyword.as_str()]).await
        }
        ActionKind::LabelRemove => {
            let ActionPayload::Label { label_id, .. } = action.payload else {
                return Err(MailError::Other("label action without a label".into()));
            };
            let keyword = db.read(|conn| labels::get(conn, label_id))?.keyword;
            session.remove_flags(&[uid], &[keyword.as_str()]).await
        }
    }
}

/// 建一个账户级标签（左栏「新建标签」用）。
pub fn create_label(core: &MailCore, account_id: i64, name: &str, color: &str) -> Result<i64> {
    core.db()
        .write_tx(|tx| labels::create(tx, &NewLabel::account(account_id, name, color)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MailPaths;
    use crate::models::{ConnectionSecurity, NewAccount};
    use crate::secrets;

    fn core(tag: &str) -> (MailCore, i64, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("duck-mail-queue-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core =
            MailCore::open_with_secrets(MailPaths::with_root(&dir), secrets::memory()).unwrap();
        let account = core
            .add_account(&NewAccount {
                email: format!("{tag}@example.com"),
                display_name: None,
                username: format!("{tag}@example.com"),
                imap_host: "imap.invalid".into(),
                imap_port: 993,
                smtp_host: "smtp.invalid".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            })
            .unwrap();
        (core, account, dir)
    }

    /// 建两个文件夹（收件箱 + 垃圾箱）与一封邮件，返回 `(folder_inbox, folder_trash, message)`。
    fn seed(core: &MailCore, account: i64) -> (i64, i64, i64) {
        let db = core.db();
        let inbox = db
            .write_tx(|tx| {
                folders::upsert(
                    tx,
                    account,
                    &folders::FolderUpsert {
                        name: "INBOX".into(),
                        delimiter: Some("/".into()),
                        role: FolderRole::Inbox,
                        uidvalidity: Some(1),
                        uidnext: Some(10),
                    },
                )
            })
            .unwrap();
        let trash = db
            .write_tx(|tx| {
                folders::upsert(
                    tx,
                    account,
                    &folders::FolderUpsert {
                        name: "Trash".into(),
                        delimiter: Some("/".into()),
                        role: FolderRole::Trash,
                        uidvalidity: Some(1),
                        uidnext: Some(10),
                    },
                )
            })
            .unwrap();
        let message = db
            .write_tx(|tx| {
                messages::upsert_envelope(
                    tx,
                    account,
                    inbox,
                    &messages::EnvelopeRow {
                        uid: 1,
                        subject: "hi".into(),
                        date: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        (inbox, trash, message)
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(3), 120);
        // 到顶不再涨。
        assert_eq!(backoff_secs(50), BACKOFF_MAX_SECS);
        assert_eq!(backoff_secs(0), 30, "0 按第 1 次处理");
    }

    #[test]
    fn wire_name_roundtrips_and_accepts_legacy_label_aliases() {
        for kind in [
            ActionKind::MarkRead,
            ActionKind::MarkUnread,
            ActionKind::Star,
            ActionKind::Unstar,
            ActionKind::Move,
            ActionKind::Delete,
            ActionKind::LabelAdd,
            ActionKind::LabelRemove,
        ] {
            assert_eq!(ActionKind::from_wire(kind.wire_name()), Some(kind));
        }
        // 007 迁移用过 `add_label` / `remove_label`。
        assert_eq!(
            ActionKind::from_wire("add_label"),
            Some(ActionKind::LabelAdd)
        );
        assert_eq!(
            ActionKind::from_wire("remove_label"),
            Some(ActionKind::LabelRemove)
        );
        assert_eq!(ActionKind::from_wire("nonsense"), None);
    }

    #[test]
    fn decode_never_panics_on_foreign_rows() {
        // 不认识的 kind。
        assert!(decode("teleport", Some(1), "{}").is_none());
        // 认识 kind 但 payload 结构不对。
        assert!(decode("move", Some(1), "{\"nope\":1}").is_none());
        // payload 不是 JSON。
        assert!(decode("star", Some(1), "not json").is_none());
        // 没有邮件主键。
        assert!(decode("star", None, "{\"was\":false,\"folderId\":1}").is_none());
        // 正常行能解出。
        let ok = decode("star", Some(7), "{\"was\":false,\"folderId\":3}").unwrap();
        assert_eq!(ok.kind, ActionKind::Star);
        assert_eq!(
            ok.payload,
            ActionPayload::Flag {
                was: false,
                folder_id: 3
            }
        );
    }

    #[test]
    fn payload_json_uses_camel_case_keys() {
        // `007` 迁移按 `$.labelId` 读旧动作，键名必须对得上。
        let json = payload_json(ActionPayload::Label {
            label_id: 5,
            was_attached: true,
            folder_id: 2,
        });
        assert!(json.contains("\"labelId\":5"), "got {json}");
        assert!(json.contains("\"wasAttached\":true"), "got {json}");
        assert!(json.contains("\"folderId\":2"), "got {json}");

        let move_json = payload_json(ActionPayload::Move {
            from_folder_id: 1,
            folder_id: 2,
        });
        assert!(move_json.contains("\"fromFolderId\":1"), "got {move_json}");
    }

    #[test]
    fn apply_then_revert_is_a_net_noop_for_every_kind() {
        let (core, account, dir) = core("net");
        let (inbox, trash, message) = seed(&core, account);
        let label = create_label(&core, account, "工作", "#ff0000").unwrap();
        let db = core.db();

        // 每个动作：先 apply → 断言净效果，再 revert → 断言回到原点。
        let cases = [
            (
                Action {
                    message_id: message,
                    kind: ActionKind::Star,
                    payload: ActionPayload::Flag {
                        was: false,
                        folder_id: inbox,
                    },
                },
                "star",
            ),
            (
                Action {
                    message_id: message,
                    kind: ActionKind::Move,
                    payload: ActionPayload::Move {
                        from_folder_id: inbox,
                        folder_id: trash,
                    },
                },
                "move",
            ),
            (
                Action {
                    message_id: message,
                    kind: ActionKind::LabelAdd,
                    payload: ActionPayload::Label {
                        label_id: label,
                        was_attached: false,
                        folder_id: inbox,
                    },
                },
                "label",
            ),
        ];

        for (action, tag) in cases {
            db.write_tx(|tx| apply_local(tx, &action)).unwrap();
            db.write_tx(|tx| revert_local(tx, &action)).unwrap();
            let location = db.read(|conn| messages::location(conn, message)).unwrap();
            assert!(!location.is_starred, "{tag}: star 应被抵消");
            assert_eq!(location.folder_id, Some(inbox), "{tag}: 文件夹应回到原处");
            assert!(
                db.read(|conn| labels::labels_for_message(conn, message))
                    .unwrap()
                    .is_empty(),
                "{tag}: 标签应被抵消"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_restores_the_exact_prior_state_not_the_opposite() {
        let (core, account, dir) = core("prior");
        let (inbox, _trash, message) = seed(&core, account);
        let db = core.db();
        // 邮件本来「已读 + 已加旗标」。
        db.write_tx(|tx| messages::set_read(tx, message, true))
            .unwrap();
        db.write_tx(|tx| messages::set_starred(tx, message, true))
            .unwrap();

        // 用户又点了一次「标为已读」：前像是「已读」。撤销必须还原成「已读」，
        // 而不是机械地翻成「未读」。
        let action = Action {
            message_id: message,
            kind: ActionKind::MarkRead,
            payload: ActionPayload::Flag {
                was: true,
                folder_id: inbox,
            },
        };
        db.write_tx(|tx| apply_local(tx, &action)).unwrap();
        db.write_tx(|tx| revert_local(tx, &action)).unwrap();
        let after = db.read(|conn| messages::location(conn, message)).unwrap();
        assert!(after.is_read, "撤销应回到动作前（已读），不是翻成未读");
        assert!(after.is_starred, "不该动旗标位");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_add_then_undo_removes_it_and_reverts_folder_move() {
        let (core, account, dir) = core("undo");
        let (_inbox, _trash, message) = seed(&core, account);
        let label = create_label(&core, account, "工作", "#ff0000").unwrap();

        let action_id = core.label_add(account, message, label).unwrap();
        // 乐观更新已生效。
        assert!(
            core.db()
                .read(|conn| labels::labels_for_message(conn, message))
                .unwrap()
                .len()
                == 1
        );
        assert!(core.undo_pending(action_id).unwrap());
        assert!(
            core.db()
                .read(|conn| labels::labels_for_message(conn, message))
                .unwrap()
                .is_empty()
        );
        // 再撤一次：行已 cancelled，返回 false。
        assert!(!core.undo_pending(action_id).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enqueue_records_the_undo_grace_window() {
        let (core, account, dir) = core("grace");
        let (_inbox, _trash, message) = seed(&core, account);
        let before = queue_repo::now_secs();
        let action_id = core.set_read(account, message, true).unwrap();
        let row = core
            .db()
            .read(|conn| queue_repo::get(conn, action_id))
            .unwrap();
        let not_before = row.not_before.expect("宽限期内 not_before 必须存在");
        assert!(
            not_before >= before + UNDO_GRACE_SECS,
            "not_before 应落在宽限期之后"
        );
        // 宽限期内 `due()` 不该取出它（补发不会抢在撤销之前发生）。
        assert!(
            core.db()
                .read(|conn| queue_repo::due(conn, account, 10))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_actions_lists_only_pending() {
        let (core, account, dir) = core("pending");
        let (_inbox, _trash, message) = seed(&core, account);
        let a = core.set_starred(account, message, true).unwrap();
        let b = core.move_message(account, message, 999).unwrap();
        assert_eq!(core.pending_actions(account).unwrap().len(), 2);
        core.undo_pending(b).unwrap();
        assert_eq!(
            core.pending_actions(account)
                .unwrap()
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            vec![a]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_moves_the_message_to_trash_locally_and_undo_brings_it_back() {
        let (core, account, dir) = core("delete");
        let (inbox, trash, message) = seed(&core, account);
        let action_id = core.delete_message(account, message).unwrap();
        let moved = core
            .db()
            .read(|conn| messages::location(conn, message))
            .unwrap();
        assert_eq!(moved.folder_id, Some(trash), "软删除：本地挪进垃圾箱");
        assert!(core.undo_pending(action_id).unwrap());
        let restored = core
            .db()
            .read(|conn| messages::location(conn, message))
            .unwrap();
        assert_eq!(restored.folder_id, Some(inbox), "撤销：挪回收件箱");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 补发闸门与条件认领（防「同账户并发补发 → 重复推送非幂等动作」）
    //
    // 下面三条闸门测试**不碰磁盘也不碰网络**：它们直接用不可能与真实账户撞号的
    // 号段（9_100_0xx）操作那张进程内静态表，所以彼此之间、与其它测试之间都不串扰。
    // -----------------------------------------------------------------------

    #[test]
    fn flush_gate_is_mutually_exclusive_per_account() {
        let account = 9_100_001;
        let first = try_begin_flush(account);
        assert!(first.is_some(), "第一次应拿到闸门");
        assert!(
            try_begin_flush(account).is_none(),
            "同一账户的第二条补发必须被挡回（否则会重复推送 move/delete）"
        );
        drop(first);
        assert!(
            try_begin_flush(account).is_some(),
            "前者收工后应能重新取得闸门"
        );
    }

    #[test]
    fn flush_gate_does_not_block_other_accounts() {
        let a = try_begin_flush(9_100_011).expect("账户 A 应能取得闸门");
        let b = try_begin_flush(9_100_012).expect("账户 B 与 A 无关，不该被阻塞");
        drop((a, b));
    }

    #[test]
    fn flush_gate_is_released_when_the_holder_panics() {
        let account = 9_100_021;
        // panic 会被 libtest 的输出捕获器收走，不会污染其它用例的输出。
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = try_begin_flush(account).expect("首次应取得闸门");
            panic!("模拟持闸期间 panic");
        }));
        assert!(caught.is_err(), "catch_unwind 应捕获到 panic");
        assert!(
            try_begin_flush(account).is_some(),
            "持闸者 unwind 时守卫必须析构放行——否则该账户的补发此后永久失效"
        );
    }

    #[test]
    fn mark_inflight_is_a_conditional_claim() {
        let (core, account, dir) = core("claim");
        let id = core
            .db()
            .write_tx(|tx| {
                queue_repo::enqueue(
                    tx,
                    &queue_repo::NewAction {
                        account_id: account,
                        kind: "star".into(),
                        message_id: None,
                        thread_id: None,
                        payload: "{}".into(),
                        not_before: None,
                    },
                )
            })
            .unwrap();

        let first = core
            .db()
            .write_tx(|tx| queue_repo::mark_inflight(tx, id))
            .unwrap();
        assert!(first, "第一次认领应成功");
        let second = core
            .db()
            .write_tx(|tx| queue_repo::mark_inflight(tx, id))
            .unwrap();
        assert!(!second, "已 inflight 的行不能再被认领——这就是防重复的落点");
        assert_eq!(
            core.db()
                .read(|conn| queue_repo::get(conn, id))
                .unwrap()
                .state,
            queue_repo::ActionState::Inflight,
            "第二次不该改写状态"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 守卫：`flush_inner` 只能有**一个**调用点（`flush_pending`）。
    ///
    /// 「只许一处入口」这类约束没法用类型系统表达，而它一旦被破坏，症状是**偶发的
    /// 重复推送**（比编译错误难发现得多）。所以这里对本文件源码做文本断言。
    ///
    /// ⚠️ 计数前**必须先切掉测试模块**：本测试自己的断言消息里就含那个 needle，
    /// 不切掉会把自己算进去（第一版就是这么错的：实得 4 而不是 2）。
    #[test]
    fn flush_inner_has_exactly_one_call_site() {
        const SRC: &str = include_str!("queue.rs");
        let production = SRC
            .split("#[cfg(test)]")
            .next()
            .expect("split 至少产出一段");
        assert!(
            production.len() < SRC.len(),
            "没切到测试模块——本文件的结构变了，请更新这个守卫"
        );
        // needle 在这里拼出来，免得它自己出现在断言消息里再被数进去。
        let needle = concat!("flush_inner", "(");
        let calls = production.matches(needle).count();
        assert_eq!(
            calls, 2,
            "flush_inner 应恰好被引用 2 次（1 处定义 + flush_pending 里 1 处调用），实得 {calls}"
        );
        assert!(
            production.contains("let Some(_guard) = try_begin_flush(account_id) else"),
            "flush_pending 必须先过闸门、再进 flush_inner"
        );
    }
}
