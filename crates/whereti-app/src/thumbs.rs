//! Picture thumbnails for the Images grid, from Windows' own thumbnail
//! cache (what Explorer shows: JPEG, PNG, HEIC, RAW with its codec, video
//! frames…). Fetched on one background thread, only for the tiles on
//! screen now (a tile scrolled past is not fetched any more); the UI
//! thread only turns finished pixels into textures, a few per frame.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};

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

/// What the thumbnail thread is to fetch: the tiles on screen without one,
/// in order, and the one it is fetching (until the window has its result).
#[derive(Default)]
struct Wanted {
    paths: Vec<String>,
    fetching: Option<String>,
}

pub struct Thumbs {
    cache: HashMap<String, Option<TextureHandle>>,
    /// Oldest first, to drop beyond `KEEP`.
    order: VecDeque<String>,
    wanted: Arc<(Mutex<Wanted>, Condvar)>,
    /// The tiles of this frame without a thumbnail yet.
    asked: Vec<String>,
    /// The grid was drawn this frame.
    drawn: bool,
    rx: Receiver<(String, Pixels)>,
}

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Thumbs {
        let wanted: Arc<(Mutex<Wanted>, Condvar)> = Arc::default();
        let (done, rx) = channel();
        let w = wanted.clone();
        std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || {
                // SAFETY: once, on this thread, before any shell call.
                let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                loop {
                    // the first tile on screen that has none yet
                    let path = {
                        let (lock, cv) = &*w;
                        let mut wanted = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while wanted.paths.is_empty() {
                            wanted = cv.wait(wanted).unwrap_or_else(|e| e.into_inner());
                        }
                        let path = wanted.paths.remove(0);
                        wanted.fetching = Some(path.clone());
                        path
                    };
                    let pixels = thumbnail(&path);
                    if done.send((path, pixels)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("a thread");
        Thumbs { cache: HashMap::new(), order: VecDeque::new(), wanted, asked: Vec::new(), drawn: false, rx }
    }

    /// The thumbnail of `path`; `None` while it is coming (or there is none).
    /// Asked for until the end of the frame (`end_frame`).
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
        self.asked.push(path.to_string());
        None
    }

    /// After the grid is drawn: the thread fetches the tiles on screen now,
    /// and nothing else.
    pub fn end_frame(&mut self) {
        self.drawn = true;
        let (lock, cv) = &*self.wanted;
        let mut wanted = lock.lock().unwrap_or_else(|e| e.into_inner());
        let fetching = wanted.fetching.clone();
        self.asked.retain(|p| Some(p) != fetching.as_ref());
        if wanted.paths != self.asked {
            wanted.paths = std::mem::take(&mut self.asked);
            cv.notify_one();
        }
        self.asked.clear();
    }

    /// Once per frame, after everything is drawn: a frame without the grid
    /// stops the fetching, and what still comes is dropped.
    pub fn after_frame(&mut self) {
        if !std::mem::take(&mut self.drawn) {
            self.stop();
        }
    }

    /// Nothing more is fetched; results on their way are dropped.
    fn stop(&mut self) {
        self.asked.clear();
        {
            let mut wanted = self.wanted.0.lock().unwrap_or_else(|e| e.into_inner());
            wanted.paths.clear();
            wanted.fetching = None;
        }
        while self.rx.try_recv().is_ok() {}
    }

    /// Call once per frame the grid is drawn, before it: turns finished
    /// thumbnails into textures.
    pub fn receive(&mut self, ctx: &egui::Context) {
        for _ in 0..PER_FRAME {
            let Ok((path, pixels)) = self.rx.try_recv() else { return };
            {
                // it may be asked for again (once dropped from the cache)
                let mut wanted = self.wanted.0.lock().unwrap_or_else(|e| e.into_inner());
                if wanted.fetching.as_ref() == Some(&path) {
                    wanted.fetching = None;
                }
            }
            // fetched twice (asked again after a stop): kept once
            if self.cache.contains_key(&path) {
                continue;
            }
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

    /// Drops every texture and stops fetching (the window was hidden).
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
        self.stop();
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
