//! Small Windows helpers: data folder, time, process counters, priorities.

use std::io;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{LocalFree, FILETIME};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessTimes, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    THREAD_MODE_BACKGROUND_END,
};
use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
use whereti_query::DateCtx;

/// `%ProgramData%\whereti`: the index lists every file name on the PC, so
/// only SYSTEM and Administrators may read it; users go through the service.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("whereti")
}

pub fn default_index_path() -> PathBuf {
    data_dir().join("names.idx")
}

/// Creates the data folder readable only by SYSTEM and Administrators.
pub fn ensure_data_dir() -> io::Result<PathBuf> {
    let dir = data_dir();
    if dir.is_dir() {
        return Ok(dir);
    }
    let sddl: Vec<u16> = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".encode_utf16().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid SDDL; sd freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let wide: Vec<u16> = dir.as_os_str().encode_wide_nul();
    // SAFETY: valid path and attributes.
    let ok = unsafe { CreateDirectoryW(wide.as_ptr(), &sa) };
    let err = io::Error::last_os_error();
    // SAFETY: allocated by the SDDL conversion.
    unsafe { LocalFree(sd) };
    if ok == 0 && !dir.is_dir() {
        return Err(err);
    }
    Ok(dir)
}

trait EncodeWideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl EncodeWideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain([0]).collect()
    }
}

/// Now and the local UTC offset, for `dm:today` & co.
pub fn date_ctx() -> DateCtx {
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    let unix_min = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0);
    let now = (unix_min + 134_774 * 1440) as u32;
    // SAFETY: zeroed POD out-parameter.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    let bias = tz.Bias + if id == TIME_ZONE_ID_DAYLIGHT { tz.DaylightBias } else { tz.StandardBias };
    DateCtx { now, tz_offset: -bias }
}

/// "2026-10-24 16:45:03"
pub fn local_timestamp() -> String {
    // SAFETY: out-parameter.
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// CPU time used by this process (user + kernel), in milliseconds.
pub fn process_cpu_ms() -> u64 {
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10_000;
    // SAFETY: zeroed out-parameters for the current process.
    unsafe {
        let (mut a, mut b, mut k, mut u) = (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        if GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) == 0 {
            return 0;
        }
        ft(k) + ft(u)
    }
}

/// Private memory of this process, in bytes.
pub fn process_private_bytes() -> u64 {
    // SAFETY: valid pointer and size.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return 0;
        }
        c.PagefileUsage as u64
    }
}

/// Lowers this thread's CPU *and* disk priority until dropped (PLAN.md §6.6).
pub struct Background;

impl Background {
    pub fn begin() -> Background {
        // SAFETY: current thread pseudo-handle.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
        Background
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        // SAFETY: current thread pseudo-handle.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
    }
}

/// A thread pool for maintenance (compaction, rescans) whose threads run in
/// background mode, so this work never competes with the user or with searches.
pub fn maintenance_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads((crate::sync::default_threads() / 2).max(1))
        .thread_name(|i| format!("maintenance-{i}"))
        .start_handler(|_| std::mem::forget(Background::begin()))
        .build()
        .expect("maintenance pool")
}
