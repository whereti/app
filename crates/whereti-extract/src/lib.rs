//! Text extraction for content search and preview (PLAN.md §6.3).
//!
//! `extract` turns the bytes of a document into plain text. It never touches
//! the disk: the caller (a broker) reads the file and passes the bytes, so the
//! extractor can run in an isolated process with no file access (see `worker`).
//!
//! Every format is bounded: output is capped at `max_chars`, and no archive
//! entry is inflated beyond `MAX_ENTRY` (zip bombs).

mod email;
mod ole;
mod rtf;
#[cfg(test)]
mod tests_mail_ole;
pub mod worker;

pub use rtf::rtf_to_text;

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
}

pub fn kind_of(ext: &str) -> Option<Kind> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => Kind::Docx,
        "pptx" | "pptm" | "ppsx" | "potx" => Kind::Pptx,
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => Kind::Spreadsheet,
        "odt" | "odp" => Kind::OpenDocument,
        "pdf" => Kind::Pdf,
        "rtf" => Kind::Rtf,
        "html" | "htm" | "xhtml" => Kind::Html,
        "eml" => Kind::Email,
        "msg" => Kind::Outlook,
        "doc" | "dot" => Kind::WordBinary,
        "ppt" | "pps" | "pot" => Kind::PowerPointBinary,
        e if TEXT_EXTS.contains(&e) => Kind::Text,
        _ => return None,
    })
}

/// Extensions with a converter, besides plain text.
const DOC_EXTS: &[&str] = &[
    "docx", "docm", "dotx", "dotm", "pptx", "pptm", "ppsx", "potx", "xlsx", "xlsm", "xlsb", "xls", "ods", "odt", "odp",
    "pdf", "rtf", "html", "htm", "xhtml", "eml", "msg", "doc", "dot", "ppt", "pps", "pot",
];

/// Every extension `extract` reads, lower case, without the dot.
pub fn extensions() -> Vec<&'static str> {
    DOC_EXTS.iter().chain(TEXT_EXTS).copied().collect()
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
    let kind = kind_of(ext).ok_or(ExtractError::Unsupported)?;
    let mut out = Out::new(max_chars);
    match kind {
        Kind::Text => out.push(&decode_text(bytes)?),
        Kind::Docx => docx(bytes, &mut out)?,
        Kind::Pptx => pptx(bytes, &mut out)?,
        Kind::Spreadsheet => spreadsheet(ext, bytes, &mut out)?,
        Kind::OpenDocument => odf(bytes, &mut out)?,
        Kind::Pdf => pdf(bytes, &mut out)?,
        Kind::Rtf => out.push(&rtf_to_text(&String::from_utf8_lossy(bytes))),
        Kind::Html => out.push(&html_to_text(&decode_text(bytes)?)),
        Kind::Email => email::eml(bytes, &mut out)?,
        Kind::Outlook => ole::msg(bytes, &mut out)?,
        Kind::WordBinary => ole::doc(bytes, &mut out)?,
        Kind::PowerPointBinary => ole::ppt(bytes, &mut out)?,
    }
    Ok(out.finish())
}

/// Collects text up to a character budget, tidying whitespace.
struct Out {
    text: String,
    budget: usize,
    truncated: bool,
}

impl Out {
    fn new(max_chars: usize) -> Out {
        Out { text: String::new(), budget: max_chars, truncated: false }
    }

    fn full(&self) -> bool {
        self.budget == 0
    }

    fn push(&mut self, s: &str) {
        for c in s.chars() {
            if self.budget == 0 {
                self.truncated = true;
                return;
            }
            let c = if c == '\r' || (c.is_control() && c != '\n' && c != '\t') { continue } else { c };
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
        Extracted { text: self.text, truncated: self.truncated }
    }
}

/// UTF-8 / UTF-16 (BOM) or a legacy code page guessed from the bytes.
/// Refuses binary data.
pub fn decode_text(bytes: &[u8]) -> Result<String, ExtractError> {
    if let Some((enc, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return Ok(enc.decode_without_bom_handling(&bytes[bom..]).0.into_owned());
    }
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return Err(ExtractError::Unsupported); // binary
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(bytes, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    Ok(enc.decode_without_bom_handling(bytes).0.into_owned())
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
            Ok(Event::GeneralRef(r)) if inside > 0 => out.push(&resolve_entity(&r.into_inner())),
            Ok(Event::Eof) | Err(_) => return,
            _ => {}
        }
        buf.clear();
    }
}

fn resolve_entity(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        "nbsp" => " ".into(),
        n if n.starts_with("#x") || n.starts_with("#X") => {
            u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32).map(String::from).unwrap_or_default()
        }
        n if n.starts_with('#') => n[1..].parse::<u32>().ok().and_then(char::from_u32).map(String::from).unwrap_or_default(),
        _ => String::new(),
    }
}

fn docx(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let doc = zip_entry(&mut zip, "word/document.xml").ok_or_else(|| ExtractError::Damaged("no word/document.xml".into()))?;
    xml_text(&doc, &["t"], &["p"], &["tab", "br"], out);
    // headers, footers, footnotes, comments
    let mut extra: Vec<String> = zip_names(&zip)
        .into_iter()
        .filter(|n| {
            n.starts_with("word/header") || n.starts_with("word/footer") || n == "word/footnotes.xml" || n == "word/comments.xml"
        })
        .collect();
    extra.sort();
    for name in extra {
        if let Some(xml) = zip_entry(&mut zip, &name) {
            out.newline();
            xml_text(&xml, &["t"], &["p"], &["tab"], out);
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
            xml_text(&xml, &["t"], &["p"], &[], out);
            out.newline();
        }
    }
    let mut notes: Vec<String> = zip_names(&zip).into_iter().filter(|n| n.starts_with("ppt/notesSlides/")).collect();
    notes.sort();
    for name in notes {
        if let Some(xml) = zip_entry(&mut zip, &name) {
            xml_text(&xml, &["t"], &["p"], &[], out);
        }
    }
    Ok(())
}

fn odf(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
    let mut zip = zip_archive(bytes)?;
    let xml = zip_entry(&mut zip, "content.xml").ok_or_else(|| ExtractError::Damaged("no content.xml".into()))?;
    xml_text(&xml, &["p", "h", "span", "a"], &["p", "h"], &["tab", "s"], out);
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
        if let Ok(range) = book.worksheet_range(&name) {
            for row in range.rows() {
                if out.full() {
                    break;
                }
                let cells: Vec<String> =
                    row.iter().map(cell_text).filter(|s| !s.is_empty()).collect();
                if !cells.is_empty() {
                    out.push(&cells.join("\t"));
                    out.newline();
                }
            }
        }
        out.newline();
    }
    Ok(())
}

/// Cell text as a person reads it: 3591.5525890622844 → 3591.55.
fn cell_text(c: &calamine::Data) -> String {
    match c {
        calamine::Data::Float(f) if f.fract() != 0.0 => {
            let s = format!("{f:.2}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        other => other.to_string(),
    }
}

fn pdf(bytes: &[u8], out: &mut Out) -> Result<(), ExtractError> {
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
    Ok(())
}

/// Text of an HTML page: no tags, scripts or styles; entities resolved.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let b = html.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            for skip in ["script", "style", "noscript"] {
                if lower[i + 1..].starts_with(skip) {
                    let close = format!("</{skip}");
                    i = lower[i..].find(&close).map_or(b.len(), |p| i + p);
                    break;
                }
            }
            let end = html[i..].find('>').map_or(b.len(), |p| i + p + 1);
            let tag = &lower[i..end];
            if ["<p", "<br", "<div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "</p", "</div", "</li"].iter().any(|t| tag.starts_with(t)) && !out.ends_with('\n') {
                out.push('\n');
            }
            i = end;
        } else if b[i] == b'&' {
            let end = html[i..].find(';').filter(|p| *p < 10).map(|p| i + p);
            match end {
                Some(e) => {
                    out.push_str(&resolve_entity(&html[i + 1..e]));
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
