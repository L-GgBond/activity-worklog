use std::time::Duration;

pub struct InputActivityProbe;

impl InputActivityProbe {
    pub fn new() -> Self {
        Self
    }

    pub fn idle_duration(&self) -> anyhow::Result<Duration> {
        platform_idle_duration()
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

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_idle_duration() -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(0))
}
