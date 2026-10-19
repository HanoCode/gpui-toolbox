//! SQL 分类抽取：把一批脚本拆成「建表 / 改表 / 数据」三类，并生成备份与回滚。
//!
//! **移植来源**：参考仓 `ifm_ai_tools/src/services/sqlClassifier.ts`（389 行）。
//! 那份代码**没有 Rust 后端**（纯前端算法），按移植规矩允许搬前端逻辑；这里是
//! 逐函数翻译，不是重新设计——正则、扫描顺序、去重与补 `;` 的细节全部保留。
//!
//! 与源实现的唯一**增量**是 [`ClassifyResult::unclassified`]：源页面把「一条规则
//! 都没命中的语句」直接丢掉、界面上看不出来；设计稿明确要求「无法归类的块不能
//! 静默丢」，所以这里只**多数一个数**（不改输出内容），由工具层报出来。
//!
//! 前端那一半（`list_directory` / `read_text_file` / `write_text_file`）不在本文件：
//! 落盘编排在 `crates/duck-sql-tool`，本文件只处理文本。

use regex::Regex;
use std::sync::OnceLock;

/// 一条语句的归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// 建表 / 建序列 / 注释（`table_create_ddl.sql`）。
    Create,
    /// 改表 / 索引 / 删表 / 重命名（`table_column_ddl.sql`）。
    Column,
    /// 数据变更（`data_init_dml.sql`）。
    Dml,
}

/// 一个文件（或一批文件累加）的分类结果；每个元素都是一整条语句（含结尾 `;`）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClassifyResult {
    /// 建表 / 建序列 / `comment on`。
    pub create: Vec<String>,
    /// 改表 / 索引 / 删表 / 重命名。
    pub column: Vec<String>,
    /// `insert` / `update` / `delete` / `merge`。
    pub dml: Vec<String>,
    /// 每张表一行 `create table …_bak_日期 as select * from …`（按表名排序）。
    pub backup_lines: Vec<String>,
    /// DDL 回滚（带上源脚本里的 `-- add by … beg/end` 标记）。
    pub rollback_ddl: Vec<String>,
    /// 数据回滚（`delete` + 从备份表 `insert`）。
    pub rollback_data: Vec<String>,
    /// 三类规则都没命中的语句条数（源实现静默丢弃，这里只计数）。
    pub unclassified: usize,
}

/// 一段被 `-- add by … beg` / `-- add by … end` 包起来的块。
struct TaggedBlock {
    /// 起始标记行（原样写回输出）。
    beg_line: String,
    /// 结束标记行。
    end_line: String,
    /// 标记之间的行。
    lines: Vec<String>,
    /// 起始行在文件里的下标。
    start_idx: usize,
    /// 结束行在文件里的下标。
    end_idx: usize,
}

/// 分类累加器；`tables` 跨语句共享，所以单独借用。
#[derive(Default)]
struct Acc {
    create: Vec<String>,
    column: Vec<String>,
    dml: Vec<String>,
    ddl_rollback: Vec<String>,
    data_rollback: Vec<String>,
}

// ── 正则（源实现的常量表，逐条对应）────────────────────────────────────

fn re_create_patterns() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?i)^\s*create\s+table\s",
            r"(?i)^\s*create\s+sequence\s",
            r"(?i)^\s*comment\s+on\s",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid create regex"))
        .collect()
    })
}

fn re_column_patterns() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?i)^\s*alter\s+",
            r"(?i)^\s*create\s+(unique\s+)?index\s",
            r"(?i)^\s*drop\s+index\s",
            r"(?i)^\s*drop\s+table\s",
            r"(?i)^\s*rename\s",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid column regex"))
        .collect()
    })
}

fn re_dml_patterns() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?i)^\s*insert\s+into\s",
            r"(?i)^\s*update\s",
            r"(?i)^\s*delete\s+from\s",
            r"(?i)^\s*merge\s+into\s",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid dml regex"))
        .collect()
    })
}

fn re_table_ref_patterns() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?i)^\s*alter\s+table\s+(\S+)",
            r"(?i)^\s*drop\s+table\s+(?:if\s+exists\s+)?(\S+)",
            r"(?i)^\s*insert\s+into\s+(\S+)",
            r"(?i)^\s*update\s+(\S+)\s+set\s+",
            r"(?i)^\s*delete\s+from\s+(\S+)",
            r"(?i)^\s*merge\s+into\s+(\S+)",
            r"(?i)^\s*rename\s+(\S+)\s+to\s+",
            r"(?i)^\s*comment\s+on\s+table\s+(\S+)",
            r"(?i)^\s*create\s+(?:unique\s+)?index\s+\S+\s+on\s+(\S+)",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid table ref regex"))
        .collect()
    })
}

fn re_comment_on_column() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*comment\s+on\s+column\s+(\S+)\.").expect("valid regex"))
}

fn re_extract_table() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:create|drop|alter)\s+table\s+(?:if\s+(?:not\s+)?exists\s+)?(\S+)")
            .expect("valid regex")
    })
}

fn re_extract_seq() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*create\s+sequence\s+(?:if\s+(?:not\s+)?exists\s+)?(\S+)")
            .expect("valid regex")
    })
}

fn re_extract_index() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*create\s+(?:unique\s+)?index\s+(\S+)").expect("valid regex")
    })
}

fn re_extract_add_col() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // 源实现是 `add\s+(\S+)`，把 `add column c1` 里的 `column` 当成了列名
    // （它的 `drop column column;` 之所以没人发现，是因为那条分支永远走不到）。
    // 这条分支现在真的会跑，所以先吃掉可选的 `column` 关键字。
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*alter\s+table\s+(\S+)\s+add\s+(?:column\s+)?(\S+)")
            .expect("valid regex")
    })
}

/// `add` 后面跟的是约束而不是列：这类语句没法机械生成回滚。
fn is_constraint_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "constraint" | "primary" | "foreign" | "unique" | "check" | "index" | "key" | "partition"
    )
}

fn re_extract_rename() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*rename\s+(\S+)\s+to\s+(\S+)").expect("valid regex"))
}

fn re_commit_only() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*COMMIT\s*;?\s*$").expect("valid regex"))
}

fn re_tag_beg() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*--\s*add\s+by\s+.*\bbeg\b").expect("valid regex"))
}

fn re_tag_end() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*--\s*add\s+by\s+.*\bend\b").expect("valid regex"))
}

// ── 表名清洗 ────────────────────────────────────────────────────────────

/// 去掉标识符两端的双引号与全部反引号（`qualified`）。
fn qualified(name: &str) -> String {
    let stripped = name.strip_prefix('"').unwrap_or(name);
    let stripped = stripped.strip_suffix('"').unwrap_or(stripped);
    stripped.replace('`', "")
}

/// 去掉 `(` 之后的一切、结尾的 `();,`，再剥引号（`cleanName`）。
fn clean_name(raw: &str) -> String {
    let without_paren = match raw.find('(') {
        Some(index) => &raw[..index],
        None => raw,
    };
    let trimmed = without_paren.trim_end_matches(['(', ')', ';', ',']).trim();
    qualified(trimmed)
}

/// 语句里出现的第一个表名（`extractTableName`）。
fn extract_table_name(stmt: &str) -> Option<String> {
    let t = stmt.trim();
    for re in re_table_ref_patterns() {
        if let Some(caps) = re.captures(t) {
            return Some(clean_name(&caps[1]));
        }
    }
    re_comment_on_column()
        .captures(t)
        .map(|caps| clean_name(&caps[1]))
}

fn extract_table(stmt: &str) -> Option<String> {
    re_extract_table()
        .captures(stmt)
        .map(|caps| clean_name(&caps[1]))
}

fn extract_seq(stmt: &str) -> Option<String> {
    re_extract_seq()
        .captures(stmt)
        .map(|caps| clean_name(&caps[1]))
}

fn extract_index(stmt: &str) -> Option<String> {
    re_extract_index()
        .captures(stmt)
        .map(|caps| clean_name(&caps[1]))
}

fn extract_add_col(stmt: &str) -> Option<(String, String)> {
    let caps = re_extract_add_col().captures(stmt)?;
    let table = clean_name(&caps[1]);
    // 源实现只切掉 `(` 之后的部分，不像 clean_name 那样再剥引号。
    let raw_col = &caps[2];
    let col = match raw_col.find('(') {
        Some(index) => &raw_col[..index],
        None => raw_col,
    };
    Some((table, clean_name(col)))
}

fn extract_rename(stmt: &str) -> Option<(String, String)> {
    let caps = re_extract_rename().captures(stmt)?;
    Some((clean_name(&caps[1]), clean_name(&caps[2])))
}

// ── 回滚生成 ────────────────────────────────────────────────────────────

fn generate_ddl_rollback(stmt: &str) -> Vec<String> {
    let t = stmt.trim();

    // ⚠️ 与源实现**唯一的有意分歧**，理由是安全而不是口味：
    // 源实现先试 `extractTable`（`create|drop|alter table …`）再试 `extractAddCol`，
    // 于是 `alter table X add column c int` 命中的是前者，回滚被写成 `drop table X;`
    // —— 把「加了一列」的回滚写成「删掉整张表」，真执行一次就是数据全丢。
    // 这里把「加列」提到最前，产出 `alter table X drop column c;`。
    // 列名拿不到时（如 `alter table X add (…)`）不猜，退回源实现的路径。
    if let Some((table, col)) = extract_add_col(t) {
        if !col.is_empty() && !is_constraint_keyword(&col) {
            return vec![format!("alter table {table} drop column {col};")];
        }
        if is_constraint_keyword(&col) {
            // `add constraint / primary key / …`：没有能机械生成的反向语句，
            // 宁可不写，也不要退化成「删表」。
            return Vec::new();
        }
    }

    if let Some(table) = extract_table(t) {
        return vec![format!("drop table {table};")];
    }
    if let Some(seq) = extract_seq(t) {
        return vec![format!("drop sequence {seq};")];
    }
    if let Some(index) = extract_index(t) {
        return vec![format!("drop index {index};")];
    }
    if let Some((from, to)) = extract_rename(t) {
        return vec![format!("rename {to} to {from};")];
    }
    Vec::new()
}

fn generate_data_rollback(stmt: &str, date_suffix: &str) -> Vec<String> {
    let Some(table) = extract_table_name(stmt.trim()) else {
        return Vec::new();
    };
    vec![
        format!("delete from {table};"),
        format!("insert into {table} select * from {table}_bak_{date_suffix};"),
    ]
}

// ── 分类 ────────────────────────────────────────────────────────────────

fn classify_statement(sql: &str) -> Option<Category> {
    if re_create_patterns().iter().any(|re| re.is_match(sql)) {
        return Some(Category::Create);
    }
    if re_column_patterns().iter().any(|re| re.is_match(sql)) {
        return Some(Category::Column);
    }
    if re_dml_patterns().iter().any(|re| re.is_match(sql)) {
        return Some(Category::Dml);
    }
    None
}

/// 按分号切语句：字符串内的分号不切，注释整段丢弃（`splitStatements`）。
///
/// 注意源实现的两处细节，都是刻意的：① 行注释连**换行符一起丢**，所以跨行写法
/// 会被拼成一行；② 最后一条没有分号也会补一个 `;`。
fn split_statements(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut stmts = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied().unwrap_or('\0');

        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if ch == '*' && next == '/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if ch == '-' && next == '-' && !in_single_quote && !in_double_quote {
            in_line_comment = true;
            i += 1;
            continue;
        }

        if ch == '/' && next == '*' && !in_single_quote && !in_double_quote {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if ch == ';' && !in_single_quote && !in_double_quote {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                stmts.push(format!("{trimmed};"));
            }
            current.clear();
            i += 1;
            continue;
        }

        current.push(ch);
        i += 1;
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        stmts.push(if trimmed.ends_with(';') {
            trimmed.to_string()
        } else {
            format!("{trimmed};")
        });
    }

    stmts
}

fn is_not_comment_or_commit(t: &str) -> bool {
    let upper = t.to_uppercase();
    if upper.starts_with("--")
        || upper.starts_with("/*")
        || upper.starts_with('*')
        || upper.ends_with("*/")
    {
        return false;
    }
    !re_commit_only().is_match(&upper)
}

fn find_tagged_blocks(lines: &[String]) -> Vec<TaggedBlock> {
    let mut blocks: Vec<TaggedBlock> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if re_tag_beg().is_match(&lines[i]) {
            let start_idx = i;
            let beg_line = lines[i].clone();
            i += 1;
            let mut block_lines: Vec<String> = Vec::new();
            while i < lines.len() {
                if re_tag_end().is_match(&lines[i]) {
                    blocks.push(TaggedBlock {
                        beg_line,
                        end_line: lines[i].clone(),
                        lines: block_lines,
                        start_idx,
                        end_idx: i,
                    });
                    i += 1;
                    break;
                }
                block_lines.push(lines[i].clone());
                i += 1;
            }
            // 没找到 end：退回起点后一格继续找下一块（源实现的写法，保持一致）。
            if blocks
                .last()
                .is_none_or(|block| block.end_idx != i.saturating_sub(1))
            {
                i = start_idx + 1;
            }
        } else {
            i += 1;
        }
    }
    blocks
}

fn find_untagged_segments(lines: &[String], tagged: &[TaggedBlock]) -> Vec<Vec<String>> {
    if tagged.is_empty() {
        return vec![lines.to_vec()];
    }

    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut last_end: Option<usize> = None;

    for block in tagged {
        let start = last_end.map_or(0, |end| end + 1);
        if block.start_idx > start {
            segments.push(lines[start..block.start_idx].to_vec());
        }
        last_end = Some(block.end_idx);
    }

    let start = last_end.map_or(0, |end| end + 1);
    if start < lines.len() {
        segments.push(lines[start..].to_vec());
    }

    segments
        .into_iter()
        .filter(|seg| seg.iter().any(|line| !line.trim().is_empty()))
        .collect()
}

fn classify_statements(
    statements: &[String],
    acc: &mut Acc,
    tables: &mut std::collections::BTreeSet<String>,
    date_suffix: &str,
    wrap: Option<(&str, &str)>,
) -> usize {
    let mut unclassified = 0;
    for stmt in statements {
        let t = stmt.trim();
        if t.is_empty() || !is_not_comment_or_commit(t) {
            continue;
        }

        let Some(category) = classify_statement(t) else {
            unclassified += 1;
            continue;
        };

        if let Some(table) = extract_table_name(t) {
            tables.insert(table);
        }

        match category {
            Category::Create => {
                acc.create.push(t.to_string());
                let rollback = generate_ddl_rollback(t);
                push_wrapped(&mut acc.ddl_rollback, rollback, wrap);
            }
            Category::Column => {
                acc.column.push(t.to_string());
                let rollback = generate_ddl_rollback(t);
                push_wrapped(&mut acc.ddl_rollback, rollback, wrap);
            }
            Category::Dml => {
                acc.dml.push(t.to_string());
                let rollback = generate_data_rollback(t, date_suffix);
                push_wrapped(&mut acc.data_rollback, rollback, wrap);
            }
        }
    }
    unclassified
}

/// 有标记时把回滚语句包回 `-- add by … beg/end`，并补一个空行（源实现的写法）。
fn push_wrapped(target: &mut Vec<String>, lines: Vec<String>, wrap: Option<(&str, &str)>) {
    if lines.is_empty() {
        return;
    }
    match wrap {
        Some((beg, end)) => {
            target.push(beg.to_string());
            target.extend(lines);
            target.push(end.to_string());
            target.push(String::new());
        }
        None => target.extend(lines),
    }
}

fn deduped_lines(lines: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    lines
        .into_iter()
        .filter(|line| seen.insert(line.clone()))
        .collect()
}

/// 分类一个文件的全文。`date_suffix`（`yyyymmdd`）由调用方给，便于测试钉住输出。
#[must_use]
pub fn classify_sql_file(content: &str, date_suffix: &str) -> ClassifyResult {
    let lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    let tagged = find_tagged_blocks(&lines);
    let untagged = find_untagged_segments(&lines, &tagged);

    let mut out = ClassifyResult::default();
    let mut tables = std::collections::BTreeSet::new();

    // ── 带标记的块：三类分别包上标记，回滚保持标记 ──
    for block in &tagged {
        let block_text = block.lines.join("\n");
        let stmts = split_statements(&block_text);
        let mut acc = Acc::default();
        out.unclassified += classify_statements(
            &stmts,
            &mut acc,
            &mut tables,
            date_suffix,
            Some((&block.beg_line, &block.end_line)),
        );

        if !acc.create.is_empty() {
            out.create.push(block.beg_line.clone());
            out.create.extend(acc.create);
            out.create.push(block.end_line.clone());
            out.create.push(String::new());
        }
        if !acc.column.is_empty() {
            out.column.push(block.beg_line.clone());
            out.column.extend(acc.column);
            out.column.push(block.end_line.clone());
            out.column.push(String::new());
        }
        if !acc.dml.is_empty() {
            out.dml.push(block.beg_line.clone());
            out.dml.extend(acc.dml);
            out.dml.push(block.end_line.clone());
            out.dml.push(String::new());
        }
        out.rollback_ddl.extend(acc.ddl_rollback);
        out.rollback_data.extend(acc.data_rollback);
    }

    // ── 未标记的段落：语句直接按类堆叠，块之间留一个空行 ──
    for segment in &untagged {
        let seg_text = segment.join("\n");
        let stmts = split_statements(&seg_text);
        let mut acc = Acc::default();
        out.unclassified += classify_statements(&stmts, &mut acc, &mut tables, date_suffix, None);

        if !acc.create.is_empty() {
            out.create.extend(acc.create);
            out.create.push(String::new());
        }
        if !acc.column.is_empty() {
            out.column.extend(acc.column);
            out.column.push(String::new());
        }
        if !acc.dml.is_empty() {
            out.dml.extend(acc.dml);
            out.dml.push(String::new());
        }
        out.rollback_ddl.extend(acc.ddl_rollback);
        out.rollback_data.extend(acc.data_rollback);
    }

    for table in &tables {
        out.backup_lines.push(format!(
            "create table {table}_bak_{date_suffix} as select * from {table};"
        ));
    }

    out.rollback_ddl = deduped_lines(out.rollback_ddl);
    out.rollback_data = deduped_lines(out.rollback_data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: &str = "20261018";

    #[test]
    fn splits_into_three_buckets_and_keeps_semicolons() {
        let sql = "\
create table t_user (id int);
alter table t_user add column name varchar2(32);
insert into t_user (id, name) values (1, 'a;b');
commit;
";
        let result = classify_sql_file(sql, DATE);
        // 每批末尾一个空行是源实现的排版（方便手工拼接多个文件）。
        assert_eq!(result.create, vec!["create table t_user (id int);", ""]);
        assert_eq!(
            result.column,
            vec!["alter table t_user add column name varchar2(32);", ""]
        );
        // 字符串里的分号不切；`commit` 不收录；每个块后一个空行。
        assert_eq!(
            result.dml,
            vec!["insert into t_user (id, name) values (1, 'a;b');", ""]
        );
        assert_eq!(result.unclassified, 0, "commit 不算「未分类」");
    }

    #[test]
    fn removes_comments_and_drops_blank_input() {
        let sql = "-- 头部\n/* 块注释 */\n\nselect 1 from dual;\n";
        let result = classify_sql_file(sql, DATE);
        // `select` 三类都不属于 ⇒ 计入未分类，且不进任何输出文件。
        assert_eq!(result.unclassified, 1);
        assert!(result.create.is_empty() && result.column.is_empty() && result.dml.is_empty());
    }

    #[test]
    fn generates_backup_and_data_rollback_per_table() {
        let sql = "insert into t_a (id) values (1);\nupdate t_b set x = 1;\n";
        let result = classify_sql_file(sql, DATE);
        assert_eq!(
            result.backup_lines,
            vec![
                "create table t_a_bak_20261018 as select * from t_a;",
                "create table t_b_bak_20261018 as select * from t_b;",
            ]
        );
        assert_eq!(
            result.rollback_data,
            vec![
                "delete from t_a;",
                "insert into t_a select * from t_a_bak_20261018;",
                "delete from t_b;",
                "insert into t_b select * from t_b_bak_20261018;",
            ]
        );
    }

    #[test]
    fn ddl_rollback_mirrors_each_shape() {
        let cases = [
            ("create table t1 (id int)", "drop table t1;"),
            ("create sequence seq1", "drop sequence seq1;"),
            ("create unique index idx1 on t1 (id)", "drop index idx1;"),
            // 见 `generate_ddl_rollback` 的注释：加列必须回滚成「删列」，
            // 不能像源实现那样退化成「删表」。
            (
                "alter table t1 add column c1 int",
                "alter table t1 drop column c1;",
            ),
            ("rename t1 to t1_old", "rename t1_old to t1;"),
        ];
        for (stmt, expected) in cases {
            let got = generate_ddl_rollback(stmt);
            assert_eq!(got, vec![expected.to_string()], "stmt = {stmt}");
        }
        // `drop table if exists` 会被当成「删表」处理（源实现的顺序如此，
        // `extractTable` 的 `if exists` 是可选组）——回滚是重建前的兜底，不再补 DDL。
        assert_eq!(
            generate_ddl_rollback("drop table if exists t9"),
            vec!["drop table t9;".to_string()]
        );
        // 认不出的形状不产生回滚（源实现同样返回空）。
        assert!(generate_ddl_rollback("truncate table t9").is_empty());
        // 列名拿不到时不猜（`add (…)` 这种写法交给源实现的「删表」路径）。
        assert_eq!(
            generate_ddl_rollback("alter table t9 add (c1 int)"),
            vec!["drop table t9;".to_string()]
        );
        // 加约束没法机械反向：不写回滚，更不能退化成删表。
        assert!(
            generate_ddl_rollback("alter table t9 add constraint pk_t9 primary key (id)")
                .is_empty()
        );
    }

    #[test]
    fn tagged_blocks_are_wrapped_in_every_output() {
        let sql = "\
-- add by zhangsan 2026-01-01 beg
create table t_c (id int);
insert into t_c (id) values (1);
-- add by zhangsan 2026-01-01 end
";
        let result = classify_sql_file(sql, DATE);
        assert_eq!(
            result.create,
            vec![
                "-- add by zhangsan 2026-01-01 beg",
                "create table t_c (id int);",
                "-- add by zhangsan 2026-01-01 end",
                "",
            ]
        );
        assert_eq!(
            result.dml,
            vec![
                "-- add by zhangsan 2026-01-01 beg",
                "insert into t_c (id) values (1);",
                "-- add by zhangsan 2026-01-01 end",
                "",
            ]
        );
        // 回滚也带标记：贴回发布脚本时不丢「谁改的、什么时候」。
        assert_eq!(
            result.rollback_ddl,
            vec![
                "-- add by zhangsan 2026-01-01 beg",
                "drop table t_c;",
                "-- add by zhangsan 2026-01-01 end",
                "",
            ]
        );
    }

    #[test]
    fn rollback_lines_are_deduplicated_but_marker_kept() {
        let sql = "insert into t_d (id) values (1);\ninsert into t_d (id) values (2);\n";
        let result = classify_sql_file(sql, DATE);
        assert_eq!(
            result.rollback_data,
            vec![
                "delete from t_d;",
                "insert into t_d select * from t_d_bak_20261018;",
            ],
            "同一张表的重复回滚要去重"
        );
    }

    #[test]
    fn clean_name_strips_quotes_and_column_lists() {
        assert_eq!(clean_name("\"t_user\""), "t_user");
        assert_eq!(clean_name("`t_user`"), "t_user");
        assert_eq!(clean_name("t_user(id);"), "t_user");
        assert_eq!(clean_name("t_user(col"), "t_user");
    }

    #[test]
    fn qualified_removes_only_the_outer_quotes() {
        assert_eq!(qualified("\"a\"\"b\""), "a\"\"b");
    }
}
