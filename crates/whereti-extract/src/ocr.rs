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
/// Total time for one file; what was read by then is kept.
const BUDGET: Duration = Duration::from_secs(20);
/// Width pages are drawn at: about 200 dpi for A4, which OCR reads well.
const PAGE_WIDTH: u32 = 1700;
/// A PDF with less text than this per page is treated as a scan.
const SCAN_CHARS_PER_PAGE: usize = 40;

pub(crate) struct Ocr {
    engine: OcrEngine,
    max_dim: u32,
}

impl Ocr {
    /// The OCR in the user's language if Windows has it, else the first
    /// language Windows has. `None` if Windows has no OCR at all.
    pub(crate) fn new() -> Option<Ocr> {
        // SAFETY: initialises WinRT for this thread; harmless if already done.
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .ok()
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
    /// enough text of its own (`own`). `truncated` when the page limit or
    /// the time budget stopped it.
    pub(crate) fn pdf_if_scanned(&self, bytes: &[u8], own: &str) -> windows::core::Result<Option<(String, bool)>> {
        let started = Instant::now();
        let stream = memory_stream(bytes)?;
        let doc = PdfDocument::LoadFromStreamAsync(&stream)?.join()?;
        let pages = doc.PageCount()?;
        if !looks_scanned(own, pages as usize) {
            return Ok(None);
        }
        let mut out = String::new();
        let mut truncated = pages > MAX_PAGES;
        for i in 0..pages.min(MAX_PAGES) {
            if started.elapsed() >= BUDGET {
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

/// A scan: a PDF whose own text is less than a few words per page.
pub(crate) fn looks_scanned(text: &str, pages: usize) -> bool {
    let chars = text.chars().filter(|c| c.is_alphanumeric()).count();
    chars < SCAN_CHARS_PER_PAGE * pages.max(1)
}

fn memory_stream(bytes: &[u8]) -> windows::core::Result<IRandomAccessStream> {
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
        assert!(looks_scanned("", 3));
        assert!(looks_scanned("12 3", 1));
        assert!(!looks_scanned(&"word ".repeat(40), 2));
    }
}
