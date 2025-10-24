//! whereti.exe - the search window and tray icon (PLAN.md §10).
//!
//! The app holds no index: it asks the whereti service over the named pipe.
//! It starts hidden in the tray with `--hidden` (autostart).

#![windows_subsystem = "windows"]

mod app;
mod file_icons;
mod i18n;
mod icons;
mod launch;
mod license;
mod links;
mod preview;
mod recent;
mod searcher;
mod snippets;
mod settings;
mod shell;
mod theme;
mod tray;
mod update;
mod usage;
mod welcome;
mod win;

use std::sync::Arc;

#[allow(unused_imports)]
pub(crate) use whereti_i18n::{trf, trn};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

fn main() -> eframe::Result<()> {
    let show_requests = match shell::claim_instance() {
        shell::Instance::First { show_requests } => show_requests,
        // Another whereti is running: it has been asked to show its window.
        shell::Instance::AlreadyRunning => return Ok(()),
    };
    let visible = !std::env::args().any(|a| a == "--hidden");
    tray::set_supporter(license::load().is_some());
    let icon = egui::IconData { rgba: tray::logo_rgba_64(), width: 64, height: 64 };
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
            setup_fonts(&cc.egui_ctx, false);
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
            let saved = settings::SettingsState::load();
            i18n::set(saved.effective_lang());
            let preferred = saved.hotkey;
            let tray = tray::install(requests.clone(), wake, preferred.as_deref()).ok();
            Ok(Box::new(app::WheretiApp::new(&ctx, requests, tray, visible)))
        }),
    )
}

/// Segoe UI Variable - the font of the Windows 11 apps - in several weights
/// from the one variable font file, with egui's fonts as fallback (emoji,
/// symbols). On Windows 10 (no variable font): classic Segoe UI files.
/// `cjk`: also Microsoft YaHei (Chinese, Japanese kana) and Malgun Gothic
/// (Korean), loaded only once such a name shows up: they take ~30 MB.
pub(crate) fn setup_fonts(ctx: &egui::Context, cjk: bool) {
    use egui::epaint::text::{FontTweak, VariationCoords};
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if cjk {
        for (name, file) in [("cjk-zh", "msyh.ttc"), ("cjk-ko", "malgun.ttf")] {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
                fallback.insert(0, name.to_string());
            }
        }
    }
    // (family, weight, optical size, classic Segoe UI file)
    let styles: [(&str, f32, f32, &str); 5] = [
        ("body", 400.0, 11.0, "segoeui.ttf"),
        ("medium", 500.0, 11.0, "seguisb.ttf"),
        ("semibold", 600.0, 11.0, "seguisb.ttf"),
        ("bold", 700.0, 11.0, "segoeuib.ttf"),
        ("display", 600.0, 28.0, "seguisb.ttf"),
    ];
    let variable = std::fs::read(dir.join("SegUIVar.ttf")).ok().map(Arc::new);
    for (name, wght, opsz, file) in styles {
        let data = match &variable {
            Some(bytes) => {
                let blob: egui::epaint::text::Blob = bytes.clone();
                let coords = VariationCoords::new([(b"wght", wght), (b"opsz", opsz)]);
                Some(FontData::from_blob(blob, 0).tweak(FontTweak { coords, ..Default::default() }))
            }
            None => std::fs::read(dir.join(file)).ok().map(FontData::from_owned),
        };
        let mut family = Vec::new();
        if let Some(d) = data {
            fonts.font_data.insert(name.into(), Arc::new(d));
            family.push(name.to_string());
        }
        family.extend(fallback.iter().cloned());
        if name == "body" {
            fonts.families.insert(FontFamily::Proportional, family.clone());
        }
        fonts.families.insert(FontFamily::Name(name.into()), family);
    }
    ctx.set_fonts(fonts);
}
