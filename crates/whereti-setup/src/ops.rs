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

/// The programs the installer carries, packed at build time (see build.rs).
pub const PAYLOAD: [(&str, &[u8]); 4] = [
    ("whereti.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti.exe"))),
    ("whereti-service.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti-service.exe"))),
    ("whereti-extract.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/whereti-extract.exe"))),
    ("wt.exe", include_bytes!(concat!(env!("WHERETI_PAYLOAD_DIR"), "/wt.exe"))),
];

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\whereti");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const SERVICE: &str = "whereti";

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    base.join("whereti")
}

fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("whereti")
}

fn start_menu_link() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(r"Microsoft\Windows\Start Menu\Programs\whereti.lnk")
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
    kill("whereti.exe");
    kill("whereti-extract.exe");

    log("Copying the files…");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for (name, bytes) in PAYLOAD {
        write_file(&dir.join(name), bytes)?;
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let uninstaller = dir.join("uninstall.exe");
    if me != uninstaller {
        std::fs::copy(&me, &uninstaller).map_err(|e| format!("cannot write the uninstaller: {e}"))?;
    }

    log("Installing the search service…");
    let service = dir.join("whereti-service.exe");
    if !service_exists() {
        run(&service, &["install"])?;
    }
    run(&service, &["start"])?;

    log("Adding whereti to the Start menu…");
    shortcut(&start_menu_link(), &dir.join("whereti.exe"))?;
    register_uninstall(&dir)?;
    Ok(())
}

/// Writes next to the target, then renames: a half-written program is never left behind.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    for attempt in 0..20 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            // a process that was just stopped may hold the file a moment longer
            Err(_) if attempt < 19 => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(format!("cannot replace {}: {e} (is whereti still running?)", path.display())),
        }
    }
    Ok(())
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

fn kill(image: &str) {
    let _ = Command::new("taskkill").args(["/IM", image, "/F"]).output();
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
    let size_kb: u32 = PAYLOAD.iter().map(|(_, b)| b.len() as u32).sum::<u32>() / 1024;
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
    kill("whereti.exe");
    kill("whereti-extract.exe");

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
    // The running uninstaller removes itself and the folder once it exits.
    let script = format!(
        "ping 127.0.0.1 -n 3 > nul & del /f /q \"{}\" & rmdir /s /q \"{}\"",
        dir.join("uninstall.exe").display(),
        dir.display()
    );
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("cmd").args(["/c", &script]).creation_flags(CREATE_NO_WINDOW).spawn();
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
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let _ = std::fs::remove_dir_all(PathBuf::from(appdata).join("whereti"));
    }
}
