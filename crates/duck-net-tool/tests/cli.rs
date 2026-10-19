//! process 轨端到端：真起一个 `duck-net-tool` 进程，走 stdin/stdout JSONL。
//!
//! 补的是「单测测不到、链路最容易断」的那一截：宿主用 `duck_plugin_process::run`
//! 送请求、插件进程用 `serve` 收请求，两边对**协议**与**分派表**（`tool` → 哪个内核
//! 函数、哪个参数名）的理解必须一致。参数名拼错、显隐写反、报告字段算错，
//! 这些都只有真跑一次才会现形。
//!
//! 算法本身的正确性由 `duck-net-core` 的 47 条单测保证，这里只钉端到端的形状：
//! 真发了请求 / 真收到了帧 / 真的写出了导出文件 / 失败时报错且不静默。
//!
//! `plugin_dir = "."` 让 locale 环境变量指向不存在的目录，取词回落到源串——所以下面
//! 断言的都是中文源串（本地化由 `plugin_locales.py` 与执行页覆盖）。
//!
//! ## 为什么不打外网
//!
//! HTTP 与报文两条都用**本地起的假服务**：测试在 CI 上跑，不该依赖外网可达性，
//! 也不该把随机域名写进断言（DNS 一抽风就红）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use duck_plugin_process::{CancelHandle, NullSink, RunRequest, run};

fn request(tool: &str, params: serde_json::Value) -> RunRequest {
    RunRequest {
        tool: tool.into(),
        input: String::new(),
        params,
        files: vec![],
        lang: "zh-Hans".into(),
        kv: Default::default(),
        // 插件私有数据目录（`<app_data>/plugin-data/<id>`）：本族没有有状态工具，留空。
        data: String::new(),
        // CLI 直跑一次正常执行，不走页内扫描。
        action: None,
    }
}

fn request_with_input(tool: &str, input: &str, params: serde_json::Value) -> RunRequest {
    RunRequest {
        input: input.into(),
        ..request(tool, params)
    }
}

fn run_once(request: &RunRequest) -> Result<String, String> {
    let bin = env!("CARGO_BIN_EXE_duck-net-tool");
    run(
        Path::new("."),
        Path::new(bin),
        request,
        &mut NullSink,
        &CancelHandle::new(),
    )
    .map(|outcome| outcome.output)
    .map_err(|error| error.to_string())
}

/// 一个只属于本测试的目录（名字带进程 id，并行跑互不踩）。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-net-tool-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn write(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("建父目录");
    }
    std::fs::write(&path, text).expect("写文件");
    path
}

/// 起一个「读一行请求、写一段固定响应、然后关闭」的假 HTTP 服务。
fn fake_http_server(response: impl Into<String>) -> (u16, std::thread::JoinHandle<String>) {
    let response = response.into();
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
    let port = listener.local_addr().expect("地址").port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        // 把请求头读干净（到空行为止），测试要断言 Authorization 有没有到。
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
        let mut writer = stream;
        let _ = writer.write_all(response.as_bytes());
        let _ = writer.flush();
        head
    });
    (port, handle)
}

// ── 分派表 ─────────────────────────────────────────────────────────────

#[test]
fn unknown_tool_is_rejected_instead_of_silently_passing() {
    let error =
        run_once(&request("duck.net.nope", serde_json::json!({}))).expect_err("未知工具必须报错");
    assert!(error.contains("unknown tool"), "错误信息：{error}");
}

// ── HTTP 请求 ──────────────────────────────────────────────────────────

#[test]
fn http_request_reports_status_headers_body_and_redacts_the_token() {
    let (port, server) = fake_http_server(
        // 正文正好 19 字节：`Content-Length` 与实际一致，测的是正常路径
        //（「对端提前断开」那条由 `duck-net-core::http` 的读取逻辑兜底）。
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\n\r\n{\"ok\":true,\"id\":42}",
    );
    let output = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": format!("http://127.0.0.1:{port}/ping"),
            "auth": "bearer",
            "token": "super-secret-token",
            "headers": "X-Trace: abc",
        }),
    ))
    .expect("请求应成功");
    let head = server.join().expect("server thread");

    // 客户端把请求头名统一小写（HTTP 头名不分大小写），断言也按小写比对。
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains("authorization: bearer super-secret-token"),
        "{head}"
    );
    assert!(lower.contains("x-trace: abc"), "自定义头要送到：{head}");
    assert!(output.contains("状态：HTTP 200 OK"), "{output}");
    assert!(output.contains("响应体"), "{output}");
    assert!(output.contains("\"ok\": true"), "JSON 要格式化：{output}");
    // 报告里不许出现明文 token。
    assert!(!output.contains("super-secret-token"), "{output}");
    assert!(output.contains("Authorization: ***"), "{output}");
    assert!(output.contains("curl"), "{output}");
    // curl 用 $TOKEN 占位，而不是把 token 抄进去。
    assert!(output.contains("$TOKEN"), "{output}");
}

#[test]
fn http_request_on_a_dead_port_reports_a_translated_error() {
    let error = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": "http://127.0.0.1:1/nope",
            "timeout_ms": 1000,
        }),
    ))
    .expect_err("连不上必须报错");
    assert!(
        error.contains("连接失败") || error.contains("等待超时"),
        "错误信息：{error}"
    );
}

#[test]
fn http_request_rejects_a_url_without_scheme() {
    let error = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({ "method": "GET", "url": "example.com" }),
    ))
    .expect_err("缺 scheme 必须报错");
    assert!(error.contains("URL 写错了"), "错误信息：{error}");
}

// ── 报文调试 ───────────────────────────────────────────────────────────

#[test]
fn packet_send_round_trips_an_ascii_length_frame() {
    // 服务端：读一行报文，回一个 ascii_length 帧（8 位长度 + 正文）。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
    let port = listener.local_addr().expect("地址").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(b"00000005PONG\n");
        // 让读端先拿到数据再关：不 sleep 的话本机可能读到 EOF 之前就断开。
        std::thread::sleep(std::time::Duration::from_millis(50));
    });

    let output = run_once(&request_with_input(
        "duck.net.packet-send",
        "PING\n",
        serde_json::json!({
            "protocol": "ascii_length",
            "ascii_width": 8,
            "host": "127.0.0.1",
            "port": port,
            "charset": "utf-8",
            "show_hex": "false",
            "read_timeout": 500,
        }),
    ))
    .expect("收发应成功");
    handle.join().expect("server thread");

    // 发出的是 8 位 ASCII 长度头（`00000005`）+ 5 字节正文 = 13 字节。
    assert!(output.contains("发出 13 字节"), "{output}");
    assert!(
        output.contains("30 30 30 30 30 30 30 35"),
        "长度头要进日志：{output}"
    );
    assert!(output.contains("收到 1 帧"), "{output}");
    assert!(output.contains("PONG"), "{output}");
}

#[test]
fn packet_send_reports_hex_frames_when_asked() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
    let port = listener.local_addr().expect("地址").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(&[0x01, 0x02, 0xFF]);
        std::thread::sleep(std::time::Duration::from_millis(50));
    });

    let output = run_once(&request_with_input(
        "duck.net.packet-send",
        "3C 3F 78 6D",
        serde_json::json!({
            "protocol": "raw",
            "host": "127.0.0.1",
            "port": port,
            "hex": "true",
            "show_hex": "true",
            "read_timeout": 500,
        }),
    ))
    .expect("收发应成功");
    handle.join().expect("server thread");

    // 十六进制报文按字节发出（4 字节），应答按十六进制展示。
    assert!(output.contains("发出 4 字节"), "{output}");
    assert!(output.contains("3C 3F 78 6D"), "{output}");
    assert!(output.contains("01 02 FF"), "{output}");
}

#[test]
fn packet_send_rejects_empty_payload_and_bad_delimiter() {
    let error = run_once(&request_with_input(
        "duck.net.packet-send",
        "   ",
        serde_json::json!({ "host": "127.0.0.1", "port": 9 }),
    ))
    .expect_err("空报文必须报错");
    assert!(error.contains("输入不完整"), "错误信息：{error}");

    let error = run_once(&request_with_input(
        "duck.net.packet-send",
        "PING",
        serde_json::json!({
            "protocol": "delimiter",
            "host": "127.0.0.1",
            "port": 9,
        }),
    ))
    .expect_err("分隔符协议缺分隔符必须报错");
    assert!(error.contains("分隔符"), "错误信息：{error}");
}

// ── S3 / FTP / SFTP 连通检测 ────────────────────────────────────────────

#[test]
fn s3_check_flags_partial_credentials_before_touching_the_network() {
    let output = run_once(&request(
        "duck.net.s3-ftp-checker",
        serde_json::json!({
            "protocol": "s3",
            "host": "https://s3.example.com",
            "bucket": "demo",
            "access_key": "AK",
            "secret_key": "",
        }),
    ))
    .expect("参数成立");
    assert!(output.contains("结论：失败"), "{output}");
    assert!(output.contains("凭据"), "{output}");
    // 失败步骤要带一句「下一步查什么」。
    assert!(output.contains("建议："), "{output}");
}

#[test]
fn ftp_check_reports_the_failed_step_not_a_single_verdict() {
    let output = run_once(&request(
        "duck.net.s3-ftp-checker",
        serde_json::json!({
            "protocol": "ftp",
            "host": "127.0.0.1",
            "port": 1,
            "username": "bank",
            "password": "secret",
            "timeout_ms": 500,
        }),
    ))
    .expect("参数成立");
    assert!(output.contains("结论：失败"), "{output}");
    assert!(output.contains("TCP 连接"), "{output}");
    // 后面的步骤是「没跑」而不是「失败」—— 报告要能区分这两件事。
    assert!(output.contains('·'), "{output}");
}

#[test]
fn ftp_check_logs_in_against_a_fake_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
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

    let output = run_once(&request(
        "duck.net.s3-ftp-checker",
        serde_json::json!({
            "protocol": "ftp",
            "host": "127.0.0.1",
            "port": port,
            "username": "bank",
            "password": "secret",
            "timeout_ms": 3000,
        }),
    ))
    .expect("参数成立");
    handle.join().expect("server thread");
    assert!(output.contains("结论：通过"), "{output}");
    assert!(output.contains("/incoming"), "{output}");
    assert!(output.contains("220 fake ftp ready"), "{output}");
}

// ── Hosts 管理 ─────────────────────────────────────────────────────────

#[test]
fn hosts_manager_merges_profiles_and_exports_the_file() {
    let root = temp_dir("hosts-merge");
    write(
        &root,
        "a-sit.hosts",
        "# name: 张三-联调\n# env: SIT\n10.0.0.1 app.local api.local\n10.0.0.2 db.local\n",
    );
    write(&root, "b-uat.hosts", "# env: UAT\n10.0.1.1 app.local\n");
    // 非方案文件（扩展名不对）必须被忽略。
    write(&root, "notes.md", "10.0.9.9 ignored.local\n");
    let out = root.join("hosts.merged");

    let output = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({
            "profiles_dir": root,
            "name": "默认方案",
            "env": "SIT",
            "out": out,
            "with_script": "true",
        }),
    ))
    .expect("合并应成功");

    assert!(output.contains("合并 2 个方案"), "{output}");
    // a-sit 的 name / env 来自文件头；b-uat 只有 env，名字回落到文件名。
    assert!(
        output.contains("# ===== 方案：张三-联调（SIT） ====="),
        "{output}"
    );
    assert!(
        output.contains("# ===== 方案：b-uat（UAT） ====="),
        "{output}"
    );
    assert!(output.contains("10.0.0.1 app.local api.local"), "{output}");
    // 域名重复要报出来（app.local 出现两次）。
    assert!(output.contains("域名重复"), "{output}");
    assert!(output.contains("已导出"), "{output}");
    // 应用命令：先备份再覆盖。
    assert!(
        output.contains("sudo cp /etc/hosts /etc/hosts.bak_$(date"),
        "{output}"
    );
    assert!(
        output.contains("sudo cp ~/hosts.merged /etc/hosts"),
        "{output}"
    );
    assert!(output.contains("ipconfig /flushdns"), "{output}");

    let written = std::fs::read_to_string(&out).expect("导出文件应存在");
    assert!(written.starts_with("# 由 Hosts 管理工具生成"), "{written}");
    assert!(written.contains("# 生成时间："), "{written}");
    assert!(
        written.contains("10.0.0.1 app.local api.local"),
        "{written}"
    );
    assert!(
        !written.contains("ignored.local"),
        "非方案文件不该进合并：{written}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hosts_manager_reports_an_empty_folder_instead_of_exporting_nothing() {
    let root = temp_dir("hosts-empty");
    let error = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({ "profiles_dir": root, "env": "SIT" }),
    ))
    .expect_err("空目录必须报错");
    assert!(error.contains("没有方案文件"), "错误信息：{error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hosts_manager_flags_format_problems_line_by_line() {
    let root = temp_dir("hosts-issues");
    write(
        &root,
        "bad.hosts",
        "# env: SIT\n999.1.1.1 broken.local\n10.0.0.1\n",
    );
    let output = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({ "profiles_dir": root, "with_script": "false" }),
    ))
    .expect("合并应成功");

    assert!(output.contains("检查发现 2 处问题"), "{output}");
    assert!(output.contains("非法 IP：999.1.1.1"), "{output}");
    assert!(output.contains("缺少主机名"), "{output}");
    // 关掉「附带写入命令」后不该出现 sudo。
    assert!(!output.contains("sudo cp"), "{output}");
    let _ = std::fs::remove_dir_all(&root);
}

// ── S3 对象存储 ────────────────────────────────────────────────────────

/// 一段可以塞进 `Content-Length` 的完整 HTTP 响应。
fn xml_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn s3_manager_lists_buckets_with_a_signed_request() {
    let (port, server) = fake_http_server(xml_response(
        "<ListAllMyBucketsResult><Buckets>\
         <Bucket><Name>demo</Name><CreationDate>2026-10-01T00:00:00.000Z</CreationDate></Bucket>\
         </Buckets></ListAllMyBucketsResult>",
    ));
    let output = run_once(&request(
        "duck.net.s3-manager",
        serde_json::json!({
            "action": "buckets",
            "endpoint": format!("http://127.0.0.1:{port}"),
            "access_key": "AKIAIOSFODNN7EXAMPLE",
            "secret_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "path_style": "true",
        }),
    ))
    .expect("列出存储桶");

    assert!(output.contains("demo"), "{output}");
    assert!(output.contains("共 1 个存储桶"), "{output}");
    let head = server.join().expect("server thread");
    // 桶级接口是 `GET /`；有 AK 就必须签名。
    // 头名要比小写：HTTP 头不区分大小写，`ureq` 发出去的是小写形态。
    assert!(head.starts_with("GET / HTTP/1.1"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: aws4-hmac-sha256"),
        "{head}"
    );
}

/// 参数名对不上时**不会静默生效**：`prefix` / `dest_key` 这些名字是内核与清单
/// 之间的约定，拼错了只会得到「输入不完整」而不是错误的复制结果。
///
/// 列举请求的 query 排序与编码由 `duck-net-core::s3` 的单测钉住（那里断言的是
/// 精确的 `GET /demo?delimiter=%2F&list-type=2&max-keys=1000&prefix=a%2F`）——
/// 这一层只验「清单里的参数真的走到了内核」。
#[test]
fn s3_manager_rejects_copy_without_a_destination() {
    let error = run_once(&request(
        "duck.net.s3-manager",
        serde_json::json!({
            "action": "copy",
            "endpoint": "http://127.0.0.1:1",
            "bucket": "demo",
            "prefix": "docs",
        }),
    ))
    .expect_err("没有目标 Key 必须报错");
    assert!(error.contains("输入不完整"), "{error}");
}

#[test]
fn s3_manager_rejects_object_actions_without_a_key() {
    // 空 key 必须当场报错：`DELETE /demo` 是**删桶**，语义完全不同。
    let error = run_once(&request(
        "duck.net.s3-manager",
        serde_json::json!({
            "action": "delete-object",
            "endpoint": "http://127.0.0.1:1",
            "bucket": "demo",
        }),
    ))
    .expect_err("没有 key 必须报错");
    assert!(error.contains("输入不完整"), "{error}");
}

/// 停用标记（文件头 `# enabled: false`）必须让方案退出合并，但**不能**让它从
/// 清单与校验里消失 —— 「停用」和「删掉」是两件事。
#[test]
fn hosts_manager_skips_disabled_profiles_but_still_lists_them() {
    let root = temp_dir("hosts-disabled");
    write(
        &root,
        "a-sit.hosts",
        "# env: SIT\n# owner: 实施组\n# ticket: CHG-2026-0001\n10.0.0.1 app.sit.bank\n",
    );
    write(
        &root,
        "b-prod.hosts",
        "# env: PROD\n# enabled: false\n10.9.9.9 app.bank\n",
    );

    let output = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({ "profiles_dir": root, "with_script": "false" }),
    ))
    .expect("合并应成功");

    assert!(output.contains("合并 1 个方案"), "{output}");
    assert!(output.contains("另有 1 个方案已停用"), "{output}");
    // 清单里两个都在，停用的那个带 ○ 与「已停用」。
    assert!(output.contains("● a-sit"), "{output}");
    assert!(output.contains("○ b-prod"), "{output}");
    assert!(output.contains("已停用"), "{output}");
    assert!(output.contains("负责人 实施组"), "{output}");
    assert!(output.contains("变更单 CHG-2026-0001"), "{output}");
    // 停用方案的内容不该进合并结果。`app.bank` 只出现在那一段里。
    let merged = output.split("## ").next().unwrap_or(&output);
    assert!(merged.contains("app.sit.bank"), "{output}");
    assert!(
        !merged
            .split("校验")
            .next()
            .unwrap_or("")
            .contains("app.bank"),
        "停用方案的内容不该进合并：{output}"
    );

    // 显式要求「连停用的一起合」时，它要回来。
    let output = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({
            "profiles_dir": root,
            "include_disabled": "true",
            "with_script": "false",
        }),
    ))
    .expect("合并应成功");
    assert!(output.contains("合并 2 个方案"), "{output}");
    assert!(output.contains("10.9.9.9 app.bank"), "{output}");
    assert!(!output.contains("另有"), "{output}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 查找 + 替换 + 对比：三段都由参数触发，替换只写**另一个目录**。
#[test]
fn hosts_manager_searches_replaces_and_diffs_profiles() {
    let root = temp_dir("hosts-search");
    write(
        &root,
        "a-sit.hosts",
        "10.0.0.1 api.sit.bank\n# api.sit.bank 旧地址\n",
    );
    write(&root, "b-uat.hosts", "10.1.0.1 api.uat.bank\n");
    let replaced = root.join("replaced");

    let output = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({
            "profiles_dir": root,
            "keyword": "api.sit.bank",
            "replace_to": "api.uat.bank",
            "replace_out": replaced,
            "diff_a": "a-sit",
            "diff_b": "b-uat",
            "with_script": "false",
        }),
    ))
    .expect("运行应成功");

    // 查找：命中两处，含被注释掉的那一行（`第 2 行`）。
    assert!(
        output.contains("查找「api.sit.bank」：2 处命中"),
        "{output}"
    );
    assert!(output.contains("# api.sit.bank 旧地址"), "{output}");
    // 替换：两处，写盘到另一个目录。
    assert!(
        output.contains("替换「api.sit.bank」→「api.uat.bank」：共 2 处"),
        "{output}"
    );
    assert!(output.contains("已写出 1 个方案文件"), "{output}");
    let written = std::fs::read_to_string(replaced.join("a-sit.hosts")).expect("应写出替换结果");
    assert!(written.contains("10.0.0.1 api.uat.bank"), "{written}");
    assert!(
        written.contains("# api.uat.bank 旧地址"),
        "注释里的也换：{written}"
    );
    // 原文件一个字都没动。
    let source = std::fs::read_to_string(root.join("a-sit.hosts")).expect("原文件还在");
    assert!(source.contains("api.sit.bank"), "{source}");
    // 对比：A 独有两行（含注释行？不 —— 注释不参与）。
    assert!(output.contains("方案对比：a-sit ←→ b-uat"), "{output}");
    assert!(output.contains("仅 a-sit 有"), "{output}");
    assert!(output.contains("- 10.0.0.1 api.sit.bank"), "{output}");
    assert!(output.contains("+ 10.1.0.1 api.uat.bank"), "{output}");

    // 替换结果目录不能是方案目录本身：那等于原地改。
    let error = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({
            "profiles_dir": root,
            "keyword": "api",
            "replace_to": "x",
            "replace_out": root,
        }),
    ))
    .expect_err("同目录必须拒绝");
    assert!(error.contains("不能和方案目录是同一个"), "{error}");

    // 对比方案写错时，错误里要列出可用的 slug（否则用户只能去翻目录）。
    let error = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({ "profiles_dir": root, "diff_a": "a-sit", "diff_b": "nope" }),
    ))
    .expect_err("不存在的方案必须报错");
    assert!(error.contains("a-sit / b-uat"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 全部停用时给一句人话，而不是「合并 0 个方案」这种半截结果。
#[test]
fn hosts_manager_rejects_a_folder_where_everything_is_disabled() {
    let root = temp_dir("hosts-all-disabled");
    write(
        &root,
        "only.hosts",
        "# enabled: false\n10.0.0.1 app.local\n",
    );
    let error = run_once(&request(
        "duck.net.hosts-manager",
        serde_json::json!({ "profiles_dir": root }),
    ))
    .expect_err("全部停用必须报错");
    assert!(error.contains("所有方案都被停用了"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

// ── HTTP 请求：本轮补的参数面（query / 变量 / 表单 / cookie / cURL） ──────

/// 假服务：读完头**再按 Content-Length 读体**，把头与体一起交回来。
///
/// `fake_http_server` 只读头（GET 够用）；POST 的表单与 multipart 必须看到体，
/// 否则「体发没发出去」只能靠报告自证——那就等于没测。
fn fake_http_server_with_body(
    response: impl Into<String>,
) -> (u16, std::thread::JoinHandle<(String, String)>) {
    let response = response.into();
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
        let mut length = 0usize;
        for line in head.lines() {
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        if length > 0 {
            let _ = reader.read_exact(&mut body);
        }
        let mut writer = stream;
        let _ = writer.write_all(response.as_bytes());
        let _ = writer.flush();
        (head, String::from_utf8_lossy(&body).into_owned())
    });
    (port, handle)
}

#[test]
fn http_request_sends_query_vars_and_urlencoded_body() {
    let (port, server) = fake_http_server_with_body(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
    );
    let output = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "POST",
            "url": format!("http://127.0.0.1:{port}/orders"),
            "vars": "who=张 三",
            "query": "page=2&q=a b",
            "body_type": "urlencoded",
            "form": "name={{ who }}",
        }),
    ))
    .expect("请求应成功");
    let (head, body) = server.join().expect("server thread");

    // 查询串已编码并拼到 URL 上（空格 → %20，不是 `+`）。
    assert!(
        head.starts_with("POST /orders?page=2&q=a%20b HTTP/1.1"),
        "{head}"
    );
    // 变量插值后再编码：`张 三` → UTF-8 百分号编码。
    assert_eq!(body, "name=%E5%BC%A0%20%E4%B8%89");
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: application/x-www-form-urlencoded"),
        "{head}"
    );
    // 报告要同时给出「查询参数」与「请求体形态」两段。
    assert!(output.contains("查询参数（2 个）"), "{output}");
    assert!(output.contains("请求体：x-www-form-urlencoded"), "{output}");
    // 变量值里带凭据含义的名字要打码，这里 `who` 不是，所以照给。
    assert!(output.contains("who = 张 三"), "{output}");
}

#[test]
fn http_request_reports_set_cookie_and_size() {
    let (port, server) = fake_http_server_with_body(
        "HTTP/1.1 200 OK\r\nSet-Cookie: sid=abc123; Path=/; HttpOnly\r\nContent-Length: 0\r\n\r\n",
    );
    let output = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": format!("http://127.0.0.1:{port}/login"),
            "body_type": "none",
        }),
    ))
    .expect("请求应成功");
    let _ = server.join().expect("server thread");
    assert!(output.contains("Set-Cookie（1 条）"), "{output}");
    assert!(output.contains("sid = abc123"), "{output}");
    assert!(output.contains("HttpOnly"), "{output}");
    // 体积按 1024 进制给人读。
    assert!(output.contains("体积 0 B"), "{output}");
}

#[test]
fn http_request_imports_a_curl_command() {
    let (port, server) =
        fake_http_server_with_body("HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n");
    let curl = format!(
        "curl 'http://127.0.0.1:{port}/v1/orders' \\\n  -X POST \\\n  -H 'Content-Type: application/json' \\\n  -H 'Authorization: Bearer abc.def' \\\n  --data-raw '{{\"sku\":\"A1\"}}'"
    );
    let output = run_once(&request_with_input(
        "duck.net.http-request",
        &curl,
        serde_json::json!({ "source": "curl" }),
    ))
    .expect("请求应成功");
    let (head, body) = server.join().expect("server thread");

    let lower = head.to_ascii_lowercase();
    assert!(lower.starts_with("post /v1/orders"), "{head}");
    assert!(lower.contains("authorization: bearer abc.def"), "{head}");
    assert_eq!(body, "{\"sku\":\"A1\"}");
    assert!(output.contains("来源：cURL 命令解析结果"), "{output}");
    assert!(output.contains("状态：HTTP 201 Created"), "{output}");
    // 命令里的 token 进了报告也只以打码形式出现。
    assert!(!output.contains("abc.def"), "{output}");
}

#[test]
fn http_request_rejects_an_undefined_variable() {
    let error = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": "http://127.0.0.1:1/{{ host }}/x",
            "body_type": "none",
        }),
    ))
    .expect_err("未定义变量必须报错");
    assert!(error.contains("变量没有定义"), "{error}");
    assert!(error.contains("host"), "{error}");
}

#[test]
fn http_request_rejects_a_malformed_pair_line() {
    let error = run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": "http://127.0.0.1:1/x",
            "query": "没有等号",
            "body_type": "none",
        }),
    ))
    .expect_err("键值行写错必须报错");
    assert!(error.contains("键值行写错了"), "{error}");
}

/// 历史要落盘，所以 **不能带查询串**：`key_in = query` 的 API Key 就在里面。
#[test]
fn http_history_never_carries_the_query_string() {
    let (port, server) = fake_http_server_with_body("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let marker = "secret-query-xyz";
    run_once(&request(
        "duck.net.http-request",
        serde_json::json!({
            "method": "GET",
            "url": format!("http://127.0.0.1:{port}/ledger"),
            "query": format!("token={marker}"),
            "body_type": "none",
        }),
    ))
    .expect("请求应成功");
    let _ = server.join().expect("server thread");

    // 数据目录 = 二进制上两级的 `data/`（见 `data_dir()`）。
    let bin = Path::new(env!("CARGO_BIN_EXE_duck-net-tool"));
    let history = bin
        .parent()
        .and_then(Path::parent)
        .expect("bin 上两级")
        .join("data/http-history.json");
    let raw = std::fs::read_to_string(&history).expect("历史应已写出");
    assert!(!raw.contains(marker), "历史里不许出现查询串的值：{raw}");
    assert!(raw.contains("/ledger"), "路径还要留着：{raw}");
}
