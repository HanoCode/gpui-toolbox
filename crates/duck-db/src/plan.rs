//! 执行计划的结构化：把各引擎的 `EXPLAIN` 输出统一成一棵树。
//!
//! # 为什么要有这一层
//!
//! `EXPLAIN` 的输出在七条引擎上是七种形状：SQLite 的 `EXPLAIN QUERY PLAN` 是四列
//! （`id` / `parent` / `notused` / `detail`，树靠 `parent` 指出来），PostgreSQL 的
//! `EXPLAIN (FORMAT JSON)` 是嵌套 JSON，MySQL 的 `EXPLAIN FORMAT=JSON` 是另一种嵌套
//! JSON（`query_block` / `table`），ClickHouse 的 `EXPLAIN` 是缩进文本。界面把它们当
//! 普通文本塞进网格时，**缩进与层级全丢了** —— 计划的价值恰好在层级上。
//!
//! 这个模块只做一件事：`&ResultSet` → [`PlanTree`]。零 gpui、零引擎句柄、纯函数，
//! 因此可以脱开连接单测（各引擎的样例直接抄官方文档的形态，见本文件的测试）。
//!
//! # 解析不了就原样返回
//!
//! [`PlanTree::Raw`] 不是失败态，是**兜底态**：认不出的形状一律把原始文本交回去，
//! 让界面照旧按文本展示。计划看不懂也比一片空白强，**绝不允许把输出吞掉**。
//!
//! # 识别靠形状，不靠引擎名
//!
//! 调用方不需要告诉这里"这是哪个引擎"：SQLite 的列名、MySQL 的列名、JSON 的顶层键
//! 各自都足够特异。这样的好处是用户自己敲 `EXPLAIN FORMAT=JSON`（MySQL 的另一种写法）
//! 或 `EXPLAIN (FORMAT JSON)` 时，同样能被认出来。

use std::collections::HashMap;

use serde_json::Value as Json;

use crate::result::{ResultSet, Value};

/// 缩进树的最大层级。
///
/// 防御畸形输入（病态缩进 / 自环的 `parent`）：层级只用于渲染缩进，超过这个数就压平，
/// 免得界面上出现一条几万层缩进的线。
const MAX_DEPTH: usize = 64;

/// 计划树里的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNode {
    /// 层级（0 = 顶层）。渲染时按它缩进。
    pub depth: usize,
    /// 这一步的主文案（PG 的 `Node Type` + 表名、SQLite 的 `detail`、MySQL 的节点键…）。
    pub label: String,
    /// 附加信息（成本 / 行数 / 过滤条件…）。顺序即展示顺序。
    pub details: Vec<(String, String)>,
}

/// 一份执行计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanTree {
    /// 解析成功：缩进树。
    Nodes(Vec<PlanNode>),
    /// 解析不了：**原样文本**（调用方照旧按文本展示，不吞输出）。
    Raw(String),
}

impl PlanTree {
    /// 是不是一棵空树（解析成功但一个节点都没有）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        match self {
            Self::Nodes(nodes) => nodes.is_empty(),
            Self::Raw(_) => false,
        }
    }
}

/// 把一份 `EXPLAIN` 的结果集解析成计划树。
///
/// 按形状依次试：SQLite 的 `parent` 树 → MySQL 的传统 EXPLAIN 表 → 「单列文本」里的
/// JSON → 「单列文本」里的缩进树；都不成形就给 [`PlanTree::Raw`]。
#[must_use]
pub fn parse_plan(result: &ResultSet) -> PlanTree {
    if let Some(nodes) = sqlite_nodes(result) {
        return PlanTree::Nodes(nodes);
    }
    if let Some(nodes) = mysql_explain_table(result) {
        return PlanTree::Nodes(nodes);
    }
    if let Some(text) = single_column_text(result) {
        if let Some(nodes) = json_nodes(&text) {
            return PlanTree::Nodes(nodes);
        }
        if let Some(nodes) = indented_nodes(&text) {
            return PlanTree::Nodes(nodes);
        }
    }
    PlanTree::Raw(raw_text(result))
}

// ── SQLite：`EXPLAIN QUERY PLAN` 的 `parent` 树 ────────────────────────────────

/// SQLite 的 `EXPLAIN QUERY PLAN`：四列 `id / parent / notused / detail`。
///
/// 树的层级要**顺着 `parent` 往上数**（`parent` 是父节点的 `id`，0 表示顶层）。
/// `detail` 列的文案本身就是给人看的（`SCAN t` / `SEARCH t USING INDEX …`），直接当标签。
fn sqlite_nodes(result: &ResultSet) -> Option<Vec<PlanNode>> {
    let names = lowered_columns(result);
    let id_index = position(&names, "id")?;
    let parent_index = position(&names, "parent")?;
    let detail_index = position(&names, "detail")?;
    let mut parents: HashMap<i64, i64> = HashMap::with_capacity(result.rows.len());
    for row in &result.rows {
        parents.insert(
            int_cell(row.get(id_index)?)?,
            int_cell(row.get(parent_index)?)?,
        );
    }
    let mut nodes = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut cursor = int_cell(row.get(id_index)?)?;
        let mut depth = 0;
        // 顺着父链往上数；自环 / 悬空父节点 / 过深的链都在这里截断（防御畸形输出）。
        while depth < MAX_DEPTH {
            match parents.get(&cursor) {
                Some(&parent)
                    if parent != 0 && parent != cursor && parents.contains_key(&parent) =>
                {
                    depth += 1;
                    cursor = parent;
                }
                _ => break,
            }
        }
        nodes.push(PlanNode {
            depth,
            label: row.get(detail_index)?.render(),
            details: Vec::new(),
        });
    }
    (!nodes.is_empty()).then_some(nodes)
}

/// 一个整数格（SQLite 的 `id` / `parent` 恒是整数；形状不对就当不是它）。
fn int_cell(value: &Value) -> Option<i64> {
    match value {
        Value::Int(number) => Some(*number),
        // 有些引擎（经 HTTP 接口）会把整数原样给成文本。
        Value::Text(text) | Value::Decimal(text) => text.trim().parse().ok(),
        _ => None,
    }
}

// ── MySQL：传统 EXPLAIN 的结果表 ──────────────────────────────────────────────

/// MySQL 传统 `EXPLAIN`（不带 `FORMAT=JSON`）返回一张**表**而不是树：
/// `id / select_type / table / type / possible_keys / key / key_len / ref / rows / Extra`。
///
/// 它没有层级信息，所以按"一行一步"平铺（`depth` 恒 0），把其余列摊成明细——
/// 好歹把「哪张表、走不走索引」摆成可读的一行行，而不是原样网格。
fn mysql_explain_table(result: &ResultSet) -> Option<Vec<PlanNode>> {
    let names = lowered_columns(result);
    let table_index = position(&names, "table")?;
    // 这条判据是「MySQL 传统 EXPLAIN」的指纹；缺了它就别认（SQLite 的 `EXPLAIN` 也是表，
    // 但那是字节码清单 `addr / opcode / p1 …`，认错了反而更糟）。
    let select_type_index = position(&names, "select_type")?;
    let mut nodes = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let select_type = row
            .get(select_type_index)
            .map_or_else(String::new, Value::render);
        let table = row.get(table_index).map_or_else(String::new, Value::render);
        let label = if table.is_empty() || table == "NULL" {
            select_type.clone()
        } else if select_type.is_empty() {
            table.clone()
        } else {
            format!("{table} · {select_type}")
        };
        let mut details = Vec::new();
        for (index, name) in names.iter().enumerate() {
            if index == table_index || index == select_type_index {
                continue;
            }
            let Some(cell) = row.get(index) else { continue };
            if cell.is_null() {
                continue;
            }
            let text = cell.render();
            if text.is_empty() {
                continue;
            }
            details.push((name.clone(), text));
        }
        nodes.push(PlanNode {
            depth: 0,
            label,
            details,
        });
    }
    (!nodes.is_empty()).then_some(nodes)
}

// ── JSON 形态：PG 的 `Plan` 树、MySQL 的 `query_block` 树 ─────────────────────

/// 一列文本 → 树。多数引擎的文本格式计划都是「一行一步、缩进表层级」。
fn json_nodes(text: &str) -> Option<Vec<PlanNode>> {
    let value: Json = serde_json::from_str(text.trim()).ok()?;
    // PostgreSQL：`[{"Plan": {…}, "Planning Time": …, "Execution Time": …}]`。
    if let Some(entries) = value.as_array() {
        for entry in entries {
            if let Some(plan) = entry.get("Plan") {
                let mut nodes = Vec::new();
                push_nested_plan(plan, 0, &mut nodes);
                if !nodes.is_empty() {
                    // 计划 / 执行耗时挂在**顶层节点**上：它们不属于任何一个计划步骤。
                    let timing = ["Planning Time", "Execution Time"]
                        .iter()
                        .filter_map(|key| {
                            entry.get(*key).map(|v| ((*key).to_owned(), scalar_text(v)))
                        })
                        .collect::<Vec<_>>();
                    if let Some(root) = nodes.first_mut() {
                        root.details.extend(timing);
                    }
                    return Some(nodes);
                }
            }
        }
    }
    // MySQL：`{"query_block": {…}}`。
    if let Some(block) = value.get("query_block") {
        let mut nodes = Vec::new();
        push_object_plan("query_block", block, 0, &mut nodes);
        if !nodes.is_empty() {
            return Some(nodes);
        }
    }
    // 顶层直接是 `{"Plan": …}` 的形态（ClickHouse 的 `EXPLAIN json = 1` 与之同构）。
    if let Some(plan) = value.get("Plan") {
        let mut nodes = Vec::new();
        push_nested_plan(plan, 0, &mut nodes);
        if !nodes.is_empty() {
            return Some(nodes);
        }
    }
    None
}

/// PostgreSQL（以及同构的 `Plan` / `Plans` 形态）的递归展开。
///
/// 标签取 `Node Type`（有 `Relation Name` 就缀上表名）——这两项是 PG 计划里唯一
/// "一眼看出这一步在干什么"的信息。其余标量字段摊成明细，嵌套结构交给 `Plans`。
fn push_nested_plan(value: &Json, depth: usize, out: &mut Vec<PlanNode>) {
    let Some(map) = value.as_object() else {
        return;
    };
    let node_type = map
        .get("Node Type")
        .map_or_else(|| "Plan".to_owned(), scalar_text);
    let label = match map.get("Relation Name") {
        Some(relation) => format!("{node_type} on {}", scalar_text(relation)),
        None => node_type,
    };
    let mut details = Vec::new();
    for (key, entry) in map {
        // 已经进了标签的、以及承载子节点的，都不再当明细。
        if matches!(
            key.as_str(),
            "Node Type" | "Relation Name" | "Alias" | "Plans"
        ) {
            continue;
        }
        if entry.is_object() || entry.is_array() {
            continue;
        }
        details.push((key.clone(), scalar_text(entry)));
    }
    out.push(PlanNode {
        depth,
        label,
        details,
    });
    if let Some(children) = map.get("Plans").and_then(Json::as_array) {
        for child in children {
            push_nested_plan(child, (depth + 1).min(MAX_DEPTH), out);
        }
    }
}

/// MySQL `EXPLAIN FORMAT=JSON` 里"还会再往下嵌套计划"的键。
///
/// 只有这些键会另起一个节点；其余对象（`cost_info` 之类）是**明细**，摊平成
/// `对象名.字段名`——不然 `cost_info` 会当成一步计划，把树撑得又深又假。
const MYSQL_PLAN_KEYS: &[&str] = &[
    "query_block",
    "table",
    "nested_loop",
    "ordering_operation",
    "grouping_operation",
    "duplicates_removal",
    "materialized_from_subquery",
    "attached_subqueries",
    "union_result",
    "buffer_result",
    "subqueries",
    "query_specifications",
    "optimized_away_subqueries",
];

/// MySQL JSON 计划的递归展开（见 [`MYSQL_PLAN_KEYS`]）。
fn push_object_plan(key: &str, value: &Json, depth: usize, out: &mut Vec<PlanNode>) {
    match value {
        // 数组（`nested_loop` / `attached_subqueries`）：每个元素各起一个同名节点。
        Json::Array(items) => {
            for item in items {
                push_object_plan(key, item, depth, out);
            }
        }
        Json::Object(map) => {
            let label = match map.get("table_name").map(scalar_text) {
                Some(name) => format!("{key} · {name}"),
                None => key.to_owned(),
            };
            let mut details = Vec::new();
            let mut children: Vec<(&str, &Json)> = Vec::new();
            for (field, entry) in map {
                if MYSQL_PLAN_KEYS.contains(&field.as_str()) {
                    children.push((field.as_str(), entry));
                    continue;
                }
                // 表名已经进了标签，不再重复成明细。
                if field == "table_name" {
                    continue;
                }
                match entry {
                    // 非计划子对象（`cost_info`）摊平成明细。
                    Json::Object(inner) => {
                        for (sub, leaf) in inner {
                            if !leaf.is_object() && !leaf.is_array() {
                                details.push((format!("{field}.{sub}"), scalar_text(leaf)));
                            }
                        }
                    }
                    Json::Array(_) | Json::Null => {}
                    leaf => details.push((field.clone(), scalar_text(leaf))),
                }
            }
            out.push(PlanNode {
                depth,
                label,
                details,
            });
            for (field, entry) in children {
                push_object_plan(field, entry, (depth + 1).min(MAX_DEPTH), out);
            }
        }
        // 数组元素里直接是标量的情形（罕见）：也给它一行，别丢。
        leaf => out.push(PlanNode {
            depth,
            label: format!("{key}: {}", scalar_text(leaf)),
            details: Vec::new(),
        }),
    }
}

// ── 文本缩进形态：PG 文本格式、ClickHouse 文本格式 ───────────────────────────

/// 缩进文本 → 树。
///
/// 判据是**缩进本身**：整段文本里出现过两种以上不同的缩进（或带 PG 的 `->` 标记）
/// 才认。否则返回 `None` —— 一段没有层级的文本（比如 `EXPLAIN` 出来的一列说明）
/// 硬套成树只会多出一层没有意义的缩进。
fn indented_nodes(text: &str) -> Option<Vec<PlanNode>> {
    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut levels: Vec<usize> = Vec::new();
    let mut saw_arrow = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let body = trimmed.strip_prefix("->").map_or(trimmed, str::trim_start);
        if body.is_empty() {
            continue;
        }
        saw_arrow |= trimmed.starts_with("->");
        if !levels.contains(&indent) {
            levels.push(indent);
        }
        rows.push((indent, body.to_owned()));
    }
    if rows.is_empty() || (levels.len() < 2 && !saw_arrow) {
        return None;
    }
    levels.sort_unstable();
    let nodes = rows
        .into_iter()
        .map(|(indent, label)| PlanNode {
            // 第二趟才知道全部缩进档，所以层级在这一趟算：比它浅的档数就是层级。
            depth: levels
                .iter()
                .position(|level| *level == indent)
                .unwrap_or(0)
                .min(MAX_DEPTH),
            label,
            details: Vec::new(),
        })
        .collect();
    Some(nodes)
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

/// 列名小写化（识别靠列名，大小写不该影响判定）。
fn lowered_columns(result: &ResultSet) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|column| column.name.trim().to_ascii_lowercase())
        .collect()
}

/// 列名在清单里的下标。
fn position(names: &[String], wanted: &str) -> Option<usize> {
    names.iter().position(|name| name == wanted)
}

/// 结果集是不是「一列文本」（多数引擎的文本计划就是这个形状：一行一步）。
///
/// 多行会拼成多行文本——缩进信息只能这样读得到。
fn single_column_text(result: &ResultSet) -> Option<String> {
    if result.columns.len() != 1 {
        return None;
    }
    let mut lines = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let Some(cell) = row.first() else { continue };
        lines.push(match cell {
            // `Json` 格直接给原文（别把引号与转义丢掉）。
            Value::Json(text) => text.clone(),
            other => other.render(),
        });
    }
    Some(lines.join("\n"))
}

/// JSON 里的标量 → 展示文本。
fn scalar_text(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        Json::Null => "NULL".to_owned(),
        other => other.to_string(),
    }
}

/// 兜底用的原样文本（多列时带上表头，与网格一致）。
fn raw_text(result: &ResultSet) -> String {
    let mut out = String::new();
    if result.columns.len() > 1 {
        let header = result
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>()
            .join("\t");
        out.push_str(&header);
        out.push('\n');
    }
    for row in &result.rows {
        let line = row.iter().map(Value::render).collect::<Vec<_>>().join("\t");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// 从 `Map` 里取一个标量字段（给测试与内部分支用）。
#[cfg(test)]
fn json_map(pairs: &[(&str, Json)]) -> Json {
    Json::Object(
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect::<serde_json::Map<String, Json>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    fn columns(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|name| Column {
                name: (*name).to_owned(),
                type_name: "text".to_owned(),
            })
            .collect()
    }

    fn text_result(names: &[&str], rows: Vec<Vec<Value>>) -> ResultSet {
        ResultSet {
            columns: columns(names),
            rows,
            truncated: false,
        }
    }

    /// 一列文本的便捷构造（PG / ClickHouse 的文本计划就是这个形状）。
    fn one_column(rows: &[&str]) -> ResultSet {
        text_result(
            &["QUERY PLAN"],
            rows.iter()
                .map(|line| vec![Value::Text((*line).to_owned())])
                .collect(),
        )
    }

    /// SQLite `EXPLAIN QUERY PLAN`：层级顺着 `parent` 数出来。
    #[test]
    fn sqlite_parent_links_become_depth() {
        let result = text_result(
            &["id", "parent", "notused", "detail"],
            vec![
                vec![
                    Value::Int(2),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Text("SCAN t".into()),
                ],
                // 子查询挂在 id=2 下 → 一层；孙节点 id=5 挂在 id=4 下 → 两层。
                vec![
                    Value::Int(4),
                    Value::Int(2),
                    Value::Int(0),
                    Value::Text("SEARCH u USING INDEX idx (a=?)".into()),
                ],
                vec![
                    Value::Int(5),
                    Value::Int(4),
                    Value::Int(0),
                    Value::Text("LIST SUBQUERY 1".into()),
                ],
            ],
        );
        assert_eq!(
            parse_plan(&result),
            PlanTree::Nodes(vec![
                PlanNode {
                    depth: 0,
                    label: "SCAN t".into(),
                    details: Vec::new(),
                },
                PlanNode {
                    depth: 1,
                    label: "SEARCH u USING INDEX idx (a=?)".into(),
                    details: Vec::new(),
                },
                PlanNode {
                    depth: 2,
                    label: "LIST SUBQUERY 1".into(),
                    details: Vec::new(),
                },
            ])
        );
    }

    /// SQLite 的父链自环 / 悬空不能让层级数飞掉。
    #[test]
    fn sqlite_depth_survives_broken_parent_links() {
        let result = text_result(
            &["id", "parent", "notused", "detail"],
            vec![
                // 自环：parent == id。
                vec![
                    Value::Int(1),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Text("SELF".into()),
                ],
                // 悬空：父节点不在结果里。
                vec![
                    Value::Int(2),
                    Value::Int(99),
                    Value::Int(0),
                    Value::Text("DANGLING".into()),
                ],
            ],
        );
        let PlanTree::Nodes(nodes) = parse_plan(&result) else {
            panic!("应当解析成树");
        };
        assert!(
            nodes.iter().all(|node| node.depth == 0),
            "畸形父链按顶层收敛"
        );
    }

    /// PostgreSQL `EXPLAIN (FORMAT JSON)`：`Plan` / `Plans` 递归，耗时挂在根上。
    #[test]
    fn postgres_json_plan_becomes_tree() {
        let plan = json_map(&[
            (
                "Plan",
                json_map(&[
                    ("Node Type", Json::String("Aggregate".into())),
                    ("Startup Cost", serde_json::json!(1.0)),
                    ("Plan Rows", serde_json::json!(1)),
                    (
                        "Plans",
                        Json::Array(vec![json_map(&[
                            ("Node Type", Json::String("Seq Scan".into())),
                            ("Relation Name", Json::String("orders".into())),
                            ("Filter", Json::String("(total > 100)".into())),
                            ("Plan Rows", serde_json::json!(3)),
                        ])]),
                    ),
                ]),
            ),
            ("Planning Time", serde_json::json!(0.42)),
        ]);
        let result = one_column(&[&Json::Array(vec![plan]).to_string()]);

        let PlanTree::Nodes(nodes) = parse_plan(&result) else {
            panic!("应当解析成树");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].label, "Aggregate");
        assert_eq!(nodes[0].depth, 0);
        // 顶层节点带上计划 / 执行耗时。
        assert!(
            nodes[0]
                .details
                .contains(&("Planning Time".to_owned(), "0.42".to_owned()))
        );
        assert_eq!(nodes[1].label, "Seq Scan on orders");
        assert_eq!(nodes[1].depth, 1);
        assert!(
            nodes[1]
                .details
                .contains(&("Filter".to_owned(), "(total > 100)".to_owned()))
        );
    }

    /// MySQL `EXPLAIN FORMAT=JSON`：`query_block` / `table` 成树，`cost_info` 摊成明细。
    #[test]
    fn mysql_json_plan_becomes_tree() {
        let plan = json_map(&[(
            "query_block",
            json_map(&[
                ("select_id", serde_json::json!(1)),
                (
                    "cost_info",
                    json_map(&[("query_cost", Json::String("0.35".into()))]),
                ),
                (
                    "table",
                    json_map(&[
                        ("table_name", Json::String("t".into())),
                        ("access_type", Json::String("ALL".into())),
                        ("rows_examined_per_scan", serde_json::json!(4)),
                    ]),
                ),
            ]),
        )]);
        let result = one_column(&[&plan.to_string()]);

        let PlanTree::Nodes(nodes) = parse_plan(&result) else {
            panic!("应当解析成树");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].label, "query_block");
        assert_eq!(nodes[0].depth, 0);
        assert!(
            nodes[0]
                .details
                .contains(&("cost_info.query_cost".to_owned(), "0.35".to_owned()))
        );
        assert!(
            nodes[0]
                .details
                .contains(&("select_id".to_owned(), "1".to_owned()))
        );
        assert_eq!(nodes[1].label, "table · t");
        assert_eq!(nodes[1].depth, 1);
        assert!(
            nodes[1]
                .details
                .contains(&("access_type".to_owned(), "ALL".to_owned()))
        );
        // 表名进了标签，不再重复成明细。
        assert!(!nodes[1].details.iter().any(|(key, _)| key == "table_name"));
    }

    /// MySQL 传统 EXPLAIN：一行一步平铺（它本来就没有层级）。
    #[test]
    fn mysql_explain_table_is_flattened() {
        let result = text_result(
            &[
                "id",
                "select_type",
                "table",
                "type",
                "possible_keys",
                "key",
                "rows",
                "Extra",
            ],
            vec![
                vec![
                    Value::Text("1".into()),
                    Value::Text("SIMPLE".into()),
                    Value::Text("orders".into()),
                    Value::Text("ALL".into()),
                    Value::Null,
                    Value::Null,
                    Value::Text("10".into()),
                    Value::Text("Using where".into()),
                ],
                // 没有表的一行（比如 `UNION RESULT`）：标签退化成 select_type。
                vec![
                    Value::Text("2".into()),
                    Value::Text("UNION RESULT".into()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            ],
        );
        let PlanTree::Nodes(nodes) = parse_plan(&result) else {
            panic!("应当解析成树");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].label, "orders · SIMPLE");
        assert_eq!(nodes[0].depth, 0);
        // NULL / 空字段不进明细（它们只是没值，不是信息）；`id` 保留——
        // 它是子查询 / UNION 块的编号，平铺之后正是区分「第几步」的唯一线索。
        assert_eq!(
            nodes[0].details,
            vec![
                ("id".to_owned(), "1".to_owned()),
                ("type".to_owned(), "ALL".to_owned()),
                ("rows".to_owned(), "10".to_owned()),
                ("extra".to_owned(), "Using where".to_owned()),
            ]
        );
        assert_eq!(nodes[1].label, "UNION RESULT");
        // 第二行整行为空/NULL：没有任何明细，标签只能退化成 `select_type`。
        assert_eq!(nodes[1].details, vec![("id".to_owned(), "2".to_owned())]);
    }

    /// 文本缩进形态（PG 文本格式 / ClickHouse 文本格式）。
    #[test]
    fn indented_text_becomes_tree() {
        // PG 文本格式：根节点没有 `->`，子节点带 `->` 且逐级缩进。
        let pg = one_column(&[
            "Hash Join  (cost=1.14..2.30 rows=3 width=8)",
            "  Hash Cond: (o.user_id = u.id)",
            "  ->  Seq Scan on orders o  (cost=0.00..1.03 rows=3 width=8)",
            "  ->  Hash  (cost=1.09..1.09 rows=5 width=4)",
            "        ->  Seq Scan on users u  (cost=0.00..1.09 rows=5 width=4)",
        ]);
        let PlanTree::Nodes(nodes) = parse_plan(&pg) else {
            panic!("应当解析成树");
        };
        assert_eq!(
            nodes[0].label,
            "Hash Join  (cost=1.14..2.30 rows=3 width=8)"
        );
        assert_eq!(nodes[0].depth, 0);
        assert_eq!(nodes[1].label, "Hash Cond: (o.user_id = u.id)");
        assert_eq!(nodes[1].depth, 1);
        assert_eq!(nodes[2].depth, 1);
        // `->` 被剥掉：树形由缩进表达，标记本身不再重复。
        assert!(nodes[2].label.starts_with("Seq Scan on orders o"));
        assert_eq!(nodes[4].depth, 2);

        // ClickHouse 文本格式（没有 `->`，纯缩进）。
        let ch = one_column(&[
            "Expression ((Projection + Before ORDER BY))",
            "  Aggregating",
            "    Expression (Before GROUP BY)",
            "      ReadFromMergeTree (default.hits)",
        ]);
        let PlanTree::Nodes(nodes) = parse_plan(&ch) else {
            panic!("应当解析成树");
        };
        assert_eq!(
            nodes.iter().map(|node| node.depth).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    /// 没有层级的文本**不许**硬套成树：宁可原样返回。
    #[test]
    fn flat_text_falls_back_to_raw() {
        let flat = one_column(&[
            "Seq Scan on t",
            "Filter: (a > 1)",
            "Rows Removed by Filter: 2",
        ]);
        let PlanTree::Raw(text) = parse_plan(&flat) else {
            panic!("平铺文本不该被当成树");
        };
        assert!(text.contains("Filter: (a > 1)"), "原始输出必须完整保留");
        assert!(text.contains("Rows Removed by Filter: 2"));
    }

    /// 认不出的形状：原样文本兜底，多列时带表头。
    #[test]
    fn unknown_shapes_keep_the_raw_output() {
        // SQLite 的 `EXPLAIN`（不是 QUERY PLAN）是字节码清单，不是计划树。
        let bytecode = text_result(
            &["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"],
            vec![vec![
                Value::Int(0),
                Value::Text("Init".into()),
                Value::Int(0),
                Value::Int(1),
                Value::Int(0),
                Value::Null,
                Value::Int(0),
                Value::Null,
            ]],
        );
        let PlanTree::Raw(text) = parse_plan(&bytecode) else {
            panic!("字节码清单不该被当成计划树");
        };
        assert!(text.starts_with("addr\topcode"), "多列时带上表头");

        // 空结果集也是兜底态，不是空树。
        let empty = ResultSet::default();
        assert!(matches!(parse_plan(&empty), PlanTree::Raw(_)));
    }

    /// JSON 语法坏了同样兜底（用户可能正在手敲半截 JSON）。
    #[test]
    fn broken_json_falls_back_to_raw() {
        let broken = one_column(&[r#"{"query_block": {"table": {"table_nam"#]);
        let PlanTree::Raw(text) = parse_plan(&broken) else {
            panic!("坏 JSON 该兜底");
        };
        assert!(text.contains("table_nam"));
    }
}
