//! SSH 终端页：连接列表（左栏）+ 会话标签页 + 终端画布 / SFTP 双栏 + 状态栏（右栏会话详情）。
//!
//! **三种标签都是真的**：
//! - `TabKind::Local` —— `duck-term`（真实 PTY + alacritty 网格）；
//! - `TabKind::Ssh` —— 同一套 `duck-term` 内核，只是 `SpawnOptions.shell` 换成
//!   `ssh -t <destination>`（见 [`crate::pages::terminal_canvas::TerminalSession::spawn_ssh`]）；
//! - `TabKind::Sftp` —— 不走 PTY，每次远端操作是一次 `duck_ssh::exec`（靠 OpenSSH 的
//!   `ControlMaster` 复用底层连接，与终端共用同一条物理连接）。
//!
//! 认证（`~/.ssh/config` / agent / 公钥 / 口令 / ProxyJump）与主机密钥（`known_hosts`）
//! 全部交给系统 `ssh` 继承，**不自研**。连接档案落 `kv: ssh_profiles`（口令在其中
//! 以 AES-256-GCM 密文存在，见 `duck_platform::vault`）。远端列举用远端 `sh` 输出自定制表符格式，**不解析 `ls -l`**
//!（其时间格式随 `LC_*` 与平台变）。
//!
//! 设计文档：`docs/terminal-ssh-sftp-design.md`（§4.3 记录了「路线 (a) 不成立 → 采用 (b)
//! 驱动系统 `ssh` / `scp`」的实测裁决）。
//!
//! 布局沿用本仓约定：左栏走页面内的 [`SidebarPanel`]（shell 只持有它的折叠态与顶栏开关），
//! 右栏走 shell 级 `with_right_sidebar`（本页只提供内容闭包）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use duck_components::{
    ButtonVariant, DuckIcon, SidebarPanel, SidebarState, body_text, button, caption_heading, card,
    content::{
        accent_tag, description_list, description_row, empty_state, neutral_tag, progress_bar,
        status_cell,
    },
    dialog_top_offset, icon_button, meta_text, modal_drag, modal_drag_area, modal_offset,
    modal_title_row, notify, sidebar_navigation_button, tabular_numbers, toolbar_action_button,
};
use duck_theme::{Lang, active_theme, t, tf, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DragMoveEvent, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_component::{
    Icon, Sizable as _, Size, WindowExt as _,
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    v_flex,
};

use crate::pages::status_support::format_bytes;
use crate::pages::terminal_canvas::{CLEAR_SCREEN, TerminalSession};
use crate::pages::terminal_data::{
    self as data, AuthKind, Connection, DirEntry, Group, LinkStatus, SessionTab, Side,
    StoredProfile, TabKind, Transfer,
};
use crate::pages::terminal_dialogs::{
    NewConnection, open_connection_dialog, open_directory_dialog,
};

/// 左栏 / 标签页共用的侧栏默认宽度。
const SIDEBAR_DEFAULT_WIDTH: f32 = 236.;
/// 左栏最小宽度。
const SIDEBAR_MIN_WIDTH: f32 = 184.;
/// 左栏最大宽度。
const SIDEBAR_MAX_WIDTH: f32 = 340.;
/// 单个标签页的最大宽度。
const TAB_MAX_WIDTH: f32 = 196.;
/// 文件列表里「大小」列宽。
const SIZE_COLUMN_WIDTH: f32 = 84.;
/// 文件列表里「修改时间」列宽。
const MODIFIED_COLUMN_WIDTH: f32 = 96.;
/// 传输行里「文件名」车道宽度。
///
/// 与进度条、百分比、状态角标同行：定宽让四种元素的列位在每行都落在同一条竖线上
/// （行与行之间可比，靠的是列位稳定，不是各自自适应）。
const TRANSFER_NAME_WIDTH: f32 = 220.;
/// **数值读数**（「20 px」「100%」）的固定车道宽度。
///
/// 读数的位数会变（`9 px` → `20 px`、`7%` → `100%`），车道不定宽时相邻的
/// ± 按钮与进度条会跟着左右跳。取最宽读数（3 位数字 + 单位）再留一格余量。
const READOUT_WIDTH: f32 = 44.;
/// 标签页 / 状态栏上的连接状态点直径。
///
/// 状态点不在图标阶梯（12/14/16）里——它是**色块**不是图标，比最小的图标还小一档，
/// 才不至于把标签文字挤出去。颜色 + 位置共同表达状态（色盲用户靠位置与文字兜底）。
const STATUS_DOT_SIZE: f32 = 6.;
/// 终端字号的允许区间（`⌘-` / `⌘+` 与工具条按钮都钳在这里）。
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 10.0..=20.0;
/// 确认弹窗宽度：只有一句标题 + 一句说明，够放下就行。
const CONFIRM_DIALOG_W: f32 = 420.;

/// 一条远端会话的句柄：目标 + 复用 socket 目录。
///
/// 页面里所有远端操作（列举、上传、下载）都靠它构造命令行。
/// `Clone` 是给后台任务用的 —— 任务活不过页面，但要在自己的闭包里持有它。
#[derive(Clone)]
struct RemoteHandle {
    /// 连接目标（主机、端口、用户、认证）。
    target: duck_ssh::Target,
    /// `ControlMaster` 复用 socket 的目录。
    socket_dir: PathBuf,
}

/// 一次传输的全部输入。
///
/// 打包成一个结构而不是八个参数：这些值要在 `cx.spawn` 的闭包里**跨线程**走一趟，
/// 一个个捕获既啰嗦，也容易漏掉一个（漏掉的那个会在编译期以「找不到变量」出现，
/// 但找错方向更浪费时间）。
#[derive(Clone)]
struct TransferRequest {
    /// 队列行 id（结果回填用）。
    id: u64,
    /// 方向。
    upload: bool,
    /// 上传时的本地源文件（`scp` 的第一个位置参数）。
    local_file: PathBuf,
    /// 下载时的本地落点目录（`scp` 的第二个位置参数）。
    local_dir: PathBuf,
    /// 上传时的远端落点目录。
    remote_dir: String,
    /// 下载时的远端源文件路径。
    remote_file: String,
}

/// 需要用户确认后才执行的破坏性动作。
///
/// 参数在构造时**冻结**（不持有对连接表的引用）：确认框可能开着好几秒，
/// 期间用户完全可能去改列表 —— 那时执行的动作必须还是他按下确认的那一个。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    /// 删除一条连接档案。
    DeleteConnection { id: String, label: String },
}

impl PendingAction {
    /// 动作涉及的连接显示名（给确认文案用）。
    fn label(&self) -> String {
        match self {
            Self::DeleteConnection { label, .. } => label.clone(),
        }
    }
}

/// 弹窗口令按保存口径的「应存明文」。
///
/// 只有「密码认证 + 勾记住 + 填了值」才算一条要保存的口令；其余情形都是
/// `None`（清除）。判据单放一份：[`apply_password`] 落库、[`TerminalPage::apply_edit`]
/// 判「口令变没变」都对着它，两处永远不会各改各的。
fn draft_password_value(draft: &NewConnection) -> Option<String> {
    (draft.auth == AuthKind::Password && draft.remember && !draft.password.is_empty())
        .then(|| draft.password.clone())
}

/// 把弹窗口令落到档案的密文字段（按弹窗口径）。
///
/// - 认证方式不是密码，或没勾「记住口令」→ 清掉（`None`）：
///   留着就是给已改掉的认证方式留一份秘密，开关的语义是「这个连接**不**保存口令」。
/// - 填了值 + 勾「记住口令」→ AES-256-GCM 加密后存进档案行
///（[`duck_platform::vault`]；SQLite 里 grep 不到明文，编辑弹窗能解密回显）。
/// - 加密失败（密钥文件写不出等）：口令不保存、连接退回终端里手输 ——
///   弹一个「保存失败」只会把用户卡在一个没有下一步可做的提示框前。
///
/// 编辑弹窗**回显已存口令**（解密回填），所以空值就是「清除口令」的明确意图，
/// 不再有「空 = 不修改」的旧口径。
fn apply_password(profile: &mut StoredProfile, draft: &NewConnection) {
    profile.password = draft_password_value(draft)
        .as_deref()
        .map(duck_platform::vault::seal)
        .transpose()
        .ok()
        .flatten();
}

/// 读出档案里保存的口令明文（解密）；没存过 / 解不开都是 `None`。
///
/// 解不开（换了机器、密钥文件被删）时退回 ssh 自己的交互提示 ——
/// 至少不会拿一个错误的值去试。
fn open_password(profile: &StoredProfile) -> Option<String> {
    let sealed = profile.password.as_deref()?;
    duck_platform::vault::open(sealed)
        .ok()
        .filter(|password| !password.is_empty())
}

/// 「打开 SFTP 面板」的目标解析：当前标签所属的连接优先，其次上次开过的那条。
///
/// 拆成纯函数只为一件可观测的事能被单测钉住：**面板关掉之后目标不能丢**。
/// 之前这里只看当前标签，于是「开 SFTP → 关掉 → 再点图标」什么也不会发生
///（活动标签已回落到本地 shell / 空态）。
fn resolve_sftp_owner(
    active: Option<&str>,
    last: Option<&str>,
    usable: &dyn Fn(&str) -> bool,
) -> Option<String> {
    [active, last]
        .into_iter()
        .flatten()
        .find(|id| usable(id))
        .map(str::to_owned)
}

/// 把一段时长说成人话（`2 小时 5 分` / `8 分 12 秒` / `46 秒`）。
///
/// 只精确到需要的那一档：连接了三天的人不关心「还有 17 秒」，
/// 而刚连上的人最想看的就是秒数在动。
///
/// 走 `tf` 而不是 `format!`：这三条模板进的是英译表，
/// 用 `format!` 的话英文界面里会长出「2 小时 5 分」。
fn human_duration(total: std::time::Duration, lang: Lang) -> String {
    let seconds = total.as_secs();
    let (hours, minutes, rest) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    if hours > 0 {
        tf(
            lang,
            "{hours} 小时 {minutes} 分",
            &[&hours.to_string(), &minutes.to_string()],
        )
    } else if minutes > 0 {
        tf(
            lang,
            "{minutes} 分 {rest} 秒",
            &[&minutes.to_string(), &rest.to_string()],
        )
    } else {
        tf(lang, "{rest} 秒", &[&rest.to_string()])
    }
}

/// 把内核错误转成当前语言的用户说明。
///
/// `duck-ssh` 零依赖、不认识 `duck-theme`，只交出「源串 + 参数」（`SshError::template`），
/// 换语言这一步只能在界面层做 —— 否则英文界面里会冒出中文报错。
fn ssh_error_text(cx: &App, error: &duck_ssh::SshError) -> String {
    let (source, args) = error.template();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tf(active_theme(cx).lang(), source, &args)
}

/// 标签拖拽的载荷类型（`on_drag` / `on_drop` / `on_drag_move` 靠它对上暗号）。
///
/// 只带标签 id：拖拽期间下标会变（标签栏重排），id 才是稳定标识。
pub(super) struct TabDrag {
    /// 被拖拽的标签 id。
    pub(super) tab_id: String,
}

/// 拖拽时跟在指针后面的幽灵缩略图。
///
/// gpui 的 `on_drag` 要求现场构造一个可渲染视图；给一个圆角小卡片
///（图标 + 标签名），比拖着一块透明区域更有「我在拖一个标签」的实感。
struct TabDragGhost {
    /// 被拖标签的名字。
    label: String,
}

impl Render for TabDragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        h_flex()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .bg(tokens.colors.surface)
            .border_1()
            .border_color(extended.colors.hairline)
            .shadow_md()
            .text_size(tokens.typography.sm.size)
            .text_color(tokens.colors.foreground)
            .child(Icon::new(DuckIcon::SquareTerminal).size(extended.icon.sm))
            .child(self.label.clone())
    }
}

/// 单个文件面板的浏览状态。
///
/// 条目**缓存**在这里而不是渲染时现算：本地面板要读磁盘、远端面板要起 `ssh`，
/// 放进 `render` 等于每帧一次系统调用 / 进程创建。
struct PaneState {
    /// 当前目录。
    path: String,
    /// 选中的文件名（决定「上传 / 下载」是否可用）。
    selected: Option<String>,
    /// 当前目录的条目。
    entries: Vec<DirEntry>,
    /// 需要展示的说明（读失败原因 / 未连接 / 已截断）。
    notice: Option<data::Notice>,
    /// 远端面板是否已经列举过一次。
    ///
    /// 用来区分「还没加载」与「加载完确实是空目录」：前者要触发一次列举，
    /// 后者只该显示「这个目录里没有可显示的条目」。
    loaded: bool,
}

impl PaneState {
    /// 初始化某侧面板：打开到该侧的初始目录。
    fn new(side: Side) -> Self {
        let mut pane = Self {
            path: side.initial(),
            selected: None,
            entries: Vec::new(),
            notice: None,
            loaded: false,
        };
        if side.is_local() {
            pane.reload(side);
        } else {
            // 远端：会话建立后由 `ensure_remote_listing` 异步加载；
            // 在那之前如实说明，而不是留一片空白。
            pane.notice = Some(data::Notice::Key(data::REMOTE_OFFLINE_NOTICE));
        }
        pane
    }

    /// 按当前路径重新列举（**本地面板专用**）。
    ///
    /// 远端列举要起一个 `ssh` 进程，属于阻塞活儿 —— 走页面的异步通道
    /// （[`TerminalPage::ensure_remote_listing`]），绝不在渲染路径上同步做。
    fn reload(&mut self, side: Side) {
        let listing = data::listing(side, &self.path);
        self.entries = listing.entries;
        self.notice = listing.notice;
        self.selected = None;
        self.loaded = true;
    }

    /// 跳到指定目录。
    fn navigate(&mut self, side: Side, path: String) {
        self.path = data::normalize(&path);
        self.selected = None;
        if side.is_local() {
            self.reload(side);
        } else {
            // 远端：把 `loaded` 打回 false，让渲染前的 `ensure_remote_listing` 重新列举。
            self.loaded = false;
        }
    }
}

/// SSH 终端页。
///
/// `sessions` / `poll_task` / `terminal_font_size` / `view` 标 `pub(super)`：本页的
/// **画布渲染层**在 `pages::terminal_canvas`（同一父模块下的兄弟模块），它要给
/// `TerminalPage` 挂 `impl` 并读写这几项。字段私有于 `pages::terminal` 时兄弟模块看不见，
/// 而为了这点访问去写一整套存取器只会把「谁拥有会话表」这件事搅浑 —— 直接开可见性更诚实。
pub struct TerminalPage {
    /// 左栏折叠 / 展开状态（shell 顶栏的侧栏开关也读它）。
    sidebar: Entity<SidebarState>,
    /// 连接搜索框。
    search: Entity<InputState>,
    /// 连接**档案表**（唯一真源，落 `kv: ssh_profiles`）。
    ///
    /// 左栏的分组视图由它派生（[`Self::group_views`]），不另存一份 `Vec<Group>` ——
    /// 两份表意味着「改了这份忘了那份」，而连接档案是用户的长期资产。
    /// 内置的本地 shell 不在表里（见 [`data::LOCAL_SHELL_ID`]），它是入口不是档案。
    profiles: Vec<StoredProfile>,
    /// 打开的标签页（顺序即展示顺序）。
    pub(super) tabs: Vec<SessionTab>,
    /// 激活标签下标。
    active: usize,
    /// 展开的连接分组（**缺省全折叠**：一进页面就满屏展开是噪音，分组由用户点开）。
    expanded: HashSet<&'static str>,
    /// 两侧目录面板（下标见 [`Side::index`]）。
    panes: [PaneState; 2],
    /// 传输队列。
    transfers: Vec<Transfer>,
    /// 终端画布字号（真实生效，「字号 -/+」与快捷键都改它）。
    pub(super) terminal_font_size: f32,
    /// 本地终端标签的运行时状态（键 = 标签 id）。SSH / SFTP 标签不进这张表。
    pub(super) sessions: HashMap<String, TerminalSession>,
    /// 事件轮询任务。持有句柄只是为了延长它的生命周期 —— 任务靠视图的**弱引用**自判退出，
    /// 不需要额外的停止信号（窗口一关、页面释放，`update` 返回 `Err`，循环自己结束）。
    pub(super) poll_task: Option<Task<()>>,
    /// 最近一次把键盘交给画布的本地标签。
    ///
    /// 焦点只在**切换**到本地标签时接管一次；用户主动点去别处（搜索框、侧栏）后
    /// 不该被反复抢回来，所以记住「已经交过了」的那个 id。
    pub(super) focused_tab: Option<String>,
    /// 最后拿到键盘焦点的终端窗格（会话键）。
    ///
    /// 分屏之后「当前会话」不再等于「当前标签」：清屏这类由工具条发起、
    /// 却作用于某个窗格的动作要落在**用户正在打字的那一半**上。
    pub(super) focus_pane: Option<String>,
    /// 上一次打开过 SFTP 面板的连接 id。
    ///
    /// 工具条那个 SFTP 图标要靠它工作：面板标签一关，界面上就没有任何地方
    /// 记着「刚才在看哪台机器」，只按当前标签推导会让图标在关掉面板后失灵。
    /// 档案被删时由 `sftp_owner` 里的存在性检查兜掉，不留悬空引用。
    last_sftp_owner: Option<String>,
    /// 正在被拖拽的标签 id（拖放进行中；拖放结束即清）。
    ///
    /// 只用来让画布决定「要不要显示右半屏的投放区」。悬置的旧值无害：
    /// 渲染条件里还有一道 `cx.has_active_drag()`，拖放一旦结束（无论落没落进
    /// 投放区）下一帧就不再显示。
    dragging_tab: Option<String>,
    /// 拖拽指针正悬停在投放区（画布右半屏）上。
    ///
    /// 由画布的 `on_drag_move` 随指针更新；`on_drop` 只在它为真时执行并入 ——
    /// 拖拽可以在画布左半屏松手，那不算数。
    split_drop_hover: bool,
    /// 远端列举是否正在进行中。
    ///
    /// `render` 每帧都会问一次「远端面板加载了吗」，没有这个标记就会**每帧起一个
    /// `ssh` 进程**——界面看起来只是慢，实际在疯狂 fork。
    remote_listing_inflight: bool,
    /// 传输任务的自增编号（队列行的 ElementId 与结果回填都靠它）。
    next_transfer_id: u64,
    /// 自引用：右栏等只有 `&App` 的地方要靠它把点击送回来。
    pub(super) view: WeakEntity<Self>,
    /// 搜索词变更订阅；持有句柄仅为延长生命周期。
    _subscriptions: Vec<Subscription>,
}

impl TerminalPage {
    /// 创建页面：左栏展开，连接表从存储读入，**不预置任何会话或演示数据**。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| {
            SidebarState::new(
                px(SIDEBAR_DEFAULT_WIDTH),
                px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH),
                cx,
            )
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "搜索连接…")));
        let mut subscriptions = Vec::new();
        // 输入即重绘：过滤结果在渲染期按当前关键字现算，不额外存一份状态。
        subscriptions.push(cx.subscribe(
            &search,
            |_this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));

        let mut this = Self {
            sidebar,
            search,
            profiles: data::load_profiles(cx),
            // 初始**没有**标签页：一进页面就是空态，由用户从左侧选连接或新建。
            // 预置演示标签会让人以为「已经连上了」，而它连的是不存在的主机。
            tabs: Vec::new(),
            active: 0,
            expanded: HashSet::new(),
            panes: [PaneState::new(Side::Local), PaneState::new(Side::Remote)],
            // 传输队列从空开始：队列里的每一条都该对应一次真实传输。
            transfers: Vec::new(),
            terminal_font_size: active_theme(cx).tokens().typography.mono_md.size.to_f64() as f32,
            sessions: HashMap::new(),
            poll_task: None,
            focused_tab: None,
            focus_pane: None,
            last_sftp_owner: None,
            dragging_tab: None,
            split_drop_hover: false,
            remote_listing_inflight: false,
            next_transfer_id: 1,
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
        };
        // 轮询任务只能在渲染之外起：渲染期间 `App` 已经被借出，再 `spawn` 就是重入。
        // 这里起一次就够了 —— 任务自己按「有没有会话」快慢变速，不随标签增删重启。
        this.start_terminal_poller(cx);
        this
    }

    /// 供 shell 顶栏共享的侧栏状态。
    #[must_use]
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    /// 某一侧目录面板的只读引用。
    fn pane(&self, side: Side) -> &PaneState {
        &self.panes[side.index()]
    }

    /// 激活某个标签。
    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active != index {
            self.active = index;
            cx.notify();
        }
    }

    /// 关闭第 `index` 个标签；关的是当前标签时优先落到原位置的下一个。
    ///
    /// 会话表由这里统一维护（`pages::terminal_canvas` 的轮询发现子进程退出后也走它），
    /// 别在调用点各写一遍 —— 漏掉一次就是一个永远不会被回收的 shell。
    pub(super) fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(index);
        // 会话随标签一起丢：`Session::drop` 给事件循环发 `Shutdown`，PTY master 关闭，
        // 子 shell 收到 `SIGHUP` —— 不会留下孤儿进程。
        self.sessions.remove(&tab.id);
        // 分屏的第二个窗格是**另一条会话**，同一条命令收掉它（漏一个就是一条
        // 永远留在后台的 shell）。
        if let Some(split) = &tab.split {
            self.sessions.remove(split);
            if self.focus_pane.as_deref() == Some(split.as_str()) {
                self.focus_pane = None;
            }
        }
        if self.focused_tab.as_deref() == Some(tab.id.as_str()) {
            self.focused_tab = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= index {
            self.active = self.active.saturating_sub(1).min(self.tabs.len() - 1);
        }
        cx.notify();
    }

    /// 关闭**当前活动标签**（`⌘W`）。
    ///
    /// 薄薄一层：`self.active` 就是「现在看得见的那个」，收尾全交给
    /// [`Self::close_tab_at`]（它负责会话回收与 `active` 指针的落点）。
    pub(super) fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        self.close_tab_at(self.active, cx);
    }

    /// 分屏 / 取消分屏当前标签。
    ///
    /// SFTP 标签没有 PTY（内容是一对文件面板），不参与分屏。
    ///
    /// 第二个窗格起的是**一条新会话**：本地标签再开一个 shell，SSH 标签再开一条
    /// `ssh -t`（复用同一条 `ControlMaster` 物理连接，实际只多一个通道）。
    /// 会话本身不在这里建 —— 本函数只改标签，`ensure_sessions` 在本帧按「会话表里
    /// 有没有这个键」接手，与「点连接只建标签」是同一条规矩。
    ///
    /// `pub(super)`：画布里的 `⌘D`（`terminal_canvas::SplitPane`）也走这里，
    /// 与工具条那个图标共用一份状态。
    pub(super) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.kind == TabKind::Sftp {
            return;
        }
        let kind = tab.kind;
        let id = tab.id.clone();
        match tab.split.take() {
            Some(split) => {
                tab.clear_split();
                // `tab` 的借用到此为止：收会话要走整个 `self`。
                self.close_split_session(&split, cx);
            }
            None => {
                tab.split = Some(data::split_session_key(&id));
                tab.split_owner = Some(data::split_owner_for(kind, &id));
            }
        }
        cx.notify();
    }

    /// 把当前标签分屏到**指定连接**（左栏连接右键菜单「在分屏中打开」）。
    ///
    /// 与 [`Self::toggle_split`] 共用同一份数据（`split` + `split_owner`），差别只在
    /// 目标连接：缺省（工具条 / `⌘D`）连的是「标签自己的连接」，这里连用户点的那条
    /// —— 本地 shell 与远端主机都行，两个窗格由此可以各连一台机器。
    ///
    /// 已分屏时再点：同一条连接 = 取消分屏（与工具条同语义）；另一条 = 换目标
    ///（旧窗格的会话就地收掉，新会话由 `ensure_sessions` 按新归属补起）。
    fn open_split_with(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        // 前置判定先做完（只借不可变部分），下面各分支再按需拿可变借用。
        let Some(active) = self.tabs.get(self.active) else {
            return;
        };
        if active.kind == TabKind::Sftp {
            return;
        }
        // 目标必须是能连的东西：内置本地 shell 或一条真实档案（档案可能刚被删）。
        if connection_id != data::LOCAL_SHELL_ID && self.profile(connection_id).is_none() {
            return;
        }
        let split = data::split_session_key(&active.id);
        let already_split = active.split.is_some();
        let same_target = active.split_owner.as_deref() == Some(connection_id);
        if !already_split {
            let tab = &mut self.tabs[self.active];
            tab.split = Some(split);
            tab.split_owner = Some(connection_id.to_owned());
            cx.notify();
            return;
        }
        if same_target {
            self.tabs[self.active].clear_split();
            self.close_split_session(&split, cx);
            return;
        }
        // 换目标：旧窗格的会话就地收掉；新键还不在会话表里，`ensure_sessions` 本帧按
        // 新归属补起。焦点标记一并清掉 —— 旧窗格没了，下一帧键盘回到主窗格。
        let old = {
            let tab = &mut self.tabs[self.active];
            let old = tab.split.take();
            tab.split_owner = Some(connection_id.to_owned());
            old
        };
        if let Some(old) = old {
            self.sessions.remove(&old);
            if self.focus_pane.as_deref() == Some(old.as_str()) {
                self.focus_pane = None;
                self.focused_tab = None;
            }
        }
        cx.notify();
    }

    /// 收掉某个分屏窗格的会话；焦点正落在它上面时把键盘还给主窗格。
    fn close_split_session(&mut self, key: &str, cx: &mut Context<Self>) {
        self.sessions.remove(key);
        if self.focus_pane.as_deref() == Some(key) {
            self.focus_pane = None;
            self.focused_tab = None;
        }
        cx.notify();
    }

    /// 把一个标签并入**当前标签**的分屏右窗格（拖拽投放的落点）。
    ///
    /// 目标永远取拖拽发生时画布上显示的那个标签（= 当时的活动标签）。被拖标签
    /// 从标签栏里消失、它的会话**整个搬过去**当右窗格 —— 与主流终端「把标签拖进
    /// 窗格」的手感一致。前提（调用方已保证，这里再挡一道）：
    ///
    /// - 双方都不是 SFTP 标签、都还没分屏（本应用的分屏就是两条会话，没有第三格）；
    /// - 被拖的不是目标自己（自己拖进自己没有意义）。
    fn merge_tab_into_split(&mut self, dragged_id: &str, cx: &mut Context<Self>) {
        let Some(d_index) = self.tabs.iter().position(|tab| tab.id == dragged_id) else {
            return;
        };
        let t_index = self.active;
        if t_index == d_index || t_index >= self.tabs.len() {
            return;
        }
        {
            // 与投放区的显示条件同一套：任一方不合格就不动（正常路径根本走不到）。
            let dragged = &self.tabs[d_index];
            let target = &self.tabs[t_index];
            if dragged.split.is_some()
                || dragged.kind == TabKind::Sftp
                || target.split.is_some()
                || target.kind == TabKind::Sftp
            {
                return;
            }
        }
        // 分屏窗格的连接归属跟被拖标签走：本地标签映射到内置本地 shell，
        // SSH 标签就是它自己的连接 id —— 会话搬过去还是连原来的机器。
        let owner = match self.tabs[d_index].kind {
            TabKind::Local => data::LOCAL_SHELL_ID.to_owned(),
            TabKind::Ssh => dragged_id.to_owned(),
            TabKind::Sftp => return,
        };
        let split = data::split_session_key(self.tabs[t_index].id.as_str());
        // 被拖标签的会话换个键继续活（没起会话也不要紧：`ensure_sessions`
        // 会按归属在本帧补一条）。
        if let Some(live) = self.sessions.remove(dragged_id) {
            self.sessions.insert(split.clone(), live);
        }
        // 关被拖的标签：会话键已搬走，`close_tab_at` 删的是空键，不会杀掉谁；
        // 它同时把 `active` 指针调对，之后目标下标按「它被删过」换算。
        self.close_tab_at(d_index, cx);
        let t_index = if t_index > d_index {
            t_index - 1
        } else {
            t_index
        };
        let tab = &mut self.tabs[t_index];
        tab.split = Some(split.clone());
        tab.split_owner = Some(owner);
        self.active = t_index;
        // 焦点交给新窗格：用户刚把它拖进来，接下来打字多半就是打给它。
        self.focus_pane = Some(split);
        self.focused_tab = None;
        cx.notify();
    }

    /// 拖拽在画布上松手（`on_drop`）：只有指针悬在右半屏投放区上才算并入。
    fn finish_tab_drop(&mut self, drag: &TabDrag, cx: &mut Context<Self>) {
        self.dragging_tab = None;
        if !self.split_drop_hover {
            return;
        }
        self.split_drop_hover = false;
        self.merge_tab_into_split(&drag.tab_id, cx);
    }

    /// 关闭除第 `index` 个以外的全部标签（右键菜单「关闭其他标签页」）。
    ///
    /// 复用 [`Self::close_tab_at`] 的收尾逻辑：从后往前删，下标不会乱。
    fn close_other_tabs(&mut self, index: usize, cx: &mut Context<Self>) {
        for i in (0..self.tabs.len()).filter(|&i| i != index).rev() {
            self.close_tab_at(i, cx);
        }
    }

    /// 关闭全部标签（右键菜单「关闭全部标签页」）。
    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        while !self.tabs.is_empty() {
            self.close_tab_at(self.tabs.len() - 1, cx);
        }
    }

    /// 清屏（工具条按钮）：作用于当前窗格。
    ///
    /// `pub(super)`：画布层的 `Clear` 动作（`⌘K`）另有更精确的落点 ——
    /// 它就在某个窗格里派发，直接清那一个（见 `pages::terminal_canvas`）。
    pub(super) fn clear_active(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.focused_session_key() {
            self.clear_session(&key, cx);
        }
    }

    /// 把 `ESC[2J` 写进某条会话的 PTY，让**应用自己**重画。
    ///
    /// 本地这条走 PTY 而不是去动网格：应用（vim、htop）自己维护着重画状态，
    /// 只清宿主看到的缓冲区会让两边内容对不上，下一帧它重画时就会露馅。
    ///
    /// 没有活动会话（无标签 / SFTP 标签没有 PTY 会话）时无事可做 —— 此前这里会置一个
    /// `cleared` 标志给「演示画布」隐藏静态行用；演示画布删除后它只写不读，一并删掉。
    pub(super) fn clear_session(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(live) = self.sessions.get(key) {
            live.write(CLEAR_SCREEN);
            // 清屏的语义是「回到当下」：用户可能正翻着历史，清完还停在旧位置会很怪。
            if let Some(session) = live.session() {
                session.scroll_to_bottom();
            }
        }
        cx.notify();
    }

    /// 展开 / 折叠一个连接分组。
    fn toggle_group(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// 激活标签的引用（越界时回落首个）。
    pub(super) fn active_tab(&self) -> Option<&SessionTab> {
        self.tabs.get(self.active).or_else(|| self.tabs.first())
    }

    /// 按 id 找连接档案（内置本地 shell 不在档案表里，返回 `None`）。
    fn profile(&self, id: &str) -> Option<&StoredProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// 一个连接的展示条目（**含内置的本地 shell**）。
    ///
    /// 工具条与右栏都要它；统一在这里兜底，免得每个调用点各写一遍 `map_or`。
    fn connection_entry(&self, id: &str) -> Option<Connection> {
        if id == data::LOCAL_SHELL_ID {
            return Some(data::local_shell_view());
        }
        self.profile(id).map(data::connection_view)
    }

    /// 「打开 SFTP 面板」该连的那条远端连接。
    ///
    /// 依次看两处：**当前标签**所属的连接（SFTP 标签取它的父连接），以及**上一次**
    /// 开过 SFTP 的连接。都取不到才返回 `None`（工具条上的图标此时置灰）。
    ///
    /// 此前只看当前标签，于是关掉 SFTP 面板后活动标签回落到本地 shell / 空态，
    /// 图标要么给本地 shell 开出一个永远列举不出东西的假面板，要么直接失灵 ——
    /// 「关了面板就再也打不开」就是这么来的。
    ///
    /// 判定本身在 [`resolve_sftp_owner`]（纯函数，单测直接钉「关了还能再开」）。
    fn sftp_owner(&self) -> Option<String> {
        let active = self.active_tab().map(|tab| {
            tab.id
                .strip_suffix("/sftp")
                .unwrap_or(tab.id.as_str())
                .to_owned()
        });
        resolve_sftp_owner(active.as_deref(), self.last_sftp_owner.as_deref(), &|id| {
            // 内置的本地 shell 不算数：它不是一台远端主机（`connection_entry` 却会
            // 热情地给它返回一个内置条目，所以这里必须自己挡一道）；
            // 档案被删的连接同样不算数 —— 否则图标会指向一条查不到的记录。
            id != data::LOCAL_SHELL_ID && self.profile(id).is_some()
        })
    }

    /// 工具条上「清屏」这类动作该落在哪条会话上。
    ///
    /// 分屏之后「当前会话」不再等于「当前标签」：以**最后拿到键盘焦点的窗格**为准，
    /// 记录不属于当前标签时（切过标签）回落到主窗格。
    fn focused_session_key(&self) -> Option<String> {
        let tab = self.active_tab()?;
        if self.focus_pane.as_deref() == tab.split.as_deref() && tab.split.is_some() {
            return tab.split.clone();
        }
        Some(tab.id.clone())
    }

    /// 某个连接当前的会话状态。
    ///
    /// 事实来源只有 `sessions` 一张表：有会话且没退出 = 已连接；会话在但已退出 = 已断开；
    /// 没有会话 = 已断开（`ensure_sessions` 会在本帧把它补上）。
    /// **不从存档读**，也不在点击时「乐观置为已连接」—— 那正是演示数据的做法。
    fn link_status(&self, id: &str) -> LinkStatus {
        match self.sessions.get(id) {
            Some(live) if live.is_exited() => LinkStatus::Disconnected,
            Some(_) => LinkStatus::Connected,
            None => LinkStatus::Disconnected,
        }
    }

    /// 左栏分组视图：档案派生 + 真实状态覆盖。
    ///
    /// 要 `cx` 只为一件事：翻译内置项（本地 shell）的名字；
    /// 用户档案的名字是数据，不翻译。
    fn group_views(&self, cx: &App) -> Vec<Group> {
        let mut groups = data::group_views(&self.profiles, active_theme(cx).lang());
        for group in &mut groups {
            for item in &mut group.items {
                item.status = self.link_status(&item.id);
            }
        }
        groups
    }

    /// 把标签状态同步成真实会话状态。
    ///
    /// SFTP 标签没有自己的会话，状态**跟随它的 SSH 连接**（`<id>/sftp` 的前半段）——
    /// 父连接断了，文件面板自然也不可用。
    fn sync_tab_status(&mut self) {
        // 先算好每一条标签的状态（这一步只读 `sessions`），再写回 `tabs`：
        // 在同一次循环里既可变借 `tabs` 又借 `self` 会被借用检查器拒掉。
        let statuses: Vec<LinkStatus> = self
            .tabs
            .iter()
            .map(|tab| {
                let owner = tab.id.strip_suffix("/sftp").unwrap_or(tab.id.as_str());
                self.link_status(owner)
            })
            .collect();
        for (tab, status) in self.tabs.iter_mut().zip(statuses) {
            tab.status = status;
        }
    }

    /// SSH 复用 socket 的目录（**不用 `/tmp`**：`/tmp` 是共享的，别人预先占住
    /// 同名 socket 就能劫持连接复用）。
    ///
    /// 放缓存目录而不是应用数据目录：后者在 macOS 下是 `~/Library/Application
    /// Support/...`，**路径带空格**，曾把 `-o ControlPath=…` 拆成两个词导致
    /// 连接直接失败（值本身已在 `duck-ssh` 层加引号兜底，但目录无空格是第一道
    /// 防线）。socket 是随时可重建的运行时产物，归缓存目录正合适。
    fn ssh_socket_dir(&self) -> PathBuf {
        duck_platform::cache_dir().join("ssh")
    }

    /// 由连接档案构造 SSH 目标。
    ///
    /// 口令从档案行里解密（[`open_password`]），只在「明确选了密码认证」时读 ——
    /// 解密是纯本地运算，不碰任何系统服务，所以点连接不会再弹系统授权框。
    pub(super) fn ssh_target(&self, id: &str) -> Option<(duck_ssh::Target, PathBuf)> {
        let profile = self.profile(id)?;
        let identity = {
            let path = profile.key_file.trim();
            (!path.is_empty()).then(|| PathBuf::from(path))
        };
        let password = (profile.auth == AuthKind::Password.value())
            .then(|| open_password(profile))
            .flatten();
        Some((
            duck_ssh::Target {
                host: profile.host.clone(),
                port: profile.port.trim().parse().unwrap_or(22),
                user: profile.user.clone(),
                identity,
                password,
                options: Vec::new(),
            },
            self.ssh_socket_dir(),
        ))
    }

    /// 打开（或切到）一个连接的会话标签。
    ///
    /// 已开着的连接只切标签，不重复开：同一主机开两条会话是无意的重复连接。
    ///
    /// 这里**只建标签**，真正的连接由 `ensure_sessions` 在本帧起 `ssh -t`。
    /// 之所以不在这里直接开进程：那需要 `&mut Context` 起任务，而本函数已经
    /// 被一个 `view.update` 闭包包着；分两步走也让「点一下 = 一个标签」这件事
    /// 与「连接成败」解耦 —— 标签先出现，成败由画布上的 ssh 输出说话。
    fn open_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = index;
            cx.notify();
            return;
        }
        // 内置本地 shell：直接开一条本地终端标签。
        if id == data::LOCAL_SHELL_ID {
            self.new_local_tab(cx);
            return;
        }
        let Some(entry) = self.connection_entry(id) else {
            return;
        };
        // 能力探测：没装 ssh 时**当面说清楚**，而不是建一个必然失败的标签。
        if !duck_ssh::probe().terminal() {
            notify::warning(
                tr(
                    cx,
                    "未找到系统 ssh 命令，无法建立 SSH 连接。请先安装 OpenSSH 客户端。",
                ),
                window,
                cx,
            );
            return;
        }
        // 复用 socket 的目录要先建出来：`ControlPath` 指向不存在的目录时，
        // ssh 不会自己创建，直接报错退出。
        let _ = duck_platform::ensure_dir(&self.ssh_socket_dir());
        self.tabs.push(SessionTab {
            id: id.to_owned(),
            title: entry.label,
            kind: TabKind::Ssh,
            // 标签先以「连接中」出现，真实状态由本帧起的会话在下一轮轮询里改写。
            status: LinkStatus::Connecting,
            split: None,
            split_owner: None,
        });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// 为一条连接开 SFTP 面板标签。
    ///
    /// 复用同一条物理连接（`ControlMaster`），所以**连接本身必须先立起来**：
    /// 用户点「SFTP 面板」的意图就是要连上它，这里顺手把 SSH 标签也开了，
    /// 而不是甩一句「请先连接」。
    ///
    /// **只对真实档案开**：内置的本地 shell 不是一台远端主机（`connection_entry`
    /// 会热情地给它返回一个内置条目），放它进来会得到一个永远列举不出东西的
    /// 假 SFTP 标签 —— 空面板比没有面板更让人困惑。调用点（工具条 / 左栏菜单 /
    /// 标签菜单 / 右栏）都已各自挡在前面，这里留一道是为了下一个调用点。
    fn open_sftp_tab(&mut self, connection_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // SFTP 要 `ssh`（列举）与 `scp`（传输）两个命令都在。
        if !duck_ssh::probe().files() {
            notify::warning(
                tr(cx, "未找到系统 ssh / scp 命令，无法打开 SFTP 面板。"),
                window,
                cx,
            );
            return;
        }
        if connection_id == data::LOCAL_SHELL_ID {
            notify::warning(
                tr(cx, "本地 shell 没有远端文件可浏览，请先连接一台远程主机。"),
                window,
                cx,
            );
            return;
        }
        if self.profile(connection_id).is_none() {
            return;
        }
        if !self.tabs.iter().any(|tab| tab.id == connection_id) {
            self.open_connection(connection_id, window, cx);
        }
        let id = format!("{connection_id}/sftp");
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.active = index;
            cx.notify();
            return;
        }
        let Some(entry) = self.connection_entry(connection_id) else {
            return;
        };
        let title = trf(cx, "SFTP · {}", &[entry.label.as_str()]);
        self.tabs.push(SessionTab {
            id,
            title,
            kind: TabKind::Sftp,
            status: entry.status,
            split: None,
            split_owner: None,
        });
        self.active = self.tabs.len() - 1;
        // 记住这条连接：面板关掉之后，工具条那个图标还得知道该给谁开面板
        //（标签没了就只剩它记着）。
        self.last_sftp_owner = Some(connection_id.to_owned());
        cx.notify();
    }

    /// 新开一条本地终端标签。
    ///
    /// 第一个标签用**固定 id**（[`data::LOCAL_SHELL_ID`]），因为左栏的内置入口
    /// 点开的就是它 —— 两边同一个 id，左栏的选中态才跟得上。
    ///
    /// `pub(super)`：画布里的 `⌘T`（`terminal_canvas::NewTab`）也走这里，
    /// 与标签条上那个 `+` 共用同一份状态机。
    pub(super) fn new_local_tab(&mut self, cx: &mut Context<Self>) {
        let mut ordinal = self
            .tabs
            .iter()
            .filter(|tab| tab.kind == TabKind::Local)
            .count();
        loop {
            let (id, title) = if ordinal == 0 {
                (
                    data::LOCAL_SHELL_ID.to_owned(),
                    tr(cx, "本地 shell").to_owned(),
                )
            } else {
                (
                    format!("{}-{}", data::LOCAL_SHELL_ID, ordinal + 1),
                    format!("{} {}", tr(cx, "本地 shell"), ordinal + 1),
                )
            };
            // 用户可能已经关掉了中间某个标签，所以 id 要一直试到空位为止。
            if !self.tabs.iter().any(|tab| tab.id == id) {
                self.tabs.push(SessionTab {
                    id,
                    title,
                    kind: TabKind::Local,
                    status: LinkStatus::Connected,
                    split: None,
                    split_owner: None,
                });
                break;
            }
            ordinal += 1;
        }
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// 分组下拉的选项：`(分组 id, 分组显示名)`。
    ///
    /// 分组骨架是**产品结构**（[`data::GROUP_DEFS`]），不是用户数据 ——
    /// 所以不跟着连接档案走，永远不会因为「一条连接都没有」而变空。
    fn group_options(&self, cx: &App) -> Vec<(&'static str, String)> {
        let lang = active_theme(cx).lang();
        data::GROUP_DEFS
            .iter()
            .map(|(id, label, _)| (*id, t(lang, label).to_owned()))
            .collect()
    }

    /// 打开「新建连接」弹窗。
    fn open_new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = self.group_options(cx);
        let socket_dir = self.ssh_socket_dir();
        let view = self.view.clone();
        open_connection_dialog(
            window,
            cx,
            groups,
            socket_dir,
            None,
            move |draft, window, cx| {
                let _ = view.update(cx, |this, cx| this.create_connection(draft, window, cx));
            },
        );
    }

    /// 打开「编辑连接」弹窗（表单回填当前档案）。
    ///
    /// 与新建**共用同一个弹窗**：字段、校验、键盘行为完全一样，
    /// 分成两个只会让「新建能填的字段编辑时填不了」这类不一致悄悄长出来。
    fn open_edit_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        let groups = self.group_options(cx);
        // 口令**解密回显**：编辑连接时用户该看得到自己存过什么（掩码显示），
        // 改个端口不用把口令重输一遍。回显要求「应用能无提示地读回口令」，
        // 这正是口令走本机加密保管箱而不是系统钥匙串的原因之一。
        let initial = NewConnection {
            label: profile.label.clone(),
            host: profile.host.clone(),
            port: profile.port.clone(),
            user: profile.user.clone(),
            auth: AuthKind::from_value(&profile.auth),
            key_file: profile.key_file.clone(),
            password: open_password(profile).unwrap_or_default(),
            group: data::GROUP_DEFS
                .iter()
                .find(|(group_id, _, _)| *group_id == profile.group)
                .map_or("prod", |(group_id, _, _)| *group_id),
            remember: profile.remember,
            // 编辑时不预勾「立即打开」：那会让人以为「保存 = 重连」，
            // 而重连与否由下方独立的「重新连接」决定。
            open_now: false,
        };
        let id = id.to_owned();
        let view = self.view.clone();
        let socket_dir = self.ssh_socket_dir();
        open_connection_dialog(
            window,
            cx,
            groups,
            socket_dir,
            Some(&initial),
            move |draft, window, cx| {
                let id = id.clone();
                let _ = view.update(cx, |this, cx| this.apply_edit(&id, draft, window, cx));
            },
        );
    }

    /// 把编辑结果写回档案。
    ///
    /// **id 不变**：它是会话标签、SFTP 面板、口令密文共同的锚点，
    /// 改名只改显示名，绝不换 id —— 换 id 等于把一条连接拆散重建。
    fn apply_edit(
        &mut self,
        id: &str,
        draft: NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        // 连接参数有没有变：变了就必须断掉重连 —— 旧会话连的是**旧参数**，
        // 留着会让用户对着一个「看起来改了其实没变」的会话发呆。
        // 口令也算参数：换了口令的旧会话是拿旧口令登进去的，留着同样误导。
        // （编辑弹窗回显旧口令，所以「没改口令」时两者相等，不会触发无谓重连。）
        let old_password = open_password(profile);
        let password_changed = old_password != draft_password_value(&draft);
        let reconnect_needed = profile.host != draft.host
            || profile.port != draft.port
            || profile.user != draft.user
            || profile.key_file != draft.key_file
            || profile.auth != draft.auth.value()
            || password_changed;

        // 口令先落档案（密文；它只读 `draft`，下面的档案字段会被 move 走）。
        apply_password(profile, &draft);
        profile.label = draft.label.clone();
        profile.host = draft.host;
        profile.port = draft.port;
        profile.user = draft.user;
        profile.auth = draft.auth.value().to_owned();
        profile.key_file = draft.key_file;
        profile.group = draft.group.to_owned();
        profile.remember = draft.remember;
        self.persist_profiles(cx);

        if reconnect_needed {
            // 摘掉旧会话：`ensure_sessions` 只看「表里有没有这个 id」，
            // 不摘掉它会认为「已经起过了」，改动就永远不会生效。
            let affected: Vec<String> = self
                .tabs
                .iter()
                .filter(|tab| tab.id == id || tab.id == format!("{id}/sftp"))
                .map(|tab| tab.id.clone())
                .collect();
            for tab_id in affected {
                self.sessions.remove(&tab_id);
            }
        }
        notify::success(tr(cx, "连接已更新。"), window, cx);
        cx.notify();
    }

    /// 落一条新建的连接档案；勾了「保存后立即打开」就顺手开一条会话。
    fn create_connection(
        &mut self,
        draft: NewConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.unique_connection_id(&draft.label);
        // 档案里存**口令密文**（勾了「记住口令」且非空时）：弹窗收到的明文在这一跳
        // 加密进档案行（AES-256-GCM，SQLite 里 grep 不到明文），或就地丢弃
        //（未勾选 —— 连接时 ssh 在终端里问）。加密先行（它只读 `draft`，
        // 下面的 push 会把字段 move 走）。
        let password = draft_password_value(&draft)
            .as_deref()
            .map(duck_platform::vault::seal)
            .transpose()
            .ok()
            .flatten();
        self.profiles.push(StoredProfile {
            id: id.clone(),
            label: draft.label.clone(),
            host: draft.host,
            port: draft.port,
            user: draft.user,
            auth: draft.auth.value().to_owned(),
            key_file: draft.key_file,
            group: draft.group.to_owned(),
            remember: draft.remember,
            password,
        });
        self.persist_profiles(cx);
        // 分组缺省折叠：新建的连接落进去却看不见，等于「点了没反应」——
        // 这是用户动作触发的展开，不属于「自动展开」。
        self.expanded.insert(draft.group);
        if draft.open_now {
            self.open_connection(&id, window, cx);
        }
        notify::success(
            trf(cx, "已创建连接「{}」。", &[draft.label.as_str()]),
            window,
            cx,
        );
        cx.notify();
    }

    /// 把档案表写回存储（**唯一落盘点**）。
    ///
    /// 所有增删改最后都汇集到这里：散落的 `kv_put` 迟早会漏掉一处，
    /// 而漏掉的那次就是「用户改完重启后改动没了」。
    fn persist_profiles(&self, cx: &App) {
        data::save_profiles(cx, &self.profiles);
    }

    /// 删一条档案（**唯一入口**）：先弹确认框，确认后才真正动手。
    ///
    /// 删除是破坏性动作，不给「撤销」这种半吊子兜底，而是**删之前问一句**。
    /// 参数在进入确认的那一刻冻结进 [`PendingAction`]，确认期间不回读连接表 ——
    /// 否则用户删完一条、又点了确认，动到的就不是他看到的那个连接了。
    fn delete_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        let action = PendingAction::DeleteConnection {
            id: id.to_owned(),
            label: profile.label.clone(),
        };
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title = trf(cx, "删除连接「{}」？", &[action.label().as_str()]);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份给内层。
            let action_for_ok = action.clone();
            let view_for_ok = view.clone();
            let drag_for_content = drag.clone();
            let title_for_content = title.clone();

            dialog
                .w(px(CONFIRM_DIALOG_W))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "删除"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    let title = title_for_content.clone();
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "terminal-confirm-title",
                                &drag_for_content,
                                title,
                                cx,
                            ))
                            .child(body_text(
                                tr(cx, "档案与已保存的口令都会被移除，本机不会留下副本。"),
                                cx,
                            )),
                    ))
                })
                .on_ok(move |_, _window, cx| {
                    // 视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                    // 不让一个点不掉的确认框挂在屏幕上。
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), cx);
                    });
                    true
                })
        });
    }

    /// 把 [`PendingAction`] 分派到真正动手的方法（`*_now` 族）。
    fn run_pending(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::DeleteConnection { id, .. } => self.delete_connection_now(&id, cx),
        }
    }

    /// 确认后的实际删除。
    ///
    /// 顺序是「关标签 → 删档案 → 收口令」，反过来会在中间态留一个引用已删档案的死标签。
    fn delete_connection_now(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.profile(id).is_none() {
            return;
        }
        // 标签：连接本身 + 它的 SFTP 面板。倒序删，正序会因下标位移漏掉后面的。
        let doomed: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.id == id || tab.id == format!("{id}/sftp"))
            .map(|(index, _)| index)
            .collect();
        for index in doomed.into_iter().rev() {
            self.close_tab_at(index, cx);
        }
        self.profiles.retain(|profile| profile.id != id);
        // 口令密文随档案行一起消失，没有第二份副本要收。
        self.persist_profiles(cx);
        cx.notify();
    }

    /// 由显示名派生一个不与现有连接冲突的 id。
    fn unique_connection_id(&self, label: &str) -> String {
        let slug: String = label
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let slug = slug.trim_matches('-');
        let base = if slug.is_empty() {
            "connection".to_owned()
        } else {
            slug.to_owned()
        };
        let mut candidate = base.clone();
        let mut suffix = 2;
        while self.profiles.iter().any(|profile| profile.id == candidate) {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }

    /// 打开某一侧的「打开目录」弹窗。
    fn open_directory(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.pane(side).path.clone();
        let view = self.view.clone();
        open_directory_dialog(window, cx, side, current, move |path, _, cx| {
            let _ = view.update(cx, |this, cx| this.navigate(side, path, cx));
        });
    }

    /// 跳到指定目录并重列举。
    fn navigate(&mut self, side: Side, path: String, cx: &mut Context<Self>) {
        self.panes[side.index()].navigate(side, path);
        cx.notify();
    }

    /// 重新读取当前目录（本地面板重新读盘；远端面板重新列举）。
    fn reload_pane(&mut self, side: Side, cx: &mut Context<Self>) {
        if side.is_local() {
            self.panes[side.index()].reload(side);
        } else {
            // 远端把 `loaded` 打回 false：渲染前的 `ensure_remote_listing` 会接手，
            // 而它走的是后台执行器 —— 一次列举要起一个 `ssh` 进程，不能在渲染里同步等。
            self.panes[side.index()].loaded = false;
        }
        cx.notify();
    }

    /// 当前 SFTP 标签对应的远端连接。
    ///
    /// 三个前提缺一不可：激活的是 SFTP 标签、它的父连接**真的连着**、档案还在。
    /// 父连接没立起来就没有可复用的通道 —— SFTP 不是独立协议，它跑在 SSH 之上。
    fn sftp_target(&self) -> Option<RemoteHandle> {
        let tab = self.active_tab()?;
        if tab.kind != TabKind::Sftp {
            return None;
        }
        let owner = tab.id.strip_suffix("/sftp")?;
        if self.link_status(owner) != LinkStatus::Connected {
            return None;
        }
        let (target, socket_dir) = self.ssh_target(owner)?;
        Some(RemoteHandle { target, socket_dir })
    }

    /// 渲染前给激活的 SFTP 面板补一次远端列举。
    ///
    /// 列举要起 `ssh`（阻塞），所以丢给后台执行器，回来后再 `notify`；
    /// `remote_listing_inflight` 防重入 —— `render` 每帧都跑，没有它会每帧起一个进程。
    pub(super) fn ensure_remote_listing(&mut self, cx: &mut Context<Self>) {
        if self.remote_listing_inflight {
            return;
        }
        let remote = Side::Remote.index();
        if self.panes[remote].loaded {
            return;
        }
        let Some(handle) = self.sftp_target() else {
            // 没有可用的远端连接：把说明摆正，也**清掉上一台机器的目录**——
            // 留着它比空白更糟，用户会以为正在看当前主机的文件。
            self.panes[remote].entries.clear();
            self.panes[remote].selected = None;
            self.panes[remote].notice = Some(data::Notice::Key(data::REMOTE_OFFLINE_NOTICE));
            return;
        };
        let path = self.panes[remote].path.clone();
        // 空路径（刚打开、还没连上）从远端主目录起步：交给远端 shell 展开 `~`，
        // 免得我们去猜一个绝对路径（`root` 和 `deploy` 的家目录不一样）。
        let path = if path.trim().is_empty() {
            "~".to_owned()
        } else {
            path
        };
        self.remote_listing_inflight = true;
        let request_path = path.clone();
        cx.spawn(async move |page, cx| {
            let result =
                cx.background_executor()
                    .spawn({
                        let handle = handle.clone();
                        let path = request_path.clone();
                        async move {
                            duck_ssh::list_dir(&handle.target, Some(&handle.socket_dir), &path)
                        }
                    })
                    .await;
            let _ = page.update(cx, |page, cx| {
                page.remote_listing_inflight = false;
                page.apply_remote_listing(request_path, result, cx);
            });
        })
        .detach();
    }

    /// 把一次远端列举的结果落到面板上。
    fn apply_remote_listing(
        &mut self,
        path: String,
        result: Result<duck_ssh::RemoteListing, duck_ssh::SshError>,
        cx: &mut Context<Self>,
    ) {
        let remote = Side::Remote.index();
        self.panes[remote].path = path;
        self.panes[remote].selected = None;
        match result {
            Ok(listing) => {
                let mut rows: Vec<DirEntry> = listing
                    .entries
                    .into_iter()
                    .map(|entry| DirEntry {
                        name: entry.name,
                        size: entry.size.map_or_else(|| "—".to_owned(), format_bytes),
                        modified: entry.modified.unwrap_or_else(|| "—".to_owned()),
                        dir: entry.dir,
                    })
                    .collect();
                data::sort_entries(&mut rows);
                self.panes[remote].entries = rows;
                // 截断了就说一句：不说的话用户会以为目录里就这么多东西。
                self.panes[remote].notice = listing
                    .truncated
                    .then_some(data::Notice::Key(data::REMOTE_TRUNCATED_NOTICE));
            }
            Err(error) => {
                self.panes[remote].entries.clear();
                // ssh 的原文（`Permission denied`、`No such file or directory`）
                // 直接给用户看：它比任何我们转述的说法都准确，也指得出下一步。
                // 只有外壳文案是我们自己的，那一层要跟随语言。
                self.panes[remote].notice = Some(data::Notice::Text(ssh_error_text(cx, &error)));
            }
        }
        self.panes[remote].loaded = true;
        cx.notify();
    }

    /// 点击文件行：单击选中，双击目录进入、双击文件先给个说明。
    fn click_entry(
        &mut self,
        side: Side,
        name: String,
        double: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !double {
            self.panes[side.index()].selected = Some(name);
            cx.notify();
            return;
        }
        let is_dir = self
            .pane(side)
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.dir);
        match is_dir {
            Some(true) => {
                let path = data::join(&self.pane(side).path, &name);
                self.navigate(side, path, cx);
            }
            // 双击文件：
            // - 本地面板 → 在访达里显示（`open -R`），这是「看一眼这个文件」最直接的落地；
            // - 远端面板 → 下载到本地面板的当前目录（复用同一条传输通道）。
            Some(false) => {
                if side.is_local() {
                    let path = PathBuf::from(&self.pane(Side::Local).path).join(&name);
                    // 平台层不返回结果：`open -R` 失败没什么可补救的
                    //（文件被删了 / 没有 Finder），弹一个错误框反而是噪音。
                    duck_platform::reveal_in_finder(&path);
                } else {
                    self.panes[Side::Remote.index()].selected = Some(name);
                    self.queue_transfer(false, window, cx);
                }
            }
            None => {}
        }
    }

    /// 把选中项加入传输队列，并**立刻开始传**。
    ///
    /// 上传取本地面板的选中项、目标是远端的当前目录；下载反过来。
    /// 传输跑在后台执行器上（`scp` 是阻塞进程），完成后刷新**对侧**目录 ——
    /// 新文件该出现在列表里，否则用户无从确认它到底传成功没有。
    fn queue_transfer(&mut self, upload: bool, window: &mut Window, cx: &mut Context<Self>) {
        let source = if upload { Side::Local } else { Side::Remote };
        let Some(name) = self.pane(source).selected.clone() else {
            notify::warning(tr(cx, "请先在文件列表里选中一项。"), window, cx);
            return;
        };
        let Some(handle) = self.sftp_target() else {
            notify::warning(tr(cx, "远端连接尚未建立，无法传输。"), window, cx);
            return;
        };
        let local_dir = self.pane(Side::Local).path.clone();
        let remote_dir = self.pane(Side::Remote).path.clone();
        // 远端目录还没列举出来时不给传：`scp` 的远端路径是真要用的，
        // 拿一个空串过去只会得到一个语焉不详的报错。
        if remote_dir.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }

        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let request = TransferRequest {
            id,
            upload,
            local_file: PathBuf::from(&local_dir).join(&name),
            local_dir: PathBuf::from(&local_dir),
            remote_dir: remote_dir.clone(),
            remote_file: format!("{}/{}", remote_dir.trim_end_matches('/'), name),
        };
        self.transfers.push(Transfer {
            id,
            name: name.clone(),
            progress: 0.,
            state: data::TransferState::Queued,
            upload,
            reason: None,
        });
        cx.notify();
        self.spawn_transfer(request, handle, cx);
        notify::success(trf(cx, "已开始传输 {}。", &[name.as_str()]), window, cx);
    }

    /// 在后台执行器上跑一次传输。
    fn spawn_transfer(
        &mut self,
        request: TransferRequest,
        handle: RemoteHandle,
        cx: &mut Context<Self>,
    ) {
        self.mark_transfer(request.id, data::TransferState::Running, None, cx);
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let handle = handle.clone();
                    let request = request.clone();
                    async move {
                        if request.upload {
                            duck_ssh::upload(
                                &handle.target,
                                Some(&handle.socket_dir),
                                &request.local_file,
                                &request.remote_dir,
                            )
                        } else {
                            duck_ssh::download(
                                &handle.target,
                                Some(&handle.socket_dir),
                                &request.remote_file,
                                &request.local_dir,
                            )
                        }
                    }
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                match result {
                    Ok(()) => {
                        page.mark_transfer(request.id, data::TransferState::Done, None, cx);
                    }
                    Err(error) => {
                        page.mark_transfer(
                            request.id,
                            data::TransferState::Failed,
                            // `scp` 的 stderr 原文：它指得出下一步（权限？磁盘满？路径不对？），
                            // 而一句「传输失败」什么也没说。
                            Some(ssh_error_text(cx, &error)),
                            cx,
                        );
                    }
                }
                if request.upload {
                    page.reload_pane(Side::Remote, cx);
                } else {
                    page.reload_pane(Side::Local, cx);
                }
            });
        })
        .detach();
    }

    /// 更新一行传输的状态。
    fn mark_transfer(
        &mut self,
        id: u64,
        state: data::TransferState,
        reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            row.state = state;
            row.reason = reason;
            if state == data::TransferState::Done {
                row.progress = 1.;
            }
        }
        cx.notify();
    }

    /// 调整终端画布字号（钳在 [`FONT_SIZE_RANGE`] 内）。
    ///
    /// `pub(super)`：画布层（`terminal_canvas`）的 `ZoomIn` / `ZoomOut` 动作也走这里，
    /// 与工具条 ± 按钮共用同一份状态与钳位。
    pub(super) fn nudge_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let next = (self.terminal_font_size + delta)
            .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end());
        if (next - self.terminal_font_size).abs() > f32::EPSILON {
            self.terminal_font_size = next;
            cx.notify();
        }
    }

    /// 字号复位到主题的等宽正文档（`⌘0`）。
    ///
    /// 复位基准就是 `new()` 的初值（`typography.mono_md`），不另设常量 ——
    /// 主题换档时「复位」跟着走，不会把用户丢回一个写死的旧字号。
    pub(super) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        let next = active_theme(cx).tokens().typography.mono_md.size.to_f64() as f32;
        if (next - self.terminal_font_size).abs() > f32::EPSILON {
            self.terminal_font_size = next;
            cx.notify();
        }
    }

    /// 左栏：搜索 + 分组连接列表。行规格与 [`sidebar_navigation_button`] 一致。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let control = theme.density().control;
        let view = self.view.clone();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();

        let new_connection = button(
            "terminal-new-connection",
            tr(cx, "新建连接"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.open_new_connection(window, cx));
            }
        });

        let header_new = icon_button(
            "terminal-new-connection-icon",
            tr(cx, "新建连接"),
            // 图标走 `icon.sm`：与数据库 / Redis 左栏 section header 同位置的方形图标按钮一致
            // （`icon_button` 框恒为 `density.control`，档位取 sm 是这三页的共同约定）。
            Icon::new(DuckIcon::Plus)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            ButtonVariant::Ghost,
            cx,
        )
        // 与同行搜索框等高：曾为了「视觉轻一点」矮过一个 `xs`，实测加号小到难点中，
        // 而且它是左栏唯一的「新建」入口 —— 可点性优先于视觉轻重。
        .h(control)
        .on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| this.open_new_connection(window, cx));
            }
        });

        let mut nav = v_flex().w_full().gap(tokens.spacing.xxs);
        let mut matched = 0_usize;
        // 视图每次渲染现算：连接表是唯一真源（`profiles`），
        // 多存一份 `Vec<Group>` 就意味着「改了这份忘了那份」。
        let groups = self.group_views(cx);

        for group in &groups {
            let items: Vec<&Connection> = group
                .items
                .iter()
                .filter(|item| {
                    !searching
                        || item.label.to_lowercase().contains(&query)
                        || item.endpoint().to_lowercase().contains(&query)
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            matched += items.len();

            // 搜索时临时展开命中的分组（不然「搜到了却看不见」）；平时只看用户点开的。
            let expanded = searching || self.expanded.contains(group.id);
            let chevron = if expanded {
                DuckIcon::ChevronDown
            } else {
                DuckIcon::ChevronRight
            };
            let group_id = group.id;

            nav = nav.child(
                sidebar_navigation_button(
                    SharedString::from(format!("terminal-group-{group_id}")),
                    SharedString::from(t(lang, group.label)),
                    Icon::new(group.icon).size(extended.icon.lg),
                    Icon::new(chevron)
                        .size(extended.icon.sm)
                        .text_color(tokens.colors.muted_foreground),
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.toggle_group(group_id, cx));
                    }
                }),
            );

            if expanded {
                for item in items {
                    let id = item.id.clone();
                    // 选中态跟随**当前激活标签**：激活的是这条连接（或它的 SFTP 标签）才亮。
                    // 之前是「开过就算选中」，三个标签一开就满栏高亮，跟当前在哪没关系。
                    let selected =
                        self.active_tab()
                            .map(|tab| tab.id.as_str())
                            .is_some_and(|active| {
                                active == id || active.strip_suffix("/sftp") == Some(id.as_str())
                            });
                    nav = nav.child(
                        sidebar_navigation_button(
                            SharedString::from(format!("terminal-conn-{id}")),
                            SharedString::from(item.label.clone()),
                            Icon::new(DuckIcon::Server)
                                .size(extended.icon.md)
                                .text_color(item.status.color(cx)),
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .line_height(tokens.typography.xs.line_height)
                                .text_color(extended.colors.text_tertiary)
                                .child(item.endpoint()),
                            selected,
                            cx,
                        )
                        .pl(tokens.spacing.md)
                        .on_click({
                            let id = id.clone();
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.open_connection(&id, window, cx);
                                });
                            }
                        })
                        .context_menu({
                            // 菜单闭包是 `Fn`（每次打开都重跑），id 每项一份、
                            // 每个 on_click 再各自克隆。
                            let id = id.clone();
                            let builtin = item.builtin;
                            let view = view.clone();
                            move |menu, _, cx| {
                                let mut menu = menu
                                    .item(PopupMenuItem::new(tr(cx, "打开")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_connection(&id, window, cx);
                                            });
                                        }
                                    }))
                                    // 把当前标签分屏到这条连接（本地 shell 与远端主机都行）：
                                    // 分屏右窗格连谁由这里显式指定，不再是「标签自己的连接」。
                                    .item(PopupMenuItem::new(tr(cx, "在分屏中打开")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_split_with(&id, cx)
                                            });
                                        }
                                    }));
                                if builtin {
                                    return menu;
                                }
                                menu = menu
                                    .item(PopupMenuItem::new(tr(cx, "打开 SFTP 面板")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_sftp_tab(&id, window, cx);
                                            });
                                        }
                                    }))
                                    .item(PopupMenuItem::new(tr(cx, "编辑连接")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_edit_connection(&id, window, cx);
                                            });
                                        }
                                    }));
                                menu.item(PopupMenuItem::new(tr(cx, "删除连接")).on_click({
                                    let id = id.clone();
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.delete_connection(&id, window, cx);
                                        });
                                    }
                                }))
                            }
                        }),
                    );
                }
            }
        }

        if searching && matched == 0 {
            nav = nav.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的连接。"), cx)),
            );
        }

        let total: usize = groups.iter().map(|group| group.items.len()).sum();
        let connection_count = meta_text(
            SharedString::from(trf(cx, "{} 个连接", &[&total.to_string()])),
            cx,
        );

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                h_flex()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "连接"), cx))
                    .child(header_new),
            )
            .child(
                div()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .h(control)
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.xs)
                            .px(tokens.spacing.sm)
                            .rounded(tokens.radius.md)
                            .border_1()
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.background)
                            .child(
                                Icon::new(DuckIcon::Search)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.search)
                                        .appearance(false)
                                        .with_size(Size::Small)
                                        .text_size(tokens.typography.sm.size)
                                        .line_height(tokens.typography.sm.line_height)
                                        .w_full(),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("terminal-connection-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(nav),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.sm)
                    .pb(tokens.spacing.sm)
                    .child(new_connection)
                    .child(h_flex().w_full().justify_center().child(connection_count)),
            )
            .into_any_element()
    }

    /// 会话标签页：图标 + 名称 + 状态点 + 关闭；激活标签以内容区底色「贴」在下方。
    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let mut row = h_flex()
            .min_w_0()
            .flex_1()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden();

        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let (foreground, weight) = if active {
                (tokens.colors.foreground, FontWeight::MEDIUM)
            } else {
                (tokens.colors.muted_foreground, FontWeight::NORMAL)
            };

            let close = toolbar_action_button(
                SharedString::from(format!("terminal-tab-close-{}", tab.id)),
                tr(cx, "关闭标签页"),
                // 标签页内嵌的关闭 ✕ 有意比 `icon.md` 小一档（三个页面的标签条都取 sm，
                // 含工具箱用 `icon_button` 画的那处）；代价是按钮 26×28 不方 —— 已知并接受。
                Icon::new(DuckIcon::X).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.close_tab_at(index, cx));
                }
            });

            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("terminal-tab-{}", tab.id)))
                    .relative()
                    .h(theme.density().control)
                    .min_w_0()
                    .max_w(px(TAB_MAX_WIDTH))
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .pl(tokens.spacing.sm)
                    .pr(tokens.spacing.xs)
                    .rounded(tokens.radius.md)
                    .cursor_pointer()
                    .text_color(foreground)
                    .text_size(tokens.typography.sm.size)
                    .font_weight(weight)
                    // 会话标签是「一组选一个」：显式给读屏 Tab 角色与选中态
                    //（自绘 on_click div 缺 role 时对辅助技术等于不存在）。
                    .role(Role::Tab)
                    .aria_selected(active)
                    .when(active, |this| this.bg(tokens.colors.surface))
                    .hover(move |style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.activate(index, cx));
                        }
                    })
                    // 拖拽源：把标签拖到画布右半屏可并入分屏（见 `render_terminal_canvas`）。
                    // 幽灵视图在拖拽开始时构造一次；顺手记下「在拖谁」，让画布显示投放区。
                    .on_drag(
                        TabDrag {
                            tab_id: tab.id.clone(),
                        },
                        {
                            let view = view.clone();
                            let title = tab.title.clone();
                            move |drag: &TabDrag, _point, _window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if this.dragging_tab.as_deref() != Some(drag.tab_id.as_str()) {
                                        this.dragging_tab = Some(drag.tab_id.clone());
                                        this.split_drop_hover = false;
                                        cx.notify();
                                    }
                                });
                                cx.new(|_| TabDragGhost {
                                    label: title.clone(),
                                })
                            }
                        },
                    )
                    .context_menu({
                        // 菜单闭包是 `'static`（且每次打开都重跑）→ 不能借循环里的 `tab`，
                        // 字段先取出来；`on_click` 各自再克隆一份。
                        let tab_id = tab.id.clone();
                        let tab_kind = tab.kind;
                        let view = view.clone();
                        move |menu, _, cx| {
                            // SFTP 标签没有自己的会话，「重新连接」落在它的父连接上
                            //（SFTP 面板依赖同一条 ControlMaster 物理连接）。
                            let reconnect_target = match tab_kind {
                                TabKind::Ssh => Some(tab_id.clone()),
                                TabKind::Sftp => Some(
                                    tab_id
                                        .strip_suffix("/sftp")
                                        .map_or_else(|| tab_id.clone(), str::to_owned),
                                ),
                                TabKind::Local => None,
                            };
                            let mut menu = menu;
                            if let Some(target) = reconnect_target {
                                menu =
                                    menu.item(PopupMenuItem::new(tr(cx, "重新连接")).on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.reconnect(&target, cx);
                                            });
                                        }
                                    }));
                            }
                            // 只有 SSH 标签有「远端文件」可看：本地 shell 不是一台
                            // 远端主机，SFTP 标签已经在那儿了 —— 两种都不给这一项。
                            if tab_kind == TabKind::Ssh {
                                menu = menu.item(
                                    PopupMenuItem::new(tr(cx, "打开 SFTP 面板")).on_click({
                                        let id = tab_id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.open_sftp_tab(&id, window, cx);
                                            });
                                        }
                                    }),
                                );
                            }
                            menu.separator()
                                .item(PopupMenuItem::new(tr(cx, "关闭标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.close_tab_at(index, cx);
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr(cx, "关闭其他标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.close_other_tabs(index, cx);
                                        });
                                    }
                                }))
                                .item(PopupMenuItem::new(tr(cx, "关闭全部标签页")).on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| this.close_all_tabs(cx));
                                    }
                                }))
                        }
                    })
                    .child(
                        Icon::new(tab.kind.icon())
                            .size(extended.icon.md)
                            .text_color(if active {
                                extended.colors.accent_text
                            } else {
                                tokens.colors.muted_foreground
                            }),
                    )
                    .child(div().min_w_0().truncate().child(tab.title.clone()))
                    .child(
                        div()
                            .size(px(STATUS_DOT_SIZE))
                            .flex_none()
                            .rounded_full()
                            .bg(tab.status.color(cx)),
                    )
                    .child(close),
            );
        }

        h_flex()
            .w_full()
            // 标签条是 chrome：比控件高一个 `sm`，让标签底部与内容区之间留出呼吸位。
            .h(theme.density().control + tokens.spacing.sm)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(row)
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-new-tab",
                            tr(cx, "新建标签页"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.new_local_tab(cx));
                            }
                        }),
                    )
                    .child({
                        // 只对终端会话有意义：SFTP 标签的内容是一对文件面板，没有 PTY
                        // 可分。禁用时把原因写进标签（悬停提示与读屏都会念到它）。
                        let split_label = match self.active_tab() {
                            None => tr(cx, "分屏"),
                            Some(tab) if tab.kind == TabKind::Sftp => {
                                tr(cx, "分屏（仅限终端会话）")
                            }
                            Some(tab) if tab.split.is_some() => tr(cx, "取消分屏"),
                            Some(_) => tr(cx, "分屏"),
                        };
                        let can_split = self
                            .active_tab()
                            .is_some_and(|tab| tab.kind != TabKind::Sftp);
                        toolbar_action_button(
                            "terminal-split",
                            split_label,
                            Icon::new(DuckIcon::Columns2).size(extended.icon.md),
                            false,
                            !can_split,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.toggle_split(cx));
                            }
                        })
                    })
                    .child({
                        // 目标是「当前标签所属的连接」或「上次开过 SFTP 的连接」；
                        // 两个都没有（一台远端主机都还没连过）才置灰，并把原因写进标签。
                        let owner = self.sftp_owner();
                        let label = if owner.is_some() {
                            tr(cx, "打开 SFTP 面板")
                        } else {
                            tr(cx, "打开 SFTP 面板（需先连接远程主机）")
                        };
                        toolbar_action_button(
                            "terminal-open-sftp-tab",
                            label,
                            Icon::new(DuckIcon::Folders).size(extended.icon.md),
                            false,
                            owner.is_none(),
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let Some(id) = this.sftp_owner() else {
                                        return;
                                    };
                                    this.open_sftp_tab(&id, window, cx);
                                });
                            }
                        })
                    }),
            )
            .into_any_element()
    }

    /// 内容区：按激活标签的类型分派（终端画布 / SFTP 面板 / 空态）。
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.active_tab() {
            Some(tab) => match tab.kind {
                TabKind::Sftp => self.render_sftp(cx),
                TabKind::Local | TabKind::Ssh => self.render_terminal(cx),
            },
            None => self.render_empty(cx),
        }
    }

    /// 空态：没有打开任何会话时引导去左栏选连接。
    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .min_h_0()
            .items_center()
            .justify_center()
            .bg(active_theme(cx).tokens().colors.surface)
            .child(
                empty_state(
                    DuckIcon::SquareTerminal,
                    tr(cx, "暂无打开的会话"),
                    Some(SharedString::from(tr(
                        cx,
                        "从左侧选择一个连接，或新建连接后回车打开。",
                    ))),
                    None,
                    cx,
                )
                .border_0(),
            )
            .into_any_element()
    }

    /// 终端工具条：会话信息 + 真实的字号控制与清屏。
    fn render_terminal_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let tab = self.active_tab();
        let title = tab.map_or_else(|| "—".to_owned(), |tab| tab.title.clone());
        let host = tab
            .and_then(|tab| self.connection_entry(&tab.id))
            .map_or_else(|| "localhost".to_owned(), |item| item.endpoint());
        let disabled_down = self.terminal_font_size <= *FONT_SIZE_RANGE.start();
        let disabled_up = self.terminal_font_size >= *FONT_SIZE_RANGE.end();

        h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        Icon::new(tab.map_or(DuckIcon::SquareTerminal, |tab| tab.kind.icon()))
                            .size(extended.icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(body_text(title, cx))
                    .child(meta_text(host, cx)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-font-smaller",
                            tr(cx, "减小字号"),
                            Icon::new(DuckIcon::Minus).size(extended.icon.md),
                            false,
                            disabled_down,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(-1., cx));
                            }
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(READOUT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .font_features(tabular_numbers())
                            .text_color(tokens.colors.muted_foreground)
                            .text_center()
                            .child(format!("{:.0} px", self.terminal_font_size)),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-font-larger",
                            tr(cx, "增大字号"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.md),
                            false,
                            disabled_up,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(1., cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-clear",
                            tr(cx, "清屏"),
                            Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.clear_active(cx));
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 终端内容区：工具条 + 画布。
    ///
    /// 画布这一层要**先**取出来（它需要 `&mut Context`），`active_theme` 返回的是
    /// 全局主题的引用（不是拷贝），一旦借上就借到函数末尾，两者不能重叠。
    fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let toolbar = self.render_terminal_toolbar(cx);
        let canvas = self.render_terminal_canvas(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(toolbar)
            .child(canvas)
            .into_any_element()
    }

    /// 终端画布分派：**本地与 SSH 走同一条路径**。
    ///
    /// SSH 就是一条 `ssh -t user@host` 的 PTY 会话（见 `duck-ssh` 模块头），
    /// 网格、键盘、滚动、选中全部复用，**没有第二套画布**。
    ///
    /// 分屏也复用这一条：右半窗格**是另一条会话**（键见
    /// [`data::split_session_key`]），画的是同一个[`Self::render_terminal_pane`]，
    /// 只是并排放。所以两半各有各的网格尺寸（每个人各自把自己的像素宽度
    /// 换算成列数上报），互不干涉。
    fn render_terminal_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.active_tab() else {
            return div().into_any_element();
        };
        let tab_id = tab.id.clone();
        let kind = tab.kind;
        // 右窗格连谁由归属决定（「不同连接分屏」），类型跟着归属走。
        let split_kind = tab.split_kind().unwrap_or(kind);
        let split = tab.split.clone();
        let Some(split) = split else {
            let pane = self.render_terminal_pane(&tab_id, kind, false, cx);
            // 未分屏的画布就是「拖标签并入分屏」的落点：挂上投放区与松手处理。
            return self.with_split_drop_target(&tab_id, pane, cx);
        };
        // 分隔线：与 SFTP 双栏同一条 1px 发丝线，不引入第二套视觉语言。
        // 主题值先拷成局部量：`active_theme` 的借用不能跨过下面两次 `&mut Context` 调用。
        let extended = active_theme(cx).extended();
        let stroke = extended.stroke.thin;
        let hairline = extended.colors.hairline;

        let left = self.render_terminal_pane(&tab_id, kind, false, cx);
        let right = self.render_terminal_pane(&split, split_kind, true, cx);
        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(div().flex_1().h_full().min_w_0().min_h_0().child(left))
            .child(div().w(stroke).flex_none().h_full().bg(hairline))
            .child(div().flex_1().h_full().min_w_0().min_h_0().child(right))
            .into_any_element()
    }

    /// 给画布包一层「拖拽并入分屏」的投放目标。
    ///
    /// 主流终端的手感：拖着一个标签划过画布、指针**越过中线**时右半屏亮出投放区，
    /// 松手后被拖的标签变成这块画布的分屏右窗格。所以落点判定与视觉共用同一条
    /// 中线：`on_drag_move` 只在指针进入右半时点亮 `split_drop_hover`，
    /// `on_drop` 只在它点亮时并入（左半屏松手 = 取消，什么都不发生）。
    fn with_split_drop_target(
        &self,
        tab_id: &str,
        pane: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = self.view.clone();
        let hovered = self.split_drop_hover && self.split_can_accept(tab_id);
        let zone = self.split_drop_zone(hovered, cx);
        let mut container = div()
            .id(SharedString::from(format!("terminal-drop-{tab_id}")))
            .relative()
            .size_full()
            .min_h_0()
            .child(pane);
        if let Some(zone) = zone {
            container = container.child(zone);
        }
        container
            // 拖拽经过时的悬停跟踪：只按「是否过中线」翻转一个布尔，
            // 变了才 notify（这回调跟着每一次指针移动跑）。
            .on_drag_move::<TabDrag>({
                let view = view.clone();
                move |event: &DragMoveEvent<TabDrag>, _window, cx| {
                    let midline = event.bounds.left() + event.bounds.size.width / 2.;
                    let hovered = event.event.position.x >= midline;
                    let _ = view.update(cx, |this, cx| {
                        if this.split_drop_hover != hovered {
                            this.split_drop_hover = hovered;
                            cx.notify();
                        }
                    });
                }
            })
            // 松手：并入与否由 `finish_tab_drop` 按悬停状态裁决（左半屏松手 = 取消）。
            .on_drop::<TabDrag>({
                let view = view.clone();
                move |drag: &TabDrag, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.finish_tab_drop(drag, cx));
                }
            })
            .into_any_element()
    }

    /// 当前拖拽能否并入 `target_id` 这块画布（投放区显示与并入执行共用同一套判定）。
    fn split_can_accept(&self, target_id: &str) -> bool {
        let Some(dragged_id) = self.dragging_tab.as_deref() else {
            return false;
        };
        if dragged_id == target_id {
            return false;
        }
        // 双方都得是「还没分屏的终端标签」：本应用的分屏是两条会话，没有第三格；
        // SFTP 标签没有 PTY，不参与。
        let acceptable = |tab: &SessionTab| tab.split.is_none() && tab.kind != TabKind::Sftp;
        let Some(dragged) = self.tabs.iter().find(|tab| tab.id == dragged_id) else {
            return false;
        };
        let Some(target) = self.tabs.iter().find(|tab| tab.id == target_id) else {
            return false;
        };
        acceptable(dragged) && acceptable(target)
    }

    /// 右半屏的投放区视觉（虚位以待的一块高亮框）。
    ///
    /// 返回 `None` = 当前没有进行中的标签拖拽，或这块画布收不了 —— 什么都不画，
    /// 画布照常交互。悬停态用强调色薄底 + 强调色文字，与全仓悬停语言一致。
    fn split_drop_zone(&self, hovered: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !cx.has_active_drag() {
            return None;
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 投放区用主题的专用语义色：`drag_border`（拖拽指示线）描边、
        // `drop_target`（拖放目标区）做悬停底色 —— 不另造第二套拖放视觉。
        let (border, fill, label_color) = if hovered {
            (
                extended.colors.drag_border,
                extended.colors.drop_target,
                extended.colors.accent_text,
            )
        } else {
            (
                extended.colors.hairline,
                extended.colors.hover_tint,
                tokens.colors.muted_foreground,
            )
        };
        Some(
            div()
                .absolute()
                .top(tokens.spacing.sm)
                .bottom(tokens.spacing.sm)
                .right(tokens.spacing.sm)
                .w(relative(0.5))
                .flex()
                .items_center()
                .justify_center()
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(border)
                .when(hovered, |this| this.bg(fill))
                .child(meta_text(tr(cx, "松开并入分屏"), cx).text_color(label_color))
                .into_any_element(),
        )
    }

    /// 一个终端窗格：会话画布（必要时带「已断开」提示条）。
    ///
    /// `split` 只影响无障碍名字 —— 两个同名「终端」的输入区对读屏是一团迷雾。
    fn render_terminal_pane(
        &self,
        key: &str,
        kind: TabKind,
        split: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.sessions.get(key) {
            Some(live) => {
                let canvas = self.render_terminal_session(key, live, split, cx);
                // SSH 会话退出（多半是连接失败）时，画布上留着 ssh 打印的原文，
                // 上面再压一条「已断开 + 重新连接」的提示条 ——
                // 用户不必去翻滚动历史确认到底怎么了。
                if live.is_exited() && kind == TabKind::Ssh {
                    v_flex()
                        .size_full()
                        .min_h_0()
                        .child(canvas)
                        .child(self.render_reconnect_bar(key, cx))
                        .into_any_element()
                } else {
                    canvas
                }
            }
            // 会话没起来（PTY 创建失败、档案已删）：如实说明，不留一块空白底。
            None => self.render_terminal_failure(cx),
        }
    }

    /// SSH 断开后的提示条：一句话 + 「重新连接」。
    ///
    /// 不做自动重连：认证失败、主机密钥不匹配这类原因**重连一百次也不会好**，
    /// 自动重试只会把真实错误刷出屏幕，还可能在账号被锁时雪上加霜。
    fn render_reconnect_bar(&self, tab_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let id = tab_id.to_owned();

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(meta_text(tr(cx, "连接已断开。"), cx))
            .child(
                toolbar_action_button(
                    "terminal-reconnect",
                    tr(cx, "重新连接"),
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let id = id.clone();
                    let _ = view.update(cx, |this, cx| this.reconnect(&id, cx));
                }),
            )
            .into_any_element()
    }

    /// 重新建立一条断开的 SSH 会话。
    ///
    /// 先把旧会话从表里摘掉：`ensure_sessions` 只看「表里有没有这个 id」，
    /// 不摘掉的话它会认为「已经起过了」，什么都不会做。
    fn reconnect(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        self.sessions.remove(tab_id);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.status = LinkStatus::Connecting;
        }
        cx.notify();
    }

    /// SFTP 双栏：两侧各自的路径栏 + 文件列表，底部传输队列。
    fn render_sftp(&self, cx: &mut Context<Self>) -> AnyElement {
        // 两侧面板先取（需要 `&mut`）：主题引用一旦借上就借到函数末尾，见 `render_terminal`。
        let left_pane = self.render_file_pane(Side::Local, cx);
        let right_pane = self.render_file_pane(Side::Remote, cx);
        let queue = self.render_transfer_queue(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let stroke = extended.stroke.thin;
        let view = self.view.clone();
        let can_upload = self.pane(Side::Local).selected.is_some();
        let can_download = self.pane(Side::Remote).selected.is_some();
        let title = self
            .active_tab()
            .map_or_else(|| "sftp".to_owned(), |tab| tab.title.clone());

        let toolbar = h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(stroke)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        Icon::new(DuckIcon::Folders)
                            .size(extended.icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(body_text(title, cx))
                    .child(meta_text(tr(cx, "双击目录进入 · 单击选中文件"), cx)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-upload",
                            tr(cx, "上传选中项"),
                            Icon::new(DuckIcon::ArrowUp).size(extended.icon.md),
                            false,
                            !can_upload,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.queue_transfer(true, window, cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-download",
                            tr(cx, "下载选中项"),
                            Icon::new(DuckIcon::ArrowDown).size(extended.icon.md),
                            false,
                            !can_download,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.queue_transfer(false, window, cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-sftp-refresh-both",
                            tr(cx, "刷新两侧"),
                            Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.reload_pane(Side::Local, cx);
                                    this.reload_pane(Side::Remote, cx);
                                });
                            }
                        }),
                    ),
            );

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(left_pane)
                    .child(
                        div()
                            .w(stroke)
                            .flex_none()
                            .h_full()
                            .bg(extended.colors.hairline),
                    )
                    .child(right_pane),
            )
            .child(queue)
            .into_any_element()
    }

    /// 单侧文件面板：面板标题 + 路径栏（上级 / 主目录 / 面包屑 / 打开目录 / 刷新）+ 列表。
    fn render_file_pane(&self, side: Side, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let pane = self.pane(side);
        let can_up = data::parent(&pane.path).is_some();
        let crumbs = data::crumbs(&pane.path);
        // 只展示最后 3 段，更长的用省略号收口：长路径不能把右侧按钮挤出视口。
        let skip = crumbs.len().saturating_sub(3);

        let mut trail = h_flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.xxs)
            .overflow_x_hidden();
        if skip > 0 {
            trail = trail.child(meta_text(tr(cx, "…"), cx)).child(
                Icon::new(DuckIcon::ChevronRight)
                    .size(extended.icon.sm)
                    .text_color(extended.colors.text_tertiary),
            );
        }
        for (index, (path, label)) in crumbs.iter().enumerate().skip(skip) {
            let current = index + 1 == crumbs.len();
            if index > 0 {
                trail = trail.child(
                    Icon::new(DuckIcon::ChevronRight)
                        .size(extended.icon.sm)
                        .text_color(extended.colors.text_tertiary),
                );
            }
            let mut crumb = div()
                .id(SharedString::from(format!(
                    "terminal-crumb-{}-{index}",
                    side.index()
                )))
                .flex_none()
                .px(tokens.spacing.xs)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .child(label.clone());
            if current {
                crumb = crumb
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground);
            } else {
                // 中间段可点：直接跳到那一级（比一路点「上一级」快）。
                let target = path.clone();
                crumb = crumb
                    .cursor_pointer()
                    .text_color(tokens.colors.muted_foreground)
                    .hover(|style| {
                        style
                            .bg(extended.colors.hover_tint)
                            .text_color(extended.colors.accent_text)
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.navigate(side, target.clone(), cx));
                        }
                    });
            }
            trail = trail.child(crumb);
        }

        let path_bar = h_flex()
            .w_full()
            .h(theme.density().control)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.sm)
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-up-{}", side.index())),
                    tr(cx, "上一级目录"),
                    Icon::new(DuckIcon::CornerLeftUp).size(extended.icon.md),
                    false,
                    !can_up,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            let path = this.pane(side).path.clone();
                            if let Some(parent) = data::parent(&path) {
                                this.navigate(side, parent, cx);
                            }
                        });
                    }
                }),
            )
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-home-{}", side.index())),
                    tr(cx, "主目录"),
                    Icon::new(DuckIcon::House).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            let home = side.home();
                            this.navigate(side, home, cx);
                        });
                    }
                }),
            )
            .child(
                // 竖分隔线：宽度取 hairline token（1px、不随缩放变粗），
                // 高度取最大档图标高——与同排按钮的视觉高度对齐。
                div()
                    .w(extended.stroke.thin)
                    .h(extended.icon.lg)
                    .flex_none()
                    .mx(tokens.spacing.xxs)
                    .bg(extended.colors.hairline),
            )
            .child(trail)
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-open-dir-{}", side.index())),
                    tr(cx, "打开目录"),
                    Icon::new(DuckIcon::FolderOpen).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.open_directory(side, window, cx));
                    }
                }),
            )
            .child(
                toolbar_action_button(
                    SharedString::from(format!("terminal-refresh-{}", side.index())),
                    tr(cx, "刷新"),
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.reload_pane(side, cx));
                    }
                }),
            );

        let column_header = h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(caption_heading(tr(cx, "名称"), cx)),
            )
            .child(
                div()
                    .w(px(SIZE_COLUMN_WIDTH))
                    .flex_none()
                    .child(caption_heading(tr(cx, "大小"), cx)),
            )
            .child(
                div()
                    .w(px(MODIFIED_COLUMN_WIDTH))
                    .flex_none()
                    .child(caption_heading(tr(cx, "修改时间"), cx)),
            );

        let rows = v_flex().w_full().children(pane.entries.iter().map(|entry| {
            let selected = pane.selected.as_deref() == Some(entry.name.as_str());
            let name = entry.name.clone();
            let is_dir = entry.dir;
            h_flex()
                .id(SharedString::from(format!(
                    "terminal-file-{}-{name}",
                    side.index()
                )))
                .w_full()
                .h(theme.density().nav_row)
                .flex_none()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .cursor_pointer()
                .rounded(tokens.radius.sm)
                // 文件行是「可激活的条目」：给读屏一个显式角色与名字
                //（自绘 on_click div 缺 role 时对辅助技术等于不存在）。
                .role(Role::ListItem)
                .aria_label(SharedString::from(entry.name.clone()))
                .when(selected, |this| {
                    this.bg(extended.colors.nav_selected)
                        .text_color(extended.colors.nav_selected_foreground)
                })
                .when(!selected, |this| {
                    this.hover(|style| style.bg(extended.colors.hover_tint))
                })
                .on_click({
                    let view = view.clone();
                    move |event: &ClickEvent, window, cx| {
                        // 双击进入目录：`down.click_count` 是平台给的连击计数（第二次按下为 2）。
                        let double =
                            matches!(event, ClickEvent::Mouse(mouse) if mouse.down.click_count > 1);
                        let name = name.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.click_entry(side, name, double, window, cx);
                        });
                    }
                })
                .child(
                    Icon::new(if is_dir {
                        DuckIcon::FolderOpen
                    } else {
                        DuckIcon::File
                    })
                    .size(extended.icon.md)
                    .text_color(if selected {
                        extended.colors.nav_selected_icon
                    } else {
                        tokens.colors.muted_foreground
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .line_height(tokens.typography.sm.line_height)
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .w(px(SIZE_COLUMN_WIDTH))
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if selected {
                            extended.colors.nav_selected_foreground
                        } else {
                            tokens.colors.muted_foreground
                        })
                        .child(entry.size.clone()),
                )
                .child(
                    div()
                        .w(px(MODIFIED_COLUMN_WIDTH))
                        .flex_none()
                        .font_features(tabular_numbers())
                        .text_size(tokens.typography.xs.size)
                        .line_height(tokens.typography.xs.line_height)
                        .text_color(if selected {
                            extended.colors.nav_selected_foreground
                        } else {
                            extended.colors.text_tertiary
                        })
                        .child(entry.modified.clone()),
                )
        }));

        // 空目录 / 读失败 / 未连接：说明**就写在列表位置**，不留一片空白让人以为界面坏了。
        let list: AnyElement = if pane.entries.is_empty() {
            v_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.xs)
                .py(tokens.spacing.xl)
                // 有说明就只说说明 ——「尚未连接」「没有权限读取」比「空目录」准确得多；
                // 没有说明时，才是真的空目录。
                .when(pane.notice.is_none(), |this| {
                    this.child(meta_text(tr(cx, "这个目录里没有可显示的条目。"), cx))
                })
                .children(
                    pane.notice
                        .as_ref()
                        .map(|notice| meta_text(notice.render(cx), cx)),
                )
                .into_any_element()
        } else {
            v_flex()
                .w_full()
                .when_some(pane.notice.as_ref(), |this, notice| {
                    this.child(
                        div()
                            .w_full()
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xs)
                            .child(meta_text(notice.render(cx), cx)),
                    )
                })
                .child(rows)
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                h_flex()
                    .w_full()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, side.title()), cx))
                    .child(meta_text(
                        SharedString::from(trf(cx, "{} 项", &[&pane.entries.len().to_string()])),
                        cx,
                    )),
            )
            .child(path_bar)
            .child(column_header)
            .child(
                div()
                    .id(SharedString::from(format!(
                        "terminal-file-list-{}",
                        side.index()
                    )))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(tokens.spacing.xxs)
                    .child(list),
            )
            .into_any_element()
    }

    /// 传输队列：一行一条，含方向图标、进度条与状态标签。
    fn render_transfer_queue(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let running = self
            .transfers
            .iter()
            .filter(|transfer| transfer.state == data::TransferState::Running)
            .count();

        let rows = v_flex()
            .w_full()
            .children(self.transfers.iter().map(|transfer| {
                let active = transfer.state == data::TransferState::Running;
                // `scp` 不给中间进度，所以读数只有 0% / 100% 两种 ——
                // 传的过程中显示「—」，而不是一个会跳动的假百分比。
                let number = match transfer.state {
                    data::TransferState::Done => "100%".to_owned(),
                    data::TransferState::Running => "—".to_owned(),
                    _ => "0%".to_owned(),
                };
                h_flex()
                    .id(SharedString::from(format!(
                        "terminal-transfer-{}",
                        transfer.id
                    )))
                    .w_full()
                    .h(theme.density().nav_row)
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .px(tokens.spacing.sm)
                    .child(
                        Icon::new(if transfer.upload {
                            DuckIcon::ArrowUp
                        } else {
                            DuckIcon::ArrowDown
                        })
                        .size(extended.icon.md)
                        .text_color(if active {
                            extended.colors.accent_text
                        } else {
                            tokens.colors.muted_foreground
                        }),
                    )
                    .child(
                        div()
                            .w(px(TRANSFER_NAME_WIDTH))
                            .flex_none()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(transfer.name.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(progress_bar(
                        SharedString::from(format!("terminal-progress-{}", transfer.name)),
                        transfer.progress,
                        cx,
                    )))
                    .child(
                        div()
                            .w(px(READOUT_WIDTH))
                            .flex_none()
                            .font_features(tabular_numbers())
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.muted_foreground)
                            .child(number),
                    )
                    .child(div().flex_none().child(match transfer.state {
                        data::TransferState::Done => {
                            accent_tag(tr(cx, "已完成"), cx).into_any_element()
                        }
                        data::TransferState::Failed => {
                            // 失败时把原因带出来：一句「失败」什么也没说，
                            // 而 `scp` 的原文指得出下一步（权限？磁盘满？路径不对？）。
                            let reason = transfer
                                .reason
                                .clone()
                                .unwrap_or_else(|| tr(cx, "传输失败").to_owned());
                            neutral_tag(reason, cx).into_any_element()
                        }
                        other => neutral_tag(tr(cx, other.label()), cx).into_any_element(),
                    }))
            }));

        v_flex()
            .w_full()
            .flex_none()
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .w_full()
                    .h(theme.density().section_header)
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "传输队列"), cx))
                    .child(meta_text(
                        SharedString::from(trf(
                            cx,
                            "{}/{} 个任务",
                            &[&running.to_string(), &self.transfers.len().to_string()],
                        )),
                        cx,
                    )),
            )
            .child(rows)
            .into_any_element()
    }

    /// 底部状态栏：左侧连接信息，右侧传输速率。SFTP 标签改报目录信息。
    fn render_status_bar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let spacing = tokens.spacing;
        let muted = tokens.colors.muted_foreground;
        let meta_size = tokens.typography.xs.size;
        let meta_line_height = tokens.typography.xs.line_height;
        let icon_size = extended.icon.sm;

        let tab = self.active_tab();
        let (status, host) = tab.map_or((LinkStatus::Disconnected, "—".to_owned()), |tab| {
            let host = self
                .connection_entry(&tab.id)
                .map_or_else(|| "localhost".to_owned(), |item| item.endpoint());
            (tab.status, host)
        });
        let is_sftp = tab.is_some_and(|tab| tab.kind == TabKind::Sftp);

        let cell = move |icon: DuckIcon, text: String| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xxs)
                .child(Icon::new(icon).size(icon_size).text_color(muted))
                .child(text)
        };

        let mut left = h_flex().min_w_0().items_center().gap(spacing.md).child(
            h_flex()
                .flex_none()
                .items_center()
                .gap(spacing.xs)
                .child(
                    div()
                        .size(px(STATUS_DOT_SIZE))
                        .flex_none()
                        .rounded_full()
                        .bg(status.color(cx)),
                )
                .child(tr(cx, status.label()))
                .child(host),
        );
        if is_sftp {
            left = left
                .child(cell(
                    DuckIcon::HardDrive,
                    self.pane(Side::Local).path.clone(),
                ))
                .child(cell(
                    DuckIcon::Folders,
                    // 远端路径为空 = 还没列举出来，显示「—」比显示空白清楚。
                    if self.pane(Side::Remote).path.trim().is_empty() {
                        "—".to_owned()
                    } else {
                        self.pane(Side::Remote).path.clone()
                    },
                ));
        } else {
            // 网格与回滚行数都是**真实读数**：前者来自内核的 `Dims`，
            // 后者来自起会话时的参数 —— 都不是写死展示的数字。
            let (columns, rows) = tab
                .and_then(|tab| self.sessions.get(&tab.id))
                .map_or((0, 0), TerminalSession::grid);
            let scrollback = duck_term::SpawnOptions::default().scrollback;
            left = left.child(cell(DuckIcon::Globe, tr(cx, "UTF-8").to_owned()));
            if columns > 0 {
                left = left.child(cell(DuckIcon::Columns2, format!("{columns}×{rows}")));
            }
            left = left.child(cell(
                DuckIcon::Layers,
                trf(cx, "回滚 {} 行", &[&scrollback.to_string()]),
            ));
        }

        h_flex()
            .w_full()
            .h(theme.density().status_bar)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(spacing.md)
            .px(spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .text_size(meta_size)
            .line_height(meta_line_height)
            .font_features(tabular_numbers())
            .text_color(muted)
            .child(left)
            // 右侧只放**确定的事实**：应用版本来自编译期。
            // 此前这里挂的是「1.2 MB/s / 486 KB/s」两个写死的速率读数 ——
            // 终端没有可用的真实速率来源，宁可空着，也不摆两个会误导人的数字。
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(spacing.md)
                    .child(trf(cx, "v{}", &[env!("CARGO_PKG_VERSION")])),
            )
            .into_any_element()
    }

    /// 右侧栏内容：会话详情 + 传输队列摘要 + 快捷键。
    pub(crate) fn render_right_sidebar(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let lang = theme.lang();
        let view = self.view.clone();

        let Some(tab) = self.active_tab() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .px(tokens.spacing.lg)
                .child(meta_text(
                    tr(cx, "打开一个会话后，这里显示连接详情与传输队列。"),
                    cx,
                ))
                .into_any_element();
        };

        let connection = self.connection_entry(&tab.id);

        // 连接时长是**真实计时**：从会话起来的时刻算起（没有会话就是 `—`）。
        let uptime = self.sessions.get(&tab.id).map_or_else(
            || "—".to_owned(),
            |live| human_duration(live.elapsed(), lang),
        );

        let mut info = vec![
            description_row(t(lang, "会话"), body_text(tab.title.clone(), cx)),
            description_row(t(lang, "类型"), body_text(t(lang, tab.kind.label()), cx)),
            description_row(
                t(lang, "主机"),
                body_text(
                    connection
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |item| item.endpoint()),
                    cx,
                ),
            ),
            description_row(
                t(lang, "端口"),
                body_text(
                    connection
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |item| item.port.clone()),
                    cx,
                ),
            ),
            description_row(
                t(lang, "认证方式"),
                body_text(
                    connection.as_ref().map_or_else(
                        || t(lang, "—").to_owned(),
                        |item| format!("{} · {}", t(lang, item.auth.label()), item.auth_detail),
                    ),
                    cx,
                ),
            ),
            description_row(t(lang, "已连接"), body_text(uptime, cx)),
            description_row(
                t(lang, "状态"),
                status_cell(tab.status.color(cx), t(lang, tab.status.label()), cx),
            ),
        ];
        if tab.kind == TabKind::Sftp {
            info.push(description_row(
                t(lang, "本地目录"),
                body_text(self.pane(Side::Local).path.clone(), cx),
            ));
            info.push(description_row(
                t(lang, "远端目录"),
                body_text(self.pane(Side::Remote).path.clone(), cx),
            ));
        }

        let transfers = v_flex()
            .w_full()
            .children(self.transfers.iter().map(|transfer| {
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(
                        Icon::new(if transfer.upload {
                            DuckIcon::ArrowUp
                        } else {
                            DuckIcon::ArrowDown
                        })
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(transfer.name.clone()),
                    )
                    .child(meta_text(tr(cx, transfer.state.label()), cx))
            }));

        // 会话对应的连接 id（SFTP 标签取它的父连接）。
        let owner = tab
            .id
            .strip_suffix("/sftp")
            .unwrap_or(tab.id.as_str())
            .to_owned();
        // 内置的本地 shell 没有档案可编辑（`builtin` 就是为这句话存在的）。
        let editable = self
            .connection_entry(&owner)
            .is_some_and(|entry| !entry.builtin);

        let mut actions = h_flex().w_full().gap(tokens.spacing.xs).flex_wrap();
        // 只有远程会话才给「打开 SFTP 面板」：本地外壳没有远端文件可传。
        if tab.kind != TabKind::Local {
            let view = view.clone();
            let owner = owner.clone();
            actions = actions.child(
                button(
                    "terminal-right-open-sftp",
                    tr(cx, "打开 SFTP 面板"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.open_sftp_tab(&owner, window, cx));
                }),
            );
        }
        if editable {
            let view_edit = view.clone();
            let owner_edit = owner.clone();
            actions = actions.child(
                button(
                    "terminal-right-edit",
                    tr(cx, "编辑连接"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view_edit.update(cx, |this, cx| {
                        this.open_edit_connection(&owner_edit, window, cx);
                    });
                }),
            );
            let view_delete = view.clone();
            let owner_delete = owner.clone();
            actions = actions.child(
                button(
                    "terminal-right-delete",
                    tr(cx, "删除连接"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = view_delete.update(cx, |this, cx| {
                        this.delete_connection(&owner_delete, window, cx);
                    });
                }),
            );
        }

        v_flex()
            .id("terminal-right-sidebar")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .gap(tokens.spacing.lg)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.md)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.sm)
                    .child(caption_heading(tr(cx, "会话详情"), cx))
                    .child(
                        card(cx)
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.sm)
                            .child(description_list(info)),
                    )
                    .child(actions),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "传输队列"), cx))
                    .child(transfers),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(caption_heading(tr(cx, "快捷键"), cx))
                    // 这里只列**真的绑了**的键。此前写的是「新建标签 ⌘T · 关闭标签 ⌘W ·
                    // 分屏 ⌘D」—— 三个一条都没绑（`app::install` 的 `"Terminal"` 上下文里
                    // 只有下面这些），等于教用户按三个没反应的键。
                    // 2026-10-12 补绑 `⌘T` / `⌘W` / `⌘C` / `⌘V` / `⌘A` 之后，
                    // 这一组与 `app::install` 的 `"Terminal"` 绑定表**逐条对应**。
                    .child(meta_text(tr(cx, "新建标签 ⌘T · 关闭标签 ⌘W"), cx))
                    .child(meta_text(
                        tr(cx, "复制 ⌘C（拖选文字后）· 粘贴 ⌘V · 全选 ⌘A"),
                        cx,
                    ))
                    .child(meta_text(
                        tr(cx, "清屏 ⌘K · 翻页 Shift+PageUp / Shift+PageDown"),
                        cx,
                    ))
                    .child(meta_text(tr(cx, "分屏 ⌘D（左右各一条独立会话）"), cx))
                    .child(meta_text(tr(cx, "行首 / 行尾 ⌘← / ⌘→"), cx))
                    .child(meta_text(tr(cx, "字号 ⌘+ / ⌘- / ⌘0"), cx))
                    .child(meta_text(tr(cx, "文件：单击选中 · 双击进入目录"), cx)),
            )
            .into_any_element()
    }
}

impl Render for TerminalPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 渲染前的五步「同步」（顺序不能换）：
        // 1. 补会话 —— 标签只负责「存在」，连接在这里落地；
        // 2. 同步状态 —— 标签上的状态点必须反映真实会话，而不是点击时的乐观值；
        // 3. 补远端列举 —— 只读一次，跑在后台执行器上；
        // 4. 量字体 —— 单元格尺寸只跟字号 / 主题有关，一次算给所有会话；
        // 5. 交焦点 —— 只在刚切到会话标签时接管一次键盘。
        self.ensure_sessions(cx);
        self.sync_tab_status();
        self.ensure_remote_listing(cx);
        self.sync_cell_metrics(window, cx);
        self.sync_canvas_focus(window, cx);

        let content = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(self.render_tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(self.render_body(cx)),
            );

        let mut panel = SidebarPanel::new("terminal-content", &self.sidebar, content, window, cx);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户实测报告：「SFTP 面板打开后关掉，再点图标就打不开了」。
    ///
    /// 根因是目标只从**当前标签**推导 —— 面板关掉后活动标签回落到本地 shell（或空态），
    /// 于是图标要么禁用、要么对着本地 shell 开出一个永远列举不出东西的假面板。
    /// 这条钉住的是修复的核心：**标签没了，目标还在**。
    #[test]
    fn sftp_target_survives_closing_the_panel() {
        let real = |id: &str| id != data::LOCAL_SHELL_ID;
        // 面板已关（当前标签是本地 shell），但上次开过 `prod-01`。
        assert_eq!(
            resolve_sftp_owner(Some(data::LOCAL_SHELL_ID), Some("prod-01"), &real).as_deref(),
            Some("prod-01"),
            "面板关掉后仍应记得上次的连接"
        );
        // 连标签都没有了（只剩记忆）。
        assert_eq!(
            resolve_sftp_owner(None, Some("prod-01"), &real).as_deref(),
            Some("prod-01")
        );
        // 当前标签就是一条远端连接时，以它为准（用户看得见的那条优先）。
        assert_eq!(
            resolve_sftp_owner(Some("staging-02"), Some("prod-01"), &real).as_deref(),
            Some("staging-02")
        );
    }

    #[test]
    fn sftp_target_refuses_local_shell_and_missing_profiles() {
        let real = |id: &str| id != data::LOCAL_SHELL_ID;
        // 本地 shell 不是远端主机 —— 一个候选都不能剩下。
        assert_eq!(
            resolve_sftp_owner(Some(data::LOCAL_SHELL_ID), None, &real),
            None
        );
        assert_eq!(
            resolve_sftp_owner(
                Some(data::LOCAL_SHELL_ID),
                Some(data::LOCAL_SHELL_ID),
                &real
            ),
            None
        );
        // 档案已删（`usable` 说不行）时越过它，而不是拿一条查不到的记录去开面板。
        let only_prod = |id: &str| id == "prod-01";
        assert_eq!(
            resolve_sftp_owner(Some("gone-01"), Some("prod-01"), &only_prod).as_deref(),
            Some("prod-01")
        );
        assert_eq!(resolve_sftp_owner(Some("gone-01"), None, &only_prod), None);
    }

    /// 分屏窗格的会话键不能撞上 SFTP 标签的键，也不能等于任何标签 id
    ///（否则 `sessions` 表里两个角色共用一个键，互相覆盖会话）。
    #[test]
    fn split_key_never_collides_with_sftp_or_tab_ids() {
        let id = "prod-01";
        let split = data::split_session_key(id);
        assert_ne!(split, id);
        assert_ne!(split, format!("{id}/sftp"));
        // 拼接是幂等的：同一标签反复开分屏拿到同一个键（会话表才不会攒下一堆孤儿）。
        assert_eq!(data::split_session_key(id), split);
    }
}
