//! `wt index` — scan volumes, build the compact name index, save it.

use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use whereti_core::{build, snapshot, Exclusions, VolumeScan};
use whereti_ntfs::{mft, Volume};

use crate::{bytes, dur, num, open_error, print_memory};

#[derive(Args)]
pub struct IndexArgs {
    /// Volumes to index, e.g. C: D: (default: every fixed drive)
    volumes: Vec<String>,
    /// Where to save the index (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
    /// Keep C:\Windows in the index (excluded by default)
    #[arg(long)]
    include_windows: bool,
    /// Parser threads per volume (default: logical CPUs, max 8)
    #[arg(long)]
    threads: Option<usize>,
}

pub fn run(args: IndexArgs) -> Result<(), String> {
    let letters: Vec<char> = if args.volumes.is_empty() {
        fixed_drives()
    } else {
        args.volumes
            .iter()
            .map(|v| crate::parse_letter(v))
            .collect::<Result<_, _>>()?
    };
    if letters.is_empty() {
        return Err("no volumes to index".into());
    }
    let threads = args.threads.unwrap_or_else(default_threads);
    let path = args.index.unwrap_or_else(default_index_path);
    let total = Instant::now();

    let mut scans = Vec::new();
    for letter in letters {
        if let Some(scan) = scan_volume(letter, threads)? {
            scans.push(scan);
        }
    }

    let windows_dir = (!args.include_windows).then(|| std::env::var("SystemRoot").ok()).flatten();
    let exclusions = Exclusions::defaults(windows_dir.as_deref());
    let t = Instant::now();
    let (index, stats) = build(scans, &exclusions);
    println!(
        "  build    {} kept, {} excluded ({}) in {}",
        num(stats.kept as u64),
        num(stats.excluded as u64),
        windows_dir.as_deref().map(|w| format!("{w}, system folders")).unwrap_or_else(|| "system folders".into()),
        dur(t.elapsed())
    );
    let mem = index.memory_bytes();
    println!(
        "  memory   {} = {:.1} B/entry",
        bytes(mem as u64),
        mem as f64 / index.len().max(1) as f64
    );

    let t = Instant::now();
    let size = snapshot::save(&index, &path).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    println!("  saved    {} ({}) in {}", path.display(), bytes(size), dur(t.elapsed()));
    println!("  total    {}", dur(total.elapsed()));
    print_memory();
    Ok(())
}

/// Full MFT scan of one volume. `Ok(None)` if it is not NTFS.
pub(crate) fn scan_volume(letter: char, threads: usize) -> Result<Option<VolumeScan>, String> {
    let vol = match Volume::open(letter, true) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            println!("  {letter}:  skipped ({e})");
            return Ok(None);
        }
        Err(e) => return Err(open_error(letter, e)),
    };
    // Journal position BEFORE the scan: replay starts here (PLAN.md §4.1).
    let journal = vol.query_usn_journal().ok();
    let t = Instant::now();
    let scan = mft::scan(&vol, threads).map_err(|e| format!("{letter}: scan failed: {e}"))?;
    println!("  {letter}:  scanned {} entries in {}", num(scan.entries.len() as u64), dur(t.elapsed()));
    Ok(Some(VolumeScan {
        letter,
        serial: vol.ntfs.volume_serial_number as u64,
        journal_id: journal.map(|j| j.usn_journal_id).unwrap_or(0),
        next_usn: journal.map(|j| j.next_usn).unwrap_or(0),
        scan,
    }))
}

pub(crate) fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
}

pub fn default_index_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("names.idx")
}

fn fixed_drives() -> Vec<char> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i) as char)
        .filter(|c| {
            let root: Vec<u16> = format!("{c}:\\").encode_utf16().chain([0]).collect();
            // SAFETY: NUL-terminated string.
            unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
        })
        .collect()
}
