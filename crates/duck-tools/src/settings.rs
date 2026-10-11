//! 工具级设置：注册时声明 schema，运行时经 `tool_state` 表持久化。
//!
//! 分两半：
//! - **schema**（[`ToolSettingSpec`] / [`ToolSettingKind`]）：注册时随 [`crate::ToolMeta`] 一起给出，
//!   是 `'static` 的，决定设置窗口里这一行长什么样；
//! - **取值 / 落盘**（[`current`] / [`apply`]）：把 schema 与 `duck-store` 的 `tool_state` 表接起来。
//!
//! 设置窗口只依赖这两半，因此**新增一个工具设置不需要改设置窗口的代码**。

use gpui::App;

/// 启用开关的保留键：所有工具通用，不必在 `settings()` 里声明。
pub const ENABLED_KEY: &str = "enabled";
/// 使用次数的保留键（由 [`crate::ToolRegistry::record_use`] 累加）。
pub const RUNS_KEY: &str = "runs";

/// 下拉选项：`value` 是落盘值（稳定），`label` 是展示名（中文源串，渲染时经 `tr`）。
#[derive(Debug, Clone, Copy)]
pub struct SelectOption {
    value: &'static str,
    label: &'static str,
}

impl SelectOption {
    /// 构造一个下拉选项。
    #[must_use]
    pub const fn new(value: &'static str, label: &'static str) -> Self {
        Self { value, label }
    }

    /// 落盘值。
    #[must_use]
    pub const fn value(&self) -> &'static str {
        self.value
    }

    /// 展示名（源串）。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }
}

/// 设置项类型（同时携带该类型的默认值）。
#[derive(Debug, Clone, Copy)]
pub enum ToolSettingKind {
    /// 开关。
    Toggle { default: bool },
    /// 单选下拉。
    Select {
        options: &'static [SelectOption],
        default: &'static str,
    },
    /// 单行文本。
    Text {
        placeholder: &'static str,
        default: &'static str,
    },
    /// 整数输入（读值时会钳到 `[min, max]`）。
    Number {
        min: i64,
        max: i64,
        default: i64,
        unit: &'static str,
    },
}

/// 一项工具设置的 schema。
#[derive(Debug, Clone, Copy)]
pub struct ToolSettingSpec {
    key: &'static str,
    label: &'static str,
    description: &'static str,
    kind: ToolSettingKind,
}

impl ToolSettingSpec {
    /// 构造一项设置；`description` 默认为空，用 [`with_description`](Self::with_description) 补。
    #[must_use]
    pub const fn new(key: &'static str, label: &'static str, kind: ToolSettingKind) -> Self {
        Self {
            key,
            label,
            description: "",
            kind,
        }
    }

    /// 补一行说明文字。
    #[must_use]
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// 落盘键。
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// 展示名（源串）。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// 说明文字（源串；可能为空）。
    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// 设置项类型。
    #[must_use]
    pub const fn kind(&self) -> ToolSettingKind {
        self.kind
    }
}

/// 一项设置的当前值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// 开关。
    Toggle(bool),
    /// 单选；值是 [`SelectOption::value`]。
    Select(&'static str),
    /// 文本。
    Text(String),
    /// 整数。
    Number(i64),
}

/// 读某项设置的原始落盘值。
#[must_use]
pub fn raw(cx: &App, tool_id: &str, key: &str) -> Option<String> {
    duck_store::tool_state_get(cx, tool_id, key)
}

/// 读某项设置的当前值（缺省或非法时回落 schema 默认值）。
#[must_use]
pub fn current(cx: &App, tool_id: &str, spec: &ToolSettingSpec) -> SettingValue {
    let stored = raw(cx, tool_id, spec.key());
    match spec.kind() {
        ToolSettingKind::Toggle { default } => {
            SettingValue::Toggle(stored.as_deref().and_then(parse_bool).unwrap_or(default))
        }
        ToolSettingKind::Select { options, default } => {
            // 非法值（schema 改过 / 旧数据）回落默认，避免下拉卡在空态。
            let matched = stored.as_deref().and_then(|stored| {
                options
                    .iter()
                    .find(|option| option.value() == stored)
                    .map(SelectOption::value)
            });
            SettingValue::Select(matched.unwrap_or(default))
        }
        ToolSettingKind::Text { default, .. } => {
            SettingValue::Text(stored.unwrap_or_else(|| default.to_owned()))
        }
        ToolSettingKind::Number {
            min, max, default, ..
        } => {
            let value = stored
                .as_deref()
                .and_then(|text| text.parse::<i64>().ok())
                .unwrap_or(default);
            SettingValue::Number(value.clamp(min, max))
        }
    }
}

/// 写某项设置。
pub fn apply(cx: &App, tool_id: &str, spec: &ToolSettingSpec, value: SettingValue) {
    let text = match value {
        SettingValue::Toggle(on) => if on { "true" } else { "false" }.to_owned(),
        SettingValue::Select(option) => option.to_owned(),
        SettingValue::Text(text) => text,
        SettingValue::Number(number) => number.to_string(),
    };
    duck_store::tool_state_put(cx, tool_id, spec.key(), text);
}

/// 工具是否启用（缺省启用）。
#[must_use]
pub fn is_enabled(cx: &App, tool_id: &str) -> bool {
    raw(cx, tool_id, ENABLED_KEY)
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(true)
}

/// 设置工具是否启用。
pub fn set_enabled(cx: &App, tool_id: &str, enabled: bool) {
    duck_store::tool_state_put(
        cx,
        tool_id,
        ENABLED_KEY,
        if enabled { "true" } else { "false" },
    );
}

/// 工具累计使用次数。
#[must_use]
pub fn runs(cx: &App, tool_id: &str) -> u64 {
    raw(cx, tool_id, RUNS_KEY)
        .as_deref()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// 使用次数 +1（UI 单线程，读改写无需额外同步）。
pub fn bump_runs(cx: &App, tool_id: &str) {
    duck_store::tool_state_put(cx, tool_id, RUNS_KEY, (runs(cx, tool_id) + 1).to_string());
}

/// 解析布尔落盘值；只认 `true` / `false`（大小写不敏感）。
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{SelectOption, ToolSettingKind, parse_bool};

    #[test]
    fn parses_known_booleans_only() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool(" TRUE "), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("yes"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn select_option_exposes_stable_value() {
        const OPTIONS: &[SelectOption] = &[
            SelectOption::new("auto", "自动"),
            SelectOption::new("manual", "手动"),
        ];
        let kind = ToolSettingKind::Select {
            options: OPTIONS,
            default: "auto",
        };
        let ToolSettingKind::Select { options, default } = kind else {
            panic!("类型不符");
        };
        assert_eq!(default, "auto");
        assert_eq!(options[1].value(), "manual");
    }
}
