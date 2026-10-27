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
            // typographic ligatures and the long s, as typed on a keyboard
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            'ſ' => out.push('s'),
            // Greek final sigma is the same letter
            'ς' => out.push('σ'),
            _ => {
                for d in std::iter::once(c).nfd() {
                    if !is_combining_mark(d) || keeps_mark(d) {
                        out.extend(d.to_lowercase());
                    }
                }
            }
        }
    }
}

/// Marks that make a different letter, not an accent: the Japanese voicing
/// marks (が is not か) and the vowel signs and marks of Indic, Thai and
/// Lao scripts.
fn keeps_mark(c: char) -> bool {
    matches!(c, '\u{3099}' | '\u{309A}' | '\u{0900}'..='\u{0EFF}')
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

    #[test]
    fn ligatures_long_s_and_final_sigma() {
        assert_eq!(fold("ﬁnal ﬀ ﬆ"), "final ff st");
        assert_eq!(fold("Weißſ"), "weisss");
        assert_eq!(fold("ΛΟΓΟΣ"), fold("λογος"));
        assert_eq!(fold("λόγος"), "λογοσ");
    }

    #[test]
    fn letters_made_with_a_mark_stay_apart() {
        assert_ne!(fold("が"), fold("か"));
        assert_eq!(fold("が"), fold("か\u{3099}")); // composed or not, the same
        assert_ne!(fold("パ"), fold("ハ"));
        assert_ne!(fold("कि"), fold("क"));
    }
}
