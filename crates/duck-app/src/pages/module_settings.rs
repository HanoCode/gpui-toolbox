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
    ButtonVariant, DuckIcon, body_text, button, dropdown_select, icon_button, notify, option_row,
    settings_section, switch_control,
};
use duck_term::Encoding;
use duck_theme::{active_theme, tr, trf};
use gpui::{
    AnyElement, App, Div, ElementId, Font, FontFeatures, FontStyle, FontWeight, IntoElement,
    ParentElement, SharedString, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{Icon, h_flex, v_flex};

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

/// 终端字号的取值区间。
///
/// **唯一一份**：设置页的下拉、画布工具条的 ± 与 `⌘+` / `⌘-` 的钳位都读它。
/// 以前有两个区间（设置 10..=20、画布 10.0..=20.0，单位还不同），调完一边另一边
/// 会把值夹回去，表现为「怎么调都跳回 20」。
pub(crate) const TERMINAL_FONT_SIZE_RANGE: std::ops::RangeInclusive<u32> = 9..=32;

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
    /// 字体回退链（逗号分隔的族名）；空 = 交给系统级联。
    ///
    /// 主字体里没有的字形（汉字、Nerd 图标、特殊符号）按这个顺序找替补。
    /// 缺省**空**是刻意的：macOS 的 Menlo / SF Mono 本来就会级联到中日韩字体，
    /// 替用户预置一串族名只会在装了其中某一个的机器上悄悄改变现有观感。
    /// 只有在系统级联选错（典型：CJK 用了一个比例字体，宽窄对不上两列）时，
    /// 用户才需要在这里点名。
    pub font_fallbacks: String,
    /// 行高（字号百分比）；`None` = 用主题的等宽行高比。
    pub line_height_percent: Option<u32>,
    /// 画布四周留白（逻辑像素）；`None` = 用主题的常规留白。
    pub padding: Option<u32>,
    /// 粘贴前是否确认（见 [`PasteConfirm`]）。
    pub paste_confirm: PasteConfirm,
    /// 新会话的编码（`"utf-8"` / `"gb18030"`）；`None` = UTF-8。
    ///
    /// 存字符串而不是内核枚举：偏好文件是可读 JSON，未知值要能原样读进来再由
    /// 调用点回落（`Encoding::from_value` 返回 `None` 即回落），不因一个坏值炸掉整份设置。
    /// GBK / GB2312 不看这一项就分不出「同一串字节是哪种中文」—— GB18030 是两者超集。
    pub encoding: Option<String>,
    /// 智能提示总开关：输入时按历史 / 内置命令 / 路径候选给建议（弹层 +
    /// `→`/`Tab` 接受）。关闭时按键路径与从前完全一致。
    pub suggest: bool,
    /// 会话空闲在提示符处时注入 shell 集成脚本（给裸 bash / zsh 补发
    /// `OSC 133` 标记，让提示符判定与命令采集精确化）。默认关 —— 它会往
    /// 用户的 shell 里发一行初始化命令，属于「明确同意才做」的事。
    pub suggest_shell_integration: bool,
}

/// 粘贴确认的三档。
///
/// 分成三档而不是一个开关，是因为「单行粘贴」和「多行粘贴」的风险差一个数量级：
/// 多行里的每个换行都会**立刻执行**，单行不会。默认「仅多行」正好卡在这条线上。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PasteConfirm {
    /// 总是问（单行也问）。
    Always,
    /// 只在文本含换行时问。
    #[default]
    Multiline,
    /// 从不问，直接粘。
    Never,
}

impl PasteConfirm {
    /// 全部档位（下拉项顺序）。
    pub(crate) const ALL: [Self; 3] = [Self::Always, Self::Multiline, Self::Never];

    /// 持久化用的值。
    const fn value(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Multiline => "multiline",
            Self::Never => "never",
        }
    }

    /// 由持久化值还原；未知值返回 `None`（调用点据此回落默认，不猜）。
    fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.value() == value)
    }
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
            font_fallbacks: String::new(),
            line_height_percent: None,
            padding: None,
            paste_confirm: PasteConfirm::default(),
            encoding: None,
            suggest: true,
            suggest_shell_integration: false,
        }
    }
}

/// 字体的「等宽与否」实测：`(是否等宽, 宽窄比)`。
///
/// **不读字体元数据**：`FixedPitch` 这个标记大量字体没写或写错（不少等宽字体的
/// 打包版就缺），读它等于信一个不可靠的字段。直接量 `i` 与 `M` 的推进量 ——
/// 相等才是等宽；不相等时把宽窄比一并给出，用户能据此判断「要不要换」。
fn font_monospace_ratio(window: &Window, family: &SharedString, size: f32) -> Option<(bool, f32)> {
    let text_system = window.text_system();
    let font = Font {
        family: family.clone(),
        features: FontFeatures::default(),
        fallbacks: None,
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    };
    let id = text_system.resolve_font(&font);
    let narrow = text_system.layout_width(id, px(size), 'i').to_f64() as f32;
    let wide = text_system.layout_width(id, px(size), 'M').to_f64() as f32;
    if narrow <= 0. || wide <= 0. {
        return None;
    }
    // 半个像素以内算「相等」：不同后端在亚像素取整上会有微差。
    Some(((wide - narrow).abs() < 0.5, wide / narrow))
}

/// 编码档位的展示名。
///
/// 编码名（UTF-8 / GB18030）是专名不进翻译表；「自动」在这里落翻译 ——
/// 内核 `duck-term` 不接翻译表，展示名只有 UI 这一层需要。
pub(super) fn encoding_label(cx: &App, encoding: Encoding) -> SharedString {
    match encoding {
        Encoding::Auto => tr(cx, "自动").into(),
        Encoding::Utf8 => "UTF-8".into(),
        Encoding::Gb18030 => "GB18030".into(),
    }
}

/// 读终端偏好。
pub(super) fn terminal_settings(cx: &App) -> TerminalSettings {
    load(cx, KV_TERMINAL)
}

/// 智能提示总开关（建议弹层 + 历史采集）。
pub(crate) fn terminal_suggest(cx: &App) -> bool {
    terminal_settings(cx).suggest
}

/// shell 集成注入开关（L2：给裸 bash / zsh 补发 `OSC 133` 标记）。
pub(crate) fn terminal_shell_integration(cx: &App) -> bool {
    terminal_settings(cx).suggest_shell_integration
}

/// 把设置里的回退链解析成 GPUI 的 `Font::fallbacks`。
///
/// 逗号（中英文都收）分隔，逐项削空白、丢空项。**不校验是否已安装**：未安装的族名
/// 会被字体系统直接跳过，而「预先检查一遍」还要一次 `all_font_names()` 的分配。
/// 全空时返回 `None`（不是 `Some(vec![])`）：`None` 才是「按系统默认级联」，
/// 空 vec 会让某些后端干脆不做任何回退 —— 汉字全变成豆腐块。
pub(crate) fn font_fallbacks(settings: &TerminalSettings) -> Option<gpui::FontFallbacks> {
    let families: Vec<String> = settings
        .font_fallbacks
        .split([',', '，'])
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .map(str::to_owned)
        .collect();
    (!families.is_empty()).then(|| gpui::FontFallbacks::from_fonts(families))
}

/// 画布实际生效的字号：设了就用它，否则跟随主题的等宽正文档。
///
/// 纯函数，取 `theme_size` / `ui_scale` 而不是 `&App`：画布每帧都要算一次，
/// 让它自己去读主题只会多一次全局查找，也挡住单测。
///
/// **界面缩放（`ui_scale`）要乘在「显式字号」上、不能乘在主题档上**：
/// 主题档（`theme_size`）本来就是 `font_size × ui_scale` 的合成结果（见
/// `duck_theme::tokens::scale_typography`），再乘一次就成了平方。而显式字号是用户
/// 在界面 100% 时挑的值，界面放大到 150% 后它必须跟着放大 —— 否则「界面上所有字都大了，
/// 只有终端还是原来那么小」，那是用户最先注意到的那种不一致。
pub(crate) fn effective_font_size(
    settings: &TerminalSettings,
    theme_size: f32,
    ui_scale: f32,
) -> f32 {
    let (min, max) = (
        *TERMINAL_FONT_SIZE_RANGE.start() as f32,
        *TERMINAL_FONT_SIZE_RANGE.end() as f32,
    );
    match settings.font_size {
        // 钳位在**用户填的那个数**上（区间描述的是下拉里能选什么），之后再缩放。
        Some(size) => {
            size.clamp(
                *TERMINAL_FONT_SIZE_RANGE.start(),
                *TERMINAL_FONT_SIZE_RANGE.end(),
            ) as f32
                * ui_scale.max(0.1)
        }
        None => theme_size.clamp(min, max),
    }
}

/// 改终端字号（`None` = 跟随主题），立刻落盘并重绘所有窗口。
///
/// **字号只留一份**：画布的 `±` / `⌘0` 与设置页的下拉都走这里，真源是
/// `kv: terminal.settings`。`TerminalPage` 不再自己存一个 `f32` ——
/// 存了必然出现「设置里改了、终端里没变」和它的反向版本。
pub(crate) fn set_terminal_font_size(cx: &mut App, size: Option<u32>) {
    let clamped = size.map(|value| {
        value.clamp(
            *TERMINAL_FONT_SIZE_RANGE.start(),
            *TERMINAL_FONT_SIZE_RANGE.end(),
        )
    });
    let mut next = terminal_settings(cx);
    if next.font_size == clamped {
        return;
    }
    next.font_size = clamped;
    save(cx, KV_TERMINAL, &next);
}

/// 终端偏好页的正文。
pub(super) fn terminal_body(window: &mut Window, cx: &mut App) -> Div {
    let tokens = active_theme(cx).tokens().clone();
    let ui_scale = active_theme(cx).appearance().ui_scale.max(0.1);
    let settings = terminal_settings(cx);

    // ── 显示 ──
    let font_options: Vec<(SharedString, SharedString)> = std::iter::once((
        SharedString::from(FONT_FOLLOW_THEME),
        SharedString::from(tr(cx, "跟随主题")),
    ))
    .chain(TERMINAL_FONT_SIZE_RANGE.map(|size| {
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
        option_row(
            tr(cx, "终端字号"),
            Some(tr(cx, "画布文字的字号。跟随主题时随界面字号一起变。").into()),
            dropdown_select(
                "duck-settings-term-font-size",
                font_options,
                font_current,
                move |value: SharedString, _, cx| {
                    let size = (value.as_ref() != FONT_FOLLOW_THEME)
                        .then(|| value.as_ref().parse().ok())
                        .flatten();
                    set_terminal_font_size(cx, size);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    // 字体族：**只收本机装了的**。填一个没装的族名，画布会静默回退到主题等宽字体 ——
    // 用户看到的是「改了但没变」，比明确拒绝保存难查一个数量级。
    let installed: Rc<Vec<String>> = Rc::new(window.text_system().all_font_names());
    let font_family_row = {
        let settings = settings.clone();
        let installed = Rc::clone(&installed);
        let shown: SharedString = settings
            .font_family
            .clone()
            .filter(|family| !family.trim().is_empty())
            .map_or_else(|| tr(cx, "跟随主题").into(), SharedString::from);
        text_setting_row(
            cx,
            "duck-settings-term-font-family",
            "字体族",
            Some("必须是本机已安装的族名；留空 = 跟随主题。填了没装的会拒绝保存。"),
            shown,
            "例如：JetBrains Mono",
            "留空并确定即恢复「跟随主题」。",
            move |value, window, cx| {
                let trimmed = value.trim();
                if !trimmed.is_empty()
                    && !installed
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(trimmed))
                {
                    notify::warning(
                        trf(cx, "本机没有安装「{}」，未保存。", &[trimmed]),
                        window,
                        cx,
                    );
                    return;
                }
                let mut next = settings.clone();
                next.font_family = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    // 回退链：主字体没有的字形（汉字 / Nerd 图标）按这个顺序找替补。
    // 缺省留空 = 交给系统级联，所以这一行默认显示「跟随系统」，不是某个族名。
    let font_fallbacks_row = {
        let settings = settings.clone();
        let shown: SharedString = if settings.font_fallbacks.trim().is_empty() {
            tr(cx, "跟随系统").into()
        } else {
            SharedString::from(settings.font_fallbacks.clone())
        };
        text_setting_row(
            cx,
            "duck-settings-term-font-fallbacks",
            "回退字体",
            Some(
                "主字体里没有的字形（汉字、图标）按这个顺序找替补，逗号分隔。留空 = 交给系统级联。",
            ),
            shown,
            "例如：Sarasa Mono SC, PingFang SC",
            "留空并确定即恢复「跟随系统」。未安装的族名会被自动跳过。",
            move |value, _window, cx| {
                let mut next = settings.clone();
                next.font_fallbacks = value.trim().to_owned();
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    // 字体预览：样例按**真实族名 + 真实字号**画，等宽与否是量出来的。
    // 这一块替代了「填个族名然后去终端里看效果」的来回：改完当场就能看清。
    let font_preview = {
        let theme = active_theme(cx);
        let extended = theme.extended();
        let hairline = extended.colors.hairline;
        let chrome = extended.colors.chrome;
        let family = super::terminal_canvas::mono_family(&settings, tokens.typography.mono.clone());
        // 预览要与画布**同一个口径**：字号一样（含界面缩放），看到的等宽结论才作数。
        let size = effective_font_size(
            &settings,
            tokens.typography.mono_md.size.to_f64() as f32,
            ui_scale,
        );
        // 预览字号封顶 20：32pt 的样例会把整页顶开，而那一档用户在终端里已经在看。
        let preview_size = size.min(20.);
        let verdict = font_monospace_ratio(window, &family, preview_size).map(|(mono, ratio)| {
            if mono {
                tr(cx, "等宽：是").to_owned()
            } else {
                trf(cx, "等宽：否（宽窄比 1 : {}）", &[&format!("{ratio:.2}")])
            }
        });
        v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(hairline)
            .bg(chrome)
            .px(tokens.spacing.md)
            .py(tokens.spacing.sm)
            .child(
                // 样例刻意混入窄字（`i l . ,`）与宽字（`M W 0 9`）：等宽字体下它们
                // 应当严格落在同一列，非等宽字体一眼就能看出被挤歪。
                div()
                    .font_family(family)
                    .text_size(px(preview_size))
                    .text_color(tokens.colors.foreground)
                    .child("illegal 1lI0O ~ user@host:~$ ls -la MMMM 0123456789"),
            )
            .when_some(verdict, |this, verdict| {
                this.child(
                    div()
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(tokens.colors.muted_foreground)
                        .child(verdict),
                )
            })
            .into_any_element()
    };

    let line_height_row = {
        let settings = settings.clone();
        let value = settings.line_height_percent.map_or(0, i64::from);
        let shown: SharedString = if value == 0 {
            tr(cx, "跟随主题").into()
        } else {
            format!("{value} %").into()
        };
        stepper_row(
            cx,
            "duck-settings-term-line-height",
            "行高",
            Some("行高占字号的百分比，5% 一档。0 = 跟随主题（字越紧，一屏装得越多）。"),
            shown,
            value,
            // 90% 以下会把字压在一起；200% 以上一屏只剩几行 —— 两端都不给。
            // 0 是「跟随主题」这个特殊档，所以区间从 0 起、步进时直接跳到 100。
            0..=200,
            5,
            move |value, cx| {
                let percent = if (1..90).contains(&value) { 100 } else { value };
                let mut next = settings.clone();
                next.line_height_percent = (percent > 0).then_some(percent as u32);
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let padding_row = {
        let settings = settings.clone();
        let value = settings.padding.map_or(0, i64::from);
        let shown: SharedString = if value == 0 {
            tr(cx, "跟随主题").into()
        } else {
            trf(cx, "{} 像素", &[&value.to_string()]).into()
        };
        stepper_row(
            cx,
            "duck-settings-term-padding",
            "内边距",
            Some("画布四周的留白，2px 一档。0 = 跟随主题。"),
            shown,
            value,
            // 上限 24：再大就是拿留白换列数，终端里没人这么用。
            0..=24,
            2,
            move |value, cx| {
                let mut next = settings.clone();
                next.padding = (value > 0).then_some(value as u32);
                save(cx, KV_TERMINAL, &next);
            },
        )
    };

    let display_reset = reset_row(
        cx,
        "duck-settings-term-reset-display",
        "恢复本节的默认值",
        |cx| {
            let mut next = terminal_settings(cx);
            let defaults = TerminalSettings::default();
            next.font_size = defaults.font_size;
            next.font_family = defaults.font_family;
            next.line_height_percent = defaults.line_height_percent;
            next.padding = defaults.padding;
            save(cx, KV_TERMINAL, &next);
        },
    );
    let display_section = settings_section(
        Some(tr(cx, "显示").into()),
        [
            font_row,
            font_family_row,
            font_fallbacks_row,
            font_preview,
            line_height_row,
            padding_row,
            display_reset,
        ],
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

    let encoding_options: Vec<(SharedString, SharedString)> = Encoding::ALL
        .iter()
        .map(|encoding| {
            (
                SharedString::from(encoding.value()),
                encoding_label(cx, *encoding),
            )
        })
        .collect();
    let encoding_current = SharedString::from(
        settings
            .encoding
            .as_deref()
            .and_then(Encoding::from_value)
            .unwrap_or_default()
            .value(),
    );
    let encoding_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "编码"),
            Some(
                tr(
                    cx,
                    "自动档按字节流特征在 UTF-8 与 GB18030（GBK / GB2312 通吃）之间判别，也可强制指定。对新会话生效，当前会话可在标签右键菜单里立刻切换。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-term-encoding",
                encoding_options,
                encoding_current,
                move |value: SharedString, _, cx| {
                    let Some(encoding) = Encoding::from_value(value.as_ref()) else {
                        return;
                    };
                    let mut next = settings.clone();
                    next.encoding = Some(encoding.value().to_owned());
                    save(cx, KV_TERMINAL, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let suggest_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "智能提示"),
            Some(
                tr(
                    cx,
                    "输入时按本机历史、内置命令与本地路径给建议，→ 或 Tab 接受。按 host 隔离历史，SSH 与本地互不串。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-term-suggest",
                settings.suggest,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.suggest = *on;
                    save(cx, KV_TERMINAL, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let shell_integration_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "注入 shell 集成脚本"),
            Some(
                tr(
                    cx,
                    "给没装 shell 集成的 bash / zsh 补发命令边界标记，让提示符判定与历史采集更精确。会在会话开始时发送一行初始化命令，默认关。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-term-shell-integration",
                settings.suggest_shell_integration,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.suggest_shell_integration = *on;
                    save(cx, KV_TERMINAL, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let session_reset = reset_row(
        cx,
        "duck-settings-term-reset-session",
        "恢复本节的默认值",
        |cx| {
            let mut next = terminal_settings(cx);
            let defaults = TerminalSettings::default();
            next.scrollback = defaults.scrollback;
            next.auto_reconnect = defaults.auto_reconnect;
            next.reconnect_max_attempts = defaults.reconnect_max_attempts;
            next.reconnect_backoff_secs = defaults.reconnect_backoff_secs;
            next.encoding = defaults.encoding;
            next.suggest = defaults.suggest;
            next.suggest_shell_integration = defaults.suggest_shell_integration;
            save(cx, KV_TERMINAL, &next);
        },
    );
    let session_section = settings_section(
        Some(tr(cx, "会话").into()),
        [
            scrollback_row,
            reconnect_row,
            attempts_row,
            backoff_row,
            encoding_row,
            suggest_row,
            shell_integration_row,
            session_reset,
        ],
        cx,
    );

    // ── 输入 ──
    let paste_options: Vec<(SharedString, SharedString)> = PasteConfirm::ALL
        .iter()
        .map(|mode| {
            let text = match mode {
                PasteConfirm::Always => tr(cx, "总是确认").to_owned(),
                PasteConfirm::Multiline => tr(cx, "仅多行时确认").to_owned(),
                PasteConfirm::Never => tr(cx, "从不确认").to_owned(),
            };
            (SharedString::from(mode.value()), SharedString::from(text))
        })
        .collect();
    let paste_current = SharedString::from(settings.paste_confirm.value());
    let paste_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "粘贴确认"),
            Some(
                tr(
                    cx,
                    "多行文本里的每个换行都会立刻执行。确认框里可以把换行折成空格再粘。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-term-paste-confirm",
                paste_options,
                paste_current,
                move |value: SharedString, _, cx| {
                    let Some(mode) = PasteConfirm::from_value(value.as_ref()) else {
                        return;
                    };
                    let mut next = settings.clone();
                    next.paste_confirm = mode;
                    save(cx, KV_TERMINAL, &next);
                },
                cx,
            ),
            cx,
        )
        .into_any_element()
    };

    let input_reset = reset_row(
        cx,
        "duck-settings-term-reset-input",
        "恢复本节的默认值",
        |cx| {
            let mut next = terminal_settings(cx);
            next.paste_confirm = TerminalSettings::default().paste_confirm;
            save(cx, KV_TERMINAL, &next);
        },
    );
    let input_section = settings_section(Some(tr(cx, "输入").into()), [paste_row, input_reset], cx);

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(display_section)
        .child(session_section)
        .child(input_section)
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
            move |value, _window, cx| {
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
    /// 每天最多单独提醒几条（**0 = 不限**）。简报不占这个额度。
    ///
    /// 默认 0：升级到这一版不改变原有行为 —— 上限是新增的手动闸门，不是新政策。
    pub daily_limit: i64,
    /// 每日简报（一场汇总，不是逐条提醒）。
    pub daily_brief: bool,
    /// 每日简报的时刻（`HH:MM`，本机时区）。
    pub daily_at: String,
    /// 每周简报。
    pub weekly_brief: bool,
    /// 每周简报的时刻（`HH:MM`）。周报固定**周一**发（见 `remind::brief_due`）。
    pub weekly_at: String,
}

impl Default for ProjectsSettings {
    fn default() -> Self {
        Self {
            reminders: true,
            // 默认值只有一处真源：判定逻辑就在 `remind` 里，天数不该在这里再写一遍。
            stale_days: super::projects::remind::STALE_DAYS,
            daily_limit: 0,
            // 简报默认开：这是「提醒」这一族里唯一一条**每天只有一次**的消息，
            // 而用户打开这一页正是想让它按时说一句。
            daily_brief: true,
            daily_at: "21:00".to_owned(),
            weekly_brief: true,
            weekly_at: "09:00".to_owned(),
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
                    "到期前一天、当天与逾期时各推一条应用内提示与系统通知；填了截止时刻的任务改为到期前一小时提醒。同一件事一天只说一次。",
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

    let limit_row = {
        let settings = settings.clone();
        number_setting_row(
            cx,
            "duck-settings-projects-daily-limit",
            "每天最多提醒几条",
            Some("到量之后当天不再逐条提醒（下方的简报不受影响）；0 表示不限。"),
            tr(cx, "条"),
            settings.daily_limit,
            0,
            move |value, cx| {
                let mut next = settings.clone();
                next.daily_limit = value;
                save(cx, KV_PROJECTS, &next);
            },
        )
    };

    let section = settings_section(
        Some(tr(cx, "提醒").into()),
        [remind_row, stale_row, limit_row],
        cx,
    );

    // ── 简报（G-PRJ-13）──────────────────────────────────────────────
    // 与上面三条的区别：上面那组是「有事就说」，这一组是「每天 / 每周到点说一句汇总」。
    // 所以单独成组 —— 关掉逐条提醒不该连带关掉简报（它们是两个意图）。
    let daily_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "每日简报"),
            Some(
                tr(
                    cx,
                    "每天到点推一条汇总：今天到期多少、逾期多少，再接前三条待办。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-projects-daily-brief",
                settings.daily_brief,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.daily_brief = *on;
                    save(cx, KV_PROJECTS, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let daily_at_row = {
        let settings = settings.clone();
        text_setting_row(
            cx,
            "duck-settings-projects-daily-at",
            "每日简报时刻",
            Some("按本机时区。过了这个点才发；当时没开着应用，就在下次启动时补一条。"),
            settings.daily_at.clone().into(),
            "例如：21:00",
            "填 24 小时制的 HH:MM；不合法的写法会保留原值。",
            move |text, _, cx| {
                let Some(at) = super::projects::remind::normalize_time(text) else {
                    // 坏时刻不写盘：写进去等于让这一期永远不发，而界面上看不出来。
                    return;
                };
                let mut next = settings.clone();
                next.daily_at = at;
                save(cx, KV_PROJECTS, &next);
            },
        )
    };

    let weekly_row = {
        let settings = settings.clone();
        option_row(
            tr(cx, "每周简报"),
            Some(
                tr(
                    cx,
                    "每周一推一条汇总：本周完成多少、逾期多少，仍在进行中多少。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-projects-weekly-brief",
                settings.weekly_brief,
                move |on: &bool, _, cx| {
                    let mut next = settings.clone();
                    next.weekly_brief = *on;
                    save(cx, KV_PROJECTS, &next);
                },
            ),
            cx,
        )
        .into_any_element()
    };

    let weekly_at_row = {
        let settings = settings.clone();
        text_setting_row(
            cx,
            "duck-settings-projects-weekly-at",
            "每周简报时刻",
            Some("固定周一发；过了这个点才发，当天没开应用就在下次启动时补一条。"),
            settings.weekly_at.clone().into(),
            "例如：09:00",
            "填 24 小时制的 HH:MM；不合法的写法会保留原值。",
            move |text, _, cx| {
                let Some(at) = super::projects::remind::normalize_time(text) else {
                    return;
                };
                let mut next = settings.clone();
                next.weekly_at = at;
                save(cx, KV_PROJECTS, &next);
            },
        )
    };

    let brief_section = settings_section(
        Some(tr(cx, "简报").into()),
        [daily_row, daily_at_row, weekly_row, weekly_at_row],
        cx,
    );

    v_flex()
        .w_full()
        .gap(tokens.spacing.lg)
        .child(section)
        .child(brief_section)
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

/// 一行**行内步进**的数值设置：`−  值  +`，点一次立刻落盘。
///
/// 与 [`number_setting_row`] 的分工按「调的时候要不要看效果」划：
/// - 需要精确输入、调完再看结果（重连上限、退避秒数）→ 弹窗；
/// - 边调边看（行高、内边距）→ 这个。弹窗会把「调一点看看」打断成
///   「开窗 → 输入 → 确定 → 关窗」四步，眼睛离开的正是要看的那块画布。
///
/// 到边界时按钮置灰而不是静默不动：点了没反应比点不动更让人怀疑程序坏了。
#[allow(clippy::too_many_arguments)]
fn stepper_row(
    cx: &App,
    id: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    shown: SharedString,
    value: i64,
    range: std::ops::RangeInclusive<i64>,
    step: i64,
    apply: impl Fn(i64, &mut App) + 'static,
) -> AnyElement {
    let tokens = active_theme(cx).tokens().clone();
    let apply = Rc::new(apply);
    let at_low = value <= *range.start();
    let at_high = value >= *range.end();
    let mut stepper = h_flex().items_center().gap(tokens.spacing.xs);

    for (delta, disabled) in [(-step, at_low), (step, at_high)] {
        let apply = Rc::clone(&apply);
        let (icon, label) = if delta < 0 {
            (DuckIcon::Minus, tr(cx, "减小"))
        } else {
            (DuckIcon::Plus, tr(cx, "增大"))
        };
        let next = (value + delta).clamp(*range.start(), *range.end());
        stepper = stepper.child(
            icon_button(
                SharedString::from(format!("{id}-{}", if delta < 0 { "dec" } else { "inc" })),
                label,
                Icon::new(icon).size(tokens.typography.sm.size),
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(disabled)
            .on_click(move |_, _, cx| {
                if !disabled {
                    apply(next, cx);
                }
            }),
        );
    }

    option_row(
        tr(cx, title),
        description.map(|text| SharedString::from(tr(cx, text))),
        stepper.child(body_text(shown, cx)),
        cx,
    )
    .into_any_element()
}

/// 一节的「恢复默认」行。
///
/// **只重置本节**：以当前设置为底，只把本节那几项换成 `Default`，别的节原样保留。
/// 直接写 `TerminalSettings::default()` 会把别的节一起清掉 —— 用户只是想重置行高，
/// 结果连回滚行数、重连策略都没了，那是撤不回来的意外。
///
/// 出场默认值一律取自 `Default::default()`（也就是 `load()` 的回落值），
/// 这里不另写一份字面量：**默认值只有一处真源**，改默认就改 `Default`。
fn reset_row(
    cx: &App,
    id: &'static str,
    title: &'static str,
    on_reset: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let restore = button(id, tr(cx, "恢复默认"), ButtonVariant::Secondary, cx).on_click(
        move |_, window, cx| {
            on_reset(cx);
            notify::info(tr(cx, "已恢复本节的默认值。"), window, cx);
        },
    );
    option_row(
        tr(cx, title),
        Some(tr(cx, "把本节各项恢复为出厂默认，其它节不受影响。").into()),
        restore,
        cx,
    )
    .into_any_element()
}

/// 一行数字设置：展示当前值 + 「编辑」→ 数字输入弹窗。
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
    apply: impl Fn(&str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let apply = Rc::new(apply);
    value_row(cx, id, title, description, shown, move |window, cx| {
        let apply = Rc::clone(&apply);
        let confirm: TextConfirm = Rc::new(move |text, window, cx| apply(text, window, cx));
        open_text_prompt(window, cx, title, title, placeholder, Some(hint), confirm);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_follows_the_theme_when_unset() {
        // `None` = 跟随主题：这是「⌘0 复位」的落点，不能变成某个写死的数。
        let settings = TerminalSettings::default();
        assert_eq!(effective_font_size(&settings, 13., 1.), 13.);
        assert_eq!(effective_font_size(&settings, 21.5, 1.), 21.5);
        // 跟随主题时主题档**已经**含界面缩放（`scale_typography`），不能再乘一遍 ——
        // 乘了就是平方，1.5 倍界面下终端字会大得离谱。
        assert_eq!(effective_font_size(&settings, 19.5, 1.5), 19.5);
    }

    #[test]
    fn font_size_is_clamped_into_the_single_range() {
        // 存档里可能有手改过的越界值（或旧版本更宽的区间）：读出来必须夹回区间，
        // 否则画布会按 99 磅排版，一屏只剩三个字。
        let mut settings = TerminalSettings::default();
        settings.font_size = Some(99);
        assert_eq!(
            effective_font_size(&settings, 13., 1.),
            *TERMINAL_FONT_SIZE_RANGE.end() as f32
        );
        settings.font_size = Some(1);
        assert_eq!(
            effective_font_size(&settings, 13., 1.),
            *TERMINAL_FONT_SIZE_RANGE.start() as f32
        );
    }

    /// 回退链的解析：中英文逗号都收、空项丢掉、全空必须是 `None`
    /// （`None` = 按系统级联；`Some(vec![])` 会让某些后端干脆不回退）。
    #[test]
    fn fallback_list_is_split_on_both_commas() {
        let mut settings = TerminalSettings::default();
        assert!(font_fallbacks(&settings).is_none(), "留空 = 交给系统级联");
        settings.font_fallbacks = " Sarasa Mono SC ，PingFang SC ,, ".to_owned();
        let list = font_fallbacks(&settings).expect("有回退链");
        assert_eq!(list.fallback_list(), ["Sarasa Mono SC", "PingFang SC"]);
    }

    /// 显式字号要**跟着界面缩放走**：界面 150% 时别的字都大了，终端不放大就是全屏最小处。
    /// 钳位发生在缩放**之前**（区间描述的是用户能选什么，不是屏幕上的磅值）。
    #[test]
    fn explicit_font_size_follows_the_ui_scale() {
        let mut settings = TerminalSettings::default();
        settings.font_size = Some(14);
        assert_eq!(effective_font_size(&settings, 13., 1.), 14.);
        assert_eq!(effective_font_size(&settings, 13., 1.5), 21.);
        // 越界值先夹回 32，再乘缩放：32 × 1.5 = 48，而不是把 99 × 1.5 夹到 32。
        settings.font_size = Some(99);
        assert_eq!(
            effective_font_size(&settings, 13., 1.5),
            *TERMINAL_FONT_SIZE_RANGE.end() as f32 * 1.5
        );
        // 缩放的 0 值不能让字号归零（画布会算不出列宽）。
        assert!(effective_font_size(&settings, 13., 0.) > 0.);
    }
}
