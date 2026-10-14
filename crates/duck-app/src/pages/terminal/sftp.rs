//! SFTP 面板：入口目标选择、目录列举与浏览、远端新建 / 重命名 / 删除（走 `PendingAction` 二次确认）。

use super::*;

impl TerminalPage {
    /// 「打开 SFTP 面板」该连的那条远端连接。
    ///
    /// 依次看两处：**当前标签**所属的连接（SFTP 标签取它的父连接），以及**上一次**
    /// 开过 SFTP 的连接。都取不到才返回 `None`（工具条上的图标此时置灰）。
    ///
    /// 此前只看当前标签，于是关掉 SFTP 面板后活动标签回落到本地 shell / 空态，
    /// 图标要么给本地 shell 开出一个永远列举不出东西的假面板，要么直接失灵 ——
    /// 「关了面板就再也打不开」就是这么来的。
    ///
    /// 判定本身在 [`resolve_sftp_owner`]（纯函数，单测直接钉「关了还能再开」）。
    pub(super) fn sftp_owner(&self) -> Option<String> {
        let active = self.active_tab().map(|tab| {
            tab.id
                .strip_suffix("/sftp")
                .unwrap_or(tab.id.as_str())
                .to_owned()
        });
        resolve_sftp_owner(active.as_deref(), self.last_sftp_owner.as_deref(), &|id| {
            // 内置的本地 shell 不算数：它不是一台远端主机（`connection_entry` 却会
            // 热情地给它返回一个内置条目，所以这里必须自己挡一道）；
            // 档案被删的连接同样不算数 —— 否则图标会指向一条查不到的记录。
            id != data::LOCAL_SHELL_ID && self.profile(id).is_some()
        })
    }

    /// 为一条连接开 SFTP 面板标签。
    ///
    /// 复用同一条物理连接（`ControlMaster`），所以**连接本身必须先立起来**：
    /// 用户点「SFTP 面板」的意图就是要连上它，这里顺手把 SSH 标签也开了，
    /// 而不是甩一句「请先连接」。
    ///
    /// **只对真实档案开**：内置的本地 shell 不是一台远端主机（`connection_entry`
    /// 会热情地给它返回一个内置条目），放它进来会得到一个永远列举不出东西的
    /// 假 SFTP 标签 —— 空面板比没有面板更让人困惑。调用点（工具条 / 左栏菜单 /
    /// 标签菜单 / 右栏）都已各自挡在前面，这里留一道是为了下一个调用点。
    pub(super) fn open_sftp_tab(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // SFTP 要 `ssh`（列举）与 `scp`（传输）两个命令都在。
        if !duck_ssh::probe().files() {
            notify::warning(
                tr(cx, "未找到系统 ssh / scp 命令，无法打开 SFTP 面板。"),
                window,
                cx,
            );
            return;
        }
        if connection_id == data::LOCAL_SHELL_ID {
            notify::warning(
                tr(cx, "本地 shell 没有远端文件可浏览，请先连接一台远程主机。"),
                window,
                cx,
            );
            return;
        }
        if self.profile(connection_id).is_none() {
            return;
        }
        if !self.tabs.iter().any(|tab| tab.id == connection_id) {
            self.open_connection(connection_id, window, cx);
        }
        let id = format!("{connection_id}/sftp");
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = index;
            cx.notify();
            return;
        }
        let Some(entry) = self.connection_entry(connection_id) else {
            return;
        };
        let title = trf(cx, "SFTP · {}", &[entry.label.as_str()]);
        self.tabs.push(SessionTab {
            id,
            title,
            kind: TabKind::Sftp,
            status: entry.status,
            split: None,
            split_owner: None,
        });
        self.active = self.tabs.len() - 1;
        // 记住这条连接：面板关掉之后，工具条那个图标还得知道该给谁开面板
        //（标签没了就只剩它记着）。
        self.last_sftp_owner = Some(connection_id.to_owned());
        cx.notify();
    }

    /// 给「当前该连的那台主机」打开 SFTP 面板（命令面板动作的落点）。
    ///
    /// 目标由 [`Self::sftp_owner`] 解析（当前标签 → 上次开过的连接，纯函数
    /// `resolve_sftp_owner`）；**不**回落成「只看当前标签」——那正是「面板关掉后
    /// 再也打不开」的老 bug。
    ///
    /// 工具条上那条路径是同一个函数：图标在目标为空时置灰，这里则明说原因
    ///（从命令面板触发时没有「灰按钮」可以表达这件事）。
    pub(crate) fn open_sftp_for_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.sftp_owner() else {
            notify::warning(tr(cx, "打开 SFTP 面板（需先连接远程主机）"), window, cx);
            return;
        };
        self.open_sftp_tab(&owner, window, cx);
    }

    /// 打开某一侧的「打开目录」弹窗。
    pub(super) fn open_directory(
        &mut self,
        side: Side,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.pane(side).path.clone();
        let view = self.view.clone();
        open_directory_dialog(window, cx, side, current, move |path, _, cx| {
            let _ = view.update(cx, |this, cx| this.navigate(side, path, cx));
        });
    }

    /// 跳到指定目录并重列举。
    pub(super) fn navigate(&mut self, side: Side, path: String, cx: &mut Context<Self>) {
        self.panes[side.index()].navigate(side, path);
        cx.notify();
    }

    /// 重新读取当前目录（本地面板重新读盘；远端面板重新列举）。
    pub(super) fn reload_pane(&mut self, side: Side, cx: &mut Context<Self>) {
        if side.is_local() {
            self.panes[side.index()].reload(side);
        } else {
            // 远端把 `loaded` 打回 false：渲染前的 `ensure_remote_listing` 会接手，
            // 而它走的是后台执行器 —— 一次列举要起一个 `ssh` 进程，不能在渲染里同步等。
            self.panes[side.index()].loaded = false;
        }
        cx.notify();
    }

    /// 当前 SFTP 标签对应的远端连接。
    ///
    /// 三个前提缺一不可：激活的是 SFTP 标签、它的父连接**真的连着**、档案还在。
    /// 父连接没立起来就没有可复用的通道 —— SFTP 不是独立协议，它跑在 SSH 之上。
    pub(super) fn sftp_target(&self) -> Option<RemoteHandle> {
        let tab = self.active_tab()?;
        if tab.kind != TabKind::Sftp {
            return None;
        }
        let owner = tab.id.strip_suffix("/sftp")?;
        if self.link_status(owner) != LinkStatus::Connected {
            return None;
        }
        let (target, socket_dir) = self.ssh_target(owner)?;
        Some(RemoteHandle { target, socket_dir })
    }

    /// 渲染前给激活的 SFTP 面板补一次远端列举。
    ///
    /// 列举要起 `ssh`（阻塞），所以丢给后台执行器，回来后再 `notify`；
    /// `remote_listing_inflight` 防重入 —— `render` 每帧都跑，没有它会每帧起一个进程。
    pub(crate) fn ensure_remote_listing(&mut self, cx: &mut Context<Self>) {
        if self.remote_listing_inflight {
            return;
        }
        let remote = Side::Remote.index();
        if self.panes[remote].loaded {
            return;
        }
        let Some(handle) = self.sftp_target() else {
            // 没有可用的远端连接：把说明摆正，也**清掉上一台机器的目录**——
            // 留着它比空白更糟，用户会以为正在看当前主机的文件。
            self.panes[remote].entries.clear();
            self.panes[remote].selected = None;
            self.panes[remote].notice = Some(data::Notice::Key(data::REMOTE_OFFLINE_NOTICE));
            return;
        };
        let path = self.panes[remote].path.clone();
        // 空路径（刚打开、还没连上）从远端主目录起步。`~` 由**远端脚本自己**展开
        //（`duck_ssh::list_dir` 的前导 `d=$1; case $d in '~') d=$HOME`）：我们不去猜
        // 一个绝对路径，`root` 与 `deploy` 的家目录不一样。
        // 列举回来时远端会把 `pwd -P` 的结果一并发回，面板随即换成绝对路径
        //（见 `apply_remote_listing`），所以 `~` 只在这第一次列举里出现。
        let path = if path.trim().is_empty() {
            "~".to_owned()
        } else {
            path
        };
        self.remote_listing_inflight = true;
        let request_path = path.clone();
        cx.spawn(async move |page, cx| {
            let result =
                cx.background_executor()
                    .spawn({
                        let handle = handle.clone();
                        let path = request_path.clone();
                        async move {
                            duck_ssh::list_dir(&handle.target, Some(&handle.socket_dir), &path)
                        }
                    })
                    .await;
            let _ = page.update(cx, |page, cx| {
                page.remote_listing_inflight = false;
                page.apply_remote_listing(request_path, result, cx);
            });
        })
        .detach();
    }

    /// 把一次远端列举的结果落到面板上。
    pub(super) fn apply_remote_listing(
        &mut self,
        path: String,
        result: Result<duck_ssh::RemoteListing, duck_ssh::SshError>,
        cx: &mut Context<Self>,
    ) {
        let remote = Side::Remote.index();
        self.panes[remote].selected = None;
        // 成功时把面板的当前路径换成远端报回的**绝对**目录（请求路径多半是 `~`）：
        // 面包屑、`join`、`mkdir`、`rename`、`scp` 之后全走绝对路径，不必每个远端
        // 操作各自再展开一次波浪号 —— 漏掉任何一处就是一条「远端命令失败」。
        // 远端没报（旧脚本 / 第一行被占）时保留请求路径，条目照常显示。
        // 失败时也保留请求路径：下一次列举还在同一处，用户能看着它改。
        self.panes[remote].path = match &result {
            Ok(listing) => listing.cwd.clone().unwrap_or(path),
            Err(_) => path,
        };
        match result {
            Ok(listing) => {
                let mut rows: Vec<DirEntry> = listing
                    .entries
                    .into_iter()
                    .map(|entry| DirEntry {
                        name: entry.name,
                        size: entry.size.map_or_else(|| "—".to_owned(), format_bytes),
                        modified: entry.modified.unwrap_or_else(|| "—".to_owned()),
                        dir: entry.dir,
                    })
                    .collect();
                data::sort_entries(&mut rows);
                self.panes[remote].entries = rows;
                // 截断了就说一句：不说的话用户会以为目录里就这么多东西。
                self.panes[remote].notice = listing
                    .truncated
                    .then_some(data::Notice::Key(data::REMOTE_TRUNCATED_NOTICE));
            }
            Err(error) => {
                self.panes[remote].entries.clear();
                // ssh 的原文（`Permission denied`、`No such file or directory`）
                // 直接给用户看：它比任何我们转述的说法都准确，也指得出下一步。
                // 只有外壳文案是我们自己的，那一层要跟随语言。
                self.panes[remote].notice = Some(data::Notice::Text(ssh_error_text(cx, &error)));
            }
        }
        self.panes[remote].loaded = true;
        cx.notify();
    }

    /// 点击文件行：单击选中，双击目录进入、双击文件先给个说明。
    pub(super) fn click_entry(
        &mut self,
        side: Side,
        name: String,
        double: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !double {
            self.panes[side.index()].selected = Some(name);
            cx.notify();
            return;
        }
        let is_dir = self
            .pane(side)
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.dir);
        match is_dir {
            Some(true) => {
                let path = data::join(&self.pane(side).path, &name);
                self.navigate(side, path, cx);
            }
            // 双击文件：
            // - 本地面板 → 在访达里显示（`open -R`），这是「看一眼这个文件」最直接的落地；
            // - 远端面板 → 下载到本地面板的当前目录（复用同一条传输通道）。
            Some(false) => {
                if side.is_local() {
                    let path = PathBuf::from(&self.pane(Side::Local).path).join(&name);
                    // 平台层不返回结果：`open -R` 失败没什么可补救的
                    //（文件被删了 / 没有 Finder），弹一个错误框反而是噪音。
                    duck_platform::reveal_in_finder(&path);
                } else {
                    self.panes[Side::Remote.index()].selected = Some(name);
                    self.queue_transfer(false, window, cx);
                }
            }
            None => {}
        }
    }

    /// 远端条目在当前目录下的完整路径。远端目录还没列举出来时是 `None` ——
    /// 拿空目录拼出来的路径 `mkdir /xxx` 会落在根上，比拒绝更糟。
    pub(super) fn remote_entry_path(&self, name: &str) -> Option<String> {
        let dir = self.pane(Side::Remote).path.clone();
        if dir.trim().is_empty() {
            return None;
        }
        Some(data::join(&dir, name))
    }

    /// 打开「新建远端目录」弹窗。
    pub(super) fn new_remote_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane(Side::Remote).path.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }
        let view = self.view.clone();
        open_entry_name_dialog(
            window,
            cx,
            NameDialogKind::NewFolder,
            String::new(),
            move |name, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    let Some(path) = this.remote_entry_path(&name) else {
                        return;
                    };
                    let success = SharedString::from(trf(cx, "已创建目录 {}。", &[name.as_str()]));
                    this.run_remote_mutation(RemoteMutation::Mkdir { path }, success, cx);
                });
            },
        );
    }

    /// 打开「重命名远端条目」弹窗。
    ///
    /// 命名弹窗**本身就是这次改动的确认**：它同时显示旧名（标题/说明）与新名，
    /// 用户敲下确认才动手 —— 比「先弹一句『确定重命名吗』再弹一个输入框」少一跳。
    pub(super) fn rename_remote_entry(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.remote_entry_path(&name) else {
            notify::warning(tr(cx, "远端连接尚未建立，无法操作。"), window, cx);
            return;
        };
        let dir = self.pane(Side::Remote).path.clone();
        let view = self.view.clone();
        open_entry_name_dialog(
            window,
            cx,
            NameDialogKind::Rename,
            name.clone(),
            move |new_name, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    if new_name == name {
                        return;
                    }
                    let to = data::join(&dir, &new_name);
                    let success =
                        SharedString::from(trf(cx, "已重命名为 {}", &[new_name.as_str()]));
                    this.run_remote_mutation(
                        RemoteMutation::Rename {
                            from: from.clone(),
                            to,
                        },
                        success,
                        cx,
                    );
                });
            },
        );
    }

    /// 删除远端条目（**唯一入口**）：先弹确认框，确认后才真的 `rm`。
    pub(super) fn delete_remote_entry(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.remote_entry_path(&name) else {
            notify::warning(tr(cx, "远端连接尚未建立，无法操作。"), window, cx);
            return;
        };
        let action = PendingAction::RemoveRemote { path, name };
        self.confirm_entry_action(action, window, cx);
    }

    /// 远端破坏性动作的确认框（删除）。
    ///
    /// 与「删除连接」同一个形态：参数冻结进 [`PendingAction`]，确认期间不回读列表 ——
    /// 否则用户删完一条又点了确认，动到的不是他看到的那个。
    pub(super) fn confirm_entry_action(
        &mut self,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title = trf(cx, "删除「{}」？", &[action.label().as_str()]);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            let action_for_ok = action.clone();
            let view_for_ok = view.clone();
            let drag_for_content = drag.clone();
            let title_for_content = title.clone();

            dialog
                .w(px(CONFIRM_DIALOG_W))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "删除"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    let title = title_for_content.clone();
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "terminal-confirm-title",
                                &drag_for_content,
                                title,
                                cx,
                            ))
                            .child(body_text(
                                tr(cx, "该操作会直接在远端服务器上执行，不可撤销。"),
                                cx,
                            )),
                    ))
                })
                .on_ok(move |_, _window, cx| {
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), cx);
                    });
                    true
                })
        });
    }

    /// 在后台执行器上跑一次远端改动，回来后重列举远端面板并推通知。
    pub(super) fn run_remote_mutation(
        &mut self,
        mutation: RemoteMutation,
        success: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.sftp_target() else {
            self.panes[Side::Remote.index()].notice =
                Some(data::Notice::Key(data::REMOTE_OFFLINE_NOTICE));
            cx.notify();
            return;
        };
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let handle = handle.clone();
                    let mutation = mutation.clone();
                    async move {
                        match mutation {
                            RemoteMutation::Mkdir { path } => {
                                duck_ssh::mkdir(&handle.target, Some(&handle.socket_dir), &path)
                            }
                            RemoteMutation::Remove { path } => {
                                duck_ssh::remove(&handle.target, Some(&handle.socket_dir), &path)
                            }
                            RemoteMutation::Rename { from, to } => duck_ssh::rename(
                                &handle.target,
                                Some(&handle.socket_dir),
                                &from,
                                &to,
                            ),
                        }
                    }
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                // 成功与失败都重列举：成功要看到新条目，失败可能是「目录已被别人改过」。
                page.reload_pane(Side::Remote, cx);
                match result {
                    Ok(()) => crate::update::on_notify_window(cx, move |window, cx| {
                        notify::success(success.clone(), window, cx);
                    }),
                    Err(error) => {
                        // ssh 的原文指得出下一步（权限？目录非空？路径不对？）。
                        let reason = ssh_error_text(cx, &error);
                        crate::update::on_notify_window(cx, move |window, cx| {
                            notify::error(reason, window, cx);
                        });
                    }
                }
            });
        })
        .detach();
    }
}
