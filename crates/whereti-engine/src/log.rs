//! A small logger: timestamped lines to a size-capped file and/or the console,
//! plus the last lines in memory for `wt status`. No telemetry, nothing leaves
//! the PC (PLAN.md §8).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use parking_lot::Mutex;

const MAX_FILE: u64 = 10 << 20;
const RECENT: usize = 30;

pub struct Logger {
    file: Option<Mutex<File>>,
    console: bool,
    recent: Mutex<VecDeque<String>>,
}

impl Logger {
    pub fn console() -> Logger {
        Logger { file: None, console: true, recent: Mutex::new(VecDeque::new()) }
    }

    /// Appends to `path`; a file over 10 MB is first moved to `path.1`.
    pub fn file(path: &Path, also_console: bool) -> io::Result<Logger> {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_FILE) {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Logger { file: Some(Mutex::new(f)), console: also_console, recent: Mutex::new(VecDeque::new()) })
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        let line = format!("{}  {}", crate::system::local_timestamp(), msg.as_ref());
        if self.console {
            println!("{line}");
        }
        if let Some(f) = &self.file {
            let _ = writeln!(f.lock(), "{line}");
        }
        let mut r = self.recent.lock();
        if r.len() == RECENT {
            r.pop_front();
        }
        r.push_back(line);
    }

    pub fn recent(&self) -> Vec<String> {
        self.recent.lock().iter().cloned().collect()
    }
}
