//! 会话持久化：`duck-ai` 自管的 SQLite 文件 `{app_data_dir}/ai.db`。
//!
//! **为什么不用 `duck-store` 的 kv**：kv 是「单键 JSON + 400ms 去抖整值重写」
//! （`duck-store/src/writer.rs`）。会话正文含长文本与工具输出，每次追加都全量重写
//! 整个数组是 O(n²) 的浪费，且 JSON 转义开销不可忽略。
//!
//! **为什么不混进 `duck-store` 的表**：知识库索引是**可重建的派生数据**，会话是
//! **不可重建的用户数据**。混在一个库里会让「重建索引」这个动作必须小心翼翼
//! （怕误删会话）。独立文件下，重建索引只影响 `kb_*` 表。
//!
//! 四张表（本增量只落前两张，`kb_*` 属 M2）：
//!
//! | 表 | 用途 |
//! | --- | --- |
//! | `conversations` | 会话元数据（列表与搜索用，**不含正文**） |
//! | `messages` | 消息正文，一条一行，流式结束后落库 |
//! | `kb_chunks` | 知识库 FTS5 虚拟表（M2） |
//! | `kb_meta` | 索引版本与来源清单（M2） |

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::AiError;
use crate::wire::{ChatMessage, Usage};

/// 自动标题的字符上限（按 `char` 计数，不是字节——中文按字算才符合直觉）。
const TITLE_MAX_CHARS: usize = 24;

/// 一个会话的元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// 最后一次使用的模型（仅展示）。
    pub model: Option<String>,
    pub archived: bool,
    /// 本会话累计输入 token（各轮相加，落库时累加）。
    pub input_tokens: u64,
    /// 本会话累计输出 token。
    pub output_tokens: u64,
}

impl Conversation {
    /// 累计总 token。
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// 一条落库的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub role: String,
    pub content_json: String,
    pub created_at: i64,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

impl StoredMessage {
    /// 反序列化正文；数据损坏时返回 `None`（调用方跳过这一条而不是整页崩掉）。
    #[must_use]
    pub fn message(&self) -> Option<ChatMessage> {
        serde_json::from_str(&self.content_json).ok()
    }
}

/// 自管数据库句柄。**不是** `Send`（`rusqlite::Connection` 不跨线程共享）；
/// 上层需要跨线程时按「一线程一连接」或「外层加锁」处理。
pub struct AiStore {
    conn: Connection,
}

impl AiStore {
    /// 打开（或创建）指定路径的库。
    pub fn open(path: &Path) -> Result<Self, AiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AiError::Storage(format!("create data dir failed: {error}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|error| AiError::Storage(format!("open ai.db failed: {error}")))?;
        Self::from_connection(conn)
    }

    /// 打开应用数据目录下的默认库 `{app_data_dir}/ai.db`。
    pub fn open_default() -> Result<Self, AiError> {
        Self::open(&default_path())
    }

    /// 内存库，仅测试用。
    pub fn open_in_memory() -> Result<Self, AiError> {
        let conn = Connection::open_in_memory()
            .map_err(|error| AiError::Storage(format!("open memory db failed: {error}")))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, AiError> {
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// 全部未归档会话，最近更新的在前。
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, AiError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, title, created_at, updated_at, model, archived,
                        input_tokens, output_tokens
                 FROM conversations WHERE archived = 0
                 ORDER BY updated_at DESC",
            )
            .map_err(map_sql)?;
        let rows = statement
            .query_map([], |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                    model: row.get(4)?,
                    archived: row.get::<_, i64>(5)? != 0,
                    input_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or_default().max(0) as u64,
                    output_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or_default().max(0) as u64,
                })
            })
            .map_err(map_sql)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql)
    }

    /// 新建一个会话。
    pub fn create_conversation(
        &self,
        title: &str,
        model: Option<&str>,
    ) -> Result<Conversation, AiError> {
        let now = now_seconds();
        let conversation = Conversation {
            id: new_id("conv"),
            title: title.to_owned(),
            created_at: now,
            updated_at: now,
            model: model.map(str::to_owned),
            archived: false,
            input_tokens: 0,
            output_tokens: 0,
        };
        self.conn
            .execute(
                "INSERT INTO conversations (id, title, created_at, updated_at, model, archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                params![
                    conversation.id,
                    conversation.title,
                    conversation.created_at,
                    conversation.updated_at,
                    conversation.model,
                ],
            )
            .map_err(map_sql)?;
        Ok(conversation)
    }

    /// 改标题。
    pub fn rename_conversation(&self, id: &str, title: &str) -> Result<(), AiError> {
        self.conn
            .execute(
                "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, title, now_seconds()],
            )
            .map_err(map_sql)?;
        Ok(())
    }

    /// 记下会话最后使用的模型。
    pub fn set_conversation_model(&self, id: &str, model: &str) -> Result<(), AiError> {
        self.conn
            .execute(
                "UPDATE conversations SET model = ?2 WHERE id = ?1",
                params![id, model],
            )
            .map_err(map_sql)?;
        Ok(())
    }

    /// 删除会话（消息随外键级联删除）。
    pub fn delete_conversation(&self, id: &str) -> Result<(), AiError> {
        self.conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])
            .map_err(map_sql)?;
        Ok(())
    }

    /// 追加一条消息，并推进会话的 `updated_at`。
    ///
    /// `seq` 由本方法自增分配：取当前最大值 +1，因此调用方不需要维护序号。
    /// 流式期间**不落库**——一轮结束后整条写入（见模块文档）。
    pub fn append_message(
        &self,
        conversation_id: &str,
        message: &ChatMessage,
        usage: Option<Usage>,
    ) -> Result<(), AiError> {
        let content_json = serde_json::to_string(message)
            .map_err(|error| AiError::Storage(format!("encode message failed: {error}")))?;
        let next_seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM messages WHERE conversation_id = ?1",
                params![conversation_id],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        let now = now_seconds();
        let (input_tokens, output_tokens) = match usage {
            Some(usage) => (
                Some(usage.input_tokens as i64),
                Some(usage.output_tokens as i64),
            ),
            None => (None, None),
        };
        self.conn
            .execute(
                "INSERT INTO messages
                 (id, conversation_id, seq, role, content_json, created_at, input_tokens, output_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    new_id("msg"),
                    conversation_id,
                    next_seq,
                    message.role.as_str(),
                    content_json,
                    now,
                    input_tokens,
                    output_tokens,
                ],
            )
            .map_err(map_sql)?;
        self.conn
            .execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                params![conversation_id, now],
            )
            .map_err(map_sql)?;
        // 会话级累计：一条消息带 `usage` 就加一笔。调用方只在每轮的最后一条助手消息
        // 上挂 `usage`（见 `AssistantPage::persist_messages`），所以不会重复计。
        if let Some(usage) = usage {
            self.conn
                .execute(
                    "UPDATE conversations
                     SET input_tokens = input_tokens + ?2, output_tokens = output_tokens + ?3
                     WHERE id = ?1",
                    params![
                        conversation_id,
                        i64::from(usage.input_tokens),
                        i64::from(usage.output_tokens),
                    ],
                )
                .map_err(map_sql)?;
        }
        Ok(())
    }

    /// 读取一个会话的全部消息（按 `seq` 升序）。
    pub fn messages(&self, conversation_id: &str) -> Result<Vec<StoredMessage>, AiError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, conversation_id, seq, role, content_json, created_at,
                        input_tokens, output_tokens
                 FROM messages WHERE conversation_id = ?1 ORDER BY seq ASC",
            )
            .map_err(map_sql)?;
        let rows = statement
            .query_map(params![conversation_id], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    seq: row.get(2)?,
                    role: row.get(3)?,
                    content_json: row.get(4)?,
                    created_at: row.get(5)?,
                    input_tokens: row
                        .get::<_, Option<i64>>(6)?
                        .and_then(|v| v.try_into().ok()),
                    output_tokens: row
                        .get::<_, Option<i64>>(7)?
                        .and_then(|v| v.try_into().ok()),
                })
            })
            .map_err(map_sql)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql)
    }

    /// 某个会话的消息条数（左栏展示用）。
    pub fn message_count(&self, conversation_id: &str) -> Result<usize, AiError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                params![conversation_id],
                |row| row.get(0),
            )
            .map_err(map_sql)?;
        Ok(count as usize)
    }

    /// 删掉一个会话里 `seq` 大于等于 `from_seq` 的消息（「重新生成」用）。
    ///
    /// **累计用量要一起回退**：它是按消息累加出来的（见 [`Self::append_message`]），
    /// 只删消息不减总量的话，「重新生成」几次之后会话总量就成了一笔只增不减的假账。
    pub fn truncate_after(&self, conversation_id: &str, from_seq: i64) -> Result<(), AiError> {
        self.conn
            .execute(
                "UPDATE conversations
                 SET input_tokens = MAX(0, input_tokens - COALESCE((
                         SELECT SUM(input_tokens) FROM messages
                         WHERE conversation_id = ?1 AND seq >= ?2), 0)),
                     output_tokens = MAX(0, output_tokens - COALESCE((
                         SELECT SUM(output_tokens) FROM messages
                         WHERE conversation_id = ?1 AND seq >= ?2), 0))
                 WHERE id = ?1",
                params![conversation_id, from_seq],
            )
            .map_err(map_sql)?;
        self.conn
            .execute(
                "DELETE FROM messages WHERE conversation_id = ?1 AND seq >= ?2",
                params![conversation_id, from_seq],
            )
            .map_err(map_sql)?;
        Ok(())
    }
}

/// 默认库路径。
#[must_use]
pub fn default_path() -> PathBuf {
    duck_platform::app_data_dir().join("ai.db")
}

/// 由首条用户消息推一个标题：取首行，压掉空白，超长截断。
///
/// 返回空串表示「没有可用素材」（例如用户只发了空白），由调用方决定兜底文案
/// ——本内核不持有任何界面文案。
#[must_use]
pub fn title_from_message(text: &str) -> String {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let collapsed: String = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TITLE_MAX_CHARS {
        return collapsed;
    }
    let mut title: String = collapsed.chars().take(TITLE_MAX_CHARS).collect();
    title.push('…');
    title
}

/// 当前 Unix 秒。
#[must_use]
pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// 生成一个进程内唯一的 id。
///
/// 不引 uuid：本仓依赖准入从严，而「纳秒时间戳 + 进程内自增计数」在单机单进程
/// 场景下的碰撞概率可以忽略（同一纳秒内还要撞上同一计数器才会重）。
fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{sequence:x}")
}

/// 建表 + 轻量迁移。任何一条到 `ai.db` 的连接都要先过这一关。
///
/// ⚠️ 外键在 SQLite 里默认是关的，必须**每条连接**显式打开，级联删除才会生效。
fn migrate(conn: &Connection) -> Result<(), AiError> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS conversations (
             id TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             model TEXT,
             archived INTEGER NOT NULL DEFAULT 0,
             input_tokens INTEGER NOT NULL DEFAULT 0,
             output_tokens INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS messages (
             id TEXT PRIMARY KEY,
             conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             seq INTEGER NOT NULL,
             role TEXT NOT NULL,
             content_json TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             input_tokens INTEGER,
             output_tokens INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, seq);
         CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(archived, updated_at DESC);",
    )
    .map_err(|error| AiError::Storage(format!("migrate ai.db failed: {error}")))?;
    // 旧库（M1 / M2 建的 `conversations`）没有用量两列，这里逐列补齐。
    // `CREATE TABLE IF NOT EXISTS` 对已存在的表是空操作，不会帮老库加列。
    ensure_column(
        conn,
        "conversations",
        "input_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "conversations",
        "output_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

fn map_sql(error: rusqlite::Error) -> AiError {
    AiError::Storage(error.to_string())
}

/// 建表后补齐缺失的列（轻量迁移）。
///
/// SQLite 的 `ALTER TABLE ADD COLUMN` 没有 `IF NOT EXISTS`，所以先查
/// `PRAGMA table_info`。表名 / 列名 / 类型都是本文件里的常量，不存在注入面。
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), AiError> {
    let present = {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(map_sql)?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(map_sql)?;
        names.filter_map(Result::ok).any(|name| name == column)
    };
    if present {
        return Ok(());
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )
    .map_err(map_sql)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 整库备份 / 还原 / 清空（`crates/duck-app/src/backup.rs` 的数据面）
// ---------------------------------------------------------------------------

/// `ai.db` 里由本模块与 [`crate::kb`] 共同管辖的表。
///
/// **顺序即安全的写入顺序**：父表在前、子表在后，逐条写入也不会出现悬空引用。
const TABLES: [&str; 5] = [
    "conversations",
    "messages",
    "kb_sources",
    "kb_chunks",
    "kb_meta",
];

/// 一张表的搬运结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRestore {
    /// 表名。
    pub table: String,
    /// 从快照搬进活库的行数。
    pub rows: usize,
}

/// `ai.db` 逻辑还原的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiDbRestore {
    /// 逐表明细（顺序即写入顺序）。快照里缺的表不会出现在这里。
    pub tables: Vec<TableRestore>,
}

impl AiDbRestore {
    /// 搬进来的总行数。
    #[must_use]
    pub fn rows(&self) -> usize {
        self.tables.iter().map(|table| table.rows).sum()
    }

    /// 是否真的搬进了东西。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// 把 `live` 库里**所有**表的数据清空（表结构保留）。
///
/// 逻辑清空而不是删文件：应用运行期间 `AiStore` 与 `KbIndex` 各持一条长连接
/// （见 [`crate::kb::KbIndex`] 的文档），`fs::remove_file` 只会把文件从目录里摘掉，
/// 连接继续在这个「已删除」的 inode 上读写——用户看到的是「重置完历史又回来了」。
/// `DELETE FROM` 对持有连接同样安全，且立即生效。
///
/// 返回被删掉的总行数；库不存在时返回 `Ok(0)`。
pub fn clear(live: &Path) -> Result<usize, AiError> {
    if !live.is_file() {
        return Ok(0);
    }
    let conn = open_migrated(live)?;
    // 外键在搬运期间关掉：整库清空的过程中必然短暂不一致，逐表校验只会误报。
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")
        .map_err(map_sql)?;
    let outcome = (|| -> Result<usize, AiError> {
        let mut removed = 0usize;
        for table in TABLES {
            if !table_exists(&conn, "main", table)? {
                continue;
            }
            conn.execute(&format!("DELETE FROM main.\"{table}\""), [])
                .map_err(map_sql)?;
            removed += changed_rows(&conn)?;
        }
        Ok(removed)
    })();
    finish_transaction(&conn, outcome)
}

/// 用 SQLite 自己的原子快照能力，把 `live` 库导出一份**一致副本**到 `dest`。
///
/// 机制是 `VACUUM INTO`（单条 SQL，SQLite ≥ 3.27，本仓 `rusqlite` 的 bundled
/// 版本远高于此）：它在一次读事务里看完整库（**含 WAL 里尚未 checkpoint 的内容**），
/// 再把结果写成一个全新的、干净的文件。因此
///
/// - **不需要** `rusqlite` 的 `backup` feature（不新增依赖）；
/// - **不会**出现「拷到一半、页面不一致」的坏快照——这正是 `fs::copy` 在 WAL 库上的坑；
/// - 与页面的连接**不打架**：这里新开一条只读连接，导完即关。
///
/// `live` 不存在时返回 `Ok(false)`（没有可备份的内容，不是错误）。
pub fn snapshot_to(live: &Path, dest: &Path) -> Result<bool, AiError> {
    if !live.is_file() {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AiError::Storage(format!("create snapshot dir failed: {error}")))?;
    }
    // `VACUUM INTO` 拒绝覆盖已存在的目标文件。
    if dest.exists() {
        std::fs::remove_file(dest)
            .map_err(|error| AiError::Storage(format!("clear stale snapshot failed: {error}")))?;
    }
    let conn = Connection::open(live)
        .map_err(|error| AiError::Storage(format!("open ai.db to snapshot failed: {error}")))?;
    // `VACUUM INTO` 的路径是 SQL 字面量，不能绑定参数；按 SQLite 的规则把单引号翻倍。
    let target = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{target}'"))
        .map_err(|error| AiError::Storage(format!("snapshot ai.db failed: {error}")))?;
    Ok(true)
}

/// 把快照 `snapshot` 的**逻辑内容**搬进活库 `live`（整表先清后插）。
///
/// **不覆盖文件**：应用运行期间 `ai.db` 被 `AiStore` / `KbIndex` 的长连接持有，
/// 直接换文件会让那些连接继续指向被摘掉的 inode。这里改成 `ATTACH` 快照 +
/// 逐表 `DELETE`/`INSERT INTO ... SELECT`，在同一个事务里落进活库——对持有连接
/// 安全，提交后立刻可见。
///
/// **老快照兼容**：快照里没有的表直接跳过（不是错误）；两侧共有的列才搬，
/// 老快照缺的新列由目标表的默认值补上。
///
/// **只读边界**：`vault.key` 不在 `ai.db` 里，密文随包走但换台机器解不开——
/// 这是 [`crate::secret`] 的有意设计，本函数不碰任何密钥文件。
pub fn restore_from(snapshot: &Path, live: &Path) -> Result<AiDbRestore, AiError> {
    if !snapshot.is_file() {
        return Err(AiError::Storage(format!(
            "snapshot file is missing: {}",
            snapshot.display()
        )));
    }
    let conn = open_migrated(live)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS incoming",
        params![snapshot.to_string_lossy()],
    )
    .map_err(map_sql)?;
    let restored = copy_all_tables(&conn);
    // 无论成败都要摘掉：`DETACH` 不能在事务里做，`copy_all_tables` 已经收尾过事务。
    let _ = conn.execute("DETACH DATABASE incoming", []);
    restored
}

/// 打开（必要时创建）`path` 上的一条连接，并保证会话表与 `kb_*` 表都存在。
fn open_migrated(path: &Path) -> Result<Connection, AiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AiError::Storage(format!("create data dir failed: {error}")))?;
    }
    let conn = Connection::open(path)
        .map_err(|error| AiError::Storage(format!("open ai.db failed: {error}")))?;
    migrate(&conn)?;
    // 会话与知识库同库异表：还原时目标库也得有 `kb_*`，否则索引行无处安放。
    crate::kb::ensure_schema(&conn)?;
    Ok(conn)
}

/// 把 `incoming` 上每张 `TABLES` 表整表搬进 `main`（事务内，先清后插）。
fn copy_all_tables(conn: &Connection) -> Result<AiDbRestore, AiError> {
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")
        .map_err(map_sql)?;
    let outcome = (|| -> Result<AiDbRestore, AiError> {
        let mut restore = AiDbRestore::default();
        for table in TABLES {
            // 老快照 / 老库没有这张表：跳过而不是报错。
            if !table_exists(conn, "incoming", table)? {
                continue;
            }
            let columns = shared_columns(conn, table)?;
            if columns.is_empty() {
                continue;
            }
            let list = columns
                .iter()
                .map(|column| format!("\"{column}\""))
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute(&format!("DELETE FROM main.\"{table}\""), [])
                .map_err(map_sql)?;
            conn.execute(
                &format!(
                    "INSERT INTO main.\"{table}\" ({list}) SELECT {list} FROM incoming.\"{table}\""
                ),
                [],
            )
            .map_err(map_sql)?;
            restore.tables.push(TableRestore {
                table: (*table).to_owned(),
                rows: changed_rows(conn)?,
            });
        }
        Ok(restore)
    })();
    finish_transaction(conn, outcome)
}

/// 提交 / 回滚一个手写事务，并把外键开关恢复回去。
///
/// 手写事务是为了让 `copy_all_tables` / `clear` 的**多表搬运**成为一个原子动作：
/// 中途出错就整段回滚，不会留下「带走了会话、没带走索引」的半截状态。
fn finish_transaction<T>(conn: &Connection, outcome: Result<T, AiError>) -> Result<T, AiError> {
    match outcome {
        Ok(value) => {
            conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")
                .map_err(map_sql)?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
            Err(error)
        }
    }
}

/// 上一次写入影响的行数（`INSERT ... SELECT` / `DELETE` 之后读）。
fn changed_rows(conn: &Connection) -> Result<usize, AiError> {
    let rows: i64 = conn
        .query_row("SELECT changes()", [], |row| row.get(0))
        .map_err(map_sql)?;
    Ok(rows.max(0) as usize)
}

/// `schema.table` 是否存在（`main` / `incoming` 都用它）。
fn table_exists(conn: &Connection, schema: &str, table: &str) -> Result<bool, AiError> {
    let found: Option<i64> = conn
        .query_row(
            &format!("SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1"),
            params![table],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sql)?;
    Ok(found.is_some())
}

/// `table` 在 `main` 与 `incoming` 两侧**共有**的列（保持 `main` 的列序）。
///
/// 取交集是为了兼容老快照：老库缺的新列不会让整张表的搬运失败，而是由目标表的
/// `DEFAULT` 补上（`conversations` 的两个用量列就是这种情况）。
fn shared_columns(conn: &Connection, table: &str) -> Result<Vec<String>, AiError> {
    let main_columns = table_columns(conn, "main", table)?;
    let incoming_columns = table_columns(conn, "incoming", table)?;
    Ok(main_columns
        .into_iter()
        .filter(|column| incoming_columns.contains(column))
        .collect())
}

/// `PRAGMA <schema>.table_info(<table>)` 的列名（按声明顺序）。
fn table_columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>, AiError> {
    // 表名来自本文件的常量，schema 只有 "main" / "incoming" 两个取值，无注入面。
    let mut statement = conn
        .prepare(&format!("PRAGMA {schema}.table_info(\"{table}\")"))
        .map_err(map_sql)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_sql)?;
    Ok(names.filter_map(Result::ok).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_roundtrip() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store
            .create_conversation("first chat", Some("gpt-4o-mini"))
            .unwrap();
        assert_eq!(store.list_conversations().unwrap().len(), 1);
        assert_eq!(store.list_conversations().unwrap()[0].id, conversation.id);
        assert_eq!(
            store.list_conversations().unwrap()[0].model.as_deref(),
            Some("gpt-4o-mini")
        );

        store
            .rename_conversation(&conversation.id, "renamed")
            .unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].title, "renamed");
    }

    #[test]
    fn messages_keep_order_and_usage() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        store
            .append_message(&conversation.id, &ChatMessage::user("hi"), None)
            .unwrap();
        store
            .append_message(
                &conversation.id,
                &ChatMessage::assistant("hello"),
                Some(Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    total_tokens: 5,
                }),
            )
            .unwrap();

        let messages = store.messages(&conversation.id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].seq, 0);
        assert_eq!(messages[1].seq, 1);
        assert_eq!(messages[0].message().unwrap().text(), "hi");
        assert_eq!(messages[1].output_tokens, Some(2));
        assert_eq!(store.message_count(&conversation.id).unwrap(), 2);
    }

    #[test]
    fn delete_cascades_to_messages() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        store
            .append_message(&conversation.id, &ChatMessage::user("hi"), None)
            .unwrap();
        store.delete_conversation(&conversation.id).unwrap();
        assert!(store.list_conversations().unwrap().is_empty());
        assert_eq!(store.message_count(&conversation.id).unwrap(), 0);
    }

    #[test]
    fn usage_accumulates_on_the_conversation() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        for _ in 0..2 {
            store
                .append_message(
                    &conversation.id,
                    &ChatMessage::assistant("hello"),
                    Some(Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                        total_tokens: 5,
                    }),
                )
                .unwrap();
        }
        let stored = store.list_conversations().unwrap()[0].clone();
        assert_eq!(stored.input_tokens, 6);
        assert_eq!(stored.output_tokens, 4);
        assert_eq!(stored.total_tokens(), 10);
    }

    /// 老库（没有用量两列）打开时补列，且历史数据不丢。
    #[test]
    fn old_database_gains_usage_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 model TEXT, archived INTEGER NOT NULL DEFAULT 0);
             INSERT INTO conversations (id, title, created_at, updated_at, archived)
             VALUES ('c', 'legacy', 1, 1, 0);",
        )
        .unwrap();
        let store = AiStore::from_connection(conn).unwrap();
        let stored = store.list_conversations().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].title, "legacy");
        assert_eq!(stored[0].total_tokens(), 0);
        store
            .append_message(
                "c",
                &ChatMessage::assistant("hi"),
                Some(Usage {
                    input_tokens: 1,
                    output_tokens: 4,
                    total_tokens: 5,
                }),
            )
            .unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].output_tokens, 4);
    }

    #[test]
    fn truncate_after_drops_the_tail() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        for text in ["a", "b", "c"] {
            store
                .append_message(&conversation.id, &ChatMessage::user(text), None)
                .unwrap();
        }
        store.truncate_after(&conversation.id, 1).unwrap();
        let messages = store.messages(&conversation.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message().unwrap().text(), "a");
    }

    /// 截断要把累计用量一起回退，否则「重新生成」会让总量只增不减。
    #[test]
    fn truncate_after_rolls_back_the_usage_totals() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        let usage = Usage {
            input_tokens: 3,
            output_tokens: 2,
            total_tokens: 5,
        };
        store
            .append_message(&conversation.id, &ChatMessage::user("q"), None)
            .unwrap();
        store
            .append_message(&conversation.id, &ChatMessage::assistant("a"), Some(usage))
            .unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].total_tokens(), 5);

        // 从「那条助手消息」起截断（seq = 1）。
        store.truncate_after(&conversation.id, 1).unwrap();
        assert_eq!(store.list_conversations().unwrap()[0].total_tokens(), 0);
    }

    #[test]
    fn assistant_message_with_tool_calls_survives_roundtrip() {
        let store = AiStore::open_in_memory().unwrap();
        let conversation = store.create_conversation("c", None).unwrap();
        let message = ChatMessage {
            role: crate::wire::Role::Assistant,
            blocks: vec![
                crate::wire::ContentBlock::Thinking { text: "hmm".into() },
                crate::wire::ContentBlock::Text {
                    text: "done".into(),
                },
                crate::wire::ContentBlock::ToolUse {
                    call: crate::wire::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"a.txt\"}".into(),
                    },
                },
            ],
        };
        store
            .append_message(&conversation.id, &message, None)
            .unwrap();
        let restored = store.messages(&conversation.id).unwrap()[0]
            .message()
            .unwrap();
        assert_eq!(restored, message);
        assert_eq!(restored.thinking().as_deref(), Some("hmm"));
        assert_eq!(restored.tool_calls().len(), 1);
    }

    #[test]
    fn title_derivation() {
        assert_eq!(title_from_message("  hello world  "), "hello world");
        assert_eq!(title_from_message("first\nsecond"), "first");
        assert_eq!(title_from_message("  \n\n  "), "");
        let long = "字".repeat(40);
        let title = title_from_message(&long);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn ids_are_unique() {
        let a = new_id("x");
        let b = new_id("x");
        assert_ne!(a, b);
        assert!(a.starts_with("x-"));
    }

    // -- 整库快照 / 还原 / 清空 ------------------------------------------

    /// 独立临时目录；进程内自增序号避免并行测试撞同一个目录。
    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-ai-store-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个「有会话、有消息、也有知识库元数据」的活库。
    ///
    /// KB 那半走裸 `Connection`：`KbIndex` 没有写 `kb_meta` 的公开入口，
    /// 而这里要验的正是「同库异表的数据有没有一起搬走」。
    fn seeded_live_db(path: &Path) {
        let store = AiStore::open(path).unwrap();
        let conversation = store.create_conversation("标题", Some("m")).unwrap();
        store
            .append_message(&conversation.id, &ChatMessage::user("hi"), None)
            .unwrap();
        store
            .append_message(
                &conversation.id,
                &ChatMessage::assistant("hello"),
                Some(Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    total_tokens: 5,
                }),
            )
            .unwrap();
        drop(store);
        let conn = Connection::open(path).unwrap();
        // `AiStore` 只建会话两张表，`kb_*` 归 `KbIndex`——这里把两半都备齐。
        crate::kb::ensure_schema(&conn).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kb_sources (id, label, path, fingerprint, files, chunks, indexed_at)
             VALUES ('s1', 'src', '/tmp/does-not-matter', 'fp', 1, 2, 7)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kb_meta (key, value) VALUES ('probe', 'kept')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn snapshot_round_trips_conversations_and_kb() {
        let dir = scratch("roundtrip");
        let live = dir.join("ai.db");
        let snap = dir.join("snap.db");
        seeded_live_db(&live);

        assert!(snapshot_to(&live, &snap).unwrap(), "有库就该产出快照");
        assert!(snap.is_file());

        // 清空活库：行数归零，表结构还在。
        let removed = clear(&live).unwrap();
        assert!(
            removed >= 4,
            "会话 1 + 消息 2 + 来源 1 + meta 1，实际 {removed}"
        );
        assert!(
            AiStore::open(&live)
                .unwrap()
                .list_conversations()
                .unwrap()
                .is_empty()
        );

        // 逻辑还原：文件没被换掉，内容回来了。
        let restored = restore_from(&snap, &live).unwrap();
        assert_eq!(restored.rows(), 5, "会话 1 + 消息 2 + 来源 1 + meta 1");

        let store = AiStore::open(&live).unwrap();
        let conversations = store.list_conversations().unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(store.messages(&conversations[0].id).unwrap().len(), 2);

        let kb = crate::kb::KbIndex::open(&live).unwrap();
        assert_eq!(kb.stats().unwrap().sources, 1, "知识库来源要随库一起回来");
        let conn = Connection::open(&live).unwrap();
        let probe: String = conn
            .query_row("SELECT value FROM kb_meta WHERE key = 'probe'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(probe, "kept", "kb_meta 也是 ai.db 的一部分");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_skips_a_missing_database() {
        let dir = scratch("missing");
        let snap = dir.join("snap.db");

        assert!(!snapshot_to(&dir.join("nope.db"), &snap).unwrap());
        assert!(!snap.exists(), "没库就不该留下空快照");
        assert_eq!(clear(&dir.join("nope.db")).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_rejects_a_missing_snapshot_file() {
        let dir = scratch("no-snapshot");
        let error = restore_from(&dir.join("nope.db"), &dir.join("ai.db")).unwrap_err();
        assert!(matches!(error, AiError::Storage(_)), "实际 {error:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_into_a_fresh_database_creates_the_schema() {
        let dir = scratch("fresh");
        let old = dir.join("old.db");
        let snap = dir.join("snap.db");
        let live = dir.join("ai.db");
        seeded_live_db(&old);
        assert!(snapshot_to(&old, &snap).unwrap());

        // 目标库根本不存在：还原要能建库（含 kb_* 表）并落数据。
        let restored = restore_from(&snap, &live).unwrap();
        assert_eq!(restored.rows(), 5);
        assert_eq!(
            AiStore::open(&live)
                .unwrap()
                .list_conversations()
                .unwrap()
                .len(),
            1
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_tolerates_a_snapshot_without_the_kb_tables() {
        let dir = scratch("old-snapshot");
        let old = dir.join("old.db");
        let snap = dir.join("snap.db");
        seeded_live_db(&old);
        assert!(snapshot_to(&old, &snap).unwrap());
        // 模拟「老版本产出的包」：那份 ai.db 里根本没有 kb_* 三张表。
        {
            let conn = Connection::open(&snap).unwrap();
            conn.execute_batch("DROP TABLE kb_chunks; DROP TABLE kb_sources; DROP TABLE kb_meta;")
                .unwrap();
        }

        let live = dir.join("ai.db");
        seeded_live_db(&live);
        let restored = restore_from(&snap, &live).unwrap();

        assert!(
            restored
                .tables
                .iter()
                .any(|table| table.table == "conversations")
        );
        assert!(
            !restored
                .tables
                .iter()
                .any(|table| table.table.starts_with("kb_")),
            "快照里没有的 kb_* 表不该出现在还原明细里"
        );
        // 活库里的知识库没被清掉：搬不回来就不动它，胜过一次「清了却补不回」。
        let kb = crate::kb::KbIndex::open(&live).unwrap();
        assert_eq!(kb.stats().unwrap().sources, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
