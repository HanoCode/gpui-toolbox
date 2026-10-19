//! HTTP 请求：方法 / URL / 查询参数 / 头 / 五种请求体 / 四种认证 / 变量插值 /
//! 超时 / 跳转 / 证书校验 / cURL 导入。
//!
//! 移植参考仓 `HttpRequest.vue`（1,518 行）里**单请求**那一段。源页面把请求放在
//! 一个 Postman 式的工作台里（左栏集合与历史、顶部多标签、环境变量管理器、
//! 集合导入导出）——那套东西要持久化多份请求，本仓执行页是「跑一次、看结果」，
//! 所以**多请求容器不迁**：集合 / 文件夹 / 多标签 / 环境管理器 / 集合导入导出。
//!
//! 页面上其余「按钮级」能力逐条落在了参数上，对照见下表（`→` 是本仓的形态）：
//!
//! | 参考页 | 本仓 |
//! |---|---|
//! | Params 标签（键值表 + 启用勾选） | `query` 参数：每行 `键=值`，注释行跳过 |
//! | 变量插值 `{{var}}` + 环境管理器 | `vars` 参数：每行 `键=值`，未定义即报错 |
//! | Body：none / raw / form-data / x-www-form-urlencoded / binary | `body_type` 五档同义；raw 的 JSON/XML/Text/HTML 由 `body_format` 给默认 Content-Type |
//! | form-data 逐行「是文件」勾选 | `form` 里写 `字段=@路径`（本仓输入是文本框，用约定代替勾选框） |
//! | Authorization：bearer / basic / API Key（header 或 query） | 同名四档 + `key_in`（header / query） |
//! | 常用头预设、UA 预设 | 不迁：那是输入辅助，写进 `headers` 即可（值都在参考页常量里，见台账） |
//! | Import cURL | `source = curl` + `curl` 参数：解析后**直接发**，报告回显解析结果 |
//! | Response：Body / Headers / Cookies 三个子标签 | 报告按段展开（含 `set-cookie` 逐条解析） |
//! | 响应时间、体积、换行开关 | 报告给耗时与体积；换行是渲染器的事 |
//! | Copy as cURL | 报告尾部固定给等价 curl（密钥用 `$TOKEN` 占位） |
//! | 保存请求 / 历史 / 集合 | 只保留最近 20 条**方法·地址·状态·耗时**（不含头体凭据） |
//!
//! ## 与源页面的三处差异（都是本仓约束导致的）
//!
//! 1. **凭据不落盘**：历史只记方法、地址、状态码、耗时；`Authorization` 之类的值
//!    只活在这一次请求的内存里。
//! 2. **凭据不进日志**：`redact_headers` 把 `Authorization` / `X-Api-Key` 之类的值
//!    打码成 `***`，报告里给的是打码后的头。
//! 3. **不静默跟随无限跳转**：跳转次数有上限（缺省 5），超了报错而不是给出
//!    「一个看起来成功的页面」。
//!
//! 传输用 `ureq`（阻塞、纯 Rust TLS）。响应体读到 8 MiB 就停，`truncated` 标记
//! 让报告能说清「你自己去取完整内容」，而不是悄悄截断。

use std::time::Duration;

use base64::Engine as _;

use crate::error::{ErrorKind, NetError, NetResult};

/// 响应体读取上限（超出即标记 `truncated`，不再继续读）。
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
/// 未填超时时的缺省值（毫秒）。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 缺省最大跳转次数。
pub const DEFAULT_REDIRECTS: u32 = 5;

/// HTTP 方法（只收设计稿里那五个 + HEAD / OPTIONS）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl Method {
    /// 从参数值解析；大写小写都收（用户手打 `get` 不该报错）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "PATCH" => Some(Self::Patch),
            "DELETE" => Some(Self::Delete),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    /// 这个方法有没有请求体（GET / HEAD / DELETE / OPTIONS 在 `ureq` 里没有 body）。
    pub fn has_body(self) -> bool {
        matches!(self, Self::Post | Self::Put | Self::Patch)
    }
}

/// 认证方式。值**只存在于本次运行**，不落盘、不进日志。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auth {
    #[default]
    None,
    /// `Authorization: Bearer <token>`
    Bearer(String),
    /// `Authorization: Basic base64(user:pass)`
    Basic { user: String, password: String },
    /// 自定义头（`X-Api-Key` 之类）
    ApiKey { header: String, value: String },
}

impl Auth {
    /// 要从参数里读的那几个值是不是都给了（缺了就是 `EmptyInput`，不是「静默发个空头」）。
    pub fn check(&self) -> NetResult<()> {
        match self {
            Self::None => Ok(()),
            Self::Bearer(token) if token.trim().is_empty() => {
                Err(NetError::new(ErrorKind::EmptyInput))
            }
            Self::Basic { user, .. } if user.trim().is_empty() => {
                Err(NetError::new(ErrorKind::EmptyInput))
            }
            Self::ApiKey { header, .. } if header.trim().is_empty() => {
                Err(NetError::new(ErrorKind::EmptyInput))
            }
            _ => Ok(()),
        }
    }

    /// 认证对应的请求头。
    pub fn headers(&self) -> Vec<(String, String)> {
        match self {
            Self::None => Vec::new(),
            Self::Bearer(token) => vec![("Authorization".to_string(), format!("Bearer {token}"))],
            Self::Basic { user, password } => {
                let raw = format!("{user}:{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                vec![("Authorization".to_string(), format!("Basic {encoded}"))]
            }
            Self::ApiKey { header, value } => vec![(header.clone(), value.clone())],
        }
    }

    /// 给人看的一行描述（**不含密文**）。
    pub fn describe(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer(_) => "bearer",
            Self::Basic { .. } => "basic",
            Self::ApiKey { .. } => "api_key",
        }
    }
}

/// 请求体形态。与参考页 `bodyTypes` 五档一一对应（`none` 也在内：
/// 「不发体」是一个明确的选择，不是「忘了填」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyKind {
    #[default]
    None,
    /// 原样发送输入框里的文本（JSON / XML / 纯文本）。
    Raw,
    /// `application/x-www-form-urlencoded`：键值表逐项百分号编码。
    UrlEncoded,
    /// `multipart/form-data`：键值表，`字段=@路径` 的那一行读文件当附件。
    FormData,
    /// 整个文件当请求体。
    Binary,
}

impl BodyKind {
    /// 解析参数值；认别名（`x-www-form-urlencoded` / `multipart` 都收）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Some(Self::None),
            "raw" => Some(Self::Raw),
            "urlencoded" | "x-www-form-urlencoded" => Some(Self::UrlEncoded),
            "form-data" | "formdata" | "multipart" => Some(Self::FormData),
            "binary" => Some(Self::Binary),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Raw => "raw",
            Self::UrlEncoded => "x-www-form-urlencoded",
            Self::FormData => "form-data",
            Self::Binary => "binary",
        }
    }

    /// 这个形态的体要不要先读文件（`allow_files` 与报告都要知道）。
    pub fn touches_files(self) -> bool {
        matches!(self, Self::FormData | Self::Binary)
    }
}

/// 一次请求的完整描述。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// 请求体**字节**：raw / urlencoded 是文本的 UTF-8，multipart 是拼好的分片，
    /// binary 是文件原文。用 `Vec<u8>` 而不是 `String`：二进制体不许被
    /// `from_utf8_lossy` 悄悄改字节。
    pub body: Vec<u8>,
    /// 体的形态（只影响报告与 curl 渲染，不影响发送）。
    pub body_kind: BodyKind,
    pub auth: Auth,
    pub timeout_ms: u64,
    pub max_redirects: u32,
    /// 跳过证书校验（银行内网自签证书用；缺省 **校验**）。
    pub insecure: bool,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            method: Method::Get,
            url: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            body_kind: BodyKind::None,
            auth: Auth::None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_redirects: DEFAULT_REDIRECTS,
            insecure: false,
        }
    }
}

/// 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub elapsed_ms: u64,
    pub headers: Vec<(String, String)>,
    /// 响应体（已按 Content-Type 格式化过：JSON 会缩进）。
    pub body: String,
    /// 原始字节数（格式化前的）。
    pub body_bytes: usize,
    /// 超过 [`MAX_BODY`]，只读到了前一段。
    pub truncated: bool,
}

impl HttpResponse {
    /// 状态码是否 2xx / 3xx（4xx/5xx 在报告里要标红）。
    pub fn is_success(&self) -> bool {
        (200..400).contains(&self.status)
    }

    /// 取某个响应头（大小写不敏感，多值取第一个）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 把「一行一个 `Name: value`」的文本解析成头列表。
///
/// 空行与 `#` 开头的行跳过（用户常把注释留在框里）；缺冒号报 [`ErrorKind::BadHeader`]
/// 并带上行号——静默丢弃一个头比报错更难查。
pub fn parse_headers(raw: &str) -> NetResult<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(NetError::with_detail(ErrorKind::BadHeader, index + 1));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(NetError::with_detail(ErrorKind::BadHeader, index + 1));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

/// 打码后的头列表（`Authorization` / 含 `key` / `token` / `secret` 的名字都盖掉）。
///
/// 报告与日志只许用这一份：源页面曾把完整 Authorization 写进历史，这里不给机会。
pub fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let secret = lower == "authorization"
                || lower == "cookie"
                || lower.contains("token")
                || lower.contains("secret")
                || lower.contains("api-key")
                || lower.contains("apikey")
                || lower.contains("password");
            if secret && !value.is_empty() {
                (name.clone(), "***".to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

// ── 键值行 / 变量 / 查询串 / 表单编码 ─────────────────────────────────

/// 解析「每行一个 `键=值`」的文本（`query` / `vars` / `form` 三处共用）。
///
/// 空行与 `#` 开头的行跳过（用户常把注释留在框里）；**键名两端去空白，值只去掉
/// 首尾空白**——值中间的空格是内容（`message=修复 登录`）。缺 `=` 的行报
/// [`ErrorKind::BadPair`] 并带行号：静默跳过会让「为什么少了一个参数」变成悬案。
pub fn parse_pairs(raw: &str) -> NetResult<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(NetError::with_detail(ErrorKind::BadPair, index + 1));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(NetError::with_detail(ErrorKind::BadPair, index + 1));
        }
        pairs.push((key.to_string(), value.trim().to_string()));
    }
    Ok(pairs)
}

/// 把 `{{名字}}` 替换成 `vars` 里的值（参考页 `resolveVariables`）。
///
/// **未定义就是错误**：参考页留空替换成空串，结果是请求静默打到
/// `https:///api`——报错比「发出去再看 404」省一轮排查。`{{` 没有配对的 `}}`
/// 或中间是空名时原样保留（JSON 里真有可能出现这两个字符）。
pub fn resolve_vars(raw: &str, vars: &[(String, String)]) -> NetResult<String> {
    if !raw.contains("{{") {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        let name = &rest[start + 2..start + 2 + end];
        let after = start + 2 + end + 2;
        if name.trim().is_empty() {
            // 空名不算占位符：整段原样搬过去，再从后面继续找。
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            continue;
        }
        let key = name.trim();
        let Some((_, value)) = vars.iter().find(|(name, _)| name == key) else {
            return Err(NetError::with_detail(ErrorKind::Variable, key));
        };
        out.push_str(&rest[..start]);
        out.push_str(value);
        rest = &rest[after..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 百分号编码（RFC 3986 非保留字符之外全部转义）。
///
/// 不引 `url` / `form_urlencoded`：`ureq` 只负责发，查询串的编码口径要自己定
/// ——空格编成 `%20` 而不是 `+`（`+` 在路径与查询里语义不同，服务器处理不一）。
pub fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 把键值对拼到 URL 上（跳过空键）。
///
/// `#fragment` 必须留在**最后**：直接 `push_str("&k=v")` 会把参数塞进片段里，
/// 服务端收不到——参考页用 `URLSearchParams` 拼接，同样要处理这一点。
pub fn append_query(url: &str, pairs: &[(String, String)]) -> String {
    let usable: Vec<&(String, String)> = pairs.iter().filter(|(key, _)| !key.is_empty()).collect();
    if usable.is_empty() {
        return url.to_string();
    }
    let (base, fragment) = match url.find('#') {
        Some(index) => (&url[..index], &url[index..]),
        None => (url, ""),
    };
    let mut out = base.to_string();
    let mut first = !out.contains('?');
    if out.ends_with('?') || out.ends_with('&') {
        first = true;
    }
    for (key, value) in usable {
        out.push(if first { '?' } else { '&' });
        first = false;
        out.push_str(&encode_component(key));
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out.push_str(fragment);
    out
}

/// `application/x-www-form-urlencoded` 的请求体（空键跳过）。
pub fn encode_urlencoded(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 拼好的 multipart 体。
#[derive(Debug)]
pub struct Multipart {
    pub boundary: String,
    pub body: Vec<u8>,
}

impl Multipart {
    /// `Content-Type` 头的值（带边界）。
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }
}

/// 生成一个不会与内容撞车的边界。
///
/// 参考页由 `FormData` 自动生成；本仓手拼，用「时间戳纳秒 + 调用序号」——
/// 不引 `rand`（只为 16 字节随机加一个依赖不划算），可读性还更好排查。
fn make_boundary() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "----DuckToolbox{:x}{:04x}",
        nanos,
        SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

/// 按扩展名猜 `Content-Type`（只覆盖联调常见的十来种）。
///
/// 不引 `mime_guess`（它带一张几百 KB 的表）：猜错的代价只是「服务端按二进制
/// 收」，用户可以在 `headers` 里显式覆盖。
pub fn guess_content_type(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" | "log" | "csv" => "text/plain",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "zip" => "application/zip",
        "jar" | "class" => "application/java-archive",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// 拼 multipart 体：值为 `@路径` 的字段读成文件附件，其余当普通文本字段。
///
/// 只做 `\r\n` 换行（RFC 7578 要求），不做 `Content-Transfer-Encoding`
/// ——那是 1990 年代的邮件写法，现代服务端都不认。
pub fn encode_multipart(pairs: &[(String, String)]) -> NetResult<Multipart> {
    let boundary = make_boundary();
    let mut body: Vec<u8> = Vec::new();
    for (key, value) in pairs {
        if key.is_empty() {
            continue;
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match value.strip_prefix('@') {
            Some(path) if !path.trim().is_empty() => {
                let path = crate::error::require_file(path.trim())?;
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
                    .replace(['"', '\r', '\n'], "_");
                let bytes = std::fs::read(&path).map_err(NetError::io)?;
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\n",
                        quote_field(key)
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(
                    format!(
                        "Content-Type: {}\r\n\r\n",
                        guess_content_type(&path.to_string_lossy())
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(&bytes);
                body.extend_from_slice(b"\r\n");
            }
            _ => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                        quote_field(key)
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(value.as_bytes());
                body.extend_from_slice(b"\r\n");
            }
        }
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(Multipart { boundary, body })
}

/// 字段名进 `Content-Disposition` 前把危险字符换掉（防头注入）。
fn quote_field(name: &str) -> String {
    name.replace(['"', '\r', '\n'], "_")
}

/// 字节数 → 人能读的大小（1024 进制，与文件管理器一致）。
///
/// 收 `u64`：HTTP 响应体是 `usize`，S3 对象是 `u64`，统一到宽的那一头，
/// 免得每个调用点各转一次。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// URL 合法性检查：只收 http/https，且必须有主机。
///
/// 不引 `url` crate：`ureq` 自己会解析，这里只是提前给出**能翻译**的错误码，
/// 而不是让传输层抛一句英文 io 错误。
pub fn validate_url(raw: &str) -> NetResult<()> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("HTTP://"))
        .or_else(|| url.strip_prefix("HTTPS://"));
    let Some(rest) = rest else {
        return Err(NetError::with_detail(ErrorKind::BadUrl, url));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Err(NetError::with_detail(ErrorKind::BadUrl, url));
    }
    Ok(())
}

/// 发一次请求。
pub fn send(request: &HttpRequest) -> NetResult<HttpResponse> {
    validate_url(&request.url)?;
    request.auth.check()?;

    let mut config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(request.timeout_ms.max(1_000))))
        .max_redirects(request.max_redirects)
        .http_status_as_error(false)
        .user_agent("DuckToolbox/0.2");
    if request.insecure {
        config = config.tls_config(
            ureq::tls::TlsConfig::builder()
                .disable_verification(true)
                .build(),
        );
    }
    let agent: ureq::Agent = config.build().into();

    // 用一个显式构造的 `http::Request` 而不是 `agent.get()/post()` 家族：
    // 后者的返回类型带 `WithoutBody` / `WithBody` 型别参数，`match` 各臂类型不同，
    // 拼不进一个变量（编译器会要求两条完全分开的分支）。`Agent::run` 收
    // `Request<impl AsSendBody>`，正好用「有体 / 无体」两个分支统一收口。
    let method = ureq::http::Method::from_bytes(request.method.as_str().as_bytes())
        .map_err(|error| NetError::with_detail(ErrorKind::Unsupported, error))?;
    let mut builder = ureq::http::Request::builder()
        .method(method)
        .uri(request.url.as_str());
    // 认证头先挂，用户手写的同名头后挂 —— 后者覆盖前者（用户显式写的说了算）。
    for (name, value) in request
        .auth
        .headers()
        .into_iter()
        .chain(request.headers.iter().cloned())
    {
        builder = builder.header(name, value);
    }

    let started = std::time::Instant::now();
    let response = if request.method.has_body() {
        let built = builder
            .body(request.body.as_slice())
            .map_err(|error| NetError::with_detail(ErrorKind::BadUrl, error))?;
        agent.run(built)
    } else {
        let built = builder
            .body(())
            .map_err(|error| NetError::with_detail(ErrorKind::BadUrl, error))?;
        agent.run(built)
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => return Err(transport_error(error, &request.url)),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let status = response.status().as_u16();
    let reason = response
        .status()
        .canonical_reason()
        .unwrap_or_default()
        .to_string();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();

    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    let mut response = response;
    // 用 `read_to_end` 而不是 `read_to_vec`：后者在「对端提前断开 / 超过上限」时
    // 只回一个 Err，**已经读到的字节一起丢掉**，报告里就成了「0 字节，已截断」——
    // 而真实场景（服务端 Content-Length 比实际正文大、边读边断开）恰恰是
    // 「收到了一部分」。`read_to_end` 出错时把已读部分留在 `raw` 里。
    let mut raw = Vec::new();
    let truncated = {
        use std::io::Read as _;
        let mut reader = response.body_mut().with_config().limit(MAX_BODY).reader();
        reader.read_to_end(&mut raw).is_err()
    };
    let body_bytes = raw.len();
    let text = String::from_utf8_lossy(&raw).into_owned();

    Ok(HttpResponse {
        status,
        reason,
        elapsed_ms,
        headers,
        body: pretty_body(&text, &content_type),
        body_bytes,
        truncated,
    })
}

/// 传输层错误 → 稳定码。
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

/// 按 Content-Type 格式化响应体：JSON 缩进，其余原样。
///
/// 「格式化」只做**能确定安全**的那一种：`serde_json` 解析失败就原样返回
///（很多人拿这个工具发 XML / 纯文本 / 二进制，硬格式化只会破坏内容）。
pub fn pretty_body(body: &str, content_type: &str) -> String {
    let looks_json = content_type.to_ascii_lowercase().contains("json")
        || (content_type.is_empty()
            && matches!(body.trim_start().chars().next(), Some('{') | Some('[')));
    if !looks_json {
        return body.to_string();
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string()),
        Err(_) => body.to_string(),
    }
}

/// 等价 curl 命令（报告的「复制」用：拿着就能在别处重放）。
///
/// 认证头**写成占位符**，绝不把 token 打进可复制的文本里。
///
/// 体分两种给法：文本（raw / urlencoded）内联 `--data-raw`；multipart 与二进制
/// 体**不内联**（分片边界、非 UTF-8 字节都会在终端里失真），改成一行注释说明
/// 怎么用文件重放——给一条能跑的假命令比给一条跑不通的真命令好。
pub fn curl_command(request: &HttpRequest) -> String {
    let mut out = format!("curl -X {}", request.method.as_str());
    for (name, value) in &request.headers {
        out.push_str(&format!(
            " \\\n  -H '{}: {}'",
            shell_escape(name),
            shell_escape(value)
        ));
    }
    match &request.auth {
        Auth::None => {}
        Auth::Bearer(_) => out.push_str(" \\\n  -H 'Authorization: Bearer $TOKEN'"),
        Auth::Basic { user, .. } => {
            out.push_str(&format!(" \\\n  -u '{}:$PASSWORD'", shell_escape(user)))
        }
        Auth::ApiKey { header, .. } => {
            out.push_str(&format!(" \\\n  -H '{}: $API_KEY'", shell_escape(header)))
        }
    }
    if request.method.has_body() && !request.body.is_empty() {
        match request.body_kind {
            BodyKind::Binary | BodyKind::FormData => {
                out.insert_str(
                    0,
                    &format!(
                        "# {} 体（{} 字节）无法内联，重放时用 --data-binary @<文件>\n",
                        request.body_kind.as_str(),
                        request.body.len()
                    ),
                );
                out.push_str(" \\\n  --data-binary @<文件>");
            }
            _ => out.push_str(&format!(
                " \\\n  --data-raw '{}'",
                shell_escape(&String::from_utf8_lossy(&request.body))
            )),
        }
    }
    out.push_str(&format!(" \\\n  '{}'", shell_escape(&request.url)));
    out
}

/// 单引号包裹 + 内部单引号转义（POSIX shell）。
fn shell_escape(value: &str) -> String {
    value.replace('\'', r"'\''")
}

// ── cURL 导入 ─────────────────────────────────────────────────────────

/// 从一条 curl 命令里拆出来的请求（参考页 `parseCurl` 的返回形状）。
///
/// `form` 非空表示命令用了 `-F`（multipart 表单），此时 `body` 一定是空的，
/// 调用方按 form-data 处理；`-F` 是参考页**没认**的参数（它只看 `-d`），
/// 但浏览器「Copy as cURL」很少用 `-F`，这条补齐不影响既有口径。
#[derive(Debug, Clone, Default)]
pub struct CurlRequest {
    pub method: Option<Method>,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub form: Vec<(String, String)>,
    pub auth: Auth,
    pub insecure: bool,
}

/// 解析一条 curl 命令。
///
/// 认的参数：`-X/--request`、`-H/--header`、`-d/--data*`（多条按 curl 的规矩用
/// `&` 连起来）、`-F/--form`、`-u/--user`、`-b/--cookie`、`-A/--user-agent`、
/// `-e/--referer`、`-I/--head`、`--url`、`-k/--insecure`。**不认**的短选项
/// （`-o`、`-v`、`--compressed`…）直接跳过：它们不影响「发什么」。
///
/// 与参考页的一处差异：`-d` 出现时参考页把 GET 改成 POST，这里同样处理，但
/// `-G` 明确要求「参数拼进 URL」时保持 GET（否则会把查询串发成请求体）。
pub fn parse_curl(raw: &str) -> NetResult<CurlRequest> {
    let tokens = tokenize_curl(raw);
    if tokens.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }

    let mut parsed = CurlRequest::default();
    let mut method_is_set = false;
    let mut use_get = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        let next = |index: usize| -> String { tokens.get(index + 1).cloned().unwrap_or_default() };
        match token {
            "-X" | "--request" => {
                let value = next(index);
                if !value.is_empty() {
                    parsed.method = Method::parse(&value);
                    method_is_set = true;
                }
                index += 2;
            }
            "-H" | "--header" => {
                let value = next(index);
                if let Some((name, header_value)) = value.split_once(':') {
                    let name = name.trim().to_string();
                    let header_value = header_value.trim().to_string();
                    if name.eq_ignore_ascii_case("authorization") {
                        parsed.auth = auth_from_header(&header_value);
                        if matches!(parsed.auth, Auth::None) {
                            parsed.headers.push((name, header_value));
                        }
                    } else {
                        parsed.headers.push((name, header_value));
                    }
                }
                index += 2;
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii"
            | "--data-urlencode" => {
                let value = next(index);
                if !parsed.body.is_empty() {
                    parsed.body.push('&');
                }
                parsed.body.push_str(&value);
                index += 2;
            }
            "-F" | "--form" => {
                let value = next(index);
                if let Some((key, field)) = value.split_once('=') {
                    parsed
                        .form
                        .push((key.trim().to_string(), field.trim().to_string()));
                }
                index += 2;
            }
            "-u" | "--user" => {
                let value = next(index);
                if let Some((user, password)) = value.split_once(':') {
                    parsed.auth = Auth::Basic {
                        user: user.to_string(),
                        password: password.to_string(),
                    };
                }
                index += 2;
            }
            "-b" | "--cookie" => {
                parsed.headers.push(("Cookie".to_string(), next(index)));
                index += 2;
            }
            "-A" | "--user-agent" => {
                parsed.headers.push(("User-Agent".to_string(), next(index)));
                index += 2;
            }
            "-e" | "--referer" => {
                parsed.headers.push(("Referer".to_string(), next(index)));
                index += 2;
            }
            "-I" | "--head" => {
                parsed.method = Some(Method::Head);
                method_is_set = true;
                index += 1;
            }
            "-G" | "--get" => {
                use_get = true;
                index += 1;
            }
            "-k" | "--insecure" => {
                parsed.insecure = true;
                index += 1;
            }
            "--url" => {
                parsed.url = next(index);
                index += 2;
            }
            _ => {
                if !token.starts_with('-') && token != "curl" && parsed.url.is_empty() {
                    parsed.url = token.to_string();
                }
                index += 1;
            }
        }
    }

    if parsed.url.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    // 没有 scheme 就按 http 补（curl 自己也这么干）。
    if !parsed.url.starts_with("http://") && !parsed.url.starts_with("https://") {
        parsed.url = format!("http://{}", parsed.url.trim_start_matches('/'));
    }
    if use_get {
        // `-G` + `-d`：把体搬到查询串（curl 的语义），体清空。
        if !parsed.body.is_empty() {
            let pairs = parse_pairs(&parsed.body.replace('&', "\n")).unwrap_or_default();
            parsed.url = append_query(&parsed.url, &pairs);
            parsed.body.clear();
        }
        if !method_is_set {
            parsed.method = Some(Method::Get);
        }
    } else if parsed.method.is_none() && (!parsed.body.is_empty() || !parsed.form.is_empty()) {
        parsed.method = Some(Method::Post);
    }
    Ok(parsed)
}

/// `Authorization: <scheme> <credential>` → [`Auth`]；认不出就留 `None`
/// （调用方会原样保留这个头，别把凭据弄丢）。
fn auth_from_header(value: &str) -> Auth {
    let lower = value.to_ascii_lowercase();
    if let Some(token) = lower.strip_prefix("bearer ") {
        return Auth::Bearer(value[value.len() - token.len()..].trim().to_string());
    }
    if lower.starts_with("basic ") {
        let encoded = value[6..].trim();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        if let Some(decoded) = decoded {
            if let Some((user, password)) = decoded.split_once(':') {
                return Auth::Basic {
                    user: user.to_string(),
                    password: password.to_string(),
                };
            }
        }
    }
    Auth::None
}

/// 按 shell 规矩切词（参考页 `tokenizeCurl` 的移植 + `$'...'` 支持）。
///
/// 处理四种写法：单引号（原样）、双引号（`\"` 与 `\\` 解转义）、`$'...'`
/// （bash 的 ANSI-C 引用，Chrome 的「Copy as cURL」会给中文/换行用这个）、
/// 裸词（到空白为止）。续行符 `\` + 换行先换成空格——否则 URL 后面的 `\`
/// 会被当成普通字符拼进地址。
fn tokenize_curl(command: &str) -> Vec<String> {
    let source = command
        .replace("\\\r\n", " ")
        .replace("\\\n", " ")
        .replace('\r', " ");
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        if chars[index] == '\'' {
            index += 1;
            let mut value = String::new();
            while index < chars.len() && chars[index] != '\'' {
                value.push(chars[index]);
                index += 1;
            }
            tokens.push(value);
            index += 1;
            continue;
        }
        if chars[index] == '$' && chars.get(index + 1) == Some(&'\'') {
            index += 2;
            let mut value = String::new();
            while index < chars.len() && chars[index] != '\'' {
                if chars[index] == '\\' && index + 1 < chars.len() {
                    let escaped = chars[index + 1];
                    value.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    index += 2;
                    continue;
                }
                value.push(chars[index]);
                index += 1;
            }
            tokens.push(value);
            index += 1;
            continue;
        }
        if chars[index] == '"' {
            index += 1;
            let mut value = String::new();
            while index < chars.len() && chars[index] != '"' {
                if chars[index] == '\\' && index + 1 < chars.len() {
                    let escaped = chars[index + 1];
                    if escaped == '"' || escaped == '\\' {
                        value.push(escaped);
                        index += 2;
                        continue;
                    }
                }
                value.push(chars[index]);
                index += 1;
            }
            tokens.push(value);
            index += 1;
            continue;
        }
        let mut value = String::new();
        while index < chars.len() && !chars[index].is_whitespace() {
            value.push(chars[index]);
            index += 1;
        }
        tokens.push(value);
    }
    tokens
}

// ── 响应 Cookie ───────────────────────────────────────────────────────

/// 一条 `Set-Cookie` 解析出来的字段。
///
/// 单请求形态下没有 cookie jar（参考页的「清空 Cookies」是给多请求工作台用的），
/// 这里只负责**看清**服务端种了什么：名字、值、域、路径、过期、三个开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub expires: Option<String>,
    pub max_age: Option<String>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
}

/// 从响应头里挑出 `set-cookie` 并逐条解析。
pub fn parse_set_cookies(headers: &[(String, String)]) -> Vec<Cookie> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(_, value)| parse_set_cookie(value))
        .collect()
}

fn parse_set_cookie(value: &str) -> Option<Cookie> {
    let mut parts = value.split(';');
    let first = parts.next()?.trim();
    let (name, cookie_value) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_string(),
        value: cookie_value.trim().to_string(),
        domain: None,
        path: None,
        expires: None,
        max_age: None,
        secure: false,
        http_only: false,
        same_site: None,
    };
    for attribute in parts {
        let attribute = attribute.trim();
        if attribute.eq_ignore_ascii_case("secure") {
            cookie.secure = true;
            continue;
        }
        if attribute.eq_ignore_ascii_case("httponly") {
            cookie.http_only = true;
            continue;
        }
        let Some((key, value)) = attribute.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "domain" => cookie.domain = Some(value),
            "path" => cookie.path = Some(value),
            "expires" => cookie.expires = Some(value),
            "max-age" => cookie.max_age = Some(value),
            "samesite" => cookie.same_site = Some(value),
            _ => {}
        }
    }
    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_parse_case_insensitively() {
        assert_eq!(Method::parse("get"), Some(Method::Get));
        assert_eq!(Method::parse("PATCH"), Some(Method::Patch));
        assert_eq!(Method::parse("TRACE"), None);
        // 只有这三个方法在 ureq 里带 body。
        assert!(Method::Post.has_body());
        assert!(!Method::Delete.has_body());
    }

    #[test]
    fn headers_parse_and_reject_malformed_lines() {
        let headers =
            parse_headers("Content-Type: application/json\n# 注释\n\nX-Trace: 1\n").expect("合法");
        assert_eq!(
            headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Trace".to_string(), "1".to_string()),
            ]
        );
        // 缺冒号 → 报错并带行号（静默丢头会让「为什么没生效」变成悬案）。
        let error = parse_headers("Content-Type: json\nThis is not a header\n").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BadHeader);
        assert_eq!(error.detail(), Some("2"));
    }

    #[test]
    fn auth_headers_are_built_and_basic_is_base64() {
        assert!(Auth::None.headers().is_empty());
        assert_eq!(
            Auth::Bearer("t".into()).headers()[0].1,
            "Bearer t".to_string()
        );
        // base64("user:pass") = dXNlcjpwYXNz
        assert_eq!(
            Auth::Basic {
                user: "user".into(),
                password: "pass".into()
            }
            .headers()[0]
                .1,
            "Basic dXNlcjpwYXNz".to_string()
        );
        assert_eq!(
            Auth::ApiKey {
                header: "X-Api-Key".into(),
                value: "k".into()
            }
            .headers()[0],
            ("X-Api-Key".to_string(), "k".to_string())
        );
    }

    #[test]
    fn empty_credentials_are_empty_input_not_a_blank_header() {
        assert_eq!(
            Auth::Bearer("  ".into()).check().unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            Auth::Basic {
                user: String::new(),
                password: "p".into()
            }
            .check()
            .unwrap_err()
            .kind(),
            ErrorKind::EmptyInput
        );
        assert!(Auth::None.check().is_ok());
    }

    #[test]
    fn secrets_are_redacted_in_reports() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer abc".to_string()),
            ("X-Api-Key".to_string(), "k".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Cookie".to_string(), "sid=1".to_string()),
        ];
        let redacted = redact_headers(&headers);
        assert_eq!(redacted[0].1, "***");
        assert_eq!(redacted[1].1, "***");
        assert_eq!(redacted[2].1, "application/json");
        assert_eq!(redacted[3].1, "***");
    }

    #[test]
    fn urls_must_carry_scheme_and_host() {
        validate_url("https://example.com/a").expect("https 合法");
        validate_url("http://10.0.0.1:8080").expect("带端口合法");
        assert_eq!(
            validate_url("example.com").unwrap_err().kind(),
            ErrorKind::BadUrl
        );
        assert_eq!(
            validate_url("  ").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            validate_url("http:///x").unwrap_err().kind(),
            ErrorKind::BadUrl
        );
    }

    #[test]
    fn json_bodies_are_pretty_and_others_are_left_alone() {
        assert_eq!(
            pretty_body("{\"a\":1}", "application/json"),
            "{\n  \"a\": 1\n}"
        );
        // 无 Content-Type 但长成 JSON 也要格式化（很多网关不回这个头）。
        assert_eq!(pretty_body("[1,2]", ""), "[\n  1,\n  2\n]");
        // 解析不了就原样，绝不留半截格式化结果。
        assert_eq!(pretty_body("{not json", "application/json"), "{not json");
        assert_eq!(pretty_body("<a/>", "application/xml"), "<a/>");
    }

    #[test]
    fn curl_command_never_contains_the_secret() {
        let request = HttpRequest {
            method: Method::Post,
            url: "https://example.com/api".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: b"{\"a\":1}".to_vec(),
            body_kind: BodyKind::Raw,
            auth: Auth::Bearer("super-secret".into()),
            ..HttpRequest::default()
        };
        let curl = curl_command(&request);
        assert!(curl.contains("curl -X POST"));
        assert!(curl.contains("Authorization: Bearer $TOKEN"));
        assert!(!curl.contains("super-secret"));
        assert!(curl.contains("--data-raw"));
    }

    #[test]
    fn pairs_skip_comments_and_report_line_numbers() {
        let pairs = parse_pairs("page=2\n# 注释\nq=hello world\n\n").expect("合法");
        assert_eq!(
            pairs,
            vec![
                ("page".to_string(), "2".to_string()),
                ("q".to_string(), "hello world".to_string()),
            ]
        );
        let error = parse_pairs("page=2\n没有等号\n").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BadPair);
        assert_eq!(error.detail(), Some("2"));
        // 空键同样是错：`=x` 拼进查询串只是垃圾。
        assert_eq!(parse_pairs("=x\n").unwrap_err().kind(), ErrorKind::BadPair);
    }

    #[test]
    fn vars_are_substituted_and_missing_ones_fail() {
        let vars = vec![
            ("host".to_string(), "10.0.0.1:8080".to_string()),
            ("id".to_string(), "7".to_string()),
        ];
        assert_eq!(
            resolve_vars("http://{{host}}/orders/{{ id }}", &vars).unwrap(),
            "http://10.0.0.1:8080/orders/7"
        );
        // 没有占位符时原样返回（不动 `{` 这类正常字符）。
        assert_eq!(resolve_vars("{\"a\":1}", &vars).unwrap(), "{\"a\":1}");
        // 未定义 → 报错并带变量名，而不是静默换成空串。
        let error = resolve_vars("{{missing}}", &vars).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Variable);
        assert_eq!(error.detail(), Some("missing"));
        // 没配对的 `{{` 保留原样（JSON / 模板串里可能出现）。
        assert_eq!(resolve_vars("a {{ b", &vars).unwrap(), "a {{ b");
    }

    #[test]
    fn query_is_appended_before_fragment_and_encoded() {
        let pairs = vec![
            ("q".to_string(), "a b&c".to_string()),
            ("page".to_string(), "2".to_string()),
        ];
        assert_eq!(
            append_query("https://example.com/s", &pairs),
            "https://example.com/s?q=a%20b%26c&page=2"
        );
        assert_eq!(
            append_query("https://example.com/s?x=1", &pairs),
            "https://example.com/s?x=1&q=a%20b%26c&page=2"
        );
        // 片段必须留在最后，否则参数会落进 `#` 后面、服务端收不到。
        assert_eq!(
            append_query("https://example.com/s#top", &pairs),
            "https://example.com/s?q=a%20b%26c&page=2#top"
        );
        // 空键不算参数。
        assert_eq!(
            append_query(
                "https://example.com/s",
                &[("".to_string(), "1".to_string())]
            ),
            "https://example.com/s"
        );
    }

    #[test]
    fn urlencoded_body_uses_percent_encoding() {
        let body = encode_urlencoded(&[
            ("name".to_string(), "张 三".to_string()),
            ("tag".to_string(), "a+b".to_string()),
        ]);
        assert_eq!(body, "name=%E5%BC%A0%20%E4%B8%89&tag=a%2Bb");
        // 空格编成 %20 而不是 `+`：`+` 在查询串里各家实现不一致。
        assert!(!body.contains('+'));
    }

    #[test]
    fn multipart_wraps_text_fields_with_boundary() {
        let multipart = encode_multipart(&[
            ("name".to_string(), "duck".to_string()),
            ("note".to_string(), "两行\r\n文本".to_string()),
        ])
        .expect("文本字段不读盘");
        let text = String::from_utf8(multipart.body.clone()).expect("UTF-8");
        assert!(text.starts_with(&format!("--{}\r\n", multipart.boundary)));
        assert!(text.ends_with(&format!("--{}--\r\n", multipart.boundary)));
        assert!(text.contains("Content-Disposition: form-data; name=\"name\""));
        assert!(text.contains("\r\n\r\nduck\r\n"));
        assert!(multipart.content_type().contains(&multipart.boundary));
    }

    #[test]
    fn multipart_reads_file_fields_and_keeps_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "duck-http-multipart-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建目录");
        let file = dir.join("blob.bin");
        // 非 UTF-8 字节必须原样进体：`from_utf8_lossy` 会把它换成 U+FFFD。
        std::fs::write(&file, [0xff, 0x00, 0x41]).expect("写文件");

        let multipart = encode_multipart(&[("upload".to_string(), format!("@{}", file.display()))])
            .expect("读得到文件");
        assert!(
            multipart
                .body
                .windows(3)
                .any(|window| window == [0xff, 0x00, 0x41])
        );
        let text = String::from_utf8_lossy(&multipart.body);
        assert!(text.contains("filename=\"blob.bin\""));
        assert!(text.contains("Content-Type: application/octet-stream"));

        // 路径不存在 → PathMissing（带路径），不是「发个空附件」。
        let error =
            encode_multipart(&[("upload".to_string(), "@ /definitely/not/here".to_string())])
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PathMissing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn curl_import_reads_method_headers_body_and_auth() {
        let curl = r#"curl 'https://api.example.com/v1/orders?page=1' \
  -X POST \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer abc.def' \
  --data-raw '{"sku":"A1","qty":2}'"#;
        let parsed = parse_curl(curl).expect("能解析");
        assert_eq!(parsed.method, Some(Method::Post));
        assert_eq!(parsed.url, "https://api.example.com/v1/orders?page=1");
        assert_eq!(parsed.auth, Auth::Bearer("abc.def".to_string()));
        assert_eq!(parsed.body, r#"{"sku":"A1","qty":2}"#);
        // Authorization 拆进 auth 后不再留在头里，避免同名头出现两次。
        assert!(
            parsed
                .headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        );
        assert_eq!(
            parsed.headers[0],
            ("Content-Type".to_string(), "application/json".to_string())
        );
    }

    #[test]
    fn curl_import_handles_basic_auth_short_flags_and_missing_scheme() {
        let parsed = parse_curl("curl -u 'admin:s3cret' -k api.example.com/ping").expect("能解析");
        assert_eq!(
            parsed.auth,
            Auth::Basic {
                user: "admin".into(),
                password: "s3cret".into()
            }
        );
        // 没写 scheme 补 http（curl 同口径）。
        assert_eq!(parsed.url, "http://api.example.com/ping");
        assert!(parsed.insecure);
        // `-d` 自动升 POST。
        let posted = parse_curl("curl https://a.b -d 'x=1' -d 'y=2'").expect("能解析");
        assert_eq!(posted.method, Some(Method::Post));
        assert_eq!(posted.body, "x=1&y=2");
        // `-G` 把体搬到查询串，方法仍是 GET。
        let get = parse_curl("curl -G https://a.b/s -d 'q=1'").expect("能解析");
        assert_eq!(get.method, Some(Method::Get));
        assert_eq!(get.url, "https://a.b/s?q=1");
        assert!(get.body.is_empty());
        // 空命令 → EmptyInput；没有 URL → EmptyInput。
        assert_eq!(parse_curl("   ").unwrap_err().kind(), ErrorKind::EmptyInput);
        assert_eq!(
            parse_curl("curl -X POST").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
    }

    #[test]
    fn set_cookies_are_parsed_into_fields() {
        let headers = vec![
            (
                "Set-Cookie".to_string(),
                "sid=abc123; Path=/; Domain=.example.com; Secure; HttpOnly; SameSite=Lax"
                    .to_string(),
            ),
            (
                "set-cookie".to_string(),
                "theme=dark; Max-Age=3600; Expires=Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
            ),
            ("Content-Type".to_string(), "text/plain".to_string()),
        ];
        let cookies = parse_set_cookies(&headers);
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].value, "abc123");
        assert_eq!(cookies[0].domain.as_deref(), Some(".example.com"));
        assert_eq!(cookies[0].path.as_deref(), Some("/"));
        assert!(cookies[0].secure);
        assert!(cookies[0].http_only);
        assert_eq!(cookies[0].same_site.as_deref(), Some("Lax"));
        assert_eq!(cookies[1].max_age.as_deref(), Some("3600"));
        assert!(!cookies[1].secure);
        // 没有 `set-cookie` 就是空表（报告据此决定要不要出这一段）。
        assert!(parse_set_cookies(&[("X".to_string(), "1".to_string())]).is_empty());
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }
}
