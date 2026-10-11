//! 动作注册表与派发。

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, Global, Hsla, SharedString, Window};
use gpui_component::Icon;

/// 动作可出现的输出面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionSlot {
    /// 原生菜单栏。
    Menu,
    /// 命令面板。
    Palette,
    /// 窗口左侧活动栏按钮。
    ActivityBar,
}

/// 动作 / 命令的元数据。
///
/// 所有字段都是 `'static` 友好的（函数指针 + 静态字符串），所以注册表可以长期持有，
/// **不需要 `Box::leak`**，也不会随语言切换而失效（文案在渲染时经 `label` 求值）。
pub struct ActionMeta {
    /// 稳定 id，例 `"toolbox.refresh"`。命令面板、菜单、快捷键、最近使用都以它为准。
    pub id: &'static str,
    /// 展示名，随当前语言解析（传 `|cx| SharedString::from(tr(cx, "刷新"))`）。
    pub label: fn(&App) -> SharedString,
    /// 附加搜索词（中英），命令面板的匹配目标之一。
    pub keywords: &'static [&'static str],
    /// 命令面板里的分组标题；`None` 表示归入默认分组。
    pub group: Option<&'static str>,
    /// 图标工厂；按当前主题解析（例：浅色月亮 / 深色太阳）。`None` 表示无图标。
    pub icon: Option<fn(&App) -> Icon>,
    /// 图标颜色工厂；`None` 用活动栏统一的弱化前景色。
    pub icon_color: Option<fn(&App) -> Hsla>,
    /// 出现的输出面。
    pub slots: &'static [ActionSlot],
    /// 菜单归属 `(顶级菜单, 子菜单)`；`None` 表示不进菜单。
    pub menu: Option<(&'static str, Option<&'static str>)>,
    /// 默认键位，见 [`crate::keymap`] 的 `$mod` 约定。
    pub keys: &'static [&'static str],
}

/// 动作处理器：接收窗口与应用上下文。
pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 不依赖窗口的动作处理器：接收应用上下文。
///
/// 菜单栏上的「退出」「打开设置」这类动作本来就与具体窗口无关，登记成这种形式后
/// **窗口全关时也能用**（见 [`install`] 的派发说明）。
pub type AppHandler = Rc<dyn Fn(&mut App)>;

/// 全局注册表。
#[derive(Default)]
pub(crate) struct Registry {
    metas: Vec<ActionMeta>,
    handlers: HashMap<&'static str, Handler>,
    app_handlers: HashMap<&'static str, AppHandler>,
}

impl Global for Registry {}

/// 桥接动作：把字符串 id 送进 gpui 的键盘 / 菜单派发链。
///
/// 它实现 `Action`，因此可以出现在 [`gpui::KeyBinding`] 与 [`gpui::MenuItem`] 里；
/// 被触发后由全局监听器转交给 [`dispatch`]。
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = duck, no_json)]
pub struct RunCommand {
    /// 目标动作 id。
    pub id: SharedString,
}

/// 建立注册表并挂上全局派发监听（在创建任何窗口之前调用一次）。
///
/// **没有活跃窗口时的派发**：菜单栏在全部窗口关闭后依然可点（macOS 上关窗不退出应用），
/// 此时 gpui 会把动作走全局监听器送过来——这里不能直接丢掉，否则「退出」「打开设置」
/// 点了毫无反应，而菜单项又是可用的（注册过全局监听器就不会置灰），看起来像卡死。
/// 所以没有窗口时改派给 [`install_app_handler`] 登记的处理器；只有窗口级处理器
/// （例如要弹 toast 的）时才真的什么都不做。
pub fn install(cx: &mut App) {
    cx.set_global(Registry::default());
    cx.on_action(move |action: &RunCommand, cx: &mut App| {
        // **不能在这里直接 `window.update`**：本监听器几乎总是在某个窗口的更新内
        // 被调用（键击 / 命令面板确认都走窗口的动作派发链），此时该窗口的租约被
        // 当前更新占着，再入更新会报 "window not found" 并被 `log_err` 静默吞掉
        // ——表现就是面板点击 / 快捷键全部哑火。推迟一拍，等本次更新结束后
        // 再拿活跃窗口执行（无活跃窗口时仍回落 App 级处理器）。
        let id = action.id.clone();
        cx.defer(move |cx| {
            let Some(window) = cx.active_window() else {
                dispatch_app(&id, cx);
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                dispatch(&id, window, cx);
            });
        });
    });
}

/// 登记一个动作的元数据。id 重复时记警告并忽略后一条。
pub fn declare(cx: &mut App, meta: ActionMeta) {
    let registry = registry_mut(cx);
    if registry.metas.iter().any(|existing| existing.id == meta.id) {
        tracing::warn!(
            id = meta.id,
            "duplicate action declaration, ignoring the later one"
        );
        return;
    }
    registry.metas.push(meta);
}

/// 为动作绑定处理器（可后于 [`declare`] 调用，方便窗口创建后再挂依赖视图的处理器）。
pub fn install_handler(
    cx: &mut App,
    id: &'static str,
    handler: impl Fn(&mut Window, &mut App) + 'static,
) {
    let handler: Handler = Rc::new(handler);
    registry_mut(cx).handlers.insert(id, handler);
}

/// 为动作绑定**不依赖窗口**的处理器：窗口全关时同样执行。
///
/// 与 [`install_handler`] 并存，同一个 id 只登记其中一种即可。两种都登记时：
/// 有窗口走窗口级，没窗口走 App 级。
pub fn install_app_handler(cx: &mut App, id: &'static str, handler: impl Fn(&mut App) + 'static) {
    let handler: AppHandler = Rc::new(handler);
    registry_mut(cx).app_handlers.insert(id, handler);
}

/// 按 id 执行动作。
///
/// 没有窗口级处理器时回落到 App 级处理器——调用方拿得到窗口（例如活动栏按钮），
/// 但动作本身未必需要它。
pub fn dispatch(id: &str, window: &mut Window, cx: &mut App) {
    let handler = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.handlers.get(id))
        .cloned();
    match handler {
        Some(handler) => handler(window, cx),
        None => dispatch_app(id, cx),
    }
}

/// 按 id 执行动作，**不使用窗口**。
///
/// 只认 [`install_app_handler`] 登记过的处理器；给没有窗口时（全部窗口已关闭）的
/// 菜单派发留一条路。
pub fn dispatch_app(id: &str, cx: &mut App) {
    let handler = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.app_handlers.get(id))
        .cloned();
    match handler {
        Some(handler) => handler(cx),
        None => tracing::debug!(id, "unregistered action was triggered"),
    }
}

/// 遍历全部已登记的动作元数据。
#[must_use]
pub fn metas(cx: &App) -> &[ActionMeta] {
    cx.try_global::<Registry>()
        .map_or(&[], |registry| &registry.metas)
}

/// 查询某个动作是否已绑定**窗口级**处理器（App 级的见 [`install_app_handler`]）。
#[must_use]
pub fn handler(cx: &App, id: &str) -> Option<Handler> {
    cx.try_global::<Registry>()
        .and_then(|registry| registry.handlers.get(id))
        .cloned()
}

/// 取（必要时创建）注册表。
fn registry_mut(cx: &mut App) -> &mut Registry {
    if !cx.has_global::<Registry>() {
        cx.set_global(Registry::default());
    }
    cx.global_mut::<Registry>()
}

#[cfg(test)]
mod tests {
    use super::RunCommand;
    use gpui::{Action as _, SharedString};

    #[test]
    fn run_command_carries_id_and_names_itself() {
        let action = RunCommand {
            id: SharedString::from("toolbox.refresh"),
        };
        assert_eq!(action.name(), "duck::RunCommand");
        assert_eq!(action.id.as_ref(), "toolbox.refresh");
    }

    #[test]
    fn run_command_equality_is_by_id() {
        let a = RunCommand {
            id: SharedString::from("a"),
        };
        let b = RunCommand {
            id: SharedString::from("a"),
        };
        let c = RunCommand {
            id: SharedString::from("b"),
        };
        assert!(a.partial_eq(&b as &dyn gpui::Action));
        assert!(!a.partial_eq(&c as &dyn gpui::Action));
    }
}
