//! 终端页的数据模型与连接档案存储。
//!
//! 本模块**不依赖任何视图代码**，它给出 `duck-ssh` 的界面契约：
//! 连接档案（[`Connection`] / [`Group`] / 持久化形态 [`StoredProfile`]）、
//! 会话标签（[`SessionTab`]）、文件条目（[`DirEntry`]）与目录模型
//!（[`listing`] / [`parent`] / [`join`] / [`crumbs`]）。
//!
//! # 数据来源：**全部真实，没有演示数据**
//!
//! | 区域 | 来源 |
//! | --- | --- |
//! | 连接列表 | `kv: ssh_profiles` 的 JSON 数组（`duck.sqlite3`，口令在其中以 AES-256-GCM 密文存在）；用户新建 / 编辑 / 删除即写回 |
//! | 会话标签 | 用户真实打开的会话（首次进入是空的，不给预置标签） |
//! | 本地面板 | 真读磁盘（`std::fs`，只读、有上限、失败如实回报） |
//! | 远端面板 | 只有真正建起 SFTP 会话才有内容；未连接时如实说明 |
//! | 传输队列 | 真队列，没任务就是空的 |
//!
//! 首次启动连接列表是空的 —— 界面给空态引导，**不用编造的连接填满列表**：
//! 假数据会让「点开没反应」看起来像 bug，也会掩盖「内核还没接上」这个事实。

use std::fs;
use std::io::ErrorKind;
use std::time::{SystemTime, UNIX_EPOCH};

use duck_components::DuckIcon;
use duck_theme::{Lang, active_theme, t};
use gpui::{App, Hsla};
use serde::{Deserialize, Serialize};

use crate::pages::status_support::format_bytes;

/// 单次列举上限：文件列表尚未虚拟化，避免超大目录（`/usr/bin`、`node_modules`）
/// 一次性建出几千个元素卡住主线程。
pub(crate) const LIST_LIMIT: usize = 400;

/// 连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkStatus {
    /// 已连接。
    Connected,
    /// 连接中。
    Connecting,
    /// 已断开。
    Disconnected,
}

impl LinkStatus {
    /// 状态文案（i18n 源串）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Connected => "已连接",
            Self::Connecting => "连接中",
            Self::Disconnected => "已断开",
        }
    }

    /// 状态点颜色，取任务状态 token（与工具箱状态栏同一套语义色）。
    pub(crate) fn color(self, cx: &App) -> Hsla {
        let colors = active_theme(cx).extended().colors;
        match self {
            Self::Connected => colors.status_completed,
            Self::Connecting => colors.status_downloading,
            Self::Disconnected => colors.status_paused,
        }
    }
}

/// 会话类型：决定标签图标与内容区形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabKind {
    /// 本地终端。
    Local,
    /// 远程终端（SSH）。
    Ssh,
    /// 文件传输（SFTP）。
    Sftp,
}

impl TabKind {
    /// 标签图标。
    pub(crate) const fn icon(self) -> DuckIcon {
        match self {
            Self::Local => DuckIcon::SquareTerminal,
            Self::Ssh => DuckIcon::Server,
            Self::Sftp => DuckIcon::Folders,
        }
    }

    /// 会话类型文案（右栏详情用，i18n 源串）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Local => "本地终端",
            Self::Ssh => "SSH 会话",
            Self::Sftp => "SFTP 文件管理",
        }
    }
}

/// 一个打开的会话标签。
pub(crate) struct SessionTab {
    /// 稳定标识：本地终端用 `local-<序号>`，远程用连接 id，SFTP 用 `<连接 id>/sftp`。
    pub(crate) id: String,
    /// 标签名（i18n 源串或连接显示名）。
    pub(crate) title: String,
    /// 会话类型。
    pub(crate) kind: TabKind,
    /// 连接状态。
    pub(crate) status: LinkStatus,
    /// 分屏：右半窗格的会话键（[`split_session_key`] 生成），`None` = 单窗格。
    ///
    /// 分屏不是「同一块画布画两遍」—— 它**是第二条会话**（另一个 shell / 另一个
    /// `ssh -t`），所以这里存的是会话键而不是一个布局开关：会话表里有没有这一条，
    /// 决定窗格里有没有东西可画。
    pub(crate) split: Option<String>,
    /// 分屏右窗格连接到哪台机器：连接 id 或内置的 [`LOCAL_SHELL_ID`]。
    ///
    /// `None` = 按旧口径回落「标签自己的连接」（拖拽并入时也可能没有别的选择）。
    /// 分屏窗格不再钉死在标签自己的连接上：用户可以把另一台主机拖进来，
    /// 或在连接右键菜单里选「在分屏中打开」。与 `split` **同生共死**，
    /// 两处只在 [`Self::set_split`] / 清分屏的地方一起改。
    pub(crate) split_owner: Option<String>,
}

impl SessionTab {
    /// 清掉分屏（会话的回收由调用方负责，这里只动标签数据）。
    pub(crate) fn clear_split(&mut self) {
        self.split = None;
        self.split_owner = None;
    }

    /// 分屏右窗格的会话类型：跟随**连接归属**而不是标签自己的类型 ——
    /// 「本地标签 + 远端分屏」与「SSH 标签 + 本地分屏」都合法。
    ///
    /// `split` 为 `None` 时返回 `None`；`split_owner` 缺失时回落标签自己的类型
    ///（旧口径的兼容：那时窗格连的就是标签自己的连接）。
    pub(crate) fn split_kind(&self) -> Option<TabKind> {
        self.split.as_ref()?;
        Some(match self.split_owner.as_deref() {
            None => self.kind,
            Some(LOCAL_SHELL_ID) => TabKind::Local,
            Some(_) => TabKind::Ssh,
        })
    }
}

/// 分屏窗格的连接归属：本地标签归内置本地 shell，SSH 标签归它自己的连接 id。
///
/// 本地标签的 id（`local-shell`、`local-shell-2`…）是**标签序号**不是连接 id，
/// 直接拿去查档案会落空 —— 一律映射到 [`LOCAL_SHELL_ID`]。
pub(crate) fn split_owner_for(kind: TabKind, tab_id: &str) -> String {
    if kind == TabKind::Local {
        LOCAL_SHELL_ID.to_owned()
    } else {
        tab_id.to_owned()
    }
}

/// 分屏右窗格的会话键。
///
/// 以 `#2` 结尾（而不是 `/2`）：SFTP 标签用的是 `<id>/sftp`，两种后缀一眼分得开；
/// 且 `#` 不在 `duck_platform::secret` 的键名白名单里，谁也不会误当档案 id 去查连接。
pub(crate) fn split_session_key(tab_id: &str) -> String {
    format!("{tab_id}#2")
}

/// 认证方式：决定新建连接弹窗里展开哪一组字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKind {
    /// 公钥。
    Key,
    /// ssh-agent 转发的密钥。
    Agent,
    /// 口令（不落配置文件，交本机加密保管箱）。
    Password,
}

impl AuthKind {
    /// 下拉项：`(值, 显示文案)`。
    pub(crate) const ALL: [(Self, &'static str, &'static str); 3] = [
        (Self::Key, "key", "公钥"),
        (Self::Agent, "agent", "SSH Agent"),
        (Self::Password, "password", "密码"),
    ];

    /// 存储值。
    pub(crate) const fn value(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Agent => "agent",
            Self::Password => "password",
        }
    }

    /// 由存储值还原；未知值回落公钥（与「公钥优先」的产品排序一致）。
    pub(crate) fn from_value(value: &str) -> Self {
        Self::ALL
            .iter()
            .find(|(_, candidate, _)| *candidate == value)
            .map_or(Self::Key, |(kind, _, _)| *kind)
    }

    /// 认证方式文案（右栏详情用）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Key => "公钥",
            Self::Agent => "SSH Agent",
            Self::Password => "密码",
        }
    }
}

/// 一条连接档案（运行时形态，附带会话状态）。
pub(crate) struct Connection {
    /// 连接标识（持久化档案的主键）。
    pub(crate) id: String,
    /// 显示名。
    pub(crate) label: String,
    /// 主机地址。
    pub(crate) host: String,
    /// 端口。
    pub(crate) port: String,
    /// 用户名。
    pub(crate) user: String,
    /// 认证方式。
    pub(crate) auth: AuthKind,
    /// 认证细节：公钥路径、口令是否已加密保存、或 `ssh-agent`。右栏详情直接显示。
    pub(crate) auth_detail: String,
    /// 连接状态。
    pub(crate) status: LinkStatus,
    /// 是否为内置项（本地 shell）。内置项不可编辑 / 删除。
    pub(crate) builtin: bool,
}

impl Connection {
    /// `用户@主机`（列表行右侧的元信息）。
    pub(crate) fn endpoint(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// 连接档案的**持久化形态**（`kv: ssh_profiles`，JSON 数组，落 `duck.sqlite3`）。
///
/// 口令以**密文**存在档案行里（[`Self::password`]，`duck_platform::vault` 的
/// AES-256-GCM 产物）——SQLite 里 grep 不到明文，编辑弹窗能解密回显，
/// 连接时能解密交给 askpass。所有字段带 `#[serde(default)]`，
/// 旧存档缺字段时按缺省值读，不会因为少一个键就整份读不出来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct StoredProfile {
    /// 主键。
    #[serde(default)]
    pub(crate) id: String,
    /// 显示名。
    #[serde(default)]
    pub(crate) label: String,
    /// 主机地址。
    #[serde(default)]
    pub(crate) host: String,
    /// 端口。
    #[serde(default)]
    pub(crate) port: String,
    /// 用户名。
    #[serde(default)]
    pub(crate) user: String,
    /// 认证方式（[`AuthKind::value`]）。
    #[serde(default)]
    pub(crate) auth: String,
    /// 私钥路径。
    #[serde(default)]
    pub(crate) key_file: String,
    /// 落点分组 id。
    #[serde(default)]
    pub(crate) group: String,
    /// 是否记住口令（决定连接参数里带不带口令）。
    #[serde(default)]
    pub(crate) remember: bool,
    /// 口令密文（[`duck_platform::vault::seal`] 的产出）；`None` = 未保存口令。
    ///
    /// 密文里的值**只有本机能解**（密钥在 `app_data_dir()/vault.key`，0600，
    /// 不随数据导出走）——把数据库文件拷到别的机器读不出任何口令。
    #[serde(default)]
    pub(crate) password: Option<String>,
    /// 端口转发（`-L` / `-R` / `-D`），按用户添加的顺序。
    #[serde(default)]
    pub(crate) forwards: Vec<StoredForward>,
    /// 是否把本机 ssh-agent 转发给远端（OpenSSH `ForwardAgent`）。
    ///
    /// 与 [`Self::auth`] 是**两件事**：`auth` 决定「本机→远端这一跳怎么证明身份」，
    /// 本项决定「**远端**能不能用本机 agent 里的密钥」（从跳板机再连第三台机器时用）。
    /// 所以它与认证方式正交，任何一种认证都能开。
    #[serde(default)]
    pub(crate) agent_forwarding: bool,
}

/// 一条端口转发的**持久化形态**（随连接档案落 `kv: ssh_profiles`）。
///
/// 端口一律存字符串而不是 `u16`：旧存档 / 手改过的 JSON 里可能是空串或
/// `"abc"`，用 `u16` 会让**整份档案读不出来**（一个字段坏掉 = 用户的连接列表
/// 集体消失）。解析失败在这里退化成「这一条转发不生效」，与其他字段同一套容错。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredForward {
    /// 方向（[`duck_ssh::ForwardKind::value`]）：`local` / `remote` / `dynamic`。
    #[serde(default)]
    pub(crate) kind: String,
    /// 绑定地址（留空用 ssh 默认）。
    #[serde(default)]
    pub(crate) bind: String,
    /// 绑定 / SOCKS 端口。
    #[serde(default)]
    pub(crate) bind_port: String,
    /// 目标地址（动态转发不用）。
    #[serde(default)]
    pub(crate) host: String,
    /// 目标端口（动态转发不用）。
    #[serde(default)]
    pub(crate) host_port: String,
}

/// 由持久化形态还原成内核的转发配置；方向认不出、端口解析不出时返回 `None`
///（丢掉这一条，**不猜**成某一种方向）。
#[must_use]
pub(crate) fn forward_from_stored(stored: &StoredForward) -> Option<duck_ssh::PortForward> {
    let kind = duck_ssh::ForwardKind::from_value(&stored.kind)?;
    Some(duck_ssh::PortForward {
        kind,
        bind: stored.bind.trim().to_owned(),
        bind_port: stored.bind_port.trim().parse().unwrap_or(0),
        host: stored.host.trim().to_owned(),
        host_port: stored.host_port.trim().parse().unwrap_or(0),
    })
}

/// 连接档案的存储键。
pub(crate) const KEY_PROFILES: &str = "ssh_profiles";

/// 内置本地 shell 的固定 id（不是用户档案）。
pub(crate) const LOCAL_SHELL_ID: &str = "local-shell";

/// 内置本地 shell 的名字（**i18n 源串**）。
///
/// 单独提出来是因为它是「界面文案被塞进数据字段」的唯一一处：档案名由用户自己取、
/// 不翻译，只有内置项要走查表（见 [`group_views`]）。
pub(crate) const LOCAL_SHELL_LABEL: &str = "本地 shell";

/// 连接分组的**内置**骨架。
///
/// 内置三档固定（用户档案按 `group` 字段落位，缺省「生产环境」）；在此之外，
/// 档案里出现过的其他 `group` 值会派生成**自定义分组**（见 [`group_views`]）——
/// 分组名是用户数据不是文案，原样显示、不进 i18n。
pub(crate) const GROUP_DEFS: [(&str, &str, DuckIcon); 3] = [
    ("prod", "生产环境", DuckIcon::Layers),
    ("staging", "测试环境", DuckIcon::Layers),
    ("local", "本地", DuckIcon::HardDrive),
];

/// 读出全部连接档案；没装存储或从没写过时是**空表**（不编造默认连接）。
pub(crate) fn load_profiles(cx: &App) -> Vec<StoredProfile> {
    duck_store::kv_get(cx, KEY_PROFILES)
        .and_then(|json| serde_json::from_str::<Vec<StoredProfile>>(&json).ok())
        .unwrap_or_default()
}

/// 写回全部连接档案（去抖落盘）。
pub(crate) fn save_profiles(cx: &App, profiles: &[StoredProfile]) {
    match serde_json::to_string(profiles) {
        Ok(json) => duck_store::kv_put(cx, KEY_PROFILES, json),
        // 序列化失败只可能是存储层坏了；不写半截 JSON 覆盖掉用户的档案。
        Err(error) => tracing::error!(%error, "ssh profiles serialization failed"),
    }
}

/// 把一条档案移进指定分组（左栏「拖连接换分组」的落点）。
///
/// 找不到档案或本就在目标分组时返回 `false`（无事发生，界面也不用重绘）；
/// 返回 `true` 表示确有迁移，调用方负责落盘。
pub(crate) fn move_profile_to_group(profiles: &mut [StoredProfile], id: &str, group: &str) -> bool {
    let Some(profile) = profiles.iter_mut().find(|profile| profile.id == id) else {
        return false;
    };
    if profile.group == group {
        return false;
    }
    profile.group = group.to_owned();
    true
}

/// 由持久化档案派生认证细节文案（i18n 源串）。
fn auth_detail(profile: &StoredProfile) -> String {
    match AuthKind::from_value(&profile.auth) {
        AuthKind::Key => profile.key_file.clone(),
        AuthKind::Agent => "ssh-agent".to_owned(),
        AuthKind::Password if profile.password.is_some() => "口令已保存".to_owned(),
        AuthKind::Password => "口令每次输入".to_owned(),
    }
}

/// 一个连接分组。
pub(crate) struct Group {
    /// 分组标识（折叠态与新建连接的落点都用它）。
    ///
    /// 自定义分组没有静态 id：**分组名即 id**（用户数据不翻译，名字就是身份）。
    pub(crate) id: String,
    /// 分组显示名（已就绪，调用方直接用；内置组在这里完成 i18n）。
    pub(crate) label: String,
    /// 分组图标。
    pub(crate) icon: DuckIcon,
    /// 组内连接。
    pub(crate) items: Vec<Connection>,
}

/// 一条档案的展示形态。
///
/// 状态一律从 [`LinkStatus::Disconnected`] 起 —— 状态是**运行时事实**，
/// 由真实会话驱动（见 `TerminalPage::link_status`），存档里也没有这一项。
#[must_use]
pub(crate) fn connection_view(profile: &StoredProfile) -> Connection {
    Connection {
        id: profile.id.clone(),
        label: profile.label.clone(),
        host: profile.host.clone(),
        port: profile.port.clone(),
        user: profile.user.clone(),
        auth: AuthKind::from_value(&profile.auth),
        auth_detail: auth_detail(profile),
        status: LinkStatus::Disconnected,
        builtin: false,
    }
}

/// 内置本地 shell 的展示形态。
///
/// 它不是用户档案，是**打开本地终端的那扇门**：因此 `builtin = true`，
/// 界面据此禁止编辑 / 删除。
///
/// `label` 存的是 i18n **源串**；真正展示时由 [`group_views`] 换成本地语言。
#[must_use]
pub(crate) fn local_shell_view() -> Connection {
    Connection {
        id: LOCAL_SHELL_ID.to_owned(),
        label: LOCAL_SHELL_LABEL.to_owned(),
        host: "localhost".to_owned(),
        port: "—".to_owned(),
        user: std::env::var("USER").unwrap_or_else(|_| "localhost".to_owned()),
        auth: AuthKind::Agent,
        auth_detail: "ssh-agent".to_owned(),
        status: LinkStatus::Disconnected,
        builtin: true,
    }
}

/// 档案里出现过的**自定义**分组名（不在 [`GROUP_DEFS`] 内），按首次出现排序。
///
/// 分组的存在性由档案派生：一条连接都没有的自定义分组不留尸体 ——
/// 它本来就只因为「有连接落在这里」而存在。
#[must_use]
pub(crate) fn custom_group_names(profiles: &[StoredProfile]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for profile in profiles {
        if GROUP_DEFS.iter().all(|(id, _, _)| *id != profile.group)
            && !names.iter().any(|name| name == &profile.group)
        {
            names.push(profile.group.clone());
        }
    }
    names
}

/// 连接清单视图：内置分组骨架 + 自定义分组 + 真实档案。
///
/// **入参就是真源**（页面的 `profiles` 字段）——本函数只做「贴标签 + 塞内置项」，
/// 不碰存储、不碰状态。
///
/// 内置组的名字走 i18n；自定义分组的名字是用户数据（`profile.group` 原文），
/// 跟着语言变才是 bug —— 与连接名同一套口径。
#[must_use]
pub(crate) fn group_views(profiles: &[StoredProfile], lang: Lang) -> Vec<Group> {
    let mut views: Vec<Group> = GROUP_DEFS
        .into_iter()
        .map(|(id, label, icon)| {
            let mut items: Vec<Connection> = profiles
                .iter()
                .filter(|profile| profile.group == id)
                .map(connection_view)
                .collect();
            // 「本地」分组永远有一条内置的本地 shell 入口。
            if id == "local" {
                let mut shell = local_shell_view();
                shell.label = t(lang, LOCAL_SHELL_LABEL).to_owned();
                items.insert(0, shell);
            }
            Group {
                id: id.to_owned(),
                label: t(lang, label).to_owned(),
                icon,
                items,
            }
        })
        .collect();
    // 自定义分组排在内置组后面：内置三档是产品骨架，位置不因用户数据漂移。
    for name in custom_group_names(profiles) {
        views.push(Group {
            items: profiles
                .iter()
                .filter(|profile| profile.group == name)
                .map(connection_view)
                .collect(),
            id: name.clone(),
            label: name,
            icon: DuckIcon::Layers,
        });
    }
    views
}

/// SFTP / 目录浏览的面板侧别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// 本地面板（读真实磁盘）。
    Local,
    /// 远端面板（内容由 `terminal.rs` 走真实 `ssh` 列举后回填）。
    Remote,
}

impl Side {
    /// 数组下标。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Remote => 1,
        }
    }

    /// 面板标题（i18n 源串）。
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Local => "本地",
            Self::Remote => "远端",
        }
    }

    /// 是否为本地侧（决定「选择目录」按钮与提示文案）。
    pub(crate) const fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }

    /// 主目录。本地取 `$HOME`；远端**返回空串**。
    ///
    /// 远端家目录只有远端自己知道（`root` 与 `deploy` 不是同一个），我们不猜：
    /// 空串表示「远端目录尚未确定」，由列举那条路在远端展开 `~`
    ///（`duck_ssh::list_dir` 的前导脚本），远端再把 `pwd -P` 的结果报回来，
    /// 面板随即换成绝对路径（见 `TerminalPage::apply_remote_listing`）——
    /// 此前的注释写的是「由 `Session::cwd()` 上报」，代码里从来没有这个上报。
    /// 空串同时也是所有远端写操作的前置检查（见 `run_remote_mutation` 的调用点）：
    /// 路径没确定就不该拿一个猜的目录去 `mkdir` / `rm`。
    pub(crate) fn home(self) -> String {
        match self {
            Self::Local => std::env::var("HOME").unwrap_or_else(|_| "/".to_owned()),
            Self::Remote => String::new(),
        }
    }

    /// 打开时的初始目录（两侧都从主目录起）。
    pub(crate) fn initial(self) -> String {
        self.home()
    }
}

/// 一次目录列举的结果。
pub(crate) struct Listing {
    /// 条目（目录在前、名称升序）。
    pub(crate) entries: Vec<DirEntry>,
    /// 需要告知用户的说明（读失败的原因 / 未连接 / 已截断）；正常时为 `None`。
    pub(crate) notice: Option<Notice>,
}

/// 需要告知用户的一件事。
///
/// 分成两支是因为**两者来源不同**：静态文案要查 i18n 表，而远端命令的 stderr
///（`Permission denied`、`No such file or directory`）本身就是给用户看的话 ——
/// 把它塞进 i18n 表只会得到一条永远查不到的 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    /// 静态文案的 i18n 源串。
    Key(&'static str),
    /// 运行时消息（远端原样返回），已经是人话。
    Text(String),
}

impl Notice {
    /// 展示用的文案。
    pub(crate) fn render(&self, cx: &App) -> String {
        match self {
            Self::Key(key) => duck_theme::tr(cx, key).to_owned(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// 文件的展示条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEntry {
    /// 文件名。
    pub(crate) name: String,
    /// 大小（已格式化；目录为 `—`）。
    pub(crate) size: String,
    /// 修改时间（`MM-DD HH:MM`）。
    pub(crate) modified: String,
    /// 是否目录（决定图标与双击行为）。
    pub(crate) dir: bool,
}

/// 远端面板未建立会话时的统一说明（i18n 源串）。
pub(crate) const REMOTE_OFFLINE_NOTICE: &str = "尚未建立 SFTP 会话，无法浏览远端目录。";

/// 远端列举被截断时的说明（i18n 源串）。
pub(crate) const REMOTE_TRUNCATED_NOTICE: &str = "远端输出过长，列表已截断。";

/// 列出某个侧别、某个路径下的条目。
///
/// 远端分支只是一条**兜底**：真正的内容由 `TerminalPage` 走 `duck_ssh::list_dir`
/// 取（要起 `ssh`，属于页面层的异步活儿）。这里返回「未连接」，
/// 免得任何一条漏网的调用路径显示出一片空白、让人误以为目录是空的。
pub(crate) fn listing(side: Side, path: &str) -> Listing {
    match side {
        Side::Local => local_listing(path),
        // 远端由 `TerminalPage` 走真实列举（要起 `ssh`，属于页面层的异步活儿）。
        // 这里兜底成「未连接」，免得任何一条漏网的调用路径显示出一片空白。
        Side::Remote => Listing {
            entries: Vec::new(),
            notice: Some(Notice::Key(REMOTE_OFFLINE_NOTICE)),
        },
    }
}

/// 本地面板：真读磁盘（只读）。失败与截断都如实回报，**不静默编数据**。
fn local_listing(path: &str) -> Listing {
    let dir = normalize(path);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(error) => {
            return Listing {
                entries: Vec::new(),
                notice: Some(Notice::Key(read_notice(&error))),
            };
        }
    };

    let mut entries = Vec::new();
    let mut truncated = false;
    for item in read.flatten() {
        let Some(entry) = local_entry(&item) else {
            continue;
        };
        if entries.len() == LIST_LIMIT {
            truncated = true;
            break;
        }
        entries.push(entry);
    }
    sort_entries(&mut entries);
    Listing {
        entries,
        notice: truncated.then_some(Notice::Key("仅显示前 400 项。")),
    }
}

/// 把一条 `DirEntry` 转成展示条目；元信息取不到时跳过（权限异常的文件）。
fn local_entry(item: &fs::DirEntry) -> Option<DirEntry> {
    let metadata = item.metadata().ok()?;
    let dir = metadata.is_dir();
    Some(DirEntry {
        name: item.file_name().to_string_lossy().into_owned(),
        size: if dir {
            "—".to_owned()
        } else {
            format_bytes(metadata.len())
        },
        modified: metadata
            .modified()
            .ok()
            .map_or_else(|| "—".to_owned(), format_modified),
        dir,
    })
}

/// 读目录失败的原因（i18n 源串）。
fn read_notice(error: &std::io::Error) -> &'static str {
    match error.kind() {
        ErrorKind::NotFound => "目录不存在。",
        ErrorKind::PermissionDenied => "没有权限读取该目录。",
        _ => "无法读取该目录。",
    }
}

/// 排序：目录在前，同类按名称升序（大小写不敏感，与 Finder 一致）。
pub(crate) fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 规范化路径：空白视作根、折叠重复分隔符、去掉末尾斜杠（根保留 `/`）。
pub(crate) fn normalize(path: &str) -> String {
    let trimmed = path.trim();
    let parts: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return "/".to_owned();
    }
    let joined = parts.join("/");
    if trimmed.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// 父目录；已在根返回 `None`（「上一级」按钮据此禁用）。
pub(crate) fn parent(path: &str) -> Option<String> {
    let path = normalize(path);
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        // `/bws` 的父级是根，`rfind` 落在 0 上。
        Some(0) | None => Some("/".to_owned()),
        Some(index) => Some(path[..index].to_owned()),
    }
}

/// 拼接子项。
pub(crate) fn join(base: &str, name: &str) -> String {
    let base = normalize(base);
    if base == "/" {
        format!("/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// 面包屑：每段给出 `(累计路径, 显示名)`，首段是根。
pub(crate) fn crumbs(path: &str) -> Vec<(String, String)> {
    let path = normalize(path);
    let mut out = vec![("/".to_owned(), "/".to_owned())];
    let mut acc = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push((acc.clone(), part.to_owned()));
    }
    out
}

/// `MM-DD HH:MM`（UTC）。
///
/// 实现已下沉到 `duck-theme` 的格式化层（[`duck_theme::datetime_short`]）——本仓原先
/// 在 `mail_data` 与本文件各有一份同源的日期换算，`civil_from_days` 更被抄了 7 份。
/// 「**不引 `chrono`/`time`**」是本仓的依赖准入结论，所以仍是 UTC；时区换算是记在
/// `docs/email-design.md` 里的未决项。
///
/// 量不出时间（早于 `UNIX_EPOCH`，或恰好落在纪元零点）时给 `—`：远端列表的
/// 「修改时间」一栏留白会被误读成「还没加载出来」。
fn format_modified(time: SystemTime) -> String {
    let Ok(elapsed) = time.duration_since(UNIX_EPOCH) else {
        return "—".to_owned();
    };
    let short = duck_theme::datetime_short(elapsed.as_secs() as i64);
    if short.is_empty() {
        "—".to_owned()
    } else {
        short
    }
}

/// 传输队列的一行。
pub(crate) struct Transfer {
    /// 唯一标识：队列行的 ElementId 与**结果回填**都靠它。
    ///
    /// 不用文件名当键 —— 传 `a.txt` 到两个目录是两次独立的传输，
    /// 用文件名会把它们混成一条，先回来的那次会把后一次的状态覆盖掉。
    pub(crate) id: u64,
    /// 文件名。
    pub(crate) name: String,
    /// 进度（0..=1）。
    ///
    /// 由 `done / total` 算出来（`total` 未知时恒为 0，界面显示「不确定」）。
    /// 真字节数来自**轮询落点大小**：上传时问远端的 `wc -c` / `du -sk`，
    /// 下载时问本地文件的长度 —— `scp` 自己不报进度，但落点是单调增长的。
    pub(crate) progress: f32,
    /// 本次传输的总字节数；`None` = 量不到（分母未知），进度按「不确定」显示。
    pub(crate) total: Option<u64>,
    /// 已传输的字节数（最近一次轮询到的落点大小）。
    pub(crate) done: u64,
    /// 状态。
    pub(crate) state: TransferState,
    /// 是否上传（决定箭头方向）。
    pub(crate) upload: bool,
    /// 失败原因（`scp` 的 stderr 原文）；其余状态为 `None`。
    pub(crate) reason: Option<String>,
}

/// 一次传输的状态。
///
/// 流转路径（终态是 `Done` / `Cancelled` / `Failed`）：
///
/// ```text
/// Queued ──▶ Active ──▶ Done
///              │  └──▶ Failed ──(重试)──▶ Active
///              └──(取消)──▶ Cancelling ──▶ Cancelled
/// ```
///
/// `Cancelling` 是**真的在等进程退出**（`scp` 被 `kill` 后要几十毫秒才落地），
/// 不是装饰态：界面据此把「取消」按钮换成不可点的「取消中」，避免连点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferState {
    /// 已入队，还没开始。
    Queued,
    /// 正在传（进度轮询在跑）。
    Active,
    /// 已请求取消，等 `scp` 进程退出。
    Cancelling,
    /// 已取消（用户主动终止）。
    Cancelled,
    /// 传完。
    Done,
    /// 失败了（网络断、远端满、没权限），原因见 `Transfer::reason`。
    Failed,
}

impl TransferState {
    /// 状态文案（i18n 源串）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Active => "传输中",
            Self::Cancelling => "取消中",
            Self::Cancelled => "已取消",
            Self::Done => "已完成",
            Self::Failed => "失败",
        }
    }

    /// 是否在进行中（进度轮询 / 可取消）。
    pub(crate) const fn is_active(self) -> bool {
        matches!(self, Self::Active | Self::Cancelling)
    }

    /// 是否终态（不会再变）。
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Done | Self::Failed)
    }

    /// 一次状态迁移是否合法（**迁移表的唯一出处**，纯函数、可单测）。
    ///
    /// 合法路径：
    ///
    /// ```text
    /// Queued ─▶ Active ─▶ Done
    ///    │         ├──▶ Failed ─(重试)─▶ Queued
    ///    └──▶ Failed
    ///              └──(取消)──▶ Cancelling ─▶ Cancelled
    /// ```
    ///
    /// 恒等（`from == to`）一律算合法：重复落同一状态是无害的空操作，界面每帧也可能
    /// 重放同一个结论。**非法迁移只记一条 warn 仍照常应用**（见 `mark_transfer`）——
    /// 状态机在这里是「软校验」：宁可状态照常前进，也不让它卡死在一个不动的格子上。
    pub(crate) const fn can_transition_to(self, next: Self) -> bool {
        if matches!(
            (self, next),
            (Self::Queued, Self::Queued)
                | (Self::Active, Self::Active)
                | (Self::Cancelling, Self::Cancelling)
                | (Self::Cancelled, Self::Cancelled)
                | (Self::Done, Self::Done)
                | (Self::Failed, Self::Failed)
        ) {
            return true;
        }
        matches!(
            (self, next),
            (Self::Queued, Self::Active)
                | (Self::Queued, Self::Failed)
                | (Self::Active, Self::Done)
                | (Self::Active, Self::Failed)
                | (Self::Active, Self::Cancelling)
                | (Self::Cancelling, Self::Cancelled)
                | (Self::Failed, Self::Queued)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小标签（字段补齐就在这一处，测试只关心分屏三件套）。
    fn tab(id: &str, kind: TabKind) -> SessionTab {
        SessionTab {
            id: id.to_owned(),
            title: id.to_owned(),
            kind,
            status: LinkStatus::Disconnected,
            split: None,
            split_owner: None,
        }
    }

    #[test]
    fn split_kind_follows_owner_not_tab_kind() {
        // SSH 标签 + 本地分屏：右窗格是本地会话。
        let mut ssh = tab("prod-01", TabKind::Ssh);
        ssh.split = Some(split_session_key(&ssh.id));
        ssh.split_owner = Some(LOCAL_SHELL_ID.to_owned());
        assert_eq!(ssh.split_kind(), Some(TabKind::Local));

        // 本地标签 + 远端分屏：右窗格是 SSH 会话。
        let mut local = tab("local-shell", TabKind::Local);
        local.split = Some(split_session_key(&local.id));
        local.split_owner = Some("prod-01".to_owned());
        assert_eq!(local.split_kind(), Some(TabKind::Ssh));

        // 未记录归属（旧口径）回落标签自己的类型；没分屏就是 None。
        let mut legacy = tab("local-shell-2", TabKind::Local);
        legacy.split = Some(split_session_key(&legacy.id));
        assert_eq!(legacy.split_kind(), Some(TabKind::Local));
        assert_eq!(tab("prod-01", TabKind::Ssh).split_kind(), None);
    }

    #[test]
    fn split_owner_maps_local_tabs_to_the_builtin_shell() {
        // 本地标签的 id 是标签序号不是连接 id：映射到内置本地 shell 才查得到。
        assert_eq!(
            split_owner_for(TabKind::Local, "local-shell-3"),
            LOCAL_SHELL_ID
        );
        // SSH 标签的 id 本来就是连接 id，原样保留。
        assert_eq!(split_owner_for(TabKind::Ssh, "prod-01"), "prod-01");
    }

    #[test]
    fn clear_split_resets_both_fields() {
        let mut ssh = tab("prod-01", TabKind::Ssh);
        ssh.split = Some(split_session_key(&ssh.id));
        ssh.split_owner = Some(ssh.id.clone());
        ssh.clear_split();
        assert_eq!(ssh.split, None);
        assert_eq!(ssh.split_owner, None);
        assert_eq!(ssh.split_kind(), None);
    }

    #[test]
    fn normalize_collapses_separators() {
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("  "), "/");
        assert_eq!(normalize("/srv//app/"), "/srv/app");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("relative/path"), "relative/path");
    }

    #[test]
    fn parent_walks_up_to_root() {
        assert_eq!(parent("/srv/app"), Some("/srv".to_owned()));
        assert_eq!(parent("/srv"), Some("/".to_owned()));
        // 根没有父级：按钮据此禁用，不能回落成根（否则点「上一级」像没反应）。
        assert_eq!(parent("/"), None);
    }

    #[test]
    fn crumbs_are_cumulative() {
        assert_eq!(
            crumbs("/srv/app"),
            vec![
                ("/".to_owned(), "/".to_owned()),
                ("/srv".to_owned(), "srv".to_owned()),
                ("/srv/app".to_owned(), "app".to_owned()),
            ]
        );
        assert_eq!(crumbs("/"), vec![("/".to_owned(), "/".to_owned())]);
    }

    #[test]
    fn join_keeps_single_separator() {
        assert_eq!(join("/", "srv"), "/srv");
        assert_eq!(join("/srv/", "app"), "/srv/app");
    }

    #[test]
    fn modified_formats_epoch_offsets() {
        // 纪元零点 + 1 天 1 小时 1 分 → 1970-01-02 01:01。
        let time = UNIX_EPOCH + std::time::Duration::from_secs(86_400 + 3_661);
        assert_eq!(format_modified(time), "01-02 01:01");
        // 纪元零点本身（以及更早的时间）归到「量不出」档 —— 留白会被当成「还没加载」。
        assert_eq!(format_modified(UNIX_EPOCH), "—");
        assert_eq!(
            format_modified(UNIX_EPOCH - std::time::Duration::from_secs(1)),
            "—"
        );
    }

    #[test]
    fn remote_listing_is_honestly_offline() {
        // 远端面板在会话建立前必须是**空的 + 有说明**：端出编造的目录树
        // 会让「上传 / 下载」看起来像坏了，也掩盖「内核还没接上」这个事实。
        let listing = listing(Side::Remote, "/srv/app");
        assert!(listing.entries.is_empty());
        assert_eq!(listing.notice, Some(Notice::Key(REMOTE_OFFLINE_NOTICE)));
        // 未连接时远端主目录是空串（不是某个编造的部署路径）。
        assert_eq!(Side::Remote.home(), "");
    }

    #[test]
    fn profiles_round_trip_through_json() {
        let profile = StoredProfile {
            id: "prod-01".to_owned(),
            label: "prod-01".to_owned(),
            host: "10.0.1.11".to_owned(),
            port: "22".to_owned(),
            user: "root".to_owned(),
            auth: "key".to_owned(),
            key_file: "~/.ssh/id_ed25519".to_owned(),
            group: "prod".to_owned(),
            remember: false,
            password: None,
            forwards: vec![StoredForward {
                kind: "local".to_owned(),
                bind: String::new(),
                bind_port: "8080".to_owned(),
                host: "127.0.0.1".to_owned(),
                host_port: "80".to_owned(),
            }],
            agent_forwarding: true,
        };
        let json = serde_json::to_string(&[profile]).expect("序列化");
        let back: Vec<StoredProfile> = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "prod-01");
        assert_eq!(back[0].group, "prod");
        assert_eq!(back[0].password, None);
        assert_eq!(back[0].forwards.len(), 1);
        assert_eq!(back[0].forwards[0].bind_port, "8080");
        assert!(back[0].agent_forwarding, "开关要跟着档案往返");

        // 旧存档里根本没有这个键：必须读成 `false`（默认关），而不是整份档案读不出来
        // ——「升级一次丢光连接列表」是这类新增字段最容易出的事故。
        let legacy = r#"[{"id":"old","label":"old","host":"h","port":"22","user":"u",
            "auth":"key","key_file":"","group":"prod","remember":false,"forwards":[]}]"#;
        let restored: Vec<StoredProfile> = serde_json::from_str(legacy).expect("旧存档要能读");
        assert_eq!(restored.len(), 1);
        assert!(
            !restored[0].agent_forwarding,
            "缺键时必须默认关（转发是「把身份借出去」的能力，缺省绝不能是开）"
        );
    }

    #[test]
    fn move_profile_to_group_only_moves_when_needed() {
        let mut profiles = vec![StoredProfile {
            id: "prod-01".to_owned(),
            group: "prod".to_owned(),
            ..StoredProfile::default()
        }];

        // 找不到档案：无事发生。
        assert!(!move_profile_to_group(&mut profiles, "missing", "staging"));
        // 拖回原组：no-op，不产生伪变更。
        assert!(!move_profile_to_group(&mut profiles, "prod-01", "prod"));
        // 正常迁移。
        assert!(move_profile_to_group(&mut profiles, "prod-01", "staging"));
        assert_eq!(profiles[0].group, "staging");

        // 迁到**自定义分组**（分组名即 id，没有静态骨架约束）。
        assert!(move_profile_to_group(&mut profiles, "prod-01", "预发布"));
        assert_eq!(profiles[0].group, "预发布");
    }

    #[test]
    fn custom_groups_derive_from_profiles_in_first_seen_order() {
        let profile = |id: &str, group: &str| StoredProfile {
            id: id.to_owned(),
            group: group.to_owned(),
            ..StoredProfile::default()
        };

        // 自定义组按**首次出现顺序**去重；内置三档不算自定义。
        let profiles = vec![
            profile("a", "预发布"),
            profile("b", "prod"),
            profile("c", "预发布"),
            profile("d", "客户 X"),
        ];
        assert_eq!(custom_group_names(&profiles), vec!["预发布", "客户 X"]);

        // 内置 id 一个字都不能漏进自定义列表（否则左栏会出现重复分组）。
        let only_builtin = vec![
            profile("a", "prod"),
            profile("b", "staging"),
            profile("c", "local"),
        ];
        assert!(custom_group_names(&only_builtin).is_empty());
    }

    #[test]
    fn group_views_append_custom_groups_after_builtin_three() {
        let profiles = vec![
            StoredProfile {
                id: "a".to_owned(),
                group: "客户 X".to_owned(),
                ..StoredProfile::default()
            },
            StoredProfile {
                id: "b".to_owned(),
                group: "prod".to_owned(),
                ..StoredProfile::default()
            },
        ];
        let groups = group_views(&profiles, Lang::Zh);
        // 内置三档在前（位置不因用户数据漂移），自定义组在后。
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].id, "prod");
        assert_eq!(groups[1].id, "staging");
        assert_eq!(groups[2].id, "local");
        assert_eq!(groups[3].id, "客户 X");
        // 自定义组的显示名就是用户原文（不翻译），id 与名字同一份。
        assert_eq!(groups[3].label, "客户 X");
        assert_eq!(groups[3].items.len(), 1);
        assert_eq!(groups[0].items.len(), 1);
    }

    #[test]
    fn forward_parsing_drops_unknown_or_broken_rows() {
        // 方向认不出 → 整条丢掉，不猜。
        let unknown = StoredForward {
            kind: "tunnel".to_owned(),
            bind_port: "8080".to_owned(),
            ..StoredForward::default()
        };
        assert!(forward_from_stored(&unknown).is_none());

        // 端口坏掉 → 解析成 0，`Target::validate` 会在连接前拦下并说明原因。
        let broken = StoredForward {
            kind: "local".to_owned(),
            bind_port: "abc".to_owned(),
            host: "127.0.0.1".to_owned(),
            host_port: "80".to_owned(),
            ..StoredForward::default()
        };
        let forward = forward_from_stored(&broken).expect("方向合法就该还原");
        assert_eq!(forward.bind_port, 0);
        assert!(forward.validate().is_err());

        // 合法的一条原样还原。
        let good = StoredForward {
            kind: "dynamic".to_owned(),
            bind: "127.0.0.1".to_owned(),
            bind_port: "1080".to_owned(),
            ..StoredForward::default()
        };
        let forward = forward_from_stored(&good).expect("动态转发");
        assert_eq!(forward.kind, duck_ssh::ForwardKind::Dynamic);
        assert_eq!(forward.spec(), "127.0.0.1:1080");
    }

    #[test]
    fn profile_json_without_new_fields_still_reads() {
        // 旧存档缺字段时必须按缺省值读出来，而不是整份读不出：
        // 读不出等于用户的连接档案集体「消失」，那是比缺字段严重得多的事故。
        let back: Vec<StoredProfile> =
            serde_json::from_str(r#"[{"id":"a","label":"a","host":"h"}]"#).expect("旧存档");
        assert_eq!(back[0].port, "");
        assert!(!back[0].remember);
        // 口令密文字段同样可缺（旧存档本来就没有）。
        assert_eq!(back[0].password, None);
        // 端口转发同理：旧存档没有这一项时读到空表，而不是整份读不出来。
        assert!(back[0].forwards.is_empty());
    }

    #[test]
    fn local_listing_sorts_dirs_first() {
        // 临时目录里放一个文件 + 一个子目录，断言目录排在前面。
        let dir = std::env::temp_dir().join(format!("duck-listing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("adir")).expect("建子目录");
        fs::write(dir.join("afile.txt"), b"x").expect("写文件");

        let listing = local_listing(&dir.to_string_lossy());
        assert_eq!(listing.notice, None);
        assert_eq!(listing.entries.len(), 2);
        assert!(listing.entries[0].dir);
        assert_eq!(listing.entries[0].name, "adir");
        assert_eq!(listing.entries[1].name, "afile.txt");
        assert_eq!(listing.entries[1].size, "1 B");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 传输状态机的合法迁移逐条钉住（含重试回 `Queued`、取消两支、恒等）。
    #[test]
    fn transfer_state_transitions_follow_the_documented_machine() {
        use TransferState::{Active, Cancelled, Cancelling, Done, Failed, Queued};

        // 正常路径：入队 → 传输 → 完成。
        assert!(Queued.can_transition_to(Active));
        assert!(Active.can_transition_to(Done));
        // 失败与重试：Active → Failed →（重试）→ Queued → Active。
        assert!(Active.can_transition_to(Failed));
        assert!(Failed.can_transition_to(Queued));
        assert!(Queued.can_transition_to(Active));
        // 取消链：Active → Cancelling → Cancelled；入队即失败（起不来）也合法。
        assert!(Active.can_transition_to(Cancelling));
        assert!(Cancelling.can_transition_to(Cancelled));
        assert!(Queued.can_transition_to(Failed));
        // 恒等是空操作，一律合法（界面可能每帧重放同一结论）。
        for state in [Queued, Active, Cancelling, Cancelled, Done, Failed] {
            assert!(state.can_transition_to(state), "{state:?} 恒等应合法");
        }
        // 非法：终态不能再动，取消中不能跳回 Active，Cancelled 不能变 Done。
        assert!(!Done.can_transition_to(Active));
        assert!(!Cancelled.can_transition_to(Active));
        assert!(!Cancelling.can_transition_to(Active));
        assert!(!Cancelling.can_transition_to(Done));
        assert!(!Queued.can_transition_to(Done));
        assert!(!Failed.can_transition_to(Done));
    }

    #[test]
    fn transfer_state_active_and_terminal_branches() {
        use TransferState::{Active, Cancelled, Cancelling, Done, Failed, Queued};

        // `is_active`：只有传输中与取消中为真（取消中仍是「在途」，进度轮询还在跑）。
        assert!(Active.is_active());
        assert!(Cancelling.is_active());
        for state in [Queued, Cancelled, Done, Failed] {
            assert!(!state.is_active(), "{state:?} 不应算进行中");
        }
        // `is_terminal`：三个终态；Cancelling / Active 不是。
        assert!(Cancelled.is_terminal());
        assert!(Done.is_terminal());
        assert!(Failed.is_terminal());
        for state in [Queued, Active, Cancelling] {
            assert!(!state.is_terminal(), "{state:?} 不是终态");
        }
        // 每个变体都有非空文案（i18n 源串的兜底检查）。
        for state in [Queued, Active, Cancelling, Cancelled, Done, Failed] {
            assert!(!state.label().is_empty(), "{state:?} 缺文案");
        }
    }
}
