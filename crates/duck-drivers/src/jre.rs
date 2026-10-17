//! 托管 JRE：jar 轨驱动需要一个 21+ 的 Java 运行时。
//!
//! # 三种档位（与 dbx 一致）
//!
//! | 档位 | 用什么 | 谁负责装 |
//! | --- | --- | --- |
//! | 托管（默认） | `<base>/jre-21/`，从注册表下 Temurin 21 | 本模块 |
//! | 系统 | 系统上已有的 `java`（`JAVA_HOME` → `PATH`） | 用户 |
//! | 自定义 | 用户指定的 `java` 绝对路径 | 用户 |
//!
//! 默认给**托管**：只有这一档能让「装个驱动就能连库」成立。系统档留给已经有 JDK 的
//! 用户（省几十 MB 下载），自定义档留给装了 JDK 8 又不想动它的用户。
//!
//! # 版本探测有两条路
//!
//! 1. `release` 文件里的 `JAVA_VERSION="21.0.12"` —— **先走这条**：一次文件读，
//!    不动进程。
//! 2. 跑 `java -version` 解析 stderr —— 兜底。有些发行版不落 `release`；
//!    也用来验证用户手填的路径**真的是 java**。
//!
//! ⚠️ `java -version` 把版本写进 **stderr**（不是 stdout），解析时要一起收。
//!
//! # 最低版本
//!
//! [`MINIMUM_MAJOR`] = 21。低于它不报「不支持」，而是**明确提示要 21+** ——
//! 这个数字来自驱动的构建目标（dbx 的 jar 编在 21 上），改它等于换一批工件。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// 托管 JRE 的最低主版本。
pub const MINIMUM_MAJOR: u32 = 21;

/// 默认（也是唯一当前在注册表里的）JRE 键。
pub const DEFAULT_KEY: &str = "21";

/// Java 运行时档位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JavaRuntimeMode {
    /// 托管：本模块下载并安装到驱动目录下。
    #[default]
    Managed,
    /// 系统：用 `JAVA_HOME` / `PATH` 里的 `java`。
    System,
    /// 自定义：用用户填的绝对路径。
    Custom,
}

impl JavaRuntimeMode {
    /// 全部档位（界面下拉顺序）。
    pub const ALL: [Self; 3] = [Self::Managed, Self::System, Self::Custom];

    /// 落盘值。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::System => "system",
            Self::Custom => "custom",
        }
    }

    /// 解析落盘值；未知值回落托管。
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "system" => Self::System,
            "custom" => Self::Custom,
            _ => Self::Managed,
        }
    }
}

/// Java 运行时配置（随状态一起落盘）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntime {
    /// 档位。
    #[serde(default)]
    pub mode: JavaRuntimeMode,
    /// 自定义档的 `java` 绝对路径。
    #[serde(default)]
    pub custom_path: Option<String>,
}

impl Default for JavaRuntime {
    fn default() -> Self {
        Self {
            mode: JavaRuntimeMode::Managed,
            custom_path: None,
        }
    }
}

/// 托管 JRE 的安装目录：`<base>/jre-<key>`。
#[must_use]
pub fn managed_dir(base_dir: &Path, key: &str) -> PathBuf {
    base_dir.join(format!("jre-{key}"))
}

/// 托管 JRE 的 `java` 可执行文件。
///
/// macOS 上的 JDK 包有两层布局（`bin/java` 与 `Contents/Home/bin/java`），
/// 两种都认——**按文件存在与否选**，不按平台猜。
#[must_use]
pub fn managed_java(base_dir: &Path, key: &str) -> Option<PathBuf> {
    let dir = managed_dir(base_dir, key);
    let name = crate::platform::executable_name("java");
    let flat = dir.join("bin").join(&name);
    if flat.is_file() {
        return Some(flat);
    }
    let bundled = dir.join("Contents").join("Home").join("bin").join(name);
    bundled.is_file().then_some(bundled)
}

/// 解包目录里**真正的发行版根**。
///
/// 实测 dbx 的 `dbx-jre-21-<platform>.tar.zst` 在顶层套了一层目录（`dbx-jre/`，
/// 里面才是 `bin/`、`lib/`、`release`）。安装前把这一层剥掉，落位之后的布局就是
/// `jre-21/bin/java` —— 与驱动目录同一口径，`managed_java` 也只需要认一种形态。
///
/// 判据是「顶层除点开头的杂物外只有一项、且它是目录」：多于一项就别动（那说明
/// 归档本来就是散的，剥了反而会把 `README` 之类混进根里）。
#[must_use]
pub fn archive_root(staging: &Path) -> PathBuf {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return staging.to_path_buf();
    };
    let items: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        // `.DS_Store` / AppleDouble 影子文件不算「一项」。
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
        })
        .collect();
    match items.as_slice() {
        [only] if only.is_dir() => only.clone(),
        _ => staging.to_path_buf(),
    }
}

/// 系统 `java`：先 `JAVA_HOME`，再 `PATH`。
#[must_use]
pub fn system_java() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("JAVA_HOME")
        && !home.trim().is_empty()
    {
        let candidate = Path::new(home.trim())
            .join("bin")
            .join(crate::platform::executable_name("java"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let name = crate::platform::executable_name("java");
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// 按当前档位选出该用哪个 `java`。
///
/// 托管档**只认托管目录**：没装就返回 `None`（上层据此提示「需先安装 Java 运行时」），
/// 不偷偷回落到系统 java —— 那会让「装好了却还是报缺 JRE」这种状态变得不可解释。
#[must_use]
pub fn resolve_java(base_dir: &Path, runtime: &JavaRuntime, jre_key: &str) -> Option<PathBuf> {
    match runtime.mode {
        JavaRuntimeMode::Managed => managed_java(base_dir, jre_key),
        JavaRuntimeMode::System => system_java(),
        JavaRuntimeMode::Custom => runtime
            .custom_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_file()),
    }
}

/// 探一个 `java` 的主版本：先读发行版自带的 `release` 文件，读不到再跑一次
/// `java -version`。
///
/// # Errors
/// 两条路都拿不到版本时返回错误说明（英文）。
pub fn probe_major_version(java: &Path) -> Result<u32, String> {
    // `release` 在 `<java>` 的上一级的上一级（`bin/java` → 根）。
    if let Some(root) = java.parent().and_then(Path::parent)
        && let Ok(text) = std::fs::read_to_string(root.join("release"))
        && let Some(major) = release_major_version(&text)
    {
        return Ok(major);
    }

    let output = Command::new(java)
        .arg("-version")
        .output()
        .map_err(|error| format!("cannot run {}: {error}", java.display()))?;
    // ⚠️ 版本写在 stderr 上。
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    version_output_major_version(&text)
        .ok_or_else(|| format!("cannot read a version from {}", java.display()))
}

/// 从 `release` 文件内容里取主版本（`JAVA_VERSION="21.0.12"`）。
#[must_use]
pub fn release_major_version(release_contents: &str) -> Option<u32> {
    let value = release_contents
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))?
        .trim()
        .trim_matches('"');
    major_from_version(value)
}

/// 从 `java -version` 的输出里取主版本（`openjdk version "21.0.12" 2024-07-16`）。
#[must_use]
pub fn version_output_major_version(output: &str) -> Option<u32> {
    // 取第一对引号里的东西：两种输出形态（`java version "1.8.0_402"` /
    // `openjdk version "21.0.12"`）都是「version」后面跟引号里的版本。
    let first_quote = output.find('"')?;
    let rest = &output[first_quote + 1..];
    let second_quote = rest.find('"')?;
    major_from_version(&rest[..second_quote])
}

/// 版本串 → 主版本号。
///
/// ⚠️ **`1.8.0_402` 的主版本是 8，不是 1**：Java 8 及更早用 `1.x` 前缀，
/// 这种老版本对我们的 21+ 要求来说是「太低」，但报出来的数字必须是对的
/// ——否则界面上会写着「需要 21+，当前 1」，用户根本不知道自己在用 Java 8。
fn major_from_version(version: &str) -> Option<u32> {
    let mut parts = version.split(['.', '_', '+', '-']);
    let first = parts.next()?.trim().parse::<u32>().ok()?;
    if first == 1 {
        return parts.next()?.trim().parse::<u32>().ok();
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `release` 文件解析。
    #[test]
    fn release_file_is_parsed() {
        assert_eq!(
            release_major_version("IMPLEMENTOR=\"Eclipse Adoptium\"\nJAVA_VERSION=\"21.0.12\"\n"),
            Some(21)
        );
        assert_eq!(release_major_version("JAVA_VERSION=\"17\""), Some(17));
        assert_eq!(release_major_version("JAVA_VERSION=\"1.8.0_402\""), Some(8));
        assert_eq!(release_major_version("nothing here"), None);
    }

    /// `java -version` 输出解析（含 Java 8 的 `1.8` 形态）。
    #[test]
    fn version_output_is_parsed() {
        assert_eq!(
            version_output_major_version(
                "openjdk version \"21.0.12\" 2024-07-16\nOpenJDK Runtime Environment"
            ),
            Some(21)
        );
        assert_eq!(
            version_output_major_version(
                "java version \"1.8.0_402\"\nJava(TM) SE Runtime Environment"
            ),
            Some(8)
        );
        assert_eq!(
            version_output_major_version("openjdk version \"24-ea\" 2025-03-18"),
            Some(24)
        );
        assert_eq!(version_output_major_version("no version here"), None);
    }

    /// 托管档**不回落**系统 java：装了就是装了，没装就该提示缺 JRE。
    #[test]
    fn managed_mode_never_falls_back_to_the_system_java() {
        let base = std::env::temp_dir().join("duck-drivers-jre-test-managed");
        let _ = std::fs::remove_dir_all(&base);
        let runtime = JavaRuntime::default();
        assert_eq!(runtime.mode, JavaRuntimeMode::Managed);
        assert_eq!(resolve_java(&base, &runtime, DEFAULT_KEY), None);

        // 造一个托管布局（macOS 的 `Contents/Home` 形态）。
        let bin = base
            .join(format!("jre-{DEFAULT_KEY}"))
            .join("Contents")
            .join("Home")
            .join("bin");
        std::fs::create_dir_all(&bin).expect("建目录");
        std::fs::write(
            bin.join(crate::platform::executable_name("java")),
            b"#!/bin/sh\n",
        )
        .expect("写文件");
        assert!(resolve_java(&base, &runtime, DEFAULT_KEY).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 自定义档只认存在的绝对路径；空串等于没配。
    #[test]
    fn custom_mode_requires_an_existing_path() {
        let base = Path::new("/nonexistent-base");
        assert_eq!(
            resolve_java(
                base,
                &JavaRuntime {
                    mode: JavaRuntimeMode::Custom,
                    custom_path: Some("   ".to_owned())
                },
                DEFAULT_KEY
            ),
            None
        );
        assert_eq!(
            resolve_java(
                base,
                &JavaRuntime {
                    mode: JavaRuntimeMode::Custom,
                    custom_path: Some("/definitely/not/here/java".to_owned())
                },
                DEFAULT_KEY
            ),
            None
        );
    }

    /// 档位落盘值往返。
    #[test]
    fn mode_names_round_trip() {
        for mode in JavaRuntimeMode::ALL {
            assert_eq!(JavaRuntimeMode::from_wire(mode.wire_name()), mode);
        }
        assert_eq!(JavaRuntimeMode::from_wire("nope"), JavaRuntimeMode::Managed);
    }

    /// 顶层套一层目录时剥掉它；散着的归档原样返回。
    #[test]
    fn a_single_top_level_directory_is_stripped() {
        let dir = std::env::temp_dir().join("duck-drivers-jre-root");
        let _ = std::fs::remove_dir_all(&dir);

        // 形态一：`dbx-jre/bin/java`（实测的归档形态）。
        std::fs::create_dir_all(dir.join("dbx-jre").join("bin")).expect("建目录");
        std::fs::write(dir.join("dbx-jre").join("bin").join("java"), b"x").expect("写文件");
        assert_eq!(archive_root(&dir), dir.join("dbx-jre"));

        // 形态二：顶层就是 `bin/`（散归档），不该剥。
        let flat = dir.join("flat");
        std::fs::create_dir_all(flat.join("bin")).expect("建目录");
        std::fs::write(flat.join("bin").join("java"), b"x").expect("写文件");
        std::fs::write(flat.join("release"), b"JAVA_VERSION=\"21\"").expect("写文件");
        assert_eq!(archive_root(&flat), flat);

        // 点开头的杂物不参与判断。
        let noisy = dir.join("noisy");
        std::fs::create_dir_all(noisy.join("dbx-jre").join("bin")).expect("建目录");
        std::fs::write(noisy.join(".DS_Store"), b"junk").expect("写文件");
        assert_eq!(archive_root(&noisy), noisy.join("dbx-jre"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本机的 `java`（如果有）能被探出版本——没有就跳过。
    ///
    /// 这条是**唯一的真实探测**用例：它钉住「跑进程 + 解析 stderr」这条路真的通。
    #[test]
    fn the_system_java_can_be_probed_when_present() {
        let Some(java) = system_java() else {
            eprintln!("系统上没有 java，跳过");
            return;
        };
        match probe_major_version(&java) {
            Ok(major) => assert!(major >= 8, "探出来的主版本不合理：{major}"),
            Err(error) => panic!("探不到 {java:?} 的版本：{error}"),
        }
    }
}
