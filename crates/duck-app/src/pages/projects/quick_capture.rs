//! 快速捕获窗 —— 在任意程序里按 `⌥⌘N` 记一条任务（G-PRJ-12）。
//!
//! 与「随手看」（[`super::float`]）是**两种窗口语义**：
//!
//! | | 随手看 / 日历浮窗 | 快速捕获窗 |
//! |---|---|---|
//! | 焦点 | `focus: false`（瞄一眼，不切走前台） | `focus: true`（要打字） |
//! | 键 | 再按一次收起 | 回车记下就走 |
//! | 内容 | 列表 / 日历 | 一行输入 + 解析预览 |
//!
//! # 为什么是独立窗口而不是主窗内的弹窗
//!
//! 页内弹窗（`dialogs::open_quick_capture`）要求先把主窗切到前台、再切到项目管理页 ——
//! 对「正在写代码时想起一件事」这个场景，两次切换比记这件事本身更贵。独立窗（
//! `WindowKind::PopUp` + 全局键）可以不改前台程序就把字收下。两者**共用同一套记号语法**
//! （[`super::quickparse`]），所以在哪个入口记都落成同一种任务。
//!
//! # 三个刻意的取舍
//!
//! 1. **保存后关窗**（用户拍板）：这是 Spotlight 语义 —— 记完即走。留在原地连续记多条
//!    听上去更快，但那种用法真正需要的是「清单」而不是「捕获」，在 620pt 的小窗里
//!    只会挤成一团。
//! 2. **不选项目就进收集箱**（`project` 为 `None`）：与随手看同一个口径 —— 匹配不上
//!    就归收集箱，宁可稍后在主窗归类，也不替用户猜。
//! 3. **解析预览是只读的一行字**，不是可点的 chips：这一窗的键盘路径只有「打字 → 回车 →
//!    关窗」，中间插进可点元素只会让 Tab 顺序变复杂。

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Styled, Subscription, Window,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, div, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::v_flex;

use duck_project::{ArchiveScope, NewTask, ProjectStore};
use duck_theme::{active_theme, tr, trf};

use super::bus;
use super::quickparse::{self, QuickParse};
use super::state::local_today;

/// 窗口尺寸：一行输入 + 一行预览。高度定死（不可缩放），宽度够放下一个长标题。
const CAPTURE_SIZE: gpui::Size<gpui::Pixels> = size(px(620.), px(112.));

/// 窗口选项。
///
/// 与两个浮窗的差别只有两处：`focus: true`（必须能直接打字）和
/// `app_owns_titlebar_drag: true`（整窗都是可点内容，顶部那 28pt 不能被系统当成拖拽区
/// 吃掉 —— 否则标题一长，第一行就点不着了）。
///
/// `WindowKind::PopUp` 保持一致：macOS 上是非激活面板，不会进 Dock、不会抢主窗的
/// 舞台管理分组。
#[must_use]
pub(crate) fn quick_capture_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::centered(CAPTURE_SIZE, cx)),
        titlebar: None,
        kind: gpui::WindowKind::PopUp,
        is_resizable: false,
        is_minimizable: false,
        is_movable: false,
        app_owns_titlebar_drag: true,
        // 透明底才能露出卡片自己的圆角与系统阴影。
        window_background: WindowBackgroundAppearance::Transparent,
        focus: true,
        show: true,
        ..WindowOptions::default()
    }
}

/// 快速捕获窗。
pub struct QuickCaptureWindow {
    /// 唯一的一行输入。
    input: Entity<InputState>,
    /// 输入框事件的订阅（Change 刷新预览、PressEnter 落库）。
    ///
    /// **持有即有效**：`gpui` 的订阅在 Drop 时退订，所以字段本身从不被读也不该删 ——
    /// 删掉就是「输入框还在，但回车没反应」。
    #[allow(dead_code)]
    sub: Option<Subscription>,
    /// 当前这句话解析出什么（随输入实时更新）。
    preview: QuickParse,
    /// 出错时的一行回执（成功不需要 —— 窗已经关了）。
    hint: Option<SharedString>,
}

impl QuickCaptureWindow {
    /// 建窗。输入框在下一轮 effect 之后拿焦点（渲染期改焦点会被 gpui 挡下）。
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "记一条：标题 #标签 @项目 !P1 明天 18:00"))
        });
        let sub = cx.subscribe(
            &input,
            |this: &mut Self, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.preview = quickparse::parse(&text, local_today());
                    // 改了字，上一条错误就不再适用于这一句。
                    this.hint = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let entity = cx.entity();
                    // 派发期不直接 `update`（沿用浮窗那条路：先 `defer` 到下一轮）。
                    crate::update::on_notify_window(cx, move |window, cx| {
                        entity.update(cx, |this, cx| this.submit(window, cx));
                    });
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        let focus = input.clone();
        cx.defer_in(window, move |_this, window, cx| {
            window.focus(&focus.focus_handle(cx), cx);
        });
        Self {
            input,
            sub: Some(sub),
            preview: QuickParse::default(),
            hint: None,
        }
    }

    /// 回车：落库 → 关窗。空输入直接忽略（回车只是确认，不是「记一条空任务」）。
    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let parsed = quickparse::parse(&text, local_today());
        if parsed.title.is_empty() {
            // 只打了记号没有标题：这多半是手滑，但**不能静默**（关掉窗会让人以为记上了）。
            self.hint = Some(tr(cx, "只有记号、没有标题，这一条没有记下来。").into());
            cx.notify();
            return;
        }
        match create(&parsed) {
            Ok(()) => {
                bus::notify(cx);
                crate::app::close_quick_capture(cx);
            }
            Err(error) => {
                self.hint = Some(trf(cx, "新建失败：{}", &[&error.to_string()]).into());
                cx.notify();
            }
        }
    }

    /// 预览行：把解析结果拼成一句人话；没有任何记号时给快捷键提示。
    fn preview_text(&self, cx: &App) -> SharedString {
        let parsed = &self.preview;
        let mut parts: Vec<String> = Vec::new();
        if let Some(project) = &parsed.project {
            parts.push(format!("@{project}"));
        }
        if let Some(priority) = parsed.priority {
            parts.push(format!("!P{}", priority.as_i64()));
        }
        if let Some(due) = parsed.due {
            parts.push(due.to_iso());
        }
        if let Some(time) = &parsed.due_time {
            parts.push(time.clone());
        }
        for tag in &parsed.tags {
            parts.push(format!("#{tag}"));
        }
        if parts.is_empty() {
            return tr(cx, "回车记下 · Esc 关闭").into();
        }
        SharedString::from(parts.join(" · "))
    }
}

/// 把解析结果落成一条任务。
///
/// 项目按名字匹配（`@Alpha`，大小写不敏感）；匹配不上就进收集箱 —— 与随手看同一个口径。
/// 标签走 `ensure_tag`（不存在就建），所以 `#后端` 可以第一次就写。
fn create(parsed: &QuickParse) -> duck_project::Result<()> {
    let store = ProjectStore::open_default()?;
    let project_id = match &parsed.project {
        Some(name) => store
            .list_projects(ArchiveScope::Live)?
            .into_iter()
            .find(|project| project.name.eq_ignore_ascii_case(name))
            .map(|project| project.id),
        None => None,
    };
    let mut tag_ids = Vec::new();
    for name in &parsed.tags {
        tag_ids.push(store.ensure_tag(name, duck_project::DEFAULT_PROJECT_COLOR)?);
    }
    let mut new = NewTask::new(parsed.title.clone()).tags(tag_ids);
    if let Some(project_id) = project_id {
        new = new.project(project_id);
    }
    if let Some(priority) = parsed.priority {
        new = new.priority(priority);
    }
    if parsed.due.is_some() {
        new = new.dates(None, parsed.due);
    }
    if let Some(time) = &parsed.due_time {
        new = new.due_time(time.clone());
    }
    store.create_task(new)?;
    Ok(())
}

impl Render for QuickCaptureWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let radius = theme.components().dialog_radius;
        let preview = self.preview_text(cx);
        let hint = self.hint.clone();

        v_flex()
            .id("duck-quick-capture")
            .size_full()
            .gap(tokens.spacing.sm)
            .p(tokens.spacing.lg)
            .rounded(radius)
            .border(extended.stroke.thin)
            .border_color(extended.colors.hairline)
            .bg(tokens.colors.surface)
            // Esc 收起。输入框对 escape 的处理最终会 `cx.propagate()`（它没开
            // `clean_on_escape`），所以这里能接住。
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    crate::app::close_quick_capture(cx);
                }
            }))
            .child(Input::new(&self.input).appearance(false))
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(tokens.typography.xs.size)
                    .line_height(tokens.typography.xs.line_height)
                    .text_color(if hint.is_some() {
                        tokens.colors.destructive
                    } else {
                        tokens.colors.muted_foreground
                    })
                    .child(hint.unwrap_or(preview)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预览只反映记号，不反映标题 —— 标题就在输入框里，重复一遍只会挤掉记号。
    #[test]
    fn the_preview_lists_only_the_markers() {
        let parsed = quickparse::parse(
            "修登录超时 #后端 @Alpha !P1 明天 18:00",
            duck_project::CalendarDate::new(2026, 10, 17).expect("合法日期"),
        );
        let parts = {
            let mut parts: Vec<String> = Vec::new();
            if let Some(project) = &parsed.project {
                parts.push(format!("@{project}"));
            }
            if let Some(priority) = parsed.priority {
                parts.push(format!("!P{}", priority.as_i64()));
            }
            if let Some(due) = parsed.due {
                parts.push(due.to_iso());
            }
            if let Some(time) = &parsed.due_time {
                parts.push(time.clone());
            }
            for tag in &parsed.tags {
                parts.push(format!("#{tag}"));
            }
            parts.join(" · ")
        };
        assert_eq!(parts, "@Alpha · !P1 · 2026-10-18 · 18:00 · #后端");
        assert!(!parts.contains("修登录超时"), "标题不进预览：{parts}");
    }

    /// 只有记号没有标题时不该落库（`parse` 已经把它剥空了，这里钉住这个前提）。
    #[test]
    fn a_marker_only_line_has_no_title() {
        let parsed = quickparse::parse(
            "#后端 !P1",
            duck_project::CalendarDate::new(2026, 10, 17).expect("合法日期"),
        );
        assert!(parsed.title.is_empty());
        assert!(parsed.has_markers());
    }
}
