//! Updates (PLAN.md §40), the way SilentSilo does them.
//!
//! - **At most one check a day, for real.** The time of a check is written to
//!   disk *before* the request goes out. A restart, a crash, waking from sleep
//!   or a failed request never leads to a second one; a check that fails waits
//!   for the next day like any other. If the clock goes back, the wait starts
//!   again from now. Only the search window checks, and it runs once.
//! - **What it sends:** `GET https://releases.whereti.com/windows/{arch}/{version}`
//!   and nothing else: no identifier, no cookies (switched off in WinHTTP).
//! - **Installing needs a click.** The installer comes from whereti's GitHub
//!   releases only, and runs only if its SHA-256 matches the manifest and it
//!   carries a valid Authenticode signature from Software Hive S.R.L. So the
//!   endpoint serving wrong data can break updating, never install code.
//! - Settings can turn the check off; then nothing is ever sent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use parking_lot::Mutex;

pub const ENDPOINT: &str = "https://releases.whereti.com";
/// Installers are only ever downloaded from here.
const DOWNLOADS: &str = "https://github.com/whereti/app/releases/download/";
const PUBLISHER: &str = "Software Hive S.R.L.";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY_SECS: u64 = 24 * 3600;
/// How often the window looks at the day gate (a file read, nothing sent).
const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
const MAX_INSTALLER: u64 = 200 << 20;
/// "Later" on the update banner hides it this long (Settings, About still
/// shows the update).
const LATER_SECS: u64 = 3 * DAY_SECS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Idle,
    Offered(Offer),
    Downloading { version: String, done: u64, total: u64 },
    /// The installer is running (Windows asks for permission); whereti closes.
    Installing,
    Failed(String),
}

pub struct Updater {
    phase: Arc<Mutex<Phase>>,
    next_look: Instant,
    /// "Later" on the banner: for which version, and when (kept in memory,
    /// written to update.txt when chosen).
    later: Option<(String, u64)>,
}

fn state_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("update.txt")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What `update.txt` holds: when the last check went out, and an update it
/// found (so a restart does not need another check to show it again).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Saved {
    last_check: Option<u64>,
    offer: Option<Offer>,
    /// The banner was put off: for which version, and when.
    later: Option<(String, u64)>,
}

impl Saved {
    fn load(path: &Path) -> Saved {
        let mut s = Saved::default();
        let Ok(text) = std::fs::read_to_string(path) else { return s };
        let (mut version, mut url, mut sha256, mut notes) = (None, None, None, String::new());
        for line in text.lines() {
            match line.split_once('=') {
                Some(("last_check", v)) => s.last_check = v.trim().parse().ok(),
                Some(("offer_version", v)) => version = Some(v.trim().to_string()),
                Some(("offer_url", v)) => url = Some(v.trim().to_string()),
                Some(("offer_sha256", v)) => sha256 = Some(v.trim().to_string()),
                Some(("offer_notes", v)) => notes = v.trim().replace("\\n", "\n"),
                Some(("later", v)) => {
                    s.later = v.trim().split_once('@').and_then(|(ver, t)| Some((ver.to_string(), t.parse().ok()?)))
                }
                _ => {}
            }
        }
        if let (Some(version), Some(url), Some(sha256)) = (version, url, sha256) {
            s.offer = Some(Offer { version, notes, url, sha256 });
        }
        s
    }

    /// Written to a temporary file and renamed: a half-written file never
    /// replaces a good one. Returns false if it could not be written.
    fn save(&self, path: &Path) -> bool {
        let mut text = String::new();
        if let Some(t) = self.last_check {
            text.push_str(&format!("last_check={t}\n"));
        }
        if let Some(o) = &self.offer {
            text.push_str(&format!(
                "offer_version={}\noffer_url={}\noffer_sha256={}\noffer_notes={}\n",
                o.version,
                o.url,
                o.sha256,
                o.notes.replace('\n', "\\n")
            ));
        }
        if let Some((v, t)) = &self.later {
            text.push_str(&format!("later={v}@{t}\n"));
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let tmp = path.with_extension("new");
        let ok = std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok();
        ok && Saved::load(path) == *self
    }
}

/// Whether a check may go out now, given the last one. A last check in the
/// future means the clock went back: not due, and the wait restarts.
fn due(last: Option<u64>, now: u64) -> Result<bool, u64> {
    match last {
        None => Ok(true),
        Some(t) if t > now => Err(now),
        Some(t) => Ok(now - t >= DAY_SECS),
    }
}

impl Updater {
    pub fn new() -> Updater {
        let saved = Saved::load(&state_file());
        let phase = match &saved.offer {
            Some(o) if newer(&o.version, VERSION) => Phase::Offered(o.clone()),
            _ => Phase::Idle,
        };
        // the first look waits a minute, so a check never slows the start
        Updater { phase: Arc::new(Mutex::new(phase)), next_look: Instant::now() + Duration::from_secs(60), later: saved.later }
    }

    pub fn phase(&self) -> Phase {
        self.phase.lock().clone()
    }

    /// Whether the big banner shows: always while downloading, installing or
    /// after a failure; for an offer, unless "Later" was chosen for this
    /// version less than three days ago.
    pub fn banner(&self) -> bool {
        match self.phase() {
            Phase::Idle => false,
            Phase::Offered(o) => match &self.later {
                Some((v, t)) if *v == o.version => now_secs().saturating_sub(*t) >= LATER_SECS || *t > now_secs(),
                _ => true,
            },
            _ => true,
        }
    }

    /// "Later": the banner goes away for three days, for this version only.
    pub fn later(&mut self) {
        if !matches!(self.phase(), Phase::Offered(_)) {
            self.retry(); // after a failure: back to the offer
        }
        if let Phase::Offered(o) = self.phase() {
            self.later = Some((o.version, now_secs()));
            let path = state_file();
            let mut saved = Saved::load(&path);
            saved.later = self.later.clone();
            saved.save(&path);
        }
    }

    /// The last check, as Unix seconds.
    pub fn last_check(&self) -> Option<u64> {
        Saved::load(&state_file()).last_check
    }

    /// Called often; does real work only when a check is due.
    pub fn tick(&mut self, enabled: bool, ctx: &egui::Context) {
        if !enabled || Instant::now() < self.next_look {
            return;
        }
        self.next_look = Instant::now() + LOOK_EVERY;
        // an offer is looked at again each day too: the server may have
        // withdrawn it, or have a newer one
        if !matches!(*self.phase.lock(), Phase::Idle | Phase::Failed(_) | Phase::Offered(_)) {
            return;
        }
        let path = state_file();
        let mut saved = Saved::load(&path);
        let now = now_secs();
        match due(saved.last_check, now) {
            Ok(false) => return,
            Err(reset) => {
                saved.last_check = Some(reset);
                saved.save(&path);
                return;
            }
            Ok(true) => {}
        }
        // Claim the day first. If that cannot be written, no request goes
        // out: one check a day must hold even when the disk is full.
        saved.last_check = Some(now);
        if !saved.save(&path) {
            return;
        }
        let (phase, ctx) = (self.phase.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            // a failed check is silent and waits for tomorrow
            let Ok(answer) = check() else { return };
            let mut saved = Saved::load(&path);
            saved.offer = answer.clone();
            saved.save(&path);
            let mut phase = phase.lock();
            // never while one is being downloaded or installed
            if matches!(*phase, Phase::Idle | Phase::Failed(_) | Phase::Offered(_)) {
                *phase = match answer {
                    Some(offer) => Phase::Offered(offer),
                    None => Phase::Idle,
                };
                ctx.request_repaint();
            }
        });
    }

    /// Downloads, verifies and runs the installer for the offered version.
    pub fn install(&self, ctx: &egui::Context) {
        let Phase::Offered(offer) = self.phase() else { return };
        *self.phase.lock() = Phase::Downloading { version: offer.version.clone(), done: 0, total: 0 };
        let (phase, ctx) = (self.phase.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("update-install".into()).spawn(move || {
            let result = download_and_run(&offer, &mut |done, total| {
                *phase.lock() = Phase::Downloading { version: offer.version.clone(), done, total };
                ctx.request_repaint();
            });
            *phase.lock() = match result {
                Ok(()) => Phase::Installing,
                Err(e) => Phase::Failed(e),
            };
            ctx.request_repaint();
        });
    }

    /// Back to the offer after a failure, so the button shows again.
    pub fn retry(&self) {
        let saved = Saved::load(&state_file());
        *self.phase.lock() = match saved.offer {
            Some(o) if newer(&o.version, VERSION) => Phase::Offered(o),
            _ => Phase::Idle,
        };
    }
}

/// "whereti/0.0.14": the only thing a request says about the PC.
fn agent() -> String {
    format!("whereti/{VERSION}")
}

fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    }
}

/// `a` is a newer version than `b` (x.y.z, a release beats its prereleases).
pub fn newer(a: &str, b: &str) -> bool {
    fn parse(v: &str) -> Option<([u64; 3], Option<String>)> {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (v, None),
        };
        let mut n = core.split('.').map(|p| p.parse::<u64>().ok());
        Some(([n.next()??, n.next()??, n.next()??], pre))
    }
    let (Some((ca, pa)), Some((cb, pb))) = (parse(a), parse(b)) else { return false };
    if ca != cb {
        return ca > cb;
    }
    match (pa, pb) {
        (None, Some(_)) => true,
        (Some(x), Some(y)) => pre_cmp(&x, &y) == std::cmp::Ordering::Greater,
        _ => false,
    }
}

/// Prerelease labels part by part, numbers as numbers (`rc.10` after `rc.9`).
fn pre_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut xs = a.split('.');
    let mut ys = b.split('.');
    loop {
        let o = match (xs.next(), ys.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(m), Ok(n)) => m.cmp(&n),
                (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                _ => x.cmp(y),
            },
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
}

/// The one request: `None` when whereti is current.
fn check() -> Result<Option<Offer>, String> {
    let url = format!("{ENDPOINT}/windows/{}/{VERSION}", arch());
    let (status, body) = whereti_net::http_get(&url, &agent(), 64 * 1024, &mut |_, _| {})?;
    match status {
        204 => Ok(None),
        200 => parse_offer(&body),
        s => Err(format!("the update server answered {s}")),
    }
}

fn parse_offer(body: &[u8]) -> Result<Option<Offer>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let version = v["version"].as_str().unwrap_or("").to_string();
    if !newer(&version, VERSION) {
        return Ok(None);
    }
    let entry = &v["platforms"][format!("windows-{}", arch())];
    let url = entry["url"].as_str().unwrap_or("").to_string();
    let sha256 = entry["sha256"].as_str().unwrap_or("").to_ascii_lowercase();
    if !url.starts_with(DOWNLOADS) || sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the update manifest is not valid".into());
    }
    Ok(Some(Offer { version, notes: v["notes"].as_str().unwrap_or("").to_string(), url, sha256 }))
}

fn download_and_run(offer: &Offer, progress: &mut dyn FnMut(u64, u64)) -> Result<(), String> {
    use crate::i18n::tr;
    let (status, bytes) = whereti_net::http_get(&offer.url, &agent(), MAX_INSTALLER, progress)?;
    if status != 200 {
        return Err(crate::trf!("The download failed (error {status}).", status));
    }
    if whereti_net::sha256_hex(&bytes)? != offer.sha256 {
        return Err(tr("The download is damaged. Try again later.").into());
    }
    // The installer says which version it is: it must be the one offered
    // (a lying update server cannot pass an older, signed installer off as new).
    if installer_version(&bytes).as_deref() != Some(offer.version.as_str()) {
        return Err(tr("The download is damaged. Try again later.").into());
    }
    let dir = std::env::temp_dir().join("whereti-update");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("whereti-setup-{}.exe", offer.version.replace(['\\', '/', ':'], "")));
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    // From here until the installer has started, nobody else may change the
    // file; and what is on disk is checked again through that same handle.
    let held = hold(&file, &offer.sha256)?;
    if let Err(e) = verify_publisher(&file) {
        drop(held);
        let _ = std::fs::remove_file(&file);
        return Err(crate::trf!("The update is not signed by {publisher}: {e}", publisher = PUBLISHER, e));
    }
    let result = run_installer(&file);
    drop(held);
    result
}

/// The version an installer carries (`WHERETI-SETUP-VERSION=x.y.z`).
fn installer_version(bytes: &[u8]) -> Option<String> {
    let mark = b"WHERETI-SETUP-VERSION=";
    let at = bytes.windows(mark.len()).position(|w| w == mark)? + mark.len();
    let end = bytes[at..].iter().take(32).position(|&b| b == 0)? + at;
    let v = std::str::from_utf8(&bytes[at..end]).ok()?;
    v.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b.is_ascii_alphanumeric()).then(|| v.to_string())
}

/// Opens the saved installer so that no one can write or delete it while
/// it is held, and checks that what is on disk is what was downloaded.
fn hold(file: &Path, sha256: &str) -> Result<std::fs::File, String> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    let mut f = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(file).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if whereti_net::sha256_hex(&bytes)? != sha256 {
        return Err(crate::i18n::tr("The download is damaged. Try again later.").into());
    }
    Ok(f)
}

/// Starts `whereti-setup.exe /S /open`: it asks Windows for permission,
/// replaces whereti (closing this window) and opens the new version.
fn run_installer(file: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let file_w: Vec<u16> = file.as_os_str().encode_wide_nul();
    let args: Vec<u16> = "/S /open".encode_utf16().chain([0]).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
    // SAFETY: zeroed struct with its size set; strings outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file_w.as_ptr();
    info.lpParameters = args.as_ptr();
    // SAFETY: valid struct.
    if unsafe { ShellExecuteExW(&mut info) } == 0 || info.hProcess.is_null() {
        return Err(crate::i18n::tr("The installer could not be started.").into());
    }
    let mut code = 0u32;
    // SAFETY: process handle from ShellExecuteExW. On success the installer
    // closes this app before it exits, so this returns only on failure.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        windows_sys::Win32::Foundation::CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(())
    } else {
        Err(crate::i18n::tr("The update was cancelled or did not finish.").into())
    }
}

trait EncodeWide {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl EncodeWide for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain([0]).collect()
    }
}


/// The company's registration number (CUI) and country, in the signing
/// certificate's subject: a name alone could be any "Software Hive S.R.L.".
const PUBLISHER_SERIAL: &str = "54366095";
const PUBLISHER_COUNTRY: &str = "RO";

/// A valid Authenticode signature of Software Hive S.R.L. (name, CUI and
/// country), from a certificate not revoked. When the revocation lists
/// cannot be reached (an offline network), the signature is checked without
/// them; a certificate known to be revoked is refused.
fn verify_publisher(file: &Path) -> Result<(), String> {
    const CERT_E_REVOCATION_FAILURE: i32 = 0x800B_010E_u32 as i32;
    const CRYPT_E_REVOCATION_OFFLINE: i32 = 0x8009_2013_u32 as i32;
    const CRYPT_E_NO_REVOCATION_CHECK: i32 = 0x8009_2012_u32 as i32;
    match verify_signature(file, true) {
        Err((code, _)) if [CERT_E_REVOCATION_FAILURE, CRYPT_E_REVOCATION_OFFLINE, CRYPT_E_NO_REVOCATION_CHECK].contains(&code) => {
            verify_signature(file, false).map_err(|(_, e)| e)
        }
        other => other.map_err(|(_, e)| e),
    }
}

/// One WinVerifyTrust pass; `revocation`: also check the chain against the
/// certificate authority's revocation lists.
fn verify_signature(file: &Path, revocation: bool) -> Result<(), (i32, String)> {
    use windows_sys::Win32::Security::Cryptography::{CertGetNameStringW, CERT_NAME_ATTR_TYPE, CERT_NAME_SIMPLE_DISPLAY_TYPE};
    use windows_sys::Win32::Security::WinTrust::{
        WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2,
        WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE,
        WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT, WTD_REVOKE_NONE, WTD_REVOKE_WHOLECHAIN,
        WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };
    let path = file.as_os_str().encode_wide_nul();
    // SAFETY: zeroed structs with their sizes set; the path outlives the calls;
    // the state is closed before returning.
    unsafe {
        let mut fi: WINTRUST_FILE_INFO = std::mem::zeroed();
        fi.cbStruct = std::mem::size_of::<WINTRUST_FILE_INFO>() as u32;
        fi.pcwszFilePath = path.as_ptr();
        let mut data: WINTRUST_DATA = std::mem::zeroed();
        data.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
        data.dwUIChoice = WTD_UI_NONE;
        data.fdwRevocationChecks = if revocation { WTD_REVOKE_WHOLECHAIN } else { WTD_REVOKE_NONE };
        data.dwUnionChoice = WTD_CHOICE_FILE;
        data.Anonymous = WINTRUST_DATA_0 { pFile: &mut fi };
        data.dwStateAction = WTD_STATEACTION_VERIFY;
        data.dwProvFlags = if revocation { WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT } else { WTD_CACHE_ONLY_URL_RETRIEVAL };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut _);
        let mut signer = String::new();
        let (mut serial, mut country) = (String::new(), String::new());
        if status == 0 {
            let prov = WTHelperProvDataFromStateData(data.hWVTStateData);
            let sgnr = if prov.is_null() { std::ptr::null_mut() } else { WTHelperGetProvSignerFromChain(prov, 0, 0, 0) };
            if !sgnr.is_null() && (*sgnr).csCertChain > 0 && !(*sgnr).pasCertChain.is_null() {
                let cert = (*(*sgnr).pasCertChain).pCert;
                let mut buf = [0u16; 256];
                let n = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, std::ptr::null(), buf.as_mut_ptr(), 256);
                signer = String::from_utf16_lossy(&buf[..n.saturating_sub(1) as usize]);
                // subject attributes by OID: serialNumber (2.5.4.5), country (2.5.4.6)
                let attr = |oid: &[u8]| {
                    let mut buf = [0u16; 128];
                    let n = CertGetNameStringW(cert, CERT_NAME_ATTR_TYPE, 0, oid.as_ptr().cast(), buf.as_mut_ptr(), 128);
                    String::from_utf16_lossy(&buf[..n.saturating_sub(1) as usize])
                };
                serial = attr(b"2.5.4.5\0");
                country = attr(b"2.5.4.6\0");
            }
        }
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(std::ptr::null_mut(), &mut action, &mut data as *mut _ as *mut _);
        if status != 0 {
            return Err((status, format!("no valid signature ({:#x})", status as u32)));
        }
        if !signer.trim().eq_ignore_ascii_case(PUBLISHER)
            || serial.trim() != PUBLISHER_SERIAL
            || !country.trim().eq_ignore_ascii_case(PUBLISHER_COUNTRY)
        {
            return Err((0, format!("signed by {signer} ({serial}, {country})")));
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_check_a_day() {
        let day = DAY_SECS;
        assert_eq!(due(None, 1000), Ok(true));
        assert_eq!(due(Some(1000), 1000 + day - 1), Ok(false));
        assert_eq!(due(Some(1000), 1000 + day), Ok(true));
        // the clock went back: wait a full day from now
        assert_eq!(due(Some(5000), 1000), Err(1000));
    }

    #[test]
    fn the_day_is_claimed_on_disk_before_asking() {
        let path = std::env::temp_dir().join(format!("whereti-update-{}.txt", std::process::id()));
        let s = Saved {
            later: Some(("0.2.0".into(), 456)),
            last_check: Some(123),
            offer: Some(Offer {
                version: "0.2.0".into(),
                notes: "line 1\nline 2".into(),
                url: format!("{DOWNLOADS}v0.2.0/whereti-setup-0.2.0.exe"),
                sha256: "ab".repeat(32),
            }),
        };
        assert!(s.save(&path));
        assert_eq!(Saved::load(&path), s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(newer("0.2.0", "0.2.0-rc.1"));
        assert!(!newer("0.2.0-rc.1", "0.2.0"));
        assert!(newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(newer("1.0.0-rc.1", "1.0.0-beta.2"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("garbage", "0.1.0"));
    }

    #[test]
    fn only_github_installers_with_a_hash_are_accepted() {
        let ok = format!(
            r#"{{"version":"99.0.0","notes":"n","platforms":{{"windows-{}":{{"url":"{DOWNLOADS}v99.0.0/x.exe","sha256":"{}"}}}}}}"#,
            arch(),
            "AB".repeat(32)
        );
        let o = parse_offer(ok.as_bytes()).unwrap().unwrap();
        assert_eq!(o.version, "99.0.0");
        assert_eq!(o.sha256, "ab".repeat(32));
        let elsewhere = ok.replace(DOWNLOADS, "https://evil.example/");
        assert!(parse_offer(elsewhere.as_bytes()).is_err());
        let old = ok.replace("99.0.0", "0.0.0");
        assert_eq!(parse_offer(old.as_bytes()).unwrap(), None);
    }

    #[test]
    fn hashes_like_sha256() {
        assert_eq!(whereti_net::sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn a_file_signed_by_software_hive_passes() {
        // SilentSilo is signed with the same certificate; skipped where it is not installed
        let Some(local) = std::env::var_os("LOCALAPPDATA") else { return };
        let exe = PathBuf::from(local).join(r"SilentSilo\silentsilo.exe");
        if exe.exists() {
            assert_eq!(verify_publisher(&exe), Ok(()));
        }
        // signed, but by someone else
        let other = PathBuf::from(r"C:\Windows\explorer.exe");
        if other.exists() {
            assert!(verify_publisher(&other).is_err());
        }
    }

    #[test]
    fn unsigned_files_are_refused() {
        let path = std::env::temp_dir().join(format!("whereti-unsigned-{}.exe", std::process::id()));
        std::fs::write(&path, b"MZ not really a program").unwrap();
        assert!(verify_publisher(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;

    /// The signed installer of a release passes (name, CUI, country,
    /// revocation); an unsigned program does not. Needs dist\ from a
    /// signed build; skipped without it.
    #[test]
    fn only_the_company_s_signature_passes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist");
        let signed = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("whereti-setup-")));
        let Some(signed) = signed else { return };
        assert_eq!(verify_publisher(&signed), Ok(()));
        let unsigned = std::env::current_exe().unwrap();
        assert!(verify_publisher(&unsigned).is_err());
    }

    #[test]
    fn the_installer_says_its_version() {
        let mut bytes = b"MZ....".to_vec();
        bytes.extend_from_slice(b"WHERETI-SETUP-VERSION=1.0.0\0rest");
        assert_eq!(installer_version(&bytes).as_deref(), Some("1.0.0"));
        assert_eq!(installer_version(b"MZ no mark"), None);
    }
}
