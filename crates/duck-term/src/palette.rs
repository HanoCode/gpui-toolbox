//! 终端调色板：把主题色映射成终端认识的颜色表。
//!
//! # 为什么内核要有自己的颜色表
//!
//! `alacritty_terminal` 的 `Colors` 只是一张 **`Option<Rgb>` 覆盖表**（默认全 `None`）：
//! 它记的是「应用用 OSC 4 改过哪些色」，而不是「终端长什么样」。真正的默认配色必须由
//! 宿主提供 —— 所以 [`Palette`] 在本 crate 里定义，不依赖 `duck-theme`（内核保持 GPUI 无关）。
//!
//! 三个消费方共用它，保证「屏幕上的颜色」和「回答应用查询的颜色」永远一致：
//!
//! 1. 渲染层逐格取色时用它兜底（`Term::colors()` 里没覆盖的走这里）；
//! 2. [`crate::Session::apply_palette`] 把它灌进 `Term` 的颜色表，于是 OSC 4 覆盖能落在
//!    同一张表上，语义与 alacritty 一致；
//! 3. 事件循环线程回答 OSC 4 / 10 / 11 查询时用它（见 `session.rs` 的 `EventProxy`）。
//!
//! 索引语义照抄 alacritty（`term/color.rs` 的 `COUNT = 269`）：
//! `0..16` ANSI、`16..232` 6×6×6 色立方、`232..256` 灰阶、`256` 前景、`257` 背景、
//! `258` 光标、`259..267` 暗色变体、`267` 亮前景、`268` 暗背景。

use alacritty_terminal::vte::ansi::Rgb;

/// 终端前景 / 背景 / 光标在查询索引里的位置（alacritty 约定）。
pub const INDEX_FOREGROUND: usize = 256;
pub const INDEX_BACKGROUND: usize = 257;
pub const INDEX_CURSOR: usize = 258;
/// 颜色表总长度（含暗色变体），与 alacritty 的 `color::COUNT` 一致。
pub const COLOR_COUNT: usize = 269;

/// 终端配色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// 16 个 ANSI 色（0..8 常规、8..16 亮色）。
    pub ansi: [[u8; 3]; 16],
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: [u8; 3],
}

impl Palette {
    /// 深色主题默认配色（xterm 系 + 略调亮以保证在深底上的对比度）。
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            ansi: [
                [0x2e, 0x34, 0x36],
                [0xcc, 0x66, 0x66],
                [0x8a, 0xb5, 0x5b],
                [0xd8, 0xa6, 0x57],
                [0x5c, 0x8f, 0xc7],
                [0x9a, 0x7a, 0xc7],
                [0x5f, 0xb3, 0xb3],
                [0xd5, 0xd8, 0xdb],
                [0x66, 0x6b, 0x6f],
                [0xe0, 0x7a, 0x7a],
                [0x9e, 0xc9, 0x6f],
                [0xf0, 0xbe, 0x6b],
                [0x74, 0xa7, 0xdf],
                [0xb2, 0x92, 0xdf],
                [0x77, 0xcb, 0xcb],
                [0xff, 0xff, 0xff],
            ],
            foreground: [0xd5, 0xd8, 0xdb],
            background: [0x1c, 0x1f, 0x22],
            cursor: [0xd5, 0xd8, 0xdb],
        }
    }

    /// 浅色主题默认配色（同色相调暗，保证在浅底上可读）。
    #[must_use]
    pub const fn light() -> Self {
        Self {
            ansi: [
                [0x24, 0x29, 0x2e],
                [0xc0, 0x39, 0x3b],
                [0x3d, 0x7a, 0x2a],
                [0x9a, 0x6b, 0x12],
                [0x2b, 0x5d, 0x9c],
                [0x7a, 0x4f, 0xa8],
                [0x1f, 0x7a, 0x7a],
                [0x6b, 0x72, 0x7a],
                [0x4a, 0x51, 0x59],
                [0xd6, 0x4f, 0x50],
                [0x4c, 0x91, 0x36],
                [0xb0, 0x7d, 0x1b],
                [0x36, 0x6f, 0xb5],
                [0x8e, 0x60, 0xbe],
                [0x28, 0x8f, 0x8f],
                [0x1a, 0x1d, 0x21],
            ],
            foreground: [0x24, 0x29, 0x2e],
            background: [0xfa, 0xfa, 0xfa],
            cursor: [0x24, 0x29, 0x2e],
        }
    }

    /// 按 alacritty 的索引语义取色。
    ///
    /// 越界返回 `None`（而不是 clamp）——调用方拿到 `None` 应当回退到前景色，
    /// 静默取一个「看着还行」的色只会把越界 bug 藏起来。
    #[must_use]
    pub fn resolve(&self, index: usize) -> Option<Rgb> {
        Some(match index {
            0..=15 => rgb(self.ansi[index]),
            16..=231 => {
                // 6×6×6 色立方：每轴 0 / 95 / 135 / 175 / 215 / 255。
                let n = index - 16;
                let steps = [0u8, 95, 135, 175, 215, 255];
                rgb([steps[n / 36], steps[(n % 36) / 6], steps[n % 6]])
            }
            232..=255 => {
                // 24 级灰阶：8 + 10·k。
                let v = 8 + (index - 232) as u8 * 10;
                rgb([v, v, v])
            }
            INDEX_FOREGROUND | 267 => rgb(self.foreground),
            INDEX_BACKGROUND | 268 => rgb(self.background),
            INDEX_CURSOR => rgb(self.cursor),
            // 259..267 是「暗色变体」，alacritty 里由应用额外配置；我们不引入第三套色，
            // 直接映射回同名常规色，视觉上等于「没配暗色变体」。
            259..=266 => rgb(self.ansi[index - 259]),
            _ => return None,
        })
    }
}

fn rgb([r, g, b]: [u8; 3]) -> Rgb {
    Rgb { r, g, b }
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_slots_follow_the_table() {
        let palette = Palette::dark();
        assert_eq!(palette.resolve(0), Some(rgb(palette.ansi[0])));
        assert_eq!(palette.resolve(15), Some(rgb(palette.ansi[15])));
    }

    #[test]
    fn color_cube_matches_xterm_boundaries() {
        let palette = Palette::dark();
        // 16  → 立方体原点 (0,0,0)
        assert_eq!(palette.resolve(16), Some(rgb([0, 0, 0])));
        // 21  → (0,0,5) 纯蓝
        assert_eq!(palette.resolve(21), Some(rgb([0, 0, 255])));
        // 196 → (5,0,0) 纯红
        assert_eq!(palette.resolve(196), Some(rgb([255, 0, 0])));
        // 231 → (5,5,5) 纯白
        assert_eq!(palette.resolve(231), Some(rgb([255, 255, 255])));
    }

    #[test]
    fn grayscale_ramp_is_evenly_spaced() {
        let palette = Palette::dark();
        assert_eq!(palette.resolve(232), Some(rgb([8, 8, 8])));
        assert_eq!(palette.resolve(255), Some(rgb([238, 238, 238])));
    }

    #[test]
    fn named_slots_map_to_theme_colors() {
        let palette = Palette::light();
        assert_eq!(
            palette.resolve(INDEX_FOREGROUND),
            Some(rgb(palette.foreground))
        );
        assert_eq!(
            palette.resolve(INDEX_BACKGROUND),
            Some(rgb(palette.background))
        );
        assert_eq!(palette.resolve(INDEX_CURSOR), Some(rgb(palette.cursor)));
    }

    #[test]
    fn out_of_range_indexes_are_reported() {
        let palette = Palette::dark();
        assert_eq!(palette.resolve(COLOR_COUNT), None);
        assert_eq!(palette.resolve(usize::MAX), None);
    }
}
