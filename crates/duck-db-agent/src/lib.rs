//! 驱动子进程运行时：**JSON-RPC 2.0 over stdio（换行分帧）**。
//!
//! # 为什么需要它
//!
//! 达梦 / 金仓 / Oracle / DB2 / Hive 这些库没有（也不会有）Rust 原生驱动，公共做法是
//! **跑一个独立进程**替我们去连库：进程里是官方 JDBC 或 Go 驱动，宿主只跟它说话。
//! 本 crate 就是「宿主怎么说这话」：起进程、发请求、按 `id` 把回应对上号、超时就报超时。
//!
//! 协议与 dbx（Apache-2.0）的 agent 完全一致 —— 换来的直接好处是**现成的驱动工件能直接用**：
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"open_session","params":{…,"agentSessionId":"s1"}}
//! ← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}
//! ```
//!
//! # 四条硬规矩
//!
//! 1. **一条 stdout 行 = 一个 JSON 文档**。响应可以**乱序**回来（不同会话并发跑），
//!    所以必须按 `id` 对账，不能假设「下一个回来的是我刚才发的那个」。
//! 2. **不向调用方暴露任何运行时类型**：本 crate 全阻塞。调用方在 gpui 的
//!    `background_executor` 上跑它，与 `duck-db` / `duck-mail` 同一条口径。
//! 3. **进程死了要能说清**：stdout EOF / 写管道断，都要把**所有在飞的请求**立刻唤醒
//!    并报错，绝不能让调用方等到超时 —— 那是「明明崩了却让你等 30 秒」的来源。
//! 4. **写永不阻塞**：请求丢进队列由独立线程落盘（写管道），调用方只管等回应。
//!    直接写 stdin 的写法在一个「收到了但不读」的对端上会卡死 —— 而且卡在
//!    **超时开始之前**，于是「设了 60 秒超时」和「无限期挂住」没有区别。
//!
//! # 每条请求都会**有回应**
//!
//! 这个协议里没有真正的通知：`close_session`、`cancel_session` 对端也会回
//! `{"ok":true}`（它们同样带 `id`）。所以本 crate 的默认姿势是「发出去、等回来」，
//! 只有取消是真等不起的 —— 见 [`AgentHandle::cancel_session`]。
//!
//! 另外，`error` 回包是**业务错误的正常通道**：SQL 写错了、口令不对、超时，
//! 都走 `error` 而不是断连接。所以 [`AgentError::Remote`] 要当成一等公民处理，
//! 别当成「通道坏了」。
//!
//! # 会话
//!
//! 一个进程可以服务多个逻辑会话（协议 v2）：每个连接用自己的 `agentSessionId`，
//! 请求里都带着它。本 crate 不强制会话数，只负责把 id 传过去 —— 谁开谁关由上层
//! （`duck-db` 的驱动）决定，因为「什么时候复用、什么时候新开」是连接语义。
//!
//! # 与 `duck-drivers` 的关系
//!
//! 本 crate **不认识**「驱动」这个概念，只认 [`AgentProcess`]（跑什么程序、什么参数、
//! 什么工作目录、什么环境变量）。`duck-db` 拿 `duck-drivers` 的 `LaunchSpec` 拼一个
//! `AgentProcess` 再交给这里 —— 于是本层也能给将来的 process 插件复用。

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub mod protocol;

pub use protocol::{
    ColumnInfo, ConnectRequest, ConnectionInfo, DatabaseInfo, ForeignKeyInfo, HandshakeInfo,
    IndexInfo, ObjectInfo, QueryOutcome, TableInfo, TriggerInfo,
};

/// 默认请求超时（连接与会话类请求之外的那些）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 起进程的超时（连库本身可能慢，给足）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 关会话 / 取消的宽限：它们不该慢，慢了就是坏了。
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// `shutdown` 的宽限。
///
/// 比 [`CLOSE_TIMEOUT`] 还短：对端的 `shutdown` 是「回一句 `{"ok":true}` 就退出循环」，
/// 微秒级的事。这里等久了不是「宽容」，是把收尾时间白送给一个已经半死的进程 ——
/// 而收尾发生在**关闭窗口/退出应用**这条路上，用户能感觉到。
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// 要跑起来的驱动进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcess {
    /// 程序（native 载荷本身，或 `java`）。
    pub program: PathBuf,
    /// 参数。
    pub args: Vec<String>,
    /// 工作目录。
    pub working_dir: Option<PathBuf>,
    /// 额外环境变量（键值对）。
    pub env: Vec<(String, String)>,
}

/// 线上方言：同一个「子进程 + 每行一条 JSON」的壳，两套封套。
///
/// 两套都是**行分隔 JSON over stdio**，差别只在信封与错误形状：
///
/// | | 请求 | 回应 | 错误 |
/// | --- | --- | --- | --- |
/// | [`JsonRpc`](Self::JsonRpc) | `{"jsonrpc":"2.0","id":1,…}` | `{"jsonrpc":"2.0","id":1,"result":…}` | `{"error":{"code":-32000,"message":…}}` |
/// | [`DbxSidecar`](Self::DbxSidecar) | `{"id":"1","method":…,"params":{…}}` | `{"id":"1","ok":true,"result":…}` | `{"id":"1","ok":false,"error":{"code":"snake_case","message":…}}` |
///
/// ⚠️ sidecar 的 `id` 是**字符串**（对端结构体里就是 `String`，送数字它会**解析失败**
/// 并回一条 `id` 为空串的 `invalid_json`）；本层因此把数字 id 转成十进制字符串送出去，
/// 收回来再解析回数字对账。
///
/// 这一档存在的原因是 **DuckDB 的官方 sidecar 工件**（`dbx-agent-duckdb-*`）说的是
/// 这一套：它没有一个字改得动，只能这边适配（见 `duck-db` 的 `drivers::duckdb`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WireDialect {
    /// 本仓自己的 JSON-RPC 2.0（44 条 JDBC/native agent 都是它）。
    #[default]
    JsonRpc,
    /// dbx 的 sidecar 封套（目前只有 DuckDB worker）。
    DbxSidecar,
}

/// 失败种类。
#[derive(Debug)]
pub enum AgentError {
    /// 起进程失败 / 管道断了 / 写不进去。
    Transport(String),
    /// 等超时了（请求已经发出，结果未知）。
    Timeout {
        /// 超时的那个方法。
        method: String,
        /// 等了多久。
        waited: Duration,
    },
    /// 对端回了 `error`。
    Remote {
        /// JSON-RPC 错误码。
        code: i64,
        /// 错误消息（**内核原文，英文**——它来自驱动进程，界面按种类自己翻）。
        message: String,
        /// 结构化恢复数据（协议 v2 的 `data`，有就原样带上）。
        data: Option<Value>,
    },
    /// 回应的形状不是 JSON-RPC 2.0 该有的样子。
    Protocol(String),
    /// 进程已经不可用了（EOF / 已关）。
    Closed,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "driver transport: {message}"),
            Self::Timeout { method, waited } => {
                write!(f, "{method} did not answer within {waited:?}")
            }
            Self::Remote { code, message, .. } => write!(f, "driver error {code}: {message}"),
            Self::Protocol(message) => write!(f, "driver protocol: {message}"),
            Self::Closed => write!(f, "the driver process is gone"),
        }
    }
}

impl std::error::Error for AgentError {}

/// 结果别名。
pub type Result<T> = std::result::Result<T, AgentError>;

/// **在飞的请求**：请求 `id` → 唤醒它的通道。
///
/// 值类型是 `Result` 而不是「成功时的 JSON」：失败（对端报错 / 进程没了 / 写不出去）
/// 也要能**从读线程和写线程**送回来，否则等待方只能靠超时。
type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value>>>>>;

/// 一条驱动进程的句柄。
///
/// `Drop` 会**尽力**收尾：先发 `shutdown`、给一点宽限，再杀进程。指望别人记得调
/// [`AgentHandle::shutdown`] 是不现实的 —— 忘了就是一个永远活着的 JVM。
///
/// # 线程模型
///
/// 一个子进程配两个线程（都不是调用方的线程）：
///
/// ```text
/// 调用方线程 ──call()──▶ 写队列 ──▶ 写线程 ──stdin──▶ 子进程
///     ▲                                                  │
///     └──────────── 按 id 对账 ──── 读线程 ◀──stdout──────┘
/// ```
///
/// 两边都是**队列 + 独立线程**，所以 `call` 里没有任何「可能无限期阻塞」的 I/O：
/// 拿到超时就是超时，不会变成挂死。
pub struct AgentHandle {
    child: Mutex<Child>,
    /// 待写队列的投递端。`None` = 已经收尾（队列一关，写线程排空后退出并丢掉 stdin，
    /// 子进程随即看到 EOF）。
    ///
    /// 为什么不是 `Mutex<ChildStdin>` 直接写：见文件顶部硬规矩 4。
    writer: Mutex<Option<Sender<String>>>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    /// 说的哪套封套（见 [`WireDialect`]）。请求与回应的形状都由它决定。
    dialect: WireDialect,
    /// 读线程句柄（收尾时 join，保证不泄漏线程）。
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 写线程句柄。**必须在子进程死之后才 join** —— 见 [`AgentHandle::shutdown`]。
    writer_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHandle")
            .field("pid", &self.pid())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl AgentHandle {
    /// 起一个驱动进程（说 JSON-RPC 2.0，见 [`WireDialect::JsonRpc`]）。
    ///
    /// # Errors
    /// 起不来（程序不存在、权限不对）、拿不到 stdin/stdout。
    pub fn spawn(process: &AgentProcess) -> Result<Self> {
        Self::spawn_with(process, WireDialect::JsonRpc)
    }

    /// 起一个驱动进程，并指定它说的**封套**（见 [`WireDialect`]）。
    ///
    /// # Errors
    /// 同 [`AgentHandle::spawn`]。
    pub fn spawn_with(process: &AgentProcess, dialect: WireDialect) -> Result<Self> {
        let mut command = Command::new(&process.program);
        command
            .args(&process.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 继承：驱动进程的崩溃信息（JVM 的 `Unrecognized option`、栈回溯）
            // 直接进应用日志，比我们截下来再转发更可靠。
            .stderr(Stdio::inherit());
        if let Some(dir) = &process.working_dir {
            command.current_dir(dir);
        }
        for (key, value) in &process.env {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|error| {
            AgentError::Transport(format!(
                "cannot start {}: {error}",
                process.program.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Transport("the child has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Transport("the child has no stdout".to_owned()))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let reader = {
            let pending = Arc::clone(&pending);
            let closed = Arc::clone(&closed);
            std::thread::Builder::new()
                .name("duck-db-agent-reader".to_owned())
                .spawn(move || read_loop(stdout, pending, closed, dialect))
                .map_err(|error| {
                    AgentError::Transport(format!("cannot start the reader: {error}"))
                })?
        };

        // 队列**故意不设容量**：设了容量，`send` 就变成会阻塞的操作，
        // 硬规矩 4 立刻破功。它唯一会堆积的场合是对端不读 stdin —— 那时调用方
        // 正在一个个超时失败，堆积的活儿横竖没人要了。
        let (writer, lines) = std::sync::mpsc::channel::<String>();
        let writer_thread = {
            let pending = Arc::clone(&pending);
            let closed = Arc::clone(&closed);
            std::thread::Builder::new()
                .name("duck-db-agent-writer".to_owned())
                .spawn(move || write_loop(stdin, lines, pending, closed))
                .map_err(|error| {
                    AgentError::Transport(format!("cannot start the writer: {error}"))
                })?
        };

        Ok(Self {
            child: Mutex::new(child),
            writer: Mutex::new(Some(writer)),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            dialect,
            reader: Mutex::new(Some(reader)),
            writer_thread: Mutex::new(Some(writer_thread)),
        })
    }

    /// 子进程 pid（日志与排障用）。
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().map(|child| child.id())
    }

    /// 进程是否已经不可用（EOF / 关过）。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// 发一个请求并等回应。
    ///
    /// 请求 `id` 由本方法分配（从 1 开始，单调递增）—— **不要**自己拼 `id`：
    /// 对账全靠它。
    ///
    /// # Errors
    /// 见 [`AgentError`]。超时**不代表请求没执行**（可能只是没回），调用方要自行决定
    /// 是重试还是丢会话。所以「超时」这条路上**不要**随手重发写语句。
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = request_envelope(self.dialect, id, method, params);

        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| AgentError::Transport("the pending table is poisoned".to_owned()))?;
            pending.insert(id, sender);
        }

        if let Err(error) = self.enqueue(id, request) {
            self.forget(id);
            return Err(error);
        }

        match receiver.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                // 超时：把这条从待办里摘掉，免得晚到的回应去唤醒一个已经不等的人。
                self.forget(id);
                Err(AgentError::Timeout {
                    method: method.to_owned(),
                    waited: timeout,
                })
            }
        }
    }

    /// 握手：拿协议版本与能力（`multi_session` / `structured_error_v1` 等）。
    ///
    /// ⚠️ **只有 JSON-RPC 方言有握手**。sidecar（[`WireDialect::DbxSidecar`]）没有这一步：
    /// 它的第一条业务请求就是 `connect`，回应 `{"ok":true,"result":{"connected":true}}`
    /// 本身就是「连上了」—— 拿本方法去问它只会得到一条 `invalid_json`
    /// （对端不认识 `handshake` 这个 variant）。
    ///
    /// # Errors
    /// 进程不认 `handshake`、或回应形状不对。
    pub fn handshake(&self) -> Result<HandshakeInfo> {
        let value = self.call("handshake", json!({}), CONNECT_TIMEOUT)?;
        serde_json::from_value(value).map_err(|error| {
            AgentError::Protocol(format!("the handshake shape is unexpected: {error}"))
        })
    }

    /// 发一条**不等回应**的请求（`fire and forget`）。
    ///
    /// 给「取消」这类等不起的动作用（[`Self::cancel_session`] 是它在 JSON-RPC 方言上的
    /// 特例）。⚠️ 回应**仍然会到达**（协议里没有真通知），它会落到「未知 id」的 trace
    /// 日志上 —— 那不是 bug。
    ///
    /// # Errors
    /// 只报「送不出去」（进程已经没了 / 队列已关）。**送到不等于对方照做了**。
    pub fn fire_method(&self, method: &str, params: Value) -> Result<()> {
        self.fire(method, params)
    }

    /// 关一个逻辑会话，**等它确认**。
    ///
    /// 为什么值得等：对端的「关会话」不只是丢个引用，它会释放 JDBC 连接、
    /// 把连接还回池子；池子清理失败时**会回错**（对端把它标成 `resource` 类），
    /// 而那条错正是「这个进程该换了」的信号。不等就等于把这个信号丢掉。
    ///
    /// # Errors
    /// 对端报错、通道断了、或超过 [`CLOSE_TIMEOUT`]。
    pub fn close_session(&self, session_id: &str) -> Result<()> {
        self.call(
            protocol::method::CLOSE_SESSION,
            protocol::session_params(session_id),
            CLOSE_TIMEOUT,
        )
        .map(|_| ())
    }

    /// 取消某个会话正在跑的语句 / 游标，**不等回应**。    ///
    /// 这里和别的请求不一样，是有意的：取消的整个意义就是「别等了」。
    /// 等 5 秒确认一条「我已经不想要了」的语句被取消，等于把取消变慢。
    ///
    /// 请求**仍然带 `id`**（协议要求），只是我们不登记待办 —— 回应到达时会分流到
    /// 「未知 id」那条 trace 日志。这不是错误，别照它去查 bug。
    ///
    /// # Errors
    /// 只报「送不出去」（进程已经没了 / 队列已关）。**送到不等于取消成功**，
    /// 调用方仍然要靠原请求自己失败/超时来收场。
    pub fn cancel_session(&self, session_id: &str) -> Result<()> {
        self.fire(
            protocol::method::CANCEL_SESSION,
            protocol::session_params(session_id),
        )
    }

    /// 收尾：先请它自己退，宽限期内没退就杀。
    ///
    /// 幂等 —— 重复调用是安全的（`Drop` 也会调）。
    ///
    /// 顺序不是随手写的，四步各自解掉一个「收不了尾」的姿势：
    ///
    /// 1. 发 `shutdown`（带超时）—— 对端自觉退出，最干净的走法；
    /// 2. **关写队列** —— 写线程排空后自己结束，顺手把 stdin 的写端关掉 ⇒ 子进程看到
    ///    EOF。这一步是关键：少了它，一个卡在「读 stdin 但不应答」的对端会一直等；
    /// 3. 等进程退，宽限期内没退就 `kill`；
    /// 4. 这时才 join 两个线程。**join 必须在进程死透之后**：写线程可能正卡在写管道上
    ///    （对端不读），管道要等进程没了才会断，早 join 就是我们自己挂死在这儿。
    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // 第 1 步：请它自己退。进程可能已经半死，所以不等太久。
        let _ =
            self.call_without_closed_check(protocol::method::SHUTDOWN, json!({}), SHUTDOWN_TIMEOUT);
        self.wake_all("the driver process was shut down");

        // 第 2 步：关队列 ⇒ 写线程退出 ⇒ stdin 关闭 ⇒ 子进程收到 EOF。
        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }

        // 第 3 步：宽限期内退不了就杀。协议说对端会等最后一个会话关掉后再退，
        // 所以「已经请过、也不该慢了」的 2 秒对健康进程绰绰有余。
        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Err(_) => break,
                }
            }
        }

        // 第 4 步：收线程。**有界**，见 [`join_within`] —— 我们已经杀了直接子进程，
        // 但它可能留下了还攥着同一根管道的孙进程（JVM 起的辅助进程、`sh -c` 没 exec 的
        // 那条尾巴），而 `read`/`write` 要等**所有**写端都关了才会返回。
        if let Ok(mut handle) = self.writer_thread.lock()
            && let Some(handle) = handle.take()
        {
            join_within(handle, PIPE_THREAD_GRACE);
        }
        if let Ok(mut reader) = self.reader.lock()
            && let Some(handle) = reader.take()
        {
            join_within(handle, PIPE_THREAD_GRACE);
        }
    }

    // ── 内部 ──────────────────────────────────────────────────────────────

    /// `shutdown` 自己要用：此时 `closed` 已经被置位，但请求还得发得出去。
    fn call_without_closed_check(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = request_envelope(self.dialect, id, method, params);
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let Ok(mut pending) = self.pending.lock() else {
                return Err(AgentError::Transport(
                    "the pending table is poisoned".to_owned(),
                ));
            };
            pending.insert(id, sender);
        }
        if let Err(error) = self.enqueue(id, request) {
            self.forget(id);
            return Err(error);
        }
        match receiver.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                self.forget(id);
                Err(AgentError::Timeout {
                    method: method.to_owned(),
                    waited: timeout,
                })
            }
        }
    }

    /// 把一条请求丢进写队列（**不登记待办**：这是「发完就算」的路径）。
    fn fire(&self, method: &str, params: Value) -> Result<()> {
        if self.is_closed() {
            return Err(AgentError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = request_envelope(self.dialect, id, method, params);
        self.enqueue(id, request)
    }

    /// 编码并投递一条请求。
    ///
    /// `id` 只用来在报错日志里指认是哪条（投递失败时压缩成一句话就够了），
    /// 对账由调用方自己登记待办做。
    fn enqueue(&self, id: u64, request: Value) -> Result<()> {
        let line = serde_json::to_string(&request).map_err(|error| {
            AgentError::Protocol(format!("cannot encode request {id}: {error}"))
        })?;
        let writer = self
            .writer
            .lock()
            .map_err(|_| AgentError::Transport("the write queue is poisoned".to_owned()))?;
        let Some(writer) = writer.as_ref() else {
            return Err(AgentError::Closed);
        };
        writer.send(line).map_err(|_| AgentError::Closed)
    }

    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    fn wake_all(&self, message: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(AgentError::Transport(message.to_owned())));
            }
        }
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 收线程时最多等多久（见 [`join_within`]）。
const PIPE_THREAD_GRACE: Duration = Duration::from_millis(500);

/// 有界 join：等不到就**放弃**，把线程留下自己走。
///
/// 为什么不无限等：`read` / `write` 的返回条件是「管道**所有的**写端都关了」，
/// 而写端会被子进程的**孙进程**继承 —— 驱动进程自己已经退出了，某个它顺手起的辅助
/// 进程还攥着 stdout 没放。那时 `join()` 会一直挂到那个进程也退出为止，
/// 而收尾路径正是「关窗口 / 退出应用」，用户**看得见**这个卡顿。
///
/// 放弃的代价只是留下一个已经不会再被唤醒的线程（它在 EOF 到来时自己结束，
/// 届时待办表早已清空）。拿一个后台线程换收尾不卡，这笔账很清楚。
fn join_within(handle: std::thread::JoinHandle<()>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            tracing::debug!(
                "a driver pipe thread outlived the shutdown grace; leaving it to finish on its own"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.join();
}

/// 写线程：把队列里的每一行原样写进子进程的 stdin。
///
/// 两个收场条件都要能唤醒调用方：
///
/// - **队列关闭**（`shutdown` 关了投递端）→ 排空后退出。函数返回时 `stdin` 被丢掉，
///   子进程看到 EOF 就会自己退 —— 这是「对端卡在读 stdin 不应答」时唯一的出路。
/// - **写失败**（管道断了，对端已经死了）→ 置位 + 唤醒在飞的请求。
///   这条不能只记一条日志：失败之后**永远不会再有回应**，不唤醒就是让调用方干等到超时。
fn write_loop(
    stdin: ChildStdin,
    lines: Receiver<String>,
    pending: Pending,
    closed: Arc<AtomicBool>,
) {
    // 一条请求 = 一次 `write` 系统调用：`BufWriter` 把「正文 + 换行」攒成一次。
    // 每条都 flush —— 不 flush 就是「请求攒在用户态缓冲里，对端一直没收到」，
    // 表现为随机的超时。
    let mut stdin = std::io::BufWriter::new(stdin);
    for line in lines {
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        if let Err(error) = written {
            closed.store(true, Ordering::Release);
            if let Ok(mut pending) = pending.lock() {
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(AgentError::Transport(format!(
                        "cannot write to the driver process: {error}"
                    ))));
                }
            }
            return;
        }
    }
}

/// 读线程：把每一行解析成 JSON，按 `id` 唤醒在等的人。
fn read_loop(
    stdout: std::process::ChildStdout,
    pending: Pending,
    closed: Arc<AtomicBool>,
    dialect: WireDialect,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => route(&pending, &message, dialect),
            Err(error) => {
                // 驱动进程往 stdout 上喷了非 JSON 的东西（调试打印、崩溃输出）。
                // 这不该让整个通道失效——记一条日志继续读。
                tracing::warn!(%error, line = %line.chars().take(200).collect::<String>(), "non-JSON line on the driver stdout");
            }
        }
    }

    // EOF：进程没了。**立刻唤醒所有在飞的请求**，不让调用方干等到超时。
    closed.store(true, Ordering::Release);
    if let Ok(mut pending) = pending.lock() {
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(AgentError::Closed));
        }
    }
}

/// 按封套拼一条请求（见 [`WireDialect`]）。
///
/// ⚠️ **sidecar 的 `id` 必须是字符串**：对端的 `DuckDbWorkerRequest.id` 是 `String`，
/// 送数字进去它会 `invalid_json` 失败，而且那条错误的 `id` 是**空串** —— 回应对不上
/// 账，调用方只能等到超时（一个「协议没错、但永远等不到」的坑）。
fn request_envelope(dialect: WireDialect, id: u64, method: &str, params: Value) -> Value {
    match dialect {
        WireDialect::JsonRpc => {
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        }
        WireDialect::DbxSidecar => {
            json!({ "id": id.to_string(), "method": method, "params": params })
        }
    }
}

/// 把一条回应交给在等它的人。
fn route(pending: &Pending, message: &Value, dialect: WireDialect) {
    let Some(id) = response_id(message, dialect) else {
        // 没有 `id` / id 是空串：通知、心跳，或**对端连请求都没解析出来**
        // （sidecar 的 `invalid_json` 就是这一档，它的 `id` 是空串）。
        // 宿主管不了这类消息，记下来即可 —— 真正在等的人会自己超时。
        tracing::trace!(%message, "driver notification");
        return;
    };
    let Ok(mut pending) = pending.lock() else {
        return;
    };
    let Some(sender) = pending.remove(&id) else {
        tracing::debug!(id, "a driver response arrived for an unknown request id");
        return;
    };
    if let Some(error) = response_error(message, dialect) {
        let _ = sender.send(Err(error));
        return;
    }
    let _ = sender.send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
}

/// 从回应里取请求 `id`。
///
/// JSON-RPC 用数字；sidecar 用十进制字符串（见 [`WireDialect::DbxSidecar`]）。
/// 取不出来时返回 `None`（调用方按「通知」处理）。
fn response_id(message: &Value, dialect: WireDialect) -> Option<u64> {
    let raw = message.get("id")?;
    match dialect {
        WireDialect::JsonRpc => raw.as_u64(),
        WireDialect::DbxSidecar => raw
            .as_str()
            .and_then(|text| text.parse::<u64>().ok())
            .or_else(|| raw.as_u64()),
    }
}

/// 把回应里的错误翻成 [`AgentError`]（没有错误返回 `None`）。
///
/// 两套封套的错误形状不同：
///
/// - JSON-RPC：`{"error":{"code":-32000,"message":…,"data":…}}`，`code` 是**数字**；
/// - sidecar：`{"ok":false,"error":{"code":"duckdb_execute_failed","message":…}}`，
///   `code` 是 **snake_case 字符串**。
///
/// sidecar 的字符串码没有可放的数字字段，所以统一挂 [`AgentError::Remote`] 的
/// `code: 0` 并把原码放进 `data.code` —— 上层的 `Remote { message, .. }` 分支照样
/// 拿到人话，而**要按码分支的调用方**（比如 DuckDB 的 `duckdb_worker_poisoned`
/// 要求杀掉进程重起）从 `data` 里取原码。
fn response_error(message: &Value, dialect: WireDialect) -> Option<AgentError> {
    match dialect {
        WireDialect::JsonRpc => {
            let error = message.get("error")?;
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the driver reported an error")
                .to_owned();
            let data = error.get("data").cloned();
            Some(AgentError::Remote {
                code,
                message: text,
                data,
            })
        }
        WireDialect::DbxSidecar => {
            // `ok: true` 就不是错误 —— sidecar 的成功回应里没有 `error` 字段，
            // 但形状不对时（两边都没有）也该报出来，不能当成「成功且结果为空」。
            match message.get("ok").and_then(Value::as_bool) {
                Some(true) => None,
                _ => {
                    let error = message.get("error");
                    let code = error
                        .and_then(|error| error.get("code"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown_error")
                        .to_owned();
                    let text = error
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("the driver reported an error")
                        .to_owned();
                    Some(AgentError::Remote {
                        code: 0,
                        message: text,
                        data: Some(json!({ "code": code })),
                    })
                }
            }
        }
    }
}

/// sidecar 错误码（`response_error` 放进 `data.code` 的那个）—— 上层按它分支。
///
/// 取不到（不是 [`AgentError::Remote`]、或 JSON-RPC 那条路）时返回 `None`。
#[must_use]
pub fn sidecar_error_code(error: &AgentError) -> Option<&str> {
    match error {
        AgentError::Remote { data, .. } => data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回应按 `id` 对账，**且能乱序** —— 协议明说并发会话的回应可能先后颠倒。
    #[test]
    fn responses_are_matched_by_id_regardless_of_order() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender_two, receiver_two) = std::sync::mpsc::channel();
        let (sender_one, receiver_one) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(2, sender_two);
        pending.lock().expect("锁").insert(1, sender_one);

        // 二号先回。
        route(
            &pending,
            &json!({ "jsonrpc": "2.0", "id": 2, "result": { "which": 2 } }),
            WireDialect::JsonRpc,
        );
        route(
            &pending,
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "which": 1 } }),
            WireDialect::JsonRpc,
        );

        assert_eq!(
            receiver_two
                .recv_timeout(Duration::from_secs(1))
                .expect("二号收到")
                .expect("成功")
                .get("which"),
            Some(&json!(2))
        );
        assert_eq!(
            receiver_one
                .recv_timeout(Duration::from_secs(1))
                .expect("一号收到")
                .expect("成功")
                .get("which"),
            Some(&json!(1))
        );
    }

    /// `error` 回应要变成带码与消息的失败，而不是「成功但内容是空」。
    #[test]
    fn an_error_response_becomes_a_failure() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(7, sender);
        route(
            &pending,
            &json!({ "jsonrpc": "2.0", "id": 7,
                     "error": { "code": -32000, "message": "ORA-12170", "data": { "category": "connection" } } }),
            WireDialect::JsonRpc,
        );
        let error = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("收到")
            .expect_err("应当是失败");
        match error {
            AgentError::Remote {
                code,
                message,
                data,
            } => {
                assert_eq!(code, -32000);
                assert_eq!(message, "ORA-12170");
                assert_eq!(
                    data.and_then(|value| value.get("category").cloned()),
                    Some(json!("connection"))
                );
            }
            other => panic!("应当是 Remote，实际 {other:?}"),
        }
    }

    /// 未知 id 的回应对不上号就丢掉（不能误唤醒别人，也不能 panic）。
    #[test]
    fn a_response_for_an_unknown_id_is_dropped() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(1, sender);
        route(
            &pending,
            &json!({ "jsonrpc": "2.0", "id": 99, "result": 1 }),
            WireDialect::JsonRpc,
        );
        assert!(receiver.try_recv().is_err(), "不该被别人的回应唤醒");
        assert!(pending.lock().expect("锁").contains_key(&1));
    }

    /// 没有 `id` 的消息（通知 / 心跳）不是回应，不该唤醒任何人。
    #[test]
    fn a_notification_without_id_is_ignored() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(1, sender);
        route(
            &pending,
            &json!({ "jsonrpc": "2.0", "method": "ping" }),
            WireDialect::JsonRpc,
        );
        assert!(receiver.try_recv().is_err());
    }

    /// 真起一个进程：握手能拿到结果；进程退出后**所有在飞的请求立刻失败**，
    /// 而不是让调用方干等到超时。
    ///
    /// 假 agent 用 `sh` 写：读到一行就回一条固定 `id` 的握手结果，然后退出。
    /// 客户端的 `id` 从 1 开始（有单测钉住），所以这里回 `id: 1` 是确定的。
    #[test]
    fn a_real_process_answers_then_its_exit_wakes_pending_requests() {
        let process = AgentProcess {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                // 读一行 → 回一条 id=1 的握手（形状照抄真 agent）→ 退出。
                r#"read line; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2,"agentProtocolVersion":2,"capabilities":["connect","query","multi_session"]}}'; exit 0"#
                    .to_owned(),
            ],
            working_dir: None,
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn(&process).expect("起进程");
        let handshake = handle.handshake().expect("握手");
        assert_eq!(handshake.protocol_version, 2);
        assert!(handshake.is_multi_session());
        assert!(handshake.supports(protocol::capability::QUERY));
        assert!(!handshake.supports(protocol::capability::TRANSACTION));

        // 进程已经退出：再发请求必须**很快**失败（EOF 唤醒），不是等到超时。
        let started = Instant::now();
        let error = handle
            .call("list_databases", json!({}), Duration::from_secs(30))
            .expect_err("进程没了就不该成功");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "EOF 应当立刻唤醒，实际等了 {:?}（{error}）",
            started.elapsed()
        );
    }

    /// sidecar 的 `id` 是**字符串**：请求得送字符串，回应里的字符串 id 也要能对上账。
    ///
    /// 这三条钉住的是同一个坑的三面：送数字过去对端**解析失败**、回来一条 `id` 为
    /// 空串的错误 —— 调用方不会立刻知道，只会在超时后拿到一句「没回应」。
    #[test]
    fn the_sidecar_dialect_sends_and_matches_string_ids() {
        let request = request_envelope(
            WireDialect::DbxSidecar,
            7,
            "execute",
            json!({ "sql": "select 1" }),
        );
        assert_eq!(request["id"], json!("7"), "sidecar 的 id 必须是字符串");
        assert_eq!(request["method"], json!("execute"));
        assert!(
            request.get("jsonrpc").is_none(),
            "sidecar 没有 jsonrpc 字段"
        );

        // JSON-RPC 那边不受影响（数字 id、带 jsonrpc 字段）。
        let request = request_envelope(WireDialect::JsonRpc, 7, "execute", json!({}));
        assert_eq!(request["id"], json!(7));
        assert_eq!(request["jsonrpc"], json!("2.0"));

        // 回应：字符串 id 认得出来；空串 id（对端没解析出请求）不算任何人的回应。
        assert_eq!(
            response_id(&json!({ "id": "7", "ok": true }), WireDialect::DbxSidecar),
            Some(7)
        );
        assert_eq!(
            response_id(&json!({ "id": "", "ok": false }), WireDialect::DbxSidecar),
            None
        );
        // 宽容一点：sidecar 回来数字 id 也认（没必要为形状多打一场）。
        assert_eq!(
            response_id(&json!({ "id": 7, "ok": true }), WireDialect::DbxSidecar),
            Some(7)
        );
    }

    /// sidecar 的错误码是 **snake_case 字符串**：它得能被上层取出来分支
    /// （`duckdb_worker_poisoned` 要求父进程杀掉并重起），而消息仍是人话。
    #[test]
    fn a_sidecar_error_carries_its_code_and_message() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(9, sender);
        route(
            &pending,
            &json!({ "id": "9", "ok": false,
                     "error": { "code": "duckdb_execute_failed", "message": "Catalog Error: Table with name nope does not exist!" } }),
            WireDialect::DbxSidecar,
        );
        let error = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("收到")
            .expect_err("应当是失败");
        assert_eq!(sidecar_error_code(&error), Some("duckdb_execute_failed"));
        let AgentError::Remote { message, .. } = &error else {
            panic!("应当是 Remote，实际 {error:?}");
        };
        assert!(message.contains("Catalog Error"), "{message}");
    }

    /// sidecar 的成功回应**没有** `error` 字段、只有 `ok: true` —— 别把结果读成空。
    #[test]
    fn a_sidecar_success_is_not_mistaken_for_an_error() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        pending.lock().expect("锁").insert(3, sender);
        route(
            &pending,
            &json!({ "id": "3", "ok": true, "result": { "columns": ["a"], "rows": [[1]] } }),
            WireDialect::DbxSidecar,
        );
        let value = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("收到")
            .expect("应当是成功");
        assert_eq!(value["columns"], json!(["a"]));
        assert_eq!(sidecar_error_code(&AgentError::Closed), None);
    }

    /// 真进程对线：sidecar 方言下请求真的长得像 sidecar（字符串 id），
    /// 而假 worker 照 sidecar 的形状回一句 `ok: true`。
    #[test]
    fn a_real_sidecar_style_process_answers_by_string_id() {
        let process = AgentProcess {
            program: PathBuf::from("/bin/sh"),
            // 读一行 → 把它**自己收到的 id** 用 sed 抠出来回过去（真 worker 就是回显 id）
            // → 退出。这样连「我们送的是不是字符串」一起验了。
            args: vec![
                "-c".to_owned(),
                r#"read line; id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'); printf '{"id":"%s","ok":true,"result":{"connected":true}}\n' "$id"; exit 0"#
                    .to_owned(),
            ],
            working_dir: None,
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn_with(&process, WireDialect::DbxSidecar).expect("起进程");
        let value = handle
            .call("connect", json!({ "path": ":memory:" }), CONNECT_TIMEOUT)
            .expect("connect 应当成功");
        assert_eq!(value["connected"], json!(true));
    }

    /// 超时要报「哪个方法等多久」，而不是一句含糊的失败。
    #[test]
    fn a_request_that_never_answers_reports_a_timeout() {
        let process = AgentProcess {
            program: PathBuf::from("/bin/sh"),
            // 读一行、什么都不回、挂着睡。
            args: vec!["-c".to_owned(), "read line; sleep 30".to_owned()],
            working_dir: None,
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn(&process).expect("起进程");
        let error = handle
            .call("list_databases", json!({}), Duration::from_millis(300))
            .expect_err("不该有回应");
        match error {
            AgentError::Timeout { method, waited } => {
                assert_eq!(method, "list_databases");
                assert_eq!(waited, Duration::from_millis(300));
            }
            other => panic!("应当是超时，实际 {other:?}"),
        }
    }

    /// 起不来的程序报「起不来」，带着程序路径。
    #[test]
    fn a_missing_program_is_a_transport_error() {
        let process = AgentProcess {
            program: PathBuf::from("/definitely/not/here/agent"),
            args: Vec::new(),
            working_dir: None,
            env: Vec::new(),
        };
        let error = AgentHandle::spawn(&process).expect_err("不该起得来");
        assert!(matches!(error, AgentError::Transport(_)), "{error:?}");
        assert!(error.to_string().contains("/definitely/not/here/agent"));
    }

    /// 对端不读 stdin 时，调用方**照样按超时收场** —— 不会卡在写管道上。
    ///
    /// 这条钉的是「写走独立队列」这个设计：直接写 stdin 的实现在这里会挂死
    /// （管道满了，`write` 无限期阻塞，而且卡在**超时开始之前**）——
    /// 于是「60 秒超时」和「永久挂起」没有区别。
    ///
    /// 载荷特意开到远大于管道缓冲（64 KiB），确保写线程真的会被堵住。
    #[test]
    fn a_peer_that_never_reads_cannot_stall_the_caller() {
        let process = AgentProcess {
            program: PathBuf::from("/bin/sh"),
            // 不读 stdin、不回任何东西，只是活着。
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            working_dir: None,
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn(&process).expect("起进程");
        let huge = "x".repeat(4 * 1024 * 1024);
        let started = Instant::now();
        let error = handle
            .call(
                "execute_query",
                json!({ "sql": huge }),
                Duration::from_millis(300),
            )
            .expect_err("不该有回应");
        assert!(
            matches!(error, AgentError::Timeout { .. }),
            "应当是超时，实际 {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "调用方被写管道拖住了，实际等了 {:?}",
            started.elapsed()
        );
    }

    /// 非 JSON 行（驱动进程的调试打印）不该让整个通道失效。
    #[test]
    fn a_non_json_line_does_not_break_the_channel() {
        let process = AgentProcess {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                r#"read line; printf 'hello from a debug print\n'; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}'; read line; sleep 1"#
                    .to_owned(),
            ],
            working_dir: None,
            env: Vec::new(),
        };
        let handle = AgentHandle::spawn(&process).expect("起进程");
        let handshake = handle.handshake().expect("杂音之后仍能握手");
        assert_eq!(handshake.protocol_version, 2);
    }
}
