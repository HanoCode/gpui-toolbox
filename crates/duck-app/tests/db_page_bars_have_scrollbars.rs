//! 数据库页的**固定横条**必须自适应：放不下时横向滚动并给可拖动的滚动条。
//!
//! gpui 的滚动容器不画系统滚动条 —— 只写 `overflow_x_scroll` 的话，内容多到放不下
//! 虽然能滚（滚轮 / 触控板），但用户面前没有任何**可拖动**的东西，也看不出右边还有
//! 东西。修法与结果网格（`tests/db_grid_horizontal_scroll.rs`）同一套形态：滚动容器
//! 挂**跨帧持久**的 `ScrollHandle`（`track_scroll`），相对容器里以 absolute 兄弟节点
//! 叠加 `Scrollbar::horizontal(..).mode(ScrollbarMode::Hover)` —— **悬停在滚动区 /
//! 滑块上才现身**（用户实测反馈：常驻档太吵）；内容网格的常驻档
//!（`render_result_columns` 的 `ScrollbarMode::Always`）是另一档位，不在本闸门内。
//!
//! 覆盖四条（都曾是「固定行直接溢出被窗口裁掉、无滚动条」的重灾区）：
//!
//! - 顶部连接标签条（`render_tabs.rs`，句柄 `tab_strip_x_scroll`）；
//! - 结果区结果标签行（`grid.rs::render_result_resbar`，句柄 `result_tabs_x_scroll`）；
//! - 全局操作条（`shell.rs::render_global_actions`，句柄 `global_actions_x_scroll`）；
//! - 编辑器工具条（`render_editor.rs::render_editor_toolbar`，句柄
//!   `editor_toolbar_x_scroll`）。
//!
//! 右组关键控件（AI 入口 / 连接读数 / Tx 徽标 / 连接与库下拉）**不进滚动区**——窄窗口
//! 下也要一眼可见。
//!
//! 判据是**源码文本**（`duck-platform` 的 `ui_never_uses_blocking_file_dialogs.rs`
//! 同一个理由：不值得为守这条纪律在类型系统上开口子）。注释行不算。
//! 曾有回归：标签条只写 `overflow_x_scroll` 没叠 `Scrollbar`，用户点名报
//! 「需要出现滚动条，方便用户拖动」；全局操作条 / 编辑器工具条在窄窗口下整排被裁
//!（截屏实证「比较数据」半个按钮悬在窗口边缘）—— 本闸门防它们静默退回。

use std::path::{Path, PathBuf};

/// 仓库根目录：`tests/` 在 `crates/duck-app/` 下，往上两层即可。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root should be reachable from the crate directory")
}

/// 摘掉注释行 / 行内注释后的源码文本（守卫只认代码）。
fn code_of(file: &Path) -> String {
    let text = std::fs::read_to_string(file)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 必须同时出现的接线片段：滚动容器挂持久句柄 + 叠加悬停档横向滚动条。
/// 元组 = (文件, Scrollbar 接线, 滚动句柄字段名)。
const WIRES: [(&str, &str, &str); 4] = [
    (
        "crates/duck-app/src/pages/db/render_tabs.rs",
        "Scrollbar::horizontal(&self.tab_strip_x_scroll)",
        "tab_strip_x_scroll",
    ),
    (
        "crates/duck-app/src/pages/db/grid.rs",
        "Scrollbar::horizontal(&self.result_tabs_x_scroll)",
        "result_tabs_x_scroll",
    ),
    (
        "crates/duck-app/src/pages/db/shell.rs",
        "Scrollbar::horizontal(&self.global_actions_x_scroll)",
        "global_actions_x_scroll",
    ),
    (
        "crates/duck-app/src/pages/db/render_editor.rs",
        "Scrollbar::horizontal(&self.editor_toolbar_x_scroll)",
        "editor_toolbar_x_scroll",
    ),
];

#[test]
fn db_fixed_bars_overlay_draggable_scrollbars() {
    let root = repo_root();
    let mut missing = Vec::new();
    for (path, wire, handle) in WIRES {
        let code = code_of(&root.join(path));
        // 滚动容器把偏移写给句柄；少这一半滚动条读不到位置。
        if !code.contains(&format!(".track_scroll(&self.{handle})")) {
            missing.push(format!("{path}: 缺 .track_scroll(持久句柄)"));
        }
        // 滚动条本体：absolute 叠加、横向、**悬停档**（用户实测反馈：常驻太吵 ——
        // 拖到滚动区 / 滑块上才现身；内容网格的常驻档不在此列，见
        // `grid.rs::render_result_columns` 的 `ScrollbarMode::Always`）。
        if !code.contains(wire) || !code.contains("ScrollbarMode::Hover") {
            missing.push(format!("{path}: 缺 {wire} + ScrollbarMode::Hover"));
        }
    }
    // 句柄必须是页面字段并跨帧持久初始化（每帧新建 = 滚轮偏移下一帧就丢）。
    let shell = code_of(&root.join("crates/duck-app/src/pages/db/shell.rs"));
    for handle in [
        "tab_strip_x_scroll",
        "result_tabs_x_scroll",
        "global_actions_x_scroll",
        "editor_toolbar_x_scroll",
    ] {
        if !shell.contains(&format!("{handle}: ScrollHandle::new()")) {
            missing.push(format!("shell.rs: 缺 {handle} 初始化"));
        }
    }
    assert!(
        missing.is_empty(),
        "固定横条少了可拖动的横向滚动条（gpui 不画系统滚动条，必须自己叠 Scrollbar）：\n{}\n\
         形态对照 grid.rs 结果网格：track_scroll + relative 容器内 absolute 叠 \
         Scrollbar::horizontal(..).mode(ScrollbarMode::Hover)。",
        missing.join("\n")
    );
}
