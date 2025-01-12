//! Direct `$MFT` reader: one pass gives names, parents, sizes, dates and
//! attributes. One thread reads large sequential chunks; worker threads parse
//! them in parallel. The channel is bounded, so memory stays flat however big
//! the MFT is.
//!
//! Output is compact: fixed-size entries plus one shared UTF-8 name buffer,
//! with no allocation per file.

use std::collections::{HashMap, HashSet};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::parse::{self, FileName, ParseOutcome, RecordInfo, Run, FRN_MASK, NS_DOS};
use crate::volume::{AlignedBuf, Volume};

const CHUNK_BYTES: usize = 4 << 20;

/// One name of one file or folder (a hard link has one entry per name).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MftEntry {
    pub record: u64,
    /// Parent record number (sequence stripped).
    pub parent: u64,
    /// Logical size of the unnamed data stream (0 for directories).
    pub size: u64,
    /// Last write time, FILETIME.
    pub modified: u64,
    pub attributes: u32,
    /// Byte range of the UTF-8 name in [`MftScan::names`].
    pub name_off: u32,
    pub name_len: u16,
    pub seq: u16,
    pub is_dir: bool,
}

#[derive(Debug, Default, Clone)]
pub struct MftStats {
    pub record_size: u32,
    pub total_records: u64,
    pub in_use: u64,
    pub base_records: u64,
    pub extension_records: u64,
    pub corrupt_records: u64,
    pub unused_records: u64,
    pub records_without_name: u64,
    pub mft_fragments: usize,
    pub bytes_read: u64,
    pub read_time: Duration,
    pub threads: usize,
}

#[derive(Debug, Default)]
pub struct MftScan {
    pub entries: Vec<MftEntry>,
    /// All names, UTF-8, back to back.
    pub names: Vec<u8>,
    pub stats: MftStats,
}

impl MftScan {
    pub fn name(&self, e: &MftEntry) -> &str {
        let b = &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
        // Names are written from `String`s only, so they are valid UTF-8.
        std::str::from_utf8(b).unwrap_or("")
    }

    /// Appends one entry. Used by the scanner and by tests that build trees.
    pub fn push(&mut self, mut e: MftEntry, name: &str) {
        let name = truncate_utf8(name, u16::MAX as usize);
        e.name_off = self.names.len() as u32;
        e.name_len = name.len() as u16;
        self.names.extend_from_slice(name.as_bytes());
        self.entries.push(e);
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Chunk {
    first_record: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct ChunkResult {
    first_record: u64,
    scan: MftScan,
    extensions: Vec<RecordInfo>,
    base_records: u64,
    corrupt: u64,
    unused: u64,
    without_name: u64,
}

pub fn scan(vol: &Volume, threads: usize) -> io::Result<MftScan> {
    let rs = vol.ntfs.bytes_per_file_record_segment as usize;
    let cs = vol.ntfs.bytes_per_cluster as u64;
    let ss = vol.ntfs.bytes_per_sector as usize;
    let threads = threads.max(1);

    // 1. Read record 0 ($MFT itself) to find where the rest of the MFT lives.
    let first_len = round_up(rs, ss);
    let mut first = AlignedBuf::new(first_len, ss.max(4096));
    vol.read_exact_at(vol.ntfs.mft_start_lcn as u64 * cs, first.as_mut_slice())?;
    let mut rec0 = first.as_mut_slice()[..rs].to_vec();
    let ParseOutcome::Record(mft) = parse::parse_record(0, &mut rec0, true) else {
        return Err(invalid("cannot parse $MFT record 0"));
    };
    let Some(data) = mft.data_runs else {
        return Err(invalid("$MFT has no data run list"));
    };
    let mft_size = mft.data_size.unwrap_or(0).min(vol.ntfs.mft_valid_data_length as u64);
    let covered: u64 = data.runs.iter().map(|r| r.clusters).sum::<u64>() * cs;
    if covered < mft_size {
        // The run list continues in extension records ($ATTRIBUTE_LIST).
        // Rare (extremely fragmented MFT); the caller falls back to ENUM_USN_DATA.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "$MFT run list spans multiple records (not supported yet)",
        ));
    }
    let total_records = mft_size / rs as u64;

    let mut stats = MftStats {
        record_size: rs as u32,
        total_records,
        mft_fragments: data.runs.len(),
        threads,
        ..Default::default()
    };

    // 2. Pipeline: reader (this thread) -> bounded channel -> parsers.
    let chunk_bytes = (CHUNK_BYTES / rs) * rs;
    let (tx, rx) = crossbeam_channel::bounded::<Chunk>(threads * 2);
    let mut results: Vec<ChunkResult> = thread::scope(|s| -> io::Result<Vec<ChunkResult>> {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let rx = rx.clone();
                s.spawn(move || {
                    let mut out = Vec::new();
                    for mut chunk in rx {
                        out.push(parse_chunk(&mut chunk, rs));
                    }
                    out
                })
            })
            .collect();
        drop(rx);

        let started = Instant::now();
        let read = read_mft(vol, &data.runs, mft_size, chunk_bytes, &tx);
        drop(tx);
        stats.read_time = started.elapsed();

        let mut all = Vec::new();
        for w in workers {
            all.extend(w.join().expect("parser thread panicked"));
        }
        stats.bytes_read = read?;
        Ok(all)
    })?;

    // 3. Concatenate chunk results in record order.
    results.sort_unstable_by_key(|r| r.first_record);
    let total_entries: usize = results.iter().map(|r| r.scan.entries.len()).sum();
    let total_names: usize = results.iter().map(|r| r.scan.names.len()).sum();
    let mut out = MftScan {
        entries: Vec::with_capacity(total_entries),
        names: Vec::with_capacity(total_names),
        stats: MftStats::default(),
    };
    let mut extensions = Vec::new();
    for r in results {
        stats.corrupt_records += r.corrupt;
        stats.unused_records += r.unused;
        stats.base_records += r.base_records;
        stats.records_without_name += r.without_name;
        let base = out.names.len() as u32;
        out.names.extend_from_slice(&r.scan.names);
        out.entries.extend(r.scan.entries.into_iter().map(|mut e| {
            e.name_off += base;
            e
        }));
        extensions.extend(r.extensions);
    }
    stats.extension_records = extensions.len() as u64;
    stats.in_use = stats.base_records + stats.extension_records;

    // 4. Names stored in extension records (files with many hard links) become
    //    extra entries of their base record.
    if !extensions.is_empty() {
        merge_extensions(&mut out, extensions);
    }
    out.stats = stats;
    Ok(out)
}

fn merge_extensions(out: &mut MftScan, extensions: Vec<RecordInfo>) {
    let wanted: HashSet<u64> = extensions.iter().map(|e| e.base).collect();
    let mut first: HashMap<u64, usize> = HashMap::with_capacity(wanted.len());
    for (i, e) in out.entries.iter().enumerate() {
        if wanted.contains(&e.record) {
            first.entry(e.record).or_insert(i);
        }
    }
    let mut existing: HashSet<(u64, u64, Vec<u8>)> = HashSet::new();
    for e in out.entries.iter().filter(|e| wanted.contains(&e.record)) {
        existing.insert((e.record, e.parent, out.name(e).as_bytes().to_vec()));
    }
    for ext in extensions {
        let Some(&bi) = first.get(&ext.base) else { continue };
        let template = out.entries[bi];
        for n in ext.names.iter().filter(|n| n.namespace != NS_DOS) {
            let parent = n.parent_ref & FRN_MASK;
            if !existing.insert((ext.base, parent, n.name.as_bytes().to_vec())) {
                continue;
            }
            let mut e = template;
            e.parent = parent;
            if template.size == 0 && !template.is_dir {
                e.size = ext.data_size.unwrap_or(0);
            }
            out.push(e, &n.name);
        }
    }
}

/// Streams the MFT as fixed-size chunks of whole records. Returns bytes read.
fn read_mft(
    vol: &Volume,
    runs: &[Run],
    mft_size: u64,
    chunk_bytes: usize,
    tx: &crossbeam_channel::Sender<Chunk>,
) -> io::Result<u64> {
    let rs = vol.ntfs.bytes_per_file_record_segment as u64;
    let cs = vol.ntfs.bytes_per_cluster as u64;
    let ss = vol.ntfs.bytes_per_sector as usize;
    let mut scratch = AlignedBuf::new(chunk_bytes, ss.max(4096));
    let mut pending: Vec<u8> = Vec::with_capacity(chunk_bytes * 2);
    let mut next_record = 0u64;
    let mut logical = 0u64;
    let mut bytes_read = 0u64;

    let send_full = |pending: &mut Vec<u8>, next_record: &mut u64, force: bool| -> io::Result<()> {
        while pending.len() >= chunk_bytes || (force && !pending.is_empty()) {
            let take = if pending.len() >= chunk_bytes {
                chunk_bytes
            } else {
                pending.len() - pending.len() % rs as usize
            };
            if take == 0 {
                pending.clear();
                break;
            }
            let rest = pending.split_off(take);
            let data = std::mem::replace(pending, rest);
            let count = (data.len() as u64) / rs;
            tx.send(Chunk { first_record: *next_record, data })
                .map_err(|_| io::Error::other("parser threads stopped"))?;
            *next_record += count;
        }
        Ok(())
    };

    for run in runs {
        if logical >= mft_size {
            break;
        }
        let take = (run.clusters * cs).min(mft_size - logical);
        match run.lcn {
            None => pending.resize(pending.len() + take as usize, 0), // sparse hole
            Some(lcn) => {
                let base = lcn as u64 * cs;
                let mut pos = 0u64;
                while pos < take {
                    let n = (chunk_bytes as u64).min(take - pos) as usize;
                    let read_len = round_up(n, ss);
                    let buf = &mut scratch.as_mut_slice()[..read_len];
                    vol.read_exact_at(base + pos, buf)?;
                    bytes_read += read_len as u64;
                    pending.extend_from_slice(&buf[..n]);
                    pos += n as u64;
                    send_full(&mut pending, &mut next_record, false)?;
                }
            }
        }
        logical += take;
    }
    send_full(&mut pending, &mut next_record, true)?;
    Ok(bytes_read)
}

fn parse_chunk(chunk: &mut Chunk, rs: usize) -> ChunkResult {
    let mut res = ChunkResult { first_record: chunk.first_record, ..Default::default() };
    res.scan.names.reserve(chunk.data.len() / rs * 24);
    for (i, rec) in chunk.data.chunks_exact_mut(rs).enumerate() {
        match parse::parse_record(chunk.first_record + i as u64, rec, false) {
            ParseOutcome::Record(info) if info.is_base() => {
                res.base_records += 1;
                if !push_base(&mut res.scan, &info) {
                    res.without_name += 1;
                }
            }
            ParseOutcome::Record(info) => res.extensions.push(info),
            ParseOutcome::Unused => res.unused += 1,
            ParseOutcome::Corrupt => res.corrupt += 1,
        }
    }
    res
}

/// Adds one entry per long name of a base record. Returns false if it has none.
fn push_base(scan: &mut MftScan, info: &RecordInfo) -> bool {
    let has_long_name = info.names.iter().any(|n| n.namespace != NS_DOS);
    let std = info.std.unwrap_or_default();
    let mut any = false;
    let mut seen: Vec<&FileName> = Vec::new();
    for n in &info.names {
        if has_long_name && n.namespace == NS_DOS {
            continue;
        }
        if seen.iter().any(|s| s.name == n.name && s.parent_ref == n.parent_ref) {
            continue;
        }
        seen.push(n);
        any = true;
        scan.push(
            MftEntry {
                record: info.index,
                seq: info.seq,
                parent: n.parent_ref & FRN_MASK,
                is_dir: info.is_dir(),
                attributes: std.attributes,
                size: if info.is_dir() { 0 } else { info.data_size.unwrap_or(0) },
                modified: std.modified,
                ..Default::default()
            },
            &n.name,
        );
    }
    any
}

fn round_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::NS_WIN32;

    fn name(parent: u64, ns: u8, s: &str) -> FileName {
        FileName { parent_ref: (1 << 48) | parent, namespace: ns, name: s.into() }
    }

    #[test]
    fn base_record_skips_dos_names_and_duplicates() {
        let info = RecordInfo {
            index: 40,
            base: 40,
            seq: 2,
            names: vec![
                name(5, NS_DOS, "RAPORT~1.PDF"),
                name(5, NS_WIN32, "Raport.pdf"),
                name(9, NS_WIN32, "Raport.pdf"),
                name(9, NS_WIN32, "Raport.pdf"),
            ],
            data_size: Some(10),
            ..Default::default()
        };
        let mut scan = MftScan::default();
        assert!(push_base(&mut scan, &info));
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.name(&scan.entries[1]), "Raport.pdf");
        assert_eq!(scan.entries[1].parent, 9);
        assert_eq!(scan.entries[0].size, 10);
    }

    #[test]
    fn extension_names_become_hard_links_of_the_base() {
        let mut scan = MftScan::default();
        scan.push(MftEntry { record: 7, parent: 5, size: 99, ..Default::default() }, "a.dll");
        let ext = RecordInfo {
            index: 300,
            base: 7,
            names: vec![name(5, NS_WIN32, "a.dll"), name(12, NS_WIN32, "a.dll")],
            ..Default::default()
        };
        merge_extensions(&mut scan, vec![ext]);
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.entries[1].parent, 12);
        assert_eq!(scan.entries[1].size, 99);
    }

    #[test]
    fn long_names_are_truncated_on_a_char_boundary() {
        let s = "ș".repeat(40_000); // 80.000 bytes
        let t = truncate_utf8(&s, u16::MAX as usize);
        assert!(t.len() <= u16::MAX as usize);
        assert!(t.chars().all(|c| c == 'ș'));
    }
}
