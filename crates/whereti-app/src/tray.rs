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

pub struct Tray {
    pub icon: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    /// The shortcut in use, e.g. "Ctrl+Alt+Space"; `None` if none could be registered.
    pub hotkey: Option<&'static str>,
}

/// Preferred shortcut first; the next ones are used if it is taken.
const HOTKEYS: [(&str, Modifiers, Code); 2] = [
    ("Ctrl+Alt+Space", Modifiers::CONTROL.union(Modifiers::ALT), Code::Space),
    ("Ctrl+Shift+Space", Modifiers::CONTROL.union(Modifiers::SHIFT), Code::Space),
];
pub const PREFERRED_HOTKEY: &str = HOTKEYS[0].0;

pub fn install(requests: Arc<Requests>, wake: impl Fn() + Send + Sync + Clone + 'static) -> Result<Tray, String> {
    // The shortcut toggles the window from any application.
    let manager = GlobalHotKeyManager::new().ok();
    let mut hotkey = None;
    if let Some(m) = &manager {
        for (label, mods, code) in HOTKEYS {
            if m.register(HotKey::new(Some(mods), code)).is_ok() {
                hotkey = Some(label);
                break;
            }
        }
    }
    if hotkey.is_some() {
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
    let open = MenuItem::new(open_label, true, None);
    let quit = MenuItem::new("Ieșire", true, None);
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());

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
    Ok(Tray { icon, _hotkeys: manager, hotkey })
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
    Icon::from_rgba(magnifier_rgba(), 32, 32).expect("valid icon")
}
