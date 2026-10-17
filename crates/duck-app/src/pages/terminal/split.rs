//! 分屏：开 / 合分屏、窗格比例与分隔条拖拽、拖放落点判定与窗格渲染。

use super::*;

impl TerminalPage {
    /// 某一侧目录面板的只读引用。
    pub(super) fn pane(&self, side: Side) -> &PaneState {
        &self.panes[side.index()]
    }

    /// 分屏 / 取消分屏当前标签。
    ///
    /// SFTP 标签没有 PTY（内容是一对文件面板），不参与分屏。
    ///
    /// 第二个窗格起的是**一条新会话**：本地标签再开一个 shell，SSH 标签再开一条
    /// `ssh -t`（复用同一条 `ControlMaster` 物理连接，实际只多一个通道）。
    /// 会话本身不在这里建 —— 本函数只改标签，`ensure_sessions` 在本帧按「会话表里
    /// 有没有这个键」接手，与「点连接只建标签」是同一条规矩。
    ///
    /// `pub(crate)`：画布里的 `⌘D`（`terminal_canvas::SplitPane`）与命令面板都走这里，
    /// 与工具条那个图标共用一份状态。
    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.kind == TabKind::Sftp {
            return;
        }
        let kind = tab.kind;
        let id = tab.id.clone();
        match tab.split.take() {
            Some(split) => {
                tab.clear_split();
                // `tab` 的借用到此为止：收会话要走整个 `self`。
                self.close_split_session(&split, cx);
            }
            None => {
                tab.split = Some(data::split_session_key(&id));
                tab.split_owner = Some(data::split_owner_for(kind, &id));
            }
        }
        cx.notify();
    }

    /// 把当前标签分屏到**指定连接**（左栏连接右键菜单「在分屏中打开」）。
    ///
    /// 与 [`Self::toggle_split`] 共用同一份数据（`split` + `split_owner`），差别只在
    /// 目标连接：缺省（工具条 / `⌘D`）连的是「标签自己的连接」，这里连用户点的那条
    /// —— 本地 shell 与远端主机都行，两个窗格由此可以各连一台机器。
    ///
    /// 已分屏时再点：同一条连接 = 取消分屏（与工具条同语义）；另一条 = 换目标
    ///（旧窗格的会话就地收掉，新会话由 `ensure_sessions` 按新归属补起）。
    pub(super) fn open_split_with(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        // 前置判定先做完（只借不可变部分），下面各分支再按需拿可变借用。
        let Some(active) = self.tabs.get(self.active) else {
            return;
        };
        if active.kind == TabKind::Sftp {
            return;
        }
        // 目标必须是能连的东西：内置本地 shell 或一条真实档案（档案可能刚被删）。
        if connection_id != data::LOCAL_SHELL_ID && self.profile(connection_id).is_none() {
            return;
        }
        let split = data::split_session_key(&active.id);
        let already_split = active.split.is_some();
        let same_target = active.split_owner.as_deref() == Some(connection_id);
        if !already_split {
            let tab = &mut self.tabs[self.active];
            tab.split = Some(split);
            tab.split_owner = Some(connection_id.to_owned());
            cx.notify();
            return;
        }
        if same_target {
            self.tabs[self.active].clear_split();
            self.close_split_session(&split, cx);
            return;
        }
        // 换目标：旧窗格的会话就地收掉；新键还不在会话表里，`ensure_sessions` 本帧按
        // 新归属补起。焦点标记一并清掉 —— 旧窗格没了，下一帧键盘回到主窗格。
        let old = {
            let tab = &mut self.tabs[self.active];
            let old = tab.split.take();
            tab.split_owner = Some(connection_id.to_owned());
            old
        };
        if let Some(old) = old {
            self.sessions.remove(&old);
            if self.focus_pane.as_deref() == Some(old.as_str()) {
                self.focus_pane = None;
                self.focused_tab = None;
            }
        }
        cx.notify();
    }

    /// 收掉某个分屏窗格的会话；焦点正落在它上面时把键盘还给主窗格。
    pub(super) fn close_split_session(&mut self, key: &str, cx: &mut Context<Self>) {
        self.sessions.remove(key);
        if self.focus_pane.as_deref() == Some(key) {
            self.focus_pane = None;
            self.focused_tab = None;
        }
        cx.notify();
    }

    /// 把一个标签并入**当前标签**的分屏右窗格（拖拽投放的落点）。
    ///
    /// 目标永远取拖拽发生时画布上显示的那个标签（= 当时的活动标签）。被拖标签
    /// 从标签栏里消失、它的会话**整个搬过去**当右窗格 —— 与主流终端「把标签拖进
    /// 窗格」的手感一致。前提（调用方已保证，这里再挡一道）：
    ///
    /// - 双方都不是 SFTP 标签、都还没分屏（本应用的分屏就是两条会话，没有第三格）；
    /// - 被拖的不是目标自己（自己拖进自己没有意义）。
    pub(super) fn merge_tab_into_split(&mut self, dragged_id: &str, cx: &mut Context<Self>) {
        let Some(d_index) = self.tabs.iter().position(|tab| tab.id == dragged_id) else {
            return;
        };
        let t_index = self.active;
        if t_index == d_index || t_index >= self.tabs.len() {
            return;
        }
        {
            // 与投放区的显示条件同一套：任一方不合格就不动（正常路径根本走不到）。
            let dragged = &self.tabs[d_index];
            let target = &self.tabs[t_index];
            if dragged.split.is_some()
                || dragged.kind == TabKind::Sftp
                || target.split.is_some()
                || target.kind == TabKind::Sftp
            {
                return;
            }
        }
        // 分屏窗格的连接归属跟被拖标签走：本地标签映射到内置本地 shell，
        // SSH 标签就是它自己的连接 id —— 会话搬过去还是连原来的机器。
        let owner = match self.tabs[d_index].kind {
            TabKind::Local => data::LOCAL_SHELL_ID.to_owned(),
            TabKind::Ssh => dragged_id.to_owned(),
            TabKind::Sftp => return,
        };
        let split = data::split_session_key(self.tabs[t_index].id.as_str());
        // 被拖标签的会话换个键继续活（没起会话也不要紧：`ensure_sessions`
        // 会按归属在本帧补一条）。
        if let Some(live) = self.sessions.remove(dragged_id) {
            self.sessions.insert(split.clone(), live);
        }
        // 关被拖的标签：会话键已搬走，`close_tab_at` 删的是空键，不会杀掉谁；
        // 它同时把 `active` 指针调对，之后目标下标按「它被删过」换算。
        self.close_tab_at(d_index, cx);
        let t_index = if t_index > d_index {
            t_index - 1
        } else {
            t_index
        };
        let tab = &mut self.tabs[t_index];
        tab.split = Some(split.clone());
        tab.split_owner = Some(owner);
        // 会话归属变了（从独立标签搬进右窗格），它自己的重连计时按「归属变更」撤销；
        // **别的标签一概不动** —— 与 `activate` 同一条口径：后台标签的重连是
        // 有界退避，不该因为用户拖了个窗格就全被清掉。
        self.cancel_reconnect(&split);
        self.active = t_index;
        // 焦点交给新窗格：用户刚把它拖进来，接下来打字多半就是打给它。
        self.focus_pane = Some(split);
        self.focused_tab = None;
        cx.notify();
    }

    /// 当前双栏的左右占比（左栏宽 / 总宽）。没拖过 = 各半。
    pub(super) fn pane_ratio(&self) -> f32 {
        self.divider_ratio.unwrap_or(0.5)
    }

    /// 拖动隔离栏时的比例换算：指针横坐标 → 占比，两端各留 [`MIN_PANE_WIDTH`]。
    pub(super) fn on_pane_divider_drag(
        &mut self,
        event: &DragMoveEvent<PaneDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = event.bounds.size.width;
        if width <= px(0.) {
            return;
        }
        // 两端最小宽换算成占比边界；容器比两倍最小宽还窄时退回各半。
        let min_fraction = (MIN_PANE_WIDTH / width.as_f32()).min(0.5);
        let raw = ((event.event.position.x - event.bounds.left()).as_f32() / width.as_f32())
            .clamp(min_fraction, 1. - min_fraction);
        let changed = match self.divider_ratio {
            None => raw != 0.5,
            Some(current) => (current - raw).abs() > f32::EPSILON,
        };
        if changed {
            self.divider_ratio = Some(raw);
            cx.notify();
        }
    }

    /// 双栏之间**可拖动**的隔离栏（分屏双终端与 SFTP 双栏共用）。
    ///
    /// 视觉仍是那条 1px 发丝线，但外面套了一条透明命中带（`spacing.sm` 宽）：
    /// 1px 的东西点不中，主流实现（VS Code / 各终端分屏）都给隔离栏一条宽命中带。
    /// 悬停与拖动中亮强调色薄底（`hover_tint`，全仓悬停语言），指针变左右箭头；
    /// 比例记进 [`Self::divider_ratio`]，拖拽用 gpui 的 drag 事件 —— 指针拖出
    /// 命中带也持续跟踪，这是 `on_mouse_move` 做不到的。
    pub(super) fn render_pane_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        div()
            .id("pane-divider")
            .w(tokens.spacing.sm)
            .h_full()
            .flex_none()
            .flex()
            .items_stretch()
            .justify_center()
            .cursor(CursorStyle::ResizeLeftRight)
            // 悬停亮薄底提示「这里抓得住」；拖动中指针会离开命中带，
            // 悬停态靠不住 —— 用 `resizing_pane` 标记保持同款视觉。
            .when(self.resizing_pane, |this| {
                this.bg(extended.colors.hover_tint)
            })
            .hover(|style| style.bg(extended.colors.hover_tint))
            .on_drag(PaneDrag, |_, _, _, cx| cx.new(|_| PaneDragGhost))
            .child(
                // 中间那条 1px 发丝线：与原实现同宽同色，不引入第二套视觉。
                div()
                    .w(extended.stroke.thin)
                    .h_full()
                    .flex_none()
                    .bg(extended.colors.hairline),
            )
            .into_any_element()
    }

    /// 给画布包一层「拖拽并入分屏」的投放目标。
    ///
    /// 主流终端的手感：拖着一个标签划过画布、指针**越过中线**时右半屏亮出投放区，
    /// 松手后被拖的标签变成这块画布的分屏右窗格。所以落点判定与视觉共用同一条
    /// 中线：`on_drag_move` 只在指针进入右半时点亮 `split_drop_hover`，
    /// `on_drop` 只在它点亮时并入（左半屏松手 = 取消，什么都不发生）。
    pub(super) fn with_split_drop_target(
        &self,
        tab_id: &str,
        pane: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = self.view.clone();
        let hovered = self.split_drop_hover && self.split_can_accept(tab_id);
        let zone = self.split_drop_zone(hovered, cx);
        let mut container = div()
            .id(SharedString::from(format!("terminal-drop-{tab_id}")))
            .relative()
            .size_full()
            .min_h_0()
            .child(pane);
        if let Some(zone) = zone {
            container = container.child(zone);
        }
        container
            // 拖拽经过时的悬停跟踪：只按「是否过中线」翻转一个布尔，
            // 变了才 notify（这回调跟着每一次指针移动跑）。
            .on_drag_move::<TabDrag>({
                let view = view.clone();
                move |event: &DragMoveEvent<TabDrag>, _window, cx| {
                    let midline = event.bounds.left() + event.bounds.size.width / 2.;
                    let hovered = event.event.position.x >= midline;
                    let _ = view.update(cx, |this, cx| {
                        if this.split_drop_hover != hovered {
                            this.split_drop_hover = hovered;
                            cx.notify();
                        }
                    });
                }
            })
            // 松手：并入与否由 `finish_tab_drop` 按悬停状态裁决（左半屏松手 = 取消）。
            .on_drop::<TabDrag>({
                let view = view.clone();
                move |drag: &TabDrag, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.finish_tab_drop(drag, cx));
                }
            })
            .into_any_element()
    }

    /// 当前拖拽能否并入 `target_id` 这块画布（投放区显示与并入执行共用同一套判定）。
    pub(super) fn split_can_accept(&self, target_id: &str) -> bool {
        let Some(dragged_id) = self.dragging_tab.as_deref() else {
            return false;
        };
        if dragged_id == target_id {
            return false;
        }
        // 双方都得是「还没分屏的终端标签」：本应用的分屏是两条会话，没有第三格；
        // SFTP 标签没有 PTY，不参与。
        let acceptable = |tab: &SessionTab| tab.split.is_none() && tab.kind != TabKind::Sftp;
        let Some(dragged) = self.tabs.iter().find(|tab| tab.id == dragged_id) else {
            return false;
        };
        let Some(target) = self.tabs.iter().find(|tab| tab.id == target_id) else {
            return false;
        };
        acceptable(dragged) && acceptable(target)
    }

    /// 右半屏的投放区视觉（虚位以待的一块高亮框）。
    ///
    /// 返回 `None` = 当前没有进行中的标签拖拽，或这块画布收不了 —— 什么都不画，
    /// 画布照常交互。悬停态用强调色薄底 + 强调色文字，与全仓悬停语言一致。
    pub(super) fn split_drop_zone(
        &self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // 只有**标签拖拽**才亮投放区：隔离栏拖拽同样是 active drag（载荷是
        // `PaneDrag`），但那是调宽度的手势，不该把「松开并入分屏」的框亮出来。
        // `dragging_tab` 由标签拖拽源登记、`on_drop` 清除，比 `has_active_drag`
        // （不区分载荷类型）更准。
        self.dragging_tab.as_ref()?;
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        // 投放区用主题的专用语义色：`drag_border`（拖拽指示线）描边、
        // `drop_target`（拖放目标区）做悬停底色 —— 不另造第二套拖放视觉。
        let (border, fill, label_color) = if hovered {
            (
                extended.colors.drag_border,
                extended.colors.drop_target,
                extended.colors.accent_text,
            )
        } else {
            (
                extended.colors.hairline,
                extended.colors.hover_tint,
                tokens.colors.muted_foreground,
            )
        };
        Some(
            div()
                .absolute()
                .top(tokens.spacing.sm)
                .bottom(tokens.spacing.sm)
                .right(tokens.spacing.sm)
                .w(relative(0.5))
                .flex()
                .items_center()
                .justify_center()
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(border)
                .when(hovered, |this| this.bg(fill))
                .child(meta_text(tr(cx, "松开并入分屏"), cx).text_color(label_color))
                .into_any_element(),
        )
    }

    /// 关闭分屏右窗格（右窗格名牌上的 ✕）。
    ///
    /// 右窗格只有这一条会话：关掉它分屏就不复存在 —— 收尾与工具条「取消分屏」
    /// 完全同一套（清标签数据 + 收会话 + 还焦点），只是入口不同。
    pub(super) fn close_split_pane(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(split) = tab.split.take() else {
            return;
        };
        tab.split_owner = None;
        // 名牌随分屏一起消失，不会有离开事件：悬停标记就地清掉，
        // 否则下次分屏时 ✕ 会替掉状态点，直到指针再进出一次。
        self.pane_header_hovered = false;
        // `tab` 的借用到此为止：收会话要走整个 `self`。
        self.close_split_session(&split, cx);
    }

    /// 分屏右窗格在标签栏里的「窗格标签」：图标 + 名称 + 状态点，悬停出关闭。
    ///
    /// 左半窗格的名字牌就是标签栏里的活动标签；右窗格是**另一条会话**，旧实现里
    /// 只有一条画布内的小名牌 —— 与顶部标签栏不在一行。现在分屏时标签栏本身按
    /// 窗格分区（见 `render_tabs` 的分屏分支），右窗格的名牌以一枚与会话标签同
    /// 节奏的芯片住进同一行：同高、同圆角、同状态点/✕ 槽位。✕ 关掉的就是这条
    /// 分屏会话（[`Self::close_split_pane`]）—— 右窗格只有一条会话，关掉即取消
    /// 分屏；点芯片本体把键盘焦点交给右窗格（与拖拽并入同一条口径）。
    pub(super) fn render_pane_tab(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.active_tab()?;
        let split = tab.split.clone()?;
        let kind = tab.split_kind().unwrap_or(tab.kind);
        // 名牌数据：连接归属决定显示名（本地 shell / 档案显示名），状态点跟会话表
        // 走。档案刚被删时如实写「终端会话」。
        let owner = tab
            .split_owner
            .clone()
            .unwrap_or_else(|| data::split_owner_for(tab.kind, &tab.id));
        let title = self
            .connection_entry(&owner)
            .map(|entry| entry.label)
            .unwrap_or_else(|| tr(cx, "终端会话").to_owned());
        let status = self.link_status(&split);
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view_click = self.view.clone();
        let view_hover = self.view.clone();
        let view_close = self.view.clone();
        let hovered = self.pane_header_hovered;
        let close = toolbar_action_button(
            "terminal-pane-close",
            tr(cx, "关闭分屏"),
            Icon::new(DuckIcon::X).size(extended.icon.sm),
            false,
            false,
            cx,
        )
        .w(px(TAB_CLOSE_SLOT))
        .on_click(move |_, _, cx| {
            // 关闭按钮在名牌内部：不掐断冒泡会顺带触发名牌上的别的手势。
            cx.stop_propagation();
            let _ = view_close.update(cx, |this, cx| this.close_split_pane(cx));
        });

        Some(
            h_flex()
                .id("terminal-pane-tab")
                .relative()
                .h(theme.density().control)
                .min_w_0()
                .max_w(px(TAB_MAX_WIDTH))
                .items_center()
                .gap(tokens.spacing.xs)
                .pl(tokens.spacing.sm)
                .pr(tokens.spacing.xs)
                .rounded(tokens.radius.md)
                .cursor_pointer()
                .text_color(tokens.colors.foreground)
                .text_size(tokens.typography.sm.size)
                .role(Role::Button)
                .aria_label(title.clone())
                .hover(|style| style.bg(extended.colors.hover_tint))
                .on_click({
                    let split = split.clone();
                    move |_, _, cx| {
                        let _ = view_click.update(cx, |this, cx| {
                            this.focus_pane = Some(split.clone());
                            this.focused_tab = None;
                            cx.notify();
                        });
                    }
                })
                .on_hover(move |hovered: &bool, _, cx| {
                    let hovered = *hovered;
                    let _ = view_hover.update(cx, |this, cx| {
                        if this.pane_header_hovered != hovered {
                            this.pane_header_hovered = hovered;
                            cx.notify();
                        }
                    });
                })
                .child(
                    Icon::new(kind.icon())
                        .size(extended.icon.md)
                        .text_color(extended.colors.accent_text),
                )
                .child(div().min_w_0().truncate().child(title))
                .child(if hovered {
                    // 与标签页同一套槽位语言：悬停换成 ✕，不悬停仍是状态点。
                    div()
                        .w(px(TAB_CLOSE_SLOT))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(close)
                } else {
                    div()
                        .w(px(TAB_CLOSE_SLOT))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .size(px(STATUS_DOT_SIZE))
                                .rounded_full()
                                .bg(status.color(cx)),
                        )
                })
                .into_any_element(),
        )
    }

    /// 一个终端窗格：会话画布（断线提示由画布自己画，见 `render_terminal_session`）。
    ///
    /// `split` 只影响无障碍名字 —— 两个同名「终端」的输入区对读屏是一团迷雾。
    /// `_kind` 保留在签名里是为了让调用点读起来仍然「按标签类型渲染」：决定
    /// 「断了要不要提示」的是会话自己的状态，不是标签类型（SFTP 标签也可能有会话）。
    pub(super) fn render_terminal_pane(
        &self,
        key: &str,
        _kind: TabKind,
        split: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.sessions.get(key) {
            Some(live) => {
                // 断线提示**不在这一层**：它是画布内部的一条绝对定位浮层
                //（`pages::terminal_canvas` 的 `reconnect` 那段），与画布共享同一个
                // 原点、不参与布局。此前这里还挂了一条「已断开 + 重新连接」的兄弟条，
                // 结果是同一件事在屏幕上说了两遍、文案与按钮语义还不一样。
                let _ = live;
                self.render_terminal_session(key, live, split, cx)
            }
            // 会话没起来（PTY 创建失败、档案已删）：如实说明，不留一块空白底。
            None => self.render_terminal_failure(cx),
        }
    }
}
