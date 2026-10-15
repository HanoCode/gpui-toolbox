//! 数据表的渲染：固定列头 + 行虚拟化 + 横向滚动（表头与行区共用一个横向容器）。
//!
//! 四项性能口径（照 dbflux `data_table/table.rs`，也是「一点都不卡顿」的根子）：
//!
//! 1. **每帧只读可见行**：行数据在 `uniform_list` 的 items 闭包里按需取，
//!    整表只 `Arc::clone` 一次，绝不逐帧深拷贝；
//! 2. **单元格文本已预算**：渲染只 bump `Arc<str>` 引用计数，不做 `format!` / 整串复制；
//! 3. **只把本帧要用的几何拷出来**（列宽 / 展示行序都是小向量），元素 id 走整数编码；
//! 4. **列也做虚拟化**（见 [`RowSlot`]）：横向视口外的列不建元素。原先只有行虚拟化，
//!    一屏 36 行 × 40 列里滑出去的那二十多列**每帧照样建、照样排版、照样画** ——
//!    实测宽表一帧的分配量有六成花在看不见的列上。裁掉之后一帧成本只与**可视列数**
//!    成正比，与表有多宽无关（1000×20 与 1000×40 每帧开销已基本相同）。

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, CursorStyle, ElementId, Entity,
    InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Toggled, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_component::{
    Icon, Sizable as _, h_flex,
    input::{Input, InputState},
    menu::ContextMenuExt as _,
    scroll::{Scrollbar, ScrollbarMode},
    tooltip::Tooltip,
    v_flex,
};

use crate::data_table::events::{CellPos, DataTableEvent, DataTableHooks, DataTableLabels};
use crate::data_table::model::{
    HEADER_HEIGHT, RESIZE_HANDLE_WIDTH, ROW_HEIGHT, ROWNUM_WIDTH, SELECT_COLUMN_WIDTH,
    fill_template,
};
use crate::data_table::state::DataTableState;
use crate::{CheckState, DuckIcon, check_mark};

/// 宿主侧传入的**展示态**（真源在宿主：选中 / 勾选 / 排序 / 就地编辑）。
#[derive(Default)]
pub struct DataTableView<'a> {
    /// 选中行（模型行下标）。
    pub selected_row: Option<usize>,
    /// 勾选的行（模型行下标）。
    pub checked: Option<&'a BTreeSet<usize>>,
    /// 列头可否点击排序（SQL 结果集的列头不可排：内容归用户的 `ORDER BY`）。
    pub sortable: bool,
    /// 排序的列名（画箭头用）。
    pub sort_column: Option<&'a str>,
    /// 该列是否降序。
    pub sort_descending: bool,
    /// 就地编辑中的格子。
    pub editing: Option<CellPos>,
    /// 就地编辑中的输入框（`InputState` 必须由宿主在 `window` 上建）。
    pub editing_input: Option<Entity<InputState>>,
    /// 「一行没中」的原位提示（宿主按当前搜索词拼好；不搜就给 `None`）。
    pub empty_hint: Option<SharedString>,
}

// ⚠️ 元素 id 用**整数编码**（`ElementId::Integer`），不是 `format!` 出来的字符串：
// 单元格 id 每帧每格都要一个，字符串 id 等于每帧几百次分配。各「种类」占一档高位，
// 同行同列的格子/勾选框/ⓘ 才不会撞在同一个 id 上。
const ID_ROW: u64 = 1 << 60;
const ID_CELL: u64 = 2 << 60;
const ID_CELL_INFO: u64 = 3 << 60;
const ID_CELL_SUBMIT: u64 = 4 << 60;
const ID_ROW_CHECK: u64 = 5 << 60;
const ID_HEAD: u64 = 6 << 60;
const ID_HANDLE: u64 = 7 << 60;

/// 悬停 ⓘ 底片的边长（正方形，居中在行高里）。
const INFO_BADGE: f32 = 18.;

/// 横向裁剪时窗口两侧各多留的像素（约两列宽）：偏移与视口宽度差一帧，
/// 留一屏外缓冲区，滚动时不会露出「还没建出来」的空白格。
const COL_OVERSCAN: f32 = 320.;

/// 一行里要处理的一「槽」：真的建元素的列，或被横向裁掉的一段空白。
///
/// 裁剪段必须留**等宽占位**：行是 `w_full()` 的，但格子的 x 位置是顺序排出来
/// 的，少了这一段后面的列会整体左移（横向滚动时与表头错位）。
#[derive(Clone, Copy)]
enum RowSlot {
    /// 被裁掉的连续列（宽 = 这些列宽之和），只占位不建元素。
    Skip(f32),
    /// 要画的列。
    Cell { col: usize, width: f32 },
}

const fn index_key(kind: u64, row: usize, col: usize) -> u64 {
    kind | ((row as u64) << 24) | (col as u64)
}

/// 列宽拖拽的载荷（框架托管一份，`on_drag_move` 只认这个类型）。
#[derive(Clone)]
struct ColumnResizeDrag {
    col: usize,
}

impl Render for ColumnResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 跟手的是光标形状（`CursorStyle::ResizeColumn`），不需要拖影。
        gpui::Empty
    }
}

/// 渲染数据表主体（横向滚动区 + 滚动条 + 固定列头 + 行区）。
///
/// 返回的元素自带 `flex_1()`：直接塞进「结果 tab 栏 / 脚注」那一列即可
/// （宿主不必再包一层滚动容器）。
pub fn data_table(
    id: &'static str,
    state: &Entity<DataTableState>,
    view: &DataTableView<'_>,
    labels: &DataTableLabels,
    hooks: &DataTableHooks,
    cx: &App,
) -> AnyElement {
    let theme = crate::active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    // 跨入 `'static` 闭包的量一律先拷成 `Copy` / 小对象（`uniform_list` 的 items
    // 闭包要求 `'static`）。
    let hairline = extended.colors.hairline;
    let thin = extended.stroke.thin;
    let hover_tint = extended.colors.hover_tint;
    let surface_bg = tokens.colors.surface;
    let rownum_bg = extended.colors.chrome;
    let selected_bg = extended.colors.nav_selected;
    let tertiary = extended.colors.text_tertiary;
    let muted = tokens.colors.muted_foreground;
    let cell_pad = tokens.spacing.xs;
    let cell_text = tokens.typography.sm.size;
    let head_text = tokens.typography.xs.size;
    let icon_sm = extended.icon.sm;
    let radius_sm = tokens.radius.sm;
    let cell_gap = tokens.spacing.xxs;
    let edit_strong = extended.stroke.strong;
    let edit_ring = tokens.colors.ring;
    let selection_bg = tokens.colors.selection;
    let accent_text = extended.colors.accent_text;
    let success = extended.colors.success;

    // 本帧要用的数据 + 几何快照（都是小向量；行数据只 `Arc::clone`）。
    let (model, view_rows, row_offset, visible) = {
        let s = state.read(cx);
        let visible: Vec<(usize, Arc<str>, Arc<str>, f32)> = s
            .visible_columns()
            .map(|(index, column)| {
                (
                    index,
                    column.name.clone(),
                    column.type_name.clone(),
                    s.column_width(index),
                )
            })
            .collect();
        (
            s.model().clone(),
            s.view_rows().to_vec(),
            s.row_offset(),
            visible,
        )
    };
    let h_scroll = state.read(cx).h_scroll().clone();
    let v_scroll = state.read(cx).v_scroll().clone();

    // 宿主真源在渲染期**拷成 `'static` 能带走的小值**（行 / 列下标与集合都小）。
    let selected_row = view.selected_row;
    let editing = view.editing;
    let editing_input = view.editing_input.clone();
    let sortable = view.sortable;
    let sort_column = view.sort_column.map(str::to_owned);
    let sort_descending = view.sort_descending;
    let empty_hint = view.empty_hint.clone();
    let checked: BTreeSet<usize> = view.checked.cloned().unwrap_or_default();
    let checked_count = checked.len();
    let row_count = model.row_count();
    let all_checked = row_count > 0 && checked_count == row_count;
    let head_state = if checked_count == 0 {
        CheckState::Unchecked
    } else if all_checked {
        CheckState::Checked
    } else {
        CheckState::Indeterminate
    };
    // 文案也要先克隆成 owned：`uniform_list` 的 items 闭包是 `'static`，
    // `&DataTableLabels` 活不到那时候。
    let row_name_label = labels.row_name.clone();
    let cell_detail_label = labels.cell_detail.clone();
    let submit_edit_label = labels.submit_edit.clone();
    let cell_menu = hooks.cell_menu.clone();
    let header_menu = hooks.header_menu.clone();
    let on_cell_double_click = hooks.on_cell_double_click.clone();
    let on_row_double_click = hooks.on_row_double_click.clone();

    // ── 列头 ─────────────────────────────────────────────────────────────
    // 把手绝对定位在**列头行**上（列头格自己有 `overflow_x_hidden`，嵌进去会被裁掉
    // 一半而够不着）；最后追加 = 绘制顺序靠后 = 优先接到拖拽。
    let mut resize_offset = ROWNUM_WIDTH + SELECT_COLUMN_WIDTH;
    let resize_handles = visible.iter().map(|(index, _, _, width)| {
        let col = *index;
        let left = resize_offset + width - RESIZE_HANDLE_WIDTH / 2.;
        resize_offset += width;
        let state_move = state.clone();
        let state_drag = state.clone();
        let state_end = state.clone();
        div()
            .id(ElementId::Integer(index_key(ID_HANDLE, 0, col)))
            .absolute()
            .top_0()
            .left(px(left))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h(px(HEADER_HEIGHT))
            .occlude()
            .cursor(CursorStyle::ResizeColumn)
            .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
                let x = f32::from(event.position.x);
                state_move.update(cx, |this, cx| {
                    this.begin_resize(col, x, event.click_count, cx);
                });
            })
            .on_drag(ColumnResizeDrag { col }, |drag, _, _, cx| {
                // 别让这次按下再冒泡成「点列头排序」。
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move::<ColumnResizeDrag>(move |event, _, cx| {
                // 载荷里带的是**真正在拖的那一列**：多列把手各自注册了同一个监听，
                // 用载荷的列号才不会被光标掠过的邻列抢走。
                let dragged = event.drag(cx).col;
                let x = f32::from(event.event.position.x);
                state_drag.update(cx, |this, cx| this.drag_resize(dragged, x, cx));
            })
            .on_mouse_up_out(gpui::MouseButton::Left, move |_, _, cx| {
                state_end.update(cx, |this, cx| this.end_resize(cx));
            })
    });
    let mut header = h_flex()
        .relative()
        .flex_none()
        .h(px(HEADER_HEIGHT))
        .bg(surface_bg)
        .border_b(thin)
        .border_color(hairline)
        .overflow_x_hidden()
        .child(
            div()
                .w(px(ROWNUM_WIDTH))
                .h(px(HEADER_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .bg(rownum_bg)
                .border_r(thin)
                .border_color(hairline)
                .text_size(head_text)
                .text_color(muted)
                .child("#"),
        )
        .child(
            div()
                .w(px(SELECT_COLUMN_WIDTH))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .border_r(thin)
                .border_color(hairline)
                .child(checkbox(
                    ElementId::NamedInteger(SharedString::from(id), 1),
                    head_state,
                    labels.select_all.clone(),
                    {
                        let state = state.clone();
                        move |_, _, cx| {
                            let target = !all_checked;
                            state.update(cx, |_, cx| {
                                cx.emit(DataTableEvent::AllCheckedToggled { checked: target });
                            });
                        }
                    },
                    cx,
                )),
        );
    for (index, name, type_name, width) in &visible {
        let col = *index;
        let arrow = if sortable {
            match sort_column.as_deref() {
                Some(active) if active == name.as_ref() => {
                    if sort_descending {
                        Some(" ↓")
                    } else {
                        Some(" ↑")
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        let label: SharedString = match arrow {
            Some(arrow) => SharedString::from(format!("{}{arrow}", name.as_ref())),
            // 无箭头时直接借 `Arc<str>`（`From<&Arc<str>>` 只 bump 引用计数）；
            // 原先走 `name.to_string()`，等于每帧把每个列名整串复制一遍。
            None => SharedString::from(name),
        };
        let mut cell = div()
            .id(ElementId::Integer(index_key(ID_HEAD, 0, col)))
            .w(px(*width))
            .flex_none()
            .px(cell_pad)
            .flex()
            .flex_col()
            .justify_center()
            .overflow_x_hidden()
            .border_r(thin)
            .border_color(hairline)
            .text_size(head_text)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(muted)
            .child(div().flex_none().truncate().child(label))
            // 类型行：引擎报什么画什么；拿不到类型就只画一行，别摆空档。
            .when(!type_name.is_empty(), |cell| {
                cell.child(
                    div()
                        .flex_none()
                        .truncate()
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(tertiary)
                        // 同上：类型名也是 `Arc<str>`，零拷贝借出去。
                        .child(SharedString::from(type_name)),
                )
            });
        if sortable {
            let sort_label = fill_template(labels.sort_by.as_ref(), &[name.as_ref()]);
            cell = cell
                .cursor_pointer()
                .role(Role::Button)
                .aria_label(SharedString::from(sort_label))
                .hover(move |style| style.bg(hover_tint))
                .on_click({
                    let state = state.clone();
                    let column = name.clone();
                    move |_, _, cx| {
                        state.update(cx, |_, cx| {
                            cx.emit(DataTableEvent::SortRequested {
                                column: column.clone(),
                            });
                        });
                    }
                });
        }
        let cell = match header_menu.as_ref() {
            Some(hook) => {
                let hook = hook.clone();
                cell.context_menu(move |menu, window, cx| hook(menu, col, window, cx))
                    .into_any_element()
            }
            None => cell.into_any_element(),
        };
        header = header.child(cell);
    }
    let header = header.children(resize_handles);

    // ── 横向裁剪：本帧真的要建元素的列 ───────────────────────────────────
    // 列不做虚拟化的话，`visible` 里**每一列**每帧都建一格：40 列的表里滑出屏幕
    // 的二十多列照样建元素、照样排版、照样画。横向视口只有一千多像素，一屏
    // 36 行 × 40 列 = 1440 个格子里一大半从未进过视口。
    //
    // 这里按**横向可视窗口**（两侧各留 [`COL_OVERSCAN`] 余量）先裁一遍，只建
    // 窗口内的列；被裁掉的段用 [`RowSlot::Skip`] 等宽占位，行宽与表头一致，
    // 横向滚动几何不变。
    //
    // ⚠️ 视口宽度来自 `ScrollHandle::bounds()`，它在**容器的 prepaint** 里才写
    // （见 gpui `elements/div.rs`），所以这里读到的是上一帧的布局；偏移
    // `offset()` 是本帧的实时值。第一帧量不出视口 ⇒ `viewport_w <= 0` ⇒ 一律
    // 不裁 —— 绝不能因为「还没量出来」把网格画成空的。
    let h_offset = -f32::from(h_scroll.offset().x);
    let viewport_w = f32::from(h_scroll.bounds().size.width);
    let col_window = (viewport_w > 0.).then_some((
        h_offset - COL_OVERSCAN,
        h_offset + viewport_w + COL_OVERSCAN,
    ));
    let mut slots: Vec<RowSlot> = Vec::with_capacity(visible.len() + 1);
    let mut slot_x = ROWNUM_WIDTH + SELECT_COLUMN_WIDTH;
    let mut skipped = 0.0_f32;
    for (col, _, _, width) in &visible {
        let keep = col_window.is_none_or(|(left, right)| {
            // 落进窗口；或者正是**就地编辑**的那一格（编辑框绝不能因为横向裁剪消失）。
            (slot_x + width > left && slot_x < right) || editing.is_some_and(|pos| pos.col == *col)
        });
        if keep {
            if skipped > 0. {
                slots.push(RowSlot::Skip(skipped));
                skipped = 0.;
            }
            slots.push(RowSlot::Cell {
                col: *col,
                width: *width,
            });
        } else {
            skipped += width;
        }
        slot_x += width;
    }
    // 尾部被裁掉的段不用占位：行本来就是 `w_full()`，后面没有东西需要对齐。

    // ── 行区（虚拟化：只渲染可见的几十行）───────────────────────────────
    let list_state = state.clone();
    let view_is_empty = view_rows.is_empty();
    let rows = uniform_list(id, view_rows.len().max(1), move |range, _, cx| {
        range
            .filter_map(|vis_index| {
                let index = *view_rows.get(vis_index)?;
                let row = model.row(index)?;
                let selected = selected_row == Some(index);
                let is_checked = checked.contains(&index);
                let editing_row = editing.map(|pos| pos.row) == Some(index);
                // 行勾选框的可访问名：模板 + 行号（文案留在宿主，组件只填参）。
                let row_number = (index + 1).to_string();
                let mut line = h_flex()
                    .id(ElementId::Integer(index_key(ID_ROW, index, 0)))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex_none()
                    .items_center()
                    .overflow_x_hidden()
                    // 行框线（dbx 同款：每行一条底线，格与行不再糊成一片）。
                    .border_b(thin)
                    .border_color(hairline)
                    .when(selected, |line| line.bg(selected_bg))
                    .hover(move |style| style.bg(hover_tint))
                    .on_click({
                        let state = list_state.clone();
                        let hook = on_row_double_click.clone();
                        move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let clicks = match event {
                                ClickEvent::Mouse(mouse) => mouse.up.click_count,
                                ClickEvent::Touch(tap) => tap.tap_count,
                                ClickEvent::Keyboard(_) => 1,
                            };
                            if clicks <= 1 {
                                state.update(cx, |_, cx| {
                                    cx.emit(DataTableEvent::RowSelected { row: index });
                                });
                                return;
                            }
                            if let Some(hook) = hook.as_ref() {
                                hook(index, window, cx);
                            }
                        }
                    })
                    // 行号列（设计稿 rownum）：显示**结果里的绝对序号**——客户端分页
                    // 翻到第 2 页从 201 起编，跟「第几行数据」一致。正在就地编辑的行，
                    // 行号跟着点亮（编辑态不止落在格子上，行号也参与指位）。
                    .child(
                        div()
                            .w(px(ROWNUM_WIDTH))
                            .h(px(ROW_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .px(cell_pad)
                            .bg(if editing_row { selection_bg } else { rownum_bg })
                            .border_r(thin)
                            .border_color(hairline)
                            .text_size(cell_text)
                            .text_color(if editing_row { accent_text } else { tertiary })
                            .child(SharedString::from(
                                (row_offset + vis_index + 1).to_string(),
                            )),
                    )
                    .child(
                        div()
                            .w(px(SELECT_COLUMN_WIDTH))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_r(thin)
                            .border_color(hairline)
                            .child(checkbox(
                                ElementId::Integer(index_key(ID_ROW_CHECK, index, 0)),
                                if is_checked {
                                    CheckState::Checked
                                } else {
                                    CheckState::Unchecked
                                },
                                SharedString::from(fill_template(
                                    row_name_label.as_ref(),
                                    &[row_number.as_str()],
                                )),
                                {
                                    let state = list_state.clone();
                                    move |_, _, cx| {
                                        // 勾选框是行内的独立点击目标：不拦一下，
                                        // 点它会把「选中这一行」也一起触发。
                                        cx.stop_propagation();
                                        let target = !is_checked;
                                        state.update(cx, |_, cx| {
                                            cx.emit(DataTableEvent::RowCheckedToggled {
                                                row: index,
                                                checked: target,
                                            });
                                        });
                                    }
                                },
                                cx,
                            )),
                    );
                for slot in &slots {
                    let (col, width) = match *slot {
                        RowSlot::Skip(width) => {
                            // 被横向裁掉的列：只占位，不建元素。
                            line = line.child(div().w(px(width)).h(px(ROW_HEIGHT)).flex_none());
                            continue;
                        }
                        RowSlot::Cell { col, width } => (col, width),
                    };
                    let position = CellPos::new(index, col);
                    let value = row.cell(col);
                    let text: Arc<str> = value
                        .map(|cell| cell.display())
                        .unwrap_or_else(|| "NULL".into());
                    let null_cell = value.is_none_or(|cell| cell.is_null());
                    let editing_here = editing == Some(position);
                    let mut element = div()
                        .id(ElementId::Integer(index_key(ID_CELL, index, col)))
                        .w(px(width))
                        .flex_none()
                        .px(cell_pad)
                        .flex()
                        .items_center()
                        .overflow_x_hidden()
                        .text_size(cell_text)
                        .line_height(px(ROW_HEIGHT))
                        // 格框线（dbx 同款：列与列之间一条竖线）。
                        .border_r(thin)
                        .border_color(hairline);
                    match (editing_here, editing_input.as_ref()) {
                        (true, Some(input)) => {
                            // 就地编辑的**整格**视觉：2px 强调描边框住整格 + 提亮底色。
                            // 输入框本身 appearance(false)，没有这层框的话编辑态与普通格
                            // 几乎没差别，用户看不出「已经进编辑」。
                            element = element
                                .bg(surface_bg)
                                .border(edit_strong)
                                .border_color(edit_ring)
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap(cell_gap)
                                        .child(
                                            Input::new(input)
                                                .appearance(false)
                                                .with_size(gpui_component::Size::Small)
                                                .flex_1()
                                                .min_w_0(),
                                        )
                                        .child(
                                            div()
                                                .id(ElementId::Integer(index_key(
                                                    ID_CELL_SUBMIT,
                                                    index,
                                                    col,
                                                )))
                                                .flex_none()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .w(px(18.))
                                                .h(px(18.))
                                                .rounded(radius_sm)
                                                .cursor_pointer()
                                                .role(Role::Button)
                                                .aria_label(submit_edit_label.clone())
                                                .hover(move |style| style.bg(hover_tint))
                                                .on_click({
                                                    let state = list_state.clone();
                                                    move |_, _, cx| {
                                                        cx.stop_propagation();
                                                        state.update(cx, |_, cx| {
                                                            cx.emit(
                                                                DataTableEvent::CellEditSubmitRequested(
                                                                    position,
                                                                ),
                                                            );
                                                        });
                                                    }
                                                })
                                                .child(
                                                    Icon::new(DuckIcon::Check)
                                                        .size(icon_sm)
                                                        .text_color(success),
                                                ),
                                        ),
                                );
                        }
                        _ => {
                            // 悬停 ⓘ（dbx 同款）：详情入口贴在每格上，平时隐形、
                            // 悬停所在格才现身（0/1 是显隐开关，不是透明档位；
                            // `group` 与 `group_hover` 必须成对，组名带行列号防串格）。
                            let group = SharedString::from(format!("dt-cell-group-{index}-{col}"));
                            let mut text_div = div().truncate().child(SharedString::from(text));
                            if null_cell {
                                text_div = text_div.text_color(tertiary);
                            }
                            element = element
                                .group(group.clone())
                                .relative()
                                .child(text_div)
                                .child(
                                    // ⓘ 的定位与底片合成**一层**（原先内层还套了一个
                                    // 18×18 的包装用于居中）。几何完全等价：底片仍是
                                    // 18×18、贴着右缘 2px、在行高里垂直居中。
                                    div()
                                        .id(ElementId::Integer(index_key(
                                            ID_CELL_INFO,
                                            index,
                                            col,
                                        )))
                                        .absolute()
                                        .right(px(2.))
                                        .top(px((ROW_HEIGHT - INFO_BADGE) / 2.))
                                        .w(px(INFO_BADGE))
                                        .h(px(INFO_BADGE))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(radius_sm)
                                        .bg(surface_bg)
                                        .cursor_pointer()
                                        .opacity(0.)
                                        .group_hover(group, |style| style.opacity(1.))
                                        .role(Role::Button)
                                        .aria_label(cell_detail_label.clone())
                                        .tooltip({
                                            let tip = cell_detail_label.clone();
                                            move |window, cx| {
                                                Tooltip::new(tip.clone()).build(window, cx)
                                            }
                                        })
                                        .on_click({
                                            let state = list_state.clone();
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                state.update(cx, |_, cx| {
                                                    cx.emit(DataTableEvent::CellDetailRequested(
                                                        position,
                                                    ));
                                                });
                                            }
                                        })
                                        .child(
                                            Icon::new(DuckIcon::Info)
                                                .size(icon_sm)
                                                .text_color(tertiary),
                                        ),
                                );
                        }
                    }
                    element = element.on_click({
                        let state = list_state.clone();
                        let hook = on_cell_double_click.clone();
                        move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                            // 不拦一下，点击会先落到「选中这一行」上。
                            cx.stop_propagation();
                            let double = match event {
                                ClickEvent::Mouse(mouse) => mouse.up.click_count > 1,
                                ClickEvent::Touch(tap) => tap.tap_count > 1,
                                ClickEvent::Keyboard(_) => false,
                            };
                            if double {
                                if let Some(hook) = hook.as_ref() {
                                    hook(position, window, cx);
                                }
                                return;
                            }
                            state.update(cx, |_, cx| {
                                cx.emit(DataTableEvent::RowSelected { row: index });
                                cx.emit(DataTableEvent::CellSelected(position));
                            });
                        }
                    });
                    if let Some(menu) = cell_menu.as_ref() {
                        let hook = menu.clone();
                        line = line.child(
                            element
                                .context_menu(move |menu, window, cx| {
                                    hook(menu, position, window, cx)
                                })
                                .into_any_element(),
                        );
                    } else {
                        line = line.child(element);
                    }
                }
                Some(line)
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .min_h_0()
    // 纵向滚动句柄由组件状态持有（跨帧持久）：纵向滚动条从它读位置。
    .track_scroll(&v_scroll);

    let content = v_flex()
        .flex_none()
        .w_auto()
        .min_w_full()
        .child(header)
        // 搜了个词却一行没中：必须明说，否则「空网格」与「查询返回了 0 行」分不清
        // （前者清掉搜索词就有数据）——与「查询返回 0 行」的空态是两回事，后者由宿主画。
        .when(view_is_empty, |content| match empty_hint {
            Some(hint) => content.child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .text_size(tokens.typography.xs.size)
                    .text_color(muted)
                    .child(hint),
            ),
            None => content,
        })
        .child(rows);

    // ── 横向滚动区（表头与行网格共用一份横向偏移，滚动时不会错位）──────────
    // 列头、过滤条、编辑条、批量条、翻页条都留在滚动区**外**，保持固定。
    div()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .relative()
        .child(
            div()
                .id(SharedString::from(format!("{id}-x-area")))
                .size_full()
                .flex()
                .flex_row()
                .overflow_x_scroll()
                // 锁轴：竖向滚轮只归行列表纵向滚动，不会被映射成横向滚动；
                // 横向靠触控板横扫、Shift + 滚轮或拖这条滚动条。
                .restrict_scroll_to_axis()
                .track_scroll(&h_scroll)
                .child(content),
        )
        .child(
            div().absolute().inset_0().child(
                Scrollbar::horizontal(&h_scroll)
                    .id(SharedString::from(format!("{id}-x-scrollbar")))
                    // 常驻：包装器默认 Hover（悬停才现身），数据网格里等于没有可发现的
                    // 滚动条；无溢出时 Scrollbar 自己会隐藏，不会摆一条死轨道。
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout(),
            ),
        )
        .child(
            div().absolute().inset_0().child(
                Scrollbar::vertical(&v_scroll)
                    .id(SharedString::from(format!("{id}-v-scrollbar")))
                    // 滚动时现身、停下淡出：行多时常驻太重，悬停才现身又等于不可发现。
                    .mode(ScrollbarMode::Scrolling),
            ),
        )
        .into_any_element()
}

/// 行首等多处共用的复选框（纯图标，名字必须**显式**给：AccessKit 的 `CheckBox`
/// 读不出 `check_mark` 里那个图标）。
fn checkbox(
    id: impl Into<ElementId>,
    state: CheckState,
    label: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = crate::active_theme(cx);
    let hover = theme.extended().colors.hover_tint;
    let press = theme.extended().colors.hover_tint_press;
    let ring = theme.tokens().colors.ring;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(ROW_HEIGHT))
        .rounded(theme.tokens().radius.sm)
        .cursor_pointer()
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(match state {
            CheckState::Checked => Toggled::True,
            CheckState::Unchecked => Toggled::False,
            CheckState::Indeterminate => Toggled::Mixed,
        })
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(press))
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.border_1().border_color(ring))
        .on_click(on_click)
        .child(check_mark(state, cx))
        .into_any_element()
}
