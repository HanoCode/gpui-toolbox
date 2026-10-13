//! 连接钩子：在建连 / 断开的前后跑一条**本机命令**。
//!
//! 对齐 DBFlux 的四个相位（PreConnect / PostConnect / PreDisconnect / PostDisconnect）。
//! 本实现只做「本机进程」这一种载体（[`HookAction`] 的两档都是 spawn 一个进程：命令
//! 是 `sh -c '<行>'`，脚本是 `<解释器> <文件>`）；DBFlux 那种"在进程内跑 Lua"要等
//! 脚本文档（独立事项）落地，这里不预埋半截实现。
//!
//! 三条纪律：
//! - **默认不阻断**（[`HookFailure::Warn`]）：钩子是用户自己写在档案里的命令，打错一个
//!   不该让生产库连不上；要阻断得对这个钩子显式选 [`HookFailure::Block`]。
//! - **超时有默认值**（[`DEFAULT_HOOK_TIMEOUT_SECS`]）：钩子挂住 = 连接挂住，必须先于
//!   连接本身了断。
//! - **口令绝不进子进程环境**：环境变量只给"这条连接是谁"这类事实（主机 / 库 / 用户名），
//!   口令一律不传——需要凭据的钩子自己去凭据文件取。
//!
//! 输出走**临时文件**而不是管道：钩子若把文件的写端交给后台孙进程，管道读端会一直等
//! 不到 EOF（join 卡死），文件没这个问题（读的是"此刻写到哪了"）。

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 一条钩子的默认超时（秒）。
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

/// 一条钩子最多留多少输出（防止 `yes` 这类命令把内存吃光）。
const OUTPUT_CAP: usize = 8 * 1024;

/// 等待子进程时的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 钩子相位：建连前 / 建连后 / 断开前 / 断开后。
///
/// 顺序即界面里的展示顺序（也是「一条连接的一生」的先后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    /// 连上之前（可以拿来做「打开 VPN」「切 DNS」这类前置动作）。
    PreConnect,
    /// 连上之后（目录都取完了才算数）。
    PostConnect,
    /// 断开之前（连接还活着时跑）。
    PreDisconnect,
    /// 断开之后（连接已经关掉了）。
    PostDisconnect,
}

impl HookPhase {
    /// 四相位全量（界面展示顺序）。
    pub const ALL: [Self; 4] = [
        Self::PreConnect,
        Self::PostConnect,
        Self::PreDisconnect,
        Self::PostDisconnect,
    ];

    /// 稳定键（持久化 / 环境变量 / 元素 id 都用它，**不随界面语言变**）。
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::PreConnect => "pre_connect",
            Self::PostConnect => "post_connect",
            Self::PreDisconnect => "pre_disconnect",
            Self::PostDisconnect => "post_disconnect",
        }
    }

    /// 从稳定键还原（下拉回投用；认不出给 `None`，不猜）。
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|phase| phase.key() == key)
    }

    /// 给子进程的环境变量值（大写）。
    #[must_use]
    pub const fn env_name(self) -> &'static str {
        match self {
            Self::PreConnect => "PRE_CONNECT",
            Self::PostConnect => "POST_CONNECT",
            Self::PreDisconnect => "PRE_DISCONNECT",
            Self::PostDisconnect => "POST_DISCONNECT",
        }
    }
}

/// 一个钩子要做的事。
///
/// 两档都是「spawn 一个进程」——脚本只是命令行的**可复用形态**（解释器 + 文件路径），
/// 两者的环境变量、超时、失败策略、输出采集完全共享同一套代码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum HookAction {
    /// 交给平台 shell 跑一行命令（macOS / Linux `sh -c '<行>'`，Windows `cmd /c <行>`）。
    Command {
        /// 命令行全文。
        line: String,
        /// 工作目录（`None` = 进程默认，一般是应用的工作目录）。
        cwd: Option<String>,
    },
    /// 用指定解释器跑一个脚本文件（`<解释器> <文件>`）。
    Script {
        /// 解释器或可执行文件（`bash` / `python3` / 绝对路径）。
        interpreter: String,
        /// 脚本文件路径。
        path: String,
        /// 工作目录（`None` = 进程默认）。
        cwd: Option<String>,
    },
}

impl HookAction {
    /// 工作目录（两档共用）。
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        match self {
            Self::Command { cwd, .. } | Self::Script { cwd, .. } => {
                cwd.as_deref().filter(|dir| !dir.trim().is_empty())
            }
        }
    }

    /// 把钩子翻译成「程序 + 参数」；空的命令 / 解释器返回 `None`（调用方按起不来处理）。
    #[must_use]
    pub fn command_line(&self) -> Option<(String, Vec<String>)> {
        match self {
            Self::Command { line, .. } => {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                // 走平台 shell 而不是自己切词：用户写的引号 / 管道 / 重定向都该照常生效。
                let (program, flag) = if cfg!(windows) {
                    ("cmd", "/c")
                } else {
                    ("sh", "-c")
                };
                Some((program.to_owned(), vec![flag.to_owned(), line.to_owned()]))
            }
            Self::Script {
                interpreter, path, ..
            } => {
                let interpreter = interpreter.trim();
                let path = path.trim();
                if interpreter.is_empty() || path.is_empty() {
                    return None;
                }
                Some((interpreter.to_owned(), vec![path.to_owned()]))
            }
        }
    }

    /// 展示用的一行（审计与界面上的「跑的是哪一条」）。
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Command { line, .. } => line.trim().to_owned(),
            Self::Script {
                interpreter, path, ..
            } => format!("{} {}", interpreter.trim(), path.trim()),
        }
    }
}

/// 钩子失败时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailure {
    /// 只记结果、继续（默认）：一个打错的钩子不该让生产库连不上。
    #[default]
    Warn,
    /// 阻断：本次建连 / 断开不再往下走。
    Block,
}

/// 一条连接钩子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionHook {
    /// 稳定 id（界面上增删改都按它定位，改相位 / 命令都不影响）。
    #[serde(default)]
    pub id: String,
    /// 在哪个相位跑。
    pub phase: HookPhase,
    /// 是否启用（停用而不删，便于临时关掉）。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 干什么（展平进本结构，kv 里读起来是一层）。
    #[serde(flatten)]
    pub action: HookAction,
    /// 失败了怎么办。
    #[serde(default)]
    pub failure: HookFailure,
    /// 单条超时（秒）；`None` / `0` 用 [`DEFAULT_HOOK_TIMEOUT_SECS`]。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `enabled` 的 serde 默认：**新写的钩子默认启用**（用户刚写完一条命令，意思就是要它跑）。
const fn default_enabled() -> bool {
    true
}

impl ConnectionHook {
    /// 实际生效的超时。
    #[must_use]
    pub fn timeout(&self) -> Duration {
        let secs = self
            .timeout_secs
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS);
        Duration::from_secs(secs)
    }

    /// 这条钩子该不该在这个相位跑。
    #[must_use]
    pub fn runs_at(&self, phase: HookPhase) -> bool {
        self.enabled && self.phase == phase
    }
}

/// 档案上的钩子集合（一个字段，`#[serde(default)]` 保证老 kv 读得进来）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionHooks {
    /// 全部钩子（顺序即执行顺序：同一相位内按这里的先后跑）。
    #[serde(default)]
    pub hooks: Vec<ConnectionHook>,
}

impl ConnectionHooks {
    /// 一条都没有（界面据此不画入口、建连路径据此不取快照）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// 某个相位有没有启用的钩子。
    ///
    /// 这是「要不要为这个相位起后台任务」的唯一判据：没有就不该白跑一次进程往返。
    #[must_use]
    pub fn has_phase(&self, phase: HookPhase) -> bool {
        self.hooks.iter().any(|hook| hook.runs_at(phase))
    }

    /// 某个相位的全部钩子（含停用的；界面要显示它们）。
    #[must_use]
    pub fn of_phase(&self, phase: HookPhase) -> Vec<&ConnectionHook> {
        self.hooks
            .iter()
            .filter(|hook| hook.phase == phase)
            .collect()
    }

    /// 生成一个新钩子 id（现有最大 +1，不回收已删除的号）。
    #[must_use]
    pub fn next_id(&self) -> String {
        let mut seq = self.hooks.len() + 1;
        loop {
            let candidate = format!("hook-{seq}");
            if !self.hooks.iter().any(|hook| hook.id == candidate) {
                return candidate;
            }
            seq += 1;
        }
    }
}

/// 钩子能看到的环境事实（只读、**不含口令**）。
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    /// 档案 id。
    pub connection_id: String,
    /// 档案显示名。
    pub connection_name: String,
    /// 引擎线名（[`crate::config::DatabaseKind::wire_name`]）。
    pub kind: String,
    /// 主机（SQLite 为空）。
    pub host: String,
    /// 端口（未填时给引擎缺省端口的字符串；都拿不到给空）。
    pub port: String,
    /// 用户名。
    pub user: String,
    /// 库名（SQLite 是文件路径）。
    pub database: String,
}

impl HookContext {
    /// 该传给子进程的环境变量（`DUCK_CONN_*` 前缀 + 当前相位）。
    fn vars(&self, phase: HookPhase) -> Vec<(String, String)> {
        vec![
            ("DUCK_HOOK_PHASE".into(), phase.env_name().to_owned()),
            ("DUCK_CONN_ID".into(), self.connection_id.clone()),
            ("DUCK_CONN_NAME".into(), self.connection_name.clone()),
            ("DUCK_CONN_KIND".into(), self.kind.clone()),
            ("DUCK_CONN_HOST".into(), self.host.clone()),
            ("DUCK_CONN_PORT".into(), self.port.clone()),
            ("DUCK_CONN_USER".into(), self.user.clone()),
            ("DUCK_CONN_DATABASE".into(), self.database.clone()),
        ]
    }
}

/// 一次钩子执行的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStatus {
    /// 正常退出（退出码 0）。
    Ok,
    /// 非零退出（`code` 为 `None` = 被信号杀掉，拿不到码）。
    Failed {
        /// 退出码。
        code: Option<i32>,
    },
    /// 超时被 kill。
    TimedOut,
    /// 起不来（命令为空 / 解释器或工作目录不存在 / 无执行权限）。
    SpawnFailed(String),
}

impl HookStatus {
    /// 这一枪算不算成功。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// 一次钩子执行的结果（审计与界面共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// 哪条钩子。
    pub hook_id: String,
    /// 哪个相位。
    pub phase: HookPhase,
    /// 实际跑的那一行（不含环境变量注入）。
    pub command: String,
    /// 结果状态。
    pub status: HookStatus,
    /// 合并后的输出（stdout 在前、stderr 在后，已截到 [`OUTPUT_CAP`]）。
    pub output: String,
    /// 耗时（毫秒）。
    pub duration_ms: u64,
}

impl HookOutcome {
    /// 失败详情，给日志 / 审计用（**英文**：本 crate 不产生面向用户的散文）。
    #[must_use]
    pub fn detail(&self) -> String {
        match &self.status {
            HookStatus::Ok => "ok".to_owned(),
            HookStatus::Failed { code } => match code {
                Some(code) => format!("exit code {code}"),
                None => "killed by signal".to_owned(),
            },
            HookStatus::TimedOut => "timed out".to_owned(),
            HookStatus::SpawnFailed(reason) => format!("spawn failed: {reason}"),
        }
    }
}

/// 一个相位的执行报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReport {
    /// 逐条结果（顺序即执行顺序）。
    pub outcomes: Vec<HookOutcome>,
    /// 第一条「失败 + 要求阻断」的钩子在 `outcomes` 里的下标。
    pub blocked: Option<usize>,
}

impl HookReport {
    /// 全部成功（或没有钩子）。
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.outcomes.iter().all(|outcome| outcome.status.is_ok())
    }

    /// 被阻断的那条结果。
    #[must_use]
    pub fn blocker(&self) -> Option<&HookOutcome> {
        self.blocked.and_then(|index| self.outcomes.get(index))
    }

    /// 失败（含只是告警）的条数。
    #[must_use]
    pub fn failures(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.status.is_ok())
            .count()
    }
}

/// 跑完某个相位的全部钩子。
///
/// **遇到「失败 + Block」立刻停**：后面的钩子不再跑 —— 已经知道这次连接要中止了，再跑
/// 下去只会多几个副作用，而用户看到的是同一条阻断结论。
#[must_use]
pub fn run_phase(hooks: &ConnectionHooks, phase: HookPhase, context: &HookContext) -> HookReport {
    let mut report = HookReport::default();
    for hook in hooks.hooks.iter().filter(|hook| hook.runs_at(phase)) {
        let outcome = run_hook(hook, phase, context);
        let failed = !outcome.status.is_ok();
        report.outcomes.push(outcome);
        if failed && hook.failure == HookFailure::Block {
            report.blocked = Some(report.outcomes.len() - 1);
            break;
        }
    }
    report
}

/// 单跑一条钩子（设置页的「试跑」用它，也是 [`run_phase`] 的最小单元）。
#[must_use]
pub fn run_hook(hook: &ConnectionHook, phase: HookPhase, context: &HookContext) -> HookOutcome {
    let command = hook.action.describe();
    let started = Instant::now();
    let mut outcome = HookOutcome {
        hook_id: hook.id.clone(),
        phase,
        command,
        status: HookStatus::Ok,
        output: String::new(),
        duration_ms: 0,
    };
    let Some((program, args)) = hook.action.command_line() else {
        outcome.status = HookStatus::SpawnFailed("hook has no command".to_owned());
        return outcome;
    };

    // 输出重定向到临时文件（见模块文档：管道会被后台孙进程拖住）。
    let log_path = temp_log_path();
    let sink = match std::fs::File::create(&log_path) {
        Ok(file) => file,
        Err(error) => {
            outcome.status = HookStatus::SpawnFailed(format!("temp file: {error}"));
            return outcome;
        }
    };
    let err_sink = match sink.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_file(&log_path);
            outcome.status = HookStatus::SpawnFailed(format!("temp file: {error}"));
            return outcome;
        }
    };

    let mut command_builder = Command::new(&program);
    command_builder
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(sink))
        .stderr(Stdio::from(err_sink))
        // 环境按「继承 + 覆盖」：钩子该看得见用户的 PATH，只是这几个键我们说了算。
        .envs(context.vars(phase));
    if let Some(cwd) = hook.action.cwd() {
        command_builder.current_dir(cwd);
    }

    match command_builder.spawn() {
        Ok(mut child) => {
            outcome.status = wait_with_timeout(&mut child, hook.timeout());
        }
        Err(error) => {
            outcome.status = HookStatus::SpawnFailed(error.to_string());
        }
    }
    outcome.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    outcome.output = read_capped(&log_path);
    let _ = std::fs::remove_file(&log_path);
    outcome
}

/// 等子进程结束；超时则 kill 并如实报 [`HookStatus::TimedOut`]。
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> HookStatus {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    HookStatus::Ok
                } else {
                    // 被信号杀死时 `code()` 是 `None` —— 不要把「被 kill」写成退出码 0。
                    HookStatus::Failed {
                        code: status.code(),
                    }
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // kill 之后仍要 `wait` 收尸，否则子进程留在僵尸态。
                    let _ = child.kill();
                    let _ = child.wait();
                    return HookStatus::TimedOut;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return HookStatus::SpawnFailed(error.to_string()),
        }
    }
}

/// 读临时日志（最多 [`OUTPUT_CAP`] 字节，非法字节按替换字符收下）。
fn read_capped(path: &std::path::Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buffer = Vec::new();
    if file
        .take(OUTPUT_CAP as u64)
        .read_to_end(&mut buffer)
        .is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&buffer).trim_end().to_owned()
}

/// 临时日志路径：进程 id + 自增序号（不引随机数依赖，同一进程内也不会撞）。
fn temp_log_path() -> std::path::PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("duck-hook-{}-{seq}.log", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条命令钩子（测试里省掉写全字段）。
    fn command_hook(line: &str, failure: HookFailure, timeout_secs: Option<u64>) -> ConnectionHook {
        ConnectionHook {
            id: "hook-1".to_owned(),
            phase: HookPhase::PreConnect,
            enabled: true,
            action: HookAction::Command {
                line: line.to_owned(),
                cwd: None,
            },
            failure,
            timeout_secs,
        }
    }

    /// 相位稳定键：四处（持久化 / 下拉 / 环境变量）都靠它，必须能往返。
    #[test]
    fn phase_keys_round_trip_and_are_distinct() {
        for phase in HookPhase::ALL {
            assert_eq!(HookPhase::from_key(phase.key()), Some(phase));
            assert!(!phase.env_name().is_empty());
        }
        assert_eq!(HookPhase::from_key("bogus"), None);
        let keys: std::collections::BTreeSet<&str> =
            HookPhase::ALL.iter().map(|phase| phase.key()).collect();
        assert_eq!(keys.len(), HookPhase::ALL.len());
    }

    /// 命令行的翻译：命令走平台 shell，脚本走「解释器 + 文件」；空的一律起不来。
    #[test]
    fn command_lines_translate_for_every_mode() {
        let command = HookAction::Command {
            line: "  echo hi  ".to_owned(),
            cwd: None,
        };
        let (program, args) = command.command_line().expect("命令非空");
        assert_eq!(args[1], "echo hi", "两端空白去掉，中间的照原样交给 shell");
        assert_eq!(program, if cfg!(windows) { "cmd" } else { "sh" });

        let script = HookAction::Script {
            interpreter: " bash ".to_owned(),
            path: " /tmp/a.sh ".to_owned(),
            cwd: None,
        };
        assert_eq!(
            script.command_line(),
            Some(("bash".to_owned(), vec!["/tmp/a.sh".to_owned()]))
        );
        assert_eq!(script.describe(), "bash /tmp/a.sh");

        // 空命令 / 空解释器 / 空路径都拿不到可执行命令行。
        assert!(
            HookAction::Command {
                line: "   ".to_owned(),
                cwd: None
            }
            .command_line()
            .is_none()
        );
        assert!(
            HookAction::Script {
                interpreter: String::new(),
                path: "/tmp/a.sh".to_owned(),
                cwd: None
            }
            .command_line()
            .is_none()
        );
        // 空 cwd 等于没给（否则 `current_dir("")` 会直接失败）。
        assert!(
            HookAction::Command {
                line: "true".to_owned(),
                cwd: Some("  ".to_owned())
            }
            .cwd()
            .is_none()
        );
    }

    /// 一条打印环境变量的钩子：`DUCK_CONN_*` 与相位名都得进去，且**不含口令**。
    #[test]
    fn hook_sees_connection_facts_but_no_secret() {
        let context = HookContext {
            connection_id: "conn-1".to_owned(),
            connection_name: "生产".to_owned(),
            kind: "postgres".to_owned(),
            host: "db.internal".to_owned(),
            port: "5432".to_owned(),
            user: "reader".to_owned(),
            database: "shop".to_owned(),
        };
        let hook = ConnectionHook {
            id: "hook-env".to_owned(),
            phase: HookPhase::PostConnect,
            enabled: true,
            action: HookAction::Command {
                line: "echo $DUCK_HOOK_PHASE $DUCK_CONN_HOST $DUCK_CONN_USER".to_owned(),
                cwd: None,
            },
            failure: HookFailure::Block,
            timeout_secs: None,
        };
        let outcome = run_hook(&hook, HookPhase::PostConnect, &context);
        assert_eq!(outcome.status, HookStatus::Ok, "{}", outcome.output);
        assert_eq!(outcome.output, "POST_CONNECT db.internal reader");
        assert!(outcome.duration_ms < 60_000);
        assert_eq!(
            outcome.command,
            "echo $DUCK_HOOK_PHASE $DUCK_CONN_HOST $DUCK_CONN_USER"
        );
    }

    /// 退出码如实带回来；被信号杀掉时**不能**当成退出码 0。
    #[test]
    fn non_zero_exit_is_reported_verbatim() {
        let context = HookContext::default();
        let outcome = run_hook(
            &command_hook("exit 3", HookFailure::Warn, None),
            HookPhase::PreConnect,
            &context,
        );
        assert_eq!(outcome.status, HookStatus::Failed { code: Some(3) });
        assert_eq!(outcome.detail(), "exit code 3");
        assert!(!outcome.status.is_ok());
    }

    /// 起不来的命令报 `SpawnFailed`，不是「退出码非零」。
    #[test]
    fn missing_interpreter_is_a_spawn_failure() {
        let context = HookContext::default();
        let hook = ConnectionHook {
            id: "hook-missing".to_owned(),
            phase: HookPhase::PreConnect,
            enabled: true,
            action: HookAction::Script {
                interpreter: "duck-no-such-interpreter".to_owned(),
                path: "/tmp/whatever".to_owned(),
                cwd: None,
            },
            failure: HookFailure::Warn,
            timeout_secs: None,
        };
        let outcome = run_hook(&hook, HookPhase::PreConnect, &context);
        assert!(
            matches!(outcome.status, HookStatus::SpawnFailed(_)),
            "{:?}",
            outcome.status
        );
        assert!(outcome.detail().starts_with("spawn failed:"));
    }

    /// 超时：钩子被 kill 掉，状态是 `TimedOut`（不是「跑完但非零」）。
    #[test]
    fn a_hanging_hook_is_killed_and_reported_as_timed_out() {
        let context = HookContext::default();
        let hook = command_hook("sleep 5", HookFailure::Warn, Some(1));
        assert_eq!(hook.timeout(), Duration::from_secs(1));
        let outcome = run_hook(&hook, HookPhase::PreConnect, &context);
        assert_eq!(outcome.status, HookStatus::TimedOut);
        assert_eq!(outcome.detail(), "timed out");
        // 真的了断了（不是等满 5 秒）：给足宽限仍应远小于 5 秒。
        assert!(
            outcome.duration_ms < 4_000,
            "耗时 {}ms",
            outcome.duration_ms
        );
    }

    /// 相位筛选：停用的不跑，别的相位不跑。
    #[test]
    fn only_enabled_hooks_of_the_phase_run() {
        let hooks = ConnectionHooks {
            hooks: vec![
                ConnectionHook {
                    id: "hook-on".to_owned(),
                    enabled: true,
                    ..command_hook("echo on", HookFailure::Warn, None)
                },
                ConnectionHook {
                    id: "hook-off".to_owned(),
                    enabled: false,
                    ..command_hook("echo off", HookFailure::Warn, None)
                },
                ConnectionHook {
                    id: "hook-later".to_owned(),
                    phase: HookPhase::PreDisconnect,
                    ..command_hook("echo later", HookFailure::Warn, None)
                },
            ],
        };
        assert!(hooks.has_phase(HookPhase::PreConnect));
        assert!(hooks.has_phase(HookPhase::PreDisconnect));
        assert!(!hooks.has_phase(HookPhase::PostConnect));
        assert_eq!(hooks.of_phase(HookPhase::PreConnect).len(), 2);
        let report = run_phase(&hooks, HookPhase::PreConnect, &HookContext::default());
        assert_eq!(report.outcomes.len(), 1, "停用的那条不该跑");
        assert_eq!(report.outcomes[0].hook_id, "hook-on");
        assert!(report.all_ok());
        assert!(report.blocked.is_none());
    }

    /// 阻断：失败即停，后面的钩子不再跑；`Warn` 则继续跑完。
    #[test]
    fn a_blocking_failure_stops_the_phase() {
        let context = HookContext::default();
        let blocking = ConnectionHooks {
            hooks: vec![
                command_hook("exit 7", HookFailure::Block, None),
                command_hook("echo never", HookFailure::Warn, None),
            ],
        };
        let report = run_phase(&blocking, HookPhase::PreConnect, &context);
        assert_eq!(report.outcomes.len(), 1, "Block 之后不再往下跑");
        assert_eq!(report.blocked, Some(0));
        assert!(!report.all_ok());
        assert_eq!(report.failures(), 1);
        assert_eq!(
            report.blocker().map(|outcome| outcome.hook_id.as_str()),
            Some("hook-1")
        );

        let warning = ConnectionHooks {
            hooks: vec![
                command_hook("exit 7", HookFailure::Warn, None),
                command_hook("echo still-runs", HookFailure::Warn, None),
            ],
        };
        let report = run_phase(&warning, HookPhase::PreConnect, &context);
        assert_eq!(report.outcomes.len(), 2, "Warn 不拦后面那条");
        assert!(report.blocked.is_none());
        assert_eq!(report.failures(), 1);
        assert_eq!(report.outcomes[1].output, "still-runs");
    }

    /// 钩子 id 不回收：删掉中间那条之后，新加的不会撞上还活着的 id。
    #[test]
    fn hook_ids_are_not_reused() {
        let hooks = ConnectionHooks::default();
        assert_eq!(hooks.next_id(), "hook-1");
        let hooks = ConnectionHooks {
            hooks: vec![
                command_hook("true", HookFailure::Warn, None),
                ConnectionHook {
                    id: "hook-3".to_owned(),
                    ..command_hook("true", HookFailure::Warn, None)
                },
            ],
        };
        assert_eq!(hooks.next_id(), "hook-4", "不复用被删掉的 hook-2");
    }

    /// 落盘往返：相位 / 载体 / 失败策略都要原样回来；老 kv（缺字段）要能回落。
    #[test]
    fn hooks_round_trip_through_json() {
        let hooks = ConnectionHooks {
            hooks: vec![
                command_hook("echo hi", HookFailure::Block, Some(5)),
                ConnectionHook {
                    id: "hook-2".to_owned(),
                    phase: HookPhase::PostDisconnect,
                    enabled: false,
                    action: HookAction::Script {
                        interpreter: "bash".to_owned(),
                        path: "/tmp/hook.sh".to_owned(),
                        cwd: Some("/tmp".to_owned()),
                    },
                    failure: HookFailure::Warn,
                    timeout_secs: None,
                },
            ],
        };
        let json = serde_json::to_string(&hooks).expect("可序列化");
        // 展平：`mode` 与 `line` 平铺在钩子那一层。
        assert!(json.contains(r#""mode":"command""#), "{json}");
        assert!(json.contains(r#""mode":"script""#), "{json}");
        let back: ConnectionHooks = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, hooks);
        assert_eq!(back.hooks[0].failure, HookFailure::Block);
        assert_eq!(back.hooks[0].timeout(), Duration::from_secs(5));
        assert!(!back.hooks[1].enabled);

        // 缺 `enabled` → 默认启用（用户刚写完一条命令就是要它跑）。
        let minimal: ConnectionHooks = serde_json::from_str(
            r#"{"hooks":[{"id":"h","phase":"pre_connect","mode":"command","line":"true"}]}"#,
        )
        .expect("缺字段要能回落");
        assert!(minimal.hooks[0].enabled);
        assert_eq!(minimal.hooks[0].failure, HookFailure::Warn, "默认不阻断");
        assert_eq!(
            minimal.hooks[0].timeout(),
            Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS)
        );
        // 整个字段缺席 → 空集合（老档案）。
        let legacy = ConnectionHooks::default();
        assert!(legacy.is_empty());
        assert!(!legacy.has_phase(HookPhase::PreConnect));
    }
}
