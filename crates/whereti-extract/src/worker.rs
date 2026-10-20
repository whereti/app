//! Running `extract` in a separate, limited process (PLAN.md §6.3).
//!
//! Document parsers are big and handle untrusted bytes; a malformed PDF can
//! loop, eat memory or crash. So the caller reads the file itself and hands
//! the bytes to `whereti-extract.exe`, which:
//! - runs inside a Job Object: 512 MB memory cap, killed when the job closes;
//! - runs at below-normal priority with no window;
//! - is killed after a timeout.
//!
//! Wire format: bytes on stdin, `--ext <ext> --max <chars>` on the command
//! line. Exit code 0 → stdout is `T` or `F` (truncated) followed by UTF-8
//! text. Other codes map to `ExtractError`; stderr holds the detail.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{extract, ExtractError, Extracted};

const EXIT_UNSUPPORTED: i32 = 2;
const EXIT_PROTECTED: i32 = 3;
const EXIT_DAMAGED: i32 = 4;

/// Worker side: the whole `main` of whereti-extract.exe.
pub fn serve_stdio() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if let Some(model) = arg("--tags") {
        return serve_tags(Path::new(&model));
    }
    if let (Some(model), Some(tokenizer)) = (arg("--embed"), arg("--tokenizer")) {
        return serve_embed(Path::new(&model), Path::new(&tokenizer));
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

/// Worker side of `TagWorker`: photos in, tags out, until a zero length.
/// Request: u32 little-endian length, then the photo's bytes. Answer: one
/// line, `ok<TAB>label:share,label:share` or `err<TAB>message`. The model is
/// loaded on the first photo, once the process is inside its job.
fn serve_tags(model: &Path) -> i32 {
    let dll = std::env::current_exe().map(|e| e.with_file_name("onnxruntime.dll")).unwrap_or_default();
    let mut tagger: Option<crate::tags::Tagger> = None;
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
            if tagger.is_none() {
                tagger = Some(crate::tags::Tagger::new(&dll, model)?);
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
fn serve_embed(model: &Path, tokenizer: &Path) -> i32 {
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
                match crate::meaning::Embedder::new(&dll, model, tokenizer) {
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
}

impl Sandbox {
    /// The worker next to the running executable.
    pub fn beside_current_exe() -> Option<Sandbox> {
        let exe = std::env::current_exe().ok()?.with_file_name("whereti-extract.exe");
        exe.is_file().then(|| Sandbox::new(exe))
    }

    pub fn new(exe: PathBuf) -> Sandbox {
        Sandbox { exe, timeout: Duration::from_secs(10), memory_limit: 512 << 20, max_file: 100 << 20 }
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
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        let file = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2 | 0x4).open(path).map_err(SandboxError::Io)?;
        let worker = Sandbox { exe: self.exe.clone(), timeout: Duration::from_secs(60), memory_limit: self.memory_limit, max_file: self.max_file };
        let result = worker.run_with(&["--mailbox".to_string(), "--max".to_string(), max_chars.to_string()], Vec::new(), Some(file.as_raw_handle()));
        drop(file);
        result
    }

    pub fn extract_bytes(&self, ext: &str, bytes: Vec<u8>, max_chars: usize) -> Result<Extracted, SandboxError> {
        self.run(&["--ext".to_string(), ext.to_string(), "--max".to_string(), max_chars.to_string()], bytes)
    }

    /// Runs the worker with `args`, `bytes` on its stdin, inside the job.
    fn run(&self, args: &[String], bytes: Vec<u8>) -> Result<Extracted, SandboxError> {
        self.run_with(args, bytes, None)
    }

    /// `run`; `give`: a handle duplicated into the worker (and only into it)
    /// once it is in its job, its value sent first on stdin.
    fn run_with(&self, args: &[String], bytes: Vec<u8>, give: Option<std::os::windows::io::RawHandle>) -> Result<Extracted, SandboxError> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

        let job = Job::new(self.memory_limit).map_err(|e| SandboxError::Failed(format!("job object: {e}")))?;
        let mut child = Command::new(&self.exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .spawn()
            .map_err(|e| SandboxError::Failed(format!("cannot start the extractor: {e}")))?;
        // The worker blocks on stdin until we write, so it does no work
        // before it is inside the job.
        if let Err(e) = job.assign(child.as_raw_handle()) {
            let _ = child.kill();
            return Err(SandboxError::Failed(format!("job object: {e}")));
        }
        let mut bytes = bytes;
        if let Some(h) = give {
            use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut target = std::ptr::null_mut();
            // SAFETY: valid handles: ours, the child process we just started
            let ok = unsafe {
                DuplicateHandle(GetCurrentProcess(), h as _, child.as_raw_handle() as _, &mut target, 0, 0, DUPLICATE_SAME_ACCESS)
            };
            if ok == 0 {
                let _ = child.kill();
                return Err(SandboxError::Failed(format!("cannot give the file to the extractor: {}", std::io::Error::last_os_error())));
            }
            let mut first = (target as usize as u64).to_le_bytes().to_vec();
            first.extend_from_slice(&bytes);
            bytes = first;
        }

        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            out
        });
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut out = String::new();
            let _ = stderr.read_to_string(&mut out);
            out
        });

        let finished = wait(child.as_raw_handle(), self.timeout);
        if !finished {
            let _ = child.kill();
        }
        let status = child.wait().map_err(|e| SandboxError::Failed(e.to_string()))?;
        let _ = writer.join();
        let out = reader.join().unwrap_or_default();
        let err = err_reader.join().unwrap_or_default();
        if !finished {
            return Err(SandboxError::Failed(format!("took longer than {:.1} s", self.timeout.as_secs_f32())));
        }
        match status.code() {
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

/// Broker side of the photo tags: one worker process that keeps the model
/// loaded and takes photos one by one (loading 84 MB per photo would cost
/// more than tagging it). Same limits as `Sandbox`: a Job Object with the
/// memory cap, below-normal priority, no window; a photo that takes longer
/// than the timeout kills the worker, and the next photo starts a new one.
pub struct TagWorker {
    exe: PathBuf,
    /// The worker's mode: `--tags <model>`, `--embed <model> --tokenizer <file>`.
    args: Vec<std::ffi::OsString>,
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
        (exe.is_file() && dll.is_file() && model.is_file() && tokenizer.is_file()).then(|| {
            EmbedWorker(TagWorker {
                exe,
                args: vec!["--embed".into(), model.into(), "--tokenizer".into(), tokenizer.into()],
                memory_limit: 768 << 20,
                timeout: Duration::from_secs(20),
                running: None,
            })
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
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Drop for RunningTagger {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(&0u32.to_le_bytes());
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TagWorker {
    /// The worker next to the running executable, with `model`; `None` when
    /// the worker or ONNX Runtime is missing.
    pub fn beside_current_exe(model: &Path) -> Option<TagWorker> {
        let exe = std::env::current_exe().ok()?.with_file_name("whereti-extract.exe");
        let dll = exe.with_file_name("onnxruntime.dll");
        (exe.is_file() && dll.is_file() && model.is_file()).then(|| TagWorker {
            exe,
            args: vec!["--tags".into(), model.into()],
            memory_limit: 768 << 20,
            timeout: Duration::from_secs(20),
            running: None,
        })
    }

    /// A given worker executable (tests); onnxruntime.dll must be beside it.
    pub fn with_exe(exe: PathBuf, model: &Path) -> TagWorker {
        TagWorker { exe, args: vec!["--tags".into(), model.into()], memory_limit: 768 << 20, timeout: Duration::from_secs(20), running: None }
    }

    fn start(&self) -> Result<RunningTagger, String> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let job = Job::new(self.memory_limit).map_err(|e| format!("job object: {e}"))?;
        let mut child = Command::new(&self.exe)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .spawn()
            .map_err(|e| format!("cannot start the extractor: {e}"))?;
        // it waits for the first photo before loading anything
        if let Err(e) = job.assign(child.as_raw_handle()) {
            let _ = child.kill();
            return Err(format!("job object: {e}"));
        }
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
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

/// Reads a file without blocking its writers and without changing its
/// last-access time.
pub fn read_shared(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
        .map_err(SandboxError::Io)?;
    let len = file.metadata().map_err(SandboxError::Io)?.len();
    if len > max {
        return Err(SandboxError::TooLarge(len));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max).read_to_end(&mut bytes).map_err(SandboxError::Io)?;
    Ok(bytes)
}

fn wait(process: std::os::windows::io::RawHandle, timeout: Duration) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    unsafe { WaitForSingleObject(process as _, timeout.as_millis().min(u32::MAX as u128 - 1) as u32) == WAIT_OBJECT_0 }
}

/// A Job Object that caps memory and kills its processes when dropped.
struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread.
unsafe impl Send for Job {}

impl Job {
    fn new(memory_limit: usize) -> std::io::Result<Job> {
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

    fn assign(&self, process: std::os::windows::io::RawHandle) -> std::io::Result<()> {
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
