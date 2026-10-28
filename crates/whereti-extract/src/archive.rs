//! Archives (PLAN.md §61): ZIP, 7z and RAR. Their text is the name of every
//! file inside, one per line, each followed by that file's own text when it
//! is a document whereti reads (ZIP and 7z; RAR is read for names only).
//! So "factura" finds `acte.zip` because `facturi/factura_martie.pdf` is in
//! it, or because that PDF says "factura".
//!
//! Every name comes first, then the text of the documents, read while there
//! is time (`READ_TIME`, or less when the caller waits less), so an archive
//! of slow documents still gives all its names. Time is looked at between
//! documents; one document whose parser takes too long cannot hold the rest
//! back either: what was read before it is left for the worker, which hands
//! it back near its deadline (see `crate::save_partial`).
//!
//! Bounded like everything here: members inflated at most `MAX_ENTRY`, at
//! most `MAX_READ` documents read, pictures not read (OCR is slow), no
//! archives inside archives, and the whole text within the caller's budget.
//! A large ZIP sends only its end (the list of names), see `zip_tail_names`.

use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use crate::{extract, kind_of, ExtractError, Kind, Out, MAX_ENTRY};

/// Documents read inside one archive.
const MAX_READ: usize = 200;
/// Names listed from one archive.
const MAX_NAMES: usize = 20_000;
/// Time for reading the documents inside one archive, when the caller
/// allows more (the service waits 30 s for a file).
const READ_TIME: Duration = Duration::from_secs(20);

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

/// Adds one member's name, on a line of its own.
fn name_line(out: &mut Out, name: &str) {
    out.newline();
    out.push(&name.replace('/', "\\"));
    out.newline();
}

/// When reading the documents inside stops: `READ_TIME` from now, or the
/// caller's deadline if sooner. Every name is listed before, so a slow
/// archive still gives all its names.
fn read_until() -> Instant {
    let own = Instant::now() + READ_TIME;
    crate::deadline().map_or(own, |d| d.min(own))
}

/// Adds a document's text, under its name again, read by `until`; then
/// leaves what there is so far for the worker (see `crate::save_partial`).
fn content(out: &mut Out, name: &str, bytes: &[u8], until: Instant) {
    let Some(ext) = readable(name) else { return };
    if out.spent() {
        out.truncated = true;
        return;
    }
    if let Ok(e) = crate::inner(|| crate::within(until, || extract(&ext, bytes, out.budget))) {
        if !e.text.is_empty() {
            name_line(out, name);
            out.push(&e.text);
            out.newline();
            crate::save_partial(out);
        }
        out.truncated |= e.truncated;
    }
}

pub(crate) fn zip(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| ExtractError::Damaged(e.to_string()))?;
    // 1. every name, from the central directory: encrypted members and
    // members in a method not built in (Deflate64, bzip2, LZMA...) too
    let mut wanted = Vec::new();
    if archive.len() > MAX_NAMES {
        out.truncated = true;
    }
    for i in 0..archive.len().min(MAX_NAMES) {
        if out.full() {
            break;
        }
        let Some(Ok(name)) = archive.name_for_index(i) else { continue };
        if name.ends_with('/') || name.ends_with('\\') {
            continue;
        }
        let name = name.into_owned();
        name_line(out, &name);
        if readable(&name).is_some() && wanted.len() < MAX_READ {
            wanted.push((i, name));
        }
    }
    // 2. the documents' text, while there is time (the names are kept
    // first, for the worker to hand back should one document take too long)
    crate::save_partial(out);
    let until = read_until();
    for (i, name) in wanted {
        if out.spent() || Instant::now() >= until {
            out.truncated = true;
            break;
        }
        // encrypted, or a method not built in: its name is enough
        let Ok(f) = archive.by_index(i) else { continue };
        if f.encrypted() || f.size() > MAX_ENTRY {
            continue;
        }
        let mut buf = Vec::with_capacity(f.size() as usize);
        if f.take(MAX_ENTRY).read_to_end(&mut buf).is_ok() {
            content(out, &name, &buf, until);
        }
    }
    Ok(())
}

pub(crate) fn sevenz(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let password = sevenz_rust2::Password::empty();
    let mut source = Cursor::new(bytes);
    let archive = sevenz_rust2::Archive::read(&mut source, &password).map_err(|e| {
        let text = e.to_string();
        if text.to_lowercase().contains("password") {
            ExtractError::Protected
        } else {
            ExtractError::Damaged(text)
        }
    })?;
    // 1. every name, from the archive's header
    let mut wanted = std::collections::HashSet::new();
    let files: Vec<usize> = (0..archive.files.len()).filter(|&i| !archive.files[i].is_directory()).collect();
    if files.len() > MAX_NAMES {
        out.truncated = true;
    }
    for &i in files.iter().take(MAX_NAMES) {
        if out.full() {
            break;
        }
        let f = &archive.files[i];
        name_line(out, f.name());
        if readable(f.name()).is_some() && f.has_stream() && f.size() <= MAX_ENTRY && wanted.len() < MAX_READ {
            wanted.insert(i);
        }
    }
    // 2. the documents' text, block by block, while there is time; a block
    // that cannot be decoded (unknown method, damaged) is skipped. The names
    // are kept first, as for a ZIP.
    crate::save_partial(out);
    let until = read_until();
    let file_block = &archive.stream_map.file_block_index;
    let mut blocks: Vec<usize> = wanted.iter().filter_map(|&i| file_block.get(i).copied().flatten()).collect();
    blocks.sort_unstable();
    blocks.dedup();
    for block in blocks {
        if out.spent() || Instant::now() >= until {
            out.truncated = true;
            break;
        }
        let first = archive.stream_map.block_first_file_index[block];
        // the last wanted file of this block: decoding stops after it
        let last = wanted.iter().copied().filter(|&i| file_block.get(i).copied().flatten() == Some(block)).max().unwrap_or(first);
        let mut index = first;
        let decoder = sevenz_rust2::BlockDecoder::new(1, block, &archive, &password, &mut source);
        let _ = decoder.for_each_entries(&mut |entry, data| {
            let i = index;
            index += 1;
            if out.spent() || Instant::now() >= until {
                out.truncated = true;
                return Ok(false);
            }
            if wanted.contains(&i) {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                if data.take(MAX_ENTRY).read_to_end(&mut buf).is_err() {
                    return Ok(false);
                }
                content(out, entry.name(), &buf, until);
            } else if !drain(data, until) {
                // a member not read must still be decoded to reach the next
                out.truncated = true;
                return Ok(false);
            }
            Ok(i < last)
        });
    }
    Ok(())
}

/// Reads a member through to its end, unless time runs out first (a huge
/// member that compresses well would hold the worker for minutes).
fn drain(data: &mut dyn Read, until: Instant) -> bool {
    let mut buf = vec![0u8; 256 << 10];
    loop {
        match data.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) if Instant::now() < until => {}
            _ => return false,
        }
    }
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
    for f in reader.archive().files.iter().filter(|f| !f.is_directory()).take(MAX_NAMES) {
        if out.full() {
            break;
        }
        name_line(out, f.name());
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
    for n in names {
        if out.full() {
            break;
        }
        name_line(out, &n);
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
                let dir = flags & 0xE0 == 0xE0;
                if !dir {
                    names.push(rar4_name(raw, flags & 0x200 != 0));
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
    let mut names = 0;
    while p + 46 <= eocd && tail[p..].starts_with(b"PK\x01\x02") && names < MAX_NAMES && !out.full() {
        let flags = u16_at(p + 8).ok_or_else(bad)?;
        let name_len = u16_at(p + 28).ok_or_else(bad)?;
        let extra = u16_at(p + 30).ok_or_else(bad)?;
        let comment = u16_at(p + 32).ok_or_else(bad)?;
        let name = tail.get(p + 46..p + 46 + name_len).ok_or_else(bad)?;
        if !name.ends_with(b"/") {
            // bit 11: the name is UTF-8; else it is in code page 437
            let name = if flags & 0x800 != 0 { String::from_utf8_lossy(name).into_owned() } else { cp437(name) };
            name_line(out, &name);
            names += 1;
        }
        p += 46 + name_len + extra + comment;
    }
    Ok(())
}

/// A RAR 4 file name. With `unicode` set, the name is UTF-8, or a plain
/// name, a 0 byte, and the Unicode name packed against it; without it, the
/// name is in the DOS code page (437 when it is not UTF-8).
fn rar4_name(raw: &[u8], unicode: bool) -> String {
    let Some(zero) = raw.iter().position(|&c| c == 0) else {
        return match std::str::from_utf8(raw) {
            Ok(s) => s.to_string(),
            Err(_) if unicode => String::from_utf8_lossy(raw).into_owned(),
            Err(_) => cp437(raw),
        };
    };
    let (plain, packed) = (&raw[..zero], &raw[zero + 1..]);
    let wide = rar4_unicode(plain, packed);
    if wide.is_empty() {
        cp437(plain)
    } else {
        String::from_utf16_lossy(&wide)
    }
}

/// Unpacks RAR 4's Unicode name: two-bit commands say whether the next
/// character is one byte (high byte 0), one byte with the name's common
/// high byte, two bytes, or a run copied from the plain name.
fn rar4_unicode(plain: &[u8], packed: &[u8]) -> Vec<u16> {
    const MAX: usize = 1024;
    let mut out: Vec<u16> = Vec::new();
    let Some((&high, mut rest)) = packed.split_first() else { return out };
    let high = (high as u16) << 8;
    let (mut flags, mut bits) = (0u8, 0u32);
    while !rest.is_empty() && out.len() < MAX {
        if bits == 0 {
            flags = rest[0];
            rest = &rest[1..];
            bits = 8;
        }
        match flags >> 6 {
            0 => {
                let Some((&c, r)) = rest.split_first() else { break };
                out.push(c as u16);
                rest = r;
            }
            1 => {
                let Some((&c, r)) = rest.split_first() else { break };
                out.push(c as u16 + high);
                rest = r;
            }
            2 => {
                let [lo, hi, r @ ..] = rest else { break };
                out.push(u16::from_le_bytes([*lo, *hi]));
                rest = r;
            }
            _ => {
                let Some((&len, r)) = rest.split_first() else { break };
                rest = r;
                if len & 0x80 != 0 {
                    let Some((&fix, r)) = rest.split_first() else { break };
                    rest = r;
                    for _ in 0..(len & 0x7F) + 2 {
                        let at = out.len();
                        let c = plain.get(at).copied().unwrap_or(0);
                        out.push((c.wrapping_add(fix)) as u16 + high);
                    }
                } else {
                    for _ in 0..len as usize + 2 {
                        let at = out.len();
                        out.push(plain.get(at).copied().unwrap_or(0) as u16);
                    }
                }
            }
        }
        flags <<= 2;
        bits -= 2;
    }
    out.truncate(MAX);
    while out.last() == Some(&0) {
        out.pop();
    }
    out
}

/// Bytes in code page 437 (the old DOS one that ZIP names default to).
fn cp437(bytes: &[u8]) -> String {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    bytes.iter().map(|&c| if c < 0x80 { c as char } else { HIGH.chars().nth(c as usize - 0x80).unwrap_or('?') }).collect()
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
    fn every_name_comes_before_the_documents_and_time_only_cuts_documents() {
        let z = zip_with(&[("a.txt", b"textul lui a"), ("b.txt", b"textul lui b"), ("poza.jpg", b"x")]);
        let e = extract("zip", &z, 10_000).unwrap();
        assert_eq!(e.text, "a.txt\nb.txt\npoza.jpg\na.txt\ntextul lui a\nb.txt\ntextul lui b");
        // no time left: the names all the same, no text
        let e = crate::within(Instant::now(), || extract("zip", &z, 10_000)).unwrap();
        assert_eq!(e.text, "a.txt\nb.txt\npoza.jpg");
        assert!(e.truncated);
    }

    #[test]
    fn the_names_are_left_for_the_worker_before_any_document_is_read() {
        let z = zip_with(&[("a.txt", b"textul lui a"), ("poza.jpg", b"x"), ("b.txt", b"textul lui b")]);
        let partial = crate::Partial::default();
        crate::keep_partial(Some(partial.clone()));
        // no time for documents: the names alone, cut
        crate::within(Instant::now(), || extract("zip", &z, 10_000)).unwrap();
        assert_eq!(partial.lock().unwrap().take(), Some(crate::Extracted { text: "a.txt\npoza.jpg\nb.txt".into(), truncated: true }));
        // with time: what was read so far, document by document
        let e = extract("zip", &z, 10_000).unwrap();
        let got = partial.lock().unwrap().take().unwrap();
        assert_eq!(got, crate::Extracted { text: e.text, truncated: true });
        // what a document inside leaves does not replace the archive's
        let mut out = Out::new(100);
        out.push("numele");
        crate::save_partial(&out);
        crate::inner(|| crate::save_partial(&Out::new(100)));
        crate::keep_partial(None);
        assert_eq!(partial.lock().unwrap().take().map(|e| e.text).as_deref(), Some("numele"));
    }

    /// Sets a flag or a method in a ZIP's local and central headers alike.
    fn patch_zip(z: &mut [u8], at_local: usize, at_central: usize, value: u16) {
        for (sig, at) in [(b"PK\x03\x04", at_local), (b"PK\x01\x02", at_central)] {
            let p = z.windows(4).position(|w| w == sig).unwrap();
            z[p + at..p + at + 2].copy_from_slice(&value.to_le_bytes());
        }
    }

    #[test]
    fn encrypted_and_unknown_method_members_keep_their_names() {
        let mut z = zip_with(&[("secret/plan.txt", b"nu se citeste")]);
        patch_zip(&mut z, 6, 8, 1); // encrypted
        let e = extract("zip", &z, 10_000).unwrap();
        assert_eq!(e.text, r"secret\plan.txt");
        let mut z = zip_with(&[("vechi/arhiva.txt", b"Deflate64")]);
        patch_zip(&mut z, 8, 10, 9); // Deflate64, not built in
        let e = extract("zip", &z, 10_000).unwrap();
        assert_eq!(e.text, r"vechi\arhiva.txt");
    }

    #[test]
    fn a_7z_lists_its_names_and_reads_its_documents() {
        let mut buf = Cursor::new(Vec::new());
        let mut w = sevenz_rust2::ArchiveWriter::new(&mut buf).unwrap();
        for (name, data) in [("acte/contract.txt", &b"Contract de inchiriere"[..]), ("mare.bin", &[0u8; 100_000][..]), ("nota.md", b"de citit")] {
            w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(data)).unwrap();
        }
        w.finish().unwrap();
        let e = extract("7z", buf.get_ref(), 10_000).unwrap();
        assert_eq!(e.text, "acte\\contract.txt\nmare.bin\nnota.md\nacte\\contract.txt\nContract de inchiriere\nnota.md\nde citit");
    }

    #[test]
    fn rar4_unicode_names_and_cp437_zip_names() {
        // "ș.txt": a plain name, a 0 byte, then the packed Unicode name:
        // high byte 0x02, flags 01 00 00 00 (one char with it, three plain), then the last plain one
        let mut raw = b"?.txt\0".to_vec();
        raw.extend([0x02, 0x40, 0x19, b'.', b't', b'x', 0x00, b't']);
        assert_eq!(rar4_name(&raw, true), "ș.txt");
        assert_eq!(rar4_name("ă.txt".as_bytes(), true), "ă.txt");
        assert_eq!(rar4_name(b"caf\x82.txt", false), "café.txt");
        // a ZIP name without the UTF-8 flag is in code page 437
        let mut z = zip_with(&[("caf\u{e9}.txt", b"x")]);
        let utf8 = "caf\u{e9}.txt".as_bytes();
        let p = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        let name_at = z[p..].windows(utf8.len()).position(|w| w == utf8).unwrap() + p;
        let cp437: Vec<u8> = b"caf\x82.txt\x00".to_vec(); // same length: one byte for "é", a padding NUL
        z[name_at..name_at + cp437.len()].copy_from_slice(&cp437);
        z[p + 8..p + 10].copy_from_slice(&0u16.to_le_bytes());
        let mut out = Out::new(1000);
        zip_tail_names(&z, &mut out).unwrap();
        assert!(out.finish().text.starts_with("café.txt"));
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
