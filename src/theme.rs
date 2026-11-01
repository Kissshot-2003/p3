//! Amber phosphor palette and font setup.
//!
//! The screen is monochrome on purpose: every ANSI colour is folded back into
//! one amber ramp by luminance, so a program that prints 24-bit RGB still looks
//! like it is running on a P3 phosphor tube.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily};

pub const BG: Color32 = Color32::from_rgb(0x14, 0x0d, 0x03);
pub const BG_DEEP: Color32 = Color32::from_rgb(0x0a, 0x07, 0x02);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb0, 0x00);
pub const AMBER_HOT: Color32 = Color32::from_rgb(0xff, 0xe0, 0xa3);
pub const AMBER_DIM: Color32 = Color32::from_rgb(0xb8, 0x7c, 0x05);
pub const AMBER_FAINT: Color32 = Color32::from_rgb(0x6b, 0x43, 0x0c);
pub const AMBER_DEEP: Color32 = Color32::from_rgb(0x4d, 0x32, 0x08);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0x6a, 0x00);

/// Interpolate the amber ramp: 0.0 is nearly black, 1.0 is the hot highlight.
pub fn tone(luma: f32) -> Color32 {
    let l = luma.clamp(0.0, 1.0);
    let (a, b, t) = if l < 0.45 {
        (AMBER_DEEP, AMBER_DIM, l / 0.45)
    } else if l < 0.8 {
        (AMBER_DIM, AMBER, (l - 0.45) / 0.35)
    } else {
        (AMBER, AMBER_HOT, (l - 0.8) / 0.2)
    };
    Color32::from_rgb(
        lerp(a.r(), b.r(), t),
        lerp(a.g(), b.g(), t),
        lerp(a.b(), b.b(), t),
    )
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8
}

fn grad(r: u8, g: u8, b: u8) -> Color32 {
    tone(0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32)
}

/// ANSI 16-colour set, pre-folded onto the amber ramp so the common colours are
/// stable across frames instead of drifting with the float maths.
const ANSI16: [Color32; 16] = [
    Color32::from_rgb(0x3a, 0x26, 0x08), // black
    Color32::from_rgb(0xff, 0x6a, 0x00), // red
    Color32::from_rgb(0xff, 0xb0, 0x00), // green
    Color32::from_rgb(0xff, 0xd4, 0x79), // yellow
    Color32::from_rgb(0xe0, 0x8a, 0x00), // blue
    Color32::from_rgb(0xff, 0x95, 0x00), // magenta
    Color32::from_rgb(0xff, 0xcc, 0x80), // cyan
    Color32::from_rgb(0xff, 0xb0, 0x00), // white
    Color32::from_rgb(0x7a, 0x4d, 0x0e), // bright black
    Color32::from_rgb(0xff, 0x8a, 0x3d), // bright red
    Color32::from_rgb(0xff, 0xc0, 0x4d), // bright green
    Color32::from_rgb(0xff, 0xe0, 0xa3), // bright yellow
    Color32::from_rgb(0xff, 0xa7, 0x33), // bright blue
    Color32::from_rgb(0xff, 0xab, 0x5c), // bright magenta
    Color32::from_rgb(0xff, 0xdd, 0xb0), // bright cyan
    Color32::from_rgb(0xff, 0xf0, 0xd0), // bright white
];

pub fn ansi(c: vt100::Color) -> Color32 {
    match c {
        vt100::Color::Default => AMBER,
        vt100::Color::Idx(i) if (i as usize) < 16 => ANSI16[i as usize],
        vt100::Color::Idx(i) => {
            if i >= 232 {
                /* 24-step grey ramp */
                let v = ((i as u32 - 232) * 255 / 23) as u8;
                return tone(v as f32 / 255.0 * 0.9);
            }
            let n = i as u32 - 16;
            let r = (n / 36) % 6 * 51;
            let g = (n / 6) % 6 * 51;
            let b = n % 6 * 51;
            grad(r as u8, g as u8, b as u8)
        }
        vt100::Color::Rgb(r, g, b) => grad(r, g, b),
    }
}

/// Consolas for Latin, SimHei as the CJK fallback. Picked from disk rather than
/// bundled so the binary stays small; a missing font is not an error.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    #[cfg(windows)]
    const CANDIDATES: [&str; 4] = [
        "C:/Windows/Fonts/consola.ttf",
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/msyh.ttc",
    ];
    #[cfg(target_os = "macos")]
    const CANDIDATES: [&str; 2] = ["/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/Hiragino Sans GB.ttc"];
    #[cfg(all(unix, not(target_os = "macos")))]
    const CANDIDATES: [&str; 3] = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    ];

    let mut mono: Vec<String> = Vec::new();
    for (i, path) in CANDIDATES.iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = format!("f{i}");
        fonts
            .font_data
            .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        mono.push(name);
    }

    if !mono.is_empty() {
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .splice(0..0, mono.clone());
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .extend(mono);
    }

    ctx.set_fonts(fonts);
}

/// Everything egui draws needs a colour; keeping the visuals in one place means
/// the rail and the terminal never disagree about the palette.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = BG_DEEP;
    visuals.override_text_color = Some(AMBER);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, AMBER_FAINT);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, AMBER_DIM);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, AMBER);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, AMBER_HOT);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(0xff, 0xb0, 0x00, 0x44);
    ctx.set_visuals(visuals);
}
