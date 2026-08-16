//! Search by meaning (PLAN.md §65), the service's side: the model files
//! (downloaded only when the user turns it on, checked by size and SHA-256),
//! the vectors of the documents (`meaning.bin` beside the content index),
//! and the comparison of a question with them.
//!
//! The model itself runs in the sandbox (whereti-extract `--embed`); one
//! worker serves both the indexing and the questions, and goes away after a
//! few idle minutes (it holds ~250 MB while alive).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use whereti_extract::meaning::{similarity, DIMS, MIN_SIMILARITY};
use whereti_extract::worker::EmbedWorker;

use crate::system;

const REVISION: &str = "761b726dd34fb83930e26aab4e9ac3899aa1fa78";
pub(crate) struct ModelFile {
    pub name: &'static str,
    pub url_path: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

pub(crate) const FILES: [ModelFile; 2] = [
    ModelFile {
        name: "multilingual-e5-small-int8.onnx",
        url_path: "onnx/model_quantized.onnx",
        sha256: "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
        size: 118_308_185,
    },
    ModelFile {
        name: "multilingual-e5-small-tokenizer.json",
        url_path: "tokenizer.json",
        sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
        size: 17_082_730,
    },
];

pub(crate) fn url(f: &ModelFile) -> String {
    format!("https://huggingface.co/Xenova/multilingual-e5-small/resolve/{REVISION}/{}", f.url_path)
}

/// Where the files live: only the service (SYSTEM) can write there.
pub(crate) fn path(f: &ModelFile) -> PathBuf {
    system::data_dir().join("models").join(f.name)
}

pub(crate) fn model_ready() -> bool {
    FILES.iter().all(|f| std::fs::metadata(path(f)).is_ok_and(|m| m.len() == f.size))
}

pub(crate) fn total_size() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

/// Pieces of text read per document.
pub(crate) const PIECES: usize = 3;
/// The worker goes away after this long without work.
const IDLE: Duration = Duration::from_secs(300);

/// One worker for indexing and questions, started when needed.
pub(crate) struct Model {
    worker: Option<EmbedWorker>,
    last_use: Instant,
}

impl Model {
    pub fn new() -> Model {
        Model { worker: None, last_use: Instant::now() }
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<i8>, String> {
        if self.worker.is_none() {
            let [model, tokenizer] = &FILES;
            self.worker = EmbedWorker::beside_current_exe(&path(model), &path(tokenizer));
        }
        self.last_use = Instant::now();
        self.worker.as_mut().ok_or("the meaning model is not available")?.embed(text)
    }

    /// Ends the worker if it was idle for a while.
    pub fn tidy(&mut self) {
        if self.worker.is_some() && self.last_use.elapsed() > IDLE {
            self.worker = None;
        }
    }
}

/// Every document's vectors (one per piece), by content-index key.
pub(crate) struct Vectors {
    path: PathBuf,
    map: HashMap<u64, Vec<Vec<i8>>>,
    dirty: bool,
}

const MAGIC: &[u8; 8] = b"WMEANv01";

impl Vectors {
    pub fn load(dir: &Path) -> Vectors {
        let path = dir.join("meaning.bin");
        let mut map = HashMap::new();
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.starts_with(MAGIC) {
                let mut r = &bytes[MAGIC.len()..];
                loop {
                    let mut head = [0u8; 9];
                    if r.read_exact(&mut head).is_err() {
                        break;
                    }
                    let key = u64::from_le_bytes(head[..8].try_into().expect("8 bytes"));
                    let mut v = Vec::with_capacity(head[8] as usize);
                    for _ in 0..head[8] {
                        let mut piece = vec![0u8; DIMS];
                        if r.read_exact(&mut piece).is_err() {
                            break;
                        }
                        v.push(piece.into_iter().map(|b| b as i8).collect());
                    }
                    map.insert(key, v);
                }
            }
        }
        Vectors { path, map, dirty: false }
    }

    pub fn contains(&self, key: u64) -> bool {
        self.map.contains_key(&key)
    }

    pub fn set(&mut self, key: u64, v: Vec<Vec<i8>>) {
        self.map.insert(key, v);
        self.dirty = true;
    }

    pub fn remove(&mut self, key: u64) {
        if self.map.remove(&key).is_some() {
            self.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.dirty = true;
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let mut out = Vec::with_capacity(8 + self.map.len() * (9 + PIECES * DIMS));
        out.extend_from_slice(MAGIC);
        for (k, v) in &self.map {
            out.extend_from_slice(&k.to_le_bytes());
            out.push(v.len().min(255) as u8);
            for piece in v.iter().take(255) {
                out.extend(piece.iter().map(|&b| b as u8));
            }
        }
        let tmp = self.path.with_extension("tmp");
        let ok = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(&out)).is_ok();
        if ok && std::fs::rename(&tmp, &self.path).is_ok() {
            self.dirty = false;
        }
    }

    /// The documents closest in meaning to `question` (a vector), best
    /// first: `(key, similarity)`, at most `limit`, none below the bar.
    pub fn closest(&self, question: &[i8], limit: usize) -> Vec<(u64, f32)> {
        let mut found: Vec<(u64, f32)> = self
            .map
            .iter()
            .filter_map(|(k, pieces)| {
                let best = pieces.iter().map(|p| similarity(question, p)).fold(f32::MIN, f32::max);
                (best >= MIN_SIMILARITY).then_some((*k, best))
            })
            .collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1));
        found.truncate(limit);
        found
    }
}

/// Is this query a question worth asking by meaning? Two plain words at
/// least, eight letters in all; no `content:`, `regex:` and the like.
pub fn question(words: &str) -> Option<String> {
    let plain: Vec<&str> = words.split_whitespace().filter(|w| !w.contains(':') && !w.starts_with('!')).collect();
    (plain.len() >= 2 && plain.iter().map(|w| w.chars().count()).sum::<usize>() >= 8).then(|| plain.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_survive_a_save_and_find_the_closest() {
        let dir = std::env::temp_dir().join(format!("whereti-meaning-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let unit = |i: usize| {
            let mut v = vec![0i8; DIMS];
            v[i] = 127;
            v
        };
        let mut s = Vectors::load(&dir);
        s.set(1, vec![unit(0), unit(1)]);
        s.set(2, vec![unit(2)]);
        s.save();
        let s = Vectors::load(&dir);
        assert_eq!(s.closest(&unit(1), 10), vec![(1, 1.0)]);
        assert!(s.closest(&unit(3), 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn questions() {
        assert_eq!(question("contractul de chirie").as_deref(), Some("contractul de chirie"));
        assert!(question("factura").is_none());
        assert!(question("a b").is_none());
    }
}
