//! 进程 runner：拉起插件二进制、双向泵、进度回调、取消与硬限制。
//!
//! 线程模型（零 tokio，全 `std`）：
//!
//! ```text
//! main（调用方线程）          stdout 线程                stderr 线程
//! ─────────────────          ─────────────────          ─────────────
//! 写请求行 → 关闭请求期
//! loop: recv_timeout(100ms)  read_until('\n')           收进共享 VecDeque
//!   ├ 事件 → sink            按行/字节限截断            （只留末尾几行，
//!   ├ cancel → cancel 流程    → mpsc<Event>              供失败详情引用）
//!   └ idle → 30s 告警
//! ```
//!
//! 取消：调用方线程把 `{"type":"cancel"}` 写进 stdin（插件体面收尾），
//! 等 [`CANCEL_GRACE`] 不退就 `SIGTERM`，再等 [`KILL_GRACE`] 就 `SIGKILL`。
//! 信号经 `libc::kill`（std 不暴露信号 API；`child.kill()` 直接是 SIGKILL，
//! 跳过了「给插件一次自己收尾」的机会）。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duck_plugin::PluginError;

use crate::protocol::{Event, MAX_LINE_BYTES, MAX_TOTAL_BYTES, RunRequest};

/// 写完取消行后等插件自己退出的宽限期。
pub const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// `SIGTERM` 之后的最后通牒。
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// 事件泵的轮询间隔（取消与告警的时间分辨率）。
const POLL: Duration = Duration::from_millis(100);
/// 失败详情里保留的 stderr 末行数。
const STDERR_TAIL: usize = 20;

/// 进度回调：runner 把插件的事件实时转发给宿主（执行页据此更新进度/日志区）。
pub trait ProgressSink {
    /// 插件的一行过程日志；`level` 是插件自报的级别（缺省 `None` = info）。
    fn on_log(&mut self, line: &str, level: Option<&str>);
    /// 进度；`total = None` 表示不定总量。
    fn on_progress(&mut self, current: u64, total: Option<u64>, message: Option<&str>);
}

/// 什么都不做的空实现（测试与「只关心最终结果」的调用方用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl ProgressSink for NullSink {
    fn on_log(&mut self, _line: &str, _level: Option<&str>) {}
    fn on_progress(&mut self, _current: u64, _total: Option<u64>, _message: Option<&str>) {}
}

/// 取消令牌：跨线程置位，runner 的主循环轮询到后走取消流程。
#[derive(Debug, Clone, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    /// 新令牌（未取消）。
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// 请求取消（可在任意线程调）。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// 是否已请求取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 一次成功运行的产出。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunOutcome {
    /// `result` 事件的正文（纯文本镜像：复制按钮复制的就是它）。
    pub output: String,
    /// `body` 分片拼起来解析出的结构化正文（`diff` / `table` 这类富结果）。
    ///
    /// 没发 `body` 就是 `None` —— 此时渲染器回落到 `output` 文本
    /// （`kind = "diff"` 会尝试按 unified diff 解析，见 `duck_plugin::diff`）。
    pub body: Option<serde_json::Value>,
}

/// stderr 收集器：只保留末尾 [`STDERR_TAIL`] 行（失败详情用），全量照走 tracing。
#[derive(Clone)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL))))
    }

    fn push(&self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        tracing::info!(target: "duck::plugin::process", stderr = %line);
        let mut tail = self.0.lock().expect("stderr tail lock");
        if tail.len() == STDERR_TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    fn join(&self) -> String {
        let tail = self.0.lock().expect("stderr tail lock");
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// 启动插件进程并跑完一次请求；返回 `result` 事件的正文。
///
/// 传给进程的环境变量：[`duck_plugin::ENV_LOCALE`] / [`duck_plugin::ENV_LOCALES_DIR`]
/// （取词的只读契约，见 `duck_plugin::locale_env`）。
///
/// # Errors
/// - 启动失败 / 请求序列化失败 → [`PluginError::Io`] / [`PluginError::Execution`]；
/// - 插件发 `error` 事件、非零退出、没有 `result` 就退出 → [`PluginError::Execution`]
///   （带 stderr 末尾）；
/// - 行长 / 总量超限、用户取消 → [`PluginError::Execution`]。
pub fn run(
    plugin_dir: &Path,
    bin: &Path,
    request: &RunRequest,
    sink: &mut dyn ProgressSink,
    cancel: &CancelHandle,
) -> Result<RunOutcome, PluginError> {
    let mut command = Command::new(bin);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 独立进程组：插件自己 fork 的子进程（sleep、ffmpeg…）会继承管道，
    // 只杀主进程的话 stdout 会被孙进程攥着不关，宿主就等不到 EOF。
    // 整组一起杀，取消才能立刻生效。
    command.process_group(0);
    for (key, value) in duck_plugin::locale_env(plugin_dir, &request.lang) {
        command.env(key, value);
    }
    // 插件私有数据目录：与 JS 轨的 `duck.dataDir` 同源（宿主按插件 id 生成）。
    // 空串**不导出**——导出空串等于告诉插件「你家就在当前目录」，它会往 cwd 里写东西。
    // 反向也要 `env_remove`：`Command` 默认继承父环境，宿主自己环境里若有同名变量，
    // 插件会拿到一个**宿主没打算给它**的目录（可能指向另一个插件的数据）。
    if request.data.is_empty() {
        command.env_remove(duck_plugin::ENV_DATA_DIR);
    } else {
        command.env(duck_plugin::ENV_DATA_DIR, &request.data);
    }
    let mut child = command
        .spawn()
        .map_err(|error| PluginError::Execution(format!("spawn {}: {error}", bin.display())))?;
    let result = drive(&mut child, request, sink, cancel);
    // 无论成败都回收僵尸：`wait` 已经在 drive 的收尾做过，这里兜底幂等。
    let _ = child.wait();
    result
}

/// 拉起之后的全部交互（单出 `run` 是为了让取消/收尾逻辑能集中测）。
fn drive(
    child: &mut Child,
    request: &RunRequest,
    sink: &mut dyn ProgressSink,
    cancel: &CancelHandle,
) -> Result<RunOutcome, PluginError> {
    let request_line = request.to_line()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| PluginError::Execution("plugin process has no stdin".into()))?;
    write_request_line(&mut stdin, &request_line)?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PluginError::Execution("plugin process has no stdout".into()))?;
    let stderr_tail = StderrTail::new();
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PluginError::Execution("plugin process has no stderr".into()))?;

    // stderr 线程：只收集（末 20 行），不参与事件流。
    let tail_for_thread = stderr_tail.clone();
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tail_for_thread.push(line);
        }
    });

    // stdout 线程：逐行限长 → 解析事件 → 通道。（通道 `Send` 边界要求事件
    // 拥有所有权，正好：`Event` 是 owned 的。）
    let (tx, rx) = std::sync::mpsc::channel::<Result<Event, PluginError>>();
    let stdout_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut total = 0usize;
        loop {
            let mut line = Vec::new();
            let mut over_limit = false;
            // 手写 `read_until`：std 的 `lines()` 不设上限，失控行能吃光内存。
            loop {
                let mut byte = [0u8; 1];
                match reader.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        if byte[0] == b'\n' {
                            break;
                        }
                        if line.len() < MAX_LINE_BYTES {
                            line.push(byte[0]);
                        } else {
                            over_limit = true;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(PluginError::Execution(format!(
                            "read plugin stdout: {error}"
                        ))));
                        return;
                    }
                }
            }
            if line.is_empty() && !over_limit {
                break; // EOF
            }
            total += line.len() + 1;
            if over_limit {
                let _ = tx.send(Err(PluginError::Execution(format!(
                    "event line exceeds the {} byte cap",
                    MAX_LINE_BYTES
                ))));
                return;
            }
            if total > MAX_TOTAL_BYTES {
                let _ = tx.send(Err(PluginError::Execution(format!(
                    "event stream exceeds the {} byte cap",
                    MAX_TOTAL_BYTES
                ))));
                return;
            }
            let Ok(text) = String::from_utf8(line) else {
                let _ = tx.send(Err(PluginError::Execution(
                    "event line is not valid UTF-8".into(),
                )));
                return;
            };
            match Event::parse_line(&text) {
                Ok(event) => {
                    if tx.send(Ok(event)).is_err() {
                        return; // 宿主已放弃（取消/出错），没必要再泵。
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            }
        }
    });

    let outcome = pump(&rx, sink, cancel, &mut stdin, child);
    drop(stdin); // 关 stdin：等结果后的正常收尾也让插件看到 EOF。
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let status = child
        .wait()
        .map_err(|error| PluginError::Execution(format!("wait plugin process: {error}")))?;
    let stderr_tail_text = stderr_tail.join();

    match outcome {
        Ok(Some(collected)) => {
            if !status.success() {
                return Err(PluginError::Execution(format!(
                    "plugin exited with {status} after reporting a result\n{stderr_tail_text}"
                )));
            }
            // 分片是 JSON 片段，只有拼完才可能判断对不对；解析失败按协议违约处理
            // （静默丢掉会让 `kind = "diff"` 的工具悄悄退化成纯文本）。
            let body = if collected.body.trim().is_empty() {
                None
            } else {
                Some(serde_json::from_str(&collected.body).map_err(|error| {
                    PluginError::Execution(format!(
                        "structured body is not valid JSON: {error}\n{stderr_tail_text}"
                    ))
                })?)
            };
            Ok(RunOutcome {
                output: collected.output,
                body,
            })
        }
        Ok(None) => {
            let reason = if cancel.is_cancelled() {
                "cancelled by user".to_owned()
            } else {
                format!("plugin exited with {status} without a result event")
            };
            Err(PluginError::Execution(format!(
                "{reason}\n{stderr_tail_text}"
            )))
        }
        Err(mut error) => {
            // 失败详情带上 stderr 末尾：退出码本身不带原因，尾巴才带。
            match &mut error {
                PluginError::Execution(message) if !stderr_tail_text.is_empty() => {
                    message.push('\n');
                    message.push_str(&stderr_tail_text);
                }
                _ => {}
            }
            Err(error)
        }
    }
}

/// 写请求行并立即刷盘（插件在读到第一行之前什么都做不了）。
fn write_request_line(stdin: &mut std::process::ChildStdin, line: &str) -> Result<(), PluginError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(PluginError::Execution(format!(
            "run request exceeds the {} byte line cap",
            MAX_LINE_BYTES
        )));
    }
    writeln!(stdin, "{line}")
        .and_then(|()| stdin.flush())
        .map_err(|error| PluginError::Execution(format!("write run request: {error}")))
}

/// `result` 事件与已拼好的 `body` 分片（收尾时才解析 JSON）。
struct Collected {
    output: String,
    body: String,
}

/// 主循环：事件 → 回调；取消 → 宽限流；超时 → 告警。
/// `Ok(Some(..))` = 拿到 result；`Ok(None)` = 流断且无 result（由收尾区分取消）。
fn pump(
    rx: &std::sync::mpsc::Receiver<Result<Event, PluginError>>,
    sink: &mut dyn ProgressSink,
    cancel: &CancelHandle,
    stdin: &mut std::process::ChildStdin,
    child: &mut Child,
) -> Result<Option<Collected>, PluginError> {
    let mut output = None;
    let mut body = String::new();
    let mut idle = Instant::now();
    let mut warned = false;
    loop {
        if cancel.is_cancelled() {
            graceful_cancel(stdin, child);
            return Ok(None);
        }
        match rx.recv_timeout(POLL) {
            Ok(Ok(Event::Log { line, level })) => {
                idle = Instant::now();
                sink.on_log(&line, level.as_deref());
            }
            Ok(Ok(Event::Progress {
                current,
                total,
                message,
            })) => {
                idle = Instant::now();
                sink.on_progress(current, total, message.as_deref());
            }
            Ok(Ok(Event::Body { chunk })) => {
                idle = Instant::now();
                // 上限在分片处就拦：等到拼完 32 MiB 再报错等于先把内存吃掉。
                if body.len() + chunk.len() > crate::protocol::MAX_BODY_BYTES {
                    return Err(PluginError::Execution(format!(
                        "structured body exceeds the {} byte cap",
                        crate::protocol::MAX_BODY_BYTES
                    )));
                }
                body.push_str(&chunk);
            }
            Ok(Ok(Event::Result { output: text })) => {
                output = Some(text);
                break;
            }
            Ok(Ok(Event::Error { message })) => {
                return Err(PluginError::Execution(message));
            }
            Ok(Err(error)) => return Err(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !warned && idle.elapsed() >= crate::protocol::IDLE_WARN {
                    warned = true;
                    tracing::warn!(target: "duck::plugin::process", "no events for 30s; plugin may be stalled (cancel is still available)");
                    sink.on_log("(host) no events for 30s; you can cancel the run", None);
                }
            }
            // stdout 线程退出且通道空：进程把流关了（可能还没 wait）。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(output.map(|output| Collected { output, body }))
}

/// 取消流程：先给体面收尾的机会，再逐级升级信号。
fn graceful_cancel(stdin: &mut std::process::ChildStdin, child: &mut Child) {
    let _ = writeln!(stdin, r#"{{"type":"cancel"}}"#);
    let _ = stdin.flush();
    // 负号 = 杀整个进程组（见 run 里的 `process_group(0)`）。
    let pgid = -(child.id() as i32);
    if wait_exit(child, CANCEL_GRACE) {
        return;
    }
    tracing::warn!(target: "duck::plugin::process", pgid = -pgid, "plugin did not exit after cancel; sending SIGTERM to the process group");
    unsafe { libc::kill(pgid, libc::SIGTERM) };
    if wait_exit(child, KILL_GRACE) {
        return;
    }
    tracing::warn!(target: "duck::plugin::process", pgid = -pgid, "plugin survived SIGTERM; sending SIGKILL to the process group");
    unsafe { libc::kill(pgid, libc::SIGKILL) };
    let _ = child.wait();
}

/// 限时等子进程退出；`true` = 在期限内退了（或已经退出）。
fn wait_exit(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => return true, // wait 出错就别再吊着了
        }
    }
}
