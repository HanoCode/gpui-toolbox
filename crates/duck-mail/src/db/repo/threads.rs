//! `threads` 表的读写：会话聚合的**重建**与查询。
//!
//! # 为什么是「整账号重建」而不是增量维护
//!
//! 线程归属不是一封邮件自己能决定的——它取决于**同一账号里其它邮件**的引用链
//! （祖先可能晚于子信到达，也可能反过来）。增量维护意味着「收到新信时还要回头
//! 修正已有线程」，那套逻辑的每条分支都要处理「祖先还没到」，越写越难验证。
//!
//! 这里选**整账号重算**：读该账号的全部消息与引用链，跑一遍
//! [`crate::threading::group_threads`]，删掉旧线程、写回新线程、回填
//! `messages.thread_id`。代价是 O(账号消息数)，但这是纯本地 SQLite 读 + 内存归并，
//! 十万封量级也就百毫秒级；收益是**只有一条代码路径**，且任意时刻「重算一遍」的
//! 结果都一致（幂等，见测试 `rebuild_is_idempotent`）。
//!
//! 调用时机由 `sync` 决定（一轮同步收尾时调一次），本模块不关心。
//!
//! # 与 `message_refs` 的关系
//!
//! 引用链**不存 `messages` 表**（那要改 schema），而是落在既有的 `message_refs`
//! 表（`WITHOUT ROWID`，`(message_id, ref_message_id)` 联合主键）。`sync` 落库时
//! 用 [`crate::db::repo::messages::replace_references`] 把每封信的引用写进去，
//! 本模块重建时再读出来喂给合并算法。

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::models::ThreadSummary;
use crate::threading::{ThreadInput, group_threads};

/// 一棵线程里成员消息的原始行（重建内部用）。
struct MemberRow {
    id: i64,
    message_id: Option<String>,
    subject: String,
    gm_thrid: Option<String>,
    date: i64,
    is_read: bool,
    is_starred: bool,
    has_attachments: bool,
    snippet: String,
    from_addr: Option<String>,
}

/// 重建某账号的全部会话，返回会话数。
///
/// **必须在单事务里调用**（读 + 清 + 写 + 回填是一条原子路径），所以参数是
/// `&Connection`——`rusqlite::Transaction` 通过 `Deref` 也能传（见 `repo/mod.rs`）。
///
/// 草稿（`is_draft = 1`）**不参与**会话合并：它们没有文件夹、`date` 是本地保存时间，
/// 混进来会让「会话最新时间」被未发出的草稿带偏。
pub fn rebuild_for_account(conn: &Connection, account_id: i64) -> Result<usize> {
    let members = load_members(conn, account_id)?;
    // 先清干净——账号没有信（或全是草稿）时也要把可能残留的旧线程清掉。
    clear_threads(conn, account_id)?;
    if members.is_empty() {
        return Ok(0);
    }

    let refs = load_refs(conn, account_id)?;
    let inputs: Vec<ThreadInput> = members
        .iter()
        .map(|m| ThreadInput {
            id: m.id,
            message_id: m.message_id.clone(),
            references: refs.get(&m.id).cloned().unwrap_or_default(),
            subject: Some(m.subject.clone()),
            gm_thrid: m.gm_thrid.clone(),
        })
        .collect();
    let groups = group_threads(&inputs);
    let by_id: HashMap<i64, &MemberRow> = members.iter().map(|m| (m.id, m)).collect();

    for group in &groups {
        let rows: Vec<&MemberRow> = group
            .members
            .iter()
            .filter_map(|id| by_id.get(id).copied())
            .collect();
        // 最新一封：会话的显示主题与摘要都由它来（列表按 `last_message_at` 排序）。
        let newest = rows.iter().max_by_key(|m| (m.date, m.id));
        let last_message_at = rows.iter().map(|m| m.date).max().unwrap_or(0);

        conn.execute(
            "INSERT INTO threads
               (account_id, gm_thrid, subject_norm, last_message_at, message_count,
                unread_count, starred_count, attachment_count, snippet, participants_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                account_id,
                group.gm_thrid,
                group.subject_norm,
                last_message_at,
                rows.len() as i64,
                rows.iter().filter(|m| !m.is_read).count() as i64,
                rows.iter().filter(|m| m.is_starred).count() as i64,
                rows.iter().filter(|m| m.has_attachments).count() as i64,
                newest.map_or(String::new(), |m| m.snippet.clone()),
                encode_participants(&rows),
            ],
        )?;
        let thread_id = conn.last_insert_rowid();
        for row in &rows {
            conn.execute(
                "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                params![thread_id, row.id],
            )?;
        }
    }

    Ok(groups.len())
}

/// 会话列表，按最新消息时间倒序分页。
pub fn list(
    conn: &Connection,
    account_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<ThreadSummary>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.account_id,
                COALESCE((SELECT m.subject FROM messages m
                          WHERE m.thread_id = t.id
                          ORDER BY m.date DESC, m.id DESC LIMIT 1), '') AS subject,
                t.snippet, t.last_message_at, t.message_count, t.unread_count,
                t.starred_count, t.attachment_count, t.participants_json
         FROM threads t
         WHERE t.account_id = ?1
         ORDER BY t.last_message_at DESC, t.id DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![account_id, limit, offset], |row| {
        let participants_json: String = row.get(9)?;
        Ok(ThreadSummary {
            id: row.get(0)?,
            account_id: row.get(1)?,
            subject: row.get(2)?,
            snippet: row.get(3)?,
            last_message_at: row.get(4)?,
            message_count: row.get(5)?,
            unread_count: row.get(6)?,
            starred_count: row.get(7)?,
            attachment_count: row.get(8)?,
            participants: serde_json::from_str(&participants_json).unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 清掉某账号的旧线程。
///
/// **先解绑 `messages` 再删 `threads`**：`messages.thread_id` 有外键指向
/// `threads.id`，直接删父行会被外键挡下（`NO ACTION`）。
fn clear_threads(conn: &Connection, account_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE messages SET thread_id = NULL WHERE account_id = ?1",
        params![account_id],
    )?;
    conn.execute(
        "DELETE FROM threads WHERE account_id = ?1",
        params![account_id],
    )?;
    Ok(())
}

/// 读该账号参与合并的消息行（排除草稿）。
fn load_members(conn: &Connection, account_id: i64) -> Result<Vec<MemberRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, subject, gm_thrid, date, is_read, is_starred,
                has_attachments, snippet, from_addr
         FROM messages
         WHERE account_id = ?1 AND is_draft = 0",
    )?;
    let rows = stmt.query_map(params![account_id], |row| {
        Ok(MemberRow {
            id: row.get(0)?,
            message_id: row.get(1)?,
            subject: row.get(2)?,
            gm_thrid: row.get(3)?,
            date: row.get(4)?,
            is_read: row.get::<_, i64>(5)? != 0,
            is_starred: row.get::<_, i64>(6)? != 0,
            has_attachments: row.get::<_, i64>(7)? != 0,
            snippet: row.get(8)?,
            from_addr: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 读该账号所有邮件的引用链：`messages.id` → `References` 的 `Message-ID` 列表。
fn load_refs(conn: &Connection, account_id: i64) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT r.message_id, r.ref_message_id
         FROM message_refs r
         JOIN messages m ON m.id = r.message_id
         WHERE m.account_id = ?1",
    )?;
    let rows = stmt.query_map(params![account_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (message_id, reference) = row?;
        out.entry(message_id).or_default().push(reference);
    }
    Ok(out)
}

/// 参与人：把成员消息的发件人地址去重（保持首现顺序），序列化成 JSON 数组。
///
/// 只取 `from_addr`：一来一回的会话里双方都会作为某封的发件人出现，收件人一侧
/// 已被覆盖；把 `to` / `cc` 也算进来，会把转发链上的无关抄送一并拉进参与者。
fn encode_participants(rows: &[&MemberRow]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let Some(addr) = row.from_addr.as_deref().map(str::trim) else {
            continue;
        };
        if addr.is_empty() || seen.contains(&addr) {
            continue;
        }
        seen.push(addr);
    }
    serde_json::to_string(&seen).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::{accounts, folders, messages};
    use crate::models::{ConnectionSecurity, FolderRole, NewAccount};

    /// 内存库 + 一个账户 + 一个收件箱文件夹。
    fn mem() -> (Connection, i64, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let account_id = accounts::insert(
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
        let folder_id = folders::upsert(
            &conn,
            account_id,
            &folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: FolderRole::Inbox,
                uidvalidity: None,
                uidnext: None,
            },
        )
        .unwrap();
        (conn, account_id, folder_id)
    }

    /// 落一封信 + 它的引用链，返回 `messages.id`。
    ///
    /// `date` 借用 `uid`，让测试能靠 uid 控制「谁最新」。
    #[allow(clippy::too_many_arguments)]
    fn add(
        conn: &Connection,
        account_id: i64,
        folder_id: i64,
        uid: i64,
        message_id: &str,
        subject: &str,
        from: &str,
        seen: bool,
        starred: bool,
        refs: &[&str],
    ) -> i64 {
        let row = messages::EnvelopeRow {
            uid,
            message_id: Some(message_id.into()),
            subject: subject.into(),
            from_name: None,
            from_addr: Some(from.into()),
            to: vec![],
            cc: vec![],
            date: uid,
            is_read: seen,
            is_starred: starred,
            has_attachments: false,
            size: None,
            snippet: subject.into(),
        };
        let id = messages::upsert_envelope(conn, account_id, folder_id, &row).unwrap();
        messages::replace_references(conn, id, refs).unwrap();
        id
    }

    fn thread_of(conn: &Connection, id: i64) -> Option<i64> {
        conn.query_row(
            "SELECT thread_id FROM messages WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn rebuild_groups_a_chain_and_backfills_thread_id() {
        let (conn, account_id, folder) = mem();
        let a = add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic",
            "a@x.com",
            true,
            false,
            &[],
        );
        let b = add(
            &conn,
            account_id,
            folder,
            2,
            "b@x",
            "Re: topic",
            "b@x.com",
            false,
            false,
            &["a@x"],
        );
        let c = add(
            &conn,
            account_id,
            folder,
            3,
            "c@x",
            "Re: topic",
            "a@x.com",
            false,
            true,
            &["a@x", "b@x"],
        );

        let threads = rebuild_for_account(&conn, account_id).unwrap();
        assert_eq!(threads, 1);

        let ta = thread_of(&conn, a).expect("a must be bound to a thread");
        assert_eq!(Some(ta), thread_of(&conn, b));
        assert_eq!(Some(ta), thread_of(&conn, c));
    }

    #[test]
    fn rebuild_aggregates_counts_and_participants() {
        let (conn, account_id, folder) = mem();
        add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic",
            "a@x.com",
            true,
            false,
            &[],
        );
        add(
            &conn,
            account_id,
            folder,
            2,
            "b@x",
            "Re: topic",
            "b@x.com",
            false,
            false,
            &["a@x"],
        );
        add(
            &conn,
            account_id,
            folder,
            3,
            "c@x",
            "Re: topic",
            "a@x.com",
            false,
            true,
            &["a@x", "b@x"],
        );
        rebuild_for_account(&conn, account_id).unwrap();

        let list = list(&conn, account_id, 50, 0).unwrap();
        assert_eq!(list.len(), 1);
        let t = &list[0];
        assert_eq!(t.message_count, 3);
        assert_eq!(t.unread_count, 2);
        assert_eq!(t.starred_count, 1);
        // 最新一封（uid 3）的主题与时间。
        assert_eq!(t.subject, "Re: topic");
        assert_eq!(t.last_message_at, 3);
        // 参与者去重、按首现顺序。
        assert_eq!(
            t.participants,
            vec!["a@x.com".to_string(), "b@x.com".to_string()]
        );
    }

    #[test]
    fn rebuild_keeps_separate_threads_apart() {
        let (conn, account_id, folder) = mem();
        add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic one",
            "a@x.com",
            true,
            false,
            &[],
        );
        add(
            &conn,
            account_id,
            folder,
            2,
            "b@x",
            "topic two",
            "b@x.com",
            true,
            false,
            &[],
        );
        let threads = rebuild_for_account(&conn, account_id).unwrap();
        assert_eq!(threads, 2);
        // 各自一条会话；列表里各自的 message_count 都是 1。
        let list = list(&conn, account_id, 50, 0).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|t| t.message_count == 1));
    }

    #[test]
    fn rebuild_is_idempotent() {
        let (conn, account_id, folder) = mem();
        add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic",
            "a@x.com",
            true,
            false,
            &[],
        );
        add(
            &conn,
            account_id,
            folder,
            2,
            "b@x",
            "Re: topic",
            "b@x.com",
            false,
            false,
            &["a@x"],
        );

        let first = rebuild_for_account(&conn, account_id).unwrap();
        let threads_after_first: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        let second = rebuild_for_account(&conn, account_id).unwrap();
        let threads_after_second: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        // 重算不能累积出新线程（`clear_threads` 必须先跑）。
        assert_eq!(threads_after_first, threads_after_second);
    }

    #[test]
    fn rebuild_of_empty_account_clears_stale_threads() {
        let (conn, account_id, folder) = mem();
        add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic",
            "a@x.com",
            true,
            false,
            &[],
        );
        rebuild_for_account(&conn, account_id).unwrap();

        // 把消息全删掉（模拟账号清空），重算应当把线程也清干净。
        conn.execute("DELETE FROM message_refs", []).unwrap();
        conn.execute("DELETE FROM messages", []).unwrap();
        let threads = rebuild_for_account(&conn, account_id).unwrap();
        assert_eq!(threads, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn draft_messages_do_not_join_threads() {
        let (conn, account_id, folder) = mem();
        add(
            &conn,
            account_id,
            folder,
            1,
            "a@x",
            "topic",
            "a@x.com",
            true,
            false,
            &[],
        );
        // 一封草稿（is_draft = 1）——不该被算进任何会话。
        let draft = messages::upsert_envelope(
            &conn,
            account_id,
            folder,
            &messages::EnvelopeRow {
                uid: 99,
                message_id: Some("draft@x".into()),
                subject: "topic".into(),
                from_name: None,
                from_addr: Some("a@x.com".into()),
                to: vec![],
                cc: vec![],
                date: 99,
                is_read: true,
                is_starred: false,
                has_attachments: false,
                size: None,
                snippet: "topic".into(),
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET is_draft = 1 WHERE id = ?1",
            params![draft],
        )
        .unwrap();

        rebuild_for_account(&conn, account_id).unwrap();
        assert_eq!(thread_of(&conn, draft), None);
    }
}
