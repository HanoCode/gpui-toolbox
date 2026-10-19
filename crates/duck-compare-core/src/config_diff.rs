//! 配置比对（参考仓 `DirCompare.vue` 的 **config 会话** + `lib/transforms/configTransforms.ts`）。
//!
//! 参考页的做法是「先归一、再拍平、再按键比」：三路输入（properties / YAML / JSON）
//! 全部先转成 JSON，用 `.` 把嵌套拍平成一层键，然后按键取并集，
//! 逐键判 `same / modified / left_only / right_only` 四档。
//!
//! ## 与参考实现的两处**有意**偏离（都是为了「别算错」）
//!
//! 1. **YAML 认缩进**。参考仓的 `yamlToJson` 是逐行的正则匹配，缩进完全没用上：
//!    `server:\n  port: 8080` 和 `client:\n  port: 9090` 会被压成同一个键 `port`，
//!    后写的覆盖前写的——两份只要有一处不同就会误判成「一样」。这里按缩进建树。
//! 2. **`[section]` 参与键路径**。参考仓 `propertiesToJson` 直接跳过 section 行，
//!    于是 `[db]` 段里的 `url` 和 `[mq]` 段里的 `url` 又是同一个键。这里拼成
//!    `db.url` / `mq.url`（同一份文件的 `propertiesToYaml` 本来就这么干，两处
//!    方向相反才是 bug）。
//!
//! 其余口径照抄：键排序取并集、值为 `null` 记空串、数组按 `a,b` 串接（JS 的
//! `String(array)` 语义）、单侧缺失时另一侧给 `None`。
//!
//! 不支持的（参考仓也不支持，这里只是把「不支持」写成明确行为）：YAML 的锚点 /
//! 引用 / 多文档、properties 的续行（行尾 `\`）。遇到就按字面值处理。

use std::collections::BTreeMap;

use serde_json::Value;

/// 参考仓 `detectSubFormat` 的四档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Properties,
    /// 带 `[section]` 的 properties（`.ini` / 老式 `.conf`）。
    Ini,
    Yaml,
    Json,
}

impl ConfigFormat {
    /// 界面上给的那枚格式徽章（参考页 `configLeftFormat` 走 `toUpperCase()`）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Properties => "PROPERTIES",
            Self::Ini => "INI",
            Self::Yaml => "YAML",
            Self::Json => "JSON",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Same,
    Modified,
    LeftOnly,
    RightOnly,
}

impl Status {
    /// 与目录比对同口径的 `kind` 串，渲染层据此共用一份标记表。
    pub fn kind(self) -> &'static str {
        match self {
            Self::Same => "equal",
            Self::Modified => "modified",
            Self::LeftOnly => "removed",
            Self::RightOnly => "added",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub left: Option<String>,
    pub right: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub same: u32,
    pub modified: u32,
    pub left_only: u32,
    pub right_only: u32,
}

impl Stats {
    /// 「两侧都有但值不同」+ 单侧缺失 = 差异条数。
    pub fn differences(&self) -> u32 {
        self.modified + self.left_only + self.right_only
    }
}

#[derive(Debug, Clone)]
pub struct ConfigDiff {
    pub entries: Vec<Entry>,
    pub stats: Stats,
    pub left_format: ConfigFormat,
    pub right_format: ConfigFormat,
}

// ── 格式识别 ────────────────────────────────────────────────────────────

/// 参考仓 `detectSubFormat` 的判定顺序，**顺序照抄、判据收紧**。
///
/// 与参考实现的差别只在 YAML 那一档：参考仓要求「有效行里过半是 `key: value`」，
/// 于是 `server:\n  port: 8080` 这种「开一层、里面只有一个键」的典型 Spring 配置
/// 会被判成 properties，一个键都读出来（两侧都空 ⇒ 报「完全一致」）。这里改成
/// 「有 `k = v` 就是 properties；没有等号、但有 `key:` 或 `key: value` 就是 YAML」。
pub fn detect_format(text: &str) -> ConfigFormat {
    let trimmed = text.trim();
    // 花括号开头只可能是 JSON；写坏了也归 JSON——`parse` 会报出具体错误，
    // 比静默当 properties（一个键都读不到）强。
    if trimmed.starts_with('{') {
        return ConfigFormat::Json;
    }
    // 方括号要真能解析才算 JSON：`[db]` 是 ini 的段，不能误判。
    if trimmed.starts_with('[') && serde_json::from_str::<Value>(trimmed).is_ok() {
        return ConfigFormat::Json;
    }

    let lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();

    let has_section = lines.iter().any(|line| is_section(line));
    let has_assignment = lines.iter().any(|line| is_assignment(line));
    let has_colon_pair = lines.iter().any(|line| is_yaml_pair(line));
    let has_nesting = lines.iter().any(|line| is_nesting_key(line));

    if has_section && has_assignment {
        return ConfigFormat::Ini;
    }
    if has_assignment {
        return ConfigFormat::Properties;
    }
    if has_section {
        return ConfigFormat::Ini;
    }
    if has_colon_pair || has_nesting {
        return ConfigFormat::Yaml;
    }
    ConfigFormat::Properties
}

/// `key:`（开一层）——`server:` 这种只有键没有值的行。
fn is_nesting_key(line: &str) -> bool {
    match line.strip_suffix(':') {
        Some(key) => {
            let key = key.trim();
            !key.is_empty()
                && !key.contains(' ')
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '"')
        }
        None => false,
    }
}

fn is_section(line: &str) -> bool {
    line.len() >= 2 && line.starts_with('[') && line.ends_with(']') && !line.contains('=')
}

/// `k = v`（等号出现在冒号之前才算「赋值」——`url: http://x` 里的冒号在等号前）。
fn is_assignment(line: &str) -> bool {
    match (line.find('='), line.find(':')) {
        (Some(eq), Some(colon)) => eq < colon,
        (Some(_), None) => true,
        _ => false,
    }
}

/// `key: value` 形态（值必须非空，与参考仓 `/^[\w.-]+\s*:\s+/` 一致——
/// 所以 `server:` 这种「开一个 nesting」的行本身不算 yaml 证据）。
fn is_yaml_pair(line: &str) -> bool {
    match line.split_once(':') {
        Some((key, value)) => {
            !key.trim().is_empty()
                && !key.contains('=')
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_' || c == '"')
                && value.starts_with(' ')
        }
        None => false,
    }
}

// ── 解析 + 拍平 ─────────────────────────────────────────────────────────

/// 把任意一路输入解析成「一层点分键 → 字符串值」。
pub fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    match detect_format(text) {
        ConfigFormat::Json => {
            // 占位符写成 `{}` 而不是 `{e}`：i18n 门禁按源码字面量登记，
            // 内联捕获的 `{e}` 与译文键 `JSON 解析失败: {}` 对不上。
            let value: Value = serde_json::from_str(text.trim())
                .map_err(|e| format!("JSON 解析失败: {}", e))?;
            let mut flat = BTreeMap::new();
            flatten(&value, "", &mut flat);
            Ok(flat)
        }
        ConfigFormat::Yaml => {
            let value = parse_yaml(text);
            let mut flat = BTreeMap::new();
            flatten(&value, "", &mut flat);
            Ok(flat)
        }
        ConfigFormat::Properties | ConfigFormat::Ini => Ok(parse_properties(text)),
    }
}

fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(child, &path, out);
            }
        }
        Value::Array(items) => {
            // JS 的 `String([1,2])` 就是 `"1,2"`——参考仓拍平时直接 String(v)，
            // 这里保持同一形态，免得两边口径不同。
            let joined = items.iter().map(scalar_text).collect::<Vec<_>>().join(",");
            out.insert(prefix.to_string(), joined);
        }
        other => {
            out.insert(prefix.to_string(), scalar_text(other));
        }
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// `k = v` / `[section]`，支持 `.` 分层的键（`a.b.c=1` 原样作为键，
/// 与「先建树再拍平」等价）。
fn parse_properties(text: &str) -> BTreeMap<String, String> {
    let mut flat = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if is_section(line) {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let path = if section.is_empty() {
                key.to_string()
            } else {
                format!("{section}.{key}")
            };
            flat.insert(path, value.trim().to_string());
        }
    }
    flat
}

/// YAML 子集：缩进建树 + 标量 / 列表 / 块标量。
///
/// 只认 `key: value`、`key:`（开子层）、`- item`、`|` / `>` 块标量这几样；
/// 锚点、多文档、flow 集合（`{a: 1}` 一行写完）都不认，遇到按字面值处理。
fn parse_yaml(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    parse_block(&lines, &mut index, 0)
}

/// 跳过空行 / 注释 / 文档分隔符，返回**第一个有效行**的（下标, 缩进, 内容）。
fn peek<'a>(lines: &[&'a str], from: usize) -> Option<(usize, i64, &'a str)> {
    for (offset, raw) in lines.iter().enumerate().skip(from) {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        return Some((offset, indent_of(raw), trimmed));
    }
    None
}

/// 从 `index` 起解析一个块（对象或列表）。`min_indent` 是这一块的最低缩进。
fn parse_block(lines: &[&str], index: &mut usize, min_indent: i64) -> Value {
    match peek(lines, *index) {
        Some((_, indent, trimmed)) if indent >= min_indent && is_list_item(trimmed) => {
            parse_list(lines, index, indent)
        }
        _ => parse_map(lines, index, min_indent),
    }
}

fn parse_map(lines: &[&str], index: &mut usize, min_indent: i64) -> Value {
    let mut map = serde_json::Map::new();
    while let Some((position, indent, trimmed)) = peek(lines, *index) {
        if indent < min_indent || is_list_item(trimmed) {
            break;
        }
        *index = position + 1;
        let Some((key, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let key = unquote(key.trim());
        if key.is_empty() {
            continue;
        }
        let rest = rest.trim();

        if rest.is_empty() {
            // 下一行更深就进这个子块，否则是空值（`key:` 后面什么都没有）。
            match peek(lines, *index) {
                Some((_, next_indent, next)) if next_indent > indent || is_list_item(next) => {
                    let child = parse_block(lines, index, next_indent.max(indent + 1));
                    map.insert(key, child);
                }
                _ => {
                    map.insert(key, Value::Null);
                }
            }
            continue;
        }

        if rest == "|" || rest == ">" || rest.starts_with("|-") || rest.starts_with(">-") {
            map.insert(key, Value::String(block_scalar(lines, index, indent)));
            continue;
        }
        if rest.starts_with('[') {
            map.insert(key, inline_list(rest));
            continue;
        }
        map.insert(key, scalar(rest));
    }
    Value::Object(map)
}

fn parse_list(lines: &[&str], index: &mut usize, indent: i64) -> Value {
    let mut items = Vec::new();
    while let Some((position, item_indent, trimmed)) = peek(lines, *index) {
        if item_indent != indent || !is_list_item(trimmed) {
            break;
        }
        *index = position + 1;
        let item = trimmed[1..].trim();
        if item.is_empty() {
            // `-` 后面跟一个子块（对象 / 列表）。
            match peek(lines, *index) {
                Some((_, next_indent, _)) if next_indent > item_indent => {
                    items.push(parse_block(lines, index, next_indent));
                }
                _ => items.push(Value::Null),
            }
            continue;
        }
        // `- key: value` 一行起一个对象：只看冒号后面的标量，够用且不会算错。
        match item.split_once(": ") {
            Some((key, value)) if !key.contains(' ') => {
                let mut entry = serde_json::Map::new();
                entry.insert(unquote(key.trim()), scalar(value.trim()));
                items.push(Value::Object(entry));
            }
            _ => items.push(scalar(&unquote(item))),
        }
    }
    Value::Array(items)
}

/// `|` / `>` 块标量：把后面所有更深（或空行）的内容原样收进来。
fn block_scalar(lines: &[&str], index: &mut usize, indent: i64) -> String {
    let mut buffer: Vec<String> = Vec::new();
    while *index < lines.len() {
        let raw = lines[*index];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            buffer.push(String::new());
            *index += 1;
            continue;
        }
        if indent_of(raw) <= indent {
            break;
        }
        buffer.push(trimmed.to_string());
        *index += 1;
    }
    while buffer.last().is_some_and(|line| line.is_empty()) {
        buffer.pop();
    }
    buffer.join("\n")
}

fn inline_list(rest: &str) -> Value {
    let inner = rest
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches(',');
    Value::Array(
        inner
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(scalar)
            .collect(),
    )
}

fn is_list_item(trimmed: &str) -> bool {
    trimmed == "-" || trimmed.starts_with("- ")
}

fn indent_of(line: &str) -> i64 {
    let mut width = 0;
    for ch in line.chars() {
        match ch {
            ' ' => width += 1,
            '\t' => width += 2,
            _ => break,
        }
    }
    width
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')))
    {
        return text[1..text.len() - 1].to_string();
    }
    text.to_string()
}

/// 标量：`null` / 布尔 / 整数 / 小数按字面转，带引号的去引号，其余原样。
fn scalar(text: &str) -> Value {
    let text = text.trim();
    match text {
        "null" | "~" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        _ => {
            if let Ok(number) = text.parse::<i64>() {
                return Value::Number(number.into());
            }
            // 不写 `if let Ok(x) = a && let Some(y) = b`：let-chains 要 1.88，
            // 本仓 MSRV 是 1.85，链式判断过不了编译。
            let decimal = text.parse::<f64>().ok().and_then(serde_json::Number::from_f64);
            if let Some(number) = decimal {
                return Value::Number(number);
            }
            Value::String(unquote(text))
        }
    }
}

// ── 比对 ────────────────────────────────────────────────────────────────

/// 两侧各解析一遍，按键取并集逐键判档。
pub fn compare(left: &str, right: &str) -> Result<ConfigDiff, String> {
    let left_format = detect_format(left);
    let right_format = detect_format(right);
    let left_map = parse(left)?;
    let right_map = parse(right)?;

    let mut keys: Vec<&String> = left_map.keys().collect();
    for key in right_map.keys() {
        if !left_map.contains_key(key) {
            keys.push(key);
        }
    }
    keys.sort();

    let mut entries = Vec::with_capacity(keys.len());
    let mut stats = Stats::default();
    for key in keys {
        let lv = left_map.get(key);
        let rv = right_map.get(key);
        let status = match (lv, rv) {
            (Some(a), Some(b)) if a == b => {
                stats.same += 1;
                Status::Same
            }
            (Some(_), Some(_)) => {
                stats.modified += 1;
                Status::Modified
            }
            (Some(_), None) => {
                stats.left_only += 1;
                Status::LeftOnly
            }
            _ => {
                stats.right_only += 1;
                Status::RightOnly
            }
        };
        entries.push(Entry {
            key: key.clone(),
            left: lv.cloned(),
            right: rv.cloned(),
            status,
        });
    }

    Ok(ConfigDiff {
        entries,
        stats,
        left_format,
        right_format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(text: &str) -> BTreeMap<String, String> {
        parse(text).expect("解析应当成功")
    }

    #[test]
    fn detects_four_formats() {
        assert_eq!(detect_format(r#"{"a": 1}"#), ConfigFormat::Json);
        assert_eq!(detect_format("a=1\nb=2"), ConfigFormat::Properties);
        assert_eq!(detect_format("[db]\nurl=x"), ConfigFormat::Ini);
        assert_eq!(detect_format("server:\n  port: 8080"), ConfigFormat::Yaml);
        // 冒号在等号后头（`url: http://x` 的注释风格）不算 YAML 证据。
        assert_eq!(detect_format("url=http://x"), ConfigFormat::Properties);
    }

    #[test]
    fn properties_keep_dotted_keys_and_sections() {
        // `mq.url` 写在 `[db]` 之前，才落在根层；写在段里会变成 `db.mq.url`。
        let flat = map_of("mq.url=y\n[db]\nurl=jdbc:mysql://x\ntimeout = 30\n\n# 注释");
        assert_eq!(
            flat.get("db.url").map(String::as_str),
            Some("jdbc:mysql://x")
        );
        assert_eq!(flat.get("db.timeout").map(String::as_str), Some("30"));
        assert_eq!(flat.get("mq.url").map(String::as_str), Some("y"));
    }

    /// 同样的键名出现在两个 section 里不能并成一个（参考仓会并）。
    #[test]
    fn sections_do_not_collide() {
        let flat = map_of("[db]\nurl=a\n[mq]\nurl=b");
        assert_eq!(flat.get("db.url").map(String::as_str), Some("a"));
        assert_eq!(flat.get("mq.url").map(String::as_str), Some("b"));
    }

    /// YAML 认缩进：两段同名的子键必须是两个键（参考仓会压成一个）。
    #[test]
    fn yaml_nesting_follows_indent() {
        let flat = map_of("server:\n  port: 8080\n  host: 0.0.0.0\nclient:\n  port: 9090");
        assert_eq!(flat.get("server.port").map(String::as_str), Some("8080"));
        assert_eq!(flat.get("server.host").map(String::as_str), Some("0.0.0.0"));
        assert_eq!(flat.get("client.port").map(String::as_str), Some("9090"));
    }

    #[test]
    fn yaml_lists_and_block_scalars() {
        let flat = map_of("hosts:\n  - a\n  - b\nnote: |\n  line1\n  line2");
        assert_eq!(flat.get("hosts").map(String::as_str), Some("a,b"));
        assert_eq!(flat.get("note").map(String::as_str), Some("line1\nline2"));
    }

    #[test]
    fn json_nesting_flattens_to_dots() {
        let flat = map_of(r#"{"a": {"b": [1, 2]}, "c": null, "d": true}"#);
        assert_eq!(flat.get("a.b").map(String::as_str), Some("1,2"));
        assert_eq!(flat.get("c").map(String::as_str), Some(""));
        assert_eq!(flat.get("d").map(String::as_str), Some("true"));
    }

    /// 四种档位各一条，另加「两侧都缺」不该出现。
    #[test]
    fn compare_reports_four_statuses() {
        let diff = compare(
            "same=1\nchanged=old\nleft=only-a",
            "same=1\nchanged=new\nright=only-b",
        )
        .expect("比对应当成功");
        let status_of = |key: &str| {
            diff.entries
                .iter()
                .find(|entry| entry.key == key)
                .map(|entry| entry.status)
        };
        assert_eq!(status_of("same"), Some(Status::Same));
        assert_eq!(status_of("changed"), Some(Status::Modified));
        assert_eq!(status_of("left"), Some(Status::LeftOnly));
        assert_eq!(status_of("right"), Some(Status::RightOnly));
        assert_eq!(diff.stats.same, 1);
        assert_eq!(diff.stats.differences(), 3);
        // 键按字典序排，界面不必再排一次。
        let keys: Vec<&str> = diff.entries.iter().map(|e| e.key.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    /// 同一份配置换个写法（properties vs YAML）应当判「全等」——
    /// 这正是「先归一」的意义。
    #[test]
    fn properties_and_yaml_with_same_content_compare_equal() {
        let diff = compare(
            "server.port=8080\nserver.host=0.0.0.0",
            "server:\n  port: 8080\n  host: 0.0.0.0",
        )
        .expect("比对应当成功");
        assert_eq!(diff.stats.differences(), 0, "{:?}", diff.entries);
        assert_eq!(diff.left_format, ConfigFormat::Properties);
        assert_eq!(diff.right_format, ConfigFormat::Yaml);
    }

    #[test]
    fn broken_json_is_reported_not_panicked() {
        let error = compare("{oops", "a=1").expect_err("坏 JSON 应当报错");
        assert!(error.contains("JSON 解析失败"), "{error}");
    }
}
