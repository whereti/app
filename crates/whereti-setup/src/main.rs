//! whereti-setup.exe - installs, updates and removes whereti.
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
//! opens whereti - as the user, not as administrator.

#![windows_subsystem = "windows"]

mod ops;

use std::sync::{Arc, Mutex};

use whereti_i18n::{tr, trf};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Pos2, Sense,
    Stroke, Vec2,
};

fn main() {
    // the installer speaks the Windows display language
    whereti_i18n::set(whereti_i18n::Lang::system());
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
        // `/open`: an update started from whereti itself, which the install
        // closed; open the new version so the user is back where they were
        if result.is_ok() && !uninstalling && has("/open") {
            ops::open_app();
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(if uninstalling { "Uninstall whereti" } else { "whereti setup" })
            .with_inner_size([560.0, 460.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(Arc::new(egui::IconData { rgba: logo_rgba(), width: 64, height: 64 })),
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "whereti-setup",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let state = if uninstalling { State::ConfirmUninstall } else { State::Ready { update: ops::installed_version() } };
            Ok(Box::new(Setup { state: Arc::new(Mutex::new(state)), autostart: true }))
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
    elevated_or_not("--do-uninstall", ops::uninstall)?;
    ops::remove_user_settings();
    Ok(())
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
}

impl Setup {
    fn start(&self, ctx: &egui::Context, uninstall: bool) {
        let state = self.state.clone();
        let ctx = ctx.clone();
        let autostart = self.autostart;
        *state.lock().unwrap() = if uninstall { State::Removing } else { State::Working };
        std::thread::spawn(move || {
            let result = if uninstall {
                ops::set_autostart(false);
                elevated_or_not("--do-uninstall", ops::uninstall).map(|()| ops::remove_user_settings())
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

/// The whereti logo, 64×64 (rendered by tools/render-icons).
fn logo_rgba() -> Vec<u8> {
    include_bytes!("../../../media/icons/whereti-64.rgba").to_vec()
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
                            &trf!("whereti {version} is already installed", version = ops::VERSION),
                        ),
                        (Some(v), Some(std::cmp::Ordering::Greater)) => title(
                            ui,
                            &trf!("A newer whereti is installed ({version})", version = v),
                        ),
                        (Some(v), _) => title(ui, &trf!("Update whereti {old} → {new}", old = v, new = ops::VERSION)),
                        (None, _) => title(ui, tr("Install whereti")),
                    }
                    ui.add_space(4.0);
                    body(ui, tr("Instant search for every file on your PC, by its name and by what is written inside PDF, Word, Excel and other documents."));
                    ui.add_space(16.0);
                    Frame::NONE
                        .fill(l.surface)
                        .stroke(Stroke::new(1.0, l.border))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for line in [
                                tr("Free and open source (MIT). Nothing leaves your PC."),
                                tr("Builds its index in the background, without slowing the PC."),
                                &trf!("Open it anywhere with {hotkey}.", hotkey = "Ctrl+Shift+Space"),
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
                    let label = egui::RichText::new(tr("Start whereti with Windows")).size(13.0).color(l.text);
                    ui.checkbox(&mut self.autostart, label);
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        if matches!(installed, Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater)) {
                            // nothing to update: open it, or install this copy again on purpose
                            if button(ui, &l, tr("Open whereti"), true).clicked() {
                                ops::launch_app();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            ui.add_space(8.0);
                            let again = if installed == Some(std::cmp::Ordering::Equal) {
                                tr("Reinstall")
                            } else {
                                tr("Install this older version")
                            };
                            if button(ui, &l, again, false).clicked() {
                                self.start(&ctx, false);
                            }
                            return;
                        }
                        let label = if update.is_some() { tr("Update") } else { tr("Install") };
                        if button(ui, &l, label, true).clicked() {
                            self.start(&ctx, false);
                        }
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {}",
                                ops::install_dir().display(),
                                tr("asks for administrator permission once")
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
                            tr("Installing whereti…")
                        } else {
                            tr("Removing whereti…")
                        };
                        title(ui, text);
                        ui.add_space(4.0);
                        body(ui, tr("This takes a few seconds."));
                    });
                }
                State::Installed => {
                    badge(ui, &l, 1);
                    ui.add_space(14.0);
                    title(ui, tr("whereti is installed"));
                    ui.add_space(4.0);
                    body(ui, tr("It is building its index now. That takes about a minute, and whereti shows the progress. You can search right away."));
                    ui.add_space(22.0);
                    ui.horizontal(|ui| {
                        if button(ui, &l, tr("Open whereti"), true).clicked() {
                            ops::launch_app();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        ui.add_space(8.0);
                        if button(ui, &l, tr("Close"), false).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                State::Failed(e) => {
                    badge(ui, &l, 2);
                    ui.add_space(14.0);
                    title(ui, tr("Something went wrong"));
                    ui.add_space(4.0);
                    body(ui, &e);
                    ui.add_space(22.0);
                    if button(ui, &l, tr("Try again"), true).clicked() {
                        *self.state.lock().unwrap() = State::Ready { update: ops::installed_version() };
                    }
                }
                State::ConfirmUninstall => {
                    badge(ui, &l, 0);
                    ui.add_space(14.0);
                    title(ui, tr("Remove whereti?"));
                    ui.add_space(4.0);
                    body(ui, tr("The app, its search service and its index are removed. Your own files are not touched."));
                    ui.add_space(22.0);
                    ui.horizontal(|ui| {
                        if button(ui, &l, tr("Remove"), true).clicked() {
                            self.start(&ctx, true);
                        }
                        ui.add_space(8.0);
                        if button(ui, &l, tr("Cancel"), false).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                State::Removed => {
                    badge(ui, &l, 1);
                    ui.add_space(14.0);
                    title(ui, tr("whereti was removed"));
                    ui.add_space(4.0);
                    body(ui, tr("Thank you for trying it."));
                    ui.add_space(22.0);
                    if button(ui, &l, tr("Close"), true).clicked() {
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
