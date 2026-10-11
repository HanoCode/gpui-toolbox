//! 发布脚本（`sbin/plugin_index.py`）与读端之间的**契约测试**。
//!
//! 索引的两端在两个语言里：写端是 Python 脚本，读端是本 crate。字段名写歪一个、
//! 能力多塞一个键、版本号写成非语义化——本 crate 会**拒掉整份索引**，而那时候
//! 所有用户的市场会一起变空。等 CI 发完版才发现就太晚了，所以在这里先跑一遍真东西：
//!
//! 1. 用仓库里真正的 `sbin/plugin_index.py` 打一次包、生成一份索引（Python ≥ 3.11）；
//! 2. 用 [`duck_plugin_registry::parse_index`] 读它——读端会不会认；
//! 3. 再拿索引里的条目真的**装一次**——摘要、字节数、清单 id/version、能力上限，
//!    四道校验是不是都对得上。
//!
//! 没有可用的 Python 3.11+（或找不到脚本）时**跳过**而不是失败：这个测试守的是
//! 「两端有没有漂移」，不该让缺解释器的机器编不过。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use duck_plugin_registry::{install_from_archive, parse_index};

/// 仓库根：`crates/duck-plugin-registry/../..`。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root must exist")
}

/// 找一个能读 `tomllib` 的 Python：先看 `DUCK_PYTHON`，再按名字试。
///
/// 不依赖 `python3` 这个裸名字——macOS 上它常常还是 3.9。
fn python_311() -> Option<String> {
    if let Some(explicit) = std::env::var("DUCK_PYTHON")
        .ok()
        .filter(|program| supports_tomllib(program))
    {
        return Some(explicit);
    }
    ["python3.13", "python3.12", "python3.11", "python3"]
        .into_iter()
        .find(|candidate| supports_tomllib(candidate))
        .map(str::to_string)
}

fn supports_tomllib(program: &str) -> bool {
    Command::new(program)
        .args(["-c", "import tomllib"])
        .status()
        .is_ok_and(|status| status.success())
}

/// 一个**进程内互不相同**的临时目录。
///
/// 光靠时间戳不够：macOS 上 `SystemTime` 只有微秒精度，而测试是并行跑的，同一微秒
/// 内起的两个测试会落到同一个目录，然后先结束的那个把它删掉，后一个就报「目录不存在」
/// ——一个只在 CI 上偶发的假失败。加一个进程内自增序号把它彻底堵死。
fn scratch_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SERIAL: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |delta| delta.as_nanos());
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("duck-plugin-contract-{tag}-{nanos}-{serial}"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// 跑一次真正的发布脚本（无镜像），返回它写出的目录。
fn publish(root: &Path, python: &str) -> PathBuf {
    publish_with(root, python, None)
}

/// 跑一次真正的发布脚本，可选 `--mirror-base`，返回它写出的目录。
fn publish_with(root: &Path, python: &str, mirror_base: Option<&str>) -> PathBuf {
    let out = scratch_dir("publish");
    let script = root.join("sbin").join("plugin_index.py");
    let mut command = Command::new(python);
    command
        .arg(&script)
        .arg("--plugins-dir")
        .arg(root.join("plugins"))
        .arg("--out-dir")
        .arg(&out)
        .arg("--base-url")
        .arg("https://example.invalid/plugins");
    if let Some(mirror) = mirror_base {
        command.arg("--mirror-base").arg(mirror);
    }
    let status = command
        .status()
        .expect("the publisher script must be runnable");
    assert!(status.success(), "the publisher script must succeed");
    out
}

#[test]
fn the_publisher_output_is_accepted_by_the_client() {
    let root = repo_root();
    if !root.join("sbin").join("plugin_index.py").is_file() {
        eprintln!("skipped: sbin/plugin_index.py is not present");
        return;
    }
    let Some(python) = python_311() else {
        eprintln!("skipped: no Python with tomllib (>= 3.11) on this machine");
        return;
    };

    let out = publish(&root, &python);
    let raw = std::fs::read(out.join("plugin-index.json")).expect("read the generated index");

    let index = parse_index(&raw).expect(
        "the publisher must emit an index the client accepts; \
         a field mismatch here would blank out every user's marketplace",
    );
    assert!(
        !index.plugins().is_empty(),
        "the repository ships at least one plugin"
    );

    // 逐条读得懂，并且每条的能力都完整摊开了。
    for entry in index.plugins() {
        assert!(!entry.name().is_empty(), "{} has no name", entry.id());
        assert!(
            entry.archive().url().starts_with("https://"),
            "{} points at a non-https archive",
            entry.id()
        );
        // 没给 `--mirror-base` 就没有备用地址。写端照写这个键（空表），
        // 线上格式因此只有一种形状。
        assert!(
            entry.archive().mirrors().is_empty(),
            "{} must have no mirrors without --mirror-base",
            entry.id()
        );
    }

    // 真的装一次：摘要、字节数、清单复核、能力上限四道关一起验。
    let entry = &index.plugins()[0];
    let file_name = entry
        .archive()
        .url()
        .rsplit('/')
        .next()
        .expect("the archive url ends with a file name");
    let archive = out.join("packages").join(file_name);
    assert!(archive.is_file(), "the archive must sit next to the index");

    let plugins_dir = scratch_dir("installed");
    let installed = install_from_archive(entry, &archive, &plugins_dir)
        .expect("the package the publisher produced must install");
    assert_eq!(installed.id(), entry.id());
    assert_eq!(installed.version(), entry.version());
    assert!(installed.dir().join("plugin.toml").is_file());

    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&plugins_dir);
}

/// 同一个插件目录打两遍必须字节一致：不然每次发布都会变成一次「假更新」。
///
/// 脚本内部已经打两遍比对了，这里从外面再验一次——外部证据比自证可信。
#[test]
fn publishing_twice_yields_byte_identical_archives() {
    let root = repo_root();
    if !root.join("sbin").join("plugin_index.py").is_file() {
        eprintln!("skipped: sbin/plugin_index.py is not present");
        return;
    }
    let Some(python) = python_311() else {
        eprintln!("skipped: no Python with tomllib (>= 3.11) on this machine");
        return;
    };

    let first = publish(&root, &python);
    let second = publish(&root, &python);

    let read_all = |dir: &Path| -> Vec<(String, Vec<u8>)> {
        let packages = dir.join("packages");
        let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&packages)
            .expect("read packages dir")
            .filter_map(Result::ok)
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    std::fs::read(entry.path()).expect("read package"),
                )
            })
            .collect();
        files.sort();
        files
    };

    let a = read_all(&first);
    let b = read_all(&second);
    assert!(!a.is_empty());
    assert_eq!(a, b, "the same plugin source must pack into the same bytes");

    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

/// 配了镜像时：**主地址是镜像、分支原址退成备用**，且顺序不能反。
///
/// 顺序反了不会报错，只会让国内用户绕开 CDN 去直连 `raw.githubusercontent.com`——
/// 慢到用户以为插件坏了，而日志里一切正常。所以这条必须从写端一直钉到读端。
///
/// 顺带验一下 `--mirror-base` 的尾斜杠会被脚本吃掉（故意传一个带尾斜杠的），
/// 否则会拼出 `.../duck//packages/...` 这种多一道斜杠的地址。
#[test]
fn a_configured_mirror_becomes_the_primary_and_the_base_url_the_backup() {
    let root = repo_root();
    if !root.join("sbin").join("plugin_index.py").is_file() {
        eprintln!("skipped: sbin/plugin_index.py is not present");
        return;
    }
    let Some(python) = python_311() else {
        eprintln!("skipped: no Python with tomllib (>= 3.11) on this machine");
        return;
    };

    let out = publish_with(&root, &python, Some("https://cdn.example.invalid/duck/"));
    let raw = std::fs::read(out.join("plugin-index.json")).expect("read the generated index");
    let index = parse_index(&raw).expect("the mirrored index must still parse");
    assert!(!index.plugins().is_empty());

    for entry in index.plugins() {
        let archive = entry.archive();
        assert!(
            archive
                .url()
                .starts_with("https://cdn.example.invalid/duck/packages/"),
            "{}: the mirror must be the primary url, got {:?}",
            entry.id(),
            archive.url()
        );
        assert_eq!(archive.mirrors().len(), 1, "{}", entry.id());
        assert!(
            archive.mirrors()[0].starts_with("https://example.invalid/plugins/packages/"),
            "{}: the branch url must survive as a mirror, got {:?}",
            entry.id(),
            archive.mirrors()[0]
        );

        let urls: Vec<&str> = archive.urls().collect();
        assert_eq!(urls.len(), 2, "{}", entry.id());
        assert_eq!(urls[0], archive.url(), "{}: primary first", entry.id());
        assert_eq!(
            urls[1],
            archive.mirrors()[0],
            "{}: then the backup",
            entry.id()
        );
    }

    let _ = std::fs::remove_dir_all(&out);
}
