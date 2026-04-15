//! What is in a photo (PLAN.md §55): CLIP ViT-B/32's image half (int8,
//! 84 MB, downloaded only when the user says yes) turns a photo into a
//! vector, compared with one vector per label of tools/clip/labels.tsv
//! (computed ahead by tools/clip/build_labels.py). The best labels above a
//! threshold are the photo's tags, written in whereti's 8 languages so a
//! search in any of them finds it.
//!
//! Runs only inside whereti-extract.exe (the sandbox), single-threaded, with
//! ONNX Runtime loaded from the exact `onnxruntime.dll` beside it (Windows
//! has an older one of its own in System32).

use std::path::Path;
use std::sync::OnceLock;

use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode,
};
use windows::Storage::Streams::{Buffer, DataReader};

/// The labels, one per line: English, then de, es, fr, it, pl, pt_BR, ro.
static LABELS_TSV: &str = include_str!("../data/clip-labels.tsv");
/// "WCLP", u32 labels, u32 dims, then labels x dims f32 (normalised).
static LABEL_VECTORS: &[u8] = include_bytes!("../data/clip-labels.bin");

/// CLIP's input: 224x224, normalised with its training mean and spread
/// (the published values, kept as written).
const SIDE: u32 = 224;
#[allow(clippy::excessive_precision)]
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
#[allow(clippy::excessive_precision)]
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_1];
/// CLIP's learned temperature.
const SCALE: f32 = 100.0;
/// A label is kept above this share of the photo, at most `MAX_TAGS`.
pub const MIN_SHARE: f32 = 0.15;
pub const MAX_TAGS: usize = 3;

/// Every label's names, in the order of the vectors.
pub fn labels() -> &'static [Vec<&'static str>] {
    static L: OnceLock<Vec<Vec<&'static str>>> = OnceLock::new();
    L.get_or_init(|| {
        LABELS_TSV
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split('\t').map(str::trim).collect())
            .collect()
    })
}

/// The label vectors as (count, dims, values).
fn vectors() -> (usize, usize, Vec<f32>) {
    let n = u32::from_le_bytes(LABEL_VECTORS[4..8].try_into().unwrap()) as usize;
    let d = u32::from_le_bytes(LABEL_VECTORS[8..12].try_into().unwrap()) as usize;
    let v = LABEL_VECTORS[12..].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    (n, d, v)
}

/// The text a photo's tags add to its entry in the index: every name of
/// every tag, e.g. "dog Hund perro chien cane pies cachorro câine".
pub fn index_text(tags: &[(usize, f32)]) -> String {
    let mut words: Vec<&str> = Vec::new();
    for &(i, _) in tags {
        for name in labels().get(i).map(Vec::as_slice).unwrap_or(&[]) {
            if !words.iter().any(|w| w.eq_ignore_ascii_case(name)) {
                words.push(name);
            }
        }
    }
    words.join(" ")
}

/// The best labels of an image vector: `(label, share)`, best first.
pub fn best_labels(image: &[f32]) -> Vec<(usize, f32)> {
    let (n, d, v) = vectors();
    if image.len() != d {
        return Vec::new();
    }
    let norm = image.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    let logits: Vec<f32> =
        (0..n).map(|i| SCALE * v[i * d..(i + 1) * d].iter().zip(image).map(|(a, b)| a * b).sum::<f32>() / norm).collect();
    let max = logits.iter().cloned().fold(f32::MIN, f32::max);
    let exp: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
    let sum: f32 = exp.iter().sum();
    let mut ranked: Vec<(usize, f32)> = exp.iter().enumerate().map(|(i, e)| (i, e / sum)).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.into_iter().take(MAX_TAGS).filter(|t| t.1 >= MIN_SHARE).collect()
}

/// The model, loaded once per worker.
pub struct Tagger {
    session: ort::session::Session,
}

impl Tagger {
    /// `onnxruntime` is the DLL to load, `model` the image model.
    pub fn new(onnxruntime: &Path, model: &Path) -> Result<Tagger, String> {
        let env = ort::init_from(onnxruntime)
            .map_err(|e| format!("cannot load {}: {e}", onnxruntime.display()))?
            .with_name("whereti")
            .build()
            .map_err(|e| e.to_string())?;
        // one thread: recognition must never weigh on the PC
        let builder = ort::session::Session::builder(&env).map_err(|e| e.to_string())?;
        let builder = builder.with_intra_threads(1).map_err(|e| e.to_string())?;
        let mut builder = builder.with_inter_threads(1).map_err(|e| e.to_string())?;
        let session = builder
            .commit_from_file(model)
            .map_err(|e| format!("cannot load the model: {e}"))?;
        // the environment must outlive the session: keep it for the process
        std::mem::forget(env);
        Ok(Tagger { session })
    }

    /// The image vector of a photo (JPEG, PNG, HEIC, WebP, TIFF, BMP, GIF:
    /// whatever Windows can decode).
    pub fn embed(&mut self, bytes: &[u8]) -> Result<Vec<f32>, String> {
        let pixels = pixels(bytes).map_err(|e| format!("cannot decode the photo: {}", e.message()))?;
        let input = ort::value::Tensor::from_array(([1usize, 3, SIDE as usize, SIDE as usize], pixels)).map_err(|e| e.to_string())?;
        let outputs = self.session.run(ort::inputs!["pixel_values" => input]).map_err(|e| e.to_string())?;
        let (_, embeds) = outputs["image_embeds"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        Ok(embeds.to_vec())
    }

    /// The photo's tags: `(label, share)`, best first.
    pub fn tags(&mut self, bytes: &[u8]) -> Result<Vec<(usize, f32)>, String> {
        Ok(best_labels(&self.embed(bytes)?))
    }
}

/// Decodes, turns by EXIF, scales the short side to 224, crops the middle
/// 224x224, and lays it out as CLIP wants it: 3 x 224 x 224, normalised.
fn pixels(bytes: &[u8]) -> windows::core::Result<Vec<f32>> {
    let stream = crate::ocr::memory_stream(bytes)?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
    let (ow, oh) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
    let scale = SIDE as f64 / ow.min(oh).max(1) as f64;
    // the transform scales the stored (not yet turned) image
    let (sw, sh) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    let transform = BitmapTransform::new()?;
    transform.SetScaledWidth(((sw as f64 * scale).round() as u32).max(SIDE))?;
    transform.SetScaledHeight(((sh as f64 * scale).round() as u32).max(SIDE))?;
    transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    let bitmap = decoder
        .GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Ignore,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )?
        .join()?;
    let (w, h) = (bitmap.PixelWidth()? as usize, bitmap.PixelHeight()? as usize);
    let buffer = Buffer::Create((w * h * 4) as u32)?;
    bitmap.CopyToBuffer(&buffer)?;
    let reader = DataReader::FromBuffer(&buffer)?;
    let mut bgra = vec![0u8; buffer.Length()? as usize];
    reader.ReadBytes(&mut bgra)?;
    let side = SIDE as usize;
    let (left, top) = (w.saturating_sub(side) / 2, h.saturating_sub(side) / 2);
    let mut out = vec![0f32; 3 * side * side];
    for y in 0..side {
        for x in 0..side {
            let (sx, sy) = ((left + x).min(w.saturating_sub(1)), (top + y).min(h.saturating_sub(1)));
            let p = (sy * w + sx) * 4;
            let (b, g, r) = (bgra.get(p).copied().unwrap_or(0), bgra.get(p + 1).copied().unwrap_or(0), bgra.get(p + 2).copied().unwrap_or(0));
            for (c, v) in [r, g, b].into_iter().enumerate() {
                out[c * side * side + y * side + x] = (v as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    Ok(out)
}

/// A picture to show (the preview): decoded by Windows (JPEG, PNG, HEIC,
/// WebP, TIFF, BMP, GIF), turned by EXIF, at most `max_side` pixels on its
/// longer side. `(width, height, RGBA)`.
pub fn picture(bytes: &[u8], max_side: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let run = || -> windows::core::Result<(u32, u32, Vec<u8>)> {
        let stream = crate::ocr::memory_stream(bytes)?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        let (ow, oh) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
        let scale = (max_side as f64 / ow.max(oh).max(1) as f64).min(1.0);
        let (sw, sh) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
        let transform = BitmapTransform::new()?;
        transform.SetScaledWidth(((sw as f64 * scale).round() as u32).max(1))?;
        transform.SetScaledHeight(((sh as f64 * scale).round() as u32).max(1))?;
        transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
        let bitmap = decoder
            .GetSoftwareBitmapTransformedAsync(
                BitmapPixelFormat::Rgba8,
                BitmapAlphaMode::Straight,
                &transform,
                ExifOrientationMode::RespectExifOrientation,
                ColorManagementMode::ColorManageToSRgb,
            )?
            .join()?;
        let (w, h) = (bitmap.PixelWidth()? as u32, bitmap.PixelHeight()? as u32);
        let buffer = Buffer::Create(w * h * 4)?;
        bitmap.CopyToBuffer(&buffer)?;
        let reader = DataReader::FromBuffer(&buffer)?;
        let mut rgba = vec![0u8; buffer.Length()? as usize];
        reader.ReadBytes(&mut rgba)?;
        Ok((w, h, rgba))
    };
    run().map_err(|e| e.message().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_label_has_eight_names_and_a_vector() {
        let (n, d, v) = vectors();
        assert_eq!(n, labels().len());
        assert_eq!((d, v.len()), (512, n * 512));
        for l in labels() {
            assert_eq!(l.len(), 8, "{l:?}");
            assert!(l.iter().all(|s| !s.is_empty()), "{l:?}");
        }
    }

    #[test]
    fn a_label_vector_finds_its_own_label() {
        let (_, d, v) = vectors();
        let dog = labels().iter().position(|l| l[0] == "dog").unwrap();
        let tags = best_labels(&v[dog * d..(dog + 1) * d]);
        assert_eq!(tags.first().map(|t| t.0), Some(dog));
        assert_eq!(index_text(&tags[..1]), "dog Hund perro chien cane pies cachorro câine");
    }
}
