//! The compact name index (PLAN.md §4.2): one column per field, no full paths.
//!
//! A freshly built or loaded index is *compact*. Live updates (live.rs) change
//! columns in place, append new entries, keep renamed names in a small `tail`
//! list and mark removed entries instead of moving data. `compact()` rebuilds
//! a clean index when that overlay grows.

use std::collections::HashMap;

/// Folder.
pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_HIDDEN: u16 = 1 << 1;
pub const FLAG_SYSTEM: u16 = 1 << 2;
/// The base name has non-ASCII characters; its folded form lives in the `alt` table.
pub const FLAG_NON_ASCII: u16 = 1 << 3;
/// OneDrive/Dropbox placeholder: content is in the cloud only.
pub const FLAG_CLOUD: u16 = 1 << 4;
/// Volume root (`C:`). Never returned as a search result.
pub const FLAG_ROOT: u16 = 1 << 5;
/// Inside a cache, build, package or system folder (AppData, node_modules, .git…).
/// Still searchable, but ranked after everything else.
pub const FLAG_NOISE: u16 = 1 << 6;
/// The file no longer exists.
pub const FLAG_DELETED: u16 = 1 << 7;
/// Moved somewhere the index does not cover (Recycle Bin, an excluded folder).
/// Kept, so a restore brings the whole folder back.
pub const FLAG_DETACHED: u16 = 1 << 8;
/// Deleted, detached, or below a deleted/detached folder: not searchable.
pub const FLAG_GONE: u16 = 1 << 9;
/// Current name is in the `tail` list (renamed or added after the build).
pub const FLAG_TAIL: u16 = 1 << 10;
/// Inside one of the user's own folders (Desktop, Documents, Downloads,
/// Pictures, Videos, Music, OneDrive…): ranked before other places.
pub const FLAG_HOME: u16 = 1 << 11;

/// Entries never returned by a search.
pub(crate) const HIDDEN_FROM_SEARCH: u16 = FLAG_ROOT | FLAG_GONE;

pub const NO_PARENT: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VolumeInfo {
    /// Drive letter as a Unicode scalar.
    pub letter: u32,
    pub root: u32,
    /// Entries of this volume were `first..end` when the index was built.
    pub first: u32,
    pub end: u32,
    pub serial: u64,
    pub journal_id: u64,
    /// Journal position up to which the index is up to date.
    pub next_usn: i64,
    /// This volume's slice of the record map in the snapshot file.
    pub rec_start: u64,
    pub rec_len: u64,
}

impl VolumeInfo {
    pub fn letter(&self) -> char {
        char::from_u32(self.letter).unwrap_or('?')
    }
}

/// How many entries each exclusion rule left out at the last build, by rule
/// text. Kept in memory only (the service saves it beside the index); it
/// never makes two indexes different.
#[derive(Debug, Default, Clone)]
pub struct SkipCounts(pub Vec<(String, u64)>);

impl PartialEq for SkipCounts {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for SkipCounts {}

/// All indexed names of all volumes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NameIndex {
    pub(crate) volumes: Vec<VolumeInfo>,
    /// Original names, UTF-8, each followed by a 0 byte (a search can never
    /// match across two names).
    pub(crate) names: Vec<u8>,
    /// Start of each base name in `names`; one extra element at the end.
    /// Entries added after the build have no base name (FLAG_TAIL).
    pub(crate) name_off: Vec<u32>,
    pub(crate) parent: Vec<u32>,
    pub(crate) flags: Vec<u16>,
    pub(crate) ext: Vec<u16>,
    pub(crate) size: Vec<u64>,
    /// Last write, minutes since 1601-01-01 UTC.
    pub(crate) mtime: Vec<u32>,
    /// Index into `volumes`.
    pub(crate) volume: Vec<u8>,
    /// Position of each entry in case-insensitive name order.
    pub(crate) name_rank: Vec<u32>,
    /// Folded names of the FLAG_NON_ASCII entries, 0-separated, plus the
    /// entry each belongs to (sorted).
    pub(crate) alt_names: Vec<u8>,
    pub(crate) alt_off: Vec<u32>,
    pub(crate) alt_entry: Vec<u32>,
    /// Extension id -> folded extension. Id 0 = no extension.
    pub(crate) exts: Vec<String>,
    /// Folders ordered so that every parent comes before its children.
    pub(crate) dir_order: Vec<u32>,
    /// Per volume: MFT record number -> first entry with that record.
    pub(crate) rec_maps: Vec<Vec<u32>>,
    /// First entry -> the other entries of the same record (hard links).
    pub(crate) links: HashMap<u32, Vec<u32>>,
    /// Exclusion paths the index was built with.
    pub(crate) exclusions: Vec<String>,
    /// What each of them left out at the last build.
    pub(crate) skipped: SkipCounts,

    // ---- live overlay (empty in a compact index) ----
    /// Names of renamed / added entries.
    pub(crate) tail_names: Vec<String>,
    pub(crate) tail_entry: Vec<u32>,
    /// Entry -> its current position in `tail_names`.
    pub(crate) tail_of: HashMap<u32, u32>,
    /// Entries that became deleted or detached since the build.
    pub(crate) removed: usize,
}

impl NameIndex {
    /// Number of entries, including removed ones not yet compacted away.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Entries a search can return (excludes volume roots and removed files).
    pub fn live_len(&self) -> usize {
        self.flags.iter().filter(|&&f| f & HIDDEN_FROM_SEARCH == 0).count()
    }

    pub fn volumes(&self) -> &[VolumeInfo] {
        &self.volumes
    }

    pub fn exclusions(&self) -> &[String] {
        &self.exclusions
    }

    /// What each exclusion left out at the last build (empty for an index
    /// loaded from disk until `set_skipped`).
    pub fn skipped(&self) -> &[(String, u64)] {
        &self.skipped.0
    }

    /// The counts saved beside an index loaded from disk.
    pub fn set_skipped(&mut self, counts: Vec<(String, u64)>) {
        self.skipped = SkipCounts(counts);
    }

    /// True when there is no live-update overlay to fold in.
    pub fn is_compact(&self) -> bool {
        self.tail_names.is_empty() && self.removed == 0 && self.name_off.len() == self.len() + 1
    }

    /// Share of entries that are overlay or garbage; compact when this grows.
    pub fn overlay_ratio(&self) -> f64 {
        let base = self.name_off.len().saturating_sub(1);
        let extra = self.len() - base + self.tail_of.len() + self.removed;
        extra as f64 / self.len().max(1) as f64
    }

    pub fn name(&self, i: u32) -> &str {
        if self.flags[i as usize] & FLAG_TAIL != 0 {
            return self.tail_name(i);
        }
        std::str::from_utf8(self.base_name(i)).unwrap_or("")
    }

    /// Name bytes. Live-updated names come from the tail list.
    pub(crate) fn name_bytes(&self, i: u32) -> &[u8] {
        if self.flags[i as usize] & FLAG_TAIL != 0 {
            self.tail_name(i).as_bytes()
        } else {
            self.base_name(i)
        }
    }

    pub(crate) fn base_name(&self, i: u32) -> &[u8] {
        let s = self.name_off[i as usize] as usize;
        let e = self.name_off[i as usize + 1] as usize - 1;
        &self.names[s..e]
    }

    pub(crate) fn tail_name(&self, i: u32) -> &str {
        self.tail_of.get(&i).map(|&k| self.tail_names[k as usize].as_str()).unwrap_or("")
    }

    /// Folded base name of a non-ASCII entry.
    pub(crate) fn alt_name(&self, i: u32) -> &[u8] {
        match self.alt_entry.binary_search(&i) {
            Ok(k) => &self.alt_names[self.alt_off[k] as usize..self.alt_off[k + 1] as usize - 1],
            Err(_) => b"",
        }
    }

    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & FLAG_DIR != 0
    }

    pub fn flags(&self, i: u32) -> u16 {
        self.flags[i as usize]
    }

    pub fn size(&self, i: u32) -> u64 {
        self.size[i as usize]
    }

    /// Last write, minutes since 1601-01-01 UTC.
    pub fn modified(&self, i: u32) -> u32 {
        self.mtime[i as usize]
    }

    pub fn parent(&self, i: u32) -> Option<u32> {
        let p = self.parent[i as usize];
        (p != NO_PARENT).then_some(p)
    }

    pub fn volume_of(&self, i: u32) -> &VolumeInfo {
        &self.volumes[self.volume[i as usize] as usize]
    }

    /// `C:\Users\ana\Documents\raport.pdf`. Built on demand (only for
    /// displayed rows); the index never stores paths.
    pub fn full_path(&self, i: u32) -> String {
        let vol = self.volume_of(i);
        let mut parts = Vec::new();
        let mut cur = i;
        for _ in 0..4096 {
            if self.flags[cur as usize] & FLAG_ROOT != 0 {
                break;
            }
            parts.push(self.name(cur));
            match self.parent(cur) {
                Some(p) => cur = p,
                None => break,
            }
        }
        let mut out = format!("{}:", vol.letter());
        if parts.is_empty() {
            out.push('\\');
        }
        for p in parts.iter().rev() {
            out.push('\\');
            out.push_str(p);
        }
        out
    }

    /// Bytes held by the index columns.
    pub fn memory_bytes(&self) -> usize {
        fn v<T>(x: &[T]) -> usize {
            std::mem::size_of_val(x)
        }
        v(&self.volumes)
            + v(&self.names)
            + v(&self.name_off)
            + v(&self.parent)
            + v(&self.flags)
            + v(&self.ext)
            + v(&self.size)
            + v(&self.mtime)
            + v(&self.volume)
            + v(&self.name_rank)
            + v(&self.alt_names)
            + v(&self.alt_off)
            + v(&self.alt_entry)
            + self.exts.iter().map(|e| e.len() + 24).sum::<usize>()
            + v(&self.dir_order)
            + self.rec_maps.iter().map(|m| v(m)).sum::<usize>()
            + self.links.values().map(|l| 48 + v(l)).sum::<usize>()
            + self.tail_names.iter().map(|n| n.len() + 24 + 16).sum::<usize>()
    }
}
