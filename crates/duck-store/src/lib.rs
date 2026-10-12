//! Duck Toolbox 持久化层：内嵌 SQLite + 版本化迁移 + 去抖写入。
//!
//! 分两条连接：读连接（`Mutex<Connection>`）给 UI 同步查询，写连接交给
//! [`writer`] 的写线程做批量事务。WAL 模式下两者互不阻塞。
//!
//! 与 UI 层的耦合只有一个 [`Store::install`]：装入 gpui 全局；其余 API 都是普通方法，
//! 所以本 crate 的读写逻辑可以在没有 gpui 的单元测试里直接跑（见 [`db`] 的测试）。

mod db;
mod writer;

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::{App, Global};
use rusqlite::Connection;

pub use db::{StoreError, now_seconds};
pub use writer::{DEBOUNCE, Op, Table, Writer};

/// 存储键：外观设置。整份 [`duck_theme::AppearanceWire`] 的 JSON。
pub const KEY_APPEARANCE: &str = "appearance";

/// 全局存储句柄。
pub struct Store {
    /// 读连接：同步查询，加锁时间短。
    conn: Mutex<Connection>,
    /// 写句柄：投递到去抖线程。
    writer: Writer,
}

impl Global for Store {}

impl Store {
    /// 打开（或创建）数据库；`path` 的父目录会自动建立。
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        let read = db::open(&path)?;
        let write = db::open(&path)?;
        tracing::info!(path = %path.display(), "persistent store opened");
        Ok(Self {
            conn: Mutex::new(read),
            writer: Writer::spawn(write),
        })
    }

    /// 在 [`duck_platform::db_path`] 处打开并装入全局。
    pub fn install(cx: &mut App) -> Result<(), StoreError> {
        let store = Self::open(duck_platform::db_path())?;
        cx.set_global(store);
        Ok(())
    }

    /// 读取全局存储；未 [`install`](Self::install) 时为 `None`。
    #[must_use]
    pub fn global(cx: &App) -> Option<&Self> {
        cx.try_global::<Self>()
    }

    /// 读全局键值（含应用设置、最近项等）。
    #[must_use]
    pub fn kv_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    }

    /// 写全局键值（去抖，异步落盘）。
    pub fn kv_put(&self, key: impl Into<String>, value: impl Into<String>) {
        self.writer.send(Op::PutKv {
            key: key.into(),
            value: value.into(),
        });
    }

    /// 删除全局键值（去抖，异步落盘）。
    ///
    /// 给「这份记录不该再存在」用（例如卸载插件时收回它的能力授权）。
    /// 注意与 `kv_put("")` 的区别：后者留下一行**空值**，读回来是 `Some("")`
    /// 而不是 `None`，凡是把「没有这一行」当缺省语义的地方都会被它骗到。
    pub fn kv_delete(&self, key: impl Into<String>) {
        self.writer.send(Op::DeleteKv { key: key.into() });
    }

    /// 读出 `kv` 全表快照（按 `key` 升序）。
    ///
    /// 供数据导出用：`kv` 里放的是应用设置（主题、语言、各种开关），是全库最值得
    /// 备份的一张表。不含凭据——口令在这张表里一律是密文（见 `duck-platform::vault`）。
    #[must_use]
    pub fn kv_all(&self) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut statement) = conn.prepare("SELECT key, value FROM kv ORDER BY key") else {
            return Vec::new();
        };
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// 覆盖式导入键值：整张 `kv` 先清空，再写入 `pairs`（去抖，异步落盘）。
    ///
    /// 调用方要确保「导入已生效」时，必须先 [`Store::flush`] 再读。
    pub fn kv_replace_all(&self, pairs: Vec<(String, String)>) {
        self.writer.send(Op::ReplaceKv { pairs });
    }

    /// 清空指定表（去抖，异步落盘）。空列表是空操作。
    pub fn clear(&self, tables: &[Table]) {
        if tables.is_empty() {
            return;
        }
        self.writer.send(Op::ClearTables {
            tables: tables.to_vec(),
        });
    }

    /// 读窗口几何 JSON。
    #[must_use]
    pub fn window_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT bounds FROM windows WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    }

    /// 写窗口几何 JSON（去抖）。
    pub fn window_put(
        &self,
        key: impl Into<String>,
        bounds: impl Into<String>,
        monitor: Option<String>,
    ) {
        self.writer.send(Op::PutWindow {
            key: key.into(),
            bounds: bounds.into(),
            monitor,
        });
    }

    /// 记一次「最近使用」。`kind` 区分 `command` / `tool`。
    pub fn touch_recent(&self, kind: impl Into<String>, id: impl Into<String>) {
        self.writer.send(Op::TouchRecent {
            kind: kind.into(),
            id: id.into(),
        });
    }

    /// 按最近时间倒序读取某类最近项（最多 `limit` 条）。
    #[must_use]
    pub fn recent(&self, kind: &str, limit: usize) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut statement) =
            conn.prepare("SELECT id FROM recent WHERE kind = ?1 ORDER BY at DESC LIMIT ?2")
        else {
            return Vec::new();
        };
        statement
            .query_map(rusqlite::params![kind, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// 读某类最近项里、时间落在最近 `seconds` 秒内的 id（按时间倒序）。
    ///
    /// **注意这张表不是使用日志**：主键是 `(kind, id)`，同一个 id 只留最后一次时间
    /// （见 [`Self::touch_recent`]）。所以返回的条数是「这段时间里动过的**不同**项数」，
    /// 不是调用次数。要真正的次数得另建一张明细表 —— 为了一个概览指标不值得。
    #[must_use]
    pub fn recent_since(&self, kind: &str, seconds: i64) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let cutoff = crate::db::now_seconds() - seconds.max(0);
        let Ok(mut statement) =
            conn.prepare("SELECT id FROM recent WHERE kind = ?1 AND at >= ?2 ORDER BY at DESC")
        else {
            return Vec::new();
        };
        statement
            .query_map(rusqlite::params![kind, cutoff], |row| {
                row.get::<_, String>(0)
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// 读某个工具的一项设置。
    #[must_use]
    pub fn tool_state_get(&self, tool_id: &str, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT value FROM tool_state WHERE tool_id = ?1 AND key = ?2",
            rusqlite::params![tool_id, key],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    /// 写某个工具的一项设置（去抖，异步落盘）。
    pub fn tool_state_put(
        &self,
        tool_id: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.writer.send(Op::PutToolState {
            tool_id: tool_id.into(),
            key: key.into(),
            value: value.into(),
        });
    }

    /// 读某个工具的全部设置（`key` → `value`）。
    #[must_use]
    pub fn tool_state_all(&self, tool_id: &str) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut statement) =
            conn.prepare("SELECT key, value FROM tool_state WHERE tool_id = ?1 ORDER BY key")
        else {
            return Vec::new();
        };
        statement
            .query_map([tool_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// 阻塞直到累积的写都落盘。
    pub fn flush(&self) {
        self.writer.flush();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // 正常退出路径上把待写数据落盘，避免「刚改的设置没保存」。
        self.writer.flush();
    }
}

/// 读全局键值（`Store` 缺席时返回 `None`）。
#[must_use]
pub fn kv_get(cx: &App, key: &str) -> Option<String> {
    Store::global(cx)?.kv_get(key)
}

/// 写全局键值（`Store` 缺席时静默忽略）。
pub fn kv_put(cx: &App, key: impl Into<String>, value: impl Into<String>) {
    if let Some(store) = Store::global(cx) {
        store.kv_put(key, value);
    }
}

/// 删除全局键值（`Store` 缺席时静默忽略）。
pub fn kv_delete(cx: &App, key: impl Into<String>) {
    if let Some(store) = Store::global(cx) {
        store.kv_delete(key);
    }
}

/// 覆盖式导入全局键值（`Store` 缺席时静默忽略）。
pub fn kv_replace_all(cx: &App, pairs: Vec<(String, String)>) {
    if let Some(store) = Store::global(cx) {
        store.kv_replace_all(pairs);
    }
}

/// 清空指定表（`Store` 缺席时静默忽略）。
pub fn clear_tables(cx: &App, tables: &[Table]) {
    if let Some(store) = Store::global(cx) {
        store.clear(tables);
    }
}

/// 落盘全部待写数据。
pub fn flush(cx: &App) {
    if let Some(store) = Store::global(cx) {
        store.flush();
    }
}

/// 读工具设置项（`Store` 缺席时返回 `None`）。
#[must_use]
pub fn tool_state_get(cx: &App, tool_id: &str, key: &str) -> Option<String> {
    Store::global(cx)?.tool_state_get(tool_id, key)
}

/// 写工具设置项（`Store` 缺席时静默忽略）。
pub fn tool_state_put(cx: &App, tool_id: &str, key: &str, value: impl Into<String>) {
    if let Some(store) = Store::global(cx) {
        store.tool_state_put(tool_id, key, value);
    }
}
