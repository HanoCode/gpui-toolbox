//! 账户层：**档案落 SQLite（脱敏）+ 凭据落 `duck-platform::secret`**。
//!
//! # 两条硬纪律
//!
//! 1. **口令 / 令牌绝不进 SQLite**（`docs/email-design.md` §4.7，沿用 `duck-db` §4.3
//!    与 `duck-redis` 的约定）。`accounts` 表里只有主机、端口、登录名、认证方式——
//!    任何能在数据库文件里被 grep 到的机密都是缺陷。
//! 2. **不用系统钥匙串**（2026-10-12 起的仓级口径）。凭据走 `duck-platform::secret`，
//!    即应用自有的 `credentials.json`（0600 权限位）；dev 构建下 ad-hoc 签名每次都变，
//!    系统钥匙串的 ACL 必然失配、每次读都弹框。
//!
//! 凭据后端**不在这里写死**：每个碰凭据的函数都收一个 [`Secrets`] 参数，生产传
//! `secrets::platform()`、测试传 `secrets::memory()`（见 [`crate::secrets`] 的说明）。
//!
//! # 写入顺序
//!
//! `add` 是「先入库拿 id → 再用 id 写凭据」——凭据的键名要带 `account_id`，只能
//! 先有 id。第二步失败就**回滚第一步**（删掉刚插的行），不允许出现「有档案没口令」
//! 的僵尸账户：那种账户连不上、删不掉、还会在左栏占一个位。

use crate::db::{MailDb, repo};
use crate::error::{MailError, Result};
use crate::models::{Account, ConnectionSecurity, NewAccount};
use crate::secrets::Secrets;

/// 凭据键名前缀（稳定，别改——老用户的密文按这个键名存着）。
const SECRET_PREFIX: &str = "mail";

/// IMAP/SMTP 口令的凭据键名。
pub fn password_key(account_id: i64) -> String {
    format!("{SECRET_PREFIX}:{account_id}:password")
}

/// OAuth2 长期令牌的凭据键名（M2 用）。
pub fn refresh_token_key(account_id: i64) -> String {
    format!("{SECRET_PREFIX}:{account_id}:refresh_token")
}

/// 写口令。
pub fn store_password(secrets: &Secrets, account_id: i64, password: &str) -> Result<()> {
    secrets.set(&password_key(account_id), password)?;
    Ok(())
}

/// 读口令。没存过返回 `None`（不是错误）。
pub fn load_password(secrets: &Secrets, account_id: i64) -> Result<Option<String>> {
    Ok(secrets.get(&password_key(account_id))?)
}

/// 删凭据（口令 + 令牌）。删除账户时调；键本来就不存在不算失败。
pub fn delete_credentials(secrets: &Secrets, account_id: i64) -> Result<()> {
    for key in [password_key(account_id), refresh_token_key(account_id)] {
        // `SecretStore::delete` 对不存在的键是幂等的（见 trait 契约）；
        // 若某天它改成报错，这里也不该让「删账户」失败——凭据残留比删不掉账户好处理。
        if let Err(e) = secrets.delete(&key) {
            tracing::warn!(key, error = %e, "failed to delete mail credential");
        }
    }
    Ok(())
}

/// 校验新账户的入参。**在落库之前调**——失败不留痕。
pub fn validate(new: &NewAccount) -> Result<()> {
    let email = new.email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(MailError::Config("email address is missing '@'".into()));
    }
    if new.username.trim().is_empty() {
        return Err(MailError::Config("username is empty".into()));
    }
    if new.imap_host.trim().is_empty() {
        return Err(MailError::Config("imap host is empty".into()));
    }
    if new.imap_port == 0 {
        return Err(MailError::Config("imap port must be 1..=65535".into()));
    }
    if new.smtp_host.trim().is_empty() {
        return Err(MailError::Config("smtp host is empty".into()));
    }
    if new.smtp_port == 0 {
        return Err(MailError::Config("smtp port must be 1..=65535".into()));
    }
    if new.security == ConnectionSecurity::Plain && !is_local_host(&new.imap_host) {
        // 明文发口令到公网是最常见的凭据泄露路径。默认挡掉，只放行本机测试服务器
        // （stalwart / dovecot 的本地联调场景，见设计文档 §2.4④）。
        return Err(MailError::Config(format!(
            "refusing to send credentials in cleartext to non-local host '{}'",
            new.imap_host
        )));
    }
    Ok(())
}

fn is_local_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    h == "localhost"
        || h == "127.0.0.1"
        || h == "::1"
        || h.starts_with("127.")
        || h.ends_with(".local")
}

/// 新增账户：校验 → 落库 → 写凭据。
pub fn add(db: &MailDb, secrets: &Secrets, new: &NewAccount) -> Result<i64> {
    validate(new)?;

    // 查重：同一邮箱只允许一个账户（界面「添加」按钮靠这条防手抖重复点）。
    if db
        .read(|conn| repo::accounts::get_by_email(conn, new.email.trim()))?
        .is_some()
    {
        return Err(MailError::Config(format!(
            "account for {} already exists",
            new.email.trim()
        )));
    }

    let id = db.write_tx(|tx| repo::accounts::insert(tx, new))?;

    if let Err(e) = store_password(secrets, id, &new.password) {
        // 回滚档案，别留僵尸账户。
        let rollback = db.write_tx(|tx| repo::accounts::delete(tx, id));
        if let Err(re) = rollback {
            tracing::error!(account_id = id, error = %re, "rollback after credential failure also failed");
        }
        return Err(e);
    }

    tracing::info!(account_id = id, email = %new.email, "mail account added");
    Ok(id)
}

/// 删除账户：先删凭据，再删档案（档案删除会级联带走邮件）。
///
/// 顺序有意这样排：万一中途失败，剩下的是「无凭据的死档案」（用户看得见、能再删），
/// 而不是「孤儿凭据」（用户看不见、永远清不掉）。
pub fn remove(db: &MailDb, secrets: &Secrets, account_id: i64) -> Result<()> {
    delete_credentials(secrets, account_id)?;
    db.write_tx(|tx| repo::accounts::delete(tx, account_id))?;
    tracing::info!(account_id, "mail account removed");
    Ok(())
}

/// 全部账户。
pub fn list(db: &MailDb) -> Result<Vec<Account>> {
    db.read(repo::accounts::list)
}

/// 取一个账户。
pub fn get(db: &MailDb, account_id: i64) -> Result<Account> {
    db.read(|conn| repo::accounts::get(conn, account_id))
}

/// 取账户 + 口令（连接前调）。口令缺失时报 [`MailError::NeedsReauth`]。
pub fn load_with_password(
    db: &MailDb,
    secrets: &Secrets,
    account_id: i64,
) -> Result<(Account, String)> {
    let account = get(db, account_id)?;
    let password = load_password(secrets, account_id)?.ok_or(MailError::NeedsReauth)?;
    Ok((account, password))
}

/// 改同步状态（同步引擎用）。
pub fn set_sync_state(
    db: &MailDb,
    account_id: i64,
    state: crate::models::SyncState,
    error: Option<&str>,
) -> Result<()> {
    db.write_tx(|tx| repo::accounts::set_sync_state(tx, account_id, state, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SyncState;

    /// 一份临时库 + 一个内存凭据后端。
    ///
    /// 凭据**必须**用内存后端：默认后端会写用户真实的
    /// `<应用数据目录>/credentials.json`，跑测试不该动它（沙箱下也会被拒）。
    fn fixture(tag: &str) -> (MailDb, Secrets, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "duck-mail-acct-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let db = MailDb::open(dir.join("mail.db")).unwrap();
        (db, crate::secrets::memory(), dir)
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
            password: "hunter2".into(),
        }
    }

    #[test]
    fn validate_rejects_broken_input() {
        let mut a = sample("a@b.com");
        assert!(validate(&a).is_ok());

        a.email = "no-at-sign".into();
        assert!(matches!(validate(&a), Err(MailError::Config(_))));

        let mut a = sample("a@b.com");
        a.imap_host = "  ".into();
        assert!(validate(&a).is_err());

        let mut a = sample("a@b.com");
        a.imap_port = 0;
        assert!(validate(&a).is_err());
    }

    #[test]
    fn cleartext_is_only_allowed_on_local_hosts() {
        let mut a = sample("a@b.com");
        a.security = ConnectionSecurity::Plain;
        // 公网明文：拒绝。
        assert!(validate(&a).is_err());
        // 本地测试服务器：放行（stalwart 联调场景）。
        a.imap_host = "127.0.0.1".into();
        assert!(validate(&a).is_ok());
        a.imap_host = "localhost".into();
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn add_then_list_and_get() {
        let (db, secrets, dir) = fixture("add");
        let id = add(&db, &secrets, &sample("me@example.com")).unwrap();
        let all = list(&db).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].email, "me@example.com");
        assert_eq!(get(&db, id).unwrap().display_name.as_deref(), Some("工作"));
        // 口令落在凭据后端里，键名带账户 id。
        assert_eq!(
            load_password(&secrets, id).unwrap().as_deref(),
            Some("hunter2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_rejects_duplicate_email() {
        let (db, secrets, dir) = fixture("dup");
        add(&db, &secrets, &sample("same@example.com")).unwrap();
        assert!(matches!(
            add(&db, &secrets, &sample("same@example.com")),
            Err(MailError::Config(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_row_never_contains_the_password() {
        let (db, secrets, dir) = fixture("nopw");
        add(&db, &secrets, &sample("p@example.com")).unwrap();
        // 把 accounts 表的每列拼成一个大字符串，确认口令不在其中。
        let dump: String = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COALESCE(email,'')||COALESCE(display_name,'')||username||
                            imap_host||smtp_host||settings_json||COALESCE(sync_error,'')
                     FROM accounts",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(
            !dump.contains("hunter2"),
            "password leaked into accounts table"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_drops_the_credential_too() {
        let (db, secrets, dir) = fixture("rm");
        let id = add(&db, &secrets, &sample("r@example.com")).unwrap();
        remove(&db, &secrets, id).unwrap();
        // 档案没了，凭据也不能留成孤儿。
        assert!(list(&db).unwrap().is_empty());
        assert!(load_password(&secrets, id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_sync_state_surfaces_error_text() {
        let (db, secrets, dir) = fixture("state");
        let id = add(&db, &secrets, &sample("s@example.com")).unwrap();
        set_sync_state(&db, id, SyncState::Error, Some("login failed")).unwrap();
        let a = get(&db, id).unwrap();
        assert_eq!(a.sync_state, SyncState::Error);
        assert_eq!(a.sync_error.as_deref(), Some("login failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_credential_reads_as_needs_reauth() {
        let (db, secrets, dir) = fixture("reauth");
        let id = add(&db, &secrets, &sample("n@example.com")).unwrap();
        // 模拟「档案在、密文丢了」（换机器只搬了数据目录的一半）。
        delete_credentials(&secrets, id).unwrap();
        assert!(matches!(
            load_with_password(&db, &secrets, id),
            Err(MailError::NeedsReauth)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_keys_are_namespaced() {
        assert_eq!(password_key(7), "mail:7:password");
        assert_eq!(refresh_token_key(7), "mail:7:refresh_token");
        // 不同账户的键必须不同——否则会互相覆盖口令。
        assert_ne!(password_key(1), password_key(2));
    }
}
