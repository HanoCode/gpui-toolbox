//! 数据库连接、PRAGMA 与版本化迁移。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// 存储层错误。
#[derive(Debug)]
pub enum StoreError {
    /// 打开 / 创建数据库文件失败。
    Io(std::io::Error),
    /// SQL 执行失败。
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "store I/O failed: {error}"),
            Self::Sqlite(error) => write!(f, "sqlite failed: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// 迁移脚本：下标 + 1 即目标 `user_version`。**只追加，不改已发布的条目。**
const MIGRATIONS: &[&str] = &[
    // v1：全量初始 schema。
    r"
    CREATE TABLE IF NOT EXISTS kv (
        key        TEXT PRIMARY KEY,
        value      TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS settings (
        scope      TEXT NOT NULL,
        key        TEXT NOT NULL,
        value      TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (scope, key)
    );
    CREATE TABLE IF NOT EXISTS windows (
        key        TEXT PRIMARY KEY,
        bounds     TEXT NOT NULL,
        monitor_id TEXT,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS recent (
        kind TEXT NOT NULL,
        id   TEXT NOT NULL,
        at   INTEGER NOT NULL,
        PRIMARY KEY (kind, id)
    );
    CREATE TABLE IF NOT EXISTS tool_state (
        tool_id    TEXT NOT NULL,
        key        TEXT NOT NULL,
        value      TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (tool_id, key)
    );
    ",
    // v2：删掉 v1 建下、却从来没有读写方的 `settings` 表。
    //
    // 它的形状（`scope` + `key`）当初是按「按作用域分组的设置」设计的，但实际实现里
    // 应用级设置走 `kv`、工具级设置走 `tool_state`，这张表一次都没被写过 —— 留着只会
    // 让后来的人以为「设置也许存在这里」而白找一圈。删表即删掉这个错误线索；
    // 因为从未写入，这里没有任何数据会丢。
    r"
    DROP TABLE IF EXISTS settings;
    ",
    // v3：审计日志（append-only）。
    //
    // 与 `recent` 的分工：`recent` 是「最近用过什么」（主键去重、只留最后一次），
    // 审计是**发生过什么**的流水（一次一行，永不合并）。两者的键、用途、保留策略都不同，
    // 所以另起一张表而不是复用。
    //
    // `outcome` / `kind` / `action` 都是**稳定英文键**（`ok` / `failed` / `blocked` /
    // `cancelled`、`connection` / `query` / `write` / `transport` / `hook` / `plugin` / `mcp`…）：
    // 界面文案按当次语言另译，日志与筛选按这些键走，换语言不会让历史记录失配。
    //
    // `fingerprint` 是语句的**归一化指纹**（字面量换成占位符后哈希），用来把
    // 「同一类语句跑了多少次」聚合起来；`trace_id` 只在出错时非空，是错误与其它
    // 记录的关联键（状态栏徽标点进去就按它过滤）。
    //
    // ⚠️ **append-only**：只增不改。下面那个 `BEFORE UPDATE` 触发器把这一点钉死
    // （误改会直接报错，而不是静默改写历史）。允许的删除只有两种，都是显式动作：
    // 超出保留上限的**裁剪**，以及用户点的「清空审计」。
    r"
    CREATE TABLE IF NOT EXISTS audit (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        at          INTEGER NOT NULL,
        kind        TEXT NOT NULL,
        action      TEXT NOT NULL,
        outcome     TEXT NOT NULL,
        scope       TEXT NOT NULL,
        detail      TEXT NOT NULL,
        fingerprint TEXT NOT NULL DEFAULT '',
        trace_id    TEXT NOT NULL DEFAULT '',
        duration_ms INTEGER
    );
    CREATE INDEX IF NOT EXISTS audit_at ON audit(at);
    CREATE INDEX IF NOT EXISTS audit_trace ON audit(trace_id);
    CREATE TRIGGER IF NOT EXISTS audit_no_update
    BEFORE UPDATE ON audit
    BEGIN
        SELECT RAISE(ABORT, 'audit rows are append-only');
    END;
    ",
];

/// 打开数据库、应用 PRAGMA 并把 schema 迁到最新版本。
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent() {
        duck_platform::ensure_dir(parent)?;
    }
    let connection = Connection::open(path)?;
    configure(&connection)?;
    migrate(&connection)?;
    Ok(connection)
}

/// 建连接时的 PRAGMA：WAL 提升并发读写，`NORMAL` 同步在桌面应用里是安全的折中。
fn configure(connection: &Connection) -> Result<(), StoreError> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// 把 `user_version` 推到 `MIGRATIONS.len()`；每个未应用的脚本在一个事务里执行。
fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let target = MIGRATIONS.len() as i64;
    for version in current..target {
        let script = MIGRATIONS[version as usize];
        connection.execute_batch("BEGIN")?;
        match connection.execute_batch(script) {
            Ok(()) => {
                let applied = version + 1;
                connection.pragma_update(None, "user_version", applied)?;
                connection.execute_batch("COMMIT")?;
                tracing::info!(version = applied, "migration applied");
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error.into());
            }
        }
    }
    Ok(())
}

/// 当前 Unix 秒（所有 `updated_at` / `at` 列统一用它）。
#[must_use]
pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{StoreError, migrate, open};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "duck-store-test-{name}-{}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn migrates_to_latest_and_is_idempotent() -> Result<(), StoreError> {
        let path = temp_path("migrate");
        let connection = open(&path)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, super::MIGRATIONS.len() as i64);
        // 再跑一次迁移应为空操作。
        migrate(&connection)?;
        let again: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(again, version);
        drop(connection);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn all_tables_exist() -> Result<(), StoreError> {
        let path = temp_path("tables");
        let connection = open(&path)?;
        for table in ["kv", "windows", "recent", "tool_state", "audit"] {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1, "缺表 {table}");
        }
        drop(connection);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// 审计表**只增不改**：误改一行必须当场报错，而不是静默改写历史。
    #[test]
    fn audit_rows_cannot_be_updated() -> Result<(), StoreError> {
        let path = temp_path("audit-append-only");
        let connection = open(&path)?;
        connection.execute(
            "INSERT INTO audit(at, kind, action, outcome, scope, detail)
             VALUES(1, 'query', 'execute', 'ok', 'conn-1', 'SELECT 1')",
            [],
        )?;
        let updated = connection.execute("UPDATE audit SET detail = '改了'", []);
        assert!(
            updated.is_err(),
            "审计行被改成功了，append-only 触发器没生效"
        );
        // 删除仍然可以（裁剪与「清空审计」都要用它）——这是**有意**留的口子。
        connection.execute("DELETE FROM audit", [])?;
        drop(connection);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// v1 里那张没有读写方的 `settings` 表，必须被 v2 删干净。
    ///
    /// 这条断言是反向的（要的是「不存在」）：将来谁想再建一张存放设置的表，会先在这里
    /// 撞上一次，从而看到 v2 的说明 —— 而不是默默把它加回来。
    #[test]
    fn the_dead_settings_table_is_dropped() -> Result<(), StoreError> {
        let path = temp_path("drop-settings");
        let connection = open(&path)?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "settings 表应当已被 v2 删除");
        drop(connection);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// 从 v1 的库升级上来时，`settings` 也必须消失（而不是只有新建的库才对）。
    #[test]
    fn upgrading_a_v1_database_drops_the_settings_table() -> Result<(), StoreError> {
        let path = temp_path("upgrade-v1");
        let connection = open(&path)?;
        // 退回 v1 并重建那张表，模拟一个「上次启动还是 v1」的库。
        connection.pragma_update(None, "user_version", 1_i64)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                 scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                 updated_at INTEGER NOT NULL, PRIMARY KEY (scope, key)
             );",
        )?;
        migrate(&connection)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(version, super::MIGRATIONS.len() as i64);
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "从 v1 升级也应删掉 settings 表");
        drop(connection);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}
