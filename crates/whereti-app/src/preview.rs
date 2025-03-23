//! Preview panel (PLAN.md §10.6): the text of the selected file, with the
//! searched words highlighted, next to the results.
//!
//! Text comes from `whereti-extract`: plain text is read here, documents
//! (PDF, Word, Excel…) go through the sandboxed worker process. A background
//! thread does the work, so moving through results never stutters; only the
//! newest selection is extracted, and recent ones are cached.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, text::LayoutJob, Align, CornerRadius, FontId, Frame, Margin, Sense, Stroke, TextFormat, Vec2};
use whereti_extract::worker::{read_shared, Sandbox, SandboxError};
use whereti_extract::{ExtractError, Kind};
use whereti_ipc::Row;

use crate::app::{match_mask, paint_tile, relative_time, semibold};
use crate::shell;
use crate::theme::{self, Palette};

/// Characters shown (and searched for highlights) in the panel.
const SHOW_CHARS: usize = 60_000;
const CACHE: usize = 16;
/// Wait this long after a selection change before extracting, so holding
/// the arrow key does not start a worker per row.
const DEBOUNCE: Duration = Duration::from_millis(90);

pub enum Content {
    /// A folder: its first entries (folders first), and how many there are.
    Folder { entries: Vec<(String, bool)>, folders: usize, files: usize },
    Text { text: String, truncated: bool, mono: bool },
    /// Nothing to show for this type (images, video, archives…).
    NoPreview,
    Failed(String),
}

struct Loaded {
    path: String,
    content: Content,
    took: Duration,
}

/// A document's text laid out with its matches marked, for one query.
struct Highlighted {
    path: String,
    needles: Vec<String>,
    job: LayoutJob,
    hits: usize,
    /// Character index of the first match.
    first: Option<usize>,
}

pub struct Preview {
    pub open: bool,
    tx: Sender<String>,
    rx: Receiver<Loaded>,
    /// The path shown (or being loaded).
    current: Option<String>,
    cache: VecDeque<Loaded>,
    /// Highlighted text, rebuilt when the document or the query changes.
    job: Option<Highlighted>,
    scroll_to_match: bool,
}

impl Preview {
    pub fn start(open: bool, wake: impl Fn() + Send + 'static) -> Preview {
        let (tx, jobs) = mpsc::channel::<String>();
        let (out, rx) = mpsc::channel::<Loaded>();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || worker(jobs, out, wake))
            .expect("preview thread");
        Preview { open, tx, rx, current: None, cache: VecDeque::new(), job: None, scroll_to_match: false }
    }

    /// Call every frame with the selected row; starts loading when it changed.
    pub fn select(&mut self, row: Option<&Row>) {
        for loaded in self.rx.try_iter().collect::<Vec<_>>() {
            if self.cache.len() >= CACHE {
                self.cache.pop_front();
            }
            self.cache.retain(|c| c.path != loaded.path);
            self.cache.push_back(loaded);
        }
        let path = row.map(|r| r.path.clone());
        if path == self.current {
            return;
        }
        self.current = path.clone();
        self.scroll_to_match = true;
        if let Some(path) = path {
            if self.cache.iter().any(|c| c.path == path) {
                return;
            }
            let _ = self.tx.send(path);
        }
    }

    fn loaded(&self) -> Option<&Loaded> {
        let path = self.current.as_ref()?;
        self.cache.iter().find(|c| &c.path == path)
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        row: Option<&Row>,
        icons: &mut crate::file_icons::FileIcons,
        needles: &[String],
        tz: i32,
    ) {
        let icon = row.and_then(|r| icons.get(ui.ctx(), &r.path, r.is_dir));
        let icon = icon.as_ref();
        let Some(row) = row else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.label(egui::RichText::new("Selectează un rezultat").size(13.0).color(p.text3)));
            return;
        };
        let (folder, name) = match row.path.rfind('\\') {
            Some(i) if i + 1 < row.path.len() => (&row.path[..i], &row.path[i + 1..]),
            _ => ("", row.path.as_str()),
        };

        // Header: tile, name, folder, facts.
        ui.horizontal_top(|ui| {
            let (tile, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
            match icon {
                Some(t) => {
                    let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
                    ui.painter().image(t.id(), tile, uv, egui::Color32::WHITE);
                }
                None => paint_tile(ui.painter(), p, tile, row.is_dir, name),
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(name).size(16.0).family(semibold()).color(p.text)).wrap());
                ui.add(egui::Label::new(egui::RichText::new(folder).size(12.0).color(p.text2)).wrap());
            });
        });
        ui.add_space(6.0);
        let mut facts = Vec::new();
        if !row.is_dir {
            facts.push(shell::size(row.size));
        }
        facts.push(format!("modificat {}", relative_time(row.modified, tz, crate::app::now_minutes())));
        if let Some(l) = self.loaded() {
            if let Content::Text { text, truncated, .. } = &l.content {
                let words = text.split_whitespace().count();
                facts.push(format!("{} cuvinte{}", shell::count(words as u64), if *truncated { "+" } else { "" }));
            }
        }
        ui.label(egui::RichText::new(facts.join("  ·  ")).size(12.0).color(p.text3));
        ui.add_space(10.0);

        if row.is_dir {
            let Some(Content::Folder { entries, folders, files }) = self.loaded().map(|l| &l.content) else {
                return;
            };
            let summary = match (*folders, *files) {
                (0, 0) => "Folder gol".to_string(),
                (d, f) => [(d, "folder", "foldere"), (f, "fișier", "fișiere")]
                    .iter()
                    .filter(|(n, _, _)| *n > 0)
                    .map(|(n, one, many)| format!("{} {}", shell::count(*n as u64), if *n == 1 { one } else { many }))
                    .collect::<Vec<_>>()
                    .join(" · "),
            };
            ui.label(egui::RichText::new(summary).size(12.0).family(semibold()).color(p.text2));
            ui.add_space(6.0);
            let entries = entries.clone();
            let base = row.path.clone();
            Frame::NONE
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().id_salt(&base).auto_shrink([false, true]).show(ui, |ui| {
                        for (name, is_dir) in &entries {
                            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
                            let r = egui::Rect::from_center_size(egui::pos2(rect.left() + 14.0, rect.center().y), Vec2::splat(18.0));
                            let full = format!("{base}\\{name}");
                            match icons.get(ui.ctx(), &full, *is_dir) {
                                Some(t) => {
                                    let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
                                    ui.painter().image(t.id(), r, uv, egui::Color32::WHITE);
                                }
                                None => paint_tile(ui.painter(), p, r, *is_dir, name),
                            }
                            ui.painter().text(
                                egui::pos2(rect.left() + 32.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                name,
                                FontId::proportional(13.0),
                                p.text,
                            );
                        }
                        let shown = entries.len();
                        if folders + files > shown {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(format!("și încă {} — Enter îl deschide în Explorer", shell::count((folders + files - shown) as u64)))
                                    .size(12.0)
                                    .color(p.text3),
                            );
                        }
                    });
                });
            return;
        }
        let Some(loaded) = self.loaded() else {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(p.text3));
                ui.label(egui::RichText::new("Se citește textul…").size(12.5).color(p.text3));
            });
            return;
        };
        let took = loaded.took;
        match &loaded.content {
            Content::Folder { .. } => {}
            Content::NoPreview => note(ui, p, "Fără previzualizare", "whereti afișează textul documentelor, al fișierelor text și al codului."),
            Content::Failed(e) => note(ui, p, "Textul nu poate fi citit", e),
            Content::Text { text, .. } if text.trim().is_empty() => note(
                ui,
                p,
                "Documentul nu conține text",
                "Probabil e scanat (doar imagini). Recunoașterea textului din imagini (OCR) vine într-o versiune viitoare.",
            ),
            Content::Text { text, truncated, mono } => {
                let (text, truncated, mono) = (text.clone(), *truncated, *mono);
                self.text_view(ui, p, &text, truncated, mono, needles, took);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text_view(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        text: &str,
        truncated: bool,
        mono: bool,
        needles: &[String],
        took: Duration,
    ) {
        let path = self.current.clone().unwrap_or_default();
        let stale = !matches!(&self.job, Some(h) if h.path == path && h.needles == needles);
        if stale {
            let (job, hits, first) = highlight(text, needles, p, mono);
            self.job = Some(Highlighted { path: path.clone(), needles: needles.to_vec(), job, hits, first });
            self.scroll_to_match = true;
        }
        let Some(Highlighted { job, hits, first, .. }) = &self.job else { return };
        let (hits, first) = (*hits, *first);

        ui.horizontal(|ui| {
            if hits > 0 {
                let label = format!("{} potriviri în text", shell::count(hits as u64));
                ui.label(egui::RichText::new(label).size(12.0).family(semibold()).color(p.accent));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let ms = took.as_millis();
                let note = if truncated { format!("primele {} caractere · {ms} ms", shell::count(SHOW_CHARS as u64)) } else { format!("{ms} ms") };
                ui.label(egui::RichText::new(note).size(11.0).color(p.text3));
            });
        });
        ui.add_space(6.0);
        Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                // One scroll state per document: a new file starts at its top
                // (or at its first match), not where the last one was left.
                let mut area = egui::ScrollArea::vertical().id_salt(&path).auto_shrink([false, false]);
                if self.scroll_to_match && first.is_none() {
                    area = area.vertical_scroll_offset(0.0);
                }
                area.show(ui, |ui| {
                    let (pos, galley, _) = egui::Label::new(job.clone()).wrap().layout_in_ui(ui);
                    ui.painter().galley(pos, galley.clone(), p.text);
                    if self.scroll_to_match {
                        self.scroll_to_match = false;
                        if let Some(c) = first {
                            let r = galley.pos_from_cursor(egui::text::CCursor::new(c)).translate(pos.to_vec2());
                            // No animation: a half-finished one would keep
                            // moving this document when it is shown again.
                            let none = egui::style::ScrollAnimation::none();
                            ui.scroll_to_rect_animation(r.expand(40.0), Some(Align::Center), none);
                        }
                    }
                });
            });
    }
}

fn note(ui: &mut egui::Ui, p: &Palette, title: &str, sub: &str) {
    Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).size(14.0).family(semibold()).color(p.text));
            if !sub.is_empty() {
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new(sub).size(12.5).color(p.text2)).wrap());
            }
        });
}

/// The text with matches marked: (job, number of matches, char index of the first).
fn highlight(text: &str, needles: &[String], p: &Palette, mono: bool) -> (LayoutJob, usize, Option<usize>) {
    let font = if mono { FontId::monospace(12.5) } else { FontId::proportional(13.5) };
    let plain = TextFormat { font_id: font.clone(), color: p.text, line_height: Some(if mono { 18.0 } else { 20.0 }), ..Default::default() };
    let mark = TextFormat {
        background: theme::with_alpha(p.accent, if p.dark { 0.45 } else { 0.25 }),
        color: p.text,
        ..plain.clone()
    };
    let mut job = LayoutJob::default();
    // "termen de livrare": "de" alone would light up half the text.
    let phrase = needles.join(" ");
    let all_words = needles.len();
    let long: Vec<String> = needles.iter().filter(|n| n.chars().count() >= 3).cloned().collect();
    let needles: &[String] = if long.is_empty() { needles } else { &long };
    let marked = if needles.is_empty() { Vec::new() } else { match_mask(text, needles) };
    if marked.is_empty() || !marked.contains(&true) {
        job.append(text, 0.0, plain);
        return (job, 0, None);
    }
    // Jump to the whole phrase when the text has it, else to the first word.
    let phrase_at = (all_words > 1).then(|| match_mask(text, &[phrase]).iter().position(|&m| m)).flatten();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let (mut hits, mut first) = (0, phrase_at);
    let mut i = 0;
    while i < chars.len() {
        let m = marked[i];
        let mut j = i;
        while j < chars.len() && marked[j] == m {
            j += 1;
        }
        if m {
            hits += 1;
            first.get_or_insert(i);
        }
        let end = chars.get(j).map_or(text.len(), |c| c.0);
        job.append(&text[chars[i].0..end], 0.0, if m { mark.clone() } else { plain.clone() });
        i = j;
    }
    (job, hits, first)
}

// ---------------------------------------------------------------- worker

fn worker(jobs: Receiver<String>, out: Sender<Loaded>, wake: impl Fn()) {
    let sandbox = Sandbox::beside_current_exe();
    while let Ok(mut path) = jobs.recv() {
        // Only the newest selection matters.
        loop {
            std::thread::sleep(DEBOUNCE);
            match jobs.try_iter().last() {
                Some(newer) => path = newer,
                None => break,
            }
        }
        let t = Instant::now();
        let content = load(&path, sandbox.as_ref());
        if out.send(Loaded { path, content, took: t.elapsed() }).is_err() {
            return;
        }
        wake();
    }
}

/// Entries listed for a folder.
const FOLDER_ENTRIES: usize = 60;

fn load(path: &str, sandbox: Option<&Sandbox>) -> Content {
    let p = Path::new(path);
    if p.is_dir() {
        let mut all: Vec<(String, bool)> = match std::fs::read_dir(p) {
            Ok(rd) => rd
                .flatten()
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                .collect(),
            Err(e) => return Content::Failed(format!("Folderul nu poate fi deschis: {e}")),
        };
        let folders = all.iter().filter(|e| e.1).count();
        let files = all.len() - folders;
        all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        all.truncate(FOLDER_ENTRIES);
        return Content::Folder { entries: all, folders, files };
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let Some(kind) = whereti_extract::kind_of(&ext) else { return Content::NoPreview };
    let mono = kind == Kind::Text && !matches!(ext.as_str(), "txt" | "md" | "markdown" | "log" | "srt" | "vtt" | "rst" | "adoc" | "org" | "tex");
    let result = if kind == Kind::Text {
        // Plain text needs no parser: read only what is shown.
        read_shared_head(p, (SHOW_CHARS * 4) as u64)
            .and_then(|b| whereti_extract::extract(&ext, &b, SHOW_CHARS).map_err(SandboxError::Extract))
    } else {
        match sandbox {
            Some(s) => s.extract_file(p, SHOW_CHARS),
            None => return Content::Failed("whereti-extract.exe lipsește de lângă whereti.exe".into()),
        }
    };
    match result {
        Ok(e) => Content::Text { text: e.text, truncated: e.truncated, mono },
        Err(SandboxError::Extract(ExtractError::Unsupported)) => Content::NoPreview,
        Err(SandboxError::Extract(ExtractError::Protected)) => Content::Failed("Documentul e protejat cu parolă.".into()),
        Err(SandboxError::Extract(ExtractError::Damaged(e))) => Content::Failed(format!("Fișierul pare deteriorat ({e}).")),
        Err(SandboxError::TooLarge(n)) => Content::Failed(format!("Fișierul e prea mare pentru previzualizare ({} MB).", n >> 20)),
        Err(SandboxError::Io(e)) => Content::Failed(format!("Fișierul nu poate fi deschis: {e}")),
        Err(SandboxError::Failed(e)) => Content::Failed(format!("Extragerea s-a oprit: {e}")),
    }
}

/// The first `max` bytes of a text file (a 2 GB log is fine).
fn read_shared_head(path: &Path, max: u64) -> Result<Vec<u8>, SandboxError> {
    match read_shared(path, max) {
        Err(SandboxError::TooLarge(_)) => {
            use std::io::Read;
            use std::os::windows::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new().read(true).share_mode(7).open(path).map_err(SandboxError::Io)?;
            let mut buf = Vec::new();
            f.take(max).read_to_end(&mut buf).map_err(SandboxError::Io)?;
            // don't cut a UTF-8 character in half
            while !buf.is_empty() && std::str::from_utf8(&buf).is_err_and(|e| e.error_len().is_none()) {
                buf.pop();
            }
            Ok(buf)
        }
        other => other,
    }
}

