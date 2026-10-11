//! 更新过程中可能失败的地方。
//!
//! `Display` 一律英文：这些串会被套进界面的中文帧里（`更新失败：{}`），
//! 而不是自己去当界面文案。项目约定见 `duck-theme/src/i18n.rs` 的表头说明。

use std::fmt;

use crate::manifest::ManifestError;

/// 更新失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// 取 manifest 时的网络 / HTTP 层失败（含非 2xx 状态码）。
    Network(String),
    /// manifest 读不懂或不敢照做。
    Manifest(ManifestError),
    /// 当前平台本项目不发版，manifest 里也不会有对应条目。
    UnsupportedPlatform,
    /// manifest 认得这个平台，但没给这条平台准备归档。
    ///
    /// **不能**当成「已是最新」：那会把「没给你准备」说成「你不需要」。
    NoArchiveForPlatform(&'static str),
    /// 下载归档时的网络层失败。
    Download(String),
    /// 归档比 manifest 承诺的大小多出一截，已放弃（避免把磁盘写满）。
    SizeMismatch {
        /// manifest 里写的字节数。
        expected: u64,
        /// 实际收到的字节数。
        actual: u64,
    },
    /// 摘要对不上：归档不是 manifest 描述的那一份。
    DigestMismatch {
        /// manifest 里写的摘要。
        expected: String,
        /// 实际算出的摘要。
        actual: String,
    },
    /// 读写本地文件失败。
    Io(String),
    /// 替换安装失败（可能已经回滚）。
    Install(String),
    /// 想回退，但没有上一版可退。
    ///
    /// 这是**正常状态**而不是故障：从来没更新过，或上一版已经被更晚的一次更新顶掉。
    /// 单独成一个变体，是为了让界面能说「没有可回退的版本」而不是一句笼统的失败。
    NoPreviousVersion,
    /// 更新源的 manifest 没有通过签名校验。
    ///
    /// 与 [`DigestMismatch`](Self::DigestMismatch) 是两件事：摘要对不上说明**传输**出了
    /// 问题，而这里说明**源**本身不可信。两者都要过。
    Signature(String),
    /// 这份更新要求的最低应用版本比当前高。
    ///
    /// 常见于格式换代：老启动器读不懂新包，所以发布方立了个门槛。
    AppTooOld {
        /// 当前版本。
        current: String,
        /// manifest 要求的最低版本。
        required: String,
    },
    /// 用户中止。
    Cancelled,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(detail) => write!(f, "could not reach the update source: {detail}"),
            Self::Manifest(error) => write!(f, "{error}"),
            Self::UnsupportedPlatform => {
                write!(f, "this build has no release archive to update from")
            }
            Self::NoArchiveForPlatform(platform) => {
                write!(f, "the manifest has no entry for {platform}")
            }
            Self::Download(detail) => write!(f, "download failed: {detail}"),
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "the archive is larger than the manifest promises (expected {expected} bytes, got {actual})"
            ),
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "the archive does not match its checksum (expected {expected}, got {actual})"
            ),
            Self::Io(detail) => write!(f, "file system error: {detail}"),
            Self::Install(detail) => write!(f, "could not install the update: {detail}"),
            Self::NoPreviousVersion => {
                write!(f, "there is no previous version to fall back to")
            }
            Self::Signature(detail) => {
                write!(
                    f,
                    "the update manifest is not signed by a trusted key: {detail}"
                )
            }
            Self::AppTooOld { current, required } => write!(
                f,
                "this update needs version {required} or newer; this build is {current}"
            ),
            Self::Cancelled => write!(f, "the update was cancelled"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<ManifestError> for UpdateError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}
