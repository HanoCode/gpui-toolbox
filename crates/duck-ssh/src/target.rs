//! 连接目标与命令行构造。
//!
//! 这一层**只做字符串**：把一条连接档案翻译成 `ssh` / `scp` 的 argv 与需要的环境变量。
//! 真正的进程启动在 [`crate::remote`]。分开是为了让「参数怎么拼」能被单测钉住 ——
//! argv 拼错的表现是「连不上」，而原因往往藏在几十个字符的选项里。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 端口转发的方向（对应 `ssh` 的 `-L` / `-R` / `-D` 三个开关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardKind {
    /// `-L`：把**本机**监听端口转发到远端可达的目标（本地转发）。
    Local,
    /// `-R`：把**远端**监听端口转发回本机可达的目标（远程转发）。
    Remote,
    /// `-D`：在本机起一个动态 SOCKS 代理。
    Dynamic,
}

impl ForwardKind {
    /// 全部方向（下拉项的顺序）。
    pub const ALL: [Self; 3] = [Self::Local, Self::Remote, Self::Dynamic];

    /// 持久化用的值。
    #[must_use]
    pub const fn value(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Dynamic => "dynamic",
        }
    }

    /// 由持久化值还原；未知值返回 `None`（调用点据此丢掉这一条，不猜）。
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.value() == value)
    }

    /// `ssh` 的参数开关。
    const fn flag(self) -> &'static str {
        match self {
            Self::Local => "-L",
            Self::Remote => "-R",
            Self::Dynamic => "-D",
        }
    }
}

/// 一条端口转发。
///
/// 构造方式是**独立 argv**（`["-L", "8080:127.0.0.1:80"]`）而不是塞进 `-o`：
/// `-o` 的值由 `ssh` 的配置解析器**再拆一次词**，含空格的绑定地址（`bind_address`）
/// 会被拆坏（与 `ControlPath` 同一个坑，见 [`Target::common_argv`]）。
/// 独立参数不做二次词法解析，值原样到达 `ssh`，因此**不需要引号**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForward {
    /// 方向。
    pub kind: ForwardKind,
    /// 绑定地址（`-L` / `-R` 的 `bind_address`，`-D` 的 SOCKS 绑定地址）。
    /// 留空 = 用 `ssh` 的默认（本地回环）。
    pub bind: String,
    /// 绑定端口（`-D` 即 SOCKS 端口）。
    pub bind_port: u16,
    /// 目标地址（`-D` 不使用）。
    pub host: String,
    /// 目标端口（`-D` 不使用）。
    pub host_port: u16,
}

impl PortForward {
    /// 跟在开关后面的那一个 argv。
    ///
    /// - `-L` / `-R`：`[bind:]port:host:hostport`
    /// - `-D`：`[bind:]port`
    #[must_use]
    pub fn spec(&self) -> String {
        let bind = self.bind.trim();
        let head = if bind.is_empty() {
            self.bind_port.to_string()
        } else {
            format!("{bind}:{}", self.bind_port)
        };
        match self.kind {
            ForwardKind::Dynamic => head,
            ForwardKind::Local | ForwardKind::Remote => {
                format!("{head}:{}:{}", self.host.trim(), self.host_port)
            }
        }
    }

    /// 参数是否可用。返回的是 **i18n 源串**（本 crate 不认识 `duck-theme`）。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.bind_port == 0 {
            return Err("转发监听端口不能为 0。");
        }
        match self.kind {
            ForwardKind::Dynamic => Ok(()),
            ForwardKind::Local | ForwardKind::Remote => {
                if self.host.trim().is_empty() {
                    return Err("转发目标地址为空。");
                }
                if self.host_port == 0 {
                    return Err("转发目标端口不能为 0。");
                }
                Ok(())
            }
        }
    }
}

/// 用户在连接档案里选的认证方式 —— 决定 `ssh` **试哪些方法**。
///
/// ⚠️ 这是「用户的选择」，不是「有没有口令 / 有没有私钥」的推论。
///
/// 两者必须分开：用户选了「密码」但**没勾「记住口令」**时，`password` 是 `None`；
/// 选了「公钥」但还没填路径时，`identity` 是 `None`。若按「字段有没有值」来推导
/// 收窄策略，这两种情况都会退化成「什么都不收窄」—— `ssh` 于是把 ssh-agent 与
/// `~/.ssh` 里的钥匙逐个递上去当认证尝试，几把就能吃光服务端 `MaxAuthTries`
///（OpenSSH 默认 6），口令那一路**根本轮不上**，用户看到的就是
/// `Permission denied (publickey,password)`，而且永远等不到口令提示。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthMethod {
    /// 指定私钥：只递 `identity` 那一把。
    Key,
    /// ssh-agent 转发的钥匙：只试公钥，不做别的。
    Agent,
    /// 口令 / 键盘交互（PAM、双因子的口令框走 kbd-interactive）。
    Password,
    /// 没指定：按 `identity` / `password` 是否存在**推断**（历史口径，
    /// 留给不标认证方式的调用方与单测，例如 `scp`）。
    #[default]
    Auto,
}

/// 一台远端主机 + 认证方式。
///
/// `Debug` 手写（见文件末尾）：`password` 是明文口令，落到日志里就是事故。
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// 主机地址（IP 或域名，也可以是 `~/.ssh/config` 里的别名）。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 用户名。
    pub user: String,
    /// 私钥文件路径（公钥认证时使用）。
    pub identity: Option<PathBuf>,
    /// 口令。**只在内存里**，通过环境变量交给 askpass 助手，不落盘、不进 argv。
    pub password: Option<String>,
    /// 用户选的认证方式。收窄 `ssh` 尝试列表**只看它**（见 [`AuthMethod`]）。
    pub auth: AuthMethod,
    /// 额外透传给 `ssh` 的 `-o` 选项（跳板机、超时等）。
    pub options: Vec<String>,
    /// 端口转发（`-L` / `-R` / `-D`）。
    ///
    /// 只加在**交互式会话**的 argv 上（[`Self::terminal_argv`]）：`scp` 没有这三个
    /// 开关，塞进去会让文件传输直接报 `unknown option`。
    pub forwards: Vec<PortForward>,
    /// 是否把本机 ssh-agent 的 socket **转发给远端**（OpenSSH `ForwardAgent yes`）。
    ///
    /// ⚠️ 与 [`AuthMethod::Agent`] 是**两件事**，两者都可以各自独立开关：
    /// - `AuthMethod::Agent`：用本机 agent 里的钥匙去认证**本机 → 远端**这一跳；
    /// - `agent_forwarding`：让**远端**反过来用本机 agent 里的钥匙
    ///   （典型场景：从跳板机再 `git clone` / `ssh` 到第三台机器，私钥全程不落地）。
    ///
    /// 默认 **关**：开着等于把「以你的身份去认证其他机器」的能力借给了远端主机 ——
    /// 远端 root 可以拿它去访问你能访问的任何地方。只在远端确实要代你访问别处时才开。
    pub agent_forwarding: bool,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 口令永远是 `***`：这个类型会出现在日志与错误里，明文口令不能跟着走。
        formatter
            .debug_struct("Target")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("identity", &self.identity)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("auth", &self.auth)
            .field("options", &self.options)
            .field("forwards", &self.forwards)
            .field("agent_forwarding", &self.agent_forwarding)
            .finish()
    }
}

impl Target {
    /// 目标串：`user@host`。
    #[must_use]
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// 参数是否可用（主机非空、用户名非空、端口非零）。
    ///
    /// 界面在连接前调用：把「没填主机」和「连不上」分开报，用户才知道该改哪里。
    /// 返回的是 **i18n 源串**（本 crate 零依赖、不认识 `duck-theme`），
    /// 由界面层交给 `duck_theme::t` 换语言。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.host.trim().is_empty() {
            return Err("主机地址为空。");
        }
        if self.user.trim().is_empty() {
            return Err("用户名为空。");
        }
        if self.port == 0 {
            return Err("端口不能为 0。");
        }
        // 转发配置也在本地就能判：留到 ssh 起进程只会得到一句含糊的
        // `Bad local forwarding specification`，用户不知道错在哪一条。
        for forward in &self.forwards {
            forward.validate()?;
        }
        Ok(())
    }

    /// `ssh` 的基础参数（**不含程序名、不含远端命令**）。
    ///
    /// 选项顺序刻意固定，便于测试比对：
    ///
    /// 1. `-p <port>`
    /// 2. `-i <identity>`（有私钥时）
    /// 3. 连接复用三件套（`socket_dir` 给了才加）
    /// 4. 保活两项
    /// 5. 认证方法协商（见下方注释：按用户选的认证方式收窄，避免钥匙把重试次数吃光）
    /// 6. 调用方给的 `options`
    #[must_use]
    pub fn base_argv(&self, socket_dir: Option<&Path>) -> Vec<String> {
        self.common_argv("-p", socket_dir)
    }

    /// `scp` 的基础参数（**不含程序名**）。
    ///
    /// 与 [`Self::base_argv`] 只差端口开关：`scp` 用**大写** `-P`，小写的 `-p`
    /// 在 scp 里是「保留时间戳 / 权限」。这处写错只在「非 22 端口」时才暴露 ——
    /// 最容易整段评审都漏过去的一类 bug。
    #[must_use]
    pub fn scp_base_argv(&self, socket_dir: Option<&Path>) -> Vec<String> {
        self.common_argv("-P", socket_dir)
    }

    /// 两个客户端共用的参数骨架（端口开关是唯一的差异点）。
    fn common_argv(&self, port_flag: &str, socket_dir: Option<&Path>) -> Vec<String> {
        let mut argv = vec![port_flag.to_owned(), self.port.to_string()];
        if let Some(identity) = &self.identity {
            argv.push("-i".to_owned());
            argv.push(identity.to_string_lossy().into_owned());
        }
        if let Some(dir) = socket_dir {
            let socket = control_path(dir, self);
            argv.push("-o".to_owned());
            argv.push("ControlMaster=auto".to_owned());
            argv.push("-o".to_owned());
            // 值必须带引号：`-o` 的值由 ssh 的配置解析器再拆一次词，路径里有空格
            //（macOS 的 `Application Support`、带空格的用户名）就会报
            // 「keyword controlpath extra arguments at end of line」，连接直接失败。
            argv.push(format!("ControlPath=\"{}\"", socket.to_string_lossy()));
            // master 在最后一个客户端退出后继续存活 5 分钟：SFTP 面板与终端标签
            // 之间来回切换时不必重新握手（重握手要几秒，而且会再要一次 MFA）。
            argv.push("-o".to_owned());
            argv.push("ControlPersist=300".to_owned());
        }
        argv.push("-o".to_owned());
        argv.push("ServerAliveInterval=30".to_owned());
        argv.push("-o".to_owned());
        argv.push("ServerAliveCountMax=3".to_owned());
        // —— 认证方法协商 ——
        //
        // 不写这一组时 ssh 会把**所有它能用的方法依次试一遍**：本机 ssh-agent 与
        // `~/.ssh/` 里挂着的每一把钥匙都算一次认证尝试，很容易把服务端的
        // `MaxAuthTries`（OpenSSH 默认 6）提前吃光 —— 于是口令这一路**根本没轮上**，
        // 用户看到的就是「用户名口令都对，却报 `Permission denied
        // (publickey,password)`」。
        //
        // 档案里已经写明了用户选的认证方式，这里就按它收窄。这与 PuTTY / Termius
        // 的「Authentication method」、以及 zed / vermilion 的 ssh 参数是同一件事：
        // **用户选了什么就只试什么**，别替他把 agent 里的钥匙全递上去。
        //
        // ⚠️ 收窄只认 [`AuthMethod`]，**不看 `identity` / `password` 有没有值**。
        // 按「有没有值」推导会在两种常见情形下退化成「完全不收窄」：
        // ① 选了「密码」但没勾「记住口令」→ `password` 是 `None`；
        // ② 选了「公钥」但还没填路径 → `identity` 是 `None`。
        // 这两种都仍然要按用户的选择收窄（见 [`AuthMethod::Auto`] 的说明）。
        match self.auth {
            AuthMethod::Key => {
                // 只认档案指定的私钥（`-i` 已在上方）；`IdentitiesOnly` 关掉
                // 「顺带把 agent 里的钥匙也递上去」。
                //
                // 没填路径时也留着它：这时 `ssh` 只试 `~/.ssh` 里的默认钥匙，
                // 仍然比「把 agent 里所有钥匙全递一遍」克制。
                argv.push("-o".to_owned());
                argv.push("IdentitiesOnly=yes".to_owned());
            }
            AuthMethod::Agent => {
                // 只试公钥，且**不能加** `IdentitiesOnly` —— 那会把 agent 一起关掉。
                argv.push("-o".to_owned());
                argv.push("PreferredAuthentications=publickey".to_owned());
            }
            AuthMethod::Password => {
                // 公钥整条关掉，只留口令与键盘交互两种问法。
                // `keyboard-interactive` 必须留着 —— 一部分服务端（PAM / 双因子）
                // 把口令框实现成 kbd-interactive 而不是 `password`。
                //
                // 没有存口令时同样收窄：`ssh` 会在 PTY 里给出正常的口令提示，
                // 而不是先去把 agent 里的钥匙试光。
                argv.push("-o".to_owned());
                argv.push("PubkeyAuthentication=no".to_owned());
                argv.push("-o".to_owned());
                argv.push("PreferredAuthentications=password,keyboard-interactive".to_owned());
                argv.push("-o".to_owned());
                argv.push("KbdInteractiveAuthentication=yes".to_owned());
            }
            // 没标认证方式：沿用「有私钥就只认它、其次有口令就只留口令」的旧口径。
            AuthMethod::Auto => {
                if self.identity.is_some() {
                    argv.push("-o".to_owned());
                    argv.push("IdentitiesOnly=yes".to_owned());
                } else if self.password.is_some() {
                    argv.push("-o".to_owned());
                    argv.push("PubkeyAuthentication=no".to_owned());
                    argv.push("-o".to_owned());
                    argv.push("PreferredAuthentications=password,keyboard-interactive".to_owned());
                    argv.push("-o".to_owned());
                    argv.push("KbdInteractiveAuthentication=yes".to_owned());
                }
            }
        }
        // —— agent 转发 ——
        //
        // 放在**共用骨架**里，而不是只给终端会话加：
        // ① `ForwardAgent` 是**连接级**选项，而本仓所有客户端（终端 / `exec` / `scp`）
        //    共用同一条物理连接（`ControlMaster` + 同一个 `ControlPath`）—— 选项由
        //    **第一个建立 master 的客户端**决定，后复用的客户端再传也改不了。
        //    只在 `terminal_argv` 上给的话，SFTP 面板先连上时转发就不生效，且
        //    「先开终端还是先开 SFTP」会导致时灵时不灵 —— 最难查的一类问题。
        // ② `scp` 同样认 `-o ForwardAgent=yes`，多带上无害。
        //
        // 与认证收窄**互不干扰**：`IdentitiesOnly=yes` / `PubkeyAuthentication=no`
        // 管的是「本机往上递哪把钥匙」，转发管的是「远端能不能用本机 agent」。
        // 所以 `AuthMethod::Key` 与转发可以同时开，两个 `-o` 并存没有冲突。
        if self.agent_forwarding {
            argv.push("-o".to_owned());
            argv.push("ForwardAgent=yes".to_owned());
        }
        for option in &self.options {
            argv.push("-o".to_owned());
            argv.push(option.clone());
        }
        argv
    }

    /// 端口转发的参数（`-L` / `-R` / `-D`，**开关与值各占一个 argv**）。
    ///
    /// 抽出来是因为「值怎么拼」要被单测钉住，而调用点（终端会话）只关心
    /// 「有没有这一组参数」。顺序按档案里的顺序 —— 多条 `-D` 会争同一个端口时的
    /// 报错顺序因此可预期。
    #[must_use]
    pub fn forward_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        for forward in &self.forwards {
            argv.push(forward.kind.flag().to_owned());
            argv.push(forward.spec());
        }
        argv
    }

    /// 交互式终端会话的完整 argv（含 `-t` 与目标串）。
    ///
    /// `-t` 是必须的：不强制分配 PTY，远端不会给 shell 提示符，
    /// 而且 `sudo`、`top` 这类程序会因为「不是终端」而拒绝运行。
    ///
    /// 端口转发**只在这里加**：`scp`（[`Self::scp_base_argv`]）不认识 `-L/-R/-D`。
    #[must_use]
    pub fn terminal_argv(&self, socket_dir: Option<&Path>) -> Vec<String> {
        let mut argv = self.base_argv(socket_dir);
        argv.extend(self.forward_argv());
        argv.push("-t".to_owned());
        argv.push(self.destination());
        argv
    }

    /// 起进程时要设置的环境变量。
    ///
    /// 两块，各自独立：
    ///
    /// - **agent socket**（选了「SSH Agent」认证，**或**开了 agent 转发时）：
    ///   macOS 上从 Finder / Dock 启动的 GUI 进程常常**没有** `SSH_AUTH_SOCK`，
    ///   于是「Agent 认证」静默拿不到任何钥匙、`ForwardAgent=yes` 静默转发一个
    ///   不存在的 socket（远端 `ssh-add -l` 只报 `Could not open a connection to
    ///   your authentication agent`）—— 补法见 [`Self::agent_socket`]。
    ///
    ///   ⚠️ 转发档下**即使**认证方式是「口令」也要补这个 socket：那档虽然带
    ///   `PubkeyAuthentication=no`（agent 钥匙用不上、收窄名实相符），但转发要的是
    ///   **socket 本身**，跟本机怎么认证无关。
    /// - **askpass 口令**：只有**带口令**时才设置，没有口令时把 `SSH_ASKPASS_REQUIRE`
    ///   设成 `force` 会让一个本来能走公钥的连接卡在「等助手」上。
    ///
    /// 口令走**环境变量**而不是 argv —— argv 在 `ps` 里对同机所有用户可见。
    #[must_use]
    pub fn env(&self, helper: Option<&Path>) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if self.auth == AuthMethod::Agent || self.agent_forwarding {
            env.extend(agent_env(Self::agent_socket()));
        }
        let Some(password) = &self.password else {
            return env;
        };
        let Some(helper) = helper else {
            // 拿不到助手脚本路径（写盘失败）：不设置，让 ssh 走自己的交互提示 ——
            // 至少不会把口令塞进一个错误的通道。
            return env;
        };
        env.extend([
            (
                "SSH_ASKPASS".to_owned(),
                helper.to_string_lossy().into_owned(),
            ),
            // `force`：即使当前有终端也走助手，用户已经明确「记住口令」。
            ("SSH_ASKPASS_REQUIRE".to_owned(), "force".to_owned()),
            ("DUCK_SSH_PASSWORD".to_owned(), password.clone()),
            // askpass 助手是个 shell 脚本，得让它能找到解释器。
            (
                "DISPLAY".to_owned(),
                std::env::var("DISPLAY").unwrap_or_default(),
            ),
        ]);
        env
    }

    /// 解析 ssh-agent 的 socket 路径（best-effort，只为「SSH Agent」这一档补环境）。
    ///
    /// macOS 上从 Finder / Dock 启动的 GUI 进程**不继承**登录 shell 的
    /// `SSH_AUTH_SOCK`：环境里没有它，`ssh` 就一个 agent 钥匙都问不到，
    /// 而界面上「SSH Agent」还是可选项 —— 表现为静默失败。
    ///
    /// 先读进程环境（终端启动 / 从 shell 拉起时本来就有）；读不到再问一次
    /// launchd（`launchctl getenv SSH_AUTH_SOCK`，登录时由 launchd 注入的那个）。
    /// 两条都拿不到就返回 `None`，不猜路径 —— 猜错会把 ssh 指向一个不存在的
    /// socket，报错比「没有 agent」更难懂。
    fn agent_socket() -> Option<String> {
        std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|socket| !socket.is_empty())
            .or_else(agent_socket_from_launchd)
    }
}

/// 问一次 launchd 要 agent socket（只有 macOS 有这一步，其余平台恒 `None`）。
#[cfg(target_os = "macos")]
fn agent_socket_from_launchd() -> Option<String> {
    let output = std::process::Command::new("launchctl")
        .args(["getenv", "SSH_AUTH_SOCK"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let socket = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!socket.is_empty()).then_some(socket)
}

/// 非 macOS 平台没有 launchd 这一步。
#[cfg(not(target_os = "macos"))]
fn agent_socket_from_launchd() -> Option<String> {
    None
}

/// 把解析到的 agent socket 拼成环境变量对（`None` 或空串就什么都不加）。
///
/// 抽成纯函数只为可测：socket 是否存在取决于跑测试的机器，而「灌空值不该产出
/// 空字符串的 `SSH_AUTH_SOCK`」这条必须被钉住 —— 空的 `SSH_AUTH_SOCK` 会让
/// `ssh` 认定「有 agent 但连不上」，反而比不设置更糟。
fn agent_env(socket: Option<String>) -> Vec<(String, String)> {
    socket
        .filter(|socket| !socket.is_empty())
        .map(|socket| vec![("SSH_AUTH_SOCK".to_owned(), socket)])
        .unwrap_or_default()
}

/// 本机 OpenSSH 客户端的可用性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Availability {
    /// `ssh` 可用。
    pub ssh: bool,
    /// `scp` 可用。
    pub scp: bool,
}

impl Availability {
    /// 能不能开 SSH 终端。
    #[must_use]
    pub fn terminal(&self) -> bool {
        self.ssh
    }

    /// 能不能用 SFTP 文件管理（列举与传输都要）。
    #[must_use]
    pub fn files(&self) -> bool {
        self.ssh && self.scp
    }

    /// 全部可用。
    #[must_use]
    pub fn all(&self) -> bool {
        self.ssh && self.scp
    }
}

/// 连接复用 socket 的路径。
///
/// 用「`用户@主机:端口` 的哈希」做文件名，而不是把目标串直接当文件名：
/// 主机名里可能有 `/`、`:` 等不能进文件名的字符，而且长度不可控
///（Unix domain socket 的路径上限是 104 字节，超了整个连接直接失败）。
///
/// 放在**应用自己的数据目录**下（调用方传 `socket_dir`）而不是 `/tmp`：
/// `/tmp` 是共享的，另一个用户预先占住同名 socket 就能劫持复用。
#[must_use]
pub fn control_path(socket_dir: &Path, target: &Target) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    target.destination().hash(&mut hasher);
    target.port.hash(&mut hasher);
    target.identity.hash(&mut hasher);
    socket_dir.join(format!("{:016x}.sock", hasher.finish()))
}

/// askpass 助手脚本的内容。
///
/// 从环境变量读口令并打到 stdout。OpenSSH 会把它当作用户在密码提示符下的输入。
#[must_use]
pub const fn askpass_body() -> &'static str {
    // 尾随换行是必须的：ssh 读一整行，没有换行会一直等到超时。
    "#!/bin/sh\nprintf '%s\\n' \"$DUCK_SSH_PASSWORD\"\n"
}

/// 把 askpass 助手脚本落到 `dir` 下并返回它的路径。
///
/// 只在**确实有口令**时调用：没有口令的连接不需要它。
/// 权限 0700 —— 脚本本身不含秘密，但它读的环境变量含，别给别人可乘之机。
#[cfg(unix)]
pub fn askpass_script(dir: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::create_dir_all(dir)?;
    let path = dir.join("askpass.sh");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(&path)?;
    file.write_all(askpass_body().as_bytes())?;
    Ok(path)
}

/// Windows 上 OpenSSH 对 `SSH_ASKPASS` 的支持不可靠，**不落脚本**。
///
/// 返回 `None` 而不是报错：口令认证在 Windows 上会退化成 ssh 自己的交互提示
///（在终端标签里用户仍能手动输入），这是可用的降级，不是故障。
#[cfg(not(unix))]
pub fn askpass_script(_dir: &Path) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SSH_ASKPASS 在 Windows 上不受支持",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            host: "10.0.1.11".to_owned(),
            port: 22,
            user: "root".to_owned(),
            identity: None,
            password: None,
            auth: AuthMethod::Auto,
            options: Vec::new(),
            forwards: Vec::new(),
            agent_forwarding: false,
        }
    }

    /// `-o <value>` 是否作为**相邻**两项出现。
    ///
    /// 只用 `contains(&value)` 会漏掉「值在、`-o` 丢了」这种半截写法 ——
    /// 那时 `ssh` 会把 `ForwardAgent=yes` 当成**目标主机名**，报错离现场很远。
    fn has_option(argv: &[String], value: &str) -> bool {
        argv.windows(2)
            .any(|pair| pair[0] == "-o" && pair[1] == value)
    }

    /// 一条本地转发：本机 8080 → 远端本地的 80。
    fn local_forward() -> PortForward {
        PortForward {
            kind: ForwardKind::Local,
            bind: String::new(),
            bind_port: 8080,
            host: "127.0.0.1".to_owned(),
            host_port: 80,
        }
    }

    #[test]
    fn port_forward_specs_follow_openssh_syntax() {
        let mut forward = local_forward();
        assert_eq!(
            forward.spec(),
            "8080:127.0.0.1:80",
            "没给绑定地址时只写端口"
        );

        // 远程转发与本地转发同形，只有开关不同。
        forward.kind = ForwardKind::Remote;
        assert_eq!(forward.spec(), "8080:127.0.0.1:80");

        // 绑定地址显式给出时带上（`*` = 监听全部网卡）。
        forward.kind = ForwardKind::Local;
        forward.bind = "127.0.0.1".to_owned();
        assert_eq!(forward.spec(), "127.0.0.1:8080:127.0.0.1:80");

        // 动态 SOCKS 只有绑定（`[bind:]port`），不带目标。
        let dynamic = PortForward {
            kind: ForwardKind::Dynamic,
            bind: String::new(),
            bind_port: 1080,
            host: "ignored".to_owned(),
            host_port: 0,
        };
        assert_eq!(dynamic.spec(), "1080");
        let bound = PortForward {
            bind: "127.0.0.1".to_owned(),
            ..dynamic
        };
        assert_eq!(bound.spec(), "127.0.0.1:1080");
    }

    #[test]
    fn forwards_are_separate_argv_entries_not_o_options() {
        // 值必须与开关**分开**：`-o` 的值会被 ssh 的配置解析器再拆一次词，
        // 含空格的绑定地址会被拆坏（与 ControlPath 同一个坑）。
        let target = Target {
            forwards: vec![
                local_forward(),
                PortForward {
                    kind: ForwardKind::Dynamic,
                    bind: String::new(),
                    bind_port: 1080,
                    host: String::new(),
                    host_port: 0,
                },
            ],
            ..target()
        };
        let argv = target.terminal_argv(None);
        assert!(argv.contains(&"-L".to_owned()));
        assert!(argv.contains(&"8080:127.0.0.1:80".to_owned()));
        assert!(argv.contains(&"-D".to_owned()));
        assert!(argv.contains(&"1080".to_owned()));
        assert!(
            !argv
                .iter()
                .any(|arg| arg.starts_with("LocalForward") || arg.starts_with("RemoteForward")),
            "不要用 -o LocalForward：那会走配置解析器的二次拆词"
        );
        // 目标串仍排在最后，转发排在它前面。
        assert_eq!(argv.last(), Some(&"root@10.0.1.11".to_owned()));
    }

    #[test]
    fn scp_argv_never_carries_forwards() {
        // `scp` 没有 -L/-R/-D：塞进去会让文件传输直接报 unknown option。
        let target = Target {
            forwards: vec![local_forward()],
            ..target()
        };
        let argv = target.scp_base_argv(None);
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "-L" || arg == "-R" || arg == "-D")
        );
    }

    #[test]
    fn forward_validation_reports_the_field() {
        let bad_port = PortForward {
            bind_port: 0,
            ..local_forward()
        };
        assert!(bad_port.validate().unwrap_err().contains("监听端口"));

        let no_host = PortForward {
            host: "  ".to_owned(),
            ..local_forward()
        };
        assert!(no_host.validate().unwrap_err().contains("目标地址"));

        let no_host_port = PortForward {
            host_port: 0,
            ..local_forward()
        };
        assert!(no_host_port.validate().unwrap_err().contains("目标端口"));

        // 动态转发不需要目标：空 host / 0 host_port 都合法。
        let dynamic = PortForward {
            kind: ForwardKind::Dynamic,
            bind_port: 1080,
            host: String::new(),
            host_port: 0,
            ..local_forward()
        };
        assert!(dynamic.validate().is_ok());

        // 转发不合法时整条目标也不合法（在本地就拦下，而不是起进程再失败）。
        let target = Target {
            forwards: vec![bad_port],
            ..target()
        };
        assert!(target.validate().is_err());
    }

    #[test]
    fn forward_kind_round_trips_through_its_value() {
        for kind in ForwardKind::ALL {
            assert_eq!(ForwardKind::from_value(kind.value()), Some(kind));
        }
        assert_eq!(ForwardKind::from_value("tunnel"), None);
    }

    #[test]
    fn terminal_argv_has_pty_and_destination() {
        let argv = target().terminal_argv(None);
        assert!(argv.contains(&"-t".to_owned()), "必须强制分配 PTY");
        assert_eq!(argv.last(), Some(&"root@10.0.1.11".to_owned()));
        assert!(argv.contains(&"22".to_owned()));
    }

    #[test]
    fn control_master_is_added_only_with_socket_dir() {
        let without = target().base_argv(None);
        assert!(!without.iter().any(|arg| arg.starts_with("ControlMaster")));

        let with = target().base_argv(Some(Path::new("/tmp/duck")));
        assert!(with.contains(&"ControlMaster=auto".to_owned()));
        assert!(with.contains(&"ControlPersist=300".to_owned()));
        assert!(
            with.iter().any(|arg| arg.starts_with("ControlPath=")),
            "给了 socket 目录就必须有 ControlPath"
        );
    }

    #[test]
    fn control_path_is_quoted_when_dir_has_spaces() {
        // macOS 的应用数据目录就带空格（`Application Support`）：不加引号时
        // ssh 的配置解析器把值拆成两个词，报
        // 「keyword controlpath extra arguments at end of line」。
        let argv = target().base_argv(Some(Path::new("/data/Application Support/sockets")));
        let control = argv
            .iter()
            .find(|arg| arg.starts_with("ControlPath="))
            .expect("给了 socket 目录就必须有 ControlPath");
        let value = control.strip_prefix("ControlPath=").expect("前缀");
        assert!(
            value.starts_with('"') && value.ends_with('"') && value.contains(' '),
            "含空格的路径必须整体加引号，实际是 {control:?}"
        );
    }

    #[test]
    fn control_path_is_stable_and_short() {
        let first = control_path(Path::new("/data/sockets"), &target());
        let second = control_path(Path::new("/data/sockets"), &target());
        assert_eq!(first, second, "同一目标必须算出同一 socket 才能复用");
        assert!(first.to_string_lossy().len() < 104, "Unix socket 路径上限");

        let other = Target {
            host: "10.0.1.12".to_owned(),
            ..target()
        };
        assert_ne!(
            control_path(Path::new("/data/sockets"), &other),
            first,
            "不同主机不能共用 socket"
        );
    }

    #[test]
    fn password_never_reaches_argv_or_debug() {
        let secret = "correct-horse";
        let with_password = Target {
            password: Some(secret.to_owned()),
            ..target()
        };
        let argv = with_password.terminal_argv(Some(Path::new("/tmp/duck")));
        assert!(
            !argv.iter().any(|arg| arg.contains(secret)),
            "口令绝不能进 argv：ps 对同机所有用户可见"
        );
        let debug = format!("{with_password:?}");
        assert!(!debug.contains(secret), "Debug 输出会把口令写进日志");
        assert!(debug.contains("***"));
    }

    #[test]
    fn env_carries_password_through_helper_only() {
        let with_password = Target {
            password: Some("pw".to_owned()),
            ..target()
        };
        // 没有助手脚本路径时不设置任何 askpass 变量，避免连接卡在「等助手」上。
        assert!(with_password.env(None).is_empty());

        let env = with_password.env(Some(Path::new("/data/askpass.sh")));
        let pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        assert!(pairs.contains(&("SSH_ASKPASS_REQUIRE", "force")));
        assert_eq!(
            pairs
                .iter()
                .find(|(key, _)| *key == "DUCK_SSH_PASSWORD")
                .map(|(_, value)| *value),
            Some("pw")
        );

        // 没有口令时不设置：否则一个本来能走公钥的连接会卡在等助手上。
        assert!(target().env(Some(Path::new("/data/askpass.sh"))).is_empty());
    }

    #[test]
    fn password_auth_stops_ssh_from_trying_keys_first() {
        // 回归：macOS 上 ssh-agent / `~/.ssh` 常有几把钥匙，ssh 默认会把它们
        // 逐个递上去当认证尝试；密钥一多就把服务端的 `MaxAuthTries` 吃光，
        // 口令那一路根本轮不上 —— 表现就是「用户名口令都对，却报 permission denied」。
        let argv = target().base_argv(None);
        assert!(
            !argv.iter().any(|arg| arg.contains("PubkeyAuthentication")),
            "没给口令时不该动公钥认证：{argv:?}"
        );

        let with_password = Target {
            password: Some("pw".to_owned()),
            ..target()
        };
        let argv = with_password.base_argv(None);
        assert!(argv.contains(&"PubkeyAuthentication=no".to_owned()));
        assert!(
            argv.contains(&"PreferredAuthentications=password,keyboard-interactive".to_owned())
        );
        assert!(
            argv.contains(&"KbdInteractiveAuthentication=yes".to_owned()),
            "PAM / 双因子的口令框走 keyboard-interactive，不能关掉"
        );
    }

    /// 回归：**选了「密码」但没勾「记住口令」时，收窄必须照样生效**。
    ///
    /// 这是 `Permission denied (publickey,password)` 最常见的成因：档案里
    /// `auth = password`，但口令没保存（用户没勾「记住」），于是 `password` 是
    /// `None`。若按「有没有口令」推导，就会退化成完全不收窄 —— ssh 先把
    /// ssh-agent 与 `~/.ssh` 里的钥匙逐个递上去，几把就吃光 `MaxAuthTries`，
    /// 口令那一路根本没轮上，用户甚至等不到口令提示。
    #[test]
    fn password_auth_narrows_even_without_a_stored_password() {
        // 口令认证 + 没有口令（没勾「记住」）：仍要关公钥、只留口令两路。
        let no_password = Target {
            auth: AuthMethod::Password,
            ..target()
        };
        let argv = no_password.base_argv(None);
        assert!(
            argv.contains(&"PubkeyAuthentication=no".to_owned()),
            "选了「密码」就必须关掉公钥，哪怕这次没保存口令：{argv:?}"
        );
        assert!(
            argv.contains(&"PreferredAuthentications=password,keyboard-interactive".to_owned())
        );
        assert!(argv.contains(&"KbdInteractiveAuthentication=yes".to_owned()));

        // 对照组：口令认证 + 有口令，argv 与上面**逐字相同** ——
        // 收窄只看认证方式，不看有没有值。
        let with_password = Target {
            password: Some("pw".to_owned()),
            auth: AuthMethod::Password,
            ..target()
        };
        assert_eq!(argv, with_password.base_argv(None));
    }

    /// 回归：**选了「公钥」就只递那一把**，agent 与 `~/.ssh` 的其它钥匙都不试。
    #[test]
    fn key_auth_only_offers_the_configured_identity() {
        let keyed = Target {
            identity: Some(PathBuf::from("/home/u/.ssh/id_ed25519")),
            auth: AuthMethod::Key,
            ..target()
        };
        let argv = keyed.base_argv(None);
        assert!(argv.contains(&"IdentitiesOnly=yes".to_owned()));
        assert!(
            !argv.iter().any(|arg| arg.contains("PubkeyAuthentication")),
            "公钥认证不该去动口令那一组：{argv:?}"
        );

        // 还没填路径也仍然关掉「顺带递 agent 钥匙」。
        let no_path = Target {
            auth: AuthMethod::Key,
            ..target()
        };
        assert!(
            no_path
                .base_argv(None)
                .contains(&"IdentitiesOnly=yes".to_owned())
        );
    }

    /// 选了「SSH Agent」时只试公钥，且**不能**带 `IdentitiesOnly` ——
    /// 那会把 agent 里的钥匙一起关掉，等于把这一档废掉。
    #[test]
    fn agent_auth_offers_publickey_only_without_closing_the_agent() {
        let agent = Target {
            auth: AuthMethod::Agent,
            ..target()
        };
        let argv = agent.base_argv(None);
        assert!(argv.contains(&"PreferredAuthentications=publickey".to_owned()));
        assert!(
            !argv.contains(&"IdentitiesOnly=yes".to_owned()),
            "`IdentitiesOnly=yes` 会让 agent 里的钥匙一个都用不上：{argv:?}"
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg.contains("PubkeyAuthentication=no")),
            "agent 认证要试公钥，不能关掉它：{argv:?}"
        );
    }

    /// 空的 `SSH_AUTH_SOCK` 比「不设置」更糟：`ssh` 会认定「有 agent 但连不上」。
    #[test]
    fn agent_env_never_sets_an_empty_socket() {
        assert!(agent_env(None).is_empty());
        assert!(agent_env(Some(String::new())).is_empty());
        assert_eq!(
            agent_env(Some("/tmp/agent.sock".to_owned())),
            vec![("SSH_AUTH_SOCK".to_owned(), "/tmp/agent.sock".to_owned())]
        );
    }

    /// 只有「SSH Agent」这一档才补 socket。口令档明确不走公钥，不该把用户 shell
    /// 里的 agent 顺带塞进子进程环境 —— 那会让「只留口令两路」的收窄变得名不副实。
    #[test]
    fn only_agent_auth_injects_the_agent_socket() {
        let with_password = Target {
            password: Some("pw".to_owned()),
            auth: AuthMethod::Password,
            ..target()
        };
        let env = with_password.env(Some(Path::new("/data/askpass.sh")));
        assert!(
            !env.iter().any(|(key, _)| key == "SSH_AUTH_SOCK"),
            "口令档不该注入 agent socket：{env:?}"
        );

        // Agent 档注入的值与 `agent_socket()` 同源（有没有 socket 取决于机器，
        // 但「注入什么」这件事是确定的）。
        let agent = Target {
            auth: AuthMethod::Agent,
            ..target()
        };
        let env = agent.env(None);
        match Target::agent_socket() {
            Some(socket) => assert_eq!(
                env,
                vec![("SSH_AUTH_SOCK".to_owned(), socket)],
                "解析到 socket 就必须原样注入"
            ),
            None => assert!(env.is_empty(), "没解析到就别猜路径：{env:?}"),
        }
    }

    /// 转发默认**关**；开了才加，而且是 `-o` + 值相邻成对。
    ///
    /// 同时钉住「终端与 `scp` 都要带」：`ForwardAgent` 是连接级选项，而同一条
    /// `ControlMaster` 连接的选项由**第一个**连上的客户端决定 —— 只在终端加，
    /// 「先开 SFTP 还是先开终端」就会让转发时灵时不灵。
    #[test]
    fn agent_forwarding_is_opt_in_and_rides_the_shared_skeleton() {
        let off = target();
        assert!(
            !off.base_argv(None)
                .iter()
                .any(|arg| arg.contains("ForwardAgent")),
            "默认不该转发：{:?}",
            off.base_argv(None)
        );
        assert!(
            !off.scp_base_argv(None)
                .iter()
                .any(|arg| arg.contains("ForwardAgent")),
            "默认不该转发（scp 同理）"
        );

        let on = Target {
            agent_forwarding: true,
            ..target()
        };
        assert!(
            has_option(&on.base_argv(None), "ForwardAgent=yes"),
            "开了转发必须成对写出：{:?}",
            on.base_argv(None)
        );
        assert!(
            has_option(&on.scp_base_argv(None), "ForwardAgent=yes"),
            "scp 也要带（连接级选项，谁先连上谁说了算）"
        );
        // 转发不加在 `-L/-R/-D` 那一组里：它与端口转发是两回事，混进去会
        // 让 `forward_argv()` 的「一开关一值」形态失真。
        assert!(
            !on.forward_argv().iter().any(|arg| arg.contains("Forward")),
            "转发不该混进端口转发参数：{:?}",
            on.forward_argv()
        );
    }

    /// 转发与**认证收窄**互不干扰：指定私钥时两个 `-o` 并存。
    ///
    /// `IdentitiesOnly=yes` 管的是「本机往上递哪把钥匙」，转发管的是「远端能不能
    /// 用本机 agent」—— 后者不过本机认证这条路，所以前者关不掉它。
    #[test]
    fn agent_forwarding_coexists_with_identity_narrowing() {
        let both = Target {
            identity: Some(PathBuf::from("/home/u/.ssh/id_ed25519")),
            auth: AuthMethod::Key,
            agent_forwarding: true,
            ..target()
        };
        let argv = both.base_argv(None);
        assert!(has_option(&argv, "IdentitiesOnly=yes"), "{argv:?}");
        assert!(has_option(&argv, "ForwardAgent=yes"), "{argv:?}");
    }

    /// 转发档下**即使认证方式是口令**也要补 agent socket —— 转发要的是 socket
    /// 本身，与「本机用什么方式认证」无关；而收窄仍在（`PubkeyAuthentication=no`），
    /// 所以「只留口令两路」这条口径没有被破坏。
    #[test]
    fn agent_forwarding_injects_the_socket_even_under_password_auth() {
        // 没开转发：守住既有口径，口令档一个 agent 变量都不塞。
        let quiet = Target {
            password: Some("pw".to_owned()),
            auth: AuthMethod::Password,
            ..target()
        };
        assert!(
            !quiet
                .env(Some(Path::new("/data/askpass.sh")))
                .iter()
                .any(|(key, _)| key == "SSH_AUTH_SOCK"),
            "没开转发时口令档不该注入 agent socket"
        );

        // 开了转发：socket 必须补上（值与本机解析结果同源），且收窄照样在。
        let forwarding = Target {
            password: Some("pw".to_owned()),
            auth: AuthMethod::Password,
            agent_forwarding: true,
            ..target()
        };
        let argv = forwarding.base_argv(None);
        assert!(
            has_option(&argv, "PubkeyAuthentication=no"),
            "转发不该动摇认证收窄：{argv:?}"
        );

        let env = forwarding.env(Some(Path::new("/data/askpass.sh")));
        match Target::agent_socket() {
            Some(socket) => assert_eq!(
                env.iter()
                    .find(|(key, _)| key == "SSH_AUTH_SOCK")
                    .map(|(_, value)| value.as_str()),
                Some(socket.as_str()),
                "解析到 socket 就必须原样注入：{env:?}"
            ),
            None => assert!(
                !env.iter().any(|(key, _)| key == "SSH_AUTH_SOCK"),
                "没解析到就别猜路径：{env:?}"
            ),
        }
    }

    #[test]
    fn an_explicit_identity_is_the_only_one_offered() {
        let with_key = Target {
            identity: Some(PathBuf::from("/home/u/.ssh/id_ed25519")),
            ..target()
        };
        let argv = with_key.base_argv(None);
        assert!(argv.contains(&"IdentitiesOnly=yes".to_owned()));
        assert!(
            !argv.iter().any(|arg| arg.contains("PubkeyAuthentication")),
            "走了私钥就别再关公钥认证"
        );
    }

    #[test]
    fn agent_auth_keeps_the_default_method_list() {
        // 既没私钥也没口令 = 交给 ssh-agent：这时**不要**收窄，
        // 否则「agent 里的钥匙 + 服务端还要口令」这种组合会被我们挡死。
        let argv = target().base_argv(None);
        assert!(
            !argv
                .iter()
                .any(|arg| arg.contains("PreferredAuthentications"))
        );
        assert!(!argv.iter().any(|arg| arg.contains("IdentitiesOnly")));
    }

    #[test]
    fn validate_reports_the_missing_field() {
        assert!(target().validate().is_ok());
        let no_host = Target {
            host: "  ".to_owned(),
            ..target()
        };
        assert!(no_host.validate().unwrap_err().contains("主机"));
        let no_port = Target {
            port: 0,
            ..target()
        };
        assert!(no_port.validate().unwrap_err().contains("端口"));
    }
}
