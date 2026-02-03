//! Tags photos through the sandboxed worker (PLAN.md §55):
//!
//!   cargo run --release -p whereti-extract --example tag_worker -- <whereti-extract.exe> <model.onnx> <photo>...

use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut w = whereti_extract::worker::TagWorker::with_exe(PathBuf::from(&args[0]), Path::new(&args[1]));
    let labels = whereti_extract::tags::labels();
    let t = Instant::now();
    for p in &args[2..] {
        let bytes = std::fs::read(p).expect("read");
        match w.tags(&bytes) {
            Ok(tags) => {
                let names: Vec<&str> = tags.iter().map(|(i, _)| labels[*i][7]).collect();
                println!("{:<30} {}", Path::new(p).file_name().unwrap().to_string_lossy(), names.join(", "));
            }
            Err(e) => println!("{p}: {e}"),
        }
    }
    println!("{} photos in {} ms, model load included", args.len() - 2, t.elapsed().as_millis());
}
