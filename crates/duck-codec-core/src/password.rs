//! 随机密码生成 + 强度评估。
//!
//! 口径 = 参考仓 `CryptoBundle.vue` 的 `generatePasswords` / `pwdStrength`：
//!
//! - 字符池按勾选顺序拼接：大写 → 小写 → 数字 → 符号 `!@#$%^&*()_+-=[]{}|;:,.<>?`；
//! - 每个字符取**一个 32 位随机数**，再按 `floor(x / 2^32 * 池长)` 映射到下标
//!   （源仓的 `scaledIndex`，本质是 Lemire 乘取高位，偏差可忽略）——
//!   这里保持同一映射，连「一位密码一个随机数」的取样次数都不变；
//! - 强度分档：长度 ≥8 记 1 分、≥12 再记 1 分、大小写都有 1 分、有数字 1 分、
//!   有符号 1 分；`≤2` 弱、`≤3` 中、否则强。
//!
//! 随机源用 `getrandom`（OS 熵：`/dev/urandom` / `BCryptGenRandom`），
//! 与浏览器的 `crypto.getRandomValues` 同级，不用伪随机数发生器——
//! 密码这的东西宁可直接要系统熵。

/// 生成参数（与参考页那排勾选框一一对应）。
#[derive(Debug, Clone)]
pub struct Options {
    /// 每个密码的长度（源仓限制 4..=128）
    pub length: usize,
    /// 生成几个（源仓限制 1..=50）
    pub count: usize,
    /// 含大写 `A-Z`
    pub upper: bool,
    /// 含小写 `a-z`
    pub lower: bool,
    /// 含数字 `0-9`
    pub digits: bool,
    /// 含符号 `!@#$%^&*()_+-=[]{}|;:,.<>?`
    pub symbols: bool,
}

/// 一条结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Password {
    /// 密码本身
    pub value: String,
    /// 强度档位：`弱` / `中` / `强`
    pub strength: &'static str,
}

/// 符号池（源仓原文，顺序也一致——但顺序不影响均匀性，只是便于逐字对照）。
const SYMBOLS: &str = "!@#$%^&*()_+-=[]{}|;:,.<>?";
/// 长度上下限（源仓 `min="4" max="128"`）。
pub const MIN_LENGTH: usize = 4;
/// 长度上限。
pub const MAX_LENGTH: usize = 128;
/// 数量上限（源仓 `min="1" max="50"`）。
pub const MAX_COUNT: usize = 50;

/// 按参数生成。
///
/// # Errors
/// 一个字符类型都没选、长度或数量越界、或取不到系统熵时报错。
pub fn generate(options: &Options) -> Result<Vec<Password>, String> {
    let mut pool = String::new();
    if options.upper {
        pool.push_str("ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    }
    if options.lower {
        pool.push_str("abcdefghijklmnopqrstuvwxyz");
    }
    if options.digits {
        pool.push_str("0123456789");
    }
    if options.symbols {
        pool.push_str(SYMBOLS);
    }
    let chars: Vec<char> = pool.chars().collect();
    if chars.is_empty() {
        return Err("请至少选择一种字符类型".to_owned());
    }
    if options.length < MIN_LENGTH || options.length > MAX_LENGTH {
        return Err(format!(
            "长度必须在 {} 到 {} 之间，实际 {}",
            MIN_LENGTH, MAX_LENGTH, options.length
        ));
    }
    if options.count == 0 || options.count > MAX_COUNT {
        return Err(format!(
            "数量必须在 1 到 {} 之间，实际 {}",
            MAX_COUNT, options.count
        ));
    }

    let mut bytes = vec![0u8; options.length * 4];
    let mut out = Vec::with_capacity(options.count);
    for _ in 0..options.count {
        getrandom::fill(&mut bytes).map_err(|error| format!("取系统随机数失败：{}", error))?;
        let mut value = String::with_capacity(options.length);
        // 每 4 字节一个 32 位随机数（长度恒为 4 的整数倍，末块不会是残块）。
        for chunk in bytes.chunks(4) {
            let raw = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            // `floor(raw / 2^32 * len)` ≡ 乘取高位，和源仓的 scaledIndex 同一映射。
            let index = ((u64::from(raw) * chars.len() as u64) >> 32) as usize;
            value.push(chars[index]);
        }
        let strength = strength(&value);
        out.push(Password { value, strength });
    }
    Ok(out)
}

/// 强度分档（源仓 `pwdStrength` 的五条计分规则）。
#[must_use]
pub fn strength(password: &str) -> &'static str {
    let mut score = 0;
    if password.chars().count() >= 8 {
        score += 1;
    }
    if password.chars().count() >= 12 {
        score += 1;
    }
    let has_upper = password.chars().any(|ch| ch.is_ascii_uppercase());
    let has_lower = password.chars().any(|ch| ch.is_ascii_lowercase());
    if has_upper && has_lower {
        score += 1;
    }
    if password.chars().any(|ch| ch.is_ascii_digit()) {
        score += 1;
    }
    if password.chars().any(|ch| !ch.is_ascii_alphanumeric()) {
        score += 1;
    }
    if score <= 2 {
        "弱"
    } else if score <= 3 {
        "中"
    } else {
        "强"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(length: usize, count: usize) -> Options {
        Options {
            length,
            count,
            upper: true,
            lower: true,
            digits: true,
            symbols: true,
        }
    }

    #[test]
    fn honours_length_count_and_pool() {
        let list = generate(&options(16, 5)).expect("生成");
        assert_eq!(list.len(), 5);
        for item in &list {
            assert_eq!(item.value.chars().count(), 16);
            assert!(
                item.value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || SYMBOLS.contains(ch)),
                "{}",
                item.value
            );
        }
        // 两次生成不该撞（128 位熵，撞了就是随机源有问题）。
        assert_ne!(
            generate(&options(16, 1)).expect("生成")[0],
            generate(&options(16, 1)).expect("生成")[0]
        );
    }

    #[test]
    fn pool_follows_the_checkboxes() {
        let mut only_digits = options(8, 3);
        only_digits.upper = false;
        only_digits.lower = false;
        only_digits.symbols = false;
        for item in generate(&only_digits).expect("生成") {
            assert!(
                item.value.chars().all(|ch| ch.is_ascii_digit()),
                "{}",
                item.value
            );
        }
        let none = Options {
            length: 8,
            count: 1,
            upper: false,
            lower: false,
            digits: false,
            symbols: false,
        };
        assert!(generate(&none).is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(generate(&options(3, 1)).is_err());
        assert!(generate(&options(129, 1)).is_err());
        assert!(generate(&options(8, 0)).is_err());
        assert!(generate(&options(8, 51)).is_err());
    }

    #[test]
    fn strength_matches_the_reference_scoring() {
        assert_eq!(strength("abcd"), "弱"); // 长度 4：0 分
        assert_eq!(strength("abcdefgh"), "弱"); // 8 位 1 分（源仓 ≤2 一律「弱」）
        assert_eq!(strength("abcdefghijkl"), "弱"); // 8 + 12 → 2 分
        assert_eq!(strength("Abcdefghijkl"), "中"); // 2 + 大小写 = 3 分
        assert_eq!(strength("Abcdefghijk1"), "强"); // 3 + 数字 = 4 分
        assert_eq!(strength("Abcdefghijk1!"), "强");
        assert_eq!(strength("Ab1!Ab1!"), "强"); // 8 位 1 + 大小写 + 数字 + 符号
    }
}
