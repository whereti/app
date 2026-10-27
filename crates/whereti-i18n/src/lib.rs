//! whereti's languages (PLAN.md §37), shared by the app and the installer.
//!
//! The English text is the key: `tr("Search…")` gives the text in the
//! current language. Every text lives in one catalog per screen
//! (`screens/*.rs`), with a note for translators and all the languages side
//! by side. Texts with values use `trf!("{count} on {drives}", count = …,
//! drives)`, texts with a number that changes the words use
//! `trn!("{n} file|{n} files", n)`: forms split by `|`, as many as the
//! language has (see `plural`). A text missing from the catalogs shows in
//! English; the tests catch that before a release.

mod locales;
mod plural;
pub mod screens;

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

pub use locales::{Lang, Locale, LOCALES};
pub use plural::form_index;

/// One text: the English (the key), a note for translators, every language.
#[derive(Debug)]
pub struct Entry {
    pub en: &'static str,
    pub note: &'static str,
    pub de: &'static str,
    pub es: &'static str,
    pub fr: &'static str,
    pub it: &'static str,
    pub pl: &'static str,
    pub pt_br: &'static str,
    pub ro: &'static str,
}

impl Entry {
    pub fn get(&self, lang: Lang) -> &'static str {
        let t = self.own(lang);
        if t.is_empty() {
            self.en
        } else {
            t
        }
    }

    /// The text as written for that language ("" = not translated yet).
    fn own(&self, lang: Lang) -> &'static str {
        match lang {
            Lang::De => self.de,
            Lang::En => self.en,
            Lang::Es => self.es,
            Lang::Fr => self.fr,
            Lang::It => self.it,
            Lang::Pl => self.pl,
            Lang::PtBr => self.pt_br,
            Lang::Ro => self.ro,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(Lang::En as u8);

pub fn set(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    Lang::ALL[LANG.load(Ordering::Relaxed) as usize]
}

fn catalog() -> &'static HashMap<&'static str, &'static Entry> {
    static MAP: OnceLock<HashMap<&'static str, &'static Entry>> = OnceLock::new();
    MAP.get_or_init(|| {
        screens::ALL
            .iter()
            .flat_map(|s| s.iter())
            .map(|e| (e.en, e))
            .collect()
    })
}

/// The entry for an English text, if the catalogs have it.
pub fn entry(en: &str) -> Option<&'static Entry> {
    catalog().get(en).copied()
}

/// The text in the current language.
pub fn tr(en: &'static str) -> &'static str {
    tr_in(current(), en)
}

/// The text in English and in the current language, for matching what a
/// user types in either (Windows setting names, for example).
pub fn tr_both(en: &'static str) -> (&'static str, &'static str) {
    (en, tr(en))
}

pub fn tr_in(lang: Lang, en: &'static str) -> &'static str {
    match entry(en) {
        Some(e) => e.get(lang),
        None => en,
    }
}

/// The form of a `|`-separated text for the number `n`.
pub fn plural(en: &'static str, n: u64) -> &'static str {
    plural_in(current(), en, n)
}

pub fn plural_in(lang: Lang, en: &'static str, n: u64) -> &'static str {
    let text = tr_in(lang, en);
    let forms: Vec<&'static str> = text.split('|').collect();
    // the language's own rule when the text has that language's forms (a
    // French text may read like the English one); else English's rule,
    // as for a missing translation shown in English
    let translated = entry(en).is_some_and(|e| !e.own(lang).is_empty());
    let rule = if translated && forms.len() == lang.locale().plural_forms {
        lang
    } else {
        Lang::En
    };
    let i = form_index(rule, n).min(forms.len() - 1);
    forms[i]
}

/// Puts the values in: `{name}` becomes the value named `name`.
pub fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(&v.to_string()),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Thousands separator and decimal mark of the current language.
pub fn separators() -> (char, char) {
    let l = current().locale();
    (l.thousands, l.decimal)
}

/// "1,243,551" in English, "1.243.551" in Romanian, "1 243 551" in French.
pub fn number(n: u64) -> String {
    number_in(current(), n)
}

pub fn number_in(lang: Lang, n: u64) -> String {
    let l = lang.locale();
    let s = n.to_string();
    // Spanish and Polish write 4-digit numbers without a separator
    if s.len() < 4 + usize::from(l.min_grouping > 1) {
        return s;
    }
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 2);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(l.thousands);
        }
        out.push(ch);
    }
    out
}

/// `format!` in the current language, with named values:
/// `trf!("{count} on {drives}", count = n, drives)`.
#[macro_export]
macro_rules! trf {
    ($t:literal $(, $k:ident $(= $v:expr)?)* $(,)?) => {
        $crate::fill($crate::tr($t), &[$((stringify!($k), &$crate::__value!($k $(, $v)?) as &dyn ::std::fmt::Display)),*])
    };
}

/// A text whose words follow a number: `trn!("{n} file|{n} files", count)`;
/// `{n}` is the number, written the language's way.
#[macro_export]
macro_rules! trn {
    ($t:literal, $n:expr $(, $k:ident $(= $v:expr)?)* $(,)?) => {{
        let n = ($n) as u64;
        $crate::fill(
            $crate::plural($t, n),
            &[("n", &$crate::number(n) as &dyn ::std::fmt::Display) $(, (stringify!($k), &$crate::__value!($k $(, $v)?) as &dyn ::std::fmt::Display))*],
        )
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value {
    ($k:ident) => {
        $k
    };
    ($k:ident, $v:expr) => {
        $v
    };
}

/// The language is global: tests that change it take turns.
#[doc(hidden)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests;
