//! 会话标签页：激活 / 关闭 / 分组折叠、拖放落点（标签与连接）、状态同步、本地标签与各会话键推导。

use super::*;

impl TerminalPage {
    /// 激活某个标签。
    ///
    /// 切走时会**撤销别的会话的自动重连计时**：用户已经看向另一个标签，后台不该还在
    /// 按退避起 `ssh`。保留的只有新活动标签自己的会话（含它的分屏窗格）。
    pub(super) fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active != index {
            self.active = index;
            let keep: Vec<String> = self
                .tabs
                .get(index)
                .map(|tab| {
                    std::iter::once(tab.id.clone())
                        .chain(tab.split.clone())
                        .collect()
                })
                .unwrap_or_default();
            self.retain_reconnect_only(&keep);
            cx.notify();
        }
    }

    /// 关闭第 `index` 个标签；关的是当前标签时优先落到原位置的下一个。
    ///
    /// 会话表由这里统一维护（`pages::terminal_canvas` 的轮询发现子进程退出后也走它），
    /// 别在调用点各写一遍 —— 漏掉一次就是一个永远不会被回收的 shell。
    pub(crate) fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(index);
        // 会话随标签一起丢：`Session::drop` 给事件循环发 `Shutdown`，PTY master 关闭，
        // 子 shell 收到 `SIGHUP` —— 不会留下孤儿进程。
        self.sessions.remove(&tab.id);
        // 分屏的第二个窗格是**另一条会话**，同一条命令收掉它（漏一个就是一条
        // 永远留在后台的 shell）。
        if let Some(split) = &tab.split {
            self.sessions.remove(split);
            if self.focus_pane.as_deref() == Some(split.as_str()) {
                self.focus_pane = None;
            }
        }
        if self.focused_tab.as_deref() == Some(tab.id.as_str()) {
            self.focused_tab = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= index {
            self.active = self.active.saturating_sub(1).min(self.tabs.len() - 1);
        }
        cx.notify();
    }

    /// 关闭**当前活动标签**（`⌘W`）。
    ///
    /// 薄薄一层：`self.active` 就是「现在看得见的那个」，收尾全交给
    /// [`Self::close_tab_at`]（它负责会话回收与 `active` 指针的落点）。
    pub(crate) fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        self.close_tab_at(self.active, cx);
    }

    /// 拖拽在画布上松手（`on_drop`）：只有指针悬在右半屏投放区上才算并入。
    pub(super) fn finish_tab_drop(&mut self, drag: &TabDrag, cx: &mut Context<Self>) {
        self.dragging_tab = None;
        if !self.split_drop_hover {
            return;
        }
        self.split_drop_hover = false;
        self.merge_tab_into_split(&drag.tab_id, cx);
    }

    /// 连接拖拽松手（`on_drop`）：落进悬停的分组（拖回原组 = no-op）。
    ///
    /// 分组只是档案上的一个字段（`StoredProfile.group`），落组即去抖落盘，
    /// 不弹确认 —— 拖错了再拖回来就是，没有破坏性。
    pub(super) fn finish_conn_drop(&mut self, drag: &ConnDrag, cx: &mut Context<Self>) {
        self.dragging_conn = None;
        let Some(target) = self.conn_drop_hover.take() else {
            return;
        };
        if data::move_profile_to_group(&mut self.profiles, &drag.profile_id, &target) {
            // 落点分组一并展开：拖进一个折叠着的分组时，行会当场「消失」
            //（它确实在组里，只是组折着），看起来像连接被删了。
            self.expanded.insert(target);
            data::save_profiles(cx, &self.profiles);
            cx.notify();
        }
    }

    /// 关闭除第 `index` 个以外的全部标签（右键菜单「关闭其他标签页」）。
    ///
    /// 复用 [`Self::close_tab_at`] 的收尾逻辑：从后往前删，下标不会乱。
    pub(super) fn close_other_tabs(&mut self, index: usize, cx: &mut Context<Self>) {
        for i in (0..self.tabs.len()).filter(|&i| i != index).rev() {
            self.close_tab_at(i, cx);
        }
    }

    /// 关闭全部标签（右键菜单「关闭全部标签页」）。
    pub(super) fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        while !self.tabs.is_empty() {
            self.close_tab_at(self.tabs.len() - 1, cx);
        }
    }

    /// 清屏（工具条按钮）：作用于当前窗格。
    ///
    /// `pub(crate)`：画布层的 `Clear` 动作（`⌘K`）另有更精确的落点 ——
    /// 它就在某个窗格里派发，直接清那一个（见 `pages::terminal_canvas`）。
    pub(crate) fn clear_active(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.focused_session_key() {
            self.clear_session(&key, cx);
        }
    }

    /// 把 `ESC[2J` 写进某条会话的 PTY，让**应用自己**重画。
    ///
    /// 本地这条走 PTY 而不是去动网格：应用（vim、htop）自己维护着重画状态，
    /// 只清宿主看到的缓冲区会让两边内容对不上，下一帧它重画时就会露馅。
    ///
    /// 没有活动会话（无标签 / SFTP 标签没有 PTY 会话）时无事可做 —— 此前这里会置一个
    /// `cleared` 标志给「演示画布」隐藏静态行用；演示画布删除后它只写不读，一并删掉。
    pub(crate) fn clear_session(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(live) = self.sessions.get(key) {
            live.write(CLEAR_SCREEN);
            // 清屏的语义是「回到当下」：用户可能正翻着历史，清完还停在旧位置会很怪。
            if let Some(session) = live.session() {
                session.scroll_to_bottom();
            }
        }
        cx.notify();
    }

    /// 展开 / 折叠一个连接分组。
    pub(super) fn toggle_group(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// 激活标签的引用（越界时回落首个）。
    pub(crate) fn active_tab(&self) -> Option<&SessionTab> {
        self.tabs.get(self.active).or_else(|| self.tabs.first())
    }

    /// 按 id 找连接档案（内置本地 shell 不在档案表里，返回 `None`）。
    pub(super) fn profile(&self, id: &str) -> Option<&StoredProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// 工具条上「清屏」这类动作该落在哪条会话上。
    ///
    /// 分屏之后「当前会话」不再等于「当前标签」：以**最后拿到键盘焦点的窗格**为准，
    /// 记录不属于当前标签时（切过标签）回落到主窗格。
    pub(super) fn focused_session_key(&self) -> Option<String> {
        let tab = self.active_tab()?;
        if self.focus_pane.as_deref() == tab.split.as_deref() && tab.split.is_some() {
            return tab.split.clone();
        }
        Some(tab.id.clone())
    }

    /// 某个连接当前的会话状态。
    ///
    /// 事实来源只有 `sessions` 一张表：有会话且没退出 = 已连接；会话在但已退出 = 已断开；
    /// 没有会话 = 已断开（`ensure_sessions` 会在本帧把它补上）。
    /// **不从存档读**，也不在点击时「乐观置为已连接」—— 那正是演示数据的做法。
    pub(super) fn link_status(&self, id: &str) -> LinkStatus {
        match self.sessions.get(id) {
            Some(live) if live.is_exited() => LinkStatus::Disconnected,
            Some(_) => LinkStatus::Connected,
            None => LinkStatus::Disconnected,
        }
    }

    /// 左栏分组视图：档案派生 + 真实状态覆盖。
    ///
    /// 要 `cx` 只为一件事：翻译内置项（本地 shell）的名字；
    /// 用户档案的名字是数据，不翻译。
    pub(super) fn group_views(&self, cx: &App) -> Vec<Group> {
        let mut groups = data::group_views(&self.profiles, active_theme(cx).lang());
        for group in &mut groups {
            for item in &mut group.items {
                item.status = self.link_status(&item.id);
            }
        }
        groups
    }

    /// 把标签状态同步成真实会话状态。
    ///
    /// SFTP 标签没有自己的会话，状态**跟随它的 SSH 连接**（`<id>/sftp` 的前半段）——
    /// 父连接断了，文件面板自然也不可用。
    pub(super) fn sync_tab_status(&mut self) {
        // 先算好每一条标签的状态（这一步只读 `sessions`），再写回 `tabs`：
        // 在同一次循环里既可变借 `tabs` 又借 `self` 会被借用检查器拒掉。
        let statuses: Vec<LinkStatus> = self
            .tabs
            .iter()
            .map(|tab| {
                let owner = tab.id.strip_suffix("/sftp").unwrap_or(tab.id.as_str());
                self.link_status(owner)
            })
            .collect();
        for (tab, status) in self.tabs.iter_mut().zip(statuses) {
            tab.status = status;
        }
    }

    /// 新开一条本地终端标签。
    ///
    /// 第一个标签用**固定 id**（[`data::LOCAL_SHELL_ID`]），因为左栏的内置入口
    /// 点开的就是它 —— 两边同一个 id，左栏的选中态才跟得上。
    ///
    /// `pub(crate)`：画布里的 `⌘T`（`terminal_canvas::NewTab`）与命令面板都走这里，
    /// 与标签条上那个 `+` 共用同一份状态机。
    pub(crate) fn new_local_tab(&mut self, cx: &mut Context<Self>) {
        let mut ordinal = self
            .tabs
            .iter()
            .filter(|tab| tab.kind == TabKind::Local)
            .count();
        loop {
            let (id, title) = if ordinal == 0 {
                (
                    data::LOCAL_SHELL_ID.to_owned(),
                    tr(cx, "本地 shell").to_owned(),
                )
            } else {
                (
                    format!("{}-{}", data::LOCAL_SHELL_ID, ordinal + 1),
                    format!("{} {}", tr(cx, "本地 shell"), ordinal + 1),
                )
            };
            // 用户可能已经关掉了中间某个标签，所以 id 要一直试到空位为止。
            if !self.tabs.iter().any(|tab| tab.id == id) {
                self.tabs.push(SessionTab {
                    id,
                    title,
                    kind: TabKind::Local,
                    status: LinkStatus::Connected,
                    split: None,
                    split_owner: None,
                });
                break;
            }
            ordinal += 1;
        }
        self.active = self.tabs.len() - 1;
        cx.notify();
    }
}
