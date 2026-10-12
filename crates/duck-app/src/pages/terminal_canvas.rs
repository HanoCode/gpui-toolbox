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

use duck_components::{DuckIcon, body_text, tabular_numbers, title_text};
use duck_term::{
    CellSize, Color, Column, CursorShape, Dimensions, Encoded, Flags, KeyStroke, Line, Palette,
    Point as TermPoint, SelectionRange, Session, SessionEvent, Side, SpawnOptions, encode,
};
use duck_theme::{ThemeMode, active_theme, tr};
use gpui::{
    AnyElement, App, Context, CursorStyle, FocusHandle, Font, FontStyle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Rgba, Role, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, Styled as _, TextRun,
    UnderlineStyle, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon,
    menu::{ContextMenuExt as _, PopupMenuItem},
    v_flex,
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
/// 清屏序列：`ED 2` 清屏 + `ED 3` 清滚回缓冲 + `CUP` 归位。
pub(super) const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[3J\x1b[H";
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
        CloseTab,
    ]
);

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
    /// 主题指纹（深色?, 字号千分位），变了才重灌调色板。
    style_key: (bool, i32),
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
            style_key: (true, i32::MIN),
            exited: false,
            started: std::time::Instant::now(),
            origin: gpui::Point::default(),
            viewport: Size::default(),
            selecting: false,
        }
    }

    /// 本地 shell。
    fn spawn_local(cx: &mut Context<TerminalPage>) -> Self {
        Self::spawn(SpawnOptions::default(), cx)
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
            style_key: (true, i32::MIN),
            exited: true,
            started: std::time::Instant::now(),
            origin: gpui::Point::default(),
            viewport: Size::default(),
            selecting: false,
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
        // 口令走 askpass 助手（脚本落盘是幂等的，`duck_ssh` 自己也会落一次）。
        let helper = duck_ssh::askpass_script(socket_dir).ok();
        // 终端里不该出现「上次登录时间」之外的宿主环境：清掉 `PWD` 之类的继承值，
        // 让远端 shell 自己决定工作目录。
        let options = SpawnOptions {
            shell: Some(("ssh".to_owned(), target.terminal_argv(Some(socket_dir)))),
            env: target.env(helper.as_deref()),
            ..SpawnOptions::default()
        };
        Self::spawn(options, cx)
    }

    /// 底层会话（标签还没起来时是 `None`）。
    ///
    /// `pub(super)`：`terminal.rs` 的清屏路径也要拿它调 `scroll_to_bottom` ——
    /// 与画布层共用同一个会话句柄，不另存一份。
    pub(super) fn session(&self) -> Option<&Arc<Session>> {
        self.handle.as_ref()
    }

    /// 往 PTY 写一段字节（会话没起来时静默忽略）。
    pub(super) fn write(&self, bytes: &[u8]) {
        if let Some(session) = self.session() {
            session.write(bytes);
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
        if self.sessions.is_empty() {
            return false;
        }
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
        // 标签的回写（标题 / 关闭）必须等会话表借完再动 —— 两者都在 `self` 上。
        let mut retitled: Vec<(String, Option<String>)> = Vec::new();
        for (id, live) in &mut self.sessions {
            let Some(session) = live.session().map(Arc::clone) else {
                continue;
            };
            let mut repaint = false;
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
                    if !ssh {
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
            if repaint {
                retitled.push((id.clone(), live.title.clone()));
                dirty = true;
            }
        }
        for (id, title) in retitled {
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                let next = title.unwrap_or_else(|| tab.title.clone());
                if tab.title != next {
                    tab.title = next;
                    dirty = true;
                }
            }
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
        if dirty {
            cx.notify();
        }
        true
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
        let font_size = px(self.terminal_font_size);
        // 行高按 token 的比例随字号缩放：`mono_md` 是 13/20，直接沿用这个比例，
        // 用户调字号时网格保持同样的疏密，而不是像写死 1.5 那样越调越挤。
        let ratio = mono_token.line_height.to_f64() / mono_token.size.to_f64();
        let line_height = px(self.terminal_font_size * ratio as f32);

        let font = Font {
            family: tokens.typography.mono.clone(),
            features: tabular_numbers(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
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
        let style_key = (dark, (self.terminal_font_size * 1000.) as i32);
        let palette = palette_for(cx);
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
    fn cell_at(&self, tab_id: &str, position: gpui::Point<Pixels>) -> Option<(i32, usize, Side)> {
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
        // 指针落在这一格的左半边还是右半边：这是「选中含不含端点格」的语义来源
        // （`SelectionRange` 里 `Side::Left` 排除端点格、`Side::Right` 含）。
        // 不做这一步，向右拖选永远差最后一格。
        let side = if x / f64::from(cell.width) - column.floor() >= 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        Some((row, column as usize, side))
    }

    /// 鼠标按下（`fresh`）或拖拽中：把像素位置换算成**网格坐标**交给内核的选区状态机。
    ///
    /// 视口行 → 网格行要减掉 `display_offset`（滚上去看历史时视口第 0 行不是网格第 0 行）。
    /// 这一步必须在内核里问 —— 宿主自己记的行号在滚动之后就是错的。
    fn extend_selection(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        fresh: bool,
        cx: &mut Context<TerminalPage>,
    ) {
        let Some((row, column, side)) = self.cell_at(tab_id, position) else {
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
            session.select_start(point, side);
        } else {
            session.select_update(point, side);
        }
        cx.notify();
    }

    /// 左键在**这块画布上**按下：立一个「正在拖选」的标记并起一个空选区。
    ///
    /// 起手就调 `select_start` 而不是等第一次移动：内核的 `Selection::new` 是**覆盖**语义，
    /// 于是「点一下空白处」天然等于清掉上一次的选区（选区随即为空，
    /// `selection_text()` 返回 `None`，复制动作不会把剪贴板冲成空串）。
    pub(super) fn begin_selection(
        &mut self,
        tab_id: &str,
        position: gpui::Point<Pixels>,
        cx: &mut Context<TerminalPage>,
    ) {
        if let Some(live) = self.sessions.get_mut(tab_id) {
            live.selecting = true;
        }
        self.extend_selection(tab_id, position, true, cx);
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
        self.extend_selection(tab_id, position, false, cx);
    }

    /// 左键松开：结束这次拖选（选区本身留在内核里，等着被复制）。
    pub(super) fn end_selection(&mut self, tab_id: &str) {
        if let Some(live) = self.sessions.get_mut(tab_id) {
            live.selecting = false;
        }
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

    /// 把系统剪贴板的内容写进 PTY（走内核的括号粘贴模式处理）。
    fn paste_clipboard(&mut self, tab_id: &str, cx: &mut Context<TerminalPage>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        session.paste(&text);
        session.scroll_to_bottom();
        cx.notify();
    }

    /// 全选（回滚区 + 当前屏幕）。
    fn select_all(&mut self, tab_id: &str, cx: &mut Context<TerminalPage>) {
        let Some(session) = self.sessions.get(tab_id).and_then(TerminalSession::session) else {
            return;
        };
        session.select_all();
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
        let Some(live) = self.sessions.get(tab_id) else {
            return;
        };
        let Some(session) = live.session() else {
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
        let Some(live) = self.sessions.get_mut(tab_id) else {
            return;
        };
        // 格子高度为 0 时除法会变成 inf：先挡掉（正常路径下 `sync_cell_metrics`
        // 已经确保它为正，这里只是不把「万一」变成「除以零」）。
        if live.cell.height <= 0. {
            return;
        }
        let Some(session) = live.session().map(Arc::clone) else {
            return;
        };
        // 符号：GPUI 的 `delta.y` 为正 = 向下滚（看更晚的内容），而 `Session::scroll`
        // 的正数 = 向上翻（看历史）。两者相反，这里取负。
        let lines = match event.delta {
            // 传统滚轮：一格固定 3 行。
            ScrollDelta::Lines(delta) => -delta.y * SCROLL_LINES_PER_NOTCH,
            // 触控板 / 精确滚动：按像素换行，余量累积到下一帧。
            ScrollDelta::Pixels(delta) => -(delta.y.to_f64() / f64::from(live.cell.height)) as f32,
        };
        live.scroll_remainder += lines;
        let whole = live.scroll_remainder.trunc();
        live.scroll_remainder -= whole;
        if whole != 0. {
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
        let mono: SharedString = tokens.typography.mono.clone();
        let font_size = px(self.terminal_font_size);
        let cell = live.cell;
        let line_height = px(cell.height);

        let base_font = Font {
            family: mono.clone(),
            // 等宽数字：终端里数字宽度不一致会让整列错位。
            features: tabular_numbers(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };

        // 画布内边距：终端文字不贴边（与页面其余内容区同一对 token）。
        // 子元素的 `Bounds` 会自动扣掉它，`apply_canvas_size` 的列行换算因此天然正确。
        let pad_x = tokens.spacing.md;
        let pad_y = tokens.spacing.sm;

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
            for indexed in content.display_iter {
                let row = indexed.point.line.0 + offset;
                if !(0..screen_lines as i32).contains(&row) {
                    continue;
                }
                lines[row as usize].push(paint_cell(indexed.cell, colors, &live.palette));
            }

            let mut rows: Vec<AnyElement> = Vec::with_capacity(screen_lines);
            for cells in &lines {
                let (text, runs) = build_row(cells, &base_font);
                rows.push(
                    div()
                        .h(line_height)
                        .flex_none()
                        // 行是**网格行**，不是流式文本：一行就是一行，绝不换行。
                        // 换行的续行会溢出定高行盒、叠在下一行上 —— `ll` 这类长行
                        // 一多就满屏鬼影。超宽的部分由画布 `overflow_hidden` 裁掉。
                        .whitespace_nowrap()
                        .child(gpui::StyledText::new(text).with_runs(runs))
                        .into_any_element(),
                );
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
            (rows, cursor, bands)
        });

        let (rows, cursor, bands) = paint.unwrap_or_default();
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

        let focus = live.focus.clone();
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
            .cursor(CursorStyle::IBeam)
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
                move |this, _: &SendCtrlC, _window, cx| this.send_ctrl(&tab_id, "c", cx)
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
                move |this, _: &PasteFromClipboard, _window, cx| this.paste_clipboard(&tab_id, cx)
            }))
            .on_action(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, _: &SelectAll, _window, cx| this.select_all(&tab_id, cx)
            }))
            // —— 标签 ——
            // 与工具条那条「新建 / 关闭」是同一份状态机（`terminal.rs`），
            // 这里只是把 `⌘T` / `⌘W` 接到它上面。
            .on_action(cx.listener(|this, _: &NewTab, _window, cx| this.new_local_tab(cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _window, cx| this.close_active_tab(cx)))
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
            // —— 鼠标：聚焦 + 拖选 ——
            //
            // 选区状态归内核（`Term::selection`），这里只做「像素 → 网格坐标」的换算与
            // 起止时机；高亮由 `selection_bands` 那叠色块画出来。
            //
            // ⚠️ 不判 `TermMode::MOUSE_MODE`（应用要求鼠标上报时该把事件转给应用）：
            // 鼠标上报的编码（`CSI M` / SGR）尚未实现，此刻把事件转给 PTY 只会让
            // vim / htop 里连选择都做不了。取舍记在 `docs/architecture-gap-analysis.md`
            // 的「终端未做」里。
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
                        this.begin_selection(&tab_id, event.position, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener({
                let tab_id = tab_id.to_owned();
                move |this, event: &MouseMoveEvent, _window, cx| {
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
                    move |this, _: &MouseUpEvent, _window, _cx| this.end_selection(&tab_id)
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener({
                    let tab_id = tab_id.to_owned();
                    move |this, _: &MouseUpEvent, _window, _cx| this.end_selection(&tab_id)
                }),
            )
            // 右键菜单：终端里最常用的三件事（复制 / 粘贴 / 全选）。
            // 「复制」按**有没有选区**置灰 —— 菜单因此如实反映「现在能做什么」。
            // 这个布尔在渲染期算：`selection_range` 走 `try_with_term`，拿不到锁当没有，
            // 不会把 UI 线程按在 PTY 上。
            .context_menu({
                let view = self.view.clone();
                let menu_tab_id = tab_id.to_owned();
                let has_selection = session.selection_range().is_some();
                move |menu, _, cx| {
                    let menu = menu.item(
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
                    );
                    let menu = menu.item(PopupMenuItem::new(tr(cx, "粘贴")).on_click({
                        let tab_id = menu_tab_id.clone();
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| this.paste_clipboard(&tab_id, cx));
                        }
                    }));
                    menu.separator()
                        .item(PopupMenuItem::new(tr(cx, "全选")).on_click({
                            let tab_id = menu_tab_id.clone();
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.select_all(&tab_id, cx));
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
                    // 于是文字压在选区底色上、光标仍压在最上面。
                    .children(highlight)
                    .children(rows)
                    .when_some(cursor, |this, cursor| this.child(cursor)),
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

/// 一个单元格画出来需要的最少信息。
struct CellPaint {
    ch: char,
    fg: Hsla,
    /// 与终端底色不同才画（`None` = 用底色）。
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
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
            ch: ' ',
            fg: foreground(palette),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
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

    // 底色与终端默认底色一致时不画 run 背景：让容器底色透出来，
    // 少一层绘制，也让「行尾之外」的空白与「行内空白」看起来是同一种底。
    let is_default_bg = bg == rgb_of(&palette.background);
    CellPaint {
        ch: cell.c,
        fg,
        bg: (!is_default_bg).then_some(bg),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL),
        strike: flags.contains(Flags::STRIKEOUT),
    }
}

/// 把一行单元格拼成文本与分色 run。
///
/// GPUI 的 `TextRun::len` 是**字节数**，且 `with_runs` 会在「run 长度之和 ≠ 文本长度」时
/// 直接 panic（`elements/text.rs:526`）。所以这里一边写 `String` 一边记每个 run 的字节长，
/// 逐格累加保证严丝合缝。相邻格子的样式完全相同就并进同一个 run —— 一整行白字只该产出一个
/// run，否则一屏 200 列 × 50 行会变成一万个 run。
fn build_row(cells: &[CellPaint], font: &Font) -> (String, Vec<TextRun>) {
    let mut text = String::with_capacity(cells.len());
    let mut runs: Vec<TextRun> = Vec::new();
    // `(run, 已累计的字节长)`：`run.len` 要等这一格串结束才知道，先攒着。
    let mut current: Option<(TextRun, usize)> = None;

    for cell in cells {
        text.push(cell.ch);
        let len = cell.ch.len_utf8();
        let matches = current.as_ref().is_some_and(|(run, _)| {
            run.color == cell.fg
                && run.background_color == cell.bg
                && (run.font.weight == FontWeight::BOLD) == cell.bold
                && (run.font.style == FontStyle::Italic) == cell.italic
                && run.underline.is_some() == cell.underline
                && run.strikethrough.is_some() == cell.strike
        });
        if !matches {
            if let Some((mut run, length)) = current.take() {
                run.len = length;
                runs.push(run);
            }
            current = Some((
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
                    underline: cell.underline.then_some(UnderlineStyle {
                        color: None,
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: cell.strike.then_some(gpui::StrikethroughStyle {
                        color: None,
                        thickness: px(1.),
                    }),
                },
                0,
            ));
        }
        if let Some((_, length)) = current.as_mut() {
            *length += len;
        }
    }

    if let Some((mut run, length)) = current {
        run.len = length;
        runs.push(run);
    }
    (text, runs)
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
            ch,
            fg: rgb_hsla(0xff, 0xff, 0xff),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
        }
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
        let (text, runs) = build_row(&cells, &font);
        assert_eq!(text, "echo hi");
        assert_runs_cover(&text, &runs);
    }

    /// 多字节字符（CJK / 宽字符）每格 3 字节：run 长度按**字节**算才不会被截断。
    #[test]
    fn multibyte_cells_count_bytes_not_chars() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "中文café".chars().map(plain).collect();
        let (text, runs) = build_row(&cells, &font);
        assert_eq!(text.chars().count(), 6);
        assert!(text.len() > 6, "应当按字节计，实际 {} 字节", text.len());
        assert_runs_cover(&text, &runs);
    }

    /// 整行同一样式只该产出一个 run —— 一屏 200 列 × 50 行若逐格产 run 就是一万个。
    #[test]
    fn identical_styles_merge_into_one_run() {
        let font = mono_font();
        let cells: Vec<CellPaint> = "-".repeat(80).chars().map(plain).collect();
        let (text, runs) = build_row(&cells, &font);
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
        let (text, runs) = build_row(&cells, &font);
        assert_eq!(text, "abC");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len, 2);
        assert_eq!(runs[1].len, 1);
        assert_eq!(runs[1].font.weight, FontWeight::BOLD);
        assert_runs_cover(&text, &runs);
    }

    /// 空行（一帧都没拿到锁时的兜底行）：文本与 runs 都为空，`with_runs` 收空切片是合法的。
    #[test]
    fn empty_row_stays_empty() {
        let font = mono_font();
        let (text, runs) = build_row(&[], &font);
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
}
