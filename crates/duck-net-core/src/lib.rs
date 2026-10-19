//! 网络与联调族内核（`duck.net.*`，计划 §6.7）。
//!
//! 四个工具、一份内核：
//!
//! | 工具 | 模块 | 参考仓来源 |
//! | --- | --- | --- |
//! | HTTP 请求 | [`http`] | `HttpRequest.vue` 的前端算法（源仓无后端：请求由前端 `fetch` 发） |
//! | 报文调试 | [`codec`] + [`tcp`] | `net/codec.rs`（240 行，逐字）+ `TcpRequest.vue` / `XmlSend.vue` / `FixSend.vue` 的通信骨架 |
//! | S3 / FTP 连通检测 | [`conn`] | `net/conn_check.rs`（374 行） |
//! | Hosts 管理 | [`hosts`] | `src/utils/hosts.ts`（154 行，逐字）+ `hosts_manager.rs` 的两条口径 |
//!
//! 纪律与前几族一致：**零 gpui、零 tauri、全同步**（`ureq` 是阻塞客户端，
//! 插件进程本来就是「跑一次就退」，不需要 tokio）。错误走 [`ErrorKind`] +
//! 细节，由插件层翻译。
//!
//! ## 四处对参考仓的主动偏离
//!
//! 1. **没有长连接**：源页面为连续调同一台机器保留 `connect_tcp` / `disconnect_tcp`。
//!    插件进程一次一退，长连接存不住 —— 每次运行都是「连 → 发 → 收 → 断」。
//! 2. **SFTP 走系统 `ssh`**（[`duck_ssh`]），不引 `ssh2`：OpenSSH 的 stderr 就是
//!    最准确的诊断，自己实现协议栈只会把它翻译成更差的句子。
//! 3. **hosts 不写系统文件**：设计稿的硬约束（写 `/etc/hosts` 要 sudo，插件没有
//!    提权通道）。只生成合并结果 + 用户自己执行的命令。详见 [`hosts`] 模块头。
//! 4. **内核不出中文**：校验问题改为枚举、合并抬头与应用注释下沉到插件译文表。

pub mod codec;
pub mod conn;
pub mod error;
pub mod hosts;
pub mod http;
pub mod s3;
pub mod tcp;

pub use codec::{
    Codec, CodecConfig, DecodeIssue, Decoded, DelimiterCodec, FixedLengthCodec, LengthFieldCodec,
    RawCodec, create,
};
pub use conn::{
    CheckOutcome, FtpTarget, S3Target, SftpTarget, Step, StepId, StepStatus, check_ftp, check_s3,
    check_sftp,
};
pub use error::{ErrorKind, NetError, NetResult, require_file};
pub use hosts::{
    ApplyCommand, ApplyScript, HostsDiff, HostsEntry, HostsIssue, HostsParseResult, HostsProfile,
    HostsStats, IssueKind, IssueLevel, MAX_HOSTS_BYTES, MergedSection, ProfileMeta, ReplaceOutcome,
    SearchHit, build_apply_script, diff_contents, find_profile, is_valid_ip, merge_sections,
    parse_hosts, parse_profile_meta, replace_in_profiles, search_profiles, validate_hosts_content,
};
pub use http::{
    Auth, BodyKind, Cookie, CurlRequest, DEFAULT_REDIRECTS, DEFAULT_TIMEOUT_MS, HttpRequest,
    HttpResponse, MAX_BODY, Method, Multipart, append_query, curl_command, encode_component,
    encode_multipart, encode_urlencoded, format_size, guess_content_type, parse_curl,
    parse_headers, parse_pairs, parse_set_cookies, pretty_body, redact_headers, resolve_vars, send,
    validate_url,
};
pub use tcp::{Charset, Payload, TcpOutcome, TcpRequest, hex_dump, parse_hex};
