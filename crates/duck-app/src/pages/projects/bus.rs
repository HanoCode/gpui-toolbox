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

/// 「把主窗定位到这条任务」—— 浮窗跳主窗时带上的上下文。
///
/// 与 [`ProjectChanged`] 分开成两种事件：那个的语义是「重读一遍」，载荷为空；
/// 这个要带 `id`，而且**只有主窗**该消费（浮窗自己不必理会自己发出的定位请求）。
/// 混成一个事件就得让每个订阅者去判「这条是不是给我的」。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FocusTask(pub(crate) i64);

impl EventEmitter<FocusTask> for ProjectBus {}

/// 请求主窗把某条任务摆到右栏。调用方自己负责把主窗抬起来、切到项目页 ——
/// 这里只发信号，不碰窗口（副窗调 `reveal_main_window` 会把焦点抢走，
/// 顺序该由调用方决定）。
pub(crate) fn focus_task(cx: &mut App, id: i64) {
    let bus = handle(cx);
    bus.update(cx, |_, cx| cx.emit(FocusTask(id)));
}

#[cfg(test)]
mod tests {
    use super::{ProjectBus, handle, install, notify};
    use gpui::{AppContext as _, Context, IntoElement, Subscription, TestAppContext, Window, div};
    use std::cell::Cell;
    use std::rc::Rc;

    /// 一个只数广播次数的宿主。
    ///
    /// 订阅必须发生在**实体的 `Context`** 上（`subscribe` 是 `Context` 的方法），
    /// 所以这里用一个真实实体承接，而不是在裸 `App` 上试。
    struct Counter {
        hits: Rc<Cell<u32>>,
        /// 订阅跟着宿主走：实体活着它就活着，drop 时自动退订 —— 正好也是产品里
        /// 两个视图的用法（存进 `_subscriptions`）。
        _subscription: Subscription,
    }

    impl Counter {
        fn new(cx: &mut Context<Self>) -> Self {
            let bus = handle(cx);
            let hits = Rc::new(Cell::new(0));
            let seen = hits.clone();
            // 事件类型要写出来：`ProjectBus` 现在是两种事件的宿主
            // （`ProjectChanged` 与 `FocusTask`），不标注推不出来订的是哪一种。
            let subscription = cx.subscribe(
                &bus,
                move |_this, _bus, _event: &super::ProjectChanged, _cx| {
                    seen.set(seen.get() + 1);
                },
            );
            Self {
                hits,
                _subscription: subscription,
            }
        }
    }

    impl gpui::Render for Counter {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// 广播要真的送到订阅方手里（这是「悬浮窗跟着主窗变」的全部机制）。
    ///
    /// ⚠️ 断言必须在 `update` **返回之后**：`Context::subscribe` 拿到的
    /// [`Subscription`] 是**惰性**的（`SubscriberSet::insert` 建出来时 `active=false`），
    /// 激活走 `App::new_subscription` 里的 `defer`；而 `emit` 本身也是一个
    /// `Effect::Emit`。两者都躺在 `pending_effects` 队列里，要等 `update` 的
    /// `finish_update` → `flush_effects` 才按 FIFO 依次跑掉（激活排在 emit 前面）。
    /// 在闭包里读，activate 还没执行，收方一律是 0 —— 这条测试第一版就是这么红的。
    #[test]
    fn a_notification_reaches_every_subscriber() {
        let cx = TestAppContext::single();
        let (first, second) = cx.update(|cx| {
            let first = cx.new(Counter::new);
            let second = cx.new(Counter::new);
            notify(cx);
            notify(cx);
            // 实体在这里交出去：出了闭包它们仍被持有，订阅才活得过效果冲写。
            (first, second)
        });
        cx.update(|cx| {
            assert_eq!(first.read(cx).hits.get(), 2, "两条广播都该收到");
            assert_eq!(second.read(cx).hits.get(), 2);
        });
    }

    /// 两个视图必须订阅到**同一个**总线，否则广播各说各话，永远不会互相唤醒 ——
    /// `install` 的幂等性就是钉这件事。
    #[test]
    fn the_handle_is_stable() {
        let cx = TestAppContext::single();
        cx.update(|cx| {
            let first: gpui::Entity<ProjectBus> = handle(cx);
            assert_eq!(first.entity_id(), handle(cx).entity_id());
            // 再显式装一次也不该换实例。
            install(cx);
            assert_eq!(first.entity_id(), handle(cx).entity_id());
        });
    }
}
