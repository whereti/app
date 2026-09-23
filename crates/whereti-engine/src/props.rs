//! File properties for `taken:`, `camera:`, `width:`, `height:`,
//! `artist:`, `album:`, `duration:` and `author:` (PLAN.md §62).
//!
//! Read while the content index goes through the files (photos, music and
//! videos in the user's folders, Office documents), with memory-safe Rust
//! readers only: EXIF (kamadak-exif), picture headers (imagesize), music and
//! video tags (lofty), and Office's own `docProps/core.xml`. Kept in
//! `props.txt` beside the content index, so no document has to be read again
//! to add them.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use whereti_query::{fold, Clause, Prop, Query, Term};

/// What a file says about itself. Zero / empty: not known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Props {
    /// When the photo was taken, minutes since 1601 UTC.
    pub taken: u32,
    pub width: u32,
    pub height: u32,
    /// Seconds.
    pub duration: u32,
    pub camera: String,
    pub artist: String,
    pub album: String,
    pub author: String,
    /// What the photo model saw: label numbers (whereti_extract::tags::labels).
    pub tags: Vec<u16>,
    /// The photo model looked at it (with or without finding anything).
    pub tagged: bool,
    /// A picture whose text was read (OCR) for the index.
    pub ocr: bool,
}

use whereti_query::kinds::{AUDIO_TAGGED as AUDIO_EXTS, RAW, VIDEO_TAGGED as VIDEO_EXTS};
const PHOTO_EXTS: &[&str] = whereti_query::kinds::PHOTOS;
const OFFICE_EXTS: &[&str] =
    &["docx", "docm", "dotx", "dotm", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "pptx", "pptm", "ppsx", "ppsm", "potx", "potm"];
const ODF_EXTS: &[&str] = &["odt", "ott", "ods", "ots", "odp", "otp"];

/// The file has properties worth reading.
pub fn has_props(ext: &str) -> bool {
    AUDIO_EXTS.contains(&ext)
        || VIDEO_EXTS.contains(&ext)
        || OTHER_VIDEO_EXTS.contains(&ext)
        || ext == "pdf"
        || PHOTO_EXTS.contains(&ext)
        || RAW.contains(&ext)
        || OFFICE_EXTS.contains(&ext)
        || ODF_EXTS.contains(&ext)
}

/// Reads the properties of `path` (`ext` lower case). Never fails: what
/// cannot be read stays empty. `tz_offset`: minutes east of UTC, for the
/// camera's local time.
pub fn read(path: &Path, ext: &str, tz_offset: i32) -> Props {
    // the readers return errors for bad files; this is for their bugs
    std::panic::catch_unwind(|| read_inner(path, ext, tz_offset)).unwrap_or_default()
}

fn read_inner(path: &Path, ext: &str, tz_offset: i32) -> Props {
    let mut p = Props::default();
    if PHOTO_EXTS.contains(&ext) || RAW.contains(&ext) {
        // the same head as the place reader (content.rs): EXIF is at the
        // start of a JPEG; HEIC/AVIF keep it anywhere, RAW a little further in
        let raw = RAW.contains(&ext);
        let head = if matches!(ext, "heic" | "heif" | "hif" | "avif" | "jxl") {
            64 << 20
        } else if raw {
            16 << 20
        } else {
            512 << 10
        };
        if let Ok(bytes) = read_head(path, head) {
            // a RAW file's first image is its small preview: its size is not the photo's
            if !raw {
                if let Ok(size) = imagesize::blob_size(&bytes) {
                    (p.width, p.height) = (size.width as u32, size.height as u32);
                }
            }
            photo_exif(&bytes, ext, tz_offset, &mut p);
        }
    } else if OTHER_VIDEO_EXTS.contains(&ext) {
        if let Ok(head) = read_head(path, 2 << 20) {
            p.duration = video_length(&head, ext).unwrap_or(0);
        }
    } else if AUDIO_EXTS.contains(&ext) || VIDEO_EXTS.contains(&ext) {
        media(path, &mut p);
    } else if ext == "pdf" {
        p.author = pdf_author(path).unwrap_or_default();
    } else if OFFICE_EXTS.contains(&ext) {
        p.author = zip_xml_tag(path, "docProps/core.xml", "dc:creator").unwrap_or_default();
    } else if ODF_EXTS.contains(&ext) {
        p.author = zip_xml_tag(path, "meta.xml", "meta:initial-creator")
            .or_else(|| zip_xml_tag(path, "meta.xml", "dc:creator"))
            .unwrap_or_default();
    }
    p
}

fn read_head(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

/// The EXIF blocks of a photo. The usual containers (JPEG, TIFF, HEIF,
/// PNG, WebP) are read whole; camera files that bend TIFF get help:
/// Olympus and Panasonic RAW (another magic number), Fujifilm RAF (EXIF in
/// its embedded JPEG), Canon CR3 (TIFF blocks in its CMT1/CMT2 boxes).
fn exif_blocks(bytes: &[u8], ext: &str) -> Vec<exif::Exif> {
    let reader = || {
        let mut r = exif::Reader::new();
        r.continue_on_error(true);
        r
    };
    let tiff = |data: Vec<u8>| reader().read_raw(data).or_else(|e| e.distill_partial_result(|_| {})).ok();
    if let Ok(e) = reader().read_from_container(&mut std::io::Cursor::new(bytes)).or_else(|e| e.distill_partial_result(|_| {})) {
        return vec![e];
    }
    match ext {
        // "IIRO"/"IIRS" (Olympus ORF), "IIU\0" (Panasonic RW2): TIFF with another magic
        "orf" | "rw2" if bytes.len() > 8 && bytes.starts_with(b"II") => {
            let mut data = bytes.to_vec();
            data[2] = 0x2A;
            data[3] = 0;
            tiff(data).into_iter().collect()
        }
        // RAF: a big-endian offset and length of the embedded JPEG at 84
        "raf" if bytes.starts_with(b"FUJIFILMCCD-RAW") && bytes.len() > 92 => {
            let at = u32::from_be_bytes(bytes[84..88].try_into().expect("4 bytes")) as usize;
            let len = u32::from_be_bytes(bytes[88..92].try_into().expect("4 bytes")) as usize;
            bytes
                .get(at..at.saturating_add(len).min(bytes.len()))
                .and_then(|jpeg| reader().read_from_container(&mut std::io::Cursor::new(jpeg)).ok())
                .into_iter()
                .collect()
        }
        // CR3: ISO boxes; CMT1 holds IFD0 (camera), CMT2 the EXIF IFD (date)
        "cr3" => [b"CMT1", b"CMT2"]
            .iter()
            .filter_map(|name| {
                let at = bytes.windows(4).position(|w| w == *name)?;
                let box_start = at.checked_sub(4)?;
                let size = u32::from_be_bytes(bytes.get(box_start..at)?.try_into().ok()?) as usize;
                tiff(bytes.get(at + 4..box_start.saturating_add(size))?.to_vec())
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn photo_exif(bytes: &[u8], ext: &str, tz_offset: i32, p: &mut Props) {
    use exif::{In, Tag, Value};
    let blocks = exif_blocks(bytes, ext);
    if blocks.is_empty() {
        return;
    }
    let field = |tag: Tag| blocks.iter().find_map(|e| e.get_field(tag, In::PRIMARY));
    let text = |tag: Tag| -> Option<String> {
        let f = field(tag)?;
        match &f.value {
            Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).trim().trim_end_matches('\0').to_string()),
            _ => None,
        }
    };
    let when = text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTimeDigitized)).or_else(|| text(Tag::DateTime));
    if let Some(t) = when.and_then(|s| exif_minutes(&s)) {
        p.taken = (t as i64 - tz_offset as i64).clamp(0, u32::MAX as i64) as u32;
    }
    let make = text(Tag::Make).unwrap_or_default();
    let model = text(Tag::Model).unwrap_or_default();
    // "Canon" + "Canon EOS 80D": the model alone says it
    p.camera = if model.to_lowercase().starts_with(&make.to_lowercase()) || make.is_empty() {
        model
    } else if model.is_empty() {
        make
    } else {
        format!("{make} {model}")
    };
    if p.width == 0 {
        let num = |tag: Tag| field(tag).and_then(|f| f.value.get_uint(0));
        p.width = num(Tag::PixelXDimension).unwrap_or(0);
        p.height = num(Tag::PixelYDimension).unwrap_or(0);
    }
}

/// Videos outside the MP4 family whose length whereti reads itself.
const OTHER_VIDEO_EXTS: &[&str] = &["mkv", "webm", "avi", "wmv", "asf"];

/// A video's length in seconds from its header (first MBs): Matroska /
/// WebM (Segment Info: Duration x TimecodeScale), AVI (avih: frames x
/// microseconds per frame), ASF / WMV (File Properties: play duration).
fn video_length(b: &[u8], ext: &str) -> Option<u32> {
    let secs = match ext {
        "mkv" | "webm" => {
            // TimecodeScale (2A D7 B1), default 1 ms; Duration (44 89) a float
            let scale = find(b, &[0x2A, 0xD7, 0xB1])
                .and_then(|at| {
                    let len = (b.get(at + 3)? & 0x7F) as usize;
                    let v = b.get(at + 4..at + 4 + len)?;
                    Some(v.iter().fold(0u64, |acc, &x| acc << 8 | x as u64))
                })
                .unwrap_or(1_000_000) as f64;
            let at = find(b, &[0x44, 0x89])?;
            let duration = match b.get(at + 2)? {
                0x84 => f32::from_be_bytes(b.get(at + 3..at + 7)?.try_into().ok()?) as f64,
                0x88 => f64::from_be_bytes(b.get(at + 3..at + 11)?.try_into().ok()?),
                _ => return None,
            };
            duration * scale / 1e9
        }
        "avi" => {
            let at = find(b, b"avih")?;
            let us_per_frame = u32::from_le_bytes(b.get(at + 8..at + 12)?.try_into().ok()?) as f64;
            let frames = u32::from_le_bytes(b.get(at + 24..at + 28)?.try_into().ok()?) as f64;
            frames * us_per_frame / 1e6
        }
        "wmv" | "asf" => {
            // File Properties Object: GUID 8CABDCA1-A947-11CF-8EE4-00C00C205365
            const GUID: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
            let at = find(b, &GUID)?;
            let play = u64::from_le_bytes(b.get(at + 64..at + 72)?.try_into().ok()?) as f64; // 100 ns
            let preroll = u64::from_le_bytes(b.get(at + 80..at + 88)?.try_into().ok()?) as f64; // ms
            play / 1e7 - preroll / 1e3
        }
        _ => return None,
    };
    (secs.is_finite() && (1.0..1e7).contains(&secs)).then(|| secs.round() as u32)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A PDF's author from its Info dictionary (`/Author (…)` or `<hex>`) or,
/// failing that, its XMP (`<dc:creator>…<rdf:li>…`). A byte search in the
/// first and last MB, no PDF parser (this runs in the service). Encrypted
/// PDFs keep their strings encrypted: none.
fn pdf_author(path: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut head = Vec::new();
    (&mut f).take(1 << 20).read_to_end(&mut head).ok()?;
    let mut tail = Vec::new();
    if len > 1 << 20 {
        f.seek(SeekFrom::Start(len.saturating_sub(1 << 20).max(1 << 20))).ok()?;
        f.take(1 << 20).read_to_end(&mut tail).ok()?;
    }
    if find(&head, b"/Encrypt").is_some() || find(&tail, b"/Encrypt").is_some() {
        return None;
    }
    for b in [&tail, &head] {
        if let Some(a) = info_author(b) {
            return Some(a);
        }
    }
    for b in [&head, &tail] {
        let at = find(b, b"<dc:creator")?;
        let li = find(&b[at..], b"<rdf:li")? + at;
        let start = li + b[li..].iter().position(|&c| c == b'>')? + 1;
        let end = start + find(&b[start..], b"</rdf:li>")?;
        let s = String::from_utf8_lossy(&b[start..end]).trim().to_string();
        if !s.is_empty() && s.len() < 200 {
            return Some(s);
        }
    }
    None
}

fn info_author(b: &[u8]) -> Option<String> {
    let mut at = find(b, b"/Author")? + 7;
    while b.get(at).is_some_and(|c| c.is_ascii_whitespace()) {
        at += 1;
    }
    let raw: Vec<u8> = match b.get(at)? {
        b'(' => {
            // a literal string, with \ escapes and nested parentheses
            let (mut out, mut depth, mut i) = (Vec::new(), 0, at + 1);
            while let Some(&c) = b.get(i) {
                match c {
                    b'\\' => {
                        let n = *b.get(i + 1)?;
                        match n {
                            b'n' => out.push(b'\n'),
                            b'r' | b't' | b'b' | b'f' => out.push(b' '),
                            b'0'..=b'7' => {
                                let digits: Vec<u8> = b[i + 1..].iter().take(3).take_while(|d| (b'0'..=b'7').contains(d)).copied().collect();
                                out.push(digits.iter().fold(0u8, |acc, d| acc.wrapping_mul(8).wrapping_add(d - b'0')));
                                i += digits.len() - 1;
                            }
                            other => out.push(other),
                        }
                        i += 2;
                        continue;
                    }
                    b'(' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' => depth -= 1,
                    _ => {}
                }
                out.push(c);
                i += 1;
                if out.len() > 1000 {
                    return None;
                }
            }
            out
        }
        b'<' => {
            let end = at + find(&b[at..], b">")?;
            let hex: Vec<u8> = b[at + 1..end].iter().copied().filter(|c| c.is_ascii_hexdigit()).collect();
            hex.chunks(2).filter_map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()
        }
        _ => return None,
    };
    // UTF-16BE with a byte-order mark, else PDFDocEncoding (about Latin-1)
    let s = if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&c| c as char).collect()
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// "2025:08:14 18:30:05" (local time) -> minutes since 1601, local.
fn exif_minutes(s: &str) -> Option<u32> {
    let s = s.trim();
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13).and_then(|v| v.parse().ok()).unwrap_or(0);
    let mi: i64 = s.get(14..16).and_then(|v| v.parse().ok()).unwrap_or(0);
    if !(1900..=2200).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = whereti_query::day_from_civil(y, mo, d);
    u32::try_from(days * 1440 + h * 60 + mi).ok()
}

fn media(path: &Path, p: &mut Props) {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;
    let Ok(probe) = lofty::probe::Probe::open(path) else { return };
    let probe = probe.options(lofty::config::ParseOptions::new().read_cover_art(false));
    let Ok(probe) = probe.guess_file_type() else { return };
    let Ok(file) = probe.read() else { return };
    p.duration = file.properties().duration().as_secs().min(u32::MAX as u64) as u32;
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        p.artist = tag.artist().map(|s| s.to_string()).unwrap_or_default();
        p.album = tag.album().map(|s| s.to_string()).unwrap_or_default();
    }
}

/// The text of the first `<tag>` in one XML part of a ZIP-based document.
fn zip_xml_tag(path: &Path, part: &str, tag: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name(part).ok()?;
    if entry.size() > 1 << 20 {
        return None;
    }
    let mut xml = String::new();
    (&mut entry).take(1 << 20).read_to_string(&mut xml).ok()?;
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let body = &xml[start..];
    let text_at = body.find('>')? + 1;
    let end = body.find(&format!("</{tag}>"))?;
    let text = body.get(text_at..end)?.trim();
    let text = text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'");
    (!text.is_empty()).then_some(text)
}

impl Props {
    fn matches(&self, p: &Prop) -> bool {
        let has = |field: &str, v: &str| !field.is_empty() && fold(field).contains(v);
        match p {
            Prop::Taken { min, max } => self.taken != 0 && (*min..=*max).contains(&self.taken),
            Prop::Width { min, max } => self.width != 0 && (*min..=*max).contains(&self.width),
            Prop::Height { min, max } => self.height != 0 && (*min..=*max).contains(&self.height),
            Prop::Duration { min, max } => self.duration != 0 && (*min..=*max).contains(&self.duration),
            Prop::Camera(v) => has(&self.camera, v),
            Prop::Artist(v) => has(&self.artist, v),
            Prop::Album(v) => has(&self.album, v),
            Prop::Author(v) => has(&self.author, v),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Props::default()
    }
}

/// The property conditions of a query, taken out of it: `(conditions,
/// the rest)`. A clause is a property condition when all its terms are.
pub fn split(query: &Query) -> (Vec<Clause>, Query) {
    let mut props = Vec::new();
    let mut rest = Query::default();
    for c in &query.clauses {
        if !c.any_of.is_empty() && c.any_of.iter().all(|t| matches!(t, Term::Prop(_))) {
            props.push(c.clone());
        } else {
            rest.clauses.push(c.clone());
        }
    }
    (props, rest)
}

fn satisfies(p: &Props, clauses: &[Clause]) -> bool {
    clauses.iter().all(|c| {
        let any = c.any_of.iter().any(|t| matches!(t, Term::Prop(prop) if p.matches(prop)));
        any != c.negated
    })
}

/// Every file's properties, by content-index key. Files read and found
/// without any are kept too (as empty), so they are not read again.
pub struct PropStore {
    path: PathBuf,
    map: HashMap<u64, Props>,
    dirty: bool,
}

const HEADER: &str = "whereti-props 1";

impl PropStore {
    pub fn load(dir: &Path) -> PropStore {
        let path = dir.join("props.txt");
        let mut map = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut lines = text.lines();
            if lines.next() == Some(HEADER) {
                for line in lines {
                    let f: Vec<&str> = line.split('\t').collect();
                    // 10 fields since 0.0.25 (the tags: "" not looked at, "x" looked
                    // at and nothing found, else label numbers); 9 before
                    // 11 fields since 0.0.25 (OCR done: "1")
                    let (fields, tags, ocr) = match f.as_slice() {
                        [rest @ .., tags, ocr] if rest.len() == 9 => (rest, *tags, *ocr == "1"),
                        [rest @ .., tags] if rest.len() == 9 => (rest, *tags, false),
                        rest => (rest, "", false),
                    };
                    let [key, taken, width, height, duration, camera, artist, album, author] = fields else { continue };
                    let Ok(key) = key.parse() else { continue };
                    let n = |s: &str| s.parse().unwrap_or(0);
                    map.insert(
                        key,
                        Props {
                            taken: n(taken),
                            width: n(width),
                            height: n(height),
                            duration: n(duration),
                            camera: camera.to_string(),
                            artist: artist.to_string(),
                            album: album.to_string(),
                            author: author.to_string(),
                            tags: tags.split(',').filter_map(|t| t.parse().ok()).collect(),
                            tagged: !tags.is_empty(),
                            ocr,
                        },
                    );
                }
            }
        }
        PropStore { path, map, dirty: false }
    }

    pub fn contains(&self, key: u64) -> bool {
        self.map.contains_key(&key)
    }

    pub fn set(&mut self, key: u64, p: Props) {
        if self.map.get(&key) != Some(&p) {
            self.map.insert(key, p);
            self.dirty = true;
        }
    }

    pub fn remove(&mut self, key: u64) {
        if self.map.remove(&key).is_some() {
            self.dirty = true;
        }
    }

    /// Writes `props.txt` if anything changed.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let mut text = String::with_capacity(self.map.len() * 48);
        text.push_str(HEADER);
        text.push('\n');
        for (k, p) in &self.map {
            if p.is_empty() {
                text.push_str(&format!("{k}\t0\t0\t0\t0\t\t\t\t\t\t\n"));
                continue;
            }
            text.push_str(&format!(
                "{k}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                p.taken,
                p.width,
                p.height,
                p.duration,
                clean(&p.camera),
                clean(&p.artist),
                clean(&p.album),
                clean(&p.author),
                if p.tagged && p.tags.is_empty() { "x".to_string() } else { p.tags.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",") },
                if p.ocr { "1" } else { "" }
            ));
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
            self.dirty = false;
        }
    }

    pub fn get(&self, key: u64) -> Option<&Props> {
        self.map.get(&key)
    }

    /// Keys of the files whose properties satisfy every clause.
    pub fn keys_matching(&self, clauses: &[Clause]) -> Vec<u64> {
        self.map.iter().filter(|(_, p)| satisfies(p, clauses)).map(|(k, _)| *k).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whereti_query::{parse, DateCtx};

    #[test]
    fn conditions_select_by_property_and_survive_a_save() {
        let dir = std::env::temp_dir().join(format!("whereti-props-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = PropStore::load(&dir);
        let canon = Props { taken: exif_minutes("2025:08:14 18:30:05").unwrap(), width: 6000, height: 4000, camera: "Canon EOS 80D".into(), ..Default::default() };
        let song = Props { duration: 330, artist: "Ștefan Bănică".into(), album: "Best of".into(), tags: vec![3, 17], tagged: true, ..Default::default() };
        s.set(1, canon);
        s.set(2, song);
        s.set(3, Props::default());
        s.set(4, Props { tagged: true, ocr: true, ..Default::default() });
        s.save();
        let s = PropStore::load(&dir);
        let ctx = DateCtx { now: exif_minutes("2026:10:24 12:00:00").unwrap(), tz_offset: 0 };
        let keys = |q: &str| {
            let (props, _) = split(&parse(q, &ctx));
            let mut k = s.keys_matching(&props);
            k.sort();
            k
        };
        assert_eq!(keys("taken:2025-08"), vec![1]);
        assert_eq!(keys("camera:canon width:>4000"), vec![1]);
        assert_eq!(keys("artist:banica duration:>=5m"), vec![2]);
        assert!(keys("artist:banica duration:>5m").is_empty(), "5:30 is not over 5 minutes: 5m covers 5:00-5:59");
        assert_eq!(keys("!camera:canon"), vec![2, 3, 4]);
        assert_eq!(keys("taken:2024"), Vec::<u64>::new());
        assert_eq!(s.get(2).unwrap().tags, vec![3, 17]);
        assert!(s.get(4).unwrap().tagged && !s.get(3).unwrap().tagged);
        assert!(s.get(4).unwrap().ocr && !s.get(2).unwrap().ocr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exif_dates() {
        assert_eq!(exif_minutes("0000:00:00 00:00:00"), None);
        assert!(exif_minutes("2025:08:14 18:30:05").is_some());
    }

    #[test]
    fn video_lengths_and_pdf_authors() {
        // Matroska: TimecodeScale 1 ms, Duration 90 500.0 (8-byte float)
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40, 0x44, 0x89, 0x88];
        mkv.extend(90_500.0f64.to_be_bytes());
        assert_eq!(video_length(&mkv, "mkv"), Some(91));
        // AVI: 40 000 us per frame, 750 frames = 30 s
        let mut avi = b"RIFF\0\0\0\0AVI LIST\0\0\0\0hdrlavih\x38\0\0\0".to_vec();
        avi.extend(40_000u32.to_le_bytes());
        avi.extend([0u8; 12]);
        avi.extend(750u32.to_le_bytes());
        avi.extend([0u8; 40]);
        assert_eq!(video_length(&avi, "avi"), Some(30));
        assert_eq!(video_length(b"nothing here", "mkv"), None);

        assert_eq!(info_author(br"<< /Author (Ion Popescu) /Title (x) >>").as_deref(), Some("Ion Popescu"));
        assert_eq!(info_author(br"/Author (A \(B\) C)").as_deref(), Some("A (B) C"));
        assert_eq!(info_author(b"/Author <FEFF0218007400650066>").as_deref(), Some("\u{0218}tef"));
        assert_eq!(info_author(b"/Title (x)"), None);
    }
}
