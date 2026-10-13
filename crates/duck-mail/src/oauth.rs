//! OAuth2（Gmail / Outlook）内核层。
//!
//! 本模块只做**协议件**：授权码 + PKCE 的参数构造、回调解析、令牌交换 / 刷新、
//! 以及把访问令牌变成 IMAP / SMTP 能用的 XOAUTH2 初始响应。**不含**浏览器交互与
//! 本地回环服务器 —— 那两者属宿主（`duck-app` 注入 `open_url`）与本模块之上的一层，
//! 见模块末的「未做」清单。
//!
//! # 三条硬口径
//!
//! 1. **令牌不进 SQLite**：本模块只把令牌交给调用方，落盘由凭据层（`duck-platform::secret`）
//!    负责，与口令同源。本模块自己不碰任何持久化。
//! 2. **错误详情一律英文**：本 crate 没有 EN 表可登记，
//!    `duck-theme::i18n::every_chinese_literal_is_translated` 会扫本文件的字符串字面量；
//!    中文只出现在注释里。
//! 3. **令牌与授权码不做任何拼接进 URL**：授权 URL 的每个值都过 [`urlencode`]；
//!    表单提交交给 `ureq::send_form`（它自己按 `application/x-www-form-urlencoded` 编码）。
//!    绝不把用户数据拼进 SQL 或命令行。
//!
//! # XOAUTH2
//!
//! SASL 初始响应是明文形式 `user=<邮箱>^Aauth=Bearer <令牌>^A^A`（`^A` = `U+0001`）：
//! IMAP 交给 `async-imap` 的 `Authenticator`（它**自己**做 base64），SMTP 由本模块的
//! [`sasl_xoauth2_base64`] 直接给出 base64 串。两者的原文必须一致，所以只有一个真源。
//!
//! # 未做（不在本模块）
//!
//! - 本地回环 HTTP 服务器（承接重定向）与浏览器打开 —— 前者属连接编排层，
//!   后者是宿主回调（`duck-app` 注入），本 crate 保持零 GPUI / 零 rfd。
//! - 凭据包（刷新令牌 + client_id）的持久化与「建连时自动刷新」——
//!   需要 [`crate::accounts`] 的凭据层扩展，属另一批。
//! - 增量授权（额外 scope，如日历 / Teams）。

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{MailError, Result};
use crate::models::Provider;

/// 令牌端点的时间预算。
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// 令牌端点的建连超时。
const TOKEN_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 令牌响应体的上限（挡住错误的重定向把巨大 body 读进来）。
const MAX_TOKEN_BODY: u64 = 1024 * 1024;
/// 单个令牌字段的字节上限。
const MAX_TOKEN_BYTES: usize = 256 * 1024;
/// 令牌寿命的合理上限（一年），超出的按协议错误处理。
const MAX_TOKEN_LIFETIME_SECONDS: i64 = 366 * 24 * 60 * 60;

const USER_AGENT: &str = concat!("DuckToolbox/", env!("CARGO_PKG_VERSION"), " (mail-oauth)");

/// PKCE verifier 的字符集（RFC 7636 §4.1 的 unreserved 集）。
const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// 拒绝采样的接受上界：`256 % 66 = 58`，直接取模会让前 58 个字符多出现一次。
/// 取 `66 * 3 = 198` 作接受区间，落在里面的字节 `% 66` 恰好每个下标对应 3 个字节值。
const SAMPLE_ACCEPT: u16 = (VERIFIER_CHARS.len() as u16) * 3;

// ---------------------------------------------------------------------------
// 服务商端点
// ---------------------------------------------------------------------------

/// 一个 OAuth2 服务商的端点与默认 scope。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoints {
    /// 授权端点。
    pub auth_url: &'static str,
    /// 令牌端点（交换与刷新共用）。
    pub token_url: &'static str,
    /// 默认请求的 scope。
    pub scopes: &'static [&'static str],
}

/// Google / Gmail。
///
/// scope 用 `https://mail.google.com/`（全量邮件）：Gmail 的「永久删除」接口要求它，
/// 只给 `gmail.modify` 的旧授权在删除时会失败。要改这里得连着看 `email-design.md` §4.12。
pub const GOOGLE: Endpoints = Endpoints {
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &["https://mail.google.com/", "openid", "email", "profile"],
};

/// Microsoft / Outlook。
///
/// `offline_access` 是**必须**的：没有它拿不到刷新令牌，账户只能活一小时。
/// scope 全部指向 `outlook.office.com`：Microsoft 发的是**单资源**令牌，混进
/// `graph.microsoft.com` 的 scope 会让 IMAP / SMTP 拿到的令牌受众不对。
pub const MICROSOFT: Endpoints = Endpoints {
    auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scopes: &[
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
        "offline_access",
        "openid",
        "profile",
        "email",
    ],
};

/// 某服务商的 OAuth2 端点；不用 OAuth2 的服务商（[`Provider::Imap`]）返回 `None`。
#[must_use]
pub fn endpoints(provider: Provider) -> Option<&'static Endpoints> {
    match provider {
        Provider::Gmail => Some(&GOOGLE),
        Provider::Microsoft => Some(&MICROSOFT),
        Provider::Imap => None,
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// 一次 PKCE（RFC 7636）的 verifier 与 S256 challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// 43–128 字符的随机串（我们固定生成 64）。
    pub verifier: String,
    /// `BASE64URL-NOPAD(SHA256(verifier))`。
    pub challenge: String,
}

impl Pkce {
    /// 生成一个新的 verifier（64 字符）。
    ///
    /// # Errors
    /// 系统随机源不可用时返回 [`MailError::Auth`]。
    pub fn generate() -> Result<Self> {
        Ok(Self::from_verifier(random_string(64)?))
    }

    /// 由一个已有的 verifier 算出 challenge。
    ///
    /// 单独拆出来是为了能对着 RFC 7636 附录 B 的固定向量做断言 ——
    /// `generate` 是随机的，测不了「算得对不对」。
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref());
        Self {
            verifier,
            challenge,
        }
    }
}

/// 生成一个 `len` 字符的随机串（PKCE verifier / state / nonce 共用）。
///
/// 用**拒绝采样**而不是取模：`VERIFIER_CHARS.len()` 是 66，`u8 % 66` 会让前 58 个字符
/// 出现得更频繁，对 state 这种防 CSRF 的用途是实打实的强度损失。接受区间取 `66 * 3 = 198`，
/// 保证落在里面的字节取模后是均匀的。
///
/// # Errors
/// 系统随机源不可用时返回 [`MailError::Auth`]。
pub fn random_string(len: usize) -> Result<String> {
    use ring::rand::SecureRandom as _;

    let rng = ring::rand::SystemRandom::new();
    let mut out = String::with_capacity(len);
    let mut byte = [0u8; 1];
    while out.len() < len {
        rng.fill(&mut byte)
            .map_err(|_| MailError::Auth("could not read random bytes".into()))?;
        if let Some(index) = sample_index(byte[0]) {
            out.push(VERIFIER_CHARS[index] as char);
        }
    }
    Ok(out)
}

/// 单个随机字节 → 字符下标；落在拒绝区间（`>= `[`SAMPLE_ACCEPT`]）返回 `None`，调用方重抽。
///
/// 把映射单独抽出来，是为了让「拒绝采样是否真的覆盖全部 66 个字符、且每个等权」这件事能
/// **确定性地**断言（遍历 256 个字节值即可）。⚠️ 别退回去用「抽 N 次随机串数碰到几个字符」
/// 那套：那是概率断言——512 次采样漏掉某个字符的概率约 3%，会让测试偶发假红。
fn sample_index(byte: u8) -> Option<usize> {
    let value = u16::from(byte);
    if value < SAMPLE_ACCEPT {
        Some((value % VERIFIER_CHARS.len() as u16) as usize)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 授权 URL
// ---------------------------------------------------------------------------

/// 构造授权端点 URL（用户浏览器要打开的那一条）。
///
/// scope 取 [`Endpoints::scopes`]；`login_hint` 是「登录哪个账户」的提示。
///
/// # Errors
/// 服务商不用 OAuth2（[`Provider::Imap`]）时返回 [`MailError::Unsupported`]。
pub fn authorization_url(
    provider: Provider,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    challenge: &str,
    login_hint: Option<&str>,
) -> Result<String> {
    let cfg = endpoints(provider).ok_or_else(|| {
        MailError::Unsupported(format!(
            "provider {} does not use oauth2",
            provider.wire_name()
        ))
    })?;
    let scopes = cfg.scopes.join(" ");
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        cfg.auth_url,
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(&scopes),
        urlencode(state),
        urlencode(nonce),
        urlencode(challenge),
    );
    if provider == Provider::Gmail {
        // `access_type=offline` 才发刷新令牌；`prompt=consent` 保证**每次**都重新发一份
        // （Google 只在首次授权给刷新令牌，重复登录而不带它就拿不到）。
        url.push_str("&access_type=offline&prompt=consent");
    }
    if let Some(hint) = login_hint.filter(|hint| !hint.trim().is_empty()) {
        url.push_str("&login_hint=");
        url.push_str(&urlencode(hint.trim()));
    } else if provider == Provider::Microsoft {
        // 没有提示时，活跃的 Microsoft SSO 会话会**静默**登录成它的主人，
        // 用户根本看不到登录页。强制出账户选择器，让「以谁的身份登录」是个显式选择。
        url.push_str("&prompt=select_account");
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// 回调解析
// ---------------------------------------------------------------------------

/// 从重定向里取出的授权结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// 授权码。
    pub code: String,
    /// 回显的 `state`（用于与发起时的值比对，防 CSRF）。
    pub state: Option<String>,
}

/// 解析重定向的 query 串（可以带前导 `?`）。
///
/// # Errors
/// - 服务商回了 `error=`：返回 [`MailError::Auth`]（带 `error_description`）；
/// - 没有 `code`：返回 [`MailError::Auth`]。
pub fn parse_redirect(query: &str) -> Result<Redirect> {
    let query = query.strip_prefix('?').unwrap_or(query);
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut error: Option<String> = None;
    let mut description: Option<String> = None;

    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        match key {
            "code" => code = Some(form_decode(value)),
            "state" => state = Some(form_decode(value)),
            "error" => error = Some(form_decode(value)),
            "error_description" => description = Some(form_decode(value)),
            _ => {}
        }
    }

    if let Some(error) = error {
        let detail = description.unwrap_or_default();
        return Err(MailError::Auth(format!(
            "authorization was refused: {error}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            }
        )));
    }
    let code = code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| MailError::Auth("redirect carried no authorization code".into()))?;
    Ok(Redirect { code, state })
}

// ---------------------------------------------------------------------------
// 令牌
// ---------------------------------------------------------------------------

/// 令牌端点返回的一组令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// 访问令牌（进 `ImapConfig` / `SmtpConfig` 当口令用）。
    pub access_token: String,
    /// 刷新令牌。Google 只在首次授权给；缺它意味着「下次登录前令牌一过期就掉线」。
    pub refresh_token: Option<String>,
    /// 寿命（秒）。缺省按 3600 处理。
    pub expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// 解析令牌端点的响应体。
///
/// # Errors
/// - 服务端给了 `error`：`invalid_grant` / `interaction_required` / `consent_required`
///   映射到 [`MailError::NeedsReauth`]（要用户重新授权），`temporarily_unavailable` /
///   `server_error` 映射到 [`MailError::Connect`]（可重试），其余是 [`MailError::Auth`]；
/// - 不是 JSON、缺 `access_token`、或字段越界：返回 [`MailError::Auth`]。
pub fn parse_token_response(body: &str) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body).map_err(|_| {
        MailError::Auth(format!(
            "token endpoint returned a non-JSON response: {}",
            excerpt(body)
        ))
    })?;

    if let Some(error) = value.get("error").and_then(Value::as_str) {
        let description = value
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(classify_token_error(error, description));
    }

    let parsed: TokenResponse = serde_json::from_value(value)
        .map_err(|_| MailError::Auth("token endpoint returned an unexpected payload".into()))?;
    let access_token = parsed.access_token.unwrap_or_default();
    validate_token(&access_token)?;
    if let Some(refresh) = parsed.refresh_token.as_deref() {
        validate_token(refresh)?;
    }
    // `expires_in` 若给定就必须落在合理区间（见 [`MAX_TOKEN_LIFETIME_SECONDS`]）。
    // 用 `filter` 而非嵌套 `if let`（本项目 MSRV 1.85 没有 let-chain）。
    if let Some(seconds) = parsed
        .expires_in
        .filter(|seconds| !(1..=MAX_TOKEN_LIFETIME_SECONDS).contains(seconds))
    {
        return Err(MailError::Auth(format!(
            "the provider returned an out-of-range token lifetime: {seconds}"
        )));
    }
    Ok(TokenSet {
        access_token,
        refresh_token: parsed.refresh_token,
        expires_in: parsed.expires_in,
    })
}

/// 把令牌端点的 `error` 归类成「要重新授权 / 可重试 / 其他」。
fn classify_token_error(error: &str, description: &str) -> MailError {
    let lower = error.to_ascii_lowercase();
    if [
        "invalid_grant",
        "interaction_required",
        "consent_required",
        "login_required",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return MailError::NeedsReauth;
    }
    if ["temporarily_unavailable", "server_error"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return MailError::Connect(format!("the oauth token service is unavailable: {error}"));
    }
    let detail = if description.is_empty() {
        error.to_owned()
    } else {
        format!("{error} ({description})")
    };
    MailError::Auth(format!("the oauth token request was rejected: {detail}"))
}

/// 令牌字段的形状检查：非空、不是控制字符、不超长。
fn validate_token(token: &str) -> Result<()> {
    let ok =
        !token.is_empty() && token.len() <= MAX_TOKEN_BYTES && !token.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(MailError::Auth(
            "the provider returned a malformed token".into(),
        ))
    }
}

/// 授权码交换的表单（`grant_type=authorization_code`）。
#[must_use]
pub fn exchange_form(
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    verifier: &str,
    client_secret: Option<&str>,
) -> Vec<(String, String)> {
    let mut form = vec![
        ("grant_type".to_owned(), "authorization_code".to_owned()),
        ("code".to_owned(), code.to_owned()),
        ("redirect_uri".to_owned(), redirect_uri.to_owned()),
        ("client_id".to_owned(), client_id.to_owned()),
        ("code_verifier".to_owned(), verifier.to_owned()),
    ];
    if let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) {
        form.push(("client_secret".to_owned(), secret.to_owned()));
    }
    form
}

/// 刷新令牌的表单（`grant_type=refresh_token`）。
///
/// `scope` 只在 **Microsoft** 上需要显式给（它的刷新令牌可能同时覆盖 Outlook 与 Graph；
/// 不指定的话拿回来的访问令牌受众不一定对得上 IMAP / SMTP）。
#[must_use]
pub fn refresh_form(
    refresh_token: &str,
    client_id: &str,
    client_secret: Option<&str>,
    scope: Option<&str>,
) -> Vec<(String, String)> {
    let mut form = vec![
        ("grant_type".to_owned(), "refresh_token".to_owned()),
        ("refresh_token".to_owned(), refresh_token.to_owned()),
        ("client_id".to_owned(), client_id.to_owned()),
    ];
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        form.push(("scope".to_owned(), scope.to_owned()));
    }
    if let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) {
        form.push(("client_secret".to_owned(), secret.to_owned()));
    }
    form
}

/// 用授权码换令牌（阻塞式）。
///
/// 阻塞而非异步：调用点都在后台执行器里（与 `duck-db` / `duck-update` 同一取舍），
/// 为一个表单 POST 牵进异步运行时是错的取舍。
///
/// # Errors
/// 服务商不用 OAuth2、网络失败、响应非法都返回对应 [`MailError`]。
pub fn exchange_code(
    provider: Provider,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenSet> {
    let cfg = endpoints(provider).ok_or_else(|| {
        MailError::Unsupported(format!(
            "provider {} does not use oauth2",
            provider.wire_name()
        ))
    })?;
    let form = exchange_form(code, redirect_uri, client_id, verifier, client_secret);
    let body = post_token_form(cfg.token_url, &form)?;
    parse_token_response(&body)
}

/// 用刷新令牌换一个新的访问令牌（阻塞式）。
///
/// # Errors
/// 同 [`exchange_code`]；刷新令牌失效（`invalid_grant`）返回 [`MailError::NeedsReauth`]。
pub fn refresh_access_token(
    provider: Provider,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<TokenSet> {
    let cfg = endpoints(provider).ok_or_else(|| {
        MailError::Unsupported(format!(
            "provider {} does not use oauth2",
            provider.wire_name()
        ))
    })?;
    // Microsoft 的刷新要显式声明 scope，否则拿回的令牌受众可能不是 IMAP / SMTP 要的。
    let scope = (provider == Provider::Microsoft).then(|| cfg.scopes.join(" "));
    let form = refresh_form(refresh_token, client_id, client_secret, scope.as_deref());
    let body = post_token_form(cfg.token_url, &form)?;
    parse_token_response(&body)
}

/// 一个受限的 `application/x-www-form-urlencoded` POST，返回响应体原文。
///
/// 只走 HTTPS、只跟随**不**重定向（`https_only` 让重定向也逃不掉）、有全局超时与
/// 体积上限。**不按状态码提前失败**：OAuth2 的令牌端点把错误写在 body 的 JSON 里，
/// 状态码可能是 400 —— 把 body 交给 [`parse_token_response`] 才能给出可区分的归因。
///
/// # Errors
/// 非 HTTPS 地址、网络失败、响应读不出来都返回 [`MailError`]。
pub fn post_token_form(url: &str, form: &[(String, String)]) -> Result<String> {
    if !url.starts_with("https://") {
        return Err(MailError::Config(format!(
            "the oauth token endpoint must be https: {url}"
        )));
    }
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(TOKEN_TIMEOUT))
        .timeout_connect(Some(TOKEN_CONNECT_TIMEOUT))
        .user_agent(USER_AGENT)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut response = agent
        .post(url)
        .send_form(
            form.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .map_err(|error| MailError::Connect(format!("oauth token request failed: {error}")))?;

    let status = response.status();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_TOKEN_BODY)
        .read_to_string()
        .map_err(|error| {
            MailError::Protocol(format!("oauth token response unreadable: {error}"))
        })?;

    // 5xx 与 429 是「服务端的问题」，重试有意义；4xx 交给 body 里的错误码分类
    // （`invalid_grant` 要的是重新授权，不是重试）。
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(MailError::Connect(format!(
            "the oauth token service returned {status}"
        )));
    }
    Ok(body)
}

// ---------------------------------------------------------------------------
// XOAUTH2
// ---------------------------------------------------------------------------

/// XOAUTH2 的 SASL 初始响应（**未** base64）。
///
/// IMAP 用这一份：`async-imap` 的 `Authenticator` 会自己编码。
#[must_use]
pub fn sasl_xoauth2(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// XOAUTH2 的 SASL 初始响应（base64）。
///
/// SMTP 用这一份：`AUTH XOAUTH2 <token>` 直接钉在命令行上。
#[must_use]
pub fn sasl_xoauth2_base64(user: &str, access_token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(sasl_xoauth2(user, access_token))
}

// ---------------------------------------------------------------------------
// 编码辅助
// ---------------------------------------------------------------------------

/// RFC 3986 的 unreserved 集之外全部百分号编码。
///
/// 自己写而不是借 `ureq` 的：授权 URL 是**拼**出来的（`ureq` 只负责表单 body 的编码），
/// 两处口径必须同源，所以这里留一份可单测的实现。
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 表单值的百分号解码（`+` 视作空格，`%XX` 视作字节）。
///
/// 非法转义**原样保留**：回调串是我们自己从浏览器拿到的，一个写坏的百分号不该让
/// 整次授权失败 —— 保留原文至少能让 `state` 不匹配被报出来。
fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把可能很长的响应体压成一行短摘要（进错误信息用）。
fn excerpt(body: &str) -> String {
    let single = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text: String = single.chars().take(200).collect();
    if single.chars().count() > 200 {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PKCE ----

    #[test]
    fn s256_challenge_matches_the_rfc7636_vector() {
        // RFC 7636 附录 B 的固定向量（外部权威值，不是手推的）。
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_in_the_allowed_charset_and_length() {
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.verifier.len(), 64);
        assert!(
            pkce.verifier
                .bytes()
                .all(|byte| VERIFIER_CHARS.contains(&byte))
        );
        // challenge 是 base64url-nopad 的 sha256：43 字符、无 padding。
        assert_eq!(pkce.challenge.len(), 43);
        assert!(!pkce.challenge.contains('='));
        // 两次生成不该撞（随机源真在工作）。
        assert_ne!(pkce.verifier, Pkce::generate().unwrap().verifier);
    }

    #[test]
    fn random_string_has_the_requested_length_and_charset() {
        let value = random_string(512).unwrap();
        assert_eq!(value.len(), 512);
        assert!(value.bytes().all(|byte| VERIFIER_CHARS.contains(&byte)));
        // 两次取样不该撞（随机源真在工作）。
        assert_ne!(value, random_string(512).unwrap());
    }

    /// 拒绝采样必须**覆盖全部 66 个字符、且每个等权**——遍历 256 个字节值**确定性**断言。
    ///
    /// ⚠️ 不要改回「抽 512 次随机串看碰到几个字符」：那是概率断言，约 3% 的运行会漏掉
    /// 某个字符而假红（`(1 - 1/66)^512 ≈ 4.3e-4`，66 个字符 → 期望漏掉 ≈ 0.028 个）。
    #[test]
    fn rejection_sampling_covers_every_character_uniformly() {
        let mut hits = vec![0u32; VERIFIER_CHARS.len()];
        let mut rejected = 0u32;
        for byte in 0u8..=255 {
            match sample_index(byte) {
                Some(index) => hits[index] += 1,
                None => rejected += 1,
            }
        }
        // 每个字符恰被 3 个字节值选中 = `value % 66` 在 0..198 上均匀。
        assert!(hits.iter().all(|&n| n == 3), "{hits:?}");
        // 被拒的字节数 = 256 - 198 = 58，正是 `256 % 66`。
        assert_eq!(rejected, 256 - SAMPLE_ACCEPT as u32);
        assert_eq!(rejected, 256 % VERIFIER_CHARS.len() as u32);
    }

    // ---- 授权 URL ----

    #[test]
    fn authorization_url_carries_the_pkce_parameters() {
        let url = authorization_url(
            Provider::Gmail,
            "client-1",
            "http://127.0.0.1:12345/cb",
            "state-1",
            "nonce-1",
            "challenge-1",
            None,
        )
        .unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code"));
        assert!(url.contains("&client_id=client-1"));
        // redirect_uri 里的 `:`/`/` 必须被编码，否则服务商的 query 解析会走样。
        assert!(url.contains("&redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Fcb"));
        assert!(url.contains("&state=state-1"));
        assert!(url.contains("&nonce=nonce-1"));
        assert!(url.contains("&code_challenge=challenge-1"));
        assert!(url.contains("&code_challenge_method=S256"));
        // Gmail 要离线刷新令牌。
        assert!(url.contains("&access_type=offline&prompt=consent"));
        // scope 之间是空格，编码后是 %20。
        assert!(url.contains("scope=https%3A%2F%2Fmail.google.com%2F%20openid"));
    }

    #[test]
    fn authorization_url_forces_the_account_picker_on_microsoft() {
        let without_hint = authorization_url(
            Provider::Microsoft,
            "c",
            "http://127.0.0.1/cb",
            "s",
            "n",
            "ch",
            None,
        )
        .unwrap();
        assert!(without_hint.contains("&prompt=select_account"));
        assert!(!without_hint.contains("login_hint"));

        let with_hint = authorization_url(
            Provider::Microsoft,
            "c",
            "http://127.0.0.1/cb",
            "s",
            "n",
            "ch",
            Some("me@example.com"),
        )
        .unwrap();
        assert!(with_hint.contains("&login_hint=me%40example.com"));
        // 有提示就不再强制选择器（那会让用户多点一步）。
        assert!(!with_hint.contains("prompt=select_account"));
    }

    #[test]
    fn authorization_url_rejects_plain_imap_accounts() {
        let error = authorization_url(
            Provider::Imap,
            "c",
            "http://127.0.0.1/cb",
            "s",
            "n",
            "ch",
            None,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsupported");
    }

    // ---- 回调 ----

    #[test]
    fn redirect_parses_code_and_state() {
        let redirect = parse_redirect("?code=abc%2Fdef&state=xyz").unwrap();
        assert_eq!(redirect.code, "abc/def");
        assert_eq!(redirect.state.as_deref(), Some("xyz"));
        // 没有前导问号也行。
        assert_eq!(parse_redirect("code=a&state=b").unwrap().code, "a");
        // 多余的参数被忽略；`+` 解成空格。
        assert_eq!(parse_redirect("code=a&scope=x+y").unwrap().code, "a");
    }

    #[test]
    fn redirect_surfaces_provider_errors() {
        let error =
            parse_redirect("?error=access_denied&error_description=user%20said%20no").unwrap_err();
        assert_eq!(error.code(), "auth");
        assert!(error.to_string().contains("access_denied"));
        assert!(error.to_string().contains("user said no"));

        let missing = parse_redirect("?state=xyz").unwrap_err();
        assert_eq!(missing.code(), "auth");
        assert!(missing.to_string().contains("no authorization code"));
    }

    #[test]
    fn form_decode_keeps_malformed_escapes() {
        assert_eq!(form_decode("a+b"), "a b");
        assert_eq!(form_decode("%41%42"), "AB");
        // 写坏的转义原样留下（不吞掉后面的字符）。
        assert_eq!(form_decode("100%"), "100%");
        assert_eq!(form_decode("%zz"), "%zz");
        // 多字节 UTF-8 能还原。
        assert_eq!(form_decode("%E4%B8%AD"), "中");
    }

    // ---- 令牌 ----

    #[test]
    fn token_response_parses_the_three_fields() {
        let tokens =
            parse_token_response(r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#)
                .unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tokens.expires_in, Some(3600));

        // 缺可选字段也能解析（Google 重复授权时可能不给 refresh_token）。
        let minimal = parse_token_response(r#"{"access_token":"at"}"#).unwrap();
        assert_eq!(minimal.refresh_token, None);
        assert_eq!(minimal.expires_in, None);
    }

    #[test]
    fn token_response_classifies_errors() {
        // 失效的刷新令牌 → 要用户重新授权。
        assert_eq!(
            parse_token_response(r#"{"error":"invalid_grant","error_description":"expired"}"#)
                .unwrap_err()
                .code(),
            "needs_reauth"
        );
        // 服务端抖动 → 可重试。
        let transient = parse_token_response(r#"{"error":"temporarily_unavailable"}"#).unwrap_err();
        assert_eq!(transient.code(), "connect");
        assert!(transient.is_retryable());
        // 其他 → 认证类错误（重试无用）。
        assert_eq!(
            parse_token_response(r#"{"error":"invalid_client"}"#)
                .unwrap_err()
                .code(),
            "auth"
        );
    }

    #[test]
    fn token_response_rejects_broken_payloads() {
        // 不是 JSON。
        assert_eq!(
            parse_token_response("<html>502</html>").unwrap_err().code(),
            "auth"
        );
        // JSON 但没有 access_token。
        assert_eq!(
            parse_token_response(r#"{"token_type":"Bearer"}"#)
                .unwrap_err()
                .code(),
            "auth"
        );
        // 空令牌。
        assert_eq!(
            parse_token_response(r#"{"access_token":""}"#)
                .unwrap_err()
                .code(),
            "auth"
        );
        // 令牌里带控制字符（夹带换行 = 头注入尝试）。
        assert!(parse_token_response(r#"{"access_token":"a\nb"}"#).is_err());
        // 越界的寿命。
        assert!(parse_token_response(r#"{"access_token":"at","expires_in":0}"#).is_err());
        assert!(parse_token_response(r#"{"access_token":"at","expires_in":99999999999}"#).is_err());
    }

    // ---- 表单 ----

    #[test]
    fn exchange_form_carries_the_verifier() {
        let form = exchange_form("code-1", "http://127.0.0.1/cb", "cid", "ver", None);
        let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("grant_type"), Some("authorization_code"));
        assert_eq!(get("code"), Some("code-1"));
        assert_eq!(get("code_verifier"), Some("ver"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1/cb"));
        // 没有 client_secret 就不该有这一段（公共客户端）。
        assert_eq!(get("client_secret"), None);

        let with_secret = exchange_form("c", "r", "cid", "ver", Some("sec"));
        assert!(
            with_secret
                .iter()
                .any(|(k, v)| k == "client_secret" && v == "sec")
        );
    }

    #[test]
    fn refresh_form_only_includes_scope_when_asked() {
        let plain = refresh_form("rt", "cid", None, None);
        assert!(!plain.iter().any(|(k, _)| k == "scope"));
        assert!(
            plain
                .iter()
                .any(|(k, v)| k == "grant_type" && v == "refresh_token")
        );

        let scoped = refresh_form("rt", "cid", Some("sec"), Some("https://x/y"));
        assert!(
            scoped
                .iter()
                .any(|(k, v)| k == "scope" && v == "https://x/y")
        );
        assert!(
            scoped
                .iter()
                .any(|(k, v)| k == "client_secret" && v == "sec")
        );
    }

    #[test]
    fn token_endpoint_must_be_https() {
        let error = post_token_form("http://example.com/token", &[]).unwrap_err();
        assert_eq!(error.code(), "config");
    }

    // ---- XOAUTH2 ----

    #[test]
    fn xoauth2_initial_response_has_the_sasl_shape() {
        let raw = sasl_xoauth2("a@b.c", "tok");
        assert_eq!(raw, "user=a@b.c\u{1}auth=Bearer tok\u{1}\u{1}");
        // base64 版本必须与原文一一对应（IMAP 与 SMTP 走同一份真源）。
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(sasl_xoauth2_base64("a@b.c", "tok"))
            .unwrap();
        assert_eq!(decoded, raw.as_bytes());
    }

    #[test]
    fn urlencode_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(urlencode("abc-._~123"), "abc-._~123");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a@b.c"), "a%40b.c");
        assert_eq!(urlencode("/"), "%2F");
        assert_eq!(urlencode("中"), "%E4%B8%AD");
    }

    #[test]
    fn excerpt_is_single_line_and_bounded() {
        assert_eq!(excerpt("a\nb\tc"), "a b c");
        let long = "x".repeat(500);
        let out = excerpt(&long);
        assert!(out.chars().count() <= 201);
        assert!(out.ends_with('…'));
    }
}
