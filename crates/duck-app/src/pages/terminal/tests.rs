//! 模块单测：克隆命名、SFTP 目标存活、分屏键唯一、私钥路径、远端家目录。

use super::*;

/// 克隆连接的命名：没占就用「副本」，占了一路往后加序号，绝不撞名。
///
/// 这条钉住的是列表里「两条一模一样的名字」不再出现 —— 重名连接在界面上无法区分，
/// 用户点哪条全凭猜。
#[test]
fn clone_label_appends_suffix_until_free() {
    // 没人占：直接用「副本」。
    assert_eq!(
        unique_clone_label("prod-01", "副本", |_| false),
        "prod-01 副本"
    );
    // 已被占：跳到下一个空位。
    let taken = ["prod-01 副本", "prod-01 副本 2"];
    assert_eq!(
        unique_clone_label("prod-01", "副本", |label| taken.contains(&label)),
        "prod-01 副本 3"
    );
    // 只占了中间一个：仍从最小可用序号起。
    let sparse = ["prod-01 副本 2"];
    assert_eq!(
        unique_clone_label("prod-01", "副本", |label| sparse.contains(&label)),
        "prod-01 副本"
    );
}

/// 用户实测报告：「SFTP 面板打开后关掉，再点图标就打不开了」。
///
/// 根因是目标只从**当前标签**推导 —— 面板关掉后活动标签回落到本地 shell（或空态），
/// 于是图标要么禁用、要么对着本地 shell 开出一个永远列举不出东西的假面板。
/// 这条钉住的是修复的核心：**标签没了，目标还在**。
#[test]
fn sftp_target_survives_closing_the_panel() {
    let real = |id: &str| id != data::LOCAL_SHELL_ID;
    // 面板已关（当前标签是本地 shell），但上次开过 `prod-01`。
    assert_eq!(
        resolve_sftp_owner(Some(data::LOCAL_SHELL_ID), Some("prod-01"), &real).as_deref(),
        Some("prod-01"),
        "面板关掉后仍应记得上次的连接"
    );
    // 连标签都没有了（只剩记忆）。
    assert_eq!(
        resolve_sftp_owner(None, Some("prod-01"), &real).as_deref(),
        Some("prod-01")
    );
    // 当前标签就是一条远端连接时，以它为准（用户看得见的那条优先）。
    assert_eq!(
        resolve_sftp_owner(Some("staging-02"), Some("prod-01"), &real).as_deref(),
        Some("staging-02")
    );
}

#[test]
fn sftp_target_refuses_local_shell_and_missing_profiles() {
    let real = |id: &str| id != data::LOCAL_SHELL_ID;
    // 本地 shell 不是远端主机 —— 一个候选都不能剩下。
    assert_eq!(
        resolve_sftp_owner(Some(data::LOCAL_SHELL_ID), None, &real),
        None
    );
    assert_eq!(
        resolve_sftp_owner(
            Some(data::LOCAL_SHELL_ID),
            Some(data::LOCAL_SHELL_ID),
            &real
        ),
        None
    );
    // 档案已删（`usable` 说不行）时越过它，而不是拿一条查不到的记录去开面板。
    let only_prod = |id: &str| id == "prod-01";
    assert_eq!(
        resolve_sftp_owner(Some("gone-01"), Some("prod-01"), &only_prod).as_deref(),
        Some("prod-01")
    );
    assert_eq!(resolve_sftp_owner(Some("gone-01"), None, &only_prod), None);
}

/// 分屏窗格的会话键不能撞上 SFTP 标签的键，也不能等于任何标签 id
///（否则 `sessions` 表里两个角色共用一个键，互相覆盖会话）。
#[test]
fn split_key_never_collides_with_sftp_or_tab_ids() {
    let id = "prod-01";
    let split = data::split_session_key(id);
    assert_ne!(split, id);
    assert_ne!(split, format!("{id}/sftp"));
    // 拼接是幂等的：同一标签反复开分屏拿到同一个键（会话表才不会攒下一堆孤儿）。
    assert_eq!(data::split_session_key(id), split);
}

/// 档案里的私钥路径只属于「公钥」认证。
///
/// 用户实测报告：「选了密码/SSH Agent 还是连不上，一直报
/// `Permission denied (publickey,password)`」。根因是档案里残留的私钥路径
/// 让 `duck-ssh` 加上 `-i` + `IdentitiesOnly=yes`，把 agent 与口令两条路一起关掉。
/// 这条钉住「非公钥 ⇒ 不留私钥路径」，从而保证 `ssh_target` 拿不到 identity。
#[test]
fn key_path_is_kept_only_for_public_key_auth() {
    let path = "/Users/me/.ssh/id_ed25519".to_owned();
    assert_eq!(
        stored_key_file(AuthKind::Key, path.clone()),
        path,
        "公钥认证必须原样保留路径"
    );
    // 切到口令 / agent ⇒ 清空（否则 IdentitiesOnly 会顶掉口令与 agent）。
    assert_eq!(
        stored_key_file(AuthKind::Password, path.clone()),
        String::new()
    );
    assert_eq!(stored_key_file(AuthKind::Agent, path), String::new());
    // 没有路径时三种认证都得到空串（不是 `Some("")`）。
    assert_eq!(stored_key_file(AuthKind::Key, String::new()), String::new());
}

/// 用户实测报告：「SFTP 打开就报 远端命令失败（退出码 1）」。
///
/// 根因在远端脚本（路径带单引号交给远端，`~` 不展开，见
/// `duck_ssh::list_dir` 的前导）。界面上还并着第二处：远端「主目录」按钮交出的是
/// `Side::home()`（空串），而 `normalize("")` 把它变成 `/` —— 于是「主目录」
/// 把面板带到**根目录**，并且「远端目录尚未确定」这个前置状态被一并抹掉
///（写操作的空路径检查随之失效，会拿一个猜的目录去 `mkdir`）。
/// 这条钉住「远端空路径保留原样，交给列举去解析 `~`」。
#[test]
fn remote_home_stays_unresolved_instead_of_becoming_root() {
    let mut pane = PaneState::new(Side::Remote);
    assert_eq!(pane.path, "", "未连接时远端不该有一个猜出来的路径");
    pane.navigate(Side::Remote, Side::Remote.home());
    assert_eq!(
        pane.path, "",
        "远端家目录不可知：留空串，由列举脚本在远端展开 `~`"
    );
    assert!(!pane.loaded, "留空串是为了触发一次列举");

    // 绝对路径照常归一（尾斜杠去掉）：远端不是「什么都原样收下」。
    pane.navigate(Side::Remote, "/var/log/".to_owned());
    assert_eq!(pane.path, "/var/log");
}
