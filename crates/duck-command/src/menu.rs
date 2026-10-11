//! 原生菜单栏：把注册表投影成 gpui 的 [`Menu`] 树并安装。
//!
//! 分两部分：
//!
//! 1. **标准菜单** —— 应用菜单里的「关于 / 服务 / 隐藏三件套」、以及「编辑」「窗口」。
//!    它们的位置与分组由 macOS 的用户习惯定死（用户靠肌肉记忆找它们），所以在这里
//!    静态搭出来；**不**进动作注册表，否则会同时污染命令面板与活动栏。
//! 2. **注册表派生的菜单**（视图 / 工具 / 帮助 …）—— 新增一个动作不需要改这里，
//!    四个输出面自动跟随。
//!
//! 注：`set_menus` 只在 macOS 上真的画出菜单栏 —— `gpui_windows` 只把它存进
//! `inner.state.menus` 供 `get_menus` 读，`gpui_linux` 根本没实现。所以「编辑」菜单里
//! 的 `OsAction` 选择器（`copy:` / `paste:` …）是按 macOS 的行为设计的。

use std::collections::HashMap;

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType};
use gpui_base::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};

use duck_theme::tr;

use crate::action::{ActionMeta, RunCommand, metas};

/// 应用菜单标题（macOS 上第一个菜单必须是应用名菜单）。
pub const APP_MENU_NAME: &str = "Duck Toolbox";
/// 退出动作 id（由应用登记处理器）。
pub const QUIT_COMMAND: &str = "app.quit";

/// 「关于 <应用>」的动作 id：打开设置的「关于」页。
pub const ABOUT_COMMAND: &str = "app.about";
/// 「隐藏 <应用>」。
pub const HIDE_COMMAND: &str = "window.hide";
/// 「隐藏其他」。
pub const HIDE_OTHERS_COMMAND: &str = "window.hide-others";
/// 「显示全部」。
pub const SHOW_ALL_COMMAND: &str = "window.show-all";
/// 「最小化」。
pub const MINIMIZE_COMMAND: &str = "window.minimize";
/// 「缩放」。
pub const ZOOM_COMMAND: &str = "window.zoom";
/// 「关闭」当前窗口。
pub const CLOSE_COMMAND: &str = "window.close";

/// 按当前语言重建并安装菜单栏。
///
/// 语言切换后需要重新调用（菜单文案在构建时定型）。
/// 顶级菜单名为 [`APP_MENU_NAME`] 的动作会并入应用菜单（macOS 上「偏好设置」的传统位置）。
pub fn refresh_menus(cx: &App) {
    let mut top_order: Vec<&'static str> = Vec::new();
    let mut direct: HashMap<&'static str, Vec<&ActionMeta>> = HashMap::new();
    let mut sub_order: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    let mut sub_items: HashMap<(&'static str, &'static str), Vec<&ActionMeta>> = HashMap::new();

    for meta in metas(cx) {
        let Some((top, sub)) = meta.menu else {
            continue;
        };
        if !top_order.contains(&top) {
            top_order.push(top);
        }
        match sub {
            None => direct.entry(top).or_default().push(meta),
            Some(sub) => {
                let subs = sub_order.entry(top).or_default();
                if !subs.contains(&sub) {
                    subs.push(sub);
                }
                sub_items.entry((top, sub)).or_default().push(meta);
            }
        }
    }

    let mut menus: Vec<Menu> = Vec::new();

    // 应用菜单：先放应用自定的条目（例：设置），再按 macOS 的既定结构补
    // 关于 / 服务 / 隐藏三件套 / 退出。
    let mut app_items: Vec<MenuItem> = direct
        .remove(APP_MENU_NAME)
        .unwrap_or_default()
        .iter()
        .map(|meta| action_item(cx, meta))
        .collect();
    if let Some(subs) = sub_order.remove(APP_MENU_NAME) {
        for sub in subs {
            app_items.push(submenu(cx, sub, sub_items.get(&(APP_MENU_NAME, sub))));
        }
    }
    app_items.push(MenuItem::separator());
    app_items.push(static_item(cx, ABOUT_COMMAND, "关于 Duck Toolbox"));
    app_items.push(MenuItem::separator());
    // 服务：内容由系统填充（macOS 会把自己的 Services 项塞进来），我们只给位置。
    app_items.push(MenuItem::os_submenu(
        tr(cx, "服务"),
        SystemMenuType::Services,
    ));
    app_items.push(MenuItem::separator());
    app_items.push(static_item(cx, HIDE_COMMAND, "隐藏 Duck Toolbox"));
    app_items.push(static_item(cx, HIDE_OTHERS_COMMAND, "隐藏其他"));
    app_items.push(static_item(cx, SHOW_ALL_COMMAND, "显示全部"));
    app_items.push(MenuItem::separator());
    app_items.push(MenuItem::action(
        tr(cx, "退出 Duck Toolbox"),
        command(QUIT_COMMAND),
    ));
    menus.push(Menu::new(APP_MENU_NAME).items(app_items));

    // 编辑：剪贴板四项走 `OsAction` —— macOS 会把它翻译成系统标准选择器
    // （`cut:` / `copy:` / `paste:` / `selectAll:`），由响应链上真正持有文本的元素
    // 处理，我们不需要自己实现剪贴板逻辑。撤消 / 重做没有对应选择器（上游注释说明
    // 它们恒不可用），改为直接派发上游的 `input::Undo` / `input::Redo` 动作。
    menus.push(Menu::new(tr(cx, "编辑")).items(vec![
        MenuItem::action(tr(cx, "撤消"), Undo),
        MenuItem::action(tr(cx, "重做"), Redo),
        MenuItem::separator(),
        MenuItem::os_action(tr(cx, "剪切"), Cut, OsAction::Cut),
        MenuItem::os_action(tr(cx, "拷贝"), Copy, OsAction::Copy),
        MenuItem::os_action(tr(cx, "粘贴"), Paste, OsAction::Paste),
        MenuItem::os_action(tr(cx, "全选"), SelectAll, OsAction::SelectAll),
    ]));

    // 注册表派生的顶级菜单。「帮助」按 macOS 习惯压在最后，所以窗口菜单插在它之前；
    // 没有帮助菜单时窗口菜单就是最后一项。
    let mut help_menus: Vec<Menu> = Vec::new();
    let mut window_added = false;
    for top in top_order
        .iter()
        .copied()
        .filter(|top| *top != APP_MENU_NAME)
    {
        let mut items: Vec<MenuItem> = direct
            .get(&top)
            .map(|entries| entries.iter().map(|meta| action_item(cx, meta)).collect())
            .unwrap_or_default();
        if let Some(subs) = sub_order.get(&top) {
            for sub in subs {
                if !items.is_empty() {
                    items.push(MenuItem::separator());
                }
                items.push(submenu(cx, sub, sub_items.get(&(top, sub))));
            }
        }
        if items.is_empty() {
            continue;
        }
        let menu = Menu::new(tr(cx, top)).items(items);
        if top == "帮助" {
            help_menus.push(menu);
            continue;
        }
        menus.push(menu);
        if !window_added {
            window_added = true;
            menus.push(window_menu(cx));
        }
    }
    if !window_added {
        menus.push(window_menu(cx));
    }
    menus.extend(help_menus);

    cx.set_menus(menus);
}

/// 窗口菜单：最小化 / 缩放 / 关闭当前窗口。
fn window_menu(cx: &App) -> Menu {
    Menu::new(tr(cx, "窗口")).items(vec![
        static_item(cx, MINIMIZE_COMMAND, "最小化"),
        static_item(cx, ZOOM_COMMAND, "缩放"),
        MenuItem::separator(),
        static_item(cx, CLOSE_COMMAND, "关闭"),
    ])
}

/// 构造一个指向固定动作 id 的菜单项（不经动作注册表，用于上面的标准菜单）。
fn static_item(cx: &App, id: &'static str, label: &'static str) -> MenuItem {
    MenuItem::action(tr(cx, label), command(id))
}

/// 构造子菜单。
fn submenu(cx: &App, name: &'static str, entries: Option<&Vec<&ActionMeta>>) -> MenuItem {
    let menu = Menu::new(tr(cx, name));
    let menu = match entries {
        Some(entries) => menu.items(entries.iter().map(|meta| action_item(cx, meta))),
        None => menu,
    };
    MenuItem::submenu(menu)
}

/// 构造一个指向动作的菜单项。
fn action_item(cx: &App, meta: &ActionMeta) -> MenuItem {
    MenuItem::action((meta.label)(cx), command(meta.id))
}

/// 构造桥接动作。
fn command(id: &'static str) -> RunCommand {
    RunCommand { id: id.into() }
}
