//! Talks to the whereti service on a background thread, so the window never
//! waits on the pipe. Only the newest search matters: while one runs, newer
//! keystrokes replace each other and the older ones are never sent.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use whereti_ipc::{Client, Request, Response, SearchReply, SearchRequest, Sort, StatusReply};

pub struct Job {
    pub generation: u64,
    pub query: String,
    pub sort: Sort,
    pub limit: u32,
}

pub enum Update {
    Results { generation: u64, reply: SearchReply, round_trip: Duration },
    Status(StatusReply),
    /// The service cannot be reached (not running, or stopped meanwhile).
    Offline(String),
}

enum Msg {
    Search(Job),
    Status,
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

    pub fn ask_status(&self) {
        let _ = self.tx.send(Msg::Status);
    }

    pub fn updates(&self) -> impl Iterator<Item = Update> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}

fn worker(jobs: Receiver<Msg>, out: Sender<Update>, wake: impl Fn()) {
    let mut client: Option<Client> = None;
    while let Ok(first) = jobs.recv() {
        // Keep only the newest search; remember whether status was asked.
        let (mut search, mut status) = (None, false);
        let mut take = |m: Msg| match m {
            Msg::Search(j) => search = Some(j),
            Msg::Status => status = true,
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
        let mut failed = false;
        if let Some(job) = search {
            let req = Request::Search(SearchRequest { query: job.query, limit: job.limit, sort: job.sort });
            let t = Instant::now();
            match c.call(&req) {
                Ok(Response::Search(reply)) => {
                    let _ = out.send(Update::Results { generation: job.generation, reply, round_trip: t.elapsed() });
                }
                Ok(Response::Error(e)) => {
                    let _ = out.send(Update::Offline(e));
                }
                Ok(_) => {}
                Err(_) => failed = true,
            }
        }
        if status && !failed {
            match c.call(&Request::Status) {
                Ok(Response::Status(s)) => {
                    let _ = out.send(Update::Status(s));
                }
                Ok(_) => {}
                Err(_) => failed = true,
            }
        }
        if failed {
            // The service restarted or stopped: reconnect on the next job.
            client = None;
            let _ = out.send(Update::Offline("connection to the service was lost".into()));
        }
        wake();
    }
}
