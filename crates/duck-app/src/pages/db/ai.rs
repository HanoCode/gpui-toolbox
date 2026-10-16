//! AI 助手（text-to-SQL）：厂商档案选择、请求准备、三个动作入口（生成 / 修错 / 解释）、
//! 产出处理，以及助手条与解释面板的渲染。
//!
//! 写语句一律退回编辑器人工审批 —— [`install_ai_result`] 只让 `Explain` 直接展示，
//! `Generate` / `Fix` 产出的 SQL 先进编辑器。本文件是 `impl DatabasePage` 的一个切片，
//! 方法定义在子模块后每个都标 `pub(super)`（与 `pages::terminal` 同一口径）。

use super::*;

impl DatabasePage {
    /// 当前选中的 AI 厂商档案：显式选择 → 助手页默认档案 → 第一条。
    ///
    /// 档案是 kv 的内存快照（[`crate::ai::load_providers`]），读它不落盘也不解密；
    /// **解密**只发生在 [`Self::ai_setup`]（发请求前一次）。
    pub(super) fn active_ai_provider(&self, cx: &App) -> Option<ProviderProfile> {
        let providers = crate::ai::load_providers(cx);
        if providers.is_empty() {
            return None;
        }
        let wanted = self
            .ai_provider
            .clone()
            .or_else(|| crate::ai::load_settings(cx).default_provider);
        wanted
            .and_then(|id| providers.iter().find(|profile| profile.id == id).cloned())
            .or_else(|| providers.first().cloned())
    }

    /// 备好一次 AI 请求要用的 provider 参数与**明文 key**。
    ///
    /// 走的是助手页**同一条**「按档案 + vault 密文」通道（[`crate::ai::load_api_key`]），
    /// 与 db 页旧的 `ai:api_key` 单键通道已彻底解耦。返回 `Err(文案)` 表示配置不齐——
    /// 文案直接给用户看（提示到「AI 助手」页重新配置），不静默失败。
    pub(super) fn ai_setup(&self, cx: &App) -> Result<OpenAiCompatible, SharedString> {
        let Some(profile) = self.active_ai_provider(cx) else {
            return Err(tr(
                cx,
                "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
            )
            .into());
        };
        let display = crate::ai::profile_display_name(&profile);
        let Some(api_key) =
            crate::ai::load_api_key(cx, &profile.id).filter(|key| !key.trim().is_empty())
        else {
            return Err(trf(
                cx,
                "厂商「{}」还没配置 API key，请到「AI 助手」页配置。",
                &[&display],
            )
            .into());
        };
        Ok(OpenAiCompatible {
            base_url: profile.base_url.clone(),
            api_key,
            model: profile.model.clone(),
            timeout_secs: 60,
        })
    }

    /// DB 场景的 AI 动作入口：`generate` / `fix` / `explain`。
    ///
    /// 动作由**调用点显式传入**（错误条给 `Fix` / `Explain`，AI 助手条给 `Generate`）；
    /// `duck-db` 侧的 `pick_action` 只是给「按上下文规则选动作」留下的纯函数入口，**当前
    /// 生产路径并未调用它**（按钮已经知道要哪个动作，不必再猜一次）。
    /// schema 摘要的静态部分（表名）在主线程备好；列级信息在工作闭包内持锁快取，
    /// 取完即释放——模型调用（网络等待）绝不持连接锁。
    ///
    /// 安全边界（与既有实现一致，不放宽）：`generate` / `fix` 产出的语句过
    /// [`allow_auto_execute`]（单条 + 可证明只读）才自动执行，其余（写 / 多条 / 混合）
    /// 只填编辑器等人工审阅；`explain` 只展示文本，**永不执行**。
    pub(super) fn run_ai_action(
        &mut self,
        action: AiAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 1. 上下文：动作决定注入什么。
        let (question, sql_ctx, error_ctx) = match action {
            AiAction::Generate => (
                self.ai_question.read(cx).value().trim().to_owned(),
                None,
                None,
            ),
            AiAction::Fix | AiAction::Explain => {
                let Some(error) = self.last_error.as_ref() else {
                    notify::info(tr(cx, "当前没有可分析的执行错误。"), window, cx);
                    return;
                };
                (
                    String::new(),
                    Some(error.sql.clone()),
                    Some(error.message.clone()),
                )
            }
        };
        if action == AiAction::Generate && question.is_empty() {
            notify::info(tr(cx, "请先输入问题。"), window, cx);
            return;
        }
        // 记下**用户这一侧**的话头（对话里那一列右对齐的气泡）：
        //   · `generate` 就是他敲的原句；
        //   · `fix` / `explain` 没有输入框，用一句等价的说明代替 —— 不记的话对话里
        //     只剩助手那半边，用户回头看不知道自己为什么问。
        // 放在所有前置校验**之后**：问题没发出去就先把话记上，等于在对话里留了一条
        // 永远不会有回应的提问（「请先连接一个数据库」之后那条问题就悬在那儿了）。
        let user_line: String = match action {
            AiAction::Generate => question.clone(),
            AiAction::Fix => tr(cx, "修正上一条执行错误").to_owned(),
            AiAction::Explain => tr(cx, "解释上一条执行错误").to_owned(),
        };
        // 2. 厂商 + key：配置不齐直接提示，不静默失败。
        let provider = match self.ai_setup(cx) {
            Ok(provider) => provider,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        let Some(session) = self.active_session() else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        if self.running.is_some() {
            notify::info(
                tr(cx, "上一条语句还在跑，先等它结束或点「停止」。"),
                window,
                cx,
            );
            return;
        }
        // 3. schema 摘要的静态部分（表名清单，最多 30 表）。
        let mut schema = String::new();
        for table in session.catalog.iter().take(30) {
            schema.push_str(&table.name);
            schema.push('\n');
        }
        // 选中表必须在目录里，列级信息才值得进 prompt。
        let selected_table = self.selected_table.clone().filter(|table| {
            session
                .catalog
                .iter()
                .any(|entry| entry.table_ref() == *table)
        });
        let conn = session.conn.clone();
        // 用户那句话落进对话 —— 放在**借 `session` 那段之后**：`session` 是 `&self`
        // 借出来的，期间不能可变借 `self` 去 push。
        // 位置仍然在所有前置校验之后：问题没发出去就先记上，等于在对话里留一条
        // 永远不会有回应的提问。
        self.push_ai_message(AiRole::User, user_line, None);
        self.status = tr(cx, "正在请求 AI…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || -> Result<(String, u32), DbError> {
                // 列级 schema（持锁快查）；取完立刻释放，AI 调用不持锁。
                let mut detail = schema;
                {
                    let mut guard = conn.lock().expect("连接锁不应中毒");
                    if let Some(table) = selected_table.as_ref() {
                        let columns = guard.table_columns(table).unwrap_or_default();
                        let columns_text = columns
                            .iter()
                            .map(|column| format!("{} {}", column.name, column.type_name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        detail.push_str(&format!("{}({columns_text})\n", table.display()));
                    }
                }
                // 提示词由**纯函数工厂**组装：generate 注入问题，fix / explain 注入
                // 真实错误原文 + 原 SQL（见 `duck_db::ai::build_action_prompt`）。
                let prompt = build_action_prompt(
                    action,
                    &detail,
                    &question,
                    sql_ctx.as_deref(),
                    error_ctx.as_deref(),
                );
                let completion = provider.complete_with(action.system_prompt(), &prompt)?;
                Ok((completion.text, completion.total_tokens))
            },
            move |this, result, window, cx| match result {
                Ok((text, tokens)) => this.install_ai_result(action, text, tokens, window, cx),
                Err(error) => {
                    this.status = trf(cx, "AI 失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "AI 请求失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 处理一次 AI 调用的产出：按动作决定「自动执行 / 填编辑器 / 只展示」。
    ///
    /// - `generate` / `fix`：单条只读 → 走可取消执行路径；其余 → 只填编辑器（人工审阅）；
    /// - `explain`：正文是**解释文本**，只作为一条助手消息展示，永不执行。
    ///
    /// 三条路径**都会往对话里记一笔**（产出的 SQL 原文 + 一行「落到哪儿了」）：
    /// 助手面板的价值就在「能回看这一轮给了什么、当时是自动跑了还是等我审阅」，
    /// 只在状态栏写一句「已填入编辑器」的话，用户回头再也找不到刚才那条 SQL。
    pub(super) fn install_ai_result(
        &mut self,
        action: AiAction,
        text: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tokens_text = tokens.to_string();
        match action {
            AiAction::Explain => {
                let explanation = text.trim().to_owned();
                if explanation.is_empty() {
                    notify::info(tr(cx, "AI 没有返回解释内容。"), window, cx);
                    return;
                }
                let note = trf(cx, "解释了上一条错误 · {} tokens", &[&tokens_text]);
                self.push_ai_message(AiRole::Assistant, explanation, Some(note.into()));
                self.status = trf(cx, "AI 已给出解释 · {} tokens", &[&tokens_text]).into();
                cx.notify();
            }
            AiAction::Generate | AiAction::Fix => {
                // 闸门是**纯函数**（`duck_db::sqlintel::allow_auto_execute`）：恰好一条且
                // 可证明只读才自动执行；写 / 多条 / 混合（含数据修改型 CTE、`SELECT …
                // INTO`、`EXPLAIN ANALYZE <写>`、赋值型 PRAGMA）一律只填编辑器。
                if allow_auto_execute(&text) {
                    // 单条只读：立刻执行（可取消 + 查询超时），与手敲执行同一条路径。
                    let note = trf(cx, "{} tokens · 单条只读，已自动执行", &[&tokens_text]);
                    self.push_ai_message(
                        AiRole::Assistant,
                        text.trim().to_owned(),
                        Some(note.into()),
                    );
                    self.execute_ai_sql(text, tokens, window, cx);
                } else {
                    // 写 / 多条 / 混合：只填编辑器，人工审阅后自己跑——**不自动执行写**。
                    self.sql.update(cx, |state, cx| {
                        state.set_value(text.clone(), window, cx);
                    });
                    let note = trf(
                        cx,
                        "{} tokens · 含写 / 多条语句，已填入编辑器待审阅",
                        &[&tokens_text],
                    );
                    self.push_ai_message(AiRole::Assistant, text, Some(note.into()));
                    self.status = trf(
                        cx,
                        "AI 产出的写 / 多条语句已填入编辑器（{} tokens），请审阅后执行",
                        &[&tokens_text],
                    )
                    .into();
                    notify::info(
                        tr(cx, "AI 产出的语句需要人工审阅，已填入 SQL 编辑器。"),
                        window,
                        cx,
                    );
                }
            }
        }
    }

    /// 往对话里追加一条消息，并把消息区滚到底。
    ///
    /// 滚动由 [`ScrollHandle::scroll_to_bottom`] 在**下一次布局**里生效（它是「标记」，
    /// 不是立即写偏移），所以新增的那条消息也会被算进新的底部 —— 不必等一帧再补一次。
    pub(super) fn push_ai_message(
        &mut self,
        role: AiRole,
        text: String,
        note: Option<SharedString>,
    ) {
        self.ai_messages.push(AiMessage { role, text, note });
        self.ai_scroll.scroll_to_bottom();
    }

    /// 执行 AI 产出的**单条只读** SQL —— 与手敲执行共用「可取消 + 查询超时」路径。
    ///
    /// 与 [`Self::execute_sql`] 的差别只有两点：状态文案带 tokens（来源是 AI），
    /// 以及**函数入口处会再跑一遍 [`allow_auto_execute`]**（单条 + 可证明只读 + 无已知
    /// 副作用，不过即拒绝），再走内核 [`Connection::execute_read_only`] 二次复核，不必再走
    /// 危险确认。模型调用本身无法带外取消（只有它自带的 HTTP 超时），所以「可停」只覆盖
    /// 真正在库上跑的那一段。
    pub(super) fn execute_ai_sql(
        &mut self,
        sql: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 闸门由**代码**强制，不靠调用点自觉：走到这里就再跑一遍
        // [`allow_auto_execute`]（单条 + 可证明只读 + 无已知副作用），不过即拒绝。
        // 这样即便将来新增调用点忘了先判，也绕不过闸门（纵深防御）。
        if !allow_auto_execute(&sql) {
            self.status = tr(cx, "该语句未通过只读闸门，已拒绝自动执行。").into();
            notify::warning(
                tr(
                    cx,
                    "该语句未通过只读闸门，已拒绝自动执行（请人工审阅后再手动执行）。",
                ),
                window,
                cx,
            );
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        if self.running.is_some() {
            notify::info(
                tr(cx, "上一条语句还在跑，先等它结束或点「停止」。"),
                window,
                cx,
            );
            return;
        }
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 与 `execute_sql` 同理：取消句柄必须在开工前抓（跑起来锁在后台线程手里）。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
        let sql_for_log = sql.clone();
        // AI 只可能是单条只读 SELECT，同样按可编辑判据过一遍（`edit_target` 是纯函数，
        // 在主线程算完带进去；结果回来后再核主键覆盖）。
        let target = edit_target(&sql).map(|EditTarget { table, schema }| TableRef {
            schema,
            name: table,
        });
        // 与 `execute_sql` 同一条口径：上限在发起时读一次。
        let row_cap = query_row_cap(cx);
        self.status = tr(cx, "正在执行 AI 生成的 SQL…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id,
            cancel,
            timeout,
            move || {
                let started = Instant::now();
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 纵深防御：即便闸门已判只读，执行仍走**内核只读路径**——它会再用
                // `prove_read_only` 复核一遍，证明不了就整体拒绝，绝不退回通用 `execute`。
                guard.execute_read_only(&sql, row_cap).map(|outcome| {
                    let edit_meta = match (&target, outcome.result.as_ref()) {
                        (Some(table), Some(result)) => {
                            let pk = guard.table_primary_key(table).unwrap_or_default();
                            let projected: Vec<String> = result
                                .columns
                                .iter()
                                .map(|column| column.name.clone())
                                .collect();
                            (!pk.is_empty() && covers_columns(&projected, &pk))
                                .then(|| (table.clone(), pk))
                        }
                        _ => None,
                    };
                    (outcome, started.elapsed().as_millis(), edit_meta)
                })
            },
            move |this, result, window, cx| match result {
                Ok((outcome, elapsed_ms, edit_meta)) => {
                    this.install_query_result(outcome, edit_meta, sql_for_log.clone(), cx);
                    this.status =
                        trf(cx, "AI 已执行只读 SQL · {} tokens", &[&tokens.to_string()]).into();
                    notify::success(
                        trf(
                            cx,
                            "AI 生成的 SQL 已执行（{} ms）。",
                            &[&elapsed_ms.to_string()],
                        ),
                        window,
                        cx,
                    );
                    tracing::info!(sql = %sql_for_log, tokens, "ai executed readonly sql");
                }
                Err(error) => {
                    if matches!(error, DbError::Cancelled(_)) {
                        let text = this.cancelled_status(cx);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        return;
                    }
                    // 与手敲执行同口径：留下错误原文 + 原 SQL，供「AI 修错 / 解释」用。
                    this.last_error = Some(QueryError {
                        sql: sql_for_log.clone(),
                        message: error.to_string(),
                    });
                    // 失败也记进对话：用户回头看这一轮时，能看到「它给的 SQL 跑挂了」，
                    // 而不是只看到一条产出、以为一切顺利。
                    let failure = trf(cx, "AI 生成的 SQL 执行失败：{}", &[&error.to_string()]);
                    this.push_ai_message(AiRole::Assistant, failure, None);
                    this.status = trf(cx, "AI 执行失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "AI 生成的 SQL 执行失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// AI 助手面板（右栏 dock 的 `Ai` 分段）。
    ///
    /// 布局对齐 dbx 的 `AiAssistant`：**头部（厂商 / 清空）/ 对话区（可滚）/ 输入区（钉底）**
    /// 三段。空对话给一处带示例问题的欢迎空态 —— 第一次打开时既没有历史也没有提示的话，
    /// 面板就是一个空框，用户不知道能问什么。
    ///
    /// 厂商档案与 API key **不在 db 页单独维护**：这里只从助手页那份档案列表里选一条
    /// （[`crate::ai::load_providers`]），key 走**同一条** vault 密文通道
    /// （[`crate::ai::load_api_key`]）。
    pub(super) fn render_ai_dock(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let providers = crate::ai::load_providers(cx);
        let active = self.active_ai_provider(cx);
        let options: Vec<(SharedString, SharedString)> = crate::ai::model_options(&providers)
            .into_iter()
            .map(|(id, label)| (SharedString::from(id), SharedString::from(label)))
            .collect();

        // ── 头部：厂商（身份）+ 清空（动作）──
        //
        // 厂商的详细提示（去助手页配置）不放这儿：右栏只有两三百像素宽，一句话塞进去要
        // 折三四行，把头部撑成一大块。那句说明落在空态里，空间充裕、也正好是用户找不到
        // 北的时候会看的地方。
        let mut header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs);
        if options.is_empty() {
            header = header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(meta_text(tr(cx, "未配置 AI 厂商"), cx)),
            );
        } else {
            let current = SharedString::from(
                active
                    .as_ref()
                    .map(|profile| profile.id.clone())
                    .unwrap_or_default(),
            );
            header = header.child(div().flex_1().min_w_0().child(dropdown_select(
                "db-ai-provider",
                options,
                current,
                {
                    let view = view.clone();
                    move |value: SharedString, _window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.ai_provider = Some(value.to_string());
                            cx.notify();
                        });
                    }
                },
                cx,
            )));
        }
        header = header.child(
            icon_button_tip(
                "db-ai-clear",
                tr(cx, "清空对话"),
                Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                tr(cx, "清空对话"),
                ButtonVariant::Ghost,
                cx,
            )
            .disabled(self.ai_messages.is_empty())
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.ai_messages.clear();
                        cx.notify();
                    });
                }
            }),
        );

        // ── 对话区：自己滚（`ai_scroll` 让新消息一定可见）──
        //
        // 空 / 非空共用一个容器与一个 id：切换时 ElementId 不变，滚动状态不会被重建打断。
        let mut conversation = v_flex()
            .id("db-ai-messages")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .overflow_y_scroll()
            .track_scroll(&self.ai_scroll);
        if self.ai_messages.is_empty() {
            conversation = conversation
                .items_center()
                .justify_center()
                .child(self.render_ai_welcome(cx));
        } else {
            for message in &self.ai_messages {
                conversation = conversation.child(ai_message_bubble(message, cx));
            }
        }

        // ── 输入区：钉在底部（对话滚，它不滚）──
        let composer = card(cx).w_full().flex_none().p(tokens.spacing.sm).child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(
                    div().w_full().child(
                        Input::new(&self.ai_question)
                            .h(theme.density().control)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
                )
                .child(
                    h_flex().w_full().items_center().justify_end().child(
                        button("db-ai-run", tr(cx, "生成"), ButtonVariant::Primary, cx)
                            .disabled(self.busy)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.run_ai_action(AiAction::Generate, window, cx);
                                    });
                                }
                            }),
                    ),
                ),
        );

        v_flex()
            .size_full()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .child(header)
            .child(conversation)
            .child(composer)
            .into_any_element()
    }

    /// AI 对话空态：图标 + 一句「这里能做什么」+ 几个可以直接填进输入框的示例问题。
    ///
    /// 示例问题点一下只**填进输入框**，不直接发：发一次要花一次额度（也可能被限流），
    /// 让用户先看清要问的是什么、再自己按「生成」更合适。
    fn render_ai_welcome(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let configured = self.active_ai_provider(cx).is_some();
        let description: SharedString = if configured {
            tr(
                cx,
                "用一句自然语言提问，我把它翻成 SQL：只读语句直接执行，写语句填进编辑器等你审阅。",
            )
            .into()
        } else {
            tr(
                cx,
                "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
            )
            .into()
        };
        // 只给三条：空态里的示例是「告诉用户这东西能问什么」，不是一份用例清单。
        let samples = [
            tr(cx, "这几张表各有多少行？"),
            tr(cx, "最近 7 天每天的记录数"),
            tr(cx, "找出没有主键的表"),
        ];
        let mut chips = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap(tokens.spacing.xs);
        for (index, sample) in samples.into_iter().enumerate() {
            chips = chips.child(
                button(
                    SharedString::from(format!("db-ai-sample-{index}")),
                    sample,
                    ButtonVariant::Secondary,
                    cx,
                )
                .disabled(!configured)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let Some(entity) = view.upgrade() else {
                            return;
                        };
                        let text = sample.to_owned();
                        entity.update(cx, |this, cx| {
                            let question = this.ai_question.clone();
                            question.update(cx, |state, cx| {
                                state.set_value(text, window, cx);
                                // 填完直接给焦点：接着改两个字或直接按「生成」都顺手。
                                state.focus(window, cx);
                            });
                            cx.notify();
                        });
                    }
                }),
            );
        }
        v_flex()
            .w_full()
            .gap(tokens.spacing.md)
            .child(
                empty_state(
                    DuckIcon::Bot,
                    tr(cx, "AI 助手"),
                    Some(description),
                    None,
                    cx,
                )
                // 空态默认带虚线描边、吃满剩余高度；嵌在面板中间时两样都要收掉。
                .border_0()
                .flex_none(),
            )
            .child(chips)
            .into_any_element()
    }
}

/// 对话里的一条消息气泡：用户右对齐（强调薄底），助手左对齐（卡片底 + 细描边）。
///
/// 宽度上限取**相对值**（85%）而不是固定像素：右栏宽度是用户自己拖出来的
/// （260–400），固定上限在大宽度下显空、小宽度下又顶到边。
fn ai_message_bubble(message: &AiMessage, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let body = tokens.typography.sm;
    let meta = tokens.typography.xs;
    let mut bubble = v_flex()
        .max_w(relative(0.85))
        .gap(tokens.spacing.xxs)
        .rounded(tokens.radius.md)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs)
        .text_size(body.size)
        .line_height(body.line_height)
        .text_color(tokens.colors.foreground)
        .child(
            div()
                .w_full()
                .child(SharedString::from(message.text.clone())),
        );
    if let Some(note) = message.note.clone() {
        bubble = bubble.child(
            div()
                .w_full()
                .text_size(meta.size)
                .line_height(meta.line_height)
                .text_color(tokens.colors.muted_foreground)
                .child(note),
        );
    }
    match message.role {
        AiRole::User => h_flex()
            .w_full()
            .justify_end()
            .child(bubble.bg(extended.colors.nav_selected)),
        AiRole::Assistant => h_flex().w_full().justify_start().child(
            bubble
                .bg(tokens.colors.surface)
                .border_1()
                .border_color(extended.colors.hairline),
        ),
    }
    .into_any_element()
}
