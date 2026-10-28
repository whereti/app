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
        Err(crate::trf!("Windows could not open {path}", path))
    }
}

/// Opens a file or folder with its default program, from a thread of its
/// own that ends right after (a NAS asleep may hold it a while): Windows
/// finishes starting the program before this returns. COM must be
/// initialised on the thread.
pub fn open_and_wait(path: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
    let (verb, file) = (wide("open"), wide(path));
    // SAFETY: zeroed struct with its size set; the strings outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: a valid struct.
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        Ok(())
    } else {
        Err(crate::trf!("Windows could not open {path}", path))
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

/// `show`: ask a whereti that already runs to show its window (not for a
/// launch that should stay hidden).
pub fn claim_instance(show: bool) -> Instance {
    let (mutex_name, event_name) = (wide("Local\\whereti-app"), wide("Local\\whereti-app-show"));
    // SAFETY: plain object creation; handles live for the whole process.
    unsafe {
        let _mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        let first = GetLastError() != ERROR_ALREADY_EXISTS;
        let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
        if first {
            Instance::First { show_requests: event }
        } else {
            if show {
                SetEvent(event);
            }
            Instance::AlreadyRunning
        }
    }
}

/// Windows 11 can reopen apps that were running when the PC restarted; it
/// would start whereti without `--hidden` and bring up its window. This
/// tells Windows to reopen it with `--hidden` (after an update or a restart,
/// not after a crash or hang).
pub fn register_restart_hidden() {
    use windows_sys::Win32::System::Recovery::{RegisterApplicationRestart, RESTART_NO_CRASH, RESTART_NO_HANG};
    let args = wide("--hidden");
    // SAFETY: a NUL-terminated string that Windows copies.
    unsafe { RegisterApplicationRestart(args.as_ptr(), RESTART_NO_CRASH | RESTART_NO_HANG) };
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
/// locked, the screen saver is on or nobody has touched the PC for a while.
pub fn user_state() -> (bool, bool) {
    let (busy, away) = notification_state();
    (busy, away || (!busy && idle_for() >= AWAY_AFTER))
}

/// No key or mouse movement for this long: the user is away.
const AWAY_AFTER: std::time::Duration = std::time::Duration::from_secs(180);

/// Time since the last key press or mouse movement in this session.
fn idle_for() -> std::time::Duration {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: an out-structure with its size set.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return std::time::Duration::ZERO;
    }
    let now = unsafe { GetTickCount() };
    std::time::Duration::from_millis(now.wrapping_sub(info.dwTime) as u64)
}

fn notification_state() -> (bool, bool) {
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

/// Writes a file whole or not at all: to a temporary file beside it, then
/// renamed over it, so a crash or power loss never leaves it half written.
pub fn write_whole(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// One saved file, written off the UI thread: only the newest text is
/// written, and a deletion (Forget) is never undone by a write still on its
/// way.
pub struct Saved {
    latest: std::sync::Mutex<u64>,
}

impl Saved {
    pub const fn new() -> Saved {
        Saved { latest: std::sync::Mutex::new(0) }
    }

    /// Writes `text` to `path` in the background, unless a newer save or a
    /// deletion comes first.
    pub fn save(&'static self, path: std::path::PathBuf, text: String) {
        let mine = {
            let mut n = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            *n
        };
        std::thread::spawn(move || {
            let n = self.latest.lock().unwrap_or_else(|e| e.into_inner());
            if *n == mine {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = write_whole(&path, text.as_bytes());
            }
        });
    }

    /// Deletes the file; saves still on their way are dropped.
    pub fn delete(&self, path: &std::path::Path) {
        let mut n = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        let _ = std::fs::remove_file(path);
    }
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
