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
fn block_reader_network(dir: &Path) -> Result<(), String> {
    let netsh = system32().join("netsh.exe");
    // none to delete on a first install: that answer does not matter
    let _ = hidden(&netsh).args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
    let program = format!("program={}", dir.join("whereti-extract.exe").display());
    let out = hidden(&netsh)
        .args(["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}"), "dir=out", "action=block", &program, "enable=yes", "profile=any"])
        .output()
        .map_err(|e| format!("cannot run {}: {e}", netsh.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        // netsh says what went wrong on its standard output
        let msg = format!("{} {}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim());
        Err(format!("cannot add the Windows Firewall rule that keeps the document reader off the network: {}", msg.trim()))
    }
}

fn unblock_reader_network() {
    let netsh = system32().join("netsh.exe");
    let _ = hidden(&netsh).args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
}

/// A console program, run without a console window flashing up.
fn hidden(exe: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW);
    c
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

/// Programs older versions installed that this one no longer carries:
/// removed on an update. Only names whereti itself put there (none yet).
const RETIRED: [&str; 0] = [];

pub fn install(log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let dir = install_dir();
    // found now, before anything is stopped
    if !is_uninstaller(&dir)? {
        own_file()?;
    }
    let _one = one_at_a_time()?;
    log("Stopping the running whereti…");
    stop_service();
    close_running(&dir);
    remove_leftovers(&dir);

    if let Err(e) = put_in_place(&dir, log) {
        // never leave whereti without its service: start the one there is
        if service_exists() {
            let _ = start_service();
        }
        return Err(e);
    }

    log("Starting the search service…");
    start_service()?;

    log("Adding whereti to the Start menu…");
    shortcut(&start_menu_link(), &dir.join("whereti.exe"))?;
    Ok(())
}

/// The files, the entry in Settings > Apps, the firewall rule and the
/// service's registration: everything but starting it.
fn put_in_place(dir: &Path, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    log("Copying the files…");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (name, packed) in PAYLOAD {
        let bytes = unpack(packed).map_err(|e| format!("the installer is damaged ({name}: {e})"))?;
        write_file(&dir.join(name), &bytes)?;
    }
    for name in RETIRED {
        let _ = std::fs::remove_file(dir.join(name));
    }
    if !is_uninstaller(dir)? {
        // through the handle held since the start, never by the path again
        let mut own = own_file()?;
        let mut bytes = Vec::new();
        std::io::Seek::seek(&mut own, std::io::SeekFrom::Start(0))
            .and_then(|_| std::io::Read::read_to_end(&mut own, &mut bytes))
            .map_err(|e| format!("cannot read the installer: {e}"))?;
        write_file(&dir.join("uninstall.exe"), &bytes)?;
    }
    // listed in Settings > Apps before the firewall rule and the service,
    // which can still fail: the files on disk can be removed from there
    register_uninstall(dir)?;

    block_reader_network(dir)?;

    log("Installing the search service…");
    let service = dir.join("whereti-service.exe");
    if service_exists() {
        configure_service(&service)
    } else {
        run(&service, &["install"])
    }
}

/// Whether this is the installed uninstall.exe itself (run again to
/// reinstall): then it is not copied over itself.
fn is_uninstaller(dir: &Path) -> Result<bool, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(me.as_os_str().eq_ignore_ascii_case(dir.join("uninstall.exe").as_os_str()))
}

/// This installer's own file, opened by `hold_self`.
static OWN_FILE: std::sync::OnceLock<Result<std::fs::File, String>> = std::sync::OnceLock::new();

/// Opens this installer's own file so that no one can change, rename or
/// delete it while the install runs (read sharing only). The copy of it put
/// in the install folder (uninstall.exe) is read through this handle, never
/// again by its path, which a user could swap meanwhile. The elevated
/// install calls it before anything else.
pub fn hold_self() {
    let _ = own_file();
}

fn own_file() -> Result<&'static std::fs::File, String> {
    OWN_FILE
        .get_or_init(|| {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x1;
            let me = std::env::current_exe().map_err(|e| format!("cannot read the installer: {e}"))?;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&me)
                .map_err(|e| format!("cannot read the installer: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// One installer at a time (a double click, an update while the window is
/// open): a second one waits for the first to finish.
fn one_at_a_time() -> Result<OneAtATime, String> {
    use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
    // SAFETY: a named mutex we create or open, closed by OneAtATime or here.
    unsafe {
        // cannot be made: go on rather than refuse to install
        let Ok(h) = CreateMutexW(None, false, w!(r"Global\whereti-setup")) else { return Ok(OneAtATime(None)) };
        match WaitForSingleObject(h, 120_000) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(OneAtATime(Some(h))),
            _ => {
                let _ = CloseHandle(h);
                Err("another whereti setup is still running: let it finish, then try again".into())
            }
        }
    }
}

struct OneAtATime(Option<HANDLE>);

impl Drop for OneAtATime {
    fn drop(&mut self) {
        use windows::Win32::System::Threading::ReleaseMutex;
        if let Some(h) = self.0 {
            // SAFETY: the mutex this thread acquired in one_at_a_time.
            unsafe {
                let _ = ReleaseMutex(h);
                let _ = CloseHandle(h);
            }
        }
    }
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
    let mut attempt = 0;
    let err = loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 7 => break e,
            // a process that was just stopped may hold the file a moment longer
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    };
    let fail = |what: &str, e: std::io::Error| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot {what} {}: {e}", path.display())
    };
    // nothing there to move aside (a first install): the rename's own error says why
    if std::fs::symlink_metadata(path).is_err() {
        return Err(fail("write", err));
    }
    let aside = aside_name(path);
    std::fs::rename(path, &aside).map_err(|e| fail("replace", e))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // the old program goes back in its place
        let _ = std::fs::rename(&aside, path);
        return Err(fail("replace", e));
    }
    delete_at_restart(&aside);
    Ok(())
}

/// The ending of a program moved aside by `write_file`.
const OLD: &str = ".old";

/// A name of its own for a file moved aside (`whereti.exe.<time>.old`).
fn aside_name(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    path.with_file_name(format!("{name}.{stamp}{OLD}"))
}

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
    let out = hidden(exe)
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
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return };
    let Ok(s) = m.open_service(SERVICE, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) else { return };
    stop_and_wait(&s);
}

/// Waits up to a minute until the service has stopped. One still starting
/// cannot be told to stop: it is let start first, then stopped. One that
/// does not stop in time is ended by close_running.
fn stop_and_wait(s: &windows_service::service::Service) {
    use windows_service::service::ServiceState;
    let t = Instant::now();
    let mut asked = false;
    while t.elapsed() < Duration::from_secs(60) {
        match s.query_status().map(|st| st.current_state) {
            Ok(ServiceState::Stopped) | Err(_) => return,
            Ok(ServiceState::Running | ServiceState::Paused) if !asked => asked = s.stop().is_ok(),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Starts the service and waits until it runs. One already running was
/// started from files that may have been replaced since (Windows restarts
/// the service a few seconds after close_running ended it), so it is
/// restarted: the version just installed is the one that runs.
fn start_service() -> Result<(), String> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(scm_text)?;
    let s = m
        .open_service(SERVICE, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)
        .map_err(|e| format!("cannot open the search service: {}", scm_text(e)))?;
    stop_and_wait(&s);
    match s.start::<&str>(&[]) {
        Ok(()) => {}
        // started by someone else this very moment, from the files now in place
        Err(e) if scm_code(&e) == Some(ERROR_SERVICE_ALREADY_RUNNING) => return Ok(()),
        Err(e) => return Err(format!("cannot start the search service: {}", scm_text(e))),
    }
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) {
        match s.query_status().map(|st| st.current_state) {
            Ok(ServiceState::Running) => return Ok(()),
            Ok(ServiceState::Stopped) => {
                let log = data_dir().join("service.log");
                return Err(format!("the search service stopped right after it started (see {})", log.display()));
            }
            Err(e) => return Err(format!("cannot start the search service: {}", scm_text(e))),
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    // still starting: Windows goes on with it
    Ok(())
}

/// On an update the service is registered already: its program and its
/// start are set again as a first install sets them, so it runs the
/// program just installed even if an older whereti or someone changed them.
fn configure_service(exe: &Path) -> Result<(), String> {
    use windows_service::service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let fail = |e| format!("cannot set up the search service: {}", scm_text(e));
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(fail)?;
    let s = m.open_service(SERVICE, ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG).map_err(fail)?;
    let config = s.query_config().map_err(fail)?;
    let info = ServiceInfo {
        name: SERVICE.into(),
        display_name: config.display_name,
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec!["run".into()],
        dependencies: vec![],
        // the account stays as `whereti-service install` set it (LocalSystem)
        account_name: None,
        account_password: None,
    };
    s.change_config(&info).map_err(fail)
}

/// Removes the service from Windows, through the Service Control Manager:
/// it works even when whereti-service.exe is already gone.
fn delete_service() -> Result<(), String> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
    let fail = |e| format!("cannot remove the search service: {}", scm_text(e));
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(fail)?;
    let s = match m.open_service(SERVICE, ServiceAccess::DELETE) {
        Ok(s) => s,
        Err(e) if scm_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(e) => return Err(fail(e)),
    };
    match s.delete() {
        Err(e) if scm_code(&e) != Some(ERROR_SERVICE_MARKED_FOR_DELETE) => Err(fail(e)),
        _ => Ok(()),
    }
}

/// The Windows error behind a service call that failed.
fn scm_code(e: &windows_service::Error) -> Option<i32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

/// What went wrong, in Windows' words ("IO error in winapi call" says nothing).
fn scm_text(e: windows_service::Error) -> String {
    match e {
        windows_service::Error::Winapi(io) => io.to_string(),
        e => e.to_string(),
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
    let _one = one_at_a_time()?;
    let dir = install_dir();
    log("Removing the search service…");
    stop_service();
    if let Err(e) = delete_service() {
        // nothing removed yet: whereti goes on working as it was
        let _ = start_service();
        return Err(e);
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
    // What cannot be deleted now (the uninstaller still running, a file held
    // open) is deleted at the next restart, and then the folder if empty.
    // Renamed first, to a name of its own: a whereti installed again before
    // that restart keeps its files.
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if entry.file_name().to_string_lossy().ends_with(OLD) {
            delete_at_restart(&p);
        } else {
            let aside = aside_name(&p);
            if std::fs::rename(&p, &aside).is_ok() {
                delete_at_restart(&aside);
            }
        }
    }
    if dir.is_dir() {
        delete_at_restart(&dir);
    }
    // The uninstaller's window may stay open a while (it says "removed"):
    // the folder goes once it closes, tried for 2 minutes.
    // Programs by their full path, run from System32: nothing is looked up
    // in a folder a user could write to.
    let sys = system32();
    let script = format!(
        "for /l %i in (1,1,60) do @if exist \"{dir}\\\" (rmdir /s /q \"{dir}\" >nul 2>&1 & if exist \"{dir}\\\" \"{ping}\" 127.0.0.1 -n 3 >nul)",
        ping = sys.join("PING.EXE").display(),
        dir = dir.display()
    );
    {
        use std::os::windows::process::CommandExt;
        let _ = hidden(&sys.join("cmd.exe"))
            // no AutoRun commands from the registry, then the script
            .args(["/d", "/c"])
            // as written: cmd does not read the \" escapes Rust would add
            .raw_arg(&script)
            .current_dir(&sys)
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
    start_app("--welcome");
}

/// Opens whereti as usual, without the welcome page (after an update).
pub fn open_app() {
    start_app("");
}

/// whereti never runs with administrator rights: an installer that was
/// itself started elevated has the desktop's Explorer start it, as the
/// signed-in user. If that fails it is not started (the Start menu has it).
fn start_app(args: &str) {
    let exe = install_dir().join("whereti.exe");
    if is_elevated() {
        let _ = shell_execute_as_desktop(&exe, args);
    } else {
        let _ = Command::new(&exe).args(args.split_whitespace()).spawn();
    }
}

/// ShellExecute through the desktop's Explorer (IShellDispatch2), so the
/// program runs with Explorer's rights, not this process's.
fn shell_execute_as_desktop(exe: &Path, args: &str) -> windows::core::Result<()> {
    use std::mem::ManuallyDrop;
    use windows::core::{Interface, BSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IDispatch, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR, VT_I4};
    use windows::Win32::UI::Shell::{
        IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, ShellWindows, CSIDL_DESKTOP,
        SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    fn text(s: &str) -> VARIANT {
        let mut v = VARIANT::default();
        // SAFETY: a VT_BSTR variant owning its string, freed by VariantClear.
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BSTR;
            inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
        }
        v
    }
    fn number(n: i32) -> VARIANT {
        let mut v = VARIANT::default();
        // SAFETY: a VT_I4 variant, nothing to free.
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = n;
        }
        v
    }
    let dir = exe.parent().unwrap_or(exe);
    // SAFETY: COM calls on interfaces we hold; every variant is cleared.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let shell_windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let at = number(CSIDL_DESKTOP as i32);
        let mut hwnd = 0;
        let desktop: IDispatch = shell_windows.FindWindowSW(&at, &VARIANT::default(), SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
        let browser: IShellBrowser = desktop.cast::<IServiceProvider>()?.QueryService(&SID_STopLevelBrowser)?;
        let view: IDispatch = browser.QueryActiveShellView()?.GetItemObject(SVGIO_BACKGROUND)?;
        let shell: IShellDispatch2 = view.cast::<IShellFolderViewDual>()?.Application()?.cast()?;
        let mut params = [text(args), text(&dir.to_string_lossy()), text("open"), number(SW_SHOWNORMAL.0)];
        let result = shell.ShellExecute(&BSTR::from(exe.to_string_lossy().as_ref()), &params[0], &params[1], &params[2], &params[3]);
        for v in &mut params {
            let _ = VariantClear(v);
        }
        result
    }
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
    // installers the updater downloaded (%TEMP%\whereti-update)
    for entry in std::fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == "whereti-update" || name.starts_with("whereti-update-") {
            let p = entry.path();
            let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
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
