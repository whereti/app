//! First start (PLAN.md §10.2): what whereti is doing while it builds its
//! index, in three steps tied to the service's real status, and how to use
//! it. Searching works at any moment; this page only explains the wait.

use eframe::egui::{self, Align, Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Pos2, Rect, Sense, Stroke, Vec2};

use crate::app::{content_active, content_progress, names_progress, semibold, state_ro, Page, WheretiApp};
use crate::i18n::tr;
use crate::theme::{self, Palette};
use crate::{trf, trn};

enum Step {
    Waiting,
    Working(Option<f32>),
    Done,
    Failed,
}

pub fn show(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let status = app.status.clone();
    let offline = app.offline.clone();
    let names_ready = status.as_ref().is_some_and(|s| s.state == "ready");
    let content = status.as_ref().map(|s| s.content.clone());
    let content_ready = names_ready && content.as_ref().is_some_and(|c| c.state == "ready");

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width().min(660.0);
        let pad = ((ui.available_width() - width) / 2.0).max(0.0);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.set_width(width);
                // ---- greeting, with the way out on the right: the whole page
                // fits the window as it first opens, without scrolling
                ui.horizontal(|ui| {
                    let right = 190.0;
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - right - 12.0);
                        ui.label(
                            egui::RichText::new(tr("Welcome to whereti"))
                                .size(24.0)
                                .family(FontFamily::Name("display".into()))
                                .color(p.text),
                        );
                        ui.add_space(2.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(tr("Instant search for every file on your PC, by its name and by what is written inside."))
                                    .size(13.5)
                                    .color(p.text2),
                            )
                            .wrap(),
                        );
                    });
                    ui.with_layout(egui::Layout::top_down(Align::Max), |ui| {
                        ui.set_width(right);
                        if primary_button(ui, p, tr("Start searching")).clicked() {
                            finish(app);
                        }
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(tr("whereti keeps working in the tray, next to the clock."))
                                    .size(11.5)
                                    .color(p.text3),
                            )
                            .halign(Align::Max)
                            .wrap(),
                        );
                    });
                });
                ui.add_space(12.0);

                // ---- the three steps
                let names_step = match (&offline, &status) {
                    (Some(_), _) => Step::Failed,
                    (None, Some(s)) if s.state == "ready" => Step::Done,
                    (None, Some(s)) => Step::Working(names_progress(&s.state)),
                    (None, None) => Step::Working(None),
                };
                let names_detail = match (&offline, &status) {
                    (Some(_), _) => tr("The whereti service is not running. Reinstall whereti, or start the service.")
                    .to_string(),
                    (None, Some(s)) if s.state == "ready" => {
                        trn!("{n} file or folder, on {drives}|{n} files and folders, on {drives}", s.entries, drives = drives(s))
                    }
                    (None, Some(s)) => state_ro(&s.state),
                    (None, None) => tr("connecting to the service…").to_string(),
                };
                let content_step = match (&content, names_ready) {
                    (_, false) => Step::Waiting,
                    (Some(c), true) if c.state == "ready" => Step::Done,
                    (Some(c), true) if content_active(c) => {
                        let all = c.done + c.queued;
                        Step::Working(Some(if all == 0 { 0.0 } else { c.done as f32 / all as f32 }))
                    }
                    (Some(c), true) if c.state.starts_with("error") => Step::Failed,
                    _ => Step::Working(None),
                };
                let content_detail = match (&content, names_ready) {
                    (_, false) => tr("Starts as soon as the names are read.").to_string(),
                    (Some(c), true) if c.state == "ready" => trn!("{n} document: PDF, Word, Excel, PowerPoint, e-mails|{n} documents: PDF, Word, Excel, PowerPoint, e-mails", c.files),
                    (Some(c), true) if content_active(c) => content_progress(c, app.content_eta(c)),
                    (Some(c), true) if c.state.starts_with("error") => c.state.clone(),
                    _ => tr("looking for documents…").to_string(),
                };
                let ready_step = if content_ready { Step::Done } else if names_ready { Step::Working(None) } else { Step::Waiting };
                let hotkey = app.hotkey_label().unwrap_or_else(|| crate::tray::PREFERRED_HOTKEY.to_string());
                let ready_detail = if names_ready {
                    trf!("Press {hotkey} in any app to open whereti. You can search already.", hotkey)
                } else {
                    tr("You can start typing now; results grow as the index is built.")
                    .to_string()
                };

                Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::symmetric(18, 2))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        step_row(ui, p, 1, &names_step, tr("Reading the file names"), &names_detail, false);
                        step_row(ui, p, 2, &content_step, tr("Reading the documents"), &content_detail, false);
                        step_row(ui, p, 3, &ready_step, tr("Ready to search"), &ready_detail, true);
                    });
                ui.add_space(10.0);

                // ---- the two optional downloads: offered once, never started unasked
                optional_downloads(app, ui, p);

            });
        });
    });
    // keep the steps moving while this page is open
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
}

/// Recognising photos and search by meaning, each a model downloaded once.
/// Offered here until answered ("Not now" or "Close"); after that only a line
/// while a download runs or fails. Settings > Search has both for good.
fn optional_downloads(app: &mut WheretiApp, ui: &mut egui::Ui, p: &Palette) {
    let tags = crate::settings::photo_tags_state(app);
    let meaning = crate::settings::meaning_state(app);
    if app.settings.photo_tags_asked {
        for line in [crate::settings::photo_tags_line(&tags), crate::settings::meaning_line(&meaning)].into_iter().flatten() {
            ui.label(egui::RichText::new(line).size(12.5).color(p.text2));
        }
        ui.add_space(12.0);
        return;
    }
    Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(18, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(tr("Optional: smarter search")).size(14.5).family(semibold()).color(p.text));
            let photos = optional_row(ui, p, tr("Recognize objects and animals in photos"), tr("Find photos by what they show: a dog, the beach, a car."), "84 MB", crate::settings::photo_tags_line(&tags), &tags);
            if photos {
                app.searcher.set_photo_tags(true);
            }
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.line));
            let words = optional_row(ui, p, tr("Search documents by meaning"), tr("Find documents written in other words: \"rental contract\" finds the lease."), "135 MB", crate::settings::meaning_line(&meaning), &meaning);
            if words {
                app.searcher.set_meaning(true);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let none_chosen = tags == "off" && meaning == "off";
                let label = if none_chosen { tr("Not now") } else { tr("Close") };
                if crate::settings::button(ui, p, label, false).clicked() {
                    app.settings.photo_tags_asked = true;
                    app.settings.save();
                }
                ui.add_space(10.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(tr("Each is downloaded once, from Hugging Face, and works only on your PC. Both stay in Settings, Search."))
                            .size(12.5)
                            .color(p.text3),
                    )
                    .wrap(),
                );
            });
        });
    ui.add_space(8.0);
}

/// One optional download: what it does, and a button while it is off, or how
/// it goes once chosen. True when the button was clicked.
fn optional_row(ui: &mut egui::Ui, p: &Palette, title: &str, desc: &str, size: &str, line: Option<String>, state: &str) -> bool {
    let off = state == "off";
    let mut clicked = false;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let label = trf!("Download ({size})", size);
        // off, or failed: the button (again); otherwise the text takes the whole width
        let with_button = off || state.starts_with("error");
        let control_w = if with_button { crate::settings::button_width(ui, &label) + 12.0 } else { 0.0 };
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - control_w).max(200.0));
            ui.label(egui::RichText::new(title).size(13.5).family(semibold()).color(p.text));
            ui.add(egui::Label::new(egui::RichText::new(desc).size(12.5).color(p.text2)).wrap());
            if let Some(line) = line {
                ui.add_space(2.0);
                let color = if state.starts_with("error") { p.error } else { p.accent };
                ui.label(egui::RichText::new(line).size(12.5).family(semibold()).color(color));
            }
        });
        if with_button {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                clicked = crate::settings::button(ui, p, &label, true).clicked();
            });
        }
    });
    ui.add_space(8.0);
    clicked
}

/// Leaves the welcome page for good.
pub fn finish(app: &mut WheretiApp) {
    app.page = Page::Search;
    app.focus_search = true;
    if !app.settings.welcomed {
        app.settings.welcomed = true;
        app.settings.save();
    }
}

fn drives(s: &whereti_ipc::StatusReply) -> String {
    s.volumes.iter().map(|v| format!("{}:", v.letter)).collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_arguments)]
fn step_row(ui: &mut egui::Ui, p: &Palette, n: usize, step: &Step, title: &str, detail: &str, last: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
    let painter = ui.painter();
    let c = Pos2::new(rect.left() + 16.0, rect.center().y);
    match step {
        Step::Done => {
            painter.circle_filled(c, 12.0, p.ok);
            let s = Stroke::new(2.0, Color32::WHITE);
            painter.line_segment([c + Vec2::new(-5.0, 0.5), c + Vec2::new(-1.5, 4.0)], s);
            painter.line_segment([c + Vec2::new(-1.5, 4.0), c + Vec2::new(5.5, -3.5)], s);
        }
        Step::Working(_) => {
            painter.circle_stroke(c, 12.0, Stroke::new(1.5, theme::with_alpha(p.accent, 0.25)));
            // a turning arc
            let t = ui.input(|i| i.time) as f32 * 4.0;
            let pts: Vec<Pos2> = (0..=16)
                .map(|k| {
                    let a = t + k as f32 * 0.12;
                    c + Vec2::new(a.cos(), a.sin()) * 12.0
                })
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(2.0, p.accent)));
        }
        Step::Waiting => {
            painter.circle_stroke(c, 12.0, Stroke::new(1.5, p.border));
            painter.text(c, egui::Align2::CENTER_CENTER, n.to_string(), FontId::new(12.0, semibold()), p.text3);
        }
        Step::Failed => {
            painter.circle_filled(c, 12.0, p.error);
            painter.text(c, egui::Align2::CENTER_CENTER, "!", FontId::new(14.0, semibold()), Color32::WHITE);
        }
    }
    let x = rect.left() + 44.0;
    let title_color = if matches!(step, Step::Waiting) { p.text3 } else { p.text };
    painter.text(Pos2::new(x, rect.top() + 5.0), egui::Align2::LEFT_TOP, title, FontId::new(14.0, semibold()), title_color);
    let detail_g = ui.fonts_mut(|f| {
        f.layout(detail.to_string(), FontId::proportional(12.5), p.text2, rect.right() - x - 8.0)
    });
    painter.galley(Pos2::new(x, rect.top() + 24.0), detail_g, p.text2);
    if let Step::Working(Some(frac)) = step {
        let bar = Rect::from_min_size(Pos2::new(rect.right() - 170.0, rect.top() + 11.0), Vec2::new(160.0, 6.0));
        painter.rect_filled(bar, 3.0, p.line);
        let mut done = bar;
        done.set_width((bar.width() * frac).max(6.0));
        painter.rect_filled(done, 3.0, p.accent);
    }
    if !last {
        painter.hline(x..=rect.right(), rect.bottom() - 0.5, Stroke::new(1.0, p.line));
    }
}

fn primary_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::new(14.0, semibold()), p.on_accent);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x + 40.0, 40.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, text));
    let fill = if resp.hovered() { theme::mix(p.accent, Color32::BLACK, 0.10) } else { p.accent };
    ui.painter().rect_filled(rect, 10.0, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.on_accent);
    let _ = Align::Center;
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
