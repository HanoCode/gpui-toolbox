//! 插件包的校验、解包、落位与卸载。
//!
//! # 顺序是有理由的
//!
//! 下载（流式算摘要）→ **重读落盘的文件再算一遍**（下载时那个只是早退信号）
//! → 对照索引承诺的字节数 → 解包到 `plugins/` 内的暂存目录 → 用
//! [`duck_plugin::PluginManifest`] 复核清单，要求 id / version 与索引一致、
//! **且声明的能力不超出索引声明的那一份** → 原子改名落位。
//!
//! 下载这一步本身按 [`crate::ArchiveRef::urls`] 的顺序试来源：主地址（通常是 CDN）
//! 取不到就换备用地址，全部失败才报错。摘要与字节数校验对**最终落盘的那份字节**
//! 无条件成立——换个来源并不放宽任何一条校验。
//!
//! 暂存目录刻意放在 `plugins/` **里面**（名字以 `.` 开头，被目录发现跳过）：
//! 只有同盘才能用 `rename` 原子替换，跨盘会退化成「拷贝 + 删除」，中途失败就会
//! 留下半个插件。

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use duck_plugin::{Capability, MANIFEST_FILE, PluginManifest};

use crate::error::{EntryProblem, RegistryError};
use crate::fetch::{DOWNLOAD_TIMEOUT, agent};
use crate::index::{RegistryEntry, is_valid_plugin_id};
use crate::sha256::{Hasher, matches};

/// 解包总量上限：插件是脚本与少量资源，超过这个量级一定是坏的或恶意的。
const MAX_EXTRACT_BYTES: u64 = 64 * 1024 * 1024;

/// 允许比承诺大出多少才判定「这不是我要的东西」。
const OVERSHOOT_ALLOWANCE: u64 = 64 * 1024;

/// 读 socket / 写文件的缓冲块。
const CHUNK: usize = 64 * 1024;

/// 进度回调告诉下载器下一步做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// 继续。
    Continue,
    /// 中止：半份归档会被删掉，比一份都没有更糟。
    Abort,
}

/// 一次成功的安装。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    id: String,
    version: String,
    dir: PathBuf,
}

impl Installed {
    /// 插件 id。
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 装上的版本。
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 落位目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// 下载并安装索引里的一条插件。
///
/// # Errors
/// 见 [`RegistryError`]。
pub fn install(
    entry: &RegistryEntry,
    plugins_dir: &Path,
    cache_dir: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<Installed, RegistryError> {
    let id = entry.id();
    if !is_valid_plugin_id(id) {
        return Err(RegistryError::InvalidEntry {
            id: id.to_string(),
            problem: EntryProblem::Id(id.to_string()),
        });
    }

    fs::create_dir_all(cache_dir).map_err(io)?;
    let archive = cache_dir.join(format!("{id}-{}.tar.gz", entry.version()));
    download(entry, &archive, progress)?;

    let outcome = install_from_archive(entry, &archive, plugins_dir);
    // 无论成败，归档都不必留着：它已经被校验并解包过了。
    let _ = fs::remove_file(&archive);
    outcome
}

/// 从一份**已经在本地**的归档安装（下载与解包分离，便于单测与复用）。
///
/// 归档必须已经由调用方取得；本函数负责字节数校验、摘要校验、解包、清单复核与落位。
///
/// # Errors
/// 见 [`RegistryError`]。
pub fn install_from_archive(
    entry: &RegistryEntry,
    archive: &Path,
    plugins_dir: &Path,
) -> Result<Installed, RegistryError> {
    let id = entry.id();
    if !is_valid_plugin_id(id) {
        return Err(RegistryError::InvalidEntry {
            id: id.to_string(),
            problem: EntryProblem::Id(id.to_string()),
        });
    }
    fs::create_dir_all(plugins_dir).map_err(io)?;

    // 1. 字节数：索引承诺了多少，盘上就该是多少。
    let actual_size = fs::metadata(archive).map_err(io)?.len();
    let expected_size = entry.archive().size();
    if actual_size != expected_size {
        return Err(RegistryError::SizeMismatch {
            expected: expected_size,
            actual: actual_size,
        });
    }

    // 2. 摘要：**重读落盘的字节**再算一遍。下载时算的那个只是早退信号。
    let actual_digest = digest_of(archive)?;
    if !matches(entry.archive().sha256(), &actual_digest) {
        return Err(RegistryError::Digest {
            expected: entry.archive().sha256().to_string(),
            actual: actual_digest,
        });
    }

    // 3. 解包到同盘暂存目录（`.` 开头 → 目录发现会跳过它）。
    let staging = plugins_dir.join(format!(".tmp-{id}-{}", unique_suffix()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(io)?;

    let prepared = prepare(entry, archive, &staging);
    let root = match prepared {
        Ok(root) => root,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    // 4. 落位：目标已存在时先挪开，失败可回滚。
    let target = plugins_dir.join(id);
    let backup = plugins_dir.join(format!(".old-{id}-{}", unique_suffix()));
    let had_target = target.exists();
    if had_target {
        fs::rename(&target, &backup).map_err(io)?;
    }
    if let Err(error) = fs::rename(&root, &target) {
        if had_target {
            let _ = fs::rename(&backup, &target);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(io(error));
    }
    if had_target {
        let _ = fs::remove_dir_all(&backup);
    }
    // `root` 可能就在 staging 里（顶层直接是清单），此时这一句是空操作。
    let _ = fs::remove_dir_all(&staging);

    tracing::info!(plugin = id, version = entry.version(), "plugin installed");
    Ok(Installed {
        id: id.to_string(),
        version: entry.version().to_string(),
        dir: target,
    })
}

/// 卸载一个插件：删除它在 `plugins/` 下的目录。
///
/// id 必须合法（于是不含路径分隔符与 `..`），且只动 `plugins_dir` 的直接子目录——
/// 这两条一起把路径穿越挡在门外。目录不存在视为成功。
///
/// # Errors
/// 见 [`RegistryError`]。
pub fn uninstall(plugin_id: &str, plugins_dir: &Path) -> Result<(), RegistryError> {
    if !is_valid_plugin_id(plugin_id) {
        return Err(RegistryError::InvalidEntry {
            id: plugin_id.to_string(),
            problem: EntryProblem::Id(plugin_id.to_string()),
        });
    }
    let target = plugins_dir.join(plugin_id);
    if target.parent() != Some(plugins_dir) {
        return Err(RegistryError::Io(format!(
            "refusing to remove {}: it is not a direct child of the plugin directory",
            target.display()
        )));
    }
    if !target.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&target).map_err(io)?;
    tracing::info!(plugin = plugin_id, "plugin uninstalled");
    Ok(())
}

/// 解包 + 清单复核，返回**内容根目录**（含 `plugin.toml` 的那一层）。
fn prepare(
    entry: &RegistryEntry,
    archive: &Path,
    staging: &Path,
) -> Result<PathBuf, RegistryError> {
    extract(archive, staging)?;
    let root = content_root(staging)?;
    let manifest = PluginManifest::load(&root.join(MANIFEST_FILE))
        .map_err(|error| RegistryError::Manifest(error.to_string()))?;
    if manifest.id != entry.id() {
        return Err(RegistryError::Manifest(format!(
            "the package declares plugin id {:?} but the index says {:?}",
            manifest.id,
            entry.id()
        )));
    }
    if manifest.version != entry.version() {
        return Err(RegistryError::Manifest(format!(
            "the package declares version {:?} but the index says {:?}",
            manifest.version,
            entry.version()
        )));
    }

    // 能力是唯一一处「索引里的元数据不止用于展示」：授权照归档清单授予，而
    // 用户在安装前点头看的是索引那份。归档若能多声明，确认框就形同虚设。
    // 只能要求「不超出」——包比索引少声明是好事，不该拦。
    let advertised = Capability::set_of(entry.capabilities());
    let declared = Capability::set_of(&manifest.capabilities);
    let mut extra: Vec<String> = declared
        .difference(&advertised)
        .map(|capability| format!("{capability:?}"))
        .collect();
    if !extra.is_empty() {
        // 排序只为让日志与测试断言确定；`HashSet` 的迭代序不稳定。
        extra.sort();
        return Err(RegistryError::Manifest(format!(
            "the package declares capabilities the index did not advertise: {}",
            extra.join(", ")
        )));
    }
    Ok(root)
}

/// 解包，并把每一条归档条目按安全规则过一遍。
///
///
/// 链接（符号链接 / 硬链接）一律拒绝：它们能让后续条目写到解包目录之外，
/// 是归档解包最经典的一类逃逸手法。插件包没有正当理由包含链接。
fn extract(archive: &Path, destination: &Path) -> Result<(), RegistryError> {
    let file = File::open(archive).map_err(io)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut total: u64 = 0;

    let entries = tar.entries().map_err(extract_err)?;
    for entry in entries {
        let mut entry = entry.map_err(extract_err)?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(RegistryError::Extract(
                "the archive contains a link entry".into(),
            ));
        }

        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACT_BYTES {
            return Err(RegistryError::Extract(format!(
                "the extracted size exceeds {MAX_EXTRACT_BYTES} bytes"
            )));
        }

        // `unpack_in` 会拒绝绝对路径与 `..` 逃逸（逃逸时返回 `Ok(false)`）。
        if !entry.unpack_in(destination).map_err(extract_err)? {
            return Err(RegistryError::Extract(
                "an archive entry would escape the destination".into(),
            ));
        }
    }
    Ok(())
}

/// 找到含 `plugin.toml` 的内容根。
///
/// 接受两种打包习惯：清单在归档顶层，或整个插件在一个顶层目录里
/// （`tar czf pkg.tar.gz text-stat/`）。多层嵌套一律拒绝，不做猜测。
fn content_root(staging: &Path) -> Result<PathBuf, RegistryError> {
    if staging.join(MANIFEST_FILE).is_file() {
        return Ok(staging.to_path_buf());
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(staging).map_err(io)? {
        let path = entry.map_err(io)?.path();
        if path.is_dir() && path.join(MANIFEST_FILE).is_file() {
            candidates.push(path);
        }
    }
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(RegistryError::Manifest(format!(
            "the package has no {MANIFEST_FILE} at its root"
        ))),
        _ => Err(RegistryError::Manifest(
            "the package has several top-level plugin roots".into(),
        )),
    }
}

/// 下载归档：按 [`crate::ArchiveRef::urls`] 的顺序试每一个来源。
///
/// 主地址失败就换备用地址——CDN 那条加速通道的「备用那一半」就是靠这个生效的。
/// **全部失败**才报错，报的是**最后一个**地址的错误：那是最接近「现在到底怎么了」
/// 的一条。
fn download(
    entry: &RegistryEntry,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<(), RegistryError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }

    let outcome = first_success(entry.archive().urls(), |url| {
        download_one(url, entry.archive().size(), destination, progress)
    });

    if outcome.is_err() {
        // 收尾放在这里，而不是散在每条失败分支上：试下一个来源时那个文件马上会被
        // `File::create` 截断重写，删它纯属多余。但**全部失败**之后必须删——
        // 半份归档比一份都没有更糟，它长得像一份真的包，却会在摘要校验那里报一个
        // 与真实原因（网络断了）毫无关系的错。
        let _ = fs::remove_file(destination);
    }
    outcome
}

/// 按顺序试每一个来源，返回**第一个**成功的结果。
///
/// 中止（[`RegistryError::Cancelled`]）是唯一会立刻穿透出来的错误：那是用户说
/// 「别下了」，拿这个意图去试下一个来源毫无意义。其余错误都先记着往下试，全部失败
/// 才把**最后一个**交出去——那是最接近「现在到底怎么了」的一条。
///
/// 抽成独立函数是为了它能在没有网络的情况下被测到：来源列表与尝试动作都是参数。
fn first_success<'a, T>(
    urls: impl Iterator<Item = &'a str>,
    mut attempt: impl FnMut(&str) -> Result<T, RegistryError>,
) -> Result<T, RegistryError> {
    let mut last: Option<RegistryError> = None;

    for (index, url) in urls.enumerate() {
        if index > 0 {
            tracing::warn!(%url, "the previous plugin download source failed; trying the next one");
        }
        match attempt(url) {
            Ok(value) => return Ok(value),
            Err(RegistryError::Cancelled) => return Err(RegistryError::Cancelled),
            Err(error) => last = Some(error),
        }
    }

    // `urls()` 至少有一个元素（主地址），所以这里不会落空；写兜底而不是 `expect`，
    // 是为了将来真把主地址改成可选时不会炸在这里。
    Err(last
        .unwrap_or_else(|| RegistryError::Download("the archive has no download url".to_owned())))
}

/// 从**一个**地址流式写到 `destination`：边收边写，超限或中止即返回错误。
///
/// 收尾（删掉半份文件）由 [`download`] 统一做——这里只回答「这一条试得怎么样」。
fn download_one(
    url: &str,
    total: u64,
    destination: &Path,
    progress: &dyn Fn(u64, u64) -> Flow,
) -> Result<(), RegistryError> {
    // `index::parse` 已经拒过非 https；这里是那条腰带的背带。
    if !url.starts_with("https://") {
        return Err(RegistryError::Download(format!(
            "not an https url: {url:?}"
        )));
    }

    let agent = agent(DOWNLOAD_TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| RegistryError::Download(error.to_string()))?;

    let mut out = File::create(destination).map_err(io)?;
    let limit = total.saturating_add(OVERSHOOT_ALLOWANCE);
    let mut buffer = vec![0u8; CHUNK];
    let mut written: u64 = 0;

    if progress(0, total) == Flow::Abort {
        return Err(RegistryError::Cancelled);
    }

    let mut body = response.body_mut().as_reader();
    loop {
        let read = body
            .read(&mut buffer)
            .map_err(|error| RegistryError::Download(error.to_string()))?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read]).map_err(io)?;
        written += read as u64;

        if written > limit {
            return Err(RegistryError::SizeMismatch {
                expected: total,
                actual: written,
            });
        }
        if progress(written, total) == Flow::Abort {
            return Err(RegistryError::Cancelled);
        }
    }

    out.flush().map_err(io)?;
    out.sync_all().map_err(io)?;

    if written != total {
        return Err(RegistryError::SizeMismatch {
            expected: total,
            actual: written,
        });
    }
    Ok(())
}

/// 重读一个文件，逐块算出 SHA-256。
fn digest_of(path: &Path) -> Result<String, RegistryError> {
    let mut file = File::open(path).map_err(io)?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize_hex())
}

/// 进程内唯一后缀，给暂存 / 备份目录用。
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

fn io(error: std::io::Error) -> RegistryError {
    RegistryError::Io(error.to_string())
}

fn extract_err(error: std::io::Error) -> RegistryError {
    RegistryError::Extract(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use super::{first_success, install_from_archive, uninstall};
    use crate::error::RegistryError;
    use crate::index::{RegistryEntry, parse};
    use crate::sha256::Hasher;

    const PLUGIN_ID: &str = "dev.duck.text-stat";
    const PLUGIN_VERSION: &str = "0.1.0";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "duck-plugin-registry-test-{tag}-{}",
            super::unique_suffix()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 打包一个合规插件：清单 + 一个脚本，清单在归档顶层。
    fn write_package(dest: &Path) {
        write_tar_gz(
            dest,
            &[
                ("plugin.toml".to_string(), MANIFEST.as_bytes().to_vec()),
                (
                    "main.js".to_string(),
                    b"export function handle() { return 'ok'; }".to_vec(),
                ),
            ],
        );
    }

    const MANIFEST: &str = "id = \"dev.duck.text-stat\"\n\
        name = \"文本统计\"\n\
        version = \"0.1.0\"\n\
        api_version = 1\n\
        execution_model = \"js\"\n\n\
        [[tools]]\n\
        id = \"dev.duck.text-stat.count\"\n\
        name = \"统计字数\"\n";

    fn write_tar_gz(dest: &Path, files: &[(String, Vec<u8>)]) {
        let file = fs::File::create(dest).expect("create archive");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, data.as_slice())
                .expect("append entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }

    /// 从本地归档出发构造一条索引项（先算真实摘要与大小）。
    fn entry_for(archive: &Path) -> RegistryEntry {
        entry_with(archive, None, None)
    }

    /// 同上，但索引里**带上能力声明**（`caps_json` 是对象的成员片段，空串表示不写这个字段）。
    fn entry_declaring(archive: &Path, caps_json: &str) -> RegistryEntry {
        entry_full(archive, None, None, caps_json)
    }

    fn entry_with(
        archive: &Path,
        digest_override: Option<&str>,
        size_override: Option<u64>,
    ) -> RegistryEntry {
        entry_full(archive, digest_override, size_override, "")
    }

    fn entry_full(
        archive: &Path,
        digest_override: Option<&str>,
        size_override: Option<u64>,
        caps_json: &str,
    ) -> RegistryEntry {
        let bytes = fs::read(archive).expect("read archive");
        let digest = digest_override.map_or_else(
            || {
                let mut hasher = Hasher::new();
                hasher.update(&bytes);
                hasher.finalize_hex()
            },
            str::to_owned,
        );
        let size = size_override.unwrap_or(bytes.len() as u64);
        let caps = if caps_json.is_empty() {
            String::new()
        } else {
            format!("\"capabilities\": {{{caps_json}}},")
        };
        let json = format!(
            r#"{{
              "index_version": 1,
              "plugins": [{{
                "id": "{PLUGIN_ID}",
                "name": "文本统计",
                "version": "{PLUGIN_VERSION}",
                "api_version": 1,
                "execution_model": "js",
                {caps}
                "archive": {{
                  "url": "https://example.invalid/text-stat-0.1.0.tar.gz",
                  "sha256": "{digest}",
                  "size": {size}
                }}
              }}]
            }}"#
        );
        parse(json.as_bytes())
            .expect("test index parses")
            .get(PLUGIN_ID)
            .expect("entry present")
            .clone()
    }

    #[test]
    fn installs_a_valid_package() {
        let work = temp_dir("install-ok");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_package(&archive);
        let entry = entry_for(&archive);

        let installed = install_from_archive(&entry, &archive, &plugins).expect("install");
        assert_eq!(installed.id(), PLUGIN_ID);
        assert_eq!(installed.version(), PLUGIN_VERSION);
        assert!(plugins.join(PLUGIN_ID).join("plugin.toml").is_file());
        assert!(plugins.join(PLUGIN_ID).join("main.js").is_file());

        // 暂存目录不留残迹。
        let leftovers: Vec<_> = fs::read_dir(&plugins)
            .expect("read plugins dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "staging dirs must be cleaned up");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn installs_a_package_wrapped_in_a_top_level_directory() {
        let work = temp_dir("install-wrapped");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_tar_gz(
            &archive,
            &[
                (
                    format!("{PLUGIN_ID}/plugin.toml"),
                    MANIFEST.as_bytes().to_vec(),
                ),
                (
                    format!("{PLUGIN_ID}/main.js"),
                    b"export function handle() {}".to_vec(),
                ),
            ],
        );
        let entry = entry_for(&archive);

        install_from_archive(&entry, &archive, &plugins).expect("install");
        assert!(plugins.join(PLUGIN_ID).join("plugin.toml").is_file());
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn refuses_a_digest_mismatch() {
        let work = temp_dir("digest");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_package(&archive);
        let entry = entry_with(&archive, Some(&"0".repeat(64)), None);

        let error = install_from_archive(&entry, &archive, &plugins).expect_err("must reject");
        assert!(
            matches!(error, crate::RegistryError::Digest { .. }),
            "{error:?}"
        );
        assert!(
            !plugins.join(PLUGIN_ID).exists(),
            "nothing may be installed"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn refuses_a_size_mismatch() {
        let work = temp_dir("size");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_package(&archive);
        let entry = entry_with(&archive, None, Some(7));

        let error = install_from_archive(&entry, &archive, &plugins).expect_err("must reject");
        assert!(
            matches!(error, crate::RegistryError::SizeMismatch { .. }),
            "{error:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn refuses_a_manifest_id_that_disagrees_with_the_index() {
        let work = temp_dir("id-mismatch");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_tar_gz(
            &archive,
            &[(
                "plugin.toml".to_string(),
                MANIFEST.replace(PLUGIN_ID, "dev.duck.other").into_bytes(),
            )],
        );
        let entry = entry_for(&archive);

        let error = install_from_archive(&entry, &archive, &plugins).expect_err("must reject");
        assert!(
            matches!(error, crate::RegistryError::Manifest(_)),
            "{error:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// 归档清单声明的能力**超出**索引声明时必须拒绝落盘。
    ///
    /// 授权是照归档清单授予的，而用户在安装前点头看的是索引那份。允许超出的那一刻，
    /// 确认框就只是一张好看的纸——包只要在自己清单里多写一行，就能拿到没被点过头的权限。
    #[test]
    fn refuses_capabilities_the_index_never_advertised() {
        let work = temp_dir("capability-escalation");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        // 归档：清单里偷偷加上网络访问（`[capabilities]` 是根级表，跟在 `[[tools]]` 之后也成立）。
        let manifest =
            format!("{MANIFEST}\n[capabilities]\nnetwork_hosts = [\"exfil.example.com\"]\n");
        write_tar_gz(
            &archive,
            &[("plugin.toml".to_string(), manifest.into_bytes())],
        );
        // 索引：只声明了通知。
        let entry = entry_declaring(&archive, "\"notifications\": true");

        let error = install_from_archive(&entry, &archive, &plugins).expect_err("must reject");
        let message = error.to_string();
        assert!(
            matches!(error, crate::RegistryError::Manifest(_)),
            "{error:?}"
        );
        assert!(
            message.contains("exfil.example.com"),
            "the refusal must name the extra capability, got: {message}"
        );
        assert!(
            !plugins.join(PLUGIN_ID).exists(),
            "nothing may be installed"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// 反面：包声明的能力不比索引多就该放行——不能把守法插件一起拦掉。
    #[test]
    fn accepts_capabilities_within_what_the_index_advertised() {
        let work = temp_dir("capability-ok");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        let manifest = format!(
            "{MANIFEST}\n[capabilities]\nnotifications = true\nnetwork_hosts = [\"api.example.com\"]\n"
        );
        write_tar_gz(
            &archive,
            &[("plugin.toml".to_string(), manifest.into_bytes())],
        );
        // 索引声明的比包还多一项（`filesystem_read`），包没用到也算合规。
        let entry = entry_declaring(
            &archive,
            "\"notifications\": true, \"network_hosts\": [\"api.example.com\"], \"filesystem_read\": [\"data/*\"]",
        );

        install_from_archive(&entry, &archive, &plugins).expect("install");
        assert!(plugins.join(PLUGIN_ID).join("plugin.toml").is_file());
        let _ = fs::remove_dir_all(&work);
    }

    /// 归档里 `../` 逃逸的条目必须被拒，且不能在解包目录之外留下任何东西。
    #[test]
    fn refuses_a_path_traversal_entry() {
        let work = temp_dir("traversal");
        let plugins = work.join("plugins");
        let archive = work.join("evil.tar.gz");
        write_raw_tar_gz(&archive, "../escaped.txt", b'0', b"pwn");

        let staging = plugins.join("staging");
        fs::create_dir_all(&staging).expect("staging");
        let error = super::extract(&archive, &staging).expect_err("must reject");

        assert!(
            matches!(error, crate::RegistryError::Extract(_)),
            "{error:?}"
        );
        assert!(!work.join("escaped.txt").exists(), "nothing may escape");
        assert!(!plugins.join("escaped.txt").exists());
        let _ = fs::remove_dir_all(&work);
    }

    /// 符号链接条目同样必须被拒：它能带着后续条目写到解包目录之外。
    #[test]
    fn refuses_a_link_entry() {
        let work = temp_dir("symlink");
        let archive = work.join("link.tar.gz");
        write_raw_tar_gz(&archive, "link", b'2', b"/etc/passwd");

        let staging = work.join("staging");
        fs::create_dir_all(&staging).expect("staging");
        let error = super::extract(&archive, &staging).expect_err("must reject");
        assert!(
            matches!(error, crate::RegistryError::Extract(_)),
            "{error:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn uninstall_removes_the_plugin_directory() {
        let work = temp_dir("uninstall");
        let plugins = work.join("plugins");
        let archive = work.join("pkg.tar.gz");
        write_package(&archive);
        let entry = entry_for(&archive);
        install_from_archive(&entry, &archive, &plugins).expect("install");

        uninstall(PLUGIN_ID, &plugins).expect("uninstall");
        assert!(!plugins.join(PLUGIN_ID).exists());
        // 再卸一次是空操作，不是错误。
        uninstall(PLUGIN_ID, &plugins).expect("second uninstall is a no-op");
        let _ = fs::remove_dir_all(&work);
    }

    /// 卸载的 id 若可用来做路径穿越，必须在下手之前就被拒。
    #[test]
    fn uninstall_refuses_ids_that_could_escape() {
        let work = temp_dir("uninstall-bad-id");
        let plugins = work.join("plugins");
        fs::create_dir_all(plugins.join("keep")).expect("seed");

        for id in ["..", "../keep", "a/b", ""] {
            assert!(
                uninstall(id, &plugins).is_err(),
                "{id:?} must not be uninstallable"
            );
        }
        assert!(plugins.join("keep").is_dir());
        let _ = fs::remove_dir_all(&work);
    }

    /// 手写一个 512 字节 tar 头 + 数据块，用来构造合规工具造不出来的恶意条目。
    fn write_raw_tar_gz(dest: &Path, name: &str, typeflag: u8, data: &[u8]) {
        let mut block = [0u8; 512];
        block[..name.len()].copy_from_slice(name.as_bytes());
        put_octal(&mut block[100..108], 0o644);
        put_octal(&mut block[108..116], 0);
        put_octal(&mut block[116..124], 0);
        put_octal(&mut block[124..136], data.len() as u64);
        put_octal(&mut block[136..148], 0);
        for byte in &mut block[148..156] {
            *byte = b' ';
        }
        block[156] = typeflag;
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        let sum: u64 = block.iter().map(|byte| u64::from(*byte)).sum();
        put_octal(&mut block[148..154], sum);
        block[154] = 0;
        block[155] = b' ';

        let mut payload = block.to_vec();
        payload.extend_from_slice(data);
        payload.resize(payload.len().div_ceil(512) * 512, 0);
        payload.extend_from_slice(&[0u8; 1024]);

        let file = fs::File::create(dest).expect("create archive");
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        encoder.write_all(&payload).expect("write gzip");
        encoder.finish().expect("finish gzip");
    }

    fn put_octal(field: &mut [u8], value: u64) {
        let text = format!("{:0width$o}", value, width = field.len() - 1);
        field[..text.len()].copy_from_slice(text.as_bytes());
        field[field.len() - 1] = 0;
    }

    /// 第一个成功的来源胜出，后面的**不再碰**——这是「CDN 通则不走 GitHub」的保证。
    #[test]
    fn the_first_success_wins_and_no_further_source_is_touched() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_string());
            Ok::<_, RegistryError>(format!("from {url}"))
        });
        assert_eq!(result.expect("the primary succeeds"), "from primary");
        assert_eq!(tried, ["primary"], "the mirror must not be touched");
    }

    /// 主地址失败就换备用地址：这是镜像通道的「备用那一半」，缺了它主地址一挂就全挂。
    #[test]
    fn a_failed_primary_falls_back_to_the_mirror() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_string());
            if url == "primary" {
                Err(RegistryError::Download("primary refused".into()))
            } else {
                Ok(format!("from {url}"))
            }
        });
        assert_eq!(result.expect("the mirror succeeds"), "from mirror");
        assert_eq!(tried, ["primary", "mirror"]);
    }

    /// 全部失败时报**最后一个**来源的错误：那是最接近「现在到底怎么了」的一条。
    #[test]
    fn the_last_error_is_reported_when_every_source_fails() {
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            Err::<(), _>(RegistryError::Download(format!("{url} refused")))
        });
        let error = result.expect_err("every source fails");
        assert!(error.to_string().contains("mirror refused"), "{error}");
    }

    /// 用户中止是唯一会立刻穿透的错误：拿「别下了」这个意图去试下一个来源毫无意义，
    /// 只会让用户多等一轮超时。
    #[test]
    fn a_cancellation_stops_the_fall_back() {
        let mut tried = Vec::new();
        let result = first_success(["primary", "mirror"].into_iter(), |url| {
            tried.push(url.to_string());
            Err::<(), _>(RegistryError::Cancelled)
        });
        assert!(matches!(result, Err(RegistryError::Cancelled)));
        assert_eq!(tried, ["primary"], "a cancellation must not try the mirror");
    }

    /// 来源列表为空也要有说法，不能 panic：`urls()` 保证非空，但这里不替它假设。
    #[test]
    fn an_empty_source_list_is_an_error_not_a_panic() {
        let empty: Vec<&str> = Vec::new();
        let result = first_success(empty.into_iter(), |url| {
            Ok::<_, RegistryError>(url.to_string())
        });
        assert!(matches!(result, Err(RegistryError::Download(_))));
    }
}
