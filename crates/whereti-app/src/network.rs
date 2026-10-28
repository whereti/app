//! Network folders and NAS (PLAN.md §64): `\\nas\photos`, a mapped `Z:\`.
//!
//! The service runs as SYSTEM: it cannot see the user's mapped drives and
//! has no password for their NAS. So the app itself, as the user, walks the
//! folders the user added (Settings > Search) once an hour, gently, and
//! keeps their names in memory and in a cache file, so results are there
//! right after a restart. Searches look through that list on the searcher
//! thread; the results come under their own heading.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use whereti_ipc::Row;
use whereti_query::{fold, glob_match, Query, Term};

/// Every folder is walked again this often.
const EVERY: Duration = Duration::from_secs(3600);
/// At most this many entries over all folders, about 100 MB (twice that
/// while a walk replaces the list): a large photo NAS fits.
const MAX_ENTRIES: usize = 500_000;

struct Entry {
    path: String,
    /// Folded name, for matching.
    name: String,
    is_dir: bool,
    size: u64,
    modified: u32,
}

/// How a folder's last walk went.
#[derive(Clone, Default)]
pub struct FolderState {
    pub files: u64,
    /// The folder could not be read last time.
    pub error: Option<String>,
    pub walking: bool,
}

static FOLDERS: RwLock<Vec<String>> = RwLock::new(Vec::new());
static ENTRIES: RwLock<Vec<Entry>> = RwLock::new(Vec::new());
static STATES: Mutex<Vec<(String, FolderState)>> = Mutex::new(Vec::new());
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static STARTED: AtomicBool = AtomicBool::new(false);

fn config_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("network.txt")
}

fn cache_file() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("network-cache.tsv")
}

/// The folders the user added.
pub fn folders() -> Vec<String> {
    FOLDERS.read().map(|f| f.clone()).unwrap_or_default()
}

/// How each folder's last walk went.
pub fn states() -> Vec<(String, FolderState)> {
    STATES.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Replaces the folders (Settings) and walks them again now.
pub fn set_folders(list: Vec<String>) {
    let text: String = list.iter().map(|f| format!("{f}\n")).collect();
    if let Some(dir) = config_file().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::shell::write_whole(&config_file(), text.as_bytes());
    if let Ok(mut f) = FOLDERS.write() {
        *f = list;
    }
    refresh();
}

/// Walk again now.
pub fn refresh() {
    let (lock, cv) = &WAKE;
    if let Ok(mut w) = lock.lock() {
        *w = true;
        cv.notify_all();
    }
}

/// Loads the folders and the cache, and starts the walker (once).
pub fn start(wake_ui: impl Fn() + Send + 'static) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // a folder on this PC (an older version took one) is the service's to search
    let list: Vec<String> = std::fs::read_to_string(config_file())
        .map(|t| {
            t.lines().map(str::trim).filter(|l| !l.is_empty() && !crate::win::is_local_drive(l)).map(String::from).collect()
        })
        .unwrap_or_default();
    if let Ok(mut f) = FOLDERS.write() {
        *f = list;
    }
    std::thread::Builder::new()
        .name("network folders".into())
        .spawn(move || {
            // the cache can be large: read it here, not on the window's thread
            load_cache();
            wake_ui();
            walker(wake_ui)
        })
        .expect("a thread");
}

/// The user added network folders.
pub fn has_folders() -> bool {
    FOLDERS.read().is_ok_and(|f| !f.is_empty())
}

fn walker(wake_ui: impl Fn()) {
    let mut last = Instant::now() - EVERY;
    loop {
        let folders = folders();
        let woke = take_wake();
        if woke || (!folders.is_empty() && last.elapsed() >= EVERY) {
            last = Instant::now();
            walk_all(&folders);
            wake_ui();
        }
        let (lock, cv) = &WAKE;
        if let Ok(w) = lock.lock() {
            if !*w {
                let _ = cv.wait_timeout(w, Duration::from_secs(60));
            }
        }
    }
}

fn take_wake() -> bool {
    WAKE.0.lock().map(|mut w| std::mem::take(&mut *w)).unwrap_or(false)
}

fn set_state(folder: &str, f: impl FnOnce(&mut FolderState)) {
    if let Ok(mut s) = STATES.lock() {
        match s.iter_mut().find(|(k, _)| k == folder) {
            Some((_, st)) => f(st),
            None => {
                let mut st = FolderState::default();
                f(&mut st);
                s.push((folder.to_string(), st));
            }
        }
    }
}

fn walk_all(folders: &[String]) {
    let mut all: Vec<Entry> = Vec::new();
    for folder in folders {
        set_state(folder, |s| s.walking = true);
        let before = all.len();
        let result = walk(Path::new(folder), &mut all);
        let files = all[before..].iter().filter(|e| !e.is_dir).count() as u64;
        match result {
            Ok(()) => set_state(folder, |s| {
                *s = FolderState { files, error: None, walking: false };
            }),
            Err(e) => {
                // unreachable now (NAS asleep, VPN off): keep what was known
                all.truncate(before);
                if let Ok(old) = ENTRIES.read() {
                    let prefix = format!("{}\\", folder.trim_end_matches('\\'));
                    all.extend(old.iter().filter(|o| o.path.starts_with(&prefix)).map(|o| Entry {
                        path: o.path.clone(),
                        name: o.name.clone(),
                        is_dir: o.is_dir,
                        size: o.size,
                        modified: o.modified,
                    }));
                }
                set_state(folder, |s| {
                    s.error = Some(e);
                    s.walking = false;
                });
            }
        }
    }
    // folders removed meanwhile drop out with the new list
    if let Ok(mut e) = ENTRIES.write() {
        *e = all;
    }
    save_cache();
}

fn walk(root: &Path, out: &mut Vec<Entry>) -> Result<(), String> {
    std::fs::read_dir(root).map_err(|e| e.to_string())?;
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            if out.len() >= MAX_ENTRIES {
                return Ok(());
            }
            let Ok(meta) = item.metadata() else { continue };
            // links can loop; they are walked where they point
            if meta.file_type().is_symlink() {
                continue;
            }
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            let is_dir = meta.is_dir();
            if is_dir {
                stack.push(path.clone());
            }
            out.push(Entry {
                path: path.to_string_lossy().into_owned(),
                name: fold(&name),
                is_dir,
                size: if is_dir { 0 } else { meta.len() },
                modified: meta.modified().map(crate::recent::minutes_since_1601).unwrap_or(0),
            });
            seen += 1;
            // gentle on the network and the NAS
            if seen.is_multiple_of(2000) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    Ok(())
}

/// Written line by line (no second copy of the list in memory), to a
/// temporary file renamed over the cache, so it is never half written.
fn save_cache() {
    use std::io::Write;
    let path = cache_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tsv.tmp");
    let written = (|| -> std::io::Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        if let Ok(entries) = ENTRIES.read() {
            for e in entries.iter() {
                writeln!(out, "{}\t{}\t{}\t{}", if e.is_dir { "d" } else { "f" }, e.size, e.modified, e.path)?;
            }
        }
        out.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Read line by line, at most `MAX_ENTRIES`.
fn load_cache() {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(cache_file()) else { return };
    let folders = folders();
    let inside = |p: &str| folders.iter().any(|f| p.starts_with(&format!("{}\\", f.trim_end_matches('\\'))));
    let entries: Vec<Entry> = std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let mut parts = l.splitn(4, '\t');
            let is_dir = parts.next()? == "d";
            let size = parts.next()?.parse().ok()?;
            let modified = parts.next()?.parse().ok()?;
            let path = parts.next()?.to_string();
            let name = fold(path.rsplit('\\').next().unwrap_or(&path));
            inside(&path).then_some(Entry { path, name, is_dir, size, modified })
        })
        .take(MAX_ENTRIES)
        .collect();
    if let Ok(mut e) = ENTRIES.write() {
        *e = entries;
    }
}

/// Does entry `e` satisfy term `t`? Terms the network list cannot answer
/// (file contents, places, properties, duplicates) never match.
fn matches(e: &Entry, t: &Term) -> bool {
    match t {
        Term::Text(s) => e.name.contains(s.as_str()),
        Term::Glob(g) => glob_match(g, &e.name),
        Term::Regex(r) => regex_lite(r, &e.name),
        Term::Ext(list) => {
            let ext = if e.is_dir { "" } else { e.name.rsplit_once('.').map_or("", |(_, x)| x) };
            list.iter().any(|x| x == ext)
        }
        Term::Path(p) => fold(&e.path).contains(p.as_str()),
        Term::Size { min, max } => !e.is_dir && (*min..=*max).contains(&e.size),
        Term::Modified { min, max } => (*min..=*max).contains(&e.modified),
        Term::FoldersOnly => e.is_dir,
        Term::FilesOnly => !e.is_dir,
        Term::Mine => true,
        Term::Content(_) | Term::Place(_) | Term::Prop(_) | Term::Dupe => false,
        Term::All(ts) => ts.iter().all(|t| matches(e, t)),
    }
}

/// `regex:` on network names: plain text inside it only (no regex engine
/// in the app); good enough for `regex:^IMG_` style prefixes.
fn regex_lite(r: &str, name: &str) -> bool {
    let anchored = r.starts_with('^');
    let lit: String = r.trim_start_matches('^').chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ' ')).collect();
    let lit = fold(&lit);
    !lit.is_empty() && if anchored { name.starts_with(&lit) } else { name.contains(&lit) }
}

/// The network entries matching `query` (with a plain word, at least: the
/// network list is not for browsing), best first: exact name, name starting
/// with the word, then the rest; the newest first inside each.
pub fn search(query: &Query, limit: usize) -> Vec<Row> {
    let word = query.clauses.iter().find_map(|c| match (c.negated, c.any_of.as_slice()) {
        (false, [Term::Text(t)]) => Some(t.clone()),
        _ => None,
    });
    let Some(word) = word else { return Vec::new() };
    let Ok(entries) = ENTRIES.read() else { return Vec::new() };
    let mut hits: Vec<(u8, u32, &Entry)> = entries
        .iter()
        .filter(|e| query.clauses.iter().all(|c| c.any_of.iter().any(|t| matches(e, t)) != c.negated))
        .map(|e| {
            let stem = e.name.rsplit_once('.').map_or(e.name.as_str(), |(s, _)| s);
            let level = if stem == word || e.name == word {
                0
            } else if e.name.starts_with(&word) {
                1
            } else {
                2
            };
            (level, u32::MAX - e.modified, e)
        })
        .collect();
    // the best `limit` only: no full sort of what may be most of the list
    let order = |a: &(u8, u32, &Entry), b: &(u8, u32, &Entry)| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| a.2.path.cmp(&b.2.path));
    if hits.len() > limit && limit > 0 {
        hits.select_nth_unstable_by(limit - 1, order);
        hits.truncate(limit);
    }
    hits.sort_by(order);
    hits.into_iter()
        .take(limit)
        .map(|(_, _, e)| Row { path: e.path.clone(), is_dir: e.is_dir, size: e.size, modified: e.modified, flags: 0 })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, size: u64) -> Entry {
        Entry { path: path.into(), name: fold(path.rsplit('\\').next().unwrap()), is_dir, size, modified: 10 }
    }

    #[test]
    fn terms_match_network_entries() {
        let ctx = whereti_query::DateCtx { now: 1_000_000, tz_offset: 0 };
        let e = entry(r"\\nas\poze\Vacanță 2025\IMG_0042.JPG", false, 3 << 20);
        let ok = |q: &str| {
            let query = whereti_query::parse(q, &ctx);
            query.clauses.iter().all(|c| c.any_of.iter().any(|t| matches(&e, t)) != c.negated)
        };
        assert!(ok("img_0042"));
        assert!(ok("ext:jpg size:>1mb"));
        assert!(ok(r"path:\\nas\poze\vacanta"));
        assert!(ok("regex:^IMG_"));
        assert!(!ok("folder:"));
        assert!(!ok("content:plaja"));
        assert!(!ok("!img"));
    }
}
