//! 自建更新器：查 → 下 → 校验 → 就地替换 → 重启。
//!
//! 更新源是一份**静态 JSON manifest**（同一份文件放哪都行：GitHub Release 资产、
//! 对象存储、任意静态站）。文档形状与 `manifest_version` 门禁见 [`manifest`]；
//! 本 crate 只负责「读得懂就照做」。
//!
//! # 分层
//!
//! - [`manifest`] / [`platform`] / [`version`] / [`sha256`] —— 纯数据：解析、比较、摘要。
//!   不碰网络也不碰磁盘，可以全部用普通 `#[test]` 覆盖。
//! - [`target`] / [`swap`] —— 纯路径算术与文件搬移：当前二进制在磁盘上是什么、
//!   怎么换位、换失败怎么回滚。
//! - [`download`] / [`verify`] / [`install`] —— 副作用层：网络、读盘、替换 `.app`。
//! - [`pipeline`] —— 把上面几件按顺序串起来，并通过回调交出进度。
//!
//! # 三条硬规矩
//!
//! 1. **没点头就不下载**：检查（[`pipeline::check`]）与安装（[`pipeline::install`]）
//!    是两个函数。本 crate 不会自己起定时器，也不会「顺便」下点东西。
//! 2. **校验过的才安装**：[`verify`] 会**重新读盘**逐块算摘要，而不是相信下载时算出的那个；
//!    归档里的字节会被解包成用户要运行的二进制，要看的是此刻磁盘上的字节。
//! 3. **归档里的东西绝不执行**：解包交给系统的 `tar`，新二进制只在用户点「重启」之后才被
//!    `open` 拉起。
//!
//! # 文案约定
//!
//! 本 crate 的错误是给用户看的，但**一律写成英文**（`Display`），由调用方套进中文帧里
//! （`trf(cx, "更新失败：{}", ..)`）。界面文案属于 `duck-app`。这样也与项目
//! 「`EN` 表只放界面文案、日志与技术文案写英文」的约定一致。

mod download;
mod error;
mod install;
mod manifest;
mod pipeline;
mod platform;
mod sha256;
mod signature;
mod swap;
mod target;
mod verify;
mod version;

/// I/O 缓冲块大小（下载 socket 读 / 校验磁盘读共用同一档）。
///
/// 够大：几十 MB 的包只要几百次迭代；够小：进度回调刷得够密，校验也不必把整包读进内存。
pub(crate) const CHUNK: usize = 64 * 1024;

pub use download::{Downloaded, Flow, stream};
pub use error::UpdateError;
pub use install::{InstallOutcome, ManualReason, PlatformInstaller, platform_installer};
pub use manifest::{Manifest, ManifestError, ManifestFile, SUPPORTED_MANIFEST_VERSION};
pub use pipeline::{
    Check, Fetcher, UpdateConfig, UpdateEvent, UpdateInfo, apply, check, check_with, install, stage,
};
pub use platform::PlatformKey;
pub use sha256::{Hasher, is_hex_digest};
pub use signature::{builtin_public_key, required as signature_required, verify_with};
pub use target::{InstallTarget, classify};
pub use verify::verify;
pub use version::{Channel, Version, compiled_version, is_newer};
