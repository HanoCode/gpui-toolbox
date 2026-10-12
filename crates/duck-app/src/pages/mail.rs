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
//! | 阅读器回复/转发/归档/稍后/删除/打标签/旗标/AI 分析/更多 | 都没有（M2 的收发闭环） | 删除 |
//! | 右栏 AI 面板 | 没有邮件 AI 后端（M2） | 删除 |
//!
//! 这一版**不留任何禁用控件**。M2 把发信、动作队列、AI 接通后，这些入口再按同一份
//! 判据接回来——那时它们才有真功能可挂。

use std::ops::RangeInclusive;
use std::rc::Rc;

use duck_components::{
    ButtonVariant, ConfirmHandler, ControlExt as _, DuckIcon, SidebarPanel, SidebarState, button,
    caption_heading, content::empty_state, dialog_top_offset, field_hint, form_field, form_gap,
    form_row, icon_button, meta_text, modal_drag, modal_drag_area, modal_offset, modal_title_row,
    notify, segmented_tabs, tabular_numbers, toolbar_action_button,
};
use duck_mail::{
    Account, ConnectionSecurity, Folder, FolderRole, MailCore, MessageDetail, MessageSummary,
    NewAccount, SyncState,
};
use duck_theme::{active_theme, tr, trf};
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _, Styled, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, WindowExt as _,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{Input, InputState},
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
const PREVIEW_LIMIT: i64 = 300;
/// 账户弹窗宽度。
const ACCOUNT_DIALOG_WIDTH: f32 = 620.;
/// 对话框正文里 IMAP / SMTP 两列主机端口的比例（端口窄、主机宽）。
const PORT_FIELD_WIDTH: f32 = 88.;

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

    /// 全部账户。
    accounts: Vec<Account>,
    /// 当前账户。
    active_account: Option<i64>,
    /// 当前账户的文件夹（已按角色排序）。
    folders: Vec<Folder>,
    /// 当前文件夹。
    active_folder: Option<i64>,

    /// 当前文件夹的邮件列表。
    messages: Vec<MessageSummary>,
    /// 当前选中的邮件主键。
    selected: Option<i64>,
    /// 选中邮件的详情。
    detail: Option<MessageDetail>,
    /// 详情对应的正文（已渲染成纯文本）。与 `detail` 分开存是因为 HTML 正文
    /// 要过一次 [`duck_mail::mime::html_to_text`]，**不能在 render 里做**——
    /// 那是每帧一次的全量转换。
    body: Option<SharedString>,

    /// 有后台任务在跑（列表条/阅读器据此显示忙碌态）。
    busy: bool,
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

        let mut this = Self {
            sidebar,
            view: cx.entity().downgrade(),
            core: None,
            core_error: None,
            accounts: Vec::new(),
            active_account: None,
            folders: Vec::new(),
            active_folder: None,
            messages: Vec::new(),
            selected: None,
            detail: None,
            body: None,
            busy: false,
            status: tr(cx, "正在打开邮箱数据库…").into(),
        };
        // `window` 只用于满足签名一致性（本页没有需要窗口才能构造的子状态）；
        // 显式 `let _` 而不是改名，是为了让调用点（`app.rs`）与其它页保持一致。
        let _ = window;
        this.open_core(cx);
        this
    }

    /// 供 shell 挂左栏折叠态。
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
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
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { work() });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let value = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.busy = false;
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
            self.messages.clear();
            self.detail = None;
            self.body = None;
            return;
        };
        let keep = self.active_folder;
        self.spawn(
            cx,
            move || core.folders(account_id),
            move |this, result, _window, cx| match result {
                Ok(mut folders) => {
                    // 排序放在界面：内核只按主键给，界面要的是「常用在前」。
                    folders.sort_by_key(|f| (f.role.order(), f.name.clone()));
                    this.folders = folders;
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
                }
                Err(error) => {
                    this.status = trf(cx, "读取文件夹失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 拉当前文件夹的邮件列表。
    fn reload_messages(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(folder_id) = self.active_folder else {
            self.messages.clear();
            return;
        };
        self.spawn(
            cx,
            move || core.messages(folder_id, PREVIEW_LIMIT, 0),
            |this, result, _window, cx| match result {
                Ok(messages) => {
                    // 选中的那封可能已经不在这一页里（换了文件夹）。
                    if !this
                        .selected
                        .is_some_and(|id| messages.iter().any(|m| m.id == id))
                    {
                        this.selected = None;
                        this.detail = None;
                        this.body = None;
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
    }

    // ------------------------------------------------------------------
    // 交互
    // ------------------------------------------------------------------

    /// 切换账户。
    fn select_account(&mut self, account_id: i64, cx: &mut Context<Self>) {
        if self.active_account == Some(account_id) {
            return;
        }
        self.active_account = Some(account_id);
        // 换了账户，上一个账户的文件夹与列表全部作废（不然会显示成新账户的）。
        self.active_folder = None;
        self.folders.clear();
        self.messages.clear();
        self.selected = None;
        self.detail = None;
        self.body = None;
        cx.notify();
        self.reload_folders(cx);
    }

    /// 切换文件夹。
    fn select_folder(&mut self, folder_id: i64, cx: &mut Context<Self>) {
        if self.active_folder == Some(folder_id) {
            return;
        }
        self.active_folder = Some(folder_id);
        self.messages.clear();
        self.selected = None;
        self.detail = None;
        self.body = None;
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
        self.body = None;
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
                // 正文没缓存就顺手拉一次；拉不到也把详情给出去（界面会说「没拉到」）。
                let detail = if detail.body_state == "cached" {
                    detail
                } else {
                    core.fetch_message_body(message_id).unwrap_or(detail)
                };
                Ok(detail)
            },
            move |this, result: Result<MessageDetail, duck_mail::MailError>, _window, cx| {
                match result {
                    Ok(detail) => {
                        if mark_read {
                            // 顺手把列表那一行也标成已读，不必再拉一次列表。
                            if let Some(row) = this.messages.iter_mut().find(|m| m.id == message_id)
                            {
                                row.is_read = true;
                            }
                        }
                        this.set_detail(detail, cx);
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

    /// 把详情转成界面状态：正文在这里就渲染成纯文本**一次**。
    fn set_detail(&mut self, detail: MessageDetail, cx: &mut Context<Self>) {
        let text = detail
            .text_body
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                // 只有 HTML 版本（营销邮件、Newsletter 常见）时压成纯文本。
                // M1 没有 HTML 渲染器，这一步是内核的解码职责（可单测），
                // 见 `duck_mail::mime::html_to_text`。
                detail
                    .html_body
                    .as_deref()
                    .map(duck_mail::mime::html_to_text)
                    .filter(|s| !s.trim().is_empty())
            });
        self.body = text.map(SharedString::from);
        self.status = trf(
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
        )
        .into();
        self.detail = Some(detail);
    }

    /// 切换选中邮件的已读状态。
    fn toggle_read(&mut self, cx: &mut Context<Self>) {
        let Some(message_id) = self.selected else {
            return;
        };
        let Some(core) = self.core.clone() else {
            return;
        };
        let next = self.detail.as_ref().map(|d| !d.is_read).unwrap_or(true);
        self.spawn(
            cx,
            move || core.set_message_read(message_id, next),
            move |this, result, _window, cx| match result {
                Ok(()) => {
                    if let Some(row) = this.messages.iter_mut().find(|m| m.id == message_id) {
                        row.is_read = next;
                    }
                    if let Some(detail) = this.detail.as_mut() {
                        detail.is_read = next;
                    }
                    this.status = if next {
                        tr(cx, "已标为已读").into()
                    } else {
                        tr(cx, "已标为未读").into()
                    };
                }
                Err(error) => {
                    this.status = trf(cx, "更新已读状态失败：{}", &[&error.to_string()]).into();
                }
            },
        );
    }

    /// 同步当前账户（真连服务器）。
    fn sync_now(&mut self, cx: &mut Context<Self>) {
        let Some(core) = self.core.clone() else {
            return;
        };
        let Some(account_id) = self.active_account else {
            return;
        };
        self.status = tr(cx, "正在同步…").into();
        self.spawn(
            cx,
            move || core.sync_account(account_id),
            |this, result, window, cx| match result {
                Ok(report) => {
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
                    // 拉完刷新账户（同步状态变了）与当前视图。
                    this.reload_accounts(cx);
                }
                Err(error) => {
                    // 失败原因已经由内核写进 `accounts.sync_error`，这里只说结论。
                    let text = trf(cx, "同步失败：{}", &[&error.to_string()]);
                    this.status = text.clone().into();
                    notify::error(text, window, cx);
                    this.reload_accounts(cx);
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

    /// 打开「添加邮箱账户」弹窗。
    fn open_account_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| AccountForm::new(window, cx));
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title: SharedString = tr(cx, "添加邮箱账户").into();

        // ⏎ 与底部「添加」共用同一份保存判断（`ConfirmHandler` 口径，
        // 同 `RedisPage::open_form_dialog`）。
        let confirm: ConfirmHandler = {
            let form = form.clone();
            let view = view.clone();
            Rc::new(move |window, cx| {
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
                        .ok_text(tr(cx, "添加"))
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
        self.spawn(
            cx,
            move || core.add_account(&new),
            move |this, result, window, cx| match result {
                Ok(_id) => {
                    work_form.update(cx, |form, cx| {
                        form.error = None;
                        cx.notify();
                    });
                    notify::success(tr(cx, "账户已添加，点「同步」拉取邮件"), window, cx);
                    window.close_dialog(cx);
                    // 新账户还没有文件夹，先选中它（`reload_accounts` 会挑第一个）。
                    this.active_account = None;
                    this.reload_accounts(cx);
                }
                Err(error) => {
                    // 落库/凭据失败：原因可能是「该邮箱已存在」或保管箱不可写——
                    // 都贴在弹窗里，用户改一下就能重试。
                    let text: SharedString = trf(cx, "添加失败：{}", &[&error.to_string()]).into();
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
        self.spawn(
            cx,
            move || core.test_connection(&new),
            move |_this, result, _window, cx| {
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
            },
        );
    }
}

// ---------------------------------------------------------------------------
// 账户弹窗的视图状态
// ---------------------------------------------------------------------------

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
            testing: false,
            test_result: None,
            error: None,
        }
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
            return Err(tr(cx, "请填写口令。").into());
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

/// 弹窗正文：账户身份 + 口令 + IMAP / SMTP 两栏 + 加密方式。
///
/// 输入框一律 `.control(cx)`（`duck_components::ControlExt`）：它把高度收到
/// `density.control`、字号收到 `typography.sm`，与全仓表单输入同规格。裸
/// `Input::new()` 会带库自己的尺寸与配色，在同一张弹窗里一眼看出是两套东西。
fn render_account_form(form: &Entity<AccountForm>, cx: &App) -> AnyElement {
    let state = form.read(cx);

    let mut body = v_flex()
        .w_full()
        .gap(form_gap(cx))
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
        .child(security_picker(&state, form, cx));

    if let Some(result) = &state.test_result {
        body = body.child(match result {
            Ok(text) => field_hint(text.clone(), cx),
            Err(text) => field_hint(trf(cx, "连接失败：{}", &[text]), cx),
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

/// 弹窗脚注：左「测试连接」、右「取消 / 添加」。
fn account_footer(
    form: &Entity<AccountForm>,
    view: &WeakEntity<MailPage>,
    cx: &App,
) -> DialogFooter {
    let test_view = view.clone();
    let test_form = form.clone();
    let ok_view = view.clone();
    let ok_form = form.clone();
    let theme = active_theme(cx);
    let spacing = theme.tokens().spacing;
    let testing = form.read(cx).testing;
    let test_label = if testing {
        tr(cx, "正在测试…")
    } else {
        tr(cx, "测试连接")
    };

    let left = h_flex().gap(spacing.sm).child(
        button(
            "mail-account-test",
            test_label,
            ButtonVariant::Secondary,
            cx,
        )
        .disabled(testing)
        .on_click(move |_, window, cx| {
            let _ = test_view.update(cx, |this, cx| this.test_account(&test_form, window, cx));
        }),
    );

    let right = h_flex().gap(spacing.sm).children({
        let mut items: Vec<AnyElement> = Vec::new();
        items.push(
            button(
                "mail-account-cancel",
                tr(cx, "取消"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx))
            .into_any_element(),
        );
        items.push(
            button(
                "mail-account-ok",
                tr(cx, "添加"),
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

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

impl Render for MailPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self.render_main(cx);

        let mut panel = SidebarPanel::new("mail-content", &self.sidebar, content, window, cx);
        if panel.is_sidebar_visible() {
            panel = panel.sidebar(self.render_sidebar(cx));
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
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
                    let _ = view.update(cx, |this, cx| this.open_account_dialog(window, cx));
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
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.select_account(layout, cx));
            },
            cx,
        )
    }

    fn folder_row(&self, folder: &Folder, cx: &App) -> AnyElement {
        let layout = folder.id;
        let view = self.view.clone();
        // `Other` 用服务端原名（那是用户自己起的），认出来的角色用本地化名。
        let name: SharedString = if folder.role == FolderRole::Other {
            folder.name.clone().into()
        } else {
            role_label(folder.role, cx).into()
        };
        let aria: SharedString = if folder.unread > 0 {
            trf(
                cx,
                "文件夹 {}，{} 封未读",
                &[&name.to_string(), &folder.unread.to_string()],
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
            Some(folder.unread),
            self.active_folder == Some(folder.id),
            move |_, _, cx| {
                let _ = view.update(cx, |this, cx| this.select_folder(layout, cx));
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

        let folder_name: SharedString = self
            .active_folder
            .and_then(|id| self.folders.iter().find(|f| f.id == id))
            .map(|folder| {
                if folder.role == FolderRole::Other {
                    folder.name.clone().into()
                } else {
                    SharedString::from(role_label(folder.role, cx))
                }
            })
            .unwrap_or_else(|| SharedString::from(tr(cx, "邮箱")));

        // 列表条：文件夹名 + 计数 | 同步（真连服务器）。
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
                    .text_size(tokens.typography.sm.size)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(folder_name),
            )
            .child(
                div()
                    .font_features(tabular_numbers())
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(SharedString::from(
                        (!self.messages.is_empty())
                            .then(|| self.messages.len().to_string())
                            .unwrap_or_default(),
                    )),
            )
            .child(div().flex_1())
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

        let body: AnyElement = if self.active_account.is_none() {
            empty_state(
                DuckIcon::Inbox,
                tr(cx, "还没有邮箱账户"),
                Some(tr(cx, "用左栏的「+」添加账户，再点「同步」拉取邮件。").into()),
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
            let rows = self.messages.iter().map(|m| self.message_row(m, cx));
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
        // 读屏读不出「加粗」与「小圆点」，所以未读状态必须进可访问名。
        let aria: SharedString = trf(
            cx,
            if message.is_read {
                "邮件：{}，来自 {}，{}"
            } else {
                "未读邮件：{}，来自 {}，{}"
            },
            &[&subject.to_string(), &sender.to_string(), &time.to_string()],
        )
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

    /// 阅读器：工具条 + 头部 + 正文 + 附件。
    fn render_reader(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        // 工具条只有**真能点**的两个动作：标为已读/未读、下载正文（未缓存时）。
        // 其余动作（回复 / 归档 / 删除 / 打标签 / AI 分析 …）的前提在 M2，按 P3-6
        // 一律不占位——见模块头那张表。
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
            .child(div().flex_1());

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
            });

        let mut column = v_flex()
            .id("mail-reader-scroll")
            .flex_1()
            .w_full()
            .min_h_0()
            .overflow_y_scroll()
            .child(header);

        // 正文区。
        let body_area: AnyElement = match self.body.clone() {
            Some(text) => div()
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
                        .child(
                            div()
                                .flex_none()
                                .font_features(tabular_numbers())
                                .text_size(tokens.typography.xs.size)
                                .text_color(extended.colors.text_tertiary)
                                .child(SharedString::from(format_size(attachment.size))),
                        ),
                );
            }
            column = column.child(list);
        }

        column.into_any_element()
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
    use super::parse_port;

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
}
