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

use std::sync::Arc;

use crate::index::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    /// Exact name > name starts with the word > a word in the name starts
    /// with it > anywhere; inside each, the more important first (see
    /// `importance`), then alphabetical.
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
    Mine,
    Never,
    All(Vec<Matcher>),
}

struct Clause {
    negated: bool,
    any: Vec<Matcher>,
}

/// Scratch buffer reused across entries by one thread.
#[derive(Default)]
struct Scratch {
    name: Vec<u8>,
}

impl NameIndex {
    pub fn search(&self, query: &Query, opts: &SearchOptions) -> SearchResults {
        self.search_with_usage(query, opts, &[])
    }

    /// Like `search`, with the files this user opens often or lately
    /// (`(full path, weight 1-12)`, from the app): those of them among the
    /// hits rank as if they were more important, and are never sent down
    /// as noise.
    pub fn search_with_usage(&self, query: &Query, opts: &SearchOptions, usage: &[(String, u8)]) -> SearchResults {
        self.search_ranked(query, opts, usage, Arc::new(Vec::new()))
    }

    /// Like `search_with_usage`, and `active`: per entry, how much the user
    /// works in that folder lately (0-8, indexed by folder entry; see
    /// `importance`). Empty: no such signal.
    pub fn search_ranked(&self, query: &Query, opts: &SearchOptions, usage: &[(String, u8)], active: Arc<Vec<u8>>) -> SearchResults {
        let t0 = std::time::Instant::now();
        let mut warnings = Vec::new();
        let hits = self.matching_entries(query, &mut warnings);
        let t_eval = t0.elapsed();
        let rank_needle = query.clauses.iter().find_map(|c| match (&c.negated, c.any_of.as_slice()) {
            (false, [Term::Text(t)]) => Some(t.clone()),
            _ => None,
        });
        let total = hits.len();
        let used = if opts.sort == SortBy::Relevance { self.usage_positions(&hits, usage) } else { Vec::new() };
        let hits = self.top(hits, opts, rank_needle.as_deref(), &used, active);
        if trace_enabled() {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!("[trace] match {:.2} ms · rank {:.2} ms · {} hits", ms(t_eval), ms(t0.elapsed() - t_eval), total);
        }
        SearchResults { total, hits, warnings }
    }

    /// Every live entry matching `query`, unranked, in index order.
    pub fn matching(&self, query: &Query) -> Vec<u32> {
        self.matching_entries(query, &mut Vec::new())
    }

    /// The entries of `entries` that are live and match `query`, in the given order.
    pub fn filter(&self, query: &Query, entries: &[u32]) -> Vec<u32> {
        let mut warnings = Vec::new();
        let clauses = self.compile_clauses(query, &mut warnings);
        let mut s = Scratch::default();
        entries
            .iter()
            .copied()
            .filter(|&i| {
                (i as usize) < self.len() && self.flags[i as usize] & HIDDEN_FROM_SEARCH == 0 && self.eval(&clauses, i, &mut s)
            })
            .collect()
    }

    fn compile_clauses(&self, query: &Query, warnings: &mut Vec<String>) -> Vec<Clause> {
        query
            .clauses
            .iter()
            .map(|c| Clause { negated: c.negated, any: c.any_of.iter().map(|t| self.compile(t, warnings)).collect() })
            .collect()
    }

    fn matching_entries(&self, query: &Query, warnings: &mut Vec<String>) -> Vec<u32> {
        let t0 = std::time::Instant::now();
        let mut clauses = self.compile_clauses(query, warnings);
        if clauses.is_empty() || self.is_empty() {
            return Vec::new();
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
        let t_compile = t0.elapsed();
        let candidates: Option<Vec<u32>> = driver.as_ref().map(|(ci, needle, covered)| {
            if *covered {
                clauses.remove(*ci);
            }
            self.scan_names(needle)
        });

        let t_scan = t0.elapsed();
        let candidate_count = candidates.as_ref().map_or(0, Vec::len);
        let hits: Vec<u32> = match candidates {
            Some(c) if clauses.is_empty() => c,
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

        if trace_enabled() {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "[trace] compile {:.2} ms · scan {:.2} ms ({} candidates) · filter {:.2} ms · {} hits",
                ms(t_compile),
                ms(t_scan - t_compile),
                candidate_count,
                ms(t0.elapsed() - t_scan),
                hits.len()
            );
        }
        hits
    }

    fn compile(&self, term: &Term, warnings: &mut Vec<String>) -> Matcher {
        match term {
            Term::Text(t) => Matcher::Text(Box::new(Finder::new(t.as_bytes()).into_owned())),
            Term::Glob(g) => Matcher::Glob(g.clone()),
            Term::Regex(r) => match regex::RegexBuilder::new(r).case_insensitive(true).build() {
                Ok(re) => Matcher::Regex(re),
                Err(_) => {
                    warnings.push(format!("invalid regex \"{r}\" - searched as plain text"));
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
            Term::Mine => Matcher::Mine,
            Term::FilesOnly => Matcher::Files,
            // Answered by the content index, never by names.
            Term::Content(_) | Term::Place(_) => Matcher::Never,
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
            Matcher::Path(p) => p.matches(self, i, &mut s.name),
            Matcher::Size(lo, hi) => !self.is_dir(i) && (*lo..=*hi).contains(&self.size[iu]),
            Matcher::Modified(lo, hi) => (*lo..=*hi).contains(&self.mtime[iu]),
            Matcher::Folders => self.is_dir(i),
            Matcher::Files => !self.is_dir(i),
            Matcher::Mine => self.flags[iu] & (FLAG_NOISE | FLAG_HIDDEN | FLAG_SYSTEM) == 0,
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
    /// Where the used files are among `hits`, with their weight.
    fn usage_positions(&self, hits: &[u32], usage: &[(String, u8)]) -> Vec<(usize, u8)> {
        let wanted: Vec<(&[u8], &str, u8)> = usage
            .iter()
            .filter_map(|(path, w)| Some((path.rsplit('\\').next()?.as_bytes(), path.as_str(), *w)))
            .filter(|(name, _, _)| !name.is_empty() && name.len() < 128)
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        // most hits are ruled out by the length of their name alone
        let lengths = wanted.iter().fold(0u128, |m, (name, _, _)| m | 1u128 << name.len());
        hits.par_iter()
            .enumerate()
            .with_min_len(8192)
            .filter_map(|(pos, &i)| {
                let name = self.name_bytes(i);
                if name.len() >= 128 || lengths & (1u128 << name.len()) == 0 {
                    return None;
                }
                let (_, _, w) = wanted.iter().find(|(n, path, _)| {
                    n.eq_ignore_ascii_case(name) && self.full_path(i).eq_ignore_ascii_case(path)
                })?;
                Some((pos, *w))
            })
            .collect()
    }

    /// `used`: positions in `hits` of files the user opens often or lately,
    /// with their weight (Relevance only).
    fn top(&self, hits: Vec<u32>, opts: &SearchOptions, needle: Option<&str>, used: &[(usize, u8)], active: Arc<Vec<u8>>) -> Vec<u32> {
        let limit = opts.limit.min(hits.len());
        if limit == 0 {
            return Vec::new();
        }
        let rank = |i: u32| self.name_rank[i as usize];
        let mut keyed: Vec<(u64, u32, u32)> = match opts.sort {
            SortBy::Relevance | SortBy::Name => {
                // The order is (score, importance, alphabetical rank). Put
                // every hit in a bucket by score, importance and the top bits
                // of its rank, count the buckets, and fully order only the
                // hits in the buckets that can reach the top `limit` -
                // thousands, not millions.
                let needle = needle.map(str::as_bytes);
                let relevance = opts.sort == SortBy::Relevance;
                let weigh = relevance.then(|| self.weigher_with(active));
                let shift = (32 - (self.len().max(1) as u32).leading_zeros()).saturating_sub(7);
                let bucket = |i: u32| -> u16 {
                    let key = match &weigh {
                        Some(w) => (self.relevance(i, needle) as u16) << 5 | self.importance(i, w) as u16,
                        None => 0,
                    };
                    (key << 7) | (rank(i) >> shift).min(127) as u16
                };
                let mut buckets: Vec<u16> = hits.par_iter().with_min_len(8192).map(|&i| bucket(i)).collect();
                if let Some(w) = &weigh {
                    for &(pos, weight) in used {
                        // a used file: no noise or hidden penalty, up one
                        // match level per 4 of weight (a file opened
                        // yesterday beats an old one that only starts with
                        // the word), and more important by its weight
                        let i = hits[pos];
                        let level = (self.relevance(i, needle) & 3).saturating_sub(weight / 4) as u16;
                        let imp = self.importance(i, w).saturating_sub(weight) as u16;
                        buckets[pos] = (level << 5 | imp) << 7 | (rank(i) >> shift).min(127) as u16;
                    }
                }
                let count = buckets
                    .par_chunks(1 << 16)
                    .map(|c| {
                        let mut h = vec![0u32; 1 << 16];
                        for &b in c {
                            h[b as usize] += 1;
                        }
                        h
                    })
                    .reduce(|| vec![0u32; 1 << 16], |mut a, b| {
                        a.iter_mut().zip(b).for_each(|(x, y)| *x += y);
                        a
                    });
                let mut cutoff = u16::MAX;
                let mut acc = 0usize;
                for (b, &c) in count.iter().enumerate() {
                    acc += c as usize;
                    if acc >= limit {
                        cutoff = b as u16;
                        break;
                    }
                }
                hits.iter()
                    .zip(&buckets)
                    .filter(|(_, &b)| b <= cutoff)
                    .map(|(&i, &b)| ((b >> 7) as u64, rank(i), i))
                    .collect()
            }
            SortBy::Size | SortBy::Modified => {
                // Largest / newest first. Find the key of the limit-th hit on a
                // compact list of keys, then order only the hits up to it.
                let key = |i: u32| match opts.sort {
                    SortBy::Size => u64::MAX - self.size[i as usize],
                    _ => (u32::MAX - self.mtime[i as usize]) as u64,
                };
                let keys: Vec<u64> = hits.par_iter().with_min_len(8192).map(|&i| key(i)).collect();
                let threshold = if keys.len() > limit {
                    let mut tmp = keys.clone();
                    *tmp.select_nth_unstable(limit - 1).1
                } else {
                    u64::MAX
                };
                hits.iter()
                    .zip(&keys)
                    .filter(|(_, &k)| k <= threshold)
                    .map(|(&i, &k)| (k, rank(i), i))
                    .collect()
            }
        };
        if keyed.len() > limit {
            keyed.select_nth_unstable(limit - 1);
            keyed.truncate(limit);
        }
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, _, i)| i).collect()
    }

    /// What `importance` needs once per search.
    pub fn weigher(&self) -> Weigher {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_secs() + 11_644_473_600) / 60)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;
        let ext = self.exts.iter().map(|e| crate::build::ext_weight(e)).collect();
        Weigher { now, ext, active: Arc::new(Vec::new()) }
    }

    /// `weigher`, with where the user works lately (see `search_ranked`).
    pub fn weigher_with(&self, active: Arc<Vec<u8>>) -> Weigher {
        Weigher { active, ..self.weigher() }
    }

    /// A file of the user's own kind, where they work: not a folder, not
    /// hidden, system, cloud-only, gone or in a tool folder; of a type a
    /// person makes (documents, pictures, music, video, archives, text and
    /// code), never an unknown extension (a program's own data, like a
    /// vault's ".sslo" blocks); and not with a made-up name (a UUID).
    pub fn is_work_file(&self, i: u32) -> bool {
        let iu = i as usize;
        let skip = FLAG_DIR | FLAG_HIDDEN | FLAG_SYSTEM | FLAG_CLOUD | FLAG_NOISE | FLAG_GONE | FLAG_ROOT;
        if iu >= self.len() || self.flags[iu] & skip != 0 {
            return false;
        }
        let e = self.ext[iu] as usize;
        let known = e != 0
            && self.exts.get(e).is_some_and(|x| crate::build::ext_weight(x) < 0 || crate::build::is_written_by_hand(x));
        known && !crate::build::looks_generated(self.name(i))
    }

    /// 0 (most) to 31 (least): how much an entry likely matters to the
    /// person, without knowing anything about them. The user's own folders,
    /// their kind of files (documents, pictures, music, video, programs) and
    /// recent changes come first; files that programs keep for themselves
    /// last. Ranks inside one match level only: a better match always wins.
    pub fn importance(&self, i: u32, w: &Weigher) -> u8 {
        let iu = i as usize;
        let f = self.flags[iu];
        let mut score: i32 = 16;
        if f & FLAG_HOME != 0 {
            score -= 6;
        }
        if f & FLAG_DIR != 0 {
            score -= 1;
        } else {
            let e = self.ext[iu] as usize;
            score += if e == 0 { 3 } else { w.ext.get(e).copied().unwrap_or(0) as i32 };
        }
        // where the user works lately: the folder itself, what is in it,
        // and (half as much) one level further down
        if !w.active.is_empty() {
            let at = |e: u32| w.active.get(e as usize).copied().unwrap_or(0);
            let parent = self.parent(i);
            let here = at(i).max(parent.map_or(0, at));
            let above = parent.and_then(|p| self.parent(p)).map_or(0, at) / 2;
            score -= here.max(above) as i32;
        }
        const DAY: u32 = 24 * 60;
        score -= match w.now.saturating_sub(self.mtime[iu]) {
            a if a < DAY => 4,
            a if a < 7 * DAY => 3,
            a if a < 30 * DAY => 2,
            a if a < 365 * DAY => 1,
            _ => 0,
        };
        score.clamp(0, 31) as u8
    }

    /// 0 = exact name (with or without its extension), 1 = name starts with the word, 2 = a word in the name
    /// starts with it, 3 = anywhere; +4 hidden/system, +8 cache/build folder.
    /// Compares in place (no copy): the needle is folded, ASCII names are
    /// compared ignoring case, other names through their folded copy.
    fn relevance(&self, i: u32, needle: Option<&[u8]>) -> u8 {
        let f = self.flags[i as usize];
        let hidden = if f & (FLAG_HIDDEN | FLAG_SYSTEM) != 0 { 4 } else { 0 };
        let noise = if f & FLAG_NOISE != 0 { 8 } else { 0 };
        let penalty = hidden + noise;
        let Some(needle) = needle else { return penalty };
        let mut scratch = Vec::new();
        let name: &[u8] = if f & FLAG_TAIL != 0 {
            self.folded_name(i, &mut scratch)
        } else if f & FLAG_NON_ASCII != 0 {
            self.alt_name(i)
        } else {
            self.base_name(i)
        };
        let n = needle.len();
        let score = if name.eq_ignore_ascii_case(needle) {
            0
        } else if name.len() >= n && name[..n].eq_ignore_ascii_case(needle) {
            // "readme" is README.md exactly: the name without its extension
            if name.get(n) == Some(&b'.') && !name[n + 1..].contains(&b'.') {
                0
            } else {
                1
            }
        } else if (1..name.len().saturating_sub(n) + 1)
            .any(|p| !name[p - 1].is_ascii_alphanumeric() && name[p..p + n].eq_ignore_ascii_case(needle))
        {
            2
        } else {
            3
        };
        score + penalty
    }
}

pub struct Weigher {
    /// Minutes since 1601, like `mtime`.
    now: u32,
    /// `ext_weight` of every extension id.
    ext: Vec<i8>,
    /// Per folder entry: how much the user works there lately (0-8).
    active: Arc<Vec<u8>>,
}

/// Set `WT_TRACE=1` to print how long each search phase takes.
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WT_TRACE").is_some())
}

/// `path:` matching without building paths (PLAN.md §4.3).
///
/// For every folder (parents first) we keep only whether its path already
/// matched and the last `k = len(pattern) - 1` bytes of its path. A match
/// either lies inside the parent's path or crosses into this name, and the
/// crossing part is at most `k` bytes of the parent's path.
///
/// Folders of one tree level depend only on the level above, so each level is
/// computed in parallel. Files need no string work at all: a file matches if
/// its folder matched, or (pattern `head\last`) if its folder's path ends with
/// `head` and its name starts with `last`.
struct PathMatcher {
    finder: Finder<'static>,
    /// Pattern before its last `\`, and after it. `head` is None without a `\`.
    head: Option<Vec<u8>>,
    last: Vec<u8>,
    slot: Vec<u32>,
    hit: Vec<bool>,
    ends_head: Vec<bool>,
}

impl PathMatcher {
    fn new(idx: &NameIndex, pattern: &str) -> Self {
        let p = pattern.as_bytes();
        // bytes of each folder's path kept while walking the tree
        let stride = p.len().saturating_sub(1).clamp(1, u16::MAX as usize);
        let (head, last) = match p.iter().rposition(|&b| b == b'\\') {
            Some(at) => (Some(p[..at].to_vec()), p[at + 1..].to_vec()),
            None => (None, p.to_vec()),
        };
        let order = &idx.dir_order;
        let dirs = order.len();
        let mut slot = vec![u32::MAX; idx.len()];
        for (s, &d) in order.iter().enumerate() {
            slot[d as usize] = s as u32;
        }
        // Level boundaries: a new level starts at the first folder whose parent
        // is in the current level, so every parent is in an earlier level.
        let mut levels = vec![0usize];
        for (s, &d) in order.iter().enumerate() {
            let ps = idx.parent(d).map_or(u32::MAX, |p| slot[p as usize]);
            if ps != u32::MAX && ps as usize >= *levels.last().unwrap() {
                levels.push(s);
            }
        }
        levels.push(dirs);

        let finder = Finder::new(p).into_owned();
        let mut hit = vec![false; dirs];
        let mut ends_head = vec![false; dirs];
        let mut tails = vec![0u8; dirs * stride];
        let mut tail_len = vec![0u16; dirs];
        for w in levels.windows(2) {
            let (lo, hi) = (w[0], w[1]);
            if lo == hi {
                continue;
            }
            let (done_hit, cur_hit) = hit.split_at_mut(lo);
            let (_, cur_ends) = ends_head.split_at_mut(lo);
            let (done_tails, cur_tails) = tails.split_at_mut(lo * stride);
            let (done_len, cur_len) = tail_len.split_at_mut(lo);
            let (done_hit, done_tails, done_len) = (&*done_hit, &*done_tails, &*done_len);
            cur_tails[..(hi - lo) * stride]
                .par_chunks_mut(stride)
                .zip(cur_hit[..hi - lo].par_iter_mut())
                .zip(cur_ends[..hi - lo].par_iter_mut())
                .zip(cur_len[..hi - lo].par_iter_mut())
                .enumerate()
                .with_min_len(256)
                .for_each_init(
                    || (Vec::new(), Vec::new()),
                    |(buf, name), (j, (((tail, h), e), len))| {
                        let d = order[lo + j];
                        buf.clear();
                        let mut parent_hit = false;
                        if idx.flags[d as usize] & FLAG_ROOT != 0 {
                            buf.extend(fold(&idx.volume_of(d).letter().to_string()).bytes());
                            buf.push(b':');
                        } else {
                            let ps = idx.parent(d).map_or(u32::MAX, |p| slot[p as usize]);
                            if ps != u32::MAX {
                                let ps = ps as usize;
                                buf.extend_from_slice(&done_tails[ps * stride..ps * stride + done_len[ps] as usize]);
                                parent_hit = done_hit[ps];
                            }
                            buf.push(b'\\');
                            buf.extend_from_slice(idx.folded_name(d, name));
                        }
                        *h = parent_hit || finder.find(buf).is_some();
                        *e = head.as_ref().is_some_and(|hd| buf.ends_with(hd));
                        let t = &buf[buf.len().saturating_sub(stride)..];
                        tail[..t.len()].copy_from_slice(t);
                        *len = t.len() as u16;
                    },
                );
        }
        PathMatcher { finder, head, last, slot, hit, ends_head }
    }

    fn matches(&self, idx: &NameIndex, i: u32, scratch: &mut Vec<u8>) -> bool {
        if idx.is_dir(i) {
            let s = self.slot[i as usize];
            return s != u32::MAX && self.hit[s as usize];
        }
        let Some(ps) = idx.parent(i).map(|p| self.slot[p as usize]).filter(|&s| s != u32::MAX) else {
            return false;
        };
        let ps = ps as usize;
        if self.hit[ps] {
            return true;
        }
        match &self.head {
            // the match crosses from the folder's path into the file name
            Some(_) => self.ends_head[ps] && idx.folded_name(i, scratch).starts_with(&self.last),
            // no `\` in the pattern: it is inside the folder path or inside the name
            None => self.finder.find(idx.folded_name(i, scratch)).is_some(),
        }
    }
}

#[cfg(test)]
impl PathMatcher {
    /// Reference implementation: build the full folded path and search it.
    fn slow_matches(idx: &NameIndex, i: u32, pattern: &str) -> bool {
        let path = fold(&idx.full_path(i));
        path.contains(pattern)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_live::{fresh, Model};

    /// An index that went through random live updates: renamed names in the
    /// tail list, folders appended to `dir_order`, detached subtrees.
    fn churned(seed: u64, batches: usize) -> NameIndex {
        let mut m = Model::new(seed);
        let mut idx = fresh(&m);
        for _ in 0..batches {
            for _ in 0..1 + m.rand(6) {
                m.step();
            }
            let changed = std::mem::take(&mut m.changed);
            let created = std::mem::take(&mut m.created);
            if idx.apply(0, &changed, &created, &mut m).unwrap().needs_rescan {
                idx = fresh(&m);
            }
        }
        idx
    }

    fn searchable(idx: &NameIndex) -> Vec<u32> {
        (0..idx.len() as u32).filter(|&i| idx.flags[i as usize] & HIDDEN_FROM_SEARCH == 0).collect()
    }

    #[test]
    fn fast_path_matching_equals_building_every_path() {
        let mut checked = 0;
        for seed in 1..=25 {
            let mut m = Model::new(seed * 7919);
            let idx = churned(seed, 60);
            let live = searchable(&idx);
            for _ in 0..40 {
                // a random piece of a real path, often crossing folder boundaries
                let e = live[m.rand(live.len())];
                let path: Vec<char> = fold(&idx.full_path(e)).chars().collect();
                let a = m.rand(path.len());
                let b = (a + 1 + m.rand(24)).min(path.len());
                let pattern: String = path[a..b].iter().collect();
                let pm = PathMatcher::new(&idx, &pattern);
                let mut scratch = Vec::new();
                for &i in &live {
                    assert_eq!(
                        pm.matches(&idx, i, &mut scratch),
                        PathMatcher::slow_matches(&idx, i, &pattern),
                        "seed {seed}, pattern {pattern:?}, entry {}",
                        idx.full_path(i)
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 5_000, "only {checked} checks");
    }

    /// The scoring as first written: fold a copy of the name, then compare.
    fn reference_relevance(idx: &NameIndex, i: u32, needle: &[u8]) -> u8 {
        let f = idx.flags[i as usize];
        let penalty = if f & (FLAG_HIDDEN | FLAG_SYSTEM) != 0 { 4 } else { 0 } + if f & FLAG_NOISE != 0 { 8 } else { 0 };
        let mut scratch = Vec::new();
        let name = idx.folded_name(i, &mut scratch).to_vec();
        let n = needle.len();
        let stem = name.starts_with(needle) && name.get(n) == Some(&b'.') && !name[n + 1..].contains(&b'.');
        let score = if name == needle || stem {
            0
        } else if name.starts_with(needle) {
            1
        } else if memchr::memmem::find_iter(&name, needle).any(|p| p > 0 && !name[p - 1].is_ascii_alphanumeric()) {
            2
        } else {
            3
        };
        score + penalty
    }

    #[test]
    fn top_n_equals_a_full_sort() {
        for seed in 1..=10 {
            let idx = churned(seed, 120);
            for needle in ["a", "ra", "raport", "masina", "data", ".txt", "(1)", "s"] {
                let hits = idx.scan_names(needle);
                let w = idx.weigher();
                let mut all: Vec<(u64, u32, u32)> = hits
                    .iter()
                    .map(|&i| {
                        let sc = idx.relevance(i, Some(needle.as_bytes()));
                        assert_eq!(sc, reference_relevance(&idx, i, needle.as_bytes()), "score of {}", idx.name(i));
                        ((sc as u64) << 5 | idx.importance(i, &w) as u64, idx.name_rank[i as usize], i)
                    })
                    .collect();
                all.sort_unstable();
                for limit in [1, 2, 3, 7, 50, 100_000] {
                    let want: Vec<u32> = all.iter().take(limit).map(|k| k.2).collect();
                    let opts = SearchOptions { limit, sort: SortBy::Relevance };
                    assert_eq!(idx.top(hits.clone(), &opts, Some(needle), &[], Arc::new(Vec::new())), want, "seed {seed}, {needle:?}, limit {limit}");
                }
            }
            // no word to rank by (e.g. a path or ext: query), and alphabetical order
            let hits = searchable(&idx);
            for (sort, needle) in [
                (SortBy::Relevance, None),
                (SortBy::Name, None),
                (SortBy::Name, Some("a")),
                (SortBy::Size, None),
                (SortBy::Modified, None),
            ] {
                let mut all: Vec<(u64, u32, u32)> = hits
                    .iter()
                    .map(|&i| {
                        let k = match sort {
                            SortBy::Relevance => (idx.relevance(i, None) as u64) << 5 | idx.importance(i, &idx.weigher()) as u64,
                            SortBy::Name => 0,
                            SortBy::Size => u64::MAX - idx.size[i as usize],
                            SortBy::Modified => (u32::MAX - idx.mtime[i as usize]) as u64,
                        };
                        (k, idx.name_rank[i as usize], i)
                    })
                    .collect();
                all.sort_unstable();
                for limit in [1, 5, 33, 100_000] {
                    let want: Vec<u32> = all.iter().take(limit).map(|k| k.2).collect();
                    let opts = SearchOptions { limit, sort };
                    assert_eq!(idx.top(hits.clone(), &opts, needle, &[], Arc::new(Vec::new())), want, "seed {seed}, {sort:?}, limit {limit}");
                }
            }
        }
    }
}
