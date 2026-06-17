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
use crate::{trf, trn};
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
    /// Ask whereti.com once a day whether a new version exists.
    pub updates: bool,
    /// Rank the files the user opens often or lately higher (usage.rs).
    pub learn: bool,
    /// Keep the searches made, for the Up arrow and the empty page (history.rs).
    pub remember_searches: bool,
    /// The welcome page asked about recognising objects and animals in
    /// photos (asked once; the switch stays in Settings > Search).
    pub photo_tags_asked: bool,
    /// The chosen language; `None` = the Windows display language.
    pub lang: Option<Lang>,
    config: Option<ConfigReply>,
    exclusions: Vec<String>,
    /// The part of Settings shown.
    pub section: Section,
    /// Waiting for the keys of a new shortcut.
    pub recording: bool,
    /// The supporter license, if one is activated (license.rs).
    pub license: Option<crate::license::License>,
    license_input: String,
    license_error: Option<String>,
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
            lang: None,
            updates: true,
            learn: true,
            remember_searches: true,
            photo_tags_asked: false,
            config: None,
            exclusions: Vec::new(),
            section: Section::General,
            recording: false,
            license: crate::license::load(),
            license_input: String::new(),
            license_error: None,
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
                    Some(("language", v)) => s.lang = Lang::from_code(v),
                    Some(("welcomed", "yes")) => s.welcomed = true,
                    Some(("updates", "off")) => s.updates = false,
                    Some(("learn", "off")) => s.learn = false,
                    Some(("searches", "off")) => s.remember_searches = false,
                    Some(("photo_tags_asked", "yes")) => s.photo_tags_asked = true,
                    _ => {}
                }
            }
        }
        crate::usage::set_enabled(s.learn);
        crate::history::set_enabled(s.remember_searches);
        s
    }

    /// The language shown: the chosen one, or Windows'.
    pub fn effective_lang(&self) -> Lang {
        self.lang.unwrap_or_else(Lang::system)
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
            self.lang.map_or("system", |l| l.locale().code),
            if self.welcomed { "yes" } else { "no" }
        );
        if let Some(h) = &self.hotkey {
            text.push_str(&format!("hotkey={h}\n"));
        }
        if !self.updates {
            text.push_str("updates=off\n");
        }
        if !self.learn {
            text.push_str("learn=off\n");
        }
        if !self.remember_searches {
            text.push_str("searches=off\n");
        }
        if self.photo_tags_asked {
            text.push_str("photo_tags_asked=yes\n");
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
    Support,
}

/// The settings page: a menu on the left (like Windows Settings), the
/// chosen section on the right. Every change applies at once.
pub fn show(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let ctx = ui.ctx().clone();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        if icon_button(ui, p, IconKind::Back, false).on_hover_text(tr("Back (Esc)")).clicked() {
            app.page = Page::Search;
            app.focus_search = true;
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(tr("Settings")).size(22.0).family(egui::FontFamily::Name("display".into())).color(p.text),
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
        (Section::General, Icon::Settings, tr("General")),
        (Section::Search, Icon::Search, tr("Search")),
        (Section::About, Icon::Sparkle, tr("About")),
        (Section::Support, Icon::Heart, tr("Support")),
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
            Section::Support => support(app, ui, p),
        }
        ui.add_space(16.0);
    });
    ui.allocate_rect(full, Sense::hover());
}

fn general(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context) {
    heading(ui, p, tr("General"), tr("How whereti looks and starts."));
    card(ui, p, |ui| {
        row(ui, p, tr("Language"), tr("The language of the whole app."), |ui| {
            // "Same as Windows" first, then every language by its own name;
            // "beta" until a native speaker has read the texts
            let name = |l: Option<Lang>| {
                let (text, beta) = match l {
                    None => (crate::trf!("Same as Windows ({language})", language = Lang::system().locale().native), false),
                    Some(l) => (l.locale().native.to_string(), !l.locale().reviewed),
                };
                let mut job = egui::text::LayoutJob::default();
                job.append(&text, 0.0, egui::TextFormat::simple(FontId::new(13.0, crate::app::medium()), p.text));
                if beta {
                    job.append("beta", 8.0, egui::TextFormat::simple(FontId::new(11.0, crate::app::medium()), p.text3));
                }
                job
            };
            let mut chosen = None;
            egui::ComboBox::from_id_salt("language")
                .selected_text(name(app.settings.lang))
                .width(230.0)
                .height(420.0)
                .show_ui(ui, |ui| {
                    for l in std::iter::once(None).chain(Lang::ALL.into_iter().map(Some)) {
                        if ui.selectable_label(app.settings.lang == l, name(l)).clicked() {
                            chosen = Some(l);
                        }
                    }
                });
            if let Some(l) = chosen {
                if l != app.settings.lang {
                    app.settings.lang = l;
                    crate::i18n::set(app.settings.effective_lang());
                    app.settings.save();
                    if let Some(t) = &app.tray {
                        t.relabel();
                    }
                    app.run_search(); // dates and counts in the new language
                }
            }
        });
        line(ui, p);
        row(ui, p, tr("Theme"), tr("Follow Windows, or always light or dark."), |ui| {
            let options = [tr("Windows"), tr("Light"), tr("Dark")];
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
            tr("Start with Windows"),
            tr("whereti waits next to the clock, ready to search."),
            |ui| {
                let mut on = app.settings.autostart;
                if toggle(ui, p, &mut on, tr("Start with Windows")).clicked() {
                    match shell::set_autostart(on) {
                        Ok(()) => app.settings.autostart = on,
                        Err(e) => app.flash(e),
                    }
                }
            },
        );
        line(ui, p);
        row(
            ui,
            p,
            tr("Check for updates"),
            tr("Once a day, whereti asks whereti.com if there is a new version. Only its version number is sent."),
            |ui| {
                let mut on = app.settings.updates;
                if toggle(ui, p, &mut on, tr("Check for updates")).clicked() {
                    app.settings.updates = on;
                    app.settings.save();
                }
            },
        );
    });
}

fn search(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    heading(
        ui,
        p,
        tr("Search"),
        tr("What whereti knows about your files."),
    );
    let status = app.status.clone();
    card(ui, p, |ui| {
        // files and folders
        let (dot, value) = match (&app.offline, &status) {
            (Some(_), _) => (p.error, tr("The whereti service is not running").to_string()),
            (None, Some(s)) => {
                let drives = s.volumes.iter().map(|v| format!("{}:", v.letter)).collect::<Vec<_>>().join(" ");
                let color = if s.state == "ready" { p.ok } else { p.warn };
                let text = if s.state == "ready" {
                    trn!("{n} file on {drives}|{n} files on {drives}", s.entries, drives)
                } else {
                    state_ro(&s.state)
                };
                (color, text)
            }
            (None, None) => (p.text3, tr("connecting…").to_string()),
        };
        row(ui, p, tr("Files and folders"), tr("Found by name, instantly."), |ui| {
            ui.label(egui::RichText::new(value).size(13.0).color(p.text));
            let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
        });
        line(ui, p);
        // documents
        let c = status.as_ref().map(|s| s.content.clone());
        let desc = tr("PDF, Word, Excel, PowerPoint, e-mails, text files, and the text in scans and photos.");
        row(ui, p, tr("Inside documents"), desc, |ui| {
            if let Some(c) = &c {
                let (dot, text) = match c.state.as_str() {
                    "ready" => (p.ok, trn!("{n} document|{n} documents", c.files)),
                    s if s.starts_with("error") => (p.error, tr("not available").to_string()),
                    _ => (p.warn, tr("reading…").to_string()),
                };
                ui.label(egui::RichText::new(text).size(13.0).color(p.text));
                let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, dot);
            }
        });
        // The progress, or the same space left empty: the switches below
        // must not move while documents are read, or a click meant for one
        // lands beside it.
        let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::hover());
        if let Some(c) = c.filter(crate::app::content_active) {
            let all = (c.done + c.queued).max(1);
            let r = egui::Rect::from_min_size(area.min + Vec2::new(0.0, 2.0), Vec2::new(area.width(), 6.0));
            ui.painter().rect_filled(r, 3.0, p.line);
            let mut done = r;
            done.set_width((r.width() * c.done as f32 / all as f32).max(6.0));
            ui.painter().rect_filled(done, 3.0, p.accent);
            let text = crate::app::content_progress(&c, app.content_eta(&c));
            ui.painter().text(area.min + Vec2::new(0.0, 15.0), egui::Align2::LEFT_TOP, text, FontId::proportional(12.0), p.text2);
        }
    });
    ui.add_space(14.0);

    // ---- learning from what the user opens
    card(ui, p, |ui| {
        row(
            ui,
            p,
            tr("Learn from what you open"),
            tr("Files you open often or lately, in whereti or in other apps, come first. This stays on your PC."),
            |ui| {
                let mut on = app.settings.learn;
                if toggle(ui, p, &mut on, tr("Learn from what you open")).clicked() {
                    app.settings.learn = on;
                    crate::usage::set_enabled(on);
                    app.settings.save();
                }
                ui.add_space(8.0);
                if button(ui, p, tr("Forget"), false).clicked() {
                    crate::usage::forget();
                    app.flash(tr("whereti forgot what you opened.").to_string());
                }
            },
        );
    });
    ui.add_space(14.0);

    // ---- the searches made
    card(ui, p, |ui| {
        row(
            ui,
            p,
            tr("Remember your searches"),
            tr("The Up arrow in an empty search box brings back your last searches; they also show on the empty page. This stays on your PC, and turning it off forgets them."),
            |ui| {
                let mut on = app.settings.remember_searches;
                if toggle(ui, p, &mut on, tr("Remember your searches")).clicked() {
                    app.settings.remember_searches = on;
                    crate::history::set_enabled(on);
                    if !on {
                        crate::history::forget();
                    }
                    app.settings.save();
                }
                ui.add_space(8.0);
                if button(ui, p, tr("Forget"), false).clicked() {
                    crate::history::forget();
                    app.flash(tr("whereti forgot your searches.").to_string());
                }
            },
        );
    });
    ui.add_space(14.0);

    // ---- where the user works
    let work = app.status.as_ref().and_then(|s| s.work_folders.clone());
    card(ui, p, |ui| {
        row(
            ui,
            p,
            tr("Learn where you work"),
            tr("Files in the folders where you create and change files lately come first. whereti sees this in the disk's own change log; it stays on your PC, and turning it off forgets it."),
            |ui| {
                let mut on = work.is_some() || app.status.is_none();
                if toggle(ui, p, &mut on, tr("Learn where you work")).clicked() {
                    app.searcher.set_work_folders(on);
                }
            },
        );
        // what it learnt, so it is never a secret
        if let Some(folders) = work.filter(|f| !f.is_empty()) {
            let text = crate::trf!("Lately: {folders}", folders = folders.join(" · "));
            ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(p.text2)).wrap());
            ui.add_space(12.0);
        }
    });
    ui.add_space(14.0);

    // ---- objects and animals in photos
    let tags = photo_tags_state(app);
    card(ui, p, |ui| {
        row(ui, p, tr("Recognize objects and animals in photos"), photo_tags_text(), |ui| {
            let mut on = tags != "off";
            if toggle(ui, p, &mut on, tr("Recognize objects and animals in photos")).clicked() {
                app.searcher.set_photo_tags(on);
                if !app.settings.photo_tags_asked {
                    app.settings.photo_tags_asked = true;
                    app.settings.save();
                }
            }
        });
        if let Some(line) = photo_tags_line(&tags) {
            ui.label(egui::RichText::new(line).size(12.5).color(if tags.starts_with("error") { p.error } else { p.text2 }));
            ui.add_space(12.0);
        }
    });
    ui.add_space(14.0);

    // ---- folders to skip
    card(ui, p, |ui| {
        ui.add_space(12.0);
        ui.label(egui::RichText::new(tr("Folders whereti skips")).size(14.5).family(semibold()).color(p.text));
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(tr("Nothing inside them shows up in results. Use it for private folders, or old archives you never look for."))
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
        if button(ui, p, tr("Add a folder…"), false).clicked() {
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
                egui::RichText::new(tr("Windows, program files and temporary or cache folders are skipped automatically."))
                .size(12.0)
                .color(p.text3),
            );
        });
        ui.add_space(12.0);
    });
}

fn about(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    heading(ui, p, tr("About"), "");
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
                    egui::RichText::new(tr("Instant file search for Windows."))
                        .size(13.0)
                        .color(p.text2),
                );
                ui.add_space(6.0);
                // who builds it, in plain sight
                ui.label(
                    egui::RichText::new(format!("{} · © 2026", trf!("Made by {company}", company = "Software Hive S.R.L.")))
                        .size(12.5)
                        .family(crate::app::medium())
                        .color(p.text),
                );
            });
        });
        ui.add_space(14.0);
        line(ui, p);
        let supporter_text = match &app.settings.license {
            Some(l) => trf!("Thank you for your support, {name}.", name = l.name),
            None => tr("Optional: one payment that pays for whereti's development. Nothing is locked without it.").to_string(),
        };
        update_row(app, ui, p);
        line(ui, p);
        for (title, text) in [
            (
                tr("Private"),
                tr("Your files never leave your PC. No account, no tracking."),
            ),
            (tr("Open source"), tr("Free, under the MIT license.")),
            // GeoNames asks for this credit (CC BY 4.0)
            (tr("Places"), tr("Where your photos were taken is named with data from GeoNames (geonames.org, CC BY 4.0).")),
            (
                tr("Supporter license"),
                supporter_text.as_str(),
            ),
        ] {
            info_row(ui, p, title, text);
        }
        line(ui, p);
        ui.add_space(10.0);
        // the website, for help and news
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (label, url) in [
                ("whereti.com", crate::links::SITE),
                (tr("Questions and help"), crate::links::HELP),
                (tr("Privacy"), crate::links::PRIVACY),
                (tr("Source code"), crate::links::SOURCE),
                (tr("Support whereti"), crate::links::SUPPORTER),
            ] {
                let link = ui
                    .add(egui::Label::new(egui::RichText::new(label).size(13.0).color(p.accent)).sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(url);
                if link.clicked() {
                    if let Err(e) = shell::open(url) {
                        app.flash(e);
                    }
                }
            }
        });
        ui.add_space(6.0);
    });
}

/// Support: the two licenses to buy, and the place to paste a key; or,
/// once activated, a thank you.
fn support(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    use crate::license::Plan;
    heading(
        ui,
        p,
        tr("Support whereti"),
        tr("whereti is free, all of it. A supporter license pays for its development, and whereti thanks you by name."),
    );
    if let Some(l) = app.settings.license.clone() {
        card(ui, p, |ui| {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let (badge, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                let gold = crate::app::supporter_gold(p);
                ui.painter().circle_filled(badge.center(), 22.0, crate::theme::with_alpha(gold, 0.16));
                crate::icons::paint(ui.painter(), badge.shrink(11.0), Icon::Heart, gold);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(trf!("Thank you, {name}!", name = l.name)).size(18.0).family(semibold()).color(p.text));
                    let what = match l.plan {
                        Plan::Personal => tr("Personal license, on all your PCs, for life.").to_string(),
                        Plan::Business => crate::trn!("Business license for {n} PC, for life.|Business license for {n} PCs, for life.", l.seats),
                    };
                    ui.label(egui::RichText::new(what).size(13.0).color(p.text2));
                });
            });
            ui.add_space(14.0);
            line(ui, p);
            row(ui, p, tr("Your key"), tr("Keep it: it works again after reinstalling Windows."), |ui| {
                if button(ui, p, tr("Remove"), false).clicked() {
                    crate::license::remove();
                    app.settings.license = None;
                    let ctx = ui.ctx().clone();
                    app.supporter_changed(&ctx, false);
                }
                if button(ui, p, tr("Copy"), false).clicked() {
                    ui.ctx().copy_text(l.key.clone());
                    app.flash(tr("Key copied"));
                }
            });
        });
        return;
    }

    // the two licenses, side by side
    let gap = 14.0;
    let w = ((ui.available_width() - gap) / 2.0).floor().max(200.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        let plans = [
            (
                tr("Personal"),
                "$19",
                tr("one payment"),
                [tr("Yours for life, with every update"), tr("On all your PCs"), tr("Your name in whereti")],
                tr("Buy for $19"),
                crate::links::BUY_PERSONAL,
                true,
            ),
            (
                tr("Business"),
                "$39",
                tr("per PC, one payment"),
                [tr("For life, with every update"), tr("An invoice with your company's details"), tr("As many PCs as you need")],
                tr("Buy for business"),
                crate::links::BUY_BUSINESS,
                false,
            ),
        ];
        for (name, price, per, points, cta, url, primary) in plans {
            ui.allocate_ui_with_layout(Vec2::new(w, 0.0), egui::Layout::top_down(Align::Min), |ui| {
                ui.set_width(w);
                card(ui, p, |ui| {
                    ui.add_space(16.0);
                    ui.label(egui::RichText::new(name).size(14.0).family(semibold()).color(p.accent));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(price).size(30.0).family(egui::FontFamily::Name("display".into())).color(p.text));
                        ui.label(egui::RichText::new(per).size(12.5).color(p.text3));
                    });
                    ui.add_space(8.0);
                    for pt in points {
                        ui.horizontal(|ui| {
                            check_mark(ui, p.ok);
                            ui.add(egui::Label::new(egui::RichText::new(pt).size(13.0).color(p.text2)).wrap());
                        });
                    }
                    ui.add_space(12.0);
                    if button(ui, p, cta, primary).clicked() {
                        if let Err(e) = shell::open(url) {
                            app.flash(e);
                        }
                    }
                    ui.add_space(16.0);
                });
            });
        }
    });
    ui.add_space(8.0);
    ui.add(
        egui::Label::new(
            egui::RichText::new(tr(
                "Payment is handled by Lemon Squeezy, which also sends the invoice. Your key arrives by e-mail a minute later.",
            ))
            .size(12.0)
            .color(p.text3),
        )
        .wrap(),
    );
    ui.add_space(14.0);

    // a key received by e-mail
    card(ui, p, |ui| {
        ui.add_space(14.0);
        ui.label(egui::RichText::new(tr("Already have a key?")).size(14.0).family(semibold()).color(p.text));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(tr("Paste it from the e-mail. No internet is needed to activate it.")).size(12.5).color(p.text2));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            // the field takes everything up to the button, at the button's
            // height, so both end flush with the card
            let gap = ui.spacing().item_spacing.x;
            let width = (ui.available_width() - button_width(ui, tr("Activate")) - gap).max(160.0);
            let field = egui::TextEdit::singleline(&mut app.settings.license_input)
                .hint_text("WHERETI-…")
                .font(FontId::monospace(12.5))
                .align(egui::Align2::LEFT_CENTER)
                .min_size(Vec2::new(width, BUTTON_HEIGHT))
                .desired_width(width);
            ui.add(field);
            if button(ui, p, tr("Activate"), true).clicked() {
                match crate::license::parse(&app.settings.license_input) {
                    Ok(l) => match crate::license::save(&l) {
                        Ok(()) => {
                            app.flash(trf!("Thank you, {name}!", name = l.name));
                            app.settings.license = Some(l);
                            app.settings.license_input.clear();
                            app.settings.license_error = None;
                            let ctx = ui.ctx().clone();
                            app.supporter_changed(&ctx, true);
                        }
                        Err(e) => app.settings.license_error = Some(e.to_string()),
                    },
                    Err(crate::license::LicenseError::Malformed) => {
                        app.settings.license_error =
                            Some(tr("This does not look like a whereti key. Copy all of it, from WHERETI- to the end.").into())
                    }
                    Err(crate::license::LicenseError::BadSignature) => {
                        app.settings.license_error = Some(tr("This key is not valid. Write to contact@whereti.com and we will sort it out.").into())
                    }
                }
            }
        });
        if let Some(e) = &app.settings.license_error {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(e).size(12.5).color(p.error));
        }
        ui.add_space(14.0);
    });
}

/// A small green tick, drawn (fonts do not all have one).
fn check_mark(ui: &mut egui::Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), Sense::hover());
    let pts = [
        egui::pos2(r.left() + 3.0, r.center().y + 0.5),
        egui::pos2(r.left() + 6.5, r.center().y + 4.0),
        egui::pos2(r.right() - 2.5, r.center().y - 4.0),
    ];
    ui.painter().add(egui::Shape::line(pts.to_vec(), Stroke::new(1.8, color)));
}

/// The update state in About: up to date, a new version to install, the
/// download, or what went wrong.
fn update_row(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    use crate::update::Phase;
    let ctx = ui.ctx().clone();
    let phase = app.updater.phase();
    let (title, desc): (String, String) = match &phase {
        Phase::Offered(o) => (
            trf!("whereti {version} is available", version = o.version),
            o.notes.lines().next().filter(|l| !l.trim().is_empty()).map(str::to_string)
                .unwrap_or_else(|| tr("A new version is ready to install.").to_string()),
        ),
        Phase::Downloading { done, total, .. } => (
            tr("Downloading the update…").to_string(),
            if *total > 0 { format!("{} %", done * 100 / total) } else { shell::size(*done) },
        ),
        Phase::Installing => (
            tr("Installing the update").to_string(),
            tr("Windows asks for permission, then whereti opens again.").to_string(),
        ),
        Phase::Failed(e) => (tr("The update did not install").to_string(), e.clone()),
        Phase::Idle if app.settings.updates => (
            tr("whereti is up to date").to_string(),
            match app.updater.last_check() {
                Some(t) => {
                    let minutes = ((t + 11_644_473_600) / 60) as u32;
                    let when = crate::app::relative_time(minutes, shell::tz_offset(), crate::app::now_minutes());
                    trf!("Last checked: {when}", when)
                }
                None => tr("Not checked yet. whereti checks once a day.").to_string(),
            },
        ),
        Phase::Idle => (
            tr("Checking for updates is off").to_string(),
            tr("Turn it on in General, or get new versions from whereti.com.").to_string(),
        ),
    };
    row(ui, p, &title, &desc, |ui| match phase {
        Phase::Offered(_) => {
            if button(ui, p, tr("Install update"), true).clicked() {
                app.updater.install(&ctx);
            }
        }
        Phase::Failed(_) => {
            if button(ui, p, tr("Try again"), false).clicked() {
                app.updater.retry();
            }
        }
        Phase::Downloading { .. } | Phase::Installing => {
            ui.add(egui::Spinner::new().size(18.0).color(p.accent));
        }
        Phase::Idle => {}
    });
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
        tr("Press the keys together, with Ctrl or Alt. Esc to cancel.")
    } else {
        tr("Opens whereti from any app.")
    };
    row(ui, p, tr("Shortcut"), desc, |ui| {
        if recording {
            if button(ui, p, tr("Cancel"), false).clicked() {
                stop_recording(app);
            }
            ui.add_space(8.0);
            let text = egui::RichText::new(tr("Press keys…")).size(13.0).family(semibold()).color(p.accent);
            Frame::new()
                .fill(p.accent_soft)
                .stroke(Stroke::new(1.0, p.accent))
                .corner_radius(8.0)
                .inner_margin(Margin::symmetric(12, 7))
                .show(ui, |ui| ui.label(text));
        } else {
            if button(ui, p, tr("Change"), false).clicked() {
                app.settings.recording = true;
                if let Some(t) = &app.tray {
                    t.pause_hotkey(true);
                }
            }
            ui.add_space(10.0);
            match &current {
                Some(k) => crate::app::key_hint(ui, p, k),
                None => {
                    ui.label(egui::RichText::new(tr("none")).size(13.0).color(p.text3));
                }
            }
        }
    });
    // the usual choices, one click each
    let others: Vec<&str> = HOTKEYS.iter().copied().filter(|h| current.as_deref() != Some(*h)).collect();
    if !recording && !others.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            ui.label(egui::RichText::new(tr("Suggestions:")).size(12.5).color(p.text2));
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
            None => Err(tr("the tray icon is not available").into()),
        };
        match result {
            Ok(()) => {
                app.settings.hotkey = Some(label.clone());
                app.settings.save();
                app.flash(trf!("The shortcut is now {label}", label));
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
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, label));
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

/// Recognition of objects and animals in photos, as the service reports it:
/// "off", "on", "downloading:<bytes>/<total>" or "error: …".
pub(crate) fn photo_tags_state(app: &WheretiApp) -> String {
    app.status.as_ref().map(|s| s.content.tags.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "off".into())
}

/// What recognising photos does and costs, said the same way everywhere.
pub(crate) fn photo_tags_text() -> &'static str {
    tr("whereti can also find photos by what they show: a dog, the beach, a car. It downloads a model once (84 MB, from Hugging Face); your photos are read on your PC and never leave it.")
}

/// A line under the switch: the download, a failure, or that it works.
pub(crate) fn photo_tags_line(state: &str) -> Option<String> {
    if let Some(rest) = state.strip_prefix("downloading:") {
        let (done, total) = rest.split_once('/')?;
        let (done, total) = (done.parse::<u64>().ok()?, total.parse::<u64>().ok()?.max(1));
        let percent = done * 100 / total;
        return Some(crate::trf!("Downloading the model: {percent}%", percent));
    }
    if let Some(e) = state.strip_prefix("error:") {
        return Some(crate::trf!("Could not download the model: {e}", e = e.trim()));
    }
    (state == "on").then(|| tr("On. Photos are read in the background, like documents.").to_string())
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
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, i == selected, *label));
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
                xr.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{} {path}", tr("Remove"))));
                if hovered || xr.hovered() {
                    if xr.hovered() {
                        ui.painter().rect_filled(x, 6.0, p.border);
                    }
                    crate::icons::paint(ui.painter(), x.shrink(7.0), Icon::Close, p.text2);
                }
                if xr.on_hover_text(tr("Stop skipping it")).clicked() {
                    removed = Some(i);
                }
            }
        });
    removed
}

/// A pill button; `primary` = solid accent.
const BUTTON_HEIGHT: f32 = 34.0;
const BUTTON_PADDING: f32 = 30.0;

/// How wide `button` draws `text` (to lay out what is beside it).
pub(crate) fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.5, semibold()), Color32::WHITE).size().x + BUTTON_PADDING
}

pub(crate) fn button(ui: &mut egui::Ui, p: &Palette, text: &str, primary: bool) -> egui::Response {
    let color = if primary { p.on_accent } else { p.text };
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.5, semibold()), color);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + BUTTON_PADDING, BUTTON_HEIGHT), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, text));
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
fn toggle(ui: &mut egui::Ui, p: &Palette, on: &mut bool, name: &str) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let state = *on;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, true, state, name));
    let t = ui.ctx().animate_bool(resp.id, *on);
    let track = if *on { p.accent } else { p.border };
    ui.painter().rect_filled(rect, 12.0, track);
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
