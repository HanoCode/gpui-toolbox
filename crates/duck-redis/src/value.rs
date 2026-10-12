//! 值模型：键的类型、已加载的值，以及 redis-rs 应答到本地模型的转换。
//!
//! 一条纪律（照抄 zedis 的核心取舍）：**所有元素都以原始字节承载**。
//! 界面显示的是解码后的内容，但写回服务端的永远是这些字节——
//! 一个 MessagePack 值编辑后仍要以 MessagePack 写回；不能识别的格式退到 hex 编辑。
//! 所以本模块只做「字节 ↔ 应答」的搬运，不做任何解码（解码属 UI 层，见设计文档 §4.6）。

use redis::Value;

/// 键的数据类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum KeyType {
    /// 字符串（`TYPE` 返回 `string`）。
    String,
    /// 列表。
    List,
    /// 集合。
    Set,
    /// 有序集合。
    ZSet,
    /// 哈希。
    Hash,
    /// 流。
    Stream,
    /// 键不存在（`TYPE` 返回 `none`）。
    #[default]
    Missing,
    /// 模块类型（`TYPE` 返回模块自定义名，如 `ReJSON-RL` / `TSDB-TYPE`）。
    Module,
}

impl KeyType {
    /// 全部类型（UI 的类型过滤器顺序）。
    pub const ALL: [Self; 8] = [
        Self::String,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Hash,
        Self::Stream,
        Self::Module,
        Self::Missing,
    ];

    /// 全部**可被 `SCAN TYPE` 过滤**的类型名（服务端只认这六个）。
    ///
    /// `Missing` 与 `Module` 不在其中：前者不是类型，后者服务端不接受这个名字过滤。
    pub const SCANNABLE: [Self; 6] = [
        Self::String,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Hash,
        Self::Stream,
    ];

    /// 由 `TYPE` 命令的应答文本解析。
    #[must_use]
    pub fn from_type_reply(reply: &str) -> Self {
        match reply {
            "string" => Self::String,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "hash" => Self::Hash,
            "stream" => Self::Stream,
            "none" => Self::Missing,
            // 其余一律当模块类型：RedisStack / RedisBloom / RediSearch 都从这条路上来。
            _ => Self::Module,
        }
    }

    /// `TYPE` / `SCAN TYPE` 用的服务端标识；`Missing` 与 `Module` 无对应值。
    #[must_use]
    pub const fn server_name(self) -> Option<&'static str> {
        match self {
            Self::String => Some("string"),
            Self::List => Some("list"),
            Self::Set => Some("set"),
            Self::ZSet => Some("zset"),
            Self::Hash => Some("hash"),
            Self::Stream => Some("stream"),
            Self::Missing | Self::Module => None,
        }
    }
}

/// 哈希字段 / 流字段的一对 `(field, value)`（都是原始字节，二进制安全）。
pub type FieldPair = (Vec<u8>, Vec<u8>);

/// 流条目：`(entry_id, fields)`。
pub type StreamEntry = (Vec<u8>, Vec<FieldPair>);

/// 一个已加载的键值。
///
/// 集合类都是**分页的一页**，不是全量——`HGETALL` 这类全量读取在大集合上会把
/// 服务端主线程拖死（设计文档 §4.6 的硬约束）。
#[derive(Debug, Clone, PartialEq)]
pub enum RedisValue {
    /// 字符串（原始字节；解码在 UI 层）。
    String(Vec<u8>),
    /// 哈希的一页 `(field, value)`。
    Hash(Vec<FieldPair>),
    /// 列表的一个窗口。
    List(Vec<Vec<u8>>),
    /// 集合的一页。
    Set(Vec<Vec<u8>>),
    /// 有序集合的一页 `(member, score)`。
    ZSet(Vec<(Vec<u8>, f64)>),
    /// 流的一页 `(id, fields)`。
    Stream(Vec<StreamEntry>),
    /// 键不存在。
    Missing,
}

impl RedisValue {
    /// 该值对应的类型。
    #[must_use]
    pub const fn key_type(&self) -> KeyType {
        match self {
            Self::String(_) => KeyType::String,
            Self::Hash(_) => KeyType::Hash,
            Self::List(_) => KeyType::List,
            Self::Set(_) => KeyType::Set,
            Self::ZSet(_) => KeyType::ZSet,
            Self::Stream(_) => KeyType::Stream,
            Self::Missing => KeyType::Missing,
        }
    }

    /// 元素条数（字符串按 1 计，缺失按 0 计）。
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::String(_) => 1,
            Self::Hash(entries) => entries.len(),
            Self::Stream(entries) => entries.len(),
            Self::List(items) | Self::Set(items) => items.len(),
            Self::ZSet(items) => items.len(),
            Self::Missing => 0,
        }
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 把一个应答值取成字节串；取不到时返回 `None`。
///
/// `Int` / `Double` / `Boolean` 都按 Redis 的文本惯例转写——服务端在
/// `RESP2` 下本来就把整数当字符串发，这样两种协议下得到的结果一致。
///
/// `BigNumber` 在**未开 `num-bigint` 特性**时载的就是 `Vec<u8>`（本 crate 正是如此，
/// 见根 `Cargo.toml` 的 `redis` 声明），直接透出字节即可。
#[must_use]
pub fn as_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(bytes) => Some(bytes.clone()),
        Value::BigNumber(bytes) => Some(bytes.clone()),
        Value::SimpleString(text) => Some(text.as_bytes().to_vec()),
        Value::VerbatimString { text, .. } => Some(text.as_bytes().to_vec()),
        Value::Int(number) => Some(number.to_string().into_bytes()),
        Value::Double(number) => Some(format_double(*number).into_bytes()),
        Value::Boolean(flag) => Some(if *flag { b"1".to_vec() } else { b"0".to_vec() }),
        Value::Okay => Some(b"OK".to_vec()),
        _ => None,
    }
}

/// 把一个应答值取成字符串（**有损**：坏字节按 `U+FFFD` 替换）。
///
/// 只用于协议层面的短文本（`TYPE` 应答、`INFO` 文本），**不用于键值内容**——
/// 键值一律走 [`as_bytes`] 保留原始字节。
#[must_use]
pub fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::SimpleString(text) | Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Okay => Some("OK".to_owned()),
        other => as_bytes(other).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    }
}

/// 把一个应答值取成整数。
#[must_use]
pub fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(number) => Some(*number),
        Value::BulkString(bytes) => String::from_utf8_lossy(bytes).trim().parse().ok(),
        Value::SimpleString(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 把一个应答值取成浮点数（`ZSCAN` 的分数走这条）。
#[must_use]
pub fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Double(number) => Some(*number),
        Value::Int(number) => Some(*number as f64),
        _ => as_bytes(value).and_then(|bytes| String::from_utf8_lossy(&bytes).trim().parse().ok()),
    }
}

/// 把一个应答值展开成数组。
///
/// `Array` 与 `Set` 都认（`RESP3` 下 `SMEMBERS` 之类的应答是 `Set`）。
#[must_use]
pub fn as_array(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) | Value::Set(items) => items.clone(),
        Value::Nil => Vec::new(),
        other => vec![other.clone()],
    }
}

/// 把一个应答值展开成字节串数组（丢掉取不出字节的元素）。
#[must_use]
pub fn as_byte_array(value: &Value) -> Vec<Vec<u8>> {
    as_array(value).iter().filter_map(as_bytes).collect()
}

/// 格式化浮点数：去掉无意义的小数尾巴，整数不带 `.0`。
///
/// `ZSCAN` 的分数经服务端往返后是文本，`1.0` 与 `1` 都合法；
/// 统一成「整数不带小数点」让界面稳定（也便于测试断言）。
#[must_use]
pub fn format_double(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// 把一条任意应答渲染成终端可读文本（redis-cli 的输出形态）。
///
/// 与 [`as_bytes`] 的取向不同：**这里要保留层级**。终端是排查工具，
/// 把 `[[a, b], [c, d]]` 压成 `a b c d` 会丢掉结构，用户就没法判断
/// 应答到底是不是自己期望的形状。所以数组按行列出并带序号、缩进表示嵌套。
#[must_use]
pub fn format_reply(value: &Value) -> String {
    let mut out = String::new();
    write_reply(value, 0, &mut out);
    out
}

/// [`format_reply`] 的递归主体。
fn write_reply(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Nil => out.push_str("(nil)"),
        Value::Array(items) | Value::Set(items) => {
            if items.is_empty() {
                out.push_str("(empty list)");
                return;
            }
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                    // 缩进用两个空格（终端里比制表符稳，不受制表位设置影响）。
                    // 只为「第二项起」加缩进：首项紧跟在父级 `N) ` 之后，
                    // 这样嵌套数组读起来与 redis-cli 一致。
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                }
                out.push_str(&(index + 1).to_string());
                out.push_str(") ");
                write_reply(item, depth + 1, out);
            }
        }
        Value::Map(pairs) => {
            for (index, (key, item)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                }
                write_reply(key, depth + 1, out);
                out.push_str(" => ");
                write_reply(item, depth + 1, out);
            }
        }
        other => match as_bytes(other) {
            Some(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
            // `Push` / `Attribute` 这类 RESP3 帧不在普通命令的应答里；
            // 真遇到了也如实说「显示不了」，不假装成空串。
            None => out.push_str("(unprintable reply)"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TYPE` 应答的解析：六个内建类型 + 缺失 + 模块（兜底）。
    #[test]
    fn key_type_parsing() {
        assert_eq!(KeyType::from_type_reply("string"), KeyType::String);
        assert_eq!(KeyType::from_type_reply("list"), KeyType::List);
        assert_eq!(KeyType::from_type_reply("set"), KeyType::Set);
        assert_eq!(KeyType::from_type_reply("zset"), KeyType::ZSet);
        assert_eq!(KeyType::from_type_reply("hash"), KeyType::Hash);
        assert_eq!(KeyType::from_type_reply("stream"), KeyType::Stream);
        assert_eq!(KeyType::from_type_reply("none"), KeyType::Missing);
        // 模块类型一律落到兜底档，不 panic、不丢键。
        assert_eq!(KeyType::from_type_reply("ReJSON-RL"), KeyType::Module);
        assert_eq!(KeyType::from_type_reply("TSDB-TYPE"), KeyType::Module);
        assert_eq!(KeyType::from_type_reply(""), KeyType::Module);
    }

    /// 可 `SCAN TYPE` 过滤的六个类型都有服务端名，另两个没有。
    #[test]
    fn scannable_types_have_server_names() {
        for key_type in KeyType::SCANNABLE {
            assert!(
                key_type.server_name().is_some(),
                "{key_type:?} 必须能被 SCAN TYPE 过滤"
            );
        }
        assert_eq!(KeyType::Missing.server_name(), None);
        assert_eq!(KeyType::Module.server_name(), None);
    }

    /// 字节转换：各态应答都能取到字节，且与 RESP 协议版本无关。
    #[test]
    fn bytes_conversion_covers_reply_shapes() {
        assert_eq!(
            as_bytes(&Value::BulkString(b"abc".to_vec())),
            Some(b"abc".to_vec())
        );
        assert_eq!(
            as_bytes(&Value::SimpleString("OK".into())),
            Some(b"OK".to_vec())
        );
        assert_eq!(as_bytes(&Value::Int(-42)), Some(b"-42".to_vec()));
        assert_eq!(as_bytes(&Value::Boolean(true)), Some(b"1".to_vec()));
        assert_eq!(as_bytes(&Value::Boolean(false)), Some(b"0".to_vec()));
        assert_eq!(as_bytes(&Value::Double(1.0)), Some(b"1".to_vec()));
        assert_eq!(as_bytes(&Value::Nil), None, "Nil 取不出字节");
    }

    /// 浮点格式化：整数不带小数点，非整数保留，科学计数法原样。
    #[test]
    fn double_formatting() {
        assert_eq!(format_double(1.0), "1");
        assert_eq!(format_double(-3.0), "-3");
        assert_eq!(format_double(1.5), "1.5");
        assert_eq!(format_double(0.0), "0");
        // 极大值不强行转 i64（会溢出/失真），保留原样。
        assert!(
            format_double(1e20).contains('e')
                || format_double(1e20).contains("100000000000000000000")
        );
    }

    /// 数组展开：Array / Set / 单体 / Nil 四种形态。
    #[test]
    fn array_flattening() {
        let array = Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Int(2)]);
        assert_eq!(as_byte_array(&array), vec![b"a".to_vec(), b"2".to_vec()]);

        let set = Value::Set(vec![Value::BulkString(b"x".to_vec())]);
        assert_eq!(as_byte_array(&set), vec![b"x".to_vec()]);

        // 单体展开成单元素数组（ZSCAN 的偶发形态）
        assert_eq!(as_byte_array(&Value::Int(7)), vec![b"7".to_vec()]);
        // Nil 展开成空
        assert!(as_byte_array(&Value::Nil).is_empty());
    }

    /// 整数与浮点解析容错（服务端在 RESP2 下把数字当字符串发）。
    #[test]
    fn numeric_parsing_tolerates_text() {
        assert_eq!(as_int(&Value::Int(5)), Some(5));
        assert_eq!(as_int(&Value::BulkString(b"5".to_vec())), Some(5));
        assert_eq!(as_int(&Value::BulkString(b"not-a-number".to_vec())), None);

        assert_eq!(as_f64(&Value::Double(2.5)), Some(2.5));
        assert_eq!(as_f64(&Value::Int(3)), Some(3.0));
        assert_eq!(as_f64(&Value::BulkString(b"2.5".to_vec())), Some(2.5));
    }

    /// 值模型的类型与计数。
    #[test]
    fn value_shape_helpers() {
        assert_eq!(RedisValue::String(b"x".to_vec()).len(), 1);
        assert_eq!(RedisValue::Missing.len(), 0);
        assert!(RedisValue::Missing.is_empty());
        assert_eq!(
            RedisValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]).key_type(),
            KeyType::Hash
        );
        assert_eq!(
            RedisValue::ZSet(vec![(b"m".to_vec(), 1.0)]).key_type(),
            KeyType::ZSet
        );
        assert_eq!(RedisValue::Missing.key_type(), KeyType::Missing);
    }

    /// 终端应答渲染：标量原样、数组带序号与缩进、空数组、`Nil`。
    ///
    /// 这条渲染是 redis-cli 终端的唯一输出通道 —— 结构丢了，用户就没法判断
    /// 应答形状对不对（例如误把二维数组读成一维）。
    #[test]
    fn reply_formatting_keeps_structure() {
        assert_eq!(format_reply(&Value::Int(7)), "7");
        assert_eq!(format_reply(&Value::SimpleString("OK".into())), "OK");
        assert_eq!(format_reply(&Value::Nil), "(nil)");
        assert_eq!(format_reply(&Value::Array(Vec::new())), "(empty list)");

        let nested = Value::Array(vec![
            Value::Array(vec![
                Value::BulkString(b"k1".to_vec()),
                Value::BulkString(b"v1".to_vec()),
            ]),
            Value::BulkString(b"tail".to_vec()),
        ]);
        assert_eq!(format_reply(&nested), "1) 1) k1\n  2) v1\n2) tail");

        let map = Value::Map(vec![(Value::BulkString(b"a".to_vec()), Value::Int(1))]);
        assert_eq!(format_reply(&map), "a => 1");
    }
}
