//! Cross-check of the two readers: the direct MFT reader must see exactly
//! what the documented `FSCTL_ENUM_USN_DATA` API sees.

use std::collections::{HashMap, HashSet};

use crate::mft::MftScan;
use crate::parse::FRN_MASK;
use crate::usn_enum::UsnEntry;

#[derive(Debug, Default)]
pub struct Diff {
    pub count: usize,
    pub samples: Vec<String>,
}

impl Diff {
    fn add(&mut self, max: usize, sample: impl FnOnce() -> String) {
        self.count += 1;
        if self.samples.len() < max {
            self.samples.push(sample());
        }
    }
}

#[derive(Debug, Default)]
pub struct CompareReport {
    pub raw_records: usize,
    pub usn_records: usize,
    pub matched: usize,
    /// Records the raw reader found but the API did not list.
    pub only_raw: Diff,
    /// Records the API listed but the raw reader missed. Must be 0.
    pub only_usn: Diff,
    /// Same record, but the API's name is not among the raw names.
    pub name_mismatch: Diff,
    /// Same record and name, different parent.
    pub parent_mismatch: Diff,
    /// Same record, different sequence number (record reused).
    pub seq_mismatch: Diff,
    /// Differences caused by files changed after the direct scan started.
    /// Expected; the live index fixes them by replaying the USN journal.
    pub changed_during_scan: Diff,
}

impl CompareReport {
    /// True when the raw reader agrees with the API on every record the API knows.
    pub fn raw_covers_api(&self) -> bool {
        self.only_usn.count == 0
            && self.name_mismatch.count == 0
            && self.parent_mismatch.count == 0
            && self.seq_mismatch.count == 0
    }
}

/// Sequence number and (parent, name) links of one raw record.
type RawLinks<'a> = (u16, Vec<(u64, &'a str)>);

/// `scan_start_usn`: journal position read just before the direct scan. API
/// records with a newer USN changed during the scan and are reported apart.
pub fn compare(
    raw: &MftScan,
    usn: &[UsnEntry],
    scan_start_usn: Option<i64>,
    max_samples: usize,
) -> CompareReport {
    let mut by_record: HashMap<u64, RawLinks> = HashMap::with_capacity(raw.entries.len());
    for e in &raw.entries {
        by_record
            .entry(e.record)
            .or_insert_with(|| (e.seq, Vec::new()))
            .1
            .push((e.parent, raw.name(e)));
    }

    let mut r = CompareReport {
        raw_records: by_record.len(),
        ..Default::default()
    };
    let mut seen: HashSet<u64> = HashSet::with_capacity(usn.len());
    for u in usn {
        let rec = u.frn & FRN_MASK;
        let seq = (u.frn >> 48) as u16;
        let parent = u.parent_frn & FRN_MASK;
        if !seen.insert(rec) {
            continue;
        }
        if scan_start_usn.is_some_and(|start| u.usn >= start) {
            let unchanged = by_record.get(&rec).is_some_and(|(s, links)| {
                *s == seq && links.iter().any(|(p, n)| *n == u.name && *p == parent)
            });
            if unchanged {
                r.matched += 1;
            } else {
                r.changed_during_scan
                    .add(max_samples, || format!("#{rec} \"{}\" (usn {})", u.name, u.usn));
            }
            continue;
        }
        let Some((raw_seq, links)) = by_record.get(&rec) else {
            r.only_usn.add(max_samples, || format!("#{rec} \"{}\" (parent #{parent})", u.name));
            continue;
        };
        if *raw_seq != seq {
            r.seq_mismatch
                .add(max_samples, || format!("#{rec} \"{}\": api seq {seq}, raw seq {raw_seq}", u.name));
            continue;
        }
        // Hard links can share a name in different folders (System32 <-> WinSxS),
        // so the API's link must match on name AND parent.
        if links.iter().any(|(p, n)| *n == u.name && *p == parent) {
            r.matched += 1;
        } else if let Some((p, _)) = links.iter().find(|(_, n)| *n == u.name) {
            r.parent_mismatch.add(max_samples, || {
                format!("#{rec} \"{}\": api parent #{parent}, raw parent #{p}", u.name)
            });
        } else {
            r.name_mismatch.add(max_samples, || {
                let raw_names: Vec<&str> = links.iter().map(|(_, n)| *n).collect();
                format!("#{rec}: api \"{}\", raw {raw_names:?}", u.name)
            });
        }
    }
    r.usn_records = seen.len();

    let mut only_raw: Vec<(&u64, &RawLinks)> =
        by_record.iter().filter(|(rec, _)| !seen.contains(rec)).collect();
    only_raw.sort_unstable_by_key(|(rec, _)| **rec);
    for (rec, (_, links)) in only_raw {
        r.only_raw.add(max_samples, || format!("#{rec} \"{}\"", links[0].1));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mft::MftEntry;

    fn raw(items: &[(u64, u16, u64, &str)]) -> MftScan {
        let mut scan = MftScan::default();
        for &(record, seq, parent, name) in items {
            scan.push(MftEntry { record, seq, parent, ..Default::default() }, name);
        }
        scan
    }

    fn api(record: u64, seq: u16, parent: u64, name: &str) -> UsnEntry {
        UsnEntry {
            frn: ((seq as u64) << 48) | record,
            parent_frn: (1u64 << 48) | parent,
            name: name.into(),
            attributes: 0,
            usn: 100,
        }
    }

    #[test]
    fn detects_every_kind_of_difference() {
        let raw = raw(&[
            (10, 1, 5, "ok.txt"),
            (11, 1, 5, "a.txt"),
            (11, 1, 7, "hardlink.txt"),
            (12, 2, 5, "renamed.txt"),
            (13, 1, 5, "moved.txt"),
            (14, 1, 5, "metafile"),
            (15, 3, 5, "reused.txt"),
        ]);
        let usn = vec![
            api(10, 1, 5, "ok.txt"),
            api(11, 1, 7, "hardlink.txt"),
            api(12, 2, 5, "old.txt"),
            api(13, 1, 9, "moved.txt"),
            api(15, 4, 5, "reused.txt"),
            api(16, 1, 5, "missing.txt"),
        ];
        let r = compare(&raw, &usn, None, 10);
        assert_eq!(r.matched, 2);
        assert_eq!(r.name_mismatch.count, 1);
        assert_eq!(r.parent_mismatch.count, 1);
        assert_eq!(r.seq_mismatch.count, 1);
        assert_eq!(r.only_usn.count, 1);
        assert_eq!(r.only_raw.count, 1);
        assert!(!r.raw_covers_api());
    }

    #[test]
    fn hard_links_with_the_same_name_in_different_folders_match() {
        // C:\Windows\System32\x.dll and C:\Windows\WinSxS\...\x.dll are one record.
        let raw = raw(&[(20, 1, 100, "x.dll"), (20, 1, 200, "x.dll")]);
        let usn = vec![api(20, 1, 200, "x.dll")];
        let r = compare(&raw, &usn, None, 10);
        assert_eq!(r.matched, 1);
        assert!(r.raw_covers_api());
    }

    #[test]
    fn files_changed_during_the_scan_are_not_errors() {
        let raw = raw(&[(10, 1, 5, "ok.txt")]);
        let mut new_file = api(30, 1, 5, "qBittorrent-data.ini");
        new_file.usn = 500;
        let usn = vec![api(10, 1, 5, "ok.txt"), new_file];
        let r = compare(&raw, &usn, Some(400), 10);
        assert_eq!(r.matched, 1);
        assert_eq!(r.changed_during_scan.count, 1);
        assert!(r.raw_covers_api());
    }
}
