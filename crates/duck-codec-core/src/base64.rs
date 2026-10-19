//! Base64 编解码（标准字母表 + `=` 补位）。
//!
//! 口径对齐参考仓 `CryptoBundle.vue` 的 `b64Encode` / `b64Decode`：
//! `new TextEncoder().encode(text)` 取 UTF-8 字节，再走浏览器 `btoa` / `atob`。
//! 两处必须照着源仓的**宽松**行为：
//! - 解码结果不是合法 UTF-8 时 `TextDecoder` 默认**不抛错**，而是替换成 U+FFFD
//!   ⇒ 这里用 [`String::from_utf8_lossy`]，不用 `from_utf8`；
//! - 解码前 `trim`：源仓的输入框里常带换行，`atob` 遇到空白会直接抛错，
//!   用户看到的是「解码失败」而不是「你多打了个换行」。

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// 文本 → Base64（UTF-8 取字节）。
#[must_use]
pub fn encode(text: &str) -> String {
    STANDARD.encode(text.as_bytes())
}

/// Base64 → 文本。
///
/// # Errors
/// 输入不是合法 Base64（含非法字符、长度不对）时返回带原因的字符串。
pub fn decode(text: &str) -> Result<String, String> {
    let bytes = STANDARD
        .decode(text.trim())
        .map_err(|error| format!("Base64 解码失败：{}", error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_utf8() {
        // 中文 / emoji 都要按 UTF-8 取字节，不能按 char 取。
        for text in ["", "hello", "恒生电子", "🦆 duck"] {
            assert_eq!(decode(&encode(text)).expect("解码"), text);
        }
    }

    #[test]
    fn matches_browser_btoa() {
        // 取值来自浏览器控制台 `btoa("恒生")`，钉住「UTF-8 + 标准字母表 + 补位」。
        assert_eq!(encode("hello"), "aGVsbG8=");
        assert_eq!(encode("恒生"), "5oGS55Sf");
    }

    #[test]
    fn rejects_garbage_with_a_reason() {
        let error = decode("!!!not base64!!!").expect_err("应当报错");
        assert!(error.starts_with("Base64 解码失败："), "{error}");
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(decode("  aGVsbG8=\n").expect("解码"), "hello");
    }
}
