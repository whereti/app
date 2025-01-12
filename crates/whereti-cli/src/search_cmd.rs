//! `wt search` — load the saved index (no admin rights needed) and search it.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, ValueEnum};
use whereti_core::{snapshot, NameIndex, SearchOptions, SortBy};
use whereti_query::{civil_from_day, parse, DateCtx};

use crate::index_cmd::default_index_path;
use crate::{bytes, dur, num};

#[derive(Args)]
pub struct SearchArgs {
    /// What to look for, e.g. raport ext:pdf, "nume exact", *.jpg dm:today
    #[arg(required = true, num_args = 1..)]
    query: Vec<String>,
    /// Index file (default: %LOCALAPPDATA%\whereti\names.idx)
    #[arg(long)]
    index: Option<PathBuf>,
    /// How many results to show
    #[arg(short = 'n', long, default_value_t = 20)]
    limit: usize,
    #[arg(long, value_enum, default_value_t = Sort::Relevance)]
    sort: Sort,
    /// Run the search N times and report timing percentiles
    #[arg(long)]
    bench: Option<usize>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Sort {
    Relevance,
    Name,
    Size,
    Date,
}

pub fn run(args: SearchArgs) -> Result<(), String> {
    let path = args.index.unwrap_or_else(default_index_path);
    let t = Instant::now();
    let index = snapshot::load(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("no index at {} — run `wt index` from an Administrator terminal first", path.display())
        } else {
            format!("cannot load {}: {e} — run `wt index` again", path.display())
        }
    })?;
    let load_time = t.elapsed();

    let ctx = date_ctx();
    let text = args.query.join(" ");
    let query = parse(&text, &ctx);
    let opts = SearchOptions {
        limit: args.limit,
        sort: match args.sort {
            Sort::Relevance => SortBy::Relevance,
            Sort::Name => SortBy::Name,
            Sort::Size => SortBy::Size,
            Sort::Date => SortBy::Modified,
        },
    };

    let t = Instant::now();
    let results = index.search(&query, &opts);
    let first = t.elapsed();

    for w in &results.warnings {
        println!("note: {w}");
    }
    for &i in &results.hits {
        let size = if index.is_dir(i) { "<dir>".to_string() } else { bytes(index.size(i)) };
        println!("{size:>10}  {}  {}", local_time(index.modified(i), ctx.tz_offset), index.full_path(i));
    }
    println!();
    println!(
        "{} results · searched {} entries in {} · index loaded in {}",
        num(results.total as u64),
        num(index.len() as u64),
        dur_fine(first),
        dur(load_time)
    );

    if let Some(n) = args.bench {
        bench(&index, &query, &opts, n.max(1));
    }
    Ok(())
}

fn bench(index: &NameIndex, query: &whereti_query::Query, opts: &SearchOptions, n: usize) {
    let mut times: Vec<Duration> = (0..n)
        .map(|_| {
            let t = Instant::now();
            let r = index.search(query, opts);
            std::hint::black_box(r.total);
            t.elapsed()
        })
        .collect();
    times.sort_unstable();
    let pct = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    println!(
        "bench ×{n}: min {} · p50 {} · p99 {} · max {}",
        dur_fine(times[0]),
        dur_fine(pct(0.5)),
        dur_fine(pct(0.99)),
        dur_fine(*times.last().unwrap())
    );
}

/// Now and the local UTC offset, for `dm:today` & co.
fn date_ctx() -> DateCtx {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    let unix_min = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    let now = (unix_min + 134_774 * 1440) as u32;
    // SAFETY: zeroed POD out-parameter.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    let bias = tz.Bias + if id == TIME_ZONE_ID_DAYLIGHT { tz.DaylightBias } else { tz.StandardBias };
    DateCtx { now, tz_offset: -bias }
}

fn local_time(minutes_utc: u32, tz_offset: i32) -> String {
    if minutes_utc == 0 {
        return "                ".into();
    }
    let local = minutes_utc as i64 + tz_offset as i64;
    let (y, m, d) = civil_from_day(local.div_euclid(1440));
    let mins = local.rem_euclid(1440);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", mins / 60, mins % 60)
}

fn dur_fine(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.2} ms")
    } else {
        dur(d)
    }
}
