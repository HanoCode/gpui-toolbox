//! 文件批处理族内核（`duck.files.*`，计划 §6.6）。
//!
//! 五个工具、一份内核：
//!
//! | 工具 | 模块 | 参考仓来源 |
//! | --- | --- | --- |
//! | 文件搜索 | [`search`] | `fast_file_search` 的匹配口径 + `file_ops::list_files_recursive` |
//! | 目录树 | [`tree`] | `file_ops::read_directory_tree` + `DirectoryTree.vue` 的渲染 |
//! | 批量改写 | [`batch`] | `batch_replace.rs` 逐字复刻 + `BatchRenamer.vue` 的改名规则 |
//! | CSV 工具 | [`csv_tools`] | `CsvTableEditor.vue` / `CsvToChart.vue` 的前端算法 |
//! | 格式转换 | [`convert`] | `converter/`（`office` / `data` / `transform`） |
//!
//! 纪律与比对族一致：**零 gpui、零 tauri、全同步**。所有写盘动作都返回结构化结果
//! （不做「失败即 panic」），错误走 [`FilesError`]（稳定码 + 细节），由插件层翻译。
//!
//! 四处对参考仓的**主动偏离**，都在各自模块头里写清了原因：
//!
//! 1. 错误改为「稳定码 + 细节」（i18n 需要抓手，参考仓直接拼中文字符串）；
//! 2. `fast_file_search` 的常驻索引 / 文件监听不迁（插件进程一次调用一次进程，
//!    没有「下一次运行」可复用索引；设计稿已裁成「限定目录 + 深度上限」的遍历）；
//! 3. 批量改写改成**撤销日志**（参考仓只写 `.bak`，无法一键回滚整批，且撤销跨运行可用）；
//! 4. 格式转换不出 PDF（参考仓的 `printpdf` + 内置 Helvetica 没有中文字形，
//!    导出的中文 PDF 是空白）；`kreuzberg` 因 MSRV 1.91 不可用，docx / pptx / xlsx
//!    改用 `zip` + `quick-xml` + `calamine` 抽取，正文口径与参考仓一致。

pub mod batch;
pub mod convert;
pub mod csv_tools;
pub mod data_format;
pub mod datagen;
pub mod datagen_tables;
pub mod error;
pub mod search;
pub mod tree;
pub mod walk;

#[cfg(test)]
mod testkit;

pub use batch::{
    ApplyOutcome, BatchMode, MAX_UNDO, RenamePlanRow, RenameRule, ReplacePreview, ReplaceResult,
    ReplaceRule, UndoEntryInfo, UndoReport, apply_rename, apply_replace, list_undo, plan_rename,
    preview_replace, undo_last,
};
pub use convert::{Format, OutFormat, convert_file, convert_text, detect_format};
pub use csv_tools::{
    ChartOptions, ChartPalette, ChartType, CsvTable, head, parse_table, render_summary,
    to_svg_chart,
};
pub use data_format::{
    Column, Grid, Output as DataOutput, delimiter_from_value, parse_columns, parse_delimited,
    parse_fixed, parse_names, parse_with_delimiter, to_csv,
};
pub use datagen::{
    AccountStatus, AddressCountry, AddressOptions, Customer, CustomerOptions, DateTimeKind, Gender,
    HolderType, Institution, InternetKind, Lang, NameOptions, NumberKind, Person, Relation, Rng,
    Shareholding, addresses, company, customer, datetime, en_name, id_card, internet, ja_name,
    names, numbers, zh_address, zh_name,
};
pub use error::{ErrorKind, FilesError, FilesResult, require_dir, require_file, validate_path};
pub use search::{SearchHit, SearchMode, SearchOptions, SearchOutcome, render_report, search};
pub use tree::{TreeOptions, TreeStats, human_size, render_markdown, render_text};
pub use walk::{
    WalkEntry, WalkOptions, WalkOutcome, any_match, collect_files, glob_match, slash, walk,
};
