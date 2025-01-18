//! Reading the USN change journal and single MFT records (PLAN.md §4.4).

use std::io;

use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_FILE_RECORD, FSCTL_READ_USN_JOURNAL};

use crate::parse::{self, u16_at, u32_at, u64_at, utf16_le, ParseOutcome, RecordInfo, FRN_MASK};
use crate::volume::{as_bytes_mut, Volume};

/// One journal record: something happened to this file.
#[derive(Debug, Clone)]
pub struct UsnChange {
    /// MFT record number (sequence stripped).
    pub record: u64,
    pub parent: u64,
    pub usn: i64,
    /// USN_REASON_* flags.
    pub reason: u32,
    pub name: String,
}

pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

#[derive(Debug)]
pub enum JournalError {
    /// The records we need were overwritten (too many changes while we were
    /// away). The index must be rebuilt by a full scan.
    Overwritten,
    /// The journal was deleted or recreated (different id) or is not active.
    Replaced,
    Io(io::Error),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Overwritten => write!(f, "journal entries were overwritten"),
            JournalError::Replaced => write!(f, "journal was deleted or recreated"),
            JournalError::Io(e) => write!(f, "{e}"),
        }
    }
}

const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
const ERROR_INVALID_PARAMETER: i32 = 87;

/// Mirror of `READ_USN_JOURNAL_DATA_V0`.
#[repr(C)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
}

/// Reads journal records from `start`. With `wait`, the call sleeps in the
/// kernel until at least one new record exists (no polling, no CPU).
/// Returns the records and the position to continue from.
pub fn read_journal(
    vol: &Volume,
    journal_id: u64,
    start: i64,
    wait: bool,
    buf: &mut Vec<u8>,
) -> Result<(Vec<UsnChange>, i64), JournalError> {
    let input = ReadUsnJournalDataV0 {
        start_usn: start,
        reason_mask: 0xFFFF_FFFF,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for: if wait { 1 } else { 0 },
        usn_journal_id: journal_id,
    };
    // SAFETY: plain repr(C) struct.
    let in_bytes = unsafe {
        std::slice::from_raw_parts(
            &input as *const ReadUsnJournalDataV0 as *const u8,
            size_of::<ReadUsnJournalDataV0>(),
        )
    };
    if buf.len() < 64 * 1024 {
        buf.resize(1 << 20, 0);
    }
    let n = vol.ioctl(FSCTL_READ_USN_JOURNAL, in_bytes, buf).map_err(|e| match e.raw_os_error() {
        Some(ERROR_JOURNAL_ENTRY_DELETED) => JournalError::Overwritten,
        Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS | ERROR_INVALID_PARAMETER) => {
            JournalError::Replaced
        }
        _ => JournalError::Io(e),
    })?;
    if n < 8 {
        return Ok((Vec::new(), start));
    }
    let next = u64_at(buf, 0).unwrap_or(start as u64) as i64;
    Ok((parse_changes(&buf[8..n]), next))
}

fn parse_changes(b: &[u8]) -> Vec<UsnChange> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let Some(len) = u32_at(b, pos) else { break };
        let len = len as usize;
        if len < 8 || pos + len > b.len() {
            break;
        }
        let r = &b[pos..pos + len];
        let parsed = match u16_at(r, 4) {
            Some(2) => (|| {
                Some(UsnChange {
                    record: u64_at(r, 8)? & FRN_MASK,
                    parent: u64_at(r, 16)? & FRN_MASK,
                    usn: u64_at(r, 24)? as i64,
                    reason: u32_at(r, 40)?,
                    name: name_at(r, 56)?,
                })
            })(),
            Some(3) => (|| {
                Some(UsnChange {
                    record: u64_at(r, 8)? & FRN_MASK,
                    parent: u64_at(r, 24)? & FRN_MASK,
                    usn: u64_at(r, 40)? as i64,
                    reason: u32_at(r, 56)?,
                    name: name_at(r, 72)?,
                })
            })(),
            _ => None,
        };
        out.extend(parsed);
        pos += len;
    }
    out
}

fn name_at(r: &[u8], len_ofs: usize) -> Option<String> {
    let name_len = u16_at(r, len_ofs)? as usize;
    let name_ofs = u16_at(r, len_ofs + 2)? as usize;
    Some(utf16_le(r.get(name_ofs..name_ofs + name_len)?))
}

/// Mirror of `NTFS_FILE_RECORD_INPUT_BUFFER`.
#[repr(C)]
#[derive(Default)]
struct FileRecordInput {
    file_reference_number: u64,
}

/// Reads the current base record `record`, including names and size stored in
/// its extension records. `Ok(None)` if it is not in use.
pub fn read_record(vol: &Volume, record: u64, buf: &mut Vec<u8>) -> io::Result<Option<RecordInfo>> {
    let Some(mut info) = read_one(vol, record, buf)?.filter(RecordInfo::is_base) else { return Ok(None) };
    for ext in std::mem::take(&mut info.ext_records) {
        match read_one(vol, ext, buf)? {
            Some(e) if e.base == record => {
                info.names.extend(e.names);
                if info.data_size.is_none() {
                    info.data_size = e.data_size;
                }
                if info.std.is_none() {
                    info.std = e.std;
                }
            }
            // The list changed under us; the next journal record re-reads it.
            _ => info.attr_list_nonresident = true,
        }
    }
    Ok(Some(info))
}

/// Reads one MFT record (base or extension) as stored now.
fn read_one(vol: &Volume, record: u64, buf: &mut Vec<u8>) -> io::Result<Option<RecordInfo>> {
    let rs = vol.ntfs.bytes_per_file_record_segment as usize;
    if buf.len() < rs + 16 {
        buf.resize(rs + 16, 0);
    }
    let mut input = FileRecordInput { file_reference_number: record };
    // SAFETY: plain repr(C) struct.
    let in_bytes = unsafe { as_bytes_mut(&mut input) };
    let n = vol.ioctl(FSCTL_GET_NTFS_FILE_RECORD, in_bytes, buf)?;
    // NTFS_FILE_RECORD_OUTPUT_BUFFER: reference (8), length (4), record bytes.
    // For a free record the call returns the nearest lower record in use.
    let got = u64_at(buf, 0).unwrap_or(u64::MAX) & FRN_MASK;
    if got != record || n < 12 {
        return Ok(None);
    }
    let len = (u32_at(buf, 8).unwrap_or(0) as usize).min(n - 12);
    let mut rec = buf[12..12 + len].to_vec();
    let mut copy = rec.clone();
    // The record may come with or without its update-sequence fixups applied.
    Ok(match parse::parse_record(record, &mut rec, false) {
        ParseOutcome::Record(info) => Some(info),
        ParseOutcome::Unused => None,
        ParseOutcome::Corrupt => match parse::parse_record_fixed(record, &mut copy) {
            ParseOutcome::Record(info) => Some(info),
            ParseOutcome::Unused => None,
            ParseOutcome::Corrupt => return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt MFT record")),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(record: u64, parent: u64, usn: i64, reason: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + units.len() * 2 + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&((3u64 << 48) | record).to_le_bytes());
        r[16..24].copy_from_slice(&((1u64 << 48) | parent).to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[56..58].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + 2 * i..62 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_journal_records() {
        let mut b = v2(100, 5, 4096, USN_REASON_FILE_CREATE, "nou.txt");
        b.extend(v2(100, 5, 4200, USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE, "redenumit.txt"));
        let c = parse_changes(&b);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].record, 100);
        assert_eq!(c[0].parent, 5);
        assert_eq!(c[0].reason, USN_REASON_FILE_CREATE);
        assert_eq!(c[1].usn, 4200);
        assert_eq!(c[1].name, "redenumit.txt");
    }

    #[test]
    fn struct_sizes_match_windows_headers() {
        assert_eq!(size_of::<ReadUsnJournalDataV0>(), 40);
        assert_eq!(size_of::<FileRecordInput>(), 8);
    }
}
