//! Windows things to open, next to files (PLAN.md §42): installed apps,
//! Settings pages and Control Panel items. Typing "printer" offers
//! "Printers & scanners", "word" offers Microsoft Word.
//!
//! - **Apps** come from `shell:AppsFolder`, the list the Start menu shows,
//!   with the names Windows gives them in its own language.
//! - **Control Panel items** come from "All Control Panel Items", named by
//!   Windows too.
//! - **Settings pages** have no list to read, so the common ones are below,
//!   named as in Windows 11 and translated with the rest of the app, each with
//!   words people use for it ("wallpaper" for Background).
//!
//! They are matched by the start of a word, in the app's language and in
//! English, and only a few good matches are shown, above the files.

use std::sync::Arc;

use parking_lot::Mutex;
use whereti_i18n::tr_both;
use whereti_query::fold;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    App,
    Setting,
    ControlPanel,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// What is opened: `shell:AppsFolder\…`, `ms-settings:…` or a Control
    /// Panel parsing name (`::{…}`).
    pub target: String,
    /// Folded name and keywords, for matching.
    words: Vec<String>,
}

/// Most items shown above the files.
pub const MAX_SHOWN: usize = 4;

/// Apps and Control Panel items, read once in the background.
pub struct Launch {
    shell: Arc<Mutex<Vec<Item>>>,
}

impl Launch {
    pub fn new(ctx: &eframe::egui::Context) -> Launch {
        let shell = Arc::new(Mutex::new(Vec::new()));
        let (out, ctx) = (shell.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("launch-items".into()).spawn(move || {
            let mut items = read_folder(r"shell:AppsFolder", Kind::App);
            items.extend(read_folder(r"shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}\0", Kind::ControlPanel));
            *out.lock() = items;
            ctx.request_repaint();
        });
        Launch { shell }
    }

    /// Reads the lists again (apps installed since whereti started).
    pub fn refresh(&self) {
        let out = self.shell.clone();
        let _ = std::thread::Builder::new().name("launch-items".into()).spawn(move || {
            let mut items = read_folder(r"shell:AppsFolder", Kind::App);
            items.extend(read_folder(r"shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}\0", Kind::ControlPanel));
            if !items.is_empty() {
                *out.lock() = items;
            }
        });
    }

    /// The best few matches for what was typed.
    pub fn search(&self, query: &str) -> Vec<Item> {
        let q: Vec<String> = query.split_whitespace().map(fold).filter(|w| !w.is_empty()).collect();
        if q.is_empty() || q.iter().map(|w| w.chars().count()).sum::<usize>() < 2 || looks_like_syntax(query) {
            return Vec::new();
        }
        let settings = settings();
        let shell = self.shell.lock();
        let mut hits: Vec<(u32, &Item)> =
            shell.iter().chain(settings.iter()).filter_map(|it| score(it, &q).map(|s| (s, it))).collect();
        // best score first; at equal score apps, then settings, then Control Panel; then shorter names
        hits.sort_by_key(|(s, it)| {
            (std::cmp::Reverse(*s), it.kind as u8, it.name.chars().count())
        });
        let mut out: Vec<Item> = Vec::new();
        for (_, it) in hits {
            if out.len() == MAX_SHOWN {
                break;
            }
            // Settings and Control Panel often name the same thing twice
            if !out.iter().any(|o| o.name.eq_ignore_ascii_case(&it.name)) {
                out.push(it.clone());
            }
        }
        out
    }
}

/// `ext:pdf`, `size:>1mb`, a path: a search for files, not for apps.
fn looks_like_syntax(q: &str) -> bool {
    q.contains(':') || q.contains('\\') || q.contains('*') || q.contains('"')
}

/// Every typed word must start a word of the item. Higher is better:
/// 3 the name starts with what was typed, 2 every word starts a word of the
/// name, 1 some words matched only keywords.
fn score(it: &Item, q: &[String]) -> Option<u32> {
    let name = &it.words[0];
    let name_words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let all_words: Vec<&str> =
        it.words.iter().flat_map(|w| w.split(|c: char| !c.is_alphanumeric())).filter(|w| !w.is_empty()).collect();
    let mut in_name = true;
    for w in q {
        if name_words.iter().any(|n| n.starts_with(w.as_str())) {
            continue;
        }
        in_name = false;
        if !all_words.iter().any(|n| n.starts_with(w.as_str())) {
            return None;
        }
    }
    let joined = q.join(" ");
    Some(if name.starts_with(&joined) {
        3
    } else if in_name {
        2
    } else {
        // a single short word matching only a keyword is too loose
        if joined.chars().count() < 3 {
            return None;
        }
        1
    })
}

fn item(kind: Kind, name: String, target: String, extra: &[&str]) -> Item {
    let mut words = vec![fold(&name)];
    words.extend(extra.iter().map(|w| fold(w)));
    Item { kind, name, target, words }
}

/// The Settings pages people look for, named as in Windows 11.
fn settings() -> Vec<Item> {
    let s = |uri: &str, (name_en, name): (&str, &str), (kw_en, kw): (&str, &str)| {
        item(Kind::Setting, name.to_string(), uri.to_string(), &[name_en, kw_en, kw])
    };
    vec![
        s("ms-settings:printers", tr_both("Printers & scanners"), tr_both("printer, print, scanner, fax")),
        s("ms-settings:bluetooth", tr_both("Bluetooth & devices"), tr_both("bluetooth, headphones, mouse, keyboard, pair")),
        s("ms-settings:network-wifi", tr_both("Wi-Fi"), tr_both("wifi, wireless, internet, network")),
        s("ms-settings:network-status", tr_both("Network & internet"), tr_both("network, internet, ethernet, connection")),
        s("ms-settings:network-vpn", tr_both("VPN"), tr_both("vpn, private network")),
        s("ms-settings:network-proxy", tr_both("Proxy"), tr_both("proxy")),
        s("ms-settings:display", tr_both("Display"), tr_both("screen, monitor, resolution, brightness, scale")),
        s("ms-settings:nightlight", tr_both("Night light"), tr_both("night light, blue light, warm screen")),
        s("ms-settings:display-advancedgraphics", tr_both("Graphics"), tr_both("graphics card, gpu, performance")),
        s("ms-settings:sound", tr_both("Sound"), tr_both("sound, volume, speakers, microphone, audio")),
        s("ms-settings:notifications", tr_both("Notifications"), tr_both("notifications, do not disturb, focus")),
        s("ms-settings:powersleep", tr_both("Power & battery"), tr_both("power, battery, sleep, energy saver")),
        s("ms-settings:storagesense", tr_both("Storage"), tr_both("storage, disk space, free up space, clean up")),
        s("ms-settings:multitasking", tr_both("Multitasking"), tr_both("snap windows, alt tab, virtual desktops")),
        s("ms-settings:clipboard", tr_both("Clipboard"), tr_both("clipboard, copy, paste, history")),
        s("ms-settings:remotedesktop", tr_both("Remote Desktop"), tr_both("remote desktop, remote access")),
        s("ms-settings:about", tr_both("About"), tr_both("pc name, rename pc, specifications, windows version, processor, memory")),
        s("ms-settings:personalization-background", tr_both("Background"), tr_both("wallpaper, desktop background, picture")),
        s("ms-settings:colors", tr_both("Colors"), tr_both("dark mode, light mode, accent color")),
        s("ms-settings:themes", tr_both("Themes"), tr_both("theme, look")),
        s("ms-settings:lockscreen", tr_both("Lock screen"), tr_both("lock screen, screen saver")),
        s("ms-settings:personalization-start", tr_both("Start"), tr_both("start menu")),
        s("ms-settings:taskbar", tr_both("Taskbar"), tr_both("taskbar, tray, system tray")),
        s("ms-settings:fonts", tr_both("Fonts"), tr_both("fonts, typeface")),
        s("ms-settings:appsfeatures", tr_both("Installed apps"), tr_both("uninstall, remove, programs, apps")),
        s("ms-settings:defaultapps", tr_both("Default apps"), tr_both("default browser, open with, default program")),
        s("ms-settings:startupapps", tr_both("Startup apps"), tr_both("startup, start with windows, autostart")),
        s("ms-settings:optionalfeatures", tr_both("Optional features"), tr_both("features, add a feature")),
        s("ms-settings:yourinfo", tr_both("Your info"), tr_both("account, profile picture")),
        s("ms-settings:emailandaccounts", tr_both("Email & accounts"), tr_both("email, accounts, microsoft account")),
        s("ms-settings:signinoptions", tr_both("Sign-in options"), tr_both("password, pin, windows hello, fingerprint, face")),
        s("ms-settings:otherusers", tr_both("Other users"), tr_both("add user, family, accounts")),
        s("ms-settings:backup", tr_both("Windows Backup"), tr_both("backup, onedrive")),
        s("ms-settings:dateandtime", tr_both("Date & time"), tr_both("date, time, clock, time zone")),
        s("ms-settings:regionlanguage", tr_both("Language & region"), tr_both("language, region, keyboard layout, format")),
        s("ms-settings:typing", tr_both("Typing"), tr_both("typing, autocorrect, spelling, keyboard")),
        s("ms-settings:speech", tr_both("Speech"), tr_both("speech, voice, dictation")),
        s("ms-settings:gaming-gamemode", tr_both("Game Mode"), tr_both("game mode, gaming")),
        s("ms-settings:easeofaccess", tr_both("Accessibility"), tr_both("accessibility, text size, magnifier, narrator, contrast")),
        s("ms-settings:mousetouchpad", tr_both("Mouse"), tr_both("mouse, pointer, scroll, cursor")),
        s("ms-settings:devices-touchpad", tr_both("Touchpad"), tr_both("touchpad, trackpad")),
        s("ms-settings:usb", tr_both("USB"), tr_both("usb")),
        s("ms-settings:autoplay", tr_both("AutoPlay"), tr_both("autoplay, usb stick, memory card")),
        s("ms-settings:privacy", tr_both("Privacy & security"), tr_both("privacy, permissions")),
        s("ms-settings:privacy-location", tr_both("Location"), tr_both("location, gps")),
        s("ms-settings:privacy-webcam", tr_both("Camera"), tr_both("camera, webcam")),
        s("ms-settings:privacy-microphone", tr_both("Microphone"), tr_both("microphone, mic")),
        s("ms-settings:windowsdefender", tr_both("Windows Security"), tr_both("antivirus, virus, defender, firewall, security")),
        s("ms-settings:deviceencryption", tr_both("Device encryption"), tr_both("bitlocker, encryption")),
        s("ms-settings:windowsupdate", tr_both("Windows Update"), tr_both("update, updates, upgrade")),
        s("ms-settings:recovery", tr_both("Recovery"), tr_both("reset this pc, recovery, restore, reinstall")),
        s("ms-settings:troubleshoot", tr_both("Troubleshoot"), tr_both("troubleshoot, fix problems")),
        s("ms-settings:activation", tr_both("Activation"), tr_both("activation, product key, license")),
    ]
}

/// The items of a shell folder, with their names as Windows shows them.
fn read_folder(folder: &str, kind: Kind) -> Vec<Item> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN_DESKTOPABSOLUTEPARSING,
        SIGDN_NORMALDISPLAY,
    };
    let mut out = Vec::new();
    // SAFETY: COM calls on interfaces we own; every string Windows returns is
    // freed with CoTaskMemFree.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(root) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(folder), None) else { return out };
        let Ok(items) = root.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems) else { return out };
        loop {
            let mut got = [None];
            let mut n = 0u32;
            if items.Next(&mut got, Some(&mut n)).is_err() || n == 0 {
                break;
            }
            let Some(it) = got[0].take() else { break };
            let text = |sigdn| -> Option<String> {
                let p = it.GetDisplayName(sigdn).ok()?;
                let s = p.to_string().ok();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            };
            let (Some(name), Some(parsing)) = (text(SIGDN_NORMALDISPLAY), text(SIGDN_DESKTOPABSOLUTEPARSING)) else {
                continue;
            };
            if kind == Kind::App && !useful_app(&name, &parsing) {
                continue;
            }
            let target = match kind {
                // the parsing name of an app is its id inside AppsFolder
                Kind::App => format!(r"shell:AppsFolder\{}", parsing.rsplit('\\').next().unwrap_or(&parsing)),
                _ => parsing,
            };
            out.push(item(kind, name, target, &[]));
        }
    }
    out
}

/// The Start menu's app list also holds uninstallers, help files and links.
fn useful_app(name: &str, parsing: &str) -> bool {
    let n = name.to_lowercase();
    let p = parsing.to_lowercase();
    !(n.starts_with("uninstall")
        || n.contains("readme")
        || [".txt", ".url", ".html", ".htm", ".chm", ".pdf", ".rtf", ".ini", ".log"].iter().any(|e| p.ends_with(e)))
}

/// Opens an item: Settings pages directly, apps and Control Panel items
/// through Explorer, which knows their shell names.
pub fn open(target: &str) -> Result<(), String> {
    if target.starts_with("ms-settings:") {
        crate::shell::open(target)
    } else {
        std::process::Command::new("explorer.exe").arg(target).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(kind: Kind, name: &str, kw: &[&str]) -> Item {
        item(kind, name.into(), String::new(), kw)
    }

    #[test]
    fn words_must_start_words() {
        let printers = it(Kind::Setting, "Printers & scanners", &["printer, print, scanner, fax"]);
        let q = |s: &str| s.split_whitespace().map(fold).collect::<Vec<_>>();
        assert_eq!(score(&printers, &q("print")), Some(3));
        assert_eq!(score(&printers, &q("scan")), Some(2));
        assert_eq!(score(&printers, &q("fax")), Some(1));
        assert_eq!(score(&printers, &q("rint")), None);
        assert_eq!(score(&printers, &q("print xyz")), None);
    }

    #[test]
    fn diacritics_do_not_matter() {
        let ro = it(Kind::Setting, "Imprimante și scanere", &[]);
        assert_eq!(score(&ro, &["imprim".into()]), Some(3));
        assert_eq!(score(&ro, &[fold("Scanere")]), Some(2));
    }

    #[test]
    fn file_searches_are_left_alone() {
        assert!(looks_like_syntax("ext:pdf"));
        assert!(looks_like_syntax(r"C:\Users"));
        assert!(!looks_like_syntax("printer"));
    }

    #[test]
    fn every_settings_page_has_a_name() {
        for s in settings() {
            assert!(s.target.starts_with("ms-settings:"));
            assert!(!s.name.is_empty());
        }
    }

    #[test]
    fn helpers_are_not_apps() {
        assert!(!useful_app("Uninstall Foo", "C:\\x\\uninstall.exe"));
        assert!(!useful_app("Foo Readme", "C:\\x\\readme.txt"));
        assert!(useful_app("Microsoft Word", "Microsoft.Office.WINWORD.EXE.15"));
    }

    #[test]
    fn reads_the_start_menu_and_control_panel() {
        let apps = read_folder(r"shell:AppsFolder", Kind::App);
        let cp = read_folder(r"shell:::{26EE0668-A00A-44D7-9371-BEB064C98683}\0", Kind::ControlPanel);
        eprintln!("{} apps, {} Control Panel items", apps.len(), cp.len());
        assert!(apps.len() > 5 && cp.len() > 5);
        let l = Launch { shell: Arc::new(Mutex::new(apps.into_iter().chain(cp).collect())) };
        for q in ["printer", "word", "uninstall", "device manager", "wifi", "notepad", "bluetooth", "control panel", "dark mode"] {
            let names: Vec<String> = l.search(q).iter().map(|i| format!("{:?}: {}", i.kind, i.name)).collect();
            eprintln!("{q:>15} -> {names:?}");
        }
    }
}