//! `~/.ssh/known_hosts` 的**只读**查询：这条主机在不在用户的信任库里。
//!
//! # 定位（重要）
//!
//! 本模块**只读、只报事实，不写库、不改连接参数**。原因见下节 —— 这是有意的设计
//! 取舍，不是没做完：
//!
//! 本仓走的是「驱动系统 `ssh`」（见 crate 文档），主机密钥校验因此由 OpenSSH 自己
//! 按**用户自己的** `StrictHostKeyChecking` / `known_hosts` 配置执行，是 fail-closed。
//! 应用再插一层「首次确认 → 写入 `~/.ssh/known_hosts`」会与那份配置**打架**：
//!
//! - 写的是 `ssh` 自己拥有的文件（含哈希化格式、多密钥类型、`@cert-authority`
//!   等本模块解析不了的形式），写坏一次就是用户所有的已知主机记录一起失效；
//! - 「主机密钥变了要拦住连接」本来就是 `StrictHostKeyChecking=yes` 的职责，
//!   应用抢过来做等于**覆盖用户的选择**（有人故意设 `no` 来连刚重建的测试机）；
//! - 拿指纹得先起一条**未经认证**的连接（`ssh-keyscan`），那个窗口本身就是 MITM
//!   的机会 —— 把「TOFU 的第一次」提前到一条额外的、不受 `known_hosts` 保护的连接上。
//!
//! 所以这里提供的能力是**只读探测**（`status`）：界面可以据此在连接前提示
//! 「这台主机还没有记录」，但**是否继续、要不要记住**仍由 OpenSSH 与用户决定。
//! 完整取舍与备选方案见交付报告。
//!
//! # 能识别的格式
//!
//! 逐行按 OpenSSH 的 `known_hosts` 语法解析：
//!
//! ```text
//! [@marker] hostname[,hostname...] keytype base64-key [comment]
//! ```
//!
//! - 空行与 `#` 注释跳过；
//! - 主机名可以是 `host`（**只匹配默认端口 22**）、`[host]:port`、含 `*` / `?`
//!   的通配模式，以及 `!` 开头的否定模式（命中即整行不作数）；
//! - `@revoked` 记为 [`HostKeyStatus::Revoked`]；`@cert-authority` 视为**不匹配**
//!   （它描述的是 CA，不是这台主机的密钥）；
//! - **哈希化主机名**（`|1|salt|hash`）无法在没有 HMAC-SHA1 的前提下反查，
//!   一律记 [`HostKeyStatus::Unknown`]（本 crate 零依赖，不引哈希实现）。

use std::path::{Path, PathBuf};

/// 某台主机在 `known_hosts` 里的记录状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 库里没有这台主机（首次连接的 TOFU 窗口）。
    ///
    /// 注意：**哈希化的记录也会落在这里** —— 那种记录的hostname 反查不出来
    ///（见模块头）。
    Unknown,
    /// 库里有记录；列出该主机已记录的公钥类型（`ssh-ed25519` / `ssh-rsa` …）。
    Known {
        /// 已记录的公钥类型，按文件出现顺序去重。
        key_types: Vec<String>,
    },
    /// 库里的记录带 `@revoked` 标记 —— 该主机的密钥被显式吊销。
    Revoked,
}

/// 默认的 `known_hosts` 路径：`$HOME/.ssh/known_hosts`。
///
/// 拿不到 `$HOME` 时返回 `None`（不猜一个相对路径出来 —— 那会读到一个
/// 与用户实际使用的完全无关的文件）。
#[must_use]
pub fn known_hosts_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".ssh").join("known_hosts"))
}

/// 查一条主机在本机 `known_hosts` 里的状态。
///
/// 文件不存在（还没连过任何主机）时返回 [`HostKeyStatus::Unknown`]；
/// 读失败（权限等）同样返回 `Unknown` 而不是报错 —— 调用方拿到的是「查不到」，
/// 而「查不到」与「没有记录」在本模块的语义里都是「不下结论」。
#[must_use]
pub fn status(host: &str, port: u16) -> HostKeyStatus {
    let Some(path) = known_hosts_path() else {
        return HostKeyStatus::Unknown;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HostKeyStatus::Unknown;
    };
    status_in(&content, host, port)
}

/// [`status`] 的纯函数形态：在给定的 `known_hosts` **内容**上查询。
///
/// 拆出来是为了让「哪一行算命中」这件事能被单测钉住 —— 线上读的是用户真实的
/// `~/.ssh/known_hosts`，测试不能依赖它。
#[must_use]
pub fn status_in(content: &str, host: &str, port: u16) -> HostKeyStatus {
    let mut key_types: Vec<String> = Vec::new();
    let mut revoked = false;
    for line in content.lines() {
        let Some((marker, fields)) = split_line(line) else {
            continue;
        };
        let mut parts = fields.split(' ');
        let hostnames = parts.next().unwrap_or_default();
        let key_type = parts.next().unwrap_or_default();
        // 密钥本体必须存在，否则这行是坏记录：半行记录不能证明任何事。
        if parts.next().is_none() {
            continue;
        }
        // 哈希化主机名：反查不出来，跳过（见模块头）。
        if hostnames.starts_with('|') {
            continue;
        }
        // `@cert-authority` 描述的是 CA 而不是这台主机的密钥，不算命中。
        if marker == Some("@cert-authority") {
            continue;
        }
        if !hostnames_match(hostnames, host, port) {
            continue;
        }
        if marker == Some("@revoked") {
            revoked = true;
            continue;
        }
        let key_type = key_type.to_owned();
        if !key_types.contains(&key_type) {
            key_types.push(key_type);
        }
    }
    if revoked {
        return HostKeyStatus::Revoked;
    }
    if key_types.is_empty() {
        HostKeyStatus::Unknown
    } else {
        HostKeyStatus::Known { key_types }
    }
}

/// 拆一行：返回 `(marker, 其余字段)`。空行 / 注释 / 字段不足时返回 `None`。
fn split_line(line: &str) -> Option<(Option<&str>, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // 标记只可能是 `@revoked` / `@cert-authority` 两种，未知标记按坏行处理：
    // 将来 OpenSSH 新增标记时，宁可漏一条记录，也不要把它的第一个字段当主机名。
    if let Some(rest) = line.strip_prefix('@') {
        let (marker, rest) = rest.split_once(' ')?;
        if marker != "revoked" && marker != "cert-authority" {
            return None;
        }
        let marker: &'static str = if marker == "revoked" {
            "@revoked"
        } else {
            "@cert-authority"
        };
        return Some((Some(marker), rest.trim_start()));
    }
    Some((None, line))
}

/// 逗号分隔的主机名模式里，有没有一条命中 `host:port`。
///
/// 否定模式（`!` 开头）命中时整行不作数 —— 与 OpenSSH 的 `mapproc` 同语义。
fn hostnames_match(hostnames: &str, host: &str, port: u16) -> bool {
    let mut hit = false;
    for pattern in hostnames.split(',') {
        let pattern = pattern.trim();
        if let Some(negated) = pattern.strip_prefix('!') {
            if single_match(negated, host, port) {
                return false;
            }
            continue;
        }
        if single_match(pattern, host, port) {
            hit = true;
        }
    }
    hit
}

/// 单条模式：`[host]:port` 带端口，`host` 只匹配默认端口 22。
fn single_match(pattern: &str, host: &str, port: u16) -> bool {
    if let Some(rest) = pattern.strip_prefix('[') {
        let Some((inner, tail)) = rest.rsplit_once("]:") else {
            return false;
        };
        return tail.parse::<u16>().ok() == Some(port) && glob(inner, host);
    }
    port == 22 && glob(pattern, host)
}

/// 简易 glob：`*` 匹配任意串、`?` 匹配单个字符；其余字符逐字比较。
///
/// 大小写不敏感（DNS 主机名不区分大小写）。不处理 `\` 转义 —— 那需要与
/// OpenSSH 的转义表逐条对齐，而本模块只做只读提示，漏掉一条通配记录只会让
/// 提示偏保守（说「没有记录」而不是误报「有记录」）。
fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let text: Vec<char> = text.to_ascii_lowercase().chars().collect();
    glob_match(&pattern, &text)
}

/// `glob` 的递归核（模式与文本都已归一化）。
fn glob_match(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => {
            // `*` 可以吃掉 0..=len 个字符。
            (0..=text.len()).any(|skip| glob_match(rest, &text[skip..]))
        }
        Some(('?', rest)) => match text.split_first() {
            Some((_, tail)) => glob_match(rest, tail),
            None => false,
        },
        Some((head, rest)) => match text.split_first() {
            Some((first, tail)) if first == head => glob_match(rest, tail),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_entry_only_matches_the_default_port() {
        let content = "10.0.1.11 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA\n";
        // 无端口的记录 = 默认 22。
        assert_eq!(
            status_in(content, "10.0.1.11", 22),
            HostKeyStatus::Known {
                key_types: vec!["ssh-ed25519".to_owned()]
            }
        );
        // 换个端口就不算命中（OpenSSH 的语义：那是另一台「主机」）。
        assert_eq!(
            status_in(content, "10.0.1.11", 2222),
            HostKeyStatus::Unknown
        );
        // 换个主机也不行。
        assert_eq!(status_in(content, "10.0.1.12", 22), HostKeyStatus::Unknown);
    }

    #[test]
    fn a_bracketed_entry_matches_its_own_port() {
        let content = "[example.com]:2222 ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB\n";
        assert_eq!(
            status_in(content, "example.com", 2222),
            HostKeyStatus::Known {
                key_types: vec!["ssh-rsa".to_owned()]
            }
        );
        assert_eq!(
            status_in(content, "example.com", 22),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn several_key_types_are_collected_once_each() {
        let content = "\
host.example ssh-ed25519 AAAA1
host.example ssh-rsa AAAA2
host.example ssh-ed25519 AAAA3
";
        assert_eq!(
            status_in(content, "host.example", 22),
            HostKeyStatus::Known {
                key_types: vec!["ssh-ed25519".to_owned(), "ssh-rsa".to_owned()]
            }
        );
    }

    #[test]
    fn comments_blanks_and_broken_lines_are_ignored() {
        let content = "\
# 这是注释

host.example ssh-ed25519
not-a-host-key-line
host.example ssh-ed25519 AAAA1
";
        // 「ssh-ed25519」那行缺密钥本体，是坏记录：不能凭它说「有记录」。
        assert_eq!(
            status_in(content, "host.example", 22),
            HostKeyStatus::Known {
                key_types: vec!["ssh-ed25519".to_owned()]
            }
        );
    }

    #[test]
    fn wildcards_and_negations_follow_openssh_semantics() {
        let content = "*.internal,10.0.0.? ssh-ed25519 AAAA1\n";
        assert!(matches!(
            status_in(content, "web-1.internal", 22),
            HostKeyStatus::Known { .. }
        ));
        assert!(matches!(
            status_in(content, "10.0.0.7", 22),
            HostKeyStatus::Known { .. }
        ));
        assert_eq!(status_in(content, "10.0.0.70", 22), HostKeyStatus::Unknown);

        // 否定模式命中 → 整行不作数。
        let negated = "*.internal,!bad.internal ssh-ed25519 AAAA1\n";
        assert_eq!(
            status_in(negated, "bad.internal", 22),
            HostKeyStatus::Unknown
        );
        assert!(matches!(
            status_in(negated, "ok.internal", 22),
            HostKeyStatus::Known { .. }
        ));
    }

    #[test]
    fn revoked_entries_are_reported_as_revoked() {
        let content = "@revoked host.example ssh-ed25519 AAAA1\n";
        assert_eq!(
            status_in(content, "host.example", 22),
            HostKeyStatus::Revoked
        );
        // 吊销是**整台主机**的结论：即使文件里还有一条正常记录也不能说「可信」。
        let mixed = "\
@revoked host.example ssh-ed25519 AAAA1
host.example ssh-rsa AAAA2
";
        assert_eq!(status_in(mixed, "host.example", 22), HostKeyStatus::Revoked);
    }

    #[test]
    fn hashed_and_cert_authority_entries_never_report_known() {
        // 哈希化主机名反查不出来：宁可说「没有记录」。
        let hashed =
            "|1|F1E1KeoE/eEWhi10WpGv4OdiO6Y=|3988QV0VE8wmZL7suNrYQLITLCg= ssh-ed25519 AAAA1\n";
        assert_eq!(
            status_in(hashed, "host.example", 22),
            HostKeyStatus::Unknown
        );
        // `@cert-authority` 描述的是 CA，不是这台主机的密钥。
        let ca = "@cert-authority *.example ssh-rsa AAAA1\n";
        assert_eq!(status_in(ca, "host.example", 22), HostKeyStatus::Unknown);
    }

    #[test]
    fn an_unknown_marker_is_treated_as_a_broken_line() {
        // 将来的新标记：宁可漏一条，也不要把标记当成主机名去匹配。
        let content = "@something-new host.example ssh-ed25519 AAAA1\n";
        assert_eq!(
            status_in(content, "@something-new", 22),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            status_in(content, "host.example", 22),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn empty_content_is_unknown() {
        assert_eq!(status_in("", "host.example", 22), HostKeyStatus::Unknown);
    }
}
