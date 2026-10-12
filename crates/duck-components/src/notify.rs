//! 通知：应用内 toast 与操作系统通知中心两条投递通道。
//!
//! 与 FluxDown 一致，不做额外的队列封装 —— 直接把 `gpui-component` 的通知推给根视图，
//! 由 `Root`（[`gpui_component::Root`]）负责堆叠与生命周期。「前后端」在这里体现为
//! **投递通道**：应用内是渲染层 toast；系统通道走 [`NotificationDelivery::System`]，
//! 由 gpui 投递到操作系统通知中心（macOS 要求进程已打包为 `.app` 且用户已授权；
//! Windows 要求进程有 AppUserModelID，见 `duck-app` 启动时的 `set_app_identity`）。
//!
//! **没有第三条「声音」通道**：`gpui-component` 的通知没有音效 API（`Notification` 上
//! 只有 `message/info/success/warning/error/…` 那一串构造与 `delivery`），要加就得自己
//! 接一个音频后端。与其摆一个按不动的开关，不如把通道如实收成两条。
//!
//! 桌面壳的所有窗口都必须挂在 `Root` 上，否则通知推不出去。
//!
//! 应用内 toast 不用库的 `type_` 默认图标：那条路径把图标绝对定位在 `top(18px)`
//! （按 16px rem 硬编码），而本项目 rem = 13px，图标会掉到首行文字下方约 5px。
//! 改用 [`Notification::content`] 自绘「图标 + 文字」行，图标相对首行行盒垂直居中。

use std::collections::VecDeque;
use std::time::SystemTime;

use gpui::{
    App, ElementId, FontWeight, Global, Hsla, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, IconName, WindowExt as _, h_flex,
    notification::{Notification, NotificationDelivery},
    v_flex,
};

use duck_theme::active_theme;

/// 通知语义级别：决定图标与颜色。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    /// 提示信息。
    #[default]
    Info,
    /// 成功。
    Success,
    /// 警告。
    Warning,
    /// 错误。
    Error,
}

impl Tone {
    /// 语义图标：内容件（提示条、状态点）与 toast 共用同一套，避免两处各挑一个。
    #[must_use]
    pub fn icon_name(self) -> IconName {
        match self {
            Tone::Info => IconName::Info,
            Tone::Success => IconName::CircleCheck,
            Tone::Warning => IconName::TriangleAlert,
            Tone::Error => IconName::CircleX,
        }
    }

    /// 语义色：信息 / 成功 / 警告取扩展 token，错误取危险色（与按钮的 Destructive 同源）。
    #[must_use]
    pub fn color(self, cx: &App) -> Hsla {
        let theme = active_theme(cx);
        match self {
            Tone::Info => theme.extended().colors.info,
            Tone::Success => theme.extended().colors.success,
            Tone::Warning => theme.extended().colors.warning,
            Tone::Error => theme.tokens().colors.destructive,
        }
    }
}

/// 构造应用内 toast：图标与文字在同一行盒内顶对齐，图标相对首行垂直居中。
///
/// 标题（可选）半粗、正文常规，字号 / 行高都取主题 `typography.sm`（13/18 基线），
/// 图标取 `icon.lg`（16px），间距取 `spacing.sm`（8px），全部随界面缩放。
///
/// 文本走 `content` 而不是 `title`/`message`，而这两者对**系统通道**才算文本
/// （见 [`system_with_toast`]）—— 所以这条通知显式声明只投应用内，免得哪天主题把
/// 默认投递方式改成 `InAppAndSystem` 时，一条没有 `title`/`message` 的通知被
/// 系统通道静默丢掉。
fn in_app(
    tone: Tone,
    title: Option<SharedString>,
    message: impl Into<SharedString>,
) -> Notification {
    let message = message.into();
    Notification::new()
        .delivery(NotificationDelivery::InApp)
        .content(move |_, _, cx| {
            let theme = active_theme(cx);
            let body = theme.tokens().typography.sm;
            let icon_size = theme.extended().icon.lg;
            // 图标相对首行行盒垂直居中：行高与图标差值的一半；行高更小时贴顶。
            let icon_offset = ((body.line_height - icon_size) / 2.).max(px(0.));
            let gap = theme.tokens().spacing.sm;
            let color = tone.color(cx);
            let icon_name = tone.icon_name();

            h_flex()
                .items_start()
                .gap(gap)
                .child(
                    div()
                        .mt(icon_offset)
                        .child(Icon::new(icon_name).size(icon_size).text_color(color)),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .gap(theme.tokens().spacing.xxs)
                        .when_some(title.clone(), |this, title| {
                            this.child(
                                div()
                                    .text_size(body.size)
                                    .line_height(body.line_height)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                        })
                        .child(
                            div()
                                .text_size(body.size)
                                .line_height(body.line_height)
                                .child(message.clone()),
                        ),
                )
                .into_any_element()
        })
}

/// 应用内横幅的抑制档位（设置 → 通知）。
///
/// 三档而不是一个开关，是因为「全部静音」和「只想少看点成功提示」是两种需求：
/// 前者是个明确的取舍（连错误也不弹），后者才是大多数人的默认偏好。
/// 无论哪一档，**通知历史照记**——这也是关掉横幅之后仍然看得到结论的唯一途径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BannerLevel {
    /// 全部提示都弹横幅。
    #[default]
    All,
    /// 只弹警告与错误；提示与成功只进历史。
    WarningsAndErrors,
    /// 一条横幅都不弹，全部只进历史。
    Off,
}

impl BannerLevel {
    /// 三档，由松到紧（下拉的顺序也是这个）。
    pub const ALL: [Self; 3] = [Self::All, Self::WarningsAndErrors, Self::Off];

    /// 落到 `kv` 里的稳定值。
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::WarningsAndErrors => "warn",
            Self::Off => "off",
        }
    }

    /// 稳定值 → 档位；认不出来给 `None`（调用方回落到默认档）。
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.wire() == value)
    }

    /// 这一档要不要为某个级别弹横幅。
    #[must_use]
    pub const fn allows(self, tone: Tone) -> bool {
        match self {
            Self::All => true,
            Self::WarningsAndErrors => matches!(tone, Tone::Warning | Tone::Error),
            Self::Off => false,
        }
    }
}

/// 通知的两条投递通道开关（App 级）。
///
/// 存在 `duck-components` 而不是读 `kv`：这一层不该依赖 `duck_store`。设置由 `duck-app`
/// 启动时读一次、改动时写一次（见 `settings::apply_notify_channels`），推给它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channels {
    /// 应用内横幅的抑制档位。
    pub banners: BannerLevel,
    /// 是否投递到操作系统通知中心。
    pub system: bool,
}

impl Default for Channels {
    /// 缺省全开：新装的应用不该悄悄少弹东西。
    fn default() -> Self {
        Self {
            banners: BannerLevel::All,
            system: true,
        }
    }
}

/// 通道开关的 App 级存储；`duck-app` 在启动时与设置变更时写入。
#[derive(Default)]
struct ChannelsGlobal(Channels);

impl Global for ChannelsGlobal {}

/// 更新通道开关。
///
/// 设置页改档位后立刻调它，**不必等重启**——改完还要重启才生效的开关是假开关。
pub fn set_channels(channels: Channels, cx: &mut App) {
    let current = cx.default_global::<ChannelsGlobal>();
    if current.0 == channels {
        return;
    }
    current.0 = channels;
    cx.refresh_windows();
}

/// 当前通道开关。
///
/// 还没被设置过时给 [`Channels::default`]（全开）：通知模块可能在任何窗口出现之前就被
/// 调用（启动早期），那一刻「什么都不弹」是错的默认。
#[must_use]
pub fn channels(cx: &App) -> Channels {
    cx.try_global::<ChannelsGlobal>()
        .map_or_else(Channels::default, |global| global.0)
}

/// 这个级别该不该弹应用内横幅。
fn banners_allow(tone: Tone, cx: &App) -> bool {
    channels(cx).banners.allows(tone)
}

/// 直接推送一条通知。
pub fn push(notification: impl Into<Notification>, window: &mut Window, cx: &mut App) {
    window.push_notification(notification.into(), cx);
}

/// 走应用内横幅通道（尊重 [`BannerLevel`]）。
///
/// `info` / `success` / `warning` / `error` / `sticky` 共用这一处判据 —— 五处各写一遍
/// `if` 迟早会有一处漏掉，而漏掉的那处就是「明明关了还在弹」。
fn push_banner(
    tone: Tone,
    title: Option<SharedString>,
    message: SharedString,
    id: Option<ElementId>,
    window: &mut Window,
    cx: &mut App,
) {
    if !banners_allow(tone, cx) {
        return;
    }
    let mut notification = in_app(tone, title, message);
    if let Some(id) = id {
        notification = notification.id1::<StickyKey>(id);
    }
    push(notification, window, cx);
}

/// 应用内提示。
pub fn info(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    record(Tone::Info, None, message.clone(), cx);
    push_banner(Tone::Info, None, message, None, window, cx);
}

/// 应用内成功提示。
pub fn success(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    record(Tone::Success, None, message.clone(), cx);
    push_banner(Tone::Success, None, message, None, window, cx);
}

/// 应用内警告提示；约 5 秒自动消失（错过可在通知历史里回看）。
pub fn warning(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    record(Tone::Warning, None, message.clone(), cx);
    push_banner(Tone::Warning, None, message, None, window, cx);
}

/// 应用内错误提示；约 5 秒自动消失（错过可在通知历史里回看）。
pub fn error(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    record(Tone::Error, None, message.clone(), cx);
    push_banner(Tone::Error, None, message, None, window, cx);
}

/// [`sticky`] 的 id 命名空间：与库内其它通知（随机 uuid）互不碰撞。
struct StickyKey;

/// 可替换提示，**按 `key` 替换**：同一个 key 再次推送会替换上一条，而不是再堆一张。
///
/// 给「反复触发也只有一条结论」的场景用（检查更新失败、保存设置失败……）。
/// 每条通知的默认 id 是随机 uuid，用户连点几次「检查更新」就会在窗口右上角
/// 堆成一列红条；固定 key 让同一动作永远只占一位。约 5 秒自动消失。
pub fn sticky(
    key: impl Into<ElementId>,
    tone: Tone,
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let message = message.into();
    record(tone, None, message.clone(), cx);
    push_banner(tone, None, message, Some(key.into()), window, cx);
}

/// 仅投递到操作系统通知中心，应用内不显示。
///
/// 系统通道关掉时**什么都不推**，但历史照记 —— 它仍然是「刚才发生了什么」的唯一记录。
pub fn system(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    let message = message.into();
    record(Tone::Info, Some(title.clone()), message.clone(), cx);
    if !channels(cx).system {
        return;
    }
    push(system_notification(title, message), window, cx);
}

/// 同时投递：应用内 toast + 操作系统通知中心。
///
/// 两条通道**各自**看自己的开关：只关了系统通道就只弹横幅，只关了横幅就只投系统，
/// 都关了则只剩历史。任意组合下都不会「因为另一条关着而把这条也吞了」。
///
/// **拆成两条通知推，不用库的 [`NotificationDelivery::InAppAndSystem`]**：那个变体
/// 要求同一条通知既带 `title`/`message`（系统通道取文本）又带 `content`（应用内自绘
/// 图标，见 [`in_app`]），而库的渲染会把这三者**依次**塞进同一个 `v_flex` —— 标题与
/// 正文会在 toast 里出现两遍。拆开之后各取所需：系统那条只有文本，因此不建 toast；
/// 应用内那条只有自绘内容，因此系统通道取不到文本、直接不发。
pub fn system_with_toast(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    let message = message.into();
    record(Tone::Info, Some(title.clone()), message.clone(), cx);
    if channels(cx).system {
        push(
            system_notification(title.clone(), message.clone()),
            window,
            cx,
        );
    }
    if banners_allow(Tone::Info, cx) {
        push(in_app(Tone::Info, Some(title), message), window, cx);
    }
}

/// 只走系统通道的那份通知。两个入口共用，免得两处各拼一遍投递方式。
fn system_notification(title: SharedString, message: SharedString) -> Notification {
    Notification::info(message)
        .title(title)
        .delivery(NotificationDelivery::System)
}

/// 通知历史上限：超过后丢最旧的（只增不改的环形截断）。
const HISTORY_LIMIT: usize = 100;

/// 一条通知的历史快照（投递时定格，渲染不再变动）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEntry {
    /// 单调递增序号：面板删条目要靠它定位（消息与时间都可能重复，序号不会）。
    id: u64,
    tone: Tone,
    title: Option<SharedString>,
    message: SharedString,
    at: SystemTime,
    /// 是否已被「打开面板」读过：决定删掉它时未读角标要不要跟着减。
    read: bool,
}

impl NotificationEntry {
    /// 历史内唯一序号（删除入口的定位键，也是列表元素 id 的来源）。
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// 通知语义级别（图标与颜色）。
    #[must_use]
    pub const fn tone(&self) -> Tone {
        self.tone
    }

    /// 标题（可选）。
    #[must_use]
    pub const fn title(&self) -> &Option<SharedString> {
        &self.title
    }

    /// 正文。
    #[must_use]
    pub const fn message(&self) -> &SharedString {
        &self.message
    }

    /// 投递时刻。
    #[must_use]
    pub const fn at(&self) -> SystemTime {
        self.at
    }
}

/// 通知历史（App 级、仅内存）：环形上限 [`HISTORY_LIMIT`] 条，重启清空。
///
/// toast 只有约 5 秒的展示窗口，自动消失后用户需要一个回看的入口；
/// 这里是唯一的事实来源，顶栏铃铛（duck-shell）从这里读。
#[derive(Default)]
struct HistoryGlobal {
    entries: VecDeque<NotificationEntry>,
    /// 未读条数：打开历史面板时一次性清零。
    unread: usize,
    /// 序号源，只增不减 —— 删掉的 id 不复用，免得面板里残留的旧 id 复活到新条目上。
    next_id: u64,
}

impl Global for HistoryGlobal {}

/// 把一条通知记入历史并催一次重绘（顶栏未读角标要跟着变）。
fn record(tone: Tone, title: Option<SharedString>, message: SharedString, cx: &mut App) {
    let history = cx.default_global::<HistoryGlobal>();
    let id = history.next_id;
    history.next_id += 1;
    history.entries.push_back(NotificationEntry {
        id,
        tone,
        title,
        message,
        at: SystemTime::now(),
        read: false,
    });
    history.unread += 1;
    while history.entries.len() > HISTORY_LIMIT {
        history.entries.pop_front();
    }
    cx.refresh_windows();
}

/// 通知历史（新→旧）。
#[must_use]
pub fn history(cx: &App) -> Vec<NotificationEntry> {
    cx.try_global::<HistoryGlobal>()
        .map(|history| history.entries.iter().rev().cloned().collect())
        .unwrap_or_default()
}

/// 未读通知条数（顶栏铃铛的角标）。
#[must_use]
pub fn unread_count(cx: &App) -> usize {
    cx.try_global::<HistoryGlobal>()
        .map_or(0, |history| history.unread)
}

/// 全部标记为已读（打开历史面板时调用）。
pub fn mark_history_read(cx: &mut App) {
    let history = cx.default_global::<HistoryGlobal>();
    if history.unread != 0 {
        for entry in &mut history.entries {
            entry.read = true;
        }
        history.unread = 0;
        cx.refresh_windows();
    }
}

/// 从历史里删掉一条通知，按 [`NotificationEntry::id`] 定位；返回是否真的删掉了。
///
/// 面板的逐条删除入口。未读角标只有在删掉**未读**那条时才减一位 —— 删一条早就读过
/// 的旧通知不该顺手把角标抹掉，否则「清掉一条不想要的旧记录」会把新来的提示也标成已读。
pub fn remove_from_history(id: u64, cx: &mut App) -> bool {
    let history = cx.default_global::<HistoryGlobal>();
    let Some(index) = history.entries.iter().position(|entry| entry.id == id) else {
        return false;
    };
    let removed = history.entries.remove(index);
    if removed.is_some_and(|entry| !entry.read) {
        history.unread = history.unread.saturating_sub(1);
    }
    cx.refresh_windows();
    true
}

/// 清空通知历史。
pub fn clear_history(cx: &mut App) {
    let history = cx.default_global::<HistoryGlobal>();
    if !history.entries.is_empty() || history.unread != 0 {
        history.entries.clear();
        history.unread = 0;
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::{BannerLevel, Channels, Tone};

    /// 落盘值必须能往返：它与 `kv` 里的字符串是同一个东西。
    #[test]
    fn every_banner_level_round_trips_through_its_wire_name() {
        for level in BannerLevel::ALL {
            assert_eq!(BannerLevel::from_wire(level.wire()), Some(level));
        }
        assert_eq!(BannerLevel::from_wire("nope"), None);
        assert_eq!(BannerLevel::from_wire("ALL"), None, "大小写敏感");
        assert_eq!(BannerLevel::default(), BannerLevel::All);
    }

    /// 档位的语义：`WarningsAndErrors` 是「少看点」，不是「全静音」。
    #[test]
    fn the_middle_level_only_keeps_warnings_and_errors() {
        let all = BannerLevel::All;
        assert!(all.allows(Tone::Info) && all.allows(Tone::Error));

        let quiet = BannerLevel::WarningsAndErrors;
        assert!(!quiet.allows(Tone::Info));
        assert!(!quiet.allows(Tone::Success));
        assert!(quiet.allows(Tone::Warning));
        assert!(quiet.allows(Tone::Error));

        let off = BannerLevel::Off;
        for tone in [Tone::Info, Tone::Success, Tone::Warning, Tone::Error] {
            assert!(!off.allows(tone), "关掉后不该再弹 {tone:?}");
        }
    }

    /// 缺省全开：新装的应用不该悄悄少弹东西。
    #[test]
    fn the_default_channels_are_wide_open() {
        let channels = Channels::default();
        assert_eq!(channels.banners, BannerLevel::All);
        assert!(channels.system);
    }
}
