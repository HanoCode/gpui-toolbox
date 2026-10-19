//! PowerDesigner PDM 读取（只读部分，逐字复刻参考仓 `pdm/`）。
//!
//! 覆盖 `pdm/mod.rs` 的 `pdm_read_file` / `pdm_scan_directory` /
//! `pdm_parse_directory` 与 `pdm_generate_sql` / `pdm_export_*` 这几个命令；
//! 要数据库连接的 `pdm_generate_from_db` 不迁（见 crate 文档）。
//!
//! 与参考仓的一处行为改进：目录解析时跳过的坏文件**回报给调用方**
//! （源仓只 `log::warn!` 一句，用户看不到「12 个文件里坏了 3 个」）。

pub mod export;
pub mod labels;
pub mod models;
pub mod parser;
pub mod sql;

use std::path::{Path, PathBuf};

pub use labels::Labels;
pub use models::{Column, Index, PdmDirectoryResult, PdmFileResult, PdmModel, Table};

/// 读取 / 解析错误：稳定码 + 细节（整句由插件层拼）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdmError {
    /// 文件读不了（不存在 / 没权限 / 不是 UTF-8）。
    Read(String),
    /// XML 结构坏了（quick-xml 的原话）。
    Xml(String),
    /// 模型里没有这张表。
    TableNotFound(String),
}

impl PdmError {
    /// 稳定码：插件层据它选译文。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Read(_) => "read-failed",
            Self::Xml(_) => "xml-invalid",
            Self::TableNotFound(_) => "table-not-found",
        }
    }

    /// 细节（路径 / XML 报错原话 / 表名）：是数据，不翻译。
    pub fn detail(&self) -> &str {
        match self {
            Self::Read(detail) | Self::Xml(detail) | Self::TableNotFound(detail) => detail,
        }
    }
}

/// 读一个 PDM / XML 文件。
pub fn read_file(path: &Path) -> Result<PdmModel, PdmError> {
    let xml = std::fs::read_to_string(path)
        .map_err(|error| PdmError::Read(format!("{}: {error}", path.display())))?;
    parser::PdmParser::parse(&xml).map_err(PdmError::Xml)
}

/// 递归找目录下的 `.pdm` / `.xml`（参考仓同名同序：按路径排序）。
pub fn scan_directory(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".pdm") || name.ends_with(".xml") {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    files
}

/// 目录解析结果：合并模型 + 每个文件各自的模型（参考仓 `PdmDirectoryResult`
/// 的形状），外加源仓只写进日志的**跳过清单**。
pub struct DirectoryScan {
    pub files: Vec<PdmFileResult>,
    pub combined: PdmModel,
    pub skipped: Vec<(String, PdmError)>,
}

/// 解析整个目录：能读的进 `files`，同时把表并进 `combined`。
///
/// `combined_name` 是合并模型的显示名（参考仓写死「合并数据字典」，
/// 内核不出中文，由插件层按当前语言给）。
pub fn parse_directory(path: &Path, combined_name: &str) -> DirectoryScan {
    let paths = scan_directory(path);
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut combined = PdmModel {
        name: combined_name.to_string(),
        code: "combined".to_string(),
        description: String::new(),
        target: String::new(),
        tables: Vec::new(),
    };

    for file_path in paths {
        match read_file(&file_path) {
            Ok(model) => {
                let file_name = file_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                if combined.target.is_empty() && !model.target.is_empty() {
                    combined.target = model.target.clone();
                }
                combined.tables.extend(model.tables.clone());
                files.push(PdmFileResult {
                    file_name,
                    file_path: file_path.to_string_lossy().to_string(),
                    model,
                });
            }
            Err(error) => skipped.push((file_path.to_string_lossy().to_string(), error)),
        }
    }

    DirectoryScan {
        files,
        combined,
        skipped,
    }
}

/// 按表代码找表（参考仓是 `t.code == table_code`，大小写敏感，保持一致）。
pub fn find_table<'a>(model: &'a PdmModel, code: &str) -> Option<&'a Table> {
    model.tables.iter().find(|table| table.code == code)
}

/// 「表」参数的筛选（移植参考仓前端 `PdmReader.vue::applySearch`，后端无对应实现）。
///
/// 参考页的做法是**一个搜索框 + 一个范围下拉**（表名 / 字段 / 注释 / 全部）。本仓参数区
/// 只有一个文本框，所以把范围压成「全部」，并把参考页那条**多关键词**规则照搬过来：
///
/// - 单个词：筛出**所有**命中的表（表名/代码/表注释/任一列的名字、代码、注释）；
/// - 多个词（逗号、中文逗号、换行分隔）：**每个词各挑第一张**命中的表，按词序去重。
///
/// 多词为什么不是「或」：那是「我手上有 20 个表名，只想导这几张」的用法，
/// 一个词对应一张表才有意义（参考页也是 `find` 而不是 `filter`）。
pub fn select_tables<'a>(model: &'a PdmModel, query: &str) -> Vec<&'a Table> {
    let terms: Vec<String> = query
        .split([',', '，', '\n', '\r'])
        .map(|term| term.trim().to_lowercase())
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return model.tables.iter().collect();
    }
    if terms.len() == 1 {
        let q = &terms[0];
        return model
            .tables
            .iter()
            .filter(|table| table_matches(table, q))
            .collect();
    }

    let mut picked: Vec<&Table> = Vec::new();
    for q in &terms {
        if let Some(table) = model.tables.iter().find(|table| table_matches(table, q)) {
            if !picked.iter().any(|seen| seen.code == table.code) {
                picked.push(table);
            }
        }
    }
    picked
}

/// 一个关键词是否命中这张表（参考页 `scope = all` 的四个判定）。
fn table_matches(table: &Table, q: &str) -> bool {
    if table.name.to_lowercase().contains(q)
        || table.code.to_lowercase().contains(q)
        || table.comment.to_lowercase().contains(q)
    {
        return true;
    }
    table.columns.iter().any(|col| {
        col.name.to_lowercase().contains(q)
            || col.code.to_lowercase().contains(q)
            || col.comment.to_lowercase().contains(q)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<?pdmodels CreationDate="20240101000000" Target="Oracle 11g"?>
<o:RootObject>
  <o:Model>
    <a:Name>存款核心</a:Name>
    <a:Code>deposit</a:Code>
    <c:Tables>
      <o:Table>
        <a:Name>账户表</a:Name>
        <a:Code>T_ACCOUNT</a:Code>
        <a:Comment>账户主表</a:Comment>
        <c:Columns>
          <o:Column Id="o1">
            <a:Name>账号</a:Name>
            <a:Code>ACC_NO</a:Code>
            <a:DataType>VARCHAR2(32)</a:DataType>
            <a:Length>32</a:Length>
            <a:Mandatory>1</a:Mandatory>
          </o:Column>
          <o:Column Id="o2">
            <a:Name>余额</a:Name>
            <a:Code>BALANCE</a:Code>
            <a:DataType>NUMBER(18,2)</a:DataType>
            <a:Precision>2</a:Precision>
            <a:DefaultValue>0</a:DefaultValue>
            <a:Comment>账户余额</a:Comment>
          </o:Column>
        </c:Columns>
        <c:Keys>
          <o:Key Id="k1">
            <a:Name>PK_ACCOUNT</a:Name>
            <a:Code>PK_ACCOUNT</a:Code>
            <c:Key.Columns>
              <o:Column Ref="o1"/>
            </c:Key.Columns>
          </o:Key>
        </c:Keys>
        <c:PrimaryKey>
          <o:Key Ref="k1"/>
        </c:PrimaryKey>
        <c:Indexes>
          <o:Index>
            <a:Name>IDX_BAL</a:Name>
            <a:Code>IDX_BAL</a:Code>
            <c:IndexColumns>
              <o:IndexColumn>
                <c:Column>
                  <o:Column Ref="o2"/>
                </c:Column>
              </o:IndexColumn>
            </c:IndexColumns>
          </o:Index>
        </c:Indexes>
      </o:Table>
    </c:Tables>
  </o:Model>
</o:RootObject>
"##;

    fn sample() -> PdmModel {
        parser::PdmParser::parse(SAMPLE).expect("样例应当能解析")
    }

    #[test]
    fn reads_model_tables_columns_keys_and_indexes() {
        let model = sample();
        assert_eq!(model.name, "存款核心");
        assert_eq!(model.code, "deposit");
        assert_eq!(model.target, "Oracle 11g");
        assert_eq!(model.tables.len(), 1);

        let table = &model.tables[0];
        assert_eq!(table.code, "T_ACCOUNT");
        assert_eq!(table.comment, "账户主表");
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[1].precision, Some(2));
        assert_eq!(table.columns[1].default_value, "0");
        assert!(table.columns[0].mandatory);
        // `Ref` 回指要落成列代码，而不是 Id。
        assert_eq!(table.primary_key, vec!["ACC_NO"]);
        assert_eq!(table.indexes.len(), 1);
        assert_eq!(table.indexes[0].columns, vec!["BALANCE"]);
    }

    /// 截断的 XML **不报错**：quick-xml 见到 EOF 就收手，参考仓也是这个行为
    /// （已经读到的元素照样返回）。这条钉住「不要为了报错去加检查」——
    /// 真实 PDM 里带 BOM / 尾部垃圾的样本比「截断」多得多。
    #[test]
    fn truncated_xml_keeps_what_it_read() {
        let model = parser::PdmParser::parse("<o:RootObject><o:Model>").unwrap();
        assert!(model.tables.is_empty());
    }

    #[test]
    fn find_table_is_exact_match() {
        let model = sample();
        assert!(find_table(&model, "T_ACCOUNT").is_some());
        assert!(find_table(&model, "t_account").is_none());
    }

    /// 插入骨架：列名摆全、值留空、标识符恒用反引号（源仓前端的写法，与方言无关）。
    #[test]
    fn insert_skeleton_lists_every_column_and_leaves_the_values_open() {
        let model = sample();
        let labels = Labels {
            title: String::new(),
            generated_at: String::new(),
            tables: String::new(),
            target: String::new(),
            target_database: String::new(),
            field: String::new(),
            code: String::new(),
            data_type: String::new(),
            length: String::new(),
            precision: String::new(),
            mandatory: String::new(),
            default_value: String::new(),
            comment: String::new(),
            primary_key: String::new(),
            index: String::new(),
            columns: String::new(),
            footer: String::new(),
            table: "表".into(),
            todo_values: "TODO: 添加值".into(),
        };
        let sql = sql::generate_insert_sql(&model.tables, &labels);
        assert_eq!(
            sql,
            "-- 表: 账户表 (T_ACCOUNT)\n\
             INSERT INTO `T_ACCOUNT` (`ACC_NO`, `BALANCE`) VALUES (\n  \
             -- TODO: 添加值\n;\n",
            "{sql}"
        );
        // 只给一张表时，末尾那个空行也在（`join` 的产物，别当尾随空白删掉）。
        assert!(sql.ends_with(";\n"), "{sql}");
    }

    /// 单关键词：表名 / 代码 / 表注释 / 列名 / 列代码 / 列注释，六个口子都算命中。
    #[test]
    fn one_keyword_matches_every_field_of_a_table() {
        let model = sample();
        for query in [
            "账户",
            "t_account",
            "账户主表",
            "账号",
            "acc_no",
            "账户余额",
        ] {
            let picked = select_tables(&model, query);
            assert_eq!(picked.len(), 1, "{query} 应当命中 1 张表");
            assert_eq!(picked[0].code, "T_ACCOUNT");
        }
        // 大小写不敏感（参考页先 `toLowerCase()`）。
        assert_eq!(select_tables(&model, "T_ACCOUNT").len(), 1);
        assert!(select_tables(&model, "不存在的表").is_empty());
    }

    /// 多关键词：**一个词挑一张表**，不是「或」；重复词与同表命中都要去重。
    #[test]
    fn many_keywords_pick_one_table_each_and_dedupe() {
        let mut model = sample();
        let mut log = model.tables[0].clone();
        log.code = "T_LOG".to_string();
        log.name = "日志表".to_string();
        log.comment = String::new();
        model.tables.push(log);

        // 逗号 / 中文逗号 / 换行都是分隔符，两个词 ⇒ 两张表，顺序跟词序。
        for query in ["账户, 日志", "账户，日志", "账户\n日志"] {
            let picked = select_tables(&model, query);
            let codes: Vec<&str> = picked.iter().map(|t| t.code.as_str()).collect();
            assert_eq!(codes, ["T_ACCOUNT", "T_LOG"], "{query}");
        }
        // 两个词指同一张表（`账号` 是列名、`T_ACCOUNT` 是表代码）⇒ 只留一份。
        assert_eq!(select_tables(&model, "账号, T_ACCOUNT").len(), 1);
        // 单关键词是「筛」而不是「挑第一张」：两张表都有 `ACC_NO` 列。
        assert_eq!(select_tables(&model, "acc_no").len(), 2);
        // 空串 = 不筛，全给。
        assert_eq!(select_tables(&model, "  ").len(), model.tables.len());
    }
}
