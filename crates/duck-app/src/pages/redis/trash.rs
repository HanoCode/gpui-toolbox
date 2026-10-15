//! 本地**回收站**：软删除的键 24 小时内可恢复（对齐 zedis 的 `trash_dialog` +
//! `zedis-db/src/trash.rs`；模型与全部常量见内核 [`duck_redis::trash`]）。
//!
//! # 三条数据流
//!
//! 1. **入站**（删键时）：`keys.rs::delete_key_at` 在发 `DEL` **之前**用
//!    [`capture_stash`] 抄一份（`MEMORY USAGE` 前置闸 → `PTTL` → `DUMP` → 长度闸），
//!    删除成功后回主线程走 [`RedisPage::stash_deleted_key`] 落进 kv。
//!    ⚠️ 抄不下来**绝不影响删除本身**——回收站是安全网，不是一个能让删除失败的依赖。
//! 2. **出站**（恢复）：走 `RESTORE`，是**写操作**，所以过写闸门
//!    （[`Capability::WriteKey`]，与 zedis 在回收站里补上 `Capability::ImportKeys` 同一考量：
//!    只读会话 / 未解锁的生产档下不恢复）。
//! 3. **本地摘行**（永久移除 / 恢复成功后）：纯本地，锁写闸门管不着。
//!
//! # 恢复为什么要另开一条连接
//!
//! `RESTORE` 落在**当前 `SELECT` 的那个库**上，而回收站按「连接」隔离、**跨库**列出
//! （一条记录带着它被删时所在的 `db`）。在页面那条活会话上 `SELECT` 来 `SELECT` 去
//! 会与状态栏缓存的 `active_config().db` 脱节（切一次库就永久错位，见 §4.8.2 的
//! `switch_db` 顺序）。所以恢复走 [`restore_entries`] 里那条**专用连接**：建连、
//! 按条目 `SELECT` 回它的库、`RESTORE`、用完即弃。代价是一次建连，换来的是
//! 「跨库恢复」天然正确、且不碰页面会话的任何状态。zedis 的 `ServerDb::new(server_id, db)`
//! 是同一件事（它那边是连接池里按 db 取一条）。
//!
//! # 为什么弹窗是独立视图
//!
//! 正文里有过滤输入框，要「边打边过滤」，就必须有一个自己的 `Render` 与自己的
//! `notify()`——宿主页面调 `cx.notify()` **到不了** `content` 闭包（同
//! `duck-components/src/dialog.rs` 模块头那条坑）。所以 [`TrashDialog`] 是
//! `Entity`，并且**自己订阅**页面（页面里的回收站变了要跟着重绘）。

use super::*;

/// 回收站弹窗宽度（与 zedis 的 `px(560.)` 一致）。
const TRASH_DIALOG_WIDTH: f32 = 560.;
/// 列表超过这么多条才给固定高度的滚动视口；再少就直接铺开。
///
/// ⚠️ 与 zedis 同款坑：**不能用 `max_h`** —— `Scrollable` 会把调用方的尺寸样式
/// 抄到内层内容上，强制 `h_auto` 不会重置 `max_h`，于是被夹住的是内容本身、永远
/// 滚不动。要么定高视口，要么不滚。
const TRASH_LIST_INLINE_MAX: usize = 7;
/// 滚动视口高度。
const TRASH_LIST_VIEWPORT: f32 = 320.;

/// 一次「删除前抄一份」的结果。
pub(super) struct KeyStash {
    /// 删除时剩余的 TTL（毫秒）；`-1` = 无过期（`PTTL` 读不到时也退成它）。
    pttl_ms: i64,
    /// `DUMP` 的原样字节。
    payload: Vec<u8>,
}

/// 尽力把即将被删的键抄一份下来（两道体积闸都在这里）。
///
/// **任何一步失败都只表示「这一份不入站」**：返回 `None`，调用方照旧发 `DEL`。
/// 这是 zedis `stash_key_to_trash` 的口径——回收站永远不该把一个本来能成功的删除
/// 变成一个错误。
///
/// 两道闸的顺序不能反：① `MEMORY USAGE` 是**估算**，先用它挡掉「一看就太大」的键，
/// 省掉一次昂贵的 `DUMP`；② `DUMP` 回来后按**精确长度**再判一次，因为内存占用
/// 普遍高估序列化后的体积，中等的值会落在两道闸之间。
pub(super) fn capture_stash(conn: &mut RedisConnection, key: &str) -> Option<KeyStash> {
    // ① 前置闸：探测失败（`None`）放行，见 `should_stash`。
    if !should_stash(conn.memory_usage(key).ok().flatten()) {
        return None;
    }
    // `PTTL` 读不到就当「无过期」：抄下来的副本本身还是好的，
    // 为一条读不到的 TTL 把整份丢掉不值。
    let pttl_ms = conn.pttl(key).unwrap_or(-1);
    let payload = conn.dump_key(key).ok()?;
    // ② 精确闸。
    payload_fits(payload.len()).then_some(KeyStash { pttl_ms, payload })
}

/// 一次恢复尝试的汇总。
pub(super) struct RestoreSummary {
    /// 成功恢复的回收站句柄（本地要把这些行摘掉）。
    pub restored: Vec<String>,
    /// `BUSYKEY`：同名键在这期间又被建出来了。**算跳过、不算失败**——恢复不该覆盖。
    pub skipped: usize,
    /// 其它失败（连不上 / 目标库不存在 / 载荷坏了）。
    pub failed: usize,
}

impl RestoreSummary {
    fn any_success(&self) -> bool {
        !self.restored.is_empty()
    }
}

/// 按顺序恢复一批回收站条目（**专用连接**，理由见模块头）。
///
/// 同库的连续条目只 `SELECT` 一次（条目按删除时间排，同库的自然相邻的概率不高，
/// 但这一步几乎不要钱）。
pub(super) fn restore_entries(
    cfg: &RedisConfig,
    entries: &[TrashEntry],
) -> Result<RestoreSummary, RedisError> {
    let password = load_password(&cfg.id).ok().flatten();
    let mut conn = RedisConnection::connect(cfg, password.as_deref())?;
    let mut summary = RestoreSummary {
        restored: Vec::new(),
        skipped: 0,
        failed: 0,
    };
    let mut selected: Option<u32> = None;
    for entry in entries {
        // 条目记得自己原来在哪个库 —— 恢复必须回到那个库。
        if selected != Some(entry.db) {
            if conn.select(entry.db).is_err() {
                summary.failed += 1;
                // 不更新 `selected`：下一条若是同一个库还会再试一次，
                // 失败了也只是再多记一次 failed，不会把条目写进错误的库。
                continue;
            }
            selected = Some(entry.db);
        }
        match conn.restore_key(&entry.key, entry.restore_pttl_ms(), &entry.payload) {
            Ok(()) => summary.restored.push(entry.id()),
            Err(error) if error.to_string().contains("BUSYKEY") => summary.skipped += 1,
            Err(_) => summary.failed += 1,
        }
    }
    Ok(summary)
}

impl RedisPage {
    /// 当前连接回收站里的列表行（最新在前，不带载荷）。
    pub(super) fn active_trash_metas(&self) -> Vec<TrashMeta> {
        self.active
            .as_deref()
            .map_or_else(Vec::new, |id| self.trash.metas(id))
    }

    /// 当前连接的回收站条目数（工具栏入口的角标）。
    pub(super) fn active_trash_len(&self) -> usize {
        self.active.as_deref().map_or(0, |id| self.trash.len(id))
    }

    /// 从 kv 读全部连接的回收站。
    ///
    /// 解析失败 / 版本对不上都由 `TrashStore::from_json` 收成空表（口径见内核
    /// `TRASH_STORE_VERSION`：宁可这次没读出来，也不拿变了形的东西覆盖它）。
    pub(super) fn load_trash(cx: &App) -> TrashStore {
        duck_store::kv_get(cx, KEY_TRASH)
            .map(|json| TrashStore::from_json(&json))
            .unwrap_or_else(TrashStore::new)
    }

    /// 把整本回收站写回 kv。
    ///
    /// 不需要显式 `flush`：kv 单写线程自己去抖落盘，退出时 `on_app_quit` 也会兜一次
    /// （与连接档案 / 键标注同一条路）。
    pub(super) fn persist_trash(&self, cx: &App) {
        match self.trash.to_json() {
            Ok(json) => duck_store::kv_put(cx, KEY_TRASH, json),
            // 结构里只有 `BTreeMap` / `Vec` / 标量，理论上不会失败；真失败了也只丢
            // 这一次写入（内存里的回收站还在），不该把界面带下去。
            Err(error) => tracing::warn!(%error, "serialize redis trash failed"),
        }
    }

    /// 把刚删掉的键的副本入站（由 `delete_key_at` 在**删除成功后**调用）。
    ///
    /// 顺手清一次过期条目：这是两个清理触发点之一（另一个是打开回收站时），
    /// 对齐 zedis `stash_key_to_trash` 里的 opportunistic cleanup。**不设定时器**
    /// ——回收站不需要「准时」，只需要「别无限长」。
    pub(super) fn stash_deleted_key(
        &mut self,
        key: &str,
        db: u32,
        stash: KeyStash,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.active.clone() else {
            return;
        };
        let entry = TrashEntry {
            key: key.to_owned(),
            db,
            pttl_ms: stash.pttl_ms,
            deleted_at_ms: now_millis(),
            payload: stash.payload,
        };
        let now = entry.deleted_at_ms;
        if self.trash.insert(&connection, entry).is_none() {
            // 精确闸在 `capture_stash` 里已经判过，这里是兜底：真走到了也只能不入站，
            // 删除照旧生效。
            tracing::warn!(key, "redis trash refused an oversized payload");
            return;
        }
        self.trash.purge_expired(&connection, now);
        self.trash.drop_if_empty(&connection);
        self.persist_trash(cx);
        cx.notify();
    }

    /// 本地摘掉回收站里的一行（永久移除，或恢复成功之后）。
    ///
    /// **纯本地、不联网**，所以不过写闸门——它改的是本机的 kv，不是服务端数据。
    pub(super) fn drop_trash_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(connection) = self.active.clone() else {
            return;
        };
        if !self.trash.remove(&connection, id) {
            return;
        }
        self.trash.drop_if_empty(&connection);
        self.persist_trash(cx);
        cx.notify();
    }

    /// 恢复一条或多条（`ids` 为空则什么都不做）。`window` 用于闸门拒绝时的提示。
    ///
    /// 单条与批量走同一条路：批量只是「一次建连、按条目逐个 `SELECT` + `RESTORE`」。
    pub(super) fn restore_trash(
        &mut self,
        ids: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `RESTORE` 是写：只读会话 / 未解锁的生产档下不给恢复。
        if let Err(reason) = self.check(Capability::WriteKey) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(connection) = self.active.clone() else {
            return;
        };
        let Some(cfg) = self.active_config().cloned() else {
            return;
        };
        let entries: Vec<TrashEntry> = ids
            .iter()
            .filter_map(|id| self.trash.get(&connection, id).cloned())
            .collect();
        if entries.is_empty() {
            notify::warning(tr(cx, "这些条目已经不在回收站里了"), window, cx);
            return;
        }
        let total = entries.len();
        // 单条恢复要报出键名，所以先把名字拷出来（载荷不进消息）。
        let names: Vec<String> = entries.iter().map(|entry| entry.key.clone()).collect();
        self.spawn_redis(
            cx,
            move || restore_entries(&cfg, &entries),
            move |this, result, window, cx| match result {
                Ok(summary) => {
                    // 成功的先摘掉；跳过 / 失败的留在回收站里等下次。
                    for id in &summary.restored {
                        this.drop_trash_row(id, cx);
                    }
                    if summary.any_success() {
                        // 键（可能）又回来了：重扫键列表 + 刷新状态栏的键数 / 内存。
                        this.refresh_keys(cx);
                        this.probe_status(cx);
                    }
                    if summary.failed == 0 && summary.skipped == 0 {
                        let message = if total == 1 {
                            trf(cx, "已恢复键 {}", &[&names[0]])
                        } else {
                            trf(cx, "已恢复 {} 个键", &[&total.to_string()])
                        };
                        notify::success(SharedString::from(message), window, cx);
                    } else {
                        // 有不成功的一律走**告警**：用户需要知道有几个没回来。
                        let detail = if total == 1 {
                            names[0].clone()
                        } else {
                            total.to_string()
                        };
                        notify::warning(
                            SharedString::from(trf(
                                cx,
                                "{}：恢复 {}、跳过 {}、失败 {}",
                                &[
                                    &detail,
                                    &summary.restored.len().to_string(),
                                    &summary.skipped.to_string(),
                                    &summary.failed.to_string(),
                                ],
                            )),
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                }
                Err(error) => {
                    // 建连就失败了：一条都没动，回收站原样留着。
                    notify::error(
                        SharedString::from(trf(cx, "恢复失败：{}", &[&error.to_string()])),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 连接被删掉时，连它的回收站一起丢。
    ///
    /// 不丢的话盘上会积一份再也点不到的载荷（最大可达十几 MB）。与
    /// [`Self::forget_connection_tags`] 同一处调用点、同一条理由。
    pub(super) fn forget_connection_trash(&mut self, connection: &str, cx: &Context<Self>) {
        if self.trash.len(connection) == 0 {
            return;
        }
        self.trash.forget_connection(connection);
        self.persist_trash(cx);
    }

    /// 打开回收站弹窗。
    ///
    /// 打开时先清一遍过期条目（对齐 zedis 的 `reload`：**先清后列**，于是列表里
    /// 永远不会出现「已经恢复不了」的行）。
    pub(super) fn open_trash_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.active.clone() else {
            return;
        };
        if self.trash.purge_expired(&connection, now_millis()) > 0 {
            self.trash.drop_if_empty(&connection);
            self.persist_trash(cx);
        }
        let page = cx.entity();
        let dialog = cx.new(|cx| TrashDialog::new(page, window, cx));
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |builder, _, cx| {
            let offset = modal_offset(&drag, cx);
            let body = dialog.clone();
            let drag_for_content = drag.clone();
            builder
                .w(px(TRASH_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                // 库默认会在面板右上角自画一个 ✕；关闭入口统一走 `modal_title_row`，
                // 这里必须关掉，否则会同时出现两个关闭按钮（同连接编辑弹窗）。
                .close_button(false)
                .button_props(DialogButtonProps::default().ok_text(tr(cx, "完成")))
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "redis-trash-title",
                                &drag_for_content,
                                tr(cx, "回收站"),
                                cx,
                            ))
                            .child(body.clone()),
                    ))
                })
        });
    }
}

/// 回收站弹窗正文（**独立视图**，理由见模块头）。
pub(super) struct TrashDialog {
    /// 宿主页面。**强引用**不会成环：弹窗栈持有本视图，本视图持有页面，
    /// 而页面不持有弹窗。
    page: Entity<RedisPage>,
    /// 键名过滤框（子串，本地筛）。
    filter: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl TrashDialog {
    fn new(page: Entity<RedisPage>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = text_input(window, cx, tr(cx, "过滤键名"));
        let mut subscriptions = Vec::new();
        // 自己打自己过滤：输入变化 ⇒ 本视图重绘（不用惊动页面）。
        subscriptions.push(cx.subscribe(&filter, |_this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        }));
        // 页面里的回收站变了（新删 / 恢复 / 移除）⇒ 也重绘。
        // ⚠️ 必须自己订阅：页面的 `notify()` 到不了 `content` 闭包里的这个视图。
        subscriptions.push(cx.observe(&page, |_this, _, cx| cx.notify()));
        Self {
            page,
            filter,
            _subscriptions: subscriptions,
        }
    }
}

/// 一条回收站记录的元信息行：`db3 · 5 分钟前 · TTL 60s · 1.2 KB`。
///
/// 与 zedis 同款：**TTL 只在有 TTL 时显示**（`pttl_ms > 0`），因为它回答的是
/// 「恢复之后这个键还能活多久」，没有 TTL 的键这句话不成立。
fn entry_meta_line(meta: &TrashMeta, cx: &App) -> String {
    let mut text = trf(
        cx,
        "db{} · {}",
        &[
            &meta.db.to_string(),
            &relative_age(meta.deleted_at_ms / 1000, now_secs(), cx),
        ],
    );
    if meta.pttl_ms > 0 {
        text.push_str(&trf(
            cx,
            " · TTL {}s",
            &[&(meta.pttl_ms / 1000).to_string()],
        ));
    }
    text.push_str(&format!(" · {}", format_bytes(meta.bytes)));
    text
}

impl Render for TrashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();

        // 读页面：**先把要的值取出来、把读锁放掉**，否则下面 `self.page.update(...)`
        // 会与这次读冲突。列表与过滤都在这一步算好（不调 `visible`——那会再读一次页面）。
        let keyword = self.filter.read(cx).value().to_string();
        let (rows, connected, can_restore, busy) = {
            let page = self.page.read(cx);
            let mut rows = page.active_trash_metas();
            if !keyword.is_empty() {
                rows.retain(|row| row.key.contains(keyword.as_str()));
            }
            let can_restore = page
                .gate()
                .as_ref()
                .is_some_and(|gate| gate.can(Capability::WriteKey));
            (rows, page.active.is_some(), can_restore, page.busy)
        };
        let total = rows.len();
        let restrict = !connected || !can_restore || busy;

        let mut list = v_flex().w_full().gap(tokens.spacing.xs);
        if rows.is_empty() {
            list = list.child(panel_empty(tr(cx, "回收站是空的"), cx));
        }

        for (index, meta) in rows.iter().enumerate() {
            let restore_id = meta.id.clone();
            let remove_id = meta.id.clone();
            list = list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(body_text(SharedString::from(meta.key.clone()), cx))
                            .child(meta_text(SharedString::from(entry_meta_line(meta, cx)), cx)),
                    )
                    .child(
                        button(
                            SharedString::from(format!("redis-trash-restore-{index}")),
                            tr(cx, "恢复"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .disabled(restrict)
                        .on_click({
                            let page = self.page.clone();
                            move |_, window, cx| {
                                let ids = vec![restore_id.clone()];
                                let _ =
                                    page.update(cx, |page, cx| page.restore_trash(ids, window, cx));
                            }
                        }),
                    )
                    .child(
                        button(
                            SharedString::from(format!("redis-trash-remove-{index}")),
                            tr(cx, "移除"),
                            ButtonVariant::Ghost,
                            cx,
                        )
                        .on_click({
                            let page = self.page.clone();
                            move |_, _window, cx| {
                                let _ =
                                    page.update(cx, |page, cx| page.drop_trash_row(&remove_id, cx));
                            }
                        }),
                    ),
            );
        }

        let visible_ids: Vec<String> = rows.iter().map(|meta| meta.id.clone()).collect();

        v_flex()
            .w_full()
            .gap(tokens.spacing.sm)
            .child(meta_text(
                tr(
                    cx,
                    "删除的键在本机保留 24 小时，到期自动清除。恢复会把键连同删除时的过期时间写回它原来的库。",
                ),
                cx,
            ))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(panel_input_box(&self.filter, cx))
                    .child(
                        button(
                            "redis-trash-restore-all",
                            tr(cx, "全部恢复"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        // 有过滤时「全部」= 当前看到的那一批（对齐 zedis），
                        // 所以禁用条件看 `visible_ids` 而不是总条数。
                        .disabled(restrict || visible_ids.is_empty())
                        .on_click({
                            let page = self.page.clone();
                            move |_, window, cx| {
                                let ids = visible_ids.clone();
                                let _ = page
                                    .update(cx, |page, cx| page.restore_trash(ids, window, cx));
                            }
                        }),
                    ),
            )
            .child(if total > TRASH_LIST_INLINE_MAX {
                // ⚠️ 滚动容器的 `.id()` 不能省：本仓的 GPUI 分叉把 `overflow_y_scroll`
                // 挂在 `StatefulInteractiveElement` 上（有滚动位置就要有 ElementId），
                // 裸 `div()` 没有这个方法（同 `mail.rs` 的 `mail-sidebar-scroll`）。
                // 配合上面定的**视口高度**（不是 `max_h`，那个会被 `Scrollable`
                // 抄进内层、永远滚不动）。
                div()
                    .id("redis-trash-scroll")
                    .w_full()
                    .h(px(TRASH_LIST_VIEWPORT))
                    .overflow_y_scroll()
                    .child(list)
                    .into_any_element()
            } else {
                div().w_full().child(list).into_any_element()
            })
            .when(busy, |this| {
                this.child(meta_text(tr(cx, "正在恢复…"), cx))
            })
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .pt(tokens.spacing.sm)
    }
}
