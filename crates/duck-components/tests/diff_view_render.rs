//! 差异渲染器的无头绘制守卫。
//!
//! 差异视图是**唯一一个**会在 `uniform_list` 里画两栏的组件：行号槽宽度、两侧
//! 缺内容的占位、`Gap` 行的居中、以及 `hit` 片段的底色都在这儿。这些分支写错了
//! 不会报错——只会画歪或画出空框。所以这里真的挂进窗口、真的布局 + 绘制一遍：
//! 任何一处 `unwrap` / 越界 / 布局断言失败都会红。
//!
//! 第二组用 5000 行验证「固定行高 + 虚拟滚动」的前提没被破坏：`uniform_list` 若
//! 退回全量布局，这一组会明显变慢（本测试不测时间，但能挡住「把行高改成随内容
//! 自适应」这类让虚拟滚动失效的改动——那样 `uniform_list` 会 panic 或错位）。

use std::rc::Rc;

use duck_components::content::{
    DiffLine, DiffLineKind, DiffSection, DiffSegment, DiffSummary, diff_summary, diff_view,
};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    UniformListScrollHandle, VisualTestContext, Window, div, px, size,
};
use gpui_component::{Root, init as component_init, v_flex};

/// 被测视图：把差异渲染器塞进一个 900×600 的面板。
struct Harness {
    sections: Rc<Vec<DiffSection>>,
    scroll: UniformListScrollHandle,
    summary: DiffSummary,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(900.))
            .h(px(600.))
            .child(diff_summary(self.summary, cx))
            .child(div().w_full().flex_1().min_h_0().child(diff_view(
                "test-diff".into(),
                self.sections.clone(),
                self.scroll.clone(),
                cx,
            )))
    }
}

fn harness(cx: &mut TestAppContext, view: Harness) -> &mut VisualTestContext {
    cx.update(|cx| {
        component_init(cx);
        duck_theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let entity = cx.new(|_| view);
        Root::new(entity, window, cx)
    });
    cx.simulate_resize(size(px(900.), px(600.)));
    draw(cx);
    cx
}

/// 绘一帧：`window.draw` 让 prepaint（虚拟列表量尺）与 paint 都真的跑过。
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

fn line(
    kind: DiffLineKind,
    old_no: Option<u32>,
    new_no: Option<u32>,
    old: &str,
    new: &str,
) -> DiffLine {
    DiffLine {
        kind,
        old_no,
        new_no,
        old: vec![DiffSegment::plain(old)],
        new: vec![DiffSegment::plain(new)],
    }
}

#[test]
fn draws_every_row_kind_without_panicking() {
    let sections = Rc::new(vec![DiffSection {
        title_a: "a/pom.xml".into(),
        title_b: "b/pom.xml".into(),
        lines: vec![
            line(DiffLineKind::Equal, Some(1), Some(1), "same", "same"),
            line(DiffLineKind::Delete, Some(2), None, "gone", ""),
            line(DiffLineKind::Insert, None, Some(2), "", "added"),
            DiffLine {
                kind: DiffLineKind::Change,
                old_no: Some(3),
                new_no: Some(3),
                old: vec![DiffSegment::plain("version="), DiffSegment::changed("1.0")],
                new: vec![DiffSegment::plain("version="), DiffSegment::changed("2.0")],
            },
            DiffLine::gap(),
            // 行号缺一截（真实数据里统一 diff 的第一段就是这种）。
            line(
                DiffLineKind::Equal,
                None,
                None,
                "no line numbers",
                "no line numbers",
            ),
        ],
    }]);
    let mut app = TestAppContext::single();
    let cx = harness(
        &mut app,
        Harness {
            sections,
            scroll: UniformListScrollHandle::new(),
            summary: DiffSummary {
                added: 2,
                removed: 1,
                modified: 1,
            },
        },
    );
    // 再画一帧：`uniform_list` 的首次绘制只量不改，第二帧才走到「真的取行」。
    draw(cx);
}

#[test]
fn draws_thousands_of_rows_with_virtual_scrolling() {
    let mut lines = Vec::new();
    for index in 0..5000u32 {
        lines.push(line(
            DiffLineKind::Equal,
            Some(index + 1),
            Some(index + 1),
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox jumps over the lazy dog",
        ));
    }
    let mut app = TestAppContext::single();
    let cx = harness(
        &mut app,
        Harness {
            sections: Rc::new(vec![DiffSection {
                title_a: String::new(),
                title_b: String::new(),
                lines,
            }]),
            scroll: UniformListScrollHandle::new(),
            summary: DiffSummary::default(),
        },
    );
    draw(cx);
}
