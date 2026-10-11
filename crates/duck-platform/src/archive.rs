//! `tar.gz` 归档读写：给「数据备份 / 恢复」用。
//!
//! 为什么是 `tar.gz` 而不是 `zip`：本仓 `rust-version = "1.85"`，`zip` 8.x/9.x 的
//! MSRV 是 1.88（会把构建顶破），插件市场链路已经因此选了 `tar` + `flate2`
//! （见根 `Cargo.toml` 的注释）。这里沿用同一对依赖，不新增任何传递依赖。
//!
//! 两条硬规则，读写两侧对称：
//!
//! 1. **打包时跳过链接**：归档里绝不留符号链接 / 硬链接条目。解包侧一律拒链接
//!    （链接能让后续条目写到目标目录之外，是归档最经典的逃逸手法），所以如果
//!    打包侧把链接原样存进去，就会出现「自己导出的包自己导不回来」。
//! 2. **解包时双重防线**：`tar::Entry::unpack_in` 拒绝对路径与 `..` 逃逸，
//!    外加一个解包后总大小的上限，避免压缩炸弹。
//!
//! 本模块只做文件字节搬运，不认识「备份」这一层语义（manifest 字段、包含哪些
//! 目录由调用方决定）。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// 解包后允许的最大总字节数（防压缩炸弹：几 KB 的归档可以解出 TB 级内容）。
const MAX_EXTRACT_BYTES: u64 = 512 * 1024 * 1024;

/// 归档操作失败。
#[derive(Debug)]
pub enum ArchiveError {
    /// 文件系统 / 读写失败。
    Io(io::Error),
    /// 归档损坏或不是合法的 gzip / tar 流。
    Malformed(String),
    /// 某条目会写到目标目录之外（绝对路径、`..` 逃逸）。
    Escape(String),
    /// 归档里含链接条目；解包侧一律拒绝。
    Link(String),
    /// 解包总大小超过 [`MAX_EXTRACT_BYTES`]。
    TooLarge(u64),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "archive I/O failed: {error}"),
            Self::Malformed(detail) => write!(f, "malformed archive: {detail}"),
            Self::Escape(name) => write!(f, "archive entry escapes the destination: {name}"),
            Self::Link(name) => write!(f, "archive contains a link entry: {name}"),
            Self::TooLarge(limit) => {
                write!(f, "extracted size exceeds the {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 一条待写入的成员内容来源。
pub enum Source {
    /// 单个文件（权限与 mtime 原样保留）。
    File(PathBuf),
    /// 目录：递归收录到 `name/` 前缀下。
    Dir(PathBuf),
    /// 直接给字节（用于 manifest.json 这类临时内容）。
    Bytes(Vec<u8>),
}

/// 一条待写入的归档成员。
pub struct Entry {
    /// 归档内路径（POSIX 风格，不含前导斜杠）。
    pub name: String,
    /// 内容来源。
    pub source: Source,
}

/// 归档成员清单项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// 归档内路径。
    pub name: String,
    /// 未压缩的字节数。
    pub size: u64,
    /// 是否是目录条目。
    pub is_dir: bool,
}

/// 把 `entries` 写成一个 `tar.gz`。
///
/// 返回写入的压缩后字节数。`dest` 的父目录必须已存在。链接条目会被**跳过**
/// （只记一条 warn），保证产出的包一定能被 [`extract`] 收回。
pub fn create(dest: &Path, entries: &[Entry]) -> Result<u64, ArchiveError> {
    let file = File::create(dest)?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.mode(tar::HeaderMode::Deterministic);

    for entry in entries {
        match &entry.source {
            Source::Bytes(data) => {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, &entry.name, data.as_slice())?;
            }
            Source::File(path) => {
                builder.append_path_with_name(path, &entry.name)?;
            }
            Source::Dir(path) => {
                builder.append_dir(&entry.name, path)?;
                append_dir(&mut builder, &entry.name, path)?;
            }
        }
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(fs::metadata(dest)?.len())
}

/// 递归收录一个目录；**跳过链接**并把跳过的事记进日志。
fn append_dir<W: io::Write>(
    builder: &mut tar::Builder<W>,
    prefix: &str,
    dir: &Path,
) -> Result<(), ArchiveError> {
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    // 排序让同一个目录每次产出相同的条目顺序（备份包更易比对）。
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let name = format!("{prefix}/{}", child.file_name().to_string_lossy());
        let path = child.path();
        let file_type = child.file_type()?;
        if file_type.is_symlink() {
            tracing::warn!(path = %path.display(), "skipping a link while archiving");
            continue;
        }
        if file_type.is_dir() {
            builder.append_dir(&name, &path)?;
            append_dir(builder, &name, &path)?;
        } else {
            builder.append_path_with_name(&path, &name)?;
        }
    }
    Ok(())
}

/// 列出归档里的成员（不解包）。
pub fn list(src: &Path) -> Result<Vec<Member>, ArchiveError> {
    let file = File::open(src)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut members = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        members.push(Member {
            name: entry.path()?.to_string_lossy().into_owned(),
            size: entry.size(),
            is_dir: entry.header().entry_type().is_dir(),
        });
    }
    Ok(members)
}

/// 读出单个成员的全部字节；成员不存在时返回 `None`。
///
/// 用于「先读 manifest 校验、再决定要不要解包」的流程，避免为一个 JSON 就把
/// 整个归档铺到磁盘上。
pub fn read_member(src: &Path, name: &str) -> Result<Option<Vec<u8>>, ArchiveError> {
    let file = File::open(src)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path.trim_end_matches('/') == name.trim_end_matches('/') {
            let mut data = Vec::with_capacity(entry.size().min(1 << 20) as usize);
            entry.read_to_end(&mut data)?;
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// 把归档解包到 `dest`（目录会自动建立）。
///
/// 拒绝一切链接条目与逃逸路径——这是解包侧的安全边界，不因调用方是「自己人」而放宽：
/// 用户的备份包来自磁盘上的任意位置，可能被换过。
pub fn extract(src: &Path, dest: &Path) -> Result<Vec<Member>, ArchiveError> {
    fs::create_dir_all(dest)?;
    let file = File::open(src)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut members = Vec::new();
    let mut total: u64 = 0;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(ArchiveError::Link(name));
        }

        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACT_BYTES {
            return Err(ArchiveError::TooLarge(MAX_EXTRACT_BYTES));
        }

        let is_dir = kind.is_dir();
        // `unpack_in` 会拒绝绝对路径与 `..` 逃逸（逃逸时返回 `Ok(false)`）。
        if !entry.unpack_in(dest)? {
            return Err(ArchiveError::Escape(name));
        }
        members.push(Member {
            name,
            size: entry.size(),
            is_dir,
        });
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::{ArchiveError, Entry, Source, create, extract, list, read_member};
    use std::path::{Path, PathBuf};

    /// 独立的临时目录；进程内自增序号避免并行测试撞同一个目录。
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("duck-archive-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建立临时目录");
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建立父目录");
        }
        std::fs::write(path, content).expect("写文件");
    }

    #[test]
    fn round_trips_files_and_directories() {
        let dir = scratch("round-trip");
        let src = dir.join("src");
        write(&src.join("config/kv.json"), "{\"a\":1}");
        write(&src.join("plugins/demo/plugin.toml"), "id = \"demo\"");
        let archive = dir.join("backup.tar.gz");

        create(
            &archive,
            &[
                Entry {
                    name: "manifest.json".to_owned(),
                    source: Source::Bytes(b"{\"format\":1}".to_vec()),
                },
                Entry {
                    name: "data".to_owned(),
                    source: Source::Dir(src.clone()),
                },
            ],
        )
        .expect("打包");

        let members = list(&archive).expect("列成员");
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"manifest.json"),
            "清单在归档顶层：{names:?}"
        );
        assert!(
            names.contains(&"data/config/kv.json"),
            "目录递归收录且带前缀：{names:?}"
        );

        let manifest = read_member(&archive, "manifest.json").expect("读成员");
        assert_eq!(manifest.as_deref(), Some(&b"{\"format\":1}"[..]));
        assert!(
            read_member(&archive, "not-there")
                .expect("读缺失成员")
                .is_none(),
            "缺失成员返回 None 而不是报错"
        );

        let out = dir.join("out");
        extract(&archive, &out).expect("解包");
        assert_eq!(
            std::fs::read_to_string(out.join("data/config/kv.json")).unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("data/plugins/demo/plugin.toml")).unwrap(),
            "id = \"demo\""
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 打包侧跳过链接 → 自产的包一定能被解包侧收下。
    #[cfg(unix)]
    #[test]
    fn skips_links_so_its_own_archive_always_extracts() {
        let dir = scratch("links");
        let src = dir.join("src");
        write(&src.join("real.txt"), "hello");
        std::os::unix::fs::symlink(src.join("real.txt"), src.join("alias.txt")).expect("建软链");
        let archive = dir.join("backup.tar.gz");

        create(
            &archive,
            &[Entry {
                name: "data".to_owned(),
                source: Source::Dir(src),
            }],
        )
        .expect("打包");

        let names: Vec<String> = list(&archive)
            .expect("列成员")
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert!(
            !names.iter().any(|name| name.ends_with("alias.txt")),
            "链接条目必须被跳过：{names:?}"
        );

        extract(&archive, &dir.join("out")).expect("自产包必须能解包");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手工塞一个逃逸路径进去：解包必须拒绝，而不是写到目标目录之外。
    ///
    /// 注意构造方式：`tar` 自己的 `append_data` / `Header::set_path` 就会拒绝 `..`，
    /// 所以这里先设一个占位路径、再直接改写 header 的 `name` 字节 —— 模拟一个
    /// 「由别的工具产出的」恶意归档。
    #[test]
    fn rejects_escaping_entries() {
        let dir = scratch("escape");
        let archive = dir.join("evil.tar.gz");
        {
            let file = std::fs::File::create(&archive).expect("建归档");
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_path("placeholder").expect("占位路径");
            let evil = b"../escaped.txt";
            header.as_gnu_mut().expect("gnu header").name[..evil.len()].copy_from_slice(evil);
            header.set_cksum();
            builder
                .append(&header, "evil".as_bytes())
                .expect("写入逃逸条目");
            builder
                .into_inner()
                .expect("收尾 tar")
                .finish()
                .expect("收尾 gzip");
        }

        let error = extract(&archive, &dir.join("out")).expect_err("逃逸条目必须被拒");
        assert!(
            matches!(error, ArchiveError::Escape(_)),
            "错误应是 Escape，实际 {error:?}"
        );
        assert!(
            !dir.join("escaped.txt").exists(),
            "拒绝之后不得在目标目录之外留下文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 gzip 内容要报 Malformed / Io，而不是 panic。
    #[test]
    fn rejects_garbage_input() {
        let dir = scratch("garbage");
        let archive = dir.join("garbage.tar.gz");
        std::fs::write(&archive, b"this is not a gzip stream").expect("写垃圾文件");
        assert!(list(&archive).is_err(), "垃圾输入必须报错");

        let missing = dir.join("nope.tar.gz");
        assert!(
            matches!(
                list(&missing).expect_err("缺失文件必须报错"),
                ArchiveError::Io(_)
            ),
            "缺失文件应报 Io"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
