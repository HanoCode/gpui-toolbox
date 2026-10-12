//! 本地邮件库：连接管理 + 迁移 + repo。
//!
//! # 为什么自带一个 SQLite 库
//!
//! 与 `duck-store` 的 kv、`duck-ai` 的会话库、`duck-db` 的用户库**分库**
//! （`docs/email-design.md` §4.5）：邮箱是几十张表的固定 schema，还有 FTS5 虚表，
//! 生命周期（常驻同步 + 定时任务）与备份策略都不同。**只共享 rusqlite 版本，
//! 不共享连接对象**——`duck-db` 的抽象是「用户任意 SQL + 可编辑网格 + 取消」，
//! 与「固定 schema + 迁移 + 事务写」没有共同点，硬共享只会让两边互相迁就。
//!
//! # 并发模型
//!
//! WAL 模式下 SQLite 的标准用法是「多读一写」：
//!
//! - **写连接**（`Mutex<Connection>`）：所有写串行化，等价于「单写者」。
//!   写路径一律**单事务**（`write_tx`），与 `duck-db` 纪律一致。
//! - **读连接**（`Mutex<Connection>`）：与写连接分离，WAL 允许它读到已提交的一致快照，
//!   不被写者阻塞。
//!
//! 本 crate 的 API 全是**同步**的；调用方（`duck-app` 页面）把调用丢进 gpui 的
//! `background_executor`，与 `duck-db` / `duck-redis` 同一口径。**本层不建 runtime、
//! 不起线程**——把长事务留在 UI 线程才是真问题，而不是「有没有写线程」。

pub mod migrations;
pub mod repo;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::error::{MailError, Result};

/// 一个打开着的邮件库。
///
/// 内部两个连接都用 `Mutex` 包着——`MailDb` 自身可 `Clone`（共享同一对连接），
/// 跨线程安全。
#[derive(Clone)]
pub struct MailDb {
    read: Arc<Mutex<Connection>>,
    write: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl MailDb {
    /// 打开（或创建）指定路径的库，并把 schema 推进到最新。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MailError::Io(format!("cannot create {}: {e}", parent.display())))?;
        }

        let mut write = Self::open_connection(&path)?;
        migrations::run(&mut write)?;
        let read = Self::open_connection(&path)?;

        tracing::info!(path = %path.display(), "mail database opened");
        Ok(Self {
            read: Arc::new(Mutex::new(read)),
            write: Arc::new(Mutex::new(write)),
            path,
        })
    }

    /// 内存库（测试用）。schema 与磁盘库一致。
    pub fn open_in_memory() -> Result<Self> {
        let mut write = Connection::open_in_memory()?;
        write.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&mut write)?;
        let read = Connection::open_in_memory()?;
        read.pragma_update(None, "foreign_keys", "ON")?;
        // 内存库的读写连接是**两个独立库**，这里只用于「不改数据的读路径」测试；
        // 真要用内存库跑读写闭环请在单连接上做（见测试）。
        Ok(Self {
            read: Arc::new(Mutex::new(read)),
            write: Arc::new(Mutex::new(write)),
            path: PathBuf::from(":memory:"),
        })
    }

    fn open_connection(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        // WAL：读不阻塞写、写不阻塞读。`journal_mode` 会**返回一行**，
        // 所以走 `query_row` 而不是 `pragma_update`（后者会报 "returned results"）。
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            // 内存库 / 某些网络文件系统上 WAL 不可用——降级但记一笔，别静默。
            tracing::warn!(mode, "mail database is not in WAL mode");
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL 下 NORMAL 是安全的：只在 checkpoint 时不 fsync，掉电最多丢最后一批，
        // 不损坏库。邮件侧重「不丢已确认的写」，而关键写我们本来就走单事务提交。
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 多连接（读写分离 + 后台同步）撞锁时等一会儿，别立刻 SQLITE_BUSY。
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    /// 库文件路径（内存库为 `:memory:`）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 只读访问。给查询用——不走写锁，不被写者阻塞。
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.lock(&self.read, "read")?;
        f(&guard)
    }

    /// 写访问（自动提交，每条语句一个事务）。
    ///
    /// ⚠️ 多条语句务必用 [`MailDb::write_tx`]，否则「写一半失败」会留下半成品。
    pub fn write<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.lock(&self.write, "write")?;
        f(&mut guard)
    }

    /// 单事务写。**所有多语句写路径必须走这里**（`duck-db` 纪律）。
    pub fn write_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.lock(&self.write, "write")?;
        let tx = guard.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn lock<'a>(
        &self,
        mutex: &'a Mutex<Connection>,
        what: &str,
    ) -> Result<MutexGuard<'a, Connection>> {
        mutex.lock().map_err(|_| {
            // 锁中毒说明上一次持锁的线程 panic 了。SQLite 连接本身可能仍可用，
            // 但状态不可知，报错比装作没事安全。
            MailError::Db(format!("mail database {what} lock poisoned"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// 磁盘库跑完整的「写 → 读」闭环（内存库的读写连接是两套库，不适合闭环测试）。
    #[test]
    fn write_then_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("duck-mail-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = MailDb::open(dir.join("mail.db")).unwrap();

        db.write_tx(|tx| {
            tx.execute(
                "INSERT INTO accounts
                   (email, provider, auth_kind, username, imap_host, imap_port,
                    smtp_host, smtp_port, created_at)
                 VALUES (?1, 'imap', 'password', ?1, 'imap.example.com', 993,
                         'smtp.example.com', 465, 0)",
                params!["me@example.com"],
            )?;
            Ok(())
        })
        .unwrap();

        let email: String = db
            .read(|conn| Ok(conn.query_row("SELECT email FROM accounts", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(email, "me@example.com");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_persists_and_migrations_stay_idempotent() {
        let dir = std::env::temp_dir().join(format!("duck-mail-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mail.db");
        {
            let db = MailDb::open(&path).unwrap();
            db.write_tx(|tx| {
                tx.execute(
                    "INSERT INTO app_settings (key, value) VALUES ('k', 'v')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        }
        // 重开：迁移不该重跑，数据还在。
        let db = MailDb::open(&path).unwrap();
        let v: String = db
            .read(|conn| Ok(conn.query_row("SELECT value FROM app_settings", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(v, "v");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let dir = std::env::temp_dir().join(format!("duck-mail-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = MailDb::open(dir.join("mail.db")).unwrap();

        let failed = db.write_tx(|tx| {
            tx.execute(
                "INSERT INTO app_settings (key, value) VALUES ('a', '1')",
                [],
            )?;
            // 主键冲突：整个事务必须回滚，'a' 也不该留下。
            tx.execute(
                "INSERT INTO app_settings (key, value) VALUES ('a', '2')",
                [],
            )?;
            Ok(())
        });
        assert!(failed.is_err());

        let n: i64 = db
            .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM app_settings", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0, "failed transaction must leave no rows behind");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
