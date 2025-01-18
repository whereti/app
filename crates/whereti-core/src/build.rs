//! Builds the compact index from MFT scans, applying exclusions (PLAN.md §5).

use std::collections::HashMap;

use rayon::prelude::*;
use whereti_ntfs::mft::MftScan;
use whereti_query::fold;

use crate::index::*;

/// File attributes that mark cloud-only placeholders.
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;

/// NTFS keeps its own metadata in records 0-15 ($MFT, $LogFile, $Extend, …).
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = 5;

pub struct VolumeScan {
    pub letter: char,
    pub serial: u64,
    pub journal_id: u64,
    /// Journal position read before the scan started.
    pub next_usn: i64,
    pub scan: MftScan,
}

/// Paths left out of the name index. `C:\Windows` applies to drive C only;
/// `\System Volume Information` (no letter) applies to every volume.
#[derive(Debug, Clone, Default)]
pub struct Exclusions {
    pub paths: Vec<String>,
}

impl Exclusions {
    /// The defaults from PLAN.md §5. `windows_dir` is `%SystemRoot%`
    /// (e.g. `C:\Windows`); pass `None` to keep it in the index.
    pub fn defaults(windows_dir: Option<&str>) -> Self {
        let mut paths: Vec<String> = [
            r"\$Recycle.Bin",
            r"\System Volume Information",
            r"\$WinREAgent",
            r"\pagefile.sys",
            r"\hiberfil.sys",
            r"\swapfile.sys",
            r"\ProgramData\Microsoft\Windows\WER",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(w) = windows_dir {
            paths.push(w.to_string());
        }
        Exclusions { paths }
    }

    /// Rules for one volume, each as folded path components.
    pub(crate) fn for_volume(&self, letter: char) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for p in &self.paths {
            let p = p.replace('/', "\\");
            let rest = match p.as_bytes() {
                [l, b':', b'\\', ..] if (*l as char).eq_ignore_ascii_case(&letter) => &p[3..],
                [_, b':', ..] => continue,
                [b'\\', ..] => &p[1..],
                _ => continue,
            };
            let comps: Vec<String> = rest.split('\\').filter(|c| !c.is_empty()).map(fold).collect();
            if !comps.is_empty() {
                out.push(comps);
            }
        }
        out
    }
}

#[derive(Debug, Default, Clone)]
pub struct BuildStats {
    pub scanned: usize,
    pub excluded: usize,
    pub kept: usize,
}

pub fn build(volumes: Vec<VolumeScan>, exclusions: &Exclusions) -> (NameIndex, BuildStats) {
    let mut idx = NameIndex {
        exts: vec![String::new()],
        name_off: vec![0],
        alt_off: vec![0],
        exclusions: exclusions.paths.clone(),
        ..Default::default()
    };
    let mut ext_ids: HashMap<String, u16> = HashMap::new();
    let mut stats = BuildStats::default();
    for vs in volumes {
        add_volume(&mut idx, &mut ext_ids, vs, exclusions, &mut stats);
    }
    idx.name_rank = compute_name_rank(&idx);
    idx.dir_order = compute_dir_order(&idx);
    mark_noise(&mut idx);
    stats.kept = idx.len();
    (idx, stats)
}

fn add_volume(
    idx: &mut NameIndex,
    ext_ids: &mut HashMap<String, u16>,
    vs: VolumeScan,
    exclusions: &Exclusions,
    stats: &mut BuildStats,
) {
    let scan = &vs.scan;
    let n = scan.entries.len();
    stats.scanned += n;
    let max_rec = scan.entries.iter().map(|e| e.record + 1).max().unwrap_or(0) as usize;

    // record -> first entry with that record (folders have exactly one)
    let mut rec_first = vec![NO_PARENT; max_rec];
    for (i, e) in scan.entries.iter().enumerate() {
        if rec_first[e.record as usize] == NO_PARENT {
            rec_first[e.record as usize] = i as u32;
        }
    }
    let parent_of = |i: usize| -> u32 {
        let p = scan.entries[i].parent as usize;
        if p < max_rec { rec_first[p] } else { NO_PARENT }
    };
    let root = if (ROOT_RECORD as usize) < max_rec { rec_first[ROOT_RECORD as usize] } else { NO_PARENT };

    // 1. Seeds: NTFS metadata files and resolved exclusion paths.
    let mut state = vec![0u8; n]; // 0 = unknown, 1 = excluded, 2 = kept
    for (i, e) in scan.entries.iter().enumerate() {
        if e.record < FIRST_USER_RECORD && e.record != ROOT_RECORD {
            state[i] = 1;
        }
    }
    if root != NO_PARENT {
        state[root as usize] = 2;
        for i in resolve_paths(scan, root, &vs_rules(exclusions, vs.letter), &parent_of) {
            state[i as usize] = 1;
        }
    }
    // 2. Every entry inherits the state of its nearest decided ancestor.
    let mut chain = Vec::new();
    for i in 0..n {
        if state[i] != 0 {
            continue;
        }
        chain.clear();
        let mut cur = i as u32;
        let decided = loop {
            if state[cur as usize] != 0 {
                break state[cur as usize];
            }
            chain.push(cur);
            let p = parent_of(cur as usize);
            if p == NO_PARENT || p == cur || chain.len() > 4096 {
                break 2; // orphan or cycle: keep it
            }
            cur = p;
        };
        for &c in &chain {
            state[c as usize] = decided;
        }
    }

    // 3. Copy kept entries into the index columns.
    let base = idx.len() as u32;
    let mut new_idx = vec![NO_PARENT; n];
    let mut next = base;
    for i in 0..n {
        if state[i] == 2 {
            new_idx[i] = next;
            next += 1;
        } else {
            stats.excluded += 1;
        }
    }
    let vol_no = idx.volumes.len() as u8;
    let mut folded = String::new();
    for i in 0..n {
        if state[i] != 2 {
            continue;
        }
        let e = &scan.entries[i];
        let name = scan.name(e);
        let is_root = i as u32 == root;
        let mut flags = attr_flags(e.attributes, e.is_dir || is_root);
        if is_root {
            flags |= FLAG_ROOT;
        }
        if !name.is_ascii() {
            flags |= FLAG_NON_ASCII;
            folded.clear();
            whereti_query::fold_into(name, &mut folded);
            idx.alt_entry.push(new_idx[i]);
            idx.alt_names.extend_from_slice(folded.as_bytes());
            idx.alt_names.push(0);
            idx.alt_off.push(idx.alt_names.len() as u32);
        }
        idx.names.extend_from_slice(name.as_bytes());
        idx.names.push(0);
        idx.name_off.push(idx.names.len() as u32);
        let new_root = if root == NO_PARENT { NO_PARENT } else { new_idx[root as usize] };
        let p = parent_of(i);
        idx.parent.push(if is_root {
            NO_PARENT
        } else if p == NO_PARENT || new_idx[p as usize] == NO_PARENT {
            new_root // orphan: show it at the volume root
        } else {
            new_idx[p as usize]
        });
        idx.flags.push(flags);
        idx.ext.push(if e.is_dir { 0 } else { ext_id(ext_ids, &mut idx.exts, name) });
        idx.size.push(e.size);
        idx.mtime.push(filetime_to_minutes(e.modified));
        idx.volume.push(vol_no);
    }

    // record -> kept entries (a hard link can be kept while another is excluded)
    let mut rec_map = vec![NO_PARENT; max_rec];
    for i in 0..n {
        if state[i] != 2 {
            continue;
        }
        let r = scan.entries[i].record as usize;
        if rec_map[r] == NO_PARENT {
            rec_map[r] = new_idx[i];
        } else {
            idx.links.entry(rec_map[r]).or_default().push(new_idx[i]);
        }
    }
    idx.rec_maps.push(rec_map);
    idx.volumes.push(VolumeInfo {
        letter: vs.letter as u32,
        root: if root == NO_PARENT { NO_PARENT } else { new_idx[root as usize] },
        first: base,
        end: next,
        serial: vs.serial,
        journal_id: vs.journal_id,
        next_usn: vs.next_usn,
        // where this volume's record map starts when the maps are stored back to back
        rec_start: idx.rec_maps.iter().rev().skip(1).map(|m| m.len() as u64).sum(),
        rec_len: max_rec as u64,
    });
}

fn vs_rules(exclusions: &Exclusions, letter: char) -> Vec<Vec<String>> {
    exclusions.for_volume(letter)
}

/// Finds the entries named by exclusion rules, one tree level per pass.
fn resolve_paths(
    scan: &MftScan,
    root: u32,
    rules: &[Vec<String>],
    parent_of: &dyn Fn(usize) -> u32,
) -> Vec<u32> {
    let mut found = Vec::new();
    // (rule, depth reached) -> entry matched so far
    let mut frontier: Vec<(usize, u32)> = (0..rules.len()).map(|r| (r, root)).collect();
    let mut depth = 0;
    while !frontier.is_empty() {
        let mut wanted: HashMap<u32, Vec<usize>> = HashMap::new();
        for &(r, at) in &frontier {
            wanted.entry(at).or_default().push(r);
        }
        let mut next = Vec::new();
        let mut folded = String::new();
        for (i, e) in scan.entries.iter().enumerate() {
            let Some(rs) = wanted.get(&parent_of(i)) else { continue };
            folded.clear();
            whereti_query::fold_into(scan.name(e), &mut folded);
            for &r in rs {
                if rules[r][depth] == folded {
                    if depth + 1 == rules[r].len() {
                        found.push(i as u32);
                    } else if e.is_dir {
                        next.push((r, i as u32));
                    }
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    found
}

/// Index flags for NTFS file attributes.
pub(crate) fn attr_flags(attributes: u32, is_dir: bool) -> u16 {
    let mut flags = 0u16;
    if is_dir {
        flags |= FLAG_DIR;
    }
    if attributes & ATTR_HIDDEN != 0 {
        flags |= FLAG_HIDDEN;
    }
    if attributes & ATTR_SYSTEM != 0 {
        flags |= FLAG_SYSTEM;
    }
    if attributes & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
        flags |= FLAG_CLOUD;
    }
    flags
}

pub(crate) fn ext_id(ids: &mut HashMap<String, u16>, exts: &mut Vec<String>, name: &str) -> u16 {
    let Some(dot) = name.rfind('.') else { return 0 };
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > 16 || ext.contains(' ') {
        return 0;
    }
    let ext = fold(ext);
    if let Some(&id) = ids.get(&ext) {
        return id;
    }
    if exts.len() >= u16::MAX as usize {
        return 0;
    }
    let id = exts.len() as u16;
    exts.push(ext.clone());
    ids.insert(ext, id);
    id
}

/// FILETIME (100 ns since 1601) -> minutes since 1601.
pub fn filetime_to_minutes(ft: u64) -> u32 {
    (ft / 600_000_000).min(u32::MAX as u64) as u32
}

/// Case- and accent-insensitive sort key of an entry.
pub(crate) fn sort_key(idx: &NameIndex, i: u32) -> &[u8] {
    if idx.flags[i as usize] & FLAG_NON_ASCII != 0 {
        idx.alt_name(i)
    } else {
        idx.name_bytes(i)
    }
}

pub(crate) fn compute_name_rank(idx: &NameIndex) -> Vec<u32> {
    let mut order: Vec<u32> = (0..idx.len() as u32).collect();
    order.par_sort_unstable_by(|&a, &b| {
        let (ka, kb) = (sort_key(idx, a), sort_key(idx, b));
        ka.iter()
            .map(u8::to_ascii_lowercase)
            .cmp(kb.iter().map(u8::to_ascii_lowercase))
            .then_with(|| idx.name_bytes(a).cmp(idx.name_bytes(b)))
            .then(a.cmp(&b))
    });
    let mut rank = vec![0u32; idx.len()];
    for (pos, &i) in order.iter().enumerate() {
        rank[i as usize] = pos as u32;
    }
    rank
}

/// Folders sorted by depth, so parents always come first.
pub(crate) fn compute_dir_order(idx: &NameIndex) -> Vec<u32> {
    let n = idx.len();
    let mut depth = vec![u16::MAX; n];
    let mut chain = Vec::new();
    for i in 0..n as u32 {
        if !idx.is_dir(i) || depth[i as usize] != u16::MAX {
            continue;
        }
        chain.clear();
        let mut cur = i;
        let mut d = loop {
            if depth[cur as usize] != u16::MAX {
                break depth[cur as usize];
            }
            chain.push(cur);
            match idx.parent(cur) {
                Some(p) if chain.len() < 4096 => cur = p,
                _ => break 0,
            }
        };
        for &c in chain.iter().rev() {
            d = d.saturating_add(1);
            depth[c as usize] = d;
        }
    }
    let mut dirs: Vec<u32> = (0..n as u32).filter(|&i| idx.is_dir(i)).collect();
    dirs.sort_by_key(|&i| (depth[i as usize], i));
    dirs
}

/// Folder names whose contents users rarely look for (PLAN.md §4.3 ranking).
const NOISE_DIRS: &[&str] = &[
    "appdata", "programdata", "program files", "program files (x86)", "node_modules",
    ".git", ".svn", ".hg", ".cache", "cache", "caches", "temp", "tmp", ".tmp",
    "__pycache__", "target", ".venv", "venv", "site-packages", ".cargo", ".rustup",
    ".npm", ".yarn", ".pnpm-store", ".gradle", ".m2", ".nuget", ".vs", ".idea",
    "obj", ".turbo", ".next", ".codex", ".vscode-server",
];

pub(crate) fn is_noise_dir(folded_name: &str) -> bool {
    NOISE_DIRS.contains(&folded_name)
}

/// Sets FLAG_NOISE on noise folders and everything below them.
fn mark_noise(idx: &mut NameIndex) {
    let mut folded = String::new();
    for k in 0..idx.dir_order.len() {
        let d = idx.dir_order[k];
        let parent_noise = idx.parent(d).is_some_and(|p| idx.flags[p as usize] & FLAG_NOISE != 0);
        folded.clear();
        whereti_query::fold_into(idx.name(d), &mut folded);
        if parent_noise || is_noise_dir(&folded) {
            idx.flags[d as usize] |= FLAG_NOISE;
        }
    }
    for i in 0..idx.len() {
        if idx.flags[i] & FLAG_DIR == 0 {
            if let Some(p) = idx.parent(i as u32) {
                if idx.flags[p as usize] & FLAG_NOISE != 0 {
                    idx.flags[i] |= FLAG_NOISE;
                }
            }
        }
    }
}
