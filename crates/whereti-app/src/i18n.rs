//! Interface language: English by default, Romanian on request (Settings).
//!
//! Every text is written in both languages where it is used:
//! `tr("Search…", "Caută…")`, or `trf!("{n} files", "{n} fișiere")` for
//! formatted ones. Numbers and dates follow the language too.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    English,
    Romanian,
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 {
        Lang::Romanian
    } else {
        Lang::English
    }
}

pub fn ro() -> bool {
    current() == Lang::Romanian
}

/// The text in the current language.
pub fn tr(en: &'static str, ro: &'static str) -> &'static str {
    if self::ro() {
        ro
    } else {
        en
    }
}

/// `format!` in the current language: `trf!("{n} files", "{n} fișiere")`.
#[macro_export]
macro_rules! trf {
    ($en:literal, $ro:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::ro() { format!($ro $(, $arg)*) } else { format!($en $(, $arg)*) }
    };
}

/// Thousands separator and decimal mark: "2,509,598" and "12.3" in
/// English, "2.509.598" and "12,3" in Romanian.
pub fn separators() -> (char, char) {
    if ro() {
        ('.', ',')
    } else {
        (',', '.')
    }
}

/// The language is global: tests that change it take turns.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
