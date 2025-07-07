use std::collections::BTreeSet;
use std::path::Path;

use super::*;

/// Every `tr("…")`, `trf!("…")` and `trn!("…")` text in the app and the
/// installer, unescaped like the compiler does.
fn texts_in_code() -> Vec<(String, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut out = Vec::new();
    for dir in ["whereti-app/src", "whereti-setup/src"] {
        for f in std::fs::read_dir(crates.join(dir)).unwrap() {
            let path = f.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            for start in ["tr(", "trf!(", "trn!(", "tr_both("] {
                for (i, _) in src.match_indices(start) {
                    let before = src[..i].chars().last().unwrap_or(' ');
                    if before.is_alphanumeric() || before == '_' || before == '.' {
                        continue;
                    }
                    // the text may start on the next line
                    let after = src[i + start.len()..].trim_start();
                    let Some(literal) = after.strip_prefix('"') else { continue };
                    let mut text = String::new();
                    let mut chars = literal.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' => match chars.next() {
                                Some('n') => text.push('\n'),
                                Some('t') => text.push('\t'),
                                Some(o) => text.push(o),
                                None => {}
                            },
                            c => text.push(c),
                        }
                    }
                    let line = src[..i].lines().count();
                    out.push((text, format!("{}:{line}", path.display())));
                }
            }
        }
    }
    out
}

fn all() -> impl Iterator<Item = &'static Entry> {
    screens::ALL.iter().flat_map(|s| s.iter())
}

fn placeholders(s: &str) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        out.insert(&rest[i + 1..i + j]);
        rest = &rest[i + j + 1..];
    }
    out
}

#[test]
fn every_text_in_the_code_is_in_a_catalog() {
    let missing: Vec<String> = texts_in_code()
        .into_iter()
        .filter(|(t, _)| entry(t).is_none())
        .map(|(t, at)| format!("{at}: {t:?}"))
        .collect();
    assert!(
        missing.is_empty(),
        "add these to a catalog in screens/:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_catalog_text_is_used() {
    let used: BTreeSet<String> = texts_in_code().into_iter().map(|(t, _)| t).collect();
    let unused: Vec<&str> = all()
        .map(|e| e.en)
        .filter(|en| !used.contains(*en))
        .collect();
    assert!(
        unused.is_empty(),
        "no longer in the code, remove: {unused:?}"
    );
}

#[test]
fn no_text_twice() {
    let mut seen = BTreeSet::new();
    for e in all() {
        assert!(seen.insert(e.en), "twice in the catalogs: {:?}", e.en);
    }
}

#[test]
fn every_language_is_complete() {
    let mut missing = Vec::new();
    for e in all() {
        for l in Lang::ALL {
            let t = match l {
                Lang::De => e.de,
                Lang::En => e.en,
                Lang::Es => e.es,
                Lang::Fr => e.fr,
                Lang::It => e.it,
                Lang::Pl => e.pl,
                Lang::PtBr => e.pt_br,
                Lang::Ro => e.ro,
            };
            if t.trim().is_empty() {
                missing.push(format!("{}: {:?}", l.locale().code, e.en));
            }
        }
        assert!(
            !e.note.trim().is_empty(),
            "no note for translators: {:?}",
            e.en
        );
    }
    assert!(
        missing.is_empty(),
        "{} missing translations:\n{}",
        missing.len(),
        missing.join("\n")
    );
}

/// The same `{values}` in every language; a plural form may leave out `{n}`
/// ("un fichier"), but not every form.
#[test]
fn placeholders_match() {
    for e in all() {
        let en: BTreeSet<&str> = e.en.split('|').flat_map(placeholders).collect();
        for l in Lang::ALL {
            let t = e.get(l);
            let mut with_n = false;
            for form in t.split('|') {
                let mut p = placeholders(form);
                with_n |= p.remove("n");
                let mut want = en.clone();
                want.remove("n");
                assert_eq!(p, want, "{}: {:?} in {:?}", l.locale().code, t, e.en);
            }
            assert_eq!(
                with_n,
                en.contains("n"),
                "{}: {{n}} missing in {:?}",
                l.locale().code,
                t
            );
        }
    }
}

#[test]
fn plural_texts_have_every_form() {
    for e in all().filter(|e| e.en.contains('|')) {
        for l in Lang::ALL {
            let t = e.get(l);
            if t == e.en && l != Lang::En {
                continue; // not translated yet: English is shown, with its own rule
            }
            assert_eq!(
                t.split('|').count(),
                l.locale().plural_forms,
                "{}: {t:?}",
                l.locale().code
            );
        }
    }
    for e in all().filter(|e| !e.en.contains('|')) {
        for l in Lang::ALL {
            assert!(
                !e.get(l).contains('|'),
                "{}: a | in {:?}",
                l.locale().code,
                e.get(l)
            );
        }
    }
}

#[test]
fn no_long_dashes() {
    for e in all() {
        for l in Lang::ALL {
            let t = e.get(l);
            assert!(
                !t.contains('\u{2014}') && !t.contains('\u{2013}'),
                "{}: {t:?}",
                l.locale().code
            );
        }
    }
}

#[test]
fn plural_rules() {
    use Lang::*;
    let forms = |l, ns: &[u64]| ns.iter().map(|&n| form_index(l, n)).collect::<Vec<_>>();
    assert_eq!(forms(En, &[0, 1, 2, 5]), [1, 0, 1, 1]);
    assert_eq!(forms(Fr, &[0, 1, 2]), [0, 0, 1]);
    assert_eq!(forms(PtBr, &[0, 1, 2]), [0, 0, 1]);
    // 1 fișier, 2 fișiere, 19 fișiere, 20 de fișiere, 101 fișiere, 120 de fișiere
    assert_eq!(
        forms(Ro, &[1, 0, 2, 19, 20, 100, 101, 119, 120]),
        [0, 1, 1, 1, 2, 2, 1, 1, 2]
    );
    // 1 plik, 2 pliki, 5 plików, 12 plików, 22 pliki, 0 plików, 112 plików, 1001 plików
    assert_eq!(
        forms(Pl, &[1, 2, 4, 5, 12, 14, 22, 24, 25, 0, 112, 1001]),
        [0, 1, 1, 2, 2, 2, 1, 1, 2, 2, 2, 2]
    );
}

#[test]
fn numbers() {
    assert_eq!(number_in(Lang::En, 2_509_598), "2,509,598");
    assert_eq!(number_in(Lang::Ro, 2_509_598), "2.509.598");
    assert_eq!(number_in(Lang::De, 1234), "1.234");
    assert_eq!(number_in(Lang::Es, 1234), "1234");
    assert_eq!(number_in(Lang::Es, 12_345), "12.345");
    assert_eq!(number_in(Lang::Fr, 12_345), "12\u{a0}345");
    assert_eq!(number_in(Lang::Pl, 1234), "1234");
    assert_eq!(number_in(Lang::En, 999), "999");
}

#[test]
fn values_are_put_in() {
    assert_eq!(
        fill("{a} and {b}", &[("a", &1), ("b", &"two")]),
        "1 and two"
    );
    assert_eq!(fill("{unknown} {", &[]), "{unknown} {");
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set(Lang::Ro);
    let drives = "C:";
    assert_eq!(
        crate::trn!("{n} file on {drives}|{n} files on {drives}", 20, drives),
        "20 de fișiere pe C:"
    );
    assert_eq!(
        crate::trn!("{n} file on {drives}|{n} files on {drives}", 1, drives),
        "1 fișier pe C:"
    );
    set(Lang::En);
    assert_eq!(
        crate::trn!(
            "{n} file on {drives}|{n} files on {drives}",
            1500,
            drives = "D:"
        ),
        "1,500 files on D:"
    );
    assert_eq!(
        crate::trf!("checking {drives} again", drives),
        "checking C: again"
    );
}

#[test]
fn language_codes() {
    assert_eq!(Lang::from_code("ro-RO"), Some(Lang::Ro));
    assert_eq!(Lang::from_code("pt-PT"), Some(Lang::PtBr));
    assert_eq!(Lang::from_code("en-US"), Some(Lang::En));
    assert_eq!(Lang::from_code("ja-JP"), None);
    for l in Lang::ALL {
        assert_eq!(Lang::from_code(l.locale().code), Some(l));
        assert_eq!(l.locale().lang, l);
    }
    // alphabetical by their own names, as in Settings
    let names: Vec<&str> = LOCALES.iter().map(|l| l.native).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}
