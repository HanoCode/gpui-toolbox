//! JSONPath 求值器：对一份 JSON 文档跑一条路径查询，返回命中节点。
//!
//! 纯逻辑：不碰网络、不碰 gpui、零新增依赖——输入 [`serde_json::Value`]，
//! 输出 [`JsonPathHit`] 列表（路径 + 类型 + 预览）。界面的「JSONPath 栏」
//! 把当前值文本解析成文档后调 [`evaluate`]，命中的 `path` 与树行的身份
//! （[`JsonTreeNode::path`]，见 `json_tree.rs`）是**同一种字符串**——同一套
//! [`render_path`] 渲染出来，所以命中结果可以直接在树上定位、展开、高亮。
//!
//! # 支持的语法（Goessner 风格的实用子集）
//!
//! | 写法 | 含义 |
//! |---|---|
//! | `$` | 根（查询必须以它开头） |
//! | `.name` / `['name']` / `["name"]` | 对象成员（带引号形式支持任意键，含 `a.b` 这类） |
//! | `.*` / `[*]` | 通配：对象全部成员 / 数组全部元素 |
//! | `..name` / `..*` / `..[n]` | 递归下降：在自身与全部后代里匹配下一段 |
//! | `[0]` / `[-1]` | 数组下标（负数从尾部数） |
//! | `['a','b']` / `[0,2]` | 并集：按书写顺序逐个匹配 |
//! | `[start:end:step]` | 切片（Python 语义，`step` 默认 1，负数表示倒着走） |
//!
//! 刻意**不做**的是脚本/过滤器表达式（`[?(@.price<10)]`）——那需要表达式
//! 求值器与「当前节点」语义，与树行的静态身份模型对不上；以后要加，从
//! [`Selector`] 加一个变体开始，界面无需改动。
//!
//! # 上限
//!
//! 结果总数封顶 [`JSON_TREE_MAX_NODES`]：查询在理论上可以命中指数级多的
//! 节点（`$..*` 对宽文档），没有上限会把一帧拖死。达到上限就**停止收集**，
//! 调用方拿到的列表长度等于上限时应当提示「结果可能不止这些」。

use serde_json::Value;

use crate::json_tree::{JSON_TREE_MAX_NODES, JsonNodeKind, PathSegment, preview, render_path};

/// 一条已解析的 JSONPath 查询。
///
/// 用 [`JsonPathQuery::parse`] 从文本构造；语法错误会带原因返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPathQuery {
    selectors: Vec<Selector>,
}

/// 查询的一个选择段。
///
/// 「递归下降」建模成 `Recursive(内层)`：先收集自身与全部后代，再对每个
/// 节点套用内层。这样 `..name` / `..*` / `..[n]` 共用同一套简单匹配逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    /// `.name` / `['name']`。
    Key(String),
    /// `.*` / `[*]`。
    Wildcard,
    /// `[n]`（负数从尾部数）。
    Index(i64),
    /// `[a,b,c]`：并集，按书写顺序逐个匹配。
    Union(Vec<Selector>),
    /// `[start:end:step]`（Python 语义；`step` 为 0 是解析期错误）。
    Slice {
        start: Option<i64>,
        end: Option<i64>,
        step: i64,
    },
    /// `..内层`：递归下降。
    Recursive(Box<Selector>),
}

/// 一次命中：路径 + 类型 + 标量预览。
///
/// `path` 与树行的 `JsonTreeNode::path` 同源（[`render_path`]），调用方
/// 可以直接拿它去展开集合里定位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPathHit {
    /// 命中节点的 JSONPath（如 `$.store.book[0].title`）。
    pub path: String,
    /// 节点类型。
    pub kind: JsonNodeKind,
    /// 标量的展示串（字符串带引号并截断）；容器为空——与树行预览同一规则。
    pub preview: String,
}

impl JsonPathQuery {
    /// 解析查询文本；不合法时返回英文原因（内核错误一律英文，界面层再翻译）。
    ///
    /// 查询必须以 `$` 开头——JSONPath 的「根」是语义的一部分，静默接受
    /// `store.book` 这种相对写法会让「它到底从哪开始」变得不可预测。
    ///
    /// # Errors
    /// 语法不合法时返回带原因的错误串。
    pub fn parse(text: &str) -> Result<Self, String> {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0usize;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '$' {
            return Err("JSONPath must start with '$'".to_string());
        }
        i += 1;
        let mut selectors = Vec::new();
        while i < chars.len() {
            match chars[i] {
                '.' => {
                    i += 1;
                    // `..` 递归下降：下一段（键 / 通配 / 括号段）都要包一层。
                    let recursive = i < chars.len() && chars[i] == '.';
                    if recursive {
                        i += 1;
                    }
                    let mut segment = Vec::new();
                    parse_after_dot(&chars, &mut i, &mut segment)?;
                    if recursive {
                        push_recursive(&mut selectors, segment);
                    } else {
                        selectors.extend(segment);
                    }
                }
                '[' => {
                    let mut segment = Vec::new();
                    parse_bracket(&chars, &mut i, &mut segment)?;
                    selectors.extend(segment);
                }
                c if c.is_whitespace() => i += 1,
                other => {
                    return Err(format!("unexpected character {other:?} in JSONPath"));
                }
            }
        }
        Ok(Self { selectors })
    }

    /// 对文档求值，返回按**文档顺序**排列的命中（上限 [`JSON_TREE_MAX_NODES`]）。
    #[must_use]
    pub fn evaluate(&self, doc: &Value) -> Vec<JsonPathHit> {
        evaluate_selectors(&self.selectors, doc)
    }
}

/// 一步到位的便捷入口：解析 + 求值。
///
/// # Errors
/// 查询语法不合法时返回解析错误。
pub fn run(query: &str, doc: &Value) -> Result<Vec<JsonPathHit>, String> {
    Ok(JsonPathQuery::parse(query)?.evaluate(doc))
}

/// 把「递归下降」包到一段选择上。
///
/// 并集 `[a,b]` 前面挂了 `..` 时，递归语义作用于**每个成员**
/// （`$..['a','b']` ≈ `$..a` 与 `$..b` 的拼接），所以拆开逐个包。
fn push_recursive(out: &mut Vec<Selector>, segment: Vec<Selector>) {
    for selector in segment {
        out.push(Selector::Recursive(Box::new(selector)));
    }
}

/// `.` / `..` 之后的部分：`name` / `*` / 紧跟的 `[...]`。
///
/// 解析出的选择段追加进 `out`（`$..['a','b']` 这类「递归 + 并集」由此
/// 自然拆成多段）。
fn parse_after_dot(chars: &[char], i: &mut usize, out: &mut Vec<Selector>) -> Result<(), String> {
    match chars.get(*i) {
        Some('*') => {
            *i += 1;
            out.push(Selector::Wildcard);
            Ok(())
        }
        Some('[') => parse_bracket(chars, i, out),
        Some(_) => {
            let name = read_identifier(chars, i);
            if name.is_empty() {
                return Err(format!("expected a name after '.' at offset {i:?}"));
            }
            out.push(Selector::Key(name));
            Ok(())
        }
        None => Err("JSONPath must not end with '.'".to_string()),
    }
}

/// 读一段裸标识符（`.` 后面的键名），到 `.` / `[` / 空白为止。
fn read_identifier(chars: &[char], i: &mut usize) -> String {
    let mut name = String::new();
    while *i < chars.len() {
        let c = chars[*i];
        if c == '.' || c == '[' || c.is_whitespace() {
            break;
        }
        name.push(c);
        *i += 1;
    }
    name
}

/// 解析一个 `[...]` 段：成员 / 通配 / 下标 / 并集 / 切片。
fn parse_bracket(chars: &[char], i: &mut usize, out: &mut Vec<Selector>) -> Result<(), String> {
    // 调用方保证 chars[*i] == '['。
    *i += 1;
    let mut members: Vec<Selector> = Vec::new();
    loop {
        skip_spaces(chars, i);
        match chars.get(*i) {
            Some(']') => {
                *i += 1;
                break;
            }
            None => return Err("unterminated '[' in JSONPath".to_string()),
            Some('\'') | Some('"') => {
                let key = read_quoted(chars, i)?;
                members.push(Selector::Key(key));
            }
            Some('*') => {
                *i += 1;
                members.push(Selector::Wildcard);
            }
            Some(_) => {
                if chars[*i] == ':' {
                    // 缺省起点：`[:n]` / `[::2]` / `[::-1]`。
                    members.push(parse_slice_rest(chars, i, None)?);
                } else {
                    let number = read_int(chars, i)?;
                    skip_spaces(chars, i);
                    if chars.get(*i) == Some(&':') {
                        let slice = parse_slice_rest(chars, i, Some(number))?;
                        members.push(slice);
                    } else {
                        members.push(Selector::Index(number));
                    }
                }
            }
        }
        skip_spaces(chars, i);
        match chars.get(*i) {
            Some(',') => {
                *i += 1;
            }
            Some(']') => {
                *i += 1;
                break;
            }
            _ => return Err("expected ',' or ']' in JSONPath brackets".to_string()),
        }
    }
    // 单成员直接展开；多成员是并集。
    if members.len() == 1 {
        out.push(members.remove(0));
    } else if !members.is_empty() {
        out.push(Selector::Union(members));
    } else {
        return Err("empty '[]' in JSONPath".to_string());
    }
    Ok(())
}

/// 解析切片剩余部分：进来时当前指向 `:`（缺省起点或已读 `start` 都一样）。
fn parse_slice_rest(chars: &[char], i: &mut usize, start: Option<i64>) -> Result<Selector, String> {
    *i += 1; // 吃掉 ':'
    let end = if chars
        .get(*i)
        .is_some_and(|c| c.is_ascii_digit() || *c == '-')
    {
        Some(read_int(chars, i)?)
    } else {
        None
    };
    skip_spaces(chars, i);
    let mut step = 1i64;
    if chars.get(*i) == Some(&':') {
        *i += 1;
        skip_spaces(chars, i);
        if chars
            .get(*i)
            .is_some_and(|c| c.is_ascii_digit() || *c == '-')
        {
            step = read_int(chars, i)?;
        }
    }
    if step == 0 {
        return Err("slice step must not be zero".to_string());
    }
    Ok(Selector::Slice { start, end, step })
}

/// 读一个带引号的键（单 / 双引号），进来时指向引号。
fn read_quoted(chars: &[char], i: &mut usize) -> Result<String, String> {
    let quote = chars[*i];
    *i += 1;
    let mut key = String::new();
    while let Some(&c) = chars.get(*i) {
        if c == quote {
            *i += 1;
            return Ok(key);
        }
        if c == '\\' {
            *i += 1;
            match chars.get(*i) {
                Some(&escaped) => {
                    key.push(escaped);
                    *i += 1;
                }
                None => return Err("unterminated escape in JSONPath key".to_string()),
            }
            continue;
        }
        key.push(c);
        *i += 1;
    }
    Err("unterminated quoted key in JSONPath".to_string())
}

/// 读一个（可带负号的）整数。
fn read_int(chars: &[char], i: &mut usize) -> Result<i64, String> {
    skip_spaces(chars, i);
    let mut digits = String::new();
    if chars.get(*i) == Some(&'-') {
        digits.push('-');
        *i += 1;
    }
    while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
        digits.push(chars[*i]);
        *i += 1;
    }
    digits
        .parse::<i64>()
        .map_err(|_| format!("invalid number in JSONPath: {digits:?}"))
}

fn skip_spaces(chars: &[char], i: &mut usize) {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
}

/// 逐段求值：当前节点集依次过每个选择段，最后渲染成命中列表。
fn evaluate_selectors(selectors: &[Selector], doc: &Value) -> Vec<JsonPathHit> {
    let mut current: Vec<(Vec<PathSegment>, &Value)> = vec![(Vec::new(), doc)];
    for selector in selectors {
        let mut next: Vec<(Vec<PathSegment>, &Value)> = Vec::new();
        for (segments, value) in &current {
            apply_selector(selector, segments.clone(), value, &mut next);
        }
        current = next;
        if current.len() >= JSON_TREE_MAX_NODES {
            current.truncate(JSON_TREE_MAX_NODES);
            break;
        }
    }
    current
        .into_iter()
        .map(|(segments, value)| JsonPathHit {
            path: render_path(&segments),
            kind: JsonNodeKind::of(value),
            preview: preview(value),
        })
        .collect()
}

/// 把一个选择段套在一个节点上，命中的子节点追加进 `next`。
fn apply_selector<'a>(
    selector: &'a Selector,
    segments: Vec<PathSegment>,
    value: &'a Value,
    next: &mut Vec<(Vec<PathSegment>, &'a Value)>,
) {
    match selector {
        Selector::Key(key) => {
            if let Value::Object(map) = value
                && let Some(child) = map.get(key)
            {
                let mut segments = segments;
                segments.push(PathSegment::Key(key.clone()));
                next.push((segments, child));
            }
        }
        Selector::Wildcard => match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let mut segments = segments.clone();
                    segments.push(PathSegment::Key(key.clone()));
                    next.push((segments, child));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let mut segments = segments.clone();
                    segments.push(PathSegment::Index(index));
                    next.push((segments, child));
                }
            }
            _ => {}
        },
        Selector::Index(index) => {
            if let Value::Array(items) = value
                && let Some(child) = resolve_index(items.len(), *index)
            {
                let mut segments = segments;
                segments.push(PathSegment::Index(child));
                next.push((segments, &items[child]));
            }
        }
        Selector::Union(members) => {
            for member in members {
                apply_selector(member, segments.clone(), value, next);
            }
        }
        Selector::Slice { start, end, step } => {
            if let Value::Array(items) = value {
                for index in slice_indices(items.len(), *start, *end, *step) {
                    let mut segments = segments.clone();
                    segments.push(PathSegment::Index(index));
                    next.push((segments, &items[index]));
                }
            }
        }
        Selector::Recursive(inner) => {
            // 递归下降按**文档序**（DFS：先判子节点命中、再深入它），命中的
            // 是「由父节点经内层选择段可到达」的节点——根自身没有父，永不命中。
            // 直接在遍历现场判断（而不是「先收集候选再逐个套内层」），保证
            // `$..id` 的命中顺序与文档书写顺序一致。
            let mut visited = 0usize;
            walk_recursive(inner, segments, value, next, &mut visited);
        }
    }
}

/// 递归下降的 DFS 遍历：每个子节点先判是否命中内层选择段，再深入它。
fn walk_recursive<'a>(
    inner: &Selector,
    segments: Vec<PathSegment>,
    value: &'a Value,
    next: &mut Vec<(Vec<PathSegment>, &'a Value)>,
    visited: &mut usize,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let mut child_segments = segments.clone();
                child_segments.push(PathSegment::Key(key.clone()));
                if selects(inner, &child_segments, None) {
                    next.push((child_segments.clone(), child));
                }
                *visited += 1;
                if *visited >= JSON_TREE_MAX_NODES {
                    return;
                }
                walk_recursive(inner, child_segments, child, next, visited);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let mut child_segments = segments.clone();
                child_segments.push(PathSegment::Index(index));
                if selects(inner, &child_segments, Some(items.len())) {
                    next.push((child_segments.clone(), child));
                }
                *visited += 1;
                if *visited >= JSON_TREE_MAX_NODES {
                    return;
                }
                walk_recursive(inner, child_segments, child, next, visited);
            }
        }
        _ => {}
    }
}

/// 节点（已知其路径段，数组节点另知容器长度）是否被选择段命中。
fn selects(selector: &Selector, segments: &[PathSegment], len: Option<usize>) -> bool {
    match selector {
        Selector::Key(key) => segments.last() == Some(&PathSegment::Key(key.clone())),
        Selector::Wildcard => true,
        Selector::Index(index) => match (segments.last(), len) {
            (Some(PathSegment::Index(actual)), _) => {
                // 递归下标用父容器长度解析负数；无长度信息时只匹配非负下标。
                len.map_or(*index >= 0 && *actual as i64 == *index, |len| {
                    resolve_index(len, *index) == Some(*actual)
                })
            }
            _ => false,
        },
        Selector::Slice { start, end, step } => match (segments.last(), len) {
            (Some(PathSegment::Index(actual)), Some(len)) => {
                slice_indices(len, *start, *end, *step).contains(actual)
            }
            _ => false,
        },
        Selector::Union(members) => members.iter().any(|member| selects(member, segments, len)),
        // 递归不会嵌套出现（`..` 后面只接简单选择段）；真嵌套时按不命中处理。
        Selector::Recursive(_) => false,
    }
}

/// 数组下标的统一解析：负数从尾部数，越界返回 `None`。
fn resolve_index(len: usize, index: i64) -> Option<usize> {
    let resolved = if index < 0 { len as i64 + index } else { index };
    if resolved < 0 || resolved >= len as i64 {
        None
    } else {
        Some(resolved as usize)
    }
}

/// 切片语义（Python）：负数端点从尾部数，`step` 为负时默认区间反转。
fn slice_indices(len: usize, start: Option<i64>, end: Option<i64>, step: i64) -> Vec<usize> {
    let clamp = |mut v: i64| -> i64 {
        if v < 0 {
            v += len as i64;
        }
        v.clamp(0, len as i64)
    };
    let mut picked = Vec::new();
    if step > 0 {
        let from = start.map_or(0, clamp);
        let to = end.map_or(len as i64, clamp);
        let mut index = from;
        while index < to {
            picked.push(index as usize);
            index += step;
        }
    } else {
        let from = start.map_or(len as i64 - 1, clamp).min(len as i64 - 1);
        let to = end.map_or(-1, clamp);
        let mut index = from;
        while index > to {
            picked.push(index as usize);
            index += step;
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_tree::tree_of;

    fn doc(text: &str) -> Value {
        serde_json::from_str(text).expect("测试文档应当合法")
    }

    fn paths(query: &str, text: &str) -> Vec<String> {
        let hits = run(query, &doc(text)).expect("查询应当合法");
        hits.into_iter().map(|hit| hit.path).collect()
    }

    #[test]
    fn dot_and_bracket_forms_agree() {
        let text = r#"{"store": {"book": ["a", "b"]}}"#;
        assert_eq!(
            paths("$.store.book", text),
            paths("$['store']['book']", text)
        );
        assert_eq!(paths("$.store.book", text), paths("$..book", text));
    }

    #[test]
    fn quoted_keys_support_dots_and_escapes() {
        let text = r#"{"a.b": 1, "q'k": 2}"#;
        assert_eq!(paths(r#"$['a.b']"#, text), [r#"$["a.b"]"#]);
        assert_eq!(paths(r#"$.a.b"#, text), Vec::<String>::new());
        assert_eq!(paths(r#"$["q'k"]"#, text), [r#"$["q'k"]"#]);
    }

    #[test]
    fn wildcard_walks_objects_and_arrays() {
        let text = r#"{"a": 1, "b": [10, 20]}"#;
        assert_eq!(paths("$.*", text), ["$.a", "$.b"]);
        assert_eq!(paths("$.b[*]", text), ["$.b[0]", "$.b[1]"]);
    }

    #[test]
    fn recursive_descent_finds_all_depths() {
        let text = r#"{"a": {"id": 1, "b": {"id": 2}}, "id": 3}"#;
        assert_eq!(paths("$..id", text), ["$.a.id", "$.a.b.id", "$.id"]);
        assert_eq!(paths("$..b.id", text), ["$.a.b.id"]);
        // `..*` 命中每个节点的全部成员（不含根自身）。
        assert_eq!(paths("$..a", text), ["$.a"]);
    }

    #[test]
    fn indexes_support_negative_and_union_and_slice() {
        let text = "[10, 20, 30, 40, 50]";
        assert_eq!(paths("$[0]", text), ["$[0]"]);
        assert_eq!(paths("$[-1]", text), ["$[4]"]);
        assert_eq!(paths("$[4,0]", text), ["$[4]", "$[0]"]);
        assert_eq!(paths("$[1:3]", text), ["$[1]", "$[2]"]);
        assert_eq!(paths("$[::2]", text), ["$[0]", "$[2]", "$[4]"]);
        assert_eq!(
            paths("$[::-1]", text),
            ["$[4]", "$[3]", "$[2]", "$[1]", "$[0]"]
        );
        assert_eq!(paths("$[-2:]", text), ["$[3]", "$[4]"]);
    }

    #[test]
    fn union_of_keys_applies_in_written_order() {
        let text = r#"{"a": 1, "b": 2, "c": 3}"#;
        assert_eq!(paths("$['c','a']", text), ["$.c", "$.a"]);
    }

    #[test]
    fn recursive_union_expands_each_member() {
        // 递归下降里的并集是**一次遍历、按文档序交错命中**（不是两条查询拼接），
        // 界面上看到的顺序与树行顺序一致。
        let text = r#"{"x": {"a": 1, "b": {"a": 2, "b": 3}}, "a": 4}"#;
        assert_eq!(
            paths("$..['a','x']", text),
            ["$.x", "$.x.a", "$.x.b.a", "$.a"]
        );
    }

    #[test]
    fn misses_are_empty_not_errors() {
        let text = r#"{"a": 1}"#;
        assert_eq!(paths("$.nope", text), Vec::<String>::new());
        assert_eq!(paths("$[3]", text), Vec::<String>::new());
        // 标量上取成员也是「未命中」。
        assert_eq!(paths("$.a.b", text), Vec::<String>::new());
    }

    #[test]
    fn hits_carry_kind_and_preview() {
        let text = r#"{"a": "hi", "b": {"c": 1}}"#;
        let hits = run("$.a", &doc(text)).expect("合法");
        assert_eq!(hits[0].kind, JsonNodeKind::String);
        assert_eq!(hits[0].preview, "\"hi\"");
        let hits = run("$.b", &doc(text)).expect("合法");
        assert_eq!(hits[0].kind, JsonNodeKind::Object);
        assert!(hits[0].preview.is_empty());
    }

    /// 命中路径与树行身份同源：同一段路径在**全展开**的树里找得到。
    #[test]
    fn hit_paths_match_tree_row_paths() {
        let text = r#"{"a": {"b": [1, 2]}}"#;
        let hits = run("$..b[*]", &doc(text)).expect("合法");
        let tree = tree_of(text).expect("建树成功");
        // 展开全部容器：命中路径可能藏在折叠的层下面。
        let mut expanded = std::collections::HashSet::new();
        let mut stack = vec![&tree];
        while let Some(node) = stack.pop() {
            if node.expandable() {
                expanded.insert(node.path.clone());
            }
            stack.extend(node.children.iter());
        }
        let row_paths: Vec<String> = crate::json_tree::flatten(&tree, &expanded)
            .into_iter()
            .filter_map(|row| match row {
                crate::json_tree::JsonTreeRow::Row { path, .. } => Some(path),
                crate::json_tree::JsonTreeRow::Omitted { .. } => None,
            })
            .collect();
        for hit in &hits {
            assert!(
                row_paths.contains(&hit.path),
                "命中路径 {} 应当是树里的一行",
                hit.path
            );
        }
    }

    #[test]
    fn syntax_errors_are_reported() {
        let value = doc("1");
        for query in [
            "",
            "store.book",
            "$.",
            "$[",
            "$['a",
            "$[]",
            "$[1:0:0]",
            "$a.b",
            "$..",
        ] {
            assert!(run(query, &value).is_err(), "{query:?} 应当报语法错误");
        }
    }

    #[test]
    fn result_count_is_capped() {
        // 600 个元素 + 上限 2 万：单条查询到不了上限，但 `$..*` 会一路收满。
        let text = format!(
            "[{}]",
            (0..600)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let hits = run("$[*]", &doc(&text)).expect("合法");
        assert_eq!(hits.len(), 600);
        // 全文档穷举也稳稳在上限内（600 节点 << 20_000）。
        let hits = run("$..*", &doc(&text)).expect("合法");
        assert!(hits.len() <= JSON_TREE_MAX_NODES);
    }

    #[test]
    fn whitespace_tolerant() {
        let text = r#"{"a b": [1, 2]}"#;
        assert_eq!(paths("$[ 'a b' ][ 1 ]", text), [r#"$["a b"][1]"#]);
    }

    #[test]
    fn top_level_scalar_document() {
        let value = doc("42");
        let hits = run("$", &value).expect("合法");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "$");
        assert_eq!(hits[0].preview, "42");
    }
}
