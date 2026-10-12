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

/// 连接分组的固定骨架。
///
/// 分组**是产品结构不是用户数据**（不做「自建分组」这层复杂度）：三档固定，
/// 用户档案按 `group` 字段落位。缺省分组是「生产环境」，与新建弹窗的默认选中一致。
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
    pub(crate) id: &'static str,
    /// 分组名（i18n 源串）。
    pub(crate) label: &'static str,
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

/// 连接清单视图：固定分组骨架 + 真实档案。
///
/// **入参就是真源**（页面的 `profiles` 字段）——本函数只做「贴标签 + 塞内置项」，
/// 不碰存储、不碰状态。
///
/// `lang` 只用来翻译**内置项**的名字：用户自己取的名字（`profile.label`）是数据、
/// 不是文案，跟着语言变才是 bug。
#[must_use]
pub(crate) fn group_views(profiles: &[StoredProfile], lang: Lang) -> Vec<Group> {
    GROUP_DEFS
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
                id,
                label,
                icon,
                items,
            }
        })
        .collect()
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

    /// 主目录。本地取 `$HOME`；远端在 SFTP 会话建立后由远端上报
    ///（`Session::cwd()`），**未连接时是空串** —— 空串即「没有可浏览的目录」，
    /// 界面据此走未连接态，不会把用户导到一个不存在的路径上。
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
/// 只用于展示：本工程尚未引时区库，日期按 UTC 计算；接真实实现时改由
/// 系统本地时区格式化。**不引 `chrono`/`time`** 是本仓的依赖准入结论。
fn format_modified(time: SystemTime) -> String {
    let Ok(elapsed) = time.duration_since(UNIX_EPOCH) else {
        return "—".to_owned();
    };
    let seconds = elapsed.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let within_day = seconds.rem_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    format!(
        "{month:02}-{day:02} {:02}:{:02}",
        within_day / 3_600,
        (within_day % 3_600) / 60
    )
}

/// 由「1970-01-01 起的天数」还原公历年月日（Howard Hinnant 的 `civil_from_days`）。
///
/// 手写而不用库：只有一处调用点，且算法是纯整数运算、可单测（见本文件测试）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as i64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    (if month <= 2 { year + 1 } else { year }, month, day)
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
    /// **只有 0 和 1 两个取值**：`scp` 不报中间进度，而插值画一个「看起来在动」
    /// 的进度条就是编数据 —— 用户会据此判断「还有多久」，那是个假答案。
    /// 真正带进度的传输要换 `sftp` 协议自己分块收发（M3 的后续项）。
    pub(crate) progress: f32,
    /// 状态。
    pub(crate) state: TransferState,
    /// 是否上传（决定箭头方向）。
    pub(crate) upload: bool,
    /// 失败原因（`scp` 的 stderr 原文）；其余状态为 `None`。
    pub(crate) reason: Option<String>,
}

/// 一次传输的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferState {
    /// 已入队，还没开始。
    Queued,
    /// 正在传。
    Running,
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
            Self::Running => "传输中",
            Self::Done => "已完成",
            Self::Failed => "失败",
        }
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
        assert_eq!(civil_from_days(0), (1970, 1, 1));
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
        };
        let json = serde_json::to_string(&[profile]).expect("序列化");
        let back: Vec<StoredProfile> = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "prod-01");
        assert_eq!(back[0].group, "prod");
        assert_eq!(back[0].password, None);
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
}
