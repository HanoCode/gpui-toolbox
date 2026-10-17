//! 会话标签页：激活 / 关闭 / 分组折叠、拖放落点（标签与连接）、状态同步、本地标签与各会话键推导。

use super::*;

use duck_term::Encoding;

impl TerminalPage {
    /// 激活某个标签。
    ///
    /// **不再撤销其他标签的自动重连**。此前这里会调 `retain_reconnect_only`，
    /// 把「非当前标签」的重连计时全部清掉 —— 依据是「用户已经切走，后台不该还在
    /// 起 `ssh`」。这条依据在有了退避与上限之后不成立了：
    ///
    /// - 后台重连是**有界**的（次数上限 + 指数退避），不会形成风暴；
    /// - 它的目的恰恰是「切走的这段时间自己长回来」，一撤销就永远长不回来；
    /// - 断开的是**后台标签**，用户看不见，撤销之后也没有任何提示 ——
    ///   切回来看到的是一个停在「已断开」的标签，还得手动点一次重连。
    ///
    /// 轮询任务（`pages::terminal_canvas::start_terminal_poller`）本来就遍历
    /// **全部**会话，与哪个标签在前台无关，所以这里删掉之后后台确实会继续重连。
    pub(super) fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        // 唤醒：恢复出来的标签在被点之前不连（见 [`SessionTab::dormant`]）。
        // 放在「下标没变也执行」的位置：点当前标签同样是「我要连上」。
        //
        // ⚠️ 唤醒本身**也要** `notify`：会话是在 `render` 里由 `ensure_sessions` 起的，
        // 而点「当前这个休眠标签」不改 `active` —— 不重绘就永远轮不到那一帧，
        // 表现是「按钮点了没反应」。
        let woke = self
            .tabs
            .get_mut(index)
            .is_some_and(|tab| std::mem::replace(&mut tab.dormant, false));
        if self.active != index || woke {
            self.active = index;
            cx.notify();
        }
    }

    /// 按方向切标签（`⌃Tab` / `⌃⇧Tab` / `⌘⇧[` / `⌘⇧]`）。
    ///
    /// 环形走：在最后一个标签上按「下一个」回到第一个（iTerm / 浏览器一致）。
    /// 只切到「存在」的标签上，所以 `delta` 用取模而不是钳制。
    pub(super) fn step_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if len < 2 {
            return;
        }
        let len = len as isize;
        let next = (self.active as isize + delta).rem_euclid(len) as usize;
        self.activate(next, cx);
    }

    /// 切换某条标签的会话编码（标签右键菜单「编码」）。
    ///
    /// 作用域是**标签**而不是单条会话：分屏的两半同属一条标签，用户在这个菜单里
    /// 改的是「这条标签怎么显示」，两半一起切才符合预期。会话句柄里的
    /// `EncodingHandle` 是唯一状态 —— 这里只负责把新值写进去（热切，不重开会话）。
    /// 重连走 [`Self::reconnect_session`] 时会从旧会话读出编码沿用，不会回落到设置默认。
    pub(super) fn set_tab_encoding(
        &mut self,
        tab_id: &str,
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<String> = match self.tabs.iter().find(|tab| tab.id == tab_id) {
            Some(tab) => std::iter::once(tab.id.clone())
                .chain(tab.split.clone())
                .collect(),
            None => return,
        };
        for key in keys {
            if let Some(live) = self.sessions.get(&key) {
                live.set_encoding(encoding);
            }
        }
        cx.notify();
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
        // 记进「重开栈」：`⌘⇧Z` 用它把刚关掉的那个再拉回来（先进后出，封顶
        // `CLOSED_TAB_CAPACITY` —— 栈太长等于没有栈，用户不会往回翻第 10 个）。
        self.closed_tabs.push(ClosedTab {
            id: tab.id.clone(),
            title: tab.title.clone(),
            kind: tab.kind,
        });
        if self.closed_tabs.len() > CLOSED_TAB_CAPACITY {
            self.closed_tabs.remove(0);
        }
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

    /// `⌘⇧Z`：重开最近关掉的那个标签。
    ///
    /// 重开 = **重新建一条会话**（旧进程已经随标签销毁），所以状态从「连接中」起，
    /// 与点连接同一个起点。标签 id 尽量沿用原来的：沿用才能让左栏那一行重新高亮；
    /// 原 id 已被占用（比如同名连接又开了一条）时退化成 `~n` 副本。
    pub(super) fn reopen_last_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.closed_tabs.pop() else {
            notify::info(tr(cx, "没有可重开的标签。"), window, cx);
            return;
        };
        // 档案被删了就没得重开：说清楚，而不是开一条注定起不来的标签。
        if closed.kind != TabKind::Local && self.profile(&closed.id).is_none() {
            notify::warning(tr(cx, "这条连接的档案已被删除，无法重开。"), window, cx);
            return;
        }
        if closed.kind == TabKind::Sftp {
            let owner = data::tab_owner(&closed.id).to_owned();
            self.open_sftp_tab(&owner, window, cx);
            return;
        }
        let id = if self.tabs.iter().any(|tab| tab.id == closed.id) {
            // 原 id 被占了：找一个 `~n` 空位，语义与 `⌘T` 的第二条会话一致。
            let owner = data::tab_owner(&closed.id).to_owned();
            let mut n = 2_u32;
            loop {
                let candidate = data::duplicate_tab_id(&owner, n);
                if !self.tabs.iter().any(|tab| tab.id == candidate) {
                    break candidate;
                }
                n += 1;
                if n >= 100 {
                    notify::warning(tr(cx, "同一主机的会话已经开太多了。"), window, cx);
                    return;
                }
            }
        } else {
            closed.id.clone()
        };
        self.tabs.push(SessionTab {
            id,
            title: closed.title,
            subtitle: None,
            kind: closed.kind,
            status: LinkStatus::Connecting,
            split: None,
            split_owner: None,
            dormant: false,
        });
        self.active = self.tabs.len() - 1;
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
    ///
    /// `id` 可以是标签 id（带 `/sftp` 或 `~n` 后缀），这里统一剥到档案 id ——
    /// 这是**唯一**的剥离点，上层调用方不必各自记得剥。
    pub(super) fn profile(&self, id: &str) -> Option<&StoredProfile> {
        let id = data::tab_owner(id);
        // 临时连接（`user@host:port` 直接连出来的）只活在 `ad_hoc` 里：
        // 回落到这里，连接 / 重连 / 右栏详情 / SFTP 就全都「不必知道它是临时的」。
        self.profiles
            .iter()
            .find(|profile| profile.id == id)
            .or_else(|| self.ad_hoc.get(id))
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
    /// 没有会话时再看**这个连接有没有开过标签** —— 开过（会话还没补上 / 已经死掉）算
    /// 已断开，从没开过只是「未连接」。
    /// **不从存档读**，也不在点击时「乐观置为已连接」—— 那正是演示数据的做法。
    pub(super) fn link_status(&self, id: &str) -> LinkStatus {
        match self.sessions.get(id) {
            Some(live) if live.is_exited() => LinkStatus::Disconnected,
            Some(_) => LinkStatus::Connected,
            None if self.tabs.iter().any(|tab| tab.id == id) => LinkStatus::Disconnected,
            None => LinkStatus::Idle,
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

    /// `⌘T`：在**同一台主机**上再开一条会话；没有远程标签时退化成新开本地终端。
    ///
    /// 为什么不是「永远开本地终端」：用户在一台机器上连着，按 `⌘T` 想要的通常是
    /// 「这台机器上再开一个 shell」（iTerm / Terminal.app 都是这个语义），
    /// 而不是「回到本机」。真要本机有 `⌘⇧T`。
    pub(super) fn new_tab_for_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = self.active_tab().and_then(|tab| {
            (tab.kind == TabKind::Ssh).then(|| data::tab_owner(&tab.id).to_owned())
        });
        let Some(owner) = owner else {
            self.new_local_tab(cx);
            return;
        };
        self.open_second_session(&owner, window, cx);
    }

    /// 在某条连接上再开一条会话标签（`⌘T` 与标签右键「再开一条会话」共用）。
    ///
    /// 找第一个 `~n` 空位（`~2`、`~3`…）。上限 99 只是防呆：正常用不到。
    /// 档案被删了：退回本地终端，而不是开一条注定起不来的标签。
    pub(super) fn open_second_session(
        &mut self,
        owner: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 调用方传的可能是带 `~n` 后缀的标签 id（标签右键菜单）：
        // 统一剥到档案 id，`~n` 空位才从 2 起算。
        let owner = data::tab_owner(owner).to_owned();
        let Some(entry) = self.connection_entry(&owner) else {
            self.new_local_tab(cx);
            return;
        };
        let mut n = 2_u32;
        while n < 100 {
            let id = data::duplicate_tab_id(&owner, n);
            if !self.tabs.iter().any(|tab| tab.id == id) {
                let title = format!("{} ({n})", entry.label);
                self.tabs.push(SessionTab {
                    id,
                    title,
                    subtitle: None,
                    kind: TabKind::Ssh,
                    // 与「点连接」同一个起点：真实状态由下一轮轮询改写。
                    status: LinkStatus::Connecting,
                    split: None,
                    split_owner: None,
                    dormant: false,
                });
                self.active = self.tabs.len() - 1;
                cx.notify();
                return;
            }
            n += 1;
        }
        notify::warning(tr(cx, "同一主机的会话已经开太多了。"), window, cx);
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
                    subtitle: None,
                    kind: TabKind::Local,
                    status: LinkStatus::Connected,
                    split: None,
                    split_owner: None,
                    dormant: false,
                });
                break;
            }
            ordinal += 1;
        }
        self.active = self.tabs.len() - 1;
        cx.notify();
    }
}
