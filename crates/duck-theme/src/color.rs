//! 把用户数据里的 `#RRGGBB` 变成主题色系统里的 [`Hsla`]。
//!
//! 为什么需要它：标签与项目的 `color` 是**用户数据**（内核的 `tags.color` /
//! `projects.color` 存的就是 `#RRGGBB` 文本），不是设计 token —— token 纪律
//! （AGENTS §3.6）要求界面不写颜色字面量，但用户自己挑的颜色必须原样呈现，
//! 所以只能在边界上转一次，转换规则集中在这里。
//!
//! 用 `gpui::hsla()` 自己算 RGB→HSL，而不是找 `From<Rgba> for Hsla`：
//! 转换只有几行，自己写就不必依赖上游那两个类型之间的 `From` 实现是否存在。

use gpui::{Hsla, hsla};

/// `#RRGGBB` → 颜色。解析不了返回 `None`（**不兜底成某个颜色**：颜色是用户数据，
/// 坏了要让调用方决定怎么显示，而不是悄悄换成另一种颜色）。
///
/// 只认带 `#` 的六位写法 —— 与内核写入时用的格式一致（`NewTag::new` 的默认值、
/// `NewProject::new` 的 `DEFAULT_PROJECT_COLOR` 都是这个形状）。
#[must_use]
pub fn color_from_hex(hex: &str) -> Option<Hsla> {
    let raw = hex.strip_prefix('#')?;
    if raw.len() != 6 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(raw, 16).ok()?;
    Some(from_rgb_u32(value))
}

/// `0xRRGGBB` → 颜色。
#[must_use]
pub fn color_from_rgb(value: u32) -> Hsla {
    from_rgb_u32(value & 0x00FF_FFFF)
}

fn from_rgb_u32(value: u32) -> Hsla {
    let r = ((value >> 16) & 0xFF) as f32 / 255.;
    let g = ((value >> 8) & 0xFF) as f32 / 255.;
    let b = (value & 0xFF) as f32 / 255.;
    from_rgb(r, g, b)
}

/// RGB（各分量 0–1）→ HSL。标准换算式，不透明度固定 1。
///
/// 色相取 `0–1` 区间 —— 这是 gpui 的约定（不是 0–360），与 `hsla()` 的第一参数一致。
fn from_rgb(r: f32, g: f32, b: f32) -> Hsla {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.;
    let delta = max - min;
    if delta == 0. {
        // 灰阶：色相与饱和度都无意义，返回中性色（避免 0/0）。
        return hsla(0., 0., lightness, 1.);
    }
    // 饱和度：分母在两端（纯黑 / 纯白）会归零，用标准式直接算即可。
    let saturation = delta / (1. - (2. * lightness - 1.).abs());
    let hue = if max == r {
        ((g - b) / delta).rem_euclid(6.) / 6.
    } else if max == g {
        (((b - r) / delta) + 2.) / 6.
    } else {
        (((r - g) / delta) + 4.) / 6.
    };
    hsla(hue, saturation, lightness, 1.)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主题 token 的色相 / 饱和度 / 明度都是 0–1；越界会让 gpui 断言或画出怪颜色。
    fn assert_in_range(color: Hsla) {
        assert!((0. ..=1.).contains(&color.h), "色相 {} 越界", color.h);
        assert!((0. ..=1.).contains(&color.s), "饱和度 {} 越界", color.s);
        assert!((0. ..=1.).contains(&color.l), "明度 {} 越界", color.l);
        assert_eq!(color.a, 1.);
    }

    #[test]
    fn parses_primary_colours() {
        let red = color_from_hex("#FF0000").expect("red");
        assert_in_range(red);
        assert!((red.s - 1.).abs() < 1e-6, "纯红饱和度应为 1");
        assert!((red.l - 0.5).abs() < 1e-6, "纯红明度应为 0.5");
        // 0° 与 360° 等价，归一化到 0。
        assert!(red.h.abs() < 1e-6 || (red.h - 1.).abs() < 1e-6);

        let green = color_from_hex("#00FF00").expect("green");
        assert_in_range(green);
        assert!((green.h - 1. / 3.).abs() < 1e-5, "纯绿色相应为 1/3");

        let blue = color_from_hex("#0000FF").expect("blue");
        assert_in_range(blue);
        assert!((blue.h - 2. / 3.).abs() < 1e-5, "纯蓝色相应为 2/3");
    }

    #[test]
    fn greys_have_no_saturation() {
        for hex in ["#000000", "#808080", "#FFFFFF"] {
            let color = color_from_hex(hex).expect(hex);
            assert_in_range(color);
            assert_eq!(color.s, 0., "{hex} 应是中性色");
        }
    }

    #[test]
    fn lowercase_and_uppercase_are_the_same() {
        assert_eq!(color_from_hex("#1A2B3C"), color_from_hex("#1a2b3c"));
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in [
            "", "#", "1A2B3C", "#12345", "#1234567", "#GGGGGG", "#12 45C",
        ] {
            assert!(color_from_hex(bad).is_none(), "{bad:?} 不该被接受");
        }
    }

    #[test]
    fn rgb_u32_matches_the_hex_form() {
        assert_eq!(color_from_rgb(0x1A2B3C), color_from_hex("#1A2B3C").unwrap());
        // 高位多余位被掩掉，不会串色。
        assert_eq!(
            color_from_rgb(0xFF1A2B3C),
            color_from_hex("#1A2B3C").unwrap()
        );
    }
}
