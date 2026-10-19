//! JWT 解析（只拆不验签）：Header / Payload 两段 Base64URL → 美化 JSON。
//!
//! 口径 = 参考仓 `CryptoBundle.vue` 的 `decodeJwt` / `base64UrlDecode`：
//! 必须正好三段（`header.payload.signature`），前两段做 Base64URL 解码
//! （`-`→`+`、`_`→`/`、补 `=`）再 `JSON.stringify(JSON.parse(x), null, 2)`。
//!
//! 签名段**不看**：参考仓也不看。这是「看清 token 里装了什么」的工具，
//! 不是校验器——把未验签的内容标成「有效」才是错的。

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 拆出来的两段（都已美化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtParts {
    /// Header 的 JSON 美化文本
    pub header: String,
    /// Payload 的 JSON 美化文本
    pub payload: String,
}

/// 第一段的 base64url 原文（调试用）。
pub fn base64_url_decode(segment: &str) -> Result<String, String> {
    let cleaned = segment.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD
        .decode(cleaned)
        .map_err(|error| format!("Base64URL 解码失败：{}", error))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 解析 token。
///
/// # Errors
/// 段数不是三段时给 [`INVALID_SHAPE`]；某段不是 Base64URL 或不是 JSON 时给
/// 「JWT 解析失败：…」。
pub fn decode(token: &str) -> Result<JwtParts, String> {
    let segments: Vec<&str> = token.trim().split('.').collect();
    if segments.len() != 3 {
        return Err(INVALID_SHAPE.to_owned());
    }
    Ok(JwtParts {
        header: segment_json("Header", segments[0])?,
        payload: segment_json("Payload", segments[1])?,
    })
}

/// 一段 → Base64URL 解码 → JSON 美化（`JSON.stringify(v, null, 2)` 的形态）。
fn segment_json(name: &str, segment: &str) -> Result<String, String> {
    let text = base64_url_decode(segment)?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} 不是合法 JSON：{}", name, error))?;
    serde_json::to_string_pretty(&value).map_err(|error| format!("{} 无法格式化：{}", name, error))
}

/// 段数不对时的固定文案（源仓原文）。
pub const INVALID_SHAPE: &str = "无效的 JWT 格式（应为 header.payload.signature 三段）";

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实结构的 token：`{"alg":"HS256","typ":"JWT"}` / `{"sub":"admin","exp":1700000000}`。
    const TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                         eyJzdWIiOiJhZG1pbiIsImV4cCI6MTcwMDAwMDAwMH0.\
                         c2lnbmF0dXJl";

    #[test]
    fn splits_header_and_payload() {
        let parts = decode(TOKEN).expect("解析");
        assert!(
            parts.header.contains("\"alg\": \"HS256\""),
            "{}",
            parts.header
        );
        assert!(
            parts.header.contains("\"typ\": \"JWT\""),
            "{}",
            parts.header
        );
        assert!(
            parts.payload.contains("\"sub\": \"admin\""),
            "{}",
            parts.payload
        );
        // 缩进两格且带换行 = `JSON.stringify(v, null, 2)` 的形态。
        assert!(parts.header.contains('\n'));
        assert!(parts.payload.contains("1700000000"));
    }

    #[test]
    fn round_trips_base64url_with_url_safe_alphabet() {
        // 含 `-` / `_` 的段不能按标准字母表解（会直接报错）。
        let payload = "eyJhIjoiPz8_Pz8ifQ"; // {"a":"?? ???"} 之类，只为覆盖 -/_
        let text = base64_url_decode(payload);
        assert!(text.is_ok(), "{text:?}");
        // 补位 `=` 也要能吃下。
        assert_eq!(base64_url_decode("YQ==").expect("解码"), "a");
        assert_eq!(base64_url_decode("YQ").expect("解码"), "a");
    }

    #[test]
    fn rejects_wrong_segment_count() {
        assert_eq!(decode("a.b").expect_err("应当报错"), INVALID_SHAPE);
        assert_eq!(decode("a.b.c.d").expect_err("应当报错"), INVALID_SHAPE);
    }

    #[test]
    fn rejects_non_json_segments() {
        // "not-json" 的 base64url 形态：解出来不是 JSON。
        let error = decode("bm90LWpzb24.bm90LWpzb24.sig").expect_err("应当报错");
        assert!(error.contains("不是合法 JSON"), "{error}");
    }
}
