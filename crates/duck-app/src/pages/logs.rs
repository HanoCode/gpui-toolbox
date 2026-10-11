//! 日志查看器：列出日志目录里的文件，读选中那个的**末尾**，可按级别与关键字过滤、导出。
//!
//! # 一个视图，两处宿主
//!
//! 它既是独立窗口（托盘 / 帮助菜单开出来的），也是「设置 → 日志」分类页的内容。
//! 两处共用同一个 [`LogView`]，差别只有**水平内边距**：独立窗口按自己的 chrome 留白
//! （`spacing.lg`），设置页里对齐设置内容区的 `CONTENT_PADDING_LEFT/RIGHT`。
//! 工具条、正文、状态栏各自的内边距若不一致，嵌进去就会看出错位的左缘。
//!
//! # 只看尾巴，而且是有理由的看尾巴
//!
//! 日志按天滚动，一个文件可以长到几十上百 MB。全量读进来再交给文本布局，是让
//! 一个「看一下最近发生了什么」的动作去换一次几十 MB 的分配和一次几十万行的排版。
//!
//! 所以这里读的是文件末尾：先按**字节**掐（[`TAIL_BYTES`]），再按**行数**兜一层
//! （[`TAIL_LINES`]）。只要动过刀子，界面上就说清楚——「文件还有更早的内容没载入」
//! 比默默少显示一半要诚实。
//!
//! # 过滤是视图层的，不动读进来的东西
//!
//! 级别与关键字过滤只改**显示什么**，`text` 里那份尾巴始终是原样的。这有两个好处：
//! 清掉过滤条件就能立刻回到全貌（不必重读文件），而「导出」要导的是**过滤后**的东西
//! ——用户刚把范围收窄到想看的那几行，再导一份几万行的完整文件回去，等于白过滤了。
//!
//! 过滤结果算一次就存起来（[`LogView::refilter`]），**不在 `render` 里算**：过滤要
//! 遍历整段尾巴的两千行，而 `render` 会因为鼠标悬停之类的原因随时被调到。
//!
//! # 从中间读会踩到 UTF-8 的码点
//!
//! 按字节偏移定位一定会有落在多字节字符中间的时候。所以**先丢首行再解码**：首行
//! 本来就是半截日志，丢掉它既解决了截断字符，也让界面从一条完整的记录开始。
//!
//! # 自动跟随为什么是轮询
//!
//! 写日志的是本进程自己的 `RollingWriter`，它没有订阅机制，也刻意不引
//! `tracing-appender`（见 `duck_platform::logging`）。所以「文件长了没有」只能自己看。
//! 代价被压到最小：比的是**文件长度**，长度没变就连读都不读、连重绘都不重绘。

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use duck_components::{
    ButtonVariant, DuckIcon, button, content, dropdown_select, meta_text, switch_control,
};
use duck_shell::AuxiliaryWindowView;
use duck_theme::{Lang, active_theme, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Task, Window, div, px, size,
};
use gpui_component::{
    Icon, Root, Sizable as _, Size, h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    v_flex,
};

use crate::app::Desktop;
use crate::pages::settings::{CONTENT_PADDING_LEFT, CONTENT_PADDING_RIGHT};
use crate::pages::status_support::format_bytes;
use crate::settings::{self, AuxWindow};

/// 日志窗口初始尺寸。
const LOGS_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(940.), px(640.));
/// 日志窗口最小尺寸。
const LOGS_WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(620.), px(400.));

/// 一次最多读文件末尾多少字节。
const TAIL_BYTES: u64 = 512 * 1024;
/// 再按行数兜一层：512 KB 的短行仍然可能有上万行。
const TAIL_LINES: usize = 2_000;

/// 自动跟随的间隔。
///
/// 一秒是「看起来实时」和「一年白跑 3153 万次」之间的折中；而长度守卫让绝大多数的
/// 那一次只是两次 `stat`。
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);

/// 只认这个后缀。日志目录里除了它没有别的东西，但目录是可以被用户放东西进去的。
const LOG_SUFFIX: &str = ".log";

/// 日志正文的字体族。
///
/// 日志是列对齐的文本，非等宽就读不出结构（时间戳宽度、`INFO` / `WARN` 的列位）。
/// `font_family` 只收一个族名，两个平台各自保证存在的那个：macOS 的 Menlo 随系统
/// 分发，Windows 的 Consolas 同理。族名缺失时 gpui 会回落到默认 UI 字体。
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const MONO_FAMILY: &str = "Consolas";

/// 级别下拉里「不过滤」那一项的值。不是合法级别名，所以不会和真级别撞上。
const LEVEL_ALL: &str = "all";

/// 日志级别。变体顺序即严重程度——`derive(Ord)` 给出 `Trace < Debug < Info < Warn < Error`，
/// 而「过滤到某级别」就是「不低于它」，所以这里恰好可以直接用比较运算符。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// 全部级别，由轻到重（下拉的顺序也是这个）。
    const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    /// 级别名。与 `tracing` 写在行首级别列里的那个词完全一致（大写），
    /// 所以它同时充当下拉的稳定值，不必再维护一份映射。
    const fn wire(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// 级别列里的词 → 级别。不是这五个词就给 `None`。
    fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.wire() == token)
    }
}

/// 一行日志的级别；没有级别列的行返回 `None`。
///
/// 认的是**第二个空白分隔的词**：`tracing-subscriber` 默认格式是
/// `时间戳 级别 目标: 正文`，时间戳是 ISO 8601、内部不含空格，所以级别恒定在第二列。
/// 不写成 `line.contains("INFO")`：正文里提到 `INFO` 的行会被误判成级别。
fn line_level(line: &str) -> Option<LogLevel> {
    LogLevel::parse(line.split_whitespace().nth(1)?)
}

/// 一行文本要不要因为关键字被留下。`query` 必须**已经是小写**。
fn line_matches(line: &str, query: &str) -> bool {
    line.to_lowercase().contains(query)
}

/// 按级别与关键字过滤一段文本，返回 `(留下的文本, 总行数, 留下的行数)`。
///
/// 过滤以**事件**为单位，不是以行为单位。`tracing` 的多行事件、`{:#?}` 展开的字段都是
/// 「头一行有级别列、后面若干行没有」的形状，所以事件的边界就是「下一个有级别列的行」。
/// 整条要么留下要么不留，有三个好处：留下的条目永远带着自己的头（不会出现看不出是谁
/// 在报的裸续行）；关键字命中续行时也能把整条捞出来（否则那么长的栈回溯永远搜不到）；
/// 「留下几行」跟「几条日志」对得上。
fn filter_text(text: &str, threshold: Option<LogLevel>, query: &str) -> (String, usize, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![false; lines.len()];

    let mut start = 0;
    while start < lines.len() {
        let mut end = start + 1;
        while end < lines.len() && line_level(lines[end]).is_none() {
            end += 1;
        }
        let event = &lines[start..end];
        // 半截事件（从文件中间读起，第一行就是续行）没有级别列，`is_some_and` 直接判否。
        let level_ok = line_level(event[0])
            .is_some_and(|level| threshold.is_none_or(|threshold| level >= threshold));
        let matches = query.is_empty() || event.iter().any(|line| line_matches(line, query));
        if level_ok && matches {
            keep[start..end].fill(true);
        }
        start = end;
    }

    let kept: Vec<&str> = lines
        .iter()
        .zip(&keep)
        .filter_map(|(line, keep)| keep.then_some(*line))
        .collect();
    (kept.join("\n"), lines.len(), kept.len())
}

/// 日志目录里的一个文件。
struct LogFile {
    path: PathBuf,
    /// 文件名（下拉里显示）。
    name: String,
    bytes: u64,
    /// 文件名里的日期戳；认不出日期时是空串（排到最后）。
    day: String,
    /// 段序号：当前文件是 0，序号越大越老。
    segment: u32,
}

/// 读进来的尾巴。
struct Tail {
    text: String,
    /// 文件还有更早的内容没读进来。
    truncated: bool,
}

/// 水平内边距：同一份内容，两处宿主各有各的留白规约。
struct LogInset {
    left: Pixels,
    right: Pixels,
}

impl LogInset {
    /// 独立窗口：两侧都按 chrome 的 `spacing.lg`。
    fn window(cx: &App) -> Self {
        let lg = active_theme(cx).tokens().spacing.lg;
        Self {
            left: lg,
            right: lg,
        }
    }

    /// 设置页内嵌：对齐设置内容区，正文与工具条才和页标题同一条左缘。
    fn page() -> Self {
        Self {
            left: px(CONTENT_PADDING_LEFT),
            right: px(CONTENT_PADDING_RIGHT),
        }
    }
}

/// 日志查看器的内容视图。
pub struct LogView {
    /// 日志目录。建起来时定下：`log_dir()` 每次调用都在算路径。
    dir: PathBuf,
    /// 水平内边距（见 [`LogInset`]）。
    inset: LogInset,
    /// 目录里的 `.log`，按文件名倒序（最新在前）。
    files: Vec<LogFile>,
    /// 选中项。存**路径**而不是下标：刷新会重排列表。
    selected: Option<PathBuf>,
    /// 读进来的尾巴（**未经过滤**）。
    text: SharedString,
    /// 正文里有几行。
    lines: usize,
    /// 过滤后实际显示的那份（`text` 的视图，见 [`LogView::refilter`]）。
    visible: SharedString,
    /// 过滤后还剩几行。
    visible_lines: usize,
    /// 级别下限；`None` = 不过滤。
    level: Option<LogLevel>,
    /// 关键字框。
    search: Entity<InputState>,
    /// 关键字，**已转小写**。
    query: String,
    /// 关键字变更订阅。**必须留着**——drop 掉监听就没了。
    _search_changed: Subscription,
    /// 搜索框占位文案使用的语言；语言切换后同步刷新（占位只在构建时定型）。
    placeholder_lang: Option<Lang>,
    /// 上一次读是不是只拿到了末尾。
    truncated: bool,
    /// 上一次读的失败原因（技术文案，界面外面再包一句人话）。
    error: Option<String>,
    /// 已读入的文件与当时的长度：长度没变就不重读。
    loaded: Option<(PathBuf, u64)>,
    /// 是否自动跟随。
    auto: bool,
    /// 跟随任务。置 `None` 即取消。
    follow: Option<Task<()>>,
}

impl LogView {
    /// 独立窗口里的查看器。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(LogInset::window(cx), window, cx)
    }

    /// 设置页内嵌的查看器（水平内边距对齐设置内容区）。
    pub fn embedded(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(LogInset::page(), window, cx)
    }

    /// 建视图，落地第一次加载，并打开自动跟随。
    fn build(inset: LogInset, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "过滤日志…")));
        let search_changed = cx.subscribe(&search, |this, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let query = input.read(cx).value().to_lowercase();
            if this.query == query {
                return;
            }
            this.query = query;
            // 重算放在这里而不是 `render` 里：过滤要遍历整段尾巴，而 `render`
            // 会因为鼠标悬停之类的原因随时被调到。
            this.refilter();
            cx.notify();
        });
        let mut this = Self {
            dir: duck_platform::log_dir(),
            inset,
            files: Vec::new(),
            selected: None,
            text: SharedString::default(),
            lines: 0,
            visible: SharedString::default(),
            visible_lines: 0,
            level: None,
            search,
            query: String::new(),
            _search_changed: search_changed,
            placeholder_lang: Some(active_theme(cx).lang()),
            truncated: false,
            error: None,
            loaded: None,
            // 默认跟随：不跟随的话，看日志就得一直手点刷新——那正是这个窗口要解决的问题。
            auto: true,
            follow: None,
        };
        this.refresh();
        this.follow = Some(Self::spawn_follow(cx));
        this
    }

    /// 目录里的文件数与 `.log` 总字节数。
    ///
    /// 给设置页的「当前占用」用：它需要的就是这两个数，而这里**已经**每秒扫过一遍目录，
    /// 让那边再 `read_dir` 一次纯属重复。
    pub(crate) fn summary(&self) -> (usize, u64) {
        (
            self.files.len(),
            self.files.iter().map(|file| file.bytes).sum(),
        )
    }

    /// 有没有开过滤（决定状态栏报「共 N 行」还是「显示 N / M 行」）。
    fn is_filtered(&self) -> bool {
        self.level.is_some() || !self.query.is_empty()
    }

    /// 重算「过滤后显示什么」。
    ///
    /// 只在这三个输入变了时调：尾巴重读了、级别改了、关键字改了。
    fn refilter(&mut self) {
        let (visible, total, kept) = filter_text(&self.text, self.level, &self.query);
        self.visible = visible.into();
        self.lines = total;
        self.visible_lines = kept;
    }

    /// 换一个显示级别。
    fn set_level(&mut self, value: &str, cx: &mut Context<Self>) {
        let level = if value == LEVEL_ALL {
            None
        } else if let Some(level) = LogLevel::parse(value) {
            Some(level)
        } else {
            // 下拉回传的稳定值；理论上不会出现别的，丢掉即可。
            return;
        };
        if self.level == level {
            return;
        }
        self.level = level;
        self.refilter();
        cx.notify();
    }

    /// 把当前视图（**过滤之后的文本**）导出成一个文本文件。
    ///
    /// 导的是眼里看到的那些行，不是整份日志：用户刚用级别 + 关键字把范围收窄，再导一份
    /// 几万行的完整文件回去，等于把过滤白做了。文件名里带上源文件名，几次导出之间才不会
    /// 互相覆盖、事后也认得出是哪个文件的切片。
    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            duck_components::notify::error(tr(cx, "当前视图没有内容可导出。"), window, cx);
            return;
        }
        let source = self
            .selected
            .as_ref()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| String::from("duck-log"));
        let stem = source.strip_suffix(LOG_SUFFIX).unwrap_or(&source);
        // 用户取消对话框：静默返回，这不是错误。
        let Some(path) = duck_platform::save_file(format!("{stem}-view.txt")) else {
            return;
        };
        match std::fs::write(&path, self.visible.as_ref()) {
            Ok(()) => duck_components::notify::success(
                trf(
                    cx,
                    "已导出到 {}（{} 行）",
                    &[&path.display().to_string(), &self.visible_lines.to_string()],
                ),
                window,
                cx,
            ),
            Err(error) => duck_components::notify::error(
                trf(cx, "导出失败：{}", &[&error.to_string()]),
                window,
                cx,
            ),
        }
    }

    /// 语言切换后同步搜索框的占位文案（占位只在构建时定型）。
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = active_theme(cx).lang();
        if self.placeholder_lang == Some(lang) {
            return;
        }
        self.placeholder_lang = Some(lang);
        let placeholder = tr(cx, "过滤日志…");
        self.search.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
    }

    /// 重扫目录、必要时重读，变了就重绘。
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.refresh() {
            cx.notify();
        }
    }

    /// 真正干活的那个；返回有没有**可见的**变化。
    ///
    /// 与 [`Self::reload`] 分开，是为了让自动跟随在「一秒里日志没长」时不必重绘。
    fn refresh(&mut self) -> bool {
        let files = list_logs(&self.dir);
        let mut changed = self.files.len() != files.len()
            || self
                .files
                .iter()
                .zip(&files)
                .any(|(old, new)| old.path != new.path || old.bytes != new.bytes);
        self.files = files;

        // 选中的文件没了（被清掉 / 换日）就落到最新的那个。
        let still_there = self
            .selected
            .as_ref()
            .is_some_and(|path| self.files.iter().any(|file| &file.path == path));
        if !still_there {
            self.selected = self.files.first().map(|file| file.path.clone());
            changed = true;
        }

        let reread = self.read_selected();
        changed || reread
    }

    /// 读选中文件的尾巴；返回有没有变化。
    fn read_selected(&mut self) -> bool {
        let Some(path) = self.selected.clone() else {
            // 目录里一个 `.log` 都没有。清干净，别留着上一个文件的残影。
            let changed = self.loaded.is_some();
            self.text = SharedString::default();
            self.truncated = false;
            self.error = None;
            self.loaded = None;
            self.refilter();
            return changed;
        };

        let length = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        if self
            .loaded
            .as_ref()
            .is_some_and(|(loaded, at)| *loaded == path && *at == length)
        {
            return false;
        }

        match read_tail(&path) {
            Ok(tail) => {
                self.text = tail.text.into();
                self.truncated = tail.truncated;
                self.error = None;
            }
            Err(error) => {
                self.text = SharedString::default();
                self.truncated = false;
                self.error = Some(error.to_string());
            }
        }
        // 过滤在读完**之后**、而且总是跑一遍：换了文件、或者同一条日志被追加进来，
        // 显示的那份都得跟着重算——不能只在过滤条件变化时算。
        self.refilter();
        self.loaded = Some((path, length));
        true
    }

    /// 换一个文件看。
    fn select(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&path) {
            return;
        }
        self.selected = Some(path);
        // 换了文件，长度守卫作废：哪怕新旧文件一样长也必须重读。
        self.loaded = None;
        self.reload(cx);
    }

    /// 手动刷新：**无条件重读**。
    ///
    /// 长度守卫省的是「什么都没变」的那次，而用户点刷新恰恰是在说「我不信你没变」
    /// （文件被截短、权限刚改回来）。所以这里把守卫清掉。
    fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.loaded = None;
        self.reload(cx);
    }

    /// 外部要求重新扫描并重读（刚清理掉一批旧日志时，列表不该还挂着已经没了的文件）。
    ///
    /// 与手动刷新同路：清掉长度守卫，无条件重读。
    pub(crate) fn rescan(&mut self, cx: &mut Context<Self>) {
        self.refresh_now(cx);
    }

    /// 打开 / 关掉自动跟随。
    fn set_auto(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.auto == on {
            return;
        }
        self.auto = on;
        self.follow = on.then(|| Self::spawn_follow(cx));
        if on {
            // 打开就先对齐一次，不然要干等一个间隔。
            self.loaded = None;
            self.reload(cx);
        } else {
            cx.notify();
        }
    }

    /// 每秒看一次文件长没长，直到视图没了或任务被丢弃。
    ///
    /// 句柄是 `Context::spawn` 直接递过来的弱引用：窗口一关，视图随之被释放，
    /// `update` 返回 `Err`，循环自己就结束了。这也是为什么不需要额外的退出信号。
    fn spawn_follow(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(FOLLOW_INTERVAL).await;
                let alive = cx.update(|cx| view.update(cx, |this, cx| this.reload(cx)).is_ok());
                if !alive {
                    return;
                }
            }
        })
    }

    /// 工具栏：上面一行是「文件与跟随」，下面一行是「过滤与导出」。
    ///
    /// 分成两行不是为了好看：文件下拉 + 刷新 + 导出 + 自动刷新 + 去访达 + 级别 + 关键字
    /// 挤在一行的话，窗口收到最小宽度（[`LOGS_WINDOW_MIN`]）时每样都被压扁，而这里
    /// 每一件都有它自己的最小可用宽度。两行各自的内边距与正文同源，左缘才对得齐。
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let hint = theme.extended().colors.hairline;

        v_flex()
            .w_full()
            .flex_none()
            .border_b_1()
            .border_color(hint)
            .child(self.render_file_bar(cx))
            .child(self.render_filter_bar(cx))
    }

    /// 第一行：选文件、刷新、导出、自动跟随、去访达。
    fn render_file_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = active_theme(cx).tokens().clone();
        // 下拉的键是路径（唯一），标签是「文件名 · 体积」。
        let options: Vec<(SharedString, SharedString)> = self
            .files
            .iter()
            .map(|file| {
                (
                    SharedString::from(file.path.to_string_lossy().into_owned()),
                    SharedString::from(format!("{} · {}", file.name, format_bytes(file.bytes))),
                )
            })
            .collect();
        let current = self
            .selected
            .as_ref()
            .map_or_else(SharedString::default, |path| {
                SharedString::from(path.to_string_lossy().into_owned())
            });

        let picker: AnyElement = if self.files.is_empty() {
            meta_text(tr(cx, "没有日志文件"), cx).into_any_element()
        } else {
            dropdown_select(
                "logs-file",
                options,
                current,
                // 下拉回传的是稳定键（路径）；原样收下即可，不必再校验。
                cx.processor(|this, value: SharedString, _window, cx| {
                    this.select(PathBuf::from(value.as_ref()), cx);
                }),
                cx,
            )
        };

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .pl(self.inset.left)
            .pr(self.inset.right)
            .pt(tokens.spacing.sm)
            .pb(tokens.spacing.xs)
            .child(picker)
            .child(
                button("logs-refresh", tr(cx, "刷新"), ButtonVariant::Secondary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_now(cx))),
            )
            .child(
                button("logs-export", tr(cx, "导出"), ButtonVariant::Secondary, cx)
                    // 没有可导出的东西时按钮就应该是灰的——比点下去弹一句「没内容」友好。
                    .disabled(self.visible.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
            )
            .child(div().flex_1())
            .child(meta_text(tr(cx, "自动刷新"), cx))
            .child(switch_control(
                "logs-auto",
                self.auto,
                cx.listener(|this, on: &bool, _, cx| this.set_auto(*on, cx)),
            ))
            .child(
                button(
                    "logs-reveal",
                    tr(cx, "在访达中显示"),
                    ButtonVariant::Secondary,
                    cx,
                )
                // 用自己记下的那个目录，而不是再问一次 `log_dir()`：
                // 两者同一个值，但这里的路径是「这个窗口正在看的」。
                .on_click({
                    let dir = self.dir.clone();
                    move |_, _, _| duck_platform::reveal_in_finder(&dir)
                }),
            )
    }

    /// 第二行：级别下限 + 关键字。两者都不改变读进来的东西，只改变显示什么。
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let chrome = theme.extended().colors;
        let opacity = theme.extended().opacity;
        let surfaces = tokens.colors;
        let icon_size = theme.extended().icon.md;
        let stroke = theme.extended().stroke.thin;
        let focus = self.search.read(cx).focus_handle(cx);
        let hover_tint = chrome.hover_tint;
        let hover_border = surfaces.primary.opacity(opacity.muted);

        let level_options: Vec<(SharedString, SharedString)> = std::iter::once((
            SharedString::from(LEVEL_ALL),
            SharedString::from(tr(cx, "全部级别")),
        ))
        .chain(LogLevel::ALL.into_iter().map(|level| {
            (
                SharedString::from(level.wire()),
                SharedString::from(level.wire()),
            )
        }))
        .collect();
        let level_current = SharedString::from(self.level.map_or(LEVEL_ALL, LogLevel::wire));

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .pl(self.inset.left)
            .pr(self.inset.right)
            .pt(tokens.spacing.xs)
            .pb(tokens.spacing.sm)
            // 关键字框：浅色胶囊 + 放大镜 + 可清空输入框（与插件中心的搜索框同形）。
            .child(
                h_flex()
                    .id("logs-search-box")
                    .track_focus(&focus)
                    .flex_1()
                    .min_w_0()
                    .h(theme.density().control)
                    .pl(tokens.spacing.sm)
                    .pr(tokens.spacing.xs)
                    .gap(tokens.spacing.xs)
                    .items_center()
                    .rounded(tokens.radius.md)
                    .border(stroke)
                    .border_color(chrome.hairline)
                    .bg(surfaces.surface)
                    .cursor_text()
                    // 悬停给强调色描边 + 薄底；聚焦态由输入框自身的光标与描边体现。
                    .hover(move |style| style.border_color(hover_border).bg(hover_tint))
                    .on_click(move |_, window, cx| window.focus(&focus, cx))
                    .child(
                        Icon::new(DuckIcon::Search)
                            .size(icon_size)
                            .text_color(surfaces.muted_foreground),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .with_size(Size::Small)
                                .text_size(tokens.typography.sm.size)
                                .line_height(tokens.typography.sm.line_height)
                                .w_full()
                                .cleanable(true)
                                // 无障碍：输入框自带 `Role::TextInput`，但**没有名字**——
                                // 占位文案不是可访问名，读屏只会念「文本栏」。
                                // 复用占位那句既有文案，不新增待翻译串。
                                .aria_label(tr(cx, "过滤日志…")),
                        ),
                    ),
            )
            .child(dropdown_select(
                "logs-level",
                level_options,
                level_current,
                cx.processor(|this, value: SharedString, _window, cx| {
                    this.set_level(value.as_ref(), cx);
                }),
                cx,
            ))
    }

    /// 正文：整个文件尾巴当一段不换行的文本，两向都能滚。
    fn render_body(&self, cx: &App) -> AnyElement {
        if self.files.is_empty() {
            return content::empty_state(
                DuckIcon::FileText,
                tr(cx, "没有日志文件"),
                Some(tr(cx, "运行一次应用就会在日志目录里留下 .log。").into()),
                None,
                cx,
            )
            .into_any_element();
        }

        // 过滤到一行不剩和「文件本来就是空的」是两回事，空态文案得分开说——
        // 否则用户会以为日志文件坏了。
        if self.visible.is_empty() {
            return content::empty_state(
                DuckIcon::Search,
                tr(cx, "没有匹配的行"),
                Some(tr(cx, "放宽级别或换一个关键字，或清空过滤条件看全部。").into()),
                None,
                cx,
            )
            .into_any_element();
        }

        let tokens = active_theme(cx).tokens();
        div()
            .id("logs-text")
            .flex_1()
            .min_h_0()
            .w_full()
            // 一起给两向滚动条：日志既长又宽，换行读反而更糟。
            .overflow_scrollbar()
            // gpui-component 的 `Scrollable` 把滚动区排成 flex-row，交叉轴默认
            // stretch 会把这个内容块（高度 auto）拉到视口高度——纵向溢出随之
            // 消失，竖向滚轮就再也推不动了（横向是主轴，不受影响）。`self_start`
            // 取消拉伸，高度回到内容本身，纵向滚动恢复。见
            // `tests/logs_scroll_repro.rs` 的变体矩阵。
            .self_start()
            .child(
                div()
                    .whitespace_nowrap()
                    .pl(self.inset.left)
                    .pr(self.inset.right)
                    .py(tokens.spacing.md)
                    .font_family(MONO_FAMILY)
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(tokens.colors.foreground)
                    .child(self.visible.clone()),
            )
            .into_any_element()
    }

    /// 底部状态栏。
    fn render_status(&self, cx: &App) -> impl IntoElement {
        let tokens = active_theme(cx).tokens();
        let muted = tokens.colors.muted_foreground;
        let mut left: Vec<AnyElement> = Vec::new();
        if let Some(file) = self
            .selected
            .as_ref()
            .and_then(|path| self.files.iter().find(|file| &file.path == path))
        {
            left.push(
                meta_text(format!("{} · {}", file.name, format_bytes(file.bytes)), cx)
                    .into_any_element(),
            );
            // 开了过滤就同时报两个数：只说「显示 3 行」会让人以为文件只有 3 行。
            let counts = if self.is_filtered() {
                trf(
                    cx,
                    "显示 {} / {} 行",
                    &[&self.visible_lines.to_string(), &self.lines.to_string()],
                )
            } else {
                trf(cx, "共 {} 行", &[&self.lines.to_string()])
            };
            left.push(meta_text(counts, cx).into_any_element());
        }

        let right: Vec<AnyElement> = match (&self.error, self.truncated) {
            (Some(detail), _) => vec![
                div()
                    .text_color(tokens.colors.destructive)
                    .child(trf(cx, "读取失败：{}", &[detail.as_str()]))
                    .into_any_element(),
            ],
            (None, true) => vec![
                div()
                    .text_color(muted)
                    .child(tr(cx, "只显示文件末尾，更早的内容没有载入。"))
                    .into_any_element(),
            ],
            (None, false) => Vec::new(),
        };

        content::status_bar(left, right, cx)
    }
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = active_theme(cx).tokens().clone();
        self.sync_placeholder(window, cx);
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(self.render_toolbar(cx))
            .child(self.render_body(cx))
            .child(self.render_status(cx))
    }
}

// ── 保留策略 ────────────────────────────────────────────────────────────
//
// 「定期清理」的三条触发路径都走 [`prune_with`]：启动一次、后台每 12 小时一次、
// 设置页的「立即清理」。真正干活的 `prune_logs` 在 `duck_platform::logging` 里
// （不依赖 gpui，所以它的取舍能在纯单测里钉住）；这里只负责「按哪个天数清」和
// 「什么时候清」——后者需要 `duck_store` 与 gpui 的定时器，都属于本层。

/// 日志保留天数在 `kv` 表里的键。
const KEY_LOG_RETENTION_DAYS: &str = "logs_retention_days";

/// 下拉里的全部档位（顺序即展示顺序）。最后一项是「永久保留」。
pub const RETENTION_CHOICES: [u32; 6] = [1, 3, 7, 14, 30, duck_platform::LOG_RETENTION_FOREVER];

/// 后台清理的间隔。
///
/// 日志按天滚动、清理只删超出保留期的旧文件，天生幂等。12 小时远小于「天」这个
/// 分辨率，所以跨天产生的过期文件最多晚半天被收走，代价只是每 12 小时一次 `read_dir`。
const PRUNE_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// 读取用户设置的保留天数；没设置、或值不在档位里时回落到默认（7 天）。
pub fn retention_days(cx: &App) -> u32 {
    duck_store::kv_get(cx, KEY_LOG_RETENTION_DAYS)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|days| RETENTION_CHOICES.contains(days))
        .unwrap_or(duck_platform::LOG_RETENTION_DEFAULT_DAYS)
}

/// 写入保留天数（去抖，异步落盘）。
pub fn set_retention_days(cx: &App, days: u32) {
    duck_store::kv_put(cx, KEY_LOG_RETENTION_DAYS, days.to_string());
}

/// 按给定的保留期清理一次，返回删掉的文件数。
///
/// 单拆出「显式给天数」这一层，是因为 `kv_put` 异步落盘：刚改完设置就立刻按
/// [`retention_days`] 去读，读到的还是旧值。
pub fn prune_with(keep_days: u32) -> usize {
    let removed = duck_platform::prune_logs(&duck_platform::log_dir(), keep_days);
    if !removed.is_empty() {
        tracing::info!(count = removed.len(), keep_days, "pruned old log files");
    }
    removed.len()
}

/// 按当前设置清理一次，返回删掉的文件数。
pub fn prune_now(cx: &App) -> usize {
    prune_with(retention_days(cx))
}

/// 每 [`PRUNE_INTERVAL`] 按当前设置清一次；任务随应用存活，不需要句柄。
///
/// 设置改小了不必等下一次调度：设置页改完会立刻自己清一次（见 `settings.rs`）。
pub fn spawn_pruner(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PRUNE_INTERVAL).await;
            cx.update(|cx| {
                prune_now(cx);
            });
        }
    })
    .detach();
}

/// 打开（或聚焦）日志窗口。
pub fn open_logs(cx: &mut App) {
    cx.defer(open_logs_or_focus);
}
/// 窗口已开着则前置激活，否则新建。
///
/// 路径与设置窗口一致（见 `pages::settings`）：句柄由 [`Desktop`] 持有，
/// `update` 返回 `Err` 说明窗口已经关了，那就当没有重开。
fn open_logs_or_focus(cx: &mut App) {
    if focus_open_logs(cx) {
        return;
    }

    let mut options = duck_shell::auxiliary_window_options(SharedString::from(tr(cx, "日志")));
    // 上次的位置尺寸（已夹回可见区域）；没有存档则主屏居中。
    options.window_bounds = Some(settings::aux_window_bounds(
        cx,
        AuxWindow::Logs,
        LOGS_WINDOW_SIZE,
    ));
    options.window_min_size = Some(LOGS_WINDOW_MIN);

    match cx.open_window(options, |window, cx| {
        let content = cx.new(|cx| LogView::new(window, cx));
        let chrome = cx.new(|cx| {
            // 占位标题：真实文案由 with_title_fn 按语言解析，此处不会被渲染。
            AuxiliaryWindowView::new("Logs", content.into(), cx)
                .with_title_fn(|cx| SharedString::from(tr(cx, "日志")))
                // 几何变化 → 去抖持久化（与主窗口同形）。
                .on_window_bounds_changed(|window, cx| {
                    settings::save_aux_bounds(AuxWindow::Logs, window, cx);
                })
        });
        crate::app::observe_system_appearance(window);
        cx.new(|cx| Root::new(chrome, window, cx))
    }) {
        Ok(handle) => {
            *Desktop::logs_window(cx) = Some(handle);
            // 记下「这个窗口开着」，供下次启动恢复（见「设置 → 行为」的开关）。
            Desktop::remember_aux_open(cx);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("日志窗口", &error, cx),
    }
}

/// 已开着的日志窗口前置激活；没有、或已经关掉了则返回 `false`（顺手清掉死句柄）。
fn focus_open_logs(cx: &mut App) -> bool {
    let Some(handle) = *Desktop::logs_window(cx) else {
        return false;
    };
    if handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok()
    {
        cx.activate(true);
        true
    } else {
        *Desktop::logs_window(cx) = None;
        false
    }
}

/// 目录里的 `.log`，按 `(日期, 段序号)` **倒序**（最新在前）。
///
/// 排序键不能直接拿文件名比：段序号是十进制数字，字典序会把 `.10` 排到 `.9` 前面，
/// 于是同一份日志的旧段跑到新段前头。日期本身是 `YYYY-MM-DD`，字典序即时间序，
/// 所以那一段照旧直接比字符串；段序号才需要按数值升序（当前文件是 0，排最前）。
///
/// 认不出日期的文件（用户自己丢进来的 `session.log`）日期取空串，排在所有按天滚动的
/// 文件之后，彼此按名字排——反正它们之间没有时间关系可言。
///
/// 认得的不只是自己的命名：日志目录里任何 `.log` 都能看。目录读不出来时给空列表，
/// 界面按「没有日志文件」处理。
fn list_logs(dir: &Path) -> Vec<LogFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<LogFile> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if !name.ends_with(LOG_SUFFIX) {
                return None;
            }
            // `file_type` 而不是 `metadata`：目录也能叫 `x.log`，读它会得到 IsADirectory。
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            Some(LogFile {
                day: duck_platform::log_file_stamp(&name).unwrap_or_default(),
                segment: duck_platform::log_file_segment(&name),
                bytes: entry.metadata().map_or(0, |meta| meta.len()),
                path,
                name,
            })
        })
        .collect();
    files.sort_by(|left, right| {
        right
            .day
            .cmp(&left.day)
            .then(left.segment.cmp(&right.segment))
            .then(left.name.cmp(&right.name))
    });
    files
}

/// 读文件末尾。
fn read_tail(path: &Path) -> io::Result<Tail> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    // `take` 是给「读的时候文件还在长」上的保险：不封顶的话这次读没有终点。
    let mut raw = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut raw)?;

    let body = tail_of(&raw, start > 0);
    let (text, dropped_lines) = keep_last_lines(&body, TAIL_LINES);
    Ok(Tail {
        text,
        truncated: start > 0 || dropped_lines,
    })
}

/// 把读到的字节变成可显示的文本。
///
/// `cut` 表示这次是从文件中间开始读的：首行多半只有半截，丢掉它而不是显示一行断掉的
/// 日志。**顺带解决了 UTF-8**——任意字节偏移可能落在码点中间，丢首行之后剩下的部分
/// 一定从字符边界开始；末尾仍有半截的话由有损解码兜底。
///
/// 纯函数，所以这两条性质是单测而不是「看一眼好像对」。
fn tail_of(raw: &[u8], cut: bool) -> String {
    let body = if cut {
        match raw.iter().position(|byte| *byte == b'\n') {
            Some(at) => &raw[at + 1..],
            // 一个换行都没有，说明整段都是半截：没有一行是完整的。
            None => &[][..],
        }
    } else {
        raw
    };
    String::from_utf8_lossy(body).into_owned()
}

/// 只留最后 `max` 行；返回值说明有没有丢掉东西。
fn keep_last_lines(text: &str, max: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return (text.to_owned(), false);
    }
    (lines[lines.len() - max..].join("\n"), true)
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_SUFFIX, LogLevel, TAIL_LINES, filter_text, keep_last_lines, line_level, list_logs,
        read_tail, tail_of,
    };
    use std::io::Write as _;
    use std::path::PathBuf;

    /// 临时目录：带进程号与标签，避免与并发跑的其它测试撞名。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-logs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn a_cut_tail_starts_at_the_next_line() {
        let raw = b"alf-line\nfirst full\nsecond full\n";
        assert_eq!(tail_of(raw, true), "first full\nsecond full\n");
    }

    /// 恰好切在一个码点中间时，剩下的也必须能解码——有损解码是保底。
    #[test]
    fn a_tail_cut_mid_codepoint_still_decodes() {
        // 「中」是 E4 B8 AD；切掉中间那个字节。
        let raw = b"\xe4\xb8AD\nnext line\n";
        let decoded = tail_of(raw, true);
        assert_eq!(decoded, "next line\n");
    }

    #[test]
    fn an_uncut_tail_keeps_everything() {
        let raw = b"only line\n";
        assert_eq!(tail_of(raw, false), "only line\n");
    }

    /// 从中间读、而且整段都没有换行：没有一行是完整的，所以什么都不显示。
    #[test]
    fn a_cut_tail_without_any_newline_shows_nothing() {
        assert_eq!(tail_of(b"half a line", true), "");
    }

    #[test]
    fn only_the_last_lines_survive_the_cap() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(keep_last_lines(text, 2), ("three\nfour".to_owned(), true));
        assert_eq!(
            keep_last_lines(text, 4),
            (text.to_owned(), false),
            "没超上限就原样返回"
        );
        assert_eq!(
            keep_last_lines(text, TAIL_LINES),
            (text.to_owned(), false),
            "远低于上限同样原样返回"
        );
    }

    #[test]
    fn the_directory_listing_is_newest_first_and_only_logs() {
        let dir = temp_dir("list");
        for name in [
            "duck-2026-10-09.log",
            "duck-2026-10-10.10.log",
            "duck-2026-10-10.2.log",
            "duck-2026-10-10.1.log",
            "duck-2026-10-10.log",
            "session.log",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"x").expect("写测试文件");
        }
        std::fs::create_dir(dir.join("trap.log")).expect("建同名目录");

        let names: Vec<String> = list_logs(&dir).into_iter().map(|file| file.name).collect();
        assert_eq!(
            names,
            [
                // 同一天里当前文件排最前，其后是序号**递增**的旧段（`.10` 在 `.2` 后面
                // ——段序号按数值比，不是按名字比）。
                "duck-2026-10-10.log",
                "duck-2026-10-10.1.log",
                "duck-2026-10-10.2.log",
                "duck-2026-10-10.10.log",
                "duck-2026-10-09.log",
                // 认不出日期的排最后。
                "session.log",
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_lists_nothing_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("duck-logs-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(list_logs(&dir).is_empty());
    }

    #[test]
    fn a_small_file_is_read_whole() {
        let dir = temp_dir("whole");
        let path = dir.join(format!("small{LOG_SUFFIX}"));
        std::fs::write(&path, b"alpha\nbeta\n").expect("写测试文件");

        let tail = read_tail(&path).expect("读小文件");
        assert_eq!(tail.text, "alpha\nbeta\n");
        assert_eq!(tail.text.lines().count(), 2);
        assert!(!tail.truncated, "没超过任何上限就不该说被截断");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超过字节上限时：起点落在半行上，所以首行被丢掉，并且必须自报截断。
    #[test]
    fn a_file_over_the_byte_cap_reports_being_truncated() {
        let dir = temp_dir("cap");
        let path = dir.join(format!("big{LOG_SUFFIX}"));
        let mut file = std::fs::File::create(&path).expect("建测试文件");
        // 先写一行填充，再把上限之后的完整行写进去。
        file.write_all(&vec![b'a'; super::TAIL_BYTES as usize + 64])
            .expect("写填充");
        file.write_all(b"\nafter the cut\n").expect("写正文");
        drop(file);

        let tail = read_tail(&path).expect("读大文件");
        assert!(tail.truncated);
        assert_eq!(tail.text, "after the cut\n");
        assert_eq!(tail.text.lines().count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 过滤 ────────────────────────────────────────────────────────────

    /// 真实日志行的形状：`时间戳 级别 目标: 正文`。
    const SAMPLE: &str = "\
2026-10-11T12:00:00.000000Z  INFO duck_app::main: 启动
2026-10-11T12:00:01.000000Z DEBUG duck_app::main: 调试
2026-10-11T12:00:02.000000Z  WARN duck_app::main: 注意
2026-10-11T12:00:03.000000Z ERROR duck_app::main: 出错
    展开的字段：{\"INFO\" 只是正文里的词}
";

    #[test]
    fn the_level_comes_from_the_second_column() {
        assert_eq!(
            line_level("2026-10-11T12:00:00Z  INFO t: x"),
            Some(LogLevel::Info)
        );
        assert_eq!(
            line_level("2026-10-11T12:00:00Z WARN t: x"),
            Some(LogLevel::Warn)
        );
        assert_eq!(line_level("    续行"), None, "多行事件的续行没有级别列");
        assert_eq!(line_level(""), None);
        // 正文里出现级别名不算级别——`contains("INFO")` 会在这里栽跟头。
        assert_eq!(
            line_level("2026-10-11T12:00:00Z DEBUG t: INFO 是三个字母"),
            Some(LogLevel::Debug)
        );
    }

    #[test]
    fn no_filter_keeps_every_line() {
        let (text, total, kept) = filter_text(SAMPLE, None, "");
        assert_eq!(text, SAMPLE.trim_end_matches('\n'));
        assert_eq!((total, kept), (5, 5));
    }

    /// 级别是「不低于所选」：选 WARN 就得同时留下 WARN 与 ERROR。
    #[test]
    fn a_level_threshold_keeps_that_level_and_above() {
        let (text, total, kept) = filter_text(SAMPLE, Some(LogLevel::Warn), "");
        assert_eq!(total, 5);
        assert_eq!(kept, 3, "WARN、ERROR 与跟着 ERROR 的续行");
        assert!(text.contains("注意"));
        assert!(text.contains("出错"));
        assert!(text.contains("展开的字段"), "续行跟着上一条留下");
        assert!(!text.contains("启动"));
        assert!(!text.contains("调试"));
    }

    /// 被过滤掉的那条事件的续行也要跟着走：不能让它黏在别的条目下面。
    #[test]
    fn a_continuation_of_a_dropped_event_is_dropped_too() {
        let text = "\
2026-10-11T12:00:00Z DEBUG t: 第一条
    第一条的续行
2026-10-11T12:00:01Z ERROR t: 第二条
";
        let (kept, _, count) = filter_text(text, Some(LogLevel::Error), "");
        assert_eq!(count, 1);
        assert_eq!(kept, "2026-10-11T12:00:01Z ERROR t: 第二条");
    }

    #[test]
    fn a_keyword_is_matched_case_insensitively_on_the_whole_line() {
        // 传进来的关键字已经是小写（`LogView` 在收下时就转了）。
        let (text, _, kept) = filter_text(SAMPLE, None, "duck_app");
        assert_eq!(kept, 5, "每条都有目标名");
        assert!(text.starts_with("2026-10-11T12:00:00"));

        // 命中续行也算命中：把整条捞出来，而不是只丢出那半截。
        let (text, _, kept) = filter_text(SAMPLE, None, "info");
        assert_eq!(
            kept, 3,
            "INFO 那条 + 续行里提到 INFO 的 ERROR 那条（含它的头）"
        );
        assert!(
            text.contains("出错"),
            "头的正文里没有 info，但它被一起带出来了"
        );
    }

    /// 两级过滤串联：先级别、再关键字，两个条件都得满足。
    #[test]
    fn level_and_keyword_are_applied_together() {
        let (text, _, kept) = filter_text(SAMPLE, Some(LogLevel::Warn), "注意");
        assert_eq!(kept, 1);
        assert_eq!(
            text,
            "2026-10-11T12:00:02.000000Z  WARN duck_app::main: 注意"
        );
    }

    #[test]
    fn filtering_everything_out_yields_an_empty_view() {
        let (text, total, kept) = filter_text(SAMPLE, None, "不存在的关键字");
        assert!(text.is_empty());
        assert_eq!(total, 5, "总行数照旧报，界面才说得清「显示 0 / 5 行」");
        assert_eq!(kept, 0);
    }

    /// 从文件中间读起时，第一行就是半截事件的续行：它没有头，任何级别下都留不下来。
    #[test]
    fn a_leading_orphan_continuation_is_dropped() {
        let (kept, total, count) = filter_text("    半截的续行\n", Some(LogLevel::Trace), "");
        assert!(kept.is_empty());
        assert_eq!((total, count), (1, 0));
    }
}
