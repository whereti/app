//! `wt rank-eval`: how well whereti orders its results (PLAN.md §51).
//!
//! Reads a set of searches with the file each one should find
//! (`query \t expected path \t opened`, from scripts\rank-set.ps1 or written
//! by hand), asks the service for each, and reports where the expected file
//! lands: place 1, top 3, top 10, and the mean reciprocal rank.
//!
//! Each search runs twice: without what the user opens (how whereti ranks a
//! file never opened), and with it, the way the app sends it (the set's
//! "opened" times stand for Windows' Recent list).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use whereti_ipc::{Client, Request, Response, SearchRequest, Sort};
use whereti_query::fold;

/// How deep a search looks for the expected file.
const DEPTH: u32 = 100;

#[derive(Args)]
pub struct RankArgs {
    /// The set of searches (default: %LOCALAPPDATA%\whereti\rank-set.tsv)
    #[arg(long)]
    set: Option<PathBuf>,
    /// How many of the worst searches to list
    #[arg(long, default_value_t = 15)]
    worst: usize,
}

struct Case {
    query: String,
    expected: String,
    /// When it was opened (seconds since 1970), if known.
    opened: Option<u64>,
}

#[derive(Default)]
struct Score {
    n: usize,
    first: usize,
    top3: usize,
    top10: usize,
    missed: usize,
    rr: f64,
}

impl Score {
    /// `k`: how many files of the set expect this same search; any of
    /// them in the first k places counts as first (only one can be 1st).
    fn add(&mut self, rank: Option<usize>, k: usize) {
        self.n += 1;
        match rank {
            Some(r) => {
                let r = if r <= k { 1 } else { r - k + 1 };
                self.first += (r == 1) as usize;
                self.top3 += (r <= 3) as usize;
                self.top10 += (r <= 10) as usize;
                self.rr += 1.0 / r as f64;
            }
            None => self.missed += 1,
        }
    }

    fn line(&self, label: &str) -> String {
        let pct = |k: usize| format!("{:>5.0}%", 100.0 * k as f64 / self.n.max(1) as f64);
        format!(
            "{label:<34}{}  {}  {}  {}   {:.2}",
            pct(self.first),
            pct(self.top3),
            pct(self.top10),
            pct(self.missed),
            self.rr / self.n.max(1) as f64
        )
    }
}

fn default_set() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("rank-set.tsv")
}

fn read_set(path: &PathBuf) -> Result<Vec<Case>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!("cannot read {}: {e}\n(make one with: powershell -File scripts\\rank-set.ps1)", path.display())
    })?;
    Ok(text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let query = parts.next()?.trim().to_string();
            let expected = parts.next()?.trim().to_string();
            let opened = parts.next().and_then(|t| t.trim().parse().ok());
            (!query.is_empty() && !expected.is_empty()).then_some(Case { query, expected, opened })
        })
        .collect())
}

/// The app's weight for a file from Windows' Recent list (usage.rs).
fn recent_weight(age_secs: u64) -> u8 {
    const DAY: u64 = 24 * 3600;
    let lately = match age_secs {
        a if a < DAY => 6,
        a if a < 7 * DAY => 4,
        a if a < 30 * DAY => 2,
        a if a < 365 * DAY => 1,
        _ => 0,
    };
    (2u8 + lately).clamp(1, 12).saturating_sub(1).max(1)
}

/// What the app would send with `query`: the opened files whose name holds
/// every word.
fn usage_for(query: &str, cases: &[Case], now: u64) -> Vec<(String, u8)> {
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    let mut out: Vec<(String, u8)> = Vec::new();
    for c in cases {
        let Some(t) = c.opened else { continue };
        let name = fold(c.expected.rsplit('\\').next().unwrap_or(&c.expected));
        if words.iter().all(|w| name.contains(w.as_str())) && !out.iter().any(|(p, _)| p.eq_ignore_ascii_case(&c.expected)) {
            out.push((c.expected.clone(), recent_weight(now.saturating_sub(t))));
        }
    }
    out.sort_by_key(|u| std::cmp::Reverse(u.1));
    out.truncate(32);
    out
}

fn rank_of(client: &mut Client, query: &str, expected: &str, usage: Vec<(String, u8)>) -> Result<(Option<usize>, String), String> {
    let req = Request::Search(SearchRequest { query: query.to_string(), limit: DEPTH, sort: Sort::Relevance, usage });
    match client.call(&req).map_err(|e| e.to_string())? {
        Response::Search(reply) => {
            let rank = reply.rows.iter().position(|r| r.path.eq_ignore_ascii_case(expected)).map(|p| p + 1);
            let first = reply.rows.first().map(|r| r.path.clone()).unwrap_or_default();
            Ok((rank, first))
        }
        Response::Error(e) => Err(e),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

pub fn run(args: RankArgs) -> Result<(), String> {
    let path = args.set.unwrap_or_else(default_set);
    let cases = read_set(&path)?;
    if cases.is_empty() {
        return Err(format!("{} has no searches", path.display()));
    }
    let mut client = Client::connect(Duration::from_secs(2)).map_err(|e| format!("cannot talk to the whereti service: {e}"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // [one word, two words] x [without use, as the app ranks]
    let mut scores: [[Score; 2]; 2] = Default::default();
    let mut misses: Vec<(usize, &Case, String)> = Vec::new();
    for c in &cases {
        let k = cases.iter().filter(|o| o.query == c.query).count();
        let words = c.query.split_whitespace().count().clamp(1, 2) - 1;
        let (rank, _) = rank_of(&mut client, &c.query, &c.expected, Vec::new())?;
        scores[words][0].add(rank, k);
        let (rank_used, first) = rank_of(&mut client, &c.query, &c.expected, usage_for(&c.query, &cases, now))?;
        scores[words][1].add(rank_used, k);
        let place = rank_used.unwrap_or(DEPTH as usize + 1);
        if place > k {
            misses.push((place - k + 1, c, first));
        }
    }
    println!("{} searches from {}\n", cases.len(), path.display());
    println!("{:<34}{:>6}  {:>6}  {:>6}  {:>6}   MRR", "", "1st", "top 3", "top 10", "missed");
    for (w, label) in [(0, "one word"), (1, "two words")] {
        println!("{}", scores[w][0].line(&format!("{label}, without what you open")));
        println!("{}", scores[w][1].line(&format!("{label}, as the app ranks")));
    }
    misses.sort_by_key(|m| std::cmp::Reverse(m.0));
    if !misses.is_empty() && args.worst > 0 {
        println!("\nworst, as the app ranks (place, search, expected; then what came first):");
        for (rank, c, first) in misses.iter().take(args.worst) {
            let place = if *rank > DEPTH as usize { format!(">{DEPTH}") } else { rank.to_string() };
            println!("  {place:>4}  {:<24} {}\n        first: {first}", format!("\"{}\"", c.query), c.expected);
        }
    }
    Ok(())
}
