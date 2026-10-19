//! 文本 ↔ 十六进制串（小写 hex，UTF-8 取字节）。
//!
//! 参考仓 `CryptoBundle.vue` 的「Hex」标签页：编码用
//! `Array.from(new TextEncoder().encode(s)).map(b => b.toString(16).padStart(2,'0'))`
//! ——**小写**、每字节两位、无分隔符，这里照抄。
//!
//! 与源仓的唯一差异在解码侧：源仓 `hexInput.match(/.{1,2}/g).map(b => parseInt(b,16))`
//! 对 `"zz"` 会得到 `NaN` 再被 `Uint8Array` 变成 `0`——**静默产出零字节**。
//! 工具场景下这比报错更糟（用户会拿着错的明文去对密码），所以这里改成显式报错，
//! 并顺手允许输入里的空白与 `0x` 前缀（粘贴 hexdump 时很常见）。

/// 文本 → 小写 hex。
#[must_use]
pub fn encode(text: &str) -> String {
    hex::encode(text.as_bytes())
}

/// 小写 hex → 文本。
///
/// # Errors
/// 出现非十六进制字符、或字符数为奇数时返回错误。
pub fn decode(text: &str) -> Result<String, String> {
    // 粘贴来的 hex 常带空格 / 换行 / `0x`，先清干净再判长度。
    let cleaned: String = text
        .trim()
        .strip_prefix("0x")
        .or_else(|| text.trim().strip_prefix("0X"))
        .unwrap_or(text.trim())
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    // 奇数位直接拒：`hex::decode` 只会报「Odd number of digits」，借这里说清楚。
    // 用 `& 1` 而不是 `% 2 != 0`：clippy 建议 `.is_multiple_of()`，而那是 1.87 的 API
    // （本仓 MSRV 1.85）。
    if cleaned.len() & 1 == 1 {
        return Err(format!(
            "Hex 解码失败：字符数必须是偶数，实际 {} 位",
            cleaned.len()
        ));
    }
    if let Some(ch) = cleaned.chars().find(|ch| !ch.is_ascii_hexdigit()) {
        return Err(format!("Hex 解码失败：{:?} 不是十六进制字符", ch));
    }
    let bytes = hex::decode(&cleaned).map_err(|error| format!("Hex 解码失败：{}", error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_lowercase_pairs() {
        assert_eq!(encode("Hi"), "4869");
        assert_eq!(encode("恒"), "e68192");
        assert!(encode("恒").chars().all(|ch| !ch.is_ascii_uppercase()));
    }

    #[test]
    fn round_trips() {
        for text in ["", "hello", "恒生电子", "\u{0}a"] {
            assert_eq!(decode(&encode(text)).expect("解码"), text);
        }
    }

    #[test]
    fn accepts_whitespace_and_prefix() {
        assert_eq!(decode("48 69").expect("解码"), "Hi");
        assert_eq!(decode("0x4869").expect("解码"), "Hi");
        assert_eq!(decode("  4869\n").expect("解码"), "Hi");
    }

    #[test]
    fn refuses_to_invent_zero_bytes() {
        // 源仓对这两个输入会静默产出 `\0\0`，这里必须报错。
        assert!(decode("zz").is_err());
        assert!(decode("486").is_err());
    }
}
