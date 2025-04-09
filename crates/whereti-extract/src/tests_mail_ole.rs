//! E-mail and old Office formats, on samples built here.

use std::io::{Cursor, Write};

use crate::extract;

fn text(ext: &str, bytes: &[u8]) -> String {
    extract(ext, bytes, 100_000).unwrap().text
}

/// A compound file with the given streams (and the storages they need).
fn compound(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut cfb = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for (path, data) in streams {
        if let Some((dir, _)) = path.rsplit_once('/') {
            if !dir.is_empty() && !cfb.exists(dir) {
                cfb.create_storage(dir).unwrap();
            }
        }
        let mut s = cfb.create_stream(path).unwrap();
        s.write_all(data).unwrap();
    }
    cfb.flush().unwrap();
    cfb.into_inner().into_inner()
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

#[test]
fn eml_headers_body_and_attachments() {
    let eml = "From: =?utf-8?B?QW5hIFBvcA==?= <ana@exemplu.ro>\r\n\
To: Ion <ion@exemplu.ro>\r\n\
Subject: =?utf-8?Q?Factur=C4=83_martie?=\r\n\
Date: Tue, 3 Mar 2026 10:15:00 +0200\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Bun=C4=83 ziua, ata=C8=99ez factura pentru martie.\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"factura-03.pdf\"\r\n\
Content-Disposition: attachment; filename=\"factura-03.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";
    let t = text("eml", eml.as_bytes());
    assert!(t.starts_with("Factură martie\nFrom: Ana Pop <ana@exemplu.ro>\nTo: Ion <ion@exemplu.ro>"), "{t}");
    assert!(t.contains("Date: 2026-03-03"), "{t}");
    assert!(t.contains("Bună ziua, atașez factura pentru martie."), "{t}");
    assert!(t.contains("factura-03.pdf"), "{t}");
}

#[test]
fn eml_with_only_html() {
    let eml = "Subject: Oferta\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Pre&#539;ul <b>total</b></p>";
    let t = text("eml", eml.as_bytes());
    assert!(t.contains("Prețul total"), "{t}");
}

#[test]
fn doc_piece_table_with_8bit_and_utf16_pieces_and_fields() {
    let mut word = vec![0u8; 1024];
    word[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
    word[2..4].copy_from_slice(&0x00C1u16.to_le_bytes()); // nFib: Word 97
    word[0x0A..0x0C].copy_from_slice(&0x0200u16.to_le_bytes()); // table stream: 1Table
    word[32..34].copy_from_slice(&14u16.to_le_bytes()); // csw
    word[62..64].copy_from_slice(&22u16.to_le_bytes()); // cslw
    word[152..154].copy_from_slice(&93u16.to_le_bytes()); // cbRgFcLcb
    let piece0 = b"Contract nr. 12\r";
    let piece1 = "Prețul total: \u{13} PAGE \u{14}1\u{15} lei\u{07}TVA\u{07}\u{07}";
    word[600..600 + piece0.len()].copy_from_slice(piece0);
    let p1 = utf16(piece1);
    word[700..700 + p1.len()].copy_from_slice(&p1);
    let n1 = piece1.encode_utf16().count() as u32;
    // the piece table: CPs 0, 16, 16+n1; then two PCDs
    let mut plc = Vec::new();
    for cp in [0u32, piece0.len() as u32, piece0.len() as u32 + n1] {
        plc.extend(cp.to_le_bytes());
    }
    for fc in [(600u32 * 2) | 0x4000_0000, 700u32] {
        plc.extend([0u8, 0]);
        plc.extend(fc.to_le_bytes());
        plc.extend([0u8, 0]);
    }
    let mut table = vec![0x02u8];
    table.extend((plc.len() as u32).to_le_bytes());
    table.extend(&plc);
    let at = 154 + 33 * 8;
    word[at..at + 4].copy_from_slice(&0u32.to_le_bytes()); // fcClx
    word[at + 4..at + 8].copy_from_slice(&(table.len() as u32).to_le_bytes()); // lcbClx
    let doc = compound(&[("/WordDocument", word.clone()), ("/1Table", table)]);
    assert_eq!(text("doc", &doc), "Contract nr. 12\nPrețul total: 1 lei\tTVA");

    // encrypted: refused as protected
    let mut locked = word;
    locked[0x0A..0x0C].copy_from_slice(&0x0300u16.to_le_bytes());
    let doc = compound(&[("/WordDocument", locked), ("/1Table", vec![])]);
    assert_eq!(extract("doc", &doc, 1000), Err(crate::ExtractError::Protected));
}

fn record(ver_inst: u16, kind: u16, body: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend(ver_inst.to_le_bytes());
    r.extend(kind.to_le_bytes());
    r.extend((body.len() as u32).to_le_bytes());
    r.extend(body);
    r
}

#[test]
fn ppt_slides_text_without_the_master() {
    let master = record(0x000F, 0x03F8, &record(0, 0x0FA0, &utf16("Click to edit Master title style")));
    let mut slide_body = record(0, 0x0FA0, &utf16("Ofertă 2026\rServicii"));
    slide_body.extend(record(0, 0x0FA8, b"Total 1.250 lei"));
    let slide = record(0x000F, 0x03EE, &slide_body);
    let mut doc_body = master;
    doc_body.extend(slide);
    let stream = record(0x000F, 0x03E8, &doc_body);
    let ppt = compound(&[("/PowerPoint Document", stream), ("/Current User", vec![0; 8])]);
    assert_eq!(text("ppt", &ppt), "Ofertă 2026\nServicii\nTotal 1.250 lei");
}

#[test]
fn msg_outlook_properties() {
    let p = |tag: &str| format!("/__substg1.0_{tag}001F");
    let msg = compound(&[
        ("/__properties_version1.0", vec![0; 32]),
        (&p("0037"), utf16("Factură martie")),
        (&p("0C1A"), utf16("Ana Pop")),
        (&p("5D01"), utf16("ana@exemplu.ro")),
        (&p("0E04"), utf16("Ion Ionescu")),
        (&p("1000"), utf16("Bună ziua,\r\natașez factura.")),
        ("/__recip_version1.0_#00000000/__substg1.0_39FE001F", utf16("ion@exemplu.ro")),
        ("/__attach_version1.0_#00000000/__substg1.0_3707001F", utf16("factura-03.pdf")),
    ]);
    let t = text("msg", &msg);
    assert!(t.starts_with("Factură martie\nFrom: Ana Pop <ana@exemplu.ro>\nTo: Ion Ionescu (ion@exemplu.ro)"), "{t}");
    assert!(t.contains("Bună ziua,\natașez factura."), "{t}");
    assert!(t.contains("factura-03.pdf"), "{t}");
}

#[test]
fn garbage_never_panics() {
    let mut seed = 11u32;
    for _ in 0..200 {
        let junk: Vec<u8> = (0..600)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        for ext in ["eml", "msg", "doc", "ppt"] {
            let _ = extract(ext, &junk, 1000);
        }
        // a valid container with garbage streams
        let c = compound(&[("/WordDocument", junk.clone()), ("/0Table", junk.clone()), ("/PowerPoint Document", junk.clone())]);
        for ext in ["doc", "ppt", "msg"] {
            let _ = extract(ext, &c, 1000);
        }
    }
}
