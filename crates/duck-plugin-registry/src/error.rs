//! 插件市场的错误类型。
//!
//! 与 `duck-update` 同一约定：`Display` 一律英文（日志与技术文案），用户可见的
//! 中文句式由 `duck-app` 用 `trf` 套壳。这样与本项目「`EN` 表只放界面文案、
//! 日志与技术文案写英文」的约定一致。

use std::fmt;

/// 单条索引项不可用的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryProblem {
    /// 插件 id 不符合命名规范。
    Id(String),
    /// 展示名为空。
    EmptyName,
    /// 版本号不是语义化版本，无从比较。
    UnreadableVersion(String),
    /// 清单 API 版本不受本客户端支持。
    ApiVersion {
        /// 索引里写的版本。
        found: u32,
        /// 本客户端支持的版本。
        supported: u32,
    },
    /// 执行模型不是 `js` / `wasm` / `process`。
    ExecutionModel(String),
    /// 归档地址不是绝对 `https://`。
    InsecureUrl(String),
    /// 归档摘要不是 64 位小写十六进制。
    Digest(String),
    /// 归档声明大小为 0。
    ZeroSize,
}

impl fmt::Display for EntryProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => write!(f, "invalid plugin id {id:?}"),
            Self::EmptyName => f.write_str("the plugin name is empty"),
            Self::UnreadableVersion(text) => write!(f, "unreadable version {text:?}"),
            Self::ApiVersion { found, supported } => write!(
                f,
                "api_version {found} is not supported by this build (supports {supported})"
            ),
            Self::ExecutionModel(model) => write!(f, "unknown execution model {model:?}"),
            Self::InsecureUrl(url) => write!(f, "not an https url: {url:?}"),
            Self::Digest(digest) => write!(f, "not a SHA-256 digest: {digest:?}"),
            Self::ZeroSize => f.write_str("the archive size is zero"),
        }
    }
}

/// 插件市场错误。
#[derive(Debug)]
pub enum RegistryError {
    /// 网络层失败（取索引 / 取归档）。
    Network(String),
    /// 索引不是 JSON，或缺了必填字段。
    Malformed(String),
    /// 索引里没有 `index_version` 键，无从判断该怎么读它。
    MissingIndexVersion,
    /// 索引由更新的客户端写就。拒绝，而不是半读。
    UnsupportedIndexVersion {
        /// 文档里写的版本。
        found: u64,
        /// 本客户端支持的版本。
        supported: u32,
    },
    /// 索引里的某一条不可用。
    InvalidEntry {
        /// 出问题的插件 id（id 本身非法时就是那个非法值）。
        id: String,
        /// 具体哪里不对。
        problem: EntryProblem,
    },
    /// 归档下载失败。
    Download(String),
    /// 归档摘要与索引承诺的不一致。
    Digest {
        /// 索引承诺的摘要。
        expected: String,
        /// 磁盘上算出来的摘要。
        actual: String,
    },
    /// 归档实际字节数与索引承诺的不一致。
    SizeMismatch {
        /// 索引承诺的字节数。
        expected: u64,
        /// 实际读到 / 写下的字节数。
        actual: u64,
    },
    /// 解包失败（格式问题、链接条目、路径逃逸、超出总量上限）。
    Extract(String),
    /// 包内清单无效，或与索引声明的 id / version 不一致。
    Manifest(String),
    /// 本地文件系统读写失败。
    Io(String),
    /// 被调用方中止。
    Cancelled,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(detail) => write!(f, "plugin index request failed: {detail}"),
            Self::Malformed(detail) => write!(f, "the plugin index is malformed: {detail}"),
            Self::MissingIndexVersion => f.write_str("the plugin index carries no index_version"),
            Self::UnsupportedIndexVersion { found, supported } => write!(
                f,
                "the plugin index is version {found}; this build reads up to {supported}"
            ),
            Self::InvalidEntry { id, problem } => {
                write!(f, "the entry for {id:?} is unusable: {problem}")
            }
            Self::Download(detail) => write!(f, "plugin download failed: {detail}"),
            Self::Digest { expected, actual } => write!(
                f,
                "the plugin archive digest is {actual}, expected {expected}"
            ),
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "the plugin archive is {actual} bytes, expected {expected}"
            ),
            Self::Extract(detail) => write!(f, "plugin extraction failed: {detail}"),
            Self::Manifest(detail) => write!(f, "plugin manifest rejected: {detail}"),
            Self::Io(detail) => write!(f, "plugin io error: {detail}"),
            Self::Cancelled => f.write_str("the plugin operation was cancelled"),
        }
    }
}

impl std::error::Error for RegistryError {}
