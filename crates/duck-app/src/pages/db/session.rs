//! 会话与标签的生命周期：连上去 / 断开来、开标签 / 关标签、当前标签与当前会话的取值。
//!
//! 一条 `DatabasePage` 的「当前可见内容」全部由本模块决定 —— [`Self::activate`] 是唯一的
//! 切换入口，[`Self::teardown_session`] 是唯一的拆除入口；标签关闭策略（关其他 / 关右侧 /
//! 关全部）都收敛到 [`Self::remove_tab`]。会话实体本身活在 `sessions` 表里，本模块只搬
//! 引用、不动它的内部状态（那是 `browse` / `grid` / `query` 的事）。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;

impl DatabasePage {
    /// 左栏「刷新」：重读 kv 档案（含外部改动），活跃会话重取目录。
    ///
    /// 不接 `Window`：刷新本身不弹通知（结论落在状态栏），目录重取走
    /// [`Self::spawn_db`] 的回投，回调自带窗口。
    pub(super) fn refresh_state(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.connections = load_connections(cx);
        self.groups = load_groups(cx);
        let Some(session) = self.active_session() else {
            self.status = tr(cx, "已刷新。").into();
            cx.notify();
            return;
        };
        let conn = session.conn.clone();
        // 重新内省的是**当前浏览的命名空间**（不是引擎缺省的那个）：用户在 schema
        // 下拉里切到别的库之后刷新，不该被弹回缺省库。
        let schema = session.schema.clone();
        self.spawn_db(
            cx,
            move || -> Result<Introspection, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                Introspection::load(&mut guard, schema.as_deref())
            },
            |this, result, _window, cx| match result {
                Ok(introspection) => {
                    if let Some(session) = this.active_session_mut() {
                        introspection.install(session);
                    }
                    this.status = tr(cx, "已刷新。").into();
                }
                Err(error) => {
                    this.status = trf(cx, "刷新失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 切换**当前库**（左栏库列表 / 右栏下拉在「库即命名空间」的引擎上走这里）。
    ///
    /// 与 [`Self::select_schema`] 的关键差别：这里会**真的切会话**——
    /// MySQL / ClickHouse / SQL Server / Mongo 各自发 `USE` / 改端点 / 记落点，
    /// PostgreSQL 回 `NeedsReconnect` 后由 [`Self::connect_entry_with_database`] 重连。
    ///
    /// 原先只把库名记在界面上（`set_namespace` 对 SQL 引擎是空操作），于是库名留空
    /// 建连的用户点了库、也看到了表，一跑 `SELECT` 却被服务端回 `No database selected`
    /// —— 界面「切了」、查询没切，正是「和 dbx 不一样」的那一处。
    pub(super) fn select_database(
        &mut self,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active.clone() else {
            return;
        };
        // 借用到这里就结束：下面要 `&mut self` 清状态、写档案，不能拖着 `session` 的
        // 不可变借用（E0502）。
        let (conn, already_there) = {
            let Some(session) = self.sessions.get(&id) else {
                return;
            };
            (
                session.conn.clone(),
                session.database.as_deref() == Some(name.as_str()),
            )
        };
        if already_there {
            return;
        }
        // 换库 = 换整套对象与列：选中表、勾选、暂存改动全部作废（与换 schema 同口径）。
        self.selected_table = None;
        self.table_rows = None;
        self.table_page = 0;
        self.mode = Mode::Query;
        self.checked_rows.clear();
        self.clear_pending();
        self.row_editor = None;
        self.selected_row = None;
        self.table_meta_for = None;
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.update_connection_field(&id, cx, |cfg| cfg.database = name.clone());
        self.status = trf(cx, "正在切换数据库 {}…", &[&name]).into();
        // 模式落到 Query、表页数据清空：组件里那份模型必须跟着走，否则切回来会拿
        // 「上一次表浏览的表」去画 SQL 结果。
        self.sync_data_table(cx);
        cx.notify();

        let for_conn = name.clone();
        // 建连闭包要把库名移进去，回投闭包还要用 → 各持一份。
        let switching = name.clone();
        self.spawn_db(
            cx,
            move || -> Result<(SwitchOutcome, Vec<CatalogTable>), DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                let outcome = guard.switch_database(&switching)?;
                // 会话已经是新库了 → 目录按新库取（`None` 即「当前库」，省一次往返）。
                let catalog = match outcome {
                    SwitchOutcome::Applied => guard.catalog(None)?,
                    // 还没切过去（要重连），这份目录属于旧库，取了也不能用。
                    SwitchOutcome::NeedsReconnect => Vec::new(),
                };
                Ok((outcome, catalog))
            },
            move |this, result, window, cx| match result {
                Ok((SwitchOutcome::Applied, catalog)) => {
                    if let Some(session) = this.active_session_mut() {
                        session.catalog = catalog;
                        // 库即命名空间：目录来处就是刚切过去的那个库。
                        session.schema = Some(for_conn.clone());
                        session.database = Some(for_conn.clone());
                    }
                    this.status = trf(cx, "已切到数据库 {}", &[&for_conn]).into();
                }
                Ok((SwitchOutcome::NeedsReconnect, _)) => {
                    // PG：库是建连参数，只能重连。状态栏先说明白，别让人以为卡住了。
                    this.status = trf(cx, "正在重连到数据库 {}…", &[&for_conn]).into();
                    this.connect_entry_with_database(&id, Some(for_conn.clone()), window, cx);
                }
                Err(error) => {
                    this.status = trf(cx, "切换数据库失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 切换当前会话浏览的命名空间：重取目录并回第一页。
    ///
    /// 选中的命名空间写回档案（`ConnectionConfig::schema`），下次连接直接落在这里。
    pub(super) fn select_schema(&mut self, schema: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if session.schema == schema {
            return;
        }
        session.schema = schema.clone();
        let conn = session.conn.clone();
        // 换库 = 换整套对象与列：选中表、勾选、暂存改动全部作废。
        self.selected_table = None;
        self.table_rows = None;
        self.table_page = 0;
        self.mode = Mode::Query;
        self.checked_rows.clear();
        self.clear_pending();
        self.row_editor = None;
        self.selected_row = None;
        self.table_meta_for = None;
        self.table_indexes.clear();
        self.table_row_count = None;
        self.table_ddl = None;
        self.update_connection_field(&id, cx, |cfg| cfg.schema = schema.clone());
        self.status = tr(cx, "正在切换命名空间…").into();
        // 同「切换数据库」：模式 / 表页都换了，组件里的模型跟着走。
        self.sync_data_table(cx);
        cx.notify();
        let for_conn = schema.clone();
        self.spawn_db(
            cx,
            move || -> Result<Introspection, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 先让**连接自己**记住落点再内省：MongoDB 的「当前库」是客户端概念，
                // 不告诉驱动的话，编辑器里的命令还会落在旧库（详见
                // `Connection::set_namespace`）。
                guard.set_namespace(for_conn.as_deref());
                Introspection::load(&mut guard, for_conn.as_deref())
            },
            move |this, result, _window, cx| match result {
                Ok(introspection) => {
                    if let Some(session) = this.active_session_mut() {
                        introspection.install(session);
                    }
                    this.status = tr(cx, "已切换命名空间。").into();
                }
                Err(error) => {
                    this.status = trf(cx, "切换命名空间失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 连接一条档案（后台建连 + 版本 + 目录）。
    ///
    /// 带上 `database` 时按那个库建连（覆盖档案里的值，且不写档案）——
    /// 这是 PostgreSQL 切库的唯一通路：`dbname` 是建连参数，一个会话终身绑死，
    /// 想换库只能重连（见 [`duck_db::SwitchOutcome::NeedsReconnect`]）。
    pub(super) fn connect_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.connect_entry_with_database(id, None, window, cx);
    }

    pub(super) fn connect_entry_with_database(
        &mut self,
        id: &str,
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        // 引用的「传输档案」在**这一次建连**里展开成内联三档，**连同档案自己的凭据**
        // 一起换过去（档案是独立凭据主体，口令存在 `dbprofile:{id}:…`）。
        // 展开后走的是同一条 `open_transport`，档案对它是不存在的。
        let (cfg, creds) = resolve_transport(
            &cfg,
            &self.tunnel_profiles,
            Credentials::from_secret_store(&cfg.id),
        );
        // 只在**本次建连**里覆盖库名：档案不动（浏览动作不该悄悄改档案）。
        let cfg = match database {
            Some(database) => ConnectionConfig { database, ..cfg },
            None => cfg,
        };
        self.status = trf(cx, "正在连接 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        let done_cfg = cfg.clone();
        self.spawn_db(
            cx,
            move || -> ConnectAttempt {
                let facts = hook_facts(&cfg);
                // 「建连前」在最前面：开 VPN / 切 DNS 这类前置动作都该发生在 socket 之前。
                let pre = run_phase(&cfg.hooks, HookPhase::PreConnect, &facts);
                log_hook_report(&cfg.id, HookPhase::PreConnect, &pre);
                if pre.blocker().is_some() {
                    // 明确阻断：**连都不连**（不是「连上了又关掉」——那会在服务端留一条认证记录）。
                    return ConnectAttempt {
                        pre,
                        post: HookReport::default(),
                        session: None,
                        error: None,
                    };
                }
                match open_session(&cfg, &creds) {
                    Ok(session) => {
                        // 「建连后」在目录都取完之后：此时这条连接**真的可用**了，
                        // 拿它做「注册到服务注册中心」「写一条审计」才有意义。
                        let post = run_phase(&cfg.hooks, HookPhase::PostConnect, &facts);
                        log_hook_report(&cfg.id, HookPhase::PostConnect, &post);
                        if post.blocker().is_some() {
                            // 阻断 = 这次「建连」整体不算数：把会话丢掉（socket 随之关闭）。
                            return ConnectAttempt {
                                pre,
                                post,
                                session: None,
                                error: None,
                            };
                        }
                        ConnectAttempt {
                            pre,
                            post,
                            session: Some(session),
                            error: None,
                        }
                    }
                    Err(error) => {
                        // 建连本身就失败了：不跑「建连后」钩子（没什么可注册的），
                        // 但「建连前」的结果照旧带回去给用户看。
                        ConnectAttempt {
                            pre,
                            post: HookReport::default(),
                            session: None,
                            error: Some(error),
                        }
                    }
                }
            },
            move |this, attempt, window, cx| {
                let cfg = &done_cfg;
                let ConnectAttempt {
                    pre,
                    post,
                    session,
                    error,
                } = attempt;
                // 先把「哪一段被钩子拦下了」记下来：报告随即被 `note_hook_report` 取走。
                let blocked = if pre.blocked.is_some() {
                    Some(HookPhase::PreConnect)
                } else if post.blocked.is_some() {
                    Some(HookPhase::PostConnect)
                } else {
                    None
                };
                this.note_hook_report(&cfg.id, pre, window, cx);
                this.note_hook_report(&cfg.id, post, window, cx);
                let Some(session) = session else {
                    // 「钩子挡下的」与「引擎连不上的」是两种处置（前者去改钩子，后者去查网络 /
                    // 凭据），提示里必须分得开 —— 所以先看是不是钩子拦的，再谈引擎错误。
                    let message = match (blocked, error) {
                        (Some(phase), _) => {
                            // 钩子阻断是**策略生效**，不是故障：记 `blocked` 而不是 `failed`
                            //（审计的「只看问题」会带上它，但结果列显示「已拦下」而不是「失败」）。
                            let reason = trf(
                                cx,
                                "{}的钩子未通过，已按配置中断连接。",
                                &[hook_phase_label(phase, cx)],
                            );
                            let detail = trf(cx, "连接 {}：{}", &[&cfg.display_name(), &reason]);
                            crate::audit::record_event(
                                cx,
                                crate::audit::AuditKind::Hook,
                                crate::audit::action::RUN_HOOK,
                                crate::audit::AuditOutcome::Blocked,
                                &cfg.id,
                                detail,
                            );
                            reason
                        }
                        (None, Some(error)) => {
                            let kind = error.kind();
                            let layer = failure_layer(&error, cfg, cx);
                            let detail = trf(
                                cx,
                                "连接 {}（{}）失败：{}",
                                &[&cfg.display_name(), &layer, &error.error().to_string()],
                            );
                            let reason = error.to_string();
                            let trace = crate::audit::record_event(
                                cx,
                                kind,
                                stage_action(kind),
                                crate::audit::AuditOutcome::Failed,
                                &cfg.id,
                                detail,
                            );
                            failure_message(cx, error.label(cx), trace.as_deref(), &reason)
                                .to_string()
                        }
                        (None, None) => {
                            let detail = trf(cx, "连接 {} 未能建立。", &[&cfg.display_name()]);
                            crate::audit::record_event(
                                cx,
                                crate::audit::AuditKind::Connection,
                                crate::audit::action::CONNECT,
                                crate::audit::AuditOutcome::Failed,
                                &cfg.id,
                                detail,
                            );
                            tr(cx, "连接失败。").to_owned()
                        }
                    };
                    this.status = SharedString::from(message.clone());
                    notify::error(SharedString::from(message), window, cx);
                    cx.notify();
                    return;
                };
                let id = cfg.id.clone();
                // 走了通道才记一条「通道已建立」：直连没有通道可记，而这一条正是
                // 「这条连接什么时候走的是隧道 / 走的是哪一档」的唯一来源。
                if let Some(transport) = session.transport.as_ref() {
                    let name = cfg.display_name();
                    let port = transport.local_port().to_string();
                    let kind_label = transport.label(cx);
                    let detail = trf(cx, "{} · {}（本地端口 {}）", &[kind_label, &name, &port]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Transport,
                        crate::audit::action::OPEN_TRANSPORT,
                        crate::audit::AuditOutcome::Ok,
                        &id,
                        detail,
                    );
                }
                this.sessions.insert(id.clone(), session);
                // 把这条会话挂进 AI 侧的只读目录，`run_sql_readonly` 据此按 id 找会话
                // （`docs/ai-design.md` §10.4）。目录键取自句柄自己的 id，保证两边
                // 不会因为来源不同而漂移；重连（同 id 再连）会覆盖旧句柄。
                if let Some(readonly) = this.readonly_session(&id) {
                    let session_id = readonly.id().to_owned();
                    let label = cfg.display_name();
                    crate::ai_tools::publish_sql_session(session_id, label, move |sql, rows| {
                        readonly.execute(sql, rows)
                    });
                }
                this.activate_conn(&id, window, cx);
                let name = cfg.display_name();
                let kind_label = transport_kind_label(cfg.transport(), cx);
                let detail = trf(cx, "连接 {}（{}）", &[&name, kind_label]);
                crate::audit::record_event(
                    cx,
                    crate::audit::AuditKind::Connection,
                    crate::audit::action::CONNECT,
                    crate::audit::AuditOutcome::Ok,
                    &id,
                    detail,
                );
                // 连接状态读数只在 globalbar 右端一处（用户明确要求去掉工具条中间
                // 这份重复）；连接成功的即时反馈交给通知气泡。
                this.status = SharedString::from("");
                notify::success(trf(cx, "已连接 {}。", &[&cfg.display_name()]), window, cx);
                cx.notify();
            },
        );
    }

    /// 派生下一个标签 id。
    pub(super) fn next_tab_id(&mut self) -> String {
        self.tab_seq += 1;
        format!("t{}", self.tab_seq)
    }

    /// 新开一个绑定 `conn_id` 的查询标签并激活（dbx 的「+」语义）。
    ///
    /// 会话按连接共享，这里只造一个**空的编辑区快照位**；当前标签的编辑区先存回
    /// 它自己的快照位，再切到新标签的空白编辑区。
    pub(super) fn open_tab(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        self.spawn_tab_for(conn_id, TabKind::Query, window, cx);
        cx.notify();
    }

    /// 造一个新标签并装上空白编辑区（当前标签的现场先存回快照位）。
    ///
    /// 新标签**插到活动标签之后**（dbx 同款），没有活动标签就追加到末尾。
    /// 身份写入 [`Self::tab_kind`]；调用方负责随后把新标签的内容装好
    /// （查询标签留空即可，数据标签接着调 [`Self::open_table`]）。
    pub(super) fn spawn_tab_for(
        &mut self,
        conn_id: &str,
        kind: TabKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.active_tab.clone() {
            let state = self.take_editor_state(cx);
            self.sessions_views.insert(previous, state);
        }
        let tab = self.next_tab_id();
        self.tab_conn.insert(tab.clone(), conn_id.to_owned());
        self.tab_kind.insert(tab.clone(), kind);
        let at = self
            .tabs
            .iter()
            .position(|existing| Some(existing.as_str()) == self.active_tab.as_deref())
            .map_or(self.tabs.len(), |at| at + 1);
        self.tabs.insert(at, tab.clone());
        self.active_tab = Some(tab.clone());
        self.active = Some(conn_id.to_owned());
        self.install_editor_state(SessionEditorState::default(), window, cx);
    }

    /// 打开（或复用）某张表 / 视图的**数据标签**。
    ///
    /// 复用策略 = 「同对象复用」：同连接 + 同对象的标签已存在就只激活（`activate`
    /// 会把它留存的浏览态整体装回来），`force_new`（⌥ 点击）跳过复用强制新开。
    /// 新标签插到活动标签之后，内容走既有 [`Self::open_table`]（清态 + 首页 + 审计）。
    pub(super) fn open_data_tab(
        &mut self,
        conn_id: &str,
        table: TableRef,
        force_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        if !force_new {
            let existing = find_reusable_tab(
                &self.tabs,
                &self.tab_kind,
                &self.tab_conn,
                conn_id,
                &TabKind::Data {
                    object: table.clone(),
                },
            );
            if let Some(tab) = existing {
                self.activate(&tab, window, cx);
                return;
            }
        }
        self.spawn_tab_for(
            conn_id,
            TabKind::Data {
                object: table.clone(),
            },
            window,
            cx,
        );
        self.open_table(table, window, cx);
    }

    /// 打开（或复用）某个库 / schema 的**库标签**：点左栏数据库节点的伴随动作。
    ///
    /// 同连接同名复用；新标签是空白查询态，标题带库名。复用已有标签时**不重置**
    /// 里面已输入的 SQL —— 换库清的是表浏览态（[`Self::select_database`]），
    /// 草稿是用户自己的。
    pub(super) fn open_database_tab(
        &mut self,
        conn_id: &str,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let kind = TabKind::Database { name: name.clone() };
        if let Some(tab) =
            find_reusable_tab(&self.tabs, &self.tab_kind, &self.tab_conn, conn_id, &kind)
        {
            self.activate(&tab, window, cx);
            return;
        }
        self.spawn_tab_for(conn_id, kind, window, cx);
    }

    /// 激活「连接 `conn_id` 的某个查询标签」：已有就切到最早的那个，没有就新开。
    ///
    /// 页面其余逻辑（左栏 / 树节点 / 仪表板节点）手里拿的都是**连接 id**，
    /// 统一从这里落到标签层。
    pub(super) fn activate_conn(
        &mut self,
        conn_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(conn_id) {
            return;
        }
        let existing = self
            .tabs
            .iter()
            .find(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == conn_id))
            .cloned();
        match existing {
            Some(tab) => self.activate(&tab, window, cx),
            None => self.open_tab(conn_id, window, cx),
        }
    }

    /// 活动标签属于 `conn_id` 时的身份；不属于（或没有活动标签）= `None`。
    ///
    /// 左栏各入口的**选中态**都问它：实例页 / 仪表板搬进标签层之后，
    /// 「主区在不在看这一档」等价于「活动标签是这个身份、且属于这条连接」——
    /// 页面级 `self.mode` 只反映活动标签装进来的那份快照，单看它会串连接。
    pub(super) fn active_tab_kind(&self, conn_id: &str) -> Option<&TabKind> {
        let tab = self.active_tab.as_deref()?;
        if self.tab_conn.get(tab)?.as_str() != conn_id {
            return None;
        }
        self.tab_kind.get(tab)
    }

    /// 切到某个查询标签：把当前编辑区存回上一个标签，再把目标标签的编辑区装回来。
    ///
    /// 这是「每个查询标签各自保留 SQL 与结果集」的落地点（见 [`SessionEditorState`]）。
    /// 重复点同一个标签是**空操作** —— 内容与结果区必须原样留着。
    pub(super) fn activate(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn_id) = self.tab_conn.get(id).cloned() else {
            return;
        };
        if !self.sessions.contains_key(&conn_id) {
            return;
        }
        if self.active_tab.as_deref() == Some(id) {
            return;
        }
        // 先把当前标签的编辑区存进它的快照位（`take_editor_state` 借 `self`，
        // `sessions_views.insert` 随后借 —— 顺序摆开，互不打架）。
        if let Some(previous) = self.active_tab.clone().filter(|prev| prev.as_str() != id) {
            let state = self.take_editor_state(cx);
            self.sessions_views.insert(previous, state);
        }
        let state = self.sessions_views.remove(id).unwrap_or_default();
        self.active_tab = Some(id.to_owned());
        self.active = Some(conn_id);
        // 换标签 = 换一份结果：上一份结果的搜索词对新结果几乎必然失效，
        // 清掉（见 `result_search` 字段注释——不按标签留存的原因）。
        self.clear_result_search(window, cx);
        self.install_editor_state(state, window, cx);
        cx.notify();
    }

    /// 标签的**基础标题**（未去重）：数据标签 `表名@库`、库标签库名、查询标签连接名。
    ///
    /// 标签 id 对应的连接已经不在（正被移除 / 从没登记）时返回 `None`，调用方跳过它
    /// —— 与 [`Self::render_tabs`] 的「未知标签不画」保持一致。去重在后一步做
    /// （见 [`dedupe_titles`]）。
    pub(super) fn tab_base_title(&self, tab_id: &str, cx: &App) -> Option<String> {
        let conn_id = self.tab_conn.get(tab_id)?;
        let cfg = self.connections.iter().find(|cfg| &cfg.id == conn_id)?;
        Some(match self.tab_kind.get(tab_id) {
            Some(TabKind::Data { object }) => match &object.schema {
                Some(namespace) => format!("{}@{}", object.name, namespace),
                None => object.name.clone(),
            },
            Some(TabKind::Database { name }) => name.clone(),
            Some(TabKind::Instance(tab)) => tab.heading(cx).to_owned(),
            Some(TabKind::Dashboard(id)) => self
                .dashboards
                .iter()
                .find(|board| board.id == *id)
                .map(|board| board.name.clone())
                .unwrap_or_else(|| tr(cx, "仪表板").to_owned()),
            _ => cfg.display_name().to_string(),
        })
    }

    /// 关闭除 `keep` 外的全部标签（连接语义同 [`Self::close_tab`]）。
    ///
    /// 先收齐被害人再逐个关：`close_tab` 会改 `self.tabs`（`retain`），边遍历边关会漏。
    pub(super) fn close_other_tabs(
        &mut self,
        keep: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let victims: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| tab.as_str() != keep)
            .cloned()
            .collect();
        for tab in victims {
            self.close_tab(&tab, window, cx);
        }
    }

    /// 关闭 `id` **右侧**的全部标签（标签顺序即用户看到的顺序）。
    pub(super) fn close_tabs_right_of(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pos) = self.tabs.iter().position(|tab| tab == id) else {
            return;
        };
        let victims: Vec<String> = self.tabs.iter().skip(pos + 1).cloned().collect();
        for tab in victims {
            self.close_tab(&tab, window, cx);
        }
    }

    /// 关闭全部标签（每个连接的最后一个标签 = 断开该连接）。
    pub(super) fn close_all_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let victims: Vec<String> = self.tabs.clone();
        for tab in victims {
            self.close_tab(&tab, window, cx);
        }
    }

    /// 关闭一个查询标签。
    ///
    /// 该连接**还有别的标签**时只关这一个标签（连接保持，其他标签的编辑区原样）；
    /// 已是最后一个标签时语义回到「断开连接」——钩子时序交给
    /// [`Self::disconnect_conn`]，与会话绑定的东西（指标历史 / AI 目录）一起清。
    pub(super) fn close_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn_id) = self.tab_conn.get(id).cloned() else {
            return;
        };
        let last_of_conn = self
            .tabs
            .iter()
            .filter(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == &conn_id))
            .count()
            <= 1;
        if !last_of_conn {
            self.remove_tab(id, window, cx);
            return;
        }
        self.disconnect_conn(&conn_id, window, cx);
    }

    /// 只移除一个查询标签（不动连接）：丢掉它的编辑区快照，必要的话把激活位
    /// 挪到剩下的第一个标签。
    pub(super) fn remove_tab(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sessions_views.remove(id);
        self.tab_conn.remove(id);
        self.tab_kind.remove(id);
        self.tabs.retain(|tab| tab != id);
        if self.active_tab.as_deref() == Some(id) {
            self.active_tab = None;
            self.active = None;
            self.install_editor_state(SessionEditorState::default(), window, cx);
            if let Some(next) = self.tabs.first().cloned() {
                self.activate(&next, window, cx);
            }
        }
        cx.notify();
    }

    /// 断开一条连接（原「关闭标签」的连接语义；连接钩子版的入口）。
    ///
    /// 有「断开」钩子时**先跑钩子再拆会话**：`PreDisconnect` 的语义是「连接还活着时跑」，
    /// 所以它必须在会话被移除之前发生 —— 于是这里把真正的拆除让给
    /// [`Self::teardown_session`]，钩子跑完在回调里再调它。没有钩子（绝大多数情况）
    /// 走同一条同步路径，行为与从前一字不差。
    pub(super) fn disconnect_conn(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 没有活跃会话就没有「断开」这回事：钩子不该为一个从未建立（或早掉了）的连接跑。
        if !self.sessions.contains_key(id) {
            self.teardown_session(id, window, cx);
            return;
        }
        let hooks = self
            .connections
            .iter()
            .find(|cfg| cfg.id == id)
            .map(|cfg| cfg.hooks.clone())
            .unwrap_or_default();
        if !hooks.has_phase(HookPhase::PreDisconnect) && !hooks.has_phase(HookPhase::PostDisconnect)
        {
            self.teardown_session(id, window, cx);
            return;
        }
        let facts = self
            .connections
            .iter()
            .find(|cfg| cfg.id == id)
            .map(hook_facts)
            .unwrap_or_default();
        let id_owned = id.to_owned();
        let post_hooks = hooks.clone();
        let post_facts = facts.clone();
        self.spawn_db(
            cx,
            move || run_phase(&hooks, HookPhase::PreDisconnect, &facts),
            move |this, pre, window, cx| {
                this.note_hook_report(&id_owned, pre, window, cx);
                this.teardown_session(&id_owned, window, cx);
                // 「断开后」在会话拆掉之后再跑；界面已经不关心这条连接了，结果只进日志
                // （审计面板会读日志那一份）——所以扔给后台，不占这次操作的收尾。
                let post_id = id_owned.clone();
                cx.background_executor()
                    .spawn(async move {
                        let post = run_phase(&post_hooks, HookPhase::PostDisconnect, &post_facts);
                        log_hook_report(&post_id, HookPhase::PostDisconnect, &post);
                    })
                    .detach();
            },
        );
    }

    /// 真正把一个会话拆掉（移除会话 / 指标历史 / AI 目录 / 标签页）。
    ///
    /// 与 [`Self::close_tab`] 分开：`close_tab` 负责**钩子时序**，这里只做拆除。
    /// 两者混在一起会让「钩子跑几次」与「会话何时消失」纠缠不清。
    pub(super) fn teardown_session(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 只有**真的拆掉了一个会话**才算「断开」：`close_tab` 在没有会话时也会走到这里
        // （见它的第一段），那种情况下记一条「断开」就是凭空多出一条流水。
        let existed = self.sessions.remove(id).is_some();
        if existed {
            let name = self
                .connections
                .iter()
                .find(|cfg| cfg.id == id)
                .map_or_else(|| id.to_owned(), ConnectionConfig::display_name);
            let detail = trf(cx, "断开 {}", &[&name]);
            crate::audit::record_event(
                cx,
                crate::audit::AuditKind::Connection,
                crate::audit::action::DISCONNECT,
                crate::audit::AuditOutcome::Ok,
                id,
                detail,
            );
        }
        // 指标时序是**会话内**的观测历史：连接都没了，攒的采样也没有归属了
        // （下次连上是新会话，从零开始采）。
        self.metric_history.remove(id);
        // 会话已经没了，AI 侧的只读目录也要跟着撤回：否则 `run_sql_readonly` 会列出一个
        // 已经关掉的连接（选了它就直接报未知连接）。
        crate::ai_tools::retract_sql_session(id);
        // 会话没了，它名下的查询标签（可能不止一个）一起拆掉。
        let closed: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| self.tab_conn.get(*tab).is_some_and(|c| c == id))
            .cloned()
            .collect();
        for tab in &closed {
            self.sessions_views.remove(tab);
            self.tab_conn.remove(tab);
            self.tab_kind.remove(tab);
        }
        self.tabs.retain(|tab| !closed.contains(tab));
        if self
            .active_tab
            .as_deref()
            .is_some_and(|tab| closed.iter().any(|c| c == tab))
        {
            // 关掉的正是当前标签：它的编辑区随快照一起丢弃。换成剩下的第一个标签
            // （没有就留空）——先清空再装，免得旧标签的内容残留到新标签上。
            self.active_tab = None;
            self.active = None;
            self.install_editor_state(SessionEditorState::default(), window, cx);
            if let Some(next) = self.tabs.first().cloned() {
                self.activate(&next, window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active.as_deref()?)
    }

    pub(super) fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active.as_deref()?)
    }

    /// 按连接 id 取一个**只读**会话句柄（`None` = 没有这个连接）。
    ///
    /// 给 AI 的 `run_sql_readonly` 用（`docs/ai-design.md` §10.4）：拿到句柄之后由调用方
    /// 丢进后台线程执行；句柄是 `Send + 'static` 的（`Arc<Mutex<Connection>>` + `String`），
    /// 因此可以直接 `move` 进 `background_executor`。
    ///
    /// ⚠️ 只读由**内核**保证（[`duck_db::Connection::execute_read_only`]），这里不做也不
    /// 依赖任何界面侧的检查 —— 上层拿到句柄同样跑不了写语句。
    /// 调用方是 `ai_tools::run_sql_readonly`（连接成功时发布、断开时撤回）。
    pub(crate) fn readonly_session(&self, id: &str) -> Option<ReadOnlySession> {
        let session = self.sessions.get(id)?;
        Some(ReadOnlySession {
            id: id.to_owned(),
            conn: session.conn.clone(),
        })
    }

    /// 活跃会话的连接句柄（SQL 补全的后台取列用）。
    pub(super) fn active_conn(&self) -> Option<Arc<Mutex<Connection>>> {
        self.active_session().map(|session| session.conn.clone())
    }

    /// 当前展示的结果集（SQL 结果或表页）。
    pub(super) fn current_result(&self) -> Option<&ResultSet> {
        match self.mode {
            Mode::Query => self
                .query_result
                .as_ref()
                .and_then(|view| view.result.as_ref()),
            Mode::Table => self.table_rows.as_ref(),
            // 实例页不是结果集：没有可导出的行。仪表板也不是——它一屏有**好几份**结果集，
            // 导出 / 复制的「当前结果集」在这页没有单一定义（要导出就点开对应图表）。
            Mode::Instance(_) | Mode::Dashboard(_) => None,
        }
    }
}
