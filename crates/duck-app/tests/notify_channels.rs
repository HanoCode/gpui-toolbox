//! 通知两条投递通道的无头验证：横幅档位**真的**拦得住、被拦下的**照记历史**。
//!
//! `duck_components::notify` 里的 `BannerLevel::allows` 只是个纯函数，断言它等于
//! 断言自己。这里走完整链路——真的建一个挂着 `Root` 的窗口、真的推通知、再数
//! `WindowExt::notifications` 里的条数，钉住的是「判据接在投递口上」而不是「判据存在」。
//!
//! 三档的取舍见 `BannerLevel` 的文档：`Off` 是个明确的取舍（连错误也不弹），
//! `WarningsAndErrors` 才是大多数人的偏好。无论哪一档，历史都照记。

use duck_components::notify::{self, BannerLevel, Channels, Tone};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, div,
};
use gpui_component::{Root, WindowExt as _, init as component_init, v_flex};

/// 最简内容根：通知挂在窗口的 `Root` 上，与内容树无关。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(div())
    }
}

/// 建一个挂着 `Root` 的窗口，并按 `main.rs` 的顺序做好初始化。
///
/// 身份那一步**不能省**：测试平台会像 Windows 一样，在拿到 AppUserModelID 之前
/// **直接丢掉**系统通知（`set_app_identity` 的实现见 gpui 的 test platform）。
fn notify_window(cx: &mut TestAppContext) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_app_identity("com.example.duck-toolbox-test", "Duck Toolbox Test");
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        let content = cx.new(|_| Blank);
        Root::new(content, window, cx)
    });
    cx
}

/// 当前窗口里的应用内通知条数。
fn count(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, cx| window.notifications(cx).len())
}

/// 设定两条通道。
fn set_channels(cx: &mut VisualTestContext, banners: BannerLevel, system: bool) {
    cx.update(|_, cx| notify::set_channels(Channels { banners, system }, cx));
}

/// 四个语义级别各推一条。
fn push_one_of_each(cx: &mut VisualTestContext) {
    cx.update(|window, cx| notify::info("这是一条信息", window, cx));
    cx.update(|window, cx| notify::success("这是一条成功提示", window, cx));
    cx.update(|window, cx| notify::warning("这是一条警告", window, cx));
    cx.update(|window, cx| notify::error("这是一条错误", window, cx));
}

/// `Off` 拦下全部四条；放开后又全部回来。
///
/// 后半段是**对照组**：只断言「关掉后没有」无法区分「真的拦住了」与「通知本来就推不出去」。
#[gpui::test]
fn turning_the_banners_off_suppresses_every_app_toast(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::Off, true);

    push_one_of_each(cx);
    assert_eq!(count(cx), 0, "关掉横幅后不该再出现任何应用内提示");

    set_channels(cx, BannerLevel::All, true);
    push_one_of_each(cx);
    assert_eq!(count(cx), 4, "放开后四条都该弹出来");
}

/// 中间档只放警告与错误：提示与成功被拦下，各只留一条。
#[gpui::test]
fn the_middle_level_lets_warnings_and_errors_through(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::WarningsAndErrors, true);

    push_one_of_each(cx);
    assert_eq!(count(cx), 2, "提示与成功不该弹，警告与错误要弹");
}

/// 被拦下的那条**仍然进历史**——这是关掉横幅之后唯一还能看到结论的地方。
#[gpui::test]
fn a_suppressed_toast_is_still_recorded_in_history(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::Off, true);
    let before = cx.update(|_, cx| notify::history(cx).len());

    cx.update(|window, cx| notify::error("磁盘写入失败", window, cx));
    assert_eq!(count(cx), 0, "横幅被档位拦下了");

    let history = cx.update(|_, cx| notify::history(cx));
    assert_eq!(history.len(), before + 1, "但历史照记");
    assert_eq!(history[0].message().as_ref(), "磁盘写入失败");
    assert_eq!(history[0].tone(), Tone::Error);
}

/// 两条都开：系统一条 + 应用内一条，各投各的。
#[gpui::test]
fn both_channels_on_deliver_to_both(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::All, true);

    cx.update(|window, cx| notify::system_with_toast("Duck", "备份完成", window, cx));
    cx.run_until_parked();

    assert_eq!(count(cx), 1, "应用内那条");
    assert_eq!(cx.shown_system_notifications().len(), 1, "系统那条");
}

/// 横幅开着但系统通道关掉：横幅照弹，**不能被另一条通道吞掉**。
#[gpui::test]
fn the_banner_still_shows_when_the_system_channel_is_off(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::All, false);

    cx.update(|window, cx| notify::system_with_toast("Duck", "备份完成", window, cx));
    cx.run_until_parked();

    assert_eq!(count(cx), 1, "系统通道关着，应用内这条还是要弹");
    assert!(
        cx.shown_system_notifications().is_empty(),
        "系统通道关着就不该有系统通知"
    );
}

/// 横幅关掉但系统通道开着：只剩系统那条，应用内不弹。
#[gpui::test]
fn system_only_delivery_leaves_no_app_toast(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::Off, true);

    cx.update(|window, cx| notify::system_with_toast("Duck", "备份完成", window, cx));
    cx.run_until_parked();

    assert_eq!(count(cx), 0, "横幅关着，应用内不该有提示");
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "系统通道开着，那条照投"
    );
}

/// 只走系统通道的 `system`：系统关掉时**什么都不推**，但历史照记。
#[gpui::test]
fn system_alone_is_silent_but_still_historical_when_turned_off(cx: &mut TestAppContext) {
    let cx = notify_window(cx);
    set_channels(cx, BannerLevel::All, false);
    let before = cx.update(|_, cx| notify::history(cx).len());

    cx.update(|window, cx| notify::system("Duck", "更新已就绪", window, cx));
    cx.run_until_parked();

    assert!(cx.shown_system_notifications().is_empty(), "开关关着就不投");
    assert_eq!(count(cx), 0, "只走系统通道的这条本来就不该建应用内提示");
    assert_eq!(
        cx.update(|_, cx| notify::history(cx).len()),
        before + 1,
        "历史照记"
    );
}
