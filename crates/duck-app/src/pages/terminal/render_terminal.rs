//! 终端区渲染：工具条、画布、查找条、重连条与重连动作。

use super::*;

impl TerminalPage {
    /// 终端工具条（**浮层**）：字号控制 + 清屏 + 钉住。
    ///
    /// 只放「画布上无处可点、又不能只靠快捷键」的动作。**不放标题与主机**：
    /// 标签上已经有标题、状态栏上已经有主机，同一件事说三遍的代价是永久吃掉 24px
    ///（见 `ssh-terminal-ui-review.md` §1 的分带账本）。
    ///
    /// 宽度只包住按钮（`flex_none` + 右对齐），这样它压在画布右上方时遮住的是
    /// 右上角那片通常是空白的区域，而不是左起第一列的提示符。
    pub(super) fn render_terminal_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let view_pin = self.view.clone();
        let font_size = self.font_size(cx);
        let disabled_down = font_size <= *FONT_SIZE_RANGE.start() as f32;
        let disabled_up = font_size >= *FONT_SIZE_RANGE.end() as f32;
        let pinned = self.toolbar_pinned;

        h_flex()
            .absolute()
            .top(px(TOOLBAR_HOTZONE))
            .right(tokens.spacing.sm)
            .items_center()
            .gap(tokens.spacing.xxs)
            .px(tokens.spacing.xs)
            .py(px(2.))
            .rounded(tokens.radius.md)
            .bg(extended.colors.chrome)
            .border_1()
            .border_color(extended.colors.hairline)
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
                    .child(selectable_text(format!("{font_size:.0} px"))),
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
            )
            .child({
                // 「钉住」是这一条工具条唯一的**状态**开关：`Flag` = 还没钉（点它钉住），
                // `Minimize2` = 已钉（点它收起）。文案说的是「点下去会发生什么」，
                // 与工具栏按钮的 aria 惯例一致。
                let icon = if pinned {
                    DuckIcon::Minimize2
                } else {
                    DuckIcon::Flag
                };
                let label = if pinned {
                    "取消固定"
                } else {
                    "固定工具条"
                };
                toolbar_action_button(
                    "terminal-toolbar-pin",
                    tr(cx, label),
                    Icon::new(icon).size(extended.icon.md),
                    pinned,
                    false,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view_pin.update(cx, |this, cx| {
                        this.toolbar_pinned = !this.toolbar_pinned;
                        // 钉住期间指针离开不该把它收起来，所以两态一起落定。
                        this.toolbar_hovered = false;
                        cx.notify();
                    });
                })
            })
            .into_any_element()
    }

    /// 终端内容区：画布 + （浮出的）工具条 + （浮出的）查找条。
    ///
    /// # 为什么两条都是浮层
    ///
    /// 竖向空间是终端页最稀缺的资源：固定 chrome（标题 40 + 标签 36 + 工具条 24 +
    /// 状态 28）在 14px 等宽字号下等于 **7.6 行**输出。工具条与查找条又是那种
    /// 「十分钟用一次、一次三秒」的东西，让它们常驻 24px 是拿最贵的资源换最少的便利。
    ///
    /// 浮层还顺带解决一个正确性问题：查找条此前在**布局里**，一开就触发 PTY 重排
    ///（`TIOCSWINSZ`，`SIGWINCH`）—— 终端里的 `vim` / `htop` 会整屏重画，看起来像
    /// 「一按 ⌘F 程序就抽了一下」。绝对定位不改变画布尺寸，也就不发窗口变更。
    pub(super) fn render_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let canvas = self.render_terminal_canvas(cx);
        let find_bar = self.render_find_bar(cx);
        let show_toolbar = self.toolbar_pinned || self.toolbar_hovered;
        // 工具条先建：它要 `&mut Context`，而 `active_theme` 借出的是 `&App` ——
        // 顺序反了就是一条「可变借用与不可变借用重叠」。
        let toolbar = self.render_terminal_toolbar(cx);
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view_hover = self.view.clone();

        div()
            .relative()
            .size_full()
            .min_h_0()
            .bg(tokens.colors.background)
            .child(canvas)
            // 查找条：右上角浮层，标称 420 宽、被窗口压住时收到内容宽度 ——
            // `max_w(relative(1.))` 以画布宽为上限，窄窗口不再向左溢出。
            .when_some(find_bar, |this, bar| {
                this.child(
                    div()
                        .absolute()
                        .top(tokens.spacing.xs)
                        .right(tokens.spacing.sm)
                        .w(px(FIND_BAR_WIDTH))
                        .max_w(relative(1.))
                        .min_w_0()
                        .child(bar),
                )
            })
            // 工具条：顶部 6px 的**热区** + 悬停/钉住时展开。
            //
            // 热区必须单独存在：工具条隐藏时整块高度为 0，没有它就没有任何东西
            // 接收 `on_hover`，指针永远唤不出工具条。
            .child(
                v_flex()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .child(
                        div()
                            .id("terminal-toolbar-hotzone")
                            .w_full()
                            .h(px(TOOLBAR_HOTZONE))
                            .flex_none()
                            .on_hover(move |hovered: &bool, _, cx| {
                                let hovered = *hovered;
                                let _ = view_hover.update(cx, |this, cx| {
                                    if this.toolbar_hovered != hovered {
                                        this.toolbar_hovered = hovered;
                                        cx.notify();
                                    }
                                });
                            }),
                    )
                    .when(show_toolbar, |this| this.child(toolbar)),
            )
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
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                // 浮层形态：四边圆角 + 一圈细边，像一张压在内容上的卡片 ——
                // 只有下边框会让它看起来仍是「布局里的一条」。
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(extended.colors.hairline)
                .bg(extended.colors.chrome)
                .child(
                    Icon::new(DuckIcon::Search)
                        .size(extended.icon.md)
                        .text_color(tokens.colors.muted_foreground),
                )
                .child(
                    // 输入框吃剩余宽度（封顶 176）：查找条被窗口压窄时先收输入框，
                    // 计数与三个按钮保持完整。
                    div().flex_1().min_w_0().max_w(px(FIND_INPUT_WIDTH)).child(
                        Input::new(&find.input)
                            .appearance(false)
                            .with_size(Size::Small)
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .w_full(),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .min_w_0()
                        .child(meta_text(SharedString::from(counter), cx)),
                )
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
        // 只是套了条可拖动的命中带。右窗格不再有自己的名牌 —— 它以一枚窗格标签
        // 住进顶部标签栏的右分区（`render_tabs` 分屏分支 + `render_pane_tab`），
        // 与左半的会话标签保持一行。
        let left = self.render_terminal_pane(&tab_id, kind, false, cx);
        let right = self.render_terminal_pane(&split, split_kind, true, cx);
        h_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            // 左栏宽度 = 记忆占比（默认各半）；右栏 `flex_1` 吃剩余空间。
            // 拖隔离栏改占比，两块画布各自按新宽度重排（`apply_canvas_bounds`）。
            // 标签栏分屏分支用**同一套弹性结构**（同比例左区 + 同宽隔条占位 +
            // `flex_1` 右区），窗格标签因此与右窗格上下对齐。
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
}
