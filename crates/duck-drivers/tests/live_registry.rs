//! 联网用例：对着 dbx 的真实注册表与真实工件跑一遍「拉注册表 → 装驱动 → 起驱动进程」。
//!
//! 这些用例**默认不跑**（`#[ignore]`）：`cargo test` 不该依赖网络，而它们拉的是几十 MB
//! 的工件。要跑就显式点名：
//!
//! ```bash
//! cargo test -p duck-drivers --test live_registry -- --ignored --nocapture
//! ```
//!
//! 它们存在的意义是**证明这条链路真的通**：内核的单测只能证明「我们的解析 / 校验 /
//! 落位在自己的假设里自洽」，只有真下一次、真起一次进程，才能证明「dbx 的工件形状与
//! 我们认的形状是同一种」。

use std::path::PathBuf;

use duck_drivers::{DownloadSource, DriverStore, EventSink, registry};

fn temp_base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-drivers-live-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 注册表能拉下来、且能认出我们关心的那几条驱动。
#[test]
#[ignore = "needs network"]
fn the_live_registry_matches_what_the_ui_expects() {
    let registry = registry::fetch_registry(DownloadSource::Official).expect("拉注册表");
    assert!(
        registry.drivers.len() >= 40,
        "驱动太少：{}",
        registry.drivers.len()
    );

    for key in ["dameng", "kingbase", "oracle", "db2", "hive"] {
        let info = registry
            .driver(key)
            .unwrap_or_else(|| panic!("注册表里没有 {key}"));
        assert!(!info.version.is_empty(), "{key} 没有版本号");
        assert!(
            info.artifact_for(duck_drivers::platform::current())
                .is_some(),
            "{key} 在本平台没有工件"
        );
    }

    // JRE 键必须能对上驱动的 `jre` 字段 —— 对不上就会「装完驱动起不来」。
    for (key, info) in &registry.drivers {
        if let Some(jre_key) = info.jre_key() {
            assert!(
                registry.jre(jre_key).is_some(),
                "{key} 要的 JRE {jre_key} 不在注册表里"
            );
        }
    }
}

/// 装一条 **native 轨**驱动：全流程（下载 → 校验 → 解包 → 落位 → 启动描述）走通，
/// 且落位的是可执行文件、没有 JRE 依赖。
#[test]
#[ignore = "downloads a real artifact"]
fn a_native_driver_installs_and_yields_a_launch_spec() {
    let base = temp_base("native");
    let store = DriverStore::new_at(base.clone());
    let registry = registry::fetch_registry(DownloadSource::Official).expect("拉注册表");

    // 本平台有原生件、且体积最小的那条：`tdengine`（~1.1 MB）。
    let key = ["tdengine", "xugu", "kingbase"]
        .into_iter()
        .find(|key| {
            registry
                .driver(key)
                .and_then(|info| info.artifact_for(duck_drivers::platform::current()))
                .is_some_and(|resolved| resolved.kind == duck_drivers::ArtifactKind::Native)
        })
        .expect("至少有一条 native 轨驱动");

    let (sender, receiver) = std::sync::mpsc::channel();
    let sink = EventSink::new(sender);
    let installed = store
        .install(&registry, key, &sink, &duck_drivers::CancelFlag::new())
        .expect("安装 native 驱动");

    assert_eq!(installed.key, key);
    assert_eq!(installed.kind, duck_drivers::ArtifactKind::Native);

    let spec = store.launch_spec(key).expect("启动描述");
    assert_eq!(spec.kind, duck_drivers::ArtifactKind::Native);
    assert!(spec.program.is_file(), "{:?} 不是一个文件", spec.program);
    assert!(spec.args.is_empty(), "native 轨不该有参数：{:?}", spec.args);

    // 事件里应该出现「下载 → 校验 → 解包 → 落位 → 结束」这几步。
    let steps: Vec<duck_drivers::ProgressStep> =
        receiver.try_iter().map(|event| event.step).collect();
    for expected in [
        duck_drivers::ProgressStep::Download,
        duck_drivers::ProgressStep::Verify,
        duck_drivers::ProgressStep::Extract,
        duck_drivers::ProgressStep::Install,
        duck_drivers::ProgressStep::Finished,
    ] {
        assert!(steps.contains(&expected), "缺 {expected:?}：{steps:?}");
    }

    // 重装一次应命中缓存（不再下载），且结果一致。
    let again = store
        .install(
            &registry,
            key,
            &EventSink::silent(),
            &duck_drivers::CancelFlag::new(),
        )
        .expect("用缓存重装");
    assert_eq!(again.version, installed.version);

    // 卸载之后目录与状态都要干净。
    store.uninstall(key, &EventSink::silent()).expect("卸载");
    assert!(store.installed(key).is_none());
    assert!(!store.driver_dir(key).exists());
    let _ = std::fs::remove_dir_all(&base);
}

/// 装一条 **jar 轨**驱动要先把托管 JRE 装上（约 35 MB）—— 这条证明「先确保 JVM，
/// 再装驱动」的顺序真的成立，并且装出来的 jar 能拼出一条完整的 `java -jar` 命令行。
#[test]
#[ignore = "downloads a real JRE and artifact"]
fn a_jar_driver_installs_with_a_managed_jre() {
    let base = temp_base("jar");
    let store = DriverStore::new_at(base.clone());
    let registry = registry::fetch_registry(DownloadSource::Official).expect("拉注册表");

    let key = "dameng";
    let installed = store
        .install(
            &registry,
            key,
            &EventSink::silent(),
            &duck_drivers::CancelFlag::new(),
        )
        .expect("安装 jar 驱动（会先装托管 JRE）");

    assert_eq!(installed.kind, duck_drivers::ArtifactKind::Jar);
    assert!(store.driver_dir(key).join("agent.jar").is_file());
    assert!(store.java_for("21").is_some(), "托管 JRE 没装上");

    let spec = store.launch_spec(key).expect("启动描述");
    assert_eq!(spec.kind, duck_drivers::ArtifactKind::Jar);
    assert!(spec.args.contains(&"-jar".to_owned()));
    assert!(
        spec.args
            .iter()
            .any(|arg| arg == "--add-opens=java.base/java.net=ALL-UNNAMED"),
        "达梦需要 java.net 的 opens：{:?}",
        spec.args
    );
    let _ = std::fs::remove_dir_all(&base);
}
