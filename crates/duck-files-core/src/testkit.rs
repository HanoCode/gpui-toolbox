//! 测试用的临时目录与夹具。
//!
//! 只在 `cfg(test)` 下编译：内核对外不带任何「造文件」的入口（那属于插件层的事）。

use std::path::PathBuf;

/// 每次测试一个独立目录；同名目录先清空，避免上一次的残留影响断言。
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("duck-files-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// 写一个文本文件（父目录按需创建）。
pub fn write(root: &std::path::Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&path, text).expect("write file");
    path
}

/// 写一个二进制文件（docx / xlsx / pdf 的夹具都是「手工拼出来的字节」）。
pub fn write_bytes(root: &std::path::Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(&path, bytes).expect("write file");
    path
}

/// 造一个 ZIP（docx / pptx 的最小夹具：容器对了，正文 XML 由各个测试自己拼）。
pub fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    let file = std::fs::File::create(path).expect("create zip");
    let mut writer = zip::ZipWriter::new(file);
    // `Stored` 而不是 `Deflated`：测试夹具只有几十字节，压不压都一样，
    // 不压缩能顺手验证「读取不依赖解压后端」。
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        writer.start_file(*name, options).expect("start entry");
        writer.write_all(bytes).expect("write entry");
    }
    writer.finish().expect("finish zip");
}
