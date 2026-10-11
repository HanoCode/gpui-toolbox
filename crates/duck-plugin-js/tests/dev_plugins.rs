//! 仓库根 `plugins/` 开发插件的端到端守卫 + **能力授予**的端到端契约。
//!
//! dev 构建（`sbin/run.sh`）直接加载仓库 `plugins/` 目录（见
//! `duck-app/src/plugins.rs`），本测试确保里面的示例插件始终满足宿主契约：
//! 可发现、清单有效、JS 轨可执行——fixture 腐化会在 `cargo test` 即刻暴露。
//!
//! 后半段（`granted` 那几条）钉的是 P2-8 的语义：**声明 ∩ 授予**才是生效授权。
//! 只断言「允了能跑」不够——那只证明「能跑」，不证明闸门接上了；所以每条都配一个
//! 「不给就真的跑不动」的反向断言，并且区分到**具体哪一项能力**：
//! 示例插件先写文件、再发通知，于是「只给写」应当写得出文件、但以通知被拒收场，
//! 「只给通知」应当连文件都写不出来。

use std::path::{Path, PathBuf};

use duck_plugin::{Capabilities, PluginIndex, PluginManifest};

fn repo_plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

/// 发现到的示例插件目录与清单。
fn sample_plugin() -> (PathBuf, PluginManifest) {
    let index = PluginIndex::discover(&repo_plugins_dir());
    assert_eq!(
        index.loaded_count(),
        1,
        "repo plugins/ must contain exactly one valid plugin"
    );
    let plugin = &index.plugins()[0];
    let manifest = plugin
        .manifest
        .as_ref()
        .expect("sample manifest must be valid")
        .clone();
    assert_eq!(manifest.id, "dev.duck.textstat");
    assert_eq!(manifest.tools.len(), 1);
    (plugin.dir.clone(), manifest)
}

/// 一次性的临时数据目录：**每次从空目录开始**，否则上一条测试留下的
/// `last-report.txt` 会让「文件不该存在」的断言假通过。
fn fresh_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-plugin-js-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

#[test]
fn repo_dev_plugin_discovers_and_executes() {
    let (dir, manifest) = sample_plugin();
    let data_dir = fresh_data_dir("dev-plugin-full");

    // 全给＝按声明全授（宿主在没有授予记录时的缺省）。
    let outcome = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &manifest.capabilities.clone(),
        "dev.duck.textstat.stats",
        "",
    )
    .expect("sample tool must execute when every declared capability is granted");
    assert!(
        outcome.output.contains("chars="),
        "unexpected output: {}",
        outcome.output
    );
    assert!(!outcome.notifications.is_empty());
    assert!(!outcome.logs.is_empty());
    assert!(
        data_dir.join("last-report.txt").is_file(),
        "report file must be written into the plugin data dir"
    );
}

/// 一个都不给：插件必须**跑不起来**，而且写不出任何文件。
///
/// 后半句才是重点——「返回 Err」也可能是别的原因（比如沙箱路径判断），
/// 文件不存在才能证明它连第一句 `duck.writeFile` 都没过去。
#[test]
fn nothing_granted_means_the_plugin_cannot_even_write_its_report() {
    let (dir, manifest) = sample_plugin();
    let data_dir = fresh_data_dir("dev-plugin-none");

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &Capabilities::default(),
        "dev.duck.textstat.stats",
        "",
    );

    let error = result.expect_err("no capability granted: the tool must fail");
    let text = error.to_string();
    assert!(
        text.contains("not granted"),
        "失败原因必须是「能力未授予」，而不是别的意外：{text}"
    );
    assert!(
        !data_dir.join("last-report.txt").exists(),
        "被拒之后不该留下任何产出"
    );
}

/// 只给「写」不给「通知」：文件写得出来，但后面 `duck.notify` 那步被拒。
///
/// 这条专门盯 [`duck.notify`] 的闸门——它原先**完全没走仲裁**（只往一个
/// `Vec` 里 push），于是确认框上取消勾选「通知」是个摆设。
#[test]
fn granting_only_the_write_leaves_the_notification_denied() {
    let (dir, manifest) = sample_plugin();
    let data_dir = fresh_data_dir("dev-plugin-write-only");
    let granted = Capabilities {
        filesystem_write: manifest.capabilities.filesystem_write.clone(),
        ..Capabilities::default()
    };

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &granted,
        "dev.duck.textstat.stats",
        "",
    );

    let error = result.expect_err("notifications were not granted: the tool must fail");
    assert!(
        error.to_string().contains("Notifications"),
        "这一半必须卡在通知上：{error}"
    );
    assert!(
        data_dir.join("last-report.txt").is_file(),
        "写权限给了就该写得出来——否则证明的不是闸门而是整条链路坏了"
    );
}

/// 只给「通知」不给「写」：连文件都写不出来（与上一条互为镜像）。
#[test]
fn granting_only_notifications_leaves_the_write_denied() {
    let (dir, manifest) = sample_plugin();
    let data_dir = fresh_data_dir("dev-plugin-notify-only");
    let granted = Capabilities {
        notifications: true,
        ..Capabilities::default()
    };

    let result = duck_plugin_js::run_tool(
        &dir,
        &data_dir,
        &manifest,
        &granted,
        "dev.duck.textstat.stats",
        "",
    );

    let error = result.expect_err("filesystem write was not granted: the tool must fail");
    assert!(
        error.to_string().contains("not granted"),
        "这一半必须卡在写权限上：{error}"
    );
    assert!(
        !data_dir.join("last-report.txt").exists(),
        "写权限没给，文件就不该出现"
    );
}

/// 授予里多出来的能力不会被执行到：`hash` 一下声明与授予后**再**求交，
/// 结果里只能有声明过的那些（示例插件根本没声明网络）。
#[test]
fn a_grant_cannot_smuggle_in_undeclared_capabilities() {
    let (_, manifest) = sample_plugin();
    let greedy = Capabilities {
        network_hosts: vec!["exfil.example.com".into()],
        process_exec: vec!["curl".into()],
        ..manifest.capabilities.clone()
    };
    let effective = manifest.capabilities.intersect(&greedy);
    assert_eq!(effective, manifest.capabilities);
    assert!(
        effective.network_hosts.is_empty() && effective.process_exec.is_empty(),
        "声明里没有的网络与进程能力不能被授予出来"
    );
}
