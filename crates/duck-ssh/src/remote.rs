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
//!
//! # 传输为什么又有一个 `*_spawn` 版本
//!
//! [`upload`] / [`download`] 是「起进程 → 等它结束」的一步式接口，进程一旦挂住
//!（网络断了但 TCP 还没超时、远端不响应）调用线程只能陪葬，界面上就是「取消不了」。
//! [`upload_spawn`] / [`download_spawn`] 把「起进程」与「等结果」拆开，交出一个
//! [`TransferHandle`]：调用方可以先 `kill` 再 `wait`。取消因此是真的终止 `scp` 进程。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(test)]
use crate::target::AuthMethod;
use crate::target::{Target, askpass_script};
use crate::{SshError, which};

/// 单次命令输出的上限：远端命令打出一部电影也不该把宿主内存吃光。
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// 一次远端命令的**总时限**。
///
/// 为什么要有它：`ConnectTimeout`（在 `Target::common_argv` 里）只覆盖「建立连接」，
/// 管不到两种情况 —— ① 复用一条**已经挂死但没断**的 master（`ssh` 会排在它的队列里，
/// 一直等），② 远端命令自己不再输出也不再退出。这两种在界面上都表现为「永久转圈」，
/// 用户唯一能做的就是杀掉整个应用。
///
/// 30 秒：列举一个上万文件的目录、`du` 一个小树都在这个量级以内；
/// 再长就说明连接确实出问题了，早点报错比让他继续等有用。
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// 等待传输进程退出的轮询间隔（waitpid 式轮询，见 [`TransferHandle::wait`]）。
///
/// 50ms 是「取消手感」与「空转开销」的折中：取消后最坏 50ms 内界面就能落定，
/// 而一次挂起的长传输每秒只多轮询 20 次。
const TRANSFER_POLL: Duration = Duration::from_millis(50);

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
    let mut command = Command::new(program);
    command
        .args(&argv)
        .envs(target.env(helper.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, stdout, stderr) = run_with_timeout(command, EXEC_TIMEOUT)?;

    let (stdout, truncated) = truncate(String::from_utf8_lossy(&stdout).into_owned());
    let result = Output {
        status: status.code(),
        stdout,
        stderr: first_line(String::from_utf8_lossy(&stderr).trim()),
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

/// 跑一个子进程并读到 EOF，**超过 `limit` 就杀掉**。
///
/// 与 `Command::output()` 的差别只有两点，但两点都关键：
/// ① 有截止时间（见 [`EXEC_TIMEOUT`] 的注释：这是「永久转圈」的唯一出口）；
/// ② 进程在 `kill` 之后**仍会被 `wait` 回收**，不留僵尸。
///
/// 输出用两条独立线程读：`ssh` 的 stderr 与 stdout 都可能超过管道缓冲（64 KiB），
/// 边等边读才不会互相堵死 —— 单线程「先读 stdout 再读 stderr」在输出大的目录上
/// 会死锁，这个坑在 `Command::output()` 里由标准库替我们避过了，自己实现就得自己避。
fn run_with_timeout(
    mut command: Command,
    limit: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), SshError> {
    let mut child = command.spawn().map_err(|error| SshError::Spawn {
        message: error.to_string(),
    })?;
    let stdout = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });

    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                // 查不到状态的进程按「孤儿」处理：杀掉，别让它继续挂在后台。
                let _ = child.kill();
                let _ = child.wait();
                return Err(SshError::Spawn {
                    message: error.to_string(),
                });
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // 读线程会因为管道 EOF 自行结束；这里不等它们 —— 已经被 kill 的进程
            // 不会再有输出，join 只是徒增等待。
            return Err(SshError::TimedOut {
                secs: limit.as_secs(),
            });
        }
        thread::sleep(TRANSFER_POLL);
    };

    let collect = |handle: Option<thread::JoinHandle<Vec<u8>>>| {
        handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    Ok((status, collect(stdout), collect(stderr)))
}

/// 关掉这台主机当前的连接复用 master（`ssh -O exit`）。
///
/// # 为什么需要显式关
///
/// `ControlPath` 是按**连接参数**哈希出来的（见 [`control_path`]），参数变了就会
/// 落到另一个 socket 上 —— 新连接自然是新的。但真正会咬人的是**反过来**：
/// 参数只改了一点点（比如只改转发），哈希变了、新 socket 也建了，**旧 master 却还在**
/// 按原来的 `ControlPersist=300` 活 5 分钟，占着本地转发端口、也让用户的
/// `ps` 里多一条看不懂的 `ssh -M`。同理，删除档案之后没有任何人负责收尾。
///
/// 所以：改档案、删档案时各调一次。**best-effort**：失败不算错误（socket 早就没了
/// 是最常见的情形，`ssh -O exit` 会返回非零），这里只回报「有没有真的关掉」，
/// 供调用方决定是否记日志 —— 绝不因为清理失败而挡住用户的操作。
pub fn close_master(target: &Target, socket_dir: &Path) -> bool {
    let Some(program) = which("ssh") else {
        return false;
    };
    let socket = crate::target::control_path(socket_dir, target);
    // socket 不在就没什么可关的：直接返回，省一次进程启动。
    if !socket.exists() {
        return false;
    }
    // 值要带引号，与 `common_argv` 同一个原因：`-o` 的值会被再拆一次词。
    let argv = [
        "-O".to_owned(),
        "exit".to_owned(),
        "-p".to_owned(),
        target.port.to_string(),
        "-o".to_owned(),
        format!("ControlPath=\"{}\"", socket.to_string_lossy()),
        target.destination(),
    ];
    let mut command = Command::new(program);
    command
        .args(argv)
        .envs(target.env(askpass_helper(target, Some(socket_dir)).as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // 3 秒：`-O exit` 只是给本地 master 进程发一个控制消息，不该等。
    run_with_timeout(command, Duration::from_secs(3)).is_ok()
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
    /// 远端 `cd` 之后 `pwd -P` 报出的**绝对**目录。
    ///
    /// 界面拿它替换掉自己那份「用户级」路径（往往是 `~`），于是 `~` 只存在于
    /// **第一次列举**这一个瞬间：之后的面包屑、`join`、`mkdir`、`rename`、`scp`
    /// 用的都是绝对路径。反过来（让每个远端操作各自去展开波浪号）是不成立的：
    /// 路径由 [`shell_quote`] 单引号包裹，单引号内不展开 `~`，漏掉任何一处
    /// 就是一条「远端命令失败」。远端没报出来时是 `None`（条目照常可用）。
    pub cwd: Option<String>,
}

/// 远端脚本里「把用户给的路径变成可 `cd` 的路径」的前导。
///
/// **为什么必须自己做**：路径是用 [`shell_quote`] 单引号包起来交给远端的，
/// 而单引号内不展开 `~` —— 直接 `cd -- "$1"` 会去找一个**名为 `~` 的目录**，
/// 必然失败，界面上的表现就是「远端命令失败（退出码 1）。」（无 stderr 可看）。
/// 远端家目录只有远端知道（`root` 与 `deploy` 不是同一个），所以只能让它展开：
/// `$HOME` 由远端 `sh` 提供，脚本不猜任何绝对路径。
const RESOLVE_DIR: &str = r#"d=$1
case $d in
  '~') d=$HOME ;;
  '~/'*) d=$HOME/${d#'~/'} ;;
esac
"#;

/// `cd` 失败的哨兵。走 **stderr**：stdout 是列举数据通道，不混控制信息。
///
/// 不看它的代价是**失去诊断**（见 [`cd_error`]）：用户只会拿到一个裸的退出码。
const NO_DIR_SENTINEL: &str = "__DUCK_NO_DIR__";

/// 列举输出第一行的前缀，其后是远端 `pwd -P` 的结果（制表符分隔）。
const CWD_HEADER: &str = "__DUCK_CWD__";

/// 拼出 [`list_dir`] 要跑的那条远端命令行。
///
/// 单独成函数是为了**能被测试真的跑一遍**（用本机 `sh`）：波浪号展开这类行为
/// 在本机没有远端主机时最难验证，而它恰好是整条链路上最容易写错的一环。
fn list_command(path: &str) -> String {
    let script = format!(
        r#"{resolve}cd -- "$d" 2>/dev/null || {{ printf '%s\n' '{sentinel}' >&2; exit 1; }}
printf '{header}\t%s\n' "$(pwd -P)"
for n in * .[!.]*; do
  [ -e "$n" ] || [ -L "$n" ] || continue
  if [ -d "$n" ]; then t=d; s=-
  elif [ -f "$n" ]; then t=f; s=$(wc -c < "$n" 2>/dev/null | tr -d ' \t')
  else t=o; s=-
  fi
  m=$(date -r "$n" +%Y-%m-%dT%H:%M 2>/dev/null)
  printf '%s\t%s\t%s\t%s\n' "$t" "$s" "$m" "$n"
done"#,
        resolve = RESOLVE_DIR,
        sentinel = NO_DIR_SENTINEL,
        header = CWD_HEADER,
    );
    format!("sh -c {} duck {}", shell_quote(&script), shell_quote(path))
}

/// 列举远端目录。
///
/// 用远端 `sh` 跑一段循环，输出头部 `__DUCK_CWD__\t绝对目录` + 每行
/// `类型\t大小\t时间\t名字` 的制表符分隔记录。
/// **不用 `ls -l`**：它的时间格式随 `LC_*` 与平台（BSD / GNU）变，解析出来的是
/// 一堆「看起来对、偶尔错一行」的数据；这里的格式由我们自己定死。
pub fn list_dir(
    target: &Target,
    socket_dir: Option<&Path>,
    path: &str,
) -> Result<RemoteListing, SshError> {
    let command = list_command(path);
    let output = exec(target, socket_dir, &command).map_err(|error| cd_error(error, path))?;
    let (cwd, body) = take_cwd(&output.stdout);
    Ok(RemoteListing {
        entries: parse_listing(body),
        truncated: output.truncated,
        cwd,
    })
}

/// 摘出列举输出第一行的绝对目录，返回 `(目录, 余下的条目正文)`。
///
/// 没有头时**不退化成失败**：条目照常解析，只是界面拿不到绝对路径
///（远端脚本与我们不同版本、或第一行被别的东西占了），`~` 会留在面板上。
fn take_cwd(stdout: &str) -> (Option<String>, &str) {
    let mut lines = stdout.splitn(2, '\n');
    let head = lines.next().unwrap_or_default();
    let Some(rest) = head.strip_prefix(CWD_HEADER) else {
        return (None, stdout);
    };
    let cwd = rest.strip_prefix('\t').unwrap_or(rest).trim();
    let body = lines.next().unwrap_or_default();
    let cwd = (!cwd.is_empty()).then(|| cwd.to_owned());
    (cwd, body)
}

/// 把「远端 `cd` 失败」翻译成能**指认路径**的错误。
///
/// 远端脚本把哨兵写进 stderr；不看它的话，用户看到的是「远端命令失败（退出码 1）。」
/// —— 既不说是哪个目录，也说不出为什么，而这一步的失败只有两类原因
///（目录不存在 / 没有进入权限），两类都指向同一个下一步：换一个目录。
///
/// 只对**带哨兵**的失败改口：别的非零退出（连接被拒、权限被拒）原样透传，
/// 不能把所有失败都说成「目录不存在」。
fn cd_error(error: SshError, path: &str) -> SshError {
    if let SshError::Failed { stderr, .. } = &error {
        if stderr.contains(NO_DIR_SENTINEL) {
            return SshError::NoSuchDir {
                path: path.to_owned(),
            };
        }
    }
    error
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
    ensure_local_source(local)?;
    transfer(
        target,
        socket_dir,
        &upload_source(local),
        &upload_destination(target, remote_dir),
    )?
    .wait()
}

/// 上传的**可中断**版本：起 `scp` 就返回句柄，成败由 [`TransferHandle::wait`] 给出。
pub fn upload_spawn(
    target: &Target,
    socket_dir: Option<&Path>,
    local: &Path,
    remote_dir: &str,
) -> Result<TransferHandle, SshError> {
    ensure_local_source(local)?;
    transfer_spawn(
        target,
        socket_dir,
        &upload_source(local),
        &upload_destination(target, remote_dir),
    )
}

/// 上传的前置检查：本地源必须存在（一步式与可中断两条路共用）。
///
/// 本地源不存在时应该**直接报本地路径问题**，而不是起一个 `scp` 再失败：
/// 后者会把「源文件没了」误报成「远端拒绝」。
fn ensure_local_source(local: &Path) -> Result<(), SshError> {
    if local.exists() {
        Ok(())
    } else {
        Err(SshError::LocalMissing {
            path: local.display().to_string(),
        })
    }
}

/// 上传的源参数（本地路径，直接交给 `scp`）。
fn upload_source(local: &Path) -> String {
    local.to_string_lossy().into_owned()
}

/// 上传的落点参数（`用户@主机:目录`）。
fn upload_destination(target: &Target, remote_dir: &str) -> String {
    format!("{}:{remote_dir}", target.destination())
}

/// 下载：远端 → 本地。
pub fn download(
    target: &Target,
    socket_dir: Option<&Path>,
    remote_path: &str,
    local_dir: &Path,
) -> Result<(), SshError> {
    transfer(
        target,
        socket_dir,
        &download_source(target, remote_path),
        &download_destination(local_dir),
    )?
    .wait()
}

/// 下载的**可中断**版本。
pub fn download_spawn(
    target: &Target,
    socket_dir: Option<&Path>,
    remote_path: &str,
    local_dir: &Path,
) -> Result<TransferHandle, SshError> {
    transfer_spawn(
        target,
        socket_dir,
        &download_source(target, remote_path),
        &download_destination(local_dir),
    )
}

/// 下载的源参数：远端路径交给远端的 shell 解析，所以按 POSIX 规则转义 ——
/// 路径里有空格（`my report.pdf`）时不转义会被拆成两个参数。
fn download_source(target: &Target, remote_path: &str) -> String {
    format!("{}:{}", target.destination(), shell_quote(remote_path))
}

/// 下载的落点参数（本地目录）。
fn download_destination(local_dir: &Path) -> String {
    local_dir.to_string_lossy().into_owned()
}

/// 远端路径当前的字节数（文件取实际大小，目录递归求和）。
///
/// 传输进度用它：上传时量**落点**、下载时量**源**。量不到（路径还不存在、
/// 远端没有 `du` / `wc`）返回 `None` —— 进度退化成「不确定」，传输本身不受影响。
///
/// 目录用 `du -sk`（POSIX，`-k` 保证单位是 1024 字节）乘回字节；文件用 `wc -c`，
/// 它是**已写入的字节数**，`scp` 正往里写时这个数就单调增长。
pub fn path_size(target: &Target, socket_dir: Option<&Path>, path: &str) -> Result<u64, SshError> {
    let script = r#"if [ -d "$1" ]; then
  n=$(du -sk "$1" 2>/dev/null | cut -f1 | tr -d ' \t')
  [ -n "$n" ] && printf '%s' "$((n * 1024))"
else
  wc -c < "$1" 2>/dev/null | tr -d ' \t'
fi"#;
    let command = format!("sh -c {} duck {}", shell_quote(script), shell_quote(path));
    let output = exec(target, socket_dir, &command)?;
    parse_size(&output.stdout).ok_or(SshError::Failed {
        status: output.status,
        stderr: format!("unexpected size output: {}", output.stdout.trim()),
    })
}

/// 解析 [`path_size`] 脚本的输出。空串 / 非数字都算「量不到」。
fn parse_size(stdout: &str) -> Option<u64> {
    let text = stdout.trim();
    if text.is_empty() {
        None
    } else {
        text.parse::<u64>().ok()
    }
}

/// 本地路径的字节数（文件取大小，目录递归求和）。
///
/// 上传进度拿它当分母。**不跟随软链接**（`symlink_metadata` + 目录项递归）：
/// `scp -r` 也不跟随，跟随会把链接目标算进来、分母虚高、进度永远到不了头。
#[must_use]
pub fn local_size(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_dir() {
        let mut total = 0u64;
        for entry in std::fs::read_dir(path).ok()?.flatten() {
            total = total.saturating_add(local_size(&entry.path()).unwrap_or(0));
        }
        Some(total)
    } else if meta.is_file() {
        Some(meta.len())
    } else {
        // 软链接 / 设备 / FIFO：不计入分母（`scp` 传的也不是它们的内容）。
        Some(0)
    }
}

/// 一次**可中断**的 `scp` 传输。
///
/// 「起进程」与「等结果」是两个动作：本结构只负责起，成败由 [`Self::wait`] 给；
/// 期间可以随时 [`Self::kill`]。
///
/// 三个字段都是 `Arc`，所以句柄可以 `Clone`（调用方一份用来等、一份用来取消，
/// 与共享同一台 `scp` 进程的两个视角）。**同一句柄只该被 `wait` 一次**：
/// stderr 读取线程会被 join 掉。
#[derive(Clone)]
pub struct TransferHandle {
    /// `scp` 子进程。用 `Mutex` 而不是独占所有权：取消要能从另一个线程下手。
    child: Arc<Mutex<Child>>,
    /// stderr 读取线程攒下的原文（进程退出、管道 EOF 后即为完整内容）。
    stderr: Arc<Mutex<String>>,
    /// stderr 读取线程句柄；`wait` 时 join，保证读到的内容完整。
    reader: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl TransferHandle {
    /// 终止传输进程。
    ///
    /// 幂等：进程已经退出时是一次无害的空操作。返回是否真的发出了信号。
    pub fn kill(&self) -> bool {
        let Ok(mut child) = self.child.lock() else {
            return false;
        };
        child.kill().is_ok()
    }

    /// 轮询等待进程退出（waitpid 式），返回传输结果。
    ///
    /// **不用 `Child::wait` 一把阻塞**：那样 `kill` 会被同一把锁挡在后面，
    /// 「取消」就变成了「等它跑完」。这里每 [`TRANSFER_POLL`] 查一次 `try_wait`，
    /// 锁只在查的那一瞬间持有，取消随时插得进来。
    pub fn wait(&self) -> Result<(), SshError> {
        let status = loop {
            let polled = {
                let mut child = self.child.lock().map_err(|_| SshError::Transfer {
                    message: "传输进程状态锁已中毒".to_owned(),
                })?;
                child.try_wait().map_err(|error| SshError::Transfer {
                    message: error.to_string(),
                })?
            };
            if let Some(status) = polled {
                break status;
            }
            // 锁在每一轮之间释放：取消随时插得进来。
            thread::sleep(TRANSFER_POLL);
        };
        // 进程已退出 → stderr 管道即将 / 已经 EOF。join 读取线程，避免把
        // 「还在缓冲里的最后一行报错」读成空串。
        let reader = self.reader.lock().ok().and_then(|mut slot| slot.take());
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        if status.success() {
            return Ok(());
        }
        let stderr = self
            .stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default();
        Err(SshError::Transfer {
            message: first_line(stderr.trim()),
        })
    }
}

/// `scp` 的公共入口（一步式）：起进程后立刻等结果。
///
/// 两个位置参数都是**字符串**而不是 `Path`：其中一个是 `用户@主机:路径` 形式，
/// 它不是本地路径，用 `Path` 承载会在 Windows 上被反斜杠规范化改写掉。
fn transfer(
    target: &Target,
    socket_dir: Option<&Path>,
    source: &str,
    destination: &str,
) -> Result<TransferHandle, SshError> {
    transfer_spawn(target, socket_dir, source, destination)
}

/// 起 `scp` 进程但**不等它**：返回可中断句柄。
fn transfer_spawn(
    target: &Target,
    socket_dir: Option<&Path>,
    source: &str,
    destination: &str,
) -> Result<TransferHandle, SshError> {
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
    let mut child = Command::new(program)
        .args(&argv)
        .envs(target.env(helper.as_deref()))
        .stdin(Stdio::null())
        // stdout 丢弃：`scp` 正常不往 stdout 写东西，接管它只会让管道缓冲成为隐患。
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| SshError::Transfer {
            message: error.to_string(),
        })?;

    // stderr 单独一个线程读到 EOF：不在 `wait` 里同步读，是因为读操作会阻塞到
    // 进程退出 —— 那就又回到了「取消不了」。
    let stderr_pipe = child.stderr.take();
    let stderr = Arc::new(Mutex::new(String::new()));
    let reader = {
        let buffer = Arc::clone(&stderr);
        thread::spawn(move || {
            let Some(mut pipe) = stderr_pipe else {
                return;
            };
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            if let Ok(mut slot) = buffer.lock() {
                *slot = text;
            }
        })
    };

    Ok(TransferHandle {
        child: Arc::new(Mutex::new(child)),
        stderr,
        reader: Arc::new(Mutex::new(Some(reader))),
    })
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
    fn cwd_header_is_stripped_from_the_listing() {
        let (cwd, body) = take_cwd("__DUCK_CWD__\t/home/bws\nf\t1\t2026-01-02T03:04\tok.txt\n");
        assert_eq!(cwd.as_deref(), Some("/home/bws"));
        // 头部不能被当成一条条目（`splitn(4)` 会把 `__DUCK_CWD__` 当第三列）。
        let entries = parse_listing(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ok.txt");
    }

    #[test]
    fn listing_without_cwd_header_still_parses() {
        // 远端脚本版本不一致 / 第一行被别的东西占了：不能整次列举都作废，
        // 只是界面拿不到绝对路径（`~` 会留在面板上）。
        let (cwd, body) = take_cwd("f\t1\t2026-01-02T03:04\tok.txt\n");
        assert_eq!(cwd, None);
        assert_eq!(parse_listing(body).len(), 1);
    }

    #[test]
    fn cd_failure_becomes_a_path_naming_error() {
        let error = cd_error(
            SshError::Failed {
                status: Some(1),
                stderr: NO_DIR_SENTINEL.to_owned(),
            },
            "~/nope",
        );
        assert_eq!(
            error,
            SshError::NoSuchDir {
                path: "~/nope".to_owned()
            }
        );
        // 别的非零退出原样透传：不能把所有失败都说成「目录不存在」。
        let other = cd_error(
            SshError::Failed {
                status: Some(255),
                stderr: "Permission denied (publickey).".to_owned(),
            },
            "/x",
        );
        assert!(matches!(other, SshError::Failed { .. }), "{other:?}");
    }

    /// 远端脚本必须真的能展开 `~`（用本机 `sh` 跑同一条命令）。
    ///
    /// 这是本次「SFTP 一打开就报 远端命令失败（退出码 1）」的**根因回归测试**：
    /// 路径被单引号包着交给远端，单引号内 `~` 不展开，`cd` 于是去找一个名为 `~`
    /// 的目录。没有远端主机时这是唯一能真验它的方式。
    #[cfg(unix)]
    #[test]
    fn list_command_expands_tilde_on_the_far_side() {
        let home = std::env::temp_dir().join(format!("duck-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join("sub")).expect("建假家目录");
        std::fs::write(home.join("probe.txt"), b"hi").expect("写文件");
        let expected = std::fs::canonicalize(&home).expect("解析家目录");

        let output = Command::new("sh")
            .arg("-c")
            .arg(list_command("~"))
            .env("HOME", &home)
            .output()
            .expect("跑 sh");
        assert!(
            output.status.success(),
            "`~` 必须能列举：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let (cwd, body) = take_cwd(&stdout);
        let expected_home = expected.to_string_lossy().into_owned();
        assert_eq!(
            cwd.as_deref(),
            Some(expected_home.as_str()),
            "远端要回报解析后的绝对目录"
        );
        assert!(
            parse_listing(body)
                .iter()
                .any(|entry| entry.name == "probe.txt"),
            "{stdout}"
        );

        // `~/子目录` 形态同样要展开：面板往下一层走时用的就是它。
        let output = Command::new("sh")
            .arg("-c")
            .arg(list_command("~/sub"))
            .env("HOME", &home)
            .output()
            .expect("跑 sh");
        assert!(
            output.status.success(),
            "`~/sub` 必须能列举：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let (cwd, _) = take_cwd(&stdout);
        let expected_sub = expected.join("sub").to_string_lossy().into_owned();
        assert_eq!(cwd.as_deref(), Some(expected_sub.as_str()));

        std::fs::remove_dir_all(&home).ok();
    }

    /// 目录不存在：非零退出 + stderr 带哨兵 + stdout 一条条目都没有。
    ///
    /// 三条缺一不可 —— `cd_error` 认哨兵，界面才不会只拿到一个裸退出码。
    #[cfg(unix)]
    #[test]
    fn missing_remote_dir_reports_itself_instead_of_a_bare_exit_code() {
        let output = Command::new("sh")
            .arg("-c")
            .arg(list_command("/definitely/not/here"))
            .output()
            .expect("跑 sh");
        assert!(!output.status.success(), "cd 失败必须是非零退出");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(NO_DIR_SENTINEL),
            "stderr 要带哨兵：{stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            parse_listing(&stdout).is_empty(),
            "失败时不能吐出半个列表：{stdout}"
        );
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
            auth: AuthMethod::Auto,
            options: Vec::new(),
            forwards: Vec::new(),
            agent_forwarding: false,
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
            auth: AuthMethod::Auto,
            options: Vec::new(),
            forwards: Vec::new(),
            agent_forwarding: false,
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

    #[test]
    fn local_size_sums_files_and_recurses_directories() {
        let root = std::env::temp_dir().join(format!("duck-size-{}", std::process::id()));
        let nested = root.join("inner");
        std::fs::create_dir_all(&nested).expect("建临时目录");
        std::fs::write(root.join("a.txt"), vec![b'a'; 100]).expect("写文件");
        std::fs::write(nested.join("b.txt"), vec![b'b'; 23]).expect("写嵌套文件");

        assert_eq!(local_size(&root.join("a.txt")), Some(100));
        // 分母是**递归总和** —— 目录上传时进度才不会停在 0。
        assert_eq!(local_size(&root), Some(123));
        assert_eq!(local_size(&root.join("nope")), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_size_output_parsing_rejects_non_numbers() {
        // 远端命令没吐出数字时（例如远端的 `sh` 里 `du` / `wc` 缺失）不能把结果当成
        // 0 字节 —— 那会让进度条凭空显示「100%」，比「不确定」误导得多。
        assert_eq!(parse_size("1024\n"), Some(1024));
        assert_eq!(parse_size("  4096 "), Some(4096));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("\n"), None);
        assert_eq!(parse_size("du: not found"), None);
    }
}
