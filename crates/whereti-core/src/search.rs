//! Search (PLAN.md §4.3).
//!
//! 1. Pick the most selective plain word of the query (the "driver").
//! 2. Scan every name for it at once: names sit back to back in one buffer,
//!    so this is one SIMD pass per CPU core, with no per-file work.
//! 3. Check the other conditions only on those candidates.
//! 4. Rank and keep the top N.

use aho_corasick::{AhoCorasick, Input};
use memchr::memmem::Finder;
use rayon::prelude::*;
use whereti_query::{fold, glob_match, longest_literal, Query, Term};

use crate::index::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    /// Exact name > name starts with the word > a word in the name starts
    /// with it > anywhere; then alphabetical.
    #[default]
    Relevance,
    Name,
    /// Largest first.
    Size,
    /// Newest first.
    Modified,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub limit: usize,
    pub sort: SortBy,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { limit: 100, sort: SortBy::Relevance }
    }
}

#[derive(Debug, Default)]
pub struct SearchResults {
    /// Number of matching entries.
    pub total: usize,
    /// The best `limit` matches, in order.
    pub hits: Vec<u32>,
    /// Parts of the query that could not be applied.
    pub warnings: Vec<String>,
}

enum Matcher {
    Text(Box<Finder<'static>>),
    Glob(String),
    Regex(regex::Regex),
    /// Indexed by extension id.
    Ext(Vec<bool>),
    Path(Box<PathMatcher>),
    Size(u64, u64),
    Modified(u32, u32),
    Folders,
    Files,
    Never,
    All(Vec<Matcher>),
}

struct Clause {
    negated: bool,
    any: Vec<Matcher>,
}

/// Scratch buffers reused across entries by one thread.
#[derive(Default)]
struct Scratch {
    name: Vec<u8>,
    path: Vec<u8>,
}

impl NameIndex {
    pub fn search(&self, query: &Query, opts: &SearchOptions) -> SearchResults {
        let mut warnings = Vec::new();
        let mut clauses: Vec<Clause> = query
            .clauses
            .iter()
            .map(|c| Clause {
                negated: c.negated,
                any: c.any_of.iter().map(|t| self.compile(t, &mut warnings)).collect(),
            })
            .collect();
        if clauses.is_empty() || self.is_empty() {
            return SearchResults { warnings, ..Default::default() };
        }

        // Driver: the longest plain word of a clause that is a single positive term.
        let mut driver: Option<(usize, String, bool)> = None; // (clause, needle, clause fully covered)
        for (ci, c) in query.clauses.iter().enumerate() {
            if c.negated || c.any_of.len() != 1 {
                continue;
            }
            let (needle, covered) = match &c.any_of[0] {
                Term::Text(t) => (t.clone(), true),
                Term::Glob(g) => (longest_literal(g).to_string(), false),
                _ => continue,
            };
            if !needle.is_empty() && driver.as_ref().is_none_or(|d| needle.len() > d.1.len()) {
                driver = Some((ci, needle, covered));
            }
        }
        let rank_needle = query.clauses.iter().find_map(|c| match (&c.negated, c.any_of.as_slice()) {
            (false, [Term::Text(t)]) => Some(t.clone()),
            _ => None,
        });

        let candidates: Option<Vec<u32>> = driver.as_ref().map(|(ci, needle, covered)| {
            if *covered {
                clauses.remove(*ci);
            }
            self.scan_names(needle)
        });

        let hits: Vec<u32> = match &candidates {
            Some(c) if clauses.is_empty() => c.clone(),
            Some(c) => c
                .par_chunks(8192)
                .map(|chunk| {
                    let mut s = Scratch::default();
                    chunk.iter().copied().filter(|&i| self.eval(&clauses, i, &mut s)).collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
                .concat(),
            None => (0..self.len() as u32)
                .into_par_iter()
                .with_min_len(8192)
                .map_init(Scratch::default, |s, i| {
                    (self.flags[i as usize] & HIDDEN_FROM_SEARCH == 0 && self.eval(&clauses, i, s)).then_some(i)
                })
                .flatten_iter()
                .collect(),
        };

        let total = hits.len();
        let hits = self.top(hits, opts, rank_needle.as_deref());
        SearchResults { total, hits, warnings }
    }

    fn compile(&self, term: &Term, warnings: &mut Vec<String>) -> Matcher {
        match term {
            Term::Text(t) => Matcher::Text(Box::new(Finder::new(t.as_bytes()).into_owned())),
            Term::Glob(g) => Matcher::Glob(g.clone()),
            Term::Regex(r) => match regex::RegexBuilder::new(r).case_insensitive(true).build() {
                Ok(re) => Matcher::Regex(re),
                Err(_) => {
                    warnings.push(format!("invalid regex \"{r}\" — searched as plain text"));
                    Matcher::Text(Box::new(Finder::new(fold(r).as_bytes()).into_owned()))
                }
            },
            Term::Ext(list) => {
                let mut table = vec![false; self.exts.len()];
                for (id, e) in self.exts.iter().enumerate() {
                    table[id] = list.iter().any(|x| x == e);
                }
                Matcher::Ext(table)
            }
            Term::Path(p) => Matcher::Path(Box::new(PathMatcher::new(self, p))),
            Term::Size { min, max } => Matcher::Size(*min, *max),
            Term::Modified { min, max } => Matcher::Modified(*min, *max),
            Term::FoldersOnly => Matcher::Folders,
            Term::FilesOnly => Matcher::Files,
            Term::Content(_) => {
                warnings.push("content search is not available yet (coming with the content index)".into());
                Matcher::Never
            }
            Term::All(ts) => Matcher::All(ts.iter().map(|t| self.compile(t, warnings)).collect()),
        }
    }

    fn eval(&self, clauses: &[Clause], i: u32, s: &mut Scratch) -> bool {
        clauses
            .iter()
            .all(|c| c.any.iter().any(|m| self.matches(m, i, s)) != c.negated)
    }

    fn matches(&self, m: &Matcher, i: u32, s: &mut Scratch) -> bool {
        let iu = i as usize;
        match m {
            Matcher::Text(f) => f.find(self.folded_name(i, &mut s.name)).is_some(),
            Matcher::Glob(g) => {
                let name = self.folded_name(i, &mut s.name);
                glob_match(g, std::str::from_utf8(name).unwrap_or(""))
            }
            Matcher::Regex(re) => re.is_match(self.name(i)),
            Matcher::Ext(table) => table.get(self.ext[iu] as usize).copied().unwrap_or(false),
            Matcher::Path(p) => {
                let Scratch { name, path } = s;
                let folded = self.folded_name(i, name);
                p.matches(self, i, folded, path)
            }
            Matcher::Size(lo, hi) => !self.is_dir(i) && (*lo..=*hi).contains(&self.size[iu]),
            Matcher::Modified(lo, hi) => (*lo..=*hi).contains(&self.mtime[iu]),
            Matcher::Folders => self.is_dir(i),
            Matcher::Files => !self.is_dir(i),
            Matcher::Never => false,
            Matcher::All(ms) => ms.iter().all(|m| self.matches(m, i, s)),
        }
    }

    /// Folded name: the `alt` copy for non-ASCII names, lowercase on the fly otherwise.
    pub(crate) fn folded_name<'a>(&'a self, i: u32, scratch: &'a mut Vec<u8>) -> &'a [u8] {
        let f = self.flags[i as usize];
        if f & FLAG_TAIL != 0 {
            let mut folded = String::new();
            whereti_query::fold_into(self.tail_name(i), &mut folded);
            scratch.clear();
            scratch.extend_from_slice(folded.as_bytes());
            scratch
        } else if f & FLAG_NON_ASCII != 0 {
            self.alt_name(i)
        } else {
            scratch.clear();
            scratch.extend(self.name_bytes(i).iter().map(u8::to_ascii_lowercase));
            scratch
        }
    }

    /// Every entry whose folded name contains `needle`, in index order.
    fn scan_names(&self, needle: &str) -> Vec<u32> {
        // Entries added by live updates have no base name; they are in the tail.
        let n = self.name_off.len() - 1;
        const SKIP_BASE: u16 = FLAG_NON_ASCII | FLAG_TAIL | HIDDEN_FROM_SEARCH;
        const SKIP_ALT: u16 = FLAG_TAIL | HIDDEN_FROM_SEARCH;
        let parts = (rayon::current_num_threads() * 8).max(1);
        let per = n.div_ceil(parts).max(1);
        let ranges: Vec<(usize, usize)> = (0..n).step_by(per).map(|a| (a, (a + per).min(n))).collect();

        let mut primary: Vec<u32> = Vec::new();
        if needle.is_ascii() {
            let ac = AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle])
                .expect("a single literal always builds");
            primary = ranges
                .par_iter()
                .map(|&(a, b)| {
                    let mut out = Vec::new();
                    let end = self.name_off[b] as usize;
                    let mut pos = self.name_off[a] as usize;
                    let mut e = a;
                    while pos < end {
                        let Some(m) = ac.find(Input::new(&self.names).range(pos..end)) else { break };
                        while self.name_off[e + 1] as usize <= m.start() {
                            e += 1;
                        }
                        if self.flags[e] & SKIP_BASE == 0 {
                            out.push(e as u32);
                        }
                        pos = self.name_off[e + 1] as usize; // next name
                    }
                    out
                })
                .collect::<Vec<_>>()
                .concat();
        }

        // Non-ASCII names: search their folded copies.
        let finder = Finder::new(needle.as_bytes());
        let alt: Vec<u32> = if self.alt_entry.is_empty() {
            Vec::new()
        } else {
            let m = self.alt_entry.len();
            let per = m.div_ceil(parts).max(1);
            (0..m)
                .step_by(per)
                .collect::<Vec<_>>()
                .par_iter()
                .map(|&a| {
                    let b = (a + per).min(m);
                    let mut out = Vec::new();
                    let end = self.alt_off[b] as usize;
                    let mut pos = self.alt_off[a] as usize;
                    let mut k = a;
                    while pos < end {
                        let Some(off) = finder.find(&self.alt_names[pos..end]) else { break };
                        let at = pos + off;
                        while self.alt_off[k + 1] as usize <= at {
                            k += 1;
                        }
                        let e = self.alt_entry[k];
                        if self.flags[e as usize] & SKIP_ALT == 0 {
                            out.push(e);
                        }
                        pos = self.alt_off[k + 1] as usize;
                    }
                    out
                })
                .collect::<Vec<_>>()
                .concat()
        };
        let mut hits = merge_sorted(primary, alt);

        // Names renamed or added by live updates (a small list).
        if !self.tail_entry.is_empty() {
            let mut folded = String::new();
            let mut extra: Vec<u32> = Vec::new();
            for (k, &e) in self.tail_entry.iter().enumerate() {
                if self.tail_of.get(&e) != Some(&(k as u32))
                    || self.flags[e as usize] & HIDDEN_FROM_SEARCH != 0
                {
                    continue;
                }
                folded.clear();
                whereti_query::fold_into(&self.tail_names[k], &mut folded);
                if finder.find(folded.as_bytes()).is_some() {
                    extra.push(e);
                }
            }
            extra.sort_unstable();
            hits = merge_sorted(hits, extra);
        }
        hits
    }

    /// Ranks all hits and returns the best `opts.limit`, in order.
    fn top(&self, hits: Vec<u32>, opts: &SearchOptions, needle: Option<&str>) -> Vec<u32> {
        let limit = opts.limit.min(hits.len());
        if limit == 0 {
            return Vec::new();
        }
        let needle = needle.map(str::as_bytes);
        let mut keyed: Vec<(u64, u32, u32)> = hits
            .par_iter()
            .with_min_len(4096)
            .map_init(Vec::new, |scratch, &i| {
                let primary = match opts.sort {
                    SortBy::Relevance => self.relevance(i, needle, scratch),
                    SortBy::Name => 0,
                    SortBy::Size => u64::MAX - self.size[i as usize],
                    SortBy::Modified => (u32::MAX - self.mtime[i as usize]) as u64,
                };
                (primary, self.name_rank[i as usize], i)
            })
            .collect();
        if keyed.len() > limit {
            keyed.select_nth_unstable(limit - 1);
            keyed.truncate(limit);
        }
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, _, i)| i).collect()
    }

    fn relevance(&self, i: u32, needle: Option<&[u8]>, scratch: &mut Vec<u8>) -> u64 {
        let f = self.flags[i as usize];
        let hidden = if f & (FLAG_HIDDEN | FLAG_SYSTEM) != 0 { 4 } else { 0 };
        let noise = if f & FLAG_NOISE != 0 { 8 } else { 0 };
        let penalty = hidden + noise;
        let Some(needle) = needle else { return penalty };
        let name = self.folded_name(i, scratch);
        let score = if name == needle {
            0
        } else if name.starts_with(needle) {
            1
        } else if memchr::memmem::find_iter(name, needle).any(|p| !name[p - 1].is_ascii_alphanumeric()) {
            2
        } else {
            3
        };
        score + penalty
    }
}

/// `path:` matching without building paths (PLAN.md §4.3).
///
/// For every folder (parents first) we keep only whether its path already
/// matched and the last `k = len(pattern) - 1` bytes of its path. A match
/// either lies inside the parent's path or crosses into this name, and the
/// crossing part is at most `k` bytes of the parent's path.
struct PathMatcher {
    finder: Finder<'static>,
    k: usize,
    slot: Vec<u32>,
    hit: Vec<bool>,
    tails: Vec<u8>,
    tail_len: Vec<u16>,
}

impl PathMatcher {
    fn new(idx: &NameIndex, pattern: &str) -> Self {
        let k = pattern.len().saturating_sub(1).min(u16::MAX as usize);
        let dirs = idx.dir_order.len();
        let mut pm = PathMatcher {
            finder: Finder::new(pattern.as_bytes()).into_owned(),
            k,
            slot: vec![u32::MAX; idx.len()],
            hit: vec![false; dirs],
            tails: vec![0; dirs * k],
            tail_len: vec![0; dirs],
        };
        let mut buf = Vec::new();
        let mut name = Vec::new();
        for (s, &d) in idx.dir_order.iter().enumerate() {
            pm.slot[d as usize] = s as u32;
            buf.clear();
            let mut parent_hit = false;
            if idx.flags[d as usize] & FLAG_ROOT != 0 {
                buf.extend(fold(&idx.volume_of(d).letter().to_string()).bytes());
                buf.push(b':');
            } else {
                if let Some(ps) = idx.parent(d).map(|p| pm.slot[p as usize]).filter(|&ps| ps != u32::MAX) {
                    buf.extend_from_slice(pm.tail(ps));
                    parent_hit = pm.hit[ps as usize];
                }
                buf.push(b'\\');
                buf.extend_from_slice(idx.folded_name(d, &mut name));
            }
            pm.hit[s] = parent_hit || pm.finder.find(&buf).is_some();
            let t = &buf[buf.len().saturating_sub(k)..];
            pm.tails[s * k..s * k + t.len()].copy_from_slice(t);
            pm.tail_len[s] = t.len() as u16;
        }
        pm
    }

    fn tail(&self, s: u32) -> &[u8] {
        let s = s as usize;
        &self.tails[s * self.k..s * self.k + self.tail_len[s] as usize]
    }

    fn matches(&self, idx: &NameIndex, i: u32, folded_name: &[u8], buf: &mut Vec<u8>) -> bool {
        if idx.is_dir(i) {
            let s = self.slot[i as usize];
            return s != u32::MAX && self.hit[s as usize];
        }
        let Some(ps) = idx.parent(i).map(|p| self.slot[p as usize]).filter(|&s| s != u32::MAX) else {
            return false;
        };
        if self.hit[ps as usize] {
            return true;
        }
        buf.clear();
        buf.extend_from_slice(self.tail(ps));
        buf.push(b'\\');
        buf.extend_from_slice(folded_name);
        self.finder.find(buf).is_some()
    }
}

fn merge_sorted(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] <= b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out.dedup();
    out
}
