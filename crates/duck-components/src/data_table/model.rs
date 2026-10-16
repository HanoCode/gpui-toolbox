//! 数据表的**中性数据模型**：组件只认这一份结构，不认任何具体数据源。
//!
//! 设计要点（照 dbflux `data_table/model.rs` 的口径）：
//!
//! 1. **展示文本在构造时算好**（[`CellValue::display`]，`Arc<str>`）——
//!    渲染每帧只 bump 引用计数，不做格式化、不分配字符串。原实现把
//!    `Value::render()`（内含整串 `clone` / `format!`）放在渲染路径上，
//!    可见行 × 列数 每帧重算一次，这是滚动掉帧的主源之一。
//! 2. **整表一个 `Arc`**（[`TableModel`]）：渲染入口 `Arc::clone` 一次，
//!    绝不整份深拷贝。
//! 3. 列宽估算也在这里（采样前 200 行、按展示文本字符数估宽再夹上下限），
//!    组件自己**缓存**这份宽度，宿主不必再算一份。

use std::sync::Arc;

/// 行高（定高是行虚拟化的前提）。
pub const ROW_HEIGHT: f32 = 24.;
/// 列头行高：两行（列名 + 类型）。
/// 列头高：**三行**（列名 / 类型+长度 / 注释；后两行缺省时不占档）。
///
/// 38 → 56 的那次扩档是给表头三段化的（列名 + 类型 + 注释，见
/// `docs/database-design.md` §4.20.7）：行区独立滚动，这里只影响表头一条。
pub const HEADER_HEIGHT: f32 = 56.;
/// 行首「行号」列宽度（不计入列宽缓存）。
pub const ROWNUM_WIDTH: f32 = 44.;
/// 行首「多选」列宽度（不计入列宽缓存）。
pub const SELECT_COLUMN_WIDTH: f32 = 34.;
/// 自动估宽的上下限。
pub const MIN_COLUMN_WIDTH: f32 = 72.;
pub const MAX_AUTO_WIDTH: f32 = 260.;
/// **手动拖拽**能拉到的最大宽度：估宽夹在 260 是为了「一屏塞得下」，
/// 但手动拖的人明确要放宽看长文本，就不该被 260 顶住。
pub const MAX_COLUMN_WIDTH: f32 = 1200.;
/// 列宽拖拽把手的可命中带宽（对齐 gpui-component 表格的 `HANDLE_PADDING`）。
pub const RESIZE_HANDLE_WIDTH: f32 = 4.;
/// 自动估宽采样的行数上限。
pub const AUTO_WIDTH_SAMPLE_ROWS: usize = 200;

/// 单元格的粗分类。
///
/// 与展示文本**分开存**：NULL 要压灰、数值将来要右对齐/等宽、JSON/二进制
/// 各有各的阅览入口 —— 这些都靠类型判，不靠「猜字符串像不像数字」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Null,
    Bool,
    Number,
    Text,
    Json,
    Bytes,
    Other,
}

/// 一个单元格：类型 + **预先算好的展示文本**。
#[derive(Debug, Clone)]
pub struct CellValue {
    kind: CellKind,
    display: Arc<str>,
}

impl CellValue {
    #[must_use]
    pub fn new(kind: CellKind, display: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            display: display.into(),
        }
    }

    /// 展示文本（克隆 `Arc<str>`：只 bump 引用计数，不复制字符串本体）。
    #[must_use]
    pub fn display(&self) -> Arc<str> {
        self.display.clone()
    }

    /// 展示文本的借用形式（估宽这类只读用途用，不 bump 计数）。
    #[must_use]
    pub fn display_ref(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub const fn kind(&self) -> CellKind {
        self.kind
    }

    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self.kind, CellKind::Null)
    }
}

/// 一列的定义：列名 + 引擎报的类型名（列头第二行画它）+ 数据库注释（第三行）。
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    pub name: Arc<str>,
    pub type_name: Arc<str>,
    /// 列注释（表头第三行；拿不到就是空串，那一行不画）。
    pub comment: Arc<str>,
}

impl ColumnSpec {
    #[must_use]
    pub fn new(name: impl Into<Arc<str>>, type_name: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            comment: Arc::from(""),
        }
    }

    /// 带注释的构造（`new` 后链一下即可，别处不受影响）。
    #[must_use]
    pub fn with_comment(mut self, comment: impl Into<Arc<str>>) -> Self {
        self.comment = comment.into();
        self
    }
}

/// 一行数据（按列序对齐）。
#[derive(Debug, Clone)]
pub struct RowData {
    pub cells: Vec<CellValue>,
}

impl RowData {
    #[must_use]
    pub fn new(cells: Vec<CellValue>) -> Self {
        Self { cells }
    }

    #[must_use]
    pub fn cell(&self, index: usize) -> Option<&CellValue> {
        self.cells.get(index)
    }
}

/// 整张表：列定义 + 行数据。
#[derive(Debug, Clone, Default)]
pub struct TableModel {
    columns: Vec<ColumnSpec>,
    rows: Vec<RowData>,
}

impl TableModel {
    #[must_use]
    pub fn new(columns: Vec<ColumnSpec>, rows: Vec<RowData>) -> Self {
        Self { columns, rows }
    }

    #[must_use]
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    #[must_use]
    pub fn rows(&self) -> &[RowData] {
        &self.rows
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    #[must_use]
    pub fn row(&self, index: usize) -> Option<&RowData> {
        self.rows.get(index)
    }

    #[must_use]
    pub fn cell(&self, row: usize, col: usize) -> Option<&CellValue> {
        self.rows.get(row).and_then(|row| row.cell(col))
    }

    /// 全列估宽（采样前 [`AUTO_WIDTH_SAMPLE_ROWS`] 行，按最长展示文本估宽再夹上下限）。
    #[must_use]
    pub fn estimated_widths(&self) -> Vec<f32> {
        let sample = self.rows.len().min(AUTO_WIDTH_SAMPLE_ROWS);
        self.columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let mut longest = column.name.chars().count();
                for row in self.rows.iter().take(sample) {
                    if let Some(cell) = row.cell(index) {
                        longest = longest.max(cell.display_ref().chars().count());
                    }
                }
                // 中文按 2 倍宽近似：字符数 × 7px，上下限夹住。
                (longest as f32 * 7.0 + 16.0).clamp(MIN_COLUMN_WIDTH, MAX_AUTO_WIDTH)
            })
            .collect()
    }

    /// 单列估宽（双击把手「按内容自适应」用）。
    #[must_use]
    pub fn estimated_width(&self, col: usize) -> Option<f32> {
        let sample = self.rows.len().min(AUTO_WIDTH_SAMPLE_ROWS);
        let column = self.columns.get(col)?;
        let mut longest = column.name.chars().count();
        for row in self.rows.iter().take(sample) {
            if let Some(cell) = row.cell(col) {
                longest = longest.max(cell.display_ref().chars().count());
            }
        }
        Some((longest as f32 * 7.0 + 16.0).clamp(MIN_COLUMN_WIDTH, MAX_AUTO_WIDTH))
    }
}

/// 列宽的前缀和：`offsets[col]` = 该列**左缘**相对第一条数据列的距离。
///
/// 「跳转列」与列宽把手都要按同一份口径算横向位置，收在一处免得两处漂移。
#[must_use]
pub fn column_offsets(widths: &[f32]) -> Vec<f32> {
    let mut offsets = Vec::with_capacity(widths.len());
    let mut acc = 0.;
    for width in widths {
        offsets.push(acc);
        acc += width;
    }
    offsets
}

/// 用参数依次填模板里的 `{}`（i18n 文案留在宿主侧：组件不含任何界面文案）。
///
/// 与插件 i18n 的 `duck.t(source, ...args)` 同一条口径：按出现顺序填，缺参就留着 `{}`。
#[must_use]
pub fn fill_template(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut rest = template;
    for arg in args {
        let Some(position) = rest.find("{}") else {
            break;
        };
        out.push_str(&rest[..position]);
        out.push_str(arg);
        rest = &rest[position + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(rows: Vec<Vec<&str>>) -> TableModel {
        TableModel::new(
            vec![ColumnSpec::new("c", "TEXT")],
            rows.into_iter()
                .map(|row| {
                    RowData::new(
                        row.into_iter()
                            .map(|text| CellValue::new(CellKind::Text, text))
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn display_text_is_shared_not_rebuilt() {
        let cell = CellValue::new(CellKind::Text, "hello");
        let first = cell.display();
        let second = cell.display();
        assert!(Arc::ptr_eq(&first, &second), "展示文本必须共享同一份 Arc");
    }

    #[test]
    fn estimated_width_clamps_and_follows_content() {
        let wide = model(vec![vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]]);
        assert_eq!(wide.estimated_width(0), Some(MAX_AUTO_WIDTH));
        let narrow = model(vec![vec![""]]);
        assert_eq!(narrow.estimated_width(0), Some(MIN_COLUMN_WIDTH));
    }

    #[test]
    fn estimated_width_samples_header_too() {
        let long_header = TableModel::new(
            vec![ColumnSpec::new("a-very-long-column-name-here", "TEXT")],
            vec![RowData::new(vec![CellValue::new(CellKind::Text, "x")])],
        );
        assert!(
            long_header.estimated_width(0).unwrap() > MIN_COLUMN_WIDTH,
            "列名比内容长时按列名估宽"
        );
    }

    #[test]
    fn offsets_are_prefix_sums() {
        assert_eq!(column_offsets(&[10., 20., 30.]), vec![0., 10., 30.]);
    }

    #[test]
    fn fill_template_replaces_in_order() {
        assert_eq!(fill_template("row {}", &["7"]), "row 7");
        assert_eq!(fill_template("{} / {}", &["1", "2"]), "1 / 2");
        assert_eq!(fill_template("none", &["7"]), "none");
        assert_eq!(fill_template("{} {}", &["1"]), "1 {}");
    }
}
