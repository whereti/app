//! Renders the logo into every size Windows asks for, and writes:
//!
//! - `media/icons/whereti-<size>.png` for 16…256 px;
//! - `media/icons/whereti.ico`: all sizes in one icon (PNG entries), for the
//!   .exe files (Explorer, taskbar, Start, Settings → Apps);
//! - `media/icons/whereti-<size>.rgba`: raw RGBA for the window and tray
//!   icons, loaded by the apps without an image decoder.
//!
//! Sizes up to 32 px come from `logo-small.svg` (thicker lines, no shadows,
//! which only blur when that small); larger ones from `logo.svg`.

use std::path::Path;

const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
const RAW_SIZES: [u32; 3] = [32, 64, 128];

fn main() {
    let media = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../media");
    let out = media.join("icons");
    std::fs::create_dir_all(&out).unwrap();
    let load = |name: &str| {
        let data = std::fs::read(media.join(name)).unwrap();
        resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).unwrap()
    };
    let (big, small) = (load("logo.svg"), load("logo-small.svg"));

    let mut pngs = Vec::new();
    for size in SIZES {
        let tree = if size <= 32 { &small } else { &big };
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
        let scale = size as f32 / tree.size().width();
        resvg::render(tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let png = pixmap.encode_png().unwrap();
        std::fs::write(out.join(format!("whereti-{size}.png")), &png).unwrap();
        if RAW_SIZES.contains(&size) {
            // straight (not premultiplied) RGBA, what egui and tray-icon expect
            let raw: Vec<u8> = pixmap
                .pixels()
                .iter()
                .flat_map(|p| {
                    let c = p.demultiply();
                    [c.red(), c.green(), c.blue(), c.alpha()]
                })
                .collect();
            std::fs::write(out.join(format!("whereti-{size}.rgba")), raw).unwrap();
        }
        if ICO_SIZES.contains(&size) {
            pngs.push((size, png));
        }
        println!("whereti-{size}.png");
    }

    // ICO: header, one directory entry per size, then the PNG images.
    let mut ico = Vec::new();
    ico.extend(0u16.to_le_bytes());
    ico.extend(1u16.to_le_bytes()); // icon
    ico.extend((pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len() as u32;
    for (size, png) in &pngs {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend([dim, dim, 0, 0]);
        ico.extend(1u16.to_le_bytes()); // planes
        ico.extend(32u16.to_le_bytes()); // bits per pixel
        ico.extend((png.len() as u32).to_le_bytes());
        ico.extend(offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &pngs {
        ico.extend(png);
    }
    std::fs::write(out.join("whereti.ico"), ico).unwrap();
    println!("whereti.ico");
}
