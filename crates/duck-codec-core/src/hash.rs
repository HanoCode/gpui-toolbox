//! 文本哈希：SHA-1 / SHA-256 / SHA-384 / SHA-512（小写 hex）。
//!
//! 参考仓「哈希」标签页的**文本**那一半：`crypto.subtle.digest` 一次算四种，
//! 展示名是 `alg.replace('SHA-','SHA')` ⇒ `SHA1` / `SHA256` / `SHA384` / `SHA512`。
//!
//! **文件**那一半（含 MD5）在 [`crate::file_hash`]，是后端复刻件，口径不同
//! （那边一次给五种、含 MD5），所以两个入口各留一份，不合并——合并就得选一边
//! 的口径，而两边都是「参考仓已验证」的。

use sha2::Digest;

/// 一条摘要（算法展示名 + 小写 hex）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestResult {
    /// 算法展示名：SHA1 / SHA256 / SHA384 / SHA512
    pub alg: String,
    /// 小写 hex 摘要
    pub value: String,
}

fn digest<D: Digest>(data: &[u8]) -> String {
    let mut hasher = D::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// 对文本算四种 SHA，顺序与参考仓一致。
#[must_use]
pub fn hash_text(text: &str) -> Vec<DigestResult> {
    let bytes = text.as_bytes();
    [
        ("SHA1", digest::<sha1::Sha1>(bytes)),
        ("SHA256", digest::<sha2::Sha256>(bytes)),
        ("SHA384", digest::<sha2::Sha384>(bytes)),
        ("SHA512", digest::<sha2::Sha512>(bytes)),
    ]
    .into_iter()
    .map(|(alg, value)| DigestResult {
        alg: alg.to_owned(),
        value,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_for_empty_and_abc() {
        let empty = hash_text("");
        assert_eq!(
            empty[0].value, "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "SHA1(空串)"
        );
        assert_eq!(
            empty[1].value, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "SHA256(空串)"
        );
        let abc = hash_text("abc");
        assert_eq!(
            abc[0].value, "a9993e364706816aba3e25717850c26c9cd0d89d",
            "SHA1(\"abc\")"
        );
        assert_eq!(
            abc[1].value, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "SHA256(\"abc\")"
        );
        // SHA-384/512 各取一处前缀，防「算法串写错、值其实还是 256」。
        assert_eq!(
            abc[2].value,
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
        );
        assert!(abc[3].value.starts_with("ddaf35a193617aba"));
    }

    #[test]
    fn order_and_names_match_the_reference_page() {
        let names: Vec<String> = hash_text("x").into_iter().map(|item| item.alg).collect();
        assert_eq!(names, ["SHA1", "SHA256", "SHA384", "SHA512"]);
    }
}
