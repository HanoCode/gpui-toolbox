//! Redis 键列表的**渲染回归**：uniform_list 必须直接挂在 flex 容器下。
//!
//! 背景（2026-10-15 线上缺陷）：键列表被包在 `div().flex_1().min_h_0()`（普通
//! div，默认 display:block）里，`uniform_list` 在 gpui-pre 0.3.8 下量不出高度、
//! **整表不渲染**——表现为「连接成功、状态栏 45/45 键、无任何报错、界面空白」，
//! 平铺 / 树形两种形态都中招。修复：把列表直接挂到 `v_flex` 下（db 页同款）。
//!
//! 两条断言：
//! 1. 修复后的容器链（列表直接挂 `v_flex`）下，行必须可见；
//! 2. 钉死坑本身：同样的列表包进 block div 就必须消失（防止回潮）。

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement, Render, Styled, TestAppContext,
    UniformListScrollHandle, VisualTestContext, Window, div, px, uniform_list,
};
use gpui_component::{h_flex, init as component_init, v_flex};

/// 与 `pages/redis/browser.rs` 同款行高。
const KEY_ROW_HEIGHT: f32 = 24.;
const ROW_COUNT: usize = 45;
const PROBE_ROW: usize = 3;

/// 行结构对照真实代码：`h_flex().id(..).h(KEY_ROW_HEIGHT).w_full().flex_none()`。
fn key_row(index: usize) -> gpui::AnyElement {
    let probe = index == PROBE_ROW;
    let label = div()
        .flex_1()
        .min_w_0()
        .child(gpui::SharedString::from(format!("key-{index}")));
    let label = if probe {
        label.debug_selector(move || "redis-probe-row".to_string())
    } else {
        label
    };
    h_flex()
        .id(gpui::SharedString::from(format!("redis-key-{index}")))
        .h(px(KEY_ROW_HEIGHT))
        .w_full()
        .flex_none()
        .items_center()
        .child(label)
        .into_any_element()
}

/// `block_wrapper` = Some(()) 时把列表包进普通 div（缺陷形态），None = 修复形态。
struct KeyListReplica {
    block_wrapper: bool,
    scroll_handle: UniformListScrollHandle,
}

impl Render for KeyListReplica {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let scroll_handle = self.scroll_handle.clone();
        let list = uniform_list("redis-key-list", ROW_COUNT, move |range, _window, _cx| {
            range
                .filter(|index| *index < ROW_COUNT)
                .map(key_row)
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .track_scroll(&scroll_handle);

        let list = if self.block_wrapper {
            // 缺陷形态：block div 包裹（修复前 browser.rs:724 的写法）。
            div().flex_1().min_h_0().child(list).into_any_element()
        } else {
            // 修复形态：列表直接挂 v_flex（db 页同款）。
            list.into_any_element()
        };

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            // 工具条 / 类型条：固定高度，占住列表上方的两行。
            .child(h_flex().flex_none().w_full().h(px(40.)))
            .child(h_flex().flex_none().w_full().h(px(30.)))
            .child(list)
    }
}

fn add_replica_view(cx: &mut TestAppContext, block_wrapper: bool) -> &mut VisualTestContext {
    cx.update(component_init);
    let (_, cx) = cx.add_window_view(|_, _| KeyListReplica {
        block_wrapper,
        scroll_handle: UniformListScrollHandle::new(),
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx
}

/// 修复形态：行必须真的画出来（有宽、有行高、落在首屏）。
#[gpui::test]
fn key_list_renders_rows(cx: &mut TestAppContext) {
    let cx = add_replica_view(cx, false);
    let probe = cx
        .debug_bounds("redis-probe-row")
        .expect("修复形态下键行必须渲染");
    assert!(probe.size.width > px(100.), "键行宽度异常: {probe:?}");
    assert!(
        probe.size.height >= px(KEY_ROW_HEIGHT) && probe.size.height < px(KEY_ROW_HEIGHT * 2.),
        "键行高度异常: {probe:?}"
    );
    assert!(
        probe.origin.y >= px(70.),
        "首行应落在两条固定条之下: {probe:?}"
    );
}

/// 钉死坑：同样的列表包进 block div 就必须整体消失（gpui-pre 0.3.8）。
#[gpui::test]
fn block_div_wrapper_hides_uniform_list(cx: &mut TestAppContext) {
    let cx = add_replica_view(cx, true);
    assert!(
        cx.debug_bounds("redis-probe-row").is_none(),
        "block div 包裹下的 uniform_list 竟然渲染了？该坑已被上游修复，\
         请核对 gpui-pre 版本并考虑简化本回归"
    );
}
