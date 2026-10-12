//! `messages` / `message_bodies` / `attachments` 的读写。
//!
//! 列表查询直接基于 `messages`（M1 不做会话合并）。写路径全部是 upsert，键为
//! `(account_id, folder_id, uid)`——同一封邮件重复同步（重启后增量重跑）不会产生
//! 副本，这正是 M1 验收项「重启后增量同步不重复拉取」要的行为。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};
use crate::models::{AttachmentMeta, MessageDetail, MessageSummary};

/// 从 IMAP 信封（或 BODYSTRUCTURE 摘要）落库的一行。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvelopeRow {
    /// IMAP UID（文件夹内唯一）。
    pub uid: i64,
    /// `Message-ID` 头。
    pub message_id: Option<String>,
    /// 主题。
    pub subject: String,
    /// 发件人显示名。
    pub from_name: Option<String>,
    /// 发件人地址。
    pub from_addr: Option<String>,
    /// 收件人（`"名字 <addr>"` 形态）。
    pub to: Vec<String>,
    /// 抄送。
    pub cc: Vec<String>,
    /// 发送时间（Unix 秒）。
    pub date: i64,
    /// 是否已读（`\Seen`）。
    pub is_read: bool,
    /// 是否旗标（`\Flagged`）。
    pub is_starred: bool,
    /// 是否有附件。
    pub has_attachments: bool,
    /// 字节数。
    pub size: Option<i64>,
    /// 摘要（列表第三行）。
    pub snippet: String,
}

/// 写入或更新一封邮件的信封，返回主键。
///
/// 已存在时只更新**信封类字段**（旗标、主题、摘要…），**不碰 `body_state` 与正文**
/// ——正文已经拉过了，重复覆盖会让阅读器闪回「加载中」。
pub fn upsert_envelope(
    conn: &Connection,
    account_id: i64,
    folder_id: i64,
    env: &EnvelopeRow,
) -> Result<i64> {
    let to_json = serde_json::to_string(&env.to).unwrap_or_else(|_| "[]".into());
    let cc_json = serde_json::to_string(&env.cc).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO messages
           (account_id, folder_id, uid, message_id, subject, from_name, from_addr,
            to_json, cc_json, date, is_read, is_starred, has_attachments, size, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(account_id, folder_id, uid) DO UPDATE SET
             message_id      = excluded.message_id,
             subject         = excluded.subject,
             from_name       = excluded.from_name,
             from_addr       = excluded.from_addr,
             to_json         = excluded.to_json,
             cc_json         = excluded.cc_json,
             date            = excluded.date,
             is_read         = excluded.is_read,
             is_starred      = excluded.is_starred,
             has_attachments = excluded.has_attachments,
             size            = excluded.size,
             snippet         = excluded.snippet",
        params![
            account_id,
            folder_id,
            env.uid,
            env.message_id,
            env.subject,
            env.from_name,
            env.from_addr,
            to_json,
            cc_json,
            env.date,
            env.is_read as i64,
            env.is_starred as i64,
            env.has_attachments as i64,
            env.size,
            env.snippet,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE account_id = ?1 AND folder_id = ?2 AND uid = ?3",
        params![account_id, folder_id, env.uid],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 某文件夹的列表行，按时间倒序，分页。
pub fn list_by_folder(
    conn: &Connection,
    folder_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<MessageSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, folder_id, uid, subject, from_name, from_addr, snippet, date,
                is_read, is_starred, has_attachments
         FROM messages
         WHERE folder_id = ?1
         ORDER BY date DESC, id DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![folder_id, limit, offset], row_to_summary)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 某文件夹的邮件总数。
pub fn count_in_folder(conn: &Connection, folder_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
        [folder_id],
        |row| row.get(0),
    )?)
}

/// 文件夹内已有的全部 UID（删除对账用）。
pub fn existing_uids(conn: &Connection, folder_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT uid FROM messages WHERE folder_id = ?1 AND uid IS NOT NULL")?;
    let rows = stmt.query_map([folder_id], |row| row.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 文件夹内的最大 UID（水位对账用）。
pub fn max_uid(conn: &Connection, folder_id: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MAX(uid) FROM messages WHERE folder_id = ?1",
        [folder_id],
        |row| row.get(0),
    )?)
}

/// 按 UID 批量删除（服务端已删掉的邮件）。
pub fn delete_by_uids(conn: &Connection, folder_id: i64, uids: &[i64]) -> Result<usize> {
    let mut removed = 0usize;
    for uid in uids {
        removed += conn.execute(
            "DELETE FROM messages WHERE folder_id = ?1 AND uid = ?2",
            params![folder_id, uid],
        )?;
    }
    Ok(removed)
}

/// 取一封邮件的详情（含正文与附件）。
pub fn get_detail(conn: &Connection, id: i64) -> Result<MessageDetail> {
    let mut detail = conn
        .query_row(
            "SELECT m.id, m.account_id, m.thread_id, m.folder_id, m.uid, m.message_id,
                    m.subject, m.from_name, m.from_addr, m.to_json, m.cc_json, m.date,
                    m.is_read, m.is_starred, m.has_attachments, m.size, m.body_state,
                    b.text_body, b.html_body
             FROM messages m
             LEFT JOIN message_bodies b ON b.message_id = m.id
             WHERE m.id = ?1",
            [id],
            |row| {
                let to_json: String = row.get(9)?;
                let cc_json: String = row.get(10)?;
                Ok(MessageDetail {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    thread_id: row.get(2)?,
                    folder_id: row.get(3)?,
                    uid: row.get(4)?,
                    message_id: row.get(5)?,
                    subject: row.get(6)?,
                    from_name: row.get(7)?,
                    from_addr: row.get(8)?,
                    to: parse_address_list(&to_json),
                    cc: parse_address_list(&cc_json),
                    date: row.get(11)?,
                    is_read: row.get::<_, i64>(12)? != 0,
                    is_starred: row.get::<_, i64>(13)? != 0,
                    has_attachments: row.get::<_, i64>(14)? != 0,
                    size: row.get(15)?,
                    body_state: row.get(16)?,
                    text_body: row.get(17)?,
                    html_body: row.get(18)?,
                    attachments: Vec::new(),
                })
            },
        )
        .optional()?
        .ok_or_else(|| MailError::NotFound(format!("message {id}")))?;

    detail.attachments = attachments_for(conn, id)?;
    Ok(detail)
}

/// 写正文（幂等 upsert）。`body_state` 置 `cached`。
pub fn save_body(conn: &Connection, id: i64, text: Option<&str>, html: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO message_bodies (message_id, text_body, html_body)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(message_id) DO UPDATE SET
             text_body = excluded.text_body,
             html_body = excluded.html_body",
        params![id, text, html],
    )?;
    conn.execute(
        "UPDATE messages SET body_state = 'cached' WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// 置正文状态（`none` / `fetching` / `cached`）。
pub fn set_body_state(conn: &Connection, id: i64, state: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET body_state = ?2 WHERE id = ?1",
        params![id, state],
    )?;
    Ok(())
}

/// 改已读 / 旗标。
pub fn set_flags(conn: &Connection, id: i64, is_read: bool, is_starred: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_read = ?2, is_starred = ?3 WHERE id = ?1",
        params![id, is_read as i64, is_starred as i64],
    )?;
    Ok(())
}

/// 置「含附件」标记。
///
/// 列表阶段只有头部，猜不出有没有附件（附件可能没有 `filename`、也可能纯靠
/// `Content-Disposition` 区分），所以这一位只能等正文拉下来、MIME 结构解析完再回填。
pub fn set_has_attachments(conn: &Connection, id: i64, has: bool) -> Result<()> {
    conn.execute(
        "UPDATE messages SET has_attachments = ?2 WHERE id = ?1",
        params![id, has as i64],
    )?;
    Ok(())
}

/// 按 UID 置旗标（同步时按服务端结果批量覆盖本地）。
pub fn set_flags_by_uid(
    conn: &Connection,
    folder_id: i64,
    uid: i64,
    is_read: bool,
    is_starred: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE messages SET is_read = ?3, is_starred = ?4
         WHERE folder_id = ?1 AND uid = ?2",
        params![folder_id, uid, is_read as i64, is_starred as i64],
    )?;
    Ok(())
}

/// 替换一封邮件的附件清单（重新解析正文后调用）。
pub fn replace_attachments(
    conn: &Connection,
    message_id: i64,
    attachments: &[NewAttachment],
) -> Result<()> {
    conn.execute(
        "DELETE FROM attachments WHERE message_id = ?1",
        [message_id],
    )?;
    for a in attachments {
        conn.execute(
            "INSERT INTO attachments
               (message_id, filename, mime_type, size, content_id, is_inline,
                file_path, imap_section)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message_id,
                a.filename,
                a.mime_type,
                a.size,
                a.content_id,
                a.is_inline as i64,
                a.file_path,
                a.imap_section,
            ],
        )?;
    }
    Ok(())
}

/// 新附件（入库用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// 文件名。
    pub filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 字节数。
    pub size: i64,
    /// `Content-ID`（去尖括号）。
    pub content_id: Option<String>,
    /// 是否内联。
    pub is_inline: bool,
    /// 本地实体文件路径（未下载为 `None`）。
    pub file_path: Option<String>,
    /// IMAP 段号。
    pub imap_section: Option<String>,
}

/// 某邮件的附件清单。
pub fn attachments_for(conn: &Connection, message_id: i64) -> Result<Vec<AttachmentMeta>> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id, COALESCE(filename, ''), COALESCE(mime_type, ''), COALESCE(size, 0),
                content_id, is_inline, file_path, imap_section
         FROM attachments WHERE message_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([message_id], |row| {
        Ok(AttachmentMeta {
            id: row.get(0)?,
            message_id: row.get(1)?,
            filename: row.get(2)?,
            mime_type: row.get(3)?,
            size: row.get(4)?,
            content_id: row.get(5)?,
            is_inline: row.get::<_, i64>(6)? != 0,
            file_path: row.get(7)?,
            imap_section: row.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn parse_address_list(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        uid: row.get(2)?,
        subject: row.get(3)?,
        from_name: row.get(4)?,
        from_addr: row.get(5)?,
        snippet: row.get(6)?,
        date: row.get(7)?,
        is_read: row.get::<_, i64>(8)? != 0,
        is_starred: row.get::<_, i64>(9)? != 0,
        has_attachments: row.get::<_, i64>(10)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::{accounts, folders};
    use crate::models::{ConnectionSecurity, FolderRole, NewAccount};

    fn mem() -> (Connection, i64, i64) {
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
        let folder = folders::upsert(
            &conn,
            acct,
            &folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: FolderRole::Inbox,
                uidvalidity: Some(1),
                uidnext: Some(10),
            },
        )
        .unwrap();
        (conn, acct, folder)
    }

    fn env(uid: i64, subject: &str, date: i64) -> EnvelopeRow {
        EnvelopeRow {
            uid,
            message_id: Some(format!("<{uid}@example.com>")),
            subject: subject.into(),
            from_name: Some("张三".into()),
            from_addr: Some("z@example.com".into()),
            to: vec!["me@example.com".into()],
            cc: vec![],
            date,
            is_read: false,
            is_starred: false,
            has_attachments: false,
            size: Some(1234),
            snippet: "你好".into(),
        }
    }

    #[test]
    fn upsert_is_idempotent_on_uid() {
        let (conn, acct, folder) = mem();
        let a = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        let b = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        assert_eq!(a, b);
        assert_eq!(count_in_folder(&conn, folder).unwrap(), 1);
    }

    #[test]
    fn re_sync_does_not_reset_cached_body() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        save_body(&conn, id, Some("正文"), None).unwrap();
        // 重启后增量重跑：信封会重写，但正文必须留着。
        upsert_envelope(&conn, acct, folder, &env(1, "Hi", 100)).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert_eq!(d.body_state, "cached");
        assert_eq!(d.text_body.as_deref(), Some("正文"));
    }

    #[test]
    fn list_is_newest_first() {
        let (conn, acct, folder) = mem();
        upsert_envelope(&conn, acct, folder, &env(1, "old", 100)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(2, "new", 300)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(3, "mid", 200)).unwrap();
        let subs: Vec<String> = list_by_folder(&conn, folder, 10, 0)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(subs, vec!["new", "mid", "old"]);
    }

    #[test]
    fn list_paginates() {
        let (conn, acct, folder) = mem();
        for uid in 1..=5 {
            upsert_envelope(&conn, acct, folder, &env(uid, &format!("m{uid}"), uid)).unwrap();
        }
        let page1 = list_by_folder(&conn, folder, 2, 0).unwrap();
        let page2 = list_by_folder(&conn, folder, 2, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].subject, "m5");
        assert_eq!(page2[0].subject, "m3");
    }

    #[test]
    fn sender_falls_back_to_address() {
        let (conn, acct, folder) = mem();
        let mut e = env(1, "x", 1);
        e.from_name = None;
        let id = upsert_envelope(&conn, acct, folder, &e).unwrap();
        let row = list_by_folder(&conn, folder, 1, 0).unwrap().remove(0);
        assert_eq!(row.id, id);
        assert_eq!(row.sender_display(), "z@example.com");
    }

    #[test]
    fn uid_reconciliation_helpers() {
        let (conn, acct, folder) = mem();
        upsert_envelope(&conn, acct, folder, &env(1, "a", 1)).unwrap();
        upsert_envelope(&conn, acct, folder, &env(5, "b", 2)).unwrap();
        assert_eq!(max_uid(&conn, folder).unwrap(), Some(5));
        assert_eq!(existing_uids(&conn, folder).unwrap(), vec![1, 5]);
        // 服务端只认 1：5 该被删。
        let removed = delete_by_uids(&conn, folder, &[5]).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(existing_uids(&conn, folder).unwrap(), vec![1]);
    }

    #[test]
    fn flags_roundtrip_by_uid_and_id() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(7, "x", 1)).unwrap();
        set_flags(&conn, id, true, true).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert!(d.is_read && d.is_starred);
        set_flags_by_uid(&conn, folder, 7, false, true).unwrap();
        let d = get_detail(&conn, id).unwrap();
        assert!(!d.is_read && d.is_starred);
    }

    #[test]
    fn attachments_replace_and_read_back() {
        let (conn, acct, folder) = mem();
        let id = upsert_envelope(&conn, acct, folder, &env(1, "x", 1)).unwrap();
        replace_attachments(
            &conn,
            id,
            &[NewAttachment {
                filename: "报告.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 2048,
                content_id: None,
                is_inline: false,
                file_path: None,
                imap_section: Some("2".into()),
            }],
        )
        .unwrap();
        let list = attachments_for(&conn, id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, "报告.pdf");
        // 重解析时替换而非追加。
        replace_attachments(&conn, id, &[]).unwrap();
        assert!(attachments_for(&conn, id).unwrap().is_empty());
    }

    #[test]
    fn detail_of_missing_message_is_not_found() {
        let (conn, _, _) = mem();
        assert!(matches!(
            get_detail(&conn, 999),
            Err(MailError::NotFound(_))
        ));
    }
}
