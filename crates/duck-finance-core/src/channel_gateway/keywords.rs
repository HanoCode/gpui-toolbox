//! 表头关键词表（逐字复刻参考仓 `channel_gateway/excel_parser.rs` 顶部的常量）。
//!
//! **这是数据，不是界面文案**：这些字符串拿去 `contains` 匹配银行 Excel 里的表头
//! （「字段含义」「是否必填」「ESB字段名」…），永不渲染到屏幕上，也不该被翻译——
//! 翻译了反而匹配不上真实文件。所以整份文件登记在 `duck-theme` 的
//! [`NON_UI_SOURCES`](../../../duck_theme/src/i18n.rs) 里，理由与
//! `duck-files-core/src/datagen_tables.rs` 相同。
//!
//! 顺序有意义：`contains` 匹配是**逐表**全扫，短词（「说明」）会命中长词
//! （「功能说明」）——参考仓就是这么写的，改顺序等于改解析结果。

/// 「交易名称 / 接口说明」这类**元信息**行（不是字段，用来补接口描述）。
pub(crate) const EXPECTED_DESC_HEADS: &[&str] = &[
    "交易名称",
    "交易描述",
    "功能名称",
    "功能说明",
    "接口名称",
    "接口说明",
    "业务名称",
    "业务描述",
    "服务名称",
    "服务描述",
    "说明",
];

/// 「字段含义」列。
pub(crate) const EXPECTED_MEANING_HEADS: &[&str] = &[
    "字段含义",
    "字段描述",
    "含义",
    "描述",
    "中文名称",
    "中文名",
    "字段说明",
    "说明",
    "备注",
    "具体描述",
];

/// 「内部字段名」列（Java 常量名的来源）。
pub(crate) const EXPECTED_FIELD_NAME_HEADS: &[&str] = &[
    "内部字段名",
    "内部字段",
    "字段名",
    "字段名称",
    "名称",
    "代码字段",
    "field_name",
    "field",
    "name",
    "属性名",
    "属性名称",
    "英文名称",
];

/// 「ESB 字段名」列（外部字段名 / 报文字段名 / 渠道字段名都是它）。
pub(crate) const EXPECTED_ESB_HEADS: &[&str] = &[
    "esb字段名",
    "esb字段",
    "esb字段名称",
    "esb_name",
    "esb",
    "外部字段名",
    "外部字段",
    "接口字段名",
    "接口字段",
    "渠道字段名",
    "渠道字段",
    "报文字段名",
    "报文字段",
    "转换字段名",
    "转换字段",
    "xml字段名",
    "xml字段",
];

pub(crate) const EXPECTED_TYPE_HEADS: &[&str] = &[
    "数据类型",
    "数据类别",
    "字段类型",
    "类型",
    "类别",
    "data_type",
    "datatype",
    "type",
];

pub(crate) const EXPECTED_REQUIRED_HEADS: &[&str] = &[
    "是否必填",
    "必填",
    "是否必须",
    "必须",
    "是否为空",
    "required",
    "mandatory",
    "是否可空",
    "填写标志",
    "是否必输",
];

/// 「请求字段」分区标记。
pub(crate) const EXPECTED_REQUEST_HEADS: &[&str] = &[
    "请求字段",
    "请求字段名",
    "请求报文",
    "request",
    "req",
    "请求要素",
    "输入",
    "输入区域",
    "输入区",
];

/// 「应答字段」分区标记。
pub(crate) const EXPECTED_RESPONSE_HEADS: &[&str] = &[
    "应答字段",
    "应答字段名",
    "响应字段",
    "应答报文",
    "response",
    "resp",
    "应答要素",
    "输出",
    "输出区域",
    "输出区",
];

/// 这些名字的行是**分区标题**（合并单元格），不是字段。
pub(crate) const SECTION_LABEL_KEYWORDS: &[&str] = &[
    "总行系统报文头",
    "系统报文头",
    "报文头",
    "公共请求头",
    "公共应答头",
    "公共报文头",
    "消息头",
];

/// 数据类型归一化时认成「数值」的词（命中后还会再判是否「金额」）。
pub(crate) const NUMERIC_TYPES: &[&str] = &[
    "number",
    "numeric",
    "decimal",
    "int",
    "integer",
    "float",
    "double",
    "金额",
    "数值",
    "数字",
    "money",
    "amount",
    "price",
    "价格",
    "bigdecimal",
    "long",
];

/// 这些 sheet 是「修订记录 / 目录 / 说明」之类的非接口页，整页跳过。
pub(crate) const SKIP_SHEET_KEYWORDS: &[&str] = &[
    "修订记录",
    "修订",
    "索引",
    "目录",
    "说明",
    "变更历史",
    "版本记录",
    "版本说明",
    "sheet",
];

/// 「公共报文头」页：里面有公共字段，但参考仓**只跳过**、不并进任何接口。
pub(crate) const COMMON_HEADER_KEYWORDS: &[&str] = &[
    "报文头",
    "公共头",
    "报文公共",
    "公共报文",
    "公共字段",
    "公共请求",
    "公共应答",
    "公共参数",
    "公共部分",
    "消息头",
    "消息头字段",
];

/// 数组分组的起止标记（`list` / `array` / `数组` 三种写法都认）。
pub(crate) const ARRAY_MARKER_NAMES: &[&str] = &["list", "array", "数组"];
pub(crate) const ARRAY_START_KEYWORDS: &[&str] = &["start", "begin"];
pub(crate) const ARRAY_END_KEYWORDS: &[&str] = &["end", "stop"];

/// 「数组」语义（用在「字段含义」列里）。`post_process_arrays` 与
/// `is_array_start_marker` 判的是同一件事，共用这一份。
pub(crate) const ARRAY_MEANING_WORDS: &[&str] = &["数组", "list"];

/// 这些字段名是**接口元信息**（交易名称 / 接口描述…），不是字段行。
pub(crate) const META_FIELD_KEYWORDS: &[&str] = &[
    "交易名称",
    "交易描述",
    "接口名称",
    "接口描述",
    "功能名称",
    "功能说明",
    "服务名称",
    "服务描述",
];

/// 表头前几行里「交易码」那一行的判据（取它的值当 `function_id`）。
pub(crate) const FUNCTION_ID_KEYWORDS: &[&str] = &["交易码", "交易代码", "功能号", "function"];

/// 分区标记：出现即切到「请求字段」。
pub(crate) const SECTION_REQUEST_WORDS: &[&str] = &["请求"];
/// 分区标记：出现即切到「应答字段」。
pub(crate) const SECTION_RESPONSE_WORDS: &[&str] = &["应答", "响应"];

/// `is_required` 里除 ASCII 真假值外，还认这些「必填」写法。
pub(crate) const REQUIRED_TRUE_WORDS: &[&str] = &["是", "必填", "必须", "不能为空"];

/// 数据类型归一化时再判一次「这是金额」（大小写已抹平）。
pub(crate) const AMOUNT_WORDS: &[&str] = &["金额", "amount", "money", "price", "价格"];

/// 生成 ESB 实现时判「这个字段是数值」的类型名表（源仓 `code_gen.rs::is_numeric`）。
///
/// ⚠️ 与上面的 [`NUMERIC_TYPES`] **不是同一张表，不要合并**：那张表拿 `contains`
/// 去猜**原始**类型串（`"VARCHAR2(32)"` 这种），这张表是对**已归一化**的
/// `data_type` 做精确相等（`normalize_data_type` 只会吐出 `string` / `number` /
/// `amount`）；合并会连带改掉生成出来的 Java 判断条件，产物就和参考仓不一样了。
pub(crate) const CODE_NUMERIC_TYPES: &[&str] = &[
    "number", "numeric", "decimal", "int", "integer", "float", "double", "金额", "数值", "数字",
];
