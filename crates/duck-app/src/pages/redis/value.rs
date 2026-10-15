//! 值查看：详情构造、三档解码、编码切换控件与值编辑器。

use super::*;

/// 把一次读取的结果装配成值面板与 shell 右栏所需的全部信息。字符串按 `decode` 档位渲染。
///
/// **这是 `KeyDetail` 唯一的构造点**：`object`（`OBJECT ENCODING` / `IDLETIME`）
/// 也在这里一起落进结构体，免得出现「建好了但少填两个字段」的半成品。
pub(super) fn build_detail(
    key: String,
    key_type: KeyType,
    ttl: i64,
    object: KeyObjectInfo,
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
    // 「树」档只在 **JSON 解码档**下成立：解码出 JSON 才谈得上折叠。
    // 建树是一次纯内存遍历（`serde_json` 已先挡了 128 层解析深度上限），
    // 随详情一起算好，渲染期只做展平查表。
    //
    // ⚠️ 只在 JSON 档建，不在「文本档里恰好是合法 JSON」时偷偷建：那会让
    // 档位按钮的含义变得不可预测（同一段文字，切到文本档就看不了树）。
    let tree = match (&view, decode) {
        (ValueView::Text { rendered, .. }, DecodeKind::Json) => tree_of(rendered),
        _ => None,
    };
    KeyDetail {
        key,
        key_type,
        ttl,
        size,
        truncated,
        raw,
        view,
        tree,
        encoding: object.encoding,
        idletime_secs: object.idletime_secs,
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

// ── 呈现档位与 JSON 树档（P3）─────────────────────────────────────────────

impl RedisPage {
    /// 树档在当前键上是否可用（决定档位按钮灰不灰）。
    ///
    /// 判据只有一条：**详情里有没有建出树**。建树的条件（JSON 解码档 + 解析
    /// 成功）在 [`build_detail`] 里一处决定，这里不重复判定——否则两处判据
    /// 迟早会分叉，出现「按钮亮着但点开是空的」。
    pub(super) fn json_available(&self) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|detail| detail.tree.is_some())
    }

    /// 当前键上**实际生效**的呈现档位。
    ///
    /// `value_view` 是用户的偏好，不保证这个键上能生效：非 JSON 值一律按文本显示。
    /// 偏好原样留着，换回一个 JSON 键就自动回到树档，不必再点一次。
    pub(super) fn effective_value_view(&self) -> ValueViewMode {
        if self.value_view == ValueViewMode::Tree && self.json_available() {
            ValueViewMode::Tree
        } else {
            ValueViewMode::Text
        }
    }

    /// 新详情到位后接管树档状态。
    ///
    /// 「换键」按默认档重来（展开两层），**同一个键**重新加载（写完刷新、手动刷新）
    /// 则保留用户的折叠状态——刚改完一个字段就把整棵树折回去很烦人。
    ///
    /// ⚠️ 「上一个键也是它」还不够，还得**上一个键那时确实有树**：在同一个键上把
    /// 解码档从「文本」切到「JSON」时，折叠集合是从「没有树」的状态里出来的，
    /// 必须补一次默认展开，否则树档只剩一行光秃秃的根。
    pub(super) fn adopt_json_tree(&mut self, key: &str) {
        let same_key_with_tree = self.json_key.as_deref() == Some(key);
        if !same_key_with_tree {
            self.json_expanded = self
                .detail
                .as_ref()
                .and_then(|detail| detail.tree.as_ref())
                .map(default_expanded)
                .unwrap_or_default();
        }
        // `json_key` 只在**确实有树**时才记：它就是「这份折叠状态从哪来」的凭据。
        self.json_key = self.json_available().then(|| key.to_owned());
        self.sync_json_rows();
    }

    /// 切呈现档位（文本 / 树）。纯本地：不重扫、不重新读值。
    pub(super) fn set_value_view(&mut self, mode: ValueViewMode, cx: &mut Context<Self>) {
        // 不可用的档位不接。按钮本来就灰着，这里再挡一道是为了挡住别的调用路径。
        if mode == ValueViewMode::Tree && !self.json_available() {
            return;
        }
        if self.value_view != mode {
            self.value_view = mode;
            self.sync_json_rows();
            cx.notify();
        }
    }

    /// 展开 / 收起树里的一行。
    pub(super) fn toggle_json_row(&mut self, path: &str, cx: &mut Context<Self>) {
        toggle_path(&mut self.json_expanded, path);
        self.sync_json_rows();
        cx.notify();
    }

    /// 按当前展开集合重算树的行。
    ///
    /// 行序**只在这里算**：渲染期每帧重算一遍展平会为每个节点克隆一遍
    /// `path` / `name` / `preview`（两万节点就是六万次分配），滚动直接垮掉。
    fn sync_json_rows(&mut self) {
        let rows = self
            .detail
            .as_ref()
            .and_then(|detail| detail.tree.as_ref())
            .map(|root| flatten(root, &self.json_expanded))
            .unwrap_or_default();
        self.json_rows = Arc::new(rows);
    }

    /// 主区右栏：**值编辑器**（对齐 zedis 的值编辑区）。
    ///
    /// 自上而下四段：键信息条（键名 / 类型 / 原始大小 / TTL / 刷新 / 保存）→
    /// 呈现与解码档位 → 值本体（文本或可折叠树）→ 该类型的结构化写入口。
    ///
    /// 键的**属性与危险操作**不在这里，在 shell 级右栏
    /// （`sidebar::render_right_sidebar`）——两块面板各司其职，右栏是「关于这个键」，
    /// 这里是「这个键的值」。
    pub(super) fn render_value_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let view = self.view.clone();

        let Some(detail) = &self.detail else {
            return v_flex()
                .size_full()
                .id("redis-value-editor")
                .items_center()
                .justify_center()
                .p(tokens.spacing.lg)
                .child(meta_text(
                    if self.selected.is_some() {
                        tr(cx, "正在读取值…")
                    } else {
                        tr(cx, "从左边选择一个键查看值。")
                    },
                    cx,
                ))
                .into_any_element();
        };

        let gate = self.gate();
        let can_write = gate
            .as_ref()
            .is_some_and(|gate| gate.can(Capability::WriteKey));

        let mut column = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .id("redis-value-editor")
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.sm)
            .child(value_header(detail, &view, cx))
            .child(value_tier_row(self, detail, &view, cx));

        // 值本体。⚠️ 树档的 `uniform_list` 必须**直接挂在 flex 容器下**：包一层
        // 默认 `display:block` 的 `div()` 会让它在 gpui-pre 0.3.8 下量不出高度、
        // **整表不渲染**（键数是照常计数的、无任何报错，界面一片空白）——
        // 与键列表同一个坑，见 `render_browser` 的同名注释与无头回归测试。
        column = if self.effective_value_view() == ValueViewMode::Tree {
            let rows = self.json_rows.clone();
            let row_count = rows.len();
            let row_view = view.clone();
            column.child(
                uniform_list("redis-json-tree", row_count, move |range, _window, cx| {
                    range
                        .filter_map(|index| rows.get(index).map(|row| json_row(row, &row_view, cx)))
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .min_h_0(),
            )
        } else {
            column.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .id("redis-value-body")
                    .overflow_y_scroll()
                    .child(text_body(self, &view, cx)),
            )
        };

        // 按类型给编辑入口（字符串 / 哈希 / 列表 / 集合 / 有序集合各一套字段）。
        column = column.child(value_editor(self, detail.key_type, &view, can_write, cx));

        if !can_write {
            column = column.child(meta_text(
                gate.as_ref()
                    .and_then(|gate| gate.check(Capability::WriteKey).err())
                    .map(|reason| deny_message(reason, cx))
                    .unwrap_or_else(|| tr(cx, "当前连接为只读。")),
                cx,
            ));
        }

        column.into_any_element()
    }
}

/// 值面板的「文本」档内容（原来的值展示区，整体从 shell 右栏搬来）。
///
/// 可视区高度由外层容器负责（[`RedisPage::render_value_editor`] 里的
/// `redis-value-body`），这里只管内容本身。
fn text_body(page: &RedisPage, view: &WeakEntity<RedisPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let Some(detail) = &page.detail else {
        return div().into_any_element();
    };
    match &detail.view {
        ValueView::Text {
            rendered,
            notice,
            bytes,
        } => v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(
                div()
                    .w_full()
                    .p(tokens.spacing.xs)
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .child(SharedString::from(rendered.clone())),
            )
            // 解码档位的说明行（GZIP 解压出多少字节、时间戳按哪个单位解读……）：
            // 没有它，用户不知道 GZIP 档显示的这段字是从哪来的。
            .children(
                notice
                    .clone()
                    .map(|text| meta_text(SharedString::from(text), cx)),
            )
            .child(meta_text(
                trf(cx, "原始 {} 字节", &[&bytes.to_string()]),
                cx,
            ))
            .into_any_element(),
        ValueView::Binary { hex, bytes } => v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(
                div()
                    .w_full()
                    .p(tokens.spacing.xs)
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .child(SharedString::from(hex.clone())),
            )
            .child(meta_text(
                trf(cx, "二进制内容，共 {} 字节", &[&bytes.to_string()]),
                cx,
            ))
            .into_any_element(),
        ValueView::TooLarge { bytes } => meta_text(
            trf(cx, "值过大（{} 字节），未自动加载。", &[&bytes.to_string()]),
            cx,
        )
        .into_any_element(),
        ValueView::JsonFailed { error, bytes } => v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(
                div()
                    .w_full()
                    .p(tokens.spacing.xs)
                    .rounded(theme.components().input_radius)
                    .border(extended.stroke.thin)
                    .border_color(extended.colors.hairline)
                    .bg(tokens.colors.background)
                    .text_size(tokens.typography.sm.size)
                    .text_color(tokens.colors.foreground)
                    .child(SharedString::from(trf(
                        cx,
                        "不是合法 JSON：{}",
                        &[error.as_str()],
                    ))),
            )
            .child(meta_text(
                trf(
                    cx,
                    "共 {} 字节，可切到「文本」或「Hex」档查看原文。",
                    &[&bytes.to_string()],
                ),
                cx,
            ))
            .into_any_element(),
        ValueView::Hash(pairs) => hash_rows(&detail.key, pairs, view, cx),
        ValueView::ZSet(pairs) => zset_rows(&detail.key, pairs, view, cx),
        ValueView::List(items) => list_rows(&detail.key, items, page.list_editing, view, cx),
        ValueView::Set(items) => set_rows(&detail.key, items, view, cx),
        ValueView::Stream(entries) => stream_rows(entries, cx),
        ValueView::Missing => meta_text(
            tr(cx, "键已不存在（可能在选中之后被其他客户端删掉了）。"),
            cx,
        )
        .into_any_element(),
        ValueView::Unsupported(key_type) => meta_text(
            trf(
                cx,
                "「{}」类型的专用查看器尚未实现（计划在后续增量补上）。",
                &[key_type_label(*key_type, cx)],
            ),
            cx,
        )
        .into_any_element(),
    }
}

/// 树档的一行（行高统一，供 `uniform_list` 用）。
fn json_row(row: &JsonTreeRow, view: &WeakEntity<RedisPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    match row {
        JsonTreeRow::Row {
            path,
            name,
            depth,
            kind,
            preview,
            len,
            omitted,
            expandable,
            expanded,
        } => {
            // 每深一层缩进一档。用 `spacing.sm` 而不是裸数字：缩进要跟着「密度」
            // 设置一起变，否则紧凑档下树会显得松松垮垮。
            let indent = tokens.spacing.xs + tokens.spacing.sm * (*depth as f32);
            let detail: SharedString = if kind.is_container() {
                // 容器的「值」是成员数，不是内容——内容在子行里。
                let mut text = trf(cx, "{} 项", &[&len.to_string()]);
                if *omitted > 0 {
                    text = format!(
                        "{text} · {}",
                        trf(cx, "另有 {} 项未显示", &[&omitted.to_string()])
                    );
                }
                text.into()
            } else {
                SharedString::from(preview.clone())
            };
            let path = path.clone();
            h_flex()
                .id(SharedString::from(format!("redis-json-{path}")))
                .h(px(JSON_TREE_ROW_HEIGHT))
                .w_full()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xs)
                .pl(indent)
                .pr(tokens.spacing.sm)
                .role(Role::ListItem)
                .aria_label(SharedString::from(format!("{name} {detail}")))
                .when(*expandable, |row| {
                    row.cursor_pointer()
                        .hover(move |style| style.bg(extended.colors.hover_tint))
                        .on_click({
                            let view = view.clone();
                            let path = path.clone();
                            move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                let _ = view.update(cx, |this, cx| {
                                    this.toggle_json_row(&path, cx);
                                });
                            }
                        })
                })
                .child(
                    // 箭头槽位**恒占宽**（不可展开的行留白）：否则同一层的名字会随
                    // 兄弟节点能否展开而左右错开，树形就看不出来了。
                    div()
                        .flex_none()
                        .w(px(JSON_TREE_ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .children(expandable.then(|| {
                            Icon::new(if *expanded {
                                DuckIcon::ChevronDown
                            } else {
                                DuckIcon::ChevronRight
                            })
                            .size(extended.icon.sm)
                            .text_color(tokens.colors.muted_foreground)
                            .into_any_element()
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(180.))
                        .truncate()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.mono_md.size)
                        .text_color(if kind.is_container() {
                            tokens.colors.muted_foreground
                        } else {
                            tokens.colors.foreground
                        })
                        .child(detail),
                )
                .into_any_element()
        }
        JsonTreeRow::Omitted { depth, count } => {
            let indent = tokens.spacing.xs + tokens.spacing.sm * (*depth as f32);
            h_flex()
                .id(SharedString::from(format!("redis-json-omitted-{depth}")))
                .h(px(JSON_TREE_ROW_HEIGHT))
                .w_full()
                .flex_none()
                .items_center()
                .pl(indent)
                .child(meta_text(
                    trf(cx, "另有 {} 项未显示", &[&count.to_string()]),
                    cx,
                ))
                .into_any_element()
        }
    }
}

/// 流类型的行式渲染（原样从 shell 右栏搬来）。
fn stream_rows(entries: &[(String, Vec<(String, String)>)], cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
    for (id, fields) in entries {
        list = list.child(
            v_flex()
                .w_full()
                .gap(tokens.spacing.xxs)
                .p(tokens.spacing.xs)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .child(
                    div()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(SharedString::from(id.clone())),
                )
                .children(fields.iter().map(|(field, value)| {
                    div()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(SharedString::from(format!("{field}: {value}")))
                })),
        );
    }
    list.into_any_element()
}

/// 值面板的顶栏：键名 + 类型 + 原始大小 + TTL + 刷新 / 保存。
fn value_header(detail: &KeyDetail, view: &WeakEntity<RedisPage>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let ttl_text: SharedString = match detail.ttl {
        -1 => SharedString::from(tr(cx, "永久")),
        -2 => SharedString::from(tr(cx, "键不存在")),
        seconds => trf(cx, "{} 秒", &[&seconds.to_string()]).into(),
    };
    // 原始大小：字符串给**服务端上那一份的字节数**（解码 / 解压之前），
    // 集合给元素条数——两种类型的「大小」本来就不是一回事。
    let size_text: SharedString = if detail.key_type == KeyType::String {
        SharedString::from(format_bytes(detail.raw.len()))
    } else {
        trf(cx, "{} 项", &[&detail.size.to_string()]).into()
    };
    let is_string = detail.key_type == KeyType::String;

    h_flex()
        .flex_none()
        .w_full()
        .items_center()
        .gap(tokens.spacing.xs)
        .pb(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(detail.key.clone())),
        )
        .child(neutral_tag(key_type_label(detail.key_type, cx), cx))
        .child(meta_text(size_text, cx))
        .child(meta_text(trf(cx, "TTL {}", &[ttl_text.as_ref()]), cx))
        .child(
            icon_button(
                "redis-value-refresh",
                tr(cx, "重新读取这个键"),
                Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.refresh_selected_key(window, cx));
                }
            }),
        )
        .child(
            // 「保存」只对字符串有意义（其余类型的写入口是下方的结构化表单），
            // 所以不是字符串就整块不画，而不是画一个灰按钮占位。
            div().children(is_string.then(|| {
                button(
                    "redis-value-save",
                    tr(cx, "保存"),
                    ButtonVariant::Primary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.save_string_value(window, cx));
                    }
                })
                .into_any_element()
            })),
        )
}

/// 呈现档位与解码档位：两排按钮，都只改「同一份字节怎么显示」。
///
/// 拆成两行是**有意的**：两排按钮长得一样（一档一个 Secondary / Primary），
/// 挤在同一行里分不清哪个按钮管的是「呈现」、哪个管的是「解码」。
fn value_tier_row(
    page: &RedisPage,
    detail: &KeyDetail,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> Div {
    let tokens = active_theme(cx).tokens();
    let effective = page.effective_value_view();
    let available = page.json_available();

    let mut views = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(tokens.spacing.xxs);
    views = views.child(meta_text(tr(cx, "视图"), cx));
    for mode in ValueViewMode::ALL {
        let usable = mode != ValueViewMode::Tree || available;
        views = views.child(
            button(
                SharedString::from(format!("redis-value-view-{}", mode.wire_name())),
                value_view_label(mode, cx),
                if mode == effective {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .disabled(!usable)
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.set_value_view(mode, cx));
                }
            }),
        );
    }

    let mut column = v_flex()
        .flex_none()
        .w_full()
        .gap(tokens.spacing.xxs)
        .child(views);
    // 解码档位（九档）只有字符串值有，`decode_chips` 自己带 `flex_wrap`，
    // 所以面板再窄也只是多占几行、不会横向撑破。
    if let Some(chips) = decode_chips(detail.key_type, page.decode, view, cx) {
        column = column.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.xxs)
                .child(meta_text(tr(cx, "解码"), cx))
                .child(div().flex_1().min_w_0().child(chips)),
        );
    }
    column
}
