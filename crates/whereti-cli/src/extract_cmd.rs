//! `wt extract FILE...` - text extraction through the sandboxed worker.

use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use whereti_extract::worker::Sandbox;

use crate::{bytes, dur};

#[derive(Args)]
pub struct ExtractArgs {
    /// Files to extract
    #[arg(required = true, num_args = 1..)]
    files: Vec<PathBuf>,
    /// Print this many characters of each text (0: summary only)
    #[arg(long, default_value_t = 300)]
    show: usize,
    /// Most characters to extract per file
    #[arg(long, default_value_t = 1_000_000)]
    max: usize,
    /// Kill the worker after this many milliseconds
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
    /// Worker memory cap in MB
    #[arg(long, default_value_t = 512)]
    memory_mb: usize,
}

pub fn run(args: ExtractArgs) -> Result<(), String> {
    let mut sandbox = Sandbox::beside_current_exe()
        .ok_or("whereti-extract.exe is missing next to wt.exe - build it with `cargo build -p whereti-extract`")?;
    sandbox.timeout = std::time::Duration::from_millis(args.timeout_ms);
    sandbox.memory_limit = args.memory_mb << 20;
    let (mut ok, mut failed) = (0, 0);
    for f in &args.files {
        // the worker checks the real path against this one: a whole path
        let f = &std::path::absolute(f).unwrap_or_else(|_| f.clone());
        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        match sandbox.extract_file(f, args.max) {
            Ok(e) => {
                ok += 1;
                println!(
                    "ok    {:>9} → {:>9} chars{} in {}  {}",
                    bytes(size),
                    e.text.chars().count(),
                    if e.truncated { " (cut)" } else { "" },
                    dur(t.elapsed()),
                    f.display()
                );
                if args.show > 0 {
                    let sample: String = e.text.chars().take(args.show).collect();
                    for line in sample.lines() {
                        println!("      | {line}");
                    }
                }
            }
            Err(e) => {
                failed += 1;
                println!("fail  {:>9} in {}  {}: {e}", bytes(size), dur(t.elapsed()), f.display());
            }
        }
    }
    if args.files.len() > 1 {
        println!("\n{ok} extracted, {failed} failed");
    }
    if failed > 0 {
        return Err(format!("{failed} of {} files could not be extracted", ok + failed));
    }
    Ok(())
}
