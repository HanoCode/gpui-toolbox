//! 远端插件索引（`index.json`）的读取端。
//!
//! # 文档形状
//!
//! ```json
//! {
//!   "index_version": 1,
//!   "generated_at": "2026-10-11T00:00:00Z",
//!   "plugins": [
//!     {
//!       "id": "dev.duck.text-stat",
//!       "name": "文本统计",
//!       "version": "0.1.0",
//!       "description": "统计字数与行数",
//!       "category": "development",
//!       "tags": ["文本"],
//!       "api_version": 1,
//!       "execution_model": "js",
//!       "author": "Hanocode",
//!       "homepage": "https://github.com/HanoCode/gpui-toolbox",
//!       "capabilities": {
//!         "filesystem_write": ["last-report.txt"],
//!         "notifications": true
//!       },
//!       "archive": {
//!         "url": "https://example.invalid/text-stat-0.1.0.tar.gz",
//!         "mirrors": ["https://backup.example.invalid/text-stat-0.1.0.tar.gz"],
//!         "sha256": "<64 位小写十六进制>",
//!         "size": 4096
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! # `url` 与 `mirrors`
//!
//! `url` 是主地址，`mirrors` 是备用地址（可省略，省略即没有备用）。**加速通道就落在
//! 这里**：发布端把 CDN 地址写成主地址、把 GitHub 原址塞进 `mirrors`，客户端按
//! `url` → `mirrors` 的顺序试，第一个取到的胜出（见 [`crate::install`]）。
//!
//! 备用地址和主地址一样承重——库里那条走不通时全靠它——所以解析期同样只收
//! `https://`，不因为「只是备用的」就放明文进来。
//!
//! # `index_version` 先于一切字段被读出来
//!
//! 与 `duck-update::manifest` 同一取舍：先把版本号从原始 JSON 里单独拎出来比对，
//! 再反序列化整份文档。直接反序列化会很乐意地把一份 v2 文档里「恰好还对得上」的
//! 字段读进来——放到这里，就意味着**凭一份半懂的文档去下载并安装**。
//!
//! # 索引里的元数据只是**展示用**（只有 `capabilities` 例外）
//!
//! 条目里的 name / description 是给「未安装」列表做展示的；真正的权威是解包后
//! `plugin.toml` 里的清单，两者必须一致才允许落盘（见 [`crate::install`]）。
//!
//! `capabilities` 是这条规矩的**唯一例外**：它不止用于展示，还是安装前那份
//! 「这个插件要访问什么」确认框的**唯一依据**（未安装的插件本地没有清单可读）。
//! 于是它同时是**上限**——归档里声明的能力必须是索引声明的子集，多一项就拒绝落盘。
//! 因此它用 [`duck_plugin::Capabilities`] 原样承载（带 `deny_unknown_fields`）：
//! 读不懂的能力类型必须让整份文档失败，而不是被静默丢掉后让确认框漏报。

use semver::Version;
use serde::Deserialize;
use serde_json::Value;

use duck_plugin::Capabilities;

use crate::error::{EntryProblem, RegistryError};
use crate::sha256::is_hex_digest;

/// 本版本认得的最高的 `index_version`。
pub const SUPPORTED_INDEX_VERSION: u32 = 1;

/// 索引里的归档指针。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ArchiveRef {
    url: String,
    /// 备用地址，按声明顺序为准。可省略——省略就是「只有主地址」。
    #[serde(default)]
    mirrors: Vec<String>,
    sha256: String,
    size: u64,
}

impl ArchiveRef {
    /// 主地址（解析阶段已确认是绝对 `https://`）。
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 备用地址（解析阶段同样已确认都是绝对 `https://`）。
    #[must_use]
    pub fn mirrors(&self) -> &[String] {
        &self.mirrors
    }

    /// 可用的地址：**主地址在前，备用地址按声明顺序在后**。
    ///
    /// 下载端照这个顺序试，第一个取到的胜出。抽成一个迭代器而不是让调用方自己拼，
    /// 是为了让「顺序」只有一处定义——顺序写反就成了「绕过 CDN 直连 GitHub」，
    /// 在国内会退化成一分钟也下不动，而这种退化不会报错。
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.url.as_str()).chain(self.mirrors.iter().map(String::as_str))
    }

    /// 归档的 SHA-256（小写十六进制）。
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// 归档字节数（非零）。
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// 索引里的一条插件。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RegistryEntry {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    tags: Vec<String>,
    api_version: u32,
    execution_model: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    homepage: String,
    /// 这个插件声明要访问的宿主资源。缺省即「什么都没声明」。
    ///
    /// 双重身份：既是安装确认框的内容来源，也是归档清单的**授权上限**。
    /// 形状与 `plugin.toml` 的 `[capabilities]` 完全一致（同一个类型）。
    #[serde(default)]
    capabilities: Capabilities,
    archive: ArchiveRef,
}

impl RegistryEntry {
    /// 插件稳定 id。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 展示名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 发布版本串。
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 一句话说明。
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// 分类 id。
    #[must_use]
    pub fn category(&self) -> &str {
        &self.category
    }

    /// 标签。
    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// 清单 API 版本。
    #[must_use]
    pub const fn api_version(&self) -> u32 {
        self.api_version
    }

    /// 执行模型串。
    #[must_use]
    pub fn execution_model(&self) -> &str {
        &self.execution_model
    }

    /// 作者（可空）。
    #[must_use]
    pub fn author(&self) -> &str {
        &self.author
    }

    /// 主页（可空）。
    #[must_use]
    pub fn homepage(&self) -> &str {
        &self.homepage
    }

    /// 声明要访问的宿主资源（索引侧声明的**上限**）。
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// 归档指针。
    #[must_use]
    pub const fn archive(&self) -> &ArchiveRef {
        &self.archive
    }
}

/// 整份索引文档。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RegistryIndex {
    index_version: u32,
    #[serde(default)]
    generated_at: String,
    #[serde(default)]
    plugins: Vec<RegistryEntry>,
}

impl RegistryIndex {
    /// 文档声明的 `index_version`。
    #[must_use]
    pub const fn index_version(&self) -> u32 {
        self.index_version
    }

    /// 生成时间戳（RFC 3339 串，可空）。
    #[must_use]
    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    /// 全部条目。
    #[must_use]
    pub fn plugins(&self) -> &[RegistryEntry] {
        &self.plugins
    }

    /// 按 id 取一条。
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&RegistryEntry> {
        self.plugins.iter().find(|entry| entry.id == id)
    }
}

/// 读一份索引，凡是本客户端不敢照做的都拒绝。
///
/// # Errors
/// 见 [`RegistryError`]。
pub fn parse(bytes: &[u8]) -> Result<RegistryIndex, RegistryError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| RegistryError::Malformed(err.to_string()))?;

    // 版本闸门跑在原始 JSON 上，先于我们自己的任何字段。
    let found = value
        .get("index_version")
        .and_then(Value::as_u64)
        .ok_or(RegistryError::MissingIndexVersion)?;
    if found > u64::from(SUPPORTED_INDEX_VERSION) {
        return Err(RegistryError::UnsupportedIndexVersion {
            found,
            supported: SUPPORTED_INDEX_VERSION,
        });
    }

    let index: RegistryIndex =
        serde_json::from_value(value).map_err(|err| RegistryError::Malformed(err.to_string()))?;

    for entry in &index.plugins {
        validate_entry(entry)?;
    }
    Ok(index)
}

fn validate_entry(entry: &RegistryEntry) -> Result<(), RegistryError> {
    let invalid = |problem: EntryProblem| RegistryError::InvalidEntry {
        id: entry.id.clone(),
        problem,
    };

    if !is_valid_plugin_id(&entry.id) {
        return Err(invalid(EntryProblem::Id(entry.id.clone())));
    }
    if entry.name.trim().is_empty() {
        return Err(invalid(EntryProblem::EmptyName));
    }
    if Version::parse(&entry.version).is_err() {
        return Err(invalid(EntryProblem::UnreadableVersion(
            entry.version.clone(),
        )));
    }
    if entry.api_version != duck_plugin::SUPPORTED_API_VERSION {
        return Err(invalid(EntryProblem::ApiVersion {
            found: entry.api_version,
            supported: duck_plugin::SUPPORTED_API_VERSION,
        }));
    }
    if !matches!(entry.execution_model.as_str(), "js" | "wasm" | "process") {
        return Err(invalid(EntryProblem::ExecutionModel(
            entry.execution_model.clone(),
        )));
    }
    if !entry.archive.url.starts_with("https://") {
        return Err(invalid(EntryProblem::InsecureUrl(
            entry.archive.url.clone(),
        )));
    }
    if let Some(plaintext) = entry
        .archive
        .mirrors
        .iter()
        .find(|mirror| !mirror.starts_with("https://"))
    {
        // 备用地址和主地址一样承重：主地址走不通时，整包字节就是从它下来的。
        // 「只是备用的」不是放宽检查的理由。
        return Err(invalid(EntryProblem::InsecureUrl(plaintext.clone())));
    }
    if !is_hex_digest(&entry.archive.sha256) {
        return Err(invalid(EntryProblem::Digest(entry.archive.sha256.clone())));
    }
    if entry.archive.size == 0 {
        return Err(invalid(EntryProblem::ZeroSize));
    }
    Ok(())
}

/// 插件 id 规则：非空，仅 `a-z 0-9 . -`，不以 `.` / `-` 开头或结尾，长度 ≤ 128。
///
/// 与 `duck-plugin` 的清单校验同源；这里再判一次是为了**在下载之前**就挡掉可用于
/// 路径穿越的 id（解包目录名直接取自它）。
#[must_use]
pub fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !id.starts_with(['.', '-'])
        && !id.ends_with(['.', '-'])
        && !id.contains("..")
}

#[cfg(test)]
mod tests {
    use super::{RegistryError, RegistryIndex, is_valid_plugin_id, parse};

    /// 形状完整的一行。形状照真实索引抄，一旦线上格式变了，这个测试会先失败。
    const VALID: &str = r#"{
      "index_version": 1,
      "generated_at": "2026-10-11T00:00:00Z",
      "plugins": [{
        "id": "dev.duck.text-stat",
        "name": "文本统计",
        "version": "0.1.0",
        "description": "统计字数与行数",
        "category": "development",
        "tags": ["文本"],
        "api_version": 1,
        "execution_model": "js",
        "author": "Hanocode",
        "homepage": "https://github.com/HanoCode/gpui-toolbox",
        "capabilities": {
          "filesystem_write": ["last-report.txt"],
          "notifications": true
        },
        "archive": {
          "url": "https://example.invalid/text-stat-0.1.0.tar.gz",
          "mirrors": ["https://mirror.example.invalid/text-stat-0.1.0.tar.gz"],
          "sha256": "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3",
          "size": 4096
        }
      }]
    }"#;

    #[test]
    fn reads_a_published_index() {
        let index = parse(VALID.as_bytes()).expect("the published index shape must parse");
        assert_eq!(index.index_version(), 1);
        assert_eq!(index.generated_at(), "2026-10-11T00:00:00Z");
        assert_eq!(index.plugins().len(), 1);
        let entry = index.get("dev.duck.text-stat").expect("present");
        assert_eq!(entry.name(), "文本统计");
        assert_eq!(entry.api_version(), 1);
        assert_eq!(entry.archive().size(), 4096);
        assert!(entry.archive().url().ends_with(".tar.gz"));
        // 写端总是把 `mirrors` 写出来（没有镜像时是空表），这条钉住它不会悄悄消失。
        assert_eq!(entry.archive().mirrors().len(), 1);
    }

    /// 备用地址按声明顺序排在主地址之后。这个顺序只有一个定义处，而它写反了不会
    /// 报错——只会让国内用户绕过 CDN 去直连 GitHub，慢到以为插件坏了。
    #[test]
    fn mirrors_come_after_the_primary_url_in_declared_order() {
        let json = VALID.replace(
            r#""mirrors": ["https://mirror.example.invalid/text-stat-0.1.0.tar.gz"]"#,
            r#""mirrors": ["https://b.example.invalid/p.tar.gz", "https://a.example.invalid/p.tar.gz"]"#,
        );
        let index = parse(json.as_bytes()).expect("parses");
        let archive = index.get("dev.duck.text-stat").expect("present").archive();
        assert_eq!(archive.mirrors().len(), 2);
        assert_eq!(
            archive.urls().collect::<Vec<_>>(),
            [
                "https://example.invalid/text-stat-0.1.0.tar.gz",
                "https://b.example.invalid/p.tar.gz",
                "https://a.example.invalid/p.tar.gz",
            ],
            "primary first, then mirrors in the order they were declared"
        );
    }

    /// 没有 `mirrors` 键的条目照样能读——早于镜像通道发布的索引就是这个形状。
    ///
    /// 用 `serde_json` 摘字段而不是字符串替换：这里的重点是「字段缺席」这个语义。
    #[test]
    fn an_entry_without_mirrors_has_exactly_one_url() {
        let mut value: serde_json::Value = serde_json::from_str(VALID).expect("valid json");
        value["plugins"][0]["archive"]
            .as_object_mut()
            .expect("archive is an object")
            .remove("mirrors");
        let json = serde_json::to_string(&value).expect("reserialize");

        let index = parse(json.as_bytes()).expect("parses");
        let archive = index.get("dev.duck.text-stat").expect("present").archive();
        assert!(archive.mirrors().is_empty());
        assert_eq!(archive.urls().count(), 1);
    }

    /// 备用地址和主地址一样承重：主地址走不通时，整包字节就是从它下来的。
    /// 「只是备用的」不是放宽检查的理由。
    #[test]
    fn a_plaintext_mirror_is_refused_too() {
        let json = VALID.replace(
            "https://mirror.example.invalid/text-stat-0.1.0.tar.gz",
            "http://mirror.example.invalid/text-stat-0.1.0.tar.gz",
        );
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    /// 索引里的能力声明就是安装确认框的内容、也是归档清单的授权上限，
    /// 所以它必须被**完整**读出来，而不是「大概知道有这回事」。
    #[test]
    fn reads_the_declared_capabilities() {
        let index = parse(VALID.as_bytes()).expect("parses");
        let caps = index
            .get("dev.duck.text-stat")
            .expect("present")
            .capabilities();
        assert_eq!(caps.filesystem_write, ["last-report.txt"]);
        assert!(caps.notifications);
        assert!(caps.filesystem_read.is_empty());
        assert!(caps.network_hosts.is_empty());
        assert!(caps.process_exec.is_empty());
    }

    /// 不写 `capabilities` 的条目是合法的（早期索引 / 不请求任何能力）。
    ///
    /// 用 `serde_json` 摘字段而不是字符串替换：这里的重点是「字段缺席」这个语义，
    /// 不该被缩进或换行的改动弄坏。
    #[test]
    fn a_missing_capabilities_block_means_none_declared() {
        let mut value: serde_json::Value = serde_json::from_str(VALID).expect("valid json");
        value["plugins"][0]
            .as_object_mut()
            .expect("entry is an object")
            .remove("capabilities");
        let json = serde_json::to_string(&value).expect("reserialize");

        let index = parse(json.as_bytes()).expect("parses");
        assert!(
            index
                .get("dev.duck.text-stat")
                .expect("present")
                .capabilities()
                .is_empty()
        );
    }

    /// 客户端读不懂的能力类型绝不能静默忽略：漏报的能力会直接变成
    /// 「用户以为插件只要通知权，实际它还要网络」——确认框就失去意义了。
    #[test]
    fn an_unknown_capability_kind_is_refused_not_ignored() {
        let json = VALID.replace(
            "\"notifications\": true",
            "\"notifications\": true, \"kernel_modules\": [\"x\"]",
        );
        assert!(
            matches!(parse(json.as_bytes()), Err(RegistryError::Malformed(_))),
            "an unknown capability kind must fail the whole document"
        );
    }

    #[test]
    fn a_missing_index_version_is_refused() {
        let json = VALID.replace("\"index_version\": 1,", "");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::MissingIndexVersion)
        ));
    }

    #[test]
    fn an_index_version_from_the_future_is_refused_rather_than_misread() {
        let json = VALID.replace("\"index_version\": 1", "\"index_version\": 2");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::UnsupportedIndexVersion {
                found: 2,
                supported: 1
            })
        ));
    }

    #[test]
    fn a_document_that_is_not_json_is_refused() {
        assert!(matches!(
            parse(b"<html>404</html>"),
            Err(RegistryError::Malformed(_))
        ));
    }

    /// 归档会变成用户运行的脚本与资源。明文在解析期就被拒，没有任何取用路径能漏检。
    #[test]
    fn a_plaintext_archive_url_is_refused() {
        let json = VALID.replace(
            "https://example.invalid/text-stat",
            "http://example.invalid/x",
        );
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    #[test]
    fn a_truncated_digest_is_refused() {
        let json = VALID.replace(
            "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3",
            "0a404f82",
        );
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    #[test]
    fn a_zero_size_is_refused() {
        let json = VALID.replace("\"size\": 4096", "\"size\": 0");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    #[test]
    fn an_unsupported_api_version_is_refused() {
        let json = VALID.replace("\"api_version\": 1", "\"api_version\": 2");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    #[test]
    fn an_unknown_execution_model_is_refused() {
        let json = VALID.replace(
            "\"execution_model\": \"js\"",
            "\"execution_model\": \"lua\"",
        );
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    /// 版本不是语义化版本，就没法和本地比较，也就没法判「可更新」。
    #[test]
    fn an_unreadable_version_is_refused() {
        let json = VALID.replace("\"version\": \"0.1.0\"", "\"version\": \"latest\"");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(RegistryError::InvalidEntry { .. })
        ));
    }

    #[test]
    fn an_unknown_field_is_ignored_not_fatal() {
        let json = VALID.replace("\"plugins\": [", "\"new_field\": true,\n  \"plugins\": [");
        assert!(parse(json.as_bytes()).is_ok(), "forward compatibility");
    }

    #[test]
    fn plugin_ids_follow_the_manifest_rules() {
        assert!(is_valid_plugin_id("dev.duck.text-stat"));
        for id in [
            "", ".duck", "duck.", "duck..x", "DEV", "dev duck", "x-", "-x",
        ] {
            assert!(!is_valid_plugin_id(id), "{id:?} must be rejected");
        }
    }

    /// 空索引是合法文档：远端刚建起来还没有插件。
    #[test]
    fn an_empty_index_is_legal() {
        let index = parse(br#"{"index_version":1}"#).expect("an empty index must parse");
        assert!(index.plugins().is_empty());
    }

    /// 解析出的类型必须能放进全局状态里长期持有。
    #[test]
    fn the_index_is_cloneable() {
        let index: RegistryIndex = parse(VALID.as_bytes()).expect("parses");
        let copy = index.clone();
        assert_eq!(copy.plugins().len(), index.plugins().len());
    }
}
