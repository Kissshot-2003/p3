//! Screen buffer: a thin wrapper around `vt100` that owns the parser and keeps
//! the last known geometry, so the renderer never has to ask the PTY anything.

use std::num::NonZeroU16;

pub struct Term {
    parser: vt100::Parser,
    pub rows: u16,
    pub cols: u16,
    pub scroll_offset: usize,
}

/// vt100 refuses zero-sized grids, which is a real case here: the window can be
/// resized smaller than one cell while the panel animates.
fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).expect("max(1) is never zero")
}

impl Term {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(2);
        let cols = cols.max(2);
        Self {
            parser: vt100::Parser::new(nz(rows), nz(cols), 8000),
            rows,
            cols,
            scroll_offset: 0,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(2);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(nz(rows), nz(cols));
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.parser.screen_mut()
    }

    /// Wheel input. Positive delta scrolls into the history; 0 returns to live.
    pub fn scroll(&mut self, delta: i32) {
        let sb = self.parser.screen().scrollback();
        let next = (sb as i32 - delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(next);
        self.scroll_offset = next;
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.scroll_offset != 0 {
            self.parser.screen_mut().set_scrollback(0);
            self.scroll_offset = 0;
        }
    }

    /// Answer the device queries a shell sends while probing the terminal.
    ///
    /// This is not optional: PSReadLine opens with `ESC[6n` (report cursor
    /// position) and blocks until the host replies, so a terminal that ignores
    /// it shows a prompt that never arrives — which looks exactly like a hung
    /// session.
    pub fn respond(&self, chunk: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();

        if contains(chunk, b"\x1b[6n") {
            let (row, col) = self.parser.screen().cursor_position();
            out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
        }
        if contains(chunk, b"\x1b[5n") {
            out.extend_from_slice(b"\x1b[0n");
        }
        if contains(chunk, b"\x1b[c") || contains(chunk, b"\x1b[0c") {
            /* VT100 with advanced video option: what every modern emulator claims */
            out.extend_from_slice(b"\x1b[?1;2c");
        }
        if contains(chunk, b"\x1b[>c") || contains(chunk, b"\x1b[>0c") {
            out.extend_from_slice(b"\x1b[>0;100;0c");
        }

        if out.is_empty() { None } else { Some(out) }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
