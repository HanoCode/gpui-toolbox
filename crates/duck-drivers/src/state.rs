//! 安装状态的落盘：`<base>/state.json`。
//!
//! 里面只有**可丢**的信息：装了哪个版本、什么时候装的、用哪个 JRE、选的下载源、
//! Java 运行时档位。丢了最坏是重下一遍，所以它不加密、不进 `duck-store`
//! （`duck-store` 的 kv 要 gpui `App`，而本模块必须能在无 UI 的上下文里读写 —— 见
//! crate 文档）。
//!
//! # 写盘是「临时文件 + 改名」
//!
//! `state.json` 会被**并发**的安装操作改（用户连点两个驱动的「安装」），所以：
//! 所有写都要走 [`DriverState::save`]，它先写同目录的 `.tmp` 再 `rename` ——
//! 同名改名在 POSIX 与 Windows（`ReplaceFile` 语义）上都是原子的，读者永远看到
//! 完整的一份。**不要**用 `File::create("state.json")` 直接覆盖。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DownloadSource;
use crate::registry::ArtifactKind;

/// 一条已安装的驱动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledDriver {
    /// 注册表键（= 安装目录名）。
    pub key: String,
    /// 装的是哪一版（注册表口径的版本串）。
    pub version: String,
    /// 装的时间（Unix 秒）。
    pub installed_at: u64,
    /// 轨别：`jar` 要 JVM，`native` 不要。
    pub kind: ArtifactKind,
    /// 该驱动要用的 JRE 键（native 轨为空）。
    #[serde(default)]
    pub jre: Option<String>,
    /// 工件字节数（界面上的占用统计用它，免得每次去 `du` 一遍）。
    #[serde(default)]
    pub bytes: u64,
}

impl InstalledDriver {
    /// 安装目录里的可执行入口（jar 轨是 `agent.jar`，native 轨是 `agent[.exe]`）。
    #[must_use]
    pub fn entry_path(&self, driver_dir: &Path) -> PathBuf {
        match self.kind {
            ArtifactKind::Jar => driver_dir.join("agent.jar"),
            ArtifactKind::Native => driver_dir.join(crate::platform::executable_name("agent")),
        }
    }
}

/// 状态的落盘形状。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverState {
    /// 已安装的驱动（键 = 注册表键）。
    #[serde(default)]
    pub drivers: HashMap<String, InstalledDriver>,
    /// 已安装的 JRE（键 = JRE 键如 `21`，值 = 安装的版本串）。
    #[serde(default)]
    pub jre_versions: HashMap<String, String>,
    /// Java 运行时档位。
    #[serde(default)]
    pub java_runtime: crate::JavaRuntime,
    /// 用户选的下载源。
    #[serde(default)]
    pub download_source: DownloadSource,
}

impl DriverState {
    /// 从磁盘读；文件不在或读不动时给**默认状态**（不是错误：首次启动就该是空的）。
    ///
    /// 只有「文件在、但内容不是合法 JSON」才记一条警告并回落默认 —— 那说明文件被写坏
    /// 或者被手改坏了，此时宁可重下一次也不该让整个模块起不来。
    #[must_use]
    pub fn load(base_dir: &Path) -> Self {
        let path = base_dir.join(STATE_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "driver store state is unreadable; falling back to an empty state"
                );
                Self::default()
            }
        }
    }

    /// 原子写盘（先写 `.tmp` 再改名）。
    ///
    /// # Errors
    /// 建目录、写临时文件、改名任一失败。
    pub fn save(&self, base_dir: &Path) -> crate::Result<()> {
        std::fs::create_dir_all(base_dir)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", base_dir.display())))?;
        let path = base_dir.join(STATE_FILE);
        let temp = base_dir.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| crate::DriverError::Format(format!("state: {error}")))?;
        std::fs::write(&temp, text)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", temp.display())))?;
        std::fs::rename(&temp, &path)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", path.display())))
    }
}

/// 状态文件名。
pub const STATE_FILE: &str = "state.json";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaRuntimeMode;

    fn base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-drivers-state-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 没有文件时给空状态，不报错 —— 首次启动的正常路径。
    #[test]
    fn a_missing_file_is_an_empty_state() {
        let dir = base("missing");
        let state = DriverState::load(&dir);
        assert!(state.drivers.is_empty());
        assert_eq!(state.download_source, DownloadSource::Official);
    }

    /// 存进去再读出来必须逐字相同（含 JRE 档位与下载源）。
    #[test]
    fn a_saved_state_round_trips() {
        let dir = base("round-trip");
        let mut state = DriverState {
            download_source: DownloadSource::Mirror,
            java_runtime: crate::JavaRuntime {
                mode: JavaRuntimeMode::Custom,
                custom_path: Some("/opt/java/bin/java".to_owned()),
            },
            ..DriverState::default()
        };
        state.drivers.insert(
            "dameng".to_owned(),
            InstalledDriver {
                key: "dameng".to_owned(),
                version: "0.1.92".to_owned(),
                installed_at: 1_700_000_000,
                kind: ArtifactKind::Jar,
                jre: Some("21".to_owned()),
                bytes: 2_429_919,
            },
        );
        state
            .jre_versions
            .insert("21".to_owned(), "21.0.12".to_owned());
        state.save(&dir).expect("写盘");

        let loaded = DriverState::load(&dir);
        assert_eq!(loaded, state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写坏的文件回落空状态，且不留 `.tmp`。
    #[test]
    fn a_corrupt_file_falls_back_and_leaves_no_temp() {
        let dir = base("corrupt");
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join(STATE_FILE), b"{ not json").expect("写坏文件");
        let state = DriverState::load(&dir);
        assert!(state.drivers.is_empty());
        assert!(!dir.join(format!("{STATE_FILE}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧存档缺字段时补默认值（前向兼容的唯一判据：全字段 `#[serde(default)]`）。
    #[test]
    fn an_old_state_without_new_fields_still_loads() {
        let dir = base("old");
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(
            dir.join(STATE_FILE),
            br#"{"drivers":{"dameng":{"key":"dameng","version":"0.1.0","installed_at":1,"kind":"jar"}}}"#,
        )
        .expect("写旧状态");
        let state = DriverState::load(&dir);
        let driver = state.drivers.get("dameng").expect("旧条目要读得进来");
        assert_eq!(driver.jre, None);
        assert_eq!(driver.bytes, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入口路径按轨别分开：jar 是 `agent.jar`，native 是 `agent`。
    #[test]
    fn entry_paths_differ_by_track() {
        let jar = InstalledDriver {
            key: "dameng".to_owned(),
            version: "1".to_owned(),
            installed_at: 0,
            kind: ArtifactKind::Jar,
            jre: Some("21".to_owned()),
            bytes: 0,
        };
        assert_eq!(
            jar.entry_path(Path::new("/tmp/x")).file_name().unwrap(),
            "agent.jar"
        );
        let native = InstalledDriver {
            kind: ArtifactKind::Native,
            jre: None,
            ..jar
        };
        assert_eq!(
            native
                .entry_path(Path::new("/tmp/x"))
                .file_name()
                .unwrap()
                .to_string_lossy(),
            crate::platform::executable_name("agent")
        );
    }
}
