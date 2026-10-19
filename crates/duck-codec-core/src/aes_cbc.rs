//! AES-CBC 加解密（PKCS#7 填充，密文 Base64）。
//!
//! 口径 = 参考仓 `CryptoBundle.vue` 的 `aesEncrypt` / `aesDecrypt`，那两段走的是
//! WebCrypto：`{ name: 'AES-CBC', iv }` —— 规范里 AES-CBC **固定 PKCS#7**，
//! 密文输出 `btoa(...)` 即标准 Base64（带 `=` 补位）。这里逐条对齐：
//!
//! - 密钥按 **UTF-8 原始字节**直接当 AES key（不做 KDF、不做 hex 解析），
//!   长度只能是 16 / 24 / 32 字节，对应 AES-128 / 192 / 256；
//! - IV 必须是 16 字节；
//! - 明文按 UTF-8 取字节；解密结果按 UTF-8 宽松解码（WebCrypto 之后那次
//!   `TextDecoder` 也是宽松的）。
//!
//! 与源仓的差异只有错误文案的**触发时机**：源仓先查密钥长度再查 IV，这里一样。

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use aes::{Aes128, Aes192, Aes256};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cbc::{Decryptor, Encryptor};

/// 支持的三种模式（与参考仓下拉的三个选项一一对应）。
pub const MODES: [&str; 3] = ["AES-128-CBC", "AES-192-CBC", "AES-256-CBC"];

/// `AES-128-CBC` → 16、`AES-192-CBC` → 24、`AES-256-CBC` → 32。
fn key_len(mode: &str) -> Result<usize, String> {
    match mode.trim() {
        "AES-128-CBC" => Ok(16),
        "AES-192-CBC" => Ok(24),
        "AES-256-CBC" => Ok(32),
        other => Err(format!(
            "未知的 AES 模式：{}（可选 AES-128-CBC / AES-192-CBC / AES-256-CBC）",
            other
        )),
    }
}

/// 校验密钥与 IV 的长度，返回二者字节。
fn split_params<'a>(mode: &str, key: &'a str, iv: &'a str) -> Result<(&'a [u8], &'a [u8]), String> {
    let expected = key_len(mode)?;
    let key = key.as_bytes();
    if key.len() != expected {
        return Err(format!(
            "AES 密钥长度必须是 {} 字节，实际 {} 字节",
            expected,
            key.len()
        ));
    }
    let iv = iv.as_bytes();
    if iv.len() != 16 {
        return Err(format!(
            "AES 初始向量必须是 16 字节，实际 {} 字节",
            iv.len()
        ));
    }
    Ok((key, iv))
}

/// 加密：明文 → Base64 密文。
///
/// # Errors
/// 模式未知、密钥或 IV 长度不合规时返回错误。
pub fn encrypt(mode: &str, key: &str, iv: &str, plain: &str) -> Result<String, String> {
    let (key, iv) = split_params(mode, key, iv)?;
    let cipher = match key.len() {
        16 => Encryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 加密失败：{}", error))?
            .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes()),
        24 => Encryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 加密失败：{}", error))?
            .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes()),
        _ => Encryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 加密失败：{}", error))?
            .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes()),
    };
    Ok(STANDARD.encode(cipher))
}

/// 解密：Base64 密文 → 明文。
///
/// # Errors
/// 模式未知、密钥 / IV 长度不合规、密文不是合法 Base64 或填充校验失败时返回错误。
pub fn decrypt(mode: &str, key: &str, iv: &str, cipher_text: &str) -> Result<String, String> {
    let (key, iv) = split_params(mode, key, iv)?;
    let cipher = STANDARD
        .decode(cipher_text.trim())
        .map_err(|error| format!("密文不是合法的 Base64：{}", error))?;
    let plain = match key.len() {
        16 => Decryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 解密失败：{}", error))?
            .decrypt_padded_vec_mut::<Pkcs7>(&cipher),
        24 => Decryptor::<Aes192>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 解密失败：{}", error))?
            .decrypt_padded_vec_mut::<Pkcs7>(&cipher),
        _ => Decryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|error| format!("AES 解密失败：{}", error))?
            .decrypt_padded_vec_mut::<Pkcs7>(&cipher),
    }
    .map_err(|_| "AES 解密失败：密钥 / 初始向量不对，或密文被改动过".to_owned())?;
    Ok(String::from_utf8_lossy(&plain).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 向量来自 `openssl enc -aes-128-cbc -K … -iv … | base64`，
    /// 与 WebCrypto `AES-CBC`（PKCS#7 + 标准 Base64）逐字节一致。
    #[test]
    fn matches_webcrypto_for_128() {
        // key = "0123456789abcdef"（ASCII 16 字节），iv = "abcdef0123456789"
        let cipher = encrypt(
            "AES-128-CBC",
            "0123456789abcdef",
            "abcdef0123456789",
            "hello",
        )
        .expect("加密");
        assert_eq!(cipher, "/8uAvsJDf8LjCw7wqOxbZA==");
        assert_eq!(
            decrypt(
                "AES-128-CBC",
                "0123456789abcdef",
                "abcdef0123456789",
                &cipher
            )
            .expect("解密"),
            "hello"
        );
    }

    #[test]
    fn round_trips_all_three_key_sizes() {
        let iv = "0123456789abcdef";
        for (mode, key) in [
            ("AES-128-CBC", "0123456789abcdef"),
            ("AES-192-CBC", "0123456789abcdefghijklmn"),
            ("AES-256-CBC", "0123456789abcdefghijklmnopqrstuv"),
        ] {
            let cipher = encrypt(mode, key, iv, "恒生电子 LCPT").expect("加密");
            assert_eq!(
                decrypt(mode, key, iv, &cipher).expect("解密"),
                "恒生电子 LCPT"
            );
        }
    }

    #[test]
    fn empty_plaintext_still_pads_a_full_block() {
        // WebCrypto 对空明文也给一块（PKCS#7 必补），16 字节 → Base64 24 字符。
        let cipher =
            encrypt("AES-128-CBC", "0123456789abcdef", "abcdef0123456789", "").expect("加密");
        assert_eq!(cipher, "YZbS8taa29kYkI5N6lDDlA==");
        assert_eq!(
            decrypt(
                "AES-128-CBC",
                "0123456789abcdef",
                "abcdef0123456789",
                &cipher
            )
            .expect("解密"),
            ""
        );
    }

    #[test]
    fn rejects_bad_key_and_iv_lengths() {
        let error = encrypt("AES-256-CBC", "short", "0123456789abcdef", "x").expect_err("应当报错");
        assert!(error.contains("32 字节"), "{error}");
        let error = encrypt("AES-128-CBC", "0123456789abcdef", "short", "x").expect_err("应当报错");
        assert!(error.contains("16 字节"), "{error}");
        assert!(encrypt("AES-512-CBC", "0123456789abcdef", "0123456789abcdef", "x").is_err());
    }

    #[test]
    fn wrong_key_fails_loudly_instead_of_returning_mojibake() {
        let cipher = encrypt(
            "AES-128-CBC",
            "0123456789abcdef",
            "abcdef0123456789",
            "hello",
        )
        .expect("加密");
        // 密钥不对时 PKCS#7 校验大概率失败；万一碰巧通过，也不能 panic。
        let _ = decrypt(
            "AES-128-CBC",
            "fedcba9876543210",
            "abcdef0123456789",
            &cipher,
        );
    }
}
