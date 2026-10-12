//! Duck Toolbox 终端仿真内核：本地 PTY + alacritty 终端状态机。
//!
//! # 分层约束
//!
//! 本 crate **不依赖 GPUI**，只做三件事：
//!
//! 1. 用 [`alacritty_terminal`] 的 `tty` 起一个真实 PTY（`pty + fork + exec`），
//!    把子进程的字节流喂给终端状态机 `Term`；
//! 2. 把 `Term` 放进 [`FairMutex`] 让**读写两侧并发**：事件循环线程独占写入，
//!    渲染线程短暂加锁读网格；
//! 3. 把「宿主需要知道的事」（新输出、标题、退出……）转成我们自己的
//!    [`SessionEvent`]，**不让上层直接依赖 vendored crate**。
//!
//! 之所以要第 3 条：`alacritty_terminal` 是 vendored 的（见仓库根 `vendor/README.md`），
//! 升级时它的 `Event` 枚举变一个名字，都只该在本 crate 里改一行，而不是散到 UI 层。
//! 网格类型（`Term` / `Cell` / `Color` / `Point`…）反过来**故意重导出**——渲染层必须
//! 逐格读它，中间再包一层只会让每帧多一次拷贝。
//!
//! # 线程模型
//!
//! ```text
//!   UI 线程                          event-loop 线程
//!   ────────                         ───────────────
//!   session.write(bytes)  ──Msg::Input──▶  drain_recv_channel → pty.write
//!   session.resize(...)   ──Msg::Resize─▶  pty.on_resize / term.resize
//!   session.with_term(..) ──lock──────┐    pty.read → vte 解析 → term 更新
//!                                     └── 同一条 FairMutex ──┘
//!   session.drain_events() ◀──mpsc──  event_proxy.send_event(Event::Wakeup)
//! ```
//!
//! UI 侧**不该**在渲染里阻塞等锁：用 [`Session::try_with_term`] 拿不到就沿用上一帧，
//! 下一帧的 `SessionEvent::Wakeup` 会再叫醒它。

mod keymap;
mod palette;
mod session;

pub use keymap::{Encoded, KeyStroke, encode};
pub use palette::Palette;
pub use session::{CellSize, Session, SessionEvent, SpawnOptions};

// ── 渲染层需要的 alacritty 类型（重导出，避免上层再依赖 vendored crate）──────────

pub use alacritty_terminal::grid::{Dimensions, Grid};
pub use alacritty_terminal::index::{Column, Line, Point, Side};
pub use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::term::color::Colors;
pub use alacritty_terminal::term::{
    RenderableContent, RenderableCursor, Term, TermMode,
};
pub use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
