//! 建表 SQL / 数据字典 Markdown（复刻参考仓 `pdm/sql_gen.rs`）。
//!
//! 方言只有两档（`mysql` / 其余按 Oracle），与参考仓一致：引号字符 + 主键写法 +
//! 是否带注释列 + 语句结尾分号。表名的写法见 crate 文档「两处主动修复」第 1 条。
//!
//! [`generate_comment_sql`] 来自参考仓**前端**（`PdmReader.vue` 的 `generateCommentSql`，
//! 后端没有对应实现）：Oracle 的表 / 列注释必须单独 `COMMENT ON`，导完建表语句
//! 通常紧接着就要这一份。

use crate::pdm::labels::Labels;
use crate::pdm::models::*;

pub fn generate_create_table_sql(table: &Table, dialect: &str) -> String {
    let mut sql = String::new();
    let q = if dialect == "mysql" { '`' } else { '"' };

    // 参考仓这里写成了 `CREATE TABLE {q} (` + `\n    {code} (\n`（表名没进语句、
    // 少一个右括号）。按显而易见的意思：表名整体在括号前，列从下一行开始。
    sql.push_str(&format!("CREATE TABLE {q}{}{q} (", table.code));

    let col_lines: Vec<String> = table
        .columns
        .iter()
        .map(|col| {
            let mut line = format!("    {q}{}{q} {}", col.code, col.data_type);
            if col.mandatory {
                line.push_str(" NOT NULL");
            }
            if !col.default_value.is_empty() {
                let dv =
                    if dialect == "oracle" && col.data_type.to_uppercase().starts_with("VARCHAR") {
                        format!("'{}'", col.default_value.trim_matches('\''))
                    } else {
                        col.default_value.clone()
                    };
                line.push_str(&format!(" DEFAULT {}", dv));
            }
            if !col.comment.is_empty() && dialect == "mysql" {
                line.push_str(&format!(" COMMENT '{}'", col.comment.replace('\'', "\\'")));
            }
            line
        })
        .collect();

    sql.push('\n');
    sql.push_str(&col_lines.join(",\n"));

    if !table.primary_key.is_empty() {
        let pk_cols: Vec<String> = table
            .primary_key
            .iter()
            .map(|c| format!("{q}{c}{q}"))
            .collect();
        sql.push_str(&format!(",\n    PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    sql.push_str("\n)");

    if dialect == "oracle" {
        sql.push(';');
    }

    sql
}

pub fn generate_drop_table_sql(table: &Table, dialect: &str) -> String {
    let q = if dialect == "mysql" { '`' } else { '"' };
    format!("DROP TABLE {q}{}{q};", table.code)
}

/// 插入语句**骨架**（复刻参考仓前端 `PdmReader.vue::generateInsertSql`，后端无对应实现）。
///
/// 只把全部列名摆好、值留给用户填 —— 参考仓的意图是「省掉敲列名」，不是造数据。
/// 与参考仓逐字节一致的三处细节（别「优化」掉）：
///
/// 1. 标识符用**反引号**，与建表语句的方言无关（源仓这里写死 MySQL 风格）；
/// 2. `VALUES (` 不闭合，紧接着一行 `-- TODO: 添加值`，再接 `;`——填值时替换那一行；
/// 3. 每张表后面留一个空行（`join("\n")` 的产物，末表也留）。
pub fn generate_insert_sql(tables: &[Table], labels: &Labels) -> String {
    let mut lines: Vec<String> = Vec::new();
    for table in tables {
        let cols: Vec<String> = table
            .columns
            .iter()
            .map(|col| format!("`{}`", col.code))
            .collect();
        lines.push(format!(
            "-- {}: {} ({})",
            labels.table, table.name, table.code
        ));
        lines.push(format!(
            "INSERT INTO `{}` ({}) VALUES (",
            table.code,
            cols.join(", ")
        ));
        lines.push(format!("  -- {}", labels.todo_values));
        lines.push(";".to_string());
        lines.push(String::new());
    }
    lines.join("\n")
}

/// 表 / 列注释（Oracle 语法，标识符一律双引号；`'` 按 SQL 规矩翻倍）。
pub fn generate_comment_sql(tables: &[Table]) -> String {
    let mut out = String::new();
    for table in tables {
        if !table.comment.is_empty() {
            out.push_str(&format!(
                "COMMENT ON TABLE \"{}\" IS '{}';\n",
                table.code,
                table.comment.replace('\'', "''")
            ));
        }
        for col in &table.columns {
            if col.comment.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "COMMENT ON COLUMN \"{}\".\"{}\" IS '{}';\n",
                table.code,
                col.code,
                col.comment.replace('\'', "''")
            ));
        }
        out.push('\n');
    }
    out
}

pub fn generate_data_dictionary_md(model: &PdmModel, labels: &Labels) -> String {
    let mut md = String::new();
    md.push_str(&format!("# {} - {}\n\n", model.name, labels.title));
    md.push_str(&format!(
        "> {}: {}\n\n",
        labels.generated_at,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    md.push_str(&format!("- {}: {}\n", labels.target_database, model.target));
    md.push_str(&format!("- {}: {}\n\n", labels.tables, model.tables.len()));
    md.push_str("---\n\n");

    for (i, table) in model.tables.iter().enumerate() {
        md.push_str(&format!(
            "## {}. {} ({})\n\n",
            i + 1,
            table.name,
            table.code
        ));
        if !table.comment.is_empty() {
            md.push_str(&format!("> {}\n\n", table.comment));
        }

        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            labels.field,
            labels.data_type,
            labels.length,
            labels.precision,
            labels.mandatory,
            labels.default_value,
            labels.comment
        ));
        md.push_str("|--------|------|------|------|------|--------|------|\n");

        for col in &table.columns {
            let len = col.length.map(|l| l.to_string()).unwrap_or_default();
            let prec = col.precision.map(|p| p.to_string()).unwrap_or_default();
            let mandatory = if col.mandatory { "✓" } else { "" };
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                col.code, col.data_type, len, prec, mandatory, col.default_value, col.comment
            ));
        }

        if !table.primary_key.is_empty() {
            md.push_str(&format!(
                "\n**{}**: {}\n\n",
                labels.primary_key,
                table.primary_key.join(", ")
            ));
        }

        if !table.indexes.is_empty() {
            md.push_str(&format!("\n**{}**:\n\n", labels.index));
            for idx in &table.indexes {
                md.push_str(&format!("- {} ({})\n", idx.name, idx.columns.join(", ")));
            }
            md.push('\n');
        }

        md.push_str("\n---\n\n");
    }

    md
}
