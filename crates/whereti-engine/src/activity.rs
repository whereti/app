//! Where the user works lately (PLAN.md §56): the folders in which their
//! own files are created and changed, learnt from the NTFS journal the
//! engine already follows. Files in those folders rank higher.
//!
//! Only the user's kind of files count (documents, pictures, code…; see
//! `NameIndex::is_work_file`): not programs, caches, logs, hidden, system or
//! tool folders. A folder gets at most `MAX_PER_BATCH` points per journal
//! batch, so unpacking an archive is not "work". Points halve every
//! `HALF_LIFE`, so what counts is the last days. Kept in `activity.txt` next
//! to the index (only the service can read it); nothing leaves the PC.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use whereti_core::NameIndex;
use whereti_query::{fold, Clause, Query, Term};

/// Points halve in a week.
const HALF_LIFE_MINUTES: f64 = 7.0 * 24.0 * 60.0;
const MAX_PER_BATCH: u32 = 3;
/// Folders remembered; the coldest are forgotten beyond this.
const MAX_FOLDERS: usize = 2000;
/// Folders that rank higher.
const TOP: usize = 50;
/// The ranking vector is rebuilt at most this often.
const REBUILD_EVERY: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Heat {
    /// The folder as Windows shows it.
    path: String,
    points: f64,
    /// When `points` was last brought up to date, minutes since 1601.
    at: u32,
    /// The folder's entry in the index, when known (checked before use).
    entry: Option<u32>,
}

impl Heat {
    fn now_points(&self, now: u32) -> f64 {
        self.points * 0.5f64.powf(now.saturating_sub(self.at) as f64 / HALF_LIFE_MINUTES)
    }
}

pub struct Activity {
    file: PathBuf,
    off_marker: PathBuf,
    on: AtomicBool,
    /// Folded folder path -> heat.
    folders: Mutex<HashMap<String, Heat>>,
    changed: AtomicBool,
    /// Seeded from the index's dates already (once, when nothing was learnt).
    seeded: AtomicBool,
    /// Per entry: 0-8, for `search_ranked`; and when it was built, for which length.
    ranking: RwLock<(Arc<Vec<u8>>, Option<Instant>, usize)>,
}

fn now_minutes() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() + 11_644_473_600) / 60)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

/// 1 change -> 1, ~10 -> 5, 50+ -> 8.
fn weight(points: f64) -> u8 {
    ((1.0 + points).log2() * 1.5).round().clamp(1.0, 8.0) as u8
}

impl Activity {
    /// Kept in `dir` (the folder of the index).
    pub fn open(dir: &Path) -> Activity {
        let file = dir.join("activity.txt");
        let off_marker = dir.join("activity-off");
        let on = !off_marker.exists();
        let mut folders = HashMap::new();
        if on {
            for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
                let mut c = line.splitn(3, '\t');
                let (Some(p), Some(a), Some(path)) = (c.next(), c.next(), c.next()) else { continue };
                let (Ok(points), Ok(at)) = (p.parse(), a.parse()) else { continue };
                folders.insert(fold(path), Heat { path: path.to_string(), points, at, entry: None });
            }
        }
        Activity {
            file,
            off_marker,
            on: AtomicBool::new(on),
            folders: Mutex::new(folders),
            changed: AtomicBool::new(true),
            seeded: AtomicBool::new(false),
            ranking: RwLock::new((Arc::new(Vec::new()), None, 0)),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Off forgets everything learnt (and learns nothing more); on starts again.
    pub fn set_on(&self, on: bool) -> Result<(), String> {
        if on {
            let _ = std::fs::remove_file(&self.off_marker);
        } else {
            std::fs::write(&self.off_marker, "off\n").map_err(|e| e.to_string())?;
            self.folders.lock().clear();
            let _ = std::fs::remove_file(&self.file);
        }
        self.on.store(on, Ordering::Relaxed);
        // on again: start from the index's dates, as the first time
        self.seeded.store(false, Ordering::Relaxed);
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// A journal batch of volume `vol` changed `records` (after the index applied it).
    pub fn note(&self, idx: &NameIndex, vol: usize, records: &[u64]) {
        if !self.is_on() {
            return;
        }
        let mut per_folder: HashMap<u32, u32> = HashMap::new();
        for &r in records {
            let Some(e) = idx.live_entry_of(vol, r) else { continue };
            if !idx.is_work_file(e) {
                continue;
            }
            if let Some(p) = idx.parent(e) {
                let n = per_folder.entry(p).or_insert(0);
                *n = (*n + 1).min(MAX_PER_BATCH);
            }
        }
        if per_folder.is_empty() {
            return;
        }
        let now = now_minutes();
        let mut folders = self.folders.lock();
        for (folder, n) in per_folder {
            let path = idx.full_path(folder);
            let h = folders.entry(fold(&path)).or_insert(Heat { path: path.clone(), points: 0.0, at: now, entry: None });
            h.points = h.now_points(now) + n as f64;
            h.at = now;
            h.path = path;
            h.entry = Some(folder);
        }
        if folders.len() > MAX_FOLDERS {
            let mut by: Vec<(f64, String)> = folders.iter().map(|(k, h)| (h.now_points(now), k.clone())).collect();
            by.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, k) in by.into_iter().take(folders.len() - MAX_FOLDERS) {
                folders.remove(&k);
            }
        }
        self.changed.store(true, Ordering::Relaxed);
    }

    /// The hottest folders, for Settings: hottest first.
    pub fn top(&self, n: usize) -> Vec<String> {
        let now = now_minutes();
        let folders = self.folders.lock();
        let mut by: Vec<(f64, &Heat)> = folders.values().map(|h| (h.now_points(now), h)).filter(|(p, _)| *p >= 1.0).collect();
        by.sort_by(|a, b| b.0.total_cmp(&a.0));
        by.into_iter().take(n).map(|(_, h)| h.path.clone()).collect()
    }

    /// The first time, with nothing learnt yet: the last two weeks from the
    /// dates in the index, by the same rules (at most `MAX_PER_BATCH` points
    /// per folder per day, halving every week), so it helps from day one.
    fn seed(&self, idx: &NameIndex) {
        if self.seeded.swap(true, Ordering::Relaxed) || !self.folders.lock().is_empty() {
            return;
        }
        const DAY: u32 = 24 * 60;
        let now = now_minutes();
        let since = now.saturating_sub(14 * DAY);
        // (folder, day) -> files changed
        let mut per_day: HashMap<(u32, u32), u32> = HashMap::new();
        for i in 0..idx.len() as u32 {
            let m = idx.modified(i);
            if m >= since && m <= now && idx.is_work_file(i) {
                if let Some(p) = idx.parent(i) {
                    *per_day.entry((p, m / DAY)).or_insert(0) += 1;
                }
            }
        }
        let mut points: HashMap<u32, f64> = HashMap::new();
        for ((folder, day), n) in per_day {
            let age = now.saturating_sub(day * DAY + DAY / 2) as f64;
            *points.entry(folder).or_insert(0.0) += n.min(MAX_PER_BATCH) as f64 * 0.5f64.powf(age / HALF_LIFE_MINUTES);
        }
        let mut best: Vec<(u32, f64)> = points.into_iter().filter(|(_, p)| *p >= 1.0).collect();
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        best.truncate(MAX_FOLDERS / 4);
        let mut folders = self.folders.lock();
        for (folder, p) in best {
            let path = idx.full_path(folder);
            folders.insert(fold(&path), Heat { path, points: p, at: now, entry: Some(folder) });
        }
        self.changed.store(true, Ordering::Relaxed);
    }

    /// Per entry, how much the user works there (for `search_ranked`).
    /// Rebuilt when something changed, at most every `REBUILD_EVERY`.
    pub fn ranking(&self, idx: &NameIndex) -> Arc<Vec<u8>> {
        if !self.is_on() {
            return Arc::new(Vec::new());
        }
        self.seed(idx);
        {
            let r = self.ranking.read();
            let fresh = r.1.is_some_and(|t| t.elapsed() < REBUILD_EVERY) || !self.changed.load(Ordering::Relaxed);
            if r.2 == idx.len() && fresh && r.1.is_some() {
                return r.0.clone();
            }
        }
        let now = now_minutes();
        let mut hot: Vec<(f64, String, Option<u32>)> = {
            let folders = self.folders.lock();
            folders.iter().map(|(k, h)| (h.now_points(now), k.clone(), h.entry)).filter(|(p, _, _)| *p >= 1.0).collect()
        };
        hot.sort_by(|a, b| b.0.total_cmp(&a.0));
        hot.truncate(TOP);
        let mut vec = vec![0u8; idx.len()];
        let mut found = Vec::new();
        for (points, key, entry) in hot {
            // the entry may have moved (a compaction renumbers): check, else look it up
            let e = entry.filter(|&e| (e as usize) < idx.len() && fold(&idx.full_path(e)) == key).or_else(|| find_folder(idx, &key));
            if let Some(e) = e {
                vec[e as usize] = weight(points);
                found.push((key, e));
            }
        }
        {
            let mut folders = self.folders.lock();
            for (key, e) in found {
                if let Some(h) = folders.get_mut(&key) {
                    h.entry = Some(e);
                }
            }
        }
        let vec = Arc::new(vec);
        *self.ranking.write() = (vec.clone(), Some(Instant::now()), idx.len());
        self.changed.store(false, Ordering::Relaxed);
        vec
    }

    /// Writes what was learnt (with the index, and when the service stops).
    pub fn save(&self) {
        if !self.is_on() {
            return;
        }
        let text: String = self.folders.lock().values().map(|h| format!("{:.3}\t{}\t{}\n", h.points, h.at, h.path)).collect();
        let tmp = self.file.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }
}

/// A folder's entry by its folded full path (after the index was rebuilt).
fn find_folder(idx: &NameIndex, key: &str) -> Option<u32> {
    let name = key.rsplit('\\').next()?;
    if name.is_empty() {
        return None;
    }
    let q = Query {
        clauses: vec![
            Clause { negated: false, any_of: vec![Term::FoldersOnly] },
            Clause { negated: false, any_of: vec![Term::Text(name.to_string())] },
        ],
    };
    idx.matching(&q).into_iter().find(|&e| fold(&idx.full_path(e)) == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_grow_slowly_with_work() {
        assert_eq!(weight(1.0), 2);
        assert!(weight(10.0) >= 4 && weight(10.0) <= 6);
        assert_eq!(weight(500.0), 8);
    }

    #[test]
    fn points_halve_in_a_week() {
        let h = Heat { path: String::new(), points: 8.0, at: 0, entry: None };
        assert!((h.now_points((HALF_LIFE_MINUTES) as u32) - 4.0).abs() < 0.01);
        assert!(h.now_points((4.0 * HALF_LIFE_MINUTES) as u32) < 0.6);
    }

    #[test]
    fn what_was_learnt_survives_a_restart_and_off_forgets_it() {
        let dir = std::env::temp_dir().join(format!("wt-activity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = Activity::open(&dir);
        a.folders.lock().insert(fold(r"D:\Lucru"), Heat { path: r"D:\Lucru".into(), points: 5.0, at: now_minutes(), entry: None });
        a.save();
        let b = Activity::open(&dir);
        assert_eq!(b.top(5), [r"D:\Lucru"]);
        b.set_on(false).unwrap();
        assert!(b.top(5).is_empty());
        let c = Activity::open(&dir);
        assert!(!c.is_on() && c.top(5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
