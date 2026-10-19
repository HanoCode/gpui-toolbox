//! 测试用的小工具：造最小 `.class` 与真 ZIP。
//!
//! 只在 `cfg(test)` 下编译。class / jar 两个比对族的测试都要「一个能过
//! `parse_class_file` 的最小 class」，各写一份迟早漂移，所以放这里共用。

/// 一个能过 `parse_class_file` 的最小 `.class`。
///
/// constant pool 里只放一条 Utf8（内容就是版本号），没有方法、没有属性。
/// `VersionExtractor` 会走最后一条路（`scan_constant_pool`）；真实 `.class` 里版本号
/// 藏在 `getVersion()` 的返回常量里，抽出来的东西是同一个。
pub fn fake_class(version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&52u16.to_be_bytes()); // major（Java 8）
    out.extend_from_slice(&2u16.to_be_bytes()); // constant_pool_count：1 个有效项
    out.push(1); // CONSTANT_Utf8
    out.extend_from_slice(&(version.len() as u16).to_be_bytes());
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags
    out.extend_from_slice(&0u16.to_be_bytes()); // this_class
    out.extend_from_slice(&0u16.to_be_bytes()); // super_class
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    out.extend_from_slice(&0u16.to_be_bytes()); // fields
    out.extend_from_slice(&0u16.to_be_bytes()); // methods
    out.extend_from_slice(&0u16.to_be_bytes()); // attributes
    out
}

/// 写一个真 ZIP（不压缩，够 `ZipArchive` 读）。父目录自动建。
pub fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    use std::io::Write as _;

    std::fs::create_dir_all(path.parent().expect("zip path has parent")).expect("create dirs");
    let file = std::fs::File::create(path).expect("create zip");
    let mut writer = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        writer.start_file(*name, options).expect("start zip entry");
        writer.write_all(bytes).expect("write zip entry");
    }
    writer.finish().expect("finish zip");
}
