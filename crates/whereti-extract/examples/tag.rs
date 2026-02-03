//! Tags photos with the CLIP model, with timings (PLAN.md §55):
//!
//!   cargo run --release -p whereti-extract --example tag -- <onnxruntime.dll> <model.onnx> <photo>...

use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dll, model, photos) = (&args[0], &args[1], &args[2..]);
    let t = Instant::now();
    let mut tagger = whereti_extract::tags::Tagger::new(Path::new(dll), Path::new(model)).expect("model");
    println!("model loaded in {} ms", t.elapsed().as_millis());
    let labels = whereti_extract::tags::labels();
    let mut times = Vec::new();
    for p in photos {
        let bytes = std::fs::read(p).expect("read");
        let t = Instant::now();
        match tagger.tags(&bytes) {
            Ok(tags) => {
                times.push(t.elapsed().as_millis());
                let list: Vec<String> = tags.iter().map(|(i, s)| format!("{} {s:.2}", labels[*i][0])).collect();
                println!("{:<32} {}", Path::new(p).file_name().unwrap().to_string_lossy(), list.join("  "));
            }
            Err(e) => println!("{p}: {e}"),
        }
    }
    times.sort();
    if let Some(m) = times.get(times.len() / 2) {
        println!("{} photos, {m} ms each (decode + model, 1 thread)", times.len());
    }
}
