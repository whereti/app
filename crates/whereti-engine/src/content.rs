//! Content indexing and search (PLAN.md §6, §7.3).
//!
//! **What is indexed:** documents (PDF, Office, OpenDocument, RTF, HTML) and
//! text/code files inside the content folders — by default every user's
//! Desktop, Documents and Downloads.
//!
//! **How it stays current:** the name index already knows every file and is
//! kept live by the USN journal. A round ("crawl") asks it for the files in
//! the content folders, compares their size and last-write time with what
//! the content index holds, and (re)indexes only the difference; files that
//! disappeared or left the folders are removed. Files are keyed by MFT
//! record, so a rename costs nothing. A round runs at start, then only when
//! the journal shows a change to a document in the content folders (checked
//! on the few records of each change batch), plus every 30 minutes as a
//! safety net (e.g. a folder moved out of the content folders).
//!
//! **Not slowing the PC (first layers of §6.6):** one file at a time, on a
//! background-mode thread (low CPU *and* disk priority); parsers run in the
//! sandboxed `whereti-extract.exe` at below-normal priority; after each file
//! the indexer rests at least as long as the file took (≤ half a core).
//! Files in the cloud only (OneDrive placeholders) are never read: reading
//! them would download them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use whereti_content::{key, split_key, ContentIndex, Indexed, Status};
use whereti_core::index::{FLAG_CLOUD, FLAG_HIDDEN, FLAG_NOISE, FLAG_SYSTEM};
use whereti_core::NameIndex;
use whereti_extract::worker::{read_shared, Sandbox, SandboxError};
use whereti_extract::{ExtractError, Kind};
use whereti_ipc::{Caller, ContentReply, ContentStatus, Row};
use whereti_query::{fold, Clause, Query, Term};

use crate::engine::Shared;
use crate::system::{self, Background};

/// Documents larger than this are recorded as too large, not read.
const MAX_DOCUMENT: u64 = 64 << 20;
/// Of a text file, only the beginning is indexed.
const TEXT_HEAD: u64 = 8 << 20;
/// Characters indexed per file.
const MAX_CHARS: usize = 1_000_000;
const COMMIT_EVERY_DOCS: usize = 200;
const COMMIT_EVERY: Duration = Duration::from_secs(20);
/// How often the indexer looks whether a round is due.
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// A round even without a relevant change seen.
const SAFETY_ROUND: Duration = Duration::from_secs(30 * 60);
/// Content hits considered per search, before filters.
const SEARCH_DEPTH: usize = 500;

pub struct Content {
    index: Option<ContentIndex>,
    status: Mutex<ContentStatus>,
    folders: RwLock<Vec<String>>,
    /// Set to ask for a new round now (folders changed).
    wake: Mutex<bool>,
    wake_cv: Condvar,
    stop: AtomicBool,
    /// A change to a document in the content folders was seen.
    dirty: AtomicBool,
    /// Keys in the index, as of the last round (to notice deletions).
    known_keys: RwLock<HashSet<u64>>,
    exts: HashSet<&'static str>,
}

impl Content {
    pub fn open(dir: &Path, log: &dyn Fn(String)) -> Arc<Content> {
        let index = match ContentIndex::open(dir) {
            Ok(i) => Some(i),
            Err(e) => {
                log(format!("content index unavailable ({}): {e}", dir.display()));
                None
            }
        };
        let state = if index.is_some() { "starting" } else { "error: the content index cannot be opened" };
        Arc::new(Content {
            index,
            status: Mutex::new(ContentStatus { state: state.into(), ..Default::default() }),
            folders: RwLock::new(load_folders()),
            wake: Mutex::new(false),
            wake_cv: Condvar::new(),
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            known_keys: RwLock::new(HashSet::new()),
            exts: whereti_extract::extensions().into_iter().collect(),
        })
    }

    /// Called after each journal batch with the records it touched: is any
    /// of them a document in the content folders (or one we indexed)?
    pub(crate) fn note_changes(&self, idx: &NameIndex, vol: usize, records: &[u64]) {
        if self.dirty.load(Ordering::Relaxed) || self.index.is_none() || vol >= idx.volumes().len() {
            return;
        }
        let folders: Vec<String> = self.folders.read().iter().map(|f| format!("{}\\", fold(f.trim_end_matches('\\')))).collect();
        let letter = idx.volumes()[vol].letter();
        let known = self.known_keys.read();
        for &r in records {
            if known.contains(&key(letter, r)) {
                self.dirty.store(true, Ordering::Relaxed);
                return;
            }
            let Some(e) = idx.live_entry_of(vol, r) else { continue };
            let is_dir = idx.is_dir(e);
            if !is_dir {
                let name = idx.full_path(e);
                let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
                if !self.exts.contains(ext.as_str()) {
                    continue;
                }
            }
            let mut path = fold(&idx.full_path(e));
            path.push('\\');
            // a document inside, or a folder inside (moved in) or above them
            if folders.iter().any(|f| path.starts_with(f.as_str()) || (is_dir && f.starts_with(path.as_str()))) {
                self.dirty.store(true, Ordering::Relaxed);
                return;
            }
        }
    }

    /// The name index was rebuilt (rescan, new exclusions): plan again.
    pub(crate) fn recheck(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn status(&self) -> ContentStatus {
        self.status.lock().clone()
    }

    pub fn folders(&self) -> Vec<String> {
        self.folders.read().clone()
    }

    pub fn set_folders(&self, paths: &[String]) -> Result<(), String> {
        let paths = validate_folders(paths)?;
        save_folders(&paths).map_err(|e| format!("cannot save the content folders: {e}"))?;
        *self.folders.write() = paths;
        self.poke();
        Ok(())
    }

    fn poke(&self) {
        *self.wake.lock() = true;
        self.wake_cv.notify_all();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.poke();
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn set_state(&self, s: &str) {
        self.status.lock().state = s.to_string();
    }

    /// Runs on its own thread until `stop()`.
    pub(crate) fn run(self: Arc<Content>, shared: Arc<Shared>) {
        let Some(index) = self.index.as_ref() else { return };
        let _bg = Background::begin();
        // The name index must be ready first: it says which files exist.
        while !self.stopping() && !(shared.state.lock().as_str() == "ready" && shared.index.read().is_some()) {
            std::thread::sleep(Duration::from_millis(500));
        }
        let mut known = match index.known() {
            Ok(k) => k,
            Err(e) => {
                self.set_state(&format!("error: {e}"));
                return;
            }
        };
        self.update_counts(index, &known);
        let sandbox = Sandbox::beside_current_exe().map(|mut s| {
            s.timeout = Duration::from_secs(30);
            s.max_file = MAX_DOCUMENT;
            s
        });
        if sandbox.is_none() {
            shared.log.log("content: whereti-extract.exe is missing next to the service; only text files are indexed");
        }
        let mut forced = true;
        let mut last_round = Instant::now();
        while !self.stopping() {
            let dirty = self.dirty.swap(false, Ordering::Relaxed);
            if forced || dirty || last_round.elapsed() >= SAFETY_ROUND {
                last_round = Instant::now();
                if let Err(e) = self.round(index, &shared, &mut known, sandbox.as_ref()) {
                    shared.log.log(format!("content: {e}"));
                    self.set_state(&format!("error: {e}"));
                }
                *self.known_keys.write() = known.keys().copied().collect();
            }
            // Wait for the next check.
            let mut wake = self.wake.lock();
            if !*wake {
                self.wake_cv.wait_for(&mut wake, CHECK_EVERY);
            }
            forced = std::mem::take(&mut *wake);
        }
        let _ = index.commit();
    }

    fn round(
        &self,
        index: &ContentIndex,
        shared: &Shared,
        known: &mut HashMap<u64, Indexed>,
        sandbox: Option<&Sandbox>,
    ) -> Result<(), String> {
        let t = Instant::now();
        if !known.is_empty() || self.status.lock().state == "starting" {
            self.set_state("scanning");
        }
        let folders = self.folders();
        let wanted = {
            let guard = shared.index.read();
            let Some(idx) = guard.as_ref() else { return Ok(()) };
            files_in(idx, &folders)
        };
        let removed: Vec<u64> = known.keys().filter(|k| !wanted.contains_key(k)).copied().collect();
        for k in &removed {
            index.remove(*k);
            known.remove(k);
        }
        let mut todo: Vec<Candidate> = wanted
            .into_values()
            .filter(|c| known.get(&c.key).is_none_or(|i| i.size != c.size || i.modified != c.modified))
            .collect();
        // The newest files first: what the user works on becomes searchable first.
        todo.sort_unstable_by_key(|c| std::cmp::Reverse(c.modified));
        if !removed.is_empty() || !todo.is_empty() {
            shared.log.log(format!(
                "content: {} to index, {} removed (round planned in {} ms)",
                todo.len(),
                removed.len(),
                t.elapsed().as_millis()
            ));
        }
        {
            let mut s = self.status.lock();
            s.queued = todo.len() as u64;
            s.done = 0;
            s.state = if todo.is_empty() { "ready".into() } else { "indexing".into() };
        }
        let (mut since_commit, mut last_commit) = (0usize, Instant::now());
        let total = todo.len();
        let mut indexed = 0usize;
        for (n, c) in todo.into_iter().enumerate() {
            if self.stopping() || *self.wake.lock() {
                break; // stop, or folders changed: plan again
            }
            let started = Instant::now();
            if let Some((what, text)) = extract(&c, sandbox) {
                index.put(c.key, what, &text).map_err(|e| e.to_string())?;
                known.insert(c.key, what);
                since_commit += 1;
                indexed += 1;
            }
            {
                let mut s = self.status.lock();
                s.done = n as u64 + 1;
                s.queued = (total - n - 1) as u64;
            }
            if since_commit >= COMMIT_EVERY_DOCS || (since_commit > 0 && last_commit.elapsed() >= COMMIT_EVERY) {
                index.commit().map_err(|e| e.to_string())?;
                self.update_counts(index, known);
                since_commit = 0;
                last_commit = Instant::now();
            }
            // Rest at least as long as the file took.
            std::thread::sleep(started.elapsed().max(Duration::from_millis(5)));
        }
        index.commit().map_err(|e| e.to_string())?;
        self.update_counts(index, known);
        if indexed > 0 || !removed.is_empty() {
            shared.log.log(format!(
                "content: indexed {indexed} files in {} s; {} documents, {} MB on disk",
                t.elapsed().as_secs(),
                index.docs(),
                index.disk_bytes() >> 20
            ));
        }
        let mut s = self.status.lock();
        if s.queued == 0 {
            s.state = "ready".into();
        }
        Ok(())
    }

    fn update_counts(&self, index: &ContentIndex, known: &HashMap<u64, Indexed>) {
        let failed = known.values().filter(|i| matches!(i.status, Status::Failed | Status::Protected | Status::TooLarge)).count();
        let mut s = self.status.lock();
        s.files = index.docs();
        s.failed = failed as u64;
        s.disk_bytes = index.disk_bytes();
    }

    /// Content search (PLAN.md §6.5). The words come from `content:` terms,
    /// or — without them — from the plain words of the query; every other
    /// condition (ext:, path:, dm:, size:…) filters the files found.
    pub fn search(&self, idx: &NameIndex, query: &Query, limit: usize, caller: &dyn Caller) -> ContentReply {
        let t = Instant::now();
        let Some(index) = self.index.as_ref() else { return ContentReply::default() };
        let (words, filters) = split_query(query);
        if words.trim().is_empty() {
            return ContentReply::default();
        }
        let hits = match index.search(&words, SEARCH_DEPTH) {
            Ok(h) => h,
            Err(e) => return ContentReply { note: Some(format!("content search failed: {e}")), ..Default::default() },
        };
        let mut entries = Vec::with_capacity(hits.keys.len());
        for (k, _) in &hits.keys {
            let (letter, record) = split_key(*k);
            if let Some(e) = idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record)) {
                if !entries.contains(&e) {
                    entries.push(e);
                }
            }
        }
        let entries = idx.filter(&filters, &entries);
        let paths: Vec<String> = entries.iter().map(|&e| idx.full_path(e)).collect();
        let allowed = caller.can_read(&paths);
        let mut rows = Vec::new();
        let mut visible = 0u64;
        for ((e, path), ok) in entries.iter().zip(paths).zip(allowed) {
            if !ok {
                continue;
            }
            visible += 1;
            if rows.len() < limit {
                rows.push(Row { path, is_dir: false, size: idx.size(*e), modified: idx.modified(*e), flags: idx.flags(*e) });
            }
        }
        let status = self.status();
        let note = (status.state != "ready").then(|| {
            format!("content index: {} — {} documents so far, {} waiting", status.state, status.files, status.queued)
        });
        ContentReply {
            rows,
            total: if hits.keys.len() < SEARCH_DEPTH { visible } else { hits.total as u64 },
            micros: t.elapsed().as_micros() as u64,
            note,
        }
    }
}

/// Separates the words to look for in file contents from the conditions
/// that filter the files. With `content:` terms, plain words stay name
/// conditions (`raport content:total` = name has „raport”, text has „total”).
pub fn split_query(query: &Query) -> (String, Query) {
    let content_clause = |c: &Clause| !c.negated && matches!(c.any_of.as_slice(), [Term::Content(_)]);
    let explicit = query.clauses.iter().any(content_clause);
    let mut words = Vec::new();
    let mut filters = Query::default();
    for c in &query.clauses {
        match (c.negated, c.any_of.as_slice()) {
            (false, [Term::Content(t)]) => words.push(t.clone()),
            (false, [Term::Text(t)]) if !explicit => words.push(t.clone()),
            // a negated or OR-ed content term cannot filter by name
            _ if c.any_of.iter().any(|t| matches!(t, Term::Content(_))) => {}
            _ => filters.clauses.push(c.clone()),
        }
    }
    (words.join(" "), filters)
}

struct Candidate {
    key: u64,
    path: String,
    size: u64,
    modified: u32,
}

/// Files in the content folders that we can read, by key.
fn files_in(idx: &NameIndex, folders: &[String]) -> HashMap<u64, Candidate> {
    if folders.is_empty() {
        return HashMap::new();
    }
    let mut paths = Vec::new();
    for f in folders {
        let mut p = fold(f.trim_end_matches('\\'));
        p.push('\\');
        paths.push(Term::Path(p));
    }
    let exts = whereti_extract::extensions().into_iter().map(String::from).collect();
    let query = Query {
        clauses: vec![
            Clause { negated: false, any_of: vec![Term::FilesOnly] },
            Clause { negated: false, any_of: paths },
            Clause { negated: false, any_of: vec![Term::Ext(exts)] },
        ],
    };
    const SKIP: u16 = FLAG_CLOUD | FLAG_NOISE | FLAG_SYSTEM | FLAG_HIDDEN;
    let entries: Vec<u32> = idx.matching(&query).into_iter().filter(|&e| idx.flags(e) & SKIP == 0).collect();
    let records = idx.records_of(&entries);
    let mut out = HashMap::with_capacity(entries.len());
    for (e, r) in entries.into_iter().zip(records) {
        let Some(record) = r else { continue };
        let k = key(idx.volume_of(e).letter(), record);
        out.entry(k).or_insert_with(|| Candidate { key: k, path: idx.full_path(e), size: idx.size(e), modified: idx.modified(e) });
    }
    out
}

/// Reads one file. `None` when it could not be opened right now (gone,
/// locked, no access): it is tried again in a later round.
fn extract(c: &Candidate, sandbox: Option<&Sandbox>) -> Option<(Indexed, String)> {
    let what = |status| Indexed { size: c.size, modified: c.modified, status };
    let path = Path::new(&c.path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let kind = whereti_extract::kind_of(&ext)?;
    let result = if kind == Kind::Text {
        // Plain text needs no parser: read the head in-process.
        read_head(path, TEXT_HEAD).and_then(|b| whereti_extract::extract(&ext, &b, MAX_CHARS).map_err(SandboxError::Extract))
    } else if c.size > MAX_DOCUMENT {
        return Some((what(Status::TooLarge), String::new()));
    } else {
        sandbox?.extract_file(path, MAX_CHARS)
    };
    Some(match result {
        Ok(e) if e.text.trim().is_empty() => (what(Status::Empty), String::new()),
        Ok(e) => (what(Status::Ok), e.text),
        Err(SandboxError::Io(_)) => return None,
        Err(SandboxError::Extract(ExtractError::Protected)) => (what(Status::Protected), String::new()),
        Err(SandboxError::TooLarge(_)) => (what(Status::TooLarge), String::new()),
        Err(_) => (what(Status::Failed), String::new()),
    })
}

fn read_head(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    match read_shared(path, max) {
        Err(SandboxError::TooLarge(_)) => {
            use std::io::Read;
            use std::os::windows::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new().read(true).share_mode(7).open(path).map_err(SandboxError::Io)?;
            let mut buf = Vec::new();
            f.take(max).read_to_end(&mut buf).map_err(SandboxError::Io)?;
            Ok(buf)
        }
        other => other,
    }
}

// ---------------------------------------------------------------- folders

fn folders_file() -> PathBuf {
    system::data_dir().join("content-folders.txt")
}

/// Desktop, Documents and Downloads of every user (also under OneDrive).
pub fn default_folders() -> Vec<String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let users = PathBuf::from(format!(r"{drive}\Users"));
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(&users) else { return out };
    let mut profiles: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    profiles.sort();
    for p in profiles {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        if matches!(name.as_str(), "public" | "default" | "default user" | "all users") {
            continue;
        }
        for sub in ["Desktop", "Documents", "Downloads", r"OneDrive\Desktop", r"OneDrive\Documents"] {
            let f = p.join(sub);
            // not a junction (old "My Documents" style links)
            if f.is_dir() && !std::fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_symlink()) {
                out.push(f.display().to_string());
            }
        }
    }
    out
}

fn load_folders() -> Vec<String> {
    match std::fs::read_to_string(folders_file()) {
        Ok(text) => text
            .trim_start_matches('\u{feff}')
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => default_folders(),
    }
}

fn save_folders(paths: &[String]) -> std::io::Result<()> {
    system::ensure_data_dir()?;
    let mut text = String::from("# Folders whose documents whereti indexes by content, one per line.\n");
    for p in paths {
        text.push_str(p);
        text.push('\n');
    }
    std::fs::write(folders_file(), text)
}

/// `X:\folder` paths; at most 100.
pub fn validate_folders(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.len() > 100 {
        return Err("too many content folders (at most 100)".into());
    }
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let p = p.trim().replace('/', "\\");
        let p = p.trim_end_matches('\\').to_string();
        let b = p.as_bytes();
        if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') || p.len() > 1024 {
            return Err(format!(r"not a folder path: {p:?} (use C:\Folder)"));
        }
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use whereti_query::{parse, DateCtx};

    fn split(q: &str) -> (String, usize) {
        let (w, f) = split_query(&parse(q, &DateCtx { now: 0, tz_offset: 0 }));
        (w, f.clauses.len())
    }

    #[test]
    fn plain_words_search_content_and_the_rest_filters() {
        assert_eq!(split("factura 2024"), ("factura 2024".into(), 0));
        assert_eq!(split("factura ext:pdf dm:today"), ("factura".into(), 2));
        assert_eq!(split("*.pdf"), ("".into(), 1));
        assert_eq!(split("raport content:total"), ("total".into(), 1), "with content:, words stay name conditions");
        assert_eq!(split("!content:x factura"), ("factura".into(), 0), "a negated content term is ignored");
    }

    #[test]
    fn folders_are_checked() {
        assert_eq!(
            validate_folders(&[r"C:\Users\a\Documents\".into(), "d:/docs".into(), r"c:\users\A\documents".into()]).unwrap(),
            [r"C:\Users\a\Documents", r"d:\docs"]
        );
        assert!(validate_folders(&[r"\\server\share".into()]).is_err());
        assert!(validate_folders(&["Documents".into()]).is_err());
    }
}
