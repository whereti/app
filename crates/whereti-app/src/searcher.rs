//! Talks to the whereti service on a background thread, so the window never
//! waits on the pipe. Only the newest search matters: while one runs, newer
//! keystrokes replace each other and the older ones are never sent.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use whereti_ipc::{
    Client, ConfigReply, ContentReply, Request, Response, ScanReply, ScanRequest, SearchReply, SearchRequest, Sort, StatusReply,
};

/// Content search waits this long after the name results, so it runs only
/// once typing pauses.
const CONTENT_DEBOUNCE: Duration = Duration::from_millis(150);
/// A change of settings is sent again for about this many seconds while
/// the service cannot be reached.
const SETTINGS_RETRIES: u32 = 60;
/// The service closes a connection idle for 60 s: reconnect a bit before.
const RECONNECT_AFTER: Duration = Duration::from_secs(30);

pub struct Job {
    pub generation: u64,
    pub query: String,
    pub sort: Sort,
    pub limit: u32,
    /// Also look for the words inside files.
    pub content: bool,
    /// The "recent" screen: answered from Windows' own recent list.
    pub recent: bool,
    /// Also look for documents close in meaning (search by meaning is on).
    pub meaning: bool,
}

pub enum Update {
    Results { generation: u64, reply: SearchReply, round_trip: Duration },
    /// Matches in the user's network folders (network.rs), after the results.
    Network { generation: u64, rows: Vec<whereti_ipc::Row> },
    Content { generation: u64, reply: ContentReply },
    /// Documents close in meaning to the query.
    Meaning { generation: u64, reply: ContentReply },
    /// What the service read from one file (the preview shows it).
    FileInfo { path: String, info: whereti_ipc::FileInfo },
    /// One step of looking in the documents not indexed yet.
    Scan { generation: u64, reply: ScanReply },
    Status(StatusReply),
    Config(ConfigReply),
    /// A change was accepted; a short message for the user.
    Done(String),
    /// The service cannot be reached (not running, or stopped meanwhile).
    Offline(String),
}

/// One step of "search the documents not indexed yet".
pub struct ScanJob {
    pub generation: u64,
    pub query: String,
    pub skip: u64,
}

enum Msg {
    Search(Job),
    Scan(ScanJob),
    Status,
    Config,
    SetExclusions(Vec<String>),
    SetPhotoTags(bool),
    SetWorkFolders(bool),
    SetMeaning(bool),
    FileInfo(String),
}

pub struct Searcher {
    tx: Sender<Msg>,
    rx: Receiver<Update>,
}

impl Searcher {
    /// `wake` is called after each update so the window repaints.
    pub fn start(wake: impl Fn() + Send + 'static) -> Searcher {
        let (tx, jobs) = mpsc::channel::<Msg>();
        let (out, rx) = mpsc::channel::<Update>();
        std::thread::Builder::new()
            .name("searcher".into())
            .spawn(move || worker(jobs, out, wake))
            .expect("searcher thread");
        Searcher { tx, rx }
    }

    pub fn search(&self, job: Job) {
        let _ = self.tx.send(Msg::Search(job));
    }

    pub fn scan(&self, job: ScanJob) {
        let _ = self.tx.send(Msg::Scan(job));
    }

    pub fn ask_status(&self) {
        let _ = self.tx.send(Msg::Status);
    }

    pub fn ask_config(&self) {
        let _ = self.tx.send(Msg::Config);
    }

    pub fn set_exclusions(&self, paths: Vec<String>) {
        let _ = self.tx.send(Msg::SetExclusions(paths));
    }

    /// Objects and animals in photos on or off (the service downloads the
    /// model the first time).
    pub fn set_photo_tags(&self, on: bool) {
        let _ = self.tx.send(Msg::SetPhotoTags(on));
    }

    /// Search by meaning on or off (the service downloads the model the
    /// first time; off forgets the documents' vectors).
    pub fn set_meaning(&self, on: bool) {
        let _ = self.tx.send(Msg::SetMeaning(on));
    }

    /// Asks what the service read from a file (the newest ask wins).
    pub fn file_info(&self, path: String) {
        let _ = self.tx.send(Msg::FileInfo(path));
    }

    /// Learning where the user works on or off (off forgets it).
    pub fn set_work_folders(&self, on: bool) {
        let _ = self.tx.send(Msg::SetWorkFolders(on));
    }


    pub fn updates(&self) -> impl Iterator<Item = Update> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}

#[derive(Default)]
struct Pending {
    search: Option<Job>,
    scan: Option<ScanJob>,
    status: bool,
    config: bool,
    exclusions: Option<Vec<String>>,
    photo_tags: Option<bool>,
    work_folders: Option<bool>,
    meaning: Option<bool>,
    file_info: Option<String>,
}

const BIT_EXCLUSIONS: u8 = 1;
const BIT_TAGS: u8 = 2;
const BIT_WORK: u8 = 4;
const BIT_MEANING: u8 = 8;

/// Which change of settings a message is (for "answered already").
fn setting_bit(m: &Msg) -> u8 {
    match m {
        Msg::SetExclusions(_) => BIT_EXCLUSIONS,
        Msg::SetPhotoTags(_) => BIT_TAGS,
        Msg::SetWorkFolders(_) => BIT_WORK,
        Msg::SetMeaning(_) => BIT_MEANING,
        _ => 0,
    }
}

/// Waits `CONTENT_DEBOUNCE` for typing to pause: false when a newer search
/// came (or the app is closing). Anything else that comes meanwhile (a
/// status poll, file details for the preview) is kept in `carried`, to be
/// handled next, and does not cancel the wait.
fn typing_pauses(jobs: &Receiver<Msg>, carried: &mut std::collections::VecDeque<Msg>) -> bool {
    let end = Instant::now() + CONTENT_DEBOUNCE;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match jobs.recv_timeout(left) {
            Ok(m @ Msg::Search(_)) => {
                carried.push_back(m);
                return false;
            }
            Ok(m) => carried.push_back(m),
            Err(mpsc::RecvTimeoutError::Timeout) => return true,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn worker(jobs: Receiver<Msg>, out: Sender<Update>, wake: impl Fn()) {
    let mut client: Option<Client> = None;
    // messages that came while a content search waited for typing to pause
    let mut carried: std::collections::VecDeque<Msg> = std::collections::VecDeque::new();
    // tries in a row to send a change of settings (about a second apart)
    let mut retries = 0u32;
    // when the connection last answered: the service lets idle ones go
    let mut last_answer = Instant::now();
    while let Some(first) = carried.pop_front().or_else(|| jobs.recv().ok()) {
        // Collapse everything queued: newest search, newest exclusions, flags.
        let mut p = Pending::default();
        let mut take = |m: Msg| match m {
            Msg::Search(j) => p.search = Some(j),
            Msg::Scan(j) => p.scan = Some(j),
            Msg::Status => p.status = true,
            Msg::Config => p.config = true,
            Msg::SetExclusions(e) => {
                p.exclusions = Some(e);
                p.config = true;
            }
            Msg::SetPhotoTags(on) => {
                p.photo_tags = Some(on);
                p.status = true;
            }
            Msg::SetWorkFolders(on) => {
                p.work_folders = Some(on);
                p.status = true;
            }
            Msg::SetMeaning(on) => {
                p.meaning = Some(on);
                p.status = true;
            }
            Msg::FileInfo(path) => p.file_info = Some(path),
        };
        take(first);
        while let Some(m) = carried.pop_front() {
            take(m);
        }
        loop {
            match jobs.try_recv() {
                Ok(m) => take(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        // a change of settings must reach the service: kept to send again
        // when the service cannot be reached (it restarts after an update)
        let settings: Vec<Msg> = [
            p.exclusions.clone().map(Msg::SetExclusions),
            p.photo_tags.map(Msg::SetPhotoTags),
            p.work_folders.map(Msg::SetWorkFolders),
            p.meaning.map(Msg::SetMeaning),
        ]
        .into_iter()
        .flatten()
        .collect();
        // settings the service answered in this batch: never sent twice
        // (a second SetExclusions would rebuild the index again)
        let answered_settings = std::cell::Cell::new(0u8);
        let keep_settings = |carried: &mut std::collections::VecDeque<Msg>, answered: u8, retries: &mut u32| {
            let left: Vec<&Msg> = settings.iter().filter(|m| setting_bit(m) & answered == 0).collect();
            if left.is_empty() {
                return;
            }
            *retries += 1;
            if *retries <= SETTINGS_RETRIES {
                for m in left.into_iter().rev() {
                    let again = match m {
                        Msg::SetExclusions(v) => Msg::SetExclusions(v.clone()),
                        Msg::SetPhotoTags(on) => Msg::SetPhotoTags(*on),
                        Msg::SetWorkFolders(on) => Msg::SetWorkFolders(*on),
                        Msg::SetMeaning(on) => Msg::SetMeaning(*on),
                        _ => continue,
                    };
                    carried.push_front(again);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        };
        // idle long enough for the service to have closed it: a fresh one,
        // so the first request after a pause does not fail
        if last_answer.elapsed() >= RECONNECT_AFTER {
            client = None;
        }
        if client.is_none() {
            match Client::connect(Duration::from_millis(300)) {
                Ok(c) => {
                    client = Some(c);
                    // reachable again: changes made from now on get their tries
                    retries = 0;
                }
                Err(e) => {
                    let msg = if e.kind() == std::io::ErrorKind::NotFound {
                        "service not running".to_string()
                    } else {
                        e.to_string()
                    };
                    let _ = out.send(Update::Offline(msg));
                    wake();
                    keep_settings(&mut carried, 0, &mut retries);
                    continue;
                }
            }
        }
        let c = client.as_mut().expect("connected");
        // when the service last answered: its idle clock starts there
        let answered_at = std::cell::Cell::new(None::<Instant>);
        let call = |c: &mut Client, req: &Request| {
            let r = c.call(req);
            if r.is_ok() {
                answered_at.set(Some(Instant::now()));
            }
            r
        };
        let done = |bit: u8| answered_settings.set(answered_settings.get() | bit);
        let result = (|| -> std::io::Result<()> {
            if let Some(on) = p.work_folders {
                let r = call(c, &Request::SetWorkFolders { on })?;
                done(BIT_WORK);
                if let Response::Error(e) = r {
                    let _ = out.send(Update::Done(e));
                }
            }
            if let Some(on) = p.meaning {
                let r = call(c, &Request::SetMeaning { on })?;
                done(BIT_MEANING);
                if let Response::Error(e) = r {
                    let _ = out.send(Update::Done(e));
                }
            }
            if let Some(on) = p.photo_tags {
                let r = call(c, &Request::SetPhotoTags { on })?;
                done(BIT_TAGS);
                if let Response::Error(e) = r {
                    let _ = out.send(Update::Done(e));
                }
            }
            if let Some(paths) = p.exclusions {
                let r = call(c, &Request::SetExclusions { paths })?;
                done(BIT_EXCLUSIONS);
                let msg = match r {
                    Response::Ok => crate::i18n::tr("Exclusions saved. The index is being rebuilt.").to_string(),
                    Response::Error(e) => crate::trf!("Could not save the exclusions: {e}", e),
                    _ => crate::i18n::tr("Unexpected reply from the service").to_string(),
                };
                let _ = out.send(Update::Done(msg));
            }
            if let Some(job) = p.search.take_if(|j| j.recent) {
                let t = Instant::now();
                let rows = crate::recent::recent_files(job.limit as usize);
                let reply = SearchReply { total: rows.len() as u64, rows, ..Default::default() };
                let _ = out.send(Update::Results { generation: job.generation, reply, round_trip: t.elapsed() });
            }
            if let Some(job) = p.search {
                let usage = crate::usage::for_query(&job.query);
                let req = Request::Search(SearchRequest { query: job.query.clone(), limit: job.limit, sort: job.sort, usage });
                let t = Instant::now();
                match call(c, &req)? {
                    Response::Search(mut reply) => {
                        let round_trip = t.elapsed();
                        refresh_rows(&mut reply);
                        let dupes = !reply.groups.is_empty();
                        let _ = out.send(Update::Results { generation: job.generation, reply, round_trip });
                        wake();
                        // then the network folders (not for dupe:, which compares local files)
                        if !dupes && crate::network::has_folders() {
                            let query = whereti_query::parse(&job.query, &crate::app::date_ctx());
                            let rows = crate::network::search(&query, job.limit as usize);
                            if !rows.is_empty() {
                                let _ = out.send(Update::Network { generation: job.generation, rows });
                                wake();
                            }
                        }
                    }
                    // the service answered: it is running, the search was refused
                    Response::Error(e) => {
                        let _ = out.send(Update::Done(e));
                    }
                    _ => {}
                }
                if job.content {
                    // A newer search cancels it; other messages wait their turn.
                    match typing_pauses(&jobs, &mut carried) {
                        false => {}
                        true => {
                            let usage = crate::usage::for_query(&job.query);
                            let req = Request::ContentSearch(SearchRequest {
                                query: job.query.clone(),
                                limit: 200,
                                sort: job.sort,
                                usage,
                            });
                            if let Response::Content(reply) = call(c, &req)? {
                                let _ = out.send(Update::Content { generation: job.generation, reply });
                                wake();
                            }
                            // then by meaning, if typing still pauses
                            if job.meaning {
                                match typing_pauses(&jobs, &mut carried) {
                                    false => {}
                                    true => {
                                        let req = Request::MeaningSearch(SearchRequest {
                                            query: job.query,
                                            limit: 20,
                                            sort: job.sort,
                                            usage: Vec::new(),
                                        });
                                        if let Response::Content(reply) = call(c, &req)? {
                                            let _ = out.send(Update::Meaning { generation: job.generation, reply });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(path) = p.file_info {
                if let Response::FileInfo(info) = call(c, &Request::FileInfo { path: path.clone() })? {
                    let _ = out.send(Update::FileInfo { path, info });
                }
            }
            if let Some(job) = p.scan {
                let req = Request::ScanUnindexed(ScanRequest { query: job.query, skip: job.skip, budget_ms: 1500, limit: 100 });
                if let Response::Scan(reply) = call(c, &req)? {
                    let _ = out.send(Update::Scan { generation: job.generation, reply });
                }
            }
            if p.config {
                if let Response::Config(cfg) = call(c, &Request::GetConfig)? {
                    let _ = out.send(Update::Config(cfg));
                }
            }
            if p.status {
                let (busy, away) = crate::shell::user_state();
                call(c, &Request::Hint { busy, away })?;
                if let Response::Status(s) = call(c, &Request::Status)? {
                    let _ = out.send(Update::Status(s));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // The service restarted or stopped: reconnect on the next job.
            client = None;
            let _ = out.send(Update::Offline("connection to the service was lost".into()));
            keep_settings(&mut carried, answered_settings.get(), &mut retries);
        } else {
            retries = 0;
        }
        if let Some(t) = answered_at.get() {
            last_answer = t;
        }
        wake();
    }
}

/// Rows read again from disk before they are shown.
const REFRESHED: usize = 60;

/// The index learns a file's size and date from the NTFS journal, which
/// writes them for a file held open for writing (a log, a database) only
/// when it is closed. So the first rows, the ones on screen, take their size
/// and date from the disk itself, and files deleted a moment ago are dropped.
fn refresh_rows(reply: &mut SearchReply) {
    let (mut gone, mut gone_close) = (0u64, 0u64);
    let first_close = reply.rows.len().saturating_sub(reply.close as usize);
    // dupe: rows come in sets: keep them all (the next search drops what went)
    let dupes = !reply.groups.is_empty();
    let mut k = 0;
    reply.rows.retain_mut(|row| {
        k += 1;
        if k > REFRESHED || row.path.starts_with(r"\\") {
            return true;
        }
        match std::fs::metadata(&row.path) {
            Ok(m) => {
                if !m.is_dir() {
                    row.size = m.len();
                }
                if let Ok(t) = m.modified() {
                    row.modified = crate::recent::minutes_since_1601(t);
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dupes => {
                if k > first_close {
                    gone_close += 1;
                } else {
                    gone += 1;
                }
                false
            }
            Err(_) => true,
        }
    });
    reply.total = reply.total.saturating_sub(gone);
    reply.close = reply.close.saturating_sub(gone_close);
}
