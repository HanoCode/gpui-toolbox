//! 插件运行时接线：发现 → 装配进 [`ToolRegistry`] → 启用状态。
//!
//! 分工（与设计文档一致，注册表逻辑全在 `duck-plugin`，本文件只做装配）：
//!
//! 1. 启动时扫描插件目录，产出 [`PluginIndex`] 存为 GPUI 全局
//!    [`PluginState`]。目录来源分两种形态：
//!    - **dev（debug 构建）**：优先加载仓库根的 `plugins/` 目录（源码即插件，
//!      改完 `main.js` 重启 `sbin/run.sh` 即生效，方便调试），再合并已安装
//!      目录；同一插件 id 仓库侧优先；
//!    - **打包后（release 构建）**：只加载已安装目录
//!      `app_data_dir()/plugins`，插件独立安装投放。
//! 2. 把每个清单有效的插件声明的**工具贡献**注册进 [`ToolRegistry`]——
//!    侧栏、搜索索引、工具级设置界面与智能粘贴路由全部由既有管线自动派生，
//!    本文件不写任何 UI；
//! 3. 插件级启用状态存 [`duck_store`] 的 KV 表（键 `plugin_state::<id>`）。
//!    工具快照处用 [`tool_enabled`] 与工具级启停做 AND，开关即时生效、
//!    且不覆盖用户对单个工具的启停选择。
//!
//! 执行模型：`js` 轨走 [`duck_plugin_js`] 的 QuickJS 沙箱真执行；
//! `wasm` / `process` 轨运行时未接入，落到「后端尚未接入」占位提示。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use duck_components::DuckIcon;
use duck_plugin::{Capabilities, Capability, DiscoveredPlugin, ExecutionModel, PluginIndex};
use duck_plugin_js::JsToolOutcome;
use duck_theme::{tr, trf};
use duck_tools::{ToolCategory, ToolMeta, ToolRegistry};
use gpui::{App, Global, Window};

use crate::tools::run_placeholder;

/// KV 键前缀：`plugin_state::<plugin_id>`，值 `"false"` 视为停用，缺省启用。
const STATE_KEY_PREFIX: &str = "plugin_state::";

/// KV 键前缀：`plugin_caps::<plugin_id>`，值是**用户授予的能力子集**，形状与清单里的
/// `[capabilities]` 完全一致（同一个 [`Capabilities`] 类型，见 `duck-plugin` 的说明）。
///
/// **没有这一行 = 按声明全给**：本功能落地之前的安装不会有这条记录，把它读成空集
/// 会把老插件的全部能力一夜之间收走（症状是「插件突然啥也不能干」）。
const CAPS_KEY_PREFIX: &str = "plugin_caps::";

/// 全局插件索引（发现快照，启动时构建一次）。
struct PluginState(PluginIndex);

impl Global for PluginState {}

/// 用户授予的能力记录（`plugin_id` → 授予子集）。
///
/// **必须放全局，不能每次现读 `kv`**：授权要在渲染与执行两条路上反复读，而 `kv_put`
/// 是 400ms 去抖写的——写完立刻 `kv_get` 拿到的是**旧值**，界面上就是「点了开关又弹回去」。
/// 通知档位已经栽过一次（见 `2026-10-11` 日志的 P2-6），这里照同一条纪律办：
/// 运行时值放全局，`kv` 只负责活过重启。
struct PluginGrants(HashMap<String, Capabilities>);

impl Global for PluginGrants {}

/// 插件安装目录：`app_data_dir()/plugins`（macOS 为
/// `~/Library/Application Support/DuckToolbox/plugins`）。
///
/// 打包后的正式安装位置：插件独立分发时投放到这里。
pub fn plugins_dir() -> std::path::PathBuf {
    duck_platform::app_data_dir().join("plugins")
}

/// 开发期插件目录：仓库根的 `plugins/`。
///
/// 仅 debug 构建返回 `Some`（`CARGO_MANIFEST_DIR` 在打包产物里指向构建机的
/// 路径，release 绝不能引用）；路径经 `canonicalize` 收敛，便于界面直接展示。
#[must_use]
pub fn dev_plugins_dir() -> Option<std::path::PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins")
        .canonicalize()
        .unwrap_or_else(|_| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins"));
    Some(dir)
}

/// 界面提示用的主插件目录：dev 构建是仓库 `plugins/`，否则是安装目录。
#[must_use]
pub fn primary_plugins_dir() -> std::path::PathBuf {
    dev_plugins_dir().unwrap_or_else(plugins_dir)
}

/// 读取全局索引（未初始化时 `None`——例如存储初始化之前的极早期渲染）。
pub fn index(cx: &App) -> Option<&PluginIndex> {
    cx.try_global::<PluginState>().map(|state| &state.0)
}

/// 按 id 取插件清单（含工具贡献全文），供插件中心详情页渲染。
#[must_use]
pub fn manifest_of<'a>(cx: &'a App, plugin_id: &str) -> Option<&'a duck_plugin::PluginManifest> {
    index(cx)?
        .plugins()
        .iter()
        .find(|plugin| plugin.manifest.as_ref().is_some_and(|m| m.id == plugin_id))?
        .manifest
        .as_ref()
}

/// 按 id 取插件所在目录，供详情页展示安装位置。
#[must_use]
pub fn dir_of<'a>(cx: &'a App, plugin_id: &str) -> Option<&'a std::path::Path> {
    index(cx)?
        .plugins()
        .iter()
        .find(|plugin| plugin.manifest.as_ref().is_some_and(|m| m.id == plugin_id))
        .map(|plugin| plugin.dir.as_path())
}

/// 执行模型的中文展示名：`js` → JS 脚本 / `wasm` → WASM 组件 / 其余 → 独立进程。
///
/// 设置「插件」页与插件中心的卡片、详情共用同一映射，避免多处各写一套 match。
#[must_use]
pub fn model_display(model: &str, cx: &App) -> &'static str {
    match model {
        "js" => tr(cx, "JS 脚本"),
        "wasm" => tr(cx, "WASM 组件"),
        _ => tr(cx, "独立进程"),
    }
}

/// 执行模型的稳定原始键（`js` / `wasm` / `process`）。
///
/// 与 [`model_display`] 分工：这个返回机器可比的键，供合并快照长期持有；
/// 那个返回随语言变化的中文文案，只在渲染时取。
#[must_use]
pub fn model_key(model: &str) -> &'static str {
    match model {
        "js" => "js",
        "wasm" => "wasm",
        _ => "process",
    }
}

/// 插件是否启用（缺省启用；KV 值 `"false"` 视为停用）。
#[must_use]
pub fn is_enabled(cx: &App, plugin_id: &str) -> bool {
    duck_store::kv_get(cx, &format!("{STATE_KEY_PREFIX}{plugin_id}"))
        .is_none_or(|value| value != "false")
}

/// 写插件启用状态。调用方负责 `cx.refresh_windows()` 与用户提示。
pub fn set_enabled(cx: &App, plugin_id: &str, enabled: bool) {
    duck_store::kv_put(
        cx,
        format!("{STATE_KEY_PREFIX}{plugin_id}"),
        if enabled { "true" } else { "false" },
    );
}

/// 从 `kv` 装入授予记录（启动期调一次，须在存储就绪之后）。
///
/// 读不懂的记录按「没有记录」处理（回落声明），但**要说出来**：静默当成空集会把该插件的
/// 全部能力收走，而症状只会以「插件突然啥也不能干」的形式出现，从界面上看不出原因。
pub fn load_grants(cx: &mut App) {
    let Some(store) = duck_store::Store::global(cx) else {
        return;
    };
    let records: HashMap<String, Capabilities> = store
        .kv_all()
        .into_iter()
        .filter_map(|(key, value)| {
            let plugin_id = key.strip_prefix(CAPS_KEY_PREFIX)?.to_owned();
            match serde_json::from_str::<Capabilities>(&value) {
                Ok(caps) => Some((plugin_id, caps)),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        plugin = %plugin_id,
                        "unreadable plugin capability grant ignored; falling back to the declared set"
                    );
                    None
                }
            }
        })
        .collect();
    cx.set_global(PluginGrants(records));
}

/// 授予概况：`(已授予单元数, 声明单元数)`。
///
/// 返回两个数字而不是拼好的字符串：文案要在渲染时经 `trf` 取词，搬进本层就没法跟语言走。
#[must_use]
pub fn grant_summary(cx: &App, plugin_id: &str, declared: &Capabilities) -> (usize, usize) {
    (
        Capability::set_of(&granted_capabilities(cx, plugin_id)).len(),
        Capability::set_of(declared).len(),
    )
}

/// 用户授予的能力：**已按清单声明收窄**，可直接拿去仲裁。
///
/// 缺省语义 = 声明全给——没有记录（从未确认过 / 老安装 / 记录损坏）时返回清单声明。
/// 有记录时也要与声明求交：索引与归档里的清单可能不一致，而「用户点过的子集」
/// 永远不该超出「插件声明的上限」。
#[must_use]
pub fn granted_capabilities(cx: &App, plugin_id: &str) -> Capabilities {
    let declared = manifest_of(cx, plugin_id).map(|manifest| manifest.capabilities.clone());
    let record = cx
        .try_global::<PluginGrants>()
        .and_then(|grants| grants.0.get(plugin_id).cloned());
    resolve_grant(declared, record)
}

/// [`granted_capabilities`] 的纯核：`(声明, 记录)` → 生效授予。
///
/// 单独拆出来是为了**能被测住**：`plugins.rs` 活在 bin crate 里，`tests/` 拿不到它，
/// 而这里的组合语义（尤其「没记录 = 全给」）恰恰是最容易写反、写反了又最难从界面上
/// 看出来的一处。
#[must_use]
fn resolve_grant(declared: Option<Capabilities>, record: Option<Capabilities>) -> Capabilities {
    match (declared, record) {
        (Some(declared), Some(record)) => declared.intersect(&record),
        (Some(declared), None) => declared,
        (None, Some(record)) => record,
        (None, None) => Capabilities::default(),
    }
}

/// 写入用户授予（运行时全局 + 去抖持久化）。
///
/// `allowed` 是**勾选后的能力单元集合**，`declared` 是插件声明的上限；本函数负责按声明
/// 顺序收窄回 [`Capabilities`]（顺序要留在原地：同一份结果既拿去仲裁，也拿去逐条渲染）。
///
/// 显式收 `declared` 而不是自己查清单：**首次安装时插件还没被发现**，此刻
/// [`manifest_of`] 返回 `None`，靠它兜底就会「点了同意却什么都没记下」。
pub fn set_granted(
    cx: &mut App,
    plugin_id: &str,
    declared: &Capabilities,
    allowed: &HashSet<Capability>,
) {
    let caps = declared.only(allowed);
    ensure_grants(cx);
    cx.global_mut::<PluginGrants>()
        .0
        .insert(plugin_id.to_owned(), caps.clone());
    match serde_json::to_string(&caps) {
        Ok(serialized) => {
            duck_store::kv_put(cx, format!("{CAPS_KEY_PREFIX}{plugin_id}"), serialized)
        }
        // 序列化失败不回滚运行时那份：本次会话照授权执行，重启后回落「按声明全给」，
        // 这比「界面上勾了却当场不给」更接近用户的意图。
        Err(error) => {
            tracing::warn!(%error, plugin = %plugin_id, "failed to persist capability grant")
        }
    }
}

/// 收回某个插件的全部授予（卸载时调）。删记录而不是写空集——空值读回来是 `Some("")`。
pub fn forget_granted(cx: &mut App, plugin_id: &str) {
    ensure_grants(cx);
    cx.global_mut::<PluginGrants>().0.remove(plugin_id);
    duck_store::kv_delete(cx, format!("{CAPS_KEY_PREFIX}{plugin_id}"));
}

/// 确保授予记录全局存在：单测与极早期调用（存储未就绪）都要能写，
/// 否则「用户点过的授权」会被静默丢掉。
fn ensure_grants(cx: &mut App) {
    if cx.try_global::<PluginGrants>().is_none() {
        cx.set_global(PluginGrants(HashMap::new()));
    }
}

/// 工具级可见性的插件侧分量：工具属于某个插件且该插件被停用 → `false`。
///
/// 与 [`duck_tools::settings::is_enabled`]（工具级启停）AND 后才是工具真实可见性。
#[must_use]
pub fn tool_enabled(cx: &App, tool_id: &str) -> bool {
    index(cx)
        .and_then(|index| index.plugin_for_tool(tool_id))
        .map_or(true, |plugin| {
            plugin.id().is_none_or(|id| is_enabled(cx, id))
        })
}

/// 插件数据目录：`app_data_dir()/plugin-data/<plugin_id>`。
///
/// 能力声明里的文件 glob 相对这个目录解析，也是 `duck.dataDir` 暴露给
/// 插件的值——每个插件一个独立目录，互不可见。
#[must_use]
pub fn plugin_data_dir(plugin_id: &str) -> std::path::PathBuf {
    duck_platform::app_data_dir()
        .join("plugin-data")
        .join(plugin_id)
}

/// 插件工具的统一执行入口。
///
/// - `js` 轨：QuickJS 沙箱执行 `main.js` 的 `handle(toolId, input)`，
///   产出经 toast 呈现（通知逐条、输出截断展示、日志进宿主日志）；
/// - 其余轨（`wasm` / `process`）运行时未接入，落到「后端尚未接入」占位。
fn run_plugin_tool(tool_id: &str, window: &mut Window, cx: &mut App) {
    let name = ToolRegistry::get(cx, tool_id).map_or("工具", |tool| tool.meta().name());
    let info = index(cx)
        .and_then(|plugin_index| plugin_index.plugin_for_tool(tool_id))
        .and_then(|plugin| {
            plugin
                .manifest
                .clone()
                .map(|manifest| (plugin.dir.clone(), manifest))
        });
    let Some((dir, manifest)) = info else {
        return;
    };
    if manifest.execution_model != ExecutionModel::Js {
        run_placeholder(tool_id, window, cx);
        return;
    }

    let data_dir = plugin_data_dir(&manifest.id);
    if let Err(error) = duck_platform::ensure_dir(&data_dir) {
        tracing::warn!(%error, "failed to create plugin data dir {}", data_dir.display());
    }

    // **放到后台线程去跑。** 沙箱里设了 10 秒的执行上限，同步跑在主线程上就等于
    // 「界面最长冻结 10 秒」：插件里一个死循环、一次慢读盘都算数，而这段时间用户
    // 连窗口都拖不动。结果回来之后再回主线程投递 —— 提示要窗口，窗口只有主线程有。
    let tool_id_for_run = tool_id.to_owned();
    // 日志要用一份自有副本：闭包是 `'static` 的，`&str` 形参活不到结果回来那一刻。
    let tool_id_for_log = tool_id.to_owned();
    // 真正生效的是「清单声明 ∩ 用户授予」——在派发前算好并随任务带走：
    // 后台闭包是 `'static` 的，读不到 `cx`。
    let granted = granted_capabilities(cx, &manifest.id);
    let execution = cx.background_executor().spawn(async move {
        duck_plugin_js::run_tool(&dir, &data_dir, &manifest, &granted, &tool_id_for_run, "")
    });
    cx.spawn(async move |cx| {
        let result = execution.await;
        cx.update(move |cx| {
            crate::update::on_notify_window(cx, move |window, cx| match result {
                Ok(outcome) => emit_outcome(name, outcome, window, cx),
                Err(error) => {
                    tracing::warn!(tool = %tool_id_for_log, %error, "plugin tool execution failed");
                    duck_components::notify::error(
                        trf(cx, "插件执行失败：{}", &[&error.to_string()]),
                        window,
                        cx,
                    );
                }
            });
        });
    })
    .detach();
}

/// 把一次执行的产出投影成用户可见的反馈。
fn emit_outcome(
    tool_name: &'static str,
    outcome: JsToolOutcome,
    window: &mut Window,
    cx: &mut App,
) {
    for message in &outcome.notifications {
        duck_components::notify::info(message.clone(), window, cx);
    }
    for log in &outcome.logs {
        tracing::info!(target: "duck_plugin", "{log}");
    }
    if outcome.output.is_empty() {
        if outcome.notifications.is_empty() {
            let name = tr(cx, tool_name);
            duck_components::notify::info(trf(cx, "「{}」执行完成", &[name]), window, cx);
        }
        return;
    }
    // 输出按展示截断；完整产出留给后续的工作区画布（P2）。
    let output: String = if outcome.output.chars().count() > 240 {
        let truncated: String = outcome.output.chars().take(240).collect();
        format!("{truncated}…")
    } else {
        outcome.output
    };
    duck_components::notify::success(output, window, cx);
}

/// 启动装配：发现插件并注册其工具贡献。
///
/// dev 构建同时扫描仓库 `plugins/`（优先）与安装目录；release 只扫安装目录。
/// 目录不存在时先创建（首次启动即有可投放位置）；注册**无条件**执行
/// （含已停用插件的工具），可见性由快照处的 AND 决定——这样开关切换
/// 无需重建注册表，也保留用户对单个工具的启停选择。
pub fn init(cx: &mut App) {
    // 授予记录要先于任何渲染与执行装好：缺了它，`granted_capabilities` 会一律回落到
    // 「按声明全给」，用户收回去的权限在重启后又活了。
    load_grants(cx);

    let dirs = scan_dirs();
    for dir in &dirs {
        if let Err(error) = duck_platform::ensure_dir(dir) {
            tracing::warn!(%error, "failed to create plugin dir {}", dir.display());
        }
    }

    let plugin_index = PluginIndex::discover_all(&dirs);
    let registered = register_tools(cx, &plugin_index);

    tracing::info!(
        discovered = plugin_index.plugins().len(),
        loaded = plugin_index.loaded_count(),
        tools = registered,
        dirs = %dirs.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>().join(", "),
        "plugin discovery finished"
    );
    cx.set_global(PluginState(plugin_index));
}

/// 重新发现并装配。安装 / 卸载插件之后调用，让新贡献立刻出现在侧栏与搜索里。
///
/// 注册是幂等的（同 id 只留第一条），所以这里直接重跑一遍注册；被卸载插件的工具
/// 由调用方先 [`ToolRegistry::unregister`](duck_tools::ToolRegistry::unregister) 摘掉。
pub fn rescan(cx: &mut App) {
    let dirs = scan_dirs();
    let plugin_index = PluginIndex::discover_all(&dirs);
    let registered = register_tools(cx, &plugin_index);
    tracing::info!(
        loaded = plugin_index.loaded_count(),
        tools = registered,
        "plugin rescan finished"
    );
    cx.set_global(PluginState(plugin_index));
    cx.refresh_windows();
}

/// 扫描目录清单：dev 构建把仓库目录排在前（同一 id 覆盖安装版），release 只有安装目录。
fn scan_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![plugins_dir()];
    if let Some(dev_dir) = dev_plugins_dir() {
        dirs.insert(0, dev_dir);
    }
    dirs
}

/// 把索引里每个清单有效的插件所声明的工具贡献登记进注册表；返回**本次新登记**的数量。
fn register_tools(cx: &mut App, plugin_index: &PluginIndex) -> usize {
    let mut registered = 0usize;
    for plugin in plugin_index.plugins() {
        let Some(manifest) = plugin.manifest.as_ref() else {
            continue; // 清单无效的条目留在索引里供设置页展示错误，不注册。
        };
        for tool in &manifest.tools {
            if ToolRegistry::get(cx, &tool.id).is_some() {
                // 注册表里已有同名工具：要么是上一次登记的结果，要么与内置工具撞名。
                continue;
            }
            ToolRegistry::register(
                cx,
                ToolMeta::builder(
                    leak_str(&tool.id),
                    leak_str(&tool.name),
                    icon_of(tool.icon.as_deref()),
                )
                .description(leak_str(&tool.description))
                .keywords(leak_keywords(&tool.keywords))
                // 工具贡献未写分类时回落插件级分类，再回落「其他」。
                .category(if tool.category.is_empty() {
                    category_of(&manifest.category)
                } else {
                    category_of(&tool.category)
                })
                .run(run_plugin_tool)
                .build(),
            );
            registered += 1;
        }
    }
    registered
}

/// 某个插件声明的全部工具 id（卸载时要从注册表里逐个摘掉）。
#[must_use]
pub fn tool_ids_of(cx: &App, plugin_id: &str) -> Vec<String> {
    index(cx)
        .and_then(|plugin_index| {
            plugin_index.plugins().iter().find(|plugin| {
                plugin
                    .manifest
                    .as_ref()
                    .is_some_and(|manifest| manifest.id == plugin_id)
            })
        })
        .and_then(|plugin| plugin.manifest.as_ref())
        .map(|manifest| manifest.tools.iter().map(|tool| tool.id.clone()).collect())
        .unwrap_or_default()
}

/// 插件贡献里的图标资源名 → [`DuckIcon`]（无法识别回落 `Box`）。
pub(crate) fn icon_of(name: Option<&str>) -> DuckIcon {
    name.and_then(|name| {
        DuckIcon::ALL.iter().copied().find(|icon| {
            let path = icon.asset_path();
            path.trim_start_matches("duck/icons/")
                .trim_end_matches(".svg")
                == name
        })
    })
    .unwrap_or(DuckIcon::Box)
}

/// 插件贡献的分类 id → [`ToolCategory`]（无法识别回落 `Other`）。
fn category_of(id: &str) -> ToolCategory {
    ToolCategory::ALL
        .iter()
        .copied()
        .find(|category| category.id() == id)
        .unwrap_or(ToolCategory::Other)
}

/// 清单字符串的生命周期是进程级（注册表字段为 `&'static str`），所以只能泄漏。
///
/// 但**不能每次扫描都漏一遍**：`rescan` 会在每次装 / 卸 / 刷新插件市场时跑（设置页的
/// 「重新扫描」、市场里装完一个插件都会触发），而扫描是不限次数的。没有缓存的话，
/// 单次量级虽小（每个工具几条短串），却是一条**无上界**的增长路径。
/// 用内容做键把「同一份清单」收敛成同一个引用：重复扫描是空操作，新内容才多漏一次。
fn leaked_strings() -> &'static Mutex<HashMap<String, &'static str>> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn leak_str(value: &str) -> &'static str {
    // 锁中毒只说明别的线程在持锁时 panic 过；这张表是纯缓存，直接取用是安全的。
    let mut cache = leaked_strings()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = cache.get(value) {
        return existing;
    }
    let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
    cache.insert(value.to_owned(), leaked);
    leaked
}

/// 关键词数组同样泄漏为 `&'static [&'static str]`；切片本身也按内容去重
/// （否则重复扫描仍会一次次漏掉那层指针数组）。
fn leaked_keyword_slices() -> &'static Mutex<HashMap<String, &'static [&'static str]>> {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static [&'static str]>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn leak_keywords(keywords: &[String]) -> &'static [&'static str] {
    if keywords.is_empty() {
        return &[];
    }
    // 用不可能出现在关键词里的分隔符拼键，避免 ["a b"] 与 ["a", "b"] 撞成同一个。
    let key = keywords.join("\u{1f}");
    let mut cache = leaked_keyword_slices()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = cache.get(&key) {
        return existing;
    }
    let leaked: Vec<&'static str> = keywords.iter().map(|keyword| leak_str(keyword)).collect();
    let leaked: &'static [&'static str] = Box::leak(leaked.into_boxed_slice());
    cache.insert(key, leaked);
    leaked
}

/// 设置页「插件」用的展示快照（摊平索引 + 启用状态，避免渲染期借 `App`）。
pub struct PluginEntry {
    /// 插件 id（清单无效时为 `None`，此时开关不渲染）。
    pub id: Option<String>,
    /// 展示名（清单无效时回落目录名）。
    pub name: String,
    /// 插件版本（清单无效时为 `None`）。
    pub version: Option<String>,
    /// 一句话说明（清单无效时为空串）。
    pub description: String,
    /// 插件级分类 id（`media` / `network` / …；清单无效时为空串）。
    pub category: String,
    /// 插件级标签（清单无效时为空表）。
    pub tags: Vec<String>,
    /// 作者署名（清单无效时为空串）。清单自带，因此离线也展示得出。
    pub author: String,
    /// 主页 / 源码地址（清单无效时为空串）。
    pub homepage: String,
    /// 清单无效时的错误说明（英文 `Display`）。
    pub error: Option<String>,
    /// 执行模型字符串（`js` / `wasm` / `process`）。
    pub model: Option<&'static str>,
    /// 能力声明（清单无效时为 `None`）。
    pub capabilities: Option<duck_plugin::Capabilities>,
    /// 工具贡献数。
    pub tools: usize,
    /// 当前是否启用（清单无效时无意义，恒 `false`）。
    pub enabled: bool,
}

impl PluginEntry {
    /// 从索引摊平一份快照（按索引已有的 id 升序）。
    pub fn entries(cx: &App) -> Vec<Self> {
        let Some(plugin_index) = index(cx) else {
            return Vec::new();
        };
        plugin_index
            .plugins()
            .iter()
            .map(|plugin| Self::of(plugin, cx))
            .collect()
    }

    fn of(plugin: &DiscoveredPlugin, cx: &App) -> Self {
        match plugin.manifest.as_ref() {
            Some(manifest) => Self {
                id: Some(manifest.id.clone()),
                name: manifest.name.clone(),
                version: Some(manifest.version.clone()),
                description: manifest.description.clone(),
                category: manifest.category.clone(),
                tags: manifest.tags.clone(),
                author: manifest.author.clone(),
                homepage: manifest.homepage.clone(),
                error: None,
                model: Some(manifest.execution_model.as_str()),
                capabilities: Some(manifest.capabilities.clone()),
                tools: manifest.tools.len(),
                enabled: is_enabled(cx, &manifest.id),
            },
            None => Self {
                id: None,
                name: plugin.dir_name().to_string(),
                version: None,
                description: String::new(),
                category: String::new(),
                tags: Vec::new(),
                author: String::new(),
                homepage: String::new(),
                error: plugin.error.clone(),
                model: None,
                capabilities: None,
                tools: 0,
                enabled: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_grant;
    use duck_plugin::Capabilities;

    fn declared() -> Capabilities {
        Capabilities {
            filesystem_read: vec!["data/*".into()],
            filesystem_write: vec!["out/*".into()],
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            process_exec: vec!["ffmpeg".into()],
        }
    }

    /// 没有记录（从未确认过 / 老安装 / 记录损坏）＝ **按声明全给**。
    ///
    /// 这条是整套缺省语义的地基：写反成「没记录就不给」，所有已装插件会在升级后
    /// 一夜之间全部哑火，而界面上只会显示「插件执行失败」。
    #[test]
    fn no_record_means_everything_declared_is_granted() {
        assert_eq!(resolve_grant(Some(declared()), None), declared());
    }

    /// 一份记录都没有、清单也读不到：空集，不 panic。
    #[test]
    fn nothing_known_grants_nothing() {
        assert_eq!(resolve_grant(None, None), Capabilities::default());
    }

    /// 用户收回过的能力不会自己回来：记录是子集就按子集给。
    #[test]
    fn a_narrower_record_wins() {
        let record = Capabilities {
            notifications: true,
            ..Capabilities::default()
        };
        assert_eq!(
            resolve_grant(Some(declared()), Some(record.clone())),
            record
        );
    }

    /// 记录里**多出来**的能力拿不到手：声明是上限，交集说了算。
    ///
    /// 注意这里的构造方式：贪心记录必须**保留声明原有的单元再追加外来的**。
    /// 若写成「把 `network_hosts` 整个换成 `exfil.example.com`」，交集会把声明自己那条
    /// 也一并丢掉（`intersect` 是逐单元求交，不是逐字段覆盖），结果反而是空——
    /// 那不是「越权被拦」，是「声明的那条也被顺手删了」，两者别混为一谈。
    #[test]
    fn a_greedy_record_cannot_exceed_the_declaration() {
        let greedy = Capabilities {
            network_hosts: vec!["api.example.com".into(), "exfil.example.com".into()],
            process_exec: vec!["ffmpeg".into(), "curl".into()],
            ..declared()
        };
        assert_eq!(
            resolve_grant(Some(declared()), Some(greedy)),
            declared(),
            "声明里的照给，外来的两条被拦在门外"
        );
    }

    /// 记录里出现声明没有的单元时，交集只砍该单元，**不连坐同字段的其它单元**。
    /// 这条同时把上面那条测试的构造前提钉死（改坏了会在这里现形）。
    #[test]
    fn an_unknown_unit_only_removes_itself() {
        let record = Capabilities {
            network_hosts: vec!["exfil.example.com".into()],
            ..declared()
        };
        let effective = resolve_grant(Some(declared()), Some(record));
        assert!(effective.network_hosts.is_empty(), "该单元确实被砍掉");
        assert_eq!(
            effective.filesystem_read,
            declared().filesystem_read,
            "但读 glob 与它无关，必须原样保留"
        );
        assert_eq!(effective.process_exec, declared().process_exec);
        assert!(effective.notifications);
    }

    /// 清单读不到（插件刚被删、索引还没刷新）时按记录走：此刻把记录也丢掉，
    /// 等于「一次目录抖动 = 权限全没」。
    #[test]
    fn a_record_survives_a_missing_manifest() {
        assert_eq!(resolve_grant(None, Some(declared())), declared());
    }

    /// 持久化形状必须与清单里的 `[capabilities]` 完全一致（同一个类型的 serde 形状），
    /// 否则升级后老记录读不回来，而失败是**静默**的（回落成「全给」）。
    #[test]
    fn the_stored_shape_round_trips() {
        let caps = declared();
        let serialized = serde_json::to_string(&caps).expect("grant record must serialize");
        let parsed: Capabilities =
            serde_json::from_str(&serialized).expect("grant record must deserialize");
        assert_eq!(parsed, caps);
    }
}
