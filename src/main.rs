//! P3 — an amber phosphor terminal, native.
//!
//! Named after the phosphor: P1 is the green one, P3 is the amber one, P4 the white.

mod app;
mod input;
mod pty;
mod term;
mod theme;

use eframe::egui;

fn main() -> eframe::Result<()> {
    /* Replace eframe's default icon: the bundled 32px pixel-art tube matches
    the amber cabinet drawn in app.rs, and is an exact 2x scale of the 16px
    grid so the title bar stays crisp. */
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("bundled P3 icon must be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("P3")
            .with_icon(icon)
            /* no OS title bar: a white chrome strip around a black tube is the
            one thing the phosphor look cannot survive */
            .with_decorations(false)
            .with_resizable(true)
            .with_transparent(false),
        ..Default::default()
    };

    eframe::run_native(
        "p3",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)) as Box<dyn eframe::App>)),
    )
}
