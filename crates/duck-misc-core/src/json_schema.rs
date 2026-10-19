//! JSON Schema（Draft-07 子集）校验（`duck.misc.json-schema`）。
//!
//! ## 为什么是重写
//!
//! 参考仓 `JsonSchemaValidator.vue` 把校验交给前端 `ajv` + `ajv-formats` —— 那是
//! **JS 库**，不是可以「整段复制」的业务逻辑；Rust 侧也没有现成实现可用
//! （`jsonschema` crate 既不在本仓依赖图里，也不在离线缓存里）。所以这里按
//! Draft-07 规范实现一个够用子集。
//!
//! **报错文案逐句照抄 ajv**（`must have required property 'x'`、
//! `must NOT have fewer than 2 items` 那一套）。这不是偷懒：同一份 Schema 在
//! 参考仓与本仓给出的错误句子因此完全一致，用户从旧工具迁过来不用重新认字。
//! 也因为如此，这些句子是**英文常量**，不进译文表（它们本来就是英文）。
//!
//! ## 支持的关键字
//!
//! | 类别 | 关键字 |
//! | --- | --- |
//! | 类型 | `type`（含数组形式）、`enum`、`const` |
//! | 对象 | `properties`、`patternProperties`、`additionalProperties`、`required`、`minProperties`、`maxProperties` |
//! | 数组 | `items`（schema 或数组）、`additionalItems`、`minItems`、`maxItems`、`uniqueItems`、`contains` |
//! | 字符串 | `minLength`、`maxLength`、`pattern`、`format` |
//! | 数值 | `minimum`、`maximum`、`exclusiveMinimum`、`exclusiveMaximum`、`multipleOf` |
//! | 组合 | `allOf`、`anyOf`、`oneOf`、`not`、`if`/`then`/`else` |
//! | 引用 | `$ref`（仅本文件内 `#/…` 指针） |
//!
//! 不支持的关键字**静默忽略**（`ajv` 的 `strict: false` 语义），不报「未知关键字」。
//!
//! ## 写法约束
//!
//! 全文件不用 `let` 链（`if let Some(x) = a && cond` 要 Rust 1.88，本仓 MSRV 硬门
//! 是 1.85）。需要「命中 + 再判一次」的地方一律用 [`Option::filter`] 收成**一个**
//! 条件，而不是嵌两层 `if`。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde_json::{Map, Value};

/// 嵌套上限。`$ref` 成环时靠它刹车，而不是靠栈溢出。
const MAX_DEPTH: usize = 64;

/// 「不是校验失败、而是没法校验」的四种情况。
///
/// 源仓也是这么分的（`schemaError` / `instanceError` 与 `errors` 三态互斥），
/// 界面上一橙一红，语义完全不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaProblem {
    /// Schema 不是合法 JSON。
    SchemaNotJson,
    /// 实例不是合法 JSON。
    InstanceNotJson,
    /// Schema 顶层不是对象。
    SchemaNotObject,
    /// `$ref` 指不到（本轮只认本文件内的 `#/…` 指针）。
    UnknownRef(String),
    /// 正则写错（`pattern` / `patternProperties`）。
    BadRegex(String),
    /// 嵌套超过 [`MAX_DEPTH`]，基本可以断定是 `$ref` 成环。
    TooDeep,
}

/// 一条校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// JSON Pointer 形式的位置（ajv 的 `instancePath`）：根是空串，子级是 `/a/0/b`。
    pub path: String,
    /// ajv 文案（英文常量，见模块头）。
    pub message: String,
}

impl SchemaError {
    /// 界面展示用位置：根显示 `/`（源仓 `err.instancePath || '/'`）。
    pub fn display_path(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }
}

/// 校验入口：`Ok(vec![])` = 通过。
///
/// 一次跑完所有分支、收集**全部**错误（ajv 开了 `allErrors: true`，源仓同一语义）。
pub fn validate(schema_text: &str, instance_text: &str) -> Result<Vec<SchemaError>, SchemaProblem> {
    let schema: Value =
        serde_json::from_str(schema_text).map_err(|_| SchemaProblem::SchemaNotJson)?;
    let instance: Value =
        serde_json::from_str(instance_text).map_err(|_| SchemaProblem::InstanceNotJson)?;
    // 布尔 Schema 合法（Draft-07）；其余非对象一律当「不是 Schema」。
    if !schema.is_object() && !schema.is_boolean() {
        return Err(SchemaProblem::SchemaNotObject);
    }
    let mut errors = Vec::new();
    let mut ctx = Ctx { root: &schema };
    ctx.check(&schema, &instance, "", 0, &mut errors)?;
    Ok(errors)
}

struct Ctx<'a> {
    root: &'a Value,
}

impl Ctx<'_> {
    /// 校验一个节点。`path` 是 JSON Pointer，`depth` 防环。
    fn check(
        &mut self,
        schema: &Value,
        instance: &Value,
        path: &str,
        depth: usize,
        errors: &mut Vec<SchemaError>,
    ) -> Result<(), SchemaProblem> {
        if depth > MAX_DEPTH {
            return Err(SchemaProblem::TooDeep);
        }
        // 布尔 Schema：`true` 全通过，`false` 全不过。
        match schema {
            Value::Bool(true) => return Ok(()),
            Value::Bool(false) => {
                push(errors, path, "boolean schema is false".to_owned());
                return Ok(());
            }
            Value::Object(_) => {}
            _ => return Ok(()),
        }
        let object = schema.as_object().expect("checked to be an object above");

        // `$ref` 独占：Draft-07 里它与兄弟关键字互斥（ajv 同语义）。
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            let target = self.resolve(reference)?;
            return self.check(&target, instance, path, depth + 1, errors);
        }

        self.check_type(object, instance, path, errors);
        self.check_enum(object, instance, path, errors);
        self.check_combinations(object, instance, path, depth, errors)?;
        self.check_numbers(object, instance, path, errors);
        self.check_string(object, instance, path, errors)?;
        self.check_array(object, instance, path, depth, errors)?;
        self.check_object(object, instance, path, depth, errors)?;
        Ok(())
    }

    /// `#` / `#/definitions/x` / `#/$defs/x` / `#/properties/a` —— 只认本文件内指针。
    fn resolve(&self, reference: &str) -> Result<Value, SchemaProblem> {
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| SchemaProblem::UnknownRef(reference.to_owned()))?;
        if pointer.is_empty() {
            return Ok(self.root.clone());
        }
        self.root
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| SchemaProblem::UnknownRef(reference.to_owned()))
    }

    fn check_type(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        let Some(declared) = schema.get("type") else {
            return;
        };
        let names: Vec<&str> = match declared {
            Value::String(name) => vec![name.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => return,
        };
        if names.is_empty() || names.iter().any(|name| type_matches(name, instance)) {
            return;
        }
        push(errors, path, format!("must be {}", names.join(",")));
    }

    fn check_enum(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        if !enum_allows(schema, instance) {
            push(
                errors,
                path,
                "must be equal to one of the allowed values".to_owned(),
            );
        }
        if schema
            .get("const")
            .is_some_and(|constant| constant != instance)
        {
            push(errors, path, "must be equal to constant".to_owned());
        }
    }

    /// `allOf` / `anyOf` / `oneOf` / `not` / `if`-`then`-`else`。
    fn check_combinations(
        &mut self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        depth: usize,
        errors: &mut Vec<SchemaError>,
    ) -> Result<(), SchemaProblem> {
        if let Some(Value::Array(branches)) = schema.get("allOf") {
            for branch in branches {
                self.check(branch, instance, path, depth + 1, errors)?;
            }
        }

        if let Some(Value::Array(branches)) = schema.get("anyOf") {
            let mut any = false;
            for branch in branches {
                if self.is_valid(branch, instance, depth + 1)? {
                    any = true;
                    break;
                }
            }
            if !any {
                push(errors, path, "must match a schema in anyOf".to_owned());
            }
        }

        if let Some(Value::Array(branches)) = schema.get("oneOf") {
            let mut matched = 0usize;
            for branch in branches {
                if self.is_valid(branch, instance, depth + 1)? {
                    matched += 1;
                }
            }
            if matched != 1 {
                push(
                    errors,
                    path,
                    "must match exactly one schema in oneOf".to_owned(),
                );
            }
        }

        if let Some(branch) = schema.get("not") {
            let valid = self.is_valid(branch, instance, depth + 1)?;
            if valid {
                push(errors, path, "must NOT be valid".to_owned());
            }
        }

        // Draft-07 的 `if` / `then` / `else`：`if` 只做判定，不产生错误。
        if let Some(condition) = schema.get("if") {
            let branch = match self.is_valid(condition, instance, depth + 1)? {
                true => schema.get("then"),
                false => schema.get("else"),
            };
            if let Some(branch) = branch {
                self.check(branch, instance, path, depth + 1, errors)?;
            }
        }

        Ok(())
    }

    /// 这条分支过不过（错误丢弃）——组合关键字与 `contains` 都靠它。
    fn is_valid(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
    ) -> Result<bool, SchemaProblem> {
        let mut scratch = Vec::new();
        self.check(schema, instance, "", depth, &mut scratch)?;
        Ok(scratch.is_empty())
    }

    fn check_numbers(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) {
        let Some(number) = instance.as_f64() else {
            return;
        };
        if let Some(minimum) = number_at(schema, "minimum").filter(|min| number < *min) {
            push(errors, path, format!("must be >= {minimum}"));
        }
        if let Some(maximum) = number_at(schema, "maximum").filter(|max| number > *max) {
            push(errors, path, format!("must be <= {maximum}"));
        }
        if let Some(minimum) = number_at(schema, "exclusiveMinimum").filter(|min| number <= *min) {
            push(errors, path, format!("must be > {minimum}"));
        }
        if let Some(maximum) = number_at(schema, "exclusiveMaximum").filter(|max| number >= *max) {
            push(errors, path, format!("must be < {maximum}"));
        }
        if let Some(step) = number_at(schema, "multipleOf").filter(|step| *step > 0.0) {
            let quotient = number / step;
            if (quotient - quotient.round()).abs() > 1e-9 {
                push(errors, path, format!("must be multiple of {step}"));
            }
        }
    }

    fn check_string(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        errors: &mut Vec<SchemaError>,
    ) -> Result<(), SchemaProblem> {
        let Some(text) = instance.as_str() else {
            return Ok(());
        };
        let length = text.chars().count() as u64;
        if let Some(minimum) = count_at(schema, "minLength").filter(|min| length < *min) {
            push(
                errors,
                path,
                format!("must NOT have fewer than {minimum} characters"),
            );
        }
        if let Some(maximum) = count_at(schema, "maxLength").filter(|max| length > *max) {
            push(
                errors,
                path,
                format!("must NOT have more than {maximum} characters"),
            );
        }
        if let Some(pattern) = text_at(schema, "pattern") {
            let regex = compile(pattern)?;
            if !regex.is_match(text) {
                push(errors, path, format!("must match pattern \"{pattern}\""));
            }
        }
        if let Some(format) = text_at(schema, "format").filter(|f| !format_matches(f, text)) {
            push(errors, path, format!("must match format \"{format}\""));
        }
        Ok(())
    }

    fn check_array(
        &mut self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        depth: usize,
        errors: &mut Vec<SchemaError>,
    ) -> Result<(), SchemaProblem> {
        let Some(items) = instance.as_array() else {
            return Ok(());
        };
        let length = items.len() as u64;
        if let Some(minimum) = count_at(schema, "minItems").filter(|min| length < *min) {
            push(
                errors,
                path,
                format!("must NOT have fewer than {minimum} items"),
            );
        }
        if let Some(maximum) = count_at(schema, "maxItems").filter(|max| length > *max) {
            push(
                errors,
                path,
                format!("must NOT have more than {maximum} items"),
            );
        }
        if flag_at(schema, "uniqueItems") {
            let duplicate = items
                .iter()
                .enumerate()
                .any(|(index, item)| items[..index].iter().any(|seen| seen == item));
            if duplicate {
                push(errors, path, "must NOT have duplicate items".to_owned());
            }
        }
        match schema.get("items") {
            // 元组形式：逐位对齐，多出来的看 `additionalItems`。
            Some(Value::Array(tuple)) => {
                for (index, branch) in tuple.iter().enumerate() {
                    if let Some(item) = items.get(index) {
                        let at = index_path(path, index);
                        self.check(branch, item, &at, depth + 1, errors)?;
                    }
                }
                if tuple.len() < items.len() && !flag_at(schema, "additionalItems") {
                    push(
                        errors,
                        path,
                        format!("must NOT have more than {} items", tuple.len()),
                    );
                }
            }
            Some(branch) => {
                for (index, item) in items.iter().enumerate() {
                    let at = index_path(path, index);
                    self.check(branch, item, &at, depth + 1, errors)?;
                }
            }
            None => {}
        }
        if let Some(branch) = schema.get("contains") {
            let mut hit = false;
            for item in items {
                if self.is_valid(branch, item, depth + 1)? {
                    hit = true;
                    break;
                }
            }
            if !hit {
                push(
                    errors,
                    path,
                    "must contain at least 1 valid item(s)".to_owned(),
                );
            }
        }
        Ok(())
    }

    fn check_object(
        &mut self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        depth: usize,
        errors: &mut Vec<SchemaError>,
    ) -> Result<(), SchemaProblem> {
        let Some(map) = instance.as_object() else {
            return Ok(());
        };
        let length = map.len() as u64;
        if let Some(minimum) = count_at(schema, "minProperties").filter(|min| length < *min) {
            push(
                errors,
                path,
                format!("must NOT have fewer than {minimum} properties"),
            );
        }
        if let Some(maximum) = count_at(schema, "maxProperties").filter(|max| length > *max) {
            push(
                errors,
                path,
                format!("must NOT have more than {maximum} properties"),
            );
        }
        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    push(
                        errors,
                        path,
                        format!("must have required property '{name}'"),
                    );
                }
            }
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (name, branch) in properties {
                if let Some(value) = map.get(name) {
                    let at = child_path(path, name);
                    self.check(branch, value, &at, depth + 1, errors)?;
                }
            }
        }

        // `patternProperties` 命中的键也要校验（同一个键可以命中多条）。
        let mut matched: Vec<&str> = Vec::new();
        if let Some(patterns) = schema.get("patternProperties").and_then(Value::as_object) {
            for (pattern, branch) in patterns {
                let regex = compile(pattern)?;
                for (name, value) in map {
                    if regex.is_match(name) {
                        matched.push(name.as_str());
                        let at = child_path(path, name);
                        self.check(branch, value, &at, depth + 1, errors)?;
                    }
                }
            }
        }

        if let Some(extra) = schema.get("additionalProperties") {
            for (name, value) in map {
                let known = properties.is_some_and(|properties| properties.contains_key(name));
                if known || matched.contains(&name.as_str()) {
                    continue;
                }
                match extra {
                    Value::Bool(false) => push(
                        errors,
                        path,
                        "must NOT have additional properties".to_owned(),
                    ),
                    Value::Bool(true) => {}
                    branch => {
                        let at = child_path(path, name);
                        self.check(branch, value, &at, depth + 1, errors)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// 拼子路径：JSON Pointer 的 `~` / `/` 要转义（ajv 同规则）。
fn child_path(parent: &str, name: &str) -> String {
    let escaped = name.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn index_path(parent: &str, index: usize) -> String {
    format!("{parent}/{index}")
}

fn push(errors: &mut Vec<SchemaError>, path: &str, message: String) {
    errors.push(SchemaError {
        path: path.to_owned(),
        message,
    });
}

fn text_at<'a>(schema: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    schema.get(key).and_then(Value::as_str)
}

fn number_at(schema: &Map<String, Value>, key: &str) -> Option<f64> {
    schema.get(key).and_then(Value::as_f64)
}

fn count_at(schema: &Map<String, Value>, key: &str) -> Option<u64> {
    schema.get(key).and_then(Value::as_u64)
}

fn flag_at(schema: &Map<String, Value>, key: &str) -> bool {
    schema.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `enum` 放行吗？没有 `enum` 一律放行。
fn enum_allows(schema: &Map<String, Value>, instance: &Value) -> bool {
    match schema.get("enum") {
        Some(Value::Array(allowed)) => allowed.iter().any(|item| item == instance),
        _ => true,
    }
}

fn type_matches(name: &str, instance: &Value) -> bool {
    match name {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        // JS 里 `25.0` 就是整数，Draft-07 也按数学值判 —— 所以看小数部分而不是看类型。
        "integer" => instance
            .as_f64()
            .is_some_and(|value| value.is_finite() && value.fract() == 0.0),
        _ => false,
    }
}

/// 正则缓存：`patternProperties` 会在每个键上走一遍，反复编译纯属浪费。
///
/// 未命中的模式编译后 `Box::leak` 一份 `&'static`（Schema 里的模式数量有限，
/// 一次进程跑完就退，不存在「泄漏累积」）。
fn compile(pattern: &str) -> Result<&'static Regex, SchemaProblem> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().ok().and_then(|map| map.get(pattern).copied()) {
        return Ok(found);
    }
    let regex = Regex::new(pattern).map_err(|_| SchemaProblem::BadRegex(pattern.to_owned()))?;
    let leaked: &'static Regex = Box::leak(Box::new(regex));
    if let Ok(mut map) = cache.lock() {
        map.insert(pattern.to_owned(), leaked);
    }
    Ok(leaked)
}

/// `format` 校验（`ajv-formats` 里最常用的那批）。
///
/// 表里没有的格式**一律放行** —— 与 `ajv-formats` 的 `unknownFormats: true` 一致。
fn format_matches(format: &str, text: &str) -> bool {
    match format {
        "email" => regex_match(EMAIL.get_or_init(|| expect_regex(r"^[^\s@]+@[^\s@]+\.[^\s@]+$")), text),
        // ajv-formats 的 `uri` 只要求「能解析成 URI」，这里按「含 scheme:」判。
        "uri" | "url" | "iri" => regex_match(
            URI.get_or_init(|| expect_regex(r"^[A-Za-z][A-Za-z0-9+.\-]*:")),
            text,
        ),
        "ipv4" => {
            regex_match(
                IPV4.get_or_init(|| expect_regex(r"^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$")),
                text,
            ) && text
                .split('.')
                .all(|part| part.parse::<u16>().is_ok_and(|value| value <= 255))
        }
        "ipv6" => regex_match(
            IPV6.get_or_init(|| expect_regex(r"^[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]*$")),
            text,
        ),
        "hostname" => regex_match(
            HOSTNAME.get_or_init(|| {
                expect_regex(r"^[A-Za-z0-9]([A-Za-z0-9\-]{0,61}[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9\-]{0,61}[A-Za-z0-9])?)*$")
            }),
            text,
        ),
        "uuid" => regex_match(
            UUID.get_or_init(|| {
                expect_regex(r"^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$")
            }),
            text,
        ),
        "date" => {
            regex_match(DATE.get_or_init(|| expect_regex(r"^\d{4}-\d{2}-\d{2}$")), text)
                && date_is_real(text)
        }
        "time" => regex_match(
            TIME.get_or_init(|| expect_regex(r"^\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?$")),
            text,
        ),
        "date-time" => match text.split_once('T') {
            Some((date, rest)) => {
                format_matches("date", date) && format_matches("time", rest.trim_end_matches('Z'))
            }
            None => false,
        },
        "regex" => Regex::new(text).is_ok(),
        _ => true,
    }
}

/// 常量正则编译。写死的模式编译不过属于代码 bug，用 `expect` 当场炸掉比静默放行好。
fn expect_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("built-in format regex must compile")
}

fn regex_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text)
}

/// `2024-02-31` 这种形似而实非的日期要拦下（ajv-formats 也拦）。
fn date_is_real(text: &str) -> bool {
    let parts: Vec<u32> = text
        .split('-')
        .filter_map(|part| part.parse().ok())
        .collect();
    let [year, month, day] = parts[..] else {
        return false;
    };
    if month == 0 || month > 12 || day == 0 {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let limit = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    day <= limit
}

static EMAIL: OnceLock<Regex> = OnceLock::new();
static URI: OnceLock<Regex> = OnceLock::new();
static IPV4: OnceLock<Regex> = OnceLock::new();
static IPV6: OnceLock<Regex> = OnceLock::new();
static HOSTNAME: OnceLock<Regex> = OnceLock::new();
static UUID: OnceLock<Regex> = OnceLock::new();
static DATE: OnceLock<Regex> = OnceLock::new();
static TIME: OnceLock<Regex> = OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn errors_of(schema: &str, instance: &str) -> Vec<SchemaError> {
        validate(schema, instance).expect("schema 应当可校验")
    }

    /// 把错误摊成 `路径 文案`，方便整条断言。
    fn messages(schema: &str, instance: &str) -> Vec<String> {
        errors_of(schema, instance)
            .into_iter()
            .map(|error| format!("{} {}", error.display_path(), error.message))
            .collect()
    }

    #[test]
    fn accepts_the_reference_sample() {
        // 源仓「加载示例」用的那对 schema / instance，必须通过。
        let schema = r#"{
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "required": ["name", "email"],
            "properties": {
                "name": { "type": "string", "minLength": 1 },
                "email": { "type": "string", "format": "email" },
                "age": { "type": "integer", "minimum": 0 }
            },
            "additionalProperties": false
        }"#;
        let instance = r#"{"name":"张三","email":"zhangsan@example.com","age":25}"#;
        assert!(errors_of(schema, instance).is_empty());
    }

    #[test]
    fn reports_type_format_and_extra_property_together() {
        let schema = r#"{
            "type": "object",
            "required": ["name", "email"],
            "properties": {
                "name": { "type": "string" },
                "email": { "type": "string", "format": "email" }
            },
            "additionalProperties": false
        }"#;
        let messages = messages(schema, r#"{"name":1,"email":"nope","extra":true}"#);
        assert!(
            messages.contains(&"/name must be string".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"/email must match format \"email\"".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"/ must NOT have additional properties".to_owned()),
            "{messages:?}"
        );
        // `name` 存在，不该报 required。
        assert!(
            !messages
                .iter()
                .any(|line| line.contains("required property"))
        );
    }

    #[test]
    fn missing_required_is_reported_at_the_parent() {
        let schema = r#"{"type":"object","required":["name"]}"#;
        assert_eq!(
            messages(schema, "{}"),
            vec!["/ must have required property 'name'"]
        );
    }

    #[test]
    fn nested_paths_use_json_pointer_escaping() {
        let schema =
            r#"{"type":"object","properties":{"a/b":{"type":"array","items":{"type":"string"}}}}"#;
        assert_eq!(
            messages(schema, r#"{"a/b":[1]}"#),
            vec!["/a~1b/0 must be string"]
        );
    }

    #[test]
    fn integer_accepts_integral_floats_like_javascript() {
        let schema = r#"{"type":"integer"}"#;
        assert!(errors_of(schema, "25.0").is_empty());
        assert_eq!(messages(schema, "25.5"), vec!["/ must be integer"]);
    }

    #[test]
    fn union_type_lists_every_allowed_name() {
        assert_eq!(
            messages(r#"{"type":["string","number"]}"#, "true"),
            vec!["/ must be string,number"]
        );
    }

    #[test]
    fn arrays_check_bounds_uniqueness_and_contains() {
        let schema = r#"{"type":"array","minItems":2,"uniqueItems":true,"contains":{"const":"x"}}"#;
        let messages = messages(schema, r#"["a","a"]"#);
        assert!(
            messages.contains(&"/ must NOT have duplicate items".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"/ must contain at least 1 valid item(s)".to_owned()),
            "{messages:?}"
        );
        assert!(
            !messages
                .iter()
                .any(|line| line.contains("fewer than 2 items"))
        );
    }

    #[test]
    fn tuple_items_and_additional_items_false() {
        let schema = r#"{"items":[{"type":"string"}],"additionalItems":false}"#;
        assert_eq!(
            messages(schema, r#"["a","b"]"#),
            vec!["/ must NOT have more than 1 items"]
        );
    }

    #[test]
    fn pattern_properties_validate_matching_keys() {
        let schema =
            r#"{"patternProperties":{"^x-":{"type":"number"}},"additionalProperties":false}"#;
        let messages = messages(schema, r#"{"x-a":"no","y":1}"#);
        assert!(
            messages.contains(&"/x-a must be number".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"/ must NOT have additional properties".to_owned()),
            "{messages:?}"
        );
    }

    #[test]
    fn combinators_pick_the_right_branch() {
        let any_of = r#"{"anyOf":[{"type":"string"},{"type":"number"}]}"#;
        assert!(errors_of(any_of, "\"x\"").is_empty());
        assert_eq!(
            messages(any_of, "true"),
            vec!["/ must match a schema in anyOf"]
        );

        let one_of = r#"{"oneOf":[{"type":"number"},{"type":"integer"}]}"#;
        assert_eq!(
            messages(one_of, "1"),
            vec!["/ must match exactly one schema in oneOf"]
        );

        let not = r#"{"not":{"type":"string"}}"#;
        assert_eq!(messages(not, "\"x\""), vec!["/ must NOT be valid"]);
    }

    #[test]
    fn if_then_else_follows_the_condition() {
        let schema = r#"{
            "if": { "properties": { "kind": { "const": "a" } }, "required": ["kind"] },
            "then": { "required": ["aValue"] },
            "else": { "required": ["bValue"] }
        }"#;
        assert!(errors_of(schema, r#"{"kind":"a","aValue":1}"#).is_empty());
        assert_eq!(
            messages(schema, r#"{"kind":"b"}"#),
            vec!["/ must have required property 'bValue'"]
        );
    }

    #[test]
    fn local_ref_resolves_and_unknown_ref_is_reported() {
        let schema = r##"{
            "definitions": { "port": { "type": "integer", "maximum": 65535 } },
            "properties": { "port": { "$ref": "#/definitions/port" } }
        }"##;
        assert_eq!(
            messages(schema, r#"{"port":70000}"#),
            vec!["/port must be <= 65535"]
        );

        let broken = r##"{"$ref":"#/definitions/nope"}"##;
        assert_eq!(
            validate(broken, "1"),
            Err(SchemaProblem::UnknownRef("#/definitions/nope".to_owned()))
        );
    }

    #[test]
    fn recursive_ref_stops_at_the_depth_limit() {
        // 无限自引：靠 MAX_DEPTH 刹车，而不是爆栈。
        assert_eq!(
            validate(r##"{"$ref":"#"}"##, "1"),
            Err(SchemaProblem::TooDeep)
        );
    }

    #[test]
    fn numbers_honour_bounds_and_multiple_of() {
        let schema = r#"{"minimum":1,"maximum":10,"exclusiveMaximum":10,"multipleOf":3}"#;
        let messages = messages(schema, "10");
        assert!(
            messages.contains(&"/ must be < 10".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"/ must be multiple of 3".to_owned()),
            "{messages:?}"
        );
        assert!(!messages.iter().any(|line| line.contains("<= 10")));
    }

    #[test]
    fn string_length_counts_characters_not_bytes() {
        let schema = r#"{"minLength":2,"maxLength":3}"#;
        assert!(errors_of(schema, "\"中文\"").is_empty());
        assert_eq!(
            messages(schema, "\"一\""),
            vec!["/ must NOT have fewer than 2 characters"]
        );
    }

    #[test]
    fn formats_cover_the_documented_set() {
        assert!(format_matches("email", "a@b.cn"));
        assert!(!format_matches("email", "a@b"));
        assert!(format_matches("ipv4", "10.0.0.255"));
        assert!(!format_matches("ipv4", "10.0.0.256"));
        assert!(format_matches(
            "uuid",
            "2c1a4c1e-1111-2222-3333-444455556666"
        ));
        assert!(format_matches("date", "2024-02-29"));
        assert!(!format_matches("date", "2023-02-29"));
        assert!(format_matches("date-time", "2024-02-29T10:00:00"));
        assert!(format_matches("hostname", "a.b-c.example"));
        // 未知格式放行（ajv-formats 同语义）。
        assert!(format_matches("semver", "1.2.3"));
    }

    #[test]
    fn input_problems_are_reported_separately() {
        assert_eq!(validate("{", "1"), Err(SchemaProblem::SchemaNotJson));
        assert_eq!(validate("{}", "{"), Err(SchemaProblem::InstanceNotJson));
        assert_eq!(validate("[]", "1"), Err(SchemaProblem::SchemaNotObject));
        assert_eq!(
            validate(r#"{"pattern":"["}"#, "\"x\""),
            Err(SchemaProblem::BadRegex("[".to_owned()))
        );
    }

    #[test]
    fn boolean_schema_shortcuts() {
        assert!(errors_of("true", "1").is_empty());
        assert_eq!(messages("false", "1"), vec!["/ boolean schema is false"]);
    }
}
