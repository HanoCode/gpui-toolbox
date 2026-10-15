//! 本地**回收站**：软删除的键在本地留 24 小时，可恢复。
//!
//! 数据只住在本地（`duck-app` 把它序列化进 `duck-store` 的 kv 单键），**服务端
//! 一个字节都不占**——删键那条路照旧只发 `DEL`，额外的 `MEMORY USAGE` / `DUMP` /
//! `PTTL` 都在删除**之前**发一次，用于把「即将被删的那一份」抄下来。
//! 与 [`crate::tags`] 同属「纯数据 + 纯函数、零 gpui、可单测」这一类。
//!
//! # 对齐 zedis（`temp/zedis/crates/zedis-db/src/trash.rs`）
//!
//! - **24 小时保留**恒为 [`TRASH_RETENTION_MS`]，不可配：回收站是防手滑的安全网，
//!   不是备份系统。要长期留就该用导出，而不是把回收站改成一个没人清的空档。
//! - **两道体积闸**。① 删除前先 `MEMORY USAGE`（[`TRASH_MAX_VALUE_MEMORY`]，4 MiB），
//!   超了就**连 `DUMP` 都不发**——一个多兆的大键多半是「有意清理」，没必要为了
//!   一个马上要被丢掉的副本先把整份值序列化一遍再从服务端搬过来；② `DUMP` 回来后
//!   再卡一次精确长度（[`TRASH_MAX_PAYLOAD`]，1 MiB），因为内存占用普遍高估
//!   序列化后的体积，中等的值会落在两道闸之间、由这第二道决定。
//! - **载荷是 `DUMP` 的原样字节**，恢复走 `RESTORE` 并把删除时的 `PTTL` 带回去
//!   （见 [`TrashEntry::pttl_ms`]）。这也是为什么载荷必须以**二进制**存
//!   （[`payload_b64`] 只是落盘时的编码，不是语义）。
//! - **恢复不带 `REPLACE`**：同名键在这期间被重新建出来时，`RESTORE` 会以
//!   `BUSYKEY` 失败，界面把它算作「跳过」而不是「失败」——一个从回收站恢复的动作
//!   不该顺手覆盖别人刚写进去的数据。
//!
//! # 与本仓 kv 底座的三处偏差（都是刻意的）
//!
//! zedis 用 redb，一张 `trash` 表、一行一条、`id` 前缀是零填充时间戳（于是范围扫描
//! 天然按删除序、过期判定退化成字符串比较）。本仓的 kv 是 SQLite 的
//! `kv(key TEXT, value TEXT)`，没有二进制列，所以：
//!
//! 1. **载荷 base64 进 JSON**（[`payload_b64`]），整本回收站落**一个 kv 键**。
//!    代价是 33% 的体积膨胀与「一次删键重写整份文档」；这两条都由下面两个预算
//!    压住，而不是靠「一般不会删很多键」这种侥幸。
//! 2. **另加两条预算**：[`TRASH_MAX_ENTRIES`] 与 [`TRASH_MAX_TOTAL_BYTES`]，
//!    超出时按**删除时间从旧到新**驱逐。zedis 没有这两条（redb 表没有
//!    「一次重写多少钱」的问题），但我们的底座有，所以必须显式封顶。
//! 3. **过期判定用毫秒比较**，不是 id 前缀的字符串比较——因为 id 不再是存储主键，
//!    它只是给界面回传的一个稳定句柄（[`TrashEntry::id`]）。
//!
//! # 一条边界
//!
//! 回收站**按连接隔离**（[`TrashStore::servers`] 以连接 id 为键）：两台服务端上
//! 都删过 `user:1` 时，恢复其中一台的那一条绝不该把另一台也写了。与
//! [`crate::tags`] 同款理由。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一条软删除记录的保留时长（毫秒）。**不可配**，理由见模块文档。
pub const TRASH_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

/// 单条记录 `DUMP` 载荷的上限：超过就**永久删除**、不入站。
///
/// 取值与 zedis 一致。刻意小：回收站挡的是「手滑删掉一个会话 / 配置 / JSON 文档」，
/// 这类键几乎都在 1 MB 以下；而删一个多兆的大键多半是有意清理，不该在本地留副本。
pub const TRASH_MAX_PAYLOAD: usize = 1024 * 1024;

/// `DUMP` **之前**的体积闸（`MEMORY USAGE` 的估算值，字节）。
///
/// 是 [`TRASH_MAX_PAYLOAD`] 的 4 倍：内存占用普遍高估（序列化 + 压缩后的）`DUMP`
/// 体积好几倍，把前置闸卡在同一水平会误伤一批本来装得下的中等值。中等值落到
/// `DUMP` 之后再按 [`TRASH_MAX_PAYLOAD`] 精确判定。
pub const TRASH_MAX_VALUE_MEMORY: u64 = 4 * 1024 * 1024;

/// 单台连接在回收站里的条目数上限（见模块文档「三处偏差」）。
pub const TRASH_MAX_ENTRIES: usize = 200;

/// 单台连接在回收站里的载荷总字节上限（见模块文档「三处偏差」）。
///
/// 16 MiB：落盘时 base64 会膨胀到约 21 MiB，这是「一个 kv 键」还能接受的上限。
pub const TRASH_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// 落盘信封的当前版本。与 [`crate::tags::TAG_STORE_VERSION`] 同一条口径：
/// 读盘时版本对不上就**回落空表**，不猜、不硬解。
pub const TRASH_STORE_VERSION: u32 = 1;

/// 当前 Unix 毫秒。回收站的时间戳（删除时刻）与过期判定都用它。
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| delta.as_millis() as i64)
}

/// `MEMORY USAGE` 的估算值是否允许继续 `DUMP`（第一道体积闸）。
///
/// `None`（`MEMORY USAGE` 不可用：代理 / 托管云 / 无权限 / 键刚消失）**放行**——
/// 探测失败不该把回收站整个关掉，届时的兜底是 `DUMP` 之后的
/// [`payload_fits`]。这与 `conn::object_info` 那类「拿不到就不显示」的取舍方向
/// 一致：这里是「拿不到就不拦」。
#[must_use]
pub fn should_stash(memory_usage: Option<u64>) -> bool {
    memory_usage.is_none_or(|bytes| bytes <= TRASH_MAX_VALUE_MEMORY)
}

/// `DUMP` 回来的载荷是否装得下（第二道体积闸，精确长度）。
#[must_use]
pub fn payload_fits(len: usize) -> bool {
    len <= TRASH_MAX_PAYLOAD
}

/// 载荷的落盘编码：base64 字符串（见模块文档「三处偏差」第 1 条）。
///
/// 只影响**落盘**，内存里始终是 `Vec<u8>`——`DUMP` 的输出是二进制，
/// 中间插一层 `String` 只会让调用点到处做无谓的编解码。
mod payload_b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// 一条软删除的键：`DUMP` 载荷 + 恢复它需要的一切。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashEntry {
    /// 原来那个键名。
    pub key: String,
    /// 删除时所在的逻辑库（恢复时要 `SELECT` 回去，故必须存）。
    pub db: u32,
    /// 删除时剩下的 TTL（毫秒）；`<= 0` 表示删除时就没有过期时间。
    ///
    /// 恢复时原样交给 `RESTORE`：`0` 在协议里就是「不过期」，与「没有 TTL」同义，
    /// 所以负值先归一到 `0` 再下发。
    pub pttl_ms: i64,
    /// 删除时刻（Unix 毫秒）。同时决定列表顺序与过期判定。
    pub deleted_at_ms: i64,
    /// `DUMP` 的原样字节。
    #[serde(with = "payload_b64")]
    pub payload: Vec<u8>,
}

impl TrashEntry {
    /// 稳定句柄：界面把列表行回传时用它定位（`{毫秒:020}:{库}:{键}`）。
    ///
    /// 时间戳零填充到 20 位，于是**按 id 字符串排序 = 按删除时间排序**，与 zedis
    /// 存进 redb 的那个复合键同构（只是我们这里不做范围扫描，见模块文档）。
    #[must_use]
    pub fn id(&self) -> String {
        format!("{:020}:{}:{}", self.deleted_at_ms, self.db, self.key)
    }

    /// 载荷字节数（预算判定用）。
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.payload.len()
    }

    /// 恢复时给 `RESTORE` 的 TTL（毫秒）：负值归一成 `0`（= 不过期）。
    #[must_use]
    pub fn restore_pttl_ms(&self) -> i64 {
        self.pttl_ms.max(0)
    }
}

/// 列表行：除载荷以外的全部字段（列表**不碰**载荷，见 [`TrashStore::metas`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashMeta {
    /// 回传给恢复 / 移除的句柄（[`TrashEntry::id`]）。
    pub id: String,
    pub key: String,
    pub db: u32,
    pub pttl_ms: i64,
    pub deleted_at_ms: i64,
    /// 存下来的载荷字节数（列表要显示「这一条占多大」，又不该为它复制载荷）。
    pub bytes: usize,
}

impl TrashMeta {
    /// 从一条完整记录投影出列表行（丢掉载荷、只留它的长度）。
    #[must_use]
    pub fn of(entry: &TrashEntry) -> Self {
        Self {
            id: entry.id(),
            key: entry.key.clone(),
            db: entry.db,
            pttl_ms: entry.pttl_ms,
            deleted_at_ms: entry.deleted_at_ms,
            bytes: entry.byte_len(),
        }
    }
}

/// 全部连接的回收站：`连接 id → 条目（按删除时间升序）`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashStore {
    #[serde(default)]
    version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    servers: BTreeMap<String, Vec<TrashEntry>>,
}

impl Default for TrashStore {
    fn default() -> Self {
        Self {
            version: TRASH_STORE_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

impl TrashStore {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 JSON 读。整串解析失败、或版本对不上 ⇒ **回落空表**
    /// （见 [`TRASH_STORE_VERSION`]）。载荷的 base64 解不开也算解析失败 ⇒ 同样回落
    /// ——宁可这次读不出回收站，也不拿半份坏数据去覆盖它。
    #[must_use]
    pub fn from_json(json: &str) -> Self {
        let Ok(store) = serde_json::from_str::<Self>(json) else {
            return Self::default();
        };
        if store.version == TRASH_STORE_VERSION {
            store
        } else {
            Self::default()
        }
    }

    /// 序列化成落盘 JSON。
    ///
    /// # Errors
    ///
    /// 结构里只有 `BTreeMap` / `Vec` / 标量，序列化不会失败；`Result` 是为了与
    /// `serde_json::to_string` 的其他调用点写法一致。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 一台连接的条目（**删除时间升序**，即存储顺序）。
    #[must_use]
    pub fn entries(&self, server: &str) -> &[TrashEntry] {
        self.servers.get(server).map_or(&[], Vec::as_slice)
    }

    /// 一台连接的列表行（**最新在前**，且**不复制载荷**）。
    ///
    /// 倒序在这里做、不在存储层做：存储按删除时间升序，过期清理与预算驱逐都
    /// 从**头部**（最旧）取，两边各自拿到自己顺手的顺序，谁也不必将就谁。
    #[must_use]
    pub fn metas(&self, server: &str) -> Vec<TrashMeta> {
        self.entries(server)
            .iter()
            .rev()
            .map(TrashMeta::of)
            .collect()
    }

    /// 一台连接的条目数。
    #[must_use]
    pub fn len(&self, server: &str) -> usize {
        self.entries(server).len()
    }

    /// 有没有任何连接的回收站非空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.values().all(Vec::is_empty)
    }

    /// 所有连接的载荷总字节数（含全部连接，用于展示 / 断言）。
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.servers
            .values()
            .flatten()
            .map(TrashEntry::byte_len)
            .sum()
    }

    /// 按句柄取一条完整记录（含载荷）。
    #[must_use]
    pub fn get(&self, server: &str, id: &str) -> Option<&TrashEntry> {
        self.entries(server).iter().find(|entry| entry.id() == id)
    }

    /// 入站一条记录，返回被预算**驱逐**的旧条目（从最旧开始）。
    ///
    /// 载荷超出 [`TRASH_MAX_PAYLOAD`] 时**拒收**并返回 `None`——这是第二道体积闸的
    /// 兜底（第一道在 `DUMP` 之前，见 [`should_stash`]）。调用方通常已经用
    /// [`payload_fits`] 判过，这里再判一次是为了让「忘了判」不会变成「撑爆 kv」。
    ///
    /// 同一个 `id` 已存在时先移除旧的：`DUMP` 间隔极短地删同一个键（同一毫秒）
    /// 会撞出同一个 id，留着两条会让恢复时的定位有歧义。
    pub fn insert(&mut self, server: &str, entry: TrashEntry) -> Option<Vec<TrashEntry>> {
        if !payload_fits(entry.byte_len()) {
            return None;
        }
        let id = entry.id();
        let bucket = self.servers.entry(server.to_owned()).or_default();
        bucket.retain(|existing| existing.id() != id);
        bucket.push(entry);
        // 存储不变式：按删除时间升序。删除是随时间发生的，追加通常已经是升序；
        // 这里显式排一次是为了让「先塞一条旧的」这类调用点也不会破坏它。
        bucket.sort_by_key(|existing| existing.deleted_at_ms);
        Some(Self::evict_to_budget(bucket))
    }

    /// 把一条桶裁到两条预算之内，返回被裁掉的（从最旧开始）。
    fn evict_to_budget(bucket: &mut Vec<TrashEntry>) -> Vec<TrashEntry> {
        let mut evicted = Vec::new();
        while Self::over_budget(bucket) {
            // 永远从**最旧**的一端驱逐：回收站保「刚删的」比保「很久以前删的」有用。
            evicted.push(bucket.remove(0));
        }
        evicted
    }

    fn over_budget(bucket: &[TrashEntry]) -> bool {
        bucket.len() > TRASH_MAX_ENTRIES
            || bucket.iter().map(TrashEntry::byte_len).sum::<usize>() > TRASH_MAX_TOTAL_BYTES
    }

    /// 取出并移除一条记录（**恢复**用：拿到载荷后无论成败都该把它从回收站摘掉，
    /// 由调用方在恢复成功后调用 [`Self::remove`] 也行，但取走更直接）。
    pub fn take(&mut self, server: &str, id: &str) -> Option<TrashEntry> {
        let bucket = self.servers.get_mut(server)?;
        let index = bucket.iter().position(|entry| entry.id() == id)?;
        Some(bucket.remove(index))
    }

    /// 移除一条记录，返回它原本在不在。
    pub fn remove(&mut self, server: &str, id: &str) -> bool {
        let Some(bucket) = self.servers.get_mut(server) else {
            return false;
        };
        let before = bucket.len();
        bucket.retain(|entry| entry.id() != id);
        bucket.len() != before
    }

    /// 清掉一台连接里删除时间早于保留窗口的条目，返回清掉几条。
    ///
    /// 触发点是「事件」而不是定时器：新删一个键时顺手扫一遍（对齐 zedis
    /// `stash_key_to_trash` 里的 opportunistic cleanup），以及打开回收站时先清后列。
    pub fn purge_expired(&mut self, server: &str, now_ms: i64) -> usize {
        let Some(bucket) = self.servers.get_mut(server) else {
            return 0;
        };
        let cutoff = now_ms - TRASH_RETENTION_MS;
        let before = bucket.len();
        bucket.retain(|entry| entry.deleted_at_ms >= cutoff);
        before - bucket.len()
    }

    /// 清掉**所有**连接里的过期条目，返回清掉几条。
    ///
    /// 启动时兜一次底：某台连接可能从此再没被删过键、也没被打开过回收站，
    /// 只靠 `purge_expired` 就永远清不掉它的旧条目。
    pub fn purge_all_expired(&mut self, now_ms: i64) -> usize {
        let cutoff = now_ms - TRASH_RETENTION_MS;
        let mut removed = 0;
        for bucket in self.servers.values_mut() {
            let before = bucket.len();
            bucket.retain(|entry| entry.deleted_at_ms >= cutoff);
            removed += before - bucket.len();
        }
        removed
    }

    /// 一台连接被删掉时，连它的回收站一起丢。
    pub fn forget_connection(&mut self, server: &str) {
        self.servers.remove(server);
    }

    /// 某台连接已经全空 ⇒ 把它整条摘掉（不留 `"conn": []` 空壳）。
    pub fn drop_if_empty(&mut self, server: &str) {
        if self.servers.get(server).is_some_and(Vec::is_empty) {
            self.servers.remove(server);
        }
    }

    /// 当前版本号（测试用）。
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, db: u32, deleted_at_ms: i64, payload: &[u8]) -> TrashEntry {
        TrashEntry {
            key: key.to_owned(),
            db,
            pttl_ms: 5_000,
            deleted_at_ms,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn a_stashed_entry_round_trips_through_the_store() {
        let mut store = TrashStore::new();
        let stashed = entry("user:1", 2, 1_000, b"\x00\x01dump");
        let id = stashed.id();
        assert!(store.insert("conn", stashed.clone()).is_some());

        let found = store.get("conn", &id).expect("按句柄取回");
        assert_eq!(found.key, "user:1");
        assert_eq!(found.db, 2);
        assert_eq!(found.payload, b"\x00\x01dump");
        assert_eq!(store.len("conn"), 1);
    }

    #[test]
    fn the_entry_id_sorts_in_deletion_order() {
        let older = entry("k", 0, 1_000, b"a");
        let newer = entry("k", 0, 2_000, b"a");
        // 零填充 20 位 ⇒ 字典序 == 时间序（zedis 存 redb 的复合键同构）。
        assert!(older.id() < newer.id());
        assert_eq!(older.id(), format!("{:020}:0:k", 1_000));
    }

    #[test]
    fn listing_is_newest_first_and_payload_free() {
        let mut store = TrashStore::new();
        store.insert("conn", entry("user:1", 0, 1_000, b"old"));
        store.insert("conn", entry("user:2", 3, 2_000, b"new"));

        let metas = store.metas("conn");
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].key, "user:2");
        assert_eq!(metas[0].db, 3);
        assert_eq!(metas[1].key, "user:1");
        // 存储顺序与展示顺序相反。
        assert_eq!(store.entries("conn")[0].key, "user:1");
    }

    #[test]
    fn an_unknown_connection_lists_nothing() {
        let store = TrashStore::new();
        assert!(store.metas("nobody").is_empty());
        assert_eq!(store.len("nobody"), 0);
        assert!(store.get("nobody", "whatever").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn the_bin_is_isolated_per_connection() {
        let mut store = TrashStore::new();
        store.insert("srv-a", entry("user:1", 0, 1_000, b"a"));
        store.insert("srv-b", entry("user:1", 0, 1_000, b"b"));

        assert_eq!(store.len("srv-a"), 1);
        assert_eq!(store.len("srv-b"), 1);
        let id = entry("user:1", 0, 1_000, b"a").id();
        assert_eq!(store.get("srv-a", &id).unwrap().payload, b"a");
        assert_eq!(store.get("srv-b", &id).unwrap().payload, b"b");
    }

    #[test]
    fn forgetting_a_connection_drops_its_bin() {
        let mut store = TrashStore::new();
        store.insert("srv-a", entry("k", 0, 1_000, b"a"));
        store.insert("srv-b", entry("k", 0, 1_000, b"b"));
        store.forget_connection("srv-a");
        assert_eq!(store.len("srv-a"), 0);
        assert_eq!(store.len("srv-b"), 1);
    }

    #[test]
    fn taking_an_entry_removes_it_and_returns_the_payload() {
        let mut store = TrashStore::new();
        let stashed = entry("k", 1, 1_000, b"payload");
        let id = stashed.id();
        store.insert("conn", stashed);

        let taken = store.take("conn", &id).expect("取走");
        assert_eq!(taken.payload, b"payload");
        assert!(store.get("conn", &id).is_none());
        assert!(store.take("conn", &id).is_none(), "取第二次应当取不到");
    }

    #[test]
    fn removing_an_entry_reports_whether_it_was_there() {
        let mut store = TrashStore::new();
        let id = entry("k", 0, 1_000, b"x").id();
        store.insert("conn", entry("k", 0, 1_000, b"x"));
        assert!(store.remove("conn", &id));
        assert!(!store.remove("conn", &id));
        // 未知连接上的移除也应当是 `false`，不该 panic。
        assert!(!store.remove("nobody", &id));
    }

    #[test]
    fn a_second_entry_with_the_same_id_replaces_the_first() {
        let mut store = TrashStore::new();
        // 同一个 (毫秒, 库, 键) ⇒ 同一个 id。
        store.insert("conn", entry("k", 0, 1_000, b"first"));
        store.insert("conn", entry("k", 0, 1_000, b"second"));
        assert_eq!(store.len("conn"), 1, "同 id 应当替换而不是并存");
        let id = entry("k", 0, 1_000, b"x").id();
        assert_eq!(store.get("conn", &id).unwrap().payload, b"second");
    }

    #[test]
    fn a_payload_over_the_per_entry_cap_is_rejected() {
        let mut store = TrashStore::new();
        let too_big = vec![0u8; TRASH_MAX_PAYLOAD + 1];
        assert!(
            store
                .insert("conn", entry("k", 0, 1_000, &too_big))
                .is_none()
        );
        assert_eq!(store.len("conn"), 0, "拒收就不该留下任何痕迹");
    }

    #[test]
    fn a_payload_exactly_at_the_cap_is_accepted() {
        let mut store = TrashStore::new();
        let at_cap = vec![7u8; TRASH_MAX_PAYLOAD];
        assert!(
            store
                .insert("conn", entry("k", 0, 1_000, &at_cap))
                .is_some()
        );
        assert_eq!(store.len("conn"), 1);
    }

    #[test]
    fn an_empty_payload_is_still_a_record() {
        let mut store = TrashStore::new();
        assert!(store.insert("conn", entry("k", 0, 1_000, b"")).is_some());
        assert_eq!(store.len("conn"), 1);
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn the_entry_budget_evicts_the_oldest_first() {
        let mut store = TrashStore::new();
        // 塞满 + 1 条：每条的载荷都极小，所以只会撞条目数预算、不会撞字节预算。
        for index in 0..=TRASH_MAX_ENTRIES {
            let at = 1_000 + index as i64;
            store.insert("conn", entry(&format!("k{index}"), 0, at, b"x"));
        }
        assert_eq!(store.len("conn"), TRASH_MAX_ENTRIES);
        // 最旧的那条（k0）被挤掉，最新的那条还在。
        assert_eq!(store.entries("conn")[0].key, "k1");
        assert_eq!(
            store.entries("conn").last().unwrap().key,
            format!("k{TRASH_MAX_ENTRIES}")
        );
    }

    #[test]
    fn the_entry_budget_reports_what_it_evicted() {
        let mut store = TrashStore::new();
        for index in 0..TRASH_MAX_ENTRIES {
            let at = 1_000 + index as i64;
            store.insert("conn", entry(&format!("k{index}"), 0, at, b"x"));
        }
        let evicted = store
            .insert("conn", entry("newest", 0, 999_999, b"x"))
            .expect("入站");
        assert_eq!(evicted.len(), 1, "超出预算应驱逐正好一条");
        assert_eq!(evicted[0].key, "k0");
    }

    #[test]
    fn the_byte_budget_evicts_the_oldest_first() {
        let mut store = TrashStore::new();
        // 每条 1 MiB（正好在上限内），16 MiB 预算。前 16 条（k0..k15）正好压在预算上
        // 而不超（`>` 是严格比较，与 `payload_fits` 同一条口径），从第 17 条起每进一条
        // 就挤掉一条最旧的 ⇒ 收尾时桶里是 k2..k17。条目数预算（200）远没到，
        // 所以驱动驱逐的一定是字节预算。
        let blob = vec![3u8; TRASH_MAX_PAYLOAD];
        let at_capacity = TRASH_MAX_TOTAL_BYTES / TRASH_MAX_PAYLOAD;
        for index in 0..=at_capacity + 1 {
            let at = 1_000 + index as i64;
            store.insert("conn", entry(&format!("k{index}"), 0, at, &blob));
        }
        assert_eq!(store.len("conn"), at_capacity, "桶应当正好被裁到字节预算");
        // 总量始终不超预算。
        assert!(store.total_bytes() <= TRASH_MAX_TOTAL_BYTES);
        // 最旧的两条（k0 / k1）被挤掉，桶头是 k2。
        assert_eq!(store.entries("conn")[0].key, "k2");
        assert_eq!(
            store.entries("conn").last().unwrap().key,
            format!("k{}", at_capacity + 1)
        );
    }

    #[test]
    fn expired_entries_are_purged_by_the_cutoff() {
        let mut store = TrashStore::new();
        let now = 10 * TRASH_RETENTION_MS;
        store.insert(
            "conn",
            entry("stale", 0, now - TRASH_RETENTION_MS - 1, b"a"),
        );
        store.insert("conn", entry("fresh", 0, now - 1, b"b"));

        assert_eq!(store.purge_expired("conn", now), 1);
        assert_eq!(store.len("conn"), 1);
        assert_eq!(store.entries("conn")[0].key, "fresh");
        // 清第二次没有可清的。
        assert_eq!(store.purge_expired("conn", now), 0);
        // 未知连接上清也是 0，不该 panic。
        assert_eq!(store.purge_expired("nobody", now), 0);
    }

    #[test]
    fn an_entry_exactly_at_the_retention_edge_is_kept() {
        let mut store = TrashStore::new();
        let now = 10 * TRASH_RETENTION_MS;
        store.insert("conn", entry("edge", 0, now - TRASH_RETENTION_MS, b"a"));
        assert_eq!(store.purge_expired("conn", now), 0, "边界值应当保留");
        assert_eq!(store.len("conn"), 1);
    }

    #[test]
    fn purge_all_sweeps_every_connection() {
        let mut store = TrashStore::new();
        let now = 10 * TRASH_RETENTION_MS;
        let stale = now - TRASH_RETENTION_MS - 1;
        store.insert("srv-a", entry("old", 0, stale, b"a"));
        store.insert("srv-b", entry("old", 0, stale, b"b"));
        store.insert("srv-a", entry("fresh", 0, now - 1, b"c"));

        assert_eq!(store.purge_all_expired(now), 2);
        assert_eq!(store.len("srv-a"), 1);
        assert_eq!(store.len("srv-b"), 0);
    }

    #[test]
    fn a_cleared_connection_is_dropped_from_the_document() {
        let mut store = TrashStore::new();
        let id = entry("k", 0, 1_000, b"x").id();
        store.insert("conn", entry("k", 0, 1_000, b"x"));
        store.remove("conn", &id);
        store.drop_if_empty("conn");
        assert!(store.is_empty());
        // 落盘文档里不该留一个空壳。
        let json = store.to_json().expect("序列化");
        assert!(!json.contains("conn"), "空壳不该进落盘文档：{json}");
    }

    #[test]
    fn a_non_empty_connection_survives_drop_if_empty() {
        let mut store = TrashStore::new();
        store.insert("conn", entry("k", 0, 1_000, b"x"));
        store.drop_if_empty("conn");
        assert_eq!(store.len("conn"), 1);
    }

    #[test]
    fn the_store_round_trips_through_json_with_binary_payloads() {
        let mut store = TrashStore::new();
        // 含 0x00 / 0xff 的二进制载荷：JSON 数组编码会失真，base64 不会。
        let binary: Vec<u8> = (0..=255u8).collect();
        store.insert("conn", entry("bin", 4, 1_000, &binary));
        store.insert("srv-b", entry("other", 0, 2_000, b"plain"));

        let json = store.to_json().expect("序列化");
        let restored = TrashStore::from_json(&json);
        assert_eq!(restored, store);
        let id = entry("bin", 4, 1_000, b"").id();
        assert_eq!(restored.get("conn", &id).unwrap().payload, binary);
    }

    #[test]
    fn a_future_envelope_version_falls_back_to_empty() {
        let mut store = TrashStore::new();
        store.insert("conn", entry("k", 0, 1_000, b"x"));
        let json = store
            .to_json()
            .expect("序列化")
            .replace("\"version\":1", "\"version\":99");
        assert!(TrashStore::from_json(&json).is_empty());
    }

    #[test]
    fn a_broken_document_falls_back_to_empty() {
        assert!(TrashStore::from_json("not json at all").is_empty());
        assert!(TrashStore::from_json("{}").is_empty());
        // 载荷不是合法 base64 ⇒ 整份解析失败 ⇒ 空表（不拿半份坏数据）。
        let broken = r#"{"version":1,"servers":{"conn":[{"key":"k","db":0,"pttl_ms":0,"deleted_at_ms":1,"payload":"!!! not base64 !!!"}]}}"#;
        assert!(TrashStore::from_json(broken).is_empty());
    }

    #[test]
    fn a_fresh_store_carries_the_current_version() {
        assert_eq!(TrashStore::new().version(), TRASH_STORE_VERSION);
    }

    #[test]
    fn the_pre_gate_admits_unknown_and_small_values_only() {
        // 探测失败（`None`）放行——不因为量不到就把回收站关掉。
        assert!(should_stash(None));
        assert!(should_stash(Some(0)));
        // 正好在上限上放行。
        assert!(should_stash(Some(TRASH_MAX_VALUE_MEMORY)));
        // 超一点也不放行。
        assert!(!should_stash(Some(TRASH_MAX_VALUE_MEMORY + 1)));
    }

    #[test]
    fn the_payload_cap_boundary_is_inclusive() {
        assert!(payload_fits(0));
        assert!(payload_fits(TRASH_MAX_PAYLOAD));
        assert!(!payload_fits(TRASH_MAX_PAYLOAD + 1));
    }

    #[test]
    fn a_negative_ttl_is_normalised_for_restore() {
        let mut store = TrashStore::new();
        store.insert("conn", entry("k", 0, 1_000, b"x"));
        let id = entry("k", 0, 1_000, b"x").id();
        assert_eq!(store.get("conn", &id).unwrap().restore_pttl_ms(), 5_000);

        // 删除时已无过期（`PTTL` 返回 -1）⇒ 恢复时下发 0（= 不过期）。
        let mut no_ttl = entry("k", 0, 1_000, b"x");
        no_ttl.pttl_ms = -1;
        assert_eq!(no_ttl.restore_pttl_ms(), 0);
    }

    #[test]
    fn the_metas_carry_the_handle_used_to_restore() {
        let mut store = TrashStore::new();
        let stashed = entry("user:7", 3, 1_234, b"payload");
        let id = stashed.id();
        store.insert("conn", stashed);
        let metas = store.metas("conn");
        assert_eq!(metas[0].id, id);
        assert_eq!(metas[0].pttl_ms, 5_000);
        assert_eq!(metas[0].deleted_at_ms, 1_234);
        // 列表行带上载荷长度，但不带载荷本身（界面要的是「占多大」）。
        assert_eq!(metas[0].bytes, b"payload".len());
        // 列表行→句柄→完整记录：界面恢复走的就是这条路。
        assert_eq!(store.get("conn", &metas[0].id).unwrap().key, "user:7");
    }
}
