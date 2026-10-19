//! SigV4 签名（S3 专用）。
//!
//! 参考仓用 `rust-s3` crate 做这件事，本仓**不引**：`rust-s3` 捆 `reqwest` +
//! `tokio`，与「插件进程零 async、跑一次就退」直接冲突。这里只实现 S3 需要的那
//! 一份（header-based，不做 presigned query 签名），来源是源仓
//! `net/conn_check.rs::sigv4_headers`（连通检测里那段，逐行对齐 AWS 文档），
//! 并泛化到「任意方法 + 任意 query + 任意额外头」——连通检测只签空体 `HEAD`，
//! 上传/复制要签 body 与 `x-amz-copy-source`。
//!
//! 三条容易写错、写错只得到一句 `SignatureDoesNotMatch` 的地方：
//!
//! 1. **canonical URI 要编码**，但 `/` 不编码（`a/b c.txt` → `/a/b%20c.txt`）；
//! 2. **canonical query 要按编码后的 key 排序**，不是按原样；
//! 3. **`Host` 头必须带端口**（`10.0.0.5:9000`），否则自建 MinIO 一律 403。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub(crate) type HmacSha256 = Hmac<Sha256>;

/// 一把凭据。**空 `access_key` = 匿名**（公共桶），调用方据此决定要不要签名。
#[derive(Debug, Clone, Copy)]
pub struct Credentials<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

/// 一次签名请求的全部输入。字段都借出去，签名本身零分配之外的开销。
pub struct SignInput<'a> {
    pub method: &'a str,
    /// 主机（**带端口**）。
    pub host: &'a str,
    /// canonical URI，调用方用 [`encode_path`] 编好。
    pub path: &'a str,
    /// 查询参数（原样子，未编码；内部复制排序编码）。
    pub query: &'a [(String, String)],
    /// 额外参与签名的头（`x-amz-copy-source` / `content-type` 之类）。
    /// 名字大小写不敏感，内部转小写。
    pub headers: &'a [(String, String)],
    pub payload: &'a [u8],
    pub region: &'a str,
    pub credentials: Credentials<'a>,
}

/// 签出来的三个头（`Authorization` / `x-amz-date` / `x-amz-content-sha256`）。
///
/// 顺序固定（和参考仓一致）：`Authorization` 在第一个，方便调用方只取它。
pub fn sign(input: &SignInput<'_>) -> Vec<(String, String)> {
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(input.payload);

    // `host` + 两个 `x-amz-*` 是必备项，额外头插进来一起排序。
    let mut signed: Vec<(String, String)> = vec![
        ("host".to_string(), input.host.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    for (name, value) in input.headers {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name == "host" || name == "x-amz-date" {
            continue;
        }
        if name == "x-amz-content-sha256" {
            continue;
        }
        signed.push((name, value.trim().to_string()));
    }
    // 名字唯一（同名头后者覆盖前者）：S3 不接受重复的 canonical header 名。
    signed.sort_by(|a, b| a.0.cmp(&b.0));
    signed.dedup_by(|a, b| a.0 == b.0);

    let canonical_headers: String = signed
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = signed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.path,
        canonical_query(input.query),
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let scope = format!("{date_stamp}/{}/s3/aws4_request", input.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let key = format!("AWS4{}", input.credentials.secret_key);
    let k_date = hmac_sign(key.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sign(&k_date, input.region.as_bytes());
    let k_service = hmac_sign(&k_region, b"s3");
    let k_signing = hmac_sign(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sign(&k_signing, string_to_sign.as_bytes()));

    vec![
        (
            "Authorization".to_string(),
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                input.credentials.access_key
            ),
        ),
        ("x-amz-date".to_string(), amz_date),
        ("x-amz-content-sha256".to_string(), payload_hash),
    ]
}

/// canonical query string：编码后按 key 排序，`key=value` 与 `&` 拼。
///
/// 值为空的参数也要 `key=`（S3 的 `?delete` / `?acl` 子资源就是这样）。
pub fn canonical_query(params: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| (encode_component(key), encode_component(value)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// AWS 口径的 percent-encode：只留 `A-Z a-z 0-9 - _ . ~`。
pub fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 对象 key → canonical URI 的一段：与 [`encode_component`] 同口径，但**保留 `/`**。
pub fn encode_path(raw: &str) -> String {
    raw.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// 一次 HMAC-SHA256（`key` 是**上一层的摘要**，不是上一层的 HMAC 状态）。
///
/// ⚠️ 这里绝不能写成「clone 上一层的 `Hmac` 再 update」：那样算出来的是
/// `HMAC(K, m1 || m2)`，而 AWS 的签名密钥是 `HMAC(HMAC(...))` 逐层**取摘要**再当
/// 下一层的 key。两者只在第一层 coincidentally 相近，第二层开始就分叉；
/// 症状是每个请求都得到 `SignatureDoesNotMatch`（403），而且看起来特别像
/// 「AK/SK 填错了」。
pub(crate) fn hmac_sign(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_the_aws_rules() {
        // AWS 文档里的例子：空格 → `%20`，`+` → `%2B`（不是 `+`），`~` 不编码。
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("a+b"), "a%2Bb");
        assert_eq!(encode_component("a~b-c_d.e"), "a~b-c_d.e");
        assert_eq!(encode_component("中文"), "%E4%B8%AD%E6%96%87");
        // 路径保留 `/`，段内的特殊字符照样编码。
        assert_eq!(encode_path("/a b/c+d.txt"), "/a%20b/c%2Bd.txt");
        assert_eq!(encode_path("/"), "/");
    }

    #[test]
    fn query_is_sorted_after_encoding_not_before() {
        let params = vec![
            ("prefix".to_string(), "a/b".to_string()),
            ("list-type".to_string(), "2".to_string()),
            ("delimiter".to_string(), "/".to_string()),
            ("marker".to_string(), String::new()),
        ];
        assert_eq!(
            canonical_query(&params),
            "delimiter=%2F&list-type=2&marker=&prefix=a%2Fb"
        );
    }

    /// AWS 文档里的经典向量（`GET /test.txt` + `Range: bytes=0-9`，`2013-05-24T00:00:00Z`）。
    ///
    /// `sign()` 取系统时间，所以这里单独把「固定时间」那条路复刻一遍，钉住
    /// canonical request → signature 的整条链；`sign()` 与它的差别只有时间来源。
    /// 期望签名取自 AWS 文档《Signature Calculations for the Authorization Header》
    /// 的 `GET Object` 示例。
    #[test]
    fn signature_matches_the_aws_documented_vector() {
        let payload_hash = sha256_hex(b"");
        // 多头 + 排序：`host` < `range` < `x-amz-content-sha256` < `x-amz-date`。
        let canonical_headers = format!(
            "host:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\nx-amz-content-sha256:{payload_hash}\nx-amz-date:20130524T000000Z\n"
        );
        let signed_headers = "host;range;x-amz-content-sha256;x-amz-date";
        let canonical_request =
            format!("GET\n/test.txt\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

        // AWS 文档给的中间值：canonical request 的 SHA256。
        assert_eq!(
            sha256_hex(canonical_request.as_bytes()),
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972",
            "canonical request 与 AWS 文档不一致:\n{canonical_request}"
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sign(b"AWS4wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", b"20130524");
        let k_region = hmac_sign(&k_date, b"us-east-1");
        let k_service = hmac_sign(&k_region, b"s3");
        let k_signing = hmac_sign(&k_service, b"aws4_request");
        let signature = hex::encode(hmac_sign(&k_signing, string_to_sign.as_bytes()));
        assert_eq!(
            signature, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
            "SigV4 的 canonical request 或 HMAC 链与 AWS 文档不一致"
        );
    }

    #[test]
    fn extra_headers_join_the_signed_set_in_order() {
        let headers = sign(&SignInput {
            method: "PUT",
            host: "minio.lan:9000",
            path: "/demo/a.txt",
            query: &[],
            headers: &[("x-amz-copy-source".to_string(), "/demo/b.txt".to_string())],
            payload: b"hello",
            region: "us-east-1",
            credentials: Credentials {
                access_key: "AK",
                secret_key: "SK",
            },
        });
        let auth = &headers[0].1;
        assert!(
            auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-copy-source;x-amz-date"),
            "{auth}"
        );
        // body 变了，payload hash 也必须变（否则 PUT 一定 SignatureDoesNotMatch）。
        assert_ne!(
            headers[2].1,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
