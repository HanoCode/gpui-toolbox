//! 把 `duck://` 与 `.ducktool` 登记给操作系统。
//!
//! # 两端为什么长得不一样
//!
//! - **macOS**：协议与文件类型必须写在 bundle 的 `Info.plist` 里 —— LaunchServices
//!   只认清单，运行期没有可做的事。所以那一半在 `sbin/package.sh`。
//! - **Windows**：没有 bundle，靠 `HKCU\Software\Classes` 下的键。写 HKCU 意味着
//!   **不需要管理员权限**，代价是每个用户各自注册一次 —— 这正是便携版想要的：
//!   解压到哪儿就注册成哪儿，不碰机器级配置。
//!
//! # 为什么「该写哪些项」要单独拿出来
//!
//! 写注册表的 API 只有 Windows 有，但「要写哪几个键、值名是什么」没有平台属性。
//! 把它抽成 [`entries`] 之后，这部分就能在**任何平台**上单测 —— 而它恰恰是最容易
//! 写错的地方（路径里少一层 `shell`、值名写成 `URLProtocol`），且错了在开发机上
//! 完全看不出来。平台相关的部分于是退化成十几行直译。
//!
//! 每次启动都写一遍：写注册表是幂等的，而便携版换了目录之后只有这样注册才会跟上。

// 非 Windows 上 [`register_open_handlers`] 是空操作，于是 `entries` 与它用到的三个
// 常量都成了「没人调用的代码」。这不是遗漏：它们由跨平台单测守着（见文件末尾），
// 而单测恰恰是唯一能在开发机上发现「键路径写错」的手段。所以按平台放宽死代码检查。
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// 插件包扩展名（**不带点**）。
///
/// 与 `duck-app/src/deeplink.rs` 的 `PLUGIN_PACKAGE_EXTENSION` 是同一个串。两处
/// 刻意各留一份（`duck-platform` 不该反过来依赖 `duck-app`），一致性由契约测试
/// `tests/packaging_matches_the_code.rs` 盯着。
pub const PACKAGE_EXTENSION: &str = "ducktool";

/// 插件包的 ProgID。
///
/// 中间隔一层 ProgID 而不是把命令直接挂在扩展名键上：一个 ProgID 可以带多个扩展名，
/// 而且「打开方式」列表里显示的是 ProgID 的说明文字，不是键名。
const PACKAGE_PROGID: &str = "DuckToolbox.PluginPackage";

/// 插件包在系统里的显示名（plist 的 `CFBundleTypeName` 与此相同）。
const PACKAGE_LABEL: &str = "Duck Toolbox Plugin Package";

/// 登记 `duck://` 与 `.ducktool` 的打开方式。
///
/// 失败只记日志：注册不上不影响应用自己跑，代价只是「浏览器里点 `duck://` 打不开」
/// 和「双击插件包没反应」——为这个拦下启动不划算。
pub fn register_open_handlers() {
    #[cfg(target_os = "windows")]
    if let Err(error) = register_windows() {
        tracing::warn!(%error, "failed to register the duck:// and .ducktool handlers");
    }
}

/// 一次注册要写下的全部注册表项：`(键路径, 值名, 值)`。
///
/// 空值名表示该键的**默认值**（`RegSetValueExW` 对空名与 NULL 一视同仁）。
///
/// 键路径相对于 `HKEY_CURRENT_USER`；分隔符用 Windows 的 `\`——注册表不认 `/`。
#[must_use]
fn entries(command: &str) -> Vec<(String, &'static str, String)> {
    let scheme = format!(r"Software\Classes\{}", crate::URL_SCHEME);
    let progid = format!(r"Software\Classes\{PACKAGE_PROGID}");
    let extension = format!(r"Software\Classes\.{PACKAGE_EXTENSION}");
    vec![
        // 协议：这三项缺一不可。少了 `URL Protocol`（值就是空串），
        // 系统不认这是一个协议，浏览器里点 `duck://` 不会拉起应用。
        (scheme.clone(), "", format!("URL:{}", crate::URL_SCHEME)),
        (scheme.clone(), "URL Protocol", String::new()),
        (
            format!(r"{scheme}\shell\open\command"),
            "",
            command.to_owned(),
        ),
        // 文件关联：扩展名 → ProgID → 命令行。第二跳负责在「打开方式」里显示名字。
        (extension, "", PACKAGE_PROGID.to_owned()),
        (progid.clone(), "", PACKAGE_LABEL.to_owned()),
        (
            format!(r"{progid}\shell\open\command"),
            "",
            command.to_owned(),
        ),
    ]
}

/// 注册表里的命令行模板。
///
/// 可执行文件路径与 `%1` **都要**加引号：便携版解压在 `C:\Program Files\…` 或
/// 带空格的目录下很常见，少了引号系统会把路径从空格处切开。
#[must_use]
fn command_line(exe: &std::path::Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

#[cfg(target_os = "windows")]
fn register_windows() -> Result<(), String> {
    use windows_registry::CURRENT_USER;

    /// 注册表错误与 IO 错误类型各不相同，而这里唯一要做的是记一条日志——
    /// 统一压成一句话（与 `autostart` 同一个理由：错误类型没公开 re-export）。
    fn failed<E: std::fmt::Display>(error: E) -> String {
        error.to_string()
    }

    let exe = std::env::current_exe().map_err(failed)?;
    let command = command_line(&exe);
    // 先全部写出来再逐条落盘：中途失败会留下半个注册（比如协议键有了、
    // 命令行没写），那种状态比完全没注册更难排查，所以失败时把已写的报出来。
    let entries = entries(&command);
    for (path, name, value) in &entries {
        CURRENT_USER
            .create(path)
            .map_err(failed)?
            .set_string(name, value)
            .map_err(|error| format!("writing {path}\\{name} failed: {}", failed(error)))?;
    }
    tracing::info!(
        count = entries.len(),
        "registered the duck:// and .ducktool handlers"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PACKAGE_EXTENSION, PACKAGE_LABEL, PACKAGE_PROGID, command_line, entries};
    use crate::URL_SCHEME;
    use std::path::Path;

    #[test]
    fn the_scheme_key_carries_all_three_required_values() {
        let found = entries("cmd");
        let scheme = format!(r"Software\Classes\{URL_SCHEME}");

        assert!(found.contains(&(scheme.clone(), "", format!("URL:{URL_SCHEME}"))));
        // 值就是空串，但**这一项必须存在**：少了它系统不认这是协议。
        assert!(found.contains(&(scheme.clone(), "URL Protocol", String::new())));
        assert!(found.contains(&(
            format!(r"{scheme}\shell\open\command"),
            "",
            "cmd".to_owned()
        )));
    }

    #[test]
    fn the_extensions_route_through_a_progid() {
        // 扩展名不直接挂命令行：中间这一跳决定「打开方式」里显示什么名字。
        let found = entries("cmd");
        let extension = format!(r"Software\Classes\.{PACKAGE_EXTENSION}");
        assert!(found.contains(&(extension, "", PACKAGE_PROGID.to_owned())));

        let progid = format!(r"Software\Classes\{PACKAGE_PROGID}");
        assert!(found.contains(&(progid.clone(), "", PACKAGE_LABEL.to_owned())));
        assert!(found.contains(&(
            format!(r"{progid}\shell\open\command"),
            "",
            "cmd".to_owned()
        )));
    }

    #[test]
    fn every_key_is_relative_to_the_current_user_hive() {
        // 漏了没有前缀会让注册跑到别处（或者被 API 拒绝）；前缀多一层也会跑偏。
        for (path, _, _) in entries("cmd") {
            assert!(
                path.starts_with(r"Software\Classes\"),
                "unexpected key path: {path}"
            );
            assert!(
                !path.contains('/'),
                "registry paths use backslashes: {path}"
            );
        }
    }

    #[test]
    fn the_command_line_quotes_both_the_exe_and_the_placeholder() {
        // 便携版常常落在带空格的目录里；少了引号系统会从空格处切开路径。
        assert_eq!(
            command_line(Path::new(r"C:\Program Files\Duck\duck.exe")),
            r#""C:\Program Files\Duck\duck.exe" "%1""#
        );
    }

    #[test]
    fn there_are_no_duplicate_entries() {
        // 同一个 (键, 值名) 写两遍说明有一处是多余的，而后者会静默覆盖前者。
        let found = entries("cmd");
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for (path, name, _) in &found {
            let pair = (path.as_str(), *name);
            assert!(
                !seen.contains(&pair),
                "duplicate registry entry: {path}\\{name}"
            );
            seen.push(pair);
        }
    }
}
