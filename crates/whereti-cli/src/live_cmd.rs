//! Keeping the index in sync with the disk (PLAN.md §4.4, §7.2):
//!
//! - `wt update`: replay the USN journal from where the saved index stopped.
//! - `wt watch`: follow the journal live (threads sleep in the kernel, 0% CPU idle).
//! - `wt verify`: differential test on real disks — saved index + replay must
//!   equal a fresh full scan.
//!
//! All three need an Administrator terminal (they read the volumes directly).
//!
//! Two journal facts shape this code:
//! - Creating or deleting a file changes its folder's date, but the journal
//!   reports only the file. Every change therefore also re-reads the parent.
//! - For a file held open (a log being appended), the journal writes one record
//!   at the first change and one at close, not one per write. Such files are
//!   tracked as "open" and re-read periodically until their close record.

use std::collections::{BTreeSet, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::Args;
use whereti_core::{build, snapshot, ApplyStats, Exclusions, NameIndex, RecordSource, RecordState};
use whereti_ntfs::journal::{
    self, JournalError, UsnChange, USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME,
};
use whereti_ntfs::parse::{FRN_MASK, NS_DOS};
use whereti_ntfs::Volume;

use crate::index_cmd::{default_index_path, default_threads, scan_volume};
use crate::{bytes, dur, num, open_error, print_memory};

/// How often `wt watch` re-reads files that are still open.
const OPEN_REFRESH: Duration = Duration::from_secs(30);

#[derive(Args)]
pub struct UpdateArgs {
    /// Index file (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
}

#[derive(Args)]
pub struct WatchArgs {
    /// Index file (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
    /// Save the index at most this often, in seconds, when something changed
    #[arg(long, default_value_t = 300)]
    save_every: u64,
    /// Stop after this many seconds (default: run until closed)
    #[arg(long)]
    seconds: Option<u64>,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Index file (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
    /// How many differences to print
    #[arg(long, default_value_t = 10)]
    samples: usize,
}

/// Reads current record states straight from the MFT.
struct VolumeSource<'a> {
    vol: &'a Volume,
    buf: Vec<u8>,
}

impl RecordSource for VolumeSource<'_> {
    fn record(&mut self, record: u64) -> io::Result<Option<RecordState>> {
        let Some(info) = journal::read_record(self.vol, record, &mut self.buf)? else { return Ok(None) };
        let has_long = info.names.iter().any(|n| n.namespace != NS_DOS);
        let mut links: Vec<(u64, String)> = Vec::new();
        for n in &info.names {
            if has_long && n.namespace == NS_DOS {
                continue;
            }
            let link = (n.parent_ref & FRN_MASK, n.name.clone());
            if !links.contains(&link) {
                links.push(link);
            }
        }
        let std = info.std.unwrap_or_default();
        Ok(Some(RecordState {
            is_dir: info.is_dir(),
            attributes: std.attributes,
            size: if info.is_dir() { 0 } else { info.data_size.unwrap_or(0) },
            modified: std.modified,
            links,
            complete: !info.attr_list_nonresident,
        }))
    }
}

/// Records to re-read for a run of journal records.
#[derive(Default)]
struct Batch {
    records: Vec<u64>,
    created: HashSet<u64>,
}

impl Batch {
    /// Adds the changed files and their folders; tracks which files stay open.
    fn add(&mut self, changes: &[UsnChange], open: &mut HashSet<u64>) {
        for c in changes {
            self.records.push(c.record);
            self.records.push(c.parent);
            if c.reason & USN_REASON_FILE_CREATE != 0 {
                self.created.insert(c.record);
            }
            if c.reason & (USN_REASON_CLOSE | USN_REASON_FILE_DELETE) != 0 {
                open.remove(&c.record);
            } else {
                open.insert(c.record);
            }
        }
    }
}

enum CatchUp {
    Done { journal_records: usize, stats: ApplyStats },
    Rescan(&'static str),
}

/// Replays volume `v`'s journal from the index's saved position to now.
fn catch_up(index: &mut NameIndex, v: usize, vol: &Volume, open: &mut HashSet<u64>) -> Result<CatchUp, String> {
    let info = index.volumes()[v];
    if vol.ntfs.volume_serial_number as u64 != info.serial {
        return Ok(CatchUp::Rescan("a different volume has this letter now"));
    }
    let Ok(j) = vol.query_usn_journal() else { return Ok(CatchUp::Rescan("the journal is not active")) };
    if j.usn_journal_id != info.journal_id {
        return Ok(CatchUp::Rescan("the journal was recreated"));
    }
    if info.next_usn < j.first_usn {
        return Ok(CatchUp::Rescan("too many changes: the journal was overwritten"));
    }
    let mut pos = info.next_usn;
    let mut buf = Vec::new();
    let mut batch = Batch::default();
    let mut journal_records = 0;
    while pos < j.next_usn {
        match journal::read_journal(vol, j.usn_journal_id, pos, false, &mut buf) {
            Ok((changes, next)) => {
                journal_records += changes.len();
                batch.add(&changes, open);
                if next <= pos {
                    break;
                }
                pos = next;
            }
            Err(JournalError::Overwritten) => return Ok(CatchUp::Rescan("too many changes: the journal was overwritten")),
            Err(JournalError::Replaced) => return Ok(CatchUp::Rescan("the journal was recreated")),
            Err(JournalError::Io(e)) => return Err(format!("{}: reading the journal failed: {e}", info.letter())),
        }
    }
    let mut src = VolumeSource { vol, buf: Vec::new() };
    let stats = index
        .apply(v, &batch.records, &batch.created, &mut src)
        .map_err(|e| format!("{}: reading MFT records failed: {e}", info.letter()))?;
    if stats.needs_rescan {
        return Ok(CatchUp::Rescan("a folder was moved in from outside the index"));
    }
    index.set_journal_position(v, j.usn_journal_id, pos);
    Ok(CatchUp::Done { journal_records, stats })
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

fn open_volumes(index: &NameIndex) -> Result<Vec<Volume>, String> {
    index
        .volumes()
        .iter()
        .map(|v| Volume::open(v.letter(), false).map_err(|e| open_error(v.letter(), e)))
        .collect()
}

/// Brings every volume of `index` up to date, rescanning where replay is impossible.
fn sync_all(index: &mut NameIndex, vols: &[Volume], open: &mut [HashSet<u64>], quiet: bool) -> Result<(), String> {
    let mut rescan = Vec::new();
    for (v, vol) in vols.iter().enumerate() {
        let letter = index.volumes()[v].letter();
        let t = Instant::now();
        match catch_up(index, v, vol, &mut open[v])? {
            CatchUp::Done { journal_records, stats } => {
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
            CatchUp::Rescan(why) => {
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
        let records: Vec<u64> = open[v].iter().copied().collect();
        let mut src = VolumeSource { vol, buf: Vec::new() };
        let stats = index.apply(v, &records, &HashSet::new(), &mut src).map_err(|e| e.to_string())?;
        n += stats.records;
    }
    Ok(n)
}

fn save(index: &mut NameIndex, path: &std::path::Path) -> Result<(), String> {
    let t = Instant::now();
    let compacted = !index.is_compact();
    index.compact();
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

struct JournalMsg {
    vol: usize,
    changes: Vec<UsnChange>,
    next: i64,
    error: Option<String>,
}

pub fn watch(args: WatchArgs) -> Result<(), String> {
    let path = args.index.unwrap_or_else(default_index_path);
    let mut index = load(&path)?;
    let vols = open_volumes(&index)?;
    let mut open = vec![HashSet::new(); vols.len()];
    sync_all(&mut index, &vols, &mut open, false)?;
    println!("  watching {} — changes appear below", letters(&index));

    // One reader thread per volume, each with its own handle: a blocked
    // journal read must not hold up the record reads on the main handle.
    let (tx, rx) = mpsc::channel::<JournalMsg>();
    for (v, info) in index.volumes().iter().enumerate() {
        let tx = tx.clone();
        let (letter, journal_id, start) = (info.letter(), info.journal_id, info.next_usn);
        std::thread::spawn(move || follow_journal(v, letter, journal_id, start, tx));
    }
    drop(tx);

    let started = Instant::now();
    let save_every = Duration::from_secs(args.save_every.max(5));
    let mut last_save = Instant::now();
    let mut last_refresh = Instant::now();
    let mut dirty = false;
    loop {
        if args.seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
            break;
        }
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_refresh.elapsed() >= OPEN_REFRESH {
                    let n = refresh_open(&mut index, &vols, &open)?;
                    if n > 0 {
                        dirty = true;
                    }
                    last_refresh = Instant::now();
                }
                if dirty && last_save.elapsed() >= save_every {
                    save(&mut index, &path)?;
                    last_save = Instant::now();
                    dirty = false;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Batch everything that arrives within 250 ms.
        let mut msgs = vec![first];
        let deadline = Instant::now() + Duration::from_millis(250);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(m) => msgs.push(m),
                Err(_) => break,
            }
        }
        if let Some(err) = msgs.iter().find_map(|m| m.error.clone()) {
            return Err(err);
        }
        for (v, vol) in vols.iter().enumerate() {
            let mine: Vec<&JournalMsg> = msgs.iter().filter(|m| m.vol == v).collect();
            if mine.is_empty() {
                continue;
            }
            let t = Instant::now();
            let mut batch = Batch::default();
            for m in &mine {
                batch.add(&m.changes, &mut open[v]);
            }
            let next = mine.iter().map(|m| m.next).max().unwrap_or(0);
            let mut src = VolumeSource { vol, buf: Vec::new() };
            let stats = index.apply(v, &batch.records, &batch.created, &mut src).map_err(|e| e.to_string())?;
            let letter = index.volumes()[v].letter();
            let journal_id = index.volumes()[v].journal_id;
            if stats.needs_rescan {
                println!("  {letter}:  a folder was moved in from outside the index → full rescan");
                let fresh: Vec<_> = scan_volume(letter, default_threads())?.into_iter().collect();
                index = index.rebuild_with(fresh);
                open[v].clear();
            }
            index.set_journal_position(v, journal_id, next);
            dirty = true;
            let changes: Vec<&UsnChange> = mine.iter().flat_map(|m| &m.changes).collect();
            println!(
                "  {letter}:  {} journal records → +{} −{} ~{} in {}",
                num(changes.len() as u64),
                stats.added,
                stats.removed,
                stats.updated,
                dur(t.elapsed())
            );
            let mut shown = BTreeSet::new();
            for c in changes.iter().filter(|c| is_interesting(c)) {
                if shown.len() == 6 {
                    break;
                }
                if shown.insert((reason_symbol(c.reason), c.name.as_str())) {
                    println!("        {} {}", reason_symbol(c.reason), c.name);
                }
            }
        }
        if index.overlay_ratio() > 0.01 {
            let t = Instant::now();
            index.compact();
            println!("  compacted the index in {}", dur(t.elapsed()));
        }
    }
    if dirty {
        save(&mut index, &path)?;
    }
    print_memory();
    Ok(())
}

/// Reader thread: sleeps in the kernel until the journal has new records.
fn follow_journal(v: usize, letter: char, journal_id: u64, mut pos: i64, tx: mpsc::Sender<JournalMsg>) {
    let vol = match Volume::open(letter, false) {
        Ok(vol) => vol,
        Err(e) => {
            let _ = tx.send(JournalMsg { vol: v, changes: Vec::new(), next: pos, error: Some(e.to_string()) });
            return;
        }
    };
    let mut buf = Vec::new();
    let mut empty_reads = 0u32;
    loop {
        match journal::read_journal(&vol, journal_id, pos, true, &mut buf) {
            Ok((changes, next)) => {
                if changes.is_empty() {
                    // A blocking read should not return empty; never spin if it does.
                    empty_reads += 1;
                    std::thread::sleep(Duration::from_millis(if empty_reads > 3 { 500 } else { 50 }));
                } else {
                    empty_reads = 0;
                }
                pos = next.max(pos);
                if !changes.is_empty() && tx.send(JournalMsg { vol: v, changes, next: pos, error: None }).is_err() {
                    return;
                }
            }
            Err(e) => {
                let msg = format!("{letter}: journal: {e} — run `wt update` to rescan");
                let _ = tx.send(JournalMsg { vol: v, changes: Vec::new(), next: pos, error: Some(msg) });
                return;
            }
        }
    }
}

fn is_interesting(c: &UsnChange) -> bool {
    c.reason & (USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME) != 0
}

fn reason_symbol(reason: u32) -> &'static str {
    if reason & USN_REASON_FILE_DELETE != 0 {
        "−"
    } else if reason & USN_REASON_FILE_CREATE != 0 {
        "+"
    } else {
        "→"
    }
}

fn letters(index: &NameIndex) -> String {
    index.volumes().iter().map(|v| format!("{}:", v.letter())).collect::<Vec<_>>().join(" ")
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
        println!("[4] re-read the {} files whose size/date differ (written while open; the journal reports them at close)", meta.len());
        let records = saved.records_for_paths(&meta.iter().cloned().collect());
        for (v, vol) in vols.iter().enumerate() {
            let rs: Vec<u64> = records.iter().filter(|(rv, _)| *rv == v).map(|(_, r)| *r).collect();
            if rs.is_empty() {
                continue;
            }
            for idx in [&mut saved, &mut fresh] {
                let mut src = VolumeSource { vol, buf: Vec::new() };
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
    if !prefix.is_empty() || !structure.is_empty() {
        for p in structure.iter().take(samples) {
            println!("      path only on one side: {p:?}");
        }
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
