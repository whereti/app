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
use std::sync::{Arc, Mutex};
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
pub const PROTOCOL_VERSION: u32 = 12;
const MAX_MESSAGE: usize = 64 << 20;
/// What the service reads from a client in one request: requests are small
/// (500 exclusion paths fit easily), so no client makes it hold 64 MB.
const MAX_REQUEST: usize = 2 << 20;
/// Clients served at once; more wait for a free one, so no one can make the
/// service start thread after thread.
const MAX_CLIENTS: usize = 64;
/// Clients of one account at once, so one account cannot take them all.
const MAX_PER_USER: usize = 16;
/// A client that sends nothing (or reads no reply) for this long is let go,
/// so idle connections cannot keep every slot; the app and `wt` connect
/// again when they need to.
const IDLE: Duration = Duration::from_secs(60);

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
    /// Recognise objects and animals in photos (downloads the model the
    /// first time it is turned on).
    SetPhotoTags { on: bool },
    /// Search documents by meaning (downloads the text model the first
    /// time it is turned on; off forgets the documents' vectors).
    SetMeaning { on: bool },
    /// Documents close in meaning to the plain words of the query (two at
    /// least); the other conditions filter them. Answered as `Content`.
    MeaningSearch(SearchRequest),
    /// What whereti read from one file: what is in a photo, when and with
    /// what it was taken, a song's artist… (only if the caller may read it).
    FileInfo { path: String },
    /// Learn where the user works (the folders where their files change)
    /// and rank those first; off forgets what was learnt.
    SetWorkFolders { on: bool },
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
    /// How many files and folders each exclusion left out at the last build.
    #[serde(default)]
    pub skipped: Vec<(String, u64)>,
    /// The generated-folder rules by group (id, rules), which Settings lets
    /// people take back a group at a time.
    #[serde(default)]
    pub groups: Vec<(String, Vec<String>)>,
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
    /// The last `close` rows are close matches (a word one letter off),
    /// not counted in `total`.
    pub close: u64,
    /// `dupe:`: the rows come in sets of identical files, of these sizes
    /// (`total` is then the number of sets).
    pub groups: Vec<u32>,
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
    /// Objects and animals in photos (PLAN.md §55): "off", "on",
    /// "downloading:<bytes>/<total>" (the model) or "error: …".
    pub tags: String,
    /// Search by meaning (PLAN.md §65): the same states as `tags`.
    pub meaning: String,
}

/// What whereti read from a file (see `Request::FileInfo`). Zero / empty:
/// not known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileInfo {
    /// When the photo was taken, minutes since 1601 UTC.
    pub taken: u32,
    pub width: u32,
    pub height: u32,
    /// Seconds.
    pub duration: u32,
    pub camera: String,
    pub artist: String,
    pub album: String,
    pub author: String,
    /// What the photo model saw: label numbers (whereti_extract::tags::labels).
    pub tags: Vec<u16>,
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
    /// Where the user works lately, hottest first (a few folders); `None`
    /// when learning it is off.
    pub work_folders: Option<Vec<String>>,
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
    FileInfo(FileInfo),
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
    read_message_max(r, MAX_MESSAGE)
}

/// `read_message`, with a smaller ceiling.
fn read_message_max<T: for<'de> Deserialize<'de>>(r: &mut impl Read, max: usize) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
        Err(e) if e.raw_os_error() == Some(109) => return Ok(None), // ERROR_BROKEN_PIPE
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
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
                    check_server(&pipe)?;
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

/// The service's name in the Service Control Manager.
const SERVICE_NAME: &str = "whereti";

/// Refuses a pipe that someone else created under whereti's name. When the
/// service is not running the name is free, and any program could create it
/// to read the searches sent to it. Accepted: the whereti service itself (the
/// process the Service Control Manager runs for it), an elevated process
/// (`wt serve` from an administrator terminal), or this process (tests).
fn check_server(pipe: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    let mut pid = 0u32;
    // SAFETY: a valid client pipe handle and an out value.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as _, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == std::process::id() || service_pid() == Some(pid) || is_elevated(pid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the whereti pipe is served by another program (process {pid}), not the whereti service"),
        ))
    }
}

/// The process the running whereti service is in.
fn service_pid() -> Option<u32> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
        SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
    };
    let name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    // SAFETY: handles are checked and closed; the buffer is a
    // SERVICE_STATUS_PROCESS of the size passed.
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return None;
        }
        let svc = OpenServiceW(scm, name.as_ptr(), SERVICE_QUERY_STATUS);
        let mut out = None;
        if !svc.is_null() {
            let mut st: SERVICE_STATUS_PROCESS = std::mem::zeroed();
            let mut needed = 0u32;
            let ok = QueryServiceStatusEx(
                svc,
                SC_STATUS_PROCESS_INFO,
                &mut st as *mut _ as *mut u8,
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            );
            if ok != 0 && st.dwProcessId != 0 {
                out = Some(st.dwProcessId);
            }
            CloseServiceHandle(svc);
        }
        CloseServiceHandle(scm);
        out
    }
}

/// Runs with administrator rights (only an administrator could have made it).
fn is_elevated(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: handles are checked and closed; the out value is a
    // TOKEN_ELEVATION of the size passed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut token = std::ptr::null_mut();
        let mut elevated = false;
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0 {
            let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            if GetTokenInformation(token, TokenElevation, &mut e as *mut _ as *mut _, size_of::<TOKEN_ELEVATION>() as u32, &mut len) != 0 {
                elevated = e.TokenIsElevated != 0;
            }
            CloseHandle(token);
        }
        CloseHandle(process);
        elevated
    }
}

/// Who sent a request: lets the service check what that user may read.
pub trait Caller {
    /// For each path: may the caller open it for reading?
    fn can_read(&self, paths: &[String]) -> Vec<bool>;
    /// May the caller change what the service does for everyone (what is
    /// indexed, what is read)? An administrator's account, elevated or not.
    fn is_admin(&self) -> bool {
        false
    }
    /// For each folder: may the caller list it (see the names in it, as in
    /// Explorer)? A name is shown only from a folder its caller can list.
    fn can_list(&self, dirs: &[String]) -> Vec<bool> {
        vec![true; dirs.len()]
    }
}

/// A caller allowed everything (in-process use and tests).
pub struct Trusted;

impl Caller for Trusted {
    fn can_read(&self, paths: &[String]) -> Vec<bool> {
        vec![true; paths.len()]
    }
    fn is_admin(&self) -> bool {
        true
    }
}

/// `C:\…` as `\\?\C:\…` and `\\server\share\…` as `\\?\UNC\server\share\…`,
/// so a path of 260 characters or more, or a name ending in a dot or a
/// space, is opened as it is. A path Windows would have to tidy up first
/// (`.` or `..` in it, a doubled backslash) is left as it was.
fn long_path(p: &str) -> String {
    let p = p.replace('/', "\\");
    if p.starts_with(r"\\?\") || p.starts_with(r"\\.\") {
        return p;
    }
    let rest = p.strip_prefix(r"\\").unwrap_or(&p);
    if rest.contains(r"\\") || rest.split('\\').any(|c| c == "." || c == "..") {
        return p;
    }
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        format!(r"\\?\{p}")
    } else if p.starts_with(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else {
        p
    }
}

/// The client at the other end of a pipe: checked by impersonating it, so
/// Windows applies that user's own permissions.
struct PipeCaller(std::os::windows::io::RawHandle);

impl PipeCaller {
    /// The client's account (its SID, as bytes).
    fn user(&self) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{GetLengthSid, GetTokenInformation, RevertToSelf, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
        // SAFETY: valid pipe handle of a connected client.
        if unsafe { ImpersonateNamedPipeClient(self.0 as _) } == 0 {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the impersonation token of this thread; closed below.
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0;
        // SAFETY: back to the service's own rights before anything else.
        unsafe { RevertToSelf() };
        if !opened {
            return None;
        }
        let mut needed = 0u32;
        // SAFETY: size query, then a buffer of that size (8-aligned).
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
        let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) } != 0;
        let sid = ok.then(|| {
            // SAFETY: TOKEN_USER as Windows wrote it; its SID points into `buf`.
            let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
            let len = unsafe { GetLengthSid(user.User.Sid) } as usize;
            unsafe { std::slice::from_raw_parts(user.User.Sid as *const u8, len) }.to_vec()
        });
        // SAFETY: the token we opened.
        unsafe { CloseHandle(token) };
        sid
    }

    /// Opens each path as the client, asking for `access` (`flags` for
    /// CreateFileW): whether Windows lets that user have it.
    fn may_open(&self, paths: &[String], access: u32, flags: u32) -> Vec<bool> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::RevertToSelf;
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        // SAFETY: valid pipe handle of a connected client.
        if unsafe { ImpersonateNamedPipeClient(self.0 as _) } == 0 {
            return vec![false; paths.len()];
        }
        let out = paths
            .iter()
            .map(|p| {
                let wide: Vec<u16> = long_path(p).encode_utf16().chain([0]).collect();
                // SAFETY: NUL-terminated path; the handle is closed right away.
                let h = unsafe {
                    CreateFileW(
                        wide.as_ptr(),
                        access,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        flags,
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

impl Caller for PipeCaller {
    fn can_list(&self, dirs: &[String]) -> Vec<bool> {
        const FILE_LIST_DIRECTORY: u32 = 0x1;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000; // needed to open a folder
        self.may_open(dirs, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS)
    }

    fn can_read(&self, paths: &[String]) -> Vec<bool> {
        const FILE_READ_DATA: u32 = 0x1;
        self.may_open(paths, FILE_READ_DATA, 0)
    }

    /// The client's token holds the Administrators group, enabled or (not
    /// elevated) for deny only: the account is one of the PC's
    /// administrators. A standard account (a child's, a guest's) is not.
    fn is_admin(&self) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, IsWellKnownSid, RevertToSelf, TokenGroups, WinBuiltinAdministratorsSid, TOKEN_GROUPS,
            TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
        // SAFETY: valid pipe handle of a connected client.
        if unsafe { ImpersonateNamedPipeClient(self.0 as _) } == 0 {
            return false;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the impersonation token of this thread; closed below.
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0;
        // SAFETY: back to the service's own rights before anything else.
        unsafe { RevertToSelf() };
        if !opened {
            return false;
        }
        let mut needed = 0u32;
        // SAFETY: size query, then a buffer of that size (8-aligned).
        unsafe { GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut needed) };
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
        let ok = unsafe { GetTokenInformation(token, TokenGroups, buf.as_mut_ptr().cast(), needed, &mut needed) } != 0;
        let admin = ok && {
            // SAFETY: TOKEN_GROUPS as Windows wrote it: a count, then that many entries.
            let groups = unsafe { &*(buf.as_ptr() as *const TOKEN_GROUPS) };
            let list = unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
            // SAFETY: each SID points into `buf`.
            list.iter().any(|g| unsafe { IsWellKnownSid(g.Sid, WinBuiltinAdministratorsSid) } != 0)
        };
        // SAFETY: the token we opened.
        unsafe { CloseHandle(token) };
        admin
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
        Self::start_with_idle(name, handler, IDLE)
    }

    /// `start_named`, letting go of clients idle for `idle`.
    fn start_with_idle(name: &str, handler: Handler, idle: Duration) -> io::Result<Server> {
        let stop = Arc::new(AtomicBool::new(false));
        // Create the first instance now, so start() fails if the name is taken.
        let first = create_instance(name, true)?;
        let (stop2, name2) = (stop.clone(), name.to_string());
        let thread = std::thread::Builder::new()
            .name("pipe-accept".into())
            .spawn(move || accept_loop(&name2, first, handler, stop2, idle))?;
        Ok(Server { name: name.to_string(), stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        use std::os::windows::io::AsRawHandle;
        self.stop.store(true, Ordering::SeqCst);
        let Some(t) = self.thread.take() else { return };
        // Wake the accept loop blocked in ConnectNamedPipe. Between two
        // clients no instance listens and the open fails at once: tried
        // again until the loop has seen the stop, then its wait is cancelled.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !t.is_finished() && Instant::now() < deadline {
            let _ = OpenOptions::new().access_mode(CLIENT_ACCESS).open(&self.name);
            std::thread::sleep(Duration::from_millis(50));
        }
        while !t.is_finished() {
            // SAFETY: the accept thread's own handle, alive while `t` is.
            unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(t.as_raw_handle() as _) };
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = t.join();
    }
}

/// One connected client, as the accept loop and the watchdog see it.
struct Slot {
    thread: std::thread::JoinHandle<()>,
    state: Arc<ClientState>,
}

#[derive(Default)]
struct ClientState {
    /// Since when the client's thread waits on the pipe (to read a request
    /// or to write a reply); `None` while it works on a request.
    waiting: Mutex<Option<Instant>>,
    /// The client's account, known after its first request.
    user: Mutex<Option<Vec<u8>>>,
}

type Slots = Arc<Mutex<Vec<Slot>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clients still being served.
fn live(slots: &Slots) -> usize {
    let mut list = lock(slots);
    list.retain(|s| !s.thread.is_finished());
    list.len()
}

/// Lets go of clients that left the pipe idle for `idle`: their thread is
/// woken from its wait and ends. Runs until `stop`.
fn watchdog(slots: Slots, stop: Arc<AtomicBool>, idle: Duration) {
    use std::os::windows::io::AsRawHandle;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200).min(idle / 4));
        let mut list = lock(&slots);
        list.retain(|s| !s.thread.is_finished());
        for s in list.iter() {
            // held while cancelling: the thread cannot go on to other work meanwhile
            let waiting = lock(&s.state.waiting);
            if waiting.is_some_and(|since| since.elapsed() >= idle) {
                // SAFETY: the thread's own handle, alive while the JoinHandle is.
                unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(s.thread.as_raw_handle() as _) };
            }
        }
    }
}

fn accept_loop(name: &str, mut instance: OwnedHandle, handler: Handler, stop: Arc<AtomicBool>, idle: Duration) {
    let slots: Slots = Arc::new(Mutex::new(Vec::new()));
    {
        let (slots, stop) = (slots.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("pipe-watchdog".into()).spawn(move || watchdog(slots, stop, idle));
    }
    loop {
        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&instance);
        // SAFETY: valid pipe handle; synchronous call.
        let ok = unsafe { ConnectNamedPipe(raw as _, std::ptr::null_mut()) };
        let connected = ok != 0 || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if connected {
            // at most MAX_CLIENTS at once: the next one waits its turn (idle
            // ones are let go by the watchdog)
            while live(&slots) >= MAX_CLIENTS && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            // one account cannot take every connection: counted as it
            // connects (by the client process's account), before it sends
            // anything, so silent connections count too
            let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&instance);
            let me = client_process_user(raw);
            let theirs = me.as_ref().map_or(0, |me| {
                lock(&slots).iter().filter(|s| !s.thread.is_finished() && lock(&s.state.user).as_ref() == Some(me)).count()
            });
            if theirs >= MAX_PER_USER {
                // SAFETY: the connected instance; the handle is dropped below.
                unsafe { windows_sys::Win32::System::Pipes::DisconnectNamedPipe(raw as _) };
                drop(instance);
                instance = loop {
                    match create_instance(name, false) {
                        Ok(h) => break h,
                        Err(_) if stop.load(Ordering::SeqCst) => return,
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                };
                continue;
            }
            let state = Arc::new(ClientState { user: Mutex::new(me), ..ClientState::default() });
            let spawned = std::thread::Builder::new().name("pipe-client".into()).spawn({
                let (handler, state, slots) = (handler.clone(), state.clone(), slots.clone());
                move || serve_client(instance, handler, &state, &slots)
            });
            if let Ok(thread) = spawned {
                lock(&slots).push(Slot { thread, state });
            }
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

/// Runs `f` (a read or a write on the pipe) marked as waiting, so the
/// watchdog may cut it short when the client stays idle.
fn waiting<T>(state: &ClientState, f: impl FnOnce() -> T) -> T {
    *lock(&state.waiting) = Some(Instant::now());
    let out = f();
    // taken after the call: once this returns, no cancel meant for the
    // wait can reach the work that follows
    *lock(&state.waiting) = None;
    out
}

fn serve_client(handle: OwnedHandle, handler: Handler, state: &ClientState, slots: &Slots) {
    let mut pipe = File::from(handle);
    let mut first = true;
    loop {
        let req: Request = match waiting(state, || read_message_max(&mut pipe, MAX_REQUEST)) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            // ERROR_OPERATION_ABORTED: idle for too long, let go
            Err(e) if e.raw_os_error() == Some(995) => return,
            Err(e) => {
                let _ = waiting(state, || write_message(&mut pipe, &Response::Error(format!("bad request: {e}"))));
                return;
            }
        };
        let caller = PipeCaller(std::os::windows::io::AsRawHandle::as_raw_handle(&pipe));
        if first {
            // one account cannot take every connection
            first = false;
            let me = caller.user();
            *lock(&state.user) = me.clone();
            let others = lock(slots)
                .iter()
                .filter(|s| !s.thread.is_finished() && !std::ptr::eq(Arc::as_ptr(&s.state), state))
                .filter(|s| me.is_some() && *lock(&s.state.user) == me)
                .count();
            if others >= MAX_PER_USER {
                let busy = Response::Error("too many connections to the whereti service from this account".into());
                let _ = waiting(state, || write_message(&mut pipe, &busy));
                return;
            }
        }
        let resp = handler(req, &caller);
        if waiting(state, || write_message(&mut pipe, &resp)).is_err() {
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
    fn a_pipe_made_by_another_program_is_refused() {
        // From an administrator terminal (or CI) the other program would be
        // elevated too, and so accepted: nothing to show there.
        if is_elevated(std::process::id()) {
            return;
        }
        let name = format!("whereti-test-foreign-{}", std::process::id());
        let script = format!(
            "$p = New-Object System.IO.Pipes.NamedPipeServerStream('{name}'); $p.WaitForConnection(); Start-Sleep -Milliseconds 300"
        );
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()
            .expect("powershell");
        let full = format!(r"\\.\pipe\{name}");
        let start = Instant::now();
        let err = loop {
            match Client::connect_to(&full, Duration::from_secs(5)) {
                Err(e) if e.kind() == io::ErrorKind::NotFound && start.elapsed() < Duration::from_secs(20) => {
                    std::thread::sleep(Duration::from_millis(100)); // not created yet
                }
                Err(e) => break e,
                Ok(_) => panic!("a pipe served by another program was accepted"),
            }
        };
        let _ = child.wait();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[test]
    fn messages_round_trip() {
        let resp = Response::Search(SearchReply {
            total: 3,
            rows: vec![Row { path: r"C:\Users\ana\Mașină.docx".into(), is_dir: false, size: 10, modified: 5, flags: 0 }],
            warnings: vec![],
            micros: 1200,
            close: 0,
            groups: vec![],
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
    fn paths_are_checked_in_their_long_form() {
        assert_eq!(long_path(r"C:\Users\ana\a.txt"), r"\\?\C:\Users\ana\a.txt");
        assert_eq!(long_path("C:/Users/ana"), r"\\?\C:\Users\ana");
        assert_eq!(long_path(r"\\server\share\x"), r"\\?\UNC\server\share\x");
        assert_eq!(long_path(r"\\?\C:\x"), r"\\?\C:\x");
        // left for Windows to tidy up
        assert_eq!(long_path(r"C:\Users\ana\..\bob"), r"C:\Users\ana\..\bob");
        assert_eq!(long_path(r"C:\Users\\ana"), r"C:\Users\\ana");
        assert_eq!(long_path("relative"), "relative");
    }

    #[test]
    fn one_account_cannot_hold_every_connection() {
        let name = format!(r"\\.\pipe\whereti-test-cap-{}", std::process::id());
        let handler: Handler = Arc::new(|req, _| match req {
            Request::Hello { .. } => Response::Hello { version: PROTOCOL_VERSION, server: "test".into() },
            _ => Response::Ok,
        });
        let server = Server::start_with_idle(&name, handler, IDLE).unwrap();
        let held: Vec<Client> = (0..MAX_PER_USER).map(|_| Client::connect_to(&name, Duration::from_secs(5)).unwrap()).collect();
        assert!(Client::connect_to(&name, Duration::from_secs(2)).is_err(), "one more from the same account");
        drop(held);
        // let go: room again
        let mut ok = None;
        for _ in 0..50 {
            if let Ok(c) = Client::connect_to(&name, Duration::from_secs(2)) {
                ok = Some(c);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ok.is_some());
        drop(ok);
        server.stop();
    }

    #[test]
    fn a_long_path_is_seen_through_the_pipe_and_an_idle_client_is_let_go() {
        let name = format!(r"\\.\pipe\whereti-test-long-{}", std::process::id());
        let handler: Handler = Arc::new(|req, caller| match req {
            Request::Hello { .. } => Response::Hello { version: PROTOCOL_VERSION, server: "test".into() },
            Request::SetContentFolders { paths } => {
                Response::Error(format!("{:?} {:?}", caller.can_read(&paths[..1]), caller.can_list(&paths[1..])))
            }
            _ => Response::Ok,
        });
        let server = Server::start_with_idle(&name, handler, Duration::from_millis(600)).unwrap();
        // a folder and a file past 260 characters
        let mut dir = std::env::temp_dir().join(format!("whereti-long-{}", std::process::id()));
        let top = dir.clone();
        while dir.as_os_str().len() < 300 {
            dir.push("a-rather-long-folder-name-for-this-test");
        }
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let mut c = Client::connect_to(&name, Duration::from_secs(5)).unwrap();
        let paths = vec![file.display().to_string(), dir.display().to_string()];
        assert_eq!(c.call(&Request::SetContentFolders { paths }).unwrap(), Response::Error("[true] [true]".into()));
        // idle past the limit: the service lets go, a new connection works
        std::thread::sleep(Duration::from_millis(2000));
        assert!(c.call(&Request::Status).is_err(), "an idle client must be let go");
        let mut c = Client::connect_to(&name, Duration::from_secs(5)).unwrap();
        assert_eq!(c.call(&Request::Status).unwrap(), Response::Ok);
        drop(c);
        server.stop();
        let _ = std::fs::remove_dir_all(&top);
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

#[cfg(test)]
mod limits_tests {
    use super::*;

    #[test]
    fn a_request_larger_than_the_ceiling_is_refused() {
        let mut big = ((MAX_REQUEST + 1) as u32).to_le_bytes().to_vec();
        big.extend(std::iter::repeat_n(0u8, 16));
        let r: io::Result<Option<Request>> = read_message_max(&mut big.as_slice(), MAX_REQUEST);
        assert!(r.is_err());
    }

    #[test]
    fn only_admins_change_settings_by_default() {
        struct Plain;
        impl Caller for Plain {
            fn can_read(&self, p: &[String]) -> Vec<bool> {
                vec![true; p.len()]
            }
        }
        assert!(!Plain.is_admin());
        assert!(Trusted.is_admin());
    }
}

/// The account (user SID) of the process on the other end of a pipe,
/// known as soon as it connects. `None` when it cannot be read.
fn client_process_user(pipe: std::os::windows::io::RawHandle) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut pid = 0u32;
    // SAFETY: a connected pipe instance; an out-parameter.
    if unsafe { GetNamedPipeClientProcessId(pipe as _, &mut pid) } == 0 || pid == 0 {
        return None;
    }
    // SAFETY: handles opened here are closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0;
    unsafe { CloseHandle(process) };
    if !opened {
        return None;
    }
    let mut needed = 0u32;
    // SAFETY: size query, then a buffer of that size (8-aligned).
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
    let ok = unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) } != 0;
    let sid = ok.then(|| {
        // SAFETY: TOKEN_USER as Windows wrote it; its SID points into `buf`.
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        let len = unsafe { GetLengthSid(user.User.Sid) } as usize;
        unsafe { std::slice::from_raw_parts(user.User.Sid as *const u8, len) }.to_vec()
    });
    unsafe { CloseHandle(token) };
    sid
}
