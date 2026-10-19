//! 银行接口文档（Excel）解析（逐字复刻参考仓 `channel_gateway/excel_parser.rs`）。
//!
//! 干的事：把「一家银行的一份接口清单 Excel」读成 `接口 → 请求字段 / 应答字段`。
//! 难点全在**表头不规整**：列的顺序、列名、分区标记、合并单元格的标题行，各行之间
//! 没有统一格式，所以参考仓用了一套启发式（关键词表 + 双组列识别 + 数组起止标记），
//! 这里逐条照抄——启发式没有「更好的写法」，只有「和真实文件对过」的写法。
//!
//! ## 与参考仓的差异（只有两处，都是适配）
//!
//! 1. `calamine::DataType` → [`calamine::Data`]：参考仓写这份代码时枚举叫
//!    `DataType`，0.26 起改名成 `Data`（`DataType` 变成 trait）。纯机械改名，
//!    匹配分支与顺序一字未动。
//! 2. 错误从 `String` 换成 [`GatewayError`]：内核不出中文，整句由插件层拼。
//!    「工作表名不是交易码」与「没有任何接口」这两个原本拼在一起的**提示语**
//!    （「请下载标准模板…」）搬到了插件层，判定条件不变。

// 参数字面是「行号」而不是「行」：`for row_idx in .. ` 里的下标同时用来
// ① 取 `rows[row_idx]`、② 写进「第 N 行」的报错与调试信息、③ 参与
// `header_row + 1..` 这种偏移计算。换成 `enumerate()` 会把 ②③ 拆成三份互相
// 推导的变量，而这份代码是逐字复刻件——改写法就等于换一份没和真实文件对过的
// 实现。同类 allow 见 `duck-classfile/src/classfile.rs`。
#![allow(clippy::needless_range_loop)]

use calamine::{Data, Reader, open_workbook_auto};

use crate::channel_gateway::keywords::*;
use crate::channel_gateway::{
    ColMapping, ExcelParseResult, FieldDef, GatewayError, InterfaceDef, SheetHeaders, SubFieldDef,
};

fn normalize_data_type(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    if lower.starts_with('c') && lower.len() > 1 && lower[1..].chars().all(|c| c.is_ascii_digit()) {
        return "string".to_string();
    }
    if lower.starts_with("string") || lower.starts_with("varchar") || lower.starts_with("char") {
        return "string".to_string();
    }
    if NUMERIC_TYPES.iter().any(|t| lower.contains(t)) {
        if AMOUNT_WORDS.iter().any(|word| lower.contains(word)) {
            return "amount".to_string();
        }
        return "number".to_string();
    }
    "string".to_string()
}

#[derive(Debug, Clone)]
struct ColumnGroup {
    field_name: Option<usize>,
    data_type: Option<usize>,
    required: Option<usize>,
    meaning: Option<usize>,
}

fn find_header_row(rows: &[Vec<Data>]) -> Option<(usize, ColumnGroup, Option<ColumnGroup>)> {
    for (idx, row) in rows.iter().enumerate() {
        let mut fn_cols: Vec<usize> = Vec::new();
        let mut esb_cols: Vec<usize> = Vec::new();
        let mut type_cols: Vec<usize> = Vec::new();
        let mut req_cols: Vec<usize> = Vec::new();
        let mut meaning_cols: Vec<usize> = Vec::new();

        for (ci, cell) in row.iter().enumerate() {
            let val = match cell {
                Data::String(s) => s.trim().to_lowercase(),
                _ => continue,
            };
            if val.is_empty() {
                continue;
            }

            if EXPECTED_FIELD_NAME_HEADS.iter().any(|h| val.contains(h)) {
                fn_cols.push(ci);
            }
            if EXPECTED_ESB_HEADS.iter().any(|h| val.contains(h)) {
                esb_cols.push(ci);
            }
            if EXPECTED_TYPE_HEADS.iter().any(|h| val.contains(h)) {
                type_cols.push(ci);
            }
            if EXPECTED_REQUIRED_HEADS.iter().any(|h| val.contains(h)) {
                req_cols.push(ci);
            }
            if EXPECTED_MEANING_HEADS.iter().any(|h| val.contains(h)) {
                meaning_cols.push(ci);
            }
        }

        if fn_cols.is_empty() {
            continue;
        }

        if fn_cols.len() >= 2 {
            let first = fn_cols[0];
            let second = fn_cols[1];
            let group0 = build_group(first, &type_cols, &req_cols, &meaning_cols, second);
            let group1 = build_group(second, &type_cols, &req_cols, &meaning_cols, usize::MAX);
            return Some((idx, group0, Some(group1)));
        }

        let group = ColumnGroup {
            field_name: fn_cols.first().copied(),
            data_type: type_cols.first().copied(),
            required: req_cols.first().copied(),
            meaning: meaning_cols.first().copied(),
        };

        return Some((
            idx,
            group,
            esb_cols.first().copied().map(|col| ColumnGroup {
                field_name: Some(col),
                data_type: None,
                required: None,
                meaning: None,
            }),
        ));
    }
    None
}

fn build_group(
    fn_col: usize,
    type_cols: &[usize],
    req_cols: &[usize],
    meaning_cols: &[usize],
    end_bound: usize,
) -> ColumnGroup {
    let dt = type_cols
        .iter()
        .copied()
        .find(|&c| c > fn_col && c < end_bound);
    let rq = req_cols
        .iter()
        .copied()
        .find(|&c| c > fn_col && c < end_bound);
    let mn = meaning_cols
        .iter()
        .copied()
        .find(|&c| c > fn_col && c < end_bound);
    ColumnGroup {
        field_name: Some(fn_col),
        data_type: dt,
        required: rq,
        meaning: mn,
    }
}

fn is_function_id(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 4 || s.len() > 10 {
        return false;
    }
    let bytes = s.as_bytes();
    // Find where digits start
    let digit_start = bytes
        .iter()
        .position(|b| b.is_ascii_digit())
        .unwrap_or(s.len());
    if digit_start == 0 || digit_start == s.len() {
        return false;
    }
    // Prefix must be all alphabetic
    if !bytes[..digit_start].iter().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    // At least 4 digits
    let digit_count = s.len() - digit_start;
    if digit_count < 4 {
        return false;
    }
    // Remaining characters must all be digits
    bytes[digit_start..].iter().all(|b| b.is_ascii_digit())
}

/// `serial_no` / `serial-no` / `serial no` → `SerialNo`（参考仓的驼峰化）。
fn to_java_const_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    let mut result = String::with_capacity(name.len());
    let mut capitalize_next = true;
    for c in name.chars() {
        if c == '_' || c == '-' || c == ' ' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn extract_function_id(rows: &[Vec<Data>]) -> Option<String> {
    // Scan early rows for 交易码/交易代码
    let scan_limit = rows.len().min(15);
    for row_idx in 0..scan_limit {
        let cells = &rows[row_idx];
        let first = match cells.first() {
            Some(Data::String(s)) => s.trim().to_lowercase(),
            _ => continue,
        };
        if FUNCTION_ID_KEYWORDS.iter().any(|word| first.contains(word)) {
            // Value is in the next cell (column B)
            if cells.len() > 1 {
                let val = parse_field_value(&cells[1]);
                if !val.is_empty() && is_function_id(&val) {
                    return Some(val);
                }
            }
            // Or inline like "交易码:100003"
            for part in [':', '：'] {
                if let Some(idx) = first.find(part) {
                    let val = first[idx + part.len_utf8()..].trim();
                    if is_function_id(val) {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

fn extract_description(rows: &[Vec<Data>], header_row: usize) -> String {
    let scan_limit = (header_row + 8).min(rows.len());
    let mut desc_parts: Vec<String> = Vec::new();
    for row_idx in 0..scan_limit {
        let cells = &rows[row_idx];
        for (ci, cell) in cells.iter().enumerate() {
            let val = match cell {
                Data::String(s) => s.trim().to_lowercase(),
                _ => continue,
            };
            if EXPECTED_DESC_HEADS.iter().any(|h| val.contains(h)) {
                if val.contains(":") || val.contains("：") {
                    let parts: Vec<&str> = if val.contains(':') {
                        val.splitn(2, ':').collect()
                    } else {
                        val.splitn(2, '：').collect()
                    };
                    if parts.len() > 1 {
                        let trimmed = parts[1].trim().to_string();
                        if !trimmed.is_empty() {
                            desc_parts.push(trimmed);
                            break;
                        }
                    }
                }
                // Try next cell first
                if ci + 1 < cells.len() {
                    let next = parse_field_value(&cells[ci + 1]);
                    if !next.is_empty() {
                        desc_parts.push(next);
                        break;
                    }
                }
                // If next cell is empty, scan remaining cells from right to left
                // for the first non-empty value (often in the last/meaning column)
                for other_ci in ((ci + 1)..cells.len()).rev() {
                    let v = parse_field_value(&cells[other_ci]);
                    if !v.is_empty() {
                        desc_parts.push(v);
                        break;
                    }
                }
                break;
            }
        }
    }
    desc_parts.join(" / ")
}

fn parse_field_value(cell: &Data) -> String {
    match cell {
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => f.to_string(),
        Data::Int(i) => i.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn is_meta_field(field_name: &str) -> bool {
    let lower = field_name.trim().to_lowercase();
    META_FIELD_KEYWORDS.iter().any(|word| lower.contains(word))
}

// 参考仓定义了这两个判定，但 `post_process_arrays` 并不调用它们（它用的是更窄的
// 「`list` + 数组语义」检查）。保留原文以便与源仓对照，不在移植期做「看着多余就删」
// 的判断——删了下次有人对照源仓会以为漏了一段。
#[allow(dead_code)]
fn is_array_start_marker(raw_name: &str, meaning: &str, data_type: &str, remark: &str) -> bool {
    let name_lower = raw_name.trim().to_lowercase();
    if !ARRAY_MARKER_NAMES.iter().any(|m| name_lower == *m) {
        return false;
    }
    let type_lower = data_type.trim().to_lowercase();
    if type_lower == "array" || type_lower == "list" {
        return true;
    }
    if ARRAY_MEANING_WORDS
        .iter()
        .any(|word| meaning.contains(word))
    {
        return true;
    }
    let remark_lower = remark.trim().to_lowercase();
    if ARRAY_START_KEYWORDS
        .iter()
        .any(|k| remark_lower.contains(k))
    {
        return true;
    }
    false
}

#[allow(dead_code)]
fn is_array_end_marker(raw_name: &str, _meaning: &str, remark: &str) -> bool {
    let name_lower = raw_name.trim().to_lowercase();
    if !ARRAY_MARKER_NAMES.iter().any(|m| name_lower == *m) {
        return false;
    }
    let remark_lower = remark.trim().to_lowercase();
    ARRAY_END_KEYWORDS.iter().any(|k| remark_lower.contains(k))
}

fn post_process_arrays(fields: &mut Vec<FieldDef>) {
    let mut i = 0;
    while i < fields.len() {
        let is_start = {
            let f = &fields[i];
            f.field_name.to_lowercase() == "list"
                && (f.data_type.to_lowercase() == "array"
                    || ARRAY_MEANING_WORDS
                        .iter()
                        .any(|word| f.meaning.contains(word)))
        };
        if is_start {
            let array_name = {
                let f = &fields[i];
                if f.esb_field_name.is_empty() || f.esb_field_name == f.field_name {
                    "DataList".to_string()
                } else {
                    f.esb_field_name.clone()
                }
            };
            let array_meaning = fields[i].meaning.clone();

            let mut array_items: Vec<SubFieldDef> = Vec::new();
            let j = i + 1;
            while j < fields.len() {
                let is_end = {
                    let sub = &fields[j];
                    sub.field_name.to_lowercase() == "list" && sub.meaning.trim().is_empty()
                };
                if is_end {
                    fields.remove(j);
                    break;
                }
                let sub = &fields[j];
                array_items.push(SubFieldDef {
                    field_name: sub.field_name.clone(),
                    esb_field_name: sub.esb_field_name.clone(),
                    data_type: sub.data_type.clone(),
                    required: sub.required,
                    meaning: sub.meaning.clone(),
                });
                fields.remove(j);
            }

            // Replace the start marker with the array field
            fields[i] = FieldDef {
                field_name: array_name,
                esb_field_name: String::new(),
                data_type: "array".to_string(),
                required: false,
                meaning: array_meaning,
                is_array: true,
                array_items,
            };
        }
        i += 1;
    }
}

fn is_required(cell: &Data) -> bool {
    let val = match cell {
        Data::String(s) => s.trim().to_lowercase(),
        _ => return false,
    };
    match val.as_str() {
        "y" | "yes" | "true" | "1" | "not null" => true,
        "n" | "no" | "false" | "0" => false,
        // 中文写法单独一张表：它是**匹配数据**，登记在 `keywords.rs`。
        other => REQUIRED_TRUE_WORDS.contains(&other),
    }
}

fn is_data_row(cells: &[Data]) -> bool {
    if cells.is_empty() {
        return false;
    }
    let first = match &cells[0] {
        Data::String(s) => s.trim(),
        _ => return false,
    };
    if first.is_empty() {
        return false;
    }
    !first.starts_with('#') && !first.starts_with("--")
}

/// Check if a row looks like a section label rather than a data row.
/// A label row typically has very few non-empty cells (often merged)
/// and the first non-empty value matches known label keywords.
fn is_label_row(cells: &[Data], field_name_col: Option<usize>) -> bool {
    // Count non-empty cells
    let non_empty = cells
        .iter()
        .filter(|c| matches!(c, Data::String(s) if !s.trim().is_empty()))
        .count();

    // If 0 or 1 non-empty cell and row length > 2, likely a merged label row
    if non_empty <= 1 && cells.len() > 2 {
        return true;
    }

    // Check if any non-empty cell value matches a known label keyword
    for cell in cells.iter() {
        if let Data::String(s) = cell {
            let trimmed = s.trim();
            if SECTION_LABEL_KEYWORDS.iter().any(|k| trimmed.contains(k)) {
                return true;
            }
        }
    }

    // If field_name column is known and its value looks like a known label, skip
    if let Some(fn_col) = field_name_col {
        if let Some(Data::String(s)) = cells.get(fn_col) {
            let val = s.trim();
            if !val.is_empty() {
                let lower = val.to_lowercase();
                // Check if it matches section labels
                if SECTION_LABEL_KEYWORDS.iter().any(|k| lower.contains(k)) {
                    return true;
                }
                // Check if it's a section marker itself (输入区域 etc.)
                if EXPECTED_REQUEST_HEADS.iter().any(|k| lower.contains(k))
                    || EXPECTED_RESPONSE_HEADS.iter().any(|k| lower.contains(k))
                {
                    return true;
                }
            }
        }
    }

    false
}

fn should_skip_sheet(name: &str) -> bool {
    let lower = name.to_lowercase();
    SKIP_SHEET_KEYWORDS.iter().any(|k| lower.contains(k))
}

fn is_common_header_sheet(name: &str) -> bool {
    let lower = name.to_lowercase();
    COMMON_HEADER_KEYWORDS.iter().any(|k| lower.contains(k))
}

struct SheetParseResult {
    request_fields: Vec<FieldDef>,
    response_fields: Vec<FieldDef>,
    description: String,
}

fn parse_sheet_fields(rows: &[Vec<Data>], esb_side: &str) -> SheetParseResult {
    if rows.is_empty() {
        return SheetParseResult {
            request_fields: Vec::new(),
            response_fields: Vec::new(),
            description: String::new(),
        };
    }

    let (header_row, group0, group1): (usize, ColumnGroup, Option<ColumnGroup>) =
        match find_header_row(rows) {
            Some((r, g0, g1)) => (r, g0, g1),
            None => {
                let first_col = (0..rows[0].len())
                    .find(|&ci| matches!(&rows[0][ci], Data::String(s) if !s.trim().is_empty()));
                (
                    0,
                    ColumnGroup {
                        field_name: first_col,
                        data_type: None,
                        required: None,
                        meaning: None,
                    },
                    None,
                )
            }
        };

    let mut description = extract_description(rows, header_row);

    let is_dual = group1.is_some();
    let internal_group: &ColumnGroup;
    let esb_group: Option<&ColumnGroup>;

    if is_dual {
        let g1 = group1.as_ref().expect("group1 is_some checked above");
        if esb_side == "left" {
            internal_group = g1;
            esb_group = Some(&group0);
        } else {
            internal_group = &group0;
            esb_group = Some(g1);
        }
    } else {
        internal_group = &group0;
        esb_group = None;
    }

    let mut request_fields: Vec<FieldDef> = Vec::new();
    let mut response_fields: Vec<FieldDef> = Vec::new();
    let mut current_section: Option<&str> = None;

    for row_idx in (header_row + 1)..rows.len() {
        let cells = &rows[row_idx];
        if cells.is_empty() || !is_data_row(cells) {
            current_section = None;
            continue;
        }

        let first_val = parse_field_value(&cells[0]).to_lowercase();
        if SECTION_REQUEST_WORDS
            .iter()
            .any(|word| first_val.contains(word))
            || EXPECTED_REQUEST_HEADS.iter().any(|h| first_val.contains(h))
        {
            current_section = Some("request");
            continue;
        }
        if SECTION_RESPONSE_WORDS
            .iter()
            .any(|word| first_val.contains(word))
            || EXPECTED_RESPONSE_HEADS
                .iter()
                .any(|h| first_val.contains(h))
        {
            current_section = Some("response");
            continue;
        }

        if current_section.is_none() {
            current_section = Some("request");
        }

        let raw_name = internal_group
            .field_name
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        if raw_name.is_empty() {
            continue;
        }

        // Skip metadata rows (交易名称, 交易描述, etc.) — capture as interface description
        if is_meta_field(&raw_name) {
            let meta_val = internal_group
                .meaning
                .and_then(|ci| cells.get(ci))
                .map(parse_field_value)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    internal_group
                        .field_name
                        .and_then(|ci| cells.get(ci + 1))
                        .map(parse_field_value)
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_default();
            if !meta_val.is_empty() {
                let new_desc = format!("{} / {}", meta_val, raw_name);
                if description.is_empty() {
                    description = new_desc;
                } else if !description.contains(&meta_val) {
                    description.push_str(" | ");
                    description.push_str(&new_desc);
                }
            }
            continue;
        }

        let java_name = to_java_const_name(&raw_name);

        let esb_field_value = esb_group
            .and_then(|g| g.field_name)
            .or(group0.field_name)
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| java_name.clone());

        let raw_type = internal_group
            .data_type
            .or_else(|| esb_group.and_then(|g| g.data_type))
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "string".to_string());

        let required = internal_group
            .required
            .or_else(|| esb_group.and_then(|g| g.required))
            .and_then(|ci| cells.get(ci))
            .map(is_required)
            .unwrap_or(false);

        let meaning = internal_group
            .meaning
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        let field = FieldDef {
            field_name: java_name,
            esb_field_name: esb_field_value,
            data_type: normalize_data_type(&raw_type),
            required,
            meaning,
            is_array: false,
            array_items: Vec::new(),
        };

        match current_section {
            Some("request") => request_fields.push(field),
            _ => response_fields.push(field),
        }
    }

    post_process_arrays(&mut request_fields);
    post_process_arrays(&mut response_fields);

    SheetParseResult {
        request_fields,
        response_fields,
        description,
    }
}

fn parse_sheet_fields_mapped(
    rows: &[Vec<Data>],
    mapping: &ColMapping,
    section_detection: bool,
) -> SheetParseResult {
    // SCAN to find the real column-header row.
    // A header row is one where at least 2 cells match different header keyword groups.
    let header_row = {
        let scan_end = rows.len().min(10);
        let mut found = 0usize;
        'outer: for ri in 0..scan_end {
            let cells = &rows[ri];
            if cells.is_empty() {
                continue;
            }
            let mut matched_groups = 0u8;
            for cell in cells.iter() {
                let val = match cell {
                    Data::String(s) => s.trim().to_lowercase(),
                    _ => continue,
                };
                if val.is_empty() {
                    continue;
                }
                if EXPECTED_FIELD_NAME_HEADS.iter().any(|h| val.contains(h))
                    || EXPECTED_ESB_HEADS.iter().any(|h| val.contains(h))
                {
                    matched_groups |= 1;
                }
                if EXPECTED_TYPE_HEADS.iter().any(|h| val.contains(h)) {
                    matched_groups |= 2;
                }
                if EXPECTED_REQUIRED_HEADS.iter().any(|h| val.contains(h)) {
                    matched_groups |= 4;
                }
                if EXPECTED_MEANING_HEADS.iter().any(|h| val.contains(h)) {
                    matched_groups |= 8;
                }
                // If at least 2 different groups matched, this is the header row
                if matched_groups.count_ones() >= 2 {
                    found = ri;
                    break 'outer;
                }
            }
        }
        // If still not found, use the first non-empty row as fallback
        if found == 0 && !rows.is_empty() {
            for ri in 0..scan_end.min(5) {
                if rows[ri]
                    .iter()
                    .any(|c| matches!(c, Data::String(s) if !s.trim().is_empty()))
                {
                    found = ri;
                    break;
                }
            }
        }
        found
    };

    let mut description = extract_description(rows, header_row);

    let mut request_fields: Vec<FieldDef> = Vec::new();
    let mut response_fields: Vec<FieldDef> = Vec::new();
    let mut current_section: Option<&str> = None;

    for row_idx in (header_row + 1)..rows.len() {
        let cells = &rows[row_idx];
        if cells.is_empty() {
            current_section = None;
            continue;
        }

        // ── Section detection first: check ALL cells for section markers ──
        // (runs before is_data_row so "输入区域" in any column is caught)
        if section_detection {
            let first_val =
                parse_field_value(cells.first().unwrap_or(&Data::String(String::new())))
                    .to_lowercase();

            // Check column 0
            let is_req = SECTION_REQUEST_WORDS
                .iter()
                .any(|word| first_val.contains(word))
                || EXPECTED_REQUEST_HEADS.iter().any(|h| first_val.contains(h));
            let is_resp = SECTION_RESPONSE_WORDS
                .iter()
                .any(|word| first_val.contains(word))
                || EXPECTED_RESPONSE_HEADS
                    .iter()
                    .any(|h| first_val.contains(h));

            // Check the mapped field_name column for section keywords
            let fn_val = cells
                .get(mapping.field_name_col)
                .map(parse_field_value)
                .unwrap_or_default()
                .to_lowercase();
            let fn_is_req = !fn_val.is_empty()
                && (SECTION_REQUEST_WORDS
                    .iter()
                    .any(|word| fn_val.contains(word))
                    || EXPECTED_REQUEST_HEADS.iter().any(|h| fn_val.contains(h)));
            let fn_is_resp = !fn_val.is_empty()
                && (SECTION_RESPONSE_WORDS
                    .iter()
                    .any(|word| fn_val.contains(word))
                    || EXPECTED_RESPONSE_HEADS.iter().any(|h| fn_val.contains(h)));

            if is_req || fn_is_req {
                current_section = Some("request");
                continue;
            }
            if is_resp || fn_is_resp {
                current_section = Some("response");
                continue;
            }

            // Check ALL cells for section markers (solo label cells like "输入区域"
            // that may be in any column, with column 0 empty)
            let any_req = cells.iter().any(|c| {
                if let Data::String(s) = c {
                    let v = s.trim().to_lowercase();
                    !v.is_empty()
                        && (SECTION_REQUEST_WORDS.iter().any(|word| v.contains(word))
                            || EXPECTED_REQUEST_HEADS.iter().any(|h| v.contains(h)))
                } else {
                    false
                }
            });
            let any_resp = cells.iter().any(|c| {
                if let Data::String(s) = c {
                    let v = s.trim().to_lowercase();
                    !v.is_empty()
                        && (SECTION_RESPONSE_WORDS.iter().any(|word| v.contains(word))
                            || EXPECTED_RESPONSE_HEADS.iter().any(|h| v.contains(h)))
                } else {
                    false
                }
            });
            if any_req {
                current_section = Some("request");
                continue;
            }
            if any_resp {
                current_section = Some("response");
                continue;
            }
        }

        if !is_data_row(cells) {
            current_section = None;
            continue;
        }

        // Skip label rows (merged section titles etc.)
        if is_label_row(cells, Some(mapping.field_name_col)) {
            current_section = None;
            continue;
        }

        if current_section.is_none() {
            current_section = Some("request");
        }

        let raw_name = cells
            .get(mapping.field_name_col)
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        if raw_name.is_empty() {
            continue;
        }

        // If the field_name value itself looks like a column header (e.g., "英文名称",
        // "数据类型", "是否必填", "中文名称"), skip this row — it's the header row itself
        let fn_lower = raw_name.to_lowercase();
        if EXPECTED_FIELD_NAME_HEADS
            .iter()
            .any(|h| fn_lower.contains(h))
            || EXPECTED_ESB_HEADS.iter().any(|h| fn_lower.contains(h))
            || EXPECTED_TYPE_HEADS.iter().any(|h| fn_lower.contains(h))
            || EXPECTED_REQUIRED_HEADS.iter().any(|h| fn_lower.contains(h))
            || EXPECTED_MEANING_HEADS.iter().any(|h| fn_lower.contains(h))
        {
            current_section = None;
            continue;
        }

        // Skip metadata rows (交易名称, 交易描述, etc.) — capture as interface description
        if is_meta_field(&raw_name) {
            let meta_val = mapping
                .meaning_col
                .and_then(|ci| cells.get(ci))
                .map(parse_field_value)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    mapping
                        .esb_name_col
                        .and_then(|ci| cells.get(ci))
                        .map(parse_field_value)
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_default();
            if !meta_val.is_empty() {
                let new_desc = format!("{} / {}", meta_val, raw_name);
                if description.is_empty() {
                    description = new_desc;
                } else if !description.contains(&meta_val) {
                    description.push_str(" | ");
                    description.push_str(&new_desc);
                }
            }
            continue;
        }

        let java_name = to_java_const_name(&raw_name);

        let esb_field_value = mapping
            .esb_name_col
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| java_name.clone());

        let raw_type = mapping
            .data_type_col
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "string".to_string());

        let required = mapping
            .required_col
            .and_then(|ci| cells.get(ci))
            .map(is_required)
            .unwrap_or(false);

        let meaning = mapping
            .meaning_col
            .and_then(|ci| cells.get(ci))
            .map(parse_field_value)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();

        let field = FieldDef {
            field_name: java_name,
            esb_field_name: esb_field_value,
            data_type: normalize_data_type(&raw_type),
            required,
            meaning,
            is_array: false,
            array_items: Vec::new(),
        };

        match current_section {
            Some("request") => request_fields.push(field),
            _ => response_fields.push(field),
        }
    }

    post_process_arrays(&mut request_fields);
    post_process_arrays(&mut response_fields);

    SheetParseResult {
        request_fields,
        response_fields,
        description,
    }
}

pub fn preview_headers(file_path: &str) -> Result<Vec<SheetHeaders>, GatewayError> {
    let mut workbook =
        open_workbook_auto(file_path).map_err(|e| GatewayError::Open(e.to_string()))?;

    let sheet_names = workbook.sheet_names().to_vec();
    let mut result = Vec::new();

    for sheet_name in &sheet_names {
        let trimmed = sheet_name.trim();
        if should_skip_sheet(trimmed) || is_common_header_sheet(trimmed) {
            continue;
        }

        // calamine 0.30 的 `worksheet_range` 直接返回 `Result`（0.22 是
        // `Option<Result>`），所以「sheet 不在」这一档已经不可能发生。
        let range = workbook
            .worksheet_range(sheet_name)
            .map_err(|e| GatewayError::SheetRead {
                sheet: sheet_name.clone(),
                detail: e.to_string(),
            })?;

        let rows: Vec<Vec<Data>> = range.rows().map(|r: &[Data]| r.to_vec()).collect();
        if rows.is_empty() {
            result.push(SheetHeaders {
                sheet_name: trimmed.to_string(),
                headers: Vec::new(),
                sample_rows: Vec::new(),
            });
            continue;
        }

        // Find the real column-header row: look for the first row where at least
        // 2 cells match different header keyword groups (same logic as parse_sheet_fields_mapped)
        let header_idx = {
            let scan_end = rows.len().min(8);
            let mut found = 0usize;
            'outer: for ri in 0..scan_end {
                let row = &rows[ri];
                if row.is_empty() {
                    continue;
                }
                let mut matched_groups = 0u8;
                for c in row.iter() {
                    if let Data::String(s) = c {
                        let v = s.trim().to_lowercase();
                        if v.is_empty() {
                            continue;
                        }
                        if EXPECTED_FIELD_NAME_HEADS.iter().any(|h| v.contains(h))
                            || EXPECTED_ESB_HEADS.iter().any(|h| v.contains(h))
                        {
                            matched_groups |= 1;
                        }
                        if EXPECTED_TYPE_HEADS.iter().any(|h| v.contains(h)) {
                            matched_groups |= 2;
                        }
                        if EXPECTED_REQUIRED_HEADS.iter().any(|h| v.contains(h)) {
                            matched_groups |= 4;
                        }
                        if EXPECTED_MEANING_HEADS.iter().any(|h| v.contains(h)) {
                            matched_groups |= 8;
                        }
                        if matched_groups.count_ones() >= 2 {
                            found = ri;
                            break 'outer;
                        }
                    }
                }
            }
            found
        };

        // The actual column header names (from the identified header row)
        let headers: Vec<String> = rows
            .get(header_idx)
            .map(|row| row.iter().map(parse_field_value).collect())
            .unwrap_or_default();

        // Collect sample DATA rows starting AFTER the header row (up to 30 rows scanned)
        let mut sample_rows: Vec<Vec<String>> = Vec::new();
        for row in rows.iter().skip(header_idx + 1).take(30) {
            if sample_rows.len() >= 10 {
                break;
            }
            // Skip label rows (section markers like "输入区域", etc.)
            let is_label = {
                let non_empty: Vec<&Data> = row
                    .iter()
                    .filter(|c| matches!(c, Data::String(s) if !s.trim().is_empty()))
                    .collect();
                non_empty.is_empty()
                    || (non_empty.len() <= 2 && row.len() > 3)
                    || non_empty.iter().any(|c| {
                        if let Data::String(s) = c {
                            let v = s.trim().to_lowercase();
                            SECTION_LABEL_KEYWORDS.iter().any(|k| v.contains(k))
                                || EXPECTED_REQUEST_HEADS.iter().any(|h| v.contains(h))
                                || EXPECTED_RESPONSE_HEADS.iter().any(|h| v.contains(h))
                        } else {
                            false
                        }
                    })
            };
            if is_label {
                continue;
            }

            // Also skip rows that look like column headers (2+ header keyword groups),
            // in case header_idx was off
            let is_header_like = {
                let mut groups = 0u8;
                for cell in row.iter() {
                    if let Data::String(s) = cell {
                        let v = s.trim().to_lowercase();
                        if v.is_empty() {
                            continue;
                        }
                        if EXPECTED_FIELD_NAME_HEADS.iter().any(|h| v.contains(h))
                            || EXPECTED_ESB_HEADS.iter().any(|h| v.contains(h))
                        {
                            groups |= 1;
                        }
                        if EXPECTED_TYPE_HEADS.iter().any(|h| v.contains(h)) {
                            groups |= 2;
                        }
                        if EXPECTED_REQUIRED_HEADS.iter().any(|h| v.contains(h)) {
                            groups |= 4;
                        }
                        if EXPECTED_MEANING_HEADS.iter().any(|h| v.contains(h)) {
                            groups |= 8;
                        }
                    }
                }
                groups.count_ones() >= 2
            };
            if is_header_like {
                continue;
            }

            let vals: Vec<String> = row.iter().map(parse_field_value).collect();
            if vals.iter().any(|v| !v.is_empty()) {
                sample_rows.push(vals);
            }
        }

        result.push(SheetHeaders {
            sheet_name: trimmed.to_string(),
            headers,
            sample_rows,
        });
    }

    Ok(result)
}

pub fn parse_excel(
    file_path: &str,
    esb_side: &str,
    col_mapping: Option<&ColMapping>,
) -> Result<ExcelParseResult, GatewayError> {
    let mut workbook =
        open_workbook_auto(file_path).map_err(|e| GatewayError::Open(e.to_string()))?;

    let sheet_names = workbook.sheet_names().to_vec();
    if sheet_names.is_empty() {
        return Err(GatewayError::NoSheets);
    }

    let use_mapped = col_mapping.is_some();
    let mapping = col_mapping;

    // ── 格式预检 ──
    let mut valid_sheets = 0usize;
    let mut invalid_sheet_reason = String::new();
    for sheet_name in &sheet_names {
        let trimmed = sheet_name.trim();
        if trimmed.is_empty() || should_skip_sheet(trimmed) || is_common_header_sheet(trimmed) {
            continue;
        }

        if is_function_id(trimmed) || use_mapped {
            valid_sheets += 1;
        } else {
            invalid_sheet_reason = trimmed.to_string();
        }
    }

    if valid_sheets == 0 {
        // 源仓在这里还拼了一句「请下载标准模板…」的提示，挪到插件层了。
        if !invalid_sheet_reason.is_empty() {
            return Err(GatewayError::BadSheetName(invalid_sheet_reason));
        }
        return Err(GatewayError::NoInterfaces);
    }

    let mut all_interfaces: Vec<InterfaceDef> = Vec::new();
    let mut bank_name = String::new();

    for sheet_name in &sheet_names {
        let trimmed = sheet_name.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Extract bank name from any sheet name
        if bank_name.is_empty() {
            let lower = trimmed.to_lowercase();
            if !lower.starts_with("sheet")
                && !should_skip_sheet(trimmed)
                && !is_common_header_sheet(trimmed)
                && !is_function_id(trimmed)
            {
                bank_name = trimmed.to_string();
            }
        }

        // Skip irrelevant sheets
        if should_skip_sheet(trimmed) {
            continue;
        }

        // calamine 0.30 的 `worksheet_range` 直接返回 `Result`（0.22 是
        // `Option<Result>`），所以「sheet 不在」这一档已经不可能发生。
        let range = workbook
            .worksheet_range(sheet_name)
            .map_err(|e| GatewayError::SheetRead {
                sheet: sheet_name.clone(),
                detail: e.to_string(),
            })?;

        let rows: Vec<Vec<Data>> = range.rows().map(|r: &[Data]| r.to_vec()).collect();
        if rows.is_empty() {
            continue;
        }

        if is_common_header_sheet(trimmed) {
            // Skip header sheets (报文头/公共报文头/消息头) — only parse transaction code sheets
            continue;
        }

        if !is_function_id(trimmed) && !use_mapped {
            continue;
        }

        let result = if use_mapped {
            parse_sheet_fields_mapped(&rows, mapping.expect("mapping checked by use_mapped"), true)
        } else {
            parse_sheet_fields(&rows, esb_side)
        };

        // Try to extract actual function_id from sheet content
        let function_id = if use_mapped {
            trimmed.to_string()
        } else {
            extract_function_id(&rows)
                .filter(|fid| is_function_id(fid))
                .unwrap_or_else(|| trimmed.to_string())
        };

        if !result.request_fields.is_empty() || !result.response_fields.is_empty() {
            all_interfaces.push(InterfaceDef {
                function_id,
                description: result.description,
                interface_type: "bank".to_string(),
                business_domain: String::new(),
                request_fields: result.request_fields,
                response_fields: result.response_fields,
            });
        }
    }

    if all_interfaces.is_empty() {
        return Err(GatewayError::NoInterfaces);
    }

    Ok(ExcelParseResult {
        bank_name,
        interfaces: all_interfaces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[&str]) -> Vec<Data> {
        values
            .iter()
            .map(|value| Data::String((*value).to_string()))
            .collect()
    }

    #[test]
    fn function_id_needs_letters_then_at_least_four_digits() {
        assert!(is_function_id("B100001"));
        // 纯数字不行：参考仓要求「字母前缀 + 至少 4 位数字」。
        assert!(!is_function_id("100001"));
        assert!(!is_function_id("1000"));
        assert!(!is_function_id("B100"));
        assert!(!is_function_id("B10000A"));
        assert!(!is_function_id("BLONG100001"));
    }

    #[test]
    fn java_const_name_is_camel_cased_from_any_separator() {
        assert_eq!(to_java_const_name("serial_no"), "SerialNo");
        assert_eq!(to_java_const_name("serial-no"), "SerialNo");
        assert_eq!(to_java_const_name("serial no"), "SerialNo");
        assert_eq!(to_java_const_name("accNo"), "AccNo");
        assert_eq!(to_java_const_name("  "), "");
    }

    #[test]
    fn data_types_normalize_to_three_buckets() {
        assert_eq!(normalize_data_type("C18"), "string");
        assert_eq!(normalize_data_type("VARCHAR2(32)"), "string");
        assert_eq!(normalize_data_type("NUMBER"), "number");
        assert_eq!(normalize_data_type("BigDecimal"), "number");
        assert_eq!(normalize_data_type("金额"), "amount");
        assert_eq!(normalize_data_type("价格"), "amount");
        assert_eq!(normalize_data_type(""), "string");
    }

    #[test]
    fn required_flags_follow_the_reference_set() {
        assert!(is_required(&Data::String("Y".into())));
        assert!(is_required(&Data::String("必填".into())));
        assert!(!is_required(&Data::String("N".into())));
        assert!(!is_required(&Data::String("可选".into())));
        assert!(!is_required(&Data::Int(1)));
    }

    #[test]
    fn single_group_sheet_splits_request_and_response() {
        let rows = vec![
            row(&[
                "内部字段名",
                "外部字段名",
                "数据类型",
                "是否必填",
                "字段含义",
            ]),
            row(&["serial_no", "serialNo", "string", "Y", "流水号"]),
            row(&["请求字段", "", "", "", ""]),
            row(&["accNo", "accNo", "string", "Y", "账号"]),
            row(&["应答字段", "", "", "", ""]),
            row(&["returnCode", "returnCode", "string", "Y", "返回码"]),
        ];
        let parsed = parse_sheet_fields(&rows, "right");
        // 头一行之前的「请求字段」标记不起作用：第一行数据默认进请求区，
        // 所以显式标记之后的 `accNo` 仍在请求区（参考仓口径）。
        assert_eq!(
            parsed.request_fields.len(),
            2,
            "{:?}",
            parsed.request_fields
        );
        assert_eq!(parsed.request_fields[0].field_name, "SerialNo");
        assert_eq!(parsed.request_fields[0].esb_field_name, "serialNo");
        assert_eq!(
            parsed.response_fields.len(),
            1,
            "{:?}",
            parsed.response_fields
        );
        assert_eq!(parsed.response_fields[0].field_name, "ReturnCode");
        assert!(parsed.response_fields[0].required);
    }

    #[test]
    fn dual_group_sheet_picks_internal_side_by_flag() {
        let rows = vec![
            row(&["字段名", "数据类型", "字段名", "数据类型", "字段含义"]),
            row(&["serialNo", "string", "serial_no", "string", "流水号"]),
            row(&["amount", "金额", "amt", "金额", "交易金额"]),
        ];
        let right = parse_sheet_fields(&rows, "right");
        assert_eq!(right.request_fields[0].field_name, "SerialNo");
        assert_eq!(right.request_fields[0].esb_field_name, "serial_no");
        let left = parse_sheet_fields(&rows, "left");
        // 两侧都做驼峰化：`serial_no` 与 `serialNo` 都变 `SerialNo`，区别在 ESB 名。
        assert_eq!(left.request_fields[0].field_name, "SerialNo");
        assert_eq!(left.request_fields[0].esb_field_name, "serialNo");
    }

    #[test]
    fn array_markers_collapse_into_one_field_with_items() {
        let mut fields = vec![
            FieldDef {
                field_name: "List".into(),
                esb_field_name: "dataList".into(),
                data_type: "array".into(),
                required: false,
                meaning: "明细数组".into(),
                is_array: false,
                array_items: Vec::new(),
            },
            FieldDef {
                field_name: "ItemNo".into(),
                esb_field_name: "itemNo".into(),
                data_type: "string".into(),
                required: true,
                meaning: "序号".into(),
                is_array: false,
                array_items: Vec::new(),
            },
            FieldDef {
                field_name: "List".into(),
                esb_field_name: String::new(),
                data_type: "string".into(),
                required: false,
                meaning: String::new(),
                is_array: false,
                array_items: Vec::new(),
            },
            FieldDef {
                field_name: "Tail".into(),
                esb_field_name: "tail".into(),
                data_type: "string".into(),
                required: false,
                meaning: "尾部".into(),
                is_array: false,
                array_items: Vec::new(),
            },
        ];
        post_process_arrays(&mut fields);
        assert_eq!(fields.len(), 2);
        assert!(fields[0].is_array);
        assert_eq!(fields[0].field_name, "dataList");
        assert_eq!(fields[0].array_items.len(), 1);
        assert_eq!(fields[0].array_items[0].field_name, "ItemNo");
        assert_eq!(fields[1].field_name, "Tail");
    }

    #[test]
    fn metadata_rows_become_the_interface_description() {
        let rows = vec![
            row(&["内部字段名", "数据类型", "是否必填", "字段含义"]),
            row(&["交易名称", "账户余额查询", "", ""]),
            row(&["accNo", "string", "Y", "账号"]),
        ];
        let parsed = parse_sheet_fields(&rows, "right");
        assert!(
            parsed.description.contains("账户余额查询"),
            "{}",
            parsed.description
        );
        assert_eq!(parsed.request_fields.len(), 1);
    }

    #[test]
    fn mapped_mode_uses_declared_columns() {
        let rows = vec![
            row(&["序号", "英文名", "类型", "必填", "中文名"]),
            row(&["1", "accNo", "string", "Y", "账号"]),
            row(&["2", "balance", "金额", "N", "余额"]),
        ];
        let mapping = ColMapping {
            field_name_col: 1,
            esb_name_col: None,
            data_type_col: Some(2),
            required_col: Some(3),
            meaning_col: Some(4),
        };
        let parsed = parse_sheet_fields_mapped(&rows, &mapping, false);
        assert_eq!(parsed.request_fields.len(), 2);
        assert_eq!(parsed.request_fields[0].field_name, "AccNo");
        assert_eq!(parsed.request_fields[1].data_type, "amount");
        assert!(parsed.request_fields[0].required);
    }

    #[test]
    fn skip_and_header_sheet_rules() {
        assert!(should_skip_sheet("修订记录"));
        assert!(should_skip_sheet("Sheet1"));
        assert!(!should_skip_sheet("B100001"));
        assert!(is_common_header_sheet("公共报文头"));
        assert!(!is_common_header_sheet("B100001"));
    }
}
