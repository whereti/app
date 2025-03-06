//! Tray icon, its menu, and the global hotkey (PLAN.md §10.3).
//!
//! Handlers run on the UI thread's message loop and only set flags and wake
//! the app; the app acts on them in `logic()`, which eframe calls even while
//! the window is hidden.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Requests from the tray, the hotkey or another launch.
#[derive(Default)]
pub struct Requests {
    pub show: AtomicBool,
    pub toggle: AtomicBool,
    pub quit: AtomicBool,
}

impl Requests {
    pub fn take(flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::SeqCst)
    }
}

/// Shortcuts offered in Settings. The first free one is used by default.
pub const HOTKEYS: [(&str, Modifiers, Code); 4] = [
    ("Ctrl+Alt+Space", Modifiers::CONTROL.union(Modifiers::ALT), Code::Space),
    ("Ctrl+Shift+Space", Modifiers::CONTROL.union(Modifiers::SHIFT), Code::Space),
    ("Alt+Shift+Space", Modifiers::ALT.union(Modifiers::SHIFT), Code::Space),
    ("Ctrl+Alt+W", Modifiers::CONTROL.union(Modifiers::ALT), Code::KeyW),
];
pub const PREFERRED_HOTKEY: &str = HOTKEYS[0].0;

pub struct Tray {
    pub icon: TrayIcon,
    open_item: MenuItem,
    manager: Option<GlobalHotKeyManager>,
    registered: Option<HotKey>,
    /// The shortcut in use, e.g. "Ctrl+Alt+Space"; `None` if none could be registered.
    pub hotkey: Option<&'static str>,
}

impl Tray {
    /// Switches to another shortcut; keeps the old one if the new one is taken.
    pub fn set_hotkey(&mut self, label: &str) -> Result<&'static str, String> {
        let Some(m) = &self.manager else { return Err("tastele rapide nu sunt disponibile".into()) };
        let Some(&(name, mods, code)) = HOTKEYS.iter().find(|h| h.0 == label) else {
            return Err(format!("tastă necunoscută: {label}"));
        };
        let new = HotKey::new(Some(mods), code);
        if let Some(old) = self.registered {
            let _ = m.unregister(old);
        }
        match m.register(new) {
            Ok(()) => {
                self.registered = Some(new);
                self.hotkey = Some(name);
                self.open_item.set_text(format!("Caută…   {name}"));
                Ok(name)
            }
            Err(_) => {
                if let Some(old) = self.registered {
                    let _ = m.register(old);
                }
                Err(format!("{name} e folosită de altă aplicație"))
            }
        }
    }
}

pub fn install(
    requests: Arc<Requests>,
    wake: impl Fn() + Send + Sync + Clone + 'static,
    preferred: Option<&str>,
) -> Result<Tray, String> {
    // The shortcut toggles the window from any application: the saved
    // choice first, then the defaults in order.
    let manager = GlobalHotKeyManager::new().ok();
    let (mut hotkey, mut registered) = (None, None);
    if let Some(m) = &manager {
        let order = preferred.into_iter().chain(HOTKEYS.iter().map(|h| h.0));
        for label in order {
            let Some(&(name, mods, code)) = HOTKEYS.iter().find(|h| h.0 == label) else { continue };
            let key = HotKey::new(Some(mods), code);
            if m.register(key).is_ok() {
                hotkey = Some(name);
                registered = Some(key);
                break;
            }
        }
    }
    {
        let (requests, wake) = (requests.clone(), wake.clone());
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                requests.toggle.store(true, Ordering::SeqCst);
                wake();
            }
        }));
    }

    let menu = Menu::new();
    let open_label = match hotkey {
        Some(k) => format!("Caută…   {k}"),
        None => "Caută…".to_string(),
    };
    let open_item = MenuItem::new(open_label, true, None);
    let quit = MenuItem::new("Ieșire", true, None);
    menu.append_items(&[&open_item, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
    let (open_id, quit_id) = (open_item.id().clone(), quit.id().clone());

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("whereti")
        .with_icon(magnifier_icon())
        .build()
        .map_err(|e| e.to_string())?;

    {
        let (requests, wake) = (requests.clone(), wake.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                requests.show.store(true, Ordering::SeqCst);
            } else if e.id == quit_id {
                requests.quit.store(true, Ordering::SeqCst);
            }
            wake();
        }));
    }
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            requests.show.store(true, Ordering::SeqCst);
            wake();
        }
    }));
    Ok(Tray { icon, open_item, manager, registered, hotkey })
}

/// A 32×32 magnifying glass, drawn in code (no image files to ship).
pub fn magnifier_rgba() -> Vec<u8> {
    let n = 32usize;
    let mut px = vec![0u8; n * n * 4];
    let (cx, cy, r) = (13.0f32, 13.0f32, 9.0f32);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let ring = (d - r).abs() < 2.2;
            // handle: a thick line from the ring towards the corner
            let t = ((fx - cx) + (fy - cy)) / 2.0;
            let off = ((fx - cx) - (fy - cy)).abs() / std::f32::consts::SQRT_2;
            let handle = t > r * 0.72 && t < r * 1.75 && off < 2.6;
            if ring || handle {
                let i = (y * n + x) * 4;
                px[i..i + 4].copy_from_slice(&[0x2B, 0x6C, 0xD9, 0xFF]);
            }
        }
    }
    px
}

fn magnifier_icon() -> Icon {
    icon_with_progress(None)
}

/// The magnifier, with a progress bar along the bottom while indexing
/// (`percent` = how far; `None` = the plain icon).
pub fn icon_with_progress(percent: Option<u8>) -> Icon {
    let mut px = magnifier_rgba();
    if let Some(pct) = percent {
        let n = 32usize;
        let filled = 2 + (28 * pct.min(100) as usize) / 100;
        for y in 26..31 {
            for x in 1..31 {
                let i = (y * n + x) * 4;
                let edge = y == 26 || y == 30 || x == 1 || x == 30;
                let color = if edge {
                    [0xFF, 0xFF, 0xFF, 0xFF]
                } else if x < filled {
                    [0x1E, 0xA0, 0x4E, 0xFF] // green: done
                } else {
                    [0x50, 0x50, 0x58, 0xFF]
                };
                px[i..i + 4].copy_from_slice(&color);
            }
        }
    }
    Icon::from_rgba(px, 32, 32).expect("valid icon")
}
