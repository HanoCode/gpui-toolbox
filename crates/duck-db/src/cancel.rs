//! 查询取消与超时：**带外**取消通道 + 超时守护线程。
//!
//! 为什么是「带外」：本 crate 的连接是阻塞的，调用方把它包在 `Mutex<Connection>` 里
//! 丢给后台线程（`docs/database-design.md` §4.1）。查询跑着的时候锁在别人手里，
//! 所以取消**不能**再走那条锁——[`CancelHandle`] 是建连时抓下来的、与连接锁无关的
//! 句柄，握在 UI 侧，任何时刻都能把「停」送进引擎：
//!
//! | 引擎 | 原语 | 说明 |
//! | --- | --- | --- |
//! | SQLite | `sqlite3_interrupt` | 只打断本连接当前语句；空闲时是空操作 |
//! | PostgreSQL | `CancelRequest`（libpq 同款） | `postgres` 的 `CancelToken` 自带 |
//! | MySQL | `KILL QUERY <id>` | 协议要求**另一条连接**发；句柄带 Opts + 连接 id |
//! | ClickHouse | `KILL QUERY WHERE query_id = '…'` | HTTP 无会话，必须按 `query_id` 点名 |
//! | SQL Server | `KILL <spid>` | 同样要另开连接（还得自带一个小 runtime） |
//! | MongoDB | `killOp` | `opid` 只能从 `$currentOp` 里捞，故按 `appName` 点名 |
//! | Elasticsearch | `POST /_tasks/<id>/_cancel` | `id` 只能从 `_tasks` 里按 `X-Opaque-Id` 捞 |
//! | agent（子进程） | JSON-RPC `cancel_session` | 取消是**另一条请求**，靠连接 id 点名；故意不等回应 |
//!
//! 超时用 [`CancelTimer`]：一个守护线程到点调 `cancel()`。它在 `disarm()`/`drop`
//! 时立刻撤销——否则「查询已结束、计时器才到点」会把取消误伤到下一条语句。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::error::DbError;

/// 轮询 `stop` 标志的粒度：够细（最长多等 50ms 才撤销）又不空转。
const TICK: Duration = Duration::from_millis(50);

/// 一条连接的带外取消句柄（可克隆，克隆体指向同一引擎通道）。
#[derive(Clone)]
pub struct CancelHandle(Arc<CancelKind>);

/// 各引擎的取消实现。
enum CancelKind {
    /// SQLite：`sqlite3_interrupt`。
    Sqlite(rusqlite::InterruptHandle),
    /// PostgreSQL：服务器侧取消请求。
    Postgres(postgres::CancelToken),
    /// MySQL：`KILL QUERY` 要另开连接，故把建连参数与连接 id 一起带上。
    Mysql {
        /// 建连参数（含口令——与连接本体同处内存，不落盘、不进日志）。
        opts: Box<mysql::Opts>,
        /// 目标连接的 `connection_id()`。
        connection_id: u32,
    },
    /// ClickHouse：`KILL QUERY WHERE query_id = '…'` 要另发一条 HTTP 请求。
    ///
    /// HTTP 请求不像 TCP 会话那样能靠断开连接表达「停」——服务端那边还在跑，
    /// 必须按 `query_id` 显式点名。
    ClickHouse {
        /// 端点（含认证与 agent）。
        endpoint: crate::drivers::clickhouse::Endpoint,
        /// 正在跑的语句的 `query_id`；空闲时是 `None`（那时取消是空操作）。
        inflight: Arc<Mutex<Option<String>>>,
    },
    /// SQL Server：`KILL <spid>`，同样要另开一条连接（还得自带一个小 runtime——
    /// 取消是在**别的线程**上送的，不能蹭主连接那条 runtime）。
    Mssql {
        /// 建连参数（含口令）与会话 id。
        target: crate::drivers::mssql::KillTarget,
    },
    /// MongoDB：`killOp` 要点名 `opid`，而 `opid` 只能从 `$currentOp` 按 `appName`
    /// 捞出来——所以同样得另开一条客户端（也自带 runtime）。
    Mongo {
        /// 建连 URI（含口令）与本条连接独有的 `appName`。
        target: crate::drivers::mongo::MongoCancelTarget,
    },
    /// Elasticsearch：`_cancel` 要点名 `nodeId:taskId`，而那个 id 只能从 `_tasks` 里
    /// 按 `X-Opaque-Id` 捞——所以同样得另发两条 HTTP 请求（列任务 + 取消）。
    Elasticsearch {
        /// 端点（含认证与 agent）。
        endpoint: crate::drivers::elasticsearch::Endpoint,
        /// 正在跑的请求的 `X-Opaque-Id`；空闲时是 `None`（那时取消是空操作）。
        inflight: Arc<Mutex<Option<String>>>,
    },
    /// agent 轨（子进程）：给驱动进程发一条 JSON-RPC `cancel_session`。
    ///
    /// 这里跟别处不一样的地方是**不需要另开通道** —— 驱动进程本身就是个常驻的
    /// JSON-RPC 服务端，[`AgentHandle`] 的方法都只取 `&self`、内部走写队列，
    /// 所以主线程正阻塞在等某条语句的回应上时，取消照样能插进去。
    ///
    /// 之所以还值得做成带外句柄：取消除非连接锁在手，否则走的是同一条被阻塞的路径。
    Agent {
        /// 驱动进程句柄（与连接本体共享同一个 `Arc`）。
        handle: Arc<duck_db_agent::AgentHandle>,
        /// 目标会话 id —— `agentSessionId`，即建连时开的那个会话。
        session_id: String,
    },
    /// DuckDB sidecar：给 worker 发一条 `cancel` —— **不用点名**。
    ///
    /// 与 [`CancelKind::Agent`] 的差别来自协议：agent 轨是多会话的（取消要带
    /// `agentSessionId`），sidecar 只有一条连接、一个正在跑的查询，worker 自己知道
    /// 该打断谁。它的 `execute` 也是异步回报的（先 spawn 再读下一行），所以跑着查询时
    /// 这条 `cancel` 真能插进去。
    DuckDb {
        /// worker 进程句柄（与连接本体共享同一个 `Arc`）。
        handle: Arc<duck_db_agent::AgentHandle>,
    },
    /// 什么都取消不了（连接没有可用句柄时给的占位，如 DuckDB worker 中毒待重起）。
    ///
    /// 留着它而不是让 `cancel_handle()` 返回 `Option`：取消的语义本来就是
    /// 「尽力而为」，一个空转的句柄比一条 `Option` 少一层调用点分支。
    Noop,
}

impl CancelHandle {
    /// SQLite 句柄（`Connection::get_interrupt_handle()`）。
    pub fn sqlite(handle: rusqlite::InterruptHandle) -> Self {
        Self(Arc::new(CancelKind::Sqlite(handle)))
    }

    /// PostgreSQL 句柄（`Client::cancel_token()`）。
    pub fn postgres(token: postgres::CancelToken) -> Self {
        Self(Arc::new(CancelKind::Postgres(token)))
    }

    /// MySQL 句柄（建连 Opts + `Conn::connection_id()`）。
    pub fn mysql(opts: mysql::Opts, connection_id: u32) -> Self {
        Self(Arc::new(CancelKind::Mysql {
            opts: Box::new(opts),
            connection_id,
        }))
    }

    /// ClickHouse 句柄（HTTP 端点 + 当前 `query_id` 的登记位）。
    pub(crate) fn clickhouse(
        endpoint: crate::drivers::clickhouse::Endpoint,
        inflight: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self(Arc::new(CancelKind::ClickHouse { endpoint, inflight }))
    }

    /// SQL Server 句柄（建连参数 + `@@SPID`）。
    pub(crate) fn mssql(target: crate::drivers::mssql::KillTarget) -> Self {
        Self(Arc::new(CancelKind::Mssql { target }))
    }

    /// MongoDB 句柄（建连 URI + `appName`）。
    pub(crate) fn mongo(target: crate::drivers::mongo::MongoCancelTarget) -> Self {
        Self(Arc::new(CancelKind::Mongo { target }))
    }

    /// Elasticsearch 句柄（HTTP 端点 + 当前 `X-Opaque-Id` 的登记位）。
    pub(crate) fn elasticsearch(
        endpoint: crate::drivers::elasticsearch::Endpoint,
        inflight: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self(Arc::new(CancelKind::Elasticsearch { endpoint, inflight }))
    }

    /// agent 轨句柄（驱动进程 + `agentSessionId`）。
    ///
    /// 精确点说，`session_id` 是**连接级会话**，不是游标那个 `sessionId` ——
    /// 两个键在协议里长得像但语义不同，混用的表现是「翻页永远失败」。
    pub(crate) fn agent(handle: Arc<duck_db_agent::AgentHandle>, session_id: String) -> Self {
        Self(Arc::new(CancelKind::Agent { handle, session_id }))
    }

    /// DuckDB sidecar 句柄（worker 进程；`cancel` 不用点名）。
    pub(crate) fn duckdb(handle: Arc<duck_db_agent::AgentHandle>) -> Self {
        Self(Arc::new(CancelKind::DuckDb { handle }))
    }

    /// 空转句柄（没有可用通道时用，见 [`CancelKind::Noop`]）。
    pub(crate) fn none() -> Self {
        Self(Arc::new(CancelKind::Noop))
    }

    /// 送一次取消。**幂等**：查询已结束时多数引擎只是空操作，不算失败。
    pub fn cancel(&self) -> Result<(), DbError> {
        match self.0.as_ref() {
            CancelKind::Sqlite(handle) => {
                handle.interrupt();
                Ok(())
            }
            CancelKind::Postgres(token) => token
                .cancel_query(postgres::NoTls)
                .map_err(|err| DbError::Query(format!("cancel request failed: {err}"))),
            CancelKind::Mysql {
                opts,
                connection_id,
            } => {
                use mysql::prelude::Queryable as _;
                // 短连接：发完 KILL 即断（`Conn` 随作用域 drop）。
                let mut killer = mysql::Conn::new(opts.as_ref().clone())
                    .map_err(|err| DbError::Query(format!("cancel connect failed: {err}")))?;
                killer
                    .query_drop(format!("KILL QUERY {connection_id}"))
                    .map_err(|err| DbError::Query(format!("kill query failed: {err}")))?;
                Ok(())
            }
            CancelKind::ClickHouse { endpoint, inflight } => {
                let query_id = inflight
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .filter(|id| !id.is_empty());
                // 空闲时（没有语句在跑）取消是空操作——与 SQLite 的 interrupt 同语义。
                let Some(query_id) = query_id else {
                    return Ok(());
                };
                // `query_id` 由本 crate 生成（`duck-<pid>-<seq>`），仍然按字面量转义：
                // 「这个值一定安全」不该成为 SQL 文本拼装的借口。
                let escaped = query_id.replace('\'', "\\'");
                endpoint
                    .post(
                        &format!("KILL QUERY WHERE query_id = '{escaped}'"),
                        None,
                        false,
                        &[],
                    )
                    .map_err(|err| DbError::Query(format!("kill query failed: {err}")))?;
                Ok(())
            }
            CancelKind::Mssql { target } => target.kill(),
            CancelKind::Mongo { target } => target.kill(),
            CancelKind::Elasticsearch { endpoint, inflight } => {
                let opaque = inflight
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .filter(|id| !id.is_empty());
                // 空闲时（没有请求在跑）取消是空操作——与 ClickHouse 的 `query_id` 同语义。
                let Some(opaque) = opaque else {
                    return Ok(());
                };
                // ① 列任务、按 `X-Opaque-Id` 认出自己那条；认不到说明已经跑完了。
                let tasks = endpoint
                    .request("GET", "/_tasks", None, None)
                    .map_err(|err| DbError::Query(format!("list tasks failed: {err}")))?
                    .json()
                    .map_err(|err| DbError::Query(format!("list tasks failed: {err}")))?;
                let Some(task) =
                    crate::drivers::elasticsearch::find_task_by_opaque(&tasks, &opaque)
                else {
                    return Ok(());
                };
                // ② 点名取消。`nodeId:taskId` 由上面那条响应给出，仍走一次 URL 编码：
                // 「这个值一定安全」不该成为不编码的借口。
                let path = format!("/_tasks/{}/_cancel", crate::drivers::url_encode(&task));
                endpoint
                    .request("POST", &path, None, None)
                    .map_err(|err| DbError::Query(format!("cancel task failed: {err}")))?;
                Ok(())
            }
            CancelKind::Agent { handle, session_id } => {
                // 「送不出去」才算失败；送到了不等于取消成功 —— 原请求仍要自己失败或超时
                // 来收场（对端对取消只回一条确认，而那条我们故意不等）。
                handle
                    .cancel_session(session_id)
                    .map_err(|err| DbError::Query(format!("cancel session failed: {err}")))
            }
            CancelKind::DuckDb { handle } => {
                // 同一条纪律：只报「送不出去」。worker 的 `cancel` 打断的是**当前**
                // 那条查询（空闲时是空操作）。
                handle
                    .fire_method(crate::drivers::duckdb::CANCEL, serde_json::json!({}))
                    .map_err(|err| DbError::Query(format!("cancel failed: {err}")))
            }
            CancelKind::Noop => Ok(()),
        }
    }

    /// 引擎名（日志用）。
    #[must_use]
    pub fn engine(&self) -> &'static str {
        match self.0.as_ref() {
            CancelKind::Sqlite(_) => "sqlite",
            CancelKind::Postgres(_) => "postgres",
            CancelKind::Mysql { .. } => "mysql",
            CancelKind::ClickHouse { .. } => "clickhouse",
            CancelKind::Mssql { .. } => "sqlserver",
            CancelKind::Mongo { .. } => "mongo",
            CancelKind::Elasticsearch { .. } => "elasticsearch",
            CancelKind::Agent { .. } => "agent",
            CancelKind::DuckDb { .. } => "duckdb",
            CancelKind::Noop => "none",
        }
    }
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CancelHandle").field(&self.engine()).finish()
    }
}

/// 超时守护：`timeout` 到点调一次 `cancel()`。
///
/// `disarm()`（或 `Drop`）后线程立刻退出，**不会再触发**——查询正常跑完时必须撤销，
/// 否则计时器晚到的那一枪会打在下一条语句上。
pub struct CancelTimer {
    /// 撤销标志（`true` = 线程尽快退出且不触发）。
    stop: Arc<AtomicBool>,
    /// 线程句柄；`disarm()` 时 join，`Drop` 时只置标志（不阻塞析构）。
    handle: Option<JoinHandle<()>>,
}

impl CancelTimer {
    /// 为 `cancel` 挂一个 `timeout` 的守护。
    #[must_use]
    pub fn arm(cancel: &CancelHandle, timeout: Duration) -> Self {
        let cancel = cancel.clone();
        Self::arm_with(timeout, move || {
            if let Err(err) = cancel.cancel() {
                tracing::warn!(error = %err, "query cancel on timeout failed");
            }
        })
    }

    /// 实际实现（`fire` 可在单测里换成可观测的探针）。
    fn arm_with(timeout: Duration, fire: impl FnOnce() + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("duck-db-cancel-timer".into())
            .spawn(move || {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(TICK);
                }
                // 到点前被撤销就闭嘴；撤销后不触发是这条链路的正确性关键。
                if !flag.load(Ordering::Relaxed) {
                    fire();
                }
            })
            .expect("spawn cancel timer thread");
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// 撤销守护并等线程收工（查询正常结束路径）。
    pub fn disarm(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CancelTimer {
    fn drop(&mut self) {
        // 析构里不 join：只置标志，线程自己退出（避免在持有其他锁时阻塞）。
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for CancelTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelTimer")
            .field("armed", &self.handle.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 到点触发：把探针换成一个计数器（不需要真实连接）。
    #[test]
    fn timer_fires_on_deadline() {
        let hits = Arc::new(AtomicUsize::new(0));
        let probe = hits.clone();
        let _timer = CancelTimer::arm_with(Duration::from_millis(30), move || {
            probe.fetch_add(1, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "守护线程应触发一次");
    }

    /// 撤销后不触发：查询结束了，计时器晚到的一枪必须哑掉。
    #[test]
    fn disarmed_timer_does_not_fire() {
        let hits = Arc::new(AtomicUsize::new(0));
        let probe = hits.clone();
        let mut timer = CancelTimer::arm_with(Duration::from_millis(80), move || {
            probe.fetch_add(1, Ordering::SeqCst);
        });
        timer.disarm();
        std::thread::sleep(Duration::from_millis(220));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "撤销后不应触发");
    }

    /// SQLite：真实长查询被 `interrupt()` 打断，且错误归一成 [`DbError::Cancelled`]。
    #[test]
    fn sqlite_interrupt_stops_running_query() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        let handle = CancelHandle::sqlite(conn.get_interrupt_handle());
        assert_eq!(handle.engine(), "sqlite");

        let worker = std::thread::spawn(move || {
            // 无限递归 CTE：不打断就永远跑下去。
            conn.query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                 SELECT count(*) FROM c",
                [],
                |row| row.get::<_, i64>(0),
            )
        });

        std::thread::sleep(Duration::from_millis(150));
        handle.cancel().expect("interrupt 送达");

        let err = worker
            .join()
            .expect("worker 未 panic")
            .expect_err("长查询应被中断");
        assert!(
            matches!(DbError::from(err), DbError::Cancelled(_)),
            "中断错误应归一成 Cancelled"
        );
    }
}
