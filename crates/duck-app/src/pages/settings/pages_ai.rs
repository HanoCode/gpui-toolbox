//! 设置页「AI」：厂商档案、编辑器、生成参数与知识库。

use super::*;

impl SettingsWindowView {
    // ── AI ──────────────────────────────────────────────────────────────
    /// 「AI」页：模型厂商清单（就地展开编辑器）+ 生成参数。
    ///
    /// # 为什么是单列，而不是「左列表 + 右表单」
    ///
    /// 设置窗口本身已经是一条「左导航 + 右内容」的两栏。再嵌一层同形态的两栏，
    /// 界面上就有两条纵向导航、两组「选中态」，用户分不清哪条管什么。厂商通常只有
    /// 一两个，卡片清单 + 展开式编辑器够用，还少一份「当前选中哪条」的状态要维护。
    ///
    /// # 凭据状态为什么不问存储
    ///
    /// 见 [`crate::ai`] 的模块头：渲染路径上每帧解密一次是不行的。
    ///「配过密钥没有」只读内存里那份档案标记（`api_key.is_some()`，不落盘、不解密），
    /// 真正的解密只发生在**发送前**与**编辑弹窗回显**两处。
    pub(super) fn ai_body(&mut self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();

        // 别处（助手页 / 上一次会话）改了配置就重新读一份快照。
        // 只比一个整数：整份 JSON 比对留给真正需要逐字段比较的地方。
        if ai::revision(cx) != self.ai_revision {
            self.ai_reload(cx);
        } else if self.ai_kb.is_none() && self.ai_kb_error.is_none() {
            // 首次进这一页才碰 `ai.db`。放在 `else` 里是刻意的：刚 `ai_reload` 过就
            // 不必再读一遍。作用域也刻意限在 AI 页——从没配过 AI 的用户不该凭空多一个库文件。
            self.ai_kb_reload();
        }

        let secure = self.ai_secure_available;
        let configured = ai::load_configured(cx);

        let mut provider_rows: Vec<AnyElement> = vec![
            option_row(
                tr(cx, "添加厂商"),
                Some(
                    tr(
                        cx,
                        "一条记录 = 一个端点 + 一个模型。API key 加密存在本机，不写进设置文件。",
                    )
                    .into(),
                ),
                button(
                    "duck-settings-ai-add",
                    tr(cx, "添加"),
                    ButtonVariant::Primary,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.ai_start_draft(window, cx, None);
                })),
                cx,
            )
            .into_any_element(),
        ];

        if self.ai_providers.is_empty() {
            provider_rows.push(ai_hint_row(
                tr(
                    cx,
                    "还没有配置厂商。要用云端模型，先点上面的按钮选一个模板并填入 API key；只想本地跑就选 Ollama。",
                ),
                cx,
            ));
        }

        let editing_id = self.ai_draft.as_ref().and_then(|draft| draft.id.clone());
        for profile in self.ai_providers.clone() {
            let has_key = configured.iter().any(|item| item == &profile.id);
            let editing = editing_id.as_deref() == Some(profile.id.as_str());
            provider_rows.push(self.ai_provider_row(&profile, has_key, secure, editing, cx));
        }

        let mut sections: Vec<AnyElement> = vec![
            settings_section(Some(tr(cx, "模型厂商").into()), provider_rows, cx).into_any_element(),
        ];
        if self.ai_draft.is_some() {
            sections.push(self.ai_editor_body(cx));
        }
        sections.push(self.ai_generation_section(cx));
        sections.push(self.ai_tools_section(cx));
        sections.push(self.ai_kb_section(cx));

        v_flex().w_full().gap(tokens.spacing.lg).children(sections)
    }

    /// 重载「AI」页的三份快照。写入之后与版本号变化时各调一次。
    pub(super) fn ai_reload(&mut self, cx: &mut Context<Self>) {
        self.ai_providers = ai::load_providers(cx);
        self.ai_settings = ai::load_settings(cx);
        self.ai_secure_available = ai::secure_storage_available();
        self.ai_revision = ai::revision(cx);
        // 配置变过 ⇒ 工作目录 / 来源清单都可能跟着变，索引快照一并作废重读。
        self.ai_kb_reload();
        cx.notify();
    }

    /// 读一次知识库快照（来源清单 + 统计）。
    ///
    /// **不在渲染路径上逐帧调**：`KbIndex::open_default()` 会打开 SQLite 文件。
    /// 调用点只有两处：本页初始化、以及配置变更后的 [`Self::ai_reload`]。
    /// 这两处本来就要做一次 `secret::probe`（真读系统存储），开销同量级。
    ///
    /// 失败**不 panic 也不清空**：库打不开时把原因写进 [`Self::ai_kb_error`]，
    /// 界面照常显示开关与目录选择，只是把索引块换成一行错误说明——
    /// 「AI 页整页不能用」和「索引读不出来」是两件事。
    pub(super) fn ai_kb_reload(&mut self) {
        match KbIndex::open_default().and_then(|index| {
            let sources = index.sources()?;
            let stats = index.stats()?;
            Ok(KbSnapshot { sources, stats })
        }) {
            Ok(snapshot) => {
                self.ai_kb = Some(snapshot);
                self.ai_kb_error = None;
            }
            Err(error) => {
                self.ai_kb = None;
                self.ai_kb_error = Some(error.to_string().into());
            }
        }
    }

    /// 厂商清单里的一行：展示名 + 模型标签 + 端点/凭据说明 + 右侧「编辑 / 删除」。
    pub(super) fn ai_provider_row(
        &self,
        profile: &duck_ai::ProviderProfile,
        has_key: bool,
        secure: bool,
        editing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let id = profile.id.clone();

        let model = if profile.model.trim().is_empty() {
            tr(cx, "未选模型").to_owned()
        } else {
            profile.model.clone()
        };
        let credential = ai_credential_label(profile, has_key, secure, cx);

        let actions = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(
                button(
                    SharedString::from(format!("duck-settings-ai-edit-{id}")),
                    if editing {
                        tr(cx, "收起")
                    } else {
                        tr(cx, "编辑")
                    },
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let id = id.clone();
                    cx.listener(move |this, _, window, cx| {
                        if editing {
                            this.ai_draft = None;
                            cx.notify();
                            return;
                        }
                        // 先把档案抄出来：`ai_start_draft` 要 `&mut self`。
                        let target = this.ai_providers.iter().find(|item| item.id == id).cloned();
                        if let Some(target) = target {
                            this.ai_start_draft(window, cx, Some(&target));
                        }
                    })
                }),
            )
            .child(
                button(
                    SharedString::from(format!("duck-settings-ai-remove-{id}")),
                    tr(cx, "删除"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click({
                    let id = id.clone();
                    cx.listener(move |this, _, window, cx| {
                        this.ai_delete_provider(&id, window, cx);
                    })
                }),
            );

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(ai::profile_display_name(profile))),
                    )
                    .child(content::neutral_tag(SharedString::from(model), cx))
                    .child(div().flex_1().min_w_0())
                    .child(actions),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(format!(
                        "{} · {}",
                        profile.base_url, credential
                    ))),
            )
            .into_any_element()
    }

    /// 厂商编辑器：模板 / 名称 / 端点 / 模型 / 密钥 + 取消 / 保存。
    ///
    /// 只读 `self`：所有写入都走按钮回调，编辑器本身不持有可变借用，
    /// 免得「一边读草稿一边改草稿」在同一帧里打架。
    pub(super) fn ai_editor_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();

        let Some(draft) = self.ai_draft.as_ref() else {
            return div().into_any_element();
        };

        let template = duck_ai::template(&draft.template_id).copied();
        let needs_key = !template.is_some_and(|entry| entry.is_local);
        let secure = self.ai_secure_available;
        let has_key = draft
            .id
            .as_ref()
            .is_some_and(|id| ai::load_configured(cx).iter().any(|item| item == id));
        let key_url = template.and_then(|entry| entry.key_url);

        let title = match draft.id.as_deref() {
            Some(id) => {
                let name = self
                    .ai_providers
                    .iter()
                    .find(|item| item.id == id)
                    .map(ai::profile_display_name)
                    .unwrap_or_else(|| id.to_owned());
                SharedString::from(trf(cx, "编辑：{}", &[&name]))
            }
            None => SharedString::from(tr(cx, "添加厂商")),
        };

        let mut rows: Vec<AnyElement> = Vec::new();

        // 模板：只列**当前可用**的。协议没接入的模板若出现在这里，用户能选中、
        // 能保存，但第一次发请求才失败——那正是设计文档里点名不许的「静默失败」。
        rows.push(
            option_row(
                tr(cx, "模板"),
                Some(tr(cx, "切换模板会把端点与模型重填成该模板的默认值。").into()),
                dropdown_select(
                    "duck-settings-ai-template",
                    duck_ai::TEMPLATES
                        .iter()
                        .filter(|entry| entry.is_available())
                        .map(|entry| {
                            (SharedString::from(entry.id), SharedString::from(entry.name))
                        }),
                    SharedString::from(draft.template_id.clone()),
                    cx.processor(|this, value: SharedString, window, cx| {
                        this.ai_apply_template(value.as_ref(), window, cx);
                    }),
                    cx,
                ),
                cx,
            )
            .into_any_element(),
        );

        rows.push(
            option_row(
                tr(cx, "名称"),
                Some(tr(cx, "展示用，留空回落模板名。").into()),
                div().w(px(AI_FIELD_WIDTH)).child(
                    Input::new(&draft.name)
                        .control(cx)
                        // 名字用本行的可见标题：读屏读到「名称」才知道这是哪一格。
                        .aria_label(tr(cx, "名称"))
                        .w_full(),
                ),
                cx,
            )
            .into_any_element(),
        );

        rows.push(
            option_row_stacked(
                tr(cx, "端点"),
                Some(
                    tr(
                        cx,
                        "OpenAI 兼容前缀，不带尾斜杠；请求发到该前缀下的 /chat/completions。",
                    )
                    .into(),
                ),
                Input::new(&draft.base_url)
                    .control(cx)
                    .aria_label(tr(cx, "端点"))
                    .w_full(),
                cx,
            )
            .into_any_element(),
        );

        // 下拉里的候选：拉到了服务端清单就用它，否则回落模板自带的那几个。
        let live = !draft.live_models.is_empty();
        let presets: Vec<(SharedString, SharedString)> = if live {
            draft
                .live_models
                .iter()
                .map(|model| {
                    (
                        SharedString::from(model.clone()),
                        SharedString::from(model.clone()),
                    )
                })
                .collect()
        } else {
            template
                .map(|entry| {
                    entry
                        .default_models
                        .iter()
                        .map(|model| (SharedString::from(*model), SharedString::from(*model)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let model_hint: SharedString = if draft.models_loading {
            SharedString::from(tr(cx, "正在拉取模型清单…"))
        } else if let Some(error) = draft.models_error.clone() {
            error
        } else if live {
            SharedString::from(trf(
                cx,
                "已拉到 {} 个模型。",
                &[&draft.live_models.len().to_string()],
            ))
        } else {
            SharedString::from(tr(cx, "直接填模型名；从「预置模型」里挑一个会覆盖当前值。"))
        };
        let mut model_controls = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&draft.model)
                        .control(cx)
                        .aria_label(tr(cx, "模型"))
                        .w_full(),
                ),
            )
            .child(
                // 端点填错 / 密钥不对，这里一次就能看出来 —— 不必等到发第一句对话。
                button(
                    "duck-settings-ai-fetch-models",
                    tr(cx, "拉取模型"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(cx.listener(|this, _, _window, cx| this.ai_fetch_models(cx))),
            );
        if !presets.is_empty() {
            // `current` 故意写成选项里没有的字符串：按钮上就显示标签本身
            //（「预置模型」/「服务端清单」），而不是某个已选项 —— 它是一个动作而不是一个状态。
            model_controls = model_controls.child(dropdown_select(
                "duck-settings-ai-model-preset",
                presets,
                SharedString::from(if live {
                    tr(cx, "服务端清单")
                } else {
                    tr(cx, "预置模型")
                }),
                cx.processor(|this, value: SharedString, window, cx| {
                    this.ai_fill_model(value.as_ref(), window, cx);
                }),
                cx,
            ));
        }
        rows.push(
            option_row_stacked(tr(cx, "模型"), Some(model_hint), model_controls, cx)
                .into_any_element(),
        );

        if needs_key {
            let mut key_controls = h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.sm)
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&draft.api_key)
                            .control(cx)
                            .aria_label(tr(cx, "API key"))
                            .w_full(),
                    ),
                );
            if let Some(url) = key_url {
                key_controls = key_controls.child(
                    button(
                        "duck-settings-ai-key-url",
                        tr(cx, "去申请密钥"),
                        ButtonVariant::Link,
                        cx,
                    )
                    // 打不开就如实说：静默失败的按钮和假按钮是一回事
                    // （`overview.rs` 的「查看文档」同款处置）。
                    .on_click(move |_, window, cx| {
                        if let Err(error) = duck_platform::open_url(url) {
                            notify::error(
                                trf(cx, "打不开链接：{}", &[&error.to_string()]),
                                window,
                                cx,
                            );
                        }
                    }),
                );
            }
            let description = if !secure {
                SharedString::from(tr(
                    cx,
                    "本机凭据存储不可用，密钥存不下；请改用本地端点（Ollama）。",
                ))
            } else if has_key {
                SharedString::from(tr(cx, "已存过密钥。留空表示不改动，填入则覆盖。"))
            } else {
                SharedString::from(tr(cx, "加密存在本机，设置文件里不留明文。"))
            };
            rows.push(
                option_row_stacked(tr(cx, "API key"), Some(description), key_controls, cx)
                    .into_any_element(),
            );
        }

        if let Some(error) = draft.error.clone() {
            rows.push(
                div()
                    .w_full()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.md)
                    .child(
                        div()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.destructive)
                            .child(error),
                    )
                    .into_any_element(),
            );
        }

        rows.push(
            option_row(
                tr(cx, "保存"),
                Some(tr(cx, "保存后立刻出现在助手页的模型下拉里。").into()),
                h_flex()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        button(
                            "duck-settings-ai-cancel",
                            tr(cx, "取消"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai_draft = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "duck-settings-ai-save",
                            tr(cx, "保存"),
                            ButtonVariant::Primary,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.ai_save_draft(window, cx)),
                        ),
                    ),
                cx,
            )
            .into_any_element(),
        );

        settings_section(Some(title), rows, cx).into_any_element()
    }
    /// 「生成参数」分区：默认厂商 + 温度 / 上限 + 系统提示词。
    pub(super) fn ai_generation_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();

        let options: Vec<(SharedString, SharedString)> = std::iter::once((
            SharedString::from(""),
            SharedString::from(tr(cx, "跟随第一条")),
        ))
        .chain(self.ai_providers.iter().map(|profile| {
            (
                SharedString::from(profile.id.clone()),
                SharedString::from(ai::profile_display_name(profile)),
            )
        }))
        .collect();
        let current = SharedString::from(
            self.ai_settings
                .default_provider
                .clone()
                .unwrap_or_default(),
        );

        let provider_row = option_row(
            tr(cx, "默认厂商"),
            Some(tr(cx, "打开助手页时选中哪一条。").into()),
            dropdown_select(
                "duck-settings-ai-default-provider",
                options,
                current,
                cx.processor(|this, value: SharedString, _, cx| {
                    this.ai_settings.default_provider = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                    ai::save_settings(cx, &this.ai_settings);
                    cx.notify();
                }),
                cx,
            ),
            cx,
        );

        let temperature_row = option_row(
            tr(cx, "温度"),
            Some(tr(cx, "0 ~ 2；留空用服务端默认。").into()),
            div().w(px(AI_FIELD_WIDTH)).child(
                Input::new(&self.ai_temperature)
                    .control(cx)
                    .aria_label(tr(cx, "温度"))
                    .w_full(),
            ),
            cx,
        );

        let max_tokens_row = option_row(
            tr(cx, "最大输出"),
            Some(tr(cx, "输出 token 上限；留空用服务端默认。").into()),
            div().w(px(AI_FIELD_WIDTH)).child(
                Input::new(&self.ai_max_tokens)
                    .control(cx)
                    .aria_label(tr(cx, "最大输出"))
                    .w_full(),
            ),
            cx,
        );

        let max_rounds_row = option_row(
            tr(cx, "最大工具轮次"),
            Some(tr(cx, "一次问答内最多允许的连续工具调用轮数；留空用默认值 8。").into()),
            div().w(px(AI_FIELD_WIDTH)).child(
                Input::new(&self.ai_max_rounds)
                    .control(cx)
                    .aria_label(tr(cx, "最大工具轮次"))
                    .w_full(),
            ),
            cx,
        );

        // 上下文预算：**既说清代价、也不假装它是精确值**。关掉压缩 = 长会话可能超窗；
        // 调太小 = 更早丢历史。两个方向都由用户明知代价地选。
        let compaction_row = option_row(
            tr(cx, "上下文预算"),
            Some(
                tr(
                    cx,
                    "整段历史超过这个字符数就开始压缩：先裁旧工具结果，再丢最早的消息。留空用默认值 96000；填 0 表示关闭压缩，长会话可能因此超出模型的上下文窗口。",
                )
                .into(),
            ),
            div().w(px(AI_FIELD_WIDTH)).child(
                Input::new(&self.ai_compaction_budget)
                    .control(cx)
                    .aria_label(tr(cx, "上下文预算"))
                    .w_full(),
            ),
            cx,
        );

        // 多行输入框：`ControlExt` 只管单行 `Input`，所以外观（圆角 / 描边 / 底色）
        // 照助手页输入区那套自己拼一遍，两处的视觉才一致。
        let prompt_row = option_row_stacked(
            tr(cx, "系统提示词"),
            Some(tr(cx, "每次请求都作为 system 消息发出；留空则不加。").into()),
            div()
                .w_full()
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(tokens.colors.border)
                .bg(tokens.colors.surface)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .child(
                    Textarea::new(&self.ai_system_prompt)
                        .appearance(false)
                        .aria_label(tr(cx, "系统提示词"))
                        .w_full()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height),
                ),
            cx,
        );

        let save_row = option_row(
            tr(cx, "保存生成参数"),
            Some(tr(cx, "非法输入（例如温度 2.5、上限 0）会被拒绝，不落盘。").into()),
            button(
                "duck-settings-ai-save-settings",
                tr(cx, "保存"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| this.ai_save_generation(window, cx))),
            cx,
        );

        settings_section(
            Some(tr(cx, "生成参数").into()),
            [
                provider_row.into_any_element(),
                temperature_row.into_any_element(),
                max_tokens_row.into_any_element(),
                max_rounds_row.into_any_element(),
                compaction_row.into_any_element(),
                prompt_row.into_any_element(),
                save_row.into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }

    /// 助手开关的说明文案——**跟着实际状态走**。
    ///
    /// 写死一句「可以读文件、列目录、检索知识库」在没配工作目录时是假的：
    /// `ai_tools::build_registry` 此时只注册 `search_kb`。本仓 P3-6 明令不许在界面里
    /// 摆与后端不符的说明，所以这里按 `enabled × workspace` 三态各说各的。
    pub(super) fn ai_tools_hint(enabled: bool, workspace: Option<&str>, cx: &App) -> &'static str {
        match (enabled, workspace.is_some()) {
            (false, _) => tr(cx, "关闭时助手只能聊天，不注册任何工具。"),
            (true, false) => tr(cx, "已打开；还没选工作目录，所以只有知识库检索可用。"),
            (true, true) => tr(
                cx,
                "已打开；助手可以读取工作目录里的文件、列出目录、检索本地知识库。",
            ),
        }
    }

    /// 「助手工具」分区：工具总开关 + 工作目录。
    ///
    /// 标题刻意避开「工具」二字——设置页已有一个「工具」分类，管的是工具箱那批工具
    /// （`duck-tools` 的注册表、启停与工具级设置）。这里是**助手能调用的工具**，
    /// 归在 AI 分类下（`ai-design.md` §4.4：厂商与助手设置放设置页，不放助手页内）。
    pub(super) fn ai_tools_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let enabled = self.ai_settings.tools_enabled;
        let workspace = self.ai_settings.workspace_dir.clone();

        let switch_row = option_row(
            tr(cx, "允许助手调用工具"),
            Some(Self::ai_tools_hint(enabled, workspace.as_deref(), cx).into()),
            switch_control(
                "duck-settings-ai-tools-enabled",
                enabled,
                cx.listener(|this, value, _, cx| {
                    // 直接改本页快照再落盘。快照与 `kv` 的一致性由 `ai_revision`
                    // 兜底：`save_settings` 会把版本号 +1，下一帧 `ai_reload` 重读一遍。
                    this.ai_settings.tools_enabled = *value;
                    ai::save_settings(cx, &this.ai_settings);
                    cx.notify();
                }),
            ),
            cx,
        );

        // 工作目录走竖排行：绝对路径是长串，塞进 `option_row` 右侧会把标题栏挤扁。
        let workspace_row = option_row_stacked(
            tr(cx, "工作目录"),
            Some(
                tr(
                    cx,
                    "文件类工具只能访问这个目录里的内容；含 .. 的路径一律拒绝。",
                )
                .into(),
            ),
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.md)
                .child(div().flex_1().min_w_0().child(match workspace.as_deref() {
                    Some(path) => meta_text(path.to_owned(), cx),
                    None => meta_text(tr(cx, "未选择"), cx),
                }))
                .child(
                    button(
                        "duck-settings-ai-pick-workspace",
                        tr(cx, "选择目录"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.ai_pick_workspace(cx))),
                )
                .when(workspace.is_some(), |this| {
                    this.child(
                        button(
                            "duck-settings-ai-clear-workspace",
                            tr(cx, "清除"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai_settings.workspace_dir = None;
                            ai::save_settings(cx, &this.ai_settings);
                            cx.notify();
                        })),
                    )
                }),
            cx,
        );

        settings_section(
            Some(tr(cx, "助手工具").into()),
            [
                switch_row.into_any_element(),
                workspace_row.into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }

    /// 「本地知识库」分区：来源清单 + 添加 / 重建 / 停止 + 进度日志。
    pub(super) fn ai_kb_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let mut rows: Vec<AnyElement> = Vec::new();

        if let Some(error) = self.ai_kb_error.clone() {
            rows.push(ai_hint_row(trf(cx, "读不出知识库索引：{}", &[&error]), cx));
        }
        let sources = self
            .ai_kb
            .as_ref()
            .map(|snapshot| snapshot.sources.clone())
            .unwrap_or_default();
        if self.ai_kb_error.is_none() && sources.is_empty() {
            rows.push(ai_hint_row(tr(cx, "还没有索引任何目录。"), cx));
        }
        for source in &sources {
            rows.push(self.ai_kb_source_row(source, cx));
        }

        let running = self.ai_kb_job.as_ref().is_some_and(|job| !job.finished);
        let status: SharedString = if running {
            tr(cx, "正在索引…").into()
        } else if self.ai_kb_error.is_some() {
            // 错误已经用一整行说过了，状态位不再重复。
            SharedString::default()
        } else {
            match &self.ai_kb {
                Some(snapshot) if snapshot.stats.sources > 0 => trf(
                    cx,
                    "已索引 {} 个目录、{} 块",
                    &[
                        &snapshot.stats.sources.to_string(),
                        &snapshot.stats.chunks.to_string(),
                    ],
                )
                .into(),
                Some(_) => tr(cx, "从未索引").into(),
                None => SharedString::default(),
            }
        };

        let mut log: Vec<AnyElement> = Vec::new();
        if let Some(job) = &self.ai_kb_job {
            for line in &job.lines {
                log.push(
                    div()
                        .w_full()
                        .min_w_0()
                        .child(meta_text(line.clone(), cx))
                        .into_any_element(),
                );
            }
        }

        // 索引跑着时**只给「停止」**：连点两次会让两个线程往同一个库里写整源重建。
        let actions = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.md)
            .child(
                button(
                    "duck-settings-ai-kb-add",
                    tr(cx, "添加目录"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.ai_kb_add(cx))),
            )
            .child(if running {
                button(
                    "duck-settings-ai-kb-stop",
                    tr(cx, "停止"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, _| this.ai_kb_stop()))
            } else {
                button(
                    "duck-settings-ai-kb-rebuild",
                    tr(cx, "重建索引"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.ai_kb_rebuild(cx)))
            })
            .child(div().flex_1().min_w_0().child(meta_text(status, cx)));

        rows.push(
            v_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .child(actions)
                .when(!log.is_empty(), |this| {
                    this.child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xxs)
                            .font_family(MONO_FAMILY)
                            .children(log),
                    )
                })
                .into_any_element(),
        );

        settings_section(Some(tr(cx, "本地知识库").into()), rows, cx).into_any_element()
    }

    /// 知识库来源清单里的一行：目录名 + 绝对路径 + 「移除」。
    pub(super) fn ai_kb_source_row(&self, source: &KbSource, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let id = source.id.clone();
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.md)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.sm)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(tokens.spacing.xxs)
                    .child(body_text(source.label.clone(), cx))
                    .child(meta_text(source.path.to_string_lossy().into_owned(), cx)),
            )
            .child(
                button(
                    SharedString::from(format!("duck-settings-ai-kb-remove-{id}")),
                    tr(cx, "移除"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.ai_kb_remove(&id, cx))),
            )
            .into_any_element()
    }

    /// 选一个工作目录写进设置。
    ///
    /// `pick_folder()` 是**模态阻塞**调用（`duck-platform/src/dialog.rs` 模块头），
    /// 只能在动作处理器里调——这里正是。
    pub(super) fn ai_pick_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(path) = duck_platform::pick_folder() else {
            return;
        };
        self.ai_settings.workspace_dir = Some(path.to_string_lossy().into_owned());
        ai::save_settings(cx, &self.ai_settings);
        cx.notify();
    }

    /// 「添加目录」：选目录 → 立刻索引它。
    ///
    /// **添加即索引**，不留「登记了但没索引」的中间态：`kb_sources` 表的语义就是
    /// 「已索引的来源」，`KbIndex::index_source` 本身也是先扫后 upsert。空目录（扫不到
    /// 可索引文本）会让它返回 `Err`，来源因此不会被登记——不需要额外做一次回滚。
    pub(super) fn ai_kb_add(&mut self, cx: &mut Context<Self>) {
        if self.ai_kb_job.as_ref().is_some_and(|job| !job.finished) {
            return;
        }
        let Some(path) = duck_platform::pick_folder() else {
            return;
        };
        let job = self.spawn_kb_job(KbJobKind::Add(KbSource::new(path)), cx);
        self.ai_kb_job = Some(job);
        self.start_kb_poller(cx);
        cx.notify();
    }

    /// 「重建索引」：清空块表后按已登记的来源整源重扫。
    pub(super) fn ai_kb_rebuild(&mut self, cx: &mut Context<Self>) {
        if self.ai_kb_job.as_ref().is_some_and(|job| !job.finished) {
            return;
        }
        let job = self.spawn_kb_job(KbJobKind::Rebuild, cx);
        self.ai_kb_job = Some(job);
        self.start_kb_poller(cx);
        cx.notify();
    }

    /// 移除一个来源（连同它的块）。
    ///
    /// 与添加 / 重建不同，这一步**留在 UI 线程同步做**：两条 `DELETE`（一条走 FTS5 索引），
    /// 没有目录遍历，耗时与一次普通的库写入同量级，再起一个线程只是徒增复杂度。
    pub(super) fn ai_kb_remove(&mut self, id: &str, cx: &mut Context<Self>) {
        match KbIndex::open_default().and_then(|index| index.remove_source(id)) {
            Ok(()) => {
                self.ai_kb_reload();
                cx.notify();
            }
            Err(error) => {
                self.ai_kb_error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }

    /// 请求停止当前索引任务。
    ///
    /// 置位后线程在**下一批块之间**退出，不是立刻——正在写的那几百块要让事务收尾。
    /// 幂等：重复点没有额外效果。
    pub(super) fn ai_kb_stop(&mut self) {
        if let Some(job) = &self.ai_kb_job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 把一次索引任务丢进专用线程，返回前台要持有的那半。
    ///
    /// 专用线程而不是 `background_executor`：一次全盘扫描是分钟级的长任务，
    /// 占住共享 worker 会连累别的异步活儿（`duck-store` 写线程、助手页流式同款处置）。
    pub(super) fn spawn_kb_job(&self, kind: KbJobKind, cx: &mut Context<Self>) -> KbJob {
        let (sender, receiver) = mpsc::channel::<KbEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        // 线程只拿「这次要索引哪个来源」，不拿整个 `KbJobKind`：
        // 收尾时怎么合并清单是前台的事，线程不关心。
        let thread_source = match &kind {
            KbJobKind::Rebuild => None,
            KbJobKind::Add(source) => Some(source.clone()),
        };

        let spawned = std::thread::Builder::new()
            .name("duck-ai-index".into())
            .spawn(move || {
                let mut progress = |event: IndexProgress| {
                    let _ = sender.send(KbEvent::Progress(event));
                };
                let outcome = (|| -> Result<(Option<KbSource>, KbStats), duck_ai::AiError> {
                    // 线程自己开一条连接：`KbIndex` 不做跨线程共享，
                    // SQLite 的 WAL 允许它与助手页那条连接并存。
                    let mut index = KbIndex::open_default()?;
                    match &thread_source {
                        None => {
                            let stats = index.rebuild(&[], &thread_cancel, &mut progress)?;
                            Ok((None, stats))
                        }
                        Some(source) => {
                            index.index_source(source, &thread_cancel, &mut progress)?;
                            let stats = index.stats()?;
                            Ok((Some(source.clone()), stats))
                        }
                    }
                })();
                let event = match outcome {
                    Ok((added, stats)) => KbEvent::Done {
                        added,
                        stats: Some(stats),
                        error: None,
                    },
                    Err(error) => KbEvent::Done {
                        added: None,
                        stats: None,
                        error: Some(error.to_string()),
                    },
                };
                let _ = sender.send(event);
            });

        if let Err(failure) = spawned {
            // 线程都没起来：给一个「已收尾」的任务，让界面把原因显示出来，
            // 而不是留一个永远转不完的「正在索引…」。
            return KbJob {
                receiver,
                cancel: Arc::new(AtomicBool::new(false)),
                lines: vec![trf(cx, "无法启动索引线程：{}", &[&failure.to_string()]).into()],
                finished: true,
            };
        }

        KbJob {
            receiver,
            cancel,
            lines: Vec::new(),
            finished: false,
        }
    }

    /// 起索引进度轮询任务。
    ///
    /// **刻意不做「已在跑就跳过」的幂等判断**：调用方（[`Self::ai_kb_add`] /
    /// [`Self::ai_kb_rebuild`]）已经用 `job.finished` 把「有任务在跑」挡掉了；而上一轮
    /// 轮询任务在 `drain_kb_job` 报 `false` 时**自己返回**，`Task` 完成却**不会**把字段
    /// 变回 `None`。留着幂等判断只会把新任务挡在门外——助手页 `send()` 踩过同一个坑。
    pub(super) fn start_kb_poller(&mut self, cx: &mut Context<Self>) {
        self.ai_kb_poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(KB_POLL_INTERVAL).await;
                match this.update(cx, |this, cx| this.drain_kb_job(cx)) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                }
            }
        }));
    }

    /// 取走索引线程的消息；返回是否还要继续轮询。
    pub(super) fn drain_kb_job(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(job) = self.ai_kb_job.as_mut() else {
            return false;
        };
        let mut done: Option<KbEvent> = None;
        loop {
            match job.receiver.try_recv() {
                Ok(KbEvent::Progress(progress)) => {
                    job.lines.push(kb_progress_line(&progress, cx).into());
                    if job.lines.len() > KB_LOG_LIMIT {
                        job.lines.remove(0);
                    }
                }
                Ok(event @ KbEvent::Done { .. }) => {
                    done = Some(event);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // 线程没了却没发收尾事件（panic 在遍历里）：当成失败收尾，
                // 否则界面会一直停在「正在索引…」。
                Err(mpsc::TryRecvError::Disconnected) => {
                    done = Some(KbEvent::Done {
                        added: None,
                        stats: None,
                        error: Some(tr(cx, "索引线程意外结束").to_owned()),
                    });
                    break;
                }
            }
        }

        let Some(KbEvent::Done {
            added,
            stats,
            error,
        }) = done
        else {
            // 还没收尾：重绘一次把新来的日志行画出去。
            cx.notify();
            return true;
        };

        // 收尾。任务**不**从字段里清掉：把最后的日志留在屏上给用户看，
        // 下一次点「添加 / 重建」时整体替换（`finished` 就是给按钮分支看的）。
        let job = self.ai_kb_job.as_mut().expect("kb job checked above");
        job.finished = true;
        if let Some(reason) = error {
            job.lines.push(trf(cx, "索引失败：{}", &[&reason]).into());
        } else {
            job.lines.push(tr(cx, "索引完成").into());
            let mut snapshot = self.ai_kb.take().unwrap_or(KbSnapshot {
                sources: Vec::new(),
                stats: KbStats::default(),
            });
            if let Some(source) = added
                && !snapshot.sources.iter().any(|item| item.id == source.id)
            {
                snapshot.sources.push(source);
                // 与 `KbIndex::sources()` 的 `ORDER BY path` 对齐，添加后顺序不跳。
                snapshot.sources.sort_by(|a, b| a.path.cmp(&b.path));
            }
            if let Some(stats) = stats {
                snapshot.stats = stats;
            }
            self.ai_kb = Some(snapshot);
            self.ai_kb_error = None;
        }
        cx.notify();
        false
    }

    /// 打开厂商编辑器。
    ///
    /// `existing` 为空 = 新增，从第一个可用模板起；否则用该档案的现值预填。
    /// **API key 会解密回填**（掩码显示）：这里是一次解密调用，不是渲染路径，
    /// 所以没有性能顾虑；换机器 / 密钥文件被删时解不开，框就是空的。
    pub(super) fn ai_start_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        existing: Option<&duck_ai::ProviderProfile>,
    ) {
        let template = existing
            .and_then(|profile| duck_ai::template(&profile.template_id))
            .or_else(|| duck_ai::TEMPLATES.iter().find(|entry| entry.is_available()));

        let template_id = template
            .map(|entry| entry.id.to_owned())
            .unwrap_or_else(|| "openai_compatible".to_owned());
        let name = existing
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let base_url = existing
            .map(|profile| profile.base_url.clone())
            .or_else(|| template.map(|entry| entry.base_url.to_owned()))
            .unwrap_or_default();
        let model = existing
            .map(|profile| profile.model.clone())
            .or_else(|| {
                template.and_then(|entry| {
                    entry
                        .default_models
                        .first()
                        .map(|value| (*value).to_owned())
                })
            })
            .unwrap_or_default();
        let id = existing.map(|profile| profile.id.clone());
        let has_key = id
            .as_ref()
            .is_some_and(|id| ai::load_configured(cx).iter().any(|item| item == id));
        let secure = self.ai_secure_available;
        // 解密回填（见本函数的文档）。解不开就是空串，框里什么都不显示。
        let api_key = existing
            .map(|profile| ai::load_api_key(cx, &profile.id).unwrap_or_default())
            .unwrap_or_default();

        // 占位符只在创建时定型（与设置页搜索框同款）。草稿是瞬时对象，
        // 不值得为它再挂一条「语言切换后重新设置占位符」的同步。
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "留空则用模板名"))
                .default_value(name)
        });
        let base_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "https://…/v1"))
                .default_value(base_url)
        });
        let model_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "模型名"))
                .default_value(model)
        });
        // 密钥框是**掩码**输入：编辑已有档案时回填解密后的值，清空即清除。
        let api_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(api_key)
                .placeholder(if !secure {
                    tr(cx, "凭据存储不可用")
                } else if has_key {
                    tr(cx, "已保存（清空则删除密钥）")
                } else {
                    tr(cx, "sk-…")
                })
        });

        // 打开就落焦在名称上：点了「编辑」多半是想改点什么，少一次点击。
        name_input.read(cx).focus_handle(cx).focus(window, cx);

        self.ai_draft = Some(AiDraft {
            id,
            template_id,
            name: name_input,
            base_url: base_url_input,
            model: model_input,
            api_key: api_key_input,
            live_models: Vec::new(),
            models_loading: false,
            models_error: None,
            error: None,
        });
        cx.notify();
    }

    /// 换模板：把端点与模型重填成该模板的默认值。
    ///
    /// 刻意**无条件覆盖**而不是「只在为空时填」：选了模板却还留着上一家的端点，
    /// 是最容易发出一次莫名其妙的请求的形态。用户想用自定义端点，
    /// 保存前在输入框里改就是了。
    pub(super) fn ai_apply_template(
        &mut self,
        template_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(template) = duck_ai::template(template_id).copied() else {
            return;
        };
        let base_url = template.base_url.to_owned();
        let model = template
            .default_models
            .first()
            .copied()
            .unwrap_or("")
            .to_owned();
        let template_id = template.id.to_owned();

        let Some(draft) = self.ai_draft.as_mut() else {
            return;
        };
        draft.template_id = template_id;
        draft
            .base_url
            .update(cx, |state, cx| state.set_value(base_url, window, cx));
        draft
            .model
            .update(cx, |state, cx| state.set_value(model, window, cx));
        cx.notify();
    }

    /// 把「预置模型」里选中的名字填进模型输入框。
    pub(super) fn ai_fill_model(
        &mut self,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.ai_draft.as_mut() else {
            return;
        };
        draft.model.update(cx, |state, cx| {
            state.set_value(model.to_owned(), window, cx)
        });
        cx.notify();
    }

    /// 「拉取模型」：向 `GET {base_url}/models` 要一次服务端的权威清单。
    ///
    /// 用的是**表单里当前的值**（端点、密钥，都还没保存）—— 这正是在保存档案之前
    /// 确认「端点写错没有、密钥对不对」的时机。测的就是真正会走的那条路。
    ///
    /// 阻塞式 HTTP 走**后台执行器**，与终端页「测试连接」同一条纪律：
    /// 在渲染线程上发请求会把整个界面冻住一次网络往返。
    pub(super) fn ai_fetch_models(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.ai_draft.as_ref() else {
            return;
        };
        if draft.models_loading {
            return;
        }
        let template_id = draft.template_id.clone();
        let profile = duck_ai::ProviderProfile {
            id: draft.id.clone().unwrap_or_default(),
            template_id: template_id.clone(),
            name: draft.name.read(cx).value().to_string(),
            base_url: draft.base_url.read(cx).value().trim().to_owned(),
            // 清单端点与模型名无关，但结构体要求非空 —— 填个占位，不去动用户的输入。
            model: String::new(),
            is_local: duck_ai::template(&template_id).is_some_and(|entry| entry.is_local),
            api_key: None,
        };
        let api_key = draft.api_key.read(cx).value().to_string();
        let Some(draft) = self.ai_draft.as_mut() else {
            return;
        };
        draft.models_loading = true;
        draft.models_error = None;
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { duck_ai::list_models(&profile, &api_key, 20) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            // 后台线程只负责发请求；回主线程才动状态。
            // 窗口在等待期间关掉时 `update` 会失败，这时没什么可做的 —— 丢掉即可。
            let _ = this.update(cx, |this, cx| {
                let Some(draft) = this.ai_draft.as_mut() else {
                    return;
                };
                draft.models_loading = false;
                match result {
                    Ok(models) => {
                        draft.models_error = None;
                        draft.live_models = models;
                    }
                    Err(error) => {
                        // 失败就把这份清单丢掉：留着上一次的结果会让用户以为
                        // 自己看的是刚拉到的。模板兜底仍在（另一份字段）。
                        draft.live_models.clear();
                        draft.models_error = Some(SharedString::from(trf(
                            cx,
                            "拉取失败：{}",
                            &[&error.to_string()],
                        )));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 保存草稿。
    ///
    /// 顺序是**先密钥、后档案**：密钥写不进去就别改档案，否则界面显示「已保存」
    /// 而请求必然失败。校验失败与写入失败都停在同一处，草稿原样留着让用户改。
    pub(super) fn ai_save_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.ai_draft.as_ref() else {
            return;
        };
        let template_id = draft.template_id.clone();
        let is_local = duck_ai::template(&template_id).is_some_and(|entry| entry.is_local);
        let name = draft.name.read(cx).value().trim().to_owned();
        let base_url = draft.base_url.read(cx).value().trim().to_owned();
        let model = draft.model.read(cx).value().trim().to_owned();
        let api_key = draft.api_key.read(cx).value().trim().to_owned();
        let id = draft.id.clone().unwrap_or_else(ai::new_profile_id);

        // 密钥：非空就重新加密（空 = 清除，与回显口径配套）；本地端点不留密钥。
        // 加密与档案**一次写入**：分两次写会出现「档案已改、密钥没改」的中间态，
        // 那正是旧实现「先存密钥、后写标记」那个顺序问题在这套结构下的对应物。
        let sealed = if is_local || api_key.is_empty() {
            None
        } else {
            match duck_ai::secret::seal(&api_key) {
                Ok(sealed) => Some(sealed),
                Err(error) => {
                    if let Some(draft) = self.ai_draft.as_mut() {
                        draft.error = Some(SharedString::from(error.to_string()));
                    }
                    cx.notify();
                    return;
                }
            }
        };

        let profile = duck_ai::ProviderProfile {
            id: id.clone(),
            template_id,
            name,
            base_url,
            model,
            is_local,
            api_key: sealed,
        };

        if let Some(failure) = profile.validate().err() {
            if let Some(draft) = self.ai_draft.as_mut() {
                draft.error = Some(SharedString::from(failure.to_string()));
            }
            cx.notify();
            return;
        }

        let mut list = self.ai_providers.clone();
        match list.iter_mut().find(|item| item.id == id) {
            Some(slot) => *slot = profile,
            None => list.push(profile),
        }
        ai::save_providers(cx, &list);

        // 默认厂商还没选过就顺手选上：省得用户再回到上面挑一次。
        if self.ai_settings.default_provider.is_none() {
            self.ai_settings.default_provider = Some(id);
            ai::save_settings(cx, &self.ai_settings);
        }

        self.ai_draft = None;
        self.ai_reload(cx);
        notify::success(tr(cx, "厂商已保存"), window, cx);
    }

    /// 删除一条厂商档案（密钥就挂在这条档案上，跟着一起没）。
    pub(super) fn ai_delete_provider(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut list = self.ai_providers.clone();
        list.retain(|item| item.id != id);
        // 密钥存在这条档案的 `api_key` 字段里，档案删了它跟着没了，
        // 不需要再去别的地方清理一次。
        ai::save_providers(cx, &list);

        if self.ai_settings.default_provider.as_deref() == Some(id) {
            self.ai_settings.default_provider = list.first().map(|item| item.id.clone());
            ai::save_settings(cx, &self.ai_settings);
        }
        if self.ai_draft.as_ref().and_then(|draft| draft.id.as_deref()) == Some(id) {
            self.ai_draft = None;
        }

        self.ai_reload(cx);
        notify::success(tr(cx, "厂商已删除"), window, cx);
    }

    /// 保存生成参数（温度 / 上限 / 系统提示词）。
    pub(super) fn ai_save_generation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(temperature) = ai::parse_temperature(&self.ai_temperature.read(cx).value()) else {
            notify::error(tr(cx, "温度要在 0 到 2 之间"), window, cx);
            return;
        };
        let Some(max_tokens) = ai::parse_max_tokens(&self.ai_max_tokens.read(cx).value()) else {
            notify::error(tr(cx, "最大输出要是正整数"), window, cx);
            return;
        };
        let Some(max_tool_rounds) = ai::parse_max_rounds(&self.ai_max_rounds.read(cx).value())
        else {
            notify::error(tr(cx, "最大工具轮次要不小于 1 的整数"), window, cx);
            return;
        };
        let Some(compaction_max_chars) =
            ai::parse_compaction_budget(&self.ai_compaction_budget.read(cx).value())
        else {
            notify::error(tr(cx, "上下文预算要是不小于 0 的整数"), window, cx);
            return;
        };
        let system_prompt = self.ai_system_prompt.read(cx).value().to_string();

        self.ai_settings.temperature = temperature;
        self.ai_settings.max_tokens = max_tokens;
        self.ai_settings.max_tool_rounds = max_tool_rounds;
        self.ai_settings.compaction_max_chars = compaction_max_chars;
        self.ai_settings.system_prompt = system_prompt;
        ai::save_settings(cx, &self.ai_settings);
        notify::success(tr(cx, "生成参数已保存"), window, cx);
        cx.notify();
    }
}
