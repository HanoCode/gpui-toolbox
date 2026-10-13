//! AI 配置读写层：助手页与设置页共用的单一入口。
//!
//! 配置与凭据**都落 kv**，但凭据是**密文**（对齐 `docs/ai-design.md` §4.4）：
//!
//! | 内容 | 落点 | 原因 |
//! | --- | --- | --- |
//! | 厂商档案（名称 / base_url / 模型） | `kv: ai_providers` 单键 JSON 数组 | 非敏感，可随备份走 |
//! | 生成参数与默认厂商 | `kv: ai_settings` 单键 JSON | 同上 |
//! | API key | **同一条厂商档案的 `api_key` 字段，密文** | kv 里没有明文；解密要 `vault.key`，它不在归档里 |
//!
//! # 渲染路径上能做什么、不能做什么
//!
//! 「某条档案配过密钥没有」就是 `ProviderProfile::api_key.is_some()`——档案本身是
//! kv 的内存快照，读它不落盘也不解密，所以渲染路径上问它没问题。
//! 而**解密**（[`load_api_key`]）会走一次 AES-GCM，只在「发请求前」与
//! 「打开编辑弹窗回显」这两处调。
//!
//! 旧版本把 key 放进系统钥匙串，另用 `kv: ai_configured_keys` 记一份
//! 「配过密钥的 id」来绕开渲染路径上的系统调用。现在凭据跟着档案走，那份标记
//! 就是冗余的（同一个事实记在两处，迟早不一致），已删掉；
//! [`load_configured`] 保留原来的签名，改成从档案推导。
//!
//! 页面只读这里的函数，不自己拼 kv 键——键名只有一处定义，改动不会漏。

use duck_ai::{CompactionPolicy, ProviderProfile, provider};
use gpui::App;

/// 厂商档案数组的 kv 键（范式同 `db_connections` / `redis_connections`）。
pub const KEY_PROVIDERS: &str = "ai_providers";
/// 生成参数与默认厂商的 kv 键。
pub const KEY_SETTINGS: &str = "ai_settings";
/// 配置版本号（单调递增）。见 [`revision`]。
pub const KEY_REVISION: &str = "ai_config_rev";

/// 默认系统提示词。
///
/// 刻意保持「不编造」这一条：工具箱里的 AI 会读文件、看 schema，
/// 它编出来的结论比答不出来更糟。
pub const DEFAULT_SYSTEM_PROMPT: &str = "你是 Duck Toolbox 的内置助手。回答用简洁的中文，代码与命令原样给出。信息不足时直接说不确定，不要编造。";

/// AI 助手的生成参数与默认选择。
#[derive(Debug, Clone, PartialEq)]
pub struct AiSettings {
    /// 默认厂商档案 id；`None` = 用档案列表里的第一条。
    pub default_provider: Option<String>,
    /// 采样温度；`None` = 用服务端默认。
    pub temperature: Option<f32>,
    /// 最大输出 token；`None` = 用服务端默认。
    pub max_tokens: Option<u32>,
    /// 系统提示词；空串表示不加 system 消息。
    pub system_prompt: String,
    /// 是否允许助手调用工具（读文件 / 列目录 / 检索知识库）。
    ///
    /// **默认关闭**：会读本机文件的开关一律由用户显式打开，与本仓
    /// `require_confirm_writes` 同一口径。关闭时连 `tools` 字段都不下发给模型。
    pub tools_enabled: bool,
    /// 工具可访问的根目录；`None` = 未配置（此时文件类工具不注册）。
    pub workspace_dir: Option<String>,
    /// 一次问答内允许的最大工具调用轮数；`None` = 用内核默认值
    /// （[`crate::pages::assistant`] 的 `MAX_TOOL_ROUNDS`）。
    ///
    /// 是**失控循环的兜底**，不是功能开关：模型偶发会「调工具 → 看结果 → 再调同一个」，
    /// 没有上限就是无限花钱。
    pub max_tool_rounds: Option<usize>,
    /// 上下文预算上限（字符数）；`None` = 用内核默认值
    /// （[`CompactionPolicy::default`] 的 `max_chars`）。
    ///
    /// ⚠️ **`Some(0)` 表示关闭压缩**（内核语义）。关掉之后长会话可能直接超出模型的
    /// 上下文窗口，请求会被服务端拒；调小则更早丢历史。两者都只该由用户明知代价地选。
    pub compaction_max_chars: Option<usize>,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            default_provider: None,
            temperature: None,
            max_tokens: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
            tools_enabled: false,
            workspace_dir: None,
            max_tool_rounds: None,
            compaction_max_chars: None,
        }
    }
}

/// 落盘形态。
///
/// 每个字段都 `#[serde(default)]` + `Option`：旧版本存下的 JSON 缺字段时能读回默认值，
/// 而不是整份配置解析失败（本项目既有的 `AppearanceWire` 同款纪律）。
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct AiSettingsWire {
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    tools_enabled: Option<bool>,
    #[serde(default)]
    workspace_dir: Option<String>,
    #[serde(default)]
    max_tool_rounds: Option<usize>,
    #[serde(default)]
    compaction_max_chars: Option<usize>,
}

impl From<AiSettingsWire> for AiSettings {
    fn from(wire: AiSettingsWire) -> Self {
        Self {
            default_provider: wire.default_provider,
            temperature: wire.temperature,
            max_tokens: wire.max_tokens,
            system_prompt: wire
                .system_prompt
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned()),
            tools_enabled: wire.tools_enabled.unwrap_or(false),
            workspace_dir: wire.workspace_dir,
            max_tool_rounds: wire.max_tool_rounds,
            compaction_max_chars: wire.compaction_max_chars,
        }
    }
}

impl From<&AiSettings> for AiSettingsWire {
    fn from(settings: &AiSettings) -> Self {
        Self {
            default_provider: settings.default_provider.clone(),
            temperature: settings.temperature,
            max_tokens: settings.max_tokens,
            system_prompt: Some(settings.system_prompt.clone()),
            tools_enabled: Some(settings.tools_enabled),
            workspace_dir: settings.workspace_dir.clone(),
            max_tool_rounds: settings.max_tool_rounds,
            compaction_max_chars: settings.compaction_max_chars,
        }
    }
}

/// 读设置；缺失或损坏时回落默认值（不 panic、不阻塞）。
#[must_use]
pub fn load_settings(cx: &App) -> AiSettings {
    let Some(raw) = duck_store::kv_get(cx, KEY_SETTINGS) else {
        return AiSettings::default();
    };
    serde_json::from_str::<AiSettingsWire>(&raw)
        .map(AiSettings::from)
        .unwrap_or_default()
}

/// 写设置。
pub fn save_settings(cx: &App, settings: &AiSettings) {
    let wire = AiSettingsWire::from(settings);
    if let Ok(json) = serde_json::to_string(&wire) {
        duck_store::kv_put(cx, KEY_SETTINGS, json);
        bump_revision(cx);
    }
}

/// 读厂商档案；损坏时回落空表。
#[must_use]
pub fn load_providers(cx: &App) -> Vec<ProviderProfile> {
    let Some(raw) = duck_store::kv_get(cx, KEY_PROVIDERS) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<ProviderProfile>>(&raw).unwrap_or_default()
}

/// 写厂商档案。
pub fn save_providers(cx: &App, providers: &[ProviderProfile]) {
    if let Ok(json) = serde_json::to_string(providers) {
        duck_store::kv_put(cx, KEY_PROVIDERS, json);
    }
    // 这里不需要清理任何东西：凭据就挂在档案自己的 `api_key` 字段上，
    // 档案没了它也跟着没了。旧实现在这里维护过一份 `ai_configured_keys`
    // 标记表，那种「同一个事实记在两处」的结构迟早会不一致。
    bump_revision(cx);
}

// ---------------------------------------------------------------------------
// 配置版本号
// ---------------------------------------------------------------------------

/// 读配置版本号。
///
/// 助手页每帧要比一次「设置页那边改过配置没有」。档案本身可以比 `PartialEq`
/// （`ai::load_providers` 已经在每帧解析一份 JSON），但**只加了一个 API key**
/// 不会让档案集合发生变化，只看档案就会漏掉那种改动。所以另记一个单调递增的号：
/// 任何一处配置写入都 +1，读侧只比一个整数，比对整份 JSON 便宜。
#[must_use]
pub fn revision(cx: &App) -> u64 {
    duck_store::kv_get(cx, KEY_REVISION)
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or_default()
}

/// 版本号 +1。
fn bump_revision(cx: &App) {
    let next = revision(cx).wrapping_add(1);
    duck_store::kv_put(cx, KEY_REVISION, next.to_string());
}

// ---------------------------------------------------------------------------
// 凭据
// ---------------------------------------------------------------------------

/// 读出「已配过密钥」的厂商 id（按 id 升序）。
///
/// 「配过」= 档案里的 `api_key` 不是 `None`。签名与旧版一致（旧版读的是
/// `kv: ai_configured_keys` 那份标记表），所以调用点一行都不用改。
#[must_use]
pub fn load_configured(cx: &App) -> Vec<String> {
    let mut ids: Vec<String> = load_providers(cx)
        .into_iter()
        .filter(|profile| profile.api_key.is_some())
        .map(|profile| profile.id)
        .collect();
    ids.sort();
    ids
}

/// 本机的凭据存储此刻能不能用。
///
/// 实现上会真的封一个空串（首次调用顺带生成密钥文件），所以调用方要自己缓存
/// 这份结果，别在渲染里逐帧调。设置页 / 助手页各存一份进自己的视图状态。
#[must_use]
pub fn secure_storage_available() -> bool {
    duck_ai::secret::is_available()
}

/// 读出某个厂商的 API key 明文（解密）。没配过、解不开都是 `None`。
///
/// **会真的解密**：只在「发请求前」与「打开编辑弹窗回显」两处调，
/// 别放进渲染路径——每帧一次 AES-GCM 是白烧 CPU。
#[must_use]
pub fn load_api_key(cx: &App, provider_id: &str) -> Option<String> {
    let providers = load_providers(cx);
    let sealed = providers
        .iter()
        .find(|item| item.id == provider_id)?
        .api_key
        .as_deref()?;
    duck_ai::secret::open(sealed)
}

/// 新建一条档案的 id（时间戳 + 计数，与 `duck-ai` 的落库 id 同款思路）。
#[must_use]
pub fn new_profile_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("prov-{nanos:x}-{sequence:x}")
}

/// 由激活档案生成下拉选项：`(档案 id, 「厂商名 · 模型」)`。
#[must_use]
pub fn model_options(providers: &[ProviderProfile]) -> Vec<(String, String)> {
    providers
        .iter()
        .map(|profile| {
            let model = if profile.model.is_empty() {
                PLACEHOLDER_MODEL
            } else {
                profile.model.as_str()
            };
            (
                profile.id.clone(),
                format!("{} · {}", profile_display_name(profile), model),
            )
        })
        .collect()
}

/// 模型未填时的占位提示（`model_options` 与右栏共用，别再各写字面量）。
pub const PLACEHOLDER_MODEL: &str = "—";

/// 档案的展示名（空名回落模板名）。
#[must_use]
pub fn profile_display_name(profile: &ProviderProfile) -> String {
    if !profile.name.trim().is_empty() {
        return profile.name.clone();
    }
    provider::template(&profile.template_id)
        .map(|entry| entry.name.to_owned())
        .unwrap_or_else(|| profile.template_id.clone())
}

/// 分辨率化温度 / token 上限的输入。
///
/// 返回 `(值, 是否合法)`；非法输入**不写入**设置，避免把 `NaN` 或 0 落盘。
#[must_use]
pub fn parse_temperature(raw: &str) -> Option<Option<f32>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    trimmed
        .parse::<f32>()
        .ok()
        .filter(|value| (0.0..=2.0).contains(value))
        .map(Some)
}

/// 解析最大输出 token；空串 = 不限制。
#[must_use]
pub fn parse_max_tokens(raw: &str) -> Option<Option<u32>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    trimmed
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
}

/// 解析最大工具轮次；空串 = 用默认值。
#[must_use]
pub fn parse_max_rounds(raw: &str) -> Option<Option<usize>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    trimmed
        .parse::<usize>()
        .ok()
        .filter(|value| *value >= 1)
        .map(Some)
}

/// 最大工具轮次的输入框文本。
#[must_use]
pub fn max_rounds_text(settings: &AiSettings) -> String {
    settings
        .max_tool_rounds
        .map(|value| value.to_string())
        .unwrap_or_default()
}

/// 解析上下文预算（字符数）；空串 = 用内核默认。
///
/// ⚠️ 与其它数值项不同，**`0` 是合法值**：内核把 `max_chars == 0` 解释为「关闭压缩」，
/// 所以这里不加 `> 0` 的下限过滤。
#[must_use]
pub fn parse_compaction_budget(raw: &str) -> Option<Option<usize>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    trimmed.parse::<usize>().ok().map(Some)
}

/// 上下文预算的输入框文本。
#[must_use]
pub fn compaction_budget_text(settings: &AiSettings) -> String {
    settings
        .compaction_max_chars
        .map(|value| value.to_string())
        .unwrap_or_default()
}

/// 由设置得到内核的上下文压缩策略。
///
/// 只把「总预算」这一个维度暴露给用户：其余四项（近端保留长度、工具结果的头 / 尾 /
/// 裁枝阈值）是调参细节，改错了只会让压缩变得难以解释，不如固定在内核默认值上。
#[must_use]
pub fn compaction_policy(settings: &AiSettings) -> CompactionPolicy {
    match settings.compaction_max_chars {
        None => CompactionPolicy::default(),
        Some(max_chars) => CompactionPolicy {
            max_chars,
            ..CompactionPolicy::default()
        },
    }
}

/// 温度的输入框文本。
#[must_use]
pub fn temperature_text(settings: &AiSettings) -> String {
    settings
        .temperature
        .map(|value| format!("{value}"))
        .unwrap_or_default()
}

/// 最大输出 token 的输入框文本。
#[must_use]
pub fn max_tokens_text(settings: &AiSettings) -> String {
    settings
        .max_tokens
        .map(|value| value.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip_keeps_values() {
        let settings = AiSettings {
            default_provider: Some("p1".into()),
            temperature: Some(0.5),
            max_tokens: Some(1024),
            system_prompt: "custom".into(),
            tools_enabled: true,
            workspace_dir: Some("/tmp/ws".into()),
            max_tool_rounds: Some(12),
            compaction_max_chars: Some(48_000),
        };
        let wire = AiSettingsWire::from(&settings);
        let json = serde_json::to_string(&wire).unwrap();
        let back: AiSettings = serde_json::from_str::<AiSettingsWire>(&json)
            .unwrap()
            .into();
        assert_eq!(back, settings);
    }

    #[test]
    fn tools_default_to_off_on_old_configs() {
        // 旧版本落盘的 JSON 里没有这两个字段：必须回落到「工具关闭」，
        // 绝不能因为缺字段就默认打开「读本机文件」。
        let back: AiSettings = serde_json::from_str::<AiSettingsWire>("{\"temperature\":0.3}")
            .unwrap()
            .into();
        assert!(!back.tools_enabled);
        assert!(back.workspace_dir.is_none());
        // 工具轮次同理：缺字段必须回落到内核默认，而不是凭空给一个 0（0 轮 = 直接禁用）。
        assert!(back.max_tool_rounds.is_none());
        // 上下文预算同理：缺字段 = 用内核默认档，**不能**落到 `0`（那是「关闭压缩」）。
        assert!(back.compaction_max_chars.is_none());
    }

    /// 上下文预算的解析：空 = 默认，`0` = 关闭压缩（合法），非数字 = 拒绝。
    #[test]
    fn compaction_budget_parsing() {
        assert_eq!(parse_compaction_budget(""), Some(None));
        assert_eq!(parse_compaction_budget("   "), Some(None));
        assert_eq!(parse_compaction_budget("0"), Some(Some(0)));
        assert_eq!(parse_compaction_budget(" 96000 "), Some(Some(96_000)));
        assert_eq!(parse_compaction_budget("-1"), None);
        assert_eq!(parse_compaction_budget("很多"), None);
    }

    /// 设置 → 内核策略的映射：只覆盖上限，其余吃内核默认；`Some(0)` 要真的传成 0。
    #[test]
    fn compaction_policy_follows_the_budget() {
        let unset = compaction_policy(&AiSettings::default());
        assert_eq!(unset, CompactionPolicy::default(), "未设置就用内核默认");

        let off = compaction_policy(&AiSettings {
            compaction_max_chars: Some(0),
            ..AiSettings::default()
        });
        assert_eq!(off.max_chars, 0, "0 必须原样传下去（内核据此关闭压缩）");
        assert_eq!(
            off.keep_recent_chars,
            CompactionPolicy::default().keep_recent_chars,
            "其余维度仍吃内核默认"
        );

        let custom = compaction_policy(&AiSettings {
            compaction_max_chars: Some(12_345),
            ..AiSettings::default()
        });
        assert_eq!(custom.max_chars, 12_345);
    }

    #[test]
    fn rounds_parsing_rejects_zero() {
        assert_eq!(parse_max_rounds(""), Some(None));
        assert_eq!(parse_max_rounds(" 6 "), Some(Some(6)));
        assert_eq!(parse_max_rounds("0"), None);
        assert_eq!(parse_max_rounds("-1"), None);
        assert_eq!(parse_max_rounds("x"), None);
        assert_eq!(max_rounds_text(&AiSettings::default()), "");
    }

    #[test]
    fn missing_system_prompt_falls_back_to_default() {
        let back: AiSettings = serde_json::from_str::<AiSettingsWire>("{}").unwrap().into();
        assert_eq!(back.system_prompt, DEFAULT_SYSTEM_PROMPT);
        assert!(back.default_provider.is_none());
    }

    #[test]
    fn partial_wire_parses() {
        let back: AiSettings = serde_json::from_str::<AiSettingsWire>("{\"temperature\":0.2}")
            .unwrap()
            .into();
        assert_eq!(back.temperature, Some(0.2));
        assert!(back.max_tokens.is_none());
    }

    #[test]
    fn temperature_parsing_rejects_out_of_range() {
        assert_eq!(parse_temperature(""), Some(None));
        assert_eq!(parse_temperature(" 0.7 "), Some(Some(0.7)));
        assert_eq!(parse_temperature("2.5"), None);
        assert_eq!(parse_temperature("-0.1"), None);
        assert_eq!(parse_temperature("abc"), None);
    }

    #[test]
    fn token_parsing_rejects_zero() {
        assert_eq!(parse_max_tokens(""), Some(None));
        assert_eq!(parse_max_tokens("4096"), Some(Some(4096)));
        assert_eq!(parse_max_tokens("0"), None);
        assert_eq!(parse_max_tokens("x"), None);
    }

    #[test]
    fn option_labels_pair_name_and_model() {
        let providers = vec![ProviderProfile {
            id: "a".into(),
            template_id: "deepseek".into(),
            name: "DeepSeek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            is_local: false,
            api_key: None,
        }];
        assert_eq!(
            model_options(&providers),
            vec![("a".to_owned(), "DeepSeek · deepseek-chat".to_owned())]
        );
    }

    #[test]
    fn display_name_falls_back_to_template() {
        let mut profile = ProviderProfile {
            id: "a".into(),
            template_id: "openai".into(),
            name: "  ".into(),
            base_url: "x".into(),
            model: "m".into(),
            is_local: false,
            api_key: None,
        };
        assert_eq!(profile_display_name(&profile), "OpenAI");
        profile.name = "My gateway".into();
        assert_eq!(profile_display_name(&profile), "My gateway");
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_profile_id(), new_profile_id());
    }
}
