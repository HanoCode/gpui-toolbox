//! 键的**本地**标注：标签颜色 / 备注 / 收藏。
//!
//! 数据只住在本地 kv 里，**服务端一个字节都不占、一次往返都不发**。这与
//! [`crate::ttl`] / [`crate::sort`] / [`crate::query`] 是同一类东西：纯数据 + 纯函数，
//! 零 gpui、可单测。
//!
//! # 对齐 zedis（`temp/zedis/crates/zedis-db/src/key_metadata_manager.rs`）
//!
//! - 调色板**六档**（`Red`/`Orange`/`Yellow`/`Green`/`Blue`/`Purple`）：不给取色器，
//!   六个色块刚好一行，也就没有「选一个说不清的颜色」这种灰区。
//! - 一条标注 = `tag` + `note`，两者都可空；**两个都空 ⇒ 这条记录不存在**（不是
//!   「存了一条空记录」）。所以清除等于删除，落盘文档不会积死行。
//! - 落盘是**版本化信封**（`{ "version": 1, "servers": { … } }`），将来要加
//!   「多标签 / 每个标签各带备注」时才好在原地迁移，而不是让老客户端读不了。
//! - 文件夹行不自己带标签，而是由**后代键的标签**聚合出一个「众数色」；后代里
//!   出现两种以上颜色时另给一个 `mixed` 标记（渲染层据此区分「这个文件夹就是红的」
//!   与「这个文件夹里什么颜色都有」）。
//!
//! # 两条边界
//!
//! 1. **标注的键是「连接 id + 键名」**，不是全局键名：两台服务端上都有 `user:1`
//!    时，给其中一台标红不该把另一台也标红（zedis 同款，见其 `one_servers_…` 测试）。
//! 2. **键消失不会自动清标注**。SCAN 分页只覆盖已加载的键，若按「当前这批键」
//!    裁剪标注，翻一页就会把别的页上的标注整片抹掉。清理只发生在两处：用户显式
//!    删键（`KeyTags::remove`）、用户自己清除（`KeyTags::set` 传空）。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 调色板的档数（`[u32; TAG_COLOR_COUNT]` 直方图的宽度）。
pub const TAG_COLOR_COUNT: usize = 6;

/// 落盘信封的当前版本。
///
/// 读盘时版本对不上就**回落空表**（不猜、不硬解）——标注是用户手写的，宁可
/// 让它「这次没读出来」，也不能拿一套变了形的字段去覆盖它。
pub const TAG_STORE_VERSION: u32 = 1;

/// 预设标签色（六档，按显示顺序）。
///
/// 语义由**用户**赋予（红=危险、绿=常量…），内核不做任何解释——同一套颜色
/// 在不同团队里含义正好相反，硬编码一版解释等于替用户做决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TagColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl TagColor {
    /// 全部档位，**顺序 = 色块行顺序 = 直方图下标顺序**。
    pub const ALL: [Self; TAG_COLOR_COUNT] = [
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Purple,
    ];

    /// 稳定小写标识：同时是落盘 JSON 的值、直方图下标的来源、i18n 键后缀
    /// （`tag.color.red` 一类）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Purple => "purple",
        }
    }

    /// 在 [`Self::ALL`] 里的下标（= 直方图下标）。
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Red => 0,
            Self::Orange => 1,
            Self::Yellow => 2,
            Self::Green => 3,
            Self::Blue => 4,
            Self::Purple => 5,
        }
    }

    /// [`Self::as_str`] 的逆。大小写不敏感、两端空白容错（手工写的档案与
    /// 未来换了大小写习惯的版本都能读回来）；认不出的一律 `None`。
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|color| color.as_str().eq_ignore_ascii_case(name))
    }
}

/// 一个键的本地标注。
///
/// 两个字段都可空（只标色 / 只写备注 / 两者都有都是合法状态）；
/// 两个都空时 [`is_empty`](Self::is_empty) 为真，调用方应把它当作「没有记录」。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagColor>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl KeyMetadata {
    /// 色与备注都空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tag.is_none() && self.note.is_empty()
    }
}

/// 一台连接上的全部标注：`键名 → 标注` + 收藏键名集合。
///
/// 用 `BTreeMap` / `BTreeSet`：落盘 JSON 的键序稳定，前后两次保存的字节差异
/// 只反映真实改动（便于人工 diff 与「文件有没有被动过」的判断）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTags {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    entries: BTreeMap<String, KeyMetadata>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    favorites: BTreeSet<String>,
}

impl KeyTags {
    /// 常量空表：给「没有活动连接」这类调用点返回 `&KeyTags` 用
    /// （省掉一层 `Option`，也让调用方不必自备一个静态空值）。
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
            favorites: BTreeSet::new(),
        }
    }

    /// 取一个键的标注。
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&KeyMetadata> {
        self.entries.get(key)
    }

    /// 取一个键的标签色。
    #[must_use]
    pub fn tag_of(&self, key: &str) -> Option<TagColor> {
        self.entries.get(key).and_then(|meta| meta.tag)
    }

    /// 取一个键的备注（没有记录时是空串）。
    #[must_use]
    pub fn note_of(&self, key: &str) -> &str {
        self.entries.get(key).map_or("", |meta| meta.note.as_str())
    }

    /// 是否已收藏。
    #[must_use]
    pub fn is_favorite(&self, key: &str) -> bool {
        self.favorites.contains(key)
    }

    /// 写一条标注。**两个字段都空 ⇒ 删除这条记录**（不留空行）。
    pub fn set(&mut self, key: &str, tag: Option<TagColor>, note: &str) {
        let metadata = KeyMetadata {
            tag,
            note: note.to_owned(),
        };
        if metadata.is_empty() {
            self.entries.remove(key);
        } else {
            self.entries.insert(key.to_owned(), metadata);
        }
    }

    /// 只改标签色，**备注原样留着**（改色不该顺手擦掉用户写的字）。
    pub fn set_tag(&mut self, key: &str, tag: Option<TagColor>) {
        let note = self.note_of(key).to_owned();
        self.set(key, tag, &note);
    }

    /// 只改备注，标签原样留着。
    pub fn set_note(&mut self, key: &str, note: &str) {
        let tag = self.tag_of(key);
        self.set(key, tag, note);
    }

    /// 切换收藏，返回切换后的状态。
    pub fn toggle_favorite(&mut self, key: &str) -> bool {
        if self.favorites.remove(key) {
            false
        } else {
            self.favorites.insert(key.to_owned());
            true
        }
    }

    /// 丢掉一个键的全部本地痕迹（标注 + 收藏）。
    ///
    /// 只在**用户显式删键**时调用：那时这个键在服务端已经没了，留着标注只会
    /// 让将来同名的新键继承一份说不清来源的颜色。
    pub fn forget(&mut self, key: &str) {
        self.entries.remove(key);
        self.favorites.remove(key);
    }

    /// 标注条数（不含收藏）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 标注与收藏都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.favorites.is_empty()
    }

    /// 遍历全部标注（键序）。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &KeyMetadata)> {
        self.entries.iter().map(|(key, meta)| (key.as_str(), meta))
    }
}

/// 全部连接的标注集合，**落盘形态就是这个结构**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagStore {
    #[serde(default = "current_version")]
    version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    servers: BTreeMap<String, KeyTags>,
}

fn current_version() -> u32 {
    TAG_STORE_VERSION
}

impl Default for TagStore {
    fn default() -> Self {
        Self {
            version: TAG_STORE_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

impl TagStore {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 JSON 读。版本对不上、或整串解析失败时**回落空表**
    /// （见 [`TAG_STORE_VERSION`]）；这里返回 `Err` 的只有一种情况：调用方
    /// 想区分「文件是空的」与「文件读坏了」时自己用 `serde_json` 解。
    #[must_use]
    pub fn from_json(json: &str) -> Self {
        let Ok(store) = serde_json::from_str::<Self>(json) else {
            return Self::default();
        };
        if store.version == TAG_STORE_VERSION {
            store
        } else {
            Self::default()
        }
    }

    /// 序列化成落盘 JSON。
    ///
    /// # Errors
    ///
    /// `BTreeMap<String, …>` 的序列化不会失败；签名保留 `Result` 是为了让调用方
    /// 与 `serde_json::to_string` 的其他用法写法一致。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 取一台连接的标注（没有就是 `None`，由调用方决定回落什么）。
    #[must_use]
    pub fn get(&self, connection: &str) -> Option<&KeyTags> {
        self.servers.get(connection)
    }

    /// 取一台连接的标注，没有就**建一个空的**。
    pub fn get_mut(&mut self, connection: &str) -> &mut KeyTags {
        self.servers.entry(connection.to_owned()).or_default()
    }

    /// 某台连接的标注已经全空 ⇒ 把它整条摘掉。
    ///
    /// 不做这一步的话，用户「标了又清」之后落盘里会留下一个 `"conn": {}`，
    /// 次数多了文档里就积一片空壳。
    pub fn drop_if_empty(&mut self, connection: &str) {
        if self
            .servers
            .get(connection)
            .is_some_and(|tags| tags.is_empty())
        {
            self.servers.remove(connection);
        }
    }

    /// 一台连接被删掉时，连它的标注一起丢。
    pub fn forget_connection(&mut self, connection: &str) {
        self.servers.remove(connection);
    }

    /// 当前版本号（测试与展示用）。
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// 有没有任何连接的标注。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// 标签档位过滤（本地筛，与服务端的类型 / 过期时间过滤是同一层的东西）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TagFilter {
    /// 不看标签。
    #[default]
    All,
    /// 只看已标色的键。
    Tagged,
    /// 只看没标色的键。
    Untagged,
    /// 只看已收藏的键。
    Favorites,
    /// 只看着一档颜色的键。
    Color(TagColor),
}

impl TagFilter {
    /// 菜单顺序：四个整体档在前，六个颜色档按调色板顺序在后。
    pub const ALL: [Self; 10] = [
        Self::All,
        Self::Tagged,
        Self::Untagged,
        Self::Favorites,
        Self::Color(TagColor::Red),
        Self::Color(TagColor::Orange),
        Self::Color(TagColor::Yellow),
        Self::Color(TagColor::Green),
        Self::Color(TagColor::Blue),
        Self::Color(TagColor::Purple),
    ];

    /// 加一个键是否在档内。
    ///
    /// 两个入参而不是一个 `&KeyTags`：判据只有「颜色」与「是否收藏」两点，
    /// 让本函数保持无依赖的纯函数才好单测（也让树 / 平铺两条路径共用）。
    #[must_use]
    pub fn matches(self, tag: Option<TagColor>, favorite: bool) -> bool {
        match self {
            Self::All => true,
            Self::Tagged => tag.is_some(),
            Self::Untagged => tag.is_none(),
            Self::Favorites => favorite,
            Self::Color(color) => tag == Some(color),
        }
    }

    /// 稳定标识（落盘偏好 / 无障碍名 / i18n 键后缀）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Tagged => "tagged",
            Self::Untagged => "untagged",
            Self::Favorites => "favorites",
            Self::Color(TagColor::Red) => "red",
            Self::Color(TagColor::Orange) => "orange",
            Self::Color(TagColor::Yellow) => "yellow",
            Self::Color(TagColor::Green) => "green",
            Self::Color(TagColor::Blue) => "blue",
            Self::Color(TagColor::Purple) => "purple",
        }
    }
}

/// 六档颜色的计数，下标 = [`TagColor::index`]。
pub type TagHistogram = [u32; TAG_COLOR_COUNT];

/// 把一张直方图收成「文件夹行的那一个颜色」：众数 + 是否混色。
///
/// 返回 `None` 表示**后代里一个带色的键都没有**（不是「众数是红、计数 0」）。
/// 数量相同时取下标靠前的那档（`ALL` 的顺序），于是同一批键每次都得到同一个
/// 结果，不会在两次重绘之间换色。
#[must_use]
pub fn resolve_folder_tag(histogram: &TagHistogram) -> Option<(TagColor, bool)> {
    let mut best_index = 0usize;
    let mut best_count = 0u32;
    let mut distinct = 0u32;
    for (index, &count) in histogram.iter().enumerate() {
        if count > 0 {
            distinct += 1;
        }
        if count > best_count {
            best_count = count;
            best_index = index;
        }
    }
    if best_count == 0 {
        return None;
    }
    Some((TagColor::ALL[best_index], distinct > 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram(pairs: &[(TagColor, u32)]) -> TagHistogram {
        let mut out: TagHistogram = [0; TAG_COLOR_COUNT];
        for (color, count) in pairs {
            out[color.index()] += count;
        }
        out
    }

    #[test]
    fn tag_colors_round_trip_through_their_wire_name() {
        for color in TagColor::ALL {
            assert_eq!(TagColor::from_name(color.as_str()), Some(color));
            // 大小写与空白容错：手工改过档案也能读回来。
            assert_eq!(
                TagColor::from_name(&color.as_str().to_uppercase()),
                Some(color)
            );
            assert_eq!(
                TagColor::from_name(&format!("  {} ", color.as_str())),
                Some(color)
            );
        }
        assert_eq!(TagColor::from_name("turquoise"), None);
        assert_eq!(TagColor::from_name(""), None);
    }

    #[test]
    fn color_index_matches_the_palette_order() {
        for (index, color) in TagColor::ALL.into_iter().enumerate() {
            assert_eq!(color.index(), index, "{color:?} 的下标与 ALL 的顺序不一致");
        }
    }

    #[test]
    fn an_annotation_with_neither_tag_nor_note_is_not_a_record() {
        let mut tags = KeyTags::empty();
        tags.set("k", Some(TagColor::Red), "热键");
        assert_eq!(tags.len(), 1);

        // 两个字段都清空 = 删掉这条记录，而不是留一条空记录。
        tags.set("k", None, "");
        assert_eq!(tags.len(), 0);
        assert!(tags.get("k").is_none());

        // 只清一个字段仍然是一条记录。
        tags.set("k", Some(TagColor::Blue), "备注");
        tags.set_tag("k", None);
        assert_eq!(tags.note_of("k"), "备注");
        tags.set_note("k", "");
        assert!(tags.get("k").is_none());
    }

    #[test]
    fn changing_the_colour_keeps_the_note_and_vice_versa() {
        let mut tags = KeyTags::empty();
        tags.set("k", Some(TagColor::Green), "常量");
        tags.set_tag("k", Some(TagColor::Purple));
        assert_eq!(tags.note_of("k"), "常量");
        tags.set_note("k", "改了口径");
        assert_eq!(tags.tag_of("k"), Some(TagColor::Purple));
    }

    #[test]
    fn favorites_toggle_both_ways_and_are_independent_of_tags() {
        let mut tags = KeyTags::empty();
        assert!(!tags.is_favorite("k"));
        assert!(tags.toggle_favorite("k"));
        assert!(tags.is_favorite("k"));
        // 收藏不产生标注（`len` 数的是标注）。
        assert_eq!(tags.len(), 0);
        assert!(!tags.is_empty());
        assert!(!tags.toggle_favorite("k"));
        assert!(tags.is_empty(), "取消收藏之后这本表就空了");
    }

    #[test]
    fn forgetting_a_key_drops_both_its_colour_and_its_star() {
        let mut tags = KeyTags::empty();
        tags.set("k", Some(TagColor::Red), "x");
        tags.toggle_favorite("k");
        tags.forget("k");
        assert!(tags.is_empty());
    }

    #[test]
    fn two_servers_annotating_the_same_key_name_do_not_collide() {
        // 这是「标注必须按连接隔离」的那条边界（zedis 同款测试）。
        let mut store = TagStore::new();
        store
            .get_mut("redis-a")
            .set("user:1", Some(TagColor::Red), "a");
        store
            .get_mut("redis-b")
            .set("user:1", Some(TagColor::Blue), "b");
        assert_eq!(
            store.get("redis-a").and_then(|tags| tags.tag_of("user:1")),
            Some(TagColor::Red)
        );
        assert_eq!(
            store.get("redis-b").and_then(|tags| tags.tag_of("user:1")),
            Some(TagColor::Blue)
        );
    }

    #[test]
    fn tag_store_round_trips_through_json() {
        let mut store = TagStore::new();
        store
            .get_mut("redis-a")
            .set("user:1", Some(TagColor::Red), "热");
        store.get_mut("redis-a").toggle_favorite("user:2");
        store
            .get_mut("redis-b")
            .set("k", Some(TagColor::Purple), "");
        let json = store.to_json().expect("序列化不会失败");
        let back = TagStore::from_json(&json);
        assert_eq!(back.version(), TAG_STORE_VERSION);
        assert_eq!(
            back.get("redis-a").and_then(|tags| tags.tag_of("user:1")),
            Some(TagColor::Red)
        );
        assert!(
            back.get("redis-a")
                .is_some_and(|tags| tags.is_favorite("user:2"))
        );
        assert_eq!(
            back.get("redis-b")
                .and_then(|tags| tags.get("k"))
                .map(|meta| meta.note.as_str()),
            Some(""),
            "只有色、没有备注的记录也要能读回来"
        );
    }

    #[test]
    fn a_future_envelope_version_falls_back_to_empty() {
        // 版本不是本档 ⇒ 整份回落空表，而不是拿变了形的字段去覆盖用户的标注。
        let json = r#"{"version":99,"servers":{"redis-a":{"entries":{"k":{"tag":"red"}}}}}"#;
        let store = TagStore::from_json(json);
        assert_eq!(store.version(), TAG_STORE_VERSION);
        assert!(store.is_empty());

        // 解析不出来也回落空表。
        assert!(TagStore::from_json("这不是 JSON").is_empty());
        assert!(TagStore::from_json("").is_empty());
    }

    #[test]
    fn a_cleared_server_is_dropped_from_the_document() {
        let mut store = TagStore::new();
        store.get_mut("redis-a").set("k", Some(TagColor::Red), "");
        store.drop_if_empty("redis-a");
        assert_eq!(
            store.get("redis-a").map(KeyTags::len),
            Some(1),
            "非空时不动它"
        );

        store.get_mut("redis-a").set("k", None, "");
        store.drop_if_empty("redis-a");
        assert!(store.is_empty(), "标了又清 ⇒ 文档里不留空壳");
    }

    #[test]
    fn folder_tag_is_the_mode_colour() {
        let mode = resolve_folder_tag(&histogram(&[(TagColor::Red, 3), (TagColor::Blue, 1)]))
            .expect("有带色的后代就有聚合色");
        assert_eq!(mode, (TagColor::Red, true), "红多、且不止一种颜色");

        let uniform = resolve_folder_tag(&histogram(&[(TagColor::Green, 2)])).expect("同款");
        assert_eq!(uniform, (TagColor::Green, false), "只有一种颜色就不算混色");
    }

    #[test]
    fn folder_tag_ties_follow_the_palette_order() {
        // 红与紫各 1：取下标靠前的红 —— 同一批键每次得到同一个色，不会来回抖。
        let tied = resolve_folder_tag(&histogram(&[(TagColor::Purple, 1), (TagColor::Red, 1)]))
            .expect("同款");
        assert_eq!(tied, (TagColor::Red, true));
    }

    #[test]
    fn folder_without_tagged_descendants_has_no_colour() {
        assert_eq!(resolve_folder_tag(&[0; TAG_COLOR_COUNT]), None);
    }

    #[test]
    fn tag_filter_matches_only_its_own_slice() {
        // 四个整体档的口径。
        assert!(TagFilter::All.matches(None, false));
        assert!(TagFilter::All.matches(Some(TagColor::Red), true));
        assert!(TagFilter::Tagged.matches(Some(TagColor::Red), false));
        assert!(!TagFilter::Tagged.matches(None, true), "收藏不算标色");
        assert!(TagFilter::Untagged.matches(None, false));
        assert!(!TagFilter::Untagged.matches(Some(TagColor::Red), false));
        assert!(TagFilter::Favorites.matches(None, true));
        assert!(!TagFilter::Favorites.matches(Some(TagColor::Red), false));

        // 颜色档只看那一个色，且未标色的键不命中。
        assert!(TagFilter::Color(TagColor::Blue).matches(Some(TagColor::Blue), false));
        assert!(!TagFilter::Color(TagColor::Blue).matches(Some(TagColor::Red), false));
        assert!(!TagFilter::Color(TagColor::Blue).matches(None, false));
    }

    #[test]
    fn filter_wire_names_are_stable() {
        // 这些串会进落盘偏好 / 无障碍名，改名等于换语义。
        assert_eq!(TagFilter::All.as_str(), "all");
        assert_eq!(TagFilter::Favorites.as_str(), "favorites");
        assert_eq!(TagFilter::Color(TagColor::Purple).as_str(), "purple");
        for filter in TagFilter::ALL {
            assert!(!filter.as_str().is_empty());
        }
        // 菜单里六个颜色档都在，且顺序 = 调色板顺序（与色块行一致，
        // 用户在菜单里找「刚才点过的那个色」时不用重新认一遍顺序）。
        let colors: Vec<&str> = TagFilter::ALL
            .iter()
            .filter_map(|filter| match filter {
                TagFilter::Color(color) => Some(color.as_str()),
                _ => None,
            })
            .collect();
        let palette: Vec<&str> = TagColor::ALL.iter().map(|color| color.as_str()).collect();
        assert_eq!(colors, palette);
    }
}
