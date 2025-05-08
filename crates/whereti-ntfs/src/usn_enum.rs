//! Enumeration through `FSCTL_ENUM_USN_DATA` - the documented API that
//! Everything uses. It returns name, parent and attributes, but no size or dates.

use std::io;

use windows_sys::Win32::System::Ioctl::FSCTL_ENUM_USN_DATA;

use crate::parse::{u16_at, u32_at, u64_at, utf16_le};
use crate::volume::Volume;

const ERROR_HANDLE_EOF: i32 = 38;
const BUFFER_SIZE: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct UsnEntry {
    /// Full file reference: record number (low 48 bits) + sequence (high 16).
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub attributes: u32,
    /// USN of the last change to this file.
    pub usn: i64,
}

/// Mirror of `MFT_ENUM_DATA_V0`.
#[repr(C)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

/// Lists every in-use file and directory on the volume.
pub fn enumerate(vol: &Volume) -> io::Result<Vec<UsnEntry>> {
    let high_usn = vol.query_usn_journal().map(|j| j.next_usn).unwrap_or(i64::MAX);
    let mut input = MftEnumDataV0 { start_file_reference_number: 0, low_usn: 0, high_usn };
    let mut buf = vec![0u8; BUFFER_SIZE];
    let mut out = Vec::new();
    loop {
        // SAFETY: plain repr(C) struct.
        let in_bytes = unsafe {
            std::slice::from_raw_parts(
                &input as *const MftEnumDataV0 as *const u8,
                std::mem::size_of::<MftEnumDataV0>(),
            )
        };
        let n = match vol.ioctl(FSCTL_ENUM_USN_DATA, in_bytes, &mut buf) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(ERROR_HANDLE_EOF) => break,
            Err(e) => return Err(e),
        };
        if n < 8 {
            break;
        }
        let next = u64_at(&buf, 0).unwrap_or(0);
        parse_records(&buf[8..n], &mut out);
        if next == input.start_file_reference_number {
            break;
        }
        input.start_file_reference_number = next;
    }
    Ok(out)
}

/// Parses a run of `USN_RECORD_V2` / `USN_RECORD_V3` structures.
fn parse_records(b: &[u8], out: &mut Vec<UsnEntry>) {
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let Some(len) = u32_at(b, pos) else { return };
        let len = len as usize;
        if len < 8 || pos + len > b.len() {
            return;
        }
        let r = &b[pos..pos + len];
        let entry = match u16_at(r, 4) {
            Some(2) => (|| {
                Some(UsnEntry {
                    frn: u64_at(r, 8)?,
                    parent_frn: u64_at(r, 16)?,
                    attributes: u32_at(r, 52)?,
                    usn: u64_at(r, 24)? as i64,
                    name: name_at(r, 56)?,
                })
            })(),
            // V3 uses 128-bit ids; NTFS ids fit in the low 64 bits.
            Some(3) => (|| {
                Some(UsnEntry {
                    frn: u64_at(r, 8)?,
                    parent_frn: u64_at(r, 24)?,
                    attributes: u32_at(r, 68)?,
                    usn: u64_at(r, 40)? as i64,
                    name: name_at(r, 72)?,
                })
            })(),
            _ => None,
        };
        if let Some(e) = entry {
            out.push(e);
        }
        pos += len;
    }
}

fn name_at(r: &[u8], len_ofs: usize) -> Option<String> {
    let name_len = u16_at(r, len_ofs)? as usize;
    let name_ofs = u16_at(r, len_ofs + 2)? as usize;
    Some(utf16_le(r.get(name_ofs..name_ofs + name_len)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(frn: u64, parent: u64, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + units.len() * 2 + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[52..56].copy_from_slice(&0x10u32.to_le_bytes());
        r[56..58].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + 2 * i..62 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_v2_records_and_stops_on_garbage() {
        let mut b = v2(10, 5, "Documents");
        b.extend(v2(11, 10, "factură.pdf"));
        b.extend([0xFF; 3]);
        let mut out = Vec::new();
        parse_records(&b, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].name, "factură.pdf");
        assert_eq!(out[1].parent_frn, 10);
        assert_eq!(out[0].attributes, 0x10);
    }
}
