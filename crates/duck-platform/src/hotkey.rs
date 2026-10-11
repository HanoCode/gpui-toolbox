//! 全局唤起快捷键：应用不在前台时，也能按一个组合键把它叫出来。
//!
//! 分三层，边界按「谁能测」划：
//!
//! - **解析**（[`Hotkey::parse`] / [`Hotkey::from_key`] / [`Hotkey::pretty`]）：纯数据，
//!   零系统调用，全部单测覆盖（含键表本身）。键表 [`KEYS`] 是**唯一**的键名来源，
//!   所以「键盘事件里的键名」与「注册用的键名」不会各写一份然后分叉。
//! - **登记**（[`bind`]）：真的碰操作系统；失败原样交出，由调用方决定怎么告诉用户。
//! - **投递**（[`start`]）：一条常驻线程阻塞在事件通道上，按下时调用落点回调。
//!   回调跑在**后台线程**——`App` 不是 `Send`，落点自己负责把动作搬回主线程
//!   （本仓的做法是投进 `futures_channel`，与第二实例转发共用同一条路）。
//!
//! # 上游能力
//!
//! 经核实，gpui（gpui-fast）**完全没有**全局热键能力：`grep -i
//! 'hotkey|hot_key|RegisterHotKey|global_shortcut'` 在 `gpui/src/`、`gpui_macos/src/`、
//! `gpui_windows/src/` 零命中。所以这一层靠 `global-hotkey`（Tauri 生态）：
//! Windows 走 `RegisterHotKey`，macOS 走 Carbon `RegisterEventHotKey`。
//! 两条路都**不需要**辅助功能 / 输入监控授权，代价是都必须在**主线程**上创建管理器
//! ——这也是[`bind`]只从主线程调的原因。
//!
//! # 已知限制（写在这里而不是藏起来）
//!
//! - **冲突检测只覆盖「系统拒绝」**。Windows 的 `RegisterHotKey` 在组合被占用时
//!   会失败，我们能如实报出来；macOS 的 Carbon 允许两个进程登记同一个组合
//!   （先登记的得事件），返回值是成功 —— 我们**无法**从 API 看出「被 Raycast 抢了」。
//! - **媒体键与音量键一律不支持**：macOS 上它们走的是另一条路（`CGEventTap`），
//!   要辅助功能授权，而且会从系统手里抢走音量 / 播放控制。宁可拒绝，也不做
//!   「装完发现音量键失灵」的事。
//! - Linux 只有 X11 后端（`global-hotkey` 的 `x11rb`），Wayland 上创建管理器会失败，
//!   表现为[`HotkeyError::Rejected`]。本仓的发布目标只有 macOS 与 Windows（见
//!   `.github/workflows/release.yml`），所以这里不做额外补偿。

use std::fmt;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

// Windows 侧的管理器槽位是线程局部的 `RefCell`（原因见 `MANAGER` 上的说明）。
#[cfg(target_os = "windows")]
use std::cell::RefCell;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// 默认唤起键位（规范串，见 [`Hotkey::spec`]）。
///
/// 平台不同，理由也不同：
/// - macOS 用 `⌥Space`。它**不是**系统默认（`⌘Space` 才是 Spotlight，
///   `⌃Space` 是输入法切换），是最省事的「一个修饰键 + 空格」。
/// - Windows / Linux 不能用 `Alt+Space`：那是系统窗口菜单的保留组合，
///   `RegisterHotKey` 直接拒绝。也不能用 `Ctrl+Space`：中文输入法默认拿它切中英文。
///   退一格用 `Ctrl+Alt+Space`。
///
/// 写成规范形（`Space` 大写）是必须的：注册库的 `Display` 输出 `Space`，
/// 而它只接受大小写不敏感的名字 —— 但本仓把「规范串」当存档格式用，
/// 常量与存档必须是同一个形状（`the_default_spec_round_trips` 守着这件事）。
pub const DEFAULT_SPEC: &str = if cfg!(target_os = "macos") {
    "alt+Space"
} else {
    "control+alt+Space"
};

/// 键表：`(gpui 键名, 注册库键名, 给人看的名字)`。
///
/// gpui 的 `Keystroke::key` 是「键帽上印的字符」的小写形式（`space` / `f1` / `a`），
/// 见 `gpui_macos/src/events.rs::key_from_native`、`gpui_windows/src/events.rs::parse_immutable`；
/// `global-hotkey` 要的是 `Space` / `F1` / `KeyA`。两套名字都得跟，于是都在这里。
///
/// 只列**两端都认**的键：gpui 能报但注册库不认的（`f25`–`f35`、`back`、`forward`、
/// `menu`、媒体键）刻意不入表 —— 想加的人会立刻看到注释，而不是加完发现注册不上。
const KEYS: &[(&str, &str, &str)] = &[
    // 字母。gpui 给单个小写字符；显示用大写。
    ("a", "KeyA", "A"),
    ("b", "KeyB", "B"),
    ("c", "KeyC", "C"),
    ("d", "KeyD", "D"),
    ("e", "KeyE", "E"),
    ("f", "KeyF", "F"),
    ("g", "KeyG", "G"),
    ("h", "KeyH", "H"),
    ("i", "KeyI", "I"),
    ("j", "KeyJ", "J"),
    ("k", "KeyK", "K"),
    ("l", "KeyL", "L"),
    ("m", "KeyM", "M"),
    ("n", "KeyN", "N"),
    ("o", "KeyO", "O"),
    ("p", "KeyP", "P"),
    ("q", "KeyQ", "Q"),
    ("r", "KeyR", "R"),
    ("s", "KeyS", "S"),
    ("t", "KeyT", "T"),
    ("u", "KeyU", "U"),
    ("v", "KeyV", "V"),
    ("w", "KeyW", "W"),
    ("x", "KeyX", "X"),
    ("y", "KeyY", "Y"),
    ("z", "KeyZ", "Z"),
    // 数字。
    ("0", "Digit0", "0"),
    ("1", "Digit1", "1"),
    ("2", "Digit2", "2"),
    ("3", "Digit3", "3"),
    ("4", "Digit4", "4"),
    ("5", "Digit5", "5"),
    ("6", "Digit6", "6"),
    ("7", "Digit7", "7"),
    ("8", "Digit8", "8"),
    ("9", "Digit9", "9"),
    // 标点：gpui 直接给字符本身。
    ("`", "Backquote", "`"),
    ("-", "Minus", "-"),
    ("=", "Equal", "="),
    ("[", "BracketLeft", "["),
    ("]", "BracketRight", "]"),
    ("\\", "Backslash", "\\"),
    (";", "Semicolon", ";"),
    ("'", "Quote", "'"),
    (",", "Comma", ","),
    (".", "Period", "."),
    ("/", "Slash", "/"),
    // 有名键。两者都用小写名，但注册库的名字与 gpui 不完全一致（`up` vs `ArrowUp`）。
    ("space", "Space", "Space"),
    ("tab", "Tab", "Tab"),
    ("enter", "Enter", "Enter"),
    ("backspace", "Backspace", "Backspace"),
    ("delete", "Delete", "Delete"),
    ("insert", "Insert", "Insert"),
    ("escape", "Escape", "Esc"),
    ("up", "ArrowUp", "↑"),
    ("down", "ArrowDown", "↓"),
    ("left", "ArrowLeft", "←"),
    ("right", "ArrowRight", "→"),
    ("home", "Home", "Home"),
    ("end", "End", "End"),
    ("pageup", "PageUp", "PageUp"),
    ("pagedown", "PageDown", "PageDown"),
    // 功能键。到 F24 为止：注册库的表里只到 F24（gpui 能给到 f35）。
    ("f1", "F1", "F1"),
    ("f2", "F2", "F2"),
    ("f3", "F3", "F3"),
    ("f4", "F4", "F4"),
    ("f5", "F5", "F5"),
    ("f6", "F6", "F6"),
    ("f7", "F7", "F7"),
    ("f8", "F8", "F8"),
    ("f9", "F9", "F9"),
    ("f10", "F10", "F10"),
    ("f11", "F11", "F11"),
    ("f12", "F12", "F12"),
    ("f13", "F13", "F13"),
    ("f14", "F14", "F14"),
    ("f15", "F15", "F15"),
    ("f16", "F16", "F16"),
    ("f17", "F17", "F17"),
    ("f18", "F18", "F18"),
    ("f19", "F19", "F19"),
    ("f20", "F20", "F20"),
    ("f21", "F21", "F21"),
    ("f22", "F22", "F22"),
    ("f23", "F23", "F23"),
    ("f24", "F24", "F24"),
];

/// 注册库认的「媒体类」键：播放控制与音量。
///
/// 一律拒绝，原因见模块文档。注意音量键**有**扫描码（macOS 上 `Code::AudioVolumeUp`
/// → `0x48`），所以它们不会掉进「不认识的键」那条分支 —— 必须显式拦。
const MEDIA_KEYS: &[Code] = &[
    Code::MediaPlay,
    Code::MediaPause,
    Code::MediaPlayPause,
    Code::MediaStop,
    Code::MediaTrackNext,
    Code::MediaTrackPrevious,
    Code::MediaFastForward,
    Code::MediaRewind,
    Code::AudioVolumeUp,
    Code::AudioVolumeDown,
    Code::AudioVolumeMute,
];

/// 修饰键状态。
///
/// 与 gpui 的 `Modifiers` 字段一一对应，但**不依赖 gpui**：本 crate 的边界是
/// 「不依赖 UI 层」，键盘事件到这里的搬运由 `duck-app` 做（逐字段搬，一眼能看出
/// 有没有漏字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState {
    /// Ctrl。
    pub control: bool,
    /// Alt / Option。
    pub alt: bool,
    /// Shift。
    pub shift: bool,
    /// 平台主修饰键：macOS 的 ⌘、Windows 的 Win。
    pub platform: bool,
    /// Fn。注册库没有对应位，带它的组合一律拒绝。
    pub function: bool,
}

impl ModifierState {
    /// 一个「真修饰键」都没按。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !(self.control || self.alt || self.shift || self.platform || self.function)
    }
}

/// 唤起键位相关的失败原因。
///
/// `Display` 是英文（日志约定）；界面文案由调用方用 `tr` 组织，见
/// [`crate::LaunchAtLoginError`] 的同款处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// 串为空，或格式不对（修饰键必须在主键之前、只能有一个主键）。
    Malformed(String),
    /// 只按了修饰键，没有主键。
    ModifierOnly,
    /// 这个键不支持（媒体键 / 音量键 / gpui 认得出而注册库认不出的键 / Fn）。
    UnsupportedKey(String),
    /// 至少要有 ⌥ / ⌃ / ⌘ / Win 之一。只带 Shift 的全局热键会吃掉日常打字。
    NeedsModifier,
    /// 系统拒绝登记。**多半是组合已被占用**。
    Rejected(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(spec) => write!(f, "unusable hotkey spec \"{spec}\""),
            Self::ModifierOnly => {
                f.write_str("only modifiers were pressed; a real key is required")
            }
            Self::UnsupportedKey(key) => write!(f, "the key \"{key}\" cannot be used as a hotkey"),
            Self::NeedsModifier => {
                f.write_str("the hotkey needs at least one of alt, control or the platform key")
            }
            Self::Rejected(reason) => write!(f, "the system refused the hotkey: {reason}"),
        }
    }
}

impl std::error::Error for HotkeyError {}

/// 一个已解析、已校验的唤起键位。
///
/// 内部就是 [`HotKey`]，但**对外不暴露**：调用方只碰「规范串」与「显示串」两个字符串，
/// 于是存取（`kv` 里存的就是规范串）与展示都不会各自跑偏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    /// 规范串：注册库 `HotKey::into_string()` 的形状，如 `alt+Space`。
    ///
    /// 选它而不是自造格式，是因为它**自带往返保证**：解析出来再打印回去是同一个串
    /// （`"opt+space"` 会被归一成 `"alt+Space"`），所以「存档里写的是什么」与
    /// 「下次读到的是什么」不会有第二个真相。
    spec: String,
    inner: HotKey,
}

impl Hotkey {
    /// 解析一个键位串。
    ///
    /// 接受注册库的全套别名（`alt` / `option`、`cmd` / `command` / `super`、
    /// `ctrl` / `control` —— 注意**没有** `opt` 这个缩写，库的别名表里只有
    /// `option` 与 `alt`），输出统一归一化。校验见 [`Hotkey`] 的各条错误。
    pub fn parse(spec: &str) -> Result<Self, HotkeyError> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err(HotkeyError::Malformed(spec.to_owned()));
        }
        // 库的 `parse_hotkey` 只把最后一个 token 当主键，且不允许主键之后再出现修饰键。
        // 「只按了修饰键」在库里也走不通（它会把 `shift` 当**键名**去查表，查不到 → 报错），
        // 所以那条分支在解析侧是 `Malformed`；带人味的判据在 [`Self::from_key`]。
        let inner =
            HotKey::from_str(trimmed).map_err(|_| HotkeyError::Malformed(spec.to_owned()))?;
        if MEDIA_KEYS.contains(&inner.key) {
            return Err(HotkeyError::UnsupportedKey(inner.key.to_string()));
        }
        if !has_real_modifier(inner.mods) {
            return Err(HotkeyError::NeedsModifier);
        }
        Ok(Self {
            spec: inner.into_string(),
            inner,
        })
    }

    /// 从一次键盘事件构造：`key` 是 gpui 的 `Keystroke::key`，`modifiers` 是当时的修饰键。
    ///
    /// 拼出规范串之后**走 [`Self::parse`]**，不另写一套校验：校验规则只有一份，
    /// 也就不会出现「捕获时说能用、重启后读档说不能用」。
    pub fn from_key(key: &str, modifiers: ModifierState) -> Result<Self, HotkeyError> {
        if modifiers.function {
            return Err(HotkeyError::UnsupportedKey("fn".to_owned()));
        }
        let name = key.to_ascii_lowercase();
        if is_modifier_key_name(&name) {
            return Err(HotkeyError::ModifierOnly);
        }
        let token = KEYS
            .iter()
            .find(|(gpui_name, _, _)| *gpui_name == name)
            .map(|(_, token, _)| *token)
            .ok_or_else(|| HotkeyError::UnsupportedKey(key.to_owned()))?;
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if modifiers.shift {
            parts.push("shift");
        }
        if modifiers.control {
            parts.push("control");
        }
        if modifiers.alt {
            parts.push("alt");
        }
        if modifiers.platform {
            parts.push("super");
        }
        parts.push(token);
        Self::parse(&parts.join("+"))
    }

    /// 规范串 —— 存 `kv`、写日志用的就是它。
    #[must_use]
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// 给人看的串：macOS 用 `⌥Space`，其余平台用 `Alt+Space`。
    ///
    /// 修饰键顺序照 Apple 的习惯（`⌃⌥⇧⌘`），因为那是 macOS 用户唯一认得的顺序。
    #[must_use]
    pub fn pretty(&self) -> String {
        let mods = self.inner.mods;
        let mut out = String::new();
        append_modifier(&mut out, mods.contains(Modifiers::CONTROL), '⌃', "Ctrl+");
        append_modifier(&mut out, mods.contains(Modifiers::ALT), '⌥', "Alt+");
        append_modifier(&mut out, mods.contains(Modifiers::SHIFT), '⇧', "Shift+");
        append_modifier(&mut out, mods.contains(Modifiers::SUPER), '⌘', "Win+");
        out.push_str(&label_of(self.inner.key));
        out
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pretty())
    }
}

/// 把键位登记到操作系统，并在**首次**调用时起一条投递线程。
///
/// `on_press` 是「热键按下时做什么」，**只在第一次调用时生效**：进程内的服务与落点
/// 都只有一处（重复调用会记一条 warn，而不是悄悄换成新的）。它跑在后台线程上，见模块文档。
///
/// 返回 `false` 表示这条服务已经起过了。
pub fn start(on_press: impl Fn() + Send + Sync + 'static) -> bool {
    if SHARED.get().is_some() {
        tracing::warn!("the global hotkey service is already running; the new handler is ignored");
        return false;
    }
    let shared = Arc::new(Shared {
        active_id: Mutex::new(active_spec_id()),
        on_press: Box::new(on_press),
    });
    if SHARED.set(shared.clone()).is_err() {
        return false;
    }
    spawn_listener(shared);
    true
}

/// 登记 / 换绑 / 注销全局唤起键位。`None` 表示注销当前那个。
///
/// 换绑是**先登记新的、成功之后才注销旧的** —— 中间不留「按了没反应」的真空；
/// 反过来一旦新组合被系统拒绝，旧的那份仍然好着（错误原样交回，状态一个字没改）。
///
/// 只在主线程调（macOS 的 Carbon 事件处理器要挂在主线程的 run loop 上）。
pub fn bind(hotkey: Option<&Hotkey>) -> Result<(), HotkeyError> {
    with_manager(|manager| {
        if manager.is_none() {
            match GlobalHotKeyManager::new() {
                Ok(created) => {
                    *manager = Some(Manager {
                        manager: created,
                        active: None,
                    });
                }
                Err(error) => {
                    let reason = HotkeyError::Rejected(error.to_string());
                    tracing::warn!(%error, "failed to create the global hotkey manager");
                    *lock(&LAST_ERROR) = Some(reason.clone());
                    return Err(reason);
                }
            }
        }
        // 上面「空则建」的分支不是建好了就是已经 `return Err`，所以这里逻辑上必然有值。
        // 但仍然不用 `expect`：生产代码不许 panic —— 真撞上（比如将来有人改了上面的控制流）
        // 也只是「这一次没登记上」，不该把整个应用带走。
        let Some(state) = manager.as_mut() else {
            let reason = HotkeyError::Rejected("hotkey manager unavailable".to_owned());
            tracing::warn!("hotkey manager missing right after initialization");
            *lock(&LAST_ERROR) = Some(reason.clone());
            return Err(reason);
        };

        let Some(wanted) = hotkey else {
            if let Some((_, old)) = state.active.take() {
                // 注销失败不该拦住「关掉这个功能」：登记表已经在上面清空了，
                // 最坏的结果是系统里残留一个按不响的登记。
                if let Err(error) = state.manager.unregister(old) {
                    tracing::warn!(%error, "failed to unregister the global hotkey");
                }
            }
            publish(None);
            *lock(&LAST_ERROR) = None;
            return Ok(());
        };

        if state
            .active
            .as_ref()
            .is_some_and(|(spec, _)| spec == wanted.spec())
        {
            // 同一个组合：不重复登记。macOS 上再登记一次会留下一个野 `EventHotKeyRef`
            // （库内部按 id 存表，第二次会覆盖掉第一次的句柄，注销不掉）。
            // 仍然要 `publish`：`bind` 可能先于 `start` 被调到，那时投递侧还没有 `active_id`。
            publish(Some(wanted.inner.id()));
            *lock(&LAST_ERROR) = None;
            return Ok(());
        }

        if let Err(error) = state.manager.register(wanted.inner) {
            let reason = HotkeyError::Rejected(error.to_string());
            tracing::warn!(%error, spec = wanted.spec(), "the system refused the global hotkey");
            *lock(&LAST_ERROR) = Some(reason.clone());
            return Err(reason);
        }
        if let Some((_, old)) = state.active.take()
            && let Err(error) = state.manager.unregister(old)
        {
            tracing::warn!(%error, "failed to unregister the previous global hotkey");
        }
        state.active = Some((wanted.spec.clone(), wanted.inner));
        publish(Some(wanted.inner.id()));
        *lock(&LAST_ERROR) = None;
        tracing::info!(spec = wanted.spec(), "global hotkey registered");
        Ok(())
    })
}

/// 当前登记成功的热键 id；没登记时为 `None`。
fn active_spec_id() -> Option<u32> {
    with_manager(|manager| {
        manager
            .as_ref()
            .and_then(|state| state.active.as_ref().map(|(_, hotkey)| hotkey.id()))
    })
}

/// 把「当前该听哪个 id」告知投递线程。服务没起来时是空操作（`start` 会补一次）。
fn publish(id: Option<u32>) {
    if let Some(shared) = SHARED.get() {
        *lock(&shared.active_id) = id;
    }
}

/// 当前**登记成功**的键位规范串；没登记（关着 / 注册失败 / 平台不支持）时为 `None`。
#[must_use]
pub fn active_spec() -> Option<String> {
    with_manager(|manager| {
        manager
            .as_ref()
            .and_then(|state| state.active.as_ref().map(|(spec, _)| spec.clone()))
    })
}

/// 上一次登记失败的原因；成功一次之后清空。
///
/// 设置页用它显示「未生效：…」——这是 macOS 上唯一能拿到的「冲突」信号（见模块文档）。
#[must_use]
pub fn last_error() -> Option<HotkeyError> {
    lock(&LAST_ERROR).clone()
}

/// 进程内唯一的管理器与当前登记。
struct Manager {
    manager: GlobalHotKeyManager,
    /// `(规范串, 登记用的键位)`。`None` = 当前没登记。
    active: Option<(String, HotKey)>,
}

/// 投递侧：线程按 [`Shared::active_id`] 判断「这条事件是不是我们那个键」。
struct Shared {
    /// 当前该响应的注册 id（`HotKey::id()`）。换绑时更新，线程不必重启。
    active_id: Mutex<Option<u32>>,
    on_press: Box<dyn Fn() + Send + Sync>,
}

/// 管理器。建过之后常驻——`Drop for GlobalHotKeyManager` 会注销并摘掉事件处理器，
/// 而「进程活着但热键没了」比多留一个管理器糟得多。
///
/// # 为什么 Windows 上是线程局部而不是 `static`
///
/// `global-hotkey` 的 **Windows 实现持有窗口句柄（`*mut c_void`），因此
/// `GlobalHotKeyManager` 不是 `Send`**（macOS 实现是）。`static Mutex<Option<Manager>>`
/// 要求 `Manager: Send`，于是在 Windows 上直接编译不过：
///
/// ```text
/// error[E0277]: `*mut c_void` cannot be sent between threads safely
/// ```
///
/// 这里退回线程局部承接同一个契约，而不是 `unsafe impl Send` 去对编译器说谎 —— 管理器
/// 本来就**只在主线程访问**（[`bind`] 的文档已写明这条），线程局部既满足类型系统，
/// 又不需要任何 `unsafe`。非 Windows 平台维持 `static Mutex`：那边是 `Send`，
/// 还能跨线程读。
#[cfg(not(target_os = "windows"))]
static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);
// Windows 侧的管理器槽位（原因见上一条）。`RefCell` 而非 `Mutex`：它只在本线程被借出。
// 这里用行注释而不是文档注释：`thread_local!` 是宏调用，rustdoc 不为它生成文档，
// 挂 `///` 只会得到一条 `unused_doc_comments` 告警。
#[cfg(target_os = "windows")]
thread_local! {
    static MANAGER: RefCell<Option<Manager>> = RefCell::new(None);
}

/// 以统一方式访问管理器槽位；两条后端见 [`MANAGER`]。
///
/// ⚠️ 回调内**不得**再次进入本函数：Windows 那一侧是 `RefCell::borrow_mut`，二次借出会
/// panic。现有调用点（`bind` / `active_spec` / `active_spec_id`）都不递归。
fn with_manager<T>(f: impl FnOnce(&mut Option<Manager>) -> T) -> T {
    #[cfg(not(target_os = "windows"))]
    {
        f(&mut lock(&MANAGER))
    }
    #[cfg(target_os = "windows")]
    {
        MANAGER.with(|slot| f(&mut slot.borrow_mut()))
    }
}

/// 上一次登记失败的原因。
static LAST_ERROR: Mutex<Option<HotkeyError>> = Mutex::new(None);
/// 投递侧的那一半。`OnceLock` 保证「落点只有一处」。
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// 起一条常驻线程，把「按下了」搬给落点。
///
/// 用阻塞式 `iter()` 而不是轮询：空转一条线程去等一个可能几小时才来一次的事件，
/// 在笔记本上就是白白耗电。线程活到进程结束（事件通道是 `static`，永不关闭），
/// 所以不需要 `JoinHandle` —— 失败时只记一条日志，代价是「热键按了没反应」。
fn spawn_listener(shared: Arc<Shared>) {
    let spawned: std::io::Result<JoinHandle<()>> = std::thread::Builder::new()
        .name("duck-hotkey".to_owned())
        .spawn(move || {
            for event in GlobalHotKeyEvent::receiver().iter() {
                // 按下与松开都会来；只认按下。
                if event.state != HotKeyState::Pressed {
                    continue;
                }
                let ours = lock(&shared.active_id).is_some_and(|id| id == event.id);
                if ours {
                    (shared.on_press)();
                }
            }
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "failed to spawn the global hotkey listener");
    }
}

/// 取锁并**忽略中毒**：这里护的都是几个字节的状态，没有跨字段不变量，
/// 一个线程在持锁时 panic 不该让整个功能永久瘫痪（与本仓其它锁的取舍一致）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// gpui 在「只按了修饰键」时会合成一个键名就叫修饰键的事件（见 `window.rs`
/// 的 `dispatch_key_event`），这里认的就是那批名字。
///
/// 这张表只在[`Hotkey::from_key`]用得上：`Keystroke::parse` 那条路走不通
/// ——注册库把 `shift` 当**键名**去查表，查不到就报格式错（见那里的注释）。
fn is_modifier_key_name(name: &str) -> bool {
    matches!(name, "shift" | "control" | "alt" | "platform" | "function")
}

/// 至少要有一个 ⌥ / ⌃ / ⌘ / Win。
///
/// 只管 Shift 的全局热键会吃掉日常打字（`Shift+A` 在任何程序里都是打大写字），
/// 这类组合在实践中总会被用户当成 bug 报回来。
fn has_real_modifier(mods: Modifiers) -> bool {
    mods.intersects(Modifiers::ALT | Modifiers::CONTROL | Modifiers::SUPER | Modifiers::META)
}

/// 追加一个修饰键：macOS 用符号（`⌃⌥⇧⌘`），其余平台用英文缩写。
///
/// 抽成自由函数而不是闭包：闭包会借住 `out`，后面就没法再往里写键名了。
fn append_modifier(out: &mut String, on: bool, mac: char, other: &str) {
    if !on {
        return;
    }
    if cfg!(target_os = "macos") {
        out.push(mac);
    } else {
        out.push_str(other);
    }
}

/// `Code` → 给人看的名字。注册库的 `Display` 与键表第二列**逐字对应**，
/// 所以反查就行，不必再维护第三张表；查不到时退回注册库的名字（以后加了新键也不会瞎显示）。
fn label_of(code: Code) -> String {
    let token = code.to_string();
    KEYS.iter()
        .find(|(_, token_in_table, _)| *token_in_table == token)
        .map_or(token.clone(), |(_, _, label)| (*label).to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        Code, DEFAULT_SPEC, Hotkey, HotkeyError, KEYS, ModifierState, Modifiers, has_real_modifier,
        is_modifier_key_name, label_of,
    };
    use std::collections::HashSet;

    #[test]
    fn the_default_spec_round_trips() {
        let parsed = Hotkey::parse(DEFAULT_SPEC).expect("the default must be registrable");
        assert_eq!(parsed.spec(), DEFAULT_SPEC, "默认串本身就是规范形");
    }

    /// 键表是这个模块的地基：两列都不能重名，否则查找会静默命中错的条目
    /// （`find` 取第一个，后加的永远轮不到）。
    #[test]
    fn the_key_table_has_no_duplicate_names() {
        let mut gpui_names = HashSet::new();
        let mut tokens = HashSet::new();
        let mut labels = HashSet::new();
        for (gpui_name, token, label) in KEYS {
            assert!(
                gpui_names.insert(*gpui_name),
                "重复的 gpui 键名：{gpui_name}"
            );
            assert!(tokens.insert(*token), "重复的注册键名：{token}");
            // 显示名允许重复吗？不允许——`label_of` 反查的是 token，
            // 但两个键显示成同一个名字会让用户没法确认自己录了什么。
            assert!(labels.insert(*label), "重复的显示名：{label}");
            assert!(
                gpui_name.chars().all(|ch| !ch.is_ascii_uppercase()),
                "gpui 键名一律小写：{gpui_name}"
            );
        }
    }

    /// 键表的每个键名都必须能被注册库解析 —— 这张表存在的意义就是「gpui 认、库也认」。
    /// 漏一个会表现为「设置页能录进去、重启后读档失败」。
    ///
    /// 走 [`Hotkey::from_key`] 而不是直接查表，是因为它还带上了「修饰键 → 规范串」
    /// 那一段：`alt+<键>` 必须拼得出合法的规范串，且归一化的结果与表里第二列一致。
    #[test]
    fn every_key_in_the_table_is_accepted_by_the_parser() {
        let alt = ModifierState {
            alt: true,
            ..Default::default()
        };
        let both = ModifierState {
            alt: true,
            shift: true,
            ..Default::default()
        };
        for (gpui_name, token, _) in KEYS {
            let with_alt = Hotkey::from_key(gpui_name, alt)
                .unwrap_or_else(|error| panic!("alt+{gpui_name}（{token}）应当可用：{error}"));
            assert_eq!(with_alt.spec(), format!("alt+{token}"), "{gpui_name}");
            // 再叠一个修饰键走一遍：确保不是靠「恰好一个修饰键」蒙过去的。
            let with_both = Hotkey::from_key(gpui_name, both)
                .unwrap_or_else(|error| panic!("shift+alt+{gpui_name} 应当可用：{error}"));
            assert_eq!(
                with_both.spec(),
                format!("shift+alt+{token}"),
                "{gpui_name}"
            );
        }
    }

    /// 反过来：**没有**真修饰键时，每个键都该被 `NeedsModifier` 拦下 ——
    /// 包括那些单看键名完全合法的。这条与上一条一起把「表里的键都能用、
    /// 但都不会变成吃掉日常按键的裸热键」钉住。
    #[test]
    fn no_key_in_the_table_is_usable_without_a_modifier() {
        for (gpui_name, _, _) in KEYS {
            assert_eq!(
                Hotkey::from_key(gpui_name, ModifierState::default()),
                Err(HotkeyError::NeedsModifier),
                "{gpui_name} 不带修饰键时应当被拒"
            );
        }
    }

    /// 反查表必须对每个键都答得出来（`label_of` 退回注册库名字是兜底，不是常态）。
    #[test]
    fn every_key_in_the_table_has_a_human_label() {
        for (_, token, label) in KEYS {
            let parsed = Hotkey::parse(&format!("alt+{token}")).expect("alt+<token> 应当合法");
            assert!(
                parsed.pretty().ends_with(label),
                "{token} 的显示名应当是 {label}，实得 {}",
                parsed.pretty()
            );
        }
        // 表里没有的键退回注册库的名字，而不是空串。
        assert_eq!(label_of(Code::MediaPlayPause), "MediaPlayPause");
    }

    #[test]
    fn modifiers_are_normalised_and_aliases_are_accepted() {
        let cases = [
            ("option+space", "alt+Space"),
            ("ALT+SPACE", "alt+Space"),
            ("alt+space", "alt+Space"),
            ("alt+Space", "alt+Space"),
            ("cmd+KeyA", "super+KeyA"),
            ("command+KeyA", "super+KeyA"),
            ("ctrl+KeyA", "control+KeyA"),
            // 顺序无关（修饰键之间），但输出只有一个顺序。
            ("shift+control+alt+super+F5", "shift+control+alt+super+F5"),
            ("super+control+alt+shift+F5", "shift+control+alt+super+F5"),
        ];
        for (input, expected) in cases {
            let parsed = Hotkey::parse(input).unwrap_or_else(|error| panic!("{input}: {error}"));
            assert_eq!(parsed.spec(), expected, "输入 {input}");
        }
        // `opt` 不是注册库的别名（它的表里只有 `option` / `alt`）。写下来，
        // 免得以后有人照着 macOS 上的习惯缩写加进来，然后在真机上才发现读档失败。
        assert!(Hotkey::parse("opt+Space").is_err());
    }

    #[test]
    fn a_missing_modifier_is_refused() {
        // 只有 Shift 不算「真修饰键」。
        assert_eq!(Hotkey::parse("shift+KeyA"), Err(HotkeyError::NeedsModifier));
        assert_eq!(Hotkey::parse("F1"), Err(HotkeyError::NeedsModifier));
        assert!(Hotkey::parse("alt+shift+KeyA").is_ok());
    }

    /// 「只按了修饰键」是**捕获侧**的判据：解析侧根本走不到（库把 `shift` 当键名查表，
    /// 查不到就报格式错）。这条把两侧的实际表现都钉下来，省得以后有人以为
    /// `Hotkey::parse("shift")` 会给 `ModifierOnly`。
    #[test]
    fn modifier_only_input_is_told_apart_from_a_bad_key() {
        assert!(
            matches!(Hotkey::parse("shift"), Err(HotkeyError::Malformed(_))),
            "解析侧只报「格式不对」"
        );
        assert_eq!(
            Hotkey::from_key(
                "alt",
                ModifierState {
                    alt: true,
                    ..Default::default()
                }
            ),
            Err(HotkeyError::ModifierOnly)
        );
        assert_eq!(
            Hotkey::from_key("platform", ModifierState::default()),
            Err(HotkeyError::ModifierOnly)
        );
        assert_eq!(
            Hotkey::from_key("nonsense", ModifierState::default()),
            Err(HotkeyError::UnsupportedKey("nonsense".to_owned()))
        );
        assert!(is_modifier_key_name("function"));
        assert!(!is_modifier_key_name("f1"));
    }

    #[test]
    fn empty_and_malformed_specs_are_refused() {
        for spec in ["", "   ", "alt+", "+space"] {
            assert!(
                matches!(Hotkey::parse(spec), Err(HotkeyError::Malformed(_))),
                "{spec:?} 应当被判为格式不对"
            );
        }
    }

    /// 媒体键与音量键一律拒绝（原因见模块文档）：它们在 macOS 上要辅助功能授权，
    /// 而且会把系统音量 / 播放控制抢走。
    #[test]
    fn media_and_volume_keys_are_refused() {
        for spec in [
            "alt+MediaPlayPause",
            "alt+AudioVolumeUp",
            "control+shift+AudioVolumeMute",
        ] {
            assert!(
                matches!(Hotkey::parse(spec), Err(HotkeyError::UnsupportedKey(_))),
                "{spec} 应当被拒"
            );
        }
    }

    /// Fn 在注册库里没有对应位。带它的组合必须明确失败，而不是被静默丢掉修饰键
    /// ——那会注册出一个「没有 Fn 但按了 Fn」的组合。
    #[test]
    fn the_fn_modifier_is_refused_rather_than_dropped() {
        let with_fn = ModifierState {
            alt: true,
            function: true,
            ..Default::default()
        };
        assert_eq!(
            Hotkey::from_key("space", with_fn),
            Err(HotkeyError::UnsupportedKey("fn".to_owned()))
        );
    }

    #[test]
    fn from_key_keeps_every_modifier() {
        let state = ModifierState {
            control: true,
            alt: true,
            shift: true,
            platform: true,
            function: false,
        };
        let built = Hotkey::from_key("space", state).expect("四个修饰键都在表里");
        assert_eq!(built.spec(), "shift+control+alt+super+Space");
        assert_eq!(
            built.pretty(),
            if cfg!(target_os = "macos") {
                "⌃⌥⇧⌘Space"
            } else {
                "Ctrl+Alt+Shift+Win+Space"
            }
        );
    }

    #[test]
    fn from_key_treats_the_key_name_case_insensitively() {
        let state = ModifierState {
            alt: true,
            ..Default::default()
        };
        // gpui 给小写，但两边都收着更稳（写测试 / 以后换平台时不必先猜）。
        assert_eq!(
            Hotkey::from_key("SPACE", state),
            Hotkey::from_key("space", state)
        );
    }

    #[test]
    fn pretty_uses_the_platform_idiom() {
        let hotkey = Hotkey::parse("control+alt+KeyD").expect("合法");
        let expected = if cfg!(target_os = "macos") {
            "⌃⌥D"
        } else {
            "Ctrl+Alt+D"
        };
        assert_eq!(hotkey.pretty(), expected);
        assert_eq!(hotkey.to_string(), expected, "Display 与 pretty 是同一件事");
    }

    /// 规范串必须自带往返：存进 `kv` 的字节再读出来要解析成同一个东西。
    /// 这是「存档用的是规范串」这个决定的全部依据。
    #[test]
    fn the_canonical_spec_is_idempotent() {
        for spec in [
            "alt+Space",
            "shift+control+alt+super+KeyZ",
            "control+alt+F24",
        ] {
            let once = Hotkey::parse(spec).expect("合法");
            let twice = Hotkey::parse(once.spec()).expect("规范串必然还能解析");
            assert_eq!(once, twice);
            assert_eq!(once.spec(), twice.spec());
        }
    }

    /// 修饰键判定本身：只带 Shift / Fn 之类的组合不算「有真修饰键」。
    #[test]
    fn a_real_modifier_is_required() {
        assert!(!has_real_modifier(Modifiers::empty()));
        assert!(!has_real_modifier(Modifiers::SHIFT));
        assert!(has_real_modifier(Modifiers::ALT));
        assert!(has_real_modifier(Modifiers::CONTROL));
        assert!(has_real_modifier(Modifiers::SUPER));
        assert!(has_real_modifier(Modifiers::SHIFT | Modifiers::SUPER));
    }

    /// 失败原因都得能打印出来（日志里只有 `Display`，没有 `Debug` 兜底）。
    #[test]
    fn every_error_explains_itself() {
        let errors = [
            HotkeyError::Malformed("alt+".to_owned()),
            HotkeyError::ModifierOnly,
            HotkeyError::UnsupportedKey("fn".to_owned()),
            HotkeyError::NeedsModifier,
            HotkeyError::Rejected("RegisterHotKey failed".to_owned()),
        ];
        for error in errors {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?} 的 Display 是空的");
            assert!(text.is_ascii(), "{error:?} 的 Display 应当保持英文：{text}");
        }
    }

    /// 没 `start` / 没 `bind` 之前，状态是干净的 —— 这条同时守着「单测不碰操作系统」：
    /// 上面所有用例都只走纯解析，不会真的去注册一个系统热键。
    #[test]
    fn the_service_starts_out_idle() {
        assert!(super::active_spec().is_none());
        assert!(super::last_error().is_none());
    }
}
