//! Text folding: the single definition of "these two names match".
//!
//! Lowercase, diacritics removed, a few letters spelled out. Applied to file
//! names when the index is built and to the query when it is parsed, so
//! `MAȘINĂ`, `mașină`, `masina` and `maşină` (cedilla) all compare equal.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_into(s, &mut out);
    out
}

pub fn fold_into(s: &str, out: &mut String) {
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
            continue;
        }
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ı' => out.push('i'),
            'ø' | 'Ø' => out.push('o'),
            'đ' | 'Đ' => out.push('d'),
            'ł' | 'Ł' => out.push('l'),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            _ => {
                for d in std::iter::once(c).nfd() {
                    if !is_combining_mark(d) {
                        out.extend(d.to_lowercase());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn romanian_diacritics_fold_to_ascii() {
        assert_eq!(fold("MAȘINĂ"), "masina");
        assert_eq!(fold("maşină"), "masina"); // cedilla variant
        assert_eq!(fold("Țară Îngustă Âncă"), "tara ingusta anca");
    }

    #[test]
    fn other_scripts_and_specials() {
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Ærø"), "aero");
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("e\u{0301}"), "e"); // decomposed é
        assert_eq!(fold("ΔΟΚΙΜΗ"), "δοκιμη");
        assert_eq!(fold("報告.PDF"), "報告.pdf");
    }
}
