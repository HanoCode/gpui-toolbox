//! 增量 SHA-256，以及摘要的形态检查。
//!
//! 与 `duck-update::sha256` 是同一份实现。**刻意不复用**：让插件系统依赖更新器
//! （或反过来）只为省下这几十行，换来的是两个领域的构建与语义被绑在一起。二者
//! 都是「边收边算、不整包进内存」的小工具，各自演进、各自单测更划算。

use sha2::{Digest as _, Sha256};

/// SHA-256 的十六进制写法长度。
pub const HEX_LEN: usize = 64;

/// 边收边算的哈希器。
///
/// 存在的理由只有一个：下载归档时**不把整包读进内存**。字节是分块来的，
/// 每块顺手喂给它，峰值占用就是一个 64 KiB 的缓冲。
#[derive(Default)]
pub struct Hasher(Sha256);

impl Hasher {
    /// 空哈希器。
    #[must_use]
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    /// 喂一块字节。
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// 收尾，得到 64 位小写十六进制摘要。
    #[must_use]
    pub fn finalize_hex(self) -> String {
        to_hex(&self.0.finalize())
    }
}

/// 是否为 64 位**小写**十六进制摘要。
///
/// 只认小写：索引是我们自己生成的，宽松只会把「生成端写错了大小写」这种问题
/// 一路拖到比对失败才暴露。
#[must_use]
pub fn is_hex_digest(text: &str) -> bool {
    text.len() == HEX_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 两个摘要是否相同（大小写不敏感）。
#[must_use]
pub fn matches(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Hasher, is_hex_digest, matches};

    /// 空输入的 SHA-256 是个众所周知的常数，用来钉住「同一份字节得到同一个摘要」。
    #[test]
    fn an_empty_input_has_the_known_digest() {
        assert_eq!(
            Hasher::new().finalize_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// 分块喂进去与一次喂完必须得到同一个摘要——否则流式下载的校验就没有意义。
    #[test]
    fn chunked_and_whole_inputs_agree() {
        let mut chunked = Hasher::new();
        chunked.update(b"duck");
        chunked.update(b" ");
        chunked.update(b"toolbox");
        let mut whole = Hasher::new();
        whole.update(b"duck toolbox");

        assert_eq!(chunked.finalize_hex(), whole.finalize_hex());
    }

    #[test]
    fn truncated_uppercase_and_non_hex_digests_are_refused() {
        assert!(!is_hex_digest("0a404f82"), "truncated");
        assert!(!is_hex_digest(&"A".repeat(64)), "uppercase");
        assert!(!is_hex_digest(&"z".repeat(64)), "not hex");
        assert!(!is_hex_digest(""), "empty");
    }

    #[test]
    fn comparison_ignores_case_only() {
        assert!(matches("abc", "ABC"));
        assert!(!matches("abc", "abd"));
    }
}
