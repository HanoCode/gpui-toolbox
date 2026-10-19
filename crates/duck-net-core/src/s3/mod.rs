//! S3 兼容对象存储的**全套操作**（`duck.net.s3-manager`）。
//!
//! 移植自参考仓 `src-tauri/src/s3/{client,types,config}.rs` + `commands/s3.rs`
//! （528 行）。那边的能力面是 bucket / object 两级的增删查 + 上传下载复制改名，
//! 这里一一对上，**一处不减**。
//!
//! ## 为什么不用 `rust-s3`
//!
//! 参考仓用 `rust-s3` crate。它捆 `reqwest` + `tokio` + `hyper` 一整套异步栈，
//! 与本仓「插件进程零 async、跑一次就退」直接冲突（也会把 `tokio` 拖进
//! `duck-net-core`，而那个 crate 的纪律是**全同步**）。业界的 S3 协议本身很小：
//! 每个操作就是一个带 SigV4 头的 HTTP 请求，[`sign`] 里已经有了签名器。
//! 于是这里的做法与 [`crate::conn`] 的连通检测完全一致：自己拼请求。
//!
//! ## 与参考仓的两处行为差异（都是有意的）
//!
//! 1. **不自动跟随重定向**：`301` 几乎只有一种成因 —— endpoint 与桶的真实区域
//!    不一致。跟着跳会把已签名的 `Authorization` 带去新主机，得到的是一句
//!    `SignatureDoesNotMatch`，而真正的原因（区域填错了）被埋掉。这里把 301
//!    原样交给调用方，并带上响应里的 `x-amz-bucket-region`。
//! 2. **传输有上限**：参考仓 `upload_file` / `download_file` 无脑 `read_to_end`
//!    整个文件进内存，几百 MB 的对象会直接把进程打爆。这里上限 [`MAX_TRANSFER`]
//!    （64 MB），超了报 `too_large` 并说明原因 —— 与本仓其余「有上限」的地方
//!    （`MAX_BODY` / `MAX_HOSTS_BYTES`）同一口径。
//!
//! ## 凭据
//!
//! 不在内核里落盘：`access_key` / `secret_key` 只是 [`S3Config`] 的字段，
//! 由插件层从运行请求里拿。参考仓的 `s3_connections.json`（明文存 AK/SK）
//! **不迁** —— 本仓的凭据一律进 `vault`，而 process 轨拿不到 vault，
//! 所以连接参数每次由执行页填（与 `net-s3-ftp-checker` 同一个取舍）。

pub mod sign;
pub mod xml;

use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::{ErrorKind, NetError, NetResult};

/// 单个对象的传输上限（上传与下载共用）。
pub const MAX_TRANSFER: usize = 64 * 1024 * 1024;

/// 列举时单页最大条数（S3 上限 1000）。
pub const PAGE_SIZE: u32 = 1_000;

/// 连接参数（对应参考仓 `S3Connection`，去掉 `id` / `name` —— 那是本地连接簿
/// 的概念，本仓不留连接簿）。
#[derive(Debug, Clone)]
pub struct S3Config {
    /// `https://s3.amazonaws.com` / `http://minio.lan:9000`（尾部 `/` 吃掉）。
    pub endpoint: String,
    /// 签名域；留空用 `us-east-1`。
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// 路径风格（`host/bucket/key`）。自建 MinIO / Ceph 基本都要开。
    pub path_style: bool,
    /// 跳过证书校验（内网自签）。
    pub insecure: bool,
    pub timeout_ms: u64,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            region: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            // 参考仓默认 false，但它的界面在「自定义 endpoint」时默认勾上。
            // 本仓只有自定义 endpoint 这一种用法（没有 AWS 区域下拉），所以默认开。
            path_style: true,
            insecure: false,
            timeout_ms: 30_000,
        }
    }
}

/// 一个存储桶。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub name: String,
    /// Unix 秒；解析不出来是 0。
    pub creation_date: i64,
}

/// 一个对象（或一个「目录」前缀）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub last_modified: i64,
    pub etag: String,
    pub storage_class: String,
    /// 由 `<CommonPrefixes>` 来的是 true（界面上画成文件夹）。
    pub is_prefix: bool,
}

/// 一页列举结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListPage {
    pub objects: Vec<Object>,
    pub truncated: bool,
    /// `IsTruncated = true` 时的续页游标。
    pub next_token: Option<String>,
}

/// S3 客户端。一次运行建一个，`Clone` 没有意义（内部持有 `ureq::Agent`）。
pub struct Client {
    config: S3Config,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(config: S3Config) -> NetResult<Self> {
        let endpoint = config.endpoint.trim().trim_end_matches('/').to_string();
        if endpoint.is_empty() {
            return Err(NetError::new(ErrorKind::EmptyInput));
        }
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(NetError::with_detail(ErrorKind::BadUrl, endpoint));
        }
        let region = match config.region.trim() {
            "" => "us-east-1".to_string(),
            given => given.to_string(),
        };
        let access_key = config.access_key.trim().to_string();
        let secret_key = config.secret_key.trim().to_string();
        let config = S3Config {
            endpoint,
            region,
            access_key,
            secret_key,
            ..config
        };

        // `max_redirects(0)`：见模块头的差异说明 1，301 必须原样交给调用方。
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(config.timeout_ms.max(1_000))))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent("DuckToolbox/0.2");
        if config.insecure {
            builder = builder.tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            );
        }
        Ok(Self {
            config,
            agent: builder.build().into(),
        })
    }

    pub fn config(&self) -> &S3Config {
        &self.config
    }

    /// `scheme://host[:port]`（不含路径）。
    fn origin(&self) -> String {
        self.config.endpoint.clone()
    }

    /// `host[:port]`，签名里的 `Host` 头（**必须带端口**）。
    fn host(&self) -> String {
        self.origin()
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    /// 一个对象的完整 URL。`key` 是**原始**（未编码）的 key。
    pub fn object_url(&self, bucket: &str, key: &str) -> String {
        let key = key.trim_start_matches('/');
        // 桶级接口（`ListObjects` / `CreateBucket` / `DeleteBucket`）的规范形态是
        // `/bucket`，**不带**尾斜杠；带了虽然多数实现也认，但 canonical URI 会跟着
        // 变，代理与网关上的缓存键也不一样。
        let encoded = match key.is_empty() {
            true => String::new(),
            false => format!("/{}", encode_key(key)),
        };
        if self.config.path_style {
            format!("{}/{bucket}{encoded}", self.origin())
        } else {
            let (scheme, host) = self
                .origin()
                .split_once("://")
                .map(|(scheme, rest)| (scheme.to_string(), rest.to_string()))
                .unwrap_or_else(|| ("https".to_string(), self.origin()));
            format!("{scheme}://{bucket}.{host}{encoded}")
        }
    }

    /// 发一次请求并读回完整响应。
    fn call(&self, request: S3Request<'_>) -> NetResult<S3Response> {
        let url = match request.bucket {
            Some(bucket) if !bucket.is_empty() => self.object_url(bucket, request.key),
            _ => self.origin().to_string(),
        };
        let mut headers = request.headers.to_vec();
        // 匿名访问（公共桶）时既不发 `Authorization`，也不发 `x-amz-content-sha256`
        // —— 有些兼容实现看到半套签名头会直接拒绝。
        if !self.config.access_key.is_empty() && !self.config.secret_key.is_empty() {
            // 签名用的 path 必须与 URL 里那一截**逐字一致**：`object_url` 已经
            // 编码过，这里只做「抠出 path」，绝不能再编一次（会变成 `%2520`）。
            let host = self.host();
            let path = url_path(&url, &host);
            let signed = sign::sign(&sign::SignInput {
                method: request.method,
                host: &host,
                path: &path,
                query: request.query,
                headers: &headers,
                payload: request.body,
                region: &self.config.region,
                credentials: sign::Credentials {
                    access_key: &self.config.access_key,
                    secret_key: &self.config.secret_key,
                },
            });
            headers.extend(signed);
        }

        let method = ureq::http::Method::from_bytes(request.method.as_bytes())
            .map_err(|error| NetError::with_detail(ErrorKind::Unsupported, error))?;
        let mut uri = url.clone();
        let query = sign::canonical_query(request.query);
        if !query.is_empty() {
            uri.push('?');
            uri.push_str(&query);
        }
        let mut builder = ureq::http::Request::builder().method(method).uri(&uri);
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body: Vec<u8> = request.body.to_vec();
        let built = builder
            .body(body)
            .map_err(|error| NetError::with_detail(ErrorKind::BadUrl, error))?;

        let started = Instant::now();
        let mut response = match self.agent.run(built) {
            Ok(response) => response,
            Err(error) => return Err(transport_error(error, &url)),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let status = response.status().as_u16();
        let bucket_region = response
            .headers()
            .get("x-amz-bucket-region")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let limit = request.limit;
        let mut raw = Vec::new();
        let over_limit = {
            use std::io::Read as _;
            let mut reader = response.body_mut().with_config().limit(limit).reader();
            match reader.read_to_end(&mut raw) {
                Ok(_) => false,
                // 读到一半断掉：已经收到的字节要留下（错误响应体常常这样）。
                Err(_) => !raw.is_empty() && raw.len() >= limit as usize,
            }
        };

        let text = String::from_utf8_lossy(&raw).into_owned();
        if !(200..300).contains(&status) {
            return Err(http_error(status, &text, bucket_region.as_deref()));
        }
        if over_limit {
            return Err(NetError::with_detail(
                ErrorKind::TooLarge,
                format!("{limit}B {url}"),
            ));
        }
        Ok(S3Response {
            status,
            headers: response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            body: raw,
            text,
            elapsed_ms,
        })
    }

    // ─────────────────────── 桶 ───────────────────────

    /// 列全部桶。
    ///
    /// 参考仓用 `Bucket::list_buckets(region, credentials)`；对应 `GET /`。
    /// 有些网关（只给了单桶权限的 AK）会返回 403 —— 那是权限问题，不是连不上。
    pub fn list_buckets(&self) -> NetResult<Vec<Bucket>> {
        let response = self.call(S3Request {
            method: "GET",
            bucket: None,
            key: "",
            query: &[],
            headers: &[],
            body: b"",
            limit: 8 * 1024 * 1024,
        })?;
        let buckets = xml::blocks(&response.text, "Bucket")
            .into_iter()
            .filter_map(|block| {
                let name = xml::text(block, "Name")?;
                Some(Bucket {
                    name,
                    creation_date: parse_timestamp(
                        &xml::text(block, "CreationDate").unwrap_or_default(),
                    ),
                })
            })
            .collect();
        Ok(buckets)
    }

    /// 建桶。`us-east-1` 不发 `LocationConstraint`（AWS 的规矩：默认区域带这个
    /// 字段反而报 `InvalidLocationConstraint`），其余区域必须带。
    pub fn create_bucket(&self, bucket: &str) -> NetResult<()> {
        require_name(bucket)?;
        let body = if self.config.region == "us-east-1" {
            String::new()
        } else {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                self.config.region
            )
        };
        let payload = body.into_bytes();
        self.call(S3Request {
            method: "PUT",
            bucket: Some(bucket),
            key: "",
            query: &[],
            headers: &[("content-type".to_string(), "application/xml".to_string())],
            body: &payload,
            limit: 1024 * 1024,
        })?;
        Ok(())
    }

    /// 删桶（必须是空桶，否则 S3 返回 `BucketNotEmpty`）。
    pub fn delete_bucket(&self, bucket: &str) -> NetResult<()> {
        require_name(bucket)?;
        self.call(S3Request {
            method: "DELETE",
            bucket: Some(bucket),
            key: "",
            query: &[],
            headers: &[],
            body: b"",
            limit: 1024 * 1024,
        })?;
        Ok(())
    }

    // ─────────────────────── 对象 ───────────────────────

    /// 列对象（`ListObjectsV2`）。
    ///
    /// `delimiter = "/"` 时返回「当前层的目录 + 文件」，与参考仓的浏览体验一致。
    pub fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        token: &str,
    ) -> NetResult<ListPage> {
        require_name(bucket)?;
        let mut query = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), PAGE_SIZE.to_string()),
        ];
        if !prefix.is_empty() {
            query.push(("prefix".to_string(), prefix.to_string()));
        }
        if !delimiter.is_empty() {
            query.push(("delimiter".to_string(), delimiter.to_string()));
        }
        if !token.is_empty() {
            query.push(("continuation-token".to_string(), token.to_string()));
        }
        let response = self.call(S3Request {
            method: "GET",
            bucket: Some(bucket),
            key: "",
            query: &query,
            headers: &[],
            body: b"",
            limit: 8 * 1024 * 1024,
        })?;

        let mut objects: Vec<Object> = xml::blocks(&response.text, "Contents")
            .into_iter()
            .filter_map(|block| {
                let key = xml::text(block, "Key")?;
                Some(Object {
                    key,
                    size: xml::text(block, "Size")
                        .and_then(|raw| raw.parse().ok())
                        .unwrap_or(0),
                    last_modified: parse_timestamp(
                        &xml::text(block, "LastModified").unwrap_or_default(),
                    ),
                    etag: xml::text(block, "ETag")
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string(),
                    storage_class: xml::text(block, "StorageClass").unwrap_or_default(),
                    is_prefix: false,
                })
            })
            .collect();
        for block in xml::blocks(&response.text, "CommonPrefixes") {
            if let Some(prefix) = xml::text(block, "Prefix") {
                objects.push(Object {
                    // 去掉末尾 `/`：界面上是「文件夹名」，进目录时再补回来。
                    key: prefix.trim_end_matches('/').to_string(),
                    size: 0,
                    last_modified: 0,
                    etag: String::new(),
                    storage_class: String::new(),
                    is_prefix: true,
                });
            }
        }
        let truncated = xml::text(&response.text, "IsTruncated")
            .map(|raw| raw.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let next_token =
            xml::text(&response.text, "NextContinuationToken").filter(|t| !t.is_empty());
        Ok(ListPage {
            objects,
            truncated,
            next_token,
        })
    }

    /// 上传一段内存数据。
    pub fn put_object(&self, bucket: &str, key: &str, body: &[u8]) -> NetResult<()> {
        require_name(bucket)?;
        require_key(key)?;
        if body.len() > MAX_TRANSFER {
            return Err(NetError::with_detail(
                ErrorKind::TooLarge,
                format!("{}B", body.len()),
            ));
        }
        self.call(S3Request {
            method: "PUT",
            bucket: Some(bucket),
            key,
            query: &[],
            headers: &[],
            body,
            limit: 1024 * 1024,
        })?;
        Ok(())
    }

    /// 上传本地文件，返回字节数。
    pub fn put_file(&self, bucket: &str, key: &str, path: &Path) -> NetResult<u64> {
        let body = read_limited(path)?;
        self.put_object(bucket, key, &body)?;
        Ok(body.len() as u64)
    }

    /// 建「目录」：S3 没有目录，就是一个以 `/` 结尾的空对象。
    pub fn create_folder(&self, bucket: &str, key: &str) -> NetResult<()> {
        require_name(bucket)?;
        let key = format!("{}/", key.trim_end_matches('/'));
        self.put_object(bucket, &key, b"")
    }

    /// 下载到内存。
    pub fn get_object(&self, bucket: &str, key: &str) -> NetResult<Vec<u8>> {
        require_name(bucket)?;
        require_key(key)?;
        let response = self.call(S3Request {
            method: "GET",
            bucket: Some(bucket),
            key,
            query: &[],
            headers: &[],
            body: b"",
            limit: MAX_TRANSFER as u64,
        })?;
        Ok(response.body)
    }

    /// 下载到本地文件，返回字节数。
    pub fn get_to_file(&self, bucket: &str, key: &str, path: &Path) -> NetResult<u64> {
        let body = self.get_object(bucket, key)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(NetError::io)?;
        }
        std::fs::write(path, &body).map_err(NetError::io)?;
        Ok(body.len() as u64)
    }

    /// 对象大小（`HEAD`）。
    pub fn head_object(&self, bucket: &str, key: &str) -> NetResult<u64> {
        let response = self.call(S3Request {
            method: "HEAD",
            bucket: Some(bucket),
            key,
            query: &[],
            headers: &[],
            body: b"",
            limit: 0,
        })?;
        Ok(response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0))
    }

    pub fn delete_object(&self, bucket: &str, key: &str) -> NetResult<()> {
        require_name(bucket)?;
        require_key(key)?;
        self.call(S3Request {
            method: "DELETE",
            bucket: Some(bucket),
            key,
            query: &[],
            headers: &[],
            body: b"",
            limit: 1024 * 1024,
        })?;
        Ok(())
    }

    /// 服务端复制（`x-amz-copy-source`，不下载再上传）。
    ///
    /// 参考仓的 `copy_object` 是「GET 下来再 PUT 上去」，几十 MB 的对象要过两遍
    /// 客户端带宽；`CopyObject` 一条请求在服务端完成，行为一致而代价低得多。
    pub fn copy_object(&self, bucket: &str, source: &str, dest: &str) -> NetResult<()> {
        require_name(bucket)?;
        require_key(source)?;
        require_key(dest)?;
        let header = format!(
            "/{bucket}/{}",
            sign::encode_path(source.trim_start_matches('/'))
        );
        let source_header = [("x-amz-copy-source".to_string(), header)];
        let response = self.call(S3Request {
            method: "PUT",
            bucket: Some(bucket),
            key: dest,
            query: &[],
            headers: &source_header,
            body: b"",
            limit: 1024 * 1024,
        })?;
        // CopyObject 的 200 也可能是失败：错误藏在响应体的 `<Error>` 里
        // （AWS 的历史包袱）。只看状态码会把「复制失败」报成成功。
        if let Some(code) = xml::text(&response.text, "Code") {
            let message = xml::text(&response.text, "Message").unwrap_or_default();
            return Err(NetError::with_detail(
                ErrorKind::Http,
                format!("200 {code}: {message}"),
            ));
        }
        Ok(())
    }

    /// 改名 = 复制 + 删源（参考仓 `s3_rename_object` 同语义）。
    pub fn rename_object(&self, bucket: &str, source: &str, dest: &str) -> NetResult<()> {
        self.copy_object(bucket, source, dest)?;
        self.delete_object(bucket, source)
    }

    /// 对象正文（按 UTF-8 解，解不出就 lossy）。参考仓 `get_object_content`。
    pub fn object_text(&self, bucket: &str, key: &str) -> NetResult<String> {
        let body = self.get_object(bucket, key)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// 一次请求的参数（内部用）。
struct S3Request<'a> {
    method: &'a str,
    /// `None` 或空串 = 桶级接口（`GET /`）。
    bucket: Option<&'a str>,
    key: &'a str,
    query: &'a [(String, String)],
    headers: &'a [(String, String)],
    body: &'a [u8],
    /// 响应体读取上限。
    limit: u64,
}

struct S3Response {
    #[allow(dead_code)]
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    text: String,
    #[allow(dead_code)]
    elapsed_ms: u64,
}

fn split_url(url: &str) -> (String, String) {
    match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url.to_string(), String::new()),
    }
}

/// 完整 URL → 签名用的 canonical URI（**已编码**、带前导 `/`、不含 query）。
///
/// 未编码的 path 只有一种来源：`object_url` 之外的调用点。为了不让「同一个 key
/// 在 URL 里编一次、在签名里又编一次」重演，这里的职责是「抠」，不是「编」。
fn url_path(url: &str, host: &str) -> String {
    let (without_query, _) = split_url(url);
    let rest = without_query
        .strip_prefix("https://")
        .or_else(|| without_query.strip_prefix("http://"))
        .unwrap_or(&without_query);
    let path = rest.strip_prefix(host).unwrap_or(rest);
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// key → URL 里的一段：逐段编码，保留 `/`。
fn encode_key(key: &str) -> String {
    sign::encode_path(key.trim_start_matches('/'))
}

fn require_name(bucket: &str) -> NetResult<()> {
    if bucket.trim().is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    Ok(())
}

fn require_key(key: &str) -> NetResult<()> {
    if key.trim().is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    Ok(())
}

fn read_limited(path: &Path) -> NetResult<Vec<u8>> {
    let meta = std::fs::metadata(path).map_err(NetError::io)?;
    if !meta.is_file() {
        return Err(NetError::with_detail(
            ErrorKind::PathMissing,
            path.display(),
        ));
    }
    if meta.len() as usize > MAX_TRANSFER {
        return Err(NetError::with_detail(
            ErrorKind::TooLarge,
            format!("{}B {}", meta.len(), path.display()),
        ));
    }
    std::fs::read(path).map_err(NetError::io)
}

/// ISO-8601 → Unix 秒（解析不出来是 0，与参考仓 `unwrap_or(0)` 一致）。
fn parse_timestamp(raw: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|stamp| stamp.timestamp())
        .unwrap_or(0)
}

/// 非 2xx → 稳定码 + 可读细节。
///
/// `403` 一律归 [`ErrorKind::Auth`]：S3 的 403 只有三种成因（AK 错、SK 错、
/// 桶策略不给），对用户而言都是「凭据/权限」，与「网络不通」必须分开报。
fn http_error(status: u16, body: &str, bucket_region: Option<&str>) -> NetError {
    let code = xml::text(body, "Code");
    let message = xml::text(body, "Message");
    let mut detail = status.to_string();
    if let Some(code) = &code {
        detail.push(' ');
        detail.push_str(code);
    }
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        detail.push_str(": ");
        detail.push_str(&message);
    }
    // 301 = endpoint 与桶的真实区域不一致。把正确区域直接写进细节，
    // 用户照着改 `region` 就行，不必再去查控制台。
    if status == 301 {
        if let Some(region) = bucket_region {
            detail.push_str(" (region: ");
            detail.push_str(region);
            detail.push(')');
        }
    }
    let kind = match status {
        401 | 403 => ErrorKind::Auth,
        _ => ErrorKind::Http,
    };
    NetError::with_detail(kind, detail)
}

fn transport_error(error: ureq::Error, url: &str) -> NetError {
    match error {
        ureq::Error::StatusCode(code) => {
            NetError::with_detail(ErrorKind::Http, format!("{code} {url}"))
        }
        ureq::Error::Timeout(_) => NetError::with_detail(ErrorKind::Timeout, url),
        ureq::Error::HostNotFound => NetError::with_detail(ErrorKind::Dns, url),
        ureq::Error::BadUri(detail) => NetError::with_detail(ErrorKind::BadUrl, detail),
        ureq::Error::Tls(detail) => NetError::with_detail(ErrorKind::Tls, detail),
        ureq::Error::ConnectionFailed => NetError::with_detail(ErrorKind::Connect, url),
        ureq::Error::Io(error) => match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                NetError::with_detail(ErrorKind::Timeout, url)
            }
            _ => NetError::with_detail(ErrorKind::Connect, error),
        },
        other => NetError::with_detail(ErrorKind::Connect, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// 假 S3 服务端记下的一条请求。
    struct Recorded {
        /// `PUT /demo/a.txt HTTP/1.1`
        request_line: String,
        /// 原始头。**不能整体小写化**：`Authorization` 的值（`AWS4-HMAC-SHA256`、
        /// 签名 hex 的 A-F）都是大小写敏感的，比对时只让「名字」不敏感。
        head: String,
        body: Vec<u8>,
    }

    impl Recorded {
        fn path_and_query(&self) -> String {
            self.request_line
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .to_string()
        }

        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_string())
            })
        }
    }

    /// 起一个「收一条请求 → 回一段固定响应」的假服务（每个测试各起一个）。
    fn fake_s3(response: String) -> (u16, std::thread::JoinHandle<Recorded>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                head.push_str(&line);
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let length: usize = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line.split(':').nth(1))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            if length > 0 {
                let _ = reader.read_exact(&mut body);
            }
            let mut writer = stream;
            let _ = writer.write_all(response.as_bytes());
            let _ = writer.flush();
            Recorded {
                request_line: head.lines().next().unwrap_or_default().to_string(),
                head,
                body,
            }
        });
        (port, handle)
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn client(port: u16) -> Client {
        Client::new(S3Config {
            endpoint: format!("http://127.0.0.1:{port}"),
            region: String::new(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            path_style: true,
            insecure: false,
            timeout_ms: 5_000,
        })
        .expect("client")
    }

    #[test]
    fn list_buckets_parses_names_and_dates() {
        let (port, server) = fake_s3(ok("<ListAllMyBucketsResult><Buckets>\
             <Bucket><Name>alpha</Name><CreationDate>2026-10-01T00:00:00.000Z</CreationDate></Bucket>\
             <Bucket><Name>beta</Name><CreationDate>2026-10-02T00:00:00.000Z</CreationDate></Bucket>\
             </Buckets></ListAllMyBucketsResult>"));
        let buckets = client(port).list_buckets().expect("列表");
        assert_eq!(
            buckets.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(buckets[0].creation_date, 1_790_812_800);
        let recorded = server.join().expect("server");
        // 桶级接口就是 `GET /`，签名照样带上。
        assert_eq!(recorded.path_and_query(), "/");
        assert!(
            recorded
                .header("authorization")
                .expect("必须签名")
                .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date")
        );
    }

    #[test]
    fn list_objects_builds_a_sorted_v2_query_and_parses_prefixes() {
        let (port, server) = fake_s3(ok("<ListBucketResult><IsTruncated>true</IsTruncated>\
             <NextContinuationToken>tok</NextContinuationToken>\
             <Contents><Key>a/1.txt</Key><Size>12</Size>\
             <LastModified>2026-10-01T12:00:00.000Z</LastModified>\
             <ETag>&quot;abc&quot;</ETag><StorageClass>STANDARD</StorageClass></Contents>\
             <CommonPrefixes><Prefix>a/sub/</Prefix></CommonPrefixes>\
             </ListBucketResult>"));
        let page = client(port)
            .list_objects("demo", "a/", "/", "")
            .expect("列举");
        // 文件在前、目录在后；目录去掉尾斜杠。
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "a/1.txt");
        assert_eq!(page.objects[0].size, 12);
        assert_eq!(page.objects[0].etag, "abc");
        assert!(!page.objects[0].is_prefix);
        assert_eq!(page.objects[1].key, "a/sub");
        assert!(page.objects[1].is_prefix);
        assert!(page.truncated);
        assert_eq!(page.next_token.as_deref(), Some("tok"));

        let recorded = server.join().expect("server");
        // query 必须按**编码后**的 key 排序（delimiter < list-type < max-keys < prefix）。
        assert_eq!(
            recorded.path_and_query(),
            "/demo?delimiter=%2F&list-type=2&max-keys=1000&prefix=a%2F"
        );
    }

    #[test]
    fn anonymous_client_sends_no_signature_headers() {
        let (port, server) = fake_s3(ok("<ListBucketResult></ListBucketResult>"));
        let anon = Client::new(S3Config {
            endpoint: format!("http://127.0.0.1:{port}"),
            access_key: String::new(),
            secret_key: String::new(),
            ..S3Config::default()
        })
        .expect("client");
        anon.list_objects("demo", "", "/", "").expect("列举");
        let recorded = server.join().expect("server");
        // 半套签名头（只有 x-amz-date 没有 Authorization）会被兼容实现直接拒绝。
        assert!(recorded.header("authorization").is_none());
        assert!(recorded.header("x-amz-date").is_none());
        assert!(recorded.header("x-amz-content-sha256").is_none());
    }

    #[test]
    fn virtual_host_style_puts_the_bucket_in_the_host() {
        let client = Client::new(S3Config {
            endpoint: "https://s3.example.com".to_string(),
            path_style: false,
            ..S3Config::default()
        })
        .expect("client");
        assert_eq!(
            client.object_url("demo", "a/1.txt"),
            "https://demo.s3.example.com/a/1.txt"
        );
    }

    #[test]
    fn keys_are_encoded_once_in_the_url() {
        let client = Client::new(S3Config {
            endpoint: "http://127.0.0.1:9000/".to_string(),
            ..S3Config::default()
        })
        .expect("client");
        // 尾部 `/` 被吃掉，key 逐段编码，`/` 保留。
        assert_eq!(
            client.object_url("demo", "目录/a b.txt"),
            "http://127.0.0.1:9000/demo/%E7%9B%AE%E5%BD%95/a%20b.txt"
        );
    }

    #[test]
    fn forbidden_maps_to_auth_with_the_s3_error_code() {
        let (port, server) = fake_s3(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/xml\r\nContent-Length: 95\r\nConnection: close\r\n\r\n\
             <Error><Code>AccessDenied</Code><Message>Access Denied</Message><RequestId>X</RequestId></Error>"
                .to_string(),
        );
        let error = client(port).list_buckets().expect_err("403 必须失败");
        assert_eq!(error.kind(), ErrorKind::Auth);
        let detail = error.detail().unwrap_or_default();
        assert!(detail.contains("403"), "{detail}");
        assert!(detail.contains("AccessDenied"), "{detail}");
        server.join().expect("server");
    }

    #[test]
    fn missing_bucket_is_http_not_auth() {
        let body = "<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>";
        let (port, _server) = fake_s3(format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        let error = client(port)
            .list_objects("nope", "", "/", "")
            .expect_err("404");
        assert_eq!(error.kind(), ErrorKind::Http);
        assert_eq!(
            error.detail().unwrap_or_default(),
            "404 NoSuchBucket: The specified bucket does not exist"
        );
    }

    #[test]
    fn copy_object_encodes_the_source_header_and_catches_error_in_a_200() {
        let (port, server) = fake_s3(ok(
            "<Error><Code>InvalidRequest</Code><Message>source and destination are the same</Message></Error>",
        ));
        let error = client(port)
            .copy_object("demo", "目录/源 文件.txt", "dst.txt")
            .expect_err("200 里的 Error 也必须算失败");
        assert!(
            error
                .detail()
                .unwrap_or_default()
                .contains("InvalidRequest")
        );
        let recorded = server.join().expect("server");
        assert_eq!(recorded.path_and_query(), "/demo/dst.txt");
        // header 值里不能有空格/中文：已 percent-encode。
        assert_eq!(
            recorded.header("x-amz-copy-source").as_deref(),
            Some("/demo/%E7%9B%AE%E5%BD%95/%E6%BA%90%20%E6%96%87%E4%BB%B6.txt")
        );
        // 复制头必须参与签名（否则 S3 会以 SignatureDoesNotMatch 拒绝）。
        assert!(
            recorded
                .header("authorization")
                .expect("签名")
                .contains("x-amz-copy-source")
        );
    }

    #[test]
    fn put_and_get_round_trip_through_files() {
        let dir = std::env::temp_dir().join(format!("duck-s3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("目录");
        let local = dir.join("a.txt");
        std::fs::write(&local, "hello").expect("写");

        let (port, server) = fake_s3(ok(""));
        let uploaded = client(port)
            .put_file("demo", "a.txt", &local)
            .expect("上传");
        assert_eq!(uploaded, 5);
        let recorded = server.join().expect("server");
        assert_eq!(recorded.path_and_query(), "/demo/a.txt");
        assert_eq!(recorded.body, b"hello");
        // body 参与签名：payload hash 不能是空串的常量。
        assert_ne!(
            recorded.header("x-amz-content-sha256").unwrap_or_default(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let (port, server) = fake_s3(ok("hello"));
        let target = dir.join("out/b.txt");
        let downloaded = client(port)
            .get_to_file("demo", "a.txt", &target)
            .expect("下载");
        assert_eq!(downloaded, 5);
        // 目标目录不存在时要自己建出来。
        assert_eq!(std::fs::read_to_string(&target).expect("读"), "hello");
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_bucket_name_is_rejected_before_any_request() {
        // 不该悄悄发出一个 `GET /`（那会列出所有桶，语义完全不同）。
        let client = Client::new(S3Config {
            endpoint: "http://127.0.0.1:1".to_string(),
            ..S3Config::default()
        })
        .expect("client");
        assert_eq!(
            client.list_objects("  ", "", "/", "").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            client.put_object("demo", "", b"x").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
    }

    #[test]
    fn endpoint_without_scheme_is_a_bad_url() {
        let error = Client::new(S3Config {
            endpoint: "minio.lan:9000".to_string(),
            ..S3Config::default()
        })
        .err()
        .expect("必须拒绝");
        assert_eq!(error.kind(), ErrorKind::BadUrl);
        assert_eq!(
            Client::new(S3Config::default())
                .err()
                .expect("必须拒绝")
                .kind(),
            ErrorKind::EmptyInput
        );
    }
}
