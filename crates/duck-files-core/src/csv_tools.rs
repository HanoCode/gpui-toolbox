//! CSV 工具：**先看表，再画图**。
//!
//! 参考仓是两个页面（`CsvTableEditor.vue` 表格 / `CsvToChart.vue` 图表），共用
//! `papaparse` 与 `chart.js`。合并成一个工具后，这里只保留两件**纯算法**：
//!
//! 1. 解析 + 三种导出（Markdown / JSON / INSERT）——逐字移植 `copyAsMarkdown` /
//!    `copyAsJson` / `copyAsInsert`，包括 INSERT 的三条取值规则（空与 `NULL` 写
//!    `NULL`、纯数字不加引号、字符串用单引号并把 `'` 翻倍）；
//! 2. 图表的 **SVG 生成**——参考仓用 `chart.js` 在 canvas 上画，插件进程没有
//!    DOM，也没有理由为一个 8 种图表的工具引一个渲染引擎；这里直接拼 SVG 文本
//!    （能存成文件、能贴进文档，比 canvas 更适合「交付物」这个场景）。
//!
//! 两处偏离都写在这里，便于日后对账：
//!
//! - **解析改用 `csv` crate**：前端是 `split(delimiter)`，字段里带引号或换行就散架；
//!   参考仓自己的后端 CSV 转换用的就是 `csv` crate，两边口径反而统一了。
//! - **分隔符探测多认 `;` 与 `|`**：前端只区分 `,` 与 `\t`，而银行导出的 CSV 里
//!   分号分隔（欧洲区域设置）并不罕见。
//!
//! 配色取自参考仓 `palettes`，四套各 8 色，顺序一致——用户把 SVG 贴进汇报材料时，
//! 颜色与旧工具一致，不会突然变得陌生。

use serde::Serialize;

use crate::error::{ErrorKind, FilesError, FilesResult};

/// 解析出来的表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CsvTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// 实际使用的分隔符。
    pub delimiter: char,
}

impl CsvTable {
    /// 行数（不含表头）。
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 列数。
    pub fn column_count(&self) -> usize {
        self.headers.len()
    }

    /// Markdown 表格（移植 `copyAsMarkdown`）。
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("| {} |\n", self.headers.join(" | ")));
        out.push_str(&format!(
            "| {} |\n",
            self.headers
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        for row in &self.rows {
            out.push_str(&format!("| {} |\n", row.join(" | ")));
        }
        out
    }

    /// JSON 数组（移植 `copyAsJson`）：一行一个对象，键是表头。
    pub fn to_json(&self) -> FilesResult<String> {
        let objects: Vec<serde_json::Map<String, serde_json::Value>> = self
            .rows
            .iter()
            .map(|row| {
                let mut object = serde_json::Map::new();
                for (index, header) in self.headers.iter().enumerate() {
                    object.insert(
                        header.clone(),
                        serde_json::Value::String(
                            row.get(index).cloned().unwrap_or_default(),
                        ),
                    );
                }
                object
            })
            .collect();
        serde_json::to_string_pretty(&objects)
            .map_err(|error| FilesError::with_detail(ErrorKind::Io, error.to_string()))
    }

    /// INSERT 语句（移植 `copyAsInsert`）。
    ///
    /// 三条取值规则照抄参考仓：空串与 `NULL` 写 `NULL`；纯数字（含负号小数点）
    /// 原样写；其余按字符串加单引号，`'` 翻倍。
    pub fn to_insert(&self, table: &str) -> String {
        let table = if table.trim().is_empty() {
            "t_data"
        } else {
            table.trim()
        };
        let columns = self.headers.join(", ");
        let values: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let cells: Vec<String> = (0..self.headers.len())
                    .map(|index| sql_literal(row.get(index).map(String::as_str).unwrap_or("")))
                    .collect();
                format!("({})", cells.join(", "))
            })
            .collect();
        format!(
            "INSERT INTO {table} ({columns}) VALUES\n{};",
            values.join(",\n")
        )
    }
}

/// 一个单元格 → SQL 字面量。
fn sql_literal(cell: &str) -> String {
    let cell = cell.trim();
    if cell.is_empty() || cell.eq_ignore_ascii_case("NULL") {
        return "NULL".to_string();
    }
    if is_plain_number(cell) {
        return cell.to_string();
    }
    format!("'{}'", cell.replace('\'', "''"))
}

/// 纯数字判定：可选负号 + 数字 + 可选小数点（与 JS 正则 `^-?\d+(\.\d+)?$` 等价）。
fn is_plain_number(cell: &str) -> bool {
    let bytes = cell.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'-') {
        index = 1;
    }
    let digits_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == digits_start {
        return false;
    }
    if index == bytes.len() {
        return true;
    }
    if bytes[index] != b'.' {
        return false;
    }
    index += 1;
    let fraction_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    index == bytes.len() && index > fraction_start
}

/// 解析 CSV / TSV。
///
/// `delimiter` 为 `None` 时按首行探测（见模块头的偏离说明）。
pub fn parse_table(text: &str, delimiter: Option<char>) -> FilesResult<CsvTable> {
    if text.trim().is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }
    let delimiter = delimiter.unwrap_or_else(|| detect_delimiter(text));
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());

    let headers: Vec<String> = reader
        .headers()
        .map_err(|error| FilesError::with_detail(ErrorKind::Unsupported, error.to_string()))?
        .iter()
        .map(|header| header.trim().to_string())
        .collect();
    if headers.is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record
            .map_err(|error| FilesError::with_detail(ErrorKind::Unsupported, error.to_string()))?;
        let mut row: Vec<String> = record.iter().map(|cell| cell.trim().to_string()).collect();
        // 缺列补齐、多列截断：`flexible` 允许不等长，但表格渲染要求等宽。
        row.resize(headers.len(), String::new());
        rows.push(row);
    }
    Ok(CsvTable {
        headers,
        rows,
        delimiter,
    })
}

/// 按首行里的候选分隔符出现次数挑一个（并列时优先逗号）。
fn detect_delimiter(text: &str) -> char {
    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    // 引号里的分隔符不该计数：先粗切掉成对的引号段（足够应付首行）。
    let mut outside = String::with_capacity(first.len());
    let mut in_quotes = false;
    for ch in first.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes {
            outside.push(ch);
        }
    }
    let count = |needle: char| outside.matches(needle).count();
    let candidates = [(',', count(',')), ('\t', count('\t')), (';', count(';')), ('|', count('|'))];
    let mut best = (',', 0usize);
    for (candidate, hits) in candidates {
        if hits > best.1 {
            best = (candidate, hits);
        }
    }
    best.0
}

/// 图表种类（对应参考仓 `chartTypeOptions`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartType {
    Bar,
    Line,
    Pie,
    Doughnut,
}

/// 配色方案（对应参考仓 `palettes`，顺序与色值一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartPalette {
    Default,
    Soft,
    Vivid,
    Mono,
}

impl ChartPalette {
    fn colors(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[
                "#3b82f6", "#ef4444", "#22c55e", "#f59e0b", "#8b5cf6", "#ec4899", "#06b6d4",
                "#84cc16",
            ],
            Self::Soft => &[
                "#93c5fd", "#fca5a5", "#86efac", "#fcd34d", "#c4b5fd", "#f9a8d4", "#67e8f9",
                "#bef264",
            ],
            Self::Vivid => &[
                "#2563eb", "#dc2626", "#16a34a", "#d97706", "#7c3aed", "#db2777", "#0891b2",
                "#65a30d",
            ],
            Self::Mono => &[
                "#3b82f6", "#60a5fa", "#93c5fd", "#bfdbfe", "#dbeafe", "#eff6ff", "#1d4ed8",
                "#2563eb",
            ],
        }
    }

    fn color(self, index: usize) -> &'static str {
        let colors = self.colors();
        colors[index % colors.len()]
    }
}

/// 图表参数。
#[derive(Debug, Clone)]
pub struct ChartOptions {
    pub kind: ChartType,
    /// 作为横轴 / 扇区名的列（`None` = 第一列）。
    pub label_column: Option<usize>,
    /// 数值列（空 = 除标签列之外的全部列）。
    pub value_columns: Vec<usize>,
    pub palette: ChartPalette,
    pub show_legend: bool,
    pub show_grid: bool,
}

impl Default for ChartOptions {
    fn default() -> Self {
        Self {
            kind: ChartType::Bar,
            label_column: None,
            value_columns: Vec::new(),
            palette: ChartPalette::Default,
            show_legend: true,
            show_grid: true,
        }
    }
}

const WIDTH: f64 = 880.0;
const HEIGHT: f64 = 460.0;
const PAD_LEFT: f64 = 64.0;
const PAD_RIGHT: f64 = 28.0;
const PAD_TOP: f64 = 52.0;
const PAD_BOTTOM: f64 = 64.0;

const INK: &str = "#334155";
const MUTED: &str = "#94a3b8";
const GRID: &str = "#e2e8f0";

/// 生成 SVG。返回的是**完整 SVG 文档文本**，可直接写 `.svg` 文件。
pub fn to_svg_chart(table: &CsvTable, options: &ChartOptions) -> FilesResult<String> {
    let label_index = options.label_column.unwrap_or(0).min(table.column_count() - 1);
    let series: Vec<usize> = if options.value_columns.is_empty() {
        (0..table.column_count())
            .filter(|index| *index != label_index)
            .collect()
    } else {
        options
            .value_columns
            .iter()
            .copied()
            .filter(|index| *index < table.column_count())
            .collect()
    };
    if series.is_empty() {
        return Err(FilesError::with_detail(
            ErrorKind::Unsupported,
            "no numeric column",
        ));
    }

    let labels: Vec<String> = table
        .rows
        .iter()
        .map(|row| row.get(label_index).cloned().unwrap_or_default())
        .collect();
    // 每列一行数据；解析不了的按 0 处理（但不参与最大值计算）。
    let values: Vec<Vec<f64>> = series
        .iter()
        .map(|index| {
            table
                .rows
                .iter()
                .map(|row| {
                    row.get(*index)
                        .and_then(|cell| cell.trim().replace(',', "").parse::<f64>().ok())
                        .unwrap_or(0.0)
                })
                .collect()
        })
        .collect();
    if labels.is_empty() {
        return Err(FilesError::new(ErrorKind::EmptyInput));
    }

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {WIDTH:.0} {HEIGHT:.0}\" width=\"{WIDTH:.0}\" height=\"{HEIGHT:.0}\" font-family=\"-apple-system, 'Segoe UI', 'PingFang SC', 'Microsoft YaHei', sans-serif\">\n"
    ));
    svg.push_str(&format!(
        "<rect x=\"0\" y=\"0\" width=\"{WIDTH:.0}\" height=\"{HEIGHT:.0}\" fill=\"#ffffff\"/>\n"
    ));

    match options.kind {
        ChartType::Bar | ChartType::Line => {
            let names = series_names(table, &series);
            let legend_items: Vec<(String, &str)> = names
                .iter()
                .enumerate()
                .map(|(index, name)| (name.clone(), options.palette.color(index)))
                .collect();
            if options.show_legend {
                draw_legend(&mut svg, &legend_items, WIDTH - PAD_RIGHT);
            }
            draw_axes(&mut svg, &labels, &values, options);
            if options.kind == ChartType::Bar {
                draw_bars(&mut svg, &values, options);
            } else {
                draw_lines(&mut svg, &values, options);
            }
        }
        ChartType::Pie | ChartType::Doughnut => {
            let first = &values[0];
            let legend_items: Vec<(String, &str)> = labels
                .iter()
                .enumerate()
                .map(|(index, label)| (label.clone(), options.palette.color(index)))
                .collect();
            if options.show_legend {
                draw_legend(&mut svg, &legend_items, WIDTH - PAD_RIGHT);
            }
            draw_pie(&mut svg, first, options);
        }
    }

    svg.push_str(&format!(
        "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{MUTED}\" text-anchor=\"end\">{} rows · {} cols</text>\n",
        WIDTH - PAD_RIGHT,
        HEIGHT - 12.0,
        table.row_count(),
        table.column_count()
    ));
    svg.push_str("</svg>\n");
    Ok(svg)
}

fn series_names(table: &CsvTable, series: &[usize]) -> Vec<String> {
    series
        .iter()
        .map(|index| {
            let name = table
                .headers
                .get(*index)
                .cloned()
                .unwrap_or_else(|| format!("col {}", index + 1));
            if name.trim().is_empty() {
                format!("col {}", index + 1)
            } else {
                name
            }
        })
        .collect()
}

fn draw_legend(svg: &mut String, items: &[(String, &str)], right: f64) {
    // 从右往左排；超过 8 条只画前 8 条（再多就挤成一片，图例也没意义了）。
    let mut x = right;
    for (name, color) in items.iter().take(8) {
        let width = 8.0 + estimate_width(name);
        x -= width + 16.0;
        svg.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"22\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{color}\"/>\n"
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"31\" font-size=\"11\" fill=\"{INK}\">{}</text>\n",
            x + 14.0,
            escape(name)
        ));
    }
}

/// 粗估一段文字的像素宽度：中文按 11 px/字、其余按 6.2 px/字（只用于避让）。
fn estimate_width(text: &str) -> f64 {
    text.chars()
        .map(|ch| if ch.is_ascii() { 6.2 } else { 11.0 })
        .sum()
}

fn plot_area() -> (f64, f64, f64, f64) {
    (
        PAD_LEFT,
        PAD_TOP,
        WIDTH - PAD_LEFT - PAD_RIGHT,
        HEIGHT - PAD_TOP - PAD_BOTTOM,
    )
}

/// 数值范围 → 好看的刻度（1 / 2 / 5 × 10^n 步长，4 段）。
fn nice_range(values: &[Vec<f64>]) -> (f64, f64, f64) {
    let mut min = 0.0f64;
    let mut max = 0.0f64;
    for series in values {
        for value in series {
            min = min.min(*value);
            max = max.max(*value);
        }
    }
    if (max - min).abs() < f64::EPSILON {
        max = min + 1.0;
    }
    let raw_step = (max - min) / 4.0;
    let magnitude = 10f64.powf(raw_step.log10().floor());
    let normalized = raw_step / magnitude;
    let step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;
    let low = (min / step).floor() * step;
    let high = (max / step).ceil() * step;
    (low, high, step)
}

fn draw_axes(svg: &mut String, labels: &[String], values: &[Vec<f64>], options: &ChartOptions) {
    let (x0, y0, width, height) = plot_area();
    let (low, high, step) = nice_range(values);
    let scale = |value: f64| y0 + height - (value - low) / (high - low) * height;

    let mut tick = low;
    while tick <= high + step * 0.001 {
        let y = scale(tick);
        if options.show_grid {
            svg.push_str(&format!(
                "<line x1=\"{x0:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" stroke=\"{GRID}\" stroke-width=\"1\"/>\n",
                x0 + width
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"{MUTED}\" text-anchor=\"end\">{}</text>\n",
            x0 - 8.0,
            y + 4.0,
            format_tick(tick)
        ));
        tick += step;
    }
    // 零线：有负值时它需要比网格更实一点。
    if low < 0.0 && high > 0.0 {
        let y = scale(0.0);
        svg.push_str(&format!(
            "<line x1=\"{x0:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" stroke=\"{MUTED}\" stroke-width=\"1\"/>\n",
            x0 + width
        ));
    }
    // 轴线
    svg.push_str(&format!(
        "<line x1=\"{x0:.1}\" y1=\"{y0:.1}\" x2=\"{x0:.1}\" y2=\"{:.1}\" stroke=\"{MUTED}\" stroke-width=\"1\"/>\n",
        y0 + height
    ));

    // 横轴标签：类别多时隔一个画一个，避免叠字。
    let count = labels.len().max(1);
    let slot = width / count as f64;
    let stride = ((36.0 / slot).ceil() as usize).max(1);
    for (index, label) in labels.iter().enumerate() {
        if index % stride != 0 {
            continue;
        }
        let centre = x0 + slot * (index as f64 + 0.5);
        svg.push_str(&format!(
            "<text x=\"{centre:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"{INK}\" text-anchor=\"middle\">{}</text>\n",
            y0 + height + 20.0,
            escape(&truncate(label, 12))
        ));
    }
}

fn draw_bars(svg: &mut String, values: &[Vec<f64>], options: &ChartOptions) {
    let (x0, y0, width, height) = plot_area();
    let (low, high, _) = nice_range(values);
    let scale = |value: f64| y0 + height - (value - low) / (high - low) * height;
    let zero = scale(0.0f64.max(low));
    let count = values.first().map(Vec::len).unwrap_or(0).max(1);
    let slot = width / count as f64;
    let series_count = values.len() as f64;
    let bar_width = (slot * 0.72 / series_count).max(1.0);

    for (series_index, series) in values.iter().enumerate() {
        for (index, value) in series.iter().enumerate() {
            let left = x0 + slot * index as f64 + slot * 0.14 + bar_width * series_index as f64;
            let top = scale(*value).min(zero);
            let bar_height = (scale(*value) - zero).abs().max(0.6);
            svg.push_str(&format!(
                "<rect x=\"{left:.1}\" y=\"{top:.1}\" width=\"{bar_width:.1}\" height=\"{bar_height:.1}\" fill=\"{}\" rx=\"2\"/>\n",
                options.palette.color(series_index)
            ));
        }
    }
}

fn draw_lines(svg: &mut String, values: &[Vec<f64>], options: &ChartOptions) {
    let (x0, y0, width, height) = plot_area();
    let (low, high, _) = nice_range(values);
    let scale = |value: f64| y0 + height - (value - low) / (high - low) * height;
    let count = values.first().map(Vec::len).unwrap_or(0).max(1);
    let slot = width / count as f64;

    for (series_index, series) in values.iter().enumerate() {
        let color = options.palette.color(series_index);
        let points: Vec<String> = series
            .iter()
            .enumerate()
            .map(|(index, value)| {
                format!(
                    "{:.1},{:.1}",
                    x0 + slot * (index as f64 + 0.5),
                    scale(*value)
                )
            })
            .collect();
        svg.push_str(&format!(
            "<polyline fill=\"none\" stroke=\"{color}\" stroke-width=\"2\" stroke-linejoin=\"round\" points=\"{}\"/>\n",
            points.join(" ")
        ));
        // 点只在不太密的时候画：50 个点以上就成一条毛毛虫。
        if series.len() <= 30 {
            for point in &points {
                let mut parts = point.split(',');
                let x = parts.next().unwrap_or("0");
                let y = parts.next().unwrap_or("0");
                svg.push_str(&format!(
                    "<circle cx=\"{x}\" cy=\"{y}\" r=\"3\" fill=\"{color}\"/>\n"
                ));
            }
        }
    }
}

fn draw_pie(svg: &mut String, values: &[f64], options: &ChartOptions) {
    let (x0, y0, width, height) = plot_area();
    let total: f64 = values.iter().filter(|value| **value > 0.0).sum();
    if total <= 0.0 {
        return;
    }
    let cx = x0 + width / 2.0;
    let cy = y0 + height / 2.0;
    let radius = (width.min(height) / 2.0 - 12.0).max(24.0);
    let inner = if options.kind == ChartType::Doughnut {
        radius * 0.55
    } else {
        0.0
    };

    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (index, value) in values.iter().enumerate() {
        if *value <= 0.0 {
            continue;
        }
        let sweep = value / total * std::f64::consts::TAU;
        let end = angle + sweep;
        // 整圆（只有一个扇区）用两段半圆拼，单条弧线的起终点重合会画不出来。
        let path = if sweep >= std::f64::consts::TAU - 1e-6 {
            format!(
                "M {:.2} {:.2} a {radius:.2} {radius:.2} 0 1 0 {:.2} 0 a {radius:.2} {radius:.2} 0 1 0 {:.2} 0 Z",
                cx,
                cy - radius,
                radius * 2.0,
                -radius * 2.0
            )
        } else {
            let (sx, sy) = (cx + radius * angle.cos(), cy + radius * angle.sin());
            let (ex, ey) = (cx + radius * end.cos(), cy + radius * end.sin());
            let large = u8::from(sweep > std::f64::consts::PI);
            if inner > 0.0 {
                let (isx, isy) = (cx + inner * angle.cos(), cy + inner * angle.sin());
                let (iex, iey) = (cx + inner * end.cos(), cy + inner * end.sin());
                format!(
                    "M {sx:.2} {sy:.2} A {radius:.2} {radius:.2} 0 {large} 1 {ex:.2} {ey:.2} L {iex:.2} {iey:.2} A {inner:.2} {inner:.2} 0 {large} 0 {isx:.2} {isy:.2} Z"
                )
            } else {
                format!(
                    "M {cx:.2} {cy:.2} L {sx:.2} {sy:.2} A {radius:.2} {radius:.2} 0 {large} 1 {ex:.2} {ey:.2} Z"
                )
            }
        };
        svg.push_str(&format!(
            "<path d=\"{path}\" fill=\"{}\" stroke=\"#ffffff\" stroke-width=\"1.5\"/>\n",
            options.palette.color(index)
        ));
        // 扇区占比 ≥ 6% 才写百分比：再小就挤成噪点。
        if sweep / std::f64::consts::TAU >= 0.06 {
            let mid = angle + sweep / 2.0;
            let at = if inner > 0.0 {
                (radius + inner) / 2.0
            } else {
                radius * 0.62
            };
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#ffffff\" text-anchor=\"middle\">{:.0}%</text>\n",
                cx + at * mid.cos(),
                cy + at * mid.sin() + 4.0,
                value / total * 100.0
            ));
        }
        angle = end;
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut out: String = chars[..limit.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

fn format_tick(value: f64) -> String {
    if value.fract().abs() < 1e-9 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 把同一份数据渲成「表 + 说明」的纯文本摘要（结果区用）。
pub fn render_summary(table: &CsvTable, max_rows: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} rows × {} cols · delimiter={}\n",
        table.row_count(),
        table.column_count(),
        match table.delimiter {
            '\t' => "tab".to_string(),
            other => other.to_string(),
        }
    ));
    out.push('\n');
    out.push_str(&table.to_markdown());
    if table.row_count() > max_rows {
        out.push_str(&format!(
            "… ({} more rows)\n",
            table.row_count() - max_rows
        ));
    }
    out
}

/// 预览用：只保留前 `rows` 行（导出走完整表，预览不）。
pub fn head(table: &CsvTable, rows: usize) -> CsvTable {
    let mut copy = table.clone();
    copy.rows.truncate(rows);
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> CsvTable {
        parse_table("name,qty,price\napple,3,1.5\nbanana,5,0.5\n", Some(',')).expect("parse")
    }

    #[test]
    fn parses_quoted_fields_with_commas_and_newlines() {
        let text = "a,b\n\"x,1\",\"line1\nline2\"\n";
        let table = parse_table(text, Some(',')).expect("parse");
        assert_eq!(table.rows[0][0], "x,1");
        assert_eq!(table.rows[0][1], "line1\nline2");
    }

    #[test]
    fn detects_delimiter_from_the_header_line() {
        assert_eq!(parse_table("a\tb\n1\t2\n", None).expect("t").delimiter, '\t');
        assert_eq!(parse_table("a;b\n1;2\n", None).expect("s").delimiter, ';');
        assert_eq!(parse_table("a|b\n1|2\n", None).expect("p").delimiter, '|');
        // 引号里的逗号不算。
        assert_eq!(parse_table("\"a,b\"\tc\n1\t2\n", None).expect("q").delimiter, '\t');
    }

    #[test]
    fn pads_short_rows_to_header_width() {
        let table = parse_table("a,b,c\n1,2\n", Some(',')).expect("parse");
        assert_eq!(table.rows[0], vec!["1", "2", ""]);
    }

    #[test]
    fn markdown_json_and_insert_match_the_reference_shapes() {
        let table = table();
        let markdown = table.to_markdown();
        assert!(markdown.starts_with("| name | qty | price |\n| --- | --- | --- |\n"), "{markdown}");
        assert!(markdown.contains("| apple | 3 | 1.5 |"), "{markdown}");

        let json = table.to_json().expect("json");
        assert!(json.contains("\"name\": \"apple\""), "{json}");

        let insert = table.to_insert("t_fruit");
        assert!(insert.starts_with("INSERT INTO t_fruit (name, qty, price) VALUES\n"), "{insert}");
        // 数字不加引号、字符串加引号。
        assert!(insert.contains("('apple', 3, 1.5)"), "{insert}");
        assert!(insert.ends_with(';'), "{insert}");
    }

    #[test]
    fn insert_quotes_null_like_and_empty_cells() {
        let table = parse_table("a,b\n,q\nNULL,x\nit's,1\n", Some(',')).expect("parse");
        let insert = table.to_insert("");
        assert!(insert.contains("(NULL, 'q')"), "{insert}");
        assert!(insert.contains("(NULL, 'x')"), "{insert}");
        assert!(insert.contains("('it''s', 1)"), "{insert}");
    }

    #[test]
    fn plain_number_detection_is_strict() {
        assert!(is_plain_number("42"));
        assert!(is_plain_number("-1.25"));
        assert!(!is_plain_number("1."));
        assert!(!is_plain_number(".5"));
        assert!(!is_plain_number("1e3"));
        assert!(!is_plain_number("12a"));
    }

    #[test]
    fn bar_chart_has_axes_bars_and_escaped_labels() {
        let table = parse_table("name,<qty>\na,3\nb,5\n", Some(',')).expect("parse");
        let svg = to_svg_chart(&table, &ChartOptions::default()).expect("svg");
        assert!(svg.starts_with("<svg xmlns"), "{svg}");
        assert!(svg.contains("<rect"), "柱子");
        assert!(svg.contains("&lt;qty&gt;"), "图例里的标签要转义");
        assert!(svg.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn line_and_pie_charts_render() {
        let table = table();
        let line = to_svg_chart(
            &table,
            &ChartOptions {
                kind: ChartType::Line,
                value_columns: vec![1],
                ..ChartOptions::default()
            },
        )
        .expect("line");
        assert!(line.contains("<polyline"), "{line}");

        let pie = to_svg_chart(
            &table,
            &ChartOptions {
                kind: ChartType::Pie,
                value_columns: vec![1],
                ..ChartOptions::default()
            },
        )
        .expect("pie");
        assert!(pie.contains("<path"), "{pie}");
        assert!(pie.contains("%"), "占比标签");

        let doughnut = to_svg_chart(
            &table,
            &ChartOptions {
                kind: ChartType::Doughnut,
                value_columns: vec![1],
                ..ChartOptions::default()
            },
        )
        .expect("doughnut");
        // 甜甜圈是两个半径拼出来的路径（带内圈弧线）。
        assert!(doughnut.matches('A').count() >= 2, "{doughnut}");
    }

    #[test]
    fn empty_input_is_a_typed_error() {
        assert_eq!(
            parse_table("   \n", None).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
    }

    #[test]
    fn summary_counts_and_truncation_note() {
        let table = table();
        let summary = render_summary(&table, 1);
        assert!(summary.contains("2 rows × 3 cols"), "{summary}");
        assert!(summary.contains("1 more rows"), "{summary}");
    }
}
