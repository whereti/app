//! `wt update` and `wt verify` (Administrator): replay the USN journal into a
//! saved index, and the differential test on real disks. Live following is
//! the engine's job (`wt serve`, or the service).

use std::collections::{BTreeSet, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use whereti_core::{build, snapshot, Exclusions, NameIndex};
use whereti_engine::sync::{self, default_threads, open_volumes, Batch, JournalRead, Prefetched, VolumeSource};
use whereti_engine::system;
use whereti_ntfs::Volume;

use crate::index_cmd::{default_index_path, scan_volume};
use crate::{bytes, dur, num};

#[derive(Args)]
pub struct UpdateArgs {
    /// Index file (default: %ProgramData%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Index file (default: %ProgramData%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
    /// How many differences to print
    #[arg(long, default_value_t = 10)]
    samples: usize,
}

fn load(path: &std::path::Path) -> Result<NameIndex, String> {
    snapshot::load(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            format!("no index at {} — run `wt index` first", path.display())
        } else {
            format!("cannot load {}: {e} — run `wt index` again", path.display())
        }
    })
}

/// Applies a journal batch to `index` (prefetch, then apply).
fn apply(index: &mut NameIndex, v: usize, vol: &Volume, batch: &Batch) -> Result<whereti_core::ApplyStats, String> {
    let mut pre = Prefetched::read(vol, &batch.records).map_err(|e| format!("reading MFT records: {e}"))?;
    index.apply(v, &batch.records, &batch.created, &mut pre).map_err(|e| e.to_string())
}

/// Brings every volume of `index` up to date, rescanning where replay is impossible.
fn sync_all(index: &mut NameIndex, vols: &[Volume], open: &mut [HashSet<u64>], quiet: bool) -> Result<(), String> {
    let mut rescan = Vec::new();
    for (v, vol) in vols.iter().enumerate() {
        let info = index.volumes()[v];
        let letter = info.letter();
        let t = Instant::now();
        match sync::read_since(&info, vol, &mut open[v])? {
            JournalRead::Changes { batch, journal_records, journal_id, next_usn } => {
                let stats = apply(index, v, vol, &batch)?;
                if stats.needs_rescan {
                    println!("  {letter}:  a folder was moved in from outside the index → full rescan");
                    rescan.push(letter);
                    continue;
                }
                index.set_journal_position(v, journal_id, next_usn);
                if !quiet {
                    println!(
                        "  {letter}:  replayed {} journal records → {} files/folders re-read (+{} −{} ~{}) in {}",
                        num(journal_records as u64),
                        num(stats.records as u64),
                        num(stats.added as u64),
                        num(stats.removed as u64),
                        num(stats.updated as u64),
                        dur(t.elapsed())
                    );
                }
            }
            JournalRead::Rescan(why) => {
                println!("  {letter}:  {why} → full rescan");
                rescan.push(letter);
                open[v].clear();
            }
        }
    }
    if !rescan.is_empty() {
        let mut fresh = Vec::new();
        for letter in rescan {
            fresh.extend(scan_volume(letter, default_threads())?);
        }
        *index = index.rebuild_with(fresh);
    }
    Ok(())
}

/// Re-reads files still open for writing (their size/date move without journal records).
fn refresh_open(index: &mut NameIndex, vols: &[Volume], open: &[HashSet<u64>]) -> Result<usize, String> {
    let mut n = 0;
    for (v, vol) in vols.iter().enumerate() {
        if open[v].is_empty() {
            continue;
        }
        let batch = Batch { records: open[v].iter().copied().collect(), created: HashSet::new() };
        n += apply(index, v, vol, &batch)?.records;
    }
    Ok(n)
}

fn save(index: &mut NameIndex, path: &std::path::Path) -> Result<(), String> {
    let t = Instant::now();
    let compacted = !index.is_compact();
    index.compact();
    if path == system::default_index_path() {
        system::ensure_data_dir().map_err(|e| e.to_string())?;
    }
    let size = snapshot::save(index, path).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    println!(
        "  saved    {} ({}){} in {}",
        path.display(),
        bytes(size),
        if compacted { ", compacted" } else { "" },
        dur(t.elapsed())
    );
    Ok(())
}

pub fn update(args: UpdateArgs) -> Result<(), String> {
    let path = args.index.unwrap_or_else(default_index_path);
    let t = Instant::now();
    let mut index = load(&path)?;
    println!("  loaded   {} entries in {}", num(index.len() as u64), dur(t.elapsed()));
    let vols = open_volumes(&index)?;
    let mut open = vec![HashSet::new(); vols.len()];
    sync_all(&mut index, &vols, &mut open, false)?;
    save(&mut index, &path)?;
    println!("  total    {}", dur(t.elapsed()));
    Ok(())
}

pub fn verify(args: VerifyArgs) -> Result<(), String> {
    let path = args.index.unwrap_or_else(default_index_path);
    let mut saved = load(&path)?;
    let vols = open_volumes(&saved)?;
    let mut open = vec![HashSet::new(); vols.len()];
    println!("[1] saved index + journal replay");
    sync_all(&mut saved, &vols, &mut open, false)?;

    println!("[2] fresh full scan");
    let mut scans = Vec::new();
    for v in saved.volumes() {
        scans.extend(scan_volume(v.letter(), default_threads())?);
    }
    let mut fresh = build(scans, &Exclusions { paths: saved.exclusions().to_vec() }).0;

    // Same moment for both: replay what happened during step 2, then re-read
    // the files that are still open (their size moves without journal records).
    println!("[3] bring both to the same moment");
    sync_all(&mut saved, &vols, &mut open, true)?;
    sync_all(&mut fresh, &vols, &mut open, true)?;
    let n = refresh_open(&mut saved, &vols, &open)?;
    refresh_open(&mut fresh, &vols, &open)?;
    println!("  re-read {} files still open for writing", num(n as u64));

    let (structure, meta) = differences(&saved, &fresh, args.samples, "");
    println!();
    println!("  paths that differ:            {}", num(structure as u64));
    println!("  size/date/flags that differ: {}", num(meta.len() as u64));

    // Files written since the snapshot but still open have no journal record
    // yet. Re-read exactly those in both indexes: if the logic is right, the
    // differences disappear; anything left is a bug.
    let mut meta_after = Vec::new();
    if !meta.is_empty() {
        println!(
            "[4] re-read the {} files whose size/date differ (written while open; the journal reports them at close)",
            meta.len()
        );
        let records = saved.records_for_paths(&meta.iter().cloned().collect());
        for (v, vol) in vols.iter().enumerate() {
            let rs: Vec<u64> = records.iter().filter(|(rv, _)| *rv == v).map(|(_, r)| *r).collect();
            if rs.is_empty() {
                continue;
            }
            for idx in [&mut saved, &mut fresh] {
                let mut src = VolumeSource::new(vol);
                idx.apply(v, &rs, &HashSet::new(), &mut src).map_err(|e| e.to_string())?;
            }
        }
        let (s2, m2) = differences(&saved, &fresh, args.samples, "      still: ");
        if s2 != 0 {
            println!("  paths that differ after re-read: {}", num(s2 as u64));
        }
        println!("  size/date that differ after re-read: {}", num(m2.len() as u64));
        meta_after = m2;
    }
    let ok = structure == 0 && meta_after.is_empty();
    println!(
        "  result   {}",
        if !ok {
            "FAIL: indexes differ"
        } else if meta.is_empty() {
            "PASS: identical"
        } else {
            "PASS: identical (after re-reading files still open for writing)"
        }
    );
    if ok {
        Ok(())
    } else {
        Err("verification failed".into())
    }
}

/// Compares two indexes: (number of paths present in only one, paths whose
/// size/date/flags differ). Prints up to `samples` examples with `prefix`.
fn differences(a: &NameIndex, b: &NameIndex, samples: usize, prefix: &str) -> (usize, Vec<String>) {
    let ca = a.canonical();
    let cb = b.canonical();
    let pa: BTreeSet<_> = ca.iter().map(|e| (e.0.clone(), e.1)).collect();
    let pb: BTreeSet<_> = cb.iter().map(|e| (e.0.clone(), e.1)).collect();
    let structure: Vec<_> = pa.symmetric_difference(&pb).collect();
    for p in structure.iter().take(samples) {
        println!("      path only on one side: {p:?}");
    }
    let meta: BTreeSet<String> = ca
        .symmetric_difference(&cb)
        .filter(|e| pa.contains(&(e.0.clone(), e.1)) && pb.contains(&(e.0.clone(), e.1)))
        .map(|e| e.0.clone())
        .collect();
    let pre = if prefix.is_empty() { "      " } else { prefix };
    for path in meta.iter().take(samples) {
        let side = |c: &BTreeSet<(String, bool, u64, u32, u16)>| {
            c.iter().find(|e| &e.0 == path).map(|e| format!("{} B, minute {}", e.2, e.3)).unwrap_or_default()
        };
        println!("{pre}{path}: saved {} · fresh {}", side(&ca), side(&cb));
    }
    (structure.len(), meta.into_iter().collect())
}
