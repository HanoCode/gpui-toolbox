//! 全局唤起快捷键的应用侧接线。
//!
//! 平台层（[`duck_platform::hotkey`]）只管两件事：把键位登记到系统，以及按下时回调。
//! 这里管剩下的三件应用级的事：**落点**是什么（唤起 / 收起主窗）、键位**存在哪**
//! （`kv`），以及登记不上时怎么**告诉用户**。
//!
//! 三条约定值得单独说：
//!
//! 1. **落点回调跑在后台线程上**。`App` 不是 `Send`，所以回调只往 `futures_channel`
//!    投一个空信号，由一条主线程任务取出来动界面 —— 与第二实例转发完全同一种形状。
//! 2. **登记成功才落盘**。`kv` 里那个串必须与系统里真在用的那个一致，否则重启后
//!    读档会得到一个「看起来设过了、其实登记不上」的键位。
//! 3. **提示要能收住**。同一个键位反复登记失败只提示一次（`kv` 记着上次提示过的串），
//!    但换了个键位又失败就再说一次 —— 用户刚改过什么，正是最容易对上号的时候。

use duck_components::notify::{self, Tone};
use duck_platform::{Hotkey, HotkeyError, ModifierState};
use duck_theme::{tr, trf};
use futures_channel::mpsc::unbounded;
use futures_util::StreamExt as _;
use gpui::{App, SharedString, Window};

/// 启动提示的键：同键替换，反复启动不会堆一列（见 `notify::sticky` 的语义）。
const KEY_HOTKEY_NOTICE: &str = "hotkey.unavailable";

/// gpui 的修饰键状态 → 平台层的。
///
/// 逐字段搬而不是写 `From`：gpui 以后加一个修饰键时，这里的结构体字面量会**编译
/// 报错**（少字段），而不是悄悄少搬一个 —— 那种漏法是「按了没反应」，
/// 而且只在真机上才看得出来。
#[must_use]
pub fn modifiers_of(modifiers: gpui::Modifiers) -> ModifierState {
    ModifierState {
        control: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        platform: modifiers.platform,
        function: modifiers.function,
    }
}

/// 启动装配：起投递服务、定落点、按设置登记键位。
///
/// 排在存储之后（键位与开关都在 `kv` 里），排在窗口之前 —— 第一次按键可能在窗口
/// 还没开的时候到来，而落点本身能处理「窗口不存在」（它会开一个）。
pub fn install(cx: &mut App) {
    let (sender, mut receiver) = unbounded::<()>();
    duck_platform::start(move || {
        // 后台线程：碰不得 `App`，只投信号。
        let _ = sender.unbounded_send(());
    });
    cx.spawn(async move |cx| {
        while receiver.next().await.is_some() {
            cx.update(crate::app::toggle_launcher);
        }
    })
    .detach();

    if !crate::settings::hotkey_enabled(cx) {
        tracing::info!("the global hotkey is switched off");
        return;
    }
    if let Some(hotkey) = effective(cx)
        && let Err(error) = duck_platform::bind(Some(&hotkey))
    {
        // 只记日志：这时候窗口还没开，提示投不出去。稍后由
        // [`announce_startup_failure`] 补一次（它排在主窗之后）。
        tracing::warn!(%error, spec = hotkey.spec(), "the global hotkey is unavailable");
    }
}

/// 当前应当生效的键位：存档里的规范串（解析成功时），否则回落到默认键位。
///
/// 存档解析失败时用默认键位而不是「什么都不登记」：坏掉的存档不该让功能静默消失，
/// 而且日志里会留下原因（[`explain`] 那条路会把它说给用户听）。
pub fn effective(cx: &App) -> Option<Hotkey> {
    if let Some(spec) = crate::settings::hotkey_spec(cx) {
        match Hotkey::parse(&spec) {
            Ok(hotkey) => return Some(hotkey),
            Err(error) => tracing::warn!(
                %error,
                spec,
                "the stored hotkey is unusable; falling back to the default"
            ),
        }
    }
    // 默认串必然合法（`duck_platform::hotkey` 的 `the_default_spec_round_trips` 守着）。
    Hotkey::parse(duck_platform::DEFAULT_HOTKEY_SPEC).ok()
}

/// 存档里的原始键位串（**不做校验**），没有存档时是默认值。
///
/// 给「解析失败」那条降级路径一个能显示的东西：设置页要告诉用户「现在是这个键位，
/// 但它不可用」，而不是显示一片空白。
#[must_use]
pub fn raw_spec(cx: &App) -> String {
    crate::settings::hotkey_spec(cx)
        .unwrap_or_else(|| duck_platform::DEFAULT_HOTKEY_SPEC.to_owned())
}

/// 给人看的键位串（用于提示与设置页）。
///
/// 解析不出来时退回原文 —— 那正是用户需要看到的东西。
#[must_use]
pub fn pretty_spec(cx: &App) -> String {
    effective(cx).map_or_else(|| raw_spec(cx), |hotkey| hotkey.pretty())
}

/// 换绑：登记成功之后才落盘（见模块文档第 2 条）。
///
/// 失败时 `kv` 与系统登记**都保持原样**，所以调用方只需要把错误说给用户听。
pub fn rebind(cx: &App, hotkey: &Hotkey) -> Result<(), HotkeyError> {
    duck_platform::bind(Some(hotkey))?;
    crate::settings::set_hotkey_spec(cx, hotkey.spec());
    crate::settings::set_hotkey_enabled(cx, true);
    // 换了键位，「已经提示过失败」这件事就作废了：新键位以后要是登记不上，
    // 该提示还得提示。
    crate::settings::set_hotkey_announced(cx, "");
    Ok(())
}

/// 开关：`true` 登记存档里的键位，`false` 注销。
///
/// 与「开机自启」开关同一个形状 —— **先按目标值落状态，失败再回摆**，
/// 于是界面上的开关永远等于真实情况，不会出现「显示开着但其实没登记上」。
pub fn set_enabled(cx: &App, on: bool) -> Result<(), HotkeyError> {
    if !on {
        // 注销几乎不会失败（最坏是系统里残留一个按不响的登记），不值得为它分支。
        duck_platform::bind(None)?;
        crate::settings::set_hotkey_enabled(cx, false);
        return Ok(());
    }
    let Some(hotkey) = effective(cx) else {
        return Ok(());
    };
    duck_platform::bind(Some(&hotkey))?;
    crate::settings::set_hotkey_enabled(cx, true);
    Ok(())
}

/// 启动时补一次「键位没能生效」的提示。排在主窗就绪之后调（提示要有窗口可投）。
///
/// 去重规则见模块文档第 3 条：只有「这次失败的键位与上次提示过的不一样」才说。
pub fn announce_startup_failure(cx: &mut App) {
    if !crate::settings::hotkey_enabled(cx) {
        return;
    }
    let Some(error) = duck_platform::last_error() else {
        return;
    };
    let spec = raw_spec(cx);
    if crate::settings::hotkey_announced(cx).as_deref() == Some(spec.as_str()) {
        tracing::debug!(
            spec,
            "the hotkey failure was already announced; staying quiet"
        );
        return;
    }
    crate::settings::set_hotkey_announced(cx, &spec);
    let message = failure_message(cx, &pretty_spec(cx), &error);
    crate::update::on_notify_window(cx, move |window, cx| {
        notify::sticky(KEY_HOTKEY_NOTICE, Tone::Warning, message, window, cx);
    });
}

/// 设置页换键位失败时调：弹一条提示，并记下「这个键位提示过了」。
///
/// 记下来是为了**下一次启动别再重复同一句** —— 提示已经发过了，
/// 而设置页那一行会一直显示「未生效」，用户随时能回来看。
pub fn report_failure(window: &mut Window, cx: &mut App, spec: &str, error: &HotkeyError) {
    crate::settings::set_hotkey_announced(cx, spec);
    let message = failure_message(cx, spec, error);
    notify::error(message, window, cx);
}

/// 把失败原因翻成一句人话。
///
/// 平台层按约定只给英文（那是日志用的），面向用户的那一层在这里 ——
/// 与 `duck-platform::LaunchAtLoginError` 的处理一致。
///
/// 每条都以句号收尾：它既被单独投成一条提示，也会拼进 [`failure_message`]，
/// 两处都靠这个句号把句子断干净。
#[must_use]
pub fn explain(cx: &App, error: &HotkeyError) -> SharedString {
    match error {
        HotkeyError::NeedsModifier => tr(
            cx,
            "至少要按一个修饰键：⌥ / ⌃ / ⌘（Windows 上是 Alt / Ctrl / Win）。",
        )
        .into(),
        HotkeyError::UnsupportedKey(key) => trf(
            cx,
            "「{}」不能用作全局快捷键：媒体键与音量键要额外的系统授权，还会抢走系统控制。",
            &[key.as_str()],
        )
        .into(),
        HotkeyError::ModifierOnly => tr(cx, "只按下了修饰键，还要再按一个普通键。").into(),
        HotkeyError::Malformed(bad) => trf(cx, "「{}」不是可用的组合。", &[bad.as_str()]).into(),
        HotkeyError::Rejected(_) => tr(cx, "系统拒绝了这个组合，多半已被其他程序占用。").into(),
    }
}

/// 提示文案：`全局快捷键 ⌥Space 没能生效。系统拒绝了…。可在「设置 → 行为」里换一个。`
fn failure_message(cx: &App, spec: &str, error: &HotkeyError) -> String {
    let reason = explain(cx, error);
    trf(
        cx,
        "全局快捷键 {} 没能生效。{}可在「设置 → 行为」里换一个。",
        &[spec, reason.as_ref()],
    )
}
