//! 数据表对外的事件与钩子。
//!
//! **分工**：能只靠 `cx` 落地的交互走 [`DataTableEvent`]（宿主 `cx.subscribe`
//! 接收）；只有两类东西必须用闭包 ——
//!
//! 1. **右键菜单内容**：gpui-component 的菜单是**声明式**的（`.context_menu()` 挂在
//!    元素上、构建闭包在菜单弹出时才跑），拿不到「弹出时再问宿主」的机会；
//! 2. **需要 `&mut Window` 的交互**（双击开就地编辑 / 开行编辑器）：gpui 的事件订阅
//!    只给 `&mut Context<Self>`，没有窗口，而 `InputState::new` 必须要窗口。
//!
//! 所以 [`DataTableHooks`] 只放这两类，别的一律进事件。

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, SharedString, Window};
use gpui_component::menu::PopupMenu;

/// 单元格坐标（**模型行下标**，不是展示位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPos {
    pub row: usize,
    pub col: usize,
}

impl CellPos {
    #[must_use]
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// 单元格矩形选区：锚点（起选那一格）→ 焦点（现在拖到哪一格）。
///
/// 存**模型行下标**（与 `CellPos` 同口径），不是展示位置：排序 / 搜索只改展示
/// 顺序，选区跟着行走 —— 用户选的是「这几行数据」，不是「第 3 到第 5 个格子」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    pub anchor: CellPos,
    pub focus: CellPos,
}

impl CellRange {
    #[must_use]
    pub const fn new(anchor: CellPos, focus: CellPos) -> Self {
        Self { anchor, focus }
    }

    /// 规整成 `(行起, 行止, 列起, 列止)`（含两端）。
    #[must_use]
    pub fn bounds(&self) -> (usize, usize, usize, usize) {
        (
            self.anchor.row.min(self.focus.row),
            self.anchor.row.max(self.focus.row),
            self.anchor.col.min(self.focus.col),
            self.anchor.col.max(self.focus.col),
        )
    }

    #[must_use]
    pub fn contains(&self, position: CellPos) -> bool {
        let (row_start, row_end, col_start, col_end) = self.bounds();
        (row_start..=row_end).contains(&position.row)
            && (col_start..=col_end).contains(&position.col)
    }

    /// 选中的格子数（含两端）。
    #[must_use]
    pub fn cell_count(&self) -> usize {
        let (row_start, row_end, col_start, col_end) = self.bounds();
        (row_end - row_start + 1).saturating_mul(col_end - col_start + 1)
    }
}

/// 组件 → 宿主的事件。
#[derive(Debug, Clone)]
pub enum DataTableEvent {
    /// 点了某一行（含点在单元格上：单元格自己的事件之后会跟上）。
    RowSelected { row: usize },
    /// 点了某个单元格（宿主用它更新「单元格详情」指向）。
    CellSelected(CellPos),
    /// 在某格按下鼠标：宿主把选区锚点设到这一格（并清掉旧选区）。
    SelectionStarted(CellPos),
    /// 拖动中进入了某一格：宿主把选区焦点更新到这一格（矩形扩展）。
    SelectionExtended(CellPos),
    /// 某一行勾选框变化（`checked` 是**目标态**，不是「现在的态」）。
    RowCheckedToggled { row: usize, checked: bool },
    /// 表头全选框变化（目标态）。
    AllCheckedToggled { checked: bool },
    /// 点列头排序（宿主决定「升 / 降 / 取消」三态怎么轮转，排序在服务器侧做）。
    ///
    /// `additive` = 点的时候按着 `Shift`：语义是**追加一级排序**，不是替换现有排序
    /// （多列排序的通用手势，表格软件与 DBX 一致）。
    SortRequested { column: Arc<str>, additive: bool },
    /// 列宽定稿（拖拽收尾；组件自己已经用上，宿主可用它落盘）。
    ColumnResized { col: usize, width: f32 },
    /// 拖列头改了列序（组件已改好；宿主可用它落盘）。
    ColumnOrderChanged,
    /// 点了就地编辑格里的 ✓：宿主按自己的编辑记录提交（文本真源在宿主那 side
    /// 的 `InputState` 上，组件只报「该提交了」）。
    CellEditSubmitRequested(CellPos),
    /// 点了格内 ⓘ（单元格详情入口）。
    CellDetailRequested(CellPos),
}

/// 单元格右键菜单内容（宿主在这里塞自己的菜单项）。
///
/// 签名照 gpui-component 的声明式口径：菜单是**值**，闭包原样还回去
/// （`PopupMenu` 的构建器风格），不是「在菜单上挂东西」。
pub type CellMenuHook =
    Rc<dyn Fn(PopupMenu, CellPos, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
/// 列头右键菜单内容。
pub type HeaderMenuHook =
    Rc<dyn Fn(PopupMenu, usize, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
/// 双击单元格（宿主在这里决定：开就地编辑，还是提示「结果集不可编辑」）。
pub type CellDoubleClickHook = Rc<dyn Fn(CellPos, &mut Window, &mut App)>;
/// 双击行空白处（行号 / 多选列一带）：宿主在这里开行编辑器。
pub type RowDoubleClickHook = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// 需要窗口的交互钩子（见本模块文档：为什么不是事件）。
#[derive(Clone, Default)]
pub struct DataTableHooks {
    pub cell_menu: Option<CellMenuHook>,
    pub header_menu: Option<HeaderMenuHook>,
    /// 列头**下拉按钮**的菜单（左键点 ∶ 弹出，锚在按钮下方）。与右键菜单是两条入口：
    /// 下拉是「这一列怎么显示」的操作台（格式化 / 筛选 / 清除），右键是全量操作。
    pub header_dropdown: Option<HeaderMenuHook>,
    pub on_cell_double_click: Option<CellDoubleClickHook>,
    pub on_row_double_click: Option<RowDoubleClickHook>,
}

impl DataTableHooks {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cell_menu.is_none()
            && self.header_menu.is_none()
            && self.header_dropdown.is_none()
            && self.on_cell_double_click.is_none()
            && self.on_row_double_click.is_none()
    }
}

/// 组件里的**用户可见文案**（宿主注入）。
///
/// 组件自身不含任何界面文案（duck-components 的既有口径）：`tr` / `trf` 留在
/// 宿主，动态部分用 `{}` 模板传进来，组件按顺序填（见
/// [`crate::data_table::fill_template`]）。
#[derive(Clone, Debug)]
pub struct DataTableLabels {
    /// 表头全选框的可访问名。
    pub select_all: SharedString,
    /// 行勾选框的可访问名模板，形如「第 {} 行」。
    pub row_name: SharedString,
    /// 列头排序的可访问名 / 提示模板，形如「按 {} 排序」。
    pub sort_by: SharedString,
    /// 就地编辑的提交按钮名。
    pub submit_edit: SharedString,
    /// 格内 ⓘ 的名字（同时当悬停提示）。
    pub cell_detail: SharedString,
    /// 「搜了个词却一行没中」的提示（宿主按当前搜索词拼好；没有搜索时给 `None`）。
    pub empty_hint: Option<SharedString>,
    /// 列头下拉按钮的可访问名模板，形如「{} 的操作」。
    pub column_menu: SharedString,
    /// 悬停详情卡的三行标签：列名 / 类型 / 注释（没有注释时第三行不画）。
    pub detail_name: SharedString,
    pub detail_type: SharedString,
    pub detail_comment: SharedString,
    /// 详情卡上的复制按钮（复制的是**整份详情**：列名 / 类型 / 注释三行）。
    pub detail_copy: SharedString,
    /// 复制出来的三行里，「标签」与「值」之间的分隔符。
    ///
    /// 不是标点常量而是**文案**：中文是全角冒号（`列名：id`）、英文是冒号加空格
    /// （`Column: id`），所以由宿主注入、随界面语言走。
    pub detail_separator: SharedString,
}

impl DataTableLabels {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        select_all: impl Into<SharedString>,
        row_name: impl Into<SharedString>,
        sort_by: impl Into<SharedString>,
        submit_edit: impl Into<SharedString>,
        cell_detail: impl Into<SharedString>,
        column_menu: impl Into<SharedString>,
        detail_name: impl Into<SharedString>,
        detail_type: impl Into<SharedString>,
        detail_comment: impl Into<SharedString>,
        detail_copy: impl Into<SharedString>,
        detail_separator: impl Into<SharedString>,
    ) -> Self {
        Self {
            select_all: select_all.into(),
            row_name: row_name.into(),
            sort_by: sort_by.into(),
            submit_edit: submit_edit.into(),
            cell_detail: cell_detail.into(),
            empty_hint: None,
            column_menu: column_menu.into(),
            detail_name: detail_name.into(),
            detail_type: detail_type.into(),
            detail_comment: detail_comment.into(),
            detail_copy: detail_copy.into(),
            detail_separator: detail_separator.into(),
        }
    }

    #[must_use]
    pub fn with_empty_hint(mut self, hint: Option<SharedString>) -> Self {
        self.empty_hint = hint;
        self
    }
}
