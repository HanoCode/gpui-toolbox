//! SQL 钩子（连接级命令钩子）与数据库操作闸门两件事。
//!
//! - **钩子**：相位的可读名、执行状态（内核给的是英文，界面这一层才翻）、一次性结果行、
//!   钩子可见的环境事实（不含口令）、以及逐条落 `tracing` 的报告。
//! - **闸门**：`DbOpGate` / `DbOpPermit` 是进程内唯一的并发上限（多页面 / 多窗口共用），
//!   许可在 `Drop` 里归还，所以取消路径不会漏放。
//!
//! 类型定义仍在 `mod.rs`。

use super::*;

impl HookMode {
    /// 稳定键（元素 id 用；不随界面语言变）。
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Script => "script",
        }
    }

    /// 展示文案。
    pub(super) fn label(self, cx: &App) -> &'static str {
        match self {
            Self::Command => tr(cx, "命令"),
            Self::Script => tr(cx, "脚本"),
        }
    }
}

impl HookRow {
    /// 建一行：`Some(hook)` 用既有值填，`None` 走新建默认（命令档 / 建连前 / 启用 / 不阻断）。
    pub(super) fn new<T>(
        id: String,
        hook: Option<&ConnectionHook>,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let (mode, line, interpreter, path) = match hook.map(|hook| &hook.action) {
            Some(HookAction::Command { line, .. }) => (
                HookMode::Command,
                line.clone(),
                String::new(),
                String::new(),
            ),
            Some(HookAction::Script {
                interpreter, path, ..
            }) => (
                HookMode::Script,
                String::new(),
                interpreter.clone(),
                path.clone(),
            ),
            None => (
                HookMode::Command,
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        let cwd = hook
            .and_then(|hook| hook.action.cwd().map(str::to_owned))
            .unwrap_or_default();
        let timeout = hook
            .and_then(|hook| hook.timeout_secs)
            .filter(|secs| *secs > 0)
            .map_or_else(String::new, |secs| secs.to_string());
        let row = Self {
            id,
            phase: hook.map_or(HookPhase::PreConnect, |hook| hook.phase),
            enabled: hook.is_none_or(|hook| hook.enabled),
            mode,
            failure: hook.map_or(HookFailure::Warn, |hook| hook.failure),
            line: text_input(window, cx, tr(cx, "例如：openvpn --config prod.ovpn")),
            interpreter: text_input(window, cx, tr(cx, "解释器，例如 bash / python3")),
            path: text_input(window, cx, tr(cx, "脚本文件的绝对路径")),
            cwd: text_input(window, cx, tr(cx, "留空用进程默认目录")),
            timeout: text_input(
                window,
                cx,
                trf(
                    cx,
                    "留空用默认 {} 秒",
                    &[&DEFAULT_HOOK_TIMEOUT_SECS.to_string()],
                ),
            ),
        };
        let seed = [
            (&row.line, line),
            (&row.interpreter, interpreter),
            (&row.path, path),
            (&row.cwd, cwd),
            (&row.timeout, timeout),
        ];
        for (target, value) in seed {
            if !value.is_empty() {
                target.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
            }
        }
        row
    }

    /// 读回成内核结构（保存时用）。
    pub(super) fn to_hook(&self, cx: &App) -> ConnectionHook {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let cwd = {
            let value = text(&self.cwd);
            (!value.is_empty()).then_some(value)
        };
        let action = match self.mode {
            HookMode::Command => HookAction::Command {
                line: text(&self.line),
                cwd,
            },
            HookMode::Script => HookAction::Script {
                interpreter: text(&self.interpreter),
                path: text(&self.path),
                cwd,
            },
        };
        // 解析不出来的超时按「没填」处理：它在界面上不是必填项，卡住保存不值当。
        let timeout_secs = text(&self.timeout)
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0);
        ConnectionHook {
            id: self.id.clone(),
            phase: self.phase,
            enabled: self.enabled,
            action,
            failure: self.failure,
            timeout_secs,
        }
    }

    /// 启用的钩子却查不到可跑的命令（保存前拦一道，免得存下一条永远 `spawn failed` 的钩子）。
    pub(super) fn is_incomplete(&self, cx: &App) -> bool {
        if !self.enabled {
            return false;
        }
        match self.mode {
            HookMode::Command => self.line.read(cx).value().trim().is_empty(),
            HookMode::Script => {
                self.interpreter.read(cx).value().trim().is_empty()
                    || self.path.read(cx).value().trim().is_empty()
            }
        }
    }
}

/// 钩子相位的中文名（提示文案里要用；落库与元素 id 走 [`HookPhase::key`]，不用它）。
pub(super) fn hook_phase_label(phase: HookPhase, cx: &App) -> &'static str {
    match phase {
        HookPhase::PreConnect => tr(cx, "建连前"),
        HookPhase::PostConnect => tr(cx, "建连后"),
        HookPhase::PreDisconnect => tr(cx, "断开前"),
        HookPhase::PostDisconnect => tr(cx, "断开后"),
    }
}

/// 钩子执行状态的中文名（内核的 [`HookStatus`] 是**英文**的，界面这一层才翻译）。
pub(super) fn hook_status_label(status: &HookStatus, cx: &App) -> String {
    match status {
        HookStatus::Ok => tr(cx, "成功").to_owned(),
        HookStatus::Failed { code: Some(code) } => {
            trf(cx, "失败（退出码 {}）", &[&code.to_string()])
        }
        HookStatus::Failed { code: None } => tr(cx, "失败（被信号终止）").to_owned(),
        HookStatus::TimedOut => tr(cx, "超时").to_owned(),
        HookStatus::SpawnFailed(_) => tr(cx, "起不来（命令或解释器不存在）").to_owned(),
    }
}

/// 一次钩子结果的一行（状态 + 耗时 + 输出首行）。
pub(super) fn hook_outcome_line(outcome: &HookOutcome, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 成功用扩展色板里的 success（它只在扩展色里有），失败用 base 的 destructive ——
    // 与通知的语义色取法是同一条口径。
    let color = if outcome.status.is_ok() {
        extended.colors.success
    } else {
        tokens.colors.destructive
    };
    let mut line = format!(
        "{} · {} ms",
        hook_status_label(&outcome.status, cx),
        outcome.duration_ms
    );
    // 只取**首行非空**输出：整段塞进表单会把行高撑爆，剩下的内容在日志里。
    if let Some(preview) = outcome
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
    {
        line.push_str(" · ");
        line.push_str(preview);
    }
    meta_text(SharedString::from(line), cx)
        .text_color(color)
        .into_any_element()
}

/// 钩子能看到的环境事实（由档案派生；**不含口令**）。
pub(super) fn hook_facts(cfg: &ConnectionConfig) -> HookContext {
    HookContext {
        connection_id: cfg.id.clone(),
        connection_name: cfg.display_name(),
        kind: cfg.kind.wire_name().to_owned(),
        host: cfg.host.clone(),
        port: cfg
            .port
            .or_else(|| cfg.kind.default_port())
            .map_or_else(String::new, |port| port.to_string()),
        user: cfg.user.clone(),
        database: cfg.database.clone(),
    }
}

/// 把一次相位报告逐条写进 `tracing` 日志（界面提示由调用方另做）。
///
/// ⚠️ 这里**不是**审计流水的落点。钩子每建立 / 拆除一次连接就把每个钩子跑一遍，
/// 全记「成功」只会把真正该看见的东西淹掉；而钩子唯一能改变连接结局的那一次
/// （挡住 / 失败）已经由连接路径记成 `Blocked` / `Failed` 了。想看「刚才那几个钩子
/// 跑出了什么」有内存里的 `hook_log` 面板，想看历史有 tracing。
pub(super) fn log_hook_report(conn_id: &str, phase: HookPhase, report: &HookReport) {
    for outcome in &report.outcomes {
        tracing::info!(
            connection = %conn_id,
            hook = %outcome.hook_id,
            phase = %phase.key(),
            command = %outcome.command,
            status = %outcome.detail(),
            duration_ms = outcome.duration_ms,
            "connection hook finished"
        );
    }
}

impl DbOpGate {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                in_flight: 0,
                waiters: VecDeque::new(),
            }),
        }
    }

    /// 取一个名额；满了就排队，等某位持有者 [`Self::release`] 时把名额转交过来。
    pub(super) async fn acquire(self: Arc<Self>) -> DbOpPermit {
        // 锁的临界区里不 await，也不搬动 `self`：先决定「直接拿走」还是「登记排队」。
        let waiting = {
            let mut state = self.state.lock().expect("db op gate lock poisoned");
            if state.in_flight < MAX_CONCURRENT_DB_OPS {
                state.in_flight += 1;
                None
            } else {
                let (tx, rx) = futures_channel::oneshot::channel();
                state.waiters.push_back(tx);
                Some(rx)
            }
        };
        if let Some(rx) = waiting {
            // 被唤醒 = 名额已转交（`in_flight` 不变）；发送端被丢弃说明闸门没了，也照跑。
            let _ = rx.await;
        }
        DbOpPermit { gate: self }
    }

    /// 放行：优先把名额**直接交给**排在最前的等待者，没人等才减计数。
    ///
    /// 等待者可能已经放弃（页面关了 / future 被取消），那个 `send` 会失败——
    /// 这时必须接着找下一个，**绝不能吞掉名额**（吞掉就等于并发上限凭空变小一格）。
    pub(super) fn release(&self) {
        let mut state = self.state.lock().expect("db op gate lock poisoned");
        while let Some(next) = state.waiters.pop_front() {
            if next.send(()).is_ok() {
                return;
            }
        }
        state.in_flight = state.in_flight.saturating_sub(1);
    }
}

impl Drop for DbOpPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// 进程内唯一的闸门（多个页面 / 多个窗口共用同一份上限）。
pub(super) fn db_op_gate() -> Arc<DbOpGate> {
    static GATE: OnceLock<Arc<DbOpGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(DbOpGate::new())).clone()
}
