//! The search window (PLAN.md §10.4–10.8): type, see results, open.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, FontFamily, FontId, Key, Modifiers, Pos2, Rect, Sense, TextFormat,
    ViewportCommand,
};
use whereti_ipc::{Row, Sort, StatusReply};

use crate::searcher::{Job, Searcher, Update};
use crate::shell;
use crate::tray::{Requests, Tray, PREFERRED_HOTKEY};

const ROW_HEIGHT: f32 = 40.0;
const RESULT_LIMIT: u32 = 500;
const STATUS_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
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

pub struct WheretiApp {
    query: String,
    filter: Filter,
    searcher: Searcher,
    generation: u64,
    rows: Vec<Row>,
    total: u64,
    service_time: Duration,
    warnings: Vec<String>,
    recent_mode: bool,
    selected: usize,
    scroll_to_selected: bool,
    scroll_offset: f32,
    view_height: f32,
    status: Option<StatusReply>,
    offline: Option<String>,
    last_status_ask: Instant,
    message: Option<(String, Instant)>,
    focus_search: bool,
    search_id: Option<egui::Id>,
    visible: bool,
    quitting: bool,
    tz: i32,
    requests: Arc<Requests>,
    tray: Option<Tray>,
}

impl WheretiApp {
    pub fn new(ctx: &egui::Context, requests: Arc<Requests>, tray: Option<Tray>, visible: bool) -> Self {
        let wake_ctx = ctx.clone();
        let searcher = Searcher::start(move || wake_ctx.request_repaint());
        let mut app = WheretiApp {
            query: String::new(),
            filter: Filter::All,
            searcher,
            generation: 0,
            rows: Vec::new(),
            total: 0,
            service_time: Duration::ZERO,
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
            requests,
            tray,
        };
        match app.tray.as_ref().map(|t| t.hotkey) {
            Some(Some(k)) if k != PREFERRED_HOTKEY => {
                app.flash(format!("{PREFERRED_HOTKEY} e folosită de altă aplicație — tasta rapidă este {k}"))
            }
            Some(None) => app.flash("Nicio tastă rapidă disponibilă; deschide whereti din iconița din tray"),
            _ => {}
        }
        app.run_search();
        app
    }

    /// The query actually sent: what was typed plus the filter, or the
    /// "recent files" list when there is nothing to search for.
    fn effective_query(&self) -> (String, Sort, bool) {
        let q = self.query.trim();
        if q.is_empty() && self.filter == Filter::All {
            let home = std::env::var("USERPROFILE").unwrap_or_default();
            let recent = format!(r#"file: path:"{home}\Desktop" | path:"{home}\Documents" | path:"{home}\Downloads""#);
            return (recent, Sort::Modified, true);
        }
        (format!("{q} {}", self.filter.clause()), Sort::Relevance, false)
    }

    fn run_search(&mut self) {
        self.generation += 1;
        let (query, sort, recent) = self.effective_query();
        self.recent_mode = recent;
        self.searcher.search(Job { generation: self.generation, query, sort, limit: RESULT_LIMIT });
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    fn show(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.visible = true;
        self.focus_search = true;
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
                    self.service_time = round_trip; // what the user waits for
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
        let page = ((self.view_height / (ROW_HEIGHT + 4.0)) as usize).max(1);
        let (down, up, pg_down, pg_up, ctrl_enter, enter, copy, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::C),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
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

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        let edit = egui::TextEdit::singleline(&mut self.query)
            .hint_text("Caută fișiere și foldere…")
            .font(FontId::proportional(20.0))
            .margin(egui::vec2(10.0, 8.0))
            .desired_width(f32::INFINITY);
        let resp = ui.add(edit);
        self.search_id = Some(resp.id);
        if self.focus_search {
            resp.request_focus();
            self.focus_search = false;
        }
        if resp.changed() {
            self.run_search();
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for f in Filter::ALL {
                if ui.selectable_label(self.filter == f, f.label()).clicked() && self.filter != f {
                    self.filter = f;
                    self.run_search();
                    self.focus_search = true;
                }
            }
        });
        for w in &self.warnings {
            ui.label(egui::RichText::new(w).small().color(ui.visuals().warn_fg_color));
        }
    }

    fn results(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Some(msg) = &self.offline {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Serviciul whereti nu rulează").size(18.0).strong());
                ui.add_space(6.0);
                ui.label(format!("({msg})"));
                ui.label("Pornește-l dintr-un terminal de administrator: whereti-service start");
            });
            return;
        }
        if self.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let text = if self.query.trim().is_empty() && self.filter == Filter::All {
                    "Niciun fișier modificat recent în Desktop, Documents sau Downloads.".to_string()
                } else {
                    format!("Nimic găsit pentru „{}”.", self.query.trim())
                };
                ui.label(egui::RichText::new(text).size(15.0));
            });
            return;
        }
        if self.recent_mode {
            ui.label(egui::RichText::new("Recente — modificate în Desktop, Documents, Downloads").small().weak());
            ui.add_space(2.0);
        }

        // Keep the selected row visible after keyboard moves.
        let row_span = ROW_HEIGHT + ui.spacing().item_spacing.y;
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if self.scroll_to_selected {
            let top = self.selected as f32 * row_span;
            let mut off = self.scroll_offset;
            if top < off {
                off = top;
            } else if top + row_span > off + self.view_height {
                off = top + row_span - self.view_height;
            }
            area = area.vertical_scroll_offset(off.max(0.0));
            self.scroll_to_selected = false;
        }
        let mut clicked: Option<(usize, bool)> = None;
        let rows = &self.rows;
        let selected = self.selected;
        let tz = self.tz;
        let out = area.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for i in range {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
                paint_row(ui, rect, &rows[i], i == selected, resp.hovered(), tz);
                if resp.double_clicked() {
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
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        // A short message replaces the status line for a few seconds.
        if let Some((msg, at)) = &self.message {
            if at.elapsed() < Duration::from_secs(8) {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(msg).strong().color(ui.visuals().warn_fg_color));
                });
                ui.ctx().request_repaint_after(Duration::from_secs(1));
                return;
            }
        }
        ui.horizontal(|ui| {
            if self.offline.is_none() {
                let n = if self.recent_mode { self.rows.len() as u64 } else { self.total };
                let ms = format!("{:.1}", self.service_time.as_secs_f64() * 1000.0).replace('.', ",");
                ui.label(format!("{} rezultate · {ms} ms", shell::count(n)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(s) = &self.status {
                    ui.label(
                        egui::RichText::new(format!("{} fișiere · {}", shell::count(s.entries), state_ro(&s.state))).weak(),
                    );
                }
                ui.label(
                    egui::RichText::new("Enter deschide · Ctrl+Enter arată în Explorer · Ctrl+Shift+C copiază calea")
                        .weak()
                        .small(),
                );
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

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
        self.handle_keys(&ctx);

        egui::Panel::top("search").show(ui, |ui| {
            ui.add_space(8.0);
            self.search_bar(ui);
            ui.add_space(6.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(3.0);
            self.status_bar(ui);
            ui.add_space(3.0);
        });
        egui::CentralPanel::default().show(ui, |ui| self.results(ui, &ctx));
    }
}

fn state_ro(state: &str) -> String {
    match state {
        "ready" => "actualizat".into(),
        "loading" => "se încarcă indexul".into(),
        "indexing" => "se construiește indexul".into(),
        "catching up" => "se aduce la zi".into(),
        s if s.starts_with("rescanning") => s.replace("rescanning", "se rescanează"),
        s => s.to_string(),
    }
}

/// One result: icon, name (bold) over its folder, then date and size.
fn paint_row(ui: &egui::Ui, rect: Rect, row: &Row, selected: bool, hovered: bool, tz: i32) {
    let v = ui.visuals();
    let painter = ui.painter_at(rect);
    if selected {
        painter.rect_filled(rect, 4.0, v.selection.bg_fill);
    } else if hovered {
        painter.rect_filled(rect, 4.0, v.widgets.hovered.weak_bg_fill);
    }
    let strong = if selected { v.selection.stroke.color } else { v.strong_text_color() };
    let weak = if selected { v.selection.stroke.color.gamma_multiply(0.8) } else { v.weak_text_color() };

    let (folder, name) = match row.path.rfind('\\') {
        Some(i) if i + 1 < row.path.len() => (&row.path[..i], &row.path[i + 1..]),
        _ => ("", row.path.as_str()),
    };
    paint_icon(&painter, Pos2::new(rect.left() + 19.0, rect.center().y), row.is_dir, name, v.dark_mode);

    let right_w = 220.0;
    let text_left = rect.left() + 38.0;
    let text_w = (rect.width() - 38.0 - right_w).max(60.0);
    let name_g = elided(ui, name, FontId::new(15.0, FontFamily::Name("bold".into())), strong, text_w);
    painter.galley(Pos2::new(text_left, rect.top() + 3.0), name_g, strong);
    let folder_g = elided(ui, folder, FontId::proportional(12.5), weak, text_w);
    painter.galley(Pos2::new(text_left, rect.top() + 22.0), folder_g, weak);

    let date = shell::local_time(row.modified, tz);
    painter.text(
        Pos2::new(rect.right() - 90.0, rect.center().y),
        Align2::RIGHT_CENTER,
        date,
        FontId::proportional(12.5),
        weak,
    );
    let size = if row.is_dir { String::new() } else { shell::size(row.size) };
    painter.text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, size, FontId::proportional(12.5), weak);
}

/// Folder, or a page with a folded corner coloured by file type.
fn paint_icon(painter: &egui::Painter, c: Pos2, is_dir: bool, name: &str, dark: bool) {
    use egui::{epaint::PathStroke, Shape, Stroke, Vec2};
    if is_dir {
        let body = Rect::from_center_size(c + Vec2::new(0.0, 1.5), Vec2::new(22.0, 15.0));
        let tab = Rect::from_min_size(body.min - Vec2::new(0.0, 3.0), Vec2::new(9.0, 5.0));
        let fill = Color32::from_rgb(0xE8, 0xB3, 0x41);
        painter.rect_filled(tab, 1.5, fill);
        painter.rect_filled(body, 2.0, fill);
        return;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let color = match ext.as_str() {
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx"
        | "odp" | "epub" | "msg" | "eml" => Color32::from_rgb(0x2B, 0x6C, 0xD9),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "svg" | "ico" | "raw" => {
            Color32::from_rgb(0x2E, 0x9E, 0x5B)
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" => Color32::from_rgb(0x8E, 0x5B, 0xD6),
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" => Color32::from_rgb(0xD9, 0x4A, 0x3D),
        "zip" | "rar" | "7z" | "tar" | "gz" | "zst" | "cab" | "iso" => Color32::from_rgb(0x9C, 0x6B, 0x3A),
        "exe" | "msi" | "bat" | "cmd" | "ps1" | "dll" => Color32::from_rgb(0x55, 0x65, 0x7A),
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cpp" | "h" | "cs" | "php" | "rb" | "html"
        | "css" | "json" | "yaml" | "yml" | "toml" | "xml" | "sql" | "sh" => Color32::from_rgb(0xD9, 0x82, 0x2B),
        _ => if dark { Color32::from_gray(0xA0) } else { Color32::from_gray(0x80) },
    };
    let (w, h, fold) = (15.0, 19.0, 5.0);
    let tl = c - Vec2::new(w / 2.0, h / 2.0);
    let pts = vec![
        tl,
        tl + Vec2::new(w - fold, 0.0),
        tl + Vec2::new(w, fold),
        tl + Vec2::new(w, h),
        tl + Vec2::new(0.0, h),
    ];
    let paper = if dark { Color32::from_gray(0x2A) } else { Color32::WHITE };
    painter.add(Shape::convex_polygon(pts.clone(), paper, Stroke::new(1.3, color)));
    painter.add(Shape::line(
        vec![tl + Vec2::new(w - fold, 0.0), tl + Vec2::new(w - fold, fold), tl + Vec2::new(w, fold)],
        PathStroke::new(1.3, color),
    ));
    // a coloured band at the bottom, like a file-type label
    let band = Rect::from_min_max(tl + Vec2::new(0.0, h - 6.0), tl + Vec2::new(w, h));
    painter.rect_filled(band, 0.0, color);
}

/// Single-line text cut with "…" to fit `width`.
fn elided(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, color));
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts_mut(|f| f.layout_job(job))
}
