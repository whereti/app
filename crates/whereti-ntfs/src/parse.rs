//! Pure parsing of NTFS on-disk structures: no I/O, no unsafe.
//!
//! Every read is bounds-checked. Corrupt or unexpected data makes a function
//! return `None` / `Corrupt` instead of panicking.

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

pub const RECORD_IN_USE: u16 = 0x0001;
pub const RECORD_IS_DIRECTORY: u16 = 0x0002;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_AND_DOS: u8 = 3;

/// Low 48 bits of a file reference = MFT record number. High 16 bits = sequence.
pub const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// MFT record number of the root directory.
pub const ROOT_RECORD: u64 = 5;

pub(crate) fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
pub(crate) fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}
pub(crate) fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

pub(crate) fn utf16_le(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Applies the "update sequence" fixups of a FILE record in place.
///
/// NTFS overwrites the last two bytes of every sector of a record with a
/// check value and keeps the real bytes in an array in the header. Without
/// this step the record contains garbage at every sector boundary. Returns
/// `false` if the record is torn or corrupt.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    let (Some(usa_ofs), Some(usa_count)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let usa_ofs = usa_ofs as usize;
    let usa_count = usa_count as usize;
    if usa_count < 2 {
        return false;
    }
    let sectors = usa_count - 1;
    if !rec.len().is_multiple_of(sectors) {
        return false;
    }
    let stride = rec.len() / sectors;
    if stride < 2 || usa_ofs + usa_count * 2 > rec.len() {
        return false;
    }
    let check = [rec[usa_ofs], rec[usa_ofs + 1]];
    for i in 0..sectors {
        let end = (i + 1) * stride;
        if rec[end - 2..end] != check {
            return false;
        }
        let src = usa_ofs + 2 * (i + 1);
        rec[end - 2] = rec[src];
        rec[end - 1] = rec[src + 1];
    }
    true
}

/// One extent of a non-resident attribute. `lcn == None` means a sparse hole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub lcn: Option<i64>,
    pub clusters: u64,
}

/// Decodes an NTFS mapping-pairs array (run list).
pub fn decode_runlist(b: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut lcn: i64 = 0;
    loop {
        let header = *b.get(pos)?;
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        pos += 1;
        let mut clusters: u64 = 0;
        for (i, &x) in b.get(pos..pos + len_size)?.iter().enumerate() {
            clusters |= (x as u64) << (8 * i);
        }
        pos += len_size;
        let run_lcn = if off_size == 0 {
            None
        } else {
            let mut delta: i64 = 0;
            for (i, &x) in b.get(pos..pos + off_size)?.iter().enumerate() {
                delta |= (x as i64) << (8 * i);
            }
            pos += off_size;
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift; // sign-extend
            lcn = lcn.checked_add(delta)?;
            Some(lcn)
        };
        runs.push(Run { lcn: run_lcn, clusters });
    }
    Some(runs)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StdInfo {
    /// Last write time, FILETIME (100 ns ticks since 1601-01-01 UTC).
    pub modified: u64,
    /// FILE_ATTRIBUTE_* flags.
    pub attributes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// Full file reference (record number + sequence) of the parent directory.
    pub parent_ref: u64,
    pub namespace: u8,
    pub name: String,
}

/// Data from the non-resident unnamed `$DATA` attribute, used to locate `$MFT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataRuns {
    pub runs: Vec<Run>,
    /// Number of clusters the run list of this record should cover.
    pub expected_clusters: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RecordInfo {
    /// Record number this record was read from.
    pub index: u64,
    /// Base record number (equal to `index` for base records).
    pub base: u64,
    pub seq: u16,
    pub flags: u16,
    pub std: Option<StdInfo>,
    pub names: Vec<FileName>,
    /// Logical size of the unnamed `$DATA` stream.
    pub data_size: Option<u64>,
    pub data_runs: Option<DataRuns>,
    pub has_attribute_list: bool,
}

impl RecordInfo {
    pub fn is_base(&self) -> bool {
        self.base == self.index
    }
    pub fn is_dir(&self) -> bool {
        self.flags & RECORD_IS_DIRECTORY != 0
    }
}

#[derive(Debug)]
pub enum ParseOutcome {
    /// Free or never-used record.
    Unused,
    /// Torn write, bad signature or broken structure.
    Corrupt,
    Record(RecordInfo),
}

/// Parses one FILE record (already sized to `BytesPerFileRecordSegment`).
/// Fixups are applied in place.
pub fn parse_record(index: u64, rec: &mut [u8], want_runs: bool) -> ParseOutcome {
    match rec.get(0..4) {
        Some(b"FILE") => {}
        Some(b"BAAD") => return ParseOutcome::Corrupt,
        _ => return ParseOutcome::Unused,
    }
    let Some(flags) = u16_at(rec, 0x16) else {
        return ParseOutcome::Corrupt;
    };
    if flags & RECORD_IN_USE == 0 {
        return ParseOutcome::Unused;
    }
    if !apply_fixup(rec) {
        return ParseOutcome::Corrupt;
    }
    let (Some(seq), Some(first_attr), Some(in_use), Some(base_ref)) = (
        u16_at(rec, 0x10),
        u16_at(rec, 0x14),
        u32_at(rec, 0x18),
        u64_at(rec, 0x20),
    ) else {
        return ParseOutcome::Corrupt;
    };
    let base = base_ref & FRN_MASK;
    let mut info = RecordInfo {
        index,
        base: if base == 0 { index } else { base },
        seq,
        flags,
        ..Default::default()
    };

    let limit = (in_use as usize).min(rec.len());
    let mut pos = first_attr as usize;
    while pos + 16 <= limit {
        let Some(ty) = u32_at(rec, pos) else { break };
        if ty == ATTR_END {
            break;
        }
        let Some(len) = u32_at(rec, pos + 4) else { break };
        let len = len as usize;
        if len < 16 || pos + len > limit {
            return ParseOutcome::Corrupt;
        }
        let attr = &rec[pos..pos + len];
        let non_resident = attr[8] != 0;
        let name_len = attr[9];
        match ty {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                if let Some(v) = resident_value(attr) {
                    if let (Some(modified), Some(attributes)) = (u64_at(v, 8), u32_at(v, 32)) {
                        info.std = Some(StdInfo { modified, attributes });
                    }
                }
            }
            ATTR_FILE_NAME if !non_resident => {
                if let Some(name) = resident_value(attr).and_then(parse_file_name) {
                    info.names.push(name);
                }
            }
            ATTR_DATA if name_len == 0 => {
                if !non_resident {
                    if let Some(vlen) = u32_at(attr, 0x10) {
                        info.data_size = Some(vlen as u64);
                    }
                } else {
                    let lowest_vcn = u64_at(attr, 0x10).unwrap_or(u64::MAX);
                    if lowest_vcn == 0 {
                        info.data_size = u64_at(attr, 0x30);
                    }
                    if want_runs && lowest_vcn == 0 {
                        let highest_vcn = u64_at(attr, 0x18).unwrap_or(0);
                        let runs = u16_at(attr, 0x20)
                            .and_then(|off| attr.get(off as usize..))
                            .and_then(decode_runlist);
                        match runs {
                            Some(runs) => {
                                info.data_runs = Some(DataRuns {
                                    runs,
                                    expected_clusters: highest_vcn + 1,
                                })
                            }
                            None => return ParseOutcome::Corrupt,
                        }
                    }
                }
            }
            ATTR_ATTRIBUTE_LIST => info.has_attribute_list = true,
            _ => {}
        }
        pos += len;
    }
    ParseOutcome::Record(info)
}

fn resident_value(attr: &[u8]) -> Option<&[u8]> {
    let vlen = u32_at(attr, 0x10)? as usize;
    let voff = u16_at(attr, 0x14)? as usize;
    attr.get(voff..voff.checked_add(vlen)?)
}

fn parse_file_name(v: &[u8]) -> Option<FileName> {
    let parent_ref = u64_at(v, 0)?;
    let name_chars = *v.get(0x40)? as usize;
    let namespace = *v.get(0x41)?;
    let raw = v.get(0x42..0x42 + name_chars * 2)?;
    Some(FileName {
        parent_ref,
        namespace,
        name: utf16_le(raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REC: usize = 1024;
    const USA_OFS: usize = 0x30;

    /// Builds a valid FILE record with the given attributes, then "protects"
    /// it with an update sequence array, as NTFS does on disk.
    fn build_record(flags: u16, base_ref: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; REC];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&(USA_OFS as u16).to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 check value + 2 sectors
        r[0x10..0x12].copy_from_slice(&7u16.to_le_bytes()); // sequence
        let first_attr = 0x38usize;
        r[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base_ref.to_le_bytes());
        let mut pos = first_attr;
        for a in attrs {
            r[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        r[pos..pos + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        pos += 8;
        r[0x18..0x1C].copy_from_slice(&(pos as u32).to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(REC as u32).to_le_bytes());
        // protect: check value 0xABCD at each sector end, originals in USA
        r[USA_OFS..USA_OFS + 2].copy_from_slice(&[0xCD, 0xAB]);
        for i in 0..2 {
            let end = (i + 1) * 512;
            let orig = [r[end - 2], r[end - 1]];
            r[USA_OFS + 2 * (i + 1)..USA_OFS + 2 * (i + 1) + 2].copy_from_slice(&orig);
            r[end - 2..end].copy_from_slice(&[0xCD, 0xAB]);
        }
        r
    }

    fn resident_attr(ty: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let voff = 0x18usize;
        let len = (voff + value.len() + 7) & !7;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(voff as u16).to_le_bytes());
        a[voff..voff + value.len()].copy_from_slice(value);
        a
    }

    fn std_info(modified: u64, attributes: u32) -> Vec<u8> {
        let mut v = vec![0u8; 48];
        v[8..16].copy_from_slice(&modified.to_le_bytes());
        v[32..36].copy_from_slice(&attributes.to_le_bytes());
        resident_attr(ATTR_STANDARD_INFORMATION, 0, &v)
    }

    fn file_name(parent_ref: u64, ns: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        v[0..8].copy_from_slice(&parent_ref.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = ns;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        resident_attr(ATTR_FILE_NAME, 0, &v)
    }

    fn non_resident_data(runlist: &[u8], highest_vcn: u64, size: u64) -> Vec<u8> {
        let roff = 0x40usize;
        let len = (roff + runlist.len() + 7) & !7;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[0x18..0x20].copy_from_slice(&highest_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(roff as u16).to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[roff..roff + runlist.len()].copy_from_slice(runlist);
        a
    }

    #[test]
    fn runlist_decodes_positive_negative_and_sparse_runs() {
        let b = [0x21, 0x18, 0x34, 0x56, 0x11, 0x08, 0xF0, 0x01, 0x04, 0x00];
        let runs = decode_runlist(&b).unwrap();
        assert_eq!(
            runs,
            vec![
                Run { lcn: Some(0x5634), clusters: 0x18 },
                Run { lcn: Some(0x5634 - 16), clusters: 8 },
                Run { lcn: None, clusters: 4 },
            ]
        );
    }

    #[test]
    fn runlist_rejects_truncated_input() {
        assert!(decode_runlist(&[0x21, 0x18, 0x34]).is_none());
        assert!(decode_runlist(&[]).is_none());
    }

    #[test]
    fn parses_file_with_win32_and_dos_names() {
        let parent = (3u64 << 48) | 1234;
        let mut r = build_record(
            RECORD_IN_USE,
            0,
            &[
                std_info(0x01D9_0000_0000_0000, 0x20),
                file_name(parent, NS_DOS, "RAPORT~1.PDF"),
                file_name(parent, NS_WIN32, "Raport anual 2025 — mașină.pdf"),
                resident_attr(ATTR_DATA, 0, &[1, 2, 3, 4, 5]),
            ],
        );
        let ParseOutcome::Record(info) = parse_record(42, &mut r, false) else {
            panic!("expected a record");
        };
        assert!(info.is_base());
        assert!(!info.is_dir());
        assert_eq!(info.seq, 7);
        assert_eq!(info.data_size, Some(5));
        assert_eq!(info.std.unwrap().attributes, 0x20);
        assert_eq!(info.names.len(), 2);
        assert_eq!(info.names[1].name, "Raport anual 2025 — mașină.pdf");
        assert_eq!(info.names[1].parent_ref & FRN_MASK, 1234);
    }

    #[test]
    fn extension_record_points_to_base() {
        let mut r = build_record(RECORD_IN_USE, (1u64 << 48) | 99, &[file_name(5, NS_WIN32, "link")]);
        let ParseOutcome::Record(info) = parse_record(500, &mut r, false) else {
            panic!("expected a record");
        };
        assert_eq!(info.base, 99);
        assert!(!info.is_base());
    }

    #[test]
    fn mft_record_exposes_data_runs() {
        let runlist = [0x21, 0x10, 0x00, 0x01, 0x00];
        let mut r = build_record(
            RECORD_IN_USE,
            0,
            &[file_name(5, NS_WIN32_AND_DOS, "$MFT"), non_resident_data(&runlist, 15, 16 * 4096)],
        );
        let ParseOutcome::Record(info) = parse_record(0, &mut r, true) else {
            panic!("expected a record");
        };
        let runs = info.data_runs.unwrap();
        assert_eq!(runs.runs, vec![Run { lcn: Some(0x100), clusters: 16 }]);
        assert_eq!(runs.expected_clusters, 16);
        assert_eq!(info.data_size, Some(16 * 4096));
    }

    #[test]
    fn torn_record_is_corrupt() {
        let mut r = build_record(RECORD_IN_USE, 0, &[file_name(5, NS_WIN32, "x")]);
        r[1023] ^= 0xFF; // sector end no longer matches the check value
        assert!(matches!(parse_record(1, &mut r, false), ParseOutcome::Corrupt));
    }

    #[test]
    fn free_and_empty_records_are_unused() {
        let mut r = build_record(0, 0, &[file_name(5, NS_WIN32, "deleted")]);
        assert!(matches!(parse_record(1, &mut r, false), ParseOutcome::Unused));
        let mut zero = vec![0u8; REC];
        assert!(matches!(parse_record(2, &mut zero, false), ParseOutcome::Unused));
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut r = build_record(RECORD_IN_USE, 0, &[file_name(5, NS_WIN32, "abc")]);
            for _ in 0..8 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let i = (seed as usize >> 8) % REC;
                r[i] = (seed >> 3) as u8;
            }
            let _ = parse_record(3, &mut r, true);
        }
    }
}
