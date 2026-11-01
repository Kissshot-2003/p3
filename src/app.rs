//! Window, layout and the terminal renderer.
//!
//! The render pass walks the vt100 grid once per frame and merges adjacent cells
//! that share a foreground colour into a single glyph run — a full 200×55 screen
//! is a few hundred draw calls instead of twelve thousand.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Id, LayerId, Margin,
    Order, Rect, Sense, Shape, Stroke, StrokeKind, ViewportCommand, pos2, vec2,
};

use crate::input;
use crate::pty::{PtySession, ShellSpec};
use crate::term::Term;
use crate::theme;

const PAD: f32 = 8.0;
const SCAN_STEP: f32 = 3.0;
const BAR_H: f32 = 24.0;
/// Thickness of the invisible resize border along the window edge.
const EDGE_HIT: f32 = 7.0;

pub struct App {
    term: Term,
    pty: Option<PtySession>,
    font_size: f32,
    started: Instant,
    status: String,
    cwd: String,
    scanlines: bool,
    dump_tick: u128,
    rx_total: usize,
    last_chunk: Vec<u8>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::IMEAllowed(true));

        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let spec = ShellSpec::detect();

        let (pty, status) = match PtySession::spawn(&cc.egui_ctx, spec, 45, 130, &cwd) {
            Ok(p) => (Some(p), String::new()),
            Err(e) => (None, e),
        };

        Self {
            term: Term::new(45, 130),
            pty,
            font_size: 15.0,
            started: Instant::now(),
            status,
            cwd,
            scanlines: true,
            dump_tick: 0,
            rx_total: 0,
            last_chunk: Vec::new(),
        }
    }

    fn pump_pty(&mut self) {
        let Self { pty, term, status, rx_total, last_chunk, .. } = self;
        let Some(p) = pty.as_mut() else { return };

        let mut got = 0usize;
        let mut reply = Vec::new();
        p.drain(&mut |chunk| {
            got += chunk.len();
            *last_chunk = chunk.to_vec();
            term.process(chunk);
            if let Some(r) = term.respond(chunk) {
                reply.extend_from_slice(&r);
            }
        });
        *rx_total += got;
        /* the shell may be blocked on a query, so answer before anything else */
        if !reply.is_empty() {
            p.write(&reply);
        }
        p.poll_exit();
        if p.exited && status.is_empty() {
            *status = format!(
                "会话已退出 · exit {} · 按 F5 重开",
                p.exit_code.unwrap_or(-1)
            );
        }
    }

    fn respawn(&mut self, ctx: &egui::Context) {
        if let Some(mut p) = self.pty.take() {
            p.kill();
        }
        let cwd = self.cwd.clone();
        match PtySession::spawn(ctx, ShellSpec::detect(), self.term.rows, self.term.cols, &cwd) {
            Ok(p) => {
                self.pty = Some(p);
                self.status.clear();
                self.term = Term::new(self.term.rows, self.term.cols);
            }
            Err(e) => self.status = e,
        }
    }

    /* ------------------------------------------------------------------ hud */

    fn hud(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let mut hit: Option<WBtn> = None;

        egui::Panel::top("bar")
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(Margin { left: 6, right: 6, top: 3, bottom: 3 }),
            )
            .show(ui, |ui| {
                let bar = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), BAR_H - 6.0));
                let drag = ui.interact(bar, Id::unique("amber.titlebar"), Sense::click_and_drag());

                ui.horizontal(|ui| {
                    if !self.status.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.status)
                                .color(theme::WARN)
                                .size(11.0),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(b) = window_buttons(ui, maximized) {
                            hit = Some(b);
                        }
                    });
                });

                if drag.drag_started() {
                    let near_edge = ctx.pointer_latest_pos().is_some_and(|p| {
                        let vp = ctx.viewport_rect();
                        p.x - vp.left() < EDGE_HIT
                            || vp.right() - p.x < EDGE_HIT
                            || p.y - vp.top() < EDGE_HIT
                    });
                    if !near_edge {
                        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                }
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                }
            });

        match hit {
            Some(WBtn::Min) => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
            Some(WBtn::Max) => ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized)),
            Some(WBtn::Close) => ctx.send_viewport_cmd(ViewportCommand::Close),
            None => {}
        }
    }

    /* ----------------------------------------------------------- window chrome */

    /// The OS title bar is gone, so the resize borders have to be rebuilt by
    /// hand: a thin hot zone along each edge that starts an OS-level resize.
    fn resize_edges(ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            return;
        }
        let rect = ctx.viewport_rect();
        let Some(p) = ctx.pointer_latest_pos() else { return };
        if !rect.contains(p) {
            return;
        }
        let w = p.x - rect.left() < EDGE_HIT;
        let e = rect.right() - p.x < EDGE_HIT;
        let n = p.y - rect.top() < EDGE_HIT;
        let s = rect.bottom() - p.y < EDGE_HIT;
        if !(w || e || n || s) {
            return;
        }

        let (dir, cursor) = match (n, s, w, e) {
            (true, _, true, _) => (egui::ResizeDirection::NorthWest, CursorIcon::ResizeNwSe),
            (true, _, _, true) => (egui::ResizeDirection::NorthEast, CursorIcon::ResizeNeSw),
            (_, true, true, _) => (egui::ResizeDirection::SouthWest, CursorIcon::ResizeNeSw),
            (_, true, _, true) => (egui::ResizeDirection::SouthEast, CursorIcon::ResizeNwSe),
            (true, _, _, _) => (egui::ResizeDirection::North, CursorIcon::ResizeVertical),
            (_, true, _, _) => (egui::ResizeDirection::South, CursorIcon::ResizeVertical),
            (_, _, true, _) => (egui::ResizeDirection::West, CursorIcon::ResizeHorizontal),
            (_, _, _, true) => (egui::ResizeDirection::East, CursorIcon::ResizeHorizontal),
            _ => return,
        };
        ctx.set_cursor_icon(cursor);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }

    /// Draws the tube surround: one dim amber hairline against the black, plus a
    /// blackout ring just inside it, so the window reads as a bezel instead of a
    /// rectangle floating on the desktop.
    fn chrome(ctx: &egui::Context) {
        let rect = ctx.viewport_rect();
        let p = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("amber.chrome")));
        p.rect_stroke(
            rect.shrink(0.5),
            CornerRadius::ZERO,
            Stroke::new(1.0, theme::AMBER_DEEP),
            StrokeKind::Inside,
        );
        p.rect_stroke(
            rect.shrink(1.5),
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_black_alpha(170)),
            StrokeKind::Inside,
        );
    }


    /* -------------------------------------------------------------- terminal */

    fn terminal(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let font = FontId::new(self.font_size, FontFamily::Monospace);
        let (cw, rh) = ui.ctx().fonts_mut(|f| {
            (
                f.glyph_width(&font, 'M').max(1.0),
                f.row_height(&font).max(1.0),
            )
        });

        let inner = rect.shrink(PAD);
        let cols = ((inner.width() / cw).floor() as i64).clamp(8, 1000) as u16;
        let rows = ((inner.height() / rh).floor() as i64).clamp(4, 400) as u16;

        if cols != self.term.cols || rows != self.term.rows {
            self.term.resize(rows, cols);
            if let Some(p) = self.pty.as_mut() {
                p.resize(rows, cols);
            }
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, theme::BG);

        let screen = self.term.screen();
        let rows = self.term.rows;
        let cols = self.term.cols;

        for r in 0..rows {
            let y = inner.top() + r as f32 * rh;
            let mut run = String::new();
            let mut run_x = inner.left();
            let mut run_fg = theme::AMBER;
            let mut started = false;

            for c in 0..cols {
                let Some(cell) = screen.cell(r, c) else { continue };

                let mut fg = theme::ansi(cell.fgcolor());
                let mut bg = if matches!(cell.bgcolor(), vt100::Color::Default) {
                    None
                } else {
                    Some(theme::ansi(cell.bgcolor()))
                };
                if cell.inverse() {
                    let nb = Some(fg);
                    fg = bg.unwrap_or(theme::BG);
                    bg = nb;
                }
                if cell.dim() {
                    fg = fg.gamma_multiply(0.62);
                }
                if cell.bold() {
                    fg = fg.gamma_multiply(1.15);
                }

                if let Some(b) = bg {
                    painter.rect_filled(
                        Rect::from_min_size(
                            pos2(inner.left() + c as f32 * cw, y),
                            vec2(cw, rh),
                        ),
                        CornerRadius::ZERO,
                        b,
                    );
                }

                if !started || fg != run_fg {
                    flush(&painter, &mut run, run_x, y, rh, &font, run_fg);
                    run_x = inner.left() + c as f32 * cw;
                    run_fg = fg;
                    started = true;
                }
                run.push_str(&cell.contents());
            }
            flush(&painter, &mut run, run_x, y, rh, &font, run_fg);
        }

        /* caret: only at the live bottom, and only when the shell shows it */
        if self.term.scroll_offset == 0 && !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            if cr < rows && cc < cols {
                painter.rect_filled(
                    Rect::from_min_size(
                        pos2(inner.left() + cc as f32 * cw, inner.top() + cr as f32 * rh),
                        vec2(cw, rh),
                    ),
                    CornerRadius::ZERO,
                    theme::AMBER.gamma_multiply(0.55),
                );
            }
        }

        if self.scanlines {
            let mut shapes = Vec::with_capacity((rect.height() / SCAN_STEP) as usize + 2);
            let mut y = rect.top();
            let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 78));
            while y < rect.bottom() {
                shapes.push(Shape::line_segment(
                    [pos2(rect.left(), y), pos2(rect.right(), y)],
                    stroke,
                ));
                y += SCAN_STEP;
            }
            painter.extend(shapes);
        }

        /* vignette: four edge gradients are overkill here, one inner shadow is not */
        painter.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xff, 0xb0, 0x00, 0x14)),
            egui::StrokeKind::Inside,
        );

        self.dump(rect, inner, cw, rh);
    }

    /// `AMBER_DUMP=<path>` writes the current screen to disk twice a second.
    /// Useful for bug reports, and the only way to check the VT chain without
    /// eyeballing the window.
    fn dump(&mut self, rect: Rect, inner: Rect, cw: f32, rh: f32) {
        let Ok(path) = std::env::var("AMBER_DUMP") else { return };
        let tick = self.started.elapsed().as_millis() / 500;
        if tick == self.dump_tick {
            return;
        }
        self.dump_tick = tick;

        let screen = self.term.screen();
        let mut s = format!(
            "# grid {}x{}  cell {:.2}x{:.2}  rect {:.0}x{:.0}  inner {:.0}x{:.0}\n\
             # cursor {:?}  hidden {}  scroll {}  exited {}  rx {}  pid {:?}\n",
            self.term.cols,
            self.term.rows,
            cw,
            rh,
            rect.width(),
            rect.height(),
            inner.width(),
            inner.height(),
            screen.cursor_position(),
            screen.hide_cursor(),
            self.term.scroll_offset,
            self.pty.as_ref().map(|p| p.exited).unwrap_or(true),
            self.rx_total,
            self.pty.as_ref().and_then(|p| p.pid),
        );
        s.push_str(&format!("# last chunk {:?}\n", String::from_utf8_lossy(&self.last_chunk)));
        s.push_str(&screen.contents());
        let _ = std::fs::write(path, s);
    }
}

fn flush(
    painter: &egui::Painter,
    run: &mut String,
    x: f32,
    y: f32,
    rh: f32,
    font: &FontId,
    color: Color32,
) {
    if run.is_empty() {
        return;
    }
    let text = run.trim_end();
    if !text.is_empty() {
        painter.text(
            pos2(x, y + rh * 0.5),
            Align2::LEFT_CENTER,
            text,
            font.clone(),
            color,
        );
    }
    run.clear();
}

#[derive(Clone, Copy)]
enum WBtn {
    Min,
    Max,
    Close,
}

#[derive(Clone, Copy)]
enum Glyph {
    Min,
    Max,
    Restore,
    Close,
}

/// Minimise / maximise / close, drawn as line art instead of font glyphs — the
/// box-drawing characters that would spell these are a coin flip on whether the
/// mono font actually ships them.
fn window_buttons(ui: &mut egui::Ui, maximized: bool) -> Option<WBtn> {
    let mut hit = None;
    /* right-to-left layout: list them in reverse so close sits at the corner */
    let specs = [
        (WBtn::Close, Glyph::Close),
        (WBtn::Max, if maximized { Glyph::Restore } else { Glyph::Max }),
        (WBtn::Min, Glyph::Min),
    ];
    for (btn, glyph) in specs {
        let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 18.0), Sense::click());
        let close = matches!(btn, WBtn::Close);
        if resp.hovered() {
            ui.painter().rect_filled(
                rect.shrink(1.0),
                CornerRadius::ZERO,
                if close { theme::AMBER } else { theme::AMBER_DEEP },
            );
        }
        let col = match (resp.hovered(), close) {
            (true, true) => theme::BG_DEEP,
            (true, false) => theme::AMBER_HOT,
            _ => theme::AMBER_DEEP,
        };
        draw_glyph(ui.painter(), rect, glyph, col);
        if resp.clicked() {
            hit = Some(btn);
        }
    }
    hit
}

fn draw_glyph(p: &egui::Painter, rect: Rect, g: Glyph, col: Color32) {
    let c = rect.center();
    let st = Stroke::new(1.1, col);
    let s = 5.0;
    match g {
        Glyph::Min => {
            p.line_segment([pos2(c.x - s, c.y + 3.5), pos2(c.x + s, c.y + 3.5)], st);
        }
        Glyph::Max => {
            p.rect_stroke(
                Rect::from_center_size(c, vec2(s * 2.0, s * 1.6)),
                CornerRadius::ZERO,
                st,
                StrokeKind::Inside,
            );
        }
        Glyph::Restore => {
            let back = Rect::from_center_size(c - vec2(1.2, 1.2), vec2(s * 1.6, s * 1.2));
            let front = Rect::from_center_size(c + vec2(1.2, 1.2), vec2(s * 1.6, s * 1.2));
            p.rect_stroke(back, CornerRadius::ZERO, st, StrokeKind::Inside);
            p.rect_stroke(front, CornerRadius::ZERO, st, StrokeKind::Inside);
        }
        Glyph::Close => {
            p.line_segment([pos2(c.x - s, c.y - s), pos2(c.x + s, c.y + s)], st);
            p.line_segment([pos2(c.x + s, c.y - s), pos2(c.x - s, c.y + s)], st);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.pump_pty();

        let pump = input::pump(&ctx);
        match pump.zoom {
            1 => self.font_size = (self.font_size + 1.0).min(30.0),
            -1 => self.font_size = (self.font_size - 1.0).max(9.0),
            9 => self.font_size = 15.0,
            _ => {}
        }
        if pump.scroll != 0 {
            self.term.scroll(pump.scroll * 3);
        }
        if !pump.bytes.is_empty() {
            self.term.scroll_to_bottom();
            if let Some(p) = self.pty.as_mut() {
                p.write(&pump.bytes);
            }
        }
        if pump.toggle_scan {
            self.scanlines = !self.scanlines;
        }
        if pump.respawn {
            self.respawn(&ctx);
        }

        Self::resize_edges(&ctx);

        self.hud(ui);
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(theme::BG))
            .show(ui, |ui| self.terminal(ui));

        Self::chrome(&ctx);

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
