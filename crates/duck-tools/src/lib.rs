//! Duck Toolbox 工具注册表 —— 工具箱的「菜单表」。
//!
//! 每个工具登记一条 [`ToolMeta`]，注册表据此**派生**：
//! 侧栏分类、工具列表、搜索索引（名称 / 关键词 / 全拼 / 首字母）、
//! 工具级设置界面与智能粘贴路由。新增一个工具只需在 catalog 里
//! `register` 一条 [`ToolMeta`]，不必改动任何界面代码。
//!
//! # 相对 TS `ToolMeta` 的三处收敛
//!
//! | TS | Rust | 原因 |
//! | --- | --- | --- |
//! | `icon: string` | [`DuckIcon`](duck_components::DuckIcon) | 写错图标名编译失败，而不是运行时找不到资源 |
//! | `category: string` | [`ToolCategory`] | 写错分类编译失败，而不是运行时归进「其他」 |
//! | `pinyin` / `pinyinInitials` | 注册时由 [`pinyin`] 自动派生 | 免手写 14 个字段、免漏填 |
//!
//! # 分层
//!
//! - [`ToolIndex`]：纯逻辑（去重 / 拼音派生 / 筛选），**不依赖 gpui**，可在普通 `#[test]` 里跑；
//! - [`ToolRegistry`]：gpui 全局，把索引与持久化（启用状态 / 开发者模式 / 最近使用）接起来。

mod category;
mod help;
mod inbox;
mod meta;
mod pinyin;
mod query;
mod registry;
mod search;

pub mod paste;
pub mod settings;

pub use category::ToolCategory;
pub use help::HelpSection;
pub use inbox::{PasteInbox, PendingPaste};
pub use meta::{ToolMeta, ToolMetaBuilder, ToolRun};
pub use pinyin::{initials_of, plain_of};
pub use query::ToolQuery;
pub use registry::{RegisteredTool, ToolIndex, ToolRegistry, is_tool_listed, is_tool_visible};
pub use search::{score, search};
