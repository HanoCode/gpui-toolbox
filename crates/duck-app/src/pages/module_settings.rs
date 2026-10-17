//! 模块偏好页：终端 / 数据库 / 邮箱 / Redis / 项目管理。
//!
//! 设置窗口的「模块」分组。这一层只放**跨连接的偏好**与**新建连接时的默认值**：
//! 连接级配置（TLS 档位、跳板机、Redis 写策略）继续留在各自的连接编辑器里，
//! 免得同一件事有两个入口、两处真源。
//!
//! 持久化沿用项目既有约定 —— 「结构体 → `serde_json` → 单 kv 键」：
//! [`KV_TERMINAL`] / [`KV_DB`] / [`KV_MAIL`] / [`KV_REDIS`] / [`KV_PROJECTS`]。每个字段都
//! `#[serde(default)]`（配 `Default` 实现），旧版本存下的 JSON 缺字段时读回默认值，
//! 而不是整份配置解析失败。
//!
//! 写入一律「`kv_put` + `flush` + `refresh_windows`」：`kv_put` 是 400ms 去抖的，
//! 写完立刻重绘会读回旧值、把控件弹回去（与 `pages::data` 同一处理）。
//!
//! 三类**硬口径**不在这里给开关 —— 数据库危险 SQL 确认、Redis 终端破坏性命令确认、
//! SSH 主机密钥校验：给了开关等于把安全基线做成可关项。

use std::rc::Rc;

use duck_components::{
    ButtonVariant, body_text, button, dropdown_select, option_row, settings_section, switch_control,
};
use duck_theme::{active_theme, tr, trf};
use gpui::{
    AnyElement, App, Div, ElementId, IntoElement, ParentElement, SharedString, Styled, Window,
};
use gpui_component::{h_flex, v_flex};

use super::status_support::{NumberConfirm, TextConfirm, open_number_prompt, open_text_prompt};

/// 终端偏好的 kv 键。
const KV_TERMINAL: &str = "terminal.settings";
/// 数据库偏好的 kv 键。
const KV_DB: &str = "db.settings";
/// 邮箱偏好的 kv 键。
const KV_MAIL: &str = "mail.settings";
/// Redis 偏好的 kv 键。
const KV_REDIS: &str = "redis.settings";
/// 项目管理偏好的 kv 键。
const KV_PROJECTS: &str = "projects.settings";

/// 字号下拉里「跟随主题」项的值。
const FONT_FOLLOW_THEME: &str = "theme";

/// 终端字号的可选档位（与终端页工具条 ± / 快捷键共用同一区间）。
const TERMINAL_FONT_SIZES: std::ops::RangeInclusive<u32> = 10..=20;

/// 回滚行数的可选档位。
const SCROLLBACK_CHOICES: [u32; 4] = [1_000, 10_000, 50_000, 100_000];

/// 读一份模块偏好；缺失或损坏时回落默认值（不 panic、不阻塞）。
fn load<T>(cx: &App, key: &str) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    duck_store::kv_get(cx, key)
        .and_then(|raw| serde_json::from_str::<T>(&raw).ok())
        .unwrap_or_default()
}

/// 写一份模块偏好，并立刻落盘 + 重绘。
///
/// 少了 `flush`，写完立刻重绘会读回去抖窗口里的旧值；少了 `refresh_windows`，
/// 改完要看下一次别的事件才会重绘（看起来就是「点了没反应」）。
fn save<T: serde::Serialize>(cx: &mut App, key: &str, value: &T) {
    if let Ok(json) = serde_json::to_string(value) {
        duck_store::kv_put(cx, key, json);
        duck_store::flush(cx);
    }
    cx.refresh_windows();
}

// ── 终端 ────────────────────────────────────────────────────────────────

/// 终端偏好。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct TerminalSettings {
    /// 画布字号；`None` = 跟随主题的等宽字号。
    pub font_size: Option<u32>,
    /// 回滚缓冲行数（向上翻页保留的历史）。
    pub scrollback: u32,
    /// 断线后是否自动重连（仅 SSH；主机密钥 / 认证类失败一律不重试）。
    pub auto_reconnect: bool,
    /// 自动重连最多尝试几次；用尽后只保留手动入口。
    pub reconnect_max_attempts: u32,
    /// 第一次重连前等待的秒数，之后每次翻倍。
    pub reconnect_backoff_secs: u32,
    /// 字体族；`None` = 跟随主题的等宽字体。
    pub font_family: Option<String>,
    /// 行高（字号百分比）；`None` = 用主题的等宽行高比。
    pub line_height_percent: Option<u32>,
    /// 画布四周留白（逻辑像素）；`None` = 用主题的常规留白。
    pub padding: Option<u32>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_size: None,
            // 出厂默认直接取内核的 `SpawnOptions::default()`：改那里，这里跟着变。
            scrollback: duck_term::SpawnOptions::default().scrollback as u32,
            auto_reconnect: true,
            reconnect_max_attempts: super::terminal_canvas::RECONNECT_MAX_ATTEMPTS,
            reconnect_backoff_secs: super::terminal_canvas::RECONNECT_BACKOFF_START.as_secs()
                as u32,
            font_family: None,
            line_height_percent: None,
            padding: None,
        }
    }
}

/// 读终端偏好。
pub(super) fn terminal_settings(cx: &App) -> TerminalSettings {
    load(cx, KV_TERMINAL)
}

/// 终端偏好页的正文。
pub(super) fn terminal_body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let settings = terminal_settings(cx);

    // ── 显示 ──
    let font_options: Vec<(SharedString, SharedString)> = std::iter::once((
        SharedString::from(FONT_FOLLOW_THEME),
        SharedString::from(tr(cx, "跟随主题")),
    ))
    .chain(TERMINAL_FONT_SIZES.map(|size| {
        let text = SharedString::from(size.to_string());
        (text.clone(), text)
    }))
    .collect();
    let font_current = SharedString::from(
        settings
            .font_size
            .map_or_else(|| FONT_FOLLOW_THEME.to_owned(), |size| size.to_string()),
    );
    let font_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "终端字号"),
            Some(tr(cx, "画布文字的字号。跟随主题时随界面字号一起变。").into()),
            dropdown_select(
                "duck-settings-term-font-size",
                font_options,
                font_current,
                move |value: SharedString, _, cx| {
                    let mut next = settings.clone();
                    next.font_size = (value.as_ref() != FONT_FOLLOW_THEME)
                        .then(|| value.as_ref().parse().ok())
                        .flatten();
                    save(cx, KV_TERMINAL, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let font_family_row = {
        let settings = settings.clone();
        let shown: SharedString = settings
            .font_family
            .clone()
            .filter(|family| !family.trim().is_empty())
            .map_or_else(|| tr(cx, "跟随主题").into(), SharedString::from);
        text_setting_row(
            cx,
            "duck-settings-term-font-family",
            "字体族",
            Some("等宽字体的族名。留空 = 跟随主题。"),
            shown,
            "例如：JetBrains Mono",
            "留空并确定即恢复「跟随主题」。",
            move |value, cx| {
                let mut next = settings.clone();
                let trimmed = value.trim();
                next.font_family = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let line_height_row = {
        let settings = settings.clone();
        let value = settings.line_height_percent.map_or(0, i64::from);
        number_setting_row(
            cx,
            "duck-settings-term-line-height",
            "行高",
            Some("行高占字号的百分比。0 = 跟随主题。"),
            "%",
            value,
            0,
            move |value, cx| {
                let mut next = settings.clone();
                next.line_height_percent = (value > 0).then_some(value as u32);
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let padding_row = {
        let settings = settings.clone();
        let value = settings.padding.map_or(0, i64::from);
        number_setting_row(
            cx,
            "duck-settings-term-padding",
            "内边距",
            Some("画布四周的留白。0 = 跟随主题。"),
            tr(cx, "像素"),
            value,
            0,
            move |value, cx| {
                let mut next = settings.clone();
                next.padding = (value > 0).then_some(value as u32);
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let display_section = settings_section(
        Some(tr(cx, "显示").into()),
        [font_row, font_family_row, line_height_row, padding_row],
        cx,
    );

    // ── 会话 ──
    let scrollback_options: Vec<(SharedString, SharedString)> = SCROLLBACK_CHOICES
        .iter()
        .map(|rows| {
            let text = SharedString::from(trf(cx, "{} 行", &[&rows.to_string()]));
            (SharedString::from(rows.to_string()), text)
        })
        .collect();
    let scrollback_current = SharedString::from(settings.scrollback.to_string());
    let scrollback_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "回滚行数"),
            Some(tr(cx, "向上翻页时保留的历史行数；对新建会话生效。").into()),
            dropdown_select(
                "duck-settings-term-scrollback",
                scrollback_options,
                scrollback_current,
                move |value: SharedString, _, cx| {
                    let Ok(rows) = value.as_ref().parse::<u32>() else {
                        return;
                    };
                    let mut next = settings.clone();
                    next.scrollback = rows;
                    save(cx, KV_TERMINAL, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let reconnect_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "断线自动重连"),
            Some(
                tr(
                    cx,
                    "SSH 会话意外断开后按退避重试。主机密钥或认证失败不重试，只留手动入口。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-term-auto-reconnect",
                settings.auto_reconnect,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.auto_reconnect = *on;
                    save(cx, KV_TERMINAL, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let attempts_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-term-reconnect-attempts",
            "重连上限",
            Some("自动重连最多尝试几次；用尽后只保留手动重连。"),
            tr(cx, "次"),
            i64::from(settings.reconnect_max_attempts),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.reconnect_max_attempts = value as u32;
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let backoff_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-term-reconnect-backoff",
            "起始退避",
            Some("第一次重连前等待的秒数，之后每次翻倍。"),
            tr(cx, "秒"),
            i64::from(settings.reconnect_backoff_secs),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.reconnect_backoff_secs = value as u32;
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let session_section = settings_section(
        Some(tr(cx, "会话").into()),
        [scrollback_row, reconnect_row, attempts_row, backoff_row],
        cx,
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(display_section)
        .child(session_section)
}

// ── 数据库 ──────────────────────────────────────────────────────────────

/// 数据库偏好。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct DbSettings {
    /// 单条查询最多返回多少行。
    pub row_cap: u32,
    /// 表浏览 / 分页查询每页取多少行。
    pub page_size: u32,
    /// 编辑器下拉里保留的历史 SQL 条数。
    pub history_limit: u32,
    /// 新建连接时是否预置「只读档」。
    pub new_read_only: bool,
    /// 新建连接的默认查询超时（秒）；`None` = 跟随引擎。
    pub new_timeout_secs: Option<u32>,
    /// 新建连接的默认环境标签（`EnvTag` 的 wire 值）；`None` = 无标签。
    pub new_env_tag: Option<String>,
    /// SQL 格式化的关键字大小写档（wire 值：`preserve` / `upper` / `lower`）。
    pub sql_keyword_case: String,
    /// SQL 格式化缩进是否用 Tab（`false` = 空格）。
    pub sql_indent_tabs: bool,
    /// SQL 格式化空格缩进宽度（`sql_indent_tabs == false` 时生效）。
    pub sql_indent_width: u32,
    /// SQL 编辑器是否自动换行。
    pub editor_word_wrap: bool,
    /// SQL 语义诊断（解析报错 + 未知表提示，dbx 同款）是否开启。
    pub semantic_diagnostics: bool,
}

impl DbSettings {
    /// 关键字大小写档解析成内核枚举；wire 值不在取值域时回落「大写」（历史默认）。
    pub(crate) fn sql_keyword_case(&self) -> duck_db::SqlKeywordCase {
        match self.sql_keyword_case.as_str() {
            "preserve" => duck_db::SqlKeywordCase::Preserve,
            "lower" => duck_db::SqlKeywordCase::Lower,
            _ => duck_db::SqlKeywordCase::Upper,
        }
    }
}

impl Default for DbSettings {
    fn default() -> Self {
        // 默认值直接取数据库页自己的常量 / 历史行为：改那里，这里跟着变（不搞第二处真源）。
        Self {
            row_cap: super::db::QUERY_ROW_CAP as u32,
            page_size: super::db::PAGE_SIZE as u32,
            history_limit: super::db::HISTORY_LIMIT as u32,
            new_read_only: false,
            new_timeout_secs: None,
            new_env_tag: None,
            sql_keyword_case: "upper".into(),
            sql_indent_tabs: false,
            sql_indent_width: 2,
            editor_word_wrap: false,
            // dbx 同款：支持的引擎上默认开启（`SQL_SEMANTIC_DIAGNOSTICS_AUTO_ENABLED`）。
            semantic_diagnostics: true,
        }
    }
}

/// 读数据库偏好。
pub(super) fn db_settings(cx: &App) -> DbSettings {
    load(cx, KV_DB)
}

/// 只改「每页行数」并落盘（表浏览翻页条的就地切换走这里）。
///
/// 不给调用方直接 `save(KV_DB, ..)`：`save` 是模块私有的，而且「读现状 → 改一个
/// 字段 → 写回」必须在**同一次**读改写里完成——调用方自己拼一份 `DbSettings`
/// 会把此刻的 `row_cap` / `history_limit` 一起写回旧值（用户在设置页刚调过的
/// 上限会被这一次翻页条操作悄悄退回去）。
pub(super) fn set_db_page_size(cx: &mut App, page_size: u32) {
    let mut next = db_settings(cx);
    let page_size = page_size.max(1);
    if next.page_size == page_size {
        return;
    }
    next.page_size = page_size;
    save(cx, KV_DB, &next);
}

/// 只改「SQL 编辑器自动换行」并落盘（工具条开关走这里）。
pub(super) fn set_db_editor_word_wrap(cx: &mut App, on: bool) {
    let mut next = db_settings(cx);
    if next.editor_word_wrap == on {
        return;
    }
    next.editor_word_wrap = on;
    save(cx, KV_DB, &next);
}

/// 只改「SQL 语义诊断」并落盘（工具条开关走这里）。
pub(super) fn set_db_semantic_diagnostics(cx: &mut App, on: bool) {
    let mut next = db_settings(cx);
    if next.semantic_diagnostics == on {
        return;
    }
    next.semantic_diagnostics = on;
    save(cx, KV_DB, &next);
}

/// 只改「SQL 格式化关键字大小写」并落盘（工具条「小写关键字」开关走这里）。
pub(super) fn set_db_sql_keyword_case(cx: &mut App, case: duck_db::SqlKeywordCase) {
    let wire = match case {
        duck_db::SqlKeywordCase::Preserve => "preserve",
        duck_db::SqlKeywordCase::Upper => "upper",
        duck_db::SqlKeywordCase::Lower => "lower",
    };
    let mut next = db_settings(cx);
    if next.sql_keyword_case == wire {
        return;
    }
    next.sql_keyword_case = wire.into();
    save(cx, KV_DB, &next);
}

/// 数据库偏好页的正文。
pub(super) fn database_body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let settings = db_settings(cx);

    let cap_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-db-row-cap",
            "结果行数上限",
            Some("单条查询最多返回多少行，超出即截断。"),
            tr(cx, "行"),
            i64::from(settings.row_cap),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.row_cap = value as u32;
                save(cx, KV_DB, &next);
            },
        )
    };

    let page_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-db-page-size",
            "数据浏览每页行数",
            Some("表浏览与分页查询一次取多少行。"),
            tr(cx, "行"),
            i64::from(settings.page_size),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.page_size = value as u32;
                save(cx, KV_DB, &next);
            },
        )
    };

    let history_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-db-history-limit",
            "查询历史上限",
            Some("编辑器下拉里保留多少条历史 SQL。"),
            tr(cx, "条"),
            i64::from(settings.history_limit),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.history_limit = value as u32;
                save(cx, KV_DB, &next);
            },
        )
    };

    let query_section = settings_section(
        Some(tr(cx, "查询").into()),
        [cap_row, page_row, history_row],
        cx,
    );

    // 环境标签下拉用 `EnvTag` 的 wire 值当稳定标识，展示文案与连接编辑器同源。
    let env_options: Vec<(SharedString, SharedString)> = duck_db::EnvTag::ALL
        .iter()
        .map(|tag| {
            let label = match tag {
                duck_db::EnvTag::None => tr(cx, "无标签"),
                duck_db::EnvTag::Dev => tr(cx, "开发"),
                duck_db::EnvTag::Test => tr(cx, "测试"),
                duck_db::EnvTag::Staging => tr(cx, "预发布"),
                duck_db::EnvTag::Prod => tr(cx, "生产"),
            };
            (
                SharedString::from(tag.wire_name()),
                SharedString::from(label),
            )
        })
        .collect();
    let env_current = SharedString::from(
        settings
            .new_env_tag
            .clone()
            .unwrap_or_else(|| duck_db::EnvTag::None.wire_name().to_owned()),
    );

    let read_only_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "默认只读"),
            Some(tr(cx, "新建连接时预置「只读档」。连接档案里仍可单独修改。").into()),
            switch_control(
                "duck-settings-db-new-read-only",
                settings.new_read_only,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.new_read_only = *on;
                    save(cx, KV_DB, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let timeout_row = {
        let settings = settings.clone();
        let shown: SharedString = settings.new_timeout_secs.map_or_else(
            || tr(cx, "跟随引擎").into(),
            |secs| SharedString::from(trf(cx, "{} 秒", &[&secs.to_string()])),
        );
        let current = settings.new_timeout_secs;
        text_setting_row(
            cx,
            "duck-settings-db-new-timeout",
            "默认超时",
            Some("新建连接的默认查询超时（秒）；留空 = 跟随引擎。"),
            shown,
            "例如：30",
            "留空并确定即恢复「跟随引擎」。",
            move |value, cx| {
                let trimmed = value.trim();
                let parsed = if trimmed.is_empty() {
                    None
                } else {
                    match trimmed.parse::<u32>() {
                        Ok(secs) if secs > 0 => Some(secs),
                        // 非法输入按「保持原值」处理，别把用户手打的字悄悄变成默认。
                        _ => current,
                    }
                };
                let mut next = settings.clone();
                next.new_timeout_secs = parsed;
                save(cx, KV_DB, &next);
            },
        )
    };

    let env_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "默认环境标签"),
            Some(tr(cx, "新建连接时预置的环境标签。").into()),
            dropdown_select(
                "duck-settings-db-new-env-tag",
                env_options,
                env_current,
                move |value: SharedString, _, cx| {
                    let mut next = settings.clone();
                    next.new_env_tag = (value.as_ref() != duck_db::EnvTag::None.wire_name())
                        .then(|| value.to_string());
                    save(cx, KV_DB, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let default_section = settings_section(
        Some(tr(cx, "新建连接的默认值").into()),
        [read_only_row, timeout_row, env_row],
        cx,
    );

    // SQL 格式化与编辑器（dbx 对齐：关键字大小写 / 缩进 / 自动换行）。
    let keyword_options: Vec<(SharedString, SharedString)> = vec![
        (
            SharedString::from("preserve"),
            SharedString::from(tr(cx, "保留原样")),
        ),
        (
            SharedString::from("upper"),
            SharedString::from(tr(cx, "大写")),
        ),
        (
            SharedString::from("lower"),
            SharedString::from(tr(cx, "小写")),
        ),
    ];
    let keyword_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "关键字大小写"),
            Some(tr(cx, "格式化 SQL 时把关键字统一成的大小写。").into()),
            dropdown_select(
                "duck-settings-db-sql-keyword-case",
                keyword_options,
                SharedString::from(settings.sql_keyword_case.clone()),
                move |value: SharedString, _, cx| {
                    let mut next = settings.clone();
                    next.sql_keyword_case = value.to_string();
                    save(cx, KV_DB, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let indent_options: Vec<(SharedString, SharedString)> = vec![
        (
            SharedString::from("spaces-2"),
            SharedString::from(tr(cx, "空格 × 2")),
        ),
        (
            SharedString::from("spaces-4"),
            SharedString::from(tr(cx, "空格 × 4")),
        ),
        (
            SharedString::from("tabs"),
            SharedString::from(tr(cx, "Tab")),
        ),
    ];
    let indent_current = if settings.sql_indent_tabs {
        SharedString::from("tabs")
    } else if settings.sql_indent_width >= 4 {
        SharedString::from("spaces-4")
    } else {
        SharedString::from("spaces-2")
    };
    let indent_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "缩进"),
            Some(tr(cx, "格式化 SQL 时的缩进方式与宽度。").into()),
            dropdown_select(
                "duck-settings-db-sql-indent",
                indent_options,
                indent_current,
                move |value: SharedString, _, cx| {
                    let mut next = settings.clone();
                    match value.as_ref() {
                        "tabs" => {
                            next.sql_indent_tabs = true;
                        }
                        "spaces-4" => {
                            next.sql_indent_tabs = false;
                            next.sql_indent_width = 4;
                        }
                        _ => {
                            next.sql_indent_tabs = false;
                            next.sql_indent_width = 2;
                        }
                    }
                    save(cx, KV_DB, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let word_wrap_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "自动换行"),
            Some(tr(cx, "SQL 过长时在编辑器内折行，而不是横向滚动。").into()),
            switch_control(
                "duck-settings-db-editor-word-wrap",
                settings.editor_word_wrap,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.editor_word_wrap = *on;
                    save(cx, KV_DB, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let semantic_diag_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "SQL 语义诊断"),
            Some(tr(cx, "编辑器下方提示解析报错与目录里不存在的表。").into()),
            switch_control(
                "duck-settings-db-semantic-diagnostics",
                settings.semantic_diagnostics,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.semantic_diagnostics = *on;
                    save(cx, KV_DB, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let format_section = settings_section(
        Some(tr(cx, "SQL 格式化与编辑器").into()),
        [keyword_row, indent_row, word_wrap_row, semantic_diag_row],
        cx,
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(query_section)
        .child(default_section)
        .child(format_section)
}

// ── 邮箱 ────────────────────────────────────────────────────────────────

/// 邮箱偏好。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct MailSettings {
    /// 非当前文件夹来新信时是否出声（应用内横幅 + 系统通知）。
    pub notify_new_mail: bool,
    /// 两种正文都有的邮件默认显示 HTML（否则纯文本优先）。
    pub prefer_html: bool,
    /// 每个文件夹一次加载多少封邮件。
    pub preview_limit: u32,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            notify_new_mail: true,
            // 出厂**HTML 优先**：绝大多数邮件（尤其营销信、通知信、带排版的回执）的
            // 纯文本备胎是机器压出来的「兜底版」——丢链接、丢表格、丢行内图，正文里
            // 只剩一句「本邮件为 HTML 格式，请用支持 HTML 的客户端查看」。参考实现
            // （Flectar Mail）与主流客户端也一律以 HTML 为主形态。
            // 纯文本仍是一键可得（阅读器工具条），且 HTML 缺失时回落纯文本（见
            // `pages::mail::set_detail`）——所以这条只在「两种正文都有」时生效。
            prefer_html: true,
            preview_limit: super::mail::PREVIEW_LIMIT as u32,
        }
    }
}

/// 读邮箱偏好。
pub(super) fn mail_settings(cx: &App) -> MailSettings {
    load(cx, KV_MAIL)
}

/// 邮箱偏好页的正文。
pub(super) fn mail_body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let settings = mail_settings(cx);

    let notify_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "新邮件通知"),
            Some(tr(cx, "别的文件夹来新信时推一条应用内提示与系统通知。").into()),
            switch_control(
                "duck-settings-mail-notify",
                settings.notify_new_mail,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.notify_new_mail = *on;
                    save(cx, KV_MAIL, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let body_options = vec![
        (
            SharedString::from("plain"),
            SharedString::from(tr(cx, "纯文本优先")),
        ),
        (
            SharedString::from("html"),
            SharedString::from(tr(cx, "HTML 优先")),
        ),
    ];
    let body_current = SharedString::from(if settings.prefer_html {
        "html"
    } else {
        "plain"
    });
    let body_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "默认正文形态"),
            Some(
                tr(
                    cx,
                    "两种正文都有的邮件默认显示哪一份；阅读器里仍可随时切换。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-mail-body",
                body_options,
                body_current,
                move |value: SharedString, _, cx| {
                    let mut next = settings.clone();
                    next.prefer_html = value.as_ref() == "html";
                    save(cx, KV_MAIL, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let reading_section = settings_section(Some(tr(cx, "阅读").into()), [notify_row, body_row], cx);

    let limit_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-mail-preview-limit",
            "列表首屏条数",
            Some("每个文件夹一次加载多少封邮件。"),
            tr(cx, "封"),
            i64::from(settings.preview_limit),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.preview_limit = value as u32;
                save(cx, KV_MAIL, &next);
            },
        )
    };

    let list_section = settings_section(Some(tr(cx, "列表").into()), [limit_row], cx);

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(reading_section)
        .child(list_section)
}

// ── Redis ───────────────────────────────────────────────────────────────

/// Redis 偏好。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct RedisSettings {
    /// `SCAN` 每次返回的键数上限；连接档案里单独设置的以档案为准。
    pub scan_page: u32,
}

impl Default for RedisSettings {
    fn default() -> Self {
        Self {
            scan_page: super::redis::SCAN_PAGE as u32,
        }
    }
}

/// 读 Redis 偏好。
pub(super) fn redis_settings(cx: &App) -> RedisSettings {
    load(cx, KV_REDIS)
}

/// Redis 偏好页的正文。
pub(super) fn redis_body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let settings = redis_settings(cx);

    let scan_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-redis-scan-page",
            "扫描页大小",
            Some("SCAN 每次返回的键数上限；连接档案里单独设置的以档案为准。"),
            tr(cx, "个键"),
            i64::from(settings.scan_page),
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.scan_page = value as u32;
                save(cx, KV_REDIS, &next);
            },
        )
    };

    let section = settings_section(Some(tr(cx, "键浏览").into()), [scan_row], cx);

    v_flex().w_full().gap(tokens.spacing.lg).child(section)
}

// ── 项目管理 ────────────────────────────────────────────────────────────

/// 项目管理偏好。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ProjectsSettings {
    /// 到期 / 逾期 / 久未更新时是否出声（应用内横幅 + 系统通知）。
    ///
    /// 与「通知」页的通道开关是**两层**：那里管通道开不开，这里管这条消息该不该出声
    /// （与邮件页 `notify_new_mail` 同款）。
    pub reminders: bool,
    /// 「久未更新」的判定天数：高优先级任务超过这么多天没动过就提醒一次。
    pub stale_days: i64,
}

impl Default for ProjectsSettings {
    fn default() -> Self {
        Self {
            reminders: true,
            // 默认值只有一处真源：判定逻辑就在 `remind` 里，天数不该在这里再写一遍。
            stale_days: super::projects::remind::STALE_DAYS,
        }
    }
}

/// 读项目管理偏好。
pub(super) fn projects_settings(cx: &App) -> ProjectsSettings {
    load(cx, KV_PROJECTS)
}

/// 项目管理偏好页的正文。
pub(super) fn projects_body(cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let settings = projects_settings(cx);

    let remind_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "到期与逾期提醒"),
            Some(
                tr(
                    cx,
                    "到期前一天、当天与逾期时各推一条应用内提示与系统通知；同一件事一天只说一次。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-projects-remind",
                settings.reminders,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.reminders = *on;
                    save(cx, KV_PROJECTS, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let stale_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-projects-stale-days",
            "久未更新提醒",
            Some("高优先级任务超过这么多天没更新过时提醒一次；受阻与搁置的不算。"),
            tr(cx, "天"),
            settings.stale_days,
            1,
            move |value, cx| {
                let mut next = settings.clone();
                next.stale_days = value;
                save(cx, KV_PROJECTS, &next);
            },
        )
    };

    let section = settings_section(Some(tr(cx, "提醒").into()), [remind_row, stale_row], cx);

    v_flex().w_full().gap(tokens.spacing.lg).child(section)
}

// ── 行构建helper ────────────────────────────────────────────────────────

/// 一行「只读展示当前值 + 编辑按钮」；数字与文本共用一个形状，只是弹窗不同。
fn value_row(
    cx: &App,
    id: impl Into<ElementId>,
    title: &'static str,
    description: Option<&'static str>,
    shown: SharedString,
    on_edit: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let tokens = active_theme(cx).tokens().clone();
    let on_edit = Rc::new(on_edit);
    option_row(
        tr(cx, title),
        description.map(|text| SharedString::from(tr(cx, text))),
        h_flex()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(body_text(shown, cx))
            .child(
                button(id, tr(cx, "编辑"), ButtonVariant::Secondary, cx)
                    .on_click(move |_, window, cx| on_edit(window, cx)),
            ),
        cx,
    )
    .into_any_element()
}

/// 一行数字设置：展示当前值 + 「编辑」→ 数字输入弹窗。
///
/// 参数刻意保持扁平：调用点要逐一给出 `id` / 标题 / 说明 / 单位 / 范围 / 落盘闭包，
/// 收进结构体只是把同一份样板挪个地方，对照设置表反而更绕。
#[allow(clippy::too_many_arguments)]
fn number_setting_row(
    cx: &App,
    id: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    unit: &'static str,
    value: i64,
    min: i64,
    apply: impl Fn(i64, &mut App) + 'static,
) -> AnyElement {
    let shown: SharedString = if unit.is_empty() {
        value.to_string().into()
    } else {
        SharedString::from(format!("{value} {unit}"))
    };
    let apply = Rc::new(apply);
    value_row(cx, id, title, description, shown, move |window, cx| {
        let apply = Rc::clone(&apply);
        let confirm: NumberConfirm = Rc::new(move |value, _, cx| apply(value, cx));
        open_number_prompt(window, cx, title, title, unit, None, min, confirm);
    })
}

/// 一行文本设置：展示当前值 + 「编辑」→ 文本输入弹窗。
///
/// 同 [`number_setting_row`]，参数保持扁平。
#[allow(clippy::too_many_arguments)]
fn text_setting_row(
    cx: &App,
    id: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    shown: SharedString,
    placeholder: &'static str,
    hint: &'static str,
    apply: impl Fn(&str, &mut App) + 'static,
) -> AnyElement {
    let apply = Rc::new(apply);
    value_row(cx, id, title, description, shown, move |window, cx| {
        let apply = Rc::clone(&apply);
        let confirm: TextConfirm = Rc::new(move |text, _, cx| apply(text, cx));
        open_text_prompt(window, cx, title, title, placeholder, Some(hint), confirm);
    })
}
