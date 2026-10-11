//! 插件市场：远端索引的拉取与缓存、安装与卸载，以及合并后的展示快照。
//!
//! 分工：本机「有什么插件」在 [`crate::plugins`]（目录发现），本模块管「远端有什么」。
//! 两者的合并结果 [`MarketEntry`] 才是插件中心渲染用的东西——每个条目都带
//! [`PluginStatus`]，插件中心的「全部 / 已安装 / 未安装 / 可更新」筛选就是按它分的。
//!
//! 线程边界与更新器一致：HTTP、哈希、解包全跑在 gpui 的后台执行器上，主线程只负责
//! 收结果、改全局状态、刷窗口。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use duck_components::notify;
use duck_plugin_registry::{
    Flow, PluginStatus, RegistryEntry, RegistryIndex, classify, fetch_index, parse_index,
};
use duck_theme::{tr, trf};
use gpui::{App, Global, SharedString, Task};

use crate::plugins::{self, PluginEntry};

/// 市场源地址的 kv 键。
const KEY_SOURCE: &str = "plugin_market.source_url";
/// 上一次成功拉取的原始索引字节（离线也能列出来）。
const KEY_CACHE: &str = "plugin_market.index_cache";
/// 上一次成功拉取的时间戳（Unix 秒，TTL 判定用）。
const KEY_FETCHED_AT: &str = "plugin_market.fetched_at";

/// 缺省市场源：本仓库 `plugin-index` 分支根上的索引。
///
/// 为什么是**分支 + raw** 而不是 `releases/latest/download/…`：那个地址已经被应用
/// 自身的更新器占着（[`crate::update`]），而 GitHub 的 `latest` 取的是「最近一个
/// 非预发布 Release」——插件上架只要跟着发一次 Release，就会把应用的更新源顶掉。
/// 生成物分支与应用发版完全隔离，且整条分支（索引 + `packages/`）可以原样镜像。
///
/// 客户端「市场源」可在界面上改，这里只是出厂默认值。
pub const DEFAULT_SOURCE: &str =
    "https://raw.githubusercontent.com/HanoCode/gpui-toolbox/plugin-index/plugin-index.json";

/// 自动刷新间隔；手动「刷新」不受它限制。
const TTL_SECONDS: u64 = 6 * 3600;

/// 提示去重键：拉取结论只留最新一条。
const KEY_TOAST_MARKET: &str = "plugin_market.toast.market";
/// 提示去重键：安装 / 卸载结论只留最新一条。
const KEY_TOAST_ACTION: &str = "plugin_market.toast.action";

/// 远端索引的取用阶段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 还没结果（首次启动、无缓存）。
    Idle,
    /// 正在取索引。
    Loading,
    /// 有一份可用索引（可能来自缓存）。
    Ready,
    /// 取索引失败（上一次的缓存仍在用）。
    Failed(String),
}

impl Phase {
    /// 是不是正在取。
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

/// 插件市场的应用级状态。
pub struct PluginMarket {
    source: SharedString,
    phase: Phase,
    index: Option<RegistryIndex>,
    /// 上次成功拉取的 Unix 秒；取自缓存的也算。
    fetched_at: Option<u64>,
    /// 正在安装 / 卸载的插件 id（一次一个：装的时候按 id 排他）。
    busy: Option<String>,
    /// 正在跑的拉取 / 安装任务，持有以延长生命周期。
    task: Option<Task<()>>,
}

impl Global for PluginMarket {}

/// 建立市场状态：读源地址与上一次的索引缓存（离线可用）。
pub fn init(cx: &mut App) {
    let source = duck_store::kv_get(cx, KEY_SOURCE)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
    let index = duck_store::kv_get(cx, KEY_CACHE).and_then(|raw| parse_index(raw.as_bytes()).ok());
    let fetched_at = duck_store::kv_get(cx, KEY_FETCHED_AT).and_then(|raw| raw.parse::<u64>().ok());
    let phase = if index.is_some() {
        Phase::Ready
    } else {
        Phase::Idle
    };

    cx.set_global(PluginMarket {
        source: source.into(),
        phase,
        index,
        fetched_at,
        busy: None,
        task: None,
    });
}

fn market(cx: &App) -> Option<&PluginMarket> {
    cx.try_global::<PluginMarket>()
}

/// 当前市场源地址。
#[must_use]
pub fn source(cx: &App) -> SharedString {
    market(cx).map_or_else(|| SharedString::from(DEFAULT_SOURCE), |m| m.source.clone())
}

/// 改写市场源并落盘；换源后清掉缓存索引（旧源的条目不该继续展示）。
pub fn set_source(cx: &mut App, value: String) {
    let value = if value.trim().is_empty() {
        DEFAULT_SOURCE.to_string()
    } else {
        value
    };
    {
        let Some(state) = cx.try_global::<PluginMarket>() else {
            return;
        };
        if state.source.as_ref() == value {
            return;
        }
    }
    let state = cx.global_mut::<PluginMarket>();
    state.source = value.clone().into();
    state.index = None;
    state.fetched_at = None;
    state.phase = Phase::Idle;

    duck_store::kv_put(cx, KEY_SOURCE, value);
    // 存储层没有「删键」：写空串即可，读取处一律把空串当作没有。
    duck_store::kv_put(cx, KEY_CACHE, "");
    duck_store::kv_put(cx, KEY_FETCHED_AT, "");
    cx.refresh_windows();
}

/// 当前阶段。
#[must_use]
pub fn phase(cx: &App) -> Phase {
    market(cx).map_or(Phase::Idle, |m| m.phase.clone())
}

/// 上次成功拉取的时间（Unix 秒）。
#[must_use]
pub fn fetched_at(cx: &App) -> Option<u64> {
    market(cx).and_then(|m| m.fetched_at)
}

/// 远端索引里的全部条目（无缓存且未拉取过时为空）。
#[must_use]
pub fn entries(cx: &App) -> Vec<RegistryEntry> {
    market(cx)
        .and_then(|m| m.index.as_ref())
        .map_or_else(Vec::new, |index| index.plugins().to_vec())
}

/// 某个插件是否正在安装 / 卸载。
#[must_use]
pub fn is_busy(cx: &App, plugin_id: &str) -> bool {
    market(cx).and_then(|m| m.busy.as_deref()) == Some(plugin_id)
}

/// 索引是否陈旧（没拉过，或超过 TTL）。
#[must_use]
pub fn is_stale(cx: &App) -> bool {
    let now = now_seconds();
    market(cx)
        .and_then(|m| m.fetched_at)
        .is_none_or(|at| now.saturating_sub(at) > TTL_SECONDS)
}

/// 启动时的自动刷新：只在索引陈旧时动手，失败静默（不打扰刚打开应用的用户）。
pub fn refresh_if_stale(cx: &mut App) {
    if is_stale(cx) {
        spawn_fetch(cx, false);
    }
}

/// 手动刷新：无论是否陈旧都重新取一次，失败给一条常驻提示。
pub fn refresh(cx: &mut App) {
    if phase(cx).is_loading() {
        return;
    }
    spawn_fetch(cx, true);
}

/// 取一次索引。
fn spawn_fetch(cx: &mut App, announce: bool) {
    if cx.try_global::<PluginMarket>().is_none() {
        return;
    }
    let url = source(cx).to_string();
    set_phase(cx, Phase::Loading);

    let task = cx.spawn(async move |cx| {
        let fetched = cx
            .background_executor()
            .spawn(async move {
                fetch_index(&url).and_then(|bytes| parse_index(&bytes).map(|index| (bytes, index)))
            })
            .await;
        cx.update(|cx| apply_fetch(cx, fetched, announce));
    });
    store_task(cx, task);
}

/// 拉取结果落到状态上。
fn apply_fetch(
    cx: &mut App,
    fetched: Result<(Vec<u8>, RegistryIndex), duck_plugin_registry::RegistryError>,
    announce: bool,
) {
    match fetched {
        Ok((bytes, index)) => {
            let count = index.plugins().len();
            let now = now_seconds();
            {
                let state = cx.global_mut::<PluginMarket>();
                state.index = Some(index);
                state.fetched_at = Some(now);
                state.phase = Phase::Ready;
            }
            // 缓存原始字节：下次离线启动也能把「未安装」列表列出来。
            if let Ok(raw) = String::from_utf8(bytes) {
                duck_store::kv_put(cx, KEY_CACHE, raw);
            }
            duck_store::kv_put(cx, KEY_FETCHED_AT, now.to_string());
            tracing::info!(plugins = count, "plugin index refreshed");
            if announce {
                let text = trf(cx, "插件清单已更新，共 {} 个插件。", &[&count.to_string()]);
                toast(cx, KEY_TOAST_MARKET, text, notify::Tone::Success);
            }
            cx.refresh_windows();
        }
        Err(error) => {
            let detail = error.to_string();
            cx.global_mut::<PluginMarket>().phase = Phase::Failed(detail.clone());
            tracing::warn!(%detail, "plugin index refresh failed");
            if announce {
                let text = trf(cx, "获取插件清单失败：{}", &[&detail]);
                sticky(cx, KEY_TOAST_MARKET, text, notify::Tone::Error);
            }
            cx.refresh_windows();
        }
    }
}

/// 安装（或更新）一个插件。
pub fn install_plugin(cx: &mut App, plugin_id: &str) {
    if is_busy(cx, plugin_id) {
        return;
    }
    let Some(entry) = entries(cx).into_iter().find(|e| e.id() == plugin_id) else {
        return;
    };
    let plugins_dir = plugins::plugins_dir();
    let cache_dir = duck_platform::cache_dir().join("plugin-downloads");
    let id = plugin_id.to_string();
    let name = entry.name().to_string();

    set_busy(cx, Some(id.clone()));
    let task = cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move {
                duck_plugin_registry::install(&entry, &plugins_dir, &cache_dir, &|_, _| {
                    Flow::Continue
                })
            })
            .await;
        cx.update(|cx| finish_install(cx, &id, &name, outcome));
    });
    store_task(cx, task);
}

fn finish_install(
    cx: &mut App,
    plugin_id: &str,
    name: &str,
    outcome: Result<duck_plugin_registry::Installed, duck_plugin_registry::RegistryError>,
) {
    set_busy(cx, None);
    match outcome {
        Ok(installed) => {
            let version = installed.version().to_string();
            // 重装即启用：上一个「已停用」的状态不该带到新装上。
            plugins::set_enabled(cx, plugin_id, true);
            plugins::rescan(cx);
            let text = trf(cx, "插件「{}」已安装（v{}）。", &[name, version.as_str()]);
            toast(cx, KEY_TOAST_ACTION, text, notify::Tone::Success);
        }
        Err(error) => {
            let detail = error.to_string();
            tracing::warn!(plugin = plugin_id, %detail, "plugin install failed");
            let text = trf(cx, "安装插件「{}」失败：{}", &[name, &detail]);
            sticky(cx, KEY_TOAST_ACTION, text, notify::Tone::Error);
        }
    }
    cx.refresh_windows();
}

/// 卸载一个插件。
///
/// 「卸载前是否询问」由设置项 [`crate::settings::confirm_uninstall`] 决定（缺省询问）——
/// 卸载会把插件目录真删掉，误点一次就得回市场重装；关掉它的人多半是批量清理，图快。
pub fn uninstall_plugin(cx: &mut App, plugin_id: &str) {
    if is_busy(cx, plugin_id) {
        return;
    }
    let plugins_dir = plugins::plugins_dir();
    let tool_ids = plugins::tool_ids_of(cx, plugin_id);
    let id = plugin_id.to_string();
    let name = plugins::manifest_of(cx, plugin_id)
        .map_or_else(|| plugin_id.to_string(), |manifest| manifest.name.clone());

    if crate::settings::confirm_uninstall(cx)
        && !duck_platform::confirm(
            tr(cx, "卸载插件？"),
            &trf(
                cx,
                "「{}」的插件目录会被删除，它贡献的工具也会一起消失。",
                &[&name],
            ),
        )
    {
        return;
    }

    set_busy(cx, Some(id.clone()));
    let task = cx.spawn(async move |cx| {
        let target = id.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move { duck_plugin_registry::uninstall(&target, &plugins_dir) })
            .await;
        cx.update(|cx| finish_uninstall(cx, &id, &name, &tool_ids, outcome));
    });
    store_task(cx, task);
}

fn finish_uninstall(
    cx: &mut App,
    plugin_id: &str,
    name: &str,
    tool_ids: &[String],
    outcome: Result<(), duck_plugin_registry::RegistryError>,
) {
    set_busy(cx, None);
    match outcome {
        Ok(()) => {
            // 先把贡献从注册表里摘掉，再重扫：否则侧栏会留下点不动的孤儿项。
            for tool_id in tool_ids {
                if !duck_tools::ToolRegistry::unregister(cx, tool_id) {
                    tracing::debug!(tool_id, "plugin tool was not registered; nothing to drop");
                }
            }
            plugins::set_enabled(cx, plugin_id, false);
            // 卸载即收回全部权限：确认框脚注承诺过，不删记录就会在重装时把旧授权
            // 静默带回来（用户以为卸掉就没了）。
            plugins::forget_granted(cx, plugin_id);
            plugins::rescan(cx);
            let text = trf(cx, "插件「{}」已卸载。", &[name]);
            toast(cx, KEY_TOAST_ACTION, text, notify::Tone::Success);
        }
        Err(error) => {
            let detail = error.to_string();
            tracing::warn!(plugin = plugin_id, %detail, "plugin uninstall failed");
            let text = trf(cx, "卸载插件「{}」失败：{}", &[name, &detail]);
            sticky(cx, KEY_TOAST_ACTION, text, notify::Tone::Error);
        }
    }
    cx.refresh_windows();
}

// ── 展示快照 ─────────────────────────────────────────────────────────────

/// 合并「本机已装」与「远端可用」之后的展示条目。
///
/// 字段是 `pub` 的，与 [`PluginEntry`] 一致：这是给界面读的纯数据快照。
pub struct MarketEntry {
    /// 插件 id；本地清单无效时为 `None`（此时只展示、不可操作）。
    pub id: Option<String>,
    /// 展示名。
    pub name: String,
    /// 一句话说明。
    pub description: String,
    /// 分类 id。
    pub category: String,
    /// 标签。
    pub tags: Vec<String>,
    /// 本机已装版本。
    pub installed_version: Option<String>,
    /// 远端可用版本。
    pub remote_version: Option<String>,
    /// 执行模型串（`js` / `wasm` / `process`）。
    pub model: Option<&'static str>,
    /// 这个插件声明要访问的宿主资源——**远端索引优先**。
    ///
    /// 未安装的插件本地没有清单可读，而确认框必须在下载之前就把权限摊给用户看，
    /// 所以这里取索引那份；已装且远端没有该条目时回落到本地清单那份。
    pub capabilities: duck_plugin::Capabilities,
    /// 本机已装清单里的能力声明（未安装时为 `None`）。
    ///
    /// 只用来算「本次更新新增了哪些权限」：更新时把整张表重念一遍会让人脱敏，
    /// 标出新加的那几项才有意义。
    pub installed_capabilities: Option<duck_plugin::Capabilities>,
    /// 作者（远端提供时才非空）。
    pub author: String,
    /// 主页（远端提供时才非空）。
    pub homepage: String,
    /// 工具贡献数（本地为主）。
    pub tools: usize,
    /// 本地是否启用。
    pub enabled: bool,
    /// 本地清单无效时的英文错误说明。
    pub error: Option<String>,
    /// 安装目录（本地已装时才有）。
    pub dir: Option<PathBuf>,
    /// 远端归档大小（未安装时用于展示体积）。
    pub archive_size: Option<u64>,
    /// 相对「本机 + 远端」的位置。
    pub status: PluginStatus,
}

/// 合并快照（按 id 升序；清单无效的条目排在前面）。
#[must_use]
pub fn view(cx: &App) -> Vec<MarketEntry> {
    let remote = entries(cx);
    let local = PluginEntry::entries(cx);
    // 先把「本机已装的 id」抄出来：下面两个循环一个消费 `local`、一个要反查它。
    let installed_ids: Vec<String> = local.iter().filter_map(|entry| entry.id.clone()).collect();
    let mut out: Vec<MarketEntry> = Vec::new();

    for entry in local {
        let Some(id) = entry.id.clone() else {
            out.push(MarketEntry {
                id: None,
                name: entry.name,
                description: String::new(),
                category: entry.category,
                tags: entry.tags,
                installed_version: None,
                remote_version: None,
                model: None,
                capabilities: duck_plugin::Capabilities::default(),
                installed_capabilities: None,
                author: String::new(),
                homepage: String::new(),
                tools: 0,
                enabled: false,
                error: entry.error,
                dir: None,
                archive_size: None,
                status: PluginStatus::Installed,
            });
            continue;
        };

        let remote_entry = remote.iter().find(|candidate| candidate.id() == id);
        let dir = plugins::dir_of(cx, &id).map(Path::to_path_buf);
        // 先抄一份本地声明：下面要用它算「新增权限」，还得把它原地放进快照。
        let installed_capabilities = entry.capabilities.clone();
        let capabilities = match remote_entry {
            Some(remote) => remote.capabilities().clone(),
            None => installed_capabilities.clone().unwrap_or_default(),
        };
        out.push(MarketEntry {
            name: prefer(&entry.name, remote_entry.map(RegistryEntry::name)),
            description: prefer(
                &entry.description,
                remote_entry.map(RegistryEntry::description),
            ),
            category: prefer(&entry.category, remote_entry.map(RegistryEntry::category)),
            tags: merged_tags(&entry.tags, remote_entry),
            installed_version: entry.version.clone(),
            remote_version: remote_entry.map(|r| r.version().to_string()),
            model: entry.model,
            capabilities,
            installed_capabilities,
            author: prefer(&entry.author, remote_entry.map(RegistryEntry::author)),
            homepage: prefer(&entry.homepage, remote_entry.map(RegistryEntry::homepage)),
            tools: entry.tools,
            enabled: entry.enabled,
            error: None,
            dir,
            archive_size: remote_entry.map(|r| r.archive().size()),
            status: classify(
                entry.version.as_deref(),
                remote_entry.map(RegistryEntry::version),
            ),
            id: Some(id),
        });
    }

    for entry in &remote {
        if installed_ids.iter().any(|id| id == entry.id()) {
            continue;
        }
        out.push(MarketEntry {
            id: Some(entry.id().to_string()),
            name: entry.name().to_string(),
            description: entry.description().to_string(),
            category: entry.category().to_string(),
            tags: entry.tags().to_vec(),
            installed_version: None,
            remote_version: Some(entry.version().to_string()),
            model: Some(plugins::model_key(entry.execution_model())),
            capabilities: entry.capabilities().clone(),
            installed_capabilities: None,
            author: entry.author().to_string(),
            homepage: entry.homepage().to_string(),
            tools: 0,
            enabled: false,
            error: None,
            dir: None,
            archive_size: Some(entry.archive().size()),
            status: PluginStatus::NotInstalled,
        });
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 本地有就用本地（已装的是既成事实），否则用远端的。
fn prefer(local: &str, remote: Option<&str>) -> String {
    if local.trim().is_empty() {
        remote.unwrap_or_default().to_string()
    } else {
        local.to_string()
    }
}

/// 本次安装 / 更新相对本机已装清单**新增**的权限。
///
/// 更新时把整张权限表重新念一遍等于没念——多念几次就没人看了。把「这次比上次多要了
/// 什么」单独挑出来，才是值得占用一次确认的那句话。未安装（`installed` 为 `None`）
/// 时不存在「新增」，返回空集：那时整张表都是新的。
///
/// 纯函数，与界面无关，便于把「取差集」这件事单独测住。
#[must_use]
pub fn added_capabilities(
    installed: Option<&duck_plugin::Capabilities>,
    advertised: &duck_plugin::Capabilities,
) -> std::collections::HashSet<duck_plugin::Capability> {
    let Some(installed) = installed else {
        return std::collections::HashSet::new();
    };
    let before = duck_plugin::Capability::set_of(installed);
    duck_plugin::Capability::set_of(advertised)
        .difference(&before)
        .cloned()
        .collect()
}

/// 标签并集：本地在前，远端补齐，顺序稳定。
fn merged_tags(local: &[String], remote: Option<&RegistryEntry>) -> Vec<String> {
    let mut tags: Vec<String> = local.to_vec();
    if let Some(remote) = remote {
        for tag in remote.tags() {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    tags
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs())
}

fn set_phase(cx: &mut App, phase: Phase) {
    if cx.try_global::<PluginMarket>().is_some() {
        cx.global_mut::<PluginMarket>().phase = phase;
        cx.refresh_windows();
    }
}

fn set_busy(cx: &mut App, plugin_id: Option<String>) {
    if cx.try_global::<PluginMarket>().is_some() {
        cx.global_mut::<PluginMarket>().busy = plugin_id;
        cx.refresh_windows();
    }
}

fn store_task(cx: &mut App, task: Task<()>) {
    cx.global_mut::<PluginMarket>().task = Some(task);
}

/// 推一条可替换的应用内提示；没有可用窗口时静默丢弃。
fn sticky(cx: &mut App, key: &'static str, message: impl Into<SharedString>, tone: notify::Tone) {
    let message = message.into();
    crate::update::on_notify_window(cx, move |window, cx| {
        notify::sticky(key, tone, message, window, cx);
    });
}

fn toast(cx: &mut App, _key: &'static str, message: impl Into<SharedString>, tone: notify::Tone) {
    let message = message.into();
    crate::update::on_notify_window(cx, move |window, cx| match tone {
        notify::Tone::Info => notify::info(message, window, cx),
        notify::Tone::Success => notify::success(message, window, cx),
        notify::Tone::Warning => notify::warning(message, window, cx),
        notify::Tone::Error => notify::error(message, window, cx),
    });
}

/// 上次成功拉取距今多少秒（界面显示「x 分钟前」用）。
#[must_use]
pub fn age_seconds(cx: &App) -> Option<u64> {
    fetched_at(cx).map(|at| now_seconds().saturating_sub(at))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SOURCE, Phase, added_capabilities, prefer};
    use duck_plugin::Capabilities;

    #[test]
    fn only_loading_counts_as_loading() {
        assert!(Phase::Loading.is_loading());
        assert!(!Phase::Idle.is_loading());
        assert!(!Phase::Ready.is_loading());
        assert!(!Phase::Failed("x".into()).is_loading());
    }

    #[test]
    fn the_default_source_is_an_https_url_named_like_the_published_asset() {
        assert!(DEFAULT_SOURCE.starts_with("https://"));
        assert!(DEFAULT_SOURCE.ends_with("/plugin-index.json"));
    }

    /// 出厂源必须落在**生成物分支**上，不能碰 `releases/latest`。
    ///
    /// 那个地址是应用更新器的：GitHub 的 `latest` 指「最近一个非预发布 Release」，
    /// 插件上架只要跟着发一次 Release，就会把应用的更新源顶到不含 `update.json`
    /// 的发布上。这个测试就是那道护栏——别为了一时的地址顺手改回去。
    #[test]
    fn the_default_source_does_not_ride_on_the_latest_release() {
        assert!(
            !DEFAULT_SOURCE.contains("/releases/"),
            "the plugin index must not share `releases/latest` with the updater"
        );
        assert!(DEFAULT_SOURCE.contains("/plugin-index/"));
    }

    #[test]
    fn local_metadata_wins_over_remote() {
        assert_eq!(prefer("本地名", Some("远端名")), "本地名");
        assert_eq!(prefer("", Some("远端名")), "远端名");
        assert_eq!(prefer("", None), "");
    }

    /// 首次安装没有「新增」可言：整张表都是新的，标了也等于没标。
    #[test]
    fn a_first_install_has_no_added_capabilities() {
        let advertised = Capabilities {
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            ..Capabilities::default()
        };
        assert!(added_capabilities(None, &advertised).is_empty());
    }

    /// 更新时只挑出真正多出来的那一项——这正是确认框要说人话的地方。
    #[test]
    fn an_update_reports_only_what_is_new() {
        let installed = Capabilities {
            notifications: true,
            filesystem_write: vec!["last-report.txt".into()],
            ..Capabilities::default()
        };
        let advertised = Capabilities {
            notifications: true,
            filesystem_write: vec!["last-report.txt".into()],
            network_hosts: vec!["exfil.example.com".into()],
            ..Capabilities::default()
        };

        let added = added_capabilities(Some(&installed), &advertised);
        assert_eq!(added.len(), 1);
        assert!(added.contains(&duck_plugin::Capability::NetworkHost(
            "exfil.example.com".into()
        )));
    }

    /// 权限被**收回**（新版本少要了）不是「新增」，不该惊动用户。
    #[test]
    fn a_shrinking_capability_set_reports_nothing_new() {
        let installed = Capabilities {
            notifications: true,
            process_exec: vec!["ffmpeg".into()],
            ..Capabilities::default()
        };
        let advertised = Capabilities {
            notifications: true,
            ..Capabilities::default()
        };
        assert!(added_capabilities(Some(&installed), &advertised).is_empty());
    }
}
