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

/// Cloaked, a window is drawn nowhere and has no taskbar button, even while
/// Windows thinks it is shown.
pub fn cloak(hwnd: HWND, on: bool) {
    use windows::Win32::Graphics::Dwm::DWMWA_CLOAK;
    let value = windows::core::BOOL::from(on);
    // SAFETY: a valid window and a BOOL-sized value, as DWMWA_CLOAK expects.
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, (&value as *const windows::core::BOOL).cast(), std::mem::size_of_val(&value) as u32);
    }
}

/// Is the window shown (as Windows sees it, cloaked or not)?
pub fn is_shown(hwnd: HWND) -> bool {
    // SAFETY: only reads the window's state.
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool() }
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
    FileOperation, IFileOperation, IFileOperationProgressSink, FILEOPERATION_FLAGS, FOFX_ADDUNDORECORD, FOFX_RECYCLEONDELETE,
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING,
};

/// Runs one Explorer file operation (with Explorer's own dialogs, and an
/// entry in Explorer's undo). `Ok(false)`: the user cancelled it.
/// `owner`: the window its dialogs belong to; none on a thread of its own
/// (a window of another thread would tie the two threads' input together).
/// `asked`: the user already said yes in whereti, so Windows does not ask again.
fn file_operation(
    owner: Option<HWND>,
    asked: bool,
    add: impl FnOnce(&IFileOperation) -> windows::core::Result<()>,
) -> Result<bool, String> {
    let flags: FILEOPERATION_FLAGS = if asked {
        FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_NOCONFIRMATION
    } else {
        FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_ADDUNDORECORD | FOF_WANTNUKEWARNING
    };
    // SAFETY: COM calls on a thread where COM is initialised (by winit on
    // the UI thread; by the caller on others).
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| e.message())?;
        op.SetOperationFlags(flags).map_err(|e| e.message())?;
        if let Some(hwnd) = owner {
            op.SetOwnerWindow(hwnd).map_err(|e| e.message())?;
        }
        add(&op).map_err(|e| e.message())?;
        op.PerformOperations().map_err(|e| e.message())?;
        Ok(!op.GetAnyOperationsAborted().map_err(|e| e.message())?.as_bool())
    }
}

/// Moves a file or folder to the Recycle Bin, as Delete does in Explorer
/// (Windows asks first if the user set it so, and warns when the item is
/// too large for the Bin or on a drive without one). `permanent`: on a
/// drive without a Recycle Bin, after the user said yes in whereti; then
/// Windows does not ask a second time.
pub fn recycle(owner: Option<HWND>, path: &str, permanent: bool) -> Result<bool, String> {
    let item = shell_item(path)?;
    // SAFETY: a valid shell item; no progress sink.
    file_operation(owner, permanent, |op| unsafe { op.DeleteItem(&item, None::<&IFileOperationProgressSink>) })
}

/// Renames a file or folder in place (`name`: the new name only).
pub fn rename(owner: Option<HWND>, path: &str, name: &str) -> Result<bool, String> {
    let item = shell_item(path)?;
    // SAFETY: a valid shell item and name; no progress sink.
    file_operation(owner, false, |op| unsafe { op.RenameItem(&item, &HSTRING::from(name), None::<&IFileOperationProgressSink>) })
}

/// Puts the file itself on the clipboard, as Ctrl+C does in Explorer:
/// pasting in a folder, a mail or a chat pastes the file. On a thread
/// where OLE is initialised.
pub fn copy_file(path: &str) -> Result<(), String> {
    let item = shell_item(path)?;
    // SAFETY: COM calls on an OLE thread; flushing keeps the data after
    // whereti quits (or this thread ends).
    unsafe {
        let data: IDataObject = item.BindToHandler(None::<&IBindCtx>, &BHID_DataObject).map_err(|e| e.message())?;
        windows::Win32::System::Ole::OleSetClipboard(&data).map_err(|e| e.message())?;
        let _ = windows::Win32::System::Ole::OleFlushClipboard();
    }
    Ok(())
}

// ---------------------------------------------------------------- Open/Save windows
//
// whereti never presses Open or Save and never touches the File name box:
// that could save or open the wrong file. It only uses the window's address
// bar (Alt+D, as in Explorer), which can do nothing but change the folder.

use windows::core::BOOL;
use windows::Win32::Foundation::WPARAM as WP;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL,
    VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetAncestor, GetClassNameW, GetDlgCtrlID, GetGUIThreadInfo, GetParent, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, PostMessageW, SendMessageTimeoutW, SetForegroundWindow as SetForeground,
    GA_ROOT, GUITHREADINFO, SMTO_ABORTIFHUNG, WM_KEYDOWN, WM_KEYUP, WM_SETTEXT,
};

/// The address bar of Open/Save windows and Explorer (a ComboBoxEx32).
const ADDRESS_ID: i32 = 41477;

/// An Open or Save window of another app, from which whereti was called.
pub struct FileDialog {
    dialog: HWND,
    /// Its title, e.g. "Save As".
    pub title: String,
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: a buffer of its stated size.
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn text_of(h: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: a buffer of its stated size.
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// `h` is the Edit of an address bar (inside the ComboBoxEx32 `ADDRESS_ID`).
fn is_address_edit(h: HWND) -> bool {
    if class_of(h) != "Edit" {
        return false;
    }
    let mut w = h;
    for _ in 0..3 {
        // SAFETY: plain window queries.
        w = unsafe { GetParent(w) }.unwrap_or_default();
        if w.is_invalid() {
            return false;
        }
        if class_of(w) == "ComboBoxEx32" {
            // SAFETY: as above.
            return unsafe { GetDlgCtrlID(w) } == ADDRESS_ID;
        }
    }
    false
}

/// The window that had the keyboard when the shortcut was pressed, if it
/// is an Open or Save window (a dialog with an address bar).
pub fn file_dialog(foreground: isize) -> Option<FileDialog> {
    if foreground == 0 {
        return None;
    }
    // SAFETY: plain window queries; a stale handle only makes them fail.
    unsafe {
        let dialog = GetAncestor(HWND(foreground as *mut c_void), GA_ROOT);
        if dialog.is_invalid() || class_of(dialog) != "#32770" {
            return None;
        }
        unsafe extern "system" fn visit(h: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the bool below, alive for the whole enumeration.
            let found = unsafe { &mut *(data.0 as *mut bool) };
            if is_address_edit(h) {
                *found = true;
                return BOOL(0);
            }
            BOOL(1)
        }
        let mut found = false;
        let _ = EnumChildWindows(Some(dialog), Some(visit), LPARAM(&mut found as *mut bool as isize));
        found.then(|| FileDialog { dialog, title: text_of(dialog) })
    }
}

/// The control with the keyboard in `dialog`'s thread.
fn focused(dialog: HWND) -> Option<HWND> {
    // SAFETY: a zeroed GUITHREADINFO with its size set, as the call requires.
    unsafe {
        let thread = GetWindowThreadProcessId(dialog, None);
        let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        GetGUIThreadInfo(thread, &mut info).ok()?;
        (!info.hwndFocus.is_invalid()).then_some(info.hwndFocus)
    }
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
        },
    }
}

/// Takes the Open/Save window to `folder` through its address bar, and
/// leaves it in front. Nothing is opened or saved.
pub fn jump(d: &FileDialog, folder: &str) -> Result<(), String> {
    let wait = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
    // the Enter that chose the folder must be up first: held or repeated,
    // it would reach the dialog and press its Open/Save button
    for _ in 0..50 {
        // SAFETY: no arguments.
        let down = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(VK_RETURN.0 as i32) } as u16 & 0x8000 != 0;
        if !down {
            break;
        }
        wait(20);
    }
    // SAFETY: window calls on a handle checked to be alive.
    unsafe {
        if !IsWindow(Some(d.dialog)).as_bool() {
            return Err("closed".into());
        }
        let _ = SetForeground(d.dialog);
    }
    wait(150);
    // Ctrl+L puts the keyboard in the address bar, as in Explorer. Not Alt+D:
    // in some languages Alt+D is a button's shortcut (Open, Save) instead.
    let l_key = VIRTUAL_KEY(b'L' as u16);
    let keys = [key(VK_CONTROL, false), key(l_key, false), key(l_key, true), key(VK_CONTROL, true)];
    // SAFETY: four well-formed keyboard inputs.
    unsafe { SendInput(&keys, std::mem::size_of::<INPUT>() as i32) };
    // only once the address bar really has the keyboard
    let edit = (0..20)
        .find_map(|_| {
            wait(50);
            focused(d.dialog).filter(|&f| is_address_edit(f))
        })
        .ok_or("no address bar")?;
    let wide: Vec<u16> = folder.encode_utf16().chain([0]).collect();
    let mut result = 0usize;
    // SAFETY: WM_SETTEXT is marshalled by Windows across processes; 2 s at most.
    let set = unsafe {
        SendMessageTimeoutW(edit, WM_SETTEXT, WP(0), LPARAM(wide.as_ptr() as isize), SMTO_ABORTIFHUNG, 2000, Some(&mut result))
    };
    if set.0 == 0 || result != 1 {
        // an app running as administrator does not take it from ours
        return Err("refused".into());
    }
    // Enter, to the address bar itself (not to whatever has the keyboard)
    const ENTER_SCAN: isize = 0x1C << 16 | 1;
    // SAFETY: plain key messages to a live window.
    unsafe {
        PostMessageW(Some(edit), WM_KEYDOWN, WP(VK_RETURN.0 as usize), LPARAM(ENTER_SCAN)).map_err(|e| e.message())?;
        PostMessageW(Some(edit), WM_KEYUP, WP(VK_RETURN.0 as usize), LPARAM(ENTER_SCAN | 0xC000_0000u32 as isize))
            .map_err(|e| e.message())?;
    }
    Ok(())
}

/// On a network share or a mapped network drive (no Recycle Bin there, and
/// reading can block for a sleeping NAS).
pub fn is_network_path(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    if path.starts_with("\\\\") {
        return true;
    }
    let Some(letter) = path.chars().next().filter(|c| c.is_ascii_alphabetic() && path[1..].starts_with(':')) else { return false };
    const DRIVE_REMOTE: u32 = 4;
    // SAFETY: a NUL-terminated root path.
    unsafe { GetDriveTypeW(&HSTRING::from(format!("{letter}:\\"))) == DRIVE_REMOTE }
}
