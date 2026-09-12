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
            photo_exif(&bytes, tz_offset, &mut p);
        }
    } else if AUDIO_EXTS.contains(&ext) || VIDEO_EXTS.contains(&ext) {
        media(path, &mut p);
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

fn photo_exif(bytes: &[u8], tz_offset: i32, p: &mut Props) {
    use exif::{In, Tag, Value};
    let Ok(exif) = exif::Reader::new().continue_on_error(true).read_from_container(&mut std::io::Cursor::new(bytes)).or_else(|e| {
        e.distill_partial_result(|_| {})
    }) else {
        return;
    };
    let text = |tag: Tag| -> Option<String> {
        let f = exif.get_field(tag, In::PRIMARY)?;
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
        let num = |tag: Tag| exif.get_field(tag, In::PRIMARY).and_then(|f| f.value.get_uint(0));
        p.width = num(Tag::PixelXDimension).unwrap_or(0);
        p.height = num(Tag::PixelYDimension).unwrap_or(0);
    }
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
}
