//! Learning from what the user opens (PLAN.md §49). Everything stays on
//! this PC.
//!
//! Two sources: the files opened from whereti (counted, in
//! `%APPDATA%\whereti\usage.txt`) and the files Windows lists as recently
//! opened (its Recent shortcuts, from any app). For each search, the ones
//! whose name holds every word typed go to the service with a weight, and it
//! ranks them higher among its hits. The service keeps nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use whereti_query::fold;

/// Settings > Search > "Learn from what you open".
static ON: AtomicBool = AtomicBool::new(true);
/// Files remembered; the least used are dropped beyond this.
const MAX_REMEMBERED: usize = 2000;
/// Files sent with one search.
const MAX_SENT: usize = 32;
/// How often Windows' Recent list is read again.
const RECENT_EVERY: Duration = Duration::from_secs(30);

pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
struct Opened {
    count: u32,
    /// Seconds since 1970.
    last: u64,
}

struct Store {
    /// Loaded on first use.
    opened: Option<HashMap<String, Opened>>,
    /// Windows' recent files: (opened, seconds since 1970; path).
    recent: Vec<(u64, String)>,
    recent_read: Option<Instant>,
}

static STORE: Mutex<Store> = Mutex::new(Store { opened: None, recent: Vec::new(), recent_read: None });

fn file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("usage.txt")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `count \t last \t path` per line.
fn parse(text: &str) -> HashMap<String, Opened> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let count = parts.next()?.parse().ok()?;
            let last = parts.next()?.parse().ok()?;
            let path = parts.next()?.trim();
            (!path.is_empty()).then(|| (path.to_string(), Opened { count, last }))
        })
        .collect()
}

fn opened(store: &mut Store) -> &mut HashMap<String, Opened> {
    store.opened.get_or_insert_with(|| std::fs::read_to_string(file()).map(|t| parse(&t)).unwrap_or_default())
}

/// 1 (a little) to 12 (a lot): often and lately.
fn weight(count: u32, age_secs: u64) -> u8 {
    const DAY: u64 = 24 * 3600;
    let often = match count {
        0 => 0,
        1 => 2,
        2 => 3,
        3 | 4 => 4,
        5..=9 => 5,
        _ => 6,
    };
    let lately = match age_secs {
        a if a < DAY => 6,
        a if a < 7 * DAY => 4,
        a if a < 30 * DAY => 2,
        a if a < 365 * DAY => 1,
        _ => 0,
    };
    (often + lately).clamp(1, 12)
}

/// A file was opened (or shown in Explorer) from whereti.
pub fn record(path: &str) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let text = {
        let Ok(mut store) = STORE.lock() else { return };
        let map = opened(&mut store);
        let e = map.entry(path.to_string()).or_insert(Opened { count: 0, last: 0 });
        e.count = e.count.saturating_add(1);
        e.last = now();
        if map.len() > MAX_REMEMBERED {
            let t = now();
            let mut by_weight: Vec<(u8, String)> =
                map.iter().map(|(p, o)| (weight(o.count, t.saturating_sub(o.last)), p.clone())).collect();
            by_weight.sort();
            for (_, p) in by_weight.into_iter().take(map.len() - MAX_REMEMBERED) {
                map.remove(&p);
            }
        }
        map.iter().map(|(p, o)| format!("{}\t{}\t{p}\n", o.count, o.last)).collect::<String>()
    };
    SAVED.save(file(), text);
}

static SAVED: crate::shell::Saved = crate::shell::Saved::new();

/// Settings > Search > "Forget": whereti's own list. Windows' Recent list is
/// Windows' (cleared from Explorer's options).
pub fn forget() {
    if let Ok(mut store) = STORE.lock() {
        store.opened = Some(HashMap::new());
    }
    SAVED.delete(&file());
}

/// The plain words of a query, folded: what a used file's name must hold.
/// Functions (`ext:pdf`), exclusions (`!tmp`), wildcards and `|` are left
/// out; the service checks every file against the whole query anyway.
fn words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|w| w.trim_matches('"'))
        .filter(|w| !w.is_empty() && !w.contains(':') && !w.starts_with(['!', '-']) && !w.contains(['*', '?', '|']))
        .map(fold)
        .collect()
}

/// The used files whose name holds every word of `query`, heaviest first.
/// Runs on the searcher thread (it may read Windows' Recent shortcuts).
pub fn for_query(query: &str) -> Vec<(String, u8)> {
    if !ON.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let words = words(query);
    if words.is_empty() {
        return Vec::new();
    }
    let stale = STORE.lock().map(|s| s.recent_read.is_none_or(|t| t.elapsed() > RECENT_EVERY)).unwrap_or(false);
    if stale {
        let recent: Vec<(u64, String)> = crate::recent::opened_files()
            .into_iter()
            .map(|(t, p)| (t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), p.to_string_lossy().into_owned()))
            .collect();
        if let Ok(mut s) = STORE.lock() {
            s.recent = recent;
            s.recent_read = Some(Instant::now());
        }
    }
    let Ok(mut store) = STORE.lock() else { return Vec::new() };
    let t = now();
    let matches = |path: &str| {
        let name = fold(path.rsplit('\\').next().unwrap_or(path));
        words.iter().all(|w| name.contains(w.as_str()))
    };
    let mut found: HashMap<String, (String, u8)> = HashMap::new();
    let mut add = |path: &str, w: u8| {
        let e = found.entry(path.to_lowercase()).or_insert_with(|| (path.to_string(), 0));
        e.1 = e.1.max(w);
    };
    for (opened_at, path) in &store.recent {
        if matches(path) {
            add(path, weight(1, t.saturating_sub(*opened_at)).saturating_sub(1).max(1));
        }
    }
    for (path, o) in opened(&mut store).iter() {
        if matches(path) {
            add(path, weight(o.count, t.saturating_sub(o.last)));
        }
    }
    let mut out: Vec<(String, u8)> = found.into_values().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(MAX_SENT);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_grow_with_use_and_fade_with_time() {
        const DAY: u64 = 24 * 3600;
        assert!(weight(10, 0) > weight(1, 0));
        assert!(weight(1, 0) > weight(1, 40 * DAY));
        assert_eq!(weight(0, 10 * 365 * DAY), 1);
        assert_eq!(weight(100, 0), 12);
    }

    #[test]
    fn only_plain_words_are_matched() {
        assert_eq!(words(r#"Raport ext:pdf !tmp *.doc "Anual" a|b -x"#), ["raport", "anual"]);
    }

    #[test]
    fn the_file_round_trips() {
        let m = parse("3\t1700000000\tC:\\Users\\a\\Documents\\x.pdf\nbad line\n1\t5\tD:\\y z.txt\n");
        assert_eq!(m.len(), 2);
        assert_eq!(m[r"C:\Users\a\Documents\x.pdf"].count, 3);
        assert_eq!(m[r"D:\y z.txt"].last, 5);
    }
}
