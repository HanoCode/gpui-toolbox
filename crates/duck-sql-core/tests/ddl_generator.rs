//! `ddl_generator` 的行为锁。
//!
//! 这些断言不是「我设计的规格」，而是参考仓实现**当前行为**的记录：分表名规则、
//! 列名过滤、占位符大小写不敏感、VIEW 的列清单，都是生产里已经这样跑的形态。
//! 移植后它们必须仍然成立——成立就说明逐字复刻成功。

use duck_sql_core::ddl_generator::{
    extract_columns, extract_table_name, format_ddls, generate_sharded_ddls,
    generate_ta_sharded_ddls, generate_union_views, replace_sql_placeholders,
    replace_sql_placeholders_with_patterns,
};

const DDL: &str = "CREATE TABLE `fund_trans` (\n  `id` bigint(20) NOT NULL AUTO_INCREMENT,\n  `ta_code` varchar(10) DEFAULT NULL,\n  `amount` decimal(16,2) DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;";

#[test]
fn extracts_the_table_name() {
    assert_eq!(extract_table_name(DDL).as_deref(), Some("fund_trans"));
    assert_eq!(
        extract_table_name("create table if not exists TA_DATA (id int);").as_deref(),
        Some("TA_DATA")
    );
    assert_eq!(extract_table_name("SELECT 1"), None);
}

#[test]
fn extracts_columns_without_constraints() {
    assert_eq!(extract_columns(DDL), ["id", "ta_code", "amount"]);
    assert!(extract_columns("CREATE TABLE x ()").is_empty());
}

#[test]
fn sharded_tables_append_the_index() {
    let ddls = generate_sharded_ddls(DDL, 2);
    assert_eq!(ddls.len(), 2);
    assert!(
        ddls[0].contains("CREATE TABLE `fund_trans1`"),
        "{}",
        ddls[0]
    );
    assert!(
        ddls[1].contains("CREATE TABLE `fund_trans2`"),
        "{}",
        ddls[1]
    );
}

/// 表名已带 `_数字` 结尾时**替换**而不是追加（否则 `tb_0` 会变成 `tb_01`）。
#[test]
fn sharded_tables_replace_a_trailing_index() {
    let ddls = generate_sharded_ddls("CREATE TABLE tb_0 (id int);", 3);
    assert!(ddls[0].contains("tb_1"), "{}", ddls[0]);
    assert!(ddls[2].contains("tb_3"), "{}", ddls[2]);
}

#[test]
fn ta_sharded_prefixes_the_ta_code() {
    let ddls = generate_ta_sharded_ddls(DDL, "Y3", 2);
    assert!(ddls[0].contains("fund_transY31"), "{}", ddls[0]);
    assert!(ddls[1].contains("fund_transY32"), "{}", ddls[1]);
}

/// 源仓验证过的用例：`tbaccreqextendexp${TACODE}_0` + TA=Y3 → `tbaccreqextendexpY3_0`。
#[test]
fn placeholders_are_case_insensitive() {
    assert_eq!(
        replace_sql_placeholders("tbaccreqextendexp${TACODE}_0", 1, "Y3"),
        "tbaccreqextendexpY3_0"
    );
    assert_eq!(
        replace_sql_placeholders("t_${num} /* ${ta_code} */", 7, "Y3"),
        "t_7 /* Y3 */"
    );
}

#[test]
fn custom_placeholders_win_over_the_defaults() {
    assert_eq!(
        replace_sql_placeholders_with_patterns("a #N# b", 7, "Y3", Some("#N#"), None),
        "a 7 b"
    );
    assert_eq!(
        replace_sql_placeholders_with_patterns("a @TA@ b", 1, "Y3", None, Some("@TA@")),
        "a Y3 b"
    );
    // 空白串等于「没给」，回落默认匹配。
    assert_eq!(
        replace_sql_placeholders_with_patterns("t_${NUM}", 2, "Y3", Some("  "), None),
        "t_2"
    );
}

#[test]
fn union_view_lists_the_columns() {
    let views = generate_union_views(DDL, 2, false);
    assert_eq!(views.len(), 1);
    let view = &views[0];
    assert!(view.contains("CREATE VIEW view_fund_trans AS"), "{view}");
    assert!(
        view.contains("SELECT id, ta_code, amount FROM fund_trans1"),
        "{view}"
    );
    assert!(view.contains("UNION ALL"), "{view}");
}

#[test]
fn union_view_can_tag_the_shard_index() {
    let views = generate_union_views(DDL, 2, true);
    assert!(views[0].contains("1 AS table_seq"), "{}", views[0]);
    assert!(views[0].contains("2 AS table_seq"), "{}", views[0]);
}

#[test]
fn ddls_are_joined_by_a_blank_line() {
    assert_eq!(format_ddls(&["a".into(), "b".into()]), "a\n\nb");
}
