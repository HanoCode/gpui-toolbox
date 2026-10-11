//! 去抖写入线程：把高频写操作合并成事务，避免每次改动都落一次盘。
//!
//! 工作方式（对齐 dodo `session` 的取舍）：
//! - 调用方投递 [`Op`] 到通道，立即返回，不阻塞 UI；
//! - 写入线程收到第一条后等 [`DEBOUNCE`]，期间到达的 op 合并进同一批；
//! - 同一批里相同主键的写「后写覆盖前写」，只落一次；
//! - [`Op::Flush`] 跳过等待，立刻落盘并回执（退出前调用，保证不丢）。
//!
//! 写线程持有自己的连接；读走另一条连接（见 [`crate::Store`]），WAL 下两者互不阻塞。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

use crate::db::{StoreError, now_seconds};

/// 合并窗口：在这段时间内到达的写会被并成一次事务。
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// 可以被整体清空的表（数据重置用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    /// 全局键值（应用设置）。
    Kv,
    /// 「最近使用」记录。
    Recent,
    /// 工具级设置。
    ToolState,
    /// 窗口几何。
    Windows,
}

impl Table {
    /// 表名（只来自本枚举，不接用户输入，拼接是安全的）。
    #[must_use]
    pub const fn sql_name(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Recent => "recent",
            Self::ToolState => "tool_state",
            Self::Windows => "windows",
        }
    }
}

/// 一次写操作。
pub enum Op {
    /// 写全局键值。
    PutKv { key: String, value: String },
    /// 覆盖式导入键值：整张 `kv` 先清空，再写入 `pairs`。
    ///
    /// 与 [`Op::PutKv`] 的先后关系就是它们在批次里的先后关系 —— 后到的覆盖先到的，
    /// 和「同一批里相同主键后写覆盖前写」是同一条规则。
    ReplaceKv { pairs: Vec<(String, String)> },
    /// 按 key 删除一行。
    ///
    /// 与 [`Op::PutKv`] 同样遵守「批次里的先后即最终结果」：先删后写＝写进去，
    /// 先写后删＝删掉。**不能**简单理解成「本批先统一删一遍」——那会让
    /// 「删掉旧值再写新值」变成「什么都没写」。
    DeleteKv { key: String },
    /// 清空指定表。**清空先于本批的写入执行**，否则同批里的写会被自己清掉。
    ClearTables { tables: Vec<Table> },
    /// 写窗口几何；`bounds` 是 JSON（`x/y/width/height`）。
    PutWindow {
        key: String,
        bounds: String,
        monitor: Option<String>,
    },
    /// 写入「最近使用」；`kind` 区分命令 / 工具。
    TouchRecent { kind: String, id: String },
    /// 写工具级设置项。
    PutToolState {
        tool_id: String,
        key: String,
        value: String,
    },
    /// 强制落盘并回执。
    Flush(Sender<()>),
}

/// 写入端句柄。
pub struct Writer {
    tx: Sender<Op>,
}

impl Writer {
    /// 启动写线程。
    pub fn spawn(connection: Connection) -> Self {
        let (tx, rx) = mpsc::channel::<Op>();
        thread::Builder::new()
            .name("duck-store-writer".to_owned())
            .spawn(move || run(connection, &rx))
            .expect("failed to spawn store writer thread");
        Self { tx }
    }

    /// 投递一条 op；写线程已退出时静默丢弃（不 panic）。
    pub fn send(&self, op: Op) {
        if self.tx.send(op).is_err() {
            tracing::warn!("store writer thread has exited; dropping this write");
        }
    }

    /// 阻塞直到当前累积的写都落盘。
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Op::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn run(mut connection: Connection, rx: &mpsc::Receiver<Op>) {
    let mut batch: Vec<Op> = Vec::new();
    loop {
        let first = match rx.recv() {
            Ok(op) => op,
            Err(_) => break,
        };
        let mut immediate = matches!(first, Op::Flush(_));
        batch.push(first);
        while !immediate {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(op) => {
                    immediate = matches!(op, Op::Flush(_));
                    batch.push(op);
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    let acks = apply(&mut connection, &mut batch);
                    for ack in acks {
                        let _ = ack.send(());
                    }
                    return;
                }
            }
        }
        let acks = apply(&mut connection, &mut batch);
        for ack in acks {
            let _ = ack.send(());
        }
    }
}

/// 合并并执行一批写操作；返回本批需要回执的 Flush 通道。
///
/// 出错只记日志，不让写线程死掉——回执照发，避免调用方 [`Writer::flush`] 一直等。
fn apply(connection: &mut Connection, batch: &mut Vec<Op>) -> Vec<Sender<()>> {
    // 相同主键后写覆盖前写：KV / window / recent / tool_state 各自去重。
    let mut kv: HashMap<String, String> = HashMap::new();
    // 本批要删掉的 KV 键。与 `kv` 互斥：同一个 key 出现在哪边，取决于批次里谁在后面。
    let mut kv_deleted: HashSet<String> = HashSet::new();
    let mut windows: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut recent: HashSet<(String, String)> = HashSet::new();
    let mut tool_state: HashMap<(String, String), String> = HashMap::new();
    // 需要先清空的表（去重即可，顺序无关）；`kv_reset` 是覆盖式导入触发的整表清空。
    let mut clear: HashSet<Table> = HashSet::new();
    let mut kv_reset = false;
    let mut acks: Vec<Sender<()>> = Vec::new();
    for op in batch.drain(..) {
        match op {
            Op::PutKv { key, value } => {
                kv_deleted.remove(&key);
                kv.insert(key, value);
            }
            Op::ReplaceKv { pairs } => {
                kv_reset = true;
                // 整表重写会盖掉此前所有的删除（连待删的键也不存在了）。
                kv_deleted.clear();
                for (key, value) in pairs {
                    kv.insert(key, value);
                }
            }
            Op::DeleteKv { key } => {
                kv.remove(&key);
                kv_deleted.insert(key);
            }
            Op::ClearTables { tables } => {
                clear.extend(tables);
            }
            Op::PutWindow {
                key,
                bounds,
                monitor,
            } => {
                windows.insert(key, (bounds, monitor));
            }
            Op::TouchRecent { kind, id } => {
                recent.insert((kind, id));
            }
            Op::PutToolState {
                tool_id,
                key,
                value,
            } => {
                tool_state.insert((tool_id, key), value);
            }
            Op::Flush(ack) => acks.push(ack),
        }
    }

    let now = now_seconds();
    let result = (|| -> Result<(), StoreError> {
        let tx = connection.transaction()?;
        // 清空必须先于写入：否则同一批里的写会被自己这批的清空抹掉。
        if kv_reset {
            clear.insert(Table::Kv);
        }
        for table in &clear {
            tx.execute(&format!("DELETE FROM {}", table.sql_name()), [])?;
        }
        // 删除排在写入之前：同一个 key 两边都有时，只会是「先删后写」那一半
        // （另一半在后写时已经把它从待删集合里摘掉了）。
        for key in &kv_deleted {
            tx.execute("DELETE FROM kv WHERE key = ?1", rusqlite::params![key])?;
        }
        for (key, value) in &kv {
            tx.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                rusqlite::params![key, value, now],
            )?;
        }
        for (key, (bounds, monitor)) in &windows {
            tx.execute(
                "INSERT INTO windows(key, bounds, monitor_id, updated_at) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET bounds = excluded.bounds,
                     monitor_id = excluded.monitor_id, updated_at = excluded.updated_at",
                rusqlite::params![key, bounds, monitor, now],
            )?;
        }
        for (kind, id) in &recent {
            tx.execute(
                "INSERT INTO recent(kind, id, at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(kind, id) DO UPDATE SET at = excluded.at",
                rusqlite::params![kind, id, now],
            )?;
        }
        for ((tool_id, key), value) in &tool_state {
            tx.execute(
                "INSERT INTO tool_state(tool_id, key, value, updated_at) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(tool_id, key) DO UPDATE SET value = excluded.value,
                     updated_at = excluded.updated_at",
                rusqlite::params![tool_id, key, value, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    })();

    if let Err(error) = result {
        tracing::error!(%error, "batch write failed");
    }
    acks
}

#[cfg(test)]
mod tests {
    use super::{Op, Table};
    use crate::Store;

    /// 临时库文件；调用方负责在结束时删除。
    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建立临时目录");
        let path = dir.join(format!("{tag}.sqlite"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    #[test]
    fn tool_state_round_trips_and_survives_reopen() {
        let path = temp_db("tool-state");
        let store = Store::open(path.clone()).expect("打开库");

        // 同一批里同键后写覆盖前写，且 key 与 tool_id 两级都要区分开。
        store.tool_state_put("video.parse", "enabled", "false");
        store.tool_state_put("video.parse", "enabled", "true");
        store.tool_state_put("video.parse", "runs", "3");
        store.tool_state_put("image.convert", "enabled", "false");
        store.flush();

        assert_eq!(
            store.tool_state_get("video.parse", "enabled").as_deref(),
            Some("true"),
            "同键后写必须覆盖前写"
        );
        assert_eq!(
            store.tool_state_get("video.parse", "runs").as_deref(),
            Some("3")
        );
        assert_eq!(
            store.tool_state_get("image.convert", "enabled").as_deref(),
            Some("false"),
            "不同工具的同名键互不影响"
        );
        assert_eq!(store.tool_state_all("video.parse").len(), 2);
        assert!(store.tool_state_get("video.parse", "missing").is_none());

        // 换一条连接重开：验证真的落盘，而不是只活在内存里。
        drop(store);
        let reopened = Store::open(path.clone()).expect("重新打开");
        assert_eq!(
            reopened.tool_state_get("video.parse", "enabled").as_deref(),
            Some("true")
        );
        drop(reopened);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn flush_is_a_batch_boundary() {
        let path = temp_db("flush-barrier");
        let store = Store::open(path.clone()).expect("打开库");
        store.kv_put("appearance", "{\"a\":1}");
        store.flush();
        assert_eq!(store.kv_get("appearance").as_deref(), Some("{\"a\":1}"));
        store.kv_put("appearance", "{\"a\":2}");
        // 未 flush 时读连接可能还看不到新值（去抖窗口内），flush 后必须可见。
        store.flush();
        assert_eq!(store.kv_get("appearance").as_deref(), Some("{\"a\":2}"));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dedupe_keeps_last_write_per_primary_key() {
        let mut connection = crate::db::open(&temp_db("dedupe")).expect("打开库");
        let mut batch = vec![
            Op::PutKv {
                key: "k".to_owned(),
                value: "first".to_owned(),
            },
            Op::PutKv {
                key: "k".to_owned(),
                value: "second".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut batch);
        let value: String = connection
            .query_row("SELECT value FROM kv WHERE key = 'k'", [], |row| row.get(0))
            .expect("应已写入");
        assert_eq!(value, "second");
    }

    fn kv_row(connection: &rusqlite::Connection, key: &str) -> Option<String> {
        connection
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .ok()
    }

    /// 覆盖式导入：整表清空后再写，恢复包里没带的键必须消失。
    #[test]
    fn replace_kv_wipes_the_table_first() {
        let mut connection = crate::db::open(&temp_db("replace-kv")).expect("打开库");
        let mut seed = vec![
            Op::PutKv {
                key: "appearance".to_owned(),
                value: "old".to_owned(),
            },
            Op::PutKv {
                key: "stale".to_owned(),
                value: "gone".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut seed);

        let mut batch = vec![Op::ReplaceKv {
            pairs: vec![("appearance".to_owned(), "new".to_owned())],
        }];
        super::apply(&mut connection, &mut batch);

        assert_eq!(
            kv_row(&connection, "appearance").as_deref(),
            Some("new"),
            "包内有的键取包内值"
        );
        assert!(
            kv_row(&connection, "stale").is_none(),
            "包内没有的键必须被清掉，否则「恢复」只是「叠加」"
        );
    }

    /// 同一批里「清空」先于「写入」执行，写进去的值不会被自己这批清掉。
    #[test]
    fn clear_runs_before_writes_in_the_same_batch() {
        let mut connection = crate::db::open(&temp_db("clear-order")).expect("打开库");
        let mut seed = vec![Op::PutKv {
            key: "k".to_owned(),
            value: "old".to_owned(),
        }];
        super::apply(&mut connection, &mut seed);

        let mut batch = vec![
            Op::ClearTables {
                tables: vec![Table::Kv],
            },
            Op::PutKv {
                key: "k".to_owned(),
                value: "fresh".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut batch);

        assert_eq!(
            kv_row(&connection, "k").as_deref(),
            Some("fresh"),
            "清空必须排在写入之前"
        );
    }

    /// 删除一个键：**同一批里与 `PutKv` 的先后关系就是最终结果**。
    ///
    /// 这条语义不是钻牛角尖：`Op` 是异步投递的，`kv_put` 与 `kv_delete` 很容易落进
    /// 同一个 400ms 批次里（例如「卸载时先写新值再清理旧记录」），把删除一律提到
    /// 最前面会让后一次写凭空消失。
    #[test]
    fn a_delete_and_a_put_resolve_by_their_order_in_the_batch() {
        let mut connection = crate::db::open(&temp_db("delete-kv")).expect("打开库");
        let mut seed = vec![
            Op::PutKv {
                key: "keep".to_owned(),
                value: "旧".to_owned(),
            },
            Op::PutKv {
                key: "drop".to_owned(),
                value: "旧".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut seed);

        // 先删后写：键必须在（写是后发生的）。
        let mut batch = vec![
            Op::DeleteKv {
                key: "keep".to_owned(),
            },
            Op::PutKv {
                key: "keep".to_owned(),
                value: "新".to_owned(),
            },
            Op::DeleteKv {
                key: "drop".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut batch);

        assert_eq!(
            kv_row(&connection, "keep").as_deref(),
            Some("新"),
            "先删后写＝写进去"
        );
        assert!(
            kv_row(&connection, "drop").is_none(),
            "先写后删（这里是只有一次删除）＝删掉"
        );

        // 反过来：先写后删，键必须消失。
        let mut batch = vec![
            Op::PutKv {
                key: "keep".to_owned(),
                value: "又写了一次".to_owned(),
            },
            Op::DeleteKv {
                key: "keep".to_owned(),
            },
        ];
        super::apply(&mut connection, &mut batch);
        assert!(
            kv_row(&connection, "keep").is_none(),
            "先写后删＝删掉；删除不能被简化成「本批先统一删一遍」"
        );
    }

    /// 覆盖式导入会盖掉此前所有的删除（整表都没了，待删的键自然不存在）。
    #[test]
    fn replace_kv_supersedes_a_pending_delete() {
        let mut connection = crate::db::open(&temp_db("replace-beats-delete")).expect("打开库");
        let mut batch = vec![
            Op::DeleteKv {
                key: "k".to_owned(),
            },
            Op::ReplaceKv {
                pairs: vec![("k".to_owned(), "新".to_owned())],
            },
        ];
        super::apply(&mut connection, &mut batch);
        assert_eq!(
            kv_row(&connection, "k").as_deref(),
            Some("新"),
            "导入是后发生的，不能被之前的删除吃掉"
        );
    }

    /// `kv_all` 是全表快照，且顺序稳定（导出结果可比对）。
    #[test]
    fn kv_all_returns_a_sorted_snapshot() {
        let path = temp_db("kv-all");
        let store = Store::open(path.clone()).expect("打开库");
        store.kv_put("z", "1");
        store.kv_put("a", "2");
        store.kv_put("m", "3");
        store.flush();

        let all = store.kv_all();
        assert_eq!(
            all,
            vec![
                ("a".to_owned(), "2".to_owned()),
                ("m".to_owned(), "3".to_owned()),
                ("z".to_owned(), "1".to_owned()),
            ]
        );

        store.kv_replace_all(vec![("only".to_owned(), "one".to_owned())]);
        store.clear(&[Table::Recent]);
        store.flush();
        assert_eq!(
            store.kv_all(),
            vec![("only".to_owned(), "one".to_owned())],
            "覆盖式导入后全表只剩包内的键"
        );

        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
