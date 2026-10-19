//! 工具元数据：注册表的注册单元（对应 TS `ToolMeta`）。
//!
//! 三处相对 TS 接口的收敛：
//! 1. `icon: string` → [`DuckIcon`]（写错图标名编译失败）；
//! 2. `category: string` → [`ToolCategory`]（写错分类编译失败）；
//! 3. `pinyin` / `pinyinInitials` → **注册时自动派生**（见 [`crate::pinyin`]），不再是手填字段。
//!
//! 另外补了 TS 接口没有、但工具箱必须有的两项：`run`（执行入口）与 `settings`（工具级设置 schema）。

use duck_components::DuckIcon;
use gpui::{App, Hsla, Window};

use crate::category::ToolCategory;
use crate::help::HelpSection;
use crate::settings::ToolSettingSpec;

/// 工具的执行入口。
///
/// 入参是**工具自身的 id**，因此一个处理器可以服务多个工具（例如统一的「后端未接入」提示），
/// 不必为每个工具各写一个函数。
///
/// `None` 表示「只有注册信息、后端未接入」——界面照常展示，点击时提示未接入。
pub type ToolRun = fn(tool_id: &str, window: &mut Window, cx: &mut App);

/// 工具元数据。
pub struct ToolMeta {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    icon: DuckIcon,
    icon_color: Option<fn(&App) -> Hsla>,
    keywords: &'static [&'static str],
    category: ToolCategory,
    group: Option<&'static str>,
    beta: bool,
    dev: bool,
    help: &'static [HelpSection],
    settings: &'static [ToolSettingSpec],
    run: Option<ToolRun>,
}

impl ToolMeta {
    /// 开始构造：只有 `id` / `name` / `icon` 是必填。
    ///
    /// `name` 与 `description` 都是**中文源串**，渲染时经 `duck_theme::tr` 取词。
    #[must_use]
    pub fn builder(id: &'static str, name: &'static str, icon: DuckIcon) -> ToolMetaBuilder {
        ToolMetaBuilder {
            meta: Self {
                id,
                name,
                description: "",
                icon,
                icon_color: None,
                keywords: &[],
                category: ToolCategory::Other,
                group: None,
                beta: false,
                dev: false,
                help: &[],
                settings: &[],
                run: None,
            },
        }
    }

    /// 稳定 id，例 `"video.parse"`。
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// 展示名（中文源串）。
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// 一句话说明（中文源串）。
    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// 图标。
    #[must_use]
    pub const fn icon(&self) -> DuckIcon {
        self.icon
    }

    /// 图标颜色工厂（`None` 用统一的弱化前景色）。
    #[must_use]
    pub const fn icon_color(&self) -> Option<fn(&App) -> Hsla> {
        self.icon_color
    }

    /// 附加搜索词（中英）。
    #[must_use]
    pub const fn keywords(&self) -> &'static [&'static str] {
        self.keywords
    }

    /// 分类。
    #[must_use]
    pub const fn category(&self) -> ToolCategory {
        self.category
    }

    /// 二级分组（可选，用于同分类内再分节）。
    #[must_use]
    pub const fn group(&self) -> Option<&'static str> {
        self.group
    }

    /// 是否为试验功能（界面显示 BETA 徽标）。
    #[must_use]
    pub const fn is_beta(&self) -> bool {
        self.beta
    }

    /// 是否为开发者工具（仅在开发者模式下可见）。
    #[must_use]
    pub const fn is_dev(&self) -> bool {
        self.dev
    }

    /// 帮助内容。
    #[must_use]
    pub const fn help(&self) -> &'static [HelpSection] {
        self.help
    }

    /// 工具级设置 schema。
    #[must_use]
    pub const fn settings(&self) -> &'static [ToolSettingSpec] {
        self.settings
    }

    /// 执行入口；`None` 表示后端未接入。
    #[must_use]
    pub const fn run(&self) -> Option<ToolRun> {
        self.run
    }
}

/// [`ToolMeta`] 的构造器。
pub struct ToolMetaBuilder {
    meta: ToolMeta,
}

impl ToolMetaBuilder {
    /// 一句话说明。
    #[must_use]
    pub fn description(mut self, description: &'static str) -> Self {
        self.meta.description = description;
        self
    }

    /// 附加搜索词。
    #[must_use]
    pub fn keywords(mut self, keywords: &'static [&'static str]) -> Self {
        self.meta.keywords = keywords;
        self
    }

    /// 分类。
    #[must_use]
    pub fn category(mut self, category: ToolCategory) -> Self {
        self.meta.category = category;
        self
    }

    /// 二级分组；`None` = 不分组。
    #[must_use]
    pub fn group(mut self, group: Option<&'static str>) -> Self {
        self.meta.group = group;
        self
    }

    /// 标记为试验功能。
    #[must_use]
    pub fn beta(mut self) -> Self {
        self.meta.beta = true;
        self
    }

    /// 标记为开发者工具。
    #[must_use]
    pub fn dev(mut self) -> Self {
        self.meta.dev = true;
        self
    }

    /// 帮助内容。
    #[must_use]
    pub fn help(mut self, help: &'static [HelpSection]) -> Self {
        self.meta.help = help;
        self
    }

    /// 工具级设置 schema。
    #[must_use]
    pub fn settings(mut self, settings: &'static [ToolSettingSpec]) -> Self {
        self.meta.settings = settings;
        self
    }

    /// 图标颜色，随主题解析。
    #[must_use]
    pub fn icon_color(mut self, color: fn(&App) -> Hsla) -> Self {
        self.meta.icon_color = Some(color);
        self
    }

    /// 执行入口。
    #[must_use]
    pub fn run(mut self, run: ToolRun) -> Self {
        self.meta.run = Some(run);
        self
    }

    /// 完成构造。
    #[must_use]
    pub fn build(self) -> ToolMeta {
        self.meta
    }
}

#[cfg(test)]
mod tests {
    use super::ToolMeta;
    use crate::category::ToolCategory;
    use duck_components::DuckIcon;

    #[test]
    fn builder_fills_defaults() {
        let meta = ToolMeta::builder("demo.tool", "演示工具", DuckIcon::Box).build();
        assert_eq!(meta.id(), "demo.tool");
        assert_eq!(meta.name(), "演示工具");
        assert_eq!(meta.description(), "");
        assert_eq!(meta.category(), ToolCategory::Other);
        assert!(meta.group().is_none());
        assert!(!meta.is_beta());
        assert!(!meta.is_dev());
        assert!(meta.help().is_empty());
        assert!(meta.settings().is_empty());
        assert!(meta.run().is_none());
    }

    #[test]
    fn builder_sets_optional_fields() {
        let meta = ToolMeta::builder("demo.tool", "演示工具", DuckIcon::Code)
            .description("一句话说明")
            .keywords(&["demo", "演示"])
            .category(ToolCategory::Development)
            .group(Some("调试"))
            .beta()
            .dev()
            .build();
        assert_eq!(meta.description(), "一句话说明");
        assert_eq!(meta.keywords(), &["demo", "演示"]);
        assert_eq!(meta.category(), ToolCategory::Development);
        assert_eq!(meta.group(), Some("调试"));
        assert!(meta.is_beta());
        assert!(meta.is_dev());
    }
}
