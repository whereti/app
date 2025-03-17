//! The real file icons, exactly as Explorer shows them (Word, Excel, PDF
//! reader, a program's own icon), from the Windows system image list.
//!
//! Icons are per extension ("what a .docx looks like") and cached for the
//! life of the app; programs, shortcuts and .ico files get their own icon.
//! At most a few new icons are fetched per frame, so a screen of new results
//! never stalls typing: rows draw a plain tile until theirs is ready.

use std::collections::HashMap;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHIL_EXTRALARGE,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// New icons fetched per frame.
const PER_FRAME: usize = 6;

pub struct FileIcons {
    cache: HashMap<String, Option<TextureHandle>>,
    fetched_this_frame: usize,
    list: Option<IImageList>,
}

impl FileIcons {
    pub fn new() -> FileIcons {
        // SAFETY: plain shell call; the list lives as long as the process.
        let list = unsafe { SHGetImageList::<IImageList>(SHIL_EXTRALARGE as i32) }.ok();
        FileIcons { cache: HashMap::new(), fetched_this_frame: 0, list }
    }

    /// Call once per frame.
    pub fn begin_frame(&mut self) {
        self.fetched_this_frame = 0;
    }

    /// The icon for `path`; `None` while not fetched yet (or none exists).
    pub fn get(&mut self, ctx: &egui::Context, path: &str, is_dir: bool) -> Option<TextureHandle> {
        let key = cache_key(path, is_dir);
        if let Some(t) = self.cache.get(&key) {
            return t.clone();
        }
        if self.fetched_this_frame >= PER_FRAME {
            ctx.request_repaint(); // the rest next frame
            return None;
        }
        self.fetched_this_frame += 1;
        let tex = self.fetch(path, is_dir).map(|(rgba, size)| {
            let image = ColorImage::from_rgba_unmultiplied([size, size], &rgba);
            ctx.load_texture(format!("icon:{key}"), image, TextureOptions::LINEAR)
        });
        self.cache.insert(key, tex.clone());
        tex
    }

    fn fetch(&self, path: &str, is_dir: bool) -> Option<(Vec<u8>, usize)> {
        let list = self.list.as_ref()?;
        let own_icon = !is_dir && has_own_icon(path);
        let (name, attrs, flags) = if is_dir {
            (HSTRING::from("folder"), FILE_ATTRIBUTE_DIRECTORY, SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES)
        } else if own_icon {
            (HSTRING::from(path), FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_SYSICONINDEX)
        } else {
            let ext = path.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('\\')).unwrap_or("");
            (HSTRING::from(format!("file.{ext}")), FILE_ATTRIBUTE_NORMAL, SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES)
        };
        let mut info = SHFILEINFOW::default();
        // SAFETY: valid string and out-structure of the right size.
        let ok = unsafe { SHGetFileInfoW(&name, attrs, Some(&mut info), size_of::<SHFILEINFOW>() as u32, flags) };
        if ok == 0 {
            return None;
        }
        // SAFETY: index from the same system image list; the icon is destroyed below.
        let icon = unsafe { list.GetIcon(info.iIcon, ILD_TRANSPARENT.0) }.ok()?;
        let out = icon_rgba(icon);
        // SAFETY: we own the icon returned by GetIcon.
        unsafe {
            let _ = DestroyIcon(icon);
        }
        out
    }
}

/// Programs, shortcuts and icon files look different one from another.
fn has_own_icon(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "appref-ms" | "msc" | "cpl" | "scr")
}

fn cache_key(path: &str, is_dir: bool) -> String {
    if is_dir {
        return "<folder>".into();
    }
    if has_own_icon(path) {
        return path.to_lowercase();
    }
    path.rsplit_once('.').map(|(_, e)| format!(".{}", e.to_ascii_lowercase())).filter(|e| !e.contains('\\')).unwrap_or_default()
}

/// The icon's pixels as RGBA, with its real transparency.
fn icon_rgba(icon: HICON) -> Option<(Vec<u8>, usize)> {
    // SAFETY: standard GDI calls on bitmaps we own and free here.
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(icon, &mut ii).ok()?;
        let free = |ii: &ICONINFO| {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        };
        let mut bm = BITMAP::default();
        if ii.hbmColor.is_invalid()
            || GetObjectW(HGDIOBJ(ii.hbmColor.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0
        {
            free(&ii);
            return None;
        }
        let (w, h) = (bm.bmWidth as usize, bm.bmHeight as usize);
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
        let lines = GetDIBits(dc, ii.hbmColor, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
        // Old icons have no alpha channel: take transparency from the mask.
        let no_alpha = px.chunks_exact(4).all(|p| p[3] == 0);
        if no_alpha && !ii.hbmMask.is_invalid() {
            let mut mask = vec![0u8; w * h * 4];
            let mut mbmi = bmi;
            GetDIBits(dc, ii.hbmMask, 0, h as u32, Some(mask.as_mut_ptr() as *mut _), &mut mbmi, DIB_RGB_COLORS);
            for (p, m) in px.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        let _ = DeleteDC(dc);
        free(&ii);
        if lines == 0 || w == 0 || w != h {
            return None;
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA → RGBA
        }
        Some((px, w))
    }
}
