//! 状态栏：探测调度（`PING` 计时 + `INFO`）、四段渲染、DB 切换、类型统计。
//!
//! 对照 zedis：状态栏**左半段**是「这一页刚做了什么」（`self.status`，由各功能自己写），
//! **右半段**是「这台服务端现在什么样」（本模块独占写入）。一个数字只有一个写入方，
//! 才不会出现「谁把它覆盖了」的问题。
//!
//! # 探测只在事件上跑（**不轮询**）
//!
//! 一次探测 = 一次计时 `PING` + 一条 `INFO memory` / `INFO keyspace` 的 pipeline
//! （两条命令一个往返）。触发点只有三个：
//!
//! 1. 连接成功（`connect_entry` 的完成回调）；
//! 2. 切换数据库（[`RedisPage::switch_db`]）；
//! 3. 用户点右端的「重新探测」按钮。
//!
//! **刻意不做定时轮询**：空闲页面每秒往服务端发三条命令是纯浪费，而本模块的两条硬
//! 纪律（不用 `KEYS`、不整读大集合）都是冲着「别给服务端添无谓负载」去的。代价是
//! 数字会陈旧——所以刷新按钮一直在，而且**探测失败不清空旧值**：显示一个略微过期的
//! 真值，比显示空白有用得多。
//!
//! # 类型统计的口径（诚实优先）
//!
//! 「字符串 6 · 哈希 2」数的是**已加载的键**（`key_meta` 里已经有元数据的那部分），
//! 不是整个键空间。逐类型的精确计数要跑六七遍 `SCAN TYPE`，百万键下就是六七次全库
//! 遍历——正是硬纪律要避免的那类操作。所以它紧跟在「`N / M 键`」后面，读起来就是
//! 「这 N 个键的构成」，而不是「全库的构成」。

use super::*;
use duck_components::selectable_text;

// ----------------------------------------------------------------------
// 文案（纯函数，可直接单测）
// ----------------------------------------------------------------------

/// 延迟文案：`0.42 ms` / `1.2 ms` / `12 ms` / `1.05 s`。
///
/// 三档精度是刻意的：亚毫秒要两位（不然看不出 0.3 与 0.4 的差），十毫秒以内留一位，
/// 再往上取整——「12.37 ms」那两位小数在抖动面前是假精度。超过一秒改报秒，
/// 免得出现「1523 ms」这种要读者自己除的数。
#[allow(clippy::cast_precision_loss)] // 微秒数远超 2^53 不可能；这里的除法只为显示
fn latency_text(latency: Duration) -> String {
    let micros = latency.as_micros();
    match micros {
        0..=999 => format!("{:.2} ms", micros as f64 / 1000.),
        1_000..=9_999 => format!("{:.1} ms", micros as f64 / 1000.),
        _ if latency < Duration::from_secs(1) => format!("{} ms", latency.as_millis()),
        _ => format!("{:.2} s", latency.as_secs_f64()),
    }
}

/// 内存文案：`1.05M`；服务端设了上限时是 `1.05M / 512.00M`。
///
/// ⚠️ 两个数都**直接沿用服务端给的 `*_human` 串**，不自己按 1024 换算：自己算出来的
/// `1.0MB` 会和服务端显示的 `1.05M` 不一致，用户会以为是两个数。
fn memory_text(overview: &MemoryOverview) -> String {
    let Some(used) = overview.used_human.as_deref() else {
        return String::new();
    };
    match overview.max_human.as_deref() {
        Some(max) => format!("{used} / {max}"),
        None => used.to_owned(),
    }
}

// ----------------------------------------------------------------------
// 段构造（每段各返回 `Option`：拿不到就不渲染空格子）
// ----------------------------------------------------------------------

/// 延迟值（不带前缀）：`0.42 ms` / `20 ms`。
///
/// 对齐 zedis 的状态栏读法：裸数值 + 图标，不写「延迟」前缀——图标就是前缀。
fn latency_value(page: &RedisPage) -> Option<String> {
    page.probe
        .latency
        .as_ref()
        .map(|latency| latency_text(*latency))
}

/// 内存值（不带前缀）：`1.05M`（设了上限 `1.05M / 512.00M`）。
fn memory_value(page: &RedisPage) -> Option<String> {
    let text = memory_text(page.probe.memory.as_ref()?);
    (!text.is_empty()).then_some(text)
}

/// 状态栏的 DB 选择器。
///
/// 选项 = `0..=MAX_DB`（内核 `MAX_DB = 15`，与 Redis 默认 `databases 16` 对齐）。
/// **不只列非空库**：`INFO keyspace` 只报有键的库，只列它们会让「切到一个空库写进去」
/// 没有入口；非空库在菜单里缀上键数就够了。
///
/// ⚠️ 按钮文字与菜单文字**刻意不同**：按钮上只有 `db3`（状态栏要窄），菜单里才是
/// `db3 · 12 键`。所以这里不用通用的 [`toolbar_switcher`]（它的按钮文字取自当前项的
/// 菜单文字），而是直接用 `toolbar_text_dropdown_button`。
///
/// `Anchor::BottomLeft`：状态栏在窗口底部，弹层要**向上**展开；`BottomLeft` 让弹层的
/// 左下角贴住按钮的左上角，同时左边缘对齐（菜单不会顶出右缘）。
fn db_switcher(page: &RedisPage, cx: &App) -> AnyElement {
    let current = page.active_config().map_or(0, |cfg| cfg.db);
    let options: Vec<(u32, SharedString)> = (0..=MAX_DB)
        .map(|db| {
            let label = match page.probe.keyspace.iter().find(|stats| stats.db == db) {
                Some(stats) => trf(
                    cx,
                    "db{} · {} 键",
                    &[&db.to_string(), &stats.keys.to_string()],
                ),
                None => format!("db{db}"),
            };
            (db, SharedString::from(label))
        })
        .collect();
    let view = page.view.clone();
    let icon = Icon::new(DuckIcon::Database).size(active_theme(cx).extended().icon.sm);
    toolbar_text_dropdown_button(
        "redis-db-switcher",
        SharedString::from(format!("DB {current}")),
        icon,
        Anchor::BottomLeft,
        move |menu, _, _| {
            options.iter().fold(menu, |menu, (db, label)| {
                let db = *db;
                let view = view.clone();
                menu.item(
                    PopupMenuItem::new(label.clone())
                        .checked(db == current)
                        .on_click(move |_, _window, cx| {
                            let _ = view.update(cx, |this, cx| this.switch_db(db, cx));
                        }),
                )
            })
        },
        cx,
    )
}

// ----------------------------------------------------------------------
// 探测与切换
// ----------------------------------------------------------------------

/// 当前值的**格式标签**（zedis `data_format`：Binary 图标 + 小写格式名，非交互）。
fn data_format_label(page: &RedisPage, cx: &App) -> SharedString {
    match page.decode {
        DecodeKind::Json => SharedString::from("json"),
        DecodeKind::Text => SharedString::from("text"),
        DecodeKind::Hex => SharedString::from("hex"),
        other => SharedString::from(decode_label(other, cx)),
    }
}

impl RedisPage {
    /// 底栏的值编辑器三件套：软换行开关 / 格式标签 / 视图切换。
    ///
    /// 对齐 zedis（`status_bar.rs:1671-1741`）：「✓软换行」是**开时才带对勾**的
    /// 开关（体量超 [`SOFT_WRAP_MAX_BYTES`] 灰显并说明）；「json」是**非交互**的
    /// 格式读数；「视图：」下拉三档——Auto 重新按内容检测（JSON → JSON 档）、
    /// 纯文本 / Hex 直切解码档（与本仓的九档解码下拉同一真源，两处入口）。
    fn render_editor_controls(&self, cx: &App) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();

        let raw_len = self
            .detail
            .as_ref()
            .map(|detail| detail.raw.len())
            .unwrap_or(0);
        let wrap_fits = raw_len <= SOFT_WRAP_MAX_BYTES;

        // 软换行：开 = 前置对勾；超限灰显（排版几十万行会拖垮一帧，zedis 同款上限）。
        let wrap_button = if self.soft_wrap {
            icon_text_button(
                "redis-soft-wrap",
                tr(cx, "软换行"),
                Icon::new(DuckIcon::Check).size(extended.icon.sm),
                ButtonVariant::Ghost,
                cx,
            )
        } else {
            button(
                "redis-soft-wrap",
                tr(cx, "软换行"),
                ButtonVariant::Ghost,
                cx,
            )
        };
        let wrap_button = wrap_button.disabled(!wrap_fits).on_click({
            let view = view.clone();
            move |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.soft_wrap = !this.soft_wrap;
                    this.apply_soft_wrap(window, cx);
                    cx.notify();
                });
            }
        });

        // 格式标签：纯读数（zedis 把按钮 `disabled(true)`，这里直接画文字）。
        let format = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xxs)
            .child(
                Icon::new(DuckIcon::Code)
                    .size(extended.icon.sm)
                    .text_color(extended.colors.accent_text),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.accent_text)
                    .child(data_format_label(self, cx)),
            );

        // 视图切换：三档映射到解码档（Auto = 重新检测；与头部九档下拉同一真源）。
        // 勾选随**当前档位**走：Auto 勾在「检测结果」上（decode 等于 auto_decode
        // 的结论 = 用户没强制指定别的档），纯文本 / Hex 直接比档位。菜单闭包要求
        // `'static`，三个布尔在闭包外算好再捕获（DecodeKind / bool 都是 Copy）。
        let current = match self.decode {
            DecodeKind::Json => tr(cx, "Auto"),
            DecodeKind::Text => tr(cx, "纯文本"),
            DecodeKind::Hex => "Hex",
            _ => tr(cx, "Auto"),
        };
        let detected = self
            .detail
            .as_ref()
            .map(|detail| auto_decode(&detail.raw))
            .unwrap_or(DecodeKind::Text);
        let auto_checked = self.decode == detected;
        let plain_checked = self.decode == DecodeKind::Text;
        let hex_checked = self.decode == DecodeKind::Hex;
        let auto_label = SharedString::from(tr(cx, "Auto"));
        let plain_label = SharedString::from(tr(cx, "纯文本"));
        let hex_label = SharedString::from("Hex");
        let viewer = toolbar_text_dropdown_button(
            "redis-viewer-mode",
            SharedString::from(current),
            Icon::new(DuckIcon::Rows3).size(extended.icon.sm),
            Anchor::BottomLeft,
            move |menu, _, _| {
                let view_auto = view.clone();
                let menu = menu.item(
                    PopupMenuItem::new(auto_label.clone())
                        .checked(auto_checked)
                        .on_click(move |_, window, cx| {
                            let _ = view_auto.update(cx, |this, cx| {
                                let decode = this
                                    .detail
                                    .as_ref()
                                    .map(|detail| auto_decode(&detail.raw))
                                    .unwrap_or(DecodeKind::Text);
                                this.switch_decode(decode, window, cx);
                            });
                        }),
                );
                let view_plain = view.clone();
                let menu = menu.item(
                    PopupMenuItem::new(plain_label.clone())
                        .checked(plain_checked)
                        .on_click(move |_, window, cx| {
                            let _ = view_plain.update(cx, |this, cx| {
                                this.switch_decode(DecodeKind::Text, window, cx);
                            });
                        }),
                );
                let view_hex = view.clone();
                menu.item(
                    PopupMenuItem::new(hex_label.clone())
                        .checked(hex_checked)
                        .on_click(move |_, window, cx| {
                            let _ = view_hex.update(cx, |this, cx| {
                                this.switch_decode(DecodeKind::Hex, window, cx);
                            });
                        }),
                )
            },
            cx,
        );

        h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .child(wrap_button)
            .child(format)
            .child(viewer)
            .into_any_element()
    }

    /// 探测专用的后台派发：与 [`RedisPage::spawn_redis`] 同构，但**不动 `busy`**。
    ///
    /// ⚠️ 这是刻意的，不是抄漏了。`busy` 的语义是「有**用户在等**的操作在跑」——
    /// 它挡着「继续加载」按钮、也参与面板刷新按钮的灰显。探测是**后台读数**，
    /// 用户并没有在等它；把它算进 `busy` 会有两个后果：
    ///
    /// 1. 「连接成功后先 `refresh_keys` 再 `probe_status`」这种顺序里，探测会因为
    ///    扫描刚把 `busy` 立起来而**直接不跑**（一个静默失效）；
    /// 2. 探测先回来时把 `busy` 清成 `false`，而扫描还在飞——于是「继续加载」
    ///    提前可点，会拿同一个游标再发一次。
    ///
    /// 探测本身只是一个往返，且与其它调用在连接互斥锁上自然串行，不需要 `busy` 兜底。
    fn spawn_probe<F>(&mut self, cx: &mut Context<Self>, work: F)
    where
        F: FnOnce() -> (Option<Duration>, StatusOverview) + Send + 'static,
    {
        let task = cx.background_executor().spawn(async move { work() });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let (latency, overview) = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |_window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        // 失败的那一段保留旧值（见 `probe_status` 的口径）。
                        if latency.is_some() {
                            this.probe.latency = latency;
                        }
                        if overview.memory.is_some() {
                            this.probe.memory = overview.memory;
                        }
                        // 空表**也要覆盖**：`INFO keyspace` 全空 = 一台刚被清过的服务端，
                        // 保留旧表会让 DB 选择器继续标出已经不存在的库。
                        this.probe.keyspace = overview.keyspace;
                        cx.notify();
                    });
                });
            });
        })
        .detach();
    }

    /// 跑一次状态栏探测：计时 `PING` + `INFO memory` / `INFO keyspace`。
    ///
    /// **失败不清空旧值**：`PING` 出错或某个 `INFO` 段拿不到时，保留上一次的数字，
    /// 只把这一次没拿到的段留空。服务端抖一下不该让整条状态栏空掉。
    ///
    /// ⚠️ 不复用 `self.status`：那是左半段（「这一页做了什么」），这里是右半段
    /// （「服务端什么样」），两者各有各的写入方。
    pub(super) fn probe_status(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_probe(cx, move || {
            let Ok(mut conn) = lock(&conn) else {
                return (None, StatusOverview::default());
            };
            let latency = conn.ping_latency().ok();
            (latency, conn.status_overview())
        });
    }

    /// 切换逻辑库（`SELECT`）：先切连接，成功了才落盘档案 + 重扫 + 重新探测。
    ///
    /// ⚠️ **顺序不能反**：先写档案再 `SELECT`，一旦 `SELECT` 失败（库号超出服务端的
    /// `databases` 配置）界面会显示 db9、连接却还停在 db0，两边永久错位。所以档案
    /// 只在 `SELECT` 成功之后才改。
    ///
    /// 落盘的理由：状态栏的 DB 段与左栏连接行都读 `active_config().db`，不落盘就会
    /// 出现「切到 db2、退出重进又回到 db0」的错位。同一个数字只留一个真源。
    ///
    /// 不收 `Window`：成功与失败都在 [`RedisPage::spawn_redis`] 的回调里处理，
    /// 而回调自带窗口（与 `refresh_keys` 同一口径）。
    pub(super) fn switch_db(&mut self, db: u32, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        // 切到当前库是空操作——别为一个没变的值付一次重扫。
        if self.active_config().is_some_and(|cfg| cfg.db == db) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.select(db)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    if let Some(cfg) = this.connections.iter_mut().find(|cfg| cfg.id == id) {
                        cfg.db = db;
                    }
                    this.persist_connections(cx);
                    // 换库 = 换了一个键空间：探测结果（含各库键数）与键集全部作废。
                    // 刷新键集会顺带把 `status` 写成「正在扫描键空间…」。
                    this.probe = StatusProbe::default();
                    this.refresh_keys(cx);
                    this.probe_status(cx);
                }
                Err(error) => {
                    notify::error(
                        trf(cx, "切换数据库失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            },
        );
    }

    /// 状态栏：左半段「这一页做了什么」，右半段「服务端现在什么样」。
    ///
    /// 布局对齐 zedis（`status_bar.rs`）：`[状态消息] [DB 选择器] [≡ 工具菜单]
    /// [45 / 45 键] ‖ [绿点 已连接] [延迟] [内存] [类型构成] [版本]`；
    /// 断开 / 解锁写操作收进工具菜单，锁写时「解锁」按钮保留在右端
    /// （它是安全动作，值得一个常驻入口）。
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let mode = self.access_mode();
        let locked = self.gate().is_some_and(|gate| gate.is_locked());
        let connected = self.active.is_some();

        // 工具菜单的文案先算好（闭包要求 `'static`，不能借 `cx`）。
        let probe_label = SharedString::from(tr(cx, "重新探测"));
        let disconnect_label = SharedString::from(tr(cx, "断开"));
        let unlock_label = SharedString::from(trf(
            cx,
            "解锁写操作（{} 分钟）",
            &[&(WRITE_UNLOCK_SECS / 60).to_string()],
        ));
        let mode_hint =
            (mode != AccessMode::ReadWrite).then(|| SharedString::from(mode_label(mode, cx)));

        let mut bar = h_flex()
            .flex_none()
            .h(theme.density().status_bar)
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .bg(extended.colors.chrome)
            .border_t(extended.stroke.thin)
            .border_color(extended.colors.hairline);

        if connected {
            // 工具菜单：低频的服务端动作收进来（zedis 的 `≡` 同位）。
            bar = bar.child(toolbar_dropdown_button(
                "redis-status-tools",
                tr(cx, "服务端动作"),
                Icon::new(DuckIcon::Ellipsis)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
                Anchor::BottomLeft,
                {
                    let view = view.clone();
                    move |menu, _, _| {
                        let view_probe = view.clone();
                        let menu = menu.item(PopupMenuItem::new(probe_label.clone()).on_click(
                            move |_, _window, cx| {
                                let _ = view_probe.update(cx, |this, cx| this.probe_status(cx));
                            },
                        ));
                        let view_unlock = view.clone();
                        let menu = if locked {
                            menu.item(PopupMenuItem::new(unlock_label.clone()).on_click(
                                move |_, window, cx| {
                                    let _ = view_unlock
                                        .update(cx, |this, cx| this.unlock_writes(window, cx));
                                },
                            ))
                        } else {
                            menu
                        };
                        let view_disconnect = view.clone();
                        menu.item(PopupMenuItem::new(disconnect_label.clone()).on_click(
                            move |_, window, cx| {
                                let _ = view_disconnect
                                    .update(cx, |this, cx| this.disconnect_active(window, cx));
                            },
                        ))
                    }
                },
                cx,
            ));
            bar = bar.child(db_switcher(self, cx));
            // 键数：`已加载 / 全库`，读起来就是「扫到多少了」。
            bar = bar.child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(selectable_text(SharedString::from(trf(
                        cx,
                        "{} / {} 键",
                        &[&self.keys.len().to_string(), &self.dbsize.to_string()],
                    )))),
            );

            // ── 值编辑器三件套（zedis status_bar.rs 的编辑器区段）─────────
            // 只在数据视图且选中了**字符串值**时出现：软换行 / 格式标签 / 视图
            // 切换都是「这个值怎么显示」的控件，别的视图与类型没有它们的位置。
            if self.main_view == MainView::Data
                && self
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.key_type == KeyType::String)
            {
                bar = bar.child(self.render_editor_controls(cx));
            }

            // 右半段：弹性空档把健康与遥测推到右端（zedis 同构）。
            bar = bar.child(div().flex_1().min_w_0());
            bar = bar.child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xxs)
                    .child(
                        div()
                            .flex_none()
                            .size(px(7.))
                            .rounded_full()
                            .bg(extended.colors.status_completed),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(tr(cx, "已连接"))),
                    ),
            );
            bar = bar.children(mode_hint.map(|text| meta_text(text, cx)));
            if let Some(latency) = latency_value(self) {
                bar = bar.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .child(
                            Icon::new(DuckIcon::Gauge)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(selectable_text(SharedString::from(latency))),
                        ),
                );
            }
            if let Some(memory) = memory_value(self) {
                bar = bar.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        .child(
                            Icon::new(DuckIcon::HardDrive)
                                .size(extended.icon.sm)
                                .text_color(tokens.colors.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(tokens.typography.xs.size)
                                .text_color(tokens.colors.muted_foreground)
                                .child(selectable_text(SharedString::from(memory))),
                        ),
                );
            }
            if let Some(session) = self.active_session() {
                bar = bar.child(neutral_tag(SharedString::from(session.version.clone()), cx));
            }
        }

        if locked {
            // 解锁是安全动作：常驻按钮（不收进菜单），锁写期间一眼可见。
            bar = bar.child(
                button(
                    "redis-unlock-writes",
                    trf(
                        cx,
                        "解锁写操作（{} 分钟）",
                        &[&(WRITE_UNLOCK_SECS / 60).to_string()],
                    ),
                    ButtonVariant::Secondary,
                    cx,
                )
                .on_click({
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| this.unlock_writes(window, cx));
                    }
                }),
            );
        }

        bar.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 延迟文案的三档精度 + 超过一秒改报秒。
    #[test]
    fn latency_text_keeps_only_meaningful_precision() {
        assert_eq!(latency_text(Duration::from_micros(420)), "0.42 ms");
        assert_eq!(latency_text(Duration::from_micros(999)), "1.00 ms");
        assert_eq!(latency_text(Duration::from_micros(1_234)), "1.2 ms");
        assert_eq!(latency_text(Duration::from_micros(12_340)), "12 ms");
        assert_eq!(latency_text(Duration::from_millis(999)), "999 ms");
        // 超过一秒不报「1523 ms」：那要读者自己除。
        assert_eq!(latency_text(Duration::from_millis(1_523)), "1.52 s");
        // 0 也是合法读数（本机回环）。
        assert_eq!(latency_text(Duration::ZERO), "0.00 ms");
    }

    /// 内存文案：未设上限时只有一个数，设了才是 `x / y`。
    #[test]
    fn memory_text_shows_a_ceiling_only_when_there_is_one() {
        let used_only = MemoryOverview {
            used_human: Some("1.05M".to_owned()),
            used_bytes: Some(1_101_200),
            ..MemoryOverview::default()
        };
        assert_eq!(memory_text(&used_only), "1.05M");

        let capped = MemoryOverview {
            max_human: Some("512.00M".to_owned()),
            max_bytes: Some(536_870_912),
            ..used_only
        };
        assert_eq!(memory_text(&capped), "1.05M / 512.00M");

        // 一个字段都没有 ⇒ 空串，调用方据此不渲染这一段。
        assert_eq!(memory_text(&MemoryOverview::default()), "");
    }
}
