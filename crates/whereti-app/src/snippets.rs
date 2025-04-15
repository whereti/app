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

use crate::app::match_mask;
use crate::preview::{self, Content};

/// Characters read to find a match (snippets look much deeper than the
/// preview: the word may be on page 40).
const SEARCH_CHARS: usize = 2_000_000;
/// Texts the worker keeps, to re-cut snippets when the query changes.
const TEXTS: usize = 24;
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

struct Shared {
    /// Paths on screen without a snippet, in order; and the words.
    wanted: Vec<String>,
    needles: Vec<String>,
}

type Wanted = Arc<(Mutex<Shared>, Condvar)>;

pub struct Snippets {
    wanted: Wanted,
    rx: Receiver<(Vec<String>, String, Option<Snippet>)>,
    made: HashMap<String, Option<Snippet>>,
    made_for: Vec<String>,
    asked: Vec<String>,
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
                let mut texts: VecDeque<(String, Option<Arc<str>>)> = VecDeque::new();
                loop {
                    let (path, needles) = {
                        let (lock, cv) = &*w;
                        let mut sh = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while sh.wanted.is_empty() {
                            sh = cv.wait(sh).unwrap_or_else(|e| e.into_inner());
                        }
                        (sh.wanted.remove(0), sh.needles.clone())
                    };
                    let text = match texts.iter().find(|t| t.0 == path) {
                        Some(t) => t.1.clone(),
                        None => {
                            let t = match preview::load_with(&path, sandbox.as_ref(), SEARCH_CHARS) {
                                Content::Text { text, .. } => Some(Arc::<str>::from(text)),
                                _ => None,
                            };
                            if texts.len() >= TEXTS {
                                texts.pop_front();
                            }
                            texts.push_back((path.clone(), t.clone()));
                            t
                        }
                    };
                    let snippet = text.and_then(|t| make(&t, &needles));
                    if tx.send((needles, path, snippet)).is_err() {
                        return;
                    }
                    wake();
                }
            })
            .expect("snippets thread");
        Snippets { wanted, rx, made: HashMap::new(), made_for: Vec::new(), asked: Vec::new() }
    }

    /// Call before painting the rows.
    pub fn begin_frame(&mut self, needles: &[String]) {
        if self.made_for != needles {
            self.made.clear();
            self.made_for = needles.to_vec();
        }
        for (for_needles, path, snippet) in self.rx.try_iter() {
            if for_needles == self.made_for {
                if self.made.len() >= SNIPPETS {
                    self.made.clear();
                }
                self.made.insert(path, snippet);
            }
        }
        self.asked.clear();
    }

    /// The line for a visible content row; asks for it the first time.
    pub fn get(&mut self, path: &str) -> Line {
        match self.made.get(path) {
            Some(Some(s)) => Line::Excerpt(s.clone()),
            Some(None) => Line::NoExcerpt,
            None => {
                self.asked.push(path.to_string());
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
    fn no_match_no_snippet() {
        assert!(make("nimic aici", &["factura".into()]).is_none());
        assert!(make("ceva", &[]).is_none());
    }
}
