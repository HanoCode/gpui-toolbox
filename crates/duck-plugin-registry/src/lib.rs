//! Duck Toolbox 插件市场（纯逻辑层，不依赖 gpui）。
//!
//! 分层位置：与 [`duck-update`] 同级的最底层能力 crate。与 [`duck-plugin`] 的分工是
//! ——`duck-plugin` 管**本机**有什么插件（清单格式、能力网关、目录发现），
//! 本 crate 管**远端**有什么插件（索引解析、下载校验、安装卸载），二者共用同一份
//! `plugin.toml` 语义：安装的最后一关就是用 [`duck_plugin::PluginManifest`] 复核
//! 解包出来的清单，id / version 必须与索引一致。
//!
//! # 四条硬规矩
//!
//! 1. **索引先校验再采信**：`index_version` 先于一切字段被读出来比对，之后逐条校验
//!    （https-only、SHA-256 形态、size 非零、api_version 受支持）。与 `duck-update`
//!    同一取舍——半懂的文档不许走到下载那一步。
//! 2. **校验过的才安装**：下载时流式算的摘要只是**早退信号**，落盘后会**重新读盘**
//!    再算一遍，比对通过才解包。
//! 3. **解包绝不逃逸**：拒绝绝对路径与 `..`（交给 `tar::Entry::unpack_in` 判定）、
//!    拒绝符号链接与硬链接、并有解包总量上限，防止归档炸弹。
//! 4. **归档里的东西绝不执行**：安装只做「解包 + 校验 + 原子改名落位」，脚本要等用户
//!    在工具箱里真的点那个工具才会被 `duck-plugin-js` 拉起。
//! 5. **能力不超索引**：归档清单声明的能力必须是索引声明那一份的**子集**。授权照归档
//!    清单授予，而用户在安装前点头看的是索引那份——允许超出，确认框就形同虚设。
//!
//! # 文案约定
//!
//! 与 `duck-update` 一致：错误是给用户看的，但 `Display` **一律英文**，中文句式由
//! `duck-app` 用 `trf` 套壳。
//!
//! [`duck-update`]: https://docs.rs/duck-update
//! [`duck-plugin`]: https://docs.rs/duck-plugin

mod error;
mod fetch;
mod index;
mod install;
mod sha256;
mod status;

pub use error::{EntryProblem, RegistryError};
pub use fetch::fetch_index;
pub use index::parse as parse_index;
pub use index::{
    ArchiveRef, RegistryEntry, RegistryIndex, SUPPORTED_INDEX_VERSION, is_valid_plugin_id,
};
pub use install::{Flow, Installed, install, install_from_archive, uninstall};
pub use sha256::{Hasher, is_hex_digest};
pub use status::{PluginStatus, classify};
