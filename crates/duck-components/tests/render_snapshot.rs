//! 渲染快照测试基线（§6.2.2）
//!
//! 钉住「同一份组件、同一份主题、同一份输入，渲染出的像素哈希必须相同」。
//! GPUI 提供 `Window::render_to_image() -> image::RgbaImage`，本文件用
//! `image::RgbaImage` 的字节级 SHA-256 作指纹（不存 PNG，避免 baseline 仓库膨胀）。
//!
//! **断言策略**（两段式）：
//! 1. **首次跑**：写 baseline 到 `target/render-snapshots/<name>.sha256`，
//!    测试通过。**不**默认拒绝——首次必然是新基线。
//! 2. **后续跑**：读 baseline，对比指纹。差异即红。
//!
//! 接受环境变量 `BLESS=1` 重写 baseline（升级 gpui-fast / 改完主题 token 后用）。
//!
//! ⚠️ **运行期前提（2026-10-14 记录）**：`Window::render_to_image()` 需要平台
//! headless renderer；`TestAppContext` 不配置它 —— `gpui/src/platform/test/window.rs:551`
//! 在缺 renderer 时直接 `bail!("render_to_image not available: no HeadlessRenderer
//! configured")`。本仓也没有注入入口（`TestAppContext::build_with_platform` 为私有）。
//! 故本文件的用例一律 `#[ignore]`：**先保证编译**（仓库级 `clippy --all-targets` 不再被它拖红），
//! 等上游提供 renderer 钩子后再解 `ignore`。

use std::path::PathBuf;

use gpui::{
    AppContext as _, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext, Window, div,
};
use gpui_component::v_flex;
use serde::{Deserialize, Serialize};

/// 基线条目：组件名 → 当前指纹 + 元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Baseline {
    name: String,
    hash: String,
    width: u32,
    height: u32,
    /// 写入时间（ISO 8601）。CI 与本机都能读。
    captured_at: String,
}

impl Baseline {
    /// SHA-256 over RgbaImage 字节序（不平台相关）。
    ///
    /// 为何不用 `image::RgbaImage` 的 `default_hasher` 派生：跨平台字节序稳定即可，
    /// 这里**故意不用 `DefaultHasher`**——它在 Rust 标准库属于 `SipHasher13` 实现，
    /// 不保证跨版本稳定。
    fn hash_bytes(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let result = hasher.finalize();
        result.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn baseline_dir() -> PathBuf {
    let mut path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    // 找到 `target/debug/deps/` → 上溯到 `target/`，落到 `target/render-snapshots/`。
    if path.ends_with("deps") {
        path.pop(); // deps
    }
    if path.ends_with("debug") || path.ends_with("release") {
        path.pop(); // profile
    }
    path.push("render-snapshots");
    path
}

fn read_baseline(name: &str) -> Option<Baseline> {
    let path = baseline_dir().join(format!("{name}.json"));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn write_baseline(baseline: &Baseline) {
    let dir = baseline_dir();
    std::fs::create_dir_all(&dir).expect("create render-snapshots dir");
    let path = dir.join(format!("{}.json", baseline.name));
    let json = serde_json::to_string_pretty(baseline).expect("serialize baseline");
    std::fs::write(&path, json).expect("write baseline");
}

/// 公共断言：渲染给定元素树 → 拍快照 → 校验指纹。
///
/// - 测试期：根据 baseline 判断 pass / fail
/// - `BLESS=1` 环境变量：覆写 baseline
pub fn assert_snapshot(name: &str, image: &image::RgbaImage) {
    let hash = Baseline::hash_bytes(image.as_raw());
    let baseline = Baseline {
        name: name.to_owned(),
        hash: hash.clone(),
        width: image.width(),
        height: image.height(),
        captured_at: chrono_like_now(),
    };

    let bless = std::env::var("BLESS").is_ok_and(|v| v == "1");
    match (bless, read_baseline(name)) {
        // BLESS=1：直接覆写
        (true, _) => {
            write_baseline(&baseline);
            eprintln!("[bless] wrote baseline for {name}: {hash}");
        }
        // 首次跑：写 baseline，测试通过（让首次跑在干净仓库不红）
        (false, None) => {
            write_baseline(&baseline);
            eprintln!("[seed] wrote initial baseline for {name}: {hash}");
        }
        // 有 baseline：对比
        (false, Some(prev)) => {
            assert_eq!(
                prev.hash, hash,
                "render snapshot for `{name}` drifted.\n  expected: {}\n  actual:   {}\n  run with BLESS=1 to update",
                prev.hash, hash
            );
        }
    }
}

fn chrono_like_now() -> String {
    // 不引 chrono（避免新增依赖）——用 std time 拼 ISO 8601。
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

// ── 真实快照测试 ───────────────────────────────────────────────────────

struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(div())
    }
}

#[gpui::test]
#[ignore = "needs a platform headless renderer; TestAppContext does not configure one (see module doc)"]
fn empty_window_snapshot_is_stable(cx: &mut TestAppContext) {
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|_| Blank);
        gpui_component::Root::new(view, window, cx)
    });
    VisualTestContext::from_window(*window, cx).update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    let image = window
        .update(cx, |_, window, _| window.render_to_image())
        .expect("render empty window")
        .expect("render image");
    assert_snapshot("empty_window", &image);
}
