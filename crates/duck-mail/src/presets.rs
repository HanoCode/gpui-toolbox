//! 常见邮箱服务商的**收发服务器预设** + 按邮箱域名自动识别。
//!
//! # 为什么要有这一层
//!
//! IMAP/SMTP 的主机名、端口、加密方式对每个服务商都是固定值，却又是**用户
//! 最难自己查到**的三项（尤其加密方式：`993 + TLS` 与 `143 + STARTTLS` 都能用，
//! 但填错就是「连不上」而不是「慢」）。让用户手填等于把服务商的运维细节
//! 转嫁给他 —— 主流客户端（Thunderbird / Apple Mail / Outlook）都是内置
//! 一份这样的表，或直接做 autoconfig / autodiscover 自动发现。
//!
//! 本模块做的是**最省事的那一半**：一份静态表 + 域名匹配。相比 RustMailer
//! 引入 `autoconfig` crate、Stalwart 实现 Thunderbird autoconfig 与
//! Microsoft autodiscover 三种标准，静态表的覆盖面窄（只认表里的服务商），
//! 但**零网络往返、零新增依赖**，且失败模式是「没识别出来，请手填」——
//! 用户本来就要面对的最坏情况，不会更差。表里没有的域名走
//! [`preset_for_email`] 返回 `None`，界面据此退回手填。
//!
//! ⚠️ **本 crate 不吐中文**（`every_chinese_literal_is_translated` 守卫只覆盖
//! `duck-theme`）；`label` 一律是**品牌专名**（Gmail / Outlook…），中英文界面下
//! 写法相同，因此不做本地化。界面自造的两个选项（「自动识别」「自定义」）
//! 才是中文，由 `duck-app` 侧登记 EN 表。

use crate::models::ConnectionSecurity;

/// 一家服务商的固定收发参数。
///
/// 字段全是 `'static`：这份表是编译期常量，不进任何持久化结构。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderPreset {
    /// 稳定标识（**落盘用这个**，不是 `label`）：界面传 `id` 回来查表。
    pub id: &'static str,
    /// 显示名（品牌专名，不做本地化）。
    pub label: &'static str,
    /// 归属域名（小写、不含 `@`）。一封 `a@b.c` 只在 `b.c` 命中时算识别成功。
    pub domains: &'static [&'static str],
    /// 收件（IMAP）主机。
    pub imap_host: &'static str,
    /// 收件端口。
    pub imap_port: u16,
    /// 发件（SMTP）主机。
    pub smtp_host: &'static str,
    /// 发件端口。
    pub smtp_port: u16,
    /// 加密方式。**收件与发件共用一档**：表里这几家两项都支持同一档
    /// （要么都直连 TLS，要么都 STARTTLS），拆成两个字段只会让表更长。
    pub security: ConnectionSecurity,
    /// 登录名是否**必须**是完整邮箱地址。
    ///
    /// `true` 的服务商（Gmail / Outlook / QQ / 网易）不接受裸用户名，
    /// 界面据此把「登录名」留空时回填整封邮箱地址。
    pub login_is_email: bool,
}

/// 预设表。顺序即界面下拉里的展示顺序（先国际大厂，再国内常用）。
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "gmail",
        label: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        imap_host: "imap.gmail.com",
        imap_port: 993,
        smtp_host: "smtp.gmail.com",
        smtp_port: 465,
        security: ConnectionSecurity::Tls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "outlook",
        label: "Outlook / Hotmail",
        domains: &[
            "outlook.com",
            "hotmail.com",
            "live.com",
            "msn.com",
            "outlook.com.cn",
        ],
        imap_host: "outlook.office365.com",
        imap_port: 993,
        smtp_host: "smtp.office365.com",
        smtp_port: 587,
        security: ConnectionSecurity::StartTls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "qq",
        label: "QQ Mail",
        domains: &["qq.com", "vip.qq.com", "foxmail.com"],
        imap_host: "imap.qq.com",
        imap_port: 993,
        smtp_host: "smtp.qq.com",
        smtp_port: 465,
        security: ConnectionSecurity::Tls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "netease163",
        label: "NetEase 163",
        domains: &["163.com", "vip.163.com"],
        imap_host: "imap.163.com",
        imap_port: 993,
        smtp_host: "smtp.163.com",
        smtp_port: 465,
        security: ConnectionSecurity::Tls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "netease126",
        label: "NetEase 126",
        domains: &["126.com", "vip.126.com"],
        imap_host: "imap.126.com",
        imap_port: 993,
        smtp_host: "smtp.126.com",
        smtp_port: 465,
        security: ConnectionSecurity::Tls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "yahoo",
        label: "Yahoo Mail",
        domains: &["yahoo.com", "yahoo.co.jp", "ymail.com"],
        imap_host: "imap.mail.yahoo.com",
        imap_port: 993,
        smtp_host: "smtp.mail.yahoo.com",
        smtp_port: 465,
        security: ConnectionSecurity::Tls,
        login_is_email: true,
    },
    ProviderPreset {
        id: "icloud",
        label: "iCloud Mail",
        domains: &["icloud.com", "me.com", "mac.com"],
        imap_host: "imap.mail.me.com",
        imap_port: 993,
        smtp_host: "smtp.mail.me.com",
        smtp_port: 587,
        security: ConnectionSecurity::StartTls,
        login_is_email: true,
    },
];

/// `preset_for_email` 认出的域名部分：`@` 之后、全小写。
///
/// 返回 `None`：没有 `@`、`@` 在首尾、或域名里带空白 —— 都说明这还不是一个
/// 可用的地址，调用方（界面）自己会做「需含 `@`」那一层校验。
#[must_use]
pub fn email_domain(email: &str) -> Option<String> {
    let (local, domain) = email.trim().split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains(char::is_whitespace) {
        return None;
    }
    Some(domain.to_ascii_lowercase())
}

/// 按邮箱地址识别服务商。识别不出返回 `None`（界面退回手填）。
///
/// 匹配是**精确域名**而不是后缀：`mail.163.com` 这种子域不属于
/// 「用户的主邮箱地址」，硬认反而会填出一台错误的主机。
#[must_use]
pub fn preset_for_email(email: &str) -> Option<&'static ProviderPreset> {
    let domain = email_domain(email)?;
    PRESETS
        .iter()
        .find(|preset| preset.domains.contains(&domain.as_str()))
}

/// 按 `id` 查预设（下拉里选中某一项时用）。
#[must_use]
pub fn preset_by_id(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表本身要自洽：id 唯一、域名唯一、端口非零。
    ///
    /// 重复的域名会让 `preset_for_email` 的结论取决于表里谁先出现 ——
    /// 那是「同一封信的服务器取决于源码行序」这类最难复现的 bug。
    #[test]
    fn table_is_self_consistent() {
        for (index, preset) in PRESETS.iter().enumerate() {
            assert!(!preset.id.is_empty(), "第 {index} 项缺 id");
            assert!(
                !preset.imap_host.is_empty() && !preset.smtp_host.is_empty(),
                "{} 缺主机名",
                preset.id
            );
            assert!(
                preset.imap_port != 0 && preset.smtp_port != 0,
                "{} 端口为 0",
                preset.id
            );
            assert!(!preset.domains.is_empty(), "{} 没有域名", preset.id);
            for domain in preset.domains {
                assert_eq!(
                    *domain,
                    domain.to_ascii_lowercase(),
                    "{} 的域名必须是小写",
                    preset.id
                );
                let holders: Vec<&str> = PRESETS
                    .iter()
                    .filter(|other| other.domains.contains(domain))
                    .map(|other| other.id)
                    .collect();
                assert_eq!(holders.len(), 1, "域名 {domain} 被 {holders:?} 同时声明");
            }
        }
    }

    #[test]
    fn id_lookup_round_trips() {
        for preset in PRESETS {
            assert_eq!(
                preset_by_id(preset.id).map(|found| found.id),
                Some(preset.id)
            );
        }
        assert!(preset_by_id("nope").is_none());
    }

    /// 识别按**整段域名**，不做后缀包含 —— 也顺带钉住大小写与首尾空白。
    #[test]
    fn recognition_matches_the_exact_domain() {
        let gmail = preset_for_email("Someone@Gmail.COM").expect("应认出 Gmail");
        assert_eq!(gmail.id, "gmail");
        assert_eq!(gmail.imap_host, "imap.gmail.com");
        assert_eq!(gmail.imap_port, 993);
        // 大小写不敏感、前后空白忽略。
        assert_eq!(
            preset_for_email("  a@qq.com  ").map(|preset| preset.id),
            Some("qq")
        );
        // 子域 / 长得像的域名都不认（否则会填出一台用户没在用、也连不上的主机）。
        assert!(preset_for_email("a@mail.163.com").is_none());
        assert!(preset_for_email("a@notgmail.com").is_none());
        assert!(preset_for_email("a@gmail.com.example.org").is_none());
    }

    #[test]
    fn malformed_addresses_are_not_recognised() {
        assert!(preset_for_email("no-at-sign").is_none());
        assert!(preset_for_email("@gmail.com").is_none());
        assert!(preset_for_email("a@").is_none());
        assert!(preset_for_email("a@ gmail.com").is_none());
        assert!(preset_for_email("").is_none());
        assert_eq!(email_domain("a@b.c"), Some("b.c".to_owned()));
    }

    /// 每个预设在表里都能被它自己的域名认出来（防止加了行却忘了挂域名）。
    #[test]
    fn every_preset_is_reachable_by_its_domains() {
        for preset in PRESETS {
            for domain in preset.domains {
                let email = format!("someone@{domain}");
                assert_eq!(
                    preset_for_email(&email).map(|found| found.id),
                    Some(preset.id),
                    "{email} 应认出 {}",
                    preset.id
                );
            }
        }
    }
}
