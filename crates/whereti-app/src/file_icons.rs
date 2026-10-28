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
/// Icons kept while the window is hidden.
const MAX_KEPT: usize = 2000;

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

    /// Called when the window hides: programs' own icons are kept per file,
    /// so after weeks in the tray the cache would only grow.
    pub fn trim(&mut self) {
        if self.cache.len() > MAX_KEPT {
            self.cache.clear();
        }
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

    /// The icon of an app, a Control Panel item, or (for `ms-settings:`
    /// pages) the Settings app.
    pub fn get_shell(&mut self, ctx: &egui::Context, target: &str) -> Option<TextureHandle> {
        const SETTINGS_APP: &str =
            r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";
        let target = if target.starts_with("ms-settings:") { SETTINGS_APP } else { target };
        let key = format!("shell:{}", target.to_lowercase());
        if let Some(t) = self.cache.get(&key) {
            return t.clone();
        }
        if self.fetched_this_frame >= PER_FRAME {
            ctx.request_repaint();
            return None;
        }
        self.fetched_this_frame += 1;
        let tex = self.fetch_shell(target).map(|(rgba, size)| {
            let image = ColorImage::from_rgba_unmultiplied([size, size], &rgba);
            ctx.load_texture(format!("icon:{key}"), image, TextureOptions::LINEAR)
        });
        self.cache.insert(key, tex.clone());
        tex
    }

    fn fetch_shell(&self, target: &str) -> Option<(Vec<u8>, usize)> {
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{SHParseDisplayName, SHGFI_PIDL};
        let list = self.list.as_ref()?;
        let mut pidl = std::ptr::null_mut();
        // SAFETY: the PIDL Windows allocates is freed below; the info struct
        // has the right size; the icon from GetIcon is destroyed.
        unsafe {
            SHParseDisplayName(&HSTRING::from(target), None, &mut pidl, 0, None).ok()?;
            let mut info = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                windows::core::PCWSTR(pidl as *const u16),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_PIDL | SHGFI_SYSICONINDEX,
            );
            CoTaskMemFree(Some(pidl as *const _));
            if ok == 0 {
                return None;
            }
            let icon = list.GetIcon(info.iIcon, ILD_TRANSPARENT.0).ok()?;
            let out = icon_rgba(icon);
            let _ = DestroyIcon(icon);
            out
        }
    }

    fn fetch(&self, path: &str, is_dir: bool) -> Option<(Vec<u8>, usize)> {
        let list = self.list.as_ref()?;
        // a program's own icon is read from the file: not from a share, where
        // a NAS asleep would hold the window for its timeout, nor for a
        // shortcut to a share (Windows reads its icon from the target)
        let own_icon = !is_dir && has_own_icon(path) && !crate::win::is_network_path(path) && !link_to_network(path);
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

/// A shortcut (.lnk) whose target or icon is on the network, or that
/// cannot be read: it gets the plain shortcut icon. Reads only the
/// shortcut itself, never what it points to.
fn link_to_network(path: &str) -> bool {
    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    if !path.to_ascii_lowercase().ends_with(".lnk") {
        return false;
    }
    let text = |w: &[u16]| String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]);
    // SAFETY: COM calls on the UI thread (COM is initialised by winit) on
    // objects owned here; the buffers are of their stated size.
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return true };
        let Ok(file) = link.cast::<IPersistFile>() else { return true };
        if file.Load(&HSTRING::from(path), STGM_READ).is_err() {
            return true;
        }
        let (mut target, mut icon, mut index) = ([0u16; 1024], [0u16; 1024], 0);
        let _ = link.GetPath(&mut target, std::ptr::null_mut(), 0);
        let _ = link.GetIconLocation(&mut icon, &mut index);
        [text(&target), text(&icon)].iter().any(|p| !p.is_empty() && crate::win::is_network_path(p))
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
        // a bottom-up or top-down bitmap: the height may be negative
        let (w, h) = (bm.bmWidth.unsigned_abs() as usize, bm.bmHeight.unsigned_abs() as usize);
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
        let no_alpha = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
        if no_alpha && !ii.hbmMask.is_invalid() {
            let mut mask = vec![0u8; w * h * 4];
            let mut mbmi = bmi;
            GetDIBits(dc, ii.hbmMask, 0, h as u32, Some(mask.as_mut_ptr() as *mut _), &mut mbmi, DIB_RGB_COLORS);
            for (p, m) in px.as_chunks_mut::<4>().0.iter_mut().zip(mask.as_chunks::<4>().0) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        let _ = DeleteDC(dc);
        free(&ii);
        if lines == 0 || w == 0 || w != h {
            return None;
        }
        for p in px.as_chunks_mut::<4>().0 {
            p.swap(0, 2); // BGRA → RGBA
        }
        Some((px, w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shortcut to `target`, saved in the temporary folder.
    fn shortcut(name: &str, target: &str) -> String {
        use windows::core::Interface;
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        let path = std::env::temp_dir().join(format!("whereti-{}-{name}.lnk", std::process::id()));
        // SAFETY: COM set up on this test thread; objects owned here.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(&HSTRING::from(target)).unwrap();
            link.cast::<IPersistFile>().unwrap().Save(&HSTRING::from(path.as_os_str()), true).unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn shortcuts_to_a_share_get_the_plain_icon() {
        let share = shortcut("share", r"\\nas\photos\holiday.jpg");
        let local = shortcut("local", r"C:\Windows\notepad.exe");
        assert!(link_to_network(&share));
        assert!(!link_to_network(&local));
        assert!(!link_to_network(r"C:\Windows\notepad.exe"));
        let _ = std::fs::remove_file(share);
        let _ = std::fs::remove_file(local);
    }
}
