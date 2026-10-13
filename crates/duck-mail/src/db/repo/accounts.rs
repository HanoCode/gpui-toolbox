//! `accounts` 表的读写。
//!
//! **口令不在这里**：本表只有脱敏档案（主机、端口、登录名、认证方式）。口令 /
//! 令牌由 [`crate::accounts`] 写进 `duck-platform::secret`。
//!
//! `security`（TLS / STARTTLS / 明文）在 schema 里没有独立列，随 `settings_json`
//! 一起存 —— 见 [`encode_settings`] / [`decode_security`]。这样加新的连接级选项
//! 不必再开迁移。

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{MailError, Result};
use crate::models::{Account, AuthKind, ConnectionSecurity, NewAccount, Provider, SyncState};

/// 插入一个账户，返回新主键。
pub fn insert(conn: &Connection, account: &NewAccount) -> Result<i64> {
    conn.execute(
        "INSERT INTO accounts
           (email, display_name, provider, auth_kind, mail_protocol, username,
            imap_host, imap_port, smtp_host, smtp_port, sync_state, created_at,
            settings_json, sort_order)
         VALUES (?1, ?2, 'imap', 'password', 'imap', ?3,
                 ?4, ?5, ?6, ?7, 'idle', ?8, ?9, ?10)",
        params![
            account.email,
            account.display_name,
            account.username,
            account.imap_host,
            account.imap_port,
            account.smtp_host,
            account.smtp_port,
            now_secs(),
            encode_settings(account.security),
            next_sort_order(conn)?,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 全部账户，按用户排序。
pub fn list(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        "SELECT id, email, display_name, provider, auth_kind, username,
                imap_host, imap_port, smtp_host, smtp_port, sync_state,
                sync_error, created_at, settings_json
         FROM accounts
         ORDER BY sort_order, id",
    )?;
    let rows = stmt.query_map([], row_to_account)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 按主键取账户。
pub fn get(conn: &Connection, id: i64) -> Result<Account> {
    conn.query_row(
        "SELECT id, email, display_name, provider, auth_kind, username,
                imap_host, imap_port, smtp_host, smtp_port, sync_state,
                sync_error, created_at, settings_json
         FROM accounts WHERE id = ?1",
        [id],
        row_to_account,
    )
    .optional()?
    .ok_or_else(|| MailError::NotFound(format!("account {id}")))
}

/// 按邮箱地址取账户（添加时查重）。
pub fn get_by_email(conn: &Connection, email: &str) -> Result<Option<Account>> {
    Ok(conn
        .query_row(
            "SELECT id, email, display_name, provider, auth_kind, username,
                    imap_host, imap_port, smtp_host, smtp_port, sync_state,
                    sync_error, created_at, settings_json
             FROM accounts WHERE email = ?1",
            [email],
            row_to_account,
        )
        .optional()?)
}

/// 改显示名。
pub fn set_display_name(conn: &Connection, id: i64, name: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET display_name = ?2 WHERE id = ?1",
        params![id, name],
    )?;
    Ok(())
}

/// 改连接参数（编辑账户时用）。**不动口令**。
pub fn update_connection(
    conn: &Connection,
    id: i64,
    imap_host: &str,
    imap_port: u16,
    smtp_host: &str,
    smtp_port: u16,
    security: ConnectionSecurity,
) -> Result<()> {
    conn.execute(
        "UPDATE accounts
         SET imap_host = ?2, imap_port = ?3, smtp_host = ?4, smtp_port = ?5,
             settings_json = ?6
         WHERE id = ?1",
        params![
            id,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            encode_settings(security)
        ],
    )?;
    Ok(())
}

/// 编辑账户：档案字段整体更新（编辑弹窗的落库点）。**不动口令与凭据**。
///
/// `provider` / `auth_kind` / `mail_protocol` 保持原值：编辑是改连接参数，不是
/// 换协议——改协议是「删了重加」的事。
pub fn update_profile(conn: &Connection, id: i64, account: &NewAccount) -> Result<()> {
    let n = conn.execute(
        "UPDATE accounts
         SET email = ?2, display_name = ?3, username = ?4,
             imap_host = ?5, imap_port = ?6, smtp_host = ?7, smtp_port = ?8,
             settings_json = ?9
         WHERE id = ?1",
        params![
            id,
            account.email,
            account.display_name,
            account.username,
            account.imap_host,
            account.imap_port,
            account.smtp_host,
            account.smtp_port,
            encode_settings(account.security)
        ],
    )?;
    if n == 0 {
        return Err(MailError::NotFound(format!("account {id}")));
    }
    Ok(())
}

/// 写同步状态。`error` 为 `None` 时清空失败原因。
pub fn set_sync_state(
    conn: &Connection,
    id: i64,
    state: SyncState,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET sync_state = ?2, sync_error = ?3 WHERE id = ?1",
        params![id, state.wire_name(), error],
    )?;
    Ok(())
}

/// 删除账户。外键 `ON DELETE CASCADE` 会带走它的文件夹 / 线程 / 邮件。
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let n = conn.execute("DELETE FROM accounts WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(MailError::NotFound(format!("account {id}")));
    }
    Ok(())
}

fn next_sort_order(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM accounts",
        [],
        |row| row.get(0),
    )?)
}

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    let provider: String = row.get(3)?;
    let auth_kind: String = row.get(4)?;
    let sync_state: String = row.get(10)?;
    let settings: String = row.get(13)?;
    Ok(Account {
        id: row.get(0)?,
        email: row.get(1)?,
        display_name: row.get(2)?,
        provider: Provider::from_wire(&provider),
        auth_kind: AuthKind::from_wire(&auth_kind),
        username: row.get(5)?,
        imap_host: row.get(6)?,
        imap_port: row.get(7)?,
        smtp_host: row.get(8)?,
        smtp_port: row.get(9)?,
        sync_state: SyncState::from_wire(&sync_state),
        sync_error: row.get(11)?,
        created_at: row.get(12)?,
        security: decode_security(&settings),
    })
}

fn encode_settings(security: ConnectionSecurity) -> String {
    serde_json::json!({ "security": security.wire_name() }).to_string()
}

fn decode_security(settings_json: &str) -> ConnectionSecurity {
    serde_json::from_str::<serde_json::Value>(settings_json)
        .ok()
        .and_then(|v| {
            v.get("security")
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        })
        .map(|s| ConnectionSecurity::from_wire(&s))
        .unwrap_or_default()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        conn
    }

    fn sample(email: &str) -> NewAccount {
        NewAccount {
            email: email.into(),
            display_name: Some("工作".into()),
            username: email.into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            security: ConnectionSecurity::Tls,
            password: "secret".into(),
        }
    }

    #[test]
    fn insert_list_get_roundtrip() {
        let conn = mem();
        let id = insert(&conn, &sample("a@example.com")).unwrap();
        let all = list(&conn).unwrap();
        assert_eq!(all.len(), 1);
        let got = get(&conn, id).unwrap();
        assert_eq!(got.email, "a@example.com");
        assert_eq!(got.provider, Provider::Imap);
        assert_eq!(got.auth_kind, AuthKind::Password);
        assert_eq!(got.security, ConnectionSecurity::Tls);
        assert_eq!(got.imap_port, 993);
    }

    #[test]
    fn email_is_unique() {
        let conn = mem();
        insert(&conn, &sample("dup@example.com")).unwrap();
        // UNIQUE 约束必须挡下来——添加向导靠它防重复添加。
        assert!(insert(&conn, &sample("dup@example.com")).is_err());
    }

    #[test]
    fn password_is_never_stored_in_sqlite() {
        let conn = mem();
        let id = insert(&conn, &sample("p@example.com")).unwrap();
        // 把整行的每一列都导成文本，确认口令字符串不在里面。
        let settings: String = conn
            .query_row(
                "SELECT settings_json FROM accounts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!settings.contains("secret"));
        let dump: String = conn
            .query_row(
                "SELECT email || COALESCE(display_name,'') || username || settings_json
                 FROM accounts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !dump.contains("secret"),
            "credential leaked into accounts row"
        );
    }

    #[test]
    fn sync_state_and_error_roundtrip() {
        let conn = mem();
        let id = insert(&conn, &sample("s@example.com")).unwrap();
        set_sync_state(&conn, id, SyncState::Error, Some("connection refused")).unwrap();
        let got = get(&conn, id).unwrap();
        assert_eq!(got.sync_state, SyncState::Error);
        assert_eq!(got.sync_error.as_deref(), Some("connection refused"));
        // 恢复时清空错误。
        set_sync_state(&conn, id, SyncState::Idle, None).unwrap();
        assert!(get(&conn, id).unwrap().sync_error.is_none());
    }

    #[test]
    fn security_survives_settings_json() {
        let conn = mem();
        let mut a = sample("starttls@example.com");
        a.security = ConnectionSecurity::StartTls;
        let id = insert(&conn, &a).unwrap();
        assert_eq!(
            get(&conn, id).unwrap().security,
            ConnectionSecurity::StartTls
        );
        update_connection(
            &conn,
            id,
            "imap.new.com",
            143,
            "smtp.new.com",
            587,
            ConnectionSecurity::StartTls,
        )
        .unwrap();
        let got = get(&conn, id).unwrap();
        assert_eq!(got.imap_host, "imap.new.com");
        assert_eq!(got.security, ConnectionSecurity::StartTls);
    }

    #[test]
    fn update_profile_rewrites_every_editable_field() {
        let conn = mem();
        let id = insert(&conn, &sample("old@example.com")).unwrap();
        let mut edited = sample("new@example.com");
        edited.display_name = Some("新名".into());
        edited.username = "login@other.com".into();
        edited.imap_host = "imap.new.com".into();
        edited.imap_port = 143;
        edited.smtp_host = "smtp.new.com".into();
        edited.smtp_port = 587;
        edited.security = ConnectionSecurity::StartTls;
        update_profile(&conn, id, &edited).unwrap();

        let got = get(&conn, id).unwrap();
        assert_eq!(got.email, "new@example.com");
        assert_eq!(got.display_name.as_deref(), Some("新名"));
        assert_eq!(got.username, "login@other.com");
        assert_eq!(got.imap_host, "imap.new.com");
        assert_eq!(got.imap_port, 143);
        assert_eq!(got.smtp_host, "smtp.new.com");
        assert_eq!(got.smtp_port, 587);
        assert_eq!(got.security, ConnectionSecurity::StartTls);
        // 编辑是改参数不是换协议：provider / auth_kind 保持原值。
        assert_eq!(got.provider, Provider::Imap);
        assert_eq!(got.auth_kind, AuthKind::Password);
        // 编辑后的新地址不再与另一账户冲突（换址后的查重由 `accounts::update` 做）。
        assert!(get_by_email(&conn, "new@example.com").unwrap().is_some());
        // 不存在的主键报 NotFound，而不是静默成功。
        assert!(matches!(
            update_profile(&conn, 999, &edited),
            Err(MailError::NotFound(_))
        ));
    }

    #[test]
    fn delete_removes_row_and_get_reports_not_found() {
        let conn = mem();
        let id = insert(&conn, &sample("d@example.com")).unwrap();
        delete(&conn, id).unwrap();
        assert!(matches!(get(&conn, id), Err(MailError::NotFound(_))));
        assert!(matches!(delete(&conn, id), Err(MailError::NotFound(_))));
    }
}
