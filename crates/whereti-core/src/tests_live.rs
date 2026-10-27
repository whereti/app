//! Differential test of live updates (PLAN.md §14): a model file system takes
//! thousands of random operations; after every batch the live-updated index must
//! equal an index built from scratch from the same state.

use std::collections::{BTreeMap, HashSet};
use std::io;

use whereti_ntfs::mft::{MftEntry, MftScan};
use whereti_query::{fold, parse, DateCtx};

use crate::*;

const ROOT: u64 = 5;
const USERS: u64 = 20;
const DOCS: u64 = 22;
const PROIECTE: u64 = 23;
const WINDOWS: u64 = 30;
const RECYCLE: u64 = 40;
const PROTECTED: [u64; 4] = [ROOT, WINDOWS, RECYCLE, USERS];

#[derive(Clone, Debug)]
pub(crate) struct Rec {
    is_dir: bool,
    size: u64,
    modified: u64,
    attrs: u32,
    links: Vec<(u64, String)>,
}

pub(crate) struct Model {
    pub(crate) recs: BTreeMap<u64, Rec>,
    next: u64,
    rng: u64,
    pub(crate) changed: Vec<u64>,
    pub(crate) created: HashSet<u64>,
    log: Vec<String>,
}

impl RecordSource for Model {
    fn record(&mut self, r: u64) -> io::Result<Option<RecordState>> {
        Ok(self.recs.get(&r).map(|x| RecordState {
            is_dir: x.is_dir,
            attributes: x.attrs,
            size: x.size,
            modified: x.modified,
            links: x.links.clone(),
            complete: true,
        }))
    }
}

const MINUTE: u64 = 600_000_000;

impl Model {
    pub(crate) fn new(seed: u64) -> Self {
        let mut m = Model {
            recs: BTreeMap::new(),
            next: 100,
            rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            changed: Vec::new(),
            created: HashSet::new(),
            log: Vec::new(),
        };
        let dir = |p: u64, n: &str| Rec { is_dir: true, size: 0, modified: 200_000_000 * MINUTE, attrs: 0, links: vec![(p, n.into())] };
        m.recs.insert(ROOT, dir(ROOT, "."));
        m.recs.insert(USERS, dir(ROOT, "Users"));
        m.recs.insert(21, dir(USERS, "ana"));
        m.recs.insert(DOCS, dir(21, "Documents"));
        m.recs.insert(PROIECTE, dir(ROOT, "proiecte"));
        m.recs.insert(24, dir(21, "AppData"));
        m.recs.insert(WINDOWS, dir(ROOT, "Windows"));
        m.recs.insert(31, dir(WINDOWS, "System32"));
        m.recs.insert(RECYCLE, dir(ROOT, "$Recycle.Bin"));
        for (r, p, n) in [(50, DOCS, "raport.pdf"), (51, DOCS, "Mașină.docx"), (52, 31, "kernel32.dll"), (53, PROIECTE, "PLAN.md")] {
            m.recs.insert(r, Rec { is_dir: false, size: 10, modified: 210_000_000 * MINUTE, attrs: 0, links: vec![(p, n.into())] });
        }
        m
    }

    pub(crate) fn rand(&mut self, n: usize) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % n.max(1) as u64) as usize
    }

    fn scan(&self) -> MftScan {
        let mut s = MftScan::default();
        for (&r, rec) in &self.recs {
            for (p, name) in &rec.links {
                s.push(
                    MftEntry {
                        record: r,
                        parent: *p,
                        is_dir: rec.is_dir,
                        size: rec.size,
                        modified: rec.modified,
                        attributes: rec.attrs,
                        seq: 1,
                        ..Default::default()
                    },
                    name,
                );
            }
        }
        s
    }

    fn dirs(&self) -> Vec<u64> {
        self.recs.iter().filter(|(_, r)| r.is_dir).map(|(&k, _)| k).collect()
    }

    fn has_children(&self, d: u64) -> bool {
        self.recs.iter().any(|(&k, r)| k != d && r.links.iter().any(|(p, _)| *p == d))
    }

    /// True if `a` is `b` or one of its ancestors (folders have one link).
    fn is_ancestor(&self, a: u64, b: u64) -> bool {
        let mut cur = b;
        for _ in 0..1000 {
            if cur == a {
                return true;
            }
            if cur == ROOT {
                return false;
            }
            cur = self.recs[&cur].links[0].0;
        }
        false
    }

    fn name_taken(&self, parent: u64, name: &str) -> bool {
        let low = name.to_lowercase();
        self.recs.values().any(|r| r.links.iter().any(|(p, n)| *p == parent && n.to_lowercase() == low))
    }

    fn fresh_name(&mut self, parent: u64) -> String {
        const POOL: [&str; 10] =
            ["raport", "Mașină", "factura", "pagefile.sys", "node_modules", "temă", "IMG_0042.jpg", "Școală", "a.txt", "Data"];
        let base = POOL[self.rand(POOL.len())].to_string();
        let mut name = base.clone();
        let mut k = 1;
        while self.name_taken(parent, &name) {
            name = format!("{base} ({k})");
            k += 1;
        }
        name
    }

    fn pick(&mut self, pred: impl Fn(u64, &Rec) -> bool) -> Option<u64> {
        let c: Vec<u64> = self.recs.iter().filter(|(&k, r)| !PROTECTED.contains(&k) && pred(k, r)).map(|(&k, _)| k).collect();
        if c.is_empty() {
            None
        } else {
            Some(c[self.rand(c.len())])
        }
    }

    fn touch(&mut self, r: u64) {
        self.changed.push(r);
    }

    pub(crate) fn step(&mut self) {
        let op = self.rand(11);
        match op {
            0 | 1 => {
                let dirs = self.dirs();
                let p = dirs[self.rand(dirs.len())];
                let name = self.fresh_name(p);
                let r = self.next;
                self.next += 1;
                let is_dir = op == 1;
                let size = if is_dir { 0 } else { self.rand(5000) as u64 };
                let modified = (220_000_000 + self.rand(1000) as u64) * MINUTE;
                self.recs.insert(r, Rec { is_dir, size, modified, attrs: 0, links: vec![(p, name.clone())] });
                self.created.insert(r);
                self.touch(r);
                self.log.push(format!("create {} #{r} in #{p} {name}", if is_dir { "dir" } else { "file" }));
            }
            2 => {
                let Some(r) = self.pick(|_, _| true) else { return };
                if self.recs[&r].is_dir && self.has_children(r) {
                    return;
                }
                self.recs.remove(&r);
                self.touch(r);
                self.log.push(format!("delete #{r}"));
            }
            3 => {
                let Some(r) = self.pick(|_, _| true) else { return };
                let li = self.rand(self.recs[&r].links.len());
                let p = self.recs[&r].links[li].0;
                let name = self.fresh_name(p);
                self.recs.get_mut(&r).unwrap().links[li].1 = name.clone();
                self.touch(r);
                self.log.push(format!("rename #{r} -> {name}"));
            }
            4 | 8 | 9 => {
                let Some(r) = (match op {
                    9 => self.pick(|_, rec| rec.links.iter().any(|(p, _)| *p == RECYCLE)),
                    _ => self.pick(|_, _| true),
                }) else {
                    return;
                };
                let target = match op {
                    8 => RECYCLE,
                    9 => {
                        let alive: Vec<u64> =
                            [DOCS, PROIECTE].into_iter().filter(|d| self.recs.get(d).is_some_and(|r| r.is_dir)).collect();
                        if alive.is_empty() {
                            return;
                        }
                        alive[self.rand(alive.len())]
                    }
                    _ => {
                        let dirs = self.dirs();
                        dirs[self.rand(dirs.len())]
                    }
                };
                if self.recs[&r].is_dir && self.is_ancestor(r, target) {
                    return;
                }
                let li = if op == 9 {
                    self.recs[&r].links.iter().position(|(p, _)| *p == RECYCLE).unwrap()
                } else {
                    self.rand(self.recs[&r].links.len())
                };
                let mut name = self.recs[&r].links[li].1.clone();
                if self.name_taken(target, &name) {
                    name = self.fresh_name(target);
                }
                self.recs.get_mut(&r).unwrap().links[li] = (target, name);
                self.touch(r);
                self.log.push(format!("move #{r} -> #{target}"));
            }
            5 => {
                let Some(r) = self.pick(|_, rec| !rec.is_dir) else { return };
                let size = self.rand(1 << 20) as u64;
                let m = (230_000_000 + self.rand(1000) as u64) * MINUTE;
                let rec = self.recs.get_mut(&r).unwrap();
                rec.size = size;
                rec.modified = m;
                self.touch(r);
                self.log.push(format!("modify #{r}"));
            }
            6 => {
                let Some(r) = self.pick(|_, rec| !rec.is_dir) else { return };
                let dirs = self.dirs();
                let p = dirs[self.rand(dirs.len())];
                let name = self.fresh_name(p);
                self.recs.get_mut(&r).unwrap().links.push((p, name));
                self.touch(r);
                self.log.push(format!("hardlink #{r} in #{p}"));
            }
            7 => {
                let Some(r) = self.pick(|_, rec| rec.links.len() > 1) else { return };
                let li = self.rand(self.recs[&r].links.len());
                self.recs.get_mut(&r).unwrap().links.remove(li);
                self.touch(r);
                self.log.push(format!("unlink #{r}"));
            }
            _ => {
                let Some(r) = self.pick(|_, _| true) else { return };
                self.recs.get_mut(&r).unwrap().attrs ^= 0x2;
                self.touch(r);
                self.log.push(format!("toggle hidden #{r}"));
            }
        }
    }
}

fn exclusions() -> Exclusions {
    let mut e = Exclusions::defaults(Some(r"C:\Windows"));
    e.paths.push(r"**\node_modules".into()); // the model creates folders with this name
    e
}

pub(crate) fn fresh(model: &Model) -> NameIndex {
    build(vec![VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 0, scan: model.scan() }], &exclusions()).0
}

fn assert_same(live: &NameIndex, model: &Model, what: &str) {
    let a = live.canonical();
    let b = fresh(model).canonical();
    if a != b {
        let only_live: Vec<_> = a.difference(&b).take(5).collect();
        let only_fresh: Vec<_> = b.difference(&a).take(5).collect();
        panic!(
            "{what}: live index differs from a fresh build\n  only in live: {only_live:#?}\n  only in fresh: {only_fresh:#?}\n  last ops: {:#?}",
            &model.log[model.log.len().saturating_sub(12)..]
        );
    }
}

fn run_seed(seed: u64, batches: usize) -> (usize, usize) {
    let mut model = Model::new(seed);
    let mut idx = fresh(&model);
    let mut rescans = 0;
    let mut ops = 0;
    for b in 0..batches {
        let k = 1 + model.rand(8);
        for _ in 0..k {
            model.step();
        }
        ops += k;
        let changed = std::mem::take(&mut model.changed);
        let created = std::mem::take(&mut model.created);
        let stats = idx.apply(0, &changed, &created, &mut model).unwrap();
        if stats.needs_rescan {
            idx = fresh(&model);
            rescans += 1;
        }
        assert_same(&idx, &model, &format!("seed {seed}, batch {b}"));
        if b % 25 == 24 {
            idx.compact();
            assert!(idx.is_compact());
            assert_same(&idx, &model, &format!("seed {seed}, batch {b}, after compact"));
        }
    }
    (ops, rescans)
}

#[test]
fn live_updates_match_a_fresh_build() {
    let mut total_ops = 0;
    let mut total_rescans = 0;
    for seed in 1..=40 {
        let (ops, rescans) = run_seed(seed, 150);
        total_ops += ops;
        total_rescans += rescans;
    }
    // rescans are the expected fallback for folders moved in from outside, not the norm
    assert!(total_rescans * 10 < total_ops, "{total_rescans} rescans for {total_ops} ops");
}

fn ctx() -> DateCtx {
    DateCtx { now: 0, tz_offset: 0 }
}

fn hits(idx: &NameIndex, q: &str) -> Vec<String> {
    let r = idx.search(&parse(q, &ctx()), &SearchOptions::default());
    r.hits.iter().map(|&i| idx.full_path(i)).collect()
}

#[test]
fn search_sees_renames_deletes_and_restores() {
    let mut m = Model::new(7);
    let mut idx = fresh(&m);
    assert_eq!(hits(&idx, "raport"), vec![r"C:\Users\ana\Documents\raport.pdf"]);

    // rename
    m.recs.get_mut(&50).unwrap().links[0].1 = "Factură ENEL.pdf".into();
    idx.apply(0, &[50], &HashSet::new(), &mut m).unwrap();
    assert!(hits(&idx, "raport").is_empty());
    assert_eq!(hits(&idx, "factura enel"), vec![r"C:\Users\ana\Documents\Factură ENEL.pdf"]);

    // a folder with contents goes to the Recycle Bin and comes back
    m.recs.get_mut(&DOCS).unwrap().links[0] = (RECYCLE, "$R1".into());
    let s = idx.apply(0, &[DOCS], &HashSet::new(), &mut m).unwrap();
    assert!(!s.needs_rescan);
    assert!(hits(&idx, "factura").is_empty());
    assert!(hits(&idx, "masina").is_empty());
    m.recs.get_mut(&DOCS).unwrap().links[0] = (21, "Documents".into());
    let s = idx.apply(0, &[DOCS], &HashSet::new(), &mut m).unwrap();
    assert!(!s.needs_rescan, "restore needs no rescan");
    assert_eq!(hits(&idx, "masina"), vec![r"C:\Users\ana\Documents\Mașină.docx"]);

    // new folder + file inside it, reported child-first in the same batch
    m.recs.insert(200, Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(PROIECTE, "nou".into())] });
    m.recs.insert(201, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(200, "fișier.txt".into())] });
    let s = idx.apply(0, &[201, 200], &HashSet::from([200, 201]), &mut m).unwrap();
    assert!(!s.needs_rescan);
    assert_eq!(hits(&idx, "fisier"), vec![r"C:\proiecte\nou\fișier.txt"]);

    // a folder moved in from C:\Windows (never indexed) asks for a rescan
    m.recs.get_mut(&31).unwrap().links[0] = (PROIECTE, "System32".into());
    let s = idx.apply(0, &[31], &HashSet::new(), &mut m).unwrap();
    assert!(s.needs_rescan);

    // delete is idempotent
    m.recs.remove(&53);
    idx.apply(0, &[53, 53], &HashSet::new(), &mut m).unwrap();
    idx.apply(0, &[53], &HashSet::new(), &mut m).unwrap();
    assert!(hits(&idx, "plan").is_empty());
}

#[test]
fn excluded_paths_stay_out_when_created_live() {
    let mut m = Model::new(3);
    let mut idx = fresh(&m);
    m.recs.insert(300, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(ROOT, "pagefile.sys".into())] });
    m.recs.insert(301, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(31, "new.dll".into())] });
    idx.apply(0, &[300, 301], &HashSet::from([300, 301]), &mut m).unwrap();
    assert!(hits(&idx, "pagefile").is_empty());
    assert!(hits(&idx, "new.dll").is_empty());
    assert_eq!(fold("pagefile.sys"), "pagefile.sys");
}

#[test]
fn compact_index_survives_a_snapshot_round_trip() {
    let mut m = Model::new(11);
    let mut idx = fresh(&m);
    for _ in 0..200 {
        m.step();
    }
    let changed = std::mem::take(&mut m.changed);
    let created = std::mem::take(&mut m.created);
    let s = idx.apply(0, &changed, &created, &mut m).unwrap();
    if s.needs_rescan {
        idx = fresh(&m);
    }
    let dir = std::env::temp_dir().join(format!("whereti-live-{}", std::process::id()));
    let path = dir.join("names.idx");
    if !idx.is_compact() {
        assert!(snapshot::save(&idx, &path).is_err(), "saving an uncompacted index must be refused");
        idx.compact();
    }
    snapshot::save(&idx, &path).unwrap();
    let loaded = snapshot::load(&path).unwrap();
    assert_eq!(loaded, idx);
    assert_same(&loaded, &m, "after reload");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_change_in_a_folder_the_rules_leave_out_needs_no_rescan() {
    let mut m = Model::new(5);
    let dir = |p: u64, n: &str| Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(p, n.into())] };
    m.recs.insert(400, dir(DOCS, "Catalog.lrdata"));
    m.recs.insert(401, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(400, "preview.db".into())] });
    m.recs.insert(402, dir(24, "Local"));
    m.recs.insert(403, dir(402, "Browser"));
    m.recs.insert(404, dir(403, "Cache"));
    let mut e = exclusions();
    e.paths.push(r"**\*.lrdata".into());
    e.paths.push(r"**\AppData\Local\*\Cache".into());
    let mut idx = build(vec![VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 0, scan: m.scan() }], &e).0;
    assert!(hits(&idx, "preview").is_empty());
    // the left-out folders change (their dates move), nothing is created
    for r in [400, 404] {
        m.recs.get_mut(&r).unwrap().modified += MINUTE;
    }
    let s = idx.apply(0, &[400, 404], &HashSet::new(), &mut m).unwrap();
    assert!(!s.needs_rescan, "a left-out folder is not one come in from elsewhere");
    assert!(hits(&idx, "lrdata").is_empty());
    assert!(hits(&idx, "cache").is_empty());
    // a folder of the same name somewhere the rule does not reach still asks
    m.recs.get_mut(&404).unwrap().links[0] = (PROIECTE, "Cache".into());
    let s = idx.apply(0, &[404], &HashSet::new(), &mut m).unwrap();
    assert!(s.needs_rescan);
}

#[test]
fn a_folder_renamed_into_a_left_out_name_leaves_the_index() {
    let mut m = Model::new(6);
    m.recs.insert(410, Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(DOCS, "keep".into())] });
    m.recs.insert(411, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(410, "inside.txt".into())] });
    let mut e = exclusions();
    e.paths.push(r"**\*.lrdata".into());
    let mut idx = build(vec![VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 0, scan: m.scan() }], &e).0;
    assert_eq!(hits(&idx, "inside").len(), 1);
    m.recs.get_mut(&410).unwrap().links[0].1 = "Pics.lrdata".into();
    let s = idx.apply(0, &[410], &HashSet::new(), &mut m).unwrap();
    assert!(!s.needs_rescan);
    assert!(hits(&idx, "inside").is_empty(), "as a fresh build would");
}

#[test]
fn a_build_folder_left_out_stays_out_while_its_project_is_deleted() {
    let mut m = Model::new(8);
    // a project on disk only in the model: its Cargo.toml is not on the real disk,
    // so only the index tells the folder was left out
    m.recs.insert(420, Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(PROIECTE, "app".into())] });
    m.recs.insert(421, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(420, "Cargo.toml".into())] });
    m.recs.insert(422, Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(420, "target".into())] });
    m.recs.insert(423, Rec { is_dir: false, size: 1, modified: 0, attrs: 0, links: vec![(422, "app.exe".into())] });
    let mut e = exclusions();
    e.paths.push(r"**\target <Cargo.toml>".into());
    let mut idx = build(vec![VolumeScan { letter: 'C', serial: 1, journal_id: 1, next_usn: 0, scan: m.scan() }], &e).0;
    assert!(hits(&idx, "app.exe").is_empty());
    // Cargo.toml goes first, then target changes as it is emptied
    m.recs.remove(&421);
    m.recs.get_mut(&422).unwrap().modified += MINUTE;
    let s = idx.apply(0, &[421, 422], &HashSet::new(), &mut m).unwrap();
    assert!(!s.needs_rescan);
    // a new folder of that name, made now, is the user's: indexed
    m.recs.insert(424, Rec { is_dir: true, size: 0, modified: 0, attrs: 0, links: vec![(DOCS, "target".into())] });
    idx.apply(0, &[424], &HashSet::from([424]), &mut m).unwrap();
    assert_eq!(hits(&idx, "target").len(), 1);
}
