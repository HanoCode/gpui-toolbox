//! 把 core 产出的**已插值**中文串翻成当前界面语言。
//!
//! 为什么需要这一层：`sql_checker` 逐字复刻自参考仓，规则消息是 `format!` 出来的
//! 中文（模板 `表 '{}' 建议使用小写+下划线`，渲染成 `表 'T_ORDER' 建议使用小写+下划线`）。
//! 拿**渲染后**的串去查插件译文表必然 miss——表里存的键是**模板**。所以这里多走一步：
//! 先精确查，不中再按 `{}` 把模板拆开去匹配，命中后把参数填进译文模板。
//!
//! 这一层只做字符串重排，**不碰规则逻辑**：改消息文案的生成方式＝重新验证 28 条规则，
//! 正是移植要避免的成本（见 `duck-sql-core` 各模块头）。

use duck_plugin::PluginLocales;

/// 取词器：给定语言，把 core 的渲染产物翻回去。
pub struct Renderer<'a> {
    locales: &'a PluginLocales,
    lang: &'a str,
}

impl<'a> Renderer<'a> {
    #[must_use]
    pub fn new(locales: &'a PluginLocales, lang: &'a str) -> Self {
        Self { locales, lang }
    }

    /// 精确命中 → 译文；否则按模板反查；都不中 → 原串（中文界面下自然全走这条）。
    ///
    /// 模板反查取**第一个**命中：`entries` 按源串升序，顺序稳定，不存在
    /// 「有时翻、有时不翻」的抖动。两条模板同时能匹配同一串的概率可忽略，
    /// 真出现了也只是选到另一条同义译文，不会丢参数。
    #[must_use]
    pub fn translate(&self, rendered: &str) -> String {
        if let Some(text) = self.locales.lookup(self.lang, rendered) {
            return text.to_owned();
        }
        for (template, text) in self.locales.entries(self.lang) {
            // 带格式说明的占位符（`{:?}` / `{:.1}`）在渲染后只剩值：两边都归一
            // 成 `{}` 才能对上（参数已经是格式化好的串，填回去正好）。
            let source = normalize(template);
            let target = normalize(text);
            // 双方都得有占位符：单边带 `{}` 时匹配没有意义（要么是静态串，
            // 上面精确查早该命中；要么译文把参数写死了，填不进去）。
            if !source.contains("{}") || !target.contains("{}") {
                continue;
            }
            let Some(args) = match_template(&source, rendered) else {
                continue;
            };
            return fill(&target, &args);
        }
        rendered.to_owned()
    }
}

/// 把 `format!` 的各种占位符统一成 `{}`。
///
/// `sql_merge` 这类复刻件里有 `{:?}` / `{:.1}` / `{}` 混合的模板；渲染出去的
/// 消息只剩值，所以反查模板时必须把两边占位符都归一化。`{{`（转义花括号）
/// 原样保留——它渲染出来就是一个 `{`，本来就不是占位符。
fn normalize(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut index = 0;
    while index < template.len() {
        // `{{` / `}}` 是转义花括号（渲染出来就是一个 `{` / `}`），成对吃掉，
        // 否则 `{{literal}}` 里第二个 `{` 会被当成占位符的开头。
        if template[index..].starts_with("{{") || template[index..].starts_with("}}") {
            out.push_str(&template[index..index + 2]);
            index += 2;
            continue;
        }
        // 占位符的开头必是 `{`；先算出「闭合 `}` 在哪」，再走 `if let`——
        // 嵌套 `if .. if let` 会被 clippy 要求合并成 let_chains，而那是 1.88 的语法，
        // 本仓 MSRV 是 1.85。
        let closing = if template.as_bytes()[index] == b'{' {
            template[index + 1..].find('}')
        } else {
            None
        };
        if let Some(offset) = closing {
            let inner = &template[index + 1..index + 1 + offset];
            if !inner.contains('{') {
                out.push_str("{}");
                index += offset + 2;
                continue;
            }
        }
        // 按字符边界推进，别把多字节字符切开。
        let ch = template[index..]
            .chars()
            .next()
            .expect("index is in bounds");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// 拿模板（含 `{}`）去匹配一条已渲染的串，成功则返回各占位符的实参。
///
/// 段间取**最短**匹配（`find` 首次出现）：规则消息里参数本身通常不含模板里的
/// 固定片段（`'`、`，`、`:`），最短匹配足够，也比正则回溯便宜。
fn match_template<'r>(template: &str, rendered: &'r str) -> Option<Vec<&'r str>> {
    let parts: Vec<&str> = template.split("{}").collect();
    if parts.len() < 2 {
        return None;
    }
    let mut args = Vec::with_capacity(parts.len() - 1);
    let mut rest = rendered.strip_prefix(parts[0])?;
    for part in &parts[1..parts.len() - 1] {
        let index = rest.find(part)?;
        args.push(&rest[..index]);
        rest = &rest[index + part.len()..];
    }
    // 最后一段是后缀：它之后的整段都是最后一个实参。
    args.push(rest.strip_suffix(parts[parts.len() - 1])?);
    Some(args)
}

/// 把实参按序填进译文模板的 `{}`。
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
    use std::path::PathBuf;

    /// 把一张表写成临时插件目录再装载：走的是真的 `PluginLocales::load`，
    /// 不是伪造的结构体（伪造的话「装载期丢弃空译文」这类行为测不到）。
    ///
    /// 目录名带 `name`：同一进程里的测试是**并行**跑的，共用 `pid` 命名会互相
    /// `remove_dir_all` 掉对方的译文（症状就是查表全 miss、回落中文源串）。
    fn renderer(name: &str, tag: &'static str, table: &str) -> (PluginLocales, PathBuf) {
        let dir = std::env::temp_dir().join(format!("duck-compare-tool-i18n-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let locales = dir.join("locales");
        std::fs::create_dir_all(&locales).expect("建临时目录");
        std::fs::write(locales.join(format!("{tag}.toml")), table).expect("写临时译文");
        (PluginLocales::load(&dir, "zh-Hans", None), dir)
    }

    /// 静态消息：精确查表，一个字节都不该被模板逻辑碰。
    #[test]
    fn static_messages_hit_exactly() {
        let (locales, dir) = renderer(
            "static",
            "en",
            "\"DELETE语句缺少WHERE条件\" = \"DELETE has no WHERE\"\n",
        );
        let renderer = Renderer::new(&locales, "en");
        assert_eq!(
            renderer.translate("DELETE语句缺少WHERE条件"),
            "DELETE has no WHERE"
        );
        // 没登记的串原样返回，不变成空串。
        assert_eq!(renderer.translate("这条没登记"), "这条没登记");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 插值后的消息：按模板反查，再把参数搬进译文。
    #[test]
    fn interpolated_messages_are_matched_by_template() {
        let (locales, dir) = renderer(
            "template",
            "en",
            "\"表 '{}' 建议使用小写+下划线\" = \"Table '{}' should use lower_snake_case\"\n\
             \"疑似关键字拼写错误: '{}'，正确写法: '{}'\" = \"Suspected typo: '{}', use '{}'\"\n\
             \"列数不匹配：期望{}列，实际{}列\" = \"Column count mismatch: expected {}, found {}\"\n",
        );
        let renderer = Renderer::new(&locales, "en");
        assert_eq!(
            renderer.translate("表 'T_ORDER' 建议使用小写+下划线"),
            "Table 'T_ORDER' should use lower_snake_case"
        );
        // 两个参数：顺序必须保持（第一条是错的写法、第二条才是对的）。
        assert_eq!(
            renderer.translate("疑似关键字拼写错误: 'SELEC'，正确写法: 'SELECT'"),
            "Suspected typo: 'SELEC', use 'SELECT'"
        );
        // 全角冒号是源串的一部分，模板匹配不能把它当分隔符弄丢。
        assert_eq!(
            renderer.translate("列数不匹配：期望3列，实际5列"),
            "Column count mismatch: expected 3, found 5"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// 中文界面（源串语言）没有译文表 → 全部原样返回，不做任何猜测性替换。
    #[test]
    fn source_language_passes_through_untouched() {
        let (locales, dir) = renderer(
            "source-lang",
            "en",
            "\"表 '{}' 建议使用小写+下划线\" = \"Table '{}' …\"\n",
        );
        let renderer = Renderer::new(&locales, "zh-Hans");
        assert_eq!(
            renderer.translate("表 'T_ORDER' 建议使用小写+下划线"),
            "表 'T_ORDER' 建议使用小写+下划线"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// 模板匹配的边界：前缀/后缀必须对上、段间取最短、参数可以为空。
    #[test]
    fn template_matching_is_anchored_and_lazy() {
        assert_eq!(
            match_template("a{}b{}c", "a1b2c"),
            Some(vec!["1", "2"].into_iter().collect::<Vec<_>>())
        );
        // 段间的固定片段重复出现时取**第一次**：`x` 前后各一个 `|`。
        assert_eq!(
            match_template("{}|{}", "x|y|z"),
            Some(vec!["x", "y|z"].into_iter().collect::<Vec<_>>())
        );
        // 前缀/后缀不对 → 不命中（否则会把不相干的串当成同一条模板）。
        assert_eq!(match_template("a{}b", "X1b"), None);
        assert_eq!(match_template("a{}b", "a1B"), None);
        // 空实参：`a` + `` + `b`。
        assert_eq!(
            match_template("a{}b", "ab"),
            Some(vec![""].into_iter().collect::<Vec<_>>())
        );
        // 无占位符的模板不参与匹配。
        assert_eq!(match_template("abc", "abc"), None);
    }

    /// 译文占位符数量与源文不一致时的兜底：宁可留下 `{}` 也不吞掉消息。
    #[test]
    fn mismatched_placeholder_counts_never_eat_the_message() {
        assert_eq!(fill("only {}", &["1", "2"]), "only 1");
        assert_eq!(fill("{} and {}", &["1"]), "1 and {}");
        assert_eq!(fill("no placeholder", &["1"]), "no placeholder");
    }

    /// 带格式说明的占位符（`{:?}` / `{:.1}`）也要能反查：渲染后只剩值，
    /// 模板匹配前必须归一化成 `{}`。
    #[test]
    fn format_specs_are_normalised_before_matching() {
        let (locales, dir) = renderer(
            "format-spec",
            "en",
            "\"扫描到 {} 个包: {:?}\" = \"Scanned {} packages: {:?}\"\n\
             \"✅ 组完成: {} ({} 个合并文件, 耗时 {:.1}s)\" = \"✅ group done: {} ({} files, {:.1}s)\"\n",
        );
        let renderer = Renderer::new(&locales, "en");
        assert_eq!(
            renderer.translate("扫描到 3 个包: [\"a\", \"b\"]"),
            "Scanned 3 packages: [\"a\", \"b\"]"
        );
        assert_eq!(
            renderer.translate("✅ 组完成: g1 (2 个合并文件, 耗时 0.4s)"),
            "✅ group done: g1 (2 files, 0.4s)"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// 转义花括号不是占位符：`{{` 归一化后仍是 `{{`。
    #[test]
    fn escaped_braces_survive_normalisation() {
        assert_eq!(
            normalize("keep {{literal}} and {}"),
            "keep {{literal}} and {}"
        );
        assert_eq!(normalize("{:?}{}"), "{}{}");
        assert_eq!(normalize("no placeholder"), "no placeholder");
    }
}
