//! 连接编辑弹窗：六页签表单与其控件（含页签外壳与页脚）。

use super::*;

/// 「标签 + 输入框」的表单单元。
pub(super) fn editor_field(label: &'static str, input: &Entity<InputState>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    v_flex()
        .flex_1()
        .min_w_0()
        .gap(tokens.spacing.xxs)
        .child(field_label(tr(cx, label), cx))
        .child(input_shell(input, false, false, cx))
}

/// 弹窗里输入框的统一外壳：`Input` 关掉自带外观，套本仓的 hairline 圆角框。
///
/// `mask` 打开时带密码小眼睛（zedis 同款）；`disabled` 用于「档位存在但尚未接线」
/// 的灰显占位——此时不要造输入实体（会进焦点链），用 [`disabled_shell`]。
pub(super) fn input_shell(input: &Entity<InputState>, mask: bool, disabled: bool, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .h(theme.density().control)
        .w_full()
        .min_w_0()
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .when(disabled, |shell| shell.opacity(extended.opacity.disabled))
        .child({
            let input = Input::new(input)
                .appearance(false)
                .bordered(false)
                .disabled(disabled)
                .w_full();
            if mask { input.mask_toggle() } else { input }
        })
}

/// 未接线档位的灰显占位壳：空盒子 + 说明文字，不造输入实体（不进焦点链）。
pub(super) fn disabled_shell(placeholder: &'static str, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .h(theme.density().control)
        .w_full()
        .min_w_0()
        .px(tokens.spacing.sm)
        .flex()
        .items_center()
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .opacity(extended.opacity.disabled)
        .child(meta_text(tr(cx, placeholder), cx))
}

/// 单选档位行：一排薄底按钮（与状态栏 / 工具条的按钮族同一套外观）。
///
/// `on_pick` 收到 wire 值；调用方负责回写表单并 `window.refresh()`。
pub(super) fn choice_row(
    id_prefix: &'static str,
    options: &[(&'static str, &'static str)],
    current_wire: &str,
    on_pick: impl Fn(&str, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    let on_pick = Rc::new(on_pick);
    h_flex()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xxs)
        .children(options.iter().map(|(wire, label)| {
            let selected = *wire == current_wire;
            let wire = (*wire).to_owned();
            let on_pick = Rc::clone(&on_pick);
            button(
                SharedString::from(format!("{id_prefix}-{wire}")),
                tr(cx, label),
                if selected {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(move |_, window, cx| on_pick(&wire, window, cx))
        }))
}

/// 常规页签。
pub(super) fn render_tab_general(form: &Entity<EditForm>, cx: &App) -> Div {
    let name = form.read(cx).name.clone();
    let host = form.read(cx).host.clone();
    let port = form.read(cx).port.clone();
    let acl_user = form.read(cx).acl_user.clone();
    let password = form.read(cx).password.clone();
    let description = form.read(cx).description.clone();
    let group = form.read(cx).group.clone();

    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        // 名称单独一行并自带必填标记（与 zedis 的 `名称 *` 对齐）。
        .child(form_field(
            tr(cx, "名称 *"),
            input_shell(&name, false, false, cx),
            None,
            None,
            cx,
        ))
        // 主机 + 端口是同一个地址：一行，端口占 1/3（zedis 的 span 9/3）。
        .child(form_row(
            [
                form_field(
                    tr(cx, "主机 *"),
                    input_shell(&host, false, false, cx),
                    None,
                    None,
                    cx,
                )
                .flex_1()
                .min_w_0()
                .into_any_element(),
                form_field(
                    tr(cx, "端口"),
                    input_shell(&port, false, false, cx),
                    None,
                    None,
                    cx,
                )
                .flex_1()
                .min_w_0()
                .into_any_element(),
            ],
            cx,
        ))
        .child(form_field(
            tr(cx, "用户名"),
            input_shell(&acl_user, false, false, cx),
            None,
            Some(tr(cx, "仅用于 Redis 6.0+（ACL）；留空表示只用口令认证").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "密码"),
            div()
                .w_full()
                .child(
                    Input::new(&password)
                        .appearance(false)
                        .bordered(false)
                        .mask_toggle()
                        .w_full(),
                )
                .h(active_theme(cx).density().control)
                .px(active_theme(cx).tokens().spacing.sm)
                .rounded(active_theme(cx).components().input_radius)
                .border(active_theme(cx).extended().stroke.thin)
                .border_color(active_theme(cx).extended().colors.hairline)
                .bg(active_theme(cx).tokens().colors.background),
            None,
            Some(tr(cx, "加密存本机，不入连接档案").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "描述"),
            input_shell(&description, false, false, cx),
            None,
            None,
            cx,
        ))
        .child(form_field(
            tr(cx, "分组"),
            input_shell(&group, false, false, cx),
            None,
            None,
            cx,
        ))
}

/// SSL/TLS 页签：档位真实可存（内核会诚实报错），证书族等 M1.5。
pub(super) fn render_tab_tls(form: &Entity<EditForm>, cx: &App) -> Div {
    let form_for_pick = form.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "TLS"),
            choice_row(
                "redis-form-tls",
                &[
                    ("disable", "不启用"),
                    ("trust", "校验证书"),
                    ("danger-accept-invalid", "接受无效证书"),
                ],
                form.read(cx).tls.wire_name(),
                move |wire, window, cx| {
                    form_for_pick.update(cx, |f, _| f.tls = TlsMode::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(
                tr(
                    cx,
                    "本版本未启用 TLS：选非「不启用」档会在连接时明确报错，不会静默降级成明文。",
                )
                .into(),
            ),
            cx,
        ))
        .child(form_field(
            tr(cx, "服务器名称指示（SNI）"),
            disabled_shell("M1.5 接入", cx),
            None,
            Some(tr(cx, "M1.5 接入：内核尚未启用 TLS 特性。").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "客户端证书 / 根证书"),
            disabled_shell("M1.5 接入", cx),
            None,
            None,
            cx,
        ))
}

/// SSH 隧道页签：档案可先填（M1.5 前建连会明确报错，绝不静默直连）。
pub(super) fn render_tab_ssh(form: &Entity<EditForm>, cx: &App) -> Div {
    let (ssh_host, ssh_port, ssh_user, ssh_password, ssh_remote_host, enabled) = {
        let state = form.read(cx);
        (
            state.ssh_host.clone(),
            state.ssh_port.clone(),
            state.ssh_user.clone(),
            state.ssh_password.clone(),
            state.ssh_remote_host.clone(),
            state.ssh_enabled,
        )
    };

    let form_for_toggle = form.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(field_label(tr(cx, "启用 SSH 隧道"), cx))
                .child(switch_control(
                    "redis-form-ssh-enabled",
                    enabled,
                    move |value: &bool, window, cx| {
                        form_for_toggle.update(cx, |f, _| f.ssh_enabled = *value);
                        window.refresh();
                    },
                )),
        )
        .when(enabled, |this| {
            this.child(form_row(
                [
                    form_field(
                        tr(cx, "SSH 地址"),
                        input_shell(&ssh_host, false, false, cx),
                        None,
                        None,
                        cx,
                    )
                    .flex_1()
                    .min_w_0()
                    .into_any_element(),
                    form_field(
                        tr(cx, "端口"),
                        input_shell(&ssh_port, false, false, cx),
                        None,
                        None,
                        cx,
                    )
                    .flex_1()
                    .min_w_0()
                    .into_any_element(),
                ],
                cx,
            ))
            .child(form_field(
                tr(cx, "SSH 用户名"),
                input_shell(&ssh_user, false, false, cx),
                None,
                None,
                cx,
            ))
            .child(form_field(
                tr(cx, "SSH 密码"),
                input_shell(&ssh_password, true, false, cx),
                None,
                Some(tr(cx, "加密存本机，不入连接档案").into()),
                cx,
            ))
            .child(form_field(
                tr(cx, "转发目标"),
                input_shell(&ssh_remote_host, false, false, cx),
                None,
                Some(tr(cx, "相对跳板机解析；留空则连跳板机本机").into()),
                cx,
            ))
        })
        .child(field_hint(
            tr(cx, "SSH 密钥与跳板机（ProxyJump）在 M1.5 接入；当前版本开隧道会在连接时明确报错，绝不静默直连。"),
            cx,
        ))
}

/// 高级页签。
pub(super) fn render_tab_advanced(form: &Entity<EditForm>, cx: &App) -> Div {
    let db = form.read(cx).db.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            trf(cx, "默认数据库（0-{}）", &[&MAX_DB.to_string()]),
            input_shell(&db, false, false, cx),
            None,
            Some(tr(cx, "建连后立即 SELECT 该库").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "数据库数量"),
            disabled_shell("M1.4 接入", cx),
            None,
            Some(tr(cx, "M1.4 接入：留空自动探测（CONFIG GET databases）").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "连接 / 响应超时"),
            disabled_shell("M1.4 接入", cx),
            None,
            Some(tr(cx, "M1.4 接入；当前用系统级缺省").into()),
            cx,
        ))
        .child(field_hint(
            tr(
                cx,
                "运行模式（单机 / 哨兵 / 集群）在 M1.6 接入，当前仅支持单机直连。",
            ),
            cx,
        ))
}

/// 安全页签：环境标签 + 写入策略 + 写确认（分级锁写的全部输入）。
pub(super) fn render_tab_safety(form: &Entity<EditForm>, cx: &App) -> Div {
    let env_options: Vec<(&'static str, &'static str)> = EnvTag::ALL
        .iter()
        .map(|tag| match tag {
            EnvTag::None => ("none", "无标签"),
            EnvTag::Dev => ("dev", "开发"),
            EnvTag::Test => ("test", "测试"),
            EnvTag::Staging => ("staging", "预发布"),
            EnvTag::Prod => ("prod", "生产"),
        })
        .collect();
    let form_for_tag = form.clone();
    let form_for_policy = form.clone();
    let form_for_confirm = form.clone();
    let require_confirm = form.read(cx).require_confirm;

    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "环境标签"),
            choice_row(
                "redis-form-env",
                &env_options,
                form.read(cx).env_tag.wire_name(),
                move |wire, window, cx| {
                    form_for_tag.update(cx, |f, _| f.env_tag = EnvTag::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(tr(cx, "给连接列表着色分组，防「把生产库当测试库」。").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "写入"),
            choice_row(
                "redis-form-policy",
                &[
                    ("follow-tag", "跟随标签"),
                    ("allow", "允许"),
                    ("lock", "锁定，每次解锁 15 分钟"),
                    ("read-only", "只读"),
                ],
                form.read(cx).write_policy.wire_name(),
                move |wire, window, cx| {
                    form_for_policy
                        .update(cx, |f, _| f.write_policy = WritePolicy::from_wire(wire));
                    window.refresh();
                },
                cx,
            ),
            None,
            Some(
                tr(
                    cx,
                    "「跟随标签」= 生产档默认锁写；「允许」压过生产锁；「锁定」在任何档位都锁。",
                )
                .into(),
            ),
            cx,
        ))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(field_label(tr(cx, "写入确认"), cx))
                .child(switch_control(
                    "redis-form-confirm-writes",
                    require_confirm,
                    move |value: &bool, window, cx| {
                        form_for_confirm.update(cx, |f, _| f.require_confirm = *value);
                        window.refresh();
                    },
                )),
        )
        .child(field_hint(
            tr(
                cx,
                "打开后，该连接上每条写命令在执行前都要二次确认（M1.3 收尾接线）。",
            ),
            cx,
        ))
}

/// 键页签：键树 / SCAN 行为的按连接覆盖。
pub(super) fn render_tab_keys(form: &Entity<EditForm>, cx: &App) -> Div {
    let separator = form.read(cx).separator.clone();
    let scan_count = form.read(cx).scan_count.clone();
    let tree_depth = form.read(cx).tree_depth.clone();
    let auto_expand = form.read(cx).auto_expand.clone();
    div()
        .flex()
        .flex_col()
        .gap(form_gap(cx))
        .child(form_field(
            tr(cx, "键分隔符"),
            input_shell(&separator, false, false, cx),
            None,
            Some(tr(cx, "键树按它切命名空间；留空则使用 :").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "扫描键数量"),
            input_shell(&scan_count, false, false, cx),
            None,
            Some(tr(cx, "每次 SCAN 的 COUNT；留空则使用界面默认值").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "键树最大深度"),
            input_shell(&tree_depth, false, false, cx),
            None,
            Some(tr(cx, "超过这个层数就不再切分；留空则使用 8 层").into()),
            cx,
        ))
        .child(form_field(
            tr(cx, "自动展开阈值"),
            input_shell(&auto_expand, false, false, cx),
            None,
            Some(tr(cx, "子树键数不超过它就默认展开；留空或 0 表示不自动展开").into()),
            cx,
        ))
}

/// 弹窗正文：页签条 + 当前页签的表单。
pub(super) fn render_form_tabs(form: &Entity<EditForm>, cx: &App) -> Stateful<Div> {
    let tab = form.read(cx).tab;
    let tokens = active_theme(cx).tokens();
    let form_for_select = form.clone();

    let body = match tab {
        1 => render_tab_tls(form, cx),
        2 => render_tab_ssh(form, cx),
        3 => render_tab_advanced(form, cx),
        4 => render_tab_safety(form, cx),
        5 => render_tab_keys(form, cx),
        _ => render_tab_general(form, cx),
    }
    .into_any_element();

    let tabs = segmented_tabs(
        "redis-form-tabs",
        FORM_TABS
            .iter()
            .map(|label| SharedString::from(tr(cx, label))),
        tab,
        move |index, window, cx| {
            form_for_select.update(cx, |f, _| f.tab = index);
            window.refresh();
        },
        cx,
    );

    div()
        .id("redis-form-body")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.md)
                .px(tokens.spacing.lg)
                .py(tokens.spacing.md)
                .child(tabs)
                .child(body),
        )
}

/// 弹窗底部：左侧「测试 / 删除」，右侧「取消 / 添加」。
pub(super) fn redis_form_footer(
    form: &Entity<EditForm>,
    view: &WeakEntity<RedisPage>,
    is_new: bool,
    cx: &App,
) -> DialogFooter {
    let test_view = view.clone();
    let test_form = form.clone();
    let ok_view = view.clone();
    let ok_form = form.clone();
    let delete_view = view.clone();
    let delete_id = form.read(cx).id.clone();
    let theme = active_theme(cx);
    let tokens = theme.tokens();

    let left = h_flex().gap(tokens.spacing.sm).children(vec![
        button(
            "redis-form-test",
            tr(cx, "测试"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, window, cx| {
            let _ = test_view.update(cx, |this, cx| this.test_connection(&test_form, window, cx));
        })
        .into_any_element(),
    ]);

    let right = h_flex().gap(tokens.spacing.sm).children({
        let mut items: Vec<AnyElement> = Vec::new();
        if let Some(id) = delete_id {
            items.push(
                button(
                    "redis-form-delete",
                    tr(cx, "删除此连接"),
                    ButtonVariant::Destructive,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let _ = delete_view.update(cx, |this, cx| {
                        this.delete_connection(&id, window, cx);
                    });
                    window.close_dialog(cx);
                })
                .into_any_element(),
            );
        }
        items.push(
            button(
                "redis-form-cancel",
                tr(cx, "取消"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(|_, window, cx| window.close_dialog(cx))
            .into_any_element(),
        );
        items.push(
            button(
                "redis-form-ok",
                tr(cx, if is_new { "添加" } else { "保存" }),
                ButtonVariant::Primary,
                cx,
            )
            .on_click(move |_, window, cx| {
                let saved = ok_view
                    .update(cx, |this, cx| this.save_form(&ok_form, window, cx))
                    .unwrap_or(true);
                if saved {
                    window.close_dialog(cx);
                }
            })
            .into_any_element(),
        );
        items
    });

    DialogFooter::new()
        .justify_between()
        .child(left)
        .child(right)
}
