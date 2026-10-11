//! 开机自启：登录系统后自动运行本应用。
//!
//! 状态的**唯一事实来源在操作系统**，本模块不做二次持久化：
//! - macOS 走 `SMAppService`（系统设置 → 通用 → 登录项，macOS 13+）；
//! - Windows 写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
//!
//! 两端都只在对应平台参与编译；其余平台（如 Linux）返回
//! [`LaunchAtLoginError::UnsupportedPlatform`]，界面开关保持可显示但切换报错。

use std::fmt;

/// 开机自启操作失败的原因。
///
/// `Display` 是英文（日志与错误约定）；界面文案由调用方用 `tr` 组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchAtLoginError {
    /// 当前平台没有实现开机自启后端。
    UnsupportedPlatform,
    /// macOS 低于 13：`SMAppService` 不可用。
    UnsupportedOsVersion,
    /// 无法定位当前可执行文件（Windows 需要它写注册表）。
    ExecutableLocationFailed,
    /// 系统接口调用失败（注册 / 注销 / 注册表读写）。
    SystemApiFailed,
}

impl fmt::Display for LaunchAtLoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::UnsupportedPlatform => "launch at login is not supported on this platform",
            Self::UnsupportedOsVersion => "launch at login requires macOS 13 or later",
            Self::ExecutableLocationFailed => "failed to locate the current executable",
            Self::SystemApiFailed => "the system login-items API call failed",
        };
        f.write_str(text)
    }
}

impl std::error::Error for LaunchAtLoginError {}

/// 当前是否已登记为登录自启。
///
/// 查询失败（含平台不支持）一律按「未启用」处理：界面开关显示关闭，
/// 用户切换时会从 [`set_launch_at_login`] 拿到具体错误。
#[must_use]
pub fn launch_at_login_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::available() && macos::enabled()
    }
    #[cfg(target_os = "windows")]
    {
        windows::enabled()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// 登记（`enable = true`）或注销（`enable = false`）登录自启。
pub fn set_launch_at_login(enable: bool) -> Result<(), LaunchAtLoginError> {
    #[cfg(target_os = "macos")]
    {
        if !macos::available() {
            return Err(LaunchAtLoginError::UnsupportedOsVersion);
        }
        macos::set(enable)
    }
    #[cfg(target_os = "windows")]
    {
        windows::set(enable)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = enable;
        Err(LaunchAtLoginError::UnsupportedPlatform)
    }
}

// ── macOS：SMAppService（登录项）────────────────────────────────────────
#[cfg(target_os = "macos")]
mod macos {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use super::LaunchAtLoginError;

    /// `SMAppService` 需要 macOS 13+；更老的系统上该类不存在，objc2 的
    /// `class!` 会直接 panic，所以必须先用系统版本把入口拦住。
    pub fn available() -> bool {
        super::macos_major_version().is_some_and(|major| major >= 13)
    }

    pub fn enabled() -> bool {
        // 安全性：`available()` 已确认 macOS ≥ 13，`SMAppService` 类必然存在，
        // 消息发送不会命中空类。
        let service = unsafe { SMAppService::mainAppService() };
        let status = unsafe { service.status() };
        status == SMAppServiceStatus::Enabled
    }

    pub fn set(enable: bool) -> Result<(), LaunchAtLoginError> {
        // 安全性：同上，入口处已按系统版本拦截。
        let service = unsafe { SMAppService::mainAppService() };
        let result = if enable {
            unsafe { service.registerAndReturnError() }
        } else {
            unsafe { service.unregisterAndReturnError() }
        };
        if let Err(error) = &result {
            tracing::warn!(?error, "SMAppService register/unregister failed");
        }
        result.map_err(|_| LaunchAtLoginError::SystemApiFailed)
    }
}

/// 读取 macOS 主版本号（`kern.osproductversion`，如 "15.6" → 15）。
///
/// 用 `sysctlbyname` 而非 `NSProcessInfo`：不为此引入 objc2-foundation 直依赖
/// （`libc` 本就在依赖图里）。读不到按 `None` 处理，调用方当「不支持」。
#[cfg(target_os = "macos")]
fn macos_major_version() -> Option<u32> {
    use std::ffi::{CStr, CString, c_char};

    let name = CString::new("kern.osproductversion").ok()?;
    let mut buffer = [0u8; 32];
    let mut size = buffer.len();
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast::<c_char>(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size == 0 || size > buffer.len() {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buffer[..size - 1]).ok()?;
    let major = text.to_str().ok()?.split('.').next()?;
    major.parse().ok()
}

// ── Windows：HKCU Run 注册表 ────────────────────────────────────────────
#[cfg(target_os = "windows")]
mod windows {
    use std::path::Path;

    use windows_registry::CURRENT_USER;

    use super::LaunchAtLoginError;

    /// Run 键与值的命名空间（值名 = 应用显示名，删改都按这个名字定位）。
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Duck Toolbox";

    pub fn enabled() -> bool {
        CURRENT_USER
            .open(RUN_KEY)
            .ok()
            .and_then(|key| key.get_string(VALUE_NAME).ok())
            .is_some()
    }

    pub fn set(enable: bool) -> Result<(), LaunchAtLoginError> {
        if !enable {
            let key = CURRENT_USER.open(RUN_KEY).map_err(log_and_fail)?;
            key.remove_value(VALUE_NAME).map_err(log_and_fail)?;
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(log_and_fail)?;
        let command = quoted(&exe);
        let key = CURRENT_USER.create(RUN_KEY).map_err(log_and_fail)?;
        key.set_string(VALUE_NAME, command).map_err(log_and_fail)
    }

    /// 注册表 Run 值是命令行：路径带空格时必须整体加引号。
    fn quoted(path: &Path) -> String {
        format!("\"{}\"", path.display())
    }

    /// 注册表 API 的错误类型不公开 re-export，只依赖 `Display` 即可记录。
    fn log_and_fail<E: std::fmt::Display>(error: E) -> LaunchAtLoginError {
        tracing::warn!(%error, "registry Run key update failed");
        LaunchAtLoginError::SystemApiFailed
    }
}
