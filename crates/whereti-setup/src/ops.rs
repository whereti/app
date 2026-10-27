//! What installing and uninstalling do to the PC. Everything here that
//! touches Program Files, the service or HKLM runs in the elevated copy of
//! the installer; the per-user parts (autostart, starting the app) run in
//! the normal one, so they apply to the person who installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, KEY_WOW64_64KEY, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ, RRF_RT_REG_SZ,
};

/// The programs the installer carries, compressed at build time (see build.rs).
pub const PAYLOAD: [(&str, &[u8]); 5] = [
    ("whereti.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti.exe.zst"))),
    ("whereti-service.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti-service.exe.zst"))),
    ("whereti-extract.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti-extract.exe.zst"))),
    ("wt.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/wt.exe.zst"))),
    // ONNX Runtime, Microsoft's signed build: what is in photos (PLAN.md §55)
    ("onnxruntime.dll", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/onnxruntime.dll.zst"))),
];

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\whereti");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const SERVICE: &str = "whereti";

/// A folder as Windows knows it (never from environment variables: the
/// elevated installer must not take a path the user could have set).
fn known_folder(id: &windows::core::GUID, fallback: &str) -> PathBuf {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    // SAFETY: the returned string is freed with CoTaskMemFree.
    unsafe {
        match SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let path = p.to_string().map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(fallback));
                CoTaskMemFree(Some(p.0 as _));
                path
            }
            Err(_) => PathBuf::from(fallback),
        }
    }
}

pub fn install_dir() -> PathBuf {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles, r"C:\Program Files").join("whereti")
}

fn data_dir() -> PathBuf {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramData, r"C:\ProgramData").join("whereti")
}

fn start_menu_link() -> PathBuf {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_CommonPrograms, r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs")
        .join("whereti.lnk")
}

/// System32, from Windows.
fn system32() -> PathBuf {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: a buffer of the given length.
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}

/// DLLs come from System32 only from here on.
pub fn dlls_from_system32_only() {
    use windows::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
    // SAFETY: a process-wide setting with a valid flag.
    let _ = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}

const FIREWALL_RULE: &str = "whereti document reader (no network)";

/// The document reader never needs the network: Windows Firewall blocks
/// it, so a document that took it over cannot send anything out.
fn block_reader_network(dir: &Path) {
    let netsh = system32().join("netsh.exe");
    let _ = Command::new(&netsh).args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
    let program = format!("program={}", dir.join("whereti-extract.exe").display());
    let _ = Command::new(&netsh)
        .args(["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}"), "dir=out", "action=block", &program, "enable=yes", "profile=any"])
        .output();
}

fn unblock_reader_network() {
    let netsh = system32().join("netsh.exe");
    let _ = Command::new(&netsh).args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
}

/// The version already installed, if any (from Settings → Apps).
pub fn installed_version() -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: valid key/value names and an output buffer of `size` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            UNINSTALL_KEY,
            w!("DisplayVersion"),
            RRF_RT_REG_SZ | windows::Win32::System::Registry::RRF_SUBKEY_WOW6464KEY,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok().then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
}

// ---------------------------------------------------------------- install (elevated)

pub fn install(log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let dir = install_dir();
    log("Stopping the running whereti…");
    stop_service();
    close_running(&dir);
    remove_leftovers(&dir);

    log("Copying the files…");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (name, packed) in PAYLOAD {
        let bytes = unpack(packed).map_err(|e| format!("the installer is damaged ({name}: {e})"))?;
        write_file(&dir.join(name), &bytes)?;
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let uninstaller = dir.join("uninstall.exe");
    if me != uninstaller {
        let bytes = std::fs::read(&me).map_err(|e| format!("cannot read the installer: {e}"))?;
        write_file(&uninstaller, &bytes)?;
    }
    // listed in Settings > Apps before anything can fail: what is on disk
    // can always be removed from there
    register_uninstall(&dir)?;

    block_reader_network(&dir);

    log("Installing the search service…");
    let service = dir.join("whereti-service.exe");
    if !service_exists() {
        run(&service, &["install"])?;
    }
    run(&service, &["start"])?;

    log("Adding whereti to the Start menu…");
    shortcut(&start_menu_link(), &dir.join("whereti.exe"))?;
    Ok(())
}

/// A program from the installer (zstd, see build.rs).
fn unpack(packed: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut dec = zstd::stream::Decoder::new(packed)?;
    dec.window_log_max(27)?;
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut out)?;
    Ok(out)
}

/// Writes next to the target, then renames: a half-written program is never
/// left behind. A file still held open (a whereti in another user's session,
/// an antivirus scan) is moved aside, which Windows allows even for a running
/// program, and deleted at the next install or restart.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    for attempt in 0..8 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            // a process that was just stopped may hold the file a moment longer
            Err(_) if attempt < 7 => std::thread::sleep(Duration::from_millis(250)),
            Err(_) => {}
        }
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let aside = path.with_file_name(format!("{name}.{stamp}{OLD}"));
    std::fs::rename(path, &aside)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    delete_at_restart(&aside);
    Ok(())
}

/// The ending of a program moved aside by `write_file`.
const OLD: &str = ".old";

/// Deletes the programs moved aside by an earlier install, where possible.
fn remove_leftovers(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().ends_with(OLD) {
            let _ = std::fs::remove_file(entry.path());
        }
        if entry.path().extension().is_some_and(|e| e == "new") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn delete_at_restart(path: &Path) {
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    // SAFETY: a valid path string and a null target, as the API documents for deletion.
    let _ = unsafe { MoveFileExW(&HSTRING::from(path.as_os_str()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
}

/// Closes every program running from the install folder (the app in every
/// user's session, wt, the extractor, a service that would not stop) and
/// waits until each has really exited, so its files can be replaced.
fn close_running(dir: &Path) {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    let folder = dir.to_string_lossy().to_lowercase();
    let folder = folder.trim_end_matches('\\').to_string() + "\\";
    let me = std::process::id();
    let mut closing = Vec::new();
    // SAFETY: the snapshot and process handles are checked and closed below;
    // the buffers are sized as passed.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let pid = entry.th32ProcessID;
            if pid != me && pid != 0 {
                let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE;
                if let Ok(h) = OpenProcess(access, false, pid) {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    let path = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len)
                        .ok()
                        .map(|()| String::from_utf16_lossy(&buf[..len as usize]).to_lowercase());
                    // not the uninstaller itself: run without elevation, it waits
                    // for its elevated copy (this process) and says "removed"
                    let ours = path.is_some_and(|p| p.starts_with(&folder) && !p.ends_with(r"\uninstall.exe"));
                    if ours && TerminateProcess(h, 1).is_ok() {
                        closing.push(h);
                    } else {
                        let _ = CloseHandle(h);
                    }
                }
            }
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        for h in closing {
            WaitForSingleObject(h, 10_000);
            let _ = CloseHandle(h);
        }
    }
}

fn run(exe: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(exe)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("{} {}: {msg}", exe.file_name().unwrap_or_default().to_string_lossy(), args.join(" ")))
    }
}

fn service_exists() -> bool {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Stops the service (it saves its index first) and waits for it.
fn stop_service() {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return };
    let Ok(s) = m.open_service(SERVICE, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) else { return };
    let _ = s.stop();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        match s.query_status() {
            Ok(st) if st.current_state == ServiceState::Stopped => break,
            Err(_) => break,
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn shortcut(link: &Path, target: &Path) -> Result<(), String> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    // SAFETY: standard COM shell-link calls on objects we own.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        sl.SetPath(&HSTRING::from(target.as_os_str())).map_err(|e| e.to_string())?;
        sl.SetDescription(w!("Instant file search")).map_err(|e| e.to_string())?;
        if let Some(dir) = target.parent() {
            let _ = sl.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()));
        }
        let pf: IPersistFile = sl.cast().map_err(|e| e.to_string())?;
        pf.Save(&HSTRING::from(link.as_os_str()), true).map_err(|e| format!("cannot create the Start menu shortcut: {e}"))
    }
}

/// The entry in Settings → Apps (and Control Panel → Programs).
fn register_uninstall(dir: &Path) -> Result<(), String> {
    // the programs unpacked, plus the uninstaller (this installer)
    let me = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len() / 1024);
    let size_kb = (env!("WHERETI_INSTALLED_KB").parse::<u64>().unwrap_or(0) + me).min(u32::MAX as u64) as u32;
    let uninstall = format!("\"{}\" /uninstall", dir.join("uninstall.exe").display());
    let strings = [
        ("DisplayName", "whereti".to_string()),
        ("DisplayVersion", VERSION.to_string()),
        ("Publisher", "Software Hive S.R.L.".to_string()),
        ("InstallLocation", dir.display().to_string()),
        ("DisplayIcon", format!("\"{}\",0", dir.join("whereti.exe").display())),
        ("UninstallString", uninstall.clone()),
        ("QuietUninstallString", format!("{uninstall} /S")),
        ("URLInfoAbout", "https://whereti.com/".to_string()),
        ("HelpLink", "https://whereti.com/faq/".to_string()),
        ("URLUpdateInfo", "https://whereti.com/download/".to_string()),
    ];
    let dwords = [("EstimatedSize", size_kb), ("NoModify", 1), ("NoRepair", 1)];
    // SAFETY: a key we create and close; values are NUL-terminated UTF-16 / u32.
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            UNINSTALL_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_WOW64_64KEY,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(|e| format!("cannot register whereti in Settings → Apps: {e}"))?;
        for (name, value) in strings {
            let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            let _ = RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(bytes));
        }
        for (name, value) in dwords {
            let _ = RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&value.to_le_bytes()));
        }
        let _ = RegCloseKey(key);
    }
    Ok(())
}

// ---------------------------------------------------------------- uninstall (elevated)

pub fn uninstall(log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let dir = install_dir();
    log("Removing the search service…");
    stop_service();
    let service = dir.join("whereti-service.exe");
    if service_exists() && service.is_file() {
        run(&service, &["uninstall"])?;
    }
    close_running(&dir);
    unblock_reader_network();

    log("Removing the files…");
    let me = std::env::current_exe().ok();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if Some(&p) != me.as_ref() {
            let _ = std::fs::remove_file(&p);
        }
    }
    let _ = std::fs::remove_file(start_menu_link());
    // the index is rebuilt from the disk in seconds: nothing of value is lost
    let _ = std::fs::remove_dir_all(data_dir());
    // SAFETY: deleting our own key.
    unsafe {
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, UNINSTALL_KEY);
    }
    // The uninstaller removes itself and the folder once it exits: its
    // window may stay open a while (it says "removed"), so try for 2 minutes.
    // Programs by their full path, run from System32: nothing is looked up
    // in a folder a user could write to.
    let sys = system32();
    let exe = dir.join("uninstall.exe");
    let script = format!(
        "for /l %i in (1,1,60) do @if exist \"{exe}\" (del /f /q \"{exe}\" >nul 2>&1 & \"{ping}\" 127.0.0.1 -n 3 >nul) & rmdir /s /q \"{dir}\"",
        exe = exe.display(),
        ping = sys.join("PING.EXE").display(),
        dir = dir.display()
    );
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new(sys.join("cmd.exe"))
            .arg("/c")
            // as written: cmd does not read the \" escapes Rust would add
            .raw_arg(&script)
            .current_dir(&sys)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    Ok(())
}

// ---------------------------------------------------------------- per user (not elevated)

pub fn set_autostart(on: bool) {
    // SAFETY: valid key/value names; data is a NUL-terminated UTF-16 string.
    unsafe {
        if on {
            let exe = install_dir().join("whereti.exe");
            let cmd: Vec<u16> = format!("\"{}\" --hidden", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("whereti"), REG_SZ.0, Some(cmd.as_ptr() as *const _), (cmd.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("whereti"));
        }
    }
}

/// Starts the installed app as the current (not elevated) user.
pub fn launch_app() {
    let _ = Command::new(install_dir().join("whereti.exe")).arg("--welcome").spawn();
}

/// Opens whereti as usual, without the welcome page (after an update).
pub fn open_app() {
    let _ = Command::new(install_dir().join("whereti.exe")).spawn();
}

// ---------------------------------------------------------------- elevation

pub fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: querying our own process token.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0;
        let ok = GetTokenInformation(token, TokenElevation, Some(&mut e as *mut _ as *mut _), size_of::<TOKEN_ELEVATION>() as u32, &mut len)
            .is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// Runs this installer again with administrator rights (one UAC prompt),
/// waits for it, and returns what it reported.
pub fn run_elevated(args: &str) -> Result<(), String> {
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let report = std::env::temp_dir().join(format!("whereti-setup-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&report);
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let params = HSTRING::from(format!("{args} --report \"{}\"", report.display()));
    let file = HSTRING::from(me.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: valid structure; the process handle is waited on and closed.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(if e.code().0 as u32 == 0x8007_04C7 {
                "cancelled".to_string() // the UAC prompt was declined
            } else {
                e.message()
            });
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        let msg = std::fs::read_to_string(&report).unwrap_or_default();
        let _ = std::fs::remove_file(&report);
        if code == 0 {
            Ok(())
        } else {
            Err(if msg.is_empty() { format!("the installer stopped (code {code})") } else { msg })
        }
    }
}

/// The user's settings (`%APPDATA%\whereti`). Runs in the user's own,
/// non-elevated part: the elevated one may be another account's profile.
pub fn remove_user_settings() {
    // settings, license, history; and the cached listing of network folders
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let _ = std::fs::remove_dir_all(PathBuf::from(base).join("whereti"));
        }
    }
    set_autostart(false);
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_packed_program_unpacks_whole() {
        let mut total = 0u64;
        for (name, packed) in super::PAYLOAD {
            let bytes = super::unpack(packed).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&bytes[..2], b"MZ", "{name} is not a Windows program");
            assert!(bytes.len() > packed.len(), "{name} was not compressed");
            total += bytes.len() as u64;
        }
        assert_eq!(total / 1024, env!("WHERETI_INSTALLED_KB").parse::<u64>().unwrap());
    }
}
