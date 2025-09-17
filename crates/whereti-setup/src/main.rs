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
            .with_inner_size([600.0, 600.0])
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
            let mut state = if uninstalling { State::ConfirmUninstall } else { State::Ready { update: ops::installed_version() } };
            // `--preview <screen>`: shows one screen without doing anything (to check the design)
            if let Some(screen) = args.iter().position(|a| a == "--preview").and_then(|i| args.get(i + 1)) {
                state = match screen.as_str() {
                    "fresh" => State::Ready { update: None },
                    "update" => State::Ready { update: Some("0.0.1".into()) },
                    "working" => State::Working,
                    "installed" => State::Installed { updated: false },
                    "updated" => State::Installed { updated: true },
                    "failed" => State::Failed("cannot replace C:\\Program Files\\whereti\\whereti.exe (is whereti still running?)".into()),
                    "remove" => State::ConfirmUninstall,
                    "removed" => State::Removed,
                    _ => state,
                };
            }
            Ok(Box::new(Setup { state: Arc::new(Mutex::new(state)), autostart: true, logo: None, content_h: 0.0 }))
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
    /// `updated`: an older whereti was replaced (settings and index kept).
    Installed { updated: bool },
    Failed(String),
    ConfirmUninstall,
    Removing,
    Removed,
}

struct Setup {
    state: Arc<Mutex<State>>,
    autostart: bool,
    /// The logo, 128×128, loaded once.
    logo: Option<egui::TextureHandle>,
    /// The height of the last screen drawn, to centre the next one vertically.
    content_h: f32,
}

impl Setup {
    fn start(&self, ctx: &egui::Context, uninstall: bool) {
        let state = self.state.clone();
        let ctx = ctx.clone();
        let autostart = self.autostart;
        let updated = ops::installed_version().is_some();
        *state.lock().unwrap() = if uninstall { State::Removing } else { State::Working };
        std::thread::spawn(move || {
            let result = if uninstall {
                ops::set_autostart(false);
                elevated_or_not("--do-uninstall", ops::uninstall).map(|()| ops::remove_user_settings())
            } else {
                elevated_or_not("--do-install", ops::install).map(|()| ops::set_autostart(autostart))
            };
            *state.lock().unwrap() = match (result, uninstall) {
                (Ok(()), false) => State::Installed { updated },
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

/// The logo at 128×128, shown large on every screen.
fn logo_128() -> Vec<u8> {
    include_bytes!("../../../media/icons/whereti-128.rgba").to_vec()
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

/// What the logo carries on a screen.
#[derive(Clone, Copy, PartialEq)]
enum Mark {
    None,
    /// a ring turning around the logo
    Busy,
    Done,
    Error,
}

/// The logo, large and centred, with a soft shadow and the screen's mark.
fn hero(ui: &mut egui::Ui, l: &Look, logo: &egui::TextureHandle, mark: Mark) {
    let size = 96.0;
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), size + 8.0), Sense::hover());
    let tile = egui::Rect::from_center_size(r.center(), Vec2::splat(size));
    let p = ui.painter();
    // shadow: a few soft layers under the tile
    for (grow, alpha) in [(10.0, 0.05), (6.0, 0.07), (3.0, 0.08)] {
        let shade = if l.dark {
            Color32::from_rgba_unmultiplied(0, 0, 0, (alpha * 2.4 * 255.0) as u8)
        } else {
            Color32::from_rgba_unmultiplied(40, 50, 140, (alpha * 255.0) as u8)
        };
        p.rect_filled(tile.translate(Vec2::new(0.0, 5.0)).expand(grow), 22.0 + grow, shade);
    }
    p.image(logo.id(), tile, egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    match mark {
        Mark::None => {}
        Mark::Busy => {
            let c = tile.center();
            let rad = size * 0.72;
            p.circle_stroke(c, rad, Stroke::new(3.0, l.border));
            let t = ui.input(|i| i.time) as f32 * 3.2;
            let pts: Vec<Pos2> = (0..=24).map(|k| c + Vec2::angled(t + k as f32 * 0.075) * rad).collect();
            p.add(egui::Shape::line(pts, Stroke::new(3.0, l.accent)));
            ui.ctx().request_repaint();
        }
        Mark::Done | Mark::Error => {
            let c = tile.right_bottom() - Vec2::splat(6.0);
            let fill = if mark == Mark::Done { l.ok } else { l.error };
            p.circle_filled(c, 18.0, l.bg);
            p.circle_filled(c, 15.0, fill);
            if mark == Mark::Done {
                let s = Stroke::new(2.6, Color32::WHITE);
                p.line_segment([c + Vec2::new(-6.5, 0.5), c + Vec2::new(-2.0, 5.0)], s);
                p.line_segment([c + Vec2::new(-2.0, 5.0), c + Vec2::new(6.5, -4.5)], s);
            } else {
                p.text(c, egui::Align2::CENTER_CENTER, "!", FontId::new(18.0, semibold()), Color32::WHITE);
            }
        }
    }
}

/// Text centred line by line, wrapped at `width`.
fn centred(ui: &mut egui::Ui, text: &str, font: FontId, color: Color32, width: f32) {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), font, color, width);
    job.halign = egui::Align::Center;
    let g = ui.fonts_mut(|f| f.layout_job(job));
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), g.size().y), Sense::hover());
    ui.painter().galley(Pos2::new(r.center().x, r.top()), g, color);
}

/// Buttons side by side, centred as a group.
fn button_row(ui: &mut egui::Ui, l: &Look, buttons: &[(&str, bool)]) -> Option<usize> {
    let font = FontId::new(14.0, semibold());
    let gap = 10.0;
    let widths: Vec<f32> =
        buttons.iter().map(|(t, _)| ui.painter().layout_no_wrap(t.to_string(), font.clone(), l.text).size().x + 44.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + gap * (buttons.len().saturating_sub(1)) as f32;
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
        for (i, (text, primary)) in buttons.iter().enumerate() {
            if button(ui, l, text, *primary).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let l = look(&ctx);
        let state = self.state.lock().unwrap().clone();
        let logo = self
            .logo
            .get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied([128, 128], &logo_128());
                ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
            })
            .clone();
        egui::CentralPanel::default().frame(Frame::NONE.fill(l.bg).inner_margin(Margin::symmetric(36, 28))).show(ui, |ui| {
            let width = ui.available_width().min(440.0);
            let title = |ui: &mut egui::Ui, text: &str| {
                centred(ui, text, FontId::new(25.0, FontFamily::Name("display".into())), l.text, width);
            };
            let body = |ui: &mut egui::Ui, text: &str| centred(ui, text, FontId::proportional(13.5), l.text2, width);
            // centred vertically, measured on the previous frame
            let space = ((ui.available_height() - self.content_h) / 2.0).max(0.0);
            ui.add_space(space);
            let top = ui.cursor().top();
            match state {
                State::Ready { update } => {
                    hero(ui, &l, &logo, Mark::None);
                    ui.add_space(18.0);
                    // what is installed now decides what this installer offers
                    let installed = update.as_deref().map(|v| version_cmp(v, ops::VERSION));
                    let heading = match (&update, installed) {
                        (Some(_), Some(std::cmp::Ordering::Equal)) => {
                            trf!("whereti {version} is already installed", version = ops::VERSION)
                        }
                        (Some(v), Some(std::cmp::Ordering::Greater)) => {
                            trf!("A newer whereti is installed ({version})", version = v)
                        }
                        (Some(v), _) => trf!("Update whereti {old} → {new}", old = v, new = ops::VERSION),
                        (None, _) => tr("Install whereti").to_string(),
                    };
                    title(ui, &heading);
                    ui.add_space(6.0);
                    body(ui, tr("Instant search for every file on your PC, by its name and by what is written inside PDF, Word, Excel and other documents."));
                    ui.add_space(18.0);
                    // three promises, in a card centred under the text
                    let card_w = width.min(400.0);
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - card_w) / 2.0).max(0.0));
                        Frame::NONE
                            .fill(l.surface)
                            .stroke(Stroke::new(1.0, l.border))
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin::symmetric(16, 12))
                            .show(ui, |ui| {
                                ui.set_width(card_w - 32.0);
                                ui.vertical(|ui| {
                                    for line in [
                                        tr("Free and open source (MIT). Your files never leave your PC."),
                                        tr("Builds its index in the background, without slowing the PC."),
                                        &trf!("Open it anywhere with {hotkey}.", hotkey = "Ctrl+Shift+Space"),
                                    ] {
                                        ui.horizontal(|ui| {
                                            let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
                                            ui.painter().circle_filled(r.center(), 3.0, l.accent);
                                            ui.add(egui::Label::new(egui::RichText::new(line).size(13.0).color(l.text)).wrap());
                                        });
                                        ui.add_space(3.0);
                                    }
                                });
                            });
                    });
                    ui.add_space(14.0);
                    let fresh = !matches!(installed, Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater));
                    if fresh {
                        ui.vertical_centered(|ui| {
                            let label = egui::RichText::new(tr("Start whereti with Windows")).size(13.0).color(l.text);
                            ui.checkbox(&mut self.autostart, label);
                        });
                        ui.add_space(14.0);
                    }
                    if !fresh {
                        // nothing to update: open it, or install this copy again on purpose
                        let again = if installed == Some(std::cmp::Ordering::Equal) {
                            tr("Reinstall")
                        } else {
                            tr("Install this older version")
                        };
                        match button_row(ui, &l, &[(tr("Open whereti"), true), (again, false)]) {
                            Some(0) => {
                                ops::launch_app();
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            Some(_) => self.start(&ctx, false),
                            None => {}
                        }
                    } else {
                        let label = if update.is_some() { tr("Update") } else { tr("Install") };
                        if button_row(ui, &l, &[(label, true)]).is_some() {
                            self.start(&ctx, false);
                        }
                    }
                    ui.add_space(12.0);
                    let note = format!("{} · {}", ops::install_dir().display(), tr("asks for administrator permission once"));
                    centred(ui, &note, FontId::proportional(11.5), l.text3, width);
                }
                State::Working | State::Removing => {
                    hero(ui, &l, &logo, Mark::Busy);
                    ui.add_space(46.0);
                    let text = if matches!(state, State::Working) {
                        tr("Installing whereti…")
                    } else {
                        tr("Removing whereti…")
                    };
                    title(ui, text);
                    ui.add_space(6.0);
                    body(ui, tr("This takes a few seconds."));
                }
                State::Installed { updated } => {
                    hero(ui, &l, &logo, Mark::Done);
                    ui.add_space(22.0);
                    if updated {
                        title(ui, &trf!("whereti is updated to {version}", version = ops::VERSION));
                        ui.add_space(6.0);
                        body(ui, tr("Your settings and your index are kept, so you can search right away."));
                    } else {
                        title(ui, tr("whereti is installed"));
                        ui.add_space(6.0);
                        body(ui, tr("It is building its index now. That takes about a minute, and whereti shows the progress. You can search right away."));
                    }
                    ui.add_space(24.0);
                    match button_row(ui, &l, &[(tr("Open whereti"), true), (tr("Close"), false)]) {
                        Some(0) => {
                            if updated {
                                ops::open_app();
                            } else {
                                ops::launch_app();
                            }
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Some(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        None => {}
                    }
                }
                State::Failed(e) => {
                    hero(ui, &l, &logo, Mark::Error);
                    ui.add_space(22.0);
                    title(ui, tr("Something went wrong"));
                    ui.add_space(6.0);
                    body(ui, &e);
                    ui.add_space(24.0);
                    if button_row(ui, &l, &[(tr("Try again"), true)]).is_some() {
                        *self.state.lock().unwrap() = State::Ready { update: ops::installed_version() };
                    }
                }
                State::ConfirmUninstall => {
                    hero(ui, &l, &logo, Mark::None);
                    ui.add_space(22.0);
                    title(ui, tr("Remove whereti?"));
                    ui.add_space(6.0);
                    body(ui, tr("The app, its search service and its index are removed. Your own files are not touched."));
                    ui.add_space(24.0);
                    match button_row(ui, &l, &[(tr("Remove"), true), (tr("Cancel"), false)]) {
                        Some(0) => self.start(&ctx, true),
                        Some(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        None => {}
                    }
                }
                State::Removed => {
                    hero(ui, &l, &logo, Mark::Done);
                    ui.add_space(22.0);
                    title(ui, tr("whereti was removed"));
                    ui.add_space(6.0);
                    body(ui, tr("Thank you for trying it."));
                    ui.add_space(24.0);
                    if button_row(ui, &l, &[(tr("Close"), true)]).is_some() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            let h = ui.cursor().top() - ui.spacing().item_spacing.y - top;
            if (h - self.content_h).abs() > 0.5 {
                self.content_h = h;
                ctx.request_repaint();
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
