use super::*;

/// 危险语句闸门放行的**真正执行动作**（全文排队执行 / 单条执行）。
///
/// 用 `Rc<dyn Fn>` 而不是泛型 `FnOnce`：弹层构建闭包是 `Fn`（每一轮重排都重跑），
/// 里面还要再克隆一份交给按钮回调，所有权拿不出来。抽成别名也让签名与局部变量
/// 不必各写一遍那个复杂类型。
///
/// `pub(super)`：与 [`DatabasePage::guard_dangerous`] 同档可见（构建器也要过这道闸门），
/// 别名比方法本身更私的话 `private_interfaces` 会报出来。
pub(super) type GuardAction =
    Rc<dyn Fn(&mut DatabasePage, &mut Window, &mut Context<DatabasePage>)>;

/// 导出内容：文本档与原始字节档分开持有。
///
/// 为什么不做成「统一的 `Vec<u8>`」：文本档要的是 `write(&path, text)`（`String`
/// 直接 `AsRef<[u8]>`），字节档才是裸写 —— 统一成字节就得给文本再整块拷一份
///（大结果集几百 MB，这一份拷贝纯属多余）。两档共用一个「`FnOnce() -> Result`」
/// 的作业签名，就只能靠一个枚举把它们装进同一个闭包。
enum ExportPayload {
    Text(String),
    Bytes(Vec<u8>),
}

impl ExportPayload {
    /// 落盘。错误转成文案（`spawn_db` 的作业签名要求 `Result<_, String>`）。
    fn write(self, path: &std::path::Path) -> Result<(), String> {
        let written = match self {
            ExportPayload::Text(text) => std::fs::write(path, text),
            ExportPayload::Bytes(bytes) => std::fs::write(path, bytes),
        };
        written.map_err(|err| err.to_string())
    }
}

impl DatabasePage {
    /// 生效的查询超时：**单次覆盖 → 连接默认 → 全局 [`QUERY_TIMEOUT`]** 三级回落。
    pub(super) fn effective_timeout(&self) -> Duration {
        self.query_timeout_override
            .or_else(|| {
                self.active_config()
                    .and_then(ConnectionConfig::default_timeout)
            })
            .map_or(QUERY_TIMEOUT, Duration::from_secs)
    }

    /// 应用 / 清除单次超时覆盖（查询选项弹窗的落点）。
    pub(super) fn set_query_timeout(&mut self, secs: Option<u64>, cx: &mut Context<Self>) {
        self.query_timeout_override = secs;
        let text: SharedString = match secs {
            Some(secs) => trf(cx, "本次查询超时已设为 {} 秒", &[&secs.to_string()]).into(),
            None => tr(cx, "查询超时已恢复连接默认").into(),
        };
        self.status = text;
        cx.notify();
    }

    /// 「查询选项」弹窗：设本次会话的超时覆盖（留空 / 0 = 跟随连接默认）。
    ///
    /// 正文只有静态字段 + 一个输入框，所以**不需要**像连接详情那样挂视图实体：
    /// 输入框的正文由 `EditorState` 自己的视图重绘，弹窗不参与打字时的重排。
    pub(super) fn open_query_options(&self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let input = self.query_timeout_input.clone();
        let override_secs = self.query_timeout_override;
        let connection_default = self
            .active_config()
            .and_then(ConnectionConfig::default_timeout);
        // 打开时回填：有覆盖填覆盖值，否则留空（占位符说明将跟随谁）。
        let seed = override_secs
            .map(|secs| secs.to_string())
            .unwrap_or_default();
        input.update(cx, |state, cx| state.set_value(seed, window, cx));

        let fallback_hint: SharedString = match connection_default {
            Some(secs) => trf(cx, "该连接默认 {} 秒。", &[&secs.to_string()]).into(),
            None => trf(
                cx,
                "该连接未设默认，回落全局 {} 秒。",
                &[&QUERY_TIMEOUT.as_secs().to_string()],
            )
            .into(),
        };
        let active_hint: SharedString = if override_secs.is_some() {
            tr(cx, "当前使用单次覆盖，清除后回到连接默认。").into()
        } else {
            tr(cx, "当前跟随连接默认。").into()
        };

        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let input_for_apply = input.clone();
            let input_for_content = input.clone();
            let view_for_apply = view.clone();
            let view_for_clear = view.clone();
            // 外层闭包是 `Fn`（可能被反复调用），捕获的提示语只能逐帧克隆进正文闭包。
            let active_for_content = active_hint.clone();
            let fallback_for_content = fallback_hint.clone();

            let footer = DialogFooter::new()
                .child(
                    button(
                        "db-query-options-clear",
                        tr(cx, "清除覆盖"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(override_secs.is_none())
                    .on_click({
                        let view = view_for_clear;
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.set_query_timeout(None, cx));
                            window.close_dialog(cx);
                        }
                    }),
                )
                .child(div().flex_1())
                .child(
                    button(
                        "db-query-options-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    button(
                        "db-query-options-apply",
                        tr(cx, "应用"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let view = view_for_apply;
                        move |_, window, cx| {
                            let text = input_for_apply.read(cx).value().trim().to_owned();
                            // 空 / 0 = 跟随连接默认（与连接表单里的口径一致）。
                            let parsed = if text.is_empty() {
                                Some(None)
                            } else {
                                match text.parse::<u64>() {
                                    Ok(0) => Some(None),
                                    Ok(secs) if secs <= MAX_TIMEOUT_SECS => Some(Some(secs)),
                                    _ => None,
                                }
                            };
                            let Some(secs) = parsed else {
                                notify::warning(
                                    trf(
                                        cx,
                                        "查询超时必须是 1-{} 的整数秒",
                                        &[&MAX_TIMEOUT_SECS.to_string()],
                                    ),
                                    window,
                                    cx,
                                );
                                return;
                            };
                            let _ = view.update(cx, |this, cx| this.set_query_timeout(secs, cx));
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
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-query-options-title",
                                &drag_for_content,
                                tr(cx, "查询选项"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "超时（秒）"), &input_for_content))
                            .child(field_hint(active_for_content.clone(), cx))
                            .child(field_hint(fallback_for_content.clone(), cx)),
                    ))
                })
                // 不带分号：`open_dialog` 的闭包必须**返回** `Dialog`。
                .footer(footer)
        });
    }

    /// 把阻塞调用丢进后台执行器，完成后回主线程跑 `done`（拿到窗口与上下文）。
    ///
    /// 真正的活先过全局并发闸门（[`db_op_gate`]，上限 [`MAX_CONCURRENT_DB_OPS`]）：
    /// 超限时**排队**而不是拒绝，界面看到的仍是「一次点了就一定会跑」。
    /// 排队时间计入调用方那套超时预算——与「等连接互斥锁」同性质，都不另开表。
    pub(super) fn spawn_db<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            // 名额凭证活到闭包返回（`let _permit = ...` 不是 `let _ = ...`：
            // 后者会立刻析构，名额当场就还回去了）。
            let _permit = db_op_gate().acquire().await;
            work()
        });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let value = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.busy = false;
                        done(this, value, window, cx);
                        cx.notify();
                    });
                });
            });
        })
        .detach();
    }

    /// 跑一件**可取消 + 带超时**的数据库工作（读路径专用）。
    ///
    /// 与 [`Self::spawn_db`] 同形，差别只在：开工前抓一条**带外**取消句柄并挂超时守护
    /// （工具条据此把「执行」换成「停止」），收工前撤销守护。
    ///
    /// ⚠️ 只给**读路径**（SQL 执行、表页加载）用：写路径中途取消会留下半截数据，
    /// 不提供取消入口（设计稿 §4.4 的取舍）。
    pub(super) fn spawn_db_guarded<T, F, D>(
        &mut self,
        cx: &mut Context<Self>,
        session_id: String,
        cancel: CancelHandle,
        timeout: Duration,
        work: F,
        done: D,
    ) where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        let timer = CancelTimer::arm(&cancel, timeout);
        self.run_seq = self.run_seq.wrapping_add(1);
        let token = self.run_seq;
        self.running = Some(RunningQuery {
            token,
            session_id,
            cancel,
            timer,
        });
        self.stop_requested = false;
        self.run_timeout_secs = timeout.as_secs();
        self.spawn_db(cx, work, move |this, value, window, cx| {
            // 先收工（撤销守护 + 摘掉 running）再交结果：
            // ① 晚到的那一枪绝不能打到下一条语句上；
            // ② `done` 里可能立刻发起下一次运行（如保存后刷新表页），
            //    此时 `running` 应已清空，否则会被这里的清理误伤。
            // **只清理自己那次**：叠加运行（连续点表页）时，先回来的那次不许动后来者的守护。
            // 「这次是用户停的还是超时停的」记在 `stop_requested` / `run_timeout_secs`，
            // 它们保留到下一次 `arm` 覆盖，`done` 里读得到。
            if this
                .running
                .as_ref()
                .is_some_and(|running| running.token == token)
            {
                if let Some(running) = this.running.as_mut() {
                    running.timer.disarm();
                }
                this.running = None;
            }
            done(this, value, window, cx);
        });
    }

    /// 工具条「停止」：把取消送到正在跑的那条语句（不占连接锁，后台线程正握着也没关系）。
    pub(super) fn stop_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        tracing::info!(session = %running.session_id, "db query cancel requested");
        // 「执行 SQL 文件」比单条语句多一轴：那一枪只打断**手上这条**语句，谓词才决定
        // **后面的还跑不跑**。两个都置才是完整的「停止」（见 [`SqlFileJob::stop`]）。
        if let Some(job) = self.sql_file.as_ref() {
            job.stop.store(true, AtomicOrdering::Relaxed);
        }
        if let Err(error) = running.cancel.cancel() {
            self.status = trf(cx, "停止失败：{}", &[&error.to_string()]).into();
            cx.notify();
            return;
        }
        self.stop_requested = true;
        self.status = tr(cx, "正在停止…").into();
        notify::info(tr(cx, "已向数据库发送取消请求。"), window, cx);
        cx.notify();
    }

    /// 取消类错误的中文文案：点过「停止」就是用户停的，否则是超时守护开的枪。
    pub(super) fn cancelled_status(&self, cx: &App) -> SharedString {
        if self.stop_requested {
            tr(cx, "已停止。").into()
        } else {
            trf(
                cx,
                "查询超时（{} 秒）已被中止。",
                &[&self.run_timeout_secs.to_string()],
            )
            .into()
        }
    }

    /// 把查询历史写回 kv。
    pub(super) fn persist_history(&self, cx: &App) {
        match serde_json::to_string(&self.history) {
            Ok(json) => duck_store::kv_put(cx, KEY_HISTORY, json),
            Err(error) => tracing::warn!(%error, "serialize db query history failed"),
        }
    }

    /// 把已保存查询写回 kv（单键结构化数组，范式同连接档案）。
    pub(super) fn persist_saved_queries(&self, cx: &App) {
        match serde_json::to_string(&self.saved_queries) {
            Ok(json) => duck_store::kv_put(cx, KEY_SAVED_QUERIES, json),
            Err(error) => tracing::warn!(%error, "serialize db saved queries failed"),
        }
    }

    /// 把编辑器里的 SQL 存进「SQL 库」（对应 dbx 的「保存到 SQL 库」弹窗）。
    ///
    /// 与 dbx 对齐的三件事：
    /// * **文件名默认 `<表名>.sql`**（有选中表时）—— dbx 用的是标签标题，而标签标题
    ///   就是对象名；没有选中表时退回「首行前若干字符 + `.sql`」（与保存图表同口径：
    ///   给一个能改的占位，而不是「未命名」这种等于没给的东西）。
    /// * **文件夹默认留空 = 根「SQL 库」**（dbx `rootFolder` 同款）。
    /// * **填一个没出现过的名字就是新建文件夹** —— dbx 那颗「新建文件夹」按钮的等价
    ///   动作，这里做成同一个输入框 + 一排「已有」快选：少一层「弹层里再弹一层」。
    pub(super) fn open_save_query_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "编辑器里还没有 SQL。"), window, cx);
            return;
        }
        let default_name = default_sql_file_name(self.selected_table.as_ref(), &sql);
        let name_input = text_input_with(window, cx, tr(cx, "文件名"), &default_name);
        // 文件夹留空 = 根。**不预填「SQL 库」三个字**：那是根文件夹的显示名，
        // 预填成值之后用户自建一个同名文件夹就会和根撞在一起（编码里分不开）。
        let folder_input = text_input_with(window, cx, tr(cx, "SQL 库"), "");
        let known_folders = saved_query_folders(&self.saved_queries);
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let name_handle = name_input.clone();
        let confirm: ConfirmHandler = {
            let name_input = name_input.clone();
            let folder_input = folder_input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let raw = name_input.read(cx).value().trim().to_owned();
                if raw.is_empty() {
                    notify::warning(tr(cx, "请填写文件名。"), window, cx);
                    return false;
                }
                // 后缀在这一层补，而不是塞进默认值里：用户手打 `orders` 也该落成
                // `orders.sql`（dbx 的 `defaultSavedSqlName` 同口径）。
                let file = ensure_sql_extension(&raw);
                let folder = folder_input.read(cx).value().trim().to_owned();
                let folder = (!folder.is_empty()).then_some(folder);
                let sql = sql.clone();
                let _ = view.update(cx, |this, cx| {
                    let id = SavedQuery::next_id(&this.saved_queries);
                    let target = folder.clone();
                    this.saved_queries.push(SavedQuery {
                        id,
                        name: file.clone(),
                        folder,
                        sql,
                    });
                    this.persist_saved_queries(cx);
                    this.status = match target {
                        Some(folder) => trf(cx, "已保存到 SQL 库「{} › {}」。", &[&folder, &file]),
                        None => trf(cx, "已保存到 SQL 库「{}」。", &[&file]),
                    }
                    .into();
                    cx.notify();
                });
                true
            })
        };
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let confirm = Rc::clone(&confirm);
            let name_for_content = name_input.clone();
            let folder_for_content = folder_input.clone();
            let folders_for_content = known_folders.clone();
            dialog
                .w(px(SAVE_SQL_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content(move |content, _, cx| {
                    let theme = active_theme(cx);
                    let tokens = theme.tokens();
                    let mut body = v_flex()
                        .w_full()
                        .gap(form_gap(cx))
                        .child(modal_title_row(
                            "db-save-sql-title",
                            &drag_for_content,
                            tr(cx, "保存到 SQL 库"),
                            cx,
                        ))
                        .child(field_row(cx, tr(cx, "文件名"), &name_for_content));
                    let mut folder_block = v_flex()
                        .w_full()
                        .gap(tokens.spacing.xxs)
                        .child(field_row(cx, tr(cx, "文件夹"), &folder_for_content));
                    // 「已有」快选只在真有过文件夹时才画：一条挂着「已有」却空着的
                    // 标签行，比没有这行更让人怀疑控件坏了。
                    if !folders_for_content.is_empty() {
                        let mut chips = h_flex()
                            .w_full()
                            .flex_wrap()
                            .items_center()
                            .gap(tokens.spacing.xxs)
                            .child(meta_text(tr(cx, "已有"), cx));
                        for folder in &folders_for_content {
                            chips = chips.child(folder_chip(folder, &folder_for_content, cx));
                        }
                        folder_block = folder_block.child(chips);
                    }
                    folder_block = folder_block.child(field_hint(
                        tr(
                            cx,
                            "留空即存入根文件夹「SQL 库」；填一个没出现过的名字会自动新建文件夹。",
                        ),
                        cx,
                    ));
                    body = body.child(folder_block);
                    content.child(modal_drag_area(&drag_for_content, body))
                })
                // 不带分号：闭包必须返回 `Dialog`。
                .footer(dialog_footer(
                    "db-save-sql",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        name_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 给一条已保存查询改名（左栏右键「重命名」）。
    pub(super) fn open_rename_query_dialog(
        &mut self,
        id: &str,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.saved_queries.iter().find(|saved| saved.id == id) else {
            return;
        };
        let sql = saved.sql.clone();
        self.open_query_name_dialog(Some(id.to_owned()), current.to_owned(), sql, window, cx);
    }

    /// 「重命名」弹窗：只改名，不动正文，也不动它所在的文件夹。
    pub(super) fn open_query_name_dialog(
        &mut self,
        target: Option<String>,
        initial: String,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = text_input(window, cx, tr(cx, "文件名"));
        // 重命名时把原名填进去：清空重打一遍没有道理，改一个字符才是常态。
        if !initial.is_empty() {
            let seed = initial.clone();
            input.update(cx, |state, cx| state.set_value(seed, window, cx));
        }
        let drag = modal_drag(cx);
        let view = self.view.clone();
        let input_handle = input.clone();
        let confirm: ConfirmHandler = {
            let input = input.clone();
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let raw = input.read(cx).value().trim().to_owned();
                if raw.is_empty() {
                    notify::warning(tr(cx, "请填写文件名。"), window, cx);
                    return false;
                }
                let name = ensure_sql_extension(&raw);
                let sql = sql.clone();
                let target = target.clone();
                let _ = view.update(cx, |this, cx| {
                    match target {
                        // 改名：只动名字，SQL 正文保持原样（用户可能已经改过编辑器里的内容，
                        // 「重命名」的语义不该顺手覆盖正文），文件夹也不动。
                        Some(id) => {
                            if let Some(saved) =
                                this.saved_queries.iter_mut().find(|saved| saved.id == id)
                            {
                                saved.name = name;
                            }
                        }
                        None => {
                            let id = SavedQuery::next_id(&this.saved_queries);
                            this.saved_queries.push(SavedQuery {
                                id,
                                name,
                                folder: None,
                                sql,
                            });
                        }
                    }
                    this.persist_saved_queries(cx);
                    this.status = tr(cx, "已保存查询。").into();
                    cx.notify();
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
                                "db-saved-query-title",
                                &drag_for_content,
                                tr(cx, "重命名"),
                                cx,
                            ))
                            .child(field_row(cx, tr(cx, "文件名"), &input_for_content)),
                    ))
                })
                // 不带分号：闭包必须返回 `Dialog`。
                .footer(dialog_footer(
                    "db-saved-query",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
        input_handle.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 把一条已保存查询载入编辑器（左栏点击 / 右键「载入到编辑器」）。
    ///
    /// 只**填**编辑器，不执行：载入之后要不要跑是用户的决定（与「生成 DDL 骨架」
    /// 同口径——写动作都要过人工审批这一关）。
    pub(super) fn load_saved_query(
        &mut self,
        sql: String,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fill_editor(sql, window, cx);
        self.status = trf(cx, "已载入查询「{}」。", &[name]).into();
        cx.notify();
    }

    /// 把一段 SQL 填进编辑器并切回查询模式（**不执行**）。
    ///
    /// 「载入已保存查询」与「从历史放回编辑器」共用这一步：填进去但不跑是
    /// 同一条口径，状态栏那句话各自不同。
    pub(super) fn fill_editor(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.sql.update(cx, |state, cx| {
            state.set_value(sql, window, cx);
        });
        self.mode = Mode::Query;
        cx.notify();
    }

    /// 删除一条已保存查询（带二次确认：删了取不回来）。
    ///
    /// 确认走**主题弹窗**（[`open_confirm`]）而不是 `duck_platform::confirm` 的系统
    /// 模态：真正的删除搬进回调 —— 弹窗是异步的，函数在用户点下「删除」之前就返回了。
    pub(super) fn delete_saved_query(
        &mut self,
        id: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let id = id.to_owned();
        open_confirm(
            tr(cx, "删除已保存查询"),
            trf(cx, "「{}」将被删除，确定继续吗？", &[name]),
            tr(cx, "删除"),
            Rc::new(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.saved_queries.retain(|saved| saved.id != id);
                    this.persist_saved_queries(cx);
                    this.status = tr(cx, "已删除。").into();
                    notify::success(tr(cx, "已删除该查询。"), window, cx);
                    cx.notify();
                });
            }),
            window,
            cx,
        );
    }

    /// 把当前编辑区（编辑器文本 + 结果区）整体取出，交给调用方存进标签快照。
    ///
    /// 取值即搬空：调用方必须紧接着把结果装回去（[`Self::install_editor_state`]），
    /// 否则编辑区会停在空态。
    pub(super) fn take_editor_state(&mut self, cx: &App) -> SessionEditorState {
        let text = self.sql.read(cx).value().to_string();
        let view = SessionView {
            mode: self.mode.clone(),
            instance_data: std::mem::take(&mut self.instance_data),
            focused_metric: self.focused_metric.take(),
            selected_table: self.selected_table.take(),
            table_page: self.table_page,
            table_rows: self.table_rows.take(),
            table_sort: self.table_sort.take(),
            table_filters: std::mem::take(&mut self.table_filters),
            table_pk: std::mem::take(&mut self.table_pk),
            table_ddl: self.table_ddl.take(),
            table_indexes: std::mem::take(&mut self.table_indexes),
            table_row_count: self.table_row_count.take(),
            table_columns: std::mem::take(&mut self.table_columns),
            table_foreign_keys: std::mem::take(&mut self.table_foreign_keys),
            table_triggers: std::mem::take(&mut self.table_triggers),
            table_meta_for: self.table_meta_for.take(),
            structure: self.structure.take(),
            er: self.er.take(),
            compare: self.compare.take(),
            transfer: self.transfer.take(),
            query_result: self.query_result.take(),
            query_target: self.query_target.take(),
            query_pk: std::mem::take(&mut self.query_pk),
            query_sql: self.query_sql.take(),
            query_elapsed_ms: self.query_elapsed_ms.take(),
            query_loaded: self.query_loaded,
            query_has_more: self.query_has_more,
            query_page: self.query_page,
            query_sort: self.query_sort.take(),
            result_tabs: std::mem::take(&mut self.result_tabs),
            active_result_tab: self.active_result_tab,
            plan_view: self.plan_view.take(),
            selected_row: self.selected_row.take(),
            checked_rows: std::mem::take(&mut self.checked_rows),
            batch_column: self.batch_column.take(),
            pending_edits: std::mem::take(&mut self.pending_edits),
            pending_inserts: std::mem::take(&mut self.pending_inserts),
            pending_undo: std::mem::take(&mut self.pending_undo),
            pending_redo: std::mem::take(&mut self.pending_redo),
            row_editor: self.row_editor.take(),
            chart_view: self.chart_view.take(),
            result_view: self.result_view,
            result_area_tab: self.result_area_tab,
            exec_log: std::mem::take(&mut self.exec_log),
            hidden_columns: std::mem::take(&mut self.hidden_columns),
        };
        SessionEditorState { text, view }
    }

    /// 把一份编辑区快照装进页面（编辑器文本 + 结果区）。
    pub(super) fn install_editor_state(
        &mut self,
        state: SessionEditorState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SessionEditorState { text, mut view } = state;
        self.sql
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
        self.mode = view.mode;
        self.instance_data = std::mem::take(&mut view.instance_data);
        self.focused_metric = view.focused_metric.take();
        self.selected_table = view.selected_table.take();
        self.table_page = view.table_page;
        self.table_rows = view.table_rows.take();
        self.table_sort = view.table_sort.take();
        self.table_filters = std::mem::take(&mut view.table_filters);
        self.table_pk = std::mem::take(&mut view.table_pk);
        self.table_ddl = view.table_ddl.take();
        self.table_indexes = std::mem::take(&mut view.table_indexes);
        self.table_row_count = view.table_row_count.take();
        self.table_columns = std::mem::take(&mut view.table_columns);
        self.table_foreign_keys = std::mem::take(&mut view.table_foreign_keys);
        self.table_triggers = std::mem::take(&mut view.table_triggers);
        self.table_meta_for = view.table_meta_for.take();
        self.structure = view.structure.take();
        self.er = view.er.take();
        self.compare = view.compare.take();
        self.transfer = view.transfer.take();
        self.query_result = view.query_result.take();
        self.query_target = view.query_target.take();
        self.query_pk = std::mem::take(&mut view.query_pk);
        self.query_sql = view.query_sql.take();
        self.query_elapsed_ms = view.query_elapsed_ms.take();
        self.query_loaded = view.query_loaded;
        self.query_has_more = view.query_has_more;
        self.query_page = view.query_page;
        self.query_sort = view.query_sort.take();
        self.result_tabs = std::mem::take(&mut view.result_tabs);
        self.active_result_tab = view.active_result_tab;
        self.stmt_queue.clear();
        self.plan_view = view.plan_view.take();
        // 列宽**不进快照**：它现在是数据表组件的内部状态（页面级），换标签时由
        // `sync_data_table` 用 `set_model` 推新模型 —— 列名序列没变就沿用现有列宽
        //（同结构的两个标签切来切去宽度不重置），变了才重估并清掉横向位置。
        self.selected_row = view.selected_row.take();
        self.checked_rows = std::mem::take(&mut view.checked_rows);
        self.batch_column = view.batch_column.take();
        self.pending_edits = std::mem::take(&mut view.pending_edits);
        self.pending_inserts = std::mem::take(&mut view.pending_inserts);
        self.pending_undo = std::mem::take(&mut view.pending_undo);
        self.pending_redo = std::mem::take(&mut view.pending_redo);
        self.row_editor = view.row_editor.take();
        self.chart_view = view.chart_view.take();
        self.result_view = view.result_view;
        self.result_area_tab = view.result_area_tab;
        self.exec_log = std::mem::take(&mut view.exec_log);
        // 页码框**不再在这里清**：它是受控显示（页码是状态），换标签后由
        // 渲染入口的 `sync_page_jump` 按新标签的页码回写 —— 清成空再等下一帧
        // 填回来，中间那一眼会看见一个空框。
        self.hidden_columns = std::mem::take(&mut view.hidden_columns);
        // 结果区整体换了一份：把新结果推给数据表组件（表模型 / 行序 / 藏列一次推齐）。
        self.sync_data_table(cx);
    }

    /// 记一次相位报告：进会话内日志 + 失败时给用户一个提示。
    ///
    /// 「失败」有两种：`Block` 会连带把这次建连 / 断开中断（提示按错误档），
    /// `Warn` 只是没跑成（提示按警告档）—— 用户看到的措辞必须对得上后果。
    pub(super) fn note_hook_report(
        &mut self,
        id: &str,
        report: HookReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if report.outcomes.is_empty() {
            return;
        }
        // 先把「要不要提示、提示什么」算出来：`outcomes` 随即被搬进日志，之后读不到它。
        let blocked = report.blocker().cloned();
        let failures = report.failures();
        let at = now_secs();
        for outcome in report.outcomes {
            self.hook_log.push_back(HookRecord {
                connection_id: id.to_owned(),
                outcome,
                at,
            });
        }
        while self.hook_log.len() > HOOK_LOG_CAP {
            self.hook_log.pop_front();
        }
        if let Some(blocker) = blocked {
            notify::error(
                trf(
                    cx,
                    "钩子「{}」失败，已中断本次操作：{}",
                    &[&blocker.command, &blocker.detail()],
                ),
                window,
                cx,
            );
        } else if failures > 0 {
            notify::warning(
                trf(
                    cx,
                    "有 {} 条钩子失败（按配置继续）。",
                    &[&failures.to_string()],
                ),
                window,
                cx,
            );
        }
    }

    /// 执行 SQL 编辑器里的内容（工具条「运行」/ 快捷键 ⌘↵）。
    ///
    /// **选区优先**（dbx 同款）：没有独立的「选中执行」同伴按钮，主按钮自己判断 ——
    /// 编辑器里有选中文本就只跑那一段，没有就整篇跑。选段可以只是半条语句，不对应
    /// 任何语句槽，所以不给语句槽点状态标记（`stmt_index = None`）；危险闸门照样走，
    /// 「只是选了一段」不是跳过确认的理由。
    pub(super) fn run_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.editor_selection(cx) {
            let sql = selected.clone();
            self.guard_dangerous(
                &selected,
                Rc::new(move |this, window, cx| {
                    this.execute_sql(sql.clone(), None, window, cx);
                }),
                window,
                cx,
            );
            return;
        }
        let sql = self.sql.read(cx).value().trim().to_owned();
        if sql.is_empty() {
            notify::info(tr(cx, "请先输入 SQL。"), window, cx);
            return;
        }
        // **一条语句一个结果标签**（dbx 同款）：逐条排队执行，每条回来各占一个
        // 标签。切分与语句槽（gutter ▶）同源（`split_statement_spans`，空语句已滤），
        // 下标严格对齐 —— 排队序号就是语句槽序号。
        //
        // 危险确认在**批次级**做一次（`guard_dangerous` 里），排队路径直呼
        // `execute_sql`，不会逐条重复弹窗。清单先切好装进 `Rc`：确认框的
        // 回调是 `Fn`（可能反复重跑），不能把 `Vec` 移进去只消费一次。
        let statements: Rc<Vec<(usize, String)>> = Rc::new(
            split_statement_spans(&sql)
                .into_iter()
                .enumerate()
                .map(|(index, span)| (index, span.text))
                .collect(),
        );
        let run: GuardAction = Rc::new(move |this, window, cx| {
            // 全文执行是一批新开始：语句槽的状态标记整体作废（单条执行由
            // `execute_sql` 自己点亮 Running → Ok / Failed）。
            this.stmt_states.clear();
            this.result_tabs.clear();
            this.active_result_tab = 0;
            let mut queue: VecDeque<(usize, String)> = statements.iter().cloned().collect();
            if let Some((index, first)) = queue.pop_front() {
                this.stmt_queue = queue;
                this.execute_sql(first, Some(index), window, cx);
            } else {
                this.stmt_queue.clear();
            }
        });
        self.guard_dangerous(&sql, run, window, cx);
    }

    /// 危险语句闸门：不危险**直接放行**，危险则先弹主题确认框、确认后才跑 `proceed`。
    ///
    /// `proceed` 是「这条语句真正的执行动作」（全文排队执行 / 单条执行），闸门只管
    /// 「要不要问一句」，不碰执行细节 —— 两条路径共用同一句问话与同一个危险判据
    /// （`risk_of_statements`），不会一处严一处松。
    ///
    /// `pub(super)`：构建器（`db::builder`）执行自拼的语句时也走这里 —— 构建器拼出的
    /// `DELETE` / `UPDATE` 同样是「删库 / 全表清改」，不该因为「语句是界面拼的」少问一次。
    pub(super) fn guard_dangerous(
        &mut self,
        sql: &str,
        proceed: GuardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if risk_of_statements(&split_statements(sql)) != SqlRisk::Dangerous {
            proceed(self, window, cx);
            return;
        }
        let view = self.view.clone();
        open_confirm(
            tr(cx, "执行危险 SQL"),
            tr(cx, "这段 SQL 包含删库 / 全表清改类语句，确定继续吗？"),
            tr(cx, "继续执行"),
            Rc::new(move |window, cx| {
                let _ = view.update(cx, |this, cx| proceed(this, window, cx));
            }),
            window,
            cx,
        );
    }

    /// 执行**单条**语句（编辑器左栏的逐条 ▶）：与全文执行同一条危险确认 / 执行路径。
    ///
    /// 文本来自 [`duck_db::sqlintel::split_statement_spans`] 的切分——「图标钉在哪一行」
    /// 与「执行的是哪段文本」出自同一次切分，不会漂。`stmt_index` 同时是语句槽
    /// 状态标记的键（Running → Ok / Failed 就落在这格上）。
    pub(super) fn run_statement(
        &mut self,
        sql: String,
        stmt_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sql.trim().is_empty() {
            return;
        }
        // 单条执行与全文执行走**同一个**危险闸门（同一句问话、同一个判据）。
        let for_run = sql.clone();
        self.guard_dangerous(
            &sql,
            Rc::new(move |this, window, cx| {
                this.execute_sql(for_run.clone(), Some(stmt_index), window, cx);
            }),
            window,
            cx,
        );
    }

    /// 真正执行一段 SQL：**可取消 + 超时**，收尾时顺带更新事务状态与可编辑目标。
    ///
    /// 工具条「执行」与状态栏的「提交 / 回滚」共用这一条路径，所以「谁在跑、怎么停、
    /// 事务还在不在」只有一份真相。
    /// 复制所选行为 TSV（dbx 同款：**复制永远作用于选区**）。
    ///
    /// 勾选行优先（多选复制，逐行一条 TSV），其次单击选中的那一行；两者都没有 =
    /// 空选区，静默 —— 与系统 `⌘C` 在空选区上的行为一致。要拿整份结果，先全选
    /// （表头勾选框 / `⌘A`）再用右键「复制 ▸」。
    pub(super) fn copy_selected_row(&mut self, cx: &mut Context<Self>) {
        let rows: Vec<usize> = if !self.checked_rows.is_empty() {
            self.checked_rows.iter().copied().collect()
        } else if let Some(index) = self.selected_row {
            vec![index]
        } else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let text = rows
            .iter()
            .filter_map(|index| result.rows.get(*index))
            .map(|row| row_as_tsv(row))
            .collect::<Vec<_>>()
            .join("\n");
        let note: SharedString = if rows.len() == 1 {
            tr(cx, "已复制该行。").into()
        } else {
            trf(cx, "已复制所选 {} 行。", &[&rows.len().to_string()]).into()
        };
        self.put_on_clipboard(text, note, cx);
    }

    /// 把当前**选区**（dbx 同款：勾选行 > 单击选中行 > 整份结果）按 `format` 复制到剪贴板。
    ///
    /// 与「导出」共用 [`ResultFormat`] 与内核的渲染原语，只是终点不同（系统剪贴板 vs
    /// 落盘）——两条路的格式口径不会各走各的。`InsertSql` 要的表引用与引号字符由
    /// [`Self::insert_context`] 给；拿不到就静默返回（菜单项本来已禁用，走到这里只可能
    /// 是菜单弹开后引擎被换掉了）。
    ///
    /// 刻意**不**在渲染路径上预先拼好文本：见 [`Self::render_result`] 里那段注释。
    pub(super) fn copy_result_as(&mut self, format: ResultFormat, cx: &mut Context<Self>) {
        let Some(result) = self.current_result() else {
            return;
        };
        let insert = self.insert_context();
        let insert_args = insert
            .as_ref()
            .map(|(table, quote)| (table.as_str(), *quote));
        // dbx 同款：有选中就只复制选中 —— 勾选行优先（多选），其次单击选中的行；
        // 都没有才整份结果。要复制全部也可以先全选（表头勾选框 / `⌘A`）。
        let scope_rows: Option<Vec<usize>> = if !self.checked_rows.is_empty() {
            Some(self.checked_rows.iter().copied().collect())
        } else {
            self.selected_row.map(|index| vec![index])
        };
        let rendered = match scope_rows {
            None => format.render(result, insert_args),
            Some(ref rows) => {
                // 行子集：列原样共享，行按选中的下标取（下标越界的丢掉）。
                // 选中的行数通常是个位数，克隆成本可忽略；整份结果仍走零拷贝原路。
                let subset = ResultSet {
                    columns: result.columns.clone(),
                    rows: rows
                        .iter()
                        .filter_map(|index| result.rows.get(*index).cloned())
                        .collect(),
                    truncated: false,
                };
                format.render(&subset, insert_args)
            }
        };
        let Some(text) = rendered else {
            return;
        };
        let note: SharedString = match &scope_rows {
            Some(rows) => trf(cx, "已复制所选 {} 行。", &[&rows.len().to_string()]).into(),
            None => tr(cx, "已复制全部结果。").into(),
        };
        self.put_on_clipboard(text, note, cx);
    }

    /// 把**右键指向的那一行**复制成一段 SQL（dbx「SQL INSERT / UPDATE / WHERE」对位）。
    ///
    /// 与 [`Self::copy_result_as`] 的区别在作用域：那边跟着**选区**走，这边永远是
    /// 用户右键的那一行 —— 在一格上右键时，用户想拿走的是这条记录，不是整页结果。
    /// `Update` / `Where` 需要主键定位（[`Self::edit_source`]，主键列必须在结果里），
    /// `Insert` 只要有表引用就行（[`Self::insert_context`]）。文本全部在**只读借用
    /// 界内**算完，`put_on_clipboard` 的可变借用放在块外 —— 一处借用两用必撞 E0502。
    pub(super) fn copy_row_as_sql(&mut self, kind: RowSqlKind, row: usize, cx: &mut Context<Self>) {
        let text = {
            let Some(result) = self.current_result() else {
                return;
            };
            let Some((table, quote)) = self.insert_context() else {
                return;
            };
            match kind {
                RowSqlKind::Insert => {
                    // 单行子集走 INSERT 的整集渲染原路：列集原样，行只取这一条。
                    let one = ResultSet {
                        columns: result.columns.clone(),
                        rows: result.rows.get(row).cloned().into_iter().collect(),
                        truncated: false,
                    };
                    Some(one.to_insert_sql(&table, quote))
                }
                // 主键定位的两档：拿不到可编辑来源（无主键 / 主键列不在结果里）就
                // 在这里回绝 —— 菜单项虽已禁用，走到这里只可能是弹开后引擎被换掉。
                RowSqlKind::Update | RowSqlKind::Where => {
                    let Some(source) = self.edit_source() else {
                        return;
                    };
                    let pk: Vec<String> = source.pk.to_vec();
                    match kind {
                        RowSqlKind::Update => result.to_update_sql(&table, quote, &pk, row),
                        _ => result.to_where_sql(&table, quote, &pk, row),
                    }
                }
            }
        };
        let Some(text) = text else {
            self.status = tr(cx, "这一行定位不了（需要主键且主键列在结果里）。").into();
            cx.notify();
            return;
        };
        let note = kind.done_note(cx);
        self.put_on_clipboard(text, note, cx);
    }

    /// 把一格改动放进暂存区（⌘V 粘贴 / ⌘X 剪切共用）。
    ///
    /// 与就地编辑的回车提交走**同一条** `pending_edits` 路径：按 (主键, 列) 去重覆盖、
    /// 一步一记进撤销栈（`⌘Z` 可退）、状态栏回执。`value: None` = 写 `NULL`。
    pub(super) fn stage_cell_edit(
        &mut self,
        row: usize,
        col: usize,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(pk_values) = self.pk_values_for_row(row) else {
            self.status = tr(cx, "该表没有主键，无法定位行。").into();
            cx.notify();
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let Some(column) = result.columns.get(col) else {
            return;
        };
        let edit = CellEdit {
            pk: pk_values,
            column: column.name.clone(),
            value,
        };
        self.record_pending();
        self.pending_edits
            .retain(|old| !(old.pk == edit.pk && old.column == edit.column));
        self.pending_edits.push(edit);
        self.status = trf(
            cx,
            "已暂存 {} 处改动（共 {} 条待提交）",
            &[&1.to_string(), &self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 网格里的 `⌘V`（dbx 同款「粘贴进选区」）：剪贴板文本暂存为选中格的新值。
    ///
    /// 走暂存模型 —— 提交前 `⌘Z` 可退、可整批丢弃。就地编辑打开时不接：
    /// 输入框自己处理原生粘贴（更深的上下文先派发，到不了这里）。
    pub(super) fn paste_into_selected_cell(&mut self, cx: &mut Context<Self>) {
        if self.cell_editor.is_some() {
            return;
        }
        let Some((row, col)) = self.cell_detail else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if self.edit_source().is_none() {
            self.toast(
                notify::Tone::Warning,
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                cx,
            );
            return;
        }
        self.stage_cell_edit(row, col, Some(text), cx);
    }

    /// 网格里的 `⌘X`（dbx 同款剪切）：复制选中格的文本，原格暂存为 `NULL`。
    ///
    /// `NULL` 格没有可剪的东西（剪出来是空，写回去还是 NULL），直接忽略。
    pub(super) fn cut_selected_cell(&mut self, cx: &mut Context<Self>) {
        if self.cell_editor.is_some() {
            return;
        }
        let Some((row, col)) = self.cell_detail else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let Some(cell) = result.rows.get(row).and_then(|cells| cells.get(col)) else {
            return;
        };
        if cell.is_null() {
            return;
        }
        let text = cell.render().to_string();
        self.put_on_clipboard(text, tr(cx, "已剪切，原格已暂存为 NULL。"), cx);
        self.stage_cell_edit(row, col, None, cx);
    }

    /// 把编辑器里的 SQL 重新排版（缩进 + 换行；关键字大小写按设置档）。
    ///
    /// 排版本身在 [`duck_db::format_sql_with`] 里：解析得了的换成规范形式，解析不了的
    /// **原样留下**并在状态栏报出数量 —— 绝不静默改写用户手上跑得通的半截语句。
    /// 一字未改时不回写编辑器（避免无谓地移动光标与撤销栈）。
    pub(super) fn format_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let original = self.sql.read(cx).value().to_string();
        let settings = db_prefs(cx);
        let options = duck_db::SqlFormatOptions {
            keyword_case: settings.sql_keyword_case(),
            indent_tabs: settings.sql_indent_tabs,
            indent_width: settings.sql_indent_width.clamp(1, 8) as u8,
        };
        let formatted = duck_db::format_sql_with(&original, &options);
        let changed = formatted.text != original;
        if changed {
            self.sql.update(cx, |state, cx| {
                state.set_value(formatted.text.clone(), window, cx);
            });
        }
        let skipped = formatted.kept_verbatim.len();
        let message: SharedString = if skipped == 0 {
            if changed {
                tr(cx, "已格式化。").into()
            } else {
                tr(cx, "已格式化（没有变化）。").into()
            }
        } else {
            trf(
                cx,
                "已格式化；{} 段解析不了，保持原样。",
                &[&skipped.to_string()],
            )
            .into()
        };
        // 解析不了的段落是要用户**留意**的，所以那一种给警告档；其余是寻常回执。
        let tone = if skipped == 0 {
            notify::Tone::Success
        } else {
            notify::Tone::Warning
        };
        self.toast(tone, message, cx);
    }

    /// 工具条「小写关键字」开关：在大写 / 小写两档之间切换。
    ///
    /// 只改「格式化时关键字用哪种大小写」，**不动编辑器里已有的文本** —— 想立刻
    /// 看到效果再点一次「格式化」。写的是与设置页同一个键（不搞第二处真源）。
    pub(super) fn toggle_keyword_case(&mut self, cx: &mut Context<Self>) {
        let next = if db_prefs(cx).sql_keyword_case() == duck_db::SqlKeywordCase::Lower {
            duck_db::SqlKeywordCase::Upper
        } else {
            duck_db::SqlKeywordCase::Lower
        };
        super::super::module_settings::set_db_sql_keyword_case(cx, next);
        let message: SharedString = if next == duck_db::SqlKeywordCase::Lower {
            tr(cx, "格式化时关键字使用小写。").into()
        } else {
            tr(cx, "格式化时关键字使用大写。").into()
        };
        self.toast(notify::Tone::Info, message, cx);
        cx.notify();
    }

    /// 工具条「自动换行」开关：改 SQL 编辑器的软换行（并落到设置页同一个键）。
    pub(super) fn toggle_editor_word_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !db_prefs(cx).editor_word_wrap;
        super::super::module_settings::set_db_editor_word_wrap(cx, next);
        self.sql
            .update(cx, |state, cx| state.set_soft_wrap(next, window, cx));
        cx.notify();
    }

    /// 重算 SQL 语义诊断（编辑器文本变化 / 开关切换时调用，dbx 同款语义检查）。
    ///
    /// 没连上会话时**不做**表校验 —— 补全的同一口径：还没连上就不替某个引擎说话。
    /// 解析本身在 `duck_db::sqldiag`（纯函数、微秒级），同步跑即可。
    pub(super) fn refresh_sql_diagnostics(&mut self, text: &str, cx: &Context<Self>) {
        self.sql_diagnostics.clear();
        if text.trim().is_empty() || !db_prefs(cx).semantic_diagnostics {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let (catalog, kind) = (session.catalog.clone(), session.kind);
        self.sql_diagnostics = duck_db::sqldiag::diagnose_sql(kind, text, &catalog);
    }

    /// 工具条「SQL 语义诊断」开关：改的是与设置页同一个键（不搞第二处真源）。
    pub(super) fn toggle_semantic_diagnostics(&mut self, cx: &mut Context<Self>) {
        let next = !db_prefs(cx).semantic_diagnostics;
        super::super::module_settings::set_db_semantic_diagnostics(cx, next);
        if next {
            let text = self.sql.read(cx).value().to_string();
            self.refresh_sql_diagnostics(&text, cx);
        } else {
            self.sql_diagnostics.clear();
        }
        cx.notify();
    }

    /// 编辑器当前选中的文本（「运行」/「执行计划」判断选区优先的原料）；空选区返回 `None`。
    pub(super) fn editor_selection(&self, cx: &App) -> Option<String> {
        let selected = self.sql.read(cx).selected_text().to_string();
        if selected.trim().is_empty() {
            None
        } else {
            Some(selected)
        }
    }

    /// 切换选区覆盖行的 `-- ` 注释（设计稿 `commentBtn`）。
    ///
    /// 动作落在**整行**上（dbx 同款）：哪怕只选中了一行的一半，切的也是那一行。
    pub(super) fn toggle_comment_editor_sql(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, range) = {
            let state = self.sql.read(cx);
            (state.value().to_string(), state.selected_range())
        };
        let toggled = duck_db::sqlintel::toggle_line_comments(&text, range);
        let lines = toggled.lines;
        let removed = toggled.removed;
        self.sql.update(cx, |state, cx| {
            state.set_value(toggled.text, window, cx);
        });
        let message: SharedString = if removed {
            trf(cx, "已取消 {} 行的注释。", &[&lines.to_string()]).into()
        } else {
            trf(cx, "已注释 {} 行。", &[&lines.to_string()]).into()
        };
        self.toast(notify::Tone::Success, message, cx);
    }

    /// 把编辑器里的 SQL 压成单行（设计稿 `compactBtn`）。
    ///
    /// 只动注释与空白（见 [`duck_db::sqlintel::minify_sql`]），字符串字面量
    /// 原样保留——压掉字面量里的空白就是改数据。
    pub(super) fn minify_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let original = self.sql.read(cx).value().to_string();
        let minified = duck_db::sqlintel::minify_sql(&original);
        let changed = minified != original;
        if changed {
            self.sql.update(cx, |state, cx| {
                state.set_value(minified, window, cx);
            });
        }
        let message = if changed {
            tr(cx, "已压缩。")
        } else {
            tr(cx, "已压缩（没有变化）。")
        };
        self.toast(notify::Tone::Success, message, cx);
    }

    /// 对编辑器里的单条语句求执行计划（`EXPLAIN`）。
    ///
    /// 走的是与普通查询**同一条**执行路径（同一把取消句柄、超时、结果网格与截断提示），
    /// 但不动编辑器内容：按一次看一眼计划，光标与语句都还在原处。
    ///
    /// **选区优先**（与「运行」同一口径）：编辑器里写了好几条语句时，选中一条再按
    /// 「执行计划」只看这一条 —— 否则永远只能清空编辑器只留一条语句才看得了计划。
    /// 选区可以只是半条 / 不成句的片段，这时 [`explain_sql_for`] 会按「不是单条语句」
    /// 或「没有执行计划」如实报出，不会把半截语句硬塞给数据库。
    ///
    /// 包装语句**按引擎来**（[`explain_sql_for`]）：SQLite 用 `EXPLAIN QUERY PLAN`、
    /// PG 用 `EXPLAIN (FORMAT JSON)`、MySQL 用 `EXPLAIN FORMAT=JSON` —— 这三条的输出
    /// 能结构化（见 [`duck_db::plan`]），比一坨文本有用得多。
    pub(super) fn explain_editor_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 口径（dbx 同款「执行计划作用于当前查询」）：
        // ① 有选区 → 只分析选中的；
        // ② 编辑器本身就是单条语句 → 分析它；
        // ③ 编辑器是多条语句（跑完整批之后编辑器通常原样留着）→ 回落到**当前结果
        //    标签**的那条语句 —— 否则用户跑完查询再点「执行计划」，只会得到一句
        //    「一次只看一条语句」（横幅层级关掉时连这句都没有，表现为「点了没反应」）。
        let selection = self.editor_selection(cx);
        let mut sql = selection
            .clone()
            .unwrap_or_else(|| self.sql.read(cx).value().to_string());
        if selection.is_none() && split_statements(&sql).len() > 1 {
            if let Some(result_sql) = self.query_sql.clone() {
                if split_statements(&result_sql).len() == 1 {
                    sql = result_sql;
                }
            }
        }
        let Some(kind) = self.active_config().map(|config| config.kind) else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        match explain_sql_for(kind, &sql) {
            Ok(explain) => {
                // 置一次性标记：结果装回来时顺手解析成计划树。
                self.plan_next = true;
                self.execute_sql(explain, None, window, cx);
            }
            Err(ExplainSkip::NotASingleStatement) => {
                notify::info(tr(cx, "执行计划一次只看一条语句。"), window, cx);
            }
            Err(ExplainSkip::NoPlanForThisStatement) => {
                notify::info(tr(cx, "这条语句没有执行计划。"), window, cx);
            }
        }
    }

    pub(super) fn execute_sql(
        &mut self,
        sql: String,
        stmt_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 早退的两条分支都要把一次性标记清掉：没跑起来就没有「这一次是 EXPLAIN」，
        // 留着它会让**下一次**普通查询在结果区无端端解析一份计划树，审计里也会被
        // 记成「查看执行计划」—— 标记描述的是「这一次」，不是「最近一次」。
        let Some(session) = self.active_session() else {
            self.plan_next = false;
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        if self.running.is_some() {
            self.plan_next = false;
            notify::info(
                tr(cx, "上一条语句还在跑，先等它结束或点「停止」。"),
                window,
                cx,
            );
            return;
        }
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 取消句柄必须在开工前抓：跑起来之后连接锁在后台线程手里。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let statements = split_statements(&sql);
        let timeout = self.effective_timeout();
        // 只有「单条、单表、无分组」的 SELECT 才有资格被当作可编辑结果集；
        // 具体能不能改还要等结果回来核主键覆盖（`covers_columns`）。
        let target = if statements.len() == 1 {
            edit_target(&sql).map(|EditTarget { table, schema }| TableRef {
                schema,
                name: table,
            })
        } else {
            None
        };
        let history_sql = sql.clone();
        // 审计用的几件事实在**发起时**算好：它们只依赖 SQL 文本，放进后台闭包反而要多搬
        // 一份数据出来。类别按**语句风险**分（不是按「结果集有没有行」）——写死了
        // 读路径与写路径的区别，用户拿它筛「这张库被写过什么」才有意义。
        // 执行计划走的是**同一条**执行路径（[`Self::explain_editor_sql`] 只把包装语句
        // 传进来），要在这里把它认出来，否则流水里那条会是一条看不出所以然的 `EXPLAIN …`。
        // 判据用 `plan_next`：它由 `explain_editor_sql` 置上、等结果装回来才消费，此刻还是
        // true。计划只读，所以类别恒为 Query（不参与风险分类）。
        let is_explain = self.plan_next;
        let audit_kind = if is_explain || risk_of_statements(&statements) == SqlRisk::ReadOnly {
            crate::audit::AuditKind::Query
        } else {
            crate::audit::AuditKind::Write
        };
        let audit_action = if is_explain {
            crate::audit::action::EXPLAIN
        } else if audit_kind == crate::audit::AuditKind::Query {
            crate::audit::action::EXECUTE
        } else {
            crate::audit::action::COMMIT
        };
        // 指纹认「语句形状」（字面量被抹掉），摘要给人认「是哪条语句」——
        // 两者都不能少：前者用来归类，后者用来一眼看明白。
        let audit_fingerprint = fingerprint(&sql);
        let audit_summary = audit_sql_summary(&sql);
        // 结果上限在**发起这次查询时**读一次：这条语句跑多久都不影响它。
        let row_cap = query_row_cap(cx);
        self.status = tr(cx, "正在执行…").into();
        // 语句槽状态标记：逐条执行时先点亮「运行中」，结果回来落成功 / 失败
        //（全文执行由 `run_query` 先清空整张表）。
        if let Some(index) = stmt_index {
            self.stmt_states.insert(index, StmtRunState::Running);
        }
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id.clone(),
            cancel,
            timeout,
            move || {
                let started = Instant::now();
                let mut guard = conn.lock().expect("连接锁不应中毒");
                // 失败也带上耗时：一次「立刻返回的错误」和一次「跑满 30 秒才超时」是完全
                // 不同的两件事，而错误文本里都没有这个信息 —— 审计里不记就只能猜。
                // （类型从 `Result<_, DbError>` 变成 `Result<_, (DbError, u128)>`。）
                let outcome = match guard.execute(&sql, row_cap) {
                    Ok(outcome) => outcome,
                    Err(error) => return Err((error, started.elapsed().as_millis())),
                };
                // 可编辑目标：表有主键，且主键列**真的在结果里**（`SELECT name FROM t`
                // 单表合法，但改完没法定回哪一行）。
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
                Ok((outcome, started.elapsed().as_millis(), edit_meta))
            },
            move |this, result, window, cx| match result {
                Ok((outcome, elapsed_ms, edit_meta)) => {
                    // 事务状态：`BEGIN` 挂上、`COMMIT` / `ROLLBACK` 摘掉（状态栏据此显示）。
                    if let Some(session) = this.sessions.get_mut(&session_id) {
                        for stmt in &statements {
                            match tx_effect(stmt) {
                                Some(TxEffect::Begin) => session.in_tx = true,
                                Some(TxEffect::End) => session.in_tx = false,
                                None => {}
                            }
                        }
                    }
                    let status = match &outcome.result {
                        Some(result) => {
                            let mut text = trf(
                                cx,
                                "{} 行 · {} 条语句 · {} ms",
                                &[
                                    &result.row_count().to_string(),
                                    &outcome.statements_run.to_string(),
                                    &elapsed_ms.to_string(),
                                ],
                            );
                            if result.truncated {
                                text.push_str(" · ");
                                text.push_str(tr(cx, "已达上限，结果被截断"));
                            }
                            text
                        }
                        None => trf(
                            cx,
                            "影响 {} 行 · {} 条语句 · {} ms",
                            &[
                                &outcome.affected.to_string(),
                                &outcome.statements_run.to_string(),
                                &elapsed_ms.to_string(),
                            ],
                        ),
                    };
                    push_history(&mut this.history, &history_sql, history_limit(cx));
                    this.persist_history(cx);
                    // footer 要的「这一次跑了多久」：结果装进去之前记下（与 status 同源）。
                    this.query_elapsed_ms = Some(elapsed_ms);
                    this.install_query_result(outcome, edit_meta, history_sql, cx);
                    // 语句槽：这一条跑完了，标记落成功。
                    if let Some(index) = stmt_index {
                        this.stmt_states.insert(index, StmtRunState::Ok);
                    }
                    // 计划没解析出结构时如实说一句：输出还在（网格里就是原样文本），
                    // 但界面没能把它整理成树。
                    let mut status = status;
                    if this.plan_flat {
                        status.push_str(" · ");
                        status.push_str(tr(cx, "计划未能结构化，按原样文本展示"));
                    }
                    // 「消息」tab 的执行流水（dbx messages 同款）：成功也记一条，
                    // 读数与状态栏同源（行数 / 条数 / 耗时）。
                    this.push_exec_log(true, &statements, &status, elapsed_ms, cx);
                    this.status = status.into();
                    crate::audit::record_entry(
                        cx,
                        AuditEntry::new(
                            audit_kind.key(),
                            audit_action,
                            crate::audit::AuditOutcome::Ok,
                            &session_id,
                            audit_summary.clone(),
                        )
                        .with_fingerprint(&audit_fingerprint)
                        .with_duration(elapsed_ms as i64),
                    );
                    // 这条语句的结果已落进自己的标签；批次里还有就接着跑下一条。
                    this.run_next_queued_statement(window, cx);
                }
                Err((error, elapsed_ms)) => {
                    // 语句槽：这一条失败了（取消同属「没跑成」，但不盖掉 Running 之前的
                    // 状态没有意义——统一落 Failed，详情在错误条与状态栏）。
                    if let Some(index) = stmt_index {
                        this.stmt_states.insert(index, StmtRunState::Failed);
                    }
                    // 「被取消」分两种来源：用户点了停止，或超时守护到点。
                    if matches!(error, DbError::Cancelled(_)) {
                        let mut text = this.cancelled_status(cx).to_string();
                        if statements.len() > 1 {
                            // 多语句批次是逐条跑的：被中断的那条之后没跑，但之前的已经生效了。
                            text.push_str(tr(cx, "更早的语句可能已生效。"));
                        }
                        let text = SharedString::from(text);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        // 取消也记：`Cancelled` 在审计里不是「失败」，但「哪条语句被中断过」
                        // 是排查「为什么半截数据进去了」的第一手线索。
                        let detail = trf(cx, "{}（已取消）", &[&audit_summary]);
                        crate::audit::record_entry(
                            cx,
                            AuditEntry::new(
                                audit_kind.key(),
                                audit_action,
                                crate::audit::AuditOutcome::Cancelled,
                                &session_id,
                                detail,
                            )
                            .with_fingerprint(&audit_fingerprint)
                            .with_duration(elapsed_ms as i64),
                        );
                    } else {
                        // 留下真实错误原文 + 原 SQL：给「AI 修错 / 解释」注入上下文
                        // （通知会消失，这份上下文不会）。
                        let reason = error.to_string();
                        this.last_error = Some(QueryError {
                            sql: history_sql,
                            message: reason.clone(),
                        });
                        this.status = trf(cx, "执行失败：{}", &[&reason]).into();
                        let detail = trf(cx, "{}失败：{}", &[&audit_summary, &reason]);
                        let trace = crate::audit::record_entry(
                            cx,
                            AuditEntry::new(
                                audit_kind.key(),
                                audit_action,
                                crate::audit::AuditOutcome::Failed,
                                &session_id,
                                detail,
                            )
                            .with_fingerprint(&audit_fingerprint)
                            .with_duration(elapsed_ms as i64),
                        );
                        notify::error(
                            failure_message(cx, tr(cx, "SQL 执行"), trace.as_deref(), &reason),
                            window,
                            cx,
                        );
                    }
                    // 批次里**任意一条失败即停**：后面的语句没跑（语句槽保持 ▶），
                    // 队列就地作废 —— 用户改完再点执行就是新的一批。
                    this.stmt_queue.clear();
                    // 失败不落地：替换旗标别留给下一次普通运行。
                    this.result_replace_active = false;
                    // 失败 / 取消同样进「消息」tab 的流水：错误文本是排查的第一手线索。
                    let log_detail = if matches!(error, DbError::Cancelled(_)) {
                        tr(cx, "已取消").to_string()
                    } else {
                        error.to_string()
                    };
                    this.push_exec_log(false, &statements, &log_detail, elapsed_ms, cx);
                }
            },
        );
    }

    /// 查询结果客户端分页的**生效**每页行数：仅本次覆盖优先，否则跟随默认设置。
    pub(super) fn query_page_size(&self, cx: &App) -> usize {
        self.query_page_size_override
            .map_or_else(|| browse_page_size(cx), |size| size as usize)
    }

    /// footer 上的「N 行/页」下拉（dbx grid-foot 同款）：预设**仅本次**生效，
    /// 「自定义行数」内联在弹层里 —— 输入框 + ✓（快捷「仅本次」），底部
    /// 「仅本次 / 设为默认」两枚按钮分别走两条路。
    ///
    /// 预设不动落盘的默认值——改默认是「设为默认」那条路的语义，藏在预设里
    /// 会让一次顺手点击悄悄改掉表浏览的页大小（两个模式共用同一个键）。
    pub(super) fn query_page_size_dropdown(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let view = self.view.clone();
        let current = self.query_page_size(cx) as u32;
        let default = browse_page_size(cx) as u32;
        let presets = [50u32, 100, 500, 1000];
        toolbar_text_dropdown_button(
            "db-query-page-size",
            SharedString::from(trf(cx, "{} 行/页", &[&current.to_string()])),
            Icon::new(DuckIcon::ListOrdered).size(extended.icon.sm),
            Anchor::TopRight,
            move |menu, window, cx| {
                // 弹层要装下内联的「自定义行数」表单，比普通菜单宽一档。
                let menu = menu.min_w(px(260.));
                let menu_entity = cx.entity();
                // 输入框在**弹层构建时**建一次（builder 每次打开才跑，
                // 弹层存活着就复用同一份 —— 输入中的值不会丢）。
                let input = text_input(window, cx, tr(cx, "行数"));
                input.update(cx, |state, cx| {
                    state.set_value(current.to_string(), window, cx)
                });
                let on_apply: PageSizeApply = {
                    let view = view.clone();
                    Rc::new(move |size, persist, _window, cx| {
                        let _ = view
                            .update(cx, |this, cx| this.apply_query_page_size(size, persist, cx));
                    })
                };
                let close: Rc<dyn Fn(&mut App)> = {
                    let menu_entity = menu_entity.clone();
                    Rc::new(move |cx| {
                        menu_entity.update(cx, |_, cx| cx.emit(DismissEvent));
                    })
                };
                let session_label = tr(cx, "仅本次");
                // 「当前 / 默认」两栏在这里才**分得开**：查询侧有「仅本次」覆盖，
                // 当前值可能不等于落盘的默认值。表浏览两栏恒等，那一档不出这行。
                let readout = SharedString::from(trf(
                    cx,
                    "当前：{} · 默认：{}",
                    &[&current.to_string(), &default.to_string()],
                ));
                let menu = presets.into_iter().fold(menu, |menu, size| {
                    let view = view.clone();
                    let label = SharedString::from(trf(cx, "{} 行/页", &[&size.to_string()]));
                    menu.item(PopupMenuItem::new(label).checked(size == current).on_click(
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.apply_query_page_size(size, false, cx);
                            });
                        },
                    ))
                });
                menu.separator().item(PopupMenuItem::element({
                    // `ElementItem` 的 builder 是 `Fn`（会反复调用），读数行
                    // 不能直接 move 进去 —— 每次渲染现 clone 一份。
                    let readout = readout.clone();
                    move |_, cx| {
                        page_size_custom_section(
                            &input,
                            Some(readout.clone()),
                            Some(SharedString::from(session_label)),
                            on_apply.clone(),
                            close.clone(),
                            cx,
                        )
                    }
                }))
            },
            cx,
        )
    }

    /// 应用一个查询分页页大小；`persist = true` 落盘为默认（并清掉仅本次覆盖）。
    ///
    /// 页大小一变页码语义整体位移（「第 2 页」对应的是完全另一批行），回第 1 页
    /// 并说清楚 —— 与表浏览的 [`Self::set_browse_page_size`] 同一口径。
    pub(super) fn apply_query_page_size(
        &mut self,
        size: u32,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        if persist {
            super::super::module_settings::set_db_page_size(cx, size);
            self.query_page_size_override = None;
            self.status = trf(cx, "已设为默认每页 {} 行。", &[&size.to_string()]).into();
        } else {
            self.query_page_size_override = Some(size);
            self.status = trf(cx, "本次按每页 {} 行展示。", &[&size.to_string()]).into();
        }
        self.query_page = 0;
        cx.notify();
    }

    /// 把一段生成的 SQL 放进编辑器（**只放不跑**）。
    ///
    /// 结构管理（建表 / 改表 / 删表）与 DDL 改写都走这条路：界面拼出语法正确的
    /// 起点，落不落库由用户在编辑器里按「执行」决定 —— 与项目里其它写路径同一条
    /// 「人工审批」纪律。
    ///
    /// **不切模式**：用户可能正在浏览某张表，把语句塞进编辑器不该顺手把结果区挪走。
    pub(super) fn stage_sql(
        &mut self,
        sql: String,
        note: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sql
            .update(cx, |state, cx| state.set_value(sql, window, cx));
        self.status = note;
        cx.notify();
    }

    /// 装配查询结果展示态（SQL 执行与 AI 自动执行共用）。
    ///
    /// `edit_meta` 为 `Some` 时这份结果集可以在网格上直接改（单表 + 主键齐全）；
    /// `sql` 是产出它的语句，编辑后重跑用。
    pub(super) fn install_query_result(
        &mut self,
        outcome: ExecOutcome,
        edit_meta: Option<(TableRef, Vec<String>)>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::Query;
        // 新结果回来，客户端分页回第 1 页（旧行数对不上新页码）。
        self.query_page = 0;
        // 分页状态先算：结果集马上要被搬进 `QueryView`，之后在 `outcome` 上就读不到了。
        //
        // 「可以加载更多」的三个前提缺一不可：
        // ① 语句是**单条且可证明只读**——只有它才能被安全地用 `LIMIT/OFFSET` 重跑；
        // ② 当前引擎支持分页续拉（`DatabaseKind::supports_paged_read`；SQL Server /
        //    MongoDB / Elasticsearch 为假）；
        // ③ 首屏确实被行上限截断了（没截断 = 已经拿全了）。
        let single_read_only = {
            let statements = split_statements(&sql);
            statements.len() == 1 && risk_of_statements(&statements) == SqlRisk::ReadOnly
        };
        // 「能不能续拉」查内核的**单一判据**（`DatabaseKind::supports_paged_read`）——
        // SQL Server / MongoDB / Elasticsearch 都被判为不支持，界面因此不出「加载更多」，
        // 与 `Connection::execute_paged` 的拒绝口径同源。
        let paging_capable = self
            .active_config()
            .is_some_and(|cfg| cfg.kind.supports_paged_read());
        let truncated = outcome
            .result
            .as_ref()
            .is_some_and(|result| result.truncated);
        self.query_loaded = outcome
            .result
            .as_ref()
            .map_or(0, |result| result.row_count() as u64);
        self.query_has_more = single_read_only && paging_capable && truncated;
        // 新语句 = 新结果：**列名序列**变了才把横向位置回零；同一份结构的
        // 「加载更多」不经过这里，不会打断正在看右侧列的人。这步归数据表组件
        // （`set_model` 内部判列名），页面只管把新结果推过去。
        let view = QueryView {
            result: outcome.result,
        };
        match edit_meta {
            Some((table, pk)) => {
                self.query_target = Some(table);
                self.query_pk = pk;
            }
            None => {
                self.query_target = None;
                self.query_pk.clear();
            }
        }
        // 只有单条读语句才记 SQL：多语句批次重跑会把写语句再跑一遍。
        self.query_sql = Some(sql.clone());
        // **一条语句一个结果标签**（dbx 同款）：标签持有自己的视图与可编辑目标；
        // `query_result` 恒等于激活标签的视图 —— 既有渲染路径（网格 / 导出 / 编辑）
        // 不用感知标签的存在，切标签就是把标签里的东西装回去。
        //
        // 例外：编辑提交后的重跑（`result_replace_active`）**替换**激活标签 ——
        // 保存改的就是这一条语句的结果，再追加一条一模一样的标签只会堆重复。
        let new_tab = ResultTab {
            sql,
            view: view.clone(),
            target: self.query_target.clone(),
            pk: self.query_pk.clone(),
            elapsed_ms: self.query_elapsed_ms,
        };
        if self.result_replace_active && !self.result_tabs.is_empty() {
            self.result_replace_active = false;
            let slot = self.active_result_tab.min(self.result_tabs.len() - 1);
            self.result_tabs[slot] = new_tab;
            self.active_result_tab = slot;
        } else {
            self.result_tabs.push(new_tab);
            self.active_result_tab = self.result_tabs.len() - 1;
        }
        self.query_result = Some(view);
        // 这次执行成功了：上一次的错误上下文作废。
        // （AI 解释不再跟着结果作废 —— 它现在是右栏对话里的一条消息，用户回头还要看。）
        self.last_error = None;
        self.selected_row = None;
        self.row_editor = None;
        // 结果集换了：旧格子的详情指向与就地编辑器一起作废。
        self.cell_detail = None;
        self.dismiss_cell_dock_if_auto(cx);
        self.cell_editor = None;
        // 结果集换了：旧的勾选按「旧结果集的下标」记，必须清掉；暂存改动同理
        // （它们按旧行号定位，重跑后行号可能指向别的行）。
        self.checked_rows.clear();
        self.clear_pending();
        // 列头的本地筛选与格式化跟着旧列名走已经没有意义，一并清掉（列级状态
        // 只对「看着的这份结果」负责）。
        self.local_filters.clear();
        self.column_formats.clear();
        // 计划视图跟**这一次**的结果绑定：这一步是 EXPLAIN 就解析一份，否则清掉
        // （普通查询不能在结果区留下上一次的计划）。
        let planned = std::mem::take(&mut self.plan_next);
        self.plan_view = planned
            .then(|| {
                self.query_result
                    .as_ref()
                    .and_then(|view| view.result.as_ref())
                    .map(parse_plan)
            })
            .flatten()
            .and_then(|tree| match tree {
                PlanTree::Nodes(nodes) if !nodes.is_empty() => Some(PlanView { nodes, raw: false }),
                // 解析不出来就不进计划态：结果区照旧显示原始网格（这就是「原样文本
                // 展示」，输出一点没丢），只把「没解析出来」这件事记下来给状态栏说。
                PlanTree::Nodes(_) | PlanTree::Raw(_) => None,
            });
        self.plan_flat = planned && self.plan_view.is_none();
        // 计划解析成功就**直接切到「执行计划」tab**：用户点的是「执行计划」，
        // 结果区该立刻给树 —— 留在「数据表」档只看得到 EXPLAIN 原文，树要自己
        // 再点一次 tab 才出现（看起来就像「点了没反应」）。
        if planned && self.plan_view.is_some() {
            self.result_area_tab = ResultAreaTab::Plan;
        }
        // 结果换完了：表模型 / 展示行序 / 藏列一次推齐给数据表组件。
        self.sync_data_table(cx);
    }

    /// 批次队列的下一条语句（`run_query` 排队、`execute_sql` 收尾时续跑）。
    ///
    /// 调用时机在守护收工**之后**（`spawn_db_guarded` 先摘 `running` 再交结果），
    /// 所以这里发起下一条不会被「上一条还在跑」的闸门挡住。
    pub(super) fn run_next_queued_statement(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, sql)) = self.stmt_queue.pop_front() else {
            return;
        };
        self.execute_sql(sql, Some(index), window, cx);
    }

    /// 切换结果标签：把该标签的视图与可编辑目标装回当前展示态。
    ///
    /// 换了结果集，按旧结果集记的东西（分页 / 选中 / 勾选 / 暂存 / 计划 / 图表）
    /// 全部作废 —— 与重跑换结果同一口径。
    pub(super) fn switch_result_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == self.active_result_tab || index >= self.result_tabs.len() {
            return;
        }
        self.install_result_tab(index, cx);
    }

    /// 把第 `index` 条结果标签装回展示态（调用方保证下标合法）。
    ///
    /// 关标签也会走到这里（关掉当前标签要顺势装回邻座），所以不能带
    /// 「与当前相同就跳过」的前置判断——那个判断留在 [`Self::switch_result_tab`]。
    pub(super) fn install_result_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.result_tabs.get(index) else {
            return;
        };
        // 借用阶段把要搬的都克隆出来：后面要整块 `&mut self`（清暂存 / 换状态）。
        let view = tab.view.clone();
        let target = tab.target.clone();
        let pk = tab.pk.clone();
        let sql = tab.sql.clone();
        let elapsed_ms = tab.elapsed_ms;
        let loaded = view
            .result
            .as_ref()
            .map_or(0, |result| result.row_count() as u64);
        self.active_result_tab = index;
        self.query_result = Some(view);
        self.query_target = target;
        self.query_pk = pk;
        self.query_sql = Some(sql);
        self.query_elapsed_ms = elapsed_ms;
        self.query_loaded = loaded;
        // 「加载更多」只在重跑路径上成立：续拉把新行 append 进**当前**视图，
        // 装回去的旧标签是完整快照，给入口只会把快照撑破。
        self.query_has_more = false;
        self.query_page = 0;
        self.selected_row = None;
        self.row_editor = None;
        self.cell_detail = None;
        self.dismiss_cell_dock_if_auto(cx);
        self.cell_editor = None;
        self.checked_rows.clear();
        self.clear_pending();
        self.plan_view = None;
        self.plan_flat = false;
        self.chart_view = None;
        self.last_error = None;
        // 换标签 = 换结果集：列头的本地筛选与格式化跟着旧列名已经没有意义，清掉
        // （列级状态只对「看着的这份结果」负责）。
        self.local_filters.clear();
        self.column_formats.clear();
        // 换标签 = 换结果集：把这一条标签的结果推给数据表组件。列宽**不随标签存**
        // ——`set_model` 只在列名序列变化时重估，所以与本标签同结构时宽度自然沿用。
        self.sync_data_table(cx);
        cx.notify();
    }

    /// 关掉一条结果标签（标签格上的 ✕）。
    ///
    /// 关掉的是**这一条语句的结果**；把最后一条也关掉 = 结果区回到「还没有结果」
    /// 的原始态（网格 / 读数条 / 脚注一起让位）。关掉**非**当前标签时当前视图不动，
    /// 只把下标左移 —— 别顺手重装一遍，那会白丢分页 / 选中 / 暂存里的东西。
    pub(super) fn close_result_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.result_tabs.len() {
            return;
        }
        self.result_tabs.remove(index);
        if self.result_tabs.is_empty() {
            self.reset_result_area(cx);
            return;
        }
        if index == self.active_result_tab {
            // 关的正是看着的这条 → 装回左邻（首条则装回新的首条）。
            let next = index.saturating_sub(1).min(self.result_tabs.len() - 1);
            self.install_result_tab(next, cx);
        } else {
            if index < self.active_result_tab {
                self.active_result_tab -= 1;
            }
            cx.notify();
        }
    }

    /// 结果区回到「还没有结果」的原始态（关掉最后一条结果标签时）。
    pub(super) fn reset_result_area(&mut self, cx: &mut Context<Self>) {
        self.active_result_tab = 0;
        self.query_result = None;
        self.query_target = None;
        self.query_pk.clear();
        self.query_sql = None;
        self.query_elapsed_ms = None;
        self.query_loaded = 0;
        self.query_has_more = false;
        self.query_page = 0;
        self.selected_row = None;
        self.row_editor = None;
        self.cell_detail = None;
        self.dismiss_cell_dock_if_auto(cx);
        self.cell_editor = None;
        self.checked_rows.clear();
        self.clear_pending();
        self.plan_view = None;
        self.plan_flat = false;
        self.chart_view = None;
        self.last_error = None;
        // 结果区清空：给组件推一份空表（行序也清掉）；下一次装结果时会重新推。
        self.sync_data_table(cx);
        cx.notify();
    }

    /// 追加一条「消息」tab 的执行流水（新条目在尾部；超 [`EXEC_LOG_CAP`] 裁头部）。
    ///
    /// 成功 / 失败都记（[`Self::execute_sql`] 的两个回调各调一次）：dbx 的 messages
    /// 面板价值就在「这一批到底哪条没跑成、跑了多久」——只记失败会丢掉耗时线索。
    pub(super) fn push_exec_log(
        &mut self,
        ok: bool,
        statements: &[String],
        detail: &str,
        elapsed_ms: u128,
        cx: &mut Context<Self>,
    ) {
        let label = match statements {
            [one] => one_line_sql(one),
            many => trf(cx, "{} 条语句", &[&many.len().to_string()]).to_string(),
        };
        self.exec_log.push(ExecLogEntry {
            ok,
            label,
            detail: detail.to_owned(),
            elapsed_ms,
        });
        if self.exec_log.len() > EXEC_LOG_CAP {
            let overflow = self.exec_log.len() - EXEC_LOG_CAP;
            self.exec_log.drain(0..overflow);
        }
        cx.notify();
    }

    /// 切结果区顶部 tab（dbx `QueryResultViewSwitcher` 同款）。
    ///
    /// 第一次切进图表时按结果集**自动选列**（与旧「看图表」入口同一条路径，
    /// 见 [`Self::toggle_chart_view`]）：没有可画的数值列就留在空态提示，
    /// 而不是切过去给一张空图。切 tab 不清结果、不动分页与暂存。
    pub(super) fn set_result_area_tab(&mut self, tab: ResultAreaTab, cx: &mut Context<Self>) {
        if self.result_area_tab == tab {
            return;
        }
        self.result_area_tab = tab;
        // 第一次切进图表按结果集自动选列：没有可画的数值列就留在空态提示。
        if tab == ResultAreaTab::Chart
            && self.chart_view.is_none()
            && let Some(result) = self.current_result()
        {
            self.chart_view = ChartConfig::from_result(result);
        }
        cx.notify();
    }

    /// 双击单元格的就地编辑侦察（只读，借用阶段做完）：原值 + 是否 `NULL`。
    pub(super) fn cell_editor_prep(&self, row: usize, col: usize) -> Option<(String, bool)> {
        let result = self.current_result()?;
        let cell = result.rows.get(row)?.get(col)?;
        Some((cell.render().to_string(), cell.is_null()))
    }

    /// 装上就地编辑器（输入框已由调用方在 `window` 上建好、填好初值）。
    pub(super) fn install_cell_editor(
        &mut self,
        row: usize,
        col: usize,
        original: String,
        was_null: bool,
        input: Entity<InputState>,
        cx: &mut Context<Self>,
    ) {
        // 回车提交；**失焦也提交**（dbx 同款「点别处 = 确认」）——不然用户改完
        // 点一下别处，改动就静默丢了，格子外又没有可按的保存键。
        let sub = cx.subscribe(
            &input,
            move |this: &mut Self,
                  _source: Entity<InputState>,
                  event: &InputEvent,
                  cx: &mut Context<Self>| {
                match event {
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        // 就地编辑器不拿 window：报错走状态栏（弹窗内的 notify 需要
                        // window，这里是网格里的一格，状态栏更贴身）。
                        this.commit_cell_edit(cx);
                    }
                    _ => {}
                }
            },
        );
        self.cell_editor = Some(CellEditor {
            row,
            col,
            original,
            was_null,
            input,
            _sub: sub,
        });
    }

    /// 提交就地编辑的这一格（回车）：走与行编辑器**同一条**暂存路径
    /// （`pending_edits` 按 (主键, 列) 去重，「提交」时一个事务写下去）。
    pub(super) fn commit_cell_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.cell_editor.take() else {
            return;
        };
        let Some(result) = self.current_result() else {
            return;
        };
        let Some(column) = result.columns.get(editor.col) else {
            return;
        };
        let name = column.name.clone();
        let text = editor.input.read(cx).value().to_string();
        // 与行编辑器同一套「改没改」判定：`NULL` 格留空 = 没碰过，不算改动。
        let unchanged = match (!editor.was_null).then_some(editor.original.as_str()) {
            Some(original) => text == original,
            None => text.is_empty(),
        };
        if unchanged {
            cx.notify();
            return;
        }
        let Some(pk_values) = self.pk_values_for_row(editor.row) else {
            self.status = tr(cx, "该表没有主键，无法定位行。").into();
            cx.notify();
            return;
        };
        let edit = CellEdit {
            pk: pk_values,
            column: name,
            value: Some(text),
        };
        // 一步一记（撤销栈的落点）；同一格改两次后一次覆盖前一次。
        self.record_pending();
        self.pending_edits
            .retain(|old| !(old.pk == edit.pk && old.column == edit.column));
        self.pending_edits.push(edit);
        self.status = trf(
            cx,
            "已暂存 {} 处改动（共 {} 条待提交）",
            &[&1.to_string(), &self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 当前结果集的**可编辑来源**；不可编辑（无表 / 无主键 / 列对不上）返回 `None`。
    pub(super) fn edit_source(&self) -> Option<EditSource<'_>> {
        match self.mode {
            Mode::Table => Some(EditSource {
                result: self.table_rows.as_ref()?,
                table: self.selected_table.as_ref()?,
                pk: &self.table_pk,
            }),
            Mode::Query => Some(EditSource {
                result: self.query_result.as_ref()?.result.as_ref()?,
                table: self.query_target.as_ref()?,
                pk: &self.query_pk,
            }),
            // 实例信息页没有可编辑的单元格（它不是结果集，是服务器事实）。
            // 仪表板同理：面板里那些行是**读出来给人看的**，不是可改的暂存区。
            // 结构编辑器改的是定义，不是行数据，同理。
            Mode::Instance(_)
            | Mode::Dashboard(_)
            | Mode::Structure(_)
            | Mode::Er { .. }
            | Mode::Compare(_)
            | Mode::Transfer(_) => None,
        }
    }

    /// 编辑落库后重建当前视图：表浏览重取这一页，SQL 结果重跑那条语句。
    ///
    /// 重跑是安全的：能走到编辑这条路的语句必然是单表只读 SELECT（[`edit_target`] 的
    /// 前提），不可能是带副作用的语句。
    pub(super) fn reload_edit_source(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Table => {
                self.table_meta_for = None;
                self.load_table_page(cx);
            }
            Mode::Query => {
                if let Some(sql) = self.query_sql.clone() {
                    // 重跑结果**替换**当前标签（保存编辑不新增标签）。
                    self.result_replace_active = true;
                    self.execute_sql(sql, None, window, cx);
                }
            }
            // 实例信息页不可编辑，也就没有「重载来源」这回事；仪表板面板同理。
            // 结构标签改完列的落库动作走自己的「应用变更」，不进这条重载。
            Mode::Instance(_)
            | Mode::Dashboard(_)
            | Mode::Structure(_)
            | Mode::Er { .. }
            | Mode::Compare(_)
            | Mode::Transfer(_) => {}
        }
    }

    /// 指定行的编辑器数据源：列名 + 展示原值 + 是否 NULL；不可编辑时返回 `None`。
    pub(super) fn row_editor_specs(&self, row_index: usize) -> Option<Vec<(String, String, bool)>> {
        let source = self.edit_source()?;
        let row = source.result.rows.get(row_index)?;
        Some(
            source
                .result
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let cell = row.get(index);
                    let original = cell
                        .map(|cell| cell.render().to_string())
                        .unwrap_or_default();
                    let is_null = cell.is_some_and(duck_db::Value::is_null);
                    (column.name.clone(), original, is_null)
                })
                .collect(),
        )
    }

    /// 新增行的编辑器数据源：所有列，值全空（不填的列提交时落到数据库 `DEFAULT`）。
    pub(super) fn new_row_specs(&self) -> Option<Vec<(String, String, bool)>> {
        let _source = self.edit_source()?;
        let columns = self
            .current_result()?
            .columns
            .iter()
            .map(|column| (column.name.clone(), String::new(), false))
            .collect();
        Some(columns)
    }

    /// 当前行（指定下标）的主键定位值；无主键或列对不上返回 `None`。
    pub(super) fn pk_values_for_row(&self, row_index: usize) -> Option<PkValues> {
        let source = self.edit_source()?;
        if source.pk.is_empty() {
            return None;
        }
        let row = source.result.rows.get(row_index)?;
        let mut pk_values = Vec::with_capacity(source.pk.len());
        for name in source.pk {
            let index = source
                .result
                .columns
                .iter()
                .position(|column| &column.name == name)?;
            pk_values.push((name.clone(), row.get(index)?.render().to_string()));
        }
        Some(pk_values)
    }

    /// 把行编辑器的改动**暂存**起来（不落库）。
    ///
    /// 收下之后编辑卡就关掉（`row_editor = None`）：改动已经在暂存区，卡里再留着
    /// 一份输入框只会让人以为「还没生效」。
    pub(super) fn stage_row_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.row_editor.take() else {
            return;
        };
        if !self.stage_editor_edits(editor.row_index, &editor.cells, window, cx) {
            // 没收下（不可编辑 / 没有改动）：表单留在原地，别让用户白填一遍。
            self.row_editor = Some(editor);
        }
        cx.notify();
    }

    /// 把 JSON 档字段表单的改动**暂存**起来（同一套 `pending_edits` 模型）。
    ///
    /// 收下之后把表单清掉：下一轮渲染会按**当前行**的指纹重建 —— 值回到库里现在
    /// 的样子，而「有几处待提交」由工具条上的暂存计数说（与网格格子的口径一致：
    /// 暂存的改动不直接改结果集）。
    pub(super) fn stage_json_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.json_editor.take() else {
            return;
        };
        if !self.stage_editor_edits(Some(form.row), &form.cells, window, cx) {
            self.json_editor = Some(form);
        }
        cx.notify();
    }

    /// 暂存的**共用内核**：一份编辑表单（行号 + 每列输入框）的改动进暂存区。
    ///
    /// 两个宿主共用：右栏行编辑卡（`row_index: None` = 新增行）与 JSON 档字段表单
    /// （恒 `Some(行号)`）。`true` = 收下了；`false` 一律是「什么都没发生」（结果集
    /// 不可编辑 / 没改动 / 新增行一列没填）—— 调用方据此决定表单留不留。
    ///
    /// 这是 `pending_edits` 模型：编辑只进暂存区，点「提交」时成一个事务写下去。
    /// 每列一次判定：`null` 开关打开 → 写 `NULL`；否则只在文本**真的变了**时暂存。
    fn stage_editor_edits(
        &mut self,
        row_index: Option<usize>,
        cells: &[RowCell],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // 兜一道「结果集可编辑」：无表 / 无主键 / 主键列不在结果里时，暂存毫无意义
        // （点了提交也定位不到行）。正常路径上提交按钮已置灰，这里是防漏。
        if self.edit_source().is_none() {
            return false;
        }
        // 新增行：把填过的列攒成一条 INSERT（空且未勾 NULL 的列不提交，让 DEFAULT 生效）。
        let Some(row_index) = row_index else {
            let mut columns = Vec::new();
            let mut values = Vec::new();
            for cell in cells {
                let is_null = cell.null.get();
                let text = cell.input.read(cx).value().to_string();
                if is_null {
                    columns.push(cell.column.clone());
                    values.push(None);
                } else if !text.is_empty() {
                    columns.push(cell.column.clone());
                    values.push(Some(text));
                }
            }
            if columns.is_empty() {
                notify::info(
                    tr(cx, "新增行至少要填一列（空白列会落到数据库默认值）。"),
                    window,
                    cx,
                );
                return false;
            }
            self.record_pending();
            self.pending_inserts.push(InsertRow { columns, values });
            self.status = trf(
                cx,
                "已暂存 1 条新增行（共 {} 条待提交）",
                &[&self.pending_total().to_string()],
            )
            .into();
            return true;
        };
        let Some(pk_values) = self.pk_values_for_row(row_index) else {
            notify::error(tr(cx, "该表没有主键，无法定位行。"), window, cx);
            return false;
        };
        let mut staged = Vec::new();
        for cell in cells {
            let is_null = cell.null.get();
            let text = cell.input.read(cx).value().to_string();
            let value = if is_null { None } else { Some(text) };
            // 「原值」的等价形式：`NULL` 就是「没有值」，不能拿展示文本 `"NULL"` 去比
            // （否则「NULL → NULL」会被判成改动，而「把 NULL 改成空串」又判不出来）。
            let original: Option<&str> = (!cell.was_null).then_some(cell.original.as_str());
            let unchanged = match (value.as_deref(), original) {
                // 两边都没值。
                (None, None) => true,
                // 都有值：文本一致才算没改。
                (Some(text), Some(original)) => text == original,
                // 原为 NULL、输入框仍是空的 = 没碰过（NULL 格进编辑器时输入框本就是空的）。
                (Some(text), None) => text.is_empty(),
                // 有值 → NULL：是改动。
                (None, Some(_)) => false,
            };
            if unchanged {
                continue;
            }
            staged.push(CellEdit {
                pk: pk_values.clone(),
                column: cell.column.clone(),
                value,
            });
        }
        if staged.is_empty() {
            notify::info(tr(cx, "没有改动。"), window, cx);
            return false;
        }
        let count = staged.len();
        // 这一步（无论改了几格、是否覆盖了同格的旧改动）在撤销栈里**只算一步**：
        // 用户按一次「保存」，`⌘Z` 就退回按之前。
        self.record_pending();
        // 同一格改两次：后一次覆盖前一次（旧的那条按 (pk, 列) 摘掉）。
        for edit in staged {
            self.pending_edits
                .retain(|old| !(old.pk == edit.pk && old.column == edit.column));
            self.pending_edits.push(edit);
        }
        self.status = trf(
            cx,
            "已暂存 {} 处改动（共 {} 条待提交）",
            &[&count.to_string(), &self.pending_total().to_string()],
        )
        .into();
        true
    }

    /// 暂存区里一共有多少条待提交（改动 + 新增行）。
    pub(super) fn pending_total(&self) -> usize {
        self.pending_edits.len() + self.pending_inserts.len()
    }

    /// 当前暂存区的一份快照。
    pub(super) fn pending_snapshot(&self) -> PendingSnapshot {
        PendingSnapshot {
            edits: self.pending_edits.clone(),
            inserts: self.pending_inserts.clone(),
        }
    }

    /// 把暂存区换成一份快照。
    pub(super) fn restore_pending(&mut self, snapshot: PendingSnapshot) {
        self.pending_edits = snapshot.edits;
        self.pending_inserts = snapshot.inserts;
    }

    /// 在**改动暂存区之前**记一步（撤销栈的落点）。
    ///
    /// 每处会改动暂存区的操作都要先调它：`stage_row_edits`（保存）、`discard_pending`
    /// （丢弃）、以及撤销 / 重做本身。记的是**改动前**的状态——撤销就是把暂存区恢复成它。
    ///
    /// 记完之后**重做栈作废**：走了新的一步，「未来的那一步」就不存在了
    /// （与文本编辑器的撤销口径一致）。
    pub(super) fn record_pending(&mut self) {
        self.pending_undo.push(self.pending_snapshot());
        if self.pending_undo.len() > UNDO_DEPTH {
            // 丢**最旧**的一步而不是整栈清空：用户最需要的一定是最近几步。
            self.pending_undo.remove(0);
        }
        self.pending_redo.clear();
    }

    /// 清空暂存区**连同撤销 / 重做栈**。
    ///
    /// 用在「暂存区整体作废」的地方：提交成功、换表、换库、结果集被重跑替换。
    /// 这些场景下暂存区里的改动本来就不再指向当前结果集，留着撤销栈只会让 `⌘Z`
    /// 恢复出一批**对不上当前表**的改动——那比不能撤销更坏。
    pub(super) fn clear_pending(&mut self) {
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.pending_undo.clear();
        self.pending_redo.clear();
    }

    /// 撤销一步（`⌘Z`）：回退到上一次「保存 / 丢弃」之前的样子。
    ///
    /// 不动 `row_editor`：行编辑器里的输入与暂存区是两份东西，撤销顺手把它关掉等于
    /// 把用户刚敲的字抹了。它留在原地，用户想再存一次就再点一次「保存」。
    pub(super) fn undo_pending(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.pending_undo.pop() else {
            self.status = tr(cx, "没有可撤销的暂存改动").into();
            cx.notify();
            return;
        };
        // 撤销前的状态压进重做栈，`⇧⌘Z` 才能原样回去。
        self.pending_redo.push(self.pending_snapshot());
        self.restore_pending(snapshot);
        self.status = trf(
            cx,
            "已撤销一步（共 {} 条待提交）",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 重做一步（`⇧⌘Z`）。
    pub(super) fn redo_pending(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.pending_redo.pop() else {
            self.status = tr(cx, "没有可重做的暂存改动").into();
            cx.notify();
            return;
        };
        self.pending_undo.push(self.pending_snapshot());
        self.restore_pending(snapshot);
        self.status = trf(
            cx,
            "已重做一步（共 {} 条待提交）",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
    }

    /// 丢掉全部暂存改动（**可撤销**：撤销栈里留了一步）。
    pub(super) fn discard_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_total() == 0 {
            // 空暂存区不留一步空快照：否则 `⌘Z` 会「撤销」出一次什么都没发生的操作，
            // 用户按第二下才退到真正想要的那一步。
            notify::info(tr(cx, "没有待提交的改动。"), window, cx);
            return;
        }
        self.record_pending();
        self.pending_edits.clear();
        self.pending_inserts.clear();
        self.status = tr(cx, "已丢弃暂存改动（⌘Z 可撤销）").into();
        notify::info(tr(cx, "暂存改动已丢弃。"), window, cx);
        cx.notify();
    }

    /// **提交**暂存区：改格 + 新增行在一个事务里写下去。
    pub(super) fn commit_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_total() == 0 {
            notify::info(tr(cx, "没有待提交的改动。"), window, cx);
            return;
        }
        let Some(source) = self.edit_source() else {
            notify::error(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let table = source.table.clone();
        // 审计要的「改了哪张表」在**发起时**就抽成字符串：`table` 本体要交给后台闭包，
        // 收工回调（`move` 闭包）里已经拿不到它了。
        let audit_target = table.display();
        let edits = self.pending_edits.clone();
        let inserts = self.pending_inserts.clone();
        let planned_updates = u64::try_from(edits.len()).unwrap_or(u64::MAX);
        let planned_inserts = u64::try_from(inserts.len()).unwrap_or(u64::MAX);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 审计的 scope 是**会话**（与 SQL 执行那条路径同一口径），不是连接档案 id：
        // 同一条连接连两次是两个会话，流水要能分开。
        let session_id = self.active.clone().unwrap_or_default();
        self.status = trf(
            cx,
            "正在提交 {} 条改动…",
            &[&self.pending_total().to_string()],
        )
        .into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.apply_batch(&table, &edits, &inserts)
            },
            move |this, result, window, cx| match result {
                Ok(outcome) => {
                    // 提交成功：暂存区连同撤销栈一起清（这一步已经落到库里，
                    // 撤销栈里剩下的「撤销到提交前」会指向一批已经写掉、且行号可能
                    // 已变的改动）。
                    this.clear_pending();
                    let BatchOutcome { updated, inserted } = outcome;
                    // 「提交写入」单独成一类动作：它和 `UPDATE` 语句都是写，但「暂存区攒了
                    // 一批改动一起落库」这件事只能从暂存区发生，按 `COMMIT` 记才好把它
                    // 和逐条执行区分开。
                    let detail = trf(
                        cx,
                        "提交 {} · 改 {} 行 · 新增 {} 行",
                        &[&audit_target, &updated.to_string(), &inserted.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::COMMIT,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
                    this.status = trf(
                        cx,
                        "已提交：改 {} 行 · 新增 {} 行",
                        &[&updated.to_string(), &inserted.to_string()],
                    )
                    .into();
                    notify::success(
                        trf(
                            cx,
                            "已提交：改 {} 行 · 新增 {} 行。",
                            &[&updated.to_string(), &inserted.to_string()],
                        ),
                        window,
                        cx,
                    );
                    if updated < planned_updates || inserted < planned_inserts {
                        // 少命中：多半是这些行已被他人改动（主键展示文本对不上）。
                        notify::warning(
                            tr(
                                cx,
                                "有改动未命中目标行（该行可能已被他人修改），请刷新后重试。",
                            ),
                            window,
                            cx,
                        );
                    }
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    // 整体回滚也记：失败的那一批**没有**落库，这一条告诉后来人
                    // 「这个时刻有过一次没写成的提交」，否则流水里只看得到成功的那些。
                    let detail = trf(cx, "提交失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::COMMIT,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail,
                    );
                    this.status = trf(cx, "提交失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "提交失败（已整体回滚）：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    // 「勾选 / 取消勾选一行」原先在这里（`toggle_row_checked`）：行勾选框现在由数据表
    // 组件画，事件带回来的是**目标态**（`DataTableEvent::RowCheckedToggled { checked }`），
    // 页面按它直接写集合即可 —— 不需要「翻一下」这种会与组件渲染态错开一帧的写法。

    /// 表头全选 / 全不选（范围 = 当前页；勾满时点一下就是清空）。
    pub(super) fn toggle_all_checked(&mut self, cx: &mut Context<Self>) {
        let total = self.current_result().map_or(0, ResultSet::row_count);
        if total > 0 && self.checked_rows.len() == total {
            self.checked_rows.clear();
        } else {
            self.checked_rows = (0..total).collect();
        }
        cx.notify();
    }

    /// 清空多选。**翻页 / 换表 / 重新执行都必须调**：行号是「当前结果集里的下标」，
    /// 结果集一换，旧的勾选就指向别的行了。
    pub(super) fn clear_checked_rows(&mut self) {
        self.checked_rows.clear();
    }

    /// 勾选行的主键定位值；任一行定位不了（无主键 / 下标越界）返回 `None`。
    ///
    /// 整批要么都能定位、要么一个都不动 —— 「删了一半」这种中间态最难收拾。
    pub(super) fn checked_pk_values(&self) -> Option<Vec<PkValues>> {
        if self.checked_rows.is_empty() {
            return None;
        }
        let mut keys = Vec::with_capacity(self.checked_rows.len());
        for index in &self.checked_rows {
            keys.push(self.pk_values_for_row(*index)?);
        }
        Some(keys)
    }

    /// 批量编辑：把「选中列 + 新值」应用到所有勾选行（**单事务**）。
    ///
    /// 表浏览与可编辑 SELECT 结果共用同一条路（[`Self::edit_source`] 决定改哪张表）。
    pub(super) fn apply_batch_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.batch_column.clone() else {
            notify::info(tr(cx, "请先选择要修改的列。"), window, cx);
            return;
        };
        if self.batch_null {
            // 勾了「写 NULL」就以它为准，输入框内容忽略。
        }
        let value = if self.batch_null {
            None
        } else {
            Some(self.batch_value.read(cx).value().to_string())
        };
        let Some(keys) = self.checked_pk_values() else {
            notify::info(
                tr(
                    cx,
                    "请先勾选要处理的行；当前结果集不可编辑（需要单表 + 主键）。",
                ),
                window,
                cx,
            );
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let edits: Vec<CellEdit> = keys
            .into_iter()
            .map(|pk| CellEdit {
                pk,
                column: column.clone(),
                value: value.clone(),
            })
            .collect();
        let planned = u64::try_from(edits.len()).unwrap_or(u64::MAX);
        // 审计的「改了什么」= 表 + 列（只写行数看不出改的是哪个字段，而批量改最危险的
        // 就是**改错列**）。`table` 与 `column` 都要交给后台闭包，所以先拼成字符串。
        let audit_target = format!("{}.{}", table.display(), column);
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let session_id = self.active.clone().unwrap_or_default();
        self.status = trf(cx, "正在批量修改 {} 行…", &[&planned.to_string()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.update_cells(&table, &edits)
            },
            move |this, result, window, cx| match result {
                Ok(affected) => {
                    let detail = trf(
                        cx,
                        "修改 {} · {} 行",
                        &[&audit_target, &affected.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::EDIT_ROW,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
                    this.status = trf(cx, "已批量修改 {} 行", &[&affected.to_string()]).into();
                    notify::success(
                        trf(cx, "已修改 {} 处。", &[&affected.to_string()]),
                        window,
                        cx,
                    );
                    if affected < planned {
                        notify::warning(
                            tr(
                                cx,
                                "有改动未命中目标行（行可能已被他人修改），请刷新后重试。",
                            ),
                            window,
                            cx,
                        );
                    }
                    this.clear_checked_rows();
                    // 行数没变，但值变了：表浏览的元信息（索引 / 行数）可以复用。
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    let detail = trf(cx, "批量修改失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::EDIT_ROW,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
                }
            },
        );
    }

    /// 批量删除勾选的行（**单事务**）。
    ///
    /// 只负责「守卫 + 问一句」：确认走主题弹窗（[`open_confirm`]，不是 rfd 系统
    /// 模态），真正的删除在 [`Self::apply_batch_delete_confirmed`] —— 弹窗是异步的，
    /// 本函数在用户点下「删除」之前就返回了。
    pub(super) fn apply_batch_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(keys) = self.checked_pk_values() else {
            notify::info(tr(cx, "请先勾选要处理的行，或该表没有主键。"), window, cx);
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        // ⚠️ 删除范围在**点按钮那一刻**冻结（表 + 主键值），确认期间不回读页面状态：
        // 弹窗开着时用户翻页 / 切标签 / 换表，回读会删掉另一批行 —— 而问句里写的是
        // 他刚才看到的那个数（与 redis 页 `PendingAction` 同一条纪律）。
        let view = self.view.clone();
        let frozen = Rc::new((table, keys));
        open_confirm(
            tr(cx, "批量删除行"),
            trf(
                cx,
                "将从数据库中删除 {} 行，确定继续吗？",
                &[&frozen.1.len().to_string()],
            ),
            tr(cx, "删除"),
            Rc::new(move |window, cx| {
                let (table, keys) = (*frozen).clone();
                let _ = view.update(cx, |this, cx| {
                    this.apply_batch_delete_confirmed(table, keys, window, cx);
                });
            }),
            window,
            cx,
        );
    }

    /// 批量删除的执行段（确认之后）：一个事务删掉**冻结下来的**那批行，并落审计流水。
    fn apply_batch_delete_confirmed(
        &mut self,
        table: TableRef,
        keys: Vec<PkValues>,
        // 收尾提示都走 `spawn_db` 的回调（那里的 `window` 是另一份），本函数自己
        // 用不到窗口 —— 留着参数是为了与 `open_confirm` 的回调同形，调用处不分叉。
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let planned = u64::try_from(keys.len()).unwrap_or(u64::MAX);
        let audit_target = table.display();
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let session_id = self.active.clone().unwrap_or_default();
        self.status = trf(cx, "正在删除 {} 行…", &[&planned.to_string()]).into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.delete_row_set(&table, &keys)
            },
            move |this, result, window, cx| match result {
                Ok(affected) => {
                    // 删行是**最需要留痕**的写：改错了还能改回来，删掉的行只有这条流水
                    // 说得清「什么时候、按什么范围删的」（具体值在确认框里，已随通知消失）。
                    let detail = trf(
                        cx,
                        "删除 {} · {} 行",
                        &[&audit_target, &affected.to_string()],
                    );
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
                    this.status = trf(cx, "已删除 {} 行", &[&affected.to_string()]).into();
                    notify::success(
                        trf(cx, "已删除 {} 行。", &[&affected.to_string()]),
                        window,
                        cx,
                    );
                    if affected < planned {
                        notify::warning(
                            tr(cx, "有行未命中（可能已被他人删除），请刷新后重试。"),
                            window,
                            cx,
                        );
                    }
                    this.clear_checked_rows();
                    // 行数变了：让表级元信息跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    let detail = trf(cx, "批量删除失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
                }
            },
        );
    }

    /// 删除编辑中的行（确认后按主键删）。
    ///
    /// 只负责「守卫 + 问一句」：确认走主题弹窗（[`open_confirm`]，不是 rfd 系统模态），
    /// 真正的删除在 [`Self::delete_current_row_confirmed`]。
    pub(super) fn delete_current_row(
        &mut self,
        row_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pk_values) = self.pk_values_for_row(row_index) else {
            notify::error(tr(cx, "该表没有主键，无法定位行。"), window, cx);
            return;
        };
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::error(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        // ⚠️ 定位值在**点按钮那一刻**冻结：弹窗开着时用户翻页 / 切标签会让
        // `row_index` 指向另一行，回读就会删错行（与 redis 页 `PendingAction`
        // 同一条纪律：确认期间不重读动作参数）。
        let view = self.view.clone();
        let frozen = Rc::new((table, pk_values));
        open_confirm(
            tr(cx, "删除行"),
            tr(cx, "这一行将从数据库中删除，确定继续吗？"),
            tr(cx, "删除"),
            Rc::new(move |window, cx| {
                let (table, pk_values) = (*frozen).clone();
                let _ = view.update(cx, |this, cx| {
                    this.delete_current_row_confirmed(table, pk_values, window, cx);
                });
            }),
            window,
            cx,
        );
    }

    /// 单行删除的执行段（确认之后）：按**冻结下来的**主键值删，并落审计流水。
    fn delete_current_row_confirmed(
        &mut self,
        table: TableRef,
        pk_values: PkValues,
        // 收尾路径的提示都走 `spawn_db` 的回调（那里的 `window` 是另一份），本函数
        // 自己用不到窗口 —— 但签名与 `open_confirm` 的回调对齐，调用处不必分叉。
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let audit_target = table.display();
        let session_id = self.active.clone().unwrap_or_default();
        self.status = tr(cx, "正在删除行…").into();
        cx.notify();
        self.spawn_db(
            cx,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.delete_rows(&table, &pk_values)
            },
            // 这一处回调本来是**不捕获**任何东西的（所以没写 `move`）；现在它要带
            // `audit_target` / `session_id` 进闭包，加 `move` 才拿得到所有权。
            move |this, result, window, cx| match result {
                Ok(_) => {
                    // 单行删除也用「删除 {} · {} 行」这条文案（行数恒为 1）：
                    // 同一个动作在流水里应该长成同一个样子，别按入口分两种写法。
                    let detail = trf(cx, "删除 {} · {} 行", &[&audit_target, "1"]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Ok,
                        &session_id,
                        detail,
                    );
                    this.status = tr(cx, "行已删除").into();
                    notify::success(tr(cx, "行已删除。"), window, cx);
                    // 行数一定变了：让表级元信息跟着重取。
                    this.table_meta_for = None;
                    this.reload_edit_source(window, cx);
                }
                Err(error) => {
                    let detail = trf(cx, "删除失败：{}", &[&error.to_string()]);
                    crate::audit::record_event(
                        cx,
                        crate::audit::AuditKind::Write,
                        crate::audit::action::DELETE_ROWS,
                        crate::audit::AuditOutcome::Failed,
                        &session_id,
                        detail.clone(),
                    );
                    this.status = detail.clone().into();
                    notify::error(detail, window, cx);
                }
            },
        );
    }

    /// 导出当前结果集（[`ResultFormat`] 里任一档）。
    ///
    /// 导出文本由**内核的中性层**生成（见 [`ResultFormat::render`]），这里只管选格式、
    /// 拼表名与落盘——格式逻辑不进 UI，也不属于任何一条驱动。
    ///
    /// ⚠️ 落点走 [`crate::file_dialogs`]（gpui 自己的**异步**面板），不是
    /// `duck_platform::save_file`：后者在 macOS 上是嵌套模态循环，从这里调会把 gpui
    /// 的事件派发重入进来，撞成「RefCell already borrowed」并 abort（实证见
    /// `crash-2026-10-15.log` 12:46 那条）。异步随之带来一条约束 —— 内容必须在
    /// **打开面板之前**渲染完：用户挑路径的这段时间里页面可能已经翻页 / 换标签 /
    /// 换了连接，回调里再读 `current_result()` 导出的就不是他点下拉那一刻看着的那份。
    pub(super) fn export_result(
        &mut self,
        format: ResultFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 渲染与「借 `result`」都收在这个块里，块一结束借用就还回去 —— 后面那次
        // 异步的保存面板才拿得到 `&mut self`。
        let payload = {
            let Some(result) = self.current_result() else {
                notify::info(tr(cx, "当前没有可导出的结果。"), window, cx);
                return;
            };
            if format.is_binary() {
                // 原始字节档走字节路径：它不是文本，不能过 `write(&path, text)` 那道口子。
                match format.render_bytes(result) {
                    Some(bytes) => ExportPayload::Bytes(bytes),
                    None => {
                        notify::info(
                            tr(cx, "原始字节导出要求结果只有一列且全是二进制值。"),
                            window,
                            cx,
                        );
                        return;
                    }
                }
            } else {
                // INSERT 是 SQL 草稿：表名与列名都要按引擎的引号规则拼。MongoDB /
                // Elasticsearch 没有 SQL 标识符概念（菜单里已对它们禁用这一档），
                // 这里再挡一次，避免别处调进来产出坏 SQL。
                let insert = self.insert_context();
                let rendered = format.render(
                    result,
                    insert
                        .as_ref()
                        .map(|(table, quote)| (table.as_str(), *quote)),
                );
                match rendered {
                    Some(text) => ExportPayload::Text(text),
                    None => {
                        notify::info(tr(cx, "当前引擎不支持导出 INSERT 语句。"), window, cx);
                        return;
                    }
                }
            }
        };
        let file_name = format.file_name();
        let note = format.done_note(cx);
        crate::file_dialogs::save_file(cx, file_name, move |path, this, cx| {
            this.spawn_db(
                cx,
                move || payload.write(&path),
                move |this, result, window, cx| match result {
                    Ok(()) => {
                        notify::success(note, window, cx);
                        this.status = tr(cx, "导出完成").into();
                    }
                    Err(error) => {
                        notify::error(trf(cx, "导出失败：{}", &[&error]), window, cx);
                    }
                },
            );
        });
    }

    /// 渲染 INSERT 草稿需要的额外入参：表引用文本 + 引擎的引号字符。
    ///
    /// MongoDB / Elasticsearch 没有 SQL 标识符概念（`identifier_quote()` 为 `None`），
    /// 这里就返回 `None` —— 调用方据此把那一档禁掉或回绝。
    pub(super) fn insert_context(&self) -> Option<(String, char)> {
        let quote = self
            .active_config()
            .and_then(|cfg| cfg.kind.identifier_quote())?;
        Some((self.export_table_name(quote), quote))
    }

    /// 导出 INSERT 时用的表引用文本（按引擎引号拼限定名）。
    ///
    /// 表浏览模式用当前表，SQL 结果模式用 [`Self::query_target`]（可定位单表时）；
    /// 两者都拿不到（任意查询 / 多表 join）时退回占位名 `result`——用户一眼看得出
    /// 需要替换，比编一个像真的表名好。
    pub(super) fn export_table_name(&self, quote: char) -> String {
        let table = match self.mode {
            Mode::Table => self.selected_table.as_ref(),
            Mode::Query => self.query_target.as_ref(),
            Mode::Instance(_)
            | Mode::Dashboard(_)
            | Mode::Structure(_)
            | Mode::Er { .. }
            | Mode::Compare(_)
            | Mode::Transfer(_) => None,
        };
        table
            .and_then(|table| quote_qualified(table, quote).ok())
            .unwrap_or_else(|| "result".to_owned())
    }

    /// 从 CSV 文件导入数据到当前浏览的表（首行为列名，按列名对位）。
    ///
    /// ⚠️ 选文件走 [`crate::file_dialogs`]（gpui 自己的**异步**面板）。原来那句
    /// `duck_platform::open_file()` 正是这条闪退的直接原因 ——
    /// `crash-2026-10-15.log` 12:46 那条的调用链就是
    /// `import_csv → duck_platform::dialog::open_file → rfd pick_file → run_modal`，
    /// 原理见该模块注释。异步之后「去哪张表导入」也要在**开面板之前**定下来
    ///（用户挑文件的这段时间里换表 / 切标签都会变），所以表引用与连接都先冻住。
    pub(super) fn import_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        // 导入目标与改格 / 删行同源：表浏览的当前表，或可编辑 SELECT 背后的表。
        let Some(table) = self.edit_source().map(|source| source.table.clone()) else {
            notify::info(
                tr(cx, "当前结果集不可编辑（需要单表 + 主键）。"),
                window,
                cx,
            );
            return;
        };
        let conn = session.conn.clone();
        crate::file_dialogs::open_file(
            cx,
            tr(cx, "选择要导入的 CSV 文件"),
            move |path, this, cx| {
                this.status = tr(cx, "正在导入 CSV…").into();
                cx.notify();
                this.spawn_db(
                    cx,
                    move || -> Result<u64, String> {
                        let text = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
                        let mut rows = parse_csv(&text);
                        if rows.is_empty() {
                            return Err("file is empty".into());
                        }
                        let header = rows.remove(0);
                        let columns: Vec<String> = header
                            .into_iter()
                            .map(|cell| cell.unwrap_or_default())
                            .collect();
                        if columns.iter().any(String::is_empty) {
                            return Err("header contains an empty column name".into());
                        }
                        let mut guard = conn.lock().expect("连接锁不应中毒");
                        guard
                            .insert_rows(&table, &columns, &rows)
                            .map_err(|error| error.to_string())
                    },
                    |this, inserted, window, cx| match inserted {
                        Ok(count) => {
                            notify::success(
                                trf(cx, "已导入 {} 行。", &[&count.to_string()]),
                                window,
                                cx,
                            );
                            // 行数变了：表级元信息（行数）也跟着重取。
                            this.table_meta_for = None;
                            this.reload_edit_source(window, cx);
                        }
                        Err(error) => {
                            this.status = trf(cx, "导入失败：{}", &[&error]).into();
                            notify::error(trf(cx, "导入失败：{}", &[&error]), window, cx);
                        }
                    },
                );
            },
        );
    }

    /// 「执行 SQL 文件」：选一个 `.sql` 脚本，**流式**读进来分批判跑。
    ///
    /// 与 [`Self::run_query`] 正好是两条路：那条把编辑器的内容按「一条语句一个结果
    /// 标签」排队跑（每条一次加锁、一个标签）；这条走 [`execute_sql_file`] 的批处理路径
    /// ——**一次加锁 + 条数/字节双阈值刷批 + 字节级进度**，且整份脚本**不进结果标签**：
    /// 一个几百条 INSERT 的 dump 不该在结果区堆出几百个标签。
    ///
    /// ⚠️ 选文件走 [`crate::file_dialogs`]（gpui 自己的**异步**面板）：面板可能开着很久，
    /// 用户在这期间会切标签 / 换连接，所以**目标连接、结果上限全在开面板之前冻住**
    /// （与 [`Self::import_csv`] 同口径）。
    ///
    /// ⚠️ 这里**不做事前的危险确认**：判据（`risk_of_statements`）要先读一遍全文的语句
    /// 清单，而这条路的前提就是「不把整份文本端在内存里等着」。替代的三道闸是：默认
    /// **遇错即停**、随时可「停止」（谓词逐条生效 + 打断在途语句）、结束后把失败逐条
    /// 摆进执行流水与提示。
    pub(super) fn run_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            notify::info(tr(cx, "请先连接一个数据库。"), window, cx);
            return;
        };
        // 一条连接上同时压两份脚本没有意义（另一份已经在往外写，表结构还在变）。
        if self.running.is_some() {
            notify::info(tr(cx, "有数据库操作正在进行，请稍候。"), window, cx);
            return;
        }
        let session_id = self.active.clone().unwrap_or_default();
        let conn = session.conn.clone();
        // 结果上限在**发起时**读一次（同 `execute_sql`）：脚本跑多久都不影响它。
        let row_cap = query_row_cap(cx);
        crate::file_dialogs::open_file(
            cx,
            tr(cx, "选择要执行的 SQL 文件"),
            move |path, this, cx| this.start_sql_file(path, session_id, conn, row_cap, cx),
        );
    }

    /// 把选好的脚本跑起来（[`Self::run_sql_file`] 的后半段，文件已经定了）。
    ///
    /// 三样共享量（停止标志 / 进度快照 / 收工标志）走 `Arc`：后台线程拿不到
    /// `Context`，跟主线程通信只能靠它们（与 SFTP 传输那条路同一口径）。
    fn start_sql_file(
        &mut self,
        path: std::path::PathBuf,
        session_id: String,
        conn: Arc<Mutex<Connection>>,
        row_cap: usize,
        cx: &mut Context<Self>,
    ) {
        // 取消句柄必须在开工前抓：跑起来之后连接锁在后台线程手里（`execute_sql` 同）。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let name: SharedString = path
            .file_name()
            .map_or_else(
                || path.display().to_string(),
                |file| file.to_string_lossy().into_owned(),
            )
            .into();
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(FileProgress::default()));
        let done = Arc::new(AtomicBool::new(false));
        self.sql_file = Some(SqlFileJob {
            name: name.clone(),
            stop: Arc::clone(&stop),
            now: FileProgress::default(),
        });
        self.status = trf(cx, "正在执行 {}…", &[&name]).into();
        cx.notify();

        self.poll_sql_file_progress(Arc::clone(&progress), Arc::clone(&done), cx);
        let work_conn = Arc::clone(&conn);
        let work_path = path;
        let worker_name = name.clone();
        let finish_conn = Arc::clone(&conn);
        self.spawn_db_guarded(
            cx,
            session_id.clone(),
            cancel,
            SQL_FILE_GUARD_TIMEOUT,
            move || -> Result<FileReport, SqlFileError> {
                let options = SqlFileOptions::new(row_cap);
                let mut guard = work_conn.lock().expect("连接锁不应中毒");
                let should_stop = || stop.load(AtomicOrdering::Relaxed);
                let mut report_progress = |snapshot: &FileProgress| {
                    if let Ok(mut slot) = progress.lock() {
                        *slot = *snapshot;
                    }
                };
                let result = execute_sql_file(
                    &mut guard,
                    &work_path,
                    &options,
                    &should_stop,
                    // 逐条回执不往界面上摆：几百上千条会把结果区与流水一起淹掉，
                    // 失败明细已经归总在 `FileReport::errors`（最多 32 条）。
                    &mut |_| {},
                    &mut report_progress,
                );
                // 收工标志要在返回**之前**置上：轮询任务靠它退出。
                done.store(true, AtomicOrdering::Relaxed);
                result
            },
            move |this, result, window, cx| {
                this.finish_sql_file(result, worker_name, session_id, finish_conn, window, cx);
            },
        );
    }

    /// 进度轮询：把后台的进度快照搬回主线程（后台拿不到 `Context`，推不进来）。
    ///
    /// 与 SFTP 传输同一条路：后台执行器的定时器 + `WeakEntity::update`。任务在下一次
    /// 心跳看到收工标志后退出，不会漏读最后一次快照。
    fn poll_sql_file_progress(
        &mut self,
        progress: Arc<Mutex<FileProgress>>,
        done: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SQL_FILE_PROGRESS_POLL).await;
                let snapshot = progress
                    .lock()
                    .map_or_else(|_| FileProgress::default(), |slot| *slot);
                let finished = done.load(AtomicOrdering::Relaxed);
                let updated = this.update(cx, |this, cx| {
                    if let Some(job) = this.sql_file.as_mut() {
                        job.now = snapshot;
                    }
                    cx.notify();
                });
                // 页面已经没了（切走 / 关窗）：没什么可刷的，收工。
                if updated.is_err() || finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// 文件执行收尾：清任务 → 报读数 → 落流水 → 刷新左栏对象树。
    ///
    /// 「刷新对象树」不是附赠的礼节：一份 dump 最常见的后果就是**多了几张表 / 几个索引**，
    /// 不刷新的话用户执行完看到的还是那张旧树，第一反应是「没执行成功」。
    fn finish_sql_file(
        &mut self,
        result: Result<FileReport, SqlFileError>,
        name: SharedString,
        session_id: String,
        conn: Arc<Mutex<Connection>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sql_file = None;
        cx.notify();
        // 文案里的数字先都落成 `String`：`trf` 要的是同一个类型的实参切片，
        // 混着 `&str` / `&String` / `&SharedString` 会推不出共同类型。
        let name_text = name.to_string();
        match result {
            Ok(report) => {
                let label = vec![name_text.clone()];
                let encoding = report.encoding.label().to_owned();
                let elapsed = report.elapsed_ms.to_string();
                let mut detail = trf(cx, "编码 {} · 耗时 {} ms", &[&encoding, &elapsed]);
                if let Some(first) = report.errors.first() {
                    let index = first.index.to_string();
                    detail.push_str(" · ");
                    detail.push_str(&trf(cx, "第 {} 条语句失败：{}", &[&index, &first.message]));
                }
                self.push_exec_log(report.all_ok(), &label, &detail, report.elapsed_ms, cx);
                let executed = report.executed.to_string();
                let failed = report.failed.to_string();
                let affected = report.affected.to_string();
                // 停止优先于失败：被停下来的那次里，「手上这条语句被中止」也会记成一条
                // 失败，但用户要的答案是「我停了它」，不是一个红叉。
                let message = if report.stopped || report.cancelled {
                    trf(
                        cx,
                        "{} 已停止：成功 {} 条 · 失败 {} 条",
                        &[&name_text, &executed, &failed],
                    )
                } else if report.failed > 0 {
                    trf(
                        cx,
                        "{} 执行完成：成功 {} 条 · 失败 {} 条",
                        &[&name_text, &executed, &failed],
                    )
                } else {
                    trf(
                        cx,
                        "{} 执行完成：成功 {} 条 · 影响 {} 行",
                        &[&name_text, &executed, &affected],
                    )
                };
                self.status = message.clone().into();
                if report.failed > 0 {
                    notify::error(message, window, cx);
                } else {
                    notify::success(message, window, cx);
                }
                // 建表 / 建索引一类的脚本跑完，左栏那份内省已经过期了。
                if report.executed > 0 {
                    self.reload_introspection(session_id, conn, cx);
                }
            }
            Err(error) => {
                let message = trf(cx, "{} 执行失败：{}", &[&name_text, &error.to_string()]);
                self.status = message.clone().into();
                notify::error(message, window, cx);
            }
        }
    }

    /// 重取当前命名空间的内省（文件执行收尾用）。
    ///
    /// 只记警告不报错：脚本已经把数据写进去了，这时因为「列触发器时引擎不认」而弹一个
    /// 失败，是把好消息说成坏事（口径与 [`Introspection::load`] 一致 —— 只有取目录失败
    /// 才算整次内省失败，而失败与否不影响脚本本身的结果）。
    fn reload_introspection(
        &mut self,
        session_id: String,
        conn: Arc<Mutex<Connection>>,
        cx: &mut Context<Self>,
    ) {
        let schema = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.schema.clone());
        self.spawn_db(
            cx,
            move || -> Result<Introspection, DbError> {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                Introspection::load(&mut guard, schema.as_deref())
            },
            move |this, result, _window, cx| match result {
                Ok(introspection) => {
                    if let Some(session) = this.sessions.get_mut(&session_id) {
                        introspection.install(session);
                    }
                    cx.notify();
                }
                Err(error) => tracing::warn!(%error, "sql file reload introspection failed"),
            },
        );
    }

    /// 「加载更多」：以**已加载行数**为 `offset` 再取一页，**追加**到已有结果集。
    ///
    /// 与「重跑整条查询」的区别：这里不重建视图（不换 `QueryView`、不清勾选 / 暂存改动），
    /// 只把新行接到尾部——滚动位置、已选行、列宽都原样保留。
    /// 走的是可取消 + 超时的读路径（与表页加载同一档）。
    pub(super) fn load_more_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let Some(sql) = self.query_sql.clone() else {
            return;
        };
        if !self.query_has_more {
            return;
        }
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
        // 与 `execute_sql` 同理：取消句柄必须在开工前抓。
        let cancel = conn.lock().expect("连接锁不应中毒").cancel_handle();
        let timeout = self.effective_timeout();
        let offset = self.query_loaded;
        // 与首屏同一条上限：否则「加载更多」的页会比首屏大 / 小一截。
        let row_cap = query_row_cap(cx);
        self.status = tr(cx, "正在加载更多…").into();
        cx.notify();
        self.spawn_db_guarded(
            cx,
            session_id,
            cancel,
            timeout,
            move || {
                let mut guard = conn.lock().expect("连接锁不应中毒");
                guard.execute_paged(&sql, offset, row_cap)
            },
            move |this, result, window, cx| match result {
                Ok(page) => this.append_query_page(page, cx),
                Err(error) => {
                    if matches!(error, DbError::Cancelled(_)) {
                        let text = this.cancelled_status(cx);
                        this.status = text.clone();
                        notify::info(text, window, cx);
                        return;
                    }
                    this.status = trf(cx, "加载更多失败：{}", &[&error.to_string()]).into();
                    notify::error(
                        trf(cx, "加载更多失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 把一页新结果**追加**到当前结果集尾部（不重建视图）。
    pub(super) fn append_query_page(&mut self, page: ResultSet, cx: &mut Context<Self>) {
        let added = page.row_count();
        let more = page.truncated;
        let Some(view) = self.query_result.as_mut() else {
            return;
        };
        let Some(existing) = view.result.as_mut() else {
            return;
        };
        // 列集合不变：追加行即可（列宽 / 勾选 / 暂存改动都按原下标继续有效）。
        existing.rows.extend(page.rows);
        existing.truncated = more;
        self.query_loaded += added as u64;
        self.query_has_more = more;
        self.status = if more {
            trf(
                cx,
                "已加载 {} 行，可继续加载",
                &[&self.query_loaded.to_string()],
            )
            .into()
        } else {
            trf(
                cx,
                "已加载 {} 行（已到末页）",
                &[&self.query_loaded.to_string()],
            )
            .into()
        };
        // 行追加完了：组件里的模型要跟着长（列集没变，所以列宽 / 横向位置原样保留
        // ——「加载更多」不该把人拽回最左列）。展示行序随后按新行数重推。
        self.sync_data_table(cx);
        cx.notify();
    }
}
