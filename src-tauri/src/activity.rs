use std::time::Duration;

pub struct InputActivityProbe;

/// 前台窗口信息：应用名与窗口标题。仅在用户开启窗口识别后采集。
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub app_name: String,
    pub title: String,
}

impl InputActivityProbe {
    pub fn new() -> Self {
        Self
    }

    pub fn idle_duration(&self) -> anyhow::Result<Duration> {
        platform_idle_duration()
    }

    /// 当前前台应用与窗口标题；平台不支持或暂时取不到时返回 None，调用方跳过本次即可。
    pub fn foreground_window(&self) -> Option<WindowInfo> {
        platform_foreground_window().ok()
    }
}

#[cfg(windows)]
fn platform_idle_duration() -> anyhow::Result<Duration> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };

    unsafe {
        GetLastInputInfo(&mut info)?;
    }

    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
    let idle_ms = now.saturating_sub(info.dwTime);
    Ok(Duration::from_millis(u64::from(idle_ms)))
}

#[cfg(target_os = "macos")]
fn platform_idle_duration() -> anyhow::Result<Duration> {
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::dictionary::CFMutableDictionaryRef;
    use io_kit_sys::{
        IOObjectRelease, IORegistryEntryCreateCFProperties, IOServiceGetMatchingService,
        IOServiceMatching,
    };
    use std::ffi::CString;

    unsafe {
        let service_name = CString::new("IOHIDSystem")?;
        let service = IOServiceGetMatchingService(0, IOServiceMatching(service_name.as_ptr()));
        if service == 0 {
            anyhow::bail!("IOHIDSystem service not found");
        }

        let mut properties: CFMutableDictionaryRef = std::ptr::null_mut();
        let result = IORegistryEntryCreateCFProperties(service, &mut properties, std::ptr::null(), 0);
        IOObjectRelease(service);
        if result != 0 || properties.is_null() {
            anyhow::bail!("failed to read IOHIDSystem properties");
        }

        let dictionary = CFDictionary::<CFString, CFNumber>::wrap_under_create_rule(properties as _);
        let key = CFString::new("HIDIdleTime");
        let Some(value) = dictionary.find(&key) else {
            anyhow::bail!("HIDIdleTime not available");
        };

        let idle_ns = value.to_i64().unwrap_or(0).max(0) as u64;
        Ok(Duration::from_nanos(idle_ns))
    }
}

/// Linux：调用 X11 的 xprintidle（毫秒）拿系统空闲时长。
/// Wayland 会话没有该工具、或未安装 xprintidle 时返回 0，等价于“一直在活动”，
/// 对应设置里的「无法检测离开」提示，而不是静默算成离开。
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_idle_duration() -> anyhow::Result<Duration> {
    use std::process::Command;

    if std::env::var_os("DISPLAY").is_none() {
        return Ok(Duration::from_secs(0));
    }
    let output = match Command::new("xprintidle").output() {
        Ok(output) if output.status.success() => output,
        _ => return Ok(Duration::from_secs(0)),
    };
    let idle_ms = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    Ok(Duration::from_millis(idle_ms))
}

#[cfg(not(any(unix, windows)))]
fn platform_idle_duration() -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(0))
}

// ============================ 前台窗口识别 ============================

#[cfg(windows)]
fn platform_foreground_window() -> anyhow::Result<WindowInfo> {
    use windows::Win32::Foundation::{CloseHandle, PWSTR};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        anyhow::bail!("no foreground window");
    }

    let mut title_buf = [0u16; 512];
    let title_len =
        unsafe { GetWindowTextW(hwnd, PWSTR(title_buf.as_mut_ptr()), title_buf.len() as i32) };
    let title = if title_len > 0 {
        String::from_utf16_lossy(&title_buf[..title_len as usize])
    } else {
        String::new()
    };

    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }

    let app_name = if pid != 0 {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
        let mut path_buf = [0u16; 1024];
        let mut size = path_buf.len() as u32;
        let image_result = unsafe {
            QueryFullProcessImageNameW(handle, 0, PWSTR(path_buf.as_mut_ptr()), &mut size)
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        image_result?;
        let path = String::from_utf16_lossy(&path_buf[..size as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or(path)
    } else {
        String::new()
    };

    Ok(WindowInfo { app_name, title })
}

/// macOS：通过 osascript 询问 System Events，无需引入 Cocoa 绑定。
/// 窗口标题需要辅助功能授权；未授权时仍可拿到应用名，标题为空。
#[cfg(target_os = "macos")]
fn platform_foreground_window() -> anyhow::Result<WindowInfo> {
    use std::process::Command;

    let app_output = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get name of first application process whose frontmost is true",
        ])
        .output()?;
    let app_name = String::from_utf8_lossy(&app_output.stdout).trim().to_string();
    if app_name.is_empty() {
        anyhow::bail!("no frontmost application");
    }

    let title_output = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get title of front window of first application process whose frontmost is true",
        ])
        .output()?;
    let title = String::from_utf8_lossy(&title_output.stdout).trim().to_string();

    Ok(WindowInfo { app_name, title })
}

/// Linux/X11：通过 xprop 读取 _NET_ACTIVE_WINDOW，再取窗口名与进程号，
/// 与 xprintidle 一样走外部命令，Wayland 或缺少工具时直接返回错误。
#[cfg(all(unix, not(target_os = "macos")))]
fn platform_foreground_window() -> anyhow::Result<WindowInfo> {
    use std::process::Command;

    if std::env::var_os("DISPLAY").is_none() {
        anyhow::bail!("no DISPLAY");
    }

    let active = Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()?;
    let win_id = String::from_utf8_lossy(&active.stdout)
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_string();
    if win_id.is_empty() || win_id == "0x0" {
        anyhow::bail!("no active window");
    }

    let props = Command::new("xprop")
        .args(["-id", &win_id, "_NET_WM_NAME", "_NET_WM_PID", "WM_NAME"])
        .output()?;
    let text = String::from_utf8_lossy(&props.stdout);
    let mut title = String::new();
    let mut pid = String::new();
    for line in text.lines() {
        if line.starts_with("_NET_WM_NAME") || line.starts_with("WM_NAME") {
            if let Some(index) = line.find("= \"") {
                title = line[index + 3..].trim_end_matches('"').to_string();
            }
        } else if line.starts_with("_NET_WM_PID") {
            if let Some(index) = line.find('=') {
                pid = line[index + 1..].trim().to_string();
            }
        }
    }

    let app_name = if !pid.is_empty() {
        Command::new("ps")
            .args(["-p", &pid, "-o", "comm="])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };

    Ok(WindowInfo { app_name, title })
}

#[cfg(not(any(windows, unix)))]
fn platform_foreground_window() -> anyhow::Result<WindowInfo> {
    anyhow::bail!("foreground window not supported on this platform")
}
