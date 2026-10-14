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
    /// - `explain`：正文是**解释文本**，只放进 [`Self::ai_explain`] 展示，永不执行。
    pub(super) fn install_ai_result(
        &mut self,
        action: AiAction,
        text: String,
        tokens: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            AiAction::Explain => {
                let explanation = text.trim().to_owned();
                if explanation.is_empty() {
                    notify::info(tr(cx, "AI 没有返回解释内容。"), window, cx);
                    return;
                }
                self.ai_explain = Some(explanation);
                self.status = trf(cx, "AI 已给出解释 · {} tokens", &[&tokens.to_string()]).into();
                cx.notify();
            }
            AiAction::Generate | AiAction::Fix => {
                // 闸门是**纯函数**（`duck_db::sqlintel::allow_auto_execute`）：恰好一条且
                // 可证明只读才自动执行；写 / 多条 / 混合（含数据修改型 CTE、`SELECT …
                // INTO`、`EXPLAIN ANALYZE <写>`、赋值型 PRAGMA）一律只填编辑器。
                if allow_auto_execute(&text) {
                    // 单条只读：立刻执行（可取消 + 查询超时），与手敲执行同一条路径。
                    self.execute_ai_sql(text, tokens, window, cx);
                } else {
                    // 写 / 多条 / 混合：只填编辑器，人工审阅后自己跑——**不自动执行写**。
                    self.sql.update(cx, |state, cx| {
                        state.set_value(text, window, cx);
                    });
                    self.status = trf(
                        cx,
                        "AI 产出的写 / 多条语句已填入编辑器（{} tokens），请审阅后执行",
                        &[&tokens.to_string()],
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

    /// AI 助手条：厂商选择 + 提问输入 + 生成按钮。
    ///
    /// 厂商档案与 API key **不再在 db 页单独维护**：这里只从助手页那份档案列表里
    /// 选一条（[`crate::ai::load_providers`]），key 走**同一条** vault 密文通道
    /// （[`crate::ai::load_api_key`]）。没配过任何厂商时给出去「AI 助手」页配置的提示。
    pub(super) fn render_ai_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();
        let providers = crate::ai::load_providers(cx);
        let active = self.active_ai_provider(cx);
        let options: Vec<(SharedString, SharedString)> = crate::ai::model_options(&providers)
            .into_iter()
            .map(|(id, label)| (SharedString::from(id), SharedString::from(label)))
            .collect();
        let mut bar = v_flex().w_full().gap(tokens.spacing.xs);

        // 厂商行：有档案给下拉，没有就给一句「去助手页配置」的提示。
        if options.is_empty() {
            bar = bar.child(h_flex().w_full().items_center().child(meta_text(
                tr(
                    cx,
                    "尚未配置 AI 厂商，请到「AI 助手」页添加厂商档案与 API key。",
                ),
                cx,
            )));
        } else {
            let current = SharedString::from(
                active
                    .as_ref()
                    .map(|profile| profile.id.clone())
                    .unwrap_or_default(),
            );
            bar = bar.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(dropdown_select(
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
                    )),
            );
        }

        bar = bar.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.ai_question)
                            .h(theme.density().control)
                            .text_size(tokens.typography.sm.size)
                            .w_full(),
                    ),
                )
                .child(
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
        );
        card(cx)
            .id("db-ai-bar")
            .w_full()
            .flex_none()
            .p(tokens.spacing.lg)
            .child(bar)
            .into_any_element()
    }

    /// AI 解释面板：展示 `explain` 动作的正文；**只展示，绝不执行**。
    pub(super) fn render_explain_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(text) = self.ai_explain.clone() else {
            return div().into_any_element();
        };
        v_flex()
            .w_full()
            .flex_none()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(tokens.colors.surface)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(caption_heading(tr(cx, "AI 解释"), cx))
                    .child(
                        button(
                            "db-ai-explain-close",
                            tr(cx, "关闭"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.ai_explain = None;
                                    cx.notify();
                                });
                            }
                        }),
                    ),
            )
            // 解释可能很长：面板有高度上限，超出自己滚，不把结果区挤没。
            .child(
                div()
                    .id("db-ai-explain-body")
                    .w_full()
                    .max_h(px(AI_EXPLAIN_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(text)),
            )
            .into_any_element()
    }
}
