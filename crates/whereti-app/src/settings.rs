//! Settings (PLAN.md §10.9): one page, changes apply immediately.
//!
//! App preferences (shortcut, theme) live in `%APPDATA%\whereti\settings.txt`;
//! "start with Windows" is the standard per-user Run key; exclusions belong
//! to the index, so they are read from and sent to the service.

use std::path::PathBuf;

use eframe::egui::{self, Align, Color32, CornerRadius, FontId, Frame, Margin, Sense, Stroke, ThemePreference, Vec2};
use whereti_ipc::ConfigReply;

use crate::app::{chip, icon_button, semibold, state_ro, IconKind, Page, WheretiApp};
use crate::shell;
use crate::theme::Palette;
use crate::tray::HOTKEYS;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
}

pub struct SettingsState {
    /// The user's chosen shortcut; `None` = first free default.
    pub hotkey: Option<String>,
    pub theme: ThemeChoice,
    pub autostart: bool,
    /// The preview panel is open.
    pub preview: bool,
    config: Option<ConfigReply>,
    exclusions: Vec<String>,
    new_exclusion: String,
    exclusions_dirty: bool,
    content_folders: Vec<String>,
    new_content_folder: String,
    content_dirty: bool,
}

fn settings_file() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("whereti").join("settings.txt")
}

impl SettingsState {
    pub fn load() -> SettingsState {
        let mut s = SettingsState {
            hotkey: None,
            theme: ThemeChoice::System,
            autostart: shell::autostart_enabled(),
            preview: true,
            config: None,
            exclusions: Vec::new(),
            new_exclusion: String::new(),
            exclusions_dirty: false,
            content_folders: Vec::new(),
            new_content_folder: String::new(),
            content_dirty: false,
        };
        if let Ok(text) = std::fs::read_to_string(settings_file()) {
            // Tolerate a UTF-8 BOM and stray spaces: the file may be edited by hand.
            for line in text.trim_start_matches('\u{feff}').lines() {
                match line.trim().split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                    Some(("hotkey", v)) if HOTKEYS.iter().any(|h| h.0 == v) => s.hotkey = Some(v.to_string()),
                    Some(("theme", "light")) => s.theme = ThemeChoice::Light,
                    Some(("theme", "dark")) => s.theme = ThemeChoice::Dark,
                    Some(("preview", "off")) => s.preview = false,
                    _ => {}
                }
            }
        }
        s
    }

    pub fn save(&self) {
        let theme = match self.theme {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        };
        let mut text = format!("theme={theme}\npreview={}\n", if self.preview { "on" } else { "off" });
        if let Some(h) = &self.hotkey {
            text.push_str(&format!("hotkey={h}\n"));
        }
        let path = settings_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }

    pub fn apply_theme(&self, ctx: &egui::Context) {
        ctx.set_theme(match self.theme {
            ThemeChoice::System => ThemePreference::System,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::Dark => ThemePreference::Dark,
        });
    }

    pub fn on_config(&mut self, c: ConfigReply) {
        if !self.exclusions_dirty {
            self.exclusions = c.exclusions.clone();
        }
        if !self.content_dirty {
            self.content_folders = c.content_folders.clone();
        }
        self.config = Some(c);
    }
}

/// The settings page, drawn in the central panel.
pub fn show(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let ctx = ui.ctx().clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if icon_button(ui, p, IconKind::Back, false).on_hover_text("Înapoi (Esc)").clicked() {
                app.page = Page::Search;
                app.focus_search = true;
            }
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Setări").size(20.0).family(semibold()).color(p.text));
        });
        ui.add_space(12.0);

        // ---- General
        section(ui, p, "General", |ui| {
            setting_row(ui, p, "Tastă rapidă", "Deschide whereti din orice aplicație.", |ui| {
                let current = app.hotkey_label();
                let mut chosen = None;
                for (label, _, _) in HOTKEYS.iter().rev() {
                    if chip(ui, p, label, current == Some(*label)).clicked() && current != Some(*label) {
                        chosen = Some(*label);
                    }
                }
                if let Some(label) = chosen {
                    let result = match app.tray.as_mut() {
                        Some(t) => t.set_hotkey(label),
                        None => Err("iconița din tray nu e disponibilă".into()),
                    };
                    match result {
                        Ok(k) => {
                            app.settings.hotkey = Some(k.to_string());
                            app.settings.save();
                            app.flash(format!("Tasta rapidă este acum {k}"));
                        }
                        Err(e) => app.flash(e),
                    }
                }
            });
            divider(ui, p);
            setting_row(ui, p, "Pornește cu Windows", "whereti așteaptă în tray, gata de căutare.", |ui| {
                let mut on = app.settings.autostart;
                if toggle(ui, p, &mut on).clicked() {
                    match shell::set_autostart(on) {
                        Ok(()) => app.settings.autostart = on,
                        Err(e) => app.flash(e),
                    }
                }
            });
            divider(ui, p);
            setting_row(ui, p, "Temă", "Urmează tema Windows sau alege una.", |ui| {
                let mut picked = None;
                for (choice, label) in
                    [(ThemeChoice::Dark, "Întunecată"), (ThemeChoice::Light, "Luminoasă"), (ThemeChoice::System, "Ca Windows")]
                {
                    if chip(ui, p, label, app.settings.theme == choice).clicked() {
                        picked = Some(choice);
                    }
                }
                if let Some(c) = picked {
                    app.settings.theme = c;
                    app.settings.apply_theme(&ctx);
                    app.settings.save();
                }
            });
        });

        // ---- Index
        ui.add_space(14.0);
        section(ui, p, "Index", |ui| {
            let (dot, line) = match (&app.offline, &app.status) {
                (Some(_), _) => (p.error, "Serviciul whereti nu rulează".to_string()),
                (None, Some(s)) => {
                    let vols = s.volumes.iter().map(|v| format!("{}:", v.letter)).collect::<Vec<_>>().join(" ");
                    let color = if s.state == "ready" { p.ok } else { p.warn };
                    (color, format!("{} fișiere pe {vols} · {}", shell::count(s.entries), state_ro(&s.state)))
                }
                (None, None) => (p.text3, "se conectează…".to_string()),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, dot);
                ui.label(egui::RichText::new(line).size(13.5).color(p.text));
            });
            divider(ui, p);
            content_block(app, ui, p);
            divider(ui, p);
            ui.label(egui::RichText::new("Foldere excluse").size(14.0).family(semibold()).color(p.text));
            ui.label(
                egui::RichText::new(
                    r"Nu apar în rezultate. C:\Folder = acel folder · \Folder = pe toate discurile · **\node_modules = oriunde. După „Aplică”, indexul se reconstruiește în câteva secunde.",
                )
                .size(12.5)
                .color(p.text2),
            );
            ui.add_space(8.0);
            let mut remove = None;
            for (i, path) in app.settings.exclusions.iter().enumerate() {
                Frame::NONE
                    .fill(p.bg)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin { left: 12, right: 4, top: 4, bottom: 4 })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(path).size(13.0).color(p.text));
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if icon_button(ui, p, IconKind::Close, false).on_hover_text("Scoate").clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                    });
                ui.add_space(4.0);
            }
            if let Some(i) = remove {
                app.settings.exclusions.remove(i);
                app.settings.exclusions_dirty = true;
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut app.settings.new_exclusion)
                    .hint_text(egui::RichText::new(r"C:\Folder  ·  \Folder  ·  **\node_modules").color(p.text3))
                    .font(FontId::proportional(13.5))
                    .margin(Vec2::new(10.0, 6.0))
                    .desired_width(320.0);
                let resp = ui.add(edit);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (button(ui, p, "Adaugă", false).clicked() || enter) && !app.settings.new_exclusion.trim().is_empty() {
                    let path = app.settings.new_exclusion.trim().to_string();
                    if !app.settings.exclusions.contains(&path) {
                        app.settings.exclusions.push(path);
                        app.settings.exclusions_dirty = true;
                    }
                    app.settings.new_exclusion.clear();
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let dirty = app.settings.exclusions_dirty;
                if button(ui, p, "Aplică", true).clicked() && dirty {
                    app.searcher.set_exclusions(app.settings.exclusions.clone());
                    app.settings.exclusions_dirty = false;
                }
                if button(ui, p, "Restabilește implicitele", false).clicked() {
                    if let Some(c) = &app.settings.config {
                        app.settings.exclusions = c.default_exclusions.clone();
                        app.settings.exclusions_dirty = true;
                    }
                }
                if dirty {
                    ui.label(egui::RichText::new("Modificări neaplicate").size(12.5).color(p.warn));
                }
            });
        });

        // ---- About
        ui.add_space(14.0);
        section(ui, p, "Despre", |ui| {
            ui.label(
                egui::RichText::new(format!("whereti {}", env!("CARGO_PKG_VERSION"))).size(14.0).family(semibold()).color(p.text),
            );
            ui.label(
                egui::RichText::new("Căutare instantă de fișiere pentru Windows. Open source, licență MIT. Nimic nu părăsește PC-ul tău.")
                    .size(12.5)
                    .color(p.text2),
            );
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Licența de susținător vine în curând.").size(12.5).color(p.text3));
        });
        ui.add_space(20.0);
    });
}

/// Content indexing: state, progress with time left, and the folders.
fn content_block(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    ui.label(egui::RichText::new("Căutare în conținut").size(14.0).family(semibold()).color(p.text));
    if let Some(c) = app.status.as_ref().map(|s| s.content.clone()) {
        let facts = format!(
            "{} documente indexate · {} pe disc{}",
            shell::count(c.files),
            shell::size(c.disk_bytes),
            if c.failed > 0 { format!(" · {} nu au putut fi citite", shell::count(c.failed)) } else { String::new() }
        );
        let state = match c.state.as_str() {
            "ready" => "la zi".to_string(),
            "indexing" => "se indexează".to_string(),
            "paused: battery" => "în pauză: pe baterie".to_string(),
            "paused: fullscreen" => "în pauză: rulează un joc sau o prezentare".to_string(),
            "paused: busy" => "în pauză: procesorul e ocupat".to_string(),
            "scanning" => "verifică folderele".to_string(),
            "starting" => "pornește".to_string(),
            other => other.to_string(),
        };
        ui.label(egui::RichText::new(format!("{facts} · {state}")).size(12.5).color(p.text2));
        if crate::app::content_active(&c) {
            ui.add_space(6.0);
            let all = (c.done + c.queued).max(1);
            let frac = c.done as f32 / all as f32;
            let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), 8.0), Sense::hover());
            ui.painter().rect_filled(r, 4.0, p.border);
            let mut done = r;
            done.set_width(r.width() * frac);
            ui.painter().rect_filled(done, 4.0, p.accent);
            ui.add_space(4.0);
            let text = crate::app::content_progress(&c, app.content_eta(&c));
            ui.label(egui::RichText::new(text).size(12.5).color(p.text2));
        }
    }
    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(
            "Documentele (PDF, Word, Excel, PowerPoint, OpenDocument, RTF, e-mailuri) sunt căutate pe toate discurile, \
             fără folderele de sistem și de programe — nu trebuie setat nimic.",
        )
        .size(12.5)
        .color(p.text2),
    );
    ui.add_space(10.0);
    ui.label(egui::RichText::new("Fișiere text și cod").size(13.5).family(semibold()).color(p.text));
    ui.label(
        egui::RichText::new(
            "Fișierele .txt, .md, .csv, .html și codul sursă sunt căutate doar în aceste foldere (proiectele au sute de mii). \
             Adaugă un folder dacă vrei să cauți și în codul lui.",
        )
        .size(12.5)
        .color(p.text2),
    );
    ui.add_space(8.0);
    let mut remove = None;
    for (i, path) in app.settings.content_folders.iter().enumerate() {
        Frame::NONE
            .fill(p.bg)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin { left: 12, right: 4, top: 4, bottom: 4 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(path).size(13.0).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, p, IconKind::Close, false).on_hover_text("Scoate").clicked() {
                            remove = Some(i);
                        }
                    });
                });
            });
        ui.add_space(4.0);
    }
    if let Some(i) = remove {
        app.settings.content_folders.remove(i);
        app.settings.content_dirty = true;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(&mut app.settings.new_content_folder)
            .hint_text(egui::RichText::new(r"D:\Acte").color(p.text3))
            .font(FontId::proportional(13.5))
            .margin(Vec2::new(10.0, 6.0))
            .desired_width(320.0);
        let resp = ui.add(edit);
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (button(ui, p, "Adaugă", false).clicked() || enter) && !app.settings.new_content_folder.trim().is_empty() {
            let path = app.settings.new_content_folder.trim().trim_end_matches('\\').to_string();
            if !app.settings.content_folders.iter().any(|x| x.eq_ignore_ascii_case(&path)) {
                app.settings.content_folders.push(path);
                app.settings.content_dirty = true;
            }
            app.settings.new_content_folder.clear();
        }
    });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let dirty = app.settings.content_dirty;
        if button(ui, p, "Aplică", true).clicked() && dirty {
            app.searcher.set_content_folders(app.settings.content_folders.clone());
            app.settings.content_dirty = false;
        }
        if button(ui, p, "Restabilește implicitele", false).clicked() {
            if let Some(c) = &app.settings.config {
                app.settings.content_folders = c.default_content_folders.clone();
                app.settings.content_dirty = true;
            }
        }
        if dirty {
            ui.label(egui::RichText::new("Modificări neaplicate").size(12.5).color(p.warn));
        }
    });
}

/// A titled card.
fn section(ui: &mut egui::Ui, p: &Palette, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(egui::RichText::new(title.to_uppercase()).size(11.0).family(semibold()).color(p.text3));
    });
    ui.add_space(6.0);
    Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .outer_margin(Margin { left: 8, right: 8, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            body(ui);
        });
}

/// Title and description on the left, the control on the right.
fn setting_row(ui: &mut egui::Ui, p: &Palette, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).size(14.0).family(semibold()).color(p.text));
            ui.label(egui::RichText::new(desc).size(12.5).color(p.text2));
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            control(ui);
        });
    });
}

fn divider(ui: &mut egui::Ui, p: &Palette) {
    ui.add_space(10.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border));
    ui.add_space(11.0);
}

/// A pill button; `primary` = solid accent.
fn button(ui: &mut egui::Ui, p: &Palette, text: &str, primary: bool) -> egui::Response {
    let color = if primary { p.on_accent } else { p.text };
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.5, semibold()), color);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + 30.0, 32.0), Sense::click());
    let fill = match (primary, resp.hovered()) {
        (true, false) => p.accent,
        (true, true) => crate::theme::mix(p.accent, Color32::BLACK, 0.12),
        (false, false) => p.surface,
        (false, true) => p.hover,
    };
    let stroke = if primary { Stroke::NONE } else { Stroke::new(1.0, p.border) };
    ui.painter().rect(rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// An on/off switch.
fn toggle(ui: &mut egui::Ui, p: &Palette, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let track = if *on { p.accent } else { p.border };
    ui.painter().rect_filled(rect, 12.0, track);
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
