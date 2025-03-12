//! End-to-end tests on a synthetic two-volume tree.

use whereti_ntfs::mft::{MftEntry, MftScan};
use whereti_query::{day_from_civil, parse, DateCtx};

use crate::index::{FLAG_HIDDEN, FLAG_ROOT};
use crate::*;

const MB: u64 = 1024 * 1024;

fn ft(y: i64, m: i64, d: i64) -> u64 {
    (day_from_civil(y, m, d) * 1440) as u64 * 600_000_000
}

struct E {
    rec: u64,
    parent: u64,
    name: &'static str,
    dir: bool,
    size: u64,
    modified: u64,
    attrs: u32,
}

fn f(rec: u64, parent: u64, name: &'static str) -> E {
    E { rec, parent, name, dir: false, size: 100, modified: ft(2025, 6, 1), attrs: 0 }
}

fn d(rec: u64, parent: u64, name: &'static str) -> E {
    E { dir: true, size: 0, ..f(rec, parent, name) }
}

fn scan(entries: Vec<E>) -> MftScan {
    let mut s = MftScan::default();
    for e in entries {
        s.push(
            MftEntry {
                record: e.rec,
                parent: e.parent,
                is_dir: e.dir,
                size: e.size,
                modified: e.modified,
                attributes: e.attrs,
                seq: 1,
                ..Default::default()
            },
            e.name,
        );
    }
    s
}

fn drive_c() -> MftScan {
    scan(vec![
        f(0, 5, "$MFT"),
        d(5, 5, "."),
        d(11, 5, "$Extend"),
        d(30, 11, "$RmMetadata"),
        d(20, 5, "Windows"),
        d(21, 20, "System32"),
        f(40, 21, "kernel32.dll"),
        d(23, 5, "Users"),
        d(24, 23, "ana"),
        d(25, 24, "Documents"),
        E { size: 2 * MB, modified: ft(2026, 10, 24), ..f(41, 25, "Raport anual 2025.pdf") },
        f(42, 25, "Mașină de spălat.docx"),
        E { attrs: 0x2, size: 5, ..f(43, 25, "raport.txt") },
        E { size: 300, ..f(44, 25, "factura_enel.PDF") },
        f(49, 25, "IMG_0042.jpg"),
        d(31, 25, "Școală"),
        f(50, 31, "temă.txt"),
        d(32, 24, "AppData"),
        d(33, 32, "Local"),
        f(51, 33, "PLAN.md"),
        d(26, 5, "proiecte"),
        d(27, 26, "whereti"),
        f(45, 27, "PLAN.md"),
        f(47, 25, "shared.txt"),
        f(47, 26, "shared.txt"), // hard link
        d(28, 5, "$Recycle.Bin"),
        f(46, 28, "deleted.pdf"),
        d(29, 5, "System Volume Information"),
        f(48, 999, "orphan.txt"),
    ])
}

fn drive_d() -> MftScan {
    scan(vec![d(5, 5, "."), d(16, 5, "Acte"), f(17, 16, "contract.pdf")])
}

fn index() -> NameIndex {
    let vols = vec![
        VolumeScan { letter: 'C', serial: 1, journal_id: 10, next_usn: 100, scan: drive_c() },
        VolumeScan { letter: 'D', serial: 2, journal_id: 20, next_usn: 200, scan: drive_d() },
    ];
    build(vols, &Exclusions::defaults(Some(r"C:\Windows"))).0
}

fn ctx() -> DateCtx {
    DateCtx { now: (day_from_civil(2026, 10, 24) * 1440 + 720) as u32, tz_offset: 0 }
}

fn find(idx: &NameIndex, q: &str) -> Vec<String> {
    find_sorted(idx, q, SortBy::Relevance)
}

fn find_sorted(idx: &NameIndex, q: &str, sort: SortBy) -> Vec<String> {
    let r = idx.search(&parse(q, &ctx()), &SearchOptions { limit: 100, sort });
    assert_eq!(r.total, r.hits.len());
    r.hits.iter().map(|&i| idx.full_path(i)).collect()
}

fn names(idx: &NameIndex) -> Vec<&str> {
    (0..idx.len() as u32).map(|i| idx.name(i)).collect()
}

#[test]
fn default_exclusions_and_metafiles_are_left_out() {
    let idx = index();
    let all = names(&idx);
    for gone in ["$MFT", "$Extend", "$RmMetadata", "Windows", "kernel32.dll", "$Recycle.Bin", "deleted.pdf", "System Volume Information"] {
        assert!(!all.contains(&gone), "{gone} should be excluded");
    }
    for kept in ["Users", "Raport anual 2025.pdf", "temă.txt", "PLAN.md", "contract.pdf", "orphan.txt"] {
        assert!(all.contains(&kept), "{kept} should be kept");
    }
    assert_eq!(idx.volumes().len(), 2);
    assert_eq!((0..idx.len() as u32).filter(|&i| idx.flags(i) & FLAG_ROOT != 0).count(), 2);
}

#[test]
fn builds_full_paths_on_demand() {
    let idx = index();
    let p = find(&idx, "anual");
    assert_eq!(p, vec![r"C:\Users\ana\Documents\Raport anual 2025.pdf"]);
    assert_eq!(find(&idx, "contract"), vec![r"D:\Acte\contract.pdf"]);
    assert_eq!(find(&idx, "orphan"), vec![r"C:\orphan.txt"]);
}

#[test]
fn case_and_diacritics_insensitive() {
    let idx = index();
    let want = vec![r"C:\Users\ana\Documents\Mașină de spălat.docx".to_string()];
    assert_eq!(find(&idx, "masina"), want);
    assert_eq!(find(&idx, "MAȘINĂ"), want);
    assert_eq!(find(&idx, "maşină spalat"), want); // cedilla + second word
    assert_eq!(find(&idx, "PLAN")[0], r"C:\proiecte\whereti\PLAN.md");
}

#[test]
fn relevance_puts_exact_and_prefix_matches_first() {
    let idx = index();
    let r = find(&idx, "raport.txt");
    assert_eq!(r, vec![r"C:\Users\ana\Documents\raport.txt"]);
    // "raport.txt" is hidden, so the visible prefix match wins
    let r = find(&idx, "raport");
    assert_eq!(r.len(), 2);
    assert!(r[0].ends_with("Raport anual 2025.pdf"));
    let r = find(&idx, "enel");
    assert!(r[0].ends_with("factura_enel.PDF")); // word start after '_'
}

#[test]
fn functions_wildcards_and_boolean_logic() {
    let idx = index();
    assert_eq!(find(&idx, "ext:pdf").len(), 3);
    assert_eq!(find(&idx, "*.pdf").len(), 3);
    assert_eq!(find(&idx, "img_????.jpg").len(), 1);
    assert_eq!(find(&idx, r"regex:^img_\d{4}").len(), 1);
    assert_eq!(find(&idx, "raport !txt").len(), 1);
    assert_eq!(find(&idx, "enel | contract").len(), 2);
    assert_eq!(find(&idx, "size:>1mb"), vec![r"C:\Users\ana\Documents\Raport anual 2025.pdf"]);
    assert_eq!(find(&idx, "dm:today").len(), 1);
    assert_eq!(find(&idx, "folder:doc"), vec![r"C:\Users\ana\Documents"]);
    assert_eq!(find(&idx, "file:proiecte"), Vec::<String>::new());
    assert_eq!(find(&idx, "shared").len(), 2, "both hard links are listed");
    assert!(find(&idx, "kernel32").is_empty());
}

#[test]
fn path_matching_crosses_folder_boundaries() {
    let idx = index();
    let docs = find(&idx, r"ana\documents");
    assert_eq!(docs.len(), 9, "{docs:#?}"); // the folder, 7 entries in it, temă.txt
    // no plain word to rank by -> alphabetical
    assert_eq!(find(&idx, r"proiecte\wh"), vec![r"C:\proiecte\whereti\PLAN.md", r"C:\proiecte\whereti"]);
    assert_eq!(find(&idx, r"path:d:\acte ext:pdf"), vec![r"D:\Acte\contract.pdf"]);
    assert_eq!(find(&idx, r"scoala\tema"), vec![r"C:\Users\ana\Documents\Școală\temă.txt"]);
    assert_eq!(find(&idx, r"path:proiecte plan"), vec![r"C:\proiecte\whereti\PLAN.md"]);
}

#[test]
fn sorting_orders() {
    let idx = index();
    let by_size = find_sorted(&idx, "ext:pdf", SortBy::Size);
    assert!(by_size[0].ends_with("Raport anual 2025.pdf"));
    let by_date = find_sorted(&idx, "ext:pdf", SortBy::Modified);
    assert!(by_date[0].ends_with("Raport anual 2025.pdf"));
    let by_name = find_sorted(&idx, "ext:pdf", SortBy::Name);
    assert!(by_name[0].ends_with("contract.pdf"));
    assert!(by_name[2].ends_with("Raport anual 2025.pdf"));
}

#[test]
fn limit_keeps_the_best_and_counts_all() {
    let idx = index();
    let r = idx.search(&parse("a", &ctx()), &SearchOptions { limit: 2, sort: SortBy::Name });
    assert_eq!(r.hits.len(), 2);
    assert!(r.total > 2);
}

#[test]
fn unsupported_terms_warn_instead_of_failing() {
    let idx = index();
    // content terms are answered by the content index, never by names
    let r = idx.search(&parse("c:factura", &ctx()), &SearchOptions::default());
    assert_eq!(r.total, 0);
    assert!(r.warnings.is_empty());
    let r = idx.search(&parse("regex:(", &ctx()), &SearchOptions::default());
    assert_eq!(r.warnings.len(), 1);
    assert!(idx.search(&parse("", &ctx()), &SearchOptions::default()).hits.is_empty());
}

#[test]
fn files_in_cache_folders_rank_last() {
    let idx = index();
    let r = find(&idx, "plan.md");
    assert_eq!(r, vec![r"C:\proiecte\whereti\PLAN.md", r"C:\Users\ana\AppData\Local\PLAN.md"]);
}

#[test]
fn hidden_flag_is_kept() {
    let idx = index();
    let i = (0..idx.len() as u32).find(|&i| idx.name(i) == "raport.txt").unwrap();
    assert_ne!(idx.flags(i) & FLAG_HIDDEN, 0);
}

#[test]
fn snapshot_round_trip_and_damage_detection() {
    let idx = index();
    let dir = std::env::temp_dir().join(format!("whereti-test-{}", std::process::id()));
    let path = dir.join("names.idx");
    snapshot::save(&idx, &path).unwrap();
    let loaded = snapshot::load(&path).unwrap();
    assert_eq!(loaded, idx);
    assert_eq!(find(&loaded, "masina").len(), 1);

    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x55;
    std::fs::write(&path, &bytes).unwrap();
    assert!(snapshot::load(&path).is_err(), "flipped byte must be detected");

    std::fs::write(&path, &bytes[..mid]).unwrap();
    assert!(snapshot::load(&path).is_err(), "truncated file must be detected");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn records_entries_and_filters_for_the_content_index() {
    let idx = index();
    // every PDF/TXT under C:\Users\ana\Documents, unranked
    let q = parse(r#"path:"c:\users\ana\documents\" ext:pdf;txt"#, &ctx());
    let entries = idx.matching(&q);
    let mut paths: Vec<String> = entries.iter().map(|&e| idx.full_path(e)).collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            r"C:\Users\ana\Documents\Raport anual 2025.pdf",
            r"C:\Users\ana\Documents\factura_enel.PDF",
            r"C:\Users\ana\Documents\raport.txt",
            r"C:\Users\ana\Documents\shared.txt",
            r"C:\Users\ana\Documents\Școală\temă.txt",
        ]
    );
    // entry -> MFT record -> a live entry of the same record
    let records = idx.records_of(&entries);
    for (e, r) in entries.iter().zip(&records) {
        let r = r.expect("every live entry has a record");
        let back = idx.live_entry_of(0, r).expect("live");
        // a hard link may come back as the other name of the same file
        assert!(back == *e || idx.full_path(back).ends_with("shared.txt"));
    }
    assert_eq!(idx.live_entry_of(0, 999_999), None);
    assert_eq!(idx.live_entry_of(7, 41), None, "no such volume");
    // filter keeps the given order and applies the conditions
    let pdfs = idx.filter(&parse("ext:pdf", &ctx()), &entries);
    assert_eq!(pdfs.len(), 2);
    assert!(pdfs.iter().all(|&e| idx.full_path(e).to_lowercase().ends_with(".pdf")));
}

#[test]
fn generated_folders_are_left_out_and_build_folders_rank_last() {
    let c = scan(vec![
        d(5, 5, "."),
        d(20, 5, "proiect"),
        d(21, 20, "node_modules"),
        f(40, 21, "raport.js"),
        d(22, 20, ".git"),
        f(41, 22, "raport.pack"),
        d(23, 20, "Release"),
        f(42, 23, "raport.exe"),
        f(43, 20, "raport.docx"),
    ]);
    let vols = vec![VolumeScan { letter: 'E', serial: 3, journal_id: 1, next_usn: 1, scan: c }];
    let idx = build(vols, &Exclusions::defaults(None)).0;
    // node_modules and .git: gone; Release: still findable, but after the document
    assert_eq!(find(&idx, "raport"), [r"E:\proiect\raport.docx", r"E:\proiect\Release\raport.exe"]);
    // a saved index picks up the current noise list when loaded
    let dir = std::env::temp_dir().join(format!("wt-noise-{}.idx", std::process::id()));
    crate::snapshot::save(&idx, &dir).unwrap();
    let back = crate::snapshot::load(&dir).unwrap();
    let _ = std::fs::remove_file(&dir);
    assert_eq!(find(&back, "raport"), find(&idx, "raport"));
}
