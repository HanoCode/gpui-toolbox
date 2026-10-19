//! 结果表的两种形态（只读 / 可编辑）与可编辑状态。
//!
//! 为什么从 `tool_run.rs` 搬出来：那个文件已经 2,900+ 行，而 v3 的计划是把它
//! 压到 400 行以内。结果表两种形态加起来近 300 行，属于「看着像页面、其实是
//! 控件」的那部分——先搬出去，比将来跟着页面一起拆便宜。
//!
//! **可编辑的唯一开关是清单声明**（`[io.output.cells]`）：没声明就只读，宿主
//! 不猜「这列看着像能改」。理由见 `OutputSpec::cells` 的注释。

use std::collections::HashMap;

use duck_components::dropdown_select;
use duck_plugin::{CellEdit, CellKind, CellSpec, TableDoc};
use duck_theme::active_theme;
use gpui::{
    AnyElement, App, AppContext as _, Entity, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    h_flex,
    input::{Input, InputState},
    v_flex,
};

/// 一个可编辑单元格的当前值。
enum Slot {
    /// 文本格：值在 `InputState` 里，边打边改，不需要回调。
    Text(Entity<InputState>),
    /// 下拉格：值在宿主侧，改动靠 `on_select` 回写。
    ///
    /// `injected` = 「这个值是宿主为了别显示空白而塞的」——不是用户的选择。
    /// ⚠️ 不记这个标志的话，一个原本**空**的格子会因为「显示成第一个候选」而被
    /// 算成脏改动，用户什么都没点，「应用」就把空字段悄悄改成了 `string`。
    Select { value: String, injected: bool },
}

/// 一张可编辑结果表的编辑状态。
///
/// 键用「行下标 + 列名」而不是「行下标 + 列下标」：列顺序变了不会串格，而
/// `columns` 本来就由插件在运行期给。
pub struct TableEditor {
    /// 原表（用来算「这一格原来是什么」）。
    doc: TableDoc,
    /// 声明了可编辑的列。
    specs: Vec<CellSpec>,
    /// 键 = (行下标, 列名)。
    slots: HashMap<(usize, String), Slot>,
}

impl TableEditor {
    /// 按声明给每个可编辑单元格建控件。
    ///
    /// 找不到列名、或该行没有这一格时**跳过**：宁可少一个输入框，也不让
    /// 整张表因为一处声明对不上而渲染不出来。
    pub fn new(doc: TableDoc, specs: &[CellSpec], window: &mut Window, cx: &mut App) -> Self {
        let mut slots = HashMap::new();
        for (row, cells) in doc.rows.iter().enumerate() {
            for spec in specs {
                let Some(column) = doc.columns.iter().position(|name| name == &spec.column) else {
                    continue;
                };
                let current = cells.get(column).cloned().unwrap_or_default();
                let slot = match spec.kind {
                    CellKind::Text => {
                        // ⚠️ 不能用 `default_value`：它不进 `value()`（Number 参数
                        // 那边就是靠这个差别回落到清单默认值的），而我们恰恰要
                        // 「当前值就是初始值」，否则每个格子都会被判成「被清空」。
                        let state = cx.new(|cx| {
                            let mut state = InputState::new(window, cx);
                            state.set_value(current.clone(), window, cx);
                            state
                        });
                        Slot::Text(state)
                    }
                    CellKind::Select => {
                        // 空值落到第一个候选：下拉显示空白等于让人以为没得选。
                        let injected = current.is_empty();
                        let value = if injected {
                            spec.options
                                .first()
                                .map(|option| option.value.clone())
                                .unwrap_or_default()
                        } else {
                            current
                        };
                        Slot::Select { value, injected }
                    }
                };
                slots.insert((row, spec.column.clone()), slot);
            }
        }
        Self {
            doc,
            specs: specs.to_vec(),
            slots,
        }
    }

    /// 脏改动：只收「与原值不同」的格子。
    ///
    /// 收全部而不是只收脏的会让每次重跑都重写一遍没动过的字段——插件那边就
    /// 分不清「用户真改了」和「宿主动了手」。
    #[must_use]
    pub fn edits(&self, cx: &App) -> Vec<CellEdit> {
        let mut edits = Vec::new();
        for (row, cells) in self.doc.rows.iter().enumerate() {
            for spec in &self.specs {
                let Some(column) = self
                    .doc
                    .columns
                    .iter()
                    .position(|name| name == &spec.column)
                else {
                    continue;
                };
                let before = cells.get(column).cloned().unwrap_or_default();
                let Some(slot) = self.slots.get(&(row, spec.column.clone())) else {
                    continue;
                };
                let now = match slot {
                    Slot::Text(state) => state.read(cx).value().to_string(),
                    // 「宿主塞的默认值」不算改动：用户没碰过这一格。
                    Slot::Select { injected: true, .. } => continue,
                    Slot::Select { value, .. } => value.clone(),
                };
                if now != before {
                    edits.push(CellEdit {
                        row_id: self.doc.row_id(row),
                        column: spec.column.clone(),
                        value: now,
                    });
                }
            }
        }
        edits
    }

    /// 取某一格的文本控件句柄（渲染期用；克隆开销是一次 `Entity` 引用计数）。
    fn text_state(&self, row: usize, column: &str) -> Option<Entity<InputState>> {
        match self.slots.get(&(row, column.to_owned())) {
            Some(Slot::Text(state)) => Some(state.clone()),
            _ => None,
        }
    }

    /// 取某一格的当前选中值（渲染期用）。
    fn select_value(&self, row: usize, column: &str) -> Option<String> {
        match self.slots.get(&(row, column.to_owned())) {
            Some(Slot::Select { value, .. }) => Some(value.clone()),
            _ => None,
        }
    }
}

/// 渲染一张结果表。
///
/// `editor` 为 `None`（没声明可编辑列，或表是从 Markdown 回退来的）时整表只读。
pub fn render_table(
    doc: &TableDoc,
    editor: Option<&Entity<TableEditor>>,
    id_prefix: &str,
    cx: &mut App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens().clone();
    let extended = theme.extended().clone();
    let xs = tokens.typography.xs;
    let sm = tokens.typography.sm;
    let hairline = extended.colors.hairline;

    // 先把「每列的可编辑规格」取成快照：循环里再 `read` 会与外层的 `&mut App`
    // 借用来回打架，而这份快照每次渲染只算一次。
    let column_specs: Vec<Option<CellSpec>> = doc
        .columns
        .iter()
        .map(|column| {
            editor.and_then(|editor| {
                editor
                    .read(cx)
                    .specs
                    .iter()
                    .find(|spec| &spec.column == column)
                    .cloned()
            })
        })
        .collect();

    let mut header = h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs);
    for title in &doc.columns {
        header = header.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(xs.size)
                .line_height(xs.line_height)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(extended.colors.text_tertiary)
                .child(SharedString::from(title.clone())),
        );
    }

    let mut body = v_flex()
        .id(SharedString::from(format!("{id_prefix}-table-body")))
        .w_full()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px(tokens.spacing.sm)
        .pb(tokens.spacing.sm)
        .gap(tokens.spacing.xxs);

    for (row_index, row) in doc.rows.iter().enumerate() {
        let mut line = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .rounded(tokens.radius.sm)
            .py(tokens.spacing.xxs)
            .when(row_index % 2 == 1, |line| {
                line.bg(extended.colors.hover_tint)
            });
        // 直接迭代快照（它的长度就是列数），别用下标循环：clippy 会指出
        // 「拿循环变量去索引另一个数组」这个反模式。
        for (column_index, spec) in column_specs.iter().enumerate() {
            let spec = spec.clone();
            let cell = spec.and_then(|spec| match spec.kind {
                CellKind::Text => editor
                    .and_then(|editor| editor.read(cx).text_state(row_index, &spec.column))
                    .map(|state| Input::new(&state).w_full().into_any_element()),
                CellKind::Select => {
                    let current = editor
                        .and_then(|editor| editor.read(cx).select_value(row_index, &spec.column))
                        .unwrap_or_default();
                    let options: Vec<(SharedString, SharedString)> = spec
                        .options
                        .iter()
                        .map(|option| {
                            (
                                SharedString::from(option.value.clone()),
                                SharedString::from(option.label.clone()),
                            )
                        })
                        .collect();
                    let target = editor.cloned();
                    let key = (row_index, spec.column.clone());
                    Some(
                        dropdown_select(
                            SharedString::from(format!(
                                "{id_prefix}-cell-{row_index}-{}",
                                spec.column
                            )),
                            options,
                            current,
                            move |value, _, cx| {
                                if let Some(editor) = target.as_ref() {
                                    // 只改宿主侧的值，不触发重跑：用户可能连着改
                                    // 好几格，一次「应用」提交才是一次执行。
                                    editor.update(cx, |editor, cx| {
                                        if let Some(Slot::Select {
                                            value: slot,
                                            injected,
                                        }) = editor.slots.get_mut(&key)
                                        {
                                            *slot = value.to_string();
                                            // 用户真选了，就不再是「宿主塞的」。
                                            *injected = false;
                                        }
                                        cx.notify();
                                    });
                                }
                            },
                            cx,
                        )
                        .into_any_element(),
                    )
                }
            });
            line = line.child(match cell {
                Some(cell) => div().flex_1().min_w_0().child(cell),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(sm.size)
                    .line_height(sm.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(
                        row.get(column_index).cloned().unwrap_or_default(),
                    )),
            });
        }
        body = body.child(line);
    }

    v_flex()
        .w_full()
        .flex_1()
        .min_h_0()
        .rounded(tokens.radius.md)
        .border(extended.stroke.thin)
        .border_color(tokens.colors.border)
        .bg(tokens.colors.surface)
        .child(header.border_b(extended.stroke.thin).border_color(hairline))
        .child(div().flex_1().min_h_0().pt(tokens.spacing.xs).child(body))
        .into_any_element()
}
