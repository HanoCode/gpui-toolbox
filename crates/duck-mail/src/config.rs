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
