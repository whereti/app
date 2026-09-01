//! The engine the service runs (PLAN.md §3, §7, §8): one index in memory,
//! searched by any number of clients at once, kept current by one worker.
//!
//! - Searches take a read lock for the few milliseconds a search lasts.
//! - The worker is the only writer. Slow work (reading the journal and MFT
//!   records, compaction, rescans) happens without the write lock; the write
//!   lock is held only to apply prefetched states or to swap in a new index.
//! - Heavy maintenance runs on background-priority threads (CPU and disk).
//! - Nothing is lost on a crash: the journal position is saved with the
//!   index, and the next start replays everything after it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use whereti_core::{build, snapshot, ApplyStats, Exclusions, NameIndex, SearchOptions, SortBy, VolumeScan};
use whereti_ipc::{
    Caller, ConfigReply, Request, Response, Row, SearchReply, Sort, StatusReply, VolumeStatus, PROTOCOL_VERSION,
};
use whereti_ntfs::Volume;

use crate::content::{self, Content};
use crate::log::Logger;
use crate::sync::{self, Batch, JournalMsg, JournalRead, Prefetched};
use crate::system::{self, Background};
use crate::removal::Removal;
use crate::walk::{self, Drive, WALKED};

/// Re-read files still open for writing this often.
const OPEN_REFRESH: Duration = Duration::from_secs(30);
/// Collect journal records for this long before applying them as one batch.
const BATCH_WINDOW: Duration = Duration::from_millis(250);
/// Daily check only after this long without file changes.
const IDLE_FOR_RECONCILE: Duration = Duration::from_secs(60);
/// Compact when renamed/added/removed entries exceed this share of the index.
const COMPACT_AT: f64 = 0.01;
/// How often the list of drives is checked (a USB stick plugged in or out).
const DRIVE_CHECK: Duration = Duration::from_secs(3);
/// Drives without a journal are walked again at most this often...
const WALK_MIN: Duration = Duration::from_secs(60);
/// ...and at least this often; in between, by how long a walk takes.
const WALK_MAX: Duration = Duration::from_secs(30 * 60);
/// The most results one search may return.
/// How long one `dupe:` search compares files.
const DUPES_BUDGET: Duration = Duration::from_secs(6);
const MAX_LIMIT: u32 = 10_000;
/// Save once the journal has moved this share of its size since the last save,
/// so a restart can always replay instead of rescanning.
const SAVE_AT_JOURNAL_SHARE: f64 = 0.25;

#[derive(Debug, Clone)]
pub struct Config {
    pub index_path: PathBuf,
    /// Longest time between saves while something changes. Saves are rare on
    /// purpose: each one compacts and writes the whole index (~265 MB for 5M
    /// entries), while replaying the journal at startup takes milliseconds.
    pub save_every: Duration,
    /// Full rescan as a safety net (PLAN.md §7.2); `None` disables it.
    pub reconcile_every: Option<Duration>,
    pub threads: usize,
    pub include_windows: bool,
    /// Log every applied batch (console mode).
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            index_path: system::default_index_path(),
            save_every: Duration::from_secs(6 * 3600),
            reconcile_every: Some(Duration::from_secs(24 * 3600)),
            threads: sync::default_threads(),
            include_windows: false,
            verbose: false,
        }
    }
}

pub enum Msg {
    Journal(JournalMsg),
    /// New exclusion paths: rebuild the index with them.
    Reconfigure(Vec<String>),
    /// Windows asks to remove this drive: close every handle on it, then
    /// answer on the channel (removal.rs).
    Release(char, Option<mpsc::Sender<()>>),
    /// The removal was cancelled: open the drive again.
    Reacquire(char),
    Stop,
}

impl From<JournalMsg> for Msg {
    fn from(m: JournalMsg) -> Msg {
        Msg::Journal(m)
    }
}

pub(crate) struct Shared {
    pub(crate) index: RwLock<Option<NameIndex>>,
    pub(crate) state: Mutex<String>,
    started: Instant,
    pub(crate) last_change: Mutex<Option<Instant>>,
    open_files: AtomicU64,
    pub(crate) log: Logger,
    /// Told about changes, so it re-plans only when documents changed.
    pub(crate) content: std::sync::OnceLock<Arc<Content>>,
    /// Where the user works lately (PLAN.md §56).
    pub(crate) activity: crate::activity::Activity,
}

impl Shared {
    fn set_state(&self, s: impl Into<String>) {
        *self.state.lock() = s.into();
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Msg>,
    worker: Mutex<Option<JoinHandle<()>>>,
    content: Arc<Content>,
    content_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    pub fn start(cfg: Config, log: Logger) -> Arc<Engine> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            index: RwLock::new(None),
            state: Mutex::new("starting".into()),
            started: Instant::now(),
            last_change: Mutex::new(None),
            open_files: AtomicU64::new(0),
            log,
            content: std::sync::OnceLock::new(),
            // beside the index (ProgramData\whereti for the service)
            activity: crate::activity::Activity::open(cfg.index_path.parent().unwrap_or(std::path::Path::new("."))),
        });
        let content_dir = if cfg.index_path == system::default_index_path() {
            let _ = system::ensure_data_dir();
            system::data_dir().join("content")
        } else {
            cfg.index_path.with_file_name("content")
        };
        let content = Content::open(&content_dir, &|m| shared.log.log(m));
        let _ = shared.content.set(content.clone());
        let worker = {
            let (shared, tx) = (shared.clone(), tx.clone());
            std::thread::Builder::new()
                .name("engine".into())
                .spawn(move || Worker::new(shared, cfg, tx).run(rx))
                .expect("engine thread")
        };
        let content_thread = {
            let (content, shared) = (content.clone(), shared.clone());
            std::thread::Builder::new().name("content".into()).spawn(move || content.run(shared)).expect("content thread")
        };
        Arc::new(Engine {
            shared,
            tx,
            worker: Mutex::new(Some(worker)),
            content,
            content_thread: Mutex::new(Some(content_thread)),
        })
    }

    /// Saves (if needed) and stops the worker and the content indexer.
    pub fn stop(&self) {
        self.shared.activity.save();
        self.content.stop();
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.worker.lock().take() {
            let _ = h.join();
        }
        if let Some(h) = self.content_thread.lock().take() {
            let _ = h.join();
        }
    }

    pub fn state(&self) -> String {
        self.shared.state.lock().clone()
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        self.shared.log.log(msg);
    }

    pub fn handle(&self, req: Request, caller: &dyn Caller) -> Response {
        match req {
            Request::Hello { .. } => Response::Hello {
                version: PROTOCOL_VERSION,
                server: format!("whereti {}", env!("CARGO_PKG_VERSION")),
            },
            Request::Search(r) if crate::dupes::wanted(&whereti_query::parse(&r.query, &system::date_ctx())) => {
                let t = Instant::now();
                let query = whereti_query::parse(&r.query, &system::date_ctx());
                // sizes from the index (held briefly), then the files compared without it
                let candidates = {
                    let guard = self.shared.index.read();
                    let Some(idx) = guard.as_ref() else {
                        return Response::Search(SearchReply {
                            warnings: vec![format!("the index is not ready yet ({})", self.state())],
                            ..Default::default()
                        });
                    };
                    // property conditions (camera:, artist:…) narrow the files first
                    let (prop_clauses, _) = crate::props::split(&query);
                    let within: Option<std::collections::HashSet<u32>> =
                        (!prop_clauses.is_empty()).then(|| self.content.entries_with_props(idx, &prop_clauses).into_iter().collect());
                    crate::dupes::size_groups(idx, &query, within.as_ref())
                };
                let found = crate::dupes::verify(candidates, DUPES_BUDGET, r.limit.min(MAX_LIMIT) as usize, caller);
                let mut warnings = Vec::new();
                if crate::dupes::unsupported(&query) {
                    warnings.push("dupe: compares files by name, size and content; content: and loc: are not applied to it".to_string());
                }
                if found.out_of_time {
                    warnings.push(format!(
                        "the largest {} of {} sets of same-size files were compared so far; search again to go on, or narrow it (size:, ext:, path:)",
                        found.checked, found.total
                    ));
                }
                Response::Search(SearchReply {
                    total: found.groups.len() as u64,
                    groups: found.groups.iter().map(|g| g.len() as u32).collect(),
                    rows: found.groups.into_iter().flatten().collect(),
                    warnings,
                    micros: t.elapsed().as_micros() as u64,
                    close: 0,
                })
            }
            Request::Search(r) => {
                let t = Instant::now();
                let guard = self.shared.index.read();
                let Some(idx) = guard.as_ref() else {
                    return Response::Search(SearchReply {
                        warnings: vec![format!("the index is not ready yet ({})", self.state())],
                        ..Default::default()
                    });
                };
                let query = whereti_query::parse(&r.query, &system::date_ctx());
                let opts = SearchOptions {
                    limit: r.limit.min(MAX_LIMIT) as usize,
                    sort: match r.sort {
                        Sort::Relevance => SortBy::Relevance,
                        Sort::Name => SortBy::Name,
                        Sort::Size => SortBy::Size,
                        Sort::Modified => SortBy::Modified,
                    },
                };
                // taken:, camera:, artist:…: among the files with those properties
                let (prop_clauses, rest) = crate::props::split(&query);
                let res = if prop_clauses.is_empty() {
                    idx.search_ranked(&query, &opts, &r.usage, self.shared.activity.ranking(idx))
                } else if prop_clauses.iter().all(|c| c.negated) {
                    // only "not this camera / artist": every file, minus those that have it
                    // (files whose properties were never read are not excluded)
                    let positive: Vec<whereti_query::Clause> =
                        prop_clauses.iter().map(|c| whereti_query::Clause { negated: false, any_of: c.any_of.clone() }).collect();
                    let excluded: std::collections::HashSet<u32> =
                        positive.iter().flat_map(|c| self.content.entries_with_props(idx, std::slice::from_ref(c))).collect();
                    let wide = SearchOptions { limit: opts.limit + excluded.len().min(10_000), ..opts };
                    let mut res = idx.search_ranked(&rest, &wide, &r.usage, self.shared.activity.ranking(idx));
                    let before = res.hits.len();
                    res.hits.retain(|h| !excluded.contains(h));
                    res.total = res.total.saturating_sub(before - res.hits.len());
                    res.hits.truncate(opts.limit);
                    res.close = res.close.min(res.hits.len());
                    res
                } else {
                    let within = self.content.entries_with_props(idx, &prop_clauses);
                    idx.search_within(&rest, &opts, &r.usage, self.shared.activity.ranking(idx), &within)
                };
                let rows = res
                    .hits
                    .iter()
                    .map(|&i| Row {
                        path: idx.full_path(i),
                        is_dir: idx.is_dir(i),
                        size: idx.size(i),
                        modified: idx.modified(i),
                        flags: idx.flags(i),
                    })
                    .collect();
                Response::Search(SearchReply {
                    total: res.total as u64,
                    rows,
                    warnings: res.warnings,
                    micros: t.elapsed().as_micros() as u64,
                    close: res.close as u64,
                    groups: Vec::new(),
                })
            }
            Request::GetConfig => {
                let guard = self.shared.index.read();
                let (volumes, exclusions) = match guard.as_ref() {
                    Some(idx) => (idx.volumes().iter().map(|v| v.letter()).collect(), idx.exclusions().to_vec()),
                    None => (Vec::new(), Vec::new()),
                };
                Response::Config(ConfigReply {
                    volumes,
                    exclusions,
                    default_exclusions: default_exclusions(false),
                    content_folders: self.content.folders(),
                    default_content_folders: content::default_folders(),
                })
            }
            Request::ContentSearch(r) => {
                let guard = self.shared.index.read();
                let Some(idx) = guard.as_ref() else {
                    return Response::Content(Default::default());
                };
                let query = whereti_query::parse(&r.query, &system::date_ctx());
                let active = self.shared.activity.ranking(idx);
                Response::Content(self.content.search(idx, &query, r.limit.min(MAX_LIMIT) as usize, &r.usage, active, caller))
            }
            Request::ScanUnindexed(r) => {
                let query = whereti_query::parse(&r.query, &system::date_ctx());
                // the name index is held only while the list is made
                let plan = {
                    let guard = self.shared.index.read();
                    let Some(idx) = guard.as_ref() else { return Response::Scan(Default::default()) };
                    self.content.scan_plan(idx, &query)
                };
                let budget = Duration::from_millis(r.budget_ms.clamp(200, 5000) as u64);
                Response::Scan(self.content.scan_run(&plan, r.skip, budget, r.limit.clamp(1, 500) as usize, caller))
            }
            Request::Hint { busy, away } => {
                self.content.hint(busy, away);
                Response::Ok
            }
            Request::SetWorkFolders { on } => {
                self.log(format!("learning where the user works turned {} by a client", if on { "on" } else { "off" }));
                match self.shared.activity.set_on(on) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(e),
                }
            }
            Request::FileInfo { path } => {
                if !caller.can_read(std::slice::from_ref(&path)).first().copied().unwrap_or(false) {
                    return Response::FileInfo(Default::default());
                }
                let key = {
                    let guard = self.shared.index.read();
                    let Some(idx) = guard.as_ref() else { return Response::FileInfo(Default::default()) };
                    // by its name (one quick scan), then the exact path
                    let name = path.rsplit('\\').next().unwrap_or(&path);
                    if name.is_empty() {
                        return Response::FileInfo(Default::default());
                    }
                    let q = whereti_query::Query {
                        clauses: vec![whereti_query::Clause { negated: false, any_of: vec![whereti_query::Term::Text(whereti_query::fold(name))] }],
                    };
                    idx.matching(&q).into_iter().find(|&e| idx.full_path(e).eq_ignore_ascii_case(&path)).and_then(|e| {
                        let record = idx.records_of(&[e]).into_iter().next().flatten()?;
                        Some(whereti_content::key(idx.volume_of(e).letter(), record))
                    })
                };
                Response::FileInfo(key.map(|k| self.content.file_info(k)).unwrap_or_default())
            }
            Request::SetMeaning { on } => match self.content.set_meaning(on) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            Request::MeaningSearch(r) => {
                let query = whereti_query::parse(&r.query, &system::date_ctx());
                // the model first, without the name index locked
                let Some(q) = self.content.embed_question(&query) else {
                    return Response::Content(Default::default());
                };
                let guard = self.shared.index.read();
                let Some(idx) = guard.as_ref() else {
                    return Response::Content(Default::default());
                };
                Response::Content(self.content.search_meaning(idx, &query, &q, r.limit.min(MAX_LIMIT) as usize, caller))
            }
            Request::SetPhotoTags { on } => {
                self.log(format!("photo recognition turned {} by a client", if on { "on" } else { "off" }));
                match self.content.set_photo_tags(on) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(e),
                }
            }
            Request::SetContentFolders { paths } => match self.content.set_folders(&paths) {
                Ok(()) => {
                    self.log(format!("content folders changed by a client: {}", self.content.folders().join(" · ")));
                    Response::Ok
                }
                Err(e) => Response::Error(e),
            },
            Request::SetExclusions { paths } => match validate_exclusions(&paths) {
                Ok(paths) => {
                    self.log(format!("exclusions changed by a client: {}", paths.join(" · ")));
                    let _ = self.tx.send(Msg::Reconfigure(paths));
                    Response::Ok
                }
                Err(e) => Response::Error(e),
            },
            Request::Status => {
                let guard = self.shared.index.read();
                let (entries, volumes) = match guard.as_ref() {
                    Some(idx) => (
                        idx.live_len() as u64,
                        idx.volumes().iter().map(|v| VolumeStatus { letter: v.letter(), next_usn: v.next_usn }).collect(),
                    ),
                    None => (0, Vec::new()),
                };
                drop(guard);
                Response::Status(StatusReply {
                    state: self.state(),
                    entries,
                    volumes,
                    uptime_secs: self.shared.started.elapsed().as_secs(),
                    cpu_ms: system::process_cpu_ms(),
                    private_bytes: system::process_private_bytes(),
                    last_change_secs: self.shared.last_change.lock().map(|t| t.elapsed().as_secs()),
                    open_files: self.shared.open_files.load(Ordering::Relaxed),
                    recent_log: self.shared.log.recent(),
                    content: self.content.status(),
                    work_folders: self.shared.activity.is_on().then(|| self.shared.activity.top(4)),
                })
            }
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    cfg: Config,
    tx: mpsc::Sender<Msg>,
    /// Per volume in index order: the open volume, `None` for a walked drive
    /// (or an NTFS drive that could not be opened).
    vols: Vec<Option<Volume>>,
    open: Vec<HashSet<u64>>,
    /// Journal reader threads, by drive: the thread and its stop flag.
    followers: std::collections::HashMap<char, (JoinHandle<()>, Arc<AtomicBool>)>,
    /// Safe removal: the hidden window, and each drive's registration.
    removal: Option<Removal>,
    notify: HashMap<char, usize>,
    /// Drives let go for a removal; not opened again until it is done or cancelled.
    released: HashSet<char>,
    /// Walked drives: when, how long it took, and what was found.
    walks: HashMap<char, (Instant, Duration, u64)>,
    last_drive_check: Instant,
    /// Per volume: journal size, and journal position at the last save.
    journal_max: Vec<u64>,
    saved_usn: Vec<i64>,
    pool: rayon::ThreadPool,
    dirty: bool,
    last_save: Instant,
    last_refresh: Instant,
    last_reconcile: Instant,
    last_change: Instant,
}

impl Worker {
    fn new(shared: Arc<Shared>, cfg: Config, tx: mpsc::Sender<Msg>) -> Worker {
        let now = Instant::now();
        Worker {
            shared,
            cfg,
            tx,
            vols: Vec::new(),
            open: Vec::new(),
            followers: HashMap::new(),
            removal: None,
            notify: HashMap::new(),
            released: HashSet::new(),
            walks: HashMap::new(),
            last_drive_check: now,
            journal_max: Vec::new(),
            saved_usn: Vec::new(),
            pool: system::maintenance_pool(),
            dirty: false,
            last_save: now,
            last_refresh: now,
            last_reconcile: now,
            last_change: now,
        }
    }

    fn log(&self, msg: impl AsRef<str>) {
        self.shared.log.log(msg);
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        self.removal = Removal::start(self.tx.clone());
        if self.removal.is_none() {
            self.log("safe removal of external drives is not available (no notification window)");
        }
        if let Err(e) = self.startup() {
            self.log(format!("error: {e}"));
            self.shared.set_state(format!("error: {e}"));
            // Keep serving whatever index we have until asked to stop.
            while !matches!(rx.recv(), Ok(Msg::Stop) | Err(_)) {}
            return;
        }
        let mut stopping = false;
        while !stopping {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Msg::Reconfigure(paths)) => {
                    if let Err(e) = self.reconfigure(paths) {
                        self.log(format!("error: {e}"));
                    }
                }
                Ok(Msg::Release(letter, reply)) => self.release(letter, reply),
                Ok(Msg::Reacquire(letter)) => self.reacquire(letter),
                Ok(Msg::Journal(first)) => {
                    let mut msgs = vec![first];
                    let deadline = Instant::now() + BATCH_WINDOW;
                    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                        match rx.recv_timeout(left) {
                            Ok(Msg::Journal(m)) => msgs.push(m),
                            Ok(Msg::Reconfigure(paths)) => {
                                // apply the pending journal batch first, then rebuild
                                let _ = self.tx.send(Msg::Reconfigure(paths));
                                break;
                            }
                            Ok(Msg::Stop) => {
                                stopping = true;
                                break;
                            }
                            Ok(Msg::Release(letter, reply)) => self.release(letter, reply),
                            Ok(Msg::Reacquire(letter)) => {
                                let _ = self.tx.send(Msg::Reacquire(letter));
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(e) = self.apply_msgs(msgs) {
                        self.log(format!("error: {e}"));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(e) = self.timers() {
                        self.log(format!("error: {e}"));
                    }
                }
            }
        }
        if self.dirty {
            if let Err(e) = self.save() {
                self.log(format!("error: could not save on stop: {e}"));
            }
        }
        self.log("stopped");
    }

    fn startup(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.shared.set_state("loading");
        let index = match snapshot::load(&self.cfg.index_path) {
            Ok(idx) => {
                self.log(format!(
                    "loaded {} entries from {} in {} ms",
                    idx.len(),
                    self.cfg.index_path.display(),
                    t.elapsed().as_millis()
                ));
                idx
            }
            Err(e) => {
                self.log(format!("no usable index ({e}) - building one"));
                self.shared.set_state("indexing");
                let idx = self.build_fresh()?;
                self.dirty = true;
                idx
            }
        };
        // An index built with the old default exclusions (never edited by
        // the user) moves to the current defaults: generated folders out.
        let windows = (!self.cfg.include_windows).then(|| std::env::var("SystemRoot").ok()).flatten();
        let old: HashSet<String> =
            Exclusions::previous_defaults(windows.as_deref()).paths.iter().map(|p| p.to_lowercase()).collect();
        let has: HashSet<String> = index.exclusions().iter().map(|p| p.to_lowercase()).collect();
        let upgrade_defaults = has == old;
        // Drives that are gone (an external disk unplugged while the PC was
        // off) leave the index; drives that are new get scanned below.
        let present = walk::drives();
        let indexed: Vec<char> = index.volumes().iter().map(|v| v.letter()).collect();
        let gone: Vec<char> = indexed.iter().copied().filter(|l| !present.iter().any(|d| d.letter == *l)).collect();
        let index = if gone.is_empty() {
            index
        } else {
            self.log(format!("{} no longer connected: left out of the index", letters_text(&gone)));
            self.dirty = true;
            self.pool.install(|| index.rebuild(Vec::new(), &gone))
        };
        let mut rescan: Vec<char> =
            present.iter().map(|d| d.letter).filter(|l| !indexed.contains(l)).collect();
        *self.shared.index.write() = Some(index);
        self.reopen();

        self.shared.set_state("catching up");
        for v in 0..self.vols.len() {
            let letter = self.letter(v);
            let t = Instant::now();
            match self.catch_up(v)? {
                Some((records, stats)) => {
                    self.log(format!(
                        "{letter}: replayed {records} journal records → {} files/folders re-read (+{} −{} ~{}) in {} ms",
                        stats.records,
                        stats.added,
                        stats.removed,
                        stats.updated,
                        t.elapsed().as_millis()
                    ));
                    if stats.needs_rescan {
                        rescan.push(letter);
                    }
                }
                None => rescan.push(letter),
            }
        }
        if !rescan.is_empty() {
            self.rescan(&rescan)?;
        }
        self.follow_all();
        self.shared.set_state("ready");
        self.log(format!("ready in {} ms", t.elapsed().as_millis()));
        if upgrade_defaults {
            self.log("default exclusions updated: generated folders (node_modules, .git, caches…) are left out");
            self.reconfigure(default_exclusions(self.cfg.include_windows))?;
        }
        Ok(())
    }

    fn letter(&self, v: usize) -> char {
        self.shared.index.read().as_ref().map(|i| i.volumes()[v].letter()).unwrap_or('?')
    }

    /// Replays the journal for volume `v`. `None` if it must be rescanned.
    fn catch_up(&mut self, v: usize) -> Result<Option<(usize, ApplyStats)>, String> {
        let info = self.shared.index.read().as_ref().expect("index").volumes()[v];
        let Some(vol) = self.vols[v].as_ref() else { return Ok(None) };
        match sync::read_since(&info, vol, &mut self.open[v])? {
            JournalRead::Changes { batch, journal_records, journal_id, next_usn } => {
                let stats = self.apply_batch(v, batch, journal_id, next_usn)?;
                Ok(Some((journal_records, stats)))
            }
            JournalRead::Rescan(why) => {
                self.log(format!("{}: {why} → full rescan", info.letter()));
                Ok(None)
            }
        }
    }

    /// Phase 1 (no lock): read current record states. Phase 2 (write lock):
    /// apply them and move the journal position.
    fn apply_batch(&mut self, v: usize, batch: Batch, journal_id: u64, next_usn: i64) -> Result<ApplyStats, String> {
        let vol = self.vols[v].as_ref().ok_or("the drive is not open")?;
        let mut pre = Prefetched::read(vol, &batch.records).map_err(|e| format!("reading MFT records: {e}"))?;
        let records = batch.records.clone();
        let stats = {
            let mut guard = self.shared.index.write();
            let idx = guard.as_mut().expect("index");
            let stats = idx.apply(v, &batch.records, &batch.created, &mut pre).map_err(|e| e.to_string())?;
            if !stats.needs_rescan {
                idx.set_journal_position(v, journal_id, next_usn);
            }
            stats
        };
        if let Some(idx) = self.shared.index.read().as_ref() {
            if let Some(c) = self.shared.content.get() {
                c.note_changes(idx, v, &records);
            }
            self.shared.activity.note(idx, v, &records);
        }
        if stats.records > 0 {
            self.dirty = true;
            self.last_change = Instant::now();
            *self.shared.last_change.lock() = Some(self.last_change);
        }
        self.shared
            .open_files
            .store(self.open.iter().map(|o| o.len() as u64).sum(), Ordering::Relaxed);
        Ok(stats)
    }

    fn apply_msgs(&mut self, msgs: Vec<JournalMsg>) -> Result<(), String> {
        for v in 0..self.vols.len() {
            let letter = self.letter(v);
            let mine: Vec<&JournalMsg> = msgs.iter().filter(|m| m.letter == letter).collect();
            if mine.is_empty() {
                continue;
            }
            if let Some(err) = mine.iter().find_map(|m| m.error.as_ref()) {
                self.followers.remove(&letter);
                if walk::drives().iter().any(|d| d.letter == letter) {
                    self.log(format!("{err} → full rescan"));
                } else {
                    self.log(format!("{letter}: disconnected"));
                }
                // rescan() also drops the drive when it is gone
                self.rescan(&[letter])?;
                self.follow_all();
                return Ok(()); // volume order may have changed
            }
            let t = Instant::now();
            let mut batch = Batch::default();
            for m in &mine {
                batch.add(&m.changes, &mut self.open[v]);
            }
            let next = mine.iter().map(|m| m.next).max().unwrap_or(0);
            let journal_id = self.shared.index.read().as_ref().expect("index").volumes()[v].journal_id;
            let stats = self.apply_batch(v, batch, journal_id, next)?;
            if self.cfg.verbose {
                self.log(format!(
                    "{letter}: {} journal records → +{} −{} ~{} in {:.1} ms",
                    mine.iter().map(|m| m.changes.len()).sum::<usize>(),
                    stats.added,
                    stats.removed,
                    stats.updated,
                    t.elapsed().as_secs_f64() * 1000.0
                ));
            }
            if stats.needs_rescan {
                self.log(format!("{letter}: a folder was moved in from outside the index → full rescan"));
                self.rescan(&[letter])?;
            }
        }
        let ratio = self.shared.index.read().as_ref().map_or(0.0, NameIndex::overlay_ratio);
        if ratio > COMPACT_AT {
            self.compact();
        }
        Ok(())
    }

    fn timers(&mut self) -> Result<(), String> {
        if self.last_drive_check.elapsed() >= DRIVE_CHECK {
            self.last_drive_check = Instant::now();
            self.check_drives()?;
            self.walk_due()?;
        }
        if self.last_refresh.elapsed() >= OPEN_REFRESH {
            self.last_refresh = Instant::now();
            for v in 0..self.vols.len() {
                if self.open[v].is_empty() || self.vols[v].is_none() {
                    continue;
                }
                let records: Vec<u64> = self.open[v].iter().copied().collect();
                let info = self.shared.index.read().as_ref().expect("index").volumes()[v];
                let batch = Batch { records, created: HashSet::new() };
                self.apply_batch(v, batch, info.journal_id, info.next_usn)?;
            }
        }
        if self.dirty && (self.last_save.elapsed() >= self.cfg.save_every || self.journal_moved_far()) {
            self.save()?;
        }
        if let Some(every) = self.cfg.reconcile_every {
            if self.last_reconcile.elapsed() >= every && self.last_change.elapsed() >= IDLE_FOR_RECONCILE {
                self.reconcile()?;
            }
        }
        Ok(())
    }

    /// True when some volume's journal moved more than SAVE_AT_JOURNAL_SHARE
    /// of its size since the last save.
    fn journal_moved_far(&self) -> bool {
        let guard = self.shared.index.read();
        let Some(idx) = guard.as_ref() else { return false };
        idx.volumes().iter().enumerate().any(|(v, info)| {
            let moved = (info.next_usn - self.saved_usn.get(v).copied().unwrap_or(info.next_usn)).max(0) as f64;
            let max = self.journal_max.get(v).copied().unwrap_or(0) as f64;
            max > 0.0 && moved > max * SAVE_AT_JOURNAL_SHARE
        })
    }

    /// Rebuilds a clean index in the background and swaps it in.
    fn compact(&mut self) {
        let _bg = Background::begin();
        let t = Instant::now();
        let fresh = {
            let guard = self.shared.index.read();
            let idx = guard.as_ref().expect("index");
            self.pool.install(|| idx.rebuild_with(Vec::new()))
        };
        *self.shared.index.write() = Some(fresh);
        self.dirty = true;
        self.log(format!("compacted the index in {} ms", t.elapsed().as_millis()));
    }

    fn save(&mut self) -> Result<(), String> {
        if !self.shared.index.read().as_ref().is_some_and(NameIndex::is_compact) {
            self.compact();
        }
        let _bg = Background::begin();
        let t = Instant::now();
        if self.cfg.index_path == system::default_index_path() {
            system::ensure_data_dir().map_err(|e| format!("cannot create {}: {e}", system::data_dir().display()))?;
        }
        let size = {
            let guard = self.shared.index.read();
            snapshot::save(guard.as_ref().expect("index"), &self.cfg.index_path)
                .map_err(|e| format!("cannot save {}: {e}", self.cfg.index_path.display()))?
        };
        self.shared.activity.save();
        self.dirty = false;
        self.last_save = Instant::now();
        self.saved_usn = self.shared.index.read().as_ref().expect("index").volumes().iter().map(|v| v.next_usn).collect();
        self.log(format!("saved the index ({} MB) in {} ms", size >> 20, t.elapsed().as_millis()));
        Ok(())
    }

    /// Scans `letters` again and swaps in an index built from the scans, then
    /// replays what happened during the scan.
    fn rescan(&mut self, letters: &[char]) -> Result<(), String> {
        let _bg = Background::begin();
        let t = Instant::now();
        let list: String = letters.iter().map(|l| format!("{l}:")).collect::<Vec<_>>().join(" ");
        self.shared.set_state(format!("rescanning {list}"));
        let present = walk::drives();
        let mut fresh = Vec::new();
        let mut gone = Vec::new();
        for &l in letters {
            match present.iter().find(|d| d.letter == l) {
                // being removed safely: not opened again meanwhile
                Some(_) if self.released.contains(&l) => {}
                Some(d) => fresh.push(self.scan(d)?),
                None => gone.push(l),
            }
        }
        let rebuilt = {
            let guard = self.shared.index.read();
            let idx = guard.as_ref().expect("index");
            self.pool.install(|| idx.rebuild(fresh, &gone))
        };
        *self.shared.index.write() = Some(rebuilt);
        for l in &gone {
            self.walks.remove(l);
            self.released.remove(l);
        }
        let open_before = self.open_by_letter();
        self.reopen();
        for v in 0..self.vols.len() {
            let l = self.letter(v);
            if !letters.contains(&l) {
                if let Some(o) = open_before.get(&l) {
                    self.open[v] = o.clone();
                }
            }
        }
        for v in 0..self.vols.len() {
            if letters.contains(&self.letter(v)) {
                if let Some((_, stats)) = self.catch_up(v)? {
                    if stats.needs_rescan {
                        self.log(format!("{}: still inconsistent after a rescan; the next change retries", self.letter(v)));
                    }
                }
            }
        }
        self.dirty = true;
        if let Some(c) = self.shared.content.get() {
            c.recheck();
        }
        self.shared.set_state("ready");
        self.log(format!("rescanned {list} in {} ms", t.elapsed().as_millis()));
        Ok(())
    }

    /// Rebuilds the whole index with new exclusion paths (from Settings).
    fn reconfigure(&mut self, paths: Vec<String>) -> Result<(), String> {
        let _bg = Background::begin();
        let t = Instant::now();
        self.shared.set_state("reindexing");
        let letters: Vec<char> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| v.letter()).collect()
        };
        let mut scans = Vec::new();
        for d in walk::drives().iter().filter(|d| letters.contains(&d.letter)) {
            scans.push(self.scan(d)?);
        }
        let (fresh, stats) = self.pool.install(|| build(scans, &Exclusions { paths }));
        *self.shared.index.write() = Some(fresh);
        self.reopen();
        // replay what changed during the scans
        for v in 0..self.vols.len() {
            self.catch_up(v)?;
        }
        self.dirty = true;
        self.save()?;
        if let Some(c) = self.shared.content.get() {
            c.recheck();
        }
        self.shared.set_state("ready");
        self.log(format!(
            "rebuilt with new exclusions: {} entries ({} excluded) in {} ms",
            stats.kept,
            stats.excluded,
            t.elapsed().as_millis()
        ));
        Ok(())
    }

    /// Daily safety net (PLAN.md §7.2): a full rescan replaces the live index.
    fn reconcile(&mut self) -> Result<(), String> {
        self.last_reconcile = Instant::now();
        let letters: Vec<char> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| v.letter()).collect()
        };
        let before = self.shared.index.read().as_ref().map_or(0, NameIndex::live_len);
        self.log("daily check: full rescan");
        self.rescan(&letters)?;
        let after = self.shared.index.read().as_ref().map_or(0, NameIndex::live_len);
        self.log(format!("daily check done: {before} entries before, {after} after"));
        Ok(())
    }

    /// A journal reader for every NTFS drive that has none yet.
    fn follow_all(&mut self) {
        let infos: Vec<_> = self.shared.index.read().as_ref().expect("index").volumes().to_vec();
        for (v, info) in infos.into_iter().enumerate() {
            let letter = info.letter();
            let running = self.followers.get(&letter).is_some_and(|(h, _)| !h.is_finished());
            if self.vols[v].is_none() || info.journal_id == WALKED || running || self.released.contains(&letter) {
                continue;
            }
            let tx = self.tx.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            if let Ok(h) = std::thread::Builder::new()
                .name(format!("journal-{letter}"))
                .spawn(move || sync::follow_journal::<Msg>(letter, info.journal_id, info.next_usn, tx, flag))
            {
                self.followers.insert(letter, (h, stop));
            }
        }
    }

    /// Opens the volumes again after the index's drives changed, and sizes
    /// the per-volume lists to match.
    fn reopen(&mut self) {
        let infos: Vec<_> = self.shared.index.read().as_ref().expect("index").volumes().to_vec();
        for (_, n) in self.notify.drain() {
            Removal::unwatch(n);
        }
        self.vols = infos
            .iter()
            .map(|i| {
                if i.journal_id == WALKED || self.released.contains(&i.letter()) {
                    None
                } else {
                    Volume::open(i.letter(), false).ok()
                }
            })
            .collect();
        if let Some(r) = &self.removal {
            for vol in self.vols.iter().flatten() {
                if let Some(n) = r.watch(vol.letter, vol.raw_handle()) {
                    self.notify.insert(vol.letter, n);
                }
            }
        }
        self.open = vec![HashSet::new(); infos.len()];
        self.journal_max = self
            .vols
            .iter()
            .map(|v| v.as_ref().and_then(|v| v.query_usn_journal().ok()).map_or(0, |j| j.maximum_size))
            .collect();
        self.saved_usn = infos.iter().map(|v| v.next_usn).collect();
    }

    /// Windows wants to remove `letter`: close the MFT handle and stop the
    /// journal reader, then answer so the removal can go on.
    fn release(&mut self, letter: char, reply: Option<mpsc::Sender<()>>) {
        use std::os::windows::io::AsRawHandle;
        if self.released.insert(letter) {
            if let Some(n) = self.notify.remove(&letter) {
                Removal::unwatch(n);
            }
            for v in 0..self.vols.len() {
                if self.letter(v) == letter {
                    self.vols[v] = None;
                    self.open[v].clear();
                }
            }
            if let Some((h, stop)) = self.followers.remove(&letter) {
                stop.store(true, Ordering::SeqCst);
                // the reader sleeps in the kernel waiting for the journal: wake it
                let deadline = Instant::now() + Duration::from_secs(3);
                while !h.is_finished() && Instant::now() < deadline {
                    // SAFETY: the thread's own handle, alive while `h` is.
                    unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(h.as_raw_handle() as _) };
                    std::thread::sleep(Duration::from_millis(20));
                }
                if h.is_finished() {
                    let _ = h.join();
                }
            }
            self.log(format!("{letter}: let go for safe removal"));
        }
        if let Some(r) = reply {
            let _ = r.send(());
        }
    }

    /// The removal of `letter` was cancelled: follow it again.
    fn reacquire(&mut self, letter: char) {
        if self.released.remove(&letter) {
            let open_before = self.open_by_letter();
            self.reopen();
            for v in 0..self.vols.len() {
                if let Some(o) = open_before.get(&self.letter(v)) {
                    self.open[v] = o.clone();
                }
            }
            self.follow_all();
            self.log(format!("{letter}: removal cancelled, followed again"));
        }
    }

    fn open_by_letter(&self) -> HashMap<char, HashSet<u64>> {
        (0..self.open.len()).map(|v| (self.letter(v), self.open[v].clone())).collect()
    }

    /// Scans one drive (MFT or walk) and remembers walks for `walk_due`.
    fn scan(&mut self, d: &Drive) -> Result<VolumeScan, String> {
        let t = Instant::now();
        let s = sync::scan_any(d, self.cfg.threads)?;
        if s.journal_id == WALKED {
            self.walks.insert(d.letter, (Instant::now(), t.elapsed(), walk::fingerprint(&s)));
        }
        Ok(s)
    }

    /// Adds drives that appeared and drops drives that are gone.
    fn check_drives(&mut self) -> Result<(), String> {
        let present = walk::drives();
        let indexed: Vec<(char, u64, bool)> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| (v.letter(), v.serial, v.journal_id == WALKED)).collect()
        };
        let mut changed: Vec<char> = present
            .iter()
            .filter(|d| !indexed.iter().any(|(l, _, _)| *l == d.letter))
            .map(|d| d.letter)
            .collect();
        for &(l, serial, walked) in &indexed {
            match present.iter().find(|d| d.letter == l) {
                None => changed.push(l),
                Some(_) if self.released.contains(&l) => {}
                // another stick under the same letter
                Some(d) if walked && d.serial != serial => changed.push(l),
                Some(_) => {}
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        changed.sort();
        self.log(format!("drives changed: {}", letters_text(&changed)));
        self.rescan(&changed)?;
        self.follow_all();
        Ok(())
    }

    /// Walks drives without a journal again when it is their turn, and
    /// rebuilds only if the walk found a change. Small drives (a USB stick)
    /// are walked every minute, big ones less often.
    fn walk_due(&mut self) -> Result<(), String> {
        if self.last_change.elapsed() < Duration::from_secs(5) {
            return Ok(()); // the PC is busy writing files: later
        }
        let due: Vec<char> = self
            .walks
            .iter()
            .filter(|(_, (at, took, _))| at.elapsed() >= (*took * 60).clamp(WALK_MIN, WALK_MAX))
            .map(|(l, _)| *l)
            .collect();
        for letter in due {
            let Some(d) = walk::drives().into_iter().find(|d| d.letter == letter) else { continue };
            let before = self.walks.get(&letter).map(|w| w.2);
            let _bg = Background::begin();
            let s = match self.scan(&d) {
                Ok(s) => s,
                Err(e) => {
                    self.log(e);
                    continue;
                }
            };
            if before == Some(walk::fingerprint(&s)) {
                continue;
            }
            let t = Instant::now();
            let rebuilt = {
                let guard = self.shared.index.read();
                let idx = guard.as_ref().expect("index");
                self.pool.install(|| idx.rebuild(vec![s], &[]))
            };
            *self.shared.index.write() = Some(rebuilt);
            let open_before = self.open_by_letter();
            self.reopen();
            for v in 0..self.vols.len() {
                if let Some(o) = open_before.get(&self.letter(v)) {
                    self.open[v] = o.clone();
                }
            }
            self.dirty = true;
            self.last_change = Instant::now();
            *self.shared.last_change.lock() = Some(self.last_change);
            if let Some(c) = self.shared.content.get() {
                c.recheck();
            }
            self.log(format!("{letter}: changes found, index updated in {} ms", t.elapsed().as_millis()));
        }
        Ok(())
    }

    fn build_fresh(&mut self) -> Result<NameIndex, String> {
        let t = Instant::now();
        let mut scans = Vec::new();
        let drives = walk::drives();
        for (k, d) in drives.iter().enumerate() {
            let letter = d.letter;
            // "indexing C: 1/2": the app shows it as progress on first start
            self.shared.set_state(format!("indexing {letter}: {}/{}", k + 1, drives.len()));
            match self.scan(d) {
                Ok(s) => {
                    self.log(format!("{letter}: scanned {} entries ({})", s.scan.entries.len(), d.fs));
                    scans.push(s);
                }
                Err(e) if d.letter == system_drive() => return Err(e),
                Err(e) => self.log(format!("{e}; skipped")),
            }
        }
        if scans.is_empty() {
            return Err("no drive to index".into());
        }
        let windows = (!self.cfg.include_windows).then(|| std::env::var("SystemRoot").ok()).flatten();
        let (idx, stats) = build(scans, &Exclusions::defaults(windows.as_deref()));
        self.log(format!(
            "built the index: {} entries ({} excluded) in {} ms",
            stats.kept,
            stats.excluded,
            t.elapsed().as_millis()
        ));
        Ok(idx)
    }
}

/// "C: D:"
fn letters_text(letters: &[char]) -> String {
    letters.iter().map(|l| format!("{l}:")).collect::<Vec<_>>().join(" ")
}

/// The drive Windows runs from: if it cannot be indexed, nothing works.
fn system_drive() -> char {
    std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()).unwrap_or('C').to_ascii_uppercase()
}

/// The exclusions of a fresh install (PLAN.md §5).
fn default_exclusions(include_windows: bool) -> Vec<String> {
    let windows = (!include_windows).then(|| std::env::var("SystemRoot").ok()).flatten();
    Exclusions::defaults(windows.as_deref()).paths
}

/// Accepts `X:\path` or `\path` (every drive); at most 500 paths.
fn validate_exclusions(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.len() > 500 {
        return Err("too many exclusion paths (at most 500)".into());
    }
    let mut out = Vec::new();
    for p in paths {
        const SEP: u8 = b'\\';
        let p = p.trim().replace('/', "\\");
        let p = p.trim_end_matches(SEP as char).to_string();
        let b = p.as_bytes();
        let any_depth = p.strip_prefix("**\\").is_some_and(|n| !n.is_empty() && !n.contains('\\'));
        let ok = p.len() < 1024
            && (any_depth
                || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == SEP)
                || (b.len() >= 2 && b[0] == SEP && b[1] != SEP));
        if !ok {
            return Err(format!(r"not a valid path to exclude: {p:?} (use C:\Folder or \Folder)"));
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_paths_are_checked() {
        let ok = validate_exclusions(&[r"C:\Windows\".into(), r"\node_modules".into(), "d:/proiecte/vechi".into()]).unwrap();
        assert_eq!(ok, vec![r"C:\Windows", r"\node_modules", r"d:\proiecte\vechi"]);
        assert!(validate_exclusions(&["Windows".into()]).is_err());
        assert!(validate_exclusions(&[r"\\server\share".into()]).is_err());
        assert!(validate_exclusions(&[String::new()]).is_err());
    }
}
