//! `dupe:` (PLAN.md §63): identical files anywhere on the disks, the sets
//! that would free the most space first.
//!
//! 1. From the name index alone: the files matching the rest of the query
//!    (1 MB and up unless `size:` says otherwise; not cloud-only, hidden,
//!    system or program files unless `path:` points there), grouped by
//!    size. Hard links (one file, several names) count once.
//! 2. In each group of the same size, the files the caller may read are
//!    compared by content: first a quick hash of their start and end, then,
//!    for those still equal, a hash of the WHOLE file. A set is shown only
//!    when every byte matched: people delete the "copies" they are shown.
//!
//! Hashing a large file can take longer than one search: its progress is
//! remembered (by path, size and date), so the next search goes on from
//! there, and sets not proven yet are simply not shown yet.

use std::collections::{HashMap, HashSet};
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
/// Remembered hashes.
const KEEP: usize = 200_000;

/// The query asks for duplicates.
pub fn wanted(query: &Query) -> bool {
    query.clauses.iter().any(|c| !c.negated && c.any_of.iter().any(|t| matches!(t, Term::Dupe)))
}

/// What `size_groups` could not apply.
pub fn unsupported(query: &Query) -> bool {
    query.clauses.iter().any(|c| c.any_of.iter().any(|t| matches!(t, Term::Content(_) | Term::Place(_))))
}

/// Files of the same size matching the query, as groups of two or more,
/// the most space wasted first. Reads nothing from disk. `within`: only
/// these entries (from property conditions such as `camera:`).
pub fn size_groups(idx: &NameIndex, query: &Query, within: Option<&HashSet<u32>>) -> Vec<Vec<Row>> {
    let mut rest = Query::default();
    let mut sized = false;
    let mut placed = false;
    for c in &query.clauses {
        if c.any_of.iter().any(|t| matches!(t, Term::Dupe | Term::Prop(_))) {
            continue;
        }
        sized |= !c.negated && c.any_of.iter().any(|t| matches!(t, Term::Size { .. }));
        // `!path:x` does not point anywhere: only a positive path opens up hidden places
        placed |= !c.negated && c.any_of.iter().any(|t| matches!(t, Term::Path(_)));
        rest.clauses.push(c.clone());
    }
    rest.clauses.push(Clause { negated: false, any_of: vec![Term::FilesOnly] });
    // never cloud-only files (reading them would download them)
    let skip = FLAG_CLOUD | if placed { 0 } else { FLAG_NOISE | FLAG_HIDDEN | FLAG_SYSTEM };
    let entries: Vec<u32> = idx
        .matching(&rest)
        .into_iter()
        .filter(|e| within.is_none_or(|w| w.contains(e)))
        .filter(|&e| {
            let size = idx.size(e);
            size > 0 && (sized || size >= MIN_SIZE) && idx.flags(e) & skip == 0
        })
        .collect();
    // one file with several names (hard links) is not a copy
    let records = idx.records_of(&entries);
    let mut files: HashSet<(char, u64)> = HashSet::new();
    let mut by_size: HashMap<u64, Vec<u32>> = HashMap::new();
    for (e, r) in entries.into_iter().zip(records) {
        if let Some(record) = r {
            if !files.insert((idx.volume_of(e).letter(), record)) {
                continue;
            }
        }
        by_size.entry(idx.size(e)).or_default().push(e);
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

#[derive(Clone)]
struct Hashes {
    size: u64,
    modified: u32,
    edges: Option<u64>,
    whole: Option<u64>,
    /// A whole-file hash under way: bytes done, and the hasher's state.
    partial: Option<(u64, Box<xxhash_rust::xxh3::Xxh3>)>,
}

static CACHE: Mutex<Option<HashMap<String, Hashes>>> = Mutex::new(None);

fn cached(row: &Row) -> Hashes {
    let mut c = CACHE.lock();
    let map = c.get_or_insert_with(HashMap::new);
    match map.get(&row.path) {
        Some(h) if h.size == row.size && h.modified == row.modified => h.clone(),
        _ => Hashes { size: row.size, modified: row.modified, edges: None, whole: None, partial: None },
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
    // only at its real path: no link swapped in after the access check
    whereti_extract::worker::open_real(std::path::Path::new(path)).ok()
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

enum Whole {
    Done(u64),
    /// Out of time: the progress is kept for the next search.
    NotYet,
    Unreadable,
}

/// Every byte of the file, from where an earlier search stopped, until
/// `deadline`.
fn whole_hash(row: &Row, deadline: Instant) -> Whole {
    let mut h = cached(row);
    if let Some(w) = h.whole {
        return Whole::Done(w);
    }
    let Some(mut f) = open(&row.path) else { return Whole::Unreadable };
    let (mut done, mut hasher) = h.partial.take().unwrap_or_else(|| (0, Box::new(xxhash_rust::xxh3::Xxh3::new())));
    if f.seek(SeekFrom::Start(done)).is_err() {
        return Whole::Unreadable;
    }
    let mut buf = vec![0u8; 1 << 20];
    while done < row.size {
        if Instant::now() >= deadline {
            h.partial = Some((done, hasher));
            remember(&row.path, h);
            return Whole::NotYet;
        }
        let n = match f.read(&mut buf) {
            Ok(0) | Err(_) => return Whole::Unreadable, // shorter than the index says: changed meanwhile
            Ok(n) => n,
        };
        hasher.update(&buf[..n]);
        done += n as u64;
    }
    let digest = hasher.digest();
    h.whole = Some(digest);
    remember(&row.path, h);
    Whole::Done(digest)
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
    /// It stopped because the time ran out (not because enough were found).
    pub out_of_time: bool,
}

/// Compares the files of each same-size group, the largest first, until
/// `budget` is spent or `limit` files are found. Only files the caller may
/// read are opened.
pub fn verify(candidates: Vec<Vec<Row>>, budget: Duration, limit: usize, caller: &dyn Caller) -> Found {
    let deadline = Instant::now() + budget;
    let total = candidates.len();
    let mut groups = Vec::new();
    let mut files = 0;
    let mut checked = 0;
    let mut out_of_time = false;
    'groups: for group in candidates {
        if files >= limit {
            break;
        }
        if Instant::now() >= deadline && checked > 0 {
            out_of_time = true;
            break;
        }
        let paths: Vec<String> = group.iter().map(|r| r.path.clone()).collect();
        let readable: Vec<Row> = group.into_iter().zip(caller.can_read(&paths)).filter(|(_, ok)| *ok).map(|(r, _)| r).collect();
        if readable.len() < 2 {
            checked += 1;
            continue;
        }
        let edges = |r: &Row| {
            let mut h = cached(r);
            if h.edges.is_none() {
                h.edges = edges_hash(&r.path, r.size);
                remember(&r.path, h.clone());
            }
            h.edges
        };
        for same_edges in split_by(readable, edges) {
            // a file of at most 2 x EDGE was read whole already
            let same = if same_edges[0].size <= 2 * EDGE {
                vec![same_edges]
            } else {
                let mut by: HashMap<u64, Vec<Row>> = HashMap::new();
                for r in same_edges {
                    match whole_hash(&r, deadline) {
                        Whole::Done(h) => by.entry(h).or_default().push(r),
                        Whole::Unreadable => {}
                        Whole::NotYet => {
                            out_of_time = true;
                            break 'groups;
                        }
                    }
                }
                by.into_values().filter(|g| g.len() > 1).collect()
            };
            for mut g in same {
                g.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.path.cmp(&b.path)));
                files += g.len();
                groups.push(g);
            }
        }
        checked += 1;
    }
    Found { groups, checked, total, out_of_time }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whereti_ipc::Trusted;

    fn write(dir: &std::path::Path, name: &str, data: &[u8]) -> Row {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        Row { path: p.display().to_string(), is_dir: false, size: data.len() as u64, modified: 1, flags: 0 }
    }

    #[test]
    fn identical_files_are_grouped_and_lookalikes_are_not() {
        let dir = std::env::temp_dir().join(format!("whereti-dupes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut middle = big.clone();
        middle[150_000] ^= 1; // same size, same start and end, one byte different
        let a = write(&dir, "a.bin", &big);
        let b = write(&dir, "b.bin", &big);
        let c = write(&dir, "c.bin", &middle);
        let found = verify(vec![vec![a.clone(), b.clone(), c]], Duration::from_secs(10), 100, &Trusted);
        assert_eq!(found.groups.len(), 1);
        let mut paths: Vec<&str> = found.groups[0].iter().map(|r| r.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec![a.path.as_str(), b.path.as_str()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hash_out_of_time_goes_on_in_the_next_search() {
        let dir = std::env::temp_dir().join(format!("whereti-dupes-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
        let a = write(&dir, "a.bin", &data);
        let b = write(&dir, "b.bin", &data);
        // no time at all: nothing proven, nothing shown
        let first = verify(vec![vec![a.clone(), b.clone()]], Duration::ZERO, 100, &Trusted);
        assert!(first.groups.is_empty());
        // enough time later: proven
        let mut later = verify(vec![vec![a.clone(), b.clone()]], Duration::from_secs(10), 100, &Trusted);
        for _ in 0..20 {
            if !later.groups.is_empty() {
                break;
            }
            later = verify(vec![vec![a.clone(), b.clone()]], Duration::from_secs(10), 100, &Trusted);
        }
        assert_eq!(later.groups.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
