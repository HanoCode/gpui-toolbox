//! 跨模块复用的自由函数：字体初值、口令读写、SFTP 目标选择、时长与错误文案、传输进度换算、克隆命名。

use super::*;

/// 画布的**初始**字号：设置页「终端」里设了就按它，否则跟随主题的等宽字号。
///
/// 只在建页面时读一次。之后字号归工具条 / 快捷键管（用户当场调了就该立刻生效），
/// 设置页那一项是「下次进来从哪儿起」的默认值，不是实时绑定。
pub(super) fn initial_font_size(cx: &App) -> f32 {
    let theme_size = active_theme(cx).tokens().typography.mono_md.size.to_f64() as f32;
    super::super::module_settings::terminal_settings(cx)
        .font_size
        .map_or(theme_size, |size| size as f32)
        .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end())
}

/// 弹窗口令按保存口径的「应存明文」。
///
/// 只有「密码认证 + 勾记住 + 填了值」才算一条要保存的口令；其余情形都是
/// `None`（清除）。判据单放一份：[`apply_password`] 落库、[`TerminalPage::apply_edit`]
/// 判「口令变没变」都对着它，两处永远不会各改各的。
pub(super) fn draft_password_value(draft: &NewConnection) -> Option<String> {
    (draft.auth == AuthKind::Password && draft.remember && !draft.password.is_empty())
        .then(|| draft.password.clone())
}

/// 档案里该留存的私钥路径。
///
/// 认证方式不是「公钥」时一律存空串。留着旧路径有两个后果：
/// ① 用户切到「密码 / SSH Agent」后，切换本身**看不出区别**（档案里仍挂着私钥），
/// 而 `duck-ssh` 侧的行为差别极大（见 [`TerminalPage::ssh_target`] 的注释）；
/// ② 用户切回「公钥」时会看到一个自己不记得填过的路径，误以为已经配好了。
///
/// 落库与比较**共用这一份**：`apply_edit` 的「参数变没变」也拿它算，
/// 否则清空私钥这一下不会触发重连。
pub(super) fn stored_key_file(auth: AuthKind, key_file: String) -> String {
    if auth == AuthKind::Key {
        key_file
    } else {
        String::new()
    }
}

/// 把弹窗口令落到档案的密文字段（按弹窗口径）。
///
/// - 认证方式不是密码，或没勾「记住口令」→ 清掉（`None`）：
///   留着就是给已改掉的认证方式留一份秘密，开关的语义是「这个连接**不**保存口令」。
/// - 填了值 + 勾「记住口令」→ AES-256-GCM 加密后存进档案行
///（[`duck_platform::vault`]；SQLite 里 grep 不到明文，编辑弹窗能解密回显）。
/// - 加密失败（密钥文件写不出等）：口令不保存、连接退回终端里手输 ——
///   弹一个「保存失败」只会把用户卡在一个没有下一步可做的提示框前。
///
/// 编辑弹窗**回显已存口令**（解密回填），所以空值就是「清除口令」的明确意图，
/// 不再有「空 = 不修改」的旧口径。
pub(super) fn apply_password(profile: &mut StoredProfile, draft: &NewConnection) {
    profile.password = draft_password_value(draft)
        .as_deref()
        .map(duck_platform::vault::seal)
        .transpose()
        .ok()
        .flatten();
}

/// 读出档案里保存的口令明文（解密）；没存过 / 解不开都是 `None`。
///
/// 解不开（换了机器、密钥文件被删）时退回 ssh 自己的交互提示 ——
/// 至少不会拿一个错误的值去试。
pub(super) fn open_password(profile: &StoredProfile) -> Option<String> {
    let sealed = profile.password.as_deref()?;
    duck_platform::vault::open(sealed)
        .ok()
        .filter(|password| !password.is_empty())
}

/// 「打开 SFTP 面板」的目标解析：当前标签所属的连接优先，其次上次开过的那条。
///
/// 拆成纯函数只为一件可观测的事能被单测钉住：**面板关掉之后目标不能丢**。
/// 之前这里只看当前标签，于是「开 SFTP → 关掉 → 再点图标」什么也不会发生
///（活动标签已回落到本地 shell / 空态）。
pub(super) fn resolve_sftp_owner(
    active: Option<&str>,
    last: Option<&str>,
    usable: &dyn Fn(&str) -> bool,
) -> Option<String> {
    [active, last]
        .into_iter()
        .flatten()
        .find(|id| usable(id))
        .map(str::to_owned)
}

/// 把一段时长说成人话（`2 小时 5 分` / `8 分 12 秒` / `46 秒`）。
///
/// 只精确到需要的那一档：连接了三天的人不关心「还有 17 秒」，
/// 而刚连上的人最想看的就是秒数在动。
///
/// 走 `tf` 而不是 `format!`：这三条模板进的是英译表，
/// 用 `format!` 的话英文界面里会长出「2 小时 5 分」。
pub(super) fn human_duration(total: std::time::Duration, lang: Lang) -> String {
    let seconds = total.as_secs();
    let (hours, minutes, rest) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    if hours > 0 {
        tf(
            lang,
            "{hours} 小时 {minutes} 分",
            &[&hours.to_string(), &minutes.to_string()],
        )
    } else if minutes > 0 {
        tf(
            lang,
            "{minutes} 分 {rest} 秒",
            &[&minutes.to_string(), &rest.to_string()],
        )
    } else {
        tf(lang, "{rest} 秒", &[&rest.to_string()])
    }
}

/// 把内核错误转成当前语言的用户说明。
///
/// `duck-ssh` 零依赖、不认识 `duck-theme`，只交出「源串 + 参数」（`SshError::template`），
/// 换语言这一步只能在界面层做 —— 否则英文界面里会冒出中文报错。
pub(super) fn ssh_error_text(cx: &App, error: &duck_ssh::SshError) -> String {
    let (source, args) = error.template();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    tf(active_theme(cx).lang(), source, &args)
}

/// 下载落点：`scp` 会把远端文件写到「本地目录 / 远端文件名」。
///
/// 远端路径里最后一个 `/` 之后就是文件名（远端一律 POSIX 分隔符）。拿不到名字时
/// 退回整个远端路径 —— 那样量到的多半是 `None`（进度降级），但**不会误报**。
pub(super) fn local_destination(local_dir: &Path, remote_file: &str) -> PathBuf {
    let name = remote_file.rsplit('/').next().unwrap_or(remote_file);
    local_dir.join(name)
}

/// 克隆连接的**显示名**：`原名 副本`，重名再追加 ` 2`、` 3`……
///
/// 按显示名而不是 id 去重：用户认的是列表里那行字，只按 id 去重会让列表里出现两条
/// 同名连接，分不清哪条是哪条。`taken` 由调用方问「这个名字占没占」，好单测。
pub(super) fn unique_clone_label(base: &str, mark: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut label = format!("{base} {mark}");
    let mut suffix = 2;
    while taken(&label) {
        label = format!("{base} {mark} {suffix}");
        suffix += 1;
    }
    label
}

/// 一次传输的**总字节数**（进度条的分母）。量不到就是 `None`。
pub(super) fn transfer_total_bytes(
    request: &TransferRequest,
    handle: &RemoteHandle,
) -> Option<u64> {
    if request.upload {
        // 上传：源在本地，直接递归数文件大小。
        duck_ssh::local_size(&request.local_file)
    } else {
        // 下载：源在远端，起一次 ssh 问 `wc -c` / `du -sk`。
        duck_ssh::path_size(
            &handle.target,
            Some(&handle.socket_dir),
            &request.remote_file,
        )
        .ok()
    }
}

/// 已传输的**字节数**：量**落点**（`scp` 正往里写，它会单调增长）。
pub(super) fn transfer_done_bytes(request: &TransferRequest, handle: &RemoteHandle) -> Option<u64> {
    if request.upload {
        duck_ssh::path_size(
            &handle.target,
            Some(&handle.socket_dir),
            &request.remote_file,
        )
        .ok()
    } else {
        duck_ssh::local_size(&local_destination(&request.local_dir, &request.remote_file))
    }
}
