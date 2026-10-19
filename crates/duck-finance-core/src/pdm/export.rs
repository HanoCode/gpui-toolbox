//! 数据字典的四种 HTML 产物（复刻参考仓 `pdm/export.rs`）。
//!
//! 参考仓把「浏览器打印用的 HTML」「Excel 的 HTML 方言」「Word 的 HTML 方言」
//! 「A4 打印版」各写了一个函数：结构几乎一样，差别在 `<head>` 里的 Office 命名空间、
//! CSS 与列数（Excel / Word 多一列「代码」）。本仓逐字保留这四份，不做合并——
//! 合并就等于重写一遍排版，而排版正是用户拿去打印的东西。
//!
//! 与参考仓的唯一差别：表头词从 [`Labels`] 传入（内核不出中文）。

use chrono::Local;

use crate::pdm::labels::Labels;
use crate::pdm::models::*;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn build_table_rows(table: &Table) -> String {
    let mut rows = String::new();
    for col in &table.columns {
        let len = col.length.map(|l| l.to_string()).unwrap_or_default();
        let prec = col.precision.map(|p| p.to_string()).unwrap_or_default();
        let mandatory = if col.mandatory { "✓" } else { "" };
        let dv = if col.default_value.is_empty() {
            ""
        } else {
            &col.default_value
        };
        let is_pk = if table.primary_key.contains(&col.code) {
            " pk"
        } else {
            ""
        };
        rows.push_str(&format!(
            "<tr class=\"{}\"><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            is_pk,
            esc(&col.code),
            esc(&col.data_type),
            len,
            prec,
            mandatory,
            esc(dv),
            esc(&col.comment)
        ));
    }
    rows
}

fn build_index_text(table: &Table, labels: &Labels) -> String {
    if table.indexes.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = table
        .indexes
        .iter()
        .map(|idx| {
            format!(
                "{} ({})",
                esc(&idx.name),
                idx.columns
                    .iter()
                    .map(|c| esc(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    format!(
        "<div class=\"idx\"><strong>{}:</strong> {}</div>",
        labels.index,
        parts.join("; ")
    )
}

fn css_styles() -> &'static str {
    r#"
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, 'Microsoft YaHei', sans-serif; background: #f5f7fa; color: #1a1a2e; padding: 40px; }
.container { max-width: 1200px; margin: 0 auto; }
.header { background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); color: #fff; padding: 40px; border-radius: 16px; margin-bottom: 32px; }
.header h1 { font-size: 28px; margin-bottom: 8px; }
.stats { display: flex; gap: 16px; margin-top: 16px; }
.stat { background: rgba(255,255,255,0.15); padding: 12px 20px; border-radius: 8px; font-size: 13px; }
.card { background: #fff; border-radius: 12px; margin-bottom: 24px; box-shadow: 0 1px 3px rgba(0,0,0,0.08); overflow: hidden; }
.ch { padding: 16px 20px; background: #f8f9fc; border-bottom: 1px solid #e8ecf1; }
.ch h2 { font-size: 16px; color: #1a1a2e; }
.ch .code { font-size: 12px; color: #667eea; margin-top: 2px; }
.ch .cmt { font-size: 12px; color: #6b7280; margin-top: 4px; }
table { width: 100%; border-collapse: collapse; font-size: 13px; }
th { background: #f8f9fc; padding: 10px 16px; text-align: left; font-weight: 600; color: #374151; border-bottom: 2px solid #e5e7eb; }
td { padding: 10px 16px; border-bottom: 1px solid #f3f4f6; color: #4b5563; }
tr.pk { background: #f5f3ff; }
.idx { padding: 12px 20px; border-top: 1px solid #f3f4f6; font-size: 13px; color: #6b7280; }
.footer { text-align: center; padding: 24px; font-size: 12px; color: #9ca3af; }
@media print { body { padding: 0; } .header { border-radius: 0; } .card { break-inside: avoid; } }
"#
}

pub fn generate_html(model: &PdmModel, labels: &Labels) -> String {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut h = format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\">\
        <title>{} - {}</title><style>{}</style></head><body>\
        <div class=\"container\"><div class=\"header\"><h1>{} - {}</h1>\
        <div class=\"stats\"><div class=\"stat\">{}: {}</div>\
        <div class=\"stat\">{}: {}</div></div></div>",
        esc(&model.name),
        labels.title,
        css_styles(),
        esc(&model.name),
        labels.title,
        labels.tables,
        model.tables.len(),
        labels.target,
        esc(&model.target)
    );

    for table in &model.tables {
        let pk = if table.primary_key.is_empty() {
            "—".to_string()
        } else {
            table.primary_key.join(", ")
        };
        h.push_str(&format!(
            "<div class=\"card\"><div class=\"ch\"><h2>{} ({})</h2>\
            <div class=\"code\">{}  {}: {}</div>",
            esc(&table.name),
            esc(&table.code),
            esc(&table.code),
            labels.primary_key,
            esc(&pk)
        ));
        if !table.comment.is_empty() {
            h.push_str(&format!("<div class=\"cmt\">{}</div>", esc(&table.comment)));
        }
        h.push_str(&format!(
            "</div><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
            labels.field,
            labels.data_type,
            labels.length,
            labels.precision,
            labels.mandatory,
            labels.default_value,
            labels.comment
        ));
        h.push_str(&build_table_rows(table));
        h.push_str("</tbody></table>");
        h.push_str(&build_index_text(table, labels));
        h.push_str("</div>\n");
    }

    h.push_str(&format!(
        "<div class=\"footer\">{} · {}</div></div></body></html>",
        labels.footer, now
    ));
    h
}

pub fn generate_excel_html(model: &PdmModel, labels: &Labels) -> String {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut h = format!(
        "<html xmlns:o='urn:schemas-microsoft-com:office:office' \
        xmlns:x='urn:schemas-microsoft-com:office:excel' \
        xmlns='http://www.w3.org/TR/REC-html40'>\
        <head><meta charset=\"UTF-8\"><!--[if gte mso 9]>\
        <xml><x:ExcelWorkbook><x:ExcelWorksheets><x:ExcelWorksheet>\
        <x:Name>{}</x:Name><x:WorksheetOptions><x:DisplayGridlines/></x:WorksheetOptions>\
        </x:ExcelWorksheet></x:ExcelWorksheets></x:ExcelWorkbook></xml><![endif]-->\
        <style>td {{ mso-number-format:\"\\@\"; }}</style></head><body>\
        <h2>{} - {}</h2><p>{}: {} | {}: {} | {}: {}</p>",
        labels.title,
        esc(&model.name),
        labels.title,
        labels.generated_at,
        now,
        labels.tables,
        model.tables.len(),
        labels.target,
        esc(&model.target)
    );

    for table in &model.tables {
        let pk = if table.primary_key.is_empty() {
            "—".to_string()
        } else {
            table.primary_key.join(", ")
        };
        h.push_str(&format!(
            "<h3>{} ({})  {}: {}</h3>",
            esc(&table.name),
            esc(&table.code),
            labels.primary_key,
            esc(&pk)
        ));
        if !table.comment.is_empty() {
            h.push_str(&format!("<p><i>{}</i></p>", esc(&table.comment)));
        }
        h.push_str(&format!(
            "<table border=\"1\" cellpadding=\"4\" cellspacing=\"0\" style=\"border-collapse:collapse;\">\
            <tr style=\"background:#4472c4;color:#fff;\">\
            <th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr>",
            labels.field,
            labels.code,
            labels.data_type,
            labels.length,
            labels.precision,
            labels.mandatory,
            labels.default_value,
            labels.comment
        ));
        for col in &table.columns {
            let len = col.length.map(|l| l.to_string()).unwrap_or_default();
            let prec = col.precision.map(|p| p.to_string()).unwrap_or_default();
            let mandatory = if col.mandatory { "✓" } else { "" };
            let dv = if col.default_value.is_empty() {
                ""
            } else {
                &col.default_value
            };
            let bg = if table.primary_key.contains(&col.code) {
                " style=\"background:#f0f0ff;\""
            } else {
                ""
            };
            h.push_str(&format!(
                "<tr{}><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td align=\"center\">{}</td><td>{}</td><td>{}</td></tr>",
                bg,
                esc(&col.name),
                esc(&col.code),
                esc(&col.data_type),
                len,
                prec,
                mandatory,
                esc(dv),
                esc(&col.comment)
            ));
        }
        h.push_str("</table><br>");

        if !table.indexes.is_empty() {
            h.push_str(&format!("<p><b>{}:</b> ", labels.index));
            let parts: Vec<String> = table
                .indexes
                .iter()
                .map(|idx| format!("{} ({})", esc(&idx.name), idx.columns.join(", ")))
                .collect();
            h.push_str(&parts.join("; "));
            h.push_str("</p>");
        }
        h.push_str("<br>");
    }
    h.push_str("</body></html>");
    h
}

pub fn generate_word_html(model: &PdmModel, labels: &Labels) -> String {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut h = format!(
        "<html xmlns:o='urn:schemas-microsoft-com:office:office' \
        xmlns:w='urn:schemas-microsoft-com:office:word' \
        xmlns='http://www.w3.org/TR/REC-html40'>\
        <head><meta charset=\"UTF-8\"><title>{} - {}</title>\
        <!--[if gte mso 9]><xml><w:WordDocument><w:View>Print</w:View></w:WordDocument></xml><![endif]-->\
        <style>body {{ font-family: 'Microsoft YaHei', sans-serif; font-size: 11pt; }} \
        h1 {{ color: #2c3e50; font-size: 18pt; }} h2 {{ color: #34495e; font-size: 14pt; }} \
        table {{ border-collapse: collapse; width: 100%; margin-bottom: 12pt; }} \
        th {{ background: #4472c4; color: #fff; padding: 4pt 8pt; text-align: left; font-size: 10pt; }} \
        td {{ border: 1px solid #ccc; padding: 3pt 8pt; font-size: 10pt; }} \
        .pk {{ background: #f0f0ff; }} \
        .idx {{ font-size: 10pt; margin-bottom: 12pt; }}</style></head><body>\
        <h1>{} - {}</h1>\
        <p>{}: {} | {}: {} | {}: {}</p>",
        esc(&model.name),
        labels.title,
        esc(&model.name),
        labels.title,
        labels.generated_at,
        now,
        labels.tables,
        model.tables.len(),
        labels.target_database,
        esc(&model.target)
    );

    for table in &model.tables {
        let pk = if table.primary_key.is_empty() {
            "—".to_string()
        } else {
            table.primary_key.join(", ")
        };
        h.push_str(&format!(
            "<h2>{} <span style=\"color:#667eea;font-size:10pt;\">({})</span></h2>\
            <p>{}: {} | {}: {}</p>",
            esc(&table.name),
            esc(&table.code),
            labels.primary_key,
            esc(&pk),
            labels.columns,
            table.columns.len()
        ));
        if !table.comment.is_empty() {
            h.push_str(&format!("<p><i>{}</i></p>", esc(&table.comment)));
        }
        h.push_str(&format!(
            "<table><tr>\
            <th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr>",
            labels.field,
            labels.code,
            labels.data_type,
            labels.length,
            labels.precision,
            labels.mandatory,
            labels.default_value,
            labels.comment
        ));
        for col in &table.columns {
            let len = col.length.map(|l| l.to_string()).unwrap_or_default();
            let prec = col.precision.map(|p| p.to_string()).unwrap_or_default();
            let mandatory = if col.mandatory { "✓" } else { "" };
            let dv = if col.default_value.is_empty() {
                ""
            } else {
                &col.default_value
            };
            let cls = if table.primary_key.contains(&col.code) {
                " class=\"pk\""
            } else {
                ""
            };
            h.push_str(&format!(
                "<tr{}><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td align=\"center\">{}</td><td>{}</td><td>{}</td></tr>",
                cls,
                esc(&col.name),
                esc(&col.code),
                esc(&col.data_type),
                len,
                prec,
                mandatory,
                esc(dv),
                esc(&col.comment)
            ));
        }
        h.push_str("</table>");

        if !table.indexes.is_empty() {
            let parts: Vec<String> = table
                .indexes
                .iter()
                .map(|idx| format!("{} ({})", esc(&idx.name), idx.columns.join(", ")))
                .collect();
            h.push_str(&format!(
                "<p class=\"idx\"><b>{}:</b> {}</p>",
                labels.index,
                parts.join("; ")
            ));
        }
        h.push_str("<br>");
    }
    h.push_str("</body></html>");
    h
}

pub fn generate_pdf_html(model: &PdmModel, labels: &Labels) -> String {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut h = format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\">\
        <title>{} - {}</title>\
        <style>\
        @page {{ size: A4; margin: 1.5cm; }}\
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}\
        body {{ font-family: 'Microsoft YaHei', sans-serif; font-size: 10pt; color: #333; }}\
        h1 {{ font-size: 18pt; color: #2c3e50; margin-bottom: 4pt; }}\
        h2 {{ font-size: 13pt; color: #34495e; margin-top: 16pt; margin-bottom: 4pt; }}\
        .meta {{ font-size: 9pt; color: #666; margin-bottom: 12pt; }}\
        table {{ width: 100%; border-collapse: collapse; margin-bottom: 10pt; font-size: 8.5pt; }}\
        th {{ background: #4472c4; color: #fff; padding: 3pt 5pt; text-align: left; font-weight: 600; }}\
        td {{ border: 0.5pt solid #bbb; padding: 2pt 5pt; }}\
        tr:nth-child(even) {{ background: #f8f9fa; }}\
        .pk {{ background: #f0f0ff; }}\
        .idx {{ font-size: 8.5pt; color: #666; margin-bottom: 10pt; }}\
        .footer {{ text-align: center; font-size: 7.5pt; color: #999; margin-top: 20pt; border-top: 0.5pt solid #ddd; padding-top: 6pt; }}\
        @media print {{ body {{ margin: 0; }} }}\
        </style></head><body>\
        <h1>{} - {}</h1>\
        <div class=\"meta\">{}: {} &nbsp;|&nbsp; {}: {} &nbsp;|&nbsp; {}: {}</div>",
        esc(&model.name),
        labels.title,
        esc(&model.name),
        labels.title,
        labels.generated_at,
        now,
        labels.tables,
        model.tables.len(),
        labels.target_database,
        esc(&model.target)
    );

    for table in &model.tables {
        let pk = if table.primary_key.is_empty() {
            "—".to_string()
        } else {
            table.primary_key.join(", ")
        };
        h.push_str(&format!(
            "<h2>{} <span style=\"color:#667eea;font-weight:normal;font-size:10pt;\">({})</span></h2>\
            <div class=\"meta\">{}: {} | {}: {}</div>",
            esc(&table.name),
            esc(&table.code),
            labels.primary_key,
            esc(&pk),
            labels.columns,
            table.columns.len()
        ));
        if !table.comment.is_empty() {
            h.push_str(&format!(
                "<p style=\"font-size:9pt;color:#666;margin-bottom:4pt;\">{}</p>",
                esc(&table.comment)
            ));
        }
        h.push_str(&format!(
            "<table><tr>\
            <th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr>",
            labels.field,
            labels.code,
            labels.data_type,
            labels.length,
            labels.mandatory,
            labels.default_value,
            labels.comment
        ));
        for col in &table.columns {
            let len = col.length.map(|l| l.to_string()).unwrap_or_default();
            let mandatory = if col.mandatory { "✓" } else { "" };
            let dv = if col.default_value.is_empty() {
                ""
            } else {
                &col.default_value
            };
            let cls = if table.primary_key.contains(&col.code) {
                " class=\"pk\""
            } else {
                ""
            };
            h.push_str(&format!(
                "<tr{}><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td align=\"center\">{}</td><td>{}</td><td>{}</td></tr>",
                cls,
                esc(&col.name),
                esc(&col.code),
                esc(&col.data_type),
                len,
                mandatory,
                esc(dv),
                esc(&col.comment)
            ));
        }
        h.push_str("</table>");
        if !table.indexes.is_empty() {
            let parts: Vec<String> = table
                .indexes
                .iter()
                .map(|idx| format!("{} ({})", esc(&idx.name), idx.columns.join(", ")))
                .collect();
            h.push_str(&format!(
                "<div class=\"idx\">{}: {}</div>",
                labels.index,
                parts.join("; ")
            ));
        }
    }
    h.push_str(&format!(
        "<div class=\"footer\">{} · {}</div></body></html>",
        labels.footer, now
    ));
    h
}
