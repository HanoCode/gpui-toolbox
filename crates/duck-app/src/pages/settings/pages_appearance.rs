//! 设置页「外观」：主题卡、强调色、图标包与色点。

use super::*;

/// 自定义强调色取色器的槽位；随窗口缓存，避免每帧重建。
pub struct AccentPickerSlot {
    pub(super) picker: Entity<ColorPickerState>,
    pub(super) synced: Hsla,
    pub(super) _subscription: Subscription,
}

/// 取色器实体：提交颜色即写入外观设置。
pub(super) fn accent_picker(
    window: &mut Window,
    cx: &mut Context<SettingsWindowView>,
    current: Hsla,
) -> Entity<ColorPickerState> {
    let slot = window.use_keyed_state(
        SharedString::from("duck-settings-accent-picker"),
        cx,
        move |window, cx| {
            let picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(current));
            let subscription = cx.subscribe(
                &picker,
                |_: &mut AccentPickerSlot, _, event: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(Some(value)) = event else {
                        return;
                    };
                    update_appearance(cx, |appearance| appearance.accent = *value);
                },
            );
            AccentPickerSlot {
                picker,
                synced: current,
                _subscription: subscription,
            }
        },
    );
    slot.update(cx, |slot, cx| {
        if slot.synced != current {
            slot.synced = current;
            slot.picker
                .update(cx, |picker, cx| picker.set_value(current, window, cx));
        }
    });
    slot.read(cx).picker.clone()
}

/// 主题卡片：预览块 + 标签 + 选中勾。
pub(super) fn theme_card(
    id: BuiltinThemeId,
    selected: bool,
    appearance: &Appearance,
    cx: &Context<SettingsWindowView>,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let preview = id.preview();
    let row_hover = theme.extended().colors.hover_tint;
    let check_size = theme.extended().icon.sm;
    let opacity = theme.extended().opacity;
    let mode = theme.mode();
    let bar = |width: gpui::DefiniteLength, color: Hsla| {
        div()
            .w(width)
            .h(px(THEME_PREVIEW_BAR_HEIGHT))
            .rounded_full()
            .bg(color)
    };
    let preview_block = h_flex()
        .w_full()
        .h(px(THEME_PREVIEW_HEIGHT))
        .rounded(tokens.radius.md)
        .border_1()
        .border_color(preview.border)
        .bg(preview.background)
        .overflow_hidden()
        .child(
            v_flex()
                .w(px(THEME_PREVIEW_SIDEBAR_WIDTH))
                .h_full()
                .items_center()
                .justify_center()
                .gap(px(THEME_PREVIEW_BAR_HEIGHT))
                .bg(preview.surface)
                .child(bar(px(16.).into(), preview.primary))
                .child(bar(
                    px(16.).into(),
                    preview.muted_foreground.opacity(opacity.muted),
                ))
                .child(bar(
                    px(16.).into(),
                    preview.muted_foreground.opacity(opacity.muted),
                )),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .p(tokens.spacing.xs)
                .justify_center()
                .gap(px(THEME_PREVIEW_BAR_HEIGHT))
                .child(bar(relative(1.), preview.foreground.opacity(opacity.faint)))
                .child(bar(
                    relative(1.),
                    preview.muted_foreground.opacity(opacity.faint),
                ))
                .child(bar(
                    relative(0.6),
                    preview.muted_foreground.opacity(opacity.faint),
                )),
        );

    let mut next = appearance.clone();
    next.theme = next.theme.with(mode, id);
    // 焦点环色要 `move` 进闭包 → 先拷出来（`Hsla` 是 `Copy`）。
    let ring = colors.ring;
    let ring_strong = colors.foreground;
    div()
        .id(SharedString::from(format!(
            "duck-theme-card-{}",
            id.wire_name()
        )))
        .w(px(THEME_CARD_WIDTH))
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.lg)
        .border_1()
        .border_color(if selected {
            colors.primary
        } else {
            colors.border
        })
        .bg(colors.surface)
        // 无障碍：此前整排主题卡片对读屏是不存在的（a11y 树只收录带 role 的节点）。
        // 语义上是「一组里选一个」→ 单选按钮；可访问名取**显式** `aria_label`
        // （卡片自己的可见标签），不依赖「从子节点文字派生」这条本仓库验证不了的假设
        // ——`test_support.rs:318` 的 `label()` 也只认显式 label。容器在调用点补
        // `RadioGroup`。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(id.label())
        .focusable()
        .tab_stop(true)
        // 焦点描边一律 `ring`；**例外**：选中态边框本身就是 `colors.primary`(=`ring`)，
        // 两者重合会让焦点看上去「没反应」——偏偏就落在用户最可能停留的那张卡上。
        // 所以选中时换成 `colors.foreground`：仓库既有的强描边色（色点的选中/悬停
        // 用的就是它），对 `surface` 与 `primary` 都有足够反差。
        .focus_visible(move |style| style.border_color(if selected { ring_strong } else { ring }))
        .cursor_pointer()
        .when(!selected, |this| {
            this.hover(move |style| style.bg(row_hover))
        })
        .on_click(move |_, _, cx| {
            let next = next.clone();
            update_appearance(cx, move |appearance| *appearance = next.clone());
        })
        .child(preview_block)
        .child(
            h_flex()
                .mt(tokens.spacing.xs)
                .gap(tokens.spacing.xs)
                .justify_between()
                .items_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(colors.foreground)
                .child(div().min_w_0().truncate().child(id.label()))
                .children(selected.then(|| {
                    Icon::new(DuckIcon::Check)
                        .size(check_size)
                        .text_color(colors.primary)
                })),
        )
}

/// 文件图标包卡片：预览样例文件图标 + 标签 + 选中勾（样式对齐主题卡片）。
pub(super) fn icon_pack_card(
    id: &str,
    label: SharedString,
    selected: bool,
    on_select: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let row_hover = theme.extended().colors.hover_tint;
    let check_size = theme.extended().icon.sm;
    let preview_icon = theme.extended().icon.lg;
    let muted = colors.muted_foreground;
    // 焦点环色要 `move` 进闭包 → 先拷出来。
    let ring = colors.ring;
    let ring_strong = colors.foreground;
    let icons = icon_pack_preview(id);
    let preview = h_flex()
        .w_full()
        .h(px(THEME_PREVIEW_HEIGHT))
        .justify_center()
        .items_center()
        .gap(tokens.spacing.xs)
        .rounded(tokens.radius.md)
        .bg(colors.background)
        .overflow_hidden()
        .children(icons.into_iter().map(|(icon, brand)| {
            let color = brand.map_or(muted, |value| accent_from_rgb(value));
            Icon::new(icon).size(preview_icon).text_color(color)
        }));
    div()
        .id(SharedString::from(format!("duck-icon-pack-card-{id}")))
        .w(px(THEME_CARD_WIDTH))
        .p(tokens.spacing.sm)
        .rounded(tokens.radius.lg)
        .border_1()
        .border_color(if selected {
            colors.primary
        } else {
            colors.border
        })
        .bg(colors.surface)
        // 无障碍：同 `theme_card` —— 无 role 的节点不进 a11y 树；语义为单选按钮。
        // 可访问名取**显式** `aria_label`（复用下面那句可见文案），不依赖
        // 「从子节点文字派生」这条本仓库验证不了的假设。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(label.clone())
        .focusable()
        .tab_stop(true)
        // 同上：选中态边框已是 `colors.primary`(=`ring`)，焦点环改用 `foreground` 才看得见。
        .focus_visible(move |style| style.border_color(if selected { ring_strong } else { ring }))
        .cursor_pointer()
        .when(!selected, |this| {
            this.hover(move |style| style.bg(row_hover))
        })
        .on_click(on_select)
        .child(preview)
        .child(
            h_flex()
                .mt(tokens.spacing.xs)
                .gap(tokens.spacing.xs)
                .justify_between()
                .items_center()
                .text_size(tokens.typography.xs.size)
                .line_height(tokens.typography.xs.line_height)
                .text_color(colors.foreground)
                .child(div().min_w_0().truncate().child(label))
                .children(selected.then(|| {
                    Icon::new(DuckIcon::Check)
                        .size(check_size)
                        .text_color(colors.primary)
                })),
        )
}

/// 主题卡片下方的动作行：导入 / 导出（差异 · 完整）/ 更多主题。
/// 外壳未接入主题文件读写，动作均为占位提示。
pub(super) fn theme_actions(cx: &mut Context<SettingsWindowView>) -> impl IntoElement {
    let tokens = active_theme(cx).tokens();
    h_flex()
        .mt(tokens.spacing.xs)
        .gap(tokens.spacing.sm)
        .flex_wrap()
        .child(
            button(
                "duck-settings-theme-import",
                tr(cx, "导入"),
                ButtonVariant::Secondary,
                cx,
            )
            .flex_shrink_0()
            .on_click(|_, window, cx| {
                duck_components::notify::info(
                    tr(cx, "主题导入尚未接入（外壳无主题文件读写）"),
                    window,
                    cx,
                );
            }),
        )
        .child(
            Button::new("duck-settings-theme-export")
                .outline()
                .control(cx)
                .label(tr(cx, "导出"))
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::TopLeft, |menu, _, cx| {
                    let detail = tr(cx, "主题导出（差异快照）尚未接入");
                    let full = tr(cx, "主题导出（完整）尚未接入");
                    menu.item(PopupMenuItem::new(tr(cx, "差异快照（仅设置）")).on_click(
                        move |_, window, cx| {
                            duck_components::notify::info(detail, window, cx);
                        },
                    ))
                    .item(
                        PopupMenuItem::new(tr(cx, "完整主题")).on_click(move |_, window, cx| {
                            duck_components::notify::info(full, window, cx);
                        }),
                    )
                }),
        )
        .child(
            button(
                "duck-settings-theme-more",
                tr(cx, "更多主题"),
                ButtonVariant::Link,
                cx,
            )
            .flex_shrink_0()
            .on_click(|_, window, cx| {
                duck_components::notify::info(tr(cx, "主题画廊尚未提供"), window, cx);
            }),
        )
}

/// 强调色色点。
pub(super) fn color_dot(
    index: usize,
    color: Hsla,
    selected: bool,
    icon: Option<DuckIcon>,
    cx: &Context<SettingsWindowView>,
) -> impl IntoElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let colors = tokens.colors;
    let icon_size = theme.extended().icon.md;
    let ring = colors.ring;
    // 色点里只有一个勾图标、没有文字，AccessKit **没有东西可派生**可访问名 → 只能显式给。
    // 用色值本身当名字（`#8B5CF6`）是零 i18n 成本且信息量真实的选择；
    // 「蓝 / 紫 / 青」这类**本地化色名**需要往 `i18n.rs` 加串，留待后续（AGENTS 3.3）。
    let accent_rgb = ACCENTS[index];
    div()
        .id(("duck-accent-dot", index))
        .size(px(COLOR_DOT_SIZE))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(color)
        .border_2()
        .border_color(if selected { colors.foreground } else { color })
        // 无障碍：色点此前对读屏根本不存在（a11y 树只收录带 role 的节点）。
        // 一组预设色 = 「选一个」→ 单选按钮，容器在调用点补 `RadioGroup`。
        .role(Role::RadioButton)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .aria_label(SharedString::from(format!("#{accent_rgb:06X}")))
        .focusable()
        .tab_stop(true)
        // 焦点描边用 `ring`：选中态描边是 `foreground`，两者正好错开，两种状态都看得见。
        // `border_2` 本来就占着位，只改色不引起位移。
        .focus_visible(move |style| style.border_color(ring))
        .cursor_pointer()
        .when(!selected, |this| {
            // 色点语义：悬停描边加深到正文色。
            this.hover(move |style| style.border_color(colors.foreground))
        })
        .on_click(move |_, _, cx| {
            update_appearance(cx, move |appearance| appearance.accent = color);
        })
        .when_some(icon, |this, icon| {
            this.child(
                Icon::new(icon)
                    .size(icon_size)
                    .text_color(duck_theme::foreground_for(color)),
            )
        })
}
impl SettingsWindowView {
    // ── 外观：单列纵向（主题与配色 → 界面）──────────────────────────────
    pub(super) fn appearance_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let tokens = active_theme(cx).tokens().clone();
        let lang = active_theme(cx).lang();

        // 左列：主题与配色。
        let appearance = active_theme(cx).appearance().clone();
        let mode = active_theme(cx).mode();

        let mode_options = [
            ("system", tr(cx, "跟随系统")),
            ("light", tr(cx, "亮色")),
            ("dark", tr(cx, "暗色")),
        ]
        .map(|(value, label)| (SharedString::from(value), SharedString::from(label)));
        let mode_current = appearance.preference.wire_name();
        let theme_mode_row = option_row(
            tr(cx, "主题模式"),
            Some(tr(cx, "跟随系统时随 macOS 外观切换，切换即时生效。").into()),
            dropdown_select(
                "duck-settings-theme-mode",
                mode_options,
                mode_current,
                |value, _, cx| {
                    let preference = match value.as_ref() {
                        "light" => ThemePreference::Light,
                        "dark" => ThemePreference::Dark,
                        _ => ThemePreference::System,
                    };
                    update_appearance(cx, move |appearance| appearance.preference = preference);
                },
                cx,
            ),
            cx,
        );

        let slot_label = match mode {
            ThemeMode::Dark => tr(cx, "暗色主题"),
            ThemeMode::Light => tr(cx, "亮色主题"),
        };
        let cards: Vec<gpui::AnyElement> = BuiltinThemeId::presets_for(mode)
            .map(|id| {
                theme_card(id, appearance.theme.for_mode(mode) == id, &appearance, cx)
                    .into_any_element()
            })
            .collect();
        let theme_cards_row = option_row_stacked(
            tr(cx, "主题"),
            None,
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(meta_text(slot_label, cx))
                .child(
                    h_flex()
                        .id("duck-theme-card-list")
                        // 卡片按 `Role::RadioButton` + `aria_toggled` 报，容器配 `RadioGroup`。
                        .role(Role::RadioGroup)
                        .gap(tokens.spacing.sm)
                        .flex_wrap()
                        .children(cards),
                )
                .child(theme_actions(cx)),
            cx,
        );

        let preset_selected = ACCENTS
            .iter()
            .any(|value| accent_from_rgb(*value) == appearance.accent);
        let custom_selected = self.custom_accent_open && !preset_selected;
        let custom_icon_size = active_theme(cx).extended().icon.md;
        let custom_ring = tokens.colors.ring;
        let custom_dot = div()
            .id("duck-accent-dot-custom")
            .size(px(COLOR_DOT_SIZE))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(appearance.accent)
            .border_2()
            .border_color(if custom_selected {
                tokens.colors.foreground
            } else {
                appearance.accent
            })
            // 无障碍：同预设色点（无 role 就不进 a11y 树）；子节点只有一个图标，
            // 没有文字可派生名字，所以显式给——复用自定义取色器那句中英对照文案。
            .role(Role::RadioButton)
            .aria_toggled(if custom_selected {
                Toggled::True
            } else {
                Toggled::False
            })
            .aria_label(tr(cx, "自定义强调色"))
            .focusable()
            .tab_stop(true)
            // 同 `color_dot`：选中态描边是 `foreground`，焦点 `ring` 与之错开。
            .focus_visible(move |style| style.border_color(custom_ring))
            .cursor_pointer()
            .when(!custom_selected, |this| {
                // 色点语义：悬停加深自身描边到正文色，够明显的对比环。
                this.hover(move |style| style.border_color(tokens.colors.foreground))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.custom_accent_open = !this.custom_accent_open;
                cx.notify();
            }))
            .child(
                Icon::new(DuckIcon::SlidersHorizontal)
                    .size(custom_icon_size)
                    .text_color(duck_theme::foreground_for(appearance.accent)),
            );

        let dots = h_flex()
            .id("duck-accent-dot-list")
            // 预设色点也是「选一个」，与主题卡片同构 → 同样的单选组语义。
            .role(Role::RadioGroup)
            .gap(tokens.spacing.sm)
            .flex_wrap()
            .children(ACCENTS.iter().enumerate().map(|(index, value)| {
                let color = accent_from_rgb(*value);
                let selected = color == appearance.accent
                    || (appearance.accent == default_accent()
                        && *value == duck_theme::DEFAULT_ACCENT_RGB);
                color_dot(
                    index,
                    color,
                    selected,
                    selected.then_some(DuckIcon::Check),
                    cx,
                )
                .into_any_element()
            }))
            .child(custom_dot);
        let mut accent_column = v_flex().w_full().gap(tokens.spacing.sm).child(dots);
        if self.custom_accent_open {
            let picker = accent_picker(window, cx, appearance.accent);
            accent_column = accent_column.child(
                // 不传 featured colors：预设色已由上方色点提供，避免重复的 a11y 色块 id。
                ColorPicker::new(&picker)
                    .label(tr(cx, "自定义强调色"))
                    .featured_colors(Vec::new()),
            );
        }
        let theme_color_row = option_row_stacked(
            tr(cx, "主题色"),
            Some(tr(cx, "同时派生选中导航图标色与链接文字色。").into()),
            accent_column,
            cx,
        );

        // 文件图标：图标包选择卡片（预览样例文件图标，与主题相互独立）。
        let pack_cards: Vec<AnyElement> = ICON_PACKS
            .iter()
            .map(|(id, label)| {
                icon_pack_card(
                    id,
                    SharedString::from(tr(cx, label)),
                    self.icon_pack == *id,
                    cx.listener(move |this, _, _, cx| {
                        if this.icon_pack != *id {
                            this.icon_pack = SharedString::from(*id);
                            cx.notify();
                        }
                    }),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let icon_pack_row = option_row_stacked(
            tr(cx, "文件图标"),
            Some(tr(cx, "任务列表中的图标风格，与主题相互独立。").into()),
            h_flex()
                .id("duck-icon-pack-list")
                // 同主题卡片：一组「选一个」。
                .role(Role::RadioGroup)
                .gap(tokens.spacing.sm)
                .flex_wrap()
                .children(pack_cards),
            cx,
        );

        let theme_section = settings_section(
            Some(tr(cx, "主题与配色").into()),
            [
                theme_mode_row.into_any_element(),
                theme_cards_row.into_any_element(),
                theme_color_row.into_any_element(),
                icon_pack_row.into_any_element(),
            ],
            cx,
        );

        // 语言：与其它外观项一样直接读写 `Appearance.language`（无中间占位状态）。
        let language_options: Vec<(SharedString, SharedString)> = Locale::ALL
            .iter()
            .map(|locale| {
                (
                    SharedString::from(locale.wire_name()),
                    SharedString::from(t(lang, locale.label())),
                )
            })
            .collect();
        let language_row = option_row(
            tr(cx, "语言"),
            Some(tr(cx, "界面显示语言；切换后立即生效，界面文案与提示都会跟随。").into()),
            dropdown_select(
                "duck-settings-language",
                language_options,
                appearance.language.wire_name(),
                |value, _, cx| {
                    update_appearance(cx, move |appearance| {
                        appearance.language = Locale::from_wire(&value);
                    });
                },
                cx,
            ),
            cx,
        );
        // 语言并入「界面」分组，整页单列纵向排列。

        // 右列：界面。
        let scale_options: Vec<(SharedString, SharedString)> = SCALES
            .iter()
            .map(|scale| {
                let percent = (scale * 100.).round() as i32;
                (
                    SharedString::from(percent.to_string()),
                    SharedString::from(format!("{percent}%")),
                )
            })
            .collect();
        let scale_current = SharedString::from((appearance.ui_scale * 100.).round().to_string());
        let scale_row = option_row(
            tr(cx, "界面缩放"),
            Some(tr(cx, "缩放尺寸、间距、图标与文字档位。").into()),
            dropdown_select(
                "duck-settings-ui-scale",
                scale_options,
                scale_current,
                |value, _, cx| {
                    let Ok(percent) = value.parse::<f32>() else {
                        return;
                    };
                    update_appearance(cx, move |appearance| {
                        appearance.ui_scale = (percent / 100.).clamp(0.8, 1.5);
                    });
                },
                cx,
            ),
            cx,
        );

        // 界面密度：与「界面缩放」是两个独立的轴 —— 缩放是等比放大，密度是改变档位关系
        // （紧凑档下侧栏行高比控件更紧）。二者叠加生效。
        let density_options: Vec<(SharedString, SharedString)> = Density::ALL
            .iter()
            .map(|density| {
                (
                    SharedString::from(density.wire_name()),
                    SharedString::from(tr(cx, density.label())),
                )
            })
            .collect();
        let density_row = option_row(
            tr(cx, "界面密度"),
            Some(
                tr(
                    cx,
                    "控件高度与行高的疏密档位，与界面缩放叠加生效；「紧凑」同屏显示更多内容，「宽松」更易点选。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-density",
                density_options,
                appearance.density.wire_name(),
                |value, _, cx| {
                    let density = Density::from_wire_name(&value);
                    update_appearance(cx, move |appearance| appearance.density = density);
                },
                cx,
            ),
            cx,
        );

        let mut font_options = vec![(
            SharedString::from(FONT_SIZE_THEME),
            SharedString::from(tr(cx, "跟随主题")),
        )];
        font_options.extend(FONT_SIZES.iter().map(|font_size| {
            (
                SharedString::from(font_size.to_string()),
                SharedString::from(format!("{} px", *font_size as i32)),
            )
        }));
        let font_current = appearance.font_size.map_or_else(
            || SharedString::from(FONT_SIZE_THEME),
            |font_size| SharedString::from(font_size.to_string()),
        );
        let font_row = option_row(
            tr(cx, "字体大小"),
            Some(
                tr(
                    cx,
                    "本机正文字号；其余文字与行高按同一比例适配，与界面缩放叠加生效。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-font-size",
                font_options,
                font_current,
                |value, _, cx| {
                    let (min, max) = FONT_SIZE_RANGE;
                    let next = value
                        .parse::<f32>()
                        .ok()
                        .filter(|font_size| (min..=max).contains(font_size));
                    update_appearance(cx, move |appearance| appearance.font_size = next);
                },
                cx,
            ),
            cx,
        );

        // 字体：本机字体族可搜索下拉；确认选择即写外观偏好（空值 = 跟随主题字体）。
        let current_family = appearance.font_family.clone().unwrap_or_default();
        if self.font_synced.as_ref() != Some(&current_family) {
            self.font_synced = Some(current_family.clone());
            self.font_select.update(cx, |select, cx| {
                select.set_selected_value(&current_family, window, cx);
            });
        }
        let font_family_row = option_row(
            tr(cx, "字体"),
            Some(tr(cx, "选择此电脑上可用的字体。字体不可用时回退到主题字体；新安装的字体可能需要重启应用后才能读取。").into()),
            Select::new(&self.font_select)
                .placeholder(tr(cx, "使用主题字体"))
                // 字体的**舒适宽度**：不在 spacing 阶梯上（布局约束），保留裸值；
                // `.max_w_full()` 兜住窄窗口。
                .w(px(240.))
                .max_w_full()
                .h(active_theme(cx).density().control)
                .py_0(),
            cx,
        );

        let interface_section = settings_section(
            Some(tr(cx, "界面").into()),
            [
                language_row.into_any_element(),
                scale_row.into_any_element(),
                density_row.into_any_element(),
                font_family_row.into_any_element(),
                font_row.into_any_element(),
            ],
            cx,
        );

        // 布局：改的是**结构**（右栏停靠侧、间距多大、哪些 chrome 显示），不是文字与尺寸。
        // 与「界面」分组的边界是：凡改变「有没有 / 在哪」的项进这里，改「多大 / 多密」的进界面。
        //
        // ⚠️ 命名必须点明「右栏」：窗口左侧还有活动栏与页面导航，笼统的「侧栏位置」会被
        // 理解成挪那些东西（2026-10-11 用户实测报告「侧栏调整不生效」，根因即此歧义 +
        // 效果只在工具箱页可见）。描述里必须写明作用范围。
        let side_options: Vec<(SharedString, SharedString)> =
            [SidebarSide::Left, SidebarSide::Right]
                .into_iter()
                .map(|side| {
                    (
                        SharedString::from(side.wire_name()),
                        SharedString::from(tr(cx, side.label())),
                    )
                })
                .collect();
        let side_row = option_row(
            tr(cx, "右栏位置"),
            Some(
                tr(
                    cx,
                    "内容区右栏停靠在哪一侧；目前仅工具箱页提供右栏内容，切换即时生效。",
                )
                .into(),
            ),
            dropdown_select(
                "duck-settings-sidebar-side",
                side_options,
                appearance.right_sidebar_side.wire_name(),
                |value, _, cx| {
                    let side = SidebarSide::from_wire_name(&value);
                    update_appearance(cx, move |appearance| appearance.right_sidebar_side = side);
                },
                cx,
            ),
            cx,
        );

        let nav_gap_options: Vec<(SharedString, SharedString)> = NAV_GAPS
            .iter()
            .map(|gap| {
                (
                    SharedString::from(gap.to_string()),
                    SharedString::from(format!("{} px", *gap as i32)),
                )
            })
            .collect();
        let nav_gap_current = SharedString::from(appearance.nav_gap.round().to_string());
        let nav_gap_row = option_row(
            tr(cx, "侧栏项间距"),
            Some(tr(cx, "工具箱页侧栏条目之间的留白（0 为贴紧）。").into()),
            dropdown_select(
                "duck-settings-nav-gap",
                nav_gap_options,
                nav_gap_current,
                |value, _, cx| {
                    let Ok(gap) = value.parse::<f32>() else {
                        return;
                    };
                    let (min, max) = NAV_GAP_RANGE;
                    update_appearance(cx, move |appearance| {
                        appearance.nav_gap = gap.clamp(min, max);
                    });
                },
                cx,
            ),
            cx,
        );

        let show_activity_bar_row = option_row(
            tr(cx, "显示活动栏"),
            Some(
                tr(
                    cx,
                    "窗口最左侧的功能轨（概览 / 工具箱 / 设置…）。关闭后内容区占满窗口。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-show-activity-bar",
                appearance.show_activity_bar,
                |value, _, cx| {
                    update_appearance(cx, |appearance| appearance.show_activity_bar = *value);
                },
            ),
            cx,
        );

        let show_notification_bell_row = option_row(
            tr(cx, "显示通知铃铛"),
            Some(
                tr(
                    cx,
                    "顶栏的通知中心入口；关闭后通知仍记入历史，只是顶栏不再显示铃铛。",
                )
                .into(),
            ),
            switch_control(
                "duck-settings-show-notification-bell",
                appearance.show_notification_bell,
                |value, _, cx| {
                    update_appearance(cx, |appearance| {
                        appearance.show_notification_bell = *value;
                    });
                },
            ),
            cx,
        );

        let show_top_bar_extras_row = option_row(
            tr(cx, "显示顶栏附加入口"),
            Some(tr(cx, "顶栏的插件中心按钮与语言切换；关闭后二者一并隐藏。").into()),
            switch_control(
                "duck-settings-show-top-bar-extras",
                appearance.show_top_bar_extras,
                |value, _, cx| {
                    update_appearance(cx, |appearance| appearance.show_top_bar_extras = *value);
                },
            ),
            cx,
        );

        let layout_section = settings_section(
            Some(tr(cx, "布局").into()),
            [
                side_row.into_any_element(),
                nav_gap_row.into_any_element(),
                show_activity_bar_row.into_any_element(),
                show_notification_bell_row.into_any_element(),
                show_top_bar_extras_row.into_any_element(),
            ],
            cx,
        );

        v_flex()
            .w_full()
            .gap(tokens.spacing.lg)
            .child(theme_section)
            .child(interface_section)
            .child(layout_section)
    }
}
