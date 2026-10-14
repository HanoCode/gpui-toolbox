//! 连接、PRAGMA 与版本化迁移。
//!
//! 库文件是**自管**的 `{app_data_dir}/projects.db`（同 `duck-ai` 的 `ai.db`）：
//! 关系数据与 kv 不混在一个文件里，「重建索引 / 清空某模块」这类动作才不会互相牵连。
//! 迁移用 SQLite 内建的 `user_version` 做版本号，**脚本只追加、不改已发布的条目**。

use std::path::Path;

use rusqlite::Connection;

use crate::error::{ProjectError, Result};

/// 迁移脚本：下标 + 1 即目标 `user_version`。**只追加，不改已发布的条目。**
const MIGRATIONS: &[&str] = &[
    // v1：全量初始 schema（分组 / 项目 / 任务 / 标签 / 里程碑 / 看板列 / 工作日志）。
    //
    // 三条贯穿全表的约定：
    //
    // 1. **归档是与状态正交的一列**（`archived_at`）。常规视图一律附加
    //    `archived_at IS NULL`，所以「未归档」是最热门的过滤前提 —— 下面几个部分索引
    //    都带这个条件，扫描行数直接少一个量级（SQLite 3.8 起支持部分索引）。
    // 2. **日期列是 `YYYY-MM-DD` 文本**，不是 Unix 秒：日期是日历事实，存成时刻会在
    //    别的时区读成前一天（见 `crate::time` 的模块注释）。文本序 == 时间序，
    //    `ORDER BY due_date` 与 `due_date < ?` 都直接可用。
    // 3. **时刻列是 INTEGER Unix 秒**，与 `duck-store` / `duck-ai` / `duck-mail` 一致。
    //
    // `archived_root`：归档**级联归属**。`NULL` = 未归档；`'task:<id>'` / `'project:<id>'`
    // = 这条是被那个根对象带着归档的（根对象自己也记自己）。
    //
    // 它决定两件事：恢复根对象时恢复哪些行（`archived_root = 'project:7'` 全恢复）、
    // 归档中心的**任务段**只列根（`archived_root = 'task:' || t.id`，子任务不单独出现）。
    //
    // 设计文档 §2.3 原本写的是「按同一个 `archived_at` 时间戳判别」，实现时换掉了：
    // 时间戳会撞（同一秒里先归档项目 A 再单独归档任务 X，恢复 A 时会把 X 一起复活），
    // 而且「任务 id 12」与「项目 id 12」分不出来。显式的 `kind:id` 没有这两个洞。
    //
    // `projects.status` **不含 `archived`**：归档只由 `archived_at` 表达（见 §2.3 的
    // 「归档 ≠ 状态」），两处都写迟早会不一致。
    //
    // 看板列**不预置任何行**：`name` 为 NULL 表示「用内置默认文案」，默认四列由界面按
    // 当前语言取词渲染。内核里不出现界面文案（i18n 守卫会拦下中文字面量），
    // 也因此不会有「英文用户看到四个中文列名」的错。
    r"
    CREATE TABLE IF NOT EXISTS project_groups (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        name        TEXT    NOT NULL,
        icon        TEXT,
        sort_order  INTEGER NOT NULL DEFAULT 0,
        collapsed   INTEGER NOT NULL DEFAULT 0,
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS projects (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        group_id     INTEGER REFERENCES project_groups(id) ON DELETE SET NULL,
        name         TEXT    NOT NULL,
        description  TEXT    NOT NULL DEFAULT '',
        color        TEXT    NOT NULL DEFAULT '#3B82F6',
        icon         TEXT,
        status       TEXT    NOT NULL DEFAULT 'active',
        start_date   TEXT,
        due_date     TEXT,
        sort_order   INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        archived_at  INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_projects_group ON projects(group_id, sort_order);
    CREATE INDEX IF NOT EXISTS idx_projects_live
        ON projects(sort_order, id) WHERE archived_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_projects_archived
        ON projects(archived_at DESC) WHERE archived_at IS NOT NULL;

    CREATE TABLE IF NOT EXISTS tasks (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        project_id     INTEGER REFERENCES projects(id) ON DELETE CASCADE,
        parent_id      INTEGER REFERENCES tasks(id)    ON DELETE CASCADE,
        title          TEXT    NOT NULL,
        description_md TEXT    NOT NULL DEFAULT '',
        status         TEXT    NOT NULL DEFAULT 'todo',
        priority       INTEGER NOT NULL DEFAULT 2,
        quadrant       TEXT    NOT NULL DEFAULT 'Q2',
        start_date     TEXT,
        due_date       TEXT,
        due_time       TEXT,
        completed_at   INTEGER,
        progress       INTEGER NOT NULL DEFAULT 0,
        estimate_hours REAL,
        actual_hours   REAL,
        sort_order     INTEGER NOT NULL DEFAULT 0,
        created_at     INTEGER NOT NULL,
        updated_at     INTEGER NOT NULL,
        archived_at    INTEGER,
        archived_root  TEXT,
        CHECK (priority BETWEEN 0 AND 3),
        CHECK (progress BETWEEN 0 AND 100)
    );
    CREATE INDEX IF NOT EXISTS idx_tasks_board
        ON tasks(project_id, status, sort_order)
        WHERE archived_at IS NULL AND parent_id IS NULL;
    CREATE INDEX IF NOT EXISTS idx_tasks_due
        ON tasks(due_date)
        WHERE archived_at IS NULL AND status NOT IN ('done', 'cancelled');
    CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
    CREATE INDEX IF NOT EXISTS idx_tasks_archived
        ON tasks(archived_at DESC, archived_root) WHERE archived_at IS NOT NULL;

    CREATE TABLE IF NOT EXISTS tags (
        id    INTEGER PRIMARY KEY AUTOINCREMENT,
        name  TEXT    NOT NULL UNIQUE,
        color TEXT    NOT NULL
    );

    CREATE TABLE IF NOT EXISTS task_tags (
        task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        tag_id  INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
        PRIMARY KEY (task_id, tag_id)
    );
    CREATE INDEX IF NOT EXISTS idx_task_tags_tag ON task_tags(tag_id);

    CREATE TABLE IF NOT EXISTS milestones (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id           INTEGER REFERENCES tasks(id)    ON DELETE CASCADE,
        project_id        INTEGER REFERENCES projects(id) ON DELETE CASCADE,
        title             TEXT    NOT NULL,
        description       TEXT    NOT NULL DEFAULT '',
        due_date          TEXT,
        progress_snapshot INTEGER,
        reached_at        INTEGER,
        created_at        INTEGER NOT NULL,
        CHECK (task_id IS NOT NULL OR project_id IS NOT NULL)
    );
    CREATE INDEX IF NOT EXISTS idx_milestones_task ON milestones(task_id, created_at);
    CREATE INDEX IF NOT EXISTS idx_milestones_project ON milestones(project_id, created_at);

    CREATE TABLE IF NOT EXISTS board_columns (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        name       TEXT,
        status_key TEXT    NOT NULL,
        sort_order INTEGER NOT NULL DEFAULT 0,
        wip_limit  INTEGER
    );

    CREATE TABLE IF NOT EXISTS work_logs (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        date            TEXT    NOT NULL,
        kind            TEXT    NOT NULL,
        content_md      TEXT    NOT NULL DEFAULT '',
        ai_generated    INTEGER NOT NULL DEFAULT 0,
        linked_task_ids TEXT    NOT NULL DEFAULT '[]',
        UNIQUE (date, kind)
    );
    ",
    // v2：对齐 `docs/projects-mock.html` 原型的三个任务字段。
    //
    // - `starred`：置顶。查询层把它排进 `ORDER BY` 首位（见 `crate::query`），
    //   界面在卡片与详情面板给星标入口。
    // - `block_reason`：阻塞原因。卡片上的「阻塞」标签 tooltip 与详情面板的
    //   警示块都读它；`''` = 没填原因（阻塞态本身由 `status = 'blocked'` 表达）。
    // - `note`：备注（「只给自己看的提示」），与 `description_md`（面向交付的描述）
    //   分开 —— 原型里是两个输入框，语义不同。
    //
    // `ALTER TABLE ADD COLUMN` 的默认值都是常量，直接加列即可；旧行自动落到
    // 默认值，无需回填。
    r"
    ALTER TABLE tasks ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE tasks ADD COLUMN block_reason TEXT NOT NULL DEFAULT '';
    ALTER TABLE tasks ADD COLUMN note TEXT NOT NULL DEFAULT '';
    ",
];

/// 打开数据库、应用 PRAGMA 并把 schema 迁到最新版本。
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        duck_platform::ensure_dir(parent)?;
    }
    let connection = Connection::open(path)?;
    configure(&connection)?;
    migrate(&connection)?;
    Ok(connection)
}

/// 内存库（单测用）。PRAGMA 与迁移与落盘库完全一致。
pub fn open_in_memory() -> Result<Connection> {
    let connection = Connection::open_in_memory()?;
    configure(&connection)?;
    migrate(&connection)?;
    Ok(connection)
}

/// 建连接时的 PRAGMA。
///
/// - `WAL`：读写并发（页面在后台线程读、UI 线程写）。内存库不支持 WAL，
///   失败时忽略 —— 单测不关心日志模式。
/// - `synchronous = NORMAL`：桌面应用里安全且明显更快（WAL 下只在 checkpoint 时 fsync）。
/// - `foreign_keys = ON`：**必须显式开**，SQLite 默认是关的，关了那堆
///   `ON DELETE CASCADE` 全是装饰。
fn configure(connection: &Connection) -> Result<()> {
    let _ = connection.pragma_update(None, "journal_mode", "WAL");
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// 把 `user_version` 推到 `MIGRATIONS.len()`；每个未应用的脚本在**一个事务**里执行。
fn migrate(connection: &Connection) -> Result<()> {
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
                tracing::info!(version = applied, "duck-project migration applied");
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(ProjectError::Sqlite(error));
            }
        }
    }
    Ok(())
}

/// 当前 schema 版本（= 已应用的迁移条数）。
pub fn schema_version(connection: &Connection) -> Result<i64> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::{MIGRATIONS, open_in_memory, schema_version};
    use crate::error::Result;

    /// v1 之后建的表清单。新增迁移改了这里，就会逼着作者同时更新断言。
    const TABLES: [&str; 8] = [
        "project_groups",
        "projects",
        "tasks",
        "tags",
        "task_tags",
        "milestones",
        "board_columns",
        "work_logs",
    ];

    #[test]
    fn migrates_to_latest_and_is_idempotent() -> Result<()> {
        let connection = open_in_memory()?;
        assert_eq!(schema_version(&connection)?, MIGRATIONS.len() as i64);
        // 再跑一次迁移应为空操作。
        super::migrate(&connection)?;
        assert_eq!(schema_version(&connection)?, MIGRATIONS.len() as i64);
        Ok(())
    }

    #[test]
    fn all_tables_exist() -> Result<()> {
        let connection = open_in_memory()?;
        for table in TABLES {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )?;
            assert_eq!(count, 1, "缺表 {table}");
        }
        Ok(())
    }

    /// 外键必须真的生效（`ON DELETE CASCADE` 全是装饰的话，删项目会留下孤儿任务）。
    #[test]
    fn foreign_keys_are_enforced() -> Result<()> {
        let connection = open_in_memory()?;
        let enabled: i64 = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        assert_eq!(enabled, 1, "foreign_keys 必须为 ON");
        Ok(())
    }

    /// 归档相关索引必须是**部分索引**：它们服务的是「未归档」这个最热过滤前提。
    #[test]
    fn archive_indexes_are_partial() -> Result<()> {
        let connection = open_in_memory()?;
        let sql: String = connection.query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_tasks_archived'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            sql.contains("archived_at IS NOT NULL"),
            "idx_tasks_archived 应当是部分索引，实际：{sql}"
        );
        Ok(())
    }

    /// 带 CHECK 的列必须真的拒绝野值 —— 优先级 / 进度越界不能落库。
    #[test]
    fn check_constraints_reject_out_of_range() -> Result<()> {
        let connection = open_in_memory()?;
        let now = 1_700_000_000_i64;
        let bad_priority = connection.execute(
            "INSERT INTO tasks(title, priority, created_at, updated_at) VALUES('t', 9, ?1, ?1)",
            [now],
        );
        assert!(bad_priority.is_err(), "priority 越界应当被 CHECK 拦下");
        let bad_progress = connection.execute(
            "INSERT INTO tasks(title, progress, created_at, updated_at) VALUES('t', 101, ?1, ?1)",
            [now],
        );
        assert!(bad_progress.is_err(), "progress 越界应当被 CHECK 拦下");
        Ok(())
    }
}
