//! 渠道网关 Java 实现生成（复刻参考仓 `channel_gateway/code_gen.rs`）。
//!
//! 参考仓用 `tera` 渲染两份模板（`templates/BankImpl.java.tera` /
//! `HostImpl.java.tera`，逐字节复制在本目录下）。本仓不引 `tera`：模板只用到
//! `{{ 变量 }}` / `{% if %}` / `{% for %}` 三种语法，[`super::template`] 用 200 行
//! 覆盖了它们，产物与 tera 逐字节一致（标签不产生输出、周围空白原样保留）。
//!
//! 上下文与参考仓 `generate_bank_impl` / `generate_host_impl` 一字不差：
//!
//! - `package_base` 已经拼上银行代码（`{package_base}.{bank_code}`）；
//! - `bank_code` 自己**不在**上下文里（模板只用到拼好的包名，参考仓虽然 `insert` 了
//!   `bank_code`，但两份模板都没有引用它 —— 这里不 insert，免得下一个维护者以为有用）；
//! - `business_domain` 为空时回落到 `description`；
//! - 字段对象带 `is_numeric` / `is_amount`：内置模板不用，留给「自定义模板」那条路
//!   （本仓未迁，见 crate 文档），保留是为了上下文形状与参考仓一致。

use std::collections::BTreeMap;

use crate::channel_gateway::keywords::{AMOUNT_WORDS, CODE_NUMERIC_TYPES};
use crate::channel_gateway::template::{Value, render};
use crate::channel_gateway::{FieldDef, GatewayError, InterfaceDef};

/// 银行侧（渠道 → 主机）：`BankComm` 实现。
pub const BANK_IMPL_TEMPLATE: &str = "BankImpl.java.tera";
/// 主机侧（主机 → ESB）：`TcpComm` 实现。
pub const HOST_IMPL_TEMPLATE: &str = "HostImpl.java.tera";

/// 银行侧模板源（编译期从 `templates/` 读入；模板管理里「内置」那两条也用它）。
pub const BANK_TEMPLATE_SOURCE: &str = include_str!("templates/BankImpl.java.tera");
/// 主机侧模板源。
pub const HOST_TEMPLATE_SOURCE: &str = include_str!("templates/HostImpl.java.tera");

/// 生成哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bank,
    Host,
}

impl Side {
    /// `bank` / `host`（别的值按 bank，与参考仓 `side.as_deref().unwrap_or("bank")` 一致）。
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "host" => Self::Host,
            _ => Self::Bank,
        }
    }

    pub fn prefix(self) -> &'static str {
        match self {
            Self::Bank => "Bank",
            Self::Host => "Host",
        }
    }

    pub fn template_name(self) -> &'static str {
        match self {
            Self::Bank => BANK_IMPL_TEMPLATE,
            Self::Host => HOST_IMPL_TEMPLATE,
        }
    }
}

/// 生成结果：文件名与内容（参考仓 `GenerateResult`，路径部分由插件层决定）。
pub struct Generated {
    pub file_name: String,
    pub content: String,
}

/// 一个接口 + 生成参数。
pub struct CodeRequest<'a> {
    pub bank_code: &'a str,
    pub package_base: &'a str,
    pub side: Side,
    pub iface: &'a InterfaceDef,
    /// 自定义模板源（`Some` = 用它渲染，`None` = 内置那两份）。
    ///
    /// 来自 [`super::store::Store::template`]。参考仓有这条通路
    /// （`ChannelGatewayDb::render_template`），但**它的界面上从没调用过**——
    /// `cg_render_template` 注册了命令、前端零处 `invoke`，等于死代码。
    /// 这里把它接上：模板管理里存的模板能真的拿来生成。
    pub template: Option<&'a str>,
}

pub fn generate(request: &CodeRequest<'_>) -> Result<Generated, GatewayError> {
    let package = format!("{}.{}", request.package_base, request.bank_code);
    let description = request.iface.description.clone();
    let business_domain = if request.iface.business_domain.is_empty() {
        description.clone()
    } else {
        request.iface.business_domain.clone()
    };

    let mut context = BTreeMap::new();
    context.insert("package_base".to_string(), Value::Text(package));
    context.insert(
        "bank_code".to_string(),
        Value::Text(request.bank_code.to_string()),
    );
    context.insert(
        "function_id".to_string(),
        Value::Text(request.iface.function_id.clone()),
    );
    context.insert("description".to_string(), Value::Text(description));
    context.insert("business_domain".to_string(), Value::Text(business_domain));
    context.insert(
        "abstract_bank_comm_pkg".to_string(),
        Value::Text(format!(
            "com.hundsun.lcpt.gateway.{}.bank",
            request.bank_code
        )),
    );
    context.insert(
        "ibank_comm_pkg".to_string(),
        Value::Text("com.hundsun.lcpt.gateway.bank".to_string()),
    );
    context.insert(
        "request_fields".to_string(),
        fields_value(&request.iface.request_fields),
    );
    context.insert(
        "response_fields".to_string(),
        fields_value(&request.iface.response_fields),
    );

    // 自定义模板用**同一套上下文**（而不是参考仓 `render_template` 那套精简版）：
    // 那边少了 `ibank_comm_pkg` / `is_array` / `array_items`，一份照内置模板改出来的
    // 自定义模板会立刻在 `{{ ibank_comm_pkg }}` 上报「变量没有定义」。
    // 多给的键对不引用它的模板毫无影响，所以「取两个上下文的并集」是安全的。
    let source = match request.template {
        Some(custom) => custom,
        None => match request.side {
            Side::Bank => BANK_TEMPLATE_SOURCE,
            Side::Host => HOST_TEMPLATE_SOURCE,
        },
    };
    let content = render(source, &Value::Object(context)).map_err(GatewayError::Template)?;

    Ok(Generated {
        file_name: format!(
            "{}{}.java",
            request.side.prefix(),
            request.iface.function_id
        ),
        content,
    })
}

fn fields_value(fields: &[FieldDef]) -> Value {
    Value::List(fields.iter().map(field_value).collect())
}

fn field_value(field: &FieldDef) -> Value {
    let mut map = BTreeMap::new();
    map.insert(
        "java_const".to_string(),
        Value::Text(field.field_name.clone()),
    );
    map.insert(
        "esb_name".to_string(),
        Value::Text(field.esb_field_name.clone()),
    );
    map.insert("meaning".to_string(), Value::Text(field.meaning.clone()));
    map.insert(
        "is_numeric".to_string(),
        Value::Bool(is_numeric(&field.data_type)),
    );
    map.insert(
        "is_amount".to_string(),
        Value::Bool(is_amount(&field.data_type)),
    );
    map.insert("is_array".to_string(), Value::Bool(field.is_array));
    map.insert(
        "array_items".to_string(),
        Value::List(
            field
                .array_items
                .iter()
                .map(|sub| {
                    let mut sub_map = BTreeMap::new();
                    sub_map.insert(
                        "java_const".to_string(),
                        Value::Text(sub.field_name.clone()),
                    );
                    sub_map.insert(
                        "esb_name".to_string(),
                        Value::Text(sub.esb_field_name.clone()),
                    );
                    sub_map.insert("meaning".to_string(), Value::Text(sub.meaning.clone()));
                    // 参考仓对子字段恒给 false（不再细分类型）。
                    sub_map.insert("is_numeric".to_string(), Value::Bool(false));
                    sub_map.insert("is_amount".to_string(), Value::Bool(false));
                    sub_map.insert("is_array".to_string(), Value::Bool(false));
                    sub_map.insert("array_items".to_string(), Value::List(Vec::new()));
                    Value::Object(sub_map)
                })
                .collect(),
        ),
    );
    Value::Object(map)
}

fn is_numeric(data_type: &str) -> bool {
    CODE_NUMERIC_TYPES.contains(&data_type)
}

fn is_amount(data_type: &str) -> bool {
    AMOUNT_WORDS.contains(&data_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_gateway::SubFieldDef;

    fn iface() -> InterfaceDef {
        InterfaceDef {
            function_id: "B100001".into(),
            description: "账户余额查询".into(),
            interface_type: "bank".into(),
            business_domain: String::new(),
            request_fields: vec![
                FieldDef {
                    field_name: "SerialNo".into(),
                    esb_field_name: "serialNo".into(),
                    data_type: "string".into(),
                    required: true,
                    meaning: "流水号".into(),
                    is_array: false,
                    array_items: Vec::new(),
                },
                FieldDef {
                    field_name: "DataList".into(),
                    esb_field_name: String::new(),
                    data_type: "array".into(),
                    required: false,
                    meaning: "明细数组".into(),
                    is_array: true,
                    array_items: vec![SubFieldDef {
                        field_name: "ItemNo".into(),
                        esb_field_name: "itemNo".into(),
                        data_type: "string".into(),
                        required: true,
                        meaning: "序号".into(),
                    }],
                },
            ],
            response_fields: vec![FieldDef {
                field_name: "Balance".into(),
                esb_field_name: "balance".into(),
                data_type: "amount".into(),
                required: false,
                meaning: "余额".into(),
                is_array: false,
                array_items: Vec::new(),
            }],
        }
    }

    #[test]
    fn bank_side_renders_fields_and_arrays() {
        let iface = iface();
        let generated = generate(&CodeRequest {
            bank_code: "hzcb",
            package_base: "com.example.gateway",
            side: Side::Bank,
            iface: &iface,
            template: None,
        })
        .expect("应当能渲染");

        assert_eq!(generated.file_name, "BankB100001.java");
        let code = generated.content;
        // 包名、类名、注解
        assert!(
            code.contains("package com.example.gateway.hzcb.bank.impl.B100001;"),
            "{code}"
        );
        assert!(code.contains("@Service(\"BankB100001\")"), "{code}");
        assert!(
            code.contains("public class BankB100001 extends AbstractBankComm"),
            "{code}"
        );
        // 普通字段与数组字段
        assert!(
            code.contains("requestJson.put(ITag.SerialNo, XmlUtil.getDataFromTag"),
            "{code}"
        );
        assert!(
            code.contains("JSONArray DataListArray = new JSONArray();"),
            "{code}"
        );
        assert!(code.contains("obj.put(ITag.ItemNo,"), "{code}");
        // 空行与缩进按模板原样（`{% if %}` 独占一行 ⇒ 留空行）
        assert!(code.contains("\n\n"), "{code}");
        // 模板标签必须一个不剩
        assert!(!code.contains("{%"), "{code}");
        assert!(!code.contains("{{"), "{code}");
    }

    #[test]
    fn host_side_uses_esb_tags_and_byte_output() {
        let iface = iface();
        let generated = generate(&CodeRequest {
            bank_code: "hzcb",
            package_base: "com.example.gateway",
            side: Side::Host,
            iface: &iface,
            template: None,
        })
        .expect("应当能渲染");
        let code = generated.content;
        assert_eq!(generated.file_name, "HostB100001.java");
        assert!(code.contains("new XmlTag(EsbTag.serialNo)"), "{code}");
        assert!(code.contains("public byte[] packRequest"), "{code}");
        assert!(code.contains("return \"6.0.0.1\";"), "{code}");
        assert!(!code.contains("{%"), "{code}");
    }

    #[test]
    fn empty_business_domain_falls_back_to_description() {
        let iface = iface();
        let generated = generate(&CodeRequest {
            bank_code: "hzcb",
            package_base: "com.example.gateway",
            side: Side::Bank,
            iface: &iface,
            template: None,
        })
        .expect("应当能渲染");
        assert!(
            generated.content.contains("账户余额查询"),
            "{}",
            generated.content
        );
    }

    #[test]
    fn side_parsing_defaults_to_bank() {
        assert_eq!(Side::parse("host"), Side::Host);
        assert_eq!(Side::parse(""), Side::Bank);
        assert_eq!(Side::parse("bank"), Side::Bank);
        assert_eq!(Side::parse("whatever"), Side::Bank);
    }
}
