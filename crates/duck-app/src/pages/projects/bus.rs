//! 项目管理数据的**进程内变更广播**。
//!
//! 项目管理页有两个视图：主窗里的页面，与「随手看」悬浮窗（`docs/projects-design.md`
//! §4.3 ⑤）。两者**各自**打开一个 `duck-project` 连接（同一个库文件），所以「谁改了库」
//! 对方不会自动知道 —— 而原型明确要求「改动即时同步到主窗口，不需要重启或手动刷新
//! （本项目用事件通道广播变更）」。
//!
//! 这里就是那条通道：写入方在**落库之后** [`notify`]，两个视图各自 `cx.subscribe`
//! 到它、收到就把自己那读一遍。广播的是「变了」而不是「变了什么」—— 一个视图重读一次
//! 是几次本地 SELECT，比维护一份差异表便宜得多，也不会漏字段。
//!
//! # 为什么不用 `kv` 里的版本号
//!
//! 版本号要**写**存储（去抖 400ms，还多一份磁盘写），并且两个视图还得定期去查它；
//! 广播只在进程内走一圈。两个视图都在同一个 `App` 里，没有跨进程需求。

use gpui::{App, AppContext as _, Entity, EventEmitter, Global};

/// 一次「项目数据变了」。空载荷：收方只需要知道「该重读了」。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectChanged;

/// 广播中心。
///
/// 一个空类型 —— 它存在的唯一理由是当 [`EventEmitter`] 的宿主。做成实体而不是裸
/// `Global`，是为了拿到 `cx.subscribe` 的**生命周期管理**：订阅随实体走，
/// 窗口关掉就自动退订，不必手工记账。
pub(crate) struct ProjectBus;

impl EventEmitter<ProjectChanged> for ProjectBus {}

/// 广播中心的全局句柄。
#[derive(Clone)]
struct Handle(Entity<ProjectBus>);

impl Global for Handle {}

/// 把广播中心装进 `App` 全局（`app::install` 里调一次）。**幂等**：已经装过就原样留着
/// —— 换一个实例会让先订阅的那一方永远收不到消息（两个视图订阅的必须是同一个总线）。
pub(crate) fn install(cx: &mut App) {
    if cx.has_global::<Handle>() {
        return;
    }
    let bus = cx.new(|_| ProjectBus);
    cx.set_global(Handle(bus));
}

/// 取广播中心；没装过就先装一个（与 `app::Desktop::global_mut` 同款兜底 ——
/// 单测只建页面、不跑整套 `install`，不该因此 panic）。
pub(crate) fn handle(cx: &mut App) -> Entity<ProjectBus> {
    install(cx);
    cx.global::<Handle>().0.clone()
}

/// 告知「项目数据变了」。写入方在**落库之后**调（顺序不能反：收方收到就会去重读）。
pub(crate) fn notify(cx: &mut App) {
    let bus = handle(cx);
    bus.update(cx, |_, cx| cx.emit(ProjectChanged));
}

#[cfg(test)]
mod tests {
    use super::{ProjectBus, handle, install, notify};
    use gpui::{AppContext as _, Context, IntoElement as _, TestAppContext, Window, div};

    /// 一个只数广播次数的宿主。
    ///
    /// 订阅必须发生在**实体的 `Context`** 上（`subscribe` 是 `Context` 的方法），
    /// 所以这里用一个真实实体承接，而不是在裸 `App` 上试。
    struct Counter {
        hits: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl Counter {
        fn new(cx: &mut Context<Self>) -> Self {
            let bus = handle(cx);
            let hits = std::rc::Rc::new(std::cell::Cell::new(0));
            let seen = hits.clone();
            let _sub = cx.subscribe(&bus, move |_this, _bus, _event, _cx| {
                seen.set(seen.get() + 1);
            });
            // 订阅要活到测试结束；这个实体本身就是宿主，让订阅跟着它走即可。
            std::mem::forget(_sub);
            Self { hits }
        }
    }

    impl gpui::Render for Counter {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// 广播要真的送到订阅方手里（这是「悬浮窗跟着主窗变」的全部机制）。
    #[test]
    fn a_notification_reaches_every_subscriber() {
        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            let first = cx.new(Counter::new);
            let second = cx.new(Counter::new);
            notify(cx);
            notify(cx);
            assert_eq!(first.read(cx).hits.get(), 2, "两条广播都该收到");
            assert_eq!(second.read(cx).hits.get(), 2);
        });
    }

    /// 两个视图必须订阅到**同一个**总线，否则广播各说各话，永远不会互相唤醒 ——
    /// `install` 的幂等性就是钉这件事。
    #[test]
    fn the_handle_is_stable() {
        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            let first: gpui::Entity<ProjectBus> = handle(cx);
            assert_eq!(first.entity_id(), handle(cx).entity_id());
            // 再显式装一次也不该换实例。
            install(cx);
            assert_eq!(first.entity_id(), handle(cx).entity_id());
        });
    }
}
