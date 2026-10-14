//! 邮箱页的纯逻辑：地址 / 主题的展示格式化。
//!
//! 与 `terminal_data.rs` 同一形态：**能单测的部分不放进渲染文件**。渲染函数要
//! `Window` 与 `Context`，测不动；而「这个地址该怎么显示」是纯函数，一个断言
//! 就能钉住。混在 `mail.rs` 里只会让两边都变长、还都测不了。
//!
//! # 时间与大小去哪了
//!
//! 列表行时间（`MM-DD HH:MM`）、阅读器时间（`YYYY-MM-DD HH:MM`）与附件大小原先也
//! 在这里，现已下沉到 `duck-theme` 的格式化层（[`duck_theme::datetime_short`] /
//! [`duck_theme::datetime_full`] / [`duck_theme::bytes`]）——与 `plugin_center` 的
//! `format_age` 改走 [`duck_theme::tr_relative_time`] 是同一条收敛路径。
//! **口径没变**：时间仍是 **UTC**（本仓不引 `chrono` / `time`，时区换算是
//! `docs/email-design.md` 里记着的未决项，不是这次收敛的目标）。

/// 发件人显示：显示名 → 地址 → `unknown`（由调用方给本地化占位文案）。
pub fn sender_display<'a>(
    name: Option<&'a str>,
    addr: Option<&'a str>,
    unknown: &'a str,
) -> &'a str {
    name.filter(|s| !s.trim().is_empty())
        .or_else(|| addr.filter(|s| !s.trim().is_empty()))
        .unwrap_or(unknown)
}

/// 地址列表拼成一行（收件人 / 抄送用）。
pub fn join_addresses(list: &[String]) -> String {
    list.join("、")
}

/// 主题为空时给占位——空标题的行在列表里看起来像没加载出来。
pub fn subject_or_placeholder<'a>(subject: &'a str, placeholder: &'a str) -> &'a str {
    if subject.trim().is_empty() {
        placeholder
    } else {
        subject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_display_prefers_name_then_addr_then_placeholder() {
        assert_eq!(sender_display(Some("张三"), Some("z@x.com"), "?"), "张三");
        assert_eq!(sender_display(Some("  "), Some("z@x.com"), "?"), "z@x.com");
        assert_eq!(sender_display(None, Some("z@x.com"), "?"), "z@x.com");
        assert_eq!(sender_display(None, None, "?"), "?");
        assert_eq!(sender_display(Some(""), Some(""), "?"), "?");
    }

    #[test]
    fn subject_placeholder_only_for_blank_subjects() {
        assert_eq!(subject_or_placeholder("", "(无主题)"), "(无主题)");
        assert_eq!(subject_or_placeholder("   ", "(无主题)"), "(无主题)");
        assert_eq!(subject_or_placeholder("hi", "(无主题)"), "hi");
    }

    #[test]
    fn addresses_join_with_the_cjk_separator() {
        assert_eq!(join_addresses(&[]), "");
        assert_eq!(join_addresses(&["a@x.com".into()]), "a@x.com");
        assert_eq!(
            join_addresses(&["a@x.com".into(), "b@x.com".into()]),
            "a@x.com、b@x.com"
        );
    }
}
