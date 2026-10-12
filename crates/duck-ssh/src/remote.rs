//! 远端命令执行与文件操作。
//!
//! # 一次往返 = 一次 `ssh`
//!
//! 每个操作都起一个 `ssh` 进程（靠 `ControlMaster` 复用底层连接，握手不重来）。
//! 这比「常驻一个 SSH 会话 + 自己维护 channel 多路复用」朴素，但换来两个确定性：
//!
//! 1. **不会挂**：进程结束状态就是操作结果，没有「连接还活着但 channel 已死」的中间态；
//! 2. **可解释**：失败时拿到的是 ssh 自己的 stderr（`Permission denied`、
//!    `Host key verification failed`），直接给用户看就是准确的，不用我们翻译。
//!
//! # 远端脚本一律走 `sh -c`
//!
//! 远端用户的登录 shell 可能是 zsh / fish，它们的 glob 与 `read` 行为各不相同 ——
//! 让脚本在**显式指定的 `sh`** 里跑，行为才是可预期的（`*` 不匹配时保持字面量，
//! 而不是像 zsh 那样直接报错退出）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::target::{Target, askpass_script};
use crate::{SshError, which};

/// 单次命令输出的上限：远端命令打出一部电影也不该把宿主内存吃光。
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// 一次远端命令的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// 退出码；`None` 表示被信号终止。
    pub status: Option<i32>,
    /// 标准输出。
    pub stdout: String,
    /// 标准错误（只取首行）。
    pub stderr: String,
    /// `stdout` 是否因为超过上限被截断。
    ///
    /// 截断**不往内容里塞标记**：此前是把「…（输出已截断）」拼进 stdout，
    /// 于是列举目录时那行会被当成一个真文件名解析出来。现在只置这个标志，
    /// 由调用方决定怎么说（见 [`RemoteListing::truncated`]）。
    pub truncated: bool,
}

impl Output {
    /// 是否正常退出。
    #[must_use]
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// 远端目录里的一个条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// 文件名。
    pub name: String,
    /// 是否目录（决定双击行为与图标）。
    pub dir: bool,
    /// 字节数；非普通文件（目录、设备、FIFO）是 `None`。
    pub size: Option<u64>,
    /// 修改时间（`MM-DD HH:MM`）；远端拿不到时为 `None`。
    pub modified: Option<String>,
}

/// 跑一次远端命令。
///
/// `socket_dir` 给出后启用连接复用（与终端标签共用同一条物理连接）。
pub fn exec(target: &Target, socket_dir: Option<&Path>, command: &str) -> Result<Output, SshError> {
    target
        .validate()
        .map_err(|reason| SshError::InvalidTarget { reason })?;
    let program = which("ssh").ok_or(SshError::NotFound { program: "ssh" })?;

    let mut argv = target.base_argv(socket_dir);
    // 没带口令时必须 `BatchMode`：否则 ssh 会在一个没有终端的子进程里
    // 等密码提示符的输入，一等等到超时，界面表现为「卡住」。
    // 带了口令则交给 askpass 助手（它需要非 BatchMode 才被调用）。
    if target.password.is_none() {
        argv.push("-o".to_owned());
        argv.push("BatchMode=yes".to_owned());
    }
    argv.push(target.destination());
    argv.push(command.to_owned());

    let helper = askpass_helper(target, socket_dir);
    let output = Command::new(program)
        .args(&argv)
        .envs(target.env(helper.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| SshError::Spawn {
            message: error.to_string(),
        })?;

    let (stdout, truncated) = truncate(String::from_utf8_lossy(&output.stdout).into_owned());
    let result = Output {
        status: output.status.code(),
        stdout,
        stderr: first_line(String::from_utf8_lossy(&output.stderr).trim()),
        truncated,
    };
    if result.success() {
        Ok(result)
    } else {
        Err(SshError::Failed {
            status: result.status,
            stderr: result.stderr,
        })
    }
}

/// 用给定目标探一次活，成功时返回远端的身份说明（`uname -srm` 的结果）。
///
/// 与终端标签那条路的区别是**不开 PTY**：没有终端，ssh 就不会停下来等口令提示，
/// 失败时拿到的第一行 stderr 就是准确原因（`Permission denied (publickey).`、
/// `Connection refused`、`Host key verification failed`），
/// 界面把它原样显示出来，比任何「连接失败」的泛化文案都有用。
///
/// 口令认证走 askpass 助手（与终端、SFTP 同一套 `Target::env`），
/// 所以「测试连接」测的就是真正连接时会走的那条认证路径。
pub fn test_connection(target: &Target, socket_dir: Option<&Path>) -> Result<String, SshError> {
    // `2>/dev/null || true`：`uname` 在极少数裁剪过的系统上不存在，
    // 但「跑通了一条命令」本身已经证明连接成立，不该因此报失败。
    let output = exec(target, socket_dir, "uname -srm 2>/dev/null || true")?;
    let banner = output.stdout.trim();
    Ok(if banner.is_empty() {
        "ok".to_owned()
    } else {
        banner.to_owned()
    })
}

/// 一次远端目录列举的结果。
///
/// 把「条目」与「是否被截断」一起交出来，而不是把截断标记拼进内容里 ——
/// 界面才能看着这个标志说一句「列表已截断」，而不是让用户以为目录就这么多东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteListing {
    /// 目录条目（已排序前的原始顺序）。
    pub entries: Vec<RemoteEntry>,
    /// 远端的输出超过上限，条目**不完整**。
    pub truncated: bool,
}

/// 列举远端目录。
///
/// 用远端 `sh` 跑一段循环，输出 `类型\t大小\t时间\t名字` 的制表符分隔记录。
/// **不用 `ls -l`**：它的时间格式随 `LC_*` 与平台（BSD / GNU）变，解析出来的是
/// 一堆「看起来对、偶尔错一行」的数据；这里的格式由我们自己定死。
pub fn list_dir(
    target: &Target,
    socket_dir: Option<&Path>,
    path: &str,
) -> Result<RemoteListing, SshError> {
    // 纯字面量：脚本里没有要插的变量，用 `format!` 只会逼着把 `{` `}` 写成 `{{` `}}`。
    let script = r#"cd -- "$1" 2>/dev/null || { printf '__DUCK_NO_DIR__\n'; exit 1; }
for n in * .[!.]*; do
  [ -e "$n" ] || [ -L "$n" ] || continue
  if [ -d "$n" ]; then t=d; s=-
  elif [ -f "$n" ]; then t=f; s=$(wc -c < "$n" 2>/dev/null | tr -d ' \t')
  else t=o; s=-
  fi
  m=$(date -r "$n" +%Y-%m-%dT%H:%M 2>/dev/null)
  printf '%s\t%s\t%s\t%s\n' "$t" "$s" "$m" "$n"
done"#;
    let command = format!("sh -c {} duck {}", shell_quote(script), shell_quote(path));
    let output = exec(target, socket_dir, &command)?;
    Ok(RemoteListing {
        entries: parse_listing(&output.stdout),
        truncated: output.truncated,
    })
}

/// 解析 [`list_dir`] 的脚本输出。
fn parse_listing(stdout: &str) -> Vec<RemoteEntry> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let kind = parts.next()?;
            let size = parts.next()?;
            let modified = parts.next()?;
            let name = parts.next()?;
            if name.is_empty() {
                return None;
            }
            Some(RemoteEntry {
                name: name.to_owned(),
                dir: kind == "d",
                size: size.parse::<u64>().ok(),
                modified: format_stamp(modified),
            })
        })
        .collect()
}

/// `YYYY-MM-DDTHH:MM` → `MM-DD HH:MM`；拿不到时间就是 `None`。
fn format_stamp(stamp: &str) -> Option<String> {
    let (date, time) = stamp.split_once('T')?;
    let mut date_parts = date.split('-');
    let _year = date_parts.next()?;
    let month = date_parts.next()?;
    let day = date_parts.next()?;
    Some(format!("{month}-{day} {time}"))
}

/// 上传：本地 → 远端（`scp -r`，目录也能传）。
pub fn upload(
    target: &Target,
    socket_dir: Option<&Path>,
    local: &Path,
    remote_dir: &str,
) -> Result<(), SshError> {
    if !local.exists() {
        return Err(SshError::LocalMissing {
            path: local.display().to_string(),
        });
    }
    let destination = format!("{}:{remote_dir}", target.destination());
    transfer(target, socket_dir, &local.to_string_lossy(), &destination)
}

/// 下载：远端 → 本地。
pub fn download(
    target: &Target,
    socket_dir: Option<&Path>,
    remote_path: &str,
    local_dir: &Path,
) -> Result<(), SshError> {
    // 远端路径交给远端的 shell 解析，所以按 POSIX 规则转义：
    // 路径里有空格（`my report.pdf`）时不转义会被拆成两个参数。
    let source = format!("{}:{}", target.destination(), shell_quote(remote_path));
    transfer(target, socket_dir, &source, &local_dir.to_string_lossy())
}

/// `scp` 的公共入口。
///
/// 两个位置参数都是**字符串**而不是 `Path`：其中一个是 `用户@主机:路径` 形式，
/// 它不是本地路径，用 `Path` 承载会在 Windows 上被反斜杠规范化改写掉。
fn transfer(
    target: &Target,
    socket_dir: Option<&Path>,
    source: &str,
    destination: &str,
) -> Result<(), SshError> {
    target
        .validate()
        .map_err(|reason| SshError::InvalidTarget { reason })?;
    let program = which("scp").ok_or(SshError::NotFound { program: "scp" })?;

    let mut argv = target.scp_base_argv(socket_dir);
    // `-r` 无条件加：目录传输需要它，单文件传时它无副作用。
    // `-p` 保留远端文件的 mtime / 权限位（否则下载回来的文件时间是「刚才」）。
    argv.push("-r".to_owned());
    argv.push("-p".to_owned());
    argv.push(source.to_owned());
    argv.push(destination.to_owned());

    let helper = askpass_helper(target, socket_dir);
    let output = Command::new(program)
        .args(&argv)
        .envs(target.env(helper.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| SshError::Transfer {
            message: error.to_string(),
        })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(SshError::Transfer {
            message: first_line(String::from_utf8_lossy(&output.stderr).trim()),
        })
    }
}

/// 新建远端目录。
pub fn mkdir(target: &Target, socket_dir: Option<&Path>, path: &str) -> Result<(), SshError> {
    exec(
        target,
        socket_dir,
        &format!("mkdir -- {}", shell_quote(path)),
    )?;
    Ok(())
}

/// 删除远端文件或空目录（**不加 `-r`**：递归删除要显式确认，见界面层的
/// `require_confirm_writes`）。
pub fn remove(target: &Target, socket_dir: Option<&Path>, path: &str) -> Result<(), SshError> {
    exec(target, socket_dir, &format!("rm -- {}", shell_quote(path)))?;
    Ok(())
}

/// 远端重命名 / 移动。
pub fn rename(
    target: &Target,
    socket_dir: Option<&Path>,
    from: &str,
    to: &str,
) -> Result<(), SshError> {
    exec(
        target,
        socket_dir,
        &format!("mv -- {} {}", shell_quote(from), shell_quote(to)),
    )?;
    Ok(())
}

/// askpass 助手路径：只有**确实有口令**时才落脚本。
///
/// 写不出脚本不是致命错误（磁盘满、目录权限不对）：这时返回 `None`，
/// ssh 会退化成自己的交互提示 —— 在终端标签里用户仍能手动输入口令。
/// 这里刻意**不引日志库**（本 crate 保持零依赖），失败是静默的降级而非故障。
fn askpass_helper(target: &Target, dir: Option<&Path>) -> Option<PathBuf> {
    target.password.as_ref()?;
    let dir = dir?;
    askpass_script(dir).ok()
}

/// 用单引号把字符串包成 shell 字面量。
///
/// 单引号内除了 `'` 本身一切都是字面量，所以只需把 `'` 替换成 `'\''`。
/// 这条规则同时用于本地 argv 与远端命令 —— 两边的解析器都是 POSIX shell。
#[must_use]
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 截断超长输出（保头不留尾：开头的报错信息比结尾的滚动日志有用）。
///
/// 切点**回退到最后一个完整行**：`list_dir` 的输出是逐行解析的，
/// 切在行中间会留下半行，那半行会被当成一个真条目解析出来。
/// 返回值第二项表示「确实截断过」。
fn truncate(text: String) -> (String, bool) {
    if text.len() <= MAX_OUTPUT {
        return (text, false);
    }
    let mut cut = MAX_OUTPUT;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // 回退到最近的换行之后；一行就超过上限时（没有可回退的换行）保留按字符切的点。
    let end = text[..cut].rfind('\n').map_or(cut, |index| index + 1);
    (text[..end].to_owned(), true)
}

/// stderr 只取首行：ssh 的多行报错里，第一行是根因，后面多是上下文。
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_parses_tab_separated_records() {
        let raw = "d\t-\t2026-10-11T22:41\treleases\nf\t923\t2026-09-30T10:15\tnginx.conf\n";
        let entries = parse_listing(raw);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].dir);
        assert_eq!(entries[0].name, "releases");
        assert_eq!(entries[0].modified.as_deref(), Some("10-11 22:41"));
        assert_eq!(entries[1].size, Some(923));
        assert!(!entries[1].dir);
    }

    #[test]
    fn listing_keeps_names_with_spaces_and_tabs_are_bounded() {
        // 名字里有空格必须完整保留（splitn(4) 的意义就在这里）。
        let entries = parse_listing("f\t10\t2026-01-02T03:04\tmy report final.pdf\n");
        assert_eq!(entries[0].name, "my report final.pdf");
    }

    #[test]
    fn listing_skips_malformed_lines() {
        // 远端 sh 版本不同可能吐出意外行：跳过它，别让整次列举失败。
        let entries = parse_listing("garbage\nf\t1\t2026-01-02T03:04\tok.txt\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ok.txt");
    }

    #[test]
    fn missing_timestamp_is_none_not_a_fake_date() {
        assert_eq!(format_stamp(""), None);
        let entries = parse_listing("f\t1\t\tplain.txt\n");
        assert_eq!(entries[0].modified, None);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn missing_local_source_fails_before_spawning_ssh() {
        // 本地源不存在时应该**直接报本地路径问题**，而不是起一个 ssh 再失败：
        // 后者会把「源文件没了」误报成「远端拒绝」。
        let target = Target {
            host: "h".to_owned(),
            port: 22,
            user: "u".to_owned(),
            identity: None,
            password: None,
            options: Vec::new(),
        };
        let error = upload(&target, None, Path::new("/definitely/not/here"), "/tmp")
            .expect_err("本地源不存在必须失败");
        assert!(matches!(error, SshError::LocalMissing { .. }));
    }

    #[test]
    fn invalid_target_fails_before_spawning() {
        let target = Target {
            host: String::new(),
            port: 22,
            user: "u".to_owned(),
            identity: None,
            password: None,
            options: Vec::new(),
        };
        // 参数不合法要报成 `InvalidTarget`，而不是笼统的 `Spawn`：
        // 界面据此给出的下一步完全不同（改表单 vs 查权限）。
        assert!(matches!(
            exec(&target, None, "true").expect_err("空主机必须失败"),
            SshError::InvalidTarget { .. }
        ));
    }

    #[test]
    fn truncation_keeps_whole_lines_and_reports_itself() {
        // 造一段超过上限的列举输出：每行都是合法记录。
        let line = "f\t12\t2026-01-02T03:04\treport.pdf\n";
        let raw = line.repeat(MAX_OUTPUT / line.len() + 8);
        let (cut, truncated) = truncate(raw);
        assert!(truncated, "超过上限必须如实置位");
        assert!(cut.len() <= MAX_OUTPUT);
        assert!(cut.ends_with('\n'), "切点要落在整行边界上");

        // 关键回归：旧的实现把「…（输出已截断）」拼进内容里，
        // 那行会被当成一个真文件名解析出来。现在必须一条垃圾条目都没有。
        let entries = parse_listing(&cut);
        assert!(!entries.is_empty());
        assert!(
            entries.iter().all(|entry| entry.name == "report.pdf"),
            "截断不能污染文件名"
        );
        assert_eq!(
            entries.len(),
            cut.matches('\n').count(),
            "每行都应解析成一条"
        );
    }

    #[test]
    fn short_output_is_not_touched() {
        let (kept, truncated) = truncate("f\t1\t\tok.txt\n".to_owned());
        assert_eq!(kept, "f\t1\t\tok.txt\n");
        assert!(!truncated);
    }
}
