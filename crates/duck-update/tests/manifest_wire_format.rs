//! 写端（`sbin/manifest.py`）与读端（本 crate）之间的线格式契约。
//!
//! 下面这份文档的**形状逐字段来自生成器的真实输出**（数值换成代表性的）：字段名、
//! 嵌套层次、`mirrors` 数组的有无，都是生成器实际会写出来的样子。
//!
//! 为什么不是随手写一份 fixture：单元测试里的 `VALID` 是手写的，两边可以一起错。
//! 这里刻意走**公开 API**（[`check_with`]）——那正是应用层用的那条路，也是「生成器
//! 悄悄改了个字段名」唯一能被发现的地方。

use duck_update::{Check, PlatformKey, UpdateConfig, UpdateError, Version, check_with};

/// 生成器输出的形状：镜像地址作主地址，GitHub Release 原址放在 `mirrors` 里回退。
///
/// 三个平台都在，且顺序与生成器的 `sorted(ASSETS)` 一致——这不是巧合，是契约的一部分：
/// 少一个平台，那个平台的用户拿到的是「the manifest has no entry for …」。
const GENERATED: &str = r###"{
  "manifest_version": 1,
  "channel": "stable",
  "version": "0.2.0",
  "notes": "## Duck Toolbox v0.2.0\n\n- 更新器接上了\n- Windows 也能就地更新了",
  "published_at": "2026-10-11T00:00:00Z",
  "min_app_version": "0.1.0",
  "files": {
    "macos-arm64": {
      "url": "https://cdn.example.com/duck/v0.2.0/duck-0.2.0-macos-arm64-app.tar.gz",
      "mirrors": [
        "https://github.com/HanoCode/gpui-toolbox/releases/download/v0.2.0/duck-0.2.0-macos-arm64-app.tar.gz"
      ],
      "sha256": "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3",
      "size": 11569143,
      "signature": null
    },
    "macos-x64": {
      "url": "https://cdn.example.com/duck/v0.2.0/duck-0.2.0-macos-x64-app.tar.gz",
      "mirrors": [
        "https://github.com/HanoCode/gpui-toolbox/releases/download/v0.2.0/duck-0.2.0-macos-x64-app.tar.gz"
      ],
      "sha256": "8ac817331932c2b6bbc40d7e770469955e757a184b896b1dcd1ec4a406b4485b",
      "size": 11234567,
      "signature": null
    },
    "windows-x64": {
      "url": "https://cdn.example.com/duck/v0.2.0/duck-0.2.0-windows-x64.zip",
      "mirrors": [
        "https://github.com/HanoCode/gpui-toolbox/releases/download/v0.2.0/duck-0.2.0-windows-x64.zip"
      ],
      "sha256": "1c3e4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d",
      "size": 12332862,
      "signature": null
    }
  }
}"###;

/// 一份「比线上旧一档」的配置，好让每个平台都能得出「有更新」。
fn config(platform: PlatformKey) -> UpdateConfig {
    UpdateConfig::new(
        "https://example.invalid/update.json",
        "/tmp/duck-wire-format",
    )
    .platform(Some(platform))
    .current(Version::parse("0.1.0").expect("a valid version"))
}

/// 把生成器的文档喂进去，取结论。
fn decide(platform: PlatformKey) -> Check {
    let fetch = |_: &str| Ok::<Vec<u8>, UpdateError>(GENERATED.as_bytes().to_vec());
    check_with(&config(platform), &fetch).expect("the generated document must parse")
}

#[test]
fn the_generators_document_is_readable_by_the_client() {
    let info = decide(PlatformKey::MacosArm64)
        .available()
        .expect("0.2.0 is newer than the built 0.1.0")
        .clone();

    assert_eq!(info.version().to_string(), "0.2.0");
    assert_eq!(info.platform(), PlatformKey::MacosArm64);
    assert_eq!(info.size(), 11_569_143);
    assert_eq!(
        info.sha256(),
        "0a404f822c95e9f3f5a93abdfef8b005017c033eff4d72248f595798afaad1c3"
    );
    assert!(
        info.url().starts_with("https://cdn.example.com/"),
        "the mirror is the primary url: {}",
        info.url()
    );
    assert!(
        info.notes().contains("更新器接上了"),
        "the notes survive the round trip"
    );
}

/// 三个平台各自都要能得出结论：漏一个，那个平台的用户就永远停在旧版。
#[test]
fn every_published_platform_is_reachable() {
    for platform in [
        PlatformKey::MacosArm64,
        PlatformKey::MacosX64,
        PlatformKey::WindowsX64,
    ] {
        let check = decide(platform);
        let info = check
            .available()
            .unwrap_or_else(|| panic!("{platform:?} has no entry in the generated document"));
        assert_eq!(info.platform(), platform);
        assert!(
            info.url().starts_with("https://"),
            "{platform:?} must offer a secure url"
        );
    }
}

/// macOS 那两条必须指向 `-app.tar.gz`，Windows 那条必须指向 `.zip`：
/// 安装器按后缀选解包方式，指错了就是「下载成功但装不上」。
#[test]
fn each_platform_points_at_the_archive_its_installer_expects() {
    let mac_arm = decide(PlatformKey::MacosArm64)
        .available()
        .expect("macos-arm64")
        .clone();
    assert!(mac_arm.url().ends_with("-app.tar.gz"), "{}", mac_arm.url());

    let windows = decide(PlatformKey::WindowsX64)
        .available()
        .expect("windows-x64")
        .clone();
    assert!(windows.url().ends_with(".zip"), "{}", windows.url());
    assert_eq!(windows.platform().archive_extension(), ".zip");
}

/// 生成器立了门槛时客户端要照它办：低于门槛的版本被**明确**拒绝，而不是半懂地装上去。
///
/// 这一条同时钉住两件事：字段名与生成器一致，以及它确实参与判断（而不是被读进来搁着）。
#[test]
fn a_floor_in_the_generated_document_is_enforced() {
    // 门槛卡在「有更新可装、但这台机器够不着」的位置：源上 0.2.0，门槛 0.15.0，本机 0.1.0。
    let raised = GENERATED.replace(
        "\"min_app_version\": \"0.1.0\"",
        "\"min_app_version\": \"0.1.5\"",
    );
    assert_ne!(
        raised, GENERATED,
        "the fixture must carry a floor to rewrite"
    );

    let config = UpdateConfig::new(
        "https://example.invalid/update.json",
        "/tmp/duck-wire-format",
    )
    .platform(Some(PlatformKey::MacosArm64))
    .current(Version::parse("0.1.0").expect("a valid version"));
    let fetch = move |_: &str| Ok::<Vec<u8>, UpdateError>(raised.as_bytes().to_vec());

    let error = check_with(&config, &fetch).expect_err("0.1.0 is below the 0.1.5 floor");
    assert!(
        matches!(&error, UpdateError::AppTooOld { current, required }
            if current == "0.1.0" && required == "0.1.5"),
        "{error:?}"
    );
}

/// 门槛只在「确实有更新可装」时才拦：已是最新时，能不能装是无所谓的。
#[test]
fn a_floor_does_not_turn_up_to_date_into_an_error() {
    let raised = GENERATED.replace(
        "\"min_app_version\": \"0.1.0\"",
        "\"min_app_version\": \"0.9.0\"",
    );
    let config = UpdateConfig::new(
        "https://example.invalid/update.json",
        "/tmp/duck-wire-format",
    )
    .platform(Some(PlatformKey::MacosArm64))
    .current(Version::parse("2.0.0").expect("a valid version"));
    let fetch = move |_: &str| Ok::<Vec<u8>, UpdateError>(raised.as_bytes().to_vec());

    let check = check_with(&config, &fetch).expect("2.0.0 is newer than the offered 0.2.0");
    assert!(
        matches!(check, Check::UpToDate { .. }),
        "expected up-to-date, got {check:?}"
    );
}
