//! `duck.fetch` 的端到端契约：跑在**真实 socket** 上，不是 mock。
//!
//! 为什么非得起真服务：这一项要证明的事情里，有一半只有真连接才能证伪——
//! 「闸门拒绝时**一个字节都没出去**」、「重定向的**第二跳**也要过白名单」、
//! 「响应体超限会报错而不是悄悄截断」。纯函数单测（见 `net.rs` 的 `tests`）
//! 只能证明判定逻辑对，证明不了它接在了发请求之前。
//!
//! 服务端手写而不是引 HTTP 测试框架：要断言的恰恰是「真实 socket 上的真实字节」，
//! 手写一个 60 行的回环服务比引依赖更直白，也不用跟框架的连接复用语义较劲。
//! 每个响应都带 `Connection: close`，免得 `ureq` 复用连接、把我们「一次连接一次应答」
//! 的服务假设打乱。

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use duck_plugin::PluginManifest;

/// 一次测试用的插件目录：清单里的 `network_hosts` 按调用方给的写。
struct Harness {
    dir: PathBuf,
    data_dir: PathBuf,
    manifest: PluginManifest,
}

const TOOL_ID: &str = "dev.duck.nettest.fetch";

/// 造一个临时插件目录。
///
/// 插件目录在**运行时**写，而不是放进 `tests/fixtures/`：白名单里必须写死一个具体
/// 端口，而端口是内核分配的（`:0`），静态清单写不出来。写死一个固定端口号则会与
/// 并行跑的其它测试、以及开发机上恰好占用它的进程打架。
fn harness(tag: &str, hosts: &[String], script: &str) -> Harness {
    let root = std::env::temp_dir().join(format!("duck-plugin-net-{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).expect("create plugin data dir");

    let hosts = hosts
        .iter()
        .map(|host| format!("{host:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest_src = format!(
        "id = \"dev.duck.nettest\"\n\
         name = \"网络测试\"\n\
         version = \"0.1.0\"\n\
         api_version = 1\n\
         execution_model = \"js\"\n\
         \n\
         [capabilities]\n\
         network_hosts = [{hosts}]\n\
         \n\
         [[tools]]\n\
         id = \"{TOOL_ID}\"\n\
         name = \"fetch\"\n"
    );
    std::fs::write(root.join("plugin.toml"), &manifest_src).expect("write manifest");
    std::fs::write(root.join("main.js"), script).expect("write entry");

    let manifest = PluginManifest::parse(&manifest_src).expect("test manifest must parse");
    Harness {
        dir: root,
        data_dir,
        manifest,
    }
}

impl Harness {
    /// 按声明全授跑一次工具，返回 JS 产出文本。
    fn run(&self) -> Result<String, String> {
        let granted = self.manifest.capabilities.clone();
        duck_plugin_js::run_tool(
            &self.dir,
            &self.data_dir,
            &self.manifest,
            &granted,
            TOOL_ID,
            "",
            "zh-Hans",
        )
        .map(|outcome| outcome.output)
        .map_err(|error| error.to_string())
    }
}

/// 起一个回环 HTTP 服务，最多应答 `count` 次请求，返回 `(端口, 命中计数)`。
///
/// 非阻塞 + 截止时间：用来断言「一次连接都不该发生」的那些用例里，服务线程不能
/// 一直堵在 `accept` 上——那会泄漏线程，也会让测试骨架带着一个半死线程往下跑。
fn spawn_server(
    count: usize,
    respond: impl Fn(&str) -> String + Send + 'static,
) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("read local addr").port();
    listener.set_nonblocking(true).expect("set nonblocking");
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.load(Ordering::SeqCst) < count && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    seen.fetch_add(1, Ordering::SeqCst);
                    // 显式切回阻塞：listener 是非阻塞的，不澄清这一点，下面那次
                    // 「留住连接」的读会立刻返回 `WouldBlock`，等于没留。
                    let _ = stream.set_nonblocking(false);

                    let mut buffer = [0u8; 8192];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();

                    // 写失败不算错：客户端读满上限就会断，断得早有 `Connection: close` 兜着。
                    let _ = stream.write_all(respond(&path).as_bytes());
                    let _ = stream.flush();

                    // 写完**不能立刻关**。客户端可能还没读完，这时关连接会让它先撞上
                    // RST（表现成 `io: Peer disconnected`），于是「是否报了超限」就变成了
                    // 「谁先把连接关掉」——一个与被测行为无关的竞态，而且它会**掩盖**
                    // 真正要验证的那条兜底。挂住直到客户端自己断开，把关闭时机交还客户端。
                    let _ = stream.read(&mut buffer);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (port, hits)
}

fn ok_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// 分块传输的响应：**没有** `Content-Length`。
///
/// 这条路径才是 `limit()` 真正的用武之地——没有声明长度就只能在边读边数，
/// 也正好是流式接口（真实世界里最会长成巨型响应的那类）的样子。
fn chunked_response(chunk: usize, count: usize) -> String {
    let payload = "y".repeat(chunk);
    let mut response = String::from(
        "HTTP/1.1 200 OK\r\n\
         Transfer-Encoding: chunked\r\n\
         Content-Type: text/plain\r\n\
         Connection: close\r\n\
         \r\n",
    );
    for _ in 0..count {
        response.push_str(&format!("{:x}\r\n{payload}\r\n", payload.len()));
    }
    response.push_str("0\r\n\r\n");
    response
}

fn redirect_response(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    )
}

/// 插件脚本：请求 `url`，把状态码与正文拼进产出（不抛，好断言文字）。
fn fetch_script(url: &str) -> String {
    format!(
        "export function handle(toolId, input) {{\n\
         \x20 try {{\n\
         \x20   const reply = duck.fetch({url:?});\n\
         \x20   return \"done \" + reply.status + \" \" + reply.body;\n\
         \x20 }} catch (error) {{\n\
         \x20   return \"refused: \" + error;\n\
         \x20 }}\n\
         }}\n"
    )
}

/// 白名单里有这台主机（回环 + 明文口子）→ 请求真的发出去、真的拿到正文。
#[test]
fn a_granted_host_is_actually_reached() {
    let (port, hits) = spawn_server(1, |_| ok_response("hello"));
    let h = harness(
        "granted",
        &[format!("127.0.0.1:{port}")],
        &fetch_script(&format!("http://127.0.0.1:{port}/ok")),
    );

    let output = h.run().expect("gated fetch must succeed");
    assert_eq!(output, "done 200 hello", "拿到的应当是真实响应");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "服务端应当被连到一次");
}

/// 白名单里没有这台主机 → 报可读原因，且**一个字节都没出去**。
///
/// 后半句是这条测试的重点：只看「返回了 refused」证明不了闸门接在发请求之前，
/// 也可能请求发了、响应丢了才报的错。计数为 0 才证明是**先拦后发**。
#[test]
fn an_undeclared_host_is_refused_before_any_socket_is_opened() {
    let (port, hits) = spawn_server(1, |_| ok_response("should never be served"));
    // 清单里声明的是一台与回环无关的主机。
    let h = harness(
        "undeclared",
        &["api.example.com".to_string()],
        &fetch_script(&format!("http://127.0.0.1:{port}/ok")),
    );

    let output = h.run().expect("脚本自己 catch 了，宿主不该报错");
    assert!(output.starts_with("refused:"), "应当被拒：{output}");
    assert!(
        output.contains("not granted"),
        "原因要能读懂（用户要靠它知道去授权）：{output}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "闸门必须在建连之前生效——服务端一次都不该被连到"
    );
}

/// 白名单放行的是这台主机、但**端口不对** → 也拒。端口是白名单的一部分。
#[test]
fn a_granted_host_on_another_port_is_refused() {
    let (port, hits) = spawn_server(1, |_| ok_response("nope"));
    let h = harness(
        "wrong-port",
        &[format!("127.0.0.1:{}", port + 1)],
        &fetch_script(&format!("http://127.0.0.1:{port}/ok")),
    );

    let output = h.run().expect("脚本自己 catch 了");
    assert!(
        output.contains("not granted"),
        "端口不匹配必须被拒：{output}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

/// 同主机重定向 → 跟着走（第二跳仍在白名单内）。
#[test]
fn a_same_host_redirect_is_followed() {
    let (final_port, final_hits) = spawn_server(1, |_| ok_response("final"));
    let target = format!("http://127.0.0.1:{final_port}/final");
    let (redirect_port, redirect_hits) = spawn_server(1, move |_| redirect_response(&target));

    // 裸 host 条目：放行这台主机的任意端口，于是第二跳也过闸门。
    let h = harness(
        "same-host-redirect",
        &["127.0.0.1".to_string()],
        &fetch_script(&format!("http://127.0.0.1:{redirect_port}/start")),
    );

    let output = h.run().expect("同主机重定向必须走得通");
    assert_eq!(output, "done 200 final", "应当拿到终点响应：{output}");
    assert_eq!(redirect_hits.load(Ordering::SeqCst), 1);
    assert_eq!(final_hits.load(Ordering::SeqCst), 1);
}

/// 重定向到**没授权**的端口 → 第二跳被拦。
///
/// 这条盯的是「跨域重定向」的真实语义：拦它的不是一条单独的跨域规则，而是
/// **每一跳都重过一遍白名单**。端口不同就已经是另一个目标了。
#[test]
fn a_redirect_outside_the_allowlist_is_blocked() {
    let (escape_port, escape_hits) = spawn_server(1, |_| ok_response("escaped"));
    let target = format!("http://127.0.0.1:{escape_port}/escaped");
    let (start_port, start_hits) = spawn_server(1, move |_| redirect_response(&target));

    let h = harness(
        "cross-host-redirect",
        &[format!("127.0.0.1:{start_port}")],
        &fetch_script(&format!("http://127.0.0.1:{start_port}/start")),
    );

    let output = h.run().expect("脚本自己 catch 了");
    assert!(output.starts_with("refused:"), "第二跳应当被拒：{output}");
    assert!(output.contains("not granted"), "{output}");
    assert_eq!(start_hits.load(Ordering::SeqCst), 1, "第一跳是放行的");
    assert_eq!(
        escape_hits.load(Ordering::SeqCst),
        0,
        "越出白名单的那一跳一个字节都不该出去"
    );
}

/// 响应**声明**了超限长度 → 在下载之前就拒。
///
/// 截断不行（插件会拿到一段看似合法、实则残缺的 JSON，然后以「解析失败」的样子
/// 报出来，排查方向整个跑偏），读满再拒也不行（白花一次 1 MiB 的流量）。
#[test]
fn a_body_that_declares_itself_oversized_is_refused_before_downloading() {
    let body = "x".repeat(2 * 1024 * 1024);
    let (port, _) = spawn_server(1, move |_| ok_response(&body));
    let h = harness(
        "oversized-declared",
        &[format!("127.0.0.1:{port}")],
        &fetch_script(&format!("http://127.0.0.1:{port}/big")),
    );

    let output = h.run().expect("脚本自己 catch 了");
    assert!(
        output.contains("limit"),
        "原因要说清是超限，而不是普通的读失败：{output}"
    );
    assert!(
        output.contains("declares"),
        "声明长度就该在下载前被拦下：{output}"
    );
    assert!(
        !output.contains("done 200"),
        "超限不该被当成一次成功：{output}"
    );
}

/// 分块响应（没有 `Content-Length`）超限 → 由边读边数的兜底拦下。
///
/// 少了这条，`limit()` 那一层就没被任何测试覆盖——而它恰恰是没有声明长度时
/// **唯一**的防线。
#[test]
fn a_chunked_body_over_the_limit_is_refused_while_reading() {
    let (port, _) = spawn_server(1, move |_| chunked_response(256 * 1024, 6));
    let h = harness(
        "oversized-chunked",
        &[format!("127.0.0.1:{port}")],
        &fetch_script(&format!("http://127.0.0.1:{port}/stream")),
    );

    let output = h.run().expect("脚本自己 catch 了");
    assert!(
        output.contains("limit"),
        "兜底必须报「超限」，不能漏成普通的读失败：{output}"
    );
    assert!(!output.contains("done 200"), "{output}");
}

/// 公网明文 http 在**发请求之前**就被协议策略拦掉（与白名单无关）。
#[test]
fn plain_http_to_a_public_host_is_refused_without_a_whitelist_entry() {
    // 即便白名单里写了它，明文也不该出去——两条策略是 AND，不是 OR。
    let h = harness(
        "plain-http",
        &["api.example.com".to_string()],
        &fetch_script("http://api.example.com/v1"),
    );

    let output = h.run().expect("脚本自己 catch 了");
    assert!(output.contains("plain http"), "原因要说清是明文：{output}");
}
