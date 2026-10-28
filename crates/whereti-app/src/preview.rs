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
use crate::i18n::tr;
use crate::shell;
use crate::{trf, trn};
use crate::theme::{self, Palette};

/// Characters shown (and searched for highlights) in the panel.
const SHOW_CHARS: usize = 60_000;
const CACHE: usize = 16;
/// The cache holds at most about this much (photos are ~3 MB each).
const CACHE_BYTES: usize = 24 << 20;
/// Wait this long after a selection change before extracting, so holding
/// the arrow key does not start a worker per row.
const DEBOUNCE: Duration = Duration::from_millis(90);

pub enum Content {
    /// A folder: its first entries (folders first), and how many there are.
    Folder { entries: Vec<(String, bool)>, folders: usize, files: usize },
    Text { text: String, truncated: bool, mono: bool },
    /// A photo: the picture itself (RGBA, already scaled down), where it was
    /// taken, and the words read in it (OCR), if any.
    Picture { width: u32, height: u32, rgba: std::sync::Arc<Vec<u8>>, place: Option<String>, text: String },
    /// Nothing to show for this type (images, video, archives…).
    NoPreview,
    Failed(String),
}

/// A file as it is now: its path, size and date. A file changed since it
/// was read has another key, so it is read again.
type Key = (String, u64, u32);

fn key_of(row: &Row) -> Key {
    (row.path.clone(), row.size, row.modified)
}

struct Loaded {
    key: Key,
    content: Content,
    took: Duration,
}

impl Content {
    /// About how much memory it holds.
    fn bytes(&self) -> usize {
        match self {
            Content::Folder { entries, .. } => entries.iter().map(|e| e.0.len() + 32).sum(),
            Content::Text { text, .. } => text.len(),
            Content::Picture { rgba, place, text, .. } => rgba.len() + text.len() + place.as_ref().map_or(0, |p| p.len()),
            Content::NoPreview => 0,
            Content::Failed(e) => e.len(),
        }
    }
}

/// A document's text laid out with its matches marked, for one query.
struct Highlighted {
    key: Key,
    needles: Vec<String>,
    job: LayoutJob,
    hits: usize,
    /// Character index of the first match.
    first: Option<usize>,
}

pub struct Preview {
    pub open: bool,
    tx: Sender<Key>,
    rx: Receiver<Loaded>,
    /// The file shown (or being loaded).
    current: Option<Key>,
    cache: VecDeque<Loaded>,
    /// Highlighted text, rebuilt when the document or the query changes.
    job: Option<Highlighted>,
    scroll_to_match: bool,
    /// The shown photo, as a texture (made on the UI thread).
    picture: Option<(Key, egui::TextureHandle)>,
    /// Chinese, Japanese or Korean text was read (the fonts for it are loaded then).
    pub saw_cjk: bool,
}

impl Preview {
    pub fn start(open: bool, wake: impl Fn() + Send + 'static) -> Preview {
        let (tx, jobs) = mpsc::channel::<Key>();
        let (out, rx) = mpsc::channel::<Loaded>();
        std::thread::Builder::new()
            .name("preview".into())
            .spawn(move || worker(jobs, out, wake))
            .expect("preview thread");
        Preview { open, tx, rx, current: None, cache: VecDeque::new(), job: None, scroll_to_match: false, picture: None, saw_cjk: false }
    }

    /// Call every frame with the selected row; starts loading when it changed.
    pub fn select(&mut self, row: Option<&Row>) {
        for loaded in self.rx.try_iter().collect::<Vec<_>>() {
            self.saw_cjk |= match &loaded.content {
                Content::Text { text, .. } | Content::Picture { text, .. } => crate::app::has_cjk(text),
                Content::Folder { entries, .. } => entries.iter().any(|e| crate::app::has_cjk(&e.0)),
                _ => false,
            };
            // (the same file as it was before goes too)
            self.cache.retain(|c| c.key.0 != loaded.key.0);
            self.cache.push_back(loaded);
            // the oldest go first, beyond so many or so much
            let mut bytes: usize = self.cache.iter().map(|c| c.content.bytes()).sum();
            while self.cache.len() > 1 && (self.cache.len() > CACHE || bytes > CACHE_BYTES) {
                if let Some(old) = self.cache.pop_front() {
                    bytes -= old.content.bytes();
                }
            }
        }
        let key = row.map(key_of);
        if key == self.current {
            return;
        }
        self.current = key.clone();
        self.scroll_to_match = true;
        if let Some(key) = key {
            if self.cache.iter().any(|c| c.key == key) {
                return;
            }
            let _ = self.tx.send(key);
        }
    }

    fn loaded(&self) -> Option<&Loaded> {
        let key = self.current.as_ref()?;
        self.cache.iter().find(|c| &c.key == key)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        row: Option<&Row>,
        icons: &mut crate::file_icons::FileIcons,
        needles: &[String],
        tz: i32,
        info: Option<&whereti_ipc::FileInfo>,
    ) {
        let icon = row.and_then(|r| icons.get(ui.ctx(), &r.path, r.is_dir));
        let icon = icon.as_ref();
        let Some(row) = row else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.label(egui::RichText::new(tr("Select a result")).size(13.0).color(p.text3)));
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
        facts.push(trf!("modified {when}", when = relative_time(row.modified, tz, crate::app::now_minutes())));
        if let Some(l) = self.loaded() {
            if let Content::Text { text, truncated, .. } = &l.content {
                let words = text.split_whitespace().count();
                let plus = if *truncated { "+" } else { "" };
                facts.push(trn!("{n} word{plus}|{n} words{plus}", words, plus));
            }
        }
        ui.label(egui::RichText::new(facts.join("  ·  ")).size(12.0).color(p.text3));
        if let Some(info) = info {
            details(ui, p, info, tz);
        }
        ui.add_space(10.0);

        if row.is_dir {
            let Some(Content::Folder { entries, folders, files }) = self.loaded().map(|l| &l.content) else {
                return;
            };
            let summary = match (*folders, *files) {
                (0, 0) => tr("Empty folder").to_string(),
                (d, f) => {
                    let mut parts = Vec::new();
                    if d > 0 {
                        parts.push(trn!("{n} folder|{n} folders", d));
                    }
                    if f > 0 {
                        parts.push(trn!("{n} file|{n} files", f));
                    }
                    parts.join(" · ")
                }
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
                            // long names end in "…" instead of being cut at the edge
                            let mut job = egui::text::LayoutJob::simple_singleline(name.clone(), FontId::proportional(13.0), p.text);
                            job.wrap = egui::text::TextWrapping {
                                max_width: (rect.right() - rect.left() - 40.0).max(20.0),
                                max_rows: 1,
                                break_anywhere: true,
                                overflow_character: Some('…'),
                            };
                            let g = ui.fonts_mut(|f| f.layout_job(job));
                            ui.painter().galley(egui::pos2(rect.left() + 32.0, rect.center().y - g.size().y / 2.0), g, p.text);
                        }
                        let shown = entries.len();
                        if folders + files > shown {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(trn!("and {n} more. Enter opens it in Explorer.|and {n} more. Enter opens it in Explorer.", folders + files - shown))
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
                ui.label(egui::RichText::new(tr("Reading the text…")).size(12.5).color(p.text3));
            });
            return;
        };
        let took = loaded.took;
        match &loaded.content {
            Content::Folder { .. } => {}
            Content::NoPreview => note(
                ui,
                p,
                tr("No preview"),
                tr("whereti shows the text of documents, text files and code."),
            ),
            Content::Failed(e) => note(ui, p, tr("The text cannot be read"), e),
            Content::Text { text, .. } if text.trim().is_empty() => note(
                ui,
                p,
                tr("No text found"),
                tr("whereti found no words in it, not even in its pictures."),
            ),
            Content::Text { text, truncated, mono } => {
                let (text, truncated, mono) = (text.clone(), *truncated, *mono);
                self.text_view(ui, p, &text, truncated, mono, needles, took);
            }
            Content::Picture { width, height, rgba, place, text } => {
                let (w, h, rgba, place, text) = (*width, *height, rgba.clone(), place.clone(), text.clone());
                let key = self.current.clone().unwrap_or_default();
                if self.picture.as_ref().is_none_or(|(k, _)| k != &key) {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let texture = ui.ctx().load_texture("preview-picture", image, egui::TextureOptions::LINEAR);
                    self.picture = Some((key, texture));
                }
                if let Some((_, texture)) = &self.picture {
                    // the whole picture, as large as the pane allows
                    let max_w = ui.available_width();
                    let max_h = (ui.available_height() * 0.62).max(160.0);
                    let scale = (max_w / w as f32).min(max_h / h as f32).min(1.0);
                    let size = Vec2::new(w as f32 * scale, h as f32 * scale);
                    ui.add(egui::Image::new((texture.id(), size)).corner_radius(10.0));
                }
                if let Some(place) = place {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(place).size(12.5).color(p.text2));
                }
                if !text.trim().is_empty() {
                    ui.add_space(8.0);
                    self.text_view(ui, p, &text, false, false, needles, took);
                }
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
        let key = self.current.clone().unwrap_or_default();
        let path = key.0.clone();
        let stale = !matches!(&self.job, Some(h) if h.key == key && h.needles == needles);
        if stale {
            let (job, hits, first) = highlight(text, needles, p, mono);
            self.job = Some(Highlighted { key, needles: needles.to_vec(), job, hits, first });
            self.scroll_to_match = true;
        }
        let Some(Highlighted { job, hits, first, .. }) = &self.job else { return };
        let (hits, first) = (*hits, *first);

        ui.horizontal(|ui| {
            if hits > 0 {
                let label = trn!("{n} match in the text|{n} matches in the text", hits);
                ui.label(egui::RichText::new(label).size(12.0).family(semibold()).color(p.accent));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let ms = took.as_millis();
                let note = if truncated { trn!("first {n} character · {ms} ms|first {n} characters · {ms} ms", SHOW_CHARS, ms) } else { format!("{ms} ms") };
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

fn worker(jobs: Receiver<Key>, out: Sender<Loaded>, wake: impl Fn()) {
    let sandbox = Sandbox::beside_current_exe();
    while let Ok(mut key) = jobs.recv() {
        // Only the newest selection matters.
        loop {
            std::thread::sleep(DEBOUNCE);
            match jobs.try_iter().last() {
                Some(newer) => key = newer,
                None => break,
            }
        }
        let t = Instant::now();
        let content = load(&key.0, sandbox.as_ref());
        if out.send(Loaded { key, content, took: t.elapsed() }).is_err() {
            return;
        }
        wake();
    }
}

/// Entries listed for a folder.
const FOLDER_ENTRIES: usize = 60;

fn load(path: &str, sandbox: Option<&Sandbox>) -> Content {
    picture(path, sandbox).unwrap_or_else(|| load_with(path, sandbox, SHOW_CHARS))
}

/// Longer side of the picture shown in the preview.
const PICTURE_SIDE: u32 = 900;

/// A photo for the preview: the picture (decoded by Windows, like Explorer's
/// thumbnails), where it was taken, and its words (OCR, in the sandbox).
/// `None` when it is not a picture Windows can decode.
fn picture(path: &str, sandbox: Option<&Sandbox>) -> Option<Content> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?.to_ascii_lowercase();
    if whereti_extract::kind_of(&ext) != Some(Kind::Image) {
        return None;
    }
    let bytes = whereti_extract::worker::read_shared(Path::new(path), 64 << 20).ok()?;
    let (width, height, rgba) = whereti_extract::tags::picture(&bytes, PICTURE_SIDE).ok()?;
    let place = whereti_places::gps(&bytes).and_then(|(lat, lon)| whereti_places::place_of(lat, lon)).map(|p| p.label());
    let text = match load_with(path, sandbox, SHOW_CHARS) {
        Content::Text { text, .. } => text,
        _ => String::new(),
    };
    Some(Content::Picture { width, height, rgba: std::sync::Arc::new(rgba), place, text })
}

/// The file's text (or what it is instead), at most `max_chars`.
pub(crate) fn load_with(path: &str, sandbox: Option<&Sandbox>, max_chars: usize) -> Content {
    let p = Path::new(path);
    if p.is_dir() {
        let mut all: Vec<(String, bool)> = match std::fs::read_dir(p) {
            Ok(rd) => rd
                .flatten()
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                .collect(),
            Err(e) => return Content::Failed(trf!("The folder cannot be opened: {e}", e)),
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
        read_shared_head(p, (max_chars * 4) as u64)
            .and_then(|b| whereti_extract::extract(&ext, &b, max_chars).map_err(SandboxError::Extract))
    } else {
        match sandbox {
            Some(s) => s.extract_file(p, max_chars),
            None => return Content::Failed(tr("whereti-extract.exe is missing next to whereti.exe").into()),
        }
    };
    match result {
        Ok(e) => Content::Text { text: e.text, truncated: e.truncated, mono },
        Err(SandboxError::Extract(ExtractError::Unsupported)) => Content::NoPreview,
        Err(SandboxError::Extract(ExtractError::Protected)) => Content::Failed(tr("The document is password-protected.").into()),
        Err(SandboxError::Extract(ExtractError::Damaged(e))) => Content::Failed(trf!("The file looks damaged ({e}).", e)),
        Err(SandboxError::TooLarge(n)) => Content::Failed(trf!("The file is too large to preview ({size} MB).", size = n >> 20)),
        Err(SandboxError::Io(e)) => Content::Failed(trf!("The file cannot be opened: {e}", e)),
        Err(SandboxError::Failed(e)) => Content::Failed(trf!("Reading the file stopped: {e}", e)),
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


/// What whereti read from the file: what the photo model saw, when and with
/// what a photo was taken, its size, a song's artist and length, a
/// document's author. Nothing when nothing is known.
fn details(ui: &mut egui::Ui, p: &Palette, info: &whereti_ipc::FileInfo, tz: i32) {
    let mut lines: Vec<String> = Vec::new();
    if !info.tags.is_empty() {
        // the labels in the app's language (en, de, es, fr, it, pl, pt-BR, ro)
        let col = match whereti_i18n::current().locale().code.get(..2).unwrap_or("en") {
            "de" => 1,
            "es" => 2,
            "fr" => 3,
            "it" => 4,
            "pl" => 5,
            "pt" => 6,
            "ro" => 7,
            _ => 0,
        };
        let labels = whereti_extract::tags::labels();
        let names: Vec<&str> = info.tags.iter().filter_map(|&t| labels.get(t as usize)?.get(col).copied()).collect();
        if !names.is_empty() {
            lines.push(trf!("In the photo: {things}", things = names.join(", ")));
        }
    }
    let mut photo = Vec::new();
    if info.taken != 0 {
        photo.push(trf!("taken {when}", when = relative_time(info.taken, tz, crate::app::now_minutes())));
    }
    if !info.camera.is_empty() {
        photo.push(info.camera.clone());
    }
    if info.width != 0 && info.height != 0 {
        photo.push(format!("{} × {}", info.width, info.height));
    }
    if !photo.is_empty() {
        lines.push(photo.join("  ·  "));
    }
    let mut media = Vec::new();
    if !info.artist.is_empty() {
        media.push(info.artist.clone());
    }
    if !info.album.is_empty() {
        media.push(info.album.clone());
    }
    if info.duration != 0 {
        let (h, m, s) = (info.duration / 3600, info.duration / 60 % 60, info.duration % 60);
        media.push(if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") });
    }
    if !media.is_empty() {
        lines.push(media.join("  ·  "));
    }
    if !info.author.is_empty() {
        lines.push(trf!("by {author}", author = info.author));
    }
    for line in lines {
        ui.add(egui::Label::new(egui::RichText::new(line).size(12.0).color(p.text2)).wrap());
    }
}
