//! `wt index` - scan volumes, build the compact name index, save it.

use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use whereti_core::{build, snapshot, Exclusions, VolumeScan};
use whereti_engine::sync::{default_threads, fixed_drives};
use whereti_engine::system;

use crate::{bytes, dur, num, print_memory};

#[derive(Args)]
pub struct IndexArgs {
    /// Volumes to index, e.g. C: D: (default: every fixed drive)
    volumes: Vec<String>,
    /// Where to save the index (default: %ProgramData%\whereti\names.idx)
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
        args.volumes.iter().map(|v| crate::parse_letter(v)).collect::<Result<_, _>>()?
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
    println!("  memory   {} = {:.1} B/entry", bytes(mem as u64), mem as f64 / index.len().max(1) as f64);

    if path == system::default_index_path() {
        system::ensure_data_dir().map_err(|e| format!("cannot create {}: {e}", system::data_dir().display()))?;
    }
    let t = Instant::now();
    let size = snapshot::save(&index, &path).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    println!("  saved    {} ({}) in {}", path.display(), bytes(size), dur(t.elapsed()));
    println!("  total    {}", dur(total.elapsed()));
    print_memory();
    Ok(())
}

/// Full MFT scan of one volume with progress output. `Ok(None)` if not NTFS.
pub(crate) fn scan_volume(letter: char, threads: usize) -> Result<Option<VolumeScan>, String> {
    let t = Instant::now();
    let scan = whereti_engine::sync::scan_volume(letter, threads)?;
    match &scan {
        Some(s) => println!("  {letter}:  scanned {} entries in {}", num(s.scan.entries.len() as u64), dur(t.elapsed())),
        None => println!("  {letter}:  skipped (not NTFS)"),
    }
    Ok(scan)
}

/// `%ProgramData%\whereti\names.idx`, readable only by SYSTEM and Administrators.
pub fn default_index_path() -> PathBuf {
    system::default_index_path()
}
