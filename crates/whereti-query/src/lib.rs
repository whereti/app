//! whereti query language (compatible with Everything's core syntax).
//!
//! ```text
//! raport 2024            AND (both in the name)
//! raport | factura       OR          !tmp      NOT
//! "nume exact.txt"       literal     *.pdf     wildcard (whole name)
//! ext:pdf;docx           size:>10mb  size:1kb..1mb  size:large
//! dm:today  dm:lastweek  dm:2024-01..2024-06
//! path:D:\proiecte       proiecte\whereti (a backslash means "match the path")
//! folder:  file:         regex:^IMG_\d{4}\.jpg$
//! mine:                  only the user's own files: no system, hidden,
//!                        program, cache or build folders
//! ```
//!
//! Parsing never fails: anything that is not valid syntax is searched literally.

mod date;
mod fold;
mod glob;

pub use date::{civil_from_day, day_from_civil, DateCtx};
pub use fold::{fold, fold_into};
pub use glob::{glob_match, longest_literal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Substring of the (folded) file name.
    Text(String),
    /// Whole-name wildcard pattern (folded).
    Glob(String),
    /// Regular expression on the original name, case-insensitive.
    Regex(String),
    /// Extension is one of these (folded, no dot; "" = no extension).
    Ext(Vec<String>),
    /// Substring of the folded full path, `\` as separator (e.g. `d:\proiecte`).
    Path(String),
    /// File size in bytes, inclusive.
    Size { min: u64, max: u64 },
    /// Last write, minutes since 1601 UTC, inclusive.
    Modified { min: u32, max: u32 },
    FoldersOnly,
    FilesOnly,
    /// Not hidden, not system, not inside program / cache / build folders.
    Mine,
    /// Content search (not available in the name index).
    Content(String),
    /// Every term must match (e.g. `folder:raport` = folder AND "raport").
    All(Vec<Term>),
}

/// Terms joined by OR, optionally negated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub negated: bool,
    pub any_of: Vec<Term>,
}

/// Clauses joined by AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub clauses: Vec<Clause>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }
}

pub fn parse(input: &str, ctx: &DateCtx) -> Query {
    let mut query = Query::default();
    let mut join_next = false;
    for token in lex(input) {
        if token.alternatives.len() == 1 && token.alternatives[0].is_bare_pipe() {
            join_next = !query.clauses.is_empty();
            continue;
        }
        let mut negated = false;
        let mut terms = Vec::new();
        for (i, mut alt) in token.alternatives.into_iter().enumerate() {
            if i == 0 && alt.quote_at != Some(0) && alt.text.starts_with('!') {
                negated = true;
                alt.text.remove(0);
                alt.quote_at = alt.quote_at.map(|q| q - 1);
            }
            if let Some(t) = parse_alternative(&alt, ctx) {
                terms.push(t);
            }
        }
        if terms.is_empty() {
            continue;
        }
        match query.clauses.last_mut() {
            Some(last) if join_next && !last.negated && !negated => last.any_of.extend(terms),
            _ => query.clauses.push(Clause { negated, any_of: terms }),
        }
        join_next = false;
    }
    query
}

/// One `|`-separated piece of a token.
#[derive(Debug, Default)]
struct Alt {
    /// Content with quotes removed.
    text: String,
    /// Byte offset in `text` where the first quoted section began.
    quote_at: Option<usize>,
    pipe_only: bool,
}

impl Alt {
    fn is_bare_pipe(&self) -> bool {
        self.pipe_only
    }
}

struct Token {
    alternatives: Vec<Alt>,
}

fn lex(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut alts: Vec<Alt> = Vec::new();
    let mut cur = Alt::default();
    let mut in_quotes = false;
    let mut saw_any = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.quote_at.get_or_insert(cur.text.len());
                saw_any = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if saw_any {
                    alts.push(std::mem::take(&mut cur));
                    tokens.push(Token { alternatives: std::mem::take(&mut alts) });
                    saw_any = false;
                }
            }
            '|' if !in_quotes => {
                if !saw_any && alts.is_empty() {
                    // standalone `|` between two words
                    tokens.push(Token { alternatives: vec![Alt { pipe_only: true, ..Default::default() }] });
                } else {
                    alts.push(std::mem::take(&mut cur));
                    saw_any = true;
                }
            }
            c => {
                cur.text.push(c);
                saw_any = true;
            }
        }
    }
    if saw_any {
        alts.push(cur);
        tokens.push(Token { alternatives: alts });
    }
    // drop empty alternatives produced by `a||b` or a trailing `|`
    for t in &mut tokens {
        if !(t.alternatives.len() == 1 && t.alternatives[0].pipe_only) {
            t.alternatives.retain(|a| !a.text.is_empty());
        }
    }
    tokens.retain(|t| !t.alternatives.is_empty());
    tokens
}

fn parse_alternative(alt: &Alt, ctx: &DateCtx) -> Option<Term> {
    let text = alt.text.as_str();
    if text.is_empty() {
        return None;
    }
    let unquoted_end = alt.quote_at.unwrap_or(text.len());
    if let Some(colon) = text[..unquoted_end].find(':') {
        let prefix = text[..colon].to_ascii_lowercase();
        let value = &text[colon + 1..];
        let value_quoted = alt.quote_at.is_some();
        if let Some(t) = parse_function(&prefix, value, value_quoted, ctx) {
            return Some(t);
        }
    }
    if alt.quote_at == Some(0) {
        return Some(if text.contains(['\\', '/']) { path_term(text) } else { Term::Text(fold(text)) });
    }
    Some(plain_term(text))
}

fn plain_term(text: &str) -> Term {
    if text.contains(['\\', '/']) {
        path_term(text)
    } else if text.contains(['*', '?']) {
        Term::Glob(fold(text))
    } else {
        Term::Text(fold(text))
    }
}

fn path_term(text: &str) -> Term {
    Term::Path(fold(&text.replace('/', "\\")))
}

fn parse_function(prefix: &str, value: &str, quoted: bool, ctx: &DateCtx) -> Option<Term> {
    match prefix {
        "ext" => Some(Term::Ext(
            value
                .split([';', ','])
                .map(|e| fold(e.trim().trim_start_matches('.')))
                .collect(),
        )),
        "path" if !value.is_empty() => Some(path_term(value)),
        "size" => parse_size(value).map(|(min, max)| Term::Size { min, max }),
        "dm" | "datemodified" => date::parse_range(value, ctx).map(|(min, max)| Term::Modified { min, max }),
        "regex" if !value.is_empty() => Some(Term::Regex(value.to_string())),
        "c" | "content" if !value.is_empty() => Some(Term::Content(fold(value))),
        "mine" if value.is_empty() => Some(Term::Mine),
        "folder" | "file" => {
            let kind = if prefix == "folder" { Term::FoldersOnly } else { Term::FilesOnly };
            if value.is_empty() {
                Some(kind)
            } else {
                let inner = if quoted { Term::Text(fold(value)) } else { plain_term(value) };
                Some(Term::All(vec![kind, inner]))
            }
        }
        _ => None,
    }
}

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const GB: u64 = MB * 1024;
const TB: u64 = GB * 1024;

/// `>10mb`, `<=1gb`, `1kb..1mb`, `10mb` (10.0–10.99 MB), `large`, … -> inclusive byte range.
fn parse_size(v: &str) -> Option<(u64, u64)> {
    let v = v.trim().to_ascii_lowercase();
    let named = match v.as_str() {
        "empty" => Some((0, 0)),
        "tiny" => Some((0, 10 * KB - 1)),
        "small" => Some((10 * KB, 100 * KB - 1)),
        "medium" => Some((100 * KB, MB - 1)),
        "large" => Some((MB, 16 * MB - 1)),
        "huge" => Some((16 * MB, 128 * MB - 1)),
        "gigantic" => Some((128 * MB, u64::MAX)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((a, b)) = v.split_once("..") {
        let (lo, _) = size_value(a)?;
        let (hi, unit) = size_value(b)?;
        let hi = hi.saturating_add(unit.saturating_sub(1));
        return (lo <= hi).then_some((lo, hi));
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            let (n, _) = size_value(rest)?;
            return Some(match op {
                ">=" => (n, u64::MAX),
                "<=" => (0, n),
                ">" => (n.saturating_add(1), u64::MAX),
                "<" => (0, n.checked_sub(1)?),
                _ => (n, n),
            });
        }
    }
    let (n, unit) = size_value(&v)?;
    Some((n, n.saturating_add(unit.saturating_sub(1))))
}

/// Parses "10mb" / "1.5 GB" / "300" -> (bytes, unit size).
fn size_value(s: &str) -> Option<(u64, u64)> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let unit_size = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" => KB,
        "m" | "mb" => MB,
        "g" | "gb" => GB,
        "t" | "tb" => TB,
        _ => return None,
    };
    if !num.is_finite() || num < 0.0 {
        return None;
    }
    Some(((num * unit_size as f64) as u64, unit_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> DateCtx {
        DateCtx { now: (day_from_civil(2026, 10, 24) * 1440 + 720) as u32, tz_offset: 180 }
    }

    fn q(s: &str) -> Vec<Clause> {
        parse(s, &ctx()).clauses
    }

    fn one(s: &str) -> Term {
        let c = q(s);
        assert_eq!(c.len(), 1, "{s}: {c:?}");
        assert_eq!(c[0].any_of.len(), 1, "{s}: {c:?}");
        c[0].any_of[0].clone()
    }

    #[test]
    fn words_are_anded_and_folded() {
        let c = q("Raport  MAȘINĂ");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].any_of, vec![Term::Text("raport".into())]);
        assert_eq!(c[1].any_of, vec![Term::Text("masina".into())]);
    }

    #[test]
    fn or_negation_and_quotes() {
        let c = q("raport | factura !tmp \"nume exact.txt\"");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].any_of.len(), 2);
        assert!(c[1].negated);
        assert_eq!(c[2].any_of, vec![Term::Text("nume exact.txt".into())]);
        let c = q("a|b|c");
        assert_eq!(c[0].any_of.len(), 3);
    }

    #[test]
    fn wildcards_paths_and_functions() {
        assert_eq!(one("*.PDF"), Term::Glob("*.pdf".into()));
        assert_eq!(one(r"D:\Proiecte\whereti"), Term::Path(r"d:\proiecte\whereti".into()));
        assert_eq!(one("proiecte/whereti"), Term::Path(r"proiecte\whereti".into()));
        assert_eq!(one("ext:pdf;.DOCX"), Term::Ext(vec!["pdf".into(), "docx".into()]));
        assert_eq!(one(r#"path:"C:\Program Files""#), Term::Path(r"c:\program files".into()));
        assert_eq!(one("folder:"), Term::FoldersOnly);
        assert_eq!(
            one("file:raport"),
            Term::All(vec![Term::FilesOnly, Term::Text("raport".into())])
        );
        assert_eq!(one(r"regex:^IMG_\d+"), Term::Regex(r"^IMG_\d+".into()));
    }

    #[test]
    fn unknown_or_invalid_syntax_is_literal() {
        assert_eq!(one("size:enorm"), Term::Text("size:enorm".into()));
        assert_eq!(one("ora:12"), Term::Text("ora:12".into()));
        assert_eq!(one("\"ext:pdf\""), Term::Text("ext:pdf".into()));
        assert!(q("   ").is_empty());
        assert!(q("|").is_empty());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size(">10mb"), Some((10 * MB + 1, u64::MAX)));
        assert_eq!(parse_size("<1kb"), Some((0, 1023)));
        assert_eq!(parse_size("1kb..1mb"), Some((KB, 2 * MB - 1)));
        assert_eq!(parse_size("10mb"), Some((10 * MB, 11 * MB - 1)));
        assert_eq!(parse_size("1.5gb"), Some((GB + GB / 2, GB + GB / 2 + GB - 1)));
        assert_eq!(parse_size("large"), Some((MB, 16 * MB - 1)));
        assert_eq!(parse_size("<0"), None);
        assert_eq!(parse_size("-5mb"), None);
    }

    #[test]
    fn dates_parse_through_the_query() {
        assert!(matches!(one("dm:today"), Term::Modified { .. }));
        assert_eq!(one("dm:nicicând"), Term::Text("dm:nicicand".into()));
    }
}
