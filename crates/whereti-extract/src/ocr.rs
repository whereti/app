//! Text in pictures and scanned PDFs (PLAN.md §41), with the OCR built into
//! Windows (`Windows.Media.Ocr`): on the PC, offline, nothing to install.
//!
//! - Pictures are decoded by Windows (JPEG, PNG, TIFF, GIF, BMP, WebP, and
//!   HEIC where its codec is installed), turned the way the camera held them
//!   (EXIF), and shrunk if larger than the OCR accepts.
//! - A PDF whose own text is (almost) empty is a scan: its pages are drawn by
//!   `Windows.Data.Pdf` at about 200 dpi and read one by one, within a time
//!   budget, so a 300-page scan cannot hold the indexer for minutes.
//! - Photos without real text (people, landscapes) give a few stray letters;
//!   those are dropped instead of filling the index with noise.
//!
//! Runs inside whereti-extract.exe, so the Job Object's memory cap and the
//! caller's timeout apply here too.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
    ExifOrientationMode, SoftwareBitmap,
};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream, IRandomAccessStream};

/// Pages of a scanned PDF read at most.
const MAX_PAGES: u32 = 50;
/// Total time for one file; what was read by then is kept. The worker's
/// caller may allow less (`--budget`), and then that is the limit.
const BUDGET: Duration = Duration::from_secs(20);
/// About what one page takes to draw and read.
const PAGE_TIME: Duration = Duration::from_millis(1500);
/// Width pages are drawn at: about 200 dpi for A4, which OCR reads well.
const PAGE_WIDTH: u32 = 1700;
/// A PDF with less text than this per page is treated as a scan.
const SCAN_CHARS_PER_PAGE: usize = 40;

pub(crate) struct Ocr {
    engine: OcrEngine,
    max_dim: u32,
}

/// OCR languages to try first (see `crate::set_ocr_languages`).
static LANGUAGES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn set_languages(tags: &[&str]) {
    *LANGUAGES.lock().unwrap_or_else(|e| e.into_inner()) = tags.iter().map(|t| t.to_string()).collect();
}

impl Ocr {
    /// The OCR in the first language the caller gave (`set_languages`) that
    /// Windows has, else in the languages of this process's profile (in
    /// the service: the system's), else in the first language Windows has.
    /// `None` if Windows has no OCR at all.
    pub(crate) fn new() -> Option<Ocr> {
        // SAFETY: initialises WinRT for this thread; harmless if already done.
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
        let wanted = LANGUAGES.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let engine = wanted
            .iter()
            .find_map(|tag| {
                let lang = Language::CreateLanguage(&windows::core::HSTRING::from(tag.as_str())).ok()?;
                if !OcrEngine::IsLanguageSupported(&lang).ok()? {
                    return None;
                }
                OcrEngine::TryCreateFromLanguage(&lang).ok()
            })
            .or_else(|| OcrEngine::TryCreateFromUserProfileLanguages().ok())
            .or_else(|| {
                let langs = OcrEngine::AvailableRecognizerLanguages().ok()?;
                let first: Language = langs.GetAt(0).ok()?;
                OcrEngine::TryCreateFromLanguage(&first).ok()
            })?;
        let max_dim = OcrEngine::MaxImageDimension().unwrap_or(10_000);
        Some(Ocr { engine, max_dim })
    }

    /// The text in a picture.
    pub(crate) fn picture(&self, bytes: &[u8]) -> windows::core::Result<String> {
        let stream = memory_stream(bytes)?;
        let bitmap = self.decode(&stream)?;
        let text = self.read(&bitmap)?;
        Ok(keep_if_words(text))
    }

    /// The text of a scanned PDF, page by page; `None` when the PDF has
    /// enough text of its own (`own_chars` letters and digits, all its pages
    /// read). `truncated` when the page limit or the time stopped it.
    pub(crate) fn pdf_if_scanned(&self, bytes: &[u8], own_chars: usize) -> windows::core::Result<Option<(String, bool)>> {
        let until = until();
        let stream = memory_stream(bytes)?;
        let doc = PdfDocument::LoadFromStreamAsync(&stream)?.join()?;
        let pages = doc.PageCount()?;
        if !looks_scanned(own_chars, pages as usize) {
            return Ok(None);
        }
        let mut out = String::new();
        let mut truncated = pages > MAX_PAGES;
        for i in 0..pages.min(MAX_PAGES) {
            // a page takes about a second: none is started that would end late
            if Instant::now() + PAGE_TIME >= until {
                truncated = true;
                break;
            }
            let page = doc.GetPage(i)?;
            let options = PdfPageRenderOptions::new()?;
            options.SetDestinationWidth(PAGE_WIDTH)?;
            let image = InMemoryRandomAccessStream::new()?;
            page.RenderWithOptionsToStreamAsync(&image, &options)?.join()?;
            image.Seek(0)?;
            let bitmap = self.decode(&image.cast()?)?;
            let text = self.read(&bitmap)?;
            if !text.trim().is_empty() {
                out.push_str(&text);
                out.push_str("\n\n");
            }
        }
        Ok(Some((keep_if_words(out), truncated)))
    }

    /// Decodes to BGRA, turned by EXIF, no larger than the OCR accepts.
    fn decode(&self, stream: &IRandomAccessStream) -> windows::core::Result<SoftwareBitmap> {
        let decoder = BitmapDecoder::CreateAsync(stream)?.join()?;
        let (w, h) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
        let transform = BitmapTransform::new()?;
        let largest = w.max(h);
        if largest > self.max_dim {
            let scale = self.max_dim as f64 / largest as f64;
            // the transform scales the stored (not yet turned) image
            let (sw, sh) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
            transform.SetScaledWidth(((sw as f64 * scale) as u32).max(1))?;
            transform.SetScaledHeight(((sh as f64 * scale) as u32).max(1))?;
            transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
        }
        decoder
            .GetSoftwareBitmapTransformedAsync(
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Premultiplied,
                &transform,
                ExifOrientationMode::RespectExifOrientation,
                ColorManagementMode::DoNotColorManage,
            )?
            .join()
    }

    fn read(&self, bitmap: &SoftwareBitmap) -> windows::core::Result<String> {
        let result = self.engine.RecognizeAsync(bitmap)?.join()?;
        // one line per OCR line, so snippets and phrases read naturally
        let mut text = String::new();
        for line in result.Lines()? {
            text.push_str(&line.Text()?.to_string_lossy());
            text.push('\n');
        }
        Ok(text)
    }
}

/// A scan: a PDF whose own text (`chars` letters and digits, from all its
/// pages) is less than a few words per page.
pub(crate) fn looks_scanned(chars: usize, pages: usize) -> bool {
    chars < SCAN_CHARS_PER_PAGE * pages.max(1)
}

/// When OCR must stop: its own budget, or sooner if the caller stops
/// waiting sooner (see `crate::within`).
fn until() -> Instant {
    let own = Instant::now() + BUDGET;
    crate::deadline().map_or(own, |d| d.min(own))
}

/// What Windows' OCR, picture decoder and PDF renderer can do in this
/// process (the worker, after its lockdown): `ocr=1 wic=1 pdf=1`, 0 for
/// what fails; then the step that loaded user32 (which needs a desktop).
pub(crate) fn probe() -> String {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
    }
    let name: Vec<u16> = "user32.dll".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name; only asks whether it is loaded.
    let user32 = || !unsafe { GetModuleHandleW(name.as_ptr()) }.is_null();
    let mut loaded_by = if user32() { "start" } else { "none" };
    let mut step = |what: &'static str| {
        if loaded_by == "none" && user32() {
            loaded_by = what;
        }
    };
    // SAFETY: initialises WinRT for this thread; harmless if already done.
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
    step("winrt");
    let ocr = Ocr::new().is_some();
    step("ocr");
    // a 2x2 BMP
    let mut bmp = vec![0u8; 54 + 16];
    bmp[0..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&70u32.to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&2i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&2i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    let wic = (|| -> windows::core::Result<()> {
        let decoder = BitmapDecoder::CreateAsync(&memory_stream(&bmp)?)?.join()?;
        decoder.GetSoftwareBitmapAsync()?.join()?;
        Ok(())
    })()
    .is_ok();
    step("wic");
    let pdf = (|| -> windows::core::Result<()> {
        let doc = PdfDocument::LoadFromStreamAsync(&memory_stream(&tiny_pdf())?)?.join()?;
        let image = InMemoryRandomAccessStream::new()?;
        doc.GetPage(0)?.RenderToStreamAsync(&image)?.join()?;
        Ok(())
    })()
    .is_ok();
    step("pdf");
    format!("ocr={} wic={} pdf={} user32:{loaded_by}", ocr as u8, wic as u8, pdf as u8)
}

// A one-page PDF with nothing on it.
fn tiny_pdf() -> Vec<u8> {
    let objects = [
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 72 72]/Resources<<>>>>",
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    pdf
}

pub(crate) fn memory_stream(bytes: &[u8]) -> windows::core::Result<IRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes)?;
    writer.StoreAsync()?.join()?;
    writer.FlushAsync()?.join()?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    stream.cast()
}

/// Keeps OCR text only if it has real words: at least three of three or
/// more letters. A photo of a face reads as "l .. I" and is dropped.
pub(crate) fn keep_if_words(text: String) -> String {
    let words = text.split_whitespace().filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3).count();
    if words >= 3 {
        text
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stray_letters_are_dropped() {
        assert_eq!(keep_if_words("l .. I\n,".into()), "");
        assert_eq!(keep_if_words("Total to pay 58.40\nDue date March".into()), "Total to pay 58.40\nDue date March");
    }

    #[test]
    fn scans_are_recognised() {
        assert!(looks_scanned(0, 3));
        assert!(looks_scanned(3, 1));
        assert!(!looks_scanned(160, 2));
    }
}
