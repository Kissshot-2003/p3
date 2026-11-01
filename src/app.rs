//! Window, layout and the terminal renderer.
//!
//! The render pass walks the vt100 grid once per frame and merges adjacent cells
//! that share a foreground colour into a single glyph run — a full 200×55 screen
//! is a few hundred draw calls instead of twelve thousand.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Id, LayerId, Margin,
    Order, Rect, Sense, Shape, Stroke, StrokeKind, ViewportCommand, pos2, vec2,
};

use crate::input;
use crate::pty::{PtySession, ShellSpec};
use crate::term::Term;
use crate::theme;

const PAD: f32 = 10.0;
const SCAN_STEP: f32 = 3.0;
/// Line pitch as a multiple of the font's own row height. 1.0 is the font's
/// idea of a line, which for a pixel face means the descenders touch.
const LINE_LEAD: f32 = 1.22;
const BAR_H: f32 = 24.0;
/// Thickness of the cabinet face visible around the glass.
const BEZEL: f32 = 22.0;
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
    /// Anchor and head of the text selection, in (row, col) screen cells. The
    /// anchor is where the drag started, the head follows the pointer, so a
    /// selection can run backwards. Only screen cells, not scrollback.
    sel: Option<((u16, u16), (u16, u16))>,
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
        /* Screenshot mode drives the screen from `DEMO` alone and never spawns a
        shell: a live prompt drags the real cwd into the picture, and it makes the
        frame depend on whatever the machine happened to print. */
        let (pty, status) = if std::env::var("P3_SHOT").is_ok() {
            term.process(DEMO.as_bytes());
            (None, String::new())
        } else {
            match PtySession::spawn(&cc.egui_ctx, spec, 45, 130, &cwd) {
                Ok(p) => (Some(p), String::new()),
                Err(e) => (None, e),
            }
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
            shot: std::env::var("P3_SHOT").ok(),
            shot_fired: false,
            sel: None,
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

    /// Ctrl+Shift+C. Returns whether anything actually went to the clipboard, so
    /// the caller can drop the marks and give the screen back.
    fn copy_selection(&self, ctx: &egui::Context) -> bool {
        let Some((a, b)) = self.sel else { return false };
        let text = selection_text(self.term.screen(), a, b);
        if text.is_empty() {
            return false;
        }
        ctx.copy_text(text);
        true
    }

    /* ------------------------------------------------------------------ hud */

    fn hud(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let mut hit: Option<WBtn> = None;

        egui::Panel::top("bar")
            .frame(
                egui::Frame::NONE
                    .fill(theme::CABINET_TOP)
                    .inner_margin(Margin { left: 6, right: 6, top: 3, bottom: 3 }),
            )
            /* no hairline under the plate: the cabinet is one piece of plastic */
            .show_separator_line(false)
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

    /// `P3_SHOT=<file.bmp>` grabs one frame and quits. Lets the renderer be
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
        /* Cell width is half a Han glyph, not the Latin advance. A terminal cell
        has to hold one Latin char or half a Han char, and the two fonts in the
        chain disagree: VT323 advances 0.40 em, the CJK fallback advances 1.00 em.
        Taking the Latin advance as the cell makes every Chinese run drift 0.25 of
        a cell per character and the whole screen look non-monospaced. */
        let (adv, wide, rh0) = ui.ctx().fonts_mut(|f| {
            (
                f.glyph_width(&font, 'M').max(1.0),
                f.glyph_width(&font, '中').max(1.0),
                f.row_height(&font).max(1.0),
            )
        });
        let cw = if wide > adv * 1.5 { wide / 2.0 } else { adv * 1.25 };
        let rh = rh0 * LINE_LEAD;
        /* stretch the Latin advance out to the cell so letters stop touching */
        let narrow_gap = (cw - adv).max(0.0);

        let glass = rect.shrink(BEZEL);
        let inner = glass.shrink(PAD);
        let cols = ((inner.width() / cw).floor() as i64).clamp(8, 1000) as u16;
        let rows = ((inner.height() / rh).floor() as i64).clamp(4, 400) as u16;

        if cols != self.term.cols || rows != self.term.rows {
            self.term.resize(rows, cols);
            if let Some(p) = self.pty.as_mut() {
                p.resize(rows, cols);
            }
            self.sel = None;
        }

        /* The shell owns the cursor, so the mouse does one job only: marking
        text. No click-to-position — that would need the terminal to feed the
        shell arrow-key traffic and it fights PSReadLine's own redraw. */
        let cols = self.term.cols;
        let rows = self.term.rows;
        let resp = ui.interact(inner, Id::unique("p3.screen"), Sense::click_and_drag());
        let cell_at = |p: egui::Pos2| -> (u16, u16) {
            let r = ((p.y - inner.top()) / rh).floor();
            let c = ((p.x - inner.left()) / cw).floor();
            (
                r.clamp(0.0, rows.saturating_sub(1) as f32) as u16,
                c.clamp(0.0, cols.saturating_sub(1) as f32) as u16,
            )
        };
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let cell = cell_at(p);
                self.sel = Some((cell, cell));
            }
        } else if resp.dragged() {
            if let (Some(p), Some((anchor, _))) = (resp.interact_pointer_pos(), self.sel) {
                self.sel = Some((anchor, cell_at(p)));
            }
        } else if resp.clicked() {
            self.sel = None;
        }
        let sel = self.sel;

        /* boot: the picture opens from a horizontal line, like the tube warming */
        let t = self.started.elapsed().as_secs_f32();
        let open = ease_out((t / 0.55).clamp(0.0, 1.0));
        let vis = if open < 1.0 {
            let h = (glass.height() * open).max(1.5);
            Rect::from_center_size(glass.center(), vec2(glass.width(), h))
        } else {
            glass
        };

        let painter = ui.painter_at(rect);
        cabinet(&painter, rect, glass, t);
        painter.rect_filled(vis, CornerRadius::ZERO, theme::SCREEN);
        haze(&painter, vis);

        let screen = self.term.screen();
        let rows = self.term.rows;
        let cols = self.term.cols;
        /* phosphor brightness is never perfectly steady */
        let flick = 1.0 + 0.016 * (t * 43.0).sin() + 0.010 * (t * 7.3).sin();
        /* horizontal sync: now and then the tube loses the line and the picture
        steps sideways for a frame or two */
        let dx = {
            let beat = (t % 9.3) / 9.3;
            if beat < 0.007 {
                2.5
            } else if beat < 0.012 {
                -1.5
            } else {
                0.0
            }
        };

        for r in 0..rows {
            let y = inner.top() + r as f32 * rh;
            let mut run = String::new();
            let mut run_x = inner.left();
            let mut run_fg = theme::AMBER;
            /* a run is either all latin or all Han: only the latin one needs the
            gap that pushes its advance out to a full cell */
            let mut run_wide = false;
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

                /* marked cells go amber-on-dark: the phosphor inverts, which is
                what a tube does when the beam is held, and it stays readable */
                if in_selection(sel, r, c) {
                    fg = theme::SCREEN;
                    bg = Some(theme::AMBER);
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

                /* vt100 stores a blank cell as zero bytes, so `contents()` gives
                "". Dropping it silently shortens the run by one cell, and every
                character after an ASCII space inside a latin run slides left by a
                column — `组价 2026` came out as `组价2026`. A blank cell is still a
                cell: it contributes one space to the run. */
                let raw = cell.contents();
                let text = if raw.is_empty() { " " } else { raw };
                let wide_char = text.chars().next().is_some_and(is_wide);

                if !started || fg != run_fg || wide_char != run_wide {
                    flush(
                        &painter,
                        &mut run,
                        run_x + dx,
                        Row { y, h: rh },
                        Style { font: &font, spacing: if run_wide { 0.0 } else { narrow_gap } },
                        run_fg.gamma_multiply(flick * 1.22),
                    );
                    run_x = inner.left() + c as f32 * cw;
                    run_fg = fg;
                    run_wide = wide_char;
                    started = true;
                }
                run.push_str(text);
            }
            flush(
                &painter,
                &mut run,
                run_x + dx,
                Row { y, h: rh },
                Style { font: &font, spacing: if run_wide { 0.0 } else { narrow_gap } },
                run_fg.gamma_multiply(flick * 1.22),
            );
        }

        /* caret: only at the live bottom, and only when the shell shows it */
        if self.term.scroll_offset == 0 && !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            if cr < rows && cc < cols {
                painter.rect_filled(
                    Rect::from_min_size(
                        pos2(inner.left() + cc as f32 * cw + dx, inner.top() + cr as f32 * rh),
                        vec2(cw, rh),
                    ),
                    CornerRadius::ZERO,
                    theme::AMBER.gamma_multiply(0.55 * flick),
                );
            }
        }

        if self.scanlines {
            /* The gap rows are what sell the tube. They are filled bands snapped
            to whole device pixels — a stroked line gets antialiased across two
            rows and the contrast halves, which is how you end up with a picture
            that just looks like amber text on black. */
            let ppp = ui.ctx().pixels_per_point().max(0.5);
            let step = ((SCAN_STEP * ppp).round().max(2.0)) / ppp;
            let h = (1.0 / ppp).max(0.5);
            let mut shapes = Vec::with_capacity((vis.height() / step) as usize + 2);
            let mut y = (vis.top() * ppp).round() / ppp;
            while y < vis.bottom() {
                shapes.push(Shape::rect_filled(
                    Rect::from_min_max(pos2(vis.left(), y), pos2(vis.right(), y + h)),
                    CornerRadius::ZERO,
                    Color32::from_black_alpha(124),
                ));
                y += step;
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
        glass_rim(&painter, vis);

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

    /// `P3_DUMP=<path>` writes the current screen to disk twice a second.
    /// Useful for bug reports, and the only way to check the VT chain without
    /// eyeballing the window.
    fn dump(&mut self, rect: Rect, inner: Rect, cw: f32, rh: f32) {
        let Ok(path) = std::env::var("P3_DUMP") else { return };
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

/// One screen row: where it starts and how tall it is. Bundled because they
/// always travel together and a bare pair of floats reads as neither.
#[derive(Clone, Copy)]
struct Row {
    y: f32,
    h: f32,
}

/// What a run of glyphs needs besides its position: the face, and the letter gap
/// that stretches the Latin advance out to a full cell.
#[derive(Clone, Copy)]
struct Style<'a> {
    font: &'a FontId,
    spacing: f32,
}

fn flush(
    painter: &egui::Painter,
    run: &mut String,
    x: f32,
    row: Row,
    style: Style<'_>,
    color: Color32,
) {
    if run.is_empty() {
        return;
    }
    let text = run.trim_end();
    if !text.is_empty() {
        let galley = painter.layout_job(job(text, style.font, color, style.spacing));
        let top = pos2(x, row.y + (row.h - galley.size().y) * 0.5);
        /* bloom on the cheap: one wide dim copy each side for the halo the glass
        picks up, one tight pair to thicken the stroke. Any more than this and the
        letters turn into blobs and the scanline cuts stop reading. */
        let halo = color.gamma_multiply(0.26);
        let bloom = color.gamma_multiply(0.09);
        painter.galley_with_override_text_color(top + vec2(-2.4, 0.0), galley.clone(), bloom);
        painter.galley_with_override_text_color(top + vec2(2.4, 0.0), galley.clone(), bloom);
        painter.galley_with_override_text_color(top + vec2(-1.0, 0.0), galley.clone(), halo);
        painter.galley_with_override_text_color(top + vec2(1.0, 0.0), galley.clone(), halo);
        painter.galley_with_override_text_color(top + vec2(0.0, 0.5), galley.clone(), color);
        painter.galley_with_override_text_color(top, galley, color);
    }
    run.clear();
}

fn job(text: &str, font: &FontId, color: Color32, spacing: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color,
            extra_letter_spacing: spacing,
            ..Default::default()
        },
    );
    job
}

/// Is this cell inside the marked range? The range is a single run of cells, so
/// ordering both ends as (row, col) makes the middle rows fall out for free:
/// everything between the anchor row and the head row is included in full.
fn in_selection(sel: Option<((u16, u16), (u16, u16))>, r: u16, c: u16) -> bool {
    let Some((a, b)) = sel else { return false };
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    (r, c) >= start && (r, c) <= end
}

/// Pull the marked cells out of the grid as plain text, the way a terminal's
/// "copy" does: one line per row, trailing blanks dropped, no wrap markers.
fn selection_text(screen: &vt100::Screen, a: (u16, u16), b: (u16, u16)) -> String {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let last_col = screen.size().1.get().saturating_sub(1);
    let mut out = String::new();
    for r in start.0..=end.0 {
        let c0 = if r == start.0 { start.1 } else { 0 };
        let c1 = if r == end.0 { end.1 } else { last_col };
        let mut line = String::new();
        for c in c0..=c1 {
            let Some(cell) = screen.cell(r, c) else { continue };
            /* the second half of a Han glyph carries no text of its own */
            if cell.is_wide_continuation() {
                continue;
            }
            let text = cell.contents();
            line.push_str(if text.is_empty() { " " } else { text });
        }
        out.push_str(line.trim_end());
        if r != end.0 {
            out.push('\n');
        }
    }
    out
}

/// Anything that occupies two terminal cells: CJK, kana, Hangul, and the full
/// width forms. Good enough — this is the same test wcwidth does.
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

const DEMO: &str = "\
\x1b[1mP3\x1b[0m   phosphor test pattern\r\n\
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
\r\n\
  net   1042 up      0 down     12.4 ms\r\n\
  mem   3184 / 16384 MB         cpu  07 %\r\n\
  disk  C:  412 GB free of 1024 GB\r\n\
\r\n\
  flashing firmware  [##########----------]  52%\r\n\
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
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// The plastic around the tube: a top-lit cabinet, a black recess where the
/// glass meets it, and a hairline of room light on the rim.
fn cabinet(p: &egui::Painter, outer: Rect, glass: Rect, t: f32) {
    quad_grad(
        p,
        [
            outer.left_top(),
            outer.right_top(),
            outer.right_bottom(),
            outer.left_bottom(),
        ],
        [
            theme::CABINET_TOP,
            theme::CABINET_TOP,
            theme::CABINET_BOT,
            theme::CABINET_BOT,
        ],
    );

    /* the gap between plastic and glass is in shadow, always */
    p.rect_stroke(
        glass.expand(4.0),
        CornerRadius::ZERO,
        Stroke::new(6.0, Color32::from_black_alpha(238)),
        StrokeKind::Middle,
    );
    p.rect_stroke(
        glass.expand(7.5),
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xff, 0xc8, 0x78, 26)),
        StrokeKind::Middle,
    );

    /* power lamp: breathing, never blinking */
    let pulse = 0.68 + 0.18 * (t * 1.9).sin();
    let led = pos2(outer.right() - 17.0, outer.bottom() - 11.0);
    p.circle_filled(led, 4.5, Color32::from_rgba_unmultiplied(0xff, 0x9a, 0x20, (26.0 * pulse) as u8));
    p.circle_filled(led, 2.0, Color32::from_rgba_unmultiplied(0xff, 0xb8, 0x1c, (235.0 * pulse) as u8));
}

/// Faint phosphor haze in the middle of the face. Without it the empty part of
/// the screen is pure black and the glass stops reading as glass.
fn haze(p: &egui::Painter, vis: Rect) {
    let mid = Color32::from_rgba_unmultiplied(0xff, 0xc4, 0x60, 4);
    let clear = Color32::TRANSPARENT;
    let (l, r, t, b) = (vis.left(), vis.right(), vis.top(), vis.bottom());
    let cy = vis.center().y;
    quad_grad(p, [pos2(l, t), pos2(r, t), pos2(r, cy), pos2(l, cy)], [clear, clear, mid, mid]);
    quad_grad(p, [pos2(l, cy), pos2(r, cy), pos2(r, b), pos2(l, b)], [mid, mid, clear, clear]);
}

/// Light caught on the inside of the glass, just off the bezel. Without this the
/// screen edge is a hard cut and the face reads flat.
fn glass_rim(p: &egui::Painter, glass: Rect) {
    p.rect_stroke(
        glass.shrink(0.5),
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xff, 0xd2, 0x8a, 40)),
        StrokeKind::Inside,
    );
    p.rect_stroke(
        glass.shrink(3.5),
        CornerRadius::ZERO,
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(0xff, 0xb0, 0x00, 14)),
        StrokeKind::Inside,
    );
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
    let d = (rect.width().min(rect.height()) * 0.17).max(26.0);
    let dark = Color32::from_black_alpha(46);
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

/// The three keys on the plate. They are drawn as stamped metal keys rather
/// than modern flat icons: a dark face, a chiselled bevel that catches light on
/// the top-left and shadows the bottom-right, and a blocky glyph. Pressing one
/// lights the face like a backlit membrane key.
fn window_buttons(ui: &mut egui::Ui, maximized: bool) -> Option<WBtn> {
    let mut hit = None;
    /* right-to-left layout: list them in reverse so close sits at the corner */
    let specs = [
        (WBtn::Close, Glyph::Close),
        (WBtn::Max, if maximized { Glyph::Restore } else { Glyph::Max }),
        (WBtn::Min, Glyph::Min),
    ];
    for (btn, glyph) in specs {
        let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 18.0), Sense::click());
        let key = rect.shrink(1.5);
        let hot = resp.hovered();
        let close = matches!(btn, WBtn::Close);
        let p = ui.painter();

        /* the key sits in its own hole in the plate */
        p.rect_filled(key, CornerRadius::ZERO, if hot { theme::AMBER_DIM } else { theme::BG_DEEP });

        /* bevel: light on the top-left, shadow on the bottom-right */
        let lit = if hot { theme::AMBER_HOT } else { theme::AMBER_FAINT };
        let st = Stroke::new(1.0, lit);
        p.line_segment([key.left_top(), key.right_top()], st);
        p.line_segment([key.left_top(), key.left_bottom()], st);
        let sh = Stroke::new(1.0, Color32::from_black_alpha(215));
        p.line_segment([key.left_bottom(), key.right_bottom()], sh);
        p.line_segment([key.right_top(), key.right_bottom()], sh);

        /* dark surround so the key reads as a separate part */
        p.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, Color32::from_black_alpha(120)), StrokeKind::Outside);

        let col = match (hot, close) {
            (true, _) => theme::BG_DEEP,
            (false, true) => theme::AMBER,
            (false, false) => theme::AMBER_DIM,
        };
        draw_glyph(p, key, glyph, col);
        if resp.clicked() {
            hit = Some(btn);
        }
    }
    hit
}

/// Chunky, pixel-flavoured marks — thin vectors look like a web app, not a
/// machine that was made in 1983.
fn draw_glyph(p: &egui::Painter, rect: Rect, g: Glyph, col: Color32) {
    let c = rect.center();
    let st = Stroke::new(1.8, col);
    let s = 4.6;
    match g {
        Glyph::Min => {
            p.rect_filled(
                Rect::from_center_size(pos2(c.x, c.y + 2.4), vec2(s * 2.0, 2.4)),
                CornerRadius::ZERO,
                col,
            );
        }
        Glyph::Max => {
            p.rect_stroke(
                Rect::from_center_size(c, vec2(s * 2.0, s * 1.7)),
                CornerRadius::ZERO,
                st,
                StrokeKind::Inside,
            );
        }
        Glyph::Restore => {
            let back = Rect::from_center_size(c - vec2(1.4, 1.4), vec2(s * 1.7, s * 1.4));
            let front = Rect::from_center_size(c + vec2(1.4, 1.4), vec2(s * 1.7, s * 1.4));
            p.rect_stroke(back, CornerRadius::ZERO, Stroke::new(1.6, col), StrokeKind::Inside);
            p.rect_stroke(front, CornerRadius::ZERO, Stroke::new(1.6, col), StrokeKind::Inside);
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
        let before = self.font_size;
        match pump.zoom {
            1 => self.font_size = (self.font_size + 2.0).min(32.0),
            -1 => self.font_size = (self.font_size - 2.0).max(10.0),
            9 => self.font_size = 16.0,
            _ => {}
        }
        if self.font_size != before {
            self.sel = None;
        }
        if pump.scroll != 0 {
            self.term.scroll(pump.scroll * 3);
            /* the marks belong to the rows that were on screen a moment ago */
            self.sel = None;
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
            self.sel = None;
        }
        if pump.copy && self.copy_selection(&ctx) {
            self.sel = None;
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

#[cfg(test)]
mod tests {
    use super::{in_selection, is_wide, selection_text};
    use crate::term::Term;

    #[test]
    fn han_and_kana_take_two_cells() {
        for c in ['中', '文', 'あ', 'ア', '한', 'Ａ', '：'] {
            assert!(is_wide(c), "{c} should be double width");
        }
    }

    #[test]
    fn latin_and_box_drawing_take_one() {
        for c in ['a', 'Z', '0', ' ', '─', '│', '→', '—', '\u{2500}'] {
            assert!(!is_wide(c), "{c:?} should be single width");
        }
    }

    #[test]
    fn selection_covers_whole_rows_between_the_ends() {
        let sel = Some(((1, 4), (3, 2)));
        assert!(!in_selection(sel, 0, 9));
        assert!(!in_selection(sel, 1, 3));
        assert!(in_selection(sel, 1, 4));
        assert!(in_selection(sel, 2, 0), "middle rows are included in full");
        assert!(in_selection(sel, 2, 999));
        assert!(in_selection(sel, 3, 2));
        assert!(!in_selection(sel, 3, 3));
        assert!(!in_selection(None, 2, 0));
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let mut t = Term::new(4, 20);
        t.process(b"one\r\ntwo\r\nthree");
        let forward = selection_text(t.screen(), (0, 0), (2, 4));
        let backward = selection_text(t.screen(), (2, 4), (0, 0));
        assert_eq!(forward, "one\ntwo\nthree");
        assert_eq!(forward, backward);
    }

    #[test]
    fn copy_drops_trailing_blanks_and_keeps_inner_spaces() {
        let mut t = Term::new(4, 20);
        t.process(b"ab  cd\r\nx");
        assert_eq!(selection_text(t.screen(), (0, 0), (0, 5)), "ab  cd");
        assert_eq!(selection_text(t.screen(), (0, 0), (1, 0)), "ab  cd\nx");
    }

    #[test]
    fn wide_glyphs_are_copied_once() {
        let mut t = Term::new(4, 20);
        t.process("组价 2026".as_bytes());
        /* the second cell of each Han glyph must not double the character */
        assert_eq!(selection_text(t.screen(), (0, 0), (0, 8)), "组价 2026");
    }
}
