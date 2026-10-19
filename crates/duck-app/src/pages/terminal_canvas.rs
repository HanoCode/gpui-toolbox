//! 终端画布：把 `duck_term::Session` 的网格画上屏，并把按键 / 滚轮送回内核。
//!
//! # 为什么不用 `overflow_scroll` 做回滚
//!
//! 终端的「往上翻」不是 DOM 滚动，而是**换一个视口起点**：`Term` 里 `display_offset`
//! 一变，整个可视区的内容就整体平移（`display_iter` 直接吐平移后的坐标）。
//! 因此这里用 `overflow_hidden` 固定住画布，滚轮事件转成 [`Session::scroll`]。
//!
//! # 每帧只做一次加锁读
//!
//! 网格读走 [`Session::try_with_term`]：拿不到锁（事件循环正在解析一大段输出）
//! 就沿用上一帧的内容。UI 线程因此**永远不会**等 PTY —— 下一帧的
//! [`SessionEvent::Wakeup`] 会再把它叫醒。取到的内容当场转成 `String + TextRun`，
//! 借用不越过闭包边界。
//!
//! # 尺寸对齐
//!
//! 列宽取自**字体度量**（`text_system` 里 `'m'` 的推进量，等宽字体下即单元格宽度），
//! 行高取自 `typography.mono_md` 的「行高 / 字号」比例 —— 两者都随字号与主题走，
//! 不比魔数。画布的真实像素尺寸由 `on_children_prepainted` 回传（它拿到的是子元素的
//! `Bounds`，比 `window.bounds()` 准 —— 后者含标题栏与侧栏）。
//!
//! # 焦点
//!
//! 画布是**自绘**的，没有原生输入控件，键盘全靠 `track_focus` + `on_key_down`。
//! 少了焦点就一个字都打不进去，所以有两个入口：鼠标点画布（`on_mouse_down`），
//! 以及切到本地标签时由 [`TerminalPage::sync_canvas_focus`] 主动接管一次。
//!
//! # 输入法（IME）
//!
//! `track_focus` + `on_key_down` 只覆盖「按键 → 字节」，**组合期（拼音候选）根本
//! 到不了这里**：macOS 要把按键交给输入法，前提是窗口里挂了一个 `InputHandler`
//!（`window.handle_input`）。本文件因此给每块画布挂一个空 `canvas` 元素，在它的
//! paint 回调里注册 [`ElementInputHandler`]，并让 `TerminalPage` 实现
//! [`gpui::EntityInputHandler`]：
//!
//! - 组合中：[`TerminalPage::replace_and_mark_text_in_range`] 把预编辑串存进会话并
//!   触发重绘，画布把它画在光标格上（`preedit` 覆盖层，与行 / 光标**共享同一原点**）；
//! - 上屏：[`TerminalPage::replace_text_in_range`] 把提交文本按 [`duck_term::encode`]
//!   同一条「文本通道」（`keymap.rs` 的第 3 步）写进 PTY；
//! - `⌘` 组合键仍由动作绑定优先吃掉（gpui 的派发顺序不变），输入法只在
//!   「没有绑定命中」时看到按键。
//!
//! 接口签名以当前 gpui 源码为准：`EntityInputHandler` 见
//! `gpui-pre-0.3.8/src/input.rs:13`，`ElementInputHandler::new` 见同文件 `:137`，
//! `Window::handle_input` 见 `gpui-pre-0.3.8/src/window.rs:5253`。
//!
//! # 键盘链路（两条铁律，改动前先读）
//!
//! 1. **冲突键必须走动作绑定**：gpui 的派发顺序是「先沿焦点链匹配 action 绑定并派发，
//!    全部未命中才把 `KeyDownEvent` 交给 `on_key_down`」（`window.rs::dispatch_key_event`）。
//!    而窗口根 `gpui_base::Root` 在 `"Root"` 上下文占着 `"tab"` / `"shift-tab"` /
//!    `"cmd-c"`（macOS）/ `"ctrl-c"`（非 macOS，`root.rs:15`）—— 不在**更深上下文**
//!    用同键动作盖掉，这些键永远到不了终端。键位表在 `app::install`，动作与处理器在本文件；
//!    参考实现是 zed 的 `"Terminal"` 上下文（`temp/zed/assets/keymaps/default-macos.json`
//!    里 "There are conflicting bindings for these keys in the global context" 那段注释）。
//! 2. **`on_key_down` 里要么写字节、要么什么都不做**：能走到那里说明没有任何绑定命中，
//!    此时带 ⌘ 的组合键含义是「用户按了一个本应用没绑的快捷键」—— 绝不能把字母本身写进
//!    shell。这条规则落在内核（[`duck_term::encode`] 的第 0 步）而不是这里：可单测，
//!    且新增调用点时漏填 `platform` 会编译报错。

use std::sync::Arc;
use std::time::Duration;

use duck_components::{
    ButtonVariant, DuckIcon, body_text, button, meta_text, notify, tabular_numbers, title_text,
};
use duck_term::{
    CellSize, Color, Column, CursorShape, Dimensions, Encoded, Encoding, Flags, KeyStroke, Line,
    MouseAction, MouseModifiers, Palette, Point as TermPoint, SelectionKind, SelectionRange,
    Session, SessionEvent, Side, SpawnOptions, TermLink, TermMode, TermMouseButton, encode,
    encode_focus, encode_mouse, prompt_bars, reports_motion, suggest as suggest_for,
};
use duck_theme::{ThemeMode, active_theme, tr, trf};
use gpui::{
    AnyElement, App, Context, CursorStyle, FocusHandle, Font, FontStyle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Rgba, Role, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, Styled as _,
    Subscription, TextRun, UnderlineStyle, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon,
    menu::{ContextMenuExt as _, PopupMenuItem},
    v_flex,
};

use super::terminal::suggest::{
    SuggestState, history_host, last_word, load_history, path_candidates, remember_command,
    save_history, suggest_popup,
};

use crate::pages::TerminalPage;
use crate::pages::terminal_data::TabKind;

/// 事件轮询间隔。16ms ≈ 一帧：本地 shell 的交互延迟主要由它决定，再密就是空转。
const POLL_INTERVAL: Duration = Duration::from_millis(16);
/// 一条会话都没有时的轮询间隔。终端页没打开时不该按帧空转 —— 只要还有弱引用活着，
/// 任务就不退出（用户随时可能再开一个本地标签），所以用慢速心跳把开销压到可忽略。
const IDLE_INTERVAL: Duration = Duration::from_millis(250);
/// 传统滚轮「一格」翻多少行。鼠标一次滚动的行数各平台不一，终端统一按 3 行 —— 与 alacritty 一致。
const SCROLL_LINES_PER_NOTCH: f32 = 3.;
/// 拖选越界时的自动滚动心跳。50ms 一档 ≈ 20 次/秒：比一帧一滚（60 行/秒）慢得多，
/// 停下来选到想要的那一行才有可能。
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);
/// 一次自动滚动最多滚几行（越界越远滚越多，但封顶）。
const AUTOSCROLL_MAX_ROWS: i32 = 4;
/// 自动重连**起始退避**的出厂默认：从断开到第一次尝试的等待
///（「设置 → 终端」的「起始退避」可改）。
pub(super) const RECONNECT_BACKOFF_START: Duration = Duration::from_secs(2);
/// 自动重连退避**上限**（每次翻倍，到此封顶）。
///
/// 不开放设置：它是「指数退避不会涨到天上去」的护栏，不是偏好。
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// 自动重连**最多尝试次数**的出厂默认；用尽即停，只保留手动「重连」入口
///（「设置 → 终端」的「重连上限」可改）。
pub(super) const RECONNECT_MAX_ATTEMPTS: u32 = 5;
/// 重连后的连接存活多久算「真的连上了」——越过它才把尝试计数清零。
///
/// 用「存活时长」而不是「收到输出」当健康判据：`Connection refused` 之类也会往
/// 屏幕上吐字，用输出判健康会把计数器一直清零，退化成无限重试。
const RECONNECT_HEALTHY_AFTER: Duration = Duration::from_secs(10);
/// 清屏序列：`ED 2` 清屏 + `ED 3` 清滚回缓冲 + `CUP` 归位。
pub(super) const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[3J\x1b[H";
/// 命令标记竖条的宽度（逻辑像素）。
///
/// 只有 2px 是刻意的：绝对定位的包含块是文字层的 wrapper，负 `left` 会被它的
/// `overflow_hidden` 裁掉，所以这条标记必然压在**第一列字符的最左侧**上。
/// 窄到 2px 時字形仍完整可读，又足以当一条「页边标」看。
const MARK_WIDTH: f32 = 2.;
/// 失败的命令标记用多浓的错误色。
///
/// 与 `drop_target`（20% 强调色）同一个量级：半透明层才压得住终端的两种底色
/// （深色主题近黑、浅色近白）而不遮字。
const MARK_FAILED_ALPHA: f32 = 0.22;
/// 光标形状（`Beam` / `Underline`）占单元格的比例。
const CURSOR_THICKNESS_RATIO: f32 = 0.12;

actions!(
    terminal,
    [
        // 全局冲突键：必须在 `"Terminal"` 上下文重绑，否则被 `gpui_base::Root` 抢走。
        SendTab,
        SendTabPrev,
        SendCtrlC,
        // 视口 / 字号。
        ZoomIn,
        ZoomOut,
        ZoomReset,
        Clear,
        // 分屏（左右两个窗格）。
        SplitPane,
        // 滚动族（对齐 zed `Terminal` 上下文的一整组 Scroll*）。
        ScrollLineUp,
        ScrollLineDown,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
        // 行编辑便利键（macOS 手感，zed 的 `SendKeystroke` 等价物）。
        MoveToLineStart,
        MoveToLineEnd,
        DeleteToLineStart,
        DeleteToLineEnd,
        DeleteWordForward,
        // 剪贴板：`⌘C` 在 `"Root"` 上下文被绑成 `Copy`（`root.rs:19`），
        // 不在终端里盖掉，⌘C 就永远到不了「复制选中的文本」这条语义上。
        CopySelection,
        PasteFromClipboard,
        SelectAll,
        // 标签：主流终端的 `⌘T` / `⌘W`（此前只写在设置页的快捷键说明里，一条都没绑）。
        NewTab,
        NewLocalTab,
        // `⌘N`：统一选择器（搜连接 / 直接连 / 新建）。**不是**直接弹 14 字段表单 ——
        // 临时连一台是高频动作，表单是它后面那一层。
        QuickConnect,
        CloseTab,
        ReopenClosedTab,
        // 按方向切标签（`⌃Tab` / `⌃⇧Tab` / `⌘⇧[` / `⌘⇧]`）。
        NextTab,
        PreviousTab,
        // 缓冲区搜索（`⌘F`）。
        Find,
        // SFTP / 文件面板（绑在页面级 `"TerminalPage"` 上下文，不是画布的 `"Terminal"`）：
        // 焦点在画布里时这几条不生效，两边互不抢键。
        EditPath,
        RefreshPane,
        NewFolderHere,
        DeleteEntry,
        CopyPath,
    ]
);

/// 切到第 N 张标签（`⌘1`..`⌘9`；越界是空操作）。
///
/// 用带字段的 action 而不是九条无参 action：键位表与处理器都只写一遍，
/// 标签数量变化不用同步改九个地方（与数据库页的 `SwitchTabAt` 同一套做法）。
///
/// ⚠️ `⌘1`..`⌘8` 全局是**路由切换**：这里在更深的 `"TerminalPage"` 上下文里覆盖，
/// 属于刻意的（主流终端里 `⌘1..9` 都是切标签）；离开终端页即恢复路由语义。
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = terminal, no_json)]
pub struct SwitchTabAt {
    /// 从 0 开始的标签下标。
    pub index: usize,
}

/// 重连快照：把「这条会话当初是怎么起来的」冻下来，断开后可原样重放。
///
/// **不读当时的最新技术参数**（档案可能已被编辑）：重连重放的是**这条会话**
/// 起进程时用的那条 argv（[`duck_ssh::Target::terminal_argv`]，见 `target.rs`），
/// 与 [`TerminalSession::spawn_ssh`] 走同一套拼装。`target: None` = 本地 shell。
struct RespawnPlan {
    /// SSH 目标；`None` = 本地 shell 会话。
    target: Option<duck_ssh::Target>,
    /// `ControlMaster` 复用 socket 目录（SSH 会话用；本地会话为空）。
    socket_dir: std::path::PathBuf,
    /// 起进程时的工作目录（本地会话可恢复；SSH 的远端 cwd 宿主无从得知，恒为 `None`）。
    cwd: Option<std::path::PathBuf>,
}

/// 自动重连的参数：从「设置 → 终端」读来的一次快照（纯数据，可单测）。
///
/// 做成值类型而不是每次现读 `kv`：一条会话的重连**跨多轮**进行（排定 → 触发 → 再断），
/// 中途用户改了设置不该把半途的退避序列改乱；快照也顺带让纯函数可测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReconnectPolicy {
    /// 最多尝试几次（用尽即落 `Stopped`，只保留手动入口）。
    pub max_attempts: u32,
    /// 第一次等待的时长（之后每次翻倍，封顶 [`RECONNECT_BACKOFF_MAX`]）。
    pub backoff_start: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: RECONNECT_MAX_ATTEMPTS,
            backoff_start: RECONNECT_BACKOFF_START,
        }
    }
}

impl ReconnectPolicy {
    /// 从模块偏好读一次（设置页「终端」的「重连上限 / 起始退避」）。
    pub(super) fn from_settings(cx: &App) -> Self {
        let prefs = super::module_settings::terminal_settings(cx);
        Self {
            // 0 次尝试会让自动重连名存实亡，用户真要关它有专门的开关；
            // 这里把 0 收敛成 1，避免「填了 0 结果这个功能悄悄没了」。
            max_attempts: prefs.reconnect_max_attempts.max(1),
            backoff_start: Duration::from_secs(u64::from(prefs.reconnect_backoff_secs.max(1))),
        }
    }
}

/// 自动重连的**调控器**：有界尝试 + 指数退避（纯数据，可单测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AutoReconnect {
    /// 已经发起过的尝试次数。
    attempts: u32,
    /// 下一次排定要用的退避时长（每次翻倍，封顶 [`RECONNECT_BACKOFF_MAX`]）。
    backoff: Duration,
    /// 最多尝试几次（来自 [`ReconnectPolicy`]，随调控器一起搬去新会话）。
    max_attempts: u32,
}

impl AutoReconnect {
    /// 起始调控器：0 次尝试、起始退避。
    const fn new(policy: ReconnectPolicy) -> Self {
        Self {
            attempts: 0,
            backoff: policy.backoff_start,
            max_attempts: policy.max_attempts,
        }
    }

    /// 按当前退避排定下一次尝试；返回 `None` = **次数用尽，停止自动重连**。
    ///
    /// 自身随即前进一格（次数 +1、退避翻倍封顶），所以同一调控器连续调用给出的是
    /// 递增的等待：2s、4s、8s、16s、32s（默认档），第 6 次调用返回 `None`。
    fn schedule(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        if self.attempts >= self.max_attempts {
            return None;
        }
        let due = now + self.backoff;
        self.attempts += 1;
        self.backoff = (self.backoff * 2).min(RECONNECT_BACKOFF_MAX);
        Some(due)
    }
}

/// 起会话时的回滚行数：设置页「终端」的模块偏好（出厂默认 = 内核的
/// [`SpawnOptions::default`]）。
fn scrollback(cx: &App) -> usize {
    super::module_settings::terminal_settings(cx).scrollback as usize
}

/// 起会话时的编码：设置页「终端」的模块偏好，未知值回落 UTF-8。
///
/// 与 `scrollback` 同一条规矩：偏好只有一份（`kv: terminal.settings`），
/// 会话不另存第二份 —— 之后的临时切换写在会话自己的 `EncodingHandle` 里。
fn terminal_encoding(cx: &App) -> Encoding {
    super::module_settings::terminal_settings(cx)
        .encoding
        .as_deref()
        .and_then(Encoding::from_value)
        .unwrap_or_default()
}

/// 终端使用的等宽字体族：设置页里填了就按它（去空白后为空 = 跟随主题），
/// 否则用主题的等宽字体。
pub(crate) fn mono_family(
    prefs: &super::module_settings::TerminalSettings,
    fallback: SharedString,
) -> SharedString {
    prefs
        .font_family
        .as_deref()
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .map_or(fallback, |family| SharedString::from(family.to_owned()))
}

/// 画布用的等宽 `Font`（族名 + 回退链 + 等宽数字特性）。
///
/// **度量与绘制必须用同一个 `Font`**：`sync_cell_metrics` 量 `'m'` 的推进量决定格宽，
/// `render_terminal_session` 按这个格宽排版。两处各拼一个 `Font` 时，只要有一处
/// 忘了带回退链 / 特性，量出来的格子与实际字形就是两套度量 —— 整屏错位，
/// 而且「只在用户改了字体设置之后」才出现。所以只留这一个构造出口。
pub(crate) fn mono_font(
    prefs: &super::module_settings::TerminalSettings,
    fallback_family: SharedString,
) -> Font {
    Font {
        family: mono_family(prefs, fallback_family),
        // 等宽数字：终端里数字宽度不一致会让整列错位。
        features: tabular_numbers(),
        // 回退链：主字体里没有的字（CJK、Nerd 图标）由它接手。留空 = 交给系统级联
        // （macOS 的 Menlo / SF Mono 本来就能级联到中日韩字体，所以缺省不改行为）。
        fallbacks: super::module_settings::font_fallbacks(prefs),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

/// 一条会话的自动重连状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReconnectState {
    /// 未进入自动重连（从未断开过，或已复位为健康）。
    #[default]
    Idle,
    /// 已排定下一次尝试时刻。
    Pending {
        driver: AutoReconnect,
        due: std::time::Instant,
    },
    /// 已发起过一次尝试，等这条新连接的结局（再断则接着排，存活够久则复位）。
    Armed { driver: AutoReconnect },
    /// 停止自动重连（错误不可重试，或次数用尽）。只保留手动入口。
    Stopped,
}

/// 判定一条自动重连状态是否「正在等待/进行中」（用于界面提示与「撤销」的范围）。
const fn reconnect_busy(state: ReconnectState) -> bool {
    matches!(
        state,
        ReconnectState::Pending { .. } | ReconnectState::Armed { .. }
    )
}

/// 一次断开后的**自动重连决策**（纯函数，可单测）。
///
/// 护栏全在这里，顺序也有讲究：
///
/// 1. 会话不具备重连资格（本地 shell）→ `Idle`，永不进入自动重连；
/// 2. `blocked`（认证 / 主机密钥类失败）或**已停**（次数用尽过）→ `Stopped`，
///    不排下一次（对同一账号反复认证可能被服务端拦下）；
/// 3. 否则带着上一轮的**次数与退避**继续排定；调控器说次数用尽就落 `Stopped`。
fn reconnect_after_disconnect(
    eligible: bool,
    blocked: bool,
    previous: ReconnectState,
    now: std::time::Instant,
    policy: ReconnectPolicy,
) -> ReconnectState {
    if !eligible {
        return ReconnectState::Idle;
    }
    if blocked || matches!(previous, ReconnectState::Stopped) {
        return ReconnectState::Stopped;
    }
    // `Armed` 来自上一次尝试；`Pending` 理论上不该出现（Pending 的会话不在跑），
    // 但真出现也不丢计数。
    let mut driver = match previous {
        ReconnectState::Armed { driver } | ReconnectState::Pending { driver, .. } => driver,
        ReconnectState::Idle | ReconnectState::Stopped => AutoReconnect::new(policy),
    };
    match driver.schedule(now) {
        Some(due) => ReconnectState::Pending { driver, due },
        None => ReconnectState::Stopped,
    }
}

/// 「主题指纹」：决定要不要把配色 / 字号重新灌进内核。
///
/// 为什么要连**颜色**一起算进来：`Palette` 的十六进制 16 色来自主题配色的默认值，
/// 而前景 / 背景 / 光标三项取自 `tokens.colors`。只看 `(深色?, 字号)` 的话，
/// 「浅色主题 A → 浅色主题 B」这类切换指纹不变 ⇒ 画布底色、文字色、光标色**都不更新**，
/// 只有退出重开标签才生效 —— 表现就是「换了主题，终端还是旧配色」。
/// 这种 a→b 同深色档的切换恰好是用户最常做的（换一个更柔和的浅色主题）。
type StyleKey = (bool, i32, [u8; 3], [u8; 3], [u8; 3]);

/// 由「深色档 + 字号 + 实际要灌进去的调色板」算出指纹。
///
/// 取的是**调色板里的值**而不是主题 token 的原始值：写入内核的就是这几个字节，
/// 比较它们才和「看起来变没变」严格对应。
fn style_key_of(dark: bool, font_size: f32, palette: &Palette) -> StyleKey {
    (
        dark,
        (font_size * 1000.) as i32,
        palette.foreground,
        palette.background,
        palette.cursor,
    )
}

/// 一个终端标签的运行时状态（本地 shell 与 SSH 会话共用）。
pub(super) struct TerminalSession {
    /// 内核会话。`None` = 起 shell 失败（只试一次，不在每帧重试）。
    handle: Option<Arc<Session>>,
    /// 键盘焦点句柄：画布要能收按键。
    focus: FocusHandle,
    /// 由字体度量算出的单元格尺寸，回传给内核做 `TIOCSWINSZ` 的像素字段。
    cell: CellSize,
    /// 当前网格（列, 行）。
    grid: (u16, u16),
    /// 滚轮的**未成整行**余量：触控板会给一堆不足一行的位移，
    /// 逐次四舍五入等于把慢速滑动整个吃掉。
    scroll_remainder: f32,
    /// 应用通过 `OSC 0/2` 设的标题。非空时盖掉标签上的静态名字（`vim foo.rs` 一眼可见）。
    title: Option<String>,
    /// 已灌进内核的配色。
    palette: Palette,
    /// 主题指纹（深色?, 字号千分位, 前景 / 背景 / 光标色），变了才重灌调色板。
    style_key: StyleKey,
    /// 子进程**已退出**。
    ///
    /// 本地 shell 退出即「这个标签的使命结束了」，直接关标签；
    /// 但 SSH 会话退出往往是**连接失败**（认证被拒、主机密钥不匹配）——
    /// 那种情形下 ssh 已经把原因打在画布上了，**必须留着标签**让用户看见，
    /// 否则表现就是「点了连接，标签一闪就没了」。
    exited: bool,
    /// 会话起始时刻，供状态栏显示真实连接时长。
    started: std::time::Instant,
    /// 画布**文字区**左上角在窗口坐标系里的位置。
    ///
    /// 鼠标像素 → 网格坐标的换算全靠它。取的是内层 `v_flex` 的 `Bounds`（即文字区，
    /// 已经扣掉了画布内边距）—— 与光标、与行共享同一个原点；
    /// 换成外层容器的原点就等于把内边距算两次，选中的格子会整体偏移一列/一行。
    origin: gpui::Point<Pixels>,
    /// 最近一次量到的文字区尺寸（与 [`Self::origin`] 同源，同一帧写入）。
    viewport: Size<Pixels>,
    /// **正在**本窗格里拖选（左键在这个画布上按下、还没松开）。
    ///
    /// 不能用「左键是否为按下态」代替：拖标签并入分屏时左键也是按下态，
    /// 指针会一路划过画布 —— 那会把一整片文字选上。判据必须落在
    /// 「这次拖拽是**从这块画布**开始的」。
    selecting: bool,
    /// 拖选**越出文字区**时的自动滚动锚点（指针的窗口坐标）。
    ///
    /// `Some` = 这次拖拽正越界：慢速 tick 朝那个方向滚视口，并把选区末端一直
    /// 挂在同一侧，于是「选中比一屏还长的输出」不必分段复制再手工拼接。
    /// 指针回到文字区内立刻归 `None`（见 [`TerminalPage::update_autoscroll`]）。
    ///
    /// 之前这里没有这一项，`cell_at` 把越界位移**直接丢弃**，选区只能停在
    /// 视口首/末行 —— 跨屏选择在界面上表现为「怎么拖都选不全」。
    autoscroll: Option<gpui::Point<Pixels>>,
    /// 鼠标此刻悬停的**链接**（用户在按 ⌘ 时才认）。
    ///
    /// 链接的两条来源（应用 `OSC 8` 标记、屏幕上的裸文本 URL/路径）由内核统一
    /// 折成「目标 + 占哪几格」，宿主只按列区间比对画下划线 —— 跨格、跨行回绕因此
    /// 自动成立，同一行里出现两次同一个 URL 时也只高亮点中的那一处。
    /// `None` = 指针不在链接上（或没按 ⌘）—— 两种情形在界面上的表现一致，所以不区分。
    hovered_link: Option<TermLink>,
    /// 焦点变化订阅（画布拿到 / 丢掉键盘焦点 → 写 `CSI I` / `CSI O`）。
    ///
    /// **必须持有**：`Subscription` 一丢，回调就不会再来（与弹窗表单里的
    /// 输入订阅同一条纪律）。在会话第一次出现时登记一次，之后不再重建。
    focus_watch: Vec<Subscription>,
    /// IME **组合中**的预编辑串（拼音候选那一串）；空串 = 没有进行中的组合。
    ///
    /// 它**不进终端网格**：组合是宿主侧的「上屏前状态」，只有提交
    ///（[`TerminalPage::replace_text_in_range`]）才把文本按编码通道写进 PTY。
    /// 画布把非空的值画在光标格上（预编辑覆盖层），与行、光标共享同一原点。
    preedit: String,
    /// 预编辑覆盖层 / IME 候选窗口的定位锚点：**画布内**的光标格 `(列, 行)`。
    ///
    /// 每帧渲染由画布写入（`Cell` 让 `&self` 的渲染路径也能更新它）。
    /// [`TerminalPage::bounds_for_range`] 用它在候选窗口跟随光标，而不是钉在画布左上角。
    ime_cursor: std::cell::Cell<(i32, i32)>,
    /// 重连快照（见 [`RespawnPlan`]）。`None` = 占位会话（起不来 / 无档案），不可重连。
    plan: Option<RespawnPlan>,
    /// 自动重连状态（见 [`ReconnectState`]）。默认 `Idle`。
    reconnect: ReconnectState,
    /// 智能提示弹层状态（候选 + 高亮位）。空 = 收起。
    suggest: SuggestState,
    /// shell 集成脚本是否已注入（每条会话至多一次；`spawn` 即发，字节在 PTY 里排队）。
    shell_integration_sent: bool,
}

impl TerminalSession {
    /// 按给定参数起一个会话。失败时保留一条「起不来」的状态，界面照常渲染并给出提示。
    fn spawn(options: SpawnOptions, cx: &mut Context<TerminalPage>) -> Self {
        let focus = cx.focus_handle();
        // 首帧的占位配色：真实的主题配色在第一次 `sync_cell_metrics` 时灌进去
        //（那里才拿得到 `window` 与字体度量）。
        let palette = Palette::default();
        let grid = (options.cols, options.rows);
        Self {
            handle: Session::spawn(options).ok().map(Arc::new),
            focus,
            cell: CellSize::default(),
            grid,
            scroll_remainder: 0.,
            title: None,
            palette,
            // 指纹先置一个不可能的值，逼第一帧就把主题色灌进去。
            // 一开始就与任何真实指纹都不相同：第一帧必定灌一次配色。
            style_key: (true, i32::MIN, [0; 3], [0; 3], [0; 3]),
            exited: false,
            started: std::time::Instant::now(),
            origin: gpui::Point::default(),
            viewport: Size::default(),
            selecting: false,
            autoscroll: None,
            hovered_link: None,
            focus_watch: Vec::new(),
            preedit: String::new(),
            ime_cursor: std::cell::Cell::new((0, 0)),
            plan: None,
            reconnect: ReconnectState::Idle,
            suggest: SuggestState::default(),
            shell_integration_sent: false,
        }
    }

    /// 本地 shell。
    fn spawn_local(cx: &mut Context<TerminalPage>) -> Self {
        let options = SpawnOptions {
            scrollback: scrollback(cx),
            encoding: terminal_encoding(cx),
            ..SpawnOptions::default()
        };
        let cwd = options.cwd.clone();
        let mut live = Self::spawn(options, cx);
        live.plan = Some(RespawnPlan {
            target: None,
            socket_dir: std::path::PathBuf::new(),
            cwd,
        });
        live
    }

    /// 一个**起不来**的占位会话（档案缺失、参数非法）。
    ///
    /// 存在的意义是让 [`TerminalPage::ensure_sessions`] 不必每帧重试：
    /// 重试的结果不会变，而画布需要一个稳定的「如实说明」状态。
    fn unavailable(cx: &mut Context<TerminalPage>) -> Self {
        Self {
            handle: None,
            focus: cx.focus_handle(),
            cell: CellSize::default(),
            grid: (80, 24),
            scroll_remainder: 0.,
            title: None,
            palette: Palette::default(),
            // 一开始就与任何真实指纹都不相同：第一帧必定灌一次配色。
            style_key: (true, i32::MIN, [0; 3], [0; 3], [0; 3]),
            exited: true,
            started: std::time::Instant::now(),
            origin: gpui::Point::default(),
            viewport: Size::default(),
            selecting: false,
            autoscroll: None,
            hovered_link: None,
            focus_watch: Vec::new(),
            preedit: String::new(),
            ime_cursor: std::cell::Cell::new((0, 0)),
            plan: None,
            reconnect: ReconnectState::Idle,
            suggest: SuggestState::default(),
            shell_integration_sent: false,
        }
    }

    /// 一条 SSH 会话：`ssh -t user@host`（见 `duck-ssh` 模块头对这条路线选择的说明）。
    ///
    /// **连接成败不在这里判**：`ssh` 进程本身起得来，认证失败 / 主机密钥不匹配是它
    /// 自己把原因打到 PTY 上然后退出。让错误留在终端画布上，用户看到的就是 ssh 的原文
    ///（`Permission denied (publickey)`、`Host key verification failed.`）——
    /// 比我们转述一遍准确得多，也省掉一层「错误码 → 人话」的映射表。
    ///
    /// 【本方法不起 SFTP】：文件面板走 [`duck_ssh::list_dir`] 那条独立的命令通道，
    /// 复用同一条物理连接（`ControlMaster`）但不占用这个 PTY。
    fn spawn_ssh(
        target: &duck_ssh::Target,
        socket_dir: &std::path::Path,
        cx: &mut Context<TerminalPage>,
    ) -> Self {
        let options = {
            let mut options = Self::ssh_options(target, socket_dir, None, None, scrollback(cx));
            // 编码是显示层偏好（不进 argv），初值取当前设置；之后可热切。
            options.encoding = terminal_encoding(cx);
            options
        };
        let mut live = Self::spawn(options, cx);
        live.plan = Some(RespawnPlan {
            target: Some(target.clone()),
            socket_dir: socket_dir.to_path_buf(),
            cwd: None,
        });
        live
    }

    /// 构造 SSH 会话的 [`SpawnOptions`]（初起与重连共用同一份 argv 拼装）。
    ///
    /// argv 来自 [`duck_ssh::Target::terminal_argv`]（`-t` + 目标串 + 远端 locale
    /// 引导，端口转发只在这一处加）—— 重连因此**重放的是同一条命令行**，
    /// 而不是另写一套。
    fn ssh_options(
        target: &duck_ssh::Target,
        socket_dir: &std::path::Path,
        cwd: Option<std::path::PathBuf>,
        size: Option<(u16, u16)>,
        scrollback: usize,
    ) -> SpawnOptions {
        // 口令走 askpass 助手（脚本落盘是幂等的，`duck_ssh` 自己也会落一次）。
        let helper = duck_ssh::askpass_script(socket_dir).ok();
        // 环境变量**必须从 `SpawnOptions::default()` 长出来**，不能整份替换：
        // 那个默认值里带着 `TERM=xterm-256color` 与 `COLORTERM=truecolor`
        //（`duck-term/src/session.rs:82`），是远端「能上 24 位色」的唯一来源。
        // 此前这里写的是 `env: target.env(..)` —— 字段被整份覆盖，两条一起丢，
        // 于是远端 `$TERM` 只剩 ssh 自己兜底的 `xterm`（16 色档），
        // 而 `ls --color` / `git diff` 的加粗与真彩全部退化。
        let mut env = SpawnOptions::default().env;
        env.extend(target.env(helper.as_deref()));
        let mut options = SpawnOptions {
            shell: Some(("ssh".to_owned(), target.terminal_argv(Some(socket_dir)))),
            env,
            scrollback,
            ..SpawnOptions::default()
        };
        if let Some(cwd) = cwd {
            options.cwd = Some(cwd);
        }
        if let Some((cols, rows)) = size {
            options.cols = cols;
            options.rows = rows;
        }
        options
    }

    /// 按冻结的 [`RespawnPlan`] 重建这条会话（断线重连）。
    ///
    /// 这是「重连」的全部：拿回当初的目标与尺寸，重放同一条 argv。会话键、标签、
    /// 分屏归属都不动 —— 调用方只把这个 `TerminalSession` 换回 `sessions` 表里的**原位**。
    ///
    /// `encoding` 由调用方从**旧会话**读出来传进来：编码是用户对这条会话的临时选择
    ///（可能是为了一个 GBK 日志），重连后不能悄悄回到设置页的默认值。
    fn respawn(
        plan: &RespawnPlan,
        grid: (u16, u16),
        encoding: Encoding,
        cx: &mut Context<TerminalPage>,
    ) -> Self {
        // 回滚行数取**当前**偏好，而不是会话当初那份：它是显示偏好（不进 argv），
        // 重连时用户多半正希望新起的这条跟上刚改过的设置。
        let scrollback = scrollback(cx);
        match &plan.target {
            Some(target) => {
                let mut options = Self::ssh_options(
                    target,
                    &plan.socket_dir,
                    plan.cwd.clone(),
                    Some(grid),
                    scrollback,
                );
                options.encoding = encoding;
                let mut live = Self::spawn(options, cx);
                live.plan = Some(RespawnPlan {
                    target: Some(target.clone()),
                    socket_dir: plan.socket_dir.clone(),
                    cwd: plan.cwd.clone(),
                });
                live
            }
            None => {
                let mut options = SpawnOptions {
                    scrollback,
                    encoding,
                    ..SpawnOptions::default()
                };
                if let Some(cwd) = &plan.cwd {
                    options.cwd = Some(cwd.clone());
                }
                options.cols = grid.0;
                options.rows = grid.1;
                let mut live = Self::spawn(options, cx);
                live.plan = Some(RespawnPlan {
                    target: None,
                    socket_dir: std::path::PathBuf::new(),
                    cwd: plan.cwd.clone(),
                });
                live
            }
        }
    }

    /// 底层会话（标签还没起来时是 `None`）。
    ///
    /// `pub(super)`：`terminal/tabs.rs` 的清屏路径也要拿它调 `scroll_to_bottom` ——
    /// 与画布层共用同一个会话句柄，不另存一份。
    pub(super) fn session(&self) -> Option<&Arc<Session>> {
        self.handle.as_ref()
    }

    /// 画布的焦点句柄。
    ///
    /// `pub(super)`：搜索条收起时要把键盘交还画布（`terminal/find.rs` 的 `close_find`），
    /// 而不该把 `focus` 字段整个开出去 —— 别的模块只需要「能聚焦」这一件事。
    pub(super) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 往 PTY 写一段字节（会话没起来时静默忽略）。
    pub(super) fn write(&self, bytes: &[u8]) {
        if let Some(session) = self.session() {
            session.write(bytes);
        }
    }

    /// 这条会话当前的编码（会话没起来时返回默认；仅用于「重连时沿用」与菜单标当前档）。
    pub(super) fn encoding(&self) -> Encoding {
        self.session()
            .map_or_else(Encoding::default, |live| live.encoding())
    }

    /// 热切换编码：显示与输入立刻按新编码走，不重开会话。
    ///
    /// `pub(super)`：标签右键菜单与设置页都从这里进来，只此一条路 ——
    /// 会话的编码状态只有内核那一份 `EncodingHandle`。
    pub(super) fn set_encoding(&self, encoding: Encoding) {
        if let Some(session) = self.session() {
            session.set_encoding(encoding);
        }
    }

    /// 子进程是否已退出（本地 shell 与 SSH 会话共用这个判据）。
    pub(super) fn is_exited(&self) -> bool {
        self.exited
    }

    /// 这条会话跑了多久（状态栏与右栏的「已连接」用它）。
    pub(super) fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// 当前网格（列, 行），状态栏显示用。
    pub(super) fn grid(&self) -> (u16, u16) {
        self.grid
    }

    /// 这条会话是否具备**自动重连的资格**：有目标（SSH）且有重连快照。
    ///
    /// 本地 shell 退出即关标签，不参与自动重连。
    fn auto_reconnect_eligible(&self) -> bool {
        self.plan.as_ref().is_some_and(|plan| plan.target.is_some())
    }

    /// 子进程**自然退出**（连接断了）后推进自动重连状态机。
    ///
    /// `blocked` = 屏幕上出现**不该自动重试**的失败（主机密钥 / 认证类）：这两类反复
    /// 重试会对同一账号连发认证（可能被 `MaxAuthTries` 或服务端风控拦下），一律不进
    /// 自动重连，直接落 `Stopped`、只留手动入口。
    ///
    /// 非 SSH（无目标）会话直接回落 `Idle`（本地 shell 不重连）。
    ///
    /// `policy` 由调用方从「设置 → 终端」读一次带进来（见 [`ReconnectPolicy`]）；
    /// 用户把「断线自动重连」关掉时，调用方会把它折进 `blocked`（同一落点 `Stopped`）。
    fn on_disconnected(&mut self, blocked: bool, now: std::time::Instant, policy: ReconnectPolicy) {
        let eligible = self.auto_reconnect_eligible();
        self.reconnect = reconnect_after_disconnect(eligible, blocked, self.reconnect, now, policy);
    }
}

impl TerminalPage {
    /// 给所有还没有运行时状态的标签补一个真实会话。
    ///
    /// 放在 `render` 开头而不是建标签的地方，是因为「起会话」需要 `&mut Context`：
    /// 让建标签与起会话分开，标签的构造就只是一次 `Vec::push`。
    ///
    /// **SFTP 标签刻意不在这里起会话**：它的内容是双栏文件面板，没有 PTY 要开
    ///（走 [`duck_ssh::list_dir`] 那条独立的命令通道，复用同一条物理连接）。
    ///
    /// 分屏的右窗格走同一条路：它就是一个普通的会话键
    ///（[`split_session_key`](crate::pages::terminal_data::split_session_key)）。
    /// 窗格连哪台机器由 `split_owner` 决定：缺省还是「标签自己的连接」（本地标签
    /// 再开一个 shell，SSH 标签复用同一个 `ControlMaster` socket）；指向另一条
    /// 连接时就是那台主机（另一条 `ssh -t`，另一条 `ControlMaster`）。
    pub(super) fn ensure_sessions(&mut self, cx: &mut Context<TerminalPage>) {
        // (`会话键`, 类型, `取目标用的连接 id`)。
        let mut missing: Vec<(String, TabKind, String)> = Vec::new();
        for tab in &self.tabs {
            if tab.kind == TabKind::Sftp {
                continue;
            }
            // 休眠标签（上次退出时恢复出来的）：不连，等用户点它。
            // 分屏的右窗格跟着一起休眠 —— 一半连着一半灰着，比整条灰着更让人困惑。
            if tab.dormant {
                continue;
            }
            if !self.sessions.contains_key(&tab.id) {
                missing.push((tab.id.clone(), tab.kind, tab.id.clone()));
            }
            if let Some(split) = &tab.split
                && !self.sessions.contains_key(split)
            {
                // 窗格连哪台机器由 `split_owner` 决定（「不同连接分屏」：本地标签可以
                // 配远端窗格，反之亦然）；缺省回落标签自己的连接 —— 旧口径不变。
                // 类型跟着归属走，而不是跟着标签走。
                let owner = tab.split_owner.clone().unwrap_or_else(|| tab.id.clone());
                let kind = tab.split_kind().unwrap_or(tab.kind);
                missing.push((split.clone(), kind, owner));
            }
        }
        for (key, kind, owner) in missing {
            let live = match kind {
                TabKind::Local => TerminalSession::spawn_local(cx),
                TabKind::Ssh => match self.ssh_target(&owner) {
                    Some((target, socket_dir)) => {
                        TerminalSession::spawn_ssh(&target, &socket_dir, cx)
                    }
                    // 档案被删了但标签还开着：插一个「起不来」的占位，
                    // 画布会如实说明原因（而不是每帧重试一遍再静默留白）。
                    None => TerminalSession::unavailable(cx),
                },
                TabKind::Sftp => continue,
            };
            self.sessions.insert(key, live);
        }
    }

    /// 断线重连：按会话的 [`RespawnPlan`] 重建**同一条**会话。
    ///
    /// 原位替换 `sessions[key]`：旧会话 `Drop` 收掉 PTY，新会话立即接管。会话键、
    /// 标签、分屏归属（`split_owner` / `split_kind`）一律不动 —— 重连是**这条会话
    /// 自己的事**，不触碰分屏语义，也不影响 `resolve_sftp_owner` 那条纯函数路径。
    ///
    /// 尺寸用断开前量到的网格（`grid`）恢复，避免重连瞬间先按 80×24 画一帧再跳。
    pub(super) fn reconnect_session(&mut self, key: &str, cx: &mut Context<TerminalPage>) {
        let Some(live) = self.sessions.get(key) else {
            return;
        };
        let Some(plan) = live.plan.as_ref() else {
            return;
        };
        let grid = live.grid;
        let encoding = live.encoding();
        let mut fresh = TerminalSession::respawn(plan, grid, encoding, cx);
        // 手动重连把自动重连的计数**归零**（回到 `Idle`）：
        // 留着 `Pending` 那个旧的 `due` 会在下一帧再触发一次 respawn，白起一条 ssh；
        // 留着 `Armed` 的计数则会让用户手动救回来之后、下一次断线只剩几次机会。
        // 「人已经出手了」的语义就是从头开始。
        fresh.reconnect = ReconnectState::Idle;
        self.sessions.insert(key.to_owned(), fresh);
        // 新会话有新的焦点句柄：清掉「已经交过焦点」的标记，下一帧重新把键盘交给画布
        //（否则焦点停在一个已随旧会话 drop 掉的死句柄上，按键无处可去）。
        self.focused_tab = None;
        cx.notify();
    }

    /// 启动事件轮询（幂等）。在 `TerminalPage::new` 里调一次。
    ///
    /// 内核的事件走的是 `std::sync::mpsc`，GPUI 的事件循环看不见它，所以要有一个
    /// 前台任务定期把它们取走并 `notify`。任务持有视图的**弱引用**：窗口一关、
    /// 视图释放，`update` 返回 `Err`，循环自己结束 —— 不需要额外的退出信号。
    pub(super) fn start_terminal_poller(&mut self, cx: &mut Context<TerminalPage>) {
        if self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |page, cx| {
            let mut interval = POLL_INTERVAL;
            loop {
                cx.background_executor().timer(interval).await;
                match page.update(cx, |page, cx| page.poll_sessions(cx)) {
                    // 有会话在跑就按帧轮询，没有就退回慢速心跳（见 `IDLE_INTERVAL`）。
                    Ok(active) => {
                        interval = if active { POLL_INTERVAL } else { IDLE_INTERVAL };
                    }
                    // 视图没了：任务收尾。唯一需要"退出信号"的场景。
                    Err(_) => return,
                }
            }
        }));
    }

    /// 取走所有会话的待处理事件；返回是否还有会话在跑。
    fn poll_sessions(&mut self, cx: &mut Context<TerminalPage>) -> bool {
        // 标签集合变了就落盘（比指纹，幂等）。放在「会话为空就早退」之前 ——
        // 空会话恰恰是最需要记下来的状态（用户刚关掉最后一个标签）。
        self.persist_tabs(cx);
        if self.sessions.is_empty() {
            return false;
        }
        // 自动重连的两项偏好在这一轮开头读一次（见 [`ReconnectPolicy`]）：一轮里
        // 每条已断会话用的是同一份快照，不会因为读了两遍而前后不一致。
        let reconnect_policy = ReconnectPolicy::from_settings(cx);
        let auto_reconnect = super::module_settings::terminal_settings(cx).auto_reconnect;
        // 标签类型要先取出来：`ChildExit` 的处理依赖它，而那时 `self.sessions` 正被借用。
        // **分屏的右窗格也要在表里**：它同样是一条会话，退出时得按同一条规矩裁决
        //（漏掉它会让它落进「未知会话」那一支）。
        let kinds: Vec<(String, TabKind)> = self
            .tabs
            .iter()
            .flat_map(|tab| {
                std::iter::once((tab.id.clone(), tab.kind)).chain(
                    tab.split
                        .iter()
                        .zip(tab.split_kind())
                        .map(|(split, kind)| (split.clone(), kind)),
                )
            })
            .collect();
        let mut dirty = false;
        let mut exited: Vec<String> = Vec::new();
        // 因主机密钥问题退出的 SSH 会话（会话键 + 屏幕信号）。等会话表借完再解析主机、
        // 推提示 —— 那一步要读 `self.profiles`，与 `&mut self.sessions` 冲突。
        let mut host_key_failures: Vec<(String, HostKeySignals)> = Vec::new();
        // 标签的回写（标题 / 关闭）必须等会话表借完再动 —— 两者都在 `self` 上。
        let mut retitled: Vec<(String, Option<String>)> = Vec::new();
        // 有输出的会话：输出意味着「网格可能变了」，建议候选要重算；顺带
        // 摘一遍新完成的命令进历史（`D` 标记本身就会触发 Wakeup）。
        let mut suggest_dirty: Vec<String> = Vec::new();
        // 待注入 shell 集成脚本的会话（设置开关在循环外读一次）。
        let mut inject_ids: Vec<String> = Vec::new();
        let shell_integration_on = super::module_settings::terminal_shell_integration(cx);
        for (id, live) in &mut self.sessions {
            let Some(session) = live.session().map(Arc::clone) else {
                continue;
            };
            let mut repaint = false;
            let mut ssh_exited = false;
            let pending = session.drain_events(|event| match event {
                SessionEvent::Wakeup => repaint = true,
                SessionEvent::Title(title) => {
                    if live.title.as_deref() != Some(title.as_str()) {
                        live.title = Some(title);
                        repaint = true;
                    }
                }
                SessionEvent::ResetTitle => {
                    if live.title.is_some() {
                        live.title = None;
                        repaint = true;
                    }
                }
                SessionEvent::Bell => repaint = true,
                SessionEvent::ChildExit(_) => {
                    live.exited = true;
                    repaint = true;
                    // 本地 shell 退出 = 这个标签的使命结束了：按终端惯例把它关掉
                    //（留在屏幕上的最后几行输出没有意义，标签栏里也不该有个死会话占位）。
                    //
                    // **SSH 会话反过来 —— 必须留着。** 它退出多半是连接失败
                    //（`Permission denied`、`Host key verification failed`），原因就打在
                    // 画布上；标签一闪即关等于把错误藏起来，用户看到的是
                    //「点了连接，什么都没发生」。
                    let ssh = kinds
                        .iter()
                        .any(|(tab_id, kind)| tab_id == id && *kind == TabKind::Ssh);
                    if ssh {
                        ssh_exited = true;
                    } else {
                        exited.push(id.clone());
                    }
                }
                // 剪贴板是同步系统调用，不需要窗口。
                SessionEvent::ClipboardStore(_, text) => {
                    let _ = duck_platform::write_text(&text);
                }
                SessionEvent::ClipboardQuery { request, .. } => {
                    // 拿不到系统剪贴板就回空串：请求必须被应答，否则应用会干等到超时。
                    let text = duck_platform::read_text().unwrap_or_default();
                    session.answer_clipboard(request, &text);
                }
                // 以下三类画布不消费，但要**取走**它们（否则 mpsc 会堆满）：
                // 光标闪烁交给系统光标、指针形状跟随全局、退出已由 `ChildExit` 覆盖。
                SessionEvent::CursorBlinkingChange
                | SessionEvent::MouseCursorDirty
                | SessionEvent::Exit => {}
            });
            // `pending` = 这一轮撞到了单次上限，还有积压。不在这里把它抽干：那个上限正是
            // 为了不让刷屏进程把 UI 线程按在事件循环里；标脏让下一跳接着取就够了。
            repaint |= pending;
            // 主机密钥告警的识别放在 `drain_events` **之外**：drain 持有事件队列锁，
            // 而扫描要拿网格锁 —— 两把锁分开，且扫描是一次性的阻塞读（见 `buffer_contains`）。
            if ssh_exited {
                let signals = scan_host_key_signals(&session);
                // 认证类失败（`Permission denied` / `Too many authentication failures`）
                // 与主机密钥问题一样**不可自动重试**：对同一账号反复认证可能被服务端拦下。
                let auth_failed = auth_failure_on_screen(&session);
                let blocked = signals.any() || auth_failed;
                if signals.any() {
                    host_key_failures.push((id.clone(), signals));
                }
                // 自然退出 → 推进自动重连状态机（用户主动关标签不会走到这里：
                // 会话已从 `sessions` 移除，本循环遍历不到它）。
                // 关掉「断线自动重连」时折进 `blocked`：同一落点 `Stopped`，
                // 只保留手动重连入口（与认证失败一个待遇）。
                live.on_disconnected(
                    blocked || !auto_reconnect,
                    std::time::Instant::now(),
                    reconnect_policy,
                );
            } else if !live.exited && reconnect_busy(live.reconnect) {
                // 重连后的连接存活够久即视为「真的连上了」：把尝试计数清零，
                // 下一次断开重新从起始退避开始。用**存活时长**而非「收到输出」当判据，
                // 否则 `Connection refused` 也会把计数清零、退化成无限重试。
                if live.started.elapsed() >= RECONNECT_HEALTHY_AFTER {
                    live.reconnect = ReconnectState::Idle;
                }
            }
            if repaint {
                retitled.push((id.clone(), live.title.clone()));
                suggest_dirty.push(id.clone());
                dirty = true;
            }
            if shell_integration_on && !live.shell_integration_sent && !live.exited {
                inject_ids.push(id.clone());
            }
        }
        for (id, title) in retitled {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                // 标签名**恒等于档案名**，OSC 标题只进副标题：
                // 远端跑一次 `vim` 再退出时，`title` 不该被改成 `/etc/nginx/nginx.conf`
                //（那样用户就认不出这是哪台机器了）。OSC 22/23 复位时 `title` 是
                // `None`，副标题跟着清空、标签回到只有档案名的样子。
                if tab.subtitle != title {
                    tab.subtitle = title;
                    dirty = true;
                }
            }
        }
        // 有输出才动历史与建议：干转的轮询（空闲心跳）一无所获，直接跳过。
        if !suggest_dirty.is_empty() {
            self.ingest_commands(cx);
            for id in &suggest_dirty {
                self.refresh_suggestions(id, cx);
            }
        }
        for id in inject_ids {
            self.inject_shell_integration(&id, cx);
        }
        for id in exited {
            // 分屏的右窗格退出：**只收那一半**，标签留着 —— 另一半还好好的。
            //（本地 shell 敲 `exit` 是常态，把整个标签关掉等于连左半边的活儿一起丢掉。）
            if let Some(index) = self
                .tabs
                .iter()
                .position(|tab| tab.split.as_deref() == Some(id.as_str()))
            {
                self.tabs[index].clear_split();
                self.sessions.remove(&id);
                if self.focus_pane.as_deref() == Some(id.as_str()) {
                    // 焦点句柄随会话没了：把「已经交过焦点」的标记一并清掉，
                    // 下一帧才会重新把键盘交给主窗格（否则按键落进一个死句柄）。
                    self.focus_pane = None;
                    self.focused_tab = None;
                }
                dirty = true;
                continue;
            }
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                self.close_tab_at(index, cx);
                dirty = true;
            }
        }
        // ssh 因主机密钥问题退出 → 给一条可操作的告警（方案 B：只加告警）。
        //
        // 只在「屏幕上确有主机密钥告警」**且**「库里此前已有这台主机的记录」时出声
        //（见 [`host_key_notice`]）：首次连接是 OpenSSH 与用户之间的 TOFU 流程，
        // 我们既不写 `known_hosts`、也不改用户的 `StrictHostKeyChecking`，更不插话。
        for (id, signals) in host_key_failures {
            let Some((host, port)) = self.ssh_endpoint(&id) else {
                continue;
            };
            let status = duck_ssh::host_key_status(&host, port);
            let Some(notice) = host_key_notice(signals, &status) else {
                continue;
            };
            let message = host_key_notice_text(notice, &host, port, cx);
            let tone = match notice {
                // 密钥变更可能是中间人：按错误级别呈现（红色）。
                HostKeyNotice::Changed => notify::Tone::Error,
                HostKeyNotice::Revoked | HostKeyNotice::Mismatch => notify::Tone::Warning,
            };
            // 同一台主机反复触发只占一条：固定 key 让新结论替换旧结论，而不是堆一列。
            let key = SharedString::from(format!("terminal-host-key-{host}:{port}"));
            crate::update::on_notify_window(cx, move |window, cx| {
                notify::sticky(key.clone(), tone, message.clone(), window, cx);
            });
        }
        // 自动重连：到期就重建会话。收集与重建分开 —— 遍历 `sessions` 要可变借用，
        // 而重建（[`Self::fire_reconnect`]）会改变 `sessions` 本身。
        dirty |= self.fire_due_reconnects(cx);
        if dirty {
            cx.notify();
        }
        // 搜索结果要跟着输出走：命中是按网格坐标算的，新行滚进来会把它们整体挪位。
        // 内部按 `FIND_REFRESH_MIN` 节流，没开搜索条时是一次空转检查。
        self.maybe_refresh_find(cx);
        true
    }

    /// 把到期的自动重连**逐条触发**（每个会话同一时刻只排一次，天然没有并发尝试）。
    ///
    /// 触发 = 用 [`RespawnPlan`] 重建会话，并把调控器（次数 / 退避）带到新会话上
    /// （`Armed`）—— 这样新连接若再断，退避会接着涨而不是从头开始。返回是否有会话被重建。
    fn fire_due_reconnects(&mut self, cx: &mut Context<TerminalPage>) -> bool {
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .sessions
            .iter()
            .filter_map(|(key, live)| match live.reconnect {
                ReconnectState::Pending { due, .. } if due <= now => Some(key.clone()),
                _ => None,
            })
            .collect();
        if due.is_empty() {
            return false;
        }
        for key in due {
            let Some(live) = self.sessions.get(&key) else {
                continue;
            };
            let ReconnectState::Pending { driver, .. } = live.reconnect else {
                continue;
            };
            let Some(plan) = live.plan.as_ref() else {
                continue;
            };
            let grid = live.grid;
            let encoding = live.encoding();
            let mut fresh = TerminalSession::respawn(plan, grid, encoding, cx);
            // 调控器随会话搬过去：Armed 表示「已发起过尝试，等结局」。
            fresh.reconnect = ReconnectState::Armed { driver };
            self.sessions.insert(key.clone(), fresh);
            // 新会话有新的焦点句柄；清掉「已交过焦点」的标记，下一帧重新接管键盘。
            if self.focused_tab.as_deref() == Some(key.as_str()) {
                self.focused_tab = None;
            }
        }
        true
    }

    /// 撤销**某条**会话的自动重连计时（会话归属变更 / 手动接管时调）。
    pub(super) fn cancel_reconnect(&mut self, key: &str) {
        if let Some(live) = self.sessions.get_mut(key) {
            live.reconnect = ReconnectState::Idle;
        }
    }

    /// 只保留 `keep` 里那些会话键的自动重连，其余全部撤销。
    ///
    /// **当前没有调用点**，保留它是因为「用户主动暂停后台重连」仍是一个合理动作
    ///（例如笔记本切到电池模式时）。切标签**不是**这样的动作 —— 见
    /// `terminal::tabs::TerminalPage::activate` 的说明：后台重连有次数上限，
    /// 而它的全部价值就在于「用户没看的时候自己长回来」。
    #[allow(dead_code)]
    pub(super) fn retain_reconnect_only(&mut self, keep: &[String]) {
        for (key, live) in &mut self.sessions {
            if reconnect_busy(live.reconnect) && !keep.iter().any(|k| k == key) {
                live.reconnect = ReconnectState::Idle;
            }
        }
    }

    /// 把字体的真实度量写进每个会话：单元格宽高 + 主题配色。
    ///
    /// 在 `render` 开头调（那里才有 `window` 与 `&mut self`）。每帧都算一遍是刻意的 ——
    /// 字号可被用户随时改（工具条的 ± 与快捷键），主题也跟随系统，缓存键只会比直接算
    /// 更复杂。真正的开销在 `layout_width` 上，而它走的是本帧的布局缓存，命中就是查表。
    pub(super) fn sync_cell_metrics(&mut self, window: &Window, cx: &App) {
        if self.sessions.is_empty() {
            return;
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let mono_token = tokens.typography.mono_md;
        let font_size_value = self.font_size(cx);
        let font_size = px(font_size_value);
        // 字体族 / 行高可被「设置 → 终端」覆盖（出厂默认 = 跟随主题）。
        // 这里每帧读一次模块偏好：它是个小对象的 `serde_json` 解析（亚微秒级），
        // 而字号/主题本来就要每帧现算（上面的理由），多这一份不改变量级。
        let prefs = super::module_settings::terminal_settings(cx);
        // 行高按 token 的比例随字号缩放：`mono_md` 是 13/20，直接沿用这个比例，
        // 用户调字号时网格保持同样的疏密，而不是像写死 1.5 那样越调越挤。
        let ratio = prefs.line_height_percent.map_or_else(
            || mono_token.line_height.to_f64() / mono_token.size.to_f64(),
            |percent| f64::from(percent) / 100.,
        );
        let line_height = px(font_size_value * ratio as f32);

        let font = mono_font(&prefs, tokens.typography.mono.clone());
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        // 等宽字体下所有字形的推进量相同，量一个 `'m'` 就够（`ch_advance` 在部分后端
        // 上会走不同路径，这里统一用实际布局宽度）。
        let cell = CellSize {
            width: text_system.layout_width(font_id, font_size, 'm').to_f64() as f32,
            height: line_height.to_f64() as f32,
        };
        // 度量拿不到（字体没加载出来）时保持旧值：0 会把网格算成 1 列 1 行。
        if cell.width <= 0. || cell.height <= 0. {
            return;
        }

        let dark = theme.mode() == ThemeMode::Dark;
        let palette = palette_for(cx);
        let style_key = style_key_of(dark, font_size_value, &palette);
        for live in self.sessions.values_mut() {
            live.cell = cell;
            if live.style_key == style_key {
                continue;
            }
            live.style_key = style_key;
            live.palette = palette;
            if let Some(session) = live.session() {
                // `apply_palette` 要锁一次 `Term`。只有主题 / 字号变了才走到这里，
                // 而主题切换本身就是一次全窗口重排 —— 这点等待淹没在里面了。
                session.apply_palette(palette);
                session.set_dark(dark);
            }
        }
    }

    /// 焦点跟随激活的本地标签。
    ///
    /// 只在「刚切到某个本地标签」时接管一次：如果用户主动把焦点挪去了别处
    ///（搜索框、侧栏），再抢回来就是在跟用户对着干。同一个标签重复渲染不会重置。
    pub(super) fn sync_canvas_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        // SFTP 标签没有 PTY 会话，焦点交给它也无处可写 —— 跳过。
        //
        // ⚠️ 这里**曾经**是 `if tab.kind != TabKind::Local { return; }`：
        // 于是「新建一个 SSH 连接」之后键盘焦点还留在标签栏上，用户必须先用鼠标
        // 点一下画布才能打字 —— 而 SSH 登录本来就要求你马上敲回车 / 输口令，
        // 那一瞬间的表现就是「连上了但打不了字」。回落条件由「是不是本地」改成
        // 「有没有会话」之后，本地与 SSH 走同一条路。
        if tab.kind == TabKind::Sftp {
            self.focused_tab = None;
            return;
        }
        let id = tab.id.clone();
        if self.focused_tab.as_deref() == Some(id.as_str()) {
            return;
        }
        let Some(live) = self.sessions.get(&id) else {
            return;
        };
        let handle = live.focus.clone();
        self.focused_tab = Some(id.clone());
        // 键盘交给主窗格，窗格指针也跟着走 —— 否则取消分屏后「当前窗格」
        // 还指着一个已经不存在的会话键。
        self.focus_pane = Some(id);
        cx.defer_in(window, move |_this, window, cx| window.focus(&handle, cx));
    }

    /// 给还没有监听器的会话挂上焦点变化订阅（每帧调一次，幂等）。
    ///
    /// 会话是在 `ensure_sessions` 里建的，而那里拿不到 `window`（订阅需要它），
    /// 所以登记推迟到渲染这一步 —— 会话表一空一满，这里都只是把缺的补上。
    ///
    /// 每帧重挂会让同一个焦点事件触发 N 次上报（N = 帧数）：应用侧的
    /// 「失焦即保存」会被刷成一串重复动作。
    pub(super) fn sync_focus_reporting(
        &mut self,
        window: &mut Window,
        cx: &mut Context<TerminalPage>,
    ) {
        for (key, live) in &mut self.sessions {
            if !live.focus_watch.is_empty() {
                continue;
            }
            let handle = live.focus.clone();
            let focus_in = {
                let key = key.clone();
                cx.on_focus_in(&handle, window, move |this, _window, cx| {
                    this.report_focus(&key, true, cx);
                })
            };
            let focus_out = {
                let key = key.clone();
                cx.on_blur(&handle, window, move |this, _window, cx| {
                    this.report_focus(&key, false, cx);
                })
            };
            live.focus_watch = vec![focus_in, focus_out];
        }
    }

    /// 画布拿到 / 丢掉键盘焦点：应用开了 `CSI ? 1004` 时通知它。
    ///
    /// 模式是**应用运行时**开的（见 `duck_term::encode_focus`），所以每次都要现问内核；
    /// 没开会得到 `None`，此时**一个字节都不写** —— 无条件发送会让 vim / tmux 这类
    /// 没开 1004 的程序把 `ESC [ I` 当输入吞掉。
    fn report_focus(&mut self, key: &str, focused: bool, _cx: &mut Context<TerminalPage>) {
        let Some(session) = self.sessions.get(key).and_then(TerminalSession::session) else {
            return;
        };
        let Some(bytes) = encode_focus(focused, session.term_mode()) else {
            return;
        };
        session.write(&bytes);
    }

    /// 画布把测量到的像素尺寸回传：换算成行列并让内核跟着变。
    ///
    /// 只在**真的变了**的时候动内核 —— 这个回调每帧都会被调一次，
    /// 无条件 `resize` 会把 `SIGWINCH` 变成每帧一次的噪声，应用（vim、htop）
    /// 会被刷屏式的重画拖垮。尺寸变了要补一次 `notify`：本帧画的是旧网格，
    /// 不重绘的话新尺寸要等到下一次输出才看得见。
    ///
    /// `bounds` 是**文字区**的边界（内层 wrapper，已扣掉画布内边距）：
    /// 尺寸用来换算行列，原点用来把鼠标像素换算成格子。两者必须同源 ——
    /// 一个来自内层、一个来自外层的话，选中的格子会整体偏掉一个内边距。
    pub(super) fn apply_canvas_bounds(
        &mut self,
        tab_id: &str,
        bounds: gpui::Bounds<Pixels>,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        // 原点与尺寸是「布局事实」，与字号无关，变了就记下来 —— 不 `notify`：
        // 这条回调每帧都跑，随手请求重绘就等于自激（同一帧里反复排帧）。
        live.origin = bounds.origin;
        live.viewport = bounds.size;
        if live.cell.width <= 0. || live.cell.height <= 0. {
            return;
        }
        let size = bounds.size;
        let cols = (size.width.to_f64() / f64::from(live.cell.width)).floor();
        let rows = (size.height.to_f64() / f64::from(live.cell.height)).floor();
        if !(cols >= 1. && rows >= 1.) {
            return;
        }
        let cols = cols.min(f64::from(u16::MAX)) as u16;
        let rows = rows.min(f64::from(u16::MAX)) as u16;
        if (cols, rows) == live.grid {
            return;
        }
        live.grid = (cols, rows);
        let cell = live.cell;
        if let Some(session) = live.session() {
            // 会话内部全是内部可变，`resize` 收 `&self` —— 握着 `Arc` 也能改。
            session.resize(cols, rows, cell);
        }
        cx.notify();
    }

    /// 鼠标像素 → 网格坐标（视口行列，未叠加 `display_offset`）。
    ///
    /// 越界不拒绝而是钳到网格内：拖到窗口边缘外继续选是常规操作，
    /// 富余的位移直接丢弃（`selection` 的末端会停在边界上），而不是让整次拖选失效。
    ///
    /// 只给**格子**，不给 `Side`：整格语义下 Side 由拖拽方向决定，见
    /// [`Self::extend_selection`] 与 `anchor_sides`。调用方（鼠标上报、链接命中）
    /// 都只用行列。
    fn cell_at(&self, tab_id: &str, position: gpui::Point<Pixels>) -> Option<(i32, usize)> {
        let live = self.sessions.get(tab_id)?;
        let cell = live.cell;
        let viewport = live.viewport;
        if cell.width <= 0. || cell.height <= 0. || live.grid == (0, 0) {
            return None;
        }
        let (columns, rows) = (usize::from(live.grid.0), usize::from(live.grid.1));
        let x = (position.x - live.origin.x).to_f64();
        let y = (position.y - live.origin.y).to_f64();
        let column = (x / f64::from(cell.width)).floor();
        let row = (y / f64::from(cell.height)).floor();
        // 视口外的位移（拖到画布外面）钳到边缘，选区跟着停在最后一行/列。
        let _ = viewport;
        let column = column.clamp(0., (columns.max(1) - 1) as f64);
        let row = row.clamp(0., (rows.max(1) - 1) as f64) as i32;
        // 端点**整格**纳入：指针落在哪一格，那一格就整个算数 —— 与 macOS Terminal.app /
        // iTerm2 一致。整格语义由内核统一保证（`Session::select_update` 里的
        // `include_all()`，按点序摆正两侧端点），这里只负责像素 → 格子，不再算
        // 「指针在格子的左半还是右半」。
        Some((row, column as usize))
    }

    /// 鼠标按下（`fresh`）或拖拽中：把像素位置换算成**网格坐标**交给内核的选区状态机。
    ///
    /// 视口行 → 网格行要减掉 `display_offset`（滚上去看历史时视口第 0 行不是网格第 0 行）。
    /// 这一步必须在内核里问 —— 宿主自己记的行号在滚动之后就是错的。
    ///
    /// # 端点整格语义（真源在内核）
    ///
    /// vendored alacritty 的 `Selection` 锚点本是「半格边缘」语义：按下侧
    /// `Side::Right` 排除本格、松手侧 `Side::Left` 排除本格。直译这个模型，用户
    /// 按在字右半边再向右拖就丢掉首字、松在字左半边就丢掉末字 —— 实测表现就是
    /// 「常常无法准确选中」。主流终端（macOS Terminal.app / iTerm2）都是**整格**
    /// 语义：端点落在哪格，哪格整个纳入。
    ///
    /// 这条语义的**单一真源是内核**：[`Session::select_update`] 每次更新末端后调
    /// `Selection::include_all()`（"Expand selection sides to include all cells"），
    /// 按点序摆正两侧端点 —— 左右两个拖拽方向都成立，块选同样受益。宿主不再
    /// 自己做 Side 的方向补偿（曾在本层做过一版，与内核机制重叠，已删）：
    /// 按下给 `Side::Left` 只是让「只按不拖」的单击保持空选区语义。
    fn extend_selection(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        kind: SelectionKind,
        fresh: bool,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some((row, column)) = self.cell_at(tab_id, position) else {
            return;
        };
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        let offset = session
            .try_with_term(|term| term.renderable_content().display_offset as i32)
            .unwrap_or_default();
        let point = TermPoint::new(Line(row - offset), Column(column));
        if fresh {
            session.select_start_kind(point, Side::Left, kind);
        } else {
            // 末端整格吸附由内核的 `include_all()` 保证，Side 传什么都会被摆正。
            session.select_update(point, Side::Right);
        }
        cx.notify();
    }

    /// 左键在**这块画布上**按下：立一个「正在拖选」的标记并按手势起选区。
    ///
    /// 起手就调 `select_start` 而不是等第一次移动：内核的 `Selection::new` 是**覆盖**语义，
    /// 于是「点一下空白处」天然等于清掉上一次的选区（选区随即为空，
    /// `selection_text()` 返回 `None`，复制动作不会把剪贴板冲成空串）。
    ///
    /// 手势分工（与主流终端一致，参考 `temp/oxideterm` 的同名分派）：
    ///
    /// | 手势 | 粒度 | 典型用途 |
    /// | --- | --- | --- |
    /// | 拖拽 | 逐格 | 任意范围 |
    /// | ⌥ + 拖拽 | 矩形块 | 按列取「表格 / `ls -l` 输出」的一列 |
    /// | 双击 | 按词（先试括号配对） | 取一个字段、一段路径 / URL、一对引号里的内容 |
    /// | 三击及以上 | 整行 | 取一整个配置行、一行日志 |
    ///
    /// **双击/三击是「选得准」的关键**：只靠像素拖选，端点容易落在字的间隙上；
    /// 按词/按行的语义边界由内核的语义搜索给出，不依赖像素精度。
    /// 端点的整格纳入规则见 [`Self::extend_selection`]。
    pub(super) fn begin_selection(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        click_count: usize,
        block: bool,
        cx: &mut Context<TerminalPage>,
    ) {
        let kind = match click_count {
            0 | 1 if block => SelectionKind::Block,
            0 | 1 => SelectionKind::Simple,
            2 => SelectionKind::Word,
            _ => SelectionKind::Line,
        };
        // 双击 / 三击是「一次性选中」，不进入拖选扩写态：
        // 之后若真的拖动，内核会按同粒度继续扩（`Selection::update` 不改 `ty`）。
        if let Some(live) = self.sessions.get_mut(tab_id) {
            live.selecting = true;
        }
        self.extend_selection(tab_id, position, kind, true, cx);
        self.update_autoscroll(tab_id, position, cx);
    }

    /// 左键按住移动：只有**这一格发起过拖选**才续接选区。
    ///
    /// 这条守卫不能省：拖标签并入分屏时左键同样是按下态、指针也要划过画布
    ///（`on_mouse_move` 只要求「悬停 + Bubble」，读不到「拖的是谁」），
    /// 少了它，一次拖标签就会顺手把整片终端文字选上。
    pub(super) fn drag_selection(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        cx: &mut Context<TerminalPage>,
    ) {
        let selecting = self.sessions.get(tab_id).is_some_and(|live| live.selecting);
        if !selecting {
            return;
        }
        // 粒度为 `Simple` 只作占位：内核里那次选区的 `ty` 在按下时就定了，
        // `select_update` 只挪末端，不会退回逐格。
        self.extend_selection(tab_id, position, SelectionKind::Simple, false, cx);
        self.update_autoscroll(tab_id, position, cx);
    }

    /// 左键松开：结束这次拖选（选区本身留在内核里，等着被复制）。
    pub(super) fn end_selection(&mut self, tab_id: &str) {
        if let Some(live) = self.sessions.get_mut(tab_id) {
            live.selecting = false;
            // 停掉自动滚动：指针已经抬起，继续滚视口就是「松手了还在动」。
            live.autoscroll = None;
        }
    }

    /// 记录 / 清除「拖到文字区外」的自动滚动锚点。
    ///
    /// 判据只看**纵向**：终端是行优先的，拖到左边界外（`x < 0`）不该滚视口，
    /// 而 `cell_at` 已经把横向越界钳进了网格。
    fn update_autoscroll(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        if !live.selecting {
            return;
        }
        let top = live.origin.y;
        let bottom = top + live.viewport.height;
        let next = (position.y < top || position.y >= bottom).then_some(position);
        if live.autoscroll != next {
            live.autoscroll = next;
            cx.notify();
        }
    }

    /// 拖选越界时朝那一侧滚视口，并把选区末端挂在同一侧继续扩。
    ///
    /// 与滚轮同一条内核入口（[`Session::scroll`]，正数 = 向上翻），
    /// 因此回滚区、`display_offset`、选区随滚动 `rotate` 的规则全部复用。
    fn tick_autoscroll(&mut self, cx: &mut Context<TerminalPage>) {
        let pending: Vec<(String, gpui::Point<Pixels>)> = self
            .sessions
            .iter()
            .filter_map(|(key, live)| live.autoscroll.map(|position| (key.clone(), position)))
            .collect();
        for (key, position) in pending {
            let Some(rows) = self.autoscroll_rows(&key) else {
                continue;
            };
            let Some(session) = self.sessions.get(&key).and_then(TerminalSession::session) else {
                continue;
            };
            session.scroll(rows);
            // 视口已经变了：用**同一个**指针位置重算网格坐标，
            // 选区末端因此停在新的视口边缘（这就是「越滚越长」）。
            self.extend_selection(&key, position, SelectionKind::Simple, false, cx);
        }
    }

    /// 越界的距离 → 一次 tick 滚几行（`None` = 没越界，不滚）。
    ///
    /// 距离越远滚得越快，但**上下各钳到 4 行**：再多只会把回滚区一冲到底，
    /// 用户反而找不到想停的位置。
    fn autoscroll_rows(&self, tab_id: &str) -> Option<i32> {
        let live = self.sessions.get(tab_id)?;
        let position = live.autoscroll?;
        if live.cell.height <= 0. || live.grid == (0, 0) {
            return None;
        }
        let top = live.origin.y;
        let bottom = top + live.viewport.height;
        let cell = f64::from(live.cell.height);
        let rows = if position.y < top {
            ((top - position.y).to_f64() / cell).ceil().max(1.)
        } else if position.y >= bottom {
            -(((position.y - bottom).to_f64() / cell).ceil().max(1.))
        } else {
            return None;
        };
        Some((rows as i32).clamp(-AUTOSCROLL_MAX_ROWS, AUTOSCROLL_MAX_ROWS))
    }

    /// 启动自动滚动的心跳（幂等）。在 `TerminalPage::new` 里调一次。
    ///
    /// 独立于事件轮询：那个是 16ms 一帧，拿它当滚动时钟会让视口一帧滚好几行。
    /// 与轮询一样持**弱引用**，窗口关掉后 `update` 返回 `Err`，循环自己结束。
    pub(super) fn start_autoscroll_poller(&mut self, cx: &mut Context<TerminalPage>) {
        if self.autoscroll_task.is_some() {
            return;
        }
        self.autoscroll_task = Some(cx.spawn(async move |page, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_INTERVAL).await;
                if page
                    .update(cx, |page, cx| page.tick_autoscroll(cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// 把一次鼠标事件编码后写进 PTY；应用没要求上报时返回 `false`（调用方据此回到本地选择）。
    ///
    /// 上报模式是**应用运行时**开的（vim / htop / tmux 一进去就开 `CSI ?1000` 之类），
    /// 所以每次事件都要现问内核（[`Session::term_mode`]），不能缓存在宿主一侧。
    fn report_mouse(
        &self,
        tab_id: &str,
        action: MouseAction,
        position: gpui::Point<Pixels>,
        modifiers: gpui::Modifiers,
        cx: &mut Context<TerminalPage>,
    ) -> bool {
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return false;
        };
        let Some((row, column)) = self.cell_at(tab_id, position) else {
            return false;
        };
        let Some(bytes) = encode_mouse(
            action,
            column,
            row,
            MouseModifiers {
                shift: modifiers.shift,
                alt: modifiers.alt,
                control: modifiers.control,
            },
            session.term_mode(),
        ) else {
            return false;
        };
        session.write(&bytes);
        cx.notify();
        true
    }

    /// 应用是否要求上报指针移动（拖拽 / 全量移动）。
    fn mouse_tracking_motion(&self, tab_id: &str) -> bool {
        self.sessions
            .get(tab_id)
            .and_then(TerminalSession::session)
            .is_some_and(|session| reports_motion(session.term_mode()))
    }

    /// 鼠标像素位置下的链接（应用 `OSC 8` 标记，或屏幕上长得像链接的裸文本）。
    /// 没有就返回 `None`。
    ///
    /// 与 [`Self::cell_at`] 同一套换算，只是把视口行再减掉 `display_offset` 换回网格行 ——
    /// 往上看历史时链接也得点得中，这一步不能省。
    fn link_at(&self, tab_id: &str, position: gpui::Point<Pixels>) -> Option<TermLink> {
        let (row, column) = self.cell_at(tab_id, position)?;
        let session = self
            .sessions
            .get(tab_id)
            .and_then(TerminalSession::session)?;
        let offset = session
            .try_with_term(|term| term.renderable_content().display_offset as i32)
            .unwrap_or_default();
        session.link_at(TermPoint::new(Line(row - offset), Column(column)))
    }

    /// 悬停时维护「指针是否落在链接上」。
    ///
    /// 只在**按下 ⌘** 时才认链接：链接的默认动作是「打开」，而终端里左键拖选是最高频
    /// 的操作，把打开做成裸点击会毁掉选区。⌘ = 「我要对它做点网页式的事」，
    /// 与 macOS 终端、iTerm 的约定一致。
    fn update_hovered_link(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        platform: bool,
        cx: &mut Context<TerminalPage>,
    ) {
        let link = platform.then(|| self.link_at(tab_id, position)).flatten();
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        if live.hovered_link != link {
            live.hovered_link = link;
            // 下划线要立刻出现 / 消失：这是「可点」的唯一视觉线索。
            cx.notify();
        }
    }

    /// 用系统默认程序打开指针下的链接。打开了返回 `true`（调用方据此不再起选区）。
    ///
    /// 打不开（没有 `open` / `xdg-open`、权限不够）时**不静默**：走统一通知，否则用户
    /// 只会看到「按了 ⌘ 点击却什么都没发生」。
    fn open_link_at(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<TerminalPage>,
    ) -> bool {
        let Some(link) = self.link_at(tab_id, position) else {
            return false;
        };
        if let Err(error) = duck_platform::open_url(&link.target.open_text()) {
            let message = trf(cx, "打开链接失败：{}", &[&error.to_string()]);
            notify::error(SharedString::from(message), window, cx);
        }
        true
    }

    /// 复制当前选区到系统剪贴板。没有选区时**什么都不做** ——
    /// 单击一下就把剪贴板冲成空串，是比「什么都没发生」严重得多的坏行为。
    fn copy_selection(&mut self, tab_id: &str, cx: &mut Context<TerminalPage>) {
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        if let Some(text) = session.selection_text() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    // 粘贴不在这里：多行确认 / 折成一行 / 裁剪首尾那一整套在
    // `terminal::paste`（`TerminalPage::paste_from_clipboard`）。早期的实现直接
    // `session.paste(&text)` —— 多行文本里的每个换行都会立刻执行，等于把「复制一段
    // 带注释的脚本」变成「照着跑一遍」。

    /// 全选（回滚区 + 当前屏幕）。
    fn select_all(&mut self, tab_id: &str, cx: &mut Context<TerminalPage>) {
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        session.select_all();
        cx.notify();
    }

    /// 当前拿到键盘焦点的那块画布的会话键。
    ///
    /// IME 的各个回调（`replace_and_mark_text_in_range` 等）只拿到 `TerminalPage`，
    /// 拿不到是哪块画布 —— 用**窗口里的实际焦点句柄**反查，比记一个「上次点过的窗格」
    /// 更可靠（后者在用户用 ⌘ 切标签 / 点侧栏后会漂）。
    fn ime_session_key(&self, window: &Window) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, live)| live.focus.is_focused(window))
            .map(|(key, _)| key.clone())
    }

    /// 记录 / 清除某条会话的预编辑串（组合中）。
    ///
    /// 只动画布状态，**不写 PTY**：组合期的内容还没上屏，写进去就等于把拼音字母
    /// 灌给了 shell。真正的提交在 [`Self::commit_ime_text`]。
    fn set_ime_preedit(&mut self, key: &str, text: String, cx: &mut Context<TerminalPage>) {
        let Some(live) = self.sessions.get_mut(key) else {
            return;
        };
        if live.preedit == text {
            return;
        }
        live.preedit = text;
        cx.notify();
    }

    /// IME 上屏：把**提交文本**按 `duck_term` 的文本通道写进 PTY，并清掉预编辑串。
    ///
    /// 走的是与按键文本同一条 [`duck_term::encode`] 通道（`keymap.rs` 第 3 步：
    /// 「能插入什么就写什么」），所以中文、emoji、空格与直接敲的字符没有两套规则。
    fn commit_ime_text(&mut self, key: &str, text: &str, cx: &mut Context<TerminalPage>) {
        if let Some(live) = self.sessions.get_mut(key) {
            live.preedit.clear();
        }
        if text.is_empty() {
            cx.notify();
            return;
        }
        let Encoded::Bytes(bytes) = encode(KeyStroke {
            key: "",
            text: Some(text),
            ctrl: false,
            alt: false,
            shift: false,
            platform: false,
        }) else {
            return;
        };
        if let Some(session) = self.sessions.get(key).and_then(TerminalSession::session) {
            session.write(&bytes);
            session.scroll_to_bottom();
        }
        cx.notify();
    }

    /// 把一次按键编码后写进 PTY（动作处理器的公共出口）。
    fn send_encoded(
        &mut self,
        tab_id: &str,
        stroke: KeyStroke<'_>,
        cx: &mut Context<TerminalPage>,
    ) {
        let Encoded::Bytes(bytes) = encode(stroke) else {
            return;
        };
        if let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) {
            session.write(&bytes);
            session.scroll_to_bottom();
            cx.notify();
        }
    }

    /// 键盘输入：编码成终端字节序列写进 PTY。
    pub(super) fn on_terminal_key(
        &mut self,
        tab_id: &str,
        event: &KeyDownEvent,
        cx: &mut Context<TerminalPage>,
    ) {
        // 建议弹层优先：它可见时 ↑/↓/→/Tab/Esc 是弹层的键，绝不透传进 PTY。
        if self.suggest_handle_key(tab_id, event, cx) {
            return;
        }
        let Some(live) = self.sessions.get(tab_id) else {
            return;
        };
        let Some(session) = live.session().map(Arc::clone) else {
            return;
        };
        let modifiers = event.keystroke.modifiers;
        // ⌘V 粘贴：主修饰键组合不进 `encode`（那是字符层的事），走系统剪贴板。
        // 内核的 `paste` 会按括号粘贴模式（`CSI ?2004`）决定要不要包标记。
        if modifiers.platform && event.keystroke.key == "v" {
            if let Some(text) = duck_platform::read_text() {
                session.paste(&text);
                session.scroll_to_bottom();
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        // 其余 ⌘ 组合由内核拒收（[`encode`] 第 0 步）：能走到 `on_key_down` 就说明
        // **没有任何**绑定命中（命中的话事件在动作阶段已被消费，这里根本不会被调用），
        // 此时若把字母写进 PTY，用户按 ⌘A / ⌘S / ⌘Q 会在 shell 里凭空多出一个字母。
        // 下面直接把 `platform` 交给编码层，规则只有一处、可单测。
        let encoded = encode(KeyStroke {
            key: event.keystroke.key.as_str(),
            text: event.keystroke.key_char.as_deref(),
            ctrl: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            platform: modifiers.platform,
        });
        let Encoded::Bytes(bytes) = encoded else {
            return;
        };
        session.write(&bytes);
        // 打字意味着要跟**当前**提示符交互：把视口拉回底部（用户可能刚翻着历史）。
        session.scroll_to_bottom();
        // 终端的按键不该同时触发全局快捷键（⌘W 关标签之类由 shell 顶栏管，别在这里放行）。
        cx.stop_propagation();
        cx.notify();
    }

    /// 送一个 `Ctrl-<字母>` 控制字节（终端行编辑快捷键的公共出口）。
    ///
    /// 动作处理器只拿得到「派发了哪个动作」，而 [`encode`] 要的是完整 [`KeyStroke`]；
    /// 这里把「Ctrl + 单字母」这一形状拼出来，免得每个动作重复一遍结构体字面量。
    fn send_ctrl(&mut self, tab_id: &str, letter: &'static str, cx: &mut Context<TerminalPage>) {
        self.send_encoded(
            tab_id,
            KeyStroke {
                key: letter,
                text: None,
                ctrl: true,
                alt: false,
                shift: false,
                platform: false,
            },
            cx,
        );
    }

    // ── 智能提示（详见 docs/terminal-autocomplete-design.md）──────────────────

    /// 建议弹层是否对这条会话可见。
    fn suggest_visible(&self, tab_id: &str) -> bool {
        self.sessions
            .get(tab_id)
            .is_some_and(|live| live.suggest.visible())
    }

    /// 弹层可见时拦截的按键。返回 `true` 表示事件已消费（调用点要
    /// `stop_propagation`，不能再把字节写进 PTY）。
    ///
    /// 刻意**不拦**的键：`Enter`（关弹层照常提交）、普通字符（透传后等回显
    /// 触发的 [`SessionEvent::Wakeup`] 重算候选）。
    fn suggest_handle_key(
        &mut self,
        tab_id: &str,
        event: &KeyDownEvent,
        cx: &mut Context<TerminalPage>,
    ) -> bool {
        if !self.suggest_visible(tab_id) {
            return false;
        }
        match event.keystroke.key.as_str() {
            "up" => self.suggest_move(tab_id, -1, cx),
            "down" => self.suggest_move(tab_id, 1, cx),
            "tab" | "right" => self.suggest_accept(tab_id, cx),
            "escape" => {
                if let Some(live) = self.sessions.get_mut(tab_id) {
                    live.suggest.clear();
                }
            }
            // 回车 = 执行当前行，建议不参与。
            "enter" => {
                if let Some(live) = self.sessions.get_mut(tab_id) {
                    live.suggest.clear();
                }
                return false;
            }
            _ => return false,
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    /// 移动弹层高亮（循环）。
    fn suggest_move(&mut self, tab_id: &str, direction: i32, cx: &mut Context<TerminalPage>) {
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        let count = live.suggest.items.len();
        if count == 0 {
            return;
        }
        let current = live.suggest.selected as i32;
        live.suggest.selected = (current + direction).rem_euclid(count as i32) as usize;
        cx.notify();
    }

    /// 接受当前高亮的建议：退格删掉已打的半个词（按**字符**数），再把差集
    /// 写进 PTY —— 行编辑仍由 shell 完成，宿主只打字。
    fn suggest_accept(&mut self, tab_id: &str, cx: &mut Context<TerminalPage>) {
        let Some(live) = self.sessions.get(tab_id) else {
            return;
        };
        let Some(session) = live.session().map(Arc::clone) else {
            return;
        };
        let Some(item) = live
            .suggest
            .items
            .get(live.suggest.selected % live.suggest.items.len().max(1))
            .cloned()
        else {
            return;
        };
        // 光标此刻必须还停在输入末尾（弹层出现的前提），拿最新的输入行算差集。
        let Some(input) = session.input_line() else {
            return;
        };
        let backspaces = item.backspaces(&input.text);
        let insert = item.insert_text(&input.text);
        if backspaces > 0 {
            session.write(&vec![0x7f; backspaces]);
        }
        if !insert.is_empty() {
            session.write_str(&insert);
        }
        session.scroll_to_bottom();
        // 接受不是终点：fish 式地接着给下一段建议（比如接受 `git status` 后
        // 紧跟一个空格，下一轮给文件名候选）。
        self.refresh_suggestions(tab_id, cx);
        cx.notify();
    }

    /// 重新计算一条会话的建议候选。任何拿不到的前提（会话没了、设置关了、
    /// 备用屏、不在提示符、输入为空）都收起弹层 —— 宁可漏弹。
    fn refresh_suggestions(&mut self, tab_id: &str, cx: &App) {
        // 历史装载要借 `self`，放在 `sessions` 可变借用**之前**。
        self.ensure_history(cx);
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        if !super::module_settings::terminal_suggest(cx) {
            live.suggest.clear();
            return;
        }
        let Some(session) = live.session() else {
            live.suggest.clear();
            return;
        };
        if session.term_mode().contains(TermMode::ALT_SCREEN) {
            live.suggest.clear();
            return;
        }
        let Some(input) = session.input_line() else {
            live.suggest.clear();
            return;
        };
        if input.text.trim_end().is_empty() {
            live.suggest.clear();
            return;
        }
        let Some(plan) = live.plan.as_ref() else {
            live.suggest.clear();
            return;
        };
        let host = history_host(plan.target.as_ref());
        let history = self.term_history.get(&host).cloned().unwrap_or_default();
        // 路径候选只给本地会话：远端列表要走 SFTP 通道（设计文档 M3），
        // 现在挂进来只会让 SSH 会话的弹层响应变慢。
        let paths = if plan.target.is_none() {
            plan.cwd
                .as_deref()
                .map(|cwd| path_candidates(cwd, last_word(&input.text)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        live.suggest.items = suggest_for(&input.text, &history, &paths, 8);
        live.suggest.selected = 0;
    }

    /// 把各会话新完成的命令摘进历史（[`Session::drain_commands`] 的水位去重
    /// 保证同一条命令只采一次），有新增才写 kv。
    fn ingest_commands(&mut self, cx: &App) {
        let mut fresh: Vec<(String, Vec<String>)> = Vec::new();
        for (id, live) in &self.sessions {
            let Some(session) = live.session() else {
                continue;
            };
            let captured: Vec<String> = session
                .drain_commands()
                .into_iter()
                .map(|record| record.command)
                .collect();
            if !captured.is_empty() {
                fresh.push((id.clone(), captured));
            }
        }
        if fresh.is_empty() {
            return;
        }
        self.ensure_history(cx);
        for (id, commands) in fresh {
            let Some(host) = self.session_host(&id) else {
                continue;
            };
            for command in commands {
                remember_command(&mut self.term_history, &host, &command);
            }
        }
        save_history(cx, &self.term_history);
    }

    /// 会话的历史命名空间（SSH = 目标串，本地 = `localhost`）。
    fn session_host(&self, tab_id: &str) -> Option<String> {
        self.sessions.get(tab_id).and_then(|live| {
            live.plan
                .as_ref()
                .map(|plan| history_host(plan.target.as_ref()))
        })
    }

    /// 历史缓存惰性装载（kv 只读一次）。
    fn ensure_history(&mut self, cx: &App) {
        if !self.term_history_loaded {
            self.term_history = load_history(cx);
            self.term_history_loaded = true;
        }
    }

    /// L2：把 shell 集成脚本发进会话（每条至多一次）。
    ///
    /// 字节在 PTY 里排队，shell 初始化完成读到即执行；远端 shell 不是
    /// bash / zsh 时脚本自己退出去，一个多余的字节都不发。
    fn inject_shell_integration(&mut self, tab_id: &str, cx: &App) {
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        if live.shell_integration_sent || live.exited {
            return;
        }
        if !super::module_settings::terminal_shell_integration(cx) {
            return;
        }
        let Some(session) = live.session().map(Arc::clone) else {
            return;
        };
        live.shell_integration_sent = true;
        // 行首空格：bash 的 `HISTCONTROL=ignorespace` 与 zsh 的
        // `HIST_IGNORE_SPACE` 都会把带空格开头的命令踢出历史 —— 尽力而为。
        session.write_str(&format!(
            " {}\r",
            super::terminal::suggest::SHELL_INTEGRATION_SCRIPT
        ));
    }

    /// 按行翻某个标签的视口（正数向上看历史，与 [`Session::scroll`] 同向）。
    fn scroll_tab(&mut self, tab_id: &str, lines: i32, cx: &mut Context<TerminalPage>) {
        if lines == 0 {
            return;
        }
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        session.scroll(lines);
        cx.notify();
    }

    /// 把某个标签的视口顶到最上 / 最下。
    fn scroll_tab_to_edge(&mut self, tab_id: &str, bottom: bool, cx: &mut Context<TerminalPage>) {
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        if bottom {
            session.scroll_to_bottom();
        } else {
            session.scroll_to_top();
        }
        cx.notify();
    }

    /// 一屏的行数（Page 键步长）。
    ///
    /// 取 `rows - 1`：留下**一行重叠**，翻页后还能看到上一屏的最后一行 —— 与 `less` /
    /// 多数终端的分页手感一致（整屏翻会让上下文断掉）。取不到尺寸时给 24 行兜底，
    /// 免得 Page 键在首帧（网格还没量出来）变成空操作。
    fn page_lines(&self, tab_id: &str) -> i32 {
        self.sessions
            .get(tab_id)
            .map(|live| i32::from(live.grid.1.saturating_sub(1).max(1)))
            .unwrap_or(24)
    }

    /// 滚轮：翻视口，不动物理滚动。
    pub(super) fn on_terminal_scroll(
        &mut self,
        tab_id: &str,
        event: &ScrollWheelEvent,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some(live) = self.sessions.get(tab_id) else {
            return;
        };
        // 格子高度为 0 时除法会变成 inf：先挡掉（正常路径下 `sync_cell_metrics`
        // 已经确保它为正，这里只是不把「万一」变成「除以零」）。
        if live.cell.height <= 0. || live.session().is_none() {
            return;
        }
        let (height, remainder) = (live.cell.height, live.scroll_remainder);
        // 符号：GPUI 的 `delta.y` 为正 = 向上滚（看更早的内容），本函数内部约定
        // 「正数 = 向上翻」（与 [`Session::scroll`] 同向）—— 两者同向，**不再取负**。
        // 平台侧的原样透传见 `gpui_macos/src/events.rs`（`scrollingDeltaY` 直取），
        // 因此这里用的就是系统「自然滚动」设置下的方向；取负会让滚轮与键盘
        // （`ScrollLineUp` → `scroll_tab(+1)`）互相矛盾。
        let lines = match event.delta {
            // 传统滚轮：一格固定 3 行。
            ScrollDelta::Lines(delta) => delta.y * SCROLL_LINES_PER_NOTCH,
            // 触控板 / 精确滚动：按像素换行，余量累积到下一帧。
            ScrollDelta::Pixels(delta) => (delta.y.to_f64() / f64::from(height)) as f32,
        };
        let total = remainder + lines;
        let whole = total.trunc();
        if let Some(live) = self.sessions.get_mut(tab_id) {
            live.scroll_remainder = total - whole;
        }
        if whole == 0. {
            return;
        }

        // 应用要求上报滚轮（`CSI ?1000` 之类）且用户没按 Shift → 每一格编成一个滚轮
        // 事件转给应用（vim / htop / tmux 靠它滚动）；否则自己翻视口。
        if !event.modifiers.shift {
            let up = whole > 0.;
            // 触控板的像素位移换算出的整行数可能很大，钳一下：几百条同样的滚轮
            // 事件对终端应用没有额外意义，只会把它的输入队列灌满。
            let steps = (whole.abs() as u32).min(64);
            let mut reported = false;
            for _ in 0..steps {
                if self.report_mouse(
                    tab_id,
                    MouseAction::Scroll { up },
                    event.position,
                    event.modifiers,
                    cx,
                ) {
                    reported = true;
                }
            }
            if reported {
                return;
            }
        }

        if let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) {
            session.scroll(whole as i32);
            cx.notify();
        }
    }

    /// 终端画布。
    ///
    /// `tab_id` 实际是**会话键**（本模块的一贯口径：单窗格时它就等于标签 id，
    /// 分屏右窗格则是 `<标签 id>#2`）—— 动作绑定、尺寸回传、元素 id 全按它走，
    /// 所以两半画布各管各的会话，同一个函数画两遍就成。
    /// `split` 只用来挑无障碍名字：两个都叫「终端」的输入区，读屏分不清。
    pub(super) fn render_terminal_session(
        &self,
        tab_id: &str,
        live: &TerminalSession,
        split: bool,
        cx: &mut Context<TerminalPage>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        // 字体族与内边距取「设置 → 终端」的偏好（出厂默认 = 跟随主题）。
        // 与 `sync_cell_metrics` 读同一份设置，两处的字体族因此必然一致
        //（度量与实际绘制用不同字体族会让整屏错位）。
        let prefs = super::module_settings::terminal_settings(cx);
        let mono: SharedString = mono_family(&prefs, tokens.typography.mono.clone());
        let font_size = px(self.font_size(cx));
        let cell = live.cell;
        let line_height = px(cell.height);

        let base_font = mono_font(&prefs, mono.clone());

        // 画布内边距：终端文字不贴边（与页面其余内容区同一对 token）。
        // 子元素的 `Bounds` 会自动扣掉它，`apply_canvas_size` 的列行换算因此天然正确。
        // 设置页给了值就按它（四周同宽，避免「上下 4 / 左右 12」这种要量两次的输入）。
        let pad = prefs.padding.map(|value| px(value as f32));
        let pad_x = pad.unwrap_or(tokens.spacing.md);
        let pad_y = pad.unwrap_or(tokens.spacing.sm);

        let Some(session) = live.session() else {
            return self.render_terminal_failure(cx);
        };

        // 一次加锁把整屏读出来（拿不到就沿用上一帧）：
        // 返回 `None` 时 `rows` 为空，画布只剩底色 —— 下一帧会补上。
        let paint = session.try_with_term(|term| {
            let content = term.renderable_content();
            // `display_iter` 吐的是**视口坐标**（可能为负，往上翻时），
            // 加上 `display_offset` 才是 0..screen_lines 的行号。
            let offset = content.display_offset as i32;
            let colors = content.colors;
            let screen_lines = Dimensions::screen_lines(term);
            let columns = Dimensions::columns(term);

            let mut lines: Vec<Vec<CellPaint>> = (0..screen_lines)
                .map(|_| Vec::with_capacity(columns))
                .collect();
            // 悬停中的链接：内核已经算好它占哪几行哪几列，这里只做区间命中。
            // 跨格 / 跨行回绕的链接因此能被整体标出来 —— 逐格比区间，而不是比「哪一格被指着」。
            let hovered = live.hovered_link.as_ref().map(|link| &link.ranges);
            for indexed in content.display_iter {
                let row = indexed.point.line.0 + offset;
                if !(0..screen_lines as i32).contains(&row) {
                    continue;
                }
                let mut painted = paint_cell(indexed.cell, colors, &live.palette);
                // 不用 let-chain：本仓 MSRV 是 1.85，let-chain 要 1.88。
                let hovered_here = hovered.is_some_and(|ranges| {
                    let line = indexed.point.line.0;
                    let column = indexed.point.column.0;
                    ranges
                        .iter()
                        .any(|(range_line, range)| *range_line == line && range.contains(&column))
                });
                if hovered_here {
                    // 悬停的链接临时加一条下划线（内核没给下划线的那种链接也看得出来）。
                    if painted.underline == UnderlineKind::None {
                        painted.underline = UnderlineKind::Single;
                    }
                }
                lines[row as usize].push(painted);
            }

            let mut rows: Vec<AnyElement> = Vec::with_capacity(screen_lines);
            for cells in &lines {
                // 一行 = 若干**按列定位**的段（见 [`build_segments`]）：段内可流式，
                // 段与段之间按 `col × cell.width` 对齐，宽字符的宽度误差因此不跨段累积。
                // 纯 ASCII 行仍然只有一段，元素数量与从前一样。
                let mut row_div = div()
                    .relative()
                    .h(line_height)
                    .flex_none()
                    // 行是**网格行**，不是流式文本：一行就是一行，绝不换行。
                    // 换行的续行会溢出定高行盒、叠在下一行上 —— `ll` 这类长行
                    // 一多就满屏鬼影。超宽的部分由画布 `overflow_hidden` 裁掉。
                    .whitespace_nowrap();
                for segment in build_segments(cells, &base_font) {
                    row_div = row_div.child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(segment.col as f32 * cell.width))
                            .child(gpui::StyledText::new(segment.text).with_runs(segment.runs)),
                    );
                }
                rows.push(row_div.into_any_element());
            }

            let cursor_row = content.cursor.point.line.0 + offset;
            let cursor = (0..screen_lines as i32)
                .contains(&cursor_row)
                .then_some(CursorPaint {
                    row: cursor_row as usize,
                    column: content.cursor.point.column.0,
                    shape: content.cursor.shape,
                });
            // 选区直接用内核给的 `SelectionRange`（网格坐标）平移成可视行 ——
            // 选区是内核状态的一部分，跟着滚动/重排自己走，宿主**不另存一份**。
            let bands = content.selection.map_or_else(Vec::new, |range| {
                selection_bands(range, offset, screen_lines, columns)
            });
            // 搜索高亮同理，只是来源换成 `⌘F` 的命中表（换算在同一处做，口径一致）。
            let find_bands = self.find_highlight(tab_id, offset, screen_lines, columns);
            // shell 集成标记（`OSC 133`）：每条提示符所在的行要不要出一道标记，
            // 以及它上一条命令的结果 —— 判定全在内核（`prompt_bars`），宿主只做定位。
            // 没装 shell 集成脚本的会话这里就是空的，不报错、不占位。
            let prompt_marks: Vec<(usize, Option<i32>)> =
                prompt_bars(&session.shell_marks_in(term))
                    .into_iter()
                    .filter_map(|(line, exit)| {
                        let row = line + offset;
                        // 行号是网格坐标，加 `display_offset` 才是屏幕行；屏幕外的丢掉。
                        (0..screen_lines as i32)
                            .contains(&row)
                            .then_some((row as usize, exit))
                    })
                    .collect();
            (rows, cursor, bands, find_bands, prompt_marks)
        });

        let (rows, cursor, bands, find_bands, prompt_marks) = paint.unwrap_or_default();
        // 把光标格记进会话：IME 的候选窗口定位（`bounds_for_range`）与预编辑覆盖层
        // 都要用它。`Cell` 让这条 `&self` 的渲染路径能更新状态（不重排、不 notify）。
        if let Some(cursor) = cursor.as_ref() {
            live.ime_cursor
                .set((cursor.column as i32, cursor.row as i32));
        }
        // 输入法**组合中**的预编辑串：画在光标格上（与行、光标共享 wrapper 的原点）。
        // 用终端底色垫底盖住光标格原本的字符，再叠一段带下划线的组合文本 ——
        // 与 macOS 原生输入框的观感一致（候选在候选窗里，画布上只显示这串）。
        let preedit = (!live.preedit.is_empty())
            .then_some(cursor.as_ref())
            .flatten()
            .map(|cursor| {
                let run = TextRun {
                    len: live.preedit.len(),
                    font: base_font.clone(),
                    color: foreground(&live.palette),
                    background_color: None,
                    underline: Some(UnderlineStyle {
                        color: None,
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                div()
                    .absolute()
                    .left(px(cursor.column as f32 * cell.width))
                    .top(px(cursor.row as f32 * cell.height))
                    .h(line_height)
                    .whitespace_nowrap()
                    .bg(rgb_of(&live.palette.background))
                    .child(gpui::StyledText::new(live.preedit.clone()).with_runs(vec![run]))
                    .into_any_element()
            });
        // 建议弹层：锚在光标格上。候选是 `refresh_suggestions` 上一轮算的 ——
        // 比本帧网格旧半帧是刻意的取舍（刷新跟随 Wakeup，不为它单独开锁）。
        let suggest_overlay = (live.suggest.visible())
            .then(|| {
                let raw = cursor.as_ref()?;
                suggest_popup(
                    &live.suggest.items,
                    live.suggest.selected,
                    raw.row,
                    raw.column,
                    cell,
                    line_height,
                    live.grid.1 as usize,
                    cx,
                )
            })
            .flatten();
        // 光标由 `cursor_shape` 直接以「行 × 列」定位：它是 wrapper 的绝对定位子元素，
        // 而 wrapper 就是行的父容器 —— 两者**共享同一原点**，不做任何内边距补偿。
        // （曾在光标坐标里加 pad_x/pad_y 去凑外层容器的 padding box，实测真机上绝对
        // 定位的包含块是这层 wrapper 而不是外层容器，补偿恰好多算了一个内边距，
        // 表现为「光标块比文字低一行内边距」。把原点统一到 wrapper 后此类歧义消失。）
        let cursor = cursor.map(|cursor| cursor_shape(cursor, cell, cursor_color(&live.palette)));

        // 选区高亮：一叠绝对定位的色块。
        //
        // 三个要点：
        // 1. 它们是行的**兄弟且排在前面** —— 兄弟的绘制顺序就是叠放顺序，
        //    先画的在下，于是高亮垫在文字底下、光标仍压在最上面；
        // 2. 绝对定位贴着**直接父级**解析（GPUI 的包含块不是 CSS 的「最近定位祖先」），
        //    所以 `left/top` 直接用「列 × 格宽 / 行 × 行高」，与行、光标共享同一原点；
        // 3. 颜色取主题里唯一的「强调色半透明层」（`drop_target`，20% 强调色）：
        //    终端底色来自它自己的调色板（深色主题近黑、浅色近白），
        //    只有**半透明**层才能在两种底色上都压得住文字。
        let selection_bg = theme.extended().colors.drop_target;
        // 搜索命中用**两档**：其余命中是中性薄层（`hover_tint`），当前命中用强调色薄层
        //（`drop_target`）—— 两者都不遮字，但强弱一眼可辨。选区仍用 `drop_target`：
        // 「当前命中」与「选区」长相接近是可以接受的（它们不会同时被用），
        // 而「命中 vs 非当前命中」必须能分辨，否则 Enter 就没有反馈。
        let match_bg = theme.extended().colors.hover_tint;
        let current_match_bg = selection_bg;
        let find_highlight: Vec<AnyElement> = find_bands
            .into_iter()
            .map(|(row, first, last, current)| {
                div()
                    .absolute()
                    .left(px(first as f32 * cell.width))
                    .top(px(row as f32 * cell.height))
                    .w(px((last - first + 1) as f32 * cell.width))
                    .h(line_height)
                    .bg(if current { current_match_bg } else { match_bg })
                    .into_any_element()
            })
            .collect();
        let highlight: Vec<AnyElement> = bands
            .into_iter()
            .map(|(row, first, last)| {
                div()
                    .absolute()
                    .left(px(first as f32 * cell.width))
                    .top(px(row as f32 * cell.height))
                    .w(px((last - first + 1) as f32 * cell.width))
                    .h(line_height)
                    .bg(selection_bg)
                    .into_any_element()
            })
            .collect();
        // 命令标记：**2px 宽的竖条**贴在最左边，而不是整行铺色。
        //
        // 位置取舍：绝对定位的包含块是这层 wrapper（见上方光标那段注释），而 wrapper 带
        // `overflow_hidden` —— 想把它挪进外层容器的内边距里就得用负 `left`，那会被裁掉。
        // 于是它压在第一列字符的最左侧 2px 上：字形本身不会被盖住，读起来仍是一条「页边标」。
        //
        // 颜色：上一条命令**非零退出**用错误色，其余（含没报退出码）用中性强调薄层 ——
        // 「刚才那步成没成」在一屏滚动里最难找，这条标记是给它的锚点。
        let mark_failed = tokens.colors.destructive.alpha(MARK_FAILED_ALPHA);
        let mark_neutral = theme.extended().colors.hover_tint;
        let prompt_marks: Vec<AnyElement> = prompt_marks
            .into_iter()
            .map(|(row, exit)| {
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(row as f32 * cell.height))
                    .w(px(MARK_WIDTH))
                    .h(line_height)
                    .bg(if exit.is_some_and(|code| code != 0) {
                        mark_failed
                    } else {
                        mark_neutral
                    })
                    .into_any_element()
            })
            .collect();

        let focus = live.focus.clone();
        // 断线重连入口：SSH 会话退出（多半是连接断了）后，画布底部给一条可操作的横幅。
        // 本地 shell 退出走的是「关标签」，不会走到这里 —— 它的 `plan.target` 是 `None`。
        //
        // **这是全页唯一的一条断线横幅**：`terminal/render_terminal.rs` 里那条
        // 「已断开 + 重新连接」的兄弟实现已经删掉（它俩同时出现在屏幕上，文案不同、
        // 按钮语义也不同 —— 一个摘掉会话重来、一个走自动重连，用户只能猜）。
        //
        // 第二行反映**自动重连状态**：排定中给「第 n/N 次 · m 秒后重试」，
        // 已停止说明理由；手动「重连」按钮始终在，即使自动重连已停也留一条退路。
        let reconnect_hint: Option<String> = match live.reconnect {
            ReconnectState::Pending { driver, due } => {
                // 向上取整到秒：`as_secs()` 截断会让「0 秒后重试」闪一下。
                let remaining = due.saturating_duration_since(std::time::Instant::now());
                let secs = remaining.as_secs() + u64::from(remaining.subsec_millis() > 0);
                Some(trf(
                    cx,
                    "自动重连 第 {}/{} 次 · {} 秒后重试",
                    &[
                        &driver.attempts.to_string(),
                        &driver.max_attempts.to_string(),
                        &secs.max(1).to_string(),
                    ],
                ))
            }
            ReconnectState::Armed { driver } => Some(trf(
                cx,
                "自动重连 第 {}/{} 次 · 正在连接…",
                &[
                    &driver.attempts.to_string(),
                    &driver.max_attempts.to_string(),
                ],
            )),
            ReconnectState::Stopped => Some(tr(cx, "已停止自动重连。").to_owned()),
            ReconnectState::Idle => None,
        };
        let reconnect = (live.exited
            && live.plan.as_ref().is_some_and(|plan| plan.target.is_some()))
        .then(|| {
            let key = tab_id.to_owned();
            v_flex()
                .absolute()
                .left(px(0.))
                .right(px(0.))
                .bottom(px(0.))
                .items_center()
                .gap(tokens.spacing.xs)
                .py(tokens.spacing.sm)
                .bg(theme.extended().colors.chrome)
                .border_t(theme.extended().stroke.thin)
                .border_color(theme.extended().colors.hairline)
                .child(body_text(tr(cx, "连接已断开。"), cx))
                .when_some(reconnect_hint, |this, hint| this.child(meta_text(hint, cx)))
                .child(
                    button(
                        SharedString::from(format!("terminal-reconnect-{tab_id}")),
                        tr(cx, "重连"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.reconnect_session(&key, cx);
                    })),
                )
                .into_any_element()
        });
        div()
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .px(pad_x)
            .py(pad_y)
            .bg(rgb_of(&live.palette.background))
            .font_family(mono)
            .text_size(font_size)
            .line_height(line_height)
            // 指针形状即「这里可点」的提示：悬停在链接上时变成小手。
            .cursor(if live.hovered_link.is_some() {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            })
            // 尺寸回传：`on_children_prepainted` 给的是子元素的 `Bounds`，
            // 子元素是 `size_full`，于是这里拿到的是画布真实可绘区。
            // 必须挂在 `.id()` **之前** —— 它定义在 `Div` 上并消费 `self`，
            // 一旦变成 `Stateful<Div>` 就找不到了（`Stateful` 的 `Deref` 救不了按值收 self 的方法）。
            .on_children_prepainted({
                let view = self.view.clone();
                let measured_id = tab_id.to_owned();
                move |bounds, _window, cx| {
                    let Some(bounds) = bounds.first().copied() else {
                        return;
                    };
                    let id = measured_id.clone();
                    let _ = view.update(cx, |page, cx| page.apply_canvas_bounds(&id, bounds, cx));
                }
            })
            .id(SharedString::from(format!("terminal-canvas-{tab_id}")))
            // 自绘输入区：读屏要知道这里能打字，名字必须显式给（不会从子节点派生）。
            .role(Role::TextInput)
            .aria_label(if split {
                tr(cx, "终端（分屏）")
            } else {
                tr(cx, "终端")
            })
            // 焦点在画布上时，按键派发路径里就有了 `"Terminal"` 上下文 ——
            // 上面那对 Tab 动作绑定因此比 `Root` 的焦点遍历更深、优先命中。
            .key_context("Terminal")
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &SendTab, _window, cx| {
                    // 弹层可见时 Tab 是「接受建议」，不再是制表符。
                    if this.suggest_visible(&tab_id) {
                        this.suggest_accept(&tab_id, cx);
                        cx.stop_propagation();
                        return;
                    }
                    this.send_encoded(&tab_id, KeyStroke::key("tab"), cx);
                }
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &SendTabPrev, _window, cx| {
                    let mut stroke = KeyStroke::key("tab");
                    stroke.shift = true;
                    this.send_encoded(&tab_id, stroke, cx);
                }
            }))
            // Ctrl-C：非 macOS 上它被 `gpui_base::Root` 绑给了 `Copy`（`root.rs:21`），
            // 不在这里盖掉，Linux / Windows 用户的 SIGINT 就是死的。
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &SendCtrlC, _window, cx| {
                    // Ctrl-C 会清掉整行输入：建议随之作废，先收弹层再发字节。
                    if let Some(live) = this.sessions.get_mut(&tab_id) {
                        live.suggest.clear();
                    }
                    this.send_ctrl(&tab_id, "c", cx)
                }
            }))
            // 字号快捷键与工具条 ± 共用同一份状态（`terminal_font_size`），钳位也在同处。
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| this.nudge_font_size(1., cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| this.nudge_font_size(-1., cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _window, cx| this.reset_font_size(cx)))
            // 清屏：动作就在**某个窗格里**派发，所以直接清这一格 —— 分屏时
            // 「⌘K 清了哪一半」不该由页面去猜（需要窗口焦点信息，工具条那条路径
            // 走 `clear_active`）。
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &Clear, _window, cx| this.clear_session(&tab_id, cx)
            }))
            // 分屏 / 取消分屏（⌘D）：动作从画布派发，所以作用的就是当前标签
            //（用户在哪一半按都算数 —— 分屏属于标签，不属于某一半）。
            .on_action(cx.listener(|this, _: &SplitPane, _window, cx| this.toggle_split(cx)))
            // 视口滚动族：只挪视口，不向 PTY 写字节 —— 与滚轮同语义。
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollLineUp, _window, cx| this.scroll_tab(&tab_id, 1, cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollLineDown, _window, cx| this.scroll_tab(&tab_id, -1, cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollPageUp, _window, cx| {
                    let lines = this.page_lines(&tab_id);
                    this.scroll_tab(&tab_id, lines, cx);
                }
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollPageDown, _window, cx| {
                    let lines = this.page_lines(&tab_id);
                    this.scroll_tab(&tab_id, -lines, cx);
                }
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollToTop, _window, cx| {
                    this.scroll_tab_to_edge(&tab_id, false, cx);
                }
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &ScrollToBottom, _window, cx| {
                    this.scroll_tab_to_edge(&tab_id, true, cx);
                }
            }))
            // 行编辑便利键：发出的是**控制字节**（Ctrl-A/E/U/K）而不是光标按键 ——
            // 与 zed 的 `["terminal::SendKeystroke","ctrl-e"]` 等价，靠 shell 的行编辑生效。
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &MoveToLineStart, _window, cx| this.send_ctrl(&tab_id, "a", cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &MoveToLineEnd, _window, cx| this.send_ctrl(&tab_id, "e", cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &DeleteToLineStart, _window, cx| this.send_ctrl(&tab_id, "u", cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &DeleteToLineEnd, _window, cx| this.send_ctrl(&tab_id, "k", cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &DeleteWordForward, _window, cx| {
                    // `⌥⌦` = `ESC d`：readline / zsh 的「删到词尾」。直接写字节而不是走
                    // `named_key("delete")` —— 后者会变成 `CSI 3;3~`（xterm 的 Alt-Delete），
                    // 与 zed 的 Terminal.app 兼容映射（`\u001bd`）不是同一个东西。
                    this.send_encoded(
                        &tab_id,
                        KeyStroke {
                            key: "d",
                            text: Some("d"),
                            ctrl: false,
                            alt: true,
                            shift: false,
                            platform: false,
                        },
                        cx,
                    );
                }
            }))
            // —— 剪贴板 / 选区 ——
            // `⌘C` 在 `"Root"` 上下文被绑给了 `Copy`（`root.rs:19`），`⌘A` 被绑给了
            // `SelectAll`：不在这里用同键动作盖掉，它们永远到不了「复制选中的文本」
            // 这条语义上。三条都绑定在 `"Terminal"` 上下文（键位表见 `app::install`）。
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &CopySelection, _window, cx| this.copy_selection(&tab_id, cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &PasteFromClipboard, window, cx| {
                    this.paste_from_clipboard(&tab_id, window, cx);
                }
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &SelectAll, _window, cx| this.select_all(&tab_id, cx)
            }))
            // —— 标签 / 查找 ——
            // 这三个动作**不在这里处理**：它们的作用对象是「整页」（标签条、查找条），
            // 键位也挂在页面级 `"TerminalPage"` 上下文、处理在页面根上。
            // 在画布上也挂一份会让一次按键触发两次（画布 + 页面根都会收到）。
            // `track_focus` 只置 `focusable`、不置 `tab_stop`，不补这一行 Tab 键到不了这里。
            .tab_stop(true)
            .track_focus(&focus)
            .on_scroll_wheel(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, event, _window, cx| this.on_terminal_scroll(&tab_id, event, cx)
            }))
            .on_key_down(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, event, _window, cx| this.on_terminal_key(&tab_id, event, cx)
            }))
            // —— 鼠标：应用要求上报时转给 PTY，否则做本地聚焦 + 拖选 ——
            //
            // 选区状态归内核（`Term::selection`），这里只做「像素 → 网格坐标」的换算与
            // 起止时机；高亮由 `selection_bands` 那叠色块画出来。
            //
            // vim / htop / tmux 这类全屏应用一进去就开 `CSI ?1000/1002/1003`，声明
            // 「鼠标事件由我处理」：那时按下 / 抬起 / 拖动 / 滚轮都要编码成 CSI 序列写回
            // PTY（编码在内核 `duck_term::encode_mouse`）。**按住 Shift 是逃逸键** ——
            // 临时改回本地选择，用户想复制应用里的一段文字时不必先退出应用。
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener({
                    let handle = focus.clone();
                    let tab_id = tab_id.to_owned();
                    move |this, event: &MouseDownEvent, window, cx| {
                        // 记下「用户正在哪一半打字」：工具条上的清屏 / 取消分屏要按它
                        // 定位窗格（焦点句柄本身读不到 —— 那只在 `Window` 里）。
                        this.focus_pane = Some(tab_id.clone());
                        window.focus(&handle, cx);
                        // ⌘+点击链接 = 用系统默认程序打开。放在最前面：这是「网页式」
                        // 意图，优先于应用自身的鼠标上报与本地拖选（与 macOS 端一致）。
                        if event.modifiers.platform
                            && this.open_link_at(&tab_id, event.position, window, cx)
                        {
                            return;
                        }
                        if !event.modifiers.shift
                            && this.report_mouse(
                                &tab_id,
                                MouseAction::Press(TermMouseButton::Left),
                                event.position,
                                event.modifiers,
                                cx,
                            )
                        {
                            return;
                        }
                        // 手势粒度：`click_count` 由平台给出（双击 = 2，三击及以上 = 3+）；
                        // ⌥ 让拖拽走矩形块选。
                        this.begin_selection(
                            &tab_id,
                            event.position,
                            event.click_count,
                            event.modifiers.alt,
                            cx,
                        );
                    }
                }),
            )
            // 中键按下也转给应用（xterm 的按钮码 1）；右键不接 —— 它被 `context_menu`
            // 拿去做本地菜单了，两条路同时接会让右键既弹菜单又上报。
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener({
                    let tab_id = tab_id.to_owned();
                    move |this, event: &MouseDownEvent, _window, cx| {
                        if event.modifiers.shift {
                            return;
                        }
                        this.report_mouse(
                            &tab_id,
                            MouseAction::Press(TermMouseButton::Middle),
                            event.position,
                            event.modifiers,
                            cx,
                        );
                    }
                }),
            )
            .on_mouse_move(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, event: &MouseMoveEvent, _window, cx| {
                    // 链接悬停先算：它决定指针形状与下划线，与「谁处理这次移动」无关
                    //（上报表模式里应用拿到的是坐标，链接高亮仍归宿主画）。
                    this.update_hovered_link(&tab_id, event.position, event.modifiers.platform, cx);
                    // 应用要求上报移动（1002 拖拽 / 1003 全量）时转给应用，不做本地拖选。
                    if !event.modifiers.shift && this.mouse_tracking_motion(&tab_id) {
                        let held = event.pressed_button.and_then(term_button);
                        if this.report_mouse(
                            &tab_id,
                            MouseAction::Motion { held },
                            event.position,
                            event.modifiers,
                            cx,
                        ) {
                            return;
                        }
                    }
                    if event.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    this.drag_selection(&tab_id, event.position, cx);
                }
            }))
            // 松手结束拖选。两条路都要走：`on_mouse_up` 只在**指针还在画布上**时回调，
            // 拖到画布外松手走 `on_mouse_up_out` —— 少了后者，指针一旦出界就再也不会
            // 结束这次拖选，回到画布上继续移动会把选区一路拖过去。
            .on_mouse_up(
                MouseButton::Left,
                cx.listener({
                    let tab_id = tab_id.to_owned();
                    move |this, event: &MouseUpEvent, _window, cx| {
                        if !event.modifiers.shift
                            && this.report_mouse(
                                &tab_id,
                                MouseAction::Release(TermMouseButton::Left),
                                event.position,
                                event.modifiers,
                                cx,
                            )
                        {
                            return;
                        }
                        this.end_selection(&tab_id);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener({
                    let tab_id = tab_id.to_owned();
                    move |this, event: &MouseUpEvent, _window, cx| {
                        // 指针已离开画布也要把抬起发给应用：否则它会一直以为按钮还按着。
                        if !event.modifiers.shift {
                            this.report_mouse(
                                &tab_id,
                                MouseAction::Release(TermMouseButton::Left),
                                event.position,
                                event.modifiers,
                                cx,
                            );
                        }
                        this.end_selection(&tab_id);
                    }
                }),
            )
            // 右键菜单：终端里最常做的事都在这儿，不必先去找工具条上的按钮。
            //
            // 「复制」按**有没有选区**置灰，「复制连接信息」按**这个窗格有没有地址**
            // 置灰 —— 菜单因此如实反映「现在能做什么」，而不是点下去才发现点了没用。
            // 这两个布尔都在渲染期算：`selection_range` 走 `try_with_term`，拿不到锁
            // 当没有，不会把 UI 线程按在 PTY 上。
            .context_menu({
                let view = self.view.clone();
                let menu_tab_id = tab_id.to_owned();
                let has_selection = session.selection_range().is_some();
                let address = self.address_for_session(tab_id);
                let split_active = self.active_tab().is_some_and(|tab| tab.split.is_some());
                move |menu, _, cx| {
                    let menu = menu
                        .item(
                            PopupMenuItem::new(tr(cx, "复制"))
                                .disabled(!has_selection)
                                .on_click({
                                    let tab_id = menu_tab_id.clone();
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.copy_selection(&tab_id, cx);
                                        });
                                    }
                                }),
                        )
                        .item(PopupMenuItem::new(tr(cx, "粘贴")).on_click({
                            let tab_id = menu_tab_id.clone();
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.paste_from_clipboard(&tab_id, window, cx);
                                });
                            }
                        }))
                        // 「粘贴并执行」= 折成一行 + ⏎：粘一长串参数再敲回车的固定动作，
                        // 折行是它有别于「粘贴」的全部意义（否则多行会逐行执行）。
                        .item(PopupMenuItem::new(tr(cx, "粘贴并执行")).on_click({
                            let tab_id = menu_tab_id.clone();
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.paste_and_run(&tab_id, window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new(tr(cx, "全选")).on_click({
                            let tab_id = menu_tab_id.clone();
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.select_all(&tab_id, cx));
                            }
                        }));
                    menu.separator()
                        .item(PopupMenuItem::new(tr(cx, "清屏")).on_click({
                            let tab_id = menu_tab_id.clone();
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.clear_session(&tab_id, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new(tr(cx, "查找…")).on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.open_find(window, cx));
                            }
                        }))
                        .item(
                            PopupMenuItem::new(if split_active {
                                tr(cx, "取消分屏")
                            } else {
                                tr(cx, "分屏")
                            })
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| this.toggle_split(cx));
                                }
                            }),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(tr(cx, "复制连接信息"))
                                .disabled(address.is_none())
                                .on_click({
                                    let address = address.clone();
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let Some(address) = address.clone() else {
                                            return;
                                        };
                                        let _ = view.update(cx, |this, cx| {
                                            this.copy_connection_text(address, window, cx);
                                        });
                                    }
                                }),
                        )
                        .item(PopupMenuItem::new(tr(cx, "增大字号")).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(1., cx));
                            }
                        }))
                        .item(PopupMenuItem::new(tr(cx, "减小字号")).on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(-1., cx));
                            }
                        }))
                }
            })
            .child(
                // 行与光标的**共同坐标系**：`relative` 让绝对定位的光标与选区高亮
                // 以本层为包含块，行是本层的流式子元素 —— 三者从结构上就是同原点。
                v_flex()
                    .relative()
                    .w_full()
                    .h_full()
                    .overflow_hidden()
                    // 高亮排在行**前面**：兄弟的绘制顺序即叠放顺序，先画的垫在下面，
                    // 于是文字压在选区底色上、光标仍压在最上面。搜索高亮再垫在更下面一层
                    //（与选区重叠时以选区为准）。命令标记垫在最底下：它只占最左 2px，
                    // 即使与选区重叠也不该把选区颜色改掉。
                    .children(prompt_marks)
                    .children(find_highlight)
                    .children(highlight)
                    .children(rows)
                    .when_some(cursor, |this, cursor| this.child(cursor))
                    // 预编辑覆盖层压在光标之上：组合中的那串字必须盖住光标格。
                    .when_some(preedit, |this, preedit| this.child(preedit))
                    // 建议弹层压在预编辑之上：它只会在非组合期出现，两者不相逢，
                    // 顺序只为可读性。
                    .when_some(suggest_overlay, |this, popup| this.child(popup))
                    // 断线横幅压在最上层：它是唯一可点的操作，不能被子内容盖住。
                    .when_some(reconnect, |this, banner| this.child(banner))
                    // 挂 IME：一块空画布，paint 阶段把 `ElementInputHandler` 注册给本画布的
                    // 焦点句柄（`window.handle_input` 只在焦点命中时才真的登记）。它铺满
                    // wrapper，于是 `bounds` 就是文字区，与行 / 光标同源 —— `bounds_for_range`
                    // 的候选窗口定位因此不需要任何内边距补偿。`w_full/h_full` 的绝对定位
                    // 子元素不参与行布局，行仍是流式的、原点不变。
                    .child({
                        let view = self.view.clone();
                        let handle = focus.clone();
                        gpui::canvas(
                            |_, _, _| {},
                            move |bounds, (), window, cx| {
                                if let Some(view) = view.upgrade() {
                                    window.handle_input(
                                        &handle,
                                        gpui::ElementInputHandler::new(bounds, view),
                                        cx,
                                    );
                                }
                            },
                        )
                        .absolute()
                        .top(px(0.))
                        .left(px(0.))
                        .w_full()
                        .h_full()
                    }),
            )
            .into_any_element()
    }

    /// 起 shell 失败时的替代内容。
    pub(super) fn render_terminal_failure(&self, cx: &mut Context<TerminalPage>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .child(
                Icon::new(DuckIcon::TriangleAlert)
                    .size(theme.extended().icon.lg)
                    .text_color(tokens.colors.destructive),
            )
            .child(title_text(tr(cx, "本地终端未启动"), cx))
            .child(body_text(
                tr(cx, "创建 PTY 失败。关闭该标签后重开一次试试。"),
                cx,
            ))
            .into_any_element()
    }
}

/// IME / 文本输入：把系统输入法的组合期与上屏接进终端。
///
/// 终端是**自绘**的输入区（`track_focus` + `on_key_down` 只覆盖「按键 → 字节」），
/// 组合期（拼音候选）的按键根本到不了 `on_key_down` —— macOS 要把按键交给输入法，
/// 前提是窗口里挂了一个 `InputHandler`（画布的 paint 回调里 `window.handle_input`）。
/// 本 impl 就是那个 handler 背后的逻辑：
///
/// - **组合中**（[`Self::replace_and_mark_text_in_range`]）：把预编辑串存进**当前聚焦
///   的那块画布**，画布把它画在光标格上（与行、光标共享同一原点）。这一步不写 PTY。
/// - **上屏**（[`Self::replace_text_in_range`]）：把提交文本按 [`duck_term::encode`]
///   的文本通道写进 PTY，并清掉预编辑串。
/// - **取消**（[`Self::unmark_text`]）：只清预编辑串。
///
/// 目标会话键由 [`Self::ime_session_key`] 用窗口的实际焦点句柄反查 —— 分屏时两块
/// 画布各有自己的焦点句柄，永远不会串台。
impl gpui::EntityInputHandler for TerminalPage {
    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // 终端没有可供输入法查询的文本模型（组合串总在末尾追加），如实返回 `None`。
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        // 选区由终端内核管理（拖选复制），与 IME 的组合范围无关。
        None
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        // 组合串始终占据「文档」的 0..len 区间（文档就是这一小段预编辑）。
        let key = self.ime_session_key(window)?;
        let len = self.sessions.get(&key)?.preedit.encode_utf16().count();
        (len > 0).then_some(0..len)
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.ime_session_key(window) {
            self.set_ime_preedit(&key, String::new(), cx);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.ime_session_key(window) {
            self.commit_ime_text(&key, text, cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.ime_session_key(window) {
            self.set_ime_preedit(&key, new_text.to_owned(), cx);
        }
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _element_bounds: gpui::Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        // 候选窗口跟随光标格：原点取画布文字区原点 + 光标格偏移，与行/光标同源。
        let key = self.ime_session_key(window)?;
        let live = self.sessions.get(&key)?;
        let (column, row) = live.ime_cursor.get();
        let cell = live.cell;
        Some(gpui::Bounds::new(
            gpui::Point {
                x: live.origin.x + px(column as f32 * cell.width),
                y: live.origin.y + px(row as f32 * cell.height),
            },
            Size {
                width: px(cell.width),
                height: px(cell.height),
            },
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// GPUI 的鼠标按钮 → 内核的上报按钮。
///
/// `Navigate`（侧键）不是终端协议里的按钮，映射成 `None`：移动事件里它就是
/// 「没有按住任何按钮」，按下 / 抬起事件则直接丢弃（见调用点的 `if let Some`）。
fn term_button(button: gpui::MouseButton) -> Option<TermMouseButton> {
    match button {
        gpui::MouseButton::Left => Some(TermMouseButton::Left),
        gpui::MouseButton::Middle => Some(TermMouseButton::Middle),
        gpui::MouseButton::Right => Some(TermMouseButton::Right),
        gpui::MouseButton::Navigate(_) => None,
    }
}

/// 一个单元格画出来需要的最少信息。
/// `SGR 2`（faint）把前景色压到多重的不透明度。
///
/// 0.6 是「看得出来更暗、但仍然读得清」的折中：再低（0.4）在深色主题上几乎消失，
/// 再高（0.8）与正常文字几乎没差别。
const DIM_FOREGROUND_ALPHA: f32 = 0.6;

/// 下划线的画法（SGR `4:0..4:5`）。
///
/// 只有 `wavy` 一个开关是不够的：`git diff` 用双下划线标「新增」，拼写检查用波浪线
/// 标「可疑」，两者在终端里必须一眼分得开。GPUI 的 `UnderlineStyle` 给了
/// `thickness` 与 `wavy` 两个自由度，正好一一对上；点线 / 虚线没有对应形状，
/// 退化成最细的直线（比「画成和单下划线一样粗」更接近原意）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UnderlineKind {
    /// 无下划线。
    None,
    /// 单下划线（`SGR 4`）。
    Single,
    /// 双下划线（`SGR 4:2`）。
    Double,
    /// 波浪线（`SGR 4:3`，拼写检查）。
    Curly,
    /// 点线（`SGR 4:4`）。
    Dotted,
    /// 虚线（`SGR 4:5`）。
    Dashed,
}

impl UnderlineKind {
    /// 由内核 flags 判档。多位同时置起时按「越特殊越优先」取一个。
    fn from_flags(flags: Flags) -> Self {
        if flags.contains(Flags::UNDERCURL) {
            Self::Curly
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            Self::Double
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Self::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Self::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            Self::Single
        } else {
            Self::None
        }
    }

    /// 翻译成 GPUI 的画法（`None` = 不画）。
    fn style(self, color: Option<Hsla>) -> Option<UnderlineStyle> {
        let (thickness, wavy) = match self {
            Self::None => return None,
            Self::Single => (1., false),
            Self::Double => (2., false),
            Self::Curly => (1., true),
            Self::Dotted | Self::Dashed => (0.5, false),
        };
        Some(UnderlineStyle {
            color,
            thickness: px(thickness),
            wavy,
        })
    }
}

/// 两条下划线画法是否相同（run 合并用）。
///
/// 不直接比 `Option<UnderlineStyle>`：那要 `UnderlineStyle: PartialEq`（外部类型，
/// 不保证有），逐字段比还顺带说明了「哪几项会影响合并」。
fn same_underline(left: Option<UnderlineStyle>, right: Option<UnderlineStyle>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.color == right.color
                && left.thickness == right.thickness
                && left.wavy == right.wavy
        }
        _ => false,
    }
}

struct CellPaint {
    /// 这一格要画的字形（**字符串**，不是单个 `char`）。
    ///
    /// 带组合记号的字（`e` + U+0301 → `é`）在 alacritty 里是**同一个格**：
    /// 基字符在 `cell.c`，组合记号在 `cell.zerowidth()`。只取 `cell.c` 会把它画成
    /// 光秃秃的 `e` —— 文件名、`é` / `ü` 变音、部分 emoji 全都不对。
    ch: String,
    fg: Hsla,
    /// 与终端底色不同才画（`None` = 用底色）。
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    /// 下划线的画法（SGR `4:x`）。
    underline: UnderlineKind,
    /// 下划线自己的颜色（SGR `58`）；`None` = 跟文字同色。
    underline_color: Option<Hsla>,
    strike: bool,
    /// 宽字符**占位格**（`WIDE_CHAR_SPACER` / `LEADING_WIDE_CHAR_SPACER`）。
    ///
    /// 它自己没有字形（`ch` 只是个替身空格），但**仍占一列**：分段时要跳过它的字形
    /// 累加列号，否则宽字符后面整行右移。
    spacer: bool,
    /// 宽字符本体（`WIDE_CHAR`）：占两列，字形由字体自己画宽。
    wide: bool,
}

/// 光标要画在哪、画成什么样。
struct CursorPaint {
    row: usize,
    column: usize,
    shape: CursorShape,
}

/// 单元格 → 画法。
///
/// 色值解析优先级：真彩色 > 终端自己的覆盖表（`OSC 4` 改过的 / 我们灌进去的）> 宿主配色。
fn paint_cell(cell: &duck_term::Cell, colors: &duck_term::Colors, palette: &Palette) -> CellPaint {
    let flags = cell.flags;
    // 宽字符占位格：它本身没有字形，前一个格子的字已经画了两列宽。
    if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
        return CellPaint {
            ch: " ".to_owned(),
            fg: foreground(palette),
            bg: None,
            bold: false,
            italic: false,
            underline: UnderlineKind::None,
            underline_color: None,
            strike: false,
            spacer: true,
            wide: false,
        };
    }

    let mut fg = resolve(cell.fg, colors, palette);
    let mut bg = resolve(cell.bg, colors, palette);

    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    // SGR 2（faint）：把前景色压暗。`DIM_BOLD` 也带 DIM 位，所以只看这一位。
    // 用 alpha 而不是「把颜色变黑一点」：终端底色可能是任意色，往透明方向靠
    // 与底色混出来的结果才是「暗」，直接算成黑在浅色主题上会变成一团污渍。
    if flags.contains(Flags::DIM) {
        fg = fg.opacity(DIM_FOREGROUND_ALPHA);
    }

    // 底色与终端默认底色一致时不画 run 背景：让容器底色透出来，
    // 少一层绘制，也让「行尾之外」的空白与「行内空白」看起来是同一种底。
    let is_default_bg = bg == rgb_of(&palette.background);
    // 基字符 + 组合记号：alacritty 把零宽字符挂在**同一个格**上，
    // 这里拼回一个字符串，交给排版当一个簇处理。
    let mut ch = String::with_capacity(4);
    ch.push(cell.c);
    if let Some(zerowidth) = cell.zerowidth() {
        ch.extend(zerowidth);
    }
    CellPaint {
        ch,
        fg,
        bg: (!is_default_bg).then_some(bg),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: UnderlineKind::from_flags(flags),
        // 下划线颜色是**独立**于前景色的（`SGR 58;…`）：`rli` / `git diff` 这类
        // 工具用它把「行内哪一段被改了」标出来，跟着前景色走就丢了这层信息。
        underline_color: cell
            .underline_color()
            .map(|color| resolve(color, colors, palette)),
        strike: flags.contains(Flags::STRIKEOUT),
        spacer: false,
        wide: flags.contains(Flags::WIDE_CHAR),
    }
}

/// 一行的文本累加器：一边写 `String` 一边记每个 run 的字节长。
///
/// GPUI 的 `TextRun::len` 是**字节数**，且 `with_runs` 会在「run 长度之和 ≠ 文本长度」时
/// 直接 panic（`elements/text.rs:526`）。所以长度必须逐格累加，不能事后按字符数推。
/// 相邻格子的样式完全相同就并进同一个 run —— 一整行白字只该产出一个 run，
/// 否则一屏 200 列 × 50 行会变成一万个 run。
struct RowText {
    text: String,
    runs: Vec<TextRun>,
    /// `(run, 已累计的字节长)`：`run.len` 要等这一格串结束才知道，先攒着。
    current: Option<(TextRun, usize)>,
}

impl RowText {
    fn with_capacity(cells: usize) -> Self {
        Self {
            text: String::with_capacity(cells),
            runs: Vec::new(),
            current: None,
        }
    }

    fn push(&mut self, cell: &CellPaint, font: &Font) {
        // 下划线的画法由「档位 + 颜色」两半决定，先算出来：合并判据与构造都用它，
        // 两处各算一遍迟早不一致（一处算走「有 / 无」，另一处算成具体粗细）。
        let underline = cell.underline.style(cell.underline_color);
        self.text.push_str(&cell.ch);
        let len = cell.ch.len();
        let matches = self.current.as_ref().is_some_and(|(run, _)| {
            run.color == cell.fg
                && run.background_color == cell.bg
                && (run.font.weight == FontWeight::BOLD) == cell.bold
                && (run.font.style == FontStyle::Italic) == cell.italic
                && same_underline(run.underline, underline)
                && run.strikethrough.is_some() == cell.strike
        });
        if !matches {
            if let Some((mut run, length)) = self.current.take() {
                run.len = length;
                self.runs.push(run);
            }
            self.current = Some((
                TextRun {
                    len: 0,
                    font: Font {
                        weight: if cell.bold {
                            FontWeight::BOLD
                        } else {
                            FontWeight::NORMAL
                        },
                        style: if cell.italic {
                            FontStyle::Italic
                        } else {
                            FontStyle::Normal
                        },
                        ..font.clone()
                    },
                    color: cell.fg,
                    background_color: cell.bg,
                    underline,
                    strikethrough: cell.strike.then_some(gpui::StrikethroughStyle {
                        color: None,
                        thickness: px(1.),
                    }),
                },
                0,
            ));
        }
        if let Some((_, length)) = self.current.as_mut() {
            *length += len;
        }
    }

    fn finish(mut self) -> (String, Vec<TextRun>) {
        if let Some((mut run, length)) = self.current.take() {
            run.len = length;
            self.runs.push(run);
        }
        (self.text, self.runs)
    }
}

/// 一行里按列定位的一段文字。
#[derive(Debug)]
struct RowSegment {
    /// 起始列（0 基）。
    col: usize,
    /// 这段的文本。
    text: String,
    /// 与 `text` 字节长严格对齐的 runs。
    runs: Vec<TextRun>,
}

/// 把一行切成**按列绝对定位**的段。
///
/// 为什么不能整行当一段流式文本排：
///
/// 1. **宽字符**：一个 CJK 字占两列，它的占位格在 [`paint_cell`] 里是个空格。
///    整行拼成 `"中 文 "` 交给排版时，字体画的 `中` 若比两格宽（或窄）一点点，
///    后面**每个字**都会跟着偏 —— 40 个汉字就能偏出半屏。终端不能靠字体自觉。
/// 2. **回退字体**：等宽字体常把 CJK 交给回退字体画，回退字体的推进量与主字体的
///    格宽本来就没有契约。整行流式排布等于把两套度量混在一个坐标系里。
///
/// 所以每段自带一个**起点列**，渲染时按 `left = col × cell.width` 绝对定位：
/// 段内错位不跨段累积，宽字符一律独占一段（错位只可能压在自己那两列里）。
///
/// 切段规则（顺序即优先级）：
/// - 占位格**不产出字形**，但列号照样 +1（否则宽字符之后整行左移一格）；
/// - 上一格是宽字符 → 断开（宽字符必须独占一段）；
/// - 本格是宽字符 → 断开（它自己起一段）；
/// - 样式变了 → 交给 [`RowText::push`] 自己断 run（同一段内可以多 run）。
fn build_segments(cells: &[CellPaint], font: &Font) -> Vec<RowSegment> {
    let mut segments: Vec<RowSegment> = Vec::new();
    let mut row = RowText::with_capacity(cells.len());
    let mut start = 0usize;
    let mut col = 0usize;
    let mut prev_wide = false;

    for cell in cells {
        if cell.spacer {
            // 占位格：没有字形，只占列。
            col += 1;
            continue;
        }
        if prev_wide && !row.text.is_empty() {
            let (text, runs) =
                std::mem::replace(&mut row, RowText::with_capacity(cells.len())).finish();
            segments.push(RowSegment {
                col: start,
                text,
                runs,
            });
            start = col;
        }
        if cell.wide && !row.text.is_empty() {
            let (text, runs) =
                std::mem::replace(&mut row, RowText::with_capacity(cells.len())).finish();
            segments.push(RowSegment {
                col: start,
                text,
                runs,
            });
            start = col;
        }
        row.push(cell, font);
        // 列号按**网格格**走：宽字符本体占一格、它的占位格占下一格，两格加起来才是
        // 视觉上的两列宽。这里按格 +1、不在宽字符上 +2，否则占位格会把列号推过头。
        col += 1;
        prev_wide = cell.wide;
    }
    if !row.text.is_empty() {
        let (text, runs) = row.finish();
        segments.push(RowSegment {
            col: start,
            text,
            runs,
        });
    }
    segments
}

/// 光标块 / 竖线 / 下划线。坐标是**网格坐标**（行 × 列 × 单元格尺寸），
/// 相对最近的可定位祖先（行容器 wrapper）—— 与行的流式布局同原点。
fn cursor_shape(cursor: CursorPaint, cell: CellSize, color: Hsla) -> AnyElement {
    let left = px(cursor.column as f32 * cell.width);
    let top = px(cursor.row as f32 * cell.height);
    let block = || {
        div()
            .absolute()
            .left(left)
            .top(top)
            .w(px(cell.width))
            .h(px(cell.height))
    };
    match cursor.shape {
        CursorShape::Hidden => div().into_any_element(),
        CursorShape::Beam => block()
            .w(px(cell.width * CURSOR_THICKNESS_RATIO))
            .bg(color)
            .into_any_element(),
        CursorShape::Underline => block()
            // 贴在单元格底边：上移一个厚度，正好压在下沿。
            .top(top + px(cell.height * (1. - CURSOR_THICKNESS_RATIO)))
            .h(px(cell.height * CURSOR_THICKNESS_RATIO))
            .bg(color)
            .into_any_element(),
        CursorShape::HollowBlock => block().border_1().border_color(color).into_any_element(),
        // `Block`（默认）与将来新增的形状都按实心块画。
        CursorShape::Block => block().bg(color).into_any_element(),
    }
}

/// 选区（网格坐标）→ 可见行上的高亮条带。
///
/// 返回值是 `(视口行, 起始列, 结束列)`（**闭区间**，都是相对文字区左上角的 0 基下标）。
///
/// # 为什么要在宿主侧做这一层
///
/// 内核给的是**网格坐标**（`SelectionRange` 的行号从回滚缓冲最顶开始，可能为负），
/// 而画布只画视口那几十行。这里做三件事：
///
/// 1. 网格行 → 视口行（加 `display_offset`），把视口外的行裁掉；
/// 2. 按「这一行是首行 / 末行 / 中间行」算出列跨度 —— 中间行整行铺满，
///    首行从选区起点铺到行尾、末行从行首铺到选区终点；
/// 3. 列号钳到网格宽度内（选区可能因为「行尾回绕」越界一格）。
///
/// 块选（`is_block`）是唯一例外：每一行都取同一对列号，不随首末行变化。
fn selection_bands(
    range: SelectionRange,
    offset: i32,
    screen_lines: usize,
    columns: usize,
) -> Vec<(usize, usize, usize)> {
    if columns == 0 || screen_lines == 0 {
        return Vec::new();
    }
    let last = columns - 1;
    let start_row = range.start.line.0 + offset;
    let end_row = range.end.line.0 + offset;
    // 与视口求交：往上翻着历史选、或选区一直拖到画布外时，两端都会越界。
    let top = start_row.max(0);
    let bottom = end_row.min(screen_lines as i32 - 1);
    if bottom < top {
        return Vec::new();
    }

    let mut bands = Vec::with_capacity((bottom - top + 1) as usize);
    for row in top..=bottom {
        let (first, last_col) = if range.is_block {
            (range.start.column.0, range.end.column.0)
        } else if row == start_row && row == end_row {
            // 单行：两端都在这一行上。
            (range.start.column.0, range.end.column.0)
        } else if row <= start_row {
            // 首行（`row <= start_row` 也覆盖「选区起点在视口上方」的情形）。
            (range.start.column.0, last)
        } else if row >= end_row {
            (0, range.end.column.0)
        } else {
            // 中间行：整行。
            (0, last)
        };
        let first = first.min(last);
        let last_col = last_col.min(last);
        if first > last_col {
            continue;
        }
        bands.push((row as usize, first, last_col));
    }
    bands
}

/// `Color` → `Hsla`：真彩色直通，索引 / 具名查覆盖表，再回落宿主配色。
fn resolve(color: Color, colors: &duck_term::Colors, palette: &Palette) -> Hsla {
    let rgb = match color {
        Color::Spec(rgb) => Some(rgb),
        Color::Indexed(index) => {
            colors[usize::from(index)].or_else(|| palette.resolve(usize::from(index)))
        }
        Color::Named(name) => colors[name].or_else(|| palette.resolve(name as usize)),
    };
    // 覆盖表里没有、宿主也解析不出来（越界索引）：退成前景色。越界是 bug，
    // 但把它渲染成一个黑洞只会让 bug 更难找 —— 至少字要看得见。
    rgb.map_or_else(|| foreground(palette), |rgb| rgb_hsla(rgb.r, rgb.g, rgb.b))
}

fn foreground(palette: &Palette) -> Hsla {
    rgb_of(&palette.foreground)
}

fn cursor_color(palette: &Palette) -> Hsla {
    rgb_of(&palette.cursor)
}

fn rgb_of(value: &[u8; 3]) -> Hsla {
    rgb_hsla(value[0], value[1], value[2])
}

fn rgb_hsla(r: u8, g: u8, b: u8) -> Hsla {
    Hsla::from(Rgba {
        r: f32::from(r) / 255.,
        g: f32::from(g) / 255.,
        b: f32::from(b) / 255.,
        a: 1.,
    })
}

/// 主题配色 → 终端调色板。
///
/// 16 个 ANSI 色用内核自带的深 / 浅两组（主题并没有这 16 色），但前景 / 背景 / 光标
/// 取主题色 —— 这样终端底色和整体界面是同一块底，不会出现「一块更黑的洞」。
pub(crate) fn palette_for(cx: &App) -> Palette {
    let theme = active_theme(cx);
    let dark = theme.mode() == ThemeMode::Dark;
    let colors = theme.tokens().colors;
    let mut palette = if dark {
        Palette::dark()
    } else {
        Palette::light()
    };
    palette.foreground = to_bytes(colors.foreground);
    palette.background = to_bytes(colors.background);
    palette.cursor = to_bytes(colors.primary);
    palette
}

fn to_bytes(color: Hsla) -> [u8; 3] {
    let rgba = Rgba::from(color);
    [
        (rgba.r.clamp(0., 1.) * 255.).round() as u8,
        (rgba.g.clamp(0., 1.) * 255.).round() as u8,
        (rgba.b.clamp(0., 1.) * 255.).round() as u8,
    ]
}

// —— 主机密钥告警（方案 B：只加告警）——
//
// 本仓走「驱动系统 ssh」，主机密钥的**校验**与**记录**仍旧完全由 OpenSSH 按用户自己的
// `~/.ssh/known_hosts` + `StrictHostKeyChecking` 执行：应用不写库、不改用户的策略、也不
// 在连接前另起一条探测连接。这里做的是**事后识别**：ssh 因主机密钥问题退出时，把它打在
// 屏幕上的原文转成一条中文化、可操作的提示。首次连接（库里没有记录）一律不出声 ——
// 那正是 OpenSSH 自己在 PTY 里问过用户的场景，我们不再补一句。

/// OpenSSH 在「主机密钥与记录不符」时的经典横幅（`sshconnect.c`）。
const HOST_KEY_CHANGED_MARKER: &str = "REMOTE HOST IDENTIFICATION HAS CHANGED";
/// 主机密钥校验失败的收尾句（变更 / 首次 + `StrictHostKeyChecking=yes` / 吊销都会出现）。
const HOST_KEY_VERIFY_FAILED_MARKER: &str = "Host key verification failed";
/// 命中 `@revoked` 记录时 OpenSSH 的说法。
const HOST_KEY_REVOKED_MARKER: &str = "has been revoked";

/// 一次 ssh 退出时，屏幕上命中了哪些主机密钥告警。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HostKeySignals {
    /// 出现「REMOTE HOST IDENTIFICATION HAS CHANGED」横幅。
    changed: bool,
    /// 出现「Host key verification failed」。
    verify_failed: bool,
    /// 出现「has been revoked」。
    revoked: bool,
}

impl HostKeySignals {
    /// 至少有一样命中。
    fn any(self) -> bool {
        self.changed || self.verify_failed || self.revoked
    }
}

/// 该给用户哪一类主机密钥提示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostKeyNotice {
    /// 密钥与记录不符（可能对方重装，也可能中间人）。
    Changed,
    /// 记录被显式吊销（`@revoked`）。
    Revoked,
    /// 库里已有记录，但本次校验失败（密钥类型 / 指纹对不上）。
    Mismatch,
}

/// 在一条**刚退出**的 ssh 会话屏幕上，采集主机密钥告警信号。
///
/// 用 [`Session::buffer_contains`]（阻塞加锁）而不是搜索框那条 `try_*`：
/// 退出判定是一次性的，一次拿不到锁就把告警丢掉是不可接受的。
fn scan_host_key_signals(session: &Session) -> HostKeySignals {
    HostKeySignals {
        changed: session.buffer_contains(HOST_KEY_CHANGED_MARKER),
        verify_failed: session.buffer_contains(HOST_KEY_VERIFY_FAILED_MARKER),
        revoked: session.buffer_contains(HOST_KEY_REVOKED_MARKER),
    }
}

/// 认证类失败的标记（OpenSSH 原文）。命中任一条即判为**不可自动重试**。
///
/// 依据：`Permission denied`（公钥 / 密码被拒）、`Authentication failed`（各平台说法）、
/// `Too many authentication failures`（`MaxAuthTries` 触发）都意味着「重试也是同样结果」，
/// 且对同一账号连发认证可能被服务端限流 / 锁定 —— 必须把决定权交回用户。
/// 与主机密钥问题（[`scan_host_key_signals`]）合成自动重连的准入判据。
const AUTH_FAILURE_MARKERS: [&str; 3] = [
    "Permission denied",
    "Authentication failed",
    "Too many authentication failures",
];

/// 一条**刚退出**的 ssh 会话屏幕上是否出现认证类失败。
///
/// 与主机密钥扫描同一套策略：用 [`Session::buffer_contains`] 一次性阻塞读，
/// 退出判定只发生一次，拿不到锁就丢结论不可接受。
fn auth_failure_on_screen(session: &Session) -> bool {
    AUTH_FAILURE_MARKERS
        .iter()
        .any(|marker| session.buffer_contains(marker))
}

/// 由「屏幕上命中了哪些告警」+「`known_hosts` 里的记录状态」判定该给哪一类提示。
///
/// 返回 `None` = **不出声**，有两种刻意的沉默：
/// - 屏幕上没有任何主机密钥告警（连接因别的原因失败，或正常退出）；
/// - 只有「校验失败」而库里**查不到**这台主机 —— 那是首次连接（TOFU）。
///   哈希化记录也会落进 `Unknown`（见 `duck_ssh::known_hosts` 模块头），
///   同样沉默：宁可偶尔少说一句，也不要对着一次正常的新主机连接误报。
///
/// 注意「密钥已变」的横幅**本身就是记录存在的证据**（未知主机不会报 `changed`），
/// 所以它不依赖 `<status>`，哈希化记录那一类也能覆盖到。
fn host_key_notice(
    signals: HostKeySignals,
    status: &duck_ssh::HostKeyStatus,
) -> Option<HostKeyNotice> {
    if !signals.any() {
        return None;
    }
    if signals.changed {
        return Some(HostKeyNotice::Changed);
    }
    if signals.revoked || matches!(status, duck_ssh::HostKeyStatus::Revoked) {
        return Some(HostKeyNotice::Revoked);
    }
    if signals.verify_failed && matches!(status, duck_ssh::HostKeyStatus::Known { .. }) {
        return Some(HostKeyNotice::Mismatch);
    }
    None
}

/// `ssh-keygen -R` / `known_hosts` 里这台主机的写法：默认端口只写主机名，
/// 非默认端口是 `[host]:port`（与 `duck_ssh::known_hosts` 的匹配语法同源）。
fn ssh_keygen_target(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    }
}

/// 把一类提示渲染成给用户看的文案（i18n 源串，`{}` 占位）。
///
/// 文案刻意**说清是什么问题 + 下一步怎么做**：
/// - 变更 / 不符 → 先核对，`ssh-keygen -R <host>` 删旧记录再重连；
/// - 吊销 → **不要**直接删记录（那会把吊销标记一起抹掉），先与服务方确认新密钥。
fn host_key_notice_text(notice: HostKeyNotice, host: &str, port: u16, cx: &App) -> String {
    let target = ssh_keygen_target(host, port);
    match notice {
        HostKeyNotice::Changed => trf(
            cx,
            "主机密钥与 ~/.ssh/known_hosts 中记录的不一致，OpenSSH 已拒绝连接。这可能是对方重装或更换了密钥，也可能是中间人攻击——请先核对，确认无误后执行 ssh-keygen -R {} 删除旧记录再重连。",
            &[target.as_str()],
        ),
        HostKeyNotice::Revoked => tr(
            cx,
            "该主机的密钥已被标记为吊销（~/.ssh/known_hosts 中的 @revoked），OpenSSH 已拒绝连接。请勿直接删除记录，先与服务提供方确认新密钥后再更新 ~/.ssh/known_hosts。",
        )
        .to_owned(),
        HostKeyNotice::Mismatch => trf(
            cx,
            "~/.ssh/known_hosts 中已有该主机记录，但本次主机密钥校验失败，OpenSSH 已拒绝连接。请核对后执行 ssh-keygen -R {} 删除旧记录再重连。",
            &[target.as_str()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono_font() -> Font {
        Font {
            family: "Menlo".into(),
            features: tabular_numbers(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    fn plain(ch: char) -> CellPaint {
        CellPaint {
            ch: ch.to_string(),
            fg: rgb_hsla(0xff, 0xff, 0xff),
            bg: None,
            bold: false,
            italic: false,
            underline: UnderlineKind::None,
            underline_color: None,
            strike: false,
            spacer: false,
            wide: false,
        }
    }

    /// 一个宽字符本体 + 它占位格的组合（内核就是这么发出来的）。
    fn wide(ch: char) -> [CellPaint; 2] {
        let mut body = plain(ch);
        body.wide = true;
        let mut spacer = plain(' ');
        spacer.spacer = true;
        [body, spacer]
    }

    /// 把一行切出的各段拼回「整行文本 + runs」。
    ///
    /// 画布早就改成逐段绝对定位了（[`build_segments`]）；这些断言按「整行」写更直观，
    /// 拼回去再断言。拼法就是按段顺序连起来 —— 段是按列单调排列的。
    fn row_text(cells: &[CellPaint], font: &Font) -> (String, Vec<TextRun>) {
        let mut text = String::new();
        let mut runs = Vec::new();
        for segment in build_segments(cells, font) {
            text.push_str(&segment.text);
            runs.extend(segment.runs);
        }
        (text, runs)
    }

    /// run 的字节长之和**必须**等于文本字节长：`StyledText::with_runs` 在不等时
    /// 直接 panic（`gpui/elements/text.rs:526` "invalid text run"）。这条断言就是那个 panic 的守卫。
    fn assert_runs_cover(text: &str, runs: &[TextRun]) {
        assert_eq!(
            runs.iter().map(|run| run.len).sum::<usize>(),
            text.len(),
            "run 覆盖长度对不上文本：{text:?} / {runs:?}"
        );
    }

    #[test]
    fn row_runs_cover_the_text_exactly() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "echo hi".chars().map(plain).collect();
        let (text, runs) = row_text(&cells, &font);
        assert_eq!(text, "echo hi");
        assert_runs_cover(&text, &runs);
    }

    /// 多字节字符（CJK / 宽字符）每格 3 字节：run 长度按**字节**算才不会被截断。
    #[test]
    fn multibyte_cells_count_bytes_not_chars() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "中文café".chars().map(plain).collect();
        let (text, runs) = row_text(&cells, &font);
        assert_eq!(text.chars().count(), 6);
        assert!(text.len() > 6, "应当按字节计，实际 {} 字节", text.len());
        assert_runs_cover(&text, &runs);
    }

    /// 整行同一样式只该产出一个 run —— 一屏 200 列 × 50 行若逐格产 run 就是一万个。
    #[test]
    fn identical_styles_merge_into_one_run() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "-".repeat(80).chars().map(plain).collect();
        let (text, runs) = row_text(&cells, &font);
        assert_eq!(runs.len(), 1);
        assert_runs_cover(&text, &runs);
    }

    /// 样式一变就断 run，且断点处长度正好切在边界上。
    #[test]
    fn style_change_splits_runs_at_the_boundary() {
        let font = mono_font();
        let mut cells = vec![plain('a'), plain('b')];
        let mut bold = plain('C');
        bold.bold = true;
        cells.push(bold);
        let (text, runs) = row_text(&cells, &font);
        assert_eq!(text, "abC");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len, 2);
        assert_eq!(runs[1].len, 1);
        assert_eq!(runs[1].font.weight, FontWeight::BOLD);
        assert_runs_cover(&text, &runs);
    }

    /// 纯 ASCII 的一行只切出一段，且起点是第 0 列 —— 元素数量与「整行一段」时代一致，
    /// 绝对定位不该让普通行变慢。
    #[test]
    fn ascii_rows_stay_a_single_segment() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "ls -la".chars().map(plain).collect();
        let segments = build_segments(&cells, &font);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].col, 0);
        assert_eq!(segments[0].text, "ls -la");
        assert_runs_cover(&segments[0].text, &segments[0].runs);
    }

    /// 宽字符**独占一段**，且段起点落在那两列的第一列上。
    ///
    /// 这是「`print('中文'*40)` 末字与第 80 列格线对齐」那条验收的守卫：
    /// 流式排布时占位格那个空格会再吃掉一格，几十个汉字就偏出半屏。
    #[test]
    fn wide_chars_get_their_own_column_anchored_segment() {
        let font = mono_font();
        let mut cells = Vec::new();
        for ch in "中文".chars() {
            cells.extend(wide(ch));
        }
        let segments = build_segments(&cells, &font);
        assert_eq!(segments.len(), 2, "两个汉字 = 两段");
        assert_eq!(segments[0].col, 0);
        assert_eq!(segments[0].text, "中");
        assert_eq!(segments[1].col, 2, "第二个汉字从第 2 列起（宽字符占两列）");
        assert_eq!(segments[1].text, "文");
        for segment in &segments {
            assert_runs_cover(&segment.text, &segment.runs);
            assert!(
                !segment.text.contains(' '),
                "占位格不该产出字形：{segment:?}"
            );
        }
    }

    /// 汉字夹在 ASCII 里：三段，起点分别是 0 / 1 / 3。
    #[test]
    fn mixed_rows_anchor_each_part_at_its_column() {
        let font = mono_font();
        let mut cells = vec![plain('a')];
        cells.extend(wide('中'));
        cells.push(plain('b'));
        let segments = build_segments(&cells, &font);
        assert_eq!(
            segments
                .iter()
                .map(|s| (s.col, s.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "a"), (1, "中"), (3, "b")]
        );
    }

    /// 六档下划线各有各的画法：单线 1px、双线 2px、波浪线 wavy、点线/虚线退成细线。
    /// 「都画成一条 1px 直线」等于把 `git diff` 的双下划线与拼写检查的波浪线
    /// 压缩成同一个视觉信号 —— 那不如不画。
    #[test]
    fn underline_kinds_are_visually_distinct() {
        let of = |kind: UnderlineKind| kind.style(None).map(|s| (f32::from(s.thickness), s.wavy));
        assert_eq!(of(UnderlineKind::None), None);
        assert_eq!(of(UnderlineKind::Single), Some((1.0, false)));
        assert_eq!(of(UnderlineKind::Double), Some((2.0, false)));
        assert_eq!(of(UnderlineKind::Curly), Some((1.0, true)));
        // 点线 / 虚线没有对应形状：退成比单线更细的直线，至少与单线可区分。
        assert_eq!(of(UnderlineKind::Dotted), Some((0.5, false)));
        assert_eq!(of(UnderlineKind::Dashed), Some((0.5, false)));
    }

    /// `SGR 58` 给的下划线色要落到 `UnderlineStyle.color` 上，而不是被丢掉。
    #[test]
    fn underline_color_reaches_the_style() {
        let red = rgb_hsla(0xff, 0x00, 0x00);
        let style = UnderlineKind::Single.style(Some(red)).expect("有下划线");
        assert_eq!(style.color, Some(red));
    }

    /// 单下划线与波浪线相邻时**不能合并**成一个 run：合并了就只能二选一，
    /// 拼写检查的波浪线会被邻居的单线吞掉。
    #[test]
    fn different_underlines_do_not_merge() {
        let font = mono_font();
        let plain_cell = plain('a');
        let mut curly = plain('b');
        curly.underline = UnderlineKind::Curly;
        let (_, runs) = row_text(&[plain_cell, curly], &font);
        assert_eq!(runs.len(), 2, "画法不同就该是两个 run");
        assert!(runs[1].underline.is_some_and(|style| style.wavy));
    }

    /// 组合记号留在**同一格**里：`e` + U+0301 是一个簇，不能拆成两段，
    /// 也不能被当成「一格两列」——列号只按网格格走。
    #[test]
    fn combining_marks_stay_inside_their_cell() {
        let font = mono_font();
        let mut accented = plain('e');
        accented.ch.push('\u{301}');
        let cells = vec![accented, plain('x')];
        let segments = build_segments(&cells, &font);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].col, 0);
        assert_eq!(segments[0].text, "e\u{301}x");
        assert_runs_cover(&segments[0].text, &segments[0].runs);
    }

    /// 全是占位格 / 全空的行走不出段：不能产出空 `StyledText`（`with_runs` 收空切片虽然合法，
    /// 但空段定位在 0 列纯属浪费，行高由行容器自己撑着）。
    #[test]
    fn rows_without_glyphs_produce_no_segments() {
        let font = mono_font();
        assert!(build_segments(&[], &font).is_empty());
        let mut spacer = plain(' ');
        spacer.spacer = true;
        assert!(build_segments(&[spacer], &font).is_empty());
    }

    /// 空行（一帧都没拿到锁时的兜底行）：文本与 runs 都为空，`with_runs` 收空切片是合法的。
    #[test]
    fn empty_row_stays_empty() {
        let font = mono_font();
        let (text, runs) = row_text(&[], &font);
        assert!(text.is_empty());
        assert!(runs.is_empty());
        assert_runs_cover(&text, &runs);
    }

    /// 主题色 → 终端调色板的字节转换必须是无损往返：`palette_for` 拿它把主题色
    /// 灌给内核，掉一位就等于终端底色和界面底色不是同一块。
    #[test]
    fn theme_colors_round_trip_through_bytes() {
        let raw = [0x12u8, 0x34, 0x56];
        assert_eq!(to_bytes(rgb_of(&raw)), raw);
        assert_eq!(to_bytes(rgb_hsla(0, 0, 0)), [0, 0, 0]);
        assert_eq!(to_bytes(rgb_hsla(255, 255, 255)), [255, 255, 255]);
    }

    /// 默认端口只写主机名，非默认端口是 `[host]:port` —— `ssh-keygen -R` 的写法。
    #[test]
    fn keygen_target_uses_bracketed_host_for_non_default_ports() {
        assert_eq!(ssh_keygen_target("example.com", 22), "example.com");
        assert_eq!(ssh_keygen_target("example.com", 2222), "[example.com]:2222");
    }

    /// 屏幕上没有任何主机密钥告警 → 绝不出声（连接因别的原因失败 / 正常退出）。
    #[test]
    fn no_screen_signal_stays_silent() {
        assert_eq!(
            host_key_notice(HostKeySignals::default(), &duck_ssh::HostKeyStatus::Unknown),
            None
        );
        // 哪怕库里查得到记录，只要屏幕没吭声就不提示 —— 记录存在本身不是告警理由。
        assert_eq!(
            host_key_notice(
                HostKeySignals::default(),
                &duck_ssh::HostKeyStatus::Known {
                    key_types: vec!["ssh-ed25519".to_owned()]
                }
            ),
            None
        );
    }

    /// 「密钥已变」的横幅本身就是记录存在的证据：即使哈希化记录让只读探测报
    /// `Unknown`，也要提示（这正是方案 B 覆盖哈希化记录的关键）。
    #[test]
    fn changed_banner_warns_even_when_the_record_is_hashed() {
        let signals = HostKeySignals {
            changed: true,
            verify_failed: true,
            revoked: false,
        };
        assert_eq!(
            host_key_notice(signals, &duck_ssh::HostKeyStatus::Unknown),
            Some(HostKeyNotice::Changed)
        );
    }

    /// 吊销优先于普通校验失败；`@revoked` 记录与屏幕文案任一条成立即可判定。
    #[test]
    fn revoked_wins_over_plain_failure() {
        let verified = HostKeySignals {
            changed: false,
            verify_failed: true,
            revoked: false,
        };
        assert_eq!(
            host_key_notice(verified, &duck_ssh::HostKeyStatus::Revoked),
            Some(HostKeyNotice::Revoked)
        );
        let on_screen = HostKeySignals {
            revoked: true,
            ..verified
        };
        assert_eq!(
            host_key_notice(on_screen, &duck_ssh::HostKeyStatus::Unknown),
            Some(HostKeyNotice::Revoked)
        );
    }

    /// 库里**已有**记录 + 校验失败 → 提示；库里**没有**记录（首次连接 TOFU）→ 沉默。
    /// 这条边界是本次需求的核心：首次未记录的主机交给 OpenSSH 自己处理。
    #[test]
    fn verify_failed_only_warns_for_a_known_host() {
        let signals = HostKeySignals {
            changed: false,
            verify_failed: true,
            revoked: false,
        };
        assert_eq!(
            host_key_notice(
                signals,
                &duck_ssh::HostKeyStatus::Known {
                    key_types: vec!["ssh-rsa".to_owned()]
                }
            ),
            Some(HostKeyNotice::Mismatch)
        );
        assert_eq!(
            host_key_notice(signals, &duck_ssh::HostKeyStatus::Unknown),
            None
        );
    }

    /// 造一个带若干 `-o` 选项的目标（不含口令，避免落 askpass 脚本）。
    fn option_target() -> duck_ssh::Target {
        duck_ssh::Target {
            host: "example.test".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            identity: None,
            password: None,
            auth: duck_ssh::AuthMethod::Auto,
            options: vec![
                "StrictHostKeyChecking=yes".to_owned(),
                "ServerAliveInterval=30".to_owned(),
            ],
            forwards: Vec::new(),
            agent_forwarding: false,
        }
    }

    /// 初起与重连**共用同一份 argv 拼装**：尺寸不同不改变命令行本身。
    #[test]
    fn ssh_options_share_the_same_argv_for_initial_and_reconnect() {
        let target = option_target();
        let dir = std::env::temp_dir();
        // 回滚行数与本测试无关：两处都取出厂默认，保证除了网格尺寸外没有任何差异。
        let scrollback = SpawnOptions::default().scrollback;
        let initial = TerminalSession::ssh_options(&target, &dir, None, None, scrollback);
        let reconnect =
            TerminalSession::ssh_options(&target, &dir, None, Some((120, 40)), scrollback);

        // 唯一的差别只该是网格尺寸 —— argv 一致，重连就是「重放同一条命令行」。
        assert_eq!(initial.shell, reconnect.shell, "初起与重连的 argv 必须一致");
        assert_eq!(reconnect.cols, 120);
        assert_eq!(reconnect.rows, 40);

        let (program, argv) = initial.shell.expect("SSH 会话必须有 argv");
        assert_eq!(program, "ssh");
        assert_eq!(argv, target.terminal_argv(Some(&dir)));
    }

    /// `-o` 选项的**顺序稳定**：骨架（复用三件套 + 保活）前置、档案 `options`
    /// 按原顺序收尾，且 argv 以 `-t <destination>` 结束。
    #[test]
    fn ssh_options_keep_option_order_and_end_with_dash_t_target() {
        let target = option_target();
        let dir = std::env::temp_dir();
        let options = TerminalSession::ssh_options(
            &target,
            &dir,
            None,
            None,
            SpawnOptions::default().scrollback,
        );
        let (_, argv) = options.shell.expect("argv");

        // 每个 `-o` 与其值成对。
        let option_pairs: Vec<&str> = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == "-o")
            .map(|(index, _)| argv[index + 1].as_str())
            .collect();
        // 骨架里的复用三件套前置（`socket_dir` 给了才有 ControlMaster/ControlPath/ControlPersist）。
        assert_eq!(option_pairs[0], "ControlMaster=auto");
        // ControlPath 含随机 socket 路径，值本身不钉死，只校验它确实排在第二位。
        assert!(option_pairs[1].starts_with("ControlPath="));
        assert_eq!(option_pairs[2], "ControlPersist=300");
        // 档案自己的选项按原顺序**收尾**（不被骨架插队、前后顺序不变）。
        let tail = &option_pairs[option_pairs.len() - target.options.len()..];
        assert_eq!(
            tail,
            &["StrictHostKeyChecking=yes", "ServerAliveInterval=30"]
        );
        // 结尾固定是 `-t <destination> <locale 引导>`：目标串是 `user@host`，
        // 端口另走 `-p`；v4 起目标串后再追加一段远端 locale 引导命令
        //（见 `duck-ssh` 的 `locale_bootstrap`），所以目标串在**倒数第二**。
        assert_eq!(argv[argv.len() - 3], "-t");
        assert_eq!(argv[argv.len() - 2], target.destination());
        assert_eq!(argv[argv.len() - 2], "deploy@example.test");
    }

    /// 自动重连退避：2s 起、每次翻倍封顶 60s、最多 5 次后停。
    #[test]
    fn auto_reconnect_backoff_is_bounded_and_exponential() {
        let start = std::time::Instant::now();
        let mut driver = AutoReconnect::new(ReconnectPolicy::default());
        let mut delays = Vec::new();
        for _ in 0..RECONNECT_MAX_ATTEMPTS {
            let due = driver.schedule(start).expect("前 5 次应排定");
            delays.push(due - start);
        }
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(32),
            ]
        );
        // 第 6 次：次数用尽，停下（返回 None，界面只留手动入口）。
        assert_eq!(driver.schedule(start), None);
        assert_eq!(driver.attempts, RECONNECT_MAX_ATTEMPTS);
    }

    /// 自定义调控参数（设置页「重连上限 / 起始退避」）真的生效。
    #[test]
    fn auto_reconnect_honours_a_custom_policy() {
        let start = std::time::Instant::now();
        let policy = ReconnectPolicy {
            max_attempts: 2,
            backoff_start: Duration::from_secs(5),
        };
        let mut driver = AutoReconnect::new(policy);
        assert_eq!(driver.schedule(start), Some(start + Duration::from_secs(5)));
        assert_eq!(
            driver.schedule(start),
            Some(start + Duration::from_secs(10))
        );
        // 只给 2 次，第 3 次就停。
        assert_eq!(driver.schedule(start), None);
    }

    /// 退避封顶：即使把调控器手动推到很大的退避，也不会超过 60s。
    #[test]
    fn auto_reconnect_backoff_caps_at_the_maximum() {
        let start = std::time::Instant::now();
        let mut driver = AutoReconnect {
            attempts: 0,
            backoff: RECONNECT_BACKOFF_MAX,
            max_attempts: RECONNECT_MAX_ATTEMPTS,
        };
        assert_eq!(driver.schedule(start), Some(start + RECONNECT_BACKOFF_MAX));
        // 翻倍后立刻被上限夹住。
        assert_eq!(driver.backoff, RECONNECT_BACKOFF_MAX);
    }

    /// 断开准入：认证 / 主机密钥类失败落 `Stopped`，其余才排定下一次尝试。
    #[test]
    fn reconnect_decision_only_schedules_retryable_disconnects() {
        let now = std::time::Instant::now();
        let idle = ReconnectState::Idle;
        let policy = ReconnectPolicy::default();

        // 不可重试：认证 / 主机密钥失败直接停。
        assert_eq!(
            reconnect_after_disconnect(true, true, idle, now, policy),
            ReconnectState::Stopped
        );

        // 可重试：Idle → Pending（第一次尝试 2s 后）。
        match reconnect_after_disconnect(true, false, idle, now, policy) {
            ReconnectState::Pending { driver, due } => {
                assert_eq!(driver.attempts, 1);
                assert_eq!(due, now + RECONNECT_BACKOFF_START);
            }
            other => panic!("应排定第一次尝试，实际 {other:?}"),
        }

        // 已停不再启：`Stopped` 不会被再次排定。
        assert_eq!(
            reconnect_after_disconnect(true, false, ReconnectState::Stopped, now, policy),
            ReconnectState::Stopped
        );

        // 上一轮是 `Armed`：计数与退避被带过来，第二次排定用 4s。
        let armed = ReconnectState::Armed {
            driver: AutoReconnect {
                attempts: 1,
                backoff: RECONNECT_BACKOFF_START * 2,
                max_attempts: RECONNECT_MAX_ATTEMPTS,
            },
        };
        match reconnect_after_disconnect(true, false, armed, now, policy) {
            ReconnectState::Pending { driver, due } => {
                assert_eq!(driver.attempts, 2);
                assert_eq!(due, now + RECONNECT_BACKOFF_START * 2);
            }
            other => panic!("应排定第二次尝试，实际 {other:?}"),
        }

        // 本地会话（无资格）永不进入自动重连。
        assert_eq!(
            reconnect_after_disconnect(false, false, idle, now, policy),
            ReconnectState::Idle
        );
    }
}
