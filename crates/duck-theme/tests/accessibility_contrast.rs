//! 无障碍对比度守卫（P2-7）。
//!
//! 这一层此前**完全没有守卫**：`builtin.rs` 的测试只钉了「主色按钮文字 ≥ 4.5:1」，
//! 而 token 文档里写死、界面上真实依赖的三条约束一条都没测到：
//!
//! 1. **焦点环**（`ring`）必须能在它实际出现的每一种底上被看见 ——
//!    键盘用户靠它判断「我现在停在哪」，看不见就等于没有键盘导航；
//! 2. **选中导航图标**（`nav_selected_icon`）对选中底（`nav_selected`）；
//! 3. **强调色文字**（`accent_text`）对内容底（`surface`）。
//!
//! 阈值取 WCAG 2.1 SC 1.4.11（非文本对比 3:1）与 SC 1.4.3（正文 4.5:1）。
//!
//! **当前状态**：三条全绿（42 预设 × 2 强调色）。
//! 焦点环那条曾实测不成立（80/420 组合低于 3:1，最低约 2.0:1），当时以 `#[ignore]` 存档；
//! 2026-10-11 修 `base_tokens` 的 `ring` 派生方式后已解除忽略，改为**常驻守卫**（详见该用例）。
//!
//! `ring` 曾经的薄弱点：`base_tokens` 只保证它与 `primary_foreground`（按钮上的文字）的
//! 对比，而它真正被画上去的地方是 `background` / `surface` / `muted` / 活动栏 `chrome` /
//! 选中导航底 `nav_selected` —— 两个集合并不相同。现在 `ring` 改为对底色收敛。
//!
//! 本文件的 WCAG 实现是**独立的一份**，刻意不复用 `duck-theme` 内部的私有
//! `relative_luminance` / `contrast_ratio`：复用就等于用被测代码验证被测代码。
//!
//! 覆盖 42 个内置预设 × 2 种强调色（默认强调色 + 各预设自带强调色）。

use duck_theme::{BuiltinThemeId, base_tokens, default_accent, extended_tokens};
use gpui::{Hsla, Rgba};

/// WCAG 2.x 相对亮度：sRGB 通道线性化后按 0.2126 / 0.7152 / 0.0722 加权。
fn relative_luminance(value: Hsla) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.039_28 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    let rgba = Rgba::from(value);
    0.2126 * linear(rgba.r) + 0.7152 * linear(rgba.g) + 0.0722 * linear(rgba.b)
}

/// WCAG 对比度比值（1:1 ~ 21:1）。
fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

/// 焦点环必须在它真正被画上去的每一种底上都 ≥ 3:1。
///
/// 之所以单列一条：`ring` 曾由 `primary` 收敛而来，而 `base_tokens` 里那次收敛只针对
/// `primary_foreground`（**按钮上的文字**）。可焦点环从不画在按钮文字上，它画在
/// 页面底色、卡片底色、选中行底色、活动栏底色上 —— 这条链路上没有任何守卫。
///
/// # 存档 → 常驻
///
/// 修 `ring` 派生方式之前，这条**实测不成立**，曾以 `#[ignore]` 存档
/// （与 `logs_scroll_repro.rs` 记录上游 `Scrollable` 缺陷同一做法）。
/// 当时首次运行结果：**80 / 420 组合低于 3:1**，其中深色主题占 75 条，分布为
/// `surface` 25 · `nav_selected` 20 · `muted` 19 · `chrome` 8 · `background` 3；
/// 浅色主题 5 条（`nav_selected` 4 · `surface` 1）。最差约 `2.10:1`（Catppuccin Frappe Dark）。
///
/// 根因：`base_tokens` 里 `colors.primary = button_surface(primary, primary_foreground)`，
/// 随后 `colors.ring = colors.primary`。深色主题的强调色被**压暗**到能配白字之后，落在同样
/// 暗的卡片 / 选中底上就几乎没有对比度 —— 也就是说键盘焦点环在这些主题上基本看不见。
///
/// 已采用的修法（2026-10-11）：`ring` 改为经 `builtin::focus_ring` 对**底色**收敛
/// （`background` / `surface` / `secondary` / `muted` / `border` 中最难的一端），不再继承只
/// 对文字负责的 `primary`。副作用是焦点环不再与强调色同色 —— 但那恰好也解掉「选中态边框
/// 已是 `primary` 时焦点环等于没画」的碰撞。本条现已解除忽略，作为常驻守卫。
#[test]
fn focus_ring_is_visible_on_every_surface_it_is_drawn_on() {
    const MIN: f32 = 3.0;

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for id in BuiltinThemeId::ALL {
        let mode = id.appearance();
        for (accent_name, accent) in [
            ("默认强调色", default_accent()),
            ("预设强调色", id.preview().primary),
        ] {
            let base = base_tokens(id, accent);
            let extended = extended_tokens(&base, mode);
            let ring = base.colors.ring;
            let surfaces = [
                ("background", base.colors.background),
                ("surface", base.colors.surface),
                ("muted", base.colors.muted),
                ("chrome", extended.colors.chrome),
                ("nav_selected", extended.colors.nav_selected),
            ];
            for (surface_name, surface) in surfaces {
                checked += 1;
                let ratio = contrast_ratio(ring, surface);
                if ratio < MIN - 1e-3 {
                    failures.push(format!(
                        "    {:<22} {:?}  {accent_name}  ring vs {surface_name}: {ratio:.2}:1",
                        id.label(),
                        mode
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "焦点环 vs 所在底的对比度低于 {MIN}:1 —— {} / {checked} 组合不合格：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 选中导航项的图标对选中底 ≥ 3:1（`ExtendedColors::nav_selected_icon` 文档写死的约束）。
#[test]
fn selected_nav_icon_meets_non_text_contrast() {
    const MIN: f32 = 3.0;

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for id in BuiltinThemeId::ALL {
        let mode = id.appearance();
        for accent in [default_accent(), id.preview().primary] {
            let base = base_tokens(id, accent);
            let extended = extended_tokens(&base, mode);
            checked += 1;
            let ratio = contrast_ratio(
                extended.colors.nav_selected_icon,
                extended.colors.nav_selected,
            );
            if ratio < MIN - 1e-3 {
                failures.push(format!(
                    "    {:<22} {:?}  nav_selected_icon vs nav_selected: {ratio:.2}:1",
                    id.label(),
                    mode
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "选中导航图标对比度低于 {MIN}:1 —— {} / {checked} 组合不合格：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 强调色文字对内容底 ≥ 4.5:1（`ExtendedColors::accent_text` 文档写死的约束）。
#[test]
fn accent_text_meets_aa_against_surface() {
    const MIN: f32 = 4.5;

    let mut failures = Vec::new();
    let mut checked = 0usize;

    for id in BuiltinThemeId::ALL {
        let mode = id.appearance();
        for accent in [default_accent(), id.preview().primary] {
            let base = base_tokens(id, accent);
            let extended = extended_tokens(&base, mode);
            checked += 1;
            let ratio = contrast_ratio(extended.colors.accent_text, base.colors.surface);
            if ratio < MIN - 1e-3 {
                failures.push(format!(
                    "    {:<22} {:?}  accent_text vs surface: {ratio:.2}:1",
                    id.label(),
                    mode
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "强调色文字对比度低于 AA {MIN}:1 —— {} / {checked} 组合不合格：\n{}",
        failures.len(),
        failures.join("\n")
    );
}
