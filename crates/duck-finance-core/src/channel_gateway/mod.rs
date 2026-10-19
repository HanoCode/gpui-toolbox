//! 渠道网关模型生成（复刻参考仓 `channel_gateway/`）。
//!
//! 三件事：
//!
//! 1. [`excel`]：把银行给的接口清单 Excel 读成「接口 → 请求 / 应答字段」（1,162 行，逐字）；
//! 2. [`code`]：按 Bank / Host 两份 Java 模板生成报文转换实现（模板逐字节复制）；
//! 3. [`generate_xml_template`]：给每个接口生成 `<lcpt>` 请求 / 应答报文骨架，
//!    填数据前先拿去和银行对字段。
//!
//! ## 持久化：文件，不是 SQLite
//!
//! 参考仓把「接口库 / 版本快照 / 变更记录 / 自定义模板」都存在自己的 SQLite 里
//! （`channel_gateway/db.rs`，710 行，6 张表）。本仓**不引数据库**：这三样东西的真实
//! 体量是「几十个接口 × 几十个字段」，一份 JSON 几百 KB，用不着事务与索引。
//!
//! 于是 [`store`] 用「一个目录 + 几个 JSON 文件」复刻了那 710 行的**全部读写语义**：
//! 保存时算变更、有变更先给旧数据打快照、版本两两对比、单接口差异、模板增删改查。
//! 落盘位置是插件的**私有数据目录**（`<app_data>/plugin-data/<plugin_id>/`），
//! 由宿主经 `DUCK_DATA_DIR` 给出——与 JS 轨的 `duck.dataDir` 同一个目录。
//!
//! 没迁的只剩 `cg_generate_template_excel`（按模板造一份空白 Excel 给银行填）：
//! 它要写用户指定的文件，属于「生成物落盘」，与本族「产物落结果区」的口径不同。
//!
//! ## `ExcelParseResult.bank_name` 的口径
//!
//! 参考仓从 sheet 名里「猜」银行名：第一个既不是 `sheet*`、不是跳过页、不是公共报文头、
//! 也不是交易码的 sheet 名。行为原样保留——猜不出来就是空串，由用户自己填。

pub mod code;
pub mod excel;
mod keywords;
pub mod store;
pub mod template;

use serde::{Deserialize, Serialize};

pub use code::{
    BANK_IMPL_TEMPLATE, BANK_TEMPLATE_SOURCE, CodeRequest, Generated, HOST_IMPL_TEMPLATE,
    HOST_TEMPLATE_SOURCE, Side,
};
pub use excel::{parse_excel, preview_headers};
pub use store::{
    Bank, BankMeta, Change, ChangeKind, FieldAttr, FieldEdit, FieldPatch, Store, Template, Version,
    diff_interfaces, diff_one_interface, diff_versions,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubFieldDef {
    pub field_name: String,
    pub esb_field_name: String,
    pub data_type: String,
    pub required: bool,
    pub meaning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub field_name: String,
    pub esb_field_name: String,
    pub data_type: String,
    pub required: bool,
    pub meaning: String,
    #[serde(default)]
    pub is_array: bool,
    #[serde(default)]
    pub array_items: Vec<SubFieldDef>,
}

/// 列映射：Excel 里的哪一列是什么（下标 0 起，参考仓同款）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColMapping {
    pub field_name_col: usize,
    pub esb_name_col: Option<usize>,
    pub data_type_col: Option<usize>,
    pub required_col: Option<usize>,
    pub meaning_col: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SheetHeaders {
    pub sheet_name: String,
    pub headers: Vec<String>,
    pub sample_rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceDef {
    pub function_id: String,
    pub description: String,
    pub interface_type: String,
    pub business_domain: String,
    pub request_fields: Vec<FieldDef>,
    pub response_fields: Vec<FieldDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcelParseResult {
    pub bank_name: String,
    pub interfaces: Vec<InterfaceDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageTemplate {
    pub function_id: String,
    pub request_xml: String,
    pub response_xml: String,
}

/// 稳定错误码 + 细节（整句由插件层拼）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// 打不开 Excel（路径不对 / 不是 xls(x) / 被占用）。
    Open(String),
    /// 一个 sheet 都没有。
    NoSheets,
    /// 单个 sheet 读失败（合并单元格坏了 / 加密）。
    SheetRead { sheet: String, detail: String },
    /// 工作表名不是交易码，且没给列映射。
    BadSheetName(String),
    /// 一个能解析的接口都没有。
    NoInterfaces,
    /// 要的那张表 / 那个接口不在结果里。
    NoSuchInterface(String),
    /// 模板渲染失败（模板语法不认识）。
    Template(String),
    /// 数据目录读不动（权限 / 被占用）。
    StoreRead(String),
    /// 数据目录写不动（磁盘满 / 只读）。
    StoreWrite(String),
    /// 数据文件不是合法 JSON（手工改坏了 / 上一版写了一半）。
    StoreParse(String),
    /// 银行代码不能当文件名用（空 / 带路径分隔符 / 太长）。
    BadBankCode(String),
    /// 模板名不合法（空 / 带路径分隔符）。
    BadTemplateName(String),
    /// 这家银行还没存过。
    NoSuchBank(String),
    /// 这个版本号不存在。
    NoSuchVersion(u64),
    /// 这个名字的模板不存在。
    NoSuchTemplate(String),
    /// 这家银行 / 这些接口里没有这个字段。
    NoSuchField {
        function_id: String,
        field_name: String,
    },
}

impl GatewayError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Open(_) => "open-failed",
            Self::NoSheets => "no-sheets",
            Self::SheetRead { .. } => "sheet-unreadable",
            Self::BadSheetName(_) => "bad-sheet-name",
            Self::NoInterfaces => "no-interfaces",
            Self::NoSuchInterface(_) => "no-such-interface",
            Self::Template(_) => "template-failed",
            Self::StoreRead(_) => "store-read-failed",
            Self::StoreWrite(_) => "store-write-failed",
            Self::StoreParse(_) => "store-corrupted",
            Self::BadBankCode(_) => "bad-bank-code",
            Self::BadTemplateName(_) => "bad-template-name",
            Self::NoSuchBank(_) => "no-such-bank",
            Self::NoSuchVersion(_) => "no-such-version",
            Self::NoSuchTemplate(_) => "no-such-template",
            Self::NoSuchField { .. } => "no-such-field",
        }
    }

    /// 细节是数据（路径 / sheet 名 / 解析器原话），不翻译。
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Open(detail)
            | Self::BadSheetName(detail)
            | Self::NoSuchInterface(detail)
            | Self::Template(detail)
            | Self::StoreRead(detail)
            | Self::StoreWrite(detail)
            | Self::StoreParse(detail)
            | Self::BadBankCode(detail)
            | Self::BadTemplateName(detail)
            | Self::NoSuchBank(detail)
            | Self::NoSuchTemplate(detail) => Some(detail.clone()),
            Self::NoSuchVersion(id) => Some(id.to_string()),
            Self::NoSuchField {
                function_id,
                field_name,
            } => Some(format!("{function_id}: {field_name}")),
            Self::SheetRead { sheet, detail } => Some(format!("{sheet}: {detail}")),
            Self::NoSheets | Self::NoInterfaces => None,
        }
    }
}

/// 报文骨架里的示例值（参考仓写死中文，这里由插件层按语言给）。
pub struct XmlSamples<'a> {
    /// 「示例流水号」。
    pub serial: &'a str,
    /// 「成功」。
    pub success: &'a str,
    /// 「示例值」。
    pub sample: &'a str,
}

/// 报文方向。**全族唯一一份**：`xml_template` 用它选字段，`store::Change` 用它标
/// 「这条变更是请求side还是应答side」——两处各定义一个同名枚举，迟早会把
/// 请求侧的变更记到应答侧去，而两边都能编译。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Request,
    Response,
}

/// 一个接口的请求 / 应答报文骨架。
///
/// 与参考仓一致：`<lcpt>` 根、`FunctionId`、`ExSerial`，应答侧多 `ErrorNo` / `ErrorInfo`；
/// 字段按声明顺序平铺（数组字段也平铺成单个元素，先拿去对字段，不表达重复）。
pub fn generate_message_template(
    iface: &InterfaceDef,
    samples: &XmlSamples<'_>,
) -> MessageTemplate {
    MessageTemplate {
        function_id: iface.function_id.clone(),
        request_xml: xml_template(iface, Direction::Request, samples),
        response_xml: xml_template(iface, Direction::Response, samples),
    }
}

pub fn xml_template(
    iface: &InterfaceDef,
    direction: Direction,
    samples: &XmlSamples<'_>,
) -> String {
    let fields = match direction {
        Direction::Request => &iface.request_fields,
        Direction::Response => &iface.response_fields,
    };
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"GBK\"?>\n<lcpt>\n");
    xml.push_str(&format!(
        "  <FunctionId>{}</FunctionId>\n",
        iface.function_id
    ));
    xml.push_str(&format!("  <ExSerial>{}</ExSerial>\n", samples.serial));
    if direction == Direction::Response {
        xml.push_str(&format!(
            "  <ErrorNo>0</ErrorNo>\n  <ErrorInfo>{}</ErrorInfo>\n",
            samples.success
        ));
    }
    for field in fields {
        let example = match field.data_type.as_str() {
            "number" | "int" | "integer" => "0",
            "amount" | "money" | "decimal" => "0.00",
            _ => samples.sample,
        };
        xml.push_str(&format!(
            "  <{}>{}</{}>\n",
            field.field_name, example, field.field_name
        ));
    }
    xml.push_str("</lcpt>");
    xml
}

/// 按交易码找接口（大小写敏感，与参考仓的 `function_id` 比较一致）。
pub fn find_interface<'a>(
    result: &'a ExcelParseResult,
    function_id: &str,
) -> Option<&'a InterfaceDef> {
    result
        .interfaces
        .iter()
        .find(|iface| iface.function_id == function_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface() -> InterfaceDef {
        InterfaceDef {
            function_id: "B100001".into(),
            description: "余额查询".into(),
            interface_type: "bank".into(),
            business_domain: String::new(),
            request_fields: vec![
                FieldDef {
                    field_name: "AccNo".into(),
                    esb_field_name: "accNo".into(),
                    data_type: "string".into(),
                    required: true,
                    meaning: "账号".into(),
                    is_array: false,
                    array_items: Vec::new(),
                },
                FieldDef {
                    field_name: "Amount".into(),
                    esb_field_name: "amount".into(),
                    data_type: "amount".into(),
                    required: false,
                    meaning: "金额".into(),
                    is_array: false,
                    array_items: Vec::new(),
                },
                FieldDef {
                    field_name: "Count".into(),
                    esb_field_name: "count".into(),
                    data_type: "number".into(),
                    required: false,
                    meaning: "笔数".into(),
                    is_array: false,
                    array_items: Vec::new(),
                },
            ],
            response_fields: vec![FieldDef {
                field_name: "ReturnCode".into(),
                esb_field_name: "returnCode".into(),
                data_type: "string".into(),
                required: true,
                meaning: "返回码".into(),
                is_array: false,
                array_items: Vec::new(),
            }],
        }
    }

    fn samples() -> XmlSamples<'static> {
        XmlSamples {
            serial: "示例流水号",
            success: "成功",
            sample: "示例值",
        }
    }

    #[test]
    fn request_template_has_every_field_and_typed_examples() {
        let xml = xml_template(&iface(), Direction::Request, &samples());
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"GBK\"?>\n<lcpt>\n"),
            "{xml}"
        );
        assert!(xml.contains("<FunctionId>B100001</FunctionId>"), "{xml}");
        assert!(xml.contains("<ExSerial>示例流水号</ExSerial>"), "{xml}");
        assert!(xml.contains("<AccNo>示例值</AccNo>"), "{xml}");
        assert!(xml.contains("<Amount>0.00</Amount>"), "{xml}");
        assert!(xml.contains("<Count>0</Count>"), "{xml}");
        assert!(xml.ends_with("</lcpt>"), "{xml}");
        assert!(!xml.contains("ErrorNo"), "{xml}");
    }

    #[test]
    fn response_template_adds_error_block() {
        let xml = xml_template(&iface(), Direction::Response, &samples());
        assert!(xml.contains("<ErrorNo>0</ErrorNo>"), "{xml}");
        assert!(xml.contains("<ErrorInfo>成功</ErrorInfo>"), "{xml}");
        assert!(xml.contains("<ReturnCode>示例值</ReturnCode>"), "{xml}");
        assert!(!xml.contains("<AccNo>"), "{xml}");
    }

    #[test]
    fn interface_lookup_is_exact() {
        let result = ExcelParseResult {
            bank_name: "某银行".into(),
            interfaces: vec![iface()],
        };
        assert!(find_interface(&result, "B100001").is_some());
        assert!(find_interface(&result, "b100001").is_none());
    }
}
