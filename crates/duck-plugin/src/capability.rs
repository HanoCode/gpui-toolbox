//! 能力网关：声明 + 授予双闸门，默认拒绝。
//!
//! 模型对齐 zed 的 `capability_granter`（manifest 声明 + 宿主授予，任一缺失即拒绝）
//! 与 deepseek 分享文档的三层模型：插件在清单里**声明**，宿主在装载时**授予**，
//! 每个宿主函数/资源访问执行前**仲裁**。`Capabilities::default()` 为空集、
//! [`CapabilityGranter::new`] 为空授权——未声明且未授予的能力一律不可访问。

use std::collections::HashSet;

use crate::PluginError;
use crate::manifest::Capabilities;

/// 一个可仲裁的能力单元（比 [`Capabilities`] 粒度更细：每条路径 / 每个主机一条）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    /// 读某个 glob 模式。
    FilesystemRead(String),
    /// 写某个 glob 模式。
    FilesystemWrite(String),
    /// 访问某个网络主机。
    NetworkHost(String),
    /// 发送系统通知。
    Notifications,
    /// 执行某个外部命令。
    ProcessExec(String),
}

impl Capability {
    /// 从清单的声明展开成能力单元集合（每条 glob / 主机 / 命令各一条）。
    #[must_use]
    pub fn set_of(caps: &Capabilities) -> HashSet<Self> {
        let mut set = HashSet::new();
        for pattern in &caps.filesystem_read {
            set.insert(Self::FilesystemRead(pattern.clone()));
        }
        for pattern in &caps.filesystem_write {
            set.insert(Self::FilesystemWrite(pattern.clone()));
        }
        for host in &caps.network_hosts {
            set.insert(Self::NetworkHost(host.clone()));
        }
        if caps.notifications {
            set.insert(Self::Notifications);
        }
        for command in &caps.process_exec {
            set.insert(Self::ProcessExec(command.clone()));
        }
        set
    }
}

/// 声明之间的关系运算：**与用户授予求交**，以及实现它的「只留这些单元」。
///
/// 放在这个文件里是有意的：[`Capability::set_of`]（声明 → 单元）与 `only`（单元 → 声明）
/// 是一对互逆的映射，分开写就迟早对不上——`duck-plugin` 的五个字段一共只有五处，
/// 但任何一处漏掉都会让某类能力**永远授予不了**（`only` 里漏了 `process_exec`，
/// 就是「确认框勾了也跑不起来」），而这类静默失败极难从界面上看出来。
impl Capabilities {
    /// 只保留 `allowed` 里的能力单元，**保留 `self` 的顺序**。
    ///
    /// 顺序必须留在原地：这一份结果既拿去仲裁，也拿去渲染安装确认框与详情页的
    /// **逐条**列表（见 `capability_items`），而 `set_of` 出来的是一个集合，顺序早丢了。
    #[must_use]
    pub fn only(&self, allowed: &HashSet<Capability>) -> Self {
        let keep = |units: &[String], wrap: fn(String) -> Capability| -> Vec<String> {
            units
                .iter()
                .filter(|unit| allowed.contains(&wrap((*unit).clone())))
                .cloned()
                .collect()
        };
        Self {
            filesystem_read: keep(&self.filesystem_read, Capability::FilesystemRead),
            filesystem_write: keep(&self.filesystem_write, Capability::FilesystemWrite),
            network_hosts: keep(&self.network_hosts, Capability::NetworkHost),
            notifications: self.notifications && allowed.contains(&Capability::Notifications),
            process_exec: keep(&self.process_exec, Capability::ProcessExec),
        }
    }

    /// 与 `other` 的**交集**：`self` 是插件声明（能力上限），`other` 是用户授予。
    ///
    /// 两边缺一不可——声明是插件自己写的，授予是用户点的头；只认声明就是「声明即授权」，
    /// 只认授予则会让「没记录 ＝ 全给」这条缺省（见 `duck-app` 的 `plugin_caps::<id>`）
    /// 变成「给了没声明的能力」。结果保留 `self` 的顺序。
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        self.only(&Capability::set_of(other))
    }
}

/// 能力网关：一个插件实例一份，持有**已授予**的能力集合。
///
/// 空集起步（默认拒绝）；宿主函数在真正触达资源前调 [`Self::require`]。
pub struct CapabilityGranter {
    granted: HashSet<Capability>,
}

impl Default for CapabilityGranter {
    fn default() -> Self {
        Self::new()
    }
}

/// 极简 glob 匹配（能力路径用）：`**` 跨任意段，`*` 单段内任意，`?` 单字符；
/// `/` 是段分隔符，`*` / `?` 不跨段。纯函数、确定性，配合单测锁定语义。
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    let path = path.trim_start_matches("./");
    glob_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_inner(pattern: &[u8], path: &[u8]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            let star = if pattern.get(1) == Some(&b'*') { 2 } else { 1 };
            let crosses = star == 2;
            // 尝试让星号吃掉 0..n 个字符；`*` 吃到的不能是段分隔符。
            for taken in 0..=path.len() {
                if !crosses && path[..taken].contains(&b'/') {
                    break;
                }
                if glob_inner(&pattern[star..], &path[taken..]) {
                    return true;
                }
            }
            false
        }
        (Some(b'?'), Some(&c)) if c != b'/' => glob_inner(&pattern[1..], &path[1..]),
        (Some(a), Some(b)) if a == b => glob_inner(&pattern[1..], &path[1..]),
        _ => false,
    }
}

/// 把主机名归一化：剥掉 IPv6 方括号、去掉首尾空白、转小写。
///
/// DNS 主机名本就大小写不敏感（RFC 4343），而 `http::Uri::host()` 原样保留大小写，
/// 所以比较前统一小写。IPv6 字面量在 URI 里**一定**带方括号、在清单里则可能带也可能不带，
/// 两边都剥掉才比得上。
fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// 把一条 `network_hosts` 条目拆成 `(host, port?)`。
///
/// 条目语义与 `plugin.toml` 的注释一致：`host` 放行该主机**任意**端口，`host:port`
/// 只放行该端口。
///
/// IPv6 字面量必须带方括号（`[::1]` / `[::1]:8080`）。形如 `::1` 的裸地址**不接受**
/// 端口拆分：`rsplit_once(':')` 会把地址里的最后一个冒号当成端口分隔符，把 `::1` 读成
/// 「主机 `::`、端口 1」——那是个静默的错误放行面（用户写 `::1` 本意是放行本机，
/// 结果放行的是 `::` 上的 1 号端口）。所以拆端口前先确认冒号左边不含冒号。
fn split_host_port(entry: &str) -> (String, Option<u16>) {
    let trimmed = entry.trim();
    if let Some(inner) = trimmed.strip_prefix('[') {
        return match inner.find(']') {
            Some(close) => (
                normalize_host(&inner[..close]),
                inner[close + 1..]
                    .strip_prefix(':')
                    .and_then(|text| text.parse().ok()),
            ),
            None => (normalize_host(inner), None),
        };
    }
    match trimmed.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            match port.parse::<u16>() {
                Ok(port) => (normalize_host(host), Some(port)),
                Err(_) => (normalize_host(trimmed), None),
            }
        }
        _ => (normalize_host(trimmed), None),
    }
}

impl CapabilityGranter {
    /// 空授权网关（默认拒绝）。
    pub fn new() -> Self {
        Self {
            granted: HashSet::new(),
        }
    }

    /// 按清单声明授予全部能力（当前策略：清单声明即授予；
    /// 引入安装时用户确认后，改为按用户勾选的子集授予）。
    pub fn grant_manifest(&mut self, caps: &Capabilities) {
        self.granted.extend(Capability::set_of(caps));
    }

    /// 授予单个能力（运行期用户追加授权用）。
    pub fn grant(&mut self, cap: Capability) {
        self.granted.insert(cap);
    }

    /// 仲裁：未授予即 [`PluginError::Invalid`]（拒绝是正常分支，不 panic）。
    ///
    /// # Errors
    /// 能力未授予时返回 [`PluginError::Invalid`]。
    pub fn require(&self, cap: &Capability) -> Result<(), PluginError> {
        if self.granted.contains(cap) {
            Ok(())
        } else {
            Err(PluginError::Invalid(format!(
                "capability not granted: {cap:?}"
            )))
        }
    }

    /// 只读查询（UI 展示「已授予」徽标用）。
    pub fn is_granted(&self, cap: &Capability) -> bool {
        self.granted.contains(cap)
    }

    /// 已授予的能力快照。
    pub fn granted(&self) -> &HashSet<Capability> {
        &self.granted
    }

    /// 仲裁一个**相对插件数据目录**的读路径：必须命中某条已授予的读 glob。
    ///
    /// # Errors
    /// 无匹配 glob 时返回 [`PluginError::Invalid`]。
    pub fn require_path_read(&self, relative: &str) -> Result<(), PluginError> {
        let matched = self.granted.iter().any(|cap| {
            matches!(cap, Capability::FilesystemRead(pattern) if glob_matches(pattern, relative))
        });
        if matched {
            Ok(())
        } else {
            Err(PluginError::Invalid(format!(
                "filesystem read not granted for {relative:?}"
            )))
        }
    }

    /// 仲裁一个出网目标：必须命中某条已授予的 host 条目。
    ///
    /// `host` 可以原样传 URI 里的形式（IPv6 带不带方括号都行，两边都会归一化）；
    /// `port` 必须是**已解析出具体端口**的值——调用方负责把 scheme 的默认端口
    /// （https 443 / http 80）补上，否则清单里的 `host:443` 永远匹配不上一个
    /// URL 里没写端口的请求，而这类失败在界面上只会表现成「插件跑不动」。
    ///
    /// 这是出网闸门里**唯一**的判定点，也是「跨域重定向自动被拦」的成因：
    /// 逐跳重定向的每一跳都要重新过这里，新 host 不在授予里自然过不去。
    ///
    /// # Errors
    /// 无匹配条目时返回 [`PluginError::Invalid`]。
    pub fn require_host(&self, host: &str, port: u16) -> Result<(), PluginError> {
        let wanted = normalize_host(host);
        let allowed = self.granted.iter().any(|cap| match cap {
            Capability::NetworkHost(entry) => {
                let (entry_host, entry_port) = split_host_port(entry);
                entry_host == wanted && entry_port.is_none_or(|entry_port| entry_port == port)
            }
            _ => false,
        });
        if allowed {
            Ok(())
        } else {
            Err(PluginError::Invalid(format!(
                "network host not granted: {wanted}:{port}"
            )))
        }
    }

    /// 仲裁一个**相对插件数据目录**的写路径（语义同 [`Self::require_path_read`]）。
    ///
    /// # Errors
    /// 无匹配 glob 时返回 [`PluginError::Invalid`]。
    pub fn require_path_write(&self, relative: &str) -> Result<(), PluginError> {
        let matched = self.granted.iter().any(|cap| {
            matches!(cap, Capability::FilesystemWrite(pattern) if glob_matches(pattern, relative))
        });
        if matched {
            Ok(())
        } else {
            Err(PluginError::Invalid(format!(
                "filesystem write not granted for {relative:?}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            filesystem_read: vec!["data/*".into()],
            filesystem_write: vec![],
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            process_exec: vec![],
        }
    }

    #[test]
    fn default_denies_everything() {
        let granter = CapabilityGranter::new();
        for cap in [
            Capability::FilesystemRead("data/*".into()),
            Capability::NetworkHost("api.example.com".into()),
            Capability::Notifications,
        ] {
            assert!(granter.require(&cap).is_err(), "{cap:?} must be denied");
        }
        assert!(granter.granted().is_empty());
    }

    #[test]
    fn manifest_grant_covers_every_declared_unit() {
        let mut granter = CapabilityGranter::new();
        granter.grant_manifest(&caps());
        assert!(
            granter
                .require(&Capability::FilesystemRead("data/*".into()))
                .is_ok()
        );
        assert!(
            granter
                .require(&Capability::NetworkHost("api.example.com".into()))
                .is_ok()
        );
        assert!(granter.require(&Capability::Notifications).is_ok());
        // 未声明的单元仍拒绝。
        assert!(
            granter
                .require(&Capability::FilesystemWrite("out/*".into()))
                .is_err()
        );
        assert!(
            granter
                .require(&Capability::ProcessExec("curl".into()))
                .is_err()
        );
        assert_eq!(granter.granted().len(), 3);
    }

    #[test]
    fn runtime_grant_extends_after_load() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::FilesystemWrite("out/*".into()));
        assert!(granter.is_granted(&Capability::FilesystemWrite("out/*".into())));
    }

    #[test]
    fn empty_manifest_grants_nothing() {
        let mut granter = CapabilityGranter::new();
        granter.grant_manifest(&Capabilities::default());
        assert!(granter.granted().is_empty());
    }

    #[test]
    fn glob_star_does_not_cross_segments() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::FilesystemRead("data/*".into()));
        assert!(granter.require_path_read("data/a.txt").is_ok());
        assert!(granter.require_path_read("data/sub/a.txt").is_err());
        assert!(granter.require_path_read("other/a.txt").is_err());
        // 写权限与读权限互不串用。
        assert!(granter.require_path_write("data/a.txt").is_err());
    }

    #[test]
    fn glob_double_star_crosses_segments() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::FilesystemWrite("out/**".into()));
        assert!(granter.require_path_write("out/a.txt").is_ok());
        assert!(granter.require_path_write("out/sub/a.txt").is_ok());
        assert!(granter.require_path_write("out").is_err());
        assert!(granter.require_path_read("out/a.txt").is_err());
    }

    #[test]
    fn glob_question_matches_single_char() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::FilesystemRead("data/?.txt".into()));
        assert!(granter.require_path_read("data/a.txt").is_ok());
        assert!(granter.require_path_read("data/ab.txt").is_err());
        assert!(granter.require_path_read("data//.txt").is_err());
    }

    /// 勾了子集之后真正生效的授权：**声明 ∩ 授予**，且每个字段都要覆盖到。
    ///
    /// 这一条是防「某一类能力漏在 `only` 外面」的：漏掉哪一类，那一类就永远授予不了
    /// （确认框勾了也跑不起来），而界面上完全看不出来。
    #[test]
    fn the_effective_grant_is_the_intersection_of_both_sides() {
        let declared = Capabilities {
            filesystem_read: vec!["data/*".into(), "cache/*".into()],
            filesystem_write: vec!["out/*".into()],
            network_hosts: vec!["api.example.com".into()],
            notifications: true,
            process_exec: vec!["ffmpeg".into()],
        };
        // 用户只勾了：第一个读 glob、网络、进程；没勾写、通知、第二个读 glob。
        let granted = Capabilities {
            filesystem_read: vec!["data/*".into()],
            network_hosts: vec!["api.example.com".into()],
            process_exec: vec!["ffmpeg".into()],
            ..Capabilities::default()
        };

        assert_eq!(
            declared.intersect(&granted),
            Capabilities {
                filesystem_read: vec!["data/*".into()],
                notifications: false,
                process_exec: vec!["ffmpeg".into()],
                network_hosts: vec!["api.example.com".into()],
                filesystem_write: vec![],
            },
            "读只留勾中的那条，写与通知被拒，网络与进程照给"
        );
    }

    /// 授予不能凭空造出声明里没有的能力：用户「全勾」也只到声明为止。
    #[test]
    fn a_grant_can_never_exceed_the_declaration() {
        let declared = Capabilities {
            network_hosts: vec!["api.example.com".into()],
            ..Capabilities::default()
        };
        let greedy = Capabilities {
            network_hosts: vec!["api.example.com".into(), "exfil.example.com".into()],
            notifications: true,
            process_exec: vec!["curl".into()],
            ..Capabilities::default()
        };
        let effective = declared.intersect(&greedy);
        assert_eq!(effective.network_hosts, ["api.example.com"]);
        assert!(!effective.notifications, "声明里没有的不能被授予出来");
        assert!(effective.process_exec.is_empty());
    }

    /// 没有授予记录时的缺省是「按声明全给」，等价于自己与自己求交。
    #[test]
    fn intersecting_with_itself_is_the_declaration_itself() {
        let declared = Capabilities {
            filesystem_read: vec!["b/*".into(), "a/*".into(), "c/*".into()],
            notifications: true,
            ..Capabilities::default()
        };
        let same = declared.intersect(&declared);
        assert_eq!(
            same, declared,
            "全给就是原样返回，且**顺序不变**（逐条列表按声明顺序渲染）"
        );
    }

    /// 一台主机都没有时，任何出网目标都必须被拒。
    #[test]
    fn default_denies_every_host() {
        let granter = CapabilityGranter::new();
        assert!(
            granter.require_host("api.example.com", 443).is_err(),
            "空授权下没有任何主机可去"
        );
    }

    /// 裸 host 条目放行该主机的**任意**端口。
    #[test]
    fn a_bare_host_entry_allows_any_port() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::NetworkHost("api.example.com".into()));
        for port in [80, 443, 8443] {
            assert!(
                granter.require_host("api.example.com", port).is_ok(),
                "裸 host 条目应当放行 {port}"
            );
        }
    }

    /// `host:port` 条目只放行那一个端口。
    ///
    /// 这条同时钉住调用方的契约：`port` 必须是**解析后**的具体端口。若调用方直接把
    /// URL 里「没写端口」传成某种缺省态，`host:8443` 就永远匹配不上，而失败会以
    /// 「插件跑不动」的样子出现，看不出是端口判错了。
    #[test]
    fn an_explicit_port_entry_allows_only_that_port() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::NetworkHost("api.example.com:8443".into()));
        assert!(granter.require_host("api.example.com", 8443).is_ok());
        assert!(
            granter.require_host("api.example.com", 443).is_err(),
            "没写的端口不该被顺带放行"
        );
    }

    /// 主机名大小写不敏感，且 URI 的方括号形式与清单的裸形式互相认得。
    #[test]
    fn host_matching_normalizes_case_and_brackets() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::NetworkHost("API.Example.COM".into()));
        granter.grant(Capability::NetworkHost("[::1]:8080".into()));

        assert!(granter.require_host("api.example.com", 443).is_ok());
        assert!(granter.require_host("API.EXAMPLE.com", 443).is_ok());
        assert!(granter.require_host("[::1]", 8080).is_ok());
        assert!(granter.require_host("::1", 8080).is_ok(), "裸形式也对得上");
        assert!(granter.require_host("::1", 80).is_err());
    }

    /// 像但不是的主机不能放行：白名单最怕的就是后缀/前缀撞车。
    #[test]
    fn a_lookalike_host_is_not_the_same_host() {
        let mut granter = CapabilityGranter::new();
        granter.grant(Capability::NetworkHost("example.com".into()));
        for host in [
            "evilexample.com",
            "example.com.evil.test",
            "sub.example.com",
            "example.co",
        ] {
            assert!(
                granter.require_host(host, 443).is_err(),
                "{host} 与 example.com 不是同一台主机，必须被拒"
            );
        }
    }

    /// 裸 IPv6 字面量里的冒号**不是**端口分隔符。
    ///
    /// `::1` 若按 `rsplit_once(':')` 拆，会读成「主机 `::`、端口 1」——等于用户想放行
    /// 本机，实际放行了另一台主机的 1 号端口。这条守着那个误读。
    #[test]
    fn an_unbracketed_ipv6_literal_is_not_read_as_a_port() {
        assert_eq!(split_host_port("::1"), ("::1".to_string(), None));
        assert_eq!(
            split_host_port("[::1]:8443"),
            ("::1".to_string(), Some(8443))
        );
        // 普通域名照常拆端口。
        assert_eq!(
            split_host_port("api.example.com:8443"),
            ("api.example.com".to_string(), Some(8443))
        );
        // 非数字尾巴不算端口（`host:abc` 当整条主机名，不静默丢端口）。
        assert_eq!(
            split_host_port("api.example.com:abc"),
            ("api.example.com:abc".to_string(), None)
        );
    }

    /// 一个都不勾 → 什么都不给；`Grannter` 上就是「未授予即拒绝」。
    #[test]
    fn granting_nothing_denies_every_declared_unit() {
        let declared = caps();
        let effective = declared.intersect(&Capabilities::default());
        assert!(effective.is_empty());

        let mut granter = CapabilityGranter::new();
        granter.grant_manifest(&effective);
        assert!(granter.granted().is_empty());
        assert!(
            granter
                .require(&Capability::NetworkHost("api.example.com".into()))
                .is_err(),
            "空授予下连声明过的能力也必须被拒"
        );
    }
}
