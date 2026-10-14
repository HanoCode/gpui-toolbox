//! `folders` 表的读写。
//!
//! 文件夹是「同步的水位记录器」：`uidvalidity` 变了说明服务端把整个文件夹重建了
//! （本地缓存必须整体作废），`uidnext` 是增量拉取的**上界**，`last_seen_uid` 是
//! 本地已经拉到的位置。三个值一起构成增量同步的正确性基础
//! （`docs/email-design.md` §4.6）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};
use crate::models::{Folder, FolderRole};

/// `LIST` 回来后要写进库的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderUpsert {
    /// IMAP 上的原始名字。
    pub name: String,
    /// 层级分隔符。
    pub delimiter: Option<String>,
    /// 角色。
    pub role: FolderRole,
    /// 服务端声明的 UIDVALIDITY。
    pub uidvalidity: Option<i64>,
    /// 服务端声明的 UIDNEXT。
    pub uidnext: Option<i64>,
}

/// 写入或更新一个文件夹，返回主键。
///
/// 冲突键是 `(account_id, imap_name)`。**故意不覆盖 `last_seen_uid`**——那是本地
/// 进度，服务端给不出来；覆盖它会退化成「每次 `LIST` 都重拉全部」。
///
/// `uidvalidity` / `uidnext` 用 `COALESCE(excluded, 旧值)`：`LIST` 阶段拿不到这两个值
/// （要 `SELECT` 之后才知道），调用方传 `None` 时**保留旧值**而不是清空——否则每次
/// `LIST` 都会把水位信息抹掉，接着就是全量重拉。
pub fn upsert(conn: &Connection, account_id: i64, folder: &FolderUpsert) -> Result<i64> {
    conn.execute(
        "INSERT INTO folders (account_id, imap_name, delimiter, role, uidvalidity, uidnext)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, imap_name) DO UPDATE SET
             delimiter   = excluded.delimiter,
             role        = excluded.role,
             uidvalidity = COALESCE(excluded.uidvalidity, folders.uidvalidity),
             uidnext     = COALESCE(excluded.uidnext, folders.uidnext)",
        params![
            account_id,
            folder.name,
            folder.delimiter,
            folder.role.wire_name(),
            folder.uidvalidity,
            folder.uidnext,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM folders WHERE account_id = ?1 AND imap_name = ?2",
        params![account_id, folder.name],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 某账户的全部文件夹，按角色权重 + 名字排。
pub fn list(conn: &Connection, account_id: i64) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.account_id, f.imap_name, f.delimiter, f.role,
                f.uidvalidity, f.uidnext, f.last_seen_uid,
                (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id AND m.is_read = 0),
                (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id)
         FROM folders f
         WHERE f.account_id = ?1 AND f.selectable = 1
         ORDER BY f.id",
    )?;
    let rows = stmt.query_map([account_id], row_to_folder)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    // 角色权重排序放在 Rust 侧：SQL 里表达「Inbox < Sent < Drafts < …」只能写
    // 一长串 CASE，可读性差且加角色要改 SQL。
    out.sort_by(|a, b| {
        a.role
            .order()
            .cmp(&b.role.order())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// 按主键取文件夹。
pub fn get(conn: &Connection, id: i64) -> Result<Folder> {
    conn.query_row(
        "SELECT f.id, f.account_id, f.imap_name, f.delimiter, f.role,
                f.uidvalidity, f.uidnext, f.last_seen_uid,
                (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id AND m.is_read = 0),
                (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id)
         FROM folders f WHERE f.id = ?1",
        [id],
        row_to_folder,
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("folder {id}")))
}

/// 按角色取文件夹（收件箱优先）。
pub fn find_by_role(
    conn: &Connection,
    account_id: i64,
    role: FolderRole,
) -> Result<Option<Folder>> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND role = ?2 ORDER BY id LIMIT 1",
            params![account_id, role.wire_name()],
            |row| row.get(0),
        )
        .optional()?;
    match id {
        Some(id) => Ok(Some(get(conn, id)?)),
        None => Ok(None),
    }
}

/// 写同步水位。
///
/// `uidvalidity` 变化时要**同时清空 `last_seen_uid`**——服务端重建了文件夹，
/// 旧 UID 全部失效，沿用它们会拉到错误的邮件。
pub fn set_uid_state(
    conn: &Connection,
    id: i64,
    uidvalidity: Option<i64>,
    uidnext: Option<i64>,
    last_seen_uid: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE folders
         SET uidvalidity = ?2, uidnext = ?3, last_seen_uid = ?4
         WHERE id = ?1",
        params![id, uidvalidity, uidnext, last_seen_uid],
    )?;
    Ok(())
}

/// 读回 `uidvalidity`（判断是否需要整folder作废）。
pub fn uidvalidity(conn: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT uidvalidity FROM folders WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten())
}

/// 某文件夹的**正文回填进度**。
///
/// `folders` 表的 `backfill_cursor` / `backfill_done` 两列在 001 迁移里就建好了
/// （对应 `docs/email-design.md` §4.6「首轮不把几万封正文全拖下来」的取舍），但直到
/// 正文自动下载落地才出现第一个写者：**按本地行 id 从新到老推进**，推到底置 `done`，
/// 之后每轮同步只做「最新若干封」的预取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackfillState {
    /// 已经回填到哪。语义就是 SQL 里的 `messages.id < cursor`；`None` = 还没开始。
    pub cursor: Option<i64>,
    /// 历史正文是否已经回填干净（再取一批为空即置位）。
    pub done: bool,
}

/// 读一个文件夹的正文回填进度。
pub fn backfill_state(conn: &Connection, id: i64) -> Result<BackfillState> {
    conn.query_row(
        "SELECT backfill_cursor, backfill_done FROM folders WHERE id = ?1",
        [id],
        |row| {
            Ok(BackfillState {
                cursor: row.get(0)?,
                done: row.get::<_, i64>(1)? != 0,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("folder {id}")))
}

/// 写正文回填进度（游标 + 是否已完成）。
pub fn set_backfill_state(
    conn: &Connection,
    id: i64,
    cursor: Option<i64>,
    done: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE folders SET backfill_cursor = ?2, backfill_done = ?3 WHERE id = ?1",
        params![id, cursor, done as i64],
    )?;
    Ok(())
}

/// 删除本账户里**不在 `keep` 中**的文件夹（服务端已删掉的）。
///
/// 返回删掉的条数。邮件随外键级联删除。
pub fn delete_missing(conn: &Connection, account_id: i64, keep: &[String]) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT id, imap_name FROM folders WHERE account_id = ?1")?;
    let existing: Vec<(i64, String)> = stmt
        .query_map([account_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut removed = 0usize;
    for (id, name) in existing {
        if !keep.iter().any(|k| k == &name) {
            conn.execute("DELETE FROM folders WHERE id = ?1", [id])?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn row_to_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    let role: Option<String> = row.get(4)?;
    Ok(Folder {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        delimiter: row.get(3)?,
        role: role
            .as_deref()
            .map(FolderRole::from_wire)
            .unwrap_or(FolderRole::Other),
        uidvalidity: row.get(5)?,
        uidnext: row.get(6)?,
        last_seen_uid: row.get(7)?,
        unread: row.get(8)?,
        total: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::models::{ConnectionSecurity, NewAccount};

    fn mem() -> (Connection, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations::run(&mut conn).unwrap();
        let id = crate::db::repo::accounts::insert(
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
        (conn, id)
    }

    fn f(name: &str, role: FolderRole) -> FolderUpsert {
        FolderUpsert {
            name: name.into(),
            delimiter: Some("/".into()),
            role,
            uidvalidity: Some(1),
            uidnext: Some(100),
        }
    }

    #[test]
    fn upsert_is_idempotent_and_keyed_on_name() {
        let (conn, acct) = mem();
        let a = upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        let b = upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        assert_eq!(a, b, "same (account, name) must reuse the row");
        assert_eq!(list(&conn, acct).unwrap().len(), 1);
    }

    #[test]
    fn upsert_preserves_local_sync_watermark() {
        let (conn, acct) = mem();
        let id = upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        set_uid_state(&conn, id, Some(1), Some(100), 42).unwrap();
        // 再 LIST 一次不该把水位打回 0——否则每次都全量重拉。
        upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        assert_eq!(get(&conn, id).unwrap().last_seen_uid, 42);
    }

    #[test]
    fn list_sorts_inbox_first_then_by_name() {
        let (conn, acct) = mem();
        upsert(&conn, acct, &f("Trash", FolderRole::Trash)).unwrap();
        upsert(&conn, acct, &f("Work", FolderRole::Other)).unwrap();
        upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        upsert(&conn, acct, &f("Drafts", FolderRole::Drafts)).unwrap();
        let names: Vec<String> = list(&conn, acct)
            .unwrap()
            .into_iter()
            .map(|x| x.name)
            .collect();
        assert_eq!(names, vec!["INBOX", "Drafts", "Trash", "Work"]);
    }

    #[test]
    fn find_by_role_returns_none_when_absent() {
        let (conn, acct) = mem();
        assert!(
            find_by_role(&conn, acct, FolderRole::Inbox)
                .unwrap()
                .is_none()
        );
        upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        let found = find_by_role(&conn, acct, FolderRole::Inbox)
            .unwrap()
            .unwrap();
        assert_eq!(found.name, "INBOX");
    }

    #[test]
    fn uidvalidity_is_readable_for_invalidation_decisions() {
        let (conn, acct) = mem();
        let id = upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        assert_eq!(uidvalidity(&conn, id).unwrap(), Some(1));
        // 服务端重建文件夹 → UIDVALIDITY 变了，调用方据此清空本地。
        let mut next = f("INBOX", FolderRole::Inbox);
        next.uidvalidity = Some(2);
        upsert(&conn, acct, &next).unwrap();
        assert_eq!(uidvalidity(&conn, id).unwrap(), Some(2));
    }

    /// 正文回填进度：新文件夹是「没推过游标、也没推完」，写完能原样读回，
    /// 且**不与增量同步的水位串台**。
    #[test]
    fn backfill_state_round_trips_without_touching_the_sync_watermark() {
        let (conn, acct) = mem();
        let id = upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        assert_eq!(
            backfill_state(&conn, id).unwrap(),
            BackfillState {
                cursor: None,
                done: false
            }
        );

        set_uid_state(&conn, id, Some(1), Some(9), 8).unwrap();
        set_backfill_state(&conn, id, Some(42), true).unwrap();
        let state = backfill_state(&conn, id).unwrap();
        assert_eq!(state.cursor, Some(42));
        assert!(state.done);
        // 两套进度各管各的：回填推到底不该动 `last_seen_uid`——那是增量同步的水位，
        // 被回填改掉会让「拉新邮件」从错误的位置继续。
        assert_eq!(get(&conn, id).unwrap().last_seen_uid, 8);
    }

    #[test]
    fn delete_missing_prunes_only_absent_folders() {
        let (conn, acct) = mem();
        upsert(&conn, acct, &f("INBOX", FolderRole::Inbox)).unwrap();
        upsert(&conn, acct, &f("Old", FolderRole::Other)).unwrap();
        let removed = delete_missing(&conn, acct, &["INBOX".to_string()]).unwrap();
        assert_eq!(removed, 1);
        let names: Vec<String> = list(&conn, acct)
            .unwrap()
            .into_iter()
            .map(|x| x.name)
            .collect();
        assert_eq!(names, vec!["INBOX"]);
    }
}
