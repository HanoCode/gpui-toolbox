//! `labels` / `message_labels` 的读写。
//!
//! # 归属与 CHECK 约束
//!
//! `007_account_label_ownership.sql` 给 `labels` 加了一条**自洽 CHECK**：
//!
//! - `scope='account'` ⟺ `owner_account_id` 非空且 `is_auto=0`；
//! - `scope='global'` ⟺ owner 为空且 `is_auto=0`；
//! - `scope='automatic'` ⟺ owner 为空且 `is_auto=1`。
//!
//! [`create`] **不给调用方乱填 `scope` / `is_auto` / `origin` 的机会**：只收
//! `owner_account_id: Option<i64>`，其余三列由它按上面第一 / 第二条规则补全
//! （`origin='user'`）。这样「创建出来的行必然满足 CHECK」是类型层面的，不靠运行时校验。
//! `automatic`（AI 自动分类标签）属 M3，本模块不产。
//!
//! # 可见性
//!
//! [`list_for_account`] 返回「该账号自己的账户级标签 + 全部全局标签」。`automatic`
//! 标签是系统自动分类用的，不进用户可见列表（M3 的 AI 面板另有入口）。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};
use crate::models::Label;

/// 新建标签的入参。
///
/// `scope` / `is_auto` / `origin` **不在**这里——它们由 `owner_account_id` 派生
/// （见模块头），以免调用方拼出一条违反 CHECK 的行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLabel {
    /// 标签名。
    pub name: String,
    /// 颜色（`#RRGGBB`）。
    pub color: String,
    /// IMAP keyword（同步到服务端时用的原子名；本地也必须非空——列是 `NOT NULL`）。
    pub keyword: String,
    /// 排序位（越小越前）。
    pub position: i64,
    /// `Some(account)` = 账户级；`None` = 全局。
    pub owner_account_id: Option<i64>,
}

impl NewLabel {
    /// 账户级标签。
    pub fn account(account_id: i64, name: impl Into<String>, color: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            keyword: keyword_for(&name),
            name,
            color: color.into(),
            position: 100,
            owner_account_id: Some(account_id),
        }
    }

    /// 全局标签。
    pub fn global(name: impl Into<String>, color: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            keyword: keyword_for(&name),
            name,
            color: color.into(),
            position: 100,
            owner_account_id: None,
        }
    }
}

/// 由标签名派生一个稳定的 IMAP keyword。
///
/// IMAP 原子不能含空格 / 特殊字符，且惯例上加前缀区分本地标签。非 ASCII（中文标签名）
/// 会被折叠成 `_`，所以两个中文名可能得到同一个 keyword——但 keyword **不是唯一键**
/// （`labels` 表没有 UNIQUE 约束），撞了也不影响本地读写；服务端同步（M2 末）才需要
/// 处理冲突。这里先保证「非空 + 合法字符集」。
fn keyword_for(name: &str) -> String {
    let mut out = String::from("DuckMail");
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.len() == "DuckMail".len() {
        out.push_str("Label");
    }
    out
}

/// 创建一个标签，返回主键。
///
/// **按 `(owner, name)` 去重**：同账户下已有同名标签时直接返回它的主键（不改颜色），
/// 避免界面反复点「新建」堆出一串同名标签。`global` 级也按 `owner IS NULL` 去重。
pub fn create(conn: &Connection, label: &NewLabel) -> Result<i64> {
    if let Some(existing) = find_by_name(conn, label.owner_account_id, &label.name)? {
        return Ok(existing);
    }
    let (scope, owner) = match label.owner_account_id {
        Some(account_id) => ("account", Some(account_id)),
        None => ("global", None),
    };
    conn.execute(
        "INSERT INTO labels (name, color, keyword, position, is_auto, scope, owner_account_id, origin)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, 'user')",
        params![
            label.name,
            label.color,
            label.keyword,
            label.position,
            scope,
            owner,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按 `(owner, name)` 找一个标签（`owner` 为 `None` 时找全局同名）。
pub fn find_by_name(conn: &Connection, owner: Option<i64>, name: &str) -> Result<Option<i64>> {
    let id: Option<i64> = match owner {
        Some(account_id) => conn
            .query_row(
                "SELECT id FROM labels
                 WHERE scope = 'account' AND owner_account_id = ?1 AND name = ?2
                 ORDER BY id LIMIT 1",
                params![account_id, name],
                |row| row.get(0),
            )
            .optional()?,
        None => conn
            .query_row(
                "SELECT id FROM labels
                 WHERE scope = 'global' AND name = ?1
                 ORDER BY id LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .optional()?,
    };
    Ok(id)
}

/// 该账号可见的标签：自己的账户级 + 全部全局级（按 `position` / 名字排）。
pub fn list_for_account(conn: &Connection, account_id: i64) -> Result<Vec<Label>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, color, keyword FROM labels
         WHERE scope = 'global' OR (scope = 'account' AND owner_account_id = ?1)
         ORDER BY position, name, id",
    )?;
    let rows = stmt.query_map([account_id], |row| {
        Ok(Label {
            id: row.get(0)?,
            name: row.get(1)?,
            color: row.get(2)?,
            keyword: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 按主键取标签。
pub fn get(conn: &Connection, id: i64) -> Result<Label> {
    conn.query_row(
        "SELECT id, name, color, keyword FROM labels WHERE id = ?1",
        [id],
        |row| {
            Ok(Label {
                id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
                keyword: row.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("label {id}")))
}

/// 给邮件贴标签（**幂等**：重复贴同一对不报错）。
///
/// 邮件或标签不存在时由外键报错（`MailError::Db`）——这是调用方的 bug，不该静默吞掉。
pub fn attach(conn: &Connection, message_id: i64, label_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO message_labels (message_id, label_id) VALUES (?1, ?2)",
        params![message_id, label_id],
    )?;
    Ok(())
}

/// 摘掉邮件的某个标签（**幂等**：本来就没贴也不报错）。
pub fn detach(conn: &Connection, message_id: i64, label_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
        params![message_id, label_id],
    )?;
    Ok(())
}

/// 邮件是否已贴某标签。
pub fn is_attached(conn: &Connection, message_id: i64, label_id: i64) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message_labels WHERE message_id = ?1 AND label_id = ?2",
        params![message_id, label_id],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// 某邮件已贴的标签。
pub fn labels_for_message(conn: &Connection, message_id: i64) -> Result<Vec<Label>> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.name, l.color, l.keyword
         FROM labels l
         JOIN message_labels ml ON ml.label_id = l.id
         WHERE ml.message_id = ?1
         ORDER BY l.position, l.name, l.id",
    )?;
    let rows = stmt.query_map([message_id], |row| {
        Ok(Label {
            id: row.get(0)?,
            name: row.get(1)?,
            color: row.get(2)?,
            keyword: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 删标签；`message_labels` 里的关联行靠 `ON DELETE CASCADE` 一起清。（不进动作队列：
/// 标签本身的增删是本地结构，同步到服务端（如 Gmail label）是 M2 末的事。）
pub fn delete(conn: &Connection, label_id: i64) -> Result<()> {
    conn.execute("DELETE FROM labels WHERE id = ?1", [label_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::repo::{accounts, messages};
    use crate::models::{ConnectionSecurity, NewAccount};

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
        let folder = crate::db::repo::folders::upsert(
            &conn,
            acct,
            &crate::db::repo::folders::FolderUpsert {
                name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: crate::models::FolderRole::Inbox,
                uidvalidity: Some(1),
                uidnext: Some(10),
            },
        )
        .unwrap();
        let msg = messages::upsert_envelope(
            &conn,
            acct,
            folder,
            &messages::EnvelopeRow {
                uid: 1,
                subject: "hi".into(),
                date: 1,
                ..Default::default()
            },
        )
        .unwrap();
        (conn, acct, msg)
    }

    #[test]
    fn create_account_and_global_labels_satisfy_the_check_constraint() {
        let (conn, acct, _) = mem();
        let a = create(&conn, &NewLabel::account(acct, "工作", "#ff0000")).unwrap();
        let g = create(&conn, &NewLabel::global("重要", "#00ff00")).unwrap();
        assert_ne!(a, g);
        // 账户级 + 全局级都能被该账户看见（且自动分类的种子标签不出现）。
        let visible = list_for_account(&conn, acct).unwrap();
        let names: Vec<&str> = visible.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"工作"));
        assert!(names.contains(&"重要"));
    }

    #[test]
    fn create_is_deduped_by_owner_and_name() {
        let (conn, acct, _) = mem();
        let a = create(&conn, &NewLabel::account(acct, "工作", "#ff0000")).unwrap();
        let b = create(&conn, &NewLabel::account(acct, "工作", "#00ff00")).unwrap();
        assert_eq!(a, b, "同名账户级标签复用同一行");
        assert_eq!(list_for_account(&conn, acct).unwrap().len(), 1);
    }

    #[test]
    fn global_labels_are_visible_to_every_account() {
        let (conn, acct, _) = mem();
        create(&conn, &NewLabel::global("重要", "#00ff00")).unwrap();
        // 换个账户也看得见全局标签。
        let other = accounts::insert(
            &conn,
            &NewAccount {
                email: "b@example.com".into(),
                display_name: None,
                username: "b@example.com".into(),
                imap_host: "h".into(),
                imap_port: 993,
                smtp_host: "s".into(),
                smtp_port: 465,
                security: ConnectionSecurity::Tls,
                password: "p".into(),
            },
        )
        .unwrap();
        assert!(
            !list_for_account(&conn, acct).unwrap().is_empty(),
            "本账户应看到全局标签"
        );
        let other_visible = list_for_account(&conn, other).unwrap();
        assert_eq!(other_visible.len(), 1);
        assert_eq!(other_visible[0].name, "重要");
    }

    #[test]
    fn attach_is_idempotent_and_detach_is_total() {
        let (conn, acct, msg) = mem();
        let label = create(&conn, &NewLabel::account(acct, "工作", "#ff0000")).unwrap();
        attach(&conn, msg, label).unwrap();
        attach(&conn, msg, label).unwrap(); // 重复贴不报错
        assert_eq!(labels_for_message(&conn, msg).unwrap().len(), 1);
        // 摘两次也不报错。
        detach(&conn, msg, label).unwrap();
        detach(&conn, msg, label).unwrap();
        assert!(labels_for_message(&conn, msg).unwrap().is_empty());
        assert!(!is_attached(&conn, msg, label).unwrap());
    }

    #[test]
    fn deleting_a_label_cascades_its_message_links() {
        let (conn, acct, msg) = mem();
        let label = create(&conn, &NewLabel::account(acct, "工作", "#ff0000")).unwrap();
        attach(&conn, msg, label).unwrap();
        delete(&conn, label).unwrap();
        // 级联清掉了关联行（否则列表查询会撞 dangling label_id）。
        assert!(labels_for_message(&conn, msg).unwrap().is_empty());
        assert!(matches!(get(&conn, label), Err(MailError::NotFound(_))));
    }

    #[test]
    fn list_by_label_returns_tagged_messages() {
        let (conn, acct, msg) = mem();
        let label = create(&conn, &NewLabel::account(acct, "工作", "#ff0000")).unwrap();
        attach(&conn, msg, label).unwrap();
        let rows = messages::list_by_label(&conn, label, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, msg);
    }

    #[test]
    fn raw_insert_violating_the_scope_check_is_rejected() {
        // 直接绕过 `create` 拼一条自相矛盾的行（account 级但没有 owner）：CHECK 必须拦下。
        let (conn, _, _) = mem();
        let result = conn.execute(
            "INSERT INTO labels (name, color, keyword, position, is_auto, scope, owner_account_id, origin)
             VALUES ('坏标签', '#000000', 'K', 0, 0, 'account', NULL, 'user')",
            [],
        );
        assert!(result.is_err(), "CHECK 约束应拒绝 account 级无 owner 的行");
    }
}
