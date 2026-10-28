//! Snippets under content results: "…atașez factura pentru **martie**…".
//!
//! The content index stores no text (it stays small), so the snippet comes
//! from the file itself, through the same sandboxed extraction as the
//! preview. Only rows on screen are read, newest request first, on a
//! background thread; texts are cached, so scrolling back is instant.
//! The app runs as the user, so it can only read what the user can.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};

use whereti_ipc::Row;

use crate::app::match_mask;
use crate::preview::{self, Content};

/// Characters read to find a match (snippets look much deeper than the
/// preview: the word may be on page 40).
const SEARCH_CHARS: usize = 2_000_000;
/// Texts the worker keeps, to re-cut snippets when the query changes.
const TEXTS: usize = 24;
/// And at most about this much text in all.
const TEXT_BYTES: usize = 16 << 20;
/// Snippets kept per query.
const SNIPPETS: usize = 400;
/// Characters around the first match.
const BEFORE: usize = 48;
const LENGTH: usize = 200;

/// A short excerpt with the matched characters marked.
#[derive(Clone)]
pub struct Snippet {
    pub text: String,
    pub marks: Vec<bool>,
}

/// What a content row shows under its name.
#[derive(Clone)]
pub enum Line {
    Loading,
    Excerpt(Snippet),
    /// Read, but the words were not found as typed (e.g. split by a line
    /// break, or only in a part the reader skips).
    NoExcerpt,
}

/// A file as it is now: its path, size and date. A file changed since its
/// text was read has another key, so it is read again.
type Key = (String, u64, u32);

struct Shared {
    /// Files on screen without a snippet, in order; and the words.
    wanted: Vec<Key>,
    needles: Vec<String>,
}

type Wanted = Arc<(Mutex<Shared>, Condvar)>;

pub struct Snippets {
    wanted: Wanted,
    rx: Receiver<(Vec<String>, Key, Option<Snippet>)>,
    made: HashMap<Key, Option<Snippet>>,
    made_for: Vec<String>,
    asked: Vec<Key>,
    /// A snippet with Chinese, Japanese or Korean text came (the fonts for
    /// it are loaded then).
    pub saw_cjk: bool,
}

impl Snippets {
    pub fn start(wake: impl Fn() + Send + 'static) -> Snippets {
        let wanted: Wanted = Arc::new((Mutex::new(Shared { wanted: Vec::new(), needles: Vec::new() }), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let w = wanted.clone();
        std::thread::Builder::new()
            .name("snippets".into())
            .spawn(move || {
                let sandbox = whereti_extract::worker::Sandbox::beside_current_exe();
                let mut texts: VecDeque<(Key, Option<Arc<str>>)> = VecDeque::new();
                loop {
                    let (key, needles) = {
                        let (lock, cv) = &*w;
                        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while sh.wanted.is_empty() {
                            sh = cv.wait(sh).unwrap_or_else(|e| e.into_inner());
                        }
                        (sh.wanted.remove(0), sh.needles.clone())
                    };
                    let text = match texts.iter().find(|t| t.0 == key) {
                        Some(t) => t.1.clone(),
                        None => {
                            let t = match preview::load_with(&key.0, sandbox.as_ref(), SEARCH_CHARS) {
                                Content::Text { text, .. } => Some(Arc::<str>::from(text)),
                                _ => None,
                            };
                            // the same file as it was before goes
                            texts.retain(|x| x.0 .0 != key.0);
                            texts.push_back((key.clone(), t.clone()));
                            keep_within(&mut texts);
                            t
                        }
                    };
                    let snippet = text.and_then(|t| make(&t, &needles)).or_else(|| photo_line(&key.0, &needles));
                    if tx.send((needles, key, snippet)).is_err() {
                        return;
                    }
                    wake();
                }
            })
            .expect("snippets thread");
        Snippets { wanted, rx, made: HashMap::new(), made_for: Vec::new(), asked: Vec::new(), saw_cjk: false }
    }

    /// Call before painting the rows.
    pub fn begin_frame(&mut self, needles: &[String]) {
        if self.made_for != needles {
            self.made.clear();
            self.made_for = needles.to_vec();
        }
        for (for_needles, key, snippet) in self.rx.try_iter() {
            if for_needles == self.made_for {
                if self.made.len() >= SNIPPETS {
                    self.made.clear();
                }
                self.saw_cjk |= snippet.as_ref().is_some_and(|s| crate::app::has_cjk(&s.text));
                self.made.insert(key, snippet);
            }
        }
        self.asked.clear();
    }

    /// The line for a visible content row; asks for it the first time (and
    /// again once the file changed).
    pub fn get(&mut self, row: &Row) -> Line {
        let key = (row.path.clone(), row.size, row.modified);
        match self.made.get(&key) {
            Some(Some(s)) => Line::Excerpt(s.clone()),
            Some(None) => Line::NoExcerpt,
            None => {
                self.asked.push(key);
                Line::Loading
            }
        }
    }

    /// Call after painting: the worker reads what is on screen now, in order.
    pub fn end_frame(&mut self) {
        let (lock, cv) = &*self.wanted;
        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
        if sh.wanted != self.asked || sh.needles != self.made_for {
            sh.wanted = std::mem::take(&mut self.asked);
            sh.needles = self.made_for.clone();
            cv.notify_one();
        }
    }
}

/// The oldest texts go, beyond `TEXTS` of them or `TEXT_BYTES` in all
/// (the newest always stays).
fn keep_within<K>(texts: &mut VecDeque<(K, Option<Arc<str>>)>) {
    let size = |t: &(K, Option<Arc<str>>)| t.1.as_ref().map_or(0, |s| s.len());
    let mut bytes: usize = texts.iter().map(size).sum();
    while texts.len() > 1 && (texts.len() > TEXTS || bytes > TEXT_BYTES) {
        if let Some(old) = texts.pop_front() {
            bytes -= size(&old);
        }
    }
}

/// A photo found by where it was taken or by what it shows: "câine ·
/// Rome, Lazio, Italy". The tags live only in the service's index, so the
/// app names the label the typed word matched (its photo's own text did not
/// have it), next to the place read from the photo.
fn photo_line(path: &str, needles: &[String]) -> Option<Snippet> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let head = match ext.as_str() {
        "heic" | "heif" | "hif" | "avif" | "jxl" => 64 << 20,
        "jpg" | "jpeg" | "jpe" | "jfif" | "tif" | "tiff" | "png" | "webp" | "bmp" | "gif" => 512 << 10,
        _ => return None,
    };
    let tag = matched_label(needles);
    let place = {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path).ok().and_then(|f| f.take(head).read_to_end(&mut bytes).ok());
        whereti_places::gps(&bytes).and_then(|(lat, lon)| whereti_places::place_of(lat, lon))
    };
    let place_text = place.as_ref().map(|place| {
        let label = place.label();
        if make(&label, needles).is_some() {
            return label;
        }
        // the place matched by another of its names ("roma" for Rome)
        let names = place.city.iter().chain(place.region.iter()).chain(place.country.iter());
        let matched: Vec<&String> =
            names.filter(|n| needles.iter().any(|w| whereti_query::fold(n).contains(w.as_str()))).take(2).collect();
        if matched.is_empty() {
            label
        } else {
            format!("{label} · {}", matched.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        }
    });
    let text = match (tag, place_text) {
        (Some(t), Some(p)) => format!("{t} · {p}"),
        (Some(t), None) => t.to_string(),
        (None, Some(p)) => p,
        (None, None) => return None,
    };
    make(&text, needles).or(Some(Snippet { marks: vec![false; text.chars().count()], text }))
}

/// The label of the photo model that a typed word names, in the language
/// the word was typed in ("câine", "Hund", "dog").
fn matched_label(needles: &[String]) -> Option<&'static str> {
    for w in needles.iter().filter(|w| w.chars().count() >= 3) {
        for names in whereti_extract::tags::labels() {
            if let Some(name) = names.iter().find(|n| whereti_query::fold(n) == *w) {
                return Some(name);
            }
        }
    }
    None
}

/// The excerpt around the first match: the whole phrase if the text has it,
/// else the first (long enough) word. Whitespace is collapsed.
pub fn make(text: &str, needles: &[String]) -> Option<Snippet> {
    let long: Vec<String> = needles.iter().filter(|n| n.chars().count() >= 3).cloned().collect();
    let words: &[String] = if long.is_empty() { needles } else { &long };
    if words.is_empty() {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let marks = match_mask(text, words);
    let phrase = (needles.len() > 1).then(|| match_mask(text, &[needles.join(" ")]).iter().position(|&m| m)).flatten();
    let at = phrase.or_else(|| marks.iter().position(|&m| m))?;

    // start a little before the match, at a word boundary
    let mut start = at.saturating_sub(BEFORE);
    if start > 0 {
        let limit = start.saturating_sub(15);
        while start > limit && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
    }
    let mut end = (start + LENGTH).min(chars.len());
    if end < chars.len() {
        let limit = end.saturating_sub(20).max(at + 1);
        while end > limit && !chars[end].is_whitespace() {
            end -= 1;
        }
    }
    let mut out = String::new();
    let mut out_marks = Vec::new();
    if start > 0 {
        out.push('…');
        out_marks.push(false);
    }
    let mut last_space = true;
    for k in start..end {
        let c = chars[k];
        if c.is_whitespace() || c.is_control() {
            if !last_space {
                out.push(' ');
                out_marks.push(false);
                last_space = true;
            }
            continue;
        }
        out.push(c);
        out_marks.push(marks[k]);
        last_space = false;
    }
    if end < chars.len() {
        if out.ends_with(' ') {
            out.pop();
            out_marks.pop();
        }
        out.push('…');
        out_marks.push(false);
    }
    Some(Snippet { text: out, marks: out_marks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(s: &Snippet) -> String {
        s.text.chars().zip(&s.marks).filter(|(_, &m)| m).map(|(c, _)| c).collect()
    }

    #[test]
    fn excerpt_around_the_first_match() {
        let text = format!("{}Bună ziua,\n\natașez   factura pentru martie 2026. Total: 1.250 lei.{}", "x ".repeat(100), " y".repeat(200));
        let s = make(&text, &["factura".into()]).unwrap();
        assert!(s.text.starts_with('…') && s.text.ends_with('…'), "{}", s.text);
        assert!(s.text.contains("Bună ziua, atașez factura pentru martie 2026."), "whitespace collapsed: {}", s.text);
        assert_eq!(marked(&s), "factura");
        assert_eq!(s.text.chars().count(), s.marks.len());
    }

    #[test]
    fn phrase_first_and_short_words_not_marked() {
        let text = "de la început de tot. Termenul de livrare este de 15 zile.";
        let s = make(text, &["termen".into(), "de".into(), "livrare".into()]).unwrap();
        assert!(s.text.starts_with("de la început"), "short text: no ellipsis, {}", s.text);
        assert_eq!(marked(&s), "Termenlivrare", "'de' is not marked");
        // diacritics fold: "masina" marks "mașină"
        let s = make("O mașină nouă", &["masina".into()]).unwrap();
        assert_eq!(marked(&s), "mașină");
    }

    #[test]
    fn texts_kept_stay_small() {
        let text = |n: usize| Some(Arc::<str>::from("x".repeat(n)));
        let mut texts: VecDeque<(u32, Option<Arc<str>>)> = (0..TEXTS as u32 + 5).map(|k| (k, text(10))).collect();
        keep_within(&mut texts);
        assert_eq!(texts.len(), TEXTS);
        assert_eq!(texts.front().map(|t| t.0), Some(5), "the oldest went");
        // a few big texts: only as many as fit
        texts.push_back((100, text(TEXT_BYTES / 2)));
        texts.push_back((101, text(TEXT_BYTES / 2)));
        keep_within(&mut texts);
        assert_eq!(texts.back().map(|t| t.0), Some(101));
        assert!(texts.iter().map(|t| t.1.as_ref().map_or(0, |s| s.len())).sum::<usize>() <= TEXT_BYTES);
        // one larger than the whole budget is still kept, alone
        texts.push_back((102, text(TEXT_BYTES + 1)));
        keep_within(&mut texts);
        assert_eq!(texts.len(), 1);
    }

    #[test]
    fn no_match_no_snippet() {
        assert!(make("nimic aici", &["factura".into()]).is_none());
        assert!(make("ceva", &[]).is_none());
    }
}
