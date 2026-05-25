//! Native window touches: a title bar painted in the app's own colours (so
//! the window reads as one surface) and dark mode for the frame.

use std::ffi::c_void;

use eframe::egui::Color32;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};

/// The native window handle behind the eframe window.
pub fn hwnd(frame: &eframe::Frame) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let window = frame.winit_window()?;
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

fn colorref(c: Color32) -> COLORREF {
    COLORREF(c.r() as u32 | (c.g() as u32) << 8 | (c.b() as u32) << 16)
}

/// Caption, caption text and border in our palette; dark frame in dark mode;
/// rounded corners. Each call is best effort (older Windows ignores some).
pub fn style_frame(hwnd: HWND, dark: bool, caption: Color32, text: Color32, border: Color32) {
    unsafe fn set<T>(hwnd: HWND, attr: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, value: &T) {
        // SAFETY: value points to a T of the size the attribute expects.
        let _ = unsafe { DwmSetWindowAttribute(hwnd, attr, value as *const T as *const c_void, size_of::<T>() as u32) };
    }
    // SAFETY: valid window handle; values match each attribute's type.
    unsafe {
        set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &(dark as i32));
        set(hwnd, DWMWA_CAPTION_COLOR, &colorref(caption));
        set(hwnd, DWMWA_TEXT_COLOR, &colorref(text));
        set(hwnd, DWMWA_BORDER_COLOR, &colorref(border));
        set(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
    }
}

// ---------------------------------------------------------------- shell

use std::cell::RefCell;

use windows::core::{Interface, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{IBindCtx, IDataObject};
use windows::Win32::System::Ole::{IDropSource, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu3, IShellItem, RemoveWindowSubclass, SHCreateItemFromParsingName,
    SHDoDragDrop, SetWindowSubclass, BHID_DataObject, BHID_SFUIObject, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow, TrackPopupMenuEx, SW_SHOWNORMAL, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_MENUSELECT,
};

/// `CMIC_MASK_UNICODE` (same value as `SEE_MASK_UNICODE`; missing from the bindings).
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;

fn shell_item(path: &str) -> Result<IShellItem, String> {
    // SAFETY: valid path string; no bind context.
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None::<&IBindCtx>) }.map_err(|e| e.message())
}

/// Starts a shell drag of one file or folder: drop it on Explorer, the
/// desktop, a mail or chat window… Windows draws the drag image.
pub fn drag_file(hwnd: HWND, path: &str) -> Result<(), String> {
    let item = shell_item(path)?;
    // SAFETY: COM calls on the UI thread (OLE is initialised by winit).
    unsafe {
        let data: IDataObject = item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject).map_err(|e| e.message())?;
        SHDoDragDrop(Some(hwnd), &data, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK)
            .map(|_| ())
            .map_err(|e| e.message())
    }
}

thread_local! {
    /// The menu being shown, for owner-drawn items and lazy submenus.
    static ACTIVE_MENU: RefCell<Option<IContextMenu3>> = const { RefCell::new(None) };
}

/// Forwards the messages "Open with" / "Send to" need to fill their submenus.
unsafe extern "system" fn menu_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT) {
        let handled = ACTIVE_MENU.with(|m| {
            let m = m.borrow();
            let cm = m.as_ref()?;
            let mut result = LRESULT(0);
            // SAFETY: forwarding a message to the menu that owns it.
            unsafe { cm.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }.ok().map(|_| result)
        });
        if let Some(r) = handled {
            return r;
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Shows the real Explorer context menu for `path` at the mouse position
/// (Open with, Send to, Copy, Properties, and items other programs add).
pub fn context_menu(hwnd: HWND, path: &str) -> Result<(), String> {
    const FIRST_ID: u32 = 1;
    let item = shell_item(path)?;
    // SAFETY: COM and menu calls on the UI thread; every handle is released below.
    unsafe {
        let cm: IContextMenu = item.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject).map_err(|e| e.message())?;
        let menu = CreatePopupMenu().map_err(|e| e.message())?;
        let shown = (|| -> Result<(), String> {
            cm.QueryContextMenu(menu, 0, FIRST_ID, 0x7FFF, CMF_NORMAL).ok().map_err(|e| e.message())?;
            let mut pt = POINT::default();
            GetCursorPos(&mut pt).map_err(|e| e.message())?;
            let _ = SetForegroundWindow(hwnd);
            ACTIVE_MENU.with(|m| *m.borrow_mut() = cm.cast::<IContextMenu3>().ok());
            let _ = SetWindowSubclass(hwnd, Some(menu_subclass), 0x5748, 0);
            let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None).0 as u32;
            let _ = RemoveWindowSubclass(hwnd, Some(menu_subclass), 0x5748);
            ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
            if cmd >= FIRST_ID {
                let verb = (cmd - FIRST_ID) as usize;
                let info = CMINVOKECOMMANDINFOEX {
                    cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                    fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
                    hwnd,
                    lpVerb: PCSTR(verb as *const u8),
                    lpVerbW: PCWSTR(verb as *const u16),
                    nShow: SW_SHOWNORMAL.0,
                    ptInvoke: pt,
                    ..Default::default()
                };
                cm.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO)
                    .map_err(|e| e.message())?;
            }
            Ok(())
        })();
        let _ = DestroyMenu(menu);
        shown
    }
}

/// The Windows folder picker; the chosen folder's path, or `None`.
pub fn pick_folder(owner: HWND) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    // SAFETY: standard COM dialog calls on objects we own; the path string
    // is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dialog.Show(Some(owner)).ok()?; // cancelled: an error, so None
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}

// ---------------------------------------------------------------- file actions

use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, IFileOperationProgressSink, FOFX_ADDUNDORECORD, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO,
    FOF_WANTNUKEWARNING,
};

/// Runs one Explorer file operation (with Explorer's own dialogs, and an
/// entry in Explorer's undo). `Ok(false)`: the user cancelled it.
fn file_operation(hwnd: HWND, add: impl FnOnce(&IFileOperation) -> windows::core::Result<()>) -> Result<bool, String> {
    // SAFETY: COM calls on the UI thread (COM is initialised by winit).
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| e.message())?;
        op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_WANTNUKEWARNING)
            .map_err(|e| e.message())?;
        op.SetOwnerWindow(hwnd).map_err(|e| e.message())?;
        add(&op).map_err(|e| e.message())?;
        op.PerformOperations().map_err(|e| e.message())?;
        Ok(!op.GetAnyOperationsAborted().map_err(|e| e.message())?.as_bool())
    }
}

/// Moves a file or folder to the Recycle Bin, as Delete does in Explorer
/// (Windows asks first if the user set it so, and warns when the item is
/// too large for the Bin or on a drive without one).
pub fn recycle(hwnd: HWND, path: &str) -> Result<bool, String> {
    let item = shell_item(path)?;
    // SAFETY: a valid shell item; no progress sink.
    file_operation(hwnd, |op| unsafe { op.DeleteItem(&item, None::<&IFileOperationProgressSink>) })
}

/// Renames a file or folder in place (`name`: the new name only).
pub fn rename(hwnd: HWND, path: &str, name: &str) -> Result<bool, String> {
    let item = shell_item(path)?;
    // SAFETY: a valid shell item and name; no progress sink.
    file_operation(hwnd, |op| unsafe { op.RenameItem(&item, &HSTRING::from(name), None::<&IFileOperationProgressSink>) })
}

/// Puts the file itself on the clipboard, as Ctrl+C does in Explorer:
/// pasting in a folder, a mail or a chat pastes the file.
pub fn copy_file(path: &str) -> Result<(), String> {
    let item = shell_item(path)?;
    // SAFETY: COM calls on the UI thread; flushing keeps the data after whereti quits.
    unsafe {
        let data: IDataObject = item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject).map_err(|e| e.message())?;
        windows::Win32::System::Ole::OleSetClipboard(&data).map_err(|e| e.message())?;
        let _ = windows::Win32::System::Ole::OleFlushClipboard();
    }
    Ok(())
}
