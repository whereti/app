//! Search by meaning (PLAN.md §65): multilingual-e5-small (int8, 118 MB,
//! with its 17 MB tokenizer; downloaded only when the user turns it on)
//! turns a piece of text into 384 numbers; texts that mean the same are
//! close, in any of its ~100 languages, even with no word in common.
//!
//! Runs only inside whereti-extract.exe (the sandbox), single-threaded, with
//! the `onnxruntime.dll` beside it, like the photo model (tags.rs).

use std::path::Path;

/// Vector length.
pub const DIMS: usize = 384;
/// Below this, a document is not shown as close in meaning (e5 scores
/// related texts about 0.85 and up, unrelated ones about 0.80).
pub const MIN_SIMILARITY: f32 = 0.84;
/// Tokens read from one piece of text (the model takes 512; less is faster).
const MAX_TOKENS: usize = 256;

pub struct Embedder {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
    /// The model wants token_type_ids too (some exports do).
    type_ids: bool,
}

impl Embedder {
    pub fn new(onnxruntime: &Path, model: &Path, tokenizer: &Path) -> Result<Embedder, String> {
        let env = ort::init_from(onnxruntime)
            .map_err(|e| format!("cannot load {}: {e}", onnxruntime.display()))?
            .with_name("whereti")
            .build()
            .map_err(|e| e.to_string())?;
        // one thread: never weigh on the PC
        let builder = ort::session::Session::builder(&env).map_err(|e| e.to_string())?;
        let builder = builder.with_intra_threads(1).map_err(|e| e.to_string())?;
        let mut builder = builder.with_inter_threads(1).map_err(|e| e.to_string())?;
        let session = builder.commit_from_file(model).map_err(|e| format!("cannot load the model: {e}"))?;
        std::mem::forget(env);
        let type_ids = session.inputs().iter().any(|i| i.name() == "token_type_ids");
        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer).map_err(|e| format!("cannot load the tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(|e| e.to_string())?;
        tokenizer.with_padding(None);
        Ok(Embedder { session, tokenizer, type_ids })
    }

    /// The vector of one text (already prefixed "query: " or "passage: "),
    /// normalised to length 1.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        let enc = self.tokenizer.encode(text, true).map_err(|e| e.to_string())?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&m| m as i64).collect();
        let n = ids.len();
        if n == 0 {
            return Err("empty text".into());
        }
        let ids_t = ort::value::Tensor::from_array(([1usize, n], ids)).map_err(|e| e.to_string())?;
        let mask_t = ort::value::Tensor::from_array(([1usize, n], mask.clone())).map_err(|e| e.to_string())?;
        let outputs = if self.type_ids {
            let types = ort::value::Tensor::from_array(([1usize, n], vec![0i64; n])).map_err(|e| e.to_string())?;
            self.session.run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t, "token_type_ids" => types])
        } else {
            self.session.run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t])
        }
        .map_err(|e| e.to_string())?;
        let (shape, hidden) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        let dims = *shape.last().ok_or("no output")? as usize;
        if dims != DIMS || hidden.len() < n * dims {
            return Err(format!("unexpected output shape {shape:?}"));
        }
        // the mean over the real tokens, then length 1
        let mut v = vec![0f32; dims];
        let mut count = 0f32;
        for (t, &m) in mask.iter().enumerate() {
            if m == 0 {
                continue;
            }
            count += 1.0;
            for (d, x) in v.iter_mut().enumerate() {
                *x += hidden[t * dims + d];
            }
        }
        let norm = v.iter().map(|x| (x / count) * (x / count)).sum::<f32>().sqrt().max(1e-6) * count;
        Ok(v.into_iter().map(|x| x / norm).collect())
    }
}

/// A vector as 384 signed bytes (x 127): small to keep, quick to compare.
pub fn quantize(v: &[f32]) -> Vec<i8> {
    v.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

/// How alike two quantized vectors are: about the cosine, -1..1.
pub fn similarity(a: &[i8], b: &[i8]) -> f32 {
    let dot: i32 = a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum();
    dot as f32 / (127.0 * 127.0)
}

/// The pieces of a document's text that are read for its meaning: its
/// first `max` paragraphs-worth of about 900 characters each.
pub fn pieces(text: &str, max: usize) -> Vec<String> {
    const SIZE: usize = 900;
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.len() + word.len() + 1 > SIZE {
            out.push(std::mem::take(&mut cur));
            if out.len() >= max {
                return out;
            }
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if cur.split_whitespace().count() >= 3 {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_and_similarity() {
        let text = "cuvant ".repeat(400);
        let p = pieces(&text, 2);
        assert_eq!(p.len(), 2);
        assert!(p[0].len() <= 900);
        assert!(pieces("doar doua", 3).is_empty());
        let a = quantize(&[0.6, 0.8]);
        assert!((similarity(&a, &a) - 1.0).abs() < 0.02);
    }

    /// With the real model (set WHERETI_E5 to the folder with model.onnx and
    /// tokenizer.json, and have onnxruntime.dll beside the test).
    #[test]
    fn meaning_finds_the_rental_contract() {
        let Some(dir) = std::env::var_os("WHERETI_E5") else { return };
        let dir = std::path::PathBuf::from(dir);
        let mut e = Embedder::new(&dir.join("onnxruntime.dll"), &dir.join("model.onnx"), &dir.join("tokenizer.json")).unwrap();
        let q = quantize(&e.embed("query: contractul de chirie al apartamentului").unwrap());
        let rent = quantize(&e.embed("passage: Proprietarul închiriază locuința chiriașului pentru 450 euro pe lună, începând cu 1 martie.").unwrap());
        let invoice = quantize(&e.embed("passage: Factura nr. 123 pentru servicii de mentenanță IT, total de plată 1200 lei.").unwrap());
        let english = quantize(&e.embed("passage: The landlord rents the flat to the tenant for 450 euros a month.").unwrap());
        let (s_rent, s_invoice, s_en) = (similarity(&q, &rent), similarity(&q, &invoice), similarity(&q, &english));
        eprintln!("rent {s_rent:.3} invoice {s_invoice:.3} english {s_en:.3}");
        assert!(s_rent > s_invoice && s_rent > MIN_SIMILARITY);
        let _ = s_en; // across languages the small model is weaker: shown, not required
    }
}
