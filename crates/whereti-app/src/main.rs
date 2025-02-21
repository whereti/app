//! whereti.exe — the search window and tray icon (PLAN.md §10).
//!
//! The app holds no index: it asks the whereti service over the named pipe.
//! It starts hidden in the tray with `--hidden` (autostart).

#![windows_subsystem = "windows"]

mod app;
mod preview;
mod searcher;
mod settings;
mod shell;
mod theme;
mod tray;
mod win;

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

fn main() -> eframe::Result<()> {
    let show_requests = match shell::claim_instance() {
        shell::Instance::First { show_requests } => show_requests,
        // Another whereti is running: it has been asked to show its window.
        shell::Instance::AlreadyRunning => return Ok(()),
    };
    let visible = !std::env::args().any(|a| a == "--hidden");
    let icon = egui::IconData { rgba: tray::magnifier_rgba(), width: 32, height: 32 };
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("whereti")
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([620.0, 380.0])
            .with_visible(visible)
            .with_icon(Arc::new(icon)),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "whereti",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let requests = Arc::new(tray::Requests::default());
            let wake = {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            };
            {
                let (requests, wake) = (requests.clone(), wake.clone());
                shell::listen_for_show_requests(show_requests, move || {
                    requests.show.store(true, std::sync::atomic::Ordering::SeqCst);
                    wake();
                });
            }
            let preferred = settings::SettingsState::load().hotkey;
            let tray = tray::install(requests.clone(), wake, preferred.as_deref()).ok();
            Ok(Box::new(app::WheretiApp::new(&ctx, requests, tray, visible)))
        }),
    )
}

/// Segoe UI (the Windows UI font) for text, egui's fonts as fallback (emoji).
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut load = |name: &str, file: &str| -> bool {
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    let regular = load("segoe", "segoeui.ttf");
    let bold = load("segoe-bold", "segoeuib.ttf");
    let semibold = load("segoe-semibold", "seguisb.ttf");
    if regular {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    let mut bold_family = Vec::new();
    if bold {
        bold_family.push("segoe-bold".to_string());
    }
    bold_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    let mut semibold_family = Vec::new();
    if semibold {
        semibold_family.push("segoe-semibold".to_string());
    }
    semibold_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("semibold".into()), semibold_family);
    ctx.set_fonts(fonts);
}
