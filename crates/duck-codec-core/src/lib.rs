//! 加解密内核：恒生 LCPT 算法 + 文件哈希 + 通用编解码。
//!
//! 三份来源，口径各不相同：
//! - [`hundsun_crypt`] / [`file_hash`] 是**逐字复刻件**，出处见各自模块头
//!   （`ifm_ai_tools/src-tauri/src/hundsun_crypt.rs` 与同目录 `lib.rs` 的
//!   `compute_file_hash`）；
//! - [`base64`] / [`url`] / [`hex`] / [`hash`] / [`aes_cbc`] / [`jwt`] /
//!   [`password`] 对应参考仓 `src/tools/CryptoBundle.vue` 那几个标签页，源仓是
//!   前端 WebCrypto / `btoa` 实现；本仓没有浏览器环境（`process` 轨），
//!   所以按**同一口径**用 Rust 重写了等价实现，算法与错误口径逐条对齐，
//!   差异写在各自模块头里。
//!
//! 为什么复刻件不参与 rustfmt：要与参考仓逐行 `diff`，重排只会让对比失效。
//! 因此 `mod` 声明上挂 `#[rustfmt::skip]`（crate 级 `#![rustfmt::skip]` 是
//! nightly 才有的 custom inner attribute，稳定版编译不过）。

pub mod aes_cbc;
pub mod base64;
pub mod file_hash;
pub mod hash;
pub mod hex;
pub mod jwt;
pub mod password;
pub mod url;

#[rustfmt::skip]
pub mod hundsun_crypt;
