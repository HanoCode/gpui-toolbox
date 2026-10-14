//! 数据表：列定义 + 表头 + 行 + 空态 / 加载态。
//!
//! 上游 `gpui_component::table` 走 `TableDelegate` + 虚拟化，适合上千行的数据集；
//! 工具箱里的列表通常几十行，手写行更省事也更好控（列宽、行内按钮、勾选列都要自己定）。
//! 所以这里给的是**表壳**：把列头、行区、空态、加载态四块拼成一套统一外观，并把
//! [`super::feedback`] 的空状态与骨架接进来；需要虚拟化大数据集时直接用上游 `Table`。

use duck_theme::active_theme;
use gpui::{
    AnyElement, App, Div, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, RenderOnce, SharedString, StatefulInteractiveElement as _, Styled,
    Window, div, px,
};
use gpui_component::{h_flex, v_flex};

use crate::{DuckIcon, content::feedback, tabular_numbers};

/// 列内内容的水平对齐。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TableAlign {
    /// 左对齐（文本列的默认值）。
    #[default]
    Start,
    /// 居中。
    Center,
    /// 右对齐（数字 / 计数列）。
    End,
}

/// 列定义。
///
/// `id` 是列的稳定标识，同时用于派生元素 id（不能用下标：列随页面的显隐配置变化，
/// 下标做键会让状态错位）；`title` 是表头文案；`width` 为空表示这一列吃剩余宽度。
#[derive(Debug, Clone)]
pub struct DataColumn {
    id: SharedString,
    title: SharedString,
    width: Option<Pixels>,
    align: TableAlign,
}

impl DataColumn {
    /// 创建一列。
    pub fn new(id: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            width: None,
            align: TableAlign::Start,
        }
    }

    /// 固定列宽；不设置则吃剩余宽度。
    #[must_use]
    pub fn width(mut self, width: Pixels) -> Self {
        self.width = Some(width);
        self
    }

    /// 列内内容的水平对齐。
    #[must_use]
    pub fn align(mut self, align: TableAlign) -> Self {
        self.align = align;
        self
    }

    /// 列标识。
    #[must_use]
    pub fn id(&self) -> &SharedString {
        &self.id
    }

    /// 表头文案。
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// 固定列宽；`None` 表示吃剩余宽度。
    #[must_use]
    pub const fn width_px(&self) -> Option<Pixels> {
        self.width
    }

    /// 列内内容的水平对齐。
    #[must_use]
    pub const fn alignment(&self) -> TableAlign {
        self.align
    }
}

/// 表体状态。
enum Body {
    /// 默认：既没灌行、也没声明空态 —— 只留白（无数据时调用方应调 [`DataTable::empty`]）。
    Blank,
    /// 正常：渲染行。
    Rows(Vec<AnyElement>),
    /// 加载中：渲染骨架行。
    Loading,
    /// 空：渲染空状态。
    Empty {
        icon: DuckIcon,
        title: SharedString,
        description: Option<SharedString>,
    },
}

/// 数据表外壳：表头 + 行区，行区按状态渲染行 / 骨架 / 空态。
///
/// ```ignore
/// DataTable::new("tools", columns, cx)
///     .rows(rows)
///     .loading(is_loading)
///     .empty(DuckIcon::Search, tr(cx, "没有匹配的工具。"), None)
/// ```
pub struct DataTable {
    id: ElementId,
    columns: Vec<DataColumn>,
    body: Body,
    skeleton_rows: usize,
}

impl DataTable {
    /// 创建表壳；`columns` 为空时只有行区（无表头）。
    pub fn new(
        id: impl Into<ElementId>,
        columns: impl IntoIterator<Item = DataColumn>,
        _cx: &App,
    ) -> Self {
        Self {
            id: id.into(),
            columns: columns.into_iter().collect(),
            body: Body::Blank,
            skeleton_rows: 6,
        }
    }

    /// 灌入数据行。
    #[must_use]
    pub fn rows(mut self, rows: impl IntoIterator<Item = AnyElement>) -> Self {
        self.body = Body::Rows(rows.into_iter().collect());
        self
    }

    /// 切到加载态（渲染骨架，忽略行数据）。
    #[must_use]
    pub fn loading(mut self) -> Self {
        self.body = Body::Loading;
        self
    }

    /// 切到空态（只能在没有行时生效；有行仍按行渲染）。
    #[must_use]
    pub fn empty(
        mut self,
        icon: DuckIcon,
        title: impl Into<SharedString>,
        description: Option<SharedString>,
    ) -> Self {
        if matches!(self.body, Body::Rows(ref rows) if !rows.is_empty()) {
            return self;
        }
        self.body = Body::Empty {
            icon,
            title: title.into(),
            description,
        };
        self
    }

    /// 加载骨架的行数（默认 6）。
    #[must_use]
    pub fn skeleton_rows(mut self, rows: usize) -> Self {
        self.skeleton_rows = rows;
        self
    }
}

/// 表头行：列名 + 列间竖向 hairline，样式对齐 FluxDown 列表表头。
fn table_header(columns: &[DataColumn], cx: &App) -> Option<Div> {
    if columns.is_empty() {
        return None;
    }
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let xs = tokens.typography.xs;
    let extended = theme.extended();
    let hairline = extended.colors.hairline;
    // 列间竖线：宽取 hairline token（1px、不随缩放变粗），高取最大档图标高
    // ——与终端工具栏那处竖分隔线同款（`terminal/`）。
    let divider_w = extended.stroke.thin;
    let divider_h = extended.icon.lg;
    let mut header = h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.xs);
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            header = header.child(
                div()
                    .w(divider_w)
                    .h(divider_h)
                    .flex_none()
                    .rounded_full()
                    .bg(hairline),
            );
        }
        let text = div()
            .text_size(xs.size)
            .line_height(xs.line_height)
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.extended().colors.text_tertiary)
            .truncate()
            .child(column.title().clone());
        header = header.child(column_cell(column, text));
    }
    Some(header)
}

/// 行区：按状态渲染数据行 / 骨架 / 空态。
///
/// 消费式入参：`AnyElement` 不是 `Clone`，行必须移出而非克隆 —— 这也是
/// [`DataTable`] 实现 `RenderOnce`（消费 `self`）而非 `Render`（借用）的原因。
fn table_body(id: &ElementId, body: Body, skeleton_rows: usize, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    match body {
        Body::Loading => div()
            .flex_1()
            .min_h_0()
            .px(tokens.spacing.sm)
            .child(feedback::skeleton_rows(skeleton_rows, cx))
            .into_any_element(),
        Body::Empty {
            icon,
            title,
            description,
        } => feedback::empty_state(icon, title, description, None, cx)
            .flex_none()
            // 空态高度是**固定容器尺寸**（让空态粗略顶住表格正文的视觉高度），
            // 不是间距 / 圆角，故不用 spacing 阶梯、保留裸值。
            .h(px(260.))
            .into_any_element(),
        Body::Blank => div().flex_1().min_h_0().into_any_element(),
        Body::Rows(rows) => div()
            .id(ElementId::NamedInteger(
                SharedString::from(format!("{id}-body")),
                0,
            ))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(tokens.spacing.sm)
            .pb(tokens.spacing.sm)
            .child(v_flex().w_full().gap(tokens.spacing.xxs).children(rows))
            .into_any_element(),
    }
}

impl RenderOnce for DataTable {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let hairline = theme.extended().colors.hairline;
        let Self {
            id,
            columns,
            body,
            skeleton_rows,
        } = self;
        let header = table_header(&columns, cx);
        let body = table_body(&id, body, skeleton_rows, cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .children(header.map(|header| {
                header
                    .border_b(theme.extended().stroke.thin)
                    .border_color(hairline)
            }))
            .child(div().flex_1().min_h_0().pt(tokens.spacing.xs).child(body))
    }
}

/// 单元格：按列宽与对齐摆放内容（表头与数据行共用同一套几何，所以两处天然对齐）。
fn column_cell(column: &DataColumn, content: impl IntoElement) -> Div {
    let cell = div().min_w_0();
    let cell = match column.alignment() {
        TableAlign::Start => cell,
        TableAlign::Center => cell.flex().justify_center(),
        TableAlign::End => cell.flex().justify_end().font_features(tabular_numbers()),
    };
    match column.width_px() {
        Some(width) => cell.w(width).flex_none(),
        None => cell.flex_1(),
    }
    .child(content)
}
