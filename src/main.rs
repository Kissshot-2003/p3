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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("P3")
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
