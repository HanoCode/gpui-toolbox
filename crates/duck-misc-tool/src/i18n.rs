//! 报告排版的插值助手（与网络族同一份，见 `crates/duck-net-tool/src/i18n.rs` 的说明）。
//!
//! 内核只产出**稳定码 + 细节**（`SchemaProblem` / `NginxError`），整句由本 crate
//! 用译文拼；`fill` 负责把实参塞进译文模板的 `{}`。

/// 把实参按序填进模板的 `{}`。
///
/// 实参多时多出来的丢掉、少时后面的 `{}` 原样留着 —— 两种情况都说明译文与源文
/// 模板不一致，但把整条消息变成空白更糟（用户看不到结果）。
pub(crate) fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    for arg in args {
        let Some((head, tail)) = rest.split_once("{}") else {
            break;
        };
        out.push_str(head);
        out.push_str(arg);
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_placeholders_in_order() {
        assert_eq!(fill("第 {} 行：{}", &["3", "oops"]), "第 3 行：oops");
        assert_eq!(fill("{} 个错误", &["2"]), "2 个错误");
        assert_eq!(fill("{} error(s)", &["2"]), "2 error(s)");
    }

    #[test]
    fn template_without_placeholder_is_returned_as_is() {
        assert_eq!(fill("校验通过", &[]), "校验通过");
        assert_eq!(fill("", &[]), "");
    }

    #[test]
    fn mismatched_counts_never_eat_the_message() {
        assert_eq!(fill("only {}", &["1", "2"]), "only 1");
        assert_eq!(fill("{} and {}", &["1"]), "1 and {}");
    }

    #[test]
    fn escaped_braces_are_left_alone() {
        // 生成的 nginx 配置里 `$uri`、`{` 都是**内容**，插值只认 `{}` 本身。
        assert_eq!(
            fill("keep {{literal}} and {}", &["x"]),
            "keep {{literal}} and x"
        );
    }
}
