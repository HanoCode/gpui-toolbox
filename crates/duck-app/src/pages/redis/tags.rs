//! 键的本地标注：标签色 / 备注 / 收藏（对齐 zedis 的 KeyMetadata + 收藏）。
//!
//! 数据只住本地 kv（[`KEY_TAGS`] 单键 → [`TagStore`]），**服务端一个字节都不占、
//! 一次往返都不发**。标注是使用者给自己的备忘（「这个键是热键」「这条是上古遗留」），
//! 不是键的属性——所以它不进 `SCAN`、不进 `key_meta`，也不受锁写闸门管
//! （改的是本地文件，不是服务端数据）。
//!
//! # 三个口子
//!
//! 1. **读**：[`RedisPage::active_tags`] 按当前连接 id 取那一张子表；没有活动连接
//!    或没标过时回落 [`NO_TAGS`]（常量空表，省掉一层 `Option`）。
//! 2. **写**：`set_key_tag` / `set_key_note` / `toggle_key_favorite` 三个动作，
//!    各自「改内存 → 摘空壳 → 落盘 → 重算视图」。
//! 3. **存**：[`RedisPage::persist_tags`] 把**整本** `TagStore` 序列化进一个 kv 键。
//!    不拆成「一连接一个 kv 键」：那样删连接时要按前缀枚举去清，而且多出一个
//!    「哪些键属于哪条连接」的约定；一个键一份 JSON 更直白（标注总量是
//!    几十到几百条，不构成体积问题）。
//!
//! # 为什么改一个键也要重算整棵视图
//!
//! 颜色进了**文件夹行的聚合色**（[`stamp_folder_tags`]）——给一个键标红可能让
//! 三层祖先的众数色翻盘。所以染色后必须重跑 [`RedisPage::rebuild_views`]。
//! **备注例外**：备注不进树（只在右栏显示），所以它只落盘 + 重绘，不重建视图——
//! 否则打一行字就要把十万个键重排一次。

use super::*;
// `Hsla` 是 `tag_color` 的返回类型：父模块的 `use gpui::{…}` 里没列它
// （父模块自己不直接用），glob 取不到，这里显式引一次（同 `helpers.rs`）。
use gpui::Hsla;

/// 没有活动连接 / 该连接还没标过任何东西时给调用方的空表。
///
/// `KeyTags::empty()` 是 `const fn`（`BTreeMap::new()` / `BTreeSet::new()` 都是），
/// 所以能直接当 `static` 的初值——省得每个调用点各造一份空表。
static NO_TAGS: KeyTags = KeyTags::empty();

impl RedisPage {
    /// 当前连接的本地标注（没有活动连接 ⇒ 空表）。
    pub(super) fn active_tags(&self) -> &KeyTags {
        self.active
            .as_deref()
            .and_then(|id| self.tags.get(id))
            .unwrap_or(&NO_TAGS)
    }

    /// 从 kv 读全部连接的标注。
    ///
    /// 解析失败 / 版本对不上都由 [`TagStore::from_json`] 收成空表（见内核
    /// `TAG_STORE_VERSION` 的口径：宁可这次没读出来，也不拿变了形的字段覆盖用户的标注）。
    pub(super) fn load_tags(cx: &App) -> TagStore {
        duck_store::kv_get(cx, KEY_TAGS)
            .map(|json| TagStore::from_json(&json))
            .unwrap_or_else(TagStore::new)
    }

    /// 把整本标注写回 kv。
    ///
    /// 不需要显式 `flush`：kv 单写线程自己会去抖落盘，退出时 `on_app_quit` 也会兜一次
    /// （与连接档案同一条路，见 `persist_connections`）。
    pub(super) fn persist_tags(&self, cx: &App) {
        match self.tags.to_json() {
            Ok(json) => duck_store::kv_put(cx, KEY_TAGS, json),
            // 序列化 `BTreeMap<String, …>` 理论上不会失败；真失败了也只丢这一次写入
            // （内存里的标注还在），不该把界面带下去。
            Err(error) => tracing::warn!(%error, "serialize redis key tags failed"),
        }
    }

    /// 染色之后的三件事：摘空壳、落盘、重算视图。
    ///
    /// ⚠️ 顺序不能反：先摘空壳再落盘，否则「标了又清」的那一条会以空表的形式
    /// 留在盘上，下次读回来又是一份空壳。
    fn after_tags_changed(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.tags.drop_if_empty(&id);
        self.persist_tags(cx);
        // 颜色参与文件夹聚合，必须重算；备注不走这条路（见模块文档）。
        self.rebuild_views();
        cx.notify();
    }

    /// 给一个键换标签色（`None` = 取消标色）。**备注原样留着**。
    pub(super) fn set_key_tag(&mut self, key: &str, tag: Option<TagColor>, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.tags.get_mut(&id).set_tag(key, tag);
        self.after_tags_changed(cx);
    }

    /// 改一个键的备注。**标签原样留着**。
    ///
    /// 只有「有没有备注」这个**布尔**翻转时才重算视图：树行的备注徽记是缓存字段，
    /// 空↔非空要刷新；而备注正文本身不进树行，打一个字就重排十万个键是不可接受的。
    pub(super) fn set_key_note(&mut self, key: &str, note: &str, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let had_note = !self.active_tags().note_of(key).is_empty();
        self.tags.get_mut(&id).set_note(key, note);
        // 备注也会让「标注表」从空变非空 / 从非空变空，同样要摘空壳。
        self.tags.drop_if_empty(&id);
        self.persist_tags(cx);
        if had_note != !note.is_empty() {
            self.rebuild_views();
        }
        cx.notify();
    }

    /// 切换一个键的收藏。
    pub(super) fn toggle_key_favorite(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.tags.get_mut(&id).toggle_favorite(key);
        self.after_tags_changed(cx);
    }

    /// 换标签档过滤（纯本地：只重算视图，零往返）。
    pub(super) fn set_tag_filter(&mut self, filter: TagFilter, cx: &mut Context<Self>) {
        if self.tag_filter == filter {
            return;
        }
        self.tag_filter = filter;
        self.rebuild_views();
        cx.notify();
    }

    /// 键被删掉时，把它的本地痕迹一并丢掉。
    ///
    /// 只在**用户显式删键**时调用：服务端上那个键已经没了，留着标注只会让将来
    /// 同名的另一个键继承一份说不清来源的颜色。翻页 / 重扫**不**清
    /// （SCAN 只覆盖已加载的那一批，按它裁剪会把别的页上的标注整片抹掉）。
    pub(super) fn forget_key_tags(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let Some(tags) = self.tags.get(&id) else {
            return;
        };
        if tags.get(key).is_none() && !tags.is_favorite(key) {
            return;
        }
        self.tags.get_mut(&id).forget(key);
        self.tags.drop_if_empty(&id);
        self.persist_tags(cx);
        cx.notify();
    }

    /// 连接被删掉时，连同它的标注一起丢（否则盘上会积一堆再也点不到的连接）。
    pub(super) fn forget_connection_tags(&mut self, connection: &str, cx: &Context<Self>) {
        if self.tags.get(connection).is_none() {
            return;
        }
        self.tags.forget_connection(connection);
        self.persist_tags(cx);
    }
}

/// 标签色的展示文案。
///
/// 参数里没有 `&self`：渲染闭包（色块行、过滤菜单）拿不到页面，全走这个自由函数。
/// 与 [`tag_filter_text`] 一样，**文案留在界面层**——内核只给 `as_str()` 那种
/// 稳定标识，不吐成品中文。
pub(super) fn tag_color_text(color: TagColor, cx: &App) -> &'static str {
    match color {
        TagColor::Red => tr(cx, "红"),
        TagColor::Orange => tr(cx, "橙"),
        TagColor::Yellow => tr(cx, "黄"),
        TagColor::Green => tr(cx, "绿"),
        TagColor::Blue => tr(cx, "蓝"),
        TagColor::Purple => tr(cx, "紫"),
    }
}

/// 标签档过滤的展示文案。
pub(super) fn tag_filter_text(filter: TagFilter, cx: &App) -> String {
    match filter {
        TagFilter::All => tr(cx, "标签：全部").to_owned(),
        TagFilter::Tagged => tr(cx, "标签：已标").to_owned(),
        TagFilter::Untagged => tr(cx, "标签：未标").to_owned(),
        TagFilter::Favorites => tr(cx, "标签：收藏").to_owned(),
        // 颜色档带上色名，否则菜单里六个「标签」分不清谁是谁。
        TagFilter::Color(color) => trf(cx, "标签：{}", &[tag_color_text(color, cx)]),
    }
}

/// 标签色 → 具体颜色（明暗各一套，与 `key_type_color` 同法）。
///
/// 六个色相在两套主题上都要能分辨。取值刻意**压没到中低亮度**：色条只有 3px 宽，
/// 高饱和的亮色在浅色底上会糊成一条白线，而在深色底上又会刺眼。
///
/// 不改用主题语义色（`status_failed` 一类）：那是一套「状态」语义，六个状态色
/// 各自有既定含义，借来当标签用会让「红色」同时意味着「失败」与「用户标的红」。
pub(super) fn tag_color(color: TagColor, dark: bool) -> Hsla {
    let (light, dark_value) = match color {
        TagColor::Red => (0xb23838, 0xd07070),
        TagColor::Orange => (0xa9621f, 0xd3924a),
        TagColor::Yellow => (0x8a7a12, 0xc9b23f),
        TagColor::Green => (0x2f7d32, 0x66ab69),
        TagColor::Blue => (0x2f5fa8, 0x6e93cf),
        TagColor::Purple => (0x7a3fa8, 0xa87cc9),
    };
    duck_theme::color_from_rgb(if dark { dark_value } else { light })
}
