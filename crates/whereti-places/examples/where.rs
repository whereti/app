//! Prints where each photo given was taken (PLAN.md §53), with timings:
//!
//!   cargo run --release -p whereti-places --example where -- <photo or folder>...

use std::path::{Path, PathBuf};
use std::time::Instant;

fn photos(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            photos(&e.path(), out);
        }
    } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "heic" | "heif" | "png" | "tif" | "tiff" | "webp" | "dng")
    }) {
        out.push(p.to_path_buf());
    }
}

fn main() {
    let mut files = Vec::new();
    for a in std::env::args().skip(1) {
        photos(Path::new(&a), &mut files);
    }
    let t = Instant::now();
    let (mut with_gps, mut named) = (0, 0);
    let mut labels: std::collections::BTreeMap<String, usize> = Default::default();
    for f in &files {
        let Ok(bytes) = std::fs::read(f) else { continue };
        if let Some((lat, lon)) = whereti_places::gps(&bytes) {
            with_gps += 1;
            if let Some(p) = whereti_places::place_of(lat, lon) {
                named += 1;
                *labels.entry(p.label()).or_default() += 1;
            }
        }
    }
    println!("{} photos, {with_gps} with GPS, {named} named, in {} ms", files.len(), t.elapsed().as_millis());
    for (l, n) in labels {
        println!("  {n:>5}  {l}");
    }
}
