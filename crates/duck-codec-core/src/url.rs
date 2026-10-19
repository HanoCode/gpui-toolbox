//! URL 百分号编解码，口径 = 浏览器 `encodeURIComponent` / `decodeURIComponent`。
//!
//! 参考仓 `CryptoBundle.vue` 的「URL」标签页就是这两行 JS。标准库没有等价物，
//! `form_urlencoded` 也不一样（它把空格变 `+`、把 `!` `'` `(` `)` 也转义），
//! 所以这里按 ECMAScript 规范 §19.2.6.5 的**保留字符集**自己实现：
//!
//! 不转义的只有 `A-Z a-z 0-9` 与 `- _ . ! ~ * ' ( )`，其余一律 UTF-8 逐字节
//! 转成大写 `%XX`。`encodeURI` 比它少转义 `# $ & + , / : ; = ? @` 那 11 个，
//! 这里**不**采用——源仓用的是 `encodeURIComponent`。

/// 是否需要原样保留（`encodeURIComponent` 的 unreserved + `!~*'()` 子集）。
fn keep(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

/// 文本 → `encodeURIComponent` 形态。
#[must_use]
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        if keep(*byte) {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// `encodeURIComponent` 形态 → 文本。
///
/// # Errors
/// `%` 后面不是两位十六进制时返回错误（浏览器这时候抛 `URIError`，
/// 源仓接住后提示「解码失败」）。解出的字节不是合法 UTF-8 时按替换字符处理，
/// 对齐 `decodeURIComponent` 之外那层 `TextDecoder` 的宽松行为。
pub fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .ok_or_else(|| "URL 解码失败：% 后面不足两位十六进制".to_owned())?;
                let pair = std::str::from_utf8(hex)
                    .map_err(|_| "URL 解码失败：% 后面不是 ASCII 字符".to_owned())?;
                let byte = u8::from_str_radix(pair, 16)
                    .map_err(|_| format!("URL 解码失败：%{} 不是合法的十六进制", pair))?;
                out.push(byte);
                index += 3;
            }
            // `+` 在 `decodeURIComponent` 里**不**当空格（那是表单编码的规则）。
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_javascript_unreserved_set() {
        // 浏览器 console：encodeURIComponent("a-b_c.d!e~f*g'h(i)j")
        assert_eq!(encode("a-b_c.d!e~f*g'h(i)j"), "a-b_c.d!e~f*g'h(i)j");
        // 这 11 个 `encodeURI` 才保留，`encodeURIComponent` 必须转义。
        assert_eq!(encode("#$&+,/:;=?@"), "%23%24%26%2B%2C%2F%3A%3B%3D%3F%40");
        assert_eq!(encode(" "), "%20");
        // 多字节一律按 UTF-8 逐字节转义、大写 hex。
        assert_eq!(encode("恒"), "%E6%81%92");
        assert_eq!(encode("🦆"), "%F0%9F%A6%86");
    }

    #[test]
    fn round_trips() {
        for text in ["", "a b", "https://x.cn/a?b=1&c=恒生", "100%"] {
            assert_eq!(decode(&encode(text)).expect("解码"), text);
        }
    }

    #[test]
    fn plus_is_not_a_space() {
        assert_eq!(decode("a+b").expect("解码"), "a+b");
    }

    #[test]
    fn rejects_malformed_escapes() {
        assert!(decode("%").is_err());
        assert!(decode("%2").is_err());
        assert!(decode("%zz").is_err());
    }
}
