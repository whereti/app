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
    quit_item: MenuItem,
    manager: Option<GlobalHotKeyManager>,
    registered: Option<HotKey>,
    /// The shortcut in use, e.g. "Ctrl+Alt+Space"; `None` if none could be registered.
    pub hotkey: Option<&'static str>,
}

fn open_label(hotkey: Option<&str>) -> String {
    let search = crate::i18n::tr("Search…", "Caută…");
    match hotkey {
        Some(k) => format!("{search}   {k}"),
        None => search.to_string(),
    }
}

fn quit_label() -> &'static str {
    crate::i18n::tr("Quit", "Ieșire")
}

impl Tray {
    /// Menu texts in the current language.
    pub fn relabel(&self) {
        self.open_item.set_text(open_label(self.hotkey));
        self.quit_item.set_text(quit_label());
    }

    /// Switches to another shortcut; keeps the old one if the new one is taken.
    pub fn set_hotkey(&mut self, label: &str) -> Result<&'static str, String> {
        let Some(m) = &self.manager else {
            return Err(crate::i18n::tr("shortcuts are not available", "tastele rapide nu sunt disponibile").into());
        };
        let Some(&(name, mods, code)) = HOTKEYS.iter().find(|h| h.0 == label) else {
            return Err(crate::trf!("unknown shortcut: {label}", "tastă necunoscută: {label}"));
        };
        let new = HotKey::new(Some(mods), code);
        if let Some(old) = self.registered {
            let _ = m.unregister(old);
        }
        match m.register(new) {
            Ok(()) => {
                self.registered = Some(new);
                self.hotkey = Some(name);
                self.open_item.set_text(open_label(Some(name)));
                Ok(name)
            }
            Err(_) => {
                if let Some(old) = self.registered {
                    let _ = m.register(old);
                }
                Err(crate::trf!("{name} is used by another app", "{name} e folosită de altă aplicație"))
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
    let open_item = MenuItem::new(open_label(hotkey), true, None);
    let quit = MenuItem::new(quit_label(), true, None);
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
    Ok(Tray { icon, open_item, quit_item: quit, manager, registered, hotkey })
}

/// The whereti logo, 32×32 (rendered from media/logo-small.svg by
/// tools/render-icons).
pub fn logo_rgba_32() -> Vec<u8> {
    include_bytes!("../../../media/icons/whereti-32.rgba").to_vec()
}

/// The logo at 64×64, for the window (taskbar, Alt+Tab).
pub fn logo_rgba_64() -> Vec<u8> {
    include_bytes!("../../../media/icons/whereti-64.rgba").to_vec()
}

fn magnifier_icon() -> Icon {
    icon_with_progress(None)
}

/// The logo, with a progress bar along the bottom while indexing
/// (`percent` = how far; `None` = the plain icon).
pub fn icon_with_progress(percent: Option<u8>) -> Icon {
    let mut px = logo_rgba_32();
    if let Some(pct) = percent {
        let n = 32usize;
        let filled = 3 + (26 * pct.min(100) as usize) / 100;
        for y in 26..31 {
            for x in 2..30 {
                let i = (y * n + x) * 4;
                let edge = y == 26 || y == 30 || x == 2 || x == 29;
                let color = if edge {
                    [0x14, 0x10, 0x3A, 0xFF]
                } else if x < filled {
                    [0x3D, 0xDC, 0x84, 0xFF] // green: done
                } else {
                    [0xE8, 0xE8, 0xF0, 0xFF]
                };
                px[i..i + 4].copy_from_slice(&color);
            }
        }
    }
    Icon::from_rgba(px, 32, 32).expect("valid icon")
}
