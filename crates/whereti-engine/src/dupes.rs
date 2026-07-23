//! `dupe:` (PLAN.md §63): identical files anywhere on the disks, the sets
//! that would free the most space first.
//!
//! 1. From the name index alone: the files matching the rest of the query
//!    (1 MB and up unless `size:` says otherwise; not cloud-only, hidden,
//!    system or program files unless `path:` points there), grouped by size.
//! 2. In each group of the same size, the files the caller may read are
//!    compared by content: first a quick hash of their start and end, then,
//!    for those still equal, a hash of the whole file (above 1 GB, of 64
//!    pieces spread over it). Only same hash, same size counts.
//!
//! Hashes are remembered (by path, size and date) for the next search.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use whereti_core::index::{FLAG_CLOUD, FLAG_HIDDEN, FLAG_NOISE, FLAG_SYSTEM};
use whereti_core::NameIndex;
use whereti_ipc::{Caller, Row};
use whereti_query::{Clause, Query, Term};

/// Smallest file looked at, unless the query has `size:`.
const MIN_SIZE: u64 = 1 << 20;
/// Bytes hashed from each end for the quick comparison.
const EDGE: u64 = 64 << 10;
/// Above this, the whole-file hash samples the file instead of reading it all.
const SAMPLED: u64 = 1 << 30;
const SAMPLES: u64 = 64;
/// Remembered hashes.
const KEEP: usize = 200_000;

/// The query asks for duplicates.
pub fn wanted(query: &Query) -> bool {
    query.clauses.iter().any(|c| !c.negated && c.any_of.iter().any(|t| matches!(t, Term::Dupe)))
}

/// Files of the same size matching the query, as groups of two or more,
/// the most space wasted first. Reads nothing from disk.
pub fn size_groups(idx: &NameIndex, query: &Query) -> Vec<Vec<Row>> {
    let mut rest = Query::default();
    let mut sized = false;
    let mut placed = false;
    for c in &query.clauses {
        if c.any_of.iter().any(|t| matches!(t, Term::Dupe)) {
            continue;
        }
        sized |= c.any_of.iter().any(|t| matches!(t, Term::Size { .. }));
        placed |= c.any_of.iter().any(|t| matches!(t, Term::Path(_)));
        rest.clauses.push(c.clone());
    }
    rest.clauses.push(Clause { negated: false, any_of: vec![Term::FilesOnly] });
    // never cloud-only files (reading them would download them)
    let skip = FLAG_CLOUD | if placed { 0 } else { FLAG_NOISE | FLAG_HIDDEN | FLAG_SYSTEM };
    let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
    for e in idx.matching(&rest) {
        let size = idx.size(e);
        if size == 0 || (!sized && size < MIN_SIZE) || idx.flags(e) & skip != 0 {
            continue;
        }
        by_size.entry(size).or_default().push(e);
    }
    let mut groups: Vec<(u64, Vec<u32>)> = by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    groups.sort_by_key(|(size, v)| std::cmp::Reverse(size.saturating_mul(v.len() as u64 - 1)));
    groups
        .into_iter()
        .map(|(size, entries)| {
            entries
                .into_iter()
                .map(|e| Row { path: idx.full_path(e), is_dir: false, size, modified: idx.modified(e), flags: idx.flags(e) })
                .collect()
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Hashes {
    size: u64,
    modified: u32,
    edges: Option<u64>,
    whole: Option<u64>,
}

static CACHE: Mutex<Option<HashMap<String, Hashes>>> = Mutex::new(None);

fn cached(row: &Row) -> Hashes {
    let mut c = CACHE.lock();
    let map = c.get_or_insert_with(HashMap::new);
    match map.get(&row.path) {
        Some(h) if h.size == row.size && h.modified == row.modified => *h,
        _ => Hashes { size: row.size, modified: row.modified, edges: None, whole: None },
    }
}

fn remember(path: &str, h: Hashes) {
    let mut c = CACHE.lock();
    let map = c.get_or_insert_with(HashMap::new);
    if map.len() >= KEEP {
        map.clear();
    }
    map.insert(path.to_string(), h);
}

fn open(path: &str) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).share_mode(7).open(path).ok()
}

/// The first and last `EDGE` bytes.
fn edges_hash(path: &str, size: u64) -> Option<u64> {
    let mut f = open(path)?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; EDGE.min(size) as usize];
    f.read_exact(&mut buf).ok()?;
    h.update(&buf);
    if size > EDGE {
        let tail = EDGE.min(size - EDGE);
        f.seek(SeekFrom::Start(size - tail)).ok()?;
        buf.resize(tail as usize, 0);
        f.read_exact(&mut buf).ok()?;
        h.update(&buf);
    }
    Some(h.digest())
}

/// The whole file; above `SAMPLED`, `SAMPLES` pieces spread over it.
fn whole_hash(path: &str, size: u64) -> Option<u64> {
    let mut f = open(path)?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    if size > SAMPLED {
        let piece = EDGE as usize;
        for i in 0..SAMPLES {
            f.seek(SeekFrom::Start((size - EDGE) / (SAMPLES - 1) * i)).ok()?;
            f.read_exact(&mut buf[..piece]).ok()?;
            h.update(&buf[..piece]);
        }
    } else {
        let mut left = size;
        while left > 0 {
            let n = f.read(&mut buf).ok()?;
            if n == 0 {
                return None; // shorter than the index says: changed meanwhile
            }
            h.update(&buf[..n]);
            left = left.saturating_sub(n as u64);
        }
    }
    Some(h.digest())
}

/// Splits `rows` by a hash; files that cannot be read drop out.
fn split_by(rows: Vec<Row>, hash: impl Fn(&Row) -> Option<u64>) -> Vec<Vec<Row>> {
    let mut by: HashMap<u64, Vec<Row>> = HashMap::new();
    for r in rows {
        if let Some(h) = hash(&r) {
            by.entry(h).or_default().push(r);
        }
    }
    by.into_values().filter(|g| g.len() > 1).collect()
}

/// What `verify` found: identical files, group after group.
pub struct Found {
    pub groups: Vec<Vec<Row>>,
    /// Same-size groups looked at, of `total`.
    pub checked: usize,
    pub total: usize,
}

/// Compares the files of each same-size group, the largest first, until
/// `budget` is spent or `limit` files are found. Only files the caller may
/// read are opened.
pub fn verify(candidates: Vec<Vec<Row>>, budget: Duration, limit: usize, caller: &dyn Caller) -> Found {
    let t = Instant::now();
    let total = candidates.len();
    let mut groups = Vec::new();
    let mut files = 0;
    let mut checked = 0;
    for group in candidates {
        if (t.elapsed() >= budget && checked > 0) || files >= limit {
            break;
        }
        checked += 1;
        let paths: Vec<String> = group.iter().map(|r| r.path.clone()).collect();
        let readable: Vec<Row> = group.into_iter().zip(caller.can_read(&paths)).filter(|(_, ok)| *ok).map(|(r, _)| r).collect();
        if readable.len() < 2 {
            continue;
        }
        let edges = |r: &Row| {
            let mut h = cached(r);
            if h.edges.is_none() {
                h.edges = edges_hash(&r.path, r.size);
                remember(&r.path, h);
            }
            h.edges
        };
        let whole = |r: &Row| {
            let mut h = cached(r);
            if h.whole.is_none() {
                h.whole = whole_hash(&r.path, r.size);
                remember(&r.path, h);
            }
            h.whole
        };
        for same_edges in split_by(readable, edges) {
            // a file of at most 2 x EDGE was read whole already
            let same = if same_edges[0].size <= 2 * EDGE { vec![same_edges] } else { split_by(same_edges, whole) };
            for mut g in same {
                g.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.path.cmp(&b.path)));
                files += g.len();
                groups.push(g);
            }
        }
    }
    Found { groups, checked, total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whereti_ipc::Trusted;

    #[test]
    fn identical_files_are_grouped_and_lookalikes_are_not() {
        let dir = std::env::temp_dir().join(format!("whereti-dupes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut middle = big.clone();
        middle[150_000] ^= 1; // same size, same start and end, one byte different
        let write = |name: &str, data: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            Row { path: p.display().to_string(), is_dir: false, size: data.len() as u64, modified: 1, flags: 0 }
        };
        let a = write("a.bin", &big);
        let b = write("b.bin", &big);
        let c = write("c.bin", &middle);
        let found = verify(vec![vec![a.clone(), b.clone(), c]], Duration::from_secs(10), 100, &Trusted);
        assert_eq!(found.groups.len(), 1);
        let mut paths: Vec<&str> = found.groups[0].iter().map(|r| r.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec![a.path.as_str(), b.path.as_str()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
