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
use crate::{trf, trn};
use crate::icons::{self, Icon};
use crate::preview::Preview;
use crate::searcher::{Job, Searcher, Update};
use crate::settings::SettingsState;
use crate::shell;
use crate::theme::{self, Palette};
use crate::tray::{Requests, Tray, PREFERRED_HOTKEY};

const ROW_HEIGHT: f32 = 52.0;
/// Content results have a third line: the snippet.
const CONTENT_ROW_HEIGHT: f32 = 72.0;
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
            Filter::All => tr("All"),
            Filter::Documents => tr("Documents"),
            Filter::Images => tr("Images"),
            Filter::Audio => tr("Audio"),
            Filter::Video => tr("Video"),
            Filter::Folders => tr("Folders"),
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
    /// First start: what whereti is doing while the index is built.
    Welcome,
}

/// Chinese, Japanese or Korean letters: the fonts for them are loaded then.
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| matches!(c as u32, 0x1100..=0x11FF | 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF))
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
    /// `rows[close_start..]` (up to `content_start`) are close matches: a
    /// word of the query one letter off.
    close_start: Option<usize>,
    /// `dupe:`: where each set of identical files starts in `rows`, with
    /// its header.
    group_labels: std::collections::HashMap<usize, String>,
    /// The first set's header, shown as the list's own header.
    first_group_label: Option<String>,
    /// The rename box (F2), while it is open.
    renaming: Option<Renaming>,
    /// The Up arrow brought back this past search (index in history::list).
    history_pos: Option<usize>,
    /// Called from an Open/Save window: Enter moves it to the result's folder.
    file_dialog: Option<crate::win::FileDialog>,
    /// The rename box just closed: focus the search field next frame (while
    /// the box is still drawn, egui keeps focus inside it).
    refocus_after_rename: bool,
    /// Content result -> the other files with exactly the same content.
    copies: std::collections::HashMap<String, Vec<String>>,
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
    pub(crate) hwnd: Option<windows::Win32::Foundation::HWND>,
    pub(crate) page: Page,
    pub(crate) settings: SettingsState,
    pub(crate) requests: Arc<Requests>,
    pub(crate) tray: Option<Tray>,
    preview: Preview,
    /// Explorer's own file icons, cached per type.
    pub(crate) file_icons: FileIcons,
    /// Picture thumbnails for the Images grid.
    thumbs: crate::thumbs::Thumbs,
    /// Tiles per line in the Images grid, as last drawn (for the arrow keys).
    grid_cols: usize,
    /// Text excerpts under content results.
    snippets: crate::snippets::Snippets,
    /// The CJK fonts are loaded (see `setup_fonts`).
    cjk_fonts: bool,
    /// The once-a-day update check (update.rs).
    pub(crate) updater: crate::update::Updater,
    /// Apps, Settings pages and Control Panel items (launch.rs).
    launch: crate::launch::Launch,
    last_launch_refresh: Instant,
    /// Shown above the files: `rows[..launch_items.len()]` are these.
    launch_items: Vec<crate::launch::Item>,
    /// Matches for the search in flight, shown when its results arrive.
    pending_launch: Vec<crate::launch::Item>,
    /// "Search the documents not indexed yet", for the current search.
    scan: Option<Scan>,
    /// Confetti after a supporter license is activated: when it started.
    confetti: Option<Instant>,
    /// The "?" panel of search commands is open (F1).
    help_open: bool,
    /// Where the search field is, to place the "?" panel under it.
    search_rect: Rect,
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
            close_start: None,
            group_labels: std::collections::HashMap::new(),
            first_group_label: None,
            renaming: None,
            history_pos: None,
            file_dialog: None,
            refocus_after_rename: false,
            copies: Default::default(),
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
            thumbs: crate::thumbs::Thumbs::new(ctx.clone()),
            grid_cols: 1,
            cjk_fonts: false,
            updater: crate::update::Updater::new(),
            launch: crate::launch::Launch::new(ctx),
            last_launch_refresh: Instant::now(),
            launch_items: Vec::new(),
            pending_launch: Vec::new(),
            scan: None,
            confetti: None,
            help_open: false,
            search_rect: Rect::NOTHING,
            snippets: {
                let wake_ctx = ctx.clone();
                crate::snippets::Snippets::start(move || wake_ctx.request_repaint())
            },
        };
        app.settings.apply_theme(ctx);
        // First start (or right after the installer): the welcome page, and
        // whereti starts with Windows unless the user turns it off later.
        if !app.settings.welcomed || std::env::args().any(|a| a == "--welcome") {
            app.page = Page::Welcome;
            if !app.settings.welcomed && !app.settings.autostart && shell::set_autostart(true).is_ok() {
                app.settings.autostart = true;
            }
        }
        match app.tray.as_ref().map(|t| t.hotkey.clone()) {
            // Said once: the shortcut actually used is then remembered.
            Some(Some(k)) if k != PREFERRED_HOTKEY && app.settings.hotkey.is_none() => {
                app.flash(trf!("{PREFERRED_HOTKEY} is used by another app, so the shortcut is {k}", PREFERRED_HOTKEY, k));
                app.settings.hotkey = Some(k.to_string());
                app.settings.save();
            }
            Some(None) => app.flash(tr("No shortcut is free; open whereti from its tray icon")),
            _ => {}
        }
        app.run_search();
        app
    }

    pub(crate) fn content_eta(&self, c: &whereti_ipc::ContentStatus) -> Option<Duration> {
        self.eta.left(c)
    }

    pub(crate) fn hotkey_label(&self) -> Option<String> {
        self.tray.as_ref().and_then(|t| t.hotkey.clone())
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
        self.pending_launch =
            if recent || self.filter != Filter::All { Vec::new() } else { self.launch.search(&self.query) };
        self.scan = None; // a new search: offer it again, from the start
        // dupe: compares files; their text is not the question
        let dupes = self.query.split_whitespace().any(|w| matches!(w.to_ascii_lowercase().as_str(), "dupe:" | "dupes:" | "duplicate:" | "duplicates:"));
        let content = !recent && !dupes && !highlight_needles(&self.query).is_empty();
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
        // apps installed meanwhile
        if self.last_launch_refresh.elapsed() > Duration::from_secs(10 * 60) {
            self.last_launch_refresh = Instant::now();
            self.launch.refresh();
        }
        self.run_search(); // fresh results every time the window appears
    }

    /// Over the recent files: the last searches, one click each. `true`
    /// when one was clicked (it is searched; the list is drawn next frame).
    fn past_searches(&mut self, ui: &mut egui::Ui, p: &Palette) -> bool {
        let past = crate::history::list();
        if past.is_empty() {
            return false;
        }
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(tr("SEARCHES")).size(11.0).color(p.text3).family(semibold()));
        });
        ui.add_space(6.0);
        let (mut picked, mut removed) = (None, None);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.x = 6.0;
            for q in past.iter().take(8) {
                let shown: String = if q.chars().count() > 32 { q.chars().take(31).chain(['…']).collect() } else { q.clone() };
                // only whole chips: stop where the next would not fit
                let width = ui.painter().layout_no_wrap(shown.clone(), FontId::new(13.0, medium()), p.text2).size().x + 44.0;
                if width > ui.available_width() {
                    break;
                }
                match past_chip(ui, p, &shown, q) {
                    Some(true) => picked = Some(q.clone()),
                    Some(false) => removed = Some(q.clone()),
                    None => {}
                }
            }
        });
        ui.add_space(14.0);
        if let Some(q) = removed {
            crate::history::remove(&q);
            self.focus_search = true;
        }
        let Some(q) = picked else { return false };
        self.query = q;
        self.history_pos = None;
        self.run_search();
        self.focus_search = true;
        true
    }

    /// The search on screen was used: remember it (history.rs).
    fn remember_search(&self) {
        if !self.recent_mode && !self.rows.is_empty() {
            crate::history::record(&self.query);
        }
    }

    /// Up at the top of the list, in an empty box or on a search it brought
    /// back: the search before.
    fn recall_older(&mut self, ctx: &egui::Context) {
        let past = crate::history::list();
        let next = self.history_pos.map_or(0, |p| p + 1);
        let Some(q) = past.get(next) else { return };
        self.history_pos = Some(next);
        self.query = q.clone();
        self.run_search();
        // the cursor at the end, ready to add a word
        if let Some(id) = self.search_id {
            let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
            let end = egui::text::CCursor::new(self.query.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            state.store(ctx, id);
        }
        self.focus_search = true;
    }

    fn hide(&mut self, ctx: &egui::Context) {
        self.remember_search();
        self.file_dialog = None;
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
                    let launch = std::mem::take(&mut self.pending_launch);
                    let mut rows: Vec<Row> = launch
                        .iter()
                        .map(|it| Row { path: it.target.clone(), is_dir: false, size: 0, modified: 0, flags: 0 })
                        .collect();
                    let close = reply.close as usize;
                    // sets of identical files: a header over each
                    self.group_labels.clear();
                    let mut at = rows.len();
                    for (k, &n) in reply.groups.iter().enumerate() {
                        let n = n as usize;
                        let size = reply.rows.get(at - rows.len()).map_or(0, |r| r.size);
                        let label = trf!(
                            "{n} IDENTICAL FILES · {size} EACH · {free} TO FREE",
                            n = n,
                            size = shell::size(size),
                            free = shell::size(size * (n as u64 - 1))
                        );
                        self.group_labels.insert(at, if k == 0 { String::new() } else { label });
                        at += n;
                    }
                    let first_group = reply.groups.first().map(|&n| {
                        let size = reply.rows.first().map_or(0, |r| r.size);
                        trf!("{n} IDENTICAL FILES · {size} EACH · {free} TO FREE", n = n, size = shell::size(size), free = shell::size(size * (n as u64 - 1)))
                    });
                    self.first_group_label = first_group;
                    rows.extend(reply.rows);
                    self.close_start = (close > 0).then(|| rows.len() - close.min(rows.len()));
                    self.launch_items = launch;
                    self.rows = rows;
                    self.total = reply.total;
                    self.content_start = None;
                    self.copies.clear();
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
                    let mut copies = reply.copies.into_iter();
                    let mut extra = Vec::new();
                    for r in reply.rows {
                        let others = copies.next().unwrap_or_default();
                        if named.contains(&r.path) {
                            continue;
                        }
                        if !others.is_empty() {
                            self.copies.insert(r.path.clone(), others);
                        }
                        extra.push(r);
                    }
                    if !extra.is_empty() {
                        self.content_start = Some(self.rows.len());
                        self.rows.extend(extra);
                    }
                }
                Update::Scan { generation, reply } => {
                    let Some(scan) = self.scan.as_mut().filter(|s| s.generation == generation && s.running) else { continue };
                    scan.looked = reply.next;
                    scan.total = reply.total;
                    scan.found += reply.rows.len() as u64;
                    if reply.done {
                        scan.running = false;
                    } else {
                        self.searcher.scan(crate::searcher::ScanJob {
                            generation,
                            query: scan.query.clone(),
                            skip: reply.next,
                        });
                    }
                    let known: std::collections::HashSet<String> = self.rows.iter().map(|r| r.path.clone()).collect();
                    let extra: Vec<Row> = reply.rows.into_iter().filter(|r| !known.contains(&r.path)).collect();
                    if !extra.is_empty() {
                        if self.content_start.is_none() {
                            self.content_start = Some(self.rows.len());
                        }
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
                        let mut tip = trn!("whereti: {n} file · {state}|whereti: {n} files · {state}", s.entries, state = state_ro(&s.state));
                        if content_active(&s.content) {
                            tip.push_str(&trf!("\nContents: {progress}", progress = content_progress(&s.content, eta)));
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

    /// Row `i` is an app, a Settings page or a Control Panel item.
    fn is_launch(&self, i: usize) -> bool {
        i < self.launch_items.len()
    }

    fn open_selected(&mut self, ctx: &egui::Context, reveal: bool) {
        let Some(row) = self.rows.get(self.selected) else { return };
        let path = row.path.clone();
        let launch = self.is_launch(self.selected);
        // called from an Open/Save window: Enter takes that window to the folder
        if !launch && !reveal && self.file_dialog.is_some() {
            let folder = if row.is_dir { path.clone() } else { path.rsplit_once('\\').map_or(path.clone(), |(f, _)| f.to_string()) };
            let folder = if folder.ends_with(':') { format!("{folder}\\") } else { folder };
            let dialog = self.file_dialog.take();
            self.hide(ctx);
            if let Some(d) = dialog {
                if crate::win::jump(&d, &folder).is_err() {
                    self.show(ctx);
                    self.flash(tr("That window did not accept the folder (an app running as administrator does not take it from whereti)."));
                }
            }
            return;
        }
        let result = if launch {
            crate::launch::open(&path)
        } else if reveal {
            shell::reveal(&path)
        } else {
            shell::open(&path)
        };
        match result {
            Ok(()) => {
                if !launch {
                    crate::usage::record(&path);
                }
                self.hide(ctx)
            }
            Err(e) => self.flash(e),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Settings is listening for a new shortcut: every key goes there.
        if self.settings.recording {
            if self.page == Page::Settings && self.settings.section == crate::settings::Section::General {
                return;
            }
            crate::settings::stop_recording(self);
        }
        // the rename box takes the keys while it is open
        if self.renaming.is_some() {
            return;
        }
        let page = ((self.view_height / ROW_HEIGHT) as usize).max(1);
        let search_page = self.page == Page::Search;
        let f1 = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F1));
        if f1 {
            self.help_open = !self.help_open;
        }
        if self.help_open && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.help_open = false;
            return;
        }
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
        if self.page == Page::Welcome {
            if esc || enter {
                crate::welcome::finish(self);
            }
            return;
        }
        if tab {
            self.toggle_preview();
        }
        self.file_keys(ctx);
        let last = self.rows.len().saturating_sub(1);
        let before = self.selected;
        // the grid: a line is `step` tiles; Left/Right move one tile, but
        // only where the search box has no use for them (cursor at its start
        // or end)
        let step = if self.grid_view() { self.grid_cols.max(1) } else { 1 };
        if step > 1 {
            let range = self.search_id.and_then(|id| egui::text_edit::TextEditState::load(ctx, id)).and_then(|s| s.cursor.char_range());
            let n = self.query.chars().count();
            let at = |pos: usize| range.is_none_or(|r| r.is_empty() && r.primary.index.0 == pos);
            let (left, right) = ctx.input_mut(|i| {
                (at(0) && i.consume_key(Modifiers::NONE, Key::ArrowLeft), at(n) && i.consume_key(Modifiers::NONE, Key::ArrowRight))
            });
            if right {
                self.selected = (self.selected + 1).min(last);
            }
            if left {
                self.selected = self.selected.saturating_sub(1);
            }
        }
        if down {
            self.selected = (self.selected + step).min(last);
        }
        if up {
            if self.selected < step && (self.query.trim().is_empty() || self.history_pos.is_some()) {
                self.recall_older(ctx);
            } else if self.selected >= step {
                self.selected -= step;
            }
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
        if menu && !self.is_launch(self.selected) {
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
        if copy && !self.is_launch(self.selected) {
            if let Some(row) = self.rows.get(self.selected) {
                ctx.copy_text(row.path.clone());
                self.flash(tr("Path copied"));
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

    /// F2 renames the selected file, Delete moves it to the Recycle Bin,
    /// Ctrl+C copies the file itself, as in Explorer. The search field keeps
    /// its own: Ctrl+C with text selected copies the text, Delete inside
    /// the text deletes a letter.
    fn file_keys(&mut self, ctx: &egui::Context) {
        let on_file = self.rows.get(self.selected).is_some() && !self.is_launch(self.selected);
        if !on_file {
            return;
        }
        let range = self.search_id.and_then(|id| egui::text_edit::TextEditState::load(ctx, id)).and_then(|s| s.cursor.char_range());
        let selected_text = range.is_some_and(|r| !r.is_empty());
        let at_end = range.is_none_or(|r| r.primary.index.0 >= self.query.chars().count());
        let (rename, delete, copy) = ctx.input_mut(|i| {
            let copy = !selected_text && i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            if copy {
                i.events.retain(|e| !matches!(e, egui::Event::Copy));
            }
            (
                i.consume_key(Modifiers::NONE, Key::F2),
                !selected_text && at_end && i.consume_key(Modifiers::NONE, Key::Delete),
                copy,
            )
        });
        let path = self.rows[self.selected].path.clone();
        if copy || rename || delete {
            self.remember_search();
        }
        if copy {
            match crate::win::copy_file(&path) {
                Ok(()) => self.flash(tr("Copied: paste it in a folder, an e-mail or a chat")),
                Err(e) => self.flash(e),
            }
        }
        if rename {
            let name = path.rsplit('\\').next().unwrap_or("").to_string();
            if !name.is_empty() && !name.ends_with(':') {
                self.renaming = Some(Renaming { path, name, fresh: true, error: None });
            }
        } else if delete {
            let Some(hwnd) = self.hwnd else { return };
            match crate::win::recycle(hwnd, &path) {
                Ok(true) => {
                    if let Some(i) = self.rows.iter().position(|r| r.path == path) {
                        self.remove_row(i);
                    }
                    self.flash(tr("Moved to the Recycle Bin"));
                }
                Ok(false) => {}
                Err(e) => self.flash(e),
            }
            self.focus_search = true;
        }
    }

    /// Takes row `i` off the list (a file just deleted), keeping the
    /// sections and the count right.
    fn remove_row(&mut self, i: usize) {
        let named_end = self.close_start.or(self.content_start).unwrap_or(self.rows.len());
        if i >= self.launch_items.len() && i < named_end {
            self.total = self.total.saturating_sub(1);
        }
        let gone = self.rows.remove(i);
        self.thumbs.forget(&gone.path);
        for s in [&mut self.close_start, &mut self.content_start].into_iter().flatten() {
            if *s > i {
                *s -= 1;
            }
        }
        // a section left empty goes away with its header
        if self.content_start == Some(self.rows.len()) {
            self.content_start = None;
        }
        if self.close_start.is_some() && self.close_start == Some(self.content_start.unwrap_or(self.rows.len())) {
            self.close_start = None;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    /// The rename box (F2): the name, with the part before the extension
    /// selected, as in Explorer. Enter renames, Esc or a click outside cancels.
    fn rename_box(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(r) = self.renaming.as_mut() else {
            if std::mem::take(&mut self.refocus_after_rename) {
                self.focus_search = true;
            }
            return;
        };
        let id = egui::Id::unique("rename-field");
        let (mut apply, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::unique("rename"))
            .frame(
                Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18)),
            )
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(egui::RichText::new(tr("Rename")).size(15.0).family(semibold()).color(p.text));
                ui.add_space(10.0);
                let field = egui::TextEdit::singleline(&mut r.name)
                    .id(id)
                    .desired_width(f32::INFINITY)
                    .font(FontId::proportional(14.0))
                    .margin(Margin::symmetric(8, 6));
                let resp = ui.add(field);
                // Just opened: focus the field with the name before the
                // extension selected. The modal's first frame only measures
                // it (focus is dropped there), so ask until it holds.
                if r.fresh {
                    if resp.has_focus() {
                        r.fresh = false;
                    } else {
                        resp.request_focus();
                        let is_dir = std::path::Path::new(&r.path).is_dir();
                        let stem = match r.name.rfind('.') {
                            Some(dot) if dot > 0 && !is_dir => r.name[..dot].chars().count(),
                            _ => r.name.chars().count(),
                        };
                        let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(stem),
                        )));
                        state.store(ui.ctx(), id);
                        ui.ctx().request_repaint();
                    }
                }
                if resp.changed() {
                    r.error = None;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    apply = true;
                }
                if let Some(e) = &r.error {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(e).size(12.5).color(p.warn));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if crate::settings::button(ui, p, tr("Rename"), true).clicked() {
                        apply = true;
                    }
                    if crate::settings::button(ui, p, tr("Cancel"), false).clicked() {
                        cancel = true;
                    }
                });
            });
        if modal.should_close() {
            cancel = true;
        }
        if apply {
            self.apply_rename();
        } else if cancel {
            self.renaming = None;
        }
        if self.renaming.is_none() {
            self.refocus_after_rename = true;
            ctx.request_repaint();
        }
    }

    fn apply_rename(&mut self) {
        let Some(r) = self.renaming.as_mut() else { return };
        let name = r.name.trim().to_string();
        let old = r.path.rsplit('\\').next().unwrap_or("");
        if name.is_empty() || name == old {
            self.renaming = None;
            return;
        }
        if name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            r.error = Some(tr("A name cannot contain \\ / : * ? \" < > or a vertical bar").to_string());
            return;
        }
        let parent = r.path.rsplit_once('\\').map_or("", |(p, _)| p);
        let new_path = format!("{parent}\\{name}");
        // same name in other letters (raport -> Raport) is a rename, not a clash
        if !name.eq_ignore_ascii_case(old) && std::path::Path::new(&new_path).exists() {
            r.error = Some(tr("There is already a file or folder with this name here.").to_string());
            return;
        }
        let Some(hwnd) = self.hwnd else { return };
        let path = r.path.clone();
        match crate::win::rename(hwnd, &path, &name) {
            Ok(true) => {
                self.renaming = None;
                if let Some(row) = self.rows.iter_mut().find(|row| row.path == path) {
                    row.path = new_path;
                }
                self.flash(tr("Renamed"));
            }
            Ok(false) => self.renaming = None,
            Err(e) => r.error = Some(e),
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
                    .hint_text(egui::RichText::new(tr("Search files, folders, documents…")).size(18.0).color(p.text3))
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
                    self.history_pos = None;
                    if self.page == Page::Welcome {
                        crate::welcome::finish(self);
                    }
                    self.page = Page::Search;
                    self.run_search();
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let gear =
                        icon_button(ui, p, IconKind::Gear, self.page == Page::Settings).on_hover_text(tr("Settings (Ctrl+,)"));
                    if gear.clicked() {
                        self.toggle_settings();
                    }
                    if self.page == Page::Search {
                        let tip = if self.preview.open {
                            tr("Hide the preview (Tab)")
                        } else {
                            tr("Show the preview (Tab)")
                        };
                        if icon_button(ui, p, IconKind::Preview, self.preview.open).on_hover_text(tip).clicked() {
                            self.toggle_preview();
                        }
                    }
                    // a hairline between the buttons and the field's own controls
                    let (sep, _) = ui.allocate_exact_size(Vec2::new(13.0, 20.0), Sense::hover());
                    ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, p.border));
                    if !self.query.is_empty() {
                        if icon_button(ui, p, IconKind::Close, false).on_hover_text(tr("Clear (Esc)")).clicked() {
                            self.query.clear();
                            self.run_search();
                            self.focus_search = true;
                        }
                    } else if let Some(k) = self.hotkey_label() {
                        key_hint(ui, p, &k);
                    }
                    let help = icon_button(ui, p, IconKind::Help, self.help_open).on_hover_text(tr("Search commands (F1)"));
                    if help.clicked() {
                        self.help_open = !self.help_open;
                        // the commands are for searching: show them over the results
                        self.page = Page::Search;
                    }
                });
            });
        });
        self.search_rect = out.response.rect;
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
            // on the right: an invitation to support whereti, or a thank-you
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                // a supporter: quiet grey text, only the heart in amber
                let (text, color, heart, beat) = match &self.settings.license {
                    Some(l) => (trf!("Supporter: {name}", name = l.name), p.text3, supporter_gold(p), 1.0),
                    // not a supporter yet: a red heart that beats now and then
                    None => (tr("Support whereti").to_string(), p.accent, heart_red(p), self.heartbeat(ui.ctx())),
                };
                if support_link(ui, p, &text, color, heart, beat).clicked() {
                    self.settings.section = crate::settings::Section::Support;
                    self.page = Page::Settings;
                }
            });
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
                tr("The whereti service is not running"),
                &trf!("Start it from an Administrator terminal: whereti-service start   ({msg})", msg),
                &[],
            );
            return;
        }
        self.update_banner(ui, ctx, p);
        self.scan_strip(ui, p);
        if self.rows.is_empty() && self.content_pending {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(p.text3));
                ui.add_space(6.0);
                ui.label(egui::RichText::new(tr("Looking inside files too…")).size(13.0).color(p.text3));
            });
            return;
        }
        // Searching before the index is built: say so instead of "nothing found".
        let building = self.status.as_ref().filter(|s| s.state != "ready");
        if self.rows.is_empty() && building.is_some() {
            let s = building.cloned().unwrap_or_default();
            empty_state(
                ui,
                p,
                Icon::Clock,
                p.accent,
                tr("The index is still being built"),
                &trf!("{state}. Results appear as soon as it is ready, usually within a minute.", state = state_ro(&s.state)),
                &[],
            );
            return;
        }
        if self.rows.is_empty() {
            let (title, sub) = if self.query.trim().is_empty() && self.filter == Filter::All {
(
                    tr("Nothing opened recently"),
                    tr("Files you open and download show up here.").to_string(),
                )
            } else {
                (
                    tr("Nothing found"),
                    trf!("No file has “{query}” in its name or text. Try:", query = self.query.trim()),
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
        if let Some(d) = &self.file_dialog {
            let title = if d.title.trim().is_empty() { tr("Open / Save").to_string() } else { d.title.clone() };
            let text = trf!("Enter takes “{window}” to the folder of the result.", window = title);
            Frame::NONE
                .fill(p.accent_soft)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new(text).size(13.0).color(p.accent));
                });
            ui.add_space(8.0);
        }
        if self.recent_mode && self.past_searches(ui, p) {
            return;
        }
        let launch_n = self.launch_items.len();
        let header = if self.recent_mode {
            tr("RECENT").to_string()
        } else if launch_n > 0 {
            tr("APPS AND SETTINGS").to_string()
        } else if let Some(first) = &self.first_group_label {
            first.clone()
        } else if self.close_start == Some(0) {
            tr("CLOSE MATCHES").to_string()
        } else if self.content_start == Some(0) {
            tr("NO NAME MATCHES").to_string()
        } else {
            trn!("{n} RESULT|{n} RESULTS", self.total)
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

        if self.grid_view() {
            self.results_grid(ui, ctx, p);
            return;
        }

        // The list: name results, the section header, content results (taller,
        // with a snippet). Heights differ, so the list is virtualised here:
        // only the items in view are laid out and painted.
        // Items: apps and settings, then the files' header, the files, then
        // "found in contents" and those files.
        let content_start = self.content_start;
        let close_start = self.close_start;
        let section = self.content_section_label();
        let mut labels: Vec<String> = Vec::new();
        let mut list: Vec<Item> = Vec::with_capacity(self.rows.len() + 2);
        let mut item_of_row = Vec::with_capacity(self.rows.len());
        for i in 0..self.rows.len() {
            if launch_n > 0 && i == launch_n && content_start != Some(i) && close_start != Some(i) {
                let files = if self.total == 0 { tr("FILES").to_string() } else { trn!("{n} RESULT|{n} RESULTS", self.total) };
                labels.push(files);
                list.push(Item::Section(labels.len() - 1));
            }
            if let Some(label) = self.group_labels.get(&i).filter(|l| !l.is_empty()) {
                labels.push(label.clone());
                list.push(Item::Section(labels.len() - 1));
            }
            if close_start == Some(i) && i > 0 {
                labels.push(tr("CLOSE MATCHES").to_string());
                list.push(Item::Section(labels.len() - 1));
            }
            if content_start == Some(i) {
                labels.push(section.clone());
                list.push(Item::Section(labels.len() - 1));
            }
            item_of_row.push(list.len());
            list.push(Item::Row(i));
        }
        let items = list.len();
        let height_of = |item: usize| match list[item] {
            Item::Section(_) => ROW_HEIGHT,
            Item::Row(i) if content_start.is_some_and(|s| i >= s) => CONTENT_ROW_HEIGHT,
            Item::Row(_) => ROW_HEIGHT,
        };
        let mut tops = Vec::with_capacity(items + 1);
        let mut y = 0.0;
        for item in 0..items {
            tops.push(y);
            y += height_of(item);
        }
        tops.push(y);
        let total = y;
        let item_of = |row: usize| item_of_row.get(row).copied().unwrap_or(0);

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        // Keep the selected row visible after keyboard moves.
        // (the first frame does not know the view height yet: wait for it)
        if self.scroll_to_selected && self.view_height > 0.0 && !self.rows.is_empty() {
            let it = item_of(self.selected.min(self.rows.len() - 1));
            let (top, bottom) = (tops[it], tops[it + 1]);
            let mut off = self.scroll_offset;
            if top < off {
                off = top;
            } else if bottom > off + self.view_height {
                off = bottom - self.view_height;
            }
            area = area.vertical_scroll_offset(off.max(0.0));
            self.scroll_to_selected = false;
        }
        let mut clicked: Option<(usize, bool)> = None;
        let mut dragged: Option<usize> = None;
        let mut menu_for: Option<usize> = None;
        let needles = highlight_needles(&self.query);
        self.snippets.begin_frame(&needles);
        let rows = &self.rows;
        let launch_items = &self.launch_items;
        let file_icons = &mut self.file_icons;
        let snippets = &mut self.snippets;
        let copies = &self.copies;
        let selected = self.selected;
        let tz = self.tz;
        let now = now_minutes();
        let list_id = ui.scope_id().with("results");
        let out = area.show_viewport(ui, |ui, viewport| {
            let (whole, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total), Sense::hover());
            let first = tops.partition_point(|&t| t + 80.0 < viewport.min.y).saturating_sub(1);
            for (item, &top) in tops.iter().enumerate().take(items).skip(first) {
                if top > viewport.max.y {
                    break;
                }
                let rect = Rect::from_min_size(
                    Pos2::new(whole.left(), whole.top() + top),
                    Vec2::new(whole.width(), height_of(item)),
                );
                let i = match list[item] {
                    Item::Section(k) => {
                        paint_section(ui, p, rect, &labels[k]);
                        continue;
                    }
                    Item::Row(i) => i,
                };
                let row = &rows[i];
                if let Some(it) = launch_items.get(i) {
                    let resp = ui.interact(rect, list_id.with(i), Sense::click());
                    let kind = match it.kind {
                        crate::launch::Kind::App => tr("App"),
                        crate::launch::Kind::Setting => tr("Windows settings"),
                        crate::launch::Kind::ControlPanel => tr("Control Panel"),
                    };
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(egui::Role::ListBoxOption, true, i == selected, format!("{}, {kind}", it.name))
                    });
                    let icon = file_icons.get_shell(ui.ctx(), &it.target);
                    paint_launch_row(ui, p, rect, it, icon.as_ref(), i == selected, resp.hovered(), &needles);
                    if resp.double_clicked() {
                        clicked = Some((i, true));
                    } else if resp.clicked() {
                        clicked = Some((i, false));
                    }
                    continue;
                }
                let resp = ui.interact(rect, list_id.with(i), Sense::click_and_drag());
                // name, then folder: what a screen reader says for the row
                resp.widget_info(|| {
                    let (folder, name) = row.path.rsplit_once('\\').unwrap_or(("", row.path.as_str()));
                    egui::WidgetInfo::selected(egui::Role::ListBoxOption, true, i == selected, format!("{name}, {folder}"))
                });
                let icon = file_icons.get(ui.ctx(), &row.path, row.is_dir);
                if content_start.is_some_and(|s| i >= s) {
                    let line = snippets.get(&row.path);
                    let others = copies.get(&row.path).map_or(&[][..], |v| v.as_slice());
                    let hovered = resp.hovered();
                    paint_content_row(ui, p, rect, row, icon.as_ref(), &line, others, i == selected, hovered, tz, now, &needles);
                    if hovered && !others.is_empty() {
                        // where the other copies are
                        let folders: Vec<&str> = others.iter().map(|o| o.rsplit_once('\\').map_or(o.as_str(), |x| x.0)).collect();
                        let text = format!("{}\n{}", tr("The same file is also in:"), folders.join("\n"));
                        egui::Tooltip::for_widget(&resp).show(|ui| {
                            ui.label(text);
                        });
                    }
                } else {
                    paint_row(ui, p, rect, row, icon.as_ref(), i == selected, resp.hovered(), tz, now, &needles);
                }
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
        self.snippets.end_frame();
        self.scroll_offset = out.state.clamped_offset().y;
        self.view_height = out.inner_rect.height();
        self.after_list(ctx, clicked, dragged, menu_for);
    }

    /// A click, double click, drag or right click on result `i`.
    fn after_list(&mut self, ctx: &egui::Context, clicked: Option<(usize, bool)>, dragged: Option<usize>, menu_for: Option<usize>) {
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
            self.remember_search();
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

    /// Pictures as a grid of thumbnails: the Images filter.
    fn grid_view(&self) -> bool {
        self.page == Page::Search && self.filter == Filter::Images && self.launch_items.is_empty()
    }

    /// The Images grid: square-ish tiles with Windows' thumbnail and the
    /// name, virtualised by line like the list.
    fn results_grid(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) {
        const MIN_TILE: f32 = 156.0;
        const GAP: f32 = 10.0;
        const NAME_H: f32 = 30.0;
        self.thumbs.receive(ctx);
        let width = (ui.available_width() - 4.0).max(MIN_TILE);
        let cols = (((width + GAP) / (MIN_TILE + GAP)).floor() as usize).max(1);
        let tile_w = (width - GAP * (cols - 1) as f32) / cols as f32;
        let image_h = (tile_w * 0.75).round();
        let line_h = image_h + NAME_H + GAP;
        self.grid_cols = cols;
        let lines = self.rows.len().div_ceil(cols);
        let total = lines as f32 * line_h;

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if self.scroll_to_selected && self.view_height > 0.0 && !self.rows.is_empty() {
            let top = (self.selected.min(self.rows.len() - 1) / cols) as f32 * line_h;
            let mut off = self.scroll_offset;
            if top < off {
                off = top;
            } else if top + line_h > off + self.view_height {
                off = top + line_h - self.view_height;
            }
            area = area.vertical_scroll_offset(off.max(0.0));
            self.scroll_to_selected = false;
        }
        let (mut clicked, mut dragged, mut menu_for) = (None, None, None);
        let rows = &self.rows;
        let thumbs = &mut self.thumbs;
        let file_icons = &mut self.file_icons;
        let selected = self.selected;
        let grid_id = ui.scope_id().with("grid");
        let out = area.show_viewport(ui, |ui, viewport| {
            let (whole, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), total), Sense::hover());
            let first = (viewport.min.y / line_h).floor().max(0.0) as usize;
            let last = ((viewport.max.y / line_h).ceil() as usize).min(lines);
            for line in first..last {
                for col in 0..cols {
                    let i = line * cols + col;
                    let Some(row) = rows.get(i) else { break };
                    let tile = Rect::from_min_size(
                        Pos2::new(whole.left() + col as f32 * (tile_w + GAP), whole.top() + line as f32 * line_h),
                        Vec2::new(tile_w, image_h + NAME_H),
                    );
                    let resp = ui.interact(tile, grid_id.with(i), Sense::click_and_drag());
                    let name = row.path.rsplit('\\').next().unwrap_or(&row.path);
                    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::ListBoxOption, true, i == selected, name));
                    let painter = ui.painter_at(tile.expand(2.0));
                    let is_sel = i == selected;
                    let fill = if is_sel { p.accent_soft } else if resp.hovered() { p.hover } else { p.surface };
                    painter.rect_filled(tile, 10.0, fill);
                    if is_sel {
                        painter.rect_stroke(tile, 10.0, Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
                    }
                    let image_rect = Rect::from_min_size(tile.min, Vec2::new(tile_w, image_h)).shrink(6.0);
                    match thumbs.get(&row.path) {
                        Some(tex) => {
                            // fill the tile, cropping the middle (as Explorer's tiles)
                            let [tw, th] = tex.size().map(|v| v as f32);
                            let (iw, ih) = (image_rect.width(), image_rect.height());
                            let scale = (iw / tw).max(ih / th);
                            let (uw, uh) = ((iw / (tw * scale)).min(1.0), (ih / (th * scale)).min(1.0));
                            let uv = Rect::from_center_size(Pos2::new(0.5, 0.5), Vec2::new(uw, uh));
                            egui::Image::new((tex.id(), image_rect.size())).uv(uv).corner_radius(7.0).paint_at(ui, image_rect);
                        }
                        None => {
                            // until it comes: the file's icon
                            if let Some(icon) = file_icons.get(ui.ctx(), &row.path, row.is_dir) {
                                let r = Rect::from_center_size(image_rect.center(), Vec2::splat(40.0));
                                painter.image(icon.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                            }
                        }
                    }
                    let name_rect = Rect::from_min_max(
                        Pos2::new(tile.left() + 8.0, image_rect.bottom() + 4.0),
                        Pos2::new(tile.right() - 8.0, tile.bottom() - 4.0),
                    );
                    let mut job = egui::text::LayoutJob::simple_singleline(
                        name.to_string(),
                        FontId::proportional(12.5),
                        if is_sel { p.text } else { p.text2 },
                    );
                    job.wrap = egui::text::TextWrapping::truncate_at_width(name_rect.width());
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    painter.galley(Pos2::new(name_rect.left(), name_rect.center().y - galley.size().y / 2.0), galley, p.text2);
                    let resp = resp.on_hover_text(row.path.as_str());
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
            }
        });
        self.scroll_offset = out.state.clamped_offset().y;
        self.view_height = out.inner_rect.height();
        self.after_list(ctx, clicked, dragged, menu_for);
    }

    /// While document contents are still being indexed: an offer to look in
    /// the ones not read yet, then its progress, then what it found.
    fn scan_strip(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let words = !highlight_needles(&self.query).is_empty();
        let indexing = self.status.as_ref().map(|s| &s.content).filter(|c| content_active(c) && c.queued > 0);
        if self.recent_mode || !words || (indexing.is_none() && self.scan.is_none()) {
            return;
        }
        let (text, action): (String, Option<&str>) = match &self.scan {
            None => {
                let c = indexing.expect("indexing");
                let all = c.done + c.queued;
                let pct = (c.done * 100).checked_div(all).unwrap_or(0);
                (
                    trf!("Document contents are {pct}% indexed, so some may not be found yet.", pct),
                    Some(tr("Search them too")),
                )
            }
            Some(s) if s.running => (
                if s.total == 0 {
                    tr("Looking in the documents not indexed yet…").to_string()
                } else {
                    trf!("Looking in {looked} of {total} documents not indexed yet…", looked = shell::count(s.looked), total = shell::count(s.total))
                },
                Some(tr("Stop")),
            ),
            Some(s) => (
                crate::trn!(
                    "Looked in {n} document not indexed yet: {found} more found.|Looked in {n} documents not indexed yet: {found} more found.",
                    s.looked,
                    found = shell::count(s.found)
                ),
                None,
            ),
        };
        ui.add_space(10.0);
        let mut clicked = false;
        Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10.0)
            .inner_margin(Margin { left: 14, right: 8, top: 6, bottom: 6 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if self.scan.as_ref().is_some_and(|s| s.running) {
                        ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                        ui.add_space(4.0);
                    } else {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        crate::icons::paint(ui.painter(), r, Icon::Document, p.text3);
                        ui.add_space(4.0);
                    }
                    ui.label(egui::RichText::new(&text).size(12.5).color(p.text2));
                    if let Some(a) = action {
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let b = egui::Button::new(egui::RichText::new(a).size(12.5).family(semibold()).color(p.accent))
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.5)))
                                .corner_radius(8.0);
                            if ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                clicked = true;
                            }
                        });
                    }
                });
            });
        if clicked {
            match &mut self.scan {
                None => {
                    self.scan = Some(Scan { generation: self.generation, query: self.query.clone(), looked: 0, total: 0, found: 0, running: true });
                    self.searcher.scan(crate::searcher::ScanJob { generation: self.generation, query: self.query.clone(), skip: 0 });
                }
                Some(s) => s.running = false, // Stop: the next step is simply not asked for
            }
            self.focus_search = true;
        }
    }

    /// A new version, shown big above the results until it is installed or
    /// put off for a few days.
    fn update_banner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Palette) {
        use crate::update::Phase;
        if !self.updater.banner() {
            return;
        }
        let phase = self.updater.phase();
        let (title, text): (String, String) = match &phase {
            Phase::Offered(o) => (
                trf!("whereti {version} is ready", version = o.version),
                o.notes.lines().next().filter(|l| !l.trim().is_empty()).map(str::to_string).unwrap_or_else(|| {
                    tr("New features and fixes. It takes a minute, and your settings stay as they are.").to_string()
                }),
            ),
            Phase::Downloading { version, .. } => (
                trf!("Downloading whereti {version}…", version),
                tr("You can keep searching meanwhile.").to_string(),
            ),
            Phase::Installing => (
                tr("Installing the update").to_string(),
                tr("Windows asks for permission, then whereti opens again.").to_string(),
            ),
            Phase::Failed(e) => (tr("The update did not install").to_string(), e.clone()),
            Phase::Idle => return,
        };
        ui.add_space(12.0);
        Frame::new()
            .fill(p.accent_soft)
            .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.45)))
            .corner_radius(14.0)
            .inner_margin(Margin { left: 16, right: 14, top: 14, bottom: 14 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    // the badge
                    let (badge, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                    ui.painter().circle_filled(badge.center(), 20.0, p.accent);
                    crate::icons::paint(ui.painter(), badge.shrink(10.0), Icon::Sparkle, Color32::WHITE);
                    ui.add_space(12.0);
                    let buttons_w = 260.0;
                    let text_w = (ui.available_width() - buttons_w).max(160.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_w);
                        ui.label(egui::RichText::new(&title).size(15.5).family(semibold()).color(p.text));
                        ui.add_space(2.0);
                        ui.add(egui::Label::new(egui::RichText::new(&text).size(12.5).color(p.text2)).wrap());
                        if let Phase::Downloading { done, total, .. } = &phase {
                            ui.add_space(8.0);
                            let f = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                            ui.add(egui::ProgressBar::new(f).desired_height(6.0).fill(p.accent));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let primary = |ui: &mut egui::Ui, label: &str| {
                            ui.add(
                                egui::Button::new(egui::RichText::new(label).size(14.0).family(semibold()).color(p.on_accent))
                                    .fill(p.accent)
                                    .corner_radius(10.0)
                                    .min_size(Vec2::new(0.0, 36.0)),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                        };
                        let quiet = |ui: &mut egui::Ui, label: &str| {
                            ui.add(
                                egui::Button::new(egui::RichText::new(label).size(13.5).color(p.text2))
                                    .fill(Color32::TRANSPARENT)
                                    .stroke(Stroke::NONE)
                                    .min_size(Vec2::new(0.0, 36.0)),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                        };
                        match &phase {
                            Phase::Offered(_) => {
                                if primary(ui, tr("Install update")).clicked() {
                                    self.updater.install(ctx);
                                }
                                if quiet(ui, tr("Later")).clicked() {
                                    self.updater.later();
                                }
                            }
                            Phase::Failed(_) => {
                                if primary(ui, tr("Try again")).clicked() {
                                    self.updater.retry();
                                    self.updater.install(ctx);
                                }
                                if quiet(ui, tr("Later")).clicked() {
                                    self.updater.later();
                                }
                            }
                            Phase::Downloading { done, total, .. } if *total > 0 => {
                                ui.label(egui::RichText::new(format!("{} %", done * 100 / total)).size(13.0).color(p.text2));
                            }
                            _ => {
                                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
                            }
                        }
                    });
                });
            });
        ui.add_space(4.0);
    }

    /// "GĂSIT ÎN CONȚINUT · 12" (+ indexing progress while it runs).
    fn content_section_label(&self) -> String {
        let shown = self.content_start.map_or(0, |s| self.rows.len() - s) as u64;
        let mut label = trf!("FOUND IN CONTENTS · {count}", count = shell::count(shown));
        if let Some(c) = self.status.as_ref().map(|s| &s.content) {
            if content_active(c) {
                label.push_str(&trf!("   ·   indexing, {progress}", progress = content_progress(c, self.eta.left(c))));
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
            (Some(_), _) => (p.error, tr("the service is not running").to_string()),
            (None, Some(s)) if s.state == "ready" && content_active(&s.content) => (
                p.warn,
                trn!("{n} file · contents {progress}|{n} files · contents {progress}", s.entries, progress = content_progress(&s.content, self.eta.left(&s.content))),
            ),
            (None, Some(s)) if s.state == "ready" => (p.ok, trn!("{n} file indexed · up to date|{n} files indexed · up to date", s.entries)),
            (None, Some(s)) => (p.warn, trn!("{n} file · {state}|{n} files · {state}", s.entries, state = state_ro(&s.state))),
            (None, None) => (p.text3, tr("connecting…").to_string()),
        };
        let right_w = width(ui, &state) + 14.0 + 16.0;

        // Left: a message, or the longest set of hints that still fits.
        let room = rect.width() - right_w - 14.0 - 24.0;
        let mut left: Option<(String, Color32)> = None;
        let mut update_link = false;
        if let crate::update::Phase::Offered(o) = self.updater.phase() {
            if self.message.is_none() && self.page == Page::Search && !self.updater.banner() {
                left = Some((trf!("whereti {version} is available · Install", version = o.version), p.accent));
                update_link = true;
            }
        }
        if let Some((msg, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(8) {
                left = Some((msg.clone(), p.warn));
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
        }
        if left.is_none() && self.offline.is_none() && self.page == Page::Search {
            let ms = format!("{:.0} ms", self.round_trip.as_secs_f64() * 1000.0);
            let options = [
                trf!("{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer  ·  right-click for the menu  ·  drag into any app", ms),
                trf!("{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer  ·  right-click for the menu", ms),
                trf!("{ms}  ·  Enter opens  ·  Ctrl+Enter shows in Explorer", ms),
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
            let at = Pos2::new(rect.left() + 14.0, y - g.size().y / 2.0);
            let hit = egui::Rect::from_min_size(at, g.size()).expand(4.0);
            painter.galley(at, g, color);
            if update_link {
                let r = ui.interact(hit, ui.scope_id().with("update-link"), egui::Sense::click());
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, tr("Install update")));
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.settings.section = crate::settings::Section::About;
                    self.page = Page::Settings;
                }
            }
        }
        let painter = ui.painter();
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
            let from = self.requests.from.swap(0, std::sync::atomic::Ordering::SeqCst);
            if self.visible {
                self.hide(ctx);
            } else {
                self.show(ctx);
                self.file_dialog = crate::win::file_dialog(from);
            }
        }
        self.apply_updates();
        self.updater.tick(self.settings.updates, ctx);
        if !self.cjk_fonts && (has_cjk(&self.query) || self.rows.iter().any(|r| has_cjk(&r.path))) {
            self.cjk_fonts = true;
            crate::setup_fonts(ctx, true);
        }
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
        self.paint_confetti(ui.ctx());
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
        // the "?" panel floats above everything (placed under last frame's search field)
        self.help_panel(&ctx, &p);
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
        self.rename_box(&ctx, &p);
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
            let file = if self.is_launch(self.selected) { None } else { self.rows.get(self.selected) };
        self.preview.select(file);
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
                    let row = if self.is_launch(self.selected) { None } else { self.rows.get(self.selected) };
                    self.preview.show(ui, &p, row, &mut self.file_icons, &needles, self.tz)
                });
        }
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 0 }))
            .show(ui, |ui| match self.page {
                Page::Search => self.results(ui, &ctx, &p),
                Page::Settings => crate::settings::show(self, ui, &p),
                Page::Welcome => crate::welcome::show(self, ui, &p),
            });
    }
}

pub(crate) fn state_ro(state: &str) -> String {
    match state {
        "ready" => tr("up to date").into(),
        "loading" => tr("loading the index").into(),
        "indexing" => tr("building the index").into(),
        s if s.starts_with("indexing ") => {
            let drive = s.trim_start_matches("indexing ").split_whitespace().next().unwrap_or("");
            trf!("reading the file names on {drive}", drive)
        }
        "catching up" => tr("catching up").into(),
        "reindexing" => tr("rebuilding the index").into(),
        s if s.starts_with("rescanning ") => {
            let drives = s.trim_start_matches("rescanning ");
            trf!("checking {drives} again", drives)
        }
        s => s.to_string(),
    }
}

/// "1.234 din 5.678 documente (22%) · ~3 min rămase"
pub(crate) fn content_progress(c: &whereti_ipc::ContentStatus, left: Option<Duration>) -> String {
    let all = c.done + c.queued;
    let pct = (c.done * 100).checked_div(all).unwrap_or(100);
    let mut s = trn!("{done} of {n} document ({pct}%)|{done} of {n} documents ({pct}%)", all, done = shell::count(c.done), pct);
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
        trf!("~{secs} s left", secs = secs.max(5).div_ceil(5) * 5)
    } else if secs < 3600 {
        trf!("~{mins} min left", mins = secs.div_ceil(60))
    } else {
        trf!("~{hours} h {mins} min left", hours = secs / 3600, mins = (secs % 3600) / 60)
    }
}

/// How far indexing is, if something is being indexed (0.0–1.0).
fn indexing_fraction(s: &StatusReply) -> Option<f32> {
    if let Some(f) = names_progress(&s.state) {
        return Some(f);
    }
    if s.state == "reindexing" {
        return Some(0.0);
    }
    let c = &s.content;
    (content_active(c) && c.done + c.queued > 0).then(|| c.done as f32 / (c.done + c.queued) as f32)
}

/// "indexing C: 1/2" → how far the first build is (drives done / all).
pub(crate) fn names_progress(state: &str) -> Option<f32> {
    let rest = state.strip_prefix("indexing")?;
    let Some((k, n)) = rest.rsplit(' ').next().and_then(|t| t.split_once('/')) else { return Some(0.0) };
    let (k, n): (f32, f32) = (k.parse().ok()?, n.parse().ok()?);
    Some(((k - 1.0) / n.max(1.0)).clamp(0.0, 1.0))
}

/// Indexing, or paused in the middle of it.
pub(crate) fn content_active(c: &whereti_ipc::ContentStatus) -> bool {
    c.state == "indexing" || c.state.starts_with("paused")
}

/// "în pauză (pe baterie)" for a paused state, else empty.
pub(crate) fn pause_note(c: &whereti_ipc::ContentStatus) -> &'static str {
    match c.state.as_str() {
        "paused: battery" => tr(" · paused: on battery"),
        "paused: fullscreen" => tr(" · paused: a game or presentation is running"),
        "paused: busy" => tr(" · paused: the processor is busy"),
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
    Help,
}

/// A 32×32 icon button with a line icon; tinted when active.
pub(crate) fn icon_button(ui: &mut egui::Ui, p: &Palette, kind: IconKind, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    // what a screen reader says for it
    let name = match kind {
        IconKind::Gear => tr("Settings (Ctrl+,)"),
        IconKind::Close => tr("Clear (Esc)"),
        IconKind::Back => tr("Back (Esc)"),
        IconKind::Preview => tr("Preview"),
        IconKind::Help => tr("Search commands (F1)"),
    };
    let enabled = ui.is_enabled();
    match kind {
        IconKind::Preview | IconKind::Help => {
            resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, active, name))
        }
        _ => resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, name)),
    }
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
        IconKind::Help => Icon::Help,
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
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, text));
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
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, text));
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

/// A past search: its text, and a small × that forgets it. `Some(true)`:
/// search it again; `Some(false)`: the × was clicked.
fn past_chip(ui: &mut egui::Ui, p: &Palette, shown: &str, query: &str) -> Option<bool> {
    let font = FontId::new(13.0, medium());
    let galley = ui.painter().layout_no_wrap(shown.to_string(), font, p.text2);
    let size = Vec2::new(galley.size().x + 44.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, query));
    let x_rect = Rect::from_center_size(Pos2::new(rect.right() - 15.0, rect.center().y), Vec2::splat(18.0));
    let x_resp = ui.interact(x_rect, resp.id.with("x"), Sense::click());
    let x_label = trf!("Forget “{search}”", search = query);
    x_resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &x_label));
    let painter = ui.painter();
    let (fill, color) = if resp.hovered() || x_resp.hovered() { (p.hover, p.text) } else { (p.surface, p.text2) };
    painter.rect(rect, 8.0, fill, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.galley(Pos2::new(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0), galley, color);
    if x_resp.hovered() {
        painter.circle_filled(x_rect.center(), 9.0, theme::with_alpha(p.text3, 0.18));
    }
    icons::paint(painter, x_rect.shrink(4.0), Icon::Close, if x_resp.hovered() { p.text } else { p.text3 });
    if x_resp.on_hover_text(&x_label).clicked() {
        return Some(false);
    }
    resp.on_hover_text(query).on_hover_cursor(egui::CursorIcon::PointingHand).clicked().then_some(true)
}

/// "Ctrl+Shift+Space" drawn as small key caps with a hairline.
pub(crate) fn key_hint(ui: &mut egui::Ui, p: &Palette, keys: &str) {
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

/// A heart and a short text, clickable.
/// The supporter's amber, from the pin in the logo; lighter on dark themes.
pub(crate) fn supporter_gold(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_rgb(255, 196, 102)
    } else {
        Color32::from_rgb(201, 106, 18)
    }
}

/// The heart of "Support whereti": red, a little lighter on dark themes.
fn heart_red(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_rgb(255, 107, 107)
    } else {
        Color32::from_rgb(226, 52, 61)
    }
}

/// How large the heart is during a heartbeat: two quick beats ("lub-dub")
/// every `BEAT_EVERY`, the heart growing ~20%, then at rest.
fn heart_scale(t: f32) -> f32 {
    let bump = |start: f32, len: f32, height: f32| {
        if (start..start + len).contains(&t) {
            height * (std::f32::consts::PI * (t - start) / len).sin()
        } else {
            0.0
        }
    };
    1.0 + bump(0.0, 0.22, 0.22) + bump(0.30, 0.26, 0.14)
}

const BEAT_EVERY: f32 = 6.0;

fn support_link(ui: &mut egui::Ui, p: &Palette, text: &str, color: Color32, heart_color: Color32, beat: f32) -> egui::Response {
    let font = FontId::new(13.0, medium());
    let g = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = Vec2::new(g.size().x + 22.0, g.size().y.max(18.0) + 8.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, text));
    if resp.hovered() {
        ui.painter().rect_filled(rect.expand(2.0), 8.0, p.hover);
    }
    let heart = Rect::from_center_size(Pos2::new(rect.left() + 8.0, rect.center().y), Vec2::splat(13.0 * beat));
    crate::icons::paint(ui.painter(), heart, Icon::Heart, heart_color);
    ui.painter().galley(Pos2::new(rect.left() + 22.0, rect.center().y - g.size().y / 2.0), g, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

impl WheretiApp {
    /// The heart's size now, and the next repaint it needs. Still when
    /// Windows' animations are off; asks for frames only while the window
    /// is shown (hidden, the app must not wake up for it).
    fn heartbeat(&self, ctx: &egui::Context) -> f32 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        static ANIMATE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ANIMATE.get_or_init(animations_on) || !self.visible {
            return 1.0;
        }
        let t = START.get_or_init(Instant::now).elapsed().as_secs_f32() % BEAT_EVERY;
        if t < 0.6 {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(BEAT_EVERY - t));
        }
        heart_scale(t)
    }

    /// After a license is activated or removed: the logo everywhere, and a
    /// little confetti for a new supporter.
    pub(crate) fn supporter_changed(&mut self, ctx: &egui::Context, celebrate: bool) {
        crate::tray::set_supporter(self.settings.license.is_some());
        if let Some(t) = &self.tray {
            let _ = t.icon.set_icon(Some(crate::tray::icon_with_progress(self.tray_progress)));
        }
        let icon = egui::IconData { rgba: crate::tray::logo_rgba_64(), width: 64, height: 64 };
        ctx.send_viewport_cmd(ViewportCommand::Icon(Some(Arc::new(icon))));
        if celebrate && animations_on() {
            self.confetti = Some(Instant::now());
        }
    }

    /// The "?" panel under the search field: what can be typed, in three
    /// groups, each with an example that is typed and searched on a click.
    fn help_panel(&mut self, ctx: &egui::Context, p: &Palette) {
        // only over the search page: leaving it (Settings) closes the panel
        if self.page != Page::Search {
            self.help_open = false;
        }
        if !self.help_open || self.search_rect == Rect::NOTHING {
            return;
        }
        // examples with words are in the user's language; commands are not
        let groups: [(&str, Vec<(String, &str)>); 4] = [
            (
                tr("Words"),
                vec![
                    (tr("report 2025").to_string(), tr("Every word, anywhere in the name")),
                    (tr("\"annual report\"").to_string(), tr("These words together, exactly as typed")),
                    (format!("{} | {}", tr("invoice"), tr("receipt")), tr("One or the other")),
                    (tr("report !draft").to_string(), tr("Leave out names with this word")),
                    ("img_????.jpg".to_string(), tr("* stands for any letters, ? for one letter")),
                ],
            ),
            (
                tr("Filters"),
                vec![
                    ("ext:pdf;docx".to_string(), tr("Only these file types")),
                    ("dm:today".to_string(), tr("Changed on a day: also yesterday, thisweek, lastmonth, 2025, 2025-03")),
                    ("dm:2025-03-01..2025-06-30".to_string(), tr("Changed between two dates")),
                    ("size:>10mb".to_string(), tr("By size: also large, huge, 1mb..5mb")),
                    (tr("folder:projects").to_string(), tr("Only folders (file: only files)")),
                    (r"path:C:\Users".to_string(), tr("Only inside a folder")),
                    ("mine:".to_string(), tr("Only your own files, without programs and system files")),
                    ("dupe: size:>100mb".to_string(), tr("Identical files: the copies you could delete to free space")),
                ],
            ),
            (
                tr("Inside files and photos"),
                vec![
                    (tr("content:rent").to_string(), tr("Files that have the words inside")),
                    (tr("loc:italy").to_string(), tr("Photos taken in a town, region or country")),
                    (r"regex:^IMG_\d{4}".to_string(), tr("A regular expression, for experts")),
                ],
            ),
            (
                tr("Photos, music and documents"),
                vec![
                    ("taken:2025-08".to_string(), tr("Photos taken in a month, a year or between two dates")),
                    ("camera:iphone".to_string(), tr("Photos taken with this camera or phone")),
                    ("width:>4000".to_string(), tr("Pictures this wide (height: as well), in pixels")),
                    ("artist:queen".to_string(), tr("Songs by this artist (album: as well)")),
                    ("duration:>5m".to_string(), tr("Songs and videos this long: 90s, 5m, 1h")),
                    (tr("author:popescu").to_string(), tr("Word, Excel and PowerPoint files by this author")),
                ],
            ),
        ];
        let width = self.search_rect.width().min(660.0);
        let top = self.search_rect.left_bottom() + Vec2::new(0.0, 8.0);
        // always under the field: a small window scrolls instead of the
        // panel being pushed up over the field
        let max_height = (ctx.content_rect().bottom() - top.y - 16.0).max(160.0);
        let mut chosen: Option<String> = None;
        let area = egui::Area::new(egui::Id::unique("search-help"))
            .order(egui::Order::Foreground)
            .fixed_pos(top)
            .constrain(false)
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(12))
                    .shadow(egui::epaint::Shadow { offset: [0, 6], blur: 24, spread: 0, color: theme::with_alpha(Color32::BLACK, 0.16) })
                    .inner_margin(Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(width - 32.0);
                        ui.label(egui::RichText::new(tr("Search commands")).size(15.0).family(semibold()).color(p.text));
                        ui.label(egui::RichText::new(tr("Click an example to try it.")).size(12.5).color(p.text2));
                        egui::ScrollArea::vertical().max_height(max_height - 80.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (title, rows) in &groups {
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new(title.to_uppercase()).size(11.5).family(semibold()).color(p.text3));
                            ui.add_space(2.0);
                            egui::Grid::new(("help-group", *title)).num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                                for (example, what) in rows {
                                    let font = FontId::monospace(12.5);
                                    let galley = ui.painter().layout_no_wrap(example.clone(), font, p.accent);
                                    let size = galley.size() + Vec2::new(16.0, 6.0);
                                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(size.x.max(150.0), size.y), Sense::click());
                                    let name = example.clone();
                                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &name));
                                    let fill = if resp.hovered() { theme::with_alpha(p.accent, 0.16) } else { p.accent_soft };
                                    ui.painter().rect_filled(rect, 6.0, fill);
                                    ui.painter().galley(rect.left_top() + Vec2::new(8.0, 3.0), galley, p.accent);
                                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                        chosen = Some(example.clone());
                                    }
                                    ui.label(egui::RichText::new(*what).size(13.0).color(p.text2));
                                    ui.end_row();
                                }
                            });
                        }
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new(tr("Keys").to_uppercase()).size(11.5).family(semibold()).color(p.text3));
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(tr(
                                "Enter opens · Ctrl+Enter shows in Explorer · F2 renames · Delete moves to the Recycle Bin · Ctrl+C copies the file · Ctrl+Shift+C copies its path",
                            ))
                            .size(13.0)
                            .color(p.text2),
                        );
                        });
                    });
            });
        if let Some(q) = chosen {
            self.query = q;
            self.page = Page::Search;
            self.help_open = false;
            self.focus_search = true;
            self.run_search();
            return;
        }
        // a click anywhere else closes it (the search field and its "?" excepted)
        let clicked_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|pos| !area.response.rect.contains(pos) && !self.search_rect.contains(pos))
        });
        if clicked_outside {
            self.help_open = false;
        }
    }

    /// Confetti in the logo's colours, falling over the whole window.
    fn paint_confetti(&mut self, ctx: &egui::Context) {
        const LIFE: f32 = 2.6;
        let Some(start) = self.confetti else { return };
        let t = start.elapsed().as_secs_f32();
        if t > LIFE {
            self.confetti = None;
            return;
        }
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("confetti")));
        let colors = [
            Color32::from_rgb(47, 107, 255),
            Color32::from_rgb(74, 75, 240),
            Color32::from_rgb(108, 43, 217),
            Color32::from_rgb(255, 214, 107),
            Color32::from_rgb(255, 138, 61),
        ];
        for i in 0..110u32 {
            // a fixed pseudo-random shape per piece
            let h = |k: u32| {
                // splitmix64's finaliser: unrelated values for every (piece, k)
                let mut x = ((i as u64) << 32 | k as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
                x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                ((x ^ (x >> 31)) % 10_000) as f32 / 10_000.0
            };
            let delay = h(1) * 0.5;
            let lt = t - delay;
            if lt <= 0.0 {
                continue;
            }
            let x0 = screen.left() + screen.width() * (0.1 + 0.8 * h(2));
            let vx = (h(3) - 0.5) * 160.0;
            let vy = 60.0 + h(4) * 140.0;
            let x = x0 + vx * lt + (lt * 3.0 + h(5) * 6.0).sin() * 18.0;
            let y = screen.top() - 20.0 + vy * lt + 0.5 * 420.0 * lt * lt;
            let fade = (1.0 - (lt / (LIFE - delay)).powi(3)).clamp(0.0, 1.0);
            let color = theme::with_alpha(colors[(h(6) * colors.len() as f32) as usize % colors.len()], fade);
            let angle = h(7) * std::f32::consts::TAU + lt * (h(8) * 8.0 - 4.0);
            let (w, hgt) = (4.0 + h(9) * 4.0, 2.5 + h(10) * 2.5);
            let (s, c) = angle.sin_cos();
            let corner = |dx: f32, dy: f32| Pos2::new(x + dx * c - dy * s, y + dx * s + dy * c);
            painter.add(egui::Shape::convex_polygon(
                vec![corner(-w, -hgt), corner(w, -hgt), corner(w, hgt), corner(-w, hgt)],
                color,
                Stroke::NONE,
            ));
        }
        ctx.request_repaint();
    }
}

/// Windows' "Animation effects" setting (Accessibility, Visual effects).
fn animations_on() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
    let mut on: i32 = 1;
    // SAFETY: the out value is a BOOL, as SPI_GETCLIENTAREAANIMATION expects.
    unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on as *mut i32 as *mut _, 0) };
    on != 0
}

/// "Search the documents not indexed yet" for one search.
struct Scan {
    generation: u64,
    query: String,
    looked: u64,
    total: u64,
    found: u64,
    running: bool,
}

/// An item of the results list.
#[derive(Clone, Copy)]
enum Item {
    /// A header, by its index in the labels.
    Section(usize),
    Row(usize),
}

/// An app, a Settings page or a Control Panel item: its icon, its name, and
/// what it is.
#[allow(clippy::too_many_arguments)]
fn paint_launch_row(
    ui: &egui::Ui,
    p: &Palette,
    rect: Rect,
    it: &crate::launch::Item,
    icon: Option<&egui::TextureHandle>,
    selected: bool,
    hovered: bool,
    needles: &[String],
) {
    use crate::launch::Kind;
    let painter = ui.painter_at(rect);
    let inner = rect.shrink2(Vec2::new(2.0, 2.0));
    if selected {
        painter.rect_filled(inner, 10.0, p.accent_soft);
    } else if hovered {
        painter.rect_filled(inner, 10.0, p.hover);
    }
    let tile = Rect::from_center_size(Pos2::new(inner.left() + 26.0, inner.center().y), Vec2::splat(32.0));
    match icon {
        Some(t) => {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(t.id(), tile, uv, Color32::WHITE);
        }
        None => {
            painter.rect_filled(tile.shrink(3.0), 7.0, p.accent_soft);
            crate::icons::paint(&painter, tile.shrink(8.0), crate::icons::Icon::Settings, p.accent);
        }
    }
    let text_left = tile.right() + 14.0;
    let text_w = (inner.right() - 160.0 - text_left).max(80.0);
    let name_job = highlighted(&it.name, needles, FontId::new(14.5, medium()), p.text, p.accent, text_w);
    let name_g = ui.fonts_mut(|f| f.layout_job(name_job));
    painter.galley(Pos2::new(text_left, inner.center().y - name_g.size().y / 2.0), name_g, p.text);
    let kind = match it.kind {
        Kind::App => tr("App"),
        Kind::Setting => tr("Windows settings"),
        Kind::ControlPanel => tr("Control Panel"),
    };
    painter.text(Pos2::new(inner.right() - 14.0, inner.center().y), Align2::RIGHT_CENTER, kind, FontId::proportional(12.0), p.text3);
}

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

/// A content result: name, the snippet with the found words, the folder.
#[allow(clippy::too_many_arguments)]
fn paint_content_row(
    ui: &egui::Ui,
    p: &Palette,
    rect: Rect,
    row: &Row,
    icon: Option<&egui::TextureHandle>,
    line: &crate::snippets::Line,
    copies: &[String],
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
    let tile = Rect::from_center_size(Pos2::new(inner.left() + 26.0, inner.top() + 26.0), Vec2::splat(32.0));
    match icon {
        Some(t) => {
            painter.image(t.id(), tile, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        None => paint_tile(&painter, p, tile.shrink(1.0), row.is_dir, name),
    }
    let right_w = 210.0;
    let text_left = tile.right() + 14.0;
    let text_w = (inner.right() - right_w - text_left).max(80.0);
    let name_job = highlighted(name, needles, FontId::new(14.5, medium()), p.text, p.accent, text_w);
    let name_g = ui.fonts_mut(|f| f.layout_job(name_job));
    painter.galley(Pos2::new(text_left, inner.top() + 6.0), name_g, p.text);

    // the snippet runs under the name and the date column, to the right edge
    let snip_w = inner.right() - 14.0 - text_left;
    let snip_pos = Pos2::new(text_left, inner.top() + 27.0);
    match line {
        crate::snippets::Line::Excerpt(sn) => {
            let job = snippet_job(sn, p, snip_w);
            let g = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(snip_pos, g, p.text2);
        }
        crate::snippets::Line::NoExcerpt => {
            painter.text(
                snip_pos,
                Align2::LEFT_TOP,
                tr("The words are in this file"),
                FontId::proportional(12.5),
                p.text3,
            );
        }
        crate::snippets::Line::Loading => {
            // a quiet placeholder while the text is read
            let bar = Rect::from_min_size(snip_pos + Vec2::new(0.0, 5.0), Vec2::new((snip_w * 0.55).min(320.0), 7.0));
            painter.rect_filled(bar, 3.5, p.line);
        }
    }
    let folder_text = middle_elide(ui, folder, FontId::proportional(12.0), text_w);
    let folder_rect =
        painter.text(Pos2::new(text_left, inner.top() + 47.0), Align2::LEFT_TOP, folder_text, FontId::proportional(12.0), p.text3);
    if !copies.is_empty() {
        // the same file elsewhere: one result, with how many more
        painter.text(
            Pos2::new(folder_rect.right() + 10.0, inner.top() + 47.0),
            Align2::LEFT_TOP,
            crate::trn!("+{n} copy|+{n} copies", copies.len()),
            FontId::new(12.0, medium()),
            p.accent,
        );
    }

    let size = if row.is_dir { String::new() } else { shell::size(row.size) };
    let y = inner.top() + 16.0;
    painter.text(Pos2::new(inner.right() - 14.0, y), Align2::RIGHT_CENTER, size, FontId::proportional(12.0), p.text3);
    painter.text(
        Pos2::new(inner.right() - 92.0, y),
        Align2::RIGHT_CENTER,
        relative_time(row.modified, tz, now),
        FontId::proportional(12.0),
        p.text2,
    );
}

/// One line of snippet text; the found words in accent colour and medium weight.
fn snippet_job(sn: &crate::snippets::Snippet, p: &Palette, width: f32) -> LayoutJob {
    let chars: Vec<(usize, char)> = sn.text.char_indices().collect();
    let mut job = LayoutJob::default();
    let mut i = 0;
    while i < chars.len() {
        let m = sn.marks[i];
        let mut j = i;
        while j < chars.len() && sn.marks[j] == m {
            j += 1;
        }
        let end = chars.get(j).map_or(sn.text.len(), |c| c.0);
        let format = if m {
            TextFormat::simple(FontId::new(12.5, semibold()), p.accent)
        } else {
            TextFormat::simple(FontId::proportional(12.5), p.text2)
        };
        job.append(&sn.text[chars[i].0..end], 0.0, format);
        i = j;
    }
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    job
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
    let label: String = if ext.is_empty() { String::new() } else { ext.to_uppercase().chars().take(4).collect() };
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
        .map(|w| {
            ["content:", "c:", "loc:", "location:", "place:"].iter().find_map(|p| w.strip_prefix(p)).unwrap_or(w)
        })
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
/// - or the Romanian "azi, 14:32", "ieri, 09:10", "joi, 18:05", "12 oct.", "3 mar. 2024".
pub(crate) fn relative_time(minutes_utc: u32, tz: i32, now_utc: i64) -> String {
    if minutes_utc == 0 {
        return String::new();
    }
    let months = [
        tr("Jan"), tr("Feb"), tr("Mar"), tr("Apr"), tr("May"), tr("Jun"),
        tr("Jul"), tr("Aug"), tr("Sep"), tr("Oct"), tr("Nov"), tr("Dec"),
    ];
    let days = [tr("Mon"), tr("Tue"), tr("Wed"), tr("Thu"), tr("Fri"), tr("Sat"), tr("Sun")];
    let local = minutes_utc as i64 + tz as i64;
    let today = (now_utc + tz as i64).div_euclid(1440);
    let day = local.div_euclid(1440);
    let hm = format!("{:02}:{:02}", local.rem_euclid(1440) / 60, local.rem_euclid(1440) % 60);
    let (y, m, d) = whereti_query::civil_from_day(day);
    let (ty, _, _) = whereti_query::civil_from_day(today);
    match today - day {
        0 => trf!("today, {hm}", hm),
        1 => trf!("yesterday, {hm}", hm),
        // day 0 (1601-01-01) was a Monday
        2..=6 => trf!("{weekday}, {hm}", weekday = days[day.rem_euclid(7) as usize], hm),
        _ if y == ty => trf!("{month} {day}", month = months[(m - 1) as usize], day = d),
        _ => trf!("{month} {day}, {year}", month = months[(m - 1) as usize], day = d, year = y),
    }
}

/// The rename box of a file (F2).
struct Renaming {
    path: String,
    /// The name being typed.
    name: String,
    /// Just opened: select the name before the extension.
    fresh: bool,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_the_default() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::En);
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
        crate::i18n::set(crate::i18n::Lang::Ro);
        assert_eq!(time_left(Duration::from_secs(3)), "~5 s rămase");
        assert_eq!(time_left(Duration::from_secs(42)), "~45 s rămase");
        assert_eq!(time_left(Duration::from_secs(61)), "~2 min rămase");
        assert_eq!(time_left(Duration::from_secs(7800)), "~2 h 10 min rămase");
        let c = whereti_ipc::ContentStatus { state: "indexing".into(), done: 120, queued: 236, ..Default::default() };
        assert_eq!(content_progress(&c, None), "120 din 356 de documente (33%)");
        assert_eq!(content_progress(&c, Some(Duration::from_secs(150))), "120 din 356 de documente (33%) · ~3 min rămase");
        // no estimate before there is enough to measure
        let mut eta = Eta::default();
        eta.sample(&c);
        assert_eq!(eta.left(&c), None);
    }

    #[test]
    fn relative_times_in_romanian() {
        let _guard = crate::i18n::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Lang::Ro);
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
