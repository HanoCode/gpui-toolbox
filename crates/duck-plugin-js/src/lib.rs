//! Duck Toolbox 插件 JS 执行轨（QuickJS 沙箱，不依赖 gpui）。
//!
//! 设计取舍（2026-10-11 定案）：原计划接 `gpui-shell`，但核实其 crates.io
//! 只有 0.1.0（0.7.1 快照未发布），且公开 API 全是 View 语义、没有「无 UI
//! 求值」入口——引擎本体（`with_js` / `Ctx`）都在 `pub(crate)` 后面。因此
//! P1 改为直接依赖同一内核谱系的成熟发布版 [`rquickjs`]，自建约 200 行薄封装；
//! 将来 JS 侧要渲染面板时再评估接 gpui-shell 的 ScriptView 轨。
//!
//! 插件契约（`main.js`，ESM）：
//!
//! ```js
//! export function handle(toolId, input) {
//!   duck.notify("处理完成");
//!   return "结果文本";
//! }
//! ```
//!
//! 宿主能力经全局 `duck` 对象暴露，每个函数执行前过
//! [`duck_plugin::CapabilityGranter`] 仲裁（声明 + 授予双闸门，默认拒绝）：
//!
//! - `duck.id` / `duck.dataDir`：插件标识与数据目录（能力 glob 的根）；
//! - `duck.locale`：当前语言 tag（`"en"` / `"zh-Hans"`），在**执行时**由宿主注入，
//!   跟随「外观 → 语言」开关；
//! - `duck.t(source, ...args)`：用**插件自己的** `locales/<tag>.toml` 取词
//!   （装载与降级链见 `duck_plugin::i18n`）；miss 返回源串本身；给了 `args` 就按
//!   顺序替换模板里的 `{}`（语义同宿主 `trf`）。与 `duck.locale` 一样**不加能力
//!   闸门**——它们只在插件自己的包内数据上做纯查表；
//! - `duck.notify(message)`：产出用户提示（执行结束后由宿主统一弹 toast），
//!   需 `notifications` 能力；
//! - `duck.readClipboard()` / `duck.writeClipboard(text)`：剪贴板；
//! - `duck.readFile(rel)` / `duck.writeFile(rel, text)`：受限文件读写，
//!   `rel` 相对 `dataDir`，禁止绝对路径与 `..` 逃逸，且需命中读/写 glob；
//! - `duck.fetch(url, options?)`：受限出网，需命中声明的 `network_hosts`
//!   （逐条精确主机白名单），策略与上限见 [`net`]。
//!
//! 每次调用独立建 Runtime：无跨调用状态（热重载天然成立）、崩溃不影响宿主；
//! 内存 / 栈 / 时长三重上限防失控脚本。

pub mod net;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use duck_plugin::{
    Capabilities, CapabilityGranter, DEFAULT_SOURCE_TAG, PluginError, PluginLocales, PluginManifest,
};

/// 入口文件名（每个插件目录固定一份，与 `plugin.toml` 同级）。
pub const ENTRY_FILE: &str = "main.js";

/// 单次执行的内存上限（32 MiB）。
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
/// 单次执行的栈上限（256 KiB）。
const MAX_STACK_SIZE: usize = 256 * 1024;
/// 单次执行的墙钟上限（10 秒，到点中断脚本）。
const TIME_LIMIT: Duration = Duration::from_secs(10);

/// 一次 JS 工具执行的产出。
#[derive(Debug, Default, Clone)]
pub struct JsToolOutcome {
    /// `handle` 的返回值（工具产出文本）。
    pub output: String,
    /// 插件经 `duck.notify` 产出的用户提示（宿主统一弹 toast）。
    pub notifications: Vec<String>,
    /// 插件经 `console.log` / `warn` / `error` 产出的日志（进宿主日志，不弹 UI）。
    pub logs: Vec<String>,
}

/// 执行期间宿主侧共享的可变状态（通知 / 日志收集 + 能力网关）。
struct ExecState {
    granter: CapabilityGranter,
    notifications: Vec<String>,
    logs: Vec<String>,
}

/// 出网预算的共享句柄。
///
/// **必须跨请求共享**：QuickJS 的 10 秒上限拦不住阻塞中的 Rust 调用，网络这条路的
/// 时长只能由它自己记（详见 [`net`] 模块文档第 1 条）。
///
/// 单独用一个 `RefCell` 而不是塞进 [`ExecState`]：发出请求期间要**可变**借用预算，
/// 而同一时刻闸门闭包还要**只读**借用 `ExecState` 取能力网关。共用一个 `RefCell`
/// 会在第一跳就 `already borrowed` panic——把两份状态分开，借用区间天然不重叠。
type NetBudgetHandle = Rc<RefCell<net::NetBudget>>;

type SharedState = Rc<RefCell<ExecState>>;

/// 执行一个 JS 插件工具。
///
/// # Arguments
/// - `plugin_dir`：插件目录（含 `main.js`）；
/// - `data_dir`：该插件的数据目录（能力 glob 的根，宿主保证已存在）；
/// - `manifest`：清单（插件标识与**能力上限**的来源）；
/// - `granted`：用户授予的能力（安装确认框上勾过的那些）；
/// - `tool_id`：被调用的工具 id（原样传给 `handle`）；
/// - `input`：输入文本（P1 固定空串，插件可自行读剪贴板）；
/// - `lang`：当前语言 tag（如 `"en"` / `"zh-Hans"`）。喂给沙箱里的
///   `duck.locale` 与 `duck.t`，让插件自己产出的文案跟随宿主的语言开关。
///   按**执行时**的语言取值：插件跑到一半用户切了语言，本次提示仍是发起时的语言。
///
/// 真正授予的是 `manifest.capabilities ∩ granted`——两边缺一不可，见
/// [`Capabilities::intersect`]。本函数**不读存储**：授予是宿主的事，这一层不依赖
/// `duck_store`，调用方把算好的结果传进来即可。
///
/// # Errors
/// 入口缺失、脚本异常、能力被拒、超时等均返回 [`PluginError`]。
pub fn run_tool(
    plugin_dir: &Path,
    data_dir: &Path,
    manifest: &PluginManifest,
    granted: &Capabilities,
    tool_id: &str,
    input: &str,
    lang: &str,
) -> Result<JsToolOutcome, PluginError> {
    let entry = plugin_dir.join(ENTRY_FILE);
    let source = std::fs::read_to_string(&entry).map_err(|error| {
        PluginError::Execution(format!("cannot read {}: {error}", entry.display()))
    })?;

    // 翻译表**每次执行重新装载**（而不是缓存）：`locales/` 与 `main.js` 同级、
    // 同属插件源码，`main.js` 改完重启生效的语义应当对译文一样成立 ——
    // 译者改一行 TOML 就能立刻看到，不必等进程重启。
    // 代价是一次目录读 + 若干小文件解析，而工具执行本就是用户点击触发（非每帧）。
    let (source_tag, declared) = match manifest.i18n.as_ref() {
        Some(config) => (config.source_tag(), config.declared_locales()),
        None => (DEFAULT_SOURCE_TAG, None),
    };
    let locales = PluginLocales::load(plugin_dir, source_tag, declared);

    // 截止时刻先算出来：它同时喂给 QuickJS 的中断处理器**和**出网预算。
    // 两者必须同源——引擎的中断在阻塞调用里不触发，出网那条路的时长只能靠
    // 同一份截止时刻自己兜（见 `net` 模块文档第 1 条）。
    let deadline = Instant::now() + TIME_LIMIT;

    let mut granter = CapabilityGranter::new();
    granter.grant_manifest(&manifest.capabilities.intersect(granted));
    let state: SharedState = Rc::new(RefCell::new(ExecState {
        granter,
        notifications: Vec::new(),
        logs: Vec::new(),
    }));
    let budget: NetBudgetHandle = Rc::new(RefCell::new(net::NetBudget::new(deadline)));

    let rt = rquickjs::Runtime::new().map_err(|error| PluginError::Execution(error.to_string()))?;
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(MAX_STACK_SIZE);
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

    let ctx =
        rquickjs::Context::full(&rt).map_err(|error| PluginError::Execution(error.to_string()))?;

    let data_dir_string = data_dir.to_string_lossy().into_owned();
    let outcome = ctx.with(|ctx| -> Result<JsToolOutcome, PluginError> {
        install_host_functions(
            &ctx,
            &manifest.id,
            &data_dir_string,
            lang,
            &locales,
            &state,
            &budget,
        )
        .map_err(|error| js_failure(&ctx, error))?;

        let (module, promise) = rquickjs::Module::declare(ctx.clone(), ENTRY_FILE, source)
            .and_then(|module| module.eval())
            .map_err(|error| js_failure(&ctx, error))?;
        // 顶层 await 的收尾（无 TLA 时是已决 promise，顺手清空任务队列）。
        promise
            .finish::<rquickjs::Value>()
            .map_err(|error| js_failure(&ctx, error))?;

        let handle: rquickjs::Function<'_> = module.get("handle").map_err(|_| {
            PluginError::Execution("main.js must export function handle(toolId, input)".into())
        })?;
        let output: String = handle
            .call((tool_id, input))
            .map_err(|error| js_failure(&ctx, error))?;

        let mut exec = state.borrow_mut();
        Ok(JsToolOutcome {
            output,
            notifications: std::mem::take(&mut exec.notifications),
            logs: std::mem::take(&mut exec.logs),
        })
    })?;
    Ok(outcome)
}

/// 把 Rust 侧失败转成 JS 异常（挂起一条带消息的异常，返回 `Error::Exception`）。
fn js_error(ctx: &rquickjs::Ctx<'_>, message: String) -> rquickjs::Error {
    rquickjs::Exception::throw_message(ctx, &message)
}

/// 把 rquickjs 的失败转成带**可读原因**的 [`PluginError`]。
///
/// `Error::Exception` 的 `Display` 只有一句「Exception generated by QuickJS」——真正的原因
/// （例如 `capability not granted: Notifications`）挂在 `Ctx` 上，必须 `catch()` 取回来。
/// 少了这一步，「权限被拒」与「脚本写错」在调用方看来一模一样，界面上只能弹一句
/// 「插件执行失败」，用户既不知道要去开权限，也不知道该改哪行代码。
///
/// 取过一次即清空挂起异常，所以本函数只能对同一个 `error` 调一次。
fn js_failure(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> PluginError {
    if matches!(error, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(message) = caught
            .as_exception()
            .and_then(|exception| exception.message())
        {
            return PluginError::Execution(message);
        }
    }
    PluginError::Execution(error.to_string())
}

/// 在全局对象上安装 `duck`（宿主能力）与 `console`（日志收集）。
fn install_host_functions<'js>(
    ctx: &rquickjs::Ctx<'js>,
    plugin_id: &str,
    data_dir: &str,
    lang: &str,
    locales: &PluginLocales,
    state: &SharedState,
    budget: &NetBudgetHandle,
) -> Result<(), rquickjs::Error> {
    use rquickjs::function::{Func, Rest};

    let globals = ctx.globals();

    let duck = rquickjs::Object::new(ctx.clone())?;
    duck.set("id", plugin_id)?;
    duck.set("dataDir", data_dir)?;
    // 当前语言 tag 与「用插件自己的 locales/ 取词」。两者都**不加能力闸门**：
    // 它们只在插件自己的包内数据上做纯查表，和 `duck.id` 同级，不触达宿主资源。
    duck.set("locale", lang)?;
    duck.set(
        "t",
        Func::from({
            let locales = locales.clone();
            let lang = lang.to_owned();
            move |source: String, args: Rest<rquickjs::Value>| -> String {
                // miss 返回源串本身（与宿主 `t()` 同一口径）。这一跳走
                // `PluginLocales::text`，与 process / wasm 轨**共用同一处实现**——
                // 各轨各写一遍 `lookup(..).unwrap_or(..)` 迟早会有一条漏成空串。
                let template = locales.text(&lang, &source);
                fill_placeholders(template, args.iter().map(value_to_string))
            }
        }),
    )?;
    duck.set(
        "notify",
        Func::from({
            let state = state.clone();
            move |ctx: rquickjs::Ctx<'_>, message: String| -> rquickjs::Result<()> {
                // 与文件读写一样要过闸门。少了这一道，确认框上的「通知」勾选框
                // 就是个摆设——用户取消勾选之后插件照弹不误。
                let allowed = state
                    .borrow()
                    .granter
                    .require(&duck_plugin::Capability::Notifications);
                if let Err(error) = allowed {
                    return Err(js_error(&ctx, error.to_string()));
                }
                state.borrow_mut().notifications.push(message);
                Ok(())
            }
        }),
    )?;
    duck.set(
        "readClipboard",
        Func::from(|| duck_platform::read_text().unwrap_or_default()),
    )?;
    duck.set(
        "writeClipboard",
        Func::from({
            move |ctx: rquickjs::Ctx<'_>, text: String| {
                duck_platform::write_text(&text).map_err(|reason| js_error(&ctx, reason))
            }
        }),
    )?;
    duck.set(
        "readFile",
        Func::from({
            let state = state.clone();
            let data_dir = data_dir.to_string();
            move |ctx: rquickjs::Ctx<'_>, relative: String| -> rquickjs::Result<String> {
                let path = resolve_sandbox(Path::new(&data_dir), &relative)
                    .map_err(|reason| js_error(&ctx, reason))?;
                if let Err(error) = state.borrow().granter.require_path_read(&relative) {
                    return Err(js_error(&ctx, error.to_string()));
                }
                std::fs::read_to_string(&path).map_err(|error| js_error(&ctx, error.to_string()))
            }
        }),
    )?;
    duck.set(
        "writeFile",
        Func::from({
            let state = state.clone();
            let data_dir = data_dir.to_string();
            move |ctx: rquickjs::Ctx<'_>,
                  relative: String,
                  contents: String|
                  -> rquickjs::Result<()> {
                let path = resolve_sandbox(Path::new(&data_dir), &relative)
                    .map_err(|reason| js_error(&ctx, reason))?;
                if let Err(error) = state.borrow().granter.require_path_write(&relative) {
                    return Err(js_error(&ctx, error.to_string()));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| js_error(&ctx, error.to_string()))?;
                }
                std::fs::write(&path, contents).map_err(|error| js_error(&ctx, error.to_string()))
            }
        }),
    )?;
    duck.set(
        "fetch",
        Func::from(fetch_host(
            state.clone(),
            budget.clone(),
            plugin_id.to_string(),
        )),
    )?;
    globals.set("duck", duck)?;

    let console = rquickjs::Object::new(ctx.clone())?;
    for name in ["log", "info", "warn", "error"] {
        console.set(
            name,
            Func::from({
                let state = state.clone();
                move |values: Rest<rquickjs::Value>| {
                    let text = values
                        .iter()
                        .map(|value| value_to_string(value))
                        .collect::<Vec<_>>()
                        .join(" ");
                    state.borrow_mut().logs.push(text);
                }
            }),
        )?;
    }
    globals.set("console", console)?;
    Ok(())
}

/// 把 `duck.t` 的模板里 `{}` 按 `args` 顺序替换——语义与宿主的
/// `duck_theme::tf` 完全一致（多余的参数忽略、不足时保留未替换的 `{}`）。
///
/// 单开一份而不复用 `duck_theme`：那是 gpui 侧 crate，本 crate 的分层纪律是
/// 「不依赖 gpui」，跨过去只为这 10 行不划算。
fn fill_placeholders(template: &str, args: impl Iterator<Item = String>) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    for arg in args {
        match rest.find("{}") {
            Some(index) => {
                out.push_str(&rest[..index]);
                out.push_str(&arg);
                rest = &rest[index + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// 脚本**不许**自设的请求头。
///
/// 这几个描述的是「这条连接本身」而不是「这次请求的业务内容」，交给脚本等于把传输层
/// 交出去。`host` 尤其：目的地已经由白名单定死，再允许改 `Host` 就能在共享 IP 上
/// 指示虚拟主机路由；`content-length` / `transfer-encoding` 同理，交给 `ureq` 自己算。
const RESERVED_HEADERS: [&str; 4] = ["host", "content-length", "transfer-encoding", "connection"];

/// `duck.fetch` 的入参（脚本能控制的全部东西）。
struct FetchOptions {
    method: ureq::http::Method,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    /// 单请求超时上限。只能**收紧**不能放宽——放宽的那部分由执行预算兜着。
    timeout_cap: Option<Duration>,
}

impl FetchOptions {
    /// 解析第二个参数（可省略）。
    fn parse<'js>(
        ctx: &rquickjs::Ctx<'js>,
        options: Option<rquickjs::Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let mut parsed = Self {
            method: ureq::http::Method::GET,
            headers: Vec::new(),
            body: None,
            timeout_cap: None,
        };
        // 两种「没给」都要认：整个参数省略（`Opt` 给出 `None`），以及显式写
        // `null` / `undefined`。前者是 `duck.fetch(url)`，后者是 `duck.fetch(url, null)`。
        let Some(options) = options.filter(|value| !value.is_undefined() && !value.is_null())
        else {
            return Ok(parsed);
        };
        if !options.is_object() {
            return Err(js_error(ctx, "duck.fetch options must be an object".into()));
        }
        let object = rquickjs::Object::from_value(options)
            .map_err(|error| js_error(ctx, error.to_string()))?;

        if let Some(method) = object.get::<_, Option<String>>("method")? {
            parsed.method = parse_method(&method).map_err(|reason| js_error(ctx, reason))?;
        }
        if let Some(body) = object.get::<_, Option<String>>("body")? {
            parsed.body = Some(body.into_bytes());
        }
        if let Some(timeout) = object.get::<_, Option<f64>>("timeoutMs")? {
            parsed.timeout_cap = Some(clamp_timeout(timeout));
        }
        let headers: Option<rquickjs::Object<'js>> = object.get("headers")?;
        if let Some(headers) = headers {
            for entry in headers.props::<String, String>() {
                let (name, value) = entry?;
                if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                    return Err(js_error(
                        ctx,
                        format!("header {name:?} cannot be set by a plugin"),
                    ));
                }
                parsed.headers.push((name, value));
            }
        }
        Ok(parsed)
    }
}

/// 脚本能用的 HTTP 方法白名单。
///
/// 刻意不开放任意方法：`ureq` 默认也拒非标准方法，而放行一个我们没想过的动词
/// （`TRACE` / `CONNECT`）没有收益、只有面。
fn parse_method(raw: &str) -> Result<ureq::http::Method, String> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Ok(ureq::http::Method::GET),
        "POST" => Ok(ureq::http::Method::POST),
        "PUT" => Ok(ureq::http::Method::PUT),
        "PATCH" => Ok(ureq::http::Method::PATCH),
        "DELETE" => Ok(ureq::http::Method::DELETE),
        "HEAD" => Ok(ureq::http::Method::HEAD),
        other => Err(format!(
            "unsupported method {other:?} (GET / POST / PUT / PATCH / DELETE / HEAD)"
        )),
    }
}

/// 把脚本给的超时夹进 `[100ms, REQUEST_TIMEOUT]`。
///
/// 下限不是为了保护脚本（它想立刻失败随它），而是别让 `0` / `NaN` 这种值进到
/// `ureq` 的超时配置里——那会变成一个「总是立刻超时」的诡异端点。
fn clamp_timeout(millis: f64) -> Duration {
    if !millis.is_finite() || millis <= 0.0 {
        return Duration::from_millis(100);
    }
    let ceiling = net::REQUEST_TIMEOUT.as_millis() as f64;
    Duration::from_millis(millis.min(ceiling) as u64)
}

/// 造出 `duck.fetch` 这个宿主函数。
///
/// 单独抽成一个泛型函数，是因为**闭包没法自己声明生命周期**：直接写
/// `move |ctx: Ctx<'_>, url: String, options: Value<'_>| -> Result<Object<'_>>`
/// 会被展开成三个互不相干的省略生命周期，而 `Object<'js>` 对 `'js` 是**不变**的
/// （invariant），编译器无法把它们统一，只能报 "lifetime may not live long enough"。
/// 放进泛型函数后 `'js` 是具名的，闭包不再是 higher-ranked，三个位置自然同源。
///
/// 第三个参数用 [`rquickjs::function::Opt`] 而不是 `Option<T>`：这个版本里后者走的是
/// 「必需参数」那条实现，`duck.fetch(url)` 会直接在 JS 侧报
/// `TypeError: ... 1 argument(s) while 2 where expected`，连宿主函数都进不去。
fn fetch_host<'js>(
    state: SharedState,
    budget: NetBudgetHandle,
    plugin_id: String,
) -> impl Fn(
    rquickjs::Ctx<'js>,
    String,
    rquickjs::function::Opt<rquickjs::Value<'js>>,
) -> rquickjs::Result<rquickjs::Object<'js>>
+ 'js {
    move |ctx, url, options| run_fetch(&ctx, &state, &budget, &plugin_id, &url, options.0)
}

/// `duck.fetch` 的实现：解析参数 → 过闸门 → 出网 → 组装返回对象。
fn run_fetch<'js>(
    ctx: &rquickjs::Ctx<'js>,
    state: &SharedState,
    budget: &NetBudgetHandle,
    plugin_id: &str,
    url: &str,
    options: Option<rquickjs::Value<'js>>,
) -> rquickjs::Result<rquickjs::Object<'js>> {
    let options = FetchOptions::parse(ctx, options)?;
    let target = net::parse_target(url).map_err(|reason| js_error(ctx, reason))?;
    let host = target.host().to_string();
    let port = target.port();

    // 闸门绑成闭包交给网络层：重定向的**每一跳**都要重新问一次，
    // 于是「跨域重定向」不需要单独写规则——新 host 本来就不在授予里。
    let allow = |host: &str, port: u16| -> Result<(), String> {
        state
            .borrow()
            .granter
            .require_host(host, port)
            .map_err(|error| error.to_string())
    };

    let outcome = {
        let mut budget = budget.borrow_mut();
        net::fetch(
            &mut budget,
            &options.method,
            &target,
            &options.headers,
            options.body.as_deref(),
            options.timeout_cap,
            &allow,
        )
    };

    match outcome {
        Ok(reply) => {
            // 落笔口径：**只记方法与目的地与结果，绝不记 header 与 body**。
            // 下一项的凭据就挂在 header 上，记了等于把 token 抄进日志文件
            //（而日志是要被导出、被贴进 issue 的）。审计流水（duck-app 的 `audit`）
            // 沿用同一条口径：应用层只记「哪个插件、跑的是哪个工具」。
            tracing::info!(
                target: "duck::plugin::net",
                plugin = plugin_id,
                method = %options.method,
                host = %host,
                port,
                status = reply.status,
                bytes = reply.body.len(),
                "plugin network request"
            );
            fetch_reply_object(ctx, reply)
        }
        Err(reason) => {
            tracing::warn!(
                target: "duck::plugin::net",
                plugin = plugin_id,
                method = %options.method,
                host = %host,
                port,
                reason = %reason,
                "plugin network request refused"
            );
            Err(js_error(ctx, reason))
        }
    }
}

/// 把响应组装成 JS 对象 `{status, ok, headers, body}`。
///
/// 同名响应头（如多个 `set-cookie`）只保留最后一个：这一层不做多值语义，
/// 需要多值的话是后续再把 `headers` 换成数组的事，不在这里猜。
fn fetch_reply_object<'js>(
    ctx: &rquickjs::Ctx<'js>,
    reply: net::Reply,
) -> rquickjs::Result<rquickjs::Object<'js>> {
    let result = rquickjs::Object::new(ctx.clone())?;
    result.set("status", reply.status)?;
    result.set("ok", (200..300).contains(&reply.status))?;
    let headers = rquickjs::Object::new(ctx.clone())?;
    for (name, value) in reply.headers {
        headers.set(name, value)?;
    }
    result.set("headers", headers)?;
    result.set("body", reply.body)?;
    Ok(result)
}

/// 沙箱路径解析：拒绝绝对路径与 `..` 逃逸，返回数据目录内的绝对路径。
fn resolve_sandbox(data_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.trim().is_empty() {
        return Err("path must not be empty".into());
    }
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        return Err(format!(
            "absolute path {relative:?} is outside the plugin sandbox"
        ));
    }
    if relative_path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!("path {relative:?} must not contain '..'"));
    }
    Ok(data_dir.join(relative_path))
}

/// `rquickjs::Value` → 调试字符串（console.log 收集用；非字符串值降级为占位）。
fn value_to_string(value: &rquickjs::Value) -> String {
    value
        .as_string()
        .and_then(|string| string.to_string().ok())
        .unwrap_or_else(|| "unserializable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use duck_plugin::{PluginError, PluginManifest};

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn sample_manifest() -> PluginManifest {
        manifest_of("sample")
    }

    /// 读某个 fixture 的 `plugin.toml`。
    fn manifest_of(name: &str) -> PluginManifest {
        std::fs::read_to_string(fixtures_dir().join(name).join("plugin.toml"))
            .map_err(PluginError::Io)
            .and_then(|source| PluginManifest::parse(&source))
            .unwrap_or_else(|error| panic!("{name} 的清单必须解析通过：{error}"))
    }

    /// `duck.locale` / `duck.t` 跟随宿主注入的语言 tag：
    /// 命中取译文、没登记的源串原样返回、`...args` 按顺序填 `{}`。
    ///
    /// 这是「插件文案跟随语言开关」在 JS 轨上的端到端证据：译文来自
    /// **插件自己的** `locales/<tag>.toml`，与宿主 `locales/en.toml` 无关。
    #[test]
    fn duck_t_follows_the_injected_locale() {
        let dir = fixtures_dir().join("i18n");
        let manifest = manifest_of("i18n");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // 取词是纯查表，不需要任何能力。
        let granted = Capabilities::default();

        let english = run_tool(
            &dir,
            &data,
            &manifest,
            &granted,
            "dev.duck.i18n.stats",
            "",
            "en",
        )
        .expect("must run");
        assert_eq!(
            english.output,
            "en|Text stats|Report written to out.txt|这条没翻"
        );

        let chinese = run_tool(
            &dir,
            &data,
            &manifest,
            &granted,
            "dev.duck.i18n.stats",
            "",
            "zh-Hans",
        )
        .expect("must run");
        assert_eq!(
            chinese.output,
            "zh-Hans|文本统计|报告已写入 out.txt|这条没翻"
        );
    }

    #[test]
    fn executes_tool_and_collects_side_effects() {
        let manifest = sample_manifest();
        let dir = fixtures_dir().join("sample");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();

        let granted = manifest.capabilities.clone();
        let outcome = run_tool(
            &dir,
            &data,
            &manifest,
            &granted,
            "dev.duck.sample.hash",
            "duck",
            "zh-Hans",
        )
        .expect("must run");
        assert_eq!(outcome.output, "handled: dev.duck.sample.hash:duck");
        assert_eq!(outcome.notifications, ["已处理 1 项"]);
        assert!(outcome.logs.iter().any(|log| log.contains("sample tool")));
    }

    #[test]
    fn missing_handle_export_is_an_error() {
        let manifest = sample_manifest();
        let dir = fixtures_dir().join("no-handle");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let granted = manifest.capabilities.clone();
        let error = run_tool(
            &dir,
            &data,
            &manifest,
            &granted,
            "dev.duck.sample.hash",
            "",
            "zh-Hans",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("handle"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn ungranted_path_is_denied() {
        let manifest = sample_manifest(); // 只声明了 data/* 读 + 通知
        let dir = fixtures_dir().join("sample");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let granted = manifest.capabilities.clone();
        let outcome = run_tool(
            &dir,
            &data,
            &manifest,
            &granted,
            "dev.duck.sample.hash",
            "deny",
            "zh-Hans",
        )
        .expect("must run");
        assert!(
            outcome.output.contains("denied"),
            "expected denial marker, got: {outcome:?}"
        );
    }

    #[test]
    fn sandbox_rejects_escape_paths() {
        let data = Path::new("/tmp/duck-plugin-data");
        for path in ["/etc/passwd", "../secret", "a/../../secret", ""] {
            assert!(
                resolve_sandbox(data, path).is_err(),
                "path {path:?} must be rejected"
            );
        }
        assert!(resolve_sandbox(data, "data/a.txt").is_ok());
    }
}
