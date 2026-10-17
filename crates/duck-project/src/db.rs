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
    // v3：周计划独立成表 + 工作日志的标签列。
    //
    // # 为什么周计划不能塞进 `work_logs`
    //
    // v1 里周计划是 `work_logs` 的一个 `kind`（`UNIQUE (date, kind)`）。但它与日报 /
    // 周报**不是同一类东西**：
    //
    // 1. **主键不同**。日志的键是「哪一天」，周计划的键是「哪一周」（`2026-W42`）——
    //    同一周里换一天写，日志是另一篇，周计划还是同一份。硬塞进日期键，界面就得
    //    自己保证「一周只写一行」，而约束在库里根本没有。
    // 2. **多两列语义**。`task_ids`（当前计划内的任务）与 `original_task_ids`
    //    （**首次定型**时的任务）之差就是「计划 vs 实际」里的「新增项」——
    //    日志表没有这两列，也不能靠日期算出来。
    // 3. **状态机**。`draft` / `confirmed` 是周计划独有的：确认过的计划是那一周的
    //    承诺，之后的改动应当看得见（新增项），而不是悄悄覆盖。
    //
    // 所以 v3 把它提出来独立建表；`work_logs.kind = 'plan'` 从此**不再使用**
    // （旧库里已存在的行仍读得出来，界面按 `work_logs` 那一档继续显示，不迁移 ——
    // 迁移要猜「这条 plan 属于哪一周」，猜错会把用户的承诺挪到别的周）。
    //
    // # `week_key` 的形态
    //
    // `YYYY-Www`（如 `2026-W42`，ISO-8601 周编号：**周四是哪一年，这周就属于哪一年**）。
    // 文本序 == 时间序，所以 `ORDER BY week_key` 直接可用 —— 与日期列同一个理由
    // （见 v1 的第 2 条约定）。生成与反解在 `crate::time`（`CalendarDate::week_key` /
    // `from_week_key`），单测钉着，界面不自己拼。
    //
    // `UNIQUE` 不是「顺手加的」：upsert 靠它做冲突目标，没有它「一周一行」就只是
    // 界面上的君子协定。
    r"
    CREATE TABLE IF NOT EXISTS weekly_plans (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        week_key          TEXT    NOT NULL UNIQUE,
        content_md        TEXT    NOT NULL DEFAULT '',
        task_ids          TEXT    NOT NULL DEFAULT '[]',
        original_task_ids TEXT    NOT NULL DEFAULT '[]',
        status            TEXT    NOT NULL DEFAULT 'draft',
        created_at        INTEGER NOT NULL,
        updated_at        INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_weekly_plans_week_key ON weekly_plans(week_key);
    CREATE INDEX IF NOT EXISTS idx_weekly_plans_updated
        ON weekly_plans(updated_at DESC);

    ALTER TABLE work_logs ADD COLUMN tags TEXT NOT NULL DEFAULT '';
    ",
    // v4：任务的**工作上下文**（关联的 Git 分支 + 「干到哪儿了」）与标签收藏。
    //
    // # 为什么不是一列 JSON
    //
    // 内核**零 serde**。把上下文塞进一列就得手写一整套 JSON 转义（引号 / 反斜杠 /
    // 控制字符），而这三样各自都有更简单的自然编码：
    //
    // 1. `context_files`：一行一个 `行:列:路径`。路径取**前两个冒号之后**的全部内容，
    //    所以 Windows 的 `C:\…` 不会被切开，路径里带冒号也安全。
    // 2. `context_sql`：**单独一列、原样存**。它是三样里唯一可能带换行的，单独成列
    //    就根本不存在「需要转义的字符」；若与别的一起按行编，一段多行 SQL 会散成好几行。
    // 3. `context_tabs`：一行一个链接。
    //
    // 编解码在 `store::{encode_files, decode_files, encode_lines, decode_lines}`，与
    // `join_tags` / `split_tags` 放在同一处、同一套「宁可丢也不猜」的口径（含换行的项
    // 直接不要 —— 宁可少一条，也不要在库里留下一个回读不出来的字节）。
    //
    // `tags.is_favorite` 是「左栏标签段按收藏排序」的落点：排序口径放内核（`ORDER BY
    // is_favorite DESC, name`），界面不自己排 —— 否则「悬浮窗 / 主窗各排一遍」迟早
    // 会排出两个顺序。
    r"
    ALTER TABLE tasks ADD COLUMN git_branch TEXT NOT NULL DEFAULT '';
    ALTER TABLE tasks ADD COLUMN context_files TEXT NOT NULL DEFAULT '';
    ALTER TABLE tasks ADD COLUMN context_sql TEXT NOT NULL DEFAULT '';
    ALTER TABLE tasks ADD COLUMN context_tabs TEXT NOT NULL DEFAULT '';
    ALTER TABLE tags ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0;
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
    const TABLES: [&str; 9] = [
        "project_groups",
        "projects",
        "tasks",
        "tags",
        "task_tags",
        "milestones",
        "board_columns",
        "work_logs",
        "weekly_plans",
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

    /// 「一周一行」必须是库级约束（upsert 的冲突目标），不能只是界面上的君子协定。
    #[test]
    fn weekly_plans_week_key_is_unique() -> Result<()> {
        let connection = open_in_memory()?;
        let insert = "INSERT INTO weekly_plans(week_key, created_at, updated_at) VALUES(?1, 1, 1)";
        assert!(connection.execute(insert, ["2026-W42"]).is_ok());
        let again = connection.execute(insert, ["2026-W42"]);
        assert!(again.is_err(), "同一个 week_key 不该能插第二行");
        assert!(connection.execute(insert, ["2026-W43"]).is_ok());
        Ok(())
    }

    /// v3 的 schema 细节：两列的默认值必须是 upsert 认得的形态（`'[]'` / `'draft'`）。
    ///
    /// 直接断言默认值而不是「能不能插进去」：默认值漂了（比如改成 NULL）会让
    /// 读取端解析失败，但插入照样成功 —— 那种洞只有读默认值才看得出来。
    #[test]
    fn weekly_plans_defaults_are_wire_ready() -> Result<()> {
        let connection = open_in_memory()?;
        connection.execute(
            "INSERT INTO weekly_plans(week_key, created_at, updated_at) VALUES('2026-W42', 1, 1)",
            [],
        )?;
        let (tasks, original, status, content): (String, String, String, String) = connection
            .query_row(
                "SELECT task_ids, original_task_ids, status, content_md FROM weekly_plans",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(tasks, "[]");
        assert_eq!(original, "[]");
        assert_eq!(status, "draft");
        assert_eq!(content, "");
        Ok(())
    }

    /// v3 给 `work_logs` 加的 `tags` 列：默认空且不许为 NULL（读端不必处理 `None`）。
    #[test]
    fn work_logs_tags_column_is_not_null() -> Result<()> {
        let connection = open_in_memory()?;
        connection.execute(
            "INSERT INTO work_logs(date, kind) VALUES('2026-10-14', 'daily')",
            [],
        )?;
        let tags: String =
            connection.query_row("SELECT tags FROM work_logs", [], |row| row.get(0))?;
        assert!(tags.is_empty(), "默认应当是空串（= 没有标签）");
        let null_attempt = connection.execute(
            "INSERT INTO work_logs(date, kind, tags) VALUES('2026-10-15', 'daily', NULL)",
            [],
        );
        assert!(null_attempt.is_err(), "tags 有 NOT NULL，写 NULL 应当被拒");
        Ok(())
    }
}
