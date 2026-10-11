//! 系统外壳：用默认程序打开链接、在文件管理器里定位文件。
//!
//! 用系统命令而非第三方 crate：`open` / `xdg-open` / `explorer` 是各平台稳定接口，
//! 免去为一个动作引入一整棵依赖树。

use std::path::Path;
use std::process::Command;

/// 用系统默认程序打开 URL（浏览器、邮件客户端…）。
///
/// 返回**能被观察到的那部分**结果：启动器没能拉起来（命令不存在 / 权限不够）时报错，
/// 交给调用方提示。至于「拉起来了但系统没有程序认这个链接」，`open` / `xdg-open`
/// 自己也是静默的 —— 这里不假装比它们知道得更多。
///
/// 子进程在**后台线程**回收：直接 `spawn` 完不管会在 Unix 上留下僵尸进程（点一次链接
/// 攒一个），而原地 `wait` 又可能在某些 `xdg-open` 实现上阻塞 UI 线程。
pub fn open_url(url: &str) -> std::io::Result<()> {
    let mut child = opener_command(url).spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 各平台的「打开链接」命令。
fn opener_command(url: &str) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(url);
        command
    }
    #[cfg(target_os = "windows")]
    {
        // 第一个空参数是 `start` 的「窗口标题」占位：少了它，带引号的 URL 会被
        // `cmd` 当成标题吃掉。
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    }
}

/// 在文件管理器中定位文件（macOS 是 Finder 的「显示简介」式选中）。
pub fn reveal_in_finder(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // 没有统一「选中」语义，退化为打开其所在目录。
        let dir = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}
