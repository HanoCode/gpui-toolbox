//! 设置页「热键」：录入器与按键处理。

use super::*;

impl SettingsWindowView {
    // ── 唤起（全局快捷键）───────────────────────────────────────────────
    pub(super) fn hotkey_section(&self, cx: &mut Context<Self>) -> Div {
        // 状态行的三种可能，按「用户最需要知道什么」排序：登记不上 → 说原因；关了 →
        // 说关了；否则说用法。失败排在最前是因为它最容易被误解成「功能没做」——
        // 后面那句「已关闭」还会盖住原因（开关之所以关，正是因为它登记不上）。
        let status: SharedString = if let Some(error) = duck_platform::last_error() {
            let reason = crate::hotkey::explain(cx, &error);
            trf(cx, "未生效：{}", &[reason.as_ref()]).into()
        } else if !self.hotkey_enabled {
            tr(cx, "已关闭。").into()
        } else {
            tr(
                cx,
                "在别的程序里也能用；Duck 已经在前台时再按一次会把它收起来。",
            )
            .into()
        };

        let enabled_row = option_row(
            tr(cx, "启用唤起快捷键"),
            Some(
                tr(
                    cx,
                    "全局生效，与上面的应用内键位互不影响：应用内键位只在 Duck 有焦点时管用，这一条在任何程序里都能把 Duck 叫出来。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-hotkey-enabled",
                self.hotkey_enabled,
                cx.listener(|this, next: &bool, window, cx| {
                    // 先按目标值落状态，失败再按真实值回摆 —— 与「开机自启」同一个形状，
                    // 保证开关显示的就是真实情况。
                    this.hotkey_enabled = *next;
                    if let Err(error) = crate::hotkey::set_enabled(cx, *next) {
                        this.hotkey_enabled = duck_platform::active_spec().is_some();
                        let spec = crate::hotkey::raw_spec(cx);
                        crate::hotkey::report_failure(window, cx, &spec, &error);
                    }
                    cx.notify();
                }),
            ),
            cx,
        );

        // `tr()` 返回 `&'static str`（`Copy`），两处都直接用，不需要 `.clone()`。
        let hotkey_label = tr(cx, "快捷键");
        let binding_row = option_row(
            hotkey_label,
            Some(status),
            self.hotkey_recorder(hotkey_label.into(), cx),
            cx,
        );

        // 唤起语义。缺省关：`⌥Space` 自带启动器语义，默认只弹浮层而不把主窗切到前台。
        // 开着则回到旧行为（完整主窗），也是浮窗那条路出问题时的兜底。
        let opens_main_window_row = option_row(
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
        );

        settings_section(
            Some(tr(cx, "唤起").into()),
            [
                enabled_row.into_any_element(),
                binding_row.into_any_element(),
                opens_main_window_row.into_any_element(),
            ],
            cx,
        )
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
        action_label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let chrome = extended.colors;
        let recording = self.hotkey_recording;
        let label: SharedString = if recording {
            tr(cx, "按下组合键…（Esc 取消）").into()
        } else {
            crate::hotkey::pretty_spec(cx).into()
        };

        div()
            .id("duck-settings-hotkey-recorder")
            // 无障碍：`track_focus` 只置 `focusable`、**不置 `tab_stop`**，所以此前这个
            // 控件 Tab 根本到不了。可访问名 = 行标签 + 当前键位（两段文案都取自既有
            // i18n 表，不新增待翻译串）；只报键位等于报了个值却没说它是什么。录入中
            // `label` 就是「按下组合键…（Esc 取消）」，恰好状态自述。
            .role(Role::Button)
            .aria_label(SharedString::from(format!("{action_label} {label}")))
            .tab_stop(true)
            .track_focus(&self.hotkey_focus)
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
                cx.listener(|this, _, window, cx| {
                    // 先要焦点，再进录入：没有焦点的话按键根本不会派发到这里。
                    window.focus(&this.hotkey_focus, cx);
                    if !this.hotkey_recording {
                        this.hotkey_recording = true;
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(Self::on_hotkey_key_down))
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
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if !self.hotkey_recording {
            // 键盘入口：鼠标是靠 `on_mouse_down` 进录入的，纯键盘本来没有入口——那样
            // `tab_stop(true)` 只是多了一个「能聚焦但动不了」的站，比不可聚焦更糟。
            // 这里补 Enter / Space（而且要**无修饰键**，免得抢走 `⌘↵` 之类的应用内键位）。
            if !keystroke.modifiers.modified()
                && (keystroke.key.eq_ignore_ascii_case("enter")
                    || keystroke.key.eq_ignore_ascii_case("space"))
            {
                cx.stop_propagation();
                self.hotkey_recording = true;
                cx.notify();
            }
            return;
        }
        // Esc 取消（不带修饰键的那个 Esc，`⌘Esc` 是另一回事）。
        if keystroke.key.eq_ignore_ascii_case("escape") && !keystroke.modifiers.modified() {
            self.hotkey_recording = false;
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
                self.hotkey_recording = false;
                self.hotkey_enabled = true;
                if let Err(error) = crate::hotkey::rebind(cx, &hotkey) {
                    // 登记失败时 `kv` 与系统登记都没动，所以这里也要把开关摆回真实值。
                    self.hotkey_enabled = duck_platform::active_spec().is_some();
                    crate::hotkey::report_failure(window, cx, hotkey.spec(), &error);
                }
                cx.notify();
            }
            Err(duck_platform::HotkeyError::ModifierOnly) => {
                // 还在按修饰键，继续等主键。不退出录入，也不提示。
            }
            Err(error) => {
                self.hotkey_recording = false;
                crate::hotkey::report_failure(window, cx, &keystroke.key, &error);
                cx.notify();
            }
        }
    }
}
