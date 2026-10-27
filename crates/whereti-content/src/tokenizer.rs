//! The content tokenizer (PLAN.md §6.4).
//!
//! - Words are runs of letters and digits; everything else separates them.
//! - Each word is folded like file names (lower case, no diacritics), so
//!   `factura` finds „Factură” and `masina` finds „MAȘINĂ”.
//! - Code words are also split: `getUserName` is indexed as `getusername`
//!   and, at the same position, `get`, `user`, `name`; `snake_case` is split
//!   by the underscore anyway.
//! - Very long "words" (base64, hashes, minified code) are dropped: nobody
//!   searches for them and they would bloat the dictionary.
//! - Scripts written without spaces (Chinese, Japanese, Thai, Lao, Khmer,
//!   Burmese) are indexed letter by letter, each at its own position: a
//!   query in them is then looked for as a phrase of those letters.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Longest word kept, in bytes after folding.
pub const MAX_WORD: usize = 40;

#[derive(Clone, Default)]
pub struct WordTokenizer {
    /// Also emit the parts of camelCase words (indexing, not queries).
    pub split_code: bool,
}

pub struct WordStream {
    tokens: Vec<Token>,
    next: usize,
}

impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = WordStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordStream {
        WordStream { tokens: tokenize(text, self.split_code), next: 0 }
    }
}

impl TokenStream for WordStream {
    fn advance(&mut self) -> bool {
        self.next += 1;
        self.next <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.next - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.next - 1]
    }
}

pub fn tokenize(text: &str, split_code: bool) -> Vec<Token> {
    let mut out = Vec::new();
    let mut position = 0;
    let mut start: Option<usize> = None;
    let mut folded = String::new();
    let flush = |from: usize, to: usize, position: &mut usize, out: &mut Vec<Token>, folded: &mut String| {
        let word = &text[from..to];
        folded.clear();
        whereti_query::fold_into(word, folded);
        if folded.is_empty() || folded.len() > MAX_WORD {
            return;
        }
        out.push(Token {
            offset_from: from,
            offset_to: to,
            position: *position,
            text: folded.clone(),
            position_length: 1,
        });
        if split_code {
            let parts = code_parts(word);
            if parts.len() > 1 {
                for (a, b) in parts {
                    let mut p = String::new();
                    whereti_query::fold_into(&word[a..b], &mut p);
                    if !p.is_empty() && p != *folded {
                        out.push(Token {
                            offset_from: from + a,
                            offset_to: from + b,
                            position: *position,
                            text: p,
                            position_length: 1,
                        });
                    }
                }
            }
        }
        *position += 1;
    };
    for (i, c) in text.char_indices() {
        if unspaced(c) {
            if let Some(s) = start.take() {
                flush(s, i, &mut position, &mut out, &mut folded);
            }
            if c.is_alphanumeric() {
                flush(i, i + c.len_utf8(), &mut position, &mut out, &mut folded);
            }
            continue;
        }
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            flush(s, i, &mut position, &mut out, &mut folded);
        }
    }
    if let Some(s) = start {
        flush(s, text.len(), &mut position, &mut out, &mut folded);
    }
    out
}

/// A letter of a script written without spaces between words: each is a
/// word of its own for the index.
pub fn unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF // Thai, Lao
        | 0x1000..=0x109F // Myanmar
        | 0x1780..=0x17FF // Khmer
        | 0x3040..=0x30FF // Hiragana, Katakana
        | 0x31F0..=0x31FF // Katakana extensions
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK
        | 0xF900..=0xFAFF // CJK compatibility
        | 0xFF66..=0xFF9F // half-width Katakana
        | 0x20000..=0x3134F // CJK extensions B-G
    )
}

/// Byte ranges of the parts of a camelCase / PascalCase / HTTPServer word.
fn code_parts(word: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = word.char_indices().collect();
    let mut parts = Vec::new();
    let mut start = 0;
    for k in 1..chars.len() {
        let (prev, cur) = (chars[k - 1].1, chars[k].1);
        let next = chars.get(k + 1).map(|c| c.1);
        let boundary = (prev.is_lowercase() && cur.is_uppercase())
            || (prev.is_alphabetic() && cur.is_ascii_digit())
            || (prev.is_ascii_digit() && cur.is_alphabetic())
            // "HTTPServer" → HTTP | Server
            || (prev.is_uppercase() && cur.is_uppercase() && next.is_some_and(char::is_lowercase));
        if boundary {
            parts.push((chars[start].0, chars[k].0));
            start = k;
        }
    }
    parts.push((chars[start].0, word.len()));
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str, split: bool) -> Vec<(usize, String)> {
        tokenize(text, split).into_iter().map(|t| (t.position, t.text)).collect()
    }

    #[test]
    fn folds_and_splits() {
        assert_eq!(
            words("Factură: TOTAL de plată, mașină-2024!", false),
            [(0, "factura"), (1, "total"), (2, "de"), (3, "plata"), (4, "masina"), (5, "2024")]
                .map(|(p, w)| (p, w.to_string()))
        );
    }

    #[test]
    fn code_words_keep_the_whole_and_the_parts() {
        let w = words("let getUserName = HTTPServer2;", true);
        let at = |p: usize| w.iter().filter(|(q, _)| *q == p).map(|(_, t)| t.as_str()).collect::<Vec<_>>();
        assert_eq!(at(0), ["let"]);
        assert_eq!(at(1), ["getusername", "get", "user", "name"]);
        assert_eq!(at(2), ["httpserver2", "http", "server", "2"]);
    }

    #[test]
    fn text_without_spaces_is_split_into_letters() {
        assert_eq!(
            words("東京都の天気 ok", false),
            [(0, "東"), (1, "京"), (2, "都"), (3, "の"), (4, "天"), (5, "気"), (6, "ok")].map(|(p, w)| (p, w.to_string()))
        );
        // a long run is no longer one dropped "word"
        let long = "这是一个很长的中文句子没有任何空格但是应该可以被搜索到";
        assert_eq!(words(long, false).len(), long.chars().count());
        // mixed with Latin letters, both sides stay words
        assert_eq!(words("abc漢字def", false).len(), 4);
    }

    #[test]
    fn long_garbage_is_dropped() {
        let b64 = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5eg";
        assert_eq!(words(&format!("cheie {b64} gata"), false), [(0, "cheie".into()), (1, "gata".into())]);
    }
}
