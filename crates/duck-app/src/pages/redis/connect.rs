//! 连接生命周期：新建 / 编辑 / 测试 / 激活 / 断开写锁与连接档案持久化。

use super::*;

impl RedisPage {
    /// 把阻塞调用丢进后台执行器，完成后回主线程跑 `done`。
    pub(super) fn spawn_redis<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { work() });
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

    /// 保存弹窗里的连接档案；返回 `false` 表示校验未过（弹窗保持打开）。
    pub(super) fn save_form(
        &mut self,
        form: &Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let cfg = match form.read(cx).to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(SharedString::from(message), window, cx);
                return false;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return false;
        }
        // 口令：非空才写凭据文件（编辑时留空 = 保持既有口令不变）。
        let password = EditForm::input_value(&form.read(cx).password, cx);
        if !password.is_empty()
            && let Err(error) = store_password(&cfg.id, &password)
        {
            notify::error(
                trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        // SSH 口令单独一条凭据记录；开关没开就删掉旧的，别留死凭据。
        if form.read(cx).ssh_enabled {
            let ssh_password = EditForm::input_value(&form.read(cx).ssh_password, cx);
            if !ssh_password.is_empty()
                && let Err(error) = duck_redis::config::store_ssh_password(&cfg.id, &ssh_password)
            {
                notify::error(
                    trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
        } else {
            let _ = duck_redis::config::delete_ssh_password(&cfg.id);
        }
        match self.connections.iter_mut().find(|item| item.id == cfg.id) {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
        self.persist_connections(cx);
        self.editing = None;
        // 档案里改的可能正是树参数（分隔符 / 深度 / 展开阈值），树要跟着重建。
        self.rebuild_views();
        notify::success(tr(cx, "连接已保存"), window, cx);
        cx.notify();
        true
    }

    /// 删除一条连接档案（连带凭据文件里的口令）。
    pub(super) fn delete_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connections.retain(|cfg| cfg.id != id);
        self.sessions.remove(id);
        let _ = delete_password(id);
        let _ = duck_redis::config::delete_ssh_password(id);
        // 标注也跟着走：连接都没了，那台服务端上的颜色 / 备注 / 收藏再也点不到，
        // 留着只会在盘上积一份永远用不上的记录。
        self.forget_connection_tags(id, cx);
        // 回收站同理，而且更贵——里面是 `DUMP` 载荷（最大可到十几 MB）。
        self.forget_connection_trash(id, cx);
        if self.active.as_deref() == Some(id) {
            self.active = None;
            self.keys.clear();
            self.detail = None;
            self.selected = None;
            self.rebuild_views();
        }
        self.persist_connections(cx);
        self.editing = None;
        notify::info(tr(cx, "连接已删除"), window, cx);
        cx.notify();
    }

    /// 打开「添加新服务器」弹窗。
    pub(super) fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| EditForm::new(window, cx));
        self.open_connection_dialog(form, window, cx);
    }

    /// 打开「编辑服务器」弹窗。
    pub(super) fn edit_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cfg) = self.connections.iter().find(|cfg| cfg.id == id).cloned() else {
            return;
        };
        let form = cx.new(|cx| EditForm::from_config(&cfg, window, cx));
        self.open_connection_dialog(form, window, cx);
    }

    /// 用弹窗里的**当前表单值**发一次真实建连 + `PING`，只报结果不建会话。
    pub(super) fn test_connection(
        &mut self,
        form: &Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = match form.read(cx).to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(SharedString::from(message), window, cx);
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return;
        }
        // 弹窗里填了口令就用它；编辑旧连接留空则用凭据文件里的既有口令。
        let typed = EditForm::input_value(&form.read(cx).password, cx);
        let password = if typed.is_empty() {
            load_password(&cfg.id).ok().flatten()
        } else {
            Some(typed)
        };
        self.spawn_redis(
            cx,
            move || -> Result<String, RedisError> {
                let mut conn = RedisConnection::connect(&cfg, password.as_deref())?;
                conn.ping()?;
                conn.server_version()
            },
            move |this, result, window, cx| match result {
                Ok(version) => {
                    notify::success(trf(cx, "连接成功（{}）", &[&version]), window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "连接失败：{}", &[&error.to_string()]), window, cx);
                    this.status = trf(cx, "连接失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 打开连接编辑弹窗（页签 / 底部动作与 zedis 的「添加新服务器」同构）。
    pub(super) fn open_connection_dialog(
        &mut self,
        form: Entity<EditForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_new = form.read(cx).id.is_none();
        let title = if is_new {
            tr(cx, "添加新服务器").to_owned()
        } else {
            tr(cx, "编辑服务器").to_owned()
        };
        self.editing = Some(form.clone());
        let view = self.view.clone();
        let drag = modal_drag(cx);

        // 键盘 ⏎ 与底部主按钮共用同一份保存判断（`ConfirmHandler` 口径）。
        let confirm: ConfirmHandler = {
            let form = form.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |this, cx| this.save_form(&form, window, cx))
                    .unwrap_or(true)
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let title = title.clone();
            let confirm = Rc::clone(&confirm);
            let form_for_content = form.clone();
            let drag_for_content = drag.clone();
            let is_new = form.read(cx).id.is_none();
            let ok_text = tr(cx, if is_new { "添加" } else { "保存" });
            let offset = modal_offset(&drag, cx);

            dialog
                .w(px(FORM_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                // 库默认还会在面板右上角自画一个 ✕（`Dialog::close_button` 缺省 true），
                // 关闭入口统一走 `modal_title_row`，这里必须关掉，否则出现两个关闭按钮。
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text)
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "redis-form-title",
                                &drag_for_content,
                                title.clone(),
                                cx,
                            ))
                            .child(render_form_tabs(&form_for_content, cx)),
                    ))
                })
                .on_ok(move |_, window, cx| confirm(window, cx))
                .footer(redis_form_footer(&form, &view, is_new, cx))
        });
    }

    pub(super) fn persist_connections(&self, cx: &App) {
        match serde_json::to_string(&self.connections) {
            Ok(json) => duck_store::kv_put(cx, KEY_CONNECTIONS, json),
            Err(error) => tracing::warn!(%error, "serialize redis connections failed"),
        }
    }

    // ------------------------------------------------------------------
    // 连接生命周期。
    // ------------------------------------------------------------------

    /// 连接一条档案（后台建连 + 版本 + ACL 探测）。
    pub(super) fn connect_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|cfg| cfg.id == id).cloned() else {
            return;
        };
        let Some(session) = self.sessions.get(id) else {
            // 完成回调要跨线程存活，`id` 必须先脱掉借用（`&str` 不能进 `'static` 闭包）。
            let id = id.to_owned();
            let password = load_password(&id).ok().flatten();
            self.status = trf(cx, "正在连接 {}…", &[&cfg.display_name()]).into();
            self.spawn_redis(
                cx,
                move || -> Result<(RedisConnection, String, AclProbe, ServerFeatures), RedisError> {
                    let mut conn = RedisConnection::connect(&cfg, password.as_deref())?;
                    let version = conn.server_version()?;
                    let acl = conn.acl_probe();
                    // 能力探测是**只读**的，失败也不能拦住连接（见 `probe_features` 的口径：
                    // 探测不出来时按「可尝试」处理，而不是把面板全灰掉）。
                    let features = conn
                        .probe_features()
                        .unwrap_or_else(|_| ServerFeatures::unprobed(Some(version.clone())));
                    Ok((conn, version, acl, features))
                },
                move |this, result, window, cx| match result {
                    Ok((conn, version, acl, features)) => {
                        let target = conn.target().to_owned();
                        this.sessions.insert(
                            id.clone(),
                            Session {
                                conn: Arc::new(Mutex::new(conn)),
                                version: version.clone(),
                                acl,
                                unlock_until: None,
                                features: Some(features),
                            },
                        );
                        this.active = Some(id.clone());
                        // 换了连接：面板数据与监控流都作废（旧服务端的数字挂在新连接上没有意义）。
                        // 状态栏探测同理：延迟 / 内存 / 库键数都是**这一台**的读数。
                        this.panel_data = PanelData::default();
                        this.probe = StatusProbe::default();
                        this.stop_monitor(cx);
                        this.status = trf(cx, "已连接 {}（{}）", &[&target, &version]).into();
                        notify::success(tr(cx, "连接成功"), window, cx);
                        // 连上就取一次键总数并扫第一页。
                        this.refresh_keys(cx);
                        // 再跑一次状态栏探测（延迟 / 内存 / 各库键数）。
                        this.probe_status(cx);
                    }
                    Err(error) => {
                        this.status = trf(cx, "连接失败：{}", &[&error.to_string()]).into();
                        notify::error(trf(cx, "连接失败：{}", &[&error.to_string()]), window, cx);
                    }
                },
            );
            return;
        };
        // 已连上：只切激活连接（会话保留，不重连）。
        let _ = session;
        self.activate(id, window, cx);
    }

    /// 切换激活连接并按新的档案重扫键空间。
    pub(super) fn activate(&mut self, id: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_deref() != Some(id) {
            self.active = Some(id.to_owned());
            self.keys.clear();
            self.detail = None;
            self.selected = None;
            self.scan_cursor = 0;
            self.scan_done = true;
            self.limit_announced = false;
            // 换了连接就是换了一个键空间：展开状态（按节点路径记的）全部作废，
            // 留着会以旧路径误展开新键空间里同名但不同层的节点。
            self.tree_overrides.clear();
            self.rebuild_views();
            // 面板数据与监控流同理作废：它们反映的是**另一台服务端**的现状。
            self.panel_data = PanelData::default();
            self.stop_monitor(cx);
        }
        if let Some(cfg) = self.active_config() {
            self.status = trf(cx, "当前连接：{}", &[&cfg.display_name()]).into();
        }
        cx.notify();
    }

    /// 断开激活连接。
    pub(super) fn disconnect_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.sessions.remove(&id);
        self.keys.clear();
        self.detail = None;
        self.selected = None;
        self.scan_done = true;
        self.rebuild_views();
        self.status = tr(cx, "已断开").into();
        notify::info(tr(cx, "已断开连接"), window, cx);
        cx.notify();
    }

    /// 生产档解锁（限时窗口，见设计文档 §4.4）。
    pub(super) fn unlock_writes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.unlock_until = Some(Instant::now() + Duration::from_secs(WRITE_UNLOCK_SECS));
        let minutes = WRITE_UNLOCK_SECS / 60;
        self.status = trf(cx, "写操作已解锁 {} 分钟", &[&minutes.to_string()]).into();
        notify::warning(
            trf(
                cx,
                "写操作已解锁 {} 分钟，到期自动上锁",
                &[&minutes.to_string()],
            ),
            window,
            cx,
        );
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 服务端观测（M1.4）：视图切换、能力探测、面板拉取、终端、实时监控。
    // ------------------------------------------------------------------

    /// 把列表里的一项回填到配置编辑框。
    pub(super) fn pick_config(
        &mut self,
        entry: &ConfigEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name: SharedString = entry.name().to_owned().into();
        let value: SharedString = entry.value().to_owned().into();
        self.config_name
            .update(cx, |input, cx| input.set_value(name, window, cx));
        self.config_value
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    /// 提交配置修改（`CONFIG SET`，受 `Capability::ConfigWrite` 约束）。
    pub(super) fn save_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.config_name.read(cx).value().trim().to_owned();
        let value = self.config_value.read(cx).value().to_string();
        if name.is_empty() {
            notify::warning(tr(cx, "请先填参数名（点列表中一项可回填）"), window, cx);
            return;
        }
        self.confirm_or_run(
            Capability::ConfigWrite,
            PendingAction::ConfigSet { name, value },
            window,
            cx,
        );
    }

    /// 真正写入配置项（`*_at` 族：调用方之外的入口也不能绕过闸门）。
    pub(super) fn set_config_at(
        &mut self,
        name: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(Capability::ConfigWrite) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let echo = name.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.config_set(&name, &value)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(trf(cx, "已修改配置 {}", &[&echo]), window, cx);
                    // 改完立刻重读：服务端可能对取值做了归一化（`1gb` → 字节数），
                    // 继续显示用户填的原值会让下一次编辑以错值为基础。
                    this.load_panel(window, cx);
                }
                Err(error) => notify::error(
                    trf(cx, "修改配置失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                ),
            },
        );
    }
}
