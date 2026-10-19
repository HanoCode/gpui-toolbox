use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PdmModel {
    pub name: String,
    pub code: String,
    pub description: String,
    pub target: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Table {
    pub name: String,
    pub code: String,
    pub comment: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub indexes: Vec<Index>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Column {
    pub name: String,
    pub code: String,
    pub data_type: String,
    pub length: Option<i64>,
    pub precision: Option<i64>,
    pub mandatory: bool,
    pub default_value: String,
    pub comment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Index {
    pub name: String,
    pub code: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdmFileResult {
    pub file_name: String,
    pub file_path: String,
    pub model: PdmModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdmDirectoryResult {
    pub files: Vec<PdmFileResult>,
    pub combined: PdmModel,
}
