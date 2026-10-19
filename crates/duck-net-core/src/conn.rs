//! 连通性**分步**检测：S3（SigV4 + HEAD bucket）/ FTP（真协议登录）/ SFTP（系统 `ssh`）。
//!
//! 移植自参考仓 `src-tauri/src/net/conn_check.rs`（374 行）的四个 `#[tauri::command]`
//! 中的三个（`conn_check_port` 不迁：那是「TCP 端口探测」，本仓已有 `packet-send`
//! 的 raw 协议，单独一个「只连不发」的工具没有独立价值）。
//!
//! ## 为什么要把「一步」做成数据结构
//!
//! 源仓的四个命令各自 `format!` 一句中文 detail 返回前端，`ok` 只是一个 bool。
//! 连通检测的价值恰恰在**分步**（设计稿原话：「一步失败要指出是网络不通、凭据错
//! 还是权限不足」），所以这里把过程摊成 [`Step`] 列表：每一步有稳定 id、结论、
//! 可选细节。插件层按 id 查译文、拼细节，于是「DNS 失败」在英文界面里也是英文，
//! 而内核里一个中文字面量都不需要。
//!
//! `ok` 仍保留，由步骤推导（[`CheckOutcome::ok`]），调用方不必自己算。
//!
//! ## 两处实现路线
//!
//! - **S3 / FTP**：自己发协议（`ureq` + 裸 `TcpStream`），与源仓同路线。
//! - **SFTP**：**不引 `ssh2`**，改走系统 `ssh`（[`duck_ssh`]）。理由与
//!   `duck-ssh` crate 文档里「为什么不用 russh / ssh2」完全一致：OpenSSH 的
//!   stderr（`Permission denied (password).`）本身就是最准确的诊断，自己实现
//!   协议栈只能把它翻译成更差的句子。副产物：口令认证直接复用 `askpass` 那条
//!   已经验证过的路，不必在这里再写一遍。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use duck_ssh::{AuthMethod, SshError, Target, close_master, exec, shell_quote, test_connection};

use crate::error::{ErrorKind, NetError, NetResult};
use crate::http::{self, HttpRequest, Method};

/// 一步的稳定 id（插件层按它查译文，改名等于改协议）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepId {
    /// 凭据形态（S3）：`signed` / `anonymous` / `partial`。
    Credential,
    /// 发请求（S3）/ 建连接（FTP/SFTP）。
    Request,
    /// 对端回应（S3）。
    Response,
    /// TCP 握手（FTP/SFTP）。
    Tcp,
    /// 服务端欢迎语（FTP 220）。
    Banner,
    /// `USER`（FTP 331）。
    User,
    /// `PASS`（FTP 230）。
    Password,
    /// `PWD`（FTP 257）。
    Pwd,
    /// SSH 认证（SFTP）。
    Ssh,
    /// 远端路径可访问（SFTP）。
    Path,
}

impl StepId {
    /// 稳定 id。插件译文表的键就是这个。
    pub fn id(self) -> &'static str {
        match self {
            Self::Credential => "credential",
            Self::Request => "request",
            Self::Response => "response",
            Self::Tcp => "tcp",
            Self::Banner => "banner",
            Self::User => "user",
            Self::Password => "password",
            Self::Pwd => "pwd",
            Self::Ssh => "ssh",
            Self::Path => "path",
        }
    }
}

/// 一步的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Fail,
    /// 前面的步骤已经失败，这一步没跑（不算失败，报告里要能区分「没做」与「做错了」）。
    Skipped,
}

impl StepStatus {
    pub fn id(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// 一步。
///
/// `detail` 是**给机器看的短串**（HTTP 状态码、FTP 响应码、`signed`/`anonymous`），
/// 或者对端自己吐的英文原文（ssh 的 stderr、FTP 应答行）。插件层直接拼在译文后面，
/// 不再二次加工 —— 那是唯一能保证「原文不被翻译坏」的做法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: StepId,
    pub status: StepStatus,
    pub detail: Option<String>,
}

impl Step {
    pub fn ok(id: StepId) -> Self {
        Self {
            id,
            status: StepStatus::Ok,
            detail: None,
        }
    }

    pub fn ok_with(id: StepId, detail: impl Into<String>) -> Self {
        Self {
            id,
            status: StepStatus::Ok,
            detail: Some(detail.into()),
        }
    }

    pub fn fail(id: StepId, detail: impl Into<String>) -> Self {
        Self {
            id,
            status: StepStatus::Fail,
            detail: Some(detail.into()),
        }
    }

    /// 失败但拿不到细节（超时、连接被关）。
    pub fn failed(id: StepId) -> Self {
        Self {
            id,
            status: StepStatus::Fail,
            detail: None,
        }
    }

    pub fn skipped(id: StepId) -> Self {
        Self {
            id,
            status: StepStatus::Skipped,
            detail: None,
        }
    }
}

/// 一次检测的产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub elapsed_ms: u64,
    pub steps: Vec<Step>,
    /// 对端自报的身份（SFTP 的 `uname -srm`、FTP 的欢迎语）。可能没有。
    pub banner: Option<String>,
}

impl CheckOutcome {
    /// 全部步骤都通过（没跑的不算失败）。
    pub fn ok(&self) -> bool {
        !self
            .steps
            .iter()
            .any(|step| step.status == StepStatus::Fail)
    }

    /// 第一个失败步骤（报告里给结论用）。
    pub fn failed_step(&self) -> Option<&Step> {
        self.steps
            .iter()
            .find(|step| step.status == StepStatus::Fail)
    }

    fn finish(started: Instant, steps: Vec<Step>, banner: Option<String>) -> Self {
        Self {
            elapsed_ms: started.elapsed().as_millis() as u64,
            steps,
            banner,
        }
    }
}

// ───────────────────────────── S3 ─────────────────────────────

/// S3 兼容接口的检测参数。
#[derive(Debug, Clone)]
pub struct S3Target {
    /// `https://s3.amazonaws.com` / `http://minio.lan:9000`（尾部 `/` 会被吃掉）。
    pub endpoint: String,
    pub bucket: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    /// 签名域；留空用 `us-east-1`（与源仓同口径）。
    pub region: String,
    pub timeout_ms: u64,
    /// 跳过证书校验（内网自签 MinIO 用）。
    pub insecure: bool,
}

impl Default for S3Target {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            bucket: String::new(),
            access_key: None,
            secret_key: None,
            region: String::new(),
            timeout_ms: 10_000,
            insecure: false,
        }
    }
}

/// HEAD 一个 bucket。**有凭据就签名，没凭据就匿名**（私有桶会得到 403）。
///
/// 返回 `Err` 只表示「参数本身不成立」（Endpoint / Bucket 没填），
/// 网络与权限问题一律落在步骤里 —— 那才是用户想逐条看的。
pub fn check_s3(target: &S3Target) -> NetResult<CheckOutcome> {
    let started = Instant::now();
    let endpoint = target.endpoint.trim().trim_end_matches('/');
    let bucket = target.bucket.trim();
    if endpoint.is_empty() || bucket.is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }

    let url = format!("{endpoint}/{bucket}/");
    let region = match target.region.trim() {
        "" => DEFAULT_REGION,
        given => given,
    };
    let mut steps = Vec::new();

    let access = target
        .access_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let secret = target
        .secret_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut request = HttpRequest {
        method: Method::Head,
        url: url.clone(),
        timeout_ms: target.timeout_ms,
        insecure: target.insecure,
        ..HttpRequest::default()
    };

    match (access, secret) {
        (Some(access), Some(secret)) => {
            let host = endpoint_host(&url).unwrap_or_default();
            request.headers = sigv4_headers("HEAD", &url, &host, region, access, secret);
            steps.push(Step::ok_with(StepId::Credential, "signed"));
        }
        // 两个都空 = 匿名访问（公共桶）。这是合法用法，不算失败。
        (None, None) => steps.push(Step::ok_with(StepId::Credential, "anonymous")),
        // 只填了一半：几乎都是粘贴漏了一行。继续匿名请求只会得到 403，
        // 报「权限不足」就把人指到错的方向去了。
        _ => {
            steps.push(Step::fail(StepId::Credential, "partial"));
            steps.push(Step::skipped(StepId::Request));
            steps.push(Step::skipped(StepId::Response));
            return Ok(CheckOutcome::finish(started, steps, None));
        }
    }

    let response = match http::send(&request) {
        Ok(response) => response,
        Err(error) => {
            steps.push(Step::fail(
                StepId::Request,
                join_detail(error.code(), error.detail()),
            ));
            steps.push(Step::skipped(StepId::Response));
            return Ok(CheckOutcome::finish(started, steps, None));
        }
    };
    steps.push(Step::ok(StepId::Request));

    // 200 = 通；403 = 桶在但没权限（凭据错 / 策略错）；404 = 桶名不对或区域不对；
    // 301 = endpoint 与桶的实际区域不一致（S3 特有，源仓漏了这一档，看到 301 只会
    // 显示「响应异常」，用户完全无从下手）。
    let status = response.status;
    if status == 200 {
        steps.push(Step::ok_with(StepId::Response, "200"));
    } else {
        steps.push(Step::fail(StepId::Response, status.to_string()));
    }
    let banner = response
        .header("server")
        .map(|value| value.to_string())
        .or_else(|| response.header("x-amz-request-id").map(str::to_string));
    Ok(CheckOutcome::finish(started, steps, banner))
}

const DEFAULT_REGION: &str = "us-east-1";

/// `https://host:port/path` → `host:port`（签名里的 `Host` 头必须**带端口**）。
fn endpoint_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// SigV4 签名头（`Authorization` / `x-amz-date` / `x-amz-content-sha256`）。
///
/// 委托给 [`crate::s3::sign`]：连通检测只签空体 `HEAD`，而 `s3-manager` 要签
/// body 与额外头，两份实现没有理由不一致。**这一处曾经是个真 bug** ——
/// 老版本把 HMAC 做成「clone 上一层状态再 update」，算出来的是
/// `HMAC(K, m1 || m2)` 而不是 `HMAC(HMAC(K, m1), m2)`，于是签名永远对不上；
/// 因为检测只看 200/403，这个错一直被当成「权限不足」显示出来。
fn sigv4_headers(
    method: &str,
    url: &str,
    host: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Vec<(String, String)> {
    let path = canonical_path(url, host);
    let path = crate::s3::sign::encode_path(&path);
    crate::s3::sign::sign(&crate::s3::sign::SignInput {
        method,
        host,
        path: &path,
        query: &[],
        headers: &[],
        payload: b"",
        region,
        credentials: crate::s3::sign::Credentials {
            access_key,
            secret_key,
        },
    })
}

/// URL 的 path 部分（含前导 `/`）。
fn canonical_path(url: &str, host: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    // 只剩 `host[:port]/...`，把 authority 去掉。
    let path = without_scheme.strip_prefix(host).unwrap_or(without_scheme);
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// `code` 与可选细节拼成一行（`connect: 127.0.0.1:21 Connection refused`）。
fn join_detail(code: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) if !detail.is_empty() => format!("{code}: {detail}"),
        _ => code.to_string(),
    }
}

// ───────────────────────────── FTP ─────────────────────────────

/// FTP 检测参数。
#[derive(Debug, Clone)]
pub struct FtpTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub timeout_ms: u64,
}

impl Default for FtpTarget {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 21,
            username: String::new(),
            password: String::new(),
            timeout_ms: 10_000,
        }
    }
}

/// 真协议登录：连 → 读 220 → `USER` → `PASS` → `PWD` → `QUIT`。
///
/// **不做被动模式的数据连接**：检测只需要控制通道上的认证结论，建立数据连接
/// 会额外打开一个端口，在内网防火墙下反而多一个失败点。
pub fn check_ftp(target: &FtpTarget) -> NetResult<CheckOutcome> {
    let started = Instant::now();
    let host = target.host.trim();
    if host.is_empty() || target.port == 0 {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    let timeout = Duration::from_millis(target.timeout_ms.max(1_000));
    let address = format!("{host}:{}", target.port);
    let mut steps = Vec::new();

    let stream = match crate::tcp::connect(&address, target.timeout_ms) {
        Ok(stream) => {
            steps.push(Step::ok(StepId::Tcp));
            stream
        }
        Err(error) => {
            steps.push(Step::fail(
                StepId::Tcp,
                join_detail(error.code(), error.detail()),
            ));
            for id in [StepId::Banner, StepId::User, StepId::Password, StepId::Pwd] {
                steps.push(Step::skipped(id));
            }
            return Ok(CheckOutcome::finish(started, steps, None));
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let mut reader = match stream.try_clone() {
        Ok(cloned) => BufReader::new(cloned),
        Err(error) => return Err(NetError::io(error)),
    };
    let mut writer = stream;
    let mut line = String::new();

    // 220 欢迎语。多行欢迎语（`220-`）由 read_reply 读到 `220 ` 为止。
    let code = match read_reply(&mut reader, &mut line, timeout) {
        Ok(code) => code,
        Err(error) => {
            steps.push(Step::fail(
                StepId::Banner,
                join_detail(error.code(), error.detail()),
            ));
            for id in [StepId::User, StepId::Password, StepId::Pwd] {
                steps.push(Step::skipped(id));
            }
            let banner = non_empty(first_line(&line));
            return Ok(CheckOutcome::finish(started, steps, banner));
        }
    };
    let banner = non_empty(first_line(&line));
    if code != 220 {
        steps.push(Step::fail(StepId::Banner, code.to_string()));
        for id in [StepId::User, StepId::Password, StepId::Pwd] {
            steps.push(Step::skipped(id));
        }
        return Ok(CheckOutcome::finish(started, steps, banner));
    }
    steps.push(Step::ok_with(StepId::Banner, "220"));

    // USER / PASS：源仓对 USER 的响应是「读到就行，码只记下来」，PASS 的 230 才
    // 是判定依据 —— 有些服务端对匿名账号回 230（跳过 USER），按 331 硬判会误报失败。
    let user = target.username.trim();
    let (user_code, _) = ftp_command(
        &mut writer,
        &mut reader,
        &format!("USER {user}"),
        "USER",
        timeout,
    )
    .unwrap_or((0, String::new()));
    steps.push(match user_code {
        0 => Step::failed(StepId::User),
        code => Step::ok_with(StepId::User, code.to_string()),
    });

    let (pass_code, pass_line) = ftp_command(
        &mut writer,
        &mut reader,
        &format!("PASS {password}", password = target.password),
        "PASS",
        timeout,
    )
    .unwrap_or((0, String::new()));
    if pass_code == 230 {
        steps.push(Step::ok_with(StepId::Password, "230"));
    } else {
        let detail = match pass_code {
            0 => first_line(&pass_line),
            code => format!("{code} {}", first_line(&pass_line))
                .trim()
                .to_string(),
        };
        steps.push(Step::fail(StepId::Password, detail));
        steps.push(Step::skipped(StepId::Pwd));
        let _ = ftp_command(&mut writer, &mut reader, "QUIT", "QUIT", timeout);
        return Ok(CheckOutcome::finish(started, steps, banner));
    }

    // PWD：确认这条会话真的能取目录（认证通过但被 chroot 到不可读的位置也有）。
    let (pwd_code, pwd_line) =
        ftp_command(&mut writer, &mut reader, "PWD", "PWD", timeout).unwrap_or((0, String::new()));
    if pwd_code == 257 {
        steps.push(Step::ok_with(StepId::Pwd, pwd_line.trim().to_string()));
    } else {
        steps.push(Step::fail(StepId::Pwd, pwd_code.to_string()));
    }
    let _ = ftp_command(&mut writer, &mut reader, "QUIT", "QUIT", timeout);

    Ok(CheckOutcome::finish(started, steps, banner))
}

/// 发一条 FTP 命令并读回它的应答。
fn ftp_command(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    command: &str,
    label: &str,
    timeout: Duration,
) -> NetResult<(u32, String)> {
    // `label` 只进错误细节：口令永远不能跟着命令一起进错误串或日志。
    let payload = format!("{command}\r\n");
    writer
        .write_all(payload.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| NetError::with_detail(ErrorKind::Connect, format!("{label} {error}")))?;
    let mut line = String::new();
    let code = read_reply(reader, &mut line, timeout)?;
    Ok((code, line))
}

/// 读一条 FTP 应答（支持 `123-` 多行，读到 `123 ` 为止），返回三位响应码。
///
/// 移植源仓 `conn_check.rs::read_reply`：多行的判定是「第 4 个字节是空格」，
/// 与 RFC 959 的 `123-` / `123 ` 规则一致。
fn read_reply(
    reader: &mut BufReader<TcpStream>,
    out: &mut String,
    timeout: Duration,
) -> NetResult<u32> {
    let deadline = Instant::now() + timeout;
    let mut last = 0u32;
    loop {
        if Instant::now() > deadline {
            return Err(NetError::with_detail(ErrorKind::Timeout, first_line(out)));
        }
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| NetError::with_detail(ErrorKind::Io, error.to_string()))?;
        if count == 0 {
            return Err(NetError::with_detail(ErrorKind::Connect, first_line(out)));
        }
        out.push_str(&line);
        let trimmed = line.trim();
        if let Some(code) = trimmed
            .chars()
            .take(3)
            .collect::<String>()
            .parse::<u32>()
            .ok()
            .filter(|code| (200..=599).contains(code))
        {
            last = code;
        }
        if trimmed.len() >= 4 && trimmed.as_bytes()[3] == b' ' {
            break;
        }
        // `\n` 结尾但不足 4 字节（例如单行 `220`）：也是完整回应。
        if trimmed.len() <= 3 && last != 0 {
            break;
        }
    }
    Ok(last)
}

/// 第一行（欢迎语常是多行，报告里只需要一行）。
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// 空串当「没有」（报告里不该出现一个空的「对端：」）。
fn non_empty(text: String) -> Option<String> {
    match text.is_empty() {
        true => None,
        false => Some(text),
    }
}

// ───────────────────────────── SFTP ─────────────────────────────

/// SFTP 检测参数。
#[derive(Debug, Clone)]
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// 口令（走 askpass 助手，不进 argv）。
    pub password: Option<String>,
    /// 私钥路径。
    pub key_path: Option<String>,
    /// 要验证可访问的远端目录（留空 = 不检查）。
    pub remote_path: String,
    pub timeout_ms: u64,
}

impl Default for SftpTarget {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            username: String::new(),
            password: None,
            key_path: None,
            remote_path: String::new(),
            timeout_ms: 10_000,
        }
    }
}

/// 用系统 `ssh` 认证一次，再 `test -d` 一下远端目录。
///
/// 连接复用目录用**临时目录**而不是用户的 `~/.ssh/sockets`：这里只是检测，
/// 不该在用户的会话里留下一个 5 分钟后才消失的 master。检测完立刻
/// [`close_master`] 收口 —— 那条连接是我们自己拉起来的。
pub fn check_sftp(target: &SftpTarget) -> NetResult<CheckOutcome> {
    let started = Instant::now();
    let host = target.host.trim();
    if host.is_empty() || target.port == 0 {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    let mut steps = Vec::new();

    let address = format!("{host}:{}", target.port);
    match crate::tcp::connect(&address, target.timeout_ms) {
        Ok(stream) => {
            // 只要握手成功就够了；关掉让 ssh 自己去连（它不认别人的 socket）。
            drop(stream);
            steps.push(Step::ok(StepId::Tcp));
        }
        Err(error) => {
            steps.push(Step::fail(
                StepId::Tcp,
                join_detail(error.code(), error.detail()),
            ));
            steps.push(Step::skipped(StepId::Ssh));
            steps.push(Step::skipped(StepId::Path));
            return Ok(CheckOutcome::finish(started, steps, None));
        }
    }

    let ssh_target = build_target(target);
    let dir = std::env::temp_dir().join("duck-net-sftp-check");
    let banner = match test_connection(&ssh_target, Some(&dir)) {
        Ok(banner) => {
            steps.push(Step::ok(StepId::Ssh));
            Some(banner)
        }
        Err(error) => {
            let (kind, detail) = map_ssh(&error);
            let mut text = kind.code().to_string();
            if let Some(detail) = detail {
                text = format!("{text}: {detail}");
            }
            steps.push(Step::fail(StepId::Ssh, text));
            steps.push(Step::skipped(StepId::Path));
            let _ = close_master(&ssh_target, &dir);
            return Ok(CheckOutcome::finish(started, steps, None));
        }
    };

    let remote = target.remote_path.trim();
    if remote.is_empty() {
        steps.push(Step::skipped(StepId::Path));
    } else {
        let command = format!("test -d {} && pwd", shell_quote(remote));
        match exec(&ssh_target, Some(&dir), &command) {
            Ok(output) => {
                let resolved = output.stdout.trim();
                if resolved.is_empty() {
                    steps.push(Step::ok(StepId::Path));
                } else {
                    steps.push(Step::ok_with(StepId::Path, resolved.to_string()));
                }
            }
            Err(error) => {
                let (kind, detail) = map_ssh(&error);
                let mut text = kind.code().to_string();
                if let Some(detail) = detail {
                    text = format!("{text}: {detail}");
                }
                steps.push(Step::fail(StepId::Path, text));
            }
        }
    }

    let _ = close_master(&ssh_target, &dir);
    Ok(CheckOutcome::finish(started, steps, banner))
}

/// 参数 → `duck_ssh::Target`。
///
/// 认证方式按「给了私钥就只认私钥、否则有口令就只认口令」收窄（与源仓的
/// `password` 优先、`private_key_path` 其次的判定顺序一致）；超时通过 `-o
/// ConnectTimeout` 覆盖，`base_argv` 里那个 10 秒的默认值在后、会被它顶掉。
fn build_target(target: &SftpTarget) -> Target {
    let key = target
        .key_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from);
    let password = target
        .password
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let auth = if key.is_some() {
        AuthMethod::Key
    } else if password.is_some() {
        AuthMethod::Password
    } else {
        AuthMethod::Auto
    };
    // 秒级向上取整：`ConnectTimeout` 只认整数秒。
    let seconds = target.timeout_ms.max(1_000).div_ceil(1_000);
    Target {
        host: target.host.trim().to_string(),
        port: target.port,
        user: target.username.trim().to_string(),
        identity: key,
        password,
        auth,
        options: vec![format!("ConnectTimeout={seconds}")],
        forwards: Vec::new(),
        agent_forwarding: false,
    }
}

/// `SshError` → 稳定错误码 + 详情。
///
/// 详情只取对端原文（`Permission denied (password).` 这种英文串），
/// **不取** `SshError` 自己的中文模板 —— 那是给界面层用的源串，内核不转发。
fn map_ssh(error: &SshError) -> (ErrorKind, Option<String>) {
    match error {
        SshError::NotFound { program } => (ErrorKind::Unsupported, Some((*program).to_string())),
        // `reason` 是 i18n 源串（中文），不能进详情。
        SshError::InvalidTarget { .. } => (ErrorKind::EmptyInput, None),
        SshError::Spawn { message } => (ErrorKind::Io, Some(message.clone())),
        SshError::Failed { status, stderr } => {
            let lower = stderr.to_ascii_lowercase();
            let kind = if lower.contains("permission denied")
                || lower.contains("authentication failed")
                || lower.contains("no more authentication")
            {
                ErrorKind::Auth
            } else if lower.contains("host key verification failed")
                || lower.contains("remote host identification has changed")
            {
                ErrorKind::Tls
            } else if lower.contains("timed out") || lower.contains("connection timed out") {
                ErrorKind::Timeout
            } else if lower.contains("could not resolve")
                || lower.contains("name or service not known")
            {
                ErrorKind::Dns
            } else {
                ErrorKind::Connect
            };
            let detail = if stderr.trim().is_empty() {
                status.map(|code| code.to_string())
            } else {
                Some(stderr.trim().to_string())
            };
            (kind, detail)
        }
        SshError::NoSuchDir { path } => (ErrorKind::PathMissing, Some(path.clone())),
        SshError::Transfer { message } => (ErrorKind::Io, Some(message.clone())),
        SshError::TimedOut { secs } => (ErrorKind::Timeout, Some(secs.to_string())),
        SshError::LocalMissing { path } => (ErrorKind::PathMissing, Some(path.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_ids_are_stable_ascii() {
        for id in [
            StepId::Credential,
            StepId::Request,
            StepId::Response,
            StepId::Tcp,
            StepId::Banner,
            StepId::User,
            StepId::Password,
            StepId::Pwd,
            StepId::Ssh,
            StepId::Path,
        ] {
            assert!(id.id().is_ascii(), "{id:?}");
        }
        assert!(StepStatus::Ok.id().is_ascii());
    }

    #[test]
    fn outcome_is_ok_unless_some_step_failed() {
        let mut outcome = CheckOutcome {
            elapsed_ms: 1,
            steps: vec![Step::ok(StepId::Tcp), Step::skipped(StepId::Path)],
            banner: None,
        };
        assert!(outcome.ok(), "跳过的步骤不算失败");
        outcome.steps.push(Step::failed(StepId::Ssh));
        assert!(!outcome.ok());
        assert_eq!(outcome.failed_step().map(|step| step.id), Some(StepId::Ssh));
    }

    #[test]
    fn empty_targets_are_input_errors_not_step_failures() {
        assert_eq!(
            check_s3(&S3Target::default()).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            check_ftp(&FtpTarget::default()).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        assert_eq!(
            check_sftp(&SftpTarget::default()).unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
    }

    #[test]
    fn s3_reports_partial_credentials_without_touching_the_network() {
        let outcome = check_s3(&S3Target {
            endpoint: "https://s3.example.com".into(),
            bucket: "demo".into(),
            access_key: Some("AK".into()),
            secret_key: Some("  ".into()),
            ..S3Target::default()
        })
        .expect("参数成立");
        assert!(!outcome.ok());
        let step = outcome.failed_step().expect("应有失败步骤");
        assert_eq!(step.id, StepId::Credential);
        assert_eq!(step.detail.as_deref(), Some("partial"));
        // 后面两步必须是「没跑」，而不是「失败」—— 报告要能区分这两件事。
        assert_eq!(outcome.steps[1].status, StepStatus::Skipped);
    }

    #[test]
    fn sigv4_signature_matches_aws_reference_vector() {
        // AWS 文档里的签名示例（GET /test.txt，2013-05-24 的固定时间无法注进来，
        // 所以这里只钉两件与时间无关的事实：签名头的形状，以及同样的输入是否稳定）。
        let headers = sigv4_headers(
            "HEAD",
            "https://s3.amazonaws.com/demo/",
            "s3.amazonaws.com",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Authorization", "x-amz-date", "x-amz-content-sha256"]
        );
        let auth = &headers[0].1;
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/",));
        assert!(auth.contains("/us-east-1/s3/aws4_request"));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        // 空体的 SHA256 是常量，可以直接断言。
        let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(headers[2].1, empty_hash);
    }

    #[test]
    fn canonical_path_keeps_the_leading_slash() {
        assert_eq!(
            canonical_path("https://s3.amazonaws.com/demo/", "s3.amazonaws.com"),
            "/demo/"
        );
        assert_eq!(
            canonical_path("https://s3.amazonaws.com", "s3.amazonaws.com"),
            "/"
        );
        assert_eq!(
            canonical_path("http://minio.lan:9000/demo", "minio.lan:9000"),
            "/demo"
        );
        assert_eq!(canonical_path("/demo/", "s3.amazonaws.com"), "/demo/");
    }

    #[test]
    fn endpoint_host_keeps_the_port_for_the_signed_header() {
        assert_eq!(
            endpoint_host("https://s3.amazonaws.com/demo/").as_deref(),
            Some("s3.amazonaws.com")
        );
        assert_eq!(
            endpoint_host("http://10.0.0.5:9000/demo/").as_deref(),
            Some("10.0.0.5:9000")
        );
        assert_eq!(endpoint_host("ftp://x/y"), None);
    }

    #[test]
    fn ftp_check_against_a_closed_port_stops_at_tcp() {
        let outcome = check_ftp(&FtpTarget {
            host: "127.0.0.1".into(),
            port: 1,
            username: "u".into(),
            password: "p".into(),
            timeout_ms: 300,
        })
        .expect("参数成立");
        assert!(!outcome.ok());
        assert_eq!(outcome.failed_step().map(|step| step.id), Some(StepId::Tcp));
        // 后续全部标「没跑」，报告里不会出现四个红叉。
        assert!(
            outcome.steps[1..]
                .iter()
                .all(|step| step.status == StepStatus::Skipped)
        );
    }

    #[test]
    fn ftp_login_works_against_a_fake_server() {
        // 一个只会照着脚本应答的本地服务：220 → 331 → 230 → 257。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let _ = writer.write_all(b"220 fake ftp ready\r\n");
            let mut line = String::new();
            for reply in [
                "331 need password\r\n",
                "230 logged in\r\n",
                "257 \"/incoming\" is current\r\n",
                "221 bye\r\n",
            ] {
                line.clear();
                let _ = reader.read_line(&mut line);
                let _ = writer.write_all(reply.as_bytes());
            }
        });

        let outcome = check_ftp(&FtpTarget {
            host: "127.0.0.1".into(),
            port,
            username: "bank".into(),
            password: "secret".into(),
            timeout_ms: 3_000,
        })
        .expect("参数成立");
        handle.join().expect("server thread");
        assert!(outcome.ok(), "{:?}", outcome.steps);
        assert_eq!(outcome.banner.as_deref(), Some("220 fake ftp ready"));
        let pwd = outcome
            .steps
            .iter()
            .find(|step| step.id == StepId::Pwd)
            .expect("pwd 步骤");
        assert!(pwd.detail.as_deref().unwrap_or("").contains("/incoming"));
    }

    #[test]
    fn ftp_wrong_password_marks_auth_and_skips_pwd() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let _ = writer.write_all(b"220 ready\r\n");
            let mut line = String::new();
            for reply in [
                "331 need password\r\n",
                "530 login incorrect\r\n",
                "221 bye\r\n",
            ] {
                line.clear();
                let _ = reader.read_line(&mut line);
                let _ = writer.write_all(reply.as_bytes());
            }
        });

        let outcome = check_ftp(&FtpTarget {
            host: "127.0.0.1".into(),
            port,
            username: "bank".into(),
            password: "wrong".into(),
            timeout_ms: 3_000,
        })
        .expect("参数成立");
        handle.join().expect("server thread");
        assert!(!outcome.ok());
        let failed = outcome.failed_step().expect("应有失败");
        assert_eq!(failed.id, StepId::Password);
        assert!(failed.detail.as_deref().unwrap_or("").starts_with("530"));
        assert_eq!(
            outcome
                .steps
                .iter()
                .find(|step| step.id == StepId::Pwd)
                .map(|step| step.status),
            Some(StepStatus::Skipped)
        );
    }

    #[test]
    fn sftp_target_narrows_auth_like_the_reference() {
        let key = build_target(&SftpTarget {
            host: "10.0.0.9".into(),
            username: "deploy".into(),
            key_path: Some("/home/u/.ssh/id_ed25519".into()),
            password: Some("ignored".into()),
            ..SftpTarget::default()
        });
        assert_eq!(key.auth, AuthMethod::Key);
        assert!(key.identity.is_some());

        let password = build_target(&SftpTarget {
            host: "10.0.0.9".into(),
            username: "deploy".into(),
            password: Some("pw".into()),
            ..SftpTarget::default()
        });
        assert_eq!(password.auth, AuthMethod::Password);

        let bare = build_target(&SftpTarget {
            host: "10.0.0.9".into(),
            username: "deploy".into(),
            ..SftpTarget::default()
        });
        assert_eq!(bare.auth, AuthMethod::Auto);
        // 超时通过 `-o ConnectTimeout` 顶掉 `base_argv` 里的默认 10 秒。
        assert!(
            bare.options
                .iter()
                .any(|option| option == "ConnectTimeout=10")
        );
    }

    #[test]
    fn ssh_errors_map_to_auth_connect_and_timeout() {
        let auth = map_ssh(&SshError::Failed {
            status: Some(255),
            stderr: "Permission denied (password).".into(),
        });
        assert_eq!(auth.0, ErrorKind::Auth);
        assert_eq!(auth.1.as_deref(), Some("Permission denied (password)."));

        let refused = map_ssh(&SshError::Failed {
            status: Some(255),
            stderr: "ssh: connect to host 10.0.0.9 port 22: Connection refused".into(),
        });
        assert_eq!(refused.0, ErrorKind::Connect);

        let timeout = map_ssh(&SshError::TimedOut { secs: 30 });
        assert_eq!(timeout.0, ErrorKind::Timeout);
        assert_eq!(timeout.1.as_deref(), Some("30"));
    }
}
