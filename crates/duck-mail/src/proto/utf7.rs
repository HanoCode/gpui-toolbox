//! IMAP 邮箱名的 Modified UTF-7 解码（RFC 3501 §5.1.3）。
//!
//! IMAP `LIST` 返回的邮箱名默认是 **Modified UTF-7**：非 ASCII 段用改版 base64
//! （`,` 代替 `/`、省略 padding）编码，`&…-` 定界——于是「已发送」在界面上就是
//! `&XfJT0ZAB-` 这种乱码。库存**原始名**（`SELECT` / 同步等命令要用），显示层
//! 用 [`decode_mailbox_name`] 解回 UTF-8。
//!
//! 解码是**尽力而为**：不含 `&…-` 的名字原样返回；坏的编码段（非法字符、
//! 孤立代理项、缺 `-` 收尾）整段保持原样，绝不 panic、绝不丢字。

/// 把 IMAP Modified UTF-7 邮箱名解成 UTF-8 显示名。
///
/// - `&-` → `&`（RFC 规定的转义）；
/// - `&<base64>-` → 对应的 UTF-16 序列（含代理项对）；
/// - 其余字符原样保留。
pub fn decode_mailbox_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // 多字节字符按字节拷回原串是安全的：`&`（0x26）不可能出现在
            // UTF-8 多字节序列内部，切到下一个字符边界即可。
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&name[start..i]);
            continue;
        }
        // `&` 之后先找收尾 `-`。
        let Some(end) = bytes[i + 1..]
            .iter()
            .position(|&b| b == b'-')
            .map(|p| i + 1 + p)
        else {
            // 没有 `-`：整段原样保留（不是编码）。
            out.push_str(&name[i..]);
            break;
        };
        let body = &name[i + 1..end];
        if body.is_empty() {
            out.push('&');
            i = end + 1;
            continue;
        }
        match decode_body(body) {
            Some(decoded) => out.push_str(&decoded),
            // 坏段原样保留：显示成 `&XX??YY-` 比丢字强。
            None => out.push_str(&name[i..=end]),
        }
        i = end + 1;
    }
    out
}

/// 解码一个 `&…-` 段的 base64 体。改版 base64：`,` 代替 `/`、无 padding。
fn decode_body(body: &str) -> Option<String> {
    let mut acc: u32 = 0;
    let mut acc_bits: u32 = 0;
    let mut units: Vec<u16> = Vec::with_capacity(body.len() * 6 / 16 + 1);
    for byte in body.bytes() {
        let value = match byte {
            b'A'..=b'Z' => (byte - b'A') as u32,
            b'a'..=b'z' => (byte - b'a' + 26) as u32,
            b'0'..=b'9' => (byte - b'0' + 52) as u32,
            b'+' => 62,
            b',' => 63,
            _ => return None,
        };
        acc = (acc << 6) | value;
        acc_bits += 6;
        if acc_bits >= 16 {
            acc_bits -= 16;
            units.push(((acc >> acc_bits) & 0xFFFF) as u16);
            // 只留下「还没成对」的低位，别让 acc 无限左移（否则高位迟早溢出）。
            acc &= (1u32 << acc_bits) - 1;
        }
    }
    // 末尾不足 16 bit 的是 padding 位，按 RFC 直接丢弃；但一个完整 16-bit
    // 单元都凑不出来（如 `&D-`）不是合法编码——整段保持原样，别解成空串。
    if units.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(units.len());
    let mut iter = units.into_iter();
    while let Some(unit) = iter.next() {
        let ch = match unit {
            // 高代理项必须紧跟低代理项，组成一个增补平面字符。
            0xD800..=0xDBFF => match iter.next() {
                Some(low @ 0xDC00..=0xDFFF) => {
                    let code =
                        0x1_0000 + (((unit as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
                    char::from_u32(code)?
                }
                _ => return None,
            },
            // 孤立的低代理项 / 代理区其他值都不是合法 UTF-16。
            0xDC00..=0xDFFF => return None,
            _ => char::from_u32(unit as u32)?,
        };
        out.push(ch);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode_mailbox_name;

    #[test]
    fn plain_ascii_passes_through() {
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(decode_mailbox_name("Sent"), "Sent");
        assert_eq!(decode_mailbox_name("Work/2026"), "Work/2026");
        assert_eq!(decode_mailbox_name(""), "");
    }

    #[test]
    fn ampersand_escape() {
        // `&-` 是 `&` 本身。
        assert_eq!(decode_mailbox_name("R&D-"), "R&D-");
        assert_eq!(decode_mailbox_name("A&-B"), "A&B");
    }

    #[test]
    fn rfc3501_example() {
        // RFC 3501 §5.1.3 的示例邮箱名。
        assert_eq!(
            decode_mailbox_name("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
    }

    #[test]
    fn chinese_folder_names() {
        // 真实服务器（QQ / 163 / Gmail 中文界面）常见的内置文件夹；编码值由
        // 独立实现核对过，不是手算的。
        assert_eq!(decode_mailbox_name("&XfJT0ZAB-"), "已发送");
        assert_eq!(decode_mailbox_name("&XfJSIJZk-"), "已删除");
        assert_eq!(decode_mailbox_name("&g0l6P3ux-"), "草稿箱");
        assert_eq!(decode_mailbox_name("&V4NXPpCuTvY-"), "垃圾邮件");
        assert_eq!(decode_mailbox_name("&kK5O9l9SaGM-"), "邮件归档");
        // 混合：ASCII 前缀 + 编码段 + ASCII 后缀。
        assert_eq!(
            decode_mailbox_name("[QQ Mail]/&XfJT0ZAB-"),
            "[QQ Mail]/已发送"
        );
    }

    #[test]
    fn surrogate_pair_decodes_emoji() {
        // 😀 = U+1F600 = UTF-16 D83D DE00 = `&2D3eAA-`。
        assert_eq!(decode_mailbox_name("&2D3eAA-"), "\u{1F600}");
    }

    #[test]
    fn malformed_segments_stay_verbatim() {
        // 非法 base64 字符：整段原样。
        assert_eq!(decode_mailbox_name("&??x-"), "&??x-");
        // 孤立高代理项（D83D）：整段原样。
        assert_eq!(decode_mailbox_name("&2D0-"), "&2D0-");
        // 孤立低代理项（DDE0）：整段原样。
        assert_eq!(decode_mailbox_name("&3eA-"), "&3eA-");
        // 凑不出一个 16-bit 单元：原样（别解成空串）。
        assert_eq!(decode_mailbox_name("R&D-"), "R&D-");
        // 没有 `-` 收尾：原样。
        assert_eq!(decode_mailbox_name("abc&XfJT"), "abc&XfJT");
        // 好坏混排：坏的保留、好的照解。
        assert_eq!(decode_mailbox_name("&??-&XfJT0ZAB-"), "&??-已发送");
    }
}
