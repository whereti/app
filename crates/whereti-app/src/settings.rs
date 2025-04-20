//! Settings (PLAN.md §10.9): one page, changes apply immediately.
//!
//! App preferences (shortcut, theme) live in `%APPDATA%\whereti\settings.txt`;
//! "start with Windows" is the standard per-user Run key; exclusions belong
//! to the index, so they are read from and sent to the service.

use std::path::PathBuf;

use eframe::egui::{self, Align, Color32, CornerRadius, FontId, Frame, Margin, Sense, Stroke, ThemePreference, Vec2};
use whereti_ipc::ConfigReply;

use crate::app::{chip, icon_button, semibold, state_ro, IconKind, Page, WheretiApp};
use crate::i18n::{tr, Lang};
use crate::shell;
use crate::trf;
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
    /// The welcome page was seen and closed.
    pub welcomed: bool,
    /// The preview panel is open.
    pub preview: bool,
    pub lang: Lang,
    config: Option<ConfigReply>,
    exclusions: Vec<String>,
    new_exclusion: String,
    exclusions_dirty: bool,
    content_folders: Vec<String>,
    new_content_folder: String,
    content_dirty: bool,
    /// The automatic exclusion rules are shown one by one.
    show_auto: bool,
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
            welcomed: false,
            preview: true,
            lang: Lang::English,
            config: None,
            exclusions: Vec::new(),
            new_exclusion: String::new(),
            exclusions_dirty: false,
            content_folders: Vec::new(),
            new_content_folder: String::new(),
            content_dirty: false,
            show_auto: false,
        };
        if let Ok(text) = std::fs::read_to_string(settings_file()) {
            // Tolerate a UTF-8 BOM and stray spaces: the file may be edited by hand.
            for line in text.trim_start_matches('\u{feff}').lines() {
                match line.trim().split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                    Some(("hotkey", v)) if HOTKEYS.iter().any(|h| h.0 == v) => s.hotkey = Some(v.to_string()),
                    Some(("theme", "light")) => s.theme = ThemeChoice::Light,
                    Some(("theme", "dark")) => s.theme = ThemeChoice::Dark,
                    Some(("preview", "off")) => s.preview = false,
                    Some(("language", "ro")) => s.lang = Lang::Romanian,
                    Some(("welcomed", "yes")) => s.welcomed = true,
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
        let mut text = format!(
            "theme={theme}\npreview={}\nlanguage={}\nwelcomed={}\n",
            if self.preview { "on" } else { "off" },
            if self.lang == Lang::Romanian { "ro" } else { "en" },
            if self.welcomed { "yes" } else { "no" }
        );
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
            if icon_button(ui, p, IconKind::Back, false).on_hover_text(tr("Back (Esc)", "Înapoi (Esc)")).clicked() {
                app.page = Page::Search;
                app.focus_search = true;
            }
            ui.add_space(4.0);
            ui.label(egui::RichText::new(tr("Settings", "Setări")).size(20.0).family(semibold()).color(p.text));
        });
        ui.add_space(12.0);

        // ---- General
        section(ui, p, tr("General", "General"), |ui| {
            setting_row(ui, p, tr("Language", "Limbă"), tr("The language of the whole app.", "Limba întregii aplicații."), |ui| {
                let mut picked = None;
                for (lang, label) in [(Lang::Romanian, "Română"), (Lang::English, "English")] {
                    if chip(ui, p, label, app.settings.lang == lang).clicked() {
                        picked = Some(lang);
                    }
                }
                if let Some(l) = picked.filter(|l| *l != app.settings.lang) {
                    app.settings.lang = l;
                    crate::i18n::set(l);
                    app.settings.save();
                    if let Some(t) = &app.tray {
                        t.relabel();
                    }
                    app.run_search(); // dates and counts in the new language
                }
            });
            divider(ui, p);
            setting_row(ui, p, tr("Shortcut", "Tastă rapidă"), tr("Opens whereti from any app.", "Deschide whereti din orice aplicație."), |ui| {
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
                        None => Err(tr("the tray icon is not available", "iconița din tray nu e disponibilă").into()),
                    };
                    match result {
                        Ok(k) => {
                            app.settings.hotkey = Some(k.to_string());
                            app.settings.save();
                            app.flash(trf!("The shortcut is now {k}", "Tasta rapidă este acum {k}"));
                        }
                        Err(e) => app.flash(e),
                    }
                }
            });
            divider(ui, p);
            setting_row(
                ui,
                p,
                tr("Start with Windows", "Pornește cu Windows"),
                tr("whereti waits in the tray, ready to search.", "whereti așteaptă în tray, gata de căutare."),
                |ui| {
                let mut on = app.settings.autostart;
                if toggle(ui, p, &mut on).clicked() {
                    match shell::set_autostart(on) {
                        Ok(()) => app.settings.autostart = on,
                        Err(e) => app.flash(e),
                    }
                }
            });
            divider(ui, p);
            setting_row(ui, p, tr("Theme", "Temă"), tr("Follow the Windows theme, or pick one.", "Urmează tema Windows sau alege una."), |ui| {
                let mut picked = None;
                for (choice, label) in
                    [
                    (ThemeChoice::Dark, tr("Dark", "Întunecată")),
                    (ThemeChoice::Light, tr("Light", "Luminoasă")),
                    (ThemeChoice::System, tr("Like Windows", "Ca Windows")),
                ]
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
        section(ui, p, tr("Index", "Index"), |ui| {
            let (dot, line) = match (&app.offline, &app.status) {
                (Some(_), _) => (p.error, tr("The whereti service is not running", "Serviciul whereti nu rulează").to_string()),
                (None, Some(s)) => {
                    let vols = s.volumes.iter().map(|v| format!("{}:", v.letter)).collect::<Vec<_>>().join(" ");
                    let color = if s.state == "ready" { p.ok } else { p.warn };
                    (color, trf!("{} files on {vols} · {}", "{} fișiere pe {vols} · {}", shell::count(s.entries), state_ro(&s.state)))
                }
                (None, None) => (p.text3, tr("connecting…", "se conectează…").to_string()),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, dot);
                ui.label(egui::RichText::new(line).size(13.5).color(p.text));
            });
            divider(ui, p);
            content_block(app, ui, p);
            divider(ui, p);
            ui.label(egui::RichText::new(tr("Excluded folders", "Foldere excluse")).size(14.0).family(semibold()).color(p.text));
            ui.label(
                egui::RichText::new(tr(
                    "They never show up in results. After “Apply”, the index is rebuilt in a few seconds.",
                    "Nu apar deloc în rezultate. După „Aplică”, indexul se reconstruiește în câteva secunde.",
                ))
                    .size(12.5)
                    .color(p.text2),
            );
            ui.add_space(10.0);
            let defaults: Vec<String> =
                app.settings.config.as_ref().map(|c| c.default_exclusions.clone()).unwrap_or_default();
            let is_default = |x: &String| defaults.iter().any(|d| d.eq_ignore_ascii_case(x));
            let auto: Vec<String> = app.settings.exclusions.iter().filter(|x| is_default(x)).cloned().collect();
            let own: Vec<String> = app.settings.exclusions.iter().filter(|x| !is_default(x)).cloned().collect();
            let generated = auto.iter().filter(|x| x.starts_with("**\\")).count();
            let system = auto.len() - generated;
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::hover());
                crate::icons::paint(ui.painter(), r, crate::icons::Icon::Lock, p.text3);
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(trf!(
                        "Left out automatically: {system} system folders and {generated} kinds of generated folders (node_modules, .git, caches…)",
                        "Excluse automat: {system} foldere ale sistemului și {generated} tipuri de foldere generate (node_modules, .git, cache-uri…)"
                    ))
                    .size(12.5)
                    .color(p.text2),
                );
                let label = if app.settings.show_auto { tr("Hide", "Ascunde") } else { tr("Show the list", "Arată lista") };
                if link(ui, p, label).clicked() {
                    app.settings.show_auto = !app.settings.show_auto;
                }
            });
            let mut remove: Option<String> = None;
            if app.settings.show_auto {
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for rule in &auto {
                        if removable_chip(ui, p, &rule_label(rule)).on_hover_text(rule.as_str()).clicked() {
                            remove = Some(rule.clone());
                        }
                    }
                });
            }
            if !own.is_empty() {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(tr("Added by you", "Adăugate de tine")).size(12.5).family(semibold()).color(p.text2));
                ui.add_space(4.0);
                if let Some(i) = path_list(ui, p, &own, &mut app.file_icons) {
                    remove = Some(own[i].clone());
                }
            }
            if let Some(r) = remove {
                app.settings.exclusions.retain(|x| x != &r);
                app.settings.exclusions_dirty = true;
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut app.settings.new_exclusion)
                    .hint_text(egui::RichText::new(tr(r"A folder, e.g. D:\Old archive", r"Un folder, de ex. D:\Arhivă veche")).color(p.text3))
                    .font(FontId::proportional(13.5))
                    .margin(Vec2::new(10.0, 6.0))
                    .desired_width(320.0);
                let resp = ui.add(edit);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (button(ui, p, tr("Add", "Adaugă"), false).clicked() || enter) && !app.settings.new_exclusion.trim().is_empty() {
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
                if button(ui, p, tr("Apply", "Aplică"), true).clicked() && dirty {
                    app.searcher.set_exclusions(app.settings.exclusions.clone());
                    app.settings.exclusions_dirty = false;
                }
                if button(ui, p, tr("Restore defaults", "Restabilește implicitele"), false).clicked() {
                    if let Some(c) = &app.settings.config {
                        app.settings.exclusions = c.default_exclusions.clone();
                        app.settings.exclusions_dirty = true;
                    }
                }
                if dirty {
                    ui.label(egui::RichText::new(tr("Changes not applied", "Modificări neaplicate")).size(12.5).color(p.warn));
                }
            });
        });

        // ---- About
        ui.add_space(14.0);
        section(ui, p, tr("About", "Despre"), |ui| {
            ui.label(
                egui::RichText::new(format!("whereti {}", env!("CARGO_PKG_VERSION"))).size(14.0).family(semibold()).color(p.text),
            );
            ui.label(
                egui::RichText::new(tr(
                    "Instant file search for Windows. Open source, MIT license. Nothing leaves your PC.",
                    "Căutare instantă de fișiere pentru Windows. Open source, licență MIT. Nimic nu părăsește PC-ul tău.",
                ))
                    .size(12.5)
                    .color(p.text2),
            );
            ui.add_space(4.0);
            ui.label(egui::RichText::new(tr("The supporter license is coming soon.", "Licența de susținător vine în curând.")).size(12.5).color(p.text3));
        });
        ui.add_space(20.0);
    });
}

/// Content indexing: state, progress with time left, and the folders.
fn content_block(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    ui.label(egui::RichText::new(tr("Search in contents", "Căutare în conținut")).size(14.0).family(semibold()).color(p.text));
    if let Some(c) = app.status.as_ref().map(|s| s.content.clone()) {
        let facts = trf!(
            "{} documents indexed · {} on disk{}",
            "{} documente indexate · {} pe disc{}",
            shell::count(c.files),
            shell::size(c.disk_bytes),
            if c.failed > 0 { trf!(" · {} could not be read", " · {} nu au putut fi citite", shell::count(c.failed)) } else { String::new() }
        );
        let state = match c.state.as_str() {
            "ready" => tr("up to date", "la zi").to_string(),
            "indexing" => tr("indexing", "se indexează").to_string(),
            "paused: battery" => tr("paused: on battery", "în pauză: pe baterie").to_string(),
            "paused: fullscreen" => tr("paused: a game or presentation is running", "în pauză: rulează un joc sau o prezentare").to_string(),
            "paused: busy" => tr("paused: the processor is busy", "în pauză: procesorul e ocupat").to_string(),
            "scanning" => tr("checking the folders", "verifică folderele").to_string(),
            "starting" => tr("starting", "pornește").to_string(),
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
        egui::RichText::new(tr(
            "Documents (PDF, Word, Excel, PowerPoint, OpenDocument, RTF, e-mails) are searched on every drive, \
             outside system and program folders — nothing to set up.",
            "Documentele (PDF, Word, Excel, PowerPoint, OpenDocument, RTF, e-mailuri) sunt căutate pe toate discurile, \
             fără folderele de sistem și de programe — nu trebuie setat nimic.",
        ))
        .size(12.5)
        .color(p.text2),
    );
    ui.add_space(10.0);
    ui.label(egui::RichText::new(tr("Text and code files", "Fișiere text și cod")).size(13.5).family(semibold()).color(p.text));
    ui.label(
        egui::RichText::new(tr(
            ".txt, .md, .csv, .html files and source code are searched only in these folders (projects hold hundreds of \
             thousands). Add a folder to search inside its code too.",
            "Fișierele .txt, .md, .csv, .html și codul sursă sunt căutate doar în aceste foldere (proiectele au sute de mii). \
             Adaugă un folder dacă vrei să cauți și în codul lui.",
        ))
        .size(12.5)
        .color(p.text2),
    );
    ui.add_space(8.0);
    let folders = app.settings.content_folders.clone();
    if let Some(i) = path_list(ui, p, &folders, &mut app.file_icons) {
        app.settings.content_folders.remove(i);
        app.settings.content_dirty = true;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(&mut app.settings.new_content_folder)
            .hint_text(egui::RichText::new(tr(r"D:\Projects", r"D:\Proiecte")).color(p.text3))
            .font(FontId::proportional(13.5))
            .margin(Vec2::new(10.0, 6.0))
            .desired_width(320.0);
        let resp = ui.add(edit);
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (button(ui, p, tr("Add", "Adaugă"), false).clicked() || enter) && !app.settings.new_content_folder.trim().is_empty() {
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
        if button(ui, p, tr("Apply", "Aplică"), true).clicked() && dirty {
            app.searcher.set_content_folders(app.settings.content_folders.clone());
            app.settings.content_dirty = false;
        }
        if button(ui, p, tr("Restore defaults", "Restabilește implicitele"), false).clicked() {
            if let Some(c) = &app.settings.config {
                app.settings.content_folders = c.default_content_folders.clone();
                app.settings.content_dirty = true;
            }
        }
        if dirty {
            ui.label(egui::RichText::new(tr("Changes not applied", "Modificări neaplicate")).size(12.5).color(p.warn));
        }
    });
}

/// "**\\node_modules" → "node_modules"; "\\Folder" → "Folder (on every drive)".
fn rule_label(rule: &str) -> String {
    if let Some(name) = rule.strip_prefix("**\\") {
        name.to_string()
    } else if let Some(rest) = rule.strip_prefix('\\') {
        trf!("{rest} (on every drive)", "{rest} (pe orice disc)")
    } else {
        rule.to_string()
    }
}

/// A compact list of folders: icon, path, × on hover; fine lines between
/// rows. Returns the index whose × was clicked.
fn path_list(ui: &mut egui::Ui, p: &Palette, paths: &[String], icons: &mut crate::file_icons::FileIcons) -> Option<usize> {
    let mut removed = None;
    Frame::NONE
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, path) in paths.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
                let hovered = ui.rect_contains_pointer(rect);
                if hovered {
                    ui.painter().rect_filled(rect.shrink(1.0), 7.0, p.hover);
                }
                if i > 0 {
                    ui.painter().hline(rect.x_range().shrink(10.0), rect.top(), Stroke::new(1.0, p.line));
                }
                let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(20.0));
                match icons.get(ui.ctx(), path, true) {
                    Some(t) => {
                        let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
                        ui.painter().image(t.id(), icon_rect, uv, Color32::WHITE);
                    }
                    None => crate::icons::paint(ui.painter(), icon_rect, crate::icons::Icon::Folder, p.text3),
                }
                ui.painter().text(
                    egui::pos2(rect.left() + 40.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    path,
                    FontId::proportional(13.0),
                    p.text,
                );
                let x = egui::Rect::from_center_size(egui::pos2(rect.right() - 20.0, rect.center().y), Vec2::splat(26.0));
                let xr = ui.interact(x, resp.id.with("x"), Sense::click());
                if hovered || xr.hovered() {
                    if xr.hovered() {
                        ui.painter().rect_filled(x, 6.0, p.border);
                    }
                    crate::icons::paint(ui.painter(), x.shrink(7.0), crate::icons::Icon::Close, p.text2);
                }
                if xr.on_hover_text(tr("Remove", "Scoate")).clicked() {
                    removed = Some(i);
                }
            }
        });
    removed
}

/// A small chip with a × at the end; the whole chip removes.
fn removable_chip(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(12.5), p.text2);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + 36.0, 26.0), Sense::click());
    let fill = if resp.hovered() { p.hover } else { p.bg };
    ui.painter().rect(rect, 13.0, fill, Stroke::new(1.0, p.line), egui::StrokeKind::Inside);
    ui.painter().galley(egui::pos2(rect.left() + 11.0, rect.center().y - galley.size().y / 2.0), galley, p.text2);
    let x = egui::Rect::from_center_size(egui::pos2(rect.right() - 14.0, rect.center().y), Vec2::splat(10.0));
    crate::icons::paint(ui.painter(), x, crate::icons::Icon::Close, if resp.hovered() { p.text } else { p.text3 });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Accent text that acts as a button.
fn link(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    let resp = ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(p.accent)).sense(Sense::click()));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
