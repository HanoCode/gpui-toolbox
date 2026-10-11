//! `plugin.toml` 清单：三类执行模型共用的声明格式。
//!
//! 设计对齐 zed 的 `extension.toml`（声明式、`deny_unknown_fields` 防拼写漂移）
//! 与 deepseek-harness 的「约定式声明」：清单只声明**是什么、要什么**，
//! 怎么执行由 `execution_model` 决定，宿主据此选择加载轨道。
//!
//! 最小示例：
//!
//! ```toml
//! id = "dev.duck.sample"
//! name = "示例插件"
//! version = "0.1.0"
//! api_version = 1
//! execution_model = "js"
//!
//! [[tools]]
//! id = "dev.duck.sample.hash"
//! name = "示例计算"
//! ```
//!
//! 工具 id **强制**以插件 id 加 `.` 为前缀（见 [`PluginManifest::validate`]），
//! 从源头杜绝插件冒充内置工具（如 `video.parse`）。

use serde::Deserialize;

use crate::PluginError;

/// 当前支持的清单 API 版本。清单的 `api_version` 必须精确匹配才可加载；
/// 未来不兼容演进时抬升此值并保留旧版本加载轨道（参考 zed 的 WIT 快照方案）。
pub const SUPPORTED_API_VERSION: u32 = 1;

/// 执行模型：决定宿主把插件装进哪条加载轨道。
///
/// - [`ExecutionModel::Js`]：进程内 QuickJS 脚本（gpui-shell），UI 面板主路径；
/// - [`ExecutionModel::Wasm`]：wasmtime 沙箱组件，后台计算引擎，无 UI；
/// - [`ExecutionModel::Process`]：独立进程 + IPC，系统级能力，无热重载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionModel {
    /// 进程内 JS 脚本插件。
    Js,
    /// WASM 沙箱组件插件。
    Wasm,
    /// 独立进程插件。
    Process,
}

impl ExecutionModel {
    /// 清单里的字符串形态（`plugin.toml` 用小写下划线）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Js => "js",
            Self::Wasm => "wasm",
            Self::Process => "process",
        }
    }
}

impl<'de> Deserialize<'de> for ExecutionModel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "js" | "script" => Ok(Self::Js),
            "wasm" => Ok(Self::Wasm),
            "process" | "external-process" => Ok(Self::Process),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["js", "wasm", "process"],
            )),
        }
    }
}

/// 能力声明：插件请求访问的宿主资源。
///
/// **声明 ≠ 授予**：这里只是插件的请求，实际授权由
/// [`crate::CapabilityGranter`]（及安装时的用户确认）决定，未授予即拒绝。
///
/// 派生 `PartialEq` / `Eq` 是给**远端索引**用的：索引条目也带一份这份声明，
/// 落盘前要拿它和归档里的清单比对（见 `duck-plugin-registry` 的安装流程）。
/// 派生 `Serialize` 是给 `duck-app` 存的**用户授予记录**用的（`kv: plugin_caps::<id>`）：
/// 那一份用的就是这个类型，形状与清单里的 `[capabilities]` 完全一致。
/// `deny_unknown_fields` 在索引侧同样是硬闸门——客户端读不懂的能力类型
/// 不能被静默忽略，否则确认框会漏报用户即将放行的权限。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// 可读路径 glob（相对插件数据目录）。
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    /// 可写路径 glob（相对插件数据目录）。
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    /// 可访问的网络主机（精确 host 或 `host:port`）。
    #[serde(default)]
    pub network_hosts: Vec<String>,
    /// 是否请求系统通知。
    #[serde(default)]
    pub notifications: bool,
    /// 允许执行的外部命令（精确命令名，不含参数通配）。
    #[serde(default)]
    pub process_exec: Vec<String>,
}

impl Capabilities {
    /// 是否一项能力都没声明。
    pub fn is_empty(&self) -> bool {
        self.filesystem_read.is_empty()
            && self.filesystem_write.is_empty()
            && self.network_hosts.is_empty()
            && !self.notifications
            && self.process_exec.is_empty()
    }
}

/// 工具贡献：插件要装进工具箱的一个工具。
///
/// `category` 是 [`duck-tools::ToolCategory`] 的 `id()` 字符串；本 crate 不依赖
/// duck-tools，无法识别的值由上层回落到 `Other`。`icon` 同理，是
/// [`duck_components::DuckIcon`] 资源名，缺失或无法识别时上层给默认图标。
///
/// [`duck-tools::ToolCategory`]: https://docs.rs/duck-tools
/// [`duck_components::DuckIcon`]: https://docs.rs/duck-components
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolContribution {
    /// 工具稳定 id，必须以插件 id 加 `.` 开头。
    pub id: String,
    /// 展示名（中文源串，渲染时经 `tr` 取词）。
    pub name: String,
    /// 一句话说明（中文源串，可省略）。
    #[serde(default)]
    pub description: String,
    /// 分类 id（`media` / `network` / …），无法识别回落 `other`。
    #[serde(default)]
    pub category: String,
    /// 图标资源名（如 `"film"`），可省略。
    #[serde(default)]
    pub icon: Option<String>,
    /// 附加搜索词（中英），可省略。
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// 插件清单（`plugin.toml`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// 插件稳定 id，例 `"dev.duck.sample"`（小写字母 / 数字 / `.` / `-`）。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 插件自身版本号（语义化版本字符串，本层不解析）。
    pub version: String,
    /// 一句话说明，可省略。
    #[serde(default)]
    pub description: String,
    /// 插件级分类 id（`media` / `network` / …），可省略；无法识别的值由上层
    /// 回落到「其他」。工具贡献里未写分类时也用它兜底。
    #[serde(default)]
    pub category: String,
    /// 插件级标签（自由词，插件中心用于筛选与展示），可省略。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 作者署名，可省略。
    ///
    /// 只管展示：插件可能由第三方发布，署名因此属于清单本身而不是打包脚本的参数。
    /// 同时是远端索引里 `author` 字段的来源（写端从清单取，避免两处各填一份）。
    #[serde(default)]
    pub author: String,
    /// 主页 / 源码地址，可省略。展示用，语义同 [`Self::author`]。
    #[serde(default)]
    pub homepage: String,
    /// 清单 API 版本，必须等于 [`SUPPORTED_API_VERSION`]。
    pub api_version: u32,
    /// 执行模型。
    pub execution_model: ExecutionModel,
    /// 能力声明，可省略（缺省即不请求任何能力）。
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 工具贡献列表，可省略。
    #[serde(default)]
    pub tools: Vec<ToolContribution>,
}

impl PluginManifest {
    /// 从字符串解析并校验清单。
    pub fn parse(source: &str) -> Result<Self, PluginError> {
        let manifest: Self = toml::from_str(source).map_err(PluginError::Parse)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 从文件解析并校验清单。
    pub fn load(path: &std::path::Path) -> Result<Self, PluginError> {
        let source = std::fs::read_to_string(path)?;
        Self::parse(&source)
    }

    /// 校验清单：id 规范、API 版本受支持、工具 id 命名空间隔离。
    ///
    /// # Errors
    /// 任一规则不满足即返回 [`PluginError::Invalid`]。
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_id(&self.id)?;
        if self.version.trim().is_empty() {
            return Err(PluginError::Invalid("version must not be empty".into()));
        }
        if self.api_version != SUPPORTED_API_VERSION {
            return Err(PluginError::Invalid(format!(
                "unsupported api_version {} (supported: {SUPPORTED_API_VERSION})",
                self.api_version
            )));
        }
        if self.name.trim().is_empty() {
            return Err(PluginError::Invalid("name must not be empty".into()));
        }
        let prefix = format!("{}.", self.id);
        let mut seen = std::collections::HashSet::new();
        for tool in &self.tools {
            if tool.id.trim().is_empty() {
                return Err(PluginError::Invalid("tool id must not be empty".into()));
            }
            if !tool.id.starts_with(&prefix) {
                return Err(PluginError::Invalid(format!(
                    "tool id {:?} must start with {:?} (plugin namespace)",
                    tool.id, prefix
                )));
            }
            if tool.name.trim().is_empty() {
                return Err(PluginError::Invalid(format!(
                    "tool {:?} name must not be empty",
                    tool.id
                )));
            }
            if !seen.insert(tool.id.as_str()) {
                return Err(PluginError::Invalid(format!(
                    "duplicate tool id {:?}",
                    tool.id
                )));
            }
        }
        Ok(())
    }
}

/// id 规则：非空，仅 `a-z 0-9 . -`，不以 `.` / `-` 开头或结尾，长度 ≤ 128。
fn validate_id(id: &str) -> Result<(), PluginError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !id.starts_with(['.', '-'])
        && !id.ends_with(['.', '-'])
        && !id.contains("..");
    if valid {
        Ok(())
    } else {
        Err(PluginError::Invalid(format!("invalid plugin id {id:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        id = "dev.duck.sample"
        name = "示例插件"
        version = "0.1.0"
        api_version = 1
        execution_model = "js"
    "#;

    #[test]
    fn parses_minimal_manifest() {
        let manifest = PluginManifest::parse(MINIMAL).expect("minimal manifest must parse");
        assert_eq!(manifest.id, "dev.duck.sample");
        assert_eq!(manifest.execution_model, ExecutionModel::Js);
        assert!(manifest.capabilities.is_empty());
        assert!(manifest.tools.is_empty());
        assert_eq!(manifest.execution_model.as_str(), "js");
        assert!(manifest.category.is_empty());
        assert!(manifest.tags.is_empty());
        assert!(manifest.author.is_empty());
        assert!(manifest.homepage.is_empty());
    }

    #[test]
    fn parses_full_manifest() {
        let manifest = PluginManifest::parse(
            r#"
            id = "dev.duck.sample"
            name = "示例插件"
            version = "1.2.0"
            description = "一句话说明"
            category = "development"
            tags = ["文本", "utils"]
            author = "Hanocode"
            homepage = "https://github.com/HanoCode/gpui-toolbox"
            api_version = 1
            execution_model = "wasm"

            [capabilities]
            filesystem_read = ["data/*"]
            notifications = true

            [[tools]]
            id = "dev.duck.sample.hash"
            name = "示例计算"
            description = "算个哈希"
            category = "development"
            icon = "code"
            keywords = ["sample", "哈希"]
            "#,
        )
        .expect("full manifest must parse");
        assert_eq!(manifest.execution_model, ExecutionModel::Wasm);
        assert_eq!(manifest.category, "development");
        assert_eq!(manifest.tags, ["文本", "utils"]);
        assert_eq!(manifest.author, "Hanocode");
        assert_eq!(
            manifest.homepage,
            "https://github.com/HanoCode/gpui-toolbox"
        );
        assert_eq!(manifest.capabilities.filesystem_read, ["data/*"]);
        assert!(manifest.capabilities.notifications);
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].icon.as_deref(), Some("code"));
        assert_eq!(manifest.tools[0].keywords, ["sample", "哈希"]);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = PluginManifest::parse(
            r#"
            id = "dev.duck.sample"
            name = "n"
            version = "0.1.0"
            api_version = 1
            execution_model = "js"
            typo_field = true
            "#,
        )
        .expect_err("unknown field must be rejected");
        assert!(matches!(err, PluginError::Parse(_)));
    }

    #[test]
    fn rejects_unsupported_api_version() {
        let err = PluginManifest::parse(&MINIMAL.replace("api_version = 1", "api_version = 2"))
            .expect_err("future api_version must be rejected");
        assert!(matches!(err, PluginError::Invalid(_)));
    }

    #[test]
    fn rejects_tool_outside_plugin_namespace() {
        let err = PluginManifest::parse(
            r#"
            id = "dev.duck.sample"
            name = "n"
            version = "0.1.0"
            api_version = 1
            execution_model = "js"

            [[tools]]
            id = "video.parse"
            name = "冒充内置工具"
            "#,
        )
        .expect_err("tool id outside plugin namespace must be rejected");
        assert!(matches!(err, PluginError::Invalid(_)));
    }

    #[test]
    fn rejects_duplicate_tool_ids() {
        let err = PluginManifest::parse(
            r#"
            id = "dev.duck.sample"
            name = "n"
            version = "0.1.0"
            api_version = 1
            execution_model = "js"

            [[tools]]
            id = "dev.duck.sample.a"
            name = "a"

            [[tools]]
            id = "dev.duck.sample.a"
            name = "b"
            "#,
        )
        .expect_err("duplicate tool id must be rejected");
        assert!(matches!(err, PluginError::Invalid(_)));
    }

    #[test]
    fn rejects_bad_ids() {
        for id in [
            "", "DEV.duck", "dev duck", ".duck", "duck.", "duck..x", "x-",
        ] {
            assert!(
                PluginManifest::parse(&MINIMAL.replace("dev.duck.sample", id)).is_err(),
                "id {id:?} must be rejected"
            );
        }
    }
}
