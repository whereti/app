//! The visual system: one palette per theme, built from the Windows accent
//! colour, and the egui style derived from it. Every colour the app paints
//! comes from here.

use eframe::egui::{self, style::ScrollStyle, Color32, CornerRadius, Margin, Shadow, Stroke, Theme, Vec2};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Window background.
    pub bg: Color32,
    /// Cards: search box, chips, menus.
    pub surface: Color32,
    pub hover: Color32,
    pub border: Color32,
    /// Hairlines inside a surface (dividers): fainter than `border`.
    pub line: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    pub accent: Color32,
    /// Accent tint behind a selected row.
    pub accent_soft: Color32,
    /// Text on a solid accent fill.
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub error: Color32,
}

impl Palette {
    pub fn new(dark: bool, accent: Color32) -> Palette {
        if dark {
            let accent = mix(accent, Color32::WHITE, 0.28); // readable on dark
            Palette {
                dark,
                bg: Color32::from_rgb(0x1A, 0x1A, 0x1D),
                surface: Color32::from_rgb(0x23, 0x23, 0x27),
                hover: Color32::from_rgb(0x2A, 0x2A, 0x2F),
                border: Color32::from_rgb(0x36, 0x36, 0x3C),
                line: Color32::from_rgb(0x2C, 0x2C, 0x31),
                text: Color32::from_rgb(0xF0, 0xF0, 0xF3),
                text2: Color32::from_rgb(0xA8, 0xA8, 0xB2),
                text3: Color32::from_rgb(0x78, 0x78, 0x82),
                accent,
                accent_soft: with_alpha(accent, 0.20),
                on_accent: Color32::from_rgb(0x10, 0x10, 0x14),
                ok: Color32::from_rgb(0x4C, 0xC3, 0x8A),
                warn: Color32::from_rgb(0xF0, 0xB4, 0x4C),
                error: Color32::from_rgb(0xF2, 0x6D, 0x6D),
            }
        } else {
            Palette {
                dark,
                bg: Color32::from_rgb(0xF7, 0xF7, 0xF9),
                surface: Color32::WHITE,
                hover: Color32::from_rgb(0xEE, 0xEE, 0xF2),
                border: Color32::from_rgb(0xE2, 0xE2, 0xE8),
                line: Color32::from_rgb(0xEA, 0xEA, 0xEF),
                text: Color32::from_rgb(0x16, 0x16, 0x1B),
                text2: Color32::from_rgb(0x5E, 0x5E, 0x6A),
                text3: Color32::from_rgb(0x92, 0x92, 0x9E),
                accent,
                accent_soft: with_alpha(accent, 0.12),
                on_accent: Color32::WHITE,
                ok: Color32::from_rgb(0x1F, 0x9D, 0x5C),
                warn: Color32::from_rgb(0xB7, 0x79, 0x0F),
                error: Color32::from_rgb(0xD1, 0x3B, 0x3B),
            }
        }
    }

    pub fn of(ctx: &egui::Context, accent: Color32) -> Palette {
        Palette::new(ctx.theme() == Theme::Dark, accent)
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0).round() as u8)
}

/// The Windows accent colour (Settings → Personalization → Colors), or the
/// default Windows blue.
pub fn system_accent() -> Color32 {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: valid out-pointers of the right size.
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    if ok.is_ok() {
        // stored as 0xAABBGGRR
        Color32::from_rgb(value as u8, (value >> 8) as u8, (value >> 16) as u8)
    } else {
        Color32::from_rgb(0x00, 0x67, 0xC0)
    }
}

/// Applies the palette to egui's own widgets (text field, scrollbars, menus).
pub fn apply(ctx: &egui::Context, accent: Color32) {
    for theme in [Theme::Light, Theme::Dark] {
        let p = Palette::new(theme == Theme::Dark, accent);
        let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = p.bg;
        v.window_fill = p.surface;
        v.extreme_bg_color = p.surface;
        v.text_edit_bg_color = Some(p.surface);
        v.faint_bg_color = p.hover;
        v.window_stroke = Stroke::new(1.0, p.border);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if p.dark { 110 } else { 38 }) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(if p.dark { 100 } else { 30 }) };
        v.selection.bg_fill = with_alpha(p.accent, if p.dark { 0.45 } else { 0.28 });
        v.selection.stroke = Stroke::new(1.0, p.text);
        v.hyperlink_color = p.accent;
        v.warn_fg_color = p.warn;
        v.error_fg_color = p.error;
        v.text_cursor.stroke = Stroke::new(2.0, p.accent);
        v.override_text_color = None;
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
        v.widgets.inactive.bg_fill = p.surface;
        v.widgets.inactive.weak_bg_fill = p.surface;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
        v.widgets.hovered.bg_fill = p.hover;
        v.widgets.hovered.weak_bg_fill = p.hover;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
        v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
        v.widgets.active.bg_fill = p.accent_soft;
        v.widgets.active.weak_bg_fill = p.accent_soft;
        v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
        v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
        ctx.set_visuals_of(theme, v);
        ctx.style_mut_of(theme, |s| {
            s.spacing.item_spacing = Vec2::new(8.0, 0.0);
            s.spacing.button_padding = Vec2::new(12.0, 6.0);
            s.spacing.window_margin = Margin::same(14);
            s.spacing.menu_margin = Margin::same(6);
            s.spacing.interact_size = Vec2::new(24.0, 28.0);
            s.spacing.scroll = ScrollStyle::floating();
        });
    }
}
