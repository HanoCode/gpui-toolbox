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

use crate::audit::AuditEntry;
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
    /// 审计日志（流水；清空是用户显式动作，见 [`Op::PushAudit`]）。
    Audit,
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
            Self::Audit => "audit",
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
    /// 追加一条审计记录，并把表裁到最多 `keep` 条。
    ///
    /// `keep` 跟着每条记录一起走而不是单独一条 op：**裁剪与写入是同一件事的两半**
    /// （「记下这条，并保证表不会无限长」）。分成两个 op 的话，只发写入不发裁剪就会
    /// 悄悄把库撑大，而那种遗漏在测试里很难被抓到。
    ///
    /// 审计是**只增**的：这里只 `INSERT`（外加同事务的裁剪 `DELETE`），
    /// 修改由数据库的 `audit_no_update` 触发器直接拒绝。
    PushAudit { entry: AuditEntry, keep: usize },
    /// 只裁剪、不写入：把表裁到最多 `keep` 条。
    ///
    /// 与 [`Op::PushAudit`] 的分工：那条是「记一条，**顺带**保证表不会无限长」——
    /// 写入必然带裁剪，所以表永远长不起来。这条是「把上限**调小**之后让新上限当场生效」，
    /// 它没有新记录可写。分开是必要的而不是对称强迫症：只有它存在，才可能出现
    /// 「裁剪没有任何写入陪着」的情况；而没有这条，用户把 2000 改成 200 之后
    /// 列表里还是那 2000 行，得等下一次写入才缩 —— 看起来就是「改了没反应」。
    TrimAudit { keep: usize },
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
    // 审计记录：**不去重**，每一条都是一次真实发生的事，顺序即到达顺序。
    let mut audit: Vec<AuditEntry> = Vec::new();
    let mut audit_keep: usize = 0;
    // 单独的裁剪请求（上限调小）。与 `audit_keep` 取最大：谁要求留得多听谁的 ——
    // 「一次裁剪」不该把同批里刚记下的记录按更小的上限砍掉。
    let mut audit_trim: usize = 0;
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
            Op::PushAudit { entry, keep } => {
                audit.push(entry);
                audit_keep = audit_keep.max(keep);
            }
            Op::TrimAudit { keep } => {
                audit_trim = audit_trim.max(keep);
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
        // 审计：**只 INSERT**（append-only，修改被数据库触发器拒绝），同一批里一行都不合并
        // —— 每一条都是一次真实发生的事。裁剪紧随其后、同事务完成，保证「写完那一刻起
        // 就不超过上限」；否则一次长跑会话能把库撑到用户看不见的地方去。
        for entry in &audit {
            tx.execute(
                "INSERT INTO audit(at, kind, action, outcome, scope, detail, fingerprint, trace_id,
                     duration_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    now,
                    entry.kind,
                    entry.action,
                    entry.outcome,
                    entry.scope,
                    entry.detail,
                    entry.fingerprint,
                    entry.trace_id,
                    entry.duration_ms
                ],
            )?;
        }
        // 裁剪跑在这批 INSERT 之后：`audit_limit` 是「写记录带来的上限」与「单独裁剪请求」
        // 里更宽松的那个 —— 上限调小是用户意图，但不该把同批里刚记下的东西按更小的数砍掉。
        let audit_limit = audit_keep.max(audit_trim);
        if !audit.is_empty() || audit_trim > 0 {
            // 保留最近 `keep` 条：按 id 倒序取，而不是 `MAX(id) - keep` —— AUTOINCREMENT
            // 的 id 之间有空洞（回滚过的插入也占号），减法会留下比预期多的行。
            // `keep` 下限夹到 1：0 会把刚写进去的这条也删掉，那是「写了个寂寞」。
            tx.execute(
                "DELETE FROM audit WHERE id NOT IN (SELECT id FROM audit ORDER BY id DESC LIMIT ?1)",
                rusqlite::params![audit_limit.max(1) as i64],
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

    /// 审计：只增、按条件查、超出上限裁掉**最旧**的、清空留空表。
    #[test]
    fn audit_appends_queries_and_trims() {
        use crate::audit::{AuditEntry, AuditFilter, AuditOutcome};

        let path = temp_db("audit");
        let store = Store::open(path.clone()).expect("打开库");
        let entry = |n: usize, outcome: AuditOutcome| {
            AuditEntry::new("query", "execute", outcome, "conn-1", format!("SELECT {n}"))
        };
        // keep = 3：同一批里写 5 条，只该留最后 3 条。
        for n in 0..5 {
            store.audit_push(entry(n, AuditOutcome::Ok), 3);
        }
        store.flush();
        assert_eq!(store.audit_count(), 3, "超出上限的旧记录必须被裁掉");
        let rows = store.audit_query(&AuditFilter::default());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].detail, "SELECT 4", "最新的在最前");
        assert_eq!(
            rows[2].detail, "SELECT 2",
            "留下的是**最近**三条，不是最早三条"
        );

        // 只看问题：成功的那三条不该出现在结果里。
        store.audit_push(entry(9, AuditOutcome::Failed), 100);
        store.flush();
        let problems = store.audit_query(&AuditFilter {
            problems_only: true,
            ..AuditFilter::default()
        });
        assert_eq!(problems.len(), 1);
        assert!(problems[0].is_problem());

        // 按 trace 过滤（点错误徽标进来的那条路）。
        store.audit_push(
            AuditEntry::new("query", "execute", AuditOutcome::Failed, "conn-2", "boom")
                .with_trace("t-1"),
            100,
        );
        store.flush();
        let traced = store.audit_query(&AuditFilter {
            trace_id: Some("t-1".to_owned()),
            ..AuditFilter::default()
        });
        assert_eq!(traced.len(), 1);
        assert_eq!(traced[0].scope, "conn-2");

        // 按类别过滤：认得出的是「这一类有几条」，认不出的类别是空而不是全部。
        let by_kind = |kind: &str| {
            store.audit_query(&AuditFilter {
                kind: Some(kind.to_owned()),
                ..AuditFilter::default()
            })
        };
        assert_eq!(by_kind("query").len(), 5);
        assert_eq!(by_kind("connection").len(), 0);

        // 清空是**用户显式动作**，与自动裁剪是仅有的两条删除路径。
        store.audit_clear();
        store.flush();
        assert_eq!(store.audit_count(), 0);

        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// 审计分两批写、批间 flush，裁剪按**累积**结果生效（不是每批各留 keep 条）。
    #[test]
    fn audit_trim_looks_at_the_whole_table() {
        use crate::audit::{AuditEntry, AuditFilter, AuditOutcome};

        let path = temp_db("audit-trim-twice");
        let store = Store::open(path.clone()).expect("打开库");
        for n in 0..4 {
            store.audit_push(
                AuditEntry::new(
                    "write",
                    "commit",
                    AuditOutcome::Ok,
                    "conn-1",
                    format!("#{n}"),
                ),
                2,
            );
            store.flush();
        }
        assert_eq!(store.audit_count(), 2, "每批都裁一次，最终仍只剩 2 条");
        let rows = store.audit_query(&AuditFilter::default());
        assert_eq!(rows[0].detail, "#3");
        assert_eq!(rows[1].detail, "#2");

        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// 只裁剪：**没有新记录**也能把上限调小，且同批里刚记下的那条不会被误砍。
    #[test]
    fn trim_alone_shrinks_the_table_without_writing() {
        use crate::audit::{AuditEntry, AuditFilter, AuditOutcome};

        let path = temp_db("audit-trim-alone");
        let store = Store::open(path.clone()).expect("打开库");
        for n in 0..5 {
            store.audit_push(
                AuditEntry::new(
                    "query",
                    "execute",
                    AuditOutcome::Ok,
                    "conn-1",
                    format!("#{n}"),
                ),
                100,
            );
            store.flush();
        }
        assert_eq!(store.audit_count(), 5);

        // 上限调小 → 当场缩表（不靠下一次写入顺手裁）。
        store.audit_trim(3);
        store.flush();
        assert_eq!(store.audit_count(), 3);
        let rows = store.audit_query(&AuditFilter::default());
        assert_eq!(rows[0].detail, "#4", "留下的是最近三条");
        assert_eq!(rows[2].detail, "#2");

        // 同批里「写一条 + 裁剪到 2」：取更宽松的那个上限，刚记下的那条必须在。
        store.audit_push(
            AuditEntry::new("query", "execute", AuditOutcome::Ok, "conn-1", "#new"),
            4,
        );
        store.audit_trim(2);
        store.flush();
        let rows = store.audit_query(&AuditFilter::default());
        assert_eq!(rows.len(), 4, "同批里写入带来的上限更宽松时听它（4 > 2）");
        assert_eq!(rows[0].detail, "#new");

        drop(store);
        let _ = std::fs::remove_file(&path);
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
