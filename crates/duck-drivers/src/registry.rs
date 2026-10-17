//! 远端注册表：**哪些驱动有得下、下的这一版是什么**。
//!
//! 形状照搬 dbx（Apache-2.0）的 `agent-registry.json`：
//!
//! ```json
//! {
//!   "jres":    { "21": { "version": "...", "platforms": { "macos-aarch64": {…} } } },
//!   "drivers": { "dameng": { "version": "0.1.92", "label": "达梦 DM8", "jre": "21",
//!                            "jar":  {…}, "native": { "macos-aarch64": {…} } } }
//! }
//! ```
//!
//! 每个工件（[`ArtifactInfo`]）带 `url` / `sha256` / `size` / `format`，可选 `delta`
//! （zstd 增量补丁）。**我们只走全量下载**：`delta` 解析出来但不用 —— 增量要求本地
//! 留着上一版的工件当补丁基底，省下的那点流量换不来一套「基底对不上就静默重下」的
//! 状态机。字段保留是为了**不丢信息**（将来要做时不必改解析）。
//!
//! # 缓存
//!
//! 按下载源缓存 10 分钟：驱动商店一开就拉一次，用户连点几个「安装」不该每次
//! 都去 GitHub 走一趟。缓存是**进程内**的（`OnceLock`），不落盘 —— 落盘就等于多一份
//! 会和远端漂移的「真源」。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::DownloadSource;
use crate::Result;

/// dbx 的注册表直链（官方源）。
pub const REGISTRY_URL: &str =
    "https://github.com/t8y2/dbx/releases/download/agents-latest/agent-registry.json";

/// 官方源在 CDN 上的路径（镜像源优先取它）。
pub const REGISTRY_CDN_PATH: &str = "agents/agent-registry.json";

/// 缓存有效期。
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// 注册表本体大小上限：真实值 ~60 KB，给 4 MiB 是防一个跳转把内存吃满。
const REGISTRY_LIMIT: u64 = 4 * 1024 * 1024;

/// 工件的归档格式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    /// `tar` 经 zstd 压缩（当前全部工件都是这个，所以是缺省值）。
    #[default]
    TarZstd,
}

/// 一份可下载的工件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    /// 下载地址（https）。
    pub url: String,
    /// SHA-256（小写十六进制）。注册表理论上可以不给，那时只能靠体积把关。
    #[serde(default)]
    pub sha256: Option<String>,
    /// 字节数。
    pub size: u64,
    /// 归档格式；缺省 = `tar.gz`（dbx 的历史形态）。
    #[serde(default)]
    pub format: Option<ArtifactFormat>,
    /// 增量补丁（解析保留，当前不使用）。
    #[serde(default)]
    pub delta: Option<DeltaInfo>,
}

/// 增量更新补丁的描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaInfo {
    /// 打补丁的基底版本。
    pub base_version: String,
    /// 补丁地址。
    pub url: String,
    /// 补丁自身的 SHA-256。
    pub sha256: String,
    /// 补丁字节数。
    pub size: u64,
}

/// 一条驱动的远端信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    /// 版本（注册表口径，字符串比较，不解析语义化版本）。
    pub version: String,
    /// 展示名（**中文源串**，界面过 `tr`）。
    #[serde(default)]
    pub label: String,
    /// 需要的最低宿主版本 —— 对齐 dbx 用的，本仓不解释（我们不是 dbx，这个字段
    /// 只作为信息保留，不影响安装决策）。
    #[serde(default)]
    pub min_app_version: String,
    /// 该驱动要用的 JRE 键（如 `"21"`）。
    #[serde(default)]
    pub jre: String,
    /// **jar 轨**工件：需要 JVM 才能跑。
    #[serde(default)]
    pub jar: Option<ArtifactInfo>,
    /// **native 轨**工件：按平台键给的独立可执行文件。
    #[serde(default)]
    pub native: HashMap<String, ArtifactInfo>,
    /// 是否还需要用户自备外部 JDBC 驱动（如某些数据库的官方 jar）。
    #[serde(default)]
    pub external_driver_required: bool,
}

impl DriverInfo {
    /// 装哪一条：**优先 native**（不依赖 JVM、启动快），没有本平台的原生件才回落 jar。
    ///
    /// 与 dbx 的策略一致。两条都没有时返回 `None` —— 调用方报「本平台没有工件」，
    /// 而不是退而装一个跑不起来的 jar。
    #[must_use]
    pub fn artifact_for(&self, platform: Option<&str>) -> Option<ResolvedArtifact<'_>> {
        if let Some(platform) = platform
            && let Some(info) = self.native.get(platform)
        {
            return Some(ResolvedArtifact {
                kind: ArtifactKind::Native,
                info,
            });
        }
        self.jar.as_ref().map(|info| ResolvedArtifact {
            kind: ArtifactKind::Jar,
            info,
        })
    }

    /// 本平台是否只有 jar 轨（界面据此提示「需要 Java 运行时」）。
    #[must_use]
    pub fn needs_jre(&self, platform: Option<&str>) -> bool {
        matches!(
            self.artifact_for(platform).map(|resolved| resolved.kind),
            Some(ArtifactKind::Jar)
        )
    }

    /// 该驱动是否需要托管 JRE（jar 轨且注册表没给 native 件时，装之前得先有 JVM）。
    #[must_use]
    pub fn jre_key(&self) -> Option<&str> {
        (!self.jre.is_empty()).then_some(self.jre.as_str())
    }
}

/// 工件的轨别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// 独立可执行文件。
    Native,
    /// 需要 JVM 的 jar。
    Jar,
}

/// 选定的工件。
#[derive(Debug, Clone, Copy)]
pub struct ResolvedArtifact<'a> {
    /// 轨别。
    pub kind: ArtifactKind,
    /// 工件描述。
    pub info: &'a ArtifactInfo,
}

/// JRE 的远端信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JreInfo {
    /// 版本串（如 `21.0.12+kerberos.ec.2`）。
    #[serde(default)]
    pub version: String,
    /// 按平台键给的工件。
    #[serde(default)]
    pub platforms: HashMap<String, ArtifactInfo>,
}

/// 注册表本体。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverRegistry {
    /// 全部 JRE（键如 `"21"`）。
    #[serde(default)]
    pub jres: HashMap<String, JreInfo>,
    /// 全部驱动。
    #[serde(default)]
    pub drivers: HashMap<String, DriverInfo>,
    /// 旧版注册表把单个 JRE 放在顶层 `jre` 字段（dbx 的兼容字段）。
    #[serde(default)]
    pub jre: Option<JreInfo>,
}

impl DriverRegistry {
    /// 解析注册表 JSON。
    ///
    /// # Errors
    /// JSON 语法错、或结构不符（`drivers` 不是对象）时返回
    /// [`DriverError::Format`](crate::DriverError::Format)。
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|error| {
            crate::DriverError::Format(format!("registry json is not readable: {error}"))
        })
    }

    /// 从磁盘上的注册表文件读（离线包导入用）。
    ///
    /// # Errors
    /// 读盘失败或解析失败。
    pub fn read_file(path: &std::path::Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", path.display())))?;
        Self::parse(&bytes)
    }

    /// 取某条驱动的远端信息。
    #[must_use]
    pub fn driver(&self, key: &str) -> Option<&DriverInfo> {
        self.drivers.get(key)
    }

    /// 取某个 JRE 键的信息（`jres` 里没有就回落顶层旧字段）。
    #[must_use]
    pub fn jre(&self, key: &str) -> Option<&JreInfo> {
        self.jres.get(key).or(self.jre.as_ref())
    }

    /// 本平台有工件、且能装的全部驱动键（按驱动目录的顺序）。
    #[must_use]
    pub fn installable(&self, platform: Option<&str>) -> Vec<&str> {
        crate::catalog::catalog()
            .iter()
            .filter(|entry| {
                self.drivers
                    .get(entry.key)
                    .is_some_and(|info| info.artifact_for(platform).is_some())
            })
            .map(|entry| entry.key)
            .collect()
    }
}

/// 注册表缓存（进程内，按下载源各存一份）。
fn cache() -> &'static std::sync::Mutex<HashMap<DownloadSource, (Instant, DriverRegistry)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<DownloadSource, (Instant, DriverRegistry)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 拉注册表（带缓存）。
///
/// `force` 为真时跳过缓存（界面上的「刷新」按钮）。
///
/// # Errors
/// [拉注册表失败](fetch_registry) 的各种原因。
pub fn registry(source: DownloadSource, force: bool) -> Result<DriverRegistry> {
    if !force
        && let Ok(guard) = cache().lock()
        && let Some((at, registry)) = guard.get(&source)
        && at.elapsed() < CACHE_TTL
    {
        return Ok(registry.clone());
    }

    let registry = fetch_registry(source)?;
    if let Ok(mut guard) = cache().lock() {
        guard.insert(source, (Instant::now(), registry.clone()));
    }
    Ok(registry)
}

/// 拉一次注册表（不走缓存）。
///
/// # Errors
/// 每个候选地址都失败时报最后一个错误；解析失败报
/// [`DriverError::Format`](crate::DriverError::Format)。
pub fn fetch_registry(source: DownloadSource) -> Result<DriverRegistry> {
    let urls = crate::source::registry_candidate_urls(source);
    let bytes = crate::download::fetch_small(&urls, REGISTRY_LIMIT, "driver registry")?;
    DriverRegistry::parse(&bytes)
}

/// 把上游的注册表缓存清掉（测试与「刷新」用）。
pub fn clear_cache() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "jres": { "21": { "version": "21.0.12", "platforms": {
        "macos-aarch64": { "url": "https://example.com/jre21.tar.zst", "sha256": "aa", "size": 10, "format": "tar_zstd" }
      } } },
      "drivers": {
        "dameng": { "version": "0.1.92", "label": "达梦 DM8", "jre": "21",
          "jar": { "url": "https://example.com/dameng.tar.zst", "sha256": "bb", "size": 20, "format": "tar_zstd" } },
        "oracle": { "version": "0.1.71", "label": "Oracle", "jre": "21",
          "native": { "macos-aarch64": { "url": "https://example.com/oracle.tar.zst", "sha256": "cc", "size": 30, "format": "tar_zstd" } },
          "jar": { "url": "https://example.com/oracle.jar.tar.zst", "sha256": "dd", "size": 31, "format": "tar_zstd" } }
      }
    }"#;

    #[test]
    fn parses_the_upstream_shape() {
        let registry = DriverRegistry::parse(SAMPLE.as_bytes()).expect("样例可解析");
        assert_eq!(registry.drivers.len(), 2);
        assert_eq!(registry.jre("21").expect("JRE 21").version, "21.0.12");
        let dameng = registry.driver("dameng").expect("dameng 在表里");
        assert_eq!(dameng.version, "0.1.92");
        assert_eq!(dameng.label, "达梦 DM8");
        assert!(dameng.jar.is_some());
        assert!(dameng.native.is_empty());
    }

    /// native 优先、jar 回落、两条都没有就是 `None`。
    #[test]
    fn artifact_selection_prefers_native_then_falls_back_to_jar() {
        let registry = DriverRegistry::parse(SAMPLE.as_bytes()).expect("样例可解析");
        let oracle = registry.driver("oracle").expect("oracle 在表里");
        assert_eq!(
            oracle.artifact_for(Some("macos-aarch64")).map(|r| r.kind),
            Some(ArtifactKind::Native)
        );
        assert_eq!(
            oracle.artifact_for(Some("windows-x64")).map(|r| r.kind),
            Some(ArtifactKind::Jar),
            "本平台没有原生件时应回落 jar"
        );
        assert!(!oracle.needs_jre(Some("macos-aarch64")));
        assert!(oracle.needs_jre(Some("windows-x64")));

        let dameng = registry.driver("dameng").expect("dameng 在表里");
        assert_eq!(
            dameng.artifact_for(None).map(|r| r.kind),
            Some(ArtifactKind::Jar)
        );
    }

    /// 结构不符要报 `Format`，不能静默当成空表（那会让商店显示「一条驱动都没有」）。
    #[test]
    fn malformed_registry_is_a_format_error() {
        let error = DriverRegistry::parse(b"{ not json").expect_err("坏 JSON 必须报错");
        assert!(matches!(error, crate::DriverError::Format(_)), "{error:?}");
        let error = DriverRegistry::parse(br#"{"drivers": []}"#).expect_err("类型不符必须报错");
        assert!(matches!(error, crate::DriverError::Format(_)), "{error:?}");
    }

    /// 老注册表（顶层 `jre`）仍然能取到 JRE 信息。
    #[test]
    fn legacy_single_jre_still_resolves() {
        let legacy = br#"{"jre": {"version": "21.0.1", "platforms": {}}, "drivers": {}}"#;
        let registry = DriverRegistry::parse(legacy).expect("旧形状可解析");
        assert_eq!(registry.jre("21").expect("回落顶层 jre").version, "21.0.1");
    }

    /// delta 字段解析出来（当前不用，但解析不能丢）。
    #[test]
    fn delta_is_parsed_even_though_we_do_full_downloads() {
        let with_delta = br#"{"drivers": {"x": {"version": "2", "jar": {
            "url": "https://e/x", "size": 5,
            "delta": {"base_version": "1", "url": "https://e/x.delta", "sha256": "zz", "size": 3} }}}}"#;
        let registry = DriverRegistry::parse(with_delta).expect("可解析");
        let delta = registry
            .driver("x")
            .and_then(|info| info.jar.as_ref())
            .and_then(|jar| jar.delta.as_ref())
            .expect("delta 已解析");
        assert_eq!(delta.base_version, "1");
        assert_eq!(delta.size, 3);
    }
}
