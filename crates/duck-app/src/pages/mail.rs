//! 邮箱页：账户与文件夹（左栏）+ 邮件列表 + 阅读器。
//!
//! 布局与交互范式沿用本仓 `RedisPage` / `DatabasePage` / `TerminalPage`：左栏走页面内
//! [`SidebarPanel`]；**没有右栏**（见下「关于右栏」一节）。纵向尺寸一律取
//! `duck-theme` 的 `DensityTokens`（行高 / 条高），只有横向分配比例与弹窗宽度留在
//! 本文件的常量里——与 `docs/mail-ui-mockup.html` 第 ①/② 屏对应。
//!
//! # 数据流
//!
//! 页面持有 [`MailCore`]（`duck-mail` 的门面）。它是**全同步**的——SQLite 读写、
//! IMAP 往返都在调用线程上完成——所以每一个调用都经 [`Self::spawn`] 丢进
//! `background_executor`，回主线程后再 `cx.notify()`。渲染函数里**不碰内核**：
//! 它只读页面自己的字段（拉到的东西已经躺在里面了）。
//!
//! 打开库这一步也是后台：`MailCore::open_default()` 会建目录、跑迁移，属于 I/O，
//! 不该压在窗口构造上。
//!
//! ## 变化从哪来：事件通道（不是每个动作都手写 reload）
//!
//! 内核在同步 / 拉正文时会把 [`MailEvent`] 推进一个 `std::sync::mpsc`（
//! `MailCore::set_event_sink`，在 [`MailPage::open_core`] 里注册）。GPUI 的事件
//! 循环看不见标准库通道，所以页面起一个**常驻的前台轮询任务**（`cx.spawn` +
//! `background_executor().timer(40ms)`）把事件取走、按变体刷新对应区域。
//!
//! 40ms 是硬要求：逐事件 `notify()` 会重演 `dialog.rs` 的 prepaint 自激
//! （`docs/email-design.md` §4.1）。范式与 `assistant.rs` 的流式轮询一致，
//! 区别只在于它一轮流结束就返回，邮箱的事件是**长期**的（多账号、后台同步），
//! 所以这个任务挂到页面销毁为止。
//!
//! 于是「同步 → 文件夹/列表/账户状态自动更新」不必在 `sync_now` 里手写一串
//! `reload_*`：内核报什么就刷什么。
//!
//! ## 正文：HTML 渲染 + 纯文本兜底
//!
//! 正文形态在 [获取正文时] 就算好（[`RenderedBody`]），render 里只做分支——
//! `html_to_text` 与远程图剥离都是全量字符串处理，放 render 就是每帧一次。
//!
//! - 只有纯文本 → 纯文本；
//! - 只有 HTML → `TextView::html`（`gpui-base` 的文档模型渲染，无 JS / 无 CSS 布局）；
//! - 两者都有（`multipart/alternative`）→ **默认给 HTML**（§4.8，纯文本版通常是
//!   机器压出来的兜底，丢链接丢表格），工具条上可一键切回纯文本。这条开关是后端
//!   已有的两份正文，不是演示控件。
//!
//! 远程图片**默认显示**（用户拍板），提示条上可一键收回并说明代价——追踪像素
//! 会让发件人知道你读过信，取舍交还用户。CID 内联图不受这些影响——内核在拉取
//! 正文时已经把它们换成 `data:`（`duck_mail::mime::inline_cid_images`）。
//!
//! 链接点击**只放行 `http` / `https` / `mailto`**（[`openable_link`]）：`TextView`
//! 默认把链接交给系统打开器，而正文里的链接是别人写的——`file:` 或自定义 scheme
//! 落到打开器上就是「点一下就动本地」。`<script>` / `<style>` 与 CSS 引擎由
//! `gpui-base` 的解析器天然丢弃（渲染目标是文档模型，不是浏览器），不必再引净化库。
//!
//! # 关于「右栏 AI 面板」——按 P3-6 删除
//!
//! 前一版这里有一个 shell 级右栏「AI 面板」（摘要 / 回复建议 / 问答三页签）与
//! 阅读器工具条上的 8 个动作按钮，全部**禁用 + 文案写「M2 接入」**。
//! `docs/architecture-gap-analysis.md` 的 **P3-6** 把这种「自我标注为待接入」
//! 明确判为不可保留：「界面不应再有演示数据 / 演示文字，每个图标的功能都需要
//! 实现完整」，处置判据是**后端存在 → 接线；后端不存在 → 删除**。
//!
//! 于是本次按判据逐条处置：
//!
//! | 位置 | 后端 | 处置 |
//! | --- | --- | --- |
//! | 左栏「添加账户」 | `MailCore::add_account` | **接线**（账户向导弹窗） |
//! | 左栏「移除账户」 | `MailCore::remove_account` | **接线**（走系统确认框） |
//! | 左栏「标签」分组 | 没有 `labels` 仓储 | 删除 |
//! | 列表条「筛选」 | 没有筛选后端 | 删除 |
//! | 列表条「刷新」 | `MailCore::sync_account` | **接线** |
//! | 阅读器「标为已读/未读」 | `MailCore::set_message_read` | **接线** |
//! | 阅读器「下载正文」 | `MailCore::fetch_message_body` | **接线** |
//! | 阅读器「回复 / 转发 / 归档 / 稍后 / 删除 / 打标签 / 旗标 / AI 分析 / 更多」 | 都没有（M2 的收发闭环） | 删除 |
//! | 右栏 AI 面板 | 没有邮件 AI 后端（M2） | 删除 |
//!
//! 这一版**不留任何禁用控件**。M2 把发信、动作队列、AI 接通后，这些入口再按同一份
//! 判据接回来——那时它们才有真功能可挂。
//!
//! # 发信：撰写 / 回信（M2 收口）
//!
//! 上表里被删掉的「回复」回来了——因为后端补齐了：`duck-mail` 现在有
//! `compose`（RFC 5322 头部装配 + RFC 2047 编码）+ `proto::smtp`（EHLO / STARTTLS /
//! AUTH / MAIL / RCPT / DATA / QUIT）与门面 [`MailCore::send`]。判据没变：
//! **后端存在 → 接线**。
//!
//! 于是本文件多了一处撰写弹窗（[`ComposeForm`]），入口两个：
//!
//! | 入口 | 预填 |
//! | --- | --- |
//! | 列表条「写邮件」 | 空草稿 |
//! | 阅读器「回信」 | 收件人 = 原发件人、主题 = `回复：…`、正文带归属行与引用块 |
//! | 草稿箱里点一条草稿 | 整条草稿（含附件）回填，改存走同一行 |
//!
//! **仍然不做的**（后端没有，做了就是 P3-6 禁止的空控件）：转发、定时发送、
//! 把草稿推到服务器的 `\Drafts` 文件夹（`drafts_meta.remote_uid` 先留空）、
//! 关弹窗自动存草稿（没有去抖自动保存，只有显式「存草稿」按钮）。
//!
//! ## 为什么发信不走动作队列
//!
//! 动作队列（`duck-mail::queue`）的语义是「**先本地生效，稍后补发远端**」——它成立
//! 的前提是本地有那份状态（`is_read` 改了就改了，远端没跟上也只是没跟上）。发信没有
//! 这一半：本模块没有本地发件箱，信要么发出去了、要么没有。所以 [`MailCore::send`]
//! 是**当场阻塞直发**，失败就把原因贴在弹窗里让用户重试，而不是悄悄排队。

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use duck_components::{
    ButtonVariant, ConfirmHandler, ControlExt as _, DuckIcon, MailLogo, SidebarPanel, SidebarState,
    button, caption_heading, content::accent_tag, content::empty_state, dialog_top_offset,
    field_error, field_hint, field_success, form_field, form_gap, form_row, icon_button,
    icon_text_button, meta_text, modal_drag, modal_drag_area, modal_offset, modal_title_row,
    notify, segmented_tabs, status_button, status_button_content, tabular_numbers,
    toolbar_action_button, toolbar_dropdown_button,
};
use duck_mail::{
    Account, ConnectionSecurity, Draft, DraftAttachmentRef, DraftMode, DraftRecord, DraftSummary,
    Folder, FolderRole, Label, MailCore, MailEvent, Mailbox, MessageDetail, MessageSummary,
    NewAccount, OutgoingAttachment, SyncState, presets,
};
use duck_theme::{Lang, active_theme, tr, trf};
use gpui::{
    Anchor, AnyElement, App, AppContext, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, WindowExt as _,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::PopupMenuItem,
    text::TextView,
    v_flex,
};

use super::mail_data::{
    format_full_time, format_list_time, format_size, join_addresses, sender_display,
    subject_or_placeholder,
};

/// 左栏宽度档位（与 `db.rs` / `redis.rs` / `terminal.rs` 保持一致：
/// 四个页面互相切换时侧栏不跳宽）。
const SIDEBAR_WIDTH_RANGE: RangeInclusive<f32> = 184.0..=340.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 中栏邮件列表宽度（设计稿第 ① 屏 340）。
///
/// 这一条**不进 token**：它是本页三栏的**横向**分配比例，不是密度档位的一环
/// （密度只管纵向节奏与控件尺寸）。同类的还有下面的账户头 / 列表条高度。
const LIST_WIDTH: f32 = 340.;
/// 左栏账户头高度（设计稿 44）。
const ACCOUNT_HEADER_HEIGHT: f32 = 44.;
/// 列表条 / 阅读器工具条的条高（设计稿 44）。
const BAR_HEIGHT: f32 = 44.;
/// 一次取多少封邮件。
///
/// 不叫 `PAGE`：这里没有分页控件，只是**首屏上限**——一次同步可能落进几万封，
/// 而列表一屏能看的就几十行。滚动加载在 M1.5。
pub(super) const PREVIEW_LIMIT: i64 = 300;
/// 账户弹窗宽度。
const ACCOUNT_DIALOG_WIDTH: f32 = 620.;
/// 对话框正文里 IMAP / SMTP 两列主机端口的比例（端口窄、主机宽）。
const PORT_FIELD_WIDTH: f32 = 88.;
/// 「新建标签」弹窗宽度。
const LABEL_DIALOG_WIDTH: f32 = 420.;
/// 撰写（写邮件 / 回信）弹窗宽度。
///
/// 比账户弹窗（620）宽 60：这里面有一个正文框，而正文是**唯一**会写长内容的地方——
/// 收件人 / 主题都是一行。620 下正文每行只剩五十来个字，写三句话就满屏。
const COMPOSE_DIALOG_WIDTH: f32 = 680.;
/// 撰写弹窗正文框的行数档（空闲高度即 `min_rows`，口径同 `assistant.rs` 的
/// `COMPOSER_MIN_ROWS`：改这里，别加 `min_h`）。
///
/// `min_rows` 比助手的输入框（3）大：发信是「一次性写完再发」的动作，开场就该看见
/// 一整段的位置；助手的输入框是聊天框，压扁一点反而省地方。
const COMPOSE_BODY_MIN_ROWS: usize = 8;
const COMPOSE_BODY_MAX_ROWS: usize = 20;
/// 「搜索语法」帮助弹窗宽度。
///
/// 比撰写弹窗（680）窄：这里只有两列短文本，宽度一过 560，说明列每行只剩几个字，
/// 反而更难扫。
const SEARCH_HELP_DIALOG_WIDTH: f32 = 560.;
/// 搜索语法表里「写法」那一列的宽度。
///
/// 按最长的一条量：`has:attachment` 与 `before:2026-01-01`，等宽 13px 下约 130px；
/// 留到 150 是给「说明」列留出发言权——两列各占一半时右侧会被挤成一条窄缝。
const SEARCH_SYNTAX_COLUMN: f32 = 150.;

/// 撤销条存活时长（秒）。
///
/// **与内核的动作宽限期同源**（`duck_mail::queue::UNDO_GRACE_SECS`）：宽限期内动作只在
/// 本地生效、远端尚未补发，所以「撤销」才是真的撤销而不是「撤回一条已发出的命令」。
const UNDO_SECS: u64 = duck_mail::queue::UNDO_GRACE_SECS as u64;

/// 内核事件的轮询周期。
///
/// 40ms 是硬要求（`docs/email-design.md` §4.1）：逐事件 `notify()` 会让
/// prepaint 自激。与 `assistant.rs` 的流式轮询同一档。
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// 邮箱页的键盘上下文。
///
/// 比 `"Root"` 深：页面级键位另开一层才不会和全局动作抢（同 `db.rs` 的 `"Database"`）。
/// 邮箱页目前只有一条页内键位（撰写），但上下文现在就立起来——否则将来给它加第二条时
/// 会先去绑到全局，撞车了才发现。
const KEY_CONTEXT: &str = "Mail";

actions!(
    mail,
    [
        /// 写新邮件（`⌘N`）。与列表条那个「写邮件」按钮是同一条路径。
        NewMessage,
    ]
);

/// 绑定邮箱页的键位（由 `app::install` 调一次）。
///
/// `⌘N` 是「新建」的通用键位，本页最贴近它的动作就是撰写。
///
/// ⚠️ **刻意只绑 `Some(KEY_CONTEXT)`，不写进 `ActionMeta::keys`**：后者会被
/// `duck_command::bind_all` 绑到**全局**上下文（`actions.rs` 在终端那批键位上方
/// 写明了这条）。「刷新收件箱」同理没有页内键位——`⌘R` 已被全局的
/// `toolbox.refresh` 占着（`actions.rs:354`），要用它就得让页面压掉一条全局动作，
/// 那是产品取舍，不在这里单方面定。
pub(crate) fn bind_keys(cx: &mut App) {
    let modifier = duck_command::primary_modifier();
    cx.bind_keys([KeyBinding::new(
        &format!("{modifier}-n"),
        NewMessage,
        Some(KEY_CONTEXT),
    )]);
}

// 三处纵向尺寸**不在本文件定义**，一律取 `DensityTokens`——它们是密度档位的一环，
// 写成本页常量就会出现「调了密度设置、邮箱页不动」：
//
// | 位置 | token | 紧凑 / 标准 / 宽松 |
// | --- | --- | --- |
// | 左栏账户 / 文件夹行 | `density.nav_row` | 24 / 28 / 34 |
// | 左栏分组标题行 | `density.section_header` | 22 / 24 / 28 |
// | 邮件列表行 | `density.mail_row` | 56 / 64 / 76 |
//
// `mail_row` 是 2026-10-12 为邮箱页新加的档（设计稿要 64，而 `task_row` 最宽只有
// 52 且语义是「双行任务」，压不住三行邮件）。`docs/email-design.md` §4.10 之前把
// 这一条列为悬空项，至此收口。

/// 文件夹角色 → 图标。
///
/// 角色来自内核（`FolderRole::from_imap` 认旗标与中英文名），界面只做映射——
/// 「已发送」到底该画纸飞机还是文件夹，是展示决策，不该进内核。
fn role_icon(role: FolderRole) -> DuckIcon {
    match role {
        FolderRole::Inbox => DuckIcon::Inbox,
        FolderRole::Sent => DuckIcon::Send,
        FolderRole::Drafts => DuckIcon::Pencil,
        FolderRole::Archive => DuckIcon::Archive,
        FolderRole::Junk => DuckIcon::TriangleAlert,
        FolderRole::Trash => DuckIcon::Trash2,
        FolderRole::Other => DuckIcon::Folders,
    }
}

/// 文件夹角色 → 界面名（认出来的角色用本地化名，其余用服务端原名）。
fn role_label(role: FolderRole, cx: &App) -> &'static str {
    match role {
        FolderRole::Inbox => tr(cx, "收件箱"),
        FolderRole::Sent => tr(cx, "已发送"),
        FolderRole::Drafts => tr(cx, "草稿"),
        FolderRole::Archive => tr(cx, "归档"),
        FolderRole::Junk => tr(cx, "垃圾邮件"),
        FolderRole::Trash => tr(cx, "已删除"),
        // `Other` 不在这里翻译：那类文件夹的名字是用户自己在服务端起的
        // （`Work/Projects`），本地化它会变成另一个文件夹。
        FolderRole::Other => tr(cx, "文件夹"),
    }
}

/// 文件夹在界面上的显示名。
///
/// - 认出来的角色（`\Sent` 等）用本地化名；
/// - `Other` 用服务端原名，但**必须解 Modified UTF-7**：IMAP 的邮箱名默认是
///   `&…-` 编码的（RFC 3501 §5.1.3），原样显示就是 `&XfJT0ZAB-` 这种乱码。
///   库存的仍是原始名（`SELECT` / 同步命令要用），只在显示这一层解。
fn folder_display_name(folder: &Folder, cx: &App) -> SharedString {
    if folder.role == FolderRole::Other {
        duck_mail::proto::utf7::decode_mailbox_name(&folder.name).into()
    } else {
        role_label(folder.role, cx).into()
    }
}

/// 账户同步状态 → 状态栏文案。
fn sync_state_label(state: SyncState, cx: &App) -> &'static str {
    match state {
        SyncState::Idle => tr(cx, "已同步"),
        SyncState::Syncing => tr(cx, "同步中…"),
        SyncState::Error => tr(cx, "同步失败"),
        SyncState::NeedsReauth => tr(cx, "需要重新登录"),
        SyncState::Offline => tr(cx, "离线"),
    }
}

/// 列表一次加载多少封邮件：设置页「邮箱」里的模块偏好优先，
/// 未设置时回落到出厂默认 [`PREVIEW_LIMIT`]。
fn preview_limit(cx: &App) -> i64 {
    i64::from(super::module_settings::mail_settings(cx).preview_limit)
}

/// 阅读器正文的两种形态。
///
/// 为什么不在 render 里现算：`html_to_text` 与远程图剥离都是 O(正文长度) 的字符串
/// 处理，放 render 就是每帧一次。所以在**取到正文时**定形态，render 只做分支。
#[derive(Clone)]
enum RenderedBody {
    /// 纯文本（原生的 `text/plain`，或用户选了纯文本版本的 HTML 邮件）。
    Plain(SharedString),
    /// HTML（已过 CID 内联；远程图按开关剥离过）。
    Html(SharedString),
}

/// 依据详情 + 两个开关算出正文形态。
///
/// 返回 `(正文, HTML 里是否含远程图片)`——第二个值驱动提示条：只要 HTML 里有远程图，
/// 提示条就在（按钮文案随开关变化），用户不必猜「图为什么没出来」。
///
/// `show_html` 只对「两种正文都有」的邮件有意义（`multipart/alternative`）。
fn render_body(
    detail: &MessageDetail,
    show_html: bool,
    allow_remote_images: bool,
) -> (Option<RenderedBody>, bool) {
    let text = detail.text_body.as_deref().filter(|s| !s.trim().is_empty());
    let html = detail.html_body.as_deref().filter(|s| !s.trim().is_empty());

    let use_html = match (text.is_some(), html.is_some()) {
        // 两种都有：按「设置 → 邮箱 → 默认正文形态」那一档（**出厂 HTML 优先**），
        // 用户可随时在阅读器里切。纯文本版通常是机器压出来的兜底版（丢链接、丢表格、
        // 丢行内图），所以默认不该是它。
        (true, true) => show_html,
        // 只有 HTML 的邮件（营销邮件、Newsletter 常见）必须渲染 HTML，否则没得看。
        (false, true) => true,
        _ => false,
    };

    if use_html && let Some(raw) = html {
        // 表格布局拍平成块级流：营销邮件的嵌套 `<table>` 在文档模型里挤成一团
        // （`duck_mail::mime::flatten_email_tables` 的文档有根因），拍平后按单元格
        // 堆叠，可读性立回来。这一步是打开 / 切换时跑一次，不在 render 每帧。
        let flattened = duck_mail::mime::flatten_email_tables(raw);
        let has_remote = duck_mail::mime::has_remote_images(&flattened);
        let rendered = if has_remote && !allow_remote_images {
            duck_mail::mime::strip_remote_images(&flattened)
        } else {
            flattened
        };
        return (Some(RenderedBody::Html(rendered.into())), has_remote);
    }
    (
        text.map(|t| RenderedBody::Plain(t.to_string().into())),
        false,
    )
}

/// 正文里的链接是否允许交给系统打开器。
///
/// 只放行 `http` / `https` / `mailto`（大小写不敏感）——`docs/email-design.md` §4.8
/// 的第三层：`TextView` 默认会把链接直接交给 `App::open_url`，而正文里的链接是
/// **别人写的**。`file:` 或自定义 scheme 落到系统打开器上就是「点一下就执行本地动作」，
/// 那扇门不该由一封陌生邮件来推。相对链接（无 scheme）也拦掉：邮件里没有可解析的
/// 基准地址，交给系统只会得到「没有程序认这个链接」。
fn openable_link(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .filter(|scheme| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        });
    matches!(
        scheme.map(str::to_ascii_lowercase).as_deref(),
        Some("http" | "https" | "mailto")
    )
}

/// 撤销条的存活内容：一条已入队、尚未补发的动作。
///
/// 不带账户：补发目标是 [`MailPage::flush_account`]，与撤销条分开维护（没有撤销条的
/// 动作也要补发）。
struct UndoEntry {
    /// 动作主键（撤销时传给 `undo_pending`）。
    action_id: i64,
    /// 显示文案（「已移动」「已删除」…）。
    text: SharedString,
    /// 到期时刻；一过就收起撤销条并补发。
    deadline: Instant,
}

/// 把标签颜色（`#RRGGBB`）解析成 `Hsla`；解析不出来时退回主题的三级文字色。
///
/// 颜色是用户建的标签自己带的，可能被手工改坏；解析失败不该 panic，退回中性色即可。
fn label_color(color: &str, cx: &App) -> gpui::Hsla {
    let theme = active_theme(cx);
    let fallback = theme.extended().colors.text_tertiary;
    let hex = color.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return fallback;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    match (channel(0..2), channel(2..4), channel(4..6)) {
        (Some(r), Some(g), Some(b)) => gpui::rgb(u32::from_be_bytes([0, r, g, b])).into(),
        _ => fallback,
    }
}

/// 「本地草稿」在界面上的**合成文件夹 id**。
///
/// 为什么需要它：本地草稿的 `folder_id` 是 NULL，只挂在「草稿箱」那一屏下
/// （见 [`MailPage::is_drafts_folder`]）。而那个入口本来取决于**服务端有没有
/// `\Drafts` 角色**的文件夹——多数服务商有，但没有的话用户就**根本走不到**自己的本地草稿。
/// 所以：账户的文件夹清单里没有 `Drafts` 角色时，界面补一行合成行，id 用它。
///
/// 取**负数**：`folders.id` 是 SQLite 的 `INTEGER PRIMARY KEY`（自增、恒正），负值不可能撞上。
/// 这一行只活在界面内存里——不落库、不参与同步；点开它只会去查 `folder_id = -1`（恒空）
/// 再叠加本地草稿，所以走的是与真实草稿箱**完全相同**的代码路径。
const LOCAL_DRAFTS_FOLDER_ID: i64 = -1;

/// 合成一行「草稿」文件夹（只在服务端没有 `\Drafts` 角色时用，见上）。
///
/// `name` 只是个占位：`folder_row` 对**非 `Other` 角色**一律用 `role_label` 的本地化名渲染，
/// 这一行是 `Drafts` 角色，所以那个 `"Drafts"` 串永远不会显示出来。
fn synthetic_drafts_folder(account_id: i64) -> Folder {
    Folder {
        id: LOCAL_DRAFTS_FOLDER_ID,
        account_id,
        name: "Drafts".to_owned(),
        delimiter: None,
        role: FolderRole::Drafts,
        uidvalidity: None,
        uidnext: None,
        last_seen_uid: 0,
        unread: 0,
        total: 0,
    }
}

/// 邮箱页。
pub struct MailPage {
    /// 左栏折叠 / 展开状态（shell 顶栏的侧栏开关也读它）。
    sidebar: Entity<SidebarState>,
    /// 自引用：后台任务回投与列表行的点击闭包都要用它。
    view: WeakEntity<Self>,

    /// 内核门面。`None` = 还没打开或打开失败（那时 `core_error` 有原因）。
    core: Option<MailCore>,
    /// 打开库失败的原因。
    core_error: Option<SharedString>,

    /// 内核事件通道。`tx` 在库打开后交给内核（注册一次），`rx` 由前台轮询任务 drain。
    events_tx: Option<mpsc::Sender<MailEvent>>,
    events_rx: Option<Receiver<MailEvent>>,
    /// 事件轮询任务：页面存活期间常驻（事件是长期的，不像助手页的流有终点）。
    event_poller: Option<Task<()>>,

    /// 全部账户。
    accounts: Vec<Account>,
    /// 当前账户。
    active_account: Option<i64>,
    /// 当前账户的文件夹（已按角色排序）。
    folders: Vec<Folder>,
    /// 当前文件夹。
    active_folder: Option<i64>,

    /// 列表搜索框（全账号 FTS5 检索）。
    search: Entity<InputState>,
    /// 搜索框的变字订阅（`InputEvent::Change` → 重查）。
    ///
    /// 名字带下划线是**有意的**：这个字段既不被读、也不被改，它存在的唯一意义是
    /// **持有 `Subscription`** —— `Subscription` 一旦 drop 就退订（同
    /// `settings.rs::_font_subscription` / `logs.rs::_search_changed`）。
    /// 编译器的 `dead_code` 警告在这里是噪声。
    _search_subscription: Subscription,
    /// 占位文案最近一次同步到的语言（切语言时才写回 `InputState`）。
    search_placeholder_lang: Option<Lang>,
    /// 当前生效的搜索词；`None` = 不在搜索态，列表跟当前文件夹走。
    search_query: Option<SharedString>,

    /// 当前展示的列表（文件夹列表或搜索结果，取决于 [`Self::search_query`]）。
    messages: Vec<MessageSummary>,
    /// 本地草稿（`drafts_meta`）。**只在选中草稿箱时才拉**——草稿的 `folder_id`
    /// 是 NULL，不属于任何 IMAP 文件夹，所以它们不在 `messages` 里，另行置顶渲染。
    drafts: Vec<DraftSummary>,
    /// 本地草稿**条数**。与 `drafts` 分开存：`drafts` 只在选中草稿箱时才拉，而侧栏
    /// 「草稿」行的角标要在**任何**文件夹下都对（草稿的 `folder_id` 是 NULL，落不进
    /// `folders` 的未读计数，照那个显示会永远是 0）。
    local_drafts: i64,
    /// 当前选中的邮件主键。
    selected: Option<i64>,
    /// 选中邮件的详情。
    detail: Option<MessageDetail>,
    /// 详情对应的正文（形态已定，见 [`RenderedBody`]）。
    body: Option<RenderedBody>,
    /// HTML 正文里含远程图片（提示条据此显示）。
    body_has_remote_images: bool,
    /// 「两种正文都有」时是否展示 HTML 版本。
    show_html: bool,
    /// 是否放行远程图片。**默认放行**（用户拍板：图片默认要显示），提示条上可一键
    /// 收回。**不落盘**（邮箱偏好落哪个是新设计取舍，见 `docs/email-design.md`
    /// §4.11 / §8.5），开一封新邮件时回到默认档。
    allow_remote_images: bool,

    /// 当前账户可见的标签（左栏「标签」分组 + 阅读器「打标签」菜单）。
    labels: Vec<Label>,
    /// 选中的标签（`Some` = 列表按标签过滤；与搜索态互斥，二者都是「列表的另一种来源」）。
    active_label: Option<i64>,
    /// 阅读器当前邮件已贴的标签主键（标签菜单的勾选态）。
    message_labels: Vec<i64>,
    /// 撤销条：一条刚入队、尚未补发的动作。
    undo: Option<UndoEntry>,
    /// 下一个要补发的账户。**没有撤销条的动作也走这里**（例如「打开即已读」）——
    /// 只要动作进了队列，就得有人负责在宽限期内把它推到服务器，否则它永远停在
    /// `pending`（见 [`Self::schedule_flush`]）。
    flush_account: Option<i64>,
    /// 补发时刻。每次入队都把它推到「现在 + 宽限期」（取最晚的一次，保证该时刻
    /// 之前入队的动作全到点）；补发后清空。为 `None` = 没有待补发的动作。
    flush_at: Option<Instant>,

    /// 有后台任务在跑（列表条/阅读器据此显示忙碌态）。
    busy: bool,
    /// 正在「另存为」的附件主键（附件行据此显示「下载中」态）。
    saving_attachment: Option<i64>,
    /// 最近一次另存为失败的 `(附件主键, 原因)`——附件行据此显示失败态，再点一次即重试。
    attachment_error: Option<(i64, SharedString)>,
    /// 本次会话里附件被「另存为」到的**用户选定落点** `(附件主键, 目标路径)`。
    ///
    /// ⚠️ 存的是用户选的那个位置，**不是** `AttachmentMeta::file_path` —— 后者指的是
    /// `app_data_dir()/mail/attachments` 里的应用缓存副本，「在访达中显示」拿它去显示
    /// 会把人带到应用目录，而不是用户刚存的地方。
    saved_attachments: Vec<(i64, PathBuf)>,
    /// 状态栏文案。
    status: SharedString,
}

impl MailPage {
    /// 创建页面：展开左栏，并在后台打开邮箱库。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(*SIDEBAR_WIDTH_RANGE.start())..px(*SIDEBAR_WIDTH_RANGE.end()),
                cx,
            )
        });

        // 事件通道先建好：库是后台打开的，等它回来再把发送端交给它（见 `open_core`）。
        let (events_tx, events_rx) = mpsc::channel();
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr(cx, "搜索邮件，如 from: 或 is:unread"))
        });
        // 订阅要挂在 `cx` 上而不是页面字段之后：变字就重查（FTS5 在本地，单次查询很轻）。
        let search_subscription =
            cx.subscribe(&search, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.run_search(cx);
                }
            });

        let mut this = Self {
            sidebar,
            view: cx.entity().downgrade(),
            core: None,
            core_error: None,
            events_tx: Some(events_tx),
            events_rx: Some(events_rx),
            event_poller: None,
            accounts: Vec::new(),
            active_account: None,
            folders: Vec::new(),
            active_folder: None,
            search,
            _search_subscription: search_subscription,
            search_placeholder_lang: None,
            search_query: None,
            messages: Vec::new(),
            drafts: Vec::new(),
            local_drafts: 0,
            selected: None,
            detail: None,
            body: None,
            body_has_remote_images: false,
            show_html: false,
            allow_remote_images: true,
            labels: Vec::new(),
            active_label: None,
            message_labels: Vec::new(),
            undo: None,
            flush_account: None,
            flush_at: None,
            busy: false,
            saving_attachment: None,
            attachment_error: None,
            saved_attachments: Vec::new(),
            status: tr(cx, "正在打开邮箱数据库…").into(),
        };
        // `window` 只用于构造搜索框与账户弹窗里的输入框（用完后不再需要）。
        this.start_event_poller(cx);
        this.open_core(cx);
        this
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    // ------------------------------------------------------------------
    // 内核事件通道
    // ------------------------------------------------------------------

    /// 起前台轮询任务（只起一次）。
    ///
    /// 内核的事件走 `std::sync::mpsc`，GPUI 的事件循环看不见它，所以要有一个前台任务
    /// 定期取走。任务持有视图**弱引用**：窗口一关、视图释放，`update` 返回 `Err`，
    /// 循环自己结束——不需要显式取消。
    fn start_event_poller(&mut self, cx: &mut Context<Self>) {
        if self.event_poller.is_some() {
            return;
        }
        self.event_poller = Some(cx.spawn(async move |page, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                match page.update(cx, |page, cx| page.drain_events(cx)) {
                    // 通道还在（`true`）就继续；断开（`false`）或视图没了就结束。
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                }
            }
        }));
    }

    /// 取走待处理事件并按变体刷新。返回通道是否还活着。
    ///
    /// 先把一整批事件**收进局部 Vec** 再逐个 `apply_event`：退出 `try_recv` 循环后，
    /// 对 `self.events_rx` 的不可变借用已经结束，`apply_event(&mut self)` 才不会和它
    /// 撞车（直接边收边处理会触发借用检查的 E0502）。
    fn drain_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut pending = Vec::new();
        let mut disconnected = false;
        match self.events_rx.as_ref() {
            Some(rx) => loop {
                match rx.try_recv() {
                    Ok(event) => pending.push(event),
                    Err(TryRecvError::Empty) => break,
                    // 发送端全没了（库没打开成功，或内核被丢掉）：收完这批就收工。
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            },
            None => return false,
        }

        let changed = !pending.is_empty();
        for event in pending {
            self.apply_event(event, cx);
        }
        // 一批变化只重绘一次——这就是 40ms 节流的目的。
        if changed {
            cx.notify();
        }
        // 撤销条到期 / 队列到点补发都在这里一并检查：轮询本来就是 40ms 一跳，
        // 不必为它单起一个定时器。
        self.check_pending_flush(cx);
        !disconnected
    }

    /// 一个事件 → 对应区域的刷新。
    fn apply_event(&mut self, event: MailEvent, cx: &mut Context<Self>) {
        match event {
            // 状态只在本地改：`reload_accounts` 是后台往返，为一个状态字重拉账户不划算。
            MailEvent::AccountState {
                account_id,
                state,
                error,
            } => {
                if let Some(account) = self.accounts.iter_mut().find(|a| a.id == account_id) {
                    account.sync_state = state;
                    account.sync_error = error;
                }
            }
            MailEvent::AccountsChanged => self.reload_accounts(cx),
            MailEvent::FoldersChanged { account_id, .. } => {
                if self.active_account == Some(account_id) {
                    self.reload_folders(cx);
                }
            }
            MailEvent::MessagesAdded {
                account_id,
                folder_id,
                count,
            } => {
                // 用户正在看的就是这个文件夹（且不在搜索态）→ 列表自己刷新即可。
                let viewing_this_folder = self.active_account == Some(account_id)
                    && self.active_folder == Some(folder_id);
                if self.search_query.is_none() && viewing_this_folder {
                    self.reload_messages(cx);
                } else if !viewing_this_folder {
                    // 别的文件夹 / 别的账户来了信 → 推一条通知（G-MAIL-2）。
                    self.notify_new_mail(account_id, folder_id, count, cx);
                }
            }
            // 正文已落库：当前正看着这封、又还没拿到正文时补一次（读本地，不连网）。
            MailEvent::BodyReady { message_id } => {
                if self.selected == Some(message_id)
                    && self
                        .detail
                        .as_ref()
                        .is_none_or(|d| d.body_state != "cached")
                {
                    self.reload_detail(message_id, cx);
                }
            }
            // 索引更新只影响后续检索，不必主动重查（用户下一次输入会带上）。
            MailEvent::SearchIndexUpdated { .. } => {}
            // M1 没有进度条：同步按钮已经用 `busy` 表达「在跑」。
            MailEvent::SyncProgress { .. } => {}
        }
    }

    /// 新邮件到达、且**不是**当前正在看的那个文件夹时推一条通知（G-MAIL-2）。
    ///
    /// 两道闸门：
    /// 1. **只对「常驻增量」出声**：账户处于 `Syncing`（首次全量 / 手动同步那一轮）时
    ///    静默——否则第一次打开邮箱会被几十条「新邮件」淹没，而那一轮的结论已经由同步
    ///    按钮的结果条说清楚了。只有 IDLE 推送 / 增量轮（`Idle`）到达的才算「真的来了
    ///    一封新信」。
    /// 2. **不打扰正在看的**：调用方已经排除了「当前账户 + 当前文件夹」，这里不再重复。
    ///
    /// 通道走 [`notify::system_with_toast`]：新邮件既要在应用内横幅看得见，也要落到
    /// 系统通知中心（用户可能正在别的应用里）。两条通道各自看自己的开关，历史只记一条
    /// —— 这正是 `AGENTS.md` §3.8 给「需要系统通知中心」场景定下的语义。
    fn notify_new_mail(&self, account_id: i64, folder_id: i64, count: u64, cx: &mut Context<Self>) {
        if count == 0 {
            return;
        }
        // 闸门 0：模块偏好里关掉「新邮件通知」就整条不出声（两条通道一起关，
        // 与「通知」设置页里的通道开关是**两层**：那里管通道，这里管这条消息该不该出声）。
        if !super::module_settings::mail_settings(cx).notify_new_mail {
            return;
        }
        // 闸门 1：账户必须已经处于 `Idle`（常驻增量），全量 / 手动同步那一轮不出声。
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id) else {
            return;
        };
        if account.sync_state != SyncState::Idle {
            return;
        }
        let account_name = account
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| account.email.clone());
        // 文件夹名优先取已加载的清单；非当前账户的文件夹没加载过，回落到一次本地读
        // （`MailCore::folders` 是纯 SQLite 查询，不连网）。显示名要解 Modified UTF-7，
        // 否则通知里会带着 `&XfJT0ZAB-` 这种乱码。
        let folder_name = self
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| duck_mail::proto::utf7::decode_mailbox_name(&f.name))
            .or_else(|| {
                self.core
                    .as_ref()
                    .and_then(|core| core.folders(account_id).ok())
                    .and_then(|folders| folders.into_iter().find(|f| f.id == folder_id))
                    .map(|f| duck_mail::proto::utf7::decode_mailbox_name(&f.name))
            })
            .unwrap_or_else(|| tr(cx, "文件夹").to_owned());
        let title = tr(cx, "新邮件").to_owned();
        let message = trf(
            cx,
            "{} · {}：{} 封新邮件",
            &[&account_name, &folder_name, &count.to_string()],
        );
        crate::update::on_notify_window(cx, move |window, cx| {
            notify::system_with_toast(title.clone(), message.clone(), window, cx);
        });
    }

    // ------------------------------------------------------------------
    // 后台任务
    // ------------------------------------------------------------------

    /// 把阻塞内核调用丢进后台执行器，完成后回主线程跑 `done`。
    ///
    /// 与 `RedisPage::spawn_redis` 同源：内核全是同步 API，直接在渲染线程上调用会
    /// 把 UI 冻住（一次 IMAP 往返在网络差的时候是秒级）。
    fn spawn<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.spawn_with(true, cx, work, done);
    }

    /// 同 [`Self::spawn`]，但**不点亮 `busy`**——用于纯本地读（读标签、读详情），
    /// 它们没有网络往返，没必要让列表条上的同步按钮闪一下禁用态。
    fn spawn_local<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        self.spawn_with(false, cx, work, done);
    }

    fn spawn_with<T, F, D>(&mut self, mark_busy: bool, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    {
        if mark_busy {
            self.busy = true;
        }
        cx.notify();
        let task = cx.background_executor().spawn(async move { work() });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let value = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if mark_busy {
                            this.busy = false;
                        }
                        done(this, value, window, cx);
                        cx.notify();
                    });
                });
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------
    // 打开内核 → 拉账户 / 文件夹 / 列表
    // ------------------------------------------------------------------

    /// 后台打开邮箱库；成功后接着拉账户。
    fn open_core(&mut self, cx: &mut Context<Self>) {
        self.spawn(
            cx,
            MailCore::open_default,
            |this, opened, _window, cx| match opened {
                Ok(core) => {
                    // 事件通道在这里交给内核——**注册一次**，之后所有 emit 都进页面。
                    // 没有它，同步/拉正文的事件全部静默丢弃，页面只能靠手动 reload。
                    if let Some(tx) = this.events_tx.take() {
                        core.set_event_sink(tx);
                    }
                    this.core = Some(core);
                    this.core_error = None;
                    this.reload_accounts(cx);
                }
                Err(error) => {
                    // 打不开库是**硬错误**：状态栏说清楚，界面走空态而不是假装没事。
                    tracing::error!(%error, "opening the mail database failed");
                    let text = trf(cx, "打开邮箱数据库失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    this.core_error = Some(text.into());
                }
            },
        );
    }

    /// 拉账户列表，并选中「上次那个 / 第一个」账户。
    fn reload_accounts(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn(
            cx,
            move || core.accounts(),
            |this, result, _window, cx| match result {
                Ok(accounts) => {
                    this.accounts = accounts;
                    // 账户列表落定（首次打开 / 增删）后，为每个账户拉起常驻同步 actor
                    // （G-MAIL-1：IDLE 推送 + 断线退避重连 + 变更触发增量）。**幂等**——
                    // 已在跑的账号被 `start_sync_actor` 内部按 key 跳过，所以这里可以无条件
                    // 全量启一遍；账号移除时那条路径已经先把它的 actor 停掉了（见
                    // `MailCore::remove_account`）。
                    if let Some(core) = this.core.as_ref() {
                        for account in &this.accounts {
                            if let Err(error) = core.start_sync_actor(account.id) {
                                tracing::warn!(
                                    account_id = account.id,
                                    %error,
                                    "mail: cannot start the resident sync actor"
                                );
                            }
                        }
                    }
                    // 保持当前选择；选中的账户被删掉（或首次进入）时落到第一个。
                    let still_there = this
                        .active_account
                        .is_some_and(|id| this.accounts.iter().any(|a| a.id == id));
                    if !still_there {
                        this.active_account = this.accounts.first().map(|a| a.id);
                    }
                    this.status = match this.accounts.len() {
                        0 => tr(cx, "尚未添加邮箱账户").into(),
                        _ => trf(cx, "{} 个账户", &[&this.accounts.len().to_string()]).into(),
                    };
                    this.reload_folders(cx);
                }
                Err(error) => {
                    this.status = trf(cx, "读取账户失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 拉当前账户的文件夹，并选中「收件箱 / 第一个」。
    fn reload_folders(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            self.folders.clear();
            self.active_folder = None;
            self.labels.clear();
            self.active_label = None;
            self.messages.clear();
            self.drafts.clear();
            self.local_drafts = 0;
            self.detail = None;
            self.clear_body();
            return;
        };
        let keep = self.active_folder;
        self.spawn(
            cx,
            move || core.folders(account_id),
            move |this, result, _window, cx| match result {
                Ok(mut folders) => {
                    // 排序放在界面：内核只按主键给，界面要的是「常用在前」。
                    // 同名角色之间按**显示名**排（原始名是 Modified UTF-7，按它排
                    // 等于按乱码排）。
                    folders.sort_by_key(|f| {
                        (
                            f.role.order(),
                            duck_mail::proto::utf7::decode_mailbox_name(&f.name),
                        )
                    });
                    // 服务端没有 `\Drafts` 角色文件夹时补一行合成的（见
                    // `LOCAL_DRAFTS_FOLDER_ID`）——否则本地草稿**没有任何入口**。
                    // ⚠️ 补在算 `still_there` **之前**：不然「已经停在草稿箱」会被判成
                    // 「这个文件夹没了」，一点同步就把用户踢回收件箱。
                    // ⚠️ `folders.is_empty()` = **还没同步过**（IMAP 一定有 INBOX），
                    // 这时不补合成行——保持「点「同步」拉取文件夹」那条提示原样。
                    if !folders.is_empty() && !folders.iter().any(|f| f.role == FolderRole::Drafts)
                    {
                        folders.push(synthetic_drafts_folder(account_id));
                    }
                    this.folders = folders;
                    this.reload_draft_count(cx);
                    let still_there =
                        keep.is_some_and(|id| this.folders.iter().any(|f| f.id == id));
                    if !still_there {
                        // 有收件箱就落收件箱，否则第一个。
                        this.active_folder = this
                            .folders
                            .iter()
                            .find(|f| f.role == FolderRole::Inbox)
                            .or_else(|| this.folders.first())
                            .map(|f| f.id);
                    }
                    this.reload_messages(cx);
                    this.reload_labels(cx);
                }
                Err(error) => {
                    this.status = trf(cx, "读取文件夹失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 拉当前账户可见的标签（左栏「标签」分组 + 阅读器标签菜单）。
    fn reload_labels(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            self.labels.clear();
            self.active_label = None;
            return;
        };
        self.spawn_local(
            cx,
            move || core.labels(account_id),
            move |this, result, _window, cx| match result {
                Ok(labels) => {
                    // 选中的标签若被删了，退回文件夹视图，别停在一个空列表上。
                    if this
                        .active_label
                        .is_some_and(|id| !labels.iter().any(|l| l.id == id))
                    {
                        this.active_label = None;
                        this.reload_messages(cx);
                    }
                    this.labels = labels;
                }
                Err(error) => {
                    this.status = trf(cx, "读取标签失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 拉当前列表：搜索态拉检索结果，否则拉当前文件夹。
    fn reload_messages(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(query) = self.search_query.clone() else {
            match self.active_label {
                Some(label_id) => self.reload_label_messages(label_id, cx),
                None => self.reload_folder_messages(cx),
            }
            return;
        };
        // 两个闭包都要用这个词：一个拿去查、一个用来判断结果是否**还生效**，
        // 所以各持一份（`SharedString` 克隆只加引用计数）。
        let guard = query.clone();
        let limit = preview_limit(cx);
        self.spawn(
            cx,
            move || core.search_messages(&query, limit),
            move |this, result, _window, cx| {
                // 打字快时会有多个查询在飞：只有**还生效**的那个查询的结果才写回，
                // 否则慢的旧查询会覆盖新查询（列表闪回上一个关键词的结果）。
                if this.search_query.as_deref() != Some(guard.as_str()) {
                    return;
                }
                match result {
                    Ok(messages) => {
                        let count = messages.len();
                        this.messages = messages;
                        this.status = if count == 0 {
                            tr(cx, "没有匹配的邮件").into()
                        } else {
                            trf(cx, "搜索到 {} 封邮件", &[&count.to_string()]).into()
                        };
                    }
                    Err(error) => {
                        this.status = trf(cx, "搜索失败：{}", &[&error.to_string()]).into();
                    }
                }
            },
        );
    }

    /// 拉当前文件夹的邮件列表。
    fn reload_folder_messages(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(folder_id) = self.active_folder else {
            self.messages.clear();
            self.drafts.clear();
            return;
        };
        let limit = preview_limit(cx);
        self.spawn(
            cx,
            move || core.messages(folder_id, limit, 0),
            |this, result, _window, cx| match result {
                Ok(messages) => {
                    // 选中的那封可能已经不在这一页里（换了文件夹）。
                    if !this
                        .selected
                        .is_some_and(|id| messages.iter().any(|m| m.id == id))
                    {
                        this.selected = None;
                        this.detail = None;
                        this.clear_body();
                    }
                    let count = messages.len();
                    this.messages = messages;
                    this.status = trf(cx, "{} 封邮件", &[&count.to_string()]).into();
                }
                Err(error) => {
                    this.status = trf(cx, "读取邮件列表失败：{}", &[&error.to_string()]).into();
                }
            },
        );
        // 草稿箱要多拉一份**本地草稿**：它们的 `folder_id` 是 NULL，压根不在
        // `messages` 里（见 `duck_mail::db::repo::drafts` 模块头）。别的文件夹则把
        // 上一轮留下的草稿清掉，免得切来切去之后列表上还挂着别的文件夹的东西。
        if self.is_drafts_folder(folder_id) {
            self.reload_drafts(cx);
        } else {
            self.drafts.clear();
        }
    }

    /// 某个文件夹是不是「草稿箱」角色（决定列表要不要置顶本地草稿）。
    ///
    /// 判据是**角色**而不是文件夹名：`Drafts` 是 IMAP 的 `\Drafts` 特殊用途邮箱，
    /// 名字各服务商不同（`Drafts` / `草稿箱` / `草稿`），角色是唯一稳的那一个。
    fn is_drafts_folder(&self, folder_id: i64) -> bool {
        self.folders
            .iter()
            .any(|folder| folder.id == folder_id && folder.role == FolderRole::Drafts)
    }

    /// 拉一次**本地草稿条数**（侧栏「草稿」行的角标）。
    ///
    /// 不复用 [`Self::reload_drafts`]：那个只在选中草稿箱时才跑，而角标要在**任何**文件夹
    /// 下都对。纯本地读（一条 `COUNT(*)`）→ `spawn_local`，不点亮同步按钮。
    fn reload_draft_count(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            self.local_drafts = 0;
            return;
        };
        self.spawn_local(
            cx,
            move || core.draft_count(account_id),
            |this, result, _window, _cx| {
                if let Ok(count) = result {
                    this.local_drafts = count;
                }
            },
        );
    }

    /// 拉本地草稿列表。
    ///
    /// 纯本地读（`drafts_meta` 就在本地库），没有网络往返 → `spawn_local`，不点亮列表条
    /// 上的同步按钮。
    fn reload_drafts(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            self.drafts.clear();
            return;
        };
        self.spawn_local(
            cx,
            move || core.list_drafts(account_id),
            |this, result, _window, cx| match result {
                Ok(drafts) => {
                    // 顺带把侧栏角标对齐（新存 / 删掉一条之后，列表与角标必须同源）。
                    this.local_drafts = drafts.len() as i64;
                    this.drafts = drafts;
                }
                Err(error) => {
                    this.status = trf(cx, "读取草稿失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 拉某个标签下的邮件（跨文件夹，按时间倒序）。
    fn reload_label_messages(&mut self, label_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let limit = preview_limit(cx);
        self.spawn_local(
            cx,
            move || core.messages_by_label(label_id, limit),
            move |this, result, _window, cx| {
                // 慢查询回来时用户可能已经换了标签：只有还生效的结果才写回。
                if this.active_label != Some(label_id) {
                    return;
                }
                match result {
                    Ok(messages) => {
                        if !this
                            .selected
                            .is_some_and(|id| messages.iter().any(|m| m.id == id))
                        {
                            this.selected = None;
                            this.detail = None;
                            this.clear_body();
                        }
                        this.status = trf(cx, "{} 封邮件", &[&messages.len().to_string()]).into();
                        this.messages = messages;
                    }
                    Err(error) => {
                        this.status = trf(cx, "读取标签邮件失败：{}", &[&error.to_string()]).into();
                    }
                }
            },
        );
    }

    /// 搜索框变字：更新生效的搜索词并查一次（空串 = 回到文件夹列表）。
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().trim().to_owned();
        let next = (!query.is_empty()).then(|| SharedString::from(query));
        let changed = self.search_query != next;
        self.search_query = next;
        if self.search_query.is_some() {
            // 检索与标签过滤都是「列表的另一种来源」，同时只留一个。
            self.active_label = None;
        }
        if changed {
            // 换过的检索意义不同（相关度序 vs 时间序），旧的选中与正文都作废。
            self.selected = None;
            self.detail = None;
            self.clear_body();
        }
        self.reload_messages(cx);
    }

    /// 退出搜索态（选文件夹 / 换账户时调）：清空输入框与生效搜索词。
    ///
    /// `set_value` 期间输入框**不派发** `InputEvent::Change`（见 `gpui-base` 的实现），
    /// 所以这里不会递归触发一次多余的查询。
    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_query.is_none() && self.search.read(cx).value().is_empty() {
            return;
        }
        self.search_query = None;
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// 语言切换后同步搜索框占位文案（同 `SettingsWindowView::sync_search_placeholder`）。
    ///
    /// 占位文案是 `InputState` 里的**状态**，不是每次 render 读的 props：语言切了必须
    /// 显式写回，否则中文界面下会一直留着上一次的英文占位。
    fn sync_search_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = active_theme(cx).lang();
        if self.search_placeholder_lang == Some(lang) {
            return;
        }
        self.search_placeholder_lang = Some(lang);
        let placeholder = tr(cx, "搜索邮件，如 from: 或 is:unread");
        self.search.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
    }

    /// 把正文相关的三个字段一起清掉（它们必须同步，否则会出现「有正文没形态」）。
    fn clear_body(&mut self) {
        self.body = None;
        self.body_has_remote_images = false;
        self.show_html = false;
        self.allow_remote_images = true;
    }

    /// 只重读一封邮件的详情（读本地库，不连网）。
    fn reload_detail(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn(
            cx,
            move || core.message_detail(message_id),
            move |this, result, _window, cx| match result {
                Ok(detail) => {
                    if this.selected == Some(message_id) {
                        this.set_detail(detail, cx);
                    }
                }
                Err(error) => {
                    this.status = trf(cx, "打开邮件失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 交互
    // ------------------------------------------------------------------

    /// 切换账户。
    fn select_account(&mut self, account_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_account == Some(account_id) {
            return;
        }
        self.active_account = Some(account_id);
        // 换了账户，上一个账户的文件夹与列表全部作废（不然会显示成新账户的）。
        self.active_folder = None;
        self.folders.clear();
        self.messages.clear();
        // 草稿也是按账户存的，上一个账户的不能留在列表上（角标同理）。
        self.drafts.clear();
        self.local_drafts = 0;
        self.selected = None;
        self.detail = None;
        self.clear_body();
        // 检索结果跨账号，换账户时留在搜索态会让人以为「切过去了但列表没变」。
        self.clear_search(window, cx);
        cx.notify();
        self.reload_folders(cx);
    }

    /// 切换文件夹。
    fn select_folder(&mut self, folder_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_folder == Some(folder_id)
            && self.search_query.is_none()
            && self.active_label.is_none()
        {
            return;
        }
        self.active_folder = Some(folder_id);
        // 点文件夹 = 明确要看这个文件夹，退出搜索态与标签过滤。
        self.active_label = None;
        self.messages.clear();
        self.selected = None;
        self.detail = None;
        self.clear_body();
        self.clear_search(window, cx);
        cx.notify();
        self.reload_messages(cx);
    }

    /// 切换标签（左栏「标签」分组）：列表改为该标签下的邮件。
    fn select_label(&mut self, label_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_label == Some(label_id) && self.search_query.is_none() {
            return;
        }
        self.active_label = Some(label_id);
        self.messages.clear();
        self.selected = None;
        self.detail = None;
        self.clear_body();
        self.clear_search(window, cx);
        cx.notify();
        self.reload_messages(cx);
    }

    /// 打开一封邮件：先给本地详情（列表阶段已有信封），正文没缓存再后台拉。
    fn open_message(&mut self, message_id: i64, cx: &mut Context<Self>) {
        if self.selected == Some(message_id) {
            return;
        }
        self.selected = Some(message_id);
        self.detail = None;
        self.clear_body();
        cx.notify();

        let Some(core) = self.core.clone() else {
            return;
        };
        // 打开即已读（与所有图形邮件客户端一致）；本地失败不影响看正文。
        let mark_read = self
            .messages
            .iter()
            .find(|m| m.id == message_id)
            .is_some_and(|m| !m.is_read);
        self.spawn(
            cx,
            move || {
                if mark_read {
                    let _ = core.set_message_read(message_id, true);
                }
                let detail = core.message_detail(message_id)?;
                // 正文没缓存就顺手拉一次。
                //
                // ⚠️ **拉不到绝不能静默**。旧实现是
                // `core.fetch_message_body(message_id).unwrap_or(detail)`——错误被整条
                // 吞掉，界面于是只显示「正文还没下载」，用户完全看不出是网络不通、
                // 认证失败还是服务端拒绝（「正文自动获取看起来没生效」的根因就在这里，
                // 而且它还会让人以为要手动点「下载正文」）。失败原因原样带回主线程，
                // 由界面包一层 `trf` 外壳——内核错误一律英文（AGENTS §i18n）。
                if detail.body_state == "cached" {
                    return Ok((detail, None));
                }
                match core.fetch_message_body(message_id) {
                    Ok(fetched) => Ok((fetched, None)),
                    Err(error) => Ok((detail, Some(error.to_string()))),
                }
            },
            move |this,
                  result: Result<(MessageDetail, Option<String>), duck_mail::MailError>,
                  window,
                  cx| {
                // 「打开即已读」没有撤销条，但动作照样进了队列：安排一次补发，否则
                // 这条 MarkRead 会一直停在 `pending`（读邮件是最常走的路径）。
                // 不用 `if … && let …`：let-chain 要到 1.88 才稳定，本仓 MSRV 是 1.85。
                if let Some(account_id) = this.active_account.filter(|_| mark_read) {
                    this.schedule_flush(account_id, cx);
                }
                match result {
                    Ok((detail, body_error)) => {
                        if mark_read {
                            // 顺手把列表那一行也标成已读，不必再拉一次列表。
                            if let Some(row) = this.messages.iter_mut().find(|m| m.id == message_id)
                            {
                                row.is_read = true;
                            }
                        }
                        this.set_detail(detail, cx);
                        if let Some(reason) = body_error {
                            // 正文拉取失败是用户在正文区**直接看到**的问题（一片空白），
                            // 只在状态栏写一句不够，要同时给一条能看见的通知。
                            let text = trf(cx, "正文未能获取：{}", &[&reason]);
                            this.status = text.clone().into();
                            notify::error(text, window, cx);
                        }
                    }
                    Err(error) => {
                        this.status = trf(cx, "打开邮件失败：{}", &[&error.to_string()]).into();
                    }
                }
            },
        );
    }

    /// 单独重拉正文（正文之前没拉到时的「下载正文」）。
    fn fetch_body(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn(
            cx,
            move || core.fetch_message_body(message_id),
            |this, result, window, cx| match result {
                Ok(detail) => {
                    this.set_detail(detail, cx);
                    notify::success(tr(cx, "正文已下载"), window, cx);
                }
                Err(error) => {
                    let text = trf(cx, "下载正文失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                }
            },
        );
    }

    /// 把详情转成界面状态：正文形态在这里定一次（见 [`render_body`]）。
    fn set_detail(&mut self, detail: MessageDetail, cx: &mut Context<Self>) {
        // 换了一封邮件就回到默认口径：远程图回到「默认显示」（提示条可一键收回），
        // 正文形态回到「设置 → 邮箱」里选的那一档（出厂默认 HTML 优先）。
        // 这两个开关**不跨邮件继承**——否则用户在某封邮件上拦一次，之后每一封
        // 都悄悄照搬，图没了都不知道去哪开。
        self.show_html = super::module_settings::mail_settings(cx).prefer_html;
        self.allow_remote_images = true;
        let message_id = detail.id;

        // ⚠️ 渲染的必须是**这一封**。旧实现 `let Some(detail) =
        // self.detail.replace(detail) else { return }` 把 `replace` 的返回值
        // （**旧详情**）拿去渲染：头衔是新的、正文是上一封的；而且页面刚打开时
        // `self.detail` 是 `None`，`replace` 回 `None` 直接早退——第一封永远没正文，
        // 要等 `BodyReady` 二次回调才碰巧盖回来。这就是「切走再回来正文没了」。
        let status_line = trf(
            cx,
            "{} · {}",
            &[
                subject_or_placeholder(&detail.subject, tr(cx, "(无主题)")),
                sender_display(
                    detail.from_name.as_deref(),
                    detail.from_addr.as_deref(),
                    tr(cx, "(未知发件人)"),
                ),
            ],
        );
        let (body, has_remote) = render_body(&detail, self.show_html, self.allow_remote_images);
        self.detail = Some(detail);
        self.body = body;
        self.body_has_remote_images = has_remote;
        self.status = status_line.into();
        // 标签菜单的勾选态依赖「这封邮件贴了哪些标签」，与详情一起刷新。
        self.reload_message_labels(message_id, cx);
    }

    /// 拉一封邮件已贴的标签（标签菜单勾选态；纯本地读）。
    fn reload_message_labels(&mut self, message_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn_local(
            cx,
            move || core.labels_for_message(message_id),
            move |this, result, _window, cx| {
                if this.selected != Some(message_id) {
                    return;
                }
                match result {
                    Ok(labels) => {
                        this.message_labels = labels.into_iter().map(|l| l.id).collect();
                        cx.notify();
                    }
                    // 纯本地读失败（库坏了）不该盖掉状态栏里更要紧的信息，只记日志。
                    Err(error) => {
                        tracing::warn!(message_id, %error, "reading message labels failed");
                    }
                }
            },
        );
    }

    /// 切换正文版本（只在 `multipart/alternative` 的两种正文都有时可用）。
    ///
    /// 切回纯文本时**顺带把远程图重新拦上**：用户切纯文本的动机通常正是「别去连
    /// 远程」。反过来切 HTML 不动远程开关（放行由提示条上的按钮决定）。
    fn toggle_body_view(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        if detail.text_body.as_deref().is_none_or(str::is_empty)
            || detail.html_body.as_deref().is_none_or(str::is_empty)
        {
            return;
        }
        self.show_html = !self.show_html;
        if !self.show_html {
            // 切回纯文本后远程图无关紧要，开关回默认档（放行）。
            self.allow_remote_images = true;
        }
        let (body, has_remote) = render_body(detail, self.show_html, self.allow_remote_images);
        self.body = body;
        self.body_has_remote_images = has_remote;
        cx.notify();
    }

    /// 放行 / 重新拦截远程图片。
    fn toggle_remote_images(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.as_ref() else {
            return;
        };
        self.allow_remote_images = !self.allow_remote_images;
        let (body, has_remote) = render_body(detail, self.show_html, self.allow_remote_images);
        self.body = body;
        self.body_has_remote_images = has_remote;
        cx.notify();
    }

    /// 「两种正文都有」吗——工具条上的版本切换按钮只在此时出现。
    fn has_both_bodies(&self) -> bool {
        self.detail.as_ref().is_some_and(|d| {
            d.text_body.as_deref().is_some_and(|s| !s.trim().is_empty())
                && d.html_body.as_deref().is_some_and(|s| !s.trim().is_empty())
        })
    }

    /// 切换选中邮件的已读状态。
    fn toggle_read(&mut self, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let next = self.detail.as_ref().map(|d| !d.is_read).unwrap_or(true);
        // 本地先翻（内核也会写库，这里只是不想等一次后台往返才有反馈）。
        if let Some(row) = self.messages.iter_mut().find(|m| m.id == message_id) {
            row.is_read = next;
        }
        if let Some(detail) = self.detail.as_mut() {
            detail.is_read = next;
        }
        let text: SharedString = tr(
            cx,
            if next {
                "已标为已读"
            } else {
                "已标为未读"
            },
        )
        .into();
        self.status = text.clone();
        self.run_action(
            cx,
            move |core| core.set_read(account_id, message_id, next),
            text,
        );
    }

    // ------------------------------------------------------------------
    // 动作队列：旗标 / 移动 / 删除 / 标签 + 撤销条
    //
    // 每个动作走同一条路：内核把「净效果」写进本地库（乐观更新）+ 入队；这里只负责
    // 立刻翻转界面、亮起撤销条、刷新列表。真正的远端补发由
    // [`Self::check_pending_flush`] 在宽限期结束时触发。
    // ------------------------------------------------------------------

    /// 入队一个动作；成功就亮起撤销条，失败在状态栏与通知里说清楚。
    fn run_action<F>(&mut self, cx: &mut Context<Self>, work: F, undo_text: SharedString)
    where
        F: FnOnce(&MailCore) -> Result<i64, duck_mail::MailError> + Send + 'static,
    {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        self.spawn(
            cx,
            move || work(&core),
            move |this, result, window, cx| match result {
                Ok(action_id) => {
                    this.arm_undo(account_id, action_id, undo_text, cx);
                    this.refresh_after_action(cx);
                }
                Err(error) => {
                    let text = trf(cx, "操作失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                }
            },
        );
    }

    /// 亮起撤销条（覆盖上一条：同一时刻只有一条可撤销）。
    fn arm_undo(
        &mut self,
        account_id: i64,
        action_id: i64,
        text: SharedString,
        cx: &mut Context<Self>,
    ) {
        let deadline = Instant::now() + Duration::from_secs(UNDO_SECS);
        self.undo = Some(UndoEntry {
            action_id,
            text,
            deadline,
        });
        // 撤销条与补发共用同一个到点时刻：宽限期一过，条子收起、动作补发。
        self.flush_account = Some(account_id);
        self.flush_at = Some(deadline);
        cx.notify();
    }

    /// 安排一次补发，但**不亮撤销条**（用于没有可见撤销入口的动作，如「打开即已读」）。
    ///
    /// 每次入队都重置到「现在 + 宽限期」：取最晚的一次，保证补发时刻之前入队的动作
    /// 全部到点（[`MailCore::flush_pending`] 一次把所有到点动作推完）。已知副作用：
    /// 宽限期内连续操作（如 <8s 连读多封信）会把补发一直往后推——用户停手后自愈，
    /// 且「同步成功后补发」（见 [`Self::sync_now`]）兜了一层，不做去抖处理。
    fn schedule_flush(&mut self, account_id: i64, cx: &mut Context<Self>) {
        self.flush_account = Some(account_id);
        self.flush_at = Some(Instant::now() + Duration::from_secs(UNDO_SECS));
        cx.notify();
    }

    /// 动作生效后刷新受影响的区域（都是本地读，不连网）。
    fn refresh_after_action(&mut self, cx: &mut Context<Self>) {
        self.reload_messages(cx);
        if let Some(message_id) = self.selected {
            self.reload_detail(message_id, cx);
        }
    }

    /// 每 40ms 一跳：撤销条到期就收起，到点就把队列里的动作补发到服务器。
    ///
    /// 两件事分开判——撤销条只在**显式动作**后出现，而补发对所有入队动作都成立
    /// （「打开即已读」没有撤销条，但一样要推上去）。
    fn check_pending_flush(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self
            .undo
            .as_ref()
            .is_some_and(|entry| now >= entry.deadline)
        {
            self.undo = None;
            cx.notify();
        }
        let Some(at) = self.flush_at else {
            return;
        };
        if now < at {
            return;
        }
        self.flush_at = None;
        let Some(account_id) = self.flush_account.take() else {
            return;
        };
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn(
            cx,
            move || core.flush_pending(account_id),
            |this, result, _window, cx| match result {
                // 0 条 = 别的路径已经补发过了，不必刷状态栏。
                Ok(0) => {}
                Ok(count) => {
                    this.status = trf(cx, "已同步 {} 个操作", &[&count.to_string()]).into();
                }
                Err(error) => {
                    this.status = trf(cx, "同步操作失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 撤销条上的「撤销」：取消队列行 + 反向复原本地。
    fn undo_pending_action(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.take() else {
            return;
        };
        let action_id = entry.action_id;
        cx.notify();
        let Some(core) = self.core.clone() else {
            return;
        };
        self.spawn(
            cx,
            move || core.undo_pending(action_id),
            |this, result, window, cx| match result {
                Ok(true) => {
                    this.status = tr(cx, "已撤销").into();
                    this.refresh_after_action(cx);
                }
                // 已经补发（或已被撤过）：以库里的真实状态为准刷新一次。
                Ok(false) => this.refresh_after_action(cx),
                Err(error) => {
                    let text = trf(cx, "撤销失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                }
            },
        );
    }

    /// 加 / 去选中邮件的旗标。
    fn toggle_star(&mut self, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let starred = self.detail.as_ref().map(|d| !d.is_starred).unwrap_or(true);
        if let Some(row) = self.messages.iter_mut().find(|m| m.id == message_id) {
            row.is_starred = starred;
        }
        if let Some(detail) = self.detail.as_mut() {
            detail.is_starred = starred;
        }
        let text: SharedString = tr(
            cx,
            if starred {
                "已加旗标"
            } else {
                "已取消旗标"
            },
        )
        .into();
        self.status = text.clone();
        self.run_action(
            cx,
            move |core| core.set_starred(account_id, message_id, starred),
            text,
        );
    }

    /// 把选中邮件移到某个文件夹。
    fn move_selected(&mut self, folder_id: i64, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let text: SharedString = tr(cx, "已移动").into();
        self.status = text.clone();
        self.run_action(
            cx,
            move |core| core.move_message(account_id, message_id, folder_id),
            text,
        );
    }

    /// 删除选中邮件。
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let text: SharedString = tr(cx, "已删除").into();
        self.status = text.clone();
        self.run_action(
            cx,
            move |core| core.delete_message(account_id, message_id),
            text,
        );
    }

    /// 给选中邮件加 / 去某个标签。
    fn toggle_label(&mut self, label_id: i64, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let attached = self.message_labels.contains(&label_id);
        let name = self
            .labels
            .iter()
            .find(|l| l.id == label_id)
            .map(|l| l.name.clone())
            .unwrap_or_default();
        let text: SharedString = trf(
            cx,
            if attached {
                "已移除标签「{}」"
            } else {
                "已添加标签「{}」"
            },
            &[&name],
        )
        .into();
        self.status = text.clone();
        self.run_action(
            cx,
            move |core| {
                if attached {
                    core.label_remove(account_id, message_id, label_id)
                } else {
                    core.label_add(account_id, message_id, label_id)
                }
            },
            text,
        );
    }

    /// 建一个账户级标签（左栏「标签」分组的「+」）。
    fn create_label(
        &mut self,
        account_id: i64,
        name_input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(core) = self.core.clone() else {
            notify::error(tr(cx, "邮箱数据库尚未打开"), window, cx);
            return false;
        };
        let name = name_input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            notify::warning(tr(cx, "标签名不能为空"), window, cx);
            return false;
        }
        self.spawn(
            cx,
            move || core.label_create(account_id, &name, "#6b7280"),
            |this, result, window, cx| match result {
                Ok(_id) => {
                    window.close_dialog(cx);
                    notify::success(tr(cx, "标签已创建"), window, cx);
                    this.reload_labels(cx);
                }
                Err(error) => {
                    notify::error(
                        trf(cx, "创建标签失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
        true
    }

    /// 打开「新建标签」弹窗。
    fn open_label_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account_id) = self.active_account else {
            return;
        };
        let name_input = text_input(window, cx, "标签名");
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title: SharedString = tr(cx, "新建标签").into();

        let confirm: ConfirmHandler = {
            let name_input = name_input.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |this, cx| {
                    this.create_label(account_id, &name_input, window, cx)
                })
                .unwrap_or(true)
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let title = title.clone();
            let confirm = Rc::clone(&confirm);
            let name_for_content = name_input.clone();
            let drag_for_content = drag.clone();
            let offset = modal_offset(&drag, cx);

            dialog
                .w(px(LABEL_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "创建"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "mail-label-title",
                                &drag_for_content,
                                title.clone(),
                                cx,
                            ))
                            .child(form_field(
                                tr(cx, "标签名"),
                                Input::new(&name_for_content).control(cx).w_full(),
                                None,
                                Some(SharedString::from(tr(cx, "仅本账户可见。"))),
                                cx,
                            )),
                    ))
                })
                .on_ok(move |_, window, cx| confirm(window, cx))
        });
    }

    /// 同步当前账户（真连服务器）。
    ///
    /// 这里**不再手动 reload**：内核在同步过程中会依次 emit `AccountState`
    /// （同步中 → 已同步 / 失败）、`FoldersChanged`、`MessagesAdded`，由事件轮询任务
    /// 按变体刷新账户行 / 文件夹 / 列表。手动再拉一遍只是重复一次后台往返
    /// （见模块头的「变化从哪来」一节）。这个回调只负责**用户可见的结论**：状态栏与通知。
    fn sync_now(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        self.status = tr(cx, "正在同步…").into();
        // 嵌套 Result 的外层错误类型推断不出来，这里显式标注（别拆成两次 spawn：
        // 补发必须只在上面的同步真正成功后跑）。
        type SyncOutcome = (
            duck_mail::sync::SyncReport,
            Result<usize, duck_mail::MailError>,
        );
        self.spawn(
            cx,
            move || {
                // 同步成功后把队列里到点的动作补发到服务器：拉 + 推，「同步」才算双向。
                // 上次会话退出时还没过宽限期的 pending 靠这一步自愈。同步失败就不推——
                // 网络本来不通，再推一轮只会白失败并白涨动作的 attempts 计数。
                let report = core.sync_account(account_id)?;
                let flushed = core.flush_pending(account_id);
                Ok((report, flushed))
            },
            |this, result: Result<SyncOutcome, duck_mail::MailError>, window, cx| match result {
                Ok((report, flushed)) => {
                    let text = trf(
                        cx,
                        "同步完成：{} 个文件夹，新增 {} 封",
                        &[
                            &report.folders_synced.to_string(),
                            &report.new_messages.to_string(),
                        ],
                    );
                    this.status = text.clone().into();
                    notify::success(text, window, cx);
                    // 补发失败不推翻同步结论，但要让用户看见；成功条数不另行提示
                    //（40ms 轮询那条路径的到点补发被抢跑时会拿到 0，无需区分）。
                    if let Err(error) = flushed {
                        let text = trf(cx, "同步操作失败：{}", &[&error.to_string()]);
                        this.status = text.clone().into();
                        notify::error(text, window, cx);
                    }
                }
                Err(error) => {
                    // 失败原因已经由内核写进 `accounts.sync_error`，这里只说结论。
                    let text = trf(cx, "同步失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                }
            },
        );
    }

    /// 移除当前账户：走系统确认框（**不可撤销**——连同本地缓存邮件一起删）。
    ///
    /// 确认框是**模态**的（`duck-platform` 走原生对话框）：不能在 `render` 里调，
    /// 只能在点击回调里调——所以这个函数要 `&mut Context`，不能拿 `&App`。
    fn remove_active_account(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        let Some(account) = self.accounts.iter().find(|a| a.id == account_id) else {
            return;
        };
        let email = account.email.clone();
        let confirmed = duck_platform::confirm(
            tr(cx, "移除邮箱账户"),
            &trf(
                cx,
                "将移除 {}（含本地缓存的邮件与凭据）。这个动作无法撤销。",
                &[&email],
            ),
        );
        if !confirmed {
            return;
        }
        self.spawn(
            cx,
            move || core.remove_account(account_id),
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(tr(cx, "账户已移除"), window, cx);
                    this.active_account = None;
                    // 从右往左清：先清列表再清文件夹，避免中间帧出现「有文件夹无列表」。
                    this.messages.clear();
                    this.selected = None;
                    this.detail = None;
                    this.body = None;
                    this.folders.clear();
                    this.active_folder = None;
                    this.reload_accounts(cx);
                }
                Err(error) => {
                    let text = trf(cx, "移除账户失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 账户弹窗
    // ------------------------------------------------------------------

    /// 打开账户弹窗：`None` = 添加向导（两步）；`Some(account)` = 编辑该账户
    /// （直接进表单步，全部字段回填，见 [`AccountForm::for_edit`]）。
    fn open_account_dialog(
        &mut self,
        editing: Option<Account>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_edit = editing.is_some();
        let form = match &editing {
            Some(account) => cx.new(|cx| AccountForm::for_edit(account, window, cx)),
            None => cx.new(|cx| AccountForm::new(window, cx)),
        };
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title: SharedString = tr(
            cx,
            if is_edit {
                "编辑邮箱账户"
            } else {
                "添加邮箱账户"
            },
        )
        .into();

        // ⏎ 与底部「保存 / 添加」共用同一份保存判断（`ConfirmHandler` 口径，
        // 同 `RedisPage::open_form_dialog`）。第 1 步回车**不算**保存——
        // 卡片点击本身就是确认，回车落在选卡页时直接忽略（返回 `false`
        // 让弹窗留着）。
        let confirm: ConfirmHandler = {
            let form = form.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
                if form.read(cx).step == AccountStep::Provider {
                    return false;
                }
                view.update(cx, |this, cx| this.save_account(&form, window, cx))
                    .unwrap_or(true)
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let title = title.clone();
            let confirm = Rc::clone(&confirm);
            let form_for_content = form.clone();
            let drag_for_content = drag.clone();
            let offset = modal_offset(&drag, cx);

            dialog
                .w(px(ACCOUNT_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                // 量到高度前透明度是 0，挂上才能「先量高度再出现」而不是从顶部冒出。
                .opacity(offset.opacity)
                // 库默认右上角自画 ✕，会与 `modal_title_row` 叠成两个关闭按钮。
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, if is_edit { "保存" } else { "添加" }))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "mail-account-title",
                                &drag_for_content,
                                title.clone(),
                                cx,
                            ))
                            .child(render_account_form(&form_for_content, cx)),
                    ))
                })
                .on_ok(move |_, window, cx| confirm(window, cx))
                .footer(account_footer(&form, &view, cx))
        });
    }

    /// 保存账户：校验 → 落库 + 写凭据。返回 `false` 表示校验没过（弹窗留着）。
    fn save_account(
        &mut self,
        form: &Entity<AccountForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(core) = self.core.clone() else {
            notify::error(tr(cx, "邮箱数据库尚未打开"), window, cx);
            return false;
        };
        let new = match form.read(cx).to_new_account(cx) {
            Ok(new) => new,
            Err(error) => {
                // 校验失败把原因贴回弹窗正文，而不是关掉弹窗再弹一条通知。
                form.update(cx, |form, cx| {
                    form.error = Some(error);
                    cx.notify();
                });
                return false;
            }
        };

        let work_form = form.clone();
        let editing = form.read(cx).editing_account;
        self.spawn(
            cx,
            move || match editing {
                Some(id) => core.update_account(id, &new).map(|_| id),
                None => core.add_account(&new),
            },
            move |this, result, window, cx| match result {
                Ok(_id) => {
                    work_form.update(cx, |form, cx| {
                        form.error = None;
                        cx.notify();
                    });
                    if editing.is_some() {
                        // 编辑不改文件夹归属：保持当前选中账户，只重读清单。
                        notify::success(tr(cx, "账户已更新"), window, cx);
                        this.reload_accounts(cx);
                    } else {
                        notify::success(tr(cx, "账户已添加，点「同步」拉取邮件"), window, cx);
                        // 新账户还没有文件夹，先选中它（`reload_accounts` 会挑第一个）。
                        this.active_account = None;
                        this.reload_accounts(cx);
                    }
                    window.close_dialog(cx);
                }
                Err(error) => {
                    // 落库/凭据失败：原因可能是「该邮箱已存在」或保管箱不可写——
                    // 都贴在弹窗里，用户改一下就能重试。
                    let text: SharedString = trf(
                        cx,
                        if editing.is_some() {
                            "更新失败：{}"
                        } else {
                            "添加失败：{}"
                        },
                        &[&error.to_string()],
                    )
                    .into();
                    work_form.update(cx, |form, cx| {
                        form.error = Some(text);
                        cx.notify();
                    });
                }
            },
        );
        true
    }

    /// 测试弹窗里尚未保存的账户能否连上（真连一次 IMAP）。
    ///
    /// 结果写回弹窗正文（而不是弹一条通知）：这个按钮的用途是「边填边验」，
    /// 结论要停在用户正在看的字段旁边，改一个字段就能再试一次。
    fn test_account(
        &mut self,
        form: &Entity<AccountForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(core) = self.core.clone() else {
            notify::error(tr(cx, "邮箱数据库尚未打开"), window, cx);
            return;
        };
        let new = match form.read(cx).to_new_account(cx) {
            Ok(new) => new,
            Err(error) => {
                form.update(cx, |form, cx| {
                    form.error = Some(error);
                    cx.notify();
                });
                return;
            }
        };
        form.update(cx, |form, cx| {
            form.testing = true;
            form.test_result = None;
            form.error = None;
            cx.notify();
        });
        let work_form = form.clone();
        // 编辑态 + 口令留空 = 用已存口令测新服务器参数（`test_account_update`）。
        let editing = form.read(cx).editing_account;
        let testing_task = move || match editing {
            Some(id) => core.test_account_update(id, &new),
            None => core.test_connection(&new),
        };
        self.spawn(cx, testing_task, move |_this, result, _window, cx| {
            work_form.update(cx, |form, cx| {
                form.testing = false;
                form.test_result = Some(match result {
                    Ok(report) if report.ok => Ok(trf(
                        cx,
                        "连接成功，服务端报告 {} 个文件夹。",
                        &[&report.folder_count.to_string()],
                    )),
                    Ok(report) => Err(report
                        .error
                        .unwrap_or_else(|| tr(cx, "连接失败").to_owned())),
                    Err(error) => Err(error.to_string()),
                });
                cx.notify();
            });
        });
    }

    // ------------------------------------------------------------------
    // 撰写弹窗（写邮件 / 回信）
    // ------------------------------------------------------------------

    /// 打开「写邮件」弹窗（空草稿）。
    fn open_new_mail_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_compose_dialog(ComposeSeed::default(), window, cx);
    }

    /// 打开「回信」弹窗：从当前选中的邮件算预填。
    ///
    /// 预填在**点击时**算而不是 render 时算：引用正文要整段拼接原邮件正文，放在
    /// render 里就是每帧一次字符串复制（同 [`Self::render_body`] 那条注释的理由）。
    fn open_reply_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self.detail.clone() else {
            return;
        };
        let unknown = tr(cx, "(未知发件人)");
        let prefix = tr(cx, "回复：");
        let attribution = trf(
            cx,
            "在 {}，{} 写道：",
            &[
                &format_full_time(detail.date),
                sender_display(
                    detail.from_name.as_deref(),
                    detail.from_addr.as_deref(),
                    unknown,
                ),
            ],
        );
        self.open_compose_dialog(reply_seed(&detail, prefix, &attribution), window, cx);
    }

    /// 打开撰写弹窗（新写 / 回信）：这条邮件**还没有存成草稿**。
    fn open_compose_dialog(
        &mut self,
        seed: ComposeSeed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_compose_dialog_loaded(seed, LoadedCompose::default(), window, cx);
    }

    /// 打开撰写弹窗。
    ///
    /// 发件身份取自**当前账户**：本模块没有多身份 / 别名，所以这里没有身份选择器。
    /// 没有账户时直接拒绝并提示——一个连不上任何服务器的撰写框是骗人的。
    ///
    /// `loaded` 是「这条草稿已经存过」的那三样（行号 / 由来 / 已读好的附件）：新写与
    /// 回信走 [`LoadedCompose::default`]，从草稿箱点进来则是整条草稿。三种入口共用同一个
    /// 弹窗——差别只有初值与「存草稿」要改存哪一行，没必要分两个弹窗。
    fn open_compose_dialog_loaded(
        &mut self,
        seed: ComposeSeed,
        loaded: LoadedCompose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.active_account().cloned() else {
            notify::warning(tr(cx, "先添加一个邮箱账户再写信"), window, cx);
            return;
        };
        let sender = match account
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => Mailbox::with_name(account.email.clone(), name),
            None => Mailbox::new(account.email.clone()),
        };
        let account_id = account.id;
        // 回信还是新写，标题不一样；判据就是「有没有 `In-Reply-To`」——预填里只有
        // 这条是回信独有的。
        let title: SharedString = if seed.in_reply_to.is_some() {
            tr(cx, "回复").into()
        } else {
            tr(cx, "写邮件").into()
        };

        let form = cx.new(|cx| ComposeForm::new(window, cx, account_id, sender, seed, loaded));
        let view = self.view.clone();
        let drag = modal_drag(cx);

        // ⏎ 与底部「发送」共用同一份判断（`ConfirmHandler` 口径，同账户弹窗）。
        // 它在两种情况下都返回 `false`（= 弹窗留着）：校验没过，或者已经发出去了
        // 但结果还没回来。关闭由 `send_compose` 的成功回调负责。
        let confirm: ConfirmHandler = {
            let form = form.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |this, cx| this.send_compose(&form, window, cx))
                    .unwrap_or(false)
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let title = title.clone();
            let confirm = Rc::clone(&confirm);
            let form_for_content = form.clone();
            let view_for_content = view.clone();
            let drag_for_content = drag.clone();
            let offset = modal_offset(&drag, cx);

            dialog
                .w(px(COMPOSE_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "发送"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "mail-compose-title",
                                &drag_for_content,
                                title.clone(),
                                cx,
                            ))
                            .child(render_compose_form(
                                &form_for_content,
                                &view_for_content,
                                cx,
                            )),
                    ))
                })
                .on_ok(move |_, window, cx| confirm(window, cx))
                .footer(compose_footer(&form, &view, cx))
        });
    }

    /// 打开「搜索语法」速查弹窗。
    ///
    /// 只读，所以没有 `on_ok`：`DialogButtonProps::ok_handler()` 缺省就是「关掉」。
    /// 也没给 `footer` —— 默认底栏只渲染一个「知道了」，正好。
    fn open_search_help_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let drag_for_content = drag.clone();
            let offset = modal_offset(&drag_for_content, cx);

            dialog
                .w(px(SEARCH_HELP_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default().ok_text(tr(cx, "知道了")))
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "mail-search-help-title",
                                &drag_for_content,
                                tr(cx, "搜索语法"),
                                cx,
                            ))
                            .child(render_search_syntax(cx)),
                    ))
                })
        });
    }

    /// 发送：校验 → 后台 SMTP 直发。
    ///
    /// 返回 `false` **恒成立**，这不是笔误：发信是异步的（`MailCore::send` 内部走
    /// tokio 阻塞等结果），这一刻拿不到成败。真正的关闭在回调里，失败则把原因贴回
    /// 弹窗。`sending` 是重复提交的闸门——连按两下 ⏎ 只会发一封。
    fn send_compose(
        &mut self,
        form: &Entity<ComposeForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(core) = self.core.clone() else {
            notify::error(tr(cx, "邮箱数据库尚未打开"), window, cx);
            return false;
        };
        if form.read(cx).sending {
            return false;
        }
        // 先收成 `outcome` 再 match：直接在 `match form.read(cx)...` 里判会让
        // `read` 的临时借用活到整个 match 末尾，Err 分支里就用不了 `cx`。
        let outcome = {
            let state = form.read(cx);
            state.to_draft(cx)
        };
        let draft = match outcome {
            Ok(draft) => draft,
            Err(error) => {
                // 校验没过：原因贴回弹窗正文（同保存账户的口径），而不是弹一条通知。
                form.update(cx, |form, cx| {
                    form.error = Some(error);
                    cx.notify();
                });
                return false;
            }
        };
        let account_id = form.read(cx).account_id;
        form.update(cx, |form, cx| {
            form.sending = true;
            form.error = None;
            cx.notify();
        });
        let work_form = form.clone();
        let view = self.view.clone();
        // `spawn_local`：发信有自己的「发送中…」态，不必再点亮列表条上的忙碌态
        // （那会让旁边的同步按钮闪一下禁用，像页面在跑别的事）。
        self.spawn_local(
            cx,
            move || core.send(account_id, &draft),
            move |_this, result, window, cx| {
                work_form.update(cx, |form, cx| {
                    form.sending = false;
                    cx.notify();
                });
                match result {
                    Ok(()) => {
                        // 这封信是**从草稿发出去的** → 草稿不该再留着（同所有邮件客户端）。
                        // 交给 `delete_draft` 而不是在这儿自己拼一次写库：那条路已经把
                        // 「删掉 + 刷新列表 + 失败不连坐」都写好了。
                        if let Some(draft_id) = work_form.read(cx).draft_id {
                            let _ = view.update(cx, |this, cx| this.delete_draft(draft_id, cx));
                        }
                        notify::success(tr(cx, "邮件已发出"), window, cx);
                        window.close_dialog(cx);
                    }
                    Err(error) => {
                        let text: SharedString =
                            trf(cx, "发送失败：{}", &[&error.to_string()]).into();
                        work_form.update(cx, |form, cx| {
                            form.error = Some(text);
                            cx.notify();
                        });
                    }
                }
            },
        );
        false
    }

    /// 挑附件：原生多选对话框 → **读盘放后台** → 追加进撰写表单。
    ///
    /// ⚠️ 原生对话框是**模态阻塞**调用（`duck-platform/src/dialog.rs` 模块头写明「不要
    /// 在 render 里调用」），所以只能挂在动作处理器上。读盘则必须挪去后台：几十 MB 的
    /// 附件在渲染线程上读会把界面冻住。这条路径**没有任何网络往返**，所以用
    /// `spawn_local`（不点亮列表条上的同步按钮）。
    fn pick_attachments(&mut self, form: &Entity<ComposeForm>, cx: &mut Context<Self>) {
        // 用户取消 = 什么都不做，也不弹提示（同 `save_attachment`）。
        let Some(paths) = duck_platform::open_files() else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let work_form = form.clone();
        self.spawn_local(
            cx,
            move || {
                let mut read = Vec::new();
                let mut failure = None;
                for path in &paths {
                    match OutgoingAttachment::from_path(path) {
                        Ok(payload) => read.push(ComposeAttachment {
                            // 路径**当场**记下：存草稿只落 `draft_attachments.file_path`，
                            // 等真去存的时候再回头问系统，文件可能已经被挪走了。
                            source: path.clone(),
                            payload,
                        }),
                        // 只记**第一个**失败原因，且**不丢弃**其余读成功的文件——
                        // 一次挑了五个、其中一个刚被删掉，不该整批退回。
                        Err(error) => {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                    }
                }
                (read, failure)
            },
            move |_this, (read, failure), window, cx| {
                let added = work_form.update(cx, |form, cx| {
                    let before = form.attachments.len();
                    for attachment in read {
                        // 同名只留一个：清单上两行一模一样的文件名分不清谁是谁，而
                        // 「同一封邮件挂两份同名文件」在实践里只能是误操作。
                        if !form.attachments.iter().any(|existing| {
                            existing.payload.filename == attachment.payload.filename
                        }) {
                            form.attachments.push(attachment);
                        }
                    }
                    let added = form.attachments.len() - before;
                    if added > 0 {
                        cx.notify();
                    }
                    added
                });
                if let Some(reason) = failure {
                    notify::error(trf(cx, "有附件读不出来：{}", &[&reason]), window, cx);
                } else if added == 0 {
                    notify::info(tr(cx, "这些文件都已经在附件里了"), window, cx);
                }
            },
        );
    }

    /// 打开一条草稿：读库 → 逐份重读附件 → 用整条草稿开撰写弹窗。
    ///
    /// ⚠️ 附件在库里**只存路径**（`draft_attachments` 表结构给的就是 `file_path`，没有
    /// 字节列），所以这里必须**重新读盘**。读不出来的那些**不阻断**打开——草稿本身
    /// 还好好的，只是少一个附件；丢了几条会明说，不能让用户以为附件还在。
    fn open_draft(&mut self, draft_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        self.spawn_local(
            cx,
            move || -> Result<LoadedDraft, duck_mail::MailError> {
                let record = core.load_draft(account_id, draft_id)?;
                let mut attachments = Vec::new();
                let mut missing = 0usize;
                for reference in &record.attachments {
                    match OutgoingAttachment::from_path(Path::new(&reference.file_path)) {
                        Ok(payload) => attachments.push(ComposeAttachment {
                            source: PathBuf::from(&reference.file_path),
                            payload,
                        }),
                        Err(_) => missing += 1,
                    }
                }
                Ok(LoadedDraft {
                    seed: ComposeSeed {
                        to: record.draft.to.clone(),
                        cc: record.draft.cc.clone(),
                        subject: record.draft.subject.clone(),
                        body: record.draft.body.clone(),
                        in_reply_to: record.draft.in_reply_to.clone(),
                        references: record.draft.references.clone(),
                    },
                    draft_id: record.id,
                    mode: record.mode,
                    attachments,
                    missing,
                })
            },
            move |this, result, window, cx| match result {
                Ok(loaded) => {
                    if loaded.missing > 0 {
                        notify::warning(
                            trf(
                                cx,
                                "有 {} 个附件读不出来，已从草稿里略去。",
                                &[&loaded.missing.to_string()],
                            ),
                            window,
                            cx,
                        );
                    }
                    this.open_compose_dialog_loaded(
                        loaded.seed,
                        LoadedCompose {
                            draft_id: loaded.draft_id,
                            mode: loaded.mode,
                            attachments: loaded.attachments,
                        },
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    this.status = trf(cx, "打开草稿失败：{}", &[&error.to_string()]).into();
                    cx.notify();
                }
            },
        );
    }

    /// 存草稿：**不校验收件人**。
    ///
    /// 草稿的全部意义就是「还没写完」——要求先填收件人才能存，等于把最该被存下来的
    /// 那半途而废挡在门外（`duck_mail` 那边的 `save` 也不校验地址语法）。
    ///
    /// 存完**不关弹窗**：存草稿是幂等的、可反复做的动作，关掉反而逼着用户再打开一次
    /// 才能接着写。`ComposeForm::draft_id` 会记下行号，所以再点一次是**改存同一行**。
    fn save_draft(
        &mut self,
        form: &Entity<ComposeForm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(core) = self.core.clone() else {
            notify::error(tr(cx, "邮箱数据库尚未打开"), window, cx);
            return;
        };
        if form.read(cx).saving_draft {
            return;
        }
        let record = form.read(cx).to_draft_record(cx);
        let work_form = form.clone();
        let view = self.view.clone();
        form.update(cx, |form, cx| {
            form.saving_draft = true;
            form.error = None;
            cx.notify();
        });
        self.spawn_local(
            cx,
            move || core.save_draft(&record),
            move |_this, result, window, cx| {
                work_form.update(cx, |form, cx| {
                    form.saving_draft = false;
                    cx.notify();
                });
                match result {
                    Ok(id) => {
                        work_form.update(cx, |form, cx| {
                            form.draft_id = Some(id);
                            cx.notify();
                        });
                        notify::success(tr(cx, "草稿已保存"), window, cx);
                        // 列表若正停在草稿箱，这一条要立刻冒出来。
                        let _ = view.update(cx, |this, cx| this.reload_drafts(cx));
                    }
                    Err(error) => {
                        let text: SharedString =
                            trf(cx, "保存草稿失败：{}", &[&error.to_string()]).into();
                        work_form.update(cx, |form, cx| {
                            form.error = Some(text);
                            cx.notify();
                        });
                    }
                }
            },
        );
    }

    /// 删一条草稿（**只删本地行**）。
    ///
    /// 不走动作队列：队列的语义是「先本地生效、稍后补发远端」，而草稿在服务器上**没有
    /// 副本**（`drafts_meta.remote_uid` 还是空的），没有什么可补发的。
    fn delete_draft(&mut self, draft_id: i64, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        self.spawn_local(
            cx,
            move || core.delete_draft(account_id, draft_id),
            move |this, result, _window, cx| match result {
                Ok(true) => {
                    this.drafts.retain(|draft| draft.id != draft_id);
                    // 角标与列表同源：这一条走的是「本地改完就收」，没重拉列表，所以
                    // 角标得跟着减（`reload_draft_count` 只在换账户 / 换文件夹时跑）。
                    this.local_drafts = (this.local_drafts - 1).max(0);
                    this.status = tr(cx, "草稿已删除").into();
                }
                // `false` = 这行已经没了（不该发生，但另一个窗口删掉就是这种结果）。
                // 不是错误，重拉一次把列表对齐。
                Ok(false) => this.reload_drafts(cx),
                Err(error) => {
                    this.status = trf(cx, "删除草稿失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 把一个附件另存到用户选的位置。
    ///
    /// ⚠️ 原生「另存为」是**模态阻塞**调用（`duck-platform/src/dialog.rs` 模块头写明了
    /// 「不要在 render 里调用」），所以整条路径只能挂在动作处理器上。
    ///
    /// 走 `spawn`（点亮 `busy`）而不是 `spawn_local`：没有本地副本的这一次会**真的连
    /// 服务器**（整封取回 + MIME 解码，见 `duck_mail::sync::download_attachment`），
    /// 那是网络往返，列表条上的同步按钮该跟着禁用。
    fn save_attachment(&mut self, attachment_id: i64, filename: &str, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        // 用户取消 = 什么都不做，**也不弹提示**。点了取消还被告知「已取消」是噪音。
        let Some(dest) = duck_platform::save_file(filename) else {
            return;
        };
        // 落点选定后才进入「下载中」态：原生对话框阻塞期间用户还没确认要存。
        self.saving_attachment = Some(attachment_id);
        self.attachment_error = None;
        self.spawn(
            cx,
            move || core.download_attachment(attachment_id, &dest),
            move |this, result, window, cx| {
                // 只清掉**这一条**附件的高亮：同一封邮件里连点两个附件时，先回来的那个
                // 不该把后一个还在下载的态一起抹掉。
                if this.saving_attachment == Some(attachment_id) {
                    this.saving_attachment = None;
                }
                match result {
                    Ok(saved) => {
                        this.attachment_error = None;
                        let path = saved.path.display().to_string();
                        // 记下用户选的落点，附件行据此多出一个「在访达中显示」。
                        // 同一个附件重复另存时以最后一次为准（去重后追加，不涨行数）。
                        this.saved_attachments
                            .retain(|(id, _)| *id != attachment_id);
                        this.saved_attachments
                            .push((attachment_id, saved.path.clone()));
                        notify::success(trf(cx, "已保存到 {}", &[&path]), window, cx);
                    }
                    Err(error) => {
                        // 失败态停在那一行（再点一次即重试），同时给一条可读的提示。
                        let reason: SharedString =
                            trf(cx, "保存附件失败：{}", &[&error.to_string()]).into();
                        this.attachment_error = Some((attachment_id, reason.clone()));
                        notify::error(reason, window, cx);
                    }
                }
            },
        );
    }
}

impl Drop for MailPage {
    /// 页面销毁（关窗 / 退出）时停掉本页内核拉起的常驻 IDLE actor。
    ///
    /// 协作式 stop（`AtomicBool` + `Notify`）之后 `abort` 兜底，保证不留悬挂任务。
    /// `MailCore` 的 runtime 析构也会再兜底停一次（见 `duck_mail::runtime`），两处都幂等。
    fn drop(&mut self) {
        if let Some(core) = self.core.as_ref() {
            core.stop_all_sync_actors();
        }
    }
}

// ---------------------------------------------------------------------------
// 账户弹窗的视图状态
// ---------------------------------------------------------------------------

/// 「服务商」选择卡里的哨兵值（其余取值是 `duck_mail::presets` 的 `preset.id`）。
///
/// 用 **`__` 前缀**而不是 `""`：`id` 来自 `presets` 表（纯 ASCII 小写字母数字），
/// 前缀保证永不撞车 —— 撞车的表现是「选了自定义却回填了服务器」。
const PROVIDER_CUSTOM: &str = "__custom";

/// 服务商选择卡里的品牌 logo 边长（对齐 `db.rs` 选择卡的 `PICKER_LOGO_SIZE`）。
const PROVIDER_LOGO_SIZE: f32 = 24.;

/// 「添加邮箱账户」弹窗的两步向导步骤。
///
/// 第 1 步选服务商（带 logo 的卡片网格），第 2 步填账户信息——服务器与加密档
/// 已由所选服务商的模板填好，仍可手改。分两步是把「你是谁家的邮箱」与「你是谁」
/// 拆开：第一问点一张卡就答完，第二问只剩用户真正需要关心的四五个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountStep {
    /// 第 1 步：选择服务商。
    Provider,
    /// 第 2 步：账户信息（服务器已按模板预填）。
    Details,
}

/// 「添加邮箱账户」弹窗的表单状态。
///
/// 挂成 [`Entity`] 而不是页面字段：`open_dialog` 的内容闭包是 `Fn`（重排会重跑），
/// 拿不到 `&mut MailPage`，但可以克隆 `Entity` 自由读写（同 `redis.rs::EditForm`）。
struct AccountForm {
    email: Entity<InputState>,
    display_name: Entity<InputState>,
    username: Entity<InputState>,
    imap_host: Entity<InputState>,
    imap_port: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    password: Entity<InputState>,
    /// 连接加密方式。
    security: ConnectionSecurity,
    /// 「服务商」的当前取值：`PROVIDER_CUSTOM` 或 `duck_mail::presets` 的 `preset.id`。
    ///
    /// 两步向导里它由第 1 步的卡片点击决定，不再有「自动识别」档——
    /// 选了哪张卡，第 2 步的服务器 / 端口 / 加密档就按谁回填。
    provider: String,
    /// 向导当前步骤。
    step: AccountStep,
    /// 编辑态：`Some(id)` = 正在编辑这个账户（口令留空 = 保持现有口令）；
    /// `None` = 添加向导。
    editing_account: Option<i64>,
    /// 正在测连接。
    testing: bool,
    /// 最近一次测连接的结果。
    test_result: Option<Result<String, String>>,
    /// 保存失败的原因（贴在弹窗正文里）。
    error: Option<SharedString>,
}

impl AccountForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            email: text_input(window, cx, "you@example.com"),
            // 显示名是可选项：留空时界面用邮箱地址当账户名。
            display_name: text_input(window, cx, "工作邮箱"),
            username: text_input(window, cx, "登录名（多为邮箱地址）"),
            imap_host: text_input(window, cx, "imap.example.com"),
            imap_port: text_input(window, cx, "993"),
            smtp_host: text_input(window, cx, "smtp.example.com"),
            smtp_port: text_input(window, cx, "465"),
            // 口令走掩码显示：掩码只是显示层，`value()` 拿到的仍是明文。
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("口令 / 应用专用密码")
                    .masked(true)
            }),
            security: ConnectionSecurity::default(),
            provider: PROVIDER_CUSTOM.to_owned(),
            step: AccountStep::Provider,
            editing_account: None,
            testing: false,
            test_result: None,
            error: None,
        }
    }

    /// 编辑模式的表单：全部字段按档案回填；口令**留空**（保持现有口令，
    /// 见 `to_new_account` 与内核 `accounts::update` 的口径）。
    ///
    /// 服务商用 [`presets::preset_for_email`] 按邮箱域名识别：认得出就带上模板
    /// （与添加向导第 1 步等价），认不出落「其他邮箱」。直接进第 2 步。
    fn for_edit(account: &Account, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut form = Self::new(window, cx);
        let set = |field: &Entity<InputState>,
                   value: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            field.update(cx, |state, cx| state.set_value(value, window, cx));
        };
        set(&form.email, account.email.clone(), window, cx);
        if let Some(name) = account.display_name.as_deref() {
            set(&form.display_name, name.to_owned(), window, cx);
        }
        set(&form.username, account.username.clone(), window, cx);
        set(&form.imap_host, account.imap_host.clone(), window, cx);
        set(&form.imap_port, account.imap_port.to_string(), window, cx);
        set(&form.smtp_host, account.smtp_host.clone(), window, cx);
        set(&form.smtp_port, account.smtp_port.to_string(), window, cx);
        form.password.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
            state.set_placeholder(tr(cx, "口令 / 应用专用密码（留空保持不变）"), window, cx);
        });
        form.security = account.security;
        form.provider = presets::preset_for_email(&account.email)
            .map(|preset| preset.id.to_owned())
            .unwrap_or_else(|| PROVIDER_CUSTOM.to_owned());
        form.editing_account = Some(account.id);
        form.step = AccountStep::Details;
        form
    }

    /// 点了第 1 步的某张服务商卡片。
    ///
    /// 这是「内置模板」的落点：命中 `duck_mail::presets` 的卡片直接把 IMAP /
    /// SMTP 主机、端口、加密方式一次填好（`presets::ProviderPreset`，零网络往返）；
    /// 「其他邮箱」卡什么都不动，第 2 步全部手填。
    ///
    /// 登录名只在**空着**时才回填：表里这些服务商都不接受裸用户名。
    /// 字段仍可手改——模板给的是「最可能对」的值，不是唯一值。
    fn choose_provider(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.provider = id.to_owned();
        // 换了服务商，上一次的测试结果就不再说明这一套服务器，一并作废。
        self.test_result = None;
        if let Some(preset) = presets::preset_by_id(id) {
            let set =
                |field: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App| {
                    field.update(cx, |state, cx| state.set_value(value, window, cx));
                };
            set(&self.imap_host, preset.imap_host.to_owned(), window, cx);
            set(&self.imap_port, preset.imap_port.to_string(), window, cx);
            set(&self.smtp_host, preset.smtp_host.to_owned(), window, cx);
            set(&self.smtp_port, preset.smtp_port.to_string(), window, cx);
            self.security = preset.security;
            if preset.login_is_email && self.username.read(cx).value().trim().is_empty() {
                let email = self.email.read(cx).value().trim().to_owned();
                if !email.is_empty() {
                    set(&self.username, email, window, cx);
                }
            }
        }
        self.step = AccountStep::Details;
        cx.notify();
    }

    /// 第 2 步点「更换」回到第 1 步重选服务商。
    ///
    /// 已填的邮箱地址、口令等字段**一律保留**：换服务商只是换一套服务器参数，
    /// 没理由让用户把刚输完的口令再敲一遍。预填值会按新选的卡直接覆盖。
    fn back_to_provider(&mut self, cx: &mut Context<Self>) {
        self.step = AccountStep::Provider;
        cx.notify();
    }

    /// 从表单取值，拼成 [`NewAccount`]。
    ///
    /// 这里的校验只做「填没填、格式对不对」——**能不能连上由内核的
    /// `accounts::validate` + 真连接决定**，不在这里重复一份规则。
    fn to_new_account(&self, cx: &App) -> Result<NewAccount, SharedString> {
        let read = |field: &Entity<InputState>| field.read(cx).value().trim().to_owned();
        let email = read(&self.email);
        if !email.contains('@') {
            return Err(tr(cx, "请填写邮箱地址（需含 @）。").into());
        }
        let imap_host = read(&self.imap_host);
        if imap_host.is_empty() {
            return Err(tr(cx, "请填写 IMAP 服务器地址。").into());
        }
        let smtp_host = read(&self.smtp_host);
        if smtp_host.is_empty() {
            return Err(tr(cx, "请填写 SMTP 服务器地址。").into());
        }
        let imap_port = parse_port(&read(&self.imap_port))
            .ok_or_else(|| SharedString::from(tr(cx, "IMAP 端口需是 1–65535。")))?;
        let smtp_port = parse_port(&read(&self.smtp_port))
            .ok_or_else(|| SharedString::from(tr(cx, "SMTP 端口需是 1–65535。")))?;
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() {
            // 编辑态允许空口令：语义是「保持现有口令」（内核 `accounts::update`）。
            // 添加向导没有「现有」，必须填。
            if self.editing_account.is_none() {
                return Err(tr(cx, "请填写口令。").into());
            }
        }
        let display_name = read(&self.display_name);
        // 登录名留空就用邮箱地址——绝大多数服务商就是这个口径。
        let username = {
            let typed = read(&self.username);
            if typed.is_empty() {
                email.clone()
            } else {
                typed
            }
        };
        Ok(NewAccount {
            email,
            display_name: (!display_name.is_empty()).then_some(display_name),
            username,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            security: self.security,
            password,
        })
    }
}

/// 端口文本 → `u16`；非法返回 `None`。
fn parse_port(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|port| *port > 0)
}

fn text_input<C: AppContext>(
    window: &mut Window,
    cx: &mut C,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 弹窗正文：按向导步骤分发到「选服务商」或「账户信息」。
fn render_account_form(form: &Entity<AccountForm>, cx: &App) -> AnyElement {
    match form.read(cx).step {
        AccountStep::Provider => render_provider_picker(form, cx),
        AccountStep::Details => render_account_details(form, cx),
    }
}

/// 第 1 步：选择服务商（带品牌 logo 的卡片网格，样式对齐 `db.rs` 的引擎选择卡）。
///
/// 一张卡 = logo + 品牌名 + 归属域名提示。点击即选定并进入第 2 步——
/// 没有单独的「下一步」按钮：卡片本身既是选择又是确认，少一次无意义的往返。
/// 最后一张「其他邮箱」卡对应 `PROVIDER_CUSTOM`：服务器与端口全部手填。
fn render_provider_picker(form: &Entity<AccountForm>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let hover_bg = extended.colors.hover_tint;
    let selected_border = tokens.colors.accent;
    let current = form.read(cx).provider.clone();

    // 网格内容：先 7 家预设，再「其他邮箱」收尾。
    let mut cards: Vec<(String, MailLogo, SharedString, SharedString)> = presets::PRESETS
        .iter()
        .map(|preset| {
            (
                preset.id.to_owned(),
                provider_logo(preset.id),
                SharedString::from(preset.label),
                SharedString::from(preset.domains.join(" / ")),
            )
        })
        .collect();
    cards.push((
        PROVIDER_CUSTOM.to_owned(),
        MailLogo::Custom,
        tr(cx, "其他邮箱").into(),
        tr(cx, "手动填写服务器与端口").into(),
    ));

    let form_for_click = form.clone();
    let mut grid = div().grid().grid_cols(2).gap(tokens.spacing.xs).w_full();
    for (id, logo, name, hint) in cards {
        let is_current = id == current;
        let click_form = form_for_click.clone();
        let click_id = id.clone();
        grid = grid.child(
            h_flex()
                .id(SharedString::from(format!("mail-provider-{id}")))
                .min_w_0()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.sm)
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(if is_current {
                    selected_border
                } else {
                    tokens.colors.border
                })
                .bg(tokens.colors.background)
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .child(logo.element(px(PROVIDER_LOGO_SIZE)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_size(tokens.typography.sm.size)
                                .font_weight(FontWeight::MEDIUM)
                                .child(name),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(hint),
                        ),
                )
                .on_click(move |_, window, cx| {
                    click_form.update(cx, |form, cx| {
                        form.choose_provider(&click_id, window, cx);
                    });
                }),
        );
    }

    v_flex()
        .w_full()
        .gap(form_gap(cx))
        .child(caption_heading(tr(cx, "选择邮箱服务商"), cx))
        .child(meta_text(
            tr(
                cx,
                "选一家服务商会自动填好收发服务器与加密方式，下一步仍可修改。",
            ),
            cx,
        ))
        .child(grid)
        .into_any_element()
}

/// 服务商 `preset.id` → 品牌 logo。`PROVIDER_CUSTOM` 与未知 id 落到通用信封图。
fn provider_logo(id: &str) -> MailLogo {
    match id {
        "gmail" => MailLogo::Gmail,
        "outlook" => MailLogo::Outlook,
        "qq" => MailLogo::Qq,
        "netease163" => MailLogo::Netease163,
        "netease126" => MailLogo::Netease126,
        "yahoo" => MailLogo::Yahoo,
        "icloud" => MailLogo::Icloud,
        _ => MailLogo::Custom,
    }
}

/// 第 2 步：账户身份 + 口令 + IMAP / SMTP 两栏 + 加密方式。
///
/// 顶部是「已选服务商」条：品牌 logo + 名称 + 「更换」——回到第 1 步重选。
/// 服务器与端口已按所选模板预填，仍是普通输入框，随时可改。
///
/// 输入框一律 `.control(cx)`（`duck_components::ControlExt`）：它把高度收到
/// `density.control`、字号收到 `typography.sm`，与全仓表单输入同规格。裸
/// `Input::new()` 会带库自己的尺寸与配色，在同一张弹窗里一眼看出是两套东西。
fn render_account_details(form: &Entity<AccountForm>, cx: &App) -> AnyElement {
    let state = form.read(cx);

    // ── 已选服务商条 ──
    let provider_id = state.provider.clone();
    let (provider_name, provider_logo) = match presets::preset_by_id(&provider_id) {
        Some(preset) => (
            SharedString::from(preset.label),
            provider_logo(&provider_id),
        ),
        None => (tr(cx, "其他邮箱").into(), MailLogo::Custom),
    };
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let back_form = form.clone();
    let provider_bar = h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.sm)
        .px(tokens.spacing.sm)
        .py(tokens.spacing.sm)
        .rounded(tokens.radius.md)
        .border_1()
        .border_color(tokens.colors.border)
        .bg(tokens.colors.background)
        .child(provider_logo.element(px(PROVIDER_LOGO_SIZE)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .font_weight(FontWeight::MEDIUM)
                .child(provider_name),
        )
        .child(
            icon_text_button(
                "mail-account-change-provider",
                tr(cx, "更换"),
                Icon::new(DuckIcon::ChevronLeft)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Link,
                cx,
            )
            .on_click(move |_, _, cx| {
                back_form.update(cx, |form, cx| form.back_to_provider(cx));
            }),
        );

    let mut body = v_flex()
        .w_full()
        .gap(form_gap(cx))
        .child(provider_bar)
        .child(form_row(
            [
                form_field(
                    tr(cx, "邮箱地址"),
                    Input::new(&state.email).control(cx).w_full(),
                    None,
                    Some(SharedString::from(tr(
                        cx,
                        "用于在账户列表里区分，也是默认登录名。",
                    ))),
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "显示名"),
                    Input::new(&state.display_name).control(cx).w_full(),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_field(
            tr(cx, "登录名"),
            Input::new(&state.username).control(cx).w_full(),
            None,
            Some(SharedString::from(tr(cx, "留空则用邮箱地址。"))),
            cx,
        ))
        .child(form_field(
            tr(cx, "口令"),
            // `mask_toggle` 自带「显示 / 隐藏」小眼睛：口令大多是一串随机字符，
            // 输入时看不见自己敲了什么，是密码框最高频的补救需求。
            Input::new(&state.password)
                .control(cx)
                .w_full()
                .mask_toggle(),
            None,
            Some(SharedString::from(tr(
                cx,
                "存进应用自己的凭据文件（仅本用户可读），不进数据库，也不走系统钥匙串。",
            ))),
            cx,
        ))
        .child(form_row(
            [
                form_field(
                    tr(cx, "IMAP 服务器"),
                    Input::new(&state.imap_host).control(cx).w_full(),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "端口"),
                    Input::new(&state.imap_port)
                        .control(cx)
                        .w(px(PORT_FIELD_WIDTH)),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_row(
            [
                form_field(
                    tr(cx, "SMTP 服务器"),
                    Input::new(&state.smtp_host).control(cx).w_full(),
                    None,
                    // M1 不发信，但 SMTP 地址是账户档案的一部分，先收着。
                    Some(SharedString::from(tr(
                        cx,
                        "M2 发信会用；现在只随档案保存。",
                    ))),
                    cx,
                )
                .into_any_element(),
                form_field(
                    tr(cx, "端口"),
                    Input::new(&state.smtp_port)
                        .control(cx)
                        .w(px(PORT_FIELD_WIDTH)),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        ))
        .child(security_picker(state, form, cx));

    if let Some(result) = &state.test_result {
        body = body.child(match result {
            // 测试结论要有色彩语义：成功绿 / 失败红，别落回中性说明灰。
            Ok(text) => field_success(text.clone(), cx),
            Err(text) => field_error(trf(cx, "连接失败：{}", &[text]), cx),
        });
    }
    if let Some(error) = &state.error {
        body = body.child(duck_components::field_error(error.clone(), cx));
    }
    body.into_any_element()
}

/// 加密方式三选一。
///
/// 用 `segmented_tabs` 而不是下拉：只有三个选项、且**选错是安全问题**（明文会把口令
/// 发给服务器），摊开比收起来更该看得见。分段控件本身对读屏是一组页签（`Role::Tab`
/// + `aria_selected`，见 `kit::segmented_tabs`），选中态读得出来。
///
/// 明文档的后果写在控件**下方**的说明行里，并且只在选中它时出现——把「会泄露口令」
/// 常驻在推荐档旁边，会让用户以为这是三个平级的正常选项。
fn security_picker(state: &AccountForm, form: &Entity<AccountForm>, cx: &App) -> AnyElement {
    /// `(取值, 标签, 危险档?)`。顺序即界面顺序：安全的在前。
    const OPTIONS: [(ConnectionSecurity, &str, bool); 3] = [
        (ConnectionSecurity::Tls, "TLS（993 / 465）", false),
        (ConnectionSecurity::StartTls, "STARTTLS（143 / 587）", false),
        (ConnectionSecurity::Plain, "明文（仅本地测试）", true),
    ];

    let view = form.clone();
    let labels: Vec<SharedString> = OPTIONS
        .iter()
        .map(|(_, label, _)| tr(cx, label).into())
        .collect();
    let selected = OPTIONS
        .iter()
        .position(|(mode, _, _)| *mode == state.security)
        .unwrap_or(0);
    let is_plain = OPTIONS[selected].2;

    let control = segmented_tabs(
        "mail-account-security",
        labels,
        selected,
        move |index, _, cx| {
            let Some((mode, _, _)) = OPTIONS.get(index).copied() else {
                return;
            };
            view.update(cx, |form, cx| {
                form.security = mode;
                // 换了加密方式，上一次的测试结果就过期了。
                form.test_result = None;
                cx.notify();
            });
        },
        cx,
    );

    let mut field = form_field(
        tr(cx, "加密方式"),
        control,
        None,
        Some(SharedString::from(tr(
            cx,
            "默认选 TLS；STARTTLS 用于 143 / 587。",
        ))),
        cx,
    );
    if is_plain {
        field = field.child(duck_components::field_error(
            tr(
                cx,
                "口令会以明文发给服务器，任何人都能在链路上读到；只用于本机测试服务器。",
            ),
            cx,
        ));
    }
    field.into_any_element()
}

/// 弹窗脚注：按向导步骤换按钮，全部带前置图标（`icon_text_button`）。
///
/// 第 1 步只有「取消」——选卡即进入下一步，无需「下一步」按钮。
/// 第 2 步左「测试连接」，右「上一步 / 取消 / 添加」。
fn account_footer(
    form: &Entity<AccountForm>,
    view: &WeakEntity<MailPage>,
    cx: &App,
) -> DialogFooter {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let step = form.read(cx).step;
    let testing = form.read(cx).testing;
    // 编辑态的底部按钮换文案，且没有「上一步」（回填进来的，不回选卡页重走模板；
    // 用户真想换服务商模板可以改字段，或删了重加）。
    let is_edit = form.read(cx).editing_account.is_some();

    let mut right = h_flex().gap(tokens.spacing.sm);
    if step == AccountStep::Provider {
        right = right.child(
            icon_text_button(
                "mail-account-cancel",
                tr(cx, "取消"),
                Icon::new(DuckIcon::X)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx)),
        );
        return DialogFooter::new().justify_between().child(right);
    }

    // ── 第 2 步 ──
    let test_view = view.clone();
    let test_form = form.clone();
    let ok_view = view.clone();
    let ok_form = form.clone();
    let back_form = form.clone();
    let test_label = if testing {
        tr(cx, "正在测试…")
    } else {
        tr(cx, "测试连接")
    };

    let left = h_flex().gap(tokens.spacing.sm).child(
        icon_text_button(
            "mail-account-test",
            test_label,
            Icon::new(DuckIcon::Plug)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            ButtonVariant::Secondary,
            cx,
        )
        .disabled(testing)
        .on_click(move |_, window, cx| {
            let _ = test_view.update(cx, |this, cx| this.test_account(&test_form, window, cx));
        }),
    );

    right = right
        .when(!is_edit, |right| {
            right.child(
                icon_text_button(
                    "mail-account-back",
                    tr(cx, "上一步"),
                    Icon::new(DuckIcon::ChevronLeft)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    back_form.update(cx, |form, cx| form.back_to_provider(cx));
                }),
            )
        })
        .child(
            icon_text_button(
                "mail-account-cancel",
                tr(cx, "取消"),
                Icon::new(DuckIcon::X)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
            icon_text_button(
                "mail-account-ok",
                tr(cx, if is_edit { "保存" } else { "添加" }),
                Icon::new(DuckIcon::Check).size(extended.icon.sm),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let saved = ok_view
                    .update(cx, |this, cx| this.save_account(&ok_form, window, cx))
                    .unwrap_or(true);
                if saved {
                    window.close_dialog(cx);
                }
            }),
        );

    DialogFooter::new()
        .justify_between()
        .child(left)
        .child(right)
}

// ---------------------------------------------------------------------------
// 撰写弹窗（写邮件 / 回信）
// ---------------------------------------------------------------------------
//
// 这一节的纯函数（[`parse_mailbox`] / [`parse_mailbox_list`] / [`quote_reply`] /
// [`reply_body`] / [`with_reply_prefix`] / [`reply_seed`]）刻意**不碰 UI**：它们是
// 「界面上的字符串 ↔ 内核的 `Draft`」之间的全部转换，也正是最容易写错的地方
// （地址被 `,` 切碎、主题被前缀两次、引用漏掉空行），所以在下面有对应的测试。
//
// 需要 `&App` 的只有两处文案（`回复：` 与归属行），它们由调用方用 `tr` / `trf`
// 算好再传进来——这样纯函数本身不需要 GPUI 上下文，测试里直接调用即可。

/// 撰写弹窗的预填内容。`default()` = 空草稿（写新邮件）。
///
/// 与 [`Draft`] 分开：`Draft` 里的 `date_unix` / `message_id` 要等发信时才由
/// `Draft::stamp` 补，而 `from` 取自账户档案（不在预填里）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ComposeSeed {
    /// 收件人（回信时 = 原发件人）。
    to: Vec<Mailbox>,
    /// 抄送。回信**不回全体**，所以这里恒为空——没有「回复全部」入口，也就没有
    /// 需要预填的场合。
    cc: Vec<Mailbox>,
    /// 主题（回信时已带前缀）。
    subject: String,
    /// 正文（回信时已带归属行与引用块）。
    body: String,
    /// 原邮件的 `Message-ID`（进 `In-Reply-To`）。
    in_reply_to: Option<String>,
    /// 引用链（进 `References`）。
    references: Vec<String>,
}

/// 撰写表单里的一个附件：**字节** + **源路径**配对。
///
/// 内核的 `Draft.attachments` 只有字节，而 `draft_attachments` 表只有路径——两边各要
/// 一半，所以界面层把它们配对存着。字节在**选文件那一刻**就读好了（见
/// [`MailPage::pick_attachments`]），路径是同一时刻顺手记下的，不是等存草稿再去问系统：
/// 那时候文件可能已经不在原处了。
struct ComposeAttachment {
    /// 用户当初挑的那个文件（存草稿时落 `draft_attachments.file_path`）。
    source: PathBuf,
    /// 已读进内存的载荷（点「发送」时直接编码，不再读盘）。
    payload: OutgoingAttachment,
}

/// 「这条草稿已经存过」的那几样——开撰写弹窗时与预填内容分开传。
///
/// 新写与回信走 [`LoadedCompose::default`]（`draft_id` 为 `None` = 点「存草稿」新建一行）。
#[derive(Default)]
struct LoadedCompose {
    /// 已存草稿的本地行号。`Some` = 点「存草稿」改存那一行。
    draft_id: Option<i64>,
    /// 草稿由来（落 `drafts_meta.mode`）。
    mode: DraftMode,
    /// 已读好的附件（连同源路径）。
    attachments: Vec<ComposeAttachment>,
}

/// [`MailPage::open_draft`] 在后台攒出来的东西：预填内容 + 已存草稿的元信息 +
/// **读不出来的附件条数**（那个数要报给用户，所以不能只留在后台闭包里）。
struct LoadedDraft {
    /// 表单初值。
    seed: ComposeSeed,
    /// 已存草稿的行号。
    draft_id: Option<i64>,
    /// 草稿由来。
    mode: DraftMode,
    /// 读成功的附件。
    attachments: Vec<ComposeAttachment>,
    /// 路径还在、文件读不出来的附件条数。
    missing: usize,
}

/// `Mailbox` 在**输入框里**的写法：`显示名 <地址>`，没有显示名就只剩地址。
///
/// 刻意**不做** RFC 2047 编码：这里是给人编辑的输入框，编码是 `compose` 装配
/// 头部时才该做的事（`Mailbox::header_value` 干那个）。反过来，[`parse_mailbox`]
/// 认这个写法，所以「预填 → 用户改一改 → 发出去」能原样往返。
fn mailbox_field_text(mailbox: &Mailbox) -> String {
    match mailbox
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => format!("{name} <{}>", mailbox.address),
        None => mailbox.address.clone(),
    }
}

/// 输入框里的一段文本 → 一个 [`Mailbox`]；空串 / 只有尖括号 → `None`。
///
/// 两种写法都认：
///
/// - `张三 <a@b.com>`（也容忍 `"张三" <a@b.com>`，反引号外的引号是通讯录里常见的）
/// - `a@b.com`
///
/// 取**最后一个** `<`：显示名里带尖括号（`A <test> B <a@b.com>`）时，地址在最后一对里。
/// 这里不校验收件人格式——那是 [`MailPage::send_compose`] 的事，一处规则不写两遍。
fn parse_mailbox(token: &str) -> Option<Mailbox> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if let Some(open) = token.rfind('<') {
        let rest = &token[open + 1..];
        let close = rest.find('>')?;
        let address = rest[..close].trim();
        if address.is_empty() {
            return None;
        }
        let name = token[..open].trim().trim_matches('"').trim();
        return Some(if name.is_empty() {
            Mailbox::new(address)
        } else {
            Mailbox::with_name(address, name)
        });
    }
    Some(Mailbox::new(token))
}

/// 收件人框里的一整段文本 → 地址列表。
///
/// 分隔符三种都收：`,` / `，` / `;` / `；` 与换行。中文逗号必须认——用户从
/// 通讯录或聊天记录里拷过来的地址常常是中文标点分隔的，不认就等于「粘进来是一坨」。
fn parse_mailbox_list(text: &str) -> Vec<Mailbox> {
    text.split([',', '，', ';', '；', '\n'])
        .filter_map(parse_mailbox)
        .collect()
}

/// 引用原邮件正文：每行前置 `"> "`，空行写成 `">"`。
///
/// 空行写成裸 `>`（而不是 `"> "`）是 RFC 3676 §4.5 的口径：行尾空白在转手上会被
/// 削掉，`"> "` 收完就变成 `">"`，不如一开始就写裸的。
fn quote_reply(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + text.len() / 8);
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            quoted.push('\n');
        }
        let line = line.trim_end();
        if line.is_empty() {
            quoted.push('>');
        } else {
            quoted.push_str("> ");
            quoted.push_str(line);
        }
    }
    quoted
}

/// 回信正文：两个空行 + 归属行 + 引用块；`original` 为空时**返回空串**。
///
/// 为什么正文没下载时不留归属行：只留一句「在 xx，yy 写道：」而下面什么都没有，
/// 用户还得先把它删掉。宁可给一个干净的空框——写什么由用户决定。
fn reply_body(attribution: &str, original: Option<&str>) -> String {
    let Some(text) = original.filter(|text| !text.trim().is_empty()) else {
        return String::new();
    };
    format!("\n\n{attribution}\n{}", quote_reply(text))
}

/// 给回信主题补前缀；已经有 `Re:` / 前缀本身时原样返回。
///
/// 认 `Re:`（大小写不敏感）是因为：对面用英文客户端回过来时主题就带 `Re:`，
/// 再叠一层会变成 `回复：Re: xxx`，来回两轮就成 `回复：Re: 回复：Re: …`。
fn with_reply_prefix(prefix: &str, subject: &str) -> String {
    let bare = subject.trim();
    if bare.is_empty() {
        return prefix.trim_end().to_owned();
    }
    let lowered = bare.to_ascii_lowercase();
    if lowered.starts_with("re:") || bare.starts_with(prefix.trim_end()) {
        return bare.to_owned();
    }
    format!("{prefix}{bare}")
}

/// 从原邮件算出回信草稿的预填。
///
/// `prefix` 是本地化的 `回复：`，`attribution` 是本地化的归属行（都由调用方用
/// `tr` / `trf` 算好）。
///
/// ## `References` 只有一环，这是**已知的**取舍
///
/// RFC 5322 §3.6.4 的完整做法是「原邮件的 `References` + 原邮件的 `Message-ID`」，
/// 但本模块的 `messages` 表只存了 `message_id`，没有存 `references` 头。于是这里
/// 只能给一环：线程链在**回复链的深度**上仍然正确（每封都指向它的直接父节点），
/// 只是「从本封一路回溯到根」这个能力依赖客户端自己拼。要修得动 schema（加一列 +
/// 迁移 + 解析时的 `references` 头），不是这一批能顺带做的。
fn reply_seed(detail: &MessageDetail, prefix: &str, attribution: &str) -> ComposeSeed {
    // 发件人地址缺失（脏数据 / 退信）时不预填收件人——让用户自己填，而不是把
    // 一个空 `<>` 塞进收件人框。
    let to = detail
        .from_addr
        .as_deref()
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(|address| {
            let name = detail
                .from_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty());
            match name {
                Some(name) => Mailbox::with_name(address, name),
                None => Mailbox::new(address),
            }
        })
        .into_iter()
        .collect();

    // `message_id` 是头部里的裸值（可能带尖括号），两种形态都原样带上——
    // `compose::build_message` 会在写头部时规整尖括号。
    let message_id = detail
        .message_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);

    ComposeSeed {
        to,
        cc: Vec::new(),
        subject: with_reply_prefix(prefix, &detail.subject),
        body: reply_body(attribution, detail.text_body.as_deref()),
        references: message_id.clone().into_iter().collect(),
        in_reply_to: message_id,
    }
}

/// 撰写弹窗的表单状态（同 [`AccountForm`]：挂 [`Entity`] 才能在 `Fn` 闭包里改）。
struct ComposeForm {
    /// 发件账户主键。发送时用它取 SMTP 身份（主机 / 端口 / 用户名 / 口令）。
    account_id: i64,
    /// 发件人（账户档案里的地址与显示名）。**只读展示**：本模块没有多身份 / 别名，
    /// 所以这里不给选择器——给了也是空的。
    sender: Mailbox,
    to: Entity<InputState>,
    cc: Entity<InputState>,
    subject: Entity<InputState>,
    /// 正文。多行用 [`TextareaState`]（`InputState` 是单行引擎，回车即提交，
    /// 写不了正文）。
    body: Entity<TextareaState>,
    /// 回信时的 `In-Reply-To`。
    in_reply_to: Option<String>,
    /// 回信时的 `References`。
    references: Vec<String>,
    /// 已选附件。**字节在选文件那一刻就读好**（见 [`MailPage::pick_attachments`]）：
    /// 只存路径、等点「发送」再读的话，「文件没了 / 读不出来」会推迟到发送那一刻才
    /// 暴露，而且 `Draft` 也不再是纯数据（装配就成了带 IO 的操作）。路径一并留着，是
    /// 为了让「存草稿」能落 `draft_attachments.file_path`。
    attachments: Vec<ComposeAttachment>,
    /// 已存草稿的本地行号。`None` = 还没存过（点「存草稿」新建一行）；`Some` = 改存
    /// 那一行（从草稿箱点进来的，或连点两次「存草稿」）。
    draft_id: Option<i64>,
    /// 草稿由来（新写 / 回复 / 转发），落 `drafts_meta.mode`。
    ///
    /// 刻意**不在弹窗里显示**：回信的主题本来就已经带了「回复：」前缀，再来一行
    /// 「回复」标签是重复的。它落库是为了以后的分支（线程视图、转发的转发）。
    mode: DraftMode,
    /// 正在保存草稿（「存草稿」按钮的防重闸门与文案）。
    saving_draft: bool,
    /// 正在发送。
    sending: bool,
    /// 发送或保存草稿失败的原因（贴在弹窗正文里）。
    error: Option<SharedString>,
}

impl ComposeForm {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        account_id: i64,
        sender: Mailbox,
        seed: ComposeSeed,
        loaded: LoadedCompose,
    ) -> Self {
        let to_text = seed
            .to
            .iter()
            .map(mailbox_field_text)
            .collect::<Vec<_>>()
            .join(", ");
        let cc_text = seed
            .cc
            .iter()
            .map(mailbox_field_text)
            .collect::<Vec<_>>()
            .join(", ");

        Self {
            account_id,
            sender,
            to: seeded_text_input(window, cx, "someone@example.com", to_text),
            cc: seeded_text_input(window, cx, "可留空", cc_text),
            subject: seeded_text_input(window, cx, "主题", seed.subject),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .default_value(seed.body)
                    .auto_grow(COMPOSE_BODY_MIN_ROWS, COMPOSE_BODY_MAX_ROWS)
            }),
            in_reply_to: seed.in_reply_to,
            references: seed.references,
            attachments: loaded.attachments,
            draft_id: loaded.draft_id,
            mode: loaded.mode,
            saving_draft: false,
            sending: false,
            error: None,
        }
    }

    /// 表单取值 → [`Draft`]。
    ///
    /// 只校验「有没有收件人、地址像不像地址」——**能不能投递由服务器决定**，
    /// 不在这里重复一份 RFC 5322 的地址语法（写全了也不会有服务器认同）。
    fn to_draft(&self, cx: &App) -> Result<Draft, SharedString> {
        let to = parse_mailbox_list(&self.to.read(cx).value());
        if to.is_empty() {
            return Err(tr(cx, "请填写至少一个收件人。").into());
        }
        let cc = parse_mailbox_list(&self.cc.read(cx).value());
        if let Some(bad) = to.iter().chain(cc.iter()).find(|m| !is_address(&m.address)) {
            return Err(trf(cx, "「{}」不是有效的邮箱地址。", &[&bad.address]).into());
        }
        Ok(Draft {
            from: self.sender.clone(),
            to,
            cc,
            // 密送没有输入框：写信界面上的 Bcc 是「高级」档，而它的**唯一**效果是
            // 「不进头部」。没有收到明确需求前不给这个开关——给了用户就会以为
            // 「填进去的抄送也是密送」。
            bcc: Vec::new(),
            subject: self.subject.read(cx).value().trim().to_owned(),
            body: self.body.read(cx).value().to_string(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self
                .attachments
                .iter()
                .map(|attachment| attachment.payload.clone())
                .collect(),
            // 留空：由 `MailCore::send` 走 `Draft::stamp` 补 `Date` 与 `Message-ID`。
            date_unix: 0,
            message_id: String::new(),
        })
    }

    /// 表单取值 → [`DraftRecord`]（存草稿用）。
    ///
    /// 与 [`Self::to_draft`] 的差别只有一处：**什么都不校验**。草稿的全部意义就是
    /// 「还没写完」——要求先填一个像样的收件人才能存，等于把最该被存下来的那半途而废
    /// 挡在门外。地址同理：`a@` 这种半截地址也得能存。
    fn to_draft_record(&self, cx: &App) -> DraftRecord {
        DraftRecord {
            // `None` = 新建一行；`Some` = 改存那一行（内核会拒绝「不属于本账户的 id」）。
            id: self.draft_id,
            account_id: self.account_id,
            draft: Draft {
                from: self.sender.clone(),
                to: parse_mailbox_list(&self.to.read(cx).value()),
                cc: parse_mailbox_list(&self.cc.read(cx).value()),
                bcc: Vec::new(),
                subject: self.subject.read(cx).value().trim().to_owned(),
                body: self.body.read(cx).value().to_string(),
                in_reply_to: self.in_reply_to.clone(),
                references: self.references.clone(),
                attachments: self
                    .attachments
                    .iter()
                    .map(|attachment| attachment.payload.clone())
                    .collect(),
                // `message_id` 留空：内核的 `drafts::save` 是「新草稿生成一个、改存沿用
                // 旧的」，在这里自己生成会让改存换掉 id，别人引这一封的 `In-Reply-To`
                // 就悬空了。
                date_unix: 0,
                message_id: String::new(),
            },
            mode: self.mode,
            attachments: self
                .attachments
                .iter()
                .map(|attachment| {
                    DraftAttachmentRef::new(
                        attachment.source.to_string_lossy().into_owned(),
                        attachment.payload.filename.clone(),
                        Some(attachment.payload.content_type.clone()),
                    )
                })
                .collect(),
        }
    }
}

/// 「像不像一个邮箱地址」：恰好一个 `@`，两侧都非空且不含空白。
///
/// 刻意宽松：这里要拦的是「把名字填进了收件人框」「整段文本粘成一坨」，而不是
/// 给出一个邮箱地址的正则（RFC 5321 的地址语法复杂到没有客户端的正则是对的）。
fn is_address(address: &str) -> bool {
    let mut parts = address.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !address.chars().any(char::is_whitespace)
}

/// 单行输入框 + 可选初值。
///
/// 初值走 `default_value`（构造器）而不是 `set_value`：后者要 `&mut Window`，而
/// 构造之后窗口句柄就还给调用方了（`AccountForm` 也是这个道理，它干脆不填初值）。
/// 空串不填——填了会把占位文案顶掉，一个纯空白框比带占位文案的框更让人犹豫。
fn seeded_text_input<C: AppContext>(
    window: &mut Window,
    cx: &mut C,
    placeholder: impl Into<SharedString>,
    value: String,
) -> Entity<InputState> {
    let placeholder: SharedString = placeholder.into();
    let value: SharedString = value.trim().to_owned().into();
    cx.new(|cx| {
        let state = InputState::new(window, cx).placeholder(placeholder);
        if value.trim().is_empty() {
            state
        } else {
            state.default_value(value)
        }
    })
}

/// 弹窗正文：发件人（只读）+ 收件人 / 抄送 / 主题 + 正文 + 附件 + 错误。
fn render_compose_form(
    form: &Entity<ComposeForm>,
    view: &WeakEntity<MailPage>,
    cx: &App,
) -> AnyElement {
    let state = form.read(cx);
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let sender_name = tr(cx, "发件人");
    let mut body = v_flex()
        .w_full()
        .gap(form_gap(cx))
        .child(form_field(
            sender_name,
            // 只读原因写在下面的说明里，而不是灰掉一个输入框——灰框会让人以为
            // 「有什么条件没满足」，其实这里就是没有可选项。
            div()
                .w_full()
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .child(SharedString::from(mailbox_field_text(&state.sender))),
            None,
            Some(SharedString::from(tr(cx, "取自账户档案，不在这里更改。"))),
            cx,
        ))
        .child(form_field(
            tr(cx, "收件人"),
            Input::new(&state.to).control(cx).w_full(),
            None,
            Some(SharedString::from(tr(cx, "多个地址用逗号或分号分隔。"))),
            cx,
        ))
        .child(form_field(
            tr(cx, "抄送"),
            Input::new(&state.cc).control(cx).w_full(),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "主题"),
            Input::new(&state.subject).control(cx).w_full(),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "正文"),
            // 正文框自绘外框（同 `assistant.rs` 的输入区）：`Input::control` 的
            // 高度口径只服务单行，多行框要的是「随内容长到 20 行」，两套尺寸逻辑
            // 混在一个控件里就是上次那种「空框很高、打两行反而不长」的鬼影。
            div()
                .w_full()
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(tokens.colors.border)
                .bg(tokens.colors.surface)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .child(
                    Textarea::new(&state.body)
                        .appearance(false)
                        .aria_label(tr(cx, "邮件正文"))
                        .w_full()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height),
                ),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "附件"),
            render_compose_attachments(form, view, cx),
            None,
            Some(SharedString::from(tr(
                cx,
                "附件随信一起编码发出；总体积超过服务器声明的上限时，会在点「发送」时被拒。",
            ))),
            cx,
        ));

    if let Some(error) = &state.error {
        body = body.child(duck_components::field_error(error.clone(), cx));
    }
    // 「发出去了但没有已发送副本」是**用户看得见**的差异（换台机器打开邮箱就找不到
    // 这封信），所以常驻一行说明，而不是等用户来问。
    body = body.child(field_hint(
        tr(
            cx,
            "信由本机直接经 SMTP 发出；不会自动写进服务器的「已发送」文件夹。",
        ),
        cx,
    ));
    body.into_any_element()
}

/// 「附件」区的控件：「添加附件」+ 已选清单（每行一个移除按钮）。
///
/// 写入口只有两个：这里的移除按钮与 [`MailPage::pick_attachments`]（从草稿回填走的是
/// `ComposeForm::new` 那条路）。两者都改 `ComposeForm::attachments` 再 `notify`——弹窗层
/// （`WindowStateLayers`）每帧重跑一次 `content` 闭包，所以清单会立刻跟着变。
fn render_compose_attachments(
    form: &Entity<ComposeForm>,
    view: &WeakEntity<MailPage>,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();

    let add = {
        let form = form.clone();
        let view = view.clone();
        icon_text_button(
            "mail-compose-attach-add",
            tr(cx, "添加附件"),
            Icon::new(DuckIcon::Paperclip)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, _window, cx| {
            let _ = view.update(cx, |this, cx| this.pick_attachments(&form, cx));
        })
    };

    let mut column = v_flex()
        .w_full()
        .min_w_0()
        .gap(tokens.spacing.xs)
        .child(div().flex_none().child(add));

    for (index, attachment) in form.read(cx).attachments.iter().enumerate() {
        let remove = {
            let form = form.clone();
            icon_button(
                SharedString::from(format!("mail-compose-attach-remove-{index}")),
                tr(cx, "移除附件"),
                Icon::new(DuckIcon::X)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click(move |_, _window, cx| {
                form.update(cx, |form, cx| {
                    // 清单可能已经变了，越界就什么都不做（别 panic）。
                    if index < form.attachments.len() {
                        form.attachments.remove(index);
                        cx.notify();
                    }
                });
            })
        };
        column = column.child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(tokens.spacing.xs)
                .child(
                    Icon::new(DuckIcon::Paperclip)
                        .size(extended.icon.sm)
                        .text_color(extended.colors.text_tertiary),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .child(SharedString::from(attachment.payload.filename.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(SharedString::from(format_size(
                            attachment.payload.len() as i64
                        ))),
                )
                .child(remove),
        );
    }

    column.into_any_element()
}

/// 撰写弹窗脚注：左「存草稿」、右「取消 / 发送」。
///
/// 左右分工是刻意的：左边是**不结束**这次撰写的动作（存草稿可以反复做），右边是**两个
/// 出口**。把「存草稿」摆在「取消」旁边会让它看起来也像出口——其实点了之后弹窗还在。
fn compose_footer(
    form: &Entity<ComposeForm>,
    view: &WeakEntity<MailPage>,
    cx: &App,
) -> DialogFooter {
    let save_view = view.clone();
    let save_form = form.clone();
    let send_view = view.clone();
    let send_form = form.clone();
    let theme = active_theme(cx);
    let spacing = theme.tokens().spacing;
    let sending = form.read(cx).sending;
    let saving_draft = form.read(cx).saving_draft;
    let send_label = if sending {
        tr(cx, "发送中…")
    } else {
        tr(cx, "发送")
    };
    let draft_label = if saving_draft {
        tr(cx, "保存中…")
    } else {
        tr(cx, "存草稿")
    };

    let left = h_flex().gap(spacing.sm).child(
        button(
            "mail-compose-save-draft",
            draft_label,
            ButtonVariant::Secondary,
            cx,
        )
        // 发送途中不许存草稿：这一刻的正文已经交给发信那一路了，再落一份会让人以为
        // 「信没发出去，但草稿留下了」。
        .disabled(saving_draft || sending)
        .on_click(move |_, window, cx| {
            let _ = save_view.update(cx, |this, cx| this.save_draft(&save_form, window, cx));
        }),
    );

    let right = h_flex().gap(spacing.sm).children({
        let mut items: Vec<AnyElement> = Vec::new();
        items.push(
            button(
                "mail-compose-cancel",
                tr(cx, "取消"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx))
            .into_any_element(),
        );
        items.push(
            button("mail-compose-send", send_label, ButtonVariant::Primary, cx)
                .disabled(sending)
                .on_click(move |_, window, cx| {
                    // 返回 `false`（弹窗留着）是**对的**：发信是异步的，这一刻还不知道
                    // 成败。真正的关闭在 `send_compose` 的成功回调里。
                    let _ =
                        send_view.update(cx, |this, cx| this.send_compose(&send_form, window, cx));
                })
                .into_any_element(),
        );
        items
    });

    DialogFooter::new()
        .justify_between()
        .child(left)
        .child(right)
}

/// 「搜索语法」弹窗里的速查表：`(写法, 中文源串)`。
///
/// 「写法」是**要用户照着敲的原文**，所以直接写死而不走 `tr(...)`——`from:` 翻到哪个
/// 语言的界面里都还是 `from:`（它们最终落到 `duck-mail/src/db/repo/search.rs` 的词法里，
/// 不是文案）。右列是待翻译的**源串**，渲染时才查表。
///
/// 是常量而不是 `fn(cx) -> Vec<...>`：这张表与语言无关，`tr()` 属于渲染那一步。这样
/// [`tests::search_syntax_rows_keep_the_ui_translatable`] 不需要 `App`——`duck-app` 的
/// 单元测试里没有 `DuckThemeState` 全局（`active_theme` 会 panic），纯静态断言就够。
///
/// ⚠️ **左列一个字都不许带中文**：`duck-theme` 的守卫 `every_chinese_literal_is_translated`
/// 是从源码行里**逐字取引号之间的原文**（`\"两个词\"` 取出来就是带反斜杠的 `\"两个词\"`）
/// 再去表里查——而表里的键是**已解析**的（`"两个词"`）。两者永远对不上，于是左列一旦
/// 出现中文（哪怕只是示例）就会让守卫红，且没法靠登记条目消掉。
///
/// 顺序与 `search.rs` 模块头那张表**逐条对齐**：那张表是给改检索的人看的，这里是给用
/// 检索的人看的。加一条语法要同时改两处。
const SEARCH_SYNTAX_ROWS: &[(&str, &str)] = &[
    (
        "from:",
        "发件人地址或显示名，子串匹配；中文「发件人:」同义，冒号全角半角都认",
    ),
    ("to:", "收件人，子串匹配；中文「收件人:」同义"),
    ("subject:", "主题，子串匹配；中文「主题:」或「标题:」同义"),
    (
        "folder:",
        "文件夹名，或 inbox / sent / archive 这类角色；中文「文件夹:」同义",
    ),
    (
        "is:unread",
        "只看未读（is:read 看已读；中文值 is:未读 / is:已读 同义）",
    ),
    (
        "is:starred",
        "只看有旗标的（is:unstarred 看没旗标的；中文值 is:有旗标 同义）",
    ),
    ("has:attachment", "只看带附件的（中文值 has:附件 同义）"),
    (
        "before:2026-01-01",
        "早于这一天（older: 同义；中文「早于:」「之前:」同义）",
    ),
    (
        "after:2026-01-01",
        "不早于这一天（newer: 同义；中文「晚于:」「之后:」同义）",
    ),
    (
        "after:today",
        "日期也认相对词：今天 / 昨天 / 前天 / 本周 / 上周 / 本月 / 上月 / 最近7天",
    ),
    (
        "zhang@example.com",
        "直接搜邮箱地址，自动按「发件人或收件人」匹配，不用写 from: / to:",
    ),
    ("\"...\"", "双引号里当一个短语，不拆词"),
    ("report*", "前缀匹配，汉字与字母数字都行"),
];

/// 「搜索语法」弹窗正文：两列表 + 一条横线 + 两条脚注。
///
/// 左列走等宽（`typography.mono_md` + `mono` 字体族）——`has:attachment` 与
/// `before:2026-01-01` 这类串要能一眼对齐着看，正文比例字体下冒号会各飘各的。
fn render_search_syntax(cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let mono = tokens.typography.mono_md;

    let rows = SEARCH_SYNTAX_ROWS.iter().map(|(syntax, source)| {
        h_flex()
            .w_full()
            .items_start()
            .gap(tokens.spacing.sm)
            .child(
                div()
                    .w(px(SEARCH_SYNTAX_COLUMN))
                    .flex_none()
                    .font_family(tokens.typography.mono.clone())
                    .text_size(mono.size)
                    .line_height(mono.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(*syntax),
            )
            // `min_w_0`：不给的话 `flex_1` 在窄宽度下不肯收缩，说明会被推出弹窗。
            .child(meta_text(tr(cx, source), cx).flex_1().min_w_0())
            .into_any_element()
    });

    v_flex()
        .w_full()
        .gap(tokens.spacing.xs)
        .children(rows)
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .mt(tokens.spacing.xs)
                .pt(tokens.spacing.sm)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(meta_text(
                    tr(cx, "多个条件之间、条件与关键词之间都是「同时满足」。"),
                    cx,
                ))
                .child(meta_text(
                    tr(cx, "认不出的写法会当成普通关键词原样搜索，不会报错。"),
                    cx,
                )),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

impl Render for MailPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 占位文案跟着语言走（同 `settings.rs`）。要在 `render_main` 前跑：
        // 列表条的搜索框是它渲染出来的。
        self.sync_search_placeholder(window, cx);
        let content = self.render_main(cx);

        let mut panel = SidebarPanel::new("mail-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            // 键位落在页面根上（上下文 `"Mail"`，见 [`bind_keys`]）。焦点在搜索框里时
            // gpui 沿焦点链往外找绑定，走到这里命中——`⌘N` 在输入框自己的表里没有归属。
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &NewMessage, window, cx| {
                this.open_new_mail_dialog(window, cx);
            }))
            .child(div().flex_1().min_h_0().min_w_0().child(panel))
            .child(self.render_status_bar(cx))
    }
}

impl MailPage {
    // ------------------------------------------------------------------
    // 左栏：账户头 + 账户切换 + 文件夹
    // ------------------------------------------------------------------

    fn render_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        let active = self.active_account();
        // 账户头：头像位 + 账户名 + 「添加 / 移除账户」。
        let header = h_flex()
            .flex_none()
            .w_full()
            .h(px(ACCOUNT_HEADER_HEIGHT))
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_none()
                    .size(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(extended.colors.hover_tint)
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .when_some(active, |this, account| {
                        this.child(SharedString::from(account.initial().to_string()))
                    })
                    .when(active.is_none(), |this| {
                        this.child(
                            Icon::new(DuckIcon::Mail)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.text_tertiary),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(match active {
                        Some(account) => SharedString::from(account.label().to_string()),
                        None => SharedString::from(tr(cx, "未添加账户")),
                    }),
            )
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-edit-account",
                    tr(cx, "编辑当前账户"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(active.is_none())
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        let Some(account) = this.active_account().cloned() else {
                            return;
                        };
                        this.open_account_dialog(Some(account), window, cx);
                    });
                })
            })
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-remove-account",
                    tr(cx, "移除当前账户"),
                    Icon::new(DuckIcon::Trash2)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(active.is_none())
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.remove_active_account(cx));
                })
            })
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-add-account",
                    tr(cx, "添加邮箱账户"),
                    Icon::new(DuckIcon::Plus)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.core.is_none())
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_account_dialog(None, window, cx));
                })
            });

        let mut body = v_flex()
            .id("mail-sidebar-scroll")
            .flex_1()
            .w_full()
            .min_h_0()
            // ⚠️ 滚动容器的 `.id()` 不能省：本仓的 GPUI 分叉把 `overflow_y_scroll`
            // 挂在 `StatefulInteractiveElement` 上（只对 `Stateful<E>` 实现），
            // 因为滚动位置要按 ElementId 做保留。裸 `v_flex()` 没有这个方法。
            .overflow_y_scroll()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.xs)
            .py(tokens.spacing.xs);

        if let Some(error) = &self.core_error {
            body = body.child(field_hint(error.clone(), cx));
        }

        // 多个账户才需要切换行；只有一个账户时那行是纯噪音。
        if self.accounts.len() > 1 {
            body = body.child(section_heading(tr(cx, "账户"), cx));
            for account in &self.accounts {
                body = body.child(self.account_row(account, cx));
            }
        }

        body = body.child(section_heading(tr(cx, "文件夹"), cx));
        if self.folders.is_empty() {
            body = body.child(field_hint(
                if self.active_account.is_some() {
                    tr(cx, "点「同步」拉取文件夹。")
                } else {
                    tr(cx, "添加账户后，这里列出收件箱、草稿等文件夹。")
                },
                cx,
            ));
        } else {
            for folder in &self.folders {
                body = body.child(self.folder_row(folder, cx));
            }
        }

        // 标签分组：没有账户时整段不画（没有账户就没有账户级标签可言）。
        if self.active_account.is_some() {
            body = body.child(section_heading(tr(cx, "标签"), cx));
            if self.labels.is_empty() {
                body = body.child(field_hint(tr(cx, "还没有标签，点下面的「新建标签」。"), cx));
            } else {
                for label in &self.labels {
                    body = body.child(self.label_row(label, cx));
                }
            }
            body = body.child(self.new_label_row(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// 当前账户。
    fn active_account(&self) -> Option<&Account> {
        let id = self.active_account?;
        self.accounts.iter().find(|a| a.id == id)
    }

    /// 左栏一行（账户 / 文件夹共用）：图标 + 名字 + 可选计数徽标。
    ///
    /// 自绘而不是走 `nav_list`：`NavItem` 只支持「图标 + 文本」，而文件夹行要挂未读
    /// 计数；`nav_list` 的滑动高亮块还依赖固定行高与下标，多一行就错位。
    fn sidebar_row(
        &self,
        id: SharedString,
        label: SharedString,
        aria: SharedString,
        icon: DuckIcon,
        count: Option<i64>,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let hover = extended.colors.hover_tint;
        let press = extended.colors.hover_tint_press;
        let ring = tokens.colors.ring;

        h_flex()
            .id(id)
            .flex_none()
            .w_full()
            .h(theme.density().nav_row)
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.xs)
            .rounded(tokens.radius.sm)
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(aria)
            .when(selected, |this| this.bg(extended.colors.nav_selected))
            .when(!selected, |this| this.hover(move |style| style.bg(hover)))
            .active(move |style| style.bg(press))
            .focusable()
            .tab_stop(true)
            .focus_visible(move |style| style.border_1().border_color(ring))
            .on_click(on_click)
            .child(
                Icon::new(icon)
                    .size(extended.icon.sm)
                    .text_color(if selected {
                        tokens.colors.foreground
                    } else {
                        extended.colors.text_tertiary
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .child(label),
            )
            .when_some(count.filter(|n| *n > 0), |this, count| {
                this.child(
                    div()
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(SharedString::from(count.to_string())),
                )
            })
            .into_any_element()
    }

    fn account_row(&self, account: &Account, cx: &App) -> AnyElement {
        let layout = account.id;
        let view = self.view.clone();
        self.sidebar_row(
            format!("mail-account-{}", account.id).into(),
            SharedString::from(account.label().to_string()),
            SharedString::from(trf(cx, "账户 {}", &[&account.label().to_string()])),
            // 账户行不用角色图标，用「服务器」——一眼区分「这是账户」。
            DuckIcon::Server,
            None,
            self.active_account == Some(account.id),
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.select_account(layout, window, cx));
            },
            cx,
        )
    }

    fn folder_row(&self, folder: &Folder, cx: &App) -> AnyElement {
        let layout = folder.id;
        let view = self.view.clone();
        // `Other` 用服务端原名（那是用户自己起的，但要解 IMAP 的 Modified UTF-7），
        // 认出来的角色用本地化名。
        let name: SharedString = folder_display_name(folder, cx);
        // 草稿箱的角标用**本地**草稿数，不是那个 IMAP 文件夹的未读数：本地草稿的
        // `folder_id` 是 NULL，压根不在服务端的 `\Drafts` 里（见 `repo::drafts` 模块头）
        // ——照 `folder.unread` 显示的话，用户存了一堆草稿，角标还是 0。
        let badge = if folder.role == FolderRole::Drafts {
            self.local_drafts
        } else {
            folder.unread
        };
        let aria: SharedString = if folder.role == FolderRole::Drafts {
            trf(
                cx,
                "文件夹 {}，{} 封草稿",
                &[&name.to_string(), &badge.to_string()],
            )
            .into()
        } else if badge > 0 {
            trf(
                cx,
                "文件夹 {}，{} 封未读",
                &[&name.to_string(), &badge.to_string()],
            )
            .into()
        } else {
            trf(cx, "文件夹 {}", &[&name.to_string()]).into()
        };
        self.sidebar_row(
            format!("mail-folder-{}", folder.id).into(),
            name,
            aria,
            role_icon(folder.role),
            Some(badge),
            self.active_folder == Some(folder.id),
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.select_folder(layout, window, cx));
            },
            cx,
        )
    }

    /// 左栏标签行（文件夹行的变体）：颜色点 + 名字。
    ///
    /// 不复用 [`Self::sidebar_row`]：那里首元素是 `DuckIcon`，而标签要显示的是**用户自己
    /// 挑的颜色**（`label.color`，色块），图标表达不了。其余交互（选中底、悬停、按下、
    /// 焦点环、行高）与文件夹行逐条对齐，保证两段视觉一致。
    fn label_row(&self, label: &Label, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let hover = extended.colors.hover_tint;
        let press = extended.colors.hover_tint_press;
        let ring = tokens.colors.ring;
        let selected = self.active_label == Some(label.id);
        let id = label.id;
        let name: SharedString = SharedString::from(label.name.clone());
        let aria: SharedString = trf(cx, "标签 {}", &[&label.name]).into();
        let view = self.view.clone();

        h_flex()
            .id(format!("mail-label-{id}"))
            .flex_none()
            .w_full()
            .h(theme.density().nav_row)
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.xs)
            .rounded(tokens.radius.sm)
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(aria)
            .when(selected, |this| this.bg(extended.colors.nav_selected))
            .when(!selected, |this| this.hover(move |style| style.bg(hover)))
            .active(move |style| style.bg(press))
            .focusable()
            .tab_stop(true)
            .focus_visible(move |style| style.border_1().border_color(ring))
            .on_click(move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.select_label(id, window, cx));
            })
            // 色块占位与文件夹行的图标同宽（`icon.sm`），两段左边缘因此齐平。
            .child(
                div()
                    .flex_none()
                    .size(extended.icon.sm)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(8.))
                            .flex_none()
                            .rounded_full()
                            .bg(label_color(&label.color, cx)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .child(name),
            )
            .into_any_element()
    }

    /// 左栏「新建标签」入口行：点击打开新建标签弹窗。
    fn new_label_row(&self, cx: &App) -> AnyElement {
        let view = self.view.clone();
        self.sidebar_row(
            "mail-label-new".into(),
            SharedString::from(tr(cx, "新建标签")),
            SharedString::from(tr(cx, "新建标签")),
            DuckIcon::Plus,
            None,
            false,
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.open_label_dialog(window, cx));
            },
            cx,
        )
    }

    // ------------------------------------------------------------------
    // 中栏 + 阅读器
    // ------------------------------------------------------------------

    fn render_main(&self, cx: &App) -> AnyElement {
        h_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_list(cx))
            .child(self.render_reader(cx))
            .into_any_element()
    }

    /// 中栏：列表条 + 邮件列表。
    fn render_list(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        // 搜索态下列表是检索结果（跨文件夹、按相关度），标题与清空入口都要跟着变——
        // 不然用户会以为「搜完还停在这个文件夹里」。
        let searching = self.search_query.is_some();
        // 草稿箱要在列表里**置顶本地草稿**——它们的 `folder_id` 是 NULL，压根不在
        // `messages` 里（见 `duck_mail::db::repo::drafts` 模块头）。搜索态与标签态不算：
        // 那两种列表的来源与「当前文件夹」无关。
        let showing_drafts = !searching
            && self
                .active_folder
                .is_some_and(|id| self.is_drafts_folder(id));
        let folder_name: SharedString = self
            .active_folder
            .and_then(|id| self.folders.iter().find(|f| f.id == id))
            .map(|folder| folder_display_name(folder, cx))
            .unwrap_or_else(|| SharedString::from(tr(cx, "邮箱")));
        let title: SharedString = if searching {
            SharedString::from(tr(cx, "搜索结果"))
        } else {
            folder_name
        };

        // 列表条：标题 + 计数 | 清空搜索（搜索态）/ 同步（真连服务器）。
        let bar = h_flex()
            .flex_none()
            .w_full()
            .h(px(BAR_HEIGHT))
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .font_features(tabular_numbers())
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(
                        // 草稿箱里两类行都算上：本地草稿 + 该文件夹里已同步的邮件。
                        (!self.messages.is_empty() || showing_drafts && !self.drafts.is_empty())
                            .then(|| {
                                (self.messages.len()
                                    + if showing_drafts { self.drafts.len() } else { 0 })
                                .to_string()
                            })
                            .unwrap_or_default(),
                    )),
            )
            .child(div().flex_1())
            // 写邮件：`MailCore::send`（SMTP 直发）。搜索态下也在——写信跟列表在看的
            // 是哪个文件夹无关，藏起来只会让人先清搜索再写。
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-compose",
                    tr(cx, "写邮件"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.active_account.is_none())
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_new_mail_dialog(window, cx));
                })
            })
            .when(searching, |this| {
                let view = self.view.clone();
                this.child(
                    icon_button(
                        "mail-clear-search",
                        tr(cx, "清空搜索"),
                        Icon::new(DuckIcon::X)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.clear_search(window, cx);
                            // `set_value` 不派发 `InputEvent::Change`，所以这里要显式拉一次，
                            // 否则列表会停在上一次的检索结果上。
                            this.reload_messages(cx);
                        });
                    }),
                )
            })
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-list-refresh",
                    tr(cx, "同步当前账户"),
                    Icon::new(DuckIcon::RotateCw)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.active_account.is_none() || self.busy)
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.sync_now(cx));
                })
            });

        // 搜索行：全账号 FTS5 检索的入口。搜索框**独立一行**而不是塞进 44px 的列表条：
        // 340px 宽减去标题/计数/同步按钮后，输入框会窄到只剩几个字。
        let search_row = h_flex()
            .flex_none()
            .w_full()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div().flex_1().min_w_0().child(
                    // `.control`：Medium 档字号 + 显式控件高度（同 `settings.rs`）。
                    Input::new(&self.search)
                        .control(cx)
                        .prefix(
                            Icon::new(DuckIcon::Search)
                                .size(extended.icon.md)
                                .text_color(extended.colors.text_tertiary),
                        )
                        .cleanable(true)
                        // 输入框自带 `Role::TextInput` 但没有可访问名（占位不是名字），
                        // 读屏只会念「文本栏」。复用「搜索全部邮件」当名字。
                        .aria_label(tr(cx, "搜索全部邮件")),
                ),
            )
            // 语法速查入口。`from:` / `is:unread` 这套条件**没有别的地方能让人知道**，
            // 光靠占位文案只能露出两个例子；把一个只会被扫一眼的表挂在搜索框旁边，
            // 比写进文档靠谱（没人会去翻文档再回来搜邮件）。
            .child({
                let view = self.view.clone();
                icon_button(
                    "mail-search-help",
                    tr(cx, "搜索语法"),
                    Icon::new(DuckIcon::Info)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    ButtonVariant::Ghost,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_search_help_dialog(window, cx));
                })
            });

        let body: AnyElement = if self.active_account.is_none() {
            empty_state(
                DuckIcon::Inbox,
                tr(cx, "还没有邮箱账户"),
                Some(tr(cx, "用左栏的「+」添加账户，再点「同步」拉取邮件。").into()),
                None,
                cx,
            )
            .into_any_element()
        } else if self.messages.is_empty() && searching {
            // 搜不到和「文件夹空着」是两回事，文案得分开说：否则用户会以为邮件丢了。
            // 说明里点出条件语法 —— 搜不到的最常见原因是「条件写错了」，而这一屏正是
            // 用户会停下来想办法的时候。
            empty_state(
                DuckIcon::Search,
                tr(cx, "没有匹配的邮件"),
                Some(tr(cx, "换个关键词，或清空搜索框回到文件夹列表；也可以用 from: / is:unread 这类条件。").into()),
                None,
                cx,
            )
            .into_any_element()
        } else if self.messages.is_empty() && showing_drafts && self.drafts.is_empty() {
            // 草稿箱的空态得单独说：它既不是「没账户」也不是「服务器上没邮件」——
            // 草稿全在本机，跟同步无关，说「点同步拉取」会把人带偏。
            empty_state(
                DuckIcon::Pencil,
                tr(cx, "草稿箱还没有草稿"),
                Some(
                    tr(
                        cx,
                        "点列表条右侧的「写邮件」，写完点「存草稿」就会留在这里。",
                    )
                    .into(),
                ),
                None,
                cx,
            )
            .into_any_element()
        } else if self.messages.is_empty() {
            empty_state(
                DuckIcon::Inbox,
                tr(cx, "这个文件夹还没有邮件"),
                Some(tr(cx, "如果是第一次，点列表条右侧的同步按钮从服务器拉取。").into()),
                None,
                cx,
            )
            .into_any_element()
        } else {
            // 草稿箱：本地草稿**置顶**，其后是该文件夹里已同步的邮件。
            let mut rows: Vec<AnyElement> = Vec::new();
            if showing_drafts {
                rows.extend(self.drafts.iter().map(|draft| self.draft_row(draft, cx)));
            }
            rows.extend(self.messages.iter().map(|m| self.message_row(m, cx)));
            v_flex()
                .id("mail-list-scroll")
                .flex_1()
                .w_full()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };

        v_flex()
            .flex_none()
            .w(px(LIST_WIDTH))
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .bg(tokens.colors.surface)
            .border_r(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(bar)
            .child(search_row)
            .child(div().flex_1().w_full().min_h_0().flex().child(body))
            .into_any_element()
    }

    /// 一封邮件的列表行：发件人 + 时间 / 主题 / 摘要。
    fn message_row(&self, message: &MessageSummary, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let hover = extended.colors.hover_tint;
        let press = extended.colors.hover_tint_press;
        let ring = tokens.colors.ring;

        let id = message.id;
        let view = self.view.clone();
        let unknown = tr(cx, "(未知发件人)");
        let no_subject = tr(cx, "(无主题)");
        let sender = SharedString::from(
            sender_display(
                message.from_name.as_deref(),
                message.from_addr.as_deref(),
                unknown,
            )
            .to_string(),
        );
        let subject =
            SharedString::from(subject_or_placeholder(&message.subject, no_subject).to_string());
        let time = SharedString::from(format_list_time(message.date));
        // 读屏读不出「加粗」「小圆点」与那个星形图标，所以未读与旗标两个状态都必须
        // 进可访问名。四种组合各写一条，而不是拼一个「，已加旗标」的尾巴——尾巴那种
        // 写法把标点交给译者，换个语序就散了。
        let args: [&str; 3] = [&subject, &sender, &time];
        let aria: SharedString = match (message.is_read, message.is_starred) {
            (true, false) => trf(cx, "邮件：{}，来自 {}，{}", &args),
            (false, false) => trf(cx, "未读邮件：{}，来自 {}，{}", &args),
            (true, true) => trf(cx, "邮件：{}，来自 {}，{}（已加旗标）", &args),
            (false, true) => trf(cx, "未读邮件：{}，来自 {}，{}（已加旗标）", &args),
        }
        .into();
        let selected = self.selected == Some(message.id);
        let unread = !message.is_read;
        let has_attachments = message.has_attachments;

        v_flex()
            .id(format!("mail-row-{}", message.id))
            .flex_none()
            .w_full()
            .h(theme.density().mail_row)
            .min_w_0()
            .justify_center()
            .gap(px(2.))
            .px(tokens.spacing.sm)
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(aria)
            .when(selected, |this| this.bg(extended.colors.nav_selected))
            .when(!selected, |this| this.hover(move |style| style.bg(hover)))
            .active(move |style| style.bg(press))
            .focusable()
            .tab_stop(true)
            .focus_visible(move |style| style.border_1().border_color(ring))
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.open_message(id, cx));
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    // 未读→实心圆点；已读留同宽占位，避免整行文字左右抖动。
                    .child(div().flex_none().size(px(6.)).rounded_full().bg(if unread {
                        tokens.colors.foreground
                    } else {
                        gpui::transparent_black()
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(if unread {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .child(sender),
                    )
                    .when(has_attachments, |this| {
                        this.child(
                            Icon::new(DuckIcon::Paperclip)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.text_tertiary),
                        )
                    })
                    // 旗标：后端（`is_starred`）与切换入口（工具条「旗标」）都早就在，
                    // 只差这一处渲染——列表上看不出哪封加了旗标，工具条那个按钮就
                    // 没有可核对的反馈。只在**已加**时画，不画灰星：列表行没有逐行
                    // hover 态，一个常驻的灰星在 340px 宽的列表里只是噪声。
                    //
                    // 颜色借 `warning`（琥珀）：主题里没有 `flag` / `star` 专用档，
                    // 而它就是那份暖色强调（与 `sun` 同值，见 `builtin.rs:995`）；
                    // 为一个字形新增 token 不划算。
                    .when(message.is_starred, |this| {
                        this.child(
                            Icon::new(DuckIcon::Star)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.warning),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .font_features(tabular_numbers())
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(time),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .child(subject),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(message.snippet.clone())),
            )
            .into_any_element()
    }

    /// 一条草稿的列表行：收件人（+ 附件数）+ 时间 / 主题。
    ///
    /// 比 [`Self::message_row`] 少一行（`DraftSummary` 不带 `snippet`），高度照样按
    /// `density().mail_row` 走、内容在垂直方向居中，所以草稿行与邮件行混排时**不跳高**。
    ///
    /// 「删除」做成行内**兄弟节点**，而不是套在那块可点击区域里面：草稿行的主体整块是
    /// 一个按钮（点开撰写弹窗），往按钮里再塞一个按钮，两次点击谁先拿到就交给事件冒泡
    /// 去决定了——「点了删除却打开了弹窗」这种错在这一层不值得赌。
    fn draft_row(&self, draft: &DraftSummary, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let hover = extended.colors.hover_tint;
        let press = extended.colors.hover_tint_press;
        let ring = tokens.colors.ring;

        let id = draft.id;
        let open_view = self.view.clone();
        let delete_view = self.view.clone();
        let recipients = draft.recipients_display();
        let recipients: SharedString = if recipients.trim().is_empty() {
            tr(cx, "(无收件人)").into()
        } else {
            recipients.into()
        };
        let subject: SharedString = SharedString::from(
            subject_or_placeholder(&draft.subject, tr(cx, "(无主题)")).to_owned(),
        );
        let time: SharedString = SharedString::from(format_list_time(draft.date));
        let attachment_count = draft.attachment_count;
        // 读屏读不出加粗与纸夹图标，所以「这是草稿」「发给谁」「几个附件」都得进可访问名
        // （同 `message_row` 的口径：四种组合各写一条完整文案，不拼尾巴）。
        let count_text = attachment_count.to_string();
        let args: [&str; 4] = [&subject, &recipients, &time, &count_text];
        let aria: SharedString = if attachment_count > 0 {
            trf(cx, "草稿：{}，发给 {}，{}，带 {} 个附件", &args).into()
        } else {
            trf(cx, "草稿：{}，发给 {}，{}", &args[..3]).into()
        };

        let open = v_flex()
            .id(format!("mail-draft-{id}"))
            .flex_1()
            .min_w_0()
            .justify_center()
            .gap(px(2.))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(aria)
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(press))
            .focusable()
            .tab_stop(true)
            .focus_visible(move |style| style.border_1().border_color(ring))
            .on_click(move |_, _window, cx| {
                let _ = open_view.update(cx, |this, cx| this.open_draft(id, cx));
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    // 铅笔而不是 `message_row` 那个未读圆点：这一行是「还没写完的信」，
                    // 不是「没读过的信」，用同一个圆点会让人以为草稿有未读态。
                    .child(
                        Icon::new(DuckIcon::Pencil)
                            .size(extended.icon.sm)
                            .text_color(extended.colors.text_tertiary),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(recipients),
                    )
                    .when(attachment_count > 0, |this| {
                        this.child(
                            Icon::new(DuckIcon::Paperclip)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.text_tertiary),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_features(tabular_numbers())
                                .text_size(tokens.typography.xs.size)
                                .text_color(extended.colors.text_tertiary)
                                .child(SharedString::from(count_text)),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .font_features(tabular_numbers())
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(time),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .child(subject),
            );

        let delete = icon_button(
            SharedString::from(format!("mail-draft-delete-{id}")),
            tr(cx, "删除草稿"),
            Icon::new(DuckIcon::Trash2)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            ButtonVariant::Ghost,
            cx,
        )
        .on_click(move |_, _window, cx| {
            let _ = delete_view.update(cx, |this, cx| this.delete_draft(id, cx));
        });

        h_flex()
            .flex_none()
            .w_full()
            .h(theme.density().mail_row)
            .min_w_0()
            .px(tokens.spacing.sm)
            .gap(tokens.spacing.xs)
            .child(open)
            .child(delete)
            .into_any_element()
    }

    /// 阅读器：工具条 + 头部 + 正文 + 附件。
    fn render_reader(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        // 工具条上每个按钮都有真后端：回信走 `MailCore::send`（SMTP 直发）；标为
        // 已读/未读、旗标、移动、打标签、删除走动作队列（乐观更新 + 撤销条）；
        // 下载正文走 `fetch_message_body`。
        // 仍不占位的只有转发 / 归档 / 稍后 / AI 分析——那些后端还没有，按 P3-6
        // 一律不画。
        let detail = self.detail.as_ref();
        let is_read = detail.map(|d| d.is_read).unwrap_or(true);
        let body_cached = detail.map(|d| d.body_state == "cached").unwrap_or(false);

        let bar = h_flex()
            .flex_none()
            .w_full()
            .h(px(BAR_HEIGHT))
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            // 回信：`MailCore::send` + 预填（收件人 = 原发件人、主题带前缀、正文带
            // 引用块）。放在第一位——它是阅读一封邮件之后最常做的动作。
            .child({
                let view = self.view.clone();
                toolbar_action_button(
                    "mail-reply",
                    tr(cx, "回信"),
                    Icon::new(DuckIcon::Send).size(extended.icon.md),
                    false,
                    // 要 `detail` 而不是 `selected`：预填要用原邮件的发件人与正文，
                    // 详情还没到就点下去只会静默什么都不做——那就是个假控件。
                    self.detail.is_some() && self.active_account.is_some(),
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_reply_dialog(window, cx));
                })
            })
            .child({
                let view = self.view.clone();
                toolbar_action_button(
                    "mail-toggle-read",
                    tr(
                        cx,
                        if is_read {
                            "标为未读"
                        } else {
                            "标为已读"
                        },
                    ),
                    Icon::new(if is_read {
                        DuckIcon::Mail
                    } else {
                        DuckIcon::Check
                    })
                    .size(extended.icon.md),
                    false,
                    self.selected.is_some(),
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_read(cx));
                })
            })
            // 旗标（星标）：`MailCore::set_starred`（走动作队列，可撤销）。
            .child({
                let view = self.view.clone();
                let starred = detail.is_some_and(|d| d.is_starred);
                toolbar_action_button(
                    "mail-toggle-star",
                    tr(cx, if starred { "取消旗标" } else { "旗标" }),
                    Icon::new(DuckIcon::Star).size(extended.icon.md),
                    false,
                    self.selected.is_some(),
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_star(cx));
                })
            })
            // 移动到文件夹：`MailCore::move_message`。菜单项 = 当前账户的文件夹；
            // 目录所在文件夹勾选且禁用（移到原地没有意义）。
            .child({
                let view = self.view.clone();
                let options: Vec<(i64, SharedString)> = self
                    .folders
                    .iter()
                    // 合成的「草稿」行（`LOCAL_DRAFTS_FOLDER_ID`）**不能当移动目标**：
                    // 它不对应任何 IMAP 文件夹，选它只会让动作队列去 COPY 一个不存在的
                    // uid，白失败一次。它是本地草稿的入口，不是可投递的目的地。
                    .filter(|folder| folder.id != LOCAL_DRAFTS_FOLDER_ID)
                    .map(|folder| (folder.id, folder_display_name(folder, cx)))
                    .collect();
                let current = self.active_folder;
                let has_selection = self.selected.is_some();
                let empty: SharedString = tr(cx, "没有可用的文件夹").into();
                toolbar_dropdown_button(
                    "mail-move",
                    tr(cx, "移动到文件夹"),
                    Icon::new(DuckIcon::FolderOpen).size(extended.icon.md),
                    Anchor::BottomLeft,
                    move |menu, _, _| {
                        if options.is_empty() {
                            return menu.item(PopupMenuItem::new(empty.clone()).disabled(true));
                        }
                        options.iter().fold(menu, |menu, (folder_id, name)| {
                            let folder_id = *folder_id;
                            let is_current = current == Some(folder_id);
                            menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(is_current)
                                    .disabled(!has_selection || is_current)
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.move_selected(folder_id, cx)
                                            });
                                        }
                                    }),
                            )
                        })
                    },
                    cx,
                )
            })
            // 打标签：`label_add` / `label_remove`。菜单项勾选态 = 已贴该标签；末尾固定一个
            // 「新建标签…」入口（等于左栏「标签」分组的「+」）。
            .child({
                let view = self.view.clone();
                let options: Vec<(i64, SharedString)> = self
                    .labels
                    .iter()
                    .map(|label| (label.id, SharedString::from(label.name.clone())))
                    .collect();
                let attached = self.message_labels.clone();
                let has_selection = self.selected.is_some();
                let empty: SharedString = tr(cx, "还没有标签").into();
                let new_label: SharedString = tr(cx, "新建标签…").into();
                toolbar_dropdown_button(
                    "mail-label",
                    tr(cx, "打标签"),
                    Icon::new(DuckIcon::Tag).size(extended.icon.md),
                    Anchor::BottomLeft,
                    move |menu, _, _| {
                        let menu = if options.is_empty() {
                            menu.item(PopupMenuItem::new(empty.clone()).disabled(true))
                        } else {
                            options.iter().fold(menu, |menu, (label_id, name)| {
                                let label_id = *label_id;
                                let checked = attached.contains(&label_id);
                                menu.item(
                                    PopupMenuItem::new(name.clone())
                                        .checked(checked)
                                        .disabled(!has_selection)
                                        .on_click({
                                            let view = view.clone();
                                            move |_, _, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.toggle_label(label_id, cx)
                                                });
                                            }
                                        }),
                                )
                            })
                        };
                        menu.separator()
                            .item(PopupMenuItem::new(new_label.clone()).on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view
                                        .update(cx, |this, cx| this.open_label_dialog(window, cx));
                                }
                            }))
                    },
                    cx,
                )
            })
            // 删除：`MailCore::delete_message`（有废纸篓就移进去，否则打 `\Deleted` 再 EXPUNGE）。
            .child({
                let view = self.view.clone();
                toolbar_action_button(
                    "mail-delete",
                    tr(cx, "删除"),
                    Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                    true,
                    self.selected.is_some(),
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.delete_selected(cx));
                })
            })
            .child({
                let view = self.view.clone();
                let message_id = self.selected;
                toolbar_action_button(
                    "mail-fetch-body",
                    tr(cx, "下载正文"),
                    Icon::new(DuckIcon::Download).size(extended.icon.md),
                    false,
                    self.selected.is_some() && !body_cached,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let Some(id) = message_id else {
                        return;
                    };
                    let _ = view.update(cx, |this, cx| this.fetch_body(id, cx));
                })
            })
            .child(div().flex_1())
            // 「两种正文都有」（`multipart/alternative`）时才出现的版本切换：
            // 标签写的是**点下去会变成什么**（HTML 在显示 → 按钮是「查看纯文本版本」）。
            .when(self.has_both_bodies(), |this| {
                let view = self.view.clone();
                let to_html = !self.show_html;
                this.child(
                    toolbar_action_button(
                        "mail-toggle-body-view",
                        tr(
                            cx,
                            if to_html {
                                "查看 HTML 版本"
                            } else {
                                "查看纯文本版本"
                            },
                        ),
                        Icon::new(if to_html {
                            DuckIcon::Code
                        } else {
                            DuckIcon::FileText
                        })
                        .size(extended.icon.md),
                        false,
                        false,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_body_view(cx));
                    }),
                )
            });

        let body: AnyElement = match (self.selected, detail) {
            (None, _) => empty_state(
                DuckIcon::Mail,
                tr(cx, "选择一封邮件开始阅读"),
                Some(tr(cx, "从中间列表选一封邮件，正文会在这里显示。").into()),
                None,
                cx,
            )
            .into_any_element(),
            (Some(_), None) => empty_state(DuckIcon::Mail, tr(cx, "正在打开邮件…"), None, None, cx)
                .into_any_element(),
            (Some(_), Some(detail)) => self.render_message_detail(detail, cx),
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .bg(tokens.colors.background)
            .child(bar)
            .child(div().flex_1().w_full().min_h_0().flex().child(body))
            .into_any_element()
    }

    /// 阅读器正文：头部信息 + 正文 + 附件清单。
    fn render_message_detail(&self, detail: &MessageDetail, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let unknown = tr(cx, "(未知发件人)");

        // 这封邮件已贴的标签：在头部平铺成小胶囊（`accent_tag`）。增删入口在工具条的
        // 「打标签」菜单（那里有勾选态与「新建标签…」），这里只做展示。
        let mut chips = h_flex().w_full().flex_wrap().gap(tokens.spacing.xxs);
        for id in &self.message_labels {
            if let Some(label) = self.labels.iter().find(|l| l.id == *id) {
                chips = chips.child(accent_tag(label.name.clone(), cx));
            }
        }

        let header = v_flex()
            .flex_none()
            .w_full()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .w_full()
                    .text_size(tokens.typography.lg.size)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(
                        subject_or_placeholder(&detail.subject, tr(cx, "(无主题)")).to_string(),
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(tokens.spacing.xs)
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(
                        sender_display(
                            detail.from_name.as_deref(),
                            detail.from_addr.as_deref(),
                            unknown,
                        )
                        .to_string(),
                    ))
                    .child(div().flex_1())
                    .child(SharedString::from(format_full_time(detail.date)))
                    // 时间按 UTC 显示（见 `mail_data` 的说明），标出来免得用户
                    // 以为是本地时间。改成时区换算要先把依赖口径定下来。
                    .child(SharedString::from(tr(cx, "UTC"))),
            )
            .child(field_hint(
                trf(cx, "收件人：{}", &[&join_addresses(&detail.to)]),
                cx,
            ))
            .when(!detail.cc.is_empty(), |this| {
                this.child(field_hint(
                    trf(cx, "抄送：{}", &[&join_addresses(&detail.cc)]),
                    cx,
                ))
            })
            .when(detail.size.is_some(), |this| {
                this.child(field_hint(
                    trf(
                        cx,
                        "大小：{}",
                        &[&format_size(detail.size.unwrap_or_default())],
                    ),
                    cx,
                ))
            })
            .when(!self.message_labels.is_empty(), |this| this.child(chips));

        let mut column = v_flex()
            .id("mail-reader-scroll")
            .flex_1()
            .w_full()
            .min_h_0()
            .overflow_y_scroll()
            .child(header);

        // 正文区。形态已定（见 [`RenderedBody`]），这里只做分支。
        let body_area: AnyElement = match self.body.clone() {
            Some(RenderedBody::Html(html)) => {
                let mut area = v_flex().w_full().gap(tokens.spacing.sm);
                if self.body_has_remote_images {
                    area = area.child(self.render_remote_image_notice(cx));
                }
                area.child(
                    // ⚠️ 每封邮件一个**独立** ElementId：`TextView::html` 内部用
                    // `window.use_keyed_state(id + "/state")` 缓存解析结果，同一个 id
                    // 换了 html 文本**不会重新解析**（切邮件会显示上一封的正文）。
                    // 所以 id 必须从邮件主键派生。
                    TextView::html(SharedString::from(format!("mail-body-{}", detail.id)), html)
                        .selectable(true)
                        // 链接点击必须自己接：不接的话 `TextView` 走 `App::open_url`，
                        // 正文里写 `file:` / 自定义 scheme 就能让「点一下」落到系统
                        // 打开器（见 [`openable_link`]）。
                        .on_link_click(|url, event, window, cx| {
                            // 复刻库的默认语义：只有左键 / 中键 / 键盘激活才算「点了
                            // 链接」，右击不打开（接了处理器之后这个过滤得自己做）。
                            let activated = match event {
                                ClickEvent::Mouse(click) => matches!(
                                    click.up.button,
                                    MouseButton::Left | MouseButton::Middle
                                ),
                                ClickEvent::Keyboard(_) => true,
                                ClickEvent::Touch(click) => !click.long_press,
                            };
                            if !activated {
                                return;
                            }
                            if !openable_link(url) {
                                notify::error(
                                    tr(cx, "只允许打开 http、https 与 mailto 链接。"),
                                    window,
                                    cx,
                                );
                                return;
                            }
                            if let Err(error) = duck_platform::open_url(url) {
                                notify::error(
                                    trf(cx, "打不开链接：{}", &[&error.to_string()]),
                                    window,
                                    cx,
                                );
                            }
                        }),
                )
                .into_any_element()
            }
            Some(RenderedBody::Plain(text)) => div()
                .w_full()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .text_size(tokens.typography.sm.size)
                .line_height(tokens.typography.sm.line_height)
                .text_color(tokens.colors.foreground)
                .child(text)
                .into_any_element(),
            None if detail.body_state == "fetching" => {
                field_hint(tr(cx, "正在下载正文…"), cx).into_any_element()
            }
            // 没有正文，也没有缓存状态：说明这次没拉到。工具栏上的「下载正文」
            // 可以重试，所以这里只说明现状、不假装有内容。
            None => v_flex()
                .w_full()
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .gap(tokens.spacing.xs)
                .child(field_hint(tr(cx, "这封邮件的正文还没下载到本地。"), cx))
                .child(field_hint(
                    tr(cx, "点上方工具条的「下载正文」从服务器取一次。"),
                    cx,
                ))
                .into_any_element(),
        };
        column = column.child(body_area);

        if !detail.attachments.is_empty() {
            let mut list = v_flex()
                .w_full()
                .gap(px(2.))
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .border_t(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(caption_heading(tr(cx, "附件"), cx));
            for attachment in &detail.attachments {
                let attachment_id = attachment.id;
                // 下载中：入口换成状态字（不再可点，免得连点叠出几次下载）。
                let downloading = self.saving_attachment == Some(attachment_id);
                let failed = self
                    .attachment_error
                    .as_ref()
                    .is_some_and(|(id, _)| *id == attachment_id);
                // 本次会话里这个附件被「另存为」到哪了。没有就不给「在访达中显示」——
                // 还没落过盘就无处可显示，给个永远弹不出东西的按钮不如不给。
                let revealed = self
                    .saved_attachments
                    .iter()
                    .rev()
                    .find(|(id, _)| *id == attachment_id)
                    .map(|(_, path)| path.clone());
                let save = if downloading {
                    div()
                        .flex_none()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(tr(cx, "下载中"))
                        .into_any_element()
                } else {
                    // 每一行一个稳定的控件 id：同一封邮件里两个同名附件不能撞。
                    let view = self.view.clone();
                    let filename = attachment.filename.clone();
                    icon_button(
                        SharedString::from(format!("mail-attachment-save-{attachment_id}")),
                        tr(cx, "另存为…"),
                        Icon::new(DuckIcon::Download)
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground),
                        ButtonVariant::Ghost,
                        cx,
                    )
                    .on_click(move |_, _window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.save_attachment(attachment_id, &filename, cx);
                        });
                    })
                    .into_any_element()
                };
                list = list.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .child(
                            Icon::new(DuckIcon::Paperclip)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.text_tertiary),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(tokens.typography.xs.size)
                                .child(SharedString::from(attachment.filename.clone())),
                        )
                        // 失败态：行上留一个警示图标（原因已由提示条给出），再点「另存为」
                        // 即重试——不清掉这一行，用户不必去通知历史里翻原因。
                        .when(failed, |this| {
                            this.child(
                                Icon::new(DuckIcon::TriangleAlert)
                                    .size(extended.icon.sm)
                                    .text_color(tokens.colors.destructive),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .font_features(tabular_numbers())
                                .text_size(tokens.typography.xs.size)
                                .text_color(extended.colors.text_tertiary)
                                .child(SharedString::from(format_size(attachment.size))),
                        )
                        .when_some(revealed, |this, path| {
                            this.child(
                                icon_button(
                                    SharedString::from(format!(
                                        "mail-attachment-reveal-{attachment_id}"
                                    )),
                                    tr(cx, "在访达中显示"),
                                    Icon::new(DuckIcon::FolderOpen)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click(
                                    move |_, _window, _cx| duck_platform::reveal_in_finder(&path),
                                ),
                            )
                        })
                        .child(save),
                );
            }
            column = column.child(list);
        }

        column.into_any_element()
    }

    /// HTML 正文里含远程图片时的提示条。
    ///
    /// 远程图**默认放行**（用户拍板：图片默认要显示）。但这不是无脑放行——提示条
    /// 把「放行是有代价的」说清楚：追踪像素会让发件人知道你读过这封信。旁边一个
    /// 按钮让用户**显式**收回；收回后再按一次又放回来。
    fn render_remote_image_notice(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        let allow = self.allow_remote_images;
        let view = self.view.clone();
        h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(extended.colors.hover_tint)
            .child(
                Icon::new(DuckIcon::TriangleAlert)
                    .flex_none()
                    .size(extended.icon.md)
                    .text_color(extended.colors.warning),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(tr(
                        cx,
                        if allow {
                            "远程图片已显示，发件人可能由此得知你已读过这封信。"
                        } else {
                            "已拦截远程图片，打开可能向发件人暴露你的阅读状态。"
                        },
                    )),
            )
            .child(
                button(
                    "mail-remote-images",
                    tr(
                        cx,
                        if allow {
                            "隐藏远程图片"
                        } else {
                            "显示远程图片"
                        },
                    ),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_remote_images(cx));
                }),
            )
            .into_any_element()
    }

    /// 状态栏：左「当前状态」+ 右「账户同步态」。
    fn render_status_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        let sync_label: SharedString = match self.active_account() {
            Some(account) => {
                // 同步失败时状态栏直接给原因：用户不必翻设置才知道为什么没有新邮件。
                if account.sync_state == SyncState::Error {
                    if let Some(error) = account.sync_error.as_deref() {
                        SharedString::from(trf(
                            cx,
                            "{}：{}",
                            &[sync_state_label(account.sync_state, cx), error],
                        ))
                    } else {
                        SharedString::from(sync_state_label(account.sync_state, cx))
                    }
                } else {
                    SharedString::from(sync_state_label(account.sync_state, cx))
                }
            }
            None => SharedString::from(tr(cx, "未同步")),
        };

        h_flex()
            .flex_none()
            .w_full()
            .h(theme.density().status_bar)
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(self.status.clone()),
            )
            // 撤销条：动作入队后在这里出现，宽限期内点「撤销」等于取消那条命令；
            // 到期由事件轮询器收起并补发（[`Self::check_pending_flush`]）。
            .when_some(self.undo.as_ref(), |this, entry| {
                let view = self.view.clone();
                let text: SharedString = trf(cx, "{} · 撤销", &[entry.text.as_ref()]).into();
                this.child(
                    status_button("mail-undo", cx)
                        .child(status_button_content(
                            Some(DuckIcon::RotateCw),
                            Some(text),
                            None,
                            cx,
                        ))
                        .on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.undo_pending_action(cx));
                        }),
                )
            })
            .child(meta_text(sync_label, cx))
            .into_any_element()
    }
}

/// 左栏分组标题行（固定高度，与设计稿的 24 对齐）。
fn section_heading(label: &'static str, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    h_flex()
        .flex_none()
        .w_full()
        .h(theme.density().section_header)
        .items_center()
        .px(theme.tokens().spacing.xs)
        .child(caption_heading(label, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{
        KEY_CONTEXT, NewMessage, SEARCH_SYNTAX_ROWS, bind_keys, is_address, mailbox_field_text,
        openable_link, parse_mailbox, parse_mailbox_list, parse_port, quote_reply, reply_body,
        reply_seed, with_reply_prefix,
    };
    use duck_mail::{Mailbox, MessageDetail};
    // `Action::name()` 是 trait 方法，要对 `&dyn Action` 调用就得把它带进作用域。
    use gpui::{Action as _, TestAppContext};

    /// 造一封只填了「回信会用到的那几个字段」的邮件。
    fn detail(overrides: impl FnOnce(&mut MessageDetail)) -> MessageDetail {
        let mut detail = MessageDetail {
            id: 1,
            account_id: 1,
            thread_id: None,
            folder_id: Some(1),
            uid: Some(7),
            message_id: Some("<abc@example.com>".to_owned()),
            subject: "项目进度".to_owned(),
            from_name: Some("张三".to_owned()),
            from_addr: Some("zhangsan@example.com".to_owned()),
            to: vec!["me@example.com".to_owned()],
            cc: Vec::new(),
            date: 1_760_000_000,
            is_read: true,
            is_starred: false,
            has_attachments: false,
            size: Some(2048),
            body_state: "cached".to_owned(),
            text_body: Some("第一行\n\n第三行".to_owned()),
            html_body: None,
            attachments: Vec::new(),
        };
        overrides(&mut detail);
        detail
    }

    #[test]
    fn port_parsing_rejects_zero_and_garbage() {
        assert_eq!(parse_port("993"), Some(993));
        assert_eq!(parse_port(" 143 ".trim()), Some(143));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("abc"), None);
        // 超出 u16 的端口直接判非法，不截断。
        assert_eq!(parse_port("70000"), None);
    }

    #[test]
    fn only_web_and_mail_links_survive_the_whitelist() {
        // 放行档。
        assert!(openable_link("http://example.com/a"));
        assert!(openable_link("https://example.com/a?b=c#d"));
        assert!(openable_link("mailto:someone@example.com"));
        // scheme 大小写不敏感（RFC 3986）。
        assert!(openable_link("HTTPS://example.com"));
        assert!(openable_link("MailTo:x@y"));

        // 拦下档：交给系统打开器就等于「点一下执行本地动作」。
        assert!(!openable_link("file:///etc/passwd"));
        assert!(!openable_link("javascript:alert(1)"));
        assert!(!openable_link("data:text/html;base64,PHNjcmlwdD4="));
        assert!(!openable_link("smb://server/share"));
        // 相对链接：邮件里没有可解析的基准地址，交出去只会「没有程序认」。
        assert!(!openable_link("example.com/a"));
        assert!(!openable_link("/a/b"));
        assert!(!openable_link(""));
        // 形如 scheme 但不是（前导冒号 / 非法字符）。
        assert!(!openable_link(":http://x"));
        assert!(!openable_link("ht tp://x"));
    }

    // ------------------------------------------------------------------
    // 撰写弹窗的纯函数
    // ------------------------------------------------------------------

    #[test]
    fn mailbox_parsing_accepts_both_shapes() {
        let bare = parse_mailbox("a@b.com").expect("裸地址");
        assert_eq!(bare.address, "a@b.com");
        assert_eq!(bare.name, None);

        let named = parse_mailbox("张三 <a@b.com>").expect("名字 + 尖括号");
        assert_eq!(named.address, "a@b.com");
        assert_eq!(named.name.as_deref(), Some("张三"));

        // 通讯录里拷出来的写法常带引号。
        let quoted = parse_mailbox("\"李四\" <a@b.com>").expect("带引号的名字");
        assert_eq!(quoted.name.as_deref(), Some("李四"));

        // 名字里出现尖括号时取**最后**一对——地址永远在最后。
        let nested = parse_mailbox("A <test> B <a@b.com>").expect("名字里带尖括号");
        assert_eq!(nested.address, "a@b.com");
        assert_eq!(nested.name.as_deref(), Some("A <test> B"));

        // 前后空白一律忽略。
        assert_eq!(
            parse_mailbox("  a@b.com  ").map(|m| m.address),
            Some("a@b.com".to_owned())
        );

        // 这些不是地址，宁可丢也不要把垃圾塞进 `To:`。
        assert_eq!(parse_mailbox(""), None);
        assert_eq!(parse_mailbox("   "), None);
        assert_eq!(parse_mailbox("<>"), None);
        assert_eq!(parse_mailbox("张三 <>"), None);
        // 只有左尖括号：不补一个凭空的地址出来。
        assert_eq!(parse_mailbox("张三 <a@b.com"), None);
    }

    #[test]
    fn mailbox_list_splits_on_every_separator_people_paste() {
        let list = parse_mailbox_list("a@x.com, b@y.com; c@z.com，d@w.com\ne@v.com");
        let addresses: Vec<&str> = list.iter().map(|m| m.address.as_str()).collect();
        assert_eq!(
            addresses,
            ["a@x.com", "b@y.com", "c@z.com", "d@w.com", "e@v.com"],
            "中文逗号 / 分号 / 换行都要认，顺序不能乱"
        );

        // 空段（连着两个分隔符）被丢掉，不会变成一个空收件人。
        assert_eq!(parse_mailbox_list("a@x.com,,b@y.com").len(), 2);
        assert!(parse_mailbox_list("   ").is_empty());
        assert!(parse_mailbox_list("").is_empty());

        // 一段里带名字的照常解析。
        let named = parse_mailbox_list("张三 <a@x.com>, 李四 <b@y.com>");
        assert_eq!(named.len(), 2);
        assert_eq!(named[1].name.as_deref(), Some("李四"));
    }

    #[test]
    fn mailbox_field_text_round_trips_through_the_parser() {
        let with_name = Mailbox::with_name("a@b.com", "张三");
        assert_eq!(mailbox_field_text(&with_name), "张三 <a@b.com>");
        assert_eq!(
            parse_mailbox(&mailbox_field_text(&with_name)),
            Some(with_name)
        );

        // 名字只有空白 = 没有名字（否则会预填成 `  <a@b.com>`）。
        let blank_name = Mailbox {
            name: Some("   ".to_owned()),
            address: "a@b.com".to_owned(),
        };
        assert_eq!(mailbox_field_text(&blank_name), "a@b.com");

        let bare = Mailbox::new("a@b.com");
        assert_eq!(mailbox_field_text(&bare), "a@b.com");
        assert_eq!(parse_mailbox(&mailbox_field_text(&bare)), Some(bare));
    }

    #[test]
    fn quoting_marks_blank_lines_bare() {
        assert_eq!(quote_reply("第一行\n\n第三行"), "> 第一行\n>\n> 第三行");
        // 行尾空白削掉再引用——收信端的编辑器普遍会削，这里先削省得对面行尾多一个空格。
        assert_eq!(
            quote_reply("行尾有空格   \n下一行"),
            "> 行尾有空格\n> 下一行"
        );
        // 末尾换行不额外产出一行空引用。
        assert_eq!(quote_reply("只有一行\n"), "> 只有一行");
        assert_eq!(quote_reply(""), "");
        // 全是空行：每行都是裸 `>`。
        assert_eq!(quote_reply("\n\n"), ">\n>");
    }

    #[test]
    fn reply_body_is_dropped_when_there_is_nothing_to_quote() {
        // 正文没下载 / 只有空白：给空串，而不是留一句悬空的归属行。
        assert_eq!(reply_body("在 t，x 写道：", None), "");
        assert_eq!(reply_body("在 t，x 写道：", Some("   \n  ")), "");

        let body = reply_body("在 t，x 写道：", Some("第一行\n第二行"));
        assert_eq!(body, "\n\n在 t，x 写道：\n> 第一行\n> 第二行");
        // 归属行与引用块之间必须有换行，否则会拼成一行。
        assert!(body.starts_with("\n\n"));
    }

    #[test]
    fn reply_prefix_never_stacks() {
        assert_eq!(with_reply_prefix("回复：", "项目进度"), "回复：项目进度");
        // 已经是中文前缀：原样。
        assert_eq!(
            with_reply_prefix("回复：", "回复：项目进度"),
            "回复：项目进度"
        );
        // 对面用英文客户端回过：`Re:` 也认，否则来回两轮就叠成两层。
        assert_eq!(with_reply_prefix("回复：", "Re: 项目进度"), "Re: 项目进度");
        assert_eq!(with_reply_prefix("回复：", "RE: 项目进度"), "RE: 项目进度");
        // 空主题：给一个只有前缀的主题。中文前缀尾无空白，原样保留；英文前缀的
        // 尾随空格会被削掉（`"Re: "` → `"Re:"`）——不然会是一个只有空格的尾巴。
        assert_eq!(with_reply_prefix("回复：", "   "), "回复：");
        assert_eq!(with_reply_prefix("Re: ", "   "), "Re:");
        // 前缀的「已带」判定也要按削尾后的形态走，否则 `Re: ` 永远判不出来。
        assert_eq!(with_reply_prefix("Re: ", "Re: 项目进度"), "Re: 项目进度");
        // `Re:` 必须带冒号才算前缀——「Report」不是回复。
        assert_eq!(with_reply_prefix("回复：", "Report"), "回复：Report");
    }

    #[test]
    fn reply_seed_fills_recipient_subject_and_thread_headers() {
        let seed = reply_seed(&detail(|_| {}), "回复：", "在 t，张三 写道：");

        // 收件人 = 原发件人（带显示名），抄送为空（不回全体）。
        assert_eq!(seed.to.len(), 1);
        assert_eq!(seed.to[0].address, "zhangsan@example.com");
        assert_eq!(seed.to[0].name.as_deref(), Some("张三"));
        assert!(seed.cc.is_empty());

        assert_eq!(seed.subject, "回复：项目进度");
        // `In-Reply-To` 与 `References` 都指向原邮件；`compose` 负责规整尖括号。
        assert_eq!(seed.in_reply_to.as_deref(), Some("<abc@example.com>"));
        assert_eq!(seed.references, ["<abc@example.com>"]);
        assert_eq!(seed.body, "\n\n在 t，张三 写道：\n> 第一行\n>\n> 第三行");
    }

    #[test]
    fn reply_seed_tolerates_missing_sender_and_uncached_body() {
        // 没有发件人地址（退信 / 脏数据）：不预填收件人，让用户自己填。
        let seed = reply_seed(
            &detail(|d| {
                d.from_addr = None;
                d.from_name = None;
            }),
            "回复：",
            "在 t，x 写道：",
        );
        assert!(seed.to.is_empty());

        // 正文没下载：只有收件人与主题，正文留空。
        let seed = reply_seed(
            &detail(|d| {
                d.body_state = "none".to_owned();
                d.text_body = None;
            }),
            "回复：",
            "在 t，x 写道：",
        );
        assert_eq!(seed.body, "");
        assert_eq!(seed.subject, "回复：项目进度");

        // 没有 `Message-ID`：两个线程头都不带，而不是塞一个空串。
        let seed = reply_seed(&detail(|d| d.message_id = None), "回复：", "在 t，x 写道：");
        assert_eq!(seed.in_reply_to, None);
        assert!(seed.references.is_empty());

        // 空白 `Message-ID` 与没有同等对待。
        let seed = reply_seed(
            &detail(|d| d.message_id = Some("   ".to_owned())),
            "回复：",
            "在 t，x 写道：",
        );
        assert_eq!(seed.in_reply_to, None);
    }

    #[test]
    fn address_validation_needs_exactly_one_at() {
        assert!(is_address("a@b.com"));
        // 这里刻意宽松：`.` / `+` / 国际化域名都放过，拦的是「填错了」这一档。
        assert!(is_address("first.last+tag@sub.example.co.uk"));
        assert!(is_address("用户@例子.中国"));

        assert!(!is_address("a@b@c"));
        assert!(!is_address("@b.com"));
        assert!(!is_address("a@"));
        assert!(!is_address("张三"));
        assert!(!is_address("a b@c.com"));
        assert!(!is_address(""));
    }

    /// 页内键位必须真的绑在 `"Mail"` 上下文里（同 `db.rs` 的同名守卫）。
    ///
    /// 为什么测绑定而不是测行为：`bind_keys` 写错（上下文漏了、键名拼错）时**界面照样
    /// 编译、按钮照样能点**，只有把键位表读回来才看得见。不断言「不与别处相撞」——
    /// 那是 `duck_command::bind_all` 的职责（会打 `keybinding conflict` 警告）；
    /// 这里只保证「声明出来的键位确实生效」。
    #[gpui::test]
    fn the_compose_key_is_bound_in_the_mail_context(cx: &mut TestAppContext) {
        cx.update(bind_keys);
        cx.update(|cx| {
            let compose = NewMessage;
            let bindings = cx.key_bindings();
            let keymap = bindings.borrow();
            let binding = keymap
                .bindings_for_action(&compose as &dyn gpui::Action)
                .next()
                .unwrap_or_else(|| panic!("`{}` 没有绑定键位", compose.name()));

            assert_eq!(
                binding
                    .predicate()
                    .map(|predicate| predicate.to_string())
                    .as_deref(),
                Some(KEY_CONTEXT),
                "撰写必须绑在页面上下文里，否则焦点稍深一点就失效"
            );
            let strokes = binding.keystrokes();
            assert_eq!(strokes.len(), 1, "应当是单笔画键位");
            let stroke = &strokes[0];
            assert_eq!(stroke.key(), "n", "`⌘N` 的键名不对");
            assert!(
                stroke.modifiers().secondary(),
                "必须带平台主修饰键（⌘ / Ctrl）"
            );
            assert!(!stroke.modifiers().shift, "`⌘N` 不带 Shift");
        });
    }

    /// 搜索语法表的硬约束（纯静态，不需要 `App`）。
    ///
    /// ① 左列（要用户照抄的写法）**一个字都不许带中文**。这不是审美要求：i18n 守卫
    ///    是从源码行里逐字取引号之间的**原文**（`\"两个词\"` 取出来就是带反斜杠的
    ///    `\"两个词\"`）再去表里查，而表里的键是**已解析**的（`"两个词"`）——两者永远
    ///    对不上，所以左列一旦出现中文，`every_chinese_literal_is_translated` 必红，
    ///    而且没法靠登记条目消掉。把这条钉在测试里，好过留给下一个加语法的人去撞。
    /// ② 写法与说明都不许为空：只有写法没有解释、或反过来，这一行都等于没写。
    /// ③ 关键词类语法必须齐 —— 少一条就是「内核支持、界面不说」，用户不可能猜出来。
    ///    （右列的中文是否已进 EN 表由 `duck-theme` 的守卫管，这里不重复。）
    #[test]
    fn search_syntax_rows_keep_the_ui_translatable() {
        assert!(!SEARCH_SYNTAX_ROWS.is_empty(), "空表等于没有帮助");
        for (syntax, source) in SEARCH_SYNTAX_ROWS {
            assert!(
                !syntax.chars().any(is_cjk),
                "语法列不许出现中文（会顶掉 i18n 守卫）：{syntax:?}"
            );
            assert!(
                !syntax.trim().is_empty() && !source.is_empty(),
                "写法与源串都不许为空：{syntax:?}"
            );
        }

        let keys: Vec<&str> = SEARCH_SYNTAX_ROWS.iter().map(|row| row.0).collect();
        for expected in [
            "from:",
            "to:",
            "subject:",
            "folder:",
            "is:unread",
            "is:starred",
            "has:attachment",
            "before:2026-01-01",
            "after:2026-01-01",
        ] {
            assert!(keys.contains(&expected), "语法表漏了 {expected}");
        }
    }

    /// 与 i18n 守卫 `contains_cjk` 同一判据（CJK 统一表意文字基本区）。
    fn is_cjk(ch: char) -> bool {
        ('\u{4e00}'..='\u{9fff}').contains(&ch)
    }
}
