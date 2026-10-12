//! 服务端观测面板的数据模型与应答解析（`docs/redis-design.md` §4.7，M1.4）。
//!
//! 本模块只做两件事：**把应答解析成本地模型**（纯函数，可普通 `#[test]` 覆盖），
//! 以及**描述本连接的服务端能力**（[`ServerFeatures`]）。所有网络调用都在
//! [`crate::conn`] 的 `RedisConnection` 上，本模块不开连接、不依赖 gpui。
//!
//! # 一条通则：缺什么命令就灰显并说明原因
//!
//! 面板与按钮是否可用**只看 [`ServerFeatures`]**，不做云厂商 / 代理品牌的特判
//! （设计文档 §4.7 的硬口径）。探测不出来时一律按「可尝试」处理——把真实错误
//! 摆到用户面前，比我们自己猜一个「大概不支持」更有用。

use std::collections::HashMap;

use redis::Value;

use crate::conn::pair_up;
use crate::value::{as_array, as_bytes, as_int};

/// 连接后一次性探测的命令清单。
///
/// 只列**面板会用到的、且在各版本 / 各发行版间存在性差异大**的命令。
/// 不列 `GET` / `SCAN` 这类人人都有、永远可用的基础命令——它们不进探测表，
/// [`ServerFeatures::supports`] 对查不到的名字返回「可尝试」。
pub const PROBE_COMMANDS: &[&str] = &[
    "acl", "client", "cluster", "command", "config", "function", "hotkeys", "latency", "memory",
    "module", "monitor", "object", "pubsub", "script", "sentinel", "slowlog", "xinfo",
];

/// 一条慢日志（`SLOWLOG GET`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowlogEntry {
    id: u64,
    timestamp: i64,
    micros: u64,
    args: Vec<String>,
    client: String,
    name: String,
}

impl SlowlogEntry {
    /// 慢日志序号（服务端自增，重启后归零）。
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 发生时刻的 Unix 时间戳（秒）。
    #[must_use]
    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    /// 耗时（微秒）——服务端 `SLOWLOG` 的原始单位。
    #[must_use]
    pub fn micros(&self) -> u64 {
        self.micros
    }

    /// 耗时的毫秒文本（保留三位小数，避免「1.2 毫秒」这种粗糙口径）。
    #[must_use]
    pub fn duration_ms(&self) -> String {
        format!("{:.3}", self.micros as f64 / 1000.0)
    }

    /// 命令行（已按参数拆开，展示时再拼接）。
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// 命令行拼成一个字符串（参数间用空格）。
    #[must_use]
    pub fn command_line(&self) -> String {
        self.args.join(" ")
    }

    /// 发起该命令的客户端地址（`SLOWLOG GET` 4 元以上的应答才有）。
    #[must_use]
    pub fn client(&self) -> &str {
        &self.client
    }

    /// 发起该命令的连接名（旧服务端没有这一列，为空串）。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// 一条客户端连接（`CLIENT LIST` 的一行）。
///
/// 字段名与 `CLIENT LIST` 的键名一一对应；缺列一律回落空串 / `-1`，
/// **不猜**（不同版本 / 不同发行版的列不一样）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    id: String,
    addr: String,
    name: String,
    user: String,
    db: i64,
    age: i64,
    idle: i64,
    cmd: String,
    flags: String,
}

impl ClientInfo {
    /// 客户端 id（`CLIENT KILL ID` 用）。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 对端地址（`CLIENT KILL ADDR` 用，也是这里的主要标识）。
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 连接名（`CLIENT SETNAME` 设的，多为空）。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// ACL 用户名。
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// 所在逻辑库。
    #[must_use]
    pub fn db(&self) -> i64 {
        self.db
    }

    /// 连接存活秒数。
    #[must_use]
    pub fn age(&self) -> i64 {
        self.age
    }

    /// 空闲秒数（判断「僵尸连接」的主要依据）。
    #[must_use]
    pub fn idle(&self) -> i64 {
        self.idle
    }

    /// 最近一条命令。
    #[must_use]
    pub fn cmd(&self) -> &str {
        &self.cmd
    }

    /// 连接标志（`N` 普通 / `S` 从库 / `O` 被 MONITOR 独占 …）。
    #[must_use]
    pub fn flags(&self) -> &str {
        &self.flags
    }
}

/// 一条服务端配置项（`CONFIG GET`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    name: String,
    value: String,
}

impl ConfigEntry {
    /// 构造（解析之外也用得上：把局部改动合并进列表时）。
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// 参数名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 参数值（**原样文本**：服务端配置本来就是字符串，不在这里做类型转换）。
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// 本连接的服务端能力快照。
///
/// 连接成功后跑一次只读探测（`INFO server` + `COMMAND INFO …` + `MODULE LIST`）得到；
/// 之后界面上「面板灰显 + 说明原因」全部以它为准。
///
/// **探测失败时 [`Self::probed`] 为假，[`Self::supports`] 一律返回 `true`**——
/// 探测失败（老服务端没有 `COMMAND INFO`、当前用户无权执行）不该变成「功能全禁」，
/// 真正不支持时那条命令自己会报错，界面把错误摆出来即可。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerFeatures {
    version: Option<String>,
    /// 小写命令名（便于大小写无关查询）。
    commands: Vec<String>,
    modules: Vec<String>,
    probed: bool,
}

impl ServerFeatures {
    /// 探测成功后的构造。
    #[must_use]
    pub fn new(version: Option<String>, commands: Vec<String>, modules: Vec<String>) -> Self {
        Self {
            version,
            commands,
            modules,
            probed: true,
        }
    }

    /// 探测失败 / 尚未探测：不隐藏任何能力。
    #[must_use]
    pub fn unprobed(version: Option<String>) -> Self {
        Self {
            version,
            commands: Vec::new(),
            modules: Vec::new(),
            probed: false,
        }
    }

    /// 是否真的做过探测。
    #[must_use]
    pub fn probed(&self) -> bool {
        self.probed
    }

    /// 服务端版本描述（`INFO server` 的 `redis_version` / `valkey_version`）。
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// 已加载的模块名（`MODULE LIST`）。
    #[must_use]
    pub fn modules(&self) -> &[String] {
        &self.modules
    }

    /// 服务端是否有这条命令（大小写无关）。
    ///
    /// 未探测 / 探测失败 → 返回 `true`（见类型级文档的取舍）。
    #[must_use]
    pub fn supports(&self, command: &str) -> bool {
        if !self.probed {
            return true;
        }
        let command = command.to_ascii_lowercase();
        self.commands.iter().any(|known| known == &command)
    }

    /// 服务端版本是否不低于 `major.minor`。
    ///
    /// 版本取不到（老服务端 / 非标准发行版）→ 返回 `true`，同样不隐藏能力。
    #[must_use]
    pub fn version_at_least(&self, major: u32, minor: u32) -> bool {
        match self.version.as_deref().and_then(version_tuple) {
            Some(actual) => actual >= (major, minor),
            None => true,
        }
    }
}

/// 把 `7.2.4` / `7.2` 解析成 `(7, 2)`；解析不出返回 `None`。
fn version_tuple(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// 把应答配成 `(键, 值)` 对。
///
/// 三种形态都要认：
/// - `RESP2` 的平铺数组（`CONFIG GET` / `MEMORY STATS`）；
/// - `RESP3` 的 `Map`（同样是 `CONFIG GET`）；
/// - `MODULE LIST` 那种「数组套数组」的条目（由调用方逐个喂进来）。
#[must_use]
pub fn as_pairs(value: &Value) -> Vec<(Vec<u8>, Vec<u8>)> {
    if let Value::Map(items) = value {
        return items
            .iter()
            .filter_map(|(key, value)| Some((as_bytes(key)?, as_bytes(value)?)))
            .collect();
    }
    pair_up(crate::value::as_byte_array(value))
}

/// 解析 `SLOWLOG GET` 的应答。
///
/// 条目形如 `[id, timestamp, micros, [args…], client, name]`；**4 元以上的尾巴
/// 都按可选处理**——Redis 6 之前没有 client / name 两列，缺了就当空串。
/// 结构不完整的条目直接跳过（宁可少一条，不要显示一条假的）。
#[must_use]
pub fn parse_slowlog(reply: &Value) -> Vec<SlowlogEntry> {
    let mut entries = Vec::new();
    for item in as_array(reply) {
        let parts = as_array(&item);
        if parts.len() < 4 {
            continue;
        }
        let id = as_int(&parts[0]).unwrap_or(0).max(0).unsigned_abs();
        let timestamp = as_int(&parts[1]).unwrap_or(0);
        let micros = as_int(&parts[2]).unwrap_or(0).max(0).unsigned_abs();
        let args = as_array(&parts[3])
            .iter()
            .filter_map(as_bytes)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .collect();
        let client = text_at(&parts, 4);
        let name = text_at(&parts, 5);
        entries.push(SlowlogEntry {
            id,
            timestamp,
            micros,
            args,
            client,
            name,
        });
    }
    entries
}

/// 取应答数组里第 `index` 项的有损文本；缺项 / 取不出字节返回空串。
fn text_at(parts: &[Value], index: usize) -> String {
    parts
        .get(index)
        .and_then(as_bytes)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// 解析 `CLIENT LIST` 的原文。
///
/// 每行是空格分隔的 `键=值` 序列（**值里不会再出现空格**，这是服务端保证的）；
/// 空行跳过，缺列回落默认值。
#[must_use]
pub fn parse_client_list(text: &str) -> Vec<ClientInfo> {
    let mut clients = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields: HashMap<&str, &str> = HashMap::new();
        for token in line.split_ascii_whitespace() {
            if let Some((key, value)) = token.split_once('=') {
                fields.insert(key, value);
            }
        }
        // 没有 `id` 也没有 `addr` 的行不是客户端行（服务端在 `CLIENT LIST` 里
        // 不会发别的，但代理 / 网关可能夹带一行说明文本）。
        if !fields.contains_key("id") && !fields.contains_key("addr") {
            continue;
        }
        let get = |key: &str| fields.get(key).copied().unwrap_or_default().to_owned();
        let number = |key: &str| {
            fields
                .get(key)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(-1)
        };
        clients.push(ClientInfo {
            id: get("id"),
            addr: get("addr"),
            name: get("name"),
            user: get("user"),
            db: number("db"),
            age: number("age"),
            idle: number("idle"),
            cmd: get("cmd"),
            flags: get("flags"),
        });
    }
    clients
}

/// 解析 `CONFIG GET` 的应答（`RESP2` 平铺数组 / `RESP3` Map 都能吃）。
#[must_use]
pub fn parse_config_entries(reply: &Value) -> Vec<ConfigEntry> {
    as_pairs(reply)
        .into_iter()
        .map(|(name, value)| ConfigEntry {
            name: String::from_utf8_lossy(&name).into_owned(),
            value: String::from_utf8_lossy(&value).into_owned(),
        })
        .collect()
}

/// 解析 `MEMORY STATS` 的应答：平铺的 `[名, 值, 名, 值, …]`。
///
/// 值有三种形态：整数 / 浮点（直接取字节）、字符串、以及 `db.0` 这种**嵌套数组**
/// （渲染成 `[a, b, c]`）。落单的尾巴丢弃。
#[must_use]
pub fn parse_memory_stats(reply: &Value) -> Vec<(String, String)> {
    let items = as_array(reply);
    let mut stats = Vec::with_capacity(items.len() / 2);
    let mut iter = items.iter();
    while let (Some(name), Some(value)) = (iter.next(), iter.next()) {
        let name = as_bytes(name)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        stats.push((name, render_stat_value(value)));
    }
    stats
}

/// 把一个 `MEMORY STATS` 的值渲染成文本（嵌套数组递归展开）。
fn render_stat_value(value: &Value) -> String {
    match value {
        Value::Array(items) | Value::Set(items) => {
            let parts: Vec<String> = items.iter().map(render_stat_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Nil => "-".to_owned(),
        other => as_bytes(other)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|| "-".to_owned()),
    }
}

/// 解析 `COMMAND INFO` 的应答，取出**存在的**命令名。
///
/// 不存在的命令在应答里是一个 `Nil` 占位——跳过它，剩下的就是本服务端支持的。
#[must_use]
pub fn parse_command_names(reply: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for item in as_array(reply) {
        if matches!(item, Value::Nil) {
            continue;
        }
        let parts = as_array(&item);
        if let Some(name) = parts.first().and_then(as_bytes) {
            names.push(String::from_utf8_lossy(&name).into_owned());
        }
    }
    names
}

/// 解析 `MODULE LIST` 的应答，取出模块名。
#[must_use]
pub fn parse_module_list(reply: &Value) -> Vec<String> {
    let mut modules = Vec::new();
    for item in as_array(reply) {
        let pairs = as_pairs(&item);
        if let Some((_, name)) = pairs
            .into_iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(b"name"))
        {
            modules.push(String::from_utf8_lossy(&name).into_owned());
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    /// 慢日志解析：完整 6 元、老版本 4 元、结构不完整跳过。
    #[test]
    fn slowlog_parsing() {
        let reply = Value::Array(vec![
            Value::Array(vec![
                Value::Int(42),
                Value::Int(1_700_000_000),
                Value::Int(15_500),
                Value::Array(vec![bulk("GET"), bulk("user:1")]),
                bulk("127.0.0.1:52684"),
                bulk("worker"),
            ]),
            // Redis 6 之前没有 client / name 两列。
            Value::Array(vec![
                Value::Int(41),
                Value::Int(1_699_999_000),
                Value::Int(9_000),
                Value::Array(vec![bulk("PING")]),
            ]),
            // 结构不完整 —— 跳过，不 panic。
            Value::Array(vec![Value::Int(1), Value::Int(2)]),
        ]);
        let entries = parse_slowlog(&reply);
        assert_eq!(entries.len(), 2, "不完整条目必须被跳过");
        assert_eq!(entries[0].id(), 42);
        assert_eq!(entries[0].duration_ms(), "15.500");
        assert_eq!(entries[0].command_line(), "GET user:1");
        assert_eq!(entries[0].client(), "127.0.0.1:52684");
        assert_eq!(entries[0].name(), "worker");
        assert_eq!(entries[1].client(), "", "缺列回落空串");
        assert_eq!(entries[1].command_line(), "PING");
    }

    /// 客户端列表解析：多行、缺列、夹带非客户端行。
    #[test]
    fn client_list_parsing() {
        let text = "id=3 addr=127.0.0.1:52684 laddr=127.0.0.1:6379 fd=8 name= age=0 idle=0 \
                    flags=N db=0 cmd=client|list user=default\n\
                    id=7 addr=10.0.0.5:41234 name=worker user=app db=3 age=91 idle=12 \
                    flags=N cmd=get\n\
                    \n\
                    这里是一行说明文本\n";
        let clients = parse_client_list(text);
        assert_eq!(clients.len(), 2, "空行与非客户端行都要剔除");
        assert_eq!(clients[0].id(), "3");
        assert_eq!(clients[0].addr(), "127.0.0.1:52684");
        assert_eq!(clients[0].cmd(), "client|list");
        assert_eq!(clients[0].user(), "default");
        assert_eq!(clients[1].name(), "worker");
        assert_eq!(clients[1].db(), 3);
        assert_eq!(clients[1].age(), 91);
        assert_eq!(clients[1].idle(), 12);
        assert!(!clients[0].idle().to_string().is_empty());
    }

    /// 缺列时数字回落 `-1`（区分「未知」与「真的是 0」）。
    #[test]
    fn client_list_missing_numbers_fall_back_to_minus_one() {
        let clients = parse_client_list("id=1 addr=1.2.3.4:1 name= flags=N cmd=ping");
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].db(), -1);
        assert_eq!(clients[0].idle(), -1);
        assert_eq!(clients[0].user(), "");
    }

    /// 配置解析：RESP2 平铺数组与 RESP3 Map 都认。
    #[test]
    fn config_parsing_both_protocols() {
        let resp2 = Value::Array(vec![bulk("maxmemory"), bulk("1073741824")]);
        let entries = parse_config_entries(&resp2);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "maxmemory");
        assert_eq!(entries[0].value(), "1073741824");

        let resp3 = Value::Map(vec![(bulk("appendonly"), bulk("yes"))]);
        let entries = parse_config_entries(&resp3);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "appendonly");
        assert_eq!(entries[0].value(), "yes");
    }

    /// 内存统计解析：整数、字符串、嵌套数组三种值形态。
    #[test]
    fn memory_stats_parsing_renders_nested_values() {
        let reply = Value::Array(vec![
            bulk("peak.allocated"),
            Value::Int(1_234_567),
            bulk("fragmentation"),
            Value::Double(1.08),
            bulk("db.0"),
            Value::Array(vec![bulk("overhead.hashtable.main"), Value::Int(104)]),
        ]);
        let stats = parse_memory_stats(&reply);
        assert_eq!(stats.len(), 3);
        assert_eq!(
            stats[0],
            ("peak.allocated".to_owned(), "1234567".to_owned())
        );
        assert_eq!(stats[1], ("fragmentation".to_owned(), "1.08".to_owned()));
        assert_eq!(
            stats[2],
            (
                "db.0".to_owned(),
                "[overhead.hashtable.main, 104]".to_owned()
            )
        );
    }

    /// 命令探测：`COMMAND INFO` 里不存在的命令是 `Nil`，必须跳过。
    #[test]
    fn command_names_skip_nil_entries() {
        let reply = Value::Array(vec![
            Value::Array(vec![bulk("slowlog"), Value::Int(-1)]),
            Value::Nil,
            Value::Array(vec![bulk("memory"), Value::Int(-1)]),
        ]);
        let names = parse_command_names(&reply);
        assert_eq!(names, vec!["slowlog".to_owned(), "memory".to_owned()]);
    }

    /// 模块列表：数组套数组，取 `name` 字段。
    #[test]
    fn module_list_parsing() {
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("name"),
            bulk("search"),
            bulk("ver"),
            Value::Int(20_800),
        ])]);
        assert_eq!(parse_module_list(&reply), vec!["search".to_owned()]);
        // 没有 name 字段的条目跳过。
        assert!(parse_module_list(&Value::Array(vec![Value::Array(vec![bulk("ver")])])).is_empty());
    }

    /// 能力快照：未探测一律放行（不把「探测失败」变成「功能全禁」）。
    #[test]
    fn unprobed_features_allow_everything() {
        let features = ServerFeatures::unprobed(Some("4.0.0".to_owned()));
        assert!(!features.probed());
        assert!(features.supports("slowlog"));
        assert!(features.supports("hotkeys"), "未探测时不隐藏任何命令");
        // 版本**已知**且偏低时，版本门槛要老实报 false——不隐藏能力只针对「不知道」。
        assert!(!features.version_at_least(7, 4));

        // 版本取不到才是「不知道」：一律放行。
        let unknown = ServerFeatures::unprobed(None);
        assert!(unknown.version_at_least(9, 9), "版本未知时不隐藏能力");
    }

    /// 能力快照：探测后按清单判定，大小写无关。
    #[test]
    fn probed_features_answer_by_command_list() {
        let features = ServerFeatures::new(
            Some("7.2.4".to_owned()),
            vec!["slowlog".to_owned(), "client".to_owned()],
            vec!["search".to_owned()],
        );
        assert!(features.probed());
        assert!(features.supports("SLOWLOG"));
        assert!(features.supports("client"));
        assert!(!features.supports("hotkeys"), "不在清单里 = 不支持");
        assert_eq!(features.version(), Some("7.2.4"));
        assert_eq!(features.modules(), &["search".to_owned()]);
    }

    /// 版本门槛：主次版本比较、缺次版本、非数字版本。
    #[test]
    fn version_floor_comparison() {
        let v72 = ServerFeatures::new(Some("7.2.4".to_owned()), Vec::new(), Vec::new());
        assert!(v72.version_at_least(7, 0));
        assert!(v72.version_at_least(7, 2));
        assert!(!v72.version_at_least(7, 4));
        assert!(!v72.version_at_least(8, 0));

        let v7 = ServerFeatures::new(Some("7".to_owned()), Vec::new(), Vec::new());
        assert!(v7.version_at_least(7, 0));
        assert!(!v7.version_at_least(7, 2));

        // 非标准版本号（例如 255.255.255 的 unstable）解析失败 → 不隐藏能力。
        let weird = ServerFeatures::new(Some("unstable".to_owned()), Vec::new(), Vec::new());
        assert!(weird.version_at_least(9, 9));
    }
}
