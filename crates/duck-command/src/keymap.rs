//! 键位：把所有动作的默认键位绑进 gpui 键盘系统，并支持用户覆盖与冲突检测。
//!
//! 键位串约定：`$mod` 代表平台主修饰键（macOS 是 ⌘，其他平台是 Ctrl），因此同一份
//! 声明在两个平台都自然。例 `"$mod-p"`、`"$mod-shift-p"`。
//!
//! 用户覆盖：配置文件 `<配置目录>/keybindings.toml`，形如
//! ```toml
//! "toolbox.refresh" = ["$mod-r"]
//! ```
//! 出现该文件时，表中列出的动作改用用户键位；未列出的沿用默认值。

use std::collections::HashMap;

use gpui::{App, KeyBinding};

use crate::action::{RunCommand, metas};

/// 平台主修饰键名（gpui 键位串里用）。
#[must_use]
pub fn primary_modifier() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    }
}

/// 把声明用的 `$mod` 展开成当前平台的主修饰键。
#[must_use]
pub fn resolve(keys: &str) -> String {
    keys.replace("$mod", primary_modifier())
}

/// 绑定全部键位（在动作登记完成后调用一次）。
pub fn bind_all(cx: &mut App) {
    let overrides = load_overrides();
    // 先收集成拥有所有权的数据，结束对 cx 的不可变借用。
    let plans: Vec<(&'static str, Vec<String>)> = metas(cx)
        .iter()
        .map(|meta| {
            let keys = overrides
                .get(meta.id)
                .cloned()
                .unwrap_or_else(|| meta.keys.iter().map(|key| resolve(key)).collect());
            (meta.id, keys)
        })
        .collect();

    let mut seen: HashMap<String, &'static str> = HashMap::new();
    let mut bindings: Vec<KeyBinding> = Vec::new();
    for (id, keys) in plans {
        for key in keys {
            if let Some(previous) = seen.insert(key.clone(), id) {
                tracing::warn!(key = %key, previous, id, "keybinding conflict: one keystroke bound to two actions");
            }
            bindings.push(KeyBinding::new(&key, RunCommand { id: id.into() }, None));
        }
    }
    cx.bind_keys(bindings);
}

/// 某个动作当前生效的键位（已展开 `$mod`，供提示 / 菜单显示）。
#[must_use]
pub fn keybindings_for(cx: &App, id: &str) -> Vec<String> {
    let overrides = load_overrides();
    if let Some(keys) = overrides.get(id) {
        return keys.clone();
    }
    metas(cx)
        .iter()
        .find(|meta| meta.id == id)
        .map(|meta| meta.keys.iter().map(|key| resolve(key)).collect())
        .unwrap_or_default()
}

/// 把键位串渲染成界面上的写法：`cmd-shift-p` → macOS 的 `⌘⇧P` / 其余平台的 `Ctrl+Shift+P`。
///
/// 与 [`keybindings_for`] 配对使用：角标、菜单提示这类「显示给用户看的键位」都必须走
/// 这里，而不是在界面里写死一个 `⌘K` —— 用户改过 `keybindings.toml` 之后，写死的那份
/// 会与**真正生效**的键位对不上，变成「界面上写着 ⌘K、按下去没反应」。
///
/// macOS 的修饰键顺序固定为 `⌃⌥⇧⌘`（Apple 的唯一写法，与声明顺序无关）。
/// 认不出的修饰键**原样返回整串**：显示得丑一点，好过静默丢掉一段。
#[must_use]
pub fn pretty(keys: &str) -> String {
    // gpui 的键位串形如 `修饰键-修饰键-主键`，主键永远在最后一段。
    let mut parts: Vec<&str> = keys.split('-').collect();
    let key = parts.pop().unwrap_or_default();
    let (mut control, mut alt, mut shift, mut command) = (false, false, false, false);
    for part in parts {
        match part {
            "ctrl" | "control" => control = true,
            "alt" | "opt" | "option" => alt = true,
            "shift" => shift = true,
            "cmd" | "command" | "super" | "win" => command = true,
            _ => return keys.to_owned(),
        }
    }

    let mut out = String::new();
    if cfg!(target_os = "macos") {
        if control {
            out.push('⌃');
        }
        if alt {
            out.push('⌥');
        }
        if shift {
            out.push('⇧');
        }
        if command {
            out.push('⌘');
        }
    } else {
        for (on, label) in [
            (control, "Ctrl+"),
            (alt, "Alt+"),
            (shift, "Shift+"),
            (command, "Win+"),
        ] {
            if on {
                out.push_str(label);
            }
        }
    }
    out.push_str(&main_key(key));
    out
}

/// 主键的显示写法：单字符键大写，其余首字母大写（`space` → `Space`）。
fn main_key(key: &str) -> String {
    let mut chars = key.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// 读取用户键位覆盖；文件缺失或解析失败时返回空表（用默认键位）。
fn load_overrides() -> HashMap<String, Vec<String>> {
    let path = duck_platform::config_dir().join("keybindings.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match toml::from_str::<HashMap<String, Vec<String>>>(&text) {
        Ok(map) => {
            tracing::info!(path = %path.display(), entries = map.len(), "loaded user keybinding overrides");
            map
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to parse keybinding overrides, using defaults");
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{pretty, resolve};

    #[test]
    fn expands_primary_modifier() {
        let resolved = resolve("$mod-shift-p");
        assert!(!resolved.contains("$mod"));
        if cfg!(target_os = "macos") {
            assert_eq!(resolved, "cmd-shift-p");
        } else {
            assert_eq!(resolved, "ctrl-shift-p");
        }
    }

    #[test]
    fn leaves_plain_keys_untouched() {
        assert_eq!(resolve("escape"), "escape");
    }

    /// 渲染走平台惯例：macOS 用符号，其余平台用 `+` 连接的缩写。
    #[test]
    fn pretty_follows_the_platform_idiom() {
        if cfg!(target_os = "macos") {
            assert_eq!(pretty("cmd-k"), "⌘K");
            // 顺序是 Apple 的 `⌃⌥⇧⌘`，不是声明顺序 —— 所以 ⇧ 在 ⌘ 左边。
            assert_eq!(pretty("cmd-shift-p"), "⇧⌘P");
            assert_eq!(pretty("cmd-shift-alt-ctrl-k"), "⌃⌥⇧⌘K");
        } else {
            assert_eq!(pretty("ctrl-k"), "Ctrl+K");
            assert_eq!(pretty("ctrl-shift-alt-k"), "Ctrl+Alt+Shift+K");
        }
        // 无修饰键的单键照旧可读。
        assert_eq!(pretty("escape"), "Escape");
        assert_eq!(pretty("space"), "Space");
    }

    /// 认不出的修饰键原样返回整串 —— 不要静默丢掉一段，那会让角标撒谎。
    #[test]
    fn pretty_keeps_unrecognised_modifiers_verbatim() {
        assert_eq!(pretty("hyper-k"), "hyper-k");
    }
}
