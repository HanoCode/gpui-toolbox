//! 连接档案的增删改查与批量流转：新建 / 编辑弹窗、探测、分组、隧道档案、导入导出。
//!
//! 档案落 kv 单键 [`KEY_CONNECTIONS`]（结构体 → serde_json，范式同 `appearance`），
//! 分组与隧道档案各自一个键；三者都是「整表覆盖写」，所以本模块里每个改动入口都跟着
//! 一次 `persist_*`。探活（[`spawn_probe`]）只写 [`ConnState`]，不改档案。
//!
//! 本文件是 `impl DatabasePage` 的一个切片，方法定义在子模块后每个都标 `pub(super)`
//!（与 `pages::terminal` 同一口径）；类型定义仍在 `mod.rs`。

use super::*;

impl DatabasePage {
    /// 打开「新建连接」驱动选择弹窗（dbflux 同形：分区卡片网格 + 搜索 + 底部操作）。
    pub(super) fn open_new_connection_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = self.view.clone();
        let picker = cx.new(|cx| DriverPickerView::new(page, window, cx));
        self.open_picker_dialog(picker, window, cx);
    }

    /// 「新建连接」弹窗外壳：标题行 + 视图正文 + 底部「从文件导入… / 取消」。
    ///
    /// 正文只挂 [`DriverPickerView`] 实体 —— 搜索框要边打边重排网格，而弹窗的 `content`
    /// 闭包拿不到 `&mut DatabasePage`，实测也够不到宿主的 `notify()`（见该结构体的说明）。
    pub(super) fn open_picker_dialog(
        &self,
        picker: Entity<DriverPickerView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let view_for_import = view.clone();
            let picker_for_content = picker.clone();
            let drag_for_content = drag.clone();

            let footer = DialogFooter::new()
                .child(
                    button(
                        "db-picker-import",
                        tr(cx, "从文件导入…"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view_for_import.update(cx, |this, cx| {
                            this.import_connections(window, cx);
                        });
                    }),
                )
                .child(div().flex_1())
                .child(
                    button(
                        "db-picker-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                );

            dialog
                .w(px(PICKER_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, cx| {
                    // 正文是**视图实体**，逐帧克隆挂进来即可 —— `content` 闭包是 `Fn`，
                    // 元素树搬进来只够用一次，`Entity` 可以反复克隆。
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .child(modal_title_row(
                                "db-picker-title",
                                &drag_for_content,
                                tr(cx, "新建连接"),
                                cx,
                            ))
                            .child(picker_for_content.clone()),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 连接详情弹窗（picker 的**子页**，dbx 同形）。
    ///
    /// 表单不再占主区：picker 选中引擎（或列表铅笔）→ 关掉当前弹窗 → 同一个窗口里
    /// 开出详情弹窗，正文就是 [`EditFormView`] 这个视图实体，返回键回到 picker。
    /// 脚注「测试连接（编辑态带删除）｜取消 / 保存连接」由弹窗外壳挂。
    ///
    /// ⚠️ 两条硬约束（都试错过）：
    /// 1. 正文必须挂成视图实体（理由见 [`EditFormView`] 的说明）：环境 / SSL / SSH 这些
    ///    点了要变色的控件靠视图自己 `notify` 重绘，宿主页面的 `notify()` 到不了弹窗正文；
    /// 2. 页面**不持有**表单实体，弹窗是唯一所有者 —— 关窗即释放，同时避开
    ///    「在自身更新期间析构」那类崩溃。
    pub(super) fn open_edit_dialog(
        &self,
        form: Entity<EditFormView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = self.view.clone();
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let form_for_test = form.clone();
            let form_for_content = form.clone();
            let form_for_save = form.clone();
            let page_for_delete = page.clone();

            // 删除按钮只在编辑态出现。**先建好再按需挂**：`when_some` 的闭包若要
            // `move` 进 `cx`（`&mut App` 不 Copy），后面的 `tr(cx, …)` 会被借用检查拦下。
            let delete_slot = form.read(cx).id.clone().map(|id| {
                icon_text_button(
                    "db-edit-delete",
                    tr(cx, "删除连接"),
                    Icon::new(DuckIcon::Trash2).size(active_theme(cx).extended().icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = page_for_delete
                        .update(cx, |page, cx| page.delete_connection(&id, window, cx));
                    window.close_dialog(cx);
                })
                .into_any_element()
            });

            let footer = DialogFooter::new()
                .child(
                    icon_text_button(
                        "db-edit-test",
                        tr(cx, "测试连接"),
                        Icon::new(DuckIcon::Zap).size(active_theme(cx).extended().icon.sm),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        form_for_test.update(cx, |form, cx| form.test(window, cx));
                    }),
                )
                .when_some(delete_slot, |footer, button| footer.child(button))
                .child(div().flex_1())
                .child(
                    button(
                        "db-edit-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    button(
                        "db-edit-save",
                        tr(cx, "保存连接"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        // 校验不过时 `save` 返回 false（提示贴在字段上），弹窗留下；
                        // 只有真的写盘成功才关。
                        let saved = form_for_save.update(cx, |form, cx| form.save(window, cx));
                        if saved {
                            window.close_dialog(cx);
                        }
                    }),
                );

            dialog
                .w(px(EDIT_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, _cx| {
                    // 每帧把视图实体挂进来（`Fn` 闭包，只能逐帧克隆）。
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex().w_full().min_h_0().child(form_for_content.clone()),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 选定引擎后进入详情表单（同窗切换为表单子页，不离开弹窗）。
    pub(super) fn start_new_connection(
        &mut self,
        kind: DatabaseKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = EditFormView::new(
            self.view.clone(),
            kind,
            self.tunnel_profiles.clone(),
            window,
            cx,
        );
        let form = cx.new(|_| form);
        self.open_edit_dialog(form, window, cx);
    }

    /// 保存连接档案（表单视图校验通过后调）：按 id 更新或追加，再落 kv。
    pub(super) fn upsert_connection(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        let id = cfg.id.clone();
        match self
            .connections
            .iter_mut()
            .find(|existing| existing.id == id)
        {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
        self.persist_connections(cx);
        cx.notify();
    }

    /// 「测试连接」：用表单当前值探活（不保存、不开会话），报告版本或错误。
    ///
    /// 只收「表单已经读好」的两件套：配置 + 口令（口令的取值与回落语义在
    /// [`EditFormView::prepared`] 里，测试与保存共用同一套）。
    pub(super) fn spawn_probe(
        &mut self,
        cfg: ConnectionConfig,
        creds: Credentials,
        cx: &mut Context<Self>,
    ) {
        self.status = trf(cx, "正在测试 {}…", &[&cfg.display_name()]).into();
        cx.notify();
        // 收工回调里要报名字与 id，而 `cfg` 已经进了后台闭包。
        let done_cfg = cfg.clone();
        self.spawn_db(
            cx,
            move || probe_connection(&cfg, &creds),
            // `move`：回调要把 `done_cfg` 一起带走（它比这次调用活得久）。
            // ⚠️ 下面每个 `record_event` 的明细都先落在局部变量里再传：参数从左往右求值，
            // 写成 `record_event(cx, …, trf(cx, …))` 会「先可变借、再不可变借」同一个 `cx`。
            move |this, result, window, cx| match result {
                Ok(version) => {
                    // 探活也要留痕：它是唯一一条「不建会话就碰了服务器」的路径，
                    // 出问题时这段历史里没有别的线索。
                    let detail = trf(cx, "测试连接 {}：{}", &[&done_cfg.display_name(), &version]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Connection,
                        crate::audit::action::PROBE,
                        crate::audit::AuditOutcome::Ok,
                        &done_cfg.id,
                        detail,
                    );
                    this.status = trf(cx, "测试成功：{}", &[&version]).into();
                    notify::success(trf(cx, "连接成功：{}", &[&version]), window, cx);
                }
                Err(error) => {
                    // 「断在哪一层」由 `StageError` 决定（通道 / 连接），类别与动作键跟着走。
                    let kind = error.kind();
                    let layer = failure_layer(&error, &done_cfg, cx);
                    let detail = trf(
                        cx,
                        "测试连接 {}（{}）失败：{}",
                        &[&done_cfg.display_name(), &layer, &error.error().to_string()],
                    );
                    let reason = error.to_string();
                    let trace = crate::audit::record_event(
                        cx,
                        kind,
                        stage_action(kind),
                        crate::audit::AuditOutcome::Failed,
                        &done_cfg.id,
                        detail,
                    );
                    this.status = trf(cx, "测试失败：{}", &[&reason]).into();
                    notify::error(
                        failure_message(cx, error.label(cx), trace.as_deref(), &reason),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 删除连接（档案 + 凭据文件里的口令 + 已开会话）。
    pub(super) fn delete_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = duck_platform::confirm(
            tr(cx, "删除连接"),
            tr(cx, "连接档案与其保存的口令都会被删除，确定继续吗？"),
        );
        if !confirmed {
            return;
        }
        // 顺序要紧：`close_tab` 要从档案里取这条连接的**断开钩子**，档案必须先还在
        // （删档之后再拆会话，钩子就查不到了，等于静默跳过用户配好的收尾动作）。
        self.close_tab(id, window, cx);
        self.connections.retain(|cfg| cfg.id != id);
        self.persist_connections(cx);
        let _ = delete_password(id);
        let _ = delete_ssh_password(id);
        let _ = delete_proxy_password(id);
        notify::success(tr(cx, "连接已删除。"), window, cx);
    }

    /// 把档案数组写回 kv。
    pub(super) fn persist_connections(&self, cx: &App) {
        match serde_json::to_string(&self.connections) {
            Ok(json) => duck_store::kv_put(cx, KEY_CONNECTIONS, json),
            Err(error) => tracing::warn!(%error, "serialize db connections failed"),
        }
    }

    /// 把传输档案数组写回 kv。
    pub(super) fn persist_tunnel_profiles(&self, cx: &App) {
        match serde_json::to_string(&self.tunnel_profiles) {
            Ok(json) => duck_store::kv_put(cx, KEY_TUNNEL_PROFILES, json),
            Err(error) => tracing::warn!(%error, "serialize tunnel profiles failed"),
        }
    }

    /// 新增 / 覆盖一份传输档案（同 id 覆盖，否则追加）。返回档案 id。
    ///
    /// 交由页面来做而不是表单自己存：档案与凭据是**页面级**的东西（一份档案被多条连接
    /// 引用），表单只是它的一个编辑入口。
    pub(super) fn upsert_tunnel_profile(
        &mut self,
        profile: TunnelProfile,
        cx: &mut Context<Self>,
    ) -> String {
        let id = profile.id.clone();
        match self
            .tunnel_profiles
            .iter_mut()
            .find(|existing| existing.id == id)
        {
            Some(existing) => *existing = profile,
            None => self.tunnel_profiles.push(profile),
        }
        self.persist_tunnel_profiles(cx);
        cx.notify();
        id
    }

    /// 删一份传输档案，**并清掉引用它的连接上的引用**。
    ///
    /// 不清引用的话，那些连接会一直指着一个不存在的 id（建连时回落到自带三档，
    /// 而用户以为自己在走档案）。一并清掉，界面上就回到「自定义」这个诚实的状态。
    pub(super) fn delete_tunnel_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        self.tunnel_profiles
            .retain(|profile| profile.id != profile_id);
        self.persist_tunnel_profiles(cx);
        let mut touched = false;
        for cfg in &mut self.connections {
            if cfg.tunnel_profile.as_deref() == Some(profile_id) {
                cfg.tunnel_profile = None;
                touched = true;
            }
        }
        if touched {
            self.persist_connections(cx);
        }
        let _ = delete_profile_ssh_password(profile_id);
        let _ = delete_profile_proxy_password(profile_id);
        cx.notify();
    }

    /// 引用某份传输档案的连接条数（删档案前告知影响面）。
    pub(super) fn tunnel_profile_refs(&self, profile_id: &str) -> usize {
        self.connections
            .iter()
            .filter(|cfg| cfg.tunnel_profile.as_deref() == Some(profile_id))
            .count()
    }

    /// 把分组数组写回 kv。
    pub(super) fn persist_groups(&self, cx: &App) {
        match serde_json::to_string(&self.groups) {
            Ok(json) => duck_store::kv_put(cx, KEY_GROUPS, json),
            Err(error) => tracing::warn!(%error, "serialize db groups failed"),
        }
    }

    /// 把某条档案的某个字段改掉并落盘（当前用在「记住上次浏览的命名空间」）。
    ///
    /// 走「改 `self.connections` 再整份持久化」而不是直接写 kv：页面里那份数组是
    /// 唯一的真源，绕过它会出现「界面上是 A、磁盘上是 B」。
    pub(super) fn update_connection_field(
        &mut self,
        id: &str,
        cx: &App,
        edit: impl FnOnce(&mut ConnectionConfig),
    ) {
        let mut touched = false;
        for cfg in &mut self.connections {
            if cfg.id == id {
                edit(cfg);
                touched = true;
                break;
            }
        }
        if touched {
            self.persist_connections(cx);
        }
    }

    /// 左栏「新建分组」：弹窗输入分组名，创建后空分组也显示在左栏。
    pub(super) fn open_new_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = text_input(window, cx, tr(cx, "分组名"));
        let drag = modal_drag(cx);
        let view = self.view.clone();
        // `open_dialog` 的闭包是 `Fn + 'static`：`input` 会被整个搬进去，因此外面再留
        // 一份句柄用于开窗后聚焦（实体是 `Rc` 语义，两份指向同一个）。
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            // 形参显式标注：闭包体里 `input.read(cx)` 只要求 `&App`，若不标注，推断会
            // 把 `cx` 定成 `&App`，随后 `notify::warning(.., cx)` 需要 `&mut App` 就报
            // 「types differ in mutability」。
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写分组名。"), window, cx);
                    return false;
                }
                let _ = view.update(cx, |this, cx| {
                    if !this.groups.contains(&name) {
                        this.groups.push(name);
                        this.persist_groups(cx);
                    }
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            // 外层闭包是 `Fn`（每次重绘都会重跑）：`confirm` 与 `input` 都只能**克隆**
            // 取用，不能把捕获值搬走 —— 搬一次之后第二次调用就没得搬了。
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-group-title",
                                &drag_for_content,
                                tr(cx, "新建分组"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "分组名"), &input_for_content)),
                    ))
                })
                // 不带分号：闭包必须返回 `Dialog`。
                .footer(dialog_footer(
                    "db-group",
                    tr(cx, "创建"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |state, cx| state.focus(window, cx));
    }

    /// 导出连接档案为 JSON 文件（口令不导出——它们只存凭据文件）。
    pub(super) fn export_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connections.is_empty() {
            notify::info(tr(cx, "当前没有可导出的连接。"), window, cx);
            return;
        }
        let Some(path) = duck_platform::save_file("duck-connections.json") else {
            return;
        };
        let json = match serde_json::to_vec_pretty(&self.connections) {
            Ok(json) => json,
            Err(error) => {
                notify::error(trf(cx, "导出失败：{}", &[&error.to_string()]), window, cx);
                return;
            }
        };
        self.spawn_db(
            cx,
            move || std::fs::write(&path, json).map_err(|err| err.to_string()),
            |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "已导出连接档案。"), window, cx);
                    this.status = tr(cx, "导出完成").into();
                }
                Err(error) => {
                    notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 从 JSON 文件导入连接档案。
    ///
    /// 两种来源都收：本应用自己导出的（`Vec<ConnectionConfig>`）与第三方客户端的档案
    /// （DBeaver 的 `data-sources.json`、TablePlus 的 `.tableplusconnection`，见
    /// [`duck_db::import`]）。按 id 去重、已存在的跳过；口令一律要重新录入。
    pub(super) fn import_connections(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = duck_platform::open_file() else {
            return;
        };
        self.spawn_db(
            cx,
            move || -> Result<Vec<ConnectionConfig>, ProfileImportError> {
                let text = std::fs::read_to_string(&path)
                    .map_err(|err| ProfileImportError::Read(err.to_string()))?;
                // 先按自家档案读；读不出来再试第三方格式。
                if let Ok(profiles) = serde_json::from_str::<Vec<ConnectionConfig>>(&text) {
                    return Ok(profiles);
                }
                import_third_party(&text)
                    .map(|(_source, connections)| connections)
                    .map_err(ProfileImportError::ThirdParty)
            },
            |this, result, window, cx| match result {
                Ok(incoming) => {
                    let existing: std::collections::HashSet<String> =
                        this.connections.iter().map(|cfg| cfg.id.clone()).collect();
                    let mut added = 0usize;
                    let mut skipped = 0usize;
                    for (index, mut cfg) in incoming.into_iter().enumerate() {
                        // 第三方档案不带我们的 id（`import` 层刻意留空），在这里统一补。
                        if cfg.id.trim().is_empty() {
                            cfg.id =
                                format!("conn-{}", id_suffix(&format!("{}-{index}", cfg.name)));
                        }
                        if existing.contains(&cfg.id) || cfg.validate().is_err() {
                            skipped += 1;
                        } else {
                            this.connections.push(cfg);
                            added += 1;
                        }
                    }
                    this.persist_connections(cx);
                    cx.notify();
                    if added > 0 {
                        notify::success(
                            trf(
                                cx,
                                "已导入 {} 条连接，跳过 {} 条。",
                                &[&added.to_string(), &skipped.to_string()],
                            ),
                            window,
                            cx,
                        );
                    } else {
                        notify::info(tr(cx, "没有新增连接（id 全部重复或无效）。"), window, cx);
                    }
                }
                Err(error) => {
                    let message = match error {
                        ProfileImportError::Read(error) => trf(cx, "读取文件失败：{}", &[&error]),
                        ProfileImportError::ThirdParty(error) => {
                            import_error_message(error, cx).to_owned()
                        }
                    };
                    notify::error(trf(cx, "导入失败：{}", &[&message]), window, cx);
                }
            },
        );
    }

    /// 进入编辑表单（弹窗子页）。
    pub(super) fn edit_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let form = EditFormView::from_config(
            self.view.clone(),
            &cfg,
            self.tunnel_profiles.clone(),
            window,
            cx,
        );
        let form = cx.new(|_| form);
        self.open_edit_dialog(form, window, cx);
    }

    /// 右键「复制连接」（dbx 同款）：克隆档案为新 id、名字加「副本」，随即进编辑表单。
    ///
    /// 口令**不**复制：凭据按连接 id 存在凭据文件里（`dbconn:{id}:*`），新 id 天然
    /// 没有口令——这是安全口径不是疏漏，编辑表单让用户重填一遍。
    pub(super) fn duplicate_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut cfg) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        // `id_suffix` 混入了纳秒时间戳：同一名字连点两次「复制连接」也是两个新 id。
        cfg.id = format!("conn-{}", id_suffix(&format!("{}-copy", cfg.name)));
        cfg.name = if cfg.name.trim().is_empty() {
            format!("{} 副本", cfg.display_name())
        } else {
            format!("{} 副本", cfg.name.trim())
        };
        self.upsert_connection(cfg.clone(), cx);
        self.edit_connection(&cfg.id, window, cx);
    }

    /// 右键「移至新分组」（dbx 同款）：弹窗输入分组名，创建（如无）并把这条连接
    /// 挪进去。与「新建分组」同一套弹窗骨架；空输入按「取消」处理。
    pub(super) fn open_move_to_group_dialog(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "分组名"));
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            let id = id.to_owned();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    notify::warning(tr(cx, "请填写分组名。"), window, cx);
                    return false;
                }
                let _ = view.update(cx, |this, cx| {
                    if !this.groups.contains(&name) {
                        this.groups.push(name.clone());
                        this.persist_groups(cx);
                    }
                    this.update_connection_field(&id, cx, |cfg| cfg.group = Some(name.clone()));
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let input_for_content = input.clone();
            dialog
                .w(px(GROUP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-move-group-title",
                                &drag_for_content,
                                tr(cx, "移至新分组"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "分组名"), &input_for_content)),
                    ))
                })
                .footer(dialog_footer(
                    "db-move-group",
                    tr(cx, "移动"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |state, cx| state.focus(window, cx));
    }
}

/// 环境标签的展示文本（`None` 不展示）。
pub(super) fn env_tag_label(tag: EnvTag, cx: &App) -> Option<SharedString> {
    let label = match tag {
        EnvTag::None => return None,
        EnvTag::Dev => tr(cx, "开发"),
        EnvTag::Test => tr(cx, "测试"),
        EnvTag::Staging => tr(cx, "预发布"),
        EnvTag::Prod => tr(cx, "生产"),
    };
    Some(SharedString::from(label))
}

/// 引擎展示名（源串是 ASCII，不进 i18n 表）。
pub(super) fn kind_label(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "SQLite",
        DatabaseKind::Postgres => "PostgreSQL",
        DatabaseKind::Mysql => "MySQL",
        DatabaseKind::ClickHouse => "ClickHouse",
        DatabaseKind::SqlServer => "SQL Server",
        DatabaseKind::Mongo => "MongoDB",
        DatabaseKind::Elasticsearch => "Elasticsearch",
    }
}

/// 引擎惯用的管理账号（新建连接时预填，省掉手打）：MySQL 系用 `root`，
/// PostgreSQL 系用 `postgres`，ClickHouse 用它的默认账号 `default`，
/// SQL Server 用内建管理员 `sa`；SQLite 无账号概念，返回空串。
///
/// Elasticsearch 预填内建超级用户 `elastic`：8.x 起安全模块**默认开启**，
/// 内建账号就是它。安全性没开的集群**不会**因此报错——`xpack.security.enabled:
/// false` 时 Security 模块整个不加载，凭证字段被直接忽略（驱动那边也只是
/// 把它拼成 `Authorization` 头，服务端不看）。真要让出空账号，表单里清掉即可。
pub(super) fn default_user(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "",
        DatabaseKind::Postgres => "postgres",
        DatabaseKind::Mysql => "root",
        DatabaseKind::ClickHouse => "default",
        DatabaseKind::SqlServer => "sa",
        DatabaseKind::Mongo => "admin",
        DatabaseKind::Elasticsearch => "elastic",
    }
}

/// 库名留空时落到哪里 —— **文案住在界面层**。
///
/// 内核只给「是不是必填」这类事实（`DatabaseKind::needs_database`），不吐成品中文：
/// 零依赖的 `duck-db` 不参与 i18n，字面量也不该出现在那边。而这件事**偏偏只有
/// 界面能说清**——「不填库名」在五个网络引擎上是五种语义（见各驱动模块文档），
/// 泛泛写一句「可留空」等于没说。
pub(super) fn database_hint(kind: DatabaseKind, cx: &App) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => tr(
            cx,
            "SQLite 这里填数据库文件路径（:memory: 也允许），不可留空。",
        ),
        DatabaseKind::Postgres => tr(cx, "可留空。留空时 PostgreSQL 连到用户名同名的库。"),
        DatabaseKind::Mysql => tr(
            cx,
            "可留空。留空即不选库：连上后在命名空间下拉里选一个，或把表名写成 db.table。",
        ),
        DatabaseKind::ClickHouse => tr(cx, "可留空。留空用惯例库 default。"),
        DatabaseKind::SqlServer => tr(cx, "可留空。留空落入登录名的默认库（通常 master）。"),
        DatabaseKind::Mongo => tr(cx, "可留空。留空即不选库，连上后在命名空间下拉里选一个。"),
        DatabaseKind::Elasticsearch => tr(
            cx,
            "可留空。这里填默认索引或别名（支持 logs-* 这类通配），留空即全部索引。",
        ),
    }
}

/// 卡片是否命中过滤词（名称 / id / 提示，大小写不敏感）。
pub(super) fn driver_matches_query(driver: &PickerDriver, query: &str) -> bool {
    query.is_empty()
        || driver.name.to_lowercase().contains(query)
        || driver.id.to_lowercase().contains(query)
        || driver.hint.to_lowercase().contains(query)
}

/// 引擎 → 品牌 logo（详情表单头部用）。
pub(super) fn kind_logo(kind: DatabaseKind) -> DatabaseLogo {
    match kind {
        DatabaseKind::Sqlite => DatabaseLogo::Sqlite,
        DatabaseKind::Postgres => DatabaseLogo::Postgres,
        DatabaseKind::Mysql => DatabaseLogo::Mysql,
        DatabaseKind::ClickHouse => DatabaseLogo::Clickhouse,
        DatabaseKind::SqlServer => DatabaseLogo::Sqlserver,
        DatabaseKind::Mongo => DatabaseLogo::Mongodb,
        DatabaseKind::Elasticsearch => DatabaseLogo::Elasticsearch,
    }
}
