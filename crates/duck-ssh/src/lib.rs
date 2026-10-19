//! Duck Toolbox 远端能力：SSH 终端 + SFTP 文件管理的**真实实现**。
//!
//! # 为什么是「驱动系统 ssh」而不是内置 SSH 客户端
//!
//! `docs/terminal-ssh-sftp-design.md` §4.3 把三条胶水路线列了出来，首选是
//! 「借道 vendored alacritty 的 `SshPty` 补丁」（路线 a）。**该前提已实测不成立**：
//! 本仓 vendored 的 alacritty 只补了 ASCII 快路径，没有 `SshPty` 后端，
//! 也就是说「把 SSH channel 喂给 `EventLoop`」这件事没有现成实现可借。
//!
//! 于是按文档给出的回退走**路线 b**：由系统 `ssh` 起一个真实 PTY 会话
//!（`ssh -t host`），`duck-term` 的 [`SpawnOptions::shell`](duck_term::SpawnOptions)
//! 本来就能接任意 argv，内核一行都不用改。
//!
//! 这条路线的收益是**立刻拿到真实连接**，而不是一条要走通 800 行 channel↔IO 胶水
//! 才能看见光标动的赌注：
//!
//! - **认证**：`~/.ssh/config`、`ssh-agent`、公钥、密码、跳板机、`ProxyCommand`
//!   全部由 OpenSSH 自己处理，等于直接继承了用户已有的全部登录能力；
//! - **主机密钥**：走系统 `~/.ssh/known_hosts`，首次连接的指纹确认就是 ssh 自己的
//!   交互流程（在 PTY 里能应答），变更时会按用户的 `StrictHostKeyChecking` 报错 ——
//!   **fail-closed，且不替用户做决定**；
//! - **连接复用**：靠 OpenSSH 自己的 `ControlMaster`，终端与 SFTP 共用同一条物理连接
//!   （见 [`Target::base_argv`]），不需要自研连接池。
//!
//! 代价说清楚：需要机器上有 `ssh` / `scp`（macOS、Linux 自带；Windows 10+ 需
//! 「可选功能 → OpenSSH 客户端」）。缺了会在 [`probe`] 里如实报告，界面据此禁用入口，
//! **不会静默失败**。
//!
//! # 分层
//!
//! 本 crate **不依赖 GPUI、不依赖 duck-term**，只产出「命令行 + 进程输出」：
//!
//! - [`Target`] —— 一台主机 + 认证方式，负责构造 argv；
//! - [`exec`] / [`list_dir`] —— 跑一次远端命令并解析结果（远端目录浏览走它）；
//! - [`upload`] / [`download`] —— 基于 `scp` 的文件传输；
//! - [`probe`] —— 探测 `ssh` / `scp` 是否可用。
//!
//! 终端那条路更短：调用方拿 [`Target::base_argv`] 拼出 `ssh -t ...`，
//! 直接交给 `duck_term::Session::spawn`。

mod known_hosts;
mod remote;
mod target;

pub use known_hosts::{HostKeyStatus, known_hosts_path, status as host_key_status};
pub use remote::{
    Output, RemoteEntry, RemoteListing, TransferHandle, close_master, download, download_spawn,
    exec, list_dir, local_size, mkdir, path_size, remove, rename, shell_quote, test_connection,
    upload, upload_spawn,
};
pub use target::{
    AuthMethod, Availability, ForwardKind, PortForward, Target, askpass_script, control_path,
};

/// 远端能力的错误。
///
/// 刻意不包 `io::Error` 的通配分支：调用方（界面）要按**用户能看懂的原因**分派，
/// 「ssh 没装」和「主机拒绝连接」该给不同的下一步建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshError {
    /// 系统找不到 `ssh` / `scp`。`program` 是缺的那个。
    NotFound { program: &'static str },
    /// 目标参数不合法（主机 / 用户名 / 端口），见 [`Target::validate`]。
    /// `reason` 直接就是 i18n 源串。
    InvalidTarget { reason: &'static str },
    /// 进程起不来（权限、fork 失败……）。`message` 是 OS 给的原文。
    Spawn { message: String },
    /// 远端命令返回非零。带上 stderr 的首行供界面展示。
    Failed { status: Option<i32>, stderr: String },
    /// 远端目录进不去（列举前的 `cd` 失败）：目录不存在、或没有进入权限。
    ///
    /// 单独立一个变体而不是塞进 [`Self::Failed`]：那三类的下一步动作完全不同 ——
    /// 「目录不存在」要用户换一个目录，而 `Failed` 里的「连接被拒 / 权限被拒」
    /// 要去查认证与主机配置。远端脚本用 stderr 上的哨兵把这一情况标出来。
    NoSuchDir { path: String },
    /// 传输失败（`scp` 非零退出）。`message` 是 `scp` 的原文。
    Transfer { message: String },
    /// 操作超过时限还没结果（进程已被杀掉）。
    ///
    /// 单独立一个变体而不是混进 [`Self::Failed`]：卡死的下一步是「重试 / 检查网络」，
    /// 而 `Failed` 的下一步是「看远端到底报了什么」—— 混在一起用户只能看到
    /// 「没有输出」，那是上一版最让人无措的一类提示。
    TimedOut { secs: u64 },
    /// 本地源路径不存在（**上传前**的检查，见 [`upload`]）。
    LocalMissing { path: String },
}

impl SshError {
    /// 文案模板与占位参数（`{}` 站位，与 `duck_theme::tf` 同一约定）。
    ///
    /// 本 crate **刻意零依赖、不认识 `duck-theme`**（见 crate 文档），
    /// 所以内核只交出「i18n 源串 + 参数」，语言由界面层决定；
    /// [`Self::message()`] 用**同一份数据**渲染中文，供日志与 `Display` 使用 ——
    /// 两边同源，不会各写一份而慢慢漂移。
    ///
    /// 文案留在内核而不是撒到调用点：错误文案与错误成因一一对应，
    /// 分散开就会出现「同一类失败，三处说法不一样」。
    #[must_use]
    pub fn template(&self) -> (&'static str, Vec<String>) {
        match self {
            Self::NotFound { program } => (
                "未找到系统命令 {}，请先安装 OpenSSH 客户端。",
                vec![(*program).to_owned()],
            ),
            Self::InvalidTarget { reason } => (reason, Vec::new()),
            Self::Spawn { message } => ("无法启动连接进程：{}", vec![message.clone()]),
            Self::Failed { status, stderr } => {
                let detail = stderr.trim();
                match (status, detail.is_empty()) {
                    (Some(code), true) => ("远端命令失败（退出码 {}）。", vec![code.to_string()]),
                    (Some(code), false) => (
                        "远端命令失败（退出码 {}）：{}",
                        vec![code.to_string(), detail.to_owned()],
                    ),
                    // 没有退出码 = 被信号杀掉（用户中断、OOM killer）。
                    // 此前这里把「信号终止」这个**短语**塞进退出码的槽位，
                    // 于是文案变成「退出码 信号终止」—— 抽出模板时顺手改掉了。
                    (None, true) => ("远端命令被信号终止。", Vec::new()),
                    (None, false) => ("远端命令被信号终止：{}", vec![detail.to_owned()]),
                }
            }
            Self::Transfer { message } => ("传输失败：{}", vec![message.clone()]),
            Self::TimedOut { secs } => (
                "远端操作超时（{} 秒无结果），请检查网络后重试。",
                vec![secs.to_string()],
            ),
            Self::NoSuchDir { path } => ("远端目录不存在或无法进入：{}", vec![path.clone()]),
            Self::LocalMissing { path } => ("本地路径不存在：{}", vec![path.clone()]),
        }
    }

    /// 给用户看的说明（中文）。
    ///
    /// 内核内部与 `Display` 用；**界面请走 [`Self::template`]**，
    /// 否则英文界面里会冒出中文。
    #[must_use]
    pub fn message(&self) -> String {
        let (source, args) = self.template();
        fill(source, &args)
    }
}

/// 把 `source` 里的 `{}` 按顺序换成 `args`（与 `duck_theme::tf` 同规则：
/// 参数多了忽略，少了保留未替换的占位符）。
fn fill(source: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(source.len() + 32);
    let mut rest = source;
    for arg in args {
        match rest.find("{}") {
            Some(index) => {
                out.push_str(&rest[..index]);
                out.push_str(arg);
                rest = &rest[index + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

impl std::fmt::Display for SshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SshError {}

/// 探测本机 OpenSSH 客户端是否可用。
///
/// 界面在**进入页面时调用一次**并缓存结果：每次点击都探测会拖慢交互，
/// 而「装没装 ssh」在一次会话里不会变。
#[must_use]
pub fn probe() -> Availability {
    Availability {
        ssh: which("ssh").is_some(),
        scp: which("scp").is_some(),
    }
}

/// 在 `PATH` 里找一个可执行文件。
///
/// 自己找而不是起 `which`：Windows 上没有 `which`，而且探测本身不该再开一个进程。
#[must_use]
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["exe", "cmd", ""]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let candidate = if ext.is_empty() {
                dir.join(program)
            } else {
                dir.join(format!("{program}.{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个变体都要给出模板与消息，且**占位符个数与参数个数一致**。
    ///
    /// 这条不变量是英译能对上的前提：源串里的 `{}` 由界面层按参数顺序替换，
    /// 少一个参数就会把 `{}` 原样漏到界面上；多一个参数则被静默丢掉。
    #[test]
    fn template_placeholders_match_arguments() {
        let cases = [
            SshError::NotFound { program: "ssh" },
            SshError::InvalidTarget {
                reason: "主机地址为空。",
            },
            SshError::Spawn {
                message: "permission denied".to_owned(),
            },
            SshError::Failed {
                status: Some(1),
                stderr: "boom".to_owned(),
            },
            SshError::Failed {
                status: Some(0),
                stderr: String::new(),
            },
            SshError::Failed {
                status: None,
                stderr: String::new(),
            },
            SshError::Failed {
                status: None,
                stderr: "killed".to_owned(),
            },
            SshError::Transfer {
                message: "no space left".to_owned(),
            },
            SshError::NoSuchDir {
                path: "~/nope".to_owned(),
            },
            SshError::LocalMissing {
                path: "/nope".to_owned(),
            },
        ];
        for error in cases {
            let (source, args) = error.template();
            assert_eq!(
                source.matches("{}").count(),
                args.len(),
                "{error:?} 的模板与参数对不上：{source}"
            );
            assert!(
                !error.message().contains("{}"),
                "{error:?} 渲染后仍留着占位符"
            );
        }
    }

    /// 被信号终止是**独立的一句**，不再往「退出码」的槽位里塞短语。
    #[test]
    fn signal_death_is_not_faked_as_an_exit_code() {
        let error = SshError::Failed {
            status: None,
            stderr: String::new(),
        };
        let message = error.message();
        assert!(message.contains("信号终止"), "{message}");
        assert!(!message.contains("退出码"), "{message}");
    }
}
