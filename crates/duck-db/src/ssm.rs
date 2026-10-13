//! AWS SSM 隧道：走用户本机的 `aws` CLI 做端口转发。
//!
//! 与 [`crate::tunnel`]（SSH）同一条思路，**尊重用户本机工具**：
//!
//! - 不引 AWS SDK（那是一整棵依赖树 + 一套凭据链），而是拉起用户已经装好、已经配好
//!   （`aws configure` / SSO / 环境变量 / IAM role）的 `aws` 命令，让它去认证与转发。
//! - 命令只传最小必要参数（目标实例 / 端口 / 区域 / profile），凭据一律交给 CLI 自己解析。
//! - 形态与 SSH 隧道一致：**对内只暴露一个本机监听端口**。
//!
//! 两条官方文档的分工（Session Manager 的端口转发文档）：
//!
//! | 文档 | 参数 | 用途 |
//! | --- | --- | --- |
//! | `AWS-StartPortForwardingSession` | `portNumber` / `localPortNumber` | 转发到**实例自身**的端口 |
//! | `AWS-StartPortForwardingSessionToRemoteHost` | 再加 `host` | 转发到实例能访问的**另一台主机**（如 RDS） |
//!
//! 选哪条由「有没有填远端主机」决定：填了就走 ToRemoteHost（RDS 场景），
//! 没填就走前者（直连实例上跑的数据库）。

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::DbError;

/// 轮询等待转发的本地端口监听起来的间隔与次数（合起来约 20 秒）。
///
/// 比 SSH 那条宽松：SSM 建会话要先过一轮 AWS API（拿会话令牌），冷启动明显更慢。
const POLL_INTERVAL_MS: u64 = 200;
const POLL_ATTEMPTS: usize = 100;

/// AWS SSM 端口转发的配置（进连接档案；凭据不入 kv）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsmConfig {
    /// 目标 EC2 实例 id（`i-…`）或托管实例 id（`mi-…`）。
    #[serde(default)]
    pub instance: String,
    /// 转发到实例**能访问的**另一台主机（如 RDS 端点）；留空 = 实例自身。
    #[serde(default)]
    pub remote_host: Option<String>,
    /// AWS 区域（留空 = 用 CLI 的默认区域）。
    #[serde(default)]
    pub region: Option<String>,
    /// CLI profile（留空 = 用默认 profile）。
    #[serde(default)]
    pub profile: Option<String>,
}

impl SsmConfig {
    /// 必填项齐不齐（实例 id 不能空）。
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.instance.trim().is_empty()
    }

    /// 该用哪条文档（见模块文档的表）。
    #[must_use]
    pub fn document_name(&self) -> &'static str {
        if self
            .remote_host
            .as_deref()
            .is_some_and(|host| !host.trim().is_empty())
        {
            "AWS-StartPortForwardingSessionToRemoteHost"
        } else {
            "AWS-StartPortForwardingSession"
        }
    }
}

/// 开一条 SSM 隧道的参数。
pub struct SsmTunnelSpec {
    /// 配置本体。
    pub config: SsmConfig,
    /// 最终要连到的主机（**只在走 ToRemoteHost 文档时**作为 `host` 参数下发；
    /// 走实例自身文档时它只是给日志看的）。
    pub target_host: String,
    /// 最终要连到的端口（数据库端口）。
    pub target_port: u16,
}

/// 一条 SSM 隧道。
#[derive(Debug)]
pub struct SsmTunnel {
    /// 本机监听端口。
    local_port: u16,
    /// `aws` 子进程句柄；`None` 表示已退出 / 已 kill。
    child: Mutex<Option<Child>>,
}

impl SsmTunnel {
    /// 建立隧道：挑随机本地端口并拉起 `aws ssm start-session`。
    ///
    /// 返回前轮询确认端口已被监听，避免建连抢跑（同 [`crate::tunnel`] 的口径）。
    pub fn open(spec: SsmTunnelSpec) -> Result<Self, DbError> {
        let config = spec.config;
        if !config.is_usable() {
            return Err(DbError::Config("ssm instance id is empty".into()));
        }
        let remote_host = config
            .remote_host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .unwrap_or(spec.target_host.trim())
            .to_owned();
        let local_port = free_local_port()
            .ok_or_else(|| DbError::Connect("no free local port for ssm tunnel".into()))?;

        let mut parameters = format!(
            "portNumber={},localPortNumber={local_port}",
            spec.target_port
        );
        if config.document_name() == "AWS-StartPortForwardingSessionToRemoteHost" {
            // `host` 参数只在 ToRemoteHost 文档里存在：实例自身的文档传了会被拒。
            parameters.push_str(&format!(",host={remote_host}"));
        }

        let mut command = Command::new("aws");
        command
            .arg("ssm")
            .arg("start-session")
            .arg("--target")
            .arg(config.instance.trim())
            .arg("--document-name")
            .arg(config.document_name())
            .arg("--parameters")
            .arg(parameters)
            .stdin(Stdio::null());
        if let Some(region) = config
            .region
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            command.arg("--region").arg(region);
        }
        if let Some(profile) = config
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            command.arg("--profile").arg(profile);
        }

        let child = command.spawn().map_err(|err| {
            DbError::Connect(format!(
                "spawn aws: {err} (is the AWS CLI installed and on PATH?)"
            ))
        })?;
        tracing::info!(
            local_port,
            instance = %config.instance,
            document = config.document_name(),
            "ssm tunnel opening"
        );

        let tunnel = Self {
            local_port,
            child: Mutex::new(Some(child)),
        };
        tunnel.wait_until_listening()?;
        Ok(tunnel)
    }

    /// 本机监听端口。
    #[must_use]
    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    /// 隧道是否还活着（子进程未退出）。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().expect("ssm lock must not be poisoned");
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// 轮询等待端口被监听（进程提前退出即判定失败，带退出码）。
    fn wait_until_listening(&self) -> Result<(), DbError> {
        for _ in 0..POLL_ATTEMPTS {
            if !self.is_alive() {
                let code = self
                    .child
                    .lock()
                    .expect("ssm lock must not be poisoned")
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten())
                    .and_then(|status| status.code());
                return Err(DbError::Connect(match code {
                    Some(code) => format!("aws ssm exited with code {code}"),
                    None => "aws ssm exited".into(),
                }));
            }
            if TcpListener::bind(("127.0.0.1", self.local_port)).is_err() {
                // 端口被占用（aws 已监听）——这正是我们要的信号。
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
        Err(DbError::Connect(
            "ssm tunnel did not start listening in time".into(),
        ))
    }
}

impl Drop for SsmTunnel {
    fn drop(&mut self) {
        if let Some(mut child) = self
            .child
            .lock()
            .expect("ssm lock must not be poisoned")
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 挑一个空闲本地端口（绑定后立即释放；有理论竞态，与 SSH 隧道同款）。
fn free_local_port() -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实例 id 是唯一必填项；缺了**不拉进程**。
    #[test]
    fn rejects_an_empty_instance_id() {
        assert!(!SsmConfig::default().is_usable());
        assert!(
            !SsmConfig {
                instance: "   ".into(),
                ..SsmConfig::default()
            }
            .is_usable()
        );
        let error = SsmTunnel::open(SsmTunnelSpec {
            config: SsmConfig::default(),
            target_host: "db".into(),
            target_port: 5432,
        })
        .unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
    }

    /// 文档选择：「填了远端主机 → ToRemoteHost」，没填 → 实例自身。
    #[test]
    fn the_document_follows_the_remote_host() {
        let base = SsmConfig {
            instance: "i-123".into(),
            ..SsmConfig::default()
        };
        assert_eq!(base.document_name(), "AWS-StartPortForwardingSession");
        // 只填空格也算没填（否则会往实例自身的文档里塞一个不存在的 `host` 参数）。
        let blank = SsmConfig {
            remote_host: Some("   ".into()),
            ..base.clone()
        };
        assert_eq!(blank.document_name(), "AWS-StartPortForwardingSession");
        let with_host = SsmConfig {
            remote_host: Some("db.abc.eu-west-1.rds.amazonaws.com".into()),
            ..base
        };
        assert_eq!(
            with_host.document_name(),
            "AWS-StartPortForwardingSessionToRemoteHost"
        );
    }

    /// SSM 配置随档案落 kv（这里面本来就没有凭据——那是 CLI 自己的事）。
    #[test]
    fn ssm_config_round_trips() {
        let config = SsmConfig {
            instance: "i-abc".into(),
            remote_host: Some("db.internal".into()),
            region: Some("eu-west-1".into()),
            profile: Some("prod".into()),
        };
        let json = serde_json::to_string(&config).expect("可序列化");
        let back: SsmConfig = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, config);
        // 缺字段回落（老档案）。
        let minimal: SsmConfig =
            serde_json::from_str(r#"{"instance":"i-x"}"#).expect("缺字段要能回落");
        assert_eq!(minimal.region, None);
        assert_eq!(minimal.profile, None);
        assert_eq!(minimal.remote_host, None);
    }
}
