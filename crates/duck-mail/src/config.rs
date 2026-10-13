//! 邮箱模块的文件系统落点。
//!
//! 独立子目录 `{app_data_dir}/mail/`，与 `duck-store` 的 kv 库、`duck-ai` 的会话库、
//! `duck-db` 的用户库**分库**（`docs/email-design.md` §4.5）：生命周期、备份策略与
//! 损坏爆炸半径各不相同，混在一起会互相牵连。
//!
//! ```text
//! ~/Library/Application Support/DuckToolbox/mail/
//! ├── mail.db            # 账户 / 文件夹 / 线程 / 邮件 / 正文 / 附件元数据 + FTS5
//! └── attachments/       # 附件实体文件（按 <message_id>/<attachment_id> 分目录）
//! ```

use std::path::{Path, PathBuf};

use crate::error::{MailError, Result};

/// 邮箱模块的数据落点。
///
/// `Clone` 且廉价——内部只存一个根路径。
#[derive(Debug, Clone)]
pub struct MailPaths {
    root: PathBuf,
}

impl MailPaths {
    /// 默认落点：`{app_data_dir}/mail`。
    pub fn default_root() -> Self {
        Self {
            root: duck_platform::app_data_dir().join("mail"),
        }
    }

    /// 指定根目录（测试与「自定义数据目录」用）。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 主库文件路径。
    pub fn db_path(&self) -> PathBuf {
        self.root.join("mail.db")
    }

    /// 附件实体目录。
    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join("attachments")
    }

    /// 一个附件在**应用管理目录**内的缓存落点。
    ///
    /// 结构 `attachments/<message_id>/<attachment_id>-<安全文件名>`：一个邮件一个子目录，
    /// 同一封里多个附件不会因同名互相覆盖（`attachment_id` 是主键）。返回前**校验**
    /// 拼出来的路径仍在 [`Self::attachments_dir`] 之内——文件名来自服务端，
    /// `../` / 绝对路径之类必须被挡掉（见 [`sanitize_filename`]）。
    pub fn attachment_cache_path(
        &self,
        message_id: i64,
        attachment_id: i64,
        filename: &str,
    ) -> Result<PathBuf> {
        let dir = self.attachments_dir().join(message_id.to_string());
        let file = format!("{attachment_id}-{}", sanitize_filename(filename));
        let path = dir.join(file);
        // 防御性复核：清洗之后仍必须落在附件目录内（`starts_with` 按组件比较，
        // 不会把 `/x/attachments-evil` 当成 `/x/attachments` 的子路径）。
        if !path.starts_with(self.attachments_dir()) {
            return Err(MailError::Io(format!(
                "attachment cache path escapes the attachments directory: {}",
                path.display()
            )));
        }
        Ok(path)
    }

    /// 创建目录树（幂等）。
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| MailError::Io(format!("cannot create {}: {e}", self.root.display())))?;
        std::fs::create_dir_all(self.attachments_dir()).map_err(|e| {
            MailError::Io(format!(
                "cannot create {}: {e}",
                self.attachments_dir().display()
            ))
        })?;
        Ok(())
    }
}

impl Default for MailPaths {
    fn default() -> Self {
        Self::default_root()
    }
}

/// 把服务端给的文件名清洗成「单一路径组件、无穿越风险」的安全名字。
///
/// 规则：
/// - 只保留最后一段（丢掉目录部分，`a/b/../c.txt` → `c.txt`）；
/// - 路径分隔符、控制字符、Windows 保留字符（`<>:"|?*`）换成 `_`；
/// - 前导 `..` 与纯点集合（`.` / `..`）被替换，避免 `join` 后回到上一级；
/// - 保留中文等正常字符，尾部截断到 120 个字符（按字符边界，不切开 UTF-8）。
///
/// 清洗后为空（文件名本来就只有非法字符）时回落到 `attachment`——调用方仍会拼上
/// `attachment_id` 前缀，所以不会退化成同名碰撞。
pub fn sanitize_filename(filename: &str) -> String {
    // 先丢掉目录部分：`/` 与 `\` 两种分隔符都认。
    let base = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        let illegal =
            ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\');
        out.push(if illegal { '_' } else { ch });
    }
    // 去掉纯点 / 前导点：`.`、`..`、`.hidden` 在 `join` 语义下都可能指向别处。
    let trimmed = out.trim_start_matches('.').to_string();
    let sanitized = if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed
    };
    // 限长，按字符边界截断（避免把多字节字符切两半）。
    if sanitized.chars().count() > 120 {
        sanitized.chars().take(120).collect()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        let paths = MailPaths::with_root("/tmp/duck-mail-test");
        assert_eq!(paths.db_path(), Path::new("/tmp/duck-mail-test/mail.db"));
        assert_eq!(
            paths.attachments_dir(),
            Path::new("/tmp/duck-mail-test/attachments")
        );
    }

    #[test]
    fn sanitize_filename_neutralizes_traversal_and_separators() {
        assert_eq!(sanitize_filename("报告.pdf"), "报告.pdf");
        // 目录部分被丢掉。
        assert_eq!(sanitize_filename("a/b/报告.pdf"), "报告.pdf");
        assert_eq!(sanitize_filename("a\\b\\x.txt"), "x.txt");
        // 前导点被剥掉，避免 `.` / `..` 语义。
        assert_eq!(sanitize_filename(".."), "attachment");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        // 保留字符换成下划线。
        assert_eq!(sanitize_filename("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_filename(""), "attachment");
        // 超长文件名按字符截断（不切开 UTF-8）。
        let long = "中".repeat(300);
        assert_eq!(sanitize_filename(&long).chars().count(), 120);
    }

    #[test]
    fn attachment_cache_path_stays_inside_the_managed_directory() {
        let paths = MailPaths::with_root("/tmp/duck-mail-cache");
        let path = paths
            .attachment_cache_path(7, 3, "../../evil/报告.pdf")
            .unwrap();
        assert_eq!(
            path,
            Path::new("/tmp/duck-mail-cache/attachments/7/3-报告.pdf")
        );
        assert!(path.starts_with(paths.attachments_dir()));
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("duck-mail-ensure-{}", std::process::id()));
        let paths = MailPaths::with_root(&dir);
        paths.ensure().unwrap();
        paths.ensure().unwrap();
        assert!(paths.db_path().parent().unwrap().is_dir());
        assert!(paths.attachments_dir().is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
