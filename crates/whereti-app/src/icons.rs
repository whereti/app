//! Interface icons, drawn as fine lines (1.5 px, round ends) at any size -
//! no image files to ship, sharp at every DPI. Drawn on a 24-unit grid like
//! common line icon sets, then scaled into the target rect.

use eframe::egui::{epaint::PathShape, epaint::PathStroke, Color32, Painter, Pos2, Rect, Stroke, Vec2};

/// The full set; not every icon is in use on every screen yet.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Search,
    Close,
    Settings,
    /// A window with a side pane (the preview toggle).
    Panel,
    Back,
    Folder,
    Document,
    Image,
    Music,
    Film,
    Clock,
    Lock,
    Plug,
    Sparkle,
    Heart,
    /// A question mark in a circle (the search commands).
    Help,
}

/// Draws `icon` centred in `rect` (the rect's smaller side is the icon size).
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    let k = size / 24.0;
    let o = rect.center() - Vec2::splat(12.0 * k);
    let p = |x: f32, y: f32| Pos2::new(o.x + x * k, o.y + y * k);
    let w = (1.5 * k).max(1.2);
    let s = Stroke::new(w, color);
    let line = |pts: &[(f32, f32)]| {
        let points: Vec<Pos2> = pts.iter().map(|&(x, y)| p(x, y)).collect();
        painter.add(PathShape { points, closed: false, fill: Color32::TRANSPARENT, stroke: PathStroke::new(w, color) });
        // round ends
        for q in [pts[0], pts[pts.len() - 1]] {
            painter.circle_filled(p(q.0, q.1), w / 2.0, color);
        }
    };
    let closed = |pts: &[(f32, f32)]| {
        let points: Vec<Pos2> = pts.iter().map(|&(x, y)| p(x, y)).collect();
        painter.add(PathShape { points, closed: true, fill: Color32::TRANSPARENT, stroke: PathStroke::new(w, color) });
    };
    let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| -> Vec<(f32, f32)> {
        let n = ((a1 - a0).abs() / 0.18).ceil().max(2.0) as usize;
        (0..=n).map(|i| a0 + (a1 - a0) * i as f32 / n as f32).map(|a| (cx + r * a.cos(), cy + r * a.sin())).collect()
    };
    let rounded = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        painter.rect_stroke(Rect::from_min_max(p(x0, y0), p(x1, y1)), r * k, s, eframe::egui::StrokeKind::Middle);
    };
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    match icon {
        Icon::Heart => {
            // filled: two round lobes and a point, readable even at 14 px
            let r = 4.6 * k;
            painter.circle_filled(p(8.0, 9.2), r, color);
            painter.circle_filled(p(16.0, 9.2), r, color);
            painter.add(eframe::egui::Shape::convex_polygon(
                vec![p(3.55, 10.6), p(20.45, 10.6), p(12.0, 20.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Help => {
            painter.circle_stroke(p(12.0, 12.0), 9.0 * k, s);
            line(&[(9.4, 9.6), (9.9, 8.2), (11.0, 7.4), (12.4, 7.3), (13.8, 7.9), (14.6, 9.1), (14.4, 10.5), (13.3, 11.5), (12.3, 12.3), (12.0, 13.6)]);
            painter.circle_filled(p(12.0, 16.6), 1.1 * k, color);
        }
        Icon::Search => {
            painter.circle_stroke(p(10.5, 10.5), 6.5 * k, s);
            line(&[(15.4, 15.4), (20.0, 20.0)]);
        }
        Icon::Close => {
            line(&[(6.5, 6.5), (17.5, 17.5)]);
            line(&[(17.5, 6.5), (6.5, 17.5)]);
        }
        Icon::Settings => {
            // eight rounded teeth around a ring
            let mut pts = Vec::new();
            for t in 0..8 {
                let a = t as f32 * TAU / 8.0;
                for (da, r) in [(-0.26, 6.6), (-0.17, 9.0), (0.17, 9.0), (0.26, 6.6)] {
                    pts.push((12.0 + r * (a + da).cos(), 12.0 + r * (a + da).sin()));
                }
            }
            closed(&pts);
            painter.circle_stroke(p(12.0, 12.0), 3.0 * k, s);
        }
        Icon::Panel => {
            rounded(3.5, 5.0, 20.5, 19.0, 2.5);
            line(&[(14.0, 5.0), (14.0, 19.0)]);
        }
        Icon::Back => {
            line(&[(19.0, 12.0), (5.0, 12.0)]);
            line(&[(11.0, 6.0), (5.0, 12.0), (11.0, 18.0)]);
        }
        Icon::Folder => {
            closed(&[(3.5, 7.0), (3.5, 18.0), (20.5, 18.0), (20.5, 8.5), (11.5, 8.5), (9.5, 6.0), (3.5, 6.0)]);
        }
        Icon::Document => {
            closed(&[(6.0, 3.5), (14.0, 3.5), (18.5, 8.0), (18.5, 20.5), (6.0, 20.5)]);
            line(&[(14.0, 3.5), (14.0, 8.0), (18.5, 8.0)]);
            line(&[(9.0, 13.0), (15.5, 13.0)]);
            line(&[(9.0, 16.5), (13.5, 16.5)]);
        }
        Icon::Image => {
            rounded(3.5, 4.5, 20.5, 19.5, 2.5);
            painter.circle_stroke(p(9.0, 9.5), 1.6 * k, s);
            line(&[(4.0, 17.0), (9.5, 12.5), (13.0, 15.5), (16.0, 13.0), (20.0, 16.5)]);
        }
        Icon::Music => {
            line(&[(9.0, 17.0), (9.0, 5.5), (19.0, 4.0), (19.0, 15.5)]);
            painter.circle_stroke(p(6.8, 17.0), 2.3 * k, s);
            painter.circle_stroke(p(16.8, 15.5), 2.3 * k, s);
        }
        Icon::Film => {
            rounded(3.5, 5.0, 20.5, 19.0, 2.5);
            closed(&[(10.0, 9.0), (15.5, 12.0), (10.0, 15.0)]);
        }
        Icon::Clock => {
            painter.circle_stroke(p(12.0, 12.0), 8.5 * k, s);
            line(&[(12.0, 7.5), (12.0, 12.0), (15.0, 14.0)]);
        }
        Icon::Lock => {
            rounded(5.0, 10.5, 19.0, 20.0, 2.0);
            line(&arc(12.0, 10.5, 4.0, PI, TAU));
        }
        Icon::Plug => {
            line(&[(9.0, 3.5), (9.0, 8.0)]);
            line(&[(15.0, 3.5), (15.0, 8.0)]);
            let mut body = vec![(6.0, 8.0), (18.0, 8.0), (18.0, 11.0)];
            body.extend(arc(12.0, 11.0, 6.0, 0.0, PI));
            body.push((6.0, 8.0));
            closed(&body);
            line(&[(12.0, 17.0), (12.0, 20.5)]);
        }
        Icon::Sparkle => {
            let star = |cx: f32, cy: f32, r: f32| {
                let pts: Vec<(f32, f32)> = (0..8)
                    .map(|i| {
                        let a = i as f32 * FRAC_PI_2 / 2.0 - FRAC_PI_2;
                        let rr = if i % 2 == 0 { r } else { r * 0.32 };
                        (cx + rr * a.cos(), cy + rr * a.sin())
                    })
                    .collect();
                closed(&pts);
            };
            star(10.5, 12.5, 7.0);
            star(18.0, 6.0, 3.0);
        }
    }
}
