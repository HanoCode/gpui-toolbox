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
//!
//! ---
//!
//! # 文件划分（P3-3，2026-10-14 拆自单文件 `terminal.rs`，零行为变更）
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `mod.rs` | 模块文档 / use / 常量 / 全部类型 / `impl Render` / 页面壳方法 |
//! | `helpers.rs` | 自由函数：字体初值、口令读写、SFTP 目标选择、时长与错误文案、传输进度换算、克隆命名 |
//! | `conn.rs` | 连接档案生命周期：端点、ssh 目标、新建 / 编辑 / 克隆 / 删除、持久化 |
//! | `tabs.rs` | 会话标签页：激活 / 关闭 / 分组、拖放落点、状态同步 |
//! | `split.rs` | 分屏：开合、窗格比例与分隔条、拖放落点与窗格渲染 |
//! | `sftp.rs` | SFTP：入口目标、目录列举与浏览、远端新建 / 重命名 / 删除 |
//! | `transfer.rs` | 传输队列：入队 / 上传 / 执行 / 取消 / 重试 / 进度 |
//! | `find.rs` | 终端查找：开关、刷新与高亮、跳转与节流 |
//! | `render_sidebar.rs` | 左栏连接列表 |
//! | `render_tabs.rs` | 标签栏与主体骨架（含空态） |
//! | `render_terminal.rs` | 终端区：工具条、画布、查找条、重连条 |
//! | `render_sftp.rs` | SFTP 双栏与传输队列 |
//! | `render_status.rs` | 状态栏与 shell 级右栏详情 |
//! | `tests.rs` | 模块单测 |
//!
//! 三条口径（与 `pages/settings/`、`pages/redis/` 相同）：
//! 1. 子模块靠 `use super::*;` 取父模块的私有 `use` 绑定与私有类型，不必复制 use 块；
//! 2. 子模块里 `pub(super)` == 可见于 `terminal` 及其全部后代，与原来「模块私有」同一范围；
//! 3. 原 `pub(super)`（可见于 `pages`）的方法搬出后**升为 `pub(crate)`** —— 因为
//!    `pages::terminal_canvas` 与 `crate::actions` 是**兄弟/外部**模块，得继续能调。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use duck_components::{
    ButtonVariant, DuckIcon, SidebarPanel, SidebarState, body_text, button, caption_heading, card,
    content::{
        accent_tag, description_list, description_row, empty_state, neutral_tag, progress_bar,
        status_cell,
    },
    dialog_footer, dialog_top_offset, icon_button, icon_text_button, meta_text, modal_drag,
    modal_drag_area, modal_offset, modal_title_row, notify, selectable_text,
    sidebar_navigation_button, tabular_numbers, toolbar_action_button,
};
use duck_theme::{Lang, active_theme, t, tf, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, CursorStyle, DragMoveEvent, Entity,
    ExternalPaths, Focusable as _, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_component::{
    Icon, Sizable as _, Size, WindowExt as _, h_flex,
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
    ForwardDraft, NameDialogKind, NewConnection, open_connection_dialog, open_directory_dialog,
    open_entry_name_dialog,
};

mod conn;
mod find;
mod helpers;
mod render_sftp;
mod render_sidebar;
mod render_status;
mod render_tabs;
mod render_terminal;
mod sftp;
mod split;
mod tabs;
mod transfer;

// 含自由函数的子模块，名字要转出到父模块供兄弟模块取用。
use self::helpers::*;

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
/// 传输进度的轮询间隔。
///
/// `scp` 不报中间进度，只能自己量落点大小。300ms 是这笔账的折中：更密会让
/// 每次量远端大小都起一个 `ssh`（哪怕有 `ControlMaster` 复用，也仍有往返开销），
/// 更疏则进度条看起来像卡住了。传输本身完全不受影响 —— 量的是**另一个**进程。
const PROGRESS_POLL: Duration = Duration::from_millis(300);
/// 缓冲区搜索在两帧之间的最小重算间隔。
///
/// 终端一直在输出，而**每次输出都会把已有的命中整体挪位**（回滚区行号是相对的），
/// 不重算高亮就会「框住的不是那几个字」。但每帧都全网格重搜（10k 行回滚）太贵，
/// 按这个间隔节流：4Hz 的重算对眼睛来说已经是连续的。
const FIND_REFRESH_MIN: Duration = Duration::from_millis(250);
/// 缓冲区搜索输入框宽度：够看清一段命令，又不至于把搜索条拉满整行。
const FIND_INPUT_WIDTH: f32 = 260.;

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

/// 一次远端文件系统改动（新建目录 / 删除 / 重命名）。
///
/// 打包成一个枚举而不是三个各自 `spawn` 的方法：三者共用同一条「后台跑 → 回主线程
/// 刷面板」的通道，分叉只在真正调用的那一个 `duck_ssh` 函数上。
#[derive(Clone)]
enum RemoteMutation {
    /// 新建目录（不覆盖同名目录，见 [`duck_ssh::mkdir`]）。
    Mkdir { path: String },
    /// 删除文件或空目录（`remove` 内部**不加** `-r`）。
    Remove { path: String },
    /// 重命名 / 移动。
    Rename { from: String, to: String },
}

/// 需要用户确认后才执行的破坏性动作。
///
/// 参数在构造时**冻结**（不持有对连接表的引用）：确认框可能开着好几秒，
/// 期间用户完全可能去改列表 —— 那时执行的动作必须还是他按下确认的那一个。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    /// 删除一条连接档案。
    DeleteConnection { id: String, label: String },
    /// 删除远端文件 / 空目录。
    RemoveRemote { path: String, name: String },
}

impl PendingAction {
    /// 动作涉及的条目显示名（给确认文案用）。
    fn label(&self) -> String {
        match self {
            Self::DeleteConnection { label, .. } => label.clone(),
            Self::RemoveRemote { name, .. } => name.clone(),
        }
    }
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
///（图标 + 名字），比拖着一块透明区域更有「我在拖一个东西」的实感。
/// 标签拖拽与连接拖拽共用同一张卡片，只换图标。
struct TabDragGhost {
    /// 幽灵卡片上的图标。
    icon: DuckIcon,
    /// 被拖对象的名字。
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
            .child(Icon::new(self.icon).size(extended.icon.sm))
            .child(self.label.clone())
    }
}

/// 连接拖拽的载荷类型（左栏「拖连接换分组」；`on_drag` / `on_drop` 靠它对上暗号）。
///
/// 与 [`TabDrag`] / [`PaneDrag`] 是三个类型：拖连接时不能触发「拖标签并入分屏」
/// 的投放区，gpui 按载荷类型分派拖拽事件，类型分开就互不串扰。
/// 只带档案 id：拖拽期间列表会随搜索 / 分组重排，id 才是稳定标识。
struct ConnDrag {
    /// 被拖拽的连接档案 id。
    profile_id: String,
}

/// 双栏隔离栏拖拽的载荷类型（`on_drag` / `on_drag_move` / `on_drop` 靠它对上暗号）。
///
/// 与 [`TabDrag`] 是两个类型：拖隔离栏时不能触发「拖标签并入分屏」的投放区，
/// 反之亦然 —— gpui 按载荷类型分派拖拽事件，类型分开就互不串扰。
#[derive(Debug)]
pub(super) struct PaneDrag;

/// 隔离栏拖拽的幽灵视图：刻意渲染成 0×0。
///
/// 拖隔离栏时比例实时跟手，画面上已经有可见反馈（左右两栏在变宽变窄），
/// 再跟一个幽灵缩略图是纯噪声。
struct PaneDragGhost;

impl Render for PaneDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 双栏两侧各自的最小宽度（px）：拖到这个宽度就停，谁也别把谁挤没。
const MIN_PANE_WIDTH: f32 = 160.;

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
        // 空路径是**远端专有**的一个合法状态：「家目录还没解析出来」。
        // 远端家目录不可知（见 `ensure_remote_listing`），所以「主目录」按钮交出来的
        // 就是空串；而 `normalize("")` 会把它变成 `/` —— 于是「主目录」把面板带到根目录。
        // 这里保留空串，让列举那条路去解析 `~`。
        self.path = if side.is_local() || !path.trim().is_empty() {
            data::normalize(&path)
        } else {
            String::new()
        };
        self.selected = None;
        if side.is_local() {
            self.reload(side);
        } else {
            // 远端：把 `loaded` 打回 false，让渲染前的 `ensure_remote_listing` 重新列举。
            self.loaded = false;
        }
    }
}

/// 缓冲区搜索（`⌘F`）的运行时状态。
///
/// 只属于**一块画布**（`session` = 会话键）：分屏时两半是两条独立会话，
/// 搜索结果不该互相串。`⌘F` 的目标取「用户最后点过的窗格」，没有就退回当前标签。
struct FindState {
    /// 搜索输入框。
    input: Entity<InputState>,
    /// 目标会话键。
    session: String,
    /// 命中（网格坐标：首行, 首列, 末行, 末列；行号可为负 = 回滚区）。
    ///
    /// 存 `i32 / usize` 而不是内核的 `Point`：界面层不需要认识网格类型，
    /// 换算在 `refresh_find` 一处做掉。
    matches: Vec<(i32, usize, i32, usize)>,
    /// 当前命中的下标（`matches` 非空时有效）。
    current: usize,
    /// 上次重算命中的时刻（见 [`FIND_REFRESH_MIN`]）。
    refreshed: Instant,
    /// 输入变更 / 回车订阅；持有句柄仅为延长生命周期。
    _subscriptions: Vec<Subscription>,
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
    ///
    /// 键是分组标识；自定义分组的标识就是它的名字（`String`），没有 `'static` 可借。
    expanded: HashSet<String>,
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
    /// 拖选自动滚动的心跳任务（与 [`Self::poll_task`] 同一套「弱引用自判退出」的规矩）。
    ///
    /// 单独立一个任务而不是搭在 `poll_task` 上：那个是 16ms 一帧，
    /// 拿它当滚动时钟会让视口一帧滚好几行，用户停不到想选的那一行。
    pub(super) autoscroll_task: Option<Task<()>>,
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
    /// 正在被拖拽的连接档案 id（左栏「拖连接换分组」进行中）。
    ///
    /// 只用来让左栏决定「要不要把空分组也渲染出来」——不渲染的话，把某组
    /// 最后一条连接拖走后这个组就从界面上消失了，永远拖不回来。悬置的旧值
    /// 无害：渲染条件里还有一道 `cx.has_active_drag()`，松手（无论落没落进
    /// 分组）下一帧就不再显示。
    ///
    /// ⚠️ `has_active_drag()` 只答「有没有拖拽」、认不出载荷类型，所以另一类
    /// 拖拽的**拖拽源**负责把这个残留清掉（标签拖拽见 `render_tabs`）。已知
    /// 残留：外部文件拖进画布（`ExternalPaths`）时没有 ConnDrag 事件经过，
    /// 若上一轮连接拖拽是在空白处松手的，左栏会短暂亮着上次悬停的分组 ——
    /// 纯粹是视觉残留，不写存储、不落组。
    dragging_conn: Option<String>,
    /// 连接拖拽指针正悬停的分组（落组高亮）。
    ///
    /// 由分组区的 `on_drag_move` 随指针更新；`on_drop` 只落进它指向的分组，
    /// 落完即清。渲染高亮同样要过 `cx.has_active_drag()` 这道闸。
    conn_drop_hover: Option<String>,
    /// 双栏隔离栏的左右占比（左栏宽 / 总宽）。
    ///
    /// 分屏双终端与 SFTP 双栏共用同一份：两条隔离栏是同一个交互（同宽命中带、
    /// 同手感），比例互相继承是特性而不是泄漏。`None` = 没拖过（默认各半）；
    /// 页面生命周期内记忆，重启后回各半 —— 双栏比例不值得进 kv。
    divider_ratio: Option<f32>,
    /// 隔离栏**正在被拖动**（拖拽源登记，`on_drop` 清除）。
    ///
    /// 拖动中指针会离开隔离栏的命中带，悬停态（hover）靠不住；用这个标记
    /// 让隔离栏在整个拖拽期间保持「抓住」的视觉。
    resizing_pane: bool,
    /// 远端列举是否正在进行中。
    ///
    /// `render` 每帧都会问一次「远端面板加载了吗」，没有这个标记就会**每帧起一个
    /// `ssh` 进程**——界面看起来只是慢，实际在疯狂 fork。
    remote_listing_inflight: bool,
    /// 传输任务的自增编号（队列行的 ElementId 与结果回填都靠它）。
    next_transfer_id: u64,
    /// 传输行的**可中断句柄**（取消时对它 `kill`）。
    ///
    /// 键是队列行 id。句柄在 [`Self::spawn_transfer`] 里**同步登记**（`Command::spawn`
    /// 不阻塞，只有等结果才阻塞），所以界面上「传输中」的行永远能立刻取消。
    /// 传输落地（完成 / 取消 / 失败）后由后台任务回主线程移除。
    transfer_handles: HashMap<u64, duck_ssh::TransferHandle>,
    /// 传输行的**输入快照**（失败后「重试」用它重放同一条传输）。
    ///
    /// 成功 / 取消后移除；失败时保留 —— 重试就是拿它再跑一遍。
    transfer_requests: HashMap<u64, (TransferRequest, RemoteHandle)>,
    /// 缓冲区搜索条。`None` = 没打开（`⌘F` 打开，`Esc` / 关闭按钮收起）。
    find: Option<FindState>,
    /// 自引用：右栏等只有 `&App` 的地方要靠它把点击送回来。
    pub(super) view: WeakEntity<Self>,
    /// 搜索词变更订阅；持有句柄仅为延长生命周期。
    _subscriptions: Vec<Subscription>,
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
        self.sync_focus_reporting(window, cx);
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
            terminal_font_size: initial_font_size(cx),
            sessions: HashMap::new(),
            poll_task: None,
            autoscroll_task: None,
            focused_tab: None,
            focus_pane: None,
            last_sftp_owner: None,
            dragging_tab: None,
            split_drop_hover: false,
            dragging_conn: None,
            conn_drop_hover: None,
            divider_ratio: None,
            resizing_pane: false,
            remote_listing_inflight: false,
            next_transfer_id: 1,
            transfer_handles: HashMap::new(),
            transfer_requests: HashMap::new(),
            find: None,
            view: cx.entity().downgrade(),
            _subscriptions: subscriptions,
        };
        // 轮询任务只能在渲染之外起：渲染期间 `App` 已经被借出，再 `spawn` 就是重入。
        // 这里起一次就够了 —— 任务自己按「有没有会话」快慢变速，不随标签增删重启。
        this.start_terminal_poller(cx);
        this.start_autoscroll_poller(cx);
        this
    }

    /// 供 shell 顶栏共享的侧栏状态。
    #[must_use]
    pub fn sidebar_state(&self) -> Entity<SidebarState> {
        self.sidebar.clone()
    }

    /// 调整终端画布字号（钳在 [`FONT_SIZE_RANGE`] 内）。
    ///
    /// `pub(crate)`：画布层（`terminal_canvas`）的 `ZoomIn` / `ZoomOut` 动作与命令面板都走这里，
    /// 与工具条 ± 按钮共用同一份状态与钳位。
    pub(crate) fn nudge_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
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
    pub(crate) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        let next = active_theme(cx).tokens().typography.mono_md.size.to_f64() as f32;
        if (next - self.terminal_font_size).abs() > f32::EPSILON {
            self.terminal_font_size = next;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests;
