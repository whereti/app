//! Safe removal of external NTFS drives (PLAN.md §43).
//!
//! The engine keeps a handle open on each NTFS drive (to read MFT records)
//! and a journal reader thread keeps another. An open handle makes Windows
//! refuse "Safely remove" ("this device is in use"). So every such handle is
//! registered with `RegisterDeviceNotification`, and when Windows asks
//! (`DBT_DEVICEQUERYREMOVE`) the engine closes both before answering; if the
//! removal is cancelled (`DBT_DEVICEQUERYREMOVEFAILED`) it opens them again.
//!
//! Notifications arrive at a hidden window on its own thread; the window
//! hands them to the engine's worker and, for a removal request, waits for
//! the worker to say the handles are closed.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, RegisterDeviceNotificationW,
    TranslateMessage, UnregisterDeviceNotification, DBT_DEVICEQUERYREMOVE, DBT_DEVICEQUERYREMOVEFAILED,
    DBT_DEVICEREMOVECOMPLETE, DBT_DEVICEREMOVEPENDING, DBT_DEVTYP_HANDLE, DEVICE_NOTIFY_WINDOW_HANDLE,
    DEV_BROADCAST_HANDLE, DEV_BROADCAST_HDR, HWND_MESSAGE, MSG, WM_DEVICECHANGE, WNDCLASSW,
};

use crate::engine::Msg;

/// How long a removal request waits for the worker to close its handles.
const RELEASE_WAIT: Duration = Duration::from_secs(8);

struct State {
    tx: Sender<Msg>,
    /// Registered notification -> drive letter.
    letters: HashMap<usize, char>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// The hidden window that receives device notifications.
pub struct Removal {
    hwnd: usize,
}

impl Removal {
    /// Starts the window thread. `None` if the window could not be made
    /// (whereti then works as before, without safe removal).
    pub fn start(tx: Sender<Msg>) -> Option<Removal> {
        if STATE.set(Mutex::new(State { tx, letters: HashMap::new() })).is_err() {
            return None; // one per process
        }
        let (done, hwnd) = mpsc::channel();
        std::thread::Builder::new()
            .name("device-notifications".into())
            .spawn(move || {
                let name: Vec<u16> = "whereti-device-notifications\0".encode_utf16().collect();
                // SAFETY: a window class and a message-only window owned by this
                // thread, which runs their message loop until the process ends.
                unsafe {
                    let module = GetModuleHandleW(std::ptr::null());
                    let mut class: WNDCLASSW = std::mem::zeroed();
                    class.lpfnWndProc = Some(window_proc);
                    class.hInstance = module;
                    class.lpszClassName = name.as_ptr();
                    RegisterClassW(&class);
                    let hwnd = CreateWindowExW(
                        0,
                        name.as_ptr(),
                        name.as_ptr(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        HWND_MESSAGE,
                        std::ptr::null_mut(),
                        module,
                        std::ptr::null(),
                    );
                    let _ = done.send(hwnd as usize);
                    if hwnd.is_null() {
                        return;
                    }
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .ok()?;
        let hwnd = hwnd.recv_timeout(Duration::from_secs(5)).ok().filter(|&h| h != 0)?;
        Some(Removal { hwnd })
    }

    /// Asks Windows to tell us before `handle` (an open volume of drive
    /// `letter`) is removed. Returns the registration, to undo with `unwatch`.
    pub fn watch(&self, letter: char, handle: isize) -> Option<usize> {
        // SAFETY: a zeroed DEV_BROADCAST_HANDLE with its size, type and the
        // handle set, as the API requires.
        let n = unsafe {
            let mut filter: DEV_BROADCAST_HANDLE = std::mem::zeroed();
            filter.dbch_size = std::mem::size_of::<DEV_BROADCAST_HANDLE>() as u32;
            filter.dbch_devicetype = DBT_DEVTYP_HANDLE;
            filter.dbch_handle = handle as HANDLE;
            RegisterDeviceNotificationW(self.hwnd as HWND, &filter as *const _ as *const _, DEVICE_NOTIFY_WINDOW_HANDLE)
        };
        if n.is_null() {
            return None;
        }
        if let Some(s) = STATE.get() {
            s.lock().unwrap_or_else(|e| e.into_inner()).letters.insert(n as usize, letter);
        }
        Some(n as usize)
    }

    /// Undoes `watch`; call before closing the handle.
    pub fn unwatch(n: usize) {
        if let Some(s) = STATE.get() {
            s.lock().unwrap_or_else(|e| e.into_inner()).letters.remove(&n);
        }
        // SAFETY: a registration returned by RegisterDeviceNotificationW.
        unsafe {
            UnregisterDeviceNotification(n as *mut _);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DEVICECHANGE && lparam != 0 {
        let event = wparam as u32;
        // SAFETY: for these events lParam points to a DEV_BROADCAST_HDR, and
        // for DBT_DEVTYP_HANDLE to a whole DEV_BROADCAST_HANDLE.
        let hdr = unsafe { &*(lparam as *const DEV_BROADCAST_HDR) };
        if hdr.dbch_devicetype == DBT_DEVTYP_HANDLE {
            let h = unsafe { &*(lparam as *const DEV_BROADCAST_HANDLE) };
            let (letter, tx) = match STATE.get() {
                Some(s) => {
                    let s = s.lock().unwrap_or_else(|e| e.into_inner());
                    (s.letters.get(&(h.dbch_hdevnotify as usize)).copied(), s.tx.clone())
                }
                None => return 1,
            };
            if let Some(letter) = letter {
                match event {
                    DBT_DEVICEQUERYREMOVE => {
                        // close everything on that drive, then let Windows go on
                        let (done, wait) = mpsc::channel();
                        if tx.send(Msg::Release(letter, Some(done))).is_ok() {
                            let _ = wait.recv_timeout(RELEASE_WAIT);
                        }
                        return 1; // TRUE: removal may proceed
                    }
                    DBT_DEVICEQUERYREMOVEFAILED => {
                        let _ = tx.send(Msg::Reacquire(letter));
                    }
                    DBT_DEVICEREMOVEPENDING | DBT_DEVICEREMOVECOMPLETE => {
                        // pulled out without asking: let go at once
                        let _ = tx.send(Msg::Release(letter, None));
                    }
                    _ => {}
                }
            }
            return 1;
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    #[test]
    fn the_window_starts_and_registers_handles() {
        let (tx, _rx) = mpsc::channel();
        let r = Removal::start(tx).expect("notification window");
        let path = std::env::temp_dir().join(format!("whereti-removal-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let n = r.watch('C', f.as_raw_handle() as isize).expect("registration");
        assert_eq!(STATE.get().unwrap().lock().unwrap().letters.get(&n), Some(&'C'));
        Removal::unwatch(n);
        assert!(STATE.get().unwrap().lock().unwrap().letters.is_empty());
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
}
