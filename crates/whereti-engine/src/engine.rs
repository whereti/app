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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use whereti_core::{build, snapshot, ApplyStats, Exclusions, NameIndex, SearchOptions, SortBy};
use whereti_ipc::{
    Caller, ConfigReply, Request, Response, Row, SearchReply, Sort, StatusReply, VolumeStatus, PROTOCOL_VERSION,
};
use whereti_ntfs::Volume;

use crate::content::{self, Content};
use crate::log::Logger;
use crate::sync::{self, Batch, JournalMsg, JournalRead, Prefetched};
use crate::system::{self, Background};

/// Re-read files still open for writing this often.
const OPEN_REFRESH: Duration = Duration::from_secs(30);
/// Collect journal records for this long before applying them as one batch.
const BATCH_WINDOW: Duration = Duration::from_millis(250);
/// Daily check only after this long without file changes.
const IDLE_FOR_RECONCILE: Duration = Duration::from_secs(60);
/// Compact when renamed/added/removed entries exceed this share of the index.
const COMPACT_AT: f64 = 0.01;
/// The most results one search may return.
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
                let res = idx.search(&query, &opts);
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
                Response::Content(self.content.search(idx, &query, r.limit.min(MAX_LIMIT) as usize, caller))
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
                })
            }
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    cfg: Config,
    tx: mpsc::Sender<Msg>,
    vols: Vec<Volume>,
    open: Vec<HashSet<u64>>,
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
                self.log(format!("no usable index ({e}) — building one"));
                self.shared.set_state("indexing");
                let idx = self.build_fresh()?;
                self.dirty = true;
                idx
            }
        };
        self.vols = sync::open_volumes(&index)?;
        self.open = vec![HashSet::new(); self.vols.len()];
        self.journal_max = self.vols.iter().map(|v| v.query_usn_journal().map_or(0, |j| j.maximum_size)).collect();
        self.saved_usn = index.volumes().iter().map(|v| v.next_usn).collect();
        *self.shared.index.write() = Some(index);

        self.shared.set_state("catching up");
        let mut rescan = Vec::new();
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
        for v in 0..self.vols.len() {
            self.spawn_follower(v);
        }
        self.shared.set_state("ready");
        self.log(format!("ready in {} ms", t.elapsed().as_millis()));
        Ok(())
    }

    fn letter(&self, v: usize) -> char {
        self.shared.index.read().as_ref().map(|i| i.volumes()[v].letter()).unwrap_or('?')
    }

    /// Replays the journal for volume `v`. `None` if it must be rescanned.
    fn catch_up(&mut self, v: usize) -> Result<Option<(usize, ApplyStats)>, String> {
        let info = self.shared.index.read().as_ref().expect("index").volumes()[v];
        match sync::read_since(&info, &self.vols[v], &mut self.open[v])? {
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
        let mut pre = Prefetched::read(&self.vols[v], &batch.records).map_err(|e| format!("reading MFT records: {e}"))?;
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
        if let (Some(c), Some(idx)) = (self.shared.content.get(), self.shared.index.read().as_ref()) {
            c.note_changes(idx, v, &records);
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
            let mine: Vec<&JournalMsg> = msgs.iter().filter(|m| m.vol == v).collect();
            if mine.is_empty() {
                continue;
            }
            let letter = self.letter(v);
            if let Some(err) = mine.iter().find_map(|m| m.error.as_ref()) {
                self.log(format!("{err} → full rescan"));
                self.rescan(&[letter])?;
                self.spawn_follower(v);
                continue;
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
        if self.last_refresh.elapsed() >= OPEN_REFRESH {
            self.last_refresh = Instant::now();
            for v in 0..self.vols.len() {
                if self.open[v].is_empty() {
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
        let mut fresh = Vec::new();
        for &l in letters {
            fresh.extend(sync::scan_volume(l, self.cfg.threads)?);
        }
        let rebuilt = {
            let guard = self.shared.index.read();
            let idx = guard.as_ref().expect("index");
            self.pool.install(|| idx.rebuild_with(fresh))
        };
        *self.shared.index.write() = Some(rebuilt);
        for (v, open) in self.open.iter_mut().enumerate() {
            if letters.contains(&self.shared.index.read().as_ref().expect("index").volumes()[v].letter()) {
                open.clear();
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
        for &l in &letters {
            scans.extend(sync::scan_volume(l, self.cfg.threads)?);
        }
        let (fresh, stats) = self.pool.install(|| build(scans, &Exclusions { paths }));
        *self.shared.index.write() = Some(fresh);
        for open in &mut self.open {
            open.clear();
        }
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

    fn spawn_follower(&self, v: usize) {
        let info = self.shared.index.read().as_ref().expect("index").volumes()[v];
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new()
            .name(format!("journal-{}", info.letter()))
            .spawn(move || sync::follow_journal::<Msg>(v, info.letter(), info.journal_id, info.next_usn, tx));
    }

    fn build_fresh(&mut self) -> Result<NameIndex, String> {
        let t = Instant::now();
        let mut scans = Vec::new();
        for letter in sync::fixed_drives() {
            if let Some(s) = sync::scan_volume(letter, self.cfg.threads)? {
                self.log(format!("{letter}: scanned {} entries", s.scan.entries.len()));
                scans.push(s);
            }
        }
        if scans.is_empty() {
            return Err("no NTFS volume to index".into());
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
