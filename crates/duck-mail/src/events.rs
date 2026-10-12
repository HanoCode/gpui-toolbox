//! 内核 → 宿主的事件。
//!
//! 形态与 `duck-ai` / `duck-db` 一致：内核把事件推进一个
//! `std::sync::mpsc::Sender<MailEvent>`，宿主（`duck-app` 的页面）用
//! `cx.spawn` + `background_executor().timer(40ms)` **节流 drain**，
//! 一次 `cx.notify()` 反映一批变化。
//!
//! ⚠️ **禁止逐事件 `notify()`**（`duck-components/src/dialog.rs` 的 prepaint
//! 自激是前车之鉴），必须节流（`docs/email-design.md` §4.1）。

use serde::{Deserialize, Serialize};

use crate::models::SyncState;

/// 内核广播给宿主的变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailEvent {
    /// 同步进度（首次全量时驱动进度条）。
    SyncProgress {
        /// 账户。
        account_id: i64,
        /// 已完成条数。
        done: u64,
        /// 预估总条数（未知时为 `None`）。
        total: Option<u64>,
    },
    /// 账户状态变化（`syncing` / `error` / `offline`…）。
    AccountState {
        /// 账户。
        account_id: i64,
        /// 新状态。
        state: SyncState,
        /// 失败原因（[`SyncState::Error`] / [`SyncState::NeedsReauth`] 时非空）。
        error: Option<String>,
    },
    /// 账户列表变化（新增 / 删除 / 改名）。
    AccountsChanged,
    /// 某账户的文件夹集合变化（新增文件夹、认到新角色）。
    FoldersChanged {
        /// 账户。
        account_id: i64,
        /// 变化后的文件夹数（用于日志与自检）。
        folder_count: usize,
    },
    /// 某文件夹新增了邮件（IDLE 推送或增量同步到达）。
    MessagesAdded {
        /// 账户。
        account_id: i64,
        /// 文件夹。
        folder_id: i64,
        /// 新增条数。
        count: u64,
    },
    /// 某封邮件的正文已拉取完成（阅读器可刷新）。
    BodyReady {
        /// 邮件主键。
        message_id: i64,
    },
    /// 检索索引已更新（FTS5 写入完成）。
    SearchIndexUpdated {
        /// 账户。
        account_id: i64,
    },
}

/// 一条邮件通知（托盘 / 系统通知用，M2 落地）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailNotification {
    /// 账户。
    pub account_id: i64,
    /// 邮件主键。
    pub message_id: i64,
    /// 发件人显示名。
    pub sender_name: Option<String>,
    /// 发件人地址。
    pub sender_addr: Option<String>,
    /// 主题。
    pub subject: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_cloneable_and_comparable() {
        let e = MailEvent::AccountState {
            account_id: 7,
            state: SyncState::Error,
            error: Some("connection refused".into()),
        };
        assert_eq!(e.clone(), e);
        assert_ne!(
            e,
            MailEvent::AccountState {
                account_id: 7,
                state: SyncState::Idle,
                error: None,
            }
        );
    }
}
