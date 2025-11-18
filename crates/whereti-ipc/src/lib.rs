//! Protocol between the whereti service and its clients (UI, `wt`), over a
//! local named pipe (PLAN.md §11).
//!
//! Messages are length-prefixed `postcard`. The pipe accepts only local
//! clients; interactive users may connect and send requests, but not create
//! pipe instances, so they cannot take over the service's pipe name.

#![cfg(windows)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

pub const PIPE_NAME: &str = r"\\.\pipe\whereti";
pub const PROTOCOL_VERSION: u32 = 7;
const MAX_MESSAGE: usize = 64 << 20;

/// The account that created the pipe (OW), SYSTEM and Administrators: full
/// access. Interactive users: read + write data only, no
/// FILE_CREATE_PIPE_INSTANCE. Nothing for network logons.
const PIPE_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012008b;;;IU)";
/// What a client asks for: FILE_GENERIC_READ | FILE_WRITE_DATA.
const CLIENT_ACCESS: u32 = 0x0012_0089 | 0x0000_0002;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Sort {
    #[default]
    Relevance,
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: u32,
    pub sort: Sort,
    /// Files this user opens often or lately that may match (full path,
    /// weight 1-12): ranked higher. Kept on the user's PC by the app; the
    /// service only uses them for this one search.
    pub usage: Vec<(String, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    Search(SearchRequest),
    Status,
    /// What the index covers and leaves out.
    GetConfig,
    /// Replace the exclusion paths; the service rebuilds the index (seconds).
    SetExclusions { paths: Vec<String> },
    /// Files whose text contains the plain words of the query (or its
    /// `content:` terms); the other conditions (ext:, path:, dm:…) filter them.
    /// Only files the caller may read are returned.
    ContentSearch(SearchRequest),
    /// Folders whose documents are indexed by content.
    SetContentFolders { paths: Vec<String> },
    /// What the user is doing, as seen from their session: `busy` = a
    /// full-screen game or video, or a presentation; `away` = screen locked
    /// or screen saver. Background work pauses or speeds up accordingly.
    Hint { busy: bool, away: bool },
    /// Look inside the documents the content index has not read yet (while
    /// it is still working), one step of about `budget_ms` at a time; call
    /// again with `skip = next` until `done`.
    ScanUnindexed(ScanRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConfigReply {
    /// Indexed drive letters.
    pub volumes: Vec<char>,
    /// Paths left out of the index (`C:\Windows`, or `\Folder` on every drive).
    pub exclusions: Vec<String>,
    /// The exclusions a fresh install uses.
    pub default_exclusions: Vec<String>,
    /// Folders indexed by content.
    pub content_folders: Vec<String>,
    pub default_content_folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Last write, minutes since 1601-01-01 UTC.
    pub modified: u32,
    pub flags: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SearchReply {
    pub total: u64,
    pub rows: Vec<Row>,
    pub warnings: Vec<String>,
    /// Time spent searching inside the service.
    pub micros: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContentReply {
    /// Best first.
    pub rows: Vec<Row>,
    /// For each row: the other files with exactly the same content (same
    /// size and same bytes), shown as one result.
    pub copies: Vec<Vec<String>>,
    /// Files whose text matches, before filters and the limit.
    pub total: u64,
    pub micros: u64,
    /// Said under the results, e.g. "indexing: 1,234 of 5,000 documents".
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScanRequest {
    pub query: String,
    /// Documents already looked at in earlier steps.
    pub skip: u64,
    pub budget_ms: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScanReply {
    /// Documents of this step whose text has every word.
    pub rows: Vec<Row>,
    /// Where the next step starts.
    pub next: u64,
    /// Documents not indexed yet, in all.
    pub total: u64,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContentStatus {
    /// "starting", "scanning", "indexing", "ready", "error: …", or
    /// "paused: battery" / "paused: fullscreen" / "paused: busy" (the CPU)
    pub state: String,
    /// Documents in the index.
    pub files: u64,
    /// Waiting to be (re)indexed.
    pub queued: u64,
    /// Indexed in this round (for a progress bar: done / (done + queued)).
    pub done: u64,
    /// Could not be read (damaged, protected, too large).
    pub failed: u64,
    pub disk_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub letter: char,
    pub next_usn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StatusReply {
    /// "loading", "indexing", "catching up", "ready", "rescanning C:", …
    pub state: String,
    pub entries: u64,
    pub volumes: Vec<VolumeStatus>,
    pub uptime_secs: u64,
    pub cpu_ms: u64,
    pub private_bytes: u64,
    pub last_change_secs: Option<u64>,
    pub open_files: u64,
    pub recent_log: Vec<String>,
    pub content: ContentStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32, server: String },
    Search(SearchReply),
    Content(ContentReply),
    Status(StatusReply),
    Config(ConfigReply),
    Ok,
    Error(String),
    Scan(ScanReply),
}

pub fn write_message<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(&bytes);
    w.write_all(&frame)?;
    w.flush()
}

/// `Ok(None)` when the other side closed the connection cleanly.
pub fn read_message<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
        Err(e) if e.raw_os_error() == Some(109) => return Ok(None), // ERROR_BROKEN_PIPE
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A connection to the service.
pub struct Client {
    pipe: File,
}

impl Client {
    /// Connects, waiting up to `timeout` if every pipe instance is busy.
    /// Fails fast with `NotFound` when the service is not running.
    pub fn connect(timeout: Duration) -> io::Result<Client> {
        Self::connect_to(PIPE_NAME, timeout)
    }

    pub fn connect_to(name: &str, timeout: Duration) -> io::Result<Client> {
        let start = Instant::now();
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        loop {
            match OpenOptions::new().access_mode(CLIENT_ACCESS).open(name) {
                Ok(pipe) => {
                    let mut c = Client { pipe };
                    match c.call(&Request::Hello { version: PROTOCOL_VERSION })? {
                        Response::Hello { version, .. } if version == PROTOCOL_VERSION => return Ok(c),
                        Response::Hello { version, server } => {
                            return Err(io::Error::other(format!(
                                "service speaks protocol v{version} ({server}), this client v{PROTOCOL_VERSION}"
                            )))
                        }
                        other => return Err(io::Error::other(format!("unexpected handshake reply: {other:?}"))),
                    }
                }
                Err(e) if e.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY: wait for a free instance
                    let left = timeout.saturating_sub(start.elapsed());
                    if left.is_zero() {
                        return Err(e);
                    }
                    // SAFETY: NUL-terminated name.
                    unsafe { WaitNamedPipeW(wide.as_ptr(), left.as_millis().max(1) as u32) };
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn call(&mut self, req: &Request) -> io::Result<Response> {
        write_message(&mut self.pipe, req)?;
        read_message(&mut self.pipe)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "service closed the connection"))
    }
}

/// Who sent a request: lets the service check what that user may read.
pub trait Caller {
    /// For each path: may the caller open it for reading?
    fn can_read(&self, paths: &[String]) -> Vec<bool>;
}

/// A caller allowed everything (in-process use and tests).
pub struct Trusted;

impl Caller for Trusted {
    fn can_read(&self, paths: &[String]) -> Vec<bool> {
        vec![true; paths.len()]
    }
}

/// The client at the other end of a pipe: checked by impersonating it, so
/// Windows applies that user's own permissions.
struct PipeCaller(std::os::windows::io::RawHandle);

impl Caller for PipeCaller {
    fn can_read(&self, paths: &[String]) -> Vec<bool> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::RevertToSelf;
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        const FILE_READ_DATA: u32 = 0x1;
        // SAFETY: valid pipe handle of a connected client.
        if unsafe { ImpersonateNamedPipeClient(self.0 as _) } == 0 {
            return vec![false; paths.len()];
        }
        let out = paths
            .iter()
            .map(|p| {
                let wide: Vec<u16> = p.encode_utf16().chain([0]).collect();
                // SAFETY: NUL-terminated path; the handle is closed right away.
                let h = unsafe {
                    CreateFileW(
                        wide.as_ptr(),
                        FILE_READ_DATA,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if h == INVALID_HANDLE_VALUE {
                    // A file locked by its writer is still the caller's to see.
                    io::Error::last_os_error().raw_os_error() == Some(32) // ERROR_SHARING_VIOLATION
                } else {
                    unsafe { CloseHandle(h) };
                    true
                }
            })
            .collect();
        // SAFETY: undo the impersonation on this thread.
        unsafe { RevertToSelf() };
        out
    }
}

pub type Handler = Arc<dyn Fn(Request, &dyn Caller) -> Response + Send + Sync>;

/// Serves the pipe on background threads (one per client) until `stop()`.
pub struct Server {
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(handler: Handler) -> io::Result<Server> {
        Self::start_named(PIPE_NAME, handler)
    }

    pub fn start_named(name: &str, handler: Handler) -> io::Result<Server> {
        let stop = Arc::new(AtomicBool::new(false));
        // Create the first instance now, so start() fails if the name is taken.
        let first = create_instance(name, true)?;
        let (stop2, name2) = (stop.clone(), name.to_string());
        let thread = std::thread::Builder::new()
            .name("pipe-accept".into())
            .spawn(move || accept_loop(&name2, first, handler, stop2))?;
        Ok(Server { name: name.to_string(), stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop blocked in ConnectNamedPipe.
        let _ = OpenOptions::new().access_mode(CLIENT_ACCESS).open(&self.name);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(name: &str, mut instance: OwnedHandle, handler: Handler, stop: Arc<AtomicBool>) {
    loop {
        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&instance);
        // SAFETY: valid pipe handle; synchronous call.
        let ok = unsafe { ConnectNamedPipe(raw as _, std::ptr::null_mut()) };
        let connected = ok != 0 || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if connected {
            let handler = handler.clone();
            let _ = std::thread::Builder::new()
                .name("pipe-client".into())
                .spawn(move || serve_client(instance, handler));
        }
        // A fresh instance for the next client.
        instance = loop {
            match create_instance(name, false) {
                Ok(h) => break h,
                Err(_) if stop.load(Ordering::SeqCst) => return,
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        };
    }
}

fn serve_client(handle: OwnedHandle, handler: Handler) {
    let mut pipe = File::from(handle);
    loop {
        let req: Request = match read_message(&mut pipe) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                let _ = write_message(&mut pipe, &Response::Error(format!("bad request: {e}")));
                return;
            }
        };
        let caller = PipeCaller(std::os::windows::io::AsRawHandle::as_raw_handle(&pipe));
        let resp = handler(req, &caller);
        if write_message(&mut pipe, &resp).is_err() {
            return;
        }
    }
}

fn create_instance(pipe_name: &str, first: bool) -> io::Result<OwnedHandle> {
    let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated SDDL; sd is freed with LocalFree below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut())
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = pipe_name.encode_utf16().chain([0]).collect();
    let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: all pointers valid for the duration of the call.
    let h = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            1 << 16,
            1 << 16,
            0,
            &sa,
        )
    };
    let err = io::Error::last_os_error();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(sd) };
    if h == INVALID_HANDLE_VALUE {
        return Err(err);
    }
    // SAFETY: fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let resp = Response::Search(SearchReply {
            total: 3,
            rows: vec![Row { path: r"C:\Users\ana\Mașină.docx".into(), is_dir: false, size: 10, modified: 5, flags: 0 }],
            warnings: vec![],
            micros: 1200,
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();
        let back: Response = read_message(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(back, resp);
        let empty: Option<Response> = read_message(&mut [].as_slice()).unwrap();
        assert!(empty.is_none());
    }

    #[test]
    fn client_and_server_talk_over_a_real_pipe() {
        let name = format!(r"\\.\pipe\whereti-test-{}", std::process::id());
        let handler: Handler = Arc::new(|req, _caller| match req {
            Request::Hello { .. } => Response::Hello { version: PROTOCOL_VERSION, server: "test".into() },
            Request::Search(s) => Response::Search(SearchReply { total: s.limit as u64, ..Default::default() }),
            // report what the impersonated caller may read
            Request::SetContentFolders { paths } => Response::Error(format!("{:?}", _caller.can_read(&paths))),
            other => Response::Error(format!("unsupported in this test: {other:?}")),
        });
        let server = Server::start_named(&name, handler.clone()).unwrap();
        // the name is taken: a second server must not start
        assert!(Server::start_named(&name, handler).is_err());
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let name = name.clone();
                std::thread::spawn(move || {
                    let mut c = Client::connect_to(&name, Duration::from_secs(5)).unwrap();
                    for k in 0..50u32 {
                        let req = Request::Search(SearchRequest { query: "x".into(), limit: t * 100 + k, sort: Sort::Name, usage: Vec::new() });
                        match c.call(&req).unwrap() {
                            Response::Search(r) => assert_eq!(r.total, (t * 100 + k) as u64),
                            other => panic!("{other:?}"),
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut c = Client::connect_to(&name, Duration::from_secs(5)).unwrap();
        let mine = std::env::current_exe().unwrap().display().to_string();
        let paths = vec![mine, r"C:\definitely
ot\here.txt".to_string()];
        assert_eq!(c.call(&Request::SetContentFolders { paths }).unwrap(), Response::Error("[true, false]".into()));
        drop(c);
        server.stop();
        assert!(Client::connect_to(&name, Duration::from_millis(100)).is_err(), "pipe must be gone after stop");
    }

    #[test]
    fn oversized_or_garbage_frames_are_rejected() {
        let mut huge = (MAX_MESSAGE as u32 + 1).to_le_bytes().to_vec();
        huge.extend([0; 8]);
        assert!(read_message::<Request>(&mut huge.as_slice()).is_err());
        let garbage = [3u8, 0, 0, 0, 0xFF, 0xFF, 0xFF];
        assert!(read_message::<Request>(&mut garbage.as_slice()).is_err());
    }
}
