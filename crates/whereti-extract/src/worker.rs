//! Running `extract` in a separate, limited process (PLAN.md §6.3).
//!
//! Document parsers are big and handle untrusted bytes; a malformed PDF can
//! loop, eat memory or crash. So the caller reads the file itself and hands
//! the bytes to `whereti-extract.exe`, which:
//! - runs inside a Job Object: 512 MB memory cap, killed when the job closes;
//! - runs at below-normal priority with no window;
//! - is killed after a timeout.
//!
//! - starts with a restricted token (see `spawn`), so it cannot read what
//!   only its caller (SYSTEM, or one user) may, then gives up the rest of
//!   its rights itself (`lockdown`) before reading a byte.
//!
//! Wire format: bytes on stdin, `--ext <ext> --max <chars> --budget <ms>`
//! on the command line. Exit code 0 → stdout is `T` or `F` (truncated)
//! followed by UTF-8 text. Other codes map to `ExtractError`; stderr holds
//! the detail.

use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{extract, ExtractError, Extracted};

const EXIT_UNSUPPORTED: i32 = 2;
const EXIT_PROTECTED: i32 = 3;
const EXIT_DAMAGED: i32 = 4;
/// The worker could not give up its rights: it reads nothing.
const EXIT_LOCKDOWN: i32 = 5;

/// Gives up what the worker inherits from the SYSTEM service before it reads
/// a single byte of a document: every privilege (bar traversing folders),
/// and its integrity drops to Low. A document that took over a parser then
/// can write no file or registry key the user or the system uses, cannot
/// touch the service or any other process, and cannot use SYSTEM's
/// privileges (debug, impersonate, load drivers, take ownership...). The
/// installer also blocks the worker's network access in Windows Firewall.
fn lockdown() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LUID, ERROR_NOT_ALL_ASSIGNED, HANDLE};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, AllocateAndInitializeSid, FreeSid, GetTokenInformation, LookupPrivilegeValueW,
        SetTokenInformation, TokenIntegrityLevel, TokenPrivileges, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_PRIVILEGE_REMOVED,
        SID_AND_ATTRIBUTES, SID_IDENTIFIER_AUTHORITY, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::SystemServices::{SECURITY_MANDATORY_LOW_RID, SE_GROUP_INTEGRITY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let err = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: our own process token, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_ADJUST_DEFAULT | TOKEN_QUERY, &mut token) } == 0 {
        return Err(err("open token"));
    }
    struct Close(HANDLE);
    impl Drop for Close {
        fn drop(&mut self) {
            // SAFETY: a handle we opened.
            unsafe { CloseHandle(self.0) };
        }
    }
    let _close = Close(token);

    // 1. every privilege removed for good, except passing through folders
    // (kept enabled, as Windows has it: without it every folder on a path
    // would need its own right to pass through)
    let mut needed = 0u32;
    // SAFETY: size query, then a buffer of that size.
    unsafe { GetTokenInformation(token, TokenPrivileges, std::ptr::null_mut(), 0, &mut needed) };
    let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
    // SAFETY: the buffer is at least `needed` bytes and 8-aligned.
    if unsafe { GetTokenInformation(token, TokenPrivileges, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(err("read privileges"));
    }
    let mut keep = LUID { LowPart: 0, HighPart: 0 };
    let name: Vec<u16> = "SeChangeNotifyPrivilege".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name.
    unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut keep) };
    // SAFETY: TOKEN_PRIVILEGES as Windows wrote it: a count, then that many entries.
    let privs = unsafe { &mut *(buf.as_mut_ptr() as *mut TOKEN_PRIVILEGES) };
    let entries: &mut [LUID_AND_ATTRIBUTES] =
        // SAFETY: `PrivilegeCount` entries follow the count, inside `buf`.
        unsafe { std::slice::from_raw_parts_mut(privs.Privileges.as_mut_ptr(), privs.PrivilegeCount as usize) };
    let mut removed = 0u32;
    for e in entries.iter_mut() {
        if e.Luid.LowPart == keep.LowPart && e.Luid.HighPart == keep.HighPart {
            e.Attributes = SE_PRIVILEGE_ENABLED;
        } else {
            e.Attributes = SE_PRIVILEGE_REMOVED;
            removed += 1;
        }
    }
    if removed > 0 {
        // SAFETY: the list Windows gave us, each entry marked.
        let ok = unsafe { AdjustTokenPrivileges(token, 0, privs, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
        // SAFETY: plain read of the thread's last error.
        if ok == 0 || unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(err("remove privileges"));
        }
    }

    // 2. integrity down to Low (a process may always lower its own)
    let authority = SID_IDENTIFIER_AUTHORITY { Value: [0, 0, 0, 0, 0, 16] };
    let mut sid = std::ptr::null_mut();
    // SAFETY: the mandatory label authority with one sub-authority; freed below.
    if unsafe { AllocateAndInitializeSid(&authority, 1, SECURITY_MANDATORY_LOW_RID as u32, 0, 0, 0, 0, 0, 0, 0, &mut sid) } == 0 {
        return Err(err("low integrity SID"));
    }
    let label = TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: sid, Attributes: SE_GROUP_INTEGRITY as u32 } };
    let size = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + 12;
    // SAFETY: a valid label for our own token; the SID lives until FreeSid.
    let ok = unsafe { SetTokenInformation(token, TokenIntegrityLevel, (&label as *const TOKEN_MANDATORY_LABEL).cast(), size) };
    // SAFETY: the SID we allocated.
    unsafe { FreeSid(sid) };
    if ok == 0 {
        return Err(err("lower integrity"));
    }
    Ok(())
}

/// Worker side: the whole `main` of whereti-extract.exe.
pub fn serve_stdio() -> i32 {
    // before reading anything (the service writes stdin only once this
    // process is inside its job; parsing starts after this)
    if let Err(e) = lockdown() {
        eprintln!("whereti-extract: {e}");
        return EXIT_LOCKDOWN;
    }
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    // for the tests: after the lockdown, writing a file must be refused
    if let Some(probe) = arg("--probe-lockdown") {
        return match std::fs::write(&probe, b"x") {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 0,
            _ => 1,
        };
    }
    // for the tests: with the restricted token, a file only its user may
    // read must be refused too
    if let Some(probe) = arg("--probe-read") {
        return match std::fs::read(&probe) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => 0,
            _ => 1,
        };
    }
    // what Windows' OCR, picture and PDF code can do from here
    if args.iter().any(|a| a == "--probe-sandbox") {
        let _ = std::io::stdout().lock().write_all(crate::ocr::probe().as_bytes());
        return 0;
    }
    // the caller's time: slow work stops in time to hand back what it has
    if let Some(ms) = arg("--budget").and_then(|m| m.parse::<u64>().ok()) {
        crate::set_deadline(Some(std::time::Instant::now() + Duration::from_millis(ms)));
    }
    if let Some(langs) = arg("--ocr-lang") {
        crate::set_ocr_languages(&langs.split(',').map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>());
    }
    let given = args.iter().any(|a| a == "--given");
    if let Some(model) = arg("--tags") {
        return serve_tags(Model::new(&model, given));
    }
    if let (Some(model), Some(tokenizer)) = (arg("--embed"), arg("--tokenizer")) {
        let model = Model::new(&model, given);
        let tokenizer = Model::new(&tokenizer, given);
        return serve_embed(model, tokenizer);
    }
    if args.iter().any(|a| a == "--mailbox") {
        let max = arg("--max").and_then(|m| m.parse().ok()).unwrap_or(1_000_000);
        // the file's handle comes first on stdin (put into this process by
        // the service once it is inside its job)
        let mut h = [0u8; 8];
        if std::io::stdin().lock().read_exact(&mut h).is_err() {
            return EXIT_DAMAGED;
        }
        return serve_mailbox(u64::from_le_bytes(h) as usize, max);
    }
    let ext = arg("--ext").unwrap_or_default();
    let max = arg("--max").and_then(|m| m.parse().ok()).unwrap_or(1_000_000);

    let mut bytes = Vec::new();
    if std::io::stdin().lock().read_to_end(&mut bytes).is_err() {
        return EXIT_DAMAGED;
    }
    let result = std::panic::catch_unwind(|| extract(&ext, &bytes, max))
        .unwrap_or_else(|_| Err(ExtractError::Damaged("the parser failed".into())));
    match result {
        Ok(e) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(if e.truncated { b"T" } else { b"F" });
            let _ = out.write_all(e.text.as_bytes());
            let _ = out.flush();
            0
        }
        Err(ExtractError::Unsupported) => EXIT_UNSUPPORTED,
        Err(ExtractError::Protected) => EXIT_PROTECTED,
        Err(ExtractError::Damaged(msg)) => {
            eprint!("{msg}");
            EXIT_DAMAGED
        }
    }
}

/// A model file for the worker: by its path, or (`--given`) as a handle
/// the caller opened and put into this process, its value first on stdin
/// (the restricted worker cannot open ProgramData\whereti itself).
enum Model {
    Path(PathBuf),
    Handle(usize),
}

impl Model {
    fn new(path: &str, given: bool) -> Model {
        if !given {
            return Model::Path(PathBuf::from(path));
        }
        let mut h = [0u8; 8];
        match std::io::stdin().lock().read_exact(&mut h) {
            Ok(()) => Model::Handle(u64::from_le_bytes(h) as usize),
            Err(_) => Model::Handle(0),
        }
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        use std::io::Seek;
        use std::os::windows::io::FromRawHandle;
        match self {
            Model::Path(p) => std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display())),
            Model::Handle(0) => Err("no model was given".into()),
            Model::Handle(h) => {
                // SAFETY: the caller put a file handle opened for reading into
                // this process; it stays open (ManuallyDrop) for another read.
                let mut file = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(*h as std::os::windows::io::RawHandle) });
                let mut buf = Vec::new();
                file.seek(std::io::SeekFrom::Start(0)).and_then(|_| file.read_to_end(&mut buf)).map_err(|e| format!("cannot read the model: {e}"))?;
                Ok(buf)
            }
        }
    }
}

/// Worker side of `TagWorker`: photos in, tags out, until a zero length.
/// Request: u32 little-endian length, then the photo's bytes. Answer: one
/// line, `ok<TAB>label:share,label:share` or `err<TAB>message`. The model is
/// loaded on the first photo, once the process is inside its job.
fn serve_tags(model: Model) -> i32 {
    let dll = std::env::current_exe().map(|e| e.with_file_name("onnxruntime.dll")).unwrap_or_default();
    let mut tagger: Option<crate::tags::Tagger> = None;
    // a model that cannot load is not loaded again for every photo
    let mut failed: Option<String> = None;
    let (mut input, mut out) = (std::io::stdin().lock(), std::io::stdout().lock());
    loop {
        let mut len = [0u8; 4];
        if input.read_exact(&mut len).is_err() {
            return 0;
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 {
            return 0;
        }
        let mut bytes = vec![0u8; len];
        if input.read_exact(&mut bytes).is_err() {
            return EXIT_DAMAGED;
        }
        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(e) = &failed {
                return Err(e.clone());
            }
            if tagger.is_none() {
                match model.bytes().and_then(|m| crate::tags::Tagger::from_bytes(&dll, &m)) {
                    Ok(t) => tagger = Some(t),
                    Err(e) => {
                        failed = Some(e.clone());
                        return Err(e);
                    }
                }
            }
            tagger.as_mut().map_or(Err("no model".to_string()), |t| t.tags(&bytes))
        }))
        .unwrap_or_else(|_| Err("the model failed".into()));
        let line = match answer {
            Ok(tags) => format!("ok\t{}\n", tags.iter().map(|(i, s)| format!("{i}:{s:.3}")).collect::<Vec<_>>().join(",")),
            Err(e) => format!("err\t{}\n", e.replace(['\n', '\r', '\t'], " ")),
        };
        if out.write_all(line.as_bytes()).and_then(|_| out.flush()).is_err() {
            return 0;
        }
    }
}

/// Worker side of `Sandbox::extract_file` for a mailbox: the file comes
/// as a handle the service opened read-only and let this process inherit.
fn serve_mailbox(handle: usize, max: usize) -> i32 {
    use std::os::windows::io::FromRawHandle;
    // SAFETY: the service duplicated a valid file handle, opened for reading, into this process
    let file = unsafe { std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = crate::Out::new(max);
        crate::mailbox::mailbox(|| file.try_clone(), &mut out).map(|_| out.finish())
    }))
    .unwrap_or_else(|_| Err(ExtractError::Damaged("the mailbox reader failed".into())));
    match result {
        Ok(e) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(if e.truncated { b"T" } else { b"F" });
            let _ = out.write_all(e.text.as_bytes());
            let _ = out.flush();
            0
        }
        Err(ExtractError::Unsupported) => EXIT_UNSUPPORTED,
        Err(ExtractError::Protected) => EXIT_PROTECTED,
        Err(ExtractError::Damaged(msg)) => {
            eprint!("{msg}");
            EXIT_DAMAGED
        }
    }
}

/// Worker side of `EmbedWorker`: texts in, vectors out, until a zero
/// length. Request: u32 little-endian length, then UTF-8 text. Answer: one
/// line, `ok<TAB>` and the 384 signed bytes in hex, or `err<TAB>message`.
fn serve_embed(model: Model, tokenizer: Model) -> i32 {
    let dll = std::env::current_exe().map(|e| e.with_file_name("onnxruntime.dll")).unwrap_or_default();
    let mut embedder: Option<crate::meaning::Embedder> = None;
    // a model that cannot load is not loaded again for every text
    let mut failed: Option<String> = None;
    let (mut input, mut out) = (std::io::stdin().lock(), std::io::stdout().lock());
    loop {
        let mut len = [0u8; 4];
        if input.read_exact(&mut len).is_err() {
            return 0;
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 {
            return 0;
        }
        let mut bytes = vec![0u8; len];
        if input.read_exact(&mut bytes).is_err() {
            return EXIT_DAMAGED;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(e) = &failed {
                return Err(e.clone());
            }
            if embedder.is_none() {
                let loaded = model
                    .bytes()
                    .and_then(|m| Ok((m, tokenizer.bytes()?)))
                    .and_then(|(m, t)| crate::meaning::Embedder::from_bytes(&dll, &m, &t));
                match loaded {
                    Ok(e) => embedder = Some(e),
                    Err(e) => {
                        failed = Some(e.clone());
                        return Err(e);
                    }
                }
            }
            embedder.as_mut().map_or(Err("no model".to_string()), |e| e.embed(&text))
        }))
        .unwrap_or_else(|_| Err("the model failed".into()));
        let line = match answer {
            Ok(v) => format!("ok\t{}\n", crate::meaning::quantize(&v).iter().map(|b| format!("{:02x}", *b as u8)).collect::<String>()),
            Err(e) => format!("err\t{}\n", e.replace(['\n', '\r', '\t'], " ")),
        };
        if out.write_all(line.as_bytes()).and_then(|_| out.flush()).is_err() {
            return 0;
        }
    }
}

#[derive(Debug)]
pub enum SandboxError {
    Extract(ExtractError),
    /// The file is larger than the caller allows.
    TooLarge(u64),
    /// Could not read the file (locked, gone, no access).
    Io(std::io::Error),
    /// The worker ran out of time or memory, or crashed.
    Failed(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Extract(e) => e.fmt(f),
            SandboxError::TooLarge(n) => write!(f, "file too large ({} MB)", n >> 20),
            SandboxError::Io(e) => write!(f, "cannot read the file: {e}"),
            SandboxError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Broker side: runs whereti-extract.exe with limits.
pub struct Sandbox {
    exe: PathBuf,
    pub timeout: Duration,
    pub memory_limit: usize,
    pub max_file: u64,
    /// OCR languages to try first (BCP-47, "ro-RO"), e.g. those of the user
    /// who asked; else the system's, else any Windows has.
    pub ocr_languages: Vec<String>,
}

impl Sandbox {
    /// The worker next to the running executable.
    pub fn beside_current_exe() -> Option<Sandbox> {
        let exe = std::env::current_exe().ok()?.with_file_name("whereti-extract.exe");
        exe.is_file().then(|| Sandbox::new(exe))
    }

    pub fn new(exe: PathBuf) -> Sandbox {
        Sandbox { exe, timeout: Duration::from_secs(10), memory_limit: 512 << 20, max_file: 100 << 20, ocr_languages: Vec::new() }
    }

    /// The worker runs with the restricted token (see `spawn`). `false`
    /// only where Windows' OCR, picture or PDF code failed with it and
    /// works without it: then the worker has its own lockdown alone.
    pub fn restricted(&self) -> bool {
        restricted_works(&self.exe)
    }

    /// One line for the log: whether the worker has the restricted token,
    /// and what Windows' OCR, picture and PDF code can do in it.
    pub fn sandbox_report(&self) -> String {
        sandbox_check(&self.exe).1
    }

    /// Reads `path` (sharing it with writers, so open files work) and
    /// extracts its text in the worker.
    pub fn extract_file(&self, path: &Path, max_chars: usize) -> Result<Extracted, SandboxError> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
        match crate::kind_of(&ext) {
            None => return Err(SandboxError::Extract(ExtractError::Unsupported)),
            // a mailbox can be gigabytes: the worker reads the open file itself
            Some(crate::Kind::Mailbox) => return self.extract_mailbox(path, max_chars),
            _ => {}
        }
        let bytes = read_shared(path, self.max_file)?;
        self.extract_bytes(&ext, bytes, max_chars)
    }

    /// A mailbox: opened here read-only, sharing it with any writer (while
    /// Outlook has it open, Windows refuses the open: it is tried again
    /// later). Its handle is duplicated into the worker alone, once the
    /// worker is in its job; at most 60 s, so Outlook started meanwhile
    /// waits little.
    fn extract_mailbox(&self, path: &Path, max_chars: usize) -> Result<Extracted, SandboxError> {
        use std::os::windows::io::AsRawHandle;
        let file = open_real(path).map_err(SandboxError::Io)?;
        let worker = Sandbox { exe: self.exe.clone(), timeout: Duration::from_secs(60), memory_limit: self.memory_limit, max_file: self.max_file, ocr_languages: Vec::new() };
        let result = worker.run_with(&["--mailbox".to_string(), "--max".to_string(), max_chars.to_string()], Vec::new(), Some(file.as_raw_handle()));
        drop(file);
        result
    }

    pub fn extract_bytes(&self, ext: &str, bytes: Vec<u8>, max_chars: usize) -> Result<Extracted, SandboxError> {
        let mut args = vec!["--ext".to_string(), ext.to_string(), "--max".to_string(), max_chars.to_string()];
        if !self.ocr_languages.is_empty() {
            args.extend(["--ocr-lang".to_string(), self.ocr_languages.join(",")]);
        }
        self.run(&args, bytes)
    }

    /// Runs the worker with `args`, `bytes` on its stdin, inside the job.
    fn run(&self, args: &[String], bytes: Vec<u8>) -> Result<Extracted, SandboxError> {
        self.run_with(args, bytes, None)
    }

    /// For the tests: the worker's exit code with `args` (a `--probe-...`).
    #[doc(hidden)]
    pub fn probe_exit(&self, args: &[&str]) -> Option<i32> {
        let job = Job::new(self.memory_limit).ok()?;
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let (mut child, _) = crate::spawn::spawn(&self.exe, &args, self.restricted(), false, &job, &[]).ok()?;
        drop(child.stdin.take());
        if !child.wait_for(self.timeout) {
            child.kill();
            return None;
        }
        child.exit_code()
    }

    /// `run`; `give`: a handle duplicated into the worker (and only into it)
    /// once it is in its job, its value sent first on stdin.
    fn run_with(&self, args: &[String], bytes: Vec<u8>, give: Option<std::os::windows::io::RawHandle>) -> Result<Extracted, SandboxError> {
        // The worker stops its slow work (OCR, the documents in an archive)
        // a quarter before the timeout, so what it read gets back in time.
        let budget = (self.timeout.as_millis() as u64 * 3 / 4).to_string();
        let mut args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        args.extend([std::ffi::OsStr::new("--budget"), std::ffi::OsStr::new(&budget)]);

        let job = Job::new(self.memory_limit).map_err(|e| SandboxError::Failed(format!("job object: {e}")))?;
        let give: Vec<_> = give.into_iter().collect();
        let (mut child, given) = crate::spawn::spawn(&self.exe, &args, self.restricted(), true, &job, &give)
            .map_err(|e| SandboxError::Failed(format!("cannot start the extractor: {e}")))?;
        // the handle's value in the worker comes first
        let mut bytes = bytes;
        if let Some(h) = given.first() {
            let mut first = h.to_le_bytes().to_vec();
            first.extend_from_slice(&bytes);
            bytes = first;
        }

        let mut stdin = child.stdin.take().expect("piped");
        let writer = PipeThread::start(move || {
            let _ = stdin.write_all(&bytes);
        });
        let mut stdout = child.stdout.take().expect("piped");
        let reader = PipeThread::start(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            out
        });
        let mut stderr = child.stderr.take().expect("piped");
        let err_reader = PipeThread::start(move || {
            let mut out = String::new();
            let _ = stderr.read_to_string(&mut out);
            out
        });

        let finished = child.wait_for(self.timeout);
        if !finished {
            child.kill();
        }
        let code = child.exit_code();
        // its pipes close with it (the job ends anything it could have left)
        drop(job);
        writer.finish();
        let out = reader.finish();
        let err = err_reader.finish();
        if !finished {
            return Err(SandboxError::Failed(format!("took longer than {:.1} s", self.timeout.as_secs_f32())));
        }
        match code {
            Some(0) if !out.is_empty() => Ok(Extracted {
                truncated: out[0] == b'T',
                text: String::from_utf8_lossy(&out[1..]).into_owned(),
            }),
            Some(EXIT_UNSUPPORTED) => Err(SandboxError::Extract(ExtractError::Unsupported)),
            Some(EXIT_PROTECTED) => Err(SandboxError::Extract(ExtractError::Protected)),
            Some(EXIT_DAMAGED) => Err(SandboxError::Extract(ExtractError::Damaged(err))),
            code => Err(SandboxError::Failed(format!(
                "the extractor stopped (code {}){}",
                code.map_or("?".into(), |c| format!("{c:#x}")),
                if out.is_empty() && err.contains("memory") { " - out of memory" } else { "" }
            ))),
        }
    }
}

/// For the tests: restricted workers get a window station and desktop of
/// their own, as in the service, even when run as a user.
#[doc(hidden)]
pub fn use_own_desktop() {
    crate::spawn::OWN_DESKTOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the worker `exe` does its work with the restricted token, found
/// once per worker: it is started with it and without it, and asked what
/// Windows' OCR, picture and PDF code can do (`--probe-sandbox`). Without
/// the restricted token only if it can do less with it.
fn restricted_works(exe: &Path) -> bool {
    sandbox_check(exe).0
}

/// `restricted_works`, and what was found, for the log.
fn sandbox_check(exe: &Path) -> (bool, String) {
    static KNOWN: std::sync::Mutex<Vec<(PathBuf, bool, String)>> = std::sync::Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, ok, report)) = known.iter().find(|(p, ..)| p == exe) {
        return (*ok, report.clone());
    }
    // what the worker answers, or why it did not
    let probe = |restricted: bool| -> Result<String, String> {
        let job = Job::new(512 << 20).map_err(|e| format!("job object: {e}"))?;
        let (mut child, _) =
            crate::spawn::spawn(exe, &[std::ffi::OsStr::new("--probe-sandbox")], restricted, false, &job, &[]).map_err(|e| e.to_string())?;
        drop(child.stdin.take());
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = PipeThread::start(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            out
        });
        let finished = child.wait_for(Duration::from_secs(20));
        if !finished {
            child.kill();
        }
        let code = child.exit_code();
        drop(job);
        let out = reader.finish();
        match (finished, code) {
            (true, Some(0)) => Ok(out),
            (false, _) => Err("no answer in 20 s".into()),
            (_, code) => Err(format!("stopped with code {:#x}", code.unwrap_or(-1))),
        }
    };
    let can = |answer: &str| -> Vec<String> { answer.split_whitespace().filter(|w| w.ends_with("=1")).map(String::from).collect() };
    let (ok, report) = match probe(true) {
        Err(e) => (false, format!("restricted token: no ({e})")),
        Ok(with) if can(&with).len() >= 3 => (true, format!("restricted token: yes ({with})")),
        Ok(with) => match probe(false) {
            Ok(without) if !can(&without).iter().all(|w| can(&with).contains(w)) => {
                (false, format!("restricted token: no ({with}; without it: {without})"))
            }
            _ => (true, format!("restricted token: yes ({with})")),
        },
    };
    known.push((exe.to_path_buf(), ok, report.clone()));
    (ok, report)
}

/// A thread reading or writing one of a worker's pipes. Once the worker
/// has ended, its pipe ends close with it, but a process another thread
/// started meanwhile may hold copies for as long as it runs (see `spawn`):
/// then the read or write is cancelled after a short wait, and what was
/// read is kept.
struct PipeThread<T> {
    thread: std::thread::JoinHandle<()>,
    result: std::sync::mpsc::Receiver<T>,
}

impl<T: Send + Default + 'static> PipeThread<T> {
    fn start(f: impl FnOnce() -> T + Send + 'static) -> PipeThread<T> {
        let (tx, result) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        PipeThread { thread, result }
    }

    /// Its result, once the worker has ended.
    fn finish(self) -> T {
        use std::os::windows::io::AsRawHandle;
        use std::sync::mpsc::RecvTimeoutError;
        let mut wait = Duration::from_secs(2);
        loop {
            match self.result.recv_timeout(wait) {
                Ok(v) => return v,
                Err(RecvTimeoutError::Disconnected) => return T::default(),
                Err(RecvTimeoutError::Timeout) => {
                    // SAFETY: the handle of a thread we started, still held.
                    unsafe { windows_sys::Win32::System::IO::CancelSynchronousIo(self.thread.as_raw_handle() as _) };
                    wait = Duration::from_millis(50);
                }
            }
        }
    }
}

// Broker side of the photo tags: one worker process that keeps the model
/// loaded and takes photos one by one (loading 84 MB per photo would cost
/// more than tagging it). Same limits as `Sandbox`: a Job Object with the
/// memory cap, below-normal priority, no window; a photo that takes longer
/// than the timeout kills the worker, and the next photo starts a new one.
pub struct TagWorker {
    exe: PathBuf,
    /// The worker's mode: `--tags <model>`, `--embed <model> --tokenizer <file>`.
    args: Vec<std::ffi::OsString>,
    /// The model files, opened here and handed to the worker in that order
    /// (`--given`): with its restricted token it cannot open them itself.
    files: Vec<PathBuf>,
    memory_limit: usize,
    pub timeout: Duration,
    running: Option<RunningTagger>,
}

/// The text model's worker (search by meaning): the same process kind as
/// `TagWorker`, in its `--embed` mode.
pub struct EmbedWorker(TagWorker);

impl EmbedWorker {
    /// The worker next to the running executable, with the model and its
    /// tokenizer; `None` when something is missing.
    pub fn beside_current_exe(model: &Path, tokenizer: &Path) -> Option<EmbedWorker> {
        let exe = std::env::current_exe().ok()?.with_file_name("whereti-extract.exe");
        let dll = exe.with_file_name("onnxruntime.dll");
        (exe.is_file() && dll.is_file() && model.is_file() && tokenizer.is_file()).then(|| EmbedWorker::with_exe(exe, model, tokenizer))
    }

    /// A given worker executable (tests); onnxruntime.dll must be beside it.
    pub fn with_exe(exe: PathBuf, model: &Path, tokenizer: &Path) -> EmbedWorker {
        EmbedWorker(TagWorker {
            exe,
            args: vec!["--embed".into(), model.into(), "--tokenizer".into(), tokenizer.into(), "--given".into()],
            files: vec![model.to_path_buf(), tokenizer.to_path_buf()],
            memory_limit: 768 << 20,
            timeout: Duration::from_secs(20),
            running: None,
        })
    }

    /// The vector of one text ("query: …" or "passage: …"), 384 signed bytes.
    pub fn embed(&mut self, text: &str) -> Result<Vec<i8>, String> {
        let hex = self.0.ask(text.as_bytes())?;
        let v: Vec<i8> = (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok().map(|b| b as i8))
            .collect();
        if v.len() == crate::meaning::DIMS {
            Ok(v)
        } else {
            Err("bad vector".into())
        }
    }
}

struct RunningTagger {
    _job: Job,
    child: crate::spawn::Child,
    stdin: std::fs::File,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Drop for RunningTagger {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(&0u32.to_le_bytes());
        let _ = self.stdin.flush();
        self.child.kill();
        let _ = self.child.wait_for(Duration::from_secs(5));
    }
}

impl TagWorker {
    /// The worker next to the running executable, with `model`; `None` when
    /// the worker or ONNX Runtime is missing.
    pub fn beside_current_exe(model: &Path) -> Option<TagWorker> {
        let exe = std::env::current_exe().ok()?.with_file_name("whereti-extract.exe");
        let dll = exe.with_file_name("onnxruntime.dll");
        (exe.is_file() && dll.is_file() && model.is_file()).then(|| TagWorker::with_exe(exe, model))
    }

    /// A given worker executable (tests); onnxruntime.dll must be beside it.
    pub fn with_exe(exe: PathBuf, model: &Path) -> TagWorker {
        TagWorker {
            exe,
            args: vec!["--tags".into(), model.into(), "--given".into()],
            files: vec![model.to_path_buf()],
            memory_limit: 768 << 20,
            timeout: Duration::from_secs(20),
            running: None,
        }
    }

    fn start(&self) -> Result<RunningTagger, String> {
        use std::os::windows::io::AsRawHandle;
        let job = Job::new(self.memory_limit).map_err(|e| format!("job object: {e}"))?;
        let files: Vec<std::fs::File> =
            self.files.iter().map(|f| std::fs::File::open(f).map_err(|e| format!("cannot open {}: {e}", f.display()))).collect::<Result<_, _>>()?;
        let handles: Vec<_> = files.iter().map(|f| f.as_raw_handle()).collect();
        let args: Vec<&std::ffi::OsStr> = self.args.iter().map(|a| a.as_os_str()).collect();
        let (mut child, given) = crate::spawn::spawn(&self.exe, &args, restricted_works(&self.exe), false, &job, &handles)
            .map_err(|e| format!("cannot start the extractor: {e}"))?;
        drop(files);
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        // the model files' handles first; it waits for the first request
        // before loading anything
        let first: Vec<u8> = given.iter().flat_map(|h| h.to_le_bytes()).collect();
        if let Err(e) = stdin.write_all(&first) {
            child.kill();
            return Err(format!("the tagger stopped: {e}"));
        }
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Ok(RunningTagger { _job: job, child, stdin, lines })
    }

    /// The tags of one photo: `(label, share)`, best first.
    pub fn tags(&mut self, bytes: &[u8]) -> Result<Vec<(usize, f32)>, String> {
        let list = self.ask(bytes)?;
        Ok(list
            .split(',')
            .filter_map(|t| {
                let (i, s) = t.split_once(':')?;
                Some((i.parse().ok()?, s.parse().ok()?))
            })
            .collect())
    }

    /// One request; the answer after `ok<TAB>`.
    fn ask(&mut self, bytes: &[u8]) -> Result<String, String> {
        if self.running.is_none() {
            self.running = Some(self.start()?);
        }
        let run = self.running.as_mut().expect("started");
        let sent = run.stdin.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| run.stdin.write_all(bytes)).and_then(|_| run.stdin.flush());
        if let Err(e) = sent {
            self.running = None;
            return Err(format!("the tagger stopped: {e}"));
        }
        let line = match run.lines.recv_timeout(self.timeout) {
            Ok(line) => line,
            Err(_) => {
                // too slow, out of memory or crashed: a new one next time
                self.running = None;
                return Err(format!("no answer in {} s", self.timeout.as_secs()));
            }
        };
        match line.split_once('\t') {
            Some(("ok", answer)) => Ok(answer.to_string()),
            Some(("err", e)) => Err(e.to_string()),
            _ => Err(format!("unexpected answer: {line}")),
        }
    }
}

/// Opens `path` to read, sharing it with its writers, only if the file
/// opened is the one at `path` itself. The service reads as SYSTEM files a
/// user asked about (and checked to be readable by that user); a folder on
/// the way swapped for a junction would otherwise send it to someone
/// else's file. Read from the handle this returns, never by path again.
pub fn open_real(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)?;
    let real = final_path(&file)?;
    if !same_path(&real, path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} leads to {} (a link on the way): not read", path.display(), real.display()),
        ));
    }
    Ok(file)
}

/// Where an open file really is, as a DOS path.
fn final_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS};
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: an open handle and a buffer of the given length.
        let n = unsafe {
            GetFinalPathNameByHandleW(file.as_raw_handle() as _, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS)
        } as usize;
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n < buf.len() {
            let s = String::from_utf16_lossy(&buf[..n]);
            let s = match s.strip_prefix(r"\\?\UNC\") {
                Some(rest) => format!(r"\\{rest}"),
                None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_string(),
            };
            return Ok(PathBuf::from(s));
        }
        buf.resize(n + 1, 0);
    }
}

/// The same path, letter case, slashes and short 8.3 names aside (`real` is
/// what Windows reports for an open file; `asked` may use short names, a
/// `\\?\` prefix, or a SUBST or network drive letter).
fn same_path(real: &Path, asked: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let asked = dos_target(asked);
    norm(real) == norm(&asked) || norm(real) == norm(&long_path(&asked))
}

/// `path` as Windows names an open file: no `\\?\` prefix, and a drive
/// letter made by SUBST or for a network share replaced by the folder or
/// share it stands for. Drive letters are the system's (or, in the app,
/// the user's own), which no other user can change: no race here.
fn dos_target(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let s = match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_string(),
    };
    let b = s.as_bytes();
    if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
        return PathBuf::from(s);
    }
    let drive = s[..2].to_ascii_uppercase();
    let Some(target) = dos_device(&drive) else { return PathBuf::from(s) };
    let mapped = if let Some(dir) = target.strip_prefix(r"\??\") {
        // SUBST: \??\C:\some\folder
        Some(dir.trim_end_matches('\\').to_string())
    } else {
        // a network drive: \Device\LanmanRedirector\;Z:0000000000012345\server\share
        let upper = target.to_ascii_uppercase();
        upper.find(&format!(";{drive}")).and_then(|at| target[at + 1..].find('\\').map(|p| format!(r"\{}", &target[at + 1 + p..])))
    };
    match mapped {
        Some(m) => PathBuf::from(format!("{m}{}", &s[2..])),
        None => PathBuf::from(s),
    }
}

/// What a DOS device name (`X:`) stands for.
fn dos_device(name: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated name and a buffer of the given length.
    let n = unsafe { QueryDosDeviceW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 {
        return None;
    }
    // a list of NUL-terminated names: the first is the current one
    let first = buf[..n].split(|&c| c == 0).next()?;
    Some(String::from_utf16_lossy(first))
}

/// `path` with its short 8.3 names spelled out (`RUNNER~1` -> `runneradmin`).
/// It names each folder as it is, links included, never where they lead.
fn long_path(path: &Path) -> PathBuf {
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: a NUL-terminated path and a buffer of the given length.
        let n = unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return path.to_path_buf();
        }
        if n < buf.len() {
            return PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n + 1, 0);
    }
}

/// Reads a file without blocking its writers and without changing its
/// last-access time (only at its real path, see `open_real`).
pub fn read_shared(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    let file = open_real(path).map_err(SandboxError::Io)?;
    let len = file.metadata().map_err(SandboxError::Io)?.len();
    if len > max {
        return Err(SandboxError::TooLarge(len));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max).read_to_end(&mut bytes).map_err(SandboxError::Io)?;
    Ok(bytes)
}

/// A Job Object that caps memory and kills its processes when dropped.
pub(crate) struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Job {}

impl Job {
    pub(crate) fn new(memory_limit: usize) -> std::io::Result<Job> {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Job(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = 1;
            info.ProcessMemoryLimit = memory_limit;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    pub(crate) fn assign(&self, process: std::os::windows::io::RawHandle) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        if unsafe { AssignProcessToJobObject(self.0, process as _) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_pipe_held_elsewhere_is_not_waited_for() {
        use std::os::windows::io::FromRawHandle;
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: two out-pointers, no attributes.
        assert_ne!(unsafe { windows_sys::Win32::System::Pipes::CreatePipe(&mut r, &mut w, std::ptr::null(), 0) }, 0);
        // SAFETY: fresh handles, owned from here.
        let (mut read, mut write) = unsafe { (std::fs::File::from_raw_handle(r as _), std::fs::File::from_raw_handle(w as _)) };
        write.write_all(b"partial").unwrap();
        // the write end stays open, as in a process that inherited it
        let t = Instant::now();
        let reader = PipeThread::start(move || {
            let mut out = Vec::new();
            let _ = read.read_to_end(&mut out);
            out
        });
        assert_eq!(reader.finish(), b"partial");
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        drop(write);
    }
}
