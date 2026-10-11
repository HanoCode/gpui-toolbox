//! 重新读盘校验归档：manifest 承诺的字节数与 SHA-256。
//!
//! # 为什么不信下载时算的那个摘要
//!
//! 这不是「多一道保险」。要紧的是**将要被解包的那些字节**，也就是此刻磁盘上的字节，
//! 而不是刚才从 socket 上流过去的字节。两件事之间还隔着一次落盘、一次可能的替换，
//! 以及任何别的东西。顺带的好处是：用别的途径下到本地的文件，用同一个函数也能验。

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use crate::CHUNK;
use crate::error::UpdateError;
use crate::manifest::ManifestFile;
use crate::sha256::{Hasher, matches};

/// 按 manifest 的描述校验磁盘上的 `archive`。
///
/// 依次检查：文件大小是否等于 `size`；实际读到的字节数是否等于 `size`（短读=被截断）；
/// 摘要是否与 `sha256` 相同。
///
/// # Errors
/// 文件读不了 → [`UpdateError::Io`]；大小 / 摘要对不上 → [`UpdateError::SizeMismatch`]
/// 或 [`UpdateError::DigestMismatch`]。
pub fn verify(archive: &Path, expected: &ManifestFile) -> Result<(), UpdateError> {
    let file = File::open(archive)
        .map_err(|error| UpdateError::Io(format!("{}: {error}", archive.display())))?;

    // 先看元数据是个便宜的早退：大小都不对，没必要把整包读一遍。
    let on_disk = file
        .metadata()
        .map_err(|error| UpdateError::Io(format!("{}: {error}", archive.display())))?
        .len();
    if on_disk != expected.size() {
        return Err(UpdateError::SizeMismatch {
            expected: expected.size(),
            actual: on_disk,
        });
    }

    let mut reader = file;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut read_total: u64 = 0;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| UpdateError::Io(format!("{}: {error}", archive.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        read_total += read as u64;
    }

    // 元数据说是那么大、实际读到的却更少：文件在校验途中被截断了。
    if read_total != expected.size() {
        return Err(UpdateError::SizeMismatch {
            expected: expected.size(),
            actual: read_total,
        });
    }

    let actual = hasher.finalize_hex();
    if !matches(expected.sha256(), &actual) {
        return Err(UpdateError::DigestMismatch {
            expected: expected.sha256().to_owned(),
            actual,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify;
    use crate::error::UpdateError;
    use crate::manifest::Manifest;
    use crate::platform::PlatformKey;

    /// 造一份 manifest，把 `payload` 的真实摘要与长度填进去。
    fn manifest_for(payload: &[u8], digest: &str) -> Manifest {
        let json = format!(
            r#"{{
              "manifest_version": 1,
              "channel": "stable",
              "version": "9.9.9",
              "notes": "n",
              "published_at": "2026-10-10T00:00:00Z",
              "files": {{
                "macos-arm64": {{
                  "url": "https://example.invalid/x.tar.gz",
                  "sha256": "{digest}",
                  "size": {},
                  "signature": null
                }}
              }}
            }}"#,
            payload.len()
        );
        crate::manifest::parse(json.as_bytes()).expect("valid manifest")
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-verify-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        dir
    }

    fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("writes");
        path
    }

    fn digest_of(payload: &[u8]) -> String {
        let mut hasher = crate::sha256::Hasher::new();
        hasher.update(payload);
        hasher.finalize_hex()
    }

    #[test]
    fn a_file_matching_the_manifest_passes() {
        let payload = b"the real archive bytes";
        let manifest = manifest_for(payload, &digest_of(payload));
        let dir = scratch("ok");
        let path = write(&dir, "ok.tar.gz", payload);

        let file = manifest.file_for(PlatformKey::MacosArm64).expect("present");
        assert_eq!(verify(&path, file), Ok(()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内容换了、长度没变：只有摘要在守这一关。
    #[test]
    fn content_that_does_not_match_its_digest_is_refused() {
        let payload = b"the real archive bytes";
        let manifest = manifest_for(payload, &digest_of(payload));
        let dir = scratch("digest");
        let tampered = b"the Fake archive bytes";
        assert_eq!(payload.len(), tampered.len(), "same length on purpose");
        let path = write(&dir, "tampered.tar.gz", tampered);

        let file = manifest.file_for(PlatformKey::MacosArm64).expect("present");
        assert!(matches!(
            verify(&path, file),
            Err(UpdateError::DigestMismatch { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_is_refused_without_hashing_it() {
        let payload = b"the real archive bytes";
        let manifest = manifest_for(payload, &digest_of(payload));
        let dir = scratch("short");
        let path = write(&dir, "short.tar.gz", &payload[..5]);

        let file = manifest.file_for(PlatformKey::MacosArm64).expect("present");
        assert!(matches!(
            verify(&path, file),
            Err(UpdateError::SizeMismatch { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_a_mismatch() {
        let payload = b"x";
        let manifest = manifest_for(payload, &digest_of(payload));
        let dir = scratch("missing");

        let file = manifest.file_for(PlatformKey::MacosArm64).expect("present");
        assert!(matches!(
            verify(&dir.join("nothing-here.tar.gz"), file),
            Err(UpdateError::Io(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
