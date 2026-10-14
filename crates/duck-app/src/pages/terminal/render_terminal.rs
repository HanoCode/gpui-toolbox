//! 终端区渲染：工具条、画布、查找条、重连条与重连动作。

use super::*;

impl TerminalPage {
    /// 终端工具条：会话信息 + 真实的字号控制与清屏。
    pub(super) fn render_terminal_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let tab = self.active_tab();
        let title = tab.map_or_else(|| "—".to_owned(), |tab| tab.title.clone());
        let host = tab
            .and_then(|tab| self.connection_entry(&tab.id))
            .map_or_else(|| "localhost".to_owned(), |item| item.endpoint());
        let disabled_down = self.terminal_font_size <= *FONT_SIZE_RANGE.start();
        let disabled_up = self.terminal_font_size >= *FONT_SIZE_RANGE.end();

        h_flex()
            .w_full()
            .h(theme.density().section_header)
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_b(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        Icon::new(tab.map_or(DuckIcon::SquareTerminal, |tab| tab.kind.icon()))
                            .size(extended.icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(body_text(title, cx))
                    .child(meta_text(host, cx)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        toolbar_action_button(
                            "terminal-font-smaller",
                            tr(cx, "减小字号"),
                            Icon::new(DuckIcon::Minus).size(extended.icon.md),
                            false,
                            disabled_down,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(-1., cx));
                            }
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(READOUT_WIDTH))
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .font_features(tabular_numbers())
                            .text_color(tokens.colors.muted_foreground)
                            .text_center()
                            .child(format!("{:.0} px", self.terminal_font_size)),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-font-larger",
                            tr(cx, "增大字号"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.md),
                            false,
                            disabled_up,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.nudge_font_size(1., cx));
                            }
                        }),
                    )
                    .child(
                        toolbar_action_button(
                            "terminal-clear",
                            tr(cx, "清屏"),
                            Icon::new(DuckIcon::Trash2).size(extended.icon.md),
                            false,
                            false,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.clear_active(cx));
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 终端内容区：工具条 + 画布。
    ///
    /// 画布这一层要**先**取出来（它需要 `&mut Context`），`active_theme` 返回的是
    /// 全局主题的引用（不是拷贝），一旦借上就借到函数末尾，两者不能重叠。
    pub(super) fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let toolbar = self.render_terminal_toolbar(cx);
        let find_bar = self.render_find_bar(cx);
        let canvas = self.render_terminal_canvas(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();

        v_flex()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(toolbar)
            .when_some(find_bar, |this, bar| this.child(bar))
            .child(canvas)
            .into_any_element()
    }

    /// 缓冲区搜索条（`⌘F` 打开）。没打开时返回 `None`。
    ///
    /// 位置在工具条与画布之间：贴着它作用的内容，但不挤占画布的宽度。
    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let empty = find.matches.is_empty();
        // 计数用「第 n / 共 m 项」：只给 m 的话，用户不知道 Enter 走到哪了。
        let counter = if empty {
            tr(cx, "无结果").to_owned()
        } else {
            trf(
                cx,
                "{} / {} 项",
                &[
                    &(find.current + 1).to_string(),
                    &find.matches.len().to_string(),
                ],
            )
        };
        let view_prev = self.view.clone();
        let view_next = self.view.clone();
        let view_close = self.view.clone();

        Some(
            h_flex()
                .id(SharedString::from("terminal-find-bar"))
                .w_full()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .bg(extended.colors.chrome)
                .border_b(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    Icon::new(DuckIcon::Search)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                )
                .child(
                    div().w(px(FIND_INPUT_WIDTH)).flex_none().min_w_0().child(
                        Input::new(&find.input)
                            .appearance(false)
                            .with_size(Size::Small)
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .w_full(),
                    ),
                )
                .child(meta_text(SharedString::from(counter), cx))
                .child(
                    toolbar_action_button(
                        "terminal-find-prev",
                        tr(cx, "上一个"),
                        Icon::new(DuckIcon::ArrowUp).size(extended.icon.md),
                        false,
                        empty,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view_prev.update(cx, |this, cx| this.step_find(-1, cx));
                    }),
                )
                .child(
                    toolbar_action_button(
                        "terminal-find-next",
                        tr(cx, "下一个"),
                        Icon::new(DuckIcon::ArrowDown).size(extended.icon.md),
                        false,
                        empty,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let _ = view_next.update(cx, |this, cx| this.step_find(1, cx));
                    }),
                )
                .child(
                    toolbar_action_button(
                        "terminal-find-close",
                        tr(cx, "关闭搜索"),
                        Icon::new(DuckIcon::X).size(extended.icon.md),
                        false,
                        false,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view_close.update(cx, |this, cx| this.close_find(window, cx));
                    }),
                )
                // `Esc` 收起、再按一次 `⌘F` 重新聚焦输入框。
                //
                // 搜索条**刻意不挂** `key_context("Terminal")`：挂了的话这里会变成又一层
                // 终端上下文，输入框里的 Tab / Ctrl-C 之类会绕过 `Input` 的默认处理跑到
                // 终端动作上。所以快捷键只在这一条 `on_key_down` 里手判。
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if key == "escape" {
                        this.close_find(window, cx);
                        cx.stop_propagation();
                    } else if event.keystroke.modifiers.platform && key == "f" {
                        if let Some(find) = this.find.as_ref() {
                            let handle = find.input.read(cx).focus_handle(cx);
                            window.focus(&handle, cx);
                        }
                        cx.stop_propagation();
                    }
                }))
                .into_any_element(),
        )
    }

    /// 终端画布分派：**本地与 SSH 走同一条路径**。
    ///
    /// SSH 就是一条 `ssh -t user@host` 的 PTY 会话（见 `duck-ssh` 模块头），
    /// 网格、键盘、滚动、选中全部复用，**没有第二套画布**。
    ///
    /// 分屏也复用这一条：右半窗格**是另一条会话**（键见
    /// [`data::split_session_key`]），画的是同一个[`Self::render_terminal_pane`]，
    /// 只是并排放。所以两半各有各的网格尺寸（每个人各自把自己的像素宽度
    /// 换算成列数上报），互不干涉。
    pub(super) fn render_terminal_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.active_tab() else {
            return div().into_any_element();
        };
        let tab_id = tab.id.clone();
        let kind = tab.kind;
        // 右窗格连谁由归属决定（「不同连接分屏」），类型跟着归属走。
        let split_kind = tab.split_kind().unwrap_or(kind);
        let split = tab.split.clone();
        let Some(split) = split else {
            let pane = self.render_terminal_pane(&tab_id, kind, false, cx);
            // 未分屏的画布就是「拖标签并入分屏」的落点：挂上投放区与松手处理。
            return self.with_split_drop_target(&tab_id, pane, cx);
        };
        // 隔离线：与 SFTP 双栏同一条发丝线（见 [`Self::render_pane_divider`]），
        // 只是套了条可拖动的命中带。
        let left = self.render_terminal_pane(&tab_id, kind, false, cx);
        let right = self.render_terminal_pane(&split, split_kind, true, cx);
        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            // 左栏宽度 = 记忆占比（默认各半）；右栏 `flex_1` 吃剩余空间。
            // 拖隔离栏改占比，两块画布各自按新宽度重排（`apply_canvas_bounds`）。
            .child(
                div()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .w(relative(self.pane_ratio()))
                    .flex_none()
                    .child(left),
            )
            .child(self.render_pane_divider(cx))
            .child(div().flex_1().h_full().min_w_0().min_h_0().child(right))
            // 拖动中比例实时跟手：事件挂在**包含隔离栏的整条容器**上，
            // 指针拖出隔离栏命中带也持续生效（gpui 的 drag 语义）。
            .on_drag_move::<PaneDrag>(cx.listener(Self::on_pane_divider_drag))
            // 松手（落点在容器内）即收尾：清「正在拖动」标记。
            .on_drop::<PaneDrag>(cx.listener(|this, _, _window, cx| {
                if this.resizing_pane {
                    this.resizing_pane = false;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// SSH 断开后的提示条：一句话 + 「重新连接」。
    ///
    /// 不做自动重连：认证失败、主机密钥不匹配这类原因**重连一百次也不会好**，
    /// 自动重试只会把真实错误刷出屏幕，还可能在账号被锁时雪上加霜。
    pub(super) fn render_reconnect_bar(&self, tab_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let id = tab_id.to_owned();

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .child(meta_text(tr(cx, "连接已断开。"), cx))
            .child(
                toolbar_action_button(
                    "terminal-reconnect",
                    tr(cx, "重新连接"),
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.md),
                    false,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let id = id.clone();
                    let _ = view.update(cx, |this, cx| this.reconnect(&id, cx));
                }),
            )
            .into_any_element()
    }

    /// 重新建立一条断开的 SSH 会话。
    ///
    /// 先把旧会话从表里摘掉：`ensure_sessions` 只看「表里有没有这个 id」，
    /// 不摘掉的话它会认为「已经起过了」，什么都不会做。
    pub(super) fn reconnect(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        self.sessions.remove(tab_id);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.status = LinkStatus::Connecting;
        }
        cx.notify();
    }
}
