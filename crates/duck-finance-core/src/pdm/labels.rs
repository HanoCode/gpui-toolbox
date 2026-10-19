//! 数据字典导出的**表头词**（中文源串由插件层按当前语言给）。
//!
//! 参考仓把这些词直接写在 `export.rs` / `sql_gen.rs` 里；本仓内核不出中文，所以
//! 抽成一份「词表」当参数传进来 —— 与 nginx 族的 `Comments` 同一个做法。
//! 词表只覆盖**表头与固定短语**，表名 / 字段名 / 注释这些数据照原样进产物。
//!
//! 用 `String` 而不是 `&str`：调用方是「按当前语言取词」的函数（返回 `String`），
//! 借字符串会让插件层为了凑生命周期绕一圈，而这里的量级是十几个短句。

pub struct Labels {
    /// 「数据字典」（标题后缀）。
    pub title: String,
    /// 「生成时间」。
    pub generated_at: String,
    /// 「表数量」。
    pub tables: String,
    /// 「目标」（PDM 的 DBMS 名，如 Oracle 11g）。
    pub target: String,
    /// 「目标数据库」（Markdown / Word / PDF 版用词）。
    pub target_database: String,
    /// 「字段名」。
    pub field: String,
    /// 「代码」（Excel / Word 版多一列代码）。
    pub code: String,
    /// 「类型」。
    pub data_type: String,
    /// 「长度」。
    pub length: String,
    /// 「精度」。
    pub precision: String,
    /// 「非空」。
    pub mandatory: String,
    /// 「默认值」。
    pub default_value: String,
    /// 「注释」。
    pub comment: String,
    /// 「主键」。
    pub primary_key: String,
    /// 「索引」。
    pub index: String,
    /// 「字段数」。
    pub columns: String,
    /// 「由 PDM 阅读器生成」。
    pub footer: String,
    /// 「表」（单数）：插入语句上方那行 `-- 表: 名称 (代码)` 的用词。
    ///
    /// 与上面 [`Self::tables`]（「表数量」，是列头）分开：一个是名词、一个是表头，
    /// 中英都不是同一个词（`表` / `Tables`），合成一个字段会逼出一句不通的译文。
    pub table: String,
    /// 「TODO: 添加值」：插入语句骨架里留给用户填值的那行注释。
    pub todo_values: String,
}
