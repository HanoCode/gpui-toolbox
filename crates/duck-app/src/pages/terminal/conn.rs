//! 连接档案生命周期：端点推导、ssh 目标拼装、新建 / 编辑 / 克隆 / 删除、写锁与 `kv: ssh_profiles` 持久化。

use std::rc::Rc;

use super::*;

impl TerminalPage {
    /// 会话键 → 该会话所连的 `(主机, 端口)`；本地 / SFTP 会话返回 `None`。
    ///
    /// 归属口径与 [`Self::ensure_sessions`] 起会话时**必须一致**：主窗格的会话键就是
    /// 标签 id；分屏右窗格的键形如 `<id>#2`（[`data::split_session_key`]），连哪台机器
    /// 由 `split_owner` 决定。用户档案即真源 —— 内置本地 shell 没有档案，天然落空。
    ///
    /// 给「主机密钥告警」用（见 `terminal_canvas::poll_sessions`）：ssh 因主机密钥
    /// 退出时拿它去查 `known_hosts`，判断这台主机此前有没有记录。
    pub(crate) fn ssh_endpoint(&self, session_id: &str) -> Option<(String, u16)> {
        // 主窗格：会话键就是标签 id。
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == session_id) {
            return self.endpoint_of(tab.kind, &tab.id);
        }
        // 分屏右窗格：会话键 == `tab.split`，连哪台机器看 `split_owner`。
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.split.as_deref() == Some(session_id))?;
        let kind = tab.split_kind().unwrap_or(tab.kind);
        let owner = tab.split_owner.clone().unwrap_or_else(|| tab.id.clone());
        self.endpoint_of(kind, &owner)
    }

    /// 连接归属 + 类型 → `(主机, 端口)`；本地会话（没有档案）返回 `None`。
    pub(super) fn endpoint_of(&self, kind: TabKind, owner: &str) -> Option<(String, u16)> {
        if kind != TabKind::Ssh {
            return None;
        }
        let profile = self.profile(owner)?;
        let port = profile.port.trim().parse().unwrap_or(22);
        Some((profile.host.clone(), port))
    }

    /// 会话键 → **这一格**背后的档案（本地 shell / 分屏两侧 / 同主机第二条都算）。
    ///
    /// 与 [`Self::ssh_endpoint`] 同一套归属口径，差别只在这里要整个档案
    ///（用户名、端口原文）而不是 `(主机, 端口)`。
    fn profile_of_session(&self, session_key: &str) -> Option<&StoredProfile> {
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == session_key) {
            return self.profile(&tab.id);
        }
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.split.as_deref() == Some(session_key))?;
        let owner = tab.split_owner.clone().unwrap_or_else(|| tab.id.clone());
        self.profile(data::tab_owner(&owner))
    }

    /// 右键菜单「复制连接信息」用的 `user@host[:port]`。
    ///
    /// 端口 22 不写出来：`ssh host` 与 `ssh host:22` 对用户是两件事，前者更短，
    /// 也正好是他要粘进另一个终端的那一种写法。本地 shell 没有地址（`None` ⇒ 菜单项置灰）。
    pub(crate) fn address_for_session(&self, session_key: &str) -> Option<String> {
        let profile = self.profile_of_session(session_key)?;
        if profile.host.trim().is_empty() {
            return None;
        }
        let port = profile.port.trim();
        Some(if port.is_empty() || port == "22" {
            format!("{}@{}", profile.user, profile.host)
        } else {
            format!("{}@{}:{}", profile.user, profile.host, port)
        })
    }

    /// 把一段连接文本写进剪贴板并回执。
    pub(crate) fn copy_connection_text(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        notify::success(trf(cx, "已复制连接地址：{}", &[&text]), window, cx);
    }

    /// 一个连接的展示条目（**含内置的本地 shell**）。
    ///
    /// 工具条与右栏都要它；统一在这里兜底，免得每个调用点各写一遍 `map_or`。
    pub(super) fn connection_entry(&self, id: &str) -> Option<Connection> {
        // 先剥后缀（`/sftp`、`~n`）：传进来的多是标签 id，不是档案 id。
        let id = data::tab_owner(id);
        if id == data::LOCAL_SHELL_ID {
            return Some(data::local_shell_view());
        }
        self.profile(id).map(data::connection_view)
    }

    /// SSH 复用 socket 的目录（**不用 `/tmp`**：`/tmp` 是共享的，别人预先占住
    /// 同名 socket 就能劫持连接复用）。
    ///
    /// 放缓存目录而不是应用数据目录：后者在 macOS 下是 `~/Library/Application
    /// Support/...`，**路径带空格**，曾把 `-o ControlPath=…` 拆成两个词导致
    /// 连接直接失败（值本身已在 `duck-ssh` 层加引号兜底，但目录无空格是第一道
    /// 防线）。socket 是随时可重建的运行时产物，归缓存目录正合适。
    pub(super) fn ssh_socket_dir(&self) -> PathBuf {
        duck_platform::cache_dir().join("ssh")
    }

    /// 由连接档案构造 SSH 目标。
    ///
    /// 口令从档案行里解密（[`open_password`]），只在「明确选了密码认证」时读 ——
    /// 解密是纯本地运算，不碰任何系统服务，所以点连接不会再弹系统授权框。
    pub(crate) fn ssh_target(&self, id: &str) -> Option<(duck_ssh::Target, PathBuf)> {
        let profile = self.profile(id)?;
        let auth = AuthKind::from_value(&profile.auth);
        // ⚠️ 私钥**必须**跟着认证方式走，不能只看路径非空。
        //
        // `duck-ssh` 的 `common_argv` 是「有私钥就 `-i <key>` + `IdentitiesOnly=yes`」
        // （`target.rs:245-249`）。`IdentitiesOnly=yes` 会**同时**关掉两条路：
        // ssh-agent 里已加载的钥匙不再被递上去，而下方 `else if` 的
        // 「口令认证收窄」（`PubkeyAuthentication=no` +
        // `PreferredAuthentications=password,keyboard-interactive`）因为走的是
        // 互斥分支而**永不执行**。于是用户明明选了「密码」或「SSH Agent」，
        // 却只剩档案里那把残留的旧钥匙在试 —— 服务端只会回一句
        // `Permission denied (publickey,password)`，而口令提示根本不会出现。
        let identity = (auth == AuthKind::Key)
            .then(|| profile.key_file.trim())
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let password = (auth == AuthKind::Password)
            .then(|| open_password(profile))
            .flatten();
        // 端口转发随档案持久化：认不出方向的条目在还原时就被丢掉
        //（见 `data::forward_from_stored`），这里不再二次过滤。
        let forwards = profile
            .forwards
            .iter()
            .filter_map(data::forward_from_stored)
            .collect();
        // 跳板机走 `-o ProxyJump=…`（`Target::options` 是给 ssh 透传 `-o` 的口子）。
        //
        // 放在 `options` 而不是给 `Target` 加一个专用字段：`options` 已经被
        // `connection_key` 逐条哈希进 `ControlPath`，改跳板机**自动**换新 socket ——
        // 加字段就得记着同步哈希，漏一次的表现是「改了跳板机，5 分钟后才生效」。
        let options: Vec<String> = data::proxy_jump_option(&profile.proxy_jump)
            .into_iter()
            .collect();
        Some((
            duck_ssh::Target {
                host: profile.host.clone(),
                port: profile.port.trim().parse().unwrap_or(22),
                user: profile.user.clone(),
                identity,
                password,
                // ⚠️ 认证方式必须显式传下去 —— `duck-ssh` 的收窄只看它。
                // 只靠 `identity` / `password` 有没有值来推导，会在
                // 「选了密码但没勾记住口令」「选了公钥但还没填路径」这两种情况下
                // 退化成完全不收窄，ssh 把 agent 里的钥匙试光 →
                // `Permission denied (publickey,password)`，且等不到口令提示。
                auth: match auth {
                    AuthKind::Key => duck_ssh::AuthMethod::Key,
                    AuthKind::Agent => duck_ssh::AuthMethod::Agent,
                    AuthKind::Password => duck_ssh::AuthMethod::Password,
                },
                options,
                forwards,
                // agent 转发随档案走：它管的是「远端能不能用本机 agent」，
                // 与上面那个认证收窄**正交**，所以不走 `auth` 分支推导。
                agent_forwarding: profile.agent_forwarding,
            },
            self.ssh_socket_dir(),
        ))
    }

    /// 打开（或切到）一个连接的会话标签。
    ///
    /// 已开着的连接只切标签，不重复开：同一主机开两条会话是无意的重复连接。
    ///
    /// 这里**只建标签**，真正的连接由 `ensure_sessions` 在本帧起 `ssh -t`。
    /// 之所以不在这里直接开进程：那需要 `&mut Context` 起任务，而本函数已经
    /// 被一个 `view.update` 闭包包着；分两步走也让「点一下 = 一个标签」这件事
    /// 与「连接成败」解耦 —— 标签先出现，成败由画布上的 ssh 输出说话。
    pub(super) fn open_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 右栏跟着「最后碰过的连接」走：点开一条就在右栏看它的详情。
        if self.selected_profile.as_deref() != Some(id) {
            self.selected_profile = Some(id.to_owned());
            cx.notify();
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = index;
            cx.notify();
            return;
        }
        // 内置本地 shell：直接开一条本地终端标签。
        if id == data::LOCAL_SHELL_ID {
            self.new_local_tab(cx);
            return;
        }
        let Some(entry) = self.connection_entry(id) else {
            return;
        };
        // 能力探测：没装 ssh 时**当面说清楚**，而不是建一个必然失败的标签。
        if !duck_ssh::probe().terminal() {
            notify::warning(
                tr(
                    cx,
                    "未找到系统 ssh 命令，无法建立 SSH 连接。请先安装 OpenSSH 客户端。",
                ),
                window,
                cx,
            );
            return;
        }
        // 复用 socket 的目录要先建出来：`ControlPath` 指向不存在的目录时，
        // ssh 不会自己创建，直接报错退出。
        let _ = duck_platform::ensure_dir(&self.ssh_socket_dir());
        // 「最近用过」在**打开时**就记，不等连上：连不上的机器恰恰是最想再试一次的。
        data::remember_recent(cx, id);
        self.tabs.push(SessionTab {
            id: id.to_owned(),
            title: entry.label,
            // 副标题留空：等远端 shell 发 OSC 标题再填。
            subtitle: None,
            kind: TabKind::Ssh,
            // 标签先以「连接中」出现，真实状态由本帧起的会话在下一轮轮询里改写。
            status: LinkStatus::Connecting,
            split: None,
            split_owner: None,
            dormant: false,
        });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// 「直接连」：把一行 `user@host:port` 变成一条**临时**连接并打开。
    ///
    /// 这是「临时连一台」的成本下限 —— 不用先起名、选分组、挑认证方式。
    /// 认证一律走 `SSH Agent`（服务器上的 `authorized_keys` + 本机 agent 是绝大多数
    /// 人的实际形态），口令认证与私钥仍然要进完整表单填，弹窗底部那一行就是入口。
    pub(super) fn open_direct(&mut self, raw: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = data::parse_direct_target(raw) else {
            notify::warning(
                trf(
                    cx,
                    "「{}」不是有效地址，写成 user@host:port。",
                    &[raw.trim()],
                ),
                window,
                cx,
            );
            return;
        };
        // 端口是 22 时不写出来：`alice@web-01:22` 只是噪音。
        let label = if target.port == 22 {
            format!("{}@{}", target.user, target.host)
        } else {
            format!("{}@{}:{}", target.user, target.host, target.port)
        };
        // 同一台机器连第二次要**复用那一条**（与档案连接一致）：两条临时连接指向
        // 同一台机器时，用户没法从标签上分辨，只会以为多开了一个。
        let existing = self
            .ad_hoc
            .iter()
            .find(|(_, profile)| {
                profile.host == target.host
                    && profile.user == target.user
                    && profile.port.trim().parse::<u16>().ok() == Some(target.port)
            })
            .map(|(id, _)| id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                let id = self.next_ad_hoc_id();
                self.ad_hoc.insert(
                    id.clone(),
                    StoredProfile {
                        id: id.clone(),
                        label,
                        host: target.host,
                        port: target.port.to_string(),
                        user: target.user,
                        auth: AuthKind::Agent.value().to_owned(),
                        key_file: String::new(),
                        // 空分组：临时连接不进左栏的任何分组（它压根不在 `profiles` 里），
                        // 但右栏详情与编辑弹窗会读它，留空比编一个「临时」分组干净。
                        group: String::new(),
                        remember: false,
                        password: None,
                        forwards: Vec::new(),
                        agent_forwarding: false,
                        proxy_jump: String::new(),
                    },
                );
                id
            }
        };
        self.open_connection(&id, window, cx);
    }

    /// 下一个可用的临时连接 id（`direct:1`、`direct:2`…）。
    fn next_ad_hoc_id(&self) -> String {
        let mut n = 1_u32;
        loop {
            let id = format!("{}{n}", data::AD_HOC_PREFIX);
            if self.profile(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }

    /// 分组下拉的选项：`(分组 id, 分组显示名)`。
    ///
    /// 内置三档是产品骨架、永远在列；档案里出现过的自定义分组跟在后面
    /// （`id` = 名字 = 用户原文）。跟着连接档案走：一个自定义分组只要有连接
    /// 落在它名下，编辑/新建别的连接时就能选到它。
    pub(super) fn group_options(&self, cx: &App) -> Vec<(String, String)> {
        let lang = active_theme(cx).lang();
        let mut options: Vec<(String, String)> = data::GROUP_DEFS
            .iter()
            .map(|(id, label, _)| ((*id).to_owned(), t(lang, label).to_owned()))
            .collect();
        options.extend(
            data::custom_group_names(&self.profiles)
                .into_iter()
                .map(|name| (name.clone(), name)),
        );
        options
    }

    /// 打开「新建连接」弹窗。
    ///
    /// `pub(crate)`：命令面板的「新建连接」动作（`actions::ids::TERMINAL_NEW_CONNECTION`）
    /// 与左栏那个按钮走同一条路 —— 弹窗、校验、落库只有一份。
    pub(crate) fn open_new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = self.group_options(cx);
        let socket_dir = self.ssh_socket_dir();
        let view = self.view.clone();
        open_connection_dialog(
            window,
            cx,
            groups,
            socket_dir,
            None,
            move |draft, window, cx| {
                let _ = view.update(cx, |this, cx| this.create_connection(draft, window, cx));
            },
        );
    }

    /// 打开「编辑连接」弹窗（表单回填当前档案）。
    ///
    /// 与新建**共用同一个弹窗**：字段、校验、键盘行为完全一样，
    /// 分成两个只会让「新建能填的字段编辑时填不了」这类不一致悄悄长出来。
    pub(super) fn open_edit_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        let groups = self.group_options(cx);
        // 口令**解密回显**：编辑连接时用户该看得到自己存过什么（掩码显示），
        // 改个端口不用把口令重输一遍。回显要求「应用能无提示地读回口令」，
        // 这正是口令走本机加密保管箱而不是系统钥匙串的原因之一。
        let initial = NewConnection {
            label: profile.label.clone(),
            host: profile.host.clone(),
            port: profile.port.clone(),
            user: profile.user.clone(),
            auth: AuthKind::from_value(&profile.auth),
            key_file: profile.key_file.clone(),
            password: open_password(profile).unwrap_or_default(),
            group: profile.group.clone(),
            remember: profile.remember,
            // 编辑时不预勾「立即打开」：那会让人以为「保存 = 重连」，
            // 而重连与否由下方独立的「重新连接」决定。
            open_now: false,
            // 端口转发按档案回填；认不出方向的旧条目直接不出现在弹窗里
            //（它本来也不会生效 —— 连接时同样会被丢掉）。
            forwards: profile
                .forwards
                .iter()
                .filter_map(ForwardDraft::from_stored)
                .collect(),
            agent_forwarding: profile.agent_forwarding,
            proxy_jump: profile.proxy_jump.clone(),
        };
        let id = id.to_owned();
        let view = self.view.clone();
        let socket_dir = self.ssh_socket_dir();
        open_connection_dialog(
            window,
            cx,
            groups,
            socket_dir,
            Some(&initial),
            move |draft, window, cx| {
                let id = id.clone();
                let _ = view.update(cx, |this, cx| this.apply_edit(&id, draft, window, cx));
            },
        );
    }

    /// 把编辑结果写回档案。
    ///
    /// **id 不变**：它是会话标签、SFTP 面板、口令密文共同的锚点，
    /// 改名只改显示名，绝不换 id —— 换 id 等于把一条连接拆散重建。
    pub(super) fn apply_edit(
        &mut self,
        id: &str,
        draft: NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 旧目标要在**档案被改写之前**取，而且要在下面的 `iter_mut` 之前 ——
        // `ssh_target` 借的是整张档案表（只读），与 `iter_mut` 的可变借用不共存。
        // `ControlPath` 按连接参数哈希（`duck_ssh::control_path`）：改完再算就落到
        // 新 socket 上，旧 master 谁也找不到，只能等 `ControlPersist` 的 5 分钟自己过期。
        let old_target = self.ssh_target(id).map(|(target, _)| target);
        let socket_dir = self.ssh_socket_dir();
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        // 连接参数有没有变：变了就必须断掉重连 —— 旧会话连的是**旧参数**，
        // 留着会让用户对着一个「看起来改了其实没变」的会话发呆。
        // 口令也算参数：换了口令的旧会话是拿旧口令登进去的，留着同样误导。
        // （编辑弹窗回显旧口令，所以「没改口令」时两者相等，不会触发无谓重连。）
        let old_password = open_password(profile);
        let password_changed = old_password != draft_password_value(&draft);
        // 端口转发也是连接参数：转发是在 `ssh` 起进程时加的，
        // 改了转发不重连，旧会话上新增的隧道一条都不会生效。
        let forwards: Vec<data::StoredForward> =
            draft.forwards.iter().map(ForwardDraft::to_stored).collect();
        let forwards_changed = profile.forwards != forwards;
        // agent 转发同样是**起进程时**加的参数：翻了不重连，远端那条链路不会变
        //（旧会话仍是 `ForwardAgent` 默认关）—— 与 `forwards_changed` 同一个道理。
        let agent_forwarding_changed = profile.agent_forwarding != draft.agent_forwarding;
        // 跳板机同理：它是**起进程时**的路由参数，改了不重连，旧会话还径直连着
        //（而且旧 master 认的还是旧路由）。`ControlPath` 按 `options` 哈希，
        // 断掉旧 master 后新 socket 自然落到新路径上，不需要手工清缓存。
        let proxy_jump_changed = profile.proxy_jump.trim() != draft.proxy_jump.trim();
        // 私钥先按认证方式规整（非公钥 → 空串），比较与落库都用这一份。
        let key_file = stored_key_file(draft.auth, draft.key_file.clone());
        let reconnect_needed = profile.host != draft.host
            || profile.port != draft.port
            || profile.user != draft.user
            || profile.key_file != key_file
            || profile.auth != draft.auth.value()
            || password_changed
            || forwards_changed
            || agent_forwarding_changed
            || proxy_jump_changed;

        // 口令先落档案（密文；它只读 `draft`，下面的档案字段会被 move 走）。
        apply_password(profile, &draft);
        profile.label = draft.label.clone();
        profile.host = draft.host;
        profile.port = draft.port;
        profile.user = draft.user;
        profile.auth = draft.auth.value().to_owned();
        profile.key_file = key_file;
        profile.group = draft.group;
        profile.remember = draft.remember;
        profile.forwards = forwards;
        profile.agent_forwarding = draft.agent_forwarding;
        // 存原文（只削两侧空白）：ssh 语法原样保存，不解析、不重排。
        profile.proxy_jump = draft.proxy_jump.trim().to_owned();
        self.persist_profiles(cx);

        if reconnect_needed {
            // 摘掉旧会话：`ensure_sessions` 只看「表里有没有这个 id」，
            // 不摘掉它会认为「已经起过了」，改动就永远不会生效。
            let affected: Vec<String> = self
                .tabs
                .iter()
                .filter(|tab| tab.id == id || tab.id == format!("{id}/sftp"))
                .map(|tab| tab.id.clone())
                .collect();
            for tab_id in affected {
                self.sessions.remove(&tab_id);
            }
            // 旧 master 必须显式关：它按旧的连接参数建起来（可能带着旧转发、旧的
            // `ForwardAgent`），会一直占着本地端口直到 `ControlPersist` 到期。
            // 不关它的表现是「改了转发，5 分钟后才生效」——最难查的一类。
            if let Some(old) = old_target {
                duck_ssh::close_master(&old, &socket_dir);
            }
        }
        notify::success(tr(cx, "连接已更新。"), window, cx);
        cx.notify();
    }

    /// 克隆一条连接档案（右键菜单「克隆连接」）。
    ///
    /// 复制的是**已保存的配置**：主机、端口、用户、认证方式、端口转发全都照搬，只换
    /// id 与显示名（原名 +「副本」，重名再追加序号）。口令在档案里存的是**密文**，直接
    /// 搬密文 —— 不走「解密再加密」，少一条明文路径。
    ///
    /// 克隆出来的连接**不自动连**：派生一份配置通常是为了改参数（换端口、换用户），
    /// 连上反而碍事；要连它，列表里点一下就行。
    pub(super) fn clone_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.profile(id).cloned() else {
            return;
        };
        // 按**显示名**去重（`副本`、`副本 2`……），规则见 [`unique_clone_label`]。
        let label = unique_clone_label(&source.label, tr(cx, "副本"), |candidate| {
            self.profiles
                .iter()
                .any(|profile| profile.label == candidate)
        });
        // 展开的分组直接取档案上的 `group` 原文：自定义分组名同样是合法落点，
        // 不再需要「只在静态骨架里找得到才展开」那层限制（旧写法会把克隆出来的
        // 自定义分组条目折叠着藏起来，看着像没克隆成功）。
        let group = source.group.clone();
        let new_id = self.unique_connection_id(&label);
        self.profiles.push(StoredProfile {
            id: new_id,
            label: label.clone(),
            ..source
        });
        self.persist_profiles(cx);
        // 与新建同口径：分组缺省折叠，克隆出来的条目落进去看不见就等于「点了没反应」。
        self.expanded.insert(group);
        notify::success(trf(cx, "已克隆连接「{}」。", &[label.as_str()]), window, cx);
        cx.notify();
    }

    /// 落一条新建的连接档案；勾了「保存后立即打开」就顺手开一条会话。
    pub(super) fn create_connection(
        &mut self,
        draft: NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.unique_connection_id(&draft.label);
        // 档案里存**口令密文**（勾了「记住口令」且非空时）：弹窗收到的明文在这一跳
        // 加密进档案行（AES-256-GCM，SQLite 里 grep 不到明文），或就地丢弃
        //（未勾选 —— 连接时 ssh 在终端里问）。加密先行（它只读 `draft`，
        // 下面的 push 会把字段 move 走）。
        let password = draft_password_value(&draft)
            .as_deref()
            .map(duck_platform::vault::seal)
            .transpose()
            .ok()
            .flatten();
        self.profiles.push(StoredProfile {
            id: id.clone(),
            label: draft.label.clone(),
            host: draft.host,
            port: draft.port,
            user: draft.user,
            auth: draft.auth.value().to_owned(),
            key_file: stored_key_file(draft.auth, draft.key_file),
            group: draft.group.clone(),
            remember: draft.remember,
            password,
            forwards: draft.forwards.iter().map(ForwardDraft::to_stored).collect(),
            agent_forwarding: draft.agent_forwarding,
            proxy_jump: draft.proxy_jump.trim().to_owned(),
        });
        self.persist_profiles(cx);
        // 分组缺省折叠：新建的连接落进去却看不见，等于「点了没反应」——
        // 这是用户动作触发的展开，不属于「自动展开」。
        self.expanded.insert(draft.group);
        if draft.open_now {
            self.open_connection(&id, window, cx);
        }
        notify::success(
            trf(cx, "已创建连接「{}」。", &[draft.label.as_str()]),
            window,
            cx,
        );
        cx.notify();
    }

    /// 把档案表写回存储（**唯一落盘点**）。
    ///
    /// 所有增删改最后都汇集到这里：散落的 `kv_put` 迟早会漏掉一处，
    /// 而漏掉的那次就是「用户改完重启后改动没了」。
    pub(super) fn persist_profiles(&self, cx: &App) {
        data::save_profiles(cx, &self.profiles);
    }

    /// 删一条档案（**唯一入口**）：先弹确认框，确认后才真正动手。
    ///
    /// 删除是破坏性动作，不给「撤销」这种半吊子兜底，而是**删之前问一句**。
    /// 参数在进入确认的那一刻冻结进 [`PendingAction`]，确认期间不回读连接表 ——
    /// 否则用户删完一条、又点了确认，动到的就不是他看到的那个连接了。
    pub(super) fn delete_connection(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        let action = PendingAction::DeleteConnection {
            id: id.to_owned(),
            label: profile.label.clone(),
        };
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title = trf(cx, "删除连接「{}」？", &[action.label().as_str()]);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份给内层。
            let action_for_ok = action.clone();
            let view_for_ok = view.clone();
            let action_for_footer = action.clone();
            let view_for_footer = view.clone();
            let drag_for_content = drag.clone();
            let title_for_content = title.clone();

            dialog
                .w(px(CONFIRM_DIALOG_W))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
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
                                tr(cx, "档案与已保存的口令都会被移除，本机不会留下副本。"),
                                cx,
                            )),
                    ))
                })
                // ⚠️ 可见按钮必须显式挂 footer：`button_props` 只登记回调，
                // `Dialog::render` 只渲染 `.footer(...)`，不挂就是「只有一个 ✕」的空壳。
                .footer(dialog_footer(
                    "terminal-conn-confirm",
                    tr(cx, "删除"),
                    ButtonVariant::Destructive,
                    Rc::new(move |_, cx| {
                        let _ = view_for_footer.update(cx, |this, cx| {
                            this.run_pending(action_for_footer.clone(), cx);
                        });
                        true
                    }),
                    cx,
                ))
                // 键盘 ⏎ 走 base 的 `Confirm` action → 这里；鼠标点按钮走上面 footer。
                .on_ok(move |_, _window, cx| {
                    // 视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                    // 不让一个点不掉的确认框挂在屏幕上。
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), cx);
                    });
                    true
                })
        });
    }

    /// 把 [`PendingAction`] 分派到真正动手的方法（`*_now` 族）。
    pub(super) fn run_pending(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::DeleteConnection { id, .. } => self.delete_connection_now(&id, cx),
            PendingAction::RemoveRemote { path, name } => {
                let success = SharedString::from(trf(cx, "已删除 {}。", &[name.as_str()]));
                self.run_remote_mutation(RemoteMutation::Remove { path }, success, cx);
            }
        }
    }

    /// 确认后的实际删除。
    ///
    /// 顺序是「关标签 → 删档案 → 收口令」，反过来会在中间态留一个引用已删档案的死标签。
    pub(super) fn delete_connection_now(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.profile(id).is_none() {
            return;
        }
        // 档案删掉之后再也算不出它的 `ControlPath`（哈希的输入是档案字段），
        // 所以目标必须在删除前取好 —— 否则这条 master 会留到 `ControlPersist` 过期，
        // 期间 `ps` 里一直挂着一条指向已删主机的 `ssh -M`。
        let target = self.ssh_target(id).map(|(target, _)| target);
        let socket_dir = self.ssh_socket_dir();
        // 标签：连接本身 + 它的 SFTP 面板。倒序删，正序会因下标位移漏掉后面的。
        let doomed: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.id == id || tab.id == format!("{id}/sftp"))
            .map(|(index, _)| index)
            .collect();
        for index in doomed.into_iter().rev() {
            self.close_tab_at(index, cx);
        }
        self.profiles.retain(|profile| profile.id != id);
        if let Some(target) = target {
            duck_ssh::close_master(&target, &socket_dir);
        }
        // 口令密文随档案行一起消失，没有第二份副本要收。
        self.persist_profiles(cx);
        cx.notify();
    }

    /// 由显示名派生一个不与现有连接冲突的 id。
    pub(super) fn unique_connection_id(&self, label: &str) -> String {
        let slug: String = label
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim_matches('-');
        let base = if slug.is_empty() {
            "connection".to_owned()
        } else {
            slug.to_owned()
        };
        let mut candidate = base.clone();
        let mut suffix = 2;
        while self.profiles.iter().any(|profile| profile.id == candidate) {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }
}
