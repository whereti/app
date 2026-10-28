//! OCR, pictures and scanned PDFs read by the sandboxed worker, with its
//! restricted token and, as in the service, a window station and desktop
//! of its own (Windows' PDF renderer loads user32, which needs one).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use whereti_extract::worker::{use_own_desktop, Sandbox};

fn sandbox() -> Sandbox {
    use_own_desktop();
    Sandbox::new(PathBuf::from(env!("CARGO_BIN_EXE_whereti-extract")))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)).unwrap()
}

#[test]
fn the_restricted_worker_reads_pictures_and_scans() {
    let s = sandbox();
    assert!(s.restricted(), "{}", s.sandbox_report());
    if !whereti_extract::ocr_available() {
        eprintln!("no OCR language on this Windows: skipped");
        return;
    }
    let png = s.extract_bytes("png", fixture("invoice.png"), 100_000).unwrap();
    assert!(png.text.to_lowercase().contains("invoice"), "{}", png.text);
    let pdf = s.extract_bytes("pdf", fixture("scanned.pdf"), 100_000).unwrap();
    assert_eq!(pdf.text.to_lowercase().matches("20417").count(), 2, "{}", pdf.text);
    assert!(!pdf.truncated);
}

#[test]
fn ocr_stops_inside_the_callers_time() {
    if !whereti_extract::ocr_available() {
        return;
    }
    // a scan of many pages, with little time: what was read comes back
    // before the timeout instead of nothing
    let one = fixture("scanned.pdf");
    let mut s = sandbox();
    s.timeout = Duration::from_millis(2500);
    // the check of the worker (which extracting never waits for) done
    // first, so its probes do not compete with the timed part
    let _ = s.sandbox_report();
    let t = Instant::now();
    let got = s.extract_bytes("pdf", one, 100_000);
    assert!(t.elapsed() < Duration::from_millis(2500), "{:?}", t.elapsed());
    assert!(got.is_ok(), "{got:?}");
}
