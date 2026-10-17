//! 设置页「热键」：录入器与按键处理。
//!
//! 每段结构相同（开关 + 键位），只有命令面板那段多一行「唤起时打开主窗口」：
//!
//! - **唤起命令面板**（`⌥Space`）—— 现行的全局唤起键。
//! - **随手看悬浮窗**（`⌥⌘T`）—— 项目管理页的「随手看」入口（`docs/projects-design.md` §4.3 ⑤）。
//! - **快速捕获窗**（`⌥⌘N`）—— 一句话记一条任务（G-PRJ-12）。
//!
//! 每段各有自己的开关：全局热键会**占掉**一个组合，默认给出几个键就得给用户几个关掉
//! 它的出口（「不打扰」原则）。各槽位的界面状态按 [`HotkeySlot::index`] 存在
//! `SettingsWindowView::hotkeys` 里，行与录入器都由槽位参数化 —— 加新键时这里
//! 不用再抄一遍。

use super::*;

use crate::hotkey::HotkeySlot;

impl SettingsWindowView {
    // ── 唤起（全局快捷键）───────────────────────────────────────────────
    pub(super) fn hotkey_section(&self, cx: &mut Context<Self>) -> Div {
        let launcher = settings_section(
            Some(tr(cx, "唤起命令面板").into()),
            [
                self.hotkey_enabled_row(HotkeySlot::Launcher, cx),
                self.hotkey_binding_row(HotkeySlot::Launcher, cx),
                self.launcher_main_window_row(cx),
            ],
            cx,
        );
        let float = settings_section(
            Some(tr(cx, "随手看悬浮窗").into()),
            [
                self.hotkey_enabled_row(HotkeySlot::TaskFloat, cx),
                self.hotkey_binding_row(HotkeySlot::TaskFloat, cx),
            ],
            cx,
        );
        let capture = settings_section(
            Some(tr(cx, "快速捕获窗").into()),
            [
                self.hotkey_enabled_row(HotkeySlot::QuickCapture, cx),
                self.hotkey_binding_row(HotkeySlot::QuickCapture, cx),
            ],
            cx,
        );
        let tokens = active_theme(cx).tokens();
        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(tokens.spacing.xl)
            .child(launcher)
            .child(float)
            .child(capture)
    }

    /// 「启用…快捷键」那一行。
    ///
    /// 开关先按目标值落状态、失败再按真实值回摆 —— 与「开机自启」同一个形状，
    /// 保证开关显示的就是真实情况（`active_spec` 问的是系统里到底有没有登记上）。
    fn hotkey_enabled_row(&self, slot: HotkeySlot, cx: &mut Context<Self>) -> AnyElement {
        let (title, hint) = match slot {
            HotkeySlot::Launcher => (
                tr(cx, "启用唤起快捷键"),
                tr(
                    cx,
                    "全局生效，与上面的应用内键位互不影响：应用内键位只在 Duck 有焦点时管用，这一条在任何程序里都能把 Duck 叫出来。",
                ),
            ),
            HotkeySlot::TaskFloat => (
                tr(cx, "启用「随手看」快捷键"),
                tr(
                    cx,
                    "在任何程序里按一下都能把「随手看」悬浮窗叫出来，再按一次收起来。它不抢焦点，适合瞄一眼进行中与待办。",
                ),
            ),
            HotkeySlot::CalendarFloat => (
                tr(cx, "启用日历浮窗快捷键"),
                tr(
                    cx,
                    "在任何程序里按一下都能把日历浮窗叫出来，再按一次收起来。它不抢焦点，适合瞄一眼哪天有什么。",
                ),
            ),
            HotkeySlot::QuickCapture => (
                tr(cx, "启用快速捕获快捷键"),
                tr(
                    cx,
                    "在任何程序里按一下就能记一条任务：回车保存并关窗，Esc 放弃。它抢焦点，因为下一件事就是打字。",
                ),
            ),
        };
        option_row(
            title,
            Some(hint.into()),
            switch_control(
                // `SharedString` 自己就 `Into<ElementId>`（`ElementId::Name` 那层包装是多余的，
                // 而且 `ElementId` 不在这里的作用域里）。
                SharedString::from(format!("duck-settings-hotkey-enabled-{}", slot.name())),
                self.hotkey_entry(slot).enabled,
                cx.listener(move |this, next: &bool, window, cx| {
                    this.hotkey_entry_mut(slot).enabled = *next;
                    if let Err(error) = crate::hotkey::set_enabled(cx, slot, *next) {
                        this.hotkey_entry_mut(slot).enabled =
                            duck_platform::active_spec(slot.name()).is_some();
                        let spec = crate::hotkey::raw_spec(cx, slot);
                        crate::hotkey::report_failure(window, cx, slot, &spec, &error);
                    }
                    cx.notify();
                }),
            ),
            cx,
        )
        .into_any_element()
    }

    /// 键帽那一行：状态行 + 录入器。
    fn hotkey_binding_row(&self, slot: HotkeySlot, cx: &mut Context<Self>) -> AnyElement {
        // `tr()` 返回 `&'static str`（`Copy`），两处都直接用，不需要 `.clone()`。
        let label = tr(cx, "快捷键");
        let status = hotkey_status(self, slot, cx);
        option_row(
            label,
            Some(status),
            self.hotkey_recorder(slot, label.into(), cx),
            cx,
        )
        .into_any_element()
    }

    /// 「唤起时打开主窗口」那一行（只有命令面板有：悬浮窗本来就是窗口）。
    ///
    /// 缺省关：`⌥Space` 自带启动器语义，默认只弹浮层而不把主窗切到前台。
    /// 开着则回到旧行为（完整主窗），也是浮窗那条路出问题时的兜底。
    fn launcher_main_window_row(&self, cx: &mut Context<Self>) -> AnyElement {
        option_row(
            tr(cx, "唤起时打开主窗口"),
            Some(
                tr(
                    cx,
                    "关闭时只弹出一个快速面板，盖在当前工作之上，不动主窗口；打开时唤起完整主窗口（经典行为）。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-launcher-main-window",
                self.launcher_opens_main_window,
                cx.listener(|this, next: &bool, _, cx| {
                    this.launcher_opens_main_window = *next;
                    settings::set_launcher_opens_main_window(cx, *next);
                    cx.notify();
                }),
            ),
            cx,
        )
        .into_any_element()
    }

    /// 快捷键录入控件：一个可聚焦的「键帽框」。
    ///
    /// 点一下进入录入，之后按下的第一个「修饰键 + 普通键」组合就是新键位，Esc 取消；
    /// 纯键盘走到这儿按 Enter / Space 也能进录入（见 [`Self::on_hotkey_key_down`]）。
    /// 用裸 `div` + `track_focus` 而不是按钮：这里要的是**原始按键**，而按钮会把
    /// 按键当「激活」处理掉，拿不到组合。
    ///
    /// `action_label` 是该行的可见标签（「快捷键」），只用来拼可访问名：行标签是
    /// **兄弟节点**，不会自动并进控件的名字里去。
    pub(super) fn hotkey_recorder(
        &self,
        slot: HotkeySlot,
        action_label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let chrome = extended.colors;
        let recording = self.hotkey_entry(slot).recording;
        let label: SharedString = if recording {
            tr(cx, "按下组合键…（Esc 取消）").into()
        } else {
            crate::hotkey::pretty_spec(cx, slot).into()
        };

        div()
            .id(SharedString::from(format!(
                "duck-settings-hotkey-recorder-{}",
                slot.name()
            )))
            // 无障碍：`track_focus` 只置 `focusable`、**不置 `tab_stop`**，所以此前这个
            // 控件 Tab 根本到不了。可访问名 = 行标签 + 当前键位（两段文案都取自既有
            // i18n 表，不新增待翻译串）；只报键位等于报了个值却没说它是什么。录入中
            // `label` 就是「按下组合键…（Esc 取消）」，恰好状态自述。
            .role(Role::Button)
            .aria_label(SharedString::from(format!("{action_label} {label}")))
            .tab_stop(true)
            .track_focus(&self.hotkey_entry(slot).focus)
            .h(theme.density().control)
            // 快捷键录入按钮的最小宽度：要装得下「⌘⇧⌥⌃ + 字母」这类最长组合，
            // 属布局约束（不在 spacing 阶梯上），保留裸值。
            .min_w(px(220.))
            .px(tokens.spacing.md)
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme.components().input_radius)
            .border(extended.stroke.thin)
            .border_color(if recording {
                tokens.colors.ring
            } else {
                tokens.colors.border
            })
            .bg(if recording {
                chrome.hover_tint
            } else {
                tokens.colors.surface
            })
            .text_size(tokens.typography.sm.size)
            .text_color(tokens.colors.foreground)
            .cursor_pointer()
            .focus_visible({
                let ring = tokens.colors.ring;
                move |style| style.border_color(ring)
            })
            .when(!recording, |element| {
                element.hover(move |style| style.bg(chrome.hover_tint))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    // 先要焦点，再进录入：没有焦点的话按键根本不会派发到这里。
                    let focus = this.hotkey_entry(slot).focus.clone();
                    window.focus(&focus, cx);
                    if !this.hotkey_entry(slot).recording {
                        this.hotkey_entry_mut(slot).recording = true;
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(move |this, event, window, cx| {
                this.on_hotkey_key_down(slot, event, window, cx);
            }))
            .child(label)
            .into_any_element()
    }

    /// 录入中的一次按键。
    ///
    /// 三段：Esc 退出、只有修饰键就继续等、其余都当成「就是它了」。
    /// 全程 `stop_propagation` —— 否则 `⌘1`（切路由）之类的应用内键位会在录入的
    /// 同一瞬间也生效，用户看到的是一边录一边跳页。
    pub(super) fn on_hotkey_key_down(
        &mut self,
        slot: HotkeySlot,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if !self.hotkey_entry(slot).recording {
            // 键盘入口：鼠标是靠 `on_mouse_down` 进录入的，纯键盘本来没有入口——那样
            // `tab_stop(true)` 只是多了一个「能聚焦但动不了」的站，比不可聚焦更糟。
            // 这里补 Enter / Space（而且要**无修饰键**，免得抢走 `⌘↵` 之类的应用内键位）。
            if !keystroke.modifiers.modified()
                && (keystroke.key.eq_ignore_ascii_case("enter")
                    || keystroke.key.eq_ignore_ascii_case("space"))
            {
                cx.stop_propagation();
                self.hotkey_entry_mut(slot).recording = true;
                cx.notify();
            }
            return;
        }
        // Esc 取消（不带修饰键的那个 Esc，`⌘Esc` 是另一回事）。
        if keystroke.key.eq_ignore_ascii_case("escape") && !keystroke.modifiers.modified() {
            self.hotkey_entry_mut(slot).recording = false;
            cx.notify();
            return;
        }
        let modifiers = crate::hotkey::modifiers_of(keystroke.modifiers);
        if modifiers.is_empty() {
            // 还没按到主键（用户会先按住修饰键再按主键）。继续等。
            return;
        }
        cx.stop_propagation();
        match duck_platform::Hotkey::from_key(&keystroke.key, modifiers) {
            Ok(hotkey) => {
                self.hotkey_entry_mut(slot).recording = false;
                self.hotkey_entry_mut(slot).enabled = true;
                if let Err(error) = crate::hotkey::rebind(cx, slot, &hotkey) {
                    // 登记失败时 `kv` 与系统登记都没动，所以这里也要把开关摆回真实值。
                    self.hotkey_entry_mut(slot).enabled =
                        duck_platform::active_spec(slot.name()).is_some();
                    crate::hotkey::report_failure(window, cx, slot, hotkey.spec(), &error);
                }
                cx.notify();
            }
            Err(duck_platform::HotkeyError::ModifierOnly) => {
                // 还在按修饰键，继续等主键。不退出录入，也不提示。
            }
            Err(error) => {
                self.hotkey_entry_mut(slot).recording = false;
                crate::hotkey::report_failure(window, cx, slot, &keystroke.key, &error);
                cx.notify();
            }
        }
    }
}

/// 状态行三种可能，按「用户最需要知道什么」排序：登记不上 → 说原因；关了 →
/// 说关了；否则说用法。失败排在最前是因为它最容易被误解成「功能没做」——
/// 后面那句「已关闭」还会盖住原因（开关之所以关，正是因为它登记不上）。
fn hotkey_status(
    this: &SettingsWindowView,
    slot: HotkeySlot,
    cx: &Context<SettingsWindowView>,
) -> SharedString {
    if let Some(error) = duck_platform::last_error(slot.name()) {
        let reason = crate::hotkey::explain(cx, &error);
        trf(cx, "未生效：{}", &[reason.as_ref()]).into()
    } else if !this.hotkey_entry(slot).enabled {
        tr(cx, "已关闭。").into()
    } else {
        match slot {
            HotkeySlot::Launcher => tr(
                cx,
                "在别的程序里也能用；Duck 已经在前台时再按一次会把它收起来。",
            ),
            HotkeySlot::TaskFloat => tr(cx, "在别的程序里也能用；再按一次会把它收起来。"),
            HotkeySlot::CalendarFloat => tr(cx, "在别的程序里也能用；再按一次会把它收起来。"),
            HotkeySlot::QuickCapture => tr(cx, "在别的程序里也能用；回车保存后自动关窗。"),
        }
        .into()
    }
}
