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
use std::sync::{Arc, OnceLock};
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
/// Characters indexed from an Outlook mailbox (its newest messages).
const MAILBOX_CHARS: usize = 4_000_000;
/// A mailbox changes whenever Outlook runs: it is read again at most this
/// often (minutes).
const MAILBOX_EVERY: u32 = 24 * 60;
/// Of a ZIP larger than `MAX_DOCUMENT`, its end: the list of its files.
const ZIP_TAIL: u64 = 16 << 20;
/// Characters indexed per file.
const MAX_CHARS: usize = 1_000_000;
const COMMIT_EVERY_DOCS: usize = 200;
/// Documents whose text is kept for the meaning model between the two
/// rounds (a few KB each); beyond this, read again.
const MEANING_KEPT: usize = 50_000;
const COMMIT_EVERY: Duration = Duration::from_secs(20);
/// How often the indexer looks whether a round is due.
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// A round even without a relevant change seen.
const SAFETY_ROUND: Duration = Duration::from_secs(30 * 60);
/// Documents indexed wherever they are (outside system and noise folders).
const EVERYWHERE: &[&str] = &[
    "pdf", "docx", "docm", "dotx", "dotm", "doc", "dot", "xlsx", "xlsm", "xlsb", "xls", "xltx", "xltm", "pptx", "pptm",
    "ppsx", "ppsm", "potx", "potm", "ppt", "pps", "pot", "odt", "ott", "ods", "ots", "odp", "otp", "rtf", "eml", "msg",
    "epub", "fb2", "xps", "oxps",
];
const SKIP: u16 = FLAG_CLOUD | FLAG_NOISE | FLAG_SYSTEM | FLAG_HIDDEN;

/// A user hint counts for this long (the app repeats it every 5 s).
const HINT_VALID: Duration = Duration::from_secs(20);
/// Pause while the PC's CPU (without us) is busier than this.
const BUSY_CPU: f32 = 0.70;
/// How often a paused indexer looks again.
const PAUSE_CHECK: Duration = Duration::from_secs(5);

/// Content hits considered per search, before filters; deeper (up to
/// `MAX_SEARCH_DEPTH`) while filters and permissions leave too few.
const SEARCH_DEPTH: usize = 500;
const MAX_SEARCH_DEPTH: usize = 20_000;
/// Rounds started by changes seen in the journal: at most this often (a
/// file written all the time, such as a game's settings, would otherwise
/// plan a round every check).
const DIRTY_GAP: Duration = Duration::from_secs(60);
/// A model that failed (search by meaning, photo tags) is tried again
/// after this long, with one small question first.
const MODEL_RETRY: Duration = Duration::from_secs(60 * 60);
/// Properties and vectors are written at most this often while indexing
/// (and at the end of a round, before a pause and at stop). Lost ones are
/// read again: they are kept per file.
const SIDE_SAVE_EVERY: Duration = Duration::from_secs(5 * 60);
/// A CPU busy for this long (a program at idle priority, a miner) no
/// longer stops the indexing: it goes on, slowly.
const BUSY_MAX: Duration = Duration::from_secs(15 * 60);
/// How many recent hints are kept (one per user session, every 5 s).
const HINTS_KEPT: usize = 16;
/// Opening the content index failed: tried again this often.
const REOPEN_EVERY: Duration = Duration::from_secs(60);

pub struct Content {
    /// Set once the index is open (it is opened again later when it could
    /// not be at start).
    index: OnceLock<ContentIndex>,
    status: Mutex<ContentStatus>,
    folders: RwLock<Vec<String>>,
    /// Set to ask for a new round now (folders changed).
    wake: Mutex<bool>,
    wake_cv: Condvar,
    stop: AtomicBool,
    /// A change to a document in the content folders was seen.
    dirty: AtomicBool,
    /// Keys in the index with the size and date indexed, as of the last
    /// round (to notice deletions and real changes).
    known_keys: RwLock<HashMap<u64, (u64, u32)>>,
    exts: HashSet<&'static str>,
    /// Recent hints from the user sessions: (when, busy, away).
    hints: Mutex<Vec<(Instant, bool, bool)>>,
    /// Content hashes for grouping identical copies, so a file is read
    /// again only after it changes.
    hashes: HashCache,
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
    /// Files whose reading once took the service down (a parser crash
    /// aborts the process): never read in-process again (crash.txt).
    crashed: Mutex<HashSet<u64>>,
    /// Search by meaning: wanted by the user (meaning.txt), its state, the
    /// documents' vectors and the model's worker (meaning.rs).
    meaning_on: AtomicBool,
    meaning_state: Mutex<String>,
    vectors: Mutex<crate::meaning::Vectors>,
    model: Mutex<crate::meaning::Model>,
    /// When the meaning model / the photo model last failed: not used for
    /// indexing until `MODEL_RETRY` has passed and a question works.
    meaning_failed: Mutex<Option<Instant>>,
    tags_failed: Mutex<Option<Instant>>,
    /// A model download is under way (one at a time).
    meaning_downloading: AtomicBool,
    tags_downloading: AtomicBool,
    /// Music and videos (no text: not counted as documents).
    media_keys: Mutex<HashSet<u64>>,
    /// Text read for meaning while the documents' text came first: kept
    /// until the model reads it in the next round, so no document is read
    /// twice. Lost on a restart (then read again).
    meaning_text: Mutex<HashMap<u64, Vec<String>>>,
    /// The last round read only text: this one is the models' turn, whatever
    /// waits (a file locked for good must not hold the photos back).
    models_turn: AtomicBool,
    /// Files whose reading failed once in a way that may pass (a timeout
    /// on a busy PC): read once more in a later round before it counts.
    failed_once: Mutex<HashMap<u64, (u64, u32)>>,
    /// Documents that need the sandbox while it is missing: not planned
    /// again until it is back.
    no_sandbox: Mutex<HashMap<u64, (u64, u32)>>,
    /// A background thread hashes documents to group copies.
    hashing: Arc<AtomicBool>,
}

/// The photo model (PLAN.md §55): CLIP ViT-B/32's image half, int8, from
/// Hugging Face at a fixed revision; downloaded only once the user turns
/// recognition on, and used only if its size and SHA-256 match.
const MODEL_FILE: &str = "clip-vit-b32-vision-int8.onnx";
const MODEL_URL: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/d15189d7028b43f1d3e65039190477f6af591c2a/onnx/vision_model_quantized.onnx";
const MODEL_SHA256: &str = "583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299";
const MODEL_SIZE: u64 = 89_117_001;
/// An unchanged copy of the optional models (whereti/app, release
/// `models-1`), used only when Hugging Face cannot give a file: the maker
/// may take a model down, or a network may block huggingface.co. A file is
/// kept only if its size and SHA-256 match, wherever it came from.
pub(crate) const MODELS_COPY: &str = "https://github.com/whereti/app/releases/download/models-1/";

/// Downloads a model file from `urls` in order: the first copy with the
/// right size and SHA-256 wins. `progress` gets the bytes of the current try.
pub(crate) fn fetch_model(
    urls: &[String],
    agent: &str,
    size: u64,
    sha256: &str,
    progress: &mut dyn FnMut(u64),
) -> Result<Vec<u8>, String> {
    let mut last = String::from("no source for the model");
    for url in urls {
        match whereti_net::http_get(url, agent, size + 1, &mut |done, _| progress(done)) {
            Ok((200, bytes)) => {
                if bytes.len() as u64 == size && whereti_net::sha256_hex(&bytes)? == sha256 {
                    return Ok(bytes);
                }
                last = "the model did not arrive whole".into();
            }
            Ok((status, _)) => last = format!("the server answered {status}"),
            Err(e) => last = e,
        }
    }
    Err(last)
}

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
        let index = OnceLock::new();
        let mut known = HashMap::new();
        match ContentIndex::open(dir) {
            Ok(i) => {
                // what is indexed already, for changes seen before the first round
                known = i.known().map(|k| known_sizes(&k)).unwrap_or_default();
                let _ = index.set(i);
            }
            Err(e) => log(format!("content index unavailable ({}): {e}; tried again later", dir.display())),
        }
        let state = if index.get().is_some() { "starting" } else { "error: the content index cannot be opened" };
        Arc::new(Content {
            index,
            status: Mutex::new(ContentStatus { state: state.into(), ..Default::default() }),
            folders: RwLock::new(load_folders()),
            wake: Mutex::new(false),
            wake_cv: Condvar::new(),
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            known_keys: RwLock::new(known),
            exts: whereti_extract::extensions().into_iter().collect(),
            hints: Mutex::new(Vec::new()),
            hashes: Arc::new(Mutex::new(HashMap::new())),
            tags_on: AtomicBool::new(std::fs::read_to_string(dir.join("photo-tags.txt")).is_ok_and(|s| s.trim() == "on")),
            tags_state: Mutex::new("off".into()),
            pictures: RwLock::new(picture_folders()),
            dir: dir.to_path_buf(),
            props: Mutex::new(crate::props::PropStore::load(dir)),
            crashed: Mutex::new(load_crashed(dir, log)),
            meaning_on: AtomicBool::new(std::fs::read_to_string(dir.join("meaning.txt")).is_ok_and(|s| s.trim() == "on")),
            meaning_state: Mutex::new("off".into()),
            vectors: Mutex::new(crate::meaning::Vectors::load(dir)),
            model: Mutex::new(crate::meaning::Model::new()),
            meaning_failed: Mutex::new(None),
            tags_failed: Mutex::new(None),
            meaning_downloading: AtomicBool::new(false),
            tags_downloading: AtomicBool::new(false),
            media_keys: Mutex::new(HashSet::new()),
            meaning_text: Mutex::new(HashMap::new()),
            models_turn: AtomicBool::new(false),
            failed_once: Mutex::new(HashMap::new()),
            no_sandbox: Mutex::new(HashMap::new()),
            hashing: Arc::new(AtomicBool::new(false)),
        })
    }

    fn index(&self) -> Option<&ContentIndex> {
        self.index.get()
    }

    /// A hint from one user session (each repeats its own every 5 s; there
    /// may be several, e.g. a locked one and the one at the screen).
    pub fn hint(&self, busy: bool, away: bool) {
        let mut hints = self.hints.lock();
        hints.retain(|h| h.0.elapsed() < HINT_VALID);
        if hints.len() >= HINTS_KEPT {
            hints.remove(0);
        }
        hints.push((Instant::now(), busy, away));
    }

    /// (busy, away) from the recent hints: busy when any session says so
    /// (a game in full screen), away only when every one does.
    fn recent_hint(&self) -> (bool, bool) {
        combine_hints(&self.hints.lock())
    }

    /// Why indexing should wait now, if it should (PLAN.md §6.6).
    fn pause_reason(&self, cpu: &mut system::CpuMeter) -> Option<&'static str> {
        if system::on_battery() {
            return Some("battery");
        }
        if self.recent_hint().0 {
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
        if self.dirty.load(Ordering::Relaxed) || self.index().is_none() || vol >= idx.volumes().len() {
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
        let now = system::date_ctx().now;
        for &r in records {
            let entry = idx.live_entry_of(vol, r);
            if let Some(&(size, modified)) = known.get(&key(letter, r)) {
                if let Some(e) = entry.filter(|&e| !idx.is_dir(e)) {
                    // the same size and date: only read, renamed or its attributes set
                    if idx.size(e) == size && idx.modified(e) == modified {
                        continue;
                    }
                    // a mailbox is read again only once its date moved a day on
                    if is_mailbox(idx.name(e)) && now < modified.saturating_add(MAILBOX_EVERY) {
                        continue;
                    }
                }
                // changed, or gone: plan again
                self.dirty.store(true, Ordering::Relaxed);
                return;
            }
            let Some(e) = entry else { continue };
            let is_dir = idx.is_dir(e);
            if !is_dir {
                // hidden, system, cloud-only and noise files are never indexed
                if idx.flags(e) & SKIP != 0 {
                    continue;
                }
                let name = idx.name(e);
                let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
                let photo_or_media = CAMERA_EXTS.contains(&ext.as_str())
                    || whereti_query::kinds::RAW.contains(&ext.as_str())
                    || whereti_query::kinds::AUDIO_TAGGED.contains(&ext.as_str())
                    || whereti_query::kinds::VIDEO_TAGGED.contains(&ext.as_str());
                if !self.exts.contains(ext.as_str()) && !PHOTO_EXTS.contains(&ext.as_str()) && !photo_or_media {
                    continue;
                }
                // a document, photo or song anywhere (outside noise) counts right away
                if (EVERYWHERE.contains(&ext.as_str()) || photo_or_media) && idx.flags(e) & SKIP == 0 {
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
        // turned on or off by the user: a model that failed is tried again
        *self.meaning_failed.lock() = None;
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

    /// The meaning model failed (onnxruntime missing, a damaged model, no
    /// answer): documents are not read for their meaning for a while.
    fn meaning_failure(&self, e: &str) {
        *self.meaning_failed.lock() = Some(Instant::now());
        *self.meaning_state.lock() = format!("error: {e}");
    }

    /// The meaning model answered: it works (again).
    fn meaning_works(&self) {
        if self.meaning_failed.lock().take().is_some() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        let mut st = self.meaning_state.lock();
        if st.starts_with("error") && self.meaning_on.load(Ordering::SeqCst) {
            *st = "on".into();
        }
    }

    /// May the indexing use the meaning model now? After a failure, only
    /// once `MODEL_RETRY` has passed and one small question works.
    fn meaning_usable(&self) -> bool {
        if !self.meaning_ready() {
            return false;
        }
        let failed = *self.meaning_failed.lock();
        match failed {
            None => true,
            Some(t) if t.elapsed() < MODEL_RETRY => false,
            Some(_) => match self.meaning_probe() {
                Ok(()) => {
                    self.meaning_works();
                    true
                }
                Err(e) => {
                    self.meaning_failure(&e);
                    false
                }
            },
        }
    }

    /// One small question to the meaning model: does it answer?
    fn meaning_probe(&self) -> Result<(), String> {
        let answer = self.model.lock().embed("passage: whereti");
        answer.map(|_| ())
    }

    /// Downloads the text model if it is missing (once, in the background).
    fn ensure_meaning_model(self: &Arc<Self>) {
        let total = crate::meaning::total_size();
        if crate::meaning::model_ready() {
            *self.meaning_state.lock() = "on".into();
            return;
        }
        *self.meaning_state.lock() = format!("downloading:0/{total}");
        // turned off and on again during a download: that one goes on
        if self.meaning_downloading.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("meaning-model".into()).spawn(move || {
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
                    let urls = [crate::meaning::url(f), format!("{MODELS_COPY}{}", f.name)];
                    let bytes = fetch_model(&urls, &agent, f.size, f.sha256, &mut |done| {
                        if (done.abs_diff(last) >= 1 << 20 || done == f.size) && me.meaning_on.load(Ordering::SeqCst) {
                            last = done;
                            *me.meaning_state.lock() = format!("downloading:{}/{total}", before + done);
                        }
                    })?;
                    let dir = path.parent().expect("models dir");
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                    let part = path.with_extension("part");
                    std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
                    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
                    before += f.size;
                }
                Ok(())
            })();
            {
                // turned off meanwhile: off it stays
                let mut st = me.meaning_state.lock();
                *st = match result {
                    _ if !me.meaning_on.load(Ordering::SeqCst) => "off".into(),
                    Ok(()) => "on".into(),
                    Err(e) => format!("error: {e}"),
                };
                me.meaning_downloading.store(false, Ordering::SeqCst);
            }
            me.dirty.store(true, Ordering::Relaxed);
            me.poke();
        });
        if spawned.is_err() {
            self.meaning_downloading.store(false, Ordering::SeqCst);
            *self.meaning_state.lock() = "error: cannot start the download".into();
        }
    }

    /// The vectors of one document's text (its first pieces). `false` when
    /// the model failed (the document is tried again later, its old vectors
    /// gone: they are of an older text). A piece the model cannot read
    /// while it still answers is the document's trouble: the document keeps
    /// what could be read, so it is not read again until it changes.
    fn read_meaning(&self, key: u64, pieces: &[String]) -> bool {
        let embed = &mut |piece: &str| self.model.lock().embed(&format!("passage: {piece}"));
        match embed_pieces(pieces, embed, &mut || self.meaning_probe()) {
            Ok(vectors) => {
                if !vectors.is_empty() {
                    self.meaning_works();
                }
                // kept even when empty (no text): such a document is not read again
                self.vectors.lock().set(key, vectors);
                true
            }
            Err(e) => {
                self.meaning_failure(&e);
                self.vectors.lock().remove(key);
                false
            }
        }
    }

    /// Keeps a document's pieces for the meaning model, so it is not read
    /// again for them (up to `MEANING_KEPT` documents).
    fn keep_meaning_text(&self, key: u64, pieces: Vec<String>) {
        let mut kept = self.meaning_text.lock();
        if kept.len() < MEANING_KEPT || kept.contains_key(&key) {
            kept.insert(key, pieces);
        }
    }

    /// The question's vector, before the name index is locked (the model
    /// may take a moment to start); `None` when it is not a question.
    pub fn embed_question(&self, query: &Query) -> Option<Vec<i8>> {
        if !self.meaning_ready() {
            return None;
        }
        let (words, _) = split_query(query);
        let question = crate::meaning::question(&words)?;
        // never wait long behind the indexing for the model
        let mut model = self.model.try_lock_for(Duration::from_secs(3))?;
        let v = model.embed(&format!("query: {question}")).ok()?;
        drop(model);
        self.meaning_works();
        Some(v)
    }

    /// Notes the file being read in-process (a crash then names it).
    fn reading(&self, key: Option<u64>) {
        let path = self.dir.join("reading.txt");
        let _ = match key {
            Some(k) => std::fs::write(path, k.to_string()),
            None => std::fs::write(path, ""),
        };
    }

    /// Documents close in meaning to the query's plain words; the other
    /// conditions filter them. Only files the caller may read.
    pub fn search_meaning(&self, idx: &NameIndex, query: &Query, q: &[i8], limit: usize, caller: &dyn Caller) -> ContentReply {
        let t = Instant::now();
        let (_, filters) = split_query(query);
        // every document above the bar, best first; filtered a part at a
        // time until enough are left
        let hits = self.vectors.lock().closest(q, MAX_SEARCH_DEPTH);
        let (prop_clauses, filters) = crate::props::split(&filters);
        let with: Option<HashSet<u32>> =
            (!prop_clauses.is_empty()).then(|| self.entries_with_props(idx, &prop_clauses).into_iter().collect());
        let mut rows: Vec<Row> = Vec::new();
        for part in hits.chunks(SEARCH_DEPTH / 2) {
            if rows.len() >= limit {
                break;
            }
            let entries: Vec<u32> = part
                .iter()
                .filter_map(|(k, _)| {
                    let (letter, record) = split_key(*k);
                    idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record))
                })
                .collect();
            let kept: HashSet<u32> =
                idx.filter(&filters, &entries).into_iter().filter(|e| with.as_ref().is_none_or(|w| w.contains(e))).collect();
            // in the order of `hits`
            let ordered: Vec<u32> = entries.into_iter().filter(|e| kept.contains(e)).collect();
            let paths: Vec<String> = ordered.iter().map(|&e| idx.full_path(e)).collect();
            let allowed = caller.can_read(&paths);
            rows.extend(
                ordered
                    .iter()
                    .zip(paths)
                    .zip(allowed)
                    .filter(|(_, ok)| *ok)
                    .map(|((&e, path), _)| Row { path, is_dir: false, size: idx.size(e), modified: idx.modified(e), flags: idx.flags(e) }),
            );
        }
        rows.truncate(limit);
        ContentReply { total: rows.len() as u64, copies: vec![Vec::new(); rows.len()], rows, micros: t.elapsed().as_micros() as u64, note: None }
    }

    /// Turns recognising objects and animals in photos on or off. On, the
    /// model is downloaded the first time (in the background); the photos
    /// are then read again, with their tags, and off, without.
    pub fn set_photo_tags(self: &Arc<Self>, on: bool) -> Result<(), String> {
        std::fs::write(self.dir.join("photo-tags.txt"), if on { "on\n" } else { "off\n" })
            .map_err(|e| format!("cannot save the setting: {e}"))?;
        self.tags_on.store(on, Ordering::SeqCst);
        *self.tags_failed.lock() = None;
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

    /// The photo model's worker for this round, when it may be used: after
    /// a failure, only once `MODEL_RETRY` has passed and it answers on one
    /// of `photos` (a few are tried; one may be unreadable, or a picture
    /// Windows cannot decode, which says nothing of the model).
    fn tagger_for_round<'a>(&self, photos: impl Iterator<Item = &'a Candidate>) -> Option<TagWorker> {
        if !self.tags_ready() {
            return None;
        }
        let mut tagger = TagWorker::beside_current_exe(&model_path())?;
        let failed = *self.tags_failed.lock();
        match failed {
            None => return Some(tagger),
            Some(t) if t.elapsed() < MODEL_RETRY => return None,
            Some(_) => {}
        }
        // the model loaded and answered, though not with tags
        let mut answered = false;
        for c in photos.filter(|c| c.size <= MAX_TAGGED).take(10) {
            let Ok(bytes) = read_shared(Path::new(&c.path), MAX_TAGGED) else { continue };
            match tagger.tags(&bytes) {
                Ok(_) => {
                    self.tags_works();
                    return Some(tagger);
                }
                Err(e) if undecodable(&e) => answered = true,
                Err(e) => {
                    self.tags_failure(&e);
                    return None;
                }
            }
        }
        if answered {
            self.tags_works();
            return Some(tagger);
        }
        // nothing to try it on: later
        *self.tags_failed.lock() = Some(Instant::now());
        None
    }

    fn tags_failure(&self, e: &str) {
        *self.tags_failed.lock() = Some(Instant::now());
        *self.tags_state.lock() = format!("error: {e}");
    }

    fn tags_works(&self) {
        self.tags_failed.lock().take();
        let mut st = self.tags_state.lock();
        if st.starts_with("error") && self.tags_on.load(Ordering::SeqCst) {
            *st = "on".into();
        }
    }

    /// Downloads the model if it is missing (once, in the background).
    fn ensure_model(self: &Arc<Self>) {
        if model_ready() {
            *self.tags_state.lock() = "on".into();
            return;
        }
        *self.tags_state.lock() = format!("downloading:0/{MODEL_SIZE}");
        // turned off and on again during a download: that one goes on
        if self.tags_downloading.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("photo-model".into()).spawn(move || {
            let _bg = Background::begin();
            let result = (|| -> Result<(), String> {
                if model_ready() {
                    return Ok(());
                }
                let agent = format!("whereti/{}", env!("CARGO_PKG_VERSION"));
                let mut last = 0u64;
                let urls = [MODEL_URL.to_string(), format!("{MODELS_COPY}{MODEL_FILE}")];
                let bytes = fetch_model(&urls, &agent, MODEL_SIZE, MODEL_SHA256, &mut |done| {
                    if (done.abs_diff(last) >= 1 << 20 || done == MODEL_SIZE) && me.tags_on.load(Ordering::SeqCst) {
                        last = done;
                        *me.tags_state.lock() = format!("downloading:{done}/{MODEL_SIZE}");
                    }
                })?;
                let path = model_path();
                let dir = path.parent().expect("models dir");
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let part = path.with_extension("part");
                std::fs::write(&part, &bytes).map_err(|e| e.to_string())?;
                std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
                Ok(())
            })();
            {
                // turned off meanwhile: off it stays
                let mut st = me.tags_state.lock();
                *st = match result {
                    _ if !me.tags_on.load(Ordering::SeqCst) => "off".into(),
                    Ok(()) => "on".into(),
                    Err(e) => format!("error: {e}"),
                };
                me.tags_downloading.store(false, Ordering::SeqCst);
            }
            me.dirty.store(true, Ordering::Relaxed);
            me.poke();
        });
        if spawned.is_err() {
            self.tags_downloading.store(false, Ordering::SeqCst);
            *self.tags_state.lock() = "error: cannot start the download".into();
        }
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
        let _bg = Background::begin();
        // The name index must be ready first: it says which files exist.
        while !self.stopping() && !(shared.state.lock().as_str() == "ready" && shared.index.read().is_some()) {
            std::thread::sleep(Duration::from_millis(500));
        }
        // The content index could not be opened at start (a file locked for
        // a moment): tried again now and then, never removed for it.
        while self.index().is_none() && !self.stopping() {
            match ContentIndex::open(&self.dir) {
                Ok(i) => {
                    *self.known_keys.write() = i.known().map(|k| known_sizes(&k)).unwrap_or_default();
                    let _ = self.index.set(i);
                    shared.log.log("content: the content index could be opened now");
                    self.set_state("starting");
                }
                Err(e) => {
                    self.set_state(&format!("error: the content index cannot be opened ({e})"));
                    let mut wake = self.wake.lock();
                    if !*wake {
                        self.wake_cv.wait_for(&mut wake, REOPEN_EVERY);
                    }
                    *wake = false;
                }
            }
        }
        let Some(index) = self.index() else { return };
        let mut known = match index.known() {
            Ok(k) => k,
            Err(e) => {
                self.set_state(&format!("error: {e}"));
                return;
            }
        };
        *self.known_keys.write() = known_sizes(&known);
        // Once, when OCR arrives: documents that gave no text before (scanned
        // PDFs) are read again. Forgetting them makes the next round redo them.
        let ocr_marker = self.dir.join("ocr-1");
        let places_marker = self.dir.join("places-2"); // 2: again, with the tolerant EXIF reader (0.0.12)
        if known.is_empty() {
            // a new index: nothing indexed before to read again
            let _ = std::fs::write(&ocr_marker, "new index\n");
            let _ = std::fs::write(&places_marker, "new index\n");
        }
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
        let mut sandbox = new_sandbox();
        // how tightly documents are read (restricted token, or lockdown only)
        if let Some(s) = &sandbox {
            shared.log.log(format!("content: document reader: {}", s.sandbox_report()));
        }
        if sandbox.is_none() {
            shared.log.log("content: whereti-extract.exe is missing next to the service; only text files are indexed");
        }
        let mut forced = true;
        let mut last_round = Instant::now();
        while !self.stopping() {
            // changes seen in the journal: a round, at most every DIRTY_GAP
            let dirty = self.dirty.load(Ordering::Relaxed) && last_round.elapsed() >= DIRTY_GAP;
            if forced || dirty || last_round.elapsed() >= SAFETY_ROUND {
                // what changes from now on asks for the next round
                self.dirty.store(false, Ordering::Relaxed);
                last_round = Instant::now();
                // the extractor back (reinstalled, out of quarantine): what waited for it, now
                if sandbox.is_none() {
                    sandbox = new_sandbox();
                    if sandbox.is_some() {
                        shared.log.log("content: whereti-extract.exe is back; the documents that waited for it are read");
                        self.no_sandbox.lock().clear();
                    }
                }
                match self.round(index, &shared, &mut known, sandbox.as_ref()) {
                    Ok(()) => {
                        let all_done = self.status.lock().queued == 0 && !self.models_turn.load(Ordering::Relaxed);
                        if ocr_pass && all_done && !ocr_marker.exists() {
                            let _ = std::fs::write(&ocr_marker, "documents without text were read again with OCR\n");
                        }
                        if places_pass && all_done && !places_marker.exists() {
                            let _ = std::fs::write(&places_marker, "photos were read again for where they were taken\n");
                        }
                    }
                    Err(e) => {
                        shared.log.log(format!("content: {e}"));
                        self.set_state(&format!("error: {e}"));
                    }
                }
                *self.known_keys.write() = known_sizes(&known);
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
        self.save_side();
    }

    /// Writes the properties and the vectors (what changed).
    fn save_side(&self) {
        self.props.lock().save();
        self.vectors.lock().save();
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
        // music and videos: no text, not counted as documents
        *self.media_keys.lock() = wanted.values().filter(|c| c.media_only).map(|c| c.key).collect();
        let crashed = self.crashed.lock().clone();
        // Recognition on: photos not looked at yet by the model are read again;
        // off: photos that carry tags are read again without them. Kept per
        // photo (props `tagged`), so a pass cut short goes on after a restart.
        // While the model fails, the photos wait for it (nothing is read again).
        let want_tags = self.tags_ready();
        let mut tagger = self.tagger_for_round(wanted.values().filter(|c| is_picture(&c.path) && !crashed.contains(&c.key)));
        let can_tag = tagger.is_some();
        // photos' text (OCR): wherever the photo is, when Windows has OCR
        let ocr_on = sandbox.is_some() && whereti_extract::ocr_available();
        {
            let props = self.props.lock();
            let redo: Vec<u64> = wanted
                .values()
                .filter(|c| is_picture(&c.path) && known.contains_key(&c.key) && crate::props::has_props(&ext_of(&c.path)))
                .filter(|c| !crashed.contains(&c.key))
                // read without the model but should be (on), or with it and should not be (off);
                // photos with no properties yet are only redone when recognition is on
                .filter(|c| match props.get(c.key) {
                    Some(p) => (p.tagged != want_tags && (can_tag || !want_tags)) || (ocr_on && !p.ocr && !is_raw(&c.path)),
                    None => (want_tags && can_tag) || (ocr_on && !is_raw(&c.path)),
                })
                .map(|c| c.key)
                .collect();
            if !redo.is_empty() {
                shared.log.log(format!(
                    "content: {} photos are read again ({} recognition)",
                    redo.len(),
                    if want_tags { "with" } else { "without" }
                ));
            }
            for k in redo {
                known.remove(&k);
            }
        }
        let removed: Vec<u64> = known.keys().filter(|k| !wanted.contains_key(k)).copied().collect();
        for k in &removed {
            index.remove(*k);
            known.remove(k);
        }
        // properties and vectors of files gone (or of files never recorded)
        self.props.lock().retain(|k| wanted.contains_key(&k));
        self.vectors.lock().retain(|k| wanted.contains_key(&k));
        self.meaning_text.lock().retain(|k, _| wanted.contains_key(k));
        // search by meaning turned on: the documents already indexed, again
        // for it; not while the model fails (they would be read for nothing)
        let mut meaning_now = self.meaning_usable();
        let meaning_missing: Vec<Candidate> = if meaning_now {
            let vectors = self.vectors.lock();
            wanted
                .values()
                .filter(|c| {
                    !c.photo_only
                        && !c.media_only
                        && !crashed.contains(&c.key)
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
                        && !crashed.contains(&c.key)
                        && crate::props::has_props(&ext_of(&c.path))
                })
                .map(|c| Candidate { props_only: true, media_only: false, photo_only: false, path: c.path.clone(), ..*c })
                .collect()
        };
        let now = system::date_ctx().now;
        // documents that need the missing extractor, unchanged since: they wait for it
        let waiting: HashMap<u64, (u64, u32)> = if sandbox.is_none() { self.no_sandbox.lock().clone() } else { HashMap::new() };
        let mut todo: Vec<Candidate> = wanted
            .into_values()
            .filter(|c| known.get(&c.key).is_none_or(|i| i.size != c.size || i.modified != c.modified))
            // a mailbox that changed: again only once its date moved a day on
            .filter(|c| !is_mailbox(&c.path) || known.get(&c.key).is_none_or(|i| now >= i.modified.saturating_add(MAILBOX_EVERY)))
            .filter(|c| waiting.get(&c.key) != Some(&(c.size, c.modified)))
            .chain(props_missing)
            .chain(meaning_missing)
            .collect();
        let tz = system::date_ctx().tz_offset;
        // Text first: while documents wait to be read, this round reads only
        // their text, quickly; photos and meaning (the slow models) come in
        // the next round, right after. Whatever the user writes becomes
        // searchable in minutes, not after hours of model work.
        let is_later = |c: &Candidate| c.meaning_only || c.photo_only || is_picture(&c.path);
        let text_waiting = todo.iter().any(|c| !is_later(c) && !c.props_only && !c.media_only);
        let models_turn = self.models_turn.swap(false, Ordering::Relaxed);
        let models_later = !models_turn && text_waiting && (meaning_now || todo.iter().any(is_later));
        if models_later {
            todo.retain(|c| !is_later(c));
        }
        // turned off: the text kept for it goes (while the model only fails, it waits)
        if !self.meaning_ready() {
            self.meaning_text.lock().clear();
        }
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
        let (mut since_commit, mut last_commit, mut last_side_save) = (0usize, Instant::now(), Instant::now());
        let total = todo.len();
        let mut indexed = 0usize;
        let mut cpu = system::CpuMeter::new();
        let mut last_cpu_check = Instant::now();
        let mut paused_for: Option<&'static str> = None;
        // the CPU busy since (for BUSY_MAX), and going on slowly despite it
        let (mut busy_since, mut slow): (Option<Instant>, bool) = (None, false);
        let mut tag_round = TagRound::default();
        'files: for (n, c) in todo.into_iter().enumerate() {
            if self.stopping() || *self.wake.lock() {
                break; // stop, or folders changed: plan again
            }
            // Wait while the user games, presents, is on battery or the CPU is busy.
            loop {
                // the CPU is measured over at least a second
                let measured = last_cpu_check.elapsed() >= Duration::from_secs(1) || paused_for.is_some();
                let mut reason = if measured {
                    last_cpu_check = Instant::now();
                    self.pause_reason(&mut cpu)
                } else if system::on_battery() {
                    Some("battery")
                } else if self.recent_hint().0 {
                    Some("fullscreen")
                } else {
                    None
                };
                if measured {
                    match reason {
                        // busy for long (a program at idle priority, a miner): on, slowly
                        Some("busy") => {
                            if busy_since.get_or_insert_with(Instant::now).elapsed() >= BUSY_MAX {
                                if !slow {
                                    shared.log.log("content: the CPU stays busy; indexing goes on slowly");
                                }
                                slow = true;
                                reason = None;
                            }
                        }
                        None => (busy_since, slow) = (None, false),
                        Some(_) => {}
                    }
                }
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
                                self.save_side();
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
            let mut looked = Looked::No;
            let mut kept: Option<Vec<u16>> = None;
            // the file's work; `false`: nothing was read (no rest after it)
            let worked = 'file: {
                // a file that once crashed a parser is not read in-process again
                if crashed.contains(&c.key) {
                    if !c.props_only && !c.meaning_only {
                        let what = Indexed { size: c.size, modified: c.modified, status: Status::Failed };
                        index.put(c.key, what, "").map_err(|e| e.to_string())?;
                        known.insert(c.key, what);
                    }
                    break 'file false;
                }
                // the crash marker is set only around what runs in this process
                // (a sandboxed read cut short by a shutdown is no crash)
                let mark = |on: bool| self.reading(on.then_some(c.key));
                if c.meaning_only {
                    // the model failed meanwhile: not read for nothing
                    if !meaning_now {
                        break 'file false;
                    }
                    // the text kept for it stays until the model has read it
                    let kept = self.meaning_text.lock().get(&c.key).cloned();
                    let pieces = match kept {
                        Some(p) => Some(p),
                        None => extract(&c, sandbox, None, None, &mut Looked::No, &mark)
                            .ok()
                            .map(|(_, text)| whereti_extract::meaning::pieces(&text, crate::meaning::PIECES)),
                    };
                    if let Some(pieces) = pieces {
                        if self.read_meaning(c.key, &pieces) {
                            self.meaning_text.lock().remove(&c.key);
                        } else {
                            meaning_now = false;
                            self.keep_meaning_text(c.key, pieces);
                        }
                    }
                    since_commit += 1;
                } else if !c.props_only {
                    // a photo unchanged since the model looked at it keeps those tags
                    // while the model gives none now (off for a while, failing)
                    if want_tags && is_picture(&c.path) && self.known_keys.read().get(&c.key) == Some(&(c.size, c.modified)) {
                        kept = self.props.lock().get(c.key).filter(|p| p.tagged).map(|p| p.tags.clone());
                    }
                    let read = match extract(&c, sandbox, tagger.as_mut(), kept.as_deref(), &mut looked, &mark) {
                        Ok(r) => r,
                        // gone, locked, no access: in a later round (nothing is kept of it)
                        Err(Skip::Later) => break 'file true,
                        Err(Skip::NoSandbox) => {
                            self.no_sandbox.lock().insert(c.key, (c.size, c.modified));
                            break 'file false;
                        }
                        // may pass (a timeout on a busy PC): once more in a later round
                        Err(Skip::Transient) => {
                            let mut once = self.failed_once.lock();
                            if once.remove(&c.key) != Some((c.size, c.modified)) {
                                once.insert(c.key, (c.size, c.modified));
                                break 'file true;
                            }
                            (Indexed { size: c.size, modified: c.modified, status: Status::Failed }, String::new())
                        }
                    };
                    let (what, text) = read;
                    index.put(c.key, what, &text).map_err(|e| e.to_string())?;
                    known.insert(c.key, what);
                    since_commit += 1;
                    indexed += 1;
                    // read again: the text kept for the model is of an older version
                    self.meaning_text.lock().remove(&c.key);
                    if meaning_now && what.status == Status::Ok && !c.photo_only && !c.media_only && !is_picture(&c.path) {
                        let pieces = whereti_extract::meaning::pieces(&text, crate::meaning::PIECES);
                        if models_later {
                            // for the model, in the next round
                            self.vectors.lock().remove(c.key);
                            self.keep_meaning_text(c.key, pieces);
                        } else if !self.read_meaning(c.key, &pieces) {
                            meaning_now = false; // the model failed: not for the rest of this round
                            self.keep_meaning_text(c.key, pieces);
                        }
                    } else {
                        // a record reused by another kind of file: no old vectors on it
                        self.vectors.lock().remove(c.key);
                    }
                }
                let ext = ext_of(&c.path);
                if crate::props::has_props(&ext) {
                    mark(true);
                    let mut p = crate::props::read(Path::new(&c.path), &ext, tz);
                    mark(false);
                    if c.props_only || c.meaning_only {
                        // only the properties were read: keep what the model saw and the OCR mark
                        if let Some(old) = self.props.lock().get(c.key) {
                            p.tags = old.tags.clone();
                            p.tagged = old.tagged;
                            p.ocr = old.ocr;
                        }
                        since_commit += 1;
                    } else {
                        // tagged only when the model looked at it (now, or before: `kept`)
                        let look = tag_round.note(c.key, std::mem::replace(&mut looked, Looked::No), kept.take());
                        if let Some(e) = &look.model_fails {
                            shared.log.log(format!("content: the photo model fails ({e}); tried again later"));
                            self.tags_failure(e);
                            tagger = None;
                        }
                        if !look.settled.is_empty() {
                            let mut store = self.props.lock();
                            for k in look.settled {
                                if let Some(mut q) = store.get(k).cloned() {
                                    q.tagged = true;
                                    store.set(k, q);
                                }
                            }
                        }
                        (p.tags, p.tagged) = (look.tags, look.tagged);
                        p.ocr = ocr_on && is_picture(&c.path) && !is_raw(&c.path);
                    }
                    self.props.lock().set(c.key, p);
                } else if !c.props_only && !c.meaning_only {
                    // a record reused by another kind of file: no old properties on it
                    self.props.lock().remove(c.key);
                }
                true
            };
            {
                let mut s = self.status.lock();
                s.done = n as u64 + 1;
                s.queued = (total - n - 1) as u64;
            }
            if since_commit >= COMMIT_EVERY_DOCS || (since_commit > 0 && last_commit.elapsed() >= COMMIT_EVERY) {
                index.commit().map_err(|e| e.to_string())?;
                if last_side_save.elapsed() >= SIDE_SAVE_EVERY {
                    self.save_side();
                    last_side_save = Instant::now();
                }
                self.update_counts(index, known);
                since_commit = 0;
                last_commit = Instant::now();
            }
            if !worked {
                continue;
            }
            // Rest at least as long as the file took; a quarter of that
            // while nobody is at the PC (locked, screen saver), four times
            // that while the CPU stays busy. A stop or a new plan ends it.
            let took = started.elapsed();
            let rest = if slow {
                took * 4
            } else if self.recent_hint().1 {
                took / 4
            } else {
                took
            };
            let mut wake = self.wake.lock();
            if !*wake && !self.stopping() {
                self.wake_cv.wait_for(&mut wake, rest.max(Duration::from_millis(5)));
            }
        }
        index.commit().map_err(|e| e.to_string())?;
        if models_later {
            // the vectors are written once, by the round of the models right after (or at stop)
            self.props.lock().save();
        } else {
            self.save_side();
        }
        self.update_counts(index, known);
        // the text is in: photos and meaning now, without waiting for a change
        if models_later && !self.stopping() {
            shared.log.log("content: the documents' text is in; now photos and meaning");
            self.models_turn.store(true, Ordering::Relaxed);
            *self.wake.lock() = true;
        }
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
        // music and videos are in the index (for their properties), not documents
        let media = self.media_keys.lock().iter().filter(|k| known.contains_key(k)).count() as u64;
        let mut s = self.status.lock();
        s.files = index.docs().saturating_sub(media);
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
        let Some(index) = self.index() else { return ContentReply::default() };
        let (words, filters) = split_query(query);
        if words.trim().is_empty() {
            return ContentReply::default();
        }
        let (prop_clauses, filters) = crate::props::split(&filters);
        let with: Option<HashSet<u32>> =
            (!prop_clauses.is_empty()).then(|| self.entries_with_props(idx, &prop_clauses).into_iter().collect());
        let w = idx.weigher_with(active);
        // The best hits, filtered and checked for the caller as they come;
        // deeper while too few are left (filters, other users' documents).
        let mut found: Vec<(f32, u32, String)> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let (mut depth, mut looked) = (SEARCH_DEPTH, 0usize);
        loop {
            let hits = match index.search(&words, depth) {
                Ok(h) => h,
                Err(e) => return ContentReply { note: Some(format!("content search failed: {e}")), ..Default::default() },
            };
            let mut entries = Vec::new();
            let mut text_score: HashMap<u32, f32> = HashMap::new();
            for (k, score) in hits.keys.iter().skip(looked) {
                let (letter, record) = split_key(*k);
                if let Some(e) = idx.volume_index(letter).and_then(|v| idx.live_entry_of(v, record)) {
                    if seen.insert(e) {
                        entries.push(e);
                        text_score.insert(e, *score);
                    }
                }
            }
            looked = hits.keys.len();
            let mut entries = idx.filter(&filters, &entries);
            if let Some(with) = &with {
                entries.retain(|e| with.contains(e));
            }
            let paths: Vec<String> = entries.iter().map(|&e| idx.full_path(e)).collect();
            let allowed = caller.can_read(&paths);
            found.extend(entries.iter().zip(paths).zip(allowed).filter(|(_, ok)| *ok).map(|((&e, path), _)| {
                let used = usage.iter().find(|(p, _)| p.eq_ignore_ascii_case(&path)).map(|u| u.1);
                let score = text_score.get(&e).copied().unwrap_or(0.0) * weight(idx.importance(e, &w), idx.flags(e), used);
                (score, e, path)
            }));
            // all of them seen, or enough for the page (with some to spare:
            // copies become one row), or as deep as a search goes
            let all_seen = hits.keys.len() < depth;
            if all_seen || found.len() >= limit.saturating_mul(2).max(limit.saturating_add(10)) || depth >= MAX_SEARCH_DEPTH {
                break;
            }
            depth = (depth * 4).min(MAX_SEARCH_DEPTH);
        }
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
        let groups = self.group_copies(idx, &found);
        // only what this caller may read and the filters keep: never a count
        // that says how many of someone else's documents hold the words
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
        ContentReply { rows, copies, total: visible, micros: t.elapsed().as_micros() as u64, note }
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
/// Time a search may spend reading files to group copies (the name index
/// is locked meanwhile); the rest are read in the background, for the
/// next search.
const HASH_BUDGET: Duration = Duration::from_millis(150);

/// Remembered content hashes: path -> (size, last write and change time,
/// hash).
type HashCache = Arc<Mutex<HashMap<String, (u64, crate::dupes::Stamp, u64)>>>;

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
        let deadline = Instant::now() + HASH_BUDGET;
        let mut later: Vec<(String, u64)> = Vec::new();
        let mut copy_of: HashMap<usize, usize> = HashMap::new();
        for (&size, same_size) in by_size.iter().filter(|(_, v)| v.len() > 1) {
            let mut first_with: HashMap<u64, usize> = HashMap::new();
            for &k in same_size {
                let (_, _, path) = &found[k];
                let h = match content_hash(&self.hashes, path, size, Instant::now() < deadline) {
                    Hashed::Yes(h) => h,
                    Hashed::NotNow => {
                        later.push((path.clone(), size));
                        continue;
                    }
                    Hashed::No => continue,
                };
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
        if !later.is_empty() && !self.hashing.swap(true, Ordering::SeqCst) {
            let (cache, busy) = (self.hashes.clone(), self.hashing.clone());
            let spawned = std::thread::Builder::new().name("copy-hashes".into()).spawn(move || {
                let _bg = Background::begin();
                for (path, size) in later {
                    content_hash(&cache, &path, size, true);
                }
                busy.store(false, Ordering::SeqCst);
            });
            if spawned.is_err() {
                self.hashing.store(false, Ordering::SeqCst);
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
}

enum Hashed {
    Yes(u64),
    /// Not remembered, and no time to read it now.
    NotNow,
    /// Cannot be read (gone, locked, changed size).
    No,
}

/// The hash of a whole file, remembered while the file stays as it was
/// (its size, last write and change time); read only when `read`. Opened
/// only at its real path (no link swapped in after the access check).
fn content_hash(cache: &HashCache, path: &str, size: u64, read: bool) -> Hashed {
    use std::io::Read;
    let Ok(f) = whereti_extract::worker::open_real(Path::new(path)) else { return Hashed::No };
    let Some(stamp) = crate::dupes::stamp(&f) else { return Hashed::No };
    if let Some(&(s, st, h)) = cache.lock().get(path) {
        if s == size && st == stamp {
            return Hashed::Yes(h);
        }
    }
    if !read {
        return Hashed::NotNow;
    }
    let mut bytes = Vec::with_capacity(size as usize);
    if f.take(size + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 != size {
        return Hashed::No;
    }
    let h = xxhash_rust::xxh3::xxh3_64(&bytes);
    let mut cache = cache.lock();
    if cache.len() > 20_000 {
        cache.clear();
    }
    cache.insert(path.to_string(), (size, stamp, h));
    Hashed::Yes(h)
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
            files_in(idx, &self.folders(), &[]).into_values().filter(|c| !known.contains_key(&c.key) && !c.photo_only && !c.media_only && !is_mailbox(&c.path)).collect();
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
                    if let Ok((_, text)) = extract(c, sandbox.as_ref(), None, None, &mut Looked::No, &|_| {}) {
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
    /// What was read from the file with this key (props.rs).
    pub fn file_info(&self, key: u64) -> whereti_ipc::FileInfo {
        let Some(p) = self.props.lock().get(key).cloned() else { return Default::default() };
        whereti_ipc::FileInfo {
            taken: p.taken,
            width: p.width,
            height: p.height,
            duration: p.duration,
            camera: p.camera,
            artist: p.artist,
            album: p.album,
            author: p.author,
            tags: p.tags,
        }
    }

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

fn is_mailbox(path: &str) -> bool {
    ext_of(path) == "pst"
}

fn is_raw(path: &str) -> bool {
    whereti_query::kinds::RAW.contains(&ext_of(path).as_str())
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
                let photos = PHOTO_EXTS.iter().chain(whereti_query::kinds::RAW).map(|e| e.to_string()).collect();
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
    /// A photo outside the content folders: where it was taken, what is in
    /// it and (not for RAW) its text.
    photo_only: bool,
    /// Music or a video: no text, only its properties.
    media_only: bool,
    /// Already indexed: only its properties are read now.
    props_only: bool,
    /// Already indexed: read again only for its meaning.
    meaning_only: bool,
}

/// Photos a camera or phone writes its GPS position into.
const PHOTO_EXTS: &[&str] = whereti_query::kinds::PHOTOS;
/// What cameras and phones write (PHOTOS without PNG and WebP).
const CAMERA_EXTS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "heic", "heif", "hif", "avif", "jxl", "tif", "tiff"];

/// Keys of files whose in-process reading crashed the service: the one
/// named in reading.txt at start (it was being read when the service went
/// down) is added to crash.txt for good.
fn load_crashed(dir: &Path, log: &dyn Fn(String)) -> HashSet<u64> {
    let list = dir.join("crash.txt");
    let mut set: HashSet<u64> =
        std::fs::read_to_string(&list).map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect()).unwrap_or_default();
    let reading = dir.join("reading.txt");
    if let Some(k) = std::fs::read_to_string(&reading).ok().and_then(|t| t.trim().parse::<u64>().ok()) {
        if set.insert(k) {
            log(format!("content: file {k} stopped the service while it was read; it is skipped from now on"));
            let text: String = set.iter().map(|k| format!("{k}\n")).collect();
            let _ = std::fs::write(&list, text);
        }
    }
    let _ = std::fs::write(&reading, "");
    set
}

/// Where a photo was taken, as text for the index: "Rome, Lazio, Italy"
/// and then every name of the place. `None` without a GPS position.
fn place_text(path: &Path, ext: &str) -> Option<String> {
    let raw = whereti_query::kinds::RAW.contains(&ext);
    if !PHOTO_EXTS.contains(&ext) && !raw {
        return None;
    }
    let bytes = crate::props::photo_bytes(path, ext).ok()?;
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
    // photos anywhere on the disks (not in programs, caches, hidden or
    // system folders): where they were taken and what is in them
    {
        // anywhere: what cameras and phones write; PNG and WebP (screenshots,
        // game and app images) only in the Pictures and content folders
        let camera: Vec<String> =
            CAMERA_EXTS.iter().chain(whereti_query::kinds::RAW).map(|e| e.to_string()).collect();
        let anywhere = Query { clauses: vec![file(), Clause { negated: false, any_of: vec![Term::Ext(camera)] }] };
        let mut entries: Vec<u32> = idx.matching(&anywhere);
        let places: Vec<Term> = pictures
            .iter()
            .chain(folders)
            .map(|f| {
                let mut p = fold(f.trim_end_matches('\\'));
                p.push('\\');
                Term::Path(p)
            })
            .collect();
        if !places.is_empty() {
            let screens = Query {
                clauses: vec![
                    file(),
                    Clause { negated: false, any_of: places },
                    Clause { negated: false, any_of: vec![Term::Ext(vec!["png".into(), "webp".into()])] },
                ],
            };
            entries.extend(idx.matching(&screens));
        }
        let entries: Vec<u32> = entries.into_iter().filter(|&e| idx.flags(e) & SKIP == 0 && idx.size(e) >= MIN_PICTURE).collect();
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
    // Outlook data files (.pst) anywhere, AppData included; never cloud-only
    // ones (an .ost, the cache of an Exchange account, is not readable)
    {
        let mailboxes = Query {
            clauses: vec![file(), Clause { negated: false, any_of: vec![Term::Ext(vec!["pst".into()])] }],
        };
        let entries: Vec<u32> = idx.matching(&mailboxes).into_iter().filter(|&e| idx.flags(e) & FLAG_CLOUD == 0).collect();
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
                media_only: false,
                props_only: false,
                meaning_only: false,
            });
        }
    }
    // music and videos anywhere on the disks (not in programs, caches,
    // hidden or system folders): their properties. Outside the user's own
    // folders only from 1 MB up (games keep thousands of small sounds).
    {
        let exts = whereti_query::kinds::AUDIO_TAGGED.iter().chain(whereti_query::kinds::VIDEO_TAGGED).map(|e| e.to_string()).collect();
        let media = Query { clauses: vec![file(), Clause { negated: false, any_of: vec![Term::Ext(exts)] }] };
        let own: Vec<String> = folders
            .iter()
            .chain(pictures)
            .cloned()
            .chain(media_folders())
            .map(|f| {
                let mut p = fold(f.trim_end_matches('\\'));
                p.push('\\');
                p
            })
            .collect();
        let entries: Vec<u32> = idx
            .matching(&media)
            .into_iter()
            .filter(|&e| idx.flags(e) & SKIP == 0)
            .filter(|&e| idx.size(e) >= 1 << 20 || {
                let p = fold(&idx.full_path(e));
                own.iter().any(|f| p.starts_with(f.as_str()))
            })
            .collect();
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
        clauses: vec![Clause {
            negated: false,
            any_of: vec![Term::Ext(PHOTO_EXTS.iter().chain(whereti_query::kinds::RAW).map(|e| e.to_string()).collect())],
        }],
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

/// What the photo model made of a photo.
#[derive(Debug)]
enum Looked {
    /// It did not look (off, failing, the photo unreadable now).
    No,
    /// Its labels (none found: empty).
    Tags(Vec<u16>),
    /// Windows cannot decode the picture (HEIC without its codec, CMYK):
    /// the model answered, so it is looked at, with nothing found.
    Unreadable,
    /// Its worker failed on it (no answer, stopped, the model did not load).
    Failed(String),
}

/// The worker's answer for a picture Windows cannot decode (the text of
/// whereti_extract::tags::Tagger::embed), unlike a failure of the model.
fn undecodable(e: &str) -> bool {
    e.starts_with("cannot decode the photo")
}

/// The words a photo's tags add to its text (labels as in props `tags`).
fn tags_text(tags: &[u16]) -> String {
    let tags: Vec<(usize, f32)> = tags.iter().map(|&t| (t as usize, 0.0)).collect();
    whereti_extract::tags::index_text(&tags)
}

/// What is in a photo, as words for the index ("dog Hund perro chien
/// cane pies cachorro câine"); `None` without tags.
fn photo_tags(path: &Path, tagger: &mut TagWorker, looked: &mut Looked) -> Option<String> {
    let bytes = match read_shared(path, MAX_TAGGED) {
        Ok(b) => b,
        // too large to look at: as good as looked at, nothing found
        Err(SandboxError::TooLarge(_)) => {
            *looked = Looked::Tags(Vec::new());
            return None;
        }
        Err(_) => return None,
    };
    match tagger.tags(&bytes) {
        Ok(tags) => {
            *looked = Looked::Tags(tags.iter().map(|(i, _)| *i as u16).collect());
            (!tags.is_empty()).then(|| whereti_extract::tags::index_text(&tags))
        }
        Err(e) if undecodable(&e) => {
            *looked = Looked::Unreadable;
            None
        }
        Err(e) => {
            *looked = Looked::Failed(e);
            None
        }
    }
}

/// The photo model over one round: whether it worked, its failures in a
/// row, and the photos its worker failed on before it worked.
#[derive(Default)]
struct TagRound {
    worked: bool,
    fails: u32,
    unsure: Vec<u64>,
}

/// What a photo's properties keep of the model's look.
#[derive(Debug, PartialEq)]
struct Look {
    tags: Vec<u16>,
    tagged: bool,
    /// The worker fails three times in a row: the model, not the photos;
    /// it is not used for the rest of the round.
    model_fails: Option<String>,
    /// Photos it failed on before it worked: those photos were the trouble,
    /// they count as looked at.
    settled: Vec<u64>,
}

impl TagRound {
    /// `kept`: the tags the model gave this photo, unchanged since, before
    /// (`None`: it never looked at it). They stay while it gives none now.
    fn note(&mut self, key: u64, looked: Looked, kept: Option<Vec<u16>>) -> Look {
        let had = kept.is_some();
        let mut look = Look { tags: kept.unwrap_or_default(), tagged: had, model_fails: None, settled: Vec::new() };
        match looked {
            Looked::Tags(tags) => {
                if !self.worked {
                    look.settled = std::mem::take(&mut self.unsure);
                }
                (self.worked, self.fails) = (true, 0);
                (look.tags, look.tagged) = (tags, true);
            }
            Looked::Unreadable => look.tagged = true,
            Looked::Failed(e) => {
                self.fails += 1;
                if self.fails >= 3 {
                    look.model_fails = Some(e);
                } else if self.worked {
                    look.tagged = true;
                } else if !had {
                    self.unsure.push(key);
                }
            }
            Looked::No => {}
        }
        look
    }
}

/// The vectors of a document's pieces. A piece that fails while `probe`
/// (one small question) still works is left out: the document's trouble.
/// `Err` when the probe fails too: the model's.
fn embed_pieces(
    pieces: &[String],
    embed: &mut dyn FnMut(&str) -> Result<Vec<i8>, String>,
    probe: &mut dyn FnMut() -> Result<(), String>,
) -> Result<Vec<Vec<i8>>, String> {
    let mut vectors = Vec::new();
    for piece in pieces {
        match embed(piece) {
            Ok(v) => vectors.push(v),
            Err(_) => probe()?,
        }
    }
    Ok(vectors)
}

/// Why a file was not read now.
enum Skip {
    /// Gone, locked, no access: in a later round.
    Later,
    /// It needs whereti-extract.exe, which is missing.
    NoSandbox,
    /// The extractor failed in a way that may pass (out of time on a busy
    /// PC, could not start): once more in a later round.
    Transient,
}

/// Reads one file. `tagger`: recognise what is in photos (when on).
/// `kept`: the tags the model gave the photo before, which stay when it
/// gives none now. `looked`: what the photo model made of a photo.
/// `mark`: called with `true` before and `false` after parsing in this
/// process.
fn extract(
    c: &Candidate,
    sandbox: Option<&Sandbox>,
    tagger: Option<&mut TagWorker>,
    kept: Option<&[u16]>,
    looked: &mut Looked,
    mark: &dyn Fn(bool),
) -> Result<(Indexed, String), Skip> {
    let what = |status| Indexed { size: c.size, modified: c.modified, status };
    if c.media_only {
        return Ok((what(Status::Empty), String::new()));
    }
    let path = Path::new(&c.path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let kind = whereti_extract::kind_of(&ext).ok_or(Skip::Later)?;
    // a photo: where it was taken and what is in it
    let place = if kind == Kind::Image {
        let tags = tagger.and_then(|t| photo_tags(path, t, looked));
        let tags = match kept {
            Some(k) if tags.is_none() && !matches!(looked, Looked::Tags(_)) && !k.is_empty() => Some(tags_text(k)),
            _ => tags,
        };
        mark(true);
        let place = place_text(path, &ext);
        mark(false);
        match (place, tags) {
            (Some(p), Some(t)) => Some(format!("{p}\n{t}")),
            (p, t) => p.or(t),
        }
    } else {
        None
    };
    // a RAW photo: where it was taken and what is in it, not its text
    // (decoding a camera file for OCR costs too much); the same for any
    // photo when the sandbox is missing
    if (c.photo_only && whereti_query::kinds::RAW.contains(&ext.as_str())) || (kind == Kind::Image && sandbox.is_none()) {
        return Ok(match place {
            Some(text) => (what(Status::Ok), text),
            None => (what(Status::Empty), String::new()),
        });
    }
    let result = if kind == Kind::Text {
        // Plain text needs no parser: read the head in-process.
        read_head(path, TEXT_HEAD).and_then(|b| whereti_extract::extract(&ext, &b, MAX_CHARS).map_err(SandboxError::Extract))
    } else if kind == Kind::Mailbox {
        // Outlook's mailbox, however large: the worker reads it from the open
        // file, newest messages first, up to a larger budget
        sandbox.ok_or(Skip::NoSandbox)?.extract_file(path, MAILBOX_CHARS)
    } else if c.size > MAX_DOCUMENT && ext == "7z" {
        // a large 7z: its names, from its list at the end (and its start header)
        let sandbox = sandbox.ok_or(Skip::NoSandbox)?;
        read_head(path, 32)
            .and_then(|head| read_tail(path, ZIP_TAIL).map(|tail| (head, tail)))
            .and_then(|(head, tail)| {
                let len = std::fs::metadata(path).map(|m| m.len()).map_err(SandboxError::Io)?;
                sandbox.extract_bytes(whereti_extract::SEVENZ_ENDS, whereti_extract::sevenz_ends(len, &head, &tail), MAX_CHARS)
            })
    } else if c.size > MAX_DOCUMENT && ext == "zip" {
        // a large ZIP: its names, from the directory at its end
        let sandbox = sandbox.ok_or(Skip::NoSandbox)?;
        read_tail(path, ZIP_TAIL).and_then(|b| sandbox.extract_bytes(whereti_extract::ZIP_END, b, MAX_CHARS))
    } else if c.size > MAX_DOCUMENT {
        // too large to read its text: a photo still keeps where it was taken
        return Ok(match place {
            Some(text) => (what(Status::Ok), text),
            None => (what(Status::TooLarge), String::new()),
        });
    } else {
        sandbox.ok_or(Skip::NoSandbox)?.extract_file(path, MAX_CHARS)
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
    Ok(match result {
        Ok(e) if e.text.trim().is_empty() => (what(Status::Empty), String::new()),
        Ok(e) => (what(Status::Ok), e.text),
        Err(SandboxError::Io(_)) => return Err(Skip::Later),
        Err(SandboxError::Failed(_)) => return Err(Skip::Transient),
        // the extension promised a format the file is not in (a Tcl .msg): no text, not a failure
        Err(SandboxError::Extract(ExtractError::Unsupported)) => (what(Status::Empty), String::new()),
        Err(SandboxError::Extract(ExtractError::Protected)) => (what(Status::Protected), String::new()),
        Err(SandboxError::TooLarge(_)) => (what(Status::TooLarge), String::new()),
        Err(_) => (what(Status::Failed), String::new()),
    })
}

/// The extractor next to the service, with the indexing's limits.
fn new_sandbox() -> Option<Sandbox> {
    Sandbox::beside_current_exe().map(|mut s| {
        s.timeout = Duration::from_secs(30);
        s.max_file = MAX_DOCUMENT;
        s
    })
}

/// Size and date of each indexed file, by key.
fn known_sizes(known: &HashMap<u64, Indexed>) -> HashMap<u64, (u64, u32)> {
    known.iter().map(|(k, i)| (*k, (i.size, i.modified))).collect()
}

/// (busy, away) from the hints of the last `HINT_VALID`, from any user
/// session: busy when any says so, away only when every one does.
fn combine_hints(hints: &[(Instant, bool, bool)]) -> (bool, bool) {
    let recent: Vec<&(Instant, bool, bool)> = hints.iter().filter(|h| h.0.elapsed() < HINT_VALID).collect();
    (recent.iter().any(|h| h.1), !recent.is_empty() && recent.iter().all(|h| h.2))
}

/// The last `max` bytes of a file.
fn read_tail(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = whereti_extract::worker::open_real(path).map_err(SandboxError::Io)?;
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
            let f = whereti_extract::worker::open_real(path).map_err(SandboxError::Io)?;
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

    #[test]
    fn hints_from_several_sessions_combine() {
        let now = Instant::now();
        let old = now - Duration::from_secs(60);
        // a locked session away, the one at the screen in a game: busy, not away
        assert_eq!(combine_hints(&[(now, false, true), (now, true, false)]), (true, false));
        // every session away: away
        assert_eq!(combine_hints(&[(now, false, true), (now, false, true)]), (false, true));
        // old hints count for nothing
        assert_eq!(combine_hints(&[(old, true, false), (now, false, true)]), (false, true));
        assert_eq!(combine_hints(&[]), (false, false));
    }

    /// A name index of one folder, C:\docs, with these files (records from 100).
    fn docs_index(names: &[String]) -> NameIndex {
        use whereti_ntfs::mft::{MftEntry, MftScan};
        let mut scan = MftScan::default();
        let entry = |record: u64, parent: u64, is_dir: bool| MftEntry {
            record,
            parent,
            is_dir,
            size: 100,
            modified: 133_000_000_000_000_000,
            seq: 1,
            ..Default::default()
        };
        scan.push(entry(5, 5, true), ".");
        scan.push(entry(20, 5, true), "docs");
        for (i, n) in names.iter().enumerate() {
            scan.push(entry(100 + i as u64, 20, false), n);
        }
        let vols = vec![whereti_core::VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 1, scan }];
        whereti_core::build(vols, &whereti_core::Exclusions::defaults(None)).0
    }

    #[test]
    fn filters_find_what_ranks_below_the_first_hits() {
        // 600 short notes rank above 3 long PDFs for "alfa": ext:pdf must still find the PDFs
        let mut names: Vec<String> = (0..600).map(|i| format!("note{i}.txt")).collect();
        names.extend((0..3).map(|i| format!("long{i}.pdf")));
        let idx = docs_index(&names);
        let dir = std::env::temp_dir().join(format!("whereti-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let content = Content::open(&dir, &|_| {});
        let index = content.index().unwrap();
        let filler = "cuvant ".repeat(300);
        for (i, n) in names.iter().enumerate() {
            let text = if n.ends_with(".pdf") { format!("alfa {filler}") } else { "alfa".to_string() };
            index.put(key('C', 100 + i as u64), Indexed { size: 100, modified: 1, status: Status::Ok }, &text).unwrap();
        }
        index.commit().unwrap();
        let ctx = DateCtx { now: 0, tz_offset: 0 };
        let reply = content.search(&idx, &parse("alfa ext:pdf", &ctx), 50, &[], Arc::new(Vec::new()), &whereti_ipc::Trusted);
        assert_eq!(reply.rows.len(), 3, "{:?}", reply.rows.iter().map(|r| &r.path).collect::<Vec<_>>());
        assert_eq!(reply.total, 3);
        // without filters: a page of the best, and no count beyond what was seen
        let reply = content.search(&idx, &parse("alfa", &ctx), 50, &[], Arc::new(Vec::new()), &NotSecrets);
        assert_eq!(reply.rows.len(), 50);
        assert!(reply.total <= SEARCH_DEPTH as u64);
        drop(content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picture_windows_cannot_decode_is_not_a_model_failure() {
        assert!(undecodable("cannot decode the photo: The component cannot be found."));
        assert!(!undecodable("no answer in 20 s") && !undecodable("cannot load the model: bad file"));
        let mut round = TagRound::default();
        // undecodable pictures, however many: looked at, nothing found, the model stays on
        for k in 0..5 {
            let look = round.note(k, Looked::Unreadable, None);
            assert_eq!((look.tags, look.tagged, look.model_fails), (vec![], true, None));
        }
        // the worker failing before it ever worked: the photo waits; three in a row: the model
        assert!(!round.note(10, Looked::Failed("no answer in 20 s".into()), None).tagged);
        assert!(round.note(11, Looked::Unreadable, None).model_fails.is_none());
        assert!(round.note(12, Looked::Failed("no answer in 20 s".into()), None).model_fails.is_none());
        let look = round.note(13, Looked::Failed("the tagger stopped".into()), Some(vec![4]));
        assert_eq!(look.model_fails.as_deref(), Some("the tagger stopped"));
        // tags it gave before stay while it gives none
        assert_eq!((look.tags, look.tagged), (vec![4], true));
        let look = round.note(14, Looked::No, Some(vec![2, 9]));
        assert_eq!((look.tags, look.tagged), (vec![2, 9], true));
        assert_eq!(round.note(15, Looked::No, None), Look { tags: vec![], tagged: false, model_fails: None, settled: vec![] });
    }

    #[test]
    fn photos_the_worker_failed_on_count_as_looked_at_once_it_works() {
        let mut round = TagRound::default();
        assert!(!round.note(1, Looked::Failed("no answer in 20 s".into()), None).tagged);
        // it works: the photo it failed on was the trouble
        let look = round.note(2, Looked::Tags(vec![7]), None);
        assert_eq!((look.tags, look.tagged, look.settled), (vec![7], true, vec![1]));
        // once it worked, a failure is the photo's, and the count starts again
        assert!(round.note(3, Looked::Failed("no answer in 20 s".into()), None).tagged);
        assert!(round.note(4, Looked::Failed("no answer in 20 s".into()), None).model_fails.is_none());
        // new tags replace the ones kept
        assert_eq!(round.note(5, Looked::Tags(vec![]), Some(vec![3])).tags, Vec::<u16>::new());
    }

    #[test]
    fn a_piece_the_model_cannot_read_is_the_documents_trouble() {
        let pieces: Vec<String> = ["one", "bad", "three"].map(String::from).to_vec();
        let embed = &mut |p: &str| if p == "bad" { Err("bad piece".to_string()) } else { Ok(vec![p.len() as i8]) };
        // the model still answers: the document keeps the pieces that could be read
        assert_eq!(embed_pieces(&pieces, embed, &mut || Ok(())), Ok(vec![vec![3], vec![5]]));
        // it does not: the model's failure, nothing is kept
        assert_eq!(embed_pieces(&pieces, embed, &mut || Err("no answer in 20 s".into())), Err("no answer in 20 s".into()));
        assert_eq!(embed_pieces(&[], embed, &mut || Err("never asked".into())), Ok(vec![]));
    }

    #[test]
    fn a_file_that_crashed_the_service_is_skipped_for_good() {
        let dir = std::env::temp_dir().join(format!("whereti-crash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reading.txt"), "42").unwrap();
        let set = load_crashed(&dir, &|_| {});
        assert!(set.contains(&42));
        // the next start remembers it, and nothing new is blamed
        let again = load_crashed(&dir, &|_| {});
        assert_eq!(again.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod copy_tests {
    /// Hugging Face gone (a revision that does not exist): the copy on GitHub
    /// is used, checked by size and SHA-256. Needs the network:
    /// `cargo test -p whereti-engine -- --ignored`.
    #[test]
    #[ignore]
    fn the_models_copy_steps_in_when_hugging_face_fails() {
        let f = &crate::meaning::FILES[1];
        let urls = [
            "https://huggingface.co/Xenova/multilingual-e5-small/resolve/0000000000000000000000000000000000000000/tokenizer.json".to_string(),
            format!("{}{}", super::MODELS_COPY, f.name),
        ];
        let bytes = super::fetch_model(&urls, "whereti-test", f.size, f.sha256, &mut |_| {}).unwrap();
        assert_eq!(bytes.len() as u64, f.size);
        // a wrong hash is refused from every source
        assert!(super::fetch_model(&urls[1..], "whereti-test", f.size, &"0".repeat(64), &mut |_| {}).is_err());
    }
}
