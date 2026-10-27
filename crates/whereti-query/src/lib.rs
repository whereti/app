//! whereti query language (compatible with Everything's core syntax).
//!
//! ```text
//! raport 2024            AND (both in the name)
//! raport | factura       OR          !tmp      NOT
//! "nume exact.txt"       literal     *.pdf     wildcard (whole name)
//! ext:pdf;docx           size:>10mb  size:1kb..1mb  size:10mb..  size:large
//! dm:today  dm:lastweek  dm:2024-01..2024-06  dm:2024..
//! path:D:\proiecte       proiecte\whereti (a backslash means "match the path")
//! C:\Users  D:           a drive letter is a path, not c: (content)
//! D:\poze\*.jpg          folders as a path, the last part as a wildcard
//! folder:  file:         regex:^IMG_\d{4}\.jpg$   regex:^(IMG|DSC)_
//!                        (a | inside regex: is part of it; quote spaces)
//! taken:2025-08  camera:canon  width:>4000  height:<1000
//! artist:queen  album:abbey  duration:>5m  author:popescu
//! mine:                  only the user's own files: no system, hidden,
//!                        program, cache or build folders
//! ```
//!
//! Parsing never fails: anything that is not valid syntax is searched literally.

mod date;
pub mod kinds;
mod fold;
mod glob;

pub use date::{civil_from_day, day_from_civil, set_tz_rules, DateCtx, Transition, TzRules};
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
    /// Where a photo was taken: a town, region or country (`loc:roma`),
    /// looked up in the content index among photos.
    Place(String),
    /// A property read from the file: when a photo was taken, its camera
    /// and size, a song's artist, a video's length, a document's author.
    /// Answered by the engine's property store, never by names.
    Prop(Prop),
    /// `dupe:` identical files (answered by the engine, which compares
    /// their content; to the name index, simply files).
    Dupe,
    /// Every term must match (e.g. `folder:raport` = folder AND "raport").
    All(Vec<Term>),
}

/// A file property condition (`Term::Prop`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prop {
    /// When the photo was taken, minutes since 1601 UTC, inclusive.
    Taken { min: u32, max: u32 },
    /// Picture size in pixels, inclusive (videos: not read).
    Width { min: u32, max: u32 },
    Height { min: u32, max: u32 },
    /// Song or video length in seconds, inclusive.
    Duration { min: u32, max: u32 },
    /// Folded text the property must contain.
    Camera(String),
    Artist(String),
    Album(String),
    Author(String),
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
    let mut negate_next = false;
    for token in lex(input) {
        if token.alternatives.len() == 1 && token.alternatives[0].is_bare_pipe() {
            join_next = !query.clauses.is_empty();
            continue;
        }
        // a lone `!` negates the next word: `! tmp` = `!tmp`
        if token.alternatives.len() == 1 && token.alternatives[0].is_bare_bang() {
            negate_next = true;
            continue;
        }
        let mut negated = std::mem::take(&mut negate_next);
        let mut terms = Vec::new();
        // A clause is OR-ed terms or a negated OR, so `a | !b` cannot be
        // said exactly: the negated part is kept as its own `!b` clause
        // (a AND NOT b, a narrower result than a OR NOT b).
        let mut negated_alts = Vec::new();
        for (i, mut alt) in token.alternatives.into_iter().enumerate() {
            let bang = alt.quote_at != Some(0) && alt.text.len() > 1 && alt.text.starts_with('!');
            if bang {
                alt.text.remove(0);
                alt.quote_at = alt.quote_at.map(|q| q - 1);
            }
            let Some(t) = parse_alternative(&alt, ctx) else { continue };
            if bang && i > 0 {
                negated_alts.push(t);
            } else {
                negated |= bang;
                terms.push(t);
            }
        }
        if terms.is_empty() && negated_alts.is_empty() {
            continue;
        }
        if !terms.is_empty() {
            match query.clauses.last_mut() {
                Some(last) if join_next && !last.negated && !negated => last.any_of.extend(terms),
                _ => query.clauses.push(Clause { negated, any_of: terms }),
            }
        }
        for t in negated_alts {
            query.clauses.push(Clause { negated: true, any_of: vec![t] });
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

    fn is_bare_bang(&self) -> bool {
        self.text == "!" && self.quote_at.is_none()
    }

    /// Inside a `regex:` value a `|` belongs to the expression
    /// (`regex:^(IMG|DSC)_`); a space still needs quotes (`regex:"a b"`).
    fn in_regex(&self) -> bool {
        let t = self.text.strip_prefix('!').unwrap_or(&self.text);
        self.quote_at != Some(0) && t.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("regex:"))
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
    // the word ended with `|`: `raport| factura` joins like `raport | factura`
    let mut ends_with_pipe = false;
    let pipe = || Token { alternatives: vec![Alt { pipe_only: true, ..Default::default() }] };
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.quote_at.get_or_insert(cur.text.len());
                saw_any = true;
                ends_with_pipe = false;
            }
            c if c.is_whitespace() && !in_quotes => {
                if saw_any {
                    alts.push(std::mem::take(&mut cur));
                    tokens.push(Token { alternatives: std::mem::take(&mut alts) });
                    if ends_with_pipe {
                        tokens.push(pipe());
                    }
                    saw_any = false;
                }
                ends_with_pipe = false;
            }
            '|' if !in_quotes && !cur.in_regex() => {
                if !saw_any && alts.is_empty() {
                    // standalone `|` between two words
                    tokens.push(pipe());
                } else {
                    alts.push(std::mem::take(&mut cur));
                    saw_any = true;
                    ends_with_pipe = true;
                }
            }
            c => {
                cur.text.push(c);
                saw_any = true;
                ends_with_pipe = false;
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
        // `C:\Users`, `c:/x`, `D:`: a drive, not `c:` (content)
        let drive = colon == 1
            && text.as_bytes()[0].is_ascii_alphabetic()
            && (value.is_empty() || value.starts_with(['\\', '/']));
        if !drive {
            if let Some(t) = parse_function(&prefix, value, value_quoted, ctx) {
                return Some(t);
            }
        }
    }
    if alt.quote_at == Some(0) {
        return Some(if text.contains(['\\', '/']) { path_term(text) } else { Term::Text(fold(text)) });
    }
    Some(plain_term(text))
}

fn plain_term(text: &str) -> Term {
    if text.len() == 2 && text.as_bytes()[0].is_ascii_alphabetic() && text.ends_with(':') {
        // `D:` alone: everything on that drive
        Term::Path(fold(text))
    } else if text.contains(['\\', '/']) {
        if text.contains(['*', '?']) {
            path_glob(text)
        } else {
            path_term(text)
        }
    } else if text.contains(['*', '?']) {
        Term::Glob(fold(text))
    } else {
        Term::Text(fold(text))
    }
}

fn path_term(text: &str) -> Term {
    Term::Path(fold(&text.replace('/', "\\")))
}

/// `D:\poze\*.jpg`: the folders as a path (`d:\poze\`, so its subfolders
/// too) and the last part as a whole-name wildcard. A folder with a
/// wildcard is left out: `d:\*\vacanta\*.jpg` is `*.jpg` somewhere under
/// `d:\` and under a folder named `vacanta`.
fn path_glob(text: &str) -> Term {
    let folded = fold(&text.replace('/', "\\"));
    let folded = folded.trim_end_matches('\\');
    let (head, last) = folded.rsplit_once('\\').unwrap_or(("", folded));
    let mut terms = Vec::new();
    // runs of folders without wildcards; the first one anchors at the start
    let mut run: Vec<&str> = Vec::new();
    let mut first = true;
    let parts = if head.is_empty() { Vec::new() } else { head.split('\\').collect() };
    for part in parts.into_iter().map(Some).chain([None]) {
        match part {
            Some(p) if !p.contains(['*', '?']) => run.push(p),
            _ => {
                let joined = run.join("\\");
                if !joined.is_empty() {
                    terms.push(Term::Path(if first { format!("{joined}\\") } else { format!("\\{joined}\\") }));
                }
                run.clear();
                first = false;
            }
        }
    }
    terms.push(Term::Glob(last.to_string()));
    if terms.len() == 1 {
        terms.remove(0)
    } else {
        Term::All(terms)
    }
}

fn parse_function(prefix: &str, value: &str, quoted: bool, ctx: &DateCtx) -> Option<Term> {
    match prefix {
        "ext" => {
            // `ext:pdf;` is pdf only; `ext:` alone means "no extension"
            let mut exts: Vec<String> = value
                .split([';', ','])
                .map(|e| fold(e.trim().trim_start_matches('.')))
                .filter(|e| !e.is_empty())
                .collect();
            if exts.is_empty() {
                exts.push(String::new());
            }
            Some(Term::Ext(exts))
        }
        "path" if !value.is_empty() => {
            Some(if !quoted && value.contains(['*', '?']) { path_glob(value) } else { path_term(value) })
        }
        "size" => parse_size(value).map(|(min, max)| Term::Size { min, max }),
        "dm" | "datemodified" => date::parse_range(value, ctx).map(|(min, max)| Term::Modified { min, max }),
        "regex" if !value.is_empty() => Some(Term::Regex(value.to_string())),
        "c" | "content" | "contains" if !value.is_empty() => Some(Term::Content(fold(value))),
        "loc" | "location" | "place" if !value.is_empty() => Some(Term::Place(fold(value))),
        "taken" | "dt" | "datetaken" => date::parse_range(value, ctx).map(|(min, max)| Term::Prop(Prop::Taken { min, max })),
        "width" => parse_count(value, 1).map(|(min, max)| Term::Prop(Prop::Width { min, max })),
        "height" => parse_count(value, 1).map(|(min, max)| Term::Prop(Prop::Height { min, max })),
        "duration" | "length" => parse_duration(value).map(|(min, max)| Term::Prop(Prop::Duration { min, max })),
        "camera" if !value.is_empty() => Some(Term::Prop(Prop::Camera(fold(value)))),
        "artist" if !value.is_empty() => Some(Term::Prop(Prop::Artist(fold(value)))),
        "album" if !value.is_empty() => Some(Term::Prop(Prop::Album(fold(value)))),
        "author" if !value.is_empty() => Some(Term::Prop(Prop::Author(fold(value)))),
        "mine" if value.is_empty() => Some(Term::Mine),
        "dupe" | "dupes" | "duplicate" | "duplicates" if value.is_empty() => Some(Term::Dupe),
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

/// `>4000`, `<=1000`, `1000..2000`, `4000` -> inclusive range, each number
/// times `unit`.
fn parse_count(v: &str, unit: u32) -> Option<(u32, u32)> {
    parse_range_with(v, |s| s.trim().parse::<u32>().ok().map(|n| (n.saturating_mul(unit), 1)))
}

/// `>5m`, `<90`, `1h..2h`, `3m30s` (seconds by default) -> seconds, inclusive.
fn parse_duration(v: &str) -> Option<(u32, u32)> {
    parse_range_with(v, |s| {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        // "1h30m", "3m30s", "90s", "90", "5min"
        let (mut total, mut num, mut unit) = (0u32, String::new(), 1u32);
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c.is_ascii_digit() {
                num.push(c);
                continue;
            }
            let u = match c {
                'h' => 3600,
                'm' => {
                    // "min"
                    if chars.peek() == Some(&'i') {
                        chars.next();
                        if chars.peek() == Some(&'n') {
                            chars.next();
                        }
                    }
                    60
                }
                's' => 1,
                _ => return None,
            };
            total = total.saturating_add(num.parse::<u32>().ok()?.saturating_mul(u));
            num.clear();
            unit = u;
        }
        if !num.is_empty() {
            total = total.saturating_add(num.parse::<u32>().ok()?);
            unit = 1;
        }
        // "5m" means 5:00 to 5:59
        Some((total, unit))
    })
}

/// The comparison syntax of `size:`, for any value parser returning
/// `(value, unit)`: `=v` / `v` cover a whole unit.
fn parse_range_with(v: &str, value: impl Fn(&str) -> Option<(u32, u32)>) -> Option<(u32, u32)> {
    let v = v.trim();
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.trim().is_empty() { 0 } else { value(a)?.0 };
        let hi = if b.trim().is_empty() { u32::MAX } else { value(b).map(|(n, u)| n.saturating_add(u - 1))? };
        return (lo <= hi).then_some((lo, hi));
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            let (n, u) = value(rest)?;
            // a value covers its whole unit: "5m" is 5:00 to 5:59, so ">5m"
            // starts at 6:00 and "<=5m" ends at 5:59 (as dates do)
            return Some(match op {
                ">=" => (n, u32::MAX),
                "<=" => (0, n.saturating_add(u - 1)),
                ">" => (n.saturating_add(u), u32::MAX),
                "<" => (0, n.checked_sub(1)?),
                _ => (n, n.saturating_add(u - 1)),
            });
        }
    }
    let (n, u) = value(v)?;
    Some((n, n.saturating_add(u - 1)))
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
        // `10mb..` and `..10mb` are open ends
        if a.trim().is_empty() && b.trim().is_empty() {
            return None;
        }
        let lo = if a.trim().is_empty() { 0 } else { size_value(a)?.0 };
        let hi = if b.trim().is_empty() {
            u64::MAX
        } else {
            // an upper bound covers its last digit, or a tenth of the unit
            // for a whole number (`..5mb` = up to what shows as "5.0 MB")
            let (n, unit, decimals) = size_value(b)?;
            n.saturating_add(step(unit, decimals.max(1)) - 1)
        };
        return (lo <= hi).then_some((lo, hi));
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            let (n, _, _) = size_value(rest)?;
            return Some(match op {
                ">=" => (n, u64::MAX),
                "<=" => (0, n),
                ">" => (n.saturating_add(1), u64::MAX),
                "<" => (0, n.checked_sub(1)?),
                _ => (n, n),
            });
        }
    }
    // "5mb" is roughly 5 MB (5.0 to 5.99 MB); "1.5gb" is 1.5 to 1.59 GB
    let (n, unit, decimals) = size_value(&v)?;
    Some((n, n.saturating_add(step(unit, decimals) - 1)))
}

/// The size one step of the last digit stands for: a whole unit, or a
/// tenth of it after one decimal, and so on (at least one byte).
fn step(unit: u64, decimals: u32) -> u64 {
    (unit / 10u64.saturating_pow(decimals)).max(1)
}

/// Parses "10mb" / "1.5 GB" / "300" -> (bytes, unit size, decimals typed).
fn size_value(s: &str) -> Option<(u64, u64, u32)> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let decimals = num.split_once('.').map_or(0, |(_, f)| f.len() as u32);
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
    Some(((num * unit_size as f64) as u64, unit_size, decimals))
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
        // an upper bound reaches a tenth of its unit past the value, not a whole unit
        assert_eq!(parse_size("1kb..1mb"), Some((KB, MB + MB / 10 - 1)));
        assert_eq!(parse_size("1kb..1.25mb"), Some((KB, MB + MB / 4 + MB / 100 - 1)));
        assert_eq!(parse_size("..300"), Some((0, 300)));
        // a bare whole number is roughly that size, a fraction is tighter
        assert_eq!(parse_size("10mb"), Some((10 * MB, 11 * MB - 1)));
        assert_eq!(parse_size("1.5gb"), Some((GB + GB / 2, GB + GB / 2 + GB / 10 - 1)));
        assert_eq!(parse_size("300"), Some((300, 300)));
        assert_eq!(parse_size("large"), Some((MB, 16 * MB - 1)));
        assert_eq!(parse_size("<0"), None);
        assert_eq!(parse_size("-5mb"), None);
    }

    #[test]
    fn open_ended_sizes() {
        assert_eq!(parse_size("10mb.."), Some((10 * MB, u64::MAX)));
        assert_eq!(parse_size("..10mb"), Some((0, 10 * MB + MB / 10 - 1)));
        assert_eq!(parse_size(".."), None);
        assert_eq!(one("size:1gb.."), Term::Size { min: GB, max: u64::MAX });
    }

    #[test]
    fn drive_letters_are_paths_not_content() {
        assert_eq!(one(r"C:\Users\ana\Documents"), Term::Path(r"c:\users\ana\documents".into()));
        assert_eq!(one(r"c:\windows"), Term::Path(r"c:\windows".into()));
        assert_eq!(one("c:/windows"), Term::Path(r"c:\windows".into()));
        assert_eq!(one("c:"), Term::Path("c:".into()));
        assert_eq!(one("D:"), Term::Path("d:".into()));
        assert_eq!(one(r#""C:\Program Files""#), Term::Path(r"c:\program files".into()));
        // still content when a word follows
        assert_eq!(one("c:factura"), Term::Content("factura".into()));
        assert_eq!(one("content:raport"), Term::Content("raport".into()));
    }

    #[test]
    fn a_bar_at_either_end_of_a_word_still_means_or() {
        for s in ["raport| factura", "raport |factura", "raport | factura", "raport|factura"] {
            let c = q(s);
            assert_eq!(c.len(), 1, "{s}: {c:?}");
            assert_eq!(c[0].any_of, vec![Term::Text("raport".into()), Term::Text("factura".into())], "{s}");
        }
        let c = q("raport| factura 2024");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn a_bar_inside_a_regex_belongs_to_it() {
        assert_eq!(one(r"regex:^(IMG|DSC)_\d+"), Term::Regex(r"^(IMG|DSC)_\d+".into()));
        assert_eq!(one(r"REGEX:a|b"), Term::Regex("a|b".into()));
        assert_eq!(one(r#"regex:"^a b|c""#), Term::Regex("^a b|c".into()));
        let c = q(r"!regex:^(a|b)$");
        assert!(c[0].negated);
        assert_eq!(c[0].any_of, vec![Term::Regex("^(a|b)$".into())]);
        // a standalone bar still joins a regex with a word
        let c = q("regex:^x | raport");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].any_of.len(), 2);
    }

    #[test]
    fn wildcards_in_a_path() {
        assert_eq!(
            one(r"D:\poze\*.jpg"),
            Term::All(vec![Term::Path(r"d:\poze\".into()), Term::Glob("*.jpg".into())])
        );
        assert_eq!(
            one("poze/IMG_????.jpg"),
            Term::All(vec![Term::Path(r"poze\".into()), Term::Glob("img_????.jpg".into())])
        );
        assert_eq!(
            one(r"path:D:\poze\*.jpg"),
            Term::All(vec![Term::Path(r"d:\poze\".into()), Term::Glob("*.jpg".into())])
        );
        assert_eq!(
            one(r"d:\*\vacanta\*.jpg"),
            Term::All(vec![
                Term::Path(r"d:\".into()),
                Term::Path(r"\vacanta\".into()),
                Term::Glob("*.jpg".into())
            ])
        );
        assert_eq!(one(r"*\*.jpg"), Term::Glob("*.jpg".into()));
        assert_eq!(
            one(r"\poze\*"),
            Term::All(vec![Term::Path(r"\poze\".into()), Term::Glob("*".into())])
        );
        // quoted: literal
        assert_eq!(one(r#"path:"D:\a*b""#), Term::Path(r"d:\a*b".into()));
    }

    #[test]
    fn empty_extensions_are_dropped() {
        assert_eq!(one("ext:pdf;"), Term::Ext(vec!["pdf".into()]));
        assert_eq!(one("ext:;pdf,,docx"), Term::Ext(vec!["pdf".into(), "docx".into()]));
        assert_eq!(one("ext:"), Term::Ext(vec![String::new()]));
    }

    #[test]
    fn negation_next_to_a_bar() {
        // a lone `!` negates the next word
        let c = q("a | ! b");
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(!c[0].negated && c[1].negated);
        assert_eq!(c[1].any_of, vec![Term::Text("b".into())]);
        let c = q("! tmp raport");
        assert!(c[0].negated);
        assert!(!c[1].negated);
        // `a | !b` and `a|!b` cannot be one OR clause: a AND NOT b, never "!b" as text
        for s in ["a | !b", "a|!b"] {
            let c = q(s);
            assert_eq!(c.len(), 2, "{s}: {c:?}");
            assert_eq!(c[0], Clause { negated: false, any_of: vec![Term::Text("a".into())] });
            assert_eq!(c[1], Clause { negated: true, any_of: vec![Term::Text("b".into())] });
        }
        // `!a|b` negates the whole group, as before
        let c = q("!a|b");
        assert_eq!(c.len(), 1);
        assert!(c[0].negated);
        assert_eq!(c[0].any_of.len(), 2);
        // a lone `!` at the end is ignored
        assert_eq!(q("raport !").len(), 1);
    }

    #[test]
    fn dates_parse_through_the_query() {
        assert!(matches!(one("dm:today"), Term::Modified { .. }));
        assert_eq!(one("dm:nicicând"), Term::Text("dm:nicicand".into()));
        assert!(matches!(one("dm:2024.."), Term::Modified { max: u32::MAX, .. }));
        assert!(matches!(one("dm:..2024"), Term::Modified { min: 0, .. }));
        assert_eq!(one("taken:9999"), Term::Text("taken:9999".into()));
        assert_eq!(one("dm:2024-02-31"), Term::Text("dm:2024-02-31".into()));
    }

    #[test]
    fn property_terms() {
        let ctx = DateCtx { now: (day_from_civil(2026, 10, 24) * 1440) as u32, tz_offset: 0 };
        let one = |q: &str| match parse(q, &ctx).clauses.as_slice() {
            [c] => c.any_of[0].clone(),
            other => panic!("{q}: {other:?}"),
        };
        assert_eq!(one("width:>4000"), Term::Prop(Prop::Width { min: 4001, max: u32::MAX }));
        assert_eq!(one("height:1000..2000"), Term::Prop(Prop::Height { min: 1000, max: 2000 }));
        assert_eq!(one("duration:>5m"), Term::Prop(Prop::Duration { min: 360, max: u32::MAX }));
        assert_eq!(one("duration:<=5m"), Term::Prop(Prop::Duration { min: 0, max: 359 }));
        assert_eq!(one("duration:3m30s"), Term::Prop(Prop::Duration { min: 210, max: 210 }));
        assert_eq!(one("duration:5min"), Term::Prop(Prop::Duration { min: 300, max: 359 }));
        assert_eq!(one("duration:1h.."), Term::Prop(Prop::Duration { min: 3600, max: u32::MAX }));
        assert_eq!(one("camera:Canon"), Term::Prop(Prop::Camera("canon".into())));
        assert_eq!(one("artist:\"Ștefan Bănică\""), Term::Prop(Prop::Artist("stefan banica".into())));
        assert!(matches!(one("taken:2025-08"), Term::Prop(Prop::Taken { .. })));
        // not a property: searched as text
        assert_eq!(one("width:wide"), Term::Text("width:wide".into()));
    }
}
