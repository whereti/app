//! "Recente": the files the user actually opened, like Explorer's "Recent
//! files" and the Start menu - not every file some program wrote.
//!
//! Windows keeps a shortcut in `%APPDATA%\Microsoft\Windows\Recent` for each
//! file opened from Explorer, Office, the file dialogs and most apps; its
//! time is when it was opened. A game saving into Documents never shows up
//! there. Files downloaded in the last days are added: a new download is
//! often the next thing someone looks for.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use whereti_ipc::Row;
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

const DOWNLOADS_FOR: Duration = Duration::from_secs(3 * 24 * 3600);
/// Newest shortcuts read: enough for a screen of recent files, fast to read.
const MAX_LINKS: usize = 250;

thread_local! {
    /// shortcut -> (its time, target): a shortcut is read once until it changes.
    static TARGETS: std::cell::RefCell<std::collections::HashMap<PathBuf, (SystemTime, Option<PathBuf>)>> =
        Default::default();
}

/// Newest first. Runs on the searcher thread.
pub fn recent_files(limit: usize) -> Vec<Row> {
    // SAFETY: COM on this thread; a second call returns S_FALSE, which is fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    let mut items: Vec<(SystemTime, PathBuf)> = Vec::new();

    // 1. opened files, from Windows' recent shortcuts
    let mut links: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(appdata.join(r"Microsoft\Windows\Recent"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    links.sort_by_key(|l| std::cmp::Reverse(l.0));
    for (opened, link) in links.into_iter().take(MAX_LINKS.min(limit + 50)) {
        let cached = TARGETS.with(|t| t.borrow().get(&link).filter(|c| c.0 == opened).map(|c| c.1.clone()));
        let target = match cached {
            Some(t) => t,
            None => {
                let t = link_target(&link);
                TARGETS.with(|m| m.borrow_mut().insert(link.clone(), (opened, t.clone())));
                t
            }
        };
        if let Some(target) = target {
            items.push((opened, target));
        }
    }

    // 2. fresh downloads
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let downloads = PathBuf::from(home).join("Downloads");
        let since = SystemTime::now() - DOWNLOADS_FOR;
        for e in std::fs::read_dir(downloads).into_iter().flatten().flatten() {
            if let Ok(m) = e.metadata() {
                let t = m.modified().unwrap_or(SystemTime::UNIX_EPOCH).max(m.created().unwrap_or(SystemTime::UNIX_EPOCH));
                if m.is_file() && t >= since && !is_partial_download(&e.path()) {
                    items.push((t, e.path()));
                }
            }
        }
    }

    items.sort_by_key(|i| std::cmp::Reverse(i.0));
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for (time, path) in items {
        if rows.len() >= limit {
            break;
        }
        let key = path.to_string_lossy().to_lowercase();
        if !seen.insert(key.clone()) || is_noise(&key) {
            continue;
        }
        // still there, and a file (folders are in the Recent list too)
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        rows.push(Row {
            path: path.to_string_lossy().into_owned(),
            is_dir: false,
            size: meta.len(),
            modified: minutes_since_1601(time),
            flags: 0,
        });
    }
    rows
}

/// Where a `.lnk` points (unresolved: no disk search for moved targets).
fn link_target(link: &Path) -> Option<PathBuf> {
    // SAFETY: standard COM shell-link calls on an object we own.
    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let pf: IPersistFile = sl.cast().ok()?;
        pf.Load(&HSTRING::from(link.as_os_str()), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        sl.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        if len == 0 {
            return None; // a link to a web page or a shell object
        }
        let raw = String::from_utf16_lossy(&buf[..len]);
        // RAWPATH keeps %VARIABLES%: expand the usual ones
        Some(PathBuf::from(expand_env(&raw)))
    }
}

fn expand_env(s: &str) -> String {
    let mut out = s.to_string();
    for var in ["USERPROFILE", "OneDrive", "APPDATA", "LOCALAPPDATA", "SystemDrive", "PUBLIC"] {
        if let Ok(v) = std::env::var(var) {
            out = out.replace(&format!("%{var}%"), &v);
        }
    }
    out
}

/// Things opened by programs rather than people.
fn is_noise(lower_path: &str) -> bool {
    lower_path.contains(r"\appdata\") || lower_path.contains(r"\temp\") || lower_path.ends_with(".tmp")
}

fn is_partial_download(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "crdownload" | "part" | "partial" | "download" | "tmp")
}

fn minutes_since_1601(t: SystemTime) -> u32 {
    let unix = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    (unix + 134_774 * 1440) as u32
}
