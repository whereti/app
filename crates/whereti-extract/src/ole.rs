//! The old Office formats, all stored in an OLE compound file (`cfb`):
//!
//! - **`.doc`** (Word 97–2003): the text is assembled from the "piece
//!   table" ([MS-DOC] §2.4.1): the FIB points to the CLX in the table
//!   stream, whose pieces say where each run of characters lives in the
//!   WordDocument stream and whether it is 8-bit or UTF-16. Field codes are
//!   dropped and only their results kept.
//! - **`.ppt`** (PowerPoint 97–2003): every TextCharsAtom (UTF-16) and
//!   TextBytesAtom (8-bit) in the "PowerPoint Document" stream, slides and
//!   notes, without the master slides' placeholder text ([MS-PPT]).
//! - **`.msg`** (Outlook): subject, sender, recipients, date, body and
//!   attachment names are separate property streams ([MS-OXMSG]).

use std::io::{Cursor, Read};

use crate::email::{attachments, Headers};
use crate::{html_to_text, ExtractError, Out};

type Cfb<'a> = cfb::CompoundFile<Cursor<&'a [u8]>>;

fn open(bytes: &[u8]) -> Result<Cfb<'_>, ExtractError> {
    cfb::CompoundFile::open(Cursor::new(bytes)).map_err(|e| ExtractError::Damaged(format!("not an Office file: {e}")))
}

fn stream(cfb: &mut Cfb, path: &str) -> Option<Vec<u8>> {
    let mut s = cfb.open_stream(path).ok()?;
    let mut buf = Vec::new();
    s.by_ref().take(crate::MAX_ENTRY).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------- .doc

pub(crate) fn doc(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut cfb = open(bytes)?;
    let damaged = |what: &str| ExtractError::Damaged(format!("Word document: {what}"));
    let word = stream(&mut cfb, "/WordDocument").ok_or_else(|| damaged("no WordDocument stream"))?;
    if u16_at(&word, 0) != Some(0xA5EC) {
        return Err(damaged("not a Word 97-2003 file"));
    }
    let nfib = u16_at(&word, 2).unwrap_or(0);
    if nfib < 0x00C1 {
        return Err(ExtractError::Damaged("Word 6/95 documents are not supported".into()));
    }
    let flags = u16_at(&word, 0x0A).unwrap_or(0);
    if flags & 0x0100 != 0 {
        return Err(ExtractError::Protected);
    }
    let table_name = if flags & 0x0200 != 0 { "/1Table" } else { "/0Table" };
    let table = stream(&mut cfb, table_name).ok_or_else(|| damaged("no table stream"))?;

    // FIB: base (32 B) · csw · fibRgW · cslw · fibRgLw · cbRgFcLcb · fibRgFcLcb
    let csw = u16_at(&word, 32).ok_or_else(|| damaged("short FIB"))? as usize;
    let at_cslw = 34 + csw * 2;
    let cslw = u16_at(&word, at_cslw).ok_or_else(|| damaged("short FIB"))? as usize;
    let at_fclcb = at_cslw + 2 + cslw * 4 + 2;
    // fcClx / lcbClx are pair 33 of FibRgFcLcb97
    let fc_clx = u32_at(&word, at_fclcb + 33 * 8).ok_or_else(|| damaged("short FIB"))? as usize;
    let lcb_clx = u32_at(&word, at_fclcb + 33 * 8 + 4).ok_or_else(|| damaged("short FIB"))? as usize;
    let clx = table.get(fc_clx..fc_clx + lcb_clx).ok_or_else(|| damaged("CLX outside the table stream"))?;

    // skip Prc entries, find the Pcdt (0x02)
    let mut i = 0;
    let plc = loop {
        match clx.get(i) {
            Some(1) => i += 3 + u16_at(clx, i + 1).unwrap_or(0) as usize,
            Some(2) => {
                let lcb = u32_at(clx, i + 1).ok_or_else(|| damaged("bad piece table"))? as usize;
                break clx.get(i + 5..i + 5 + lcb).ok_or_else(|| damaged("bad piece table"))?;
            }
            _ => return Err(damaged("no piece table")),
        }
    };
    let n = plc.len().saturating_sub(4) / 12;
    let mut fields = Fields::default();
    for k in 0..n {
        if out.full() {
            break;
        }
        let (cp0, cp1) = (u32_at(plc, k * 4).unwrap_or(0), u32_at(plc, k * 4 + 4).unwrap_or(0));
        let count = cp1.saturating_sub(cp0) as usize;
        let pcd = 4 * (n + 1) + k * 8;
        let fc = u32_at(plc, pcd + 2).unwrap_or(0);
        let text = if fc & 0x4000_0000 != 0 {
            let at = ((fc & 0x3FFF_FFFF) / 2) as usize;
            let raw = word.get(at..at + count).unwrap_or(&[]);
            encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw).0.into_owned()
        } else {
            let at = fc as usize;
            utf16(word.get(at..at + count * 2).unwrap_or(&[]))
        };
        fields.push(&text, out);
    }
    Ok(())
}

/// Word's special characters: field codes (keep only the result), cell
/// and paragraph marks, page breaks, embedded objects.
#[derive(Default)]
struct Fields {
    /// Nesting of fields still in their code part (between 0x13 and 0x14).
    in_code: Vec<bool>,
    /// The last character was a cell mark (0x07).
    after_cell: bool,
}

impl Fields {
    fn push(&mut self, text: &str, out: &mut Out) {
        let mut s = String::with_capacity(text.len());
        for c in text.chars() {
            let cell = c == '\u{07}';
            let row_end = cell && self.after_cell;
            self.after_cell = cell && !row_end;
            match c {
                '\u{13}' => self.in_code.push(true),
                '\u{14}' => {
                    if let Some(top) = self.in_code.last_mut() {
                        *top = false;
                    }
                }
                '\u{15}' => {
                    self.in_code.pop();
                }
                _ if self.in_code.iter().any(|&c| c) => {}
                '\r' | '\u{0B}' | '\u{0C}' | '\u{0E}' => s.push('\n'),
                // a cell mark ends a cell; a second one right after ends the row
                '\u{07}' if row_end => {
                    if s.ends_with('\t') {
                        s.pop();
                    }
                    s.push('\n');
                }
                '\u{07}' => s.push('\t'),
                '\u{1E}' => s.push('-'),
                c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
                c => s.push(c),
            }
        }
        out.push(&s);
    }
}

// ---------------------------------------------------------------- .ppt

pub(crate) fn ppt(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut cfb = open(bytes)?;
    if cfb.exists("/EncryptedSummary") {
        return Err(ExtractError::Protected);
    }
    let doc = stream(&mut cfb, "/PowerPoint Document")
        .ok_or_else(|| ExtractError::Damaged("no PowerPoint Document stream".into()))?;
    const MAIN_MASTER: u16 = 0x03F8;
    const TEXT_CHARS: u16 = 0x0FA0;
    const TEXT_BYTES: u16 = 0x0FA8;
    // containers open at this point: (end offset, inside a master slide)
    let mut open: Vec<(usize, bool)> = Vec::new();
    let mut pos = 0usize;
    let mut any = false;
    while pos + 8 <= doc.len() && !out.full() {
        while open.last().is_some_and(|&(end, _)| pos >= end) {
            open.pop();
        }
        let ver_inst = u16_at(&doc, pos).unwrap_or(0);
        let kind = u16_at(&doc, pos + 2).unwrap_or(0);
        let len = u32_at(&doc, pos + 4).unwrap_or(0) as usize;
        let skip = open.last().is_some_and(|o| o.1);
        if ver_inst & 0x000F == 0x000F {
            // a container: look inside it
            open.push((pos + 8 + len, skip || kind == MAIN_MASTER));
            pos += 8;
            continue;
        }
        let body = doc.get(pos + 8..(pos + 8 + len).min(doc.len())).unwrap_or(&[]);
        if !skip {
            let text = match kind {
                TEXT_CHARS => Some(utf16(body)),
                TEXT_BYTES => Some(encoding_rs::WINDOWS_1252.decode_without_bom_handling(body).0.into_owned()),
                _ => None,
            };
            if let Some(t) = text {
                out.push(&t.replace(['\r', '\u{0B}'], "\n"));
                out.newline();
                any = true;
            }
        }
        pos += 8 + len;
    }
    if !any && doc.len() < 8 {
        return Err(ExtractError::Damaged("empty presentation".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- .msg

pub(crate) fn msg(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    // .msg is also the extension of Tcl/Tk language files (plain text):
    // anything that is not an Outlook container is simply not this format.
    let Ok(mut cfb) = open(bytes) else { return Err(ExtractError::Unsupported) };
    if !cfb.exists("/__properties_version1.0") {
        return Err(ExtractError::Unsupported);
    }
    // A property is "__substg1.0_<tag><type>": type 001F = UTF-16, 001E = 8-bit.
    let prop = |cfb: &mut Cfb, dir: &str, tag: &str| -> String {
        if let Some(b) = stream(cfb, &format!("{dir}/__substg1.0_{tag}001F")) {
            return utf16(&b).trim_end_matches('\0').to_string();
        }
        if let Some(b) = stream(cfb, &format!("{dir}/__substg1.0_{tag}001E")) {
            let b = b.strip_suffix(&[0]).unwrap_or(&b);
            return match std::str::from_utf8(b) {
                Ok(s) => s.to_string(),
                Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(b).0.into_owned(),
            };
        }
        String::new()
    };
    let mut headers = Headers {
        subject: prop(&mut cfb, "", "0037"),
        from: {
            let name = prop(&mut cfb, "", "0C1A");
            let mail = prop(&mut cfb, "", "5D01");
            match (name.is_empty(), mail.is_empty()) {
                (false, false) if name != mail => format!("{name} <{mail}>"),
                (false, _) => name,
                _ => mail,
            }
        },
        to: prop(&mut cfb, "", "0E04"),
        cc: prop(&mut cfb, "", "0E03"),
        date: String::new(),
    };
    // recipients' addresses, when the display list has only names
    let mut addresses = Vec::new();
    let storages: Vec<String> =
        cfb.read_storage("/").map(|it| it.map(|e| e.name().to_string()).collect()).unwrap_or_default();
    for s in storages.iter().filter(|s| s.starts_with("__recip_version1.0_")) {
        let mail = prop(&mut cfb, &format!("/{s}"), "39FE");
        let mail = if mail.is_empty() { prop(&mut cfb, &format!("/{s}"), "3003") } else { mail };
        if !mail.is_empty() && !headers.to.contains(&mail) && !headers.cc.contains(&mail) {
            addresses.push(mail);
        }
    }
    if !addresses.is_empty() {
        let extra = addresses.join(", ");
        headers.to = if headers.to.is_empty() { extra } else { format!("{} ({extra})", headers.to) };
    }
    headers.write(out);

    let body = prop(&mut cfb, "", "1000");
    if !body.trim().is_empty() {
        out.push(&body);
    } else if let Some(html) = stream(&mut cfb, "/__substg1.0_10130102") {
        out.push(&html_to_text(&crate::decode_text(&html).unwrap_or_default()));
    }
    out.newline();
    let names: Vec<String> = storages
        .iter()
        .filter(|s| s.starts_with("__attach_version1.0_"))
        .map(|s| {
            let long = prop(&mut cfb, &format!("/{s}"), "3707");
            if long.is_empty() {
                prop(&mut cfb, &format!("/{s}"), "3704")
            } else {
                long
            }
        })
        .filter(|n| !n.is_empty())
        .collect();
    attachments(&names, out);
    Ok(())
}
