//! `tera` 的**极小子集**渲染器：只够渲染 `templates/*.java.tera`。
//!
//! 参考仓用 `tera` 渲染两个 Java 模板，并把同一个引擎开给用户自定义模板（存 SQLite）。
//! 本仓不迁自定义模板（见 crate 文档），所以这里只实现模板里真正出现的语法：
//!
//! | 语法 | 出现处 |
//! | --- | --- |
//! | `{{ a.b }}` | 变量（路径取不到时渲染成空串，与 tera 一致） |
//! | `{% if EXPR %}` / `{% else %}` / `{% endif %}` | 字段是否数组、字段表是否为空 |
//! | `{% for x in a.b %}` / `{% endfor %}` | 请求 / 应答字段、数组子字段 |
//! | `EXPR` 的三种写法 | `x is defined`、`x \| length > 0`、裸 `x`（真值）、`and` 连接 |
//!
//! **不**实现：过滤器（除 `length`）、`{%- -%}` 空白控制、继承 / 宏 / 包含、表达式运算。
//! 遇到不认识的标签**报错**而不是静默吞掉 —— 模板是复制来的，哪天有人换成真 tera
//! 语法，这里必须当场说出来，而不是吐出一份 `{% for %}` 还在里面的「代码」。
//!
//! ## 空白处理
//!
//! 逐字符照抄：标签本身**不产生输出**，标签之间的文本原样保留。这正是 tera 的默认
//! 行为（`{% if %}` 独占一行 ⇒ 输出里留一行空行），所以产物与参考仓逐字节一致。

use std::collections::BTreeMap;

/// 模板上下文里的一个值。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Bool(bool),
    List(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    fn at(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(map) => map.get(key),
            _ => None,
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Self::Text(text) => !text.is_empty(),
            Self::Bool(flag) => *flag,
            Self::List(items) => !items.is_empty(),
            Self::Object(map) => !map.is_empty(),
        }
    }

    fn length(&self) -> usize {
        match self {
            Self::Text(text) => text.chars().count(),
            Self::List(items) => items.len(),
            Self::Object(map) => map.len(),
            Self::Bool(_) => 0,
        }
    }

    fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Bool(flag) => flag.to_string(),
            Self::List(_) | Self::Object(_) => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Cond {
    All(Vec<Cond>),
    Defined(String),
    LengthGreater(String, usize),
    Truthy(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var(String),
    If {
        cond: Cond,
        yes: Vec<Node>,
        no: Vec<Node>,
    },
    For {
        var: String,
        path: String,
        body: Vec<Node>,
    },
}

/// 渲染一份模板。错误只来自「模板语法不认识」。
pub fn render<'a>(template: &str, context: &'a Value) -> Result<String, String> {
    let mut pos = 0usize;
    let (nodes, stop) = parse_nodes(template, &mut pos, &[])?;
    if !stop.is_empty() {
        return Err(format!("意外的块结束标签: {}", stop));
    }
    let mut out = String::with_capacity(template.len() + 256);
    let mut scope: Vec<(&str, &'a Value)> = Vec::new();

    render_nodes(&nodes, context, &mut scope, &mut out);
    Ok(out)
}

/// 找到下一个 `{{` / `{%`（返回起点与种类）。
fn next_tag(source: &str, from: usize) -> Option<(usize, bool)> {
    let rest = &source[from..];
    let var = rest.find("{{").map(|index| (index, true));
    let block = rest.find("{%").map(|index| (index, false));
    match (var, block) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_nodes(
    source: &str,
    pos: &mut usize,
    stop_at: &[&str],
) -> Result<(Vec<Node>, String), String> {
    let mut nodes = Vec::new();
    loop {
        let Some((offset, is_var)) = next_tag(source, *pos) else {
            if *pos < source.len() {
                nodes.push(Node::Text(source[*pos..].to_string()));
            }
            *pos = source.len();
            if stop_at.is_empty() {
                return Ok((nodes, String::new()));
            }
            return Err(format!("块没有结束（缺少 {}）", stop_at.join(" / ")));
        };
        let start = *pos + offset;
        if start > *pos {
            nodes.push(Node::Text(source[*pos..start].to_string()));
        }
        let (open, close) = if is_var { ("{{", "}}") } else { ("{%", "%}") };
        let body_start = start + open.len();
        let Some(body_end) = source[body_start..].find(close) else {
            return Err(format!("标签没有闭合: {}", open));
        };
        let body = source[body_start..body_start + body_end].trim().to_string();
        *pos = body_start + body_end + close.len();

        if is_var {
            if body.is_empty() {
                return Err("空的变量标签".to_string());
            }
            nodes.push(Node::Var(body));
            continue;
        }

        let keyword = body
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        if stop_at.contains(&keyword.as_str()) {
            return Ok((nodes, keyword));
        }
        match keyword.as_str() {
            "if" => {
                let cond = parse_cond(body["if".len()..].trim())?;
                let (yes, stop) = parse_nodes(source, pos, &["else", "endif"])?;
                let no = if stop == "else" {
                    let (no, stop) = parse_nodes(source, pos, &["endif"])?;
                    if stop != "endif" {
                        return Err("if 块缺少 endif".to_string());
                    }
                    no
                } else {
                    Vec::new()
                };
                nodes.push(Node::If { cond, yes, no });
            }
            "for" => {
                let (var, path) = parse_for(body["for".len()..].trim())?;
                let (body_nodes, stop) = parse_nodes(source, pos, &["endfor"])?;
                if stop != "endfor" {
                    return Err("for 块缺少 endfor".to_string());
                }
                nodes.push(Node::For {
                    var,
                    path,
                    body: body_nodes,
                });
            }
            other => return Err(format!("不支持的模板标签: {}", other)),
        }
    }
}

fn parse_for(expr: &str) -> Result<(String, String), String> {
    let Some((var, rest)) = expr.split_once(" in ") else {
        return Err(format!("for 写法不对: {}", expr));
    };
    let var = var.trim();
    let path = rest.trim();
    if var.is_empty() || path.is_empty() {
        return Err(format!("for 写法不对: {}", expr));
    }
    Ok((var.to_string(), path.to_string()))
}

fn parse_cond(expr: &str) -> Result<Cond, String> {
    let clauses: Vec<Cond> = expr
        .split(" and ")
        .map(parse_clause)
        .collect::<Result<_, _>>()?;
    Ok(match clauses.len() {
        0 => return Err("空的 if 条件".to_string()),
        1 => clauses.into_iter().next().unwrap_or(Cond::All(Vec::new())),
        _ => Cond::All(clauses),
    })
}

fn parse_clause(clause: &str) -> Result<Cond, String> {
    let clause = clause.trim();
    if let Some(path) = clause.strip_suffix(" is defined") {
        return Ok(Cond::Defined(path.trim().to_string()));
    }
    if let Some((path, rest)) = clause.split_once("| length") {
        if let Some(number) = rest.trim().strip_prefix('>') {
            let count = number
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("条件里的数字不对: {}", clause))?;
            return Ok(Cond::LengthGreater(path.trim().to_string(), count));
        }
    }
    if clause.is_empty() {
        return Err("空的 if 条件".to_string());
    }
    Ok(Cond::Truthy(clause.to_string()))
}

fn lookup<'a>(path: &str, context: &'a Value, scope: &[(&str, &'a Value)]) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let head = parts.next()?;
    let mut current = match scope.iter().rev().find(|(name, _)| *name == head) {
        Some((_, value)) => *value,
        None => context.at(head)?,
    };
    for part in parts {
        current = current.at(part)?;
    }
    Some(current)
}

fn holds<'a>(cond: &Cond, context: &'a Value, scope: &[(&str, &'a Value)]) -> bool {
    match cond {
        Cond::All(clauses) => clauses.iter().all(|clause| holds(clause, context, scope)),
        Cond::Defined(path) => lookup(path, context, scope).is_some(),
        Cond::LengthGreater(path, count) => {
            lookup(path, context, scope).is_some_and(|value| value.length() > *count)
        }
        Cond::Truthy(path) => lookup(path, context, scope).is_some_and(Value::truthy),
    }
}

fn render_nodes<'a, 'b>(
    nodes: &'b [Node],
    context: &'a Value,
    scope: &mut Vec<(&'b str, &'a Value)>,
    out: &mut String,
) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Var(path) => {
                if let Some(value) = lookup(path, context, scope) {
                    out.push_str(&value.as_text());
                }
            }
            Node::If { cond, yes, no } => {
                if holds(cond, context, scope) {
                    render_nodes(yes, context, scope, out);
                } else {
                    render_nodes(no, context, scope, out);
                }
            }
            Node::For { var, path, body } => {
                let Some(Value::List(items)) = lookup(path, context, scope) else {
                    continue;
                };
                for item in items {
                    scope.push((var.as_str(), item));
                    render_nodes(body, context, scope, out);
                    scope.pop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Text(value.to_string())
    }

    fn object(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn substitutes_paths_and_keeps_missing_empty() {
        let context = object(&[("a", object(&[("b", text("x"))]))]);
        assert_eq!(render("{{ a.b }}!", &context).unwrap(), "x!");
        assert_eq!(render("[{{ a.zzz }}]", &context).unwrap(), "[]");
    }

    #[test]
    fn if_else_and_length_guard() {
        let empty = object(&[("list", Value::List(Vec::new()))]);
        let template = "{% if list is defined and list | length > 0 %}has{% else %}none{% endif %}";
        assert_eq!(render(template, &empty).unwrap(), "none");
        let full = object(&[("list", Value::List(vec![text("a")]))]);
        assert_eq!(render(template, &full).unwrap(), "has");
    }

    #[test]
    fn nested_loop_keeps_order_and_scope() {
        let context = object(&[(
            "rows",
            Value::List(vec![
                object(&[
                    ("name", text("A")),
                    ("subs", Value::List(vec![text("p"), text("q")])),
                ]),
                object(&[("name", text("B")), ("subs", Value::List(Vec::new()))]),
            ]),
        )]);
        let template = "{% for row in rows %}{{ row.name }}({% for sub in row.subs %}{{ sub }}{% endfor %}){% endfor %}";
        assert_eq!(render(template, &context).unwrap(), "A(pq)B()");
    }

    #[test]
    fn tags_leave_the_surrounding_whitespace_alone() {
        // tera 默认不做空白控制：`{% if %}` 独占一行 ⇒ 产物里留一行空行。
        let context = object(&[("ok", Value::Bool(true))]);
        let template = "head\n{% if ok %}\nbody\n{% endif %}\ntail\n";
        assert_eq!(
            render(template, &context).unwrap(),
            "head\n\nbody\n\ntail\n"
        );
    }

    #[test]
    fn unknown_tags_are_reported_not_swallowed() {
        let context = object(&[]);
        let error = render("{% macro x %}", &context).unwrap_err();
        assert!(error.contains("macro"), "{error}");
        let error = render("{% if ok %}", &context).unwrap_err();
        assert!(error.contains("endif"), "{error}");
        let error = render("{{ a ", &context).unwrap_err();
        assert!(error.contains("闭合"), "{error}");
    }
}
