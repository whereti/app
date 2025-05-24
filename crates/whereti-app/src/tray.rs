//! Tray icon, its menu, and the global hotkey (PLAN.md §10.3).
//!
//! Handlers run on the UI thread's message loop and only set flags and wake
//! the app; the app acts on them in `logic()`, which eframe calls even while
//! the window is hidden.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
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

/// Shortcuts suggested in Settings. The first free one is used by default.
pub const HOTKEYS: [&str; 4] = ["Ctrl+Alt+Space", "Ctrl+Shift+Space", "Alt+Shift+Space", "Ctrl+Alt+W"];
pub const PREFERRED_HOTKEY: &str = HOTKEYS[0];

/// The keys a shortcut can end with: egui's key, the name shown, the key
/// for Windows.
const KEYS: &[(egui::Key, &str, Code)] = {
    use egui::Key as K;
    &[
        (K::Space, "Space", Code::Space),
        (K::A, "A", Code::KeyA), (K::B, "B", Code::KeyB), (K::C, "C", Code::KeyC), (K::D, "D", Code::KeyD),
        (K::E, "E", Code::KeyE), (K::F, "F", Code::KeyF), (K::G, "G", Code::KeyG), (K::H, "H", Code::KeyH),
        (K::I, "I", Code::KeyI), (K::J, "J", Code::KeyJ), (K::K, "K", Code::KeyK), (K::L, "L", Code::KeyL),
        (K::M, "M", Code::KeyM), (K::N, "N", Code::KeyN), (K::O, "O", Code::KeyO), (K::P, "P", Code::KeyP),
        (K::Q, "Q", Code::KeyQ), (K::R, "R", Code::KeyR), (K::S, "S", Code::KeyS), (K::T, "T", Code::KeyT),
        (K::U, "U", Code::KeyU), (K::V, "V", Code::KeyV), (K::W, "W", Code::KeyW), (K::X, "X", Code::KeyX),
        (K::Y, "Y", Code::KeyY), (K::Z, "Z", Code::KeyZ),
        (K::Num0, "0", Code::Digit0), (K::Num1, "1", Code::Digit1), (K::Num2, "2", Code::Digit2),
        (K::Num3, "3", Code::Digit3), (K::Num4, "4", Code::Digit4), (K::Num5, "5", Code::Digit5),
        (K::Num6, "6", Code::Digit6), (K::Num7, "7", Code::Digit7), (K::Num8, "8", Code::Digit8),
        (K::Num9, "9", Code::Digit9),
        (K::F1, "F1", Code::F1), (K::F2, "F2", Code::F2), (K::F3, "F3", Code::F3), (K::F4, "F4", Code::F4),
        (K::F5, "F5", Code::F5), (K::F6, "F6", Code::F6), (K::F7, "F7", Code::F7), (K::F8, "F8", Code::F8),
        (K::F9, "F9", Code::F9), (K::F10, "F10", Code::F10), (K::F11, "F11", Code::F11), (K::F12, "F12", Code::F12),
        (K::Enter, "Enter", Code::Enter), (K::Tab, "Tab", Code::Tab), (K::Insert, "Insert", Code::Insert),
        (K::Home, "Home", Code::Home), (K::End, "End", Code::End), (K::PageUp, "PageUp", Code::PageUp),
        (K::PageDown, "PageDown", Code::PageDown), (K::ArrowUp, "Up", Code::ArrowUp),
        (K::ArrowDown, "Down", Code::ArrowDown), (K::ArrowLeft, "Left", Code::ArrowLeft),
        (K::ArrowRight, "Right", Code::ArrowRight), (K::Backtick, "`", Code::Backquote),
        (K::Minus, "-", Code::Minus), (K::Equals, "=", Code::Equal), (K::Comma, ",", Code::Comma),
        (K::Period, ".", Code::Period), (K::Slash, "/", Code::Slash), (K::Semicolon, ";", Code::Semicolon),
        (K::Quote, "'", Code::Quote), (K::OpenBracket, "[", Code::BracketLeft),
        (K::CloseBracket, "]", Code::BracketRight), (K::Backslash, "\\", Code::Backslash),
    ]
};

/// "Ctrl+Alt+Space" as a key for Windows. It needs Ctrl or Alt, so that
/// typing in other apps is never taken over.
pub fn parse(label: &str) -> Option<HotKey> {
    let mut parts: Vec<&str> = label.split('+').collect();
    // "Ctrl++" (the plus key) is not offered, so the last part is the key
    let key = parts.pop()?;
    let mut mods = Modifiers::empty();
    for m in parts {
        mods |= match m {
            "Ctrl" => Modifiers::CONTROL,
            "Alt" => Modifiers::ALT,
            "Shift" => Modifiers::SHIFT,
            _ => return None,
        };
    }
    if !mods.intersects(Modifiers::CONTROL | Modifiers::ALT) {
        return None;
    }
    let &(_, _, code) = KEYS.iter().find(|k| k.1 == key)?;
    Some(HotKey::new(Some(mods), code))
}

/// The label for keys pressed in the window, e.g. "Ctrl+Shift+K"; `None`
/// while only modifiers are held, or without Ctrl or Alt.
pub fn label_for(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    if !(m.ctrl || m.alt) {
        return None;
    }
    let &(_, name, _) = KEYS.iter().find(|k| k.0 == key)?;
    let mut label = String::new();
    for (on, text) in [(m.ctrl, "Ctrl+"), (m.alt, "Alt+"), (m.shift, "Shift+")] {
        if on {
            label.push_str(text);
        }
    }
    label.push_str(name);
    Some(label)
}

pub struct Tray {
    pub icon: TrayIcon,
    open_item: MenuItem,
    quit_item: MenuItem,
    manager: Option<GlobalHotKeyManager>,
    registered: Option<HotKey>,
    /// The shortcut in use, e.g. "Ctrl+Alt+Space"; `None` if none could be registered.
    pub hotkey: Option<String>,
}

fn open_label(hotkey: Option<&str>) -> String {
    let search = crate::i18n::tr("Search…");
    match hotkey {
        Some(k) => format!("{search}   {k}"),
        None => search.to_string(),
    }
}

fn quit_label() -> &'static str {
    crate::i18n::tr("Quit")
}

impl Tray {
    /// Menu texts in the current language.
    pub fn relabel(&self) {
        self.open_item.set_text(open_label(self.hotkey.as_deref()));
        self.quit_item.set_text(quit_label());
    }

    /// Switches to another shortcut; keeps the old one if the new one is taken.
    pub fn set_hotkey(&mut self, label: &str) -> Result<(), String> {
        let Some(m) = &self.manager else {
            return Err(crate::i18n::tr("shortcuts are not available").into());
        };
        let Some(new) = parse(label) else {
            return Err(crate::i18n::tr("Use Ctrl or Alt with another key").into());
        };
        if let Some(old) = self.registered {
            let _ = m.unregister(old);
        }
        match m.register(new) {
            Ok(()) => {
                self.registered = Some(new);
                self.hotkey = Some(label.to_string());
                self.open_item.set_text(open_label(Some(label)));
                Ok(())
            }
            Err(_) => {
                if let Some(old) = self.registered {
                    let _ = m.register(old);
                }
                Err(crate::trf!("{label} is used by another app", label))
            }
        }
    }

    /// While a new shortcut is typed in Settings the old one must not
    /// fire (it would hide the window).
    pub fn pause_hotkey(&self, paused: bool) {
        if let (Some(m), Some(k)) = (&self.manager, self.registered) {
            let _ = if paused { m.unregister(k) } else { m.register(k) };
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
        let order = preferred.into_iter().chain(HOTKEYS);
        for label in order {
            let Some(key) = parse(label) else { continue };
            if m.register(key).is_ok() {
                hotkey = Some(label.to_string());
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
    let open_item = MenuItem::new(open_label(hotkey.as_deref()), true, None);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_parse() {
        for h in HOTKEYS {
            assert!(parse(h).is_some(), "{h}");
        }
    }

    #[test]
    fn needs_ctrl_or_alt() {
        assert!(parse("Shift+A").is_none());
        assert!(parse("A").is_none());
        assert!(parse("Ctrl+Nope").is_none());
        assert_eq!(parse("Ctrl+Shift+K"), Some(HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyK)));
    }

    #[test]
    fn labels_round_trip() {
        let m = egui::Modifiers { ctrl: true, shift: true, ..Default::default() };
        let label = label_for(egui::Key::F5, m).unwrap();
        assert_eq!(label, "Ctrl+Shift+F5");
        assert!(parse(&label).is_some());
        assert_eq!(label_for(egui::Key::A, egui::Modifiers::SHIFT), None);
    }
}
