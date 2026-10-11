//! 静态更新 manifest 的**读取端**。
//!
//! # 文档形状
//!
//! ```json
//! {
//!   "manifest_version": 1,
//!   "channel": "stable",
//!   "version": "0.2.0",
//!   "notes": "改了什么",
//!   "published_at": "2026-10-10T00:00:00Z",
//!   "min_app_version": "0.1.0",
//!   "files": {
//!     "macos-arm64": {
//!       "url": "https://cdn.example.com/v0.2.0/duck-0.2.0-macos-arm64-app.tar.gz",
//!       "mirrors": [
//!         "https://github.com/HanoCode/gpui-toolbox/releases/download/v0.2.0/duck-0.2.0-macos-arm64-app.tar.gz"
//!       ],
//!       "sha256": "<64 位小写十六进制>",
//!       "size": 12345678,
//!       "signature": null
//!     }
//!   }
//! }
//! ```
//!
//! # `url` 与 `mirrors`
//!
//! `url` 是主地址，`mirrors` 是备用地址（可省略，省略即没有备用）。**加速通道就落在
//! 这里**：主地址指向国内可达的 CDN，备用地址指回 GitHub 原址；下载器按顺序试，
//! 全部失败才报错。这样「换一条更快的通道」是一次**只动 manifest** 的操作，
//! 客户端一个字节都不用改。
//!
//! # `manifest_version` 先于一切字段被读出来
//!
//! 先把版本号从原始 JSON 里单独拎出来比对，再反序列化整份文档。直接
//! `serde_json::from_slice` 进 [`Manifest`] 会很乐意地把一份 v2 文档里「恰好还对得上」
//! 的字段读进来——放到这里，就意味着**凭一份半懂的文档去下载并安装**。
//!
//! # 每一条校验的理由
//!
//! - **`sha256` 必须是 64 位十六进制**：截断的或占位符要在 12 MB 动起来**之前**被拦下，
//!   而不是之后。
//! - **`size` 必须非零**：零会让进度计算除零，也会让「读少了」和「读完了」无法区分。
//! - **`url` 必须是绝对 `https://`**：归档会变成用户运行的二进制，明文传输等于把
//!   它交给路径上的任何人，而本该发现这件事的摘要来自同一条连接。在这里而非取用时
//!   拒绝，是为了**任何一条忘了检查的代码路径都碰不到它**。`mirrors` 里的每一条
//!   同样受这条约束——备用通道不是「可以松一点」的地方。
//! - **`version` 必须能解析**：整个判断就是拿它做比较。
//! - **`min_app_version`（可选）必须能解析**：立了闸却写错阈值，等于没闸——
//!   它会悄悄放行它本该拦下的那些版本。写错就地拒绝，不猜。
//!
//! # 不认识的平台键**不是**错误
//!
//! 本版本不认识的键（发布之后才加的平台）会留在表里，[`Manifest::file_for`] 不去取它。
//! 反过来（**本平台缺席**）才是真失败，而且要指名道姓地报——「没给你准备」绝不能
//! 显示成「你不需要」。

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use crate::platform::PlatformKey;
use crate::sha256::is_hex_digest;
use crate::version::{Channel, Version};

/// 本版本认得的最高的 `manifest_version`。
pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// manifest 里的一条可下载归档。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    url: String,
    /// 备用地址，按顺序。主地址取不到时依次再试。
    ///
    /// 加速通道落在这个字段上（主＝近的 CDN，备＝GitHub 原址）。`#[serde(default)]`
    /// 让不带这个键的文档照样读得进来：空列表就是「没有备用」。
    #[serde(default)]
    mirrors: Vec<String>,
    sha256: String,
    size: u64,
    /// 预留给分离式签名。**当前没有任何东西校验它**——还没有签名体系，
    /// 这个字段在已发布的文档里恒为 `null`。读它是为了让将来开始要求签名时
    /// 不必破坏 schema；`#[serde(default)]` 让没有这个键的旧文档仍能读。
    #[serde(default)]
    signature: Option<String>,
}

impl ManifestFile {
    /// 归档的主地址（解析阶段已确认是绝对 `https://`）。
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 备用地址，按顺序；没有备用时为空。
    #[must_use]
    pub fn mirrors(&self) -> &[String] {
        &self.mirrors
    }

    /// 主地址 + 备用地址，**按尝试顺序**。
    ///
    /// 「先主后备」这件事只在这里定义一次；下载器照这个顺序走，不在别处重复这条规则。
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.url.as_str()).chain(self.mirrors.iter().map(String::as_str))
    }

    /// 归档的 SHA-256（小写十六进制）。
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// 归档的字节数（非零）。
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// 预留的签名字段；当前恒为 `None`。
    #[must_use]
    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

/// 整份文档。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    manifest_version: u32,
    channel: Channel,
    version: String,
    notes: String,
    published_at: String,
    /// 能接受这份更新的**最低**应用版本；省略即不设门槛。
    ///
    /// 存在的理由是格式换代：老启动器读不懂新包时会走一条半懂的路，而不是干脆停下。
    /// 发布方在这里立一道闸，客户端自己比对（[`pipeline::decide`](crate::pipeline)），
    /// 低于门槛就明说「先升级启动器」。
    #[serde(default)]
    min_app_version: Option<String>,
    files: BTreeMap<String, ManifestFile>,
}

impl Manifest {
    /// 该文档声明的 `manifest_version`。
    #[must_use]
    pub const fn manifest_version(&self) -> u32 {
        self.manifest_version
    }

    /// 发布通道。
    #[must_use]
    pub const fn channel(&self) -> Channel {
        self.channel
    }

    /// 文档声明的版本串（[`parse`] 已确认它可解析）。
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 该版本的已解析版本号。
    #[must_use]
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version).ok()
    }

    /// 发布说明（Markdown 原样保留，怎么渲染是界面的事）。
    #[must_use]
    pub fn notes(&self) -> &str {
        &self.notes
    }

    /// 发布时间戳（RFC 3339 串）。
    #[must_use]
    pub fn published_at(&self) -> &str {
        &self.published_at
    }

    /// 能接受这份更新的最低应用版本；没立门槛时为 `None`。
    #[must_use]
    pub fn min_app_version(&self) -> Option<&str> {
        self.min_app_version.as_deref()
    }

    /// 某个平台的归档；manifest 没给它准备时为 `None`。
    #[must_use]
    pub fn file_for(&self, platform: PlatformKey) -> Option<&ManifestFile> {
        self.files.get(platform.key())
    }

    /// 文档里出现过的全部平台键（含本版本不认识的）。
    pub fn platform_keys(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }
}

/// manifest 不可用的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// 不是 JSON，或缺了必填字段。`serde_json` 的英文原文保留在内部。
    Malformed(String),
    /// 没有 `manifest_version` 键：无从判断该怎么读它。
    MissingVersion,
    /// 由更新的客户端写的。拒绝，而不是半读。
    UnsupportedVersion {
        /// 文档里写的版本。
        found: u64,
        /// 本客户端支持的版本。
        supported: u32,
    },
    /// `version` 不是语义化版本，没法和任何东西比较。
    UnreadableVersion(String),
    /// `min_app_version` 不是语义化版本：这道闸没法比对，就等于没闸。
    UnreadableMinVersion(String),
    /// 本进程会因此行动的某条 `files` 项不可用。
    InvalidFile {
        /// 出问题的平台键。
        platform: String,
        /// 具体哪里不对。
        problem: FileProblem,
    },
}

/// 单条 `files` 项的问题。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileProblem {
    /// 摘要不是 64 位小写十六进制。
    Digest(String),
    /// 声明的字节数为零。
    ZeroSize,
    /// 地址不是绝对 `https://`。
    InsecureUrl(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "the update manifest is malformed: {detail}"),
            Self::MissingVersion => {
                write!(f, "the update manifest carries no manifest_version")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "the update manifest is version {found}; this build reads up to {supported}"
            ),
            Self::UnreadableVersion(text) => {
                write!(f, "the update manifest offers unreadable version {text:?}")
            }
            Self::UnreadableMinVersion(text) => write!(
                f,
                "the update manifest sets an unreadable min_app_version {text:?}"
            ),
            Self::InvalidFile { platform, problem } => {
                write!(f, "the entry for {platform} is unusable: {problem}")
            }
        }
    }
}

impl fmt::Display for FileProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Digest(digest) => write!(f, "not a SHA-256 digest: {digest:?}"),
            Self::ZeroSize => write!(f, "the archive size is zero"),
            Self::InsecureUrl(url) => write!(f, "not an https url: {url:?}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// 读一份 manifest，凡是本客户端不敢照做的都拒绝。
///
/// # Errors
/// 见 [`ManifestError`]。注意「本平台不在 `files` 里」**不在这里**报——那是
/// [`Manifest::file_for`] 返回 `None` 的事，由调用方转成
/// [`NoArchiveForPlatform`](crate::UpdateError::NoArchiveForPlatform)。
pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| ManifestError::Malformed(err.to_string()))?;

    // 版本闸门跑在原始 JSON 上，先于我们自己的任何字段。
    let found = value
        .get("manifest_version")
        .and_then(Value::as_u64)
        .ok_or(ManifestError::MissingVersion)?;
    if found > u64::from(SUPPORTED_MANIFEST_VERSION) {
        return Err(ManifestError::UnsupportedVersion {
            found,
            supported: SUPPORTED_MANIFEST_VERSION,
        });
    }

    let manifest: Manifest =
        serde_json::from_value(value).map_err(|err| ManifestError::Malformed(err.to_string()))?;

    if Version::parse(&manifest.version).is_err() {
        return Err(ManifestError::UnreadableVersion(manifest.version.clone()));
    }

    // 门槛立了就得立得住：解析不了的阈值只能有两种下场——当成没立（悄悄放行，
    // 正是它要防的那件事），或者在这里拒绝。选后者。
    if let Some(min) = &manifest.min_app_version
        && Version::parse(min).is_err()
    {
        return Err(ManifestError::UnreadableMinVersion(min.clone()));
    }

    // 只校验本版本**认得**的条目；走文档自己的键（而不是 `PlatformKey::ALL`）正是
    // 为了把「这个平台我们认不认」和「它在不在我们表里」区分开。
    for (key, file) in &manifest.files {
        if PlatformKey::parse(key).is_some() {
            validate_file(key, file)?;
        }
    }

    Ok(manifest)
}

fn validate_file(platform: &str, file: &ManifestFile) -> Result<(), ManifestError> {
    let invalid = |problem: FileProblem| ManifestError::InvalidFile {
        platform: platform.to_owned(),
        problem,
    };

    if !is_hex_digest(&file.sha256) {
        return Err(invalid(FileProblem::Digest(file.sha256.clone())));
    }
    if file.size == 0 {
        return Err(invalid(FileProblem::ZeroSize));
    }
    // 主地址与**每一条备用地址**都要过同一关：它们都会变成用户运行的二进制，
    // 备用通道不是可以松一点的地方。
    for url in file.urls() {
        if !url.starts_with("https://") {
            return Err(invalid(FileProblem::InsecureUrl(url.to_owned())));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FileProblem, Manifest, ManifestError, parse};
    use crate::platform::PlatformKey;
    use crate::version::Channel;

    /// 一份形状完整、只留一个平台的文档。形状照真实发布的文档抄，
    /// 这样一旦线上格式变了，这个测试会先失败。
    const VALID: &str = r###"{
      "manifest_version": 1,
      "channel": "stable",
      "version": "0.2.0",
      "notes": "## Duck Toolbox v0.2.0\n\n发布说明",
      "published_at": "2026-10-10T00:00:00Z",
      "files": {
        "macos-arm64": {
          "url": "https://example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
          "mirrors": [
            "https://mirror.example.invalid/duck-0.2.0-macos-arm64-app.tar.gz"
          ],
          "sha256": "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3",
          "size": 11569143,
          "signature": null
        },
        "windows-x64": {
          "url": "https://example.invalid/duck-0.2.0-windows-x64.zip",
          "sha256": "8ac817331932c2b6bbc40d7e770469955e757a184b896b1dcd1ec4a406b4485b",
          "size": 12332862,
          "signature": null
        }
      }
    }"###;

    fn valid() -> Manifest {
        parse(VALID.as_bytes()).expect("the published manifest shape must parse")
    }

    #[test]
    fn reads_a_published_manifest() {
        let manifest = valid();
        assert_eq!(manifest.manifest_version(), 1);
        assert_eq!(manifest.channel(), Channel::Stable);
        assert_eq!(manifest.version(), "0.2.0");
        assert_eq!(
            manifest.parsed_version().map(|v| v.to_string()).as_deref(),
            Some("0.2.0")
        );
        assert!(manifest.notes().contains("发布说明"));
        assert_eq!(manifest.published_at(), "2026-10-10T00:00:00Z");
        assert_eq!(
            manifest.min_app_version(),
            None,
            "no key means no floor, not an empty one"
        );

        let file = manifest
            .file_for(PlatformKey::MacosArm64)
            .expect("macos-arm64 is present");
        assert_eq!(file.size(), 11_569_143);
        assert!(file.url().ends_with("-app.tar.gz"), "{}", file.url());
        assert_eq!(file.signature(), None, "nothing signs anything yet");
    }

    /// 下载顺序只定义一次：主地址在前，备用依次跟上。
    #[test]
    fn urls_are_offered_primary_first_then_mirrors_in_order() {
        let manifest = valid();
        let file = manifest.file_for(PlatformKey::MacosArm64).expect("present");

        assert_eq!(file.mirrors().len(), 1);
        assert_eq!(
            file.urls().collect::<Vec<_>>(),
            vec![
                "https://example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
                "https://mirror.example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
            ]
        );
    }

    /// 没有 `mirrors` 键的条目照样能读——已发布的旧文档就是这个形状。
    #[test]
    fn an_entry_without_mirrors_has_exactly_one_url() {
        let manifest = valid();
        let file = manifest.file_for(PlatformKey::WindowsX64).expect("present");

        assert!(file.mirrors().is_empty());
        assert_eq!(file.urls().count(), 1);
    }

    /// 备用通道走同一道关：明文地址一样会被拒。
    #[test]
    fn a_plaintext_mirror_is_refused_too() {
        let json = VALID.replace(
            "https://mirror.example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
            "http://mirror.example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
        );
        assert!(
            matches!(
                parse(json.as_bytes()),
                Err(ManifestError::InvalidFile {
                    ref platform,
                    problem: FileProblem::InsecureUrl(_)
                }) if platform == "macos-arm64"
            ),
            "a mirror is just as load-bearing as the primary url"
        );
    }

    /// macOS 那条必须指向 `.app` 包而不是裸二进制：安装器换的是整个 bundle。
    #[test]
    fn the_macos_entry_names_the_app_bundle() {
        let manifest = valid();
        let url = manifest
            .file_for(PlatformKey::MacosArm64)
            .expect("present")
            .url();
        assert!(url.ends_with("-app.tar.gz"), "{url}");
    }

    #[test]
    fn a_platform_with_no_entry_is_absent_rather_than_wrong() {
        assert!(valid().file_for(PlatformKey::LinuxX64).is_none());
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let json = r#"{"manifest_version":1,"channel":"stable","version":"9.9.9",
                       "published_at":"x","files":{}}"#;
        assert!(
            matches!(parse(json.as_bytes()), Err(ManifestError::Malformed(_))),
            "a document with no `notes` must not half-parse"
        );
    }

    #[test]
    fn a_document_that_is_not_json_is_refused() {
        assert!(matches!(
            parse(b"<html>404</html>"),
            Err(ManifestError::Malformed(_))
        ));
    }

    #[test]
    fn a_manifest_version_from_the_future_is_refused_rather_than_misread() {
        let json = VALID.replace("\"manifest_version\": 1", "\"manifest_version\": 2");
        assert_eq!(
            parse(json.as_bytes()),
            Err(ManifestError::UnsupportedVersion {
                found: 2,
                supported: 1
            })
        );
    }

    #[test]
    fn a_manifest_with_no_version_key_is_refused() {
        let json = VALID.replace("\"manifest_version\": 1,", "");
        assert_eq!(parse(json.as_bytes()), Err(ManifestError::MissingVersion));
    }

    /// 前向兼容中要紧的那个方向：多一个平台，不能让已经能用的那些挂掉。
    #[test]
    fn an_unknown_platform_key_is_ignored_not_fatal() {
        let json = VALID.replace(
            "\"files\": {",
            r#""files": {
              "freebsd-x64": {
                "url": "https://example.invalid/x", "sha256": "nope", "size": 0, "signature": null
              },"#,
        );
        let manifest = parse(json.as_bytes()).expect("an unknown key must not fail the parse");
        assert!(manifest.file_for(PlatformKey::MacosArm64).is_some());
        assert!(
            manifest.platform_keys().any(|key| key == "freebsd-x64"),
            "kept in the map, just not resolvable"
        );
    }

    #[test]
    fn an_unreadable_version_is_refused() {
        let json = VALID.replace("\"version\": \"0.2.0\"", "\"version\": \"latest\"");
        assert_eq!(
            parse(json.as_bytes()),
            Err(ManifestError::UnreadableVersion("latest".into()))
        );
    }

    #[test]
    fn a_manifest_with_a_floor_reads_it_back() {
        let json = VALID.replace(
            "\"published_at\": \"2026-10-10T00:00:00Z\",",
            "\"published_at\": \"2026-10-10T00:00:00Z\", \"min_app_version\": \"0.1.5\",",
        );
        let manifest = parse(json.as_bytes()).expect("a floor is a normal field");
        assert_eq!(manifest.min_app_version(), Some("0.1.5"));
    }

    /// 阈值写错就地拒绝：放行等于把「立了闸」变成「没闸」。
    #[test]
    fn an_unreadable_floor_is_refused_rather_than_treated_as_absent() {
        let json = VALID.replace(
            "\"published_at\": \"2026-10-10T00:00:00Z\",",
            "\"published_at\": \"2026-10-10T00:00:00Z\", \"min_app_version\": \"any\",",
        );
        assert_eq!(
            parse(json.as_bytes()),
            Err(ManifestError::UnreadableMinVersion("any".into()))
        );
    }

    #[test]
    fn a_malformed_digest_fails_the_parse_not_the_download() {
        let json = VALID.replace(
            "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3",
            "0a404f82",
        );
        let error = parse(json.as_bytes()).expect_err("a truncated digest is unusable");
        assert!(
            matches!(
                error,
                ManifestError::InvalidFile { ref platform, problem: FileProblem::Digest(_) }
                    if platform == "macos-arm64"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_zero_size_is_refused() {
        let json = VALID.replace("\"size\": 11569143", "\"size\": 0");
        assert!(matches!(
            parse(json.as_bytes()),
            Err(ManifestError::InvalidFile {
                problem: FileProblem::ZeroSize,
                ..
            })
        ));
    }

    /// 归档会变成用户运行的二进制。明文在解析期就被拒，这样没有任何取用路径能漏检。
    #[test]
    fn a_plaintext_url_is_refused() {
        let json = VALID.replace(
            "https://example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
            "http://example.invalid/duck-0.2.0-macos-arm64-app.tar.gz",
        );
        assert!(matches!(
            parse(json.as_bytes()),
            Err(ManifestError::InvalidFile {
                problem: FileProblem::InsecureUrl(_),
                ..
            })
        ));
    }

    /// 我们不在的平台上可以有任意内容；为它拒绝整份文档，等于让一行坏数据拖垮所有人。
    #[test]
    fn a_bad_entry_for_a_platform_we_are_not_does_not_block_the_parse() {
        let json = VALID.replace("\"size\": 12332862", "\"size\": 12332862, \"extra\": true");
        assert!(parse(json.as_bytes()).is_ok(), "unknown fields are ignored");
    }
}
