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
use whereti_core::{
    build, snapshot, ApplyStats, Exclusions, NameIndex, SearchOptions, SearchResults, SortBy, VolumeScan, DEFAULT_GROUPS,
};
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
/// FileInfo looks at the paths of at most this many files of that name.
const FILE_INFO_CANDIDATES: usize = 20_000;
/// The timers (drive checks, saves, refreshes) are looked at this often.
const TIMERS_EVERY: Duration = Duration::from_millis(500);
/// A drive that could not be scanned is tried again after this, then after
/// twice as long each time, up to RETRY_MAX.
const RETRY_FIRST: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(30 * 60);
/// A drive let go for a removal that is still there after this was not
/// removed (the notification that says so was lost): it is followed again.
const RELEASE_MAX: Duration = Duration::from_secs(60);
/// After Windows says a removal is done, the drive is looked at again this
/// soon (gone: left out; back already: followed again).
const RELEASE_DONE: Duration = Duration::from_secs(5);
/// After a save failed, the next one is tried this much later.
const SAVE_RETRY: Duration = Duration::from_secs(10 * 60);

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
        // What the service indexes and reads is the same for everyone on the
        // PC: only an administrator's account may change it.
        let for_everyone = matches!(
            req,
            Request::SetExclusions { .. }
                | Request::SetContentFolders { .. }
                | Request::SetPhotoTags { .. }
                | Request::SetMeaning { .. }
                | Request::SetWorkFolders { .. }
        );
        if for_everyone && !caller.is_admin() {
            return Response::Error("only an administrator of this PC can change this setting".into());
        }
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
                let run = |opts: SearchOptions| if prop_clauses.is_empty() {
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
                    let keep: Vec<bool> = res.hits.iter().map(|h| !excluded.contains(h)).collect();
                    drop_hits(&mut res, &keep);
                    cut_hits(&mut res, opts.limit);
                    res
                } else {
                    let within = self.content.entries_with_props(idx, &prop_clauses);
                    idx.search_within(&rest, &opts, &r.usage, self.shared.activity.ranking(idx), &within)
                };
                // Names only from folders the caller can list, as in Explorer:
                // another account's documents stay unseen (one check per folder).
                // When some go, look deeper (twice at most) so the list stays full.
                let want = opts.limit;
                let mut depth = want;
                let res = loop {
                    let mut res = run(SearchOptions { limit: depth, ..opts });
                    let shown = listable(idx, &res.hits, caller);
                    let gone_exact = drop_hits(&mut res, &shown);
                    let kept_exact = res.hits.len() - res.close;
                    let more = res.total > kept_exact && kept_exact < want && depth < want * 16 && depth < 100_000;
                    if !more {
                        if gone_exact > 0 {
                            // the matches not looked at may be someone else's
                            // too: count only those the caller may see
                            res.total = kept_exact;
                        }
                        // the exact matches first, the close ones after, `want` in all
                        cut_hits(&mut res, want);
                        break res;
                    }
                    depth *= 4;
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
                let skipped = guard.as_ref().map(|i| i.skipped()).unwrap_or_default();
                drop(guard);
                let mut content_folders = self.content.folders();
                // what is set for everyone, but not other people's folder
                // names: someone who cannot change it sees only what they may list
                let (exclusions, skipped) = if caller.is_admin() {
                    (exclusions, skipped)
                } else {
                    let ok = caller_sees(caller, &exclusions);
                    let exclusions: Vec<String> = exclusions.into_iter().zip(ok).filter_map(|(p, ok)| ok.then_some(p)).collect();
                    let skipped = skipped.into_iter().filter(|(r, _)| exclusions.contains(r)).collect();
                    let ok = caller_sees(caller, &content_folders);
                    content_folders = content_folders.into_iter().zip(ok).filter_map(|(p, ok)| ok.then_some(p)).collect();
                    (exclusions, skipped)
                };
                Response::Config(ConfigReply {
                    volumes,
                    exclusions,
                    default_exclusions: default_exclusions(false),
                    skipped,
                    groups: DEFAULT_GROUPS
                        .iter()
                        .map(|(id, rules)| (id.to_string(), rules.iter().map(|r| r.to_string()).collect()))
                        .collect(),
                    content_folders,
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
                    // the whole name first (cheap): a one-letter name is in
                    // millions of others; then the path of a bounded few
                    idx.matching(&q)
                        .into_iter()
                        .filter(|&e| idx.name(e).eq_ignore_ascii_case(name))
                        .take(FILE_INFO_CANDIDATES)
                        .find(|&e| idx.full_path(e).eq_ignore_ascii_case(&path))
                        .and_then(|e| {
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
                    // log lines name folders of every account: only an
                    // administrator sees them whole
                    recent_log: if caller.is_admin() {
                        self.shared.log.recent()
                    } else {
                        self.shared.log.recent().iter().map(|l| hide_paths(l)).collect()
                    },
                    content: self.content.status(),
                    // the folders people work in, only those this caller may see
                    work_folders: self.shared.activity.is_on().then(|| {
                        let top = self.shared.activity.top(4);
                        let ok = caller.can_list(&top);
                        top.into_iter().zip(ok).filter_map(|(f, ok)| ok.then_some(f)).collect()
                    }),
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
    released: HashMap<char, Released>,
    /// Walked drives: when, how long it took, and what was found.
    walks: HashMap<char, (Instant, Duration, u64)>,
    /// Drives that could not be scanned: when to try again, and how many
    /// tries failed. Their old entries stay searchable meanwhile.
    retry: HashMap<char, (Instant, u32)>,
    last_drive_check: Instant,
    /// Per volume: journal size.
    journal_max: Vec<u64>,
    /// Per drive: the journal and its position at the last save.
    saved_usn: HashMap<char, (u64, i64)>,
    pool: rayon::ThreadPool,
    dirty: bool,
    last_save: Instant,
    /// After a failed save, none is tried before this.
    save_after: Option<Instant>,
    last_refresh: Instant,
    last_reconcile: Instant,
    last_change: Instant,
    last_timers: Instant,
}

/// A drive let go for a safe removal.
struct Released {
    /// Its removal registration, kept so a cancelled removal is told to us
    /// (DBT_DEVICEQUERYREMOVEFAILED); undone when it ends.
    notify: Option<usize>,
    /// Still there at this time: the removal did not happen, follow it again.
    until: Instant,
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
            released: HashMap::new(),
            walks: HashMap::new(),
            retry: HashMap::new(),
            last_drive_check: now,
            journal_max: Vec::new(),
            saved_usn: HashMap::new(),
            pool: system::maintenance_pool(),
            dirty: false,
            last_save: now,
            save_after: None,
            last_refresh: now,
            last_reconcile: now,
            last_change: now,
            last_timers: now,
        }
    }

    /// Swaps in a new index. Entry numbers change: what was kept by number
    /// (the work-folder ranking) is rebuilt.
    fn set_index(&self, idx: NameIndex) {
        let mut guard = self.shared.index.write();
        *guard = Some(idx);
        self.shared.activity.index_replaced();
    }

    /// The position of `letter` in the index.
    fn vol_of(&self, letter: char) -> Option<usize> {
        self.shared.index.read().as_ref()?.volume_index(letter)
    }

    fn log(&self, msg: impl AsRef<str>) {
        self.shared.log.log(msg);
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        self.removal = Removal::start(self.tx.clone());
        if self.removal.is_none() {
            self.log("safe removal of external drives is not available (no notification window)");
        }
        // Without an index nothing works: try again, less and less often,
        // serving whatever index we have meanwhile.
        let mut wait = RETRY_FIRST;
        let mut exclusions = None;
        while let Err(e) = self.startup() {
            self.log(format!("error: {e}; tried again in {} s", wait.as_secs()));
            self.shared.set_state(format!("error: {e}"));
            let deadline = Instant::now() + wait;
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Ok(Msg::Release(letter, reply)) => self.release(letter, reply),
                    // applied once there is an index
                    Ok(Msg::Reconfigure(paths)) => exclusions = Some(paths),
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
            wait = (wait * 2).min(RETRY_MAX);
        }
        if let Some(paths) = exclusions {
            let _ = self.tx.send(Msg::Reconfigure(paths));
        }
        let mut stopping = false;
        while !stopping {
            // drive checks, saves and open-file refreshes also while files
            // change all the time
            if self.last_timers.elapsed() >= TIMERS_EVERY {
                self.last_timers = Instant::now();
                self.timers();
            }
            match rx.recv_timeout(TIMERS_EVERY) {
                Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Msg::Reconfigure(paths)) => self.reconfigure(paths),
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
                    self.apply_msgs(msgs);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
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
        let mut built = false;
        let index = match snapshot::load(&self.cfg.index_path) {
            Ok(mut idx) => {
                self.log(format!(
                    "loaded {} entries from {} in {} ms",
                    idx.len(),
                    self.cfg.index_path.display(),
                    t.elapsed().as_millis()
                ));
                idx.set_skipped(load_skipped(&self.cfg.index_path));
                idx
            }
            Err(e) => {
                self.log(format!("no usable index ({e}) - building one"));
                self.shared.set_state("indexing");
                let idx = self.build_fresh()?;
                self.dirty = true;
                built = true;
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
        // Rules of the generated-folder groups join the exclusions once each,
        // next to whatever the user set: `<index>.defaults` lists the rules
        // already offered. A group taken back in Settings stays out; a group
        // added in a later version still arrives.
        let groups_marker = self.cfg.index_path.with_extension("defaults");
        let offered: HashSet<String> = std::fs::read_to_string(&groups_marker)
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().to_lowercase())
            .collect();
        let add_groups: Vec<String> = if upgrade_defaults {
            Vec::new()
        } else {
            Exclusions::group_rules()
                .into_iter()
                .filter(|r| !has.contains(&r.to_lowercase()) && !offered.contains(&r.to_lowercase()))
                .collect()
        };
        let mut with_groups = index.exclusions().to_vec();
        with_groups.extend(add_groups.iter().cloned());
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
        self.set_index(index);
        self.reopen();

        self.shared.set_state("catching up");
        for v in 0..self.vols.len() {
            let letter = self.letter(v);
            let t = Instant::now();
            match self.catch_up(v).unwrap_or_else(|e| {
                self.log(format!("{e} → full rescan"));
                None
            }) {
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
            self.rescan(&rescan);
        }
        self.follow_all();
        self.shared.set_state("ready");
        self.log(format!("ready in {} ms", t.elapsed().as_millis()));
        // a fresh index on disk at once: a crash would otherwise build it again
        if built {
            if let Err(e) = self.save() {
                self.log(format!("could not save the new index: {e}"));
            }
        }
        if upgrade_defaults {
            self.log("default exclusions updated: generated folders (node_modules, .git, caches…) are left out");
            self.reconfigure(default_exclusions(self.cfg.include_windows));
        } else if !add_groups.is_empty() {
            self.log(format!(
                "default exclusions updated: {} rules for build folders, tool caches and media caches are added",
                add_groups.len()
            ));
            self.reconfigure(with_groups);
        }
        let all: String = Exclusions::group_rules().iter().map(|r| format!("{r}\n")).collect();
        let _ = std::fs::write(&groups_marker, all);
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

    /// Applies the journal records of every drive in `msgs`. A drive whose
    /// reader failed, or whose records could not be applied, is rescanned;
    /// the other drives' records are applied all the same.
    fn apply_msgs(&mut self, msgs: Vec<JournalMsg>) {
        let mut by_letter: Vec<(char, Vec<JournalMsg>)> = Vec::new();
        for m in msgs {
            match by_letter.iter_mut().find(|(l, _)| *l == m.letter) {
                Some((_, list)) => list.push(m),
                None => by_letter.push((m.letter, vec![m])),
            }
        }
        let mut rescan = Vec::new();
        for (letter, mine) in by_letter {
            let Some(v) = self.vol_of(letter) else { continue };
            let journal_id = self.shared.index.read().as_ref().expect("index").volumes()[v].journal_id;
            // from a reader of a journal the index no longer follows
            let mine: Vec<JournalMsg> = mine.into_iter().filter(|m| m.journal_id == journal_id).collect();
            if mine.is_empty() || self.vols[v].is_none() {
                continue; // or let go for a removal meanwhile
            }
            if let Some(err) = mine.iter().find_map(|m| m.error.as_ref()) {
                // stopped, not just forgotten: a newer reader must not run on unseen
                self.stop_reader(letter);
                if walk::drives().iter().any(|d| d.letter == letter) {
                    self.log(format!("{err} → full rescan"));
                } else {
                    self.log(format!("{letter}: disconnected"));
                }
                // rescan() also drops the drive when it is gone
                rescan.push(letter);
                continue;
            }
            let t = Instant::now();
            let mut batch = Batch::default();
            for m in &mine {
                batch.add(&m.changes, &mut self.open[v]);
            }
            let next = mine.iter().map(|m| m.next).max().unwrap_or(0);
            let stats = match self.apply_batch(v, batch, journal_id, next) {
                Ok(stats) => stats,
                Err(e) => {
                    // its reader moved on: what was not applied is only found by a rescan
                    self.log(format!("{letter}: {e} → full rescan"));
                    rescan.push(letter);
                    continue;
                }
            };
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
                rescan.push(letter);
            }
        }
        if !rescan.is_empty() {
            rescan.sort_unstable();
            rescan.dedup();
            self.rescan(&rescan);
        }
        let ratio = self.shared.index.read().as_ref().map_or(0.0, NameIndex::overlay_ratio);
        if ratio > COMPACT_AT {
            self.compact();
        }
    }

    /// Drive checks, walks, open-file refreshes, saves and the daily check,
    /// each when it is due. One failing never stops the others.
    fn timers(&mut self) {
        if self.last_drive_check.elapsed() >= DRIVE_CHECK {
            self.last_drive_check = Instant::now();
            self.check_drives();
            self.walk_due();
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
                if let Err(e) = self.apply_batch(v, batch, info.journal_id, info.next_usn) {
                    self.log(format!("{}: re-reading open files: {e}", info.letter()));
                }
            }
        }
        let may_save = self.save_after.is_none_or(|t| Instant::now() >= t);
        if self.dirty && may_save && (self.last_save.elapsed() >= self.cfg.save_every || self.journal_moved_far()) {
            if let Err(e) = self.save() {
                self.log(format!("error: {e}"));
                self.save_after = Some(Instant::now() + SAVE_RETRY);
            }
        }
        if let Some(every) = self.cfg.reconcile_every {
            if self.last_reconcile.elapsed() >= every && self.last_change.elapsed() >= IDLE_FOR_RECONCILE {
                self.reconcile();
            }
        }
    }

    /// True when some volume's journal moved more than SAVE_AT_JOURNAL_SHARE
    /// of its size since the last save.
    fn journal_moved_far(&self) -> bool {
        let guard = self.shared.index.read();
        let Some(idx) = guard.as_ref() else { return false };
        idx.volumes().iter().enumerate().any(|(v, info)| {
            let saved = match self.saved_usn.get(&info.letter()) {
                Some(&(id, usn)) if id == info.journal_id => usn,
                _ => info.next_usn,
            };
            let max = self.journal_max.get(v).copied().unwrap_or(0) as f64;
            moved_far(saved, info.next_usn, max)
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
        self.set_index(fresh);
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
            let idx = guard.as_ref().expect("index");
            save_skipped(&self.cfg.index_path, idx.skipped_by_drive());
            save_exclusions(&self.cfg.index_path, idx.exclusions());
            snapshot::save(idx, &self.cfg.index_path)
                .map_err(|e| format!("cannot save {}: {e}", self.cfg.index_path.display()))?
        };
        self.shared.activity.save();
        self.dirty = false;
        self.last_save = Instant::now();
        self.save_after = None;
        self.saved_usn = self
            .shared
            .index
            .read()
            .as_ref()
            .expect("index")
            .volumes()
            .iter()
            .map(|v| (v.letter(), (v.journal_id, v.next_usn)))
            .collect();
        self.log(format!("saved the index ({} MB) in {} ms", size >> 20, t.elapsed().as_millis()));
        Ok(())
    }

    /// A drive could not be scanned: try again later, less often each time.
    fn failed(&mut self, letter: char, e: &str) {
        let tries = self.retry.get(&letter).map_or(0, |r| r.1) + 1;
        let wait = retry_wait(tries);
        self.retry.insert(letter, (Instant::now() + wait, tries));
        self.log(format!("{e}; {letter}: tried again in {} s", wait.as_secs()));
    }

    /// Scans `letters` again and swaps in an index built from the scans, then
    /// replays what happened during the scan. A drive that cannot be scanned
    /// keeps its old entries and is tried again later; the others go on.
    fn rescan(&mut self, letters: &[char]) {
        let _bg = Background::begin();
        let t = Instant::now();
        let list = letters_text(letters);
        self.shared.set_state(format!("rescanning {list}"));
        for &l in letters {
            // its reader follows the old journal position (or another disk)
            self.stop_reader(l);
        }
        let present = walk::drives();
        let mut fresh = Vec::new();
        let mut gone = Vec::new();
        for &l in letters {
            match present.iter().find(|d| d.letter == l) {
                // being removed safely: not opened again meanwhile
                Some(_) if self.released.contains_key(&l) => {}
                Some(d) => match self.scan(d) {
                    Ok(s) => {
                        self.retry.remove(&l);
                        fresh.push(s);
                    }
                    Err(e) => self.failed(l, &e),
                },
                None => gone.push(l),
            }
        }
        let scanned: Vec<char> = fresh.iter().map(|s| s.letter).collect();
        if !fresh.is_empty() || !gone.is_empty() {
            let rebuilt = {
                let guard = self.shared.index.read();
                let idx = guard.as_ref().expect("index");
                self.pool.install(|| idx.rebuild(fresh, &gone))
            };
            // by letter while the volumes are still the old ones
            let open_before = self.open_by_letter();
            self.set_index(rebuilt);
            for l in &gone {
                self.walks.remove(l);
                self.retry.remove(l);
                self.unrelease(*l);
            }
            self.reopen();
            for v in 0..self.vols.len() {
                let l = self.letter(v);
                if !scanned.contains(&l) {
                    if let Some(o) = open_before.get(&l) {
                        self.open[v] = o.clone();
                    }
                }
            }
            for v in 0..self.vols.len() {
                let l = self.letter(v);
                if !scanned.contains(&l) {
                    continue;
                }
                match self.catch_up(v) {
                    Ok(Some((_, stats))) if stats.needs_rescan => {
                        self.log(format!("{l}: still inconsistent after a rescan; the next change retries"));
                    }
                    Ok(_) => {}
                    // its reader starts at the scan's position and replays the same
                    Err(e) => self.log(e),
                }
            }
            self.dirty = true;
            if let Some(c) = self.shared.content.get() {
                c.recheck();
            }
        }
        self.follow_all();
        self.shared.set_state("ready");
        self.log(format!("rescanned {list} in {} ms", t.elapsed().as_millis()));
    }

    /// Rebuilds the whole index with new exclusion paths (from Settings).
    /// A drive that cannot be scanned now is left out and added back (with
    /// the new exclusions) when it can be read again.
    fn reconfigure(&mut self, paths: Vec<String>) {
        let _bg = Background::begin();
        let t = Instant::now();
        self.shared.set_state("reindexing");
        let letters: Vec<char> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| v.letter()).collect()
        };
        for &l in &letters {
            self.stop_reader(l);
        }
        let mut scans = Vec::new();
        let drives: Vec<Drive> = walk::drives()
            .into_iter()
            .filter(|d| letters.contains(&d.letter) && !self.released.contains_key(&d.letter))
            .collect();
        for d in &drives {
            match self.scan(d) {
                Ok(s) => {
                    self.retry.remove(&d.letter);
                    scans.push(s);
                }
                Err(e) => self.failed(d.letter, &format!("{e}; left out for now")),
            }
        }
        let (fresh, stats) = self.pool.install(|| build(scans, &Exclusions { paths }));
        self.set_index(fresh);
        self.reopen();
        // replay what changed during the scans
        for v in 0..self.vols.len() {
            if let Err(e) = self.catch_up(v) {
                self.log(e);
            }
        }
        self.follow_all();
        self.dirty = true;
        if let Err(e) = self.save() {
            self.log(format!("error: {e}"));
            self.save_after = Some(Instant::now() + SAVE_RETRY);
        }
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
    }

    /// Daily safety net (PLAN.md §7.2): a full rescan replaces the live index.
    fn reconcile(&mut self) {
        self.last_reconcile = Instant::now();
        let letters: Vec<char> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| v.letter()).collect()
        };
        let before = self.shared.index.read().as_ref().map_or(0, NameIndex::live_len);
        self.log("daily check: full rescan");
        self.rescan(&letters);
        let after = self.shared.index.read().as_ref().map_or(0, NameIndex::live_len);
        self.log(format!("daily check done: {before} entries before, {after} after"));
    }

    /// A journal reader for every NTFS drive that has none yet.
    fn follow_all(&mut self) {
        let infos: Vec<_> = self.shared.index.read().as_ref().expect("index").volumes().to_vec();
        for (v, info) in infos.into_iter().enumerate() {
            let letter = info.letter();
            let running = self.followers.get(&letter).is_some_and(|(h, _)| !h.is_finished());
            // a drive waiting for another scan is not followed: its reader
            // would fail and ask for a scan at once, again and again
            let waits = self.released.contains_key(&letter) || self.retry.contains_key(&letter);
            if self.vols.get(v).is_none_or(Option::is_none) || info.journal_id == WALKED || running || waits {
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

    /// Stops the journal reader of `letter`, if it has one.
    fn stop_reader(&mut self, letter: char) {
        use std::os::windows::io::AsRawHandle;
        let Some((h, stop)) = self.followers.remove(&letter) else { return };
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
                if i.journal_id == WALKED || self.released.contains_key(&i.letter()) {
                    return None;
                }
                // another disk under this letter is not read as this one:
                // the next drive check rescans it
                Volume::open(i.letter(), false)
                    .ok()
                    .filter(|vol| walk::same_volume(vol.ntfs.volume_serial_number as u64, i.serial))
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
        // a drive new to the index (or with a new journal) counts from here;
        // the others keep their position at the last save
        for i in &infos {
            let at_save = self.saved_usn.entry(i.letter()).or_insert((i.journal_id, i.next_usn));
            if at_save.0 != i.journal_id {
                *at_save = (i.journal_id, i.next_usn);
            }
        }
    }

    /// Windows wants to remove `letter` (`reply` set: it asks first), or it
    /// is being removed or gone already (`None`): close the MFT handle and
    /// stop the journal reader, then answer so the removal can go on.
    fn release(&mut self, letter: char, reply: Option<mpsc::Sender<()>>) {
        let asks = reply.is_some();
        if let Some(r) = self.released.get_mut(&letter) {
            if !asks {
                // the removal is done: look at the drive again soon
                r.until = r.until.min(Instant::now() + RELEASE_DONE);
            }
        } else {
            // The registration stays while the removal is under way, so a
            // cancelled one is told to us (DBT_DEVICEQUERYREMOVEFAILED).
            let notify = self.notify.remove(&letter);
            let until = Instant::now() + if asks { RELEASE_MAX } else { RELEASE_DONE };
            self.released.insert(letter, Released { notify, until });
            for v in 0..self.vols.len() {
                if self.letter(v) == letter {
                    self.vols[v] = None;
                    self.open[v].clear();
                }
            }
            self.stop_reader(letter);
            self.log(format!("{letter}: let go for safe removal"));
        }
        if let Some(r) = reply {
            let _ = r.send(());
        }
    }

    /// Forgets that `letter` was let go for a removal.
    fn unrelease(&mut self, letter: char) -> bool {
        match self.released.remove(&letter) {
            Some(r) => {
                if let Some(n) = r.notify {
                    Removal::unwatch(n);
                }
                true
            }
            None => false,
        }
    }

    /// The removal of `letter` was cancelled (or never happened): follow it again.
    fn reacquire(&mut self, letter: char) {
        if self.unrelease(letter) {
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
        } else {
            // followed through its journal now: no more walks
            self.walks.remove(&d.letter);
        }
        Ok(s)
    }

    /// Adds drives that appeared and drops drives that are gone.
    fn check_drives(&mut self) {
        let present = walk::drives();
        let indexed: Vec<(char, u64)> = {
            let guard = self.shared.index.read();
            guard.as_ref().expect("index").volumes().iter().map(|v| (v.letter(), v.serial)).collect()
        };
        let now = Instant::now();
        // Drives let go for a removal: another disk under that letter is
        // scanned; the same disk still there after a while was not removed.
        let mut changed: Vec<char> = Vec::new();
        let mut back: Vec<char> = Vec::new();
        for (&l, r) in &self.released {
            let Some(d) = present.iter().find(|d| d.letter == l) else { continue };
            match indexed.iter().find(|(il, _)| *il == l) {
                Some(&(_, serial)) if !walk::same_volume(serial, d.serial) => changed.push(l),
                None if now >= r.until => changed.push(l),
                Some(_) if now >= r.until => back.push(l),
                _ => {}
            }
        }
        for &l in &changed {
            self.unrelease(l);
        }
        for l in back {
            self.reacquire(l);
        }
        let due = |l: char| self.retry.get(&l).is_none_or(|r| now >= r.0);
        for d in &present {
            let known = indexed.iter().any(|(l, _)| *l == d.letter);
            if !known && !self.released.contains_key(&d.letter) && due(d.letter) && !changed.contains(&d.letter) {
                changed.push(d.letter);
            }
        }
        let mut other_disk: Vec<char> = Vec::new();
        for &(l, serial) in &indexed {
            match present.iter().find(|d| d.letter == l) {
                None => changed.push(l),
                Some(_) if self.released.contains_key(&l) => {}
                // another disk under the same letter: at once, whatever the
                // one before it was waiting for
                Some(d) if !walk::same_volume(d.serial, serial) => {
                    other_disk.push(l);
                    changed.push(l);
                }
                // could not be scanned last time: waits its turn
                Some(_) if !due(l) => {}
                // could not be scanned last time: its turn again
                Some(_) if self.retry.contains_key(&l) => changed.push(l),
                Some(_) => {}
            }
        }
        for l in other_disk {
            self.retry.remove(&l);
        }
        // a drive gone and not in the index waits for nothing any more
        self.retry.retain(|l, _| present.iter().any(|d| d.letter == *l) || indexed.iter().any(|(il, _)| il == l));
        // an NTFS drive whose volume could not be opened last time (a moment's
        // failure): opened again, and its journal followed from where it was
        self.reopen_missing(&present, &changed);
        if changed.is_empty() {
            return;
        }
        changed.sort_unstable();
        changed.dedup();
        self.log(format!("drives changed: {}", letters_text(&changed)));
        self.rescan(&changed);
    }

    /// Opens again the NTFS volumes left without a handle (`vols[v]` none)
    /// that are present, followed, and not waiting for anything else.
    fn reopen_missing(&mut self, present: &[Drive], changed: &[char]) {
        let infos: Vec<_> = self.shared.index.read().as_ref().expect("index").volumes().to_vec();
        let mut opened = false;
        for (v, info) in infos.iter().enumerate() {
            let l = info.letter();
            if v >= self.vols.len() || self.vols[v].is_some() || info.journal_id == WALKED {
                continue;
            }
            if changed.contains(&l) || self.released.contains_key(&l) || self.retry.contains_key(&l) {
                continue;
            }
            if !present.iter().any(|d| d.letter == l) {
                continue;
            }
            let Ok(vol) = Volume::open(l, false) else { continue };
            if !walk::same_volume(vol.ntfs.volume_serial_number as u64, info.serial) {
                continue; // another disk: the drive check rescans it
            }
            if let Some(r) = &self.removal {
                if let Some(n) = r.watch(vol.letter, vol.raw_handle()) {
                    self.notify.insert(vol.letter, n);
                }
            }
            self.journal_max[v] = vol.query_usn_journal().map_or(0, |j| j.maximum_size);
            self.vols[v] = Some(vol);
            // its old reader's messages were dropped meanwhile: a new one
            // replays from the position the index has
            self.stop_reader(l);
            self.log(format!("{l}: opened again"));
            opened = true;
        }
        if opened {
            self.follow_all();
        }
    }

    /// Walks drives without a journal again when it is their turn, and
    /// rebuilds only if the walk found a change. Small drives (a USB stick)
    /// are walked every minute, big ones less often.
    fn walk_due(&mut self) {
        if self.last_change.elapsed() < Duration::from_secs(5) {
            return; // the PC is busy writing files: later
        }
        let due: Vec<char> = self
            .walks
            .iter()
            .filter(|(_, (at, took, _))| at.elapsed() >= (*took * 60).clamp(WALK_MIN, WALK_MAX))
            .map(|(l, _)| *l)
            .collect();
        for letter in due {
            let Some(d) = walk::drives().into_iter().find(|d| d.letter == letter) else { continue };
            if self.released.contains_key(&letter) || self.retry.contains_key(&letter) {
                continue;
            }
            let before = self.walks.get(&letter).map(|w| w.2);
            let _bg = Background::begin();
            let s = match self.scan(&d) {
                Ok(s) => s,
                Err(e) => {
                    self.log(e);
                    // not again at once: at its usual interval
                    if let Some(w) = self.walks.get_mut(&letter) {
                        w.0 = Instant::now();
                    }
                    continue;
                }
            };
            // a journal turned on since is followed from now on
            let journal = s.journal_id != WALKED;
            if !journal && before == Some(walk::fingerprint(&s)) {
                continue;
            }
            let t = Instant::now();
            let rebuilt = {
                let guard = self.shared.index.read();
                let idx = guard.as_ref().expect("index");
                self.pool.install(|| idx.rebuild(vec![s], &[]))
            };
            self.set_index(rebuilt);
            let open_before = self.open_by_letter();
            self.reopen();
            for v in 0..self.vols.len() {
                if let Some(o) = open_before.get(&self.letter(v)) {
                    self.open[v] = o.clone();
                }
            }
            if journal {
                self.follow_all();
            }
            self.dirty = true;
            self.last_change = Instant::now();
            *self.shared.last_change.lock() = Some(self.last_change);
            if let Some(c) = self.shared.content.get() {
                c.recheck();
            }
            self.log(format!("{letter}: changes found, index updated in {} ms", t.elapsed().as_millis()));
        }
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
        // the user's exclusions when the index could not be read (damaged,
        // or of a format this version does not read), else the defaults
        let exclusions = match load_exclusions(&self.cfg.index_path) {
            Some(paths) => {
                self.log(format!("the {} saved exclusions are kept", paths.len()));
                Exclusions { paths }
            }
            None => Exclusions::defaults(windows.as_deref()),
        };
        let (idx, stats) = build(scans, &exclusions);
        self.log(format!(
            "built the index: {} entries ({} excluded) in {} ms",
            stats.kept,
            stats.excluded,
            t.elapsed().as_millis()
        ));
        Ok(idx)
    }
}

/// For each path (`C:\Folder`, or a rule like `\node_modules` that names no
/// one's folder): may the caller see it?
fn caller_sees(caller: &dyn Caller, paths: &[String]) -> Vec<bool> {
    let on_drive: Vec<String> = paths.iter().filter(|p| p.as_bytes().get(1) == Some(&b':')).cloned().collect();
    let mut ok = caller.can_list(&on_drive).into_iter();
    paths.iter().map(|p| if p.as_bytes().get(1) == Some(&b':') { ok.next().unwrap_or(false) } else { true }).collect()
}

/// A log line without the paths in it: "content folders changed: C:\Users\ana"
/// -> "content folders changed: …".
fn hide_paths(line: &str) -> String {
    let Some(slash) = line.find('\\') else { return line.to_string() };
    // from the start of the word the path begins in ("C:\…", "\\?\…")
    let start = line[..slash].rfind(' ').map_or(0, |s| s + 1);
    format!("{}…", &line[..start])
}

/// Drops the hits where `keep` is false. `total` counts exact matches only,
/// so it loses the exact ones that go. Returns how many exact ones went.
fn drop_hits(res: &mut SearchResults, keep: &[bool]) -> usize {
    let exact = res.hits.len() - res.close;
    let gone_exact = keep[..exact].iter().filter(|k| !**k).count();
    let gone_close = keep[exact..].iter().filter(|k| !**k).count();
    let mut it = keep.iter();
    res.hits.retain(|_| *it.next().unwrap_or(&false));
    res.total = res.total.saturating_sub(gone_exact);
    res.close -= gone_close;
    gone_exact
}

/// Keeps the first `limit` hits (the exact ones come first, the close ones
/// after), and counts the close ones left.
fn cut_hits(res: &mut SearchResults, limit: usize) {
    let cut = res.hits.len().saturating_sub(limit);
    res.hits.truncate(limit);
    res.close = res.close.saturating_sub(cut);
}

/// How long to wait before trying a drive again after `tries` failed scans.
fn retry_wait(tries: u32) -> Duration {
    RETRY_FIRST.saturating_mul(1 << tries.saturating_sub(1).min(16)).min(RETRY_MAX)
}

/// True when a journal of `max` bytes moved from `saved` to `now` by more
/// than SAVE_AT_JOURNAL_SHARE of its size.
fn moved_far(saved: i64, now: i64, max: f64) -> bool {
    let moved = (now - saved).max(0) as f64;
    max > 0.0 && moved > max * SAVE_AT_JOURNAL_SHARE
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
/// What each exclusion left out, kept beside the index (`<index>.skipped`,
/// one `drive<TAB>count<TAB>rule` per line): an index loaded from disk does
/// not carry it.
/// The exclusions, beside the index: one per line (written whole).
fn save_exclusions(index_path: &std::path::Path, paths: &[String]) {
    let text: String = paths.iter().map(|p| format!("{p}\n")).collect();
    let tmp = index_path.with_extension("exclusions.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, index_path.with_extension("exclusions"));
    }
}

fn load_exclusions(index_path: &std::path::Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(index_path.with_extension("exclusions")).ok()?;
    let paths: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    validate_exclusions(&paths).ok()
}

fn save_skipped(index_path: &std::path::Path, counts: &[(char, String, u64)]) {
    let text: String = counts.iter().map(|(l, r, n)| format!("{l}\t{n}\t{r}\n")).collect();
    let _ = std::fs::write(index_path.with_extension("skipped"), text);
}

fn load_skipped(index_path: &std::path::Path) -> Vec<(char, String, u64)> {
    std::fs::read_to_string(index_path.with_extension("skipped"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let letter = parts.next()?.chars().next()?;
            let n = parts.next()?.parse().ok()?;
            Some((letter, parts.next()?.to_string(), n))
        })
        .collect()
}

/// For each hit: is its folder one the caller may list? Each folder is
/// asked about once.
fn listable(idx: &NameIndex, hits: &[u32], caller: &dyn Caller) -> Vec<bool> {
    let parents: Vec<Option<u32>> = hits.iter().map(|&i| idx.parent(i)).collect();
    let mut unique: Vec<u32> = parents.iter().flatten().copied().collect();
    unique.sort_unstable();
    unique.dedup();
    let paths: Vec<String> = unique.iter().map(|&p| idx.full_path(p)).collect();
    let answers = caller.can_list(&paths);
    let ok: std::collections::HashMap<u32, bool> = unique.into_iter().zip(answers).collect();
    // a drive's own root is always listable
    parents.iter().map(|p| p.is_none_or(|p| ok.get(&p).copied().unwrap_or(false))).collect()
}

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
        let any_depth = Exclusions::is_valid_any_depth(&p);
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

    fn results(hits: &[u32], close: usize, total: usize) -> SearchResults {
        SearchResults { total, hits: hits.to_vec(), warnings: Vec::new(), close }
    }

    #[test]
    fn dropped_hits_keep_exact_and_close_ones_apart() {
        // 4 exact (of 10), 2 close; one exact and one close go
        let mut r = results(&[1, 2, 3, 4, 5, 6], 2, 10);
        let gone = drop_hits(&mut r, &[true, false, true, true, true, false]);
        assert_eq!(gone, 1);
        assert_eq!(r.hits, [1, 3, 4, 5]);
        assert_eq!((r.total, r.close), (9, 1));
        // cut to 3: the close one goes first
        cut_hits(&mut r, 3);
        assert_eq!(r.hits, [1, 3, 4]);
        assert_eq!(r.close, 0);
        // cut that keeps some close ones: exact hits are never called close
        let mut r = results(&[1, 2, 3, 4, 5], 3, 2);
        cut_hits(&mut r, 4);
        assert_eq!((r.hits.len(), r.close), (4, 2));
        let mut r = results(&[1, 2, 3], 0, 3);
        cut_hits(&mut r, 2);
        assert_eq!((r.hits.len(), r.close), (2, 0));
    }

    #[test]
    fn log_lines_lose_their_paths() {
        assert_eq!(hide_paths(r"12:00  content folders changed by a client: C:\Users\ana · D:\Work"), "12:00  content folders changed by a client: …");
        assert_eq!(hide_paths(r"exclusions changed: \node_modules"), "exclusions changed: …");
        assert_eq!(hide_paths("C: D: rescanned in 5 ms"), "C: D: rescanned in 5 ms");
    }

    #[test]
    fn only_folders_the_caller_may_list_are_shown() {
        struct OnlyD;
        impl Caller for OnlyD {
            fn can_read(&self, p: &[String]) -> Vec<bool> {
                vec![false; p.len()]
            }
            fn can_list(&self, dirs: &[String]) -> Vec<bool> {
                dirs.iter().map(|d| d.starts_with('D')).collect()
            }
        }
        let paths = vec![r"C:\Users\ana".to_string(), r"\node_modules".into(), r"D:\Work".into()];
        assert_eq!(caller_sees(&OnlyD, &paths), [false, true, true]);
    }

    #[test]
    fn a_failing_drive_is_tried_less_and_less_often() {
        assert_eq!(retry_wait(1), RETRY_FIRST);
        assert_eq!(retry_wait(2), RETRY_FIRST * 2);
        assert_eq!(retry_wait(3), RETRY_FIRST * 4);
        assert_eq!(retry_wait(40), RETRY_MAX);
        assert!(retry_wait(1) > DRIVE_CHECK);
    }

    #[test]
    fn a_save_is_due_when_the_journal_moved_far_since_the_last_one() {
        assert!(!moved_far(1000, 1000 + 100, 1000.0));
        assert!(moved_far(1000, 1000 + 300, 1000.0));
        // a walked drive has no journal size
        assert!(!moved_far(0, 1 << 40, 0.0));
        assert!(!moved_far(5000, 10, 1000.0));
    }
}
