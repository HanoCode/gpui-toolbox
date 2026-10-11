//! 跨平台路径契约：仓库里不许出现 Windows 保留设备名。
//!
//! 起因是一次真实的发版阻断：`crates/duck-shell/src/aux.rs` 里的 `aux` 与 `CON` / `NUL` /
//! `COM1` 同类，属 Windows 保留设备名。Windows 上 `git checkout` 会直接失败
//! （`error: invalid path 'crates/duck-shell/src/aux.rs'`），于是 CI 的 Windows 腿在
//! **检出阶段**就红了 —— 本地 macOS 开发全程无感，只有 CI 会说话，而它恰好又是打标签的
//! 前置条件之一。
//!
//! 这条守卫只做一件事：递归扫一遍仓库，任何一段路径的**主名**（去掉扩展名）命中保留名
//! 就报出来。它不依赖 git（检出后的目录树即事实），也不联网。

use std::path::{Path, PathBuf};

/// Windows 保留设备名。大小写不敏感，且**带扩展名同样非法**：`aux` / `aux.rs` / `NUL.txt`
/// 在 Windows 上一律不可用。
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 不参与扫描的目录：本地构建产物与上游参考源码，体量大且非本仓源码。
const SKIP_DIRS: &[&str] = &[
    "target",
    "temp",
    "dist",
    ".git",
    "__pycache__",
    "node_modules",
];

/// 仓库根：`crates/duck-platform` 往上两级。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> 的上一级必须是仓库根")
        .to_path_buf()
}

/// 路径的**主名**（去掉最后一个扩展名）是否命中保留名。
///
/// 只比主名而不是整串：`console.rs` / `auxiliary.rs` 这类以保留名开头的正常名字必须放行，
/// 否则守卫会变成「一有 console 就红」的噪声源。
fn is_reserved(component: &str) -> bool {
    let stem = match component.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => component,
    };
    RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name))
}

fn collect_offenders(dir: &Path, offenders: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // 目录不存在（CI 检出后某些本地目录本就没有）或不可读：不是本测试的关心点。
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if is_reserved(&name) {
            offenders.push(path.clone());
        }
        // 符号链接不跟进：仓库里出现目录环会让递归不终止。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            collect_offenders(&path, offenders);
        }
    }
}

#[test]
fn the_detector_recognises_reserved_names_and_lets_similar_ones_through() {
    // 反例（必须命中）：保留名本身、带扩展名、大小写混写。
    for hit in [
        "aux", "aux.rs", "AUX.rs", "con", "NUL.txt", "com1.rs", "Lpt9",
    ] {
        assert!(is_reserved(hit), "{hit} 应当被判为 Windows 保留名");
    }
    // 正例（必须放行）：以保留名开头或同名不同词的正常文件名。
    for miss in [
        "auxiliary.rs",
        "console.rs",
        "nulify.rs",
        "component.rs",
        "lib.rs",
        "aux_window.rs",
    ] {
        assert!(!is_reserved(miss), "{miss} 不该被判为保留名");
    }
}

#[test]
fn no_path_component_uses_a_windows_reserved_name() {
    let root = repo_root();
    let mut offenders = Vec::new();
    collect_offenders(&root, &mut offenders);

    assert!(
        offenders.is_empty(),
        "以下路径在 Windows 上无法检出（保留设备名），CI 的 Windows 腿会在 checkout 阶段失败：\n{}",
        offenders
            .iter()
            .map(|path| format!("    {}", path.strip_prefix(&root).unwrap_or(path).display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
