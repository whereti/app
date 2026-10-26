//! Picture thumbnails for the Images grid, from Windows' own thumbnail
//! cache (what Explorer shows: JPEG, PNG, HEIC, RAW with its codec, video
//! frames…). Fetched on one background thread, newest request first, so
//! the tiles on screen come before those scrolled past; the UI thread only
//! turns finished pixels into textures, a few per frame.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use windows::core::HSTRING;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_RESIZETOFIT};

/// Longer side asked from Windows (tiles are ~170 px, sharp on 150% screens).
const SIDE: i32 = 256;
/// Thumbnails kept as textures (~250 KB each); the oldest go beyond this.
const KEEP: usize = 240;
/// Textures made per frame.
const PER_FRAME: usize = 8;

type Pixels = Option<(usize, usize, Vec<u8>)>;

pub struct Thumbs {
    cache: HashMap<String, Option<TextureHandle>>,
    /// Oldest first, to drop beyond `KEEP`.
    order: VecDeque<String>,
    asked: HashSet<String>,
    tx: Sender<String>,
    rx: Receiver<(String, Pixels)>,
}

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Thumbs {
        let (tx, jobs) = channel::<String>();
        let (done, rx) = channel();
        std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || {
                // SAFETY: once, on this thread, before any shell call.
                let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let mut stack: Vec<String> = Vec::new();
                loop {
                    // take everything asked so far; the newest first
                    if stack.is_empty() {
                        match jobs.recv() {
                            Ok(p) => stack.push(p),
                            Err(_) => return,
                        }
                    }
                    loop {
                        match jobs.try_recv() {
                            Ok(p) => stack.push(p),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    let Some(path) = stack.pop() else { continue };
                    let pixels = thumbnail(&path);
                    if done.send((path, pixels)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("a thread");
        Thumbs { cache: HashMap::new(), order: VecDeque::new(), asked: HashSet::new(), tx, rx }
    }

    /// The thumbnail of `path`; `None` while it is coming (or there is none).
    pub fn get(&mut self, path: &str) -> Option<TextureHandle> {
        if let Some(t) = self.cache.get(path) {
            return t.clone();
        }
        // a NAS asleep would hold the one thumbnail thread for its timeout:
        // network files show their icon
        if crate::win::is_network_path(path) {
            self.cache.insert(path.to_string(), None);
            return None;
        }
        if self.asked.insert(path.to_string()) {
            let _ = self.tx.send(path.to_string());
        }
        None
    }

    /// Call once per frame: turns finished thumbnails into textures.
    pub fn receive(&mut self, ctx: &egui::Context) {
        for _ in 0..PER_FRAME {
            let Ok((path, pixels)) = self.rx.try_recv() else { return };
            self.asked.remove(&path);
            let tex = pixels.map(|(w, h, rgba)| {
                let image = ColorImage::from_rgba_premultiplied([w, h], &rgba);
                ctx.load_texture(format!("thumb:{path}"), image, TextureOptions::LINEAR)
            });
            self.cache.insert(path.clone(), tex);
            self.order.push_back(path);
            while self.order.len() > KEEP {
                if let Some(old) = self.order.pop_front() {
                    self.cache.remove(&old);
                }
            }
        }
        ctx.request_repaint(); // more may be waiting
    }

    /// A file changed or went away: fetch it again next time.
    pub fn forget(&mut self, path: &str) {
        self.cache.remove(path);
        self.order.retain(|p| p != path);
    }

    /// Drops every texture (the window was hidden).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
        self.asked.clear();
    }
}

/// Windows' thumbnail of a file, as premultiplied RGBA.
fn thumbnail(path: &str) -> Pixels {
    // SAFETY: shell and GDI calls on objects owned and freed here.
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None::<&IBindCtx>).ok()?;
        let bitmap = factory.GetImage(SIZE { cx: SIDE, cy: SIDE }, SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK).ok()?;
        let out = pixels(bitmap);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        out
    }
}

fn pixels(bitmap: HBITMAP) -> Pixels {
    // SAFETY: standard GDI calls on a bitmap we own.
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(HGDIOBJ(bitmap.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth.unsigned_abs() as usize, bm.bmHeight.unsigned_abs() as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; w * h * 4];
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        if lines == 0 {
            return None;
        }
        // photos come without alpha (all 0): opaque; icons come premultiplied
        let opaque = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
        for p in px.as_chunks_mut::<4>().0 {
            p.swap(0, 2); // BGRA -> RGBA
            if opaque {
                p[3] = 255;
            }
        }
        Some((w, h, px))
    }
}
