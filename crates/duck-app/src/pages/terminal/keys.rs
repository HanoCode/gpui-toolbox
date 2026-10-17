//! 终端页快捷键：**唯一来源**。
//!
//! 右栏的「快捷键」说明与 `app::install` 的绑定表都从[这张表](shortcuts)生成。
//! 两处各写一份的代价已经付过一次：说明里写着「新建标签 ⌘T · 关闭标签 ⌘W ·
//! 分屏 ⌘D」，而这三条一条都没绑 —— 用户照着按，什么也不发生，还以为是应用卡了。
//!
//! 表里的键位串用 `$mod` 写（与 `duck_command` 的声明同一套约定），绑定时交给
//! [`duck_command::keymap::resolve`] 换成本平台的主修饰键；说明里的展示形态由
//! [`Shortcut::display`] 负责（macOS 出 `⌘⇧[`，其他平台出 `Ctrl+Shift+[`）。
//!
//! `show` 那一列是**编辑判断**，不是能力开关：同义的别名（`⇧↑` 与 `⌘↑` 都是翻页）
//! 照样绑定，只是不占说明的一行 —— 说明要能一屏读完才有用。

use gpui::KeyBinding;

use crate::pages::terminal_canvas::{
    Clear, CloseTab, CopyPath, CopySelection, DeleteEntry, DeleteToLineEnd, DeleteToLineStart,
    DeleteWordForward, EditPath, Find, MoveToLineEnd, MoveToLineStart, NewFolderHere, NewLocalTab,
    NewTab, NextTab, PasteFromClipboard, PreviousTab, QuickConnect, RefreshPane, ReopenClosedTab,
    ScrollLineDown, ScrollLineUp, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop,
    SelectAll, SendCtrlC, SendTab, SendTabPrev, SplitPane, SwitchTabAt, ZoomIn, ZoomOut, ZoomReset,
};

/// 键位归属的上下文。
///
/// 两层上下文的理由见 `terminal_canvas` 与 `terminal/mod.rs` 的说明：画布的键是
/// 「发给 PTY 的字节」，页面级的键是「操作界面」。混在一层就得靠 `when` 谓词区分，
/// 而谓词写错的表现是「在终端里按 ⌘L 清了一屏日志」这类极难查的错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyScope {
    /// 终端画布（`"Terminal"`）：按键最终进 PTY，或只动视口 / 字号。
    Canvas,
    /// 整个终端页（`"TerminalPage"`）：标签、查找、文件面板。
    Page,
}

impl KeyScope {
    /// 绑定时用的 `key_context` 名。
    const fn context(self) -> &'static str {
        match self {
            Self::Canvas => "Terminal",
            Self::Page => "TerminalPage",
        }
    }

    /// 说明里的分组标题（i18n 源串）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Canvas => "终端（焦点在画布）",
            Self::Page => "页面（标签 / 文件面板）",
        }
    }
}

/// 一条快捷键要绑的键位。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Keys {
    /// 一条或多条**等价**键位（`$mod` = 平台主修饰键）。
    List(&'static [&'static str]),
    /// `$mod-1` … `$mod-9`：九个键位绑同一个带下标的动作，说明里只占一行。
    TabIndex,
}

/// 一条快捷键：键位 + 说明 + 归属上下文 + **怎么造出绑定**。
///
/// `bind` 存函数指针而不是 `Box<dyn Action>`：`KeyBinding::new` 要求具体类型，
/// 装箱的 `Box<dyn Action>` 并不实现 `Action`。第三个参数是下标（只有
/// [`Keys::TabIndex`] 用得上，其余条目忽略）。
pub(crate) struct Shortcut {
    scope: KeyScope,
    keys: Keys,
    /// 说明文案（i18n 源串）。
    label: &'static str,
    /// 是否出现在右栏的说明里。
    show: bool,
    bind: fn(&str, &'static str, usize) -> KeyBinding,
}

impl Shortcut {
    const fn new(
        scope: KeyScope,
        keys: Keys,
        label: &'static str,
        show: bool,
        bind: fn(&str, &'static str, usize) -> KeyBinding,
    ) -> Self {
        Self {
            scope,
            keys,
            label,
            show,
            bind,
        }
    }

    /// 归属上下文。
    pub(crate) const fn scope(&self) -> KeyScope {
        self.scope
    }

    /// 说明文案。
    pub(crate) const fn label(&self) -> &'static str {
        self.label
    }

    /// 是否出现在说明里。
    pub(crate) const fn shown(&self) -> bool {
        self.show
    }

    /// 说明里显示的键位（平台化：macOS 出符号，其他平台出 `Ctrl+` 这类写法）。
    pub(crate) fn display(&self) -> String {
        match self.keys {
            Keys::List(specs) => specs
                .iter()
                .map(|spec| pretty(spec))
                .collect::<Vec<_>>()
                .join(" / "),
            Keys::TabIndex => {
                let modifier = pretty("$mod");
                format!("{modifier}1 … {modifier}9")
            }
        }
    }

    /// 这条快捷键要注册的全部绑定（[`Keys::TabIndex`] 展开成九条）。
    fn bindings(&self) -> Vec<KeyBinding> {
        let context = self.scope.context();
        match self.keys {
            Keys::List(specs) => specs
                .iter()
                .map(|spec| (self.bind)(&resolve(spec), context, 0))
                .collect(),
            Keys::TabIndex => (0..9)
                .map(|index| {
                    let spec = resolve(&format!("$mod-{}", index + 1));
                    (self.bind)(&spec, context, index)
                })
                .collect(),
        }
    }
}

/// 一条出现在说明里的快捷键。
macro_rules! shown {
    ($scope:expr, [$($spec:expr),+], $label:expr, $action:expr) => {
        Shortcut::new(
            $scope,
            Keys::List(&[$($spec),+]),
            $label,
            true,
            |spec, context, _| KeyBinding::new(spec, $action, Some(context)),
        )
    };
}

/// 一条只绑定、不占说明的别名。
macro_rules! alias {
    ($scope:expr, [$($spec:expr),+], $action:expr) => {
        Shortcut::new(
            $scope,
            Keys::List(&[$($spec),+]),
            "",
            false,
            |spec, context, _| KeyBinding::new(spec, $action, Some(context)),
        )
    };
}

/// 全部快捷键（**绑定与说明共用这一份**，顺序即说明里的顺序）。
pub(crate) fn shortcuts() -> Vec<Shortcut> {
    use KeyScope::{Canvas, Page};
    vec![
        // —— 画布：与 shell 的交互 ——
        shown!(Canvas, ["tab"], "补全（Tab 直接发给 shell）", SendTab),
        alias!(Canvas, ["shift-tab"], SendTabPrev),
        shown!(Canvas, ["ctrl-c"], "中断当前命令", SendCtrlC),
        shown!(Canvas, ["$mod-c"], "复制选中的文字", CopySelection),
        shown!(Canvas, ["$mod-v"], "粘贴", PasteFromClipboard),
        shown!(Canvas, ["$mod-a"], "全选", SelectAll),
        // —— 画布：视口 / 字号 ——
        shown!(Canvas, ["$mod-=", "$mod-+"], "放大字号", ZoomIn),
        shown!(Canvas, ["$mod--"], "缩小字号", ZoomOut),
        shown!(Canvas, ["$mod-0"], "重置字号", ZoomReset),
        shown!(Canvas, ["$mod-k"], "清屏", Clear),
        shown!(Canvas, ["shift-pageup"], "上翻一页", ScrollPageUp),
        shown!(Canvas, ["shift-pagedown"], "下翻一页", ScrollPageDown),
        alias!(Canvas, ["shift-up"], ScrollLineUp),
        alias!(Canvas, ["shift-down"], ScrollLineDown),
        alias!(Canvas, ["$mod-up"], ScrollPageUp),
        alias!(Canvas, ["$mod-down"], ScrollPageDown),
        alias!(Canvas, ["shift-home", "$mod-home"], ScrollToTop),
        alias!(Canvas, ["shift-end", "$mod-end"], ScrollToBottom),
        // —— 画布：行编辑（发控制字节，靠 shell 的行编辑生效）——
        shown!(Canvas, ["$mod-left"], "跳到行首", MoveToLineStart),
        shown!(Canvas, ["$mod-right"], "跳到行尾", MoveToLineEnd),
        alias!(Canvas, ["$mod-backspace"], DeleteToLineStart),
        alias!(Canvas, ["$mod-delete"], DeleteToLineEnd),
        alias!(Canvas, ["alt-delete"], DeleteWordForward),
        shown!(Canvas, ["$mod-d"], "分屏 / 取消分屏", SplitPane),
        // —— 页面：标签 ——
        shown!(Page, ["$mod-t"], "新建标签（同一主机）", NewTab),
        shown!(Page, ["$mod-shift-t"], "新建本地终端", NewLocalTab),
        shown!(
            Page,
            ["$mod-n"],
            "连接（搜索 / 直接连 / 新建）",
            QuickConnect
        ),
        shown!(Page, ["$mod-w"], "关闭标签", CloseTab),
        shown!(Page, ["$mod-shift-z"], "重开已关闭的标签", ReopenClosedTab),
        shown!(Page, ["$mod-shift-]"], "下一个标签", NextTab),
        shown!(Page, ["$mod-shift-["], "上一个标签", PreviousTab),
        alias!(Page, ["ctrl-tab"], NextTab),
        alias!(Page, ["ctrl-shift-tab"], PreviousTab),
        // 下标直达：动作带 `index`，所以不能走 `shown!`（那几个宏要求无参动作）。
        Shortcut::new(
            Page,
            Keys::TabIndex,
            "切到第 N 个标签",
            true,
            |spec, context, index| KeyBinding::new(spec, SwitchTabAt { index }, Some(context)),
        ),
        // —— 页面：查找与文件面板 ——
        shown!(Page, ["$mod-f"], "查找…", Find),
        shown!(Page, ["$mod-l"], "编辑当前路径", EditPath),
        shown!(Page, ["$mod-r"], "刷新当前面板", RefreshPane),
        shown!(Page, ["$mod-shift-n"], "新建远端文件夹", NewFolderHere),
        shown!(Page, ["$mod-backspace"], "删除选中项", DeleteEntry),
        shown!(Page, ["$mod-alt-c"], "复制当前目录路径", CopyPath),
    ]
}

/// 把这张表注册进 gpui 的键位系统（`app::install` 里调一次）。
pub(crate) fn shortcut_bindings() -> Vec<KeyBinding> {
    shortcuts().iter().flat_map(Shortcut::bindings).collect()
}

/// `$mod` → 本平台的主修饰键（`cmd` / `ctrl`）。
fn resolve(spec: &str) -> String {
    spec.replace("$mod", duck_command::primary_modifier())
}

/// 键位串 → 说明里的写法。
///
/// 从**左边**逐段剥修饰键，剩下的整段就是键名：`$mod--` 的键名本身是 `-`，
/// 按 `-` 切开再拼会在「减号」这一条上得到三个空段。
fn pretty(spec: &str) -> String {
    let mut rest = spec;
    let mut modifiers: Vec<&str> = Vec::new();
    'outer: loop {
        for prefix in ["$mod", "shift", "alt", "ctrl"] {
            if let Some(tail) = rest
                .strip_prefix(prefix)
                .and_then(|tail| tail.strip_prefix('-'))
            {
                modifiers.push(prefix);
                rest = tail;
                continue 'outer;
            }
        }
        break;
    }
    let mac = cfg!(target_os = "macos");
    let mut parts: Vec<String> = modifiers
        .iter()
        .map(|modifier| match (*modifier, mac) {
            ("$mod", true) => "⌘".to_owned(),
            ("$mod", false) => "Ctrl".to_owned(),
            ("shift", true) => "⇧".to_owned(),
            ("shift", false) => "Shift".to_owned(),
            ("alt", true) => "⌥".to_owned(),
            ("alt", false) => "Alt".to_owned(),
            ("ctrl", true) => "⌃".to_owned(),
            ("ctrl", false) => "Ctrl".to_owned(),
            _ => String::new(),
        })
        .collect();
    parts.push(key_name(rest, mac));
    // macOS 的惯例是符号连写（`⌘⇧[`）；其他平台是 `Ctrl+Shift+[`。
    if mac { parts.concat() } else { parts.join("+") }
}

/// 键名 → 说明里的写法。
fn key_name(key: &str, mac: bool) -> String {
    let symbol = |mac_symbol: &str, plain: &str| {
        if mac {
            mac_symbol.to_owned()
        } else {
            plain.to_owned()
        }
    };
    match key {
        "pageup" => "PageUp".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        "backspace" => symbol("⌫", "Backspace"),
        "delete" => symbol("⌦", "Delete"),
        "left" => symbol("←", "Left"),
        "right" => symbol("→", "Right"),
        "up" => symbol("↑", "Up"),
        "down" => symbol("↓", "Down"),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "tab" => "Tab".to_owned(),
        // 单字符键（字母 / 数字 / 符号）大写更好读：`⌘C` 而不是 `⌘c`。
        single if single.chars().count() == 1 => single.to_uppercase(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shown_shortcut_has_a_label() {
        // 说明里出现的每一行都必须有话说：空标签在界面上就是一条空白。
        for shortcut in shortcuts().iter().filter(|shortcut| shortcut.shown()) {
            assert!(!shortcut.label().is_empty(), "{}", shortcut.display());
            assert!(!shortcut.display().is_empty());
        }
    }

    #[test]
    fn aliases_are_bound_but_not_listed() {
        // 别名照样要有绑定 —— `show` 只影响说明，不影响能不能按。
        let all = shortcuts();
        let aliases: Vec<_> = all.iter().filter(|shortcut| !shortcut.shown()).collect();
        assert!(!aliases.is_empty(), "表里应当留着若干别名");
        for shortcut in aliases {
            assert!(!shortcut.bindings().is_empty());
        }
    }

    #[test]
    fn bindings_cover_every_scope_and_expand_tab_index() {
        let bindings = shortcut_bindings();
        // 27 条表项里有一条展开成九条；下界取「表项数 + 8」即可钉住展开行为。
        assert!(bindings.len() >= shortcuts().len() + 8);
    }

    #[test]
    fn minus_key_survives_the_symbol_map() {
        // `$mod--` 的键名本身是 `-`：按 `-` 切开再拼会得到空段。
        let expected = if cfg!(target_os = "macos") {
            "⌘-"
        } else {
            "Ctrl+-"
        };
        assert_eq!(pretty("$mod--"), expected);
    }

    #[test]
    fn modifiers_are_spelled_per_platform() {
        let expected = if cfg!(target_os = "macos") {
            "⌘⇧["
        } else {
            "Ctrl+Shift+["
        };
        assert_eq!(pretty("$mod-shift-["), expected);
        let tab = if cfg!(target_os = "macos") {
            "⌃Tab"
        } else {
            "Ctrl+Tab"
        };
        assert_eq!(pretty("ctrl-tab"), tab);
    }
}
