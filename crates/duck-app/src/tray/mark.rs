//! 菜单栏标记：把 [`DuckIcon`] 的 SVG 光栅化成 `tray_icon` 要的 RGBA。
//!
//! 不新增位图资源：复用应用已有的图标集（Lucide 1.48 子集），经 gpui 自带的
//! `svg_renderer` 光栅化。选 [`DuckIcon::Box`] 是因为它最像「工具箱」——
//! 一个方正的箱体轮廓，在菜单栏那点尺寸下也认得出。
//!
//! # macOS 读 alpha，别的平台读颜色
//!
//! macOS 上状态栏图是**模板图**（`setTemplate:`）：AppKit 只读它的 alpha，颜色由系统
//! 自己画——浅色菜单栏上画深色、深色菜单栏上画浅色、菜单展开时反色。颜色通道不承载
//! 任何信息，所以直接清零，而不是把 gpui 的预乘 BGRA 反解出来。
//!
//! 其他平台没有模板图，`tray_icon` 会把 RGBA 原样交给系统，全零会画出一个**纯黑**的
//! 标记——在深色任务栏上等于看不见。所以那里给一个明确的墨色。

use duck_components::DuckIcon;
use gpui::{App, AssetSource as _, SMOOTH_SVG_SCALE_FACTOR};
use tray_icon::Icon;

use crate::assets::DesktopAssets;

/// 标记光栅化后的高度（像素）。
///
/// macOS 把状态栏图钉在 18 **点**高、宽度按源图比例推，所以 36px 正好是 2×：
/// 在 1× 与 2× 的 Mac 上都清晰。去问屏幕要 backing scale、再决定渲染 18 还是 36
/// 什么也换不来——2× 图在 1× 屏上只会被降采样，而那是正确的方向。
const MARK_HEIGHT_PX: f32 = 36.;

/// 图标 SVG 的固有高度（Lucide 的 `viewBox` 是 24×24）。
const SVG_HEIGHT_UNITS: f32 = 24.;

/// 绘制标记的主机。
///
/// 做成参数而不是 `#[cfg]`，好处是两种答案都能在同一台机器上被测到。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayHost {
    /// 标记是 AppKit 模板图。
    Macos,
    /// 标记是一张普通位图，没有东西会替它反色。
    Other,
}

impl TrayHost {
    /// 当前进程所在的主机。
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Other
        }
    }
}

/// 标记的不透明像素用什么颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkPaint {
    /// 颜色通道无意义，写零。
    Template,
    /// 明确的墨色，因为这个主机不会替位图反色。
    Solid([u8; 3]),
}

/// 非 macOS 默认的墨色：偏白，因为深色任务栏更常见。
const LIGHT_INK: [u8; 3] = [0xff, 0xff, 0xff];

/// 主机决定用什么颜色画标记。
#[must_use]
pub const fn paint(host: TrayHost) -> MarkPaint {
    match host {
        TrayHost::Macos => MarkPaint::Template,
        TrayHost::Other => MarkPaint::Solid(LIGHT_INK),
    }
}

/// 这个主机的标记是不是模板图（即 `TrayIconBuilder::with_icon_as_template` 该传什么）。
///
/// 从 [`paint`] 派生而不是各写一遍：位图颜色和 AppKit 的模板标志必须同时成立，
/// 分开写迟早会漂成两套答案。
#[must_use]
pub const fn is_template(host: TrayHost) -> bool {
    matches!(paint(host), MarkPaint::Template)
}

/// 光栅化 `icon` 得到的菜单栏标记。
///
/// `host` 由调用方判定一次再传进来：主机的答案整条链上只该问一遍。
///
/// # Errors
/// 资源取不到、SVG 解析失败、或尺寸不可用时返回一句英文说明（调用方记日志）。
pub fn render(icon: DuckIcon, host: TrayHost, cx: &App) -> Result<Icon, String> {
    let path = icon.asset_path();
    let bytes = DesktopAssets
        .load(path)
        .map_err(|error| format!("could not load {path}: {error}"))?
        .ok_or_else(|| format!("no embedded asset at {path}"))?;

    // `render_single_frame` 会把 scale 再乘一遍 `SMOOTH_SVG_SCALE_FACTOR`，
    // 所以这里除一次，落点才是 `MARK_HEIGHT_PX`。
    let scale = MARK_HEIGHT_PX / SVG_HEIGHT_UNITS / SMOOTH_SVG_SCALE_FACTOR;
    let image = cx
        .svg_renderer()
        .render_single_frame(&bytes, scale)
        // `usvg::Error` 不实现 `std::error::Error`，所以在这里就转成字符串。
        .map_err(|error| format!("could not rasterise {path}: {error}"))?;

    let size = image.size(0);
    let bgra = image
        .as_bytes(0)
        .ok_or_else(|| format!("{path} rasterised to no frame"))?;

    Icon::from_rgba(
        mark_rgba(bgra, paint(host)),
        size.width.0 as u32,
        size.height.0 as u32,
    )
    .map_err(|error| format!("{path} is not a usable tray icon: {error}"))
}

/// gpui 的预乘 BGRA → `tray_icon` 要的直通 RGBA。
///
/// alpha 逐字节保留，颜色是**替换**而不是反预乘：gpui 用 SVG 里写的颜色渲染了标记，
/// 而两个主机都不想要那个颜色。模板图完全忽略颜色通道；其余地方用 [`paint`] 选的
/// 平色，满强度——直通 RGBA 让 alpha 去做抗锯齿。
///
/// 纯函数，所以唯一要紧的性质是个单测，而不是去菜单栏前眯眼看。
fn mark_rgba(bgra: &[u8], paint: MarkPaint) -> Vec<u8> {
    let [red, green, blue] = match paint {
        MarkPaint::Template => [0, 0, 0],
        MarkPaint::Solid(ink) => ink,
    };
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[red, green, blue, pixel[3]]);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::{
        LIGHT_INK, MARK_HEIGHT_PX, MarkPaint, SVG_HEIGHT_UNITS, TrayHost, is_template, mark_rgba,
        paint,
    };
    use gpui::SMOOTH_SVG_SCALE_FACTOR;

    #[test]
    fn macos_gets_a_template_and_everything_else_an_ink() {
        assert_eq!(paint(TrayHost::Macos), MarkPaint::Template);
        assert_eq!(paint(TrayHost::Other), MarkPaint::Solid(LIGHT_INK));
    }

    /// 模板标志与墨色是同一个决定的两种说法，不能各走各的。
    #[test]
    fn only_the_template_host_asks_for_a_template_image() {
        assert!(is_template(TrayHost::Macos));
        assert!(!is_template(TrayHost::Other));
    }

    /// 模板图的颜色通道必须全零：AppKit 只读 alpha，留着别的颜色只会让它误解。
    #[test]
    fn a_template_mark_zeroes_every_colour_channel() {
        let bgra = [10, 20, 30, 200, 40, 50, 60, 255];
        assert_eq!(
            mark_rgba(&bgra, MarkPaint::Template),
            [0, 0, 0, 200, 0, 0, 0, 255]
        );
    }

    /// 颜色被替换，alpha 逐字节保留——这正是直通 RGBA 让抗锯齿成立的地方。
    #[test]
    fn a_solid_mark_keeps_the_alpha_and_replaces_the_colour() {
        let bgra = [1, 2, 3, 0, 4, 5, 6, 128, 7, 8, 9, 255];
        assert_eq!(
            mark_rgba(&bgra, MarkPaint::Solid([0x11, 0x22, 0x33])),
            [
                0x11, 0x22, 0x33, 0, 0x11, 0x22, 0x33, 128, 0x11, 0x22, 0x33, 255
            ]
        );
    }

    /// 每个像素四字节，所以输出长度必须与输入一致；末尾不足四字节的尾巴被丢掉，
    /// 不会让缓冲区错位。
    #[test]
    fn a_ragged_tail_is_dropped_rather_than_shifted() {
        let bgra = [1, 2, 3, 4, 5, 6];
        assert_eq!(mark_rgba(&bgra, MarkPaint::Template), [0, 0, 0, 4]);
    }

    /// 光栅化的落点：scale 乘回 `SMOOTH_SVG_SCALE_FACTOR` 之后应当正好是标记高度。
    #[test]
    fn the_scale_lands_on_the_mark_height() {
        let scale = MARK_HEIGHT_PX / SVG_HEIGHT_UNITS / SMOOTH_SVG_SCALE_FACTOR;
        assert!((scale * SMOOTH_SVG_SCALE_FACTOR * SVG_HEIGHT_UNITS - MARK_HEIGHT_PX).abs() < 1e-6);
    }
}
