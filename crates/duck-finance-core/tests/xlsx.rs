//! `parse_excel` / `preview_headers` 的端到端夹具：真读一份 xlsx。
//!
//! 夹具 `fixtures/interfaces.xlsx` 是最小化的 xlsx（内联字符串，无共享串表、无样式），
//! 结构与银行给的清单一致：银行名 sheet + 交易码 sheet（`B100001`）+ 一张「修订记录」
//! 干扰页。用最小结构而不是「另存为」的文件，是为了让夹具可读、可 diff——
//! 里面只有三张表的行列数据，没有 `docProps` / 主题 / 样式的噪音。

use duck_finance_core::channel_gateway::{parse_excel, preview_headers};

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/interfaces.xlsx",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn reads_bank_name_interfaces_and_array_fields() {
    let result = parse_excel(&fixture(), "right", None).expect("夹具应当能解析");

    // 银行名从「非交易码 sheet」里猜：第一张 `渠道接口清单` 就是它。
    assert_eq!(result.bank_name, "渠道接口清单");
    assert_eq!(
        result.interfaces.len(),
        2,
        "两张交易码页 + 修订记录页不该产生接口"
    );

    let iface = result
        .interfaces
        .iter()
        .find(|iface| iface.function_id == "B100001")
        .expect("应当有 B100001");
    assert_eq!(iface.function_id, "B100001");
    // 请求区：两个普通字段 + 一个数组字段（`list` 起止标记被折叠）。
    assert_eq!(iface.request_fields.len(), 3, "{:?}", iface.request_fields);
    assert_eq!(iface.request_fields[0].field_name, "SerialNo");
    assert_eq!(iface.request_fields[0].meaning, "流水号");
    assert!(iface.request_fields[0].required);
    assert_eq!(iface.request_fields[1].data_type, "amount");
    assert_eq!(iface.request_fields[2].field_name, "dataList");
    assert!(iface.request_fields[2].is_array);
    assert_eq!(iface.request_fields[2].array_items.len(), 1);
    assert_eq!(iface.request_fields[2].array_items[0].field_name, "ItemNo");

    // 应答区：`应答字段` 分区标记之后的两行。
    assert_eq!(
        iface.response_fields.len(),
        2,
        "{:?}",
        iface.response_fields
    );
    assert_eq!(iface.response_fields[0].field_name, "ReturnCode");
    assert_eq!(iface.response_fields[1].data_type, "amount");
}

#[test]
fn mapped_mode_reads_the_same_workbook_by_sheet_name() {
    // 映射模式下 sheet 名就是交易码，不做 `is_function_id` 校验。
    // 夹具的双组列布局：内部组在 0~3，ESB 字段名在第 4 列。
    let mapping = duck_finance_core::channel_gateway::ColMapping {
        field_name_col: 0,
        esb_name_col: Some(4),
        data_type_col: Some(1),
        required_col: Some(2),
        meaning_col: Some(3),
    };
    let result = parse_excel(&fixture(), "right", Some(&mapping)).expect("映射模式应当能解析");
    let iface = result
        .interfaces
        .iter()
        .find(|iface| iface.function_id == "B100001")
        .expect("应当有 B100001");
    assert_eq!(iface.request_fields[0].esb_field_name, "serialNo");
}

#[test]
fn preview_headers_lists_columns_and_sample_rows() {
    let sheets = preview_headers(&fixture()).expect("夹具应当能读列头");
    // `修订记录` 被跳过，`渠道接口清单` 留着（列头识别不出来时给第一行）。
    let ids: Vec<&str> = sheets
        .iter()
        .map(|sheet| sheet.sheet_name.as_str())
        .collect();
    assert!(ids.contains(&"B100001"), "{ids:?}");
    assert!(!ids.contains(&"修订记录"), "{ids:?}");

    let sheet = sheets
        .iter()
        .find(|sheet| sheet.sheet_name == "B100001")
        .expect("应当有 B100001");
    assert_eq!(sheet.headers[0], "内部字段名");
    assert_eq!(sheet.headers[4], "ESB字段名");
    assert_eq!(sheet.headers[7], "字段含义");
    // 样例行只收数据行：分区标题（请求字段 / 应答字段）应被剔除。
    assert!(
        sheet
            .sample_rows
            .iter()
            .all(|row| !row[0].contains("请求字段") && !row[0].contains("应答字段")),
        "{:?}",
        sheet.sample_rows
    );
    assert!(
        sheet.sample_rows.iter().any(|row| row[0] == "serialNo"),
        "{:?}",
        sheet.sample_rows
    );
}

#[test]
fn a_missing_file_is_reported_as_open_failed() {
    let error = parse_excel("definitely-not-here.xlsx", "right", None).unwrap_err();
    assert_eq!(error.code(), "open-failed");
    assert!(error.detail().is_some());
}
