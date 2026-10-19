//! 内容承载组件：把 gpui-component 0.7.1 已内置的内容件接进本项目主题。
//!
//! 上游这些件已经读 `cx.theme()` —— 本项目在 `duck-theme::manager` 里把自有 token
//! 投射到 kit 的语义槽，所以「主题化」不是重画，而是三件事：
//!
//! 1. **收敛调用点**：上游空状态要嵌四层 builder、数据表要写一个 delegate；
//!    这里给「一次调用即得」的外观，调用点只关心内容。
//! 2. **对齐规格**：字号取主题 `typography`（`xs` 为最小档）与 `extended.badge`，间距取 `spacing`，
//!    颜色取 token，不写字面量（AGENTS.md §3.6）。上游不少件用 rem 类名
//!    （`text_sm()` = 0.875rem；本项目 rem = `typography.sm` = 13px，落成 11.375px），
//!    与本项目正文不一致，这里显式覆盖回 token 字号。
//! 3. **补齐枚举**：图标用 [`DuckIcon`]（本项目自有资源），语义色用主题 token。
//!
//! 分层：本模块只决定「内容长什么样」，交互状态由调用方持有 —— 与
//! [`crate::command_palette`] 同一思路（那里只做浮层 chrome）。两个重量级件各自成模块：
//! `table`（数据表）与 `dock`（停靠系统）。
//!
//! [`DuckIcon`]: crate::DuckIcon

pub mod data;
pub mod diff;
pub mod dock;
pub mod feedback;
pub mod nav;
pub mod table;

pub use data::{
    accent_tag, badge_count, badge_dot, description_list, description_row, neutral_tag, pagination,
    status_bar, status_cell, tag,
};
pub use diff::{
    DiffLine, DiffLineKind, DiffSection, DiffSegment, DiffSummary, diff_summary, diff_view,
};
pub use dock::{CONTENT_PANEL_NAME, ContentPanel, content_panel, dock_area};
pub use feedback::{
    alert_block, empty_state, loading, loading_block, progress_bar, skeleton, skeleton_circle,
    skeleton_rows,
};
pub use nav::{accordion, tab_bar, tabs};
pub use table::{DataColumn, DataTable, TableAlign};
