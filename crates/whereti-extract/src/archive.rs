//! Archives (PLAN.md §61): ZIP, 7z and RAR. Their text is the name of every
//! file inside, one per line, each followed by that file's own text when it
//! is a document whereti reads (ZIP and 7z; RAR is read for names only).
//! So "factura" finds `acte.zip` because `facturi/factura_martie.pdf` is in
//! it, or because that PDF says "factura".
//!
//! Bounded like everything here: members inflated at most `MAX_ENTRY`, at
//! most `MAX_READ` documents read, pictures not read (OCR is slow), no
//! archives inside archives, and the whole text within the caller's budget.
//! A large ZIP sends only its end (the list of names), see `zip_tail_names`.

use std::io::{Cursor, Read};

use crate::{extract, kind_of, ExtractError, Kind, Out, MAX_ENTRY};

/// Documents read inside one archive.
const MAX_READ: usize = 200;
/// Names listed from one archive.
const MAX_NAMES: usize = 20_000;

pub(crate) fn is_archive(ext: &str) -> bool {
    matches!(ext, "zip" | "7z" | "rar")
}

/// A member worth reading: a document, not a picture or another archive.
fn readable(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match kind_of(&ext)? {
        Kind::Image | Kind::Archive => None,
        _ => Some(ext),
    }
}

/// Adds one member: its name, then (if given) its text.
fn member(out: &mut Out, name: &str, bytes: Option<Vec<u8>>, read: &mut usize) {
    out.newline();
    out.push(&name.replace('/', "\\"));
    out.newline();
    let (Some(bytes), Some(ext)) = (bytes, readable(name)) else { return };
    if *read >= MAX_READ || out.full() {
        return;
    }
    *read += 1;
    if let Ok(e) = extract(&ext, &bytes, out.budget) {
        out.push(&e.text);
        out.newline();
    }
}

pub(crate) fn zip(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| ExtractError::Damaged(e.to_string()))?;
    let mut read = 0;
    for i in 0..archive.len().min(MAX_NAMES) {
        if out.full() {
            break;
        }
        let Ok(mut f) = archive.by_index(i) else { continue };
        if f.is_dir() {
            continue;
        }
        let Ok(name) = f.name().map(|n| n.into_owned()) else { continue };
        let wanted = readable(&name).is_some() && read < MAX_READ && !f.encrypted() && f.size() <= MAX_ENTRY;
        let content = wanted.then(|| {
            let mut buf = Vec::with_capacity(f.size() as usize);
            (&mut f).take(MAX_ENTRY).read_to_end(&mut buf).ok().map(|_| buf)
        });
        member(out, &name, content.flatten(), &mut read);
    }
    Ok(())
}

pub(crate) fn sevenz(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut reader = sevenz_rust2::ArchiveReader::new(Cursor::new(bytes), sevenz_rust2::Password::empty()).map_err(|e| {
        let text = e.to_string();
        if text.to_lowercase().contains("password") {
            ExtractError::Protected
        } else {
            ExtractError::Damaged(text)
        }
    })?;
    let mut read = 0;
    let mut names = 0;
    let _ = reader.for_each_entries(|entry, data| {
        if out.full() || names >= MAX_NAMES {
            return Ok(false);
        }
        if entry.is_directory() {
            return Ok(true);
        }
        names += 1;
        let name = entry.name().to_string();
        let wanted = readable(&name).is_some() && read < MAX_READ && entry.size() <= MAX_ENTRY;
        let content = if wanted {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            data.take(MAX_ENTRY).read_to_end(&mut buf).ok().map(|_| buf)
        } else {
            // the stream must be consumed to reach the next entry
            let _ = std::io::copy(data, &mut std::io::sink());
            None
        };
        member(out, &name, content, &mut read);
        Ok(true)
    });
    Ok(())
}

/// A 7z seen through its two ends (see `crate::sevenz_ends`): reads as a
/// file of its real length whose middle is zeros. 7z keeps its list of
/// names at the end, so it can be read; the files themselves are not.
struct Ends {
    len: u64,
    head: Vec<u8>,
    tail_start: u64,
    tail: Vec<u8>,
    pos: u64,
}

impl Read for Ends {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = if self.pos < self.head.len() as u64 {
            let from = &self.head[self.pos as usize..];
            let n = from.len().min(buf.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else if self.pos >= self.tail_start {
            let from = &self.tail[(self.pos - self.tail_start) as usize..];
            let n = from.len().min(buf.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else {
            let n = ((self.tail_start - self.pos) as usize).min(buf.len());
            buf[..n].fill(0);
            n
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for Ends {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            std::io::SeekFrom::Start(p) => p as i128,
            std::io::SeekFrom::End(d) => self.len as i128 + d as i128,
            std::io::SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if target < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "before the start"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

pub(crate) fn sevenz_end_names(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let bad = || ExtractError::Damaged("not the ends of a 7z".into());
    let len = u64::from_le_bytes(bytes.get(..8).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
    let head = bytes.get(8..40).ok_or_else(bad)?.to_vec();
    let tail = bytes.get(40..).ok_or_else(bad)?.to_vec();
    let tail_start = len.checked_sub(tail.len() as u64).ok_or_else(bad)?;
    let ends = Ends { len, head, tail_start, tail, pos: 0 };
    let reader = sevenz_rust2::ArchiveReader::new(ends, sevenz_rust2::Password::empty()).map_err(|e| {
        let text = e.to_string();
        if text.to_lowercase().contains("password") {
            ExtractError::Protected
        } else {
            ExtractError::Damaged(text)
        }
    })?;
    let mut read = 0;
    for f in reader.archive().files.iter().filter(|f| !f.is_directory()).take(MAX_NAMES) {
        if out.full() {
            break;
        }
        member(out, f.name(), None, &mut read);
    }
    Ok(())
}

/// RAR (4 and 5): the names only, from the file headers.
pub(crate) fn rar(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let names = if bytes.starts_with(b"Rar!\x1a\x07\x01\x00") {
        rar5_names(bytes)?
    } else if bytes.starts_with(b"Rar!\x1a\x07\x00") {
        rar4_names(bytes)?
    } else {
        return Err(ExtractError::Damaged("not a RAR archive".into()));
    };
    let mut read = 0;
    for n in names {
        if out.full() {
            break;
        }
        member(out, &n, None, &mut read);
    }
    Ok(())
}

/// RAR 5 variable-length integer.
fn vint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn rar5_names(b: &[u8]) -> Result<Vec<String>, ExtractError> {
    let bad = || ExtractError::Damaged("bad RAR header".into());
    let mut names = Vec::new();
    let mut pos = 8;
    while pos < b.len().saturating_sub(4) && names.len() < MAX_NAMES {
        let mut p = pos + 4; // CRC32
        let size = vint(b, &mut p).ok_or_else(bad)? as usize;
        let start = p;
        let end = start.checked_add(size).filter(|&e| e <= b.len()).ok_or_else(bad)?;
        let kind = vint(b, &mut p).ok_or_else(bad)?;
        let flags = vint(b, &mut p).ok_or_else(bad)?;
        if flags & 1 != 0 {
            vint(b, &mut p).ok_or_else(bad)?; // extra area size
        }
        let data = if flags & 2 != 0 { vint(b, &mut p).ok_or_else(bad)? } else { 0 };
        match kind {
            // file header
            2 => {
                let file_flags = vint(b, &mut p).ok_or_else(bad)?;
                vint(b, &mut p).ok_or_else(bad)?; // unpacked size
                vint(b, &mut p).ok_or_else(bad)?; // attributes
                if file_flags & 2 != 0 {
                    p += 4; // mtime
                }
                if file_flags & 4 != 0 {
                    p += 4; // CRC32
                }
                vint(b, &mut p).ok_or_else(bad)?; // compression
                vint(b, &mut p).ok_or_else(bad)?; // host OS
                let len = vint(b, &mut p).ok_or_else(bad)? as usize;
                let name = b.get(p..p.checked_add(len).ok_or_else(bad)?).filter(|_| p + len <= end).ok_or_else(bad)?;
                if file_flags & 1 == 0 {
                    // not a directory
                    names.push(String::from_utf8_lossy(name).into_owned());
                }
            }
            // encrypted headers: the names cannot be read without the password
            4 => return if names.is_empty() { Err(ExtractError::Protected) } else { Ok(names) },
            // end of archive
            5 => break,
            _ => {}
        }
        // a data size past the end: the archive is cut short, stop here
        match usize::try_from(data).ok().and_then(|d| end.checked_add(d)).filter(|&p| p <= b.len()) {
            Some(p) => pos = p,
            None => break,
        }
    }
    Ok(names)
}

fn rar4_names(b: &[u8]) -> Result<Vec<String>, ExtractError> {
    let bad = || ExtractError::Damaged("bad RAR header".into());
    let u16_at = |p: usize| b.get(p..p + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
    let u32_at = |p: usize| b.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64);
    let mut names = Vec::new();
    let mut pos = 7;
    while pos < b.len().saturating_sub(6) && names.len() < MAX_NAMES {
        let kind = b[pos + 2];
        let flags = u16_at(pos + 3).ok_or_else(bad)?;
        let head = u16_at(pos + 5).ok_or_else(bad)?;
        if head < 7 {
            return Err(bad());
        }
        let mut add = if flags & 0x8000 != 0 { u32_at(pos + 7).ok_or_else(bad)? } else { 0 };
        match kind {
            // main header: encrypted headers cannot be read
            0x73 if flags & 0x80 != 0 => return Err(ExtractError::Protected),
            // file header
            0x74 => {
                let pack = u32_at(pos + 7).ok_or_else(bad)?;
                let name_len = u16_at(pos + 26).ok_or_else(bad)?;
                let (high, name_at) = if flags & 0x100 != 0 {
                    (u32_at(pos + 32).ok_or_else(bad)?, pos + 40)
                } else {
                    (0, pos + 32)
                };
                add = pack | high << 32;
                let raw = b.get(name_at..name_at + name_len).ok_or_else(bad)?;
                // a Unicode name follows the plain one after a 0 byte
                let plain = raw.split(|&c| c == 0).next().unwrap_or(raw);
                let dir = flags & 0xE0 == 0xE0;
                if !dir {
                    names.push(String::from_utf8_lossy(plain).into_owned());
                }
            }
            // end of archive
            0x7B => break,
            _ => {}
        }
        match pos.checked_add(head).and_then(|p| p.checked_add(usize::try_from(add).ok()?)).filter(|&p| p <= b.len()) {
            Some(p) => pos = p,
            None => break,
        }
    }
    Ok(names)
}

/// The names in a ZIP, from its end only (the central directory): for
/// archives too large to send whole. `tail` is the last bytes of the file.
pub(crate) fn zip_tail_names(tail: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let bad = || ExtractError::Damaged("no ZIP directory in the end of the file".into());
    let u16_at = |p: usize| tail.get(p..p + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
    let u32_at = |p: usize| tail.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
    // end of central directory: the last "PK\5\6"
    let eocd = (0..tail.len().saturating_sub(21)).rev().find(|&p| tail[p..].starts_with(b"PK\x05\x06")).ok_or_else(bad)?;
    let mut cd_size = u32_at(eocd + 12).ok_or_else(bad)?;
    // the directory ends where the end records start: ZIP64 adds its own
    // record (PK 6 6) and a 20-byte locator (PK 6 7) before the classic one
    let mut cd_end = eocd;
    if eocd >= 20 && tail[eocd - 20..].starts_with(b"PK\x06\x07") {
        let z64 = (0..eocd - 20).rev().find(|&p| tail[p..].starts_with(b"PK\x06\x06")).ok_or_else(bad)?;
        let size64 = tail.get(z64 + 40..z64 + 48).map(|s| u64::from_le_bytes(s.try_into().expect("8 bytes"))).ok_or_else(bad)?;
        cd_size = usize::try_from(size64).map_err(|_| bad())?;
        cd_end = z64;
    }
    let eocd = cd_end;
    let mut p = eocd.checked_sub(cd_size).ok_or_else(bad)?;
    let mut read = 0;
    let mut names = 0;
    while p + 46 <= eocd && tail[p..].starts_with(b"PK\x01\x02") && names < MAX_NAMES && !out.full() {
        let name_len = u16_at(p + 28).ok_or_else(bad)?;
        let extra = u16_at(p + 30).ok_or_else(bad)?;
        let comment = u16_at(p + 32).ok_or_else(bad)?;
        let name = tail.get(p + 46..p + 46 + name_len).ok_or_else(bad)?;
        if !name.ends_with(b"/") {
            member(out, &String::from_utf8_lossy(name), None, &mut read);
            names += 1;
        }
        p += 46 + name_len + extra + comment;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (name, data) in files {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn a_zip_lists_its_files_and_reads_its_documents() {
        let z = zip_with(&[("acte/factura_martie.txt", b"Total de plata 120 lei"), ("poza.jpg", b"\xff\xd8not read")]);
        let e = extract("zip", &z, 10_000).unwrap();
        assert!(e.text.contains(r"acte\factura_martie.txt"), "{}", e.text);
        assert!(e.text.contains("Total de plata 120 lei"));
        assert!(e.text.contains("poza.jpg"));
        assert!(!e.text.contains("not read"));
    }

    #[test]
    fn the_end_of_a_large_zip_gives_its_names() {
        let z = zip_with(&[("a/raport.pdf", b"x"), ("b/contract.docx", b"y")]);
        // as if the start of the file were not sent
        let tail = &z[20..];
        let mut out = Out::new(10_000);
        zip_tail_names(tail, &mut out).unwrap();
        let text = out.finish().text;
        assert!(text.contains(r"a\raport.pdf") && text.contains(r"b\contract.docx"), "{text}");
    }

    #[test]
    fn rar_names_are_read_from_rar4_and_rar5_headers() {
        // RAR 5: signature, main header, one file header "doc/a.txt", end
        let mut r5 = b"Rar!\x1a\x07\x01\x00".to_vec();
        let block = |body: &[u8]| {
            let mut b = vec![0, 0, 0, 0]; // CRC (not checked)
            b.push(body.len() as u8);
            b.extend_from_slice(body);
            b
        };
        r5.extend(block(&[1, 0, 0])); // main
        let name = b"doc/a.txt";
        let mut file = vec![2, 0, 0, 5, 0x20, 0, 0];
        file.push(name.len() as u8);
        file.extend_from_slice(name);
        r5.extend(block(&file));
        r5.extend(block(&[5, 0, 0])); // end
        assert_eq!(rar5_names(&r5).unwrap(), vec!["doc/a.txt"]);

        // RAR 4: marker, archive header, one file header "b.txt", end
        let mut r4 = b"Rar!\x1a\x07\x00".to_vec();
        r4.extend([0, 0, 0x73, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0]);
        let name = b"b.txt";
        let mut f = vec![0, 0, 0x74, 0, 0x80];
        let head = 32 + name.len();
        f.extend((head as u16).to_le_bytes());
        f.extend(0u32.to_le_bytes()); // packed size
        f.extend(1u32.to_le_bytes()); // unpacked size
        f.push(2); // host OS
        f.extend(0u32.to_le_bytes()); // CRC
        f.extend(0u32.to_le_bytes()); // time
        f.push(29); // version
        f.push(0x30); // method
        f.extend((name.len() as u16).to_le_bytes());
        f.extend(0x20u32.to_le_bytes()); // attributes
        f.extend_from_slice(name);
        r4.extend(f);
        r4.extend([0, 0, 0x7B, 0, 0x40, 7, 0]);
        assert_eq!(rar4_names(&r4).unwrap(), vec!["b.txt"]);
    }

    #[test]
    fn damaged_archives_fail_without_panicking() {
        for ext in ["zip", "7z", "rar"] {
            for junk in [&b""[..], b"PK\x03\x04garbage", b"Rar!\x1a\x07\x01\x00\xff\xff\xff\xff\xff", b"7z\xbc\xaf\x27\x1c\x00\x04"] {
                let _ = extract(ext, junk, 1000);
            }
        }
        let mut out = Out::new(100);
        let _ = zip_tail_names(b"PK\x05\x06\0\0\0\0\0\0\0\0\xff\xff\xff\xff\0\0\0\0\0\0", &mut out);
    }

    /// A real large 7z (set WHERETI_BIG7Z): its names from its two ends.
    #[test]
    fn the_ends_of_a_large_7z_give_its_names() {
        use std::io::{Seek, SeekFrom};
        let Some(path) = std::env::var_os("WHERETI_BIG7Z") else { return };
        let mut f = std::fs::File::open(&path).unwrap();
        let len = f.metadata().unwrap().len();
        let mut head = vec![0u8; 32];
        f.read_exact(&mut head).unwrap();
        let tail_len = (16u64 << 20).min(len);
        f.seek(SeekFrom::Start(len - tail_len)).unwrap();
        let mut tail = vec![0u8; tail_len as usize];
        f.read_exact(&mut tail).unwrap();
        let e = extract(crate::SEVENZ_ENDS, &crate::sevenz_ends(len, &head, &tail), 100_000).unwrap();
        eprintln!("{} names, first: {:?}", e.text.lines().count(), e.text.lines().next());
        assert!(e.text.lines().count() > 0);
    }
}
