//! 端到端：真的跑一次发布工具链，客户端真的验过。
//!
//! 单元测试里的固定向量证明「格式对得上」，这一条证明**连着工具一起也对得上**——
//! `sbin/manifest.py` 写出去的那串字节，正是客户端会签收的那串字节。
//! 分两次 dump、少一个结尾换行、签名算在序列化之前，这类错误只有真跑一遍才现形。
//!
//! 这两条测试会起 `python3`。找不到解释器时**跳过**而不是失败——本仓发版本来就要
//! python3，但把「能不能跑 cargo test」绑在它上面是另一回事。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 找一个能用的 python3。
fn python3() -> Option<&'static str> {
    for candidate in ["python3", "/usr/bin/python3"] {
        let works = Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if works {
            return Some(candidate);
        }
    }
    None
}

/// 仓库根：`crates/duck-update` 往上两层。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root is reachable from the crate")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-release-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates");
    dir
}

/// 手写实现正确性的**前置条件**：RFC 8032 的官方向量。
///
/// 没有它，「Python 签得出、Rust 验得过」只能证明两边一致地错。
#[test]
fn the_signing_helper_passes_its_own_rfc8032_selftest() {
    let Some(python) = python3() else {
        eprintln!("skipping: no python3 on PATH");
        return;
    };

    let output = Command::new(python)
        .arg(repo_root().join("sbin/minisign_sign.py"))
        .arg("--selftest")
        .output()
        .expect("runs the self test");
    assert!(
        output.status.success(),
        "the signing helper failed its own vectors:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_manifest_signed_by_the_release_tooling_verifies() {
    let Some(python) = python3() else {
        eprintln!("skipping: no python3 on PATH");
        return;
    };

    let root = repo_root();
    let dir = scratch("signed");

    // 三个平台的产物都造齐：manifest.py 默认拒绝残缺的发布，测试不该绕开那条规则。
    for name in [
        "duck-0.2.0-macos-arm64-app.tar.gz",
        "duck-0.2.0-macos-x64-app.tar.gz",
        "duck-0.2.0-windows-x64.zip",
    ] {
        std::fs::write(dir.join(name), format!("payload for {name}")).expect("writes");
    }

    let key = dir.join("key");
    let generated = Command::new(python)
        .arg(root.join("sbin/minisign_sign.py"))
        .arg("--generate-key")
        .arg(&key)
        .output()
        .expect("runs the key generator");
    assert!(
        generated.status.success(),
        "key generation failed:\n{}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let manifest = dir.join("update.json");
    let built = Command::new(python)
        .arg(root.join("sbin/manifest.py"))
        .arg("--version")
        .arg("0.2.0")
        .arg("--tag")
        .arg("v0.2.0")
        .arg("--repo")
        .arg("HanoCode/gpui-toolbox")
        .arg("--asset-dir")
        .arg(&dir)
        .arg("--out")
        .arg(&manifest)
        .arg("--published-at")
        .arg("2026-10-11T00:00:00Z")
        .arg("--sign-key")
        .arg(key.with_extension("key"))
        .output()
        .expect("runs the manifest builder");
    assert!(
        built.status.success(),
        "manifest generation failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    // 公钥文件的第二行，就是客户端要在构建时内建的那个值。
    let public_file = std::fs::read_to_string(key.with_extension("pub")).expect("reads the key");
    let public_key = public_file
        .lines()
        .nth(1)
        .expect("a minisign.pub file is a comment plus a key");

    let bytes = std::fs::read(&manifest).expect("reads the manifest");
    let signature =
        std::fs::read_to_string(dir.join("update.json.sig")).expect("reads the signature");

    duck_update::verify_with(public_key, &bytes, &signature)
        .expect("the client must accept what sbin/manifest.py signs");

    // 反过来：改一个字节就不该再验得过。少了这一半，上面那行只是在证明「有个函数返回 Ok」。
    let mut tampered = bytes.clone();
    tampered[0] ^= 0x01;
    assert!(
        duck_update::verify_with(public_key, &tampered, &signature).is_err(),
        "a tampered manifest must be refused"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
