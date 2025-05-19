//! Settings (PLAN.md §10.9): General, Search and About, like Windows
//! Settings; every change applies immediately.
//!
//! App preferences (shortcut, theme) live in `%APPDATA%\whereti\settings.txt`;
//! "start with Windows" is the standard per-user Run key; exclusions belong
//! to the index, so they are read from and sent to the service.

use std::path::PathBuf;

use eframe::egui::{self, Align, Color32, CornerRadius, FontId, Frame, Margin, Sense, Stroke, ThemePreference, Vec2};

use crate::icons::Icon;
use whereti_ipc::ConfigReply;

use crate::app::{icon_button, semibold, state_ro, IconKind, Page, WheretiApp};
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
    /// The part of Settings shown.
    pub section: Section,
    /// Waiting for the keys of a new shortcut.
    pub recording: bool,
    logo: Option<egui::TextureHandle>,
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
            section: Section::General,
            recording: false,
            logo: None,
        };
        if let Ok(text) = std::fs::read_to_string(settings_file()) {
            // Tolerate a UTF-8 BOM and stray spaces: the file may be edited by hand.
            for line in text.trim_start_matches('\u{feff}').lines() {
                match line.trim().split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                    Some(("hotkey", v)) if crate::tray::parse(v).is_some() => s.hotkey = Some(v.to_string()),
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
        self.exclusions = c.exclusions.clone();
        self.config = Some(c);
    }
}

/// Which part of Settings is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Section {
    #[default]
    General,
    Search,
    About,
}

/// The settings page: a menu on the left (like Windows Settings), the
/// chosen section on the right. Every change applies at once.
pub fn show(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let ctx = ui.ctx().clone();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        if icon_button(ui, p, IconKind::Back, false).on_hover_text(tr("Back (Esc)", "Înapoi (Esc)")).clicked() {
            app.page = Page::Search;
            app.focus_search = true;
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(tr("Settings", "Setări")).size(22.0).family(egui::FontFamily::Name("display".into())).color(p.text),
        );
    });
    ui.add_space(14.0);

    let full = ui.available_rect_before_wrap();
    let nav_w = 196.0;
    let nav = egui::Rect::from_min_size(full.min + Vec2::new(6.0, 0.0), Vec2::new(nav_w, full.height()));
    let body = egui::Rect::from_min_max(egui::pos2(nav.right() + 24.0, full.top()), full.max - Vec2::new(20.0, 0.0));

    // ---- the menu
    let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav).layout(egui::Layout::top_down(Align::Min)));
    for (section, icon, label) in [
        (Section::General, Icon::Settings, tr("General", "General")),
        (Section::Search, Icon::Search, tr("Search", "Căutare")),
        (Section::About, Icon::Sparkle, tr("About", "Despre")),
    ] {
        if nav_item(&mut nav_ui, p, icon, label, app.settings.section == section).clicked() {
            app.settings.section = section;
        }
        nav_ui.add_space(2.0);
    }

    // ---- the section
    let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(egui::Layout::top_down(Align::Min)));
    egui::ScrollArea::vertical().id_salt("settings-body").auto_shrink([false, false]).show(&mut body_ui, |ui| {
        ui.set_max_width(ui.available_width().min(720.0));
        match app.settings.section {
            Section::General => general(app, ui, p, &ctx),
            Section::Search => search(app, ui, p),
            Section::About => about(app, ui, p),
        }
        ui.add_space(16.0);
    });
    ui.allocate_rect(full, Sense::hover());
}

fn general(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context) {
    heading(ui, p, tr("General", "General"), tr("How whereti looks and starts.", "Cum arată și cum pornește whereti."));
    card(ui, p, |ui| {
        row(ui, p, tr("Language", "Limbă"), tr("The language of the whole app.", "Limba întregii aplicații."), |ui| {
            let current = if app.settings.lang == Lang::Romanian { 1 } else { 0 };
            if let Some(i) = segmented(ui, p, &["English", "Română"], current) {
                let l = if i == 1 { Lang::Romanian } else { Lang::English };
                if l != app.settings.lang {
                    app.settings.lang = l;
                    crate::i18n::set(l);
                    app.settings.save();
                    if let Some(t) = &app.tray {
                        t.relabel();
                    }
                    app.run_search(); // dates and counts in the new language
                }
            }
        });
        line(ui, p);
        row(ui, p, tr("Theme", "Temă"), tr("Follow Windows, or always light or dark.", "Ca Windows, sau mereu luminoasă ori întunecată."), |ui| {
            let options = [tr("Windows", "Windows"), tr("Light", "Luminoasă"), tr("Dark", "Întunecată")];
            let current = match app.settings.theme {
                ThemeChoice::System => 0,
                ThemeChoice::Light => 1,
                ThemeChoice::Dark => 2,
            };
            if let Some(i) = segmented(ui, p, &options, current) {
                app.settings.theme = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark][i];
                app.settings.apply_theme(ctx);
                app.settings.save();
            }
        });
    });
    ui.add_space(14.0);
    card(ui, p, |ui| {
        shortcut(app, ui, p);
        line(ui, p);
        row(
            ui,
            p,
            tr("Start with Windows", "Pornește cu Windows"),
            tr("whereti waits next to the clock, ready to search.", "whereti așteaptă lângă ceas, gata de căutare."),
            |ui| {
                let mut on = app.settings.autostart;
                if toggle(ui, p, &mut on).clicked() {
                    match shell::set_autostart(on) {
                        Ok(()) => app.settings.autostart = on,
                        Err(e) => app.flash(e),
                    }
                }
            },
        );
    });
}

fn search(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    heading(
        ui,
        p,
        tr("Search", "Căutare"),
        tr("What whereti knows about your files.", "Ce știe whereti despre fișierele tale."),
    );
    let status = app.status.clone();
    card(ui, p, |ui| {
        // files and folders
        let (dot, value) = match (&app.offline, &status) {
            (Some(_), _) => (p.error, tr("The whereti service is not running", "Serviciul whereti nu rulează").to_string()),
            (None, Some(s)) => {
                let drives = s.volumes.iter().map(|v| format!("{}:", v.letter)).collect::<Vec<_>>().join(" ");
                let color = if s.state == "ready" { p.ok } else { p.warn };
                let text = if s.state == "ready" {
                    trf!("{} on {drives}", "{} pe {drives}", shell::count(s.entries))
                } else {
                    state_ro(&s.state)
                };
                (color, text)
            }
            (None, None) => (p.text3, tr("connecting…", "se conectează…").to_string()),
        };
        row(ui, p, tr("Files and folders", "Fișiere și foldere"), tr("Found by name, instantly.", "Găsite după nume, instant."), |ui| {
            ui.label(egui::RichText::new(value).size(13.0).color(p.text));
            let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
        });
        line(ui, p);
        // documents
        let c = status.as_ref().map(|s| s.content.clone());
        let desc = tr(
            "PDF, Word, Excel, PowerPoint, e-mails and text files, found by what is written in them.",
            "PDF, Word, Excel, PowerPoint, e-mailuri și fișiere text, găsite după ce scrie în ele.",
        );
        row(ui, p, tr("Inside documents", "În documente"), desc, |ui| {
            if let Some(c) = &c {
                let (dot, text) = match c.state.as_str() {
                    "ready" => (p.ok, trf!("{} documents", "{} documente", shell::count(c.files))),
                    s if s.starts_with("error") => (p.error, tr("not available", "indisponibil").to_string()),
                    _ => (p.warn, tr("reading…", "se citesc…").to_string()),
                };
                ui.label(egui::RichText::new(text).size(13.0).color(p.text));
                let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, dot);
            }
        });
        if let Some(c) = c.filter(crate::app::content_active) {
            ui.add_space(2.0);
            let all = (c.done + c.queued).max(1);
            let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
            ui.painter().rect_filled(r, 3.0, p.line);
            let mut done = r;
            done.set_width((r.width() * c.done as f32 / all as f32).max(6.0));
            ui.painter().rect_filled(done, 3.0, p.accent);
            ui.add_space(6.0);
            let text = crate::app::content_progress(&c, app.content_eta(&c));
            ui.label(egui::RichText::new(text).size(12.0).color(p.text2));
            ui.add_space(8.0);
        }
    });
    ui.add_space(14.0);

    // ---- folders to skip
    card(ui, p, |ui| {
        ui.add_space(12.0);
        ui.label(egui::RichText::new(tr("Folders whereti skips", "Foldere ignorate")).size(14.5).family(semibold()).color(p.text));
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(tr(
                    "Nothing inside them shows up in results. Use it for private folders, or old archives you never look for.",
                    "Nimic din ele nu apare în rezultate. Util pentru foldere private sau arhive vechi pe care nu le cauți.",
                ))
                .size(12.5)
                .color(p.text2),
            )
            .wrap(),
        );
        ui.add_space(10.0);
        let defaults: Vec<String> = app.settings.config.as_ref().map(|c| c.default_exclusions.clone()).unwrap_or_default();
        let is_default = |x: &String| defaults.iter().any(|d| d.eq_ignore_ascii_case(x));
        let own: Vec<String> = app.settings.exclusions.iter().filter(|x| !is_default(x)).cloned().collect();
        let mut changed = false;
        if !own.is_empty() {
            if let Some(i) = path_list(ui, p, &own, &mut app.file_icons) {
                let gone = own[i].clone();
                app.settings.exclusions.retain(|x| x != &gone);
                changed = true;
            }
            ui.add_space(10.0);
        }
        if button(ui, p, tr("Add a folder…", "Adaugă un folder…"), false).clicked() {
            if let Some(path) = app.hwnd.and_then(crate::win::pick_folder) {
                if !app.settings.exclusions.iter().any(|x| x.eq_ignore_ascii_case(&path)) {
                    app.settings.exclusions.push(path);
                    changed = true;
                }
            }
        }
        if changed {
            // applied at once: the service rebuilds its index in a few seconds
            app.searcher.set_exclusions(app.settings.exclusions.clone());
        }
        ui.add_space(12.0);
        line(ui, p);
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
            crate::icons::paint(ui.painter(), r, crate::icons::Icon::Lock, p.text3);
            ui.label(
                egui::RichText::new(tr(
                    "Windows, program files and temporary or cache folders are skipped automatically.",
                    "Windows, fișierele programelor și folderele temporare sau de cache sunt ignorate automat.",
                ))
                .size(12.0)
                .color(p.text3),
            );
        });
        ui.add_space(12.0);
    });
}

fn about(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    heading(ui, p, tr("About", "Despre"), "");
    card(ui, p, |ui| {
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            let logo = app.settings.logo.get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], &crate::tray::logo_rgba_64());
                ui.ctx().load_texture("logo", image, egui::TextureOptions::LINEAR)
            });
            let (r, _) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover());
            let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
            ui.painter().image(logo.id(), r, uv, Color32::WHITE);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("whereti {}", env!("CARGO_PKG_VERSION")))
                        .size(18.0)
                        .family(egui::FontFamily::Name("display".into()))
                        .color(p.text),
                );
                ui.label(
                    egui::RichText::new(tr("Instant file search for Windows.", "Căutare instantă de fișiere pentru Windows."))
                        .size(13.0)
                        .color(p.text2),
                );
            });
        });
        ui.add_space(14.0);
        line(ui, p);
        for (title, text) in [
            (
                tr("Private", "Privat"),
                tr("Everything stays on your PC. No account, no tracking, no internet needed.", "Totul rămâne pe PC-ul tău. Fără cont, fără urmărire, fără internet."),
            ),
            (tr("Open source", "Open source"), tr("Free, under the MIT license.", "Gratuit, sub licența MIT.")),
            (
                tr("Supporter license", "Licență de susținător"),
                tr("Coming soon, for those who want to support the project.", "În curând, pentru cine vrea să susțină proiectul."),
            ),
        ] {
            info_row(ui, p, title, text);
        }
    });
    ui.add_space(10.0);
    ui.label(egui::RichText::new("© 2026 Software Hive S.R.L.").size(12.0).color(p.text3));
}

/// The shortcut row: the keys in use, "Change" to type new ones, and the
/// usual choices as suggestions.
fn shortcut(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let current = app.hotkey_label();
    let mut chosen: Option<String> = None;
    if app.settings.recording {
        // Keys pressed now are the new shortcut; Esc gives up.
        let mut cancel = false;
        ui.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if *key == egui::Key::Escape && modifiers.is_none() {
                        cancel = true;
                    } else if let Some(label) = crate::tray::label_for(*key, *modifiers) {
                        chosen = Some(label);
                    }
                    false
                }
                egui::Event::Key { .. } | egui::Event::Text(_) => false,
                _ => true,
            })
        });
        if cancel {
            stop_recording(app);
        }
    }
    let recording = app.settings.recording;
    let desc = if recording {
        tr("Press the keys together, with Ctrl or Alt. Esc to cancel.", "Apasă tastele împreună, cu Ctrl sau Alt. Esc ca să renunți.")
    } else {
        tr("Opens whereti from any app.", "Deschide whereti din orice aplicație.")
    };
    row(ui, p, tr("Shortcut", "Tastă rapidă"), desc, |ui| {
        if recording {
            if button(ui, p, tr("Cancel", "Renunță"), false).clicked() {
                stop_recording(app);
            }
            ui.add_space(8.0);
            let text = egui::RichText::new(tr("Press keys…", "Apasă tastele…")).size(13.0).family(semibold()).color(p.accent);
            Frame::new()
                .fill(p.accent_soft)
                .stroke(Stroke::new(1.0, p.accent))
                .corner_radius(8.0)
                .inner_margin(Margin::symmetric(12, 7))
                .show(ui, |ui| ui.label(text));
        } else {
            if button(ui, p, tr("Change", "Schimbă"), false).clicked() {
                app.settings.recording = true;
                if let Some(t) = &app.tray {
                    t.pause_hotkey(true);
                }
            }
            ui.add_space(10.0);
            match &current {
                Some(k) => crate::app::key_hint(ui, p, k),
                None => {
                    ui.label(egui::RichText::new(tr("none", "niciuna")).size(13.0).color(p.text3));
                }
            }
        }
    });
    // the usual choices, one click each
    let others: Vec<&str> = HOTKEYS.iter().copied().filter(|h| current.as_deref() != Some(*h)).collect();
    if !recording && !others.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            ui.label(egui::RichText::new(tr("Suggestions:", "Sugestii:")).size(12.5).color(p.text2));
            for h in others {
                let chip = egui::Button::new(egui::RichText::new(h).size(12.5).color(p.text))
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(12.0);
                if ui.add(chip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    chosen = Some(h.to_string());
                }
            }
        });
        ui.add_space(12.0);
    }
    if let Some(label) = chosen {
        if app.settings.recording {
            stop_recording(app);
        }
        if current.as_deref() == Some(label.as_str()) {
            return;
        }
        let result = match app.tray.as_mut() {
            Some(t) => t.set_hotkey(&label),
            None => Err(tr("the tray icon is not available", "iconița din tray nu e disponibilă").into()),
        };
        match result {
            Ok(()) => {
                app.settings.hotkey = Some(label.clone());
                app.settings.save();
                app.flash(trf!("The shortcut is now {label}", "Tasta rapidă este acum {label}"));
            }
            Err(e) => app.flash(e),
        }
    }
}

pub(crate) fn stop_recording(app: &mut WheretiApp) {
    app.settings.recording = false;
    if let Some(t) = &app.tray {
        t.pause_hotkey(false);
    }
}

// ---------------------------------------------------------------- pieces

fn nav_item(ui: &mut egui::Ui, p: &Palette, icon: Icon, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 8.0, p.accent_soft);
        let bar = egui::Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), Vec2::new(3.0, 16.0));
        painter.rect_filled(bar, 1.5, p.accent);
    } else if resp.hovered() {
        painter.rect_filled(rect, 8.0, p.hover);
    }
    let color = if selected { p.text } else { p.text2 };
    let ir = egui::Rect::from_center_size(egui::pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0));
    crate::icons::paint(painter, ir, icon, if selected { p.accent } else { p.text3 });
    let font = FontId::new(13.5, if selected { semibold() } else { crate::app::medium() });
    painter.text(egui::pos2(rect.left() + 42.0, rect.center().y), egui::Align2::LEFT_CENTER, label, font, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn heading(ui: &mut egui::Ui, p: &Palette, title: &str, sub: &str) {
    ui.label(egui::RichText::new(title).size(18.0).family(semibold()).color(p.text));
    if !sub.is_empty() {
        ui.label(egui::RichText::new(sub).size(12.5).color(p.text2));
    }
    ui.add_space(12.0);
}

/// A white card with a hairline border; rows inside, separated by `line`.
fn card(ui: &mut egui::Ui, p: &Palette, body: impl FnOnce(&mut egui::Ui)) {
    Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(18, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            body(ui);
        });
}

/// Title and description on the left, the control on the right.
fn row(ui: &mut egui::Ui, p: &Palette, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let text_w = (ui.available_width() - 280.0).max(200.0);
        ui.vertical(|ui| {
            ui.set_width(text_w);
            ui.label(egui::RichText::new(title).size(14.0).family(semibold()).color(p.text));
            ui.add_space(2.0);
            ui.add(egui::Label::new(egui::RichText::new(desc).size(12.5).color(p.text2)).wrap());
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            control(ui);
        });
    });
    ui.add_space(12.0);
}

/// A title and a text using the whole width (nothing to set).
fn info_row(ui: &mut egui::Ui, p: &Palette, title: &str, text: &str) {
    ui.add_space(12.0);
    ui.label(egui::RichText::new(title).size(14.0).family(semibold()).color(p.text));
    ui.add_space(2.0);
    ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(p.text2)).wrap());
    ui.add_space(12.0);
}

fn line(ui: &mut egui::Ui, p: &Palette) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.line));
    ui.add_space(1.0);
}

/// A segmented control (like the ones in Windows Settings); returns the
/// option clicked.
fn segmented(ui: &mut egui::Ui, p: &Palette, options: &[&str], selected: usize) -> Option<usize> {
    let font = FontId::new(13.0, crate::app::medium());
    let widths: Vec<f32> =
        options.iter().map(|o| ui.painter().layout_no_wrap(o.to_string(), font.clone(), p.text).size().x + 26.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 34.0), Sense::hover());
    ui.painter().rect(rect, 9.0, p.bg, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    let mut x = rect.left() + 3.0;
    let mut clicked = None;
    for (i, (label, w)) in options.iter().zip(&widths).enumerate() {
        let r = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 3.0), Vec2::new(*w, rect.height() - 6.0));
        let resp = ui.interact(r, ui.scope_id().with(("seg", label)), Sense::click());
        if i == selected {
            ui.painter().rect(r, 7.0, p.surface, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, 7.0, p.hover);
        }
        let color = if i == selected { p.text } else { p.text2 };
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, label, font.clone(), color);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            clicked = Some(i);
        }
        x += w;
    }
    clicked
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
                    None => crate::icons::paint(ui.painter(), icon_rect, Icon::Folder, p.text3),
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
                    crate::icons::paint(ui.painter(), x.shrink(7.0), Icon::Close, p.text2);
                }
                if xr.on_hover_text(tr("Stop skipping it", "Nu-l mai ignora")).clicked() {
                    removed = Some(i);
                }
            }
        });
    removed
}

/// A pill button; `primary` = solid accent.
fn button(ui: &mut egui::Ui, p: &Palette, text: &str, primary: bool) -> egui::Response {
    let color = if primary { p.on_accent } else { p.text };
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.5, semibold()), color);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + 30.0, 34.0), Sense::click());
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
