//! `wt` — whereti command line.
//!
//! Phase 0: `wt scan C:` reads a volume with both methods (direct MFT and
//! FSCTL_ENUM_USN_DATA), times them, measures memory and checks they agree.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};

mod index_cmd;
mod inspect_cmd;
mod live_cmd;
mod search_cmd;
use whereti_ntfs::compare::{self, Diff};
use whereti_ntfs::mft::{self, MftScan};
use whereti_ntfs::parse::ROOT_RECORD;
use whereti_ntfs::{usn_enum, Volume};

#[derive(Parser)]
#[command(name = "wt", version, about = "whereti — instant file search for Windows")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read an NTFS volume, report speed and memory, and cross-check both readers
    Scan(ScanArgs),
    /// Scan volumes and save the name index (needs an Administrator terminal)
    Index(index_cmd::IndexArgs),
    /// Search the saved index
    Search(search_cmd::SearchArgs),
    /// Bring the saved index up to date from the USN journal (Administrator)
    Update(live_cmd::UpdateArgs),
    /// Follow file changes live and keep the index current (Administrator)
    Watch(live_cmd::WatchArgs),
    /// Check that saved index + journal replay equals a fresh scan (Administrator)
    Verify(live_cmd::VerifyArgs),
    /// Show what Windows, the MFT and the index say about one path (Administrator)
    Inspect(inspect_cmd::InspectArgs),
}

#[derive(Args)]
struct ScanArgs {
    /// Volume to scan, e.g. C: or C
    volume: String,
    /// Which reader(s) to run
    #[arg(long, value_enum, default_value_t = Method::Both)]
    method: Method,
    /// Parser threads for the direct MFT reader (default: logical CPUs, max 8)
    #[arg(long)]
    threads: Option<usize>,
    /// Read the MFT through the Windows file cache instead of unbuffered
    #[arg(long)]
    buffered: bool,
    /// How many example differences and paths to print
    #[arg(long, default_value_t = 5)]
    samples: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Method {
    Both,
    Raw,
    Usn,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Scan(args) => scan(args),
        Cmd::Index(args) => index_cmd::run(args).map(|_| ExitCode::SUCCESS),
        Cmd::Search(args) => search_cmd::run(args).map(|_| ExitCode::SUCCESS),
        Cmd::Update(args) => live_cmd::update(args).map(|_| ExitCode::SUCCESS),
        Cmd::Watch(args) => live_cmd::watch(args).map(|_| ExitCode::SUCCESS),
        Cmd::Verify(args) => live_cmd::verify(args).map(|_| ExitCode::SUCCESS),
        Cmd::Inspect(args) => inspect_cmd::run(args).map(|_| ExitCode::SUCCESS),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scan(args: ScanArgs) -> Result<ExitCode, String> {
    let letter = parse_letter(&args.volume)?;

    let t = Instant::now();
    let vol = Volume::open(letter, !args.buffered).map_err(|e| open_error(letter, e))?;
    let open_time = t.elapsed();
    let n = vol.ntfs;
    let mft_bytes = n.mft_valid_data_length as u64;

    println!("whereti scan {letter}:");
    println!(
        "  volume   cluster {} B · sector {} B · record {} B · MFT {} ({} record slots) · opened in {}",
        n.bytes_per_cluster,
        n.bytes_per_sector,
        n.bytes_per_file_record_segment,
        bytes(mft_bytes),
        num(mft_bytes / n.bytes_per_file_record_segment as u64),
        dur(open_time),
    );
    match vol.query_usn_journal() {
        Ok(j) => println!(
            "  journal  id {:#018x} · next USN {} · max size {}",
            j.usn_journal_id,
            num(j.next_usn as u64),
            bytes(j.maximum_size)
        ),
        Err(e) => println!("  journal  not available ({e})"),
    }
    println!();

    let mut raw: Option<MftScan> = None;
    let mut scan_start_usn = None;
    let mut ok = true;

    if args.method != Method::Usn {
        let threads = args.threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
        });
        scan_start_usn = vol.query_usn_journal().ok().map(|j| j.next_usn);
        let t = Instant::now();
        match mft::scan(&vol, threads) {
            Ok(scan) => {
                let elapsed = t.elapsed();
                let s = &scan.stats;
                let files = scan.entries.iter().filter(|e| !e.is_dir).count();
                let total_size: u64 = scan.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                println!("[direct MFT]  {} in {}", per_sec(scan.entries.len(), elapsed), dur(elapsed));
                println!(
                    "  read     {} at {}/s · {} fragment(s) · {} · {} parser threads",
                    bytes(s.bytes_read),
                    bytes((s.bytes_read as f64 / elapsed.as_secs_f64().max(1e-9)) as u64),
                    s.mft_fragments,
                    if args.buffered { "buffered" } else { "unbuffered" },
                    s.threads
                );
                println!(
                    "  records  {} in use ({} base + {} extension) · {} free · {} corrupt · {} without name",
                    num(s.in_use),
                    num(s.base_records),
                    num(s.extension_records),
                    num(s.unused_records),
                    num(s.corrupt_records),
                    num(s.records_without_name)
                );
                println!(
                    "  entries  {} names = {} files + {} folders · {} of file data",
                    num(scan.entries.len() as u64),
                    num(files as u64),
                    num((scan.entries.len() - files) as u64),
                    bytes(total_size)
                );
                print_index_estimate(&scan, s.total_records);
                println!();
                if s.corrupt_records > 0 {
                    ok = false;
                }
                raw = Some(scan);
            }
            Err(e) => {
                println!("[direct MFT]  FAILED: {e}");
                println!();
                ok = false;
            }
        }
    }

    let mut usn = None;
    if args.method != Method::Raw {
        let t = Instant::now();
        match usn_enum::enumerate(&vol) {
            Ok(entries) => {
                let elapsed = t.elapsed();
                println!("[ENUM_USN_DATA]  {} in {}", per_sec(entries.len(), elapsed), dur(elapsed));
                println!("  note     names, parents and attributes only — no sizes or dates");
                println!();
                usn = Some(entries);
            }
            Err(e) => {
                println!("[ENUM_USN_DATA]  FAILED: {e}");
                println!();
                ok = false;
            }
        }
    }

    if let (Some(raw), Some(usn)) = (&raw, &usn) {
        let r = compare::compare(raw, usn, scan_start_usn, args.samples);
        println!("[compare]  {} records matched exactly", num(r.matched as u64));
        diff_line("missing from direct MFT (must be 0)", &r.only_usn);
        diff_line("name differs", &r.name_mismatch);
        diff_line("parent differs", &r.parent_mismatch);
        diff_line("sequence differs", &r.seq_mismatch);
        diff_line("changed during scan (fixed by USN replay)", &r.changed_during_scan);
        diff_line("only in direct MFT (not listed by API)", &r.only_raw);
        let verdict = if r.raw_covers_api() { "PASS" } else { "FAIL" };
        println!("  result   {verdict}: direct MFT {} what the API sees", if r.raw_covers_api() { "covers" } else { "does NOT cover" });
        println!();
        ok &= r.raw_covers_api();
    }

    if let Some(raw) = &raw {
        print_sample_paths(letter, raw, args.samples);
    }

    print_memory();
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// "C:", "c", "C:\\" -> 'C'.
fn parse_letter(v: &str) -> Result<char, String> {
    let t = v.trim_end_matches(['\\', '/', ':']);
    let mut chars = t.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
        _ => Err(format!("invalid volume \"{v}\" (expected e.g. C:)")),
    }
}

fn open_error(letter: char, e: std::io::Error) -> String {
    if e.raw_os_error() == Some(5) {
        format!("access denied to {letter}: — run wt from an Administrator terminal")
    } else {
        format!("cannot open {letter}: — {e}")
    }
}

/// Memory the compact index from PLAN.md §4.2 would need for this volume.
fn print_index_estimate(scan: &MftScan, total_records: u64) {
    let entries = &scan.entries;
    const FIXED_PER_ENTRY: u64 = 4 + 2 + 4 + 2 + 2 + 8 + 4 + 8; // off, len, parent, flags, ext, size, mtime, 2 sort orders
    let names = scan.names.len() as u64;
    let n = entries.len().max(1) as u64;
    let total = names + n * FIXED_PER_ENTRY + total_records * 4;
    println!(
        "  index    estimated {} in RAM = {:.1} B/entry (names avg {:.1} B UTF-8)",
        bytes(total),
        total as f64 / n as f64,
        names as f64 / n as f64
    );
}

fn print_sample_paths(letter: char, scan: &MftScan, samples: usize) {
    let raw = &scan.entries;
    if samples == 0 || raw.is_empty() {
        return;
    }
    let parents: HashMap<u64, (u64, &str)> = raw.iter().map(|e| (e.record, (e.parent, scan.name(e)))).collect();
    println!("[sample paths]");
    let step = (raw.len() / samples).max(1);
    for e in raw.iter().step_by(step).take(samples) {
        let mut parts = vec![scan.name(e)];
        let mut cur = e.parent;
        let mut complete = false;
        for _ in 0..1024 {
            if cur == ROOT_RECORD {
                complete = true;
                break;
            }
            match parents.get(&cur) {
                Some(&(p, name)) => {
                    parts.push(name);
                    cur = p;
                }
                None => break,
            }
        }
        parts.reverse();
        let path = format!("{letter}:\\{}", parts.join("\\"));
        let size = if e.is_dir { "<dir>".to_string() } else { bytes(e.size) };
        println!("  {path}  [{size}, modified {}]{}", filetime(e.modified), if complete { "" } else { "  (orphan)" });
    }
    println!();
}

fn diff_line(label: &str, d: &Diff) {
    println!("  {:<44} {}", label, num(d.count as u64));
    for s in &d.samples {
        println!("      {s}");
    }
}

fn print_memory() {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: valid pointer and size for the current process.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 {
        println!(
            "[process]  peak working set {} · private {} (spike data structures, not the final index)",
            bytes(c.PeakWorkingSetSize as u64),
            bytes(c.PagefileUsage as u64)
        );
    }
}

fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn dur(d: Duration) -> String {
    if d.as_secs_f64() >= 1.0 {
        format!("{:.2} s", d.as_secs_f64())
    } else {
        format!("{:.0} ms", d.as_secs_f64() * 1000.0)
    }
}

fn per_sec(n: usize, d: Duration) -> String {
    let rate = n as f64 / d.as_secs_f64().max(1e-9);
    format!("{} entries ({}/s)", num(n as u64), num(rate as u64))
}

/// FILETIME -> "YYYY-MM-DD" (UTC), without pulling in a date crate.
fn filetime(ft: u64) -> String {
    if ft == 0 {
        return "?".into();
    }
    let days = (ft / 10_000_000 / 86_400) as i64 - 134_774; // 1601-01-01 -> 1970-01-01
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_like_the_ui() {
        assert_eq!(num(0), "0");
        assert_eq!(num(1_243_551), "1.243.551");
        assert_eq!(bytes(1536), "1.5 KB");
    }

    #[test]
    fn converts_filetime_to_date() {
        // 2024-02-29 00:00:00 UTC
        assert_eq!(filetime(133_536_384_000_000_000), "2024-02-29");
        assert_eq!(filetime(0), "?");
    }
}
