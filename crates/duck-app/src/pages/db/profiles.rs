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
            let view_for_drivers = view.clone();
            let picker_for_content = picker.clone();
            let picker_for_next = picker.clone();
            let drag_for_content = drag.clone();

            // dbx 同形脚注：左「从文件导入… / 驱动管理…」，右「已选择：xxx · 下一步 / 取消」。
            // 单击卡片只选中；「下一步」（或双击卡片）才进表单 —— 状态读 picker 实体，
            // 选中变化时弹窗外壳随它重绘。
            let selected_label = picker
                .read(cx)
                .selected_name(cx)
                .map_or_else(String::new, |name| name.to_string());
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
                // 「驱动管理…」放在这一排：用户第一次意识到「这个引擎要装个驱动」就是
                // 在这个选择器上（卡片点进去、表单里提示缺驱动之前），也是 dbx 的入口位置。
                // 开之前先关掉选择器 —— 与 `choose` 同一条口径（同一窗口只留一层弹窗）。
                .child(
                    button(
                        "db-picker-drivers",
                        tr(cx, "驱动管理…"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let _ = view_for_drivers.update(cx, |this, cx| {
                            this.open_driver_manager(window, cx);
                        });
                    }),
                )
                .child(div().flex_1())
                .when(!selected_label.is_empty(), |footer| {
                    footer.child(
                        div()
                            .flex_none()
                            .min_w_0()
                            .truncate()
                            .child(meta_text(trf(cx, "已选择：{}", &[&selected_label]), cx)),
                    )
                })
                .child(
                    button(
                        "db-picker-next",
                        tr(cx, "下一步"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        picker_for_next.update(cx, |picker, cx| picker.proceed(window, cx));
                    }),
                )
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

            // 「选择显示数据库」（dbx 同位）：挂在「测试连接」旁边。设了过滤就显示
            // 摘要，而不是让用户记住「上次勾过」。SQLite 等无库层引擎不画。
            let visible_label = match form.read(cx).visible_databases.as_deref() {
                Some(list) if !list.is_empty() => {
                    trf(cx, "已选 {} 个库", &[&list.len().to_string()]).into()
                }
                _ => SharedString::from(tr(cx, "选择显示数据库")),
            };
            let show_visible_button = form.read(cx).kind.needs_host();
            let form_for_visible = form.clone();
            let visible_slot = show_visible_button.then(|| {
                icon_text_button(
                    "db-edit-visible-dbs",
                    visible_label,
                    Icon::new(DuckIcon::ListFilter).size(active_theme(cx).extended().icon.sm),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    form_for_visible.update(cx, |form, cx| {
                        form.open_visible_databases(window, cx);
                    });
                })
                .into_any_element()
            });

            let footer = DialogFooter::new()
                .when_some(visible_slot, |footer, button| footer.child(button))
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
    ///
    /// 三个值里**牌子最具体**：`driver` 只有 agent 档有值（[`PickerTarget::Agent`] 带过来
    /// 的驱动键），内建七档传 `None`；而 `brand` 是用户在墙上真正点中的那张卡
    /// （[`PickerTarget::Brand`]，`mysql` 协议下十几个牌子的**唯一**区分）。
    /// 三者一起往下传，表单靠它们回答「缺省端口 / 惯用账号 / 叫什么 / 有几种模式」。
    pub(super) fn start_new_connection(
        &mut self,
        kind: DatabaseKind,
        driver: Option<&'static str>,
        brand: Option<&'static Brand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = EditFormView::new(
            self.view.clone(),
            kind,
            driver,
            brand,
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
    ///
    /// 确认走**主题弹窗**（[`open_confirm`]）而不是 `duck_platform::confirm`：后者是 rfd
    /// 的同步系统模态（`NSAlert runModal`），从 `App::update` 里弹与
    /// [`Self::export_connections`] 是同一个重入雷（同属「界面里禁用 rfd」那条纪律）。
    pub(super) fn delete_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let id = id.to_owned();
        open_confirm(
            tr(cx, "删除连接"),
            tr(cx, "连接档案与其保存的口令都会被删除，确定继续吗？"),
            tr(cx, "删除"),
            Rc::new(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    // 顺序要紧：`force_close_tab` 要从档案里取这条连接的**断开钩子**，档案必须先还在
                    // （删档之后再拆会话，钩子就查不到了，等于静默跳过用户配好的收尾动作）。
                    // 走**硬关**而不走 `close_tab`：删档案的确认框里已经答过一次
                    // 「删除」，再弹一道「未保存的更改」会问不住 —— 万一用户点了
                    // 「取消」，档案删了一半就悬在半空。
                    this.force_close_tab(&id, window, cx);
                    this.connections.retain(|cfg| cfg.id != id);
                    this.persist_connections(cx);
                    let _ = delete_password(&id);
                    let _ = delete_ssh_password(&id);
                    let _ = delete_ssh_key_passphrase(&id);
                    let _ = delete_proxy_password(&id);
                    notify::success(tr(cx, "连接已删除。"), window, cx);
                });
            }),
            window,
            cx,
        );
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
        let _ = delete_profile_ssh_key_passphrase(profile_id);
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
    ///
    /// ⚠️ 这里**不能**用 `duck_platform::save_file`：它是 rfd 的同步模态（`NSSavePanel
    /// runModal`），而本方法跑在 gpui 的 `App::update` 里 ⇒ 模态循环把 AppKit 事件派发
    /// 重入回来，重入期间任何前台任务去 `AsyncApp::update_entity` 都会撞上
    /// 「RefCell already borrowed」并 abort（2026-10-16 21:25 那次闪退就是这条链，
    /// 见 `crash-2026-10-16.log` 的 `export_connections` 帧）。
    /// 用 gpui 自己的异步面板（`beginWithCompletionHandler`，不重入）。
    pub(super) fn export_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connections.is_empty() {
            notify::info(tr(cx, "当前没有可导出的连接。"), window, cx);
            return;
        }
        // JSON 在**开面板之前**就固化：面板异步，用户可能开着它去改连接 —— 那时再回读
        // 就不是「导出我现在看见的这份」了（见 `crate::file_dialogs` 模块头）。
        let json = match serde_json::to_vec_pretty(&self.connections) {
            Ok(json) => json,
            Err(error) => {
                notify::error(trf(cx, "导出失败：{}", &[&error.to_string()]), window, cx);
                return;
            }
        };
        crate::file_dialogs::save_file(cx, "duck-connections.json", move |path, this, cx| {
            this.spawn_db(
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
        });
    }

    /// 从 JSON 文件导入连接档案。
    ///
    /// 两种来源都收：本应用自己导出的（`Vec<ConnectionConfig>`）与第三方客户端的档案
    /// （DBeaver 的 `data-sources.json`、TablePlus 的 `.tableplusconnection`，见
    /// [`duck_db::import`]）。按 id 去重、已存在的跳过；口令一律要重新录入。
    pub(super) fn import_connections(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // 同 `export_connections`：系统「打开」面板是 rfd 的同步模态，从 `App::update`
        // 里弹必然重入事件循环 ⇒ 闪退。走 gpui 的异步面板。
        crate::file_dialogs::open_file(
            cx,
            tr(cx, "选择要导入的连接档案"),
            move |path, this, cx| {
                this.spawn_db(
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
                                    cfg.id = format!(
                                        "conn-{}",
                                        id_suffix(&format!("{}-{index}", cfg.name))
                                    );
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
                                notify::info(
                                    tr(cx, "没有新增连接（id 全部重复或无效）。"),
                                    window,
                                    cx,
                                );
                            }
                        }
                        Err(error) => {
                            let message = match error {
                                ProfileImportError::Read(error) => {
                                    trf(cx, "读取文件失败：{}", &[&error])
                                }
                                ProfileImportError::ThirdParty(error) => {
                                    import_error_message(error, cx).to_owned()
                                }
                            };
                            notify::error(trf(cx, "导入失败：{}", &[&message]), window, cx);
                        }
                    },
                );
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

/// 要「服务名」的两个 agent 引擎（[`EngineRef::server_name`] 的返回值）。
///
/// 同一个输入框、两套标签与占位符 —— 写档案时也各归各家的字段：
/// GBase 8s → `ConnectionConfig::gbase_server`、Informix → `informix_server`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServerName {
    /// GBase 8s：GBASESQLI 连接串里的服务名（dbx 叫 `gbase_server`）。
    Gbase,
    /// Informix：JDBC URL 里的 `INFORMIXSERVER=`（dbx 叫 `informix_server`）。
    Informix,
}

impl ServerName {
    /// 输入框的标签。
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Gbase => "服务名",
            // 对 Informix 用户，这个键名本身就是行话，不译。
            Self::Informix => "INFORMIXSERVER",
        }
    }

    /// 输入框的占位符（占位符直接教用户填什么，附上「留空走驱动推断」）。
    pub(super) fn placeholder(self) -> &'static str {
        match self {
            Self::Gbase => "如 gbase01；留空由驱动按主机推断",
            Self::Informix => "如 ol_informix1170；留空由驱动推断",
        }
    }
}

/// 「这是哪个引擎」——展示层要的三个事实：**档位** + （仅 agent 档）**驱动键** + **牌子**。
///
/// 为什么不能只看 [`DatabaseKind`]：`DatabaseKind::Agent` 是**一整个家族**（达梦 / 金仓 /
/// Oracle / DB2 / Hive / Snowflake…，55 条），它的展示名、缺省端口、品牌图标**都随驱动走**，
/// 而这些东西在 `duck-drivers` 的清单里，不在枚举里。
///
/// 为什么还需要**牌子**（[`Self::profile`]）：一个线协议下面站着十几个牌子 ——
/// `mysql` 协议就有 MySQL / MariaDB / TiDB / OceanBase / Doris / StarRocks … 12 个，
/// 档位与驱动键两边都长得一模一样，**只有名字、端口和账号不同**（3306 / 4000 / 2883 / 9030…）。
/// 没有这个字段，界面上就只能把 OceanBase 显示成 MySQL。牌子表见 [`brands`]。
///
/// 于是展示层统一收成这一个 `Copy` 值：从档案（[`Self::from_config`]）或从新建表单
/// （[`Self::new`] / [`Self::with_profile`]）造出来，再由它回答这些问题。
///
/// ⚠️ **不要**在别处重新按 `kind` 手写一遍 `match` —— 那正是当初七档时到处都有、
/// 加了 agent 档就必然漏掉几处的写法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EngineRef {
    /// 档位。
    pub kind: DatabaseKind,
    /// 驱动清单的键；只有 agent 档有值（内建档位恒 `None`）。
    pub driver: Option<&'static str>,
    /// 牌子键（`brands` 表里查得到才认，查不到按 `None`）。
    pub profile: Option<&'static str>,
}

impl EngineRef {
    /// 从档案取。`driver` / `profile` 都只认**查得到**的键：档案里写错了键时按 `None`
    /// 处理，展示层回落成通用文案（而不是把错的键原样显示给用户）。
    pub(super) fn from_config(cfg: &ConnectionConfig) -> Self {
        let driver = cfg.driver_entry().map(|entry| entry.key);
        Self {
            kind: cfg.kind,
            profile: cfg
                .profile
                .as_deref()
                .and_then(brands::brand)
                .map(|brand| brand.key),
            driver,
        }
    }

    /// 新建表单：档位 + 选中的驱动键。牌子靠 [`brands::infer`] 推 ——
    /// 推得出来就有（达梦、Oracle 这类一对一），推不出来就 `None`（MySQL 协议下十几个牌子）。
    pub(super) fn new(kind: DatabaseKind, driver: Option<&'static str>) -> Self {
        Self {
            kind,
            // 内建档位不该带驱动键（带了也只当没有——否则展示名会串到别的引擎上）。
            driver: (kind == DatabaseKind::Agent).then_some(driver).flatten(),
            profile: brands::infer(kind, driver).map(|brand| brand.key),
        }
    }

    /// 新建表单，**牌子明确**（用户在模式切换器上选了一个）。
    ///
    /// 牌子自己带档位与驱动键，所以三者一起给定 —— 这是模式族切换的唯一入口：
    /// OceanBase 从 MySQL 模式切到 Oracle 模式，换的是整条路（内建 mysql → agent）。
    pub(super) fn with_profile(brand: &'static Brand) -> Self {
        Self {
            kind: brand.kind(),
            driver: brand.driver(),
            profile: Some(brand.key),
        }
    }

    /// 品牌表里的一条（`profile` 查得到才有）。
    pub(super) fn brand(self) -> Option<&'static Brand> {
        self.profile.and_then(brands::brand)
    }

    /// 清单条目（只有 agent 档有）。
    fn entry(self) -> Option<&'static DriverEntry> {
        if self.kind != DatabaseKind::Agent {
            return None;
        }
        self.driver.and_then(duck_drivers::catalog::entry)
    }

    /// 引擎展示名。
    ///
    /// 牌子优先：`profile` 有值时用品牌表的名字（OceanBase 就是 OceanBase，
    /// 不会被显示成它底下的 mysql 协议）。两者都是**中文源串**，调用方该过 `tr`。
    ///
    /// ⚠️ 内建七档没有牌子时给 ASCII 品牌名（`kind_label`，**不进 i18n 表**）。
    pub(super) fn label(self) -> &'static str {
        if let Some(brand) = self.brand() {
            return brand.label;
        }
        match self.entry() {
            Some(entry) => entry.label,
            None => match self.kind {
                DatabaseKind::Agent => "Agent",
                kind => kind_label(kind),
            },
        }
    }

    /// 品牌 logo。三个来源按优先级：**牌子自己的图** → 驱动键的图 → 档位的图。
    ///
    /// 认不出的返回 `None`，由调用方回落成主题单色图标 —— 总好过随便安一个
    /// 别家的品牌标。
    pub(super) fn logo(self) -> Option<DatabaseLogo> {
        if let Some(brand) = self.brand() {
            return brand.logo;
        }
        if self.kind == DatabaseKind::Agent {
            return self.driver.and_then(agent_brand_logo);
        }
        kind_logo(self.kind)
    }

    /// 引擎惯用的管理账号（新建连接时预填）。
    ///
    /// 牌子优先（达梦 `SYSDBA`、Oracle `system`、DB2 `db2inst1`、IRIS `_SYSTEM`…）——
    /// 这些都是 dbx 品牌表里逐条核过的值，不是我们猜的。没有牌子的内建档回落
    /// [`default_user`]；agent 档推不出牌子时返回空串（**填错的代价比留空大**）。
    pub(super) fn default_user(self) -> &'static str {
        if let Some(brand) = self.brand() {
            return brand.user;
        }
        match self.kind {
            DatabaseKind::Agent => "",
            kind => default_user(kind),
        }
    }

    /// 品牌自己的**必需 URL 参数**（如星环 Argo 的 `auth=noSasl`）。
    ///
    /// 没有牌子/品牌表里为空时给空串 —— 多数引擎不需要它。
    pub(super) fn default_url_params(self) -> &'static str {
        self.brand().map_or("", |brand| brand.url_params)
    }

    /// 这条引擎要不要**服务名**输入框（`Some` 才画）。
    ///
    /// 两个 agent 引擎要：GBase 8s 的 GBASESQLI 服务名（dbx 的 `gbase_server`，
    /// 缺省值对端按主机推，但真实环境是管理员定的，猜错就 908 报错 —— GBase 8s
    /// 连不上的头号成因）与 Informix 的 `INFORMIXSERVER`（dbx 的 `informix_server`，
    /// 同理）。判据认**牌子**（`gbase8s` 卡）也认**驱动**（旧档案没牌子键只有驱动键）。
    pub(super) fn server_name(self) -> Option<ServerName> {
        if self.profile == Some("gbase8s") || self.driver == Some("gbase8s") {
            Some(ServerName::Gbase)
        } else if self.driver == Some("informix") {
            Some(ServerName::Informix)
        } else {
            None
        }
    }

    /// **模式族**：同族的全部牌子（含自己）。长度 > 1 才画模式切换器。
    ///
    /// 没有牌子时返回空切片（没有牌子就没有族可言）。
    pub(super) fn variants(self) -> &'static [&'static Brand] {
        match self.profile {
            Some(key) => brands::family_of(key),
            None => &[],
        }
    }

    /// **文件型引擎**：库里没有主机端口，建档时那一格填的是**文件路径**。
    ///
    /// SQLite 是原生的文件引擎；DuckDB 在档案里是 agent 档（工件从驱动商店装），
    /// 但对端接过来的是 `{path,...}` 而不是 host/port —— 所以表单要按文件型画。
    /// 判据只认驱动键，不看 `runtime_mode`：DuckDB 的工件同样是「可下载的驱动」。
    pub(super) fn is_file_backed(self) -> bool {
        self.kind == DatabaseKind::Sqlite || self.driver == Some("duckdb")
    }

    /// 库名留空时的提示（文案住在界面层，见 [`database_hint`]）。
    pub(super) fn database_hint(self, cx: &App) -> &'static str {
        // DuckDB 那一格是**文件路径**，不能沿用 agent 档「留空用它自己的落点」——
        // 留空在它那里是「没告诉我打开哪个文件」（`connect` 直接报错）。
        if self.driver == Some("duckdb") {
            return tr(
                cx,
                "DuckDB 数据库文件路径；填 `:memory:` 开一个内存库（关掉即消失）。",
            );
        }
        match self.kind {
            DatabaseKind::Agent => tr(
                cx,
                "可留空。这里的库名会随每条语句发给驱动进程；留空时用它自己的默认落点。",
            ),
            kind => database_hint(kind, cx),
        }
    }

    /// 缺省端口：**牌子优先，其次驱动**（达梦 5236、Oracle 1521、DB2 50000、
    /// TiDB 4000、OceanBase 2883…）。
    ///
    /// 与 [`ConnectionConfig::default_port`] 同源 —— 那边是档案版，这边是「还没建档案」
    /// 的新建表单版。
    pub(super) fn default_port(self) -> Option<u16> {
        if let Some(brand) = self.brand() {
            return brand.port;
        }
        match self.entry() {
            Some(entry) => entry.default_port,
            None if self.kind == DatabaseKind::Agent => None,
            None => self.kind.default_port(),
        }
    }
}

/// agent 驱动键 → 品牌 logo。
///
/// 图标整目录取自 dbx（Apache-2.0，`docs/public/icons/database/`），由
/// `scripts/gen_dblogos.py` 拷进 `duck-components` 并生成 [`DatabaseLogo`]；
/// 这张表只做「驱动键 → 图标」的对号入座。清单里**没有**对应图的
/// （星环 Argo、InterSystems Caché）返回 `None`，界面回落成主题单色图标
/// —— **不要**为了让每张卡都有图而就近安一个别家的品牌标（`nebula` 曾被
/// 误安成 Neo4j 的标，两家的图完全不是一回事）。
fn agent_brand_logo(key: &str) -> Option<DatabaseLogo> {
    Some(match key {
        // 关系型 / 国产。
        "dameng" => DatabaseLogo::Dameng,
        "kingbase" => DatabaseLogo::Kingbase,
        "highgo" => DatabaseLogo::Highgo,
        "uxdb" => DatabaseLogo::Uxdb,
        "vastbase" => DatabaseLogo::Vastbase,
        "goldendb" => DatabaseLogo::Goldendb,
        "gbase8a" | "gbase8s" => DatabaseLogo::Gbase,
        "oscar" => DatabaseLogo::Oscar,
        "sundb" => DatabaseLogo::Sundb,
        "yashandb" => DatabaseLogo::Yashandb,
        "xugu" => DatabaseLogo::Xugu,
        "databend" => DatabaseLogo::Databend,
        "oceanbase-oracle" => DatabaseLogo::Oceanbase,
        "oracle" | "oracle-oci" => DatabaseLogo::Oracle,
        "db2" => DatabaseLogo::Db2,
        "informix" => DatabaseLogo::Informix,
        "saphana" => DatabaseLogo::Saphana,
        "teradata" => DatabaseLogo::Teradata,
        "vertica" => DatabaseLogo::Vertica,
        "firebird" => DatabaseLogo::Firebird,
        "exasol" => DatabaseLogo::Exasol,
        "access" => DatabaseLogo::Access,
        "h2" | "h2-legacy" => DatabaseLogo::H2,
        "sqlserver-legacy" => DatabaseLogo::Sqlserver,
        "sqlite-worker" => DatabaseLogo::Sqlite,
        "duckdb" => DatabaseLogo::Duckdb,
        // 数仓 / 分析。
        "snowflake" => DatabaseLogo::Snowflake,
        "databricks" => DatabaseLogo::Databricks,
        "bigquery" => DatabaseLogo::Bigquery,
        "spanner" => DatabaseLogo::Spanner,
        "trino" => DatabaseLogo::Trino,
        "hive" => DatabaseLogo::Hive,
        "spark" => DatabaseLogo::Spark,
        "kylin" => DatabaseLogo::ApacheKylin,
        "ignite" | "ignite3" => DatabaseLogo::ApacheIgnite,
        "transwarp" => DatabaseLogo::TranswarpInceptor,
        // 图谱 / 文档 / 缓存 / 检索。
        "neo4j" => DatabaseLogo::Neo4j,
        "nebula" => DatabaseLogo::Nebula,
        "mongodb" => DatabaseLogo::Mongodb,
        "cassandra" => DatabaseLogo::Cassandra,
        "iris" => DatabaseLogo::Iris,
        // 时序 / 消息队列 / 注册配置。
        "tdengine" => DatabaseLogo::Tdengine,
        "iotdb" => DatabaseLogo::Iotdb,
        "etcd" | "etcd2" => DatabaseLogo::Etcd,
        "zookeeper" => DatabaseLogo::Zookeeper,
        "kafka" => DatabaseLogo::Kafka,
        "rocketmq" => DatabaseLogo::Rocketmq,
        "rabbitmq" => DatabaseLogo::Rabbitmq,
        // 清单里有、dbx 没给图的：宁缺毋滥。
        "argo" | "cache" => return None,
        _ => return None,
    })
}

/// 引擎 logo 元素：认得出品牌就画品牌图，认不出回落主题单色图标。
///
/// 为什么要留回落：agent 档几十条驱动里，dbx 没给图的就那么两三条
/// （星环 Argo / InterSystems Caché，见 [`agent_brand_logo`]）。那种情况下
/// 留白或者安一个别家的标都是错的 —— 给一个中性的数据库图标，
/// 用户至少知道「这里是一个数据源」。
///
/// 三处调用点（连接树 / 详情表单头 / 标签栏）共用它，免得各写一遍 `match`
/// 又各漏一处 —— 那正是加 agent 档时最容易出的错。
pub(super) fn engine_logo(engine: EngineRef, size: Pixels, cx: &App) -> AnyElement {
    match engine.logo() {
        Some(logo) => logo.element(size).into_any_element(),
        None => Icon::new(DuckIcon::Database)
            .size(size)
            .text_color(active_theme(cx).tokens().colors.muted_foreground)
            .into_any_element(),
    }
}

/// 引擎展示名（源串是 ASCII，不进 i18n 表）。
///
/// ⚠️ 只服务**内建七档**。agent 档的展示名在驱动清单里（中文源串），要走
/// [`EngineRef::label`] —— 这个函数对 `Agent` 给不出真名，只会给一个占位串。
fn kind_label(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "SQLite",
        DatabaseKind::Postgres => "PostgreSQL",
        DatabaseKind::Mysql => "MySQL",
        DatabaseKind::ClickHouse => "ClickHouse",
        DatabaseKind::SqlServer => "SQL Server",
        DatabaseKind::Mongo => "MongoDB",
        DatabaseKind::Elasticsearch => "Elasticsearch",
        DatabaseKind::Agent => "Agent",
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
///
/// ⚠️ 只服务**内建七档**；agent 档一律空串（理由见 [`EngineRef::default_user`]）。
fn default_user(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "",
        DatabaseKind::Postgres => "postgres",
        DatabaseKind::Mysql => "root",
        DatabaseKind::ClickHouse => "default",
        DatabaseKind::SqlServer => "sa",
        DatabaseKind::Mongo => "admin",
        DatabaseKind::Elasticsearch => "elastic",
        DatabaseKind::Agent => "",
    }
}

/// 库名留空时落到哪里 —— **文案住在界面层**。
///
/// 内核只给「是不是必填」这类事实（`DatabaseKind::needs_database`），不吐成品中文：
/// 零依赖的 `duck-db` 不参与 i18n，字面量也不该出现在那边。而这件事**偏偏只有
/// 界面能说清**——「不填库名」在五个网络引擎上是五种语义（见各驱动模块文档），
/// 泛泛写一句「可留空」等于没说。
///
/// ⚠️ 只服务**内建七档**；agent 档的提示见 [`EngineRef::database_hint`]。
fn database_hint(kind: DatabaseKind, cx: &App) -> &'static str {
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
        DatabaseKind::Agent => tr(
            cx,
            "可留空。这里的库名会随每条语句发给驱动进程；留空时用它自己的默认落点。",
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
///
/// ⚠️ 只服务**内建七档**。agent 档的品牌图标要按驱动键查，走 [`EngineRef::logo`]
/// （它给的是 `Option`，认不出的驱动回落主题单色图标）。
fn kind_logo(kind: DatabaseKind) -> Option<DatabaseLogo> {
    Some(match kind {
        DatabaseKind::Sqlite => DatabaseLogo::Sqlite,
        DatabaseKind::Postgres => DatabaseLogo::Postgres,
        DatabaseKind::Mysql => DatabaseLogo::Mysql,
        DatabaseKind::ClickHouse => DatabaseLogo::Clickhouse,
        DatabaseKind::SqlServer => DatabaseLogo::Sqlserver,
        DatabaseKind::Mongo => DatabaseLogo::Mongodb,
        DatabaseKind::Elasticsearch => DatabaseLogo::Elasticsearch,
        // agent 档的品牌图标**按驱动**查，这个函数答不出来 —— 返回 `None`
        // 让调用方回落主题图标，而不是就近安一个别家的品牌标。
        DatabaseKind::Agent => return None,
    })
}
