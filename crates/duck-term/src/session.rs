//! PTY 会话：起 shell、喂字节、读网格。
//!
//! 一个 [`Session`] 对应一个真实进程组（shell 及其子进程）。它同时是**终端的全部状态**：
//! 网格、滚动位置、选择、光标都在 [`Term`] 里，事件循环线程独占写入，UI 线程按需加锁读。

use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Handler, Rgb};

use crate::palette::{COLOR_COUNT, Palette};

/// 一次轮询最多取走的事件数：防止某个刷屏进程把 UI 线程卡在事件处理里。
const MAX_EVENTS_PER_DRAIN: usize = 512;

/// 单元格尺寸（逻辑像素）。由渲染层按字体度量算出后交给内核 —— 内核不认识字体，
/// 只知道「一格多大」，PTY 的 `TIOCSWINSZ` 需要像素值来做 `SIGWINCH` 语义。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

impl Default for CellSize {
    fn default() -> Self {
        // 13px 等宽字体的典型度量，仅作首帧的占位值：真实值在第一次布局后写入。
        Self {
            width: 8.,
            height: 16.,
        }
    }
}

/// 起会话的入参。
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    pub cols: u16,
    pub rows: u16,
    pub cell: CellSize,
    /// `None` 表示用登录 shell（`$SHELL`，再退回 `passwd`）。
    pub shell: Option<(String, Vec<String>)>,
    pub cwd: Option<PathBuf>,
    /// 追加到子进程环境变量上的键值（覆盖同名变量）。
    pub env: Vec<(String, String)>,
    /// 回滚缓冲行数。0 表示不留历史。
    pub scrollback: usize,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
        Self {
            cols: 80,
            rows: 24,
            cell: CellSize::default(),
            shell: Some((shell, Vec::new())),
            cwd: std::env::var("HOME").ok().map(PathBuf::from),
            env: vec![
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                // 让 `ls --color` / `grep --color` 走 24 位色，与渲染层的 `Rgb` 直通一致。
                ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ],
            scrollback: 10_000,
        }
    }
}

/// 剪贴板类别：终端里的 `OSC 52` 能区分「剪贴板」与「主选择区」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardKind {
    Clipboard,
    Selection,
}

/// 需要交给 UI 线程处理的事件。
///
/// **刻意保持很小**：能由事件循环线程自己答的（设备属性应答、视口尺寸查询、调色板查询、
/// 颜色方案偏好）都在 [`EventProxy`] 里就地应答了 —— 那些往返纯粹是延迟，还多一次加锁。
/// 留在这里的都是**只有 UI 线程才知道答案**的：系统剪贴板、主题明暗、以及需要重绘的信号。
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// 终端有新输出，渲染层应重绘。这是最高频的一个。
    Wakeup,
    /// 应用通过 `OSC 0/2` 设了窗口标题。
    Title(String),
    /// 应用要求恢复默认标题。
    ResetTitle,
    /// 响铃 (`BEL`)。
    Bell,
    /// 光标闪烁状态变了（应用改了 DECSCUSR）。
    CursorBlinkingChange,
    /// 网格变了，鼠标指针形状可能需要跟着变。
    MouseCursorDirty,
    /// 应用要求把文本写进系统剪贴板（`OSC 52` 写）。
    ClipboardStore(ClipboardKind, String),
    /// 应用要求读系统剪贴板。请用 [`Session::answer_clipboard`] 带上 `request` 回填，
    /// 否则应用会一直等（通常 1~2 秒后超时）。
    ClipboardQuery { request: u64, kind: ClipboardKind },
    /// 子 shell 退出。`None` 表示被信号杀死（拿不到状态码）。
    ChildExit(Option<i32>),
    /// 终端要求宿主退出。
    Exit,
}

/// 事件代理：装在 `Term` 上，终端状态机每有事就回调它。
///
/// 对外**不需要任何方法** —— 它只是 `Term<EventProxy>` 的类型参数。之所以公开类型，
/// 是因为 [`Session::with_term`] 要把 `&mut Term<EventProxy>` 交给渲染层。
#[derive(Clone)]
pub struct EventProxy {
    events: Sender<SessionEvent>,
    /// 事件循环的 `Msg` 通道。`EventLoop::new` 之后才有的东西，
    /// 而 `Term` 必须先于它构造 —— 这一格 `Option` 就是用来打破这个循环的。
    /// 外面那层 `Arc` 是为了让 [`EventProxy`] 可克隆（`std::sync::Mutex` 本身不 `Clone`）。
    sender: Arc<Mutex<Option<EventLoopSender>>>,
    /// `(cols, rows, cell_w, cell_h)` 各 16 位打包，供 `TextAreaSizeRequest` 自答。
    size: Arc<AtomicU64>,
    palette: Arc<Mutex<Palette>>,
    dark: Arc<AtomicBool>,
    /// 待回填的 `OSC 52` 读请求：id → 把剪贴板文本编成转义序列的函数。
    pending_clipboard: Arc<Mutex<HashMap<u64, ClipboardFormatter>>>,
    next_request: Arc<AtomicU64>,
}

type ClipboardFormatter = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

impl EventProxy {
    fn notify_bytes(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        // 空序列会把 PTY 写侧挂住（上游 `Notifier` 同样拦这一手）。
        if data.is_empty() {
            return;
        }
        let sender = self.sender.lock().expect("proxy sender lock").clone();
        if let Some(sender) = sender {
            // 事件循环已退出时发送失败是正常路径（关闭标签页的竞态），忽略即可。
            let _ = sender.send(Msg::Input(Cow::Owned(data)));
        }
    }

    fn window_size(&self) -> WindowSize {
        let packed = self.size.load(Ordering::Relaxed);
        WindowSize {
            num_cols: (packed >> 48) as u16,
            num_lines: (packed >> 32) as u16,
            cell_width: (packed >> 16) as u16,
            cell_height: packed as u16,
        }
    }

    /// 回答 `OSC 4 / 10 / 11`（调色板查询）。
    ///
    /// 取色**只认宿主配色**，不读 `Term::colors()` —— 这里正处在终端状态机的回调栈上，
    /// 锁已经被事件循环持有，再去锁一次就是死锁。OSC 4 的应用覆盖因此不参与应答
    /// （应用改了色再问自己改的颜色属于极罕见用法，代价可接受）。
    fn color_reply(&self, index: usize, format: &dyn Fn(Rgb) -> String) {
        let palette = *self.palette.lock().expect("proxy palette lock");
        if let Some(rgb) = palette.resolve(index) {
            self.notify_bytes(format(rgb));
        }
    }
}

impl Notify for EventProxy {
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, data: B) {
        self.notify_bytes(data.into().into_owned());
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::Wakeup => SessionEvent::Wakeup,
            Event::Title(title) => SessionEvent::Title(title),
            Event::ResetTitle => SessionEvent::ResetTitle,
            Event::Bell => SessionEvent::Bell,
            Event::CursorBlinkingChange => SessionEvent::CursorBlinkingChange,
            Event::MouseCursorDirty => SessionEvent::MouseCursorDirty,
            Event::Exit => SessionEvent::Exit,
            Event::ChildExit(status) => SessionEvent::ChildExit(status.code()),

            // —— 以下四类由本线程直接应答，不打扰 UI ——
            Event::PtyWrite(text) => {
                self.notify_bytes(text);
                return;
            }
            Event::TextAreaSizeRequest(format) => {
                let reply = format(self.window_size());
                self.notify_bytes(reply);
                return;
            }
            Event::ColorRequest(index, format) => {
                self.color_reply(index, format.as_ref());
                return;
            }
            Event::ColorSchemeRequest => {
                // xterm 的 `CSI ? 997 n`：1 = 深色、2 = 浅色。
                let reply = if self.dark.load(Ordering::Relaxed) {
                    "\x1b[?997;1n"
                } else {
                    "\x1b[?997;2n"
                };
                self.notify_bytes(reply);
                return;
            }

            Event::ClipboardStore(kind, text) => {
                SessionEvent::ClipboardStore(map_clipboard(kind), text)
            }
            Event::ClipboardLoad(kind, format) => {
                let request = self.next_request.fetch_add(1, Ordering::Relaxed);
                self.pending_clipboard
                    .lock()
                    .expect("proxy pending lock")
                    .insert(request, format);
                SessionEvent::ClipboardQuery {
                    request,
                    kind: map_clipboard(kind),
                }
            }
        };

        // UI 侧已经没了就静默丢弃：会话正在关闭。
        let _ = self.events.send(mapped);
    }
}

fn map_clipboard(kind: alacritty_terminal::term::ClipboardType) -> ClipboardKind {
    match kind {
        alacritty_terminal::term::ClipboardType::Selection => ClipboardKind::Selection,
        alacritty_terminal::term::ClipboardType::Clipboard => ClipboardKind::Clipboard,
    }
}

/// 网格尺寸。`Term` 只需要这三个数。
#[derive(Clone, Copy, Debug)]
struct Dims {
    cols: usize,
    rows: usize,
    scrollback: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows + self.scrollback
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 一个终端会话。
///
/// 生命周期与「一个标签页」对齐：`Drop` 时给事件循环发 `Shutdown`，
/// 子进程随之收到 PTY 关闭（`SIGHUP`），不会留孤儿 shell。
pub struct Session {
    term: Arc<FairMutex<Term<EventProxy>>>,
    /// `std::sync::mpsc` 的两端都是 `Send` 但 `!Sync`。UI 侧要把 `Arc<Session>` 放进
    /// `cx.spawn` 的弱引用里（那里要求 `Send + Sync`），所以两端各包一层 `Mutex` ——
    /// 代价只是每次收发多一次无竞争加锁，换来的是整个会话可以被安全共享。
    sender: Mutex<EventLoopSender>,
    events: Mutex<Receiver<SessionEvent>>,
    size: Arc<AtomicU64>,
    palette: Arc<Mutex<Palette>>,
    dark: Arc<AtomicBool>,
    pending_clipboard: Arc<Mutex<HashMap<u64, ClipboardFormatter>>>,
    /// 回滚缓冲行数。构造后不变，`resize` 重建 `Dims` 时要带上它。
    scrollback: usize,
}

impl Session {
    /// 起一个本地 shell。`options.shell` 为 `None` 时用登录 shell。
    pub fn spawn(options: SpawnOptions) -> io::Result<Self> {
        let SpawnOptions {
            cols,
            rows,
            cell,
            shell,
            cwd,
            env,
            scrollback,
        } = options;

        let (events_tx, events_rx) = mpsc::channel();
        let dims = Dims {
            cols: usize::from(cols),
            rows: usize::from(rows),
            scrollback,
        };
        let size = Arc::new(AtomicU64::new(pack_size(cols, rows, cell)));
        let palette = Arc::new(Mutex::new(Palette::default()));
        let dark = Arc::new(AtomicBool::new(true));
        let pending_clipboard = Arc::new(Mutex::new(HashMap::new()));

        let proxy = EventProxy {
            events: events_tx,
            sender: Arc::new(Mutex::new(None)),
            size: Arc::clone(&size),
            palette: Arc::clone(&palette),
            dark: Arc::clone(&dark),
            pending_clipboard: Arc::clone(&pending_clipboard),
            next_request: Arc::new(AtomicU64::new(1)),
        };

        let config = TermConfig {
            scrolling_history: scrollback,
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &dims, proxy.clone())));
        // 事件循环要独占一个 `Term` 的句柄；UI 侧留另一个读网格。
        let event_term = Arc::clone(&term);

        let pty = tty::new(
            &tty_options(shell, cwd, env),
            window_size(cols, rows, cell),
            // `window_id` 只用于把 `WINDOWID` 环境变量透给子进程；GUI 下没有原生窗口句柄，
            // 给 0（alacritty 的 headless / 测试路径同样如此）。
            0,
        )?;

        let event_loop = EventLoop::new(event_term, proxy.clone(), pty, false, false)?;
        let sender = event_loop.channel();
        *proxy.sender.lock().expect("proxy sender lock") = Some(sender.clone());
        // 线程分离：`JoinHandle` 丢掉，事件循环靠 `Msg::Shutdown` 收尾。
        drop(event_loop.spawn());

        Ok(Self {
            term,
            sender: Mutex::new(sender),
            events: Mutex::new(events_rx),
            size,
            palette,
            dark,
            pending_clipboard,
            scrollback,
        })
    }

    /// 把按键 / 粘贴产生的字节写进 PTY。
    pub fn write(&self, bytes: &[u8]) {
        self.send(Msg::Input(Cow::Owned(bytes.to_vec())));
    }

    /// 往事件循环投一条消息。事件循环已退出时静默丢弃（关标签页的正常竞态）。
    fn send(&self, msg: Msg) {
        let _ = self.sender.lock().expect("session sender lock").send(msg);
    }

    /// 把一段文本当作用户输入写进 PTY（会自动转成 UTF-8 字节）。
    pub fn write_str(&self, text: &str) {
        self.write(text.as_bytes());
    }

    /// 粘贴一段文本（⌘V / 右键粘贴都走这里）。
    ///
    /// 与 [`Session::write_str`] 的区别在**括号粘贴模式**（`CSI ?2004 h`，vim、zsh
    /// 等现代程序都会开）：程序开了它，就是声明「请把粘贴的文本和用户敲的键区分开」——
    /// 多行文本要包上 `CSI 200~ … CSI 201~`，否则粘贴进 shell 的换行会被逐行执行
    /// （粘贴一段命令脚本 = 没看清就回车了最后一行），粘贴进 vim 的缩进会层层叠加。
    /// 终端的责任是遵守这个约定；没开模式就按普通输入原样写入。
    pub fn paste(&self, text: &str) {
        // 换行统一成 CR：终端键盘上的回车是 `\r`，粘贴的文本里往往是 `\n`；
        // 行规程把输入行的 `\r` 翻成 `\n`，但裸 `\n` 没有这层待遇（ICRNL 只作用于 CR）。
        let text = if text.contains('\n') {
            Cow::Owned(text.replace("\r\n", "\r").replace('\n', "\r"))
        } else {
            Cow::Borrowed(text)
        };
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            self.write(&bytes);
        } else {
            self.write(text.as_bytes());
        }
    }

    /// 调整网格尺寸。
    ///
    /// 收 `&self` 而非 `&mut self`：会话内部全是内部可变（`FairMutex` / `AtomicU64`），
    /// 而 UI 侧持有的是 `Arc<Session>`（多个标签共享、渲染路径按 `&` 借用），
    /// 要求独占借用只会逼调用方去改数据结构，换不来任何安全性。
    ///
    /// 幂等：布局每帧都会把测量结果回传一次，参数没变就什么都不做 —— 否则
    /// `SIGWINCH` 会变成每帧一次的噪声，vim / htop 这类全屏应用会被刷屏式的重画拖垮。
    ///
    /// 两步顺序不能反：先把 `Term` 的网格改成新尺寸，再让 PTY 发 `SIGWINCH`。
    /// 反过来的话，`SIGWINCH` 唤醒的程序可能在一个还是旧尺寸的网格上重画，右缘会缺一块。
    pub fn resize(&self, cols: u16, rows: u16, cell: CellSize) {
        if cols == 0 || rows == 0 {
            return;
        }
        let packed = pack_size(cols, rows, cell);
        if self.size.swap(packed, Ordering::Relaxed) == packed {
            return;
        }
        {
            let mut term = self.term.lock();
            if term.columns() != usize::from(cols) || term.screen_lines() != usize::from(rows) {
                term.resize(Dims {
                    cols: usize::from(cols),
                    rows: usize::from(rows),
                    scrollback: self.scrollback,
                });
            }
        }
        // 单元格像素尺寸变了也要通知 PTY：程序靠它算图片 / 表格的像素宽度。
        self.send(Msg::Resize(window_size(cols, rows, cell)));
    }

    /// 当前网格尺寸。
    #[must_use]
    pub fn grid_size(&self) -> (u16, u16) {
        let packed = self.size.load(Ordering::Relaxed);
        ((packed >> 48) as u16, (packed >> 32) as u16)
    }

    /// 加锁读（并允许改）网格 —— 渲染层的主要入口。
    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<EventProxy>) -> R) -> R {
        f(&mut self.term.lock())
    }

    /// 试着加锁；事件循环正在解析大批输出时返回 `None`。
    ///
    /// 渲染路径应当用它 —— 宁可这一帧沿用旧内容，也不要把 UI 线程挂在锁上。
    pub fn try_with_term<R>(&self, f: impl FnOnce(&mut Term<EventProxy>) -> R) -> Option<R> {
        self.term.try_lock_unfair().map(|mut term| f(&mut term))
    }

    /// 取走待处理事件，交给 `handle`。
    ///
    /// 返回值表示**是否还能再取**：`true` 说明这一轮撞到了 [`MAX_EVENTS_PER_DRAIN`] 上限，
    /// 调用方应当尽快再排一次（否则高吞吐场景下事件会积压）。
    #[must_use]
    pub fn drain_events(&self, mut handle: impl FnMut(SessionEvent)) -> bool {
        let mut handled = 0;
        let events = self.events.lock().expect("session events lock");
        loop {
            match events.try_recv() {
                Ok(event) => {
                    handle(event);
                    handled += 1;
                    if handled >= MAX_EVENTS_PER_DRAIN {
                        return true;
                    }
                }
                Err(TryRecvError::Empty) => return false,
                // 事件循环线程结束了（shell 也退出了），没有后续事件。
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// 滚动视口。正数向上翻（看历史），负数向下。
    pub fn scroll(&self, lines: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    /// 把视口拉回底部（跟随输出）。
    pub fn scroll_to_bottom(&self) {
        use alacritty_terminal::grid::Scroll;
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// 把视口拉到最顶（回溯最早的历史）。
    ///
    /// 与 [`Self::scroll`] 的区别：`scroll(i32::MAX)` 也能到位，但会先做一次加法再钳位；
    /// `Scroll::Top` 是内核的直达形式，语义更清楚。
    pub fn scroll_to_top(&self) {
        use alacritty_terminal::grid::Scroll;
        self.term.lock().scroll_display(Scroll::Top);
    }

    // ── 选区 ─────────────────────────────────────────────────────────────
    //
    // 选区的**状态**归内核（alacritty 的 `Term::selection`），宿主只负责把鼠标位置
    // 换算成网格坐标。这么分工的理由有三条，任何一条都足以否掉「在 UI 层自己存一份
    // 行列区间」的做法：
    //
    // 1. **滚动/回滚时选区会跟着走**：`Term` 在滚动、重排、行被挤出屏幕时都会
    //    自己 `rotate` / 丢弃选区（`term/mod.rs` 的 `scroll_display` / `resize`）。
    //    UI 层自己存的行号在滚动之后就是错的，而错得**没有症状** —— 高亮框还在，
    //    只是框住的已经不是当初那段字。
    // 2. **宽字符**：双宽字符的尾格（`Flags::WIDE_CHAR` 后的 spacer）算不算选中，
    //    只有 `SelectionRange::contains_cell` 这一份实现是对的。
    // 3. **取文本**：`selection_to_string` 要处理软换行（`WRAPLINE`）、行尾填充、
    //    块选与「整行选」的差异 —— 这些都是 alacritty 已经写完并测过的逻辑。

    /// 开始一次简单选区（鼠标按下）。`point` 是**网格坐标**（可带负行号＝回滚区）。
    pub fn select_start(&self, point: Point, side: Side) {
        self.with_term(|term| {
            term.selection = Some(Selection::new(SelectionType::Simple, point, side));
        });
    }

    /// 拖拽中更新选区末端。没有在选的选区时退化成「开一个新的」——
    /// 鼠标移动可能先于按下事件到达（快速拖拽），丢掉这一下会让拖选断掉。
    pub fn select_update(&self, point: Point, side: Side) {
        self.with_term(|term| match term.selection.as_mut() {
            Some(selection) => selection.update(point, side),
            None => term.selection = Some(Selection::new(SelectionType::Simple, point, side)),
        });
    }

    /// 全选：从回滚缓冲最顶一行到当前屏幕最后一行。
    ///
    /// 用 `SelectionType::Lines` 而不是 Simple —— 整行选的语义（含行尾、跨行不粘连）
    /// 由内核保证，宿主不需要自己拼「这一行到哪结束」。
    pub fn select_all(&self) {
        self.with_term(|term| {
            let columns = Dimensions::columns(term);
            let top = Dimensions::topmost_line(term);
            let bottom = Dimensions::bottommost_line(term);
            if columns == 0 || bottom < top {
                term.selection = None;
                return;
            }
            let mut selection = Selection::new(
                SelectionType::Lines,
                Point::new(top, Column(0)),
                Side::Left,
            );
            selection.update(Point::new(bottom, Column(columns - 1)), Side::Right);
            term.selection = Some(selection);
        });
    }

    /// 清掉选区（点击空白处、切换标签）。
    pub fn clear_selection(&self) {
        self.with_term(|term| term.selection = None);
    }

    /// 选区里的文本；没有选区、或选区还只是个「点」时是 `None`。
    ///
    /// 「点」也返回 `None` 是刻意的：用户单击一下就想复制，剪贴板里应该是原内容，
    /// 而不是被一个空字符串冲掉。
    pub fn selection_text(&self) -> Option<String> {
        self.with_term(|term| {
            if term.selection.as_ref().is_none_or(Selection::is_empty) {
                return None;
            }
            term.selection_to_string()
        })
    }

    /// 当前选区（网格坐标），供渲染层画高亮。
    pub fn selection_range(&self) -> Option<SelectionRange> {
        self.try_with_term(|term| {
            term.selection
                .as_ref()
                .and_then(|selection| selection.to_range(term))
        })
        .flatten()
    }

    /// 设置宿主配色。
    ///
    /// 做两件事，缺一不可：
    /// 1. 灌进 `Term` 的颜色表 —— 这样逐格渲染时 `Named` / `Indexed` 色能解析成具体 RGB，
    ///    并且应用用 `OSC 4` 做的覆盖会落在同一张表上；
    /// 2. 存一份给事件循环线程 —— 它回答 `OSC 4/10/11` 查询时不能锁 `Term`（会死锁）。
    pub fn apply_palette(&self, palette: Palette) {
        {
            let mut term = self.term.lock();
            for index in 0..COLOR_COUNT {
                // `set_color` 是 `vte::ansi::Handler` 的方法，与终端收到 `OSC 4` 时走的是同一条路。
                if let Some(rgb) = palette.resolve(index) {
                    term.set_color(index, rgb);
                }
            }
        }
        *self.palette.lock().expect("session palette lock") = palette;
    }

    /// 告诉内核宿主当前是深色还是浅色主题，用于回答 `OSC 2031`。
    pub fn set_dark(&self, dark: bool) {
        self.dark.store(dark, Ordering::Relaxed);
    }

    /// 回填一个 [`SessionEvent::ClipboardQuery`]。
    pub fn answer_clipboard(&self, request: u64, text: &str) {
        let formatter = self
            .pending_clipboard
            .lock()
            .expect("session pending lock")
            .remove(&request);
        if let Some(formatter) = formatter {
            self.send(Msg::Input(Cow::Owned(formatter(text).into_bytes())));
        }
    }

    /// 当前视口尺寸（列 / 行 / 单元格像素），供渲染层对齐字符网格。
    #[must_use]
    pub fn window_size(&self) -> WindowSize {
        let packed = self.size.load(Ordering::Relaxed);
        WindowSize {
            num_cols: (packed >> 48) as u16,
            num_lines: (packed >> 32) as u16,
            cell_width: (packed >> 16) as u16,
            cell_height: packed as u16,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 事件循环收到就退出；PTY 的 `Drop` 会关闭 master fd，子进程因此收到 `SIGHUP`。
        self.send(Msg::Shutdown);
    }
}

fn tty_options(
    shell: Option<(String, Vec<String>)>,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
) -> tty::Options {
    tty::Options {
        shell: shell.map(|(program, args)| tty::Shell::new(program, args)),
        working_directory: cwd,
        drain_on_exit: false,
        env: env.into_iter().collect(),
        #[cfg(target_os = "windows")]
        escape_args: false,
    }
}

fn window_size(cols: u16, rows: u16, cell: CellSize) -> WindowSize {
    WindowSize {
        num_cols: cols,
        num_lines: rows,
        cell_width: cell.width.round().clamp(0., f32::from(u16::MAX)) as u16,
        cell_height: cell.height.round().clamp(0., f32::from(u16::MAX)) as u16,
    }
}

fn pack_size(cols: u16, rows: u16, cell: CellSize) -> u64 {
    let size = window_size(cols, rows, cell);
    (u64::from(size.num_cols) << 48)
        | (u64::from(size.num_lines) << 32)
        | (u64::from(size.cell_width) << 16)
        | u64::from(size.cell_height)
}

/// 只在测试里用（`pack_size` 的对称操作）：生产侧读尺寸都走 `Session::window_size`。
#[cfg(test)]
fn unpack_cell(packed: u64) -> CellSize {
    CellSize {
        width: f32::from((packed >> 16) as u16),
        height: f32::from(packed as u16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 只有测试会显式构造 `Line`（生产代码里的行号一律从 `Term` 上问，
    // 由 `Dimensions::topmost_line` 之类的接口给出），所以放在测试模块里。
    use alacritty_terminal::index::Line;
    use std::time::{Duration, Instant};

    /// 视口内容按行拼成文本（行尾空格保留 —— 断言里用 `contains` 就够）。
    fn grid_text(session: &Session) -> String {
        session
            .try_with_term(|term| {
                let content = term.renderable_content();
                let mut out = String::new();
                let mut line = None;
                for indexed in content.display_iter {
                    let current = indexed.point.line.0;
                    // 不用 let-chain：本仓 MSRV 是 1.85，let-chain 要 1.88。
                    if line.is_some_and(|previous| previous != current) {
                        out.push('\n');
                    }
                    line = Some(current);
                    out.push(indexed.cell.c);
                }
                out
            })
            .unwrap_or_default()
    }

    fn wait_for_text(session: &Session, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // 事件必须被取走，否则事件循环的 mpsc 很快堆满。
            let _ = session.drain_events(|_| {});
            if grid_text(session).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 端到端：真起 PTY、真跑子进程、真解析 ANSI，网格里应当有它的输出。
    ///
    /// 子进程刻意用 `sleep` 撑住不退出 —— `sh -c 'printf …'` 会在输出还没被读走时就消失，
    /// 那时 `drain_on_exit = false` 的事件循环可能已经收摊（真实终端里这不成问题，
    /// 因为外壳一直开着；但测试要的是确定性）。
    #[test]
    fn shell_output_reaches_the_grid() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "printf 'duck-ok\\n'; sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        assert!(
            wait_for_text(&session, "duck-ok", Duration::from_secs(10)),
            "网格里应出现子进程输出，实际内容：{:?}",
            grid_text(&session)
        );
    }

    /// 写进 PTY 的字节要能被 shell 读回来 —— 覆盖「键盘输入 → shell 回显」这条闭环。
    #[test]
    fn typed_bytes_round_trip_through_the_pty() {
        let session = Session::spawn(SpawnOptions {
            // `cat` 把 stdin 原样吐回，正好当回显探针；`; sleep` 同样是为了不提早收摊。
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "cat; sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        // 等 `cat` 起来再写，避免字节落在 shell 的启动窗口里。
        std::thread::sleep(Duration::from_millis(300));
        session.write(b"round-trip\n");

        assert!(
            wait_for_text(&session, "round-trip", Duration::from_secs(10)),
            "写进 PTY 的内容应被 cat 吐回，实际内容：{:?}",
            grid_text(&session)
        );
    }

    #[test]
    fn resize_updates_the_grid_dimensions() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        session.resize(120, 40, CellSize::default());
        assert_eq!(session.grid_size(), (120, 40));
        let (lines, columns) = session.with_term(|term| {
            use alacritty_terminal::grid::Dimensions;
            (term.screen_lines(), term.columns())
        });
        assert_eq!((lines, columns), (40, 120));
    }

    /// 选区：按下 → 拖拽 → 取文本。
    ///
    /// 这条是「终端里选不中字」的回归测试。内核负责「选中的是哪些格子」，
    /// 宿主只负责把鼠标像素换算成网格坐标 —— 换算错了在界面上表现为
    /// 「拖了半天什么都没选中」，在这里则表现为取出来的是别的字。
    #[test]
    fn simple_selection_returns_the_selected_text() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "printf 'duck-select-ok\\n'; sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(&session, "duck-select-ok", Duration::from_secs(10)));

        // 第 0 行第 0..3 列 = `duck`。
        //
        // 末端传 `Side::Right` = 「指针落在这一格的右半边」，于是这一格**算选中**
        //（`Side::Left` 是排除它）。这条 off-by-one 就是鼠标拖选的语义所在：
        // 界面层按指针在半格的哪一侧给 side，内核据此决定端点含不含。
        session.select_start(Point::new(Line(0), Column(0)), Side::Left);
        session.select_update(Point::new(Line(0), Column(3)), Side::Right);
        assert_eq!(
            session.selection_text().as_deref(),
            Some("duck"),
            "选区文本要与网格内容对上"
        );

        // 换一段：3..6 列 = `k-se`（起点也变，验证 update 真的在挪动末端）。
        session.select_start(Point::new(Line(0), Column(3)), Side::Left);
        session.select_update(Point::new(Line(0), Column(6)), Side::Right);
        assert_eq!(session.selection_text().as_deref(), Some("k-se"));

        // 清掉之后就没有选区了（点击空白处的路径）。
        session.clear_selection();
        assert_eq!(session.selection_text(), None);
    }

    /// 单击（只 `select_start`、没拖过）**不算选区**。
    ///
    /// 这条划的是剪贴板的边界：⌘C 在没有选区时必须是空操作 ——
    /// 否则一次普通点击就会把用户剪贴板里的东西冲成一个空串。
    #[test]
    fn a_bare_click_is_not_a_selection() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "printf 'duck-select-ok\\n'; sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(&session, "duck-select-ok", Duration::from_secs(10)));

        session.select_start(Point::new(Line(0), Column(3)), Side::Left);
        assert_eq!(
            session.selection_text(),
            None,
            "起点与终点重合的选区必须算「没有选区」"
        );
    }

    /// 全选要覆盖到回滚区（`topmost_line` 是**实际存下来的**最早一行，
    /// 不是配置里的上限），并且行尾填充不进来。
    #[test]
    fn select_all_covers_the_scrollback() {
        let session = Session::spawn(SpawnOptions {
            rows: 4,
            scrollback: 100,
            shell: Some((
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "printf 'line-1\\nline-2\\nline-3\\nline-4\\nline-5\\nline-6\\n'; sleep 5"
                        .to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(&session, "line-6", Duration::from_secs(10)));

        session.select_all();
        let text = session.selection_text().expect("全选之后必须有文本");
        assert!(
            text.contains("line-1"),
            "全选要一直回溯到最早的一行，实际：{text:?}"
        );
        assert!(text.contains("line-6"), "全选也要含屏幕上最后一行");
        assert!(
            !text.lines().any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "行尾填充不该被选中，实际：{text:?}"
        );
    }

    #[test]
    fn size_packing_round_trips() {        let cell = CellSize {
            width: 9.4,
            height: 17.6,
        };
        let packed = pack_size(100, 30, cell);
        let size = WindowSize {
            num_cols: (packed >> 48) as u16,
            num_lines: (packed >> 32) as u16,
            cell_width: (packed >> 16) as u16,
            cell_height: packed as u16,
        };
        assert_eq!((size.num_cols, size.num_lines), (100, 30));
        assert_eq!((size.cell_width, size.cell_height), (9, 18));
        assert_eq!(
            unpack_cell(packed),
            CellSize {
                width: 9.,
                height: 18.
            }
        );
    }

    /// 零尺寸会被 `Term` 里的 `columns - 1` 打穿（`usize` 下溢），必须挡住。
    #[test]
    fn zero_sized_resize_is_ignored() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        let before = session.grid_size();
        session.resize(0, 10, CellSize::default());
        session.resize(10, 0, CellSize::default());
        assert_eq!(session.grid_size(), before);
    }

    /// UI 侧要把 `Arc<Session>` 交给 `cx.spawn` 的异步循环（那里要求 `Send + Sync`）。
    /// 这条断言把「哪天有人往 `Session` 里塞进一个 `Rc` / 裸 `mpsc` 端」挡在编译期。
    #[test]
    fn session_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Session>();
        assert_send_sync::<SessionEvent>();
        assert_send_sync::<SpawnOptions>();
    }

    /// 括号粘贴：程序开启 `CSI ?2004 h` 后，粘贴的多行文本必须包上 `200~` / `201~`
    /// 标记，且换行统一成 CR —— 否则 shell 会把粘贴内容逐行执行、vim 缩进会叠层。
    #[test]
    fn paste_wraps_input_when_bracketed_paste_mode_is_on() {
        let session = Session::spawn(SpawnOptions {
            // 先开 2004 模式；`stty -icrnl` 关掉行规程的 CR→NL 翻译（不然写入的 CR
            // 在半路就变成 NL，测试分不出「发了 CR」还是「发了 LF」）；`cat -v` 把
            // 收到的控制字节显示成可见记号（ESC → `^[`、CR → `^M`）。
            shell: Some((
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "printf '\\033[?2004h'; stty -icrnl; cat -v; sleep 5".to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        std::thread::sleep(Duration::from_millis(300));
        session.paste("line-one\nline-two");

        assert!(
            wait_for_text(
                &session,
                "^[[200~line-one^Mline-two^[[201~",
                Duration::from_secs(10)
            ),
            "括号粘贴模式下粘贴应带 200~/201~ 标记，实际内容：{:?}",
            grid_text(&session)
        );
    }

    /// 没开括号粘贴时粘贴是普通写入：不带标记，换行仍统一成 CR。
    #[test]
    fn paste_without_bracketed_mode_writes_plain_bytes() {
        let session = Session::spawn(SpawnOptions {
            // `stty -icrnl`：不关的话行规程把 CR 翻成 NL，测试分不出写的是哪种。
            shell: Some((
                "/bin/sh".to_owned(),
                vec!["-c".to_owned(), "stty -icrnl; cat -v; sleep 5".to_owned()],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");

        std::thread::sleep(Duration::from_millis(300));
        session.paste("plain\r\npaste\n");

        assert!(
            wait_for_text(&session, "plain^Mpaste^M", Duration::from_secs(10)),
            "普通模式粘贴应原样写入（换行成 CR），实际内容：{:?}",
            grid_text(&session)
        );
    }
}
