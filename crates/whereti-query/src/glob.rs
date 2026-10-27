//! Whole-name wildcard matching: `*` = any run of characters, `?` = one character.

/// Both arguments must already be folded. `?` is one folded character, so
/// a letter folded to two (ß, æ, œ) needs `??`: `stra??e` finds Straße.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p = pattern.as_bytes();
    let n = name.as_bytes();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ni));
            pi += 1;
        } else if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            ni += char_len(n[ni]);
        } else if pi < p.len() && p[pi] == n[ni] {
            pi += 1;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            // backtrack: let the last `*` swallow one more character
            let next = sn + char_len(n[sn]);
            star = Some((sp, next));
            pi = sp + 1;
            ni = next;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Longest run of literal bytes in a pattern, used to pre-filter candidates.
pub fn longest_literal(pattern: &str) -> &str {
    pattern
        .split(['*', '?'])
        .max_by_key(|s| s.len())
        .unwrap_or("")
}

fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_whole_names() {
        assert!(glob_match("*.pdf", "raport.pdf"));
        assert!(!glob_match("*.pdf", "raport.pdf.bak"));
        assert!(glob_match("rap?rt*", "raport anual.docx"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("?", "ș")); // one character, two bytes
        assert!(glob_match("img_????.jpg", "img_0042.jpg"));
    }

    #[test]
    fn longest_literal_picks_the_best_prefilter() {
        assert_eq!(longest_literal("*.pdf"), ".pdf");
        assert_eq!(longest_literal("rap?rt_anual*"), "rt_anual");
        assert_eq!(longest_literal("*"), "");
    }
}
