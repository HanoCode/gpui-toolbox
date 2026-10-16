//! 键空间浏览与键级操作：分页 / 过滤 / 命名空间树 / 选中 / 重命名 / TTL。

use super::*;

impl RedisPage {
    /// 从零重扫：取 `DBSIZE` 与第一页。
    ///
    /// `MATCH` 串由**查询模式**决定（`duck_redis::query::scan_plan`）：
    /// 子串档 `*kw*`、前缀档 `kw*`、精确档**根本不扫**（转 [`Self::open_exact_key`] 直取）。
    /// 关键字与模式任一变化都等于换了一个问题，所以两者都走这里重扫。
    ///
    /// 不需要 `Window`：提示与错误都在 [`Self::spawn_redis`] 的回调里发，而回调自带窗口。
    pub(super) fn refresh_keys(&mut self, cx: &mut Context<Self>) {
        let keyword = self.pattern.read(cx).value().trim().to_owned();
        let pattern = match scan_plan(&keyword, self.query_mode) {
            ScanPlan::Exact => {
                self.open_exact_key(keyword, cx);
                return;
            }
            // 空关键字：不带 `MATCH` 扫全库（`MATCH **` 结果一样，白让服务端比一次）。
            ScanPlan::Everything => None,
            ScanPlan::Pattern(pattern) => Some(pattern),
        };
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let type_filter = self.type_filter;
        let page_size = self.scan_page_size(cx);
        self.keys.clear();
        // 重扫 = 换了一批键：旧的元数据（类型 / TTL）一律作废
        // —— 留着会让新键套上同名旧键的类型徽标与 TTL。
        self.key_meta.clear();
        self.selected = None;
        self.detail = None;
        self.scan_cursor = 0;
        self.scan_done = false;
        self.limit_announced = false;
        self.rebuild_views();

        self.status = tr(cx, "正在扫描键空间…").into();
        self.spawn_redis(
            cx,
            move || -> Result<(u64, ScanPage), RedisError> {
                let mut conn = lock(&conn)?;
                let total = conn.dbsize()?;
                let page = conn.scan(0, pattern.as_deref(), page_size, type_filter)?;
                Ok((total, page))
            },
            move |this, result, window, cx| match result {
                Ok((total, page)) => {
                    this.dbsize = total;
                    this.apply_page(page, cx);
                    this.status = this.loaded_status(cx);
                    cx.notify();
                }
                Err(error) => {
                    this.scan_done = true;
                    notify::error(trf(cx, "扫描失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 精确档：**不扫描**，按键名直接取这一个键（对齐 zedis 的 `select_key` 路径）。
    ///
    /// 键不存在时不往列表里塞一行空的：那会让人分不清「这个键没有」和「值没读回来」。
    /// 没有「下一页」——精确匹配的结果只有一条。
    pub(super) fn open_exact_key(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.keys.clear();
        self.key_meta.clear();
        self.selected = None;
        self.detail = None;
        self.scan_cursor = 0;
        self.scan_done = true;
        self.limit_announced = false;
        self.rebuild_views();
        self.status = trf(cx, "正在查询键 {}…", &[&key]).into();

        let probe = key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<KeyType, RedisError> {
                let mut conn = lock(&conn)?;
                conn.key_type(&probe)
            },
            move |this, result, window, cx| match result {
                Ok(KeyType::Missing) => {
                    this.status = trf(cx, "键不存在：{}", &[&key]).into();
                    this.rebuild_views();
                    notify::warning(trf(cx, "键不存在：{}", &[&key]), window, cx);
                }
                Ok(_) => {
                    this.keys.push(key.clone());
                    this.rebuild_views();
                    this.status = trf(cx, "已定位到键 {}", &[&key]).into();
                    // 元数据（类型徽标 / TTL 芯片）顺带补上；再选中它，右栏直接出详情。
                    this.fetch_key_meta(vec![key], cx);
                    this.select_key(0, window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "读取键失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 状态栏的「已加载 / 总数」，被 TTL 档挡掉一部分时补一句「匹配几个」。
    ///
    /// 两个数都要给：只报可见数会让人以为扫描漏了键，只报加载数又看不出过滤生效了。
    pub(super) fn loaded_status(&self, cx: &App) -> SharedString {
        let mut text = trf(
            cx,
            "已加载 {} / {} 个键",
            &[&self.keys.len().to_string(), &self.dbsize.to_string()],
        );
        if self.ttl_filter != TtlFilter::All {
            text = format!(
                "{text} · {}",
                trf(cx, "匹配 {} 个", &[&self.visible_count().to_string()])
            );
        }
        text.into()
    }

    /// 通过当前过期时间档的**键数**。
    ///
    /// ⚠️ 不能用 `tree_rows.len()` 代替：树里还有**文件夹行**，那个数是「行数」不是「键数」，
    /// 挂在状态栏的「匹配 N 个」后面就成了谎话。两种形态共用这一个口径。
    pub(super) fn visible_count(&self) -> usize {
        if self.ttl_filter == TtlFilter::All {
            return self.keys.len();
        }
        self.keys
            .iter()
            .filter(|key| {
                self.ttl_filter
                    .matches(self.key_meta.get(key.as_str()).map(|meta| meta.ttl_secs))
            })
            .count()
    }

    /// 「继续加载」：游标续扫下一批。
    pub(super) fn load_more_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scan_done || self.busy {
            return;
        }
        if self.keys.len() >= KEY_LIST_MAX {
            if !self.limit_announced {
                self.limit_announced = true;
                notify::warning(
                    trf(
                        cx,
                        "已加载 {} 个键，暂停追加；请用过滤串收窄范围",
                        &[&KEY_LIST_MAX.to_string()],
                    ),
                    window,
                    cx,
                );
            }
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let cursor = self.scan_cursor;
        // 续扫必须问**同一个问题**：`MATCH` 串与首页那次一模一样
        // （所以这里与 `refresh_keys` 共用 `scan_plan`，不各拼各的）。
        let keyword = self.pattern.read(cx).value().trim().to_owned();
        let pattern = match scan_plan(&keyword, self.query_mode) {
            ScanPlan::Pattern(pattern) => Some(pattern),
            // 精确档没有下一页（`scan_done` 已置位，走不到这里）；空关键字则不带 MATCH。
            ScanPlan::Everything | ScanPlan::Exact => None,
        };
        let type_filter = self.type_filter;
        let page_size = self.scan_page_size(cx);

        self.spawn_redis(
            cx,
            move || -> Result<ScanPage, RedisError> {
                let mut conn = lock(&conn)?;
                conn.scan(cursor, pattern.as_deref(), page_size, type_filter)
            },
            move |this, result, window, cx| match result {
                Ok(page) => {
                    this.apply_page(page, cx);
                    this.status = this.loaded_status(cx);
                    cx.notify();
                }
                Err(error) => {
                    this.scan_done = true;
                    notify::error(trf(cx, "加载失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 把一页 `SCAN` 结果并入本地列表（去重 + 上限）。
    pub(super) fn apply_page(&mut self, page: ScanPage, cx: &mut Context<Self>) {
        self.scan_cursor = page.cursor;
        self.scan_done = page.is_finished();
        // 本页真正新进来的键：只有它们需要拉元数据（重复键不必再问服务端一次）。
        let mut fresh: Vec<String> = Vec::new();
        for key in page.keys {
            if self.keys.len() >= KEY_LIST_MAX {
                break;
            }
            // 服务端允许同一键在相邻批次重复出现，本地去重（列表要稳定）。
            if !self.keys.iter().any(|existing| existing == &key) {
                self.keys.push(key.clone());
                fresh.push(key);
            }
        }
        // 键集变了，树的展平行必须跟着重建（缓存不能过期）。
        self.rebuild_views();
        if !fresh.is_empty() {
            self.fetch_key_meta(fresh, cx);
        }
        cx.notify();
    }

    /// 批量补齐新键的元数据（类型 + TTL），回来后重建树。
    ///
    /// 一次 pipeline 覆盖 [`META_CHUNK_KEYS`] 个键（内核侧分块），
    /// 所以「一页 500 键」是 2 次往返而不是 500 次——这是列表能恒显类型徽标
    /// 与 TTL 芯片的前提（旧注释里「SCAN 不取每键类型」的取舍已被这条路径取代）。
    fn fetch_key_meta(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 落位要按键名对齐：入参在 work 闭包里被吃掉，回调用这一份。
        let requested = keys.clone();
        self.spawn_redis(
            cx,
            move || -> Result<Vec<KeyMeta>, RedisError> {
                let mut conn = lock(&conn)?;
                conn.key_meta(&keys)
            },
            move |this, result, _window, cx| match result {
                Ok(meta) => {
                    // 内核保证「长度与入参相同、顺序一致」，所以按位配对即可。
                    //
                    // 不等长 = 这一批的对应关系已经不可信（竞态下 `meta` 被裁剪过），
                    // 整批丢弃：元数据只是装饰，宁可这一页暂时没有徽标，
                    // 也不能把 A 键的类型 / TTL 贴到 B 键上。
                    if meta.len() != requested.len() {
                        return;
                    }
                    // 期间重扫过一次也不怕：`refresh_keys` 已把整表清空，
                    // 迟到的旧元数据落进来也只是无主条目（键树只查当前键）。
                    for (key, meta) in requested.into_iter().zip(meta) {
                        this.key_meta.insert(key, meta);
                    }
                    this.rebuild_views();
                    cx.notify();
                }
                Err(error) => {
                    // 元数据是**装饰**：拿不到不该打断浏览，只记状态栏、不弹错误。
                    this.status = trf(cx, "键元数据读取失败：{}", &[&error.to_string()]).into();
                    cx.notify();
                }
            },
        );
    }

    /// 重建两个**派生视图缓存**：平铺列表的可见行 + 树模式的展平行。
    ///
    /// 调用点：键集变化（`apply_page` / 清空）、元数据到齐、连接切换、切形态、
    /// 切展开状态、TTL 过滤或排序档变化、树参数（分隔符 / 深度 / 阈值）变化。
    /// **渲染路径绝不调它**——百万键下每帧重算一次会把滚动拖垮（设计文档 §4.5）。
    pub(super) fn rebuild_views(&mut self) {
        self.rebuild_flat();
        self.rebuild_tree();
    }

    /// 平铺列表的可见行：TTL 档过滤 + 排序档排序，结果存 `keys` 的**下标**。
    ///
    /// 存下标而不是键名副本：`selected` 继续指向 `keys`，排序只改变「选中项排在第几行」，
    /// 不改变「谁被选中」。
    fn rebuild_flat(&mut self) {
        self.flat_rows = flat_view(
            &self.keys,
            &self.key_meta,
            self.ttl_filter,
            self.tag_filter,
            self.sort,
            self.active_tags(),
        );
    }

    /// 重建树模式的展平行缓存。
    ///
    /// 过滤与排序都在**构树时**完成（`build_tree_sorted`），所以展平看到的
    /// 就是最终顺序：TTL 档挡掉的键根本不会进树。
    fn rebuild_tree(&mut self) {
        if !self.tree_mode {
            self.tree_rows.clear();
            return;
        }
        let (separator, depth, auto_expand) = match self.active_config() {
            Some(cfg) => (
                cfg.effective_separator().to_owned(),
                cfg.effective_tree_depth(),
                cfg.effective_auto_expand(),
            ),
            // 没有连接时用内核缺省：冒号分隔、8 层、缺省展开阈值
            // （此时也没有键，展平结果必为空；取值只为口径统一）。
            None => (
                ":".to_owned(),
                duck_redis::scan::MAX_TREE_DEPTH,
                duck_redis::scan::AUTO_EXPAND_DEFAULT,
            ),
        };
        let ttl_filter = self.ttl_filter;
        let tag_filter = self.tag_filter;
        let sort = self.sort;
        // 构树要按 TTL 排序与过滤，得能按键名查剩余秒数。闭包捕获的是
        // `&key_meta`（`Copy`），所以整棵树只借用一次缓存，不复制任何元数据。
        let meta = &self.key_meta;
        let tags = self.active_tags();
        let ttl_of = |key: &str| meta.get(key).map(|meta| meta.ttl_secs);
        let mut tree = build_tree_sorted(
            self.keys
                .iter()
                .map(String::as_str)
                // 两道本地筛（过期时间 / 标签）都在构树**之前**做：被挡掉的键
                // 根本不进树，于是「文件夹里剩几个键」这类计数天然是对的，
                // 不需要在渲染期再过滤一遍。
                .filter(|key| {
                    ttl_filter.matches(ttl_of(key))
                        && tag_filter.matches(tags.tag_of(key), tags.is_favorite(key))
                }),
            &separator,
            depth,
            sort,
            ttl_of,
        );
        // 文件夹行的聚合色：由后代键的标签投票。**在构树之后、展平之前**跑一次
        // （O(节点数)），展平只搬结果——渲染期算这个会把滚动拖垮。
        stamp_folder_tags(&mut tree, |key| tags.tag_of(key));
        let mut rows = Vec::new();
        flatten_tree(
            &tree,
            0,
            "",
            &TreeShape {
                separator: &separator,
                overrides: &self.tree_overrides,
                auto_expand,
                meta,
                tags,
            },
            &mut rows,
        );
        self.tree_rows = rows;
    }

    /// 切键列表形态（平铺 ↔ 命名空间树）。
    pub(super) fn toggle_tree_mode(&mut self, cx: &mut Context<Self>) {
        self.tree_mode = !self.tree_mode;
        // 从平铺切到树时，展平行还是空的——必须立刻建一次，否则界面一片空白。
        self.rebuild_views();
        cx.notify();
    }

    /// 展开 / 收起一个树节点。
    pub(super) fn toggle_tree_node(
        &mut self,
        path: String,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.tree_overrides.insert(path, expanded);
        self.rebuild_views();
        cx.notify();
    }

    /// 选中一个键并取它的详情与首屏值。
    pub(super) fn select_key(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.keys.get(index).cloned() else {
            return;
        };
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.selected = Some(index);
        self.detail = None;

        // 两个闭包都要键名：一个拿它去查，一个拿它装配详情。
        let detail_key = key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(KeyType, i64, KeyObjectInfo, RedisValue), RedisError> {
                let mut conn = lock(&conn)?;
                let key_type = conn.key_type(&key)?;
                let ttl = conn.ttl(&key)?;
                // 内部编码与空闲时长**只对这个选中的键**探一次（两条命令、一次往返）。
                // 它永不失败：服务端不支持 `OBJECT`（代理 / 托管云 / NOPERM）时
                // 两个字段都是 `None`，右栏那两行不渲染，不影响其余信息。
                let object = conn.object_info(&key);
                // 模块类型没有通用读法（要 `DUMP`）：不要白跑一次 `load_value` 换一个
                // 英文错误，直接回 `Missing` 占位，装配阶段会换成「没有查看器」的说明。
                if key_type == KeyType::Module {
                    return Ok((key_type, ttl, object, RedisValue::Missing));
                }
                let value = conn.load_value(&key, key_type, VALUE_PAGE)?;
                Ok((key_type, ttl, object, value))
            },
            move |this, result, window, cx| match result {
                Ok((key_type, ttl, object, value)) => {
                    // 解码档随详情**重置**（zedis 的 `ViewMode::Auto` 语义：每次
                    // 加载都回 Auto）：字符串值解析为 JSON 就用 JSON 档（美化 +
                    // tree-sitter 高亮），否则文本档。用户在解码下拉里选的档位
                    // 活到下一次加载为止。
                    let decode = match (&value, key_type) {
                        (RedisValue::String(bytes), KeyType::String) => auto_decode(bytes),
                        _ => this.decode,
                    };
                    this.decode = decode;
                    let detail = build_detail(detail_key, key_type, ttl, object, value, decode, cx);
                    let key_name = detail.key.clone();
                    this.detail = Some(detail);
                    // 内容编辑器回填新值 + 按解码档切高亮语言（JSON 档着色）。
                    this.sync_value_editor(window, cx);
                    // 树档的展开状态：换键按默认档重来，同一个键（写完刷新）保留。
                    this.adopt_json_tree(&key_name);
                    // 备注框跟着选中的键走：把本地标注里的那条填进去。
                    // 回填会触发 `Change`，靠订阅里那次「与已存值比对」把它认出来。
                    let note = this.active_tags().note_of(&key_name).to_owned();
                    this.note_input.update(cx, |input, cx| {
                        input.set_value(note, window, cx);
                    });
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "读取键失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 重新读取当前选中的键。
    ///
    /// ⚠️ 值面板顶栏的「刷新」按钮与写成功后的重读**走的是同一条路**
    /// （见 [`Self::reload_selected`]）：另写一条只取值的路径会让「刷新后看到的」
    /// 和「刚选中时看到的」有两个来源，迟早对不上。
    pub(super) fn refresh_selected_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_selected(window, cx);
    }

    /// 删除当前选中的键（受 `Capability::DeleteKey` 约束）。
    pub(super) fn delete_selected_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail_key) = self.detail.as_ref().map(|detail| detail.key.clone()) else {
            return;
        };
        self.confirm_or_run(
            Capability::DeleteKey,
            PendingAction::DeleteKey { key: detail_key },
            window,
            cx,
        );
    }

    /// 真正执行删除键（闸门已过、确认已给）。
    pub(super) fn delete_key_at(
        &mut self,
        detail_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 第二道保险：直接调用本方法时也必须过闸门。
        if let Err(reason) = self.check(Capability::DeleteKey) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 回收站按副本所在的库记录条目：删除前先把它固定下来，别让回调阶段再读一次。
        let db = self.active_config().map_or(0, |cfg| cfg.db);
        let key = detail_key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(u64, Option<KeyStash>), RedisError> {
                let mut conn = lock(&conn)?;
                // **先抄一份，再删**。抄不下来只是「这一份不入站」——回收站绝不能把
                // 一个本来能成功的删除变成错误（对齐 zedis `stash_key_to_trash`）。
                let stash = capture_stash(&mut conn, &key);
                let deleted = conn.del_key(&key)?;
                // 什么都没删掉（键早已不在）时不入站：那是空操作，不是一次删除。
                Ok((deleted, if deleted > 0 { stash } else { None }))
            },
            move |this, result, window, cx| match result {
                Ok((deleted, stash)) => {
                    if deleted > 0 {
                        this.keys.retain(|existing| existing != &detail_key);
                        // `DBSIZE` 也要跟着减：状态栏的「已加载 / 总数」前半段减了、
                        // 后半段不减，看起来就像「扫漏了一个键」。
                        this.dbsize = this.dbsize.saturating_sub(deleted);
                        this.detail = None;
                        this.selected = None;
                        // 先入回收站、再丢本地标注：两处都是本地数据，但入站是
                        // 「刚删了什么」的第一手事实，先落它。
                        let stashed = if let Some(stash) = stash {
                            this.stash_deleted_key(&detail_key, db, stash, cx);
                            true
                        } else {
                            false
                        };
                        // 键在服务端已经没了，本地那份标注（颜色 / 备注 / 收藏）
                        // 一起丢掉：留着只会让将来同名的另一个键继承一份来路不明的色。
                        this.forget_key_tags(&detail_key, cx);
                        this.rebuild_views();
                        // 没进回收站时在**状态栏**留一句实话（不另外弹窗——常见的
                        // 小键走的是入站那条路，提示只该出现在真的恢复不了的时候）。
                        this.status = if stashed {
                            trf(cx, "已删除键 {}（可在回收站恢复）", &[&detail_key]).into()
                        } else {
                            trf(cx, "已删除键 {}（未进回收站）", &[&detail_key]).into()
                        };
                        notify::success(tr(cx, "键已删除"), window, cx);
                        // 键数与内存都变了，顺手把状态栏的读数刷新一次。
                        this.probe_status(cx);
                    } else {
                        notify::warning(tr(cx, "键已不存在"), window, cx);
                    }
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "删除失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // M1.3：破坏性动作的二次确认。
    //
    // 确认本身是**连接档案上的开关**（`require_confirm_writes`），不是我们强加的
    // 门槛：关掉时直接执行——否则「高级用户」每删一个哈希字段都要挨一次弹窗。
    // ------------------------------------------------------------------

    /// 闸门 →（开关打开时）确认框 → 真正执行。**所有破坏性动作的唯一入口。**
    ///
    /// 动作参数（尤其 `RenameKey` 的目标键名）在构造 `PendingAction` 那一刻就冻结，
    /// 确认期间**不回读输入框**：否则用户改完输入框再点确认，动的不是他看到的那个。
    pub(super) fn confirm_or_run(
        &mut self,
        capability: Capability,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = self.check(capability) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let requires_confirm = self
            .active_config()
            .is_some_and(|cfg| cfg.require_confirm_writes);
        if !requires_confirm {
            self.run_pending(action, window, cx);
            return;
        }

        let view = self.view.clone();
        let title = action.title(cx);
        let message = action.message(cx);
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let offset = modal_offset(&drag, cx);
            // 外层闭包是 `Fn`（每次重排都重跑）→ 每一轮都得重新克隆一份交给内层。
            let action_for_ok = action.clone();
            let view_for_ok = view.clone();
            let drag_for_content = drag.clone();
            let message_for_content = message.clone();

            dialog
                .w(px(CONFIRM_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(tr(cx, "确认执行"))
                        .cancel_text(tr(cx, "取消"))
                        .show_cancel(true),
                )
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .child(modal_title_row(
                                "redis-confirm-title",
                                &drag_for_content,
                                title,
                                cx,
                            ))
                            .child(body_text(message_for_content.clone(), cx)),
                    ))
                })
                .on_ok(move |_, window, cx| {
                    // 确认后才真正动手；视图已销毁（`update` 返回 `Err`）也照样关弹窗，
                    // 不让一个点不掉的确认框挂在屏幕上。
                    let _ = view_for_ok.update(cx, |this, cx| {
                        this.run_pending(action_for_ok.clone(), window, cx);
                    });
                    true
                })
        });
    }

    /// 把 `PendingAction` 分派到对应的**真正执行**方法（`*_at` 族）。
    ///
    /// 每个 `*_at` 内部仍会再过一次闸门——`confirm_or_run` 之外的调用点（测试、
    /// 未来新增入口）也不会绕过锁写约束。
    pub(super) fn run_pending(
        &mut self,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PendingAction::DeleteKey { key } => self.delete_key_at(key, window, cx),
            PendingAction::RenameKey { from, to } => {
                self.rename_key_from_to(from, to, window, cx);
            }
            PendingAction::ClearTtl { key } => self.clear_ttl_at(key, window, cx),
            PendingAction::DeleteHashField { key, field } => {
                self.delete_hash_field_at(key, field, window, cx);
            }
            PendingAction::DeleteListItem { key, index } => {
                self.delete_list_item_at(key, index, window, cx);
            }
            PendingAction::DeleteSetMember { key, member } => {
                self.delete_set_member_at(key, member, window, cx);
            }
            PendingAction::DeleteZSetMember { key, member } => {
                self.delete_zset_member_at(key, member, window, cx);
            }
            PendingAction::RawCommand { line } => self.run_cli_line_at(line, window, cx),
            PendingAction::ConfigSet { name, value } => {
                self.set_config_at(name, value, window, cx);
            }
            PendingAction::SlowlogReset => self.reset_slowlog_at(window, cx),
            PendingAction::ClientKill { addr } => self.kill_client_at(addr, window, cx),
        }
    }

    /// 写操作放行检查；被拒时提示原因并返回 `false`（调用方直接 `return`）。
    pub(super) fn allow_write(
        &self,
        capability: Capability,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.check(capability) {
            Ok(()) => true,
            Err(reason) => {
                notify::warning(deny_message(reason, cx), window, cx);
                false
            }
        }
    }

    /// 取当前选中键的 `(键名, 类型)`；没选中返回 `None`。
    pub(super) fn current_key(&self) -> Option<(String, KeyType)> {
        self.detail
            .as_ref()
            .map(|detail| (detail.key.clone(), detail.key_type))
    }

    /// 写成功后重读当前键。
    ///
    /// 集合类值改一笔就重新拉一页：本地拼装容易与服务端实际内容（顺序、
    /// 去重、分数）不一致，宁可多一次往返也不显示一个「看起来对」的假状态。
    pub(super) fn reload_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.select_key(index, window, cx);
        }
    }

    /// 设置键的过期时间（秒）。
    pub(super) fn set_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        let text = self.ttl_input.read(cx).value().to_string();
        let Ok(seconds) = text.trim().parse::<i64>() else {
            notify::warning(tr(cx, "请填写整数秒数"), window, cx);
            return;
        };
        if seconds <= 0 {
            // 0 在服务端语义里是「立即删除」，比「设成永不过期」危险得多：
            // 想永不过期该用「移除过期时间」，别让用户在 TTL 框里打出 0 就把键删了。
            notify::warning(
                tr(cx, "秒数必须大于 0；要永不过期请用「移除过期时间」"),
                window,
                cx,
            );
            return;
        }
        if !self.allow_write(Capability::SetTtl, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.expire(&key, seconds)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    if let Some(detail) = &mut this.detail {
                        detail.ttl = seconds;
                    }
                    this.status = trf(cx, "已为 {} 设置过期时间", &[&seconds.to_string()]).into();
                    notify::success(tr(cx, "过期时间已设置"), window, cx);
                    cx.notify();
                }
                Ok(false) => notify::warning(tr(cx, "键已不存在"), window, cx),
                Err(error) => {
                    notify::error(trf(cx, "设置失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 移除键的过期时间（改为永不过期）。
    pub(super) fn clear_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        self.confirm_or_run(
            Capability::SetTtl,
            PendingAction::ClearTtl { key },
            window,
            cx,
        );
    }

    /// 真正执行移除过期时间。
    pub(super) fn clear_ttl_at(
        &mut self,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.allow_write(Capability::SetTtl, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_redis(
            cx,
            move || -> Result<bool, RedisError> {
                let mut conn = lock(&conn)?;
                conn.persist(&key)
            },
            move |this, result, window, cx| match result {
                Ok(true) => {
                    if let Some(detail) = &mut this.detail {
                        detail.ttl = -1;
                    }
                    notify::success(tr(cx, "已改为永不过期"), window, cx);
                    cx.notify();
                }
                // 本来就没有过期时间：不是错误，但要说清楚「什么都没变」。
                Ok(false) => notify::warning(tr(cx, "该键本来就没有过期时间"), window, cx),
                Err(error) => {
                    notify::error(trf(cx, "设置失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 重命名当前键。
    ///
    /// ⚠️ `RENAME` 会**静默覆盖**同名目标键。界面上的说法只能是「已重命名」，
    /// 无法承诺「没覆盖任何东西」——所以文案里带上目标键名，让用户自己确认。
    pub(super) fn rename_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, _)) = self.current_key() else {
            return;
        };
        let new_key = self.rename_input.read(cx).value().to_string();
        let new_key = new_key.trim().to_owned();
        if new_key.is_empty() {
            notify::warning(tr(cx, "新的键名不能为空"), window, cx);
            return;
        }
        if new_key == key {
            notify::warning(tr(cx, "新键名与原名相同"), window, cx);
            return;
        }
        self.confirm_or_run(
            Capability::RenameKey,
            PendingAction::RenameKey {
                from: key,
                to: new_key,
            },
            window,
            cx,
        );
    }

    /// 真正执行重命名。
    ///
    /// ⚠️ 目标键名在**点按钮那一刻**就固定下来了（`PendingAction::RenameKey`），
    /// 确认期间不再回读输入框——否则用户改完输入框再点确认，动的不是他看到的那个。
    pub(super) fn rename_key_from_to(
        &mut self,
        old_key: String,
        new_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = old_key.clone();
        if !self.allow_write(Capability::RenameKey, window, cx) {
            return;
        }
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let renamed = new_key.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                conn.rename(&old_key, &renamed)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    // 本地同步：键列表里的旧名换成新名；详情与选中项跟着走。
                    if let Some(slot) = this.keys.iter_mut().find(|existing| **existing == key) {
                        *slot = new_key.clone();
                    }
                    if let Some(detail) = &mut this.detail {
                        detail.key = new_key.clone();
                    }
                    this.status = trf(cx, "已重命名为 {}", &[&new_key]).into();
                    // 键名变了，命名空间树的分组也跟着变（`user:1` → `app:1` 会换层）。
                    this.rebuild_views();
                    notify::success(tr(cx, "键已重命名"), window, cx);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "重命名失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 切字符串值的解码档位。
    ///
    /// 只用已缓存的原始字节重算，**不重发 `GET`**——切档是纯展示行为，
    /// 让它产生网络流量既慢又可能与服务端内容产生时间差。
    pub(super) fn switch_decode(
        &mut self,
        mode: DecodeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.decode == mode {
            return;
        }
        self.decode = mode;
        // 解码档换了，展示的文档就是另一份——JSONPath 的命中全部作废
        // （同 `adopt_json_tree` 的口径：文档变了宁可清空）。
        self.jsonpath_hits = Arc::new(Vec::new());
        self.jsonpath_error = None;
        self.jsonpath_selected = None;
        if let Some(detail) = &mut self.detail
            && detail.key_type == KeyType::String
        {
            detail.view = decode_view(&detail.raw, mode, cx);
            // 树档跟着档位走：非 JSON 档没有树，换键口径在此同样适用。
            detail.tree = match (&detail.view, mode) {
                (ValueView::Text { rendered, .. }, DecodeKind::Json) => tree_of(rendered),
                _ => None,
            };
        }
        // 内容编辑器同步回填 + 切高亮语言（JSON 档 → tree-sitter 着色）。
        self.sync_value_editor(window, cx);
        self.sync_json_rows();
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 顶栏的两个本地档位（TTL 过滤 / 排序）。
    //
    // 与查询模式不同，这两个**不发任何 Redis 往返**：过滤与排序都只读已经
    // 缓存下来的 `key_meta` 与 `keys`，所以切换是即时的、可以随便点。
    // ------------------------------------------------------------------

    /// 切 TTL 范围过滤档。
    pub(super) fn set_ttl_filter(&mut self, filter: TtlFilter, cx: &mut Context<Self>) {
        if self.ttl_filter == filter {
            return;
        }
        self.ttl_filter = filter;
        self.rebuild_views();
        self.status = self.loaded_status(cx);
        cx.notify();
    }

    /// 切排序档。
    pub(super) fn set_sort(&mut self, sort: KeySort, cx: &mut Context<Self>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.rebuild_views();
        cx.notify();
    }

    /// 切查询模式。**会重扫**——模式决定服务端的 `MATCH` 串，换了模式就是换了问题。
    pub(super) fn set_query_mode(&mut self, mode: QueryMode, cx: &mut Context<Self>) {
        if self.query_mode == mode {
            return;
        }
        self.query_mode = mode;
        self.refresh_keys(cx);
    }
}

/// 平铺列表的可见行：按 TTL 档 + 标签档过滤 + 按排序档排，返回 `keys` 的下标序列。
///
/// 纯函数：不发网络、不改状态，所以可以直接单测「哪个键被挡掉、谁排在谁前面」。
///
/// 两个本地筛（`ttl_filter` / `tag_filter`）都在**收集下标时**一次判掉，
/// 所以返回的下标序列既是「要显示的行」也是「行序」——渲染层不必再过滤一次。
pub(super) fn flat_view(
    keys: &[String],
    meta: &HashMap<String, KeyMeta>,
    ttl_filter: TtlFilter,
    tag_filter: TagFilter,
    sort: KeySort,
    tags: &KeyTags,
) -> Vec<usize> {
    let ttl_of = |key: &str| meta.get(key).map(|meta| meta.ttl_secs);
    let mut rows: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, key)| {
            ttl_filter.matches(ttl_of(key))
                && tag_filter.matches(tags.tag_of(key), tags.is_favorite(key))
        })
        .map(|(index, _)| index)
        .collect();
    rows.sort_by(|a, b| {
        // `keys` 一定含这两个下标（来自 `enumerate`），`unwrap_or_default` 只是
        // 让「理论上不可能」的分支有个确定行为，而不是 panic 掉整个界面。
        let a_key = keys.get(*a).map_or("", String::as_str);
        let b_key = keys.get(*b).map_or("", String::as_str);
        sort.compare(
            Sortable {
                label: a_key,
                is_folder: false,
                ttl_secs: ttl_of(a_key),
            },
            Sortable {
                label: b_key,
                is_folder: false,
                ttl_secs: ttl_of(b_key),
            },
        )
    });
    rows
}
