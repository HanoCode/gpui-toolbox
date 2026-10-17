//! 数据库驱动商店内核：**驱动目录 → 远端注册表 → 下载 → 校验 → 安装 / 卸载**，
//! 外加托管 JRE。
//!
//! # 它解决什么
//!
//! `duck-db` 的原生驱动只有 7 条（编进二进制、零运行时依赖），而现实里用户连的库
//! 远不止这 7 条 —— 达梦 / 金仓 / Oracle / DB2 / Hive … 这些没有 Rust 原生驱动，
//! 只能靠**独立驱动进程**（`duck-db-agent` 那条约定的 JSON-RPC 子进程）覆盖。
//! 本 crate 负责这些驱动进程的**生命周期管理**：从哪拿、下下来怎么验、装到哪、
//! 怎么判断该更新、怎么在离线环境里搬运。
//!
//! 数据模型与下载源对齐 dbx（Apache-2.0）的 `agent-registry.json`：一份**远端 JSON
//! 注册表**（`{ jres, drivers }`）+ 每个驱动一份 `tar.zst` 工件，装法是
//! 「解包 → staging → 原子替换」。这样做的直接好处是**现成的驱动工件能直接用**
//! （55 条：jar 轨走 JVM、native 轨走独立可执行文件），不必自己写几十个驱动。
//!
//! # 分层
//!
//! ```text
//! catalog   驱动目录（键 / 名称 / 分类 / 缺省端口）—— 静态表，界面侧栏与商店列表的唯一来源
//! registry  远端注册表：拉取、缓存、解析（版本、工件 url/sha256/size、增量 delta）
//! source    下载源（官方 / 镜像）与候选 URL 生成
//! download  流式下载 + SHA-256（不整包进内存）+ 进度回调
//! archive   `tar.zst` 解包（条目白名单 + 拒绝路径逃逸）
//! install   安装 / 更新 / 卸载 / 离线包导入导出；`state.json` 落盘
//! jre       托管 JRE（Temurin 21）：安装、探测、`java` 可执行文件定位
//! events    进度事件（`mpsc`，宿主侧节流 drain）
//! platform  当前平台键（`macos-aarch64` 这类注册表口径）
//! ```
//!
//! # 口径（与全仓一致的两条）
//!
//! 1. **零 gpui、零 tokio 外泄**：本 crate 全是阻塞函数，调用方在
//!    `background_executor` 上跑它们，进度经 [`events::DriverEvent`] 的 `mpsc`
//!    回流到主线程（照 `duck-mail` 的既有范式，禁止逐事件 notify）。
//! 2. **内核错误一律英文**（`AGENTS.md` 第 3 节）：错误串是排障用的，不出给用户；
//!    面向用户的提示由界面按错误**种类**自己 `trf`。
//!
//! # 为什么不落 `duck-store` 的 kv
//!
//! 安装状态（装了哪个版本、什么时候装的、用哪个 JRE）是**本模块自己的cache 类数据**：
//! 丢了最多重下一次，且必须能在**没有 gpui `App`** 的上下文里读写（离线校验、启动
//! 自检）。所以它落自己目录下的 `state.json`，与 dbx 的 `state.json` 同构。

pub mod archive;
pub mod catalog;
pub mod download;
pub mod events;
pub mod install;
pub mod jre;
pub mod platform;
pub mod registry;
pub mod source;
pub mod state;

mod catalog_gen;

pub use catalog::{
    DriverCategory, DriverEntry, agent_sources, catalog, categories, category_label_of, entries_in,
    entry,
};
pub use download::{Downloaded, Flow};
pub use events::{DriverEvent, EventSink, ProgressStep};
pub use install::{CancelFlag, DriverStore, LaunchSpec, OfflinePackage, StoreSummary};
pub use jre::{JavaRuntime, JavaRuntimeMode};
pub use registry::{
    ArtifactFormat, ArtifactInfo, ArtifactKind, DeltaInfo, DriverInfo, DriverRegistry, JreInfo,
};
pub use source::DownloadSource;
pub use state::{DriverState, InstalledDriver};

/// 本模块所有可失败操作的错误类型。
#[derive(Debug)]
pub enum DriverError {
    /// 网络层：连不上、非 2xx、超时。
    Network(String),
    /// 磁盘读写。
    Io(String),
    /// 摘要 / 体积与注册表承诺不符——**装上去的东西不可信**，一律拒绝。
    Integrity(String),
    /// 注册表 / 归档的结构不是我们认得的形状。
    Format(String),
    /// 调用方要求中止（用户点了取消）。
    Cancelled,
    /// 这个平台上没有对应工件（注册表里缺该平台键）。
    Unsupported(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(message) => write!(f, "network error: {message}"),
            Self::Io(message) => write!(f, "io error: {message}"),
            Self::Integrity(message) => write!(f, "integrity error: {message}"),
            Self::Format(message) => write!(f, "format error: {message}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Unsupported(message) => write!(f, "unsupported: {message}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<ureq::Error> for DriverError {
    fn from(error: ureq::Error) -> Self {
        Self::Network(error.to_string())
    }
}

impl From<std::io::Error> for DriverError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, DriverError>;
