//! 连接目标与命令行构造。
//!
//! 这一层**只做字符串**：把一条连接档案翻译成 `ssh` / `scp` 的 argv 与需要的环境变量。
//! 真正的进程启动在 [`crate::remote`]。分开是为了让「参数怎么拼」能被单测钉住 ——
//! argv 拼错的表现是「连不上」，而原因往往藏在几十个字符的选项里。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

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
    /// 额外透传给 `ssh` 的 `-o` 选项（跳板机、超时等）。
    pub options: Vec<String>,
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
            .field("options", &self.options)
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
        if self.identity.is_some() {
            // 指定了私钥就只认它（`-i` 已在上方）；`IdentitiesOnly` 关掉
            // 「顺带把 agent 里的钥匙也递上去」。
            argv.push("-o".to_owned());
            argv.push("IdentitiesOnly=yes".to_owned());
        } else if self.password.is_some() {
            // 选了口令认证：公钥整条关掉，只留口令与键盘交互两种问法。
            // `keyboard-interactive` 必须留着 —— 一部分服务端（PAM / 双因子）
            // 把口令框实现成 kbd-interactive 而不是 `password`。
            argv.push("-o".to_owned());
            argv.push("PubkeyAuthentication=no".to_owned());
            argv.push("-o".to_owned());
            argv.push("PreferredAuthentications=password,keyboard-interactive".to_owned());
            argv.push("-o".to_owned());
            argv.push("KbdInteractiveAuthentication=yes".to_owned());
        }
        for option in &self.options {
            argv.push("-o".to_owned());
            argv.push(option.clone());
        }
        argv
    }

    /// 交互式终端会话的完整 argv（含 `-t` 与目标串）。
    ///
    /// `-t` 是必须的：不强制分配 PTY，远端不会给 shell 提示符，
    /// 而且 `sudo`、`top` 这类程序会因为「不是终端」而拒绝运行。
    #[must_use]
    pub fn terminal_argv(&self, socket_dir: Option<&Path>) -> Vec<String> {
        let mut argv = self.base_argv(socket_dir);
        argv.push("-t".to_owned());
        argv.push(self.destination());
        argv
    }

    /// 起进程时要设置的环境变量。
    ///
    /// 只有**带口令**时才设置 askpass 那一组：没有口令时把 `SSH_ASKPASS_REQUIRE`
    /// 设成 `force` 会让一个本来能走公钥的连接卡在「等助手」上。
    ///
    /// 口令走**环境变量**而不是 argv —— argv 在 `ps` 里对同机所有用户可见。
    #[must_use]
    pub fn env(&self, helper: Option<&Path>) -> Vec<(String, String)> {
        let Some(password) = &self.password else {
            return Vec::new();
        };
        let Some(helper) = helper else {
            // 拿不到助手脚本路径（写盘失败）：不设置，让 ssh 走自己的交互提示 ——
            // 至少不会把口令塞进一个错误的通道。
            return Vec::new();
        };
        vec![
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
        ]
    }
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
            options: Vec::new(),
        }
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
        assert!(argv.contains(&"PreferredAuthentications=password,keyboard-interactive".to_owned()));
        assert!(
            argv.contains(&"KbdInteractiveAuthentication=yes".to_owned()),
            "PAM / 双因子的口令框走 keyboard-interactive，不能关掉"
        );
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
        assert!(!argv.iter().any(|arg| arg.contains("PreferredAuthentications")));
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
