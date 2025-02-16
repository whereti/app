//! The search window (PLAN.md §10.4–10.8): type, see results, open.
//!
//! Almost everything is painted by hand from the palette in `theme.rs`, so
//! the window looks the same on every PC and follows the Windows accent colour.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Key, Margin,
    Modifiers, Pos2, Rect, Sense, Stroke, TextFormat, Theme, Vec2, ViewportCommand,
};
use whereti_ipc::{Row, Sort, StatusReply};

use crate::searcher::{Job, Searcher, Update};
use crate::settings::SettingsState;
use crate::shell;
use crate::theme::{self, Palette};
use crate::tray::{Requests, Tray, PREFERRED_HOTKEY};

const ROW_HEIGHT: f32 = 56.0;
const RESULT_LIMIT: u32 = 500;
const STATUS_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Documents,
    Images,
    Audio,
    Video,
    Code,
    Folders,
}

impl Filter {
    const ALL: [Filter; 7] =
        [Filter::All, Filter::Documents, Filter::Images, Filter::Audio, Filter::Video, Filter::Code, Filter::Folders];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "Toate",
            Filter::Documents => "Documente",
            Filter::Images => "Imagini",
            Filter::Audio => "Audio",
            Filter::Video => "Video",
            Filter::Code => "Cod",
            Filter::Folders => "Foldere",
        }
    }

    /// The same query syntax a power user would type (PLAN.md §10.5).
    fn clause(self) -> &'static str {
        match self {
            Filter::All => "",
            Filter::Documents => "ext:pdf;doc;docx;xls;xlsx;ppt;pptx;odt;ods;odp;rtf;txt;md;csv;epub;msg;eml",
            Filter::Images => "ext:jpg;jpeg;png;gif;bmp;webp;heic;tif;tiff;svg;ico;raw;cr2;nef;arw",
            Filter::Audio => "ext:mp3;wav;flac;aac;ogg;m4a;wma;opus",
            Filter::Video => "ext:mp4;mkv;avi;mov;wmv;webm;m4v;mpg;mpeg",
            Filter::Code => {
                "ext:rs;py;js;ts;tsx;jsx;go;java;c;cpp;h;hpp;cs;php;rb;swift;kt;sql;sh;ps1;html;css;json;yaml;yml;toml;xml"
            }
            Filter::Folders => "folder:",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Search,
    Settings,
}

pub struct WheretiApp {
    pub(crate) query: String,
    filter: Filter,
    pub(crate) searcher: Searcher,
    generation: u64,
    rows: Vec<Row>,
    total: u64,
    round_trip: Duration,
    warnings: Vec<String>,
    recent_mode: bool,
    selected: usize,
    scroll_to_selected: bool,
    scroll_offset: f32,
    view_height: f32,
    pub(crate) status: Option<StatusReply>,
    pub(crate) offline: Option<String>,
    last_status_ask: Instant,
    pub(crate) message: Option<(String, Instant)>,
    pub(crate) focus_search: bool,
    search_id: Option<egui::Id>,
    visible: bool,
    quitting: bool,
    tz: i32,
    pub(crate) accent: Color32,
    styled_theme: Option<Theme>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
    pub(crate) page: Page,
    pub(crate) settings: SettingsState,
    pub(crate) requests: Arc<Requests>,
    pub(crate) tray: Option<Tray>,
}

impl WheretiApp {
    pub fn new(ctx: &egui::Context, requests: Arc<Requests>, tray: Option<Tray>, visible: bool) -> Self {
        let wake_ctx = ctx.clone();
        let searcher = Searcher::start(move || wake_ctx.request_repaint());
        let accent = theme::system_accent();
        theme::apply(ctx, accent);
        let mut app = WheretiApp {
            query: String::new(),
            filter: Filter::All,
            searcher,
            generation: 0,
            rows: Vec::new(),
            total: 0,
            round_trip: Duration::ZERO,
            warnings: Vec::new(),
            recent_mode: true,
            selected: 0,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            view_height: 0.0,
            status: None,
            offline: None,
            last_status_ask: Instant::now() - STATUS_EVERY,
            message: None,
            focus_search: visible,
            search_id: None,
            visible,
            quitting: false,
            tz: shell::tz_offset(),
            accent,
            styled_theme: None,
            hwnd: None,
            page: Page::Search,
            settings: SettingsState::load(),
            requests,
            tray,
        };
        app.settings.apply_theme(ctx);
        match app.tray.as_ref().map(|t| t.hotkey) {
            // Said once: the shortcut actually used is then remembered.
            Some(Some(k)) if k != PREFERRED_HOTKEY && app.settings.hotkey.is_none() => {
                app.flash(format!("{PREFERRED_HOTKEY} e folosită de altă aplicație — tasta rapidă este {k}"));
                app.settings.hotkey = Some(k.to_string());
                app.settings.save();
            }
            Some(None) => app.flash("Nicio tastă rapidă disponibilă; deschide whereti din iconița din tray"),
            _ => {}
        }
        app.run_search();
        app
    }

    pub(crate) fn hotkey_label(&self) -> Option<&'static str> {
        self.tray.as_ref().and_then(|t| t.hotkey)
    }

    /// What is sent: the typed text plus the filter, or "recent files" when
    /// there is nothing to search for.
    fn effective_query(&self) -> (String, Sort, bool) {
        let q = self.query.trim();
        if q.is_empty() && self.filter == Filter::All {
            let home = std::env::var("USERPROFILE").unwrap_or_default();
            let recent = format!(r#"file: path:"{home}\Desktop" | path:"{home}\Documents" | path:"{home}\Downloads""#);
            return (recent, Sort::Modified, true);
        }
        (format!("{q} {}", self.filter.clause()), Sort::Relevance, false)
    }

    pub(crate) fn run_search(&mut self) {
        self.generation += 1;
        let (query, sort, recent) = self.effective_query();
        self.recent_mode = recent;
        self.searcher.search(Job { generation: self.generation, query, sort, limit: RESULT_LIMIT });
    }

    pub(crate) fn flash(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn show(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.visible = true;
        self.focus_search = true;
        self.page = Page::Search;
        self.run_search(); // fresh results every time the window appears
    }

    fn hide(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.visible = false;
        // A focused text field blinks its cursor, which wakes the app twice a
        // second even while hidden. No focus while hidden: no wakeups.
        if let Some(id) = self.search_id {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
    }

    fn apply_updates(&mut self) {
        for u in self.searcher.updates().collect::<Vec<_>>() {
            match u {
                Update::Results { generation, reply, round_trip } => {
                    if generation != self.generation {
                        continue; // an older search; a newer one is on its way
                    }
                    self.offline = None;
                    self.rows = reply.rows;
                    self.total = reply.total;
                    self.round_trip = round_trip;
                    self.warnings = reply.warnings;
                    self.selected = 0;
                    self.scroll_offset = 0.0;
                    self.scroll_to_selected = true;
                }
                Update::Status(s) => {
                    self.offline = None;
                    if let Some(t) = &self.tray {
                        let tip = format!("whereti — {} fișiere · {}", shell::count(s.entries), state_ro(&s.state));
                        let _ = t.icon.set_tooltip(Some(tip));
                    }
                    self.status = Some(s);
                }
                Update::Config(c) => self.settings.on_config(c),
                Update::Done(msg) => self.flash(msg),
                Update::Offline(msg) => self.offline = Some(msg),
            }
        }
    }

    fn open_selected(&mut self, ctx: &egui::Context, reveal: bool) {
        let Some(row) = self.rows.get(self.selected) else { return };
        let path = row.path.clone();
        let result = if reveal { shell::reveal(&path) } else { shell::open(&path) };
        match result {
            Ok(()) => self.hide(ctx),
            Err(e) => self.flash(e),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let page = ((self.view_height / ROW_HEIGHT) as usize).max(1);
        let (down, up, pg_down, pg_up, ctrl_enter, enter, copy, esc, settings, menu) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::C),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::CTRL, Key::Comma),
                i.consume_key(Modifiers::SHIFT, Key::F10),
            )
        });
        if settings {
            self.toggle_settings();
            return;
        }
        if self.page == Page::Settings {
            if esc {
                self.page = Page::Search;
                self.focus_search = true;
            }
            return;
        }
        let last = self.rows.len().saturating_sub(1);
        let before = self.selected;
        if down {
            self.selected = (self.selected + 1).min(last);
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        if pg_down {
            self.selected = (self.selected + page).min(last);
        }
        if pg_up {
            self.selected = self.selected.saturating_sub(page);
        }
        if self.selected != before {
            self.scroll_to_selected = true;
        }
        if menu {
            if let (Some(hwnd), Some(row)) = (self.hwnd, self.rows.get(self.selected)) {
                if let Err(e) = crate::win::context_menu(hwnd, &row.path.clone()) {
                    self.flash(e);
                }
            }
        }
        if ctrl_enter {
            self.open_selected(ctx, true);
        } else if enter {
            self.open_selected(ctx, false);
        }
        if copy {
            if let Some(row) = self.rows.get(self.selected) {
                ctx.copy_text(row.path.clone());
                self.flash("Calea a fost copiată");
            }
        }
        if esc {
            if self.query.is_empty() {
                self.hide(ctx);
            } else {
                self.query.clear();
                self.run_search();
            }
        }
    }

    fn toggle_settings(&mut self) {
        if self.page == Page::Settings {
            self.page = Page::Search;
            self.focus_search = true;
        } else {
            self.page = Page::Settings;
            self.searcher.ask_config();
        }
    }

    /// The search card: magnifier, text field, clear button or hotkey hint, settings.
    fn search_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let focused = self.search_id.is_some_and(|id| ui.ctx().memory(|m| m.has_focus(id)));
        let stroke = if focused { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.border) };
        Frame::NONE
            .fill(p.surface)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin { left: 14, right: 10, top: 10, bottom: 10 })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (icon_rect, _) = ui.allocate_exact_size(Vec2::new(22.0, 30.0), Sense::hover());
                    paint_magnifier(ui.painter(), icon_rect.center(), 8.0, if focused { p.accent } else { p.text3 });
                    ui.add_space(6.0);
                    let right_w = 200.0;
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .hint_text(egui::RichText::new("Caută fișiere, foldere…").size(19.0).color(p.text3))
                        .font(FontId::proportional(19.0))
                        .text_color(p.text)
                        .frame(Frame::NONE)
                        .margin(Vec2::new(0.0, 4.0))
                        .desired_width((ui.available_width() - right_w).max(120.0));
                    let resp = ui.add(edit);
                    self.search_id = Some(resp.id);
                    if self.focus_search {
                        resp.request_focus();
                        self.focus_search = false;
                    }
                    if resp.changed() {
                        self.page = Page::Search;
                        self.run_search();
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let gear = icon_button(ui, p, IconKind::Gear, self.page == Page::Settings)
                            .on_hover_text("Setări (Ctrl+,)");
                        if gear.clicked() {
                            self.toggle_settings();
                        }
                        ui.add_space(4.0);
                        if !self.query.is_empty() {
                            if icon_button(ui, p, IconKind::Close, false).on_hover_text("Golește (Esc)").clicked() {
                                self.query.clear();
                                self.run_search();
                                self.focus_search = true;
                            }
                        } else if let Some(k) = self.hotkey_label() {
                            key_hint(ui, p, k);
                        }
                    });
                });
            });
    }

    fn filter_chips(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for f in Filter::ALL {
                if chip(ui, p, f.label(), self.filter == f).clicked() && self.filter != f {
                    self.filter = f;
                    self.run_search();
                    self.focus_search = true;
                }
            }
        });
    }

    fn results(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) {
        if let Some(msg) = self.offline.clone() {
            empty_state(
                ui,
                p,
                p.error,
                "Serviciul whereti nu rulează",
                &format!("Pornește-l dintr-un terminal de administrator: whereti-service start   ({msg})"),
                &[],
            );
            return;
        }
        if self.rows.is_empty() {
            let (title, sub) = if self.query.trim().is_empty() && self.filter == Filter::All {
                ("Nimic modificat recent", "Fișierele modificate în Desktop, Documents și Downloads apar aici.".to_string())
            } else {
                ("Nimic găsit", format!("Niciun nume nu se potrivește cu „{}”. Încearcă:", self.query.trim()))
            };
            if let Some(example) = empty_state(ui, p, p.text3, title, &sub, &["factura", "ext:pdf", "dm:today", "*.jpg"]) {
                self.query = example.to_string();
                self.filter = Filter::All;
                self.run_search();
                self.focus_search = true;
            }
            return;
        }
        ui.add_space(4.0);
        let header = if self.recent_mode {
            "RECENTE".to_string()
        } else {
            format!("{} REZULTATE", shell::count(self.total))
        };
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(header).size(11.0).color(p.text3).family(semibold()));
        });
        ui.add_space(4.0);
        for w in self.warnings.clone() {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(w).size(12.0).color(p.warn));
            });
        }

        // Keep the selected row visible after keyboard moves.
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if self.scroll_to_selected {
            let top = self.selected as f32 * ROW_HEIGHT;
            let mut off = self.scroll_offset;
            if top < off {
                off = top;
            } else if top + ROW_HEIGHT > off + self.view_height {
                off = top + ROW_HEIGHT - self.view_height;
            }
            area = area.vertical_scroll_offset(off.max(0.0));
            self.scroll_to_selected = false;
        }
        let mut clicked: Option<(usize, bool)> = None;
        let mut dragged: Option<usize> = None;
        let mut menu_for: Option<usize> = None;
        let rows = &self.rows;
        let selected = self.selected;
        let tz = self.tz;
        let needles = highlight_needles(&self.query);
        let now = now_minutes();
        let out = area.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for i in range {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click_and_drag());
                paint_row(ui, p, rect, &rows[i], i == selected, resp.hovered(), tz, now, &needles);
                if resp.drag_started() {
                    dragged = Some(i);
                } else if resp.secondary_clicked() {
                    menu_for = Some(i);
                } else if resp.double_clicked() {
                    clicked = Some((i, true));
                } else if resp.clicked() {
                    clicked = Some((i, false));
                }
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.view_height = out.inner_rect.height();
        if let Some((i, open)) = clicked {
            self.selected = i;
            if open {
                self.open_selected(ctx, false);
            }
            self.focus_search = true;
        }
        // Drag a result out to Explorer, the desktop, a mail…
        if let (Some(i), Some(hwnd)) = (dragged, self.hwnd) {
            self.selected = i;
            let path = self.rows[i].path.clone();
            shell::trace(format!("drag start: {path}"));
            let result = crate::win::drag_file(hwnd, &path);
            shell::trace(format!("drag end: {result:?}"));
            if let Err(e) = result {
                self.flash(e);
            }
            // The OS ran its own drag loop and swallowed the mouse release.
            ctx.stop_dragging();
        }
        // Right click: the Explorer menu for that file.
        if let (Some(i), Some(hwnd)) = (menu_for, self.hwnd) {
            self.selected = i;
            let path = self.rows[i].path.clone();
            if let Err(e) = crate::win::context_menu(hwnd, &path) {
                self.flash(e);
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let rect = ui.max_rect();
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.border));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let small = |t: String, c: Color32| egui::RichText::new(t).size(12.0).color(c);
            if let Some((msg, at)) = &self.message {
                if at.elapsed() < Duration::from_secs(8) {
                    ui.label(small(msg.clone(), p.warn));
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                    return;
                }
            }
            if self.offline.is_none() && self.page == Page::Search {
                let ms = format!("{:.0}", self.round_trip.as_secs_f64() * 1000.0);
                ui.label(small(format!("{ms} ms"), p.text3));
                ui.label(small("·".into(), p.text3));
                ui.label(small(
                    "Enter deschide  ·  Ctrl+Enter arată în Explorer  ·  click dreapta pentru meniu  ·  trage în altă aplicație"
                        .into(),
                    p.text3,
                ));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(14.0);
                let (dot, text) = match (&self.offline, &self.status) {
                    (Some(_), _) => (p.error, "serviciul nu rulează".to_string()),
                    (None, Some(s)) if s.state == "ready" => {
                        (p.ok, format!("{} fișiere indexate · actualizat", shell::count(s.entries)))
                    }
                    (None, Some(s)) => (p.warn, format!("{} fișiere · {}", shell::count(s.entries), state_ro(&s.state))),
                    (None, None) => (p.text3, "se conectează…".to_string()),
                };
                ui.label(small(text, p.text2));
                let (r, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, dot);
            });
        });
    }
}

impl eframe::App for WheretiApp {
    /// Runs even while the window is hidden (tray, hotkey, second launch).
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if Requests::take(&self.requests.quit) {
            self.quitting = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if Requests::take(&self.requests.show) {
            self.show(ctx);
        }
        if Requests::take(&self.requests.toggle) {
            if self.visible {
                self.hide(ctx);
            } else {
                self.show(ctx);
            }
        }
        self.apply_updates();
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The close button hides the window to the tray; "Ieșire" really quits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tray.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.hide(&ctx);
            return;
        }
        // Poll the service only while the window is shown: hidden, the app
        // must not wake up at all.
        if self.visible {
            if self.last_status_ask.elapsed() >= STATUS_EVERY {
                self.searcher.ask_status();
                self.last_status_ask = Instant::now();
            }
            ctx.request_repaint_after(STATUS_EVERY);
        }
        let p = Palette::of(&ctx, self.accent);
        if self.hwnd.is_none() {
            self.hwnd = crate::win::hwnd(frame);
        }
        // Title bar in our colours, refreshed when the theme changes.
        if self.styled_theme != Some(ctx.theme()) {
            if let Some(hwnd) = self.hwnd {
                crate::win::style_frame(hwnd, p.dark, p.bg, p.text2, p.border);
                self.styled_theme = Some(ctx.theme());
            }
        }
        self.handle_keys(&ctx);

        egui::Panel::bottom("status")
            .frame(Frame::NONE.fill(p.bg))
            .exact_size(34.0)
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui, &p));
        egui::Panel::top("search")
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 18, right: 18, top: 14, bottom: 8 }))
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                self.search_card(ui, &p);
                ui.add_space(10.0);
                if self.page == Page::Search {
                    self.filter_chips(ui, &p);
                }
            });
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 10, right: 10, top: 2, bottom: 0 }))
            .show(ui, |ui| match self.page {
                Page::Search => self.results(ui, &ctx, &p),
                Page::Settings => crate::settings::show(self, ui, &p),
            });
    }
}

pub(crate) fn state_ro(state: &str) -> String {
    match state {
        "ready" => "actualizat".into(),
        "loading" => "se încarcă indexul".into(),
        "indexing" => "se construiește indexul".into(),
        "catching up" => "se aduce la zi".into(),
        "reindexing" => "se reconstruiește indexul".into(),
        s if s.starts_with("rescanning") => s.replace("rescanning", "se rescanează"),
        s => s.to_string(),
    }
}

pub(crate) fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

// ---------------------------------------------------------------- widgets

#[derive(Clone, Copy)]
pub(crate) enum IconKind {
    Gear,
    Close,
    Back,
}

/// A 30×30 icon button.
pub(crate) fn icon_button(ui: &mut egui::Ui, p: &Palette, kind: IconKind, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 8.0, p.accent_soft);
    } else if resp.hovered() {
        painter.rect_filled(rect, 8.0, p.hover);
    }
    let c = rect.center();
    let col = if active {
        p.accent
    } else if resp.hovered() {
        p.text
    } else {
        p.text2
    };
    let s = Stroke::new(1.6, col);
    match kind {
        IconKind::Close => {
            let d = 5.0;
            painter.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], s);
            painter.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], s);
        }
        IconKind::Back => {
            painter.line_segment([c + Vec2::new(-6.0, 0.0), c + Vec2::new(6.0, 0.0)], s);
            painter.line_segment([c + Vec2::new(-6.0, 0.0), c + Vec2::new(-1.5, -4.5)], s);
            painter.line_segment([c + Vec2::new(-6.0, 0.0), c + Vec2::new(-1.5, 4.5)], s);
        }
        IconKind::Gear => {
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::TAU / 8.0;
                let dir = Vec2::new(a.cos(), a.sin());
                painter.line_segment([c + dir * 5.5, c + dir * 8.0], Stroke::new(2.4, col));
            }
            painter.circle_stroke(c, 5.5, s);
            painter.circle_stroke(c, 2.0, Stroke::new(1.4, col));
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A rounded pill; solid accent when selected.
pub(crate) fn chip(ui: &mut egui::Ui, p: &Palette, text: &str, selected: bool) -> egui::Response {
    let font = FontId::new(13.0, if selected { semibold() } else { FontFamily::Proportional });
    let color = if selected { p.on_accent } else { p.text2 };
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = Vec2::new(galley.size().x + 26.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 15.0, p.accent);
    } else {
        let fill = if resp.hovered() { p.hover } else { p.surface };
        painter.rect(rect, 15.0, fill, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    }
    let pos = rect.center() - galley.size() / 2.0;
    let text_color = if !selected && resp.hovered() { p.text } else { color };
    painter.galley(pos, galley, text_color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "Ctrl+Shift+Space" drawn as small key caps.
fn key_hint(ui: &mut egui::Ui, p: &Palette, keys: &str) {
    for part in keys.split('+').rev() {
        let galley = ui.painter().layout_no_wrap(part.to_string(), FontId::proportional(11.5), p.text2);
        let size = Vec2::new(galley.size().x + 12.0, 22.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter().rect(rect, 5.0, p.bg, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text2);
        ui.add_space(2.0);
    }
}

pub(crate) fn paint_magnifier(painter: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let ring = c - Vec2::new(r * 0.18, r * 0.18);
    painter.circle_stroke(ring, r * 0.78, Stroke::new(r * 0.24, color));
    let dir = Vec2::new(1.0, 1.0).normalized();
    painter.line_segment([ring + dir * r * 0.78, ring + dir * r * 1.5], Stroke::new(r * 0.28, color));
}

/// Centered icon, title, text and optional example chips (returns the clicked one).
fn empty_state(
    ui: &mut egui::Ui,
    p: &Palette,
    tint: Color32,
    title: &str,
    sub: &str,
    examples: &[&'static str],
) -> Option<&'static str> {
    let mut picked = None;
    ui.add_space((ui.available_height() * 0.18).min(90.0));
    ui.vertical_centered(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 30.0, theme::with_alpha(tint, 0.10));
        paint_magnifier(ui.painter(), r.center(), 14.0, tint);
        ui.add_space(12.0);
        ui.label(egui::RichText::new(title).size(18.0).family(semibold()).color(p.text));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(sub).size(13.0).color(p.text2));
        if !examples.is_empty() {
            ui.add_space(12.0);
            let width: f32 = examples.iter().map(|e| e.len() as f32 * 7.5 + 32.0).sum();
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                ui.spacing_mut().item_spacing.x = 6.0;
                for e in examples {
                    if chip(ui, p, e, false).clicked() {
                        picked = Some(*e);
                    }
                }
            });
        }
    });
    picked
}

// ---------------------------------------------------------------- rows

/// One result: file-type tile, name over its folder, date and size.
#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &egui::Ui,
    p: &Palette,
    rect: Rect,
    row: &Row,
    selected: bool,
    hovered: bool,
    tz: i32,
    now: i64,
    needles: &[String],
) {
    let painter = ui.painter_at(rect);
    let inner = rect.shrink2(Vec2::new(4.0, 2.0));
    if selected {
        painter.rect_filled(inner, 9.0, p.accent_soft);
        let bar = Rect::from_center_size(Pos2::new(inner.left() + 3.0, inner.center().y), Vec2::new(3.0, 22.0));
        painter.rect_filled(bar, 2.0, p.accent);
    } else if hovered {
        painter.rect_filled(inner, 9.0, p.hover);
    }

    let (folder, name) = match row.path.rfind('\\') {
        Some(i) if i + 1 < row.path.len() => (&row.path[..i], &row.path[i + 1..]),
        _ => ("", row.path.as_str()),
    };
    let tile = Rect::from_center_size(Pos2::new(inner.left() + 30.0, inner.center().y), Vec2::splat(34.0));
    paint_tile(&painter, p, tile, row.is_dir, name);

    let right_w = 230.0;
    let text_left = tile.right() + 14.0;
    let text_w = (inner.right() - right_w - text_left).max(80.0);
    let name_job = highlighted(name, needles, FontId::new(15.0, semibold()), p.text, p.accent, text_w);
    let name_g = ui.fonts_mut(|f| f.layout_job(name_job));
    painter.galley(Pos2::new(text_left, inner.top() + 8.0), name_g, p.text);
    let folder_text = middle_elide(ui, folder, FontId::proportional(12.5), text_w);
    painter.text(
        Pos2::new(text_left, inner.top() + 30.0),
        Align2::LEFT_TOP,
        folder_text,
        FontId::proportional(12.5),
        p.text2,
    );

    let size = if row.is_dir { String::new() } else { shell::size(row.size) };
    painter.text(
        Pos2::new(inner.right() - 14.0, inner.center().y),
        Align2::RIGHT_CENTER,
        size,
        FontId::proportional(12.5),
        p.text3,
    );
    painter.text(
        Pos2::new(inner.right() - 96.0, inner.center().y),
        Align2::RIGHT_CENTER,
        relative_time(row.modified, tz, now),
        FontId::proportional(12.5),
        p.text2,
    );
}

/// Folder glyph, or a coloured tile with the extension (PDF, DOCX, JPG…).
fn paint_tile(painter: &egui::Painter, p: &Palette, tile: Rect, is_dir: bool, name: &str) {
    if is_dir {
        let yellow = Color32::from_rgb(0xF2, 0xB8, 0x3E);
        painter.rect_filled(tile, 9.0, theme::with_alpha(yellow, if p.dark { 0.20 } else { 0.18 }));
        let body = Rect::from_center_size(tile.center() + Vec2::new(0.0, 1.5), Vec2::new(19.0, 13.0));
        let tab = Rect::from_min_size(body.min - Vec2::new(0.0, 3.0), Vec2::new(8.0, 5.0));
        painter.rect_filled(tab, 1.5, yellow);
        painter.rect_filled(body, 2.5, yellow);
        return;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| e.len() <= 5).unwrap_or_default();
    let color = type_color(&ext, p.dark);
    painter.rect_filled(tile, 9.0, theme::with_alpha(color, if p.dark { 0.24 } else { 0.14 }));
    let label: String = if ext.is_empty() { "—".into() } else { ext.to_uppercase().chars().take(4).collect() };
    let size = if label.chars().count() >= 4 { 9.5 } else { 10.5 };
    painter.text(tile.center(), Align2::CENTER_CENTER, label, FontId::new(size, semibold()), color);
}

fn type_color(ext: &str, dark: bool) -> Color32 {
    let c = match ext {
        "pdf" => (0xE0, 0x3E, 0x36),
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" => (0x2F, 0x6F, 0xE4),
        "xls" | "xlsx" | "ods" | "csv" => (0x1E, 0x8E, 0x4E),
        "ppt" | "pptx" | "odp" => (0xD9, 0x6A, 0x2B),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "svg" | "ico" | "raw" => {
            (0x16, 0x9E, 0x8F)
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" => (0x8E, 0x5B, 0xD6),
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" => (0xC2, 0x3B, 0x7A),
        "zip" | "rar" | "7z" | "tar" | "gz" | "zst" | "cab" | "iso" => (0x9C, 0x6B, 0x3A),
        "exe" | "msi" | "bat" | "cmd" | "ps1" | "dll" | "lnk" => (0x55, 0x65, 0x7A),
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cpp" | "h" | "cs" | "php" | "rb" | "html"
        | "css" | "json" | "yaml" | "yml" | "toml" | "xml" | "sql" | "sh" => (0xC9, 0x7A, 0x14),
        "eml" | "msg" => (0x2B, 0x8C, 0xC4),
        _ => (0x80, 0x80, 0x8C),
    };
    let base = Color32::from_rgb(c.0, c.1, c.2);
    if dark {
        theme::mix(base, Color32::WHITE, 0.30)
    } else {
        base
    }
}

/// Plain words of the query, folded, for highlighting (not ext:, !x, *.x…).
fn highlight_needles(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|w| !w.contains(':') && !w.starts_with('!') && !w.contains(['*', '?', '\\', '|', '"']))
        .map(whereti_query::fold)
        .filter(|w| !w.is_empty())
        .collect()
}

/// The name with every folded occurrence of a needle in accent colour.
/// Folding can change lengths (ș → s, ß → ss), so matches are found in the
/// folded text and mapped back to the original characters.
fn highlighted(name: &str, needles: &[String], font: FontId, color: Color32, accent: Color32, width: f32) -> LayoutJob {
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    let marked = match_mask(name, needles);
    let mut job = LayoutJob::default();
    let mut i = 0;
    while i < chars.len() {
        let m = marked[i];
        let mut j = i;
        while j < chars.len() && marked[j] == m {
            j += 1;
        }
        let start = chars[i].0;
        let end = chars.get(j).map_or(name.len(), |c| c.0);
        job.append(&name[start..end], 0.0, TextFormat::simple(font.clone(), if m { accent } else { color }));
        i = j;
    }
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    job
}

/// For each character of `name`: is it part of a (folded) needle match?
fn match_mask(name: &str, needles: &[String]) -> Vec<bool> {
    let chars: Vec<char> = name.chars().collect();
    let mut folded = String::new();
    let mut owner: Vec<usize> = Vec::new(); // folded byte -> original char index
    for (ci, ch) in chars.iter().enumerate() {
        let mut piece = String::new();
        whereti_query::fold_into(&ch.to_string(), &mut piece);
        owner.extend(std::iter::repeat_n(ci, piece.len()));
        folded.push_str(&piece);
    }
    let mut marked = vec![false; chars.len()];
    for n in needles.iter().filter(|n| !n.is_empty()) {
        let mut from = 0;
        while let Some(pos) = folded[from..].find(n.as_str()) {
            let start = from + pos;
            for &o in &owner[start..start + n.len()] {
                marked[o] = true;
            }
            from = start + n.len();
        }
    }
    marked
}

/// `C:\Users\ana\…\proiect\docs`: keeps the drive and as many trailing
/// folders as fit.
fn middle_elide(ui: &egui::Ui, path: &str, font: FontId, width: f32) -> String {
    let fits =
        |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x) <= width;
    if fits(path) {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('\\').collect();
    if parts.len() < 3 {
        return path.to_string();
    }
    for keep in (1..parts.len() - 1).rev() {
        let candidate = format!("{}\\…\\{}", parts[0], parts[parts.len() - keep..].join("\\"));
        if fits(&candidate) {
            return candidate;
        }
    }
    format!("{}\\…\\{}", parts[0], parts[parts.len() - 1])
}

fn now_minutes() -> i64 {
    let unix =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    unix as i64 + 134_774 * 1440
}

/// "azi, 14:32", "ieri, 09:10", "joi, 18:05", "12 oct.", "3 mar. 2024".
pub(crate) fn relative_time(minutes_utc: u32, tz: i32, now_utc: i64) -> String {
    if minutes_utc == 0 {
        return String::new();
    }
    const MONTHS: [&str; 12] =
        ["ian.", "feb.", "mar.", "apr.", "mai", "iun.", "iul.", "aug.", "sept.", "oct.", "nov.", "dec."];
    const DAYS: [&str; 7] = ["luni", "marți", "miercuri", "joi", "vineri", "sâmbătă", "duminică"];
    let local = minutes_utc as i64 + tz as i64;
    let today = (now_utc + tz as i64).div_euclid(1440);
    let day = local.div_euclid(1440);
    let hm = format!("{:02}:{:02}", local.rem_euclid(1440) / 60, local.rem_euclid(1440) % 60);
    let (y, m, d) = whereti_query::civil_from_day(day);
    let (ty, _, _) = whereti_query::civil_from_day(today);
    match today - day {
        0 => format!("azi, {hm}"),
        1 => format!("ieri, {hm}"),
        2..=6 => format!("{}, {hm}", DAYS[day.rem_euclid(7) as usize]), // day 0 (1601-01-01) was a Monday
        _ if y == ty => format!("{d} {}", MONTHS[(m - 1) as usize]),
        _ => format!("{d} {} {y}", MONTHS[(m - 1) as usize]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_in_romanian() {
        let day = whereti_query::day_from_civil;
        let now = day(2026, 10, 24) * 1440 + 15 * 60; // Saturday 15:00 UTC
        let at = |y, m, d, h: i64| (day(y, m, d) * 1440 + h * 60) as u32;
        assert_eq!(relative_time(at(2026, 10, 24, 11), 180, now), "azi, 14:00");
        assert_eq!(relative_time(at(2026, 10, 23, 6), 180, now), "ieri, 09:00");
        assert_eq!(relative_time(at(2026, 10, 22, 6), 180, now), "joi, 09:00");
        assert_eq!(relative_time(at(2026, 3, 12, 6), 180, now), "12 mar.");
        assert_eq!(relative_time(at(2024, 9, 3, 6), 180, now), "3 sept. 2024");
    }

    #[test]
    fn needles_skip_syntax() {
        assert_eq!(highlight_needles("Mașină ext:pdf !tmp *.doc raport"), vec!["masina", "raport"]);
    }

    #[test]
    fn highlight_maps_folded_matches_back_to_diacritics() {
        let m = match_mask("Mașină de spălat.docx", &["masina".to_string(), "spal".to_string()]);
        let marked: String = "Mașină de spălat.docx".chars().zip(&m).filter(|(_, &b)| b).map(|(c, _)| c).collect();
        assert_eq!(marked, "Mașinăspăl");
    }
}
