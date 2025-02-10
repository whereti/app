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

/// "24.10.2026 16:45" (Romanian date order).
pub fn local_time(minutes_utc: u32, tz_offset: i32) -> String {
    if minutes_utc == 0 {
        return String::new();
    }
    let local = minutes_utc as i64 + tz_offset as i64;
    let (y, m, d) = whereti_query::civil_from_day(local.div_euclid(1440));
    let mins = local.rem_euclid(1440);
    format!("{d:02}.{m:02}.{y:04} {:02}:{:02}", mins / 60, mins % 60)
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
        format!("{v:.1} {}", UNITS[u]).replace('.', ",")
    }
}

/// "1.243.551" (dot as thousands separator).
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn romanian_formats() {
        assert_eq!(count(1_243_551), "1.243.551");
        assert_eq!(size(1536), "1,5 KB");
        assert_eq!(size(12), "12 B");
        let m = (whereti_query::day_from_civil(2026, 10, 24) * 1440 + 13 * 60 + 45) as u32;
        assert_eq!(local_time(m, 180), "24.10.2026 16:45");
        assert_eq!(local_time(0, 180), "");
    }
}
