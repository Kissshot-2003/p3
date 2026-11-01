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

const PAD: f32 = 10.0;
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
    shot: Option<String>,
    shot_fired: bool,
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

        let mut term = Term::new(45, 130);
        if std::env::var("AMBER_SHOT").is_ok() {
            /* only for screenshots: a fixed screen beats a lone prompt */
            term.process(DEMO.as_bytes());
        }

        let (pty, status) = match PtySession::spawn(&cc.egui_ctx, spec, 45, 130, &cwd) {
            Ok(p) => (Some(p), String::new()),
            Err(e) => (None, e),
        };

        Self {
            term,
            pty,
            font_size: 16.0,
            started: Instant::now(),
            status,
            cwd,
            scanlines: true,
            dump_tick: 0,
            rx_total: 0,
            last_chunk: Vec::new(),
            shot: std::env::var("AMBER_SHOT").ok(),
            shot_fired: false,
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

    /// `AMBER_SHOT=<file.bmp>` grabs one frame and quits. Lets the renderer be
    /// checked without a display in front of you.
    fn grab(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot.clone() else { return };
        if !self.shot_fired && self.started.elapsed().as_millis() > 1500 {
            self.shot_fired = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |img| write_bmp(&path, &img)),
            ));
        }
        if self.shot_fired {
            let tmp = format!("{}.tmp", self.shot.as_deref().unwrap_or_default());
            if std::path::Path::new(&tmp).exists() {
                let _ = std::fs::rename(&tmp, self.shot.as_deref().unwrap_or_default());
            }
            if std::path::Path::new(self.shot.as_deref().unwrap_or_default()).exists() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
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

        /* boot: the picture opens from a horizontal line, like the tube warming */
        let t = self.started.elapsed().as_secs_f32();
        let open = ease_out((t / 0.55).clamp(0.0, 1.0));
        let vis = if open < 1.0 {
            let h = (rect.height() * open).max(1.5);
            Rect::from_center_size(rect.center(), vec2(rect.width(), h))
        } else {
            rect
        };

        let painter = ui.painter_at(vis);
        painter.rect_filled(vis, CornerRadius::ZERO, theme::SCREEN);

        let screen = self.term.screen();
        let rows = self.term.rows;
        let cols = self.term.cols;
        /* phosphor brightness is never perfectly steady */
        let flick = 1.0 + 0.016 * (t * 43.0).sin() + 0.010 * (t * 7.3).sin();

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
                        b.gamma_multiply(flick),
                    );
                }

                if !started || fg != run_fg {
                    flush(
                        &painter,
                        &mut run,
                        run_x,
                        y,
                        rh,
                        &font,
                        run_fg.gamma_multiply(flick * 1.22),
                    );
                    run_x = inner.left() + c as f32 * cw;
                    run_fg = fg;
                    started = true;
                }
                run.push_str(&cell.contents());
            }
            flush(
                &painter,
                &mut run,
                run_x,
                y,
                rh,
                &font,
                run_fg.gamma_multiply(flick * 1.22),
            );
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
                    theme::AMBER.gamma_multiply(0.55 * flick),
                );
            }
        }

        if self.scanlines {
            let mut shapes = Vec::with_capacity((vis.height() / SCAN_STEP) as usize + 2);
            let mut y = vis.top();
            /* the gap lines are what sell the tube; hard edges, no blur */
            let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 40));
            while y < vis.bottom() {
                shapes.push(Shape::line_segment(
                    [pos2(vis.left(), y), pos2(vis.right(), y)],
                    stroke,
                ));
                y += SCAN_STEP;
            }
            painter.extend(shapes);
        }

        /* vertical sync drift: a soft bright band crawling down the tube */
        if open >= 1.0 {
            let cycle = 13.0;
            let band = vis.height() * 0.16;
            let cy = vis.top() - band + (t % cycle) / cycle * (vis.height() + band * 2.0);
            let half = band * 0.5;
            quad_grad(
                &painter,
                [
                    pos2(vis.left(), cy - half),
                    pos2(vis.right(), cy - half),
                    pos2(vis.right(), cy),
                    pos2(vis.left(), cy),
                ],
                [
                    Color32::TRANSPARENT,
                    Color32::TRANSPARENT,
                    Color32::from_rgba_unmultiplied(0xff, 0xd8, 0x90, 9),
                    Color32::from_rgba_unmultiplied(0xff, 0xd8, 0x90, 9),
                ],
            );
            quad_grad(
                &painter,
                [
                    pos2(vis.left(), cy),
                    pos2(vis.right(), cy),
                    pos2(vis.right(), cy + half),
                    pos2(vis.left(), cy + half),
                ],
                [
                    Color32::from_rgba_unmultiplied(0xff, 0xd8, 0x90, 9),
                    Color32::from_rgba_unmultiplied(0xff, 0xd8, 0x90, 9),
                    Color32::TRANSPARENT,
                    Color32::TRANSPARENT,
                ],
            );
        }

        vignette(&painter, vis);

        /* glass: one soft highlight in the top-left, the way a curved face
        catches a room light */
        quad_grad(
            &painter,
            [vis.left_top(), vis.right_top(), vis.right_bottom(), vis.left_bottom()],
            [
                Color32::from_rgba_unmultiplied(0xff, 0xe6, 0xc0, 15),
                Color32::TRANSPARENT,
                Color32::TRANSPARENT,
                Color32::from_rgba_unmultiplied(0xff, 0xe6, 0xc0, 4),
            ],
        );

        if open < 1.0 {
            let glow = 255.0 * (1.0 - open);
            let line = Stroke::new(
                1.5,
                Color32::from_rgba_unmultiplied(0xff, 0xe0, 0xa3, glow as u8),
            );
            painter.line_segment([vis.left_bottom(), vis.right_bottom()], line);
            painter.line_segment([vis.left_top(), vis.right_top()], line);
        }

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
        let base = pos2(x, y + rh * 0.5);
        /* bloom on the cheap: two dim copies either side of the glyphs, plus a
        half-pixel repeat so thin pixel lettering does not read as washed out */
        let halo = color.gamma_multiply(0.20);
        let bloom = color.gamma_multiply(0.13);
        painter.text(base + vec2(-2.6, 0.0), Align2::LEFT_CENTER, text, font.clone(), bloom);
        painter.text(base + vec2(2.6, 0.0), Align2::LEFT_CENTER, text, font.clone(), bloom);
        painter.text(base + vec2(-1.4, 0.0), Align2::LEFT_CENTER, text, font.clone(), halo);
        painter.text(base + vec2(1.4, 0.0), Align2::LEFT_CENTER, text, font.clone(), halo);
        painter.text(base + vec2(0.5, 0.0), Align2::LEFT_CENTER, text, font.clone(), color);
        painter.text(base, Align2::LEFT_CENTER, text, font.clone(), color);
    }
    run.clear();
}

const DEMO: &str = "\
\x1b[1mAMBER TERMINAL\x1b[0m   phosphor test pattern\r\n\
\r\n\
[  OK  ] warm boot ................. 1.42 s\r\n\
[  OK  ] tube bias ................ 62 V\r\n\
[ WARN ] no such device: /dev/radio0\r\n\
\r\n\
  console.cpp          1942\r\n\
  graphics.lib        12003\r\n\
  radio.bin           95911\r\n\
\r\n\
  colours 31-36: \x1b[31m31 \x1b[32m32 \x1b[33m33 \x1b[34m34 \x1b[35m35 \x1b[36m36\x1b[0m\r\n\
  dim \x1b[2mthis line is dim\x1b[0m   bold \x1b[1mthis line is bold\x1b[0m\r\n\
  inverse \x1b[7m selected block \x1b[0m   underline \x1b[4munderlined\x1b[0m\r\n\
\r\n\
  中文输出：寿县项目 工程量清单 组价 2026-11-02\r\n\
\r\n";

/// 32-bit BMP: no encoder dependency, and every image viewer reads it.
fn write_bmp(path: &str, img: &egui::ColorImage) {
    let (w, h) = (img.size[0], img.size[1]);
    let row = w * 4;
    let pad = (4 - row % 4) % 4;
    let data_len = (row + pad) * h;
    let mut out = Vec::with_capacity(54 + data_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.pixels[y * w + x];
            out.push(p.b());
            out.push(p.g());
            out.push(p.r());
            out.push(255);
        }
        for _ in 0..pad {
            out.push(0);
        }
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// One quad with per-corner colours. egui has no gradient primitive, but a
/// vertex-coloured mesh is exactly that and costs nothing.
fn quad_grad(p: &egui::Painter, pts: [egui::Pos2; 4], cols: [Color32; 4]) {
    let mut m = egui::Mesh::default();
    for (pt, c) in pts.into_iter().zip(cols) {
        m.colored_vertex(pt, c);
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

/// Darkens all four edges. Overlapping corners double up, which is what a real
/// tube does too.
fn vignette(p: &egui::Painter, rect: Rect) {
    let d = (rect.width().min(rect.height()) * 0.13).max(20.0);
    let dark = Color32::from_black_alpha(34);
    let clear = Color32::TRANSPARENT;
    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    quad_grad(
        p,
        [pos2(l, t), pos2(r, t), pos2(r, t + d), pos2(l, t + d)],
        [dark, dark, clear, clear],
    );
    quad_grad(
        p,
        [pos2(l, b - d), pos2(r, b - d), pos2(r, b), pos2(l, b)],
        [clear, clear, dark, dark],
    );
    quad_grad(
        p,
        [pos2(l, t), pos2(l + d, t), pos2(l + d, b), pos2(l, b)],
        [dark, clear, clear, dark],
    );
    quad_grad(
        p,
        [pos2(r - d, t), pos2(r, t), pos2(r, b), pos2(r - d, b)],
        [clear, dark, dark, clear],
    );
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
            _ => theme::AMBER_FAINT,
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
            1 => self.font_size = (self.font_size + 2.0).min(32.0),
            -1 => self.font_size = (self.font_size - 2.0).max(10.0),
            9 => self.font_size = 16.0,
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
        self.grab(&ctx);

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
