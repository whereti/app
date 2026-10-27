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
    assert_eq!(r.total, r.hits.len() - r.close);
    r.hits[..r.total].iter().map(|&i| idx.full_path(i)).collect()
}

/// The close matches of `q` (see `SearchResults::close`).
fn close(idx: &NameIndex, q: &str) -> Vec<String> {
    let r = idx.search(&parse(q, &ctx()), &SearchOptions::default());
    r.hits[r.hits.len() - r.close..].iter().map(|&i| idx.full_path(i)).collect()
}

#[test]
fn a_word_one_letter_off_finds_close_matches_after_the_exact_ones() {
    let idx = index();
    // swapped, missing, extra and wrong letters
    for typo in ["facutra", "factra", "facttura", "fectura"] {
        assert!(find(&idx, typo).is_empty(), "{typo}");
        assert_eq!(close(&idx, typo), vec![r"C:\Users\ana\Documents\factura_enel.PDF"], "{typo}");
    }
    // the other conditions still hold
    assert!(close(&idx, "facutra ext:docx").is_empty());
    assert_eq!(close(&idx, "contarct ext:pdf"), vec![r"D:\Acte\contract.pdf"]);
    // 3 letters are not corrected, 4 are
    assert!(close(&idx, "pln").is_empty());
    assert_eq!(close(&idx, "plna").len(), 2);
    // enough exact hits: no close ones
    assert!(close(&idx, "txt").is_empty());
    // the word with the typo need not be the longest
    assert_eq!(close(&idx, "raport anaul"), vec![r"C:\Users\ana\Documents\Raport anual 2025.pdf"]);
    // close matches come after the exact ones and never repeat them
    let r = idx.search(&parse("raport", &ctx()), &SearchOptions::default());
    assert_eq!(r.hits.len(), r.total + r.close);
    let exact = &r.hits[..r.total];
    assert!(r.hits[r.total..].iter().all(|h| !exact.contains(h)));
}

#[test]
fn near_words_are_one_letter_off() {
    let v = search::near_words("abcd");
    assert!(v.contains(&"bacd".to_string()) && v.contains(&"abxcd".to_string()) && v.contains(&"abcx".to_string()));
    assert!(!v.contains(&"abcd".to_string()));
    assert!(!v.contains(&"abc".to_string()), "a missing letter only from 5 letters up");
    assert!(search::near_words("abcde").contains(&"abde".to_string()));
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

#[test]
fn importance_orders_results_inside_a_match_level() {
    use crate::index::{FLAG_HOME, FLAG_NOISE};
    let c = scan(vec![
        d(5, 5, "."),
        d(23, 5, "Users"),
        d(24, 23, "ana"),
        d(25, 24, "Documents"),
        f(40, 25, "reformatter.docx"),
        d(26, 24, "go"),
        d(27, 26, "pkg"),
        d(28, 27, "mod"),
        f(41, 28, "reformatter.go"),
        d(29, 24, ".antigravity"),
        f(42, 29, "reformatter.xml"),
        d(30, 5, "proiecte"),
        f(43, 30, "reformatter.dll"),
        f(44, 30, "reformatter.go"),
        d(31, 5, "Python313"),
        f(45, 31, "README"),
        f(46, 25, "README.md"),
        f(47, 25, "README notes.txt"),
    ]);
    let vols = vec![VolumeScan { letter: 'C', serial: 1, journal_id: 10, next_usn: 100, scan: c }];
    let idx = build(vols, &Exclusions::default()).0;
    let flags_of = |path: &str| {
        let i = (0..idx.len() as u32).find(|&i| idx.full_path(i) == path).unwrap_or_else(|| panic!("{path}"));
        idx.flags(i)
    };
    assert!(flags_of(r"C:\Users\ana\Documents\reformatter.docx") & FLAG_HOME != 0);
    assert!(flags_of(r"C:\Users\ana\go\pkg\mod\reformatter.go") & FLAG_NOISE != 0);
    assert!(flags_of(r"C:\Users\ana\.antigravity\reformatter.xml") & FLAG_NOISE != 0);
    assert_eq!(flags_of(r"C:\proiecte\reformatter.go") & (FLAG_HOME | FLAG_NOISE), 0);

    // every name has "formatter" inside a word: the same match level
    let r = find(&idx, "formatter");
    assert_eq!(
        r[..3],
        [r"C:\Users\ana\Documents\reformatter.docx", r"C:\proiecte\reformatter.go", r"C:\proiecte\reformatter.dll"],
        "{r:#?}"
    );
    assert_eq!(r.len(), 5);
    // a better match still wins over a more important place
    let r = find(&idx, "reformatter.go");
    assert_eq!(r[0], r"C:\proiecte\reformatter.go");
    // a file the user opens often rises, even out of a tool folder; a
    // used path that is not among the hits changes nothing
    let q = parse("formatter", &ctx());
    let opts = SearchOptions { limit: 100, sort: SortBy::Relevance };
    let used = [(r"C:\Users\ana\go\pkg\mod\reformatter.go".to_string(), 12), (r"C:\nowhere\reformatter.go".to_string(), 12)];
    let r = idx.search_with_usage(&q, &opts, &used);
    assert_eq!(idx.full_path(r.hits[0]), r"C:\Users\ana\go\pkg\mod\reformatter.go");
    assert_eq!(r.hits.len(), 5);
    // much use lifts a file above closer matches of the name
    // ("README notes.txt" only starts with "readme"; README.md is it exactly)
    let q = parse("readme", &ctx());
    let notes = r"C:\Users\ana\Documents\README notes.txt";
    let r = idx.search_with_usage(&q, &opts, &[(notes.to_string(), 2)]);
    assert_eq!(idx.full_path(r.hits[0]), r"C:\Users\ana\Documents\README.md", "a little use does not");
    let r = idx.search_with_usage(&q, &opts, &[(notes.to_string(), 8)]);
    assert_eq!(idx.full_path(r.hits[0]), notes);
    // a used file that does not match the query is never added
    let q = parse("reformatter.docx", &ctx());
    let used = [(r"C:\Users\ana\Documents\README notes.txt".to_string(), 12)];
    let r = idx.search_with_usage(&q, &opts, &used);
    assert_eq!(idx.full_path(r.hits[0]), r"C:\Users\ana\Documents\reformatter.docx");

    // "readme" is README.md exactly; a runtime at the root of C: is a tool folder
    let r = find(&idx, "readme");
    assert_eq!(
        r,
        [r"C:\Users\ana\Documents\README.md", r"C:\Users\ana\Documents\README notes.txt", r"C:\Python313\README"]
    );
}

#[test]
fn where_the_user_works_lately_counts() {
    let c = scan(vec![
        d(5, 5, "."),
        d(30, 5, "proiecte"),
        d(31, 30, "casa"),
        d(32, 31, "acte"),
        f(40, 31, "cerere.docx"),
        f(41, 32, "cerere vechi.docx"),
        d(33, 5, "altele"),
        f(42, 33, "cerere.docx"),
    ]);
    let vols = vec![VolumeScan { letter: 'C', serial: 1, journal_id: 10, next_usn: 100, scan: c }];
    let idx = build(vols, &Exclusions::default()).0;
    let at = |path: &str| (0..idx.len() as u32).find(|&i| idx.full_path(i) == path).unwrap();
    let (casa, here, below, other) =
        (at(r"C:\proiecte\casa"), at(r"C:\proiecte\casa\cerere.docx"), at(r"C:\proiecte\casa\acte\cerere vechi.docx"), at(r"C:\altele\cerere.docx"));
    let mut active = vec![0u8; idx.len()];
    active[casa as usize] = 8;
    let w = idx.weigher();
    let wa = idx.weigher_with(std::sync::Arc::new(active.clone()));
    assert!(idx.is_work_file(here) && !idx.is_work_file(casa));
    // in the folder: much more important; one level down: half; elsewhere: the same
    assert_eq!(idx.importance(here, &w) - idx.importance(here, &wa), 8);
    assert_eq!(idx.importance(below, &w) - idx.importance(below, &wa), 4);
    assert_eq!(idx.importance(other, &w), idx.importance(other, &wa));
    // the same name: the one where the user works comes first
    let q = parse("cerere.docx", &ctx());
    let opts = SearchOptions { limit: 10, sort: SortBy::Relevance };
    let r = idx.search_ranked(&q, &opts, &[], std::sync::Arc::new(active));
    assert_eq!(idx.full_path(r.hits[0]), r"C:\proiecte\casa\cerere.docx");
}

#[test]
fn files_a_program_makes_are_not_work() {
    use crate::build::looks_generated;
    assert!(looks_generated("945d0bda-b0c0-4d90-99ee-d6bb23acfd51.sslo"));
    assert!(looks_generated("00000000000000000348-770a40e9.op"));
    assert!(!looks_generated("Screenshot 2026-10-24 234004.png"));
    assert!(!looks_generated("Factura SOF0001-EXEMPLU.pdf"));
    let c = scan(vec![
        d(5, 5, "."),
        d(30, 5, "Silo"),
        f(40, 30, "945d0bda-b0c0-4d90-99ee-d6bb23acfd51.sslo"),
        f(41, 30, "notes.op"),
        f(42, 30, "Plan.docx"),
        f(43, 30, "main.rs"),
    ]);
    let vols = vec![VolumeScan { letter: 'C', serial: 1, journal_id: 10, next_usn: 100, scan: c }];
    let idx = build(vols, &Exclusions::default()).0;
    let work: Vec<&str> = (0..idx.len() as u32).filter(|&i| idx.is_work_file(i)).map(|i| idx.name(i)).collect();
    assert_eq!(work, ["Plan.docx", "main.rs"]);
}

#[test]
fn generated_folders_are_told_by_what_is_around_them() {
    let c = scan(vec![
        d(5, 5, "."),
        // Rust: target beside Cargo.toml goes; a "target" of one's own stays
        d(120, 5, "proj"),
        f(121, 120, "Cargo.toml"),
        d(122, 120, "target"),
        d(123, 122, "debug"),
        f(124, 123, "gone_app.exe"),
        d(125, 5, "sales"),
        d(126, 125, "target"),
        f(127, 126, "keep_target_2026.xlsx"),
        // a cache that marks itself
        d(130, 5, "blobs"),
        f(131, 130, "CACHEDIR.TAG"),
        f(132, 130, "gone_blob.bin"),
        // Go's module store, and Go code of one's own
        d(140, 5, "go"),
        d(141, 140, "pkg"),
        d(142, 141, "mod"),
        f(143, 142, "gone_mod.go"),
        d(144, 140, "src"),
        f(145, 144, "keep_main.go"),
        // Lightroom previews beside the catalog
        d(150, 5, "Pictures"),
        f(151, 150, "keep_Catalog.lrcat"),
        d(152, 150, "Catalog Previews.lrdata"),
        f(153, 152, "gone_preview.lrprev"),
        // .NET: bin beside a project file; a bin of one's own stays
        d(160, 5, "app"),
        f(161, 160, "App.csproj"),
        d(162, 160, "bin"),
        f(163, 162, "gone_App.dll"),
        d(164, 5, "tools"),
        d(165, 164, "bin"),
        f(166, 165, "keep_tool.exe"),
        // Unity: Library inside a project; a Library of books stays
        d(170, 5, "game"),
        d(171, 170, "ProjectSettings"),
        d(172, 170, "Library"),
        f(173, 172, "gone_asset.bin"),
        d(174, 5, "Library"),
        f(175, 174, "keep_book.pdf"),
    ]);
    let vols = vec![VolumeScan { letter: 'E', serial: 3, journal_id: 1, next_usn: 1, scan: c }];
    let ex = Exclusions::defaults(None);
    let idx = build(vols, &ex).0;
    assert_eq!(find(&idx, "gone_"), Vec::<String>::new());
    let mut kept = find(&idx, "keep_");
    kept.sort();
    assert_eq!(
        kept,
        [
            r"E:\Library\keep_book.pdf",
            r"E:\Pictures\keep_Catalog.lrcat",
            r"E:\go\src\keep_main.go",
            r"E:\sales\target\keep_target_2026.xlsx",
            r"E:\tools\bin\keep_tool.exe",
        ]
    );
    // what each rule left out, for Settings: target, debug and the program
    let count = |rule: &str| idx.skipped().iter().find(|(r, _)| r == rule).map(|(_, n)| *n);
    assert_eq!(count(r"**\target <Cargo.toml>"), Some(3));
    assert_eq!(count(r"**\<CACHEDIR.TAG>"), Some(3));
    assert_eq!(count(r"**\go\pkg\mod"), Some(2));
    assert_eq!(count(r"**\*.lrdata"), Some(2));
    // a compaction rebuilds from the index, where those are gone: the counts stay
    let again = idx.rebuild_with(Vec::new());
    assert_eq!(again.skipped(), idx.skipped());
}

#[test]
fn rules_that_look_around_parse_strictly() {
    use crate::build::Smart;
    assert_eq!(Smart::parse("<CACHEDIR.TAG>"), Some(Smart::Marker("cachedir.tag".into())));
    assert_eq!(Smart::parse("*.lrdata"), Some(Smart::Suffix(".lrdata".into())));
    assert_eq!(Smart::parse(r"go\pkg\mod"), Some(Smart::Tail(vec!["go".into(), "pkg".into(), "mod".into()])));
    assert_eq!(
        Smart::parse("bin <*.csproj|App.sln>"),
        Some(Smart::Beside { name: "bin".into(), manifests: vec!["*.csproj".into(), "app.sln".into()] })
    );
    for bad in ["node_modules", "*", "*lrdata", "<>", "bin <>", "bin <a*b>", r"a\\b", r"a\*", "<a|b>"] {
        assert_eq!(Smart::parse(bad), None, "{bad}");
    }
    for ok in [r"**\node_modules", r"**\<pyvenv.cfg>", r"**\target <Cargo.toml>", r"**\go\pkg\mod"] {
        assert!(Exclusions::is_valid_any_depth(ok), "{ok}");
    }
    for bad in [r"**\", r"**\a*b", r"**\bin <>"] {
        assert!(!Exclusions::is_valid_any_depth(bad), "{bad}");
    }
}

#[test]
fn app_internals_go_only_below_appdata() {
    let c = scan(vec![
        d(5, 5, "."),
        d(110, 5, "Users"),
        d(111, 110, "ana"),
        d(112, 111, "AppData"),
        d(113, 112, "Local"),
        d(114, 112, "Roaming"),
        // an Electron app's cache, below AppData
        d(120, 113, "Slack"),
        d(121, 120, "Cache"),
        f(122, 121, "gone_cache.bin"),
        // a Store app's cache: Packages\<any app>\LocalCache
        d(130, 113, "Packages"),
        d(131, 130, "Contoso.App_8wekyb3d8bbwe"),
        d(132, 131, "LocalCache"),
        f(133, 132, "gone_store.dat"),
        // an editor's local history
        d(140, 114, "Code"),
        d(141, 140, "User"),
        d(142, 141, "History"),
        f(143, 142, "gone_history.json"),
        // temporary files
        d(150, 113, "Temp"),
        f(151, 150, "gone_tmp.txt"),
        // the same names among one's own files stay
        d(160, 111, "Documents"),
        d(161, 160, "Cache"),
        f(162, 161, "keep_cache_notes.docx"),
        d(163, 160, "User"),
        d(164, 163, "History"),
        f(165, 164, "keep_history_essay.docx"),
        d(166, 160, "logs"),
        f(167, 166, "keep_ship_logs.xlsx"),
    ]);
    let vols = vec![VolumeScan { letter: 'C', serial: 3, journal_id: 1, next_usn: 1, scan: c }];
    let idx = build(vols, &Exclusions::defaults(None)).0;
    assert_eq!(find(&idx, "gone_"), Vec::<String>::new());
    let mut kept = find(&idx, "keep_");
    kept.sort();
    assert_eq!(
        kept,
        [
            r"C:\Users\ana\Documents\Cache\keep_cache_notes.docx",
            r"C:\Users\ana\Documents\User\History\keep_history_essay.docx",
            r"C:\Users\ana\Documents\logs\keep_ship_logs.xlsx",
        ]
    );
}


#[test]
fn a_folder_two_rules_catch_counts_once() {
    // a Rust target: beside Cargo.toml and marked with CACHEDIR.TAG
    let c = scan(vec![
        d(5, 5, "."),
        d(100, 5, "proj"),
        f(101, 100, "Cargo.toml"),
        d(102, 100, "target"),
        f(103, 102, "CACHEDIR.TAG"),
        d(104, 102, "debug"),
        f(105, 104, "app.exe"),
    ]);
    let vols = vec![VolumeScan { letter: 'D', serial: 4, journal_id: 1, next_usn: 1, scan: c }];
    let idx = build(vols, &Exclusions::defaults(None)).0;
    let total: u64 = idx.skipped().iter().map(|(_, n)| n).sum();
    assert_eq!(total, 4, "target, its tag, debug and the program, once");
}

#[test]
fn a_package_inside_node_modules_counts_for_node_modules() {
    let c = scan(vec![
        d(5, 5, "."),
        d(100, 5, "web"),
        f(101, 100, "package.json"),
        d(102, 100, "node_modules"),
        d(103, 102, "left-pad"),
        f(104, 103, "package.json"),
        d(105, 103, "dist"),
        f(106, 105, "index.js"),
    ]);
    let vols = vec![VolumeScan { letter: 'D', serial: 4, journal_id: 1, next_usn: 1, scan: c }];
    let idx = build(vols, &Exclusions::defaults(None)).0;
    assert_eq!(idx.skipped(), vec![(r"**\node_modules".to_string(), 5)]);
}
