//! 五类结构化值的写入与删除（字符串 / 哈希字段 / 列表元素 / 集合成员 / 有序集合成员）。

use super::*;

impl RedisPage {
    /// 保存字符串值编辑（受 `Capability::WriteKey` 约束）。
    pub(super) fn save_string_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else {
            return;
        };
        if !matches!(detail.view, ValueView::Text { .. }) {
            notify::warning(tr(cx, "只有字符串值可直接编辑"), window, cx);
            return;
        }
        if let Err(reason) = self.check(Capability::WriteKey) {
            notify::warning(deny_message(reason, cx), window, cx);
            return;
        }
        let key = detail.key.clone();
        let value = self.edit_buffer.read(cx).value().to_string();
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 后台闭包吃掉 `key` / `value`，回主线程那一步要用副本（更新右栏与状态栏）。
        let key_echo = key.clone();
        let value_echo = value.clone();
        self.spawn_redis(
            cx,
            move || -> Result<(), RedisError> {
                let mut conn = lock(&conn)?;
                // KEEPTTL：只改值，别把键上原有的过期时间顺手抹掉。
                conn.set_string(&key, value.as_bytes(), true)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    let len = value_echo.len();
                    if let Some(detail) = &mut this.detail {
                        detail.view = ValueView::Text {
                            rendered: value_echo,
                            notice: None,
                            bytes: len,
                        };
                        detail.size = len;
                    }
                    this.status = trf(cx, "已保存键 {}", &[&key_echo]).into();
                    notify::success(tr(cx, "值已保存"), window, cx);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "保存失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    // ------------------------------------------------------------------
    // 结构化的值级编辑（M1.2）：哈希字段 / 列表元素 / 集合成员 / 有序集合成员。
    //
    // 全部走内核的**参数化**写方法（`hset` / `lset` / `sadd` / `zadd` …），
    // 不拼命令行——键名与元素里带空格或非 UTF-8 字节时，命令行切词会拆坏参数。
    // ------------------------------------------------------------------

    /// 集合类写操作的统一外壳：闸门 → 后台执行 → 成功后重读当前键。
    ///
    /// `work` 拿到连接与键名（旧键名），返回 `Ok(())` 即视为成功。
    /// 写失败一律 `notify::error`，不静默吞掉——用户按下按钮就必须有反馈。
    pub(super) fn spawn_write<W>(
        &mut self,
        capability: Capability,
        key: String,
        success: &'static str,
        work: W,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        W: FnOnce(&mut RedisConnection, &str) -> Result<(), RedisError> + Send + 'static,
    {
        if !self.allow_write(capability, window, cx) {
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
                work(&mut conn, &key)
            },
            move |this, result, window, cx| match result {
                Ok(()) => {
                    notify::success(success, window, cx);
                    this.reload_selected(window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "写入失败：{}", &[&error.to_string()]), window, cx);
                }
            },
        );
    }

    /// 写一个哈希字段（新增或覆盖）。
    pub(super) fn save_hash_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Hash {
            return;
        }
        let field = self.hash_field.read(cx).value().to_string();
        let value = self.hash_value.read(cx).value().to_string();
        if field.is_empty() {
            notify::warning(tr(cx, "字段名不能为空"), window, cx);
            return;
        }
        let field_bytes = field.into_bytes();
        let value_bytes = value.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "字段已写入"),
            move |conn, key| conn.hset(key, &field_bytes, &value_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 删一个哈希字段。
    pub(super) fn delete_hash_field(
        &mut self,
        field: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Hash {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteHashField { key, field },
            window,
            cx,
        );
    }

    /// 真正执行删除哈希字段。
    pub(super) fn delete_hash_field_at(
        &mut self,
        key: String,
        field: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field_bytes = field.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "字段已删除"),
            move |conn, key| conn.hdel(key, &field_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 写一个有序集合成员（新增 / 改分数 / 改名）。
    ///
    /// `previous` 非空表示改的是既有成员（改名时内核会先加后删，见 `zset_put`）。
    pub(super) fn save_zset_member(
        &mut self,
        previous: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::ZSet {
            return;
        }
        let member = self.zset_member.read(cx).value().to_string();
        let score_text = self.zset_score.read(cx).value().to_string();
        if member.is_empty() {
            notify::warning(tr(cx, "成员不能为空"), window, cx);
            return;
        }
        let Ok(score) = score_text.trim().parse::<f64>() else {
            notify::warning(tr(cx, "分数必须是数字"), window, cx);
            return;
        };
        let member_bytes = member.into_bytes();
        let previous_bytes = previous.map(String::into_bytes);
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已写入"),
            move |conn, key| {
                conn.zset_put(key, score, &member_bytes, previous_bytes.as_deref())
                    .map(|_| ())
            },
            window,
            cx,
        );
    }

    /// 删一个有序集合成员。
    pub(super) fn delete_zset_member(
        &mut self,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::ZSet {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteZSetMember { key, member },
            window,
            cx,
        );
    }

    /// 真正执行删除有序集合成员。
    pub(super) fn delete_zset_member_at(
        &mut self,
        key: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已删除"),
            move |conn, key| conn.zset_remove(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 加一个集合成员。
    pub(super) fn add_set_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Set {
            return;
        }
        let member = self.set_member.read(cx).value().to_string();
        if member.is_empty() {
            notify::warning(tr(cx, "成员不能为空"), window, cx);
            return;
        }
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已加入"),
            move |conn, key| conn.set_add(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 删一个集合成员。
    pub(super) fn delete_set_member(
        &mut self,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::Set {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteSetMember { key, member },
            window,
            cx,
        );
    }

    /// 真正执行删除集合成员。
    pub(super) fn delete_set_member_at(
        &mut self,
        key: String,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let member_bytes = member.into_bytes();
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "成员已删除"),
            move |conn, key| conn.set_remove(key, &member_bytes).map(|_| ()),
            window,
            cx,
        );
    }

    /// 列表写入：底部表单处于「改某一行」态就 `LSET`，否则按 `front` 推入。
    pub(super) fn save_list_value(
        &mut self,
        front: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::List {
            return;
        }
        let value = self.list_value.read(cx).value().to_string();
        let value_bytes = value.into_bytes();
        let editing = self.list_editing;
        let (capability, success) = if editing.is_some() {
            (Capability::WriteKey, tr(cx, "元素已更新"))
        } else {
            (Capability::WriteKey, tr(cx, "元素已推入"))
        };
        self.spawn_write(
            capability,
            key,
            success,
            move |conn, key| match editing {
                // 注意 `list_editing` 存的是**列表下标**（0 起，与界面显示的序号一致）。
                Some(index) => conn.list_set(key, index, &value_bytes),
                None => conn.list_push(key, &value_bytes, front).map(|_| ()),
            },
            window,
            cx,
        );
    }

    /// 把某一行填进底部表单，进入「按位置改」态。
    pub(super) fn edit_list_item(
        &mut self,
        index: i64,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_editing = Some(index);
        self.list_value.update(cx, |input, cx| {
            input.set_value(value, window, cx);
        });
        cx.notify();
    }

    /// 退出「按位置改」态，回到追加态。
    pub(super) fn cancel_list_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_editing = None;
        self.list_value.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        cx.notify();
    }

    /// 删掉列表里指定位置的元素。
    pub(super) fn delete_list_item(
        &mut self,
        index: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, key_type)) = self.current_key() else {
            return;
        };
        if key_type != KeyType::List {
            return;
        }
        self.confirm_or_run(
            Capability::WriteKey,
            PendingAction::DeleteListItem { key, index },
            window,
            cx,
        );
    }

    /// 真正执行删除列表元素。
    pub(super) fn delete_list_item_at(
        &mut self,
        key: String,
        index: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 若正在改这一行，先退出编辑态——否则下标会在删除后错位。
        if self.list_editing == Some(index) {
            self.list_editing = None;
        }
        self.spawn_write(
            Capability::WriteKey,
            key,
            tr(cx, "元素已删除"),
            move |conn, key| conn.list_remove_at(key, index).map(|_| ()),
            window,
            cx,
        );
    }
}
