//! 字节单位换算常量：状态栏的容量显示与更新进度行共用同一套（二进制 1024 进制）。

/// 1 KB = 1024 B。
pub(crate) const KB: f64 = 1024.;
/// 1 MB = 1024 KB。
pub(crate) const MB: f64 = KB * 1024.;
/// 1 GB = 1024 MB。
pub(crate) const GB: f64 = MB * 1024.;
