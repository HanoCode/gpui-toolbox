//! 报告排版的插值助手。
//!
//! 与 SQL / 合并 / 比对三族的 `i18n.rs` **有意不同**：那几族的 core 会产出整句
//! 中文（`format!("表 '{}' 建议…")`），必须靠「按 `{}` 拆模板反查」才能翻回去；
//! 本族的 core 只产出**稳定码 + 细节**（[`duck_files_core::FilesError`]），
//! 整句话由本 crate 的 `fail()` 用译文拼出来 —— 反查那一套在这里没有输入，
//! 留着只会是死代码（`Renderer` / `normalize` / `match_template` 已因此删除）。
//!
//! 剩下的 `fill` 仍是必需的：报告里的「命中 {} 处」「扫描 {} 个文件」这类模板
//! **来自译文表**，译文里的 `{}` 顺序可能与源串不同（英译常把数字提前），
//! 所以只能是「模板 + 实参」而不是先插值再翻。

/// 把实参按序填进模板的 `{}`。
///
/// 实参比占位符多时**多出来的丢掉**、比占位符少时**后面的 `{}` 原样留着**——
/// 两种情况都说明译文与源文模板不一致，但把整条消息变成空白更糟（用户看不到
/// 检查结果）。留痕交给 `scripts/plugin_locales.py --strict` 的条目数比对。
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

    /// 正常填写：占位符按序吃掉实参。
    #[test]
    fn fills_placeholders_in_order() {
        assert_eq!(fill("命中 {} 处", &["3"]), "命中 3 处");
        assert_eq!(fill("{} → {}", &["a.txt", "b.txt"]), "a.txt → b.txt");
        // 译文换了语序（英译常把数字提前）也要跟着译文的 `{}` 位置走。
        assert_eq!(
            fill("expected {}, found {}", &["3", "5"]),
            "expected 3, found 5"
        );
    }

    /// 无占位符 → 原样返回，不吞参数也不报错。
    #[test]
    fn template_without_placeholder_is_returned_as_is() {
        assert_eq!(fill("没有匹配的文件", &["1"]), "没有匹配的文件");
        assert_eq!(fill("", &[]), "");
    }

    /// 占位符与实参数量不一致：宁可留下 `{}` 也不吞掉整条消息。
    #[test]
    fn mismatched_counts_never_eat_the_message() {
        assert_eq!(fill("only {}", &["1", "2"]), "only 1");
        assert_eq!(fill("{} and {}", &["1"]), "1 and {}");
    }

    /// 转义花括号不是占位符：`{{` 原样留在结果里。
    #[test]
    fn escaped_braces_are_left_alone() {
        assert_eq!(
            fill("keep {{literal}} and {}", &["x"]),
            "keep {{literal}} and x"
        );
    }
}
