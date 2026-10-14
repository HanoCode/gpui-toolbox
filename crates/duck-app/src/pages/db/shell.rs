//! 页面骨架与组装：`new`、`impl Render`、主区/右栏的拼装、全局操作组、状态栏与浮层提示。
//!
//! 这里是**唯一**把各切片拼成整页的地方 —— 键位（`KEY_CONTEXT` 那套 action）落在页面根
//! 上，主区永远是「全局操作组 + 标签条 + 编辑器 / 结果」三段，不再有「主区变身表单」态
//!（新建 / 编辑连接走弹窗子页）。右栏开合归 shell 管，页面只订阅它的状态用于高亮。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;

impl DatabasePage {
    /// 创建页面：读连接档案、展开左栏。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });
        // 占位符点明**两件事**：这个框既筛连接、也筛对象（对齐 DBFlux 的
        // 「筛选连接和 Schema…」）——只写「搜索连接」的话，没人会想到它能找表。
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索连接与对象…")));
        let sql = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(SQL_LANGUAGE)
                .placeholder(tr(cx, "输入 SQL，多条语句用分号分隔…"))
                // 自动换行：默认关（长语句横向滚动，与主流 SQL 工具一致），
                // 工具条上的开关与设置页共用同一个键。
                .soft_wrap(db_prefs(cx).editor_word_wrap)
        });
        // 结果区搜索框：变字就重渲一次页面，网格在渲染路径上现算命中集
        //（见 [`Self::render_result`]），不需要中间状态。
        let result_search =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "在结果中搜索…")));
        // 翻页条的「跳转到第 N 页」输入框：两套翻页组（查询客户端分页 / 表浏览服务器
        // 分页）共用这一个框，一次跳页输入用完即清。
        let page_jump = text_input(window, cx, "页码");
        // SQL 补全：关键字 / 函数 / 表名（点号后取列名，查库走后台线程）。
        let page = cx.entity();
        sql.update(cx, |state, _cx| {
            state.lsp_mut().completion_provider =
                Some(Rc::new(SqlCompletionProvider::new(page.downgrade())));
        });
        let mut subscriptions = Vec::new();
        // 编辑器任何 notify（滚动 / 光标 / 文本变化）都让**页面**重渲一次：
        // 左栏「逐条执行」的 ▶ 按 `行号 × 行高 − 滚动偏移` 定位，编辑器内部滚动时
        // 只有这条 observe 能把偏移变化带给页面（InputEvent 里没有滚动事件）。
        subscriptions.push(cx.observe(&sql, |this, sql, cx| {
            // 编辑器文本一变，语句序号就可能整体漂移：语句槽的执行状态标记
            // 只对「产生它们时的那份文本」有意义，文本变了就整体作废。
            let text = sql.read(cx).value().to_string();
            if text != this.stmt_states_text {
                // 语义诊断跟着重算（dbx 同款：诊断只对当前文本有意义）。
                // sqlparser 解析几 KB 文本是微秒级，observe 频率下直接同步跑。
                this.refresh_sql_diagnostics(&text, cx);
                this.stmt_states_text = text;
                this.stmt_states.clear();
            }
            cx.notify();
        }));
        subscriptions.push(cx.subscribe(
            &search,
            |this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    // 搜索词变了，命中集随之重排：客户端分页回第 1 页。
                    this.query_page = 0;
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe(
            &result_search,
            |_this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));
        // 跳页输入框回车即跳（不订阅 Change：边打边跳会在输入「12」的中途先跳到第 1 页）。
        subscriptions.push(cx.subscribe_in(
            &page_jump,
            window,
            |this: &mut Self,
             _source: &Entity<InputState>,
             event: &InputEvent,
             window: &mut Window,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.jump_to_page(window, cx);
                }
            },
        ));

        Self {
            sidebar,
            search,
            result_search,
            sql,
            sql_split: cx.new(|_| ResizableState::default()),
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
            connections: load_connections(cx),
            groups: load_groups(cx),
            sessions: HashMap::new(),
            sessions_views: HashMap::new(),
            tabs: Vec::new(),
            tab_conn: HashMap::new(),
            tab_kind: HashMap::new(),
            tab_seq: 0,
            active_tab: None,
            active: None,
            mode: Mode::Query,
            instance_data: InstanceData::default(),
            focused_metric: None,
            metric_history: std::collections::HashMap::new(),
            hook_log: std::collections::VecDeque::new(),
            selected_table: None,
            table_page: 0,
            table_rows: None,
            table_sort: None,
            query_result: None,
            query_target: None,
            query_pk: Vec::new(),
            query_sql: None,
            query_elapsed_ms: None,
            query_loaded: 0,
            query_has_more: false,
            query_page: 0,
            result_tabs: Vec::new(),
            active_result_tab: 0,
            result_replace_active: false,
            stmt_queue: VecDeque::new(),
            stmt_states: HashMap::new(),
            stmt_states_text: String::new(),
            plan_view: None,
            plan_next: false,
            browse_open_pending: false,
            plan_flat: false,
            column_widths: Vec::new(),
            col_resize: None,
            columns_mode_widths: COLUMNS_MODE_DEFAULT_WIDTHS,
            columns_mode_resize: None,
            results_visible: true,
            hidden_columns: BTreeSet::new(),
            result_x_scroll: ScrollHandle::new(),
            result_v_scroll: Arc::new(UniformListScrollHandle::new()),
            sql_diagnostics: Vec::new(),
            selected_row: None,
            checked_rows: BTreeSet::new(),
            batch_column: None,
            batch_value: text_input(window, cx, tr(cx, "要写入该列的新值")),
            batch_null: false,
            pending_edits: Vec::new(),
            pending_inserts: Vec::new(),
            pending_undo: Vec::new(),
            pending_redo: Vec::new(),
            table_filters: Vec::new(),
            filter_column: None,
            filter_op: FilterOp::Eq,
            filter_value: text_input(window, cx, tr(cx, "过滤值…")),
            table_pk: Vec::new(),
            table_ddl: None,
            table_indexes: Vec::new(),
            table_row_count: None,
            table_columns: Vec::new(),
            table_foreign_keys: Vec::new(),
            table_triggers: Vec::new(),
            table_meta_for: None,
            row_editor: None,
            chart_view: None,
            result_view: ResultViewMode::Grid,
            result_area_tab: ResultAreaTab::Data,
            exec_log: Vec::new(),
            page_jump,
            history: load_history(cx),
            saved_queries: load_saved_queries(cx),
            saved_charts: load_saved_charts(cx),
            dashboards: load_dashboards(cx),
            tunnel_profiles: load_tunnel_profiles(cx),
            dashboard_results: HashMap::new(),
            dashboard_collapsed: BTreeSet::new(),
            dashboard_editing: false,
            right_dock: RightDock::Detail,
            right_dock_auto: false,
            right_dock_subscribed: false,
            grid_focus: cx.focus_handle(),
            history_segment: HistorySegment::Recent,
            cell_detail: None,
            cell_editor: None,
            query_page_size_override: None,
            page_size_dialog: None,
            ai_open: false,
            ai_provider: None,
            ai_question: text_input(window, cx, tr(cx, "用自然语言描述要查什么…")),
            ai_explain: None,
            last_error: None,
            status: tr(cx, "就绪").into(),
            toast: None,
            toast_seq: 0,
            busy: false,
            running: None,
            stop_requested: false,
            run_timeout_secs: QUERY_TIMEOUT.as_secs(),
            run_seq: 0,
            query_timeout_override: None,
            query_timeout_input: text_input(window, cx, tr(cx, "留空跟随连接默认")),
            tree_open: std::collections::HashSet::new(),
        }
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    /// 弹一条**底部居中 toast**（dbx 同款动作回执）：约 [`TOAST_LIFETIME`] 后自消。
    ///
    /// 序号 + 定时器：定时器到点时只有 `seq` 还是自己那条才清 —— 否则两秒内连点两次
    /// 复制，第一条的定时器会把第二条一起抹掉，第二次等于没弹。后来的 toast 直接替换
    /// 前一条（不堆叠），所以页面上永远只有一枚。
    pub(super) fn toast(
        &mut self,
        tone: notify::Tone,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.toast = Some(Toast {
            message: message.into(),
            tone,
            seq,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_LIFETIME).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|toast| toast.seq == seq) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 写剪贴板，并给一条底部 toast 回执（复制类动作共用的收尾）。
    ///
    /// 不再写状态栏：那是常驻读数栏，复制这种「看完就走」的回执塞进去会一直被后面
    /// 的查询读数顶掉，反而看不到。
    pub(super) fn put_on_clipboard(
        &mut self,
        text: String,
        note: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.toast(notify::Tone::Success, note.into(), cx);
    }

    /// 主工作区顶部的**全局操作组**（dbx `AppToolbar` 对位）。
    ///
    /// 限定在主工作区内（标签条上方），不横跨窗口标题栏 —— 第三轮明确要求。
    /// 数据传输 / 驱动管理（dbx 同款入口）**没有后端**，按设计稿「灰显 = 无后端」
    /// 的口径摆成禁用态：看得见入口、点不动，不冒充功能（P3-6 判据）。
    pub(super) fn render_global_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let has_session = self.active_session().is_some();

        // 下拉 / 按钮文案先按当前语言取好：菜单构建闭包是 `'static`，进不去 `&App`。
        let new_connection_label = tr(cx, "新建连接");
        let new_query_label = tr(cx, "新建查询");
        let transfer_label = tr(cx, "数据传输");
        let drivers_label = tr(cx, "驱动管理");
        let plugins_label = tr(cx, "插件中心");
        let refresh_label = tr(cx, "刷新");
        let more_label = tr(cx, "更多");
        let import_label = tr(cx, "导入连接");
        let export_label = tr(cx, "导出连接");
        let ai_label = tr(cx, "AI 助手");

        // 连接状态读数（dbx 的连接身份一眼可见；设计稿 globalbar 右端那块读数位）。
        let conn_status: SharedString = match self.active_config() {
            Some(cfg) => {
                SharedString::from(format!("{} · {}", tr(cx, "已连接"), cfg.display_name()))
            }
            None => tr(cx, "未连接").into(),
        };

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xxs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 左组（设计稿 globalbar 同款文字按钮，dbx `AppToolbar` 同序）。
            .child(
                icon_text_button(
                    "db-global-new-connection",
                    new_connection_label,
                    Icon::new(DuckIcon::Plug).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_new_connection_picker(window, cx);
                        });
                    }
                }),
            )
            .child(
                icon_text_button(
                    "db-global-new-query",
                    new_query_label,
                    Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                // 没有活跃连接就无处开查询标签：禁用而不是静默不反应。
                .disabled(!has_session)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if let Some(conn) = this.active.clone() {
                                this.open_tab(&conn, window, cx);
                            }
                        });
                    }
                }),
            )
            // 设计稿 globalbar 左组的前两枚（dbx `AppToolbar` 同款顺序：数据传输 →
            // 驱动管理 → 插件中心）。没有对应后端，摆禁用态而非空壳。
            .child(
                icon_text_button(
                    "db-global-transfer",
                    transfer_label,
                    Icon::new(DuckIcon::ArrowRightLeft).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(true),
            )
            .child(
                icon_text_button(
                    "db-global-drivers",
                    drivers_label,
                    Icon::new(DuckIcon::HardDrive).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(true),
            )
            .child(
                icon_text_button(
                    "db-global-plugins",
                    plugins_label,
                    Icon::new(DuckIcon::Box).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                // 打开的是 shell 级插件中心窗口（与标题栏入口同一条路由）。
                .on_click(move |_, _, cx| crate::pages::plugin_center::open_plugin_center(cx)),
            )
            .child(
                icon_text_button(
                    "db-global-refresh",
                    refresh_label,
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(!has_session || self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.refresh_state(window, cx));
                    }
                }),
            )
            .child({
                let more_view = view.clone();
                toolbar_text_dropdown_button(
                    "db-global-more",
                    more_label,
                    Icon::new(DuckIcon::Ellipsis).size(extended.icon.sm),
                    Anchor::BottomLeft,
                    move |menu, _, _| {
                        let import_view = more_view.clone();
                        let export_view = more_view.clone();
                        menu.item(PopupMenuItem::new(import_label).on_click(
                            move |_, window, cx| {
                                let Some(entity) = import_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.import_connections(window, cx);
                                });
                            },
                        ))
                        .item(
                            PopupMenuItem::new(export_label).on_click(move |_, window, cx| {
                                let Some(entity) = export_view.upgrade() else {
                                    return;
                                };
                                entity.update(cx, |this, cx| {
                                    this.export_connections(window, cx);
                                });
                            }),
                        )
                    },
                    cx,
                )
            })
            // 中间弹性带。
            .child(div().flex_1().min_w_0())
            // 右组（设计稿 globalbar right）：AI 助手 + 连接状态读数。
            .child(
                icon_text_button(
                    "db-global-ai",
                    ai_label,
                    Icon::new(DuckIcon::Sparkles).size(extended.icon.sm),
                    if self.ai_open {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Ghost
                    },
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.ai_open = !this.ai_open;
                            cx.notify();
                        });
                    }
                }),
            )
            .child(meta_text(conn_status, cx))
            .into_any_element()
    }

    /// 连接切换下拉（编辑器工具条右端，dbx 同款）。
    ///
    /// 列出**打开中的标签所属的连接**（同一连接多个标签只列一次），选中即切到
    /// 该连接最早的一个查询标签。没有打开的标签就不画。
    pub(super) fn connection_picker(&self, cx: &App) -> Option<AnyElement> {
        let view = self.view.clone();
        let mut seen: Vec<&str> = Vec::new();
        let options: Vec<(SharedString, SharedString)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let conn = self.tab_conn.get(tab)?;
                if seen.contains(&conn.as_str()) {
                    return None;
                }
                seen.push(conn);
                let cfg = self.connections.iter().find(|cfg| &cfg.id == conn)?;
                Some((
                    SharedString::from(conn.clone()),
                    SharedString::from(cfg.display_name()),
                ))
            })
            .collect();
        if options.is_empty() {
            return None;
        }
        let current = self.active.clone()?;
        Some(dropdown_select(
            "db-conn-picker",
            options,
            SharedString::from(current),
            move |value: SharedString, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    this.activate_conn(&value, window, cx);
                });
            },
            cx,
        ))
    }

    /// 命名空间选择下拉（库 / Schema，由引擎分层决定）。
    ///
    /// 编辑器工具条与右栏共用一份构造；`id` 由调用方给（两处不能撞 ElementId）。
    /// 返回 `None` = 引擎没有命名空间层（如 SQLite 单文件），不画。
    pub(super) fn namespace_picker(&self, id: &'static str, cx: &App) -> Option<AnyElement> {
        let session = self.active_session()?;
        let view = self.view.clone();
        // 下拉里列的是**库**还是**库内的 schema**，由引擎决定（见 `namespace_is_database`）。
        // 库这一层优先用 `databases`（「这条连接上有哪些库」），拿不到才退回 `schemas`。
        let layer_is_database = namespace_is_database(session.kind);
        let names: Vec<String> = if layer_is_database {
            collect_namespaces(&session.databases, &session.schemas)
        } else {
            session.schemas.clone()
        };
        let current = if layer_is_database {
            session.database.clone()
        } else {
            session.schema.clone()
        };
        if names.is_empty() && current.is_none() {
            return None;
        }
        let options: Vec<(SharedString, SharedString)> = names
            .iter()
            .map(|name| {
                (
                    SharedString::from(name.clone()),
                    SharedString::from(name.clone()),
                )
            })
            .collect();
        // 当前值可能不在清单里（引擎缺省命名空间没被 `list_schemas` 报出来）→
        // 原样展示，选别的才会触发切换。
        let current = current.unwrap_or_else(|| {
            tr(
                cx,
                if layer_is_database {
                    "当前数据库"
                } else {
                    "当前命名空间"
                },
            )
            .to_string()
        });
        Some(dropdown_select(
            id,
            options,
            SharedString::from(current),
            move |value: SharedString, window, cx| {
                let Some(entity) = view.upgrade() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    // 库这一层必须走「真切会话」的那条路；schema 层保持原样。
                    if layer_is_database {
                        this.select_database(value.to_string(), window, cx);
                    } else {
                        this.select_schema(Some(value.to_string()), cx);
                    }
                });
            },
            cx,
        ))
    }

    /// 「加载更多」条：结果被行上限截断、且能续拉时，出现在网格下方。
    pub(super) fn render_load_more_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                button(
                    "db-load-more",
                    tr(cx, "加载更多"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.load_more_query(window, cx));
                    }
                }),
            )
            .child(meta_text(
                SharedString::from(trf(
                    cx,
                    "已加载 {} 行；结果被行上限截断",
                    &[&self.query_loaded.to_string()],
                )),
                cx,
            ))
            .into_any_element()
    }

    /// 主区：编辑器 + 结果网格（或空态）。
    ///
    /// 编辑器与结果区之间是一条**可拖拽的下边框**。两个面板都**不传初始尺寸**：
    /// `ResizablePanel` 在 `initial_size` 为 None 时走 `flex_grow_1 + flex_shrink_1`，
    /// 两块**默认各占一半**；首帧 prepaint 把实测尺寸记进 `ResizableState`，此后拖拽
    /// 与容器伸缩都由组件按比例自管（`adjust_to_container_size`）。把手本体
    /// （hairline + hover 胶囊 + 光标）由 gpui-component 画，本页只给编辑器面板
    /// `SQL_EDITOR_HEIGHT_RANGE` 的拖拽档位。
    pub(super) fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        // 实例信息页走另一条正文（不是「编辑器 + 结果区」）：它没有 SQL 可编辑，
        // 也不产出结果集，硬塞进结果区会让人以为「查询返回了这些」。
        if let Mode::Instance(tab) = self.mode {
            return self.render_instance_page(tab, cx);
        }
        // 仪表板同理走另一条正文：它一屏是**好几块**面板，没有「当前结果集」这东西，
        // 塞进结果区既摆不下也会让人以为「查询返回了这些」。
        if let Mode::Dashboard(board_id) = self.mode {
            return self.render_dashboard_page(board_id, cx);
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let height_range = px(*SQL_EDITOR_HEIGHT_RANGE.start())..px(*SQL_EDITOR_HEIGHT_RANGE.end());
        // 逐条 ▶ 的定位数据：语句起始行 × 行高 − 编辑器滚动偏移（行高读编辑器
        // 实际布局值，未布局完退回 mono_md 档；偏移由 `cx.observe(&sql)` 带动重渲跟随）。
        let sql_text = self.sql.read(cx).value().to_string();
        let editor_line_height = self
            .sql
            .read(cx)
            .line_height()
            .unwrap_or(tokens.typography.mono_md.line_height);
        let editor_scroll_y = self.sql.read(cx).scroll_offset().y;
        let statements: Vec<(usize, String)> = split_statement_spans(&sql_text)
            .iter()
            .map(|span| {
                let line = sql_text.as_bytes()[..span.start]
                    .iter()
                    .filter(|&&byte| byte == b'\n')
                    .count();
                (line, span.text.clone())
            })
            .collect();

        // 编辑器行（执行列 + 编辑器本体）：结果区收起 / 展开两种布局共用一份构造。
        // ⚠️ 这里**不能用 `h_flex()`**：它的 cross-axis 规则是「居中」，编辑器会被
        // 收成一行内容高。要用显式 `.flex().flex_row()` 拿默认的 stretch——左右两列
        // 才都能撑满面板高度（执行列靠 `pt` 把 ▶ 顶到第一行）。
        let editor_row = div()
            .flex()
            .flex_row()
            .size_full()
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 左侧执行列（dbx 同款）：每条**完整语句**一个 ▶，
            // 钉在语句首行外侧、只执行那一条（与全文同一条危险
            // 确认 / 执行路径）；执行全文走工具条「执行」或 ⌘↵。
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(theme.density().control + tokens.spacing.xs)
                    .overflow_hidden()
                    .children(statements.iter().enumerate().map(|(index, (line, text))| {
                        let button_height = theme.density().control;
                        // 首行文本从编辑器内边距之下起画（见
                        // `EDITOR_TOP_PADDING` 注释），漏加它整列
                        // 图标会悬在对应行上方。滚动偏移取**加号**：
                        // gpui 的滚动偏移「向下滚 = 负值」，行内容
                        // 与 ▶ 同吃这一个偏移 —— 写成减号图标就会
                        // 与文本反向移动（滚动时整列乱飘的根因）。
                        let top = editor_line_height * *line as f32
                            + px(EDITOR_TOP_PADDING)
                            + editor_scroll_y
                            + (editor_line_height - button_height) / 2.0;
                        let statement = text.clone();
                        // 执行状态标记（设计稿 gutter marker）：
                        // 没跑过 = ▶；跑过 = 运行中 / 成功 /
                        // 失败。运行中不可点；**成功 / 失败保留
                        // 可点**（dbx 同款：状态图标即重跑按钮
                        // —— 点一下就再执行这一条），只是图标
                        // 换成结果色让人一眼看到上次跑成没有。
                        match self.stmt_states.get(&index).copied() {
                            Some(StmtRunState::Running) => {
                                let (icon, color) =
                                    (DuckIcon::RotateCw, extended.colors.text_tertiary);
                                div()
                                    .absolute()
                                    .top(top)
                                    .left_0()
                                    .size(button_height)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(Icon::new(icon).size(extended.icon.sm).text_color(color))
                                    .into_any_element()
                            }
                            state => {
                                let (icon, color) = match state {
                                    Some(StmtRunState::Ok) => {
                                        (DuckIcon::CircleCheck, extended.colors.success)
                                    }
                                    Some(StmtRunState::Failed) => {
                                        (DuckIcon::CircleAlert, tokens.colors.destructive)
                                    }
                                    _ => unreachable!("None and Running are handled above"),
                                };
                                icon_button(
                                    SharedString::from(format!("db-stmt-run-{index}")),
                                    SharedString::from(trf(
                                        cx,
                                        "执行第 {} 条语句",
                                        &[&(index + 1).to_string()],
                                    )),
                                    Icon::new(icon).size(extended.icon.sm).text_color(color),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .disabled(self.busy)
                                .on_click({
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.run_statement(statement.clone(), index, window, cx)
                                        });
                                    }
                                })
                                // absolute 子项：top 已含滚动修正，
                                // 相对本列定位（列自身 overflow_hidden 裁剪）。
                                .absolute()
                                .top(top)
                                .left_0()
                                .into_any_element()
                            }
                        }
                    })),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Editor::new(&self.sql)
                        .bordered(false)
                        .p_0()
                        .h_full()
                        .w_full()
                        .text_size(tokens.typography.mono_md.size),
                ),
            );

        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(tokens.colors.background)
            .child(self.render_editor_toolbar(cx))
            // 查询历史不再挂在编辑器上方：它是**右侧栏 dock 的一个分段**
            // （见 [`Self::render_right_dock_tabs`]），工具条那个时钟按钮只负责切分段。
            .when(self.ai_open, |body| body.child(self.render_ai_bar(cx)))
            // 执行失败提示条只保留**结果区**那一条（带 AI 动作）—— 编辑器上方不再
            // 重复画一条：同一句「执行失败：…」上下各出现一次，用户看到的就是
            // 「错误出了 2 条」。AI 解释面板仍挂在编辑器上方（只展示正文）。
            .when(self.ai_explain.is_some(), |body| {
                body.child(self.render_explain_panel(cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(if self.results_visible {
                        v_resizable("db-sql-split")
                            .with_state(&self.sql_split)
                            .child(
                                resizable_panel().size_range(height_range).child(
                                    v_flex()
                                        .size_full()
                                        .min_h_0()
                                        .child(editor_row.flex_1().min_h_0())
                                        .children(self.render_sql_diagnostics_bar(cx)),
                                ),
                            )
                            .child(resizable_panel().child(self.render_result(cx)))
                            .into_any_element()
                    } else {
                        // 收起结果区：编辑器独占主区。不挂 resizable —— 上次的
                        // 分栏尺寸留在 `sql_split` 里，重新展开即原样恢复。
                        v_flex()
                            .size_full()
                            .min_h_0()
                            .child(editor_row.flex_1().min_h_0())
                            .children(self.render_sql_diagnostics_bar(cx))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    pub(super) fn active_config(&self) -> Option<&ConnectionConfig> {
        let id = self.active.as_deref()?;
        self.connections.iter().find(|cfg| cfg.id == id)
    }

    /// 底部居中 toast 浮层（dbx 同款动作回执）；没有可弹的就返回 `None`。
    ///
    /// 锚在**状态栏上沿之上**居中：既不遮结果表格的主体，也不压住状态栏那条常驻读数。
    /// 外层是整幅宽的居中容器、本身不接鼠标事件（没有 hitbox），所以底边这条浮层
    /// 不会挡住下面网格的点击 —— 挡住交互的浮层比没有浮层更烦人。
    pub(super) fn render_toast(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let toast = self.toast.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let body = tokens.typography.sm;
        Some(
            div()
                .absolute()
                .left(px(0.))
                .right(px(0.))
                .bottom(theme.density().status_bar + tokens.spacing.lg)
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .id("db-toast")
                        // 阅读宽度上限（约 40 个中文字符）：再长的反馈在正文里已经截断，
                        // 不属 spacing 阶梯，保留裸值。
                        .max_w(px(520.))
                        .items_center()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.md)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.lg)
                        .border(extended.stroke.thin)
                        .border_color(extended.colors.hairline)
                        .bg(extended.colors.chrome)
                        // 浮层要压过网格，给一点投影把「浮起来」画出来。
                        .shadow_md()
                        .child(
                            Icon::new(toast.tone.icon_name())
                                .size(extended.icon.md)
                                .text_color(toast.tone.color(cx)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(body.size)
                                .line_height(body.line_height)
                                .text_color(tokens.colors.foreground)
                                .child(toast.message.clone()),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 底部状态栏。
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        h_flex()
            .flex_none()
            .h(theme.density().status_bar)
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(div().truncate().child(self.status.clone())),
            )
            // 事务状态：`BEGIN` 挂着的时候给一个显眼的档位 + 两个出口（提交 / 回滚）。
            // 连接是常驻的，语句之间的事务由服务器维持——这里只负责把它显示出来。
            .when(
                self.active_session().is_some_and(|session| session.in_tx),
                |bar| {
                    let commit_view = self.view.clone();
                    let rollback_view = self.view.clone();
                    bar.child(accent_tag(tr(cx, "事务中"), cx))
                        .child(
                            button("db-tx-commit", tr(cx, "提交"), ButtonVariant::Secondary, cx)
                                .disabled(self.busy)
                                .on_click(move |_, window, cx| {
                                    let _ = commit_view.update(cx, |this, cx| {
                                        this.execute_sql("COMMIT".to_owned(), None, window, cx);
                                    });
                                }),
                        )
                        .child(
                            button(
                                "db-tx-rollback",
                                tr(cx, "回滚"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(self.busy)
                            .on_click(move |_, window, cx| {
                                let _ = rollback_view.update(cx, |this, cx| {
                                    this.execute_sql("ROLLBACK".to_owned(), None, window, cx);
                                });
                            }),
                        )
                },
            )
            // 待提交档位：暂存了改动但还没提交时一直挂着——「攒着没落库」这件事
            // 必须在关窗口之前看得见（关掉页面暂存就丢了）。
            .when(self.pending_total() > 0, |bar| {
                bar.child(accent_tag(
                    SharedString::from(trf(
                        cx,
                        "待提交 {} 条",
                        &[&self.pending_total().to_string()],
                    )),
                    cx,
                ))
            })
            .child(meta_text(
                SharedString::from(trf(cx, "{} 个连接", &[&self.connections.len().to_string()])),
                cx,
            ))
            // 错误徽标：有**还没看过**的失败就挂在这里。它是状态栏里唯一一个「有事发生」
            // 的信号 —— 通知会自己消失，而这个不会，直到用户真的去看了一眼流水。
            //
            // 点它带着**最近那次的关联 id** 跳审计页：那是用户唯一能对上号的东西
            //（提示语里带的是同一个 id）。
            .when_some(crate::audit::pending_failure(cx), |bar, pending| {
                let trace = pending.trace().map(ToOwned::to_owned);
                let tip = SharedString::from(trf(cx, "最近一次：{}", &[pending.summary()]));
                bar.child(
                    icon_text_button(
                        "db-status-audit-errors",
                        SharedString::from(trn(cx, "{} 条待查看错误", pending.count() as i64)),
                        Icon::new(DuckIcon::CircleAlert)
                            .size(extended.icon.sm)
                            .text_color(extended.colors.status_failed),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        // 先落盘再跳：审计的写是去抖的，不 flush 的话刚记的那条还没进库，
                        // 用户点进来看到的会是一张缺了「刚刚那次」的列表。
                        duck_store::flush(cx);
                        crate::audit::clear_failure(cx);
                        crate::audit::open_audit(cx, trace.clone());
                        cx.notify();
                        window.refresh();
                    })),
                )
            })
            .into_any_element()
    }
}

impl Render for DatabasePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 右栏的开合归 shell 管，而工具条上「历史」按钮的高亮要跟着它走 —— 首次渲染
        // 时订阅一次（页面是 `AnyView`，shell 重绘不会连带重绘它）。
        self.ensure_right_dock_subscribed(cx);
        // 连接的新建 / 编辑走弹窗子页（`open_edit_dialog`），主区始终是
        // 「全局操作组 + 标签条 + 编辑器 / 结果」——不再有「主区变身表单」这一态。
        //
        // 全局操作组**落在主工作区内**（本 v_flex 的顶端），不横跨窗口标题栏：
        // 这是第三轮的明确要求（对齐 dbx 的 AppToolbar，但不与 shell 的标题栏抢位）。
        let content = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_global_actions(cx))
            .child(self.render_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_body(cx)),
            )
            .into_any_element();

        let mut panel = SidebarPanel::new("db-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            // 键位落在页面根上：焦点在 SQL 编辑器里时，上下文栈是
            // `Editor → … → Database → Root`，页面级的 ⌘↵ / ⌘S 在更浅的一层生效，
            // 编辑器的 ⌘Z / ⌘. 仍归编辑器自己（更深优先）。
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &RunOrStopQuery, window, cx| {
                if this.running.is_some() {
                    this.stop_running(window, cx);
                } else {
                    this.run_query(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CommitPendingEdits, window, cx| {
                this.commit_pending(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DiscardPendingEdits, window, cx| {
                this.discard_pending(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoPendingEdits, _window, cx| {
                this.undo_pending(cx);
            }))
            .on_action(cx.listener(|this, _: &RedoPendingEdits, _window, cx| {
                this.redo_pending(cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectedRow, _window, cx| {
                this.copy_selected_row(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllRows, _window, cx| {
                this.toggle_all_checked(cx);
            }))
            .on_action(cx.listener(|this, _: &CutSelectedCell, _window, cx| {
                this.cut_selected_cell(cx);
            }))
            .on_action(cx.listener(|this, _: &PasteIntoSelectedCell, _window, cx| {
                this.paste_into_selected_cell(cx);
            }))
            .on_action(cx.listener(|this, _: &StopQuery, window, cx| {
                this.stop_running(window, cx);
            }))
            .child(div().flex_1().min_h_0().min_w_0().child(panel))
            .child(self.render_status_bar(cx))
            // 浮层放最后：绝对定位、贴在根容器（默认 `relative`）的状态栏上沿。
            .children(self.render_toast(cx))
    }
}
