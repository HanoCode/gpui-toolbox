//! 开源许可窗口：把构建期内嵌的第三方许可汇总整份展示出来。
//!
//! # 为什么内嵌而不是外链或随包分发
//!
//! 合规文本有个硬要求：**离线可查**。外链（指向 GitHub 上的 NOTICE）在断网、内网、
//! 上游仓库改名三种情况下都会失效；随安装包分发一个 `.md` 则要同时照顾 macOS 的
//! `.app/Contents/Resources`、Windows 的安装目录与开发态的 `target/` 三套路径，
//! 少一套就出现「开发时能看、装完看不到」。
//!
//! 所以整份文件走 `include_str!` 进二进制（见 `sbin/licenses.py` 生成它）。
//!
//! # 1.2 MiB 的单段文本会不会卡
//!
//! 这个是量过的，不是拍的：无头渲染里首帧 0.54 ms、次帧 0.23 ms，**自动换行与
//! 不换行同量级**，所以这里用换行排版（许可正文横着滚很难读）。
//! 测量与回归守卫在 `crates/duck-app/tests/licenses_view.rs`。
//!
//! # 只做「去 Markdown 骨架」，不引 Markdown 渲染器
//!
//! 那份文件是给人和 CI 读的 Markdown（表头、锚点、代码围栏）。查看器只把**骨架**
//! 拿掉——锚点行、代码围栏、标题的 `#` 前缀——正文一字不改。引一个 Markdown 渲染器
//! 去渲染 25 000 行、还只为了让 `##` 变成粗体，代价与收益不成比例
//! （同一取舍见设置页的「版本发布记录」）。

use std::sync::OnceLock;

use duck_components::{content, meta_text, selectable_text};
use duck_shell::AuxiliaryWindowView;
use duck_theme::{active_theme, tr, trf};
use gpui::{
    AnyElement, App, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, px, size,
};
use gpui_component::{Root, scroll::ScrollableElement as _, v_flex};

use crate::app::{Desktop, WindowKind};
use crate::pages::status_support::format_bytes;
use crate::settings::{self, AuxWindow};

/// 窗口初始尺寸。
const LICENSES_WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(920.), px(700.));
/// 窗口最小尺寸。
const LICENSES_WINDOW_MIN: gpui::Size<gpui::Pixels> = size(px(560.), px(420.));

/// 构建期内嵌的第三方许可汇总（由 `sbin/licenses.py` 生成）。
const DOC: &str = include_str!("../../../../assets/licenses/THIRD_PARTY_LICENSES.md");

/// 汇总文件首行的机器可读前缀。
const META_PREFIX: &str = "<!-- duck-licenses:";

/// 机器可读统计（`sbin/licenses.py` 写在首行，见该脚本的 `render`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// 第三方组件数。
    pub components: usize,
    /// 许可全文份数（按内容去重）。
    pub texts: usize,
    /// 许可声明种类数（SPDX 表达式的不同写法个数）。
    pub expressions: usize,
}

impl Stats {
    /// 解析 `<!-- duck-licenses: components=849 texts=409 expressions=39 -->`。
    ///
    /// 解析失败返回 `None`（而不是 0）：查看器宁可少显示一行统计，
    /// 也不该把「压根没解析出来」显示成「0 个组件」那种假事实。
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let body = line.trim().strip_prefix(META_PREFIX)?.strip_suffix("-->")?;
        let mut stats = Self {
            components: 0,
            texts: 0,
            expressions: 0,
        };
        let mut seen = 0;
        for field in body.split_whitespace() {
            let (key, value) = field.split_once('=')?;
            let value: usize = value.parse().ok()?;
            match key {
                "components" => stats.components = value,
                "texts" => stats.texts = value,
                "expressions" => stats.expressions = value,
                _ => continue,
            }
            seen += 1;
        }
        (seen == 3).then_some(stats)
    }
}

/// 内嵌汇总的统计；解析不出来时为 `None`。
pub fn stats() -> Option<Stats> {
    static CACHE: OnceLock<Option<Stats>> = OnceLock::new();
    *CACHE.get_or_init(|| DOC.lines().next().and_then(Stats::parse))
}

/// 展示用正文：去掉首行机器元数据，并拿掉 Markdown 骨架。
fn document() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut out = String::with_capacity(DOC.len());
        for line in DOC.lines() {
            let line = line.trim_end();
            // 首行的机器元数据、段落锚点、代码围栏：都是给解析器看的骨架。
            if line.starts_with(META_PREFIX)
                || line.starts_with("<a id=")
                || line == "```text"
                || line == "```"
            {
                continue;
            }
            out.push_str(strip_heading(line));
            out.push('\n');
        }
        out
    })
}

/// 去掉标题的 `#` 前缀：层级由正文内容自己说明，`###` 读出来只是三个井号。
fn strip_heading(line: &str) -> &str {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        &line[hashes + 1..]
    } else {
        line
    }
}

/// 打开（或聚焦）开源许可窗口。
pub fn open_licenses(cx: &mut App) {
    cx.defer(open_or_focus);
}

/// 窗口已开着则前置激活，否则新建（路径与日志 / 设置窗口一致）。
fn open_or_focus(cx: &mut App) {
    if focus_open(cx) {
        return;
    }

    let mut options = duck_shell::auxiliary_window_options(SharedString::from(tr(cx, "开源许可")));
    // 上次的位置尺寸（已夹回可见区域）；没有存档则主屏居中。
    options.window_bounds = Some(settings::aux_window_bounds(
        cx,
        AuxWindow::Licenses,
        LICENSES_WINDOW_SIZE,
    ));
    options.window_min_size = Some(LICENSES_WINDOW_MIN);

    match cx.open_window(options, |window, cx| {
        let content = cx.new(|_| LicensesView);
        let chrome = cx.new(|cx| {
            // 占位标题：真实文案由 with_title_fn 按语言解析，此处不会被渲染。
            AuxiliaryWindowView::new("Licenses", content.into(), cx)
                .with_title_fn(|cx| SharedString::from(tr(cx, "开源许可")))
                // 几何变化 → 去抖持久化（与主窗口同形）。
                .on_window_bounds_changed(|window, cx| {
                    settings::save_aux_bounds(AuxWindow::Licenses, window, cx);
                })
        });
        crate::app::observe_system_appearance(window);
        cx.new(|cx| Root::new(chrome, window, cx))
    }) {
        Ok(handle) => {
            Desktop::set_window(cx, WindowKind::Aux(AuxWindow::Licenses), handle);
            // 记下「这个窗口开着」，供下次启动恢复（见「设置 → 行为」的开关）。
            Desktop::remember_aux_open(cx);
            cx.activate(true);
        }
        Err(error) => crate::app::report_window_failure("开源许可窗口", &error, cx),
    }
}

/// 已开着的许可窗口前置激活；没有、或已经关掉了则返回 `false`（顺手清掉死句柄）。
///
/// 形状照 `pages::logs::focus_open_logs`：`let ... else` + 独立 `if`，
/// 而不是 `if let { if ... }`（后者会被 clippy 判为可折叠）。
fn focus_open(cx: &mut App) -> bool {
    let Some(handle) = Desktop::window_handle(cx, WindowKind::Aux(AuxWindow::Licenses)) else {
        return false;
    };
    if handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok()
    {
        cx.activate(true);
        true
    } else {
        Desktop::clear_window(cx, WindowKind::Aux(AuxWindow::Licenses));
        false
    }
}

/// 许可正文视图：一份可滚动的长文本 + 底部统计。
struct LicensesView;

impl Render for LicensesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = active_theme(cx).tokens().clone();

        let left: Vec<AnyElement> = stats()
            .map(|stats| {
                vec![
                    meta_text(
                        trf(
                            cx,
                            "{} 个组件 · {} 份许可全文 · {} 种许可声明",
                            &[
                                &stats.components.to_string(),
                                &stats.texts.to_string(),
                                &stats.expressions.to_string(),
                            ],
                        ),
                        cx,
                    )
                    .into_any_element(),
                ]
            })
            .unwrap_or_default();
        let right: Vec<AnyElement> =
            vec![meta_text(format_bytes(DOC.len() as u64), cx).into_any_element()];

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(tokens.colors.surface)
            .child(
                div()
                    .id("licenses-doc")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // 只给纵向：正文自动换行，横向不会溢出。
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .w_full()
                            .px(tokens.spacing.lg)
                            .py(tokens.spacing.md)
                            .font_family(MONO_FAMILY)
                            .text_size(tokens.typography.xs.size)
                            .line_height(tokens.typography.xs.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(document())),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .child(content::status_bar(left, right, cx)),
            )
    }
}

/// 正文等宽字体族：许可正文里的表格与缩进靠等宽才对得齐。
#[cfg(target_os = "macos")]
const MONO_FAMILY: &str = "Menlo";
/// 正文等宽字体族（Windows）。
#[cfg(not(target_os = "macos"))]
const MONO_FAMILY: &str = "Consolas";

#[cfg(test)]
mod tests {
    use super::{DOC, META_PREFIX, Stats, document, stats, strip_heading};

    /// 首行的机器可读统计**必须能解析**——这是「生成的写端」与「内嵌的读端」之间
    /// 唯一的契约，改了 `sbin/licenses.py` 的格式而没同步改这里，就靠这条炸出来。
    #[test]
    fn embedded_header_parses_into_stats() {
        let stats = stats().expect("首行统计应可解析");
        assert!(stats.components > 0, "组件数不该为 0");
        assert!(stats.texts > 0, "全文份数不该为 0");
        assert!(stats.expressions > 0, "许可声明种类不该为 0");
        assert!(
            stats.texts <= stats.components,
            "去重后的全文份数不该多于组件数：{} > {}",
            stats.texts,
            stats.components
        );
    }

    /// 统计里的全文份数要与正文里实际的段落数一致：写端只报数，读端靠段落切分，
    /// 两边对不上就说明有一方算错了。
    #[test]
    fn reported_text_count_matches_the_document() {
        let stats = stats().expect("首行统计应可解析");
        let sections = DOC.lines().filter(|line| line.starts_with("### ")).count();
        assert_eq!(stats.texts, sections, "统计里的全文份数与正文段落数不一致");
    }

    /// 展示用正文不得再含机器元数据与 Markdown 骨架。
    #[test]
    fn displayed_document_drops_the_markdown_scaffolding() {
        let shown = document();
        // 标题的 `#` 也一并去掉（连 H1 在内），所以首行是标题正文本身。
        assert!(!shown.starts_with('#'), "展示正文不该以 `#` 开头");
        assert!(!shown.contains(META_PREFIX), "展示正文不该带机器元数据");
        assert!(!shown.contains("<a id="), "展示正文不该带段落锚点");
        assert!(!shown.contains("```"), "展示正文不该带代码围栏");
        assert!(!shown.contains("\n### "), "展示正文的标题应去掉 `#` 前缀");
        // 去骨架不能连正文一起去掉。
        assert!(shown.contains("MIT License"), "许可正文应当保留");
    }

    /// 元数据解析：字段缺失或值非法一律判失败，不返回半份统计。
    #[test]
    fn stats_parse_rejects_incomplete_metadata() {
        assert!(Stats::parse("<!-- duck-licenses: components=1 texts=2 -->").is_none());
        assert!(
            Stats::parse("<!-- duck-licenses: components=x texts=2 expressions=3 -->").is_none()
        );
        assert!(Stats::parse("普通的一行").is_none());
        assert_eq!(
            Stats::parse("<!-- duck-licenses: components=1 texts=2 expressions=3 -->"),
            Some(Stats {
                components: 1,
                texts: 2,
                expressions: 3
            })
        );
    }

    /// `strip_heading` 只吃「井号 + 空格」这种真标题，不碰 `#!` 之类正文。
    #[test]
    fn strip_heading_only_touches_heading_markers() {
        assert_eq!(strip_heading("## 组件清单"), "组件清单");
        assert_eq!(strip_heading("# 标题"), "标题");
        assert_eq!(strip_heading("#没有空格"), "#没有空格");
        assert_eq!(strip_heading("####### 七个井号"), "####### 七个井号");
        assert_eq!(strip_heading("| a | b |"), "| a | b |");
    }
}
