//! whereti-setup.exe — installs, updates and removes whereti.
//!
//!   whereti-setup.exe              the installer window
//!   whereti-setup.exe /S           silent install (winget, scripts)
//!   uninstall.exe /uninstall       the uninstall window (Settings → Apps)
//!   uninstall.exe /uninstall /S    silent uninstall
//!
//! The window runs as the user. Only the part that writes to Program Files,
//! installs the service and registers whereti in Settings → Apps runs
//! elevated, after one UAC prompt (`--do-install` / `--do-uninstall`).
//! Afterwards the normal part sets "start with Windows" for this user and
//! opens whereti — as the user, not as administrator.

#![windows_subsystem = "windows"]

mod ops;

use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Pos2, Sense,
    Stroke, Vec2,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x.eq_ignore_ascii_case(a));
    let report = args.iter().position(|a| a == "--report").and_then(|i| args.get(i + 1)).cloned();

    // the elevated half: do the work, write what went wrong, exit
    if has("--do-install") || has("--do-uninstall") {
        let result = if has("--do-install") { ops::install(&mut |_| {}) } else { ops::uninstall(&mut |_| {}) };
        if let (Err(e), Some(path)) = (&result, report) {
            let _ = std::fs::write(path, e);
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }

    let uninstalling = has("/uninstall") || has("--uninstall");
    if has("/S") || has("/silent") {
        let result = if uninstalling { silent_uninstall() } else { silent_install() };
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(if uninstalling { "Uninstall whereti" } else { "whereti setup" })
            .with_inner_size([560.0, 460.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(Arc::new(egui::IconData { rgba: magnifier_rgba(), width: 32, height: 32 })),
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "whereti-setup",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let state = if uninstalling { State::ConfirmUninstall } else { State::Ready { update: ops::installed_version() } };
            Ok(Box::new(Setup { state: Arc::new(Mutex::new(state)), autostart: true, ro: system_is_romanian() }))
        }),
    );
    // No window could be opened (graphics driver…): say why somewhere findable.
    if let Err(e) = result {
        let _ = std::fs::write(std::env::temp_dir().join("whereti-setup-error.txt"), e.to_string());
        std::process::exit(3);
    }
}

/// The install or uninstall step (it reports progress lines, unused in the window for now).
type Step = fn(&mut dyn FnMut(&str)) -> Result<(), String>;

fn elevated_or_not(args: &str, direct: Step) -> Result<(), String> {
    if ops::is_elevated() {
        direct(&mut |_| {})
    } else {
        ops::run_elevated(args)
    }
}

fn silent_install() -> Result<(), String> {
    elevated_or_not("--do-install", ops::install)?;
    ops::set_autostart(true);
    Ok(())
}

fn silent_uninstall() -> Result<(), String> {
    ops::set_autostart(false);
    elevated_or_not("--do-uninstall", ops::uninstall)
}

#[derive(Clone)]
enum State {
    Ready { update: Option<String> },
    Working,
    Installed,
    Failed(String),
    ConfirmUninstall,
    Removing,
    Removed,
}

struct Setup {
    state: Arc<Mutex<State>>,
    autostart: bool,
    ro: bool,
}

/// Windows set to Romanian: the installer speaks Romanian (the app is
/// English by default and has its own choice in Settings).
fn system_is_romanian() -> bool {
    windows_ui_language() & 0x3FF == 0x18
}

fn windows_ui_language() -> u16 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // SAFETY: no arguments, returns a LANGID.
    unsafe { GetUserDefaultUILanguage() }
}

impl Setup {
    fn t<'a>(&self, en: &'a str, ro: &'a str) -> &'a str {
        if self.ro {
            ro
        } else {
            en
        }
    }

    fn start(&self, ctx: &egui::Context, uninstall: bool) {
        let state = self.state.clone();
        let ctx = ctx.clone();
        let autostart = self.autostart;
        *state.lock().unwrap() = if uninstall { State::Removing } else { State::Working };
        std::thread::spawn(move || {
            let result = if uninstall {
                ops::set_autostart(false);
                elevated_or_not("--do-uninstall", ops::uninstall)
            } else {
                elevated_or_not("--do-install", ops::install).map(|()| ops::set_autostart(autostart))
            };
            *state.lock().unwrap() = match (result, uninstall) {
                (Ok(()), false) => State::Installed,
                (Ok(()), true) => State::Removed,
                (Err(e), false) if e == "cancelled" => State::Ready { update: ops::installed_version() },
                (Err(e), true) if e == "cancelled" => State::ConfirmUninstall,
                (Err(e), _) => State::Failed(e),
            };
            ctx.request_repaint();
        });
    }
}

/// The whereti magnifier (the same drawing as the app's tray icon).
fn magnifier_rgba() -> Vec<u8> {
    let n = 32usize;
    let mut px = vec![0u8; n * n * 4];
    let (cx, cy, r) = (13.0f32, 13.0f32, 9.0f32);
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let ring = (d - r).abs() < 2.2;
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

/// Compares "0.10.2" with "0.9.0" by numbers, not as text.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(a).cmp(&parts(b))
}

// ---------------------------------------------------------------- look

struct Look {
    dark: bool,
    bg: Color32,
    surface: Color32,
    border: Color32,
    text: Color32,
    text2: Color32,
    text3: Color32,
    accent: Color32,
    ok: Color32,
    error: Color32,
}

fn look(ctx: &egui::Context) -> Look {
    let dark = ctx.theme() == egui::Theme::Dark;
    let accent = Color32::from_rgb(0x00, 0x67, 0xC0);
    if dark {
        Look {
            dark,
            bg: Color32::from_rgb(0x1A, 0x1A, 0x1D),
            surface: Color32::from_rgb(0x23, 0x23, 0x27),
            border: Color32::from_rgb(0x36, 0x36, 0x3C),
            text: Color32::from_rgb(0xF0, 0xF0, 0xF3),
            text2: Color32::from_rgb(0xA8, 0xA8, 0xB2),
            text3: Color32::from_rgb(0x78, 0x78, 0x82),
            accent: Color32::from_rgb(0x4C, 0xA0, 0xE8),
            ok: Color32::from_rgb(0x4C, 0xC3, 0x8A),
            error: Color32::from_rgb(0xF2, 0x6D, 0x6D),
        }
    } else {
        Look {
            dark,
            bg: Color32::from_rgb(0xF7, 0xF7, 0xF9),
            surface: Color32::WHITE,
            border: Color32::from_rgb(0xE2, 0xE2, 0xE8),
            text: Color32::from_rgb(0x16, 0x16, 0x1B),
            text2: Color32::from_rgb(0x5E, 0x5E, 0x6A),
            text3: Color32::from_rgb(0x92, 0x92, 0x9E),
            accent,
            ok: Color32::from_rgb(0x1F, 0x9D, 0x5C),
            error: Color32::from_rgb(0xD1, 0x3B, 0x3B),
        }
    }
}

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// Segoe UI Variable (Windows 11), else Segoe UI.
fn setup_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontTweak, VariationCoords};
    let mut fonts = FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let variable = std::fs::read(dir.join("SegUIVar.ttf")).ok().map(Arc::new);
    for (name, wght, opsz, file) in [("body", 400.0, 11.0, "segoeui.ttf"), ("semibold", 600.0, 11.0, "seguisb.ttf"), ("display", 600.0, 28.0, "seguisb.ttf")] {
        let data = match &variable {
            Some(bytes) => {
                let blob: egui::epaint::text::Blob = bytes.clone();
                Some(FontData::from_blob(blob, 0).tweak(FontTweak {
                    coords: VariationCoords::new([(b"wght", wght), (b"opsz", opsz)]),
                    ..Default::default()
                }))
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

fn button(ui: &mut egui::Ui, l: &Look, text: &str, primary: bool) -> egui::Response {
    let color = if primary { Color32::WHITE } else { l.text };
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::new(14.0, semibold()), color);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 40.0, 40.0), Sense::click());
    let fill = match (primary, resp.hovered()) {
        (true, false) => l.accent,
        (true, true) => l.accent.gamma_multiply(0.88),
        (false, false) => l.surface,
        (false, true) => l.border,
    };
    let stroke = if primary { Stroke::NONE } else { Stroke::new(1.0, l.border) };
    ui.painter().rect(rect, 10.0, fill, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn badge(ui: &mut egui::Ui, l: &Look, kind: u8) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
    let c = r.center();
    let p = ui.painter();
    match kind {
        // magnifier: what whereti is
        0 => {
            p.circle_filled(c, 32.0, l.accent.gamma_multiply(if l.dark { 0.25 } else { 0.12 }));
            p.circle_stroke(c - Vec2::splat(3.0), 11.0, Stroke::new(2.4, l.accent));
            p.line_segment([c + Vec2::splat(5.0), c + Vec2::splat(13.0)], Stroke::new(3.0, l.accent));
        }
        // check: done
        1 => {
            p.circle_filled(c, 30.0, l.ok);
            let s = Stroke::new(3.0, Color32::WHITE);
            p.line_segment([c + Vec2::new(-11.0, 1.0), c + Vec2::new(-3.0, 9.0)], s);
            p.line_segment([c + Vec2::new(-3.0, 9.0), c + Vec2::new(12.0, -7.0)], s);
        }
        // error
        _ => {
            p.circle_filled(c, 30.0, l.error);
            p.text(c, egui::Align2::CENTER_CENTER, "!", FontId::new(26.0, semibold()), Color32::WHITE);
        }
    }
}

fn spinner(ui: &mut egui::Ui, l: &Look) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
    let c = r.center();
    ui.painter().circle_stroke(c, 24.0, Stroke::new(3.0, l.border));
    let t = ui.input(|i| i.time) as f32 * 4.5;
    let pts: Vec<Pos2> = (0..=20).map(|k| c + Vec2::angled(t + k as f32 * 0.09) * 24.0).collect();
    ui.painter().add(egui::Shape::line(pts, Stroke::new(3.0, l.accent)));
    ui.ctx().request_repaint();
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let l = look(&ctx);
        let state = self.state.lock().unwrap().clone();
        egui::CentralPanel::default().frame(Frame::NONE.fill(l.bg).inner_margin(Margin::same(32))).show(ui, |ui| {
            let title = |ui: &mut egui::Ui, text: &str| {
                ui.label(egui::RichText::new(text).size(24.0).family(FontFamily::Name("display".into())).color(l.text));
            };
            let body = |ui: &mut egui::Ui, text: &str| {
                ui.add(egui::Label::new(egui::RichText::new(text).size(13.5).color(l.text2)).wrap());
            };
            match state {
                State::Ready { update } => {
                    badge(ui, &l, 0);
                    ui.add_space(14.0);
                    // what is installed now decides what this installer offers
                    let installed = update.as_deref().map(|v| version_cmp(v, ops::VERSION));
                    match (&update, installed) {
                        (Some(_), Some(std::cmp::Ordering::Equal)) => title(
                            ui,
                            &format!("whereti {} {}", ops::VERSION, self.t("is already installed", "e deja instalat")),
                        ),
                        (Some(v), Some(std::cmp::Ordering::Greater)) => title(
                            ui,
                            &format!("{} ({v})", self.t("A newer whereti is installed", "E instalată o versiune mai nouă")),
                        ),
                        (Some(v), _) => title(ui, &format!("{} {v} → {}", self.t("Update whereti", "Actualizează whereti"), ops::VERSION)),
                        (None, _) => title(ui, self.t("Install whereti", "Instalează whereti")),
                    }
                    ui.add_space(4.0);
                    body(ui, self.t(
                        "Instant search for every file on your PC — by its name, and by what is written inside PDF, Word, Excel and other documents.",
                        "Căutare instantă pentru orice fișier de pe PC — după nume și după ce scrie în PDF-uri, Word, Excel și alte documente.",
                    ));
                    ui.add_space(16.0);
                    Frame::NONE
                        .fill(l.surface)
                        .stroke(Stroke::new(1.0, l.border))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for line in [
                                self.t("Free and open source (MIT). Nothing leaves your PC.", "Gratuit și open source (MIT). Nimic nu părăsește PC-ul."),
                                self.t("Builds its index in the background, without slowing the PC.", "Își face indexul în fundal, fără să încetinească PC-ul."),
                                self.t("Open it anywhere with Ctrl+Shift+Space.", "Se deschide de oriunde cu Ctrl+Shift+Space."),
                            ] {
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(Vec2::new(16.0, 18.0), Sense::hover());
                                    ui.painter().circle_filled(r.center(), 3.0, l.accent);
                                    ui.label(egui::RichText::new(line).size(13.0).color(l.text));
                                });
                                ui.add_space(4.0);
                            }
                        });
                    ui.add_space(12.0);
                    let label = egui::RichText::new(self.t("Start whereti with Windows", "Pornește whereti odată cu Windows")).size(13.0).color(l.text);
                    ui.checkbox(&mut self.autostart, label);
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        if matches!(installed, Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater)) {
                            // nothing to update: open it, or install this copy again on purpose
                            if button(ui, &l, self.t("Open whereti", "Deschide whereti"), true).clicked() {
                                ops::launch_app();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            ui.add_space(8.0);
                            let again = if installed == Some(std::cmp::Ordering::Equal) {
                                self.t("Reinstall", "Reinstalează")
                            } else {
                                self.t("Install this older version", "Instalează versiunea mai veche")
                            };
                            if button(ui, &l, again, false).clicked() {
                                self.start(&ctx, false);
                            }
                            return;
                        }
                        let label = if update.is_some() { self.t("Update", "Actualizează") } else { self.t("Install", "Instalează") };
                        if button(ui, &l, label, true).clicked() {
                            self.start(&ctx, false);
                        }
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                ops::install_dir().display(),
                                self.t("asks for administrator permission once", "cere o dată permisiunea de administrator")
                            ))
                            .size(11.5)
                            .color(l.text3),
                        );
                    });
                }
                State::Working | State::Removing => {
                    ui.add_space(50.0);
                    ui.vertical_centered(|ui| {
                        spinner(ui, &l);
                        ui.add_space(16.0);
                        let text = if matches!(state, State::Working) {
                            self.t("Installing whereti…", "Se instalează whereti…")
                        } else {
                            self.t("Removing whereti…", "Se elimină whereti…")
                        };
                        title(ui, text);
                        ui.add_space(4.0);
                        body(ui, self.t("This takes a few seconds.", "Durează câteva secunde."));
                    });
                }
                State::Installed => {
                    badge(ui, &l, 1);
                    ui.add_space(14.0);
                    title(ui, self.t("whereti is installed", "whereti e instalat"));
                    ui.add_space(4.0);
                    body(ui, self.t(
                        "It is building its index now — that takes about a minute, and whereti shows the progress. You can search right away.",
                        "Își construiește acum indexul — durează cam un minut, iar whereti arată progresul. Poți căuta imediat.",
                    ));
                    ui.add_space(22.0);
                    ui.horizontal(|ui| {
                        if button(ui, &l, self.t("Open whereti", "Deschide whereti"), true).clicked() {
                            ops::launch_app();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        ui.add_space(8.0);
                        if button(ui, &l, self.t("Close", "Închide"), false).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                State::Failed(e) => {
                    badge(ui, &l, 2);
                    ui.add_space(14.0);
                    title(ui, self.t("Something went wrong", "Ceva n-a mers"));
                    ui.add_space(4.0);
                    body(ui, &e);
                    ui.add_space(22.0);
                    if button(ui, &l, self.t("Try again", "Încearcă din nou"), true).clicked() {
                        *self.state.lock().unwrap() = State::Ready { update: ops::installed_version() };
                    }
                }
                State::ConfirmUninstall => {
                    badge(ui, &l, 0);
                    ui.add_space(14.0);
                    title(ui, self.t("Remove whereti?", "Elimini whereti?"));
                    ui.add_space(4.0);
                    body(ui, self.t(
                        "The app, its search service and its index are removed. Your own files are not touched.",
                        "Se elimină aplicația, serviciul de căutare și indexul. Fișierele tale nu sunt atinse.",
                    ));
                    ui.add_space(22.0);
                    ui.horizontal(|ui| {
                        if button(ui, &l, self.t("Remove", "Elimină"), true).clicked() {
                            self.start(&ctx, true);
                        }
                        ui.add_space(8.0);
                        if button(ui, &l, self.t("Cancel", "Anulează"), false).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                State::Removed => {
                    badge(ui, &l, 1);
                    ui.add_space(14.0);
                    title(ui, self.t("whereti was removed", "whereti a fost eliminat"));
                    ui.add_space(4.0);
                    body(ui, self.t("Thank you for trying it.", "Mulțumim că l-ai încercat."));
                    ui.add_space(22.0);
                    if button(ui, &l, self.t("Close", "Închide"), true).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::version_cmp;
    use std::cmp::Ordering::*;

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(version_cmp("0.10.0", "0.9.3"), Greater);
        assert_eq!(version_cmp("0.0.1", "0.0.1"), Equal);
        assert_eq!(version_cmp("1.0.0", "1.0.1"), Less);
    }
}
