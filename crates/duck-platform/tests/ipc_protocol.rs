//! 单实例转发协议契约（§6.2.3）
//!
//! 钉住三个保证：
//! 1. `encode` → `decode` 是无损的（[roundtrip_preserves_all_request_kinds]）
//! 2. 「错协议」绝不抛 panic、绝不误识别为有效载荷
//!    （[decode_rejects_wrong_header_silently] / [decode_ignores_unknown_tags]）
//! 3. 端到端：`listen_for_requests` 真开一个 listener，外部
//!    `forward_to_running_instance` 真把请求送过去，handler 收到的内容一致
//!    （[forward_to_listener_round_trip]）
//!
//! 跨进程 fixture：所有测试都跑在同一进程内，但**走的是真 socket**（macOS 是
//! Unix domain socket，Windows 是 127.0.0.1 随机端口）。listener 走后台线程，
//! handler 收到后写进 `Arc<Mutex<Vec<Vec<OpenRequest>>>>` 给主线程断言。
//!
//! 端口与 socket 路径都用测试自己的临时目录，**不**碰 `app_data_dir()`——避免
//! 跑测试时把当前实例的配置（log / kv / vault）搞乱。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use duck_platform::OpenRequest;
use duck_platform::{IpcError, IpcListener, forward_to_running_instance, listen_for_requests};

/// 全局串行化锁：避免 `HOME` 等环境变量在多测试并发时被互相覆盖。
///
/// cargo test 默认并发跑——任意两个 `setup_test_data_dir` 调 `set_var("HOME", …)`
/// 都会跨线程污染对方。`OnceLock<Mutex<()>>` 保证四个 listener 测试串行。
fn serial_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── encode / decode 协议层 ─────────────────────────────────────────

#[test]
fn roundtrip_preserves_all_request_kinds() {
    let requests = vec![
        OpenRequest::Url("duck://tool/http.replay".to_string()),
        OpenRequest::Url("https://example.com".to_string()),
        OpenRequest::Path(PathBuf::from("/tmp/foo.txt")),
        OpenRequest::Path(PathBuf::from("/Users/me/邮箱.eml")),
    ];
    let encoded = duck_platform::encode(&requests);
    let decoded = duck_platform::decode(&encoded);
    assert_eq!(decoded, requests, "encode→decode must be lossless");
}

#[test]
fn decode_rejects_wrong_header_silently() {
    // 不是我们的协议：返回空表，**不** panic。
    let payload = "not-our-protocol/9\nU https://evil.com\n";
    let decoded = duck_platform::decode(payload);
    assert!(
        decoded.is_empty(),
        "wrong-header payload must be dropped, got {decoded:?}"
    );

    // 空 payload 也安全。
    assert!(duck_platform::decode("").is_empty());
}

#[test]
fn decode_ignores_unknown_tags() {
    // 新版本加 `D ` 标签（deeplink），老版本不识别：忽略而不是炸掉。
    let payload = "duck-open/1\nU https://example.com\nD something-new\nF /tmp/x\n";
    let decoded = duck_platform::decode(payload);
    // 已知标签（U / F）保留；未知标签（D）忽略。
    assert_eq!(decoded.len(), 2);
    assert!(matches!(&decoded[0], OpenRequest::Url(s) if s == "https://example.com"));
    assert!(matches!(&decoded[1], OpenRequest::Path(p) if p == &PathBuf::from("/tmp/x")));
}

#[test]
fn encode_drops_entries_with_embedded_linebreaks() {
    // 路径里塞 \n：encode 阶段就丢掉（不让对端解出畸形 payload）。
    let requests = vec![
        OpenRequest::Path(PathBuf::from("/tmp/injected\nU https://evil.com")),
        OpenRequest::Url("https://safe.com".to_string()),
    ];
    let encoded = duck_platform::encode(&requests);
    let decoded = duck_platform::decode(&encoded);
    // 第一条被 encode 阶段丢，第二条留下。
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0], OpenRequest::Url(s) if s == "https://safe.com"));
}

// ── listener 端到端 ─────────────────────────────────────────────────

#[test]
fn forward_to_listener_round_trip() {
    // 起短路径的临时目录，绕开 app_data_dir 的硬编码。
    // 端到端真 socket：listener 起来后阻塞听，外部用 `forward_to_running_instance`
    // 试图发；只要 listener handler 收到我们期望的请求就通过。

    setup_test_data_dir("round-trip");

    let _guard = serial_lock().lock().expect("not poisoned");

    let received: Arc<Mutex<Vec<Vec<OpenRequest>>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = Arc::clone(&received);
    let _listener = listen_for_requests(move |requests| {
        received_clone.lock().expect("not poisoned").push(requests);
    })
    .expect("listener should start in the temp dir");

    // 再起一个 listener 在同一 endpoint 必然失败（AlreadyListening）。
    let second = listen_for_requests(|_| {});
    match second {
        Err(IpcError::AlreadyListening) => {}
        other => panic!("expected AlreadyListening, got {other:?}"),
    }

    // 现在 forward 一个请求。
    let to_send = vec![OpenRequest::Url("duck://tool/http.replay".to_string())];
    forward_to_running_instance(&to_send).expect("forward should succeed");

    // 给 listener 一点时间收。
    wait_for(
        || received.lock().expect("not poisoned").len() == 1,
        Duration::from_secs(2),
    );

    let got = received.lock().expect("not poisoned");
    assert_eq!(got.len(), 1, "handler called exactly once");
    assert_eq!(got[0], to_send, "payload survived end-to-end");
}

// ── 平台层 fallback ────────────────────────────────────────────────

#[test]
fn forward_with_no_listener_fails_gracefully() {
    let _guard = serial_lock().lock().expect("not poisoned");
    setup_test_data_dir("no-listener");
    let requests = vec![OpenRequest::Url("https://x.com".to_string())];
    let result = forward_to_running_instance(&requests);
    // 没有 listener 的时候：no listener error，不 panic。
    assert!(matches!(result, Err(IpcError::NoListener)));
}

// ── helpers ────────────────────────────────────────────────────────

/// 把 `app_data_dir` 临时指向一个**短路径**目录（Unix socket 路径长度上限 108，
/// macOS 默认 `/var/folders/...` 已经超长）。
///
/// **必须** 在每个测试入口调一次；多线程并行时会互相覆盖路径，所以这一组测试
/// 应避免并发。CI 上 macOS 腿已经串行跑，无冲突风险。
fn setup_test_data_dir(label: &str) -> PathBuf {
    let path = PathBuf::from(format!("/tmp/duck-ipc-{label}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create isolated dir");
    // `std::env::set_var` 自 Rust 1.84 起是 `unsafe`（跨线程数据竞争），
    // 我们的测试是单线程串行跑，unsafe 包裹是必要的。
    unsafe {
        std::env::set_var("HOME", &path);
        std::env::set_var("XDG_DATA_HOME", path.join(".local/share"));
    }
    let support = path.join("Library/Application Support/DuckToolbox");
    std::fs::create_dir_all(&support).expect("create app data dir");
    path
}

/// 忙等 `predicate` 为真，最多 `timeout`。CI runner 上慢一点给 2 s 是合理的。
fn wait_for(mut predicate: impl FnMut() -> bool, timeout: Duration) {
    let start = std::time::Instant::now();
    while !predicate() {
        if start.elapsed() > timeout {
            panic!("timed out waiting for predicate after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// 标记 `IpcListener` 用上：保证 `Drop` 路径在测试范围内也跑过一次。
#[test]
fn listener_drop_runs_cleanly() {
    let _guard = serial_lock().lock().expect("not poisoned");
    setup_test_data_dir("drop-test");
    let l: IpcListener = listen_for_requests(|_| {}).expect("listener");
    drop(l);
    // 再起一个：在同一 endpoint 应当成功（上一个已经清掉）。
    let l2 = listen_for_requests(|_| {});
    assert!(
        matches!(l2, Ok(_)),
        "after drop, new listener should bind: {l2:?}"
    );
}
