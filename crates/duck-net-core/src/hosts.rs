//! hosts 解析 / 合并 / 差异 / 应用命令。
//!
//! 逻辑移植自参考仓**前端** `src/utils/hosts.ts`（154 行）：`isValidIp` /
//! `parseHosts` / `mergeProfiles` / `diffContents` / `buildApplyScript` 五个函数的
//! 判定规则一行没改（三条正则、`#` 截断、域名重复计数、按行去注释后求差集）。
//!
//! ## 三处为了「内核不出中文」而做的结构调整
//!
//! 源仓那几个函数把中文写死在字符串里（`'缺少主机名：格式应为 …'`、
//! `'# 由 Hosts 管理工具生成'`、`'# 1. 先备份当前系统 hosts'`）。内核 crate 不许
//! 出现中文（见 `duck-theme` 的漏译闸门），所以：
//!
//! 1. **校验问题**改成 [`IssueKind`] 枚举，只带参数（行号、IP、域名、首次出现的行号）；
//! 2. **合并**只交出 [`MergedSection`] 列表（方案名 / 环境 / 正文），
//!    抬头与分隔线由插件层用自己的译文渲染；
//! 3. **应用命令**交成 [`ApplyCommand`]（`id` + 命令行），注释行同样在插件层。
//!
//! ## 与参考仓后端的差异（有意）
//!
//! 参考仓 `src-tauri/src/hosts_manager.rs` 会**直接写系统 `/etc/hosts`**
//!（先备份成 `hosts.bak_YYYYMMDD_HHMMSS`，权限不足给 `ELEVATION_REQUIRED`）。
//! 本仓**不写系统文件**：设计稿把这条定成硬约束（写 `/etc/hosts` 需要 sudo，
//! 而插件进程没有任何提权通道，硬试只会得到一句「权限不足」）。这里只保留它的
//! 两条口径：[`MAX_HOSTS_BYTES`] 上限与「空内容拒绝写入」。
//!
//! 副产物是 `.bak_` 备份列表这种「读系统目录」的能力也不需要了 —— 备份由用户
//! 自己那条 `cp` 命令产生。
//!
//! ## 参考页七个标签页 → 这里的对应关系
//!
//! 源页面 `HostsManager.vue` 右侧是七个标签页，逐一对上（后两条是形态差异）：
//!
//! | 参考页 | 这里 |
//! |---|---|
//! | 聚合预览 / 校验 / 查找替换 / 方案对比 | [`merge_sections`] / [`parse_hosts`] / [`search_profiles`] + [`replace_in_profiles`] / [`diff_contents`] |
//! | 同步与应用 | [`build_apply_script`]（命令由用户执行，见上） |
//! | 历史留痕 / 回收站 | **无**：源仓记在 localStorage 里；插件一次一退，没有可留痕的地方。文件的 mtime 与版本库（`git log`）本来就在做这件事，再记一份只会分叉 |
//!
//! 左侧那排方案卡片（启用开关 / 复制 / 删除）同样落在文件系统上：**一个文件是
//! 一个方案**，停用写 `# enabled: false`，复制与删除就是 `cp` / `rm`。

use std::sync::OnceLock;

use crate::error::{ErrorKind, NetError, NetResult};

/// 合并结果的字节上限（与参考仓 `hosts_manager.rs` 同一个值）。
pub const MAX_HOSTS_BYTES: usize = 2 * 1024 * 1024;

/// 一条生效的 hosts 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsEntry {
    /// 原始行号（从 1 开始，报告里要能指回去）。
    pub line: usize,
    pub ip: String,
    /// 一条记录可以带多个主机名（`10.0.0.1 a.local b.local`）。
    pub hostnames: Vec<String>,
    /// 原始整行（含行尾注释）。
    pub raw: String,
}

/// 问题的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLevel {
    /// 这一行不会被 hosts 生效（解析不出 IP 或主机名）。
    Error,
    /// 能生效，但大概率是笔误（可疑主机名、域名重复）。
    Warn,
}

impl IssueLevel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
        }
    }
}

/// 问题种类（**只带参数，不含句子**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueKind {
    /// 只有 IP、没有主机名。
    MissingHostname,
    /// IP 不合法（IPv4 四段越界、IPv6 出现非法字符）。
    BadIp(String),
    /// 主机名不符合 RFC 1123 的宽松版（源仓 `HOSTNAME_RE`）。
    SuspiciousHostname(String),
    /// 同一个主机名在多行出现（后出现的不会生效）。
    DuplicateDomain { host: String, first_line: usize },
}

impl IssueKind {
    /// 稳定 id（插件层按它查译文）。
    pub fn id(&self) -> &'static str {
        match self {
            Self::MissingHostname => "missing_hostname",
            Self::BadIp(_) => "bad_ip",
            Self::SuspiciousHostname(_) => "suspicious_hostname",
            Self::DuplicateDomain { .. } => "duplicate_domain",
        }
    }
}

/// 一条问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsIssue {
    pub line: usize,
    pub level: IssueLevel,
    pub kind: IssueKind,
}

/// 统计（源仓 `stats` 五格）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostsStats {
    /// 总行数。
    pub total: usize,
    /// 生效记录数。
    pub active: usize,
    /// 注释行数（`#` 开头或整行只有注释）。
    pub comment: usize,
    /// 空行数。
    pub blank: usize,
    /// 去重后的域名数（大小写不敏感）。
    pub domains: usize,
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostsParseResult {
    pub entries: Vec<HostsEntry>,
    pub issues: Vec<HostsIssue>,
    pub stats: HostsStats,
}

fn ipv4_regex() -> &'static regex::Regex {
    static CELL: OnceLock<regex::Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        regex::Regex::new(
            r"^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$",
        )
        .expect("IPv4 pattern is a constant")
    })
}

fn ipv6_regex() -> &'static regex::Regex {
    static CELL: OnceLock<regex::Regex> = OnceLock::new();
    CELL.get_or_init(|| regex::Regex::new(r"^[0-9a-fA-F:]+$").expect("IPv6 pattern is a constant"))
}

fn hostname_regex() -> &'static regex::Regex {
    static CELL: OnceLock<regex::Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        regex::Regex::new(r"^[a-zA-Z0-9]([a-zA-Z0-9_.-]{0,251}[a-zA-Z0-9])?$")
            .expect("hostname pattern is a constant")
    })
}

/// IP 是否合法（IPv4 严格四段，IPv6 只做字符集 + 含冒号的宽松判定）。
///
/// 与源仓 `isValidIp` 完全一致：IPv6 那一支刻意宽松（`::1`、`fe80::1`、缩短写
/// 都过），因为 hosts 文件里出现完整展开的 IPv6 反而罕见，严格校验会把正确写法判错。
pub fn is_valid_ip(ip: &str) -> bool {
    if ipv4_regex().is_match(ip) {
        return true;
    }
    ip.contains(':') && ip.len() >= 2 && ipv6_regex().is_match(ip)
}

/// 解析一份 hosts 内容。
///
/// 逐行口径（与源仓 `parseHosts` 同）：
/// - 空行 / `#` 开头 → 计入 `blank` / `comment`，不产出记录；
/// - 行内 `#` 之后全部丢弃（`10.0.0.1 a  # 联调` 是合法写法）；
/// - 按空白切成 `IP 主机名...`，不足两项报 `MissingHostname`；
/// - 主机名大小写不敏感地查重，重复的**仍然产出记录**（源仓如此：hosts 里后面
///   那条不生效，但解析要如实呈现文件内容）。
pub fn parse_hosts(content: &str) -> HostsParseResult {
    let mut entries: Vec<HostsEntry> = Vec::new();
    let mut issues: Vec<HostsIssue> = Vec::new();
    let mut comment = 0usize;
    let mut blank = 0usize;
    let mut seen: Vec<(String, usize)> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (index, raw) in lines.iter().enumerate() {
        let line_no = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            blank += 1;
            continue;
        }
        if trimmed.starts_with('#') {
            comment += 1;
            continue;
        }
        let no_comment = raw.split('#').next().unwrap_or("").trim();
        if no_comment.is_empty() {
            comment += 1;
            continue;
        }
        let parts: Vec<&str> = no_comment.split_whitespace().collect();
        if parts.len() < 2 {
            issues.push(HostsIssue {
                line: line_no,
                level: IssueLevel::Error,
                kind: IssueKind::MissingHostname,
            });
            continue;
        }
        let ip = parts[0];
        if !is_valid_ip(ip) {
            issues.push(HostsIssue {
                line: line_no,
                level: IssueLevel::Error,
                kind: IssueKind::BadIp(ip.to_string()),
            });
        }
        let hostnames: Vec<String> = parts[1..].iter().map(|part| (*part).to_string()).collect();
        for host in &hostnames {
            if !hostname_regex().is_match(host) {
                issues.push(HostsIssue {
                    line: line_no,
                    level: IssueLevel::Warn,
                    kind: IssueKind::SuspiciousHostname(host.clone()),
                });
            }
            let lower = host.to_lowercase();
            match seen.iter().find(|(known, _)| *known == lower) {
                Some((_, first_line)) => issues.push(HostsIssue {
                    line: line_no,
                    level: IssueLevel::Warn,
                    kind: IssueKind::DuplicateDomain {
                        host: host.clone(),
                        first_line: *first_line,
                    },
                }),
                None => seen.push((lower, line_no)),
            }
        }
        entries.push(HostsEntry {
            line: line_no,
            ip: ip.to_string(),
            hostnames,
            raw: (*raw).to_string(),
        });
    }

    HostsParseResult {
        stats: HostsStats {
            total: lines.len(),
            active: entries.len(),
            comment,
            blank,
            domains: seen.len(),
        },
        entries,
        issues,
    }
}

/// 一个方案（源仓 `HostsProfile` 的内核侧裁剪件 + 文件化后的定位字段）。
///
/// 与源仓的差别只有两处，都是「插件没有 localStorage」的必然结果：
///
/// - 多一个 [`slug`](Self::slug)（文件名去扩展名）：源仓用 `id` 认方案，这里认文件；
///   报错、下拉、对比都靠它指人，比「方案名」可靠（名字可以重、可以带空格）。
/// - 少一个 `updatedAt`：文件系统的 mtime 就是它，没必要在正文里再抄一份。
///
/// `owner` / `ticket` / `desc` 保留 —— 在银行场景里这三项不是装饰：合并出来的
/// hosts 要进变更单，「谁改的、单号多少」得能追。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsProfile {
    /// 文件名（去扩展名），方案在这份清单里的唯一标识。
    pub slug: String,
    /// 方案名（缺省用文件名）。
    pub name: String,
    /// 环境标签（SIT / UAT / 生产……由调用方决定词表）。
    pub env: String,
    /// 负责人（`# owner:`）。
    pub owner: String,
    /// 变更单号（`# ticket:`）。
    pub ticket: String,
    /// 备注（`# desc:`）。
    pub desc: String,
    /// 是否参与合并（`# enabled: false` 停用）。
    pub enabled: bool,
    /// hosts 正文。
    pub content: String,
}

/// 合并后的一段（对应一个启用中的方案）。
///
/// 源仓 `mergeProfiles` 直接拼出一整份文本；这里只给**结构**，抬头与分隔线
/// 由插件层渲染 —— 那些是文案，不是数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedSection {
    pub name: String,
    pub env: String,
    /// 已 trim 的正文（方案正文为空时是空串，插件层据此决定要不要省略这一段）。
    pub content: String,
}

/// 按顺序把方案摊成合并段（**不做任何过滤**：启不启用是调用方的事）。
///
/// 源仓 `mergeProfiles` 里 `filter(p => p.enabled)` 那一步挪到调用方，
/// 因为插件轨道里「启用」来自参数而不是一份持久状态。
pub fn merge_sections(profiles: &[HostsProfile]) -> Vec<MergedSection> {
    profiles
        .iter()
        .map(|profile| MergedSection {
            name: profile.name.trim().to_string(),
            env: profile.env.trim().to_string(),
            content: profile.content.trim().to_string(),
        })
        .collect()
}

/// 两份内容的差异（按「去空白、去注释行」后的行集合算）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostsDiff {
    /// 只在 A 里有的行（顺序保持 A 里首次出现的顺序）。
    pub only_a: Vec<String>,
    /// 只在 B 里有的行。
    pub only_b: Vec<String>,
}

impl HostsDiff {
    pub fn is_empty(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty()
    }
}

/// 求差集（源仓 `diffContents`）。
///
/// 只比「生效行」：注释与空行不参与 —— 合并结果的抬头每次生成时间都不同，
/// 拿它算差异会让每一次比对都「有变化」，这个功能就废了。
pub fn diff_contents(a: &str, b: &str) -> HostsDiff {
    let set_a = effective_lines(a);
    let set_b = effective_lines(b);
    HostsDiff {
        only_a: set_a
            .iter()
            .filter(|line| !set_b.contains(line))
            .cloned()
            .collect(),
        only_b: set_b
            .iter()
            .filter(|line| !set_a.contains(line))
            .cloned()
            .collect(),
    }
}

/// 去注释、去空行、trim 后的行（保序去重）。
fn effective_lines(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !out.iter().any(|known| known == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// 一条跨方案查找命中（源仓 `searchResult` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// 方案下标（调用方拿它回指 [`HostsProfile`]）。
    pub profile: usize,
    /// 行号（从 1 开始）。
    pub line: usize,
    /// 命中行的 trim 结果。
    pub text: String,
}

/// 跨方案查找（源仓 `runSearch`）：大小写不敏感的子串匹配，**注释行也算**。
///
/// 注释行必须算 —— 「这条映射被我注释掉了」正是最需要被找出来的状态。
pub fn search_profiles(profiles: &[HostsProfile], keyword: &str) -> Vec<SearchHit> {
    let needle = keyword.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (index, profile) in profiles.iter().enumerate() {
        for (offset, line) in profile.content.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                hits.push(SearchHit {
                    profile: index,
                    line: offset + 1,
                    text: line.trim().to_string(),
                });
            }
        }
    }
    hits
}

/// 一个方案被替换后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceOutcome {
    pub profile: usize,
    /// 替换掉的处数。
    pub count: usize,
    /// 替换后的完整正文（**未改动方案本身**：写不写盘由调用方决定）。
    pub content: String,
}

/// 跨方案批量替换（源仓 `runReplaceAll`）：**纯字面量**替换，大小写敏感。
///
/// 与参考实现一致的三点：字面量而非正则（`.*` 不该被当通配符）；大小写敏感
/// （域名本来就大小写不敏感，替换时不动大小写是更安全的选择）；只返回**确实
/// 发生替换**的方案 —— 没命中的方案不产出条目，调用方不用再过滤一遍。
pub fn replace_in_profiles(profiles: &[HostsProfile], from: &str, to: &str) -> Vec<ReplaceOutcome> {
    if from.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, profile) in profiles.iter().enumerate() {
        let count = profile.content.matches(from).count();
        if count == 0 {
            continue;
        }
        out.push(ReplaceOutcome {
            profile: index,
            count,
            content: profile.content.replace(from, to),
        });
    }
    out
}

/// 按 slug（优先）或方案名找方案，大小写不敏感。
///
/// 源仓的「方案对比」是两个下拉框（有 `id` 就有列表）；插件拿不到方案列表去渲染
/// 下拉，只能让用户手写文件名 —— 所以这里对两种写法都认，写错时调用方能把
/// 现有 slug 全列出来。
pub fn find_profile<'a>(profiles: &'a [HostsProfile], key: &str) -> Option<&'a HostsProfile> {
    let key = key.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    profiles
        .iter()
        .find(|profile| profile.slug.to_lowercase() == key)
        .or_else(|| {
            profiles
                .iter()
                .find(|profile| profile.name.to_lowercase() == key)
        })
}

/// 应用命令的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyCommand {
    /// 稳定 id：`backup` / `apply` / `flush`（插件层按它给注释行）。
    pub id: &'static str,
    /// 可以直接粘进终端的一行命令。
    pub command: String,
}

/// 两个平台的应用脚本（**只有命令，没有注释**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyScript {
    pub mac_linux: Vec<ApplyCommand>,
    pub windows: Vec<ApplyCommand>,
}

/// 生成「用户自己执行」的应用命令（源仓 `buildApplyScript`）。
///
/// 三条命令的顺序与源仓一致：**先备份**，再覆盖，最后刷 DNS。备份那一行带时间戳，
/// 所以连点两次也不会覆盖掉上一次的备份。
///
/// `file_name` 是插件导出的合并文件名（形如 `hosts.merged`）：
/// macOS/Linux 假设它落在用户家目录，Windows 假设在 `Downloads` —— 与源仓同假设，
/// 因为插件无法预知用户的下载目录。
pub fn build_apply_script(file_name: &str) -> ApplyScript {
    let file_name = file_name.trim();
    let file_name = if file_name.is_empty() {
        "hosts.merged"
    } else {
        file_name
    };
    ApplyScript {
        mac_linux: vec![
            ApplyCommand {
                id: "backup",
                command: "sudo cp /etc/hosts /etc/hosts.bak_$(date +%Y%m%d_%H%M%S)".to_string(),
            },
            ApplyCommand {
                id: "apply",
                command: format!("sudo cp ~/{file_name} /etc/hosts"),
            },
            ApplyCommand {
                id: "flush",
                command:
                    "sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder".to_string(),
            },
        ],
        windows: vec![
            ApplyCommand {
                id: "backup",
                command: r"copy C:\Windows\System32\drivers\etc\hosts C:\Windows\System32\drivers\etc\hosts.bak".to_string(),
            },
            ApplyCommand {
                id: "apply",
                command: format!(
                    r"copy /Y %USERPROFILE%\Downloads\{file_name} C:\Windows\System32\drivers\etc\hosts"
                ),
            },
            ApplyCommand {
                id: "flush",
                command: "ipconfig /flushdns".to_string(),
            },
        ],
    }
}

/// 方案文件里的元信息（`# name:` / `# env:` / `# owner:` / `# ticket:` /
/// `# desc:` / `# enabled:`）。
///
/// 源仓把这些存在 localStorage 里；插件没有状态，只能随文件走。
/// 约定：文件**开头连续的注释行**里找，找到第一处就算 —— 正文里的同名注释不参与
///（否则一条 `# env: prod` 的样例行会把整个方案的环境带跑）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileMeta {
    pub name: Option<String>,
    pub env: Option<String>,
    pub owner: Option<String>,
    pub ticket: Option<String>,
    pub desc: Option<String>,
    /// `# enabled: false` —— 停用这个方案（不参与合并，但仍然参与校验与查找）。
    pub enabled: Option<bool>,
}

/// 从方案正文里读元信息。
pub fn parse_profile_meta(content: &str) -> ProfileMeta {
    let mut meta = ProfileMeta::default();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 只认开头这一片注释；遇到第一条非注释行就停。
        let Some(comment) = trimmed.strip_prefix('#') else {
            break;
        };
        // 没有冒号的注释是普通说明（`# 联调环境`），跳过但继续往下看。
        let Some((key, value)) = comment.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "name" if meta.name.is_none() => meta.name = Some(value.to_string()),
            "env" if meta.env.is_none() => meta.env = Some(value.to_string()),
            "owner" if meta.owner.is_none() => meta.owner = Some(value.to_string()),
            "ticket" if meta.ticket.is_none() => meta.ticket = Some(value.to_string()),
            "desc" if meta.desc.is_none() => meta.desc = Some(value.to_string()),
            "enabled" if meta.enabled.is_none() => meta.enabled = parse_enabled(value),
            _ => {}
        }
    }
    meta
}

/// `# enabled:` 的取值。**只认 ASCII 词**：内核 crate 不许出现中文（漏译闸门），
/// 而中文别名（`停用`/`启用`）在插件层也没有必要 —— 这个标记是给「写文件的人」
/// 看的，`false` 是最短、最不容易写错的写法。
fn parse_enabled(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "enable" | "enabled" => Some(true),
        "false" | "0" | "no" | "off" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// 写入前的校验：空内容与超限都拒绝（口径见模块头）。
pub fn validate_hosts_content(content: &str) -> NetResult<()> {
    if content.trim().is_empty() {
        return Err(NetError::new(ErrorKind::EmptyInput));
    }
    let size = content.len();
    if size > MAX_HOSTS_BYTES {
        return Err(NetError::with_detail(
            ErrorKind::TooLarge,
            format!("{size}/{MAX_HOSTS_BYTES}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# 联调环境
10.0.0.1   app.local api.local
10.0.0.2   db.local   # 数据库
# 10.0.0.3 disabled.local
invalid-ip host.local
10.0.0.4
::1        ipv6.local
10.0.0.5   app.local
10.0.0.6   bad_host!.local
";

    #[test]
    fn parses_entries_and_counts_like_the_reference() {
        let parsed = parse_hosts(SAMPLE);
        assert_eq!(parsed.stats.total, 9);
        assert_eq!(parsed.stats.active, 6);
        // 注释行只有开头那一条与 `# 10.0.0.3 …` 那一条：
        // `10.0.0.2 db.local # 数据库` 是**生效行 + 行尾注释**，不算注释行。
        assert_eq!(parsed.stats.comment, 2);
        assert_eq!(parsed.stats.blank, 0);
        // app.local / api.local / db.local / ipv6.local / bad_host!.local / host.local
        // （最后那个来自 `invalid-ip host.local`：IP 非法**不影响**它被计入域名统计 ——
        // 源仓先 push 记录再报问题，解析结果如实呈现文件内容）
        assert_eq!(parsed.stats.domains, 6);
        // 行内注释不参与切词。
        assert_eq!(
            parsed.entries[1].hostnames,
            vec!["db.local".to_string()],
            "{:?}",
            parsed.entries[1]
        );
        assert_eq!(parsed.entries[1].line, 3);
    }

    #[test]
    fn reports_the_five_issue_kinds() {
        let parsed = parse_hosts(SAMPLE);
        let ids: Vec<&str> = parsed.issues.iter().map(|issue| issue.kind.id()).collect();
        assert!(ids.contains(&"bad_ip"), "{ids:?}");
        assert!(ids.contains(&"missing_hostname"), "{ids:?}");
        assert!(ids.contains(&"duplicate_domain"), "{ids:?}");
        assert!(ids.contains(&"suspicious_hostname"), "{ids:?}");
        // 重复域名要指回首次出现的行号。
        let duplicate = parsed
            .issues
            .iter()
            .find(|issue| issue.kind.id() == "duplicate_domain")
            .expect("应有重复");
        assert_eq!(
            duplicate.kind,
            IssueKind::DuplicateDomain {
                host: "app.local".to_string(),
                first_line: 2
            }
        );
        assert_eq!(duplicate.level, IssueLevel::Warn);
        // 非法 IP 是 error（这一行不生效）。
        let bad = parsed
            .issues
            .iter()
            .find(|issue| issue.kind.id() == "bad_ip")
            .expect("应有非法 IP");
        assert_eq!(bad.level, IssueLevel::Error);
        assert_eq!(bad.kind, IssueKind::BadIp("invalid-ip".to_string()));
    }

    #[test]
    fn comments_after_hash_are_ignored_but_counted_as_entries() {
        let parsed = parse_hosts("10.0.0.1 a.local # note\n   # only comment\n\n");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.stats.comment, 1);
        assert_eq!(parsed.stats.blank, 1);
        assert!(parsed.issues.is_empty(), "{:?}", parsed.issues);
    }

    #[test]
    fn is_valid_ip_accepts_the_loose_ipv6_form() {
        assert!(is_valid_ip("10.0.0.1"));
        assert!(is_valid_ip("255.255.255.255"));
        assert!(!is_valid_ip("256.0.0.1"));
        assert!(!is_valid_ip("10.0.0"));
        assert!(is_valid_ip("::1"));
        assert!(is_valid_ip("fe80::1"));
        // `::`（未指定地址）**算合法** —— 这是源仓 `isValidIp` 的既有行为
        //（`contains(':') && IPV6_RE && length >= 2` 三条都过），复刻件不悄悄收紧：
        // 收紧会让「源仓能用、本仓报错」的输入出现，而 hosts 里出现 `::` 本来就极罕见。
        assert!(is_valid_ip("::"));
        assert!(!is_valid_ip("not-an-ip"));
    }

    #[test]
    fn merge_keeps_order_and_trims_bodies() {
        let sections = merge_sections(&[
            profile("a-sit", " 张三-联调 ", "SIT", "\n  10.0.0.1 a.local  \n"),
            profile("b-empty", "空方案", "", "   "),
        ]);
        assert_eq!(sections[0].name, "张三-联调");
        assert_eq!(sections[0].content, "10.0.0.1 a.local");
        // 空正文原样交出去（trim 成空串），由插件层决定要不要省略这一段。
        assert_eq!(sections[1].content, "");
    }

    /// 测试用的方案构造：只关心 slug / 名字 / 环境 / 正文，其余字段给默认值。
    fn profile(slug: &str, name: &str, env: &str, content: &str) -> HostsProfile {
        HostsProfile {
            slug: slug.to_string(),
            name: name.to_string(),
            env: env.to_string(),
            owner: String::new(),
            ticket: String::new(),
            desc: String::new(),
            enabled: true,
            content: content.to_string(),
        }
    }

    #[test]
    fn diff_ignores_comments_and_blank_lines() {
        let a = "# 抬头\n10.0.0.1 a.local\n\n10.0.0.2 b.local\n";
        let b = "# 另一个抬头\n10.0.0.1 a.local\n10.0.0.3 c.local\n";
        let diff = diff_contents(a, b);
        assert_eq!(diff.only_a, vec!["10.0.0.2 b.local".to_string()]);
        assert_eq!(diff.only_b, vec!["10.0.0.3 c.local".to_string()]);
        assert!(!diff.is_empty());
        assert!(diff_contents(a, a).is_empty());
    }

    #[test]
    fn apply_script_backs_up_before_overwriting() {
        let script = build_apply_script("hosts.merged");
        let ids: Vec<&str> = script.mac_linux.iter().map(|step| step.id).collect();
        assert_eq!(ids, vec!["backup", "apply", "flush"]);
        assert!(script.mac_linux[0].command.contains("hosts.bak_$(date"));
        assert!(script.mac_linux[1].command.contains("~/hosts.merged"));
        let windows: Vec<&str> = script.windows.iter().map(|step| step.id).collect();
        assert_eq!(windows, vec!["backup", "apply", "flush"]);
        assert!(script.windows[1].command.contains("hosts.merged"));
        // 空文件名回落到默认名，避免生成 `~/ /etc/hosts` 这种废命令。
        assert_eq!(
            build_apply_script("  ").mac_linux[1].command,
            "sudo cp ~/hosts.merged /etc/hosts"
        );
    }

    #[test]
    fn profile_meta_reads_only_the_leading_comment_block() {
        let meta =
            parse_profile_meta("# name: 张三-联调\n# env: SIT\n10.0.0.1 a.local\n# env: PROD\n");
        assert_eq!(meta.name.as_deref(), Some("张三-联调"));
        assert_eq!(meta.env.as_deref(), Some("SIT"));

        let meta = parse_profile_meta("10.0.0.1 a.local\n# env: PROD\n");
        assert_eq!(meta.env, None, "正文后面的注释不参与");

        assert_eq!(parse_profile_meta("").name, None);
    }

    #[test]
    fn profile_meta_carries_provenance_and_the_enabled_flag() {
        let meta = parse_profile_meta(
            "# 联调环境（没有冒号的注释不影响下面的解析）\n\
             # name: 张三-联调\n\
             # env: SIT\n\
             # owner: 实施组\n\
             # ticket: CHG-2026-0001\n\
             # desc: 联调网关与应用 VIP\n\
             # enabled: false\n\
             10.0.0.1 a.local\n",
        );
        assert_eq!(meta.name.as_deref(), Some("张三-联调"));
        assert_eq!(meta.owner.as_deref(), Some("实施组"));
        assert_eq!(meta.ticket.as_deref(), Some("CHG-2026-0001"));
        assert_eq!(meta.desc.as_deref(), Some("联调网关与应用 VIP"));
        assert_eq!(meta.enabled, Some(false));

        // 大小写不敏感，词形也放宽一点（yes/no、1/0）。
        assert_eq!(parse_profile_meta("# ENABLED: No\n").enabled, Some(false));
        assert_eq!(parse_profile_meta("# enabled: 1\n").enabled, Some(true));
        // 认不出来的值不当成「停用」：宁可多合并一个方案，也不要静默少一段映射。
        assert_eq!(parse_profile_meta("# enabled: 也许\n").enabled, None);
        // 空值一律忽略（`# owner:` 后面什么都没写）。
        assert_eq!(parse_profile_meta("# owner:\n").owner, None);
        // 同一个键写两次，取第一次。
        assert_eq!(
            parse_profile_meta("# env: SIT\n# env: PROD\n")
                .env
                .as_deref(),
            Some("SIT")
        );
    }

    #[test]
    fn search_hits_every_profile_including_commented_lines() {
        let profiles = [
            profile(
                "a-sit",
                "张三-联调",
                "SIT",
                "10.0.0.1 api.sit.bank\n# api.sit.bank 旧地址\n",
            ),
            profile("b-uat", "UAT", "UAT", "10.1.0.1 api.uat.bank\n"),
        ];
        let hits = search_profiles(&profiles, "API.SIT");
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].profile, 0);
        assert_eq!(hits[0].line, 1);
        // 被注释掉的那条也要能找出来（「我什么时候注释掉的」是常见疑问）。
        assert_eq!(hits[1].line, 2);
        assert_eq!(hits[1].text, "# api.sit.bank 旧地址");
        // 空关键词不扫（否则等于把整个目录打出来）。
        assert!(search_profiles(&profiles, "   ").is_empty());
        assert!(search_profiles(&profiles, "zzz").is_empty());
    }

    #[test]
    fn replace_counts_literal_occurrences_and_leaves_the_source_alone() {
        let profiles = [
            profile(
                "a-sit",
                "A",
                "SIT",
                "10.0.0.1 a.sit.bank b.sit.bank\n# a.sit.bank\n",
            ),
            profile("b-uat", "B", "UAT", "10.1.0.1 other.bank\n"),
        ];
        let outcomes = replace_in_profiles(&profiles, "sit.bank", "uat.bank");
        assert_eq!(outcomes.len(), 1, "没命中的方案不产出条目");
        assert_eq!(outcomes[0].profile, 0);
        assert_eq!(outcomes[0].count, 3);
        assert!(
            outcomes[0]
                .content
                .contains("10.0.0.1 a.uat.bank b.uat.bank")
        );
        assert!(outcomes[0].content.contains("# a.uat.bank"), "注释里的也换");
        // 纯字面量：`.` 不是通配符。
        let outcomes = replace_in_profiles(&profiles, "a.sit", "x");
        assert_eq!(outcomes[0].count, 2);
        // 源方案一个字都没动。
        assert!(profiles[0].content.contains("a.sit.bank"));
        // 空 from 不是「全删」，是什么都不做。
        assert!(replace_in_profiles(&profiles, "", "x").is_empty());
    }

    #[test]
    fn find_profile_prefers_slug_and_falls_back_to_the_name() {
        let profiles = [
            profile("a-sit", "张三-联调", "SIT", ""),
            profile("b-uat", "李四-UAT", "UAT", ""),
        ];
        assert_eq!(
            find_profile(&profiles, "b-uat").map(|p| p.slug.as_str()),
            Some("b-uat")
        );
        assert_eq!(
            find_profile(&profiles, "A-SIT").map(|p| p.slug.as_str()),
            Some("a-sit")
        );
        // 名字也能找（带空格、带中文都行）。
        assert_eq!(
            find_profile(&profiles, "李四-UAT").map(|p| p.slug.as_str()),
            Some("b-uat")
        );
        assert!(find_profile(&profiles, "nope").is_none());
        assert!(find_profile(&profiles, "  ").is_none());
    }

    #[test]
    fn writing_is_rejected_when_empty_or_too_large() {
        assert_eq!(
            validate_hosts_content("  \n ").unwrap_err().kind(),
            ErrorKind::EmptyInput
        );
        let huge = "x".repeat(MAX_HOSTS_BYTES + 1);
        assert_eq!(
            validate_hosts_content(&huge).unwrap_err().kind(),
            ErrorKind::TooLarge
        );
        assert!(validate_hosts_content("10.0.0.1 a.local").is_ok());
    }
}
