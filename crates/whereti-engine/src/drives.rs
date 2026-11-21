//! The drives whereti has seen, and whether each one is searched.
//!
//! Settings lists them, with a box to leave one out (a backup disk connected
//! only to back up). A drive connected for the first time after whereti was
//! set up is not read until the user says (the search window asks once);
//! the drives there when whereti was set up are searched, as before.
//!
//! A drive is known by its volume serial number, not its letter: an external
//! disk can come back as another letter. Drives made by a program (Google
//! Drive's) are not listed: the service does not choose for them.

use std::path::PathBuf;

use parking_lot::Mutex;
use whereti_ipc::{DriveChoice, DriveEntry};

use crate::walk::Drive;

#[derive(Debug, Clone, PartialEq)]
struct Known {
    serial: u32,
    choice: DriveChoice,
    letter: char,
    label: String,
    size: u64,
}

struct Inner {
    known: Vec<Known>,
    /// No list saved yet (a new install, or an earlier version): the drives
    /// seen now are searched, without asking.
    fresh: bool,
    /// The serial numbers of the drives connected at the last look.
    connected: Vec<u32>,
}

pub struct Drives {
    path: PathBuf,
    system: char,
    inner: Mutex<Inner>,
}

impl Drives {
    /// The list kept at `path` (one drive per line). `system`: the drive
    /// Windows runs from, always searched.
    pub fn open(path: PathBuf, system: char) -> Drives {
        let (known, fresh) = match std::fs::read_to_string(&path) {
            Ok(text) => (parse(&text), false),
            Err(_) => (Vec::new(), true),
        };
        Drives { path, system: system.to_ascii_uppercase(), inner: Mutex::new(Inner { known, fresh, connected: Vec::new() }) }
    }

    /// The drives an index loaded from disk covers: searched, when no list
    /// was saved yet (an external disk unplugged while an earlier version
    /// was updated is not asked about when it comes back).
    pub fn adopt(&self, drives: impl IntoIterator<Item = (char, u64)>) {
        let mut inner = self.inner.lock();
        if !inner.fresh {
            return;
        }
        for (letter, serial) in drives {
            let serial = serial as u32;
            if !inner.known.iter().any(|k| k.serial == serial) {
                inner.known.push(Known { serial, choice: DriveChoice::Search, letter, label: String::new(), size: 0 });
            }
        }
    }

    /// Of the drives present, those to index; remembers the new ones.
    pub fn usable(&self, present: Vec<Drive>) -> Vec<Drive> {
        let mut inner = self.inner.lock();
        let mut changed = inner.fresh;
        let mut connected = Vec::new();
        let mut out = Vec::new();
        for d in present {
            if d.by_program {
                out.push(d);
                continue;
            }
            let serial = d.serial as u32;
            let system = d.letter.to_ascii_uppercase() == self.system;
            connected.push(serial);
            let i = match inner.known.iter().position(|k| k.serial == serial) {
                Some(i) => i,
                None => {
                    let choice = if inner.fresh || system { DriveChoice::Search } else { DriveChoice::Ask };
                    inner.known.push(Known { serial, choice, letter: d.letter, label: String::new(), size: 0 });
                    changed = true;
                    inner.known.len() - 1
                }
            };
            let k = &mut inner.known[i];
            if system && k.choice != DriveChoice::Search {
                k.choice = DriveChoice::Search;
                changed = true;
            }
            if k.letter != d.letter || k.label != d.label || k.size != d.size {
                (k.letter, k.label, k.size) = (d.letter, d.label.clone(), d.size);
                changed = true;
            }
            if k.choice == DriveChoice::Search {
                out.push(d);
            }
        }
        inner.connected = connected;
        inner.fresh = false;
        if changed {
            self.save(&inner.known);
        }
        out
    }

    /// Is this drive still to be searched? A scan asks it as it goes: a
    /// drive left out meanwhile stops being read at once.
    pub fn wanted(&self, d: &Drive) -> bool {
        let serial = d.serial as u32;
        d.by_program || self.inner.lock().known.iter().find(|k| k.serial == serial).is_none_or(|k| k.choice == DriveChoice::Search)
    }

    /// Search a drive (`true`) or leave it out. The system drive is always
    /// searched.
    pub fn set(&self, serial: u32, search: bool) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let system = self.system;
        let Some(k) = inner.known.iter_mut().find(|k| k.serial == serial) else {
            return Err("this drive is not known".into());
        };
        if !search && k.letter.to_ascii_uppercase() == system {
            return Err("the drive Windows runs from is always searched".into());
        }
        k.choice = if search { DriveChoice::Search } else { DriveChoice::LeaveOut };
        let known = inner.known.clone();
        self.save(&known);
        Ok(())
    }

    /// For Settings: the drives connected now, then the ones left out while
    /// away (to put back before connecting them).
    pub fn entries(&self) -> Vec<DriveEntry> {
        let inner = self.inner.lock();
        let entry = |k: &Known, connected: bool| DriveEntry {
            serial: k.serial,
            letter: k.letter,
            label: k.label.clone(),
            size: k.size,
            connected,
            system: k.letter.to_ascii_uppercase() == self.system && connected,
            choice: k.choice,
        };
        let mut out: Vec<DriveEntry> =
            inner.known.iter().filter(|k| inner.connected.contains(&k.serial)).map(|k| entry(k, true)).collect();
        out.sort_by_key(|e| e.letter);
        out.extend(
            inner
                .known
                .iter()
                .filter(|k| !inner.connected.contains(&k.serial) && k.choice == DriveChoice::LeaveOut)
                .map(|k| entry(k, false)),
        );
        out
    }

    fn save(&self, known: &[Known]) {
        let text: String = known
            .iter()
            .map(|k| {
                let choice = match k.choice {
                    DriveChoice::Search => "search",
                    DriveChoice::LeaveOut => "out",
                    DriveChoice::Ask => "ask",
                };
                // a label has no tab or line break in it
                let label: String = k.label.chars().filter(|c| !c.is_control()).collect();
                format!("{:08x}\t{choice}\t{}\t{}\t{label}\n", k.serial, k.letter, k.size)
            })
            .collect();
        let tmp = self.path.with_extension("drives.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn parse(text: &str) -> Vec<Known> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(5, '\t');
            let serial = u32::from_str_radix(parts.next()?, 16).ok()?;
            let choice = match parts.next()? {
                "search" => DriveChoice::Search,
                "out" => DriveChoice::LeaveOut,
                "ask" => DriveChoice::Ask,
                _ => return None,
            };
            let letter = parts.next()?.chars().next()?;
            let size = parts.next()?.parse().ok()?;
            let label = parts.next().unwrap_or("").to_string();
            Some(Known { serial, choice, letter, label, size })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(letter: char, serial: u64, label: &str) -> Drive {
        Drive {
            letter,
            fs: "NTFS".into(),
            serial,
            removable: false,
            label: label.into(),
            size: 1 << 40,
            by_program: false,
        }
    }

    fn file(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("whereti-drives-{name}-{}.drives", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn letters(list: &[Drive]) -> String {
        list.iter().map(|d| d.letter).collect()
    }

    #[test]
    fn the_drives_there_at_first_are_searched_and_a_new_one_is_asked_about() {
        let path = file("first");
        let drives = Drives::open(path.clone(), 'C');
        let first = drives.usable(vec![drive('C', 1, "Windows"), drive('D', 2, "Data")]);
        assert_eq!(letters(&first), "CD");
        // a backup disk connected later waits for an answer
        let later = drives.usable(vec![drive('C', 1, "Windows"), drive('D', 2, "Data"), drive('E', 3, "Backup")]);
        assert_eq!(letters(&later), "CD");
        let asked = drives.entries();
        assert_eq!(asked.iter().find(|e| e.serial == 3).map(|e| e.choice), Some(DriveChoice::Ask));
        // and is remembered after a restart
        let again = Drives::open(path.clone(), 'C');
        assert_eq!(letters(&again.usable(vec![drive('C', 1, ""), drive('E', 3, "Backup")])), "C");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_drive_left_out_stays_out_under_another_letter_and_while_away() {
        let path = file("out");
        let drives = Drives::open(path.clone(), 'C');
        drives.usable(vec![drive('C', 1, ""), drive('E', 3, "Backup")]);
        drives.set(3, false).unwrap();
        // back as F:, an NTFS scan's 64-bit serial has the same low half
        assert_eq!(letters(&drives.usable(vec![drive('C', 1, ""), drive('F', 0x77_0000_0003, "Backup")])), "C");
        // unplugged: still listed, to be put back
        drives.usable(vec![drive('C', 1, "")]);
        let away = drives.entries();
        assert!(away.iter().any(|e| e.serial == 3 && !e.connected && e.choice == DriveChoice::LeaveOut));
        drives.set(3, true).unwrap();
        assert_eq!(letters(&drives.usable(vec![drive('C', 1, ""), drive('E', 3, "")])), "CE");
        // a scan going on asks as it reads
        assert!(drives.wanted(&drive('E', 3, "")));
        drives.set(3, false).unwrap();
        assert!(!drives.wanted(&drive('E', 3, "")));
        assert!(drives.wanted(&drive('Z', 42, "never seen")));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_windows_drive_cannot_be_left_out() {
        let path = file("system");
        let drives = Drives::open(path.clone(), 'C');
        drives.usable(vec![drive('C', 1, "")]);
        assert!(drives.set(1, false).is_err());
        assert!(drives.set(9, false).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drives_of_an_earlier_index_are_searched_when_they_come_back() {
        let path = file("adopt");
        let drives = Drives::open(path.clone(), 'C');
        // unplugged while an earlier version was updated
        drives.adopt([('C', 1), ('E', 0x99_0000_0003)]);
        drives.usable(vec![drive('C', 1, "")]);
        assert_eq!(letters(&drives.usable(vec![drive('C', 1, ""), drive('E', 3, "")])), "CE");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drives_made_by_a_program_are_not_listed() {
        let path = file("program");
        let drives = Drives::open(path.clone(), 'C');
        drives.usable(vec![drive('C', 1, "")]);
        let g = Drive { by_program: true, ..drive('G', 7, "Google Drive") };
        assert_eq!(letters(&drives.usable(vec![drive('C', 1, ""), g])), "CG");
        assert!(drives.entries().iter().all(|e| e.serial != 7));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_list_reads_back_what_was_saved() {
        let text = "00000003\tout\tE\t1099511627776\tBackup 14TB\n0000000a\tsearch\tD\t0\t\nbad line\n";
        let known = parse(text);
        assert_eq!(known.len(), 2);
        assert_eq!(known[0].label, "Backup 14TB");
        assert_eq!(known[0].choice, DriveChoice::LeaveOut);
        assert_eq!(known[1].serial, 10);
    }
}
