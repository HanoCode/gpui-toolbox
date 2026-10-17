//! 界面代码不许用会**重入事件循环**的同步文件对话框。
//!
//! `duck_platform::{save_file, open_file, open_files, pick_folder}` 走 rfd，在 macOS 上
//! 是**嵌套模态循环**（`NSOpenPanel` / `NSSavePanel` 的 `runModal`）。界面里能触发它们
//! 的动作全都在 gpui 的 `App::update` 里 —— 模态循环一开 AppKit 就把事件派发重入回来，
//! 而此刻 App 的 `RefCell` 还借在外面，重入期间任何前台任务走到
//! `AsyncApp::update_entity` 的 `borrow_mut()` 就直接 abort。
//!
//! 两次实证（同一条链）：
//!
//! - `crash-2026-10-15.log` 12:46 `import_csv → open_file → rfd pick_file → run_modal`；
//! - `crash-2026-10-16.log` 21:25 `DatabasePage::export_connections → save_file →
//!   NSSavePanel runModal`（重入期间邮件页的事件轮询器被调度 → panic）。
//!
//! 界面要开面板请走 `duck-app` 的 `file_dialogs` 模块（gpui 自己的
//! `prompt_for_paths` / `prompt_for_new_path`，`beginWithCompletionHandler` 真异步）。
//!
//! 判据是**源码文本**（与 `packaging_matches_the_code.rs` 同一个理由：不值得为了守这条
//! 纪律在类型系统上开口子）。注释行不算：这些函数名出现在说明里是应该的。
//!
//! `confirm` / `alert` 是**同一类雷**（`NSAlert runModal`），原本没进本闸门是因为迁移
//! 还没做完；现在界面侧已经全部改走 `crate::confirm` 的主题弹窗，所以一并纳入。
//!
//! - 破坏性确认 → `crate::confirm::{open_confirm, open_confirm_in}`；
//! - 文件 / 目录面板 → `crate::file_dialogs::*`。

use std::path::{Path, PathBuf};

/// 界面代码里禁止出现的 rfd 同步模态入口。
///
/// 前四个是文件 / 目录面板，后两个是消息框 —— 在 macOS 上都走嵌套模态循环。
const BANNED: [&str; 6] = [
    "save_file",
    "open_file",
    "open_files",
    "pick_folder",
    "confirm",
    "alert",
];

/// 仓库根目录：`tests/` 在 `crates/duck-platform/` 下，往上两层即可。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root should be reachable from the crate directory")
}

/// 递归收集 `dir` 下的 `.rs` 文件（顺序无关紧要，只有「有没有」要紧）。
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry should be readable").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// 这一行有没有碰到 `duck_platform` 的这个面板入口。
///
/// 比 `duck_platform::save_file` 松一档：`use duck_platform::{save_file, open_file};`
/// 这种写法也得拦住，所以判的是「同一行的代码部分里两个词都在」。
fn offending(code: &str, name: &str) -> bool {
    code.contains("duck_platform") && code.contains(name)
}

#[test]
fn ui_never_uses_the_blocking_file_dialogs() {
    let src = repo_root().join("crates/duck-app/src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        files.len() > 50,
        "只扫到 {} 个源文件，路径大概写错了：{}",
        files.len(),
        src.display()
    );

    let mut hits = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("source file should be readable");
        for (index, line) in text.lines().enumerate() {
            // 注释行（含 `///` / `//!` 文档注释）整体跳过。
            if line.trim_start().starts_with("//") {
                continue;
            }
            // 行内注释之后的内容不算代码。
            let code = line.split("//").next().unwrap_or("");
            if BANNED.iter().any(|name| offending(code, name)) {
                hits.push(format!("{}:{}: {}", file.display(), index + 1, line.trim()));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "界面代码里出现了 rfd 的同步模态 —— 它会重入事件循环，重入期间任何前台任务\n\
         做 `AsyncApp::update_entity` 都会撞「RefCell already borrowed」并 abort：\n{}\n\
         面板请改走 `crate::file_dialogs`，确认框请改走 `crate::confirm`。",
        hits.join("\n")
    );
}
