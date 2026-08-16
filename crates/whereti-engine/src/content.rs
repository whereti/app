//! Content indexing and search (PLAN.md §6, §7.3).
//!
//! **What is indexed - with nothing to set up:**
//! - **documents** (PDF, Word, Excel, PowerPoint, OpenDocument, RTF, e-mails)
//!   **everywhere** on the indexed drives, except system and noise folders
//!   (Program Files, AppData, ProgramData, node_modules, .git, caches…) -
//!   a typical PC has a few thousand;
//! - **text and code** (txt, md, csv, html, sources) only inside the content
//!   folders - by default every user's Desktop, Documents and Downloads -
//!   because projects and caches hold hundreds of thousands of them.
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
use whereti_extract::worker::{read_shared, Sandbox, SandboxError, TagWorker};
use whereti_extract::{ExtractError, Kind};
use whereti_ipc::{Caller, ContentReply, ContentStatus, Row, ScanReply};
use whereti_query::{fold, Clause, Query, Term};

use crate::engine::Shared;
use crate::system::{self, Background};

/// Documents larger than this are recorded as too large, not read.
const MAX_DOCUMENT: u64 = 64 << 20;
/// Of a text file, only the beginning is indexed.
const TEXT_HEAD: u64 = 8 << 20;
/// Of a ZIP larger than `MAX_DOCUMENT`, its end: the list of its files.
const ZIP_TAIL: u64 = 16 << 20;
/// Characters indexed per file.
const MAX_CHARS: usize = 1_000_000;
const COMMIT_EVERY_DOCS: usize = 200;
const COMMIT_EVERY: Duration = Duration::from_secs(20);
/// How often the indexer looks whether a round is due.
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// A round even without a relevant change seen.
const SAFETY_ROUND: Duration = Duration::from_secs(30 * 60);
/// Documents indexed wherever they are (outside system and noise folders).
const EVERYWHERE: &[&str] = &[
    "pdf", "docx", "docm", "dotx", "dotm", "doc", "xlsx", "xlsm", "xlsb", "xls", "pptx", "pptm", "ppsx", "potx", "ppt",
    "odt", "ods", "odp", "rtf", "eml", "msg",
];
const SKIP: u16 = FLAG_CLOUD | FLAG_NOISE | FLAG_SYSTEM | FLAG_HIDDEN;

/// A user hint counts for this long (the app repeats it every 5 s).
const HINT_VALID: Duration = Duration::from_secs(20);
/// Pause while the PC's CPU (without us) is busier than this.
const BUSY_CPU: f32 = 0.70;
/// How often a paused indexer looks again.
const PAUSE_CHECK: Duration = Duration::from_secs(5);

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
    /// Last hint from a user session: (when, busy, away).
    hint: Mutex<Option<(Instant, bool, bool)>>,
    /// Content hashes for grouping identical copies: path -> (size,
    /// modified, hash), so a file is read again only after it changes.
    hashes: Mutex<HashMap<String, (u64, u32, u64)>>,
    /// The users' Pictures folders, as of the last round (photos there are
    /// read for where they were taken).
    pictures: RwLock<Vec<String>>,
    /// Objects and animals in photos: wanted by the user (photo-tags.txt).
    tags_on: AtomicBool,
    /// "off", "on", "downloading:<bytes>/<total>", "error: …".
    tags_state: Mutex<String>,
    /// Where the content index lives (for the one-time OCR pass marker).
    dir: PathBuf,
    /// Properties of photos, music, videos and Office files (props.rs).
    props: Mutex<crate::props::PropStore>,
    /// Search by meaning: wanted by the user (meaning.txt), its state, the
    /// documents' vectors and the model's worker (meaning.rs).
    meaning_on: AtomicBool,
    meaning_state: Mutex<String>,
    vectors: Mutex<crate::meaning::Vectors>,
    model: Mutex<crate::meaning::Model>,
}

/// The photo model (PLAN.md §55): CLIP ViT-B/32's image half, int8, from
/// Hugging Face at a fixed revision; downloaded only once the user turns
/// recognition on, and used only if its size and SHA-256 match.
const MODEL_FILE: &str = "clip-vit-b32-vision-int8.onnx";
const MODEL_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/vision_model_quantized.onnx";
const MODEL_SHA256: &str = "583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299";
const MODEL_SIZE: u64 = 89_117_001;
/// Photos larger than this are not read for what is in them.
const MAX_TAGGED: u64 = 64 << 20;

/// Where the model lives: only the service (SYSTEM) can write there, so no
/// other program can swap it.
fn model_path() -> PathBuf {
    system::data_dir().join("models").join(MODEL_FILE)
}

fn model_ready() -> bool {
    std::fs::metadata(model_path()).is_ok_and(|m| m.len() == MODEL_SIZE)
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
            hint: Mutex::new(None),
            hashes: Mutex::new(HashMap::new()),
            tags_on: AtomicBool::new(std::fs::read_to_string(dir.join("photo-tags.txt")).is_ok_and(|s| s.trim() == "on")),
            tags_state: Mutex::new("off".into()),
            pictures: RwLock::new(picture_folders()),
            dir: dir.to_path_buf(),
            props: Mutex::new(crate::props::PropStore::load(dir)),
            meaning_on: AtomicBool::new(std::fs::read_to_string(dir.join("meaning.txt")).is_ok_and(|s| s.trim() == "on")),
            meaning_state: Mutex::new("off".into()),
            vectors: Mutex::new(crate::meaning::Vectors::load(dir)),
            model: Mutex::new(crate::meaning::Model::new()),
        })
    }

    pub fn hint(&self, busy: bool, away: bool) {
        *self.hint.lock() = Some((Instant::now(), busy, away));
    }

    fn recent_hint(&self) -> Option<(bool, bool)> {
        self.hint.lock().filter(|h| h.0.elapsed() < HINT_VALID).map(|h| (h.1, h.2))
    }

    /// Why indexing should wait now, if it should (PLAN.md §6.6).
    fn pause_reason(&self, cpu: &mut system::CpuMeter) -> Option<&'static str> {
        if system::on_battery() {
            return Some("battery");
        }
        if self.recent_hint().is_some_and(|(busy, _)| busy) {
            return Some("fullscreen");
        }
        if cpu.busy() > BUSY_CPU {
            return Some("busy");
        }
        None
    }

    /// Called after each journal batch with the records it touched: is any
    /// of them a document in the content folders (or one we indexed)?
    pub(crate) fn note_changes(&self, idx: &NameIndex, vol: usize, records: &[u64]) {
        if self.dirty.load(Ordering::Relaxed) || self.index.is_none() || vol >= idx.volumes().len() {
            return;
        }
        // a photo added to Pictures counts too
        let folders: Vec<String> = self
            .folders
            .read()
            .iter()
            .chain(self.pictures.read().iter())
            .map(|f| format!("{}\\", fold(f.trim_end_matches('\\'))))
            .collect();
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
                let name = idx.name(e);
                let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
                if !self.exts.contains(ext.as_str()) && !PHOTO_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                // a document anywhere (outside noise) counts right away
                if EVERYWHERE.contains(&ext.as_str()) && idx.flags(e) & SKIP == 0 {
                    self.dirty.store(true, Ordering::Relaxed);
                    return;
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
        let mut s = self.status.lock().clone();
        s.tags = self.tags_state.lock().clone();
        s.meaning = self.meaning_state.lock().clone();
        s
    }

    /// Turns search by meaning on or off. On, the text model is downloaded
    /// the first time (in the background) and the documents are read for
    /// their meaning; off forgets their vectors.
    pub fn set_meaning(self: &Arc<Self>, on: bool) -> Result<(), String> {
        std::fs::write(self.dir.join("meaning.txt"), if on { "on\n" } else { "off\n" })
            .map_err(|e| format!("cannot save the setting: {e}"))?;
        self.meaning_on.store(on, Ordering::SeqCst);
        if on {
            self.ensure_meaning_model();
        } else {
            *self.meaning_state.lock() = "off".into();
            self.vectors.lock().clear();
            *self.model.lock() = crate::meaning::Model::new();
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.poke();
        Ok(())
    }

    /// Search by meaning is wanted and the model is there.
    fn meaning_ready(&self) -> bool {
        self.meaning_on.load(Ordering::SeqCst) && crate::meaning::model_ready()
    }

    /// Downloads the text model if it is missing (once, in the background).
    fn ensure_meaning_model(self: &Arc<Self>) {
        let total = crate::meaning::total_size();
        if crate::meaning::model_ready() {
            *self.meaning_state.lock() = "on".into();
            return;
        }
        {
            let mut st = self.meaning_state.lock();
            if st.starts_with("downloading") {
                return;
            }
            *st = format!("downloading:0/{total}");
        }
        let me = self.clone();
        let _ = std::thread::Builder::new().name("meaning-model".into()).spawn(move || {
            let _bg = Background::begin();
            let result = (|| -> Result<(), String> {
                let agent = format!("whereti/{}", env!("CARGO_PKG_VERSION"));
                let mut before = 0u64;
                for f in &crate::meaning::FILES {
                    let path = crate::meaning::path(f);
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() == f.size) {
                        before += f.size;
                        continue;
                    }
                    let mut last = 0u64;
                    let (status, bytes) = whereti_net::http_get(&crate::meaning::url(f), &agent, f.size + 1, &mut |done, _| {
                        if done - last >= 1 << 20 || done == f.size {
                            last = done;
                            *me.meaning_state.lock() = format!("downloading:{}/{total}", before + done);
                        }
                    })?;
                    if status != 200 {
                        return Err(format!("the server answered {status}"));
                    }
                    if bytes.len() as u64 != f.size || whereti_net::sha256_hex(&bytes)? != f.sha256 {
                        return Err("the model did not arrive whole".into());
                    }
                    let dir = path.parent().expect("models dir");
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                    let part = path.with_extension("part");
                    std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
                    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
                    before += f.size;
                }
                Ok(())
            })();
            *me.meaning_state.lock() = match result {
                Ok(()) => "on".into(),
                Err(e) => format!("error: {e}"),
            };
            me.dirty.store(true, Ordering::Relaxed);
            me.poke();
        });
    }

    /// The vectors of one document's text (its first pieces).
    fn read_meaning(&self, key: u64, text: &str) {
        let mut vectors = Vec::new();
        for piece in whereti_extract::meaning::pieces(text, crate::meaning::PIECES) {
            match self.model.lock().embed(&format!("passage: {piece}")) {
                Ok(v) => vectors.push(v),
                Err(_) => break,
            }
        }
        // kept even when empty: a document without text is not read again
        self.vectors.lock().set(key, vectors);
    }

    /// Documents close in meaning to the query's plain words; the other
    /// conditions filter them. Only files the caller may read.
    pub fn search_meaning(&self, idx: &NameIndex, query: &Query, limit: usize, caller: &dyn Caller) -> ContentReply {
        let t = Instant::now();
        if !self.meaning_ready() {
            return ContentReply::default();
        }
        let (words, filters) = split_query(query);
        let Some(question) = crate::meaning::question(&words) else { return ContentReply::default() };
        let Ok(q) = self.model.lock().embed(&format!("query: {question}")) else { return ContentReply::default() };
        let hits = self.vectors.lock().closest(&q, 200);
        let entries: Vec<u32> = hits
            .iter()
            .filter_map(|(k, _)| {
                let (letter, record) = split_key(*k);
                idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record))
            })
            .collect();
        let (prop_clauses, filters) = crate::props::split(&filters);
        let mut kept = idx.filter(&filters, &entries);
        if !prop_clauses.is_empty() {
            let with: HashSet<u32> = self.entries_with_props(idx, &prop_clauses).into_iter().collect();
            kept.retain(|e| with.contains(e));
        }
        // in the order of `hits`
        let kept: HashSet<u32> = kept.into_iter().collect();
        let ordered: Vec<u32> = entries.into_iter().filter(|e| kept.contains(e)).collect();
        let paths: Vec<String> = ordered.iter().map(|&e| idx.full_path(e)).collect();
        let allowed = caller.can_read(&paths);
        let rows: Vec<Row> = ordered
            .iter()
            .zip(paths)
            .zip(allowed)
            .filter(|(_, ok)| *ok)
            .take(limit)
            .map(|((&e, path), _)| Row { path, is_dir: false, size: idx.size(e), modified: idx.modified(e), flags: idx.flags(e) })
            .collect();
        ContentReply { total: rows.len() as u64, copies: vec![Vec::new(); rows.len()], rows, micros: t.elapsed().as_micros() as u64, note: None }
    }

    /// Turns recognising objects and animals in photos on or off. On, the
    /// model is downloaded the first time (in the background); the photos
    /// are then read again, with their tags, and off, without.
    pub fn set_photo_tags(self: &Arc<Self>, on: bool) -> Result<(), String> {
        std::fs::write(self.dir.join("photo-tags.txt"), if on { "on\n" } else { "off\n" })
            .map_err(|e| format!("cannot save the setting: {e}"))?;
        self.tags_on.store(on, Ordering::SeqCst);
        if on {
            self.ensure_model();
        } else {
            *self.tags_state.lock() = "off".into();
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.poke();
        Ok(())
    }

    /// Recognition is wanted and the model is there.
    fn tags_ready(&self) -> bool {
        self.tags_on.load(Ordering::SeqCst) && model_ready()
    }

    /// Downloads the model if it is missing (once, in the background).
    fn ensure_model(self: &Arc<Self>) {
        if model_ready() {
            *self.tags_state.lock() = "on".into();
            return;
        }
        {
            let mut st = self.tags_state.lock();
            if st.starts_with("downloading") {
                return;
            }
            *st = format!("downloading:0/{MODEL_SIZE}");
        }
        let me = self.clone();
        let _ = std::thread::Builder::new().name("photo-model".into()).spawn(move || {
            let _bg = Background::begin();
            let result = (|| -> Result<(), String> {
                let agent = format!("whereti/{}", env!("CARGO_PKG_VERSION"));
                let mut last = 0u64;
                let (status, bytes) = whereti_net::http_get(MODEL_URL, &agent, MODEL_SIZE + 1, &mut |done, _| {
                    if done - last >= 1 << 20 || done == MODEL_SIZE {
                        last = done;
                        *me.tags_state.lock() = format!("downloading:{done}/{MODEL_SIZE}");
                    }
                })?;
                if status != 200 {
                    return Err(format!("the server answered {status}"));
                }
                if bytes.len() as u64 != MODEL_SIZE || whereti_net::sha256_hex(&bytes)? != MODEL_SHA256 {
                    return Err("the model did not arrive whole".into());
                }
                let path = model_path();
                let dir = path.parent().expect("models dir");
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let part = path.with_extension("part");
                std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
                std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
                Ok(())
            })();
            *me.tags_state.lock() = match result {
                Ok(()) => "on".into(),
                Err(e) => format!("error: {e}"),
            };
            me.dirty.store(true, Ordering::Relaxed);
            me.poke();
        });
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
        // Once, when OCR arrives: documents that gave no text before (scanned
        // PDFs) are read again. Forgetting them makes the next round redo them.
        let ocr_marker = self.dir.join("ocr-1");
        let ocr_pass = !ocr_marker.exists();
        if ocr_pass {
            let before = known.len();
            known.retain(|_, i| !matches!(i.status, Status::Empty | Status::Failed));
            if known.len() < before {
                shared.log.log(format!("content: {} documents without text are read again with OCR", before - known.len()));
            }
        }
        // Once, when places arrive: photos indexed before are read again,
        // now with where they were taken.
        if self.tags_on.load(Ordering::SeqCst) {
            self.ensure_model();
        }
        if self.meaning_on.load(Ordering::SeqCst) {
            self.ensure_meaning_model();
        }
        let places_marker = self.dir.join("places-2"); // 2: again, with the tolerant EXIF reader (0.0.12)
        let places_pass = !places_marker.exists();
        if places_pass {
            let before = known.len();
            let photos: HashSet<u64> = {
                let guard = shared.index.read();
                guard.as_ref().map(photo_keys).unwrap_or_default()
            };
            known.retain(|k, _| !photos.contains(k));
            if known.len() < before {
                shared.log.log(format!("content: {} photos are read again for where they were taken", before - known.len()));
            }
        }
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
                match self.round(index, &shared, &mut known, sandbox.as_ref()) {
                    Ok(()) => {
                        if ocr_pass && self.status.lock().queued == 0 && !ocr_marker.exists() {
                            let _ = std::fs::write(&ocr_marker, "documents without text were read again with OCR\n");
                        }
                        if places_pass && self.status.lock().queued == 0 && !places_marker.exists() {
                            let _ = std::fs::write(&places_marker, "photos were read again for where they were taken\n");
                        }
                    }
                    Err(e) => {
                        shared.log.log(format!("content: {e}"));
                        self.set_state(&format!("error: {e}"));
                    }
                }
                *self.known_keys.write() = known.keys().copied().collect();
            }
            // Wait for the next check.
            let mut wake = self.wake.lock();
            if !*wake {
                self.wake_cv.wait_for(&mut wake, CHECK_EVERY);
                self.model.lock().tidy();
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
        let pictures = picture_folders();
        *self.pictures.write() = pictures.clone();
        let wanted = {
            let guard = shared.index.read();
            let Some(idx) = guard.as_ref() else { return Ok(()) };
            files_in(idx, &folders, &pictures)
        };
        // recognition turned on (model ready) or off: the photos again
        let tags_now = if self.tags_ready() { "on" } else { "off" };
        let tags_marker = self.dir.join("tags-indexed");
        let tags_before = std::fs::read_to_string(&tags_marker).map(|s| s.trim().to_string()).unwrap_or_else(|_| "off".into());
        if tags_before != tags_now {
            let photos: HashSet<u64> = {
                let guard = shared.index.read();
                guard.as_ref().map(photo_keys).unwrap_or_default()
            };
            let before = known.len();
            known.retain(|k, _| !photos.contains(k));
            shared.log.log(format!("content: {} photos are read again ({tags_now} recognition)", before - known.len()));
            let _ = std::fs::write(&tags_marker, format!("{tags_now}\n"));
        }
        let mut tagger = if tags_now == "on" { TagWorker::beside_current_exe(&model_path()) } else { None };
        let removed: Vec<u64> = known.keys().filter(|k| !wanted.contains_key(k)).copied().collect();
        for k in &removed {
            index.remove(*k);
            known.remove(k);
            self.props.lock().remove(*k);
            self.vectors.lock().remove(*k);
        }
        // search by meaning turned on: the documents already indexed, again for it
        let meaning_now = self.meaning_ready();
        let meaning_missing: Vec<Candidate> = if meaning_now {
            let vectors = self.vectors.lock();
            wanted
                .values()
                .filter(|c| {
                    !c.photo_only
                        && !c.media_only
                        && known.get(&c.key).is_some_and(|i| i.size == c.size && i.modified == c.modified && i.status == Status::Ok)
                        && !vectors.contains(c.key)
                        && !is_picture(&c.path)
                })
                .map(|c| Candidate { meaning_only: true, props_only: false, path: c.path.clone(), ..*c })
                .collect()
        } else {
            Vec::new()
        };
        // indexed before properties were read (or read without them): only those
        let props_missing: Vec<Candidate> = {
            let store = self.props.lock();
            wanted
                .values()
                .filter(|c| {
                    known.get(&c.key).is_some_and(|i| i.size == c.size && i.modified == c.modified)
                        && !store.contains(c.key)
                        && crate::props::has_props(&ext_of(&c.path))
                })
                .map(|c| Candidate { props_only: true, media_only: false, photo_only: false, path: c.path.clone(), ..*c })
                .collect()
        };
        let mut todo: Vec<Candidate> = wanted
            .into_values()
            .filter(|c| known.get(&c.key).is_none_or(|i| i.size != c.size || i.modified != c.modified))
            .chain(props_missing)
            .chain(meaning_missing)
            .collect();
        let tz = system::date_ctx().tz_offset;
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
        let mut cpu = system::CpuMeter::new();
        let mut last_cpu_check = Instant::now();
        let mut paused_for: Option<&'static str> = None;
        'files: for (n, c) in todo.into_iter().enumerate() {
            if self.stopping() || *self.wake.lock() {
                break; // stop, or folders changed: plan again
            }
            // Wait while the user games, presents, is on battery or the CPU is busy.
            loop {
                // the CPU is measured over at least a second
                let reason = if last_cpu_check.elapsed() >= Duration::from_secs(1) || paused_for.is_some() {
                    last_cpu_check = Instant::now();
                    self.pause_reason(&mut cpu)
                } else if system::on_battery() {
                    Some("battery")
                } else if self.recent_hint().is_some_and(|(busy, _)| busy) {
                    Some("fullscreen")
                } else {
                    None
                };
                match reason {
                    None => {
                        if let Some(r) = paused_for.take() {
                            shared.log.log(format!("content: resumed (was paused: {r})"));
                            self.set_state("indexing");
                        }
                        break;
                    }
                    Some(r) => {
                        if paused_for != Some(r) {
                            if paused_for.is_none() {
                                // make the work done so far searchable before waiting
                                index.commit().map_err(|e| e.to_string())?;
                                since_commit = 0;
                            }
                            shared.log.log(format!("content: paused ({r})"));
                            paused_for = Some(r);
                            self.set_state(&format!("paused: {r}"));
                        }
                        let mut wake = self.wake.lock();
                        if !*wake {
                            self.wake_cv.wait_for(&mut wake, PAUSE_CHECK);
                        }
                        if *wake || self.stopping() {
                            break 'files;
                        }
                    }
                }
            }
            let started = Instant::now();
            if c.meaning_only {
                if let Some((_, text)) = extract(&c, sandbox, None) {
                    self.read_meaning(c.key, &text);
                }
            } else if !c.props_only {
                if let Some((what, text)) = extract(&c, sandbox, tagger.as_mut()) {
                    index.put(c.key, what, &text).map_err(|e| e.to_string())?;
                    known.insert(c.key, what);
                    since_commit += 1;
                    indexed += 1;
                    if meaning_now && !c.photo_only && !c.media_only && !is_picture(&c.path) {
                        self.read_meaning(c.key, &text);
                    }
                }
            }
            let ext = ext_of(&c.path);
            if crate::props::has_props(&ext) {
                let p = crate::props::read(Path::new(&c.path), &ext, tz);
                self.props.lock().set(c.key, p);
            }
            {
                let mut s = self.status.lock();
                s.done = n as u64 + 1;
                s.queued = (total - n - 1) as u64;
            }
            if since_commit >= COMMIT_EVERY_DOCS || (since_commit > 0 && last_commit.elapsed() >= COMMIT_EVERY) {
                index.commit().map_err(|e| e.to_string())?;
                self.props.lock().save();
                self.vectors.lock().save();
                self.update_counts(index, known);
                since_commit = 0;
                last_commit = Instant::now();
            }
            // Rest at least as long as the file took; a quarter of that
            // while nobody is at the PC (locked, screen saver).
            let away = self.recent_hint().is_some_and(|(_, away)| away);
            let rest = if away { started.elapsed() / 4 } else { started.elapsed() };
            std::thread::sleep(rest.max(Duration::from_millis(5)));
        }
        index.commit().map_err(|e| e.to_string())?;
        self.props.lock().save();
        self.vectors.lock().save();
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
    /// or - without them - from the plain words of the query; every other
    /// condition (ext:, path:, dm:, size:…) filters the files found.
    /// `usage`: files this user opens often or lately (full path, weight 1-12).
    pub fn search(
        &self,
        idx: &NameIndex,
        query: &Query,
        limit: usize,
        usage: &[(String, u8)],
        active: Arc<Vec<u8>>,
        caller: &dyn Caller,
    ) -> ContentReply {
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
        let mut text_score: HashMap<u32, f32> = HashMap::new();
        for (k, score) in &hits.keys {
            let (letter, record) = split_key(*k);
            if let Some(e) = idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record)) {
                if !entries.contains(&e) {
                    entries.push(e);
                    text_score.insert(e, *score);
                }
            }
        }
        let (prop_clauses, filters) = crate::props::split(&filters);
        let mut entries = idx.filter(&filters, &entries);
        if !prop_clauses.is_empty() {
            let with: HashSet<u32> = self.entries_with_props(idx, &prop_clauses).into_iter().collect();
            entries.retain(|e| with.contains(e));
        }
        let paths: Vec<String> = entries.iter().map(|&e| idx.full_path(e)).collect();
        let allowed = caller.can_read(&paths);
        let w = idx.weigher_with(active);
        let mut found: Vec<(f32, u32, String)> = entries
            .iter()
            .zip(paths)
            .zip(allowed)
            .filter(|(_, ok)| *ok)
            .map(|((&e, path), _)| {
                let used = usage.iter().find(|(p, _)| p.eq_ignore_ascii_case(&path)).map(|u| u.1);
                let score = text_score.get(&e).copied().unwrap_or(0.0) * weight(idx.importance(e, &w), idx.flags(e), used);
                (score, e, path)
            })
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
        let groups = self.group_copies(idx, &found);
        let visible = groups.len() as u64;
        let mut rows = Vec::new();
        let mut copies = Vec::new();
        for (first, others) in groups.into_iter().take(limit) {
            let (_, e, path) = &found[first];
            rows.push(Row { path: path.clone(), is_dir: false, size: idx.size(*e), modified: idx.modified(*e), flags: idx.flags(*e) });
            copies.push(others.into_iter().map(|k| found[k].2.clone()).collect());
        }
        let status = self.status();
        let note = (status.state != "ready").then(|| {
            format!("content index: {} - {} documents so far, {} waiting", status.state, status.files, status.queued)
        });
        ContentReply {
            rows,
            copies,
            total: if hits.keys.len() < SEARCH_DEPTH { visible } else { hits.total as u64 },
            micros: t.elapsed().as_micros() as u64,
            note,
        }
    }
}

/// How much the place, type, date and use of a document add to how well
/// its text matches: the same signals as for names (PLAN.md §48-50).
fn weight(importance: u8, flags: u16, used: Option<u8>) -> f32 {
    // importance 0 (most) .. 31 (least): x1.64 .. x0.40
    let mut f = 1.0 + (16.0 - importance as f32) * 0.04;
    match used {
        Some(u) => f *= 1.0 + u as f32 * 0.15,
        None if flags & (FLAG_NOISE | FLAG_HIDDEN | FLAG_SYSTEM) != 0 => f *= 0.5,
        None => {}
    }
    f
}

/// Files above this size are never read to group copies.
const MAX_HASHED: u64 = 64 * 1024 * 1024;

impl Content {
    /// Groups the found documents that are the same file in several places
    /// (same size, same bytes): `(best, [the other copies])`, in the order
    /// of `found`. Only files of a size another one shares are read.
    fn group_copies(&self, idx: &NameIndex, found: &[(f32, u32, String)]) -> Vec<(usize, Vec<usize>)> {
        let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
        for (k, (_, e, _)) in found.iter().enumerate() {
            let size = idx.size(*e);
            if size > 0 && size <= MAX_HASHED {
                by_size.entry(size).or_default().push(k);
            }
        }
        let mut copy_of: HashMap<usize, usize> = HashMap::new();
        for same_size in by_size.values().filter(|v| v.len() > 1) {
            let mut first_with: HashMap<u64, usize> = HashMap::new();
            for &k in same_size {
                let (_, e, path) = &found[k];
                let Some(h) = self.content_hash(path, idx.size(*e), idx.modified(*e)) else { continue };
                match first_with.get(&h) {
                    Some(&first) => {
                        copy_of.insert(k, first);
                    }
                    None => {
                        first_with.insert(h, k);
                    }
                }
            }
        }
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut slot: HashMap<usize, usize> = HashMap::new();
        for k in 0..found.len() {
            match copy_of.get(&k) {
                Some(first) => groups[slot[first]].1.push(k),
                None => {
                    slot.insert(k, groups.len());
                    groups.push((k, Vec::new()));
                }
            }
        }
        groups
    }

    fn content_hash(&self, path: &str, size: u64, modified: u32) -> Option<u64> {
        if let Some(&(s, m, h)) = self.hashes.lock().get(path) {
            if s == size && m == modified {
                return Some(h);
            }
        }
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() as u64 != size {
            return None;
        }
        let h = xxhash_rust::xxh3::xxh3_64(&bytes);
        let mut cache = self.hashes.lock();
        if cache.len() > 20_000 {
            cache.clear();
        }
        cache.insert(path.to_string(), (size, modified, h));
        Some(h)
    }
}

/// What a "search the documents not indexed yet" goes through.
#[derive(Default)]
pub struct ScanPlan {
    needles: Vec<String>,
    candidates: Vec<Candidate>,
}

impl Content {
    /// The documents the content index has not read yet that match the
    /// query's other conditions, newest first. Needs the name index (held
    /// briefly by the caller); reads nothing.
    pub fn scan_plan(&self, idx: &NameIndex, query: &Query) -> ScanPlan {
        let (words, filters) = split_query(query);
        let needles: Vec<String> =
            words.split_whitespace().map(|w| fold(w.trim_matches('"'))).filter(|w| !w.is_empty()).collect();
        if needles.is_empty() {
            return ScanPlan::default();
        }
        let known = self.known_keys.read();
        let mut candidates: Vec<Candidate> =
            files_in(idx, &self.folders(), &[]).into_values().filter(|c| !known.contains(&c.key)).collect();
        drop(known);
        if !filters.clauses.is_empty() {
            let entries: Vec<u32> = candidates
                .iter()
                .filter_map(|c| {
                    let (letter, record) = split_key(c.key);
                    idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record))
                })
                .collect();
            let keep: HashSet<String> = idx.filter(&filters, &entries).into_iter().map(|e| idx.full_path(e)).collect();
            candidates.retain(|c| keep.contains(&c.path));
        }
        candidates.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.key.cmp(&b.key)));
        ScanPlan { needles, candidates }
    }

    /// One step: reads documents from `skip` on, for about `budget`, and
    /// returns those whose text has every word. Only files the caller may
    /// read are opened at all (checked a few at a time, before reading).
    pub fn scan_run(&self, plan: &ScanPlan, skip: u64, budget: Duration, limit: usize, caller: &dyn Caller) -> ScanReply {
        let t = Instant::now();
        let n = plan.candidates.len();
        let mut i = (skip as usize).min(n);
        let mut rows = Vec::new();
        let sandbox = Sandbox::beside_current_exe().map(|mut s| {
            s.timeout = Duration::from_secs(15);
            s.max_file = MAX_DOCUMENT;
            s
        });
        // every step reads at least one document, so the caller always gets on
        'steps: while i < n && rows.len() < limit && (i == skip as usize || t.elapsed() < budget) {
            let chunk = &plan.candidates[i..(i + 16).min(n)];
            let paths: Vec<String> = chunk.iter().map(|c| c.path.clone()).collect();
            let allowed = caller.can_read(&paths);
            for (c, ok) in chunk.iter().zip(allowed) {
                i += 1;
                if ok {
                    if let Some((_, text)) = extract(c, sandbox.as_ref(), None) {
                        let folded = fold(&text);
                        if plan.needles.iter().all(|w| folded.contains(w.as_str())) {
                            rows.push(Row { path: c.path.clone(), is_dir: false, size: c.size, modified: c.modified, flags: 0 });
                        }
                    }
                }
                if (t.elapsed() >= budget && i > skip as usize) || rows.len() >= limit {
                    break 'steps;
                }
            }
        }
        ScanReply { rows, next: i as u64, total: n as u64, done: i >= n }
    }
}

impl Content {
    /// The live entries whose properties satisfy `clauses` (props.rs).
    pub fn entries_with_props(&self, idx: &NameIndex, clauses: &[Clause]) -> Vec<u32> {
        let keys = self.props.lock().keys_matching(clauses);
        keys.into_iter()
            .filter_map(|k| {
                let (letter, record) = split_key(k);
                idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record))
            })
            .collect()
    }
}

fn ext_of(path: &str) -> String {
    path.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('\\')).unwrap_or("").to_ascii_lowercase()
}

/// Separates the words to look for in file contents from the conditions
/// that filter the files. With `content:` terms, plain words stay name
/// conditions (`raport content:total` = name has „raport”, text has „total”).
pub fn split_query(query: &Query) -> (String, Query) {
    // content: makes the other words name filters; loc: does not (a photo's
    // name says nothing, its text is where it was taken)
    let content_clause = |c: &Clause| !c.negated && matches!(c.any_of.as_slice(), [Term::Content(_)]);
    let explicit = query.clauses.iter().any(content_clause);
    let mut words = Vec::new();
    let mut filters = Query::default();
    for c in &query.clauses {
        match (c.negated, c.any_of.as_slice()) {
            (false, [Term::Content(t)]) => words.push(t.clone()),
            (false, [Term::Place(t)]) => {
                // the place names are in the text of photos: look among photos only
                words.push(t.clone());
                let photos = PHOTO_EXTS.iter().map(|e| e.to_string()).collect();
                filters.clauses.push(Clause { negated: false, any_of: vec![Term::Ext(photos)] });
            }
            (false, [Term::Text(t)]) if !explicit => words.push(t.clone()),
            // a negated or OR-ed content term cannot filter by name
            _ if c.any_of.iter().any(|t| matches!(t, Term::Content(_) | Term::Place(_))) => {}
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
    /// A photo in a pictures folder: only where it was taken is read, not
    /// its text (OCR on every holiday photo would cost hours).
    photo_only: bool,
    /// Music or a video: no text, only its properties.
    media_only: bool,
    /// Already indexed: only its properties are read now.
    props_only: bool,
    /// Already indexed: read again only for its meaning.
    meaning_only: bool,
}

/// Photos a camera or phone writes its GPS position into.
const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "heic", "heif", "avif", "tif", "tiff", "png", "webp"];

/// Where a photo was taken, as text for the index: "Rome, Lazio, Italy"
/// and then every name of the place. `None` without a GPS position.
fn place_text(path: &Path, ext: &str) -> Option<String> {
    if !PHOTO_EXTS.contains(&ext) {
        return None;
    }
    // EXIF is at the start of a JPEG; a HEIC keeps it anywhere in the file
    let head = if matches!(ext, "heic" | "heif" | "avif") { 64 << 20 } else { 512 << 10 };
    let bytes = read_head(path, head).ok()?;
    let (lat, lon) = whereti_places::gps(&bytes)?;
    let place = whereti_places::place_of(lat, lon)?;
    Some(format!("{}\n{}", place.label(), place.search_text()))
}

/// The files to index, by key: documents everywhere, and text/code inside
/// the content folders (all outside cloud-only, hidden, system and noise).
fn files_in(idx: &NameIndex, folders: &[String], pictures: &[String]) -> HashMap<u64, Candidate> {
    let file = || Clause { negated: false, any_of: vec![Term::FilesOnly] };
    let documents = Query {
        clauses: vec![
            file(),
            Clause {
                negated: false,
                any_of: vec![Term::Ext(
                    EVERYWHERE.iter().filter(|e| whereti_extract::kind_of(e).is_some()).map(|e| e.to_string()).collect(),
                )],
            },
        ],
    };
    let mut entries = idx.matching(&documents);
    if !folders.is_empty() {
        let paths = folders
            .iter()
            .map(|f| {
                let mut p = fold(f.trim_end_matches('\\'));
                p.push('\\');
                Term::Path(p)
            })
            .collect();
        let text = whereti_extract::extensions().into_iter().filter(|e| !EVERYWHERE.contains(e)).map(String::from).collect();
        let in_folders = Query {
            clauses: vec![
                file(),
                Clause { negated: false, any_of: paths },
                Clause { negated: false, any_of: vec![Term::Ext(text)] },
            ],
        };
        entries.extend(idx.matching(&in_folders));
    }
    let entries: Vec<u32> = entries
        .into_iter()
        .filter(|&e| idx.flags(e) & SKIP == 0)
        .filter(|&e| !is_picture(&idx.full_path(e)) || (MIN_PICTURE..=MAX_PICTURE).contains(&idx.size(e)))
        .collect();
    let records = idx.records_of(&entries);
    let mut out = HashMap::with_capacity(entries.len());
    for (e, r) in entries.into_iter().zip(records) {
        let Some(record) = r else { continue };
        let k = key(idx.volume_of(e).letter(), record);
        out.entry(k).or_insert_with(|| Candidate {
            key: k,
            path: idx.full_path(e),
            size: idx.size(e),
            modified: idx.modified(e),
            photo_only: false,
            media_only: false,
            props_only: false, meaning_only: false,
        });
    }
    // photos in the pictures folders: where they were taken
    if !pictures.is_empty() {
        let paths = pictures
            .iter()
            .map(|f| {
                let mut p = fold(f.trim_end_matches('\\'));
                p.push('\\');
                Term::Path(p)
            })
            .collect();
        let photos = Query {
            clauses: vec![
                file(),
                Clause { negated: false, any_of: paths },
                Clause { negated: false, any_of: vec![Term::Ext(PHOTO_EXTS.iter().map(|e| e.to_string()).collect())] },
            ],
        };
        let entries: Vec<u32> =
            idx.matching(&photos).into_iter().filter(|&e| idx.flags(e) & SKIP == 0 && idx.size(e) >= MIN_PICTURE).collect();
        let records = idx.records_of(&entries);
        for (e, r) in entries.into_iter().zip(records) {
            let Some(record) = r else { continue };
            let k = key(idx.volume_of(e).letter(), record);
            // a photo in a content folder is read in full (its text too)
            out.entry(k).or_insert_with(|| Candidate {
                key: k,
                path: idx.full_path(e),
                size: idx.size(e),
                modified: idx.modified(e),
                photo_only: true,
                media_only: false,
                props_only: false, meaning_only: false,
            });
        }
    }
    // music and videos in the user's folders: their properties
    let media_dirs: Vec<String> = folders.iter().chain(pictures).cloned().chain(media_folders()).collect();
    if !media_dirs.is_empty() {
        let paths = media_dirs
            .iter()
            .map(|f| {
                let mut p = fold(f.trim_end_matches('\\'));
                p.push('\\');
                Term::Path(p)
            })
            .collect();
        let exts = crate::props::AUDIO_EXTS.iter().chain(crate::props::VIDEO_EXTS).map(|e| e.to_string()).collect();
        let media = Query {
            clauses: vec![file(), Clause { negated: false, any_of: paths }, Clause { negated: false, any_of: vec![Term::Ext(exts)] }],
        };
        let entries: Vec<u32> = idx.matching(&media).into_iter().filter(|&e| idx.flags(e) & SKIP == 0).collect();
        let records = idx.records_of(&entries);
        for (e, r) in entries.into_iter().zip(records) {
            let Some(record) = r else { continue };
            let k = key(idx.volume_of(e).letter(), record);
            out.entry(k).or_insert_with(|| Candidate {
                key: k,
                path: idx.full_path(e),
                size: idx.size(e),
                modified: idx.modified(e),
                photo_only: false,
                media_only: true,
                props_only: false, meaning_only: false,
            });
        }
    }
    out
}

/// Every user's Music and Videos folders.
fn media_folders() -> Vec<String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let Ok(dir) = std::fs::read_dir(format!(r"{drive}\Users")) else { return Vec::new() };
    let mut out = Vec::new();
    for p in dir.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        for sub in ["Music", "Videos"] {
            let f = p.join(sub);
            if f.is_dir() && !std::fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_symlink()) {
                out.push(f.display().to_string());
            }
        }
    }
    out
}

/// Every user's Pictures folder, and OneDrive's (cloud-only photos are
/// never read: they are skipped like every cloud-only file).
fn picture_folders() -> Vec<String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let Ok(dir) = std::fs::read_dir(format!(r"{drive}\Users")) else { return Vec::new() };
    let mut out = Vec::new();
    for p in dir.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        if matches!(name.as_str(), "public" | "default" | "default user" | "all users") {
            continue;
        }
        // OneDrive names the folder in the language of Windows
        for sub in ["Pictures", r"OneDrive\Pictures", r"OneDrive\Imagini", r"OneDrive\Bilder", r"OneDrive\Imágenes", r"OneDrive\Images", r"OneDrive\Immagini", r"OneDrive\Obrazy", r"OneDrive\Imagens"] {
            let f = p.join(sub);
            if f.is_dir() && !std::fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_symlink()) {
                out.push(f.display().to_string());
            }
        }
    }
    out
}

/// Keys of every photo in the name index (for the one-time places pass).
fn photo_keys(idx: &NameIndex) -> HashSet<u64> {
    let q = Query {
        clauses: vec![Clause { negated: false, any_of: vec![Term::Ext(PHOTO_EXTS.iter().map(|e| e.to_string()).collect())] }],
    };
    let entries = idx.matching(&q);
    let records = idx.records_of(&entries);
    entries
        .into_iter()
        .zip(records)
        .filter_map(|(e, r)| Some(key(idx.volume_of(e).letter(), r?)))
        .collect()
}

/// Pictures smaller than this are icons and thumbnails; larger ones are
/// raw camera files or posters. Neither is worth the OCR.
const MIN_PICTURE: u64 = 20 << 10;
const MAX_PICTURE: u64 = 30 << 20;

fn is_picture(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    whereti_extract::kind_of(ext) == Some(Kind::Image)
}

/// Reads one file. `None` when it could not be opened right now (gone,
/// locked, no access): it is tried again in a later round.
/// What is in a photo, as words for the index ("dog Hund perro chien
/// cane pies cachorro câine"); `None` without tags.
fn photo_tags(path: &Path, tagger: &mut TagWorker) -> Option<String> {
    let bytes = read_shared(path, MAX_TAGGED).ok()?;
    let tags = tagger.tags(&bytes).ok()?;
    (!tags.is_empty()).then(|| whereti_extract::tags::index_text(&tags))
}

/// Reads one file. `tagger`: recognise what is in photos (when on).
fn extract(c: &Candidate, sandbox: Option<&Sandbox>, tagger: Option<&mut TagWorker>) -> Option<(Indexed, String)> {
    let what = |status| Indexed { size: c.size, modified: c.modified, status };
    if c.media_only {
        return Some((what(Status::Empty), String::new()));
    }
    let path = Path::new(&c.path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let kind = whereti_extract::kind_of(&ext)?;
    // a photo: where it was taken and what is in it
    let place = if kind == Kind::Image {
        let tags = tagger.and_then(|t| photo_tags(path, t));
        match (place_text(path, &ext), tags) {
            (Some(p), Some(t)) => Some(format!("{p}\n{t}")),
            (p, t) => p.or(t),
        }
    } else {
        None
    };
    if c.photo_only {
        return Some(match place {
            Some(text) => (what(Status::Ok), text),
            None => (what(Status::Empty), String::new()),
        });
    }
    let result = if kind == Kind::Text {
        // Plain text needs no parser: read the head in-process.
        read_head(path, TEXT_HEAD).and_then(|b| whereti_extract::extract(&ext, &b, MAX_CHARS).map_err(SandboxError::Extract))
    } else if c.size > MAX_DOCUMENT && ext == "zip" {
        // a large ZIP: its names, from the directory at its end
        let sandbox = sandbox?;
        read_tail(path, ZIP_TAIL).and_then(|b| sandbox.extract_bytes(whereti_extract::ZIP_END, b, MAX_CHARS))
    } else if c.size > MAX_DOCUMENT {
        return Some((what(Status::TooLarge), String::new()));
    } else {
        sandbox?.extract_file(path, MAX_CHARS)
    };
    // a photo: where it was taken, then the text OCR read in it
    let result = match (result, place) {
        (Ok(mut e), Some(p)) => {
            e.text = format!("{p}\n{}", e.text);
            Ok(e)
        }
        (Err(_), Some(p)) => Ok(whereti_extract::Extracted { text: p, truncated: false }),
        (r, None) => r,
    };
    Some(match result {
        Ok(e) if e.text.trim().is_empty() => (what(Status::Empty), String::new()),
        Ok(e) => (what(Status::Ok), e.text),
        Err(SandboxError::Io(_)) => return None,
        // the extension promised a format the file is not in (a Tcl .msg): no text, not a failure
        Err(SandboxError::Extract(ExtractError::Unsupported)) => (what(Status::Empty), String::new()),
        Err(SandboxError::Extract(ExtractError::Protected)) => (what(Status::Protected), String::new()),
        Err(SandboxError::TooLarge(_)) => (what(Status::TooLarge), String::new()),
        Err(_) => (what(Status::Failed), String::new()),
    })
}

/// The last `max` bytes of a file.
fn read_tail(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::windows::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).share_mode(7).open(path).map_err(SandboxError::Io)?;
    let len = f.metadata().map_err(SandboxError::Io)?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).map_err(SandboxError::Io)?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).map_err(SandboxError::Io)?;
    Ok(buf)
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

    #[test]
    fn loc_looks_for_the_place_among_photos() {
        let ctx = DateCtx { now: 0, tz_offset: 0 };
        let (words, filters) = split_query(&parse("loc:Roma", &ctx));
        assert_eq!(words, "roma");
        assert!(filters.clauses.iter().any(|c| matches!(c.any_of.as_slice(), [Term::Ext(e)] if e.contains(&"heic".to_string()))));
        // with other words, all of them must be in the photo's text
        let (words, _) = split_query(&parse("loc:italia plaja", &ctx));
        assert_eq!(words, "italia plaja");
    }

    #[test]
    fn place_type_date_and_use_weigh_on_content_results() {
        // more important documents count more, files of tool folders less
        assert!(weight(0, 0, None) > weight(16, 0, None));
        assert!(weight(16, 0, None) > weight(31, 0, None));
        assert_eq!(weight(16, FLAG_NOISE, None), 0.5);
        // a used file is never halved as noise, and counts more with use
        assert!(weight(16, FLAG_NOISE, Some(1)) > weight(16, 0, None));
        assert!(weight(16, 0, Some(12)) > weight(16, 0, Some(2)));
    }

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

    /// Allowed everything except paths containing "secret".
    struct NotSecrets;
    impl Caller for NotSecrets {
        fn can_read(&self, paths: &[String]) -> Vec<bool> {
            paths.iter().map(|p| !p.contains("secret")).collect()
        }
    }

    #[test]
    fn documents_not_indexed_yet_are_searched_step_by_step() {
        let dir = std::env::temp_dir().join(format!("whereti-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, text: &str, modified: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            Candidate { key: modified as u64, path: path.to_string_lossy().into_owned(), size: text.len() as u64, modified, photo_only: false, media_only: false, props_only: false, meaning_only: false }
        };
        let plan = ScanPlan {
            needles: vec![fold("chirie"), fold("Iulie")],
            candidates: vec![
                file("a.txt", "Chiria pe iulie: 1200 lei", 30),
                file("b.txt", "nimic aici", 20),
                file("secret.txt", "chiria pe iulie, secret", 10),
                file("c.md", "Contract: chirie, plata în IULIE", 5),
            ],
        };
        let content = Content::open(&dir.join("index"), &|_| {});
        // a tiny budget still reads at least one document per step
        let first = content.scan_run(&plan, 0, Duration::ZERO, 100, &NotSecrets);
        assert_eq!(first.next, 1);
        let all = content.scan_run(&plan, 0, Duration::from_secs(10), 100, &NotSecrets);
        let found: Vec<&str> = all.rows.iter().map(|r| r.path.rsplit('\\').next().unwrap()).collect();
        // "chiria" does not contain "chirie": only c.md has both words; the secret is never read
        assert_eq!(found, ["c.md"]);
        assert!(all.done);
        assert_eq!((all.next, all.total), (4, 4));
        // resuming past the end gives nothing more
        let after = content.scan_run(&plan, 4, Duration::from_secs(1), 100, &NotSecrets);
        assert!(after.done && after.rows.is_empty());
        drop(content);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
