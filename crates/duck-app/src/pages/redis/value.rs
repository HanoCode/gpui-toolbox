//! 值查看：详情构造、三档解码、编码切换控件与值编辑器。

use super::*;
use duck_components::selectable_text;

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
                }
            }
        }
        Err(message) => ValueView::JsonFailed {
            error: decode_message(&message, cx),
            bytes: bytes.len(),
        },
    }
}

/// 字符串值的**自动检测**（zedis 的 `ViewMode::Auto` 语义）：内容是合法 JSON
/// 就按 JSON 档展示（美化 + 语法高亮），否则文本档。zedis 每次加载值都把
/// 视图重置回 Auto（`should_reset_viewer_mode`），这里同款——用户在解码下拉
/// 里选的档位活到下一次加载为止，不让一次临时选择粘在所有键上。
pub(super) fn auto_decode(bytes: &[u8]) -> DecodeKind {
    if bytes.len() > VALUE_RENDER_LIMIT {
        return DecodeKind::Text;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if parse_document(text).is_some() => DecodeKind::Json,
        _ => DecodeKind::Text,
    }
}

/// 有损 UTF-8（键名与元素都是二进制安全的，展示时替换坏字节）。
pub(super) fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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
        // 字符串值的编辑入口已上移到键头部的铅笔（编辑态下输入框替换值本体，
        // 见 `render_value_editor`）——这里不再画第二份表单，免得两个「保存」打架。
        KeyType::String => {}
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
            // 文档换了一份，JSONPath 的命中就不再指向「看到的那个节点」——
            // 宁可清空也不显示错位的定位（口径见 `jsonpath_hits` 字段注释）。
            self.jsonpath_hits = Arc::new(Vec::new());
            self.jsonpath_error = None;
            self.jsonpath_selected = None;
        }
        // `json_key` 只在**确实有树**时才记：它就是「这份折叠状态从哪来」的凭据。
        self.json_key = self.json_available().then(|| key.to_owned());
        self.sync_json_rows();
    }

    /// 把当前详情的文本形态回填进内容编辑器，并同步高亮语言与软换行。
    ///
    /// 两个调用点：**详情到位**（选中键 / 刷新 / 保存成功后的重读）与**切解码档**
    /// ——两者都意味着「编辑器该显示的内容换了一份」。回填走 `set_value`
    /// （不发历史、折叠 undo），语言按解码档切（JSON 档 → tree-sitter 高亮），
    /// 软换行按开关与体量上限重算（超 [`SOFT_WRAP_MAX_BYTES`] 强制关，同 zedis）。
    pub(super) fn sync_value_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rendered) = self.detail.as_ref().and_then(|detail| match &detail.view {
            ValueView::Text { rendered, .. } => Some(rendered.clone()),
            _ => None,
        }) else {
            return;
        };
        let language = if self.decode == DecodeKind::Json {
            "json"
        } else {
            "text"
        };
        let wrap = self.soft_wrap && rendered.len() <= SOFT_WRAP_MAX_BYTES;
        self.value_editor.update(cx, |state, cx| {
            state.set_value(rendered, window, cx);
            state.set_highlighter(language, cx);
            state.set_soft_wrap(wrap, window, cx);
        });
        // 回填不是用户改动：订阅里的「与详情比对」也会得出同样的结论，
        // 这里直接归零是为了不依赖事件顺序（Change 是异步投递的）。
        self.value_modified = false;
    }

    /// 底栏切软换行后把开关应用到编辑器（体量超限仍强制关，见上限常量）。
    pub(super) fn apply_soft_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wrap = self.soft_wrap
            && self
                .detail
                .as_ref()
                .is_some_and(|detail| detail.raw.len() <= SOFT_WRAP_MAX_BYTES);
        self.value_editor
            .update(cx, |state, cx| state.set_soft_wrap(wrap, window, cx));
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
    pub(super) fn sync_json_rows(&mut self) {
        let rows = self
            .detail
            .as_ref()
            .and_then(|detail| detail.tree.as_ref())
            .map(|root| flatten(root, &self.json_expanded))
            .unwrap_or_default();
        self.json_rows = Arc::new(rows);
    }

    /// 主区右栏：**值面板**（对齐 zedis 的值编辑区 + tiny-rdm 的内容页签）。
    ///
    /// 自上而下：分页条（状态 / 键详情 / 命令行 / 慢日志 / 监控命令 / 发布订阅，
    /// 2026-10-16 新增）→ 键头部 → JSONPath 栏 → 值本体 → 该类型的结构化写入口。
    /// 「键详情」即原有的值编辑器；其余页与「服务端」视图共用同一份数据。
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
            .child(value_tabs_bar(self, cx));

        // 非「键详情」页：状态 / 命令行 / 慢日志 / 监控命令 / 发布订阅，
        // 正文直接复用「服务端」视图的同名面板（两处共存、同一份数据）。
        if self.value_tab != KeyValueTab::KeyDetail {
            let body = match self.value_tab {
                KeyValueTab::Status => self.render_panel_info(cx),
                KeyValueTab::Cli => self.render_panel_cli(cx),
                KeyValueTab::Slowlog => self.render_panel_slowlog(cx),
                KeyValueTab::Monitor => self.render_panel_monitor(cx),
                KeyValueTab::PubSub => self.render_panel_pubsub(cx),
                // 不可能到这（上方已排除）；真到了也给一句空态，不 panic。
                KeyValueTab::KeyDetail => panel_empty(tr(cx, "切换到「键详情」查看值。"), cx),
            };
            return column
                .child(div().flex_1().min_h_0().min_w_0().child(body))
                .into_any_element();
        }

        column = column
            .child(value_header(self, detail, can_write, &view, cx))
            .child(jsonpath_bar(self, detail, &view, cx))
            // 求值结果条：有错误显示错误，有命中显示命中（无则整块不画）。
            .child(jsonpath_results(self, &view, cx));

        // 值本体。⚠️ 树档的 `uniform_list` 必须**直接挂在 flex 容器下**：包一层
        // 默认 `display:block` 的 `div()` 会让它在 gpui-pre 0.3.8 下量不出高度、
        // **整表不渲染**（键数是照常计数的、无任何报错，界面一片空白）——
        // 与键列表同一个坑，见 `render_browser` 的同名注释与无头回归测试。
        //
        // 文本形态（含 JSON）：**内容区就是一个代码编辑器**（zedis 同款）——
        // 可选中复制、（可写时）直接键入修改、行号与软换行内建、JSON 高亮走
        // 编辑器的语言高亮（tree-sitter，`sync_value_editor` 按解码档切换）。
        // 只读不是灰掉的 `disabled`：外观不变、照常选中复制，只是拒绝改动。
        let notice = match &detail.view {
            ValueView::Text { notice, .. } => notice.clone(),
            _ => None,
        };
        column = if self.effective_value_view() == ValueViewMode::Tree {
            let rows = self.json_rows.clone();
            let row_count = rows.len();
            let row_view = view.clone();
            // 命中高亮跟着行闭包进树（`'static`：先克隆成拥有所有权的值）。
            let row_selected = self.jsonpath_selected.clone();
            column.child(
                uniform_list("redis-json-tree", row_count, move |range, _window, cx| {
                    range
                        .filter_map(|index| {
                            rows.get(index)
                                .map(|row| json_row(row, row_selected.as_deref(), &row_view, cx))
                        })
                        .collect::<Vec<_>>()
                })
                // 点击 JSONPath 命中时 `scroll_to_item` 写进这个句柄，
                // 下一帧布局时生效（deferred，gpui-pre 的既定语义）。
                .track_scroll(&self.json_tree_scroll)
                .flex_1()
                .min_h_0(),
            )
        } else if matches!(detail.view, ValueView::Text { .. }) {
            // ⚠️ 外层必须是 **flex 容器**（`v_flex`）：`div()` 是 display:block，
            // Editor 的 `flex_1` 在它下面量不到高度——编辑器只画出内容自己那几行
            // （与 uniform_list「包一层 div 整表不渲染」同源的坑，见上）。
            column = column.child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        Editor::new(&self.value_editor)
                            .appearance(false)
                            .bordered(false)
                            .readonly(!can_write)
                            .flex_1()
                            .min_h_0(),
                    )
                    // 解码档位的说明行（GZIP 解压出多少字节、时间戳按哪个单位……）
                    // 垫在编辑器下面：没有它，用户不知道这段字是从哪来的。
                    .children(
                        notice
                            .map(|text| meta_text(SharedString::from(text), cx))
                            .into_iter(),
                    ),
            );
            column
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

    /// 执行 JSONPath 查询（「运行」按钮与输入框回车共用）。
    ///
    /// 文档来源是**当前展示串**（`ValueView::Text` 的 `rendered`）：JSONPath 是
    /// 对「用户此刻看到的内容」提问，不是对服务端上的原始字节提问。所以只有
    /// 文本 / JSON 解码档的字符串值可查，其余一律明确报错。
    ///
    /// 三个失败面分开说清楚：没选键 / 查询语法错 / 值不是 JSON——混成一句
    /// 「查询失败」会让用户排查不了自己那一半的问题。
    pub(super) fn run_jsonpath(&mut self, cx: &mut Context<Self>) {
        self.jsonpath_error = None;
        self.jsonpath_hits = Arc::new(Vec::new());
        self.jsonpath_selected = None;
        let Some(detail) = &self.detail else {
            self.jsonpath_error = Some(tr(cx, "先选中一个键，再执行 JSONPath。").to_owned());
            cx.notify();
            return;
        };
        let query_text = self.jsonpath_input.read(cx).value().trim().to_owned();
        if query_text.is_empty() {
            self.jsonpath_error = Some(
                tr(
                    cx,
                    "输入一条以 $ 开头的 JSONPath，如 $.store.book[0].title。",
                )
                .to_owned(),
            );
            cx.notify();
            return;
        }
        let rendered = match &detail.view {
            ValueView::Text { rendered, .. } => rendered.clone(),
            _ => {
                self.jsonpath_error =
                    Some(tr(cx, "JSONPath 只支持字符串值（解码档：文本 / JSON）。").to_owned());
                cx.notify();
                return;
            }
        };
        let Some(document) = parse_document(&rendered) else {
            self.jsonpath_error =
                Some(tr(cx, "当前值不是合法 JSON，无法执行 JSONPath。").to_owned());
            cx.notify();
            return;
        };
        match jsonpath_run(&query_text, &document) {
            Ok(hits) => {
                self.jsonpath_hits = Arc::new(hits);
            }
            Err(error) => {
                self.jsonpath_error = Some(error);
            }
        }
        cx.notify();
    }

    /// 点击一条 JSONPath 命中：切到树档、展开祖先、滚动到那一行并高亮。
    ///
    /// 行身份就是 [`JsonTreeNode::path`]（命中与树行同一条 `render_path` 规则，
    /// 见内核 `jsonpath.rs` 的同源约定），所以这里不做任何字符串换算——拿着
    /// 命中的 path 在树里走一遍祖先链，把链上的容器全部展开，再按行下标滚动。
    ///
    /// 边界口径：
    /// - 命中节点因树节点上限（[`JSON_TREE_MAX_NODES`]）没有建出：树里找不到
    ///   就不跳、不高亮（命中列表照常显示）——树预算与命中封顶是两套口径，
    ///   这种错位理论上存在，静默放过比编一句报错文案更诚实；
    /// - 树档不可用（值不是 JSON）：命中本就不该存在，直接返回；
    /// - 处于编辑态先退出：编辑框盖着值本体，跳了也看不见。
    pub(super) fn jump_to_jsonpath_hit(&mut self, path: &str, cx: &mut Context<Self>) {
        // 先在不可变借用里把祖先链算完（`chain` 是 owned 的），再放开手做变更
        // ——否则 `root` 的借用会跟 `set_value_view` 的 `&mut self` 打架，
        // 只好整树克隆（两万节点的分配就为一跳，不值）。
        let chain = self
            .detail
            .as_ref()
            .and_then(|detail| detail.tree.as_ref())
            .and_then(|root| ancestor_chain(root, path));
        // 树里找不到（命中节点因树预算没建出）就不跳：命中列表照常显示。
        if chain.is_none() {
            return;
        }
        self.set_value_view(ValueViewMode::Tree, cx);
        // 展开祖先链（含目标自己：容器命中顺带把子行也亮出来）。
        if let Some(chain) = chain
            && chain
                .iter()
                .any(|ancestor| self.json_expanded.insert(ancestor.clone()))
        {
            self.sync_json_rows();
        }
        // 行下标按**当前**行序找：祖先展开后目标行一定在序列里（找到了链才展开）。
        let row_index = self.json_rows.iter().position(|row| {
            matches!(row, JsonTreeRow::Row { path: row_path, .. } if row_path.as_str() == path)
        });
        if let Some(index) = row_index {
            self.json_tree_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
            self.jsonpath_selected = Some(path.to_owned());
        }
        cx.notify();
    }
}

/// 在树里找一条路径，返回**从根到它**的路径链（含它自己）；找不到返回 `None`。
///
/// 给命中点击定位用：展开集合需要的是祖先链，而祖先路径只有树自己知道——
/// 路径是 `render_path` 拼出来的，从字符串反解就得重写一遍引号转义规则
/// （`["a.b"]` 与 `.a.b` 的区分），不如直接沿着树走一遍，按身份比对。
fn ancestor_chain(root: &JsonTreeNode, target: &str) -> Option<Vec<String>> {
    fn walk(node: &JsonTreeNode, target: &str, chain: &mut Vec<String>) -> bool {
        chain.push(node.path.clone());
        if node.path == target {
            return true;
        }
        for child in &node.children {
            if walk(child, target, chain) {
                return true;
            }
        }
        chain.pop();
        false
    }
    let mut chain = Vec::new();
    walk(root, target, &mut chain).then_some(chain)
}

fn text_body(page: &RedisPage, view: &WeakEntity<RedisPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let Some(detail) = &page.detail else {
        return div().into_any_element();
    };
    // ⚠️ 文本形态**不走这里**：`render_value_editor` 里 `ValueView::Text` 分支
    // 已整体换成代码编辑器（`value_editor` 实体）。本函数只管其余形态
    // （Hex 预览 / 过大 / 解码失败 / 集合类），在「文本档不可用但树档回落
    // 文本」时也可能被调到，所以没有 Text 分支是**有意**的。
    match &detail.view {
        // 文本形态已在 `render_value_editor` 被编辑器分支拦截，到不了这里；
        // 枚举必须穷尽，给一个明确的空兜底而不是让它意外混进别的分支。
        ValueView::Text { .. } => div().into_any_element(),
        ValueView::Binary { hex, bytes } => v_flex()
            .w_full()
            .gap(tokens.spacing.xxs)
            .child(
                div()
                    .w_full()
                    .p(tokens.spacing.xs)
                    .text_size(tokens.typography.mono_md.size)
                    .line_height(tokens.typography.mono_md.line_height)
                    .child(selectable_text(SharedString::from(hex.clone()))),
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
                    .child(selectable_text(SharedString::from(trf(
                        cx,
                        "不是合法 JSON：{}",
                        &[error.as_str()],
                    )))),
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
///
/// `selected` 是 JSONPath 命中点击定位的那一行（路径即行身份）：命中行垫
/// 选中底色，滚动到位之后眼睛有的放矢。
fn json_row(
    row: &JsonTreeRow,
    selected: Option<&str>,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> AnyElement {
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
            let is_selected = selected == Some(path.as_str());
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
                .when(is_selected, |row| row.bg(extended.colors.nav_selected))
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
                        .child(selectable_text(detail)),
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
                        .child(selectable_text(SharedString::from(id.clone()))),
                )
                .children(fields.iter().map(|(field, value)| {
                    div()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.foreground)
                        .child(selectable_text(SharedString::from(format!(
                            "{field}: {value}"
                        ))))
                })),
        );
    }
    list.into_any_element()
}

/// 类型胶囊（zedis `KeyTypeBadge` 的实心形态）：类型色淡底 + 同色文字 + 细描边。
///
/// 用在键头部（一处），与键树行的 plain 文字形态区分——头部只出现一次，
/// 可以亮一点；树里重复上千次的才需要全去饱和。
fn key_type_pill(key_type: KeyType, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let dark = matches!(theme.mode(), duck_theme::ThemeMode::Dark);
    let color = key_type_color(key_type, dark);
    div()
        .flex_none()
        .px(tokens.spacing.xs)
        .py(px(1.))
        .rounded(tokens.radius.sm)
        .bg(color.opacity(0.15))
        .border(extended.stroke.thin)
        .border_color(color.opacity(0.35))
        .text_size(extended.badge.size)
        .line_height(extended.badge.line_height)
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(color)
        .child(SharedString::from(key_type.badge_code()))
        .into_any_element()
}

/// 值面板顶部的**分页条**（对齐 tiny-rdm 的键内容页签：状态 / 键详情 / 命令行 /
/// 慢日志 / 监控命令 / 发布订阅）。
///
/// 组件用全站同款 `segmented_tabs_with_icons`；「服务端」视图与这些页**两处共存**
/// （既定决策），同一份数据读两遍不会失真——写入方只有一个（`load_panel_for`）。
fn value_tabs_bar(page: &RedisPage, cx: &mut Context<RedisPage>) -> Div {
    // 先取动作（要 `&mut cx`），再取主题（只借 `&cx`）——顺序反了会撞借用。
    let actions = page.render_value_tab_actions(cx);
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let view = page.view.clone();
    let items = KeyValueTab::ALL
        .iter()
        .map(|tab| SegmentItem::new(tab.label(cx)).icon(tab.icon()));
    let selected = KeyValueTab::ALL
        .iter()
        .position(|tab| *tab == page.value_tab)
        .unwrap_or(0);
    // 面板级动作按**值分页**取（监控的开始/停止、命令行的清空）——
    // 与「服务端」视图同一套按钮，只是判断轴是 `value_tab` 不是 `panel`。
    h_flex()
        .flex_none()
        .w_full()
        .items_center()
        .gap(tokens.spacing.sm)
        .child(segmented_tabs_with_icons(
            "redis-value-tabs",
            items,
            selected,
            move |index, window, cx| {
                let Some(tab) = KeyValueTab::ALL.get(index).copied() else {
                    return;
                };
                let _ = view.update(cx, |this, cx| this.set_value_tab(tab, window, cx));
            },
            cx,
        ))
        .child(div().flex_1())
        .children(actions)
        // 与键头部同一条底线：分页条不是「另一层卡片」，右端留白对齐键头部。
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .pb(tokens.spacing.xs)
}

/// 当前详情的**可复制文本**（值头部「复制键值」按钮用）。
///
/// 取的是**当前展示串**（解码档位之后的样子）：复制出来就能直接用，
/// 与界面所见一致（tiny-rdm 同口径）。取不到的形态（过大未加载 / 键已不在 /
/// 暂无查看器）返回空串，调用点据此提示而不是复制一个空串。
pub(super) fn key_value_text(detail: &KeyDetail) -> String {
    match &detail.view {
        ValueView::Text { rendered, .. } => rendered.clone(),
        ValueView::Hash(pairs) => pairs
            .iter()
            .map(|(field, item)| format!("{field}\t{item}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ValueView::List(items) | ValueView::Set(items) => items.join("\n"),
        ValueView::ZSet(items) => items
            .iter()
            .map(|(member, score)| format!("{member}\t{score}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ValueView::Stream(entries) => entries
            .iter()
            .map(|(id, fields)| {
                let pairs = fields
                    .iter()
                    .map(|(field, item)| format!("{field}: {item}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{id}: {pairs}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        // 二进制 / 失败 / 过大 / 缺失 / 不支持：没有「用户想要的值」可复制。
        ValueView::Binary { .. }
        | ValueView::JsonFailed { .. }
        | ValueView::TooLarge { .. }
        | ValueView::Missing
        | ValueView::Unsupported(_) => String::new(),
    }
}

/// 值面板的键头部（40px 单行，对齐 zedis 的 `EDITOR_KEY_BAR_HEIGHT`）。
///
/// 从左到右：收藏星 → 类型胶囊 → mono 键名 → 大小 → 编码·空闲 → TTL 芯片 →
/// 刷新 → 解码下拉（仅字符串）→ 保存（仅字符串且有改动语义时可见）。
///
/// 「空闲 / 内部编码」从 shell 右栏借来：zedis 把它们放在头部（`24.62 kB raw · 空闲`），
/// 读值时一眼可见；右栏那两行详情保留（面板职责不同）。
fn value_header(
    page: &RedisPage,
    detail: &KeyDetail,
    can_write: bool,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let dark = matches!(theme.mode(), duck_theme::ThemeMode::Dark);
    let favorite = page.active_tags().is_favorite(&detail.key);
    let star_color = duck_theme::color_from_rgb(if dark { 0xd8b45c } else { 0xa8801a });

    // 原始大小：字符串给**服务端上那一份的字节数**（解码 / 解压之前），集合给元素
    // 条数。⚠️ 不拼「 raw」后缀：字符串的 `OBJECT ENCODING` 本来就是 `raw` /
    // `embstr` / `int`，编码段会把它显示出来——再拼一次就是「20.4 KB raw raw」。
    let size_text: SharedString = if detail.key_type == KeyType::String {
        SharedString::from(format_bytes(detail.raw.len()))
    } else {
        trf(cx, "{} 项", &[&detail.size.to_string()]).into()
    };
    // TTL 芯片：会过期的给人性化时长（`3.3h` 同款），永久的给「永久」，
    // 键已消失的给「键不存在」。不画「∞」：多数库的键都是永久的。
    let ttl_text: SharedString = match detail.ttl {
        -2 => SharedString::from(tr(cx, "键不存在")),
        -1 => SharedString::from(tr(cx, "永久")),
        seconds => format_ttl_chip(seconds)
            .map(SharedString::from)
            .unwrap_or_else(|| trf(cx, "{} 秒", &[&seconds.to_string()]).into()),
    };
    // 编码 · 空闲（`OBJECT` 拿不到就整段不画——与右栏详情同一口径）。
    let object_text = match (&detail.encoding, detail.idletime_secs) {
        (Some(encoding), Some(idle)) => Some(format!(
            "{encoding} · {}",
            trf(cx, "空闲 {} 秒", &[&idle.to_string()])
        )),
        (Some(encoding), None) => Some(encoding.clone()),
        (None, Some(idle)) => Some(trf(cx, "空闲 {} 秒", &[&idle.to_string()]).to_owned()),
        (None, None) => None,
    };
    let is_string = detail.key_type == KeyType::String;

    let mut header = h_flex()
        .flex_none()
        .w_full()
        .h(px(VALUE_BAR_HEIGHT))
        .items_center()
        .gap(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(
            // 复制键值（在星标之前，对齐 tiny-rdm 的头部图标序）：
            // 复制的是当前展示串（解码档位之后），与界面所见一致。
            icon_button(
                "redis-value-copy",
                tr(cx, "复制键值"),
                Icon::new(DuckIcon::Copy).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                let key = detail.key.clone();
                move |_, window, cx| {
                    let _ =
                        view.update(cx, |this, cx| this.copy_key_value(key.clone(), window, cx));
                }
            }),
        )
        .child(
            // 收藏星：金星 = 已收藏，弱化色 = 未收藏（与键树行的星同色源）。
            icon_button(
                "redis-value-favorite",
                if favorite {
                    tr(cx, "取消收藏")
                } else {
                    tr(cx, "收藏这个键")
                },
                Icon::new(DuckIcon::Star)
                    .size(extended.icon.sm)
                    .text_color(if favorite {
                        star_color
                    } else {
                        tokens.colors.muted_foreground
                    }),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                let key = detail.key.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_key_favorite(&key, cx));
                }
            }),
        )
        .child(key_type_pill(detail.key_type, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(tokens.typography.sm.size)
                .text_color(tokens.colors.foreground)
                .child(selectable_text(SharedString::from(detail.key.clone()))),
        )
        .child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(size_text)),
        );
    if let Some(object_text) = object_text {
        header = header.child(
            div()
                .flex_none()
                .text_size(tokens.typography.xs.size)
                .text_color(tokens.colors.muted_foreground)
                .child(selectable_text(SharedString::from(object_text))),
        );
    }
    header = header
        .child(
            // TTL 下拉（2026-10-16，对齐 tiny-rdm）：展示当前剩余时长，
            // 点开三档——按时长设置 / 按到期时刻设置 / 移除 TTL。
            // 取代了原先的纯展示芯片：编辑入口从此就在头部，右栏「键管理」保留。
            toolbar_text_dropdown_button(
                "redis-value-ttl-menu",
                ttl_text,
                Icon::new(DuckIcon::Clock)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                Anchor::BottomRight,
                {
                    // 菜单闭包每次打开都会重跑：三个条目各拿一份 `view` / `key`
                    // 的副本（`view` 是 `WeakEntity`，不 `Copy`，不能共用一份）。
                    let view = view.clone();
                    let key = detail.key.clone();
                    move |menu, _, cx| {
                        menu.item(
                            PopupMenuItem::new(tr(cx, "按时长设置"))
                                .icon(Icon::new(DuckIcon::Clock))
                                .on_click({
                                    let view = view.clone();
                                    let key = key.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.open_ttl_secs_dialog(
                                                TtlPromptTarget::One { key: key.clone() },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                        )
                        .item(
                            PopupMenuItem::new(tr(cx, "按到期时刻设置…"))
                                .icon(Icon::new(DuckIcon::Calendar))
                                .on_click({
                                    let view = view.clone();
                                    let key = key.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.open_ttl_at_dialog(key.clone(), window, cx);
                                        });
                                    }
                                }),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(tr(cx, "移除 TTL"))
                                .icon(Icon::new(DuckIcon::Eraser))
                                .on_click({
                                    let view = view.clone();
                                    let key = key.clone();
                                    move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.confirm_or_run(
                                                Capability::SetTtl,
                                                PendingAction::ClearTtl { key: key.clone() },
                                                window,
                                                cx,
                                            );
                                        });
                                    }
                                }),
                        )
                    }
                },
                cx,
            ),
        )
        .child(
            icon_button(
                "redis-value-refresh",
                tr(cx, "重新读取这个键"),
                Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
            .on_click({
                let view = view.clone();
                move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.refresh_selected_key(window, cx));
                }
            }),
        );
    // 字符串值：内容区本身就是编辑器（zedis 同款），**改了才出现**保存 / 放弃——
    // 没改时按钮常驻只会让人犹豫「我到底改没改」。放弃 = 把详情原文回填进
    // 编辑器（`value_modified` 由订阅里的比对自动归零）。
    if is_string && page.value_modified {
        header = header
            .child(
                button(
                    "redis-value-discard",
                    tr(cx, "放弃修改"),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.sync_value_editor(window, cx));
                    }
                }),
            )
            .child(
                button(
                    "redis-value-save",
                    tr(cx, "保存值"),
                    ButtonVariant::Primary,
                    cx,
                )
                .disabled(!can_write)
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.save_string_value(window, cx));
                    }
                }),
            );
    }
    // 解码下拉：只有字符串值有九档可切；其余类型整块不画（不是画一个灰按钮）。
    if is_string {
        let current = page.decode;
        header = header.child(toolbar_dropdown_button(
            "redis-decode-menu",
            tr(cx, "切换解码档位"),
            Icon::new(DuckIcon::Code)
                .size(extended.icon.sm)
                .text_color(tokens.colors.muted_foreground),
            Anchor::BottomRight,
            {
                let items: Vec<(DecodeKind, SharedString)> = DecodeKind::ALL
                    .iter()
                    .map(|&mode| (mode, SharedString::from(decode_label(mode, cx))))
                    .collect();
                let view = view.clone();
                move |menu, _, _| {
                    items.iter().fold(menu, |menu, &(mode, ref label)| {
                        let view = view.clone();
                        menu.item(
                            PopupMenuItem::new(label.clone())
                                .checked(mode == current)
                                .on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.switch_decode(mode, window, cx)
                                    });
                                }),
                        )
                    })
                }
            },
            cx,
        ));
    }
    header
}

/// JSONPath 栏（对齐 zedis 的 `bytes_editor::render_jsonpath_bar`）：
/// 左侧「JSONPath：」+ 输入框 + 运行，右侧「文本 | 树」切换。
///
/// 树档仍然只在 JSON 解码档可用（判据在 [`RedisPage::json_available`] 一处），
/// 按钮灰显的语义与从前一致——这里只是搬了位置。
fn jsonpath_bar(
    page: &RedisPage,
    detail: &KeyDetail,
    view: &WeakEntity<RedisPage>,
    cx: &App,
) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    let effective = page.effective_value_view();
    let available = page.json_available();

    let mut views = h_flex().flex_none().items_center().gap(px(2.));
    for mode in ValueViewMode::ALL {
        let usable = mode != ValueViewMode::Tree || available;
        let selected = mode == effective;
        views = views.child(
            button(
                SharedString::from(format!("redis-value-view-{}", mode.wire_name())),
                value_view_label(mode, cx),
                if selected {
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

    // JSONPath 只对「字符串值的文本形态」有意义：其余类型输入框整体灰显
    // （按钮可点、报错也行，但灰输入框能把「查不了」说在点击之前）。
    let jsonpath_usable =
        detail.key_type == KeyType::String && matches!(detail.view, ValueView::Text { .. });

    h_flex()
        .flex_none()
        .w_full()
        .h(px(VALUE_BAR_HEIGHT))
        .items_center()
        .gap(tokens.spacing.xs)
        .border_b(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .child(meta_text(tr(cx, "JSONPath："), cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(theme.density().control)
                .px(tokens.spacing.sm)
                .rounded(theme.components().input_radius)
                .border(extended.stroke.thin)
                .border_color(extended.colors.hairline)
                .bg(tokens.colors.background)
                .when(!jsonpath_usable, |box_| {
                    box_.opacity(extended.opacity.disabled)
                })
                .child(
                    Input::new(&page.jsonpath_input)
                        .appearance(false)
                        .bordered(false)
                        .w_full(),
                ),
        )
        .child(
            button(
                "redis-jsonpath-run",
                tr(cx, "运行"),
                ButtonVariant::Secondary,
                cx,
            )
            .disabled(!jsonpath_usable)
            .on_click({
                let view = view.clone();
                move |_, _window, cx| {
                    let _ = view.update(cx, |this, cx| this.run_jsonpath(cx));
                }
            }),
        )
        .child(views)
}

/// JSONPath 的求值结果条：错误一行，命中一个可滚动的小列表（含总数）。
///
/// 放在 JSONPath 栏与值本体之间（zedis 同位）。**命中行可点**：点一下切到
/// 树档、展开祖先、滚动到那一行并高亮（[`RedisPage::jump_to_jsonpath_hit`]）；
/// 被定位过的那条命中自己也垫选中底色，两边对得上号。
fn jsonpath_results(page: &RedisPage, view: &WeakEntity<RedisPage>, cx: &App) -> AnyElement {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    if let Some(error) = &page.jsonpath_error {
        return div()
            .w_full()
            .flex_none()
            .text_size(tokens.typography.xs.size)
            .text_color(theme.extended().colors.status_failed)
            .child(selectable_text(SharedString::from(error.clone())))
            .into_any_element();
    }
    let hits = page.jsonpath_hits.as_ref();
    if hits.is_empty() {
        return div().into_any_element();
    }
    let mut list = v_flex()
        .w_full()
        .flex_none()
        .max_h(px(160.))
        .id("redis-jsonpath-results")
        .overflow_y_scroll()
        .gap(tokens.spacing.xxs)
        .p(tokens.spacing.xxs)
        .rounded(theme.components().input_radius)
        .border(theme.extended().stroke.thin)
        .border_color(theme.extended().colors.hairline)
        .bg(tokens.colors.background)
        .child(meta_text(
            trf(cx, "命中 {} 项", &[&hits.len().to_string()]),
            cx,
        ));
    for (index, hit) in hits.iter().enumerate() {
        let path = hit.path.clone();
        let located = page.jsonpath_selected.as_deref() == Some(path.as_str());
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("redis-jsonpath-hit-{index}")))
                .w_full()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.xxs)
                .rounded(theme.components().input_radius)
                .role(Role::Button)
                .aria_label(trf(cx, "在树中定位 {}", &[&path]))
                .cursor_pointer()
                .hover(|style| style.bg(theme.extended().colors.hover_tint))
                .when(located, |row| row.bg(theme.extended().colors.nav_selected))
                .on_click({
                    let view = view.clone();
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        let path = path.clone();
                        let _ = view.update(cx, |this, cx| {
                            this.jump_to_jsonpath_hit(&path, cx);
                        });
                    }
                })
                .child(
                    div()
                        .flex_none()
                        .min_w(px(180.))
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.foreground)
                        .child(selectable_text(SharedString::from(hit.path.clone()))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(selectable_text(SharedString::from(hit.preview.clone()))),
                ),
        );
    }
    list.into_any_element()
}
