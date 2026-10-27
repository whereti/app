//! Drives without an NTFS journal: USB sticks and SD cards (FAT, exFAT),
//! ReFS, and any NTFS drive whose MFT cannot be read (PLAN.md §4.1, §7.2).
//!
//! Their folders are walked, and the result is shaped like an MFT scan
//! (made-up record numbers, root = record 5), so building, searching and
//! saving the index stay the same for every drive.
//!
//! Nothing is kept open on these drives between walks, so a USB stick can be
//! removed safely at any time. Changes are found by walking again now and
//! then (see the engine); a walk that finds nothing new changes nothing.

use std::hash::{Hash, Hasher};
use std::io;

use whereti_core::VolumeScan;
use whereti_ntfs::mft::{MftEntry, MftScan};

/// `VolumeInfo::journal_id` of a walked drive: it has no journal to replay.
pub const WALKED: u64 = u64::MAX;

const ROOT_RECORD: u64 = 5;
const FIRST_RECORD: u64 = 16;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;

/// A drive whereti can index right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    pub letter: char,
    /// "NTFS", "exFAT", "FAT32", "ReFS", …
    pub fs: String,
    pub serial: u64,
    /// A USB stick or a memory card (not an external hard disk: those count as fixed).
    pub removable: bool,
}

impl Drive {
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Fixed and removable drives that are ready (a card reader without a card
/// is left out). Network drives and CD/DVD drives are not indexed.
pub fn drives() -> Vec<Drive> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i) as char;
        let root = wide(&format!("{letter}:\\"));
        // SAFETY: NUL-terminated string.
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
            continue;
        }
        let mut serial = 0u32;
        let mut fs = [0u16; 64];
        // SAFETY: buffers and lengths match; unused outputs are null.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // no media, or not ready
        }
        let end = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        out.push(Drive {
            letter,
            fs: String::from_utf16_lossy(&fs[..end]),
            serial: serial as u64,
            removable: kind == DRIVE_REMOVABLE,
        });
    }
    out
}

/// Walks every folder of a drive. Junctions and other reparse points are
/// listed but not entered, so a loop can never make it walk forever.
pub fn walk(drive: &Drive) -> io::Result<VolumeScan> {
    let scan = walk_tree(&format!(r"\\?\{}:", drive.letter))?;
    Ok(VolumeScan { letter: drive.letter, serial: drive.serial, journal_id: WALKED, next_usn: 0, scan })
}

/// Walks the tree under `root` (no trailing backslash) into an MFT-shaped scan.
fn walk_tree(root: &str) -> io::Result<MftScan> {
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FIND_FIRST_EX_LARGE_FETCH,
        WIN32_FIND_DATAW,
    };

    let mut scan = MftScan::default();
    scan.push(MftEntry { record: ROOT_RECORD, parent: ROOT_RECORD, is_dir: true, ..Default::default() }, ".");
    let mut next = FIRST_RECORD;
    // (folder path without the trailing backslash, its record)
    let mut stack: Vec<(String, u64)> = vec![(root.to_string(), ROOT_RECORD)];
    let mut root_ok = false;
    while let Some((dir, record)) = stack.pop() {
        let pattern = wide(&format!(r"{dir}\*"));
        // SAFETY: an all-zero WIN32_FIND_DATAW is valid; the API fills it.
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: NUL-terminated pattern, data outlives the call.
        let h = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                &mut data as *mut _ as *mut _,
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: no arguments.
            let err = unsafe { GetLastError() };
            if record == ROOT_RECORD {
                // the whole drive is unreadable (removed, or locked): say so
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            // a folder we may not read, or one that vanished mid-walk, is left out
            continue;
        }
        if record == ROOT_RECORD {
            root_ok = true;
        }
        loop {
            let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
            let name = String::from_utf16_lossy(&data.cFileName[..len]);
            if name != "." && name != ".." {
                let is_dir = data.dwFileAttributes & ATTR_DIRECTORY != 0;
                let id = next;
                next += 1;
                scan.push(
                    MftEntry {
                        record: id,
                        parent: record,
                        size: if is_dir { 0 } else { (data.nFileSizeHigh as u64) << 32 | data.nFileSizeLow as u64 },
                        modified: (data.ftLastWriteTime.dwHighDateTime as u64) << 32
                            | data.ftLastWriteTime.dwLowDateTime as u64,
                        attributes: data.dwFileAttributes,
                        is_dir,
                        ..Default::default()
                    },
                    &name,
                );
                if is_dir && data.dwFileAttributes & ATTR_REPARSE_POINT == 0 {
                    stack.push((format!(r"{dir}\{name}"), id));
                }
            }
            // SAFETY: valid find handle and data.
            if unsafe { FindNextFileW(h, &mut data) } == 0 {
                break;
            }
        }
        // SAFETY: handle from FindFirstFileExW.
        unsafe { FindClose(h) };
    }
    if !root_ok {
        return Err(io::Error::other("the drive could not be read"));
    }
    Ok(scan)
}

/// Two serial numbers of the same volume. An MFT scan keeps NTFS's 64-bit
/// serial, `drives()` the 32-bit one Windows shows, which is its low half.
pub fn same_volume(a: u64, b: u64) -> bool {
    a as u32 == b as u32
}

/// A fingerprint of a walk: equal fingerprints mean nothing changed, so the
/// index is left alone.
pub fn fingerprint(scan: &VolumeScan) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    scan.serial.hash(&mut h);
    for e in &scan.scan.entries {
        // A folder's date changes with what is in it, which the entries
        // below already show; leaving it out avoids rebuilding the index for
        // a date Windows updates a moment after a file was written.
        let modified = if e.is_dir { 0 } else { e.modified };
        (e.record, e.parent, e.size, modified, e.attributes).hash(&mut h);
        scan.scan.name(e).hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("whereti-walk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Photos").join("2025")).unwrap();
        std::fs::write(root.join("notes.txt"), "hello").unwrap();
        std::fs::write(root.join("Photos").join("2025").join("beach ă.jpg"), [0u8; 10]).unwrap();
        root
    }

    #[test]
    fn walks_a_folder_tree_like_an_mft_scan() {
        let root = tree("tree");
        let scan = walk_tree(root.to_str().unwrap()).unwrap();
        let by_name = |n: &str| scan.entries.iter().find(|e| scan.name(e) == n).copied().unwrap();
        assert_eq!(scan.name(&scan.entries[0]), ".");
        assert_eq!(scan.entries[0].record, ROOT_RECORD);
        let photos = by_name("Photos");
        let y2025 = by_name("2025");
        let beach = by_name("beach ă.jpg");
        assert!(photos.is_dir && y2025.is_dir && !beach.is_dir);
        assert_eq!(photos.parent, ROOT_RECORD);
        assert_eq!(y2025.parent, photos.record);
        assert_eq!(beach.parent, y2025.record);
        assert_eq!(beach.size, 10);
        assert_eq!(by_name("notes.txt").size, 5);
        assert!(beach.modified > 0);
        assert_eq!(scan.entries.len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn serials_of_one_volume_match_in_both_sizes() {
        assert!(same_volume(0x1122_3344_5566_7788, 0x5566_7788));
        assert!(same_volume(0x5566_7788, 0x5566_7788));
        assert!(!same_volume(0x1122_3344_5566_7788, 0x5566_7789));
    }

    #[test]
    fn a_change_changes_the_fingerprint() {
        let root = tree("change");
        let walk = |p: &std::path::Path| VolumeScan {
            letter: 'E',
            serial: 1,
            journal_id: WALKED,
            next_usn: 0,
            scan: walk_tree(p.to_str().unwrap()).unwrap(),
        };
        let a = fingerprint(&walk(&root));
        assert_eq!(a, fingerprint(&walk(&root)));
        std::fs::write(root.join("new.docx"), "x").unwrap();
        assert_ne!(a, fingerprint(&walk(&root)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_system_drive_is_listed() {
        let sys = std::env::var("SystemDrive").unwrap_or("C:".into()).chars().next().unwrap();
        assert!(drives().iter().any(|d| d.letter.eq_ignore_ascii_case(&sys) && !d.removable));
    }

    #[test]
    fn a_missing_root_is_an_error() {
        assert!(walk_tree(r"C:\whereti-does-not-exist-123").is_err());
    }
}
