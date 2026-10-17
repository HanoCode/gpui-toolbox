//! 解包 `tar.zst` 驱动工件：**解到临时目录 → 认形状 → 挪到位**。
//!
//! # 工件长什么样（实测自 dbx 的 `agents-v0.2.142`）
//!
//! ```text
//! agent-registry.json                      ← 一条驱动的元数据（键 / 版本 / 工件名）
//! drivers/dbx-agent-dameng-0.1.92.jar      ← jar 轨载荷
//! drivers/dbx-agent-oracle-0.1.71-macos-aarch64   ← native 轨载荷（可执行）
//! ```
//!
//! 即「一条驱动一个包」，包内只有这两样东西。安装落位是 dbx 的布局：
//! `<base>/drivers/<key>/agent.jar` 或 `<base>/drivers/<key>/agent`。
//!
//! # 安全检查
//!
//! - 条目名必须**相对**且不含 `..`（`tar` 的 `unpack_in` 本来就会拒绝，这里显式报错
//!   而不是静默跳过：静默跳过会让一个恶意包表现为「装完了但跑不起来」，比直接失败难查）。
//! - 载荷必须是包内 `drivers/` 下的**单个**文件；多了少了都拒。
//! - 载荷类型（jar / native）由**解法**决定而不是注册表声明 —— 注册表说的和包里装的
//!   不一致时，以包为准并记一条警告，因为能跑起来的才是真相。
//! - 摘要关口在**解包之前**（[`crate::download::sha256_file`]）：解包后写盘的东西
//!   只在摘要对上时才会被挪进安装目录。

use std::path::{Path, PathBuf};

use crate::Result;
use crate::registry::{ArtifactKind, DriverRegistry};

/// 载荷在包内的目录。
const PAYLOAD_DIR: &str = "drivers";

/// 包内元数据文件名。
const MANIFEST_NAME: &str = "agent-registry.json";

/// 解出来的载荷。
#[derive(Debug, Clone)]
pub struct Payload {
    /// 包内载荷的**绝对**路径（解包目录里）。
    pub path: PathBuf,
    /// 轨别（由文件名推断）。
    pub kind: ArtifactKind,
}

/// 把 `archive` 解到 `dest`，**按内容判断是不是 zstd**。
///
/// 离线包导出成普通 tar（我们只有 zstd 解码器，没有压缩器），而工件是 `tar.zst`；
/// 导入路径两种都可能拿到，所以按魔数分支，而不是让调用方猜。
///
/// # Errors
/// 与 [`extract_tar_zstd`] 相同；普通 tar 读不动时按 `Format` 上报。
pub fn extract_tar_any(archive: &Path, dest: &Path) -> Result<()> {
    if is_zstd(archive)? {
        return extract_tar_zstd(archive, dest);
    }
    let file = std::fs::File::open(archive)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", archive.display())))?;
    extract_tar_reader(std::io::BufReader::new(file), dest)
}

/// 头四个字节是 zstd 的魔数（`28 B5 2F FD`）。
fn is_zstd(archive: &Path) -> Result<bool> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(archive)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", archive.display())))?;
    let mut magic = [0u8; 4];
    let read = file
        .read(&mut magic)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", archive.display())))?;
    Ok(read == magic.len() && magic == [0x28, 0xB5, 0x2F, 0xFD])
}

/// 把 `archive` 解到 `dest`（调用方负责 `dest` 是干净的空目录）。
///
/// # Errors
/// 打不开归档、zstd 解码失败、条目名不安全、写盘失败。
pub fn extract_tar_zstd(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", archive.display())))?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(file)).map_err(
        |error| {
            crate::DriverError::Format(format!("{}: not a zstd stream: {error}", archive.display()))
        },
    )?;
    extract_tar_reader(decoder, dest)
}

/// 通用的「从任意 `Read` 解一份 tar」。
fn extract_tar_reader<R: std::io::Read>(reader: R, dest: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(reader);

    std::fs::create_dir_all(dest)
        .map_err(|error| crate::DriverError::Io(format!("{}: {error}", dest.display())))?;

    let entries = tar
        .entries()
        .map_err(|error| crate::DriverError::Format(format!("tar entries: {error}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|error| crate::DriverError::Format(format!("tar entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| crate::DriverError::Format(format!("tar entry path: {error}")))?
            .into_owned();
        if !is_safe_relative(&path) {
            return Err(crate::DriverError::Format(format!(
                "unsafe archive entry: {}",
                path.display()
            )));
        }
        // `unpack_in` 保住权限位（native 载荷的可执行位就在这里落下来），
        // 返回 `false` 表示它按自己的规则跳过了这个条目 —— 与上面的检查同源。
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|error| crate::DriverError::Io(format!("{}: {error}", dest.display())))?;
        if !unpacked {
            return Err(crate::DriverError::Format(format!(
                "archive entry was refused by the extractor: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// 在解包目录里找载荷。
///
/// # Errors
/// `drivers/` 不存在、里面不是恰好一个文件时返回
/// [`DriverError::Format`](crate::DriverError::Format)。
pub fn find_payload(dir: &Path) -> Result<Payload> {
    let payload_dir = dir.join(PAYLOAD_DIR);
    let entries = std::fs::read_dir(&payload_dir).map_err(|error| {
        crate::DriverError::Format(format!("{}: {error}", payload_dir.display()))
    })?;
    let files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        // macOS 打出来的归档会带 AppleDouble 影子文件（`._name`）与 `.DS_Store`；
        // 它们不是载荷，混进来会让「恰好一个文件」的判断误报。真包在 Linux 上打，
        // 但**自己导出的离线包**将来可能在 macOS 上打 —— 宽容一点没有代价。
        .filter(|path| !is_ignorable_entry(path))
        .collect();
    let [path] = files.as_slice() else {
        return Err(crate::DriverError::Format(format!(
            "the package must contain exactly one file under {PAYLOAD_DIR}/, found {}",
            files.len()
        )));
    };
    let kind = classify_payload(path).ok_or_else(|| {
        crate::DriverError::Format(format!(
            "cannot tell whether {} is a jar or a native payload",
            path.display()
        ))
    })?;
    Ok(Payload {
        path: path.clone(),
        kind,
    })
}

/// 归档里的「非载荷」条目：AppleDouble 影子文件、`.DS_Store`、点开头的东西。
fn is_ignorable_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') || name == "._")
}

/// 读包内的元数据（`agent-registry.json`）。缺文件时返回 `None`（**不算错**：
/// 元数据只用于校验与展示，缺了不影响安装）。
///
/// # Errors
/// 文件在、但读不动或不是合法 JSON。
pub fn read_package_manifest(dir: &Path) -> Result<Option<DriverRegistry>> {
    let path = dir.join(MANIFEST_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    DriverRegistry::read_file(&path).map(Some)
}

/// 按文件名判断轨别：`.jar` = jar 轨，其余 = native 轨。
#[must_use]
pub fn classify_payload(path: &Path) -> Option<ArtifactKind> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".jar") {
        return Some(ArtifactKind::Jar);
    }
    // native 载荷的名字形如 `dbx-agent-<key>-<version>[-<platform>]`；只要它有名字
    // 且不是 jar，就当原生件 —— 是不是真的能跑由启动时的失败说话。
    (!name.is_empty() && !name.starts_with('.')).then_some(ArtifactKind::Native)
}

/// 条目名必须是相对的、且不含 `..` 或根锚。
fn is_safe_relative(path: &Path) -> bool {
    use std::path::Component;
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        && path.components().next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对路径放行，绝对路径与 `..` 一律拒绝。
    #[test]
    fn unsafe_entry_names_are_rejected() {
        assert!(is_safe_relative(Path::new("drivers/agent")));
        assert!(is_safe_relative(Path::new("./drivers/agent")));
        assert!(!is_safe_relative(Path::new("/etc/passwd")));
        assert!(!is_safe_relative(Path::new("drivers/../../etc/passwd")));
        assert!(!is_safe_relative(Path::new("")));
    }

    /// 轨别只看文件名后缀。
    #[test]
    fn payload_kind_comes_from_the_file_name() {
        assert_eq!(
            classify_payload(Path::new("drivers/dbx-agent-dameng-0.1.92.jar")),
            Some(ArtifactKind::Jar)
        );
        assert_eq!(
            classify_payload(Path::new("drivers/dbx-agent-oracle-0.1.71-macos-aarch64")),
            Some(ArtifactKind::Native)
        );
        assert_eq!(classify_payload(Path::new("drivers/.DS_Store")), None);
    }

    /// 包内没有 `drivers/` 时给一条说得清的错误。
    #[test]
    fn a_package_without_payload_is_reported() {
        let dir = std::env::temp_dir().join("duck-drivers-archive-test-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let error = find_payload(&dir).expect_err("没有 drivers/ 必须报错");
        assert!(matches!(error, crate::DriverError::Format(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 载荷多于一个也拒（包形状不对时宁可报错，也别猜装哪个）。
    #[test]
    fn a_package_with_two_payloads_is_reported() {
        let dir = std::env::temp_dir().join("duck-drivers-archive-test-two");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(PAYLOAD_DIR)).expect("建临时目录");
        std::fs::write(dir.join(PAYLOAD_DIR).join("a.jar"), b"x").expect("写文件");
        std::fs::write(dir.join(PAYLOAD_DIR).join("b"), b"y").expect("写文件");
        let error = find_payload(&dir).expect_err("两个载荷必须报错");
        assert!(matches!(error, crate::DriverError::Format(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 完整走一遍：造一个真的 `tar.zst`，解出来、认载荷、读元数据。
    ///
    /// 这条是本模块唯一的**端到端**用例：它同时钉住「zstd 解码能读真包」与
    /// 「包形状认得出来」。tar 由 `tar` crate 现打（**不借宿主 `tar`**：macOS 的
    /// bsdtar 会塞 AppleDouble 影子文件，上一次就因此误报），压缩那一步才用系统
    /// `zstd`（宿主机没有就跳过 —— `cargo test` 不该因为少一个外部工具而红）。
    #[test]
    fn a_real_tar_zstd_package_round_trips() {
        let dir = std::env::temp_dir().join("duck-drivers-archive-test-real");
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(src.join(PAYLOAD_DIR)).expect("建源目录");
        std::fs::write(
            src.join(MANIFEST_NAME),
            br#"{"drivers":{"dameng":{"version":"0.1.92"}}}"#,
        )
        .expect("写元数据");
        std::fs::write(
            src.join(PAYLOAD_DIR).join("dbx-agent-dameng-0.1.92.jar"),
            b"jar-bytes",
        )
        .expect("写载荷");

        let tar_path = dir.join("pkg.tar");
        write_tar(&src, &tar_path);
        let archive = dir.join("pkg.tar.zst");
        if !zstd_compress(&tar_path, &archive) {
            eprintln!("zstd 不可用，跳过端到端用例");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let dest = dir.join("out");
        extract_tar_zstd(&archive, &dest).expect("解包");
        let payload = find_payload(&dest).expect("认载荷");
        assert_eq!(payload.kind, ArtifactKind::Jar);
        assert_eq!(std::fs::read(&payload.path).expect("读载荷"), b"jar-bytes");
        let manifest = read_package_manifest(&dest)
            .expect("读元数据")
            .expect("包内有元数据");
        assert_eq!(
            manifest.driver("dameng").map(|info| info.version.as_str()),
            Some("0.1.92")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出侧用的打包方式（普通 tar，含 `drivers/<file>`），顺便测 `extract_tar_any`
    /// 认出「这不是 zstd」也能解。
    #[test]
    fn a_plain_tar_is_extracted_too() {
        let dir = std::env::temp_dir().join("duck-drivers-archive-test-plain");
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(src.join(PAYLOAD_DIR)).expect("建源目录");
        std::fs::write(src.join(PAYLOAD_DIR).join("agent"), b"native-bytes").expect("写载荷");
        let archive = dir.join("pkg.tar");
        write_tar(&src, &archive);

        let dest = dir.join("out");
        extract_tar_any(&archive, &dest).expect("解普通 tar");
        let payload = find_payload(&dest).expect("认载荷");
        assert_eq!(payload.kind, ArtifactKind::Native);
        assert_eq!(
            std::fs::read(&payload.path).expect("读载荷"),
            b"native-bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用 `tar` crate 把 `dir` 打成 `out`（条目名不带 `./` 前缀）。
    fn write_tar(dir: &Path, out: &Path) {
        let file = std::fs::File::create(out).expect("建归档");
        let mut builder = tar::Builder::new(file);
        builder.append_dir_all(".", dir).expect("写归档内容");
        builder.finish().expect("收尾");
    }

    /// 用系统 `zstd` 压一次；不可用返回 `false`。
    fn zstd_compress(input: &Path, output: &Path) -> bool {
        std::process::Command::new("zstd")
            .arg("-q")
            .arg("-f")
            .arg("-o")
            .arg(output)
            .arg(input)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
