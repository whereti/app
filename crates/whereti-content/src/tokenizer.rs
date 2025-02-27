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
    fn long_garbage_is_dropped() {
        let b64 = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5eg";
        assert_eq!(words(&format!("cheie {b64} gata"), false), [(0, "cheie".into()), (1, "gata".into())]);
    }
}
