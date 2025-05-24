//! Windows integration: open, reveal in Explorer, single instance, local time.

use std::os::windows::process::CommandExt;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Opens a file or folder with its default program.
pub fn open(path: &str) -> Result<(), String> {
    let (verb, file) = (wide("open"), wide(path));
    // SAFETY: NUL-terminated strings; no window handle needed.
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    if r as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows could not open {path}"))
    }
}

/// Opens Explorer with the item selected.
pub fn reveal(path: &str) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Single instance: the first process owns a named mutex; a second launch
/// signals the first one (which shows its window) and exits.
pub enum Instance {
    First { show_requests: HANDLE },
    AlreadyRunning,
}

pub fn claim_instance() -> Instance {
    let (mutex_name, event_name) = (wide("Local\\whereti-app"), wide("Local\\whereti-app-show"));
    // SAFETY: plain object creation; handles live for the whole process.
    unsafe {
        let _mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        let first = GetLastError() != ERROR_ALREADY_EXISTS;
        let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
        if first {
            Instance::First { show_requests: event }
        } else {
            SetEvent(event);
            Instance::AlreadyRunning
        }
    }
}

/// Calls `on_show` each time another launch asks this instance to show itself.
pub fn listen_for_show_requests(event: HANDLE, on_show: impl Fn() + Send + 'static) {
    let event = event as usize;
    std::thread::spawn(move || loop {
        // SAFETY: the event handle stays open for the life of the process.
        unsafe { WaitForSingleObject(event as HANDLE, INFINITE) };
        on_show();
    });
}

/// Local time minus UTC, in minutes.
pub fn tz_offset() -> i32 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    // SAFETY: zeroed POD out-parameter.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    -(tz.Bias + if id == TIME_ZONE_ID_DAYLIGHT { tz.DaylightBias } else { tz.StandardBias })
}

/// "1,2 MB" (decimal comma).
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u]).replace('.', &crate::i18n::separators().1.to_string())
    }
}

/// "1,243,551" (English) or "1.243.551" (Romanian).
pub fn count(n: u64) -> String {
    crate::i18n::number(n)
}


const RUN_KEY: windows::core::PCWSTR = windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: windows::core::PCWSTR = windows::core::w!("whereti");

/// Is whereti in the per-user "start with Windows" list?
pub fn autostart_enabled() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    // SAFETY: only asks whether the value exists.
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

/// Adds or removes `"<this exe>" --hidden` in HKCU\…\Run.
pub fn set_autostart(on: bool) -> Result<(), String> {
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    // SAFETY: valid key/value names; data is a NUL-terminated UTF-16 string.
    unsafe {
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd: Vec<u16> = format!("\"{}\" --hidden", exe.display()).encode_utf16().chain([0]).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr() as *const _),
                (cmd.len() * 2) as u32,
            )
            .ok()
            .map_err(|e| e.to_string())
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            if r.is_ok() || r.0 == 2 { Ok(()) } else { Err(r.to_hresult().message()) } // 2 = not there
        }
    }
}


/// Appends a line to `%TEMP%\wt-ui-trace.txt` when `WT_UI_TRACE` is set
/// (diagnostics only; nothing is written otherwise).
/// (busy, away): a full-screen app or presentation runs / the screen is
/// locked or the screen saver is on.
pub fn user_state() -> (bool, bool) {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    let mut state = 0;
    // SAFETY: out-parameter.
    if unsafe { SHQueryUserNotificationState(&mut state) } != 0 {
        return (false, false);
    }
    (matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE), state == QUNS_NOT_PRESENT)
}

pub fn trace(msg: impl AsRef<str>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("WT_UI_TRACE").is_some()) {
        return;
    }
    use std::io::Write;
    let path = std::env::temp_dir().join("wt-ui-trace.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{:?} {}", std::time::SystemTime::now(), msg.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn romanian_formats() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::Ro);
        assert_eq!(count(1_243_551), "1.243.551");
        assert_eq!(size(1536), "1,5 KB");
        assert_eq!(size(12), "12 B");
    }
}
