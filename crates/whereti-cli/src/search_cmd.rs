//! `wt search` and `wt status`.
//!
//! `wt search` asks the whereti service (named pipe, no admin rights needed).
//! When the service is not running, or with `--index`, it loads the saved
//! index itself.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use whereti_core::{snapshot, NameIndex, SearchOptions, SortBy};
use whereti_engine::system::date_ctx;
use whereti_ipc::{Client, Request, Response, Row, SearchRequest};
use whereti_query::{civil_from_day, parse};

use crate::index_cmd::default_index_path;
use crate::{bytes, dur, num};

#[derive(Args)]
pub struct SearchArgs {
    /// What to look for, e.g. raport ext:pdf, "nume exact", *.jpg dm:today
    #[arg(required = true, num_args = 1..)]
    query: Vec<String>,
    /// Search this index file directly instead of asking the service
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
    if args.index.is_none() {
        match Client::connect(Duration::from_millis(500)) {
            Ok(client) => return run_via_service(client, &args),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // service not running
            Err(e) => return Err(format!("cannot talk to the whereti service: {e}")),
        }
    }
    run_local(&args)
}

fn run_via_service(mut client: Client, args: &SearchArgs) -> Result<(), String> {
    let req = Request::Search(SearchRequest {
        query: args.query.join(" "),
        limit: args.limit as u32,
        usage: Vec::new(),
        sort: match args.sort {
            Sort::Relevance => whereti_ipc::Sort::Relevance,
            Sort::Name => whereti_ipc::Sort::Name,
            Sort::Size => whereti_ipc::Sort::Size,
            Sort::Date => whereti_ipc::Sort::Modified,
        },
    });
    let t = Instant::now();
    let reply = match client.call(&req).map_err(|e| e.to_string())? {
        Response::Search(r) => r,
        Response::Error(e) => return Err(e),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    let round_trip = t.elapsed();
    let tz = date_ctx().tz_offset;
    for w in &reply.warnings {
        println!("note: {w}");
    }
    let first_close = reply.rows.len() - (reply.close as usize).min(reply.rows.len());
    for (k, r) in reply.rows.iter().enumerate() {
        if k == first_close {
            println!("close matches (one letter off):");
        }
        print_row(r, tz);
    }
    println!();
    println!(
        "{} results · {} in the service, {} round trip (via the whereti service)",
        num(reply.total),
        dur_fine(Duration::from_micros(reply.micros)),
        dur_fine(round_trip)
    );
    // Then the files whose text matches (not already listed by name).
    let content_req = Request::ContentSearch(SearchRequest {
        query: args.query.join(" "),
        limit: args.limit as u32,
        sort: whereti_ipc::Sort::Relevance,
        usage: Vec::new(),
    });
    let t = Instant::now();
    if let Response::Content(c) = client.call(&content_req).map_err(|e| e.to_string())? {
        let named: std::collections::HashSet<&str> = reply.rows.iter().map(|r| r.path.as_str()).collect();
        let extra: Vec<&Row> = c.rows.iter().filter(|r| !named.contains(r.path.as_str())).collect();
        if !extra.is_empty() || c.note.is_some() {
            println!();
            println!("found in content:");
            for r in &extra {
                print_row(r, tz);
            }
            if let Some(n) = &c.note {
                println!("note: {n}");
            }
            println!();
            println!(
                "{} files contain the words · {} in the service, {} round trip",
                num(c.total),
                dur_fine(Duration::from_micros(c.micros)),
                dur_fine(t.elapsed())
            );
        }
    }
    if let Some(n) = args.bench {
        let mut service = Vec::new();
        let mut total = Vec::new();
        for _ in 0..n.max(1) {
            let t = Instant::now();
            if let Response::Search(r) = client.call(&req).map_err(|e| e.to_string())? {
                service.push(Duration::from_micros(r.micros));
            }
            total.push(t.elapsed());
        }
        print_percentiles(&format!("bench ×{n} in service"), service);
        print_percentiles(&format!("bench ×{n} round trip"), total);
    }
    Ok(())
}

fn print_row(r: &Row, tz: i32) {
    let size = if r.is_dir { "<dir>".to_string() } else { bytes(r.size) };
    println!("{size:>10}  {}  {}", local_time(r.modified, tz), r.path);
}

fn run_local(args: &SearchArgs) -> Result<(), String> {
    let path = args.index.clone().unwrap_or_else(default_index_path);
    let t = Instant::now();
    let index = snapshot::load(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("no index at {} - run `wt index` from an Administrator terminal first", path.display())
        }
        std::io::ErrorKind::PermissionDenied if args.index.is_some() => {
            format!("{} is readable only by administrators", path.display())
        }
        std::io::ErrorKind::PermissionDenied => format!(
            "the whereti service is not running, and {} is readable only by administrators - \
             start the service (`whereti-service start`) or run `wt serve` as Administrator",
            path.display()
        ),
        _ => format!("cannot load {}: {e} - run `wt index` again", path.display()),
    })?;
    let load_time = t.elapsed();

    let ctx = date_ctx();
    let query = parse(&args.query.join(" "), &ctx);
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
    let first_close = results.hits.len() - results.close;
    for (k, &i) in results.hits.iter().enumerate() {
        if k == first_close {
            println!("close matches (one letter off):");
        }
        let row = Row {
            path: index.full_path(i),
            is_dir: index.is_dir(i),
            size: index.size(i),
            modified: index.modified(i),
            flags: index.flags(i),
        };
        print_row(&row, ctx.tz_offset);
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
    let times: Vec<Duration> = (0..n)
        .map(|_| {
            let t = Instant::now();
            let r = index.search(query, opts);
            std::hint::black_box(r.total);
            t.elapsed()
        })
        .collect();
    print_percentiles(&format!("bench ×{n}"), times);
}

fn print_percentiles(label: &str, mut times: Vec<Duration>) {
    if times.is_empty() {
        return;
    }
    times.sort_unstable();
    let pct = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    println!(
        "{label}: min {} · p50 {} · p99 {} · max {}",
        dur_fine(times[0]),
        dur_fine(pct(0.5)),
        dur_fine(pct(0.99)),
        dur_fine(*times.last().unwrap())
    );
}

#[derive(Args)]
pub struct StatusArgs {
    /// Also print the service's recent log lines
    #[arg(long)]
    log: bool,
}

/// `wt status` - what the service is doing.
pub fn status(args: StatusArgs) -> Result<(), String> {
    let mut client = Client::connect(Duration::from_millis(500)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "the whereti service is not running".to_string()
        } else {
            e.to_string()
        }
    })?;
    let s = match client.call(&Request::Status).map_err(|e| e.to_string())? {
        Response::Status(s) => s,
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    println!("state          {}", s.state);
    println!("entries        {}", num(s.entries));
    for v in &s.volumes {
        println!("volume         {}:  journal position {}", v.letter, num(v.next_usn as u64));
    }
    println!("last change    {}", s.last_change_secs.map_or("none yet".into(), |x| format!("{x} s ago")));
    println!("open files     {} (re-read every 30 s until closed)", num(s.open_files));
    println!(
        "uptime         {} s · CPU used {} · memory {}",
        s.uptime_secs,
        dur(Duration::from_millis(s.cpu_ms)),
        bytes(s.private_bytes)
    );
    let c = &s.content;
    println!(
        "content        {} · {} documents, {} waiting, {} unreadable · {} on disk",
        c.state,
        num(c.files),
        num(c.queued),
        num(c.failed),
        bytes(c.disk_bytes)
    );
    println!("photos         {}", if c.tags.is_empty() { "off" } else { c.tags.as_str() });
    match &s.work_folders {
        Some(f) if !f.is_empty() => println!("works in       {}", f.join(" · ")),
        Some(_) => println!("works in       (nothing learnt yet)"),
        None => println!("works in       off"),
    }
    if args.log {
        println!();
        for line in &s.recent_log {
            println!("{line}");
        }
    }
    Ok(())
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

#[derive(Args)]
pub struct ExclusionsArgs {
    /// list (default), add or remove
    #[arg(default_value = "list")]
    action: String,
    /// Path to add or remove: C:\Folder, \Folder (every drive) or **\name (anywhere)
    path: Option<String>,
}

/// `wt exclusions [list|add|remove] [PATH]` - what the index leaves out.
pub fn exclusions(args: ExclusionsArgs) -> Result<(), String> {
    let mut client = Client::connect(Duration::from_millis(500)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "the whereti service is not running".to_string()
        } else {
            e.to_string()
        }
    })?;
    let cfg = match client.call(&Request::GetConfig).map_err(|e| e.to_string())? {
        Response::Config(c) => c,
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    let mut paths = cfg.exclusions.clone();
    match (args.action.as_str(), args.path) {
        ("list", _) => {
            // with what each one left out at the last scan, when the service knows
            for p in &paths {
                match cfg.skipped.iter().find(|(r, _)| r == p) {
                    Some((_, n)) => println!("{n:>10}  {p}"),
                    None => println!("{:>10}  {p}", "-"),
                }
            }
            return Ok(());
        }
        ("add", Some(p)) => {
            if paths.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
                return Err(format!("{p} is already excluded"));
            }
            paths.push(p);
        }
        ("remove", Some(p)) => {
            let before = paths.len();
            paths.retain(|x| !x.eq_ignore_ascii_case(&p));
            if paths.len() == before {
                return Err(format!("{p} is not in the list"));
            }
        }
        _ => return Err("usage: wt exclusions [list | add PATH | remove PATH]".into()),
    }
    match client.call(&Request::SetExclusions { paths }).map_err(|e| e.to_string())? {
        Response::Ok => {
            println!("saved - the service is rebuilding the index (a few seconds); `wt status` shows progress");
            Ok(())
        }
        Response::Error(e) => Err(e),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

#[derive(Args)]
pub struct ContentArgs {
    /// list (default), add, remove, reset (back to Desktop, Documents,
    /// Downloads), or photos on|off (recognise objects and animals in photos)
    #[arg(default_value = "list")]
    action: String,
    /// Folder to add or remove, e.g. D:\Acte
    path: Option<String>,
}

/// `wt content [list|add|remove|reset] [FOLDER]` - folders indexed by content.
pub fn content(args: ContentArgs) -> Result<(), String> {
    let mut client = Client::connect(Duration::from_millis(500)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "the whereti service is not running".to_string()
        } else {
            e.to_string()
        }
    })?;
    let cfg = match client.call(&Request::GetConfig).map_err(|e| e.to_string())? {
        Response::Config(c) => c,
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    let mut paths = cfg.content_folders.clone();
    if args.action == "photos" {
        let on = match args.path.as_deref() {
            Some("on") => true,
            Some("off") => false,
            _ => return Err("wt content photos on|off".into()),
        };
        return match client.call(&Request::SetPhotoTags { on }).map_err(|e| e.to_string())? {
            Response::Ok => {
                println!("recognising objects and animals in photos: {}", if on { "on (the model downloads first)" } else { "off" });
                Ok(())
            }
            Response::Error(e) => Err(e),
            other => Err(format!("unexpected reply: {other:?}")),
        };
    }
    match (args.action.as_str(), args.path) {
        ("list", _) => {
            for p in &paths {
                println!("{p}");
            }
            return Ok(());
        }
        ("add", Some(p)) => {
            if paths.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
                return Err(format!("{p} is already indexed"));
            }
            paths.push(p);
        }
        ("remove", Some(p)) => {
            let before = paths.len();
            paths.retain(|x| !x.eq_ignore_ascii_case(&p));
            if paths.len() == before {
                return Err(format!("{p} is not in the list"));
            }
        }
        ("reset", _) => paths = cfg.default_content_folders.clone(),
        _ => return Err("usage: wt content [list | add FOLDER | remove FOLDER | reset]".into()),
    }
    match client.call(&Request::SetContentFolders { paths }).map_err(|e| e.to_string())? {
        Response::Ok => {
            println!("saved - the service indexes the difference in the background; `wt status` shows progress");
            Ok(())
        }
        Response::Error(e) => Err(e),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}
