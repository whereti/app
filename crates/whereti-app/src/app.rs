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

use crate::file_icons::FileIcons;
use crate::i18n::tr;
use crate::trf;
use crate::icons::{self, Icon};
use crate::preview::Preview;
use crate::searcher::{Job, Searcher, Update};
use crate::settings::SettingsState;
use crate::shell;
use crate::theme::{self, Palette};
use crate::tray::{Requests, Tray, PREFERRED_HOTKEY};

const ROW_HEIGHT: f32 = 52.0;
const RESULT_LIMIT: u32 = 500;
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// While indexing, the status is asked for this often even with the window
/// hidden, for the tray icon's progress. Once ready: never while hidden.
const STATUS_HIDDEN_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    All,
    Documents,
    Images,
    Audio,
    Video,
    Folders,
}

impl Filter {
    const ALL: [Filter; 6] = [Filter::All, Filter::Documents, Filter::Images, Filter::Audio, Filter::Video, Filter::Folders];

    fn label(self) -> &'static str {
        match self {
            Filter::All => tr("All", "Toate"),
            Filter::Documents => tr("Documents", "Documente"),
            Filter::Images => tr("Images", "Imagini"),
            Filter::Audio => tr("Audio", "Audio"),
            Filter::Video => tr("Video", "Video"),
            Filter::Folders => tr("Folders", "Foldere"),
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
    /// `rows[content_start..]` were found in file contents (not by name).
    content_start: Option<usize>,
    /// A content search for the current query is on its way.
    content_pending: bool,
    round_trip: Duration,
    warnings: Vec<String>,
    recent_mode: bool,
    selected: usize,
    scroll_to_selected: bool,
    scroll_offset: f32,
    view_height: f32,
    pub(crate) status: Option<StatusReply>,
    /// Content indexing speed, for "~3 min left".
    eta: Eta,
    /// Progress shown on the tray icon, in 5 % steps (`None` = plain icon).
    tray_progress: Option<u8>,
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
    preview: Preview,
    /// Explorer's own file icons, cached per type.
    pub(crate) file_icons: FileIcons,
}

impl WheretiApp {
    pub fn new(ctx: &egui::Context, requests: Arc<Requests>, tray: Option<Tray>, visible: bool) -> Self {
        let wake_ctx = ctx.clone();
        let searcher = Searcher::start(move || wake_ctx.request_repaint());
        let settings = SettingsState::load();
        let wake_ctx = ctx.clone();
        let preview = Preview::start(settings.preview, move || wake_ctx.request_repaint());
        let accent = theme::system_accent();
        theme::apply(ctx, accent);
        let mut app = WheretiApp {
            query: String::new(),
            filter: Filter::All,
            searcher,
            generation: 0,
            rows: Vec::new(),
            total: 0,
            content_start: None,
            content_pending: false,
            round_trip: Duration::ZERO,
            warnings: Vec::new(),
            recent_mode: true,
            selected: 0,
            scroll_to_selected: false,
            scroll_offset: 0.0,
            view_height: 0.0,
            status: None,
            eta: Eta::default(),
            tray_progress: None,
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
            settings,
            requests,
            tray,
            preview,
            file_icons: FileIcons::new(),
        };
        app.settings.apply_theme(ctx);
        match app.tray.as_ref().map(|t| t.hotkey) {
            // Said once: the shortcut actually used is then remembered.
            Some(Some(k)) if k != PREFERRED_HOTKEY && app.settings.hotkey.is_none() => {
                app.flash(trf!(
                    "{PREFERRED_HOTKEY} is used by another app — the shortcut is {k}",
                    "{PREFERRED_HOTKEY} e folosită de altă aplicație — tasta rapidă este {k}"
                ));
                app.settings.hotkey = Some(k.to_string());
                app.settings.save();
            }
            Some(None) => app.flash(tr("No shortcut is free; open whereti from its tray icon", "Nicio tastă rapidă disponibilă; deschide whereti din iconița din tray")),
            _ => {}
        }
        app.run_search();
        app
    }

    pub(crate) fn content_eta(&self, c: &whereti_ipc::ContentStatus) -> Option<Duration> {
        self.eta.left(c)
    }

    pub(crate) fn hotkey_label(&self) -> Option<&'static str> {
        self.tray.as_ref().and_then(|t| t.hotkey)
    }

    /// What is sent: the typed text plus the filter, or "recent files" when
    /// there is nothing to search for.
    fn effective_query(&self) -> (String, Sort, bool) {
        let q = self.query.trim();
        if q.is_empty() && self.filter == Filter::All {
            // answered locally from Windows' list of opened files (see recent.rs)
            return (String::new(), Sort::Modified, true);
        }
        if q.is_empty() {
            // only a filter: the newest of the user's own files of that kind
            return (format!("{} mine:", self.filter.clause()), Sort::Modified, false);
        }
        (format!("{q} {}", self.filter.clause()), Sort::Relevance, false)
    }

    pub(crate) fn run_search(&mut self) {
        self.generation += 1;
        let (query, sort, recent) = self.effective_query();
        self.recent_mode = recent;
        let content = !recent && !highlight_needles(&self.query).is_empty();
        self.content_pending = content;
        self.searcher.search(Job { generation: self.generation, query, sort, limit: RESULT_LIMIT, content, recent });
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
                    self.content_start = None;
                    self.round_trip = round_trip;
                    self.warnings = reply.warnings;
                    self.selected = 0;
                    self.scroll_offset = 0.0;
                    self.scroll_to_selected = true;
                }
                Update::Content { generation, reply } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.content_pending = false;
                    let named: std::collections::HashSet<String> = self.rows.iter().map(|r| r.path.clone()).collect();
                    let extra: Vec<Row> = reply.rows.into_iter().filter(|r| !named.contains(&r.path)).collect();
                    if !extra.is_empty() {
                        self.content_start = Some(self.rows.len());
                        self.rows.extend(extra);
                    }
                }
                Update::Status(s) => {
                    self.offline = None;
                    self.eta.sample(&s.content);
                    let eta = self.eta.left(&s.content);
                    let progress = indexing_fraction(&s).map(|f| ((f * 20.0).floor() as u8).min(19) * 5);
                    if let Some(t) = &self.tray {
                        if progress != self.tray_progress {
                            let _ = t.icon.set_icon(Some(crate::tray::icon_with_progress(progress)));
                            self.tray_progress = progress;
                        }
                        let mut tip = trf!("whereti — {} files · {}", "whereti — {} fișiere · {}", shell::count(s.entries), state_ro(&s.state));
                        if content_active(&s.content) {
                            tip.push_str(&trf!("\nContents: {}", "\nConținut: {}", content_progress(&s.content, eta)));
                        }
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
        let search_page = self.page == Page::Search;
        let (down, up, pg_down, pg_up, ctrl_enter, enter, copy, esc, settings, menu, tab) = ctx.input_mut(|i| {
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
                search_page && i.consume_key(Modifiers::NONE, Key::Tab),
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
        if tab {
            self.toggle_preview();
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
                self.flash(tr("Path copied", "Calea a fost copiată"));
            }
        }
        if esc {
            if self.query.is_empty() {
                self.hide(ctx);
            } else {
                self.query.clear();
                self.run_search();
                // egui drops focus on Esc; keep typing in the search field
                self.focus_search = true;
            }
        }
    }

    fn toggle_preview(&mut self) {
        self.preview.open = !self.preview.open;
        self.settings.preview = self.preview.open;
        self.settings.save();
        self.focus_search = true;
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

    /// The search field: one quiet surface with a hairline; on focus a soft
    /// accent ring. Clear button or the shortcut on the right, then the
    /// preview and settings buttons.
    fn search_card(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let focused = self.search_id.is_some_and(|id| ui.ctx().memory(|m| m.has_focus(id)));
        let frame = Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0, if focused { theme::with_alpha(p.accent, 0.9) } else { p.border }))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin { left: 16, right: 8, top: 8, bottom: 8 });
        let out = frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                let (icon_rect, _) = ui.allocate_exact_size(Vec2::new(20.0, 32.0), Sense::hover());
                icons::paint(ui.painter(), icon_rect, Icon::Search, if focused { p.accent } else { p.text3 });
                ui.add_space(8.0);
                let right_w = 190.0;
                let edit = egui::TextEdit::singleline(&mut self.query)
                    .hint_text(egui::RichText::new(tr("Search files, folders, documents…", "Caută fișiere, foldere, documente…")).size(18.0).color(p.text3))
                    .font(FontId::proportional(18.0))
                    .text_color(p.text)
                    .frame(Frame::NONE)
                    .margin(Vec2::new(0.0, 5.0))
                    // Tab toggles the preview on the results page instead of moving focus
                    .lock_focus(self.page == Page::Search)
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
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let gear =
                        icon_button(ui, p, IconKind::Gear, self.page == Page::Settings).on_hover_text(tr("Settings (Ctrl+,)", "Setări (Ctrl+,)"));
                    if gear.clicked() {
                        self.toggle_settings();
                    }
                    if self.page == Page::Search {
                        let tip = if self.preview.open {
                            tr("Hide the preview (Tab)", "Ascunde previzualizarea (Tab)")
                        } else {
                            tr("Show the preview (Tab)", "Arată previzualizarea (Tab)")
                        };
                        if icon_button(ui, p, IconKind::Preview, self.preview.open).on_hover_text(tip).clicked() {
                            self.toggle_preview();
                        }
                    }
                    // a hairline between the buttons and the field's own controls
                    let (sep, _) = ui.allocate_exact_size(Vec2::new(13.0, 20.0), Sense::hover());
                    ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, p.border));
                    if !self.query.is_empty() {
                        if icon_button(ui, p, IconKind::Close, false).on_hover_text(tr("Clear (Esc)", "Golește (Esc)")).clicked() {
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
        if focused {
            // soft focus ring outside the hairline
            let r = out.response.rect.expand(2.5);
            ui.painter().rect_stroke(r, 14.0, Stroke::new(3.0, theme::with_alpha(p.accent, 0.16)), egui::StrokeKind::Middle);
        }
    }

    /// Filters as quiet tabs: the chosen one in full colour with a thin
    /// accent underline, a hairline under the whole row.
    fn filter_chips(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 22.0;
            ui.add_space(4.0);
            for f in Filter::ALL {
                if tab(ui, p, f.label(), self.filter == f).clicked() && self.filter != f {
                    self.filter = f;
                    self.run_search();
                    self.focus_search = true;
                }
            }
        });
        let r = row.response.rect;
        ui.painter().hline(ui.max_rect().x_range(), r.bottom(), Stroke::new(1.0, p.line));
    }

    fn results(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) {
        if let Some(msg) = self.offline.clone() {
            empty_state(
                ui,
                p,
                Icon::Plug,
                p.error,
                tr("The whereti service is not running", "Serviciul whereti nu rulează"),
                &trf!(
                    "Start it from an Administrator terminal: whereti-service start   ({msg})",
                    "Pornește-l dintr-un terminal de administrator: whereti-service start   ({msg})"
                ),
                &[],
            );
            return;
        }
        if self.rows.is_empty() && self.content_pending {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(p.text3));
                ui.add_space(6.0);
                ui.label(egui::RichText::new(tr("Looking inside files too…", "Caut și în conținutul fișierelor…")).size(13.0).color(p.text3));
            });
            return;
        }
        if self.rows.is_empty() {
            let (title, sub) = if self.query.trim().is_empty() && self.filter == Filter::All {
(
                    tr("Nothing opened recently", "Nimic deschis recent"),
                    tr("Files you open and download show up here.", "Fișierele pe care le deschizi și cele descărcate apar aici.").to_string(),
                )
            } else {
                (
                    tr("Nothing found", "Nimic găsit"),
                    trf!(
                        "No file has “{}” in its name or text. Try:",
                        "Niciun fișier nu conține „{}” în nume sau în text. Încearcă:",
                        self.query.trim()
                    ),
                )
            };
            let icon = if self.query.trim().is_empty() && self.filter == Filter::All { Icon::Clock } else { Icon::Search };
            if let Some(example) = empty_state(ui, p, icon, p.text3, title, &sub, &["factura", "ext:pdf", "dm:today", "*.jpg"]) {
                self.query = example.to_string();
                self.filter = Filter::All;
                self.run_search();
                self.focus_search = true;
            }
            return;
        }
        ui.add_space(10.0);
        let header = if self.recent_mode {
            tr("RECENT", "RECENTE").to_string()
        } else if self.content_start == Some(0) {
            tr("NO NAME MATCHES", "NICIUN NUME NU SE POTRIVEȘTE").to_string()
        } else {
            trf!("{} RESULTS", "{} REZULTATE", shell::count(self.total))
        };
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(header).size(11.0).color(p.text3).family(semibold()));
        });
        ui.add_space(6.0);
        for w in self.warnings.clone() {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(w).size(12.0).color(p.warn));
            });
        }

        // Keep the selected row visible after keyboard moves.
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        // (the first frame does not know the view height yet: wait for it)
        let item_of = |row: usize, start: Option<usize>| row + start.is_some_and(|s| row >= s) as usize;
        if self.scroll_to_selected && self.view_height > 0.0 {
            let top = item_of(self.selected, self.content_start) as f32 * ROW_HEIGHT;
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
        let section = self.content_section_label();
        let rows = &self.rows;
        let file_icons = &mut self.file_icons;
        let selected = self.selected;
        let tz = self.tz;
        let needles = highlight_needles(&self.query);
        let now = now_minutes();
        let content_start = self.content_start;
        let items = rows.len() + content_start.is_some() as usize;
        let out = area.show_rows(ui, ROW_HEIGHT, items, |ui, range| {
            for item in range {
                let i = match content_start {
                    Some(s) if item == s => {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::hover());
                        paint_section(ui, p, rect, &section);
                        continue;
                    }
                    Some(s) if item > s => item - 1,
                    _ => item,
                };
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click_and_drag());
                let icon = file_icons.get(ui.ctx(), &rows[i].path, rows[i].is_dir);
                paint_row(ui, p, rect, &rows[i], icon.as_ref(), i == selected, resp.hovered(), tz, now, &needles);
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

    /// "GĂSIT ÎN CONȚINUT · 12" (+ indexing progress while it runs).
    fn content_section_label(&self) -> String {
        let shown = self.content_start.map_or(0, |s| self.rows.len() - s) as u64;
        let mut label = trf!("FOUND IN CONTENTS · {}", "GĂSIT ÎN CONȚINUT · {}", shell::count(shown));
        if let Some(c) = self.status.as_ref().map(|s| &s.content) {
            if content_active(c) {
                label.push_str(&trf!("   ·   indexing, {}", "   ·   indexare în curs, {}", content_progress(c, self.eta.left(c))));
            }
        }
        label
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let rect = ui.max_rect();
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.border));
        let font = FontId::proportional(12.0);
        let width = |ui: &egui::Ui, t: &str| ui.fonts_mut(|f| f.layout_no_wrap(t.to_string(), font.clone(), p.text).size().x);

        // Right: the index state. It always gets its room first.
        let (dot, state) = match (&self.offline, &self.status) {
            (Some(_), _) => (p.error, tr("the service is not running", "serviciul nu rulează").to_string()),
            (None, Some(s)) if s.state == "ready" && content_active(&s.content) => (
                p.warn,
                trf!(
                    "{} files · contents {}",
                    "{} fișiere · conținut {}",
                    shell::count(s.entries),
                    content_progress(&s.content, self.eta.left(&s.content))
                ),
            ),
            (None, Some(s)) if s.state == "ready" => (p.ok, trf!("{} files indexed · up to date", "{} fișiere indexate · actualizat", shell::count(s.entries))),
            (None, Some(s)) => (p.warn, trf!("{} files · {}", "{} fișiere · {}", shell::count(s.entries), state_ro(&s.state))),
            (None, None) => (p.text3, tr("connecting…", "se conectează…").to_string()),
        };
        let right_w = width(ui, &state) + 14.0 + 16.0;

        // Left: a message, or the longest set of hints that still fits.
        let room = rect.width() - right_w - 14.0 - 24.0;
        let mut left: Option<(String, Color32)> = None;
        if let Some((msg, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(8) {
                left = Some((msg.clone(), p.warn));
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
        }
        if left.is_none() && self.offline.is_none() && self.page == Page::Search {
            let ms = format!("{:.0} ms", self.round_trip.as_secs_f64() * 1000.0);
            let options = [
                trf!(
                    "{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer  ·  right-click for the menu  ·  drag into any app",
                    "{ms}  ·  Enter deschide  ·  Ctrl+Enter arată în Explorer  ·  click dreapta pentru meniu  ·  trage în altă aplicație"
                ),
                trf!(
                    "{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer  ·  right-click for the menu",
                    "{ms}  ·  Enter deschide  ·  Ctrl+Enter arată în Explorer  ·  click dreapta pentru meniu"
                ),
                trf!("{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer", "{ms}  ·  Enter deschide  ·  Ctrl+Enter arată în Explorer"),
                ms,
            ];
            left = options.into_iter().find(|t| width(ui, t) <= room).map(|t| (t, p.text3));
        }

        let y = rect.center().y + 1.0;
        let painter = ui.painter();
        if let Some((text, color)) = left {
            // a long message is cut rather than drawn under the state
            let job = LayoutJob {
                wrap: egui::text::TextWrapping { max_width: room.max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') },
                ..LayoutJob::simple_singleline(text, font.clone(), color)
            };
            let g = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(Pos2::new(rect.left() + 14.0, y - g.size().y / 2.0), g, color);
        }
        let x = rect.right() - 14.0;
        painter.text(Pos2::new(x, y), Align2::RIGHT_CENTER, &state, font.clone(), p.text2);
        painter.circle_filled(Pos2::new(x - width(ui, &state) - 9.0, y), 3.5, dot);
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
        // Hidden: ask once at start (started with --hidden), then only while indexing.
        let indexing = self.status.as_ref().and_then(indexing_fraction).is_some();
        if !self.visible && (indexing || (self.status.is_none() && self.offline.is_none())) {
            if self.last_status_ask.elapsed() >= STATUS_HIDDEN_EVERY {
                self.searcher.ask_status();
                self.last_status_ask = Instant::now();
            }
            ctx.request_repaint_after(STATUS_HIDDEN_EVERY);
        }
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
        self.file_icons.begin_frame();

        egui::Panel::bottom("status")
            .frame(Frame::NONE.fill(p.bg))
            .exact_size(32.0)
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| self.status_bar(ui, &p));
        egui::Panel::top("search")
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 0 }))
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                self.search_card(ui, &p);
                ui.add_space(14.0);
                if self.page == Page::Search {
                    self.filter_chips(ui, &p);
                } else {
                    ui.add_space(4.0);
                }
            });
        let show_preview =
            self.page == Page::Search && self.preview.open && self.offline.is_none() && !self.rows.is_empty();
        if show_preview {
            self.preview.select(self.rows.get(self.selected));
            let needles = highlight_needles(&self.query);
            let width = ctx.content_rect().width();
            egui::Panel::right("preview")
                .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 16, right: 20, top: 12, bottom: 12 }))
                .resizable(true)
                .show_separator_line(false)
                .default_size((width * 0.42).clamp(320.0, 640.0))
                .size_range(280.0..=(width * 0.7).max(300.0))
                .show(ui, |ui| {
                    // our own fine divider instead of egui's heavier one
                    let r = ui.max_rect();
                    ui.painter().vline(r.left() - 16.0, r.y_range().expand(12.0), Stroke::new(1.0, p.line));
                    let row = self.rows.get(self.selected);
                    self.preview.show(ui, &p, row, &mut self.file_icons, &needles, self.tz)
                });
        }
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 0 }))
            .show(ui, |ui| match self.page {
                Page::Search => self.results(ui, &ctx, &p),
                Page::Settings => crate::settings::show(self, ui, &p),
            });
    }
}

pub(crate) fn state_ro(state: &str) -> String {
    match state {
        "ready" => tr("up to date", "actualizat").into(),
        "loading" => tr("loading the index", "se încarcă indexul").into(),
        "indexing" => tr("building the index", "se construiește indexul").into(),
        "catching up" => tr("catching up", "se aduce la zi").into(),
        "reindexing" => tr("rebuilding the index", "se reconstruiește indexul").into(),
        s if s.starts_with("rescanning") && crate::i18n::ro() => s.replace("rescanning", "se rescanează"),
        s => s.to_string(),
    }
}

/// "1.234 din 5.678 documente (22%) · ~3 min rămase"
pub(crate) fn content_progress(c: &whereti_ipc::ContentStatus, left: Option<Duration>) -> String {
    let all = c.done + c.queued;
    let pct = (c.done * 100).checked_div(all).unwrap_or(100);
    let mut s = trf!("{} of {} documents ({pct}%)", "{} din {} documente ({pct}%)", shell::count(c.done), shell::count(all));
    if let Some(d) = left.filter(|_| c.state == "indexing") {
        s.push_str(&format!(" · {}", time_left(d)));
    }
    s.push_str(pause_note(c));
    s
}

/// "~40 s rămase", "~3 min rămase", "~2 h 10 min rămase"
pub(crate) fn time_left(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        trf!("~{} s left", "~{} s rămase", secs.max(5).div_ceil(5) * 5)
    } else if secs < 3600 {
        trf!("~{} min left", "~{} min rămase", secs.div_ceil(60))
    } else {
        trf!("~{} h {} min left", "~{} h {} min rămase", secs / 3600, (secs % 3600) / 60)
    }
}

/// How far indexing is, if something is being indexed (0.0–1.0).
fn indexing_fraction(s: &StatusReply) -> Option<f32> {
    if s.state == "indexing" || s.state == "reindexing" {
        return Some(0.0); // the name index has no counter: an empty bar
    }
    let c = &s.content;
    (content_active(c) && c.done + c.queued > 0).then(|| c.done as f32 / (c.done + c.queued) as f32)
}

/// Indexing, or paused in the middle of it.
pub(crate) fn content_active(c: &whereti_ipc::ContentStatus) -> bool {
    c.state == "indexing" || c.state.starts_with("paused")
}

/// "în pauză (pe baterie)" for a paused state, else empty.
pub(crate) fn pause_note(c: &whereti_ipc::ContentStatus) -> &'static str {
    match c.state.as_str() {
        "paused: battery" => tr(" · paused: on battery", " · în pauză: pe baterie"),
        "paused: fullscreen" => tr(" · paused: a game or presentation is running", " · în pauză: rulează un joc sau o prezentare"),
        "paused: busy" => tr(" · paused: the processor is busy", " · în pauză: procesorul e ocupat"),
        _ => "",
    }
}

/// Measures the content indexing speed over the current round.
#[derive(Default)]
pub(crate) struct Eta {
    start: Option<(Instant, u64)>,
}

impl Eta {
    fn sample(&mut self, c: &whereti_ipc::ContentStatus) {
        match self.start {
            // a pause resets the measurement: the speed before it says little after
            _ if c.state != "indexing" => self.start = None,
            // a new round starts counting from 0 again
            Some((_, d0)) if c.done < d0 => self.start = Some((Instant::now(), c.done)),
            None => self.start = Some((Instant::now(), c.done)),
            _ => {}
        }
    }

    /// Time left, once there is enough to measure (10 s and 3 documents).
    pub(crate) fn left(&self, c: &whereti_ipc::ContentStatus) -> Option<Duration> {
        let (t0, d0) = self.start?;
        let (elapsed, done) = (t0.elapsed().as_secs_f64(), c.done.saturating_sub(d0));
        if c.state != "indexing" || elapsed < 10.0 || done < 3 {
            return None;
        }
        Some(Duration::from_secs_f64(c.queued as f64 * elapsed / done as f64))
    }
}

/// The header between name results and content results: a small label and
/// a hairline running to the right edge.
fn paint_section(ui: &egui::Ui, p: &Palette, rect: Rect, label: &str) {
    let painter = ui.painter_at(rect);
    let y = rect.bottom() - 16.0;
    let g = painter.layout_no_wrap(label.to_string(), FontId::new(11.0, semibold()), p.accent);
    let x = rect.left() + 12.0;
    painter.galley(Pos2::new(x, y - g.size().y / 2.0), g.clone(), p.accent);
    painter.hline((x + g.size().x + 10.0)..=(rect.right() - 12.0), y, Stroke::new(1.0, p.line));
}

pub(crate) fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub(crate) fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

// ---------------------------------------------------------------- widgets

#[derive(Clone, Copy)]
pub(crate) enum IconKind {
    Gear,
    Close,
    Back,
    Preview,
}

/// A 32×32 icon button with a line icon; tinted when active.
pub(crate) fn icon_button(ui: &mut egui::Ui, p: &Palette, kind: IconKind, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 8.0, p.accent_soft);
    } else if resp.hovered() {
        painter.rect_filled(rect, 8.0, p.hover);
    }
    let col = if active {
        p.accent
    } else if resp.hovered() {
        p.text
    } else {
        p.text2
    };
    let icon = match kind {
        IconKind::Gear => Icon::Settings,
        IconKind::Close => Icon::Close,
        IconKind::Back => Icon::Back,
        IconKind::Preview => Icon::Panel,
    };
    let size = if matches!(kind, IconKind::Close) { 16.0 } else { 19.0 };
    icons::paint(painter, Rect::from_center_size(rect.center(), Vec2::splat(size)), icon, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A filter tab: text, and a thin accent underline when chosen.
fn tab(ui: &mut egui::Ui, p: &Palette, text: &str, selected: bool) -> egui::Response {
    let font = FontId::new(13.5, if selected { semibold() } else { medium() });
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, p.text);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x, 34.0), Sense::click());
    let color = if selected || resp.hovered() { p.text } else { p.text2 };
    ui.painter().galley(Pos2::new(rect.left(), rect.top() + 6.0), galley, color);
    if selected {
        let bar = Rect::from_min_max(Pos2::new(rect.left() - 2.0, rect.bottom() - 2.0), Pos2::new(rect.right() + 2.0, rect.bottom() + 0.5));
        ui.painter().rect_filled(bar, 1.5, p.accent);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A rounded pill with a hairline; tinted when selected (settings, examples).
pub(crate) fn chip(ui: &mut egui::Ui, p: &Palette, text: &str, selected: bool) -> egui::Response {
    let font = FontId::new(13.0, medium());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, p.text2);
    let size = Vec2::new(galley.size().x + 24.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let (fill, stroke, color) = if selected {
        (p.accent_soft, Stroke::new(1.0, theme::with_alpha(p.accent, 0.55)), p.accent)
    } else if resp.hovered() {
        (p.hover, Stroke::new(1.0, p.border), p.text)
    } else {
        (p.surface, Stroke::new(1.0, p.border), p.text2)
    };
    painter.rect(rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "Ctrl+Shift+Space" drawn as small key caps with a hairline.
fn key_hint(ui: &mut egui::Ui, p: &Palette, keys: &str) {
    for part in keys.split('+').rev() {
        let galley = ui.painter().layout_no_wrap(part.to_string(), FontId::new(11.0, medium()), p.text3);
        let size = Vec2::new(galley.size().x + 12.0, 21.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter().rect(rect, 5.0, p.bg, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        // a slightly heavier bottom edge, like a real key
        ui.painter().hline(rect.x_range().shrink(3.0), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text3);
        ui.add_space(3.0);
    }
}

/// Centered icon, title, text and optional example chips (returns the clicked one).
fn empty_state(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    tint: Color32,
    title: &str,
    sub: &str,
    examples: &[&'static str],
) -> Option<&'static str> {
    let mut picked = None;
    ui.add_space((ui.available_height() * 0.18).min(90.0));
    ui.vertical_centered(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 30.0, theme::with_alpha(tint, 0.07));
        ui.painter().circle_stroke(r.center(), 30.0, Stroke::new(1.0, theme::with_alpha(tint, 0.25)));
        icons::paint(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(28.0)), icon, tint);
        ui.add_space(14.0);
        ui.label(egui::RichText::new(title).size(19.0).family(FontFamily::Name("display".into())).color(p.text));
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

/// One result: Explorer's icon, name over its folder, date and size.
#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &egui::Ui,
    p: &Palette,
    rect: Rect,
    row: &Row,
    icon: Option<&egui::TextureHandle>,
    selected: bool,
    hovered: bool,
    tz: i32,
    now: i64,
    needles: &[String],
) {
    let painter = ui.painter_at(rect);
    let inner = rect.shrink2(Vec2::new(2.0, 2.0));
    if selected {
        painter.rect_filled(inner, 10.0, p.accent_soft);
    } else if hovered {
        painter.rect_filled(inner, 10.0, p.hover);
    }

    let (folder, name) = match row.path.rfind('\\') {
        Some(i) if i + 1 < row.path.len() => (&row.path[..i], &row.path[i + 1..]),
        _ => ("", row.path.as_str()),
    };
    let tile = Rect::from_center_size(Pos2::new(inner.left() + 26.0, inner.center().y), Vec2::splat(32.0));
    match icon {
        Some(t) => {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(t.id(), tile, uv, Color32::WHITE);
        }
        None => paint_tile(&painter, p, tile.shrink(1.0), row.is_dir, name),
    }

    let right_w = 210.0;
    let text_left = tile.right() + 14.0;
    let text_w = (inner.right() - right_w - text_left).max(80.0);
    let name_job = highlighted(name, needles, FontId::new(14.5, medium()), p.text, p.accent, text_w);
    let name_g = ui.fonts_mut(|f| f.layout_job(name_job));
    painter.galley(Pos2::new(text_left, inner.top() + 7.0), name_g, p.text);
    let folder_text = middle_elide(ui, folder, FontId::proportional(12.0), text_w);
    painter.text(Pos2::new(text_left, inner.top() + 28.0), Align2::LEFT_TOP, folder_text, FontId::proportional(12.0), p.text3);

    let size = if row.is_dir { String::new() } else { shell::size(row.size) };
    painter.text(Pos2::new(inner.right() - 14.0, inner.center().y), Align2::RIGHT_CENTER, size, FontId::proportional(12.0), p.text3);
    painter.text(
        Pos2::new(inner.right() - 92.0, inner.center().y),
        Align2::RIGHT_CENTER,
        relative_time(row.modified, tz, now),
        FontId::proportional(12.0),
        p.text2,
    );
}

/// Folder glyph, or a coloured tile with the extension (PDF, DOCX, JPG…).
pub(crate) fn paint_tile(painter: &egui::Painter, p: &Palette, tile: Rect, is_dir: bool, name: &str) {
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
        .map(|w| w.strip_prefix("content:").or_else(|| w.strip_prefix("c:")).unwrap_or(w))
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
pub(crate) fn match_mask(name: &str, needles: &[String]) -> Vec<bool> {
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

pub(crate) fn now_minutes() -> i64 {
    let unix =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    unix as i64 + 134_774 * 1440
}

/// "today, 14:32", "yesterday, 09:10", "Thu, 18:05", "Oct 12", "Mar 3, 2024"
/// — or the Romanian "azi, 14:32", "ieri, 09:10", "joi, 18:05", "12 oct.", "3 mar. 2024".
pub(crate) fn relative_time(minutes_utc: u32, tz: i32, now_utc: i64) -> String {
    if minutes_utc == 0 {
        return String::new();
    }
    const MONTHS_RO: [&str; 12] =
        ["ian.", "feb.", "mar.", "apr.", "mai", "iun.", "iul.", "aug.", "sept.", "oct.", "nov.", "dec."];
    const DAYS_RO: [&str; 7] = ["luni", "marți", "miercuri", "joi", "vineri", "sâmbătă", "duminică"];
    const MONTHS_EN: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const DAYS_EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let ro = crate::i18n::ro();
    let (months, days) = if ro { (MONTHS_RO, DAYS_RO) } else { (MONTHS_EN, DAYS_EN) };
    let local = minutes_utc as i64 + tz as i64;
    let today = (now_utc + tz as i64).div_euclid(1440);
    let day = local.div_euclid(1440);
    let hm = format!("{:02}:{:02}", local.rem_euclid(1440) / 60, local.rem_euclid(1440) % 60);
    let (y, m, d) = whereti_query::civil_from_day(day);
    let (ty, _, _) = whereti_query::civil_from_day(today);
    match today - day {
        0 => trf!("today, {hm}", "azi, {hm}"),
        1 => trf!("yesterday, {hm}", "ieri, {hm}"),
        2..=6 => format!("{}, {hm}", days[day.rem_euclid(7) as usize]), // day 0 (1601-01-01) was a Monday
        _ if y == ty && ro => format!("{d} {}", months[(m - 1) as usize]),
        _ if y == ty => format!("{} {d}", months[(m - 1) as usize]),
        _ if ro => format!("{d} {} {y}", months[(m - 1) as usize]),
        _ => format!("{} {d}, {y}", months[(m - 1) as usize]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_default() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::English);
        assert_eq!(time_left(Duration::from_secs(61)), "~2 min left");
        assert_eq!(shell::count(2_509_598), "2,509,598");
        assert_eq!(shell::size(12_595), "12.3 KB");
        let now = (whereti_query::day_from_civil(2026, 10, 24) * 1440 + 720) as i64;
        let at = |y, m, d, h: i64| (whereti_query::day_from_civil(y, m, d) * 1440 + h * 60) as u32;
        assert_eq!(relative_time(at(2026, 10, 24, 11), 180, now), "today, 14:00");
        assert_eq!(relative_time(at(2026, 10, 22, 6), 180, now), "Thu, 09:00");
        assert_eq!(relative_time(at(2026, 3, 12, 6), 180, now), "Mar 12");
        assert_eq!(relative_time(at(2024, 9, 3, 6), 180, now), "Sep 3, 2024");
    }


    #[test]
    fn time_left_and_progress_text() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::Romanian);
        assert_eq!(time_left(Duration::from_secs(3)), "~5 s rămase");
        assert_eq!(time_left(Duration::from_secs(42)), "~45 s rămase");
        assert_eq!(time_left(Duration::from_secs(61)), "~2 min rămase");
        assert_eq!(time_left(Duration::from_secs(7800)), "~2 h 10 min rămase");
        let c = whereti_ipc::ContentStatus { state: "indexing".into(), done: 120, queued: 236, ..Default::default() };
        assert_eq!(content_progress(&c, None), "120 din 356 documente (33%)");
        assert_eq!(content_progress(&c, Some(Duration::from_secs(150))), "120 din 356 documente (33%) · ~3 min rămase");
        // no estimate before there is enough to measure
        let mut eta = Eta::default();
        eta.sample(&c);
        assert_eq!(eta.left(&c), None);
    }

    #[test]
    fn relative_times_in_romanian() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::Romanian);
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
