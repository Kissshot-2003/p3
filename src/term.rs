//! Screen buffer: a thin wrapper around `vt100` that owns the parser and keeps
//! the last known geometry, so the renderer never has to ask the PTY anything.

use std::num::NonZeroU16;

pub struct Term {
    parser: vt100::Parser,
    pub rows: u16,
    pub cols: u16,
    pub scroll_offset: usize,
    /// Tail of PTY output carried between chunks. A read is not a message
    /// boundary, so a query like `ESC[6n` can arrive as `ESC[` then `6n`.
    query_tail: Vec<u8>,
}

/// Longest supported query is five bytes; sixteen leaves room for two split
/// sequences in flight without holding on to much of the output stream.
const QUERY_TAIL: usize = 16;

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
            query_tail: Vec::new(),
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
    /// session. A PTY read is not a message boundary, so a short tail is carried
    /// between calls to catch queries split across chunks.
    pub fn respond(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if self.query_tail.len() > QUERY_TAIL {
            let excess = self.query_tail.len() - QUERY_TAIL;
            self.query_tail.drain(..excess);
        }
        self.query_tail.extend_from_slice(chunk);

        let buf = std::mem::take(&mut self.query_tail);
        let (row, col) = self.parser.screen().cursor_position();
        let mut out = Vec::new();
        let mut i = 0;
        let mut answered_through = 0;

        while i < buf.len() {
            if let Some((len, reply)) = query_reply(&buf[i..], row, col) {
                out.extend_from_slice(&reply);
                i += len;
                answered_through = i;
            } else {
                i += 1;
            }
        }

        /* Only bytes after the last answer can still matter, and only the last
        QUERY_TAIL of those: that is enough to finish a split query, while an
        answered one is dropped so it is never replied to twice. */
        let keep_from = answered_through.max(buf.len().saturating_sub(QUERY_TAIL));
        self.query_tail.extend_from_slice(&buf[keep_from..]);

        if out.is_empty() { None } else { Some(out) }
    }
}

/// The reply a device query asks for. Longer needles first, so `ESC[>0c` is
/// not shadowed by `ESC[>c`.
fn query_reply(buf: &[u8], row: u16, col: u16) -> Option<(usize, Vec<u8>)> {
    if buf.starts_with(b"\x1b[6n") {
        return Some((4, format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()));
    }
    const FIXED: [(&[u8], &[u8]); 5] = [
        (b"\x1b[>0c", b"\x1b[>0;100;0c"),
        (b"\x1b[>c", b"\x1b[>0;100;0c"),
        (b"\x1b[5n", b"\x1b[0n"),
        (b"\x1b[0c", b"\x1b[?1;2c"),
        /* VT100 with advanced video option: what every modern emulator claims */
        (b"\x1b[c", b"\x1b[?1;2c"),
    ];
    FIXED.iter().find_map(|&(needle, reply)| {
        buf.starts_with(needle)
            .then(|| (needle.len(), reply.to_vec()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(t: &mut Term, chunks: &[&[u8]]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for chunk in chunks {
            if let Some(bytes) = t.respond(chunk) {
                out.extend_from_slice(&bytes);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    #[test]
    fn reports_cursor_position() {
        let mut t = Term::new(4, 20);
        assert_eq!(reply(&mut t, &[b"\x1b[6n"]), Some(b"\x1b[1;1R".to_vec()));
    }

    #[test]
    fn answers_queries_split_across_reads() {
        let mut t = Term::new(4, 20);
        assert_eq!(reply(&mut t, &[b"prompt\x1b["]), None);
        assert_eq!(reply(&mut t, &[b"6n"]), Some(b"\x1b[1;1R".to_vec()));
    }

    #[test]
    fn never_answers_the_same_split_query_twice() {
        let mut t = Term::new(4, 20);
        assert_eq!(reply(&mut t, &[b"\x1b["]), None);
        assert_eq!(reply(&mut t, &[b"6n"]), Some(b"\x1b[1;1R".to_vec()));
        assert_eq!(reply(&mut t, &[b""]), None);
        assert_eq!(reply(&mut t, &[b"ordinary output"]), None);
    }

    #[test]
    fn handles_device_status_and_identification() {
        let mut t = Term::new(4, 20);
        assert_eq!(
            reply(&mut t, &[b"\x1b[5n\x1b[c\x1b[>c"]),
            Some(b"\x1b[0n\x1b[?1;2c\x1b[>0;100;0c".to_vec())
        );
    }
}
