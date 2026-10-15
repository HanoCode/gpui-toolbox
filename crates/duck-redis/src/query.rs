//! 键名查询的**纯逻辑**：查询模式、glob 转义与 `SCAN MATCH` 串构造。
//!
//! 移植自 zedis（`zedis-core/src/string.rs::escape_glob` +
//! `temp/zedis/src/states/server/key.rs::keyword_scan_pattern` / `QueryMode`）。
//! 本模块零网络、零 gpui：`MATCH` 串怎么拼、哪种模式根本不该扫，
//! 都可以用普通 `#[test]` 钉死，网络层只剩「把串发出去」。

use std::borrow::Cow;

/// Redis glob 的元字符。`\` 自己也在表里——它既是转义符也要被转义。
const GLOB_SPECIAL: [char; 5] = ['*', '?', '[', ']', '\\'];

/// 键名查询模式（对齐 zedis `QueryMode`，符号 `*` / `^` / `=`）。
///
/// **`All` 不是「用户自己写 glob」**：三档都先把输入当**字面量**转义，再按档位
/// 决定包不包通配符。这样 `user[1]:` 这种带元字符的键名在任何档位都指向它自己，
/// 不会因为 `[1]` 被当成字符集而无声无息地匹配到别的键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum QueryMode {
    /// 子串匹配：`*字面量*`。缺省档，也是唯一「不知道键长什么样」时的合理档。
    #[default]
    All,
    /// 前缀匹配：`字面量*`。按命名空间下钻时最省服务端算力（`MATCH` 在
    /// 服务端是逐键比对，前缀窄了命中率就低）。
    Prefix,
    /// 精确匹配：**不扫描**，按键名直接取（见 [`ScanPlan::Exact`]）。
    Exact,
}

impl QueryMode {
    /// 全部档位（顺序即菜单顺序）。
    pub const ALL: [Self; 3] = [Self::All, Self::Prefix, Self::Exact];

    /// 档位符号（输入框前缀按钮上显示的那一个字符）。
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::All => "*",
            Self::Prefix => "^",
            Self::Exact => "=",
        }
    }

    /// 稳定 wire 值（菜单项 id / 连接档案持久化用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Prefix => "prefix",
            Self::Exact => "exact",
        }
    }

    /// 解析 wire 值；也接受符号本身（`*` / `^` / `=`）与未知值回落 [`QueryMode::All`]。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value.trim() {
            "prefix" | "^" => Self::Prefix,
            "exact" | "=" => Self::Exact,
            _ => Self::All,
        }
    }
}

/// 把 Redis glob 元字符转义，让一段**字面量**在 `MATCH` 里只匹配它自己。
///
/// 没有元字符时返回借用（绝大多数键名都走这条，零分配）。
#[must_use]
pub fn escape_glob(text: &str) -> Cow<'_, str> {
    if !text.contains(GLOB_SPECIAL) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 4);
    for ch in text.chars() {
        if GLOB_SPECIAL.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    Cow::Owned(out)
}

/// 一次查询的执行计划。
///
/// 单独成一个枚举而不是「返回 `Option<String>`」，是因为**空关键字**与
/// **精确档**都会得到「没有 `MATCH` 串」，但两者要做的事完全相反：
/// 前者要扫全库，后者一个键都不该扫（扫了也是白扫，还得靠名字去挑）。
/// 让调用方 `match` 三态，编译器就能挡住「把精确档当全库扫」这种错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanPlan {
    /// 不带 `MATCH` 地扫全库（关键字为空）。
    Everything,
    /// 带 `MATCH <串>` 扫（子串 / 前缀档）。
    Pattern(String),
    /// 不扫描：按键名直接取这一个键。
    Exact,
}

/// 由关键字与查询模式得出执行计划。
///
/// 空关键字**在三档下都是 [`ScanPlan::Everything`]**：`MATCH **` 与不带 `MATCH`
/// 结果一样，但少一个参数、也让服务端少一次比对；而「精确档 + 空关键字」等于
/// 「找一个空名字的键」，那是用户没填完，不该真去查。
#[must_use]
pub fn scan_plan(keyword: &str, mode: QueryMode) -> ScanPlan {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return ScanPlan::Everything;
    }
    match mode {
        QueryMode::Exact => ScanPlan::Exact,
        QueryMode::All => ScanPlan::Pattern(format!("*{}*", escape_glob(keyword))),
        QueryMode::Prefix => ScanPlan::Pattern(format!("{}*", escape_glob(keyword))),
    }
}

#[cfg(test)]
mod tests {
    use super::{QueryMode, ScanPlan, escape_glob, scan_plan};

    /// 无元字符的串原样借用，有元字符的才转义。
    #[test]
    fn escape_glob_makes_a_name_mean_itself() {
        assert!(matches!(
            escape_glob("user:1:"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(escape_glob("user[1]:"), "user\\[1\\]:");
        assert_eq!(escape_glob("a*b?c\\d"), "a\\*b\\?c\\\\d");
        // 转义后再补通配符，匹配的就只是这个前缀本身。
        assert_eq!(format!("{}*", escape_glob("h[o]t:")), "h\\[o\\]t:*");
    }

    /// 三档的 `MATCH` 串：子串 / 前缀 / 不扫。
    #[test]
    fn scan_plan_per_mode() {
        assert_eq!(
            scan_plan("user", QueryMode::All),
            ScanPlan::Pattern("*user*".to_owned())
        );
        assert_eq!(
            scan_plan("user", QueryMode::Prefix),
            ScanPlan::Pattern("user*".to_owned())
        );
        assert_eq!(scan_plan("user:1", QueryMode::Exact), ScanPlan::Exact);
    }

    /// 关键字前后空白不影响判定（输入框里最常见的误操作）。
    #[test]
    fn keyword_is_trimmed() {
        assert_eq!(
            scan_plan("  user  ", QueryMode::Prefix),
            ScanPlan::Pattern("user*".to_owned())
        );
        for mode in QueryMode::ALL {
            assert_eq!(
                scan_plan("   ", mode),
                ScanPlan::Everything,
                "{mode:?} 档下空关键字一律扫全库"
            );
        }
    }

    /// 元字符不会漏进 `MATCH` 串：子串档也要先转义再包通配。
    #[test]
    fn metacharacters_are_escaped_in_both_pattern_modes() {
        assert_eq!(
            scan_plan("a[1]", QueryMode::All),
            ScanPlan::Pattern("*a\\[1\\]*".to_owned())
        );
        assert_eq!(
            scan_plan("a[1]", QueryMode::Prefix),
            ScanPlan::Pattern("a\\[1\\]*".to_owned())
        );
    }

    /// 符号 / wire 值双向可认（菜单 id 用 wire，输入框前缀按钮用符号）。
    #[test]
    fn mode_roundtrip_and_symbols() {
        for mode in QueryMode::ALL {
            assert_eq!(QueryMode::from_wire(mode.wire_name()), mode);
            assert_eq!(QueryMode::from_wire(mode.symbol()), mode);
        }
        assert_eq!(QueryMode::All.symbol(), "*");
        assert_eq!(QueryMode::Prefix.symbol(), "^");
        assert_eq!(QueryMode::Exact.symbol(), "=");
        assert_eq!(QueryMode::from_wire("nope"), QueryMode::All);
        assert_eq!(QueryMode::from_wire("  ^  "), QueryMode::Prefix);
    }
}
