//! 破坏性命令闸门：判断一条命令是否需要二次确认，以及确认的形式。
//!
//! 与 [`crate::capability`] 的分工：capability 决定「**能不能**做」（访问模式），
//! 本模块决定「做的时候要**怎么确认**」。两者**都要通过**才真正下发命令。
//!
//! 判定只看命令名（首 token），不看参数——纯函数、可单测、无误判面。

/// 二次确认的形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// 要求用户输入**连接的显示名**才放行。
    ///
    /// 用于不可逆的破坏性命令：手要动起来，误触就按不下去。
    TypeServerName,
    /// 不禁用，但先提示「有更安全的替代」。
    ///
    /// `KEYS` 是典型：它本身是读命令，但在大键空间上会阻塞服务端主线程。
    /// 直接拦掉会让用户失去唯一的兜底手段，所以提示后放行。
    PreferScan,
}

impl ConfirmKind {
    /// 稳定标识（英文：内核不依赖 i18n，界面文案由 `duck-app` 用 `tr` 组织）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::TypeServerName => "type_server_name",
            Self::PreferScan => "prefer_scan",
        }
    }
}

/// 取命令行的首 token（大写化）。
///
/// 容忍前导空白与多行输入（只取第一个非空行）；取不到时返回空串。
#[must_use]
pub fn command_name(line: &str) -> String {
    line.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// 取命令行的前两个 token（大写化，空格分隔）。
///
/// `CLUSTER RESET` / `SCRIPT FLUSH` 这类需要看子命令才能定级。
#[must_use]
pub fn command_head(line: &str) -> String {
    line.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// 命令是否需要确认；`None` = 直接放行。
#[must_use]
pub fn confirm_kind(line: &str) -> Option<ConfirmKind> {
    let head = command_head(line);

    // 先看两 token 的形态（子命令级危险）。
    const NEEDS_NAME_HEAD: &[&str] = &[
        "CLUSTER RESET",
        "CLUSTER FAILOVER",
        "CLUSTER SETSLOT",
        "CLUSTER FORGET",
        "SCRIPT FLUSH",
        "FUNCTION FLUSH",
        "FUNCTION DELETE",
        "CONFIG SET",
        "CONFIG REWRITE",
        "CONFIG RESETSTAT",
        "ACL SETUSER",
        "ACL DELUSER",
        "ACL LOAD",
        "MODULE UNLOAD",
    ];
    if NEEDS_NAME_HEAD.contains(&head.as_str()) {
        return Some(ConfirmKind::TypeServerName);
    }

    let name = command_name(line);
    const NEEDS_NAME: &[&str] = &[
        "FLUSHALL",
        "FLUSHDB",
        "SHUTDOWN",
        "SWAPDB",
        "MIGRATE",
        "REPLICAOF",
        "SLAVEOF",
        "DEBUG",
        "MODULE",
    ];
    if NEEDS_NAME.contains(&name.as_str()) {
        return Some(ConfirmKind::TypeServerName);
    }

    // 读命令，但在大键空间上代价极高：提示后放行。
    if name == "KEYS" {
        return Some(ConfirmKind::PreferScan);
    }

    None
}

/// 是否为需要「输入连接名」的破坏性命令。
#[must_use]
pub fn is_destructive(line: &str) -> bool {
    confirm_kind(line) == Some(ConfirmKind::TypeServerName)
}

/// 该命令行是否改动了**数据或服务端持久状态**（供终端标色 / 记历史 / 写闸门判定）。
///
/// 判定的口径是「**只读白名单的反面**」：落在白名单里算读，否则算写。
/// 宁可把未知命令当写（保守），也不能把写命令当读放过。
///
/// ⚠️ **容器命令必须看两 token**：`CONFIG GET` 是读、`CONFIG SET` 是写，
/// 只按首 token 分类必然出错。所以白名单分两张：
/// 名级（[`READ_ONLY_COMMANDS`]）与两 token 头部级（[`READ_ONLY_HEADS`]）。
///
/// **连接级命令不算写**（`AUTH` / `SELECT` / `HELLO` / `PING` …）：它们不碰数据，
/// 在只读连接里也必须能跑——否则只读模式连切库都做不到。
#[must_use]
pub fn is_write_command(line: &str) -> bool {
    let name = command_name(line);
    if name.is_empty() {
        return false;
    }
    let head = command_head(line);
    !READ_ONLY_COMMANDS.contains(&name.as_str()) && !READ_ONLY_HEADS.contains(&head.as_str())
}

/// 名级只读白名单：**没有子命令语义**的命令。
///
/// 刻意保守：只列**确定不改数据**的命令。
/// - `GETEX` / `GETDEL` / `TOUCH` 这类会改 TTL / 访问时间的**不在其中**；
/// - 容器命令（`CONFIG` / `CLUSTER` / `ACL` / `CLIENT` / `MEMORY` / `SLOWLOG` /
///   `LATENCY` / `SCRIPT` / `FUNCTION` / `SENTINEL` / `COMMAND` / `OBJECT` / `XINFO`）
///   **一律不进本表**，改由 [`READ_ONLY_HEADS`] 按子命令判定。
pub const READ_ONLY_COMMANDS: &[&str] = &[
    // 连接级（不碰数据，只读连接里也要能用）
    "PING",
    "ECHO",
    "AUTH",
    "HELLO",
    "SELECT",
    "QUIT",
    "RESET",
    "WAITAOF",
    // 服务端信息（只读）
    "INFO",
    "DBSIZE",
    "TIME",
    "ROLE",
    "LOLWUT",
    "WAIT",
    // 键元信息（只读）
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "SCAN",
    "RANDOMKEY",
    "DUMP",
    "KEYS",
    // 字符串
    "GET",
    "MGET",
    "STRLEN",
    "GETRANGE",
    "SUBSTR",
    "BITCOUNT",
    "BITPOS",
    "GETBIT",
    // 哈希
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    // 列表
    "LLEN",
    "LINDEX",
    "LRANGE",
    "LPOS",
    // 集合
    "SCARD",
    "SISMEMBER",
    "SMISMEMBER",
    "SMEMBERS",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    "SSCAN",
    // 有序集合
    "ZCARD",
    "ZSCORE",
    "ZMSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZRANGE",
    "ZREVRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGEBYSCORE",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANDMEMBER",
    "ZSCAN",
    // 流
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XREAD",
    "XPENDING",
    // 其他类型（读形态）
    "PFCOUNT",
    "GEOHASH",
    "GEOPOS",
    "GEODIST",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "BITFIELD_RO",
    "LCS",
    "SORT_RO",
];

/// 两 token 头部级的只读白名单（容器命令的读子命令）。
///
/// 只列**确定只读**的子命令；未列出的子命令（如 `CONFIG SET`、`CLUSTER RESET`、
/// `SCRIPT LOAD`、`SENTINEL FLUSHCONFIG`）一律按写处理。
pub const READ_ONLY_HEADS: &[&str] = &[
    // CONFIG
    "CONFIG GET",
    "CONFIG HELP",
    // COMMAND
    "COMMAND COUNT",
    "COMMAND LIST",
    "COMMAND INFO",
    "COMMAND DOCS",
    "COMMAND GETKEYS",
    "COMMAND GETKEYSANDFLAGS",
    // CLUSTER（只读查询）
    "CLUSTER INFO",
    "CLUSTER NODES",
    "CLUSTER SLOTS",
    "CLUSTER SHARDS",
    "CLUSTER MYID",
    "CLUSTER MYSHARDID",
    "CLUSTER KEYSLOT",
    "CLUSTER COUNTKEYSINSLOT",
    "CLUSTER GETKEYSINSLOT",
    "CLUSTER COUNT-FAILURE-REPORTS",
    "CLUSTER LINKS",
    "CLUSTER HELP",
    // SCRIPT / FUNCTION（只看不改）
    "SCRIPT EXISTS",
    "FUNCTION LIST",
    "FUNCTION STATS",
    "FUNCTION DUMP",
    "FUNCTION HELP",
    // ACL（只看不改）
    "ACL WHOAMI",
    "ACL LIST",
    "ACL CAT",
    "ACL GETUSER",
    "ACL DRYRUN",
    "ACL GENPASS",
    "ACL HELP",
    // CLIENT（只看不断）
    "CLIENT LIST",
    "CLIENT INFO",
    "CLIENT GETNAME",
    "CLIENT ID",
    "CLIENT HELP",
    // MEMORY / SLOWLOG / LATENCY（只看不重置）
    "MEMORY USAGE",
    "MEMORY DOCTOR",
    "MEMORY STATS",
    "MEMORY MALLOC-STATS",
    "MEMORY HELP",
    "SLOWLOG GET",
    "SLOWLOG LEN",
    "SLOWLOG HELP",
    "LATENCY LATEST",
    "LATENCY HISTORY",
    "LATENCY DOCTOR",
    "LATENCY GRAPH",
    "LATENCY HELP",
    // SENTINEL（只查不改）
    "SENTINEL MASTERS",
    "SENTINEL MASTER",
    "SENTINEL SENTINELS",
    "SENTINEL REPLICAS",
    "SENTINEL SLAVES",
    "SENTINEL GET-MASTER-ADDR-BY-NAME",
    "SENTINEL IS-MASTER-DOWN-BY-ADDR",
    "SENTINEL CKQUORUM",
    "SENTINEL HELP",
    // OBJECT / XINFO
    "OBJECT ENCODING",
    "OBJECT FREQ",
    "OBJECT IDLETIME",
    "OBJECT REFCOUNT",
    "OBJECT HELP",
    "XINFO STREAM",
    "XINFO GROUPS",
    "XINFO CONSUMERS",
    "XINFO HELP",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 首 token 提取：大小写、前导空白、多行输入。
    #[test]
    fn command_name_extraction() {
        assert_eq!(command_name("  get foo  "), "GET");
        assert_eq!(command_name("hset user:1 name duck"), "HSET");
        assert_eq!(command_name("HSET user:1 name duck"), "HSET");
        assert_eq!(command_name("\n\n  del a\n"), "DEL");
        assert_eq!(command_name("   "), "");
        assert_eq!(command_name(""), "");
    }

    /// 两 token 头部：子命令级定级用。
    #[test]
    fn command_head_extraction() {
        assert_eq!(command_head("cluster reset hard"), "CLUSTER RESET");
        assert_eq!(command_head("CONFIG   SET   maxmemory 1mb"), "CONFIG SET");
        assert_eq!(command_head("  flushall  "), "FLUSHALL");
        assert_eq!(command_head("cluster"), "CLUSTER");
    }

    /// 破坏性命令必须要求输入连接名。
    #[test]
    fn destructive_commands_need_name() {
        for line in [
            "FLUSHALL",
            "flushall async",
            "FLUSHDB",
            "CLUSTER RESET HARD",
            "cluster failover",
            "SCRIPT FLUSH",
            "FUNCTION FLUSH",
            "CONFIG SET maxmemory 100mb",
            "CONFIG REWRITE",
            "ACL DELUSER duck",
            "SHUTDOWN NOSAVE",
            "SWAPDB 0 1",
            "REPLICAOF NO ONE",
            "SLAVEOF host 6379",
            "DEBUG SLEEP 1",
            "MIGRATE host 6379 key 0 1000",
        ] {
            assert_eq!(
                confirm_kind(line),
                Some(ConfirmKind::TypeServerName),
                "`{line}` 应为需输入连接名"
            );
            assert!(is_destructive(line), "`{line}` 应判为破坏性");
        }
    }

    /// KEYS 提示替代方案但放行——不能拦掉用户唯一的兜底手段。
    #[test]
    fn keys_warns_but_is_not_blocked() {
        assert_eq!(confirm_kind("KEYS *"), Some(ConfirmKind::PreferScan));
        assert!(!is_destructive("KEYS *"), "KEYS 不该被升级成破坏性");
    }

    /// 普通命令直接放行。
    #[test]
    fn ordinary_commands_pass_through() {
        for line in [
            "GET foo",
            "SET foo bar",
            "HSCAN page:home 0 COUNT 100",
            "SCAN 0 MATCH user:* COUNT 500",
            "INFO memory",
            "DEL stale:key",
            "EXPIRE key 60",
            "",
            "   ",
        ] {
            assert_eq!(confirm_kind(line), None, "`{line}` 不该要求确认");
        }
    }

    /// 只读白名单：读命令不误判成写，未知命令保守判成写。
    #[test]
    fn write_detection_is_conservative() {
        // 明确的读
        for line in [
            "GET k",
            "SCAN 0",
            "INFO",
            "TTL k",
            "LRANGE l 0 -1",
            "ZRANGE z 0 -1 WITHSCORES",
            "KEYS *",
        ] {
            assert!(!is_write_command(line), "`{line}` 应判为读");
        }
        // 连接级命令不算写：只读连接里也必须能跑（否则连切库都做不到）。
        for line in ["AUTH pw", "SELECT 2", "PING", "HELLO 3", "RESET"] {
            assert!(!is_write_command(line), "`{line}` 是连接级命令，不该当写");
        }
        // 明确的写
        for line in [
            "SET k v",
            "DEL k",
            "FLUSHALL",
            "HSET h f v",
            "LPUSH l a",
            "GETDEL k",
            "GETEX k",
        ] {
            assert!(is_write_command(line), "`{line}` 应判为写");
        }
        // 未知命令按写处理（保守）
        assert!(is_write_command("ZZZUNKNOWN k"), "未知命令应保守判为写");
        // 空命令不算写
        assert!(!is_write_command("   "));
    }

    /// 容器命令按**子命令**分类：读子放行、写子拦下、未列出的子命令保守按写。
    #[test]
    fn container_commands_split_by_subcommand() {
        // 读子命令
        for line in [
            "CONFIG GET maxmemory",
            "CONFIG HELP",
            "CLUSTER INFO",
            "CLUSTER NODES",
            "CLUSTER SLOTS",
            "ACL WHOAMI",
            "ACL LIST",
            "CLIENT LIST",
            "MEMORY USAGE k",
            "MEMORY DOCTOR",
            "SLOWLOG GET 10",
            "SLOWLOG LEN",
            "SCRIPT EXISTS abc",
            "FUNCTION LIST",
            "COMMAND COUNT",
            "OBJECT ENCODING k",
            "XINFO STREAM s",
            "LATENCY LATEST",
            "SENTINEL MASTERS",
        ] {
            assert!(!is_write_command(line), "`{line}` 应判为读");
        }
        // 写子命令 / 未列出的子命令
        for line in [
            "CONFIG SET maxmemory 100mb",
            "CONFIG REWRITE",
            "CONFIG RESETSTAT",
            "CLUSTER RESET HARD",
            "CLUSTER SETSLOT 1 NODE abc",
            "ACL SETUSER duck on",
            "ACL DELUSER duck",
            "CLIENT KILL ID 3",
            "MEMORY PURGE",
            "SLOWLOG RESET",
            "SCRIPT LOAD \"return 1\"",
            "SCRIPT FLUSH",
            "FUNCTION LOAD \"#!lua\"",
            "SENTINEL FLUSHCONFIG",
            "LATENCY RESET",
            "OBJECT", // 无子命令 → 保守按写
        ] {
            assert!(is_write_command(line), "`{line}` 应判为写");
        }
    }

    /// 白名单的结构性守卫：名级表不得混入容器命令，且两张表内部不重复。
    ///
    /// 容器命令只按首 token 分类必然出错（`CONFIG GET` vs `CONFIG SET`），
    /// 所以它们**必须**留在 [`READ_ONLY_HEADS`] 里。这条测试把规矩钉死，
    /// 防止后来者图省事把 `CONFIG` 加回名级表。
    #[test]
    fn read_only_tables_stay_clean() {
        const CONTAINERS: &[&str] = &[
            "CONFIG", "CLUSTER", "ACL", "CLIENT", "MEMORY", "SLOWLOG", "LATENCY", "SCRIPT",
            "FUNCTION", "SENTINEL", "COMMAND", "OBJECT", "XINFO",
        ];
        for container in CONTAINERS {
            assert!(
                !READ_ONLY_COMMANDS.contains(container),
                "容器命令 `{container}` 不得进名级白名单（读 / 写子命令会被混为一谈）"
            );
        }

        // 两张表内部无重复（重复说明改的时候漏删）。
        let mut sorted = READ_ONLY_COMMANDS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "名级白名单有重复项");

        let mut heads = READ_ONLY_HEADS.to_vec();
        heads.sort_unstable();
        let before = heads.len();
        heads.dedup();
        assert_eq!(before, heads.len(), "头部白名单有重复项");

        // 头部表里的每一项都必须真的是两 token（否则它属于名级表）。
        for head in READ_ONLY_HEADS {
            assert_eq!(
                head.split_whitespace().count(),
                2,
                "`{head}` 不是两 token，应放名级白名单"
            );
        }
    }

    /// 稳定标识不随重构漂移。
    #[test]
    fn wire_names_are_stable() {
        assert_eq!(ConfirmKind::TypeServerName.wire_name(), "type_server_name");
        assert_eq!(ConfirmKind::PreferScan.wire_name(), "prefer_scan");
    }
}
