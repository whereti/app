//! Direct `$MFT` reader: one pass gives names, parents, sizes, dates and
//! attributes. One thread reads large sequential chunks; worker threads parse
//! them in parallel. The channel is bounded, so memory stays flat however big
//! the MFT is.

use std::collections::HashMap;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::parse::{self, ParseOutcome, RecordInfo, Run, FRN_MASK, NS_DOS};
use crate::volume::{AlignedBuf, Volume};

const CHUNK_BYTES: usize = 4 << 20;

#[derive(Debug, Clone)]
pub struct MftEntry {
    pub record: u64,
    pub seq: u16,
    /// Parent record number (sequence stripped).
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    pub attributes: u32,
    /// Logical size of the unnamed data stream (0 for directories).
    pub size: u64,
    /// Last write time, FILETIME.
    pub modified: u64,
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

pub struct MftScan {
    pub entries: Vec<MftEntry>,
    pub stats: MftStats,
}

struct Chunk {
    first_record: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct ChunkResult {
    first_record: u64,
    records: Vec<RecordInfo>,
    corrupt: u64,
    unused: u64,
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
    let results: Vec<ChunkResult> = thread::scope(|s| -> io::Result<Vec<ChunkResult>> {
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

    // 3. Merge results in record order; fold extension records into their base.
    let mut results = results;
    results.sort_unstable_by_key(|r| r.first_record);
    let mut bases: Vec<RecordInfo> = Vec::new();
    let mut extensions: Vec<RecordInfo> = Vec::new();
    for r in results {
        stats.corrupt_records += r.corrupt;
        stats.unused_records += r.unused;
        for info in r.records {
            if info.is_base() {
                bases.push(info);
            } else {
                extensions.push(info);
            }
        }
    }
    stats.base_records = bases.len() as u64;
    stats.extension_records = extensions.len() as u64;
    stats.in_use = stats.base_records + stats.extension_records;

    if !extensions.is_empty() {
        let pos: HashMap<u64, usize> = bases.iter().enumerate().map(|(i, b)| (b.index, i)).collect();
        for ext in extensions {
            if let Some(&i) = pos.get(&ext.base) {
                let base = &mut bases[i];
                base.names.extend(ext.names);
                if base.data_size.is_none() {
                    base.data_size = ext.data_size;
                }
                if base.std.is_none() {
                    base.std = ext.std;
                }
            }
        }
    }

    // 4. One entry per name (hard links appear in every folder they live in).
    let mut entries = Vec::with_capacity(bases.len());
    for b in bases {
        let has_long_name = b.names.iter().any(|n| n.namespace != NS_DOS);
        let mut any = false;
        for n in &b.names {
            if has_long_name && n.namespace == NS_DOS {
                continue;
            }
            // The same link can be listed twice when it also appears in an
            // extension record; keep one copy.
            if entries.last().is_some_and(|e: &MftEntry| {
                e.record == b.index && e.name == n.name && e.parent == n.parent_ref & FRN_MASK
            }) {
                continue;
            }
            any = true;
            let std = b.std.unwrap_or_default();
            entries.push(MftEntry {
                record: b.index,
                seq: b.seq,
                parent: n.parent_ref & FRN_MASK,
                name: n.name.clone(),
                is_dir: b.is_dir(),
                attributes: std.attributes,
                size: if b.is_dir() { 0 } else { b.data_size.unwrap_or(0) },
                modified: std.modified,
            });
        }
        if !any {
            stats.records_without_name += 1;
        }
    }
    Ok(MftScan { entries, stats })
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
    for (i, rec) in chunk.data.chunks_exact_mut(rs).enumerate() {
        match parse::parse_record(chunk.first_record + i as u64, rec, false) {
            ParseOutcome::Record(info) => res.records.push(info),
            ParseOutcome::Unused => res.unused += 1,
            ParseOutcome::Corrupt => res.corrupt += 1,
        }
    }
    res
}

fn round_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
