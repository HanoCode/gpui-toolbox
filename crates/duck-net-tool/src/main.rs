//! 网络与联调族（`duck.net.*`）的 **process 轨入口**。
//!
//! 与 SQL / 合并 / 比对 / 文件四族同一个骨架：一个二进制服务整族工具，按
//! `request.tool` 分派到 [`duck_net_core`]。新增一个工具＝加一条 `match` 分支 +
//! 一份 `plugin.toml`。
//!
//! 本文件只做四件事（逻辑全在内核里，见各模块头）：
//!
//! 1. 把 `[io]` 参数摊平成内核入参；
//! 2. 把内核的产出排成**可复制的文本报告**（宿主的结果表渲染器还没做，见计划 §8）；
//! 3. 把内核的「稳定码 / 枚举」翻成当前语言的一句话（[`i18n::fill`] 负责插值）；
//! 4. 把 HTTP 的最近请求记到插件自己的 `data/http-history.json`（见下）。
//!
//! ## 为什么历史落盘而不是走 kv
//!
//! 协议里 `kv` 是**只读快照**（ADR-4：宿主读好传入，写回要走回执），而 process
//! 轨目前没有回执事件。文件族已经踩过同一条路：撤销日志落在
//! `<plugin>/data/undo/`（`current_exe` 上两级 + `data`），这里照抄，
//! 历史落在 `<plugin>/data/http-history.json`，只留最近 [`MAX_HISTORY`] 条。
//!
//! 历史里**只记方法与地址、状态码、耗时**：请求头与请求体可能带 token，
//! 落盘就等于把凭据写进文件 —— 设计稿的硬约束是「凭据不落盘、不进日志」。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use duck_net_core::s3::{Client, S3Config};
use duck_net_core::{
    ApplyCommand, Auth, BodyKind, Charset, CodecConfig, DecodeIssue, ErrorKind, FtpTarget,
    HostsIssue, HostsProfile, HttpRequest, IssueKind, IssueLevel, Method, NetError, Payload,
    ReplaceOutcome, S3Target, SftpTarget, Step, StepId, StepStatus, TcpRequest, append_query,
    build_apply_script, check_ftp, check_s3, check_sftp, curl_command, diff_contents,
    encode_multipart, encode_urlencoded, find_profile, format_size, guess_content_type, hex_dump,
    merge_sections, parse_curl, parse_headers, parse_hosts, parse_pairs, parse_profile_meta,
    parse_set_cookies, pretty_body, redact_headers, replace_in_profiles, require_file,
    resolve_vars, search_profiles, validate_hosts_content,
};
use duck_plugin::PluginLocales;
use duck_plugin_process::{PluginOutput, RunRequest, serve};

mod i18n;

/// HTTP 历史保留条数（设计稿：最近 20 条）。
const MAX_HISTORY: usize = 20;
/// 报文报告里单帧最多显示多少字节（剩下的只报长度，避免一屏全是十六进制）。
const MAX_FRAME_PREVIEW: usize = 2048;
/// 报文报告里最多逐帧列几帧。
const MAX_FRAME_ROWS: usize = 16;

/// 参数区（与其余四族同一套：宿主把表单值序列化成字符串键值对）。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    fn text(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.trim().to_owned(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    fn flag(&self, key: &str) -> bool {
        self.text(key).eq_ignore_ascii_case("true")
    }

    /// 默认**开**的开关：键缺席时返回 `fallback`。
    ///
    /// `switch` 的 `default = "true"` 只有宿主渲染表单时才写进 params；直接构造
    /// 请求（CLI、测试、别的调用方）时键是缺席的，`flag()` 会把「默认开」静默翻成
    /// 关。文件族已经为这个坑加过同一份兜底（`text-diff` 的「忽略大小写」）。
    fn flag_or(&self, key: &str, fallback: bool) -> bool {
        if self.0.get(key).is_none() {
            return fallback;
        }
        self.flag(key)
    }

    /// 数值参数：空串 / 解析失败 → `fallback`（留空就是「用默认」）。
    fn number(&self, key: &str, fallback: usize) -> usize {
        self.text(key).parse().unwrap_or(fallback)
    }
}

/// 取词器：`locales/` 由宿主通过环境变量指路，缺失时原样返回中文源串。
struct Text {
    locales: PluginLocales,
    lang: String,
}

impl Text {
    fn load() -> Self {
        let lang = std::env::var(duck_plugin::ENV_LOCALE).unwrap_or_else(|_| "zh-Hans".to_owned());
        // `DUCK_LOCALES_DIR` 已经是 `locales/` 这一级：用 `load` 会再拼一次目录。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(std::path::Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    fn get(&self, source: &str) -> String {
        self.locales.text(&self.lang, source).to_owned()
    }

    /// 取词 + 插值一步到位（模板来自译文表，`{}` 顺序以译文为准）。
    fn fill(&self, template: &str, args: &[&str]) -> String {
        i18n::fill(&self.get(template), args)
    }
}

fn main() -> ExitCode {
    serve(handle)
}

fn handle(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
) -> Result<String, duck_plugin::PluginError> {
    let text = Text::load();
    match request.tool.as_str() {
        "duck.net.http-request" => http_run(request, out, &text),
        "duck.net.packet-send" => packet_run(request, out, &text),
        "duck.net.s3-ftp-checker" => check_run(request, out, &text),
        "duck.net.hosts-manager" => hosts_run(request, out, &text),
        "duck.net.s3-manager" => s3_run(request, out, &text),
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck-net-tool: unknown tool {other}"
        ))),
    }
}

/// 内核错误（稳定码 + 细节）→ 当前语言的一句话。
///
/// 细节（主机:端口、HTTP 状态、对端原文）不翻译：那是数据，翻它没有意义。
fn fail(text: &Text, error: &NetError) -> duck_plugin::PluginError {
    let base = text.get(match error.kind() {
        ErrorKind::EmptyInput => "输入不完整，请检查必填项",
        ErrorKind::BadUrl => "URL 写错了（需要以 http:// 或 https:// 开头）",
        ErrorKind::BadHeader => "请求头格式不对（每行应为「名字: 值」）",
        ErrorKind::Unsupported => "不支持这种协议或方法",
        ErrorKind::Timeout => "等待超时",
        ErrorKind::Dns => "域名解析失败",
        ErrorKind::Connect => "连接失败",
        ErrorKind::Tls => "TLS 握手或证书校验失败",
        ErrorKind::Http => "服务端返回了错误状态",
        ErrorKind::Auth => "认证失败",
        ErrorKind::Decode => "报文解析失败",
        ErrorKind::TooLarge => "内容超过上限",
        ErrorKind::Io => "读写失败",
        ErrorKind::PathMissing => "路径不存在",
        ErrorKind::Elevation => "权限不足",
        ErrorKind::BadPair => "键值行写错了（每行应为「键=值」）",
        ErrorKind::Variable => "变量没有定义",
    });
    match error.detail() {
        // 分隔符也走译文表：中文习惯全角冒号，英文是 `": "`。写成裸的 `"："`
        // 会让英文界面出现 `Connection failed：...` 这种半中半西的排版。
        Some(detail) => {
            let separator = text.get("：");
            duck_plugin::PluginError::Execution(format!("{base}{separator}{detail}"))
        }
        None => duck_plugin::PluginError::Execution(base),
    }
}

/// 插件自己的数据目录（HTTP 历史落这里）。
fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DUCK_PLUGIN_DATA") {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(|plugin| plugin.join("data")))
        .unwrap_or_else(|| std::env::temp_dir().join("duck-net-data"))
}

fn history_path() -> PathBuf {
    data_dir().join("http-history.json")
}

/// 最近请求（新的在前）。
fn load_history() -> Vec<serde_json::Value> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
        .unwrap_or_default()
}

/// 记一条历史。落盘失败**不影响本次结果**（历史是附加价值，不是主产物）。
fn push_history(entry: serde_json::Value) {
    let mut items = load_history();
    items.insert(0, entry);
    items.truncate(MAX_HISTORY);
    if std::fs::create_dir_all(data_dir()).is_err() {
        return;
    }
    if let Ok(raw) = serde_json::to_string_pretty(&items) {
        let _ = std::fs::write(history_path(), raw);
    }
}

// ── HTTP 请求 ──────────────────────────────────────────────────────────

/// 拼好的请求（参数与 cURL 两种来源归一到这里）。
struct Prepared {
    method: Method,
    url: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_kind: BodyKind,
    auth: Auth,
    insecure: bool,
    /// 来源是要在报告里点明的一句话（cURL 导入时提醒「参数区被忽略」）。
    from_curl: bool,
}

/// HTTP 请求：拼出请求 → 发一次 → 排一份报告。
///
/// 参数与 cURL 两条来源在这里归一：`source = curl` 时方法 / URL / 头 / 体 / 认证
/// 全部取自解析结果（参考页 import 之后也是把表单覆盖掉），`vars` / `query` 仍然
/// 生效——它们是在「发什么」之上再叠一层，与来源无关。
fn http_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let vars =
        parse_pairs(&pair_lines(&params.text("vars"))).map_err(|error| fail(text, &error))?;
    let from_curl = params.text("source") == "curl";

    let mut prepared = match from_curl {
        true => prepare_from_curl(&params, request, &vars, text)?,
        false => prepare_from_params(&params, request, &vars, text)?,
    };

    // 「API Key 放查询参数」与「放头」二选一（参考页 authApiIn）：选了查询就
    // 不能再挂同名头，否则服务端会看到两份。
    move_api_key_to_query(&mut prepared, from_curl, &params.text("key_in"));

    let http_request = HttpRequest {
        method: prepared.method,
        url: append_query(&prepared.url, &prepared.query),
        headers: prepared.headers,
        body: prepared.body,
        body_kind: prepared.body_kind,
        auth: prepared.auth,
        timeout_ms: params.number("timeout_ms", 30_000) as u64,
        max_redirects: params.number("max_redirects", 5) as u32,
        insecure: prepared.insecure || params.flag("insecure"),
    };

    out.log(text.fill(
        "请求 {} {}",
        &[http_request.method.as_str(), &http_request.url],
    ));
    let response = duck_net_core::send(&http_request).map_err(|error| fail(text, &error))?;
    out.log(text.fill(
        "响应 HTTP {}（{} ms）",
        &[
            &response.status.to_string(),
            &response.elapsed_ms.to_string(),
        ],
    ));

    let mut report = String::new();
    report.push_str(&format!(
        "{} {}\n",
        http_request.method.as_str(),
        http_request.url
    ));
    report.push_str(&text.fill(
        "认证：{}",
        &[match http_request.auth.describe() {
            "none" => "无",
            "bearer" => "Bearer",
            "basic" => "Basic",
            _ => "API Key",
        }],
    ));
    report.push('\n');
    if prepared.from_curl {
        report.push_str(&text.get("来源：cURL 命令解析结果"));
        report.push('\n');
    }

    // 变量表：名字照给，值按名字打码（`{{token}}` 这种就是凭据）。
    if !vars.is_empty() {
        report.push_str(&text.fill("变量（{} 个）", &[&vars.len().to_string()]));
        report.push('\n');
        for (name, value) in &vars {
            let shown = match looks_secret(name) {
                true => "***",
                false => value.as_str(),
            };
            report.push_str(&format!("  {name} = {shown}\n"));
        }
    }

    if !prepared.query.is_empty() {
        report.push_str(&text.fill("查询参数（{} 个）", &[&prepared.query.len().to_string()]));
        report.push('\n');
        for (name, value) in &prepared.query {
            let shown = match looks_secret(name) {
                true => "***",
                false => value.as_str(),
            };
            report.push_str(&format!("  {name} = {shown}\n"));
        }
    }

    report.push_str(&text.fill(
        "请求体：{} · {}",
        &[
            http_request.body_kind.as_str(),
            &text.fill("{} 字节", &[&http_request.body.len().to_string()]),
        ],
    ));
    report.push('\n');

    report.push_str(&format!(
        "{} {}\n",
        text.get("状态：HTTP"),
        match response.reason.is_empty() {
            true => response.status.to_string(),
            false => format!("{} {}", response.status, response.reason),
        }
    ));
    report.push_str(&text.fill(
        "耗时 {} ms · 体积 {} · 重定向上限 {}",
        &[
            &response.elapsed_ms.to_string(),
            &format_size(response.body_bytes as u64),
            &http_request.max_redirects.to_string(),
        ],
    ));
    report.push('\n');

    let shown_headers = redact_headers(
        &http_request
            .auth
            .headers()
            .into_iter()
            .chain(http_request.headers.iter().cloned())
            .collect::<Vec<_>>(),
    );
    if !shown_headers.is_empty() {
        report.push('\n');
        report.push_str(&text.get("请求头（密钥已打码）"));
        report.push('\n');
        for (name, value) in &shown_headers {
            report.push_str(&format!("  {name}: {value}\n"));
        }
    }

    report.push('\n');
    report.push_str(&text.get("响应头"));
    report.push('\n');
    if response.headers.is_empty() {
        report.push_str(&format!("  {}\n", text.get("（没有响应头）")));
    }
    for (name, value) in &response.headers {
        report.push_str(&format!("  {name}: {value}\n"));
    }

    // Cookies 子标签：单请求没有 cookie jar，能给的只有「服务端种了什么」。
    let cookies = parse_set_cookies(&response.headers);
    if !cookies.is_empty() {
        report.push('\n');
        report.push_str(&text.fill("Set-Cookie（{} 条）", &[&cookies.len().to_string()]));
        report.push('\n');
        for cookie in &cookies {
            let mut attributes: Vec<String> = Vec::new();
            if let Some(domain) = &cookie.domain {
                attributes.push(format!("Domain={domain}"));
            }
            if let Some(path) = &cookie.path {
                attributes.push(format!("Path={path}"));
            }
            if let Some(expires) = &cookie.expires {
                attributes.push(format!("Expires={expires}"));
            }
            if let Some(max_age) = &cookie.max_age {
                attributes.push(format!("Max-Age={max_age}"));
            }
            if cookie.secure {
                attributes.push("Secure".to_string());
            }
            if cookie.http_only {
                attributes.push("HttpOnly".to_string());
            }
            if let Some(same_site) = &cookie.same_site {
                attributes.push(format!("SameSite={same_site}"));
            }
            let suffix = match attributes.is_empty() {
                true => String::new(),
                false => format!("  ({})", attributes.join("; ")),
            };
            report.push_str(&format!("  {} = {}{suffix}\n", cookie.name, cookie.value));
        }
    }

    report.push('\n');
    let size_line = match response.truncated {
        true => text.fill(
            "响应体（{} 字节，已截断）",
            &[&response.body_bytes.to_string()],
        ),
        false => text.fill("响应体（{} 字节）", &[&response.body_bytes.to_string()]),
    };
    report.push_str(&size_line);
    report.push('\n');
    if response.body.trim().is_empty() {
        report.push_str(&format!("{}\n", text.get("（空响应体）")));
    } else {
        report.push_str(&pretty_body(
            &response.body,
            response.header("content-type").unwrap_or_default(),
        ));
        if !report.ends_with('\n') {
            report.push('\n');
        }
    }

    report.push('\n');
    report.push_str(&text.get("等价 curl（密钥用 $TOKEN 占位，粘贴前先导出变量）"));
    report.push('\n');
    report.push_str(&curl_command(&http_request));
    report.push('\n');

    push_history(serde_json::json!({
        "method": http_request.method.as_str(),
        "url": history_url(&http_request.url),
        "status": response.status,
        "elapsed_ms": response.elapsed_ms,
        "at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    }));
    let history = load_history();
    if !history.is_empty() {
        report.push('\n');
        report.push_str(&text.fill("最近请求（{} 条）", &[&history.len().to_string()]));
        report.push('\n');
        for item in &history {
            report.push_str(&format!(
                "  {} {} → {} ({} ms)\n",
                item.get("method").and_then(|v| v.as_str()).unwrap_or(""),
                item.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                item.get("status").and_then(|v| v.as_u64()).unwrap_or(0),
                item.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            ));
        }
    }

    Ok(report)
}

/// 把 API Key 挪进查询参数（参考页 `authApiIn = query`）。
///
/// 写成「早退 + 单个 `if let`」而不是 `if a && let Some(x) = b`：后者是
/// `let_chains`（Rust 1.88），本仓 MSRV 是 1.85。
fn move_api_key_to_query(prepared: &mut Prepared, from_curl: bool, key_in: &str) {
    if from_curl || key_in != "query" {
        return;
    }
    if let Auth::ApiKey { header, value } = prepared.auth.clone() {
        prepared.query.push((header, value));
        prepared.auth = Auth::None;
    }
}

/// 参数区来源：方法 / URL / 头 / 查询 / 体 / 认证全部从 `[io]` 读。
fn prepare_from_params(
    params: &Params,
    request: &RunRequest,
    vars: &[(String, String)],
    text: &Text,
) -> Result<Prepared, duck_plugin::PluginError> {
    let method = Method::parse(&params.text("method")).unwrap_or(Method::Get);
    let url = resolve_vars(&params.text("url"), vars).map_err(|error| fail(text, &error))?;
    let query = resolve_pairs(
        &parse_pairs(&pair_lines(&params.text("query"))).map_err(|e| fail(text, &e))?,
        vars,
    )
    .map_err(|error| fail(text, &error))?;
    let headers = resolve_pairs(
        &parse_headers(&params.text("headers")).map_err(|error| fail(text, &error))?,
        vars,
    )
    .map_err(|error| fail(text, &error))?;
    let auth = http_auth(params);
    let body_kind = BodyKind::parse(&params.text("body_type")).unwrap_or(BodyKind::Raw);

    let (body, headers) = match body_kind {
        BodyKind::None => (Vec::new(), headers),
        BodyKind::Raw => {
            // 请求体走 `input`（底部输入框），**不 trim**：报文首尾空白是内容。
            let raw = resolve_vars(&request.input, vars).map_err(|error| fail(text, &error))?;
            let content_type = match params.text("body_format").as_str() {
                "xml" => "application/xml",
                "html" => "text/html",
                "text" => "text/plain",
                _ => "application/json",
            };
            let headers = with_content_type(headers, content_type);
            (raw.into_bytes(), headers)
        }
        BodyKind::UrlEncoded => {
            let pairs = resolve_pairs(
                &parse_pairs(&pair_lines(&params.text("form"))).map_err(|e| fail(text, &e))?,
                vars,
            )
            .map_err(|error| fail(text, &error))?;
            let body = encode_urlencoded(&pairs).into_bytes();
            let headers = with_content_type(headers, "application/x-www-form-urlencoded");
            (body, headers)
        }
        BodyKind::FormData => {
            let pairs = resolve_pairs(
                &parse_pairs(&pair_lines(&params.text("form_fields")))
                    .map_err(|e| fail(text, &e))?,
                vars,
            )
            .map_err(|error| fail(text, &error))?;
            let multipart = encode_multipart(&pairs).map_err(|error| fail(text, &error))?;
            // multipart 的边界是这一次生成的，必须覆盖掉用户写的旧边界。
            let headers = set_content_type(headers, &multipart.content_type());
            (multipart.body, headers)
        }
        BodyKind::Binary => {
            let path = params.text("binary_file");
            let file = require_file(&path).map_err(|error| fail(text, &error))?;
            let bytes = std::fs::read(&file).map_err(|error| fail(text, &NetError::io(error)))?;
            let headers = with_content_type(headers, guess_content_type(&path));
            (bytes, headers)
        }
    };

    Ok(Prepared {
        method,
        url,
        query,
        headers,
        body,
        body_kind,
        auth,
        insecure: false,
        from_curl: false,
    })
}

/// cURL 来源：解析命令，忽略参数区的方法 / URL / 头 / 体（参考页 import 亦然）。
fn prepare_from_curl(
    params: &Params,
    request: &RunRequest,
    vars: &[(String, String)],
    text: &Text,
) -> Result<Prepared, duck_plugin::PluginError> {
    // cURL 命令粘在底部输入框（`[io.fields]` 只有一个可编辑文本域，
    // 框架还没有「按参数显隐的第二个文本域」）。
    let parsed = parse_curl(&request.input).map_err(|error| fail(text, &error))?;
    let method = parsed.method.unwrap_or(Method::Get);
    let url = resolve_vars(&parsed.url, vars).map_err(|error| fail(text, &error))?;
    let query = resolve_pairs(
        &parse_pairs(&pair_lines(&params.text("query"))).map_err(|e| fail(text, &e))?,
        vars,
    )
    .map_err(|error| fail(text, &error))?;
    let headers = resolve_pairs(&parsed.headers, vars).map_err(|error| fail(text, &error))?;
    let auth = parsed.auth;

    let (body, body_kind, headers) = if !parsed.form.is_empty() {
        let form = resolve_pairs(&parsed.form, vars).map_err(|error| fail(text, &error))?;
        let multipart = encode_multipart(&form).map_err(|error| fail(text, &error))?;
        let headers = set_content_type(headers, &multipart.content_type());
        (multipart.body, BodyKind::FormData, headers)
    } else if parsed.body.is_empty() {
        (Vec::new(), BodyKind::None, headers)
    } else {
        let body = resolve_vars(&parsed.body, vars).map_err(|error| fail(text, &error))?;
        (body.into_bytes(), BodyKind::Raw, headers)
    };

    Ok(Prepared {
        method,
        url,
        query,
        headers,
        body,
        body_kind,
        auth,
        insecure: parsed.insecure,
        from_curl: true,
    })
}

/// 把「一行多组」的输入摊开：`a=1&b=2` 与两行等价。
///
/// `[io.params]` 的 `text` 是**单行**输入框（宿主 `InputState` 没开多行），
/// 而 query / 表单天然是多组；不摊开的话用户只能一次填一组。`&` 在值里出现时
/// 写 `%26`（与 URL 的编码口径一致），解析器本身仍只按行切 —— 这个宽容留在
/// 工具层，内核语义保持干净。
fn pair_lines(raw: &str) -> String {
    raw.replace('&', "\n")
}

/// 键值表逐项插值（`query` / `form` 共用）。
fn resolve_pairs(
    pairs: &[(String, String)],
    vars: &[(String, String)],
) -> Result<Vec<(String, String)>, NetError> {
    pairs
        .iter()
        .map(|(key, value)| Ok((resolve_vars(key, vars)?, resolve_vars(value, vars)?)))
        .collect()
}

/// 头名是不是已经给了（大小写不敏感）。
fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case(name))
}

/// 缺 `Content-Type` 才补（用户显式写的说了算）。
fn with_content_type(headers: Vec<(String, String)>, value: &str) -> Vec<(String, String)> {
    if has_header(&headers, "content-type") {
        return headers;
    }
    let mut headers = headers;
    headers.push(("Content-Type".to_string(), value.to_string()));
    headers
}

/// 无条件设置 `Content-Type`（multipart 的边界只能由本次生成）。
fn set_content_type(mut headers: Vec<(String, String)>, value: &str) -> Vec<(String, String)> {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    headers.push(("Content-Type".to_string(), value.to_string()));
    headers
}

/// 历史里只留「地址 + 路径」，**查询串与片段一律丢掉**。
///
/// 查询串可能就是凭据的载体：`key_in = query` 的 API Key、用户手填的 `?token=`。
/// 历史要落盘，落盘就等于把凭据写进文件（模块头承诺「凭据不落盘」）。路径足够
/// 定位「打过哪个接口」，细节回 URL / 查询参数两个框里看。
fn history_url(url: &str) -> &str {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    &url[..cut]
}

/// 名字看着像凭据（变量表与查询参数里打码用）。
fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("api-key")
        || lower.contains("apikey")
        || lower.ends_with("key")
}

/// 认证参数 → [`Auth`]。
fn http_auth(params: &Params) -> Auth {
    match params.text("auth").as_str() {
        "bearer" => Auth::Bearer(params.text("token")),
        "basic" => Auth::Basic {
            user: params.text("user"),
            password: params.text("password"),
        },
        "apikey" => Auth::ApiKey {
            header: match params.text("key_name").is_empty() {
                true => "X-Api-Key".to_string(),
                false => params.text("key_name"),
            },
            value: params.text("key_value"),
        },
        _ => Auth::None,
    }
}

// ── 报文调试 ───────────────────────────────────────────────────────────

fn packet_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let host = params.text("host");
    let port = params.number("port", 0);
    let protocol = params.text("protocol");
    let charset = Charset::parse(&params.text("charset"));
    let hex_mode = params.flag("hex");
    let show_hex = params.flag("show_hex");

    let payload = match hex_mode {
        true => Payload::Hex(request.input.clone()),
        false => Payload::Text {
            text: request.input.clone(),
            charset,
        },
    };

    let codec = CodecConfig {
        protocol: match protocol.as_str() {
            "ascii_length" | "length_field" | "delimiter" | "fixed_length" => protocol.clone(),
            _ => "raw".to_string(),
        },
        header_len: Some(match protocol.as_str() {
            "ascii_length" => params.number("ascii_width", 8),
            _ => params.number("header_len", 4),
        }),
        max_body_len: Some(params.number("max_body", 10 * 1024 * 1024)),
        delimiter: Some(
            params
                .text("delimiter")
                .replace("\\n", "\n")
                .replace("\\r", "\r"),
        ),
        fixed_frame_len: Some(params.number("frame_len", 1024)),
    };
    if protocol == "delimiter" && params.text("delimiter").is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("分隔符协议要指定分隔符（例如 \\n 或 \\r\\n）"),
        ));
    }

    let tcp_request = TcpRequest {
        host: host.clone(),
        port: u16::try_from(port).unwrap_or(0),
        payload,
        codec,
        connect_timeout_ms: params.number("connect_timeout", 5_000) as u64,
        read_timeout_ms: params.number("read_timeout", 5_000) as u64,
        max_replies: params.number("max_replies", 16),
        max_bytes: params.number("max_bytes", 2 * 1024 * 1024),
        charset,
    };

    out.log(text.fill("连接 {}:{}", &[&host, &port.to_string()]));
    let outcome = duck_net_core::tcp::exchange(&tcp_request).map_err(|error| fail(text, &error))?;

    let mut report = String::new();
    report.push_str(&text.fill(
        "发送到 {}:{}（协议 {} · 编码 {}）",
        &[
            &host,
            &port.to_string(),
            &protocol_name(&protocol),
            charset.name(),
        ],
    ));
    report.push('\n');
    report.push_str(&text.fill(
        "发出 {} 字节，耗时 {} ms",
        &[
            &outcome.sent_bytes.to_string(),
            &outcome.elapsed_ms.to_string(),
        ],
    ));
    report.push('\n');
    report.push('\n');
    report.push_str(&text.get("发出（十六进制）"));
    report.push('\n');
    report.push_str(&outcome.sent_hex);
    report.push('\n');
    report.push_str(&text.fill(
        "收到 {} 帧 / {} 字节",
        &[
            &outcome.replies.len().to_string(),
            &outcome.received_bytes().to_string(),
        ],
    ));
    report.push('\n');
    for (index, frame) in outcome.replies.iter().take(MAX_FRAME_ROWS).enumerate() {
        report.push('\n');
        let index_text = (index + 1).to_string();
        let size_text = frame.len().to_string();
        report.push_str(&text.fill("第 {} 帧（{} 字节）", &[&index_text, &size_text]));
        report.push('\n');
        report.push_str(&frame_view(frame, charset, show_hex, text));
        report.push('\n');
    }
    if outcome.replies.len() > MAX_FRAME_ROWS {
        report.push('\n');
        let rest = (outcome.replies.len() - MAX_FRAME_ROWS).to_string();
        report.push_str(&text.fill("（还有 {} 帧未列出）", &[&rest]));
        report.push('\n');
    }
    if !outcome.leftover.is_empty() {
        report.push('\n');
        report.push_str(&text.fill(
            "尾巴 {} 字节（没凑成完整帧，可能是对端没发完或多给了分隔符）",
            &[&outcome.leftover.len().to_string()],
        ));
        report.push('\n');
        report.push_str(&hex_dump(
            &outcome.leftover[..outcome.leftover.len().min(256)],
        ));
    }
    if let Some(issue) = &outcome.issue {
        report.push('\n');
        report.push_str(&describe_issue(text, issue));
        report.push('\n');
    }
    if outcome.replies.is_empty() && outcome.leftover.is_empty() {
        report.push('\n');
        report.push_str(&text.get("对端没有返回任何数据（连上了，但没应答）"));
        report.push('\n');
    }

    Ok(report)
}

fn protocol_name(protocol: &str) -> String {
    match protocol {
        "ascii_length" => "ASCII 长度".to_string(),
        "length_field" => "长度字段".to_string(),
        "delimiter" => "分隔符".to_string(),
        "fixed_length" => "定长".to_string(),
        _ => "原始 TCP".to_string(),
    }
}

/// 一帧的展示：Hex 视图给十六进制转储，文本视图给解码后的字符串。
///
/// 文本视图里 `\r` / `\n` 会被转义成字面量 —— 报文里的换行是协议的一部分，
/// 原样输出会让「一帧」看起来像「好几行」，读日志的人根本分不清帧边界。
fn frame_view(frame: &[u8], charset: Charset, show_hex: bool, text: &Text) -> String {
    let clipped = &frame[..frame.len().min(MAX_FRAME_PREVIEW)];
    if show_hex {
        return hex_dump(clipped);
    }
    let decoded = charset
        .decode(clipped)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    let mut out = format!("{}\n", text.fill("（文本视图 {}）", &[charset.name()]));
    out.push_str(&decoded);
    out.push('\n');
    out
}

/// `DecodeIssue` → 一句话（七个变体逐一对应，插件表里各有一条）。
fn describe_issue(text: &Text, issue: &DecodeIssue) -> String {
    match issue {
        DecodeIssue::UnsupportedWidth(width) => text.fill(
            "长度字段宽度是 {} 字节，只支持 1 / 2 / 4",
            &[&width.to_string()],
        ),
        DecodeIssue::ZeroLength => text.get("长度字段是 0，无法据此切帧"),
        DecodeIssue::HeaderNotAscii(hex) => text.fill("ASCII 长度头部不是文本：{}", &[hex]),
        DecodeIssue::HeaderAllZeros => text.get("ASCII 长度头部全是 0"),
        DecodeIssue::HeaderNotNumber(raw) => text.fill("ASCII 长度头部不是数字：{}", &[raw]),
        DecodeIssue::TooLarge { len, max } => text.fill(
            "报文声明 {} 字节，超过上限 {}",
            &[&len.to_string(), &max.to_string()],
        ),
        DecodeIssue::Incomplete {
            expected,
            available,
        } => text.fill(
            "帧还没收全：声明 {} 字节，只到 {} 字节",
            &[&expected.to_string(), &available.to_string()],
        ),
    }
}

// ── S3 / FTP / SFTP 连通检测 ────────────────────────────────────────────

fn check_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let host = params.text("host");
    let port = params.number("port", 0);
    let timeout_ms = params.number("timeout_ms", 10_000) as u64;
    let protocol = params.text("protocol");

    // 每个分支都产出「报告抬头用的目标串 + 检测结果」这一对，所以直接让 `match`
    // 当表达式用：`let target_label; let outcome;` 那种先声明后赋值的写法会让
    // 「某个分支忘了赋值」变成运行期才发现的问题。
    let (target_label, outcome) = match protocol.as_str() {
        "ftp" => {
            let label = format!("FTP {host}:{port}");
            out.log(text.fill("FTP 登录 {}", &[&label]));
            let outcome = check_ftp(&FtpTarget {
                host: host.clone(),
                port: u16::try_from(port).unwrap_or(0),
                username: params.text("username"),
                password: params.text("password"),
                timeout_ms,
            })
            .map_err(|error| fail(text, &error))?;
            (label, outcome)
        }
        "sftp" => {
            let label = format!("SFTP {host}:{port}");
            out.log(text.fill("SSH 认证 {}", &[&label]));
            let outcome = check_sftp(&SftpTarget {
                host: host.clone(),
                port: u16::try_from(port).unwrap_or(0),
                username: params.text("username"),
                password: Some(params.text("password")).filter(|value| !value.is_empty()),
                key_path: Some(params.text("key_path")).filter(|value| !value.is_empty()),
                remote_path: params.text("remote_path"),
                timeout_ms,
            })
            .map_err(|error| fail(text, &error))?;
            (label, outcome)
        }
        _ => {
            let endpoint = match host.starts_with("http://") || host.starts_with("https://") {
                true => host.clone(),
                false => format!("https://{host}"),
            };
            let bucket = params.text("bucket");
            let label = format!("S3 {endpoint}/{bucket}");
            out.log(text.fill("HEAD bucket {}", &[&label]));
            let outcome = check_s3(&S3Target {
                endpoint,
                bucket,
                access_key: Some(params.text("access_key")).filter(|value| !value.is_empty()),
                secret_key: Some(params.text("secret_key")).filter(|value| !value.is_empty()),
                region: params.text("region"),
                timeout_ms,
                insecure: params.flag("insecure"),
            })
            .map_err(|error| fail(text, &error))?;
            (label, outcome)
        }
    };

    let mut report = String::new();
    report.push_str(&text.fill(
        "{} · {} · 耗时 {} ms",
        &[
            &text.get("目标"),
            &target_label,
            &outcome.elapsed_ms.to_string(),
        ],
    ));
    report.push('\n');
    if let Some(banner) = &outcome.banner {
        report.push_str(&text.fill("对端：{}", &[banner]));
        report.push('\n');
    }
    report.push('\n');
    report.push_str(&text.get("步骤"));
    report.push('\n');
    let separator = text.get("：");
    for step in &outcome.steps {
        report.push_str(&format!(
            "  {} {}{}\n",
            step_mark(step.status),
            text.get(step_label(step.id)),
            match &step.detail {
                Some(detail) => format!("{separator}{detail}"),
                None => String::new(),
            }
        ));
    }
    report.push('\n');
    report.push_str(&match outcome.ok() {
        true => text.get("结论：通过"),
        false => text.get("结论：失败"),
    });
    report.push('\n');
    // 「失败步骤 → 下一步查什么」用 `and_then` 串起来：两层 `if let` 在 MSRV 1.85
    // 上没法合成 `if let ... && let ...`（`let_chains` 要 1.88）。
    if let Some(hint) = outcome
        .failed_step()
        .and_then(|step| hint_for(text, &protocol, step))
    {
        report.push_str(&text.fill("建议：{}", &[&hint]));
        report.push('\n');
    }

    Ok(report)
}

fn step_mark(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "✓",
        StepStatus::Fail => "✗",
        StepStatus::Skipped => "·",
    }
}

/// 步骤名（`StepId` → 译文表里的键）。
fn step_label(id: StepId) -> &'static str {
    match id {
        StepId::Credential => "凭据",
        StepId::Request => "发出请求",
        StepId::Response => "响应状态",
        StepId::Tcp => "TCP 连接",
        StepId::Banner => "欢迎语",
        StepId::User => "USER",
        StepId::Password => "PASS",
        StepId::Pwd => "PWD",
        StepId::Ssh => "SSH 认证",
        StepId::Path => "远端路径",
    }
}

/// 失败时给一句「下一步该查什么」—— 连通检测的全部价值就在这里。
fn hint_for(text: &Text, protocol: &str, step: &Step) -> Option<String> {
    let detail = step.detail.clone().unwrap_or_default();
    match (step.id, step.status) {
        (_, StepStatus::Ok | StepStatus::Skipped) => None,
        (StepId::Credential, _) if detail == "partial" => {
            Some(text.get("Access Key 与 Secret Key 要一起填，或者都留空做匿名访问"))
        }
        (StepId::Tcp, _) => {
            Some(text.get("检查主机、端口与网络：拒绝连接看防火墙，解析失败看 DNS"))
        }
        (StepId::Ssh, _) if detail.starts_with("auth") => {
            Some(text.get("认证没过：核对用户名、口令或私钥，以及服务端的登录限制"))
        }
        (StepId::Path, _) => {
            Some(text.get("认证已通过但目录进不去：换一个路径，或确认账号有该目录的读权限"))
        }
        (StepId::Password, _) => {
            Some(text.get("口令被拒：核对用户名口令，注意服务端是否要求被动模式或 TLS"))
        }
        (StepId::Response, _) if protocol == "s3" => match detail.as_str() {
            "403" => Some(text.get("403：桶存在但没权限 —— 核对 AccessKey/SecretKey 与桶策略")),
            "404" => Some(text.get("404：桶不存在 —— 核对桶名，或 Endpoint 是否指向了正确的区域")),
            "301" => Some(text.get("301：桶在另一个区域 —— 改 Region，或换成该区域的 Endpoint")),
            status if status.starts_with('5') => {
                Some(text.get("5xx：对端内部错误 —— 稍后重试，或让对方查服务端日志"))
            }
            _ => Some(text.get("状态不是 200：先看响应头，再核对签名与桶权限")),
        },
        _ => None,
    }
}

// ── Hosts 管理 ─────────────────────────────────────────────────────────

fn hosts_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let dir = params.text("profiles_dir");
    if dir.is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("先选一个方案目录（每个 *.hosts / *.txt 文件算一个方案）"),
        ));
    }
    let default_name = params.text("name");
    let env = params.text("env");
    // 「连停用的方案一起合并」：默认**不**合并（停用标记的意义就在于默认被尊重），
    // 但允许临时覆盖 —— 比如想看看「全开」是什么样。
    let include_disabled = params.flag("include_disabled");

    let files = profile_files(Path::new(&dir)).map_err(|error| fail(text, &error))?;
    if files.is_empty() {
        return Err(duck_plugin::PluginError::Execution(
            text.get("目录里没有方案文件（*.hosts / *.txt）"),
        ));
    }

    let mut profiles = Vec::new();
    for (index, path) in files.iter().enumerate() {
        let raw = std::fs::read(path).map_err(|error| fail(text, &NetError::io(error)))?;
        let content = String::from_utf8_lossy(&raw).into_owned();
        let meta = parse_profile_meta(&content);
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("hosts");
        // 方案名的优先级：文件头 `# name:` → 参数「方案名称」（**只在单文件时**用，
        // 一个名字套到三个文件上只会得到三段同名方案，读报告时无法分辨）→ 文件名。
        let name = meta.name.unwrap_or_else(|| match default_name.is_empty() {
            false if files.len() == 1 => default_name.clone(),
            _ => stem.to_string(),
        });
        profiles.push(HostsProfile {
            slug: stem.to_string(),
            name,
            env: meta.env.unwrap_or_else(|| env.clone()),
            owner: meta.owner.unwrap_or_default(),
            ticket: meta.ticket.unwrap_or_default(),
            desc: meta.desc.unwrap_or_default(),
            enabled: meta.enabled.unwrap_or(true),
            content,
        });
        out.progress(index as u64 + 1, Some(files.len() as u64), None);
    }

    let disabled = profiles.iter().filter(|profile| !profile.enabled).count();
    let active: Vec<HostsProfile> = profiles
        .iter()
        .filter(|profile| profile.enabled || include_disabled)
        .cloned()
        .collect();
    if active.is_empty() {
        return Err(duck_plugin::PluginError::Execution(text.get(
            "所有方案都被停用了（文件头写了 `# enabled: false`）：没有内容可合并",
        )));
    }

    let sections = merge_sections(&active);
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut merged = String::new();
    merged.push_str(&text.get("# 由 Hosts 管理工具生成"));
    merged.push('\n');
    merged.push_str(&text.fill("# 生成时间：{}", &[&now]));
    merged.push('\n');
    let mut rendered = 0usize;
    for section in &sections {
        if section.content.is_empty() {
            continue;
        }
        rendered += 1;
        merged.push('\n');
        merged.push_str(&text.fill(
            "# ===== 方案：{}（{}） =====",
            &[&section.name, &section.env],
        ));
        merged.push('\n');
        merged.push_str(&section.content);
        merged.push('\n');
    }

    let parsed = parse_hosts(&merged);
    let per_profile: Vec<duck_net_core::HostsParseResult> = profiles
        .iter()
        .map(|profile| parse_hosts(&profile.content))
        .collect();

    let mut report = String::new();
    report.push_str(&text.fill(
        "合并 {} 个方案（{} 条生效 · {} 个域名）",
        &[
            &rendered.to_string(),
            &parsed.stats.active.to_string(),
            &parsed.stats.domains.to_string(),
        ],
    ));
    report.push('\n');
    if disabled > 0 && !include_disabled {
        report.push_str(&text.fill(
            "另有 {} 个方案已停用，没有进入合并",
            &[&disabled.to_string()],
        ));
        report.push('\n');
    }
    report.push('\n');

    // 方案清单：对应参考页左栏那排卡片（启用状态 / 名称 / 环境 / 条数 / 域名）。
    // 多出来的「负责人 · 变更单」来自文件头，银行场景里合并结果要进变更单，
    // 谁改的、单号多少得能追。
    report.push_str(&text.get("方案清单"));
    report.push('\n');
    for (index, profile) in profiles.iter().enumerate() {
        let stats = per_profile[index].stats;
        let mark = match profile.enabled {
            true => "●",
            false => "○",
        };
        let mut line = format!(
            "{mark} {}  {}（{}）",
            profile.slug, profile.name, profile.env
        );
        // 分隔符不进译文表：`·` 不是词，进了表只会多出一条「以空格开头的键」，
        // 而那种键会把 i18n 门禁的取键逻辑推到边界上。
        if !profile.owner.is_empty() {
            line.push_str(&format!("  · {} {}", text.get("负责人"), profile.owner));
        }
        if !profile.ticket.is_empty() {
            line.push_str(&format!("  · {} {}", text.get("变更单"), profile.ticket));
        }
        line.push_str(&format!(
            "  · {}",
            text.fill(
                "{} 条 / {} 域名",
                &[&stats.active.to_string(), &stats.domains.to_string()]
            )
        ));
        if !profile.enabled {
            line.push_str(&format!("  · {}", text.get("已停用")));
        }
        report.push_str(&line);
        report.push('\n');
        if !profile.desc.is_empty() {
            report.push_str(&format!("    {}\n", profile.desc));
        }
    }

    report.push('\n');
    report.push_str(&text.get("合并结果"));
    report.push('\n');
    report.push_str("```text\n");
    report.push_str(&merged);
    report.push_str("```\n");

    // 逐方案校验：合并前的「这份文件自己写错了没有」。
    //
    // 与下面「合并结果检查」分工不同：这里看不出跨方案冲突（两个文件抢同一个
    // 域名），那边看不出「是哪个文件写的」。单方案时两段内容完全重合，省掉。
    if profiles.len() > 1 {
        report.push('\n');
        report.push_str(&text.get("校验（逐方案）"));
        report.push('\n');
        for (index, profile) in profiles.iter().enumerate() {
            let result = &per_profile[index];
            if result.issues.is_empty() {
                report.push_str(&format!("  {}  {}\n", profile.slug, text.get("无问题")));
                continue;
            }
            report.push_str(&format!(
                "  {}  {}\n",
                profile.slug,
                text.fill("{} 处问题", &[&result.issues.len().to_string()])
            ));
            for issue in result.issues.iter().take(3) {
                report.push_str(&format!(
                    "    {}  {}\n",
                    text.fill("第 {} 行", &[&issue.line.to_string()]),
                    issue_text(text, issue)
                ));
            }
            if result.issues.len() > 3 {
                let rest = (result.issues.len() - 3).to_string();
                report.push_str(&format!(
                    "    {}\n",
                    text.fill("（还有 {} 处未列出）", &[&rest])
                ));
            }
        }
    }

    // 逐方案已经报过的「同一行、同一种问题」，在合并检查里不再重复 —— 同一句
    // 「非法 IP」带着两个不同行号出现两次，只会让人以为有两处，回头去改第二遍。
    // 合并检查剩下的就是它独有的那部分：**跨方案冲突**（两个文件抢同一个域名）。
    let mut reported: Vec<(&str, &'static str)> = Vec::new();
    if profiles.len() > 1 {
        for (index, result) in per_profile.iter().enumerate() {
            let lines: Vec<&str> = profiles[index].content.lines().collect();
            for issue in &result.issues {
                if let Some(raw) = lines.get(issue.line.saturating_sub(1)) {
                    reported.push((raw.trim(), issue.kind.id()));
                }
            }
        }
    }
    let merged_lines: Vec<&str> = merged.lines().collect();
    let remaining: Vec<&HostsIssue> = parsed
        .issues
        .iter()
        .filter(|issue| {
            let Some(raw) = merged_lines.get(issue.line.saturating_sub(1)) else {
                return true;
            };
            !reported
                .iter()
                .any(|(line, kind)| *line == raw.trim() && *kind == issue.kind.id())
        })
        .collect();

    if remaining.is_empty() {
        report.push('\n');
        if parsed.issues.is_empty() {
            report.push_str(&text.get("检查通过：没有发现格式问题"));
        } else {
            report.push_str(&text.get("合并结果没有新增问题（上面逐方案已列出）"));
        }
        report.push('\n');
    } else {
        report.push('\n');
        if profiles.len() > 1 {
            report.push_str(&text.get("合并结果检查（跨方案冲突也在这里）"));
            report.push('\n');
        }
        report.push_str(&text.fill("检查发现 {} 处问题", &[&remaining.len().to_string()]));
        report.push('\n');
        for issue in remaining.iter().take(50) {
            report.push_str(&format!(
                "  {}  {}\n",
                text.fill("第 {} 行", &[&issue.line.to_string()]),
                issue_text(text, issue)
            ));
        }
        if remaining.len() > 50 {
            let rest = (remaining.len() - 50).to_string();
            report.push_str(&format!(
                "  {}\n",
                text.fill("（还有 {} 处未列出）", &[&rest])
            ));
        }
    }

    // ── 查找 / 替换（参考页「查找替换」标签页）────────────────────────
    //
    // 两段都由 `keyword` 触发：只给关键词＝查找，再给「替换为」＝替换。
    // 搜索范围是全部方案（**含停用的**）—— 停用只是不参与合并，不是从清单里消失。
    let keyword = params.text("keyword");
    if !keyword.is_empty() {
        let hits = search_profiles(&profiles, &keyword);
        report.push('\n');
        if hits.is_empty() {
            report.push_str(&text.fill("查找「{}」：没有命中", &[&keyword]));
            report.push('\n');
        } else {
            report.push_str(&text.fill(
                "查找「{}」：{} 处命中",
                &[&keyword, &hits.len().to_string()],
            ));
            report.push('\n');
            for hit in hits.iter().take(50) {
                report.push_str(&format!(
                    "  {}  {}  {}\n",
                    profiles[hit.profile].slug,
                    text.fill("第 {} 行", &[&hit.line.to_string()]),
                    hit.text
                ));
            }
            if hits.len() > 50 {
                let rest = (hits.len() - 50).to_string();
                report.push_str(&format!(
                    "  {}\n",
                    text.fill("（还有 {} 处未列出）", &[&rest])
                ));
            }
        }

        let replace_to = params.text("replace_to");
        if !replace_to.is_empty() {
            // 替换是**字面量**替换（`.` 不是通配符），与参考页一致。
            let outcomes = replace_in_profiles(&profiles, &keyword, &replace_to);
            let total: usize = outcomes.iter().map(|outcome| outcome.count).sum();
            report.push('\n');
            report.push_str(&text.fill(
                "替换「{}」→「{}」：共 {} 处",
                &[&keyword, &replace_to, &total.to_string()],
            ));
            report.push('\n');
            if outcomes.is_empty() {
                report.push_str(&text.get("没有方案命中，什么都没改"));
                report.push('\n');
            }
            for outcome in &outcomes {
                report.push_str(&format!(
                    "  {}  {}\n",
                    profiles[outcome.profile].slug,
                    text.fill("{} 处", &[&outcome.count.to_string()])
                ));
            }

            // 预览形态的落盘：写到另一个目录，绝不回头覆盖方案目录。
            let replace_out = params.text("replace_out");
            if !replace_out.is_empty() && !outcomes.is_empty() {
                write_replaced_profiles(&dir, &replace_out, &profiles, &outcomes, text)?;
                report.push('\n');
                report.push_str(&text.fill(
                    "已写出 {} 个方案文件到 {}（原目录未改动）",
                    &[&outcomes.len().to_string(), &replace_out],
                ));
                report.push('\n');
                report.push_str(
                    &text.get("（把这个目录回填到「方案目录」再跑一次，就能拿到替换后的合并结果）"),
                );
                report.push('\n');
            }
        }
    }

    // ── 方案对比（参考页「方案对比」标签页）──────────────────────────
    let diff_a = params.text("diff_a");
    let diff_b = params.text("diff_b");
    if !diff_a.is_empty() || !diff_b.is_empty() {
        let slugs: Vec<&str> = profiles
            .iter()
            .map(|profile| profile.slug.as_str())
            .collect();
        let (Some(left), Some(right)) = (
            find_profile(&profiles, &diff_a),
            find_profile(&profiles, &diff_b),
        ) else {
            return Err(duck_plugin::PluginError::Execution(
                text.fill("找不到要对比的方案（可用的是：{}）", &[&slugs.join(" / ")]),
            ));
        };
        let diff = diff_contents(&left.content, &right.content);
        report.push('\n');
        report.push_str(&text.fill("方案对比：{} ←→ {}", &[&left.slug, &right.slug]));
        report.push('\n');
        if diff.is_empty() {
            report.push_str(&text.get("两边生效行完全一致（注释与空行不参与）"));
            report.push('\n');
        }
        // 两段分开写，不抽成「标题 + 标记」的循环：两边用的是各自的方案名，
        // 循环体里还得再判断一次「这是哪一边」，反而更难读。
        if !diff.only_a.is_empty() {
            report.push_str(&format!("  {}\n", text.fill("仅 {} 有", &[&left.slug])));
            push_diff_lines(&mut report, &diff.only_a, "-", text);
        }
        if !diff.only_b.is_empty() {
            report.push_str(&format!("  {}\n", text.fill("仅 {} 有", &[&right.slug])));
            push_diff_lines(&mut report, &diff.only_b, "+", text);
        }
    }

    let out_path = params.text("out");
    if !out_path.is_empty() {
        validate_hosts_content(&merged).map_err(|error| fail(text, &error))?;
        let target = Path::new(&out_path);
        std::fs::write(target, &merged).map_err(|error| fail(text, &NetError::io(error)))?;
        report.push('\n');
        report.push_str(&text.fill(
            "已导出 {} 字节到 {}",
            &[&merged.len().to_string(), &out_path],
        ));
        report.push('\n');
    }

    if params.flag_or("with_script", true) {
        let file_name = match out_path.is_empty() {
            true => "hosts.merged".to_string(),
            false => Path::new(&out_path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("hosts.merged")
                .to_string(),
        };
        let script = build_apply_script(&file_name);
        report.push('\n');
        report.push_str(&text.get("写入系统（插件不碰 /etc/hosts，请自己执行）· macOS / Linux"));
        report.push('\n');
        report.push_str(&commands_text(text, &script.mac_linux));
        report.push('\n');
        report.push_str(&text.get("Windows（以管理员身份运行 CMD）"));
        report.push('\n');
        report.push_str(&commands_text(text, &script.windows));
    }

    // 差异对比一段：有导出并且系统 hosts 可读时，顺便说清「会变什么」。
    //
    // 方向容易搞反：`diff_contents(a = 系统, b = 合并结果)` 的 `only_a` 是
    // **系统独有**（覆盖后会消失的），`only_b` 是**合并结果独有**（会被加进去的）。
    let system_path = system_hosts_path();
    if let Ok(current) = std::fs::read_to_string(&system_path) {
        let diff = diff_contents(&current, &merged);
        report.push('\n');
        if diff.is_empty() {
            report.push_str(&text.get("与当前系统 hosts 相比：没有变化"));
            report.push('\n');
        } else {
            report.push_str(&text.fill(
                "与当前系统 hosts 相比：本工具多 {} 行，系统多 {} 行",
                &[
                    &diff.only_b.len().to_string(),
                    &diff.only_a.len().to_string(),
                ],
            ));
            report.push('\n');
            if diff.only_a.is_empty() {
                report.push_str(&text.get("（系统 hosts 没有本工具之外的行，覆盖是安全的）"));
                report.push('\n');
            } else {
                report.push_str(
                    &text.get(
                        "（下面这些是系统里有、合并结果里没有的：覆盖后会消失，确认不用保留）",
                    ),
                );
                report.push('\n');
                for line in diff.only_a.iter().take(10) {
                    report.push_str(&format!("  - {line}\n"));
                }
                if diff.only_a.len() > 10 {
                    let rest = (diff.only_a.len() - 10).to_string();
                    report.push_str(&format!(
                        "  {}\n",
                        text.fill("（还有 {} 行未列出）", &[&rest])
                    ));
                }
            }
        }
    }

    Ok(report)
}

/// 差异行列表：最多 50 条，多出来的折成一句「还有 N 行未列出」。
fn push_diff_lines(report: &mut String, lines: &[String], mark: &str, text: &Text) {
    for line in lines.iter().take(50) {
        report.push_str(&format!("    {mark} {line}\n"));
    }
    if lines.len() > 50 {
        let rest = (lines.len() - 50).to_string();
        report.push_str(&format!(
            "    {}\n",
            text.fill("（还有 {} 行未列出）", &[&rest])
        ));
    }
}

/// 把替换结果写到一个**独立目录**（绝不覆盖方案目录）。
///
/// 参考页是「就地改 localStorage」；插件没有可改的内存副本，而直接回写方案文件
/// 属于「一次点运行就改掉用户五个文件」，风险与收益不成比例。写到旁边一个目录，
/// 用户 `diff -r` 看过再决定要不要搬回去 —— 代价只是多一次 `cp`。
fn write_replaced_profiles(
    source_dir: &str,
    target_dir: &str,
    profiles: &[HostsProfile],
    outcomes: &[ReplaceOutcome],
    text: &Text,
) -> Result<(), duck_plugin::PluginError> {
    let source = std::fs::canonicalize(source_dir).unwrap_or_else(|_| PathBuf::from(source_dir));
    let target = PathBuf::from(target_dir);
    // 同一目录＝原地改，直接拒绝：这是唯一一条会造成数据丢失的路径。
    if target.exists() {
        let resolved = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        if resolved == source {
            return Err(duck_plugin::PluginError::Execution(text.get(
                "「替换结果目录」不能和方案目录是同一个：那样会直接改掉原文件",
            )));
        }
    }
    std::fs::create_dir_all(&target).map_err(|error| fail(text, &NetError::io(error)))?;
    for outcome in outcomes {
        let profile = &profiles[outcome.profile];
        let path = target.join(format!("{}.hosts", profile.slug));
        std::fs::write(&path, &outcome.content)
            .map_err(|error| fail(text, &NetError::io(error)))?;
    }
    Ok(())
}

/// 方案目录里的文件（按文件名排序，保证合并顺序稳定）。
fn profile_files(dir: &Path) -> Result<Vec<PathBuf>, NetError> {
    if !dir.is_dir() {
        return Err(NetError::with_detail(ErrorKind::PathMissing, dir.display()));
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(NetError::io)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let keep = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "hosts" | "txt"))
            .unwrap_or(false);
        if keep {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn system_hosts_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/etc/hosts")
    }
}

/// 应用命令 → 带本地化注释的文本（注释来自译文表，命令原样输出）。
fn commands_text(text: &Text, commands: &[ApplyCommand]) -> String {
    let mut out = String::new();
    for (index, command) in commands.iter().enumerate() {
        out.push_str(&format!(
            "  # {} {}\n",
            index + 1,
            text.get(command_comment(command.id))
        ));
        out.push_str(&format!("  {}\n", command.command));
    }
    out
}

fn command_comment(id: &str) -> &'static str {
    match id {
        "backup" => "先备份当前系统 hosts",
        "apply" => "把合并结果覆盖到系统 hosts",
        _ => "刷新 DNS 缓存",
    }
}

/// `IssueKind` → 一句话（四个变体逐一对应）。
fn issue_text(text: &Text, issue: &HostsIssue) -> String {
    let body = match &issue.kind {
        IssueKind::MissingHostname => text.get("缺少主机名：格式应为「IP 主机名 …」"),
        IssueKind::BadIp(ip) => text.fill("非法 IP：{}", &[ip]),
        IssueKind::SuspiciousHostname(host) => text.fill("可疑主机名：{}", &[host]),
        IssueKind::DuplicateDomain { host, first_line } => text.fill(
            "域名重复：{}（第一次出现在第 {} 行）",
            &[host, &first_line.to_string()],
        ),
    };
    match issue.level {
        IssueLevel::Error => format!("{} {body}", text.get("错误")),
        IssueLevel::Warn => format!("{} {body}", text.get("警告")),
    }
}

// ── S3 对象存储 ────────────────────────────────────────────────────────

/// 一个动作一次运行：连接参数与动作参数都从同一张表单来。
///
/// 参考仓是「左侧列桶 / 中间列对象 / 右侧传输队列」的三栏界面，本仓执行页是
/// 声明式单表单（ADR-1），所以这里按**动作**组织：一次跑一个动作，结果是一份
/// 可复制的报告。这与 `duck.files.*` 族的做法一致，也让「删除/覆盖」这类破坏性
/// 动作天然要求一次明确的选择。
fn s3_run(
    request: &RunRequest,
    out: &mut PluginOutput<std::io::Stdout>,
    text: &Text,
) -> Result<String, duck_plugin::PluginError> {
    let params = Params::new(&request.params);
    let config = S3Config {
        endpoint: params.text("endpoint"),
        region: params.text("region"),
        access_key: params.text("access_key"),
        secret_key: params.text("secret_key"),
        path_style: params.flag("path_style"),
        insecure: params.flag("insecure"),
        timeout_ms: params.number("timeout_ms", 30_000) as u64,
    };
    let client = Client::new(config).map_err(|error| fail(text, &error))?;
    let action = params.text("action");
    let bucket = params.text("bucket");
    let prefix = params.text("prefix");
    // 对象 Key 是**底部输入框**（`[io].fields`），不在 params 里：它是每次运行
    // 都要变的那一项，位置就该是主输入区（布局铁律：输入框沉底）。
    let key = match request.input.trim() {
        "" => params.text("key"),
        given => given.to_owned(),
    };
    let full_key = join_key(&prefix, &key);
    let endpoint = client.config().endpoint.clone();

    match action.as_str() {
        "buckets" => {
            out.log(text.get("列出存储桶"));
            let started = std::time::Instant::now();
            let buckets = client.list_buckets().map_err(|error| fail(text, &error))?;
            let mut rows = vec![vec![text.get("名称"), text.get("创建时间")]];
            for bucket in &buckets {
                rows.push(vec![bucket.name.clone(), timestamp(bucket.creation_date)]);
            }
            let mut report = heading(text, &endpoint, started);
            report.push_str(&text.fill("共 {} 个存储桶", &[&buckets.len().to_string()]));
            report.push_str("\n\n");
            report.push_str(&table(&rows));
            Ok(report)
        }
        "list" => {
            out.log(text.fill("列出对象 {}", &[&format!("{endpoint}/{bucket}")]));
            let started = std::time::Instant::now();
            let page = client
                .list_objects(&bucket, &prefix, "/", "")
                .map_err(|error| fail(text, &error))?;
            let mut rows = vec![vec![
                text.get("名称"),
                text.get("大小"),
                text.get("修改时间"),
                text.get("ETag"),
            ]];
            let mut directories = 0;
            let mut files = 0;
            for object in &page.objects {
                match object.is_prefix {
                    true => {
                        directories += 1;
                        // 目录加尾斜杠，和 `ls` 的观感一致。
                        rows.push(vec![
                            format!("{}/", object.key),
                            String::new(),
                            String::new(),
                            String::new(),
                        ]);
                    }
                    false => {
                        files += 1;
                        rows.push(vec![
                            object.key.clone(),
                            format_size(object.size),
                            timestamp(object.last_modified),
                            object.etag.clone(),
                        ]);
                    }
                }
            }
            let mut report = heading(text, &endpoint, started);
            report.push_str(&text.fill(
                "{} · 共 {} 个目录 / {} 个对象（前缀 {}）",
                &[
                    &bucket,
                    &directories.to_string(),
                    &files.to_string(),
                    &(match prefix.is_empty() {
                        true => "/".to_string(),
                        false => prefix.clone(),
                    }),
                ],
            ));
            report.push_str("\n\n");
            if page.objects.is_empty() {
                report.push_str(&text.get("这里没有对象"));
                report.push('\n');
            } else {
                report.push_str(&table(&rows));
            }
            if page.truncated {
                // 单页上限 1000 条（S3 的硬上限）。不静默截断：用户看到这句
                // 才知道该往 `prefix` 里填更细的路径。
                report.push('\n');
                report.push_str(&text.get("超过一页（1000 条），请用「前缀」缩小范围"));
                report.push('\n');
            }
            Ok(report)
        }
        "create-bucket" => {
            out.log(text.fill("新建存储桶 {}", &[&bucket]));
            let started = std::time::Instant::now();
            client
                .create_bucket(&bucket)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill("已创建 {}", &[&bucket]),
            ))
        }
        "delete-bucket" => {
            out.log(text.fill("删除存储桶 {}", &[&bucket]));
            let started = std::time::Instant::now();
            client
                .delete_bucket(&bucket)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill("已删除 {}", &[&bucket]),
            ))
        }
        "mkdir" => {
            let folder = params.text("folder_name");
            let target = join_key(&prefix, &folder);
            out.log(text.fill("新建目录 {}", &[&target]));
            let started = std::time::Instant::now();
            client
                .create_folder(&bucket, &target)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill("已创建目录 {}/", &[target.trim_end_matches('/')]),
            ))
        }
        "upload" => {
            let local = params.text("local_file");
            if local.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            let path = Path::new(&local);
            // key 留空就用文件名；前缀会自动拼上（见 [`join_key`]）。
            let name = match key.is_empty() {
                true => path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                false => key,
            };
            let target = join_key(&prefix, &name);
            out.log(text.fill("上传 {} → {}", &[&local, &target]));
            let started = std::time::Instant::now();
            let bytes = client
                .put_file(&bucket, &target, path)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill("已上传 {}（{}）", &[&target, &format_size(bytes)]),
            ))
        }
        "download" => {
            let directory = params.text("local_dir");
            if full_key.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            let name = full_key
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("download");
            let target = match directory.is_empty() {
                true => PathBuf::from(name),
                false => Path::new(&directory).join(name),
            };
            out.log(text.fill("下载 {} → {}", &[&full_key, &target.display().to_string()]));
            let started = std::time::Instant::now();
            let bytes = client
                .get_to_file(&bucket, &full_key, &target)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill(
                    "已下载 {}（{}）",
                    &[&target.display().to_string(), &format_size(bytes)],
                ),
            ))
        }
        "copy" => {
            let dest = params.text("dest_key");
            if full_key.is_empty() || dest.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            let target = join_key(&prefix, &dest);
            let remove_source = params.flag("remove_source");
            let started = std::time::Instant::now();
            match remove_source {
                true => {
                    out.log(text.fill("改名 {} → {}", &[&full_key, &target]));
                    client
                        .rename_object(&bucket, &full_key, &target)
                        .map_err(|error| fail(text, &error))?;
                    Ok(done(
                        text,
                        &endpoint,
                        started,
                        &text.fill("已改名 {} → {}", &[&full_key, &target]),
                    ))
                }
                false => {
                    out.log(text.fill("复制 {} → {}", &[&full_key, &target]));
                    client
                        .copy_object(&bucket, &full_key, &target)
                        .map_err(|error| fail(text, &error))?;
                    Ok(done(
                        text,
                        &endpoint,
                        started,
                        &text.fill("已复制 {} → {}", &[&full_key, &target]),
                    ))
                }
            }
        }
        "delete-object" => {
            if full_key.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            out.log(text.fill("删除对象 {}", &[&full_key]));
            let started = std::time::Instant::now();
            client
                .delete_object(&bucket, &full_key)
                .map_err(|error| fail(text, &error))?;
            Ok(done(
                text,
                &endpoint,
                started,
                &text.fill("已删除 {}", &[&full_key]),
            ))
        }
        "preview" => {
            if full_key.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            out.log(text.fill("读取对象 {}", &[&full_key]));
            let started = std::time::Instant::now();
            let body = client
                .object_text(&bucket, &full_key)
                .map_err(|error| fail(text, &error))?;
            let mut report = heading(text, &endpoint, started);
            report.push_str(&format!("{bucket}/{full_key}\n"));
            report.push_str(&text.fill("共 {} 字节", &[&body.len().to_string()]));
            report.push_str("\n\n");
            // 只放一段：完整内容要原样看到请用「下载」。截断必须明说，
            // 不能让用户以为对象就这么长。
            match body.chars().count() > MAX_PREVIEW_CHARS {
                true => {
                    let head: String = body.chars().take(MAX_PREVIEW_CHARS).collect();
                    report.push_str(&head);
                    report.push_str("\n\n");
                    report.push_str(&text.fill(
                        "（内容过长，只显示前 {} 个字符；完整内容请用「下载」）",
                        &[&MAX_PREVIEW_CHARS.to_string()],
                    ));
                    report.push('\n');
                }
                false => {
                    report.push_str(&body);
                    report.push('\n');
                }
            }
            Ok(report)
        }
        "url" => {
            if full_key.is_empty() {
                return Err(fail(text, &NetError::new(ErrorKind::EmptyInput)));
            }
            let url = client.object_url(&bucket, &full_key);
            let mut report = heading(text, &endpoint, std::time::Instant::now());
            report.push_str(&text.get("对象地址"));
            report.push_str("\n\n");
            report.push_str(&url);
            report.push('\n');
            report.push('\n');
            // 说清楚这不是预签名地址：私有桶里它打开就是 403，用户不该白试一次。
            report.push_str(&text.get("这是对象地址，不是预签名地址：私有桶直接打开会返回 403"));
            report.push('\n');
            Ok(report)
        }
        other => Err(duck_plugin::PluginError::Execution(format!(
            "duck.net.s3-manager: unknown action {other}"
        ))),
    }
}

/// 预览动作的字符上限（一屏能看完的量级）。
const MAX_PREVIEW_CHARS: usize = 20_000;

/// 报告的抬头：`S3 · <endpoint> · 耗时 123 ms`。
fn heading(text: &Text, endpoint: &str, started: std::time::Instant) -> String {
    let mut out = text.fill(
        "{} · {} · 耗时 {} ms",
        &[
            &text.get("目标"),
            &format!("S3 {endpoint}"),
            &started.elapsed().as_millis().to_string(),
        ],
    );
    out.push('\n');
    out
}

/// 单行结论式报告（成功类动作用）。
fn done(text: &Text, endpoint: &str, started: std::time::Instant, line: &str) -> String {
    let mut report = heading(text, endpoint, started);
    report.push('\n');
    report.push_str(line);
    report.push('\n');
    report
}

/// `prefix` + `key` → 完整 key。
///
/// 三条规则（都是为了让用户少打字）：
/// 1. `prefix` 空 → 就是 `key`；
/// 2. `key` 已经以 `prefix` 开头 → 原样用（避免 `a/a/b.txt`）；
/// 3. 其余情况 → `prefix/` + `key`。
fn join_key(prefix: &str, key: &str) -> String {
    let key = key.trim().trim_start_matches('/');
    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return key.to_string();
    }
    if key.is_empty() {
        return format!("{prefix}/");
    }
    if key.starts_with(prefix) {
        return key.to_string();
    }
    format!("{prefix}/{key}")
}

/// Unix 秒 → `YYYY-MM-DD HH:MM`（本地时区）；0 显示 `-`（S3 没给这个字段）。
fn timestamp(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_string();
    }
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|stamp| {
            stamp
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// 等宽表格：按**显示宽度**对齐（中文算两列）。
///
/// 第一行当表头。末列不补空格（行尾空白在复制粘贴时很烦）。
fn table(rows: &[Vec<String>]) -> String {
    let Some(first) = rows.first() else {
        return String::new();
    };
    let columns = first.len();
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            widths[index] = widths[index].max(display_width(cell));
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate().take(columns) {
            line.push_str(cell);
            if index + 1 < columns {
                let pad = widths[index].saturating_sub(display_width(cell)) + 2;
                line.push_str(&" ".repeat(pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 显示宽度：东亚宽字符占两列（否则中英混排的表格会歪）。
fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD
    )
}
