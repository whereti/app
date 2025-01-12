//! The compact name index (PLAN.md §4.2): one column per field, no full paths.

/// Folder.
pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_HIDDEN: u16 = 1 << 1;
pub const FLAG_SYSTEM: u16 = 1 << 2;
/// The name has non-ASCII characters; its folded form lives in the `alt` table.
pub const FLAG_NON_ASCII: u16 = 1 << 3;
/// OneDrive/Dropbox placeholder: content is in the cloud only.
pub const FLAG_CLOUD: u16 = 1 << 4;
/// Volume root (`C:`). Never returned as a search result.
pub const FLAG_ROOT: u16 = 1 << 5;
/// Inside a cache, build, package or system folder (AppData, node_modules, .git…).
/// Still searchable, but ranked after everything else.
pub const FLAG_NOISE: u16 = 1 << 6;

pub const NO_PARENT: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VolumeInfo {
    /// Drive letter as a Unicode scalar.
    pub letter: u32,
    pub root: u32,
    /// Entries of this volume are `first..end`.
    pub first: u32,
    pub end: u32,
    pub serial: u64,
    pub journal_id: u64,
    /// Journal position read just before the scan; replay starts here.
    pub next_usn: i64,
    /// This volume's slice of [`NameIndex::rec_map`].
    pub rec_start: u64,
    pub rec_len: u64,
}

impl VolumeInfo {
    pub fn letter(&self) -> char {
        char::from_u32(self.letter).unwrap_or('?')
    }
}

/// All indexed names of all volumes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NameIndex {
    pub(crate) volumes: Vec<VolumeInfo>,
    /// Original names, UTF-8, each followed by a 0 byte (a search can never
    /// match across two names).
    pub(crate) names: Vec<u8>,
    /// Start of each name in `names`; one extra element at the end.
    pub(crate) name_off: Vec<u32>,
    pub(crate) parent: Vec<u32>,
    pub(crate) flags: Vec<u16>,
    pub(crate) ext: Vec<u16>,
    pub(crate) size: Vec<u64>,
    /// Last write, minutes since 1601-01-01 UTC.
    pub(crate) mtime: Vec<u32>,
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
    /// MFT record number -> entry, per volume (for live updates).
    pub(crate) rec_map: Vec<u32>,
}

impl NameIndex {
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn volumes(&self) -> &[VolumeInfo] {
        &self.volumes
    }

    pub fn name(&self, i: u32) -> &str {
        let b = self.name_bytes(i);
        std::str::from_utf8(b).unwrap_or("")
    }

    pub(crate) fn name_bytes(&self, i: u32) -> &[u8] {
        let s = self.name_off[i as usize] as usize;
        let e = self.name_off[i as usize + 1] as usize - 1;
        &self.names[s..e]
    }

    /// Folded name of a non-ASCII entry.
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
        let k = self.volumes.partition_point(|v| v.end <= i);
        &self.volumes[k.min(self.volumes.len() - 1)]
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
            + v(&self.name_rank)
            + v(&self.alt_names)
            + v(&self.alt_off)
            + v(&self.alt_entry)
            + self.exts.iter().map(|e| e.len() + 24).sum::<usize>()
            + v(&self.dir_order)
            + v(&self.rec_map)
    }
}
