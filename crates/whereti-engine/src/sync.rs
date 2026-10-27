//! Keeping an index in sync with the volumes (PLAN.md §4.4, §7.2).
//!
//! Applying a journal batch has two phases, so a shared index is locked only
//! briefly: (1) read the journal and the current MFT records - slow, no lock;
//! (2) apply the prefetched states to the index - fast, under the write lock.
//!
//! Two journal facts shape this code:
//! - Creating or deleting a file changes its folder's date, but the journal
//!   reports only the file. Every change therefore also re-reads the parent.
//! - For a file held open (a log being appended), the journal writes one record
//!   at the first change and one at close, not one per write. Such files are
//!   tracked as "open" and re-read periodically until their close record.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc;
use std::time::Duration;

use whereti_core::{NameIndex, RecordSource, RecordState, VolumeInfo, VolumeScan};
use whereti_ntfs::journal::{self, JournalError, UsnChange, USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE};
use whereti_ntfs::parse::{FRN_MASK, NS_DOS};
use whereti_ntfs::{mft, Volume};

/// Reads current record states straight from the MFT.
pub struct VolumeSource<'a> {
    vol: &'a Volume,
    buf: Vec<u8>,
}

impl<'a> VolumeSource<'a> {
    pub fn new(vol: &'a Volume) -> Self {
        VolumeSource { vol, buf: Vec::new() }
    }
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

/// Record states read ahead of time (phase 1), applied later under a lock.
pub struct Prefetched(HashMap<u64, Option<RecordState>>);

impl Prefetched {
    pub fn read(vol: &Volume, records: &[u64]) -> io::Result<Prefetched> {
        let mut src = VolumeSource::new(vol);
        let mut map = HashMap::with_capacity(records.len());
        for &r in records {
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(r) {
                e.insert(src.record(r)?);
            }
        }
        Ok(Prefetched(map))
    }
}

impl RecordSource for Prefetched {
    fn record(&mut self, record: u64) -> io::Result<Option<RecordState>> {
        match self.0.get(&record) {
            Some(s) => Ok(s.clone()),
            None => Err(io::Error::other(format!("record {record} was not prefetched"))),
        }
    }
}

/// Records to re-read for a run of journal records.
#[derive(Default, Debug)]
pub struct Batch {
    pub records: Vec<u64>,
    pub created: HashSet<u64>,
}

impl Batch {
    /// Adds the changed files and their folders; tracks which files stay open.
    pub fn add(&mut self, changes: &[UsnChange], open: &mut HashSet<u64>) {
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

pub enum JournalRead {
    Changes { batch: Batch, journal_records: usize, journal_id: u64, next_usn: i64 },
    /// The journal cannot bring this volume up to date; scan it again.
    Rescan(&'static str),
}

/// Phase 1 of a catch-up: everything the journal recorded since the index's
/// saved position for this volume.
pub fn read_since(info: &VolumeInfo, vol: &Volume, open: &mut HashSet<u64>) -> Result<JournalRead, String> {
    if vol.ntfs.volume_serial_number as u64 != info.serial {
        return Ok(JournalRead::Rescan("a different volume has this letter now"));
    }
    let Ok(j) = vol.query_usn_journal() else { return Ok(JournalRead::Rescan("the journal is not active")) };
    if j.usn_journal_id != info.journal_id {
        return Ok(JournalRead::Rescan("the journal was recreated"));
    }
    if info.next_usn < j.first_usn {
        return Ok(JournalRead::Rescan("too many changes: the journal was overwritten"));
    }
    if info.next_usn > j.next_usn {
        // same journal, but the index is ahead of it: the disk was restored
        // (a VM snapshot, a disk image) and what happened since is unknown
        return Ok(JournalRead::Rescan("the journal is behind the index (the disk was restored)"));
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
            Err(JournalError::Overwritten) => return Ok(JournalRead::Rescan("too many changes: the journal was overwritten")),
            Err(JournalError::Replaced) => return Ok(JournalRead::Rescan("the journal was recreated")),
            Err(JournalError::Io(e)) => return Err(format!("{}: reading the journal failed: {e}", info.letter())),
        }
    }
    Ok(JournalRead::Changes { batch, journal_records, journal_id: j.usn_journal_id, next_usn: pos })
}

/// Full MFT scan of one volume. `Ok(None)` if it is not NTFS.
pub fn scan_volume(letter: char, threads: usize) -> Result<Option<VolumeScan>, String> {
    let vol = match Volume::open(letter, true) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::Unsupported => return Ok(None),
        Err(e) => return Err(open_error(letter, e)),
    };
    // Journal position BEFORE the scan: replay starts here (PLAN.md §4.1).
    let journal = vol.query_usn_journal().ok();
    let scan = mft::scan(&vol, threads).map_err(|e| format!("{letter}: scan failed: {e}"))?;
    let (journal_id, next_usn) = journal_position(journal.map(|j| (j.usn_journal_id, j.next_usn)));
    Ok(Some(VolumeScan { letter, serial: vol.ntfs.volume_serial_number as u64, journal_id, next_usn, scan }))
}

/// Where replay starts for a scanned volume. Without an active journal there
/// is nothing to follow: the volume is scanned again now and then, like a
/// walked drive, and no journal is created on the user's disk.
fn journal_position(journal: Option<(u64, i64)>) -> (u64, i64) {
    journal.unwrap_or((crate::walk::WALKED, 0))
}

/// Scans any drive: through the MFT when it is NTFS, else by walking its
/// folders (crate::walk). An NTFS drive whose MFT cannot be read is walked too.
pub fn scan_any(drive: &crate::walk::Drive, threads: usize) -> Result<VolumeScan, String> {
    if drive.is_ntfs() {
        match scan_volume(drive.letter, threads) {
            Ok(Some(scan)) => return Ok(scan),
            Ok(None) => {}
            Err(e) if e.starts_with("access denied") => return Err(e),
            Err(_) => {} // e.g. a BitLocker or damaged volume: walk it instead
        }
    }
    crate::walk::walk(drive).map_err(|e| format!("{}: cannot read the drive: {e}", drive.letter))
}

pub fn open_volumes(index: &NameIndex) -> Result<Vec<Volume>, String> {
    index
        .volumes()
        .iter()
        .map(|v| Volume::open(v.letter(), false).map_err(|e| open_error(v.letter(), e)))
        .collect()
}

pub fn open_error(letter: char, e: io::Error) -> String {
    if e.raw_os_error() == Some(5) {
        format!("access denied to {letter}: - run from an Administrator terminal")
    } else {
        format!("cannot open {letter}: - {e}")
    }
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
}

/// Fixed drives (C:, D:, …), whatever their file system.
pub fn fixed_drives() -> Vec<char> {
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

/// New journal records for one volume, sent by a reader thread. Volumes are
/// named by letter: their order in the index changes when drives come and go.
pub struct JournalMsg {
    pub letter: char,
    /// The journal the reader follows: a message about a journal the index
    /// no longer follows (the drive was rescanned, or is another disk) is dropped.
    pub journal_id: u64,
    pub changes: Vec<UsnChange>,
    pub next: i64,
    pub error: Option<String>,
}

/// Reader thread: sleeps in the kernel until the journal has new records and
/// forwards them. Uses its own handle, so a blocked read never holds up the
/// record reads on the caller's handle.
/// `stop`: set before the reader is cancelled (the drive is being removed
/// safely); it then ends quietly instead of reporting an error.
pub fn follow_journal<M: From<JournalMsg> + Send + 'static>(
    letter: char,
    journal_id: u64,
    mut pos: i64,
    tx: mpsc::Sender<M>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let vol = match Volume::open(letter, false) {
        Ok(vol) => vol,
        Err(e) => {
            let _ = tx.send(JournalMsg { letter, journal_id, changes: Vec::new(), next: pos, error: Some(e.to_string()) }.into());
            return;
        }
    };
    let mut buf = Vec::new();
    let mut empty_reads = 0u32;
    loop {
        let read = journal::read_journal(&vol, journal_id, pos, true, &mut buf);
        if stop.load(Ordering::SeqCst) {
            return; // closes this thread's handle on the drive
        }
        match read {
            Ok((changes, next)) => {
                if changes.is_empty() {
                    // A blocking read should not return empty; never spin if it does.
                    empty_reads += 1;
                    std::thread::sleep(Duration::from_millis(if empty_reads > 3 { 500 } else { 50 }));
                } else {
                    empty_reads = 0;
                }
                pos = next.max(pos);
                if !changes.is_empty()
                    && tx.send(JournalMsg { letter, journal_id, changes, next: pos, error: None }.into()).is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let msg = format!("{letter}: journal: {e}");
                let _ = tx.send(JournalMsg { letter, journal_id, changes: Vec::new(), next: pos, error: Some(msg) }.into());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_without_a_journal_is_scanned_again_like_a_walked_drive() {
        assert_eq!(journal_position(None), (crate::walk::WALKED, 0));
        assert_eq!(journal_position(Some((7, 4096))), (7, 4096));
    }
}
