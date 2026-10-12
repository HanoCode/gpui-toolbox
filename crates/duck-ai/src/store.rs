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

use rusqlite::{Connection, params};

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
        // 外键在 SQLite 里默认是关的，必须每条连接显式打开，级联删除才会生效。
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS conversations (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 model TEXT,
                 archived INTEGER NOT NULL DEFAULT 0
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
        Ok(Self { conn })
    }

    /// 全部未归档会话，最近更新的在前。
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, AiError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, title, created_at, updated_at, model, archived
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
    pub fn truncate_after(&self, conversation_id: &str, from_seq: i64) -> Result<(), AiError> {
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

fn map_sql(error: rusqlite::Error) -> AiError {
    AiError::Storage(error.to_string())
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
}
