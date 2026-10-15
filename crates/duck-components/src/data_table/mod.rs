//! 数据表组件：可复用的数据网格（行虚拟化 + 固定列头 + 列宽拖拽 + 就地编辑槽）。
//!
//! ## 为什么有它
//!
//! 结果网格原先是**页面里手搓**的：渲染路径上现算每格文本（`Value::render()`
//! 内含整串复制）、每个可见行每帧算一遍整行 TSV、每格 `format!` 一个字符串 id ——
//! 行一多滚动就掉帧。这套东西抽成组件后，性能口径**固定**在组件里：
//!
//! - 表数据是 `Arc<TableModel>`（[`model`]），渲染只 `Arc::clone`；
//! - 单元格展示文本在**构造时**算好（`Arc<str>`），渲染只 bump 引用计数；
//! - 行虚拟化（`uniform_list`），每帧只碰可见的几十行；
//! - **列也虚拟化**：横向视口外的列不建元素（被裁的段用等宽占位撑住行宽）。
//!   以前只做行虚拟化，40 列的表里滑出屏幕的二十多列每帧照样建、照样排版 ——
//!   实测一帧的分配量有六成花在**看不见**的列上；
//! - 元素 id 走整数编码，不做每帧字符串分配。
//!
//! ## 怎么用
//!
//! ```ignore
//! // ① 结果换新：把结果集转成中性模型（本体不依赖任何数据源）
//! let model = Arc::new(build_table_model(&result));
//! // ② 组件状态建一次，换结果时 set_model / set_view_rows（搜索、分页）
//! let state = cx.new(|_| DataTableState::new(model));
//! // ③ 渲染：宿主真源（选中 / 勾选 / 排序 / 就地编辑）按帧传进来
//! data_table("db-result-grid", &state, &view, &labels, &hooks, cx)
//! // ④ 交互经事件回到宿主（`cx.subscribe(&state, …)`），
//! //    菜单与「需要 window 的交互」走 `DataTableHooks`。
//! ```
//!
//! ## 归属边界
//!
//! | 归属 | 内容 |
//! | --- | --- |
//! | **组件** | 表模型、列宽 + 前缀和、藏列、展示行序、滚动句柄、列宽拖拽、单元格渲染 |
//! | **宿主** | 选中 / 勾选 / 排序态、就地编辑器实体（`InputState` 要 `&mut Window`）、业务动作（暂存、菜单、剪贴板） |
//!
//! 组件**不含任何界面文案**（duck-components 的既有口径）：文案由宿主用
//! [`DataTableLabels`] 注入，动态部分用 `{}` 模板。

mod events;
mod model;
mod state;
mod table;

pub use events::{
    CellDoubleClickHook, CellMenuHook, CellPos, DataTableEvent, DataTableHooks, DataTableLabels,
    HeaderMenuHook, RowDoubleClickHook,
};
pub use model::{
    AUTO_WIDTH_SAMPLE_ROWS, CellKind, CellValue, ColumnSpec, HEADER_HEIGHT, MAX_AUTO_WIDTH,
    MAX_COLUMN_WIDTH, MIN_COLUMN_WIDTH, RESIZE_HANDLE_WIDTH, ROW_HEIGHT, ROWNUM_WIDTH, RowData,
    SELECT_COLUMN_WIDTH, TableModel, column_offsets, fill_template,
};
pub use state::{DataTableState, ResizeDrag};
pub use table::{DataTableView, data_table};
