//! Keyboard translation.
//!
//! In a real terminal the shell owns line editing, so this module does almost
//! nothing clever: it turns key presses into the byte sequences an xterm would
//! send and lets PSReadLine / readline do the rest. That is what makes inline
//! history prediction, syntax highlighting and menu completion work in the GUI
//! without writing any of them.

use eframe::egui::{self, Event, Key, Modifiers};

pub struct Pump {
    pub bytes: Vec<u8>,
    pub scroll: i32,
    /// 1 = bigger, -1 = smaller, 9 = reset, 0 = untouched
    pub zoom: i8,
    pub respawn: bool,
    pub toggle_scan: bool,
    /// Ctrl+Shift+C: put the selection on the system clipboard.
    pub copy: bool,
}

/// Take every keyboard event out of the frame so egui's own Tab-focus and
/// shortcut handling never competes with the shell. Pointer events are left
/// alone, otherwise the buttons would stop working.
///
/// One thing to know before touching this: `egui-winit` intercepts Ctrl+C /
/// Ctrl+X / Ctrl+V before we ever see a key event and turns them into
/// `Event::Copy` / `Cut` / `Paste`. A terminal that ignored `Event::Copy` would
/// have no working interrupt at all — Ctrl+C would look like it did nothing.
pub fn pump(ctx: &egui::Context) -> Pump {
    let mut out = Pump {
        bytes: Vec::new(),
        scroll: 0,
        zoom: 0,
        respawn: false,
        toggle_scan: false,
        copy: false,
    };

    ctx.input_mut(|i| {
        let events = std::mem::take(&mut i.events);
        let mut keep = Vec::with_capacity(events.len());

        for ev in events {
            match ev {
                Event::Text(t) => out.bytes.extend_from_slice(t.as_bytes()),
                Event::Paste(t) => out.bytes.extend_from_slice(t.as_bytes()),
                /* egui ate the keystroke, so hand the shell what xterm would have sent */
                Event::Copy => out.bytes.push(0x03),
                Event::Cut => out.bytes.push(0x18),
                Event::Key { key, pressed: true, modifiers, .. } => {
                    if key == Key::F5 {
                        out.respawn = true;
                        continue;
                    }
                    /* Ctrl+G would otherwise reach the shell as BEL */
                    if modifiers.ctrl && modifiers.shift && key == Key::G {
                        out.toggle_scan = true;
                        continue;
                    }
                    /* Ctrl+Shift+C never reaches `Event::Copy` (that path needs an
                    unmodified Ctrl), so it still arrives here as a plain key. Take
                    it before the C0 table turns it into another SIGINT. */
                    if modifiers.ctrl && modifiers.shift && key == Key::C {
                        out.copy = true;
                        continue;
                    }
                    if modifiers.ctrl && !modifiers.alt {
                        match key {
                            Key::Plus | Key::Equals => { out.zoom = 1; continue; }
                            Key::Minus => { out.zoom = -1; continue; }
                            Key::Num0 => { out.zoom = 9; continue; }
                            _ => {}
                        }
                    }
                    push_key(&mut out.bytes, key, modifiers);                }
                Event::MouseWheel { delta, .. } => {
                    out.scroll += delta.y.round() as i32;
                }
                other => keep.push(other),
            }
        }

        i.events = keep;
    });

    out
}

fn push_key(out: &mut Vec<u8>, key: Key, m: Modifiers) {
    if let Some(bytes) = key_bytes(key, m) {
        out.extend_from_slice(&bytes);
    }
}

/// Pure mapping from an egui key to the bytes an xterm would send.
///
/// Returns `None` for printable keys on purpose: those arrive as `Event::Text`
/// and handling them here as well would double every keystroke.
pub fn key_bytes(key: Key, m: Modifiers) -> Option<Vec<u8>> {
    if m.ctrl && !m.alt {
        if let Some(b) = ctrl_byte(key) {
            return Some(vec![b]);
        }
    }

    let seq: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Backspace => b"\x7f",
        Key::Tab => b"\t",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::Insert => b"\x1b[2~",
        Key::Delete => b"\x1b[3~",
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        Key::F6 => b"\x1b[17~",
        Key::F7 => b"\x1b[18~",
        Key::F8 => b"\x1b[19~",
        Key::F9 => b"\x1b[20~",
        Key::F10 => b"\x1b[21~",
        Key::F11 => b"\x1b[23~",
        Key::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

fn ctrl_byte(k: Key) -> Option<u8> {
    Some(match k {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        Key::Space => 0x00,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: Key) -> Option<Vec<u8>> {
        key_bytes(key, Modifiers::NONE)
    }

    fn ctrl(key: Key) -> Option<Vec<u8>> {
        key_bytes(key, Modifiers::CTRL)
    }

    #[test]
    fn printable_keys_are_left_to_text_events() {
        /* otherwise every letter would be typed twice */
        assert_eq!(k(Key::A), None);
        assert_eq!(k(Key::Z), None);
        assert_eq!(k(Key::Num1), None);
        assert_eq!(k(Key::Semicolon), None);
    }

    #[test]
    fn enter_sends_cr_not_lf() {
        /* a shell's line discipline expects carriage return */
        assert_eq!(k(Key::Enter), Some(b"\r".to_vec()));
    }

    #[test]
    fn control_keys_map_to_c0_bytes() {
        assert_eq!(ctrl(Key::C), Some(vec![0x03]));
        assert_eq!(ctrl(Key::A), Some(vec![0x01]));
        assert_eq!(ctrl(Key::R), Some(vec![0x12]));
        assert_eq!(ctrl(Key::Space), Some(vec![0x00]));
    }

    #[test]
    fn arrows_use_csi_sequences() {
        assert_eq!(k(Key::ArrowUp), Some(b"\x1b[A".to_vec()));
        assert_eq!(k(Key::ArrowDown), Some(b"\x1b[B".to_vec()));
        assert_eq!(k(Key::ArrowRight), Some(b"\x1b[C".to_vec()));
        assert_eq!(k(Key::ArrowLeft), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn editing_keys_match_xterm() {
        assert_eq!(k(Key::Tab), Some(b"\t".to_vec()));
        assert_eq!(k(Key::Backspace), Some(b"\x7f".to_vec()));
        assert_eq!(k(Key::Escape), Some(b"\x1b".to_vec()));
        assert_eq!(k(Key::Delete), Some(b"\x1b[3~".to_vec()));
        assert_eq!(k(Key::Home), Some(b"\x1b[H".to_vec()));
        assert_eq!(k(Key::End), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn ctrl_wins_over_the_plain_sequence() {
        assert_eq!(ctrl(Key::C), Some(vec![0x03]));
        /* a modified arrow has no C0 mapping, so the plain CSI form goes through */
        assert_eq!(ctrl(Key::ArrowRight), Some(b"\x1b[C".to_vec()));
    }
}
