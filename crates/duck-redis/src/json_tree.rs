//! JSON 文档 → **可折叠树**（对齐 zedis 的值树，`temp/zedis/crates/zedis-core/src/json.rs`）。
//!
//! 纯逻辑：不碰网络、不碰 gpui、不持有界面状态。输入是文本或 `serde_json::Value`，
//! 输出是节点树 + 展平后的行序列；**「哪一层展开了」由调用方给**（一个 path 集合），
//! 所以折叠本身可以普通 `#[test]` 覆盖，不需要跑起界面。
//!
//! # 为什么要有这一层
//!
//! 值面板的「树」档要能点开 / 收起每一层。把「节点树」和「展开状态」分开之后：
//!
//! - 展开 / 收起**只重算展平**，不重新解析 JSON、不重新建树（选中一个键时算一次）；
//! - 定位一行的身份是 [`JsonTreeNode::path`]（JSONPath），不是「第 N 行」——
//!   折起上面几层不会让下面的行认错自己。
//!
//! # 两道上限（都会如实计入 `omitted`，不假装没有）
//!
//! - [`JSON_TREE_MAX_CHILDREN`]：单个容器最多建这么多成员（zedis 同款）。
//!   十万个元素的数组是「一次 JSONPath 查询」，不是「一屏滚动」。
//! - [`JSON_TREE_MAX_NODES`]：**整棵树的节点总数**上限。这一条 zedis 没有——
//!   它只限每层，500 × 500 的两层嵌套就是 25 万节点，够把一帧拖成幻灯片。
//!   本 crate 的值读取本身就有 [`crate::value`] 的页大小，但 JSON 是一个整体，
//!   只能在建树时把总量掐住。
//!
//! # 两条既定事实（不是本模块的选择，但会影响看到的东西）
//!
//! 1. **成员顺序是文档里的原始顺序**。本 crate 的 `serde_json` **显式打开了
//!    `preserve_order`**（见 `Cargo.toml` 的说明），所以 `Map` 是 `IndexMap`
//!    而不是 `BTreeMap`，成员按**原文书写顺序**出来。JSON 档的**文本形态走的
//!    是同一条路**（[`crate::decode`] 也是 `from_str` → `to_string_pretty`），
//!    所以树档与文本档两边的顺序一致——切档不会让键换位置。
//!
//!    ⚠️ 这个特性**必须显式打开、不能靠特性统一碰巧拿到**：工作区里
//!    `schemars`（经 gpui-base / gpui-component）会传递地打开它，于是
//!    「单跑 `duck-redis`」与「和 `duck-theme` 一起跑」曾经得到**两种顺序**
//!    ——单跑绿、一起跑红。`Cargo.toml` 里那条声明就是为了把这个口子焊死。
//!    [`tests::object_members_keep_document_order`] 是它的守卫。
//! 2. **深度上限来自解析器**。`serde_json` 默认 128 层的解析深度上限
//!    （`remaining_depth`，本工作区未开 `unbounded_depth`），因此
//!    [`build_node`] 的递归不可能比 128 层更深，这里**不需要**再设深度上限；
//!    超深文档在 [`tree_of`] 这一层就按「不是文档」返回 `None`。

use std::collections::HashSet;

use serde_json::{Map, Value};

/// 单个容器最多建多少个成员节点。
pub const JSON_TREE_MAX_CHILDREN: usize = 500;

/// 整棵树最多建多少个节点（含根）。
///
/// 取 2 万：一屏几百行，2 万节点远超「人要看的量」，又能让建树耗时稳定在毫秒级。
/// 超出的部分由父节点计入 `omitted`，界面会明说「还有 N 项未显示」。
pub const JSON_TREE_MAX_NODES: usize = 20_000;

/// 标量预览最多显示多少个字符（超出截断并加省略号）。
pub const JSON_TREE_PREVIEW_CHARS: usize = 80;

/// 一个值在树里的类型标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonNodeKind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

impl JsonNodeKind {
    /// 值 → 类型标签。
    #[must_use]
    pub fn of(value: &Value) -> Self {
        match value {
            Value::Object(_) => Self::Object,
            Value::Array(_) => Self::Array,
            Value::String(_) => Self::String,
            Value::Number(_) => Self::Number,
            Value::Bool(_) => Self::Bool,
            Value::Null => Self::Null,
        }
    }

    /// 是不是容器（能展开）。
    #[must_use]
    pub const fn is_container(self) -> bool {
        matches!(self, Self::Object | Self::Array)
    }

    /// JSON 自己的类型名（英文，非界面文案——界面文案在 `duck-app` 层）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Bool => "boolean",
            Self::Null => "null",
        }
    }
}

/// JSONPath 的一段。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    /// 对象的成员。
    Key(String),
    /// 数组的下标。
    Index(usize),
}

/// 把路径段渲染成 JSONPath 文本。
///
/// 键是标识符时写 `.name`，否则写 `["带引号的键"]`。**这个区分不是美观问题**：
/// 路径是行在展开集合里的身份，`{"a.b": 1}` 与 `{"a": {"b": 1}}` 若都渲染成
/// `$.a.b` 就会互相顶掉展开状态。
#[must_use]
pub fn render_path(segments: &[PathSegment]) -> String {
    let mut out = String::from("$");
    for segment in segments {
        match segment {
            PathSegment::Key(key) if is_identifier(key) => {
                out.push('.');
                out.push_str(key);
            }
            PathSegment::Key(key) => {
                // 复用 serde_json 的字符串转义（引号 / 反斜杠 / 控制字符都能对上）。
                let quoted = Value::String(key.clone()).to_string();
                out.push('[');
                out.push_str(&quoted);
                out.push(']');
            }
            PathSegment::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
        }
    }
    out
}

/// 能否不带引号写进 JSONPath。
fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// 树里的一个节点。
#[derive(Debug, Clone, PartialEq)]
pub struct JsonTreeNode {
    /// 节点的 JSONPath（**身份**，也是展开集合里的键）。
    pub path: String,
    /// 成员名 / `[下标]` / 根节点是 `$`。
    pub name: String,
    /// 类型标签。
    pub kind: JsonNodeKind,
    /// 标量的展示串（字符串带引号并截断）；容器为空。
    pub preview: String,
    /// 容器的成员总数（**含未建节点的部分**）；标量为 0。
    pub len: usize,
    /// 因为两道上限而**没有建出节点**的成员数。
    pub omitted: usize,
    pub children: Vec<JsonTreeNode>,
}

impl JsonTreeNode {
    /// 能不能展开（容器且至少有一个子节点）。
    ///
    /// 空的 `{}` / `[]` 不给展开箭头：点开一个空列表什么也不会发生。
    #[must_use]
    pub fn expandable(&self) -> bool {
        self.kind.is_container() && !self.children.is_empty()
    }
}

/// 展平后的一行。
///
/// 用枚举而不是「一个带 `is_placeholder` 标志的结构体」：占位行**没有身份**
/// （没有 path、没有类型），把这种「字段此时无意义」的状态编码进类型，
/// 渲染侧就不必先判断标志再决定读哪些字段。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonTreeRow {
    /// 一个真实节点。
    Row {
        path: String,
        name: String,
        /// 缩进层级：根为 0。
        depth: usize,
        kind: JsonNodeKind,
        /// 标量预览；容器为空。
        preview: String,
        /// 容器成员总数（含省略的）；标量为 0。
        len: usize,
        /// 省略的成员数（`0` = 全建出来了）。
        omitted: usize,
        /// 能否展开（决定画不画箭头）。
        expandable: bool,
        /// 当前是否展开。
        expanded: bool,
    },
    /// 「还有 N 项未显示」的占位行：挂在建不全的容器下面。
    Omitted {
        /// 缩进层级：比父容器深一层。
        depth: usize,
        count: usize,
    },
}

/// 解析文本为 JSON 文档；不是合法 JSON 就 `None`。
///
/// 空串按「不是文档」处理：一个空值不该在树档里显示成一个空树。
#[must_use]
pub fn parse_document(text: &str) -> Option<Value> {
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// 文本 → 节点树（解析失败返回 `None`）。界面调用的是这一个入口。
#[must_use]
pub fn tree_of(text: &str) -> Option<JsonTreeNode> {
    parse_document(text).map(|doc| json_tree(&doc))
}

/// 文档 → 节点树，根为 `$`。
#[must_use]
pub fn json_tree(doc: &Value) -> JsonTreeNode {
    let mut segments = Vec::new();
    let mut budget = JSON_TREE_MAX_NODES;
    // 预算至少够根节点：`build_node` 的第一次判断因此必然放行。
    build_node(doc, "$".to_string(), &mut segments, &mut budget)
        .unwrap_or_else(|| leaf_of(doc, "$".to_string(), String::from("$")))
}

fn build_node(
    value: &Value,
    name: String,
    segments: &mut Vec<PathSegment>,
    budget: &mut usize,
) -> Option<JsonTreeNode> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let kind = JsonNodeKind::of(value);
    let path = render_path(segments);
    let mut children = Vec::new();
    let len = match value {
        Value::Object(map) => {
            for (key, child) in map.iter().take(JSON_TREE_MAX_CHILDREN) {
                segments.push(PathSegment::Key(key.clone()));
                let built = build_node(child, key.clone(), segments, budget);
                segments.pop();
                match built {
                    Some(node) => children.push(node),
                    // 预算耗尽：本层剩下的都算省略，并让上层也跟着收手。
                    None => break,
                }
            }
            map.len()
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate().take(JSON_TREE_MAX_CHILDREN) {
                segments.push(PathSegment::Index(index));
                let built = build_node(child, format!("[{index}]"), segments, budget);
                segments.pop();
                match built {
                    Some(node) => children.push(node),
                    None => break,
                }
            }
            items.len()
        }
        _ => 0,
    };
    let omitted = len.saturating_sub(children.len());
    Some(JsonTreeNode {
        path,
        name,
        kind,
        preview: preview(value),
        len,
        omitted,
        children,
    })
}

/// 预算耗尽时给根节点的兜底（不至于整个树档空着）。
fn leaf_of(value: &Value, name: String, path: String) -> JsonTreeNode {
    JsonTreeNode {
        path,
        name,
        kind: JsonNodeKind::of(value),
        preview: preview(value),
        len: 0,
        omitted: 0,
        children: Vec::new(),
    }
}

/// 标量的展示串。
fn preview(value: &Value) -> String {
    match value {
        Value::Object(_) | Value::Array(_) => String::new(),
        Value::String(text) => {
            let clipped: String = text.chars().take(JSON_TREE_PREVIEW_CHARS).collect();
            let mut shown = serde_json::to_string(&clipped).unwrap_or_default();
            if clipped.len() < text.len() {
                // 去掉收尾引号，换上省略号再补回来：截断发生在引号**里面**。
                shown.pop();
                shown.push('…');
                shown.push('"');
            }
            shown
        }
        other => other.to_string(),
    }
}

/// 默认展开哪些容器：根，以及根的直接成员。
///
/// 只展开两层是有意的——再多就把树的「折叠」价值抵消了，用户看到的第一眼
/// 应当既能看出文档形状、又不至于一屏塞满几百行。
#[must_use]
pub fn default_expanded(root: &JsonTreeNode) -> HashSet<String> {
    let mut expanded = HashSet::new();
    if root.expandable() {
        expanded.insert(root.path.clone());
        for child in &root.children {
            if child.expandable() {
                expanded.insert(child.path.clone());
            }
        }
    }
    expanded
}

/// 节点树 → 可见的行序列（`expanded` 里的容器才展开）。
///
/// 折叠的行**保留在序列里**（`expanded == false`），只是不带子行：这样界面
/// 点箭头时行位置不会跳，也不需要为了画箭头回头去查树。
#[must_use]
pub fn flatten(root: &JsonTreeNode, expanded: &HashSet<String>) -> Vec<JsonTreeRow> {
    let mut rows = Vec::new();
    push_rows(root, 0, expanded, &mut rows);
    rows
}

fn push_rows(
    node: &JsonTreeNode,
    depth: usize,
    expanded: &HashSet<String>,
    rows: &mut Vec<JsonTreeRow>,
) {
    let open = node.expandable() && expanded.contains(&node.path);
    rows.push(JsonTreeRow::Row {
        path: node.path.clone(),
        name: node.name.clone(),
        depth,
        kind: node.kind,
        preview: node.preview.clone(),
        len: node.len,
        omitted: node.omitted,
        expandable: node.expandable(),
        expanded: open,
    });
    if !open {
        return;
    }
    for child in &node.children {
        push_rows(child, depth + 1, expanded, rows);
    }
    if node.omitted > 0 {
        rows.push(JsonTreeRow::Omitted {
            depth: depth + 1,
            count: node.omitted,
        });
    }
}

/// 把 `expanded` 里的一个路径取反，返回是否**现在**处于展开态。
///
/// 单独做成函数是因为界面在两个地方做同一件事（点箭头、双击行），
/// 两处各写一遍 `insert` / `remove` 迟早会写歪一边。
pub fn toggle_path(expanded: &mut HashSet<String>, path: &str) -> bool {
    if expanded.contains(path) {
        expanded.remove(path);
        false
    } else {
        expanded.insert(path.to_string());
        true
    }
}

/// 展平后的行数（界面分页 / 虚拟列表要用，省得自己 `len()` 一遍 Vec）。
#[must_use]
pub fn row_count(rows: &[JsonTreeRow]) -> usize {
    rows.len()
}

/// 空对象，JSON 键的起始文档。
#[must_use]
pub fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(text: &str) -> Vec<JsonTreeRow> {
        let tree = tree_of(text).expect("应当解析成功");
        let expanded = default_expanded(&tree);
        flatten(&tree, &expanded)
    }

    fn row_paths(rows: &[JsonTreeRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                JsonTreeRow::Row { path, .. } => path.clone(),
                JsonTreeRow::Omitted { .. } => "<omitted>".to_string(),
            })
            .collect()
    }

    #[test]
    fn dotted_key_and_nested_key_get_different_paths() {
        // 这条是路径要做成 JSONPath 而不是「点号拼接」的**唯一理由**。
        let flat = tree_of(r#"{"a.b": 1}"#).expect("解析成功");
        let nested = tree_of(r#"{"a": {"b": 1}}"#).expect("解析成功");
        assert_eq!(flat.children[0].path, r#"$["a.b"]"#);
        assert_eq!(nested.children[0].path, "$.a");
        assert_eq!(nested.children[0].children[0].path, "$.a.b");
        assert_ne!(flat.children[0].path, nested.children[0].children[0].path);
    }

    #[test]
    fn array_indices_and_identifiers_render_as_expected() {
        let tree = tree_of(r#"{"_ok": [1, {"x9": null}], "9bad": true}"#).expect("解析成功");
        let by_name = |name: &str| {
            tree.children
                .iter()
                .find(|node| node.name == name)
                .expect("节点存在")
                .clone()
        };
        let arr = by_name("_ok");
        assert_eq!(arr.path, "$._ok");
        assert_eq!(arr.children[0].path, "$._ok[0]");
        assert_eq!(arr.children[1].name, "[1]");
        assert_eq!(arr.children[1].children[0].path, "$._ok[1].x9");
        assert_eq!(by_name("9bad").path, r#"$["9bad"]"#);
    }

    /// ⚠️ 这条测试同时是 `preserve_order` 的**守卫**：一旦那个特性掉了，
    /// 成员会变成按键排序，这里立刻红（而不是等到「和 `duck-theme` 一起跑」
    /// 才发现两种行为）。
    #[test]
    fn object_members_keep_document_order() {
        // `serde_json` 的 `Map` 在本 crate 是 `IndexMap`（`Cargo.toml` 显式开了
        // `preserve_order`），所以树里的成员顺序是**原文书写顺序**。JSON 档的文本
        // 形态走的是同一条路（`decode_json` 也是 `from_str` → `to_string_pretty`），
        // 因此树档与文本档看到的顺序一致——不会出现「切一下档键就换位置」。
        //
        // 刻意用大小写混排的键：如果是 `BTreeMap`，出来会是 `["C", "a", "b"]`
        // （ASCII 大写排在小写前），与这里期望的原文顺序**不同**，所以这条用例
        // 能真正区分两种实现。
        let tree = tree_of(r#"{"b": 1, "a": 2, "C": 3}"#).expect("解析成功");
        let names: Vec<&str> = tree
            .children
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(names, ["b", "a", "C"], "成员要按原文顺序排列");
    }

    #[test]
    fn scalars_are_previewed_strings_are_quoted_and_clipped() {
        let tree = tree_of(r#"{"s": "hi", "n": 1.5, "b": false, "z": null}"#).expect("解析成功");
        let by_name = |name: &str| {
            tree.children
                .iter()
                .find(|node| node.name == name)
                .expect("节点存在")
                .clone()
        };
        assert_eq!(by_name("s").preview, "\"hi\"");
        assert_eq!(by_name("n").preview, "1.5");
        assert_eq!(by_name("b").preview, "false");
        assert_eq!(by_name("z").preview, "null");
        assert!(by_name("s").kind == JsonNodeKind::String);
        assert!(!by_name("n").expandable());

        let long = "x".repeat(JSON_TREE_PREVIEW_CHARS + 20);
        let text = format!(r#"{{"big": "{long}"}}"#);
        let clipped = tree_of(&text).expect("解析成功").children[0]
            .preview
            .clone();
        // 省略号在引号**里面**：仍是合法的「被截断的 JSON 字符串」形状。
        assert!(
            clipped.starts_with('"') && clipped.ends_with("…\""),
            "{clipped}"
        );
    }

    #[test]
    fn folding_hides_only_descendants() {
        let tree = tree_of(r#"{"a": {"b": {"c": 1}}, "d": 2}"#).expect("解析成功");
        let mut expanded = default_expanded(&tree);
        // 默认展开两层：`$` 与 `$.a` 都在，`$.a.b` 还没展开。
        assert_eq!(
            row_paths(&flatten(&tree, &expanded)),
            ["$", "$.a", "$.a.b", "$.d"]
        );

        // 展开最深一层。
        assert!(toggle_path(&mut expanded, "$.a.b"));
        assert_eq!(
            row_paths(&flatten(&tree, &expanded)),
            ["$", "$.a", "$.a.b", "$.a.b.c", "$.d"]
        );

        // 收起中间一层：`$.a.b` 整枝都不再出现，但 `$.a.b` 自己那行还在。
        assert!(!toggle_path(&mut expanded, "$.a"));
        assert_eq!(row_paths(&flatten(&tree, &expanded)), ["$", "$.a", "$.d"]);
    }

    #[test]
    fn empty_containers_are_not_expandable() {
        let tree = tree_of(r#"{"e": {}, "f": []}"#).expect("解析成功");
        assert!(!tree.children[0].expandable());
        assert!(!tree.children[1].expandable());
        // 展开集合里塞了它也不会有子行。
        let mut expanded = default_expanded(&tree);
        expanded.insert("$.e".to_string());
        assert_eq!(row_paths(&flatten(&tree, &expanded)), ["$", "$.e", "$.f"]);
    }

    #[test]
    fn children_cap_reports_the_remainder() {
        let items: Vec<String> = (0..JSON_TREE_MAX_CHILDREN + 7)
            .map(|index| index.to_string())
            .collect();
        let text = format!("[{}]", items.join(","));
        let tree = tree_of(&text).expect("解析成功");
        assert_eq!(tree.children.len(), JSON_TREE_MAX_CHILDREN);
        assert_eq!(tree.len, JSON_TREE_MAX_CHILDREN + 7);
        assert_eq!(tree.omitted, 7);

        let rows = flatten(&tree, &default_expanded(&tree));
        match rows.last().expect("有行") {
            JsonTreeRow::Omitted { depth, count } => {
                assert_eq!((*depth, *count), (1, 7));
            }
            other => panic!("末行应当是省略占位：{other:?}"),
        }
    }

    #[test]
    fn node_budget_stops_the_tree_and_counts_omitted() {
        // 用**宽**结构而不是深结构来耗尽预算：`serde_json` 自带 128 层解析上限
        // （`remaining_depth: 128`），深结构根本走不到 `tree_of` 就被解析器拒了。
        // 100 × 400 = 4 万节点，稳稳超过 [`JSON_TREE_MAX_NODES`]。
        let inner = |offset: usize| {
            (0..400)
                .map(|index| (offset + index).to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let text = format!(
            "[{}]",
            (0..100)
                .map(|row| format!("[{inner}]", inner = inner(row * 400)))
                .collect::<Vec<_>>()
                .join(",")
        );
        let tree = tree_of(&text).expect("解析成功");

        // 预算**正好**用完：多建一个都不行，少一个说明提前收手了。
        fn count(node: &JsonTreeNode) -> usize {
            1 + node.children.iter().map(count).sum::<usize>()
        }
        assert_eq!(count(&tree), JSON_TREE_MAX_NODES);

        // 从**尾部**截断：第一个数组是完整的，说明不是随机丢节点。
        let first = &tree.children[0];
        assert_eq!(first.len, 400);
        assert_eq!(first.children.len(), 400);
        assert_eq!(first.omitted, 0);

        // 截断处如实报告；根自己也因为「后面的数组没建出来」而记上省略数。
        let last = tree.children.last().expect("至少建出一个数组");
        assert!(last.omitted > 0, "掐断处那一层要报出未建出的成员数");
        assert_eq!(tree.omitted, 100 - tree.children.len());
    }

    #[test]
    fn deep_documents_are_bounded_by_the_parser_not_here() {
        // `tree_of` 不做深度上限：`serde_json` 的解析上限已经先挡了一道，
        // 建树递归因此不可能比 128 层更深（`unbounded_depth` 未开）。
        // 这条测试钉住「超深文档走的是解析失败，而不是把栈撑爆」。
        let mut text = String::from("1");
        for _ in 0..200 {
            text = format!("{{\"n\": {text}}}");
        }
        assert!(
            tree_of(&text).is_none(),
            "超过解析深度上限应当是「不是文档」"
        );
        // 128 层以内正常。
        let mut ok = String::from("1");
        for _ in 0..100 {
            ok = format!("{{\"n\": {ok}}}");
        }
        assert!(tree_of(&ok).is_some());
    }

    #[test]
    fn non_json_text_is_not_a_document() {
        assert!(tree_of("{nope}").is_none());
        assert!(tree_of("").is_none());
        assert!(tree_of("   ").is_none());
        // 顶层标量也是文档（`3` 是合法 JSON）。
        assert!(tree_of("3").is_some());
    }

    #[test]
    fn toggle_path_is_symmetric() {
        let mut expanded = HashSet::new();
        assert!(toggle_path(&mut expanded, "$"));
        assert!(expanded.contains("$"));
        assert!(!toggle_path(&mut expanded, "$"));
        assert!(expanded.is_empty());
    }

    #[test]
    fn row_count_matches_flatten_len() {
        let rows = rows_of(r#"{"a": 1}"#);
        assert_eq!(row_count(&rows), rows.len());
        assert_eq!(row_count(&rows), 2);
    }
}
