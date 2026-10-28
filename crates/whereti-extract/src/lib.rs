//! Text extraction for content search and preview (PLAN.md §6.3).
//!
//! `extract` turns the bytes of a document into plain text. It never touches
//! the disk: the caller (a broker) reads the file and passes the bytes, so the
//! extractor can run in an isolated process with no file access (see `worker`).
//!
//! Every format is bounded: output is capped at `max_chars`, and no archive
//! entry is inflated beyond `MAX_ENTRY` (zip bombs).

mod archive;
mod email;
mod mailbox;
pub mod meaning;
mod ocr;
pub mod tags;
mod ole;
mod rtf;
mod spawn;
#[cfg(test)]
mod tests_mail_ole;
pub mod worker;

pub use rtf::{rtf_bytes_to_text, rtf_to_text};

use std::io::{Cursor, Read};

use quick_xml::events::Event;
use quick_xml::Reader;

/// Largest single entry read from an Office/ODF archive.
const MAX_ENTRY: u64 = 64 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum ExtractError {
    /// Not a format we read (or not text at all).
    Unsupported,
    /// Password-protected document.
    Protected,
    /// Damaged or not what its extension says.
    Damaged(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Unsupported => write!(f, "format not supported"),
            ExtractError::Protected => write!(f, "protected with a password"),
            ExtractError::Damaged(e) => write!(f, "damaged file: {e}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Extracted {
    pub text: String,
    /// The text was cut at `max_chars`.
    pub truncated: bool,
}

/// What kind of extractor a file extension maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Docx,
    Pptx,
    Spreadsheet,
    OpenDocument,
    Pdf,
    Rtf,
    Html,
    /// MIME e-mail (.eml)
    Email,
    /// Outlook message (.msg)
    Outlook,
    /// Word 97-2003 (.doc)
    WordBinary,
    /// PowerPoint 97-2003 (.ppt)
    PowerPointBinary,
    /// A picture: its text is read with the OCR of Windows.
    Image,
    /// ZIP, 7z, RAR: the names inside, and the text of the documents.
    Archive,
    /// EPUB e-book: its chapters (XHTML in a ZIP).
    Epub,
    /// FictionBook (.fb2): XML.
    FictionBook,
    /// XPS / OpenXPS: the text of its fixed pages.
    Xps,
    /// Outlook data file (.pst/.ost): read from an open file, not bytes
    /// (see worker::Sandbox::extract_file).
    Mailbox,
}

pub fn kind_of(ext: &str) -> Option<Kind> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => Kind::Docx,
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => Kind::Pptx,
        "xlsx" | "xlsm" | "xlsb" | "xls" | "xltx" | "xltm" | "ods" | "ots" => Kind::Spreadsheet,
        "odt" | "ott" | "odp" | "otp" => Kind::OpenDocument,
        "epub" => Kind::Epub,
        "fb2" => Kind::FictionBook,
        "xps" | "oxps" => Kind::Xps,
        "pst" => Kind::Mailbox,
        "pdf" => Kind::Pdf,
        "rtf" => Kind::Rtf,
        "html" | "htm" | "xhtml" => Kind::Html,
        "eml" => Kind::Email,
        "msg" => Kind::Outlook,
        "doc" | "dot" => Kind::WordBinary,
        "ppt" | "pps" | "pot" => Kind::PowerPointBinary,
        e if IMAGE_EXTS.contains(&e) || whereti_query::kinds::RAW.contains(&e) => Kind::Image,
        e if archive::is_archive(e) => Kind::Archive,
        e if TEXT_EXTS.contains(&e) => Kind::Text,
        _ => return None,
    })
}

/// Extensions with a converter, besides plain text.
const DOC_EXTS: &[&str] = &[
    "docx", "docm", "dotx", "dotm", "pptx", "pptm", "ppsx", "ppsm", "potx", "potm", "xlsx", "xlsm", "xlsb", "xls", "xltx",
    "xltm", "ods", "ots", "odt", "ott", "odp", "otp", "pdf", "rtf", "html", "htm", "xhtml", "eml", "msg", "doc", "dot", "ppt",
    "pps", "pot", "epub", "fb2", "xps", "oxps", "zip", "7z", "rar", "pst",
];

/// The end of a ZIP too large to send whole: `extract(ZIP_END, ...)` lists
/// its names from there (see archive::zip_tail_names).
pub const ZIP_END: &str = "zip-end";
/// The ends of a 7z too large to send whole: `extract(SEVENZ_ENDS, …)` with
/// `sevenz_ends(len, head, tail)` lists its names.
pub const SEVENZ_ENDS: &str = "7z-ends";

/// Packs a 7z's start (its 32-byte signature header), its end, and its
/// length, for `extract(SEVENZ_ENDS, …)`.
pub fn sevenz_ends(len: u64, head: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + 32 + tail.len());
    v.extend_from_slice(&len.to_le_bytes());
    let mut h = head[..head.len().min(32)].to_vec();
    h.resize(32, 0);
    v.extend_from_slice(&h);
    v.extend_from_slice(tail);
    v
}

/// Pictures whose text is read by OCR (HEIC only where its codec is installed).
/// (Windows decodes them: see whereti_query::kinds; icons are not read.)
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "jpe", "jfif", "png", "bmp", "dib", "tif", "tiff", "gif", "webp", "heic", "heif", "hif", "avif", "jxl", "jxr", "wdp"];

/// Windows has an OCR language installed (servers often have none).
pub fn ocr_available() -> bool {
    ocr::Ocr::new().is_some()
}

/// OCR languages to try first in this process (BCP-47 tags, "ro-RO"), for
/// example those of the user whose files are read; Windows' own choice
/// when none of them is installed. The worker takes them as `--ocr-lang`.
pub fn set_ocr_languages(tags: &[&str]) {
    ocr::set_languages(tags);
}

/// Every extension `extract` reads, lower case, without the dot.
pub fn extensions() -> Vec<&'static str> {
    DOC_EXTS.iter().chain(TEXT_EXTS).chain(IMAGE_EXTS).copied().collect()
}

/// Plain-text formats: documents, data and ~80 kinds of source code.
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonc", "xml", "yaml", "yml", "toml", "ini", "cfg",
    "conf", "srt", "vtt", "tex", "bib", "rst", "adoc", "org", "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx",
    "go", "java", "kt", "kts", "scala", "c", "h", "cc", "cpp", "hpp", "cs", "fs", "vb", "php", "rb", "pl", "lua",
    "swift", "m", "mm", "r", "jl", "dart", "zig", "nim", "ex", "exs", "erl", "hs", "clj", "sql", "sh", "bash",
    "zsh", "fish", "ps1", "psm1", "bat", "cmd", "css", "scss", "sass", "less", "vue", "svelte", "astro", "graphql",
    "proto", "gradle", "cmake", "mk", "dockerfile", "tf", "hcl", "nix", "env", "properties", "gitignore", "editorconfig",
];

pub fn extract(ext: &str, bytes: &[u8], max_chars: usize) -> Result<Extracted, ExtractError> {
    let mut out = Out::new(max_chars);
    if ext == ZIP_END {
        archive::zip_tail_names(bytes, &mut out)?;
        return Ok(out.finish());
    }
    if ext == SEVENZ_ENDS {
        archive::sevenz_end_names(bytes, &mut out)?;
        return Ok(out.finish());
    }
    let kind = kind_of(ext).ok_or(ExtractError::Unsupported)?;
    match kind {
        Kind::Text => out.push(&decode_text(bytes)?),
        Kind::Docx => docx(bytes, &mut out)?,
        Kind::Pptx => pptx(bytes, &mut out)?,
        Kind::Spreadsheet => spreadsheet(ext, bytes, &mut out)?,
        Kind::OpenDocument => odf(bytes, &mut out)?,
        Kind::Pdf => {
            let own = pdf(bytes, &mut out);
            // A scan has (almost) no text of its own: read its pages with OCR.
            // A PDF that pdf-extract cannot parse gets the same chance, since
            // Windows' own PDF reader is more forgiving.
            // It is judged on all the text it has, not on what fit the budget.
            if matches!(own, Ok(_) | Err(ExtractError::Damaged(_))) {
                let own_chars = own.as_ref().map_or(0, |n| *n);
                let scanned = ocr::Ocr::new().and_then(|o| o.pdf_if_scanned(bytes, own_chars).ok().flatten());
                if let Some((text, truncated)) = scanned {
                    if !text.is_empty() {
                        out = Out::new(max_chars);
                        out.push(&text);
                        out.truncated |= truncated;
                        return Ok(out.finish());
                    }
                }
            }
            own?;
        }
        Kind::Image => {
            let o = ocr::Ocr::new().ok_or(ExtractError::Unsupported)?;
            let text = o.picture(bytes).map_err(|e| ExtractError::Damaged(e.message().to_string()))?;
            out.push(&text);
        }
        Kind::Rtf => out.push(&rtf_bytes_to_text(bytes)),
        Kind::Html => out.push(&html_to_text(&decode_text(bytes)?)),
        Kind::Email => email::eml(bytes, &mut out)?,
        Kind::Outlook => ole::msg(bytes, &mut out)?,
        Kind::WordBinary => ole::doc(bytes, &mut out)?,
        Kind::PowerPointBinary => ole::ppt(bytes, &mut out)?,
        Kind::Epub => epub(bytes, &mut out)?,
        Kind::FictionBook => out.push(&html_to_text(&decode_text(bytes)?)),
        Kind::Xps => xps(bytes, &mut out)?,
        // a mailbox in memory (tests, small files)
        Kind::Mailbox => {
            let bytes = bytes.to_vec();
            mailbox::mailbox(move || Ok(Cursor::new(bytes.clone())), &mut out)?
        }
        Kind::Archive => match ext.to_ascii_lowercase().as_str() {
            "zip" => archive::zip(bytes, &mut out)?,
            "7z" => archive::sevenz(bytes, &mut out)?,
            _ => archive::rar(bytes, &mut out)?,
        },
    }
    Ok(out.finish())
}

thread_local! {
    /// When the caller stops waiting: slow work (OCR, the documents inside
    /// an archive) stops by then and hands back what it has.
    static DEADLINE: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// The time by which this thread's extraction must be done, if any.
pub(crate) fn deadline() -> Option<std::time::Instant> {
    DEADLINE.with(|d| d.get())
}

/// Sets the deadline of this thread's extractions (the worker, from its
/// caller's timeout).
pub(crate) fn set_deadline(at: Option<std::time::Instant>) {
    DEADLINE.with(|d| d.set(at));
}

/// Runs `f` with the deadline moved to `until` if that is sooner.
pub(crate) fn within<T>(until: std::time::Instant, f: impl FnOnce() -> T) -> T {
    let before = deadline();
    set_deadline(Some(before.map_or(until, |b| b.min(until))));
    let result = f();
    set_deadline(before);
    result
}

/// What a slow reader has so far (see `keep_partial`).
pub(crate) type Partial = std::sync::Arc<std::sync::Mutex<Option<Extracted>>>;

thread_local! {
    /// Where this thread's slow readers (archives, mailboxes) leave what they
    /// have so far: the worker hands it back if one parser that never looks
    /// at the clock is still busy at its deadline.
    static PARTIAL: std::cell::RefCell<Option<Partial>> = const { std::cell::RefCell::new(None) };
}

/// Sets (or, `None`, clears) where this thread's slow readers leave what
/// they have so far.
pub(crate) fn keep_partial(to: Option<Partial>) {
    PARTIAL.with(|p| *p.borrow_mut() = to);
}

/// Leaves `out` as it is now, cut, where `keep_partial` said.
pub(crate) fn save_partial(out: &Out) {
    PARTIAL.with(|p| {
        if let Some(to) = p.borrow().as_ref() {
            let text = Out { text: out.text.clone(), budget: 0, truncated: true, cut: true }.finish();
            *to.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
        }
    });
}

/// Runs `f` (one document inside another) without its own partial text
/// replacing the outer reader's.
pub(crate) fn inner<T>(f: impl FnOnce() -> T) -> T {
    let outer = PARTIAL.with(|p| p.borrow_mut().take());
    let result = f();
    keep_partial(outer);
    result
}

/// Collects text up to a character budget, tidying whitespace.
struct Out {
    text: String,
    budget: usize,
    /// Something was left out (time ran out, too many names...).
    truncated: bool,
    /// Text was refused for want of budget.
    cut: bool,
}

impl Out {
    fn new(max_chars: usize) -> Out {
        Out { text: String::new(), budget: max_chars, truncated: false, cut: false }
    }

    /// Text was refused for want of budget: callers stop adding more. A
    /// text that fits exactly is not cut, so the budget may be spent before
    /// this says so (`spent`).
    fn full(&self) -> bool {
        self.cut
    }

    /// Nothing more fits: for callers whose next step is slow (a document
    /// inside an archive), which skip it, and count it as left out.
    fn spent(&self) -> bool {
        self.budget == 0
    }

    fn push(&mut self, s: &str) {
        for c in s.chars() {
            let c = if c == '\r' || (c.is_control() && c != '\n' && c != '\t') { continue } else { c };
            if self.budget == 0 {
                // white space past the end is not text left out
                if !c.is_whitespace() {
                    self.cut = true;
                    return;
                }
                continue;
            }
            self.text.push(c);
            self.budget -= 1;
        }
    }

    fn newline(&mut self) {
        if !self.text.ends_with('\n') && !self.text.is_empty() {
            self.push("\n");
        }
    }

    fn finish(mut self) -> Extracted {
        // collapse runs of blank lines
        let mut tidy = String::with_capacity(self.text.len());
        let mut blank = 0;
        for line in self.text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                blank += 1;
                if blank > 1 {
                    continue;
                }
            } else {
                blank = 0;
            }
            tidy.push_str(line);
            tidy.push('\n');
        }
        self.text = tidy.trim().to_string();
        Extracted { text: self.text, truncated: self.truncated || self.cut }
    }
}

/// UTF-8 / UTF-16 (BOM) or a legacy code page guessed from the bytes.
/// Refuses binary data. `bytes` may be only the head of a file, cut in the
/// middle of a character: that character is dropped, not the encoding.
pub fn decode_text(bytes: &[u8]) -> Result<String, ExtractError> {
    if let Some((enc, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        let mut body = &bytes[bom..];
        if enc == encoding_rs::UTF_8 {
            body = whole_utf8(body, true);
        } else {
            // UTF-16: whole code units, and no high surrogate without its pair
            body = &body[..body.len() & !1];
            if let [.., a, b] = body {
                let unit = if enc == encoding_rs::UTF_16LE { u16::from_le_bytes([*a, *b]) } else { u16::from_be_bytes([*a, *b]) };
                if (0xD800..0xDC00).contains(&unit) {
                    body = &body[..body.len() - 2];
                }
            }
        }
        return Ok(enc.decode_without_bom_handling(body).0.into_owned());
    }
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return Err(ExtractError::Unsupported); // binary
    }
    let bytes = whole_utf8(bytes, false);
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(bytes, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    Ok(enc.decode_without_bom_handling(bytes).0.into_owned())
}

/// `bytes` without an unfinished UTF-8 character at its very end (a head
/// cut mid character), when everything before it is valid UTF-8 and, unless
/// the text is `sure` to be UTF-8 (a BOM), has a character of more than one
/// byte: else the last byte may be a legacy code page's letter ("café" in
/// Windows-1252 ends in 0xE9, which also starts a UTF-8 character).
fn whole_utf8(bytes: &[u8], sure: bool) -> &[u8] {
    match std::str::from_utf8(bytes) {
        Err(e) if e.error_len().is_none() && bytes.len() - e.valid_up_to() <= 3 => {
            let valid = &bytes[..e.valid_up_to()];
            if sure || !valid.is_ascii() {
                valid
            } else {
                bytes
            }
        }
        _ => bytes,
    }
}

fn zip_archive(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, ExtractError> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| {
        // An encrypted Office file is an OLE container, not a zip.
        if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
            ExtractError::Protected
        } else {
            ExtractError::Damaged(e.to_string())
        }
    })
}

fn zip_names(archive: &zip::ZipArchive<Cursor<&[u8]>>) -> Vec<String> {
    archive.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect()
}

fn zip_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    let file = archive.by_name(name).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_ENTRY).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Walks XML, appending the text of `text_tag` elements and a newline at the
/// end of each `para_tag` element. Entities are resolved.
fn xml_text(xml: &[u8], text_tags: &[&str], para_tags: &[&str], tab_tags: &[&str], out: &mut Out) {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut inside = 0usize;
    loop {
        if out.full() {
            return;
        }
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if text_tags.contains(&e.local_name().as_ref()) => inside += 1,
            Ok(Event::End(e)) => {
                let name = e.local_name();
                if text_tags.contains(&name.as_ref()) {
                    inside = inside.saturating_sub(1);
                }
                if para_tags.contains(&name.as_ref()) {
                    out.newline();
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                if tab_tags.contains(&name.as_ref()) {
                    out.push("\t");
                } else if para_tags.contains(&name.as_ref()) {
                    out.newline();
                }
            }
            Ok(Event::Text(t)) if inside > 0 => out.push(t.as_ref()),
            Ok(Event::GeneralRef(r)) if inside > 0 => out.push(&resolve_entity(&r.into_inner()).unwrap_or_default()),
            Ok(Event::Eof) | Err(_) => return,
            _ => {}
        }
        buf.clear();
    }
}

/// The text of an entity (`amp`, `eacute`, `#233`, `#xE9`); `None` when
/// it is not one.
fn resolve_entity(name: &str) -> Option<String> {
    let num = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        Some(u32::from_str_radix(hex, 16).ok()?)
    } else if let Some(dec) = name.strip_prefix('#') {
        Some(dec.parse::<u32>().ok()?)
    } else {
        None
    };
    if let Some(n) = num {
        // 128-159 mean what they do in Windows-1252, as browsers read them
        if (0x80..0xA0).contains(&n) {
            return Some(encoding_rs::WINDOWS_1252.decode_without_bom_handling(&[n as u8]).0.into_owned());
        }
        return Some(char::from_u32(n).filter(|c| *c != '\0').map(String::from).unwrap_or_default());
    }
    named_entity(name).map(|c| match c {
        // spaces and invisible marks: a plain space, or nothing
        '\u{a0}' | '\u{2002}' | '\u{2003}' | '\u{2009}' => " ".to_string(),
        '\u{ad}' | '\u{200c}' | '\u{200d}' | '\u{200e}' | '\u{200f}' => String::new(),
        c => c.to_string(),
    })
}

/// Latin-1 entities from `nbsp` (160) to `yuml` (255), in order.
const LATIN1_ENTITIES: [&str; 96] = [
    "nbsp", "iexcl", "cent", "pound", "curren", "yen", "brvbar", "sect", "uml", "copy", "ordf", "laquo", "not", "shy", "reg",
    "macr", "deg", "plusmn", "sup2", "sup3", "acute", "micro", "para", "middot", "cedil", "sup1", "ordm", "raquo", "frac14",
    "frac12", "frac34", "iquest", "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig", "Ccedil", "Egrave",
    "Eacute", "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve", "Oacute", "Ocirc", "Otilde",
    "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute", "THORN", "szlig", "agrave", "aacute", "acirc",
    "atilde", "auml", "aring", "aelig", "ccedil", "egrave", "eacute", "ecirc", "euml", "igrave", "iacute", "icirc", "iuml",
    "eth", "ntilde", "ograve", "oacute", "ocirc", "otilde", "ouml", "divide", "oslash", "ugrave", "uacute", "ucirc", "uuml",
    "yacute", "thorn", "yuml",
];

/// The named entities pages commonly use: Latin-1, quotes, dashes, signs.
fn named_entity(name: &str) -> Option<char> {
    if let Some(i) = LATIN1_ENTITIES.iter().position(|e| *e == name) {
        return char::from_u32(160 + i as u32);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "OElig" => 'Œ',
        "oelig" => 'œ',
        "Scaron" => 'Š',
        "scaron" => 'š',
        "Yuml" => 'Ÿ',
        "fnof" => 'ƒ',
        "circ" => 'ˆ',
        "tilde" => '˜',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "dagger" => '†',
        "Dagger" => '‡',
        "bull" => '•',
        "hellip" => '…',
        "permil" => '‰',
        "prime" => '′',
        "Prime" => '″',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "oline" => '‾',
        "euro" => '€',
        "trade" => '™',
        "larr" => '←',
        "uarr" => '↑',
        "rarr" => '→',
        "darr" => '↓',
        "harr" => '↔',
        "minus" => '−',
        "infin" => '∞',
        "ne" => '≠',
        "le" => '≤',
        "ge" => '≥',
        "asymp" => '≈',
        _ => return None,
    })
}

fn docx(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let doc = zip_entry(&mut zip, "word/document.xml").ok_or_else(|| ExtractError::Damaged("no word/document.xml".into()))?;
    xml_text(&doc, &["t"], &["p", "br", "cr"], &["tab"], out);
    // headers, footers, footnotes, endnotes, comments
    let mut extra: Vec<String> = zip_names(&zip)
        .into_iter()
        .filter(|n| {
            n.starts_with("word/header") || n.starts_with("word/footer") || n == "word/footnotes.xml" || n == "word/endnotes.xml" || n == "word/comments.xml"
        })
        .collect();
    extra.sort();
    for name in extra {
        if let Some(xml) = zip_entry(&mut zip, &name) {
            out.newline();
            xml_text(&xml, &["t"], &["p", "br", "cr"], &["tab"], out);
        }
    }
    Ok(())
}

fn pptx(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    // slides in their real order (slide2 before slide10), then speaker notes
    let mut slides: Vec<(u32, String)> = zip_names(&zip)
        .into_iter()
        .filter_map(|n| {
            let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse().ok()?;
            Some((num, n))
        })
        .collect();
    slides.sort();
    if slides.is_empty() {
        return Err(ExtractError::Damaged("no slides".into()));
    }
    for (_, name) in slides {
        if let Some(xml) = zip_entry(&mut zip, &name) {
            // <a:br/> is a line break (<a:tab> only defines tab stops)
            xml_text(&xml, &["t"], &["p", "br"], &[], out);
            out.newline();
        }
    }
    let mut notes: Vec<String> = zip_names(&zip).into_iter().filter(|n| n.starts_with("ppt/notesSlides/")).collect();
    notes.sort();
    for name in notes {
        if let Some(xml) = zip_entry(&mut zip, &name) {
            xml_text(&xml, &["t"], &["p", "br"], &[], out);
        }
    }
    Ok(())
}

/// An e-book: its chapters in reading order (the OPF spine), else every
/// XHTML file in name order.
fn epub(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let names = zip_names(&zip);
    let mut chapters: Vec<String> = Vec::new();
    if let Some(opf) = names.iter().find(|n| n.to_ascii_lowercase().ends_with(".opf")).cloned() {
        let base = opf.rsplit_once('/').map_or(String::new(), |(b, _)| format!("{b}/"));
        if let Some(xml) = zip_entry(&mut zip, &opf) {
            let xml = String::from_utf8_lossy(&xml);
            // id -> href, then the spine's itemrefs in order
            let mut hrefs = std::collections::HashMap::new();
            for tag in xml.split('<').filter(|t| t.starts_with("item ") || t.starts_with("opf:item ")) {
                if let (Some(id), Some(href)) = (xml_attr(tag, "id"), xml_attr(tag, "href")) {
                    hrefs.insert(id, href);
                }
            }
            for tag in xml.split('<').filter(|t| t.starts_with("itemref") || t.starts_with("opf:itemref")) {
                if let Some(href) = xml_attr(tag, "idref").and_then(|id| hrefs.get(&id)) {
                    // the name in the ZIP, matched in any letter case
                    let path = epub_path(&base, href);
                    let found = names.iter().find(|n| **n == path).or_else(|| names.iter().find(|n| n.eq_ignore_ascii_case(&path)));
                    if let Some(name) = found {
                        if !chapters.contains(name) {
                            chapters.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    // no spine, or none of it found: every XHTML file
    if chapters.is_empty() {
        chapters = names.into_iter().filter(|n| {
            let n = n.to_ascii_lowercase();
            n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm")
        }).collect();
        chapters.sort();
    }
    for name in chapters {
        if out.full() {
            break;
        }
        if let Some(html) = zip_entry(&mut zip, &name) {
            out.push(&html_to_text(&String::from_utf8_lossy(&html)));
            out.newline();
        }
    }
    Ok(())
}

/// An attribute of a tag (`name="v"` or `name='v'`), entities resolved.
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    loop {
        let at = from + tag[from..].find(name)?;
        from = at + name.len();
        // a whole attribute name: `id` is not the end of `idref` or `xml:id`
        let before = tag[..at].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let quote = rest.chars().next().filter(|q| *q == '"' || *q == '\'')?;
        let value = rest[1..].split(quote).next()?;
        return Some(html_to_text(value));
    }
}

/// The ZIP name a chapter's `href` points to, from the folder of the OPF:
/// no `#fragment`, percent escapes decoded, `.` and `..` resolved.
fn epub_path(base: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or("");
    // percent escapes are UTF-8 bytes
    let mut bytes = Vec::with_capacity(href.len());
    let raw = href.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hex = raw.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (raw[i], hex) {
            (b'%', Some(b)) => {
                bytes.push(b);
                i += 3;
            }
            (c, _) => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    let href = String::from_utf8_lossy(&bytes);
    let joined = match href.strip_prefix('/') {
        Some(root) => root.to_string(),
        None => format!("{base}{href}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// XPS / OpenXPS: the `UnicodeString` of every glyph run, page by page.
fn xps(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let mut pages: Vec<String> = zip_names(&zip).into_iter().filter(|n| n.to_ascii_lowercase().ends_with(".fpage")).collect();
    // Pages/1.fpage before Pages/10.fpage
    let num = |n: &str| n.rsplit('/').next().and_then(|f| f.split('.').next()).and_then(|d| d.parse::<u32>().ok()).unwrap_or(u32::MAX);
    pages.sort_by(|a, b| (num(a), a).cmp(&(num(b), b)));
    for page in pages {
        if out.full() {
            break;
        }
        let Some(xml) = zip_entry(&mut zip, &page) else { continue };
        let xml = String::from_utf8_lossy(&xml);
        for part in xml.split("UnicodeString=\"").skip(1) {
            let text = part.split('"').next().unwrap_or("");
            let text = text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'");
            // "{}" escapes a leading brace
            out.push(text.strip_prefix("{}").unwrap_or(&text));
            out.push(" ");
        }
        out.newline();
    }
    Ok(())
}

fn odf(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let xml = zip_entry(&mut zip, "content.xml").ok_or_else(|| ExtractError::Damaged("no content.xml".into()))?;
    xml_text(&xml, &["p", "h", "span", "a"], &["p", "h", "line-break"], &["tab", "s"], out);
    Ok(())
}

fn spreadsheet(ext: &str, bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    use calamine::Reader as _;
    let mut book = calamine::open_workbook_auto_from_rs(Cursor::new(bytes)).map_err(|e| {
        let msg = e.to_string();
        if msg.to_ascii_lowercase().contains("password") || msg.to_ascii_lowercase().contains("encrypt") {
            ExtractError::Protected
        } else {
            ExtractError::Damaged(format!("{ext}: {msg}"))
        }
    })?;
    for name in book.sheet_names().to_vec() {
        if out.full() {
            break;
        }
        out.push(&name);
        out.newline();
        // XLSX and XLSB are read cell by cell: a whole sheet in memory is
        // rows x columns, huge for a sheet with one cell far away
        let streamed = match &mut book {
            calamine::Sheets::Xlsx(x) => x.worksheet_cells_reader(&name).ok().map(|mut r| {
                let mut row = Row::default();
                while let Ok(Some(cell)) = r.next_cell() {
                    if !row.add(cell.get_position().0, cell_text(&cell.get_value().clone().into()), out) {
                        break;
                    }
                }
                row.end(out);
            }),
            calamine::Sheets::Xlsb(x) => x.worksheet_cells_reader(&name).ok().map(|mut r| {
                let mut row = Row::default();
                while let Ok(Some(cell)) = r.next_cell() {
                    if !row.add(cell.get_position().0, cell_text(&cell.get_value().clone().into()), out) {
                        break;
                    }
                }
                row.end(out);
            }),
            _ => None,
        };
        if streamed.is_none() {
            if let Ok(range) = book.worksheet_range(&name) {
                for row in range.rows() {
                    if out.full() {
                        break;
                    }
                    let cells: Vec<String> = row.iter().map(cell_text).filter(|s| !s.is_empty()).collect();
                    if !cells.is_empty() {
                        out.push(&cells.join("\t"));
                        out.newline();
                    }
                }
            }
        }
        out.newline();
    }
    Ok(())
}

/// The cells of one sheet row, as they stream in.
#[derive(Default)]
struct Row {
    at: Option<u32>,
    cells: Vec<String>,
}

impl Row {
    /// Adds a cell of row `row`, writing the row before when it changes.
    /// `false` once the budget is spent.
    fn add(&mut self, row: u32, text: String, out: &mut Out) -> bool {
        if self.at != Some(row) {
            self.end(out);
            self.at = Some(row);
        }
        if !text.is_empty() {
            self.cells.push(text);
        }
        !out.full()
    }

    fn end(&mut self, out: &mut Out) {
        if !self.cells.is_empty() {
            out.push(&self.cells.join("\t"));
            out.newline();
            self.cells.clear();
        }
    }
}

/// Cell text as a person reads it: 3591.5525890622844 → 3591.55, a date
/// as 2024-03-15 (with its time when it has one).
fn cell_text(c: &calamine::Data) -> String {
    match c {
        calamine::Data::Float(f) if f.fract() != 0.0 => {
            let s = format!("{f:.2}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        calamine::Data::DateTime(d) if d.is_datetime() => {
            let (y, mo, day, h, mi, s, _) = d.to_ymd_hms_milli();
            let time = match (h, mi, s) {
                (0, 0, 0) => String::new(),
                (h, mi, 0) => format!("{h:02}:{mi:02}"),
                (h, mi, s) => format!("{h:02}:{mi:02}:{s:02}"),
            };
            if d.as_f64() < 1.0 && !time.is_empty() {
                // a time of day alone
                time
            } else if time.is_empty() {
                format!("{y:04}-{mo:02}-{day:02}")
            } else {
                format!("{y:04}-{mo:02}-{day:02} {time}")
            }
        }
        other => other.to_string(),
    }
}

/// The PDF's own text; returns how many letters and digits it has in all
/// (more than `out` may keep).
fn pdf(bytes: &[u8], out: &mut Out) -> Result<usize, ExtractError> {
    if !bytes.starts_with(b"%PDF") {
        return Err(ExtractError::Damaged("not a PDF".into()));
    }
    // The PDF parser may panic on malformed files; the worker process is the
    // real isolation, this keeps an in-process caller alive too.
    let text = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes))
        .map_err(|_| ExtractError::Damaged("the PDF parser failed".into()))?
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_ascii_lowercase().contains("encrypt") {
                ExtractError::Protected
            } else {
                ExtractError::Damaged(msg)
            }
        })?;
    out.push(&text);
    Ok(text.chars().filter(|c| c.is_alphanumeric()).count())
}

/// Tags that start or end a line of their own (HTML, and FictionBook's).
const BLOCK_TAGS: &[&str] = &[
    "p", "br", "div", "li", "ul", "ol", "dl", "dt", "dd", "tr", "table", "thead", "tbody", "tfoot", "caption", "h1", "h2",
    "h3", "h4", "h5", "h6", "hr", "title", "blockquote", "pre", "section", "article", "header", "footer", "nav", "aside",
    "main", "figure", "figcaption", "address", "form", "fieldset", "legend", "option", "body", "v", "subtitle",
    "empty-line", "stanza", "epigraph", "text-author",
];

/// Text of an HTML page: no tags, scripts or styles; entities resolved.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let b = html.as_bytes();
    // the next '>' as last found: (searched from, found at), so a page full
    // of bare '<' is read in one pass, not once per '<'
    let mut gt: Option<(usize, Option<usize>)> = None;
    while i < b.len() {
        if b[i] == b'<' {
            if lower[i..].starts_with("<!--") {
                i = lower[i..].find("-->").map_or(b.len(), |p| i + p + 3);
                continue;
            }
            for skip in ["script", "style", "noscript", "binary"] {
                if lower[i + 1..].starts_with(skip) {
                    let close = format!("</{skip}");
                    i = lower[i..].find(&close).map_or(b.len(), |p| i + p);
                    break;
                }
            }
            if i >= b.len() {
                break;
            }
            let next = match gt {
                Some((from, at)) if from <= i && at.is_none_or(|p| p >= i) => at,
                _ => {
                    let at = b[i..].iter().position(|&c| c == b'>').map(|p| i + p);
                    gt = Some((i, at));
                    at
                }
            };
            let end = next.map_or(b.len(), |p| p + 1);
            let name: String = lower[i + 1..end]
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if BLOCK_TAGS.contains(&name.as_str()) {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            } else if (name == "td" || name == "th") && !lower[i + 1..].starts_with('/') && !out.is_empty() && !out.ends_with(['\n', '\t']) {
                // table cells side by side, never glued together
                out.push('\t');
            }
            i = end;
        } else if b[i] == b'&' {
            // `&name;` or `&#123;`: a short run of letters, digits and '#'
            let end = b[i + 1..]
                .iter()
                .take(33)
                .position(|&c| !(c.is_ascii_alphanumeric() || c == b'#'))
                .map(|p| i + 1 + p)
                .filter(|&e| e > i + 1 && b[e] == b';');
            match end.and_then(|e| Some((e, resolve_entity(&html[i + 1..e])?))) {
                Some((e, text)) => {
                    out.push_str(&text);
                    i = e + 1;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            }
        } else {
            let next = html[i..].find(['<', '&']).map_or(b.len(), |p| i + p);
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn epub_chapters_in_spine_order_and_xps_pages() {
        let opf = r#"<package><manifest><item id="c2" href="text/two.xhtml"/><item id="c1" href="text/one.xhtml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#;
        let book = make_zip(&[
            ("META-INF/container.xml", "<container/>"),
            ("OEBPS/content.opf", opf),
            ("OEBPS/text/two.xhtml", "<html><body><p>Capitolul doi</p></body></html>"),
            ("OEBPS/text/one.xhtml", "<html><body><p>Capitolul unu</p></body></html>"),
        ]);
        let t = extract("epub", &book, 10_000).unwrap().text;
        assert!(t.find("unu").unwrap() < t.find("doi").unwrap(), "{t}");

        let page = |s: &str| format!(r#"<FixedPage><Glyphs UnicodeString="{s}" /></FixedPage>"#);
        let xps = make_zip(&[
            ("Documents/1/Pages/10.fpage", &page("pagina zece")),
            ("Documents/1/Pages/2.fpage", &page("pagina doi &amp; trei")),
        ]);
        let t = extract("oxps", &xps, 10_000).unwrap().text;
        assert!(t.contains("pagina doi & trei") && t.find("doi").unwrap() < t.find("zece").unwrap(), "{t}");
        // templates read like the documents they make
        assert_eq!(kind_of("ott"), Some(Kind::OpenDocument));
        assert_eq!(kind_of("xltx"), Some(Kind::Spreadsheet));
        assert_eq!(kind_of("potm"), Some(Kind::Pptx));
    }

    fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (name, body) in files {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn plain_text_in_several_encodings() {
        assert_eq!(decode_text("Mașină".as_bytes()).unwrap(), "Mașină");
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("Șură".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&utf16).unwrap(), "Șură");
        // Windows-1250 (Central European): "ă" = 0xE3
        let cp1250 = [b'c', b'a', b's', 0xE3, b' ', b'm', b'a', b'r', b'e'];
        assert!(decode_text(&cp1250).unwrap().starts_with("cas"));
        assert_eq!(decode_text(&[0x4D, 0x5A, 0x00, 0x00]), Err(ExtractError::Unsupported));
    }

    #[test]
    fn docx_paragraphs_tabs_and_entities() {
        let doc = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:r><w:t>Factură</w:t></w:r><w:r><w:tab/><w:t>total &amp; TVA</w:t></w:r></w:p>
            <w:p><w:r><w:t xml:space="preserve">Termen de livrare: 15 zile</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let footer = r#"<w:ftr xmlns:w="w"><w:p><w:r><w:t>Pagina 1</w:t></w:r></w:p></w:ftr>"#;
        let bytes = make_zip(&[("word/document.xml", doc), ("word/footer1.xml", footer)]);
        let e = extract("docx", &bytes, 10_000).unwrap();
        assert_eq!(e.text, "Factură\ttotal & TVA\nTermen de livrare: 15 zile\nPagina 1");
        assert!(!e.truncated);
    }

    #[test]
    fn pptx_slides_in_numeric_order() {
        let slide = |t: &str| format!(r#"<p:sld xmlns:a="a" xmlns:p="p"><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>"#);
        let bytes = make_zip(&[
            ("ppt/slides/slide10.xml", &slide("zece")),
            ("ppt/slides/slide2.xml", &slide("doi")),
            ("ppt/slides/slide1.xml", &slide("unu")),
        ]);
        assert_eq!(extract("pptx", &bytes, 10_000).unwrap().text, "unu\ndoi\nzece");
    }

    #[test]
    fn odt_text() {
        let content = r#"<office:document-content xmlns:text="t"><office:body><office:text>
            <text:h>Titlu</text:h><text:p>Primul <text:span>paragraf</text:span></text:p>
        </office:text></office:body></office:document-content>"#;
        let bytes = make_zip(&[("content.xml", content)]);
        assert_eq!(extract("odt", &bytes, 10_000).unwrap().text, "Titlu\nPrimul paragraf");
    }

    #[test]
    fn rtf_and_html() {
        let html = "<html><head><style>p{}</style><script>var x=1;</script></head><body><p>Ofertă &amp; preț</p><p>Linia&nbsp;2</p></body></html>";
        assert_eq!(extract("html", html.as_bytes(), 1000).unwrap().text, "Ofertă & preț\nLinia 2");
    }

    #[test]
    fn a_head_cut_mid_character_keeps_its_encoding() {
        // the indexer reads 8 MB, the preview 240 KB: the cut falls anywhere
        for text in ["Mașină de spălat", "Привет, мир"] {
            let bytes = text.as_bytes();
            for cut in 1..bytes.len() {
                let got = decode_text(&bytes[..cut]).unwrap();
                let whole = text.char_indices().map(|(i, _)| i).take_while(|&i| i <= cut).last().unwrap();
                if bytes[..whole].is_ascii() {
                    // nothing yet says UTF-8: the cut byte reads as a legacy letter
                    assert!(got.starts_with(&text[..whole]) && got.chars().count() <= whole + 1, "{cut}: {got:?}");
                    continue;
                }
                assert!(text.starts_with(&got), "{cut}: {got:?}");
                assert!(got.len() + 3 >= cut, "{cut}: {got:?}");
            }
        }
        // a legacy file, plain ASCII but for its last letter, keeps it
        assert_eq!(decode_text(b"caf\xE9").unwrap(), "café");
        assert_eq!(decode_text(b"Un\xEE").unwrap(), "Unî");
        // the same letter after UTF-8 text: a cut character, dropped
        assert_eq!(decode_text("ți caf\u{e9}".as_bytes().split_last().unwrap().1).unwrap(), "ți caf");
        // with a BOM it is UTF-8 whatever comes before
        assert_eq!(decode_text(b"\xEF\xBB\xBFcaf\xC3").unwrap(), "caf");
        // UTF-16 with a BOM, cut after an odd number of bytes or inside a pair
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("Șură 😀".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&utf16[..utf16.len() - 1]).unwrap(), "Șură ");
        assert_eq!(decode_text(&utf16[..utf16.len() - 2]).unwrap(), "Șură ");
        assert_eq!(decode_text(&utf16[..7]).unwrap(), "Șu");
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("Șură".encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
        assert_eq!(decode_text(&be[..be.len() - 1]).unwrap(), "Șur");
    }

    #[test]
    fn html_entities_cells_and_headings() {
        let html = "<h5>Meniu</h5><p>caf&eacute; &amp; cr&egrave;me &euro;5 &ndash; &copy; &#8217;x&#x2019; &#150; &bogus; R&D</p>\
                    <table><tr><th>Nume</th><th>Pret</th></tr><tr><td>Ceai</td><td>7</td></tr></table><h6>Final</h6>";
        assert_eq!(html_to_text(html), "\nMeniu\ncafé & crème €5 – © ’x’ – &bogus; R&D\nNume\tPret\nCeai\t7\nFinal\n");
        // a comment is skipped whole, even with a '>' inside
        assert_eq!(html_to_text("a<!-- x > y -->b"), "ab");
        // many bare '<' and '&' are read in one pass
        let junk = "<&".repeat(200_000);
        let t = std::time::Instant::now();
        let _ = html_to_text(&junk);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn a_full_budget_marks_the_text_truncated() {
        let doc = r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>abcd</w:t></w:r></w:p><w:p><w:r><w:t>efgh</w:t></w:r></w:p></w:body></w:document>"#;
        let e = extract("docx", &make_zip(&[("word/document.xml", doc)]), 4).unwrap();
        assert_eq!(e.text, "abcd");
        assert!(e.truncated);
        let slide = |t: &str| format!(r#"<p:sld xmlns:a="a" xmlns:p="p"><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>"#);
        let e = extract("pptx", &make_zip(&[("ppt/slides/slide1.xml", &slide("unu")), ("ppt/slides/slide2.xml", &slide("doi"))]), 3).unwrap();
        assert!(e.truncated, "{e:?}");
        // an exact fit leaves nothing out
        let doc = r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>abcd</w:t></w:r></w:p><w:p></w:p></w:body></w:document>"#;
        let e = extract("docx", &make_zip(&[("word/document.xml", doc)]), 4).unwrap();
        assert_eq!((e.text.as_str(), e.truncated), ("abcd", false));
        let e = extract("pptx", &make_zip(&[("ppt/slides/slide1.xml", &slide("unu")), ("ppt/slides/slide2.xml", &slide(""))]), 3).unwrap();
        assert_eq!((e.text.as_str(), e.truncated), ("unu", false));
        assert_eq!(extract("txt", b"abcd\r\n\n", 4).unwrap(), Extracted { text: "abcd".into(), truncated: false });
        let z = make_zip(&[("a.jpg", "x")]);
        assert_eq!(extract("zip", &z, 5).unwrap(), Extracted { text: "a.jpg".into(), truncated: false });
        assert!(extract("zip", &make_zip(&[("a.jpg", "x"), ("b.jpg", "y")]), 5).unwrap().truncated);
    }

    #[test]
    fn line_breaks_and_endnotes_separate_words() {
        let slide = r#"<p:sld xmlns:a="a" xmlns:p="p"><a:p><a:r><a:t>unu</a:t></a:r><a:br/><a:r><a:t>doi</a:t></a:r></a:p></p:sld>"#;
        assert_eq!(extract("pptx", &make_zip(&[("ppt/slides/slide1.xml", slide)]), 1000).unwrap().text, "unu\ndoi");
        let content = r#"<office:document-content xmlns:text="t"><text:p>unu<text:line-break/>doi</text:p></office:document-content>"#;
        assert_eq!(extract("odt", &make_zip(&[("content.xml", content)]), 1000).unwrap().text, "unu\ndoi");
        let doc = r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
        let header = r#"<w:hdr xmlns:w="w"><w:p><w:r><w:t>Firma</w:t><w:br/><w:t>Adresa</w:t><w:tab/><w:t>Tel</w:t></w:r></w:p></w:hdr>"#;
        let endnotes = r#"<w:endnotes xmlns:w="w"><w:endnote><w:p><w:r><w:t>Nota de final</w:t></w:r></w:p></w:endnote></w:endnotes>"#;
        let bytes = make_zip(&[("word/document.xml", doc), ("word/header1.xml", header), ("word/endnotes.xml", endnotes)]);
        assert_eq!(extract("docx", &bytes, 1000).unwrap().text, "text\nNota de final\nFirma\nAdresa\tTel");
    }

    #[test]
    fn epub_spine_paths_are_resolved() {
        let opf = r#"<package><manifest>
            <item id="a" href="../Text/cap%C8%99or%201.xhtml#start" media-type="application/xhtml+xml"/>
            <item href='./Text/doi&amp;trei.xhtml' id='b'/>
        </manifest><spine><itemref idref="a"/><itemref idref="b"/></spine></package>"#;
        let book = make_zip(&[
            ("OEBPS/content/book.opf", opf),
            ("OEBPS/Text/capșor 1.xhtml", "<html><body><p>Primul</p></body></html>"),
            ("OEBPS/content/Text/doi&trei.xhtml", "<html><body><p>Al doilea</p></body></html>"),
        ]);
        assert_eq!(extract("epub", &book, 10_000).unwrap().text, "Primul\n\nAl doilea");
        // a spine that leads nowhere: every XHTML file instead
        let opf = r#"<package><manifest><item id="a" href="missing.xhtml"/></manifest><spine><itemref idref="a"/></spine></package>"#;
        let book = make_zip(&[("content.opf", opf), ("text/one.xhtml", "<p>Unu</p>")]);
        assert_eq!(extract("epub", &book, 10_000).unwrap().text, "Unu");
    }

    #[test]
    fn spreadsheet_dates_read_as_dates() {
        let types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
            <Default Extension="xml" ContentType="application/xml"/>
            <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
            <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
            <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
            </Types>"#;
        let rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let book = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
            <sheets><sheet name="Facturi" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let book_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
            <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
        let styles = r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
            <numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy-mm-dd hh:mm"/></numFmts>
            <cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/></cellXfs></styleSheet>"#;
        // a cell far away (XFD1048576) would make a dense sheet of 17 billion cells
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>Scadenta</t></is></c><c r="B1" s="1"><v>45366</v></c><c r="C1" s="2"><v>45366.5</v></c></row>
            <row r="1048576"><c r="XFD1048576"><v>7</v></c></row>
            </sheetData></worksheet>"#;
        let bytes = make_zip(&[
            ("[Content_Types].xml", types),
            ("_rels/.rels", rels),
            ("xl/workbook.xml", book),
            ("xl/_rels/workbook.xml.rels", book_rels),
            ("xl/styles.xml", styles),
            ("xl/worksheets/sheet1.xml", sheet),
        ]);
        assert_eq!(extract("xlsx", &bytes, 10_000).unwrap().text, "Facturi\nScadenta\t2024-03-15\t2024-03-15 12:00\n7");
    }

    #[test]
    fn every_listed_extension_has_a_kind() {
        for e in extensions() {
            assert!(kind_of(e).is_some(), "{e}");
        }
    }

    #[test]
    fn output_is_capped() {
        let e = extract("txt", "abcdefghij".as_bytes(), 4).unwrap();
        assert_eq!(e.text, "abcd");
        assert!(e.truncated);
    }

    #[test]
    fn damaged_and_unknown_inputs() {
        assert_eq!(extract("exe", b"MZ", 100), Err(ExtractError::Unsupported));
        assert!(matches!(extract("docx", b"not a zip", 100), Err(ExtractError::Damaged(_))));
        assert_eq!(extract("docx", &[0xD0, 0xCF, 0x11, 0xE0, 0, 0], 100), Err(ExtractError::Protected));
        assert!(matches!(extract("pdf", b"hello", 100), Err(ExtractError::Damaged(_))));
        // garbage must never panic
        let mut seed = 7u32;
        for _ in 0..300 {
            let junk: Vec<u8> = (0..200)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect();
            for ext in ["docx", "pptx", "xlsx", "odt", "rtf", "html", "txt", "pdf"] {
                let _ = extract(ext, &junk, 1000);
            }
        }
    }
}
