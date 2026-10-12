//! 迁移驱动。
//!
//! 用 `PRAGMA user_version` 记录已应用到的版本，逐个 `execute_batch` 在**单事务**里
//! 推进（照 `flectar-mail-core/src/db/migrations.rs` 的范式）。SQL 是纯 SQL，与
//! rusqlite 版本无关，故可从参考项目原样复用。
//!
//! 版本号 = 已应用迁移的条数。新增迁移只能**追加**到 [`MIGRATIONS`] 末尾——中间插
//! 一条会让老库的 `user_version` 与数组下标错位，已发布的迁移一个字节都不能改。

use rusqlite::Connection;

use crate::error::{MailError, Result};

/// 全部迁移，按应用顺序。
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_init.sql"),
    include_str!("migrations/002_attachment_files.sql"),
    include_str!("migrations/003_attachment_content.sql"),
    include_str!("migrations/004_carddav.sql"),
    include_str!("migrations/005_mailbox_count_indexes.sql"),
    include_str!("migrations/006_contact_recovery.sql"),
    include_str!("migrations/007_account_label_ownership.sql"),
    include_str!("migrations/008_sender_identities.sql"),
    include_str!("migrations/009_contact_learning_clean_start.sql"),
    include_str!("migrations/010_folder_hierarchy.sql"),
    include_str!("migrations/011_pending_body_page.sql"),
    include_str!("migrations/012_sender_identity_sources.sql"),
];

/// 当前 schema 版本。
pub const LATEST_VERSION: i64 = MIGRATIONS.len() as i64;

/// 把库推进到 [`LATEST_VERSION`]（幂等）。
///
/// 库比程序新时**拒绝打开**并保留数据不动——降级运行会写坏新 schema。
pub fn run(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > LATEST_VERSION {
        return Err(MailError::Db(format!(
            "mail database schema v{version} was written by a newer build \
             (this build supports up to v{LATEST_VERSION}); refusing to open"
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let target = (index + 1) as i64;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // `pragma_update` 在事务里改 `user_version` —— 与建表同生共死，
        // 事务回滚时版本号一起回滚，不会出现「表建了一半但版本已推进」。
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
        tracing::info!(version = target, "applied mail db migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn migrations_apply_cleanly_to_latest() {
        let conn = fresh();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, LATEST_VERSION);
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut conn = fresh();
        // 再跑一次不该报错，也不该重复建表。
        run(&mut conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, LATEST_VERSION);
    }

    #[test]
    fn core_tables_exist() {
        let conn = fresh();
        for table in [
            "accounts",
            "folders",
            "threads",
            "messages",
            "message_bodies",
            "attachments",
            "labels",
            "message_labels",
            "pending_actions",
            "sync_failures",
            "app_settings",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {table} should exist after migrations");
        }
    }

    #[test]
    fn fts5_virtual_table_is_created() {
        let conn = fresh();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // bundled SQLite 的 FTS5 无条件开启（设计文档 §4.3）。
        assert_eq!(n, 1, "messages_fts virtual table should exist");
    }

    #[test]
    fn newer_schema_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", LATEST_VERSION + 5)
            .unwrap();
        let err = run(&mut conn).unwrap_err();
        assert!(matches!(err, MailError::Db(_)));
        assert!(err.to_string().contains("newer build"));
    }
}
