//! OCR on real files: a picture and a two-page scanned PDF (an image per
//! page, no text of its own), both made from the same invoice.

use whereti_extract::{extract, kind_of, Kind};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)).unwrap()
}

fn has_invoice(text: &str) {
    let t = text.to_lowercase();
    for word in ["invoice", "20417", "february", "total to pay", "march"] {
        assert!(t.contains(word), "{word:?} not in {text:?}");
    }
}

#[test]
fn pictures_are_read() {
    assert_eq!(kind_of("JPG"), Some(Kind::Image));
    for name in ["invoice.png", "invoice.jpg"] {
        let ext = name.rsplit('.').next().unwrap();
        let got = extract(ext, &fixture(name), 100_000).unwrap();
        has_invoice(&got.text);
    }
}

#[test]
fn scanned_pdfs_are_read_page_by_page() {
    let got = extract("pdf", &fixture("scanned.pdf"), 100_000).unwrap();
    has_invoice(&got.text);
    // both pages
    assert_eq!(got.text.to_lowercase().matches("20417").count(), 2, "{}", got.text);
    assert!(!got.truncated);
}

#[test]
fn a_picture_without_text_gives_nothing() {
    // a plain grey picture: the OCR finds nothing, and nothing is kept
    let mut bmp = vec![0u8; 54 + 200 * 200 * 3];
    bmp[0..2].copy_from_slice(b"BM");
    let len = bmp.len() as u32;
    bmp[2..6].copy_from_slice(&len.to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&200i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&200i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
    for b in &mut bmp[54..] {
        *b = 0x90;
    }
    let got = extract("bmp", &bmp, 100_000).unwrap();
    assert_eq!(got.text, "");
}

#[test]
fn not_a_picture_is_an_error_not_a_crash() {
    assert!(extract("jpg", b"definitely not a jpeg", 100_000).is_err());
}
