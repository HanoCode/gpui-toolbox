//! 值查看：详情构造、三档解码、编码切换控件与值编辑器。

use super::*;

/// 把一次读取的结果装配成右栏详情。字符串按 `decode` 档位渲染。
pub(super) fn build_detail(
    key: String,
    key_type: KeyType,
    ttl: i64,
    value: RedisValue,
    decode: DecodeKind,
    cx: &App,
) -> KeyDetail {
    let size = value.len();
    let truncated = size >= VALUE_PAGE;
    // 字符串的原始字节留一份：切档只用它重算，不再跑一次 `GET`。
    let raw = match &value {
        RedisValue::String(bytes) => bytes.clone(),
        _ => Vec::new(),
    };
    // 模块类型没有通用读法（要 `DUMP`，见设计文档 M1.6）：明确说「没有查看器」，
    // 而不是让内核抛一个英文错误出去。
    let view = if key_type == KeyType::Module {
        ValueView::Unsupported(key_type)
    } else {
        match &value {
            RedisValue::String(bytes) => decode_view(bytes, decode, cx),
            RedisValue::Hash(pairs) => ValueView::Hash(
                pairs
                    .iter()
                    .map(|(field, val)| (lossy(field), lossy(val)))
                    .collect(),
            ),
            RedisValue::List(items) => {
                ValueView::List(items.iter().map(|item| lossy(item)).collect())
            }
            RedisValue::Set(items) => {
                ValueView::Set(items.iter().map(|item| lossy(item)).collect())
            }
            RedisValue::ZSet(pairs) => ValueView::ZSet(
                pairs
                    .iter()
                    .map(|(member, score)| {
                        (lossy(member), duck_redis::value::format_double(*score))
                    })
                    .collect(),
            ),
            RedisValue::Stream(entries) => ValueView::Stream(
                entries
                    .iter()
                    .map(|(id, fields)| {
                        (
                            lossy(id),
                            fields
                                .iter()
                                .map(|(field, val)| (lossy(field), lossy(val)))
                                .collect(),
                        )
                    })
                    .collect(),
            ),
            RedisValue::Missing => ValueView::Missing,
        }
    };
    KeyDetail {
        key,
        key_type,
        ttl,
        size,
        truncated,
        raw,
        view,
    }
}

/// 字节 → 展示态：**按显式档位**解码。
///
/// 解码本身全在内核（`duck_redis::decode`，九档：文本 / JSON / Hex / Base64 /
/// GZIP / JWT / 时间戳 / URL / PHP），这里只负责「档位结果 → 右栏展示态」的适配：
///
/// - 解出的内容能当文本看 → `Text`（附内核给的说明行）；
/// - 解出的是二进制（hex 预览）→ `Binary`；
/// - 解码失败 → `JsonFailed` 复用同一个错误框（文案由内核源串渲染，不写死）；
/// - 体积超过渲染上限 → `TooLarge`，连解码都不做。
pub(super) fn decode_view(bytes: &[u8], kind: DecodeKind, cx: &App) -> ValueView {
    if bytes.len() > VALUE_RENDER_LIMIT {
        return ValueView::TooLarge { bytes: bytes.len() };
    }
    match redis_decode(kind, bytes) {
        Ok(decoded) => {
            let notice = decoded
                .notice
                .as_ref()
                .map(|message| decode_message(message, cx));
            // 内核明确告诉我们这次解出的是文本还是 hex 预览（`binary`），
            // 界面不靠「猜内容像不像 hex」来判断。
            if decoded.binary {
                ValueView::Binary {
                    hex: decoded.text,
                    bytes: bytes.len(),
                }
            } else {
                ValueView::Text {
                    rendered: decoded.text,
                    notice,
                    bytes: bytes.len(),
                }
            }
        }
        Err(message) => ValueView::JsonFailed {
            error: decode_message(&message, cx),
            bytes: bytes.len(),
        },
    }
}

/// 有损 UTF-8（键名与元素都是二进制安全的，展示时替换坏字节）。
pub(super) fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
/// 解码档位切换（只有字符串值有这九档；M1.5 起从三档扩到九档）。
///
/// 九档在右栏一行放不下，所以允许换行（`flex_wrap`）：档位再多也不会把右栏撑破。
pub(super) fn decode_chips(
    key_type: KeyType,
    current: DecodeKind,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> Option<AnyElement> {
    if key_type != KeyType::String {
        return None;
    }
    let tokens = active_theme(cx).tokens();
    let mut row = h_flex().flex_wrap().gap(tokens.spacing.xxs);
    for mode in DecodeKind::ALL {
        row = row.child(
            button(
                SharedString::from(format!("redis-decode-{}", mode.wire_name())),
                decode_label(mode, cx),
                if mode == current {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.switch_decode(mode, cx));
                }
            }),
        );
    }
    Some(row.into_any_element())
}

/// 类型相关的值编辑表单（每个类型只给该类型有意义的字段）。
///
/// `enabled` = 写闸门放行；为假时全部按钮禁用，原因由调用方在下文统一说明
/// （不给每个按钮配一句提示，那会把右栏塞满重复文案）。
pub(super) fn value_editor(
    page: &RedisPage,
    key_type: KeyType,
    view: &WeakEntity<RedisPage>,
    enabled: bool,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let mut form = v_flex().w_full().gap(tokens.spacing.xs);

    match key_type {
        KeyType::String => {
            form = form
                .child(
                    v_flex()
                        .w_full()
                        .gap(tokens.spacing.xxs)
                        .child(field_label(tr(cx, "编辑字符串值"), cx))
                        .child(
                            div()
                                .w_full()
                                // 字符串值编辑框的固定高度（约 4 行文本），布局约束、非间距。
                                .h(px(64.))
                                .px(tokens.spacing.xs)
                                .py(tokens.spacing.xs)
                                .rounded(theme.components().input_radius)
                                .border(theme.extended().stroke.thin)
                                .border_color(theme.extended().colors.hairline)
                                .bg(tokens.colors.background)
                                .child(
                                    Input::new(&page.edit_buffer)
                                        .appearance(false)
                                        .bordered(false)
                                        .w_full(),
                                ),
                        ),
                )
                .child(
                    button(
                        "redis-save-value",
                        tr(cx, "保存值"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.save_string_value(window, cx));
                        }
                    }),
                );
        }
        KeyType::Hash => {
            form = form
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(editor_field("字段名", &page.hash_field, cx))
                        .child(editor_field("字段值", &page.hash_value, cx)),
                )
                .child(
                    button(
                        "redis-hash-write",
                        tr(cx, "写入字段"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.save_hash_field(window, cx));
                        }
                    }),
                );
        }
        KeyType::List => {
            form = form.child(editor_field("元素值", &page.list_value, cx));
            // 底部表单有两种态：正在改某一行（LSET）或追加（LPUSH / RPUSH）。
            // 两种态的按钮长得不一样，用户才不会点错。
            form = match page.list_editing {
                Some(_) => form.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-list-apply",
                                tr(cx, "保存这一行"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(false, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-list-cancel",
                                tr(cx, "取消修改"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.cancel_list_edit(window, cx);
                                    });
                                }
                            }),
                        ),
                ),
                None => form.child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-list-push-front",
                                tr(cx, "左推入"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(true, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-list-push-back",
                                tr(cx, "右推入"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.save_list_value(false, window, cx);
                                    });
                                }
                            }),
                        ),
                ),
            };
            if let Some(index) = page.list_editing {
                form = form.child(meta_text(
                    trf(cx, "正在修改第 {} 行", &[&index.to_string()]),
                    cx,
                ));
            }
        }
        KeyType::Set => {
            form = form
                .child(editor_field("成员", &page.set_member, cx))
                .child(
                    button(
                        "redis-set-add",
                        tr(cx, "加入成员"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.add_set_member(window, cx));
                        }
                    }),
                );
        }
        KeyType::ZSet => {
            form = form
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(editor_field("成员", &page.zset_member, cx))
                        .child(editor_field("分数", &page.zset_score, cx)),
                )
                .child(
                    button(
                        "redis-zset-write",
                        tr(cx, "写入成员"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| {
                                // 底部表单只做「新增」；改分数/改名走每一行自己的入口。
                                this.save_zset_member(None, window, cx);
                            });
                        }
                    }),
                );
        }
        // 流、模块类型与缺失键都没有通用写入口：给一句实话，而不是一堆禁用按钮。
        KeyType::Stream | KeyType::Module | KeyType::Missing => {
            form = form.child(meta_text(tr(cx, "该类型暂无可用的结构化编辑入口。"), cx));
        }
    }
    form.into_any_element()
}

/// 键管理：设置 / 移除过期时间、重命名。
pub(super) fn key_ops(
    page: &RedisPage,
    view: &WeakEntity<RedisPage>,
    enabled: bool,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    v_flex()
        .w_full()
        .gap(tokens.spacing.sm)
        .child(caption_heading(tr(cx, "键管理"), cx))
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(h_flex().w_full().gap(tokens.spacing.xs).child(editor_field(
                    "过期时间",
                    &page.ttl_input,
                    cx,
                )))
                .child(
                    h_flex()
                        .w_full()
                        .gap(tokens.spacing.xs)
                        .child(
                            button(
                                "redis-ttl-set",
                                tr(cx, "设置过期时间"),
                                ButtonVariant::Primary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.set_ttl(window, cx));
                                }
                            }),
                        )
                        .child(
                            button(
                                "redis-ttl-clear",
                                tr(cx, "移除过期时间"),
                                ButtonVariant::Secondary,
                                cx,
                            )
                            .disabled(!enabled)
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.clear_ttl(window, cx));
                                }
                            }),
                        ),
                ),
        )
        .child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xs)
                .child(editor_field("新的键名", &page.rename_input, cx))
                .child(
                    button(
                        "redis-rename",
                        tr(cx, "重命名"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(!enabled)
                    .on_click({
                        let view = view.clone();
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.rename_key(window, cx));
                        }
                    }),
                ),
        )
        .into_any_element()
}

// ── 连接弹窗（对齐 zedis 的「添加新服务器」表单）─────────────────────────
