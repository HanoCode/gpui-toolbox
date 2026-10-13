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
    dialog_top_offset, icon_button, icon_text_button, meta_text, modal_drag, modal_drag_area,
    modal_offset, modal_title_row, notify, sidebar_navigation_button, tabular_numbers,
    toolbar_action_button,
};
use duck_theme::{Lang, active_theme, t, tf, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, CursorStyle, DragMoveEvent, Entity,
    ExternalPaths, Focusable as _, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
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
    ForwardDraft, NameDialogKind, NewConnection, open_connection_dialog, open_directory_dialog,
    open_entry_name_dialog,
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

/// 画布的**初始**字号：设置页「终端」里设了就按它，否则跟随主题的等宽字号。
///
/// 只在建页面时读一次。之后字号归工具条 / 快捷键管（用户当场调了就该立刻生效），
/// 设置页那一项是「下次进来从哪儿起」的默认值，不是实时绑定。
fn initial_font_size(cx: &App) -> f32 {
    let theme_size = active_theme(cx).tokens().typography.mono_md.size.to_f64() as f32;
    super::module_settings::terminal_settings(cx)
        .font_size
        .map_or(theme_size, |size| size as f32)
        .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end())
}
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

/// 弹窗口令按保存口径的「应存明文」。
///
/// 只有「密码认证 + 勾记住 + 填了值」才算一条要保存的口令；其余情形都是
/// `None`（清除）。判据单放一份：[`apply_password`] 落库、[`TerminalPage::apply_edit`]
/// 判「口令变没变」都对着它，两处永远不会各改各的。
fn draft_password_value(draft: &NewConnection) -> Option<String> {
    (draft.auth == AuthKind::Password && draft.remember && !draft.password.is_empty())
        .then(|| draft.password.clone())
}

/// 档案里该留存的私钥路径。
///
/// 认证方式不是「公钥」时一律存空串。留着旧路径有两个后果：
/// ① 用户切到「密码 / SSH Agent」后，切换本身**看不出区别**（档案里仍挂着私钥），
/// 而 `duck-ssh` 侧的行为差别极大（见 [`TerminalPage::ssh_target`] 的注释）；
/// ② 用户切回「公钥」时会看到一个自己不记得填过的路径，误以为已经配好了。
///
/// 落库与比较**共用这一份**：`apply_edit` 的「参数变没变」也拿它算，
/// 否则清空私钥这一下不会触发重连。
fn stored_key_file(auth: AuthKind, key_file: String) -> String {
    if auth == AuthKind::Key {
        key_file
    } else {
        String::new()
    }
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

/// 下载落点：`scp` 会把远端文件写到「本地目录 / 远端文件名」。
///
/// 远端路径里最后一个 `/` 之后就是文件名（远端一律 POSIX 分隔符）。拿不到名字时
/// 退回整个远端路径 —— 那样量到的多半是 `None`（进度降级），但**不会误报**。
fn local_destination(local_dir: &Path, remote_file: &str) -> PathBuf {
    let name = remote_file.rsplit('/').next().unwrap_or(remote_file);
    local_dir.join(name)
}

/// 克隆连接的**显示名**：`原名 副本`，重名再追加 ` 2`、` 3`……
///
/// 按显示名而不是 id 去重：用户认的是列表里那行字，只按 id 去重会让列表里出现两条
/// 同名连接，分不清哪条是哪条。`taken` 由调用方问「这个名字占没占」，好单测。
fn unique_clone_label(base: &str, mark: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut label = format!("{base} {mark}");
    let mut suffix = 2;
    while taken(&label) {
        label = format!("{base} {mark} {suffix}");
        suffix += 1;
    }
    label
}

/// 一次传输的**总字节数**（进度条的分母）。量不到就是 `None`。
fn transfer_total_bytes(request: &TransferRequest, handle: &RemoteHandle) -> Option<u64> {
    if request.upload {
        // 上传：源在本地，直接递归数文件大小。
        duck_ssh::local_size(&request.local_file)
    } else {
        // 下载：源在远端，起一次 ssh 问 `wc -c` / `du -sk`。
        duck_ssh::path_size(
            &handle.target,
            Some(&handle.socket_dir),
            &request.remote_file,
        )
        .ok()
    }
}

/// 已传输的**字节数**：量**落点**（`scp` 正往里写，它会单调增长）。
fn transfer_done_bytes(request: &TransferRequest, handle: &RemoteHandle) -> Option<u64> {
    if request.upload {
        duck_ssh::path_size(
            &handle.target,
            Some(&handle.socket_dir),
            &request.remote_file,
        )
        .ok()
    } else {
        duck_ssh::local_size(&local_destination(&request.local_dir, &request.remote_file))
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

    /// 某一侧目录面板的只读引用。
    fn pane(&self, side: Side) -> &PaneState {
        &self.panes[side.index()]
    }

    /// 激活某个标签。
    ///
    /// 切走时会**撤销别的会话的自动重连计时**：用户已经看向另一个标签，后台不该还在
    /// 按退避起 `ssh`。保留的只有新活动标签自己的会话（含它的分屏窗格）。
    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active != index {
            self.active = index;
            let keep: Vec<String> = self
                .tabs
                .get(index)
                .map(|tab| {
                    std::iter::once(tab.id.clone())
                        .chain(tab.split.clone())
                        .collect()
                })
                .unwrap_or_default();
            self.retain_reconnect_only(&keep);
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
    /// `pub(crate)`：画布里的 `⌘D`（`terminal_canvas::SplitPane`）与命令面板都走这里，
    /// 与工具条那个图标共用一份状态。
    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
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
        // 两处都算「会话归属变更」：被拖会话换到了分屏键、活动标签也变了。
        // 按护栏撤销它们各自的自动重连计时，后台不再替用户起 `ssh`。
        self.cancel_reconnect(&split);
        self.active = t_index;
        let keep: Vec<String> = vec![self.tabs[t_index].id.clone(), split.clone()];
        self.retain_reconnect_only(&keep);
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

    /// 连接拖拽松手（`on_drop`）：落进悬停的分组（拖回原组 = no-op）。
    ///
    /// 分组只是档案上的一个字段（`StoredProfile.group`），落组即去抖落盘，
    /// 不弹确认 —— 拖错了再拖回来就是，没有破坏性。
    fn finish_conn_drop(&mut self, drag: &ConnDrag, cx: &mut Context<Self>) {
        self.dragging_conn = None;
        let Some(target) = self.conn_drop_hover.take() else {
            return;
        };
        if data::move_profile_to_group(&mut self.profiles, &drag.profile_id, &target) {
            // 落点分组一并展开：拖进一个折叠着的分组时，行会当场「消失」
            //（它确实在组里，只是组折着），看起来像连接被删了。
            self.expanded.insert(target);
            data::save_profiles(cx, &self.profiles);
            cx.notify();
        }
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
    /// `pub(crate)`：画布层的 `Clear` 动作（`⌘K`）另有更精确的落点 ——
    /// 它就在某个窗格里派发，直接清那一个（见 `pages::terminal_canvas`）。
    pub(crate) fn clear_active(&mut self, cx: &mut Context<Self>) {
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
    fn toggle_group(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
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

    /// 会话键 → 该会话所连的 `(主机, 端口)`；本地 / SFTP 会话返回 `None`。
    ///
    /// 归属口径与 [`Self::ensure_sessions`] 起会话时**必须一致**：主窗格的会话键就是
    /// 标签 id；分屏右窗格的键形如 `<id>#2`（[`data::split_session_key`]），连哪台机器
    /// 由 `split_owner` 决定。用户档案即真源 —— 内置本地 shell 没有档案，天然落空。
    ///
    /// 给「主机密钥告警」用（见 `terminal_canvas::poll_sessions`）：ssh 因主机密钥
    /// 退出时拿它去查 `known_hosts`，判断这台主机此前有没有记录。
    pub(super) fn ssh_endpoint(&self, session_id: &str) -> Option<(String, u16)> {
        // 主窗格：会话键就是标签 id。
        if let Some(tab) = self.tabs.iter().find(|tab| tab.id == session_id) {
            return self.endpoint_of(tab.kind, &tab.id);
        }
        // 分屏右窗格：会话键 == `tab.split`，连哪台机器看 `split_owner`。
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.split.as_deref() == Some(session_id))?;
        let kind = tab.split_kind().unwrap_or(tab.kind);
        let owner = tab.split_owner.clone().unwrap_or_else(|| tab.id.clone());
        self.endpoint_of(kind, &owner)
    }

    /// 连接归属 + 类型 → `(主机, 端口)`；本地会话（没有档案）返回 `None`。
    fn endpoint_of(&self, kind: TabKind, owner: &str) -> Option<(String, u16)> {
        if kind != TabKind::Ssh {
            return None;
        }
        let profile = self.profile(owner)?;
        let port = profile.port.trim().parse().unwrap_or(22);
        Some((profile.host.clone(), port))
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
        let auth = AuthKind::from_value(&profile.auth);
        // ⚠️ 私钥**必须**跟着认证方式走，不能只看路径非空。
        //
        // `duck-ssh` 的 `common_argv` 是「有私钥就 `-i <key>` + `IdentitiesOnly=yes`」
        // （`target.rs:245-249`）。`IdentitiesOnly=yes` 会**同时**关掉两条路：
        // ssh-agent 里已加载的钥匙不再被递上去，而下方 `else if` 的
        // 「口令认证收窄」（`PubkeyAuthentication=no` +
        // `PreferredAuthentications=password,keyboard-interactive`）因为走的是
        // 互斥分支而**永不执行**。于是用户明明选了「密码」或「SSH Agent」，
        // 却只剩档案里那把残留的旧钥匙在试 —— 服务端只会回一句
        // `Permission denied (publickey,password)`，而口令提示根本不会出现。
        let identity = (auth == AuthKind::Key)
            .then(|| profile.key_file.trim())
            .filter(|path| !path.is_empty())
            .map(PathBuf::from);
        let password = (auth == AuthKind::Password)
            .then(|| open_password(profile))
            .flatten();
        // 端口转发随档案持久化：认不出方向的条目在还原时就被丢掉
        //（见 `data::forward_from_stored`），这里不再二次过滤。
        let forwards = profile
            .forwards
            .iter()
            .filter_map(data::forward_from_stored)
            .collect();
        Some((
            duck_ssh::Target {
                host: profile.host.clone(),
                port: profile.port.trim().parse().unwrap_or(22),
                user: profile.user.clone(),
                identity,
                password,
                // ⚠️ 认证方式必须显式传下去 —— `duck-ssh` 的收窄只看它。
                // 只靠 `identity` / `password` 有没有值来推导，会在
                // 「选了密码但没勾记住口令」「选了公钥但还没填路径」这两种情况下
                // 退化成完全不收窄，ssh 把 agent 里的钥匙试光 →
                // `Permission denied (publickey,password)`，且等不到口令提示。
                auth: match auth {
                    AuthKind::Key => duck_ssh::AuthMethod::Key,
                    AuthKind::Agent => duck_ssh::AuthMethod::Agent,
                    AuthKind::Password => duck_ssh::AuthMethod::Password,
                },
                options: Vec::new(),
                forwards,
                // agent 转发随档案走：它管的是「远端能不能用本机 agent」，
                // 与上面那个认证收窄**正交**，所以不走 `auth` 分支推导。
                agent_forwarding: profile.agent_forwarding,
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

    /// 给「当前该连的那台主机」打开 SFTP 面板（命令面板动作的落点）。
    ///
    /// 目标由 [`Self::sftp_owner`] 解析（当前标签 → 上次开过的连接，纯函数
    /// `resolve_sftp_owner`）；**不**回落成「只看当前标签」——那正是「面板关掉后
    /// 再也打不开」的老 bug。
    ///
    /// 工具条上那条路径是同一个函数：图标在目标为空时置灰，这里则明说原因
    ///（从命令面板触发时没有「灰按钮」可以表达这件事）。
    pub(crate) fn open_sftp_for_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.sftp_owner() else {
            notify::warning(tr(cx, "打开 SFTP 面板（需先连接远程主机）"), window, cx);
            return;
        };
        self.open_sftp_tab(&owner, window, cx);
    }

    /// 新开一条本地终端标签。
    ///
    /// 第一个标签用**固定 id**（[`data::LOCAL_SHELL_ID`]），因为左栏的内置入口
    /// 点开的就是它 —— 两边同一个 id，左栏的选中态才跟得上。
    ///
    /// `pub(crate)`：画布里的 `⌘T`（`terminal_canvas::NewTab`）与命令面板都走这里，
    /// 与标签条上那个 `+` 共用同一份状态机。
    pub(crate) fn new_local_tab(&mut self, cx: &mut Context<Self>) {
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
    /// 内置三档是产品骨架、永远在列；档案里出现过的自定义分组跟在后面
    /// （`id` = 名字 = 用户原文）。跟着连接档案走：一个自定义分组只要有连接
    /// 落在它名下，编辑/新建别的连接时就能选到它。
    fn group_options(&self, cx: &App) -> Vec<(String, String)> {
        let lang = active_theme(cx).lang();
        let mut options: Vec<(String, String)> = data::GROUP_DEFS
            .iter()
            .map(|(id, label, _)| ((*id).to_owned(), t(lang, label).to_owned()))
            .collect();
        options.extend(
            data::custom_group_names(&self.profiles)
                .into_iter()
                .map(|name| (name.clone(), name)),
        );
        options
    }

    /// 打开「新建连接」弹窗。
    ///
    /// `pub(crate)`：命令面板的「新建连接」动作（`actions::ids::TERMINAL_NEW_CONNECTION`）
    /// 与左栏那个按钮走同一条路 —— 弹窗、校验、落库只有一份。
    pub(crate) fn open_new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            group: profile.group.clone(),
            remember: profile.remember,
            // 编辑时不预勾「立即打开」：那会让人以为「保存 = 重连」，
            // 而重连与否由下方独立的「重新连接」决定。
            open_now: false,
            // 端口转发按档案回填；认不出方向的旧条目直接不出现在弹窗里
            //（它本来也不会生效 —— 连接时同样会被丢掉）。
            forwards: profile
                .forwards
                .iter()
                .filter_map(ForwardDraft::from_stored)
                .collect(),
            agent_forwarding: profile.agent_forwarding,
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
        // 端口转发也是连接参数：转发是在 `ssh` 起进程时加的，
        // 改了转发不重连，旧会话上新增的隧道一条都不会生效。
        let forwards: Vec<data::StoredForward> =
            draft.forwards.iter().map(ForwardDraft::to_stored).collect();
        let forwards_changed = profile.forwards != forwards;
        // agent 转发同样是**起进程时**加的参数：翻了不重连，远端那条链路不会变
        //（旧会话仍是 `ForwardAgent` 默认关）—— 与 `forwards_changed` 同一个道理。
        let agent_forwarding_changed = profile.agent_forwarding != draft.agent_forwarding;
        // 私钥先按认证方式规整（非公钥 → 空串），比较与落库都用这一份。
        let key_file = stored_key_file(draft.auth, draft.key_file.clone());
        let reconnect_needed = profile.host != draft.host
            || profile.port != draft.port
            || profile.user != draft.user
            || profile.key_file != key_file
            || profile.auth != draft.auth.value()
            || password_changed
            || forwards_changed
            || agent_forwarding_changed;

        // 口令先落档案（密文；它只读 `draft`，下面的档案字段会被 move 走）。
        apply_password(profile, &draft);
        profile.label = draft.label.clone();
        profile.host = draft.host;
        profile.port = draft.port;
        profile.user = draft.user;
        profile.auth = draft.auth.value().to_owned();
        profile.key_file = key_file;
        profile.group = draft.group;
        profile.remember = draft.remember;
        profile.forwards = forwards;
        profile.agent_forwarding = draft.agent_forwarding;
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

    /// 克隆一条连接档案（右键菜单「克隆连接」）。
    ///
    /// 复制的是**已保存的配置**：主机、端口、用户、认证方式、端口转发全都照搬，只换
    /// id 与显示名（原名 +「副本」，重名再追加序号）。口令在档案里存的是**密文**，直接
    /// 搬密文 —— 不走「解密再加密」，少一条明文路径。
    ///
    /// 克隆出来的连接**不自动连**：派生一份配置通常是为了改参数（换端口、换用户），
    /// 连上反而碍事；要连它，列表里点一下就行。
    fn clone_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.profile(id).cloned() else {
            return;
        };
        // 按**显示名**去重（`副本`、`副本 2`……），规则见 [`unique_clone_label`]。
        let label = unique_clone_label(&source.label, tr(cx, "副本"), |candidate| {
            self.profiles
                .iter()
                .any(|profile| profile.label == candidate)
        });
        // 展开的分组直接取档案上的 `group` 原文：自定义分组名同样是合法落点，
        // 不再需要「只在静态骨架里找得到才展开」那层限制（旧写法会把克隆出来的
        // 自定义分组条目折叠着藏起来，看着像没克隆成功）。
        let group = source.group.clone();
        let new_id = self.unique_connection_id(&label);
        self.profiles.push(StoredProfile {
            id: new_id,
            label: label.clone(),
            ..source
        });
        self.persist_profiles(cx);
        // 与新建同口径：分组缺省折叠，克隆出来的条目落进去看不见就等于「点了没反应」。
        self.expanded.insert(group);
        notify::success(trf(cx, "已克隆连接「{}」。", &[label.as_str()]), window, cx);
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
            key_file: stored_key_file(draft.auth, draft.key_file),
            group: draft.group.clone(),
            remember: draft.remember,
            password,
            forwards: draft.forwards.iter().map(ForwardDraft::to_stored).collect(),
            agent_forwarding: draft.agent_forwarding,
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
            PendingAction::RemoveRemote { path, name } => {
                let success = SharedString::from(trf(cx, "已删除 {}。", &[name.as_str()]));
                self.run_remote_mutation(RemoteMutation::Remove { path }, success, cx);
            }
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
        // 空路径（刚打开、还没连上）从远端主目录起步。`~` 由**远端脚本自己**展开
        //（`duck_ssh::list_dir` 的前导 `d=$1; case $d in '~') d=$HOME`）：我们不去猜
        // 一个绝对路径，`root` 与 `deploy` 的家目录不一样。
        // 列举回来时远端会把 `pwd -P` 的结果一并发回，面板随即换成绝对路径
        //（见 `apply_remote_listing`），所以 `~` 只在这第一次列举里出现。
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
        self.panes[remote].selected = None;
        // 成功时把面板的当前路径换成远端报回的**绝对**目录（请求路径多半是 `~`）：
        // 面包屑、`join`、`mkdir`、`rename`、`scp` 之后全走绝对路径，不必每个远端
        // 操作各自再展开一次波浪号 —— 漏掉任何一处就是一条「远端命令失败」。
        // 远端没报（旧脚本 / 第一行被占）时保留请求路径，条目照常显示。
        // 失败时也保留请求路径：下一次列举还在同一处，用户能看着它改。
        self.panes[remote].path = match &result {
            Ok(listing) => listing.cwd.clone().unwrap_or(path),
            Err(_) => path,
        };
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
        self.begin_transfer(request, name.clone(), upload, handle, cx);
        notify::success(trf(cx, "已开始传输 {}。", &[name.as_str()]), window, cx);
    }

    /// 入队一条传输并立刻开跑（队列行 + 后台 `scp`）。
    ///
    /// 「选中项」与「从访达拖入」两条入口共用它：入队的表格形态、状态流转、
    /// 完成后的对侧刷新都只有一份实现。
    fn begin_transfer(
        &mut self,
        request: TransferRequest,
        name: String,
        upload: bool,
        handle: RemoteHandle,
        cx: &mut Context<Self>,
    ) {
        self.transfers.push(Transfer {
            id: request.id,
            name,
            progress: 0.,
            total: None,
            done: 0,
            state: data::TransferState::Queued,
            upload,
            reason: None,
        });
        cx.notify();
        self.spawn_transfer(request, handle, cx);
    }

    /// 从访达拖进来的文件 → 上传到远端当前目录。
    ///
    /// 一次可落多个文件，每个各占队列一行；目录也收（上传路径上的 `scp -r` 会递归）。
    /// 判断落在**哪一个窗格**不靠指针坐标 —— 落点处理器挂在远端面板本身上，
    /// 只有拖到远端面板才触发（拖到本地面板什么都不发生）。
    fn upload_dropped_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let Some(handle) = self.sftp_target() else {
            notify::warning(tr(cx, "远端连接尚未建立，无法传输。"), window, cx);
            return;
        };
        let remote_dir = self.pane(Side::Remote).path.clone();
        if remote_dir.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }

        let mut started = 0usize;
        for path in paths {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let id = self.next_transfer_id;
            self.next_transfer_id += 1;
            let request = TransferRequest {
                id,
                upload: true,
                local_dir: path
                    .parent()
                    .map_or_else(|| PathBuf::from("."), PathBuf::from),
                local_file: path,
                remote_dir: remote_dir.clone(),
                remote_file: format!("{}/{}", remote_dir.trim_end_matches('/'), name),
            };
            self.begin_transfer(request, name, true, handle.clone(), cx);
            started += 1;
        }
        if started > 0 {
            notify::success(
                trf(cx, "已开始传输 {} 个条目。", &[&started.to_string()]),
                window,
                cx,
            );
        }
    }

    /// 起一次传输，并在旁边挂一个**字节数轮询**。
    ///
    /// 与旧版的关键区别：`scp` 进程由 [`duck_ssh::upload_spawn`] / [`duck_ssh::download_spawn`]
    /// **在本函数（UI 线程）里同步起好**并交出一个 [`duck_ssh::TransferHandle`] ——
    /// `Command::spawn` 不阻塞（阻塞的只有「等结果」），所以句柄能立刻登记进
    /// [`Self::transfer_handles`]，界面上任何「传输中」的行都**马上可取消**。
    /// 「等结果」被挪到后台执行器的一条线程上（`TransferHandle::wait` 轮询式等待，
    /// 取消随时插得进来）。三件事各自待在合适的线程上：
    ///
    /// 1. 传输本体（阻塞等待）跑在后台执行器的一条线程上；
    /// 2. 轮询也跑在后台执行器上（远端 `wc -c` 要走 ssh，绝不能落在 UI 线程）；
    /// 3. 数值回填走 `page.update`，回到 UI 线程。
    ///
    /// 分母量不到时（远端没有 `du` / `wc`、路径还不存在）就退化成不确定进度，
    /// **不编数据**：进度条停在 0 并显示「—」，而不是假装有个百分比。
    fn spawn_transfer(
        &mut self,
        request: TransferRequest,
        handle: RemoteHandle,
        cx: &mut Context<Self>,
    ) {
        // 起进程 + 登记句柄（失败在下面统一处理）。先起后记，中间没有 let 借出 `self`。
        let spawned = if request.upload {
            duck_ssh::upload_spawn(
                &handle.target,
                Some(&handle.socket_dir),
                &request.local_file,
                &request.remote_dir,
            )
        } else {
            duck_ssh::download_spawn(
                &handle.target,
                Some(&handle.socket_dir),
                &request.remote_file,
                &request.local_dir,
            )
        };
        let transfer_handle = match spawned {
            Ok(transfer_handle) => transfer_handle,
            Err(error) => {
                // 起不来（本地源没了、没装 scp）就在这一帧直接落成失败，并保留输入快照
                // —— 「重试」按钮因此对这些失败同样有效（用户修好本地文件再点）。
                self.transfer_requests
                    .insert(request.id, (request.clone(), handle));
                self.mark_transfer(
                    request.id,
                    data::TransferState::Failed,
                    Some(ssh_error_text(cx, &error)),
                    cx,
                );
                return;
            }
        };
        self.transfer_handles
            .insert(request.id, transfer_handle.clone());
        self.transfer_requests
            .insert(request.id, (request.clone(), handle.clone()));
        // 重试会复用同一个行 id：把上一轮的读数清干净，否则进度条会从旧位置起跳。
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == request.id) {
            row.progress = 0.;
            row.done = 0;
            row.total = None;
        }
        self.mark_transfer(request.id, data::TransferState::Active, None, cx);

        cx.spawn(async move |page, cx| {
            let executor = cx.background_executor().clone();

            // 分母：上传用本地源（同步，便宜），下载问远端。
            let total = executor
                .spawn({
                    let request = request.clone();
                    let handle = handle.clone();
                    async move { transfer_total_bytes(&request, &handle) }
                })
                .await;
            if total.is_some() {
                let _ = page.update(cx, |page, cx| {
                    page.set_transfer_progress(request.id, total, 0, cx);
                });
            }

            // 传输跑到一半要能知道「结束了没」：用一个 `AtomicBool` 当跨线程信号，
            // 免得再引一条 oneshot 通道（本仓不新增依赖）。
            let finished = Arc::new(AtomicBool::new(false));
            let transfer = executor.spawn({
                let transfer_handle = transfer_handle.clone();
                let finished = Arc::clone(&finished);
                async move {
                    let result = transfer_handle.wait();
                    finished.store(true, Ordering::SeqCst);
                    result
                }
            });

            // 轮询：只在有分母时才有意义（没有分母就没得算）。
            if total.is_some() {
                while !finished.load(Ordering::SeqCst) {
                    executor.timer(PROGRESS_POLL).await;
                    if finished.load(Ordering::SeqCst) {
                        break;
                    }
                    let done = executor
                        .spawn({
                            let request = request.clone();
                            let handle = handle.clone();
                            async move { transfer_done_bytes(&request, &handle) }
                        })
                        .await;
                    if let Some(done) = done {
                        let _ = page.update(cx, |page, cx| {
                            page.set_transfer_progress(request.id, total, done, cx);
                        });
                    }
                }
            }

            let result = transfer.await;
            let _ = page.update(cx, |page, cx| {
                // 是不是**我们主动取消的**：`TransferHandle::wait` 对被杀掉的进程同样
                // 返回 `Err`（非零退出），光看 `result` 分不出「取消」与「真失败」——
                // 判据必须是「用户点过取消」（行状态已进 `Cancelling`）。
                let cancelled = page
                    .transfers
                    .iter()
                    .find(|row| row.id == request.id)
                    .is_some_and(|row| row.state == data::TransferState::Cancelling);
                page.transfer_handles.remove(&request.id);
                match result {
                    _ if cancelled => {
                        // 取消不是失败：清掉输入快照（不再提供「重试」），标成已取消。
                        page.transfer_requests.remove(&request.id);
                        page.mark_transfer(request.id, data::TransferState::Cancelled, None, cx);
                    }
                    Ok(()) => {
                        page.transfer_requests.remove(&request.id);
                        page.mark_transfer(request.id, data::TransferState::Done, None, cx);
                    }
                    Err(error) => {
                        // 失败：**保留**输入快照，行上给「重试」。
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

    /// 取消一行正在进行的传输：**真的终止** `scp` 进程（`TransferHandle::kill`）。
    ///
    /// 状态先进 `Cancelling` 再 `kill`：`kill` 只是发信号，进程要几十毫秒才退出，
    /// 而「谁取消的」这一事实得先落到行上 —— 后台等待任务回来时靠它把结果判成
    /// `Cancelled` 而不是 `Failed`（见 [`Self::spawn_transfer`] 的收尾）。
    fn cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        let cancellable = self
            .transfers
            .iter()
            .any(|row| row.id == id && row.state == data::TransferState::Active);
        if !cancellable {
            return;
        }
        self.mark_transfer(id, data::TransferState::Cancelling, None, cx);
        if let Some(handle) = self.transfer_handles.get(&id) {
            // `kill` 幂等：进程若刚好已退出，这是一次无害的空操作。
            handle.kill();
        }
        cx.notify();
    }

    /// 重试一行**失败**的传输：拿冻结的输入快照再跑一遍（新起一条 `scp`）。
    ///
    /// 只对失败行有效 —— 已完成 / 已取消的行没有「再来一次」的语义。
    /// 状态按迁移表走 `Failed → Queued → Active`：先回队列（清掉上一轮读数），
    /// 再交给 [`Self::spawn_transfer`] 起进程。
    fn retry_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        let failed = self
            .transfers
            .iter()
            .any(|row| row.id == id && row.state == data::TransferState::Failed);
        if !failed {
            return;
        }
        let Some((request, handle)) = self.transfer_requests.get(&id).cloned() else {
            return;
        };
        self.mark_transfer(id, data::TransferState::Queued, None, cx);
        self.spawn_transfer(request, handle, cx);
    }

    /// 更新一行传输的状态（**状态机唯一入口**）。
    ///
    /// 迁移合法性由 [`data::TransferState::can_transition_to`] 判定：非法迁移记一条
    /// `warn` 后**照常应用** —— 这是刻意的「软校验」。宁可状态照常前进，也不让界面
    /// 卡在一个不动的格子上（例如某条竞态路径走到未列出的边时，取消按钮会永远转圈）。
    /// 迁移表本身是纯函数，已被单测逐条钉住。
    fn mark_transfer(
        &mut self,
        id: u64,
        state: data::TransferState,
        reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            if !row.state.can_transition_to(state) {
                tracing::warn!(
                    id,
                    from = ?row.state,
                    to = ?state,
                    "传输状态出现未登记的迁移，仍按新状态应用"
                );
            }
            row.state = state;
            row.reason = reason;
            if state == data::TransferState::Done {
                row.progress = 1.;
                // 收尾时把 `done` 对齐到 `total`：某些落点（目录 `du -sk` 的块粒度）
                // 量出来会略微小于真实大小，不对齐就会显示「99%」卡着不走。
                row.done = row.total.unwrap_or(row.done);
            }
        }
        cx.notify();
    }

    /// 轮询到**真实字节数**后回填进度。
    ///
    /// 只更新数值，不动状态机：状态流转仍由 `mark_transfer` 一家负责，
    /// 否则「轮询还在跑、传输已经失败」时会互相打架。
    fn set_transfer_progress(
        &mut self,
        id: u64,
        total: Option<u64>,
        done: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            // 行已进入终态（完成 / 取消 / 失败）：迟到的进度轮询不再改动读数 ——
            // `mark_transfer` 已经把定稿（例如 `Done` 把 progress 钉在 1.0），
            // 让一次晚到的 `wc -c` 把它拉回去就会看到「100% 又掉头」。
            if row.state.is_terminal() {
                return;
            }
            if total.is_some() {
                row.total = total;
            }
            row.done = done;
            row.progress = match row.total {
                Some(total) if total > 0 => (done as f32 / total as f32).clamp(0., 1.),
                // 分母未知 / 为 0：留在 0，界面按「不确定」渲染。
                _ => 0.,
            };
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 缓冲区搜索（⌘F）。
    //
    // 命中由内核算（`Session::search_matches`，走 alacritty 的 DFA），界面只做三件事：
    // 记住「搜的是哪一块画布」、把网格坐标换算成视口行画高亮、按 Enter 在命中之间走。
    // ------------------------------------------------------------------

    /// 打开（或重新聚焦）搜索条。目标 = 用户最后点过的窗格，没有就退回当前标签。
    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self
            .focus_pane
            .clone()
            .filter(|key| {
                self.sessions
                    .get(key)
                    .and_then(TerminalSession::session)
                    .is_some()
            })
            .or_else(|| {
                self.active_tab().map(|tab| tab.id.clone()).filter(|key| {
                    self.sessions
                        .get(key)
                        .and_then(TerminalSession::session)
                        .is_some()
                })
            });
        // 没有可搜的画布（空态、SFTP 标签、PTY 起不来）：什么都不做。
        let Some(session) = session else {
            return;
        };

        if let Some(find) = self.find.as_mut() {
            // 已经开着：换目标、重新聚焦，**保留**已敲的搜索词（切窗格不该清空输入）。
            find.session = session;
            let handle = find.input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            self.refresh_find(cx);
            return;
        }

        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "在缓冲区中查找…")));
        let subscriptions = vec![cx.subscribe(
            &input,
            |this: &mut Self, _input: Entity<InputState>, event: &InputEvent, cx| match event {
                InputEvent::Change => this.refresh_find(cx),
                // Enter / Shift-Enter 在命中之间前后走。
                InputEvent::PressEnter { shift, .. } => {
                    this.step_find(if *shift { -1 } else { 1 }, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        )];
        let handle = input.read(cx).focus_handle(cx);
        self.find = Some(FindState {
            input,
            session,
            matches: Vec::new(),
            current: 0,
            refreshed: Instant::now(),
            _subscriptions: subscriptions,
        });
        window.focus(&handle, cx);
        cx.notify();
    }

    /// 收起搜索条（并把键盘交还画布 —— 否则焦点会留在一个已经消失的输入框上）。
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = self.find.take() else {
            return;
        };
        let handle = self
            .sessions
            .get(&find.session)
            .map(TerminalSession::focus_handle);
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// 用当前搜索词重算命中。
    ///
    /// 「当前命中」下标在结果变短时钳到最后一条：删搜索词的过程中命中数会掉，
    /// 不钳就会索引越界（表现为高亮消失或 panic）。
    fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let key = find.session.clone();
        let query = find.input.read(cx).value().to_string();
        let matches: Vec<(i32, usize, i32, usize)> = self
            .sessions
            .get(&key)
            .and_then(TerminalSession::session)
            .map(|session| {
                session
                    .search_matches(&query)
                    .into_iter()
                    .map(|(start, end)| (start.line.0, start.column.0, end.line.0, end.column.0))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(find) = self.find.as_mut() {
            find.matches = matches;
            find.current = find.current.min(find.matches.len().saturating_sub(1));
            find.refreshed = Instant::now();
        }
        cx.notify();
    }

    /// 搜索命中 → 可见行上的高亮条带：`(视口行, 首列, 末列, 是否当前命中)`。
    ///
    /// 与选区条带（`terminal_canvas::selection_bands`）同一套「网格行 → 视口行」换算，
    /// 差别只在来源：那边来自内核的 `SelectionRange`，这边来自搜索命中的网格区间。
    /// 多出来的 `bool` 是给渲染层的：当前命中要画得比其余命中更醒目，否则
    /// 「按 Enter 走了一圈」在画面上完全看不出来。
    pub(super) fn find_highlight(
        &self,
        tab_id: &str,
        offset: i32,
        screen_lines: usize,
        columns: usize,
    ) -> Vec<(usize, usize, usize, bool)> {
        let Some(find) = self.find.as_ref().filter(|find| find.session == tab_id) else {
            return Vec::new();
        };
        if columns == 0 || screen_lines == 0 {
            return Vec::new();
        }
        let last_column = columns - 1;
        let mut bands = Vec::with_capacity(find.matches.len());
        for (index, &(start_line, start_column, end_line, end_column)) in
            find.matches.iter().enumerate()
        {
            let current = index == find.current;
            // 与视口求交：往上翻着历史搜、或命中跨了很多行时，两端都会出界。
            let top = (start_line + offset).max(0);
            let bottom = (end_line + offset).min(screen_lines as i32 - 1);
            if bottom < top {
                continue;
            }
            for row in top..=bottom {
                // 跨行命中的中间几行整行铺满，首行从命中起点铺到行尾、末行从行首铺到终点。
                let first = if row <= start_line + offset {
                    start_column
                } else {
                    0
                };
                let last = if row >= end_line + offset {
                    end_column
                } else {
                    last_column
                };
                let first = first.min(last);
                let last = last.min(last_column);
                if first > last {
                    continue;
                }
                bands.push((row as usize, first, last, current));
            }
        }
        bands
    }

    /// 在命中之间前后走一格（环形），并把视口滚到那一处。
    fn step_find(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as i32;
        find.current = (find.current as i32 + delta).rem_euclid(len) as usize;
        let line = find.matches[find.current].0;
        let key = find.session.clone();
        let Some(session) = self
            .sessions
            .get(&key)
            .and_then(TerminalSession::session)
            .cloned()
        else {
            return;
        };
        // 把命中的那一行挪到**视口正中**：先算「需要的 display_offset」再求差值。
        // 视口行 = 网格行 + display_offset（见 `terminal_canvas`），要让它在正中，
        // 就得 offset = 半屏高 - 网格行。
        let current = session
            .try_with_term(|term| term.renderable_content().display_offset as i32)
            .unwrap_or_default();
        let rows = i32::from(session.grid_size().1);
        let desired = (rows / 2) - line;
        if desired != current {
            session.scroll(desired - current);
        }
        cx.notify();
    }

    /// 终端有输出时（按 [`FIND_REFRESH_MIN`] 节流）重算命中。
    ///
    /// 输出会不断把旧命中整体挪位（回滚区行号是相对的），不定期重算高亮就会框错字。
    pub(super) fn maybe_refresh_find(&mut self, cx: &mut Context<Self>) {
        let due = self
            .find
            .as_ref()
            .is_some_and(|find| find.refreshed.elapsed() >= FIND_REFRESH_MIN);
        if due {
            self.refresh_find(cx);
        }
    }

    // ------------------------------------------------------------------
    // 远端文件系统改动：新建目录 / 删除 / 重命名。
    //
    // 三者都是**先把意图冻进 `RemoteMutation`，再丢到后台执行器上跑一个 ssh 进程**，
    // 回主线程后重列举远端面板并推一条通知。删除与重命名是破坏性动作，走确认框
    // （删除）或命名弹窗（重命名，见 `rename_remote_entry`）。
    // ------------------------------------------------------------------

    /// 远端条目在当前目录下的完整路径。远端目录还没列举出来时是 `None` ——
    /// 拿空目录拼出来的路径 `mkdir /xxx` 会落在根上，比拒绝更糟。
    fn remote_entry_path(&self, name: &str) -> Option<String> {
        let dir = self.pane(Side::Remote).path.clone();
        if dir.trim().is_empty() {
            return None;
        }
        Some(data::join(&dir, name))
    }

    /// 打开「新建远端目录」弹窗。
    fn new_remote_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane(Side::Remote).path.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }
        let view = self.view.clone();
        open_entry_name_dialog(
            window,
            cx,
            NameDialogKind::NewFolder,
            String::new(),
            move |name, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    let Some(path) = this.remote_entry_path(&name) else {
                        return;
                    };
                    let success = SharedString::from(trf(cx, "已创建目录 {}。", &[name.as_str()]));
                    this.run_remote_mutation(RemoteMutation::Mkdir { path }, success, cx);
                });
            },
        );
    }

    /// 打开「重命名远端条目」弹窗。
    ///
    /// 命名弹窗**本身就是这次改动的确认**：它同时显示旧名（标题/说明）与新名，
    /// 用户敲下确认才动手 —— 比「先弹一句『确定重命名吗』再弹一个输入框」少一跳。
    fn rename_remote_entry(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.remote_entry_path(&name) else {
            notify::warning(tr(cx, "远端连接尚未建立，无法操作。"), window, cx);
            return;
        };
        let dir = self.pane(Side::Remote).path.clone();
        let view = self.view.clone();
        open_entry_name_dialog(
            window,
            cx,
            NameDialogKind::Rename,
            name.clone(),
            move |new_name, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    if new_name == name {
                        return;
                    }
                    let to = data::join(&dir, &new_name);
                    let success =
                        SharedString::from(trf(cx, "已重命名为 {}", &[new_name.as_str()]));
                    this.run_remote_mutation(
                        RemoteMutation::Rename {
                            from: from.clone(),
                            to,
                        },
                        success,
                        cx,
                    );
                });
            },
        );
    }

    /// 删除远端条目（**唯一入口**）：先弹确认框，确认后才真的 `rm`。
    fn delete_remote_entry(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.remote_entry_path(&name) else {
            notify::warning(tr(cx, "远端连接尚未建立，无法操作。"), window, cx);
            return;
        };
        let action = PendingAction::RemoveRemote { path, name };
        self.confirm_entry_action(action, window, cx);
    }

    /// 远端破坏性动作的确认框（删除）。
    ///
    /// 与「删除连接」同一个形态：参数冻结进 [`PendingAction`]，确认期间不回读列表 ——
    /// 否则用户删完一条又点了确认，动到的不是他看到的那个。
    fn confirm_entry_action(
        &mut self,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let title = trf(cx, "删除「{}」？", &[action.label().as_str()]);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
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
                                tr(cx, "该操作会直接在远端服务器上执行，不可撤销。"),
                                cx,
                            )),
                    ))
                })
                .on_ok(move |_, _window, cx| {
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), cx);
                    });
                    true
                })
        });
    }

    /// 在后台执行器上跑一次远端改动，回来后重列举远端面板并推通知。
    fn run_remote_mutation(
        &mut self,
        mutation: RemoteMutation,
        success: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = self.sftp_target() else {
            self.panes[Side::Remote.index()].notice =
                Some(data::Notice::Key(data::REMOTE_OFFLINE_NOTICE));
            cx.notify();
            return;
        };
        cx.spawn(async move |page, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let handle = handle.clone();
                    let mutation = mutation.clone();
                    async move {
                        match mutation {
                            RemoteMutation::Mkdir { path } => {
                                duck_ssh::mkdir(&handle.target, Some(&handle.socket_dir), &path)
                            }
                            RemoteMutation::Remove { path } => {
                                duck_ssh::remove(&handle.target, Some(&handle.socket_dir), &path)
                            }
                            RemoteMutation::Rename { from, to } => duck_ssh::rename(
                                &handle.target,
                                Some(&handle.socket_dir),
                                &from,
                                &to,
                            ),
                        }
                    }
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                // 成功与失败都重列举：成功要看到新条目，失败可能是「目录已被别人改过」。
                page.reload_pane(Side::Remote, cx);
                match result {
                    Ok(()) => crate::update::on_notify_window(cx, move |window, cx| {
                        notify::success(success.clone(), window, cx);
                    }),
                    Err(error) => {
                        // ssh 的原文指得出下一步（权限？目录非空？路径不对？）。
                        let reason = ssh_error_text(cx, &error);
                        crate::update::on_notify_window(cx, move |window, cx| {
                            notify::error(reason, window, cx);
                        });
                    }
                }
            });
        })
        .detach();
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

    /// 左栏：搜索 + 分组连接列表。行规格与 [`sidebar_navigation_button`] 一致。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 分组名在这里是**已就绪的显示名**（内置组在 `data::group_views` 里翻过），
        // 左栏不再需要 `lang` 自己翻译。
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
        // 连接拖拽进行中（拖拽源登记 + 确有活跃拖拽双保险：松手哪怕落在
        // 投放区之外，下一帧空分组与高亮也会收回去）。
        let conn_dragging = self.dragging_conn.is_some() && cx.has_active_drag();

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
            // 空分组平时不渲染；唯独拖连接时要在 —— 不然把某组最后一条
            // 连接拖走后这个组就消失了，那份连接永远拖不回去。拖拽期间以
            // 「虚线投放位」的弱化形态出现（见下），不会看着像凭空冒出的文件夹。
            if items.is_empty() && !conn_dragging {
                continue;
            }
            if !items.is_empty() {
                matched += items.len();
            }

            // 搜索时临时展开命中的分组（不然「搜到了却看不见」）；平时只看用户点开的。
            // 空分组没有内容可展开，永远按折叠画。
            let expanded = !items.is_empty() && (searching || self.expanded.contains(&group.id));
            let chevron = if expanded {
                DuckIcon::ChevronDown
            } else {
                DuckIcon::ChevronRight
            };
            let group_id = group.id.clone();
            let conn_hovered =
                conn_dragging && self.conn_drop_hover.as_deref() == Some(group_id.as_str());

            // 分组整块是投放区：悬停亮主题专用语义色（`drag_border` 描边、
            // `drop_target` 悬停底色），与画布分屏投放区同一套拖放视觉。
            // 常态描边透明（宽度不变），高亮出现时不抖布局。
            let mut section = v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .id(SharedString::from(format!(
                    "terminal-group-drop-{group_id}"
                )))
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(if conn_hovered {
                    extended.colors.drag_border
                } else {
                    gpui::transparent_black()
                })
                .when(conn_hovered, |section| {
                    section.bg(extended.colors.drop_target)
                })
                .on_drag_move::<ConnDrag>({
                    let view = view.clone();
                    let group_id = group_id.clone();
                    move |event: &DragMoveEvent<ConnDrag>, _window, cx| {
                        // ⚠️ gpui 的 `on_drag_move` 在 Capture 阶段**全局**分发
                        //（div.rs 只对载荷类型、不做命中检测），每个分组都会在
                        // 每次指针移动时收到回调，注册顺序靠后的会覆盖靠前的
                        // ——不做这一道几何自检，高亮与落组会永远钉在最后一组。
                        // 与分屏投放区（`with_split_drop_target`）同一套自救。
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        let _ = view.update(cx, |this, cx| {
                            // `group_id` 是 `String`（自定义分组没有静态 id），
                            // 而处理器是 `Fn`（每次指针移动都重跑）——只能克隆比较。
                            if this.conn_drop_hover.as_deref() != Some(group_id.as_str()) {
                                this.conn_drop_hover = Some(group_id.clone());
                                cx.notify();
                            }
                        });
                    }
                })
                .on_drop::<ConnDrag>({
                    let view = view.clone();
                    move |drag: &ConnDrag, _window, cx| {
                        let _ = view.update(cx, |this, cx| this.finish_conn_drop(drag, cx));
                    }
                });

            // 空分组（只会在拖拽期间出现）画成虚线投放位：弱化的图标 + 名字，
            // 一眼能看出「这是拖拽用的临时槽位」，而不是凭空多出来的文件夹。
            // 投放逻辑（悬停高亮 / `on_drop`）由外层 `section` 统一提供，
            // 占位块自己不用接任何事件。
            if items.is_empty() {
                section = section.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.md)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.md)
                        .border_1()
                        .border_dashed()
                        .border_color(if conn_hovered {
                            extended.colors.drag_border
                        } else {
                            extended.colors.hairline
                        })
                        .text_size(tokens.typography.sm.size)
                        .text_color(extended.colors.text_tertiary)
                        .child(
                            Icon::new(group.icon)
                                .size(extended.icon.md)
                                .text_color(extended.colors.text_tertiary),
                        )
                        .child(group.label.clone()),
                );
            } else {
                section = section.child(
                    sidebar_navigation_button(
                        SharedString::from(format!("terminal-group-{group_id}")),
                        SharedString::from(group.label.clone()),
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
                            let _ =
                                view.update(cx, |this, cx| this.toggle_group(group_id.clone(), cx));
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
                                    active == id
                                        || active.strip_suffix("/sftp") == Some(id.as_str())
                                });
                        let mut row = sidebar_navigation_button(
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
                        });
                        // 拖拽源：只有用户档案能拖（本地 shell 是内置入口，不属于任何
                        // 分组），搜索过滤视图里也不拖 —— 列表是筛出来的，落组语义混乱。
                        // 必须挂在 context_menu 之前：`.context_menu()` 返回
                        // `ContextMenu<Button>`，上面没有拖拽方法。
                        if !searching && !item.builtin {
                            row = row.on_drag(
                                ConnDrag {
                                    profile_id: id.clone(),
                                },
                                {
                                    let view = view.clone();
                                    let label = item.label.clone();
                                    move |drag: &ConnDrag, _point, _window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if this.dragging_conn.as_deref()
                                                != Some(drag.profile_id.as_str())
                                            {
                                                this.dragging_conn = Some(drag.profile_id.clone());
                                                this.conn_drop_hover = None;
                                                // 互清另一类拖拽的残留：上一轮若在空白处松手
                                                //（没有 drop 目标接住），旧值会留到下一次拖拽，
                                                // 让「空分组显示 / 分屏投放区」错档亮起。
                                                this.dragging_tab = None;
                                                this.split_drop_hover = false;
                                                cx.notify();
                                            }
                                        });
                                        cx.new(|_| TabDragGhost {
                                            icon: DuckIcon::Server,
                                            label: label.clone(),
                                        })
                                    }
                                },
                            );
                        }
                        let row = row.context_menu({
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
                                    }))
                                    // 克隆紧挨「编辑」：都是「拿这条连接再派生一份」的动作，
                                    // 区别只是克隆不问参数、直接复制一份可改的副本。
                                    .item(PopupMenuItem::new(tr(cx, "克隆连接")).on_click({
                                        let id = id.clone();
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.clone_connection(&id, window, cx);
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
                        });
                        section = section.child(row);
                    }
                }
            }

            nav = nav.child(section);
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
                                        // 互清连接拖拽的残留，理由同连接拖拽源
                                        //（`has_active_drag()` 只答「有没有拖拽」，
                                        // 两类载荷共用它做渲染闸，残留旧值会串档）。
                                        this.dragging_conn = None;
                                        this.conn_drop_hover = None;
                                        cx.notify();
                                    }
                                });
                                cx.new(|_| TabDragGhost {
                                    icon: DuckIcon::SquareTerminal,
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
                            // `reconnect_target` 下面还要给「克隆连接」用一次，
                            // 这一处先克隆（它只是个短字符串，不值得为省一次克隆改结构）。
                            if let Some(target) = reconnect_target.clone() {
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
                            // 克隆这条标签所属的连接：与连接列表右键菜单是**同一个动作**
                            //（复制一份可改的档案，不自动连）。本地 shell 没有档案，不给。
                            if let Some(owner) = reconnect_target {
                                menu =
                                    menu.item(PopupMenuItem::new(tr(cx, "克隆连接")).on_click({
                                        let view = view.clone();
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.clone_connection(&owner, window, cx);
                                            });
                                        }
                                    }));
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
        let find_bar = self.render_find_bar(cx);
        let canvas = self.render_terminal_canvas(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(toolbar)
            .when_some(find_bar, |this, bar| this.child(bar))
            .child(canvas)
            .into_any_element()
    }

    /// 缓冲区搜索条（`⌘F` 打开）。没打开时返回 `None`。
    ///
    /// 位置在工具条与画布之间：贴着它作用的内容，但不挤占画布的宽度。
    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let empty = find.matches.is_empty();
        // 计数用「第 n / 共 m 项」：只给 m 的话，用户不知道 Enter 走到哪了。
        let counter = if empty {
            tr(cx, "无结果").to_owned()
        } else {
            trf(
                cx,
                "{} / {} 项",
                &[
                    &(find.current + 1).to_string(),
                    &find.matches.len().to_string(),
                ],
            )
        };
        let view_prev = self.view.clone();
        let view_next = self.view.clone();
        let view_close = self.view.clone();

        Some(
            h_flex()
                .id(SharedString::from("terminal-find-bar"))
                .w_full()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .bg(extended.colors.chrome)
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    Icon::new(DuckIcon::Search)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                )
                .child(
                    div().w(px(FIND_INPUT_WIDTH)).flex_none().min_w_0().child(
                        Input::new(&find.input)
                            .appearance(false)
                            .with_size(Size::Small)
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .w_full(),
                    ),
                )
                .child(meta_text(SharedString::from(counter), cx))
                .child(
                    toolbar_action_button(
                        "terminal-find-prev",
                        tr(cx, "上一个"),
                        Icon::new(DuckIcon::ArrowUp).size(extended.icon.md),
                        false,
                        empty,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view_prev.update(cx, |this, cx| this.step_find(-1, cx));
                    }),
                )
                .child(
                    toolbar_action_button(
                        "terminal-find-next",
                        tr(cx, "下一个"),
                        Icon::new(DuckIcon::ArrowDown).size(extended.icon.md),
                        false,
                        empty,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view_next.update(cx, |this, cx| this.step_find(1, cx));
                    }),
                )
                .child(
                    toolbar_action_button(
                        "terminal-find-close",
                        tr(cx, "关闭搜索"),
                        Icon::new(DuckIcon::X).size(extended.icon.md),
                        false,
                        false,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view_close.update(cx, |this, cx| this.close_find(window, cx));
                    }),
                )
                // `Esc` 收起、再按一次 `⌘F` 重新聚焦输入框。
                //
                // 搜索条**刻意不挂** `key_context("Terminal")`：挂了的话这里会变成又一层
                // 终端上下文，输入框里的 Tab / Ctrl-C 之类会绕过 `Input` 的默认处理跑到
                // 终端动作上。所以快捷键只在这一条 `on_key_down` 里手判。
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if key == "escape" {
                        this.close_find(window, cx);
                        cx.stop_propagation();
                    } else if event.keystroke.modifiers.platform && key == "f" {
                        if let Some(find) = this.find.as_ref() {
                            let handle = find.input.read(cx).focus_handle(cx);
                            window.focus(&handle, cx);
                        }
                        cx.stop_propagation();
                    }
                }))
                .into_any_element(),
        )
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
        // 隔离线：与 SFTP 双栏同一条发丝线（见 [`Self::render_pane_divider`]），
        // 只是套了条可拖动的命中带。
        let left = self.render_terminal_pane(&tab_id, kind, false, cx);
        let right = self.render_terminal_pane(&split, split_kind, true, cx);
        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            // 左栏宽度 = 记忆占比（默认各半）；右栏 `flex_1` 吃剩余空间。
            // 拖隔离栏改占比，两块画布各自按新宽度重排（`apply_canvas_bounds`）。
            .child(
                div()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .w(relative(self.pane_ratio()))
                    .flex_none()
                    .child(left),
            )
            .child(self.render_pane_divider(cx))
            .child(div().flex_1().h_full().min_w_0().min_h_0().child(right))
            // 拖动中比例实时跟手：事件挂在**包含隔离栏的整条容器**上，
            // 指针拖出隔离栏命中带也持续生效（gpui 的 drag 语义）。
            .on_drag_move::<PaneDrag>(cx.listener(Self::on_pane_divider_drag))
            // 松手（落点在容器内）即收尾：清「正在拖动」标记。
            .on_drop::<PaneDrag>(cx.listener(|this, _, _window, cx| {
                if this.resizing_pane {
                    this.resizing_pane = false;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// 当前双栏的左右占比（左栏宽 / 总宽）。没拖过 = 各半。
    fn pane_ratio(&self) -> f32 {
        self.divider_ratio.unwrap_or(0.5)
    }

    /// 拖动隔离栏时的比例换算：指针横坐标 → 占比，两端各留 [`MIN_PANE_WIDTH`]。
    fn on_pane_divider_drag(
        &mut self,
        event: &DragMoveEvent<PaneDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = event.bounds.size.width;
        if width <= px(0.) {
            return;
        }
        // 两端最小宽换算成占比边界；容器比两倍最小宽还窄时退回各半。
        let min_fraction = (MIN_PANE_WIDTH / width.as_f32()).min(0.5);
        let raw = ((event.event.position.x - event.bounds.left()).as_f32() / width.as_f32())
            .clamp(min_fraction, 1. - min_fraction);
        let changed = match self.divider_ratio {
            None => raw != 0.5,
            Some(current) => (current - raw).abs() > f32::EPSILON,
        };
        if changed {
            self.divider_ratio = Some(raw);
            cx.notify();
        }
    }

    /// 双栏之间**可拖动**的隔离栏（分屏双终端与 SFTP 双栏共用）。
    ///
    /// 视觉仍是那条 1px 发丝线，但外面套了一条透明命中带（`spacing.sm` 宽）：
    /// 1px 的东西点不中，主流实现（VS Code / 各终端分屏）都给隔离栏一条宽命中带。
    /// 悬停与拖动中亮强调色薄底（`hover_tint`，全仓悬停语言），指针变左右箭头；
    /// 比例记进 [`Self::divider_ratio`]，拖拽用 gpui 的 drag 事件 —— 指针拖出
    /// 命中带也持续跟踪，这是 `on_mouse_move` 做不到的。
    fn render_pane_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        div()
            .id("pane-divider")
            .w(tokens.spacing.sm)
            .h_full()
            .flex_none()
            .flex()
            .items_stretch()
            .justify_center()
            .cursor(CursorStyle::ResizeLeftRight)
            // 悬停亮薄底提示「这里抓得住」；拖动中指针会离开命中带，
            // 悬停态靠不住 —— 用 `resizing_pane` 标记保持同款视觉。
            .when(self.resizing_pane, |this| {
                this.bg(extended.colors.hover_tint)
            })
            .hover(|style| style.bg(extended.colors.hover_tint))
            .on_drag(PaneDrag, |_, _, _, cx| cx.new(|_| PaneDragGhost))
            .child(
                // 中间那条 1px 发丝线：与原实现同宽同色，不引入第二套视觉。
                div()
                    .w(extended.stroke.thin)
                    .h_full()
                    .flex_none()
                    .bg(extended.colors.hairline),
            )
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
        // 只有**标签拖拽**才亮投放区：隔离栏拖拽同样是 active drag（载荷是
        // `PaneDrag`），但那是调宽度的手势，不该把「松开并入分屏」的框亮出来。
        // `dragging_tab` 由标签拖拽源登记、`on_drop` 清除，比 `has_active_drag`
        // （不区分载荷类型）更准。
        self.dragging_tab.as_ref()?;
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
                    // 左栏宽度 = 记忆占比（与分屏双终端共用同一条比例，见
                    // `render_pane_divider`）；右栏 `flex_1` 吃剩余空间。
                    .child(
                        div()
                            .h_full()
                            .min_w_0()
                            .min_h_0()
                            .w(relative(self.pane_ratio()))
                            .flex_none()
                            .child(left_pane),
                    )
                    .child(self.render_pane_divider(cx))
                    .child(right_pane)
                    // 拖动中比例实时跟手；松手清「正在拖动」标记。
                    .on_drag_move::<PaneDrag>(cx.listener(Self::on_pane_divider_drag))
                    .on_drop::<PaneDrag>(cx.listener(|this, _, _window, cx| {
                        if this.resizing_pane {
                            this.resizing_pane = false;
                            cx.notify();
                        }
                    })),
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
            )
            // 远端才有的三档改动动作：新建目录 / 重命名 / 删除。
            //
            // 只给远端：本地面板是「上传的取材区」，改本机文件不是这个页面的职责
            //（要动本机文件用户有访达）；而这三个动作是 `duck_ssh` 的 mkdir / mv / rm，
            // 目标就是远端。放在路径栏而非顶部工具栏，是因为它们作用于**这一侧**。
            .when(!side.is_local(), |this| {
                let has_selection = pane.selected.is_some();
                this.child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-new-dir-{}", side.index())),
                        tr(cx, "新建文件夹"),
                        Icon::new(DuckIcon::FolderPlus).size(extended.icon.md),
                        false,
                        false,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ =
                                view.update(cx, |this, cx| this.new_remote_directory(window, cx));
                        }
                    }),
                )
                .child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-rename-{}", side.index())),
                        tr(cx, "重命名"),
                        Icon::new(DuckIcon::Pencil).size(extended.icon.md),
                        false,
                        !has_selection,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if let Some(name) = this.pane(Side::Remote).selected.clone() {
                                    this.rename_remote_entry(name, window, cx);
                                }
                            });
                        }
                    }),
                )
                .child(
                    toolbar_action_button(
                        SharedString::from(format!("terminal-delete-{}", side.index())),
                        tr(cx, "删除"),
                        Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                        true,
                        !has_selection,
                        cx,
                    )
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                if let Some(name) = this.pane(Side::Remote).selected.clone() {
                                    this.delete_remote_entry(name, window, cx);
                                }
                            });
                        }
                    }),
                )
            });

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
            let row = h_flex()
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
                    // `entry.name` 还要给下面的右键菜单用：按值交给点击闭包之前先克隆一份。
                    let click_name = name.clone();
                    move |event: &ClickEvent, window, cx| {
                        // 双击进入目录：`down.click_count` 是平台给的连击计数（第二次按下为 2）。
                        let double =
                            matches!(event, ClickEvent::Mouse(mouse) if mouse.down.click_count > 1);
                        let name = click_name.clone();
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
                );
            // 右键菜单只挂远端行：本地面板没有对应的远端动作（见路径栏那三档的注释）。
            if side.is_local() {
                row.into_any_element()
            } else {
                row.context_menu({
                    let view = view.clone();
                    let menu_name = name.clone();
                    // 菜单闭包是 `Fn`（每次打开都重跑），每一项各自克隆。
                    move |menu, _, cx| {
                        let menu = menu
                            .item(PopupMenuItem::new(tr(cx, "重命名")).on_click({
                                let name = menu_name.clone();
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.rename_remote_entry(name.clone(), window, cx);
                                    });
                                }
                            }))
                            .item(PopupMenuItem::new(tr(cx, "删除")).on_click({
                                let name = menu_name.clone();
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.delete_remote_entry(name.clone(), window, cx);
                                    });
                                }
                            }));
                        menu.separator()
                            .item(PopupMenuItem::new(tr(cx, "新建文件夹")).on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.new_remote_directory(window, cx);
                                    });
                                }
                            }))
                    }
                })
                .into_any_element()
            }
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

        let pane_root = v_flex()
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
            );

        // 访达拖入只挂在**远端**面板上：拖进来即上传到当前远端目录，拖到本地面板
        // 什么都不发生（本地面板是取材区，没有「传到哪里」的语义）。
        // 平台把文件拖放翻成一次以 `ExternalPaths` 为负载的内部拖拽（见
        // `Window::dispatch_event`），所以 `drag_over` / `on_drop` 就够。
        if side.is_local() {
            pane_root.into_any_element()
        } else {
            let tint = extended.colors.hover_tint;
            let drop_view = view.clone();
            pane_root
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(tint))
                .on_drop(move |paths: &ExternalPaths, window, cx| {
                    let files = paths.paths().to_vec();
                    let _ = drop_view.update(cx, |this, cx| {
                        this.upload_dropped_files(files, window, cx);
                    });
                })
                .into_any_element()
        }
    }

    /// 传输队列：一行一条，含方向图标、进度条与状态标签。
    fn render_transfer_queue(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let running = self
            .transfers
            .iter()
            .filter(|transfer| transfer.state.is_active())
            .count();

        let rows = v_flex()
            .w_full()
            .children(self.transfers.iter().map(|transfer| {
                let active = transfer.state.is_active();
                // 读数来自**真实字节数**（落点大小 / 总大小，见 `set_transfer_progress`）。
                // 总大小量不到时（远端没有 `du` / `wc`）显示「—」而不是编一个百分比。
                let number = match transfer.state {
                    data::TransferState::Done => "100%".to_owned(),
                    data::TransferState::Active if transfer.total.is_some() => {
                        format!("{}%", (transfer.progress * 100.).round() as u32)
                    }
                    data::TransferState::Active | data::TransferState::Cancelling => "—".to_owned(),
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
                    // 取消：只在真正进行中（Active）时给，点了立刻 `kill` 进程；
                    // `Cancelling` 期间不给（换成置灰的说明），避免连点与「点了没反应」。
                    .when(transfer.state == data::TransferState::Active, |this| {
                        let id = transfer.id;
                        let view = self.view.clone();
                        this.child(
                            icon_text_button(
                                SharedString::from(format!("terminal-transfer-cancel-{id}")),
                                tr(cx, "取消"),
                                Icon::new(DuckIcon::Pause)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                let _ = view.update(cx, |this, cx| this.cancel_transfer(id, cx));
                            }),
                        )
                    })
                    // 重试：只在失败后给 —— 拿冻结的输入快照再跑一遍。
                    .when(transfer.state == data::TransferState::Failed, |this| {
                        let id = transfer.id;
                        let view = self.view.clone();
                        this.child(
                            icon_text_button(
                                SharedString::from(format!("terminal-transfer-retry-{id}")),
                                tr(cx, "重试"),
                                Icon::new(DuckIcon::RotateCw)
                                    .size(extended.icon.md)
                                    .text_color(tokens.colors.muted_foreground),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                let _ = view.update(cx, |this, cx| this.retry_transfer(id, cx));
                            }),
                        )
                    })
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
            // 后者取自起会话用的那份设置（「设置 → 终端」的「回滚行数」）
            // —— 都不是写死展示的数字。
            let (columns, rows) = tab
                .and_then(|tab| self.sessions.get(&tab.id))
                .map_or((0, 0), TerminalSession::grid);
            let scrollback = super::module_settings::terminal_settings(cx).scrollback;
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
                icon_text_button(
                    "terminal-right-open-sftp",
                    tr(cx, "打开 SFTP 面板"),
                    Icon::new(DuckIcon::Folders)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
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
                icon_text_button(
                    "terminal-right-edit",
                    tr(cx, "编辑连接"),
                    Icon::new(DuckIcon::Pencil)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
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
                icon_text_button(
                    "terminal-right-delete",
                    tr(cx, "删除连接"),
                    Icon::new(DuckIcon::Trash2)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.destructive),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 克隆连接的命名：没占就用「副本」，占了一路往后加序号，绝不撞名。
    ///
    /// 这条钉住的是列表里「两条一模一样的名字」不再出现 —— 重名连接在界面上无法区分，
    /// 用户点哪条全凭猜。
    #[test]
    fn clone_label_appends_suffix_until_free() {
        // 没人占：直接用「副本」。
        assert_eq!(
            unique_clone_label("prod-01", "副本", |_| false),
            "prod-01 副本"
        );
        // 已被占：跳到下一个空位。
        let taken = ["prod-01 副本", "prod-01 副本 2"];
        assert_eq!(
            unique_clone_label("prod-01", "副本", |label| taken.contains(&label)),
            "prod-01 副本 3"
        );
        // 只占了中间一个：仍从最小可用序号起。
        let sparse = ["prod-01 副本 2"];
        assert_eq!(
            unique_clone_label("prod-01", "副本", |label| sparse.contains(&label)),
            "prod-01 副本"
        );
    }

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

    /// 档案里的私钥路径只属于「公钥」认证。
    ///
    /// 用户实测报告：「选了密码/SSH Agent 还是连不上，一直报
    /// `Permission denied (publickey,password)`」。根因是档案里残留的私钥路径
    /// 让 `duck-ssh` 加上 `-i` + `IdentitiesOnly=yes`，把 agent 与口令两条路一起关掉。
    /// 这条钉住「非公钥 ⇒ 不留私钥路径」，从而保证 `ssh_target` 拿不到 identity。
    #[test]
    fn key_path_is_kept_only_for_public_key_auth() {
        let path = "/Users/me/.ssh/id_ed25519".to_owned();
        assert_eq!(
            stored_key_file(AuthKind::Key, path.clone()),
            path,
            "公钥认证必须原样保留路径"
        );
        // 切到口令 / agent ⇒ 清空（否则 IdentitiesOnly 会顶掉口令与 agent）。
        assert_eq!(
            stored_key_file(AuthKind::Password, path.clone()),
            String::new()
        );
        assert_eq!(stored_key_file(AuthKind::Agent, path), String::new());
        // 没有路径时三种认证都得到空串（不是 `Some("")`）。
        assert_eq!(stored_key_file(AuthKind::Key, String::new()), String::new());
    }

    /// 用户实测报告：「SFTP 打开就报 远端命令失败（退出码 1）」。
    ///
    /// 根因在远端脚本（路径带单引号交给远端，`~` 不展开，见
    /// `duck_ssh::list_dir` 的前导）。界面上还并着第二处：远端「主目录」按钮交出的是
    /// `Side::home()`（空串），而 `normalize("")` 把它变成 `/` —— 于是「主目录」
    /// 把面板带到**根目录**，并且「远端目录尚未确定」这个前置状态被一并抹掉
    ///（写操作的空路径检查随之失效，会拿一个猜的目录去 `mkdir`）。
    /// 这条钉住「远端空路径保留原样，交给列举去解析 `~`」。
    #[test]
    fn remote_home_stays_unresolved_instead_of_becoming_root() {
        let mut pane = PaneState::new(Side::Remote);
        assert_eq!(pane.path, "", "未连接时远端不该有一个猜出来的路径");
        pane.navigate(Side::Remote, Side::Remote.home());
        assert_eq!(
            pane.path, "",
            "远端家目录不可知：留空串，由列举脚本在远端展开 `~`"
        );
        assert!(!pane.loaded, "留空串是为了触发一次列举");

        // 绝对路径照常归一（尾斜杠去掉）：远端不是「什么都原样收下」。
        pane.navigate(Side::Remote, "/var/log/".to_owned());
        assert_eq!(pane.path, "/var/log");
    }
}
