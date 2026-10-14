//! 设置页「审计」。

use super::*;

impl SettingsWindowView {
    /// 按当前筛选重查审计记录，刷新快照。
    ///
    /// ⚠️ 审计的写是**去抖**的（400ms）：不 flush 就查，刚记下的那条可能还没落盘。
    /// 所以这里先落盘再读 —— 这一页的读只有「进页 / 改筛选 / 点刷新」三个时机，
    /// 一次 flush 的代价换来的是「看到的永远是完整流水」。
    pub(super) fn reload_audit(&mut self, cx: &mut Context<Self>) {
        duck_store::flush(cx);
        self.audit_rows = crate::audit::query(cx, &self.audit_filter());
    }

    /// 当前筛选条件。三项各自独立：类别、只看问题、只看某一次错误。
    pub(super) fn audit_filter(&self) -> duck_store::AuditFilter {
        duck_store::AuditFilter {
            kind: self.audit_kind.as_ref().map(|kind| kind.to_string()),
            problems_only: self.audit_problems_only,
            trace_id: self.audit_trace.as_ref().map(|trace| trace.to_string()),
            scope: None,
            limit: duck_store::AuditFilter::DEFAULT_LIMIT,
        }
    }

    /// 「审计日志」页：筛选 + 流水列表。
    pub(super) fn audit_body(&self, cx: &mut Context<Self>) -> Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();

        // 类别下拉：首项「全部」用空串当值（`AuditFilter.kind = None` 的界面形态）。
        let mut kind_options: Vec<(SharedString, SharedString)> = vec![(
            SharedString::from(""),
            SharedString::from(tr(cx, "全部类别")),
        )];
        kind_options.extend(crate::audit::AuditKind::ALL.into_iter().map(|kind| {
            (
                SharedString::from(kind.key()),
                SharedString::from(kind.label(cx)),
            )
        }));
        let current_kind = self
            .audit_kind
            .clone()
            .unwrap_or_else(|| SharedString::from(""));
        let kind_row = option_row(
            tr(cx, "类别"),
            Some(tr(cx, "连接、查询、写入这类事件各自成一行流水。").into()),
            dropdown_select(
                "duck-settings-audit-kind",
                kind_options,
                current_kind,
                // `dropdown_select` 的回调是**按值**收 `SharedString`，所以走 `processor`
                //（`listener` 给的是 `&E`，形状对不上）。
                cx.processor(|this, value: SharedString, _, cx| {
                    this.audit_kind = (!value.is_empty()).then(|| value.clone());
                    this.reload_audit(cx);
                    cx.notify();
                }),
                cx,
            ),
            cx,
        );

        let problems_row = option_row(
            tr(cx, "只看问题"),
            Some(
                tr(
                    cx,
                    "成功以外的记录（失败、被拦下、已取消）。被拦下不是错误，是策略生效。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-audit-problems",
                self.audit_problems_only,
                cx.listener(|this, on: &bool, _, cx| {
                    this.audit_problems_only = *on;
                    this.reload_audit(cx);
                    cx.notify();
                }),
            ),
            cx,
        );

        // 「只看这一次」只在**有**关联 id 时出现：它是外部投递进来的筛选（点主窗错误徽标），
        // 不是一个常驻条件 —— 常驻一个空的输入框会让人以为「这一栏还能筛别的」。
        let trace_row = self.audit_trace.as_ref().map(|trace| {
            option_row(
                tr(cx, "关联 id"),
                Some(tr(cx, "只看这一次错误。同一个 id 也会出现在那条失败提示里。").into()),
                h_flex()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(meta_text(trace.clone(), cx))
                    .child(
                        button(
                            "duck-settings-audit-trace-clear",
                            tr(cx, "清除"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.audit_trace = None;
                            this.reload_audit(cx);
                            cx.notify();
                        })),
                    ),
                cx,
            )
            .into_any_element()
        });

        let keep_options: Vec<(SharedString, SharedString)> = crate::audit::AUDIT_KEEP_CHOICES
            .iter()
            .map(|keep| {
                (
                    SharedString::from(keep.to_string()),
                    SharedString::from(trf(cx, "{} 条", &[&keep.to_string()])),
                )
            })
            .collect();
        let keep_row = option_row(
            tr(cx, "保留条数"),
            Some(
                tr(
                    cx,
                    "超出上限的旧记录会被自动删除，调小会立刻生效；要长期留证就把上限调大。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-audit-keep",
                keep_options,
                SharedString::from(self.audit_keep.to_string()),
                cx.processor(|this, value: SharedString, _, cx| {
                    // 下拉回传的是档位的稳定值（条数字符串）；非法值理论上来不了，丢掉即可。
                    let Ok(keep) = value.as_ref().parse::<usize>() else {
                        return;
                    };
                    crate::audit::set_keep_limit(cx, keep);
                    // 落盘 + 裁剪都是去抖的：顺序必须是「先让它们做完，再读列表」，
                    // 否则读到的还是按旧上限裁过的结果。
                    duck_store::flush(cx);
                    this.audit_keep = crate::audit::keep_limit(cx);
                    this.reload_audit(cx);
                    cx.notify();
                }),
                cx,
            ),
            cx,
        );

        let clear_row = option_row(
            tr(cx, "清空"),
            Some(
                tr(
                    cx,
                    "删掉全部流水。清空本身也会记一笔 —— 否则「谁把痕迹擦了」就查不出来了。",
                )
                .into(),
            ),
            button(
                "duck-settings-audit-clear",
                tr(cx, "清空审计"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                let count = this.audit_rows.len();
                if !duck_platform::confirm(
                    tr(cx, "清空审计日志？"),
                    &trf(
                        cx,
                        "当前筛选下的 {} 条记录会全部删除（整个表都会清空，不只是筛出来的这些）。",
                        &[&count.to_string()],
                    ),
                ) {
                    return;
                }
                duck_store::audit_clear(cx);
                // 清空后立刻记一笔：同批里「清空」先于写入执行，所以这一条不会被自己擦掉。
                crate::audit::record(
                    cx,
                    duck_store::AuditEntry::new(
                        crate::audit::AuditKind::Query.key(),
                        crate::audit::action::CLEAR,
                        duck_store::AuditOutcome::Ok,
                        "",
                        trf(cx, "清空审计日志（原有 {} 条）", &[&count.to_string()]),
                    ),
                );
                // 这一笔的 `detail` 是中文：它由**界面**产生、只给人看，不参与筛选与比较
                // （筛选走 kind / action / outcome / trace 这些英文键）。
                this.reload_audit(cx);
                cx.notify();
                duck_components::notify::success(tr(cx, "审计日志已清空"), window, cx);
            })),
            cx,
        );

        let mut filter_rows: Vec<AnyElement> =
            vec![kind_row.into_any_element(), problems_row.into_any_element()];
        filter_rows.extend(trace_row);
        filter_rows.push(keep_row.into_any_element());
        filter_rows.push(clear_row.into_any_element());

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(tokens.spacing.xl)
            .child(settings_section(
                Some(tr(cx, "筛选").into()),
                filter_rows,
                cx,
            ))
            .child(settings_section(
                Some(tr(cx, "流水").into()),
                [if self.audit_rows.is_empty() {
                    Self::audit_empty_state(self.audit_filter().is_empty(), self.audit_keep, cx)
                        .into_any_element()
                } else {
                    option_group(
                        self.audit_rows
                            .iter()
                            .map(|row| Self::audit_row(row, &extended, &tokens, cx)),
                        cx,
                    )
                    .into_any_element()
                }],
                cx,
            ))
    }

    /// 空态：分清「一条都没有」与「筛出来是空的」。
    ///
    /// 这两种空必须说不同的话：前者说明审计还没记下任何事（或刚被清空），
    /// 后者说明筛选太窄 —— 一句「暂无记录」会把用户引向错误的方向。
    pub(super) fn audit_empty_state(filtered: bool, keep: usize, cx: &App) -> AnyElement {
        if filtered {
            content::empty_state(
                DuckIcon::ListFilter,
                tr(cx, "没有符合条件的记录"),
                Some(tr(cx, "放宽筛选，或清掉「只看问题 / 关联 id」再看看。").into()),
                None,
                cx,
            )
            .into_any_element()
        } else {
            content::empty_state(
                DuckIcon::ShieldCheck,
                tr(cx, "还没有记录"),
                Some(
                    trf(
                        cx,
                        "连接、执行语句、写入、浏览与插件调用都会记在这里（最多保留 {} 条）。",
                        &[&keep.to_string()],
                    )
                    .into(),
                ),
                None,
                cx,
            )
            .into_any_element()
        }
    }

    /// 流水的一行：时间 · 类别 · 动作 · 结果 · 摘要（+ 关联 id）。
    ///
    /// 列宽固定而不是流式：流水是**要竖着扫**的（找「哪几行是连接」），
    /// 让每列跟着内容走会让相同的字段落在不同的水平位置上，扫视就断了。
    pub(super) fn audit_row(
        row: &duck_store::AuditRow,
        extended: &duck_theme::ExtendedTokens,
        tokens: &duck_theme::SemanticThemeTokens,
        cx: &App,
    ) -> AnyElement {
        let now = duck_store::now_seconds();
        let when = tr_relative_time(cx, now - row.at, UnderMinute::Seconds);
        let kind = crate::audit::AuditKind::from_key(&row.kind)
            .map_or_else(|| row.kind.clone(), |kind| kind.label(cx).to_owned());
        // 结果色分四档：被拦下走警示色（策略生效，不是故障），取消走三级文字
        //（用户自己按的，最不需要被强调）。
        let color = match duck_store::AuditOutcome::from_key(&row.outcome) {
            Some(duck_store::AuditOutcome::Ok) => extended.colors.success,
            Some(duck_store::AuditOutcome::Failed) => extended.colors.status_failed,
            Some(duck_store::AuditOutcome::Blocked) => extended.colors.warning,
            _ => extended.colors.text_tertiary,
        };

        let mut detail = h_flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(body_text(row.detail.clone(), cx)),
            );
        if !row.trace_id.is_empty() {
            detail = detail.child(content::accent_tag(row.trace_id.clone(), cx));
        }

        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.md)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.xs)
            .child(
                div()
                    .flex_none()
                    .w(px(AUDIT_TIME_WIDTH))
                    .child(meta_text(when, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(AUDIT_KIND_WIDTH))
                    .child(meta_text(kind, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(AUDIT_ACTION_WIDTH))
                    .child(meta_text(crate::audit::action_label(&row.action, cx), cx)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(AUDIT_OUTCOME_WIDTH))
                    .child(content::status_cell(
                        color,
                        crate::audit::outcome_label(&row.outcome, cx),
                        cx,
                    )),
            )
            .child(detail)
            .into_any_element()
    }

    /// 横幅档位的中文标签（`kv` 里存的是 [`notify::BannerLevel::wire`]）。
    pub(super) fn banner_label(level: notify::BannerLevel, cx: &App) -> SharedString {
        match level {
            notify::BannerLevel::All => tr(cx, "全部提示").into(),
            notify::BannerLevel::WarningsAndErrors => tr(cx, "仅警告与错误").into(),
            notify::BannerLevel::Off => tr(cx, "全部关掉").into(),
        }
    }
}
