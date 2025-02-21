//! RTF → text.
//!
//! Drops control words and groups like `\fonttbl`; keeps the text. Bytes
//! written as `\'xx` are decoded together in the code page of the current
//! font (`\fcharset`) or of the document (`\ansicpg`), so Japanese, Chinese
//! or Central European RTF reads correctly.

use std::collections::HashMap;

use encoding_rs::Encoding;

pub fn rtf_to_text(rtf: &str) -> String {
    let mut st = State {
        text: String::new(),
        pending: Vec::new(),
        doc_enc: encoding_rs::WINDOWS_1252,
        font_enc: None,
        fonts: HashMap::new(),
        stack: Vec::new(),
        skip_depth: None,
        fonttbl_depth: None,
        defining_font: None,
        uc: 1,
    };
    let mut chars = rtf.chars().peekable();
    let mut depth = 0usize;
    // characters still to drop after \u (its ANSI fallback)
    let mut fallback = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                depth += 1;
                st.stack.push((st.font_enc, st.uc));
            }
            '}' => {
                if st.skip_depth == Some(depth) {
                    st.skip_depth = None;
                }
                if st.fonttbl_depth == Some(depth) {
                    st.fonttbl_depth = None;
                }
                depth = depth.saturating_sub(1);
                st.flush();
                if let Some((enc, uc)) = st.stack.pop() {
                    st.font_enc = enc;
                    st.uc = uc;
                }
                fallback = 0;
            }
            '\\' => {
                let mut word = String::new();
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphabetic() {
                        word.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if word.is_empty() {
                    match chars.next() {
                        Some('\'') => {
                            let hex: String = chars.by_ref().take(2).collect();
                            if fallback > 0 {
                                fallback -= 1;
                            } else if let (Ok(b), true) = (u8::from_str_radix(&hex, 16), st.visible()) {
                                st.pending.push(b);
                            }
                        }
                        Some('*') => {
                            st.skip_depth.get_or_insert(depth);
                        }
                        Some(ch @ ('\\' | '{' | '}')) if st.visible() => st.push(ch),
                        Some('~') if st.visible() => st.push(' '),
                        _ => {}
                    }
                    continue;
                }
                let mut num = String::new();
                while let Some(&n) = chars.peek() {
                    if n == '-' || n.is_ascii_digit() {
                        num.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                let n: Option<i32> = num.parse().ok();
                fallback = 0;
                match word.as_str() {
                    "fonttbl" => {
                        st.fonttbl_depth.get_or_insert(depth);
                        st.skip_depth.get_or_insert(depth);
                    }
                    "colortbl" | "stylesheet" | "info" | "pict" | "header" | "footer" | "headerl" | "headerr"
                    | "footerl" | "footerr" | "listtable" | "listoverridetable" | "themedata" | "datastore"
                    | "latentstyles" | "object" | "xmlnstbl" | "rsidtbl" | "generator" => {
                        st.skip_depth.get_or_insert(depth);
                    }
                    "ansicpg" => {
                        if let Some(enc) = n.and_then(|n| code_page(n as u32)) {
                            st.flush();
                            st.doc_enc = enc;
                        }
                    }
                    "f" if st.fonttbl_depth.is_some() => st.defining_font = n,
                    "fcharset" if st.fonttbl_depth.is_some() => {
                        if let (Some(f), Some(enc)) = (st.defining_font, n.and_then(charset)) {
                            st.fonts.insert(f, enc);
                        }
                    }
                    "f" => {
                        st.flush();
                        st.font_enc = n.and_then(|n| st.fonts.get(&n).copied());
                    }
                    "uc" => st.uc = n.unwrap_or(1).max(0) as usize,
                    "par" | "line" | "row" | "sect" | "page" if st.visible() => st.push('\n'),
                    "tab" | "cell" if st.visible() => st.push('\t'),
                    "emdash" if st.visible() => st.push('—'),
                    "endash" if st.visible() => st.push('–'),
                    "lquote" if st.visible() => st.push('‘'),
                    "rquote" if st.visible() => st.push('’'),
                    "ldblquote" if st.visible() => st.push('“'),
                    "rdblquote" if st.visible() => st.push('”'),
                    "bullet" if st.visible() => st.push('•'),
                    "u" if st.visible() => {
                        if let Some(ch) = n.and_then(|n| char::from_u32(n as u16 as u32)) {
                            st.push(ch);
                        }
                        fallback = st.uc;
                    }
                    _ => {}
                }
            }
            '\r' | '\n' => {}
            _ if fallback > 0 => fallback -= 1,
            c if st.visible() => st.push(c),
            _ => {}
        }
    }
    st.flush();
    st.text
}

struct State {
    text: String,
    pending: Vec<u8>,
    doc_enc: &'static Encoding,
    font_enc: Option<&'static Encoding>,
    fonts: HashMap<i32, &'static Encoding>,
    /// (font encoding, \uc) saved at each `{`
    stack: Vec<(Option<&'static Encoding>, usize)>,
    skip_depth: Option<usize>,
    fonttbl_depth: Option<usize>,
    defining_font: Option<i32>,
    uc: usize,
}

impl State {
    fn visible(&self) -> bool {
        self.skip_depth.is_none()
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let enc = self.font_enc.unwrap_or(self.doc_enc);
            self.text.push_str(&enc.decode_without_bom_handling(&self.pending).0);
            self.pending.clear();
        }
    }

    fn push(&mut self, c: char) {
        self.flush();
        self.text.push(c);
    }
}

fn code_page(cp: u32) -> Option<&'static Encoding> {
    use encoding_rs::*;
    Some(match cp {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => return None,
    })
}

/// `\fcharset` numbers → code pages.
fn charset(cs: i32) -> Option<&'static Encoding> {
    code_page(match cs {
        0 => 1252,
        128 => 932,
        129 => 949,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::rtf_to_text;

    #[test]
    fn basic_text_and_skipped_groups() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0 Arial{\*\panose 0204}Arial;}{\f1 Symbol;}}\f0 Contract \'e2n vigoare\par Pre\u539?ul total}";
        assert_eq!(rtf_to_text(rtf), "Contract ân vigoare\nPrețul total");
    }

    #[test]
    fn code_pages_per_document_and_per_font() {
        let jp = r"{\rtf1\ansi\ansicpg932 \'93\'fa\'96\'7b\par\uc2 \u26085\'93\'fa}";
        assert_eq!(rtf_to_text(jp), "日本\n日");
        // Western document with a Japanese font: \'93 is a quote in f0, a lead byte in f1
        let mixed = r"{\rtf1\ansi\ansicpg1252{\fonttbl{\f0\fcharset0 Arial;}{\f1\fcharset128 MS Gothic;}}\f0 \'93Hi\'94 {\f1 \'93\'fa\'96\'7b} end}";
        assert_eq!(rtf_to_text(mixed), "“Hi” 日本 end");
    }
}
