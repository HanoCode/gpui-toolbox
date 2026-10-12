//! SSH 隧道：系统 `ssh -N -L` 本地端口转发（dbui 方案，零新增依赖）。
//!
//! 设计要点（`docs/database-design.md` §4.3）：
//! - **尊重用户 `~/.ssh/config`**：命令只传最小必要参数，别名 / ProxyJump / 密钥
//!   全部交给用户配置；口令认证走 `SSH_ASKPASS` + `SSH_ASKPASS_REQUIRE=force`，
//!   口令经环境变量注入子进程，**不落盘**。
//! - **防孤儿**：句柄 [`SshTunnel`] drop 时 kill 子进程并删除 askpass 脚本；
//!   UI 侧把句柄存进会话，断开 / 删除连接即随会话释放。
//! - russh 本地端口转发与本实现并存，汇合后可无缝替换（接口只暴露本地端口）。

use std::io::Write as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use crate::error::DbError;

/// 一条 SSH 隧道（跳板机 + 转发目标）。
#[derive(Debug)]
pub struct SshTunnel {
    /// 本机监听端口（数据库连接实际指向 `127.0.0.1:{local_port}`）。
    local_port: u16,
    /// ssh 子进程句柄；`None` 表示已退出 / 已 kill。
    child: Mutex<Option<Child>>,
    /// askpass 脚本路径（有口令认证时才有；drop 时删除）。
    askpass_path: Option<PathBuf>,
}

/// 隧道参数（来自连接档案的 `ssh_tunnel` 字段 + 凭据文件里的 SSH 口令）。
pub struct SshTunnelSpec {
    /// 跳板机主机。
    pub jump_host: String,
    /// 跳板机端口（`None` = 22）。
    pub jump_port: Option<u16>,
    /// 跳板机用户。
    pub user: String,
    /// 转发目标主机（相对跳板机解析；默认 `127.0.0.1` 即跳板机本机）。
    pub remote_host: String,
    /// 转发目标端口（数据库端口）。
    pub remote_port: u16,
    /// SSH 口令（走凭据文件；`None` = 依赖密钥 / agent）。
    pub password: Option<String>,
}

impl SshTunnel {
    /// 建立隧道：挑随机本地端口并拉起 `ssh -N -L`。
    ///
    /// 返回前会轮询确认端口已被 ssh 监听（最多约 5 秒），避免建连抢跑。
    pub fn open(spec: SshTunnelSpec) -> Result<Self, DbError> {
        if spec.jump_host.trim().is_empty() || spec.user.trim().is_empty() {
            return Err(DbError::Config("ssh tunnel host/user is empty".into()));
        }
        let local_port = free_local_port()
            .ok_or_else(|| DbError::Connect("no free local port for ssh tunnel".into()))?;

        // askpass 脚本：口令经环境变量进子进程，脚本本身只读变量、不落盘。
        let askpass_path = match spec.password.as_ref() {
            Some(_) => Some(Self::write_askpass_script(local_port)?),
            None => None,
        };

        let jump = match spec.jump_port {
            Some(port) => format!("{}@{}:{}", spec.user.trim(), spec.jump_host.trim(), port),
            None => format!("{}@{}", spec.user.trim(), spec.jump_host.trim()),
        };
        let mut command = Command::new("ssh");
        command
            .arg("-N")
            .arg("-T")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{local_port}:{}:{}",
                if spec.remote_host.trim().is_empty() {
                    "127.0.0.1"
                } else {
                    spec.remote_host.trim()
                },
                spec.remote_port
            ))
            .arg(jump)
            .stdin(Stdio::null());
        if let Some(path) = &askpass_path {
            command.env("SSH_ASKPASS", path);
            command.env("SSH_ASKPASS_REQUIRE", "force");
            command.env("DISPLAY", "duck-tunnel");
            if let Some(password) = &spec.password {
                command.env("DUCK_SSH_PASS", password);
            }
        }

        let child = command
            .spawn()
            .map_err(|err| DbError::Connect(format!("spawn ssh: {err}")))?;
        tracing::info!(local_port, jump_host = %spec.jump_host, "ssh tunnel opening");

        let tunnel = Self {
            local_port,
            child: Mutex::new(Some(child)),
            askpass_path,
        };
        tunnel.wait_until_listening()?;
        Ok(tunnel)
    }

    /// 写 askpass 脚本（0700，只读环境变量、口令不落盘）。
    fn write_askpass_script(local_port: u16) -> Result<PathBuf, DbError> {
        let path = std::env::temp_dir().join(format!(
            "duck-askpass-{}-{}.sh",
            std::process::id(),
            local_port
        ));
        let mut file = std::fs::File::create(&path)
            .map_err(|err| DbError::Connect(format!("askpass script: {err}")))?;
        writeln!(file, "#!/bin/sh").ok();
        writeln!(file, "printf '%s\\n' \"$DUCK_SSH_PASS\"").ok();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(path)
    }

    /// 本机监听端口（数据库连接指向 `127.0.0.1:{local_port}`）。
    #[must_use]
    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    /// 隧道是否还活着（子进程未退出）。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().expect("tunnel lock must not be poisoned");
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// 轮询等待 ssh 把本地端口监听起来（`ExitOnForwardFailure` 保证失败即退出）。
    fn wait_until_listening(&self) -> Result<(), DbError> {
        for _ in 0..50 {
            if !self.is_alive() {
                let code = self
                    .child
                    .lock()
                    .expect("tunnel lock must not be poisoned")
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten())
                    .and_then(|status| status.code());
                return Err(DbError::Connect(match code {
                    Some(code) => format!("ssh tunnel exited with code {code}"),
                    None => "ssh tunnel exited".into(),
                }));
            }
            if TcpListener::bind(("127.0.0.1", self.local_port)).is_err() {
                // 端口被占用（ssh 已监听）——这正是我们要的信号。
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(DbError::Connect(
            "ssh tunnel did not start listening in time".into(),
        ))
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self
            .child
            .lock()
            .expect("tunnel lock must not be poisoned")
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(path) = self.askpass_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 挑一个空闲本地端口（绑定后立即释放；有理论竞态，可接受）。
fn free_local_port() -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 必填参数缺失直接报配置错误，不拉进程。
    #[test]
    fn rejects_empty_spec() {
        let error = SshTunnel::open(SshTunnelSpec {
            jump_host: "  ".into(),
            jump_port: None,
            user: "u".into(),
            remote_host: "127.0.0.1".into(),
            remote_port: 5432,
            password: None,
        })
        .unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
    }
}
