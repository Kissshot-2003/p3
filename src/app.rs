//! Window, layout and the terminal renderer.
//!
//! The render pass walks the vt100 grid once per frame and merges adjacent cells
//! that share a foreground colour into a single glyph run — a full 200×55 screen
//! is a few hundred draw calls instead of twelve thousand.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Rect, Shape, Stroke, pos2, vec2,
};

use crate::input;
use crate::pty::{PtySession, ShellSpec};
use crate::term::Term;
use crate::theme;

const PAD: f32 = 12.0;
const SCAN_STEP: f32 = 3.0;
const RAIL_W: f32 = 248.0;

pub struct App {
    term: Term,
    pty: Option<PtySession>,
    font_size: f32,
    started: Instant,
    status: String,
    shell_label: String,
    cwd: String,
    rail_open: bool,
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
        let label = spec.label.clone();

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
            shell_label: label,
            cwd,
            rail_open: true,
            scanlines: true,
            dump_tick: 0,
            rx_total: 0,
            last_chunk: Vec::new(),
        }
    }

    fn pump_pty(&mut self) {
        let Self { pty, term, status, shell_label, rx_total, last_chunk, .. } = self;
        let Some(p) = pty.as_mut() else { return };

        let label = p.label.clone();
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
        if !label.is_empty() {
            *shell_label = label;
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
        egui::Panel::top("hud")
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(12, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        egui::RichText::new("RADIOSOL")
                            .color(theme::AMBER)
                            .size(11.5)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("// TERMINAL")
                            .color(theme::AMBER_FAINT)
                            .size(11.5),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let up = self.started.elapsed().as_secs();
                        ui.label(
                            egui::RichText::new(format!("{:02}:{:02}", up / 60, up % 60))
                                .color(theme::AMBER_FAINT)
                                .size(11.0),
                        );
                        if ui
                            .selectable_label(
                                self.scanlines,
                                egui::RichText::new("SCAN").size(10.0),
                            )
                            .clicked()
                        {
                            self.scanlines = !self.scanlines;
                        }
                        if ui
                            .selectable_label(self.rail_open, egui::RichText::new("RAIL").size(10.0))
                            .clicked()
                        {
                            self.rail_open = !self.rail_open;
                        }
                        ui.label(
                            egui::RichText::new(format!("{}×{}", self.term.cols, self.term.rows))
                                .color(theme::AMBER_FAINT)
                                .size(10.5),
                        );
                        let (dot, txt) = if self.status.is_empty() {
                            (theme::AMBER, "LINK")
                        } else {
                            (theme::WARN, "DOWN")
                        };
                        ui.label(
                            egui::RichText::new(format!("● {txt}"))
                                .color(dot)
                                .size(10.5),
                        );
                    });
                });
            });
    }

    /* ----------------------------------------------------------------- rail */

    fn rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("rail")
            .exact_size(RAIL_W)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG_DEEP)
                    .inner_margin(Margin::same(12)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;

                section(ui, "SESSION");
                kv(ui, "shell", &self.shell_label);
                kv(ui, "pid", &self.pty.as_ref().and_then(|p| p.pid).map(|p| p.to_string()).unwrap_or_else(|| "-".into()));
                kv(ui, "size", &format!("{} × {}", self.term.cols, self.term.rows));
                kv(ui, "scroll", &format!("{} 行", self.term.scroll_offset));
                ui.add_space(6.0);

                section(ui, "CWD");
                ui.label(
                    egui::RichText::new(ellipsis(&self.cwd, 42))
                        .color(theme::AMBER_DIM)
                        .size(11.0),
                );
                ui.add_space(6.0);

                section(ui, "KEYS");
                for line in [
                    "TAB     补全 / 预测接受",
                    "→ / END 接受内联预测",
                    "CTRL+R  历史反查",
                    "CTRL+C  中断命令",
                    "PGUP/DN 翻看历史",
                    "CTRL+±  字号  CTRL+0 复位",
                    "F5      重开会话",
                ] {
                    ui.label(
                        egui::RichText::new(line)
                            .color(theme::AMBER_FAINT)
                            .size(10.5)
                            .monospace(),
                    );
                }

                if !self.status.is_empty() {
                    ui.add_space(8.0);
                    section(ui, "STATUS");
                    ui.label(
                        egui::RichText::new(&self.status)
                            .color(theme::WARN)
                            .size(10.5),
                    );
                }
            });
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

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(title)
            .color(theme::AMBER_DEEP)
            .size(9.5)
            .strong(),
    );
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(egui::RichText::new(k).color(theme::AMBER_FAINT).size(10.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(v)
                    .color(theme::AMBER)
                    .size(10.5)
                    .monospace(),
            );
        });
    });
}

fn ellipsis(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - max + 1..].iter().collect();
    format!("…{tail}")
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
        if pump.respawn {
            self.respawn(&ctx);
        }

        self.hud(ui);
        if self.rail_open {
            self.rail(ui);
        }
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(theme::BG))
            .show(ui, |ui| self.terminal(ui));

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
