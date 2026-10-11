//! 打包脚本与代码里的「对外标识」必须一致。
//!
//! `duck://` 这个方案名与 `.ducktool` 这个扩展名各自出现在**两处**：
//!
//! | 位置 | 作用 |
//! | --- | --- |
//! | Rust 常量（`URL_SCHEME` / `PACKAGE_EXTENSION`） | 应用自己的解析 |
//! | `sbin/package.sh` 生成的 `Info.plist` | 系统把这二者派给谁 |
//!
//! 任意一处打错，症状都是**静默**的：浏览器点 `duck://` 没反应、双击插件包没反应，
//! 而且只有在 macOS 上才看得见，改的人多半正在别的平台上开发。这条测试把两处钉在一起
//! ——它是这个文件唯一的理由，所以它宁可读源码文本，也不为此把常量导出成构建期依赖。
//!
//! 第三处（Windows 的 `HKCU\Software\Classes`）不在这里守着：它由
//! `handlers.rs` 的 `entries` 单测覆盖，那部分代码本身就用同一组常量。

use std::path::{Path, PathBuf};

/// 仓库根目录：`tests/` 在 `crates/duck-platform/` 下，往上两层即可。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root should be reachable from the crate directory")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// 取出 `<key>{key}</key>` 之后**第一个** `<array>` … `</array>` 之间的内容。
///
/// 只看紧跟在关键字之后的那一段，不在全文件里搜字符串：`<string>duck</string>`
/// 这种短串在 plist 里出现两次也不会互相干扰。
///
/// 前提是被查的 key 后面直接跟一个**没有嵌套**的数组——`CFBundleURLSchemes` 与
/// `CFBundleTypeExtensions` 都满足（外层那个 `CFBundleDocumentTypes` 才是嵌套的）。
fn array_after(text: &str, key: &str) -> String {
    let anchor = format!("<key>{key}</key>");
    let start = text
        .find(&anchor)
        .unwrap_or_else(|| panic!("{key} is missing from the packaging script"));
    let rest = &text[start + anchor.len()..];
    let open = rest
        .find("<array>")
        .unwrap_or_else(|| panic!("{key} should be followed by an array"));
    let close = rest[open..]
        .find("</array>")
        .unwrap_or_else(|| panic!("the array after {key} should be closed"));
    rest[open..open + close].to_owned()
}

#[test]
fn the_macos_bundle_registers_our_url_scheme() {
    let script = read("sbin/package.sh");
    let schemes = array_after(&script, "CFBundleURLSchemes");
    let expected = format!("<string>{}</string>", duck_platform::URL_SCHEME);
    assert!(
        schemes.contains(&expected),
        "the bundle does not register {expected}; CFBundleURLSchemes is {schemes}"
    );
}

#[test]
fn the_macos_bundle_registers_our_package_extension() {
    let script = read("sbin/package.sh");
    let extensions = array_after(&script, "CFBundleTypeExtensions");
    let expected = format!("<string>{}</string>", duck_platform::PACKAGE_EXTENSION);
    assert!(
        extensions.contains(&expected),
        "the bundle does not claim {expected}; CFBundleTypeExtensions is {extensions}"
    );
}

#[test]
fn the_package_extension_matches_the_app_side_one() {
    // `duck-app` 的识别逻辑用的是它自己那份常量（两处刻意各留一份：`duck-platform`
    // 不该反过来依赖 `duck-app`）。不一致的后果是「系统认得这个文件、应用认不出」，
    // 也就是双击之后什么都没发生。
    let source = read("crates/duck-app/src/deeplink.rs");
    let expected = format!(
        "PLUGIN_PACKAGE_EXTENSION: &str = \"{}\";",
        duck_platform::PACKAGE_EXTENSION
    );
    assert!(
        source.contains(&expected),
        "crates/duck-app/src/deeplink.rs should declare {expected}"
    );
}
