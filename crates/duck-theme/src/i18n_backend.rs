//! rust-i18n 后端装载与「静态投影」——[`crate::i18n`] 的英译数据源。
//!
//! 目的（为什么要有这一层）：
//! 1. `duck_theme::i18n::t()` 的签名是 `-> &'static str`（全仓 3000+ 处调用点依赖它），
//!    而 rust-i18n 的 `Backend::translate` 返回 `Cow<'_, str>`。这里的投影把
//!    `messages_for_locale` 的结果一次性收成 `HashMap<&'static str, &'static str>`，
//!    从而在**不改任何签名**的前提下换成 TOML 数据源。
//! 2. `i18n!` 宏会在调用处生成一批 `_rust_i18n_*` 项。把宏关在**私有 module** 内，
//!    生成的 `pub fn _rust_i18n_backend()` 就只在 crate 内可见，不会污染对外 API。
//!
//! 取 `&'static str` 的依据：宏为「条目数 > 256」的表生成 `_rust_i18n_unpack_translations`，
//! 它把翻译值以 `Cow::Borrowed(&'static str)` 存进 `HashMap`（见
//! `rust-i18n-macro 4.2.4/src/lib.rs`），因此取出时必然是借用。仍保留 `Box::leak`
//! 兜底：若上游哪天改成 `Owned`，宁可泄漏一次也不要退化成「查不到 → 静默回落中文」。
//!
//! 语言切换：`rust_i18n::set_locale` 是 rust-i18n **crate 私有**的全局
//! （`rust-i18n 4.2.4/src/lib.rs` 的 `CURRENT_LOCALE`）。`gpui-component` 依赖的是
//! `rust-i18n = "4.2.0"`，本 crate 依赖 `"4"`，cargo 会统一成**同一份 4.2.4**，
//! 于是两边共用同一个 locale 全局——切换语言时上游内置文案也会跟着走。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

// 编译期读取 `crates/duck-theme/locales/*.{toml,yml,json}` 并生成 `_rust_i18n_*`。
//
// 必须在**本 crate 内恰好调用一次**：宏生成的是 `static` 项，调两次会重复定义。
// 「条目数 > 256」的表走 `_rust_i18n_unpack_translations` 的紧凑编码，不会把
// 2600+ 条翻译摊成 2600+ 行生成代码。
rust_i18n::i18n!("locales", fallback = "en");

/// 英文静态投影表（进程内只建一次）。
static EN_TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

/// 把 `Cow<'static, str>` 归一成 `&'static str`；`Owned` 分支只可能命中一次（首次投影）。
fn into_static(value: Cow<'static, str>) -> &'static str {
    match value {
        Cow::Borrowed(text) => text,
        Cow::Owned(text) => Box::leak(text.into_boxed_str()),
    }
}

/// 英文投影表（惰性构建）。
fn en_table() -> &'static HashMap<&'static str, &'static str> {
    EN_TABLE.get_or_init(|| {
        let entries = _rust_i18n_backend()
            .messages_for_locale("en")
            .unwrap_or_default();
        let mut table = HashMap::with_capacity(entries.len());
        for (key, value) in entries {
            table.insert(into_static(key), into_static(value));
        }
        table
    })
}

/// 查英文译文；miss 返回 `None`，由调用方回落到中文源串。
pub(crate) fn lookup_en(key: &str) -> Option<&'static str> {
    en_table().get(key).copied()
}

#[cfg(test)]
mod tests {
    /// 由 `scripts/i18n_en_toml.py` 从 `locales/en.toml` **同步产出**的 key 清单。
    const EN_KEYS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/en_keys.txt"
    ));

    fn listed_keys() -> Vec<&'static str> {
        EN_KEYS.lines().filter(|line| !line.is_empty()).collect()
    }

    /// 整套迁移的核心验收：**每一条中文源串都能在 rust-i18n 后端里查到**。
    ///
    /// 这条断言是整套迁移的守门人——rust-i18n 查不到时**不会报错**，而是回落源串，
    /// 表现为「英文界面静默显示中文」，编译过、其他守卫也绿。
    #[test]
    fn every_listed_key_resolves_through_the_projection() {
        let keys = listed_keys();
        assert!(
            !keys.is_empty(),
            "key 清单为空：先跑 scripts/i18n_en_toml.py"
        );

        let misses: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| super::lookup_en(key).is_none())
            .collect();

        assert!(
            misses.is_empty(),
            "以下 {} / {} 条 key 在投影表里查不到（英文界面会静默显示中文）：\n{}",
            misses.len(),
            keys.len(),
            misses.join("\n")
        );
    }

    /// 后端条数必须与 key 清单**完全相等**：多一条说明 TOML 里有清单外的残留，
    /// 少一条说明 flatten 把某条 key 改了形（例如被当成 dotted key 展开成嵌套表）。
    #[test]
    fn backend_and_key_list_agree_on_the_entry_count() {
        let backend = super::_rust_i18n_backend()
            .messages_for_locale("en")
            .map_or(0, |entries| entries.len());

        assert_eq!(
            backend,
            listed_keys().len(),
            "en.toml 的可查条目数与 key 清单不一致 → 重跑 scripts/i18n_en_toml.py"
        );
    }

    /// 端到端抽验：拿两条真实文案走「后端 → 取出」的完整路径。
    #[test]
    fn known_messages_resolve_to_english() {
        for (source, expected) in [("外观", "Appearance"), ("主题", "Theme")] {
            assert_eq!(
                super::lookup_en(source),
                Some(expected),
                "「{source}」应译为「{expected}」"
            );
        }
    }

    /// 未登记的中文串必须 miss（而不是误命中别的条目），这样 `t()` 才能安全回落源串。
    #[test]
    fn unregistered_text_misses_instead_of_mis_hitting() {
        assert_eq!(super::lookup_en("这条文案肯定不在表里 zzz"), None);
    }

    /// 本方案的一个设计支柱：`gpui-component` 自己也依赖 `rust-i18n = "4.2.0"`，
    /// 只要 `Cargo.lock` 里 **`rust-i18n` 只有一份**，两边就共用同一个
    /// `rust_i18n::CURRENT_LOCALE` 全局 —— 切语言时上游内置文案会跟着走。
    ///
    /// 一旦上游升到 5.x 而本 crate 还停在 4.x，就会同时存在两个后端、两个 locale 全局，
    /// 切语言会出现「界面切了、日期选择器的月份没切」这类不一致，且**没有任何编译错误**。
    #[test]
    fn only_one_rust_i18n_version_is_locked() {
        let lock_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock");
        let lock = std::fs::read_to_string(lock_path).expect("应能读到 workspace 的 Cargo.lock");

        let versions: Vec<&str> = lock
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| pair[0] == r#"name = "rust-i18n""#)
            .map(|pair| pair[1].trim())
            .collect();

        assert_eq!(
            versions.len(),
            1,
            "Cargo.lock 里有 {} 份 rust-i18n（{versions:?}）—— 请把本 crate 的 \
             rust-i18n 版本与 gpui-component 对齐，否则会有两个 locale 全局",
            versions.len()
        );
    }
}
