//! 插件发现与索引：扫描插件目录，产出装配用的 [`PluginIndex`]。
//!
//! 发现规则：插件目录下每个**子目录**若含 [`MANIFEST_FILE`]（`plugin.toml`）
//! 即视为一个插件；清单无效的条目**保留**在索引里（带错误信息），
//! 设置界面据此展示「清单无效」，而不是静默吞掉。
//!
//! 索引本身是纯数据：启用状态、与 [`duck-tools`] 注册表的装配都在上层完成。

use std::path::{Path, PathBuf};

use crate::manifest::PluginManifest;

/// 清单文件名（每个插件子目录固定一份）。
pub const MANIFEST_FILE: &str = "plugin.toml";

/// 发现到的一个插件（清单有效或无效都有可能）。
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// 插件子目录的绝对路径。
    pub dir: PathBuf,
    /// 解析校验通过的清单；无效时为 `None`。
    pub manifest: Option<PluginManifest>,
    /// 清单无效时的英文错误说明（`Display`，进日志）。
    pub error: Option<String>,
}

impl DiscoveredPlugin {
    /// 插件 id（清单无效时为 `None`）。
    pub fn id(&self) -> Option<&str> {
        self.manifest.as_ref().map(|manifest| manifest.id.as_str())
    }

    /// 展示名（清单无效时回落子目录名，让界面仍有可读标识）。
    pub fn name(&self) -> &str {
        self.manifest
            .as_ref()
            .map_or_else(|| self.dir_name(), |manifest| manifest.name.as_str())
    }

    /// 清单无效时界面用的回退名：子目录文件名。
    pub fn dir_name(&self) -> &str {
        self.dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
    }
}

/// 插件索引：一次发现的快照，按 id 升序（展示稳定）。
#[derive(Debug, Clone, Default)]
pub struct PluginIndex {
    plugins: Vec<DiscoveredPlugin>,
}

impl PluginIndex {
    /// 扫描插件目录。目录不存在（首次启动）返回空索引，不算错误。
    pub fn discover(dir: &Path) -> Self {
        Self::discover_all(std::slice::from_ref(&dir.to_path_buf()))
    }

    /// 按顺序扫描多个插件目录，合并为一个索引。
    ///
    /// 用于开发期「仓库 `plugins/` 目录 + 已安装目录」双源加载：**排在前面的
    /// 目录优先**——同一插件 id（无效清单按目录名）在多个目录出现时只保留
    /// 首个，后者告警跳过，避免同名插件被重复注册两次工具贡献。
    pub fn discover_all(dirs: &[PathBuf]) -> Self {
        let mut plugins: Vec<DiscoveredPlugin> = Vec::new();
        for dir in dirs {
            for plugin in Self::scan_dir(dir).plugins {
                let key = plugin
                    .id()
                    .map(str::to_owned)
                    .unwrap_or_else(|| plugin.dir_name().to_string());
                let duplicate = plugins.iter().any(|existing| {
                    existing
                        .id()
                        .map_or_else(|| existing.dir_name().to_string(), str::to_owned)
                        == key
                });
                if duplicate {
                    tracing::warn!(
                        "duplicate plugin {key} in {}; keeping the earlier discovery",
                        plugin.dir.display()
                    );
                    continue;
                }
                plugins.push(plugin);
            }
        }
        plugins.sort_by(|a, b| {
            a.id()
                .or(Some(a.dir_name()))
                .cmp(&b.id().or(Some(b.dir_name())))
        });
        Self { plugins }
    }

    /// 单目录扫描（[`Self::discover`] 的原始实现，供 [`Self::discover_all`] 复用）。
    fn scan_dir(dir: &Path) -> Self {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // 首次启动插件目录还不存在，是正常分支。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(err) => {
                tracing::warn!("failed to read plugin dir {}: {err}", dir.display());
                return Self::default();
            }
        };

        let mut plugins = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // 跳过隐藏目录：插件市场的暂存目录（`.tmp-*` / `.old-*`）就落在这里，
            // 它们带着清单但还没落位，被当成插件发现会与真身撞同一个 id。
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            {
                continue;
            }
            let manifest_path = path.join(MANIFEST_FILE);
            if !manifest_path.is_file() {
                continue;
            }
            match PluginManifest::load(&manifest_path) {
                Ok(manifest) => plugins.push(DiscoveredPlugin {
                    dir: path,
                    manifest: Some(manifest),
                    error: None,
                }),
                Err(err) => {
                    tracing::warn!("invalid plugin manifest {}: {err}", manifest_path.display());
                    plugins.push(DiscoveredPlugin {
                        dir: path,
                        manifest: None,
                        error: Some(err.to_string()),
                    });
                }
            }
        }
        plugins.sort_by(|a, b| {
            a.id()
                .or(Some(a.dir_name()))
                .cmp(&b.id().or(Some(b.dir_name())))
        });
        Self { plugins }
    }

    /// 已发现的插件（含清单无效条目，按 id 升序）。
    pub fn plugins(&self) -> &[DiscoveredPlugin] {
        &self.plugins
    }

    /// 清单有效的插件数（供「共 N 个」文案）。
    pub fn loaded_count(&self) -> usize {
        self.plugins
            .iter()
            .filter(|plugin| plugin.manifest.is_some())
            .count()
    }

    /// 反查某个工具 id 属于哪个插件（贡献回填后的归属查询）。
    pub fn plugin_for_tool(&self, tool_id: &str) -> Option<&DiscoveredPlugin> {
        self.plugins.iter().find(|plugin| {
            plugin
                .manifest
                .as_ref()
                .is_some_and(|manifest| manifest.tools.iter().any(|tool| tool.id == tool_id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn discovers_valid_and_invalid_plugins() {
        let index = PluginIndex::discover(&fixtures_dir());
        assert_eq!(index.plugins().len(), 2);
        assert_eq!(index.loaded_count(), 1);

        // 排序键取 id（无效条目取目录名），"broken" < "dev.duck.sample"。
        let invalid = &index.plugins()[0];
        assert!(invalid.manifest.is_none());
        assert!(invalid.error.is_some());
        assert_eq!(invalid.name(), invalid.dir_name());

        let valid = &index.plugins()[1];
        assert_eq!(valid.id(), Some("dev.duck.sample"));
        assert_eq!(valid.name(), "示例插件");
        assert_eq!(valid.manifest.as_ref().map_or(0, |m| m.tools.len()), 2);
    }

    #[test]
    fn missing_dir_yields_empty_index() {
        let index = PluginIndex::discover(Path::new("/nonexistent/duck-plugins"));
        assert!(index.plugins().is_empty());
        assert_eq!(index.loaded_count(), 0);
    }

    #[test]
    fn discover_all_dedupes_by_id_keeping_first() {
        let fixtures = fixtures_dir();
        let index = PluginIndex::discover_all(&[fixtures.clone(), fixtures]);
        assert_eq!(index.plugins().len(), 2);
        assert_eq!(index.loaded_count(), 1);
    }

    #[test]
    fn resolves_tool_owner() {
        let index = PluginIndex::discover(&fixtures_dir());
        assert_eq!(
            index
                .plugin_for_tool("dev.duck.sample.hash")
                .map(DiscoveredPlugin::id),
            Some(Some("dev.duck.sample"))
        );
        assert!(index.plugin_for_tool("video.parse").is_none());
    }

    /// 插件市场的暂存目录（`.tmp-*`）落在插件目录里且带着清单；被发现就会与真身
    /// 撞同一个 id，把已装的那个顶掉。隐藏目录必须一律跳过。
    #[test]
    fn hidden_directories_are_skipped() {
        let root = std::env::temp_dir().join(format!(
            "duck-plugin-hidden-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |delta| delta.as_nanos())
        ));
        let staging = root.join(".tmp-dev.duck.sample");
        std::fs::create_dir_all(&staging).expect("建暂存目录");
        std::fs::write(
            staging.join(MANIFEST_FILE),
            "id = \"dev.duck.sample\"\nname = \"n\"\nversion = \"0.1.0\"\napi_version = 1\nexecution_model = \"js\"\n",
        )
        .expect("写清单");

        let index = PluginIndex::discover(&root);
        assert!(
            index.plugins().is_empty(),
            "隐藏的暂存目录不得被当成插件发现"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
