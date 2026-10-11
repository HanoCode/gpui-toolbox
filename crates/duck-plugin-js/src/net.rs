//! 插件出网原语：受策略约束的 HTTPS 客户端。
//!
//! 只做一件事——「发一个请求，拿回一个响应」，其余全部由策略写死在这里。
//! 插件脚本能控制的只有 method / headers / body / timeoutMs 四项，**不能**控制
//! 协议、端口解析口径、重定向策略、响应体上限。这些一旦交给脚本，能力闸门就只剩
//! 一个装饰性的白名单。
//!
//! ## 三个必须记住的设计约束
//!
//! 1. **超时不能靠 QuickJS 的 10 秒上限。** `run_tool` 用 `set_interrupt_handler`
//!    实现执行上限，而 interrupt 只在**字节码之间**触发。Rust 侧一旦进入阻塞的
//!    socket 调用，引擎拿不回控制权，10 秒上限形同不存在。所以出网自带
//!    [`NetBudget`]（总时长 + 请求条数），且**跨请求共享**——否则插件连发 6 个
//!    请求就能把 10 秒撑成 48 秒，卡住的是宿主的后台执行线程。
//! 2. **重定向必须逐跳自己走。** `ureq` 跟随重定向是「跟完才把结果给你」，
//!    那时字节已经从任意主机出去了，白名单只在第一跳有效。所以配置里
//!    `max_redirects(0)`，把 3xx 原样交回来，由 [`fetch`] 逐跳校验。
//!    副作用是好的：**跨域重定向自动被拦**，因为新 host 本来就不在授予里，
//!    走的还是同一套判定，不需要单独写一条「跨域」规则。
//! 3. **明文 http 只对回环地址开口子。** 插件对接本机服务（本地 API、开发中的
//!    服务）是真实需求，也让 CI 能起本地服务测真实请求路径；但公网走明文，
//!    下一项的凭据（在 header 里）就会漏在链路上。判定在 [`parse_target`]，
//!    用的是自己的规则而**不是** `ureq` 的 `https_only`——后者是全局开关，
//!    没有例外这个档位。
//!
//! 已知且**有意保留**的一点：`ureq` 默认 `Proxy::try_from_env()`，即尊重
//! `HTTPS_PROXY` 等环境变量，本层不覆盖。理由是与 `duck-update` /
//! `duck-plugin-registry` 一致（用户自己的环境变量，不是插件能设置的东西），
//! 且关掉会让企业代理后面的用户整个用不了。这里真正的出网控制是「插件想找哪台
//! 主机」，那一层仍然生效。

use std::time::{Duration, Instant};

use ureq::http::{Method, Uri};

/// 单次请求的墙钟上限（8 秒）。
///
/// 比 [`crate::TIME_LIMIT`]（10 秒）少 2 秒：那 2 秒要留给脚本处理响应、收尾。
/// 用光预算的后果不是「慢一点」，是引擎**再也回不到脚本**。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// 建连超时（5 秒）。建连失败要快，否则一个黑洞地址就吃掉整个预算。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 响应体上限（1 MiB）。超出即报错，**不是**截断——截断会让插件拿到一段
/// 看似合法、实则残缺的 JSON，报错才能让它知道自己该改什么。
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;
/// 一次重定向链最多多少跳。
const MAX_REDIRECTS: usize = 5;
/// 单次执行最多发多少个请求。
///
/// 总时长预算已经能兜住「一直发」，这里再拦一道是因为**失败可以极快**：
/// 连接被拒只要几百微秒，10 秒里能堆出成千上万次连接尝试。挡的是连接洪水，
/// 不是慢请求。
pub const MAX_REQUESTS_PER_RUN: u32 = 32;

const USER_AGENT: &str = concat!("DuckToolbox/", env!("CARGO_PKG_VERSION"), " (plugin)");

/// 一次 JS 执行的出网预算：总时长 + 请求条数，跨请求共享。
///
/// 见模块文档第 1 条：没有它，10 秒执行上限对阻塞的网络调用**完全无效**。
#[derive(Debug)]
pub struct NetBudget {
    deadline: Instant,
    remaining_requests: u32,
}

impl NetBudget {
    /// 按宿主给定的执行截止时刻建预算。
    #[must_use]
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            remaining_requests: MAX_REQUESTS_PER_RUN,
        }
    }

    /// 领一次请求配额，返回这次请求可用的超时。
    ///
    /// `cap` 是脚本自己提的收紧上限（`timeoutMs`），只能**收紧**：它先与
    /// [`REQUEST_TIMEOUT`] 取小，再与剩余预算取小。三道取小的顺序无所谓，
    /// 要紧的是脚本那一项不参与「放宽」。
    ///
    /// # Errors
    /// 条数用尽或时间预算归零时返回可读原因（会原样抛给插件）。
    fn reserve(&mut self, cap: Option<Duration>) -> Result<Duration, String> {
        if self.remaining_requests == 0 {
            return Err(format!(
                "too many requests in one run (limit {MAX_REQUESTS_PER_RUN})"
            ));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the script ran out of its time budget before this request".into());
        }
        self.remaining_requests -= 1;
        Ok(request_timeout(remaining, cap))
    }
}

/// 单请求超时的取值规则：`REQUEST_TIMEOUT`、脚本提的上限、剩余预算**三者取小**。
///
/// 抽成纯函数是为了能脱开时钟测。挂在 [`NetBudget::reserve`] 里测的话，断言免不了
/// 依赖「构造到断言之间没被调度走」——300ms 的预算窗口在负载高的机器上会被跨过去，
/// 于是测试偶发失败，而失败信息指向的是被测行为（超时算错），不是真实原因。
/// 时间相关的偶发失败最费时间，因为它们看起来像真 bug。
fn request_timeout(remaining: Duration, cap: Option<Duration>) -> Duration {
    let ceiling = cap.map_or(REQUEST_TIMEOUT, |cap| REQUEST_TIMEOUT.min(cap));
    // 与剩余预算取小：不能让一次请求吃掉整个脚本剩下的时间。
    ceiling.min(remaining)
}

/// 一个通过策略校验的出网目标。
#[derive(Debug, Clone)]
pub struct Target {
    uri: Uri,
    authority: String,
    /// 归一化后的小写主机名（IPv6 不含方括号）。
    host: String,
    port: u16,
}

impl Target {
    /// 归一化后的主机名（闸门用它比对）。
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 解析后的具体端口（默认端口已补上）。
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// 一次成功完成的请求。
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应头（原样，含重定向后的最终响应）。
    pub headers: Vec<(String, String)>,
    /// 响应体（UTF-8；非法字节按替换字符处理）。
    pub body: String,
}

/// 把脚本给的 URL 解析成出网目标，并施加协议策略。
///
/// 这是**唯一**决定「能不能出去」的地方；[`fetch`] 的每一跳都要重新过它，
/// 所以「禁止访问非 http(s)」「明文只给回环」两条没有绕路。
///
/// # Errors
/// 非绝对 URL、缺 scheme / host、协议不受支持、公网明文 http，各自返回可读原因。
pub fn parse_target(raw: &str) -> Result<Target, String> {
    let uri: Uri = raw
        .parse()
        .map_err(|_| format!("not a valid absolute url: {raw:?}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| format!("missing scheme in {raw:?}"))?;
    let host = uri
        .host()
        .ok_or_else(|| format!("missing host in {raw:?}"))?
        .to_string();
    match scheme {
        "https" => {}
        "http" if is_loopback(&host) => {}
        "http" => {
            return Err(format!(
                "plain http is only allowed for loopback hosts, got {host:?} \
                 (use https, or a 127.0.0.1 / ::1 / localhost address)"
            ));
        }
        other => {
            return Err(format!(
                "unsupported scheme {other:?} (only https, plus http on loopback)"
            ));
        }
    }
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let authority = uri
        .authority()
        .map(ToString::to_string)
        .unwrap_or_else(|| host.clone());
    Ok(Target {
        uri,
        authority,
        host: trim_brackets(&host).to_ascii_lowercase(),
        port,
    })
}

/// 回环地址判定：`127.0.0.0/8`、`::1`、以及字面量 `localhost`。
///
/// `localhost` 单独列进来是因为它**不是** IP 字面量，解析不出 `IpAddr`；
/// 而真要把它解析成地址又得查 DNS，那是网络行为，不该出现在纯判定里。
fn is_loopback(host: &str) -> bool {
    let bare = trim_brackets(host);
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// 剥掉 IPv6 字面量的方括号（`http::Uri` 给出的是带括号形式）。
fn trim_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

/// 建一个受约束的 agent。
///
/// 每次请求新建一个：出网是稀罕事，为它养一个连接池不划算，也就没有跨请求的
/// 连接复用需要管（复用还会让「换主机」这件事更难审计）。
fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT.min(timeout)))
        // 重定向自己走，见模块文档第 2 条。`will_error(false)` 是必须的：
        // 默认 `max_redirects(0)` 之外还有「超限即报错」那一层，我们要的是把
        // 3xx 原样交回来。
        .max_redirects(0)
        .max_redirects_will_error(false)
        // 4xx / 5xx 当正常响应交给插件：401 / 404 是它要处理的业务结果，
        // 宿主不该替它决定这是「错误」。这也让返回值形状对所有状态码一致。
        .http_status_as_error(false)
        // 刻意不开 `https_only`：协议策略在 `parse_target` 里（要给回环留口子）。
        .build();
    ureq::Agent::new_with_config(config)
}

/// 发一个请求，逐跳跟随重定向，每跳都过 `allow`。
///
/// `allow(host, port)` 就是 [`duck_plugin::CapabilityGranter::require_host`] 的
/// 绑定；传闭包而不是传闸门本身，是为了让这一层不依赖能力模型、可以脱离
/// QuickJS 与清单单独测。
///
/// # Errors
/// 闸门拒绝、预算耗尽、网络失败、响应超限，各自返回可读原因。
pub fn fetch(
    budget: &mut NetBudget,
    method: &Method,
    start: &Target,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    timeout_cap: Option<Duration>,
    allow: &dyn Fn(&str, u16) -> Result<(), String>,
) -> Result<Reply, String> {
    let mut current = start.clone();
    let mut hops = 0usize;
    loop {
        // 每一跳都重新过闸门——包括第一跳。跨域重定向就是在这里被拦下的。
        allow(current.host(), current.port())?;

        let timeout = budget.reserve(timeout_cap)?;
        let response = send(&agent(timeout), method, &current, headers, body)?;
        let status = response.status().as_u16();

        if let Some(next) = redirect_target(status, &response, &current) {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(format!(
                    "too many redirects (limit {MAX_REDIRECTS}) starting at {}",
                    start.uri
                ));
            }
            current = next;
            continue;
        }

        // 先看声明的长度：能提前拒的就不下载。真实场景里超限的多数是「自己声明了
        // 大小的巨大响应」，等读满 1 MiB 再报错等于白下 1 MiB——对一个 100 MB 的
        // 文件，那 1 MiB 也算钱。
        if let Some(declared) = declared_length(&response)
            && declared > MAX_BODY_BYTES
        {
            return Err(format!(
                "response body declares {declared} bytes, over the {MAX_BODY_BYTES} byte limit"
            ));
        }

        let mut response = response;
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(describe_body_error)?;
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect();
        return Ok(Reply {
            status,
            headers,
            body: text,
        });
    }
}

/// 响应头里声明的正文长度（`Content-Length` 存在且是数字时才有）。
fn declared_length(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get(ureq::http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 把「读响应体失败」翻成可读原因。
///
/// 限长那条错误在上游有**两种到达方式**：直接是 [`ureq::Error::BodyExceedsLimit`]，
/// 或者被 [`ureq::Error::BodyExceedsLimit::into_io`] 包成 IO 错误再带上来。只认其中
/// 一条，超限就会在界面与日志里显示成一次莫名的 IO 失败——用户以为网络坏了，
/// 实际上是响应太大，排查方向整个跑偏。
fn describe_body_error(error: ureq::Error) -> String {
    if let ureq::Error::BodyExceedsLimit(limit) = error {
        return format!("response body exceeds the {limit} byte limit");
    }
    let text = error.to_string();
    if text.contains("request limit") {
        return format!("response body exceeds the {MAX_BODY_BYTES} byte limit");
    }
    format!("cannot read response body: {text}")
}

/// 发一跳（不做任何策略判断，调用方保证目标已校验过）。
fn send(
    agent: &ureq::Agent,
    method: &Method,
    target: &Target,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut request = ureq::http::Request::builder()
        .method(method.clone())
        .uri(target.uri.clone());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let request = request
        .body(body.map(<[u8]>::to_vec).unwrap_or_default())
        .map_err(|error| format!("cannot build request: {error}"))?;
    agent.run(request).map_err(|error| error.to_string())
}

/// 若这一跳是重定向，解析出下一跳目标。
///
/// 只认带 `Location` 的 3xx。**不在这里判「跨域」**——下一跳照样过闸门，
/// 新 host 不在授予里自然过不去；多写一条跨域规则只会多一个可能写错的地方。
fn redirect_target(
    status: u16,
    response: &ureq::http::Response<ureq::Body>,
    base: &Target,
) -> Option<Target> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response
        .headers()
        .get(ureq::http::header::LOCATION)?
        .to_str()
        .ok()?;
    resolve_location(base, location).ok()
}

/// 把 `Location` 解析成绝对目标（`Location` 允许相对引用）。
///
/// 解析得不准只会导致**多拒一次**（每次都用同一套闸门校验），不会导致绕过：
/// 主机名无论如何都要过 `parse_target` 的协议策略与 `allow` 的白名单。
fn resolve_location(base: &Target, location: &str) -> Result<Target, String> {
    let location = location.trim();
    let scheme = base.uri.scheme_str().unwrap_or("https");
    let absolute = if location.starts_with("//") {
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        format!("{scheme}://{}{location}", base.authority)
    } else if location.contains("://") {
        location.to_string()
    } else {
        // 相对路径：以 base 的目录为基准拼接。
        let path = base.uri.path();
        let dir = &path[..path.rfind('/').map_or(0, |index| index + 1)];
        format!("{scheme}://{}{dir}{location}", base.authority)
    };
    parse_target(&absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_allowed_and_gets_the_default_port() {
        let target = parse_target("https://api.example.com/v1/items?q=1").expect("must parse");
        assert_eq!(target.host(), "api.example.com");
        assert_eq!(target.port(), 443);
    }

    #[test]
    fn an_explicit_port_survives_parsing() {
        let target = parse_target("https://api.example.com:8443/x").expect("must parse");
        assert_eq!(target.port(), 8443);
    }

    /// 明文 http 只给回环：公网明文会把 header 里的凭据漏在链路上。
    #[test]
    fn plain_http_is_loopback_only() {
        for url in [
            "http://127.0.0.1:8080/x",
            "http://127.1.2.3/x",
            "http://localhost:9000/x",
            "http://[::1]:7000/x",
        ] {
            assert!(parse_target(url).is_ok(), "{url} 应当放行");
        }
        for url in [
            "http://api.example.com/x",
            "http://192.168.1.10/x",
            "http://10.0.0.1/x",
        ] {
            let error = parse_target(url).expect_err("公网明文必须被拒");
            assert!(error.contains("plain http"), "原因要能读懂：{error}");
        }
    }

    /// 公网寻址前缀不能被误判成回环。
    #[test]
    fn a_hostname_that_merely_starts_with_127_is_not_loopback() {
        assert!(parse_target("http://127.0.0.1.evil.test/x").is_err());
        assert!(parse_target("http://localhost.evil.test/x").is_err());
    }

    #[test]
    fn other_schemes_are_rejected() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "data:text/plain,hi",
        ] {
            assert!(parse_target(url).is_err(), "{url} 必须被拒");
        }
        assert!(parse_target("//example.com/x").is_err(), "缺 scheme");
        assert!(parse_target("not a url").is_err());
    }

    #[test]
    fn relative_redirects_stay_on_the_same_authority() {
        let base = parse_target("https://api.example.com/v1/items").expect("must parse");
        for (location, expected) in [
            ("/v2/items", "https://api.example.com/v2/items"),
            ("other", "https://api.example.com/v1/other"),
            ("//api.example.com/x", "https://api.example.com/x"),
        ] {
            let next = resolve_location(&base, location).expect("must resolve");
            assert_eq!(next.uri.to_string(), expected);
            assert_eq!(next.host(), "api.example.com");
        }
    }

    /// 协议相对（`//host/path`）换的是主机——解析出来必须换，好让闸门去拦它。
    #[test]
    fn a_protocol_relative_redirect_changes_the_host() {
        let base = parse_target("https://api.example.com/v1/items").expect("must parse");
        let next = resolve_location(&base, "//evil.test/x").expect("must resolve");
        assert_eq!(next.host(), "evil.test");
        assert_eq!(next.uri.to_string(), "https://evil.test/x");
    }

    #[test]
    fn the_budget_is_shared_and_finite() {
        let mut budget = NetBudget::new(Instant::now() + Duration::from_secs(10));
        for _ in 0..MAX_REQUESTS_PER_RUN {
            assert!(budget.reserve(None).is_ok());
        }
        let error = budget.reserve(None).expect_err("条数用尽必须拒绝");
        assert!(error.contains("too many requests"), "{error}");
    }

    /// 单请求超时是三个上限里的最小值；脚本那一项只能**收紧**不能放宽。
    ///
    /// 刻意不碰时钟：见 [`request_timeout`] 的注释——挂在真实预算上断言，
    /// 窗口一被调度跨过去就成了偶发失败。
    #[test]
    fn the_request_timeout_is_the_smallest_of_the_three_limits() {
        let ample = Duration::from_secs(600);
        // 预算充足、脚本没提 → 单请求上限。
        assert_eq!(request_timeout(ample, None), REQUEST_TIMEOUT);
        // 剩余预算更小 → 跟预算走（一次请求不能吃掉脚本剩下的时间）。
        assert_eq!(
            request_timeout(Duration::from_millis(300), None),
            Duration::from_millis(300)
        );
        // 脚本提得更紧 → 跟脚本走。
        assert_eq!(
            request_timeout(ample, Some(Duration::from_millis(500))),
            Duration::from_millis(500)
        );
        // 脚本想放宽 → 顶回单请求上限，放宽的那部分不生效。
        assert_eq!(
            request_timeout(ample, Some(Duration::from_secs(300))),
            REQUEST_TIMEOUT
        );
    }

    #[test]
    fn an_expired_budget_refuses_before_opening_a_socket() {
        let mut budget = NetBudget::new(Instant::now());
        let error = budget.reserve(None).expect_err("预算归零必须拒绝");
        assert!(error.contains("time budget"), "{error}");
    }
}
