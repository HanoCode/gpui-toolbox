//! 数据表的**组件内部状态**：几何（列宽 / 横向偏移 / 滚动位置）与「展示哪些行」。
//!
//! 边界（宿主侧保留真源的东西一律不进这里）：
//!
//! | 归属 | 内容 |
//! | --- | --- |
//! | **组件** | 表模型 `Arc<TableModel>`、列宽 + 前缀和、藏起来的列、展示行序、滚动句柄、列宽拖拽态 |
//! | **宿主** | 选中行 / 单元格、勾选行、排序态、就地编辑器实体（`InputState` 需要 `&mut Window`，宿主才有）、暂存改动 |
//!
//! 选中 / 勾选走「宿主真源 + 组件渲染」而不是搬进组件：宿主那边有二十多处
//! 依赖这些集合的业务（批量条 / 删除行 / 翻页 / 右栏），搬进来等于把一次组件化
//! 变成一次全页重构。组件只负责把它们**画对**、把变化**报出去**。

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{Context, EventEmitter, ScrollHandle, UniformListScrollHandle};

use super::events::DataTableEvent;
use super::model::{
    MAX_COLUMN_WIDTH, MIN_COLUMN_WIDTH, ROWNUM_WIDTH, SELECT_COLUMN_WIDTH, TableModel,
    column_offsets,
};

/// 列宽拖拽态（起拖时的指针 x 与起始列宽）。
#[derive(Debug, Clone, Copy)]
pub struct ResizeDrag {
    pub col: usize,
    pub start_x: f32,
    pub start_width: f32,
}

/// 数据表组件状态。
pub struct DataTableState {
    model: Arc<TableModel>,
    /// 展示哪些行（**模型行下标**，按展示顺序）——搜索命中 + 客户端分页都由宿主算好。
    view_rows: Vec<usize>,
    /// 行号起点：展示行 `n` 的序号是 `row_offset + n + 1`。
    row_offset: usize,
    /// 列宽（按**原始列序**对齐，与行数据同一口径）。
    widths: Vec<f32>,
    /// 列宽前缀和（横向定位用）。
    offsets: Vec<f32>,
    /// 藏起来的列（原始下标；口径与宿主「字段筛选」一致 —— 只影响显示）。
    hidden: BTreeSet<usize>,
    /// 列宽拖拽中（`None` = 没在拖）。
    resize: Option<ResizeDrag>,
    /// 横向滚动句柄（表头与行区共用；滚动条从它读位置）。
    h_scroll: ScrollHandle,
    /// 纵向滚动句柄（`uniform_list` 自己的；跨帧持久才能「切视图回来位置还在」）。
    v_scroll: UniformListScrollHandle,
}

impl EventEmitter<DataTableEvent> for DataTableState {}

impl DataTableState {
    #[must_use]
    pub fn new(model: Arc<TableModel>) -> Self {
        let widths = model.estimated_widths();
        let offsets = column_offsets(&widths);
        Self {
            model,
            view_rows: Vec::new(),
            row_offset: 0,
            widths,
            offsets,
            hidden: BTreeSet::new(),
            resize: None,
            h_scroll: ScrollHandle::default(),
            v_scroll: UniformListScrollHandle::new(),
        }
    }

    /// 换模型（新结果集）：列集变了就重估列宽并清掉藏列与横向位置。
    ///
    /// 返回是否发生了「列集变化」（宿主可据此决定要不要把选中 / 勾选一起清掉）。
    ///
    /// ⚠️ 判据是**列名序列**而不是列数：两张表都恰好 3 列时，只比列数会把
    /// `(id, name, age)` 的列宽套到 `(id, title, body)` 上（每一列都宽窄错位）。
    pub fn set_model(&mut self, model: Arc<TableModel>, cx: &mut Context<Self>) -> bool {
        let columns_changed = model.columns().len() != self.model.columns().len()
            || model
                .columns()
                .iter()
                .zip(self.model.columns())
                .any(|(new, old)| new.name != old.name);
        self.model = model;
        if columns_changed {
            self.widths = self.model.estimated_widths();
            self.offsets = column_offsets(&self.widths);
            self.hidden.clear();
            self.h_scroll
                .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        }
        cx.notify();
        columns_changed
    }

    /// 换展示行序（搜索 / 客户端分页）：`rows` 是模型行下标，`offset` 是行号起点。
    pub fn set_view_rows(&mut self, rows: Vec<usize>, offset: usize, cx: &mut Context<Self>) {
        self.view_rows = rows;
        self.row_offset = offset;
        cx.notify();
    }

    /// 藏起来的列（按列名给；内部映射成下标）。列名对不上的忽略 ——
    /// 结果集换了列以后，旧名字自然失效。
    pub fn set_hidden_names(&mut self, names: &BTreeSet<String>, cx: &mut Context<Self>) {
        let hidden: BTreeSet<usize> = self
            .model
            .columns()
            .iter()
            .enumerate()
            .filter(|(_, column)| names.contains(column.name.as_ref()))
            .map(|(index, _)| index)
            .collect();
        if hidden != self.hidden {
            self.hidden = hidden;
            cx.notify();
        }
    }

    #[must_use]
    pub fn model(&self) -> &Arc<TableModel> {
        &self.model
    }

    #[must_use]
    pub fn view_rows(&self) -> &[usize] {
        &self.view_rows
    }

    #[must_use]
    pub const fn row_offset(&self) -> usize {
        self.row_offset
    }

    #[must_use]
    pub fn widths(&self) -> &[f32] {
        &self.widths
    }

    #[must_use]
    pub fn offsets(&self) -> &[f32] {
        &self.offsets
    }

    #[must_use]
    pub const fn h_scroll(&self) -> &ScrollHandle {
        &self.h_scroll
    }

    #[must_use]
    pub const fn v_scroll(&self) -> &UniformListScrollHandle {
        &self.v_scroll
    }

    /// 可见列：`(原始下标, 列)`。藏起来的列不进列头也不进行。
    pub fn visible_columns(&self) -> impl Iterator<Item = (usize, &super::model::ColumnSpec)> {
        self.model
            .columns()
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.hidden.contains(index))
    }

    /// 可见列的宽度合计（含行号列与多选列）—— 横向滚动的内容宽度。
    #[must_use]
    pub fn content_width(&self) -> f32 {
        self.visible_columns()
            .map(|(index, _)| self.widths.get(index).copied().unwrap_or(MIN_COLUMN_WIDTH))
            .sum::<f32>()
            + ROWNUM_WIDTH
            + SELECT_COLUMN_WIDTH
    }

    #[must_use]
    pub fn column_width(&self, col: usize) -> f32 {
        self.widths.get(col).copied().unwrap_or(MIN_COLUMN_WIDTH)
    }

    /// 把横向滚动移到某一列的左缘（宿主「跳转列」用）。
    ///
    /// ⚠️ 两处口径必须与行区渲染（`table.rs` 的 `slot_x`）一致，否则「跳转」会停在
    /// 一个空档上：
    ///
    /// 1. 前缀和按**可见列**逐列累加 —— [`Self::offsets`] 是**全部列**的前缀和，
    ///    藏过列之后与画面上的位置对不上（藏列的宽度仍在里面）；
    /// 2. `ScrollHandle` 的偏移**越滚越负**（gpui `Div::set_offset` 的文档：偏移是
    ///    容器左上到第一个子元素左上的距离，「往下滚得越多越负」），且 prepaint 里会
    ///    钳到 `[-max, 0]`。传正值会被钳回 0 —— 表现就是「点了完全没反应」。
    ///
    /// 目标列右侧剩余宽度不足时，上面的钳位会把偏移停在最大滚动量：列贴不到最左缘，
    /// 那是横向滚动的物理边界（「后面没有了就是没有了」），不是缺陷。
    pub fn scroll_to_column(&mut self, col: usize, cx: &mut Context<Self>) {
        let mut x = ROWNUM_WIDTH + SELECT_COLUMN_WIDTH;
        let mut found = false;
        for (index, _) in self.visible_columns() {
            if index == col {
                found = true;
                break;
            }
            x += self.widths.get(index).copied().unwrap_or(MIN_COLUMN_WIDTH);
        }
        // 藏起来的列不是合法目标：滚过去也在屏幕上找不到它（宿主已过滤，这是防漏）。
        if !found {
            return;
        }
        self.h_scroll
            .set_offset(gpui::point(gpui::px(-x), gpui::px(0.)));
        cx.notify();
    }

    // ── 列宽拖拽（把手在列头右缘；双击把手 = 按内容自适应）──────

    /// 按下把手：双击走「自适应」，否则进拖动态。
    pub fn begin_resize(&mut self, col: usize, x: f32, click_count: usize, cx: &mut Context<Self>) {
        if click_count >= 2 {
            self.resize = None;
            if let Some(width) = self.model.estimated_width(col) {
                self.set_width(col, width, cx);
            }
            return;
        }
        self.resize = Some(ResizeDrag {
            col,
            start_x: x,
            start_width: self.column_width(col),
        });
    }

    /// 拖动中：按位移改列宽（夹在 `[MIN, MAX]`）。
    pub fn drag_resize(&mut self, col: usize, x: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.resize else {
            return;
        };
        if drag.col != col {
            return;
        }
        self.set_width(col, drag.start_width + (x - drag.start_x), cx);
    }

    /// 收尾：清拖动态并把最终宽度报给宿主（宿主可落盘）。
    pub fn end_resize(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.resize.take() else {
            return;
        };
        let width = self.column_width(drag.col);
        cx.emit(DataTableEvent::ColumnResized {
            col: drag.col,
            width,
        });
    }

    /// 写列宽（夹在上下限之间；无变化不重绘）。
    pub fn set_width(&mut self, col: usize, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH);
        let Some(slot) = self.widths.get_mut(col) else {
            return;
        };
        if (*slot - width).abs() <= f32::EPSILON {
            return;
        }
        *slot = width;
        self.offsets = column_offsets(&self.widths);
        cx.notify();
    }
}
