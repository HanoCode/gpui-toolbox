//! 仓库自带插件（`plugins/`）必须能被当前解析器装载。
//!
//! 这是**发行闸门**：`plugins/` 下的每个目录都会随 debug 构建被扫描，清单
//! 解析失败＝工具静默消失（Rust loader 刻意宽容，只记 warn）。本测试把
//! 「宽容运行」补上一条硬门禁——凡是随仓库分发的清单，必须在此全绿。
//!
//! 注意路径是 CARGO_MANIFEST_DIR 相对的仓库根，因此本测试只在**仓库内**
//! 跑 `cargo test -p duck-plugin` 时有意义（发布 crate 后跳过）。

use std::path::PathBuf;

fn repo_plugins_dir() -> Option<PathBuf> {
    // 测试产物：crates/duck-plugin/ → 仓库根两层之上。
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("plugins");
    dir.is_dir().then_some(dir)
}

#[test]
fn every_repo_plugin_manifest_parses() {
    let Some(dir) = repo_plugins_dir() else {
        eprintln!("plugins/ 不存在（crate 单独发布场景），跳过");
        return;
    };
    let mut checked = 0usize;
    let mut failures = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read plugins dir")
        .map(|entry| entry.expect("read entry").path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    for plugin_dir in entries {
        let manifest_path = plugin_dir.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        checked += 1;
        match duck_plugin::PluginManifest::load(&manifest_path) {
            Ok(manifest) => {
                // [io] 契约本身也要过校验（parse 已含 validate，这里显式断言一次）。
                for tool in &manifest.tools {
                    if let Some(io) = &tool.io {
                        io.validate(manifest.api_version).unwrap_or_else(|error| {
                            failures.push(format!("{} / {}: {error}", manifest.id, tool.id))
                        });
                    }
                }
            }
            Err(error) => failures.push(format!("{}: {error}", manifest_path.display())),
        }
    }
    assert!(
        checked > 0,
        "plugins/ 里没有任何 plugin.toml —— 要么目录被移空，要么本测试的路径假设失效"
    );
    assert!(
        failures.is_empty(),
        "随仓库分发的插件清单解析失败（发行闸门）：\n{}",
        failures.join("\n")
    );
}
