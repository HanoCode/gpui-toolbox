//! 阻塞连接：建连、服务端探测、键读取与原始命令。
//!
//! **全部方法阻塞**（设计文档 §4.2）。调用方负责把调用丢进
//! `background_executor`；本 crate 不开线程、不建 runtime、不引 tokio。
//!
//! 本模块只做「拼命令 → 取应答 → 转成本地模型」，不做任何解码与展示。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use redis::{Client, Connection, Value};

use crate::config::RedisConfig;
use crate::error::RedisError;
use crate::scan::{ScanPage, clamp_count, parse_scan_reply};
use crate::server::{self, ClientInfo, ConfigEntry, ServerFeatures, SlowlogEntry};
use crate::value::{KeyType, RedisValue, StreamEntry, as_byte_array, as_bytes, as_int, as_text};

/// ACL 只读探测的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclProbe {
    /// 探测成功，服务端明确允许 / 拒绝写操作。
    Known {
        /// 该用户是否只读（`ACL DRYRUN` 拒了一个无害的写命令）。
        read_only: bool,
    },
    /// 探测不出来：服务端版本低于 7（没有 `ACL DRYRUN`），或当前用户无权执行它。
    ///
    /// **未知按「可写」处理**——否则会在旧服务端上把写全禁掉，
    /// 而分级锁写本来就还有「环境标签 + 只读开关」两道闸门兜着。
    Unknown,
}

impl AclProbe {
    /// 供 [`crate::capability::Gate`] 使用的 `acl_read_only` 输入。
    #[must_use]
    pub const fn acl_read_only(self) -> bool {
        matches!(self, Self::Known { read_only: true })
    }
}

/// 一条已建立的阻塞连接。
pub struct RedisConnection {
    conn: Connection,
    /// 连接目标的展示串（`host:port/db`），用于日志与错误文案。
    target: String,
}

impl RedisConnection {
    /// 按档案建连。
    ///
    /// `password` 为 `None` 表示匿名连接。TLS 与 SSH 隧道在 M1 均未接入：
    /// 档案里启用了任何一个都**明确报错**而不是静默降级——隧道尤其如此，
    /// 静默直连会把本应走加密链路的流量以明文发出去。
    pub fn connect(cfg: &RedisConfig, password: Option<&str>) -> Result<Self, RedisError> {
        let conn = open_connection(cfg, password)?;
        Ok(Self {
            conn,
            target: connection_target(cfg),
        })
    }

    /// 连接目标（`host:port/db`）。
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// 活性探测。
    pub fn ping(&mut self) -> Result<(), RedisError> {
        let reply: String = redis::cmd("PING").query(&mut self.conn)?;
        if reply.trim() == "PONG" {
            Ok(())
        } else {
            Err(RedisError::Command(format!(
                "unexpected PING reply: {reply}"
            )))
        }
    }

    /// 切换逻辑库。
    pub fn select(&mut self, db: u32) -> Result<(), RedisError> {
        if db > crate::config::MAX_DB {
            return Err(RedisError::Config(format!("db out of range: {db}")));
        }
        redis::cmd("SELECT")
            .arg(db)
            .query::<Value>(&mut self.conn)?;
        self.target = format!(
            "{}/{}",
            self.target
                .rsplit_once('/')
                .map_or(self.target.as_str(), |(head, _)| head),
            db
        );
        Ok(())
    }

    /// 取 `INFO` 原文；`section` 为 `None` 时取全部。
    pub fn info(&mut self, section: Option<&str>) -> Result<String, RedisError> {
        let mut cmd = redis::cmd("INFO");
        if let Some(section) = section {
            cmd.arg(section);
        }
        Ok(cmd.query::<String>(&mut self.conn)?)
    }

    /// 服务端版本描述（`INFO server` 里的 `redis_version`，Valkey 回落 `valkey_version`）。
    ///
    /// 两者都不在时返回整个 `INFO` 文本里的第一行非空内容，
    /// 再不济就返回 `unknown`——**不报错**，版本只是展示信息。
    pub fn server_version(&mut self) -> Result<String, RedisError> {
        let text = self.info(Some("server"))?;
        Ok(info_field(&text, "redis_version")
            .or_else(|| info_field(&text, "valkey_version"))
            .unwrap_or_else(|| "unknown".to_owned()))
    }

    /// 当前库的键总数（`DBSIZE`）。
    pub fn dbsize(&mut self) -> Result<u64, RedisError> {
        let reply: Value = redis::cmd("DBSIZE").query(&mut self.conn)?;
        as_int(&reply)
            .map(|n| u64::try_from(n).unwrap_or(0))
            .ok_or_else(|| RedisError::Command("DBSIZE reply is not an integer".into()))
    }

    /// 探测服务端用户的写权限（`ACL DRYRUN`，Redis 7+）。
    ///
    /// 刻意用一个**绝对不会落地**的假键名做试探：`ACL DRYRUN` 只做权限判定，
    /// 不执行命令，所以不会真的写进去，也不会留下痕迹。
    pub fn acl_probe(&mut self) -> AclProbe {
        let probe = redis::cmd("ACL")
            .arg("DRYRUN")
            .arg("default")
            .arg("SET")
            .arg("__duck_acl_probe__")
            .arg("1")
            .query::<Value>(&mut self.conn);
        match probe {
            Ok(_) => AclProbe::Known { read_only: false },
            Err(err) => {
                let text = err.to_string();
                // 权限不足 / 未知命令 → 探测不出结论，按可写处理（见 AclProbe 文档）。
                if text.contains("NOPERM") || text.to_ascii_lowercase().contains("unknown command")
                {
                    if text.contains("NOPERM") && !text.contains("ACL") {
                        return AclProbe::Known { read_only: true };
                    }
                    if text.contains("NOPERM") && text.contains("ACL") {
                        // 连 ACL 命令本身都没权限 → 无从判断。
                        return AclProbe::Unknown;
                    }
                    return AclProbe::Unknown;
                }
                AclProbe::Known { read_only: false }
            }
        }
    }

    /// 取键类型（`TYPE`）。
    pub fn key_type(&mut self, key: &str) -> Result<KeyType, RedisError> {
        let reply: Value = redis::cmd("TYPE").arg(key).query(&mut self.conn)?;
        Ok(as_text(&reply)
            .map(|text| KeyType::from_type_reply(text.trim()))
            .unwrap_or(KeyType::Module))
    }

    /// 取键的剩余存活秒数。
    ///
    /// `-1` = 无过期时间；`-2` = 键不存在（与 Redis 协议一致，原样返回）。
    pub fn ttl(&mut self, key: &str) -> Result<i64, RedisError> {
        let reply: Value = redis::cmd("TTL").arg(key).query(&mut self.conn)?;
        as_int(&reply).ok_or_else(|| RedisError::Command("TTL reply is not an integer".into()))
    }

    /// 单批 `SCAN`。
    ///
    /// `pattern` 为 `None` 时不过滤；`type_filter` 走服务端 `TYPE` 过滤
    /// （只接受 [`KeyType::SCANNABLE`] 里的类型，其余忽略而不是报错——
    /// 界面上的「模块类型」筛选本来就无法在服务端表达）。
    pub fn scan(
        &mut self,
        cursor: u64,
        pattern: Option<&str>,
        count: usize,
        type_filter: Option<KeyType>,
    ) -> Result<ScanPage, RedisError> {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor);
        if let Some(pattern) = pattern.filter(|p| !p.trim().is_empty()) {
            cmd.arg("MATCH").arg(pattern);
        }
        cmd.arg("COUNT").arg(clamp_count(count));
        if let Some(name) = type_filter.and_then(KeyType::server_name) {
            cmd.arg("TYPE").arg(name);
        }
        let reply: Value = cmd.query(&mut self.conn)?;
        parse_scan_reply(&reply)
    }

    /// 读取一个键的**第一页**内容。
    ///
    /// 集合类一律走游标扫描 / 窗口读取，**绝不 `HGETALL` / `SMEMBERS` / `ZRANGE ... 全量`**
    /// （设计文档 §4.6）：大集合的全量读取会把服务端主线程拖死。
    pub fn load_value(
        &mut self,
        key: &str,
        key_type: KeyType,
        page: usize,
    ) -> Result<RedisValue, RedisError> {
        let count = clamp_count(page);
        match key_type {
            KeyType::Missing => Ok(RedisValue::Missing),
            KeyType::String => {
                let reply: Value = redis::cmd("GET").arg(key).query(&mut self.conn)?;
                match as_bytes(&reply) {
                    Some(bytes) => Ok(RedisValue::String(bytes)),
                    None => Ok(RedisValue::Missing),
                }
            }
            KeyType::Hash => {
                let reply: Value = redis::cmd("HSCAN")
                    .arg(key)
                    .arg(0u64)
                    .arg("COUNT")
                    .arg(count)
                    .query(&mut self.conn)?;
                let pairs = as_byte_array(&reply);
                Ok(RedisValue::Hash(pair_up(pairs)))
            }
            KeyType::Set => {
                let reply: Value = redis::cmd("SSCAN")
                    .arg(key)
                    .arg(0u64)
                    .arg("COUNT")
                    .arg(count)
                    .query(&mut self.conn)?;
                // SSCAN 应答是 [cursor, members]，取第二元。
                let members = crate::scan::parse_scan_reply(&reply)
                    .map(|page| page.keys.into_iter().map(String::into_bytes).collect())
                    .unwrap_or_default();
                Ok(RedisValue::Set(members))
            }
            KeyType::List => {
                let reply: Value = redis::cmd("LRANGE")
                    .arg(key)
                    .arg(0i64)
                    .arg(i64::try_from(count).unwrap_or(99).saturating_sub(1))
                    .query(&mut self.conn)?;
                Ok(RedisValue::List(as_byte_array(&reply)))
            }
            KeyType::ZSet => {
                let reply: Value = redis::cmd("ZSCAN")
                    .arg(key)
                    .arg(0u64)
                    .arg("COUNT")
                    .arg(count)
                    .query(&mut self.conn)?;
                let flat = as_byte_array(&reply);
                Ok(RedisValue::ZSet(score_pair(flat)))
            }
            KeyType::Stream => {
                let reply: Value = redis::cmd("XRANGE")
                    .arg(key)
                    .arg("-")
                    .arg("+")
                    .arg("COUNT")
                    .arg(count)
                    .query(&mut self.conn)?;
                Ok(RedisValue::Stream(parse_stream_entries(&reply)))
            }
            // 模块类型的值需要 `DUMP`，M1.6 才做（且要限制体积）。
            KeyType::Module => Err(RedisError::Unsupported(format!(
                "module type `{}` viewer is not implemented yet",
                key_type_label(key_type)
            ))),
        }
    }

    /// 删除一个键，返回真正删掉的数量（0 或 1）。
    ///
    /// 不走 [`Self::raw_command`]：那条路径按空格切命令行，键名里带空格或
    /// 非 UTF-8 字节就不能往返。写操作一律走这里这类**参数化**方法。
    pub fn del_key(&mut self, key: &str) -> Result<u64, RedisError> {
        let reply: Value = redis::cmd("DEL").arg(key).query(&mut self.conn)?;
        Ok(u64::try_from(as_int(&reply).unwrap_or(0)).unwrap_or(0))
    }

    /// 覆盖写入一个字符串键（`SET`）。
    ///
    /// `keep_ttl` 为真时带 `KEEPTTL`：编辑既有键时保住它原有的过期时间
    /// （否则 `SET` 会顺手把 TTL 抹掉，这是最容易踩的一脚）。
    /// 值按**原始字节**下发，不对 UTF-8 做任何假设。
    pub fn set_string(
        &mut self,
        key: &str,
        value: &[u8],
        keep_ttl: bool,
    ) -> Result<(), RedisError> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        if keep_ttl {
            cmd.arg("KEEPTTL");
        }
        let _: Value = cmd.query(&mut self.conn)?;
        Ok(())
    }

    // ── 结构化写操作 ────────────────────────────────────────────────────
    //
    // 全部**参数化**：键名与元素按原始字节下发，不经命令行切分。理由同
    // [`Self::del_key`]——`raw_command` 按空格切词，键名里带空格或非 UTF-8
    // 字节就不能往返，而结构化编辑恰恰是键名千奇百怪的场景。
    //
    // 本层**不做安全判定**：写闸门（`capability`）与破坏性确认（`writegate`）
    // 由调用方在派发前跑完。

    /// 写一个哈希字段（`HSET`）；返回是否**新建**了该字段。
    pub fn hset(&mut self, key: &str, field: &[u8], value: &[u8]) -> Result<bool, RedisError> {
        let added: i64 = redis::cmd("HSET")
            .arg(key)
            .arg(field)
            .arg(value)
            .query(&mut self.conn)?;
        Ok(added > 0)
    }

    /// 删一个哈希字段（`HDEL`）；返回删除数量（0 或 1）。
    pub fn hdel(&mut self, key: &str, field: &[u8]) -> Result<u64, RedisError> {
        let removed: i64 = redis::cmd("HDEL")
            .arg(key)
            .arg(field)
            .query(&mut self.conn)?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// 推入列表元素（`LPUSH` / `RPUSH`）；返回推入后的列表长度。
    pub fn list_push(&mut self, key: &str, value: &[u8], front: bool) -> Result<u64, RedisError> {
        let command = if front { "LPUSH" } else { "RPUSH" };
        let length: i64 = redis::cmd(command)
            .arg(key)
            .arg(value)
            .query(&mut self.conn)?;
        Ok(u64::try_from(length).unwrap_or(0))
    }

    /// 按位置改列表元素（`LSET`）。
    pub fn list_set(&mut self, key: &str, index: i64, value: &[u8]) -> Result<(), RedisError> {
        let _: Value = redis::cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(value)
            .query(&mut self.conn)?;
        Ok(())
    }

    /// 删掉列表里指定位置的元素。
    ///
    /// **Redis 没有「按位置删元素」的命令**，故走 zedis 同款的两步法：先 `LSET`
    /// 把该位置置成一个**唯一哨兵**，再 `LREM 1 <哨兵>` 把它删掉。
    ///
    /// 两条防线：① 先 `LINDEX` 回读，位置不存在（`Nil`）直接报错而不是瞎写；
    /// ② 哨兵带纳秒时间戳 + 进程内自增序号，并校验与原值不同——若撞车就换一个。
    /// 少了任何一条都可能删掉用户的真实数据。
    pub fn list_remove_at(&mut self, key: &str, index: i64) -> Result<u64, RedisError> {
        let current: Value = redis::cmd("LINDEX")
            .arg(key)
            .arg(index)
            .query(&mut self.conn)?;
        if matches!(current, Value::Nil) {
            return Err(RedisError::Command(format!(
                "list index {index} is out of range"
            )));
        }
        let original = as_bytes(&current).unwrap_or_default();
        let marker = unique_marker(&original);
        let _: Value = redis::cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(&marker)
            .query(&mut self.conn)?;
        // `LREM` 的 count 用 1：哨兵唯一，删第一个匹配即可——**绝不用 0**，
        // 那会删光列表里所有等于哨兵的元素（哨兵撞车时就是数据灾难）。
        let removed: i64 = redis::cmd("LREM")
            .arg(key)
            .arg(1)
            .arg(&marker)
            .query(&mut self.conn)?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// 集合加成员（`SADD`）；返回是否**新增**（已在集合里返回 `false`）。
    pub fn set_add(&mut self, key: &str, member: &[u8]) -> Result<bool, RedisError> {
        let added: i64 = redis::cmd("SADD")
            .arg(key)
            .arg(member)
            .query(&mut self.conn)?;
        Ok(added > 0)
    }

    /// 集合删成员（`SREM`）；返回删除数量（0 或 1）。
    pub fn set_remove(&mut self, key: &str, member: &[u8]) -> Result<u64, RedisError> {
        let removed: i64 = redis::cmd("SREM")
            .arg(key)
            .arg(member)
            .query(&mut self.conn)?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// 写有序集合成员分数（`ZADD`）；返回是否**新增**成员。
    ///
    /// `previous` 非空且与新成员不同 = **成员改名**：先加新成员、再删旧成员。
    /// 顺序刻意如此——中途失败最多留一个多余的旧成员（可见、可再删），
    /// 反过来先删后加则会**丢数据**。本内核不开事务（阻塞连接 + 单命令语义），
    /// 两命令之间的窗口只能靠顺序把损失压到最小。
    pub fn zset_put(
        &mut self,
        key: &str,
        score: f64,
        member: &[u8],
        previous: Option<&[u8]>,
    ) -> Result<u64, RedisError> {
        let added: i64 = redis::cmd("ZADD")
            .arg(key)
            .arg(score)
            .arg(member)
            .query(&mut self.conn)?;
        if let Some(previous) = previous.filter(|prev| *prev != member) {
            self.zset_remove(key, previous)?;
        }
        Ok(u64::try_from(added).unwrap_or(0))
    }

    /// 有序集合删成员（`ZREM`）；返回删除数量（0 或 1）。
    pub fn zset_remove(&mut self, key: &str, member: &[u8]) -> Result<u64, RedisError> {
        let removed: i64 = redis::cmd("ZREM")
            .arg(key)
            .arg(member)
            .query(&mut self.conn)?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// 设置键的存活秒数（`EXPIRE`）；返回是否真的设上了（键不存在 / 秒数非法则 `false`）。
    ///
    /// `seconds` 为 0 表示**立即过期**（服务端语义），界面要拦在按下去之前。
    pub fn expire(&mut self, key: &str, seconds: i64) -> Result<bool, RedisError> {
        let applied: i64 = redis::cmd("EXPIRE")
            .arg(key)
            .arg(seconds)
            .query(&mut self.conn)?;
        Ok(applied > 0)
    }

    /// 移除键的过期时间（`PERSIST`）；返回是否真的移除了（本来就没 TTL 则 `false`）。
    pub fn persist(&mut self, key: &str) -> Result<bool, RedisError> {
        let applied: i64 = redis::cmd("PERSIST").arg(key).query(&mut self.conn)?;
        Ok(applied > 0)
    }

    /// 重命名键（`RENAME`）。
    ///
    /// ⚠️ `RENAME` 会**静默覆盖**已存在的目标键（Redis 语义，不是本层的选择）。
    /// 本方法**不做存在性检查**：多一次往返挡不住「检查完到重命名之间被其他客户端
    /// 插入目标键」的竞态，真正的保护是界面上让用户看清目标键名。
    pub fn rename(&mut self, key: &str, new_key: &str) -> Result<(), RedisError> {
        let _: Value = redis::cmd("RENAME")
            .arg(key)
            .arg(new_key)
            .query(&mut self.conn)?;
        Ok(())
    }

    /// 执行一条原始命令（redis-cli 终端走这里）。
    ///
    /// 参数按空格切分，支持单/双引号包裹（见 [`split_command_args`]）。
    /// **不做任何安全判定**——写闸门与二次确认由调用方在派发前跑完。
    pub fn raw_command(&mut self, line: &str) -> Result<Value, RedisError> {
        let args = split_command_args(line);
        let Some((name, rest)) = args.split_first() else {
            return Err(RedisError::Config("empty command".into()));
        };
        let mut cmd = redis::cmd(name);
        for arg in rest {
            cmd.arg(arg);
        }
        Ok(cmd.query(&mut self.conn)?)
    }

    /// [`Self::raw_command`] 的展示形态：把应答渲染成终端可读文本。
    ///
    /// 单独开一个方法而不是把 `raw_command` 改成返回 `String`：
    /// `redis::Value` 是内核与调用方共享的应答模型，界面层拿它要额外依赖
    /// `redis` crate（本仓纪律是界面层只依赖 `duck-redis`），渲染下沉到这里。
    pub fn raw_command_text(&mut self, line: &str) -> Result<String, RedisError> {
        self.raw_command(line)
            .map(|reply| crate::value::format_reply(&reply))
    }

    // ── 服务端观测（M1.4） ──────────────────────────────────────────────
    //
    // 与「结构化写操作」同一纪律：本层只负责拼命令、取应答、转本地模型，
    // **不做安全判定**——闸门（`capability`）与破坏性确认（`writegate`）
    // 由调用方在派发前跑完。

    /// 读最近 `count` 条慢日志（`SLOWLOG GET`）。
    pub fn slowlog_get(&mut self, count: usize) -> Result<Vec<SlowlogEntry>, RedisError> {
        let reply: Value = redis::cmd("SLOWLOG")
            .arg("GET")
            .arg(count)
            .query(&mut self.conn)?;
        Ok(server::parse_slowlog(&reply))
    }

    /// 慢日志条数（`SLOWLOG LEN`）。
    pub fn slowlog_len(&mut self) -> Result<u64, RedisError> {
        let reply: Value = redis::cmd("SLOWLOG").arg("LEN").query(&mut self.conn)?;
        Ok(as_int(&reply).unwrap_or(0).max(0).unsigned_abs())
    }

    /// 清空慢日志（`SLOWLOG RESET`）。破坏性动作，调用方须先过闸门与二次确认。
    pub fn slowlog_reset(&mut self) -> Result<(), RedisError> {
        let _: Value = redis::cmd("SLOWLOG").arg("RESET").query(&mut self.conn)?;
        Ok(())
    }

    /// 列出所有客户端连接（`CLIENT LIST`）。
    pub fn client_list(&mut self) -> Result<Vec<ClientInfo>, RedisError> {
        let reply: Value = redis::cmd("CLIENT").arg("LIST").query(&mut self.conn)?;
        Ok(server::parse_client_list(
            &as_text(&reply).unwrap_or_default(),
        ))
    }

    /// 按地址杀掉一条客户端连接（`CLIENT KILL ADDR`）；返回是否真的杀掉了。
    ///
    /// 只能按 `addr` 杀：`CLIENT KILL ID` 是 Redis 6 才有的，而 addr 从 2.6 就有。
    pub fn client_kill_addr(&mut self, addr: &str) -> Result<bool, RedisError> {
        let reply: Value = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ADDR")
            .arg(addr)
            .query(&mut self.conn)?;
        // 旧版本回 `+OK`，新版本回被杀的连接数；两种都当「杀掉了」处理。
        Ok(match &reply {
            Value::Okay => true,
            other => as_int(other).unwrap_or(0) > 0,
        })
    }

    /// 内存诊断报告（`MEMORY DOCTOR`）。
    pub fn memory_doctor(&mut self) -> Result<String, RedisError> {
        let reply: Value = redis::cmd("MEMORY").arg("DOCTOR").query(&mut self.conn)?;
        Ok(as_text(&reply).unwrap_or_default())
    }

    /// 内存统计（`MEMORY STATS`）。
    pub fn memory_stats(&mut self) -> Result<Vec<(String, String)>, RedisError> {
        let reply: Value = redis::cmd("MEMORY").arg("STATS").query(&mut self.conn)?;
        Ok(server::parse_memory_stats(&reply))
    }

    /// 单个键占用的字节数（`MEMORY USAGE`）；键不存在返回 `None`。
    pub fn memory_usage(&mut self, key: &str) -> Result<Option<u64>, RedisError> {
        let reply: Value = redis::cmd("MEMORY")
            .arg("USAGE")
            .arg(key)
            .query(&mut self.conn)?;
        Ok(as_int(&reply).and_then(|bytes| u64::try_from(bytes).ok()))
    }

    /// 读服务端配置（`CONFIG GET <pattern>`）。
    pub fn config_get(&mut self, pattern: &str) -> Result<Vec<ConfigEntry>, RedisError> {
        let reply: Value = redis::cmd("CONFIG")
            .arg("GET")
            .arg(pattern)
            .query(&mut self.conn)?;
        Ok(server::parse_config_entries(&reply))
    }

    /// 写一个服务端配置项（`CONFIG SET`）。破坏性动作，调用方须先过闸门与二次确认。
    pub fn config_set(&mut self, name: &str, value: &str) -> Result<(), RedisError> {
        let _: Value = redis::cmd("CONFIG")
            .arg("SET")
            .arg(name)
            .arg(value)
            .query(&mut self.conn)?;
        Ok(())
    }

    /// 把当前配置落盘（`CONFIG REWRITE`）。破坏性动作，同上。
    pub fn config_rewrite(&mut self) -> Result<(), RedisError> {
        let _: Value = redis::cmd("CONFIG").arg("REWRITE").query(&mut self.conn)?;
        Ok(())
    }

    /// 探测服务端能力（`INFO server` + `COMMAND INFO` + `MODULE LIST`）。
    ///
    /// 探测失败（老服务端没有 `COMMAND INFO`、当前用户无权执行）**不算错误**：
    /// 返回 [`ServerFeatures::unprobed`]——它的 `supports` 一律为真，界面按
    /// 「可尝试」处理。真正不支持时那条命令自己会报错，把错误摆出来比我们猜更有用。
    pub fn probe_features(&mut self) -> Result<ServerFeatures, RedisError> {
        let version = self.server_version().ok();
        let mut cmd = redis::cmd("COMMAND");
        cmd.arg("INFO");
        for name in server::PROBE_COMMANDS {
            cmd.arg(name);
        }
        let commands = match cmd.query::<Value>(&mut self.conn) {
            Ok(reply) => server::parse_command_names(&reply)
                .into_iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
            Err(_) => return Ok(ServerFeatures::unprobed(version)),
        };
        // `MODULE LIST` 在无模块的服务端上也会成功（空数组）；真失败就当没有模块。
        let modules = redis::cmd("MODULE")
            .arg("LIST")
            .query::<Value>(&mut self.conn)
            .map(|reply| server::parse_module_list(&reply))
            .unwrap_or_default();
        Ok(ServerFeatures::new(version, commands, modules))
    }
}

/// 按档案开一条阻塞连接。
///
/// TLS 与 SSH 隧道在 M1 均未接入：档案里启用了任何一个都**明确报错**而不是静默降级——
/// 隧道尤其如此，静默直连会把本应走加密链路的流量以明文发出去。
///
/// 抽成自由函数是为了让第二条链路（[`crate::monitor::MonitorSession`]）
/// 复用同一套守卫，而不是各自写一遍（漏掉一处就是一条明文旁路）。
pub(crate) fn open_connection(
    cfg: &RedisConfig,
    password: Option<&str>,
) -> Result<Connection, RedisError> {
    cfg.validate()?;
    if let Some(tunnel) = &cfg.ssh_tunnel {
        return Err(RedisError::Unsupported(format!(
            "SSH tunnel ({}@{}:{}) is not wired up yet",
            tunnel.user,
            tunnel.host,
            tunnel.port.unwrap_or(22)
        )));
    }
    if cfg.tls.is_enabled() {
        return Err(RedisError::Unsupported(format!(
            "TLS ({}) is not enabled in this build; use an SSH tunnel instead",
            cfg.tls.wire_name()
        )));
    }
    // `connection_url` 已把用户名 / 口令百分号编码，元字符不会拆坏 URL。
    let url = cfg.connection_url(password);
    let client = Client::open(url).map_err(RedisError::from)?;
    client.get_connection().map_err(RedisError::from)
}

/// 连接目标的展示串（`host:port/db`），用于日志与错误文案。
pub(crate) fn connection_target(cfg: &RedisConfig) -> String {
    format!("{}:{}/{}", cfg.host.trim(), cfg.effective_port(), cfg.db)
}

/// 类型名的展示标签（错误文案用，内核不依赖 i18n）。
fn key_type_label(key_type: KeyType) -> &'static str {
    match key_type {
        KeyType::String => "string",
        KeyType::List => "list",
        KeyType::Set => "set",
        KeyType::ZSet => "zset",
        KeyType::Hash => "hash",
        KeyType::Stream => "stream",
        KeyType::Missing => "missing",
        KeyType::Module => "module",
    }
}

/// 生成一个列表删除用的哨兵串，并保证它**不等于** `avoid`（原值）。
///
/// 哨兵 = `__duck_lrem_<纳秒时间戳:x>_<进程内序号:x>__`。序号每次自增，所以
/// 循环最多转两圈就退出——`loop` 只是把「万一」写清楚，不是真会重试很多次。
fn unique_marker(avoid: &[u8]) -> Vec<u8> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    loop {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let candidate = format!("__duck_lrem_{nanos:x}_{seq:x}__").into_bytes();
        if candidate != avoid {
            return candidate;
        }
    }
}

/// 从 `INFO` 文本里取一个字段。
#[must_use]
pub fn info_field(text: &str, field: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 把 `[f1, v1, f2, v2, …]` 配对成 `(field, value)`；落单的尾巴丢弃。
#[must_use]
pub fn pair_up(flat: Vec<Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = Vec::with_capacity(flat.len() / 2);
    let mut iter = flat.into_iter();
    while let (Some(field), Some(value)) = (iter.next(), iter.next()) {
        pairs.push((field, value));
    }
    pairs
}

/// 把 `ZSCAN` 的 `[member, score, …]` 配对成 `(member, score)`。
///
/// 分数取不出数字时按 `0` 处理——**不整条丢弃**：成员名本身仍有价值，
/// 而把整页清空会让界面看起来像「键坏了」。
#[must_use]
pub fn score_pair(flat: Vec<Vec<u8>>) -> Vec<(Vec<u8>, f64)> {
    let mut pairs = Vec::with_capacity(flat.len() / 2);
    let mut iter = flat.into_iter();
    while let (Some(member), Some(score)) = (iter.next(), iter.next()) {
        let parsed = String::from_utf8_lossy(&score)
            .trim()
            .parse::<f64>()
            .unwrap_or(0.0);
        pairs.push((member, parsed));
    }
    pairs
}

/// 解析 `XRANGE` 的应答：`[[id, [f1, v1, f2, v2, …]], …]`。
#[must_use]
pub fn parse_stream_entries(reply: &Value) -> Vec<StreamEntry> {
    let mut entries = Vec::new();
    for item in crate::value::as_array(reply) {
        let parts = crate::value::as_array(&item);
        if parts.len() < 2 {
            continue;
        }
        let Some(id) = as_bytes(&parts[0]) else {
            continue;
        };
        let fields = pair_up(as_byte_array(&parts[1]));
        entries.push((id, fields));
    }
    entries
}

/// 把命令行按空格切成参数，支持单引号与双引号包裹。
///
/// 这是 redis-cli 的行为：`SET k "hello world"` 是一个三参数命令。
/// 引号内的引号用反斜杠转义（`"a\"b"` → `a"b`）；不闭合的引号当作**到行尾为止**
/// （比报错更符合交互直觉——用户边打边跑时不该被一句语法错误打断）。
#[must_use]
pub fn split_command_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut started = false;

    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, ch) {
            (Some(_), '\\') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => {
                current.push(c);
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started || !current.is_empty() {
        args.push(current);
    }
    args
}

/// 取 `INFO` 解析后的键值对（`INFO everything` 浏览器用）。
#[must_use]
pub fn parse_info_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// 取 `INFO` 的分节结构：`(节名, [(键, 值)])`。`INFO everything` 浏览器按节折叠。
#[must_use]
pub fn parse_info_sections(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(title) = line.strip_prefix('#') {
            sections.push((title.trim().to_owned(), Vec::new()));
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if sections.is_empty() {
            sections.push((String::new(), Vec::new()));
        }
        if let Some((_, pairs)) = sections.last_mut() {
            pairs.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `INFO` 字段提取：命中、缺失、空值、同名前缀不误命中。
    #[test]
    fn info_field_extraction() {
        let text = "# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\nempty:\r\n";
        assert_eq!(info_field(text, "redis_version").as_deref(), Some("7.2.4"));
        assert_eq!(
            info_field(text, "redis_mode").as_deref(),
            Some("standalone")
        );
        assert_eq!(info_field(text, "empty"), None, "空值视为缺失");
        assert_eq!(info_field(text, "missing"), None);
        // `redis_version` 与 `redis_ver` 不得混淆（按 `field:` 精确前缀匹配）。
        assert_eq!(info_field(text, "redis_ver"), None);
    }

    /// `INFO` 分节解析（原始 INFO 浏览器用）。
    #[test]
    fn info_sections_parsing() {
        let text = "# Server\r\nredis_version:7.2.4\r\n# Clients\r\nconnected_clients:3\r\n";
        let sections = parse_info_sections(text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "Server");
        assert_eq!(
            sections[0].1,
            vec![("redis_version".to_owned(), "7.2.4".to_owned())]
        );
        assert_eq!(sections[1].0, "Clients");
        assert_eq!(sections[1].1[0].0, "connected_clients");

        let pairs = parse_info_pairs(text);
        assert_eq!(pairs.len(), 2, "注释行被剔除");
    }

    /// 平铺配对：正常、落单尾巴、空输入。
    #[test]
    fn pair_helpers() {
        let flat = vec![
            b"f1".to_vec(),
            b"v1".to_vec(),
            b"f2".to_vec(),
            b"v2".to_vec(),
        ];
        assert_eq!(
            pair_up(flat),
            vec![
                (b"f1".to_vec(), b"v1".to_vec()),
                (b"f2".to_vec(), b"v2".to_vec())
            ]
        );

        // 落单的尾巴丢弃，不 panic。
        let odd = vec![b"f1".to_vec(), b"v1".to_vec(), b"orphan".to_vec()];
        assert_eq!(pair_up(odd).len(), 1);

        assert!(pair_up(Vec::new()).is_empty());
    }

    /// ZSCAN 分数配对：正常、坏分数按 0（不丢成员）。
    #[test]
    fn score_pairing_keeps_members() {
        let flat = vec![
            b"m1".to_vec(),
            b"1.5".to_vec(),
            b"m2".to_vec(),
            b"2".to_vec(),
        ];
        let pairs = score_pair(flat);
        assert_eq!(pairs, vec![(b"m1".to_vec(), 1.5), (b"m2".to_vec(), 2.0)]);

        // 分数不是数字：成员必须保留。
        let broken = vec![b"m".to_vec(), b"NaN-ish".to_vec()];
        let pairs = score_pair(broken);
        assert_eq!(pairs.len(), 1, "坏分数不得丢成员");
        assert_eq!(pairs[0].0, b"m".to_vec());
        assert_eq!(pairs[0].1, 0.0);
    }

    /// XRANGE 应答解析。
    #[test]
    fn stream_entry_parsing() {
        let reply = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"1700000000000-0".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"field".to_vec()),
                Value::BulkString(b"value".to_vec()),
            ]),
        ])]);
        let entries = parse_stream_entries(&reply);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, b"1700000000000-0".to_vec());
        assert_eq!(entries[0].1, vec![(b"field".to_vec(), b"value".to_vec())]);

        // 结构不完整时跳过而不是 panic。
        let malformed = Value::Array(vec![Value::Array(vec![Value::BulkString(
            b"only-id".to_vec(),
        )])]);
        assert!(parse_stream_entries(&malformed).is_empty());
    }

    /// 命令行切分：引号、转义、连续空格、不闭合引号。
    #[test]
    fn command_line_splitting() {
        assert_eq!(split_command_args("GET foo"), vec!["GET", "foo"]);
        assert_eq!(
            split_command_args("SET k \"hello world\""),
            vec!["SET", "k", "hello world"]
        );
        assert_eq!(
            split_command_args("SET k 'single quoted'"),
            vec!["SET", "k", "single quoted"]
        );
        assert_eq!(
            split_command_args("SET k \"a\\\"b\""),
            vec!["SET", "k", "a\"b"],
            "引号内转义"
        );
        assert_eq!(
            split_command_args("  GET    foo  "),
            vec!["GET", "foo"],
            "连续空白不产生空参数"
        );
        assert_eq!(
            split_command_args("SET k \"unterminated"),
            vec!["SET", "k", "unterminated"],
            "不闭合引号取到行尾"
        );
        // 空引号是一个**空参数**，不能丢掉（`SET k ""` 有意义）。
        assert_eq!(split_command_args("SET k \"\""), vec!["SET", "k", ""]);
        assert!(split_command_args("   ").is_empty());
    }

    /// 列表删除哨兵：有可辨认前缀、每次不同、且绝不等于原值。
    ///
    /// 这三条都是「误删用户数据」的必要条件，缺一条就会在极端情形下吃掉真实元素。
    #[test]
    fn lrem_marker_is_unique_and_avoids_original() {
        let first = unique_marker(b"value");
        assert!(
            first.starts_with(b"__duck_lrem_"),
            "哨兵要有可辨认前缀，便于事后排查"
        );
        assert_ne!(first, b"value".to_vec(), "不得与原值相同");

        let second = unique_marker(b"value");
        assert_ne!(first, second, "两次调用必须不同（否则可能误删）");

        // 原值恰好长得像哨兵时，也必须换一个。
        let avoid = first.clone();
        assert_ne!(unique_marker(&avoid), avoid, "不得返回与原值相同的哨兵");
    }

    /// ACL 探测结果的语义映射。
    #[test]
    fn acl_probe_semantics() {
        assert!(!AclProbe::Known { read_only: false }.acl_read_only());
        assert!(AclProbe::Known { read_only: true }.acl_read_only());
        assert!(
            !AclProbe::Unknown.acl_read_only(),
            "探测不出来时必须按可写处理，否则旧服务端会被全面禁写"
        );
    }
}
