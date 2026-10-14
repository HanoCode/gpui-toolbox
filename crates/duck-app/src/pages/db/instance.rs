use super::*;

impl InstanceTab {
    /// 四档全量（左栏渲染顺序）。
    pub(super) const ALL: [Self; 4] = [
        Self::Dashboard,
        Self::Overview,
        Self::Metrics,
        Self::Inspector,
    ];

    /// 左栏入口文案（`tr` 回 `&'static str`，与全站取词一致）。
    pub(super) fn heading(self, cx: &App) -> &'static str {
        match self {
            Self::Dashboard => tr(cx, "仪表盘"),
            Self::Overview => tr(cx, "实例概览"),
            Self::Metrics => tr(cx, "实例指标"),
            Self::Inspector => tr(cx, "实例检查器"),
        }
    }

    /// 元素 id / 展开键用的稳定英文键（**不翻译**，见 [`TreeCategory::key`] 同一理由）。
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Overview => "overview",
            Self::Metrics => "metrics",
            Self::Inspector => "inspector",
        }
    }

    /// 需要读服务器数据的那几档（仪表盘只用手上已有的数据，不取）。
    pub(super) const fn fetches_server(self) -> bool {
        !matches!(self, Self::Dashboard)
    }
}

impl MetricHistory {
    /// 记一次采样（超过上限丢最旧的）。
    pub(super) fn push(&mut self, at: i64, metrics: Vec<InstanceMetric>) {
        self.samples.push_back((at, metrics));
        while self.samples.len() > METRIC_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// 采了几次。
    pub(super) fn len(&self) -> usize {
        self.samples.len()
    }

    /// 可画的指标名（按**最近一次**采样的顺序去重）。
    ///
    /// 用最新一轮而不是「所有轮次的并集」：指标清单本身会随引擎版本变（白名单改了），
    /// 并集会把早就不报的项一直挂在图上。
    pub(super) fn keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some((_, latest)) = self.samples.back() {
            for metric in latest {
                if !keys.contains(&metric.key) {
                    keys.push(metric.key.clone());
                }
            }
        }
        keys
    }

    /// 某个指标的时间序列（`(采样时刻, 值)`）。
    ///
    /// 某一轮没报这个指标就**跳过**那一轮，不补 0：补 0 会在图上画出一段实际不存在的
    /// 「跌到底」，比断开更难解释。
    pub(super) fn series(&self, key: &str) -> Vec<(i64, f64)> {
        self.samples
            .iter()
            .filter_map(|(at, metrics)| {
                metrics
                    .iter()
                    .find(|metric| metric.key == key)
                    .map(|metric| (*at, metric.value))
            })
            .collect()
    }

    /// 缓冲覆盖的时长（秒；不足两次采样为 0）。
    pub(super) fn span_secs(&self) -> i64 {
        match (self.samples.front(), self.samples.back()) {
            (Some((first, _)), Some((last, _))) => (last - first).max(0),
            _ => 0,
        }
    }
}

/// 实例信息页入口的图标（左栏与页面标题共用，两处不会画成两个样子）。
pub(super) fn instance_tab_icon(tab: InstanceTab) -> DuckIcon {
    match tab {
        InstanceTab::Dashboard => DuckIcon::Gauge,
        InstanceTab::Overview => DuckIcon::Server,
        InstanceTab::Metrics => DuckIcon::DatabaseZap,
        InstanceTab::Inspector => DuckIcon::ListOrdered,
    }
}

/// 实例页的一张卡：小标题 + 卡片正文（与右栏「索引 / 外键 / 触发器」卡同形）。
///
/// `title` 收 `Into<SharedString>`：仪表盘与概览传中文（进 EN 表），指标 / 检查器传
/// `trf` 拼出来的计数文案（不带方括号的静态串 → 用 `&'static str` 反而要编常量）。
pub(super) fn instance_card(
    title: impl Into<SharedString>,
    body: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .child(caption_heading(title, cx))
        .child(
            card(cx)
                .w_full()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.sm)
                .child(body),
        )
        .into_any_element()
}

/// 实例页的一句提示（空态 / 取数中；比整块 `empty_state` 轻）。
pub(super) fn instance_hint(message: &'static str, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    card(cx)
        .w_full()
        .px(tokens.spacing.lg)
        .py(tokens.spacing.sm)
        .child(meta_text(message, cx))
        .into_any_element()
}

impl DatabasePage {
    /// 打开实例信息页（左栏连接节点下那四个入口）——现在落在**实例标签**里。
    ///
    /// 复用策略与库 / 数据标签同款：同连接同档已开就只激活（快照原样装回），
    /// 没有才新开。连接 id 由调用方显式传入 —— 入口行长在**某条**连接下面，
    /// 不能拿「当前活跃连接」凑数（左栏完全可能在浏览另一条连接）。
    ///
    /// 取数策略：**点开才取**，不在建连时预取——建连那一刻用户要的是「有哪些库」，
    /// 让他多等三条服务器探询语句没有道理。已取过的档位不重复取（`refresh` 会重取）。
    pub(super) fn open_instance_tab(
        &mut self,
        conn_id: &str,
        tab: InstanceTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let kind = TabKind::Instance(tab);
        if let Some(existing) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&existing, window, cx);
            return;
        }
        // 新标签装的是干净快照：选中行 / 行编辑器 / 指标聚焦都不从上一个标签带过来
        // （`spawn_tab_for` 已 `install_editor_state(default)`），这里只把模式拨过去。
        self.spawn_tab_for(conn_id, kind, window, cx);
        self.mode = Mode::Instance(tab);
        self.fetch_instance_tab(conn_id, tab, cx);
        cx.notify();
    }

    /// 拉某一档的服务器数据（落页面级字段；切标签后由快照各归各家）。
    ///
    /// [`Self::open_instance_tab`] 与 [`Self::refresh_instance_tab`] 共用：
    /// 刷新路径缓存已清，这里只剩「发枪」。
    pub(super) fn fetch_instance_tab(
        &mut self,
        conn_id: &str,
        tab: InstanceTab,
        cx: &mut Context<Self>,
    ) {
        let cached = match tab {
            InstanceTab::Dashboard => true,
            InstanceTab::Overview => self.instance_data.overview.is_some(),
            InstanceTab::Metrics => self.instance_data.metrics.is_some(),
            InstanceTab::Inspector => self.instance_data.processes.is_some(),
        };
        if cached || !tab.fetches_server() {
            return;
        }
        let Some(session) = self.sessions.get(conn_id) else {
            return;
        };
        let conn = session.conn.clone();
        // 历史按连接存：回投闭包里要用，先克隆一份键。
        let history_key = conn_id.to_owned();
        self.status = trf(cx, "正在读取{}…", &[tab.heading(cx)]).into();
        self.spawn_db(
            cx,
            move || -> Result<InstancePayload, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                Ok(match tab {
                    InstanceTab::Overview => InstancePayload::Overview(guard.instance_overview()?),
                    InstanceTab::Metrics => {
                        let table = guard.instance_table(InstanceView::Metrics)?;
                        // 数值序列从**同一张表**筛出来：指标白名单只有引擎侧一份。
                        let points = table.numeric();
                        InstancePayload::Metrics(table, points)
                    }
                    InstanceTab::Inspector => {
                        InstancePayload::Processes(guard.instance_table(InstanceView::Processes)?)
                    }
                    // 仪表盘不取服务器数据（`fetches_server()` 已挡在前面）。
                    InstanceTab::Dashboard => InstancePayload::Overview(Default::default()),
                })
            },
            move |this, result, _window, cx| match result {
                Ok(payload) => {
                    match payload {
                        InstancePayload::Overview(overview) => {
                            this.instance_data.overview = Some(overview);
                        }
                        InstancePayload::Metrics(table, points) => {
                            this.instance_data.metrics = Some(table);
                            // 每取一轮 = 一次采样。缓冲按连接累积（切库 / 换标签不打断），
                            // 断开连接时随会话一起丢（`close_tab`）。
                            this.metric_history
                                .entry(history_key)
                                .or_default()
                                .push(now_secs(), points);
                        }
                        InstancePayload::Processes(table) => {
                            this.instance_data.processes = Some(table);
                        }
                    }
                    this.status = trf(cx, "已读取{}。", &[tab.heading(cx)]).into();
                }
                Err(error) => {
                    this.status = trf(cx, "读取失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 实例信息页（左栏连接节点的四个入口）。
    ///
    /// 四档共用一套骨架：页头（图标 + 标题 + 刷新）+ 内容区。内容区按档位给不同的卡，
    /// 取数状态（还没取 / 取失败）由**同一句提示**表达——三档都一样，不必各写一遍。
    pub(super) fn render_instance_page(
        &self,
        tab: InstanceTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let subtitle: SharedString = match self.active_config() {
            Some(cfg) => {
                SharedString::from(format!("{} · {}", kind_label(cfg.kind), cfg.display_name()))
            }
            None => SharedString::from(tr(cx, "未连接")),
        };
        let mut header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                Icon::new(instance_tab_icon(tab))
                    .size(extended.icon.lg)
                    .text_color(extended.colors.accent_text),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(tokens.typography.md.size)
                            .line_height(tokens.typography.md.line_height)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(tab.heading(cx))),
                    )
                    .child(meta_text(subtitle, cx)),
            );
        if tab.fetches_server() {
            header = header.child(
                icon_text_button(
                    "db-instance-refresh",
                    tr(cx, "刷新"),
                    Icon::new(DuckIcon::RotateCw)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.busy)
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.refresh_instance_tab(tab, window, cx);
                    });
                }),
            );
        }

        let mut column = v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(header)
            .child(match tab {
                InstanceTab::Dashboard => self.render_instance_dashboard(cx),
                InstanceTab::Overview => self.render_instance_overview(cx),
                InstanceTab::Metrics => self.render_instance_metrics(cx),
                InstanceTab::Inspector => self.render_instance_inspector(cx),
            });
        // 还没取到的档位：把「正在读」写在内容区，而不是留一片空白让人以为坏了。
        if tab.fetches_server() && !self.instance_tab_loaded(tab) {
            column = column.child(meta_text(
                if self.busy {
                    tr(cx, "正在读取实例信息…")
                } else {
                    tr(cx, "还没有读取实例信息，点右上角「刷新」重试。")
                },
                cx,
            ));
        }
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("db-instance-page")
            .overflow_y_scroll()
            .children(vec![
                div()
                    .w_full()
                    .p(tokens.spacing.md)
                    .child(column)
                    .into_any_element(),
            ])
            .into_any_element()
    }

    /// 这一档的服务器数据到手了没有。
    pub(super) fn instance_tab_loaded(&self, tab: InstanceTab) -> bool {
        match tab {
            InstanceTab::Dashboard => true,
            InstanceTab::Overview => self.instance_data.overview.is_some(),
            InstanceTab::Metrics => self.instance_data.metrics.is_some(),
            InstanceTab::Inspector => self.instance_data.processes.is_some(),
        }
    }

    /// 丢弃某一档的缓存后重取（页头「刷新」）。
    pub(super) fn refresh_instance_tab(
        &mut self,
        tab: InstanceTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match tab {
            InstanceTab::Dashboard => {}
            InstanceTab::Overview => self.instance_data.overview = None,
            InstanceTab::Metrics => self.instance_data.metrics = None,
            InstanceTab::Inspector => self.instance_data.processes = None,
        }
        // 仪表盘没有服务器数据可重取，但它的对象计数来自会话内省 ——
        // 顺手把目录也刷一遍，省得用户跑去左栏点第二个刷新按钮。
        if tab == InstanceTab::Dashboard {
            self.refresh_state(window, cx);
            return;
        }
        // 刷新按钮长在实例页自己头上，连接就是活动标签所属的那条；直接重取，
        // 不走 `open_instance_tab`（同档标签已开时那只做激活、不发枪）。
        let Some(conn_id) = self
            .active_tab
            .as_deref()
            .and_then(|tab| self.tab_conn.get(tab).cloned())
        else {
            return;
        };
        self.fetch_instance_tab(&conn_id, tab, cx);
    }

    /// 仪表盘：这条连接的**构成**与**最近做过的事**（全用界面已有的数据）。
    pub(super) fn render_instance_dashboard(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(session) = self.active_session() else {
            return instance_hint(tr(cx, "先连上一条连接。"), cx);
        };
        let mut cards = v_flex().w_full().gap(tokens.spacing.md);

        // ── 连接信息 ──
        if let Some(cfg) = self.active_config() {
            let mut rows: Vec<(SharedString, SharedString)> = vec![
                ("engine".into(), kind_label(cfg.kind).into()),
                ("host".into(), cfg.host.clone().into()),
                (
                    "port".into(),
                    cfg.port
                        .or_else(|| cfg.kind.default_port())
                        .map(|port| port.to_string())
                        .unwrap_or_default()
                        .into(),
                ),
                ("database".into(), cfg.database.clone().into()),
                ("user".into(), cfg.user.clone().into()),
            ];
            if let Some(group) = cfg.group.as_deref().filter(|group| !group.is_empty()) {
                rows.insert(0, ("group".into(), group.to_owned().into()));
            }
            rows.push(("server_version".into(), session.version.clone().into()));
            cards = cards.child(self.instance_kv_card(tr(cx, "连接信息"), rows, cx));
        }

        // ── 对象构成：左栏树上的那些分类，这里给个数 ──
        let mut objects: Vec<(SharedString, SharedString)> = TreeCategory::ALL
            .iter()
            .filter_map(|category| {
                let count = session
                    .catalog
                    .iter()
                    .filter(|entry| category.matches(entry.kind))
                    .count()
                    + session.database_objects(*category).len();
                // 计数为 0 的分类不列：这一页是「这里有什么」，不是「什么没有」。
                (count > 0).then(|| {
                    (
                        SharedString::from(category.heading(cx)),
                        SharedString::from(count.to_string()),
                    )
                })
            })
            .collect();
        objects.insert(
            0,
            (
                tr(cx, "命名空间").into(),
                collect_namespaces(&session.databases, &session.schemas)
                    .len()
                    .to_string()
                    .into(),
            ),
        );
        cards = cards.child(self.instance_kv_card(tr(cx, "对象构成"), objects, cx));

        // ── 最近查询：历史里最近几条，点一条载入编辑器 ──
        let mut recent = v_flex().w_full().gap(tokens.spacing.xxs);
        if self.history.is_empty() {
            recent = recent.child(meta_text(tr(cx, "还没有执行过语句。"), cx));
        }
        for sql in self.history.iter().take(5) {
            let view = self.view.clone();
            let load = sql.clone();
            recent = recent.child(
                sidebar_navigation_button(
                    SharedString::from(format!("db-dash-history-{sql}")),
                    SharedString::from(one_line(sql)),
                    Icon::new(DuckIcon::Clock)
                        .size(theme.extended().icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    div().flex_none(),
                    false,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let Some(entity) = view.upgrade() else {
                        return;
                    };
                    let sql = load.clone();
                    entity.update(cx, |this, cx| {
                        this.fill_editor(sql, window, cx);
                        // 与历史面板同口径：放回编辑器不执行，状态栏也照那边说。
                        this.status = tr(cx, "已从历史放回编辑器。").into();
                        cx.notify();
                    });
                }),
            );
        }
        cards
            .child(instance_card(tr(cx, "最近查询"), recent, cx))
            .child(self.dashboard_transport_card(cx))
            .child(self.dashboard_hook_card(cx))
            .into_any_element()
    }

    /// 仪表盘上的「传输通道」卡：这条连接经什么出去、通道还活着吗。
    ///
    /// 为什么放在这一页：隧道 / 代理 / SSM 都是**外部进程或本机端口**，档案里那几栏
    /// 只说明「配了什么」，回答不了「现在到底通不通」。而通道一死，表现全是「查询超时」——
    /// 用户会去查库、查网络，最后才想到「ssh 早就退出了」。这一卡把这个弯路截掉。
    pub(super) fn dashboard_transport_card(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let active = self.active.clone().unwrap_or_default();
        let transport = self
            .sessions
            .get(&active)
            .and_then(|session| session.transport.as_ref());
        // 判据要按**展开后的**档案算：只引用了一份代理档案的连接，`active_config()` 上
        // 三档全是 `None`，直接算会显示「直连」。
        let expanded = self
            .active_config()
            .map(|cfg| expand_tunnel_profile(cfg, &self.tunnel_profiles).0);
        let configured = expanded
            .as_ref()
            .map_or(TransportKind::Direct, ConnectionConfig::transport);
        let mut body = v_flex().w_full().gap(tokens.spacing.xxs);
        match transport {
            Some(transport) => {
                let alive = transport.is_alive();
                body = body
                    .child(description_list(vec![
                        description_row(tr(cx, "接入方式"), transport.label(cx)),
                        description_row(tr(cx, "本机端口"), transport.local_port().to_string()),
                    ]))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .child(
                                Icon::new(if alive {
                                    DuckIcon::CircleCheck
                                } else {
                                    DuckIcon::CircleAlert
                                })
                                .size(extended.icon.md)
                                .text_color(if alive {
                                    extended.colors.success
                                } else {
                                    tokens.colors.destructive
                                }),
                            )
                            .child(meta_text(
                                if alive {
                                    tr(cx, "通道正常。")
                                } else {
                                    tr(cx, "通道已断开，请重新连接。")
                                },
                                cx,
                            )),
                    );
            }
            None => {
                body = body.child(meta_text(
                    if configured == TransportKind::Direct {
                        tr(cx, "直连：没有配置隧道 / 代理。")
                    } else {
                        tr(cx, "档案配了传输通道，但当前会话还没建立。")
                    },
                    cx,
                ));
            }
        }
        // 配了但填不全的档位**不会生效**：不说出来的话，用户会以为自己已经在走代理。
        if expanded
            .as_ref()
            .is_some_and(ConnectionConfig::has_incomplete_transport)
        {
            body = body.child(meta_text(tr(cx, "有一档传输配置没填完，不会生效。"), cx));
        }
        instance_card(tr(cx, "传输通道"), body, cx)
    }

    /// 仪表盘上的「连接钩子」卡：这条连接最近的钩子执行（成功 / 失败一眼可见）。
    ///
    /// 为什么放在这一页：钩子是**静默**跑的（失败默认也不拦），平时没有任何界面会提到它；
    /// 而这一页的定位正是「这条连接的构成与最近做过的事」，钩子恰好属于后者。
    pub(super) fn dashboard_hook_card(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let active = self.active.clone().unwrap_or_default();
        let records: Vec<&HookRecord> = self
            .hook_log
            .iter()
            .rev()
            .filter(|record| record.connection_id == active)
            .take(5)
            .collect();
        let configured = self
            .active_config()
            .is_some_and(|cfg| !cfg.hooks.is_empty());
        let mut body = v_flex().w_full().gap(tokens.spacing.xxs);
        if records.is_empty() {
            // 空态分两种：配了但还没跑（连上就会跑），和压根没配（去编辑档案加）。
            body = body.child(meta_text(
                if configured {
                    tr(cx, "这条连接配了钩子，还没跑过。")
                } else {
                    tr(cx, "这条连接没有配置钩子。")
                },
                cx,
            ));
        }
        let now = now_secs();
        for record in records {
            let ok = record.outcome.status.is_ok();
            body = body.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(
                        Icon::new(if ok {
                            DuckIcon::CircleCheck
                        } else {
                            DuckIcon::CircleAlert
                        })
                        .size(extended.icon.md)
                        .text_color(if ok {
                            extended.colors.success
                        } else {
                            tokens.colors.destructive
                        }),
                    )
                    .child(div().flex_1().min_w_0().child(meta_text(
                        SharedString::from(format!(
                            "{} · {} · {} · {}",
                            hook_phase_label(record.outcome.phase, cx),
                            one_line(&record.outcome.command),
                            hook_status_label(&record.outcome.status, cx),
                            tr_relative_time(cx, now - record.at, UnderMinute::JustNow),
                        )),
                        cx,
                    ))),
            );
        }
        instance_card(tr(cx, "连接钩子"), body, cx)
    }

    /// 实例概览：服务器级事实（版本 / 在线时长 / 体量 / 字符集…）。
    pub(super) fn render_instance_overview(&self, cx: &App) -> AnyElement {
        let Some(overview) = self.instance_data.overview.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<(SharedString, SharedString)> =
            vec![("version".into(), overview.version.clone().into())];
        if !overview.database.is_empty() {
            rows.push(("database".into(), overview.database.clone().into()));
        }
        if !overview.user.is_empty() {
            rows.push(("user".into(), overview.user.clone().into()));
        }
        if let Some(uptime) = overview.uptime.clone() {
            rows.push(("uptime".into(), uptime.into()));
        }
        for fact in &overview.facts {
            rows.push((
                SharedString::from(fact.name.clone()),
                SharedString::from(fact.value.clone()),
            ));
        }
        self.instance_kv_card(tr(cx, "实例概览"), rows, cx)
    }

    /// 实例指标：时序折线（本会话内累积的采样）+ 明细表。
    ///
    /// 两张东西的分工：折线看**走势**（需要历史），明细表看**此刻的值**（含画不进坐标系的
    /// 文案项，如体量 `8493 kB`）。只有一次采样时折线还画不出走势，那时如实说清楚
    /// 「多刷几次就有」，而不是画一条只有一个点的线让人以为指标是常数。
    pub(super) fn render_instance_metrics(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(table) = self.instance_data.metrics.as_ref() else {
            return div().into_any_element();
        };
        let mut column = v_flex().w_full().gap(tokens.spacing.md);
        if let Some(history) = self
            .active
            .as_deref()
            .and_then(|id| self.metric_history.get(id))
        {
            let mut keys = history.keys();
            // 左栏指标子项点进来的聚焦指标排到**最前**：页面是长滚动列表，高亮圈
            // 若停在视口外等于没做；置于首位才是「点谁谁在眼前」。
            // （实例页的滚动容器没挂 scroll handle，程序化滚到卡片不可靠，不赌 API。）
            let focus_position = self
                .focused_metric
                .as_ref()
                .and_then(|focus| keys.iter().position(|key| key == focus));
            if let Some(position) = focus_position {
                let focused = keys.remove(position);
                keys.insert(0, focused);
            }
            // 图上限：指标白名单有十几项，全铺出来是一屏碎图；取前几张（顺序即引擎的
            // 白名单顺序，最要紧的排前面）——剩下的在明细表里一个不少。
            let charts: Vec<String> = keys.into_iter().take(METRIC_CHART_LIMIT).collect();
            if charts.is_empty() || history.len() < 2 {
                column = column.child(meta_text(
                    if charts.is_empty() {
                        tr(cx, "这台服务器没报出可画的数值指标。")
                    } else {
                        tr(cx, "再刷新几次就会出现趋势（当前只有一次采样）。")
                    },
                    cx,
                ));
            } else {
                column = column.child(meta_text(
                    SharedString::from(trf(
                        cx,
                        "最近 {} 次采样 · 覆盖 {}",
                        &[
                            &history.len().to_string(),
                            &compact_duration(history.span_secs()),
                        ],
                    )),
                    cx,
                ));
                for chunk in charts.chunks(2) {
                    let mut row = h_flex().w_full().gap(tokens.spacing.md);
                    for key in chunk {
                        let focused = self.focused_metric.as_deref() == Some(key.as_str());
                        row = row.child(self.metric_chart_card(
                            key,
                            &history.series(key),
                            focused,
                            cx,
                        ));
                    }
                    // 奇数张时补一个空位，免得最后一张被拉成整行宽（两张图一行的节奏）。
                    if chunk.len() == 1 {
                        row = row.child(div().flex_1());
                    }
                    column = column.child(row);
                }
            }
        }
        column
            .child(self.instance_table_card(table, cx))
            .into_any_element()
    }

    /// 一张指标折线卡：标题 + 折线（横轴是「距现在多久」，纵轴是该指标的值）。
    ///
    /// `focused` = 左栏指标子项刚点进来的那一张：描边换强调色、标题提亮，
    /// 与其余卡片一眼区分（不另起发光之类的花活，描边就是全站的高亮语言）。
    pub(super) fn metric_chart_card(
        &self,
        key: &str,
        series: &[(i64, f64)],
        focused: bool,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 采样点：横轴文案用「距今」的紧凑时长（数字 + s/m/h，无需翻译），
        // 纵轴直接用值。`series` 为空时给一张只有标题的卡（界面不至于开天窗）。
        let points: Vec<(String, f64)> = series
            .iter()
            .map(|(at, value)| (compact_duration((now_secs() - at).max(0)), *value))
            .collect();
        let stroke = extended.colors.accent_text;
        let mut card = v_flex()
            .flex_1()
            .min_w_0()
            .gap(tokens.spacing.xs)
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border(extended.stroke.thin)
            .border_color(if focused {
                extended.colors.accent_text
            } else {
                extended.colors.hairline
            })
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(if focused {
                        extended.colors.accent_text
                    } else {
                        tokens.colors.muted_foreground
                    })
                    .truncate()
                    .child(SharedString::from(key.to_owned())),
            );
        if points.is_empty() {
            return card
                .child(meta_text(tr(cx, "没有采到值。"), cx))
                .into_any_element();
        }
        // 数据点全部交给图表（形状不丢）：抽稀是它自己的事。
        card = card.child(
            div().w_full().h(px(120.)).child(
                LineChart::new(points)
                    .id(SharedString::from(format!("db-metric-{key}")))
                    .x(|(label, _)| label.clone())
                    .y(|(_, value)| *value)
                    .stroke(stroke)
                    .x_axis(false)
                    .grid(true)
                    .y_tick_format(compact_number)
                    .tooltip_title(|(label, _)| SharedString::from(label.clone()))
                    .tooltip_value({
                        let name = SharedString::from(key.to_owned());
                        move |_, value| {
                            SharedString::from(format!("{name}: {}", compact_number(value)))
                        }
                    }),
            ),
        );
        card.into_any_element()
    }

    /// 实例检查器：服务端会话 / 连接现状（SQLite 是本连接挂载的数据库）。
    pub(super) fn render_instance_inspector(&self, cx: &App) -> AnyElement {
        let Some(table) = self.instance_data.processes.as_ref() else {
            return div().into_any_element();
        };
        let mut column = v_flex().w_full().gap(active_theme(cx).tokens().spacing.sm);
        // 引擎口径的解释由**界面**给（内核只出数据，不出面向用户的散文）：
        // SQLite 是进程内数据库，没有服务端会话列表，能检查的只有挂载了哪些文件。
        if self
            .active_config()
            .is_some_and(|cfg| cfg.kind == DatabaseKind::Sqlite)
        {
            column = column.child(meta_text(
                tr(
                    cx,
                    "SQLite 是进程内数据库，没有服务端会话列表；下面是本连接挂载的数据库文件。",
                ),
                cx,
            ));
        }
        if table.rows.is_empty() {
            column = column.child(meta_text(tr(cx, "当前没有会话。"), cx));
        } else {
            column = column.child(self.instance_table_card(table, cx));
        }
        column.into_any_element()
    }

    /// 一张「名字 — 值」卡（实例概览与仪表盘共用）。
    ///
    /// 走全站既有的 `description_list` 而不是自绘行：右栏「连接信息」、行详情卡都是它，
    /// 标签列宽与行距才不会一处一个样。
    pub(super) fn instance_kv_card(
        &self,
        title: impl Into<SharedString>,
        rows: impl IntoIterator<Item = (SharedString, SharedString)>,
        cx: &App,
    ) -> AnyElement {
        let items = rows
            .into_iter()
            .map(|(name, value)| description_row(name, value))
            .collect::<Vec<_>>();
        instance_card(title, description_list(items), cx)
    }

    /// 一张**通用表**卡（指标 / 检查器共用；列数与列名全由内核给）。
    pub(super) fn instance_table_card(&self, table: &InstanceTable, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let mut grid = v_flex().w_full();
        // 表头
        let mut head = h_flex().w_full().py(tokens.spacing.xs);
        for column in &table.columns {
            head = head.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().truncate().child(SharedString::from(column.clone()))),
            );
        }
        grid = grid.child(head);
        for row in &table.rows {
            grid = grid.child(
                div()
                    .w_full()
                    .h(px(1.))
                    .pl(tokens.spacing.lg)
                    .bg(extended.colors.hairline),
            );
            let mut line = h_flex().w_full().py(tokens.spacing.xs);
            for cell in row {
                line = line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .text_color(tokens.colors.foreground)
                        .child(div().truncate().child(SharedString::from(cell.clone()))),
                );
            }
            grid = grid.child(line);
        }
        instance_card(
            SharedString::from(trf(cx, "明细（{} 行）", &[&table.rows.len().to_string()])),
            grid,
            cx,
        )
    }
}
