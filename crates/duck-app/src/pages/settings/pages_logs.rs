//! 设置页「日志」：保留策略与查看入口。

use super::*;

/// 日志清理结果的提示文案：删了就说删了几个，一个没删也明说（沉默会让人以为按钮没生效）。
pub(super) fn pruned_message(removed: usize, cx: &App) -> SharedString {
    if removed == 0 {
        SharedString::from(tr(cx, "没有需要清理的日志。"))
    } else {
        SharedString::from(trf(cx, "已删除 {} 个过期日志。", &[&removed.to_string()]))
    }
}

/// 一个 MiB 的字节数。只用于把 [`duck_platform`] 的日志上限常量写成给人看的话。
pub(super) const MIB: u64 = 1024 * 1024;

impl SettingsWindowView {
    /// 「日志」页的查看器，按需创建（见字段注释）。
    ///
    /// 建好后订阅它：查看器每秒扫一次目录、长了就 `notify`，这里跟着把占用数字抄过来，
    /// 数字才不是「进这一页时的快照」。`notify` 只在真有变化时发（见
    /// `LogView::reload`），所以订阅不会让设置窗口每秒白重绘一次。
    pub(super) fn logs_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<LogView> {
        if let Some(view) = &self.logs_view {
            return view.clone();
        }
        let view = cx.new(|cx| LogView::embedded(window, cx));
        cx.observe(&view, |this, view, cx| {
            let summary = view.read(cx).summary();
            if this.logs_summary != summary {
                this.logs_summary = summary;
                cx.notify();
            }
        })
        .detach();
        self.logs_summary = view.read(cx).summary();
        self.logs_view = Some(view.clone());
        view
    }

    /// 「日志」页顶部的保留设置：保留时长下拉 + 立即清理。
    ///
    /// 只出现在设置页宿主里；独立日志窗口保持「纯查看器」——那里没有配置的位置。
    pub(super) fn logs_retention_section(&self, cx: &mut Context<Self>) -> Div {
        let options: Vec<(SharedString, SharedString)> = crate::pages::logs::RETENTION_CHOICES
            .iter()
            .map(|days| {
                let label = if *days == duck_platform::LOG_RETENTION_FOREVER {
                    SharedString::from(tr(cx, "永久保留"))
                } else {
                    SharedString::from(trf(cx, "{} 天", &[&days.to_string()]))
                };
                (SharedString::from(days.to_string()), label)
            })
            .collect();

        let this = cx.entity().downgrade();
        let retention_row = option_row(
            tr(cx, "保留时长"),
            Some(
                tr(
                    cx,
                    "超出保留期的日志会被自动删除；日志按天滚动，一天一个文件。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-log-retention",
                options,
                SharedString::from(self.log_retention_days.to_string()),
                move |value, window, cx| {
                    // 下拉回传的是档位的稳定值（天数字符串）；非法值理论上不会出现，丢掉即可。
                    let Ok(days) = value.as_ref().parse::<u32>() else {
                        return;
                    };
                    if !crate::pages::logs::RETENTION_CHOICES.contains(&days) {
                        return;
                    }
                    crate::pages::logs::set_retention_days(cx, days);
                    // 改成更短的保留期要立刻见效，所以当场按新值清一次：`kv_put` 是异步
                    // 落盘的，回读 `retention_days` 只会拿到旧值。
                    let removed = crate::pages::logs::prune_with(days);
                    if let Ok(()) = this.update(cx, |this, cx| {
                        this.log_retention_days = days;
                        this.refresh_logs_view(cx);
                        cx.notify();
                    }) {}
                    duck_components::notify::success(pruned_message(removed, cx), window, cx);
                },
                cx,
            ),
            cx,
        );

        let clean_row = option_row(
            tr(cx, "立即清理"),
            Some(tr(cx, "按上面的保留时长删掉旧日志，不等下一次自动清理。").into()),
            button(
                "duck-settings-log-prune",
                tr(cx, "清理"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                let removed = crate::pages::logs::prune_with(this.log_retention_days);
                this.refresh_logs_view(cx);
                duck_components::notify::success(pruned_message(removed, cx), window, cx);
            })),
            cx,
        );

        // 下面两项只读：两个数字都是**已生效的**兜底值，不是可调的设置。
        // 摆出来是因为「按天滚动」听起来无害，实际单日、整目录各有一条上限；
        // 不写出来，用户只能靠磁盘满了才发现。
        let segments = duck_platform::LOG_SEGMENTS_PER_DAY.to_string();
        let directory_cap = format!("{} MB", duck_platform::LOG_DIR_MAX_BYTES / MIB);
        let per_file_row = option_row(
            tr(cx, "单文件上限"),
            Some(
                trf(
                    cx,
                    "写满就轮转到下一段；一天最多留 {} 段，目录总量兜底 {}。",
                    &[&segments, &directory_cap],
                )
                .into(),
            ),
            meta_text(
                format!("{} MB", duck_platform::LOG_FILE_MAX_BYTES / MIB),
                cx,
            ),
            cx,
        );

        let (files, bytes) = self.logs_summary;
        let usage_row = option_row(
            tr(cx, "当前占用"),
            Some(tr(cx, "日志目录里所有 .log 的总大小。").into()),
            meta_text(
                trf(
                    cx,
                    "{} 个文件 · {}",
                    &[&number(files as i64), &format_bytes(bytes)],
                ),
                cx,
            ),
            cx,
        );

        settings_section(
            Some(tr(cx, "日志保留").into()),
            [
                retention_row.into_any_element(),
                clean_row.into_any_element(),
                per_file_row.into_any_element(),
                usage_row.into_any_element(),
            ],
            cx,
        )
    }

    /// 让日志查看器立刻重扫目录：清理刚删掉的文件不该在列表里留到下一次轮询。
    ///
    /// 查看器是**懒建**的（见字段注释）：没进过日志页就没有它，也就没有要刷的东西。
    pub(super) fn refresh_logs_view(&self, cx: &mut Context<Self>) {
        if let Some(view) = &self.logs_view {
            view.update(cx, |view, cx| view.rescan(cx));
        }
    }
}
