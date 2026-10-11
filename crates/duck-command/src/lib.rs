//! Duck Toolbox 交互骨架：**一份动作注册表，派生四个输出面**。
//!
//! 快捷键、原生菜单栏、命令面板、工具栏按钮本质是同一份动作清单的不同投影。本 crate 只
//! 保存那份清单（[`ActionMeta`] + 处理器），再把它们投影出去：
//!
//! ```text
//!                        ┌──────────────► cx.bind_keys()     全局快捷键
//! ActionMeta + Handler ──┼──────────────► cx.set_menus()    原生菜单栏
//!                        ├──────────────► CommandGroup       命令面板
//!                        └──────────────► (后续) 工具栏按钮
//! ```
//!
//! 之所以能把「字符串 id」接到 gpui 的动作系统上，是因为 gpui 的动作必须是具体类型：
//! 这里用一个统一的桥接动作 [`RunCommand`]（携带 id）承接所有绑定，再由 [`dispatch`]
//! 查表调用真正的处理器。

mod action;
pub mod fuzzy;
mod keymap;
mod menu;
mod palette;

pub use action::{
    ActionMeta, ActionSlot, AppHandler, Handler, RunCommand, declare, dispatch, dispatch_app,
    handler, install, install_app_handler, install_handler, metas,
};
pub use keymap::{bind_all, keybindings_for, pretty, primary_modifier};
pub use menu::{
    ABOUT_COMMAND, APP_MENU_NAME, CLOSE_COMMAND, HIDE_COMMAND, HIDE_OTHERS_COMMAND,
    MINIMIZE_COMMAND, QUIT_COMMAND, SHOW_ALL_COMMAND, ZOOM_COMMAND, refresh_menus,
};
pub use palette::palette_groups;
