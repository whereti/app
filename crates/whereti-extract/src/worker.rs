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
        if crate::kind_of(&ext).is_none() {
            return Err(SandboxError::Extract(ExtractError::Unsupported));
        }
        let bytes = read_shared(path, self.max_file)?;
        self.extract_bytes(&ext, bytes, max_chars)
    }

    pub fn extract_bytes(&self, ext: &str, bytes: Vec<u8>, max_chars: usize) -> Result<Extracted, SandboxError> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

        let job = Job::new(self.memory_limit).map_err(|e| SandboxError::Failed(format!("job object: {e}")))?;
        let mut child = Command::new(&self.exe)
            .args(["--ext", ext, "--max", &max_chars.to_string()])
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
