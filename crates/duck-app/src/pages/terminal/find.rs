//! 终端查找：开 / 关查找条、结果刷新与高亮、上下跳转与节流刷新。

use super::*;

impl TerminalPage {
    /// 打开（或重新聚焦）搜索条。目标 = 用户最后点过的窗格，没有就退回当前标签。
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self
            .focus_pane
            .clone()
            .filter(|key| {
                self.sessions
                    .get(key)
                    .and_then(TerminalSession::session)
                    .is_some()
            })
            .or_else(|| {
                self.active_tab().map(|tab| tab.id.clone()).filter(|key| {
                    self.sessions
                        .get(key)
                        .and_then(TerminalSession::session)
                        .is_some()
                })
            });
        // 没有可搜的画布（空态、SFTP 标签、PTY 起不来）：什么都不做。
        let Some(session) = session else {
            return;
        };

        if let Some(find) = self.find.as_mut() {
            // 已经开着：换目标、重新聚焦，**保留**已敲的搜索词（切窗格不该清空输入）。
            find.session = session;
            let handle = find.input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
            self.refresh_find(cx);
            return;
        }

        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(cx, "在缓冲区中查找…")));
        let subscriptions = vec![cx.subscribe(
            &input,
            |this: &mut Self, _input: Entity<InputState>, event: &InputEvent, cx| match event {
                InputEvent::Change => this.refresh_find(cx),
                // Enter / Shift-Enter 在命中之间前后走。
                InputEvent::PressEnter { shift, .. } => {
                    this.step_find(if *shift { -1 } else { 1 }, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        )];
        let handle = input.read(cx).focus_handle(cx);
        self.find = Some(FindState {
            input,
            session,
            matches: Vec::new(),
            current: 0,
            refreshed: Instant::now(),
            _subscriptions: subscriptions,
        });
        window.focus(&handle, cx);
        cx.notify();
    }

    /// 收起搜索条（并把键盘交还画布 —— 否则焦点会留在一个已经消失的输入框上）。
    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(find) = self.find.take() else {
            return;
        };
        let handle = self
            .sessions
            .get(&find.session)
            .map(TerminalSession::focus_handle);
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// 用当前搜索词重算命中。
    ///
    /// 「当前命中」下标在结果变短时钳到最后一条：删搜索词的过程中命中数会掉，
    /// 不钳就会索引越界（表现为高亮消失或 panic）。
    pub(super) fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_ref() else {
            return;
        };
        let key = find.session.clone();
        let query = find.input.read(cx).value().to_string();
        let matches: Vec<(i32, usize, i32, usize)> = self
            .sessions
            .get(&key)
            .and_then(TerminalSession::session)
            .map(|session| {
                session
                    .search_matches(&query)
                    .into_iter()
                    .map(|(start, end)| (start.line.0, start.column.0, end.line.0, end.column.0))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(find) = self.find.as_mut() {
            find.matches = matches;
            find.current = find.current.min(find.matches.len().saturating_sub(1));
            find.refreshed = Instant::now();
        }
        cx.notify();
    }

    /// 搜索命中 → 可见行上的高亮条带：`(视口行, 首列, 末列, 是否当前命中)`。
    ///
    /// 与选区条带（`terminal_canvas::selection_bands`）同一套「网格行 → 视口行」换算，
    /// 差别只在来源：那边来自内核的 `SelectionRange`，这边来自搜索命中的网格区间。
    /// 多出来的 `bool` 是给渲染层的：当前命中要画得比其余命中更醒目，否则
    /// 「按 Enter 走了一圈」在画面上完全看不出来。
    pub(crate) fn find_highlight(
        &self,
        tab_id: &str,
        offset: i32,
        screen_lines: usize,
        columns: usize,
    ) -> Vec<(usize, usize, usize, bool)> {
        let Some(find) = self.find.as_ref().filter(|find| find.session == tab_id) else {
            return Vec::new();
        };
        if columns == 0 || screen_lines == 0 {
            return Vec::new();
        }
        let last_column = columns - 1;
        let mut bands = Vec::with_capacity(find.matches.len());
        for (index, &(start_line, start_column, end_line, end_column)) in
            find.matches.iter().enumerate()
        {
            let current = index == find.current;
            // 与视口求交：往上翻着历史搜、或命中跨了很多行时，两端都会出界。
            let top = (start_line + offset).max(0);
            let bottom = (end_line + offset).min(screen_lines as i32 - 1);
            if bottom < top {
                continue;
            }
            for row in top..=bottom {
                // 跨行命中的中间几行整行铺满，首行从命中起点铺到行尾、末行从行首铺到终点。
                let first = if row <= start_line + offset {
                    start_column
                } else {
                    0
                };
                let last = if row >= end_line + offset {
                    end_column
                } else {
                    last_column
                };
                let first = first.min(last);
                let last = last.min(last_column);
                if first > last {
                    continue;
                }
                bands.push((row as usize, first, last, current));
            }
        }
        bands
    }

    /// 在命中之间前后走一格（环形），并把视口滚到那一处。
    pub(super) fn step_find(&mut self, delta: i32, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len() as i32;
        find.current = (find.current as i32 + delta).rem_euclid(len) as usize;
        let line = find.matches[find.current].0;
        let key = find.session.clone();
        let Some(session) = self
            .sessions
            .get(&key)
            .and_then(TerminalSession::session)
            .cloned()
        else {
            return;
        };
        // 把命中的那一行挪到**视口正中**：先算「需要的 display_offset」再求差值。
        // 视口行 = 网格行 + display_offset（见 `terminal_canvas`），要让它在正中，
        // 就得 offset = 半屏高 - 网格行。
        let current = session
            .try_with_term(|term| term.renderable_content().display_offset as i32)
            .unwrap_or_default();
        let rows = i32::from(session.grid_size().1);
        let desired = (rows / 2) - line;
        if desired != current {
            session.scroll(desired - current);
        }
        cx.notify();
    }

    /// 终端有输出时（按 [`FIND_REFRESH_MIN`] 节流）重算命中。
    ///
    /// 输出会不断把旧命中整体挪位（回滚区行号是相对的），不定期重算高亮就会框错字。
    pub(crate) fn maybe_refresh_find(&mut self, cx: &mut Context<Self>) {
        let due = self
            .find
            .as_ref()
            .is_some_and(|find| find.refreshed.elapsed() >= FIND_REFRESH_MIN);
        if due {
            self.refresh_find(cx);
        }
    }

    // ------------------------------------------------------------------
    // 远端文件系统改动：新建目录 / 删除 / 重命名。
    //
    // 三者都是**先把意图冻进 `RemoteMutation`，再丢到后台执行器上跑一个 ssh 进程**，
    // 回主线程后重列举远端面板并推一条通知。删除与重命名是破坏性动作，走确认框
    // （删除）或命名弹窗（重命名，见 `rename_remote_entry`）。
    // ------------------------------------------------------------------
}
