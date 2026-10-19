//! 文件哈希：对一个文件同时算 MD5 / SHA-1 / SHA-256 / SHA-384 / SHA-512。
//!
//! **逐字复刻**参考仓 `ifm_ai_tools/src-tauri/src/lib.rs` 的
//! `compute_file_hash` 命令体（两处适配：Tauri async 命令 → 同步函数；
//! `HashResult` 从 `lib.rs` 挪进本模块）。算法、口径（小写 hex）与
//! 「整文件读入内存」的取字节方式都保持原样——参考实现一次 `read_to_end`，
//! 对实施场景里的包 / dump 文件足够；超大文件的流式化留给将来。

/// 一条哈希结果（算法名 + 小写 hex 值）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HashResult {
    /// 算法名：MD5 / SHA1 / SHA256 / SHA384 / SHA512
    pub alg: String,
    /// 小写 hex 摘要
    pub value: String,
}

/// 对 `file_path` 指向的文件算五种摘要，顺序与参考仓一致。
///
/// # Errors
/// 文件打不开或读不完时返回中文错误串（与参考仓同文案）。
pub fn compute_file_hash(file_path: &str) -> Result<Vec<HashResult>, String> {
    use md5::Md5;
    use sha2::Digest;
    use sha2::{Sha256, Sha384, Sha512};
    use std::io::Read;

    let mut file = std::fs::File::open(file_path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .map_err(|e| format!("读取文件失败: {}", e))?;

    let mut md5_hasher = Md5::new();
    md5_hasher.update(&buffer);
    let md5_result = format!("{:x}", md5_hasher.finalize());

    let mut sha1_hasher = sha1::Sha1::new();
    sha1_hasher.update(&buffer);
    let sha1_result = format!("{:x}", sha1_hasher.finalize());

    let mut sha256_hasher = Sha256::new();
    sha256_hasher.update(&buffer);
    let sha256_result = format!("{:x}", sha256_hasher.finalize());

    let mut sha384_hasher = Sha384::new();
    sha384_hasher.update(&buffer);
    let sha384_result = format!("{:x}", sha384_hasher.finalize());

    let mut sha512_hasher = Sha512::new();
    sha512_hasher.update(&buffer);
    let sha512_result = format!("{:x}", sha512_hasher.finalize());

    Ok(vec![
        HashResult {
            alg: "MD5".into(),
            value: md5_result,
        },
        HashResult {
            alg: "SHA1".into(),
            value: sha1_result,
        },
        HashResult {
            alg: "SHA256".into(),
            value: sha256_result,
        },
        HashResult {
            alg: "SHA384".into(),
            value: sha384_result,
        },
        HashResult {
            alg: "SHA512".into(),
            value: sha512_result,
        },
    ])
}
