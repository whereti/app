//! Live updates (PLAN.md §4.4, §7.2).
//!
//! The USN journal only tells us *which* MFT records changed. For each one we
//! read its *current* state (all names, parents, size, date) and make the index
//! match it. Applying a record twice, or applying records in a different order,
//! gives the same result, so missed or duplicated journal events cannot drift
//! the index away from the disk.
//!
//! - A file or folder moved somewhere the index does not cover (Recycle Bin,
//!   an excluded folder) is *detached*: hidden with everything under it, but
//!   kept, so a restore brings it all back.
//! - A folder that appears from outside the index (not created now, not
//!   detached before) may contain files we have never seen: the caller must
//!   rescan the volume (`ApplyStats::needs_rescan`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;

use whereti_ntfs::mft::{MftEntry, MftScan};
use whereti_query::fold;

use crate::build::{
    attr_flags, build, compute_dir_order, ext_id, filetime_to_minutes, place_flags, Exclusions, Smart, VolumeScan,
};
use crate::index::*;

const ROOT_RECORD: u64 = 5;
const FIRST_USER_RECORD: u64 = 16;
const KEEP_FLAGS: u16 = FLAG_ROOT | FLAG_NON_ASCII | FLAG_TAIL | FLAG_DELETED | FLAG_DETACHED | FLAG_GONE;
// FLAG_NOISE and FLAG_HOME are worked out again from the parent (place_flags)

/// `RecordState::size` when it could not be read.
pub const SIZE_UNKNOWN: u64 = u64::MAX;

/// Current on-disk state of one MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordState {
    pub is_dir: bool,
    pub attributes: u32,
    /// `SIZE_UNKNOWN` when the data's size is in a record that was not read
    /// (a very fragmented file): the size known so far is kept.
    pub size: u64,
    /// Last write, FILETIME.
    pub modified: u64,
    /// (parent record, name) of every hard link.
    pub links: Vec<(u64, String)>,
    /// False when some names may be stored in extension records that were not
    /// read; links missing from `links` are then kept instead of removed.
    pub complete: bool,
}

/// Where current record states come from: the volume (FSCTL_GET_NTFS_FILE_RECORD)
/// in production, a model file system in tests.
pub trait RecordSource {
    /// Current state of `record`, or `None` if the record is not in use.
    fn record(&mut self, record: u64) -> io::Result<Option<RecordState>>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyStats {
    pub records: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// A folder came in from outside the index; rescan the volume.
    pub needs_rescan: bool,
}

#[derive(Default)]
struct Pass {
    /// Folder deleted, detached, reattached or turned into a file.
    structural: bool,
    /// Folder moved or created out of parent-first order.
    dirs_reordered: bool,
    touched: Vec<u32>,
}

impl NameIndex {
    pub fn volume_index(&self, letter: char) -> Option<usize> {
        self.volumes.iter().position(|v| v.letter().eq_ignore_ascii_case(&letter))
    }

    /// Records how far the index is up to date with the volume's journal.
    pub fn set_journal_position(&mut self, vol: usize, journal_id: u64, next_usn: i64) {
        self.volumes[vol].journal_id = journal_id;
        self.volumes[vol].next_usn = next_usn;
    }

    /// Makes the index match the current state of `records` on volume `vol`.
    /// `created`: records whose journal events include a creation.
    pub fn apply(
        &mut self,
        vol: usize,
        records: &[u64],
        created: &HashSet<u64>,
        src: &mut dyn RecordSource,
    ) -> io::Result<ApplyStats> {
        let mut stats = ApplyStats::default();
        let mut pass = Pass::default();
        let exclusions = Exclusions { paths: self.exclusions.clone() };
        let rules = exclusions.for_volume(self.volumes[vol].letter());
        let smart: Vec<Smart> = exclusions.smart().into_iter().map(|(_, s)| s).collect();
        let mut ext_ids: HashMap<String, u16> =
            self.exts.iter().enumerate().map(|(i, e)| (e.clone(), i as u16)).collect();

        // Read every state once, then process parents before children: a file
        // created in a new folder must find that folder already in the index.
        let mut order: Vec<u64> = Vec::new();
        let mut seen = HashSet::new();
        for &r in records {
            if seen.insert(r) && !(r < FIRST_USER_RECORD && r != ROOT_RECORD) {
                order.push(r);
            }
        }
        let mut states: HashMap<u64, Option<RecordState>> = HashMap::with_capacity(order.len());
        for &r in &order {
            states.insert(r, src.record(r)?);
        }
        let mut pending: Vec<u64> = order;
        while !pending.is_empty() {
            let waiting: HashSet<u64> = pending.iter().copied().collect();
            let ready = |r: &u64| match &states[r] {
                Some(st) => st.links.iter().all(|(p, _)| *p == *r || !waiting.contains(p)),
                None => true,
            };
            let (now, later): (Vec<u64>, Vec<u64>) = pending.iter().partition(|r| ready(r));
            // No progress means a cycle in what we read (a move raced the reads):
            // process everything in order; the next batch fixes any leftover.
            let (now, later) = if now.is_empty() { (later, Vec::new()) } else { (now, later) };
            for r in now {
                let st = states[&r].clone();
                self.apply_record(vol, r, st, created.contains(&r), &rules, &smart, &mut ext_ids, &mut stats, &mut pass);
                stats.records += 1;
            }
            pending = later;
        }

        if pass.dirs_reordered {
            self.dir_order = compute_dir_order(self);
        }
        if pass.structural || pass.dirs_reordered {
            self.recompute_gone();
        } else {
            for e in pass.touched {
                self.update_gone(e);
            }
        }
        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_record(
        &mut self,
        vol: usize,
        r: u64,
        state: Option<RecordState>,
        created: bool,
        rules: &[Vec<String>],
        smart: &[Smart],
        ext_ids: &mut HashMap<String, u16>,
        stats: &mut ApplyStats,
        pass: &mut Pass,
    ) {
        let existing: Vec<u32> = self
            .entries_of(vol, r)
            .into_iter()
            .filter(|&e| self.flags[e as usize] & FLAG_DELETED == 0)
            .collect();

        let Some(st) = state else {
            for &e in &existing {
                self.mark_deleted(e, pass);
                stats.removed += 1;
            }
            self.set_entries_of(vol, r, &[]);
            let letter = self.volumes[vol].letter();
            if let Some(left) = self.left_out.get_mut(&letter) {
                left.remove(&r);
            }
            return;
        };

        if r == ROOT_RECORD {
            for &e in &existing {
                self.mtime[e as usize] = filetime_to_minutes(st.modified);
            }
            return;
        }

        // Where each link should be: an indexed, non-excluded folder, or nowhere.
        let desired: Vec<(Option<u32>, &str)> = st
            .links
            .iter()
            .map(|(p, name)| {
                let pe = self
                    .live_dir(vol, *p)
                    .filter(|&pe| !self.is_excluded_child(pe, name, rules))
                    // a folder not in the index that the build leaves out
                    // (a `target` beside Cargo.toml, a browser cache): it
                    // stays out; it is not a folder come in from elsewhere
                    // (also one moved or renamed into such a place; one that
                    // stays where it is is left as it is until compaction)
                    .filter(|&pe| {
                        let stays = existing.iter().any(|&e| self.parent(e) == Some(pe) && self.name(e) == name);
                        // left out by the build, and still where it was
                        let left = existing.is_empty() && !created && self.was_left_out(vol, r, *p, name);
                        !(st.is_dir && !stays && (left || self.smart_excludes(pe, name, smart)))
                    });
                (pe, name.as_str())
            })
            .collect();
        // A folder kept out here (made in a left-out place, or moved there)
        // is known from now on as the build knows its own: a later change
        // in it, still where it is, is no folder come in from elsewhere.
        if st.is_dir && st.links.len() == 1 && desired[0].0.is_none() {
            let (p, name) = &st.links[0];
            if self.live_dir(vol, *p).is_some() {
                let letter = self.volumes[vol].letter();
                self.left_out.entry(letter).or_default().insert(r, (*p, fold(name)));
            }
        }

        // Keep entries that are already right, reuse the others for renames/moves.
        let mut used = vec![false; existing.len()];
        let mut assign: Vec<Option<usize>> = vec![None; desired.len()];
        for (d, (pe, name)) in desired.iter().enumerate() {
            if let Some(k) = (0..existing.len()).find(|&k| !used[k] && self.is_at(existing[k], *pe, name)) {
                used[k] = true;
                assign[d] = Some(k);
            }
        }
        for slot in assign.iter_mut().filter(|a| a.is_none()) {
            if let Some(k) = (0..existing.len()).find(|&k| !used[k]) {
                used[k] = true;
                *slot = Some(k);
            }
        }

        let mut result = Vec::with_capacity(desired.len());
        for (d, (pe, name)) in desired.iter().enumerate() {
            match assign[d] {
                Some(k) => {
                    let e = existing[k];
                    if st.is_dir && !self.is_dir(e) && !created {
                        stats.needs_rescan = true; // a file record became a folder we never saw
                    }
                    self.place(e, *pe, name, &st, ext_ids, pass);
                    result.push(e);
                    stats.updated += 1;
                }
                None => {
                    let Some(p) = *pe else { continue }; // outside the index: nothing to add
                    if st.is_dir && !created {
                        stats.needs_rescan = true; // its contents were never indexed
                    }
                    let e = self.push_entry(vol, p, name, &st, ext_ids, pass);
                    result.push(e);
                    stats.added += 1;
                }
            }
        }
        for (k, &e) in existing.iter().enumerate() {
            if used[k] {
                continue;
            }
            if st.complete {
                self.mark_deleted(e, pass);
                stats.removed += 1;
            } else {
                result.push(e); // its name may be in an extension record we did not read
            }
        }
        self.set_entries_of(vol, r, &result);
    }

    /// The live folder entry of a record, if indexed.
    fn live_dir(&self, vol: usize, record: u64) -> Option<u32> {
        self.entries_of(vol, record)
            .into_iter()
            .find(|&e| self.flags[e as usize] & (FLAG_DIR | FLAG_DELETED) == FLAG_DIR)
    }

    fn is_at(&self, e: u32, pe: Option<u32>, name: &str) -> bool {
        let detached = self.flags[e as usize] & FLAG_DETACHED != 0;
        let place_ok = match pe {
            None => detached,
            Some(p) => !detached && self.parent[e as usize] == p,
        };
        place_ok && self.name(e) == name
    }

    /// True when a folder `name` in `parent` is left out by one of the rules
    /// that look around a folder, as the build decides it. Project files
    /// beside it and markers inside it are looked up on disk.
    /// Folder record `r` of volume `vol` was left out by the build, under
    /// `parent` (a record) with this name: it has not moved since.
    fn was_left_out(&self, vol: usize, r: u64, parent: u64, name: &str) -> bool {
        let letter = self.volumes[vol].letter();
        self.left_out
            .get(&letter)
            .and_then(|left| left.get(&r))
            .is_some_and(|(p, n)| *p == parent && *n == fold(name))
    }

    fn smart_excludes(&self, parent: u32, name: &str, smart: &[Smart]) -> bool {
        if smart.is_empty() {
            return false;
        }
        let on_disk = name;
        let name = fold(name);
        // the names of the folders above, nearest first (the drive's root not included)
        let above = |n: usize| -> Vec<String> {
            let mut out = Vec::new();
            let mut cur = parent;
            while out.len() < n && self.flags[cur as usize] & FLAG_ROOT == 0 {
                out.push(fold(self.name(cur)));
                match self.parent(cur) {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            out
        };
        // `comps` ends with this folder; `*` matches any one folder
        let tail = |comps: &[String]| -> bool {
            let Some((last, up)) = comps.split_last() else { return false };
            if *last != name {
                return false;
            }
            let have = above(up.len());
            have.len() == up.len() && up.iter().rev().zip(&have).all(|(c, h)| c == "*" || c == h)
        };
        let dir = || {
            let p = self.full_path(parent);
            std::path::PathBuf::from(if p.ends_with('\\') { p } else { p + "\\" })
        };
        smart.iter().any(|s| match s {
            Smart::Suffix(sfx) => name.ends_with(sfx.as_str()) && name.len() > sfx.len(),
            Smart::Tail(comps) => tail(comps),
            Smart::Under { anchor, tail: comps } => {
                tail(comps) && above(4096).iter().skip(comps.len() - 1).any(|a| a == anchor)
            }
            Smart::Beside { name: want, manifests } => {
                *want == name
                    && std::fs::read_dir(dir()).is_ok_and(|entries| {
                        entries.take(100_000).flatten().any(|e| {
                            let f = fold(&e.file_name().to_string_lossy());
                            manifests.iter().any(|m| match m.strip_prefix('*') {
                                Some(ext) => f.ends_with(ext) && f.len() > ext.len(),
                                None => f == *m,
                            })
                        })
                    })
            }
            Smart::Marker(m) => {
                let mut p = dir();
                p.push(on_disk); // its name as on disk, not folded
                p.push(m);
                std::fs::symlink_metadata(p).is_ok()
            }
        })
    }

    /// True when `parent\name` is one of the exclusion paths.
    fn is_excluded_child(&self, parent: u32, name: &str, rules: &[Vec<String>]) -> bool {
        if rules.is_empty() {
            return false;
        }
        let folded = fold(name);
        if rules.iter().any(|r| r[0] == crate::build::ANY_DEPTH && r[1] == folded) {
            return true; // `**\name`: this name anywhere
        }
        let mut comps = vec![folded];
        let mut cur = parent;
        for _ in 0..4096 {
            if self.flags[cur as usize] & FLAG_ROOT != 0 {
                break;
            }
            comps.push(fold(self.name(cur)));
            match self.parent(cur) {
                Some(p) => cur = p,
                None => return false,
            }
        }
        comps.reverse();
        rules.contains(&comps)
    }

    fn place(
        &mut self,
        e: u32,
        pe: Option<u32>,
        name: &str,
        st: &RecordState,
        ext_ids: &mut HashMap<String, u16>,
        pass: &mut Pass,
    ) {
        let i = e as usize;
        let was_dir = self.flags[i] & FLAG_DIR != 0;
        if self.name(e) != name {
            self.set_tail_name(e, name);
            self.ext[i] = if st.is_dir { 0 } else { ext_id(ext_ids, &mut self.exts, name) };
            self.name_rank[i] = u32::MAX; // exact order restored by the next compaction
        }
        match pe {
            None => {
                if self.flags[i] & FLAG_DETACHED == 0 {
                    self.removed += 1;
                    self.flags[i] |= FLAG_DETACHED;
                    self.parent[i] = NO_PARENT;
                    pass.structural |= was_dir;
                }
            }
            Some(p) => {
                if self.flags[i] & FLAG_DETACHED != 0 {
                    self.flags[i] &= !FLAG_DETACHED;
                    self.removed = self.removed.saturating_sub(1);
                    pass.structural |= was_dir;
                }
                if self.parent[i] != p {
                    self.parent[i] = p;
                    pass.dirs_reordered |= was_dir;
                }
            }
        }
        let keep = self.flags[i] & KEEP_FLAGS;
        self.flags[i] = keep | attr_flags(st.attributes, st.is_dir) | self.noise_for(pe, name, st.is_dir);
        if was_dir != st.is_dir {
            pass.structural = true;
            pass.dirs_reordered = true;
            // a reused record: a folder has no extension, a file its own
            self.ext[i] = if st.is_dir { 0 } else { ext_id(ext_ids, &mut self.exts, name) };
        }
        self.size[i] = match (st.is_dir, st.size) {
            (true, _) => 0,
            (false, SIZE_UNKNOWN) if !was_dir => self.size[i],
            (false, SIZE_UNKNOWN) => 0,
            (false, size) => size,
        };
        self.mtime[i] = filetime_to_minutes(st.modified);
        pass.touched.push(e);
    }

    fn push_entry(
        &mut self,
        vol: usize,
        parent: u32,
        name: &str,
        st: &RecordState,
        ext_ids: &mut HashMap<String, u16>,
        pass: &mut Pass,
    ) -> u32 {
        let e = self.len() as u32;
        self.parent.push(parent);
        self.flags.push(attr_flags(st.attributes, st.is_dir) | self.noise_for(Some(parent), name, st.is_dir));
        self.ext.push(if st.is_dir { 0 } else { ext_id(ext_ids, &mut self.exts, name) });
        self.size.push(if st.is_dir || st.size == SIZE_UNKNOWN { 0 } else { st.size });
        self.mtime.push(filetime_to_minutes(st.modified));
        self.volume.push(vol as u8);
        self.name_rank.push(u32::MAX);
        self.set_tail_name(e, name);
        if st.is_dir {
            self.dir_order.push(e); // its parent is already in the index, so it comes earlier
        }
        pass.touched.push(e);
        e
    }

    fn noise_for(&self, parent: Option<u32>, name: &str, is_dir: bool) -> u16 {
        place_flags(self, parent, &fold(name), is_dir)
    }

    fn set_tail_name(&mut self, e: u32, name: &str) {
        let k = self.tail_names.len() as u32;
        self.tail_names.push(name.to_string());
        self.tail_entry.push(e);
        self.tail_of.insert(e, k);
        self.flags[e as usize] |= FLAG_TAIL;
    }

    fn mark_deleted(&mut self, e: u32, pass: &mut Pass) {
        let f = &mut self.flags[e as usize];
        if *f & (FLAG_DELETED | FLAG_DETACHED) == 0 {
            self.removed += 1;
        }
        if *f & FLAG_DIR != 0 {
            pass.structural = true;
        }
        *f |= FLAG_DELETED | FLAG_GONE;
    }

    fn update_gone(&mut self, e: u32) {
        let i = e as usize;
        let own = self.flags[i] & (FLAG_DELETED | FLAG_DETACHED) != 0;
        let parent_gone = self.parent(e).is_some_and(|p| self.flags[p as usize] & FLAG_GONE != 0);
        if own || parent_gone {
            self.flags[i] |= FLAG_GONE;
        } else {
            self.flags[i] &= !FLAG_GONE;
        }
    }

    /// Recomputes FLAG_GONE for everything (folders parents-first, then files).
    fn recompute_gone(&mut self) {
        for k in 0..self.dir_order.len() {
            let d = self.dir_order[k];
            self.update_gone(d);
        }
        for e in 0..self.len() as u32 {
            if self.flags[e as usize] & FLAG_DIR == 0 {
                self.update_gone(e);
            }
        }
    }

    pub(crate) fn entries_of(&self, vol: usize, record: u64) -> Vec<u32> {
        let Some(&first) = self.rec_maps[vol].get(record as usize) else { return Vec::new() };
        if first == NO_PARENT {
            return Vec::new();
        }
        let mut out = vec![first];
        if let Some(more) = self.links.get(&first) {
            out.extend_from_slice(more);
        }
        out
    }

    fn set_entries_of(&mut self, vol: usize, record: u64, list: &[u32]) {
        let r = record as usize;
        let map = &mut self.rec_maps[vol];
        if map.len() <= r {
            map.resize(r + 1, NO_PARENT);
        }
        let old = map[r];
        if old != NO_PARENT {
            self.links.remove(&old);
        }
        match list {
            [] => self.rec_maps[vol][r] = NO_PARENT,
            [first, rest @ ..] => {
                self.rec_maps[vol][r] = *first;
                if !rest.is_empty() {
                    self.links.insert(*first, rest.to_vec());
                }
            }
        }
    }

    /// Rebuilds a clean index (no overlay, exact sort order) from the current
    /// state. Detached subtrees are dropped; restoring one later triggers a rescan.
    pub fn compact(&mut self) {
        if self.is_compact() {
            return;
        }
        let exclusions = Exclusions { paths: self.exclusions.clone() };
        let scans = self.to_scans();
        let skipped = self.skipped.clone();
        let left_out = std::mem::take(&mut self.left_out);
        *self = build(scans, &exclusions).0;
        // rebuilt from what the index holds: what was left out is not there
        // to count again, nor to be known again
        self.skipped = skipped;
        self.left_out = left_out;
    }

    /// A clean index where the given volumes come from fresh scans and the
    /// others from this index (used when a volume's journal cannot be replayed).
    pub fn rebuild_with(&self, fresh: Vec<VolumeScan>) -> NameIndex {
        self.rebuild(fresh, &[])
    }

    /// Like `rebuild_with`, and also drops the volumes in `remove` (a USB
    /// stick taken out, a drive that is gone).
    pub fn rebuild(&self, fresh: Vec<VolumeScan>, remove: &[char]) -> NameIndex {
        let exclusions = Exclusions { paths: self.exclusions.clone() };
        let mut scans: Vec<VolumeScan> = self
            .to_scans()
            .into_iter()
            .filter(|s| !fresh.iter().any(|f| f.letter.eq_ignore_ascii_case(&s.letter)))
            .filter(|s| !remove.iter().any(|r| r.eq_ignore_ascii_case(&s.letter)))
            .collect();
        let fresh_letters: Vec<char> = fresh.iter().map(|f| f.letter.to_ascii_uppercase()).collect();
        scans.extend(fresh);
        scans.sort_by_key(|s| s.letter.to_ascii_uppercase());
        let mut idx = build(scans, &exclusions).0;
        // Drives scanned again count anew; the others were rebuilt from the
        // index, where what was left out is gone: they keep their counts.
        let gone = |l: char| fresh_letters.contains(&l) || remove.iter().any(|r| r.to_ascii_uppercase() == l);
        let mut skipped: Vec<(char, String, u64)> =
            self.skipped.0.iter().filter(|(l, _, _)| !gone(*l)).cloned().collect();
        skipped.extend(idx.skipped.0.iter().filter(|(l, _, _)| fresh_letters.contains(l)).cloned());
        idx.skipped = crate::index::SkipCounts(skipped);
        // the same for the folders left out: scanned drives anew, the others kept
        for (l, folders) in self.left_out.iter() {
            if !gone(*l) {
                idx.left_out.insert(*l, folders.clone());
            }
        }
        idx
    }

    /// Converts the live entries back into per-volume scans.
    fn to_scans(&self) -> Vec<VolumeScan> {
        let mut rec_of = vec![u64::MAX; self.len()];
        for map in &self.rec_maps {
            for (r, &f) in map.iter().enumerate() {
                if f == NO_PARENT {
                    continue;
                }
                rec_of[f as usize] = r as u64;
                if let Some(more) = self.links.get(&f) {
                    for &l in more {
                        rec_of[l as usize] = r as u64;
                    }
                }
            }
        }
        let mut scans: Vec<VolumeScan> = self
            .volumes
            .iter()
            .map(|v| VolumeScan {
                letter: v.letter(),
                serial: v.serial,
                journal_id: v.journal_id,
                next_usn: v.next_usn,
                scan: MftScan::default(),
            })
            .collect();
        // Exact room first: a list grown by doubling holds both the old and
        // the new copy for a moment, hundreds of MB on a big disk (PLAN.md §52).
        let mut need = vec![(0usize, 0usize); scans.len()];
        for i in 0..self.len() as u32 {
            let f = self.flags[i as usize];
            if f & FLAG_GONE != 0 || rec_of[i as usize] == u64::MAX {
                continue;
            }
            let n = &mut need[self.volume[i as usize] as usize];
            n.0 += 1;
            n.1 += if f & FLAG_ROOT != 0 { 1 } else { self.name(i).len() };
        }
        for (s, (entries, bytes)) in scans.iter_mut().zip(need) {
            s.scan.entries.reserve_exact(entries);
            s.scan.names.reserve_exact(bytes);
        }
        for i in 0..self.len() as u32 {
            let f = self.flags[i as usize];
            let record = rec_of[i as usize];
            if f & FLAG_GONE != 0 || record == u64::MAX {
                continue;
            }
            let is_root = f & FLAG_ROOT != 0;
            let parent = if is_root {
                ROOT_RECORD
            } else {
                match self.parent(i).map(|p| rec_of[p as usize]) {
                    Some(p) if p != u64::MAX => p,
                    _ => continue,
                }
            };
            let mut attributes = 0;
            if f & FLAG_HIDDEN != 0 {
                attributes |= 0x2;
            }
            if f & FLAG_SYSTEM != 0 {
                attributes |= 0x4;
            }
            if f & FLAG_CLOUD != 0 {
                attributes |= 0x0040_0000;
            }
            scans[self.volume[i as usize] as usize].scan.push(
                MftEntry {
                    record,
                    parent,
                    size: self.size[i as usize],
                    modified: self.mtime[i as usize] as u64 * 600_000_000,
                    attributes,
                    is_dir: f & FLAG_DIR != 0,
                    ..Default::default()
                },
                if is_root { "." } else { self.name(i) },
            );
        }
        scans
    }

    /// The MFT record of each entry (`None` for removed entries), in one pass
    /// over the record maps.
    pub fn records_of(&self, entries: &[u32]) -> Vec<Option<u64>> {
        let mut want: HashMap<u32, usize> = HashMap::with_capacity(entries.len());
        for (k, &e) in entries.iter().enumerate() {
            want.insert(e, k);
        }
        let mut out = vec![None; entries.len()];
        for map in &self.rec_maps {
            for (record, &first) in map.iter().enumerate() {
                if first == NO_PARENT {
                    continue;
                }
                if let Some(&k) = want.get(&first) {
                    out[k] = Some(record as u64);
                }
                if let Some(more) = self.links.get(&first) {
                    for m in more {
                        if let Some(&k) = want.get(m) {
                            out[k] = Some(record as u64);
                        }
                    }
                }
            }
        }
        out
    }

    /// The live entry (first hard link) of an MFT record, if any.
    pub fn live_entry_of(&self, vol: usize, record: u64) -> Option<u32> {
        if vol >= self.rec_maps.len() {
            return None;
        }
        self.entries_of(vol, record).into_iter().find(|&e| self.flags[e as usize] & HIDDEN_FROM_SEARCH == 0)
    }

    /// Entries (any state) of one MFT record, for diagnostics.
    pub fn entries_for_record(&self, vol: usize, record: u64) -> Vec<u32> {
        self.entries_of(vol, record)
    }

    /// (volume, MFT record) of the live entries whose full path is in `paths`.
    /// Slow (builds every path); meant for diagnostics such as `wt verify`.
    pub fn records_for_paths(&self, paths: &HashSet<String>) -> Vec<(usize, u64)> {
        let mut wanted: HashSet<u32> = HashSet::new();
        for i in 0..self.len() as u32 {
            if self.flags[i as usize] & HIDDEN_FROM_SEARCH == 0 && paths.contains(&self.full_path(i)) {
                wanted.insert(i);
            }
        }
        let mut out = Vec::new();
        for (v, map) in self.rec_maps.iter().enumerate() {
            for (r, &f) in map.iter().enumerate() {
                if f == NO_PARENT {
                    continue;
                }
                let hit = wanted.contains(&f) || self.links.get(&f).is_some_and(|l| l.iter().any(|e| wanted.contains(e)));
                if hit {
                    out.push((v, r as u64));
                }
            }
        }
        out
    }

    /// Everything a search can see, in a form two indexes can be compared by:
    /// (path, is folder, size, minutes, hidden/system/cloud flags).
    pub fn canonical(&self) -> BTreeSet<(String, bool, u64, u32, u16)> {
        (0..self.len() as u32)
            .filter(|&i| self.flags[i as usize] & HIDDEN_FROM_SEARCH == 0)
            .map(|i| {
                let f = self.flags[i as usize];
                (
                    self.full_path(i),
                    f & FLAG_DIR != 0,
                    self.size[i as usize],
                    self.mtime[i as usize],
                    f & (FLAG_HIDDEN | FLAG_SYSTEM | FLAG_CLOUD),
                )
            })
            .collect()
    }
}
