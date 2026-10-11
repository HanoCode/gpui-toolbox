//! 构建脚本：Windows 目标下把 `assets/duck.ico` 编译进 .exe 资源段，
//! 文件管理器 / 任务栏 / 快捷方式即显示应用图标。
//! 其余平台空操作（依赖也按 target 门控，不进非 Windows 的编译图）。

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 必须存在");
    // 图标统一放在 workspace 根的 assets/ 下，crate 位于 crates/duck-app，故回退两级。
    let icon = std::path::Path::new(&manifest_dir).join("../../assets/duck.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let icon = icon.to_str().expect("图标路径必须是合法 UTF-8");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon);
    res.compile().expect("编译 Windows 资源（duck.ico）失败");
}
