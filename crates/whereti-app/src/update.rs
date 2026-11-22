//! New versions (PLAN.md §40): whereti says one is out, and the user gets it
//! from whereti.com. Nothing is downloaded or installed by the app.
//!
//! - **At most one check a day, for real.** The time of a check is written to
//!   disk *before* the request goes out. A restart, a crash, waking from sleep
//!   or a failed request never leads to a second one; a check that fails waits
//!   for the next day like any other. If the clock goes back, the wait starts
//!   again from now. Only the search window checks, and it runs once.
//! - **What it sends:** `GET https://whereti.com/latest.json`, a public file,
//!   and nothing else: no version, no identifier, no cookies (switched off in
//!   WinHTTP).
//! - **What it trusts:** from that file, only a version number (digits and
//!   dots), one line of text shown as text, and whether the version is
//!   important. The page a click opens is whereti.com's download
//!   (links.rs), written in the app: nothing in the file can change it.
//! - Settings can turn the check off; then nothing is ever sent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use parking_lot::Mutex;

const ENDPOINT: &str = "https://whereti.com/latest.json";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY_SECS: u64 = 24 * 3600;
/// How often the window looks at the day gate (a file read, nothing sent).
const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
/// "In a week" on the banner hides it this long, for that version (Settings,
/// About still shows it). An important version cannot be put off.
const LATER_SECS: u64 = 7 * DAY_SECS;
/// The line shown under the title: at most this many characters.
const MAX_NOTES: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub version: String,
    /// One line about it, shown as plain text; may be empty.
    pub notes: String,
    /// A fix that should not wait a week (security): no "In a week".
    pub important: bool,
}

pub struct Updater {
    offer: Arc<Mutex<Option<Offer>>>,
    next_look: Instant,
    /// "In a week" on the banner: for which version, and when (kept in
    /// memory, written to update.txt when chosen).
    later: Option<(String, u64)>,
    /// When the last check went out, as Unix seconds, kept here so Settings
    /// does not read the file every frame.
    last_check: Option<u64>,
}

fn state_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("update.txt")
}

/// update.txt is read, changed and written again by the window and by the
/// check's thread: one at a time, so neither loses what the other wrote
/// (and they never share the temporary file).
static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_state<T>(f: impl FnOnce(&Path) -> T) -> T {
    let _one_at_a_time = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(&state_file())
}

/// Checking for updates is on (Settings), as `tick` last saw it.
static CHECKS_ON: AtomicBool = AtomicBool::new(true);

/// Hidden in the tray, the window draws nothing and `tick` never runs: this
/// thread wakes it when the day's check is due, so a new version is found
/// without the window being opened. It only reads update.txt (nothing is
/// sent), at most once an hour while no check is due.
pub fn wake_when_due(ctx: egui::Context, enabled: bool) {
    CHECKS_ON.store(enabled, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("update-wake".into()).spawn(move || {
        // after the first look, a minute after the start
        std::thread::sleep(Duration::from_secs(61));
        loop {
            let last = with_state(|path| Saved::load(path).last_check);
            let wait = match due(last, now_secs()) {
                Ok(true) => {
                    if CHECKS_ON.load(Ordering::Relaxed) {
                        ctx.request_repaint();
                    }
                    LOOK_EVERY
                }
                Ok(false) => Duration::from_secs(last.map_or(0, |t| (t + DAY_SECS).saturating_sub(now_secs()))),
                Err(_) => Duration::from_secs(DAY_SECS),
            };
            // looked at again within the hour: the PC may have slept
            std::thread::sleep(wait.clamp(Duration::from_secs(60), Duration::from_secs(3600)));
        }
    });
}

/// A version as releases are numbered: digits and dots ("1.3.0"), or with a
/// prerelease label ("1.3.0-rc.1"). Nothing else gets into a page address.
fn plain_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !v.contains("..")
        && !v.starts_with(['.', '-'])
}

/// An offer from update.txt that is still newer than this version.
fn still_offered(saved: &Saved) -> Option<Offer> {
    saved.offer.clone().filter(|o| plain_version(&o.version) && newer(&o.version, VERSION))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What `update.txt` holds: when the last check went out, and a version it
/// found (so a restart does not need another check to show it again).
/// Lines an earlier version wrote (the installer's address and hash) are
/// read past.
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
        let (mut version, mut notes, mut important) = (None, String::new(), false);
        for line in text.lines() {
            match line.split_once('=') {
                Some(("last_check", v)) => s.last_check = v.trim().parse().ok(),
                Some(("offer_version", v)) => version = Some(v.trim().to_string()),
                Some(("offer_notes", v)) => notes = clean_notes(&v.replace("\\n", "\n")),
                Some(("offer_important", v)) => important = v.trim() == "1",
                Some(("later", v)) => {
                    s.later = v.trim().split_once('@').and_then(|(ver, t)| Some((ver.to_string(), t.parse().ok()?)))
                }
                _ => {}
            }
        }
        if let Some(version) = version {
            s.offer = Some(Offer { version, notes, important });
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
                "offer_version={}\noffer_notes={}\noffer_important={}\n",
                o.version,
                o.notes,
                if o.important { 1 } else { 0 }
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
        let saved = with_state(Saved::load);
        // the first look waits a minute, so a check never slows the start
        Updater {
            offer: Arc::new(Mutex::new(still_offered(&saved))),
            next_look: Instant::now() + Duration::from_secs(60),
            later: saved.later,
            last_check: saved.last_check,
        }
    }

    /// A newer version, if one is out.
    pub fn offer(&self) -> Option<Offer> {
        self.offer.lock().clone()
    }

    /// Whether the banner shows: while checking for updates is on
    /// (`enabled`), unless "In a week" was chosen for this version less than
    /// a week ago. An important version always shows.
    pub fn banner(&self, enabled: bool) -> bool {
        match self.offer() {
            None => false,
            Some(_) if !enabled => false,
            Some(o) if o.important => true,
            Some(o) => match &self.later {
                Some((v, t)) if *v == o.version => now_secs().saturating_sub(*t) >= LATER_SECS || *t > now_secs(),
                _ => true,
            },
        }
    }

    /// "In a week": the banner goes away for seven days, for this version
    /// only (a newer one shows at once).
    pub fn later(&mut self) {
        let Some(o) = self.offer() else { return };
        self.later = Some((o.version, now_secs()));
        let later = self.later.clone();
        with_state(|path| {
            let mut saved = Saved::load(path);
            saved.later = later;
            saved.save(path);
        });
    }

    /// The last check, as Unix seconds.
    pub fn last_check(&self) -> Option<u64> {
        self.last_check
    }

    /// Called often; does real work only when a check is due.
    pub fn tick(&mut self, enabled: bool, ctx: &egui::Context) {
        CHECKS_ON.store(enabled, Ordering::Relaxed);
        if !enabled || Instant::now() < self.next_look {
            return;
        }
        self.next_look = Instant::now() + LOOK_EVERY;
        let now = now_secs();
        let claimed = with_state(|path| {
            let mut saved = Saved::load(path);
            match due(saved.last_check, now) {
                // checked meanwhile by another window of this account
                Ok(false) => {
                    self.last_check = saved.last_check;
                    return false;
                }
                Err(reset) => {
                    saved.last_check = Some(reset);
                    saved.save(path);
                    self.last_check = Some(reset);
                    return false;
                }
                Ok(true) => {}
            }
            // Claim the day first. If that cannot be written, no request goes
            // out: one check a day must hold even when the disk is full.
            saved.last_check = Some(now);
            let saved_ok = saved.save(path);
            if saved_ok {
                self.last_check = Some(now);
            }
            saved_ok
        });
        if !claimed {
            return;
        }
        let (offer, ctx) = (self.offer.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            // a failed check is silent and waits for tomorrow
            let Ok(answer) = check() else { return };
            with_state(|path| {
                let mut saved = Saved::load(path);
                saved.offer = answer.clone();
                saved.save(path);
            });
            *offer.lock() = answer;
            ctx.request_repaint();
        });
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
    // "whereti": the request says nothing about this PC, not even the version
    let (status, body) = whereti_net::http_get(ENDPOINT, "whereti", 64 * 1024, &mut |_, _| {})?;
    match status {
        200 => parse_offer(&body),
        s => Err(format!("whereti.com answered {s}")),
    }
}

/// One line of plain text: no line breaks or control characters, at most
/// `MAX_NOTES` characters.
fn clean_notes(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line: String = line.chars().filter(|c| !c.is_control()).collect();
    match line.char_indices().nth(MAX_NOTES) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line,
    }
}

/// From latest.json: `{"version": "1.3.0", "notes": "…", "important": false}`
/// (other fields, like the installer's address, are not looked at).
fn parse_offer(body: &[u8]) -> Result<Option<Offer>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let version = v["version"].as_str().unwrap_or("").trim().to_string();
    if !plain_version(&version) {
        return Err("latest.json has no valid version".into());
    }
    if !newer(&version, VERSION) {
        return Ok(None);
    }
    Ok(Some(Offer {
        version,
        notes: clean_notes(v["notes"].as_str().unwrap_or("")),
        important: v["important"].as_bool().unwrap_or(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_is_due_once_a_day_and_a_clock_set_back_waits_again() {
        assert_eq!(due(None, 1000), Ok(true));
        assert_eq!(due(Some(1000), 1000 + DAY_SECS - 1), Ok(false));
        assert_eq!(due(Some(1000), 1000 + DAY_SECS), Ok(true));
        assert_eq!(due(Some(5000), 1000), Err(1000));
    }

    #[test]
    fn the_day_is_claimed_on_disk_before_asking() {
        let path = std::env::temp_dir().join(format!("whereti-update-{}.txt", std::process::id()));
        let s = Saved {
            later: Some(("0.2.0".into(), 456)),
            last_check: Some(123),
            offer: Some(Offer { version: "0.2.0".into(), notes: "Drives can be left out".into(), important: true }),
        };
        assert!(s.save(&path));
        assert_eq!(Saved::load(&path), s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_an_earlier_version_saved_is_read_past() {
        let path = std::env::temp_dir().join(format!("whereti-update-old-{}.txt", std::process::id()));
        std::fs::write(
            &path,
            "last_check=99\noffer_version=1.2.0\noffer_url=https://github.com/x.exe\noffer_sha256=ab\noffer_notes=a\\nb\n",
        )
        .unwrap();
        let s = Saved::load(&path);
        assert_eq!(s.last_check, Some(99));
        assert_eq!(s.offer, Some(Offer { version: "1.2.0".into(), notes: "a".into(), important: false }));
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
    fn latest_json_gives_a_version_a_line_and_nothing_else() {
        let o = parse_offer(br#"{"version":"99.0.0","notes":"Drives can be left out.\nMore.","important":true,"url":"https://evil.example/x.exe"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(o, Offer { version: "99.0.0".into(), notes: "Drives can be left out.".into(), important: true });
        // a click leads to whereti.com's download, whatever the file holds
        assert_eq!(crate::links::DOWNLOAD, "https://whereti.com/#download");
        // not newer: nothing to show
        assert_eq!(parse_offer(br#"{"version":"0.0.1"}"#).unwrap(), None);
        // a version that is not a version is refused, so it never reaches the address
        for bad in [r#"{"version":"99.0.0/../../x"}"#, r#"{"version":"99.0.0#@evil.example"}"#, r#"{"version":""}"#, "{}", "not json"] {
            assert!(parse_offer(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_line_shown_is_one_short_line_of_text() {
        assert_eq!(clean_notes("\n  first line  \nsecond"), "first line");
        assert_eq!(clean_notes("a\u{7}b\u{1b}[31mc"), "ab[31mc");
        let long = "x".repeat(500);
        assert_eq!(clean_notes(&long).chars().count(), MAX_NOTES + 1);
        assert!(clean_notes(&long).ends_with('…'));
    }

    #[test]
    fn an_important_version_cannot_be_put_off() {
        let mut u = Updater {
            offer: Arc::new(Mutex::new(Some(Offer { version: "99.0.0".into(), notes: String::new(), important: false }))),
            next_look: Instant::now(),
            later: None,
            last_check: None,
        };
        assert!(u.banner(true));
        assert!(!u.banner(false));
        u.later = Some(("99.0.0".into(), now_secs()));
        assert!(!u.banner(true));
        // a newer one shows at once
        *u.offer.lock() = Some(Offer { version: "99.0.1".into(), notes: String::new(), important: false });
        assert!(u.banner(true));
        *u.offer.lock() = Some(Offer { version: "99.0.0".into(), notes: String::new(), important: true });
        assert!(u.banner(true));
    }
}
