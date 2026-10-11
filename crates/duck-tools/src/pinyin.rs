//! 拼音索引：从工具中文名自动派生「全拼」与「首字母」，供搜索使用。
//!
//! 这两个字段在 TS 接口里是可选的手填字段；这里改为**注册时自动派生**，
//! 避免手写 14 个字段且漏填无校验。派生结果缓存进 [`crate::RegisteredTool`]。

use pinyin::{Pinyin, ToPinyin};

/// 全拼：中文字符取无声调拼音，ASCII 字母数字保留并小写，其余字符（空格 / 标点 / 全角符号）跳过。
///
/// 例：`"HTTP 请求回放"` → `"httpqingqiuhuifang"`。
#[must_use]
pub fn plain_of(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        push_char(&mut out, ch, Pinyin::plain);
    }
    out
}

/// 首字母：中文取拼音首字母，ASCII 字母数字保留并小写，其余字符跳过。
///
/// 例：`"视频统一解析"` → `"sptyjx"`。
#[must_use]
pub fn initials_of(text: &str) -> String {
    let mut out = String::with_capacity(text.len() / 2);
    for ch in text.chars() {
        push_char(&mut out, ch, Pinyin::first_letter);
    }
    out
}

/// 单字符归一：有拼音走 `pick`，否则只保留 ASCII 字母数字。
fn push_char(out: &mut String, ch: char, pick: fn(Pinyin) -> &'static str) {
    match ch.to_pinyin() {
        Some(pinyin) => out.push_str(pick(pinyin)),
        None => {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{initials_of, plain_of};

    #[test]
    fn derives_plain_and_initials() {
        assert_eq!(plain_of("视频"), "shipin");
        assert_eq!(initials_of("视频统一解析"), "sptyjx");
        assert_eq!(plain_of("网盘直链解析"), "wangpanzhilianjiexi");
    }

    #[test]
    fn keeps_ascii_and_drops_separators() {
        // ASCII 保留并小写；空格与全角斜杠被丢掉。
        assert_eq!(plain_of("HTTP 请求回放"), "httpqingqiuhuifang");
        assert_eq!(initials_of("WebP / AVIF 互转"), "webpavifhz");
        assert_eq!(plain_of("命令片段"), "minglingpianduan");
    }

    #[test]
    fn non_cjk_without_ascii_yields_empty() {
        assert_eq!(plain_of("———"), "");
        assert_eq!(initials_of("（）"), "");
    }
}
