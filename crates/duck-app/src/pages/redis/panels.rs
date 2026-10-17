//! 服务端面板：数据拉取（慢日志 / 客户端 / 配置 / 内存 / 监视器 / CLI）与它们的渲染。

use super::*;
use duck_components::selectable_text;

/// 终端面板的等宽族（对齐 `logs.rs` / `assistant.rs` 的同款常量）。
/// `font_family` 只收一个族名，两个平台各自保证存在的那个；缺失时回落默认 UI 字体。
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const MONO_FAMILY: &str = "Consolas";

impl RedisPage {
    /// 拉取**当前服务端面板**的数据（`load_panel_for` 的便捷形态）。
    pub(super) fn load_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_panel_for(self.panel, window, cx);
    }

    /// 拉取指定面板的数据。
    ///
    /// 「服务端」视图与值面板顶部分页**两处共存、共用同一份数据**：分页切到
    /// 「状态 / 慢日志」时也走这里（`set_value_tab` 按目标页传面板名），
    /// 不另建第二份拉取链。
    ///
    /// `Monitor` / `Cli` 不走批量拉取：前者是流、后者每次输入现发一条。
    pub(super) fn load_panel_for(
        &mut self,
        panel: ServerPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.active_session() else {
            return;
        };
        if matches!(panel, ServerPanel::Monitor | ServerPanel::Cli) {
            return;
        }
        let conn = session.conn.clone();
        // 配置面板的过滤串是**服务端 glob**（`CONFIG GET <pattern>`），交给服务端做；
        // 别的面板用不到，空串归一成 `*`。
        let pattern = {
            let text = self.config_pattern.read(cx).value().trim().to_owned();
            if text.is_empty() {
                "*".to_owned()
            } else {
                text
            }
        };
        self.panel_busy = true;
        cx.notify();
        self.spawn_redis(
            cx,
            move || -> Result<PanelData, RedisError> {
                let mut conn = lock(&conn)?;
                let mut data = PanelData::default();
                match panel {
                    ServerPanel::Info => {
                        let text = conn.info(None)?;
                        data.info_sections = parse_info_sections(&text);
                    }
                    ServerPanel::Slowlog => {
                        data.slowlog = conn.slowlog_get(SLOWLOG_PAGE)?;
                        data.slowlog_len = conn.slowlog_len().unwrap_or(0);
                    }
                    ServerPanel::Clients => data.clients = conn.client_list()?,
                    ServerPanel::Config => data.config = conn.config_get(&pattern)?,
                    ServerPanel::Memory => {
                        // 诊断报告与统计表分开取、各自兜底：老服务端可能没有 DOCTOR，
                        // 那不该把有数据的统计表一起拖垮。
                        data.memory_doctor = conn.memory_doctor().unwrap_or_default();
                        data.memory_stats = conn.memory_stats()?;
                    }
                    ServerPanel::Monitor | ServerPanel::Cli => {}
                }
                Ok(data)
            },
            move |this, result, window, cx| {
                this.panel_busy = false;
                match result {
                    Ok(data) => this.merge_panel_data(panel, data),
                    Err(error) => notify::error(
                        trf(cx, "读取服务端信息失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    ),
                }
                cx.notify();
            },
        );
    }

    /// 把一次拉取的结果并进 `panel_data`。
    ///
    /// **只覆盖本面板的字段**：拉取期间用户可能已经切到别的面板，
    /// 整块赋值会把已经加载好的数据清掉。
    pub(super) fn merge_panel_data(&mut self, panel: ServerPanel, data: PanelData) {
        match panel {
            ServerPanel::Info => self.panel_data.info_sections = data.info_sections,
            ServerPanel::Slowlog => {
                self.panel_data.slowlog = data.slowlog;
                self.panel_data.slowlog_len = data.slowlog_len;
            }
            ServerPanel::Clients => self.panel_data.clients = data.clients,
            ServerPanel::Config => self.panel_data.config = data.config,
            ServerPanel::Memory => {
                self.panel_data.memory_doctor = data.memory_doctor;
                self.panel_data.memory_stats = data.memory_stats;
            }
            ServerPanel::Monitor | ServerPanel::Cli => {}
        }
    }

    /// 清空慢日志（`SLOWLOG RESET`）。
    ///
    /// 走 `ServerAdmin` 档**并强制二次确认**：慢日志是排查线上事故的唯一线索，
    /// 误清一次就没了（这一点与「用户自己关掉写入确认」的开关无关）。
    pub(super) fn reset_slowlog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_or_run(
            Capability::ServerAdmin,
            PendingAction::SlowlogReset,
            window,
            cx,
        );
    }

    /// 真正清空慢日志。
    pub(super) fn reset_slowlog_at(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.check(Capability::ServerAdmin) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.slowlog_reset()
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "慢日志已清空"), window, cx);
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "清空慢日志失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 强制断开一条客户端连接（`CLIENT KILL ADDR`）。
    pub(super) fn kill_client(
        &mut self,
        addr: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_or_run(
            Capability::ServerAdmin,
            PendingAction::ClientKill { addr },
            window,
            cx,
        );
    }

    /// 真正断开一条客户端连接。
    pub(super) fn kill_client_at(
        &mut self,
        addr: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(Capability::ServerAdmin) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = addr.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.client_kill_addr(&addr)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    notify::success(trf(cx, "已断开 {}", &[&echo]), window, cx);
                    this.load_panel(window, cx);
                }
                Ok(false) => {
                    notify::warning(trf(cx, "未找到客户端 {}", &[&echo]), window, cx);
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "断开客户端失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 查询单个键的内存占用（`MEMORY USAGE`）。
    pub(super) fn query_memory_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.memory_key.read(cx).value().trim().to_owned();
        if key.is_empty() {
            notify::warning(tr(cx, "请填写键名"), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<Option<u64>, RedisError> {
                let mut conn = lock(&conn)?;
                conn.memory_usage(&key)
            },
            move |this, result, window, cx| match result {
                Ok(bytes) => {
                    this.panel_data.memory_usage = Some((echo, bytes));
                    cx.notify();
                }
                Err(error) => notify::error(
                    trf(cx, "查询内存占用失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }

    /// 提交终端输入（多行会按行拆开、依次执行）。
    pub(super) fn submit_cli(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.cli_input.read(cx).value().to_string();
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if lines.is_empty() {
            return;
        }
        self.cli_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        for line in &lines {
            // 历史去重后置顶：同一条命令反复执行只占一行。
            self.cli_history.retain(|existing| existing != line);
            self.cli_history.push(line.clone());
        }
        while self.cli_history.len() > CLI_HISTORY_MAX {
            self.cli_history.remove(0);
        }
        self.cli_queue.extend(lines);
        self.pump_cli_queue(window, cx);
    }

    /// 串行泵出终端队列里的一条命令。
    ///
    /// **一次一条**：所有命令共用同一条阻塞连接，并发下发会让应答交错，
    /// 终端上就分不清哪条回复对应哪条命令了。
    pub(super) fn pump_cli_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return; // 上一条还在跑；它回主线程时会再泵一次
        }
        let Some(line) = self.cli_queue.pop_front() else {
            return;
        };
        // 闸门：`RawCommand` 是写动作，SafeMode 下一律拒。
        // 终端在客户端**无法判定**一条任意命令是否只读，所以整条通道按写口径管。
        if let Err(reason) = self.check(Capability::RawCommand) {
            self.cli_lines.push(CliLine {
                command: true,
                error: false,
                text: line,
            });
            self.cli_lines.push(CliLine {
                command: false,
                error: true,
                text: deny_message(reason, cx).to_owned(),
            });
            self.trim_cli_lines();
            cx.notify();
            return;
        }
        // `KEYS` 不被拦，但要提醒它有 `SCAN` 这个更安全的替代。
        if confirm_kind(&line) == Some(ConfirmKind::PreferScan) {
            notify::warning(
                tr(cx, "KEYS 会遍历整个键空间，可能阻塞服务端；建议改用 SCAN。"),
                window,
                cx,
            );
        }
        self.cli_lines.push(CliLine {
            command: true,
            error: false,
            text: line.clone(),
        });
        self.trim_cli_lines();
        // 不可逆的破坏性命令（`FLUSHALL` / `SHUTDOWN` / `CONFIG SET` …）**无条件**二次确认：
        // 这是终端与结构化编辑最大的区别——后者只动用户看得见的那一个键。
        let confirm = is_destructive(&line)
            || self
                .active_config()
                .is_some_and(|cfg| cfg.require_confirm_writes);
        if confirm {
            self.confirm_or_run(
                Capability::RawCommand,
                PendingAction::RawCommand { line },
                window,
                cx,
            );
            cx.notify();
            return;
        }
        self.run_cli_line_at(line, window, cx);
    }

    /// 真正下发一条原始命令。
    ///
    /// `confirm_or_run` 之外的入口（含未来的）也不能绕过锁写约束，所以这里**再过一次闸门**。
    pub(super) fn run_cli_line_at(
        &mut self,
        line: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(Capability::RawCommand) {
            self.cli_lines.push(CliLine {
                command: false,
                error: true,
                text: deny_message(reason, cx).to_owned(),
            });
            self.trim_cli_lines();
            cx.notify();
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<String, RedisError> {
                let mut conn = lock(&conn)?;
                conn.raw_command_text(&line)
            },
            move |this, result, window, cx| {
                match result {
                    Ok(text) => this.cli_lines.push(CliLine {
                        command: false,
                        error: false,
                        text,
                    }),
                    Err(error) => this.cli_lines.push(CliLine {
                        command: false,
                        error: true,
                        text: error.to_string(),
                    }),
                }
                this.trim_cli_lines();
                // 队列里还有就接着来（`spawn_redis` 已把 `busy` 降下来）。
                this.pump_cli_queue(window, cx);
                cx.notify();
            },
        );
    }

    /// 终端输出裁剪到上限（丢最旧的），并把视口贴到底部。
    pub(super) fn trim_cli_lines(&mut self) {
        if self.cli_lines.len() > CLI_LINES_MAX {
            let excess = self.cli_lines.len() - CLI_LINES_MAX;
            self.cli_lines.drain(0..excess);
        }
        self.cli_scroll.scroll_to_bottom();
    }

    /// 开启 `MONITOR`。
    ///
    /// 建连与读循环都在**专用连接**上（设计文档 §4.7）：`MONITOR` 一旦开启，
    /// 该连接上再也不能发别的命令，所以绝不能复用主连接。
    pub(super) fn start_monitor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.monitor_running {
            return;
        }
        let Some(cfg) = self.active_config().cloned() else {
            return;
        };
        // 探测明确说没有 `MONITOR` 就别开——省得开出来一个永远空的面板。
        if let Some(features) = self.features()
            && !features.supports("monitor")
        {
            notify::warning(tr(cx, "本服务端不支持 MONITOR。"), window, cx);
            return;
        }
        self.monitor_error = None;
        self.monitor_lines.clear();
        self.monitor_running = true;
        self.monitor_generation = self.monitor_generation.wrapping_add(1);
        let stop = Arc::new(AtomicBool::new(false));
        self.monitor_stop = Some(stop.clone());
        self.monitor_task = Some(Self::spawn_monitor_poll(
            cx,
            cfg,
            stop,
            self.monitor_generation,
        ));
        self.status = tr(cx, "实时监控已开启").into();
        cx.notify();
    }

    /// 停止 `MONITOR`（只置停止位，读循环自己收工）。
    pub(super) fn stop_monitor(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.monitor_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if self.monitor_running {
            self.monitor_running = false;
            self.status = tr(cx, "实时监控已停止").into();
            cx.notify();
        }
    }

    /// 清空监控输出（不停会话）。
    pub(super) fn clear_monitor(&mut self, cx: &mut Context<Self>) {
        self.monitor_lines.clear();
        self.monitor_error = None;
        cx.notify();
    }

    /// `MONITOR` 的轮询循环。
    ///
    /// 结构是「前台节拍 + 后台批读」：阻塞读丢进 `background_executor`，
    /// 会话所有权往返一次（`(session, batch)`）——这样读阻塞不会冻住界面线程，
    /// 也不需要另开线程或共享锁。
    pub(super) fn spawn_monitor_poll(
        cx: &mut Context<Self>,
        cfg: RedisConfig,
        stop: Arc<AtomicBool>,
        generation: u64,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            // 建连本身是阻塞的（TCP + AUTH + MONITOR 握手），同样丢后台。
            let started = cx
                .background_executor()
                .spawn(async move {
                    let password = load_password(&cfg.id).ok().flatten();
                    MonitorSession::start(&cfg, password.as_deref())
                })
                .await;
            let mut session = match started {
                Ok(session) => session,
                Err(error) => {
                    let message = error.to_string();
                    let _ = this.update(cx, |page, cx| {
                        if page.monitor_generation == generation {
                            page.monitor_running = false;
                        }
                        page.monitor_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };

            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor().timer(MONITOR_TICK).await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // 批读（最多 `POLL_TIMEOUT`）挪到后台；会话所有权往返。
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = session.poll(MONITOR_BATCH);
                        (session, batch)
                    })
                    .await;
                session = back;
                match batch {
                    Ok(lines) => {
                        if lines.is_empty() {
                            continue;
                        }
                        let alive = this
                            .update(cx, |page, cx| {
                                page.monitor_lines.extend(lines);
                                while page.monitor_lines.len() > MONITOR_LINES_MAX {
                                    page.monitor_lines.pop_front();
                                }
                                page.monitor_scroll.scroll_to_bottom();
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return; // 视图销毁，任务随之结束
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = this.update(cx, |page, cx| {
                            if page.monitor_generation == generation {
                                page.monitor_running = false;
                            }
                            page.monitor_error = Some(message);
                            cx.notify();
                        });
                        return;
                    }
                }
            }

            // 正常收工（用户停止 / 切走视图）：只在仍属本轮时回落状态。
            let _ = this.update(cx, |page, cx| {
                if page.monitor_generation == generation && page.monitor_running {
                    page.monitor_running = false;
                    cx.notify();
                }
            });
        })
    }

    // ------------------------------------------------------------------
    // 发布 / 订阅（SUBSCRIBE，专用连接）。
    //
    // 与 `MONITOR` 同一条纪律：专用连接、拉模型、只置停止位。区别只在
    // 「频道由用户现填、随时可换」——换频道 = 停旧会话、开新会话。
    // ------------------------------------------------------------------

    /// 输入框里的频道清单（空白分隔、去空去重；保序）。
    fn pubsub_channels(&self, cx: &App) -> Vec<String> {
        let text = self.pubsub_channel.read(cx).value().to_string();
        let mut channels: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        channels.dedup();
        channels
    }

    /// 开启订阅。
    ///
    /// 建连与读循环都在**专用连接**上（同 `MONITOR`）：订阅一旦开启，
    /// 该连接上再也不能发别的命令。
    pub(super) fn start_pubsub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pubsub_running {
            return;
        }
        let Some(cfg) = self.active_config().cloned() else {
            return;
        };
        let channels = self.pubsub_channels(cx);
        if channels.is_empty() {
            notify::warning(tr(cx, "请先填写要订阅的频道"), window, cx);
            return;
        }
        self.pubsub_error = None;
        self.pubsub_running = true;
        self.pubsub_generation = self.pubsub_generation.wrapping_add(1);
        let stop = Arc::new(AtomicBool::new(false));
        self.pubsub_stop = Some(stop.clone());
        self.pubsub_task = Some(Self::spawn_pubsub_poll(
            cx,
            cfg,
            channels,
            stop,
            self.pubsub_generation,
        ));
        self.status = tr(cx, "订阅已开启").into();
        cx.notify();
    }

    /// 停止订阅（只置停止位，读循环自己收工）。
    pub(super) fn stop_pubsub(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.pubsub_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if self.pubsub_running {
            self.pubsub_running = false;
            self.status = tr(cx, "订阅已停止").into();
            cx.notify();
        }
    }

    /// 清空订阅输出（不停会话）。
    pub(super) fn clear_pubsub(&mut self, cx: &mut Context<Self>) {
        self.pubsub_lines.clear();
        self.pubsub_error = None;
        cx.notify();
    }

    /// 订阅的轮询循环（结构与 [`Self::spawn_monitor_poll`] 完全同构）。
    pub(super) fn spawn_pubsub_poll(
        cx: &mut Context<Self>,
        cfg: RedisConfig,
        channels: Vec<String>,
        stop: Arc<AtomicBool>,
        generation: u64,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move {
                    let password = load_password(&cfg.id).ok().flatten();
                    PubSubSession::start(&cfg, password.as_deref(), &channels)
                })
                .await;
            let mut session = match started {
                Ok(session) => session,
                Err(error) => {
                    let message = error.to_string();
                    let _ = this.update(cx, |page, cx| {
                        if page.pubsub_generation == generation {
                            page.pubsub_running = false;
                        }
                        page.pubsub_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            };

            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor().timer(MONITOR_TICK).await;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = session.poll(PUBSUB_BATCH);
                        (session, batch)
                    })
                    .await;
                session = back;
                match batch {
                    Ok(messages) => {
                        if messages.is_empty() {
                            continue;
                        }
                        let alive = this
                            .update(cx, |page, cx| {
                                let now = now_secs();
                                for message in messages {
                                    page.pubsub_lines.push_back(PubSubLine {
                                        channel: message.channel,
                                        payload: message.payload,
                                        at: now,
                                    });
                                }
                                while page.pubsub_lines.len() > PUBSUB_LINES_MAX {
                                    page.pubsub_lines.pop_front();
                                }
                                page.pubsub_scroll.scroll_to_bottom();
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            return;
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = this.update(cx, |page, cx| {
                            if page.pubsub_generation == generation {
                                page.pubsub_running = false;
                            }
                            page.pubsub_error = Some(message);
                            cx.notify();
                        });
                        return;
                    }
                }
            }

            let _ = this.update(cx, |page, cx| {
                if page.pubsub_generation == generation && page.pubsub_running {
                    page.pubsub_running = false;
                    cx.notify();
                }
            });
        })
    }

    /// 「发布/订阅」面板：频道输入 + 订阅开关 + 消息流。
    pub(super) fn render_panel_pubsub(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let running = self.pubsub_running;

        // 顶部一行：频道输入 + 订阅 / 停止 + 清空。
        let mut controls = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(panel_input_box(&self.pubsub_channel, cx))
            .child(
                button(
                    "redis-pubsub-toggle",
                    if running {
                        tr(cx, "停止")
                    } else {
                        tr(cx, "订阅")
                    },
                    if running {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Primary
                    },
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if this.pubsub_running {
                                this.stop_pubsub(cx);
                            } else {
                                this.start_pubsub(window, cx);
                            }
                        });
                    }
                }),
            )
            .child(
                button(
                    "redis-pubsub-clear",
                    tr(cx, "清空"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.pubsub_lines.is_empty())
                .on_click({
                    let view = view.clone();
                    move |_, _window, cx| {
                        let _ = view.update(cx, |this, cx| this.clear_pubsub(cx));
                    }
                }),
            );

        // 状态行：在跑 / 出错 / 空闲各说各的实话。
        let status_line = if let Some(error) = &self.pubsub_error {
            meta_text(trf(cx, "订阅中断：{}", &[error]), cx)
        } else if running {
            meta_text(tr(cx, "订阅中（专用连接；离开本页自动停止）。"), cx)
        } else {
            meta_text(tr(cx, "未订阅。填好频道后点「订阅」。"), cx)
        };

        // 消息流：滚动容器 + 每条「频道 · 载荷 · 时刻」。空态给指引。
        let lines = self.pubsub_lines.clone();
        let body = if lines.is_empty() {
            panel_empty(tr(cx, "还没有消息。订阅后，推送会实时出现在这里。"), cx)
        } else {
            let mut flow = v_flex().w_full().gap(tokens.spacing.xxs);
            for line in lines {
                flow = flow.child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.xs)
                        .py(tokens.spacing.xxs)
                        .rounded(theme.components().input_radius)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(PANEL_LABEL_WIDTH / 2.))
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(selectable_text(SharedString::from(line.channel))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(selectable_text(SharedString::from(line.payload))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(selectable_text(SharedString::from(relative_age(
                                    line.at,
                                    now_secs(),
                                    cx,
                                )))),
                        ),
                );
            }
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .id("redis-pubsub-log")
                .overflow_y_scroll()
                .p(tokens.spacing.sm)
                .track_scroll(&self.pubsub_scroll)
                .child(flow)
                .into_any_element()
        };

        controls = controls.child(div().flex_1());
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(controls)
            .child(status_line)
            .child(body)
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 键空间。
    // ------------------------------------------------------------------
}

impl RedisPage {
    /// 服务端观测：面板页签 + 面板动作 + 面板正文。
    pub(super) fn render_server_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let labels: Vec<SharedString> = ServerPanel::ALL
            .iter()
            .map(|panel| SharedString::from(panel.label(cx)))
            .collect();
        let selected = ServerPanel::ALL
            .iter()
            .position(|panel| *panel == self.panel)
            .unwrap_or(0);

        let tabs = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(segmented_tabs(
                "redis-server-panels",
                labels,
                selected,
                move |index, window, cx| {
                    let Some(panel) = ServerPanel::ALL.get(index).copied() else {
                        return;
                    };
                    let _ = view.update(cx, |this, cx| this.set_server_panel(panel, window, cx));
                },
                cx,
            ))
            .child(div().flex_1())
            .children(self.render_panel_actions(cx));

        // 设计文档 §4.7 的硬口径：缺什么命令就灰显并说明原因，不做品牌特判。
        let missing = self.panel_missing(self.panel);
        let body: AnyElement = if missing.is_empty() {
            match self.panel {
                ServerPanel::Info => self.render_panel_info(cx),
                ServerPanel::Slowlog => self.render_panel_slowlog(cx),
                ServerPanel::Clients => self.render_panel_clients(cx),
                ServerPanel::Config => self.render_panel_config(cx),
                ServerPanel::Memory => self.render_panel_memory(cx),
                ServerPanel::Monitor => self.render_panel_monitor(cx),
                ServerPanel::Cli => self.render_panel_cli(cx),
            }
        } else {
            let commands = missing.join(" / ");
            panel_empty(
                trf(
                    cx,
                    "本服务端没有 {} 命令，无法显示「{}」面板。",
                    &[&commands, self.panel.label(cx)],
                ),
                cx,
            )
        };

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(tabs)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }

    /// 面板级动作（刷新 / 开始停止 / 清空）。
    pub(super) fn render_panel_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.panel {
            ServerPanel::Monitor => self.monitor_actions(cx),
            ServerPanel::Cli => self.cli_actions(cx),
            _ => {
                let theme = active_theme(cx);
                let icons = theme.extended().icon;
                let view = self.view.clone();
                vec![
                    toolbar_action_button(
                        "redis-panel-refresh",
                        tr(cx, "刷新"),
                        Icon::new(DuckIcon::RotateCw).size(icons.md),
                        false,
                        self.panel_busy,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.load_panel(window, cx));
                        }
                    })
                    .into_any_element(),
                ]
            }
        }
    }

    /// 值面板分页（KeyValueTab）对应的面板级动作。
    ///
    /// 值面板与「服务端」视图**共用正文渲染**（`render_panel_monitor` 等），但
    /// `self.panel` 只跟服务端视图走，这里必须按**值分页**给动作——否则监控页
    /// 的空态提示「点右上角开始」会落空（值面板右上角没按钮）。状态 / 慢日志
    /// 进页即自动拉取、发布订阅的按钮在面板内部，都不需要这里给。
    pub(super) fn render_value_tab_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.value_tab {
            KeyValueTab::Monitor => self.monitor_actions(cx),
            KeyValueTab::Cli => self.cli_actions(cx),
            _ => Vec::new(),
        }
    }

    /// 监控页的动作：开始 / 停止 + 清空。
    fn monitor_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = self.view.clone();
        let running = self.monitor_running;
        vec![
            button(
                "redis-monitor-toggle",
                if running {
                    tr(cx, "停止")
                } else {
                    tr(cx, "开始")
                },
                if running {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Primary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this.monitor_running {
                            this.stop_monitor(cx);
                        } else {
                            this.start_monitor(window, cx);
                        }
                    });
                }
            })
            .into_any_element(),
            button(
                "redis-monitor-clear",
                tr(cx, "清空"),
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(self.monitor_lines.is_empty())
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.clear_monitor(cx));
                }
            })
            .into_any_element(),
        ]
    }

    /// 命令行页的动作：清空输出。
    fn cli_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = self.view.clone();
        vec![
            button(
                "redis-cli-clear",
                tr(cx, "清空"),
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(self.cli_lines.is_empty())
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.cli_lines.clear();
                        this.cli_scroll.scroll_to_bottom();
                        cx.notify();
                    });
                }
            })
            .into_any_element(),
        ]
    }

    /// 「概览」面板：`INFO` 分节浏览。
    pub(super) fn render_panel_info(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        if self.panel_data.info_sections.is_empty() {
            return panel_empty(tr(cx, "点右上角「刷新」读取 INFO。"), cx);
        }
        let mut body = v_flex().w_full().gap(tokens.spacing.sm);
        for (section, pairs) in &self.panel_data.info_sections {
            let title = if section.is_empty() {
                tr(cx, "其它")
            } else {
                section.as_str()
            };
            let mut card = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(title, cx));
            for (name, value) in pairs {
                card = card.child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.sm)
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(PANEL_LABEL_WIDTH))
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(selectable_text(SharedString::from(value.clone()))),
                        ),
                );
            }
            body = body.child(card);
        }
        panel_scroll("redis-panel-info", body.into_any_element(), cx)
    }

    /// 「慢日志」面板：`SLOWLOG GET`。
    pub(super) fn render_panel_slowlog(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_reset = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ServerAdmin));

        if self.panel_data.slowlog.is_empty() {
            return panel_empty(tr(cx, "没有慢日志记录。"), cx);
        }
        let now = now_secs();
        let mut body = v_flex().w_full().gap(tokens.spacing.xs).child(meta_text(
            trf(
                cx,
                "共 {} 条，显示最近 {} 条。",
                &[
                    &self.panel_data.slowlog_len.to_string(),
                    &self.panel_data.slowlog.len().to_string(),
                ],
            ),
            cx,
        ));
        for entry in &self.panel_data.slowlog {
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(SLOWLOG_META_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(format!(
                                "#{}  {} ms  {}",
                                entry.id(),
                                entry.duration_ms(),
                                relative_age(entry.timestamp(), now, cx),
                            )))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(entry.command_line()))),
                    ),
            );
        }
        body = body.child(
            h_flex().w_full().justify_end().child(
                button(
                    "redis-slowlog-reset",
                    tr(cx, "清空慢日志"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .disabled(!can_reset)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.reset_slowlog(window, cx));
                    }
                }),
            ),
        );
        panel_scroll("redis-panel-slowlog", body.into_any_element(), cx)
    }

    /// 「客户端」面板：`CLIENT LIST` + `CLIENT KILL`。
    pub(super) fn render_panel_clients(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_kill = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ServerAdmin));

        if self.panel_data.clients.is_empty() {
            return panel_empty(tr(cx, "没有客户端连接。"), cx);
        }
        let mut body = v_flex().w_full().gap(px(1.));
        for client in &self.panel_data.clients {
            let addr = client.addr().to_owned();
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xs)
                    .py(tokens.spacing.xxs)
                    .rounded(theme.components().input_radius)
                    .border_b(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_ADDR_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(addr.clone()))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(
                                client.user().to_owned(),
                            ))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(trf(
                                cx,
                                "库 {}",
                                &[&client.db().to_string()],
                            ))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(CLIENT_SHORT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(trf(
                                cx,
                                "空闲 {} 秒",
                                &[&client.idle().to_string()],
                            ))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(client.cmd().to_owned()))),
                    )
                    .child(
                        button(
                            SharedString::from(format!("redis-client-kill-{}", client.id())),
                            tr(cx, "断开"),
                            ButtonVariant::Destructive,
                            cx,
                        )
                        .disabled(!can_kill)
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let addr = addr.clone();
                                let _ =
                                    view.update(cx, |this, cx| this.kill_client(addr, window, cx));
                            }
                        }),
                    ),
            );
        }
        panel_scroll("redis-panel-clients", body.into_any_element(), cx)
    }

    /// 「配置」面板：`CONFIG GET` 列表 + `CONFIG SET` 编辑。
    pub(super) fn render_panel_config(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let can_write = self
            .gate()
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::ConfigWrite));

        let filter_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(panel_input_box(&self.config_pattern, cx))
            .child(
                button(
                    "redis-config-filter",
                    tr(cx, "过滤"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_panel(window, cx));
                    }
                }),
            );

        let edit_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(panel_input_box(&self.config_name, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(panel_input_box(&self.config_value, cx)),
            )
            .child(
                button(
                    "redis-config-save",
                    tr(cx, "写入"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!can_write)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.save_config(window, cx));
                    }
                }),
            );

        let mut list = v_flex().w_full().gap(px(1.));
        if self.panel_data.config.is_empty() {
            list = list.child(meta_text(tr(cx, "没有匹配的配置项。"), cx));
        }
        for entry in &self.panel_data.config {
            let name = entry.name().to_owned();
            let click_target = entry.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("redis-config-{name}")))
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.xxs)
                    .py(px(1.))
                    .rounded(theme.components().input_radius)
                    .role(Role::Button)
                    .aria_label(SharedString::from(trf(cx, "把 {} 回填到编辑框", &[&name])))
                    .cursor_pointer()
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .on_click({
                        let view = view.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let entry = click_target.clone();
                            let _ =
                                view.update(cx, |this, cx| this.pick_config(&entry, window, cx));
                        }
                    })
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(PANEL_LABEL_WIDTH))
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(
                                entry.value().to_owned(),
                            ))),
                    ),
            );
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .child(filter_row)
            .child(edit_row)
            .child(
                div()
                    .id("redis-config-list")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    /// 「内存」面板：`MEMORY DOCTOR` / `MEMORY STATS` / `MEMORY USAGE`。
    pub(super) fn render_panel_memory(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let doctor = if self.panel_data.memory_doctor.trim().is_empty() {
            tr(cx, "服务端没有给出诊断报告。").to_owned()
        } else {
            self.panel_data.memory_doctor.clone()
        };

        let usage_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(panel_input_box(&self.memory_key, cx))
            .child(
                button(
                    "redis-memory-usage",
                    tr(cx, "查询占用"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.query_memory_usage(window, cx));
                    }
                }),
            );

        let mut body = v_flex().w_full().gap(tokens.spacing.sm).child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(tr(cx, "诊断"), cx))
                .child(body_text(SharedString::from(doctor), cx)),
        );
        body = body.child(usage_row);
        if let Some((key, bytes)) = &self.panel_data.memory_usage {
            let text = match bytes {
                Some(bytes) => trf(cx, "{} 占用 {}", &[key, &duck_theme::bytes(*bytes)]),
                None => trf(cx, "{} 不存在。", &[key]),
            };
            body = body.child(meta_text(text, cx));
        }
        if !self.panel_data.memory_stats.is_empty() {
            let mut stats = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(tr(cx, "统计"), cx));
            for (name, value) in &self.panel_data.memory_stats {
                stats = stats.child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.sm)
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(PANEL_LABEL_WIDTH))
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(selectable_text(SharedString::from(value.clone()))),
                        ),
                );
            }
            body = body.child(stats);
        }
        panel_scroll("redis-panel-memory", body.into_any_element(), cx)
    }

    /// 「实时监控」面板：`MONITOR` 专用连接上的增量日志。
    pub(super) fn render_panel_monitor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        if !self.monitor_running && self.monitor_lines.is_empty() {
            let hint = match &self.monitor_error {
                Some(error) => SharedString::from(error.clone()),
                None => {
                    SharedString::from(tr(cx, "点右上角「开始」，在一条专用连接上开启实时命令流。"))
                }
            };
            return panel_empty(hint, cx);
        }

        let mut lines = v_flex().w_full().gap(px(1.));
        if let Some(error) = &self.monitor_error {
            lines = lines.child(
                div()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.destructive)
                    .child(selectable_text(SharedString::from(error.clone()))),
            );
        }
        for line in &self.monitor_lines {
            lines = lines.child(
                div()
                    .w_full()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(selectable_text(SharedString::from(line.clone()))),
            );
        }

        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("redis-monitor-output")
            .overflow_y_scroll()
            .track_scroll(&self.monitor_scroll)
            .p(tokens.spacing.sm)
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.background)
            .child(lines)
            .into_any_element()
    }

    /// 「终端」面板：redis-cli 式的**一体化终端**。
    ///
    /// 单个终端框 = 滚动输出区 + 框内底部的 prompt 行（`host:port[db]>` 前缀 +
    /// 无框输入，hairline 分隔）。回车即执行（`submit_cli`；Enter 订阅在
    /// `mod.rs` 的输入事件监听里），不设独立「执行」按钮。历史 chips 保留在
    /// 框下方，点一下回填输入框。
    pub(super) fn render_panel_cli(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        // redis-cli 风格 prompt：`host:port>`，逻辑库非 0 时补 `[db]`（redis-cli 同款）。
        let prompt: SharedString = self.active_config().map_or_else(
            || SharedString::from("redis>"),
            |cfg| {
                SharedString::from(if cfg.db > 0 {
                    format!("{}:{}[{}]>", cfg.host, cfg.effective_port(), cfg.db)
                } else {
                    format!("{}:{}>", cfg.host, cfg.effective_port())
                })
            },
        );

        let mut output = v_flex().w_full().gap(px(1.));
        if self.cli_lines.is_empty() {
            output = output.child(
                div()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(tokens.colors.muted_foreground)
                    .child(selectable_text(SharedString::from(tr(
                        cx,
                        "# 输入命令后回车执行；破坏性命令（FLUSHALL 等）会先弹确认框。",
                    )))),
            );
        }
        for line in &self.cli_lines {
            let color = if line.error {
                tokens.colors.destructive
            } else if line.command {
                tokens.colors.muted_foreground
            } else {
                tokens.colors.foreground
            };
            let text = if line.command {
                format!("{} {}", prompt, line.text)
            } else {
                line.text.clone()
            };
            output = output.child(
                div()
                    .w_full()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(color)
                    .child(selectable_text(SharedString::from(text))),
            );
        }

        let output_scroll = div()
            .id("redis-cli-output")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.cli_scroll)
            .p(tokens.spacing.sm)
            .child(output);

        // prompt 行：贴在终端框底部，hairline 与输出区分隔；输入框无边框透明，
        // 视觉上就是「在终端里打字」。
        let prompt_row = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .text_color(tokens.colors.muted_foreground)
                    .child(selectable_text(prompt)),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.cli_input)
                        .appearance(false)
                        .bordered(false)
                        .w_full(),
                ),
            );

        let terminal = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .font_family(MONO_FAMILY)
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.background)
            .child(output_scroll)
            .child(prompt_row);

        // 历史：最近若干条做成可点 chips，点一下回填输入框（比手打快得多）。
        let mut history_row = h_flex()
            .flex_none()
            .w_full()
            .flex_wrap()
            .gap(tokens.spacing.xxs);
        for (index, command) in self
            .cli_history
            .iter()
            .rev()
            .take(CLI_HISTORY_CHIPS)
            .enumerate()
        {
            let label: String = command.chars().take(CLI_HISTORY_CHIP_CHARS).collect();
            let fill = command.clone();
            history_row = history_row.child(
                button(
                    SharedString::from(format!("redis-cli-history-{index}")),
                    SharedString::from(label),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let fill = fill.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.cli_input
                                .update(cx, |input, cx| input.set_value(fill, window, cx));
                        });
                    }
                }),
            );
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .child(terminal)
            .children((!self.cli_history.is_empty()).then_some(history_row))
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // 右栏：键详情与值。
    // ------------------------------------------------------------------
}
