//! 工具注册表：全局单例 + 纯逻辑索引。
//!
//! 拆成两层，与 `duck-store` 的取舍一致：
//! - [`ToolIndex`]：**纯逻辑**，不碰 gpui，可在普通 `#[test]` 里直接跑（去重、拼音派生、筛选）；
//! - [`ToolRegistry`]：gpui 全局包装，把索引与「启用状态 / 开发者模式 / 最近使用」的持久化接起来。
//!
//! 注册时同时派生拼音索引（全拼 / 首字母）并缓存，搜索不必每次重算。

use gpui::{App, Global, Window};

use crate::category::ToolCategory;
use crate::meta::{ToolMeta, ToolRun};
use crate::pinyin::{initials_of, plain_of};
use crate::settings;

/// 开发者模式开关的存储键（`kv` 表）。
const DEV_MODE_KEY: &str = "dev_mode";
/// 收藏夹的存储键（`kv` 表）：一行一个工具 id，**行序即侧栏顺序**。
///
/// 用换行分隔而不是 JSON：id 里不可能有换行，读回来零解析成本、也不给
/// `duck-tools` 添 serde 依赖（本 crate 的其余持久化都走 `duck-store` 的现成表）。
const FAVORITES_KEY: &str = "tool_favorites";
/// 「最近使用」的类别名（`recent` 表）。
const RECENT_KIND: &str = "tool";
/// 一天的秒数（「最近 N 天」换算成 `recent_since` 要的秒数）。
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// 注册表中的一条工具记录：元数据 + 派生索引。
pub struct RegisteredTool {
    meta: ToolMeta,
    plain: String,
    initials: String,
}

impl RegisteredTool {
    /// 元数据。
    #[must_use]
    pub fn meta(&self) -> &ToolMeta {
        &self.meta
    }

    /// 稳定 id（等价于 `meta().id()`，供 `ElementId` 等高频场景使用）。
    #[must_use]
    pub fn id(&self) -> &'static str {
        self.meta.id()
    }

    /// 自动派生的全拼索引。
    #[must_use]
    pub fn plain(&self) -> &str {
        &self.plain
    }

    /// 自动派生的首字母索引。
    #[must_use]
    pub fn initials(&self) -> &str {
        &self.initials
    }
}

/// 是否进入列表：`dev` 工具仅在开发者模式下列出。
///
/// 与 [`is_tool_visible`] 的区别：**停用的工具仍然要列出来**（列表里有「状态」列显示「已停用」），
/// 只有 `dev` 工具是彻底不出现。
#[must_use]
pub const fn is_tool_listed(is_dev: bool, dev_mode: bool) -> bool {
    !is_dev || dev_mode
}

/// 是否可用：已启用，且已列出。
#[must_use]
pub const fn is_tool_visible(enabled: bool, is_dev: bool, dev_mode: bool) -> bool {
    enabled && is_tool_listed(is_dev, dev_mode)
}

/// 纯逻辑索引：登记、去重、拼音派生、分类筛选。
#[derive(Default)]
pub struct ToolIndex {
    tools: Vec<RegisteredTool>,
}

impl ToolIndex {
    /// 登记一个工具；id 重复时返回 `false` 且不覆盖已有记录。
    pub fn insert(&mut self, meta: ToolMeta) -> bool {
        if self.tools.iter().any(|tool| tool.id() == meta.id()) {
            return false;
        }
        let plain = plain_of(meta.name());
        let initials = initials_of(meta.name());
        self.tools.push(RegisteredTool {
            meta,
            plain,
            initials,
        });
        true
    }

    /// 全部已登记工具（注册顺序）。
    #[must_use]
    pub fn all(&self) -> &[RegisteredTool] {
        &self.tools
    }

    /// 注销一个工具；返回是否确有删除（不存在时 `false`）。
    ///
    /// 插件卸载时用它清掉该插件贡献的工具——否则注册表里会留下一个点不动的孤儿项。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tools.len();
        self.tools.retain(|tool| tool.id() != id);
        self.tools.len() != before
    }

    /// 按 id 取一条。
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&RegisteredTool> {
        self.tools.iter().find(|tool| tool.id() == id)
    }

    /// 按谓词筛选，保持注册顺序。
    #[must_use]
    pub fn filter(&self, mut keep: impl FnMut(&RegisteredTool) -> bool) -> Vec<&RegisteredTool> {
        self.tools.iter().filter(|tool| keep(tool)).collect()
    }

    /// 当前有可见工具的分类（按 [`ToolCategory::ALL`] 顺序）。
    #[must_use]
    pub fn categories_of(&self, tools: &[&RegisteredTool]) -> Vec<ToolCategory> {
        ToolCategory::ALL
            .iter()
            .copied()
            .filter(|category| tools.iter().any(|tool| tool.meta().category() == *category))
            .collect()
    }
}

/// 工具注册表（gpui 全局）。
#[derive(Default)]
pub struct ToolRegistry {
    index: ToolIndex,
}

impl Global for ToolRegistry {}

impl ToolRegistry {
    /// 建立空注册表（在创建任何视图之前调用一次）。
    pub fn install(cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
    }

    /// 登记一个工具；id 重复时记警告并忽略后一条。
    pub fn register(cx: &mut App, meta: ToolMeta) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let id = meta.id();
        if !cx.global_mut::<Self>().index.insert(meta) {
            tracing::warn!(id, "duplicate tool registration, ignoring the later one");
        }
    }

    /// 注销一个工具；返回是否确有删除。
    ///
    /// 与 [`register`](Self::register) 相对，供插件卸载等运行时摘除贡献用。只动注册表
    /// 本身：工具级的启用状态仍留在 KV 里，重装后照旧生效。
    pub fn unregister(cx: &mut App, id: &str) -> bool {
        if !cx.has_global::<Self>() {
            return false;
        }
        cx.global_mut::<Self>().index.remove(id)
    }

    /// 全部已登记工具（含不可见项）。
    #[must_use]
    pub fn all(cx: &App) -> &[RegisteredTool] {
        cx.try_global::<Self>()
            .map_or(&[], |registry| registry.index.all())
    }

    /// 按 id 取一条。
    #[must_use]
    pub fn get<'a>(cx: &'a App, id: &str) -> Option<&'a RegisteredTool> {
        cx.try_global::<Self>()
            .and_then(|registry| registry.index.get(id))
    }

    /// 某个工具当前是否列出（无视启停，只管 `dev` 过滤）。
    #[must_use]
    pub fn is_listed(cx: &App, tool: &RegisteredTool) -> bool {
        is_tool_listed(tool.meta().is_dev(), Self::dev_mode(cx))
    }

    /// 列表用的工具集合：含「已停用」项（状态列会显示），但过滤掉未开开发者模式的 `dev` 工具。
    #[must_use]
    pub fn listed(cx: &App) -> Vec<&RegisteredTool> {
        Self::all(cx)
            .iter()
            .filter(|tool| Self::is_listed(cx, tool))
            .collect()
    }

    /// 某个工具当前是否可用（已启用且已列出）。
    #[must_use]
    pub fn is_visible(cx: &App, tool: &RegisteredTool) -> bool {
        is_tool_visible(
            settings::is_enabled(cx, tool.id()),
            tool.meta().is_dev(),
            Self::dev_mode(cx),
        )
    }

    /// 可用工具列表（已启用且已列出）。
    #[must_use]
    pub fn visible(cx: &App) -> Vec<&RegisteredTool> {
        Self::all(cx)
            .iter()
            .filter(|tool| Self::is_visible(cx, tool))
            .collect()
    }

    /// 列表中属于某分类的子集；`None` 表示「全部工具」。
    #[must_use]
    pub fn in_category(cx: &App, category: Option<ToolCategory>) -> Vec<&RegisteredTool> {
        Self::listed(cx)
            .into_iter()
            .filter(|tool| category.is_none_or(|want| tool.meta().category() == want))
            .collect()
    }

    /// 当前有工具的分类（按 [`ToolCategory::ALL`] 顺序）。
    #[must_use]
    pub fn present_categories(cx: &App) -> Vec<ToolCategory> {
        let listed = Self::listed(cx);
        cx.try_global::<Self>()
            .map_or_else(Vec::new, |registry| registry.index.categories_of(&listed))
    }

    /// 记一次使用：累加使用次数并写入「最近使用」。
    pub fn record_use(cx: &App, id: &str) {
        settings::bump_runs(cx, id);
        Self::mark_used(cx, id);
    }

    /// 记一次使用（只写 `recent` 表，不动计数）。
    pub fn mark_used(cx: &App, id: &str) {
        if let Some(store) = duck_store::Store::global(cx) {
            store.touch_recent(RECENT_KIND, id);
        }
    }

    /// 最近使用的工具（按时间倒序，最多 `limit` 条）。
    #[must_use]
    pub fn recent(cx: &App, limit: usize) -> Vec<&RegisteredTool> {
        let Some(store) = duck_store::Store::global(cx) else {
            return Vec::new();
        };
        store
            .recent(RECENT_KIND, limit)
            .iter()
            .filter_map(|id| Self::get(cx, id))
            .collect()
    }

    /// 最近 `days` 天内用过的工具（按时间倒序，已注册的才算）。
    ///
    /// 「用过」以 [`Self::touch_recent`] 写下的那座时间戳为准 —— 它是**每工具一条**，
    /// 所以这里数的是「这段时间里动过的不同工具数」，不是调用次数（见
    /// [`duck_store::Store::recent_since`] 的说明）。
    #[must_use]
    pub fn recent_within_days(cx: &App, days: i64) -> Vec<&RegisteredTool> {
        let Some(store) = duck_store::Store::global(cx) else {
            return Vec::new();
        };
        store
            .recent_since(RECENT_KIND, days * SECONDS_PER_DAY)
            .iter()
            .filter_map(|id| Self::get(cx, id))
            .collect()
    }

    /// 收藏的工具（**按收藏顺序**；已注销的、被 dev 模式过滤掉的自动跳过）。
    ///
    /// 侧栏「收藏夹」分组用它。参考项目里收藏夹是一等公民（排在所有分类之前），
    /// 这里保持同样的语义：收藏不改变工具所属分类，只是多一个快捷入口。
    #[must_use]
    pub fn favorites(cx: &App) -> Vec<&RegisteredTool> {
        Self::favorite_ids(cx)
            .iter()
            .filter_map(|id| Self::get(cx, id))
            .filter(|tool| Self::is_listed(cx, tool))
            .collect()
    }

    /// 某个工具是否已收藏。
    #[must_use]
    pub fn is_favorite(cx: &App, id: &str) -> bool {
        Self::favorite_ids(cx).iter().any(|saved| saved == id)
    }

    /// 切换收藏状态；返回**切换后**是否处于收藏。
    pub fn toggle_favorite(cx: &App, id: &str) -> bool {
        let mut ids = Self::favorite_ids(cx);
        let favorited = match ids.iter().position(|saved| saved == id) {
            Some(index) => {
                ids.remove(index);
                false
            }
            None => {
                ids.push(id.to_string());
                true
            }
        };
        duck_store::kv_put(cx, FAVORITES_KEY, ids.join("\n"));
        favorited
    }

    /// 已收藏的 id 列表（按收藏顺序）。
    ///
    /// 每次读一遍 KV：单个工具的收藏切换频率极低（人手点的），而 KV 读是内存里
    /// 的一次 `SELECT`，比在 `ToolRegistry` 里再挂一份缓存（还要处理多窗口同步）
    /// 便宜得多。
    fn favorite_ids(cx: &App) -> Vec<String> {
        duck_store::kv_get(cx, FAVORITES_KEY)
            .map(|raw| {
                raw.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 取工具的执行入口；`None` 表示未注册或后端未接入。
    #[must_use]
    pub fn run(cx: &App, id: &str) -> Option<ToolRun> {
        Self::get(cx, id)?.meta().run()
    }

    /// 执行工具：先记一次使用，再调用执行入口。
    ///
    /// 返回 `false` 表示未注册或后端未接入（调用方可以据此提示）。
    /// **不检查启用状态**——是否允许执行由调用方决定（停用工具的入口在 UI 上本来就不该出现）。
    pub fn invoke(id: &str, window: &mut Window, cx: &mut App) -> bool {
        let Some(run) = Self::run(cx, id) else {
            return false;
        };
        Self::record_use(cx, id);
        run(id, window, cx);
        true
    }

    /// 开发者模式是否开启。
    #[must_use]
    pub fn dev_mode(cx: &App) -> bool {
        duck_store::kv_get(cx, DEV_MODE_KEY).is_some_and(|value| value.eq_ignore_ascii_case("true"))
    }

    /// 设置开发者模式。
    pub fn set_dev_mode(cx: &App, on: bool) {
        duck_store::kv_put(cx, DEV_MODE_KEY, if on { "true" } else { "false" });
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolIndex, is_tool_listed, is_tool_visible};
    use crate::category::ToolCategory;
    use crate::meta::ToolMeta;
    use duck_components::DuckIcon;

    fn index_with_two() -> ToolIndex {
        let mut index = ToolIndex::default();
        index.insert(
            ToolMeta::builder("demo.media", "视频统一解析", DuckIcon::Film)
                .category(ToolCategory::Media)
                .build(),
        );
        index.insert(
            ToolMeta::builder("demo.dev", "命令片段", DuckIcon::SquareTerminal)
                .category(ToolCategory::Development)
                .dev()
                .build(),
        );
        index
    }

    #[test]
    fn derives_pinyin_index_on_insert() {
        let index = index_with_two();
        let tool = index.get("demo.media").expect("应已登记");
        assert_eq!(tool.plain(), "shipintongyijiexi");
        assert_eq!(tool.initials(), "sptyjx");
        let dev = index.get("demo.dev").expect("应已登记");
        assert_eq!(dev.plain(), "minglingpianduan");
    }

    #[test]
    fn duplicate_id_is_rejected_without_overwrite() {
        let mut index = index_with_two();
        let accepted = index.insert(ToolMeta::builder("demo.media", "重复", DuckIcon::Box).build());
        assert!(!accepted, "重复 id 必须被拒绝");
        assert_eq!(index.all().len(), 2);
        assert_eq!(
            index.get("demo.media").expect("仍在").meta().name(),
            "视频统一解析"
        );
    }

    #[test]
    fn visibility_rule_hides_dev_tools() {
        assert!(is_tool_visible(true, false, false), "普通工具默认可见");
        assert!(!is_tool_visible(true, true, false), "开发者工具默认隐藏");
        assert!(is_tool_visible(true, true, true), "开开发者模式后可见");
        assert!(!is_tool_visible(false, false, true), "停用的一律不可见");
    }

    #[test]
    fn listing_keeps_disabled_tools() {
        // 停用只是状态，仍要出现在列表里（状态列显示「已停用」）。
        assert!(is_tool_listed(false, false), "停用的普通工具仍应列出");
        assert!(is_tool_listed(false, true), "开开发者模式时停用工具也列出");
        assert!(!is_tool_listed(true, false), "开发者工具未开模式时不列出");
    }

    #[test]
    fn remove_drops_exactly_the_named_tool() {
        let mut index = index_with_two();
        assert!(index.remove("demo.media"), "存在的工具应删除成功");
        assert!(index.get("demo.media").is_none());
        assert_eq!(index.all().len(), 1);
        assert!(!index.remove("demo.media"), "重复注销返回 false");
        assert!(index.get("demo.dev").is_some(), "其它工具不受影响");
    }

    #[test]
    fn filters_by_category_preserving_order() {
        let index = index_with_two();
        let media = index.filter(|tool| tool.meta().category() == ToolCategory::Media);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].id(), "demo.media");
        // 顺序跟随 `ToolCategory::ALL`（侧栏顺序），不是插入顺序：
        // `ALL` 已按参考项目的分类顺序排列（代码开发在前，内置的媒体等在后）。
        assert_eq!(
            index.categories_of(&index.all().iter().collect::<Vec<_>>()),
            vec![ToolCategory::Development, ToolCategory::Media]
        );
    }
}
