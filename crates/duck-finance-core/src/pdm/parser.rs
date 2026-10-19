//! PowerDesigner PDM 解析（逐字复刻参考仓 `pdm/parser.rs`）。
//!
//! PDM 是 PowerDesigner 的 XML 方言：`<o:Table>` / `<c:Columns>` / `<a:Code>` 三个
//! 命名空间混排，主键与索引都是「先收集 Id → 再按 `Ref` 回指」的两段式引用。
//! 所以这份解析器不建 DOM，而是走一遍事件流 + 一张 `Id → 列下标` 的表。
//!
//! 两处「源仓如此」的细节，别按直觉改：
//! 1. 元素名用 `local_name()`（丢掉命名空间前缀）——`o:Table` 与 `Table` 都认；
//! 2. 文本只有落在 [`TEXT_ATTRS`] 里的元素才收，且 `cur_attr` 在对应 `End` 时清空
//!    ——PDM 里同一层会有大量无关元素（`<a:ObjectID>`、`<c:Tables>`），不设白名单
//!    会把 `Id` 之类的值串到别处。

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::pdm::models::*;

const TEXT_ATTRS: [&str; 11] = [
    "Name",
    "Code",
    "Comment",
    "Description",
    "DataType",
    "Length",
    "Precision",
    "Mandatory",
    "DefaultValue",
    "ConstraintName",
    "ObjectID",
];

pub struct PdmParser;

#[derive(Clone, PartialEq)]
enum State {
    Root,
    Model,
    Table,
    Columns,
    Column,
    Keys,
    Key,
    KeyColumns,
    PrimaryKey,
    Indexes,
    Index,
    IndexColumns,
    IndexColumn,
}

impl PdmParser {
    pub fn parse(xml: &str) -> Result<PdmModel, String> {
        let mut reader = Reader::from_str(xml);

        let mut model = PdmModel::default();
        let mut current_table = Table::default();
        let mut current_column = Column::default();
        let mut current_index = Index::default();

        let mut column_id_map: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut key_columns_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut pending_key_id: Option<String> = None;
        let mut pending_key_columns: Vec<String> = Vec::new();

        let mut cur_attr = String::new();
        let mut buf = Vec::new();

        let mut stack: Vec<State> = vec![State::Root];

        macro_rules! top {
            () => {
                stack.last().cloned().unwrap_or(State::Root)
            };
        }

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    let cur_state = top!();

                    match name.as_str() {
                        "Model" => stack.push(State::Model),
                        "Table" => {
                            current_table = Table::default();
                            column_id_map.clear();
                            key_columns_map.clear();
                            pending_key_id = None;
                            pending_key_columns.clear();
                            stack.push(State::Table);
                        }
                        "Columns" if cur_state == State::Table => stack.push(State::Columns),
                        "Column" if cur_state == State::Columns => {
                            current_column = Column::default();
                            if let Some(id) = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"Id")
                            {
                                let cid = String::from_utf8_lossy(&id.value).to_string();
                                column_id_map.insert(cid, current_table.columns.len());
                            }
                            stack.push(State::Column);
                        }
                        "Keys" if cur_state == State::Table => stack.push(State::Keys),
                        "Key" if cur_state == State::Keys => {
                            pending_key_id = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"Id")
                                .map(|a| String::from_utf8_lossy(&a.value).to_string());
                            pending_key_columns.clear();
                            stack.push(State::Key);
                        }
                        "Key.Columns" if cur_state == State::Key => stack.push(State::KeyColumns),
                        "PrimaryKey" if cur_state == State::Table => stack.push(State::PrimaryKey),
                        "Indexes" if cur_state == State::Table => stack.push(State::Indexes),
                        "Index" if cur_state == State::Indexes => {
                            current_index = Index::default();
                            stack.push(State::Index);
                        }
                        "IndexColumns" if cur_state == State::Index => {
                            stack.push(State::IndexColumns);
                        }
                        "IndexColumn" if cur_state == State::IndexColumns => {
                            stack.push(State::IndexColumn);
                        }
                        _ => {
                            if TEXT_ATTRS.contains(&name.as_str()) {
                                cur_attr = name;
                            }
                        }
                    }
                }
                Ok(Event::Text(ref e)) => {
                    if cur_attr.is_empty() {
                        continue;
                    }
                    let text = e.unescape().unwrap_or_default().to_string();
                    let val = text.trim().to_string();
                    if val.is_empty() {
                        continue;
                    }

                    match top!() {
                        State::Model => match cur_attr.as_str() {
                            "Name" => {
                                model.name = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Code" => {
                                model.code = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Comment" | "Description" => {
                                model.description = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            _ => {}
                        },
                        State::Table => match cur_attr.as_str() {
                            "Name" => {
                                current_table.name = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Code" => {
                                current_table.code = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Comment" => {
                                current_table.comment = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            _ => {}
                        },
                        State::Column => match cur_attr.as_str() {
                            "Name" => {
                                current_column.name = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Code" => {
                                current_column.code = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "DataType" => {
                                current_column.data_type = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Length" => current_column.length = val.parse().ok(),
                            "Precision" => current_column.precision = val.parse().ok(),
                            "Mandatory" => current_column.mandatory = val == "1",
                            "DefaultValue" => {
                                current_column.default_value = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Comment" => {
                                current_column.comment = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            _ => {}
                        },
                        State::Index => match cur_attr.as_str() {
                            "Name" => {
                                current_index.name = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            "Code" => {
                                current_index.code = if val == "undefined" {
                                    String::new()
                                } else {
                                    val
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    match name.as_str() {
                        "Model" => {
                            stack.pop();
                        }
                        "Table" => {
                            stack.pop();
                            if !current_table.columns.is_empty() || !current_table.code.is_empty() {
                                model.tables.push(current_table.clone());
                            }
                            current_table = Table::default();
                        }
                        "Column" => {
                            if top!() == State::Column {
                                stack.pop();
                                current_table.columns.push(current_column.clone());
                            }
                        }
                        "Key" => {
                            if top!() == State::Key {
                                stack.pop();
                                if let Some(kid) = &pending_key_id {
                                    key_columns_map
                                        .insert(kid.clone(), pending_key_columns.clone());
                                }
                                pending_key_id = None;
                                pending_key_columns.clear();
                            }
                        }
                        "Index" => {
                            if top!() == State::Index {
                                stack.pop();
                                if !current_index.name.is_empty() || !current_index.code.is_empty()
                                {
                                    current_table.indexes.push(current_index.clone());
                                }
                            }
                        }
                        "IndexColumn" => {
                            if top!() == State::IndexColumn {
                                stack.pop();
                            }
                        }
                        "Key.Columns" => {
                            stack.pop();
                        }
                        "PrimaryKey" | "Columns" | "Keys" | "Indexes" | "IndexColumns" => {
                            stack.pop();
                        }
                        _ => {
                            if TEXT_ATTRS.contains(&name.as_str()) {
                                cur_attr.clear();
                            }
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    let cur_state = top!();

                    match (name.as_str(), cur_state) {
                        ("Column", State::KeyColumns) => {
                            if let Some(r) = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"Ref")
                            {
                                let ref_id = String::from_utf8_lossy(&r.value).to_string();
                                if let Some(&idx) = column_id_map.get(&ref_id) {
                                    pending_key_columns
                                        .push(current_table.columns[idx].code.clone());
                                }
                            }
                        }
                        ("Column", State::IndexColumn) => {
                            if let Some(r) = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"Ref")
                            {
                                let ref_id = String::from_utf8_lossy(&r.value).to_string();
                                if let Some(&idx) = column_id_map.get(&ref_id) {
                                    current_index
                                        .columns
                                        .push(current_table.columns[idx].code.clone());
                                }
                            }
                        }
                        ("Key", State::PrimaryKey) => {
                            if let Some(r) = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"Ref")
                            {
                                let ref_id = String::from_utf8_lossy(&r.value).to_string();
                                if let Some(cols) = key_columns_map.get(&ref_id) {
                                    current_table.primary_key = cols.clone();
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::PI(ref e)) => {
                    let content = String::from_utf8_lossy(e.as_ref()).to_string();
                    if let Some(start) = content.find("Target=\"") {
                        let s = start + 8;
                        if let Some(end) = content[s..].find('"') {
                            model.target = content[s..s + end].to_string();
                        }
                    }
                }
                Ok(Event::Eof) => break,
                // 细节原样上交，整句由插件层拼（内核不出中文）。
                Err(e) => return Err(e.to_string()),
                _ => {}
            }
            buf.clear();
        }

        Ok(model)
    }
}
