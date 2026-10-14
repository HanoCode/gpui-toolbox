//! 全局唤起快捷键的应用侧接线。
//!
//! 平台层（[`duck_platform`]）只管三件事：把键位登记到系统、按下时回调、以及**按槽位**
//! 分开记这两样。这里管剩下的应用级的事：槽位**各是什么**（唤起命令面板 / 唤起「随手看」
//! 悬浮窗）、按下之后**去哪**、键位**存在哪**（`kv`），以及登记不上时怎么**告诉用户**。
//!
//! 四条约定值得单独说：
//!
//! 1. **落点回调跑在后台线程上**。`App` 不是 `Send`，所以回调只把槽位名投进
//!    `futures_channel`，由一条主线程任务取出来动界面 —— 与第二实例转发完全同一种形状。
//! 2. **登记成功才落盘**。`kv` 里那个串必须与系统里真在用的那个一致，否则重启后
//!    读档会得到一个「看起来设过了、其实登记不上」的键位。
//! 3. **提示要能收住**。同一个键位反复登记失败只提示一次（`kv` 记着上次提示过的串），
//!    但换了个键位又失败就再说一次 —— 用户刚改过什么，正是最容易对上号的时候。
//!    提示按槽位**分开记**：两个键位都可能失败，一条提示只该说清其中一个。
//! 4. **槽位表是唯一真相**：`kv` 键名、默认键位、给人看的名字都挂在 [`HotkeySlot`] 上。
//!    加一个全局键 = 枚举加一个变体 + [`dispatch`] 加一条分支，不必到处找散落的键名。

use duck_components::notify::{self, Tone};
use duck_platform::{Hotkey, HotkeyError, ModifierState};
use duck_theme::{tr, trf};
use futures_channel::mpsc::unbounded;
use futures_util::StreamExt as _;
use gpui::{App, SharedString, Window};

/// 全局热键的槽位。
///
/// 定义在应用层而不是平台层：平台层不认识「launcher」这类业务概念（见
/// `duck_platform::hotkey` 的模块文档），名字从这里给过去。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeySlot {
    /// 唤起命令面板（`⌥Space`）。
    Launcher,
    /// 唤起「随手看」悬浮窗（`⌥⌘T`）。
    TaskFloat,
}

impl HotkeySlot {
    /// 全部槽位（装配时逐个登记，`dispatch` 与提示也按它遍历）。
    pub const ALL: [Self; 2] = [Self::Launcher, Self::TaskFloat];

    /// 平台层的槽名。
    ///
    /// 必须是 `'static`（平台层把它当不透明的键存着），所以写成字面量而不是 `String`。
    pub(crate) const fn name(self) -> duck_platform::Slot {
        match self {
            Self::Launcher => "launcher",
            Self::TaskFloat => "task_float",
        }
    }

    /// `kv` 里三项的键名：`（开关, 键位, 已提示失败的键位）`。
    ///
    /// launcher 那一组沿用既有键名（`hotkey_enabled` / `hotkey` / `hotkey_failure_announced`），
    /// **存档语义不变** —— 改名的代价是每个老用户的设置被静默复位。
    pub(crate) const fn keys(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Launcher => ("hotkey_enabled", "hotkey", "hotkey_failure_announced"),
            Self::TaskFloat => (
                "float_hotkey_enabled",
                "float_hotkey",
                "float_hotkey_failure_announced",
            ),
        }
    }

    /// 存档缺省键位（平台相关，见 `duck_platform::hotkey` 里那两个常量的注释）。
    pub(crate) const fn default_spec(self) -> &'static str {
        match self {
            Self::Launcher => duck_platform::LAUNCHER_HOTKEY_SPEC,
            Self::TaskFloat => duck_platform::FLOAT_HOTKEY_SPEC,
        }
    }

    /// 在 [`Self::ALL`] 里的下标。
    ///
    /// 存在的理由：设置页把每个槽位的录入状态放进一个定长数组，靠它索引。加槽位时
    /// `ALL` 与这里必须同改 —— 由 `every_slot_has_its_own_index` 钉住。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Launcher => 0,
            Self::TaskFloat => 1,
        }
    }

    /// 这个槽位唤起什么 —— 提示与设置页的行标题都用它。
    pub(crate) fn label(self, cx: &App) -> SharedString {
        match self {
            Self::Launcher => tr(cx, "唤起命令面板").into(),
            Self::TaskFloat => tr(cx, "打开「随手看」").into(),
        }
    }

    /// 平台层回调给的槽名 → 本枚举。不认识的名字返回 `None`（将来的第三个键在旧版本上
    /// 只是「按了没反应」，而不是 panic）。
    fn of_name(name: duck_platform::Slot) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.name() == name)
    }
}

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

/// 启动装配：起投递服务、定落点、按设置逐个登记槽位。
///
/// 排在存储之后（键位与开关都在 `kv` 里），排在窗口之前 —— 第一次按键可能在窗口
/// 还没开的时候到来，而落点本身能处理「窗口不存在」（它会开一个）。
pub fn install(cx: &mut App) {
    let (sender, mut receiver) = unbounded::<HotkeySlot>();
    duck_platform::start(move |slot| {
        // 后台线程：碰不得 `App`，只投信号。认不出的槽名在这里丢掉（见 `of_name`）。
        if let Some(slot) = HotkeySlot::of_name(slot) {
            let _ = sender.unbounded_send(slot);
        }
    });
    cx.spawn(async move |cx| {
        while let Some(slot) = receiver.next().await {
            cx.update(|cx| dispatch(slot, cx));
        }
    })
    .detach();

    for slot in HotkeySlot::ALL {
        if !crate::settings::hotkey_enabled(cx, slot) {
            tracing::info!(slot = slot.name(), "the global hotkey is switched off");
            continue;
        }
        if let Some(hotkey) = effective(cx, slot)
            && let Err(error) = duck_platform::bind(slot.name(), Some(&hotkey))
        {
            // 只记日志：这时候窗口还没开，提示投不出去。稍后由
            // [`announce_startup_failure`] 补一次（它排在主窗之后）。
            tracing::warn!(
                %error,
                spec = hotkey.spec(),
                slot = slot.name(),
                "the global hotkey is unavailable"
            );
        }
    }
}

/// 某个槽位被按下之后去哪。**落点只有这一处**（不是每个槽位各挂一个回调）。
fn dispatch(slot: HotkeySlot, cx: &mut App) {
    match slot {
        HotkeySlot::Launcher => crate::app::toggle_launcher(cx),
        HotkeySlot::TaskFloat => crate::app::toggle_task_float(cx),
    }
}

/// 某个槽位当前应当生效的键位：存档里的规范串（解析成功时），否则回落到该槽位默认键位。
///
/// 存档解析失败时用默认键位而不是「什么都不登记」：坏掉的存档不该让功能静默消失，
/// 而且日志里会留下原因（[`explain`] 那条路会把它说给用户听）。
pub fn effective(cx: &App, slot: HotkeySlot) -> Option<Hotkey> {
    if let Some(spec) = crate::settings::hotkey_spec(cx, slot) {
        match Hotkey::parse(&spec) {
            Ok(hotkey) => return Some(hotkey),
            Err(error) => tracing::warn!(
                %error,
                spec,
                slot = slot.name(),
                "the stored hotkey is unusable; falling back to the default"
            ),
        }
    }
    // 默认串必然合法（`duck_platform::hotkey` 的 `the_default_specs_round_trip` 守着）。
    Hotkey::parse(slot.default_spec()).ok()
}

/// 某个槽位存档里的原始键位串（**不做校验**），没有存档时是默认值。
///
/// 给「解析失败」那条降级路径一个能显示的东西：设置页要告诉用户「现在是这个键位，
/// 但它不可用」，而不是显示一片空白。
#[must_use]
pub fn raw_spec(cx: &App, slot: HotkeySlot) -> String {
    crate::settings::hotkey_spec(cx, slot).unwrap_or_else(|| slot.default_spec().to_owned())
}

/// 某个槽位给人看的键位串（用于提示与设置页）。
///
/// 解析不出来时退回原文 —— 那正是用户需要看到的东西。
#[must_use]
pub fn pretty_spec(cx: &App, slot: HotkeySlot) -> String {
    effective(cx, slot).map_or_else(|| raw_spec(cx, slot), |hotkey| hotkey.pretty())
}

/// 换绑：登记成功之后才落盘（见模块文档第 2 条）。
///
/// 失败时 `kv` 与系统登记**都保持原样**，所以调用方只需要把错误说给用户听。
pub fn rebind(cx: &App, slot: HotkeySlot, hotkey: &Hotkey) -> Result<(), HotkeyError> {
    duck_platform::bind(slot.name(), Some(hotkey))?;
    crate::settings::set_hotkey_spec(cx, slot, hotkey.spec());
    crate::settings::set_hotkey_enabled(cx, slot, true);
    // 换了键位，「已经提示过失败」这件事就作废了：新键位以后要是登记不上，
    // 该提示还得提示。
    crate::settings::set_hotkey_announced(cx, slot, "");
    Ok(())
}

/// 某个槽位的开关：`true` 登记存档里的键位，`false` 注销。
///
/// 与「开机自启」开关同一个形状 —— **先按目标值落状态，失败再回摆**，
/// 于是界面上的开关永远等于真实情况，不会出现「显示开着但其实没登记上」。
pub fn set_enabled(cx: &App, slot: HotkeySlot, on: bool) -> Result<(), HotkeyError> {
    if !on {
        // 注销几乎不会失败（最坏是系统里残留一个按不响的登记），不值得为它分支。
        duck_platform::bind(slot.name(), None)?;
        crate::settings::set_hotkey_enabled(cx, slot, false);
        return Ok(());
    }
    let Some(hotkey) = effective(cx, slot) else {
        return Ok(());
    };
    duck_platform::bind(slot.name(), Some(&hotkey))?;
    crate::settings::set_hotkey_enabled(cx, slot, true);
    Ok(())
}

/// 启动时补一次「键位没能生效」的提示。排在主窗就绪之后调（提示要有窗口可投）。
///
/// 去重规则见模块文档第 3 条：只有「这次失败的键位与上次提示过的不一样」才说。
/// 逐个槽位查：一个键位被占不该把另一个的提示也吞掉。
pub fn announce_startup_failure(cx: &mut App) {
    for slot in HotkeySlot::ALL {
        announce_slot_failure(cx, slot);
    }
}

/// [`announce_startup_failure`] 的单个槽位版本。
fn announce_slot_failure(cx: &mut App, slot: HotkeySlot) {
    if !crate::settings::hotkey_enabled(cx, slot) {
        return;
    }
    let Some(error) = duck_platform::last_error(slot.name()) else {
        return;
    };
    let spec = raw_spec(cx, slot);
    if crate::settings::hotkey_announced(cx, slot).as_deref() == Some(spec.as_str()) {
        tracing::debug!(
            spec,
            slot = slot.name(),
            "the hotkey failure was already announced; staying quiet"
        );
        return;
    }
    crate::settings::set_hotkey_announced(cx, slot, &spec);
    let message = failure_message(cx, slot, &spec, &error);
    let key = SharedString::from(format!("hotkey.unavailable.{}", slot.name()));
    crate::update::on_notify_window(cx, move |window, cx| {
        notify::sticky(key, Tone::Warning, message, window, cx);
    });
}

/// 设置页换键位失败时调：弹一条提示，并记下「这个键位提示过了」。
///
/// 记下来是为了**下一次启动别再重复同一句** —— 提示已经发过了，
/// 而设置页那一行会一直显示「未生效」，用户随时能回来看。
pub fn report_failure(
    window: &mut Window,
    cx: &mut App,
    slot: HotkeySlot,
    spec: &str,
    error: &HotkeyError,
) {
    crate::settings::set_hotkey_announced(cx, slot, spec);
    let message = failure_message(cx, slot, spec, error);
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

/// 提示文案：`「唤起命令面板」的快捷键 ⌥Space 没能生效。系统拒绝了…。可在「设置 → 行为」里换一个。`
///
/// 带上**是哪个键**（`slot.label`）：两个全局键都可能失败，只说「全局快捷键」等于
/// 让用户自己猜是哪一条。
fn failure_message(cx: &App, slot: HotkeySlot, spec: &str, error: &HotkeyError) -> String {
    let reason = explain(cx, error);
    let label = slot.label(cx);
    trf(
        cx,
        "{}的快捷键 {} 没能生效。{}可在「设置 → 行为」里换一个。",
        &[label.as_ref(), spec, reason.as_ref()],
    )
}

#[cfg(test)]
mod tests {
    use super::HotkeySlot;
    use std::collections::HashSet;

    /// 槽位表的四条不变量。
    ///
    /// 「下标与 `ALL` 一致」是给设置页那份定长数组用的；三条「不撞」都是会**静默**
    /// 出错的那类：平台槽名撞了 `of_name` 只会认出第一个（后加的槽位按了没反应）；
    /// `kv` 开关键名撞了两个槽位共用一个开关；默认键位撞了则 `bind` 会拒掉后一个
    /// （跨槽不许同键），表现为「出厂即报错」。
    #[test]
    fn the_slot_table_is_consistent() {
        assert_eq!(
            HotkeySlot::ALL.len(),
            2,
            "槽位数变了：请连带核对设置页的数组与 `index()`"
        );
        let mut names = HashSet::new();
        let mut switches = HashSet::new();
        let mut specs = HashSet::new();
        for (index, slot) in HotkeySlot::ALL.into_iter().enumerate() {
            assert_eq!(slot.index(), index, "{slot:?} 的下标与 ALL 的顺序不一致");
            assert!(names.insert(slot.name()), "{slot:?} 的平台槽名撞了");
            assert!(
                switches.insert(slot.keys().0),
                "{slot:?} 的 kv 开关键名撞了"
            );
            assert!(specs.insert(slot.default_spec()), "{slot:?} 的默认键位撞了");
        }
    }
}
