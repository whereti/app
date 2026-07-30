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

pub struct Job {
    pub generation: u64,
    pub query: String,
    pub sort: Sort,
    pub limit: u32,
    /// Also look for the words inside files.
    pub content: bool,
    /// The "recent" screen: answered from Windows' own recent list.
    pub recent: bool,
}

pub enum Update {
    /// `network`: matches in the user's network folders (network.rs).
    Results { generation: u64, reply: SearchReply, round_trip: Duration, network: Vec<whereti_ipc::Row> },
    Content { generation: u64, reply: ContentReply },
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
}

fn worker(jobs: Receiver<Msg>, out: Sender<Update>, wake: impl Fn()) {
    let mut client: Option<Client> = None;
    let mut carried: Option<Msg> = None;
    while let Some(first) = carried.take().or_else(|| jobs.recv().ok()) {
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
        };
        take(first);
        loop {
            match jobs.try_recv() {
                Ok(m) => take(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if client.is_none() {
            match Client::connect(Duration::from_millis(300)) {
                Ok(c) => client = Some(c),
                Err(e) => {
                    let msg = if e.kind() == std::io::ErrorKind::NotFound {
                        "service not running".to_string()
                    } else {
                        e.to_string()
                    };
                    let _ = out.send(Update::Offline(msg));
                    wake();
                    continue;
                }
            }
        }
        let c = client.as_mut().expect("connected");
        let result = (|| -> std::io::Result<()> {
            if let Some(on) = p.work_folders {
                if let Response::Error(e) = c.call(&Request::SetWorkFolders { on })? {
                    let _ = out.send(Update::Done(e));
                }
            }
            if let Some(on) = p.photo_tags {
                if let Response::Error(e) = c.call(&Request::SetPhotoTags { on })? {
                    let _ = out.send(Update::Done(e));
                }
            }
            if let Some(paths) = p.exclusions {
                let msg = match c.call(&Request::SetExclusions { paths })? {
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
                let _ = out.send(Update::Results { generation: job.generation, reply, round_trip: t.elapsed(), network: Vec::new() });
            }
            if let Some(job) = p.search {
                let usage = crate::usage::for_query(&job.query);
                let req = Request::Search(SearchRequest { query: job.query.clone(), limit: job.limit, sort: job.sort, usage });
                let t = Instant::now();
                match c.call(&req)? {
                    Response::Search(mut reply) => {
                        let round_trip = t.elapsed();
                        refresh_rows(&mut reply);
                        let network = if reply.groups.is_empty() {
                            let query = whereti_query::parse(&job.query, &crate::app::date_ctx());
                            crate::network::search(&query, job.limit as usize)
                        } else {
                            Vec::new()
                        };
                        let _ = out.send(Update::Results { generation: job.generation, reply, round_trip, network });
                        wake();
                    }
                    Response::Error(e) => {
                        let _ = out.send(Update::Offline(e));
                    }
                    _ => {}
                }
                if job.content {
                    // A newer keystroke cancels it; the new message is handled next.
                    match jobs.recv_timeout(CONTENT_DEBOUNCE) {
                        Ok(m) => carried = Some(m),
                        Err(_) => {
                            let usage = crate::usage::for_query(&job.query);
                            let req = Request::ContentSearch(SearchRequest {
                                query: job.query,
                                limit: 200,
                                sort: job.sort,
                                usage,
                            });
                            if let Response::Content(reply) = c.call(&req)? {
                                let _ = out.send(Update::Content { generation: job.generation, reply });
                            }
                        }
                    }
                }
            }
            if let Some(job) = p.scan {
                let req = Request::ScanUnindexed(ScanRequest { query: job.query, skip: job.skip, budget_ms: 1500, limit: 100 });
                if let Response::Scan(reply) = c.call(&req)? {
                    let _ = out.send(Update::Scan { generation: job.generation, reply });
                }
            }
            if p.config {
                if let Response::Config(cfg) = c.call(&Request::GetConfig)? {
                    let _ = out.send(Update::Config(cfg));
                }
            }
            if p.status {
                let (busy, away) = crate::shell::user_state();
                c.call(&Request::Hint { busy, away })?;
                if let Response::Status(s) = c.call(&Request::Status)? {
                    let _ = out.send(Update::Status(s));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // The service restarted or stopped: reconnect on the next job.
            client = None;
            let _ = out.send(Update::Offline("connection to the service was lost".into()));
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
