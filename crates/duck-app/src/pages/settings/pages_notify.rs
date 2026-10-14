//! 设置页「通知」。

use super::*;

impl SettingsWindowView {
    // ── 通知 ─────────────────────────────────────────────────────────────
    /// 「通知」页：两条通道的开关 + 三条试一下 + 历史概览。
    ///
    /// 三个按钮不是装饰：通知是**跨窗口投递**的（落在当前活跃窗口上），而这一页恰好
    /// 就开在一个窗口里——用户需要能在自己的环境里确认「到底弹不弹、弹在哪」，
    /// 否则关掉横幅之后唯一的判断依据就是「我觉得好像没弹」。
    pub(super) fn notify_body(&self, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        // 读**运行时**的通道档位，不读 `kv`：`kv_put` 是去抖落盘的（400ms），改完立刻
        // 回读拿到的是旧值 —— 下拉会在下一次重绘时弹回上一档，看起来就是「点了没反应」。
        // 运行时那份由 `settings::set_notify_*` 同步更新，与真正决定弹不弹的是同一个值。
        let channels = notify::channels(cx);

        let banners_row = option_row(
            tr(cx, "应用内横幅"),
            Some(
                tr(
                    cx,
                    "窗口右上角的提示条。无论哪一档，通知都会记入历史（顶栏铃铛）。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-notify-banners",
                notify::BannerLevel::ALL
                    .into_iter()
                    .map(|level| {
                        (
                            SharedString::from(level.wire()),
                            Self::banner_label(level, cx),
                        )
                    })
                    .collect::<Vec<_>>(),
                SharedString::from(channels.banners.wire()),
                cx.processor(|_, value: SharedString, _, cx| {
                    // 认不出来的值丢掉：下拉只回传上面那三个稳定值。
                    let Some(level) = notify::BannerLevel::from_wire(value.as_ref()) else {
                        return;
                    };
                    settings::set_notify_banners(cx, level);
                    // 没有本地快照要维护：上面那句已经把运行时档位改掉了，这次重绘读的就是它。
                    cx.notify();
                }),
                cx,
            ),
            cx,
        );

        let system_row = option_row(
            tr(cx, "系统通知"),
            Some(
                tr(
                    cx,
                    "投递到操作系统通知中心。需要已打包成应用并授权；开发构建下通常不显示。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-notify-system",
                channels.system,
                cx.listener(|_, on: &bool, _, cx| {
                    settings::set_notify_system(cx, *on);
                    cx.notify();
                }),
            ),
            cx,
        );

        // 三个按钮不是装饰：通知是**跨窗口投递**的（落在当前活跃窗口上），而这一页恰好
        // 就开在一个窗口里——用户需要能在自己的环境里确认「到底弹不弹、弹在哪」，
        // 否则关掉横幅之后唯一的判断依据就是「我觉得好像没弹」。
        //
        // 这一行**特意不写说明**：上面两行的说明已经说清了两条通道各是什么、去哪看，
        // 再重复一遍就成了噪音（`option_row_stacked` 的说明位本来就允许为空）。
        let try_row = option_row_stacked(
            tr(cx, "试一下"),
            None,
            h_flex()
                .w_full()
                .gap(tokens.spacing.sm)
                .flex_wrap()
                .child(
                    button(
                        "duck-settings-notify-toast",
                        tr(cx, "应用内提示"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::success(tr(cx, "这是一条应用内提示"), window, cx);
                    }),
                )
                .child(
                    button(
                        "duck-settings-notify-system-demo",
                        tr(cx, "系统通知"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::system(
                            crate::APP_NAME,
                            tr(cx, "这是一条系统通知"),
                            window,
                            cx,
                        );
                    }),
                )
                .child(
                    button(
                        "duck-settings-notify-both",
                        tr(cx, "两者同时"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        duck_components::notify::system_with_toast(
                            crate::APP_NAME,
                            tr(cx, "应用内 + 系统通知"),
                            window,
                            cx,
                        );
                    }),
                ),
            cx,
        );

        // 历史：这一页只报个数并给一个清空入口，**列表留给顶栏铃铛**——两处都渲染一份
        // 列表就得维护两条时间格式 / 空态 / 滚动路径，而它们没有任何理由不一样。
        let (total, unread) = (notify::history(cx).len(), notify::unread_count(cx));
        let history_row = option_row(
            tr(cx, "通知历史"),
            Some(tr(cx, "最近 100 条投递记录，只在本次运行内保留。").into()),
            h_flex()
                .gap(tokens.spacing.sm)
                .items_center()
                .child(meta_text(
                    trf(
                        cx,
                        "{} 条 · {} 条未读",
                        &[&total.to_string(), &unread.to_string()],
                    ),
                    cx,
                ))
                .child(
                    button(
                        "duck-settings-notify-clear",
                        tr(cx, "清空"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(total == 0)
                    // 清空只动 `notify` 的全局历史，不需要 `self`；但历史里那份快照
                    // （`total` / `unread`）是本次渲染算出来的，得靠 `cx.notify()` 重算。
                    .on_click(cx.listener(|_, _, _, cx| {
                        duck_components::notify::clear_history(cx);
                        cx.notify();
                    })),
                ),
            cx,
        );

        div().w_full().child(settings_section(
            Some(tr(cx, "通知").into()),
            [
                banners_row.into_any_element(),
                system_row.into_any_element(),
                try_row.into_any_element(),
                history_row.into_any_element(),
            ],
            cx,
        ))
    }

    // ── 审计日志 ──────────────────────────────────────────────────────────
    //
    // 这一页只做两件事：**缩范围**（筛选）与**看流水**（列表）。没有编辑、没有删除 ——
    // 唯一的破坏性动作是「清空」，而它自己也会在清空后立刻记一笔（见下面那段）。
}
