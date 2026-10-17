//! 设置**项**级搜索索引。
//!
//! 侧栏那个搜索框原先只匹配分类的标题与描述：搜「行高」「编码」「回滚」一律搜不到，
//! 因为它们在某一页的正文里，而那一页的标题叫「终端」、描述叫「字号、回滚与会话行为」——
//! 用户得先猜「这条设置归哪一页」才搜得动。这一页索引把「项」抬成搜索的一等公民。
//!
//! # 为什么是一张手写表
//!
//! 试过的另一条路是从渲染出来的行里抽文案（GPUI 没有反射，抽不到），或者给每个行构造函数
//! 都塞一份关键词（22 个构造点 × 每次改文案都要同步）。写成一张表的好处是：
//!
//! - **别名可以任意写**：用户搜「字体大小」要能命中「终端字号」——这种同义词只有人能想到；
//! - 中英双语**免费**：`label` 是界面上的中文源串，英译从 `en.toml` 取；`keywords` 里塞
//!   几个英文词，中文界面下搜 `font` 也能中。
//!
//! 代价是表会陈旧（改行名忘了改表）。所以**只索引稳定的大件**：字号 / 行高 / 回填 / 重连 /
//! 上限这类天天被找的；不为每一个开关都建一条。表里指错页的代价是「搜到但跳过去没那一行」，
//! 比「搜不到」轻得多。

use super::*;

/// 一条可被搜到的设置项。
pub(crate) struct SettingItem {
    /// 所属分类 key（[`PAGES`] 里的那个）。
    pub(super) page: &'static str,
    /// 行标题：与正文里那一行**同名**，同时也是 i18n 键。
    pub(super) label: &'static str,
    /// 别名。用户不会记得我们的用词（「行高」vs「行距」vs「line height」）。
    pub(super) keywords: &'static [&'static str],
}

/// 索引表。按分类分组排列，方便对照正文。
pub(super) const ITEMS: &[SettingItem] = &[
    // ── 外观 ──
    SettingItem {
        page: "appearance",
        label: "主题模式",
        keywords: &["深色", "浅色", "暗色", "dark", "light", "theme", "mode"],
    },
    SettingItem {
        page: "appearance",
        label: "主题",
        keywords: &["配色", "皮肤", "theme", "color", "scheme"],
    },
    SettingItem {
        page: "appearance",
        label: "主题色",
        keywords: &["强调色", "accent", "color"],
    },
    SettingItem {
        page: "appearance",
        label: "语言",
        keywords: &["中文", "英文", "language", "locale", "i18n"],
    },
    SettingItem {
        page: "appearance",
        label: "界面缩放",
        keywords: &["放大", "缩小", "zoom", "scale", "dpi"],
    },
    SettingItem {
        page: "appearance",
        label: "界面密度",
        keywords: &["紧凑", "宽松", "density", "compact"],
    },
    SettingItem {
        page: "appearance",
        label: "字体大小",
        keywords: &["字号", "font", "size"],
    },
    SettingItem {
        page: "appearance",
        label: "字体",
        keywords: &["字体族", "font", "family"],
    },
    // ── 行为 ──
    SettingItem {
        page: "behavior",
        label: "开机自动启动",
        keywords: &["自启", "登录项", "autostart", "login"],
    },
    SettingItem {
        page: "behavior",
        label: "卸载插件前弹确认",
        keywords: &["确认", "confirm", "uninstall"],
    },
    // ── 通知 ──
    SettingItem {
        page: "notify",
        label: "应用内横幅",
        keywords: &["提示", "notice", "in-app"],
    },
    SettingItem {
        page: "notify",
        label: "系统通知",
        keywords: &["横幅", "system", "notification"],
    },
    // ── AI ──
    SettingItem {
        page: AI_PAGE,
        label: "模型厂商",
        keywords: &["接口", "provider", "model", "api"],
    },
    SettingItem {
        page: AI_PAGE,
        label: "API key",
        keywords: &["key", "token", "api key", "凭据"],
    },
    // ── 终端 ──
    SettingItem {
        page: TERMINAL_PAGE,
        label: "终端字号",
        keywords: &["字号", "字体大小", "font", "size", "zoom"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "字体族",
        keywords: &["字体", "font", "family", "mono"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "回退字体",
        keywords: &["字体", "fallback", "cjk", "中文", "图标"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "行高",
        keywords: &["行距", "line height", "spacing"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "内边距",
        keywords: &["留白", "边距", "padding", "margin"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "回滚行数",
        keywords: &["历史", "缓冲", "scrollback", "history", "buffer"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "断线自动重连",
        keywords: &["重试", "reconnect", "retry"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "重连上限",
        keywords: &["次数", "reconnect", "attempts"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "起始退避",
        keywords: &["重连", "等待", "backoff", "delay"],
    },
    SettingItem {
        page: TERMINAL_PAGE,
        label: "粘贴确认",
        keywords: &["剪贴板", "多行", "paste", "confirm", "clipboard"],
    },
    // ── 数据库 ──
    SettingItem {
        page: DATABASE_PAGE,
        label: "结果行数上限",
        keywords: &["条数", "limit", "rows", "cap"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "数据浏览每页行数",
        keywords: &["分页", "page", "rows"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "查询历史上限",
        keywords: &["历史", "history"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "默认只读",
        keywords: &["只读", "read only", "安全"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "默认超时",
        keywords: &["超时", "timeout", "秒"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "关键字大小写",
        keywords: &["格式化", "大小写", "sql", "format", "case"],
    },
    SettingItem {
        page: DATABASE_PAGE,
        label: "自动换行",
        keywords: &["折行", "wrap", "sql"],
    },
    // ── 邮箱 ──
    SettingItem {
        page: MAIL_PAGE,
        label: "新邮件通知",
        keywords: &["来信", "提醒", "mail", "notify"],
    },
    SettingItem {
        page: MAIL_PAGE,
        label: "默认正文形态",
        keywords: &["纯文本", "html", "mail", "body"],
    },
    SettingItem {
        page: MAIL_PAGE,
        label: "列表首屏条数",
        keywords: &["条数", "列表", "mail", "limit"],
    },
    // ── Redis ──
    SettingItem {
        page: REDIS_PAGE,
        label: "扫描页大小",
        keywords: &["键", "keys", "scan", "limit"],
    },
    // ── 项目管理 ──
    SettingItem {
        page: PROJECTS_PAGE,
        label: "到期与逾期提醒",
        keywords: &["截止", "提醒", "due", "remind"],
    },
    SettingItem {
        page: PROJECTS_PAGE,
        label: "久未更新提醒",
        keywords: &["陈旧", "stale", "days"],
    },
    // ── 日志 ──
    SettingItem {
        page: LOGS_PAGE,
        label: "保留时长",
        keywords: &["清理", "retention", "log", "days"],
    },
    // ── 数据 ──
    SettingItem {
        page: DATA_PAGE,
        label: "导出备份",
        keywords: &["导出", "backup", "export"],
    },
    SettingItem {
        page: DATA_PAGE,
        label: "从备份文件导入",
        keywords: &["导入", "restore", "import"],
    },
];

/// 按关键词找设置项。
///
/// 匹配面比界面宽：`label` 的中文源串、它的英译、以及每个别名的中英两种写法都算命中。
/// 这样中文界面下搜 `font`、英文界面下搜「行高」都能中——搜设置的人往往记不清
/// 当前界面是哪种语言（尤其是刚切完语言想找回来的时候）。
pub(super) fn hits(query: &str, lang: Lang) -> Vec<&'static SettingItem> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    ITEMS
        .iter()
        .filter(|item| {
            // 三份都要：**源串**（中文界面下的显示名）、当前语言的译名、英译。
            // 少掉源串那一份，英文界面下搜「行高」就一条都中不了 ——
            // 而那正是「刚切完语言想找回来」的人唯一记得的写法。
            let haystacks = [item.label, t(lang, item.label), t(Lang::En, item.label)]
                .into_iter()
                .chain(
                    item.keywords
                        .iter()
                        .copied()
                        .flat_map(|keyword| [keyword, t(lang, keyword), t(Lang::En, keyword)]),
                );
            haystacks.into_iter().any(|text| {
                let text = text.to_lowercase();
                text.contains(&query)
            })
        })
        .collect()
}

/// 某个分类的标题 / 描述是否命中（页级匹配，与项级并行）。
pub(super) fn page_matches(page: &SettingsPageSpec, query: &str, lang: Lang) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    [page.title, page.description]
        .into_iter()
        // 中英两种写法都算命中：切完语言想找回来的人，通常还记着上一种写法。
        .flat_map(|text| [text, t(lang, text), t(Lang::En, text)])
        .any(|text| text.to_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 索引里的每一条都必须指向**真实存在**的分类：写错 key 的话，命中了也跳不过去，
    /// 而侧栏里那一行还会留在那儿——比搜不到更难查。
    #[test]
    fn every_item_points_at_a_real_page() {
        for item in ITEMS {
            assert!(
                PAGES.iter().any(|page| page.key == item.page),
                "「{}」指向了不存在的分类 {}",
                item.label,
                item.page
            );
        }
    }

    /// 用户真会这么搜：中文词、英文词、大小写混着。
    #[test]
    fn known_words_find_their_row() {
        let find = |query: &str| hits(query, Lang::Zh);
        assert_eq!(find("行高")[0].label, "行高");
        assert!(find("行距")[0].label == "行高", "别名要能中");
        assert!(find("LINE HEIGHT")[0].label == "行高", "英文别名不分大小写");
        assert!(find("scrollback")[0].label == "回滚行数");
        assert!(find("字体大小").iter().any(|item| item.label == "终端字号"));
    }

    /// 英文界面下搜中文也要中（刚切完语言、还没适应的人最需要这个）。
    #[test]
    fn english_ui_still_answers_chinese_queries() {
        let found = hits("行高", Lang::En);
        assert_eq!(found[0].label, "行高");
        // 反方向：中文界面搜英译名。
        assert!(!hits("line height", Lang::Zh).is_empty());
    }

    /// 空查询不返回任何项：侧栏不该在没搜索时冒出一坨「命中」。
    #[test]
    fn blank_query_matches_nothing() {
        assert!(hits("   ", Lang::Zh).is_empty());
    }
}
