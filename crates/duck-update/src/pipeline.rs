//! 把「查 → 下 → 校验 → 装」串起来。
//!
//! # 为什么查和装是两个函数
//!
//! 因为「没点头就不下载」这条规矩要能被结构性保证，而不是靠调用方自觉。
//! [`check`] 只取一份几 KB 的 JSON 然后给出结论，一个字节的归档都不会动；
//! [`install`] 是另一个函数，只有用户确认之后才轮得到它。
//!
//! [`stage`] 与 [`apply`] 是 [`install`] 拆出来的两半，这样「用一份已校验的归档完成安装」
//! 这件事可以脱离网络被测到。

use std::path::{Path, PathBuf};

use crate::download::{self, Flow};
use crate::error::UpdateError;
use crate::install::{InstallOutcome, ManualReason, PlatformInstaller};
use crate::manifest::{self, Manifest, ManifestFile};
use crate::platform::PlatformKey;
use crate::version::{Version, compiled_version, is_newer};

/// 取一份文档的原始字节。默认实现走 HTTPS；测试用它注入一份本地 JSON。
pub type Fetcher = dyn Fn(&str) -> Result<Vec<u8>, UpdateError> + Send + Sync;

/// 一次检查 / 安装的全部输入。
#[derive(Clone, Debug)]
pub struct UpdateConfig {
    manifest_url: String,
    download_dir: PathBuf,
    platform: Option<PlatformKey>,
    current: Version,
}

impl UpdateConfig {
    /// 指向 `manifest_url`、把归档下到 `download_dir`。
    ///
    /// 平台与「当前版本」默认取本进程的真实值，需要时用 builder 覆盖。
    #[must_use]
    pub fn new(manifest_url: impl Into<String>, download_dir: impl Into<PathBuf>) -> Self {
        Self {
            manifest_url: manifest_url.into(),
            download_dir: download_dir.into(),
            platform: PlatformKey::current(),
            current: compiled_version(),
        }
    }

    /// 覆盖平台（默认取当前进程所在的平台）。
    #[must_use]
    pub fn platform(mut self, platform: Option<PlatformKey>) -> Self {
        self.platform = platform;
        self
    }

    /// 覆盖「当前版本」（默认取本二进制编译时的版本）。
    #[must_use]
    pub fn current(mut self, current: Version) -> Self {
        self.current = current;
        self
    }

    /// 更新源地址。
    #[must_use]
    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    /// 归档下载目录。
    #[must_use]
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// 目标平台。
    #[must_use]
    pub const fn target_platform(&self) -> Option<PlatformKey> {
        self.platform
    }

    /// 当作「当前版本」的那个版本。
    #[must_use]
    pub const fn current_version(&self) -> &Version {
        &self.current
    }
}

/// 一次检查的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// 已经是最新（或比源上更新）。
    UpToDate {
        /// 本程序当前版本。
        current: Version,
        /// 更新源上的版本。
        latest: Version,
        /// 源上版本的发布说明（Markdown 原样）。
        ///
        /// 一并带出，是为了让「关于」页在**已是最新**时也能展示发布记录，
        /// 而不是只剩一句占位提示。
        notes: String,
    },
    /// 有可用的更新。
    Available(UpdateInfo),
}

impl Check {
    /// 有更新时给出它的信息。
    #[must_use]
    pub fn available(&self) -> Option<&UpdateInfo> {
        match self {
            Self::UpToDate { .. } => None,
            Self::Available(info) => Some(info),
        }
    }

    /// 源上版本的发布说明；manifest 里没写（空串）时为 `None`。
    #[must_use]
    pub fn notes(&self) -> Option<&str> {
        let notes = match self {
            Self::UpToDate { notes, .. } => notes,
            Self::Available(info) => info.notes(),
        };
        (!notes.trim().is_empty()).then_some(notes)
    }
}

/// 待安装的更新。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    version: Version,
    notes: String,
    published_at: String,
    file: ManifestFile,
    platform: PlatformKey,
}

impl UpdateInfo {
    /// 源上那个版本。
    #[must_use]
    pub const fn version(&self) -> &Version {
        &self.version
    }

    /// 发布说明（Markdown 原样）。
    #[must_use]
    pub fn notes(&self) -> &str {
        &self.notes
    }

    /// 发布时间戳串。
    #[must_use]
    pub fn published_at(&self) -> &str {
        &self.published_at
    }

    /// 归档的字节数。
    #[must_use]
    pub fn size(&self) -> u64 {
        self.file.size()
    }

    /// 归档地址。
    #[must_use]
    pub fn url(&self) -> &str {
        self.file.url()
    }

    /// 归档的期望摘要。
    #[must_use]
    pub fn sha256(&self) -> &str {
        self.file.sha256()
    }

    /// 这条更新是给哪个平台的。
    #[must_use]
    pub const fn platform(&self) -> PlatformKey {
        self.platform
    }
}

/// 安装途中向调用方汇报的进度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateEvent {
    /// 正在下载。
    Downloading {
        /// 已收字节。
        downloaded: u64,
        /// 总字节（manifest 承诺的）。
        total: u64,
    },
    /// 正在重新读盘校验。
    Verifying,
    /// 正在替换安装。
    Installing,
    /// 装好了，重启即新版本。
    Installed,
    /// 归档下好也校验过了，但这台机器无法就地更新。
    Manual {
        /// 为什么不能就地装。
        reason: ManualReason,
        /// 已验证的归档留在哪。
        archive: PathBuf,
    },
}

/// 查一次更新。
///
/// **不会下载任何归档**，只取 manifest。返回
/// [`Check::UpToDate`] 也可能是「源上比你旧」。
///
/// # Errors
/// 取不到 manifest、文档读不懂、当前平台没有对应条目
/// （[`UpdateError::NoArchiveForPlatform`]——这条**不能**被当成「已是最新」）。
pub fn check(config: &UpdateConfig) -> Result<Check, UpdateError> {
    check_with(config, &download::fetch)
}

/// [`check`] 的可注入版本：`fetch` 决定 manifest 从哪来。
///
/// # Errors
/// 同 [`check`]。
pub fn check_with(config: &UpdateConfig, fetch: &Fetcher) -> Result<Check, UpdateError> {
    let bytes = fetch(config.manifest_url())?;
    verify_signature(config.manifest_url(), &bytes, fetch)?;
    let manifest = manifest::parse(&bytes)?;
    decide(&manifest, config)
}

/// 签名关卡：这份构建内建了公钥时，manifest 必须附上一份**验得过**的 detached 签名。
///
/// 排在 `parse` **之前**是刻意的——不该先花力气去读一份我们未必采信的文档。
/// 反过来，这一关排在取回 manifest 之后、任何下载之前，所以一份没签名的文档
/// 不会让哪怕一个字节的归档动起来。
///
/// # Errors
/// 签名取不到、读不懂、或验不过。
fn verify_signature(url: &str, bytes: &[u8], fetch: &Fetcher) -> Result<(), UpdateError> {
    if !crate::signature::required() {
        return Ok(());
    }

    let detached = fetch(&signature_url(url))?;
    let text = String::from_utf8(detached)
        .map_err(|_| UpdateError::Signature("the signature is not text".to_owned()))?;
    crate::signature::verify_builtin(bytes, &text)
}

/// 签名文件放在 manifest 旁边：`update.json` → `update.json.sig`。
///
/// 约定成「同一个地址加后缀」而不是在 manifest 里写一个字段，是因为**签名不能写在
/// 它要保护的那份文档里**——那会变成自引用，验签前就得先信任一半内容。
fn signature_url(manifest_url: &str) -> String {
    format!("{manifest_url}.sig")
}

/// 由一份**已解析**的 manifest 与当前配置得出结论。纯函数，便于测。
fn decide(manifest: &Manifest, config: &UpdateConfig) -> Result<Check, UpdateError> {
    let Some(platform) = config.target_platform() else {
        return Err(UpdateError::UnsupportedPlatform);
    };

    // 本平台缺席是真失败，而且要指名道姓地报：把「没给你准备」显示成「你不需要」
    // 是这里最不能犯的错。
    let Some(file) = manifest.file_for(platform) else {
        return Err(UpdateError::NoArchiveForPlatform(platform.key()));
    };

    let latest = manifest.parsed_version().ok_or_else(|| {
        UpdateError::Manifest(manifest::ManifestError::UnreadableVersion(
            manifest.version().to_owned(),
        ))
    })?;

    if !is_newer(&latest, config.current_version()) {
        return Ok(Check::UpToDate {
            current: config.current_version().clone(),
            latest,
            notes: manifest.notes().to_owned(),
        });
    }

    // 门槛只在「确实有更新可装」时才拦：没有更新可装的时候，能不能装是无所谓的。
    // 这一步排在 `is_newer` 之后，正是为了让「已是最新」保持它原本的样子。
    if let Some(required) = manifest.min_app_version() {
        // `parse` 已经确认它能解析；这里再取一次是为了不必把 `Option<Version>` 塞进
        // `Manifest`（那会让「原始串」与「解析结果」两份事实并存，迟早不一致）。
        let floor = Version::parse(required).map_err(|_| {
            UpdateError::Manifest(manifest::ManifestError::UnreadableMinVersion(
                required.to_owned(),
            ))
        })?;
        if is_newer(&floor, config.current_version()) {
            return Err(UpdateError::AppTooOld {
                current: config.current_version().to_string(),
                required: required.to_owned(),
            });
        }
    }

    Ok(Check::Available(UpdateInfo {
        version: latest,
        notes: manifest.notes().to_owned(),
        published_at: manifest.published_at().to_owned(),
        file: file.clone(),
        platform,
    }))
}

/// 下载 + 校验 + 安装。
///
/// `sink` 在下载途中反复被调用，它的回答决定是否中止（返回 [`Flow::Abort`] 时
/// 会删掉半份归档并返回 [`UpdateError::Cancelled`]）。
///
/// # Errors
/// 下载、校验、安装任一步失败；用户中止。
pub fn install(
    config: &UpdateConfig,
    info: &UpdateInfo,
    installer: &dyn PlatformInstaller,
    sink: &dyn Fn(UpdateEvent) -> Flow,
) -> Result<InstallOutcome, UpdateError> {
    let archive = stage(config, info, sink)?;
    apply(&archive, installer, sink)
}

/// 把 `info` 的归档下到本地并校验，返回它的路径。
///
/// # Errors
/// 下载失败、用户中止、大小或摘要对不上。
pub fn stage(
    config: &UpdateConfig,
    info: &UpdateInfo,
    sink: &dyn Fn(UpdateEvent) -> Flow,
) -> Result<PathBuf, UpdateError> {
    let destination = config.download_dir().join(archive_name(info));

    let progress =
        |downloaded: u64, total: u64| sink(UpdateEvent::Downloading { downloaded, total });
    download::stream(&info.file, &destination, &progress)?;

    if sink(UpdateEvent::Verifying) == Flow::Abort {
        let _ = std::fs::remove_file(&destination);
        return Err(UpdateError::Cancelled);
    }

    if let Err(error) = crate::verify::verify(&destination, &info.file) {
        // 内容对不上的文件留在盘上没有意义，而且下一个拿到它的人可能不会校验。
        let _ = std::fs::remove_file(&destination);
        return Err(error);
    }

    Ok(destination)
}

/// 用一份**已校验**的归档完成安装。
///
/// 校验不在这里做：`stage` 已经做过，而这个函数也允许调用方拿一份别处验好的归档来用。
///
/// # Errors
/// 用户中止、安装器报错。
pub fn apply(
    archive: &Path,
    installer: &dyn PlatformInstaller,
    sink: &dyn Fn(UpdateEvent) -> Flow,
) -> Result<InstallOutcome, UpdateError> {
    if sink(UpdateEvent::Installing) == Flow::Abort {
        return Err(UpdateError::Cancelled);
    }

    let outcome = installer.install(archive)?;
    match &outcome {
        InstallOutcome::Installed => {
            sink(UpdateEvent::Installed);
        }
        InstallOutcome::Manual { reason, archive } => {
            sink(UpdateEvent::Manual {
                reason: *reason,
                archive: archive.clone(),
            });
        }
    }
    Ok(outcome)
}

/// 归档存到本地的文件名：取 URL 最后一段。
///
/// 取不到（URL 以 `/` 结尾之类）就落一个固定名字——它只影响临时目录里的一个文件，
/// 不值得为它报错。
fn archive_name(info: &UpdateInfo) -> String {
    info.url()
        .rsplit('/')
        .find(|segment| !segment.is_empty() && !segment.contains('?'))
        .map(|segment| {
            // 只留文件名该有的字符：URL 路径段可能带 `%20`、`..` 之类。
            segment
                .chars()
                .filter(|ch| !matches!(ch, '/' | '\\' | ':' | '\0'))
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("duck-{}.archive", info.version()))
}

#[cfg(test)]
mod tests {
    use super::{
        Check, UpdateConfig, UpdateEvent, apply, archive_name, check_with, decide, signature_url,
    };
    use crate::download::Flow;
    use crate::error::UpdateError;
    use crate::install::{InstallOutcome, ManualReason, PlatformInstaller};
    use crate::manifest;
    use crate::platform::PlatformKey;
    use crate::version::Version;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const DIGEST: &str = "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3";

    fn manifest_json(version: &str) -> String {
        format!(
            r#"{{
              "manifest_version": 1,
              "channel": "stable",
              "version": "{version}",
              "notes": "notes for {version}",
              "published_at": "2026-10-10T00:00:00Z",
              "files": {{
                "macos-arm64": {{
                  "url": "https://example.invalid/duck-{version}-macos-arm64-app.tar.gz",
                  "sha256": "{DIGEST}",
                  "size": 4096,
                  "signature": null
                }}
              }}
            }}"#
        )
    }

    fn config(current: &str) -> UpdateConfig {
        UpdateConfig::new(
            "https://example.invalid/update.json",
            "/tmp/duck-update-tests",
        )
        .platform(Some(PlatformKey::MacosArm64))
        .current(Version::parse(current).expect("valid version"))
    }

    fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "duck-pipeline-{tag}-{}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    #[test]
    fn a_newer_manifest_offers_an_update() {
        let parsed = manifest::parse(manifest_json("0.2.0").as_bytes()).expect("valid");
        let check = decide(&parsed, &config("0.1.0")).expect("decides");

        let info = check.available().expect("an update is available");
        assert_eq!(info.version().to_string(), "0.2.0");
        assert_eq!(info.notes(), "notes for 0.2.0");
        assert_eq!(info.size(), 4096);
        assert_eq!(info.platform(), PlatformKey::MacosArm64);
        assert_eq!(info.sha256(), DIGEST);
    }

    #[test]
    fn the_same_version_is_up_to_date() {
        let parsed = manifest::parse(manifest_json("0.1.0").as_bytes()).expect("valid");
        assert!(matches!(
            decide(&parsed, &config("0.1.0")).expect("decides"),
            Check::UpToDate { .. }
        ));
    }

    /// 发布说明要能从**两个结论**里取到：「关于」页在「已是最新」时也得有发布记录，
    /// 不能只剩一句占位。
    #[test]
    fn release_notes_are_available_from_both_outcomes() {
        let parsed = manifest::parse(manifest_json("0.1.0").as_bytes()).expect("valid");
        let up_to_date = decide(&parsed, &config("0.1.0")).expect("decides");
        assert_eq!(up_to_date.notes(), Some("notes for 0.1.0"));

        let parsed = manifest::parse(manifest_json("0.2.0").as_bytes()).expect("valid");
        let available = decide(&parsed, &config("0.1.0")).expect("decides");
        assert_eq!(available.notes(), Some("notes for 0.2.0"));
    }

    /// 源上比本地旧：同样是「没有更新」。降级不是更新器的事。
    #[test]
    fn an_older_manifest_is_up_to_date() {
        let parsed = manifest::parse(manifest_json("0.0.9").as_bytes()).expect("valid");
        assert!(matches!(
            decide(&parsed, &config("0.1.0")).expect("decides"),
            Check::UpToDate { .. }
        ));
    }

    /// 「没给你准备条目」必须是错误，绝不能显示成「你不需要更新」。
    #[test]
    fn a_platform_missing_from_the_manifest_is_an_error_not_up_to_date() {
        let parsed = manifest::parse(manifest_json("0.2.0").as_bytes()).expect("valid");
        let target = config("0.1.0").platform(Some(PlatformKey::LinuxX64));

        assert_eq!(
            decide(&parsed, &target),
            Err(UpdateError::NoArchiveForPlatform("linux-x64"))
        );
    }

    #[test]
    fn a_build_with_no_release_target_is_rejected() {
        let parsed = manifest::parse(manifest_json("0.2.0").as_bytes()).expect("valid");
        assert_eq!(
            decide(&parsed, &config("0.1.0").platform(None)),
            Err(UpdateError::UnsupportedPlatform)
        );
    }

    /// 检查只读一份 JSON：它不在任何地方下载归档。
    #[test]
    fn checking_never_touches_an_archive() {
        let body = manifest_json("0.2.0");
        let fetch = move |_: &str| Ok(body.clone().into_bytes());

        let check = check_with(&config("0.1.0"), &fetch).expect("fetches");
        assert!(check.available().is_some());
    }

    #[test]
    fn a_network_failure_surfaces_as_a_network_error() {
        let fetch = |_: &str| Err(UpdateError::Network("offline".to_owned()));
        assert_eq!(
            check_with(&config("0.1.0"), &fetch),
            Err(UpdateError::Network("offline".to_owned()))
        );
    }

    #[test]
    fn a_garbage_manifest_surfaces_as_a_manifest_error() {
        let fetch = |_: &str| Ok(b"<html>not json</html>".to_vec());
        assert!(matches!(
            check_with(&config("0.1.0"), &fetch),
            Err(UpdateError::Manifest(_))
        ));
    }

    #[test]
    fn the_archive_name_is_the_last_url_segment() {
        let parsed = manifest::parse(manifest_json("0.2.0").as_bytes()).expect("valid");
        let Check::Available(info) = decide(&parsed, &config("0.1.0")).expect("decides") else {
            panic!("expected an update");
        };
        assert_eq!(archive_name(&info), "duck-0.2.0-macos-arm64-app.tar.gz");
    }

    /// 记录器：不真装，只记住被要求装的是什么，并汇报一个预设的结局。
    struct RecordingInstaller {
        outcome: InstallOutcome,
        installed: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl RecordingInstaller {
        fn new(outcome: InstallOutcome) -> Self {
            Self {
                outcome,
                installed: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn installed_paths(&self) -> Vec<PathBuf> {
            self.installed.lock().expect("not poisoned").clone()
        }
    }

    impl PlatformInstaller for RecordingInstaller {
        fn install(&self, archive: &Path) -> Result<InstallOutcome, UpdateError> {
            self.installed
                .lock()
                .expect("not poisoned")
                .push(archive.to_path_buf());
            Ok(self.outcome.clone())
        }

        fn relaunch(&self) -> Result<(), UpdateError> {
            Ok(())
        }

        fn sweep_stale(&self) {}

        fn rollback(&self) -> Result<(), UpdateError> {
            Ok(())
        }

        fn has_previous(&self) -> bool {
            false
        }
    }

    #[test]
    fn the_signature_lives_next_to_the_manifest_it_covers() {
        assert_eq!(
            signature_url("https://cdn.example.com/duck/update.json"),
            "https://cdn.example.com/duck/update.json.sig"
        );
        // 换通道时两个地址一起换：签名跟着 manifest 走，不写死在别处。
        assert_eq!(
            signature_url(
                "https://github.com/HanoCode/gpui-toolbox/releases/latest/download/update.json"
            ),
            "https://github.com/HanoCode/gpui-toolbox/releases/latest/download/update.json.sig"
        );
    }

    #[test]
    fn applying_a_verified_archive_hands_it_to_the_installer() {
        let dir = scratch("apply");
        let archive = dir.join("duck.tar.gz");
        std::fs::write(&archive, b"already verified").expect("writes");

        let installer = RecordingInstaller::new(InstallOutcome::Installed);
        let events = std::sync::Mutex::new(Vec::new());
        let sink = |event: UpdateEvent| {
            events.lock().expect("not poisoned").push(event);
            Flow::Continue
        };

        let outcome = apply(&archive, &installer, &sink).expect("applies");

        assert_eq!(outcome, InstallOutcome::Installed);
        assert_eq!(installer.installed_paths(), vec![archive]);
        assert_eq!(
            events.lock().expect("not poisoned").as_slice(),
            &[UpdateEvent::Installing, UpdateEvent::Installed]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusing_installer_is_reported_as_manual_with_the_archive() {
        let dir = scratch("manual");
        let archive = dir.join("duck.tar.gz");
        std::fs::write(&archive, b"already verified").expect("writes");

        let installer = RecordingInstaller::new(InstallOutcome::Manual {
            reason: ManualReason::NotABundle,
            archive: archive.clone(),
        });
        let outcome = apply(&archive, &installer, &|_| Flow::Continue).expect("applies");

        assert_eq!(
            outcome,
            InstallOutcome::Manual {
                reason: ManualReason::NotABundle,
                archive: archive.clone(),
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 中止发生在替换之前：安装器一次都不该被叫到。
    #[test]
    fn aborting_before_the_swap_never_calls_the_installer() {
        let dir = scratch("abort");
        let archive = dir.join("duck.tar.gz");
        std::fs::write(&archive, b"x").expect("writes");

        let installer = RecordingInstaller::new(InstallOutcome::Installed);
        let result = apply(&archive, &installer, &|_| Flow::Abort);

        assert_eq!(result, Err(UpdateError::Cancelled));
        assert!(installer.installed_paths().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
