//! PTY 会话：起 shell、喂字节、读网格。
//!
//! 一个 [`Session`] 对应一个真实进程组（shell 及其子进程）。它同时是**终端的全部状态**：
//! 网格、滚动位置、选择、光标都在 [`Term`] 里，事件循环线程独占写入，UI 线程按需加锁读。

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{
    Event, EventListener, Notify, ShellMark as TermShellMark, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Handler, Rgb};

use crate::palette::{COLOR_COUNT, Palette};

/// 一次轮询最多取走的事件数：防止某个刷屏进程把 UI 线程卡在事件处理里。
const MAX_EVENTS_PER_DRAIN: usize = 512;

/// 最多保留多少条 shell 集成标记。
///
/// 标记只服务于「屏幕上看得见的那一段」：超过回滚缓冲的行本来就滚到看不见的地方了，
/// 留着一份会随会话时长线性增长的表毫无意义。给一个比常见回滚缓冲（几千行）宽松的
/// 上限，超出就丢最旧的。
const SHELL_MARK_LIMIT: usize = 4096;

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

/// 选区的**粒度**：鼠标按下时由宿主按手势决定（拖拽 / Alt 拖拽 / 双击 / 三击）。
///
/// 宿主只负责「这一次点按是哪种手势」，边界怎么算全部交给内核 ——
/// 宽字符尾格、软换行、回滚区行号这些规则在 `Term` 里已实现并测过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// 逐格（只收被完整覆盖的格子）。
    Simple,
    /// 矩形块选（终端表格 / 列对齐数据）。
    Block,
    /// 按词 / 语义边界（先试括号配对，再向两侧扩到词首词尾）。
    Word,
    /// 整行。
    Line,
}

impl SelectionKind {
    /// 映射到内核的选择类型。
    pub(crate) const fn to_selection_type(self) -> SelectionType {
        match self {
            Self::Simple => SelectionType::Simple,
            Self::Block => SelectionType::Block,
            Self::Word => SelectionType::Semantic,
            Self::Line => SelectionType::Lines,
        }
    }
}

/// 屏幕上一条**点得动**的链接，连同它占的格子。
///
/// 「占哪几格」由内核算：一条链接可能被软换行切成两行，宿主渲染时只按行比对列区间，
/// 不必自己重做回绕判断。同一行上出现两次同一个 URL 时，两处的高亮范围**互不影响** ——
/// 这是逐格算出来的，不是「全屏找同 URI」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermLink {
    /// 点开时交给系统的目标。
    pub target: crate::links::LinkTarget,
    /// 高亮范围：每个网格行一段**闭**列区间（左闭右闭）。跨行回绕时会有多段。
    pub ranges: Vec<(i32, std::ops::RangeInclusive<usize>)>,
}

/// Shell 集成标记（`OSC 133`）。
///
/// `OSC 133` 是 shell 与终端之间的一条**约定**：shell 把每条命令用这几个标记夹起来，
/// 终端因此知道提示符、用户输入与命令输出各自从哪里开始。它靠 shell 侧的一段脚本
/// （bash 的 `PROMPT_COMMAND` / zsh 的 `precmd`+`preexec`，或现成的
/// `shell-integration` 脚本）发出 —— 没装脚本的会话就一条标记都没有，
/// 这与「终端不认识这些标记」是两回事，所以功能缺席时不该报错。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellMark {
    /// `A` —— 提示符开始。
    PromptStart,
    /// `B` —— 提示符结束，用户输入开始。
    PromptEnd,
    /// `C` —— 命令已提交，输出开始。
    OutputStart,
    /// `D;<exit>` —— 命令结束。`None` = shell 没报退出码（不是「成功」）。
    CommandFinished(Option<i32>),
}

/// 一条标记在**滚动不变**坐标系里的位置。
///
/// 网格行号是相对的：每滚一行，所有行号一起减一。标记必须跟着**内容**走，
/// 所以这里存的是锚点（见 `alacritty_terminal::grid::Grid::anchor`），
/// 取用时才按当前的滚动量换算回行号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StoredShellMark {
    anchor: i64,
    mark: ShellMark,
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
    /// shell 集成标记（`OSC 133`）。与 [`Session`] 共享同一份表：标记在**解析那一刻**
    /// 才落位，那发生在事件循环线程上，而读它的是渲染线程。
    shell_marks: Arc<Mutex<VecDeque<StoredShellMark>>>,
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

            // shell 集成标记**不进** `SessionEvent`：它没有「只有 UI 线程才知道答案」的
            // 成分，直接落进共享表即可（表本身就是为这条跨线程通道建的）。
            Event::ShellMark { anchor, mark } => {
                if let Ok(mut marks) = self.shell_marks.lock() {
                    marks.push_back(StoredShellMark {
                        anchor,
                        mark: map_shell_mark(mark),
                    });
                    while marks.len() > SHELL_MARK_LIMIT {
                        marks.pop_front();
                    }
                }
                // 标记常常没有任何可见输出（`D` 就是纯控制序列），所以自己叫醒渲染，
                // 否则「命令跑完 → 标记出现」这一步在界面上要等下一次输出才看得见。
                let _ = self.events.send(SessionEvent::Wakeup);
                return;
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

/// vendored 的标记类型 → 本 crate 的。存在的理由与 [`SessionEvent`] 相同：
/// 上层不该被迫依赖 vendored crate 的名字。
fn map_shell_mark(mark: TermShellMark) -> ShellMark {
    match mark {
        TermShellMark::PromptStart => ShellMark::PromptStart,
        TermShellMark::PromptEnd => ShellMark::PromptEnd,
        TermShellMark::OutputStart => ShellMark::OutputStart,
        TermShellMark::CommandFinished(exit) => ShellMark::CommandFinished(exit),
    }
}

/// 把标记序列折算成「哪些行要画命令标记、以及上一条命令的结果」。
///
/// 规则：**每条提示符开始（`A`）所在的行**出一道标记。颜色取**它之前最近一次**
/// `D` 报出的退出码 —— `Some(0)` 成功、`Some(非零)` 失败、`None` 没结论
///（shell 没报退出码，或这是会话里第一次提示符）。
///
/// 「用掉即清」是刻意的：留着上一次的结论会让**没有 `D` 的提示符**继承别人的结果，
/// 那是一种会骗人的显示。
///
/// `marks` 允许乱序（锚点换算过来后仍可能因截断而缺项），函数内部按行号排一遍。
#[must_use]
pub fn prompt_bars(marks: &[(i32, ShellMark)]) -> Vec<(i32, Option<i32>)> {
    let mut sorted: Vec<(i32, ShellMark)> = marks.to_vec();
    sorted.sort_by_key(|(line, _)| *line);

    let mut bars: Vec<(i32, Option<i32>)> = Vec::new();
    let mut finished: Option<Option<i32>> = None;
    for (line, mark) in sorted {
        match mark {
            ShellMark::CommandFinished(exit) => finished = Some(exit),
            ShellMark::PromptStart => {
                bars.push((line, finished.take().flatten()));
            }
            ShellMark::PromptEnd | ShellMark::OutputStart => {}
        }
    }
    bars
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
    /// shell 集成标记（`OSC 133`），与 [`EventProxy`] 共享同一份表。
    shell_marks: Arc<Mutex<VecDeque<StoredShellMark>>>,
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
        let shell_marks: Arc<Mutex<VecDeque<StoredShellMark>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        let proxy = EventProxy {
            events: events_tx,
            sender: Arc::new(Mutex::new(None)),
            size: Arc::clone(&size),
            palette: Arc::clone(&palette),
            dark: Arc::clone(&dark),
            pending_clipboard: Arc::clone(&pending_clipboard),
            next_request: Arc::new(AtomicU64::new(1)),
            shell_marks: Arc::clone(&shell_marks),
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
            shell_marks,
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

    /// 当前终端模式位（鼠标上报、括号粘贴、备用屏……）。
    ///
    /// 宿主据此决定「这次鼠标事件该转给应用，还是自己做选择」—— 上报模式是
    /// **应用运行时**开的（vim / htop / tmux 一进去就开），只能现问不能缓存。
    /// 拿不到锁时按「空模式」处理（等同于「没要求上报」），让事件退回本地选择：
    /// 鼠标事件是低频的用户输入，一次拿不到锁不值得把 UI 挂住等，更不能把事件丢了。
    #[must_use]
    pub fn term_mode(&self) -> TermMode {
        self.try_with_term(|term| *term.mode())
            .unwrap_or_else(TermMode::empty)
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
        self.select_start_kind(point, side, SelectionKind::Simple);
    }

    /// 按**粒度**开始一次选区（鼠标按下时决定，之后拖拽沿用它）。
    ///
    /// 粒度的差别只在 `Term` 里的 `SelectionType` 上——而真正做边界计算的是
    /// vendored alacritty 的 `Selection::to_range`：
    ///
    /// - [`SelectionKind::Simple`]：逐格，只收「被完整覆盖」的格子（`range_simple`）。
    /// - [`SelectionKind::Block`]：矩形，每行取同一对列号（`range_block`）——
    ///   终端里的表格、列对齐输出按列取用这个。
    /// - [`SelectionKind::Word`]：先试**括号配对**（`bracket_search`），失败再按
    ///   语义边界向两侧扩到词首/词尾（`semantic_search_left/right`）。于是
    ///   「双击取一个字段 / 一段 URL / 一对引号里的内容」是同一个入口。
    /// - [`SelectionKind::Line`]：整行（`line_search_left/right`），三击用。
    ///
    /// ⚠️ 宿主**不要**自己算词边界：宽字符、软换行、回滚区行号这些边界规则
    /// 已在 `Term` 里实现并测过（见本模块选区小节的三条理由）。
    pub fn select_start_kind(&self, point: Point, side: Side, kind: SelectionKind) {
        self.with_term(|term| {
            term.selection = Some(Selection::new(kind.to_selection_type(), point, side));
        });
    }

    /// 拖拽中更新选区末端。没有在选的选区时退化成「开一个新的」——
    /// 鼠标移动可能先于按下事件到达（快速拖拽），丢掉这一下会让拖选断掉。
    pub fn select_update(&self, point: Point, side: Side) {
        self.with_term(|term| match term.selection.as_mut() {
            Some(selection) => {
                selection.update(point, side);
                // —— 整格吸附（拖选「漏字段」的根治点）——
                //
                // vendored alacritty 的 `range_simple` / `range_block` 规则是
                // 「**只有被完整覆盖**的格子才算选中」（`selection.rs` 模块文档原话），
                // 端点格含不含由 `Side` 决定：`Side::Left` 在末端是**排除这一格**、
                // `Side::Right` 在起点是**排除这一格**。
                //
                // 宿主按「指针落在格子的左半还是右半」给 `Side`（`cell_at`），于是
                // 最自然的拖选手势 —— 从字符中间按下、在字符中间松开 —— 会正好
                // 让**两个端点格都被排除**：选 `duck` 只剩中间的 `uc`，选一段路径
                // 会丢掉开头与结尾的关键字符。用户描述的「选不准、容易漏字段」
                // 就是这一条。
                //
                // 终端没有「半个格子高亮」这回事，整格吸附才是符合直觉的语义：
                // **从哪一格按下、在哪一格松开，这两格都算在内**。`include_all()`
                // 正是 alacritty 为这件事留的入口（"Expand selection sides to
                // include all cells"）：它按点的先后把两侧端点摆正，因此左右两个
                // 方向的拖选都成立（往左拖不会又被反过来各减一格）。
                //
                // 不放进 `select_start_kind`：单击（只 start、没有 update）必须仍旧
                // 算「没有选区」，否则一次普通点击会把剪贴板冲成单个字符。
                selection.include_all();
            }
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
            let mut selection =
                Selection::new(SelectionType::Lines, Point::new(top, Column(0)), Side::Left);
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

    /// 某一格上的超链接 URI（应用用 `OSC 8` 标记过的格子才有）。
    ///
    /// `point` 是**网格坐标**（可带负行号 = 回滚区），与 [`Self::select_start`] 同一口径 ——
    /// 宿主把鼠标像素换算成网格行时已经减过 `display_offset`，这里不再动它。
    /// 没有超链接时返回 `None`（普通字符格、或应用压根没用 `OSC 8`）。
    #[must_use]
    pub fn hyperlink_at(&self, point: Point) -> Option<String> {
        self.try_with_term(|term| {
            term.grid()[point]
                .hyperlink()
                .map(|link| link.uri().to_owned())
        })
        .flatten()
    }

    /// 某一格上**点得动**的东西 —— 宿主「⌘ + 点击」的落点判定就靠它。
    ///
    /// 两条来源，优先级从高到低：
    ///
    /// 1. `OSC 8` **显式**标记的格子（`ls --hyperlink`、`gh` 会这么干）。URI 是应用给的，
    ///    照用，不猜；
    /// 2. 屏幕上只是**长得像链接**的一串字符 —— 这才是绝大多数：`cargo` 报的文档地址、
    ///    `git` 打的远端 URL、编译器给的文件路径都不带标记（识别规则见
    ///    [`crate::links`]）。
    ///
    /// 第 2 条只在**逻辑行**（沿软换行拼起来的一整行）里找，因此 `https://…` 被终端
    /// 折成两行时仍能整段点开。`point` 是**网格坐标**（可带负行号 = 回滚区），
    /// 与 [`Self::hyperlink_at`] 同一口径。
    #[must_use]
    pub fn link_at(&self, point: Point) -> Option<TermLink> {
        self.try_with_term(|term| term_link_at(term, point))
            .flatten()
    }

    /// 当前网格里（含回滚区）的 shell 集成标记 —— 已从锚点换算成**网格行号**。
    ///
    /// ⚠️ 参数是**调用方已经加锁拿到的** `term`，不是自己去加锁：换算要用网格当前的
    /// 滚动量（[`Grid::anchor_base`](alacritty_terminal::grid::Grid::anchor_base)），
    /// 而渲染路径本来就是先拿 `Term` 锁、再在里面把整屏读出来的 ——
    /// 这里若再 `try_with_term` 一次，`FairMutex` 上就是自己等自己，整帧卡死。
    ///
    /// 返回的行号与 [`Self::link_at`] 的 `point` 同一口径（可负 = 回滚区），
    /// 因此渲染层可以直接拿它跟 `display_iter` 吐出来的行号比。
    #[must_use]
    pub fn shell_marks_in(&self, term: &Term<EventProxy>) -> Vec<(i32, ShellMark)> {
        let base = term.grid().anchor_base();
        let top = Dimensions::topmost_line(term).0;
        let marks = self.shell_marks.lock().expect("shell marks lock");
        marks
            .iter()
            .filter_map(|entry| {
                let line = entry.anchor.saturating_sub(base);
                // 锚点若落在回滚缓冲之外（配置改过、或被截断过），那一行已经不存在了。
                (line >= i64::from(top))
                    .then(|| i32::try_from(line).ok())
                    .flatten()
                    .map(|line| (line, entry.mark))
            })
            .collect()
    }

    /// 整块网格（含回滚区）里**有没有**命中一段正则 —— **阻塞加锁**版。
    ///
    /// 与 [`Self::search_matches`] 共用同一套正则语义，唯一的差别是**拿锁方式**：
    /// 渲染路径必须非阻塞（宁可这一帧用旧内容），而这里是给「子进程退出后做一次
    /// 终局判定」用的 —— 那种判定**不能因为一次 `try_lock` 失败就把结论丢掉**。
    /// 具体场景见 `duck-app` 里对 ssh 主机密钥告警的识别：漏判的代价是用户
    /// 看不到本该出现的告警。因此此处用 [`Self::with_term`] 直接等锁，
    /// 且只在子进程退出这种一次性时机调用。
    ///
    /// 空串或正则非法时返回 `false`。
    #[must_use]
    pub fn buffer_contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let Ok(mut regex) = RegexSearch::new(needle) else {
            return false;
        };
        self.with_term(|term| {
            let start = Point::new(Dimensions::topmost_line(term), Column(0));
            let end = Point::new(
                Dimensions::bottommost_line(term),
                Dimensions::last_column(term),
            );
            RegexIter::new(start, end, Direction::Right, term, &mut regex)
                .next()
                .is_some()
        })
    }

    /// 在整块网格（含回滚区）里搜一段**区分大小写的正则**，返回每个命中的首末格。
    ///
    /// 返回**网格坐标**（闭区间）而不是视口坐标：视口会随滚动变，高亮跟着内核的坐标走
    /// 才不会「滚一下就偏」。行号可能为负（回滚区），与 [`Self::select_start`] 同一口径。
    ///
    /// 空串、或正则非法（用户正在敲 `[` 之类）时返回空表 —— 搜索框里的中间态不该报错。
    /// 大小写规则沿用 alacritty：串里**有大写就区分大小写**，全小写则不区分。
    #[must_use]
    pub fn search_matches(&self, needle: &str) -> Vec<(Point, Point)> {
        if needle.is_empty() {
            return Vec::new();
        }
        let Ok(mut regex) = RegexSearch::new(needle) else {
            return Vec::new();
        };
        self.try_with_term(|term| {
            let start = Point::new(Dimensions::topmost_line(term), Column(0));
            let end = Point::new(
                Dimensions::bottommost_line(term),
                Dimensions::last_column(term),
            );
            RegexIter::new(start, end, Direction::Right, term, &mut regex)
                .map(|found| (*found.start(), *found.end()))
                .collect()
        })
        .unwrap_or_default()
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

/// 把「某一格」翻成「那格所在的链接」。见 [`Session::link_at`] 的口径说明。
fn term_link_at(term: &Term<EventProxy>, point: Point) -> Option<TermLink> {
    let line = LogicalLine::build(term, point.line);
    // 点中的那一格落在逻辑行的第几个字符上；点到宽字符的尾格时归给它的主格。
    let index = line.index_of(point)?;
    // 1) 应用显式标记的 `OSC 8`：URI 是权威的，按「同一 URI 且相邻」向两侧扩。
    if let Some(link) = line.osc8_link_at(index) {
        return Some(link);
    }
    // 2) 裸文本：只在逻辑行内部找，回绕因此天然被覆盖。
    let span = crate::links::scan(&line.text)
        .into_iter()
        .find(|span| span.start <= index && index < span.end)?;
    Some(TermLink {
        target: span.target,
        ranges: line.ranges_for(span.start, span.end),
    })
}

/// 一条**逻辑行**：沿软换行（`WRAPLINE`）拼起来的一整行，以及每个字符落在哪一格。
///
/// 之所以不逐格判断，是因为「一行里第 30 个字符在第几列」只有把整行拼起来才知道：
/// 宽字符占两列、`OSC 8` 的标记是按格存的，两者都要在这张映射表上对齐。
struct LogicalLine {
    /// 整行文本（宽字符的尾格不占字符，已跳过）。
    text: String,
    /// 第 n 个字符所在的网格格（宽字符只记主格）。
    origins: Vec<Point>,
    /// 宽字符尾格 → 它归属的字符下标。点到尾格要算点中了那个字符。
    spacers: Vec<(Point, usize)>,
    /// 第 n 个字符上的 `OSC 8` URI 在 `uris` 里的下标；没有标记时 `None`。
    uri_of: Vec<Option<usize>>,
    /// 去重后的 URI 表（一行里通常 0~2 个）。
    uris: Vec<String>,
}

impl LogicalLine {
    fn build(term: &Term<EventProxy>, line: Line) -> Self {
        let grid = term.grid();
        let last_col = Dimensions::last_column(term);
        let top = Dimensions::topmost_line(term);
        let bottom = Dimensions::bottommost_line(term);

        // 软换行的续行会把自己标在**上一行最后一格**上（`WRAPLINE`），
        // 因此向前向后各吃一遍就能圈出整条逻辑行。
        let mut first = line;
        while first > top
            && grid[Line(first.0 - 1)][last_col]
                .flags
                .contains(Flags::WRAPLINE)
        {
            first = Line(first.0 - 1);
        }
        let mut last = line;
        while last < bottom && grid[last][last_col].flags.contains(Flags::WRAPLINE) {
            last = Line(last.0 + 1);
        }

        let mut text = String::new();
        let mut origins = Vec::new();
        let mut spacers = Vec::new();
        let mut uri_of = Vec::new();
        let mut uris: Vec<String> = Vec::new();

        for row in first.0..=last.0 {
            let cells = &grid[Line(row)];
            for col in 0..=last_col.0 {
                let origin = Point::new(Line(row), Column(col));
                let cell = &cells[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    spacers.push((origin, origins.len().saturating_sub(1)));
                    continue;
                }
                origins.push(origin);
                text.push(cell.c);
                let id = cell
                    .hyperlink()
                    .map(|link| intern_uri(&mut uris, link.uri()));
                uri_of.push(id);
            }
        }

        Self {
            text,
            origins,
            spacers,
            uri_of,
            uris,
        }
    }

    /// 网格格 → 字符下标。点到宽字符的尾格时归给它的主格。
    fn index_of(&self, point: Point) -> Option<usize> {
        if let Some(index) = self.origins.iter().position(|origin| *origin == point) {
            return Some(index);
        }
        self.spacers
            .iter()
            .find(|(spacer, _)| *spacer == point)
            .map(|(_, index)| *index)
    }

    /// 点中的字符若在 `OSC 8` 标记里，返回整段标记（同 URI 且相邻的字符）。
    fn osc8_link_at(&self, index: usize) -> Option<TermLink> {
        let id = self.uri_of.get(index).copied().flatten()?;
        let mut start = index;
        while start > 0 && self.uri_of[start - 1] == Some(id) {
            start -= 1;
        }
        let mut end = index + 1;
        while end < self.uri_of.len() && self.uri_of[end] == Some(id) {
            end += 1;
        }
        Some(TermLink {
            target: crate::links::LinkTarget::Url(self.uris[id].clone()),
            ranges: self.ranges_for(start, end),
        })
    }

    /// 字符区间 `[start, end)` 折成「按行分段」的列区间表。
    ///
    /// 同一行上连续相邻的列会被合成一段；跨到下一行（或列号不连续，比如软换行处）就另起一段。
    fn ranges_for(&self, start: usize, end: usize) -> Vec<(i32, std::ops::RangeInclusive<usize>)> {
        let mut ranges: Vec<(i32, std::ops::RangeInclusive<usize>)> = Vec::new();
        for origin in &self.origins[start..end] {
            let line = origin.line.0;
            let col = origin.column.0;
            match ranges.last_mut() {
                Some((last_line, range)) if *last_line == line && *range.end() + 1 == col => {
                    *range = *range.start()..=col;
                }
                _ => ranges.push((line, col..=col)),
            }
        }
        ranges
    }
}

/// 把 URI 收进去重表，返回它的下标。
fn intern_uri(uris: &mut Vec<String>, uri: &str) -> usize {
    let existing = uris.iter().position(|known| known == uri);
    if let Some(index) = existing {
        return index;
    }
    uris.push(uri.to_owned());
    uris.len() - 1
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
                vec![
                    "-c".to_owned(),
                    "printf 'duck-select-ok\\n'; sleep 5".to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(
            &session,
            "duck-select-ok",
            Duration::from_secs(10)
        ));

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

    /// 回归：**整格吸附** —— 从字符中间按下、在字符中间松开，两个端点格都要算上。
    ///
    /// 这是「选文本容易漏字段」的回归测试。宿主按「指针在格子的左半还是右半」给
    /// `Side`，于是最自然的拖选手势给出的正是 `start = Right` / `end = Left`；
    /// 而 alacritty 的 `range_simple` 对这个组合是「起点在右半边 → 去掉第一格、
    /// 末端在左半边 → 去掉最后一格」：`duck` 会只剩 `uc`。
    /// `select_update` 里的 `include_all()` 把两侧端点摆正，下面两种手势都必须
    /// 拿到完整文本，且不能多出格子。
    #[test]
    fn dragging_from_mid_cell_to_mid_cell_keeps_both_end_cells() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "printf 'duck-select-ok\\n'; sleep 5".to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(
            &session,
            "duck-select-ok",
            Duration::from_secs(10)
        ));

        // 最「不巧」的组合：按下落在第 0 格的右半边、松开落在第 3 格的左半边。
        session.select_start(Point::new(Line(0), Column(0)), Side::Right);
        session.select_update(Point::new(Line(0), Column(3)), Side::Left);
        assert_eq!(
            session.selection_text().as_deref(),
            Some("duck"),
            "两个端点格都不能被丢掉"
        );

        // 反向拖选（从右往左）同样整格吸附：不能反过来又把两侧各减一格。
        session.select_start(Point::new(Line(0), Column(3)), Side::Left);
        session.select_update(Point::new(Line(0), Column(0)), Side::Right);
        assert_eq!(
            session.selection_text().as_deref(),
            Some("duck"),
            "反向拖选的端点格同样要算上"
        );
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
                vec![
                    "-c".to_owned(),
                    "printf 'duck-select-ok\\n'; sleep 5".to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(
            &session,
            "duck-select-ok",
            Duration::from_secs(10)
        ));

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
            !text
                .lines()
                .any(|line| line.ends_with(' ') || line.ends_with('\t')),
            "行尾填充不该被选中，实际：{text:?}"
        );
    }

    /// `OSC 8` 标记过的格子要能被读出来（宿主据此画下划线、⌘ 点击打开）。
    ///
    /// 这条覆盖的是「终端里点了链接没反应」的另一半原因：不是打开动作坏了，
    /// 而是宿主根本不知道哪一格是链接。
    #[test]
    fn osc8_hyperlink_is_readable_from_the_grid() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    // `OSC 8 ; ; URI ST` … `OSC 8 ; ; ST`（ST 用 `ESC \\`）。
                    "printf '\\033]8;;https://example.com\\033\\\\link\\033]8;;\\033\\\\\\n'; sleep 5"
                        .to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(&session, "link", Duration::from_secs(10)));

        assert_eq!(
            session
                .hyperlink_at(Point::new(Line(0), Column(0)))
                .as_deref(),
            Some("https://example.com"),
            "被 OSC 8 标记的格子应当带回 URI"
        );
        // 普通字符格没有超链接（回归：别把整行都当成链接）。
        assert_eq!(session.hyperlink_at(Point::new(Line(5), Column(0))), None);
    }

    /// 起一个只打印一行、然后挂住的会话。
    fn session_printing(script: String) -> Session {
        Session::spawn(SpawnOptions {
            shell: Some(("/bin/sh".to_owned(), vec!["-c".to_owned(), script])),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来")
    }

    #[test]
    fn a_plain_url_on_screen_is_clickable() {
        // 没有 `OSC 8` 的一串网址：以前按 ⌘ 点上去毫无反应，现在要能命中。
        let session =
            session_printing("printf 'see https://example.com/a for docs\\n'; sleep 5".to_owned());
        assert!(wait_for_text(&session, "for docs", Duration::from_secs(10)));

        let link = session
            .link_at(Point::new(Line(0), Column(6)))
            .expect("点在网址中间应当命中链接");
        assert_eq!(
            link.target,
            crate::links::LinkTarget::Url("https://example.com/a".to_owned())
        );
        assert_eq!(link.ranges, vec![(0, 4..=24)], "高亮正好盖住 URL 那几列");

        // 同一行里不在链接上的格子仍然什么都没有。
        assert_eq!(session.link_at(Point::new(Line(0), Column(0))), None);
    }

    #[test]
    fn a_plain_path_on_screen_is_clickable() {
        let session = session_printing(
            "printf 'error at /tmp/duck/main.rs:12:5 here\\n'; sleep 5".to_owned(),
        );
        assert!(wait_for_text(&session, "here", Duration::from_secs(10)));

        let link = session
            .link_at(Point::new(Line(0), Column(12)))
            .expect("路径也该能点");
        assert_eq!(
            link.target,
            crate::links::LinkTarget::Path("/tmp/duck/main.rs:12:5".to_owned())
        );
    }

    #[test]
    fn an_osc8_mark_wins_over_the_bare_text_scan() {
        // 应用显式标了 URI 时以它为准，不去按肉眼长相猜。
        let session = session_printing(
            "printf '\\033]8;;https://real.example/x\\033\\\\https://made-up.example/y_\\033]8;;\\033\\\\\\n'; sleep 5"
                .to_owned(),
        );
        assert!(wait_for_text(&session, "made-up", Duration::from_secs(10)));

        let link = session
            .link_at(Point::new(Line(0), Column(2)))
            .expect("被标记的格子应当命中");
        assert_eq!(
            link.target,
            crate::links::LinkTarget::Url("https://real.example/x".to_owned())
        );
    }

    #[test]
    fn a_link_wrapped_onto_two_rows_stays_one_link() {
        // 110 个字符的网址在 80 列里必然回绕：两行都得能点，且是同一条链接。
        let long_url = format!("https://example.com/{}", "a".repeat(90));
        let session = session_printing(format!("printf '%s\\n' '{long_url}'; sleep 5"));
        assert!(wait_for_text(&session, "aaaa", Duration::from_secs(10)));

        let start = session
            .link_at(Point::new(Line(0), Column(0)))
            .expect("第一行开头应当命中");
        assert_eq!(
            start.target,
            crate::links::LinkTarget::Url(long_url.clone())
        );
        assert_eq!(start.ranges.len(), 2, "跨行回绕的链接要报两段高亮");

        let tail = session
            .link_at(Point::new(Line(1), Column(5)))
            .expect("续行上也要能点");
        assert_eq!(tail.target, crate::links::LinkTarget::Url(long_url));
    }

    #[test]
    fn only_the_clicked_occurrence_is_highlighted() {
        // 同一行里同一个 URL 出现两次：高亮只盖住点中的那一处。
        let session =
            session_printing("printf 'https://a.io/x then https://a.io/x\\n'; sleep 5".to_owned());
        assert!(wait_for_text(&session, "then", Duration::from_secs(10)));

        let first = session
            .link_at(Point::new(Line(0), Column(2)))
            .expect("第一处应当命中");
        assert_eq!(
            first.ranges,
            vec![(0, 0..=13)],
            "「https://a.io/x」共 14 列"
        );
        let second = session
            .link_at(Point::new(Line(0), Column(22)))
            .expect("第二处应当命中");
        assert_eq!(
            second.ranges,
            vec![(0, 20..=33)],
            "只盖住第二处，不连带第一处"
        );
    }

    #[test]
    fn prose_slashes_are_not_links() {
        let session = session_printing("printf 'and/or TCP/IP 3/4 done\\n'; sleep 5".to_owned());
        assert!(wait_for_text(&session, "done", Duration::from_secs(10)));
        // 整行扫一遍：正文里的斜杠一个都不该被当成路径。
        for column in 0..25 {
            assert_eq!(
                session.link_at(Point::new(Line(0), Column(column))),
                None,
                "第 {column} 列不该被认成链接"
            );
        }
    }

    /// shell 集成标记要走完整条链路：`OSC 133` 字节 → vendored 解析 → 事件代理 → 共享表
    /// → 按当前滚动量换算回行号。这条测试用一个「假 shell 集成」的 printf 顶替真 shell。
    #[test]
    fn shell_marks_survive_the_round_trip() {
        // 四段标记各占一行（每段后面补一个换行，行进才可预期）：
        //   row 0: A（提示符开始）
        //   row 1: B（输入开始）
        //   row 2: C（输出开始）
        //   row 3: D;7（命令结束，退出码 7）
        let session = session_printing(
            "printf '\\033]133;A\\007\\n\\033]133;B\\007\\n\\033]133;C\\007\\n\\033]133;D;7\\007\\n'; sleep 5"
                .to_owned(),
        );
        // 全部都是控制序列，没有可等的正文：直接等到标记齐了。
        let marks = wait_for_marks(&session, 4, Duration::from_secs(10));
        assert_eq!(marks.len(), 4, "四条标记都该到：{marks:?}");
        assert_eq!(marks[0], (0, ShellMark::PromptStart));
        assert_eq!(marks[1], (1, ShellMark::PromptEnd));
        assert_eq!(marks[2], (2, ShellMark::OutputStart));
        assert_eq!(marks[3], (3, ShellMark::CommandFinished(Some(7))));
    }

    /// `D` 不带退出码时是「没结论」，不能当成成功。
    #[test]
    fn a_finish_mark_without_an_exit_code_is_not_a_success() {
        let session =
            session_printing("printf '\\033]133;D\\007\\n\\033]133;A\\007\\n'; sleep 5".to_owned());
        let marks = wait_for_marks(&session, 2, Duration::from_secs(10));
        assert_eq!(marks[0], (0, ShellMark::CommandFinished(None)));
        assert_eq!(marks[1], (1, ShellMark::PromptStart));
        assert_eq!(
            prompt_bars(&marks),
            vec![(1, None)],
            "没报退出码 = 没结论（中性色），不是成功"
        );
    }

    #[test]
    fn prompt_bars_take_the_result_of_the_command_before_them() {
        use ShellMark::{CommandFinished, OutputStart, PromptEnd, PromptStart};
        let marks = vec![
            (0, PromptStart),
            (0, PromptEnd),
            (1, OutputStart),
            (3, CommandFinished(Some(0))),
            (4, PromptStart),
            (4, PromptEnd),
            (5, OutputStart),
            (8, CommandFinished(Some(1))),
            (9, PromptStart),
            // 第三次提示符之前没有 `D`：不该继承上一次的失败结论。
            (11, PromptStart),
        ];
        assert_eq!(
            prompt_bars(&marks),
            vec![(0, None), (4, Some(0)), (9, Some(1)), (11, None)]
        );
    }

    #[test]
    fn prompt_bars_tolerate_unordered_input() {
        use ShellMark::{CommandFinished, PromptStart};
        // 传入顺序乱掉也要给出按行号的稳定结果（截断 / 共享表可能让它不连续）。
        let marks = vec![
            (9, PromptStart),
            (0, PromptStart),
            (3, CommandFinished(Some(2))),
            (4, PromptStart),
        ];
        assert_eq!(
            prompt_bars(&marks),
            vec![(0, None), (4, Some(2)), (9, None)]
        );
    }

    /// 等到共享表里攒够 `count` 条标记，返回按行号排序后的快照。
    fn wait_for_marks(session: &Session, count: usize, timeout: Duration) -> Vec<(i32, ShellMark)> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let marks = session
                .try_with_term(|term| session.shell_marks_in(term))
                .unwrap_or_default();
            if marks.len() >= count {
                return marks;
            }
            if std::time::Instant::now() >= deadline {
                return marks;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn search_matches_finds_every_occurrence() {
        let session = Session::spawn(SpawnOptions {
            shell: Some((
                "/bin/sh".to_owned(),
                vec![
                    "-c".to_owned(),
                    "printf 'alpha beta ALPHA\\n'; sleep 5".to_owned(),
                ],
            )),
            ..SpawnOptions::default()
        })
        .expect("PTY 应当能起来");
        assert!(wait_for_text(&session, "ALPHA", Duration::from_secs(10)));

        let matches = session.search_matches("alpha");
        assert_eq!(matches.len(), 2, "两处命中都要找到，实际：{matches:?}");
        assert_eq!(matches[0].0, Point::new(Line(0), Column(0)));
        assert_eq!(matches[0].1, Point::new(Line(0), Column(4)));
        // 大小写：串里没有大写时按不区分大小写搜（alacritty 的既定规则）。
        assert_eq!(matches[1].0, Point::new(Line(0), Column(11)));

        // 用户正在敲 `[` 时正则非法：不能 panic，也不能留着上一次的结果。
        assert!(session.search_matches("[").is_empty());
        assert!(session.search_matches("").is_empty());
    }

    #[test]
    fn size_packing_round_trips() {
        let cell = CellSize {
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
