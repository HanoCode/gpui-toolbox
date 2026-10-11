//! 工具帮助内容（对应 TS `ToolMeta.helpContent` 的元素）。

/// 帮助里的一节。
///
/// `title` 与 `paragraphs` 都是中文源串，渲染时经 `tr`。
#[derive(Debug, Clone, Copy)]
pub struct HelpSection {
    title: &'static str,
    paragraphs: &'static [&'static str],
}

impl HelpSection {
    /// 构造一节帮助。
    #[must_use]
    pub const fn new(title: &'static str, paragraphs: &'static [&'static str]) -> Self {
        Self { title, paragraphs }
    }

    /// 小节标题。
    #[must_use]
    pub const fn title(&self) -> &'static str {
        self.title
    }

    /// 小节正文段落。
    #[must_use]
    pub const fn paragraphs(&self) -> &'static [&'static str] {
        self.paragraphs
    }
}
