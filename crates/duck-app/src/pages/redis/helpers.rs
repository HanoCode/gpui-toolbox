//! 跨模块复用的自由函数：锁 / 拒绝文案 / 连接档案读写 / 面板骨架与时间与标签。

use super::*;
// `Hsla` 是 `key_type_color` / `ttl_chip_color` 的返回类型：父模块的 `use gpui::{…}`
// 里没列它（父模块自己不直接用），glob 取不到，这里显式引一次。
use gpui::Hsla;

/// 会话互斥锁的取用：毒化时返回连接错误（不 panic——UI 线程不该被后台线程的崩溃带走）。
pub(super) fn lock(
    conn: &Arc<Mutex<RedisConnection>>,
) -> Result<std::sync::MutexGuard<'_, RedisConnection>, RedisError> {
    conn.lock()
        .map_err(|_| RedisError::Command("connection lock poisoned".into()))
}

/// 拒绝原因 → 用户可读文案。
///
/// 返回 `&'static str`（`tr` 的返回类型）：这些文案只进 `notify` / `meta_text`，
/// 不必各自分配一个 `SharedString`。
pub(super) fn deny_message(reason: DenyReason, cx: &App) -> &'static str {
    match reason {
        DenyReason::ConnectionReadOnly => tr(cx, "该连接的写入策略是「只读」，请在连接设置里改"),
        DenyReason::WriteLocked => tr(cx, "连接当前锁写，请先在状态栏解锁（限时 15 分钟）"),
        DenyReason::AclDenied => tr(cx, "服务端 ACL 未授予写权限，解锁也无法写入"),
    }
}

/// 连接 id：`redis-` + 时间戳后缀（稳定且不依赖随机数源）。
pub(super) fn new_connection_id() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("redis-{stamp}")
}

pub(super) fn text_input<C: AppContext>(
    window: &mut Window,
    cx: &mut C,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 从 kv 读连接档案；解析失败回落空表（用户重存即可覆盖）。
///
/// 2026-10-12 换档迁移：旧 wire 是 `read_only: bool`，新 wire 是 `write_policy`
/// 枚举（serde 读不回旧字段）。这里从原始 JSON 把旧布尔的 `true` 抢救成
/// `WritePolicy::ReadOnly`——只对**没有** `write_policy` 字段的条目生效。
pub(super) fn load_connections(cx: &App) -> Vec<RedisConfig> {
    let raw = duck_store::kv_get(cx, KEY_CONNECTIONS);
    let legacy_read_only: HashMap<String, bool> = raw
        .as_ref()
        .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(json).ok())
        .unwrap_or_default()
        .iter()
        .filter(|entry| entry.get("write_policy").is_none())
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let read_only = entry
                .get("read_only")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Some((id, read_only))
        })
        .collect();
    raw.as_ref()
        .and_then(|json| serde_json::from_str::<Vec<RedisConfig>>(json).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|mut cfg| {
            if cfg.write_policy == WritePolicy::FollowTag
                && legacy_read_only.get(&cfg.id).copied().unwrap_or(false)
            {
                cfg.write_policy = WritePolicy::ReadOnly;
            }
            cfg
        })
        .collect()
}

// ----------------------------------------------------------------------
// 服务端观测面板的渲染辅助（M1.4）
// ----------------------------------------------------------------------

/// 面板正文的统一滚动容器。
pub(super) fn panel_scroll(id: &'static str, body: AnyElement, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .id(id)
        .overflow_y_scroll()
        .p(tokens.spacing.sm)
        .child(body)
        .into_any_element()
}

/// 面板空态（还没拉数据 / 拉回来是空的 / 明确不支持）。
pub(super) fn panel_empty(message: impl Into<SharedString>, cx: &App) -> AnyElement {
    let tokens = active_theme(cx).tokens();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(tokens.spacing.lg)
        .child(meta_text(message.into(), cx))
        .into_any_element()
}

/// 面板里的输入框（外观与数据浏览的过滤框同源）。
pub(super) fn panel_input_box(input: &Entity<InputState>, cx: &App) -> Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    let extended = theme.extended();
    div()
        .flex_1()
        .min_w_0()
        .h(theme.density().control)
        .px(tokens.spacing.sm)
        .rounded(theme.components().input_radius)
        .border(extended.stroke.thin)
        .border_color(extended.colors.hairline)
        .bg(tokens.colors.background)
        .child(Input::new(input).appearance(false).bordered(false).w_full())
}

/// 当前 Unix 时间戳（秒）。
pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 Unix 时间戳渲染成「多久以前」。
///
/// **刻意不做绝对时间**：本仓不引时区库，硬凑一个 UTC 字符串会让人误以为是本地时间。
/// 而「这条慢日志是刚发生的还是陈年老账」才是真正要回答的问题，相对时间更直接。
///
/// 分档逻辑在 `duck-theme` 的格式化层（[`relative_time`]）。这里取**秒档**——
/// 慢日志要能区分「刚发生」与「几秒前」，而通知中心那两处取的是「刚刚」档。
pub(super) fn relative_age(timestamp: i64, now: i64, cx: &App) -> String {
    let delta = now.saturating_sub(timestamp).max(0);
    tr_relative_time(cx, delta, UnderMinute::Seconds)
}

/// 字节数的紧凑写法（`512 B` / `1.2 KB` / `3.4 MB`）。
///
/// 值面板顶部要给的是**服务端上的原始字节数**（解码、解压之前的那一份），
/// 所以单位走二进制前缀（1024 进制）、最多一位小数——差几个字节没有意义，
/// 有用的信息是量级。
///
/// 不带 `cx`：单位和数字都是 ASCII，没有要翻的文案。
pub(super) fn format_bytes(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        // 字节档不留小数：`512 B` 比 `512.0 B` 干净。
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 环境标签的展示文案（`None` 档不显示标签）。
pub(super) fn env_tag_label(tag: EnvTag, cx: &App) -> Option<&'static str> {
    match tag {
        EnvTag::None => None,
        EnvTag::Dev => Some(tr(cx, "开发")),
        EnvTag::Test => Some(tr(cx, "测试")),
        EnvTag::Staging => Some(tr(cx, "预发布")),
        EnvTag::Prod => Some(tr(cx, "生产")),
    }
}

/// 键类型的展示文案。
pub(super) fn key_type_label(key_type: KeyType, cx: &App) -> &'static str {
    match key_type {
        KeyType::String => tr(cx, "字符串"),
        KeyType::List => tr(cx, "列表"),
        KeyType::Set => tr(cx, "集合"),
        KeyType::ZSet => tr(cx, "有序集合"),
        KeyType::Hash => tr(cx, "哈希"),
        KeyType::Stream => tr(cx, "流"),
        KeyType::Module => tr(cx, "模块类型"),
        KeyType::Missing => tr(cx, "不存在"),
    }
}

/// 访问模式的展示文案。
pub(super) fn mode_label(mode: AccessMode, cx: &App) -> &'static str {
    match mode {
        AccessMode::ReadWrite => tr(cx, "读写"),
        AccessMode::SafeMode => tr(cx, "安全模式（锁写）"),
        AccessMode::StrictReadOnly => tr(cx, "严格只读（ACL 限制）"),
    }
}

/// 类型徽标的颜色（移植 zedis `KeyType::color` 的**去饱和**调色板）。
///
/// 一套色服务两种主题行不通：深色行上好看的取值放到浅色行只有 2.3–3.0:1 对比度，
/// 所以明暗各一套，且**每套在各自最难读的底（浅色选中 `#e2e2e2` / 深色选中
/// `#222222`）上都不低于 4.5:1**。
///
/// 之所以整列去饱和：类型徽标会在键树每一行重复，鲜艳填充会堆成一条噪音色带，
/// 跟键名抢注意力。
pub(super) fn key_type_color(key_type: KeyType, dark: bool) -> Hsla {
    let (light, dark_value) = match key_type {
        KeyType::String => (0x3c649f, 0x6e8cb9), // 蓝
        KeyType::List => (0x923fa6, 0xae75bd),   // 紫
        KeyType::Hash => (0x805d28, 0xb88f51),   // 橙
        KeyType::Set => (0x296b6b, 0x5aafaf),    // 青
        KeyType::ZSet => (0xb23838, 0xc77575),   // 红
        KeyType::Stream => (0x386f2a, 0x5b9b4b), // 绿
        KeyType::Module | KeyType::Missing => (0x735e5e, 0x8a8a8a), // 灰
    };
    duck_theme::color_from_rgb(if dark { dark_value } else { light })
}

/// TTL 芯片的三层配色（`< 1m` / `< 1h` / 其余），与 zedis 的 `render_item` 同口径。
///
/// **按值取好再带进渲染闭包**：`uniform_list` 的行闭包要求 `'static`，不能持有
/// 借自 `cx` 的 `&ExtendedTokens`；而 `Hsla` 是 `Copy` 的，先算成三个色值即可。
#[derive(Clone, Copy)]
pub(super) struct TtlChipPalette {
    /// `< 1m`：马上就没（警示色）。
    urgent: Hsla,
    /// `< 1h`：本次会话内可能消失（琥珀）。
    soon: Hsla,
    /// 其余：弱化色。
    calm: Hsla,
}

impl TtlChipPalette {
    pub(super) fn of(tokens: &duck_theme::ExtendedTokens) -> Self {
        Self {
            urgent: tokens.colors.status_failed,
            // zedis 的琥珀 `#cba26a`：比警示色弱、比正文弱化色显眼。
            soon: duck_theme::color_from_rgb(0xcba26a),
            calm: tokens.colors.text_tertiary,
        }
    }

    /// 按剩余秒数选一档。
    ///
    /// 调用点已先过 [`duck_redis::ttl_chip_kind`] 滤掉持久的键，
    /// 所以这里的入参只可能是真会过期的正值。
    pub(super) fn pick(&self, ttl_secs: i64) -> Hsla {
        if (0..60).contains(&ttl_secs) {
            self.urgent
        } else if (60..3600).contains(&ttl_secs) {
            self.soon
        } else {
            self.calm
        }
    }
}
