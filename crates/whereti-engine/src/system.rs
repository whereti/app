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
///
/// A folder already there is used only if SYSTEM or Administrators own it
/// and it is a real folder. Any user may create folders in ProgramData: one
/// made before whereti was installed (or after it was removed) would let
/// that user read the index and point the service's writes elsewhere (a
/// junction). Such a folder is removed, without following links inside it,
/// and made again: it holds nothing the service cannot rebuild.
pub fn ensure_data_dir() -> io::Result<PathBuf> {
    let dir = data_dir();
    if let Ok(meta) = std::fs::symlink_metadata(&dir) {
        if is_ours(&dir, &meta) {
            return Ok(dir);
        }
        let link = meta.file_type().is_symlink() || is_reparse(&meta);
        // a link goes, not what it points to; a real folder goes whole
        let removed = if link { std::fs::remove_dir(&dir).or_else(|_| std::fs::remove_file(&dir)) } else { std::fs::remove_dir_all(&dir) };
        if let Err(e) = removed {
            return Err(io::Error::other(format!("{} is not whereti's and cannot be removed: {e}", dir.display())));
        }
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

fn is_reparse(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// A real folder owned by SYSTEM or Administrators.
fn is_ours(dir: &std::path::Path, meta: &std::fs::Metadata) -> bool {
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION, PSID,
    };
    if !meta.is_dir() || meta.file_type().is_symlink() || is_reparse(meta) {
        return false;
    }
    let wide = dir.as_os_str().encode_wide_nul();
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid path; `owner` points into `sd`, freed below.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 || owner.is_null() {
        return false;
    }
    // SAFETY: `owner` is a valid SID inside `sd`.
    let ours = unsafe { IsWellKnownSid(owner, WinLocalSystemSid) != 0 || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0 };
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(sd) };
    ours
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

/// True when running on battery (PLAN.md §6.6: no indexing then).
pub fn on_battery() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // SAFETY: zeroed out-parameter.
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // ACLineStatus: 0 = offline (battery), 1 = online, 255 = unknown (desktops)
    unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0 }
}

/// How busy the whole PC's CPU was since the last call, without this
/// process (0.0–1.0).
pub struct CpuMeter {
    last: Option<(u64, u64, u64)>,
}

impl CpuMeter {
    pub fn new() -> CpuMeter {
        CpuMeter { last: None }
    }

    pub fn busy(&mut self) -> f32 {
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        let ft = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
        // SAFETY: zeroed out-parameters.
        let (mut idle, mut kernel, mut user) = unsafe { (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed()) };
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return 0.0;
        }
        // kernel time includes idle time; all in 100 ns units
        let now = (ft(idle), ft(kernel) + ft(user), process_cpu_ms() * 10_000);
        let Some(prev) = self.last.replace(now) else { return 0.0 };
        let total = now.1.saturating_sub(prev.1);
        if total == 0 {
            return 0.0;
        }
        let busy = total.saturating_sub(now.0.saturating_sub(prev.0));
        let ours = now.2.saturating_sub(prev.2);
        busy.saturating_sub(ours) as f32 / total as f32
    }
}

impl Default for CpuMeter {
    fn default() -> Self {
        Self::new()
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
