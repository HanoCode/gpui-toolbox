//! Duck Toolbox 插件框架（纯逻辑层，不依赖 gpui）。
//!
//! 分层位置：与 [`duck-platform`] / [`duck-store`] 同级的最底层能力 crate。
//! 职责只有三件事：
//!
//! 1. **清单解析**（[`manifest`]）——统一 `plugin.toml` 格式，三类执行模型
//!    （JS 脚本 / WASM 组件 / 独立进程）共用同一份声明；
//! 2. **能力网关**（[`capability`]）——声明 + 授予双闸门，未授予的能力一律拒绝；
//! 3. **发现与索引**（[`registry`]）——扫描插件目录，产出 [`PluginIndex`]，
//!    由上层（`duck-app`）把它装配进 [`duck-tools`] 的工具注册表与设置界面。
//!
//! 本 crate 刻意**不依赖 gpui / duck-tools**：清单校验与能力仲裁必须是
//! 可独立单测的纯函数，gpui 侧的注册与渲染全部留在 `duck-app`。
//!
//! [`duck-platform`]: https://docs.rs/duck-platform
//! [`duck-store`]: https://docs.rs/duck-store
//! [`duck-tools`]: https://docs.rs/duck-tools

use std::fmt;

mod capability;
mod manifest;
mod registry;

pub use capability::{Capability, CapabilityGranter};
pub use manifest::{
    Capabilities, ExecutionModel, PluginManifest, SUPPORTED_API_VERSION, ToolContribution,
};
pub use registry::{DiscoveredPlugin, MANIFEST_FILE, PluginIndex};

/// 插件框架错误。
///
/// `Display` 一律英文（日志/错误信息约定，不进 i18n）；用户可见的
/// 归属判断（如「清单无效」标签）由 `duck-app` 的界面文案承担。
#[derive(Debug)]
pub enum PluginError {
    /// 读目录 / 读文件失败。
    Io(std::io::Error),
    /// `plugin.toml` 解析失败（TOML 语法或结构不符）。
    Parse(toml::de::Error),
    /// 清单通过解析但违反校验规则（空 id、不支持的 API 版本等）。
    Invalid(String),
    /// 插件在执行期出错（脚本异常、能力被拒、超时等）。
    Execution(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "plugin io error: {err}"),
            Self::Parse(err) => write!(f, "plugin manifest parse error: {err}"),
            Self::Invalid(reason) => write!(f, "invalid plugin manifest: {reason}"),
            Self::Execution(reason) => write!(f, "plugin execution error: {reason}"),
        }
    }
}

impl std::error::Error for PluginError {}

impl From<std::io::Error> for PluginError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
