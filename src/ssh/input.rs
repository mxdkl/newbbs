//! Turning an ssh byte stream into the key events the UI already speaks.
//!
//! crossterm parses this for a local terminal, but its parser is private and
//! tied to reading a tty, so an ssh session has to do its own. The set of keys
//! the BBS binds is small and fully known, so this covers exactly that and
//! ignores anything it does not recognise rather than guessing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Default)]
pub struct InputParser {
    /// A sequence split across packets waits here for the rest of itself.
    pending: Vec<u8>,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk of bytes and take whatever complete keys fall out.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<KeyEvent> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;

        while at < self.pending.len() {
            match self.step(at) {
                Step::Key(key, used) => {
                    out.push(key);
                    at += used;
                }
                Step::Skip(used) => at += used,
                // Not enough bytes yet: keep the tail for the next packet.
                Step::Incomplete => break,
            }
        }
        self.pending.drain(..at);
        out
    }

    fn step(&self, at: usize) -> Step {
        let buf = &self.pending[at..];
        match buf[0] {
            0x1b => self.escape(buf),
            b'\r' | b'\n' => Step::Key(plain(KeyCode::Enter), 1),
            b'\t' => Step::Key(plain(KeyCode::Tab), 1),
            0x08 | 0x7f => Step::Key(plain(KeyCode::Backspace), 1),
            0x00 => Step::Key(
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
                1,
            ),
            // Ctrl-a to Ctrl-z, minus the three that have their own meaning.
            c @ 0x01..=0x1a => Step::Key(
                KeyEvent::new(
                    KeyCode::Char((c - 1 + b'a') as char),
                    KeyModifiers::CONTROL,
                ),
                1,
            ),
            c @ 0x1c..=0x1f => Step::Key(
                KeyEvent::new(
                    KeyCode::Char((c - 0x1c + b'4') as char),
                    KeyModifiers::CONTROL,
                ),
                1,
            ),
            _ => decode_char(buf),
        }
    }

    fn escape(&self, buf: &[u8]) -> Step {
        // A lone escape at the end of a packet is the Esc key. Terminals send
        // the rest of a sequence in the same write, so waiting for more would
        // hang Esc -- which this UI leans on heavily.
        if buf.len() == 1 {
            return Step::Key(plain(KeyCode::Esc), 1);
        }
        match buf[1] {
            b'[' => csi(buf),
            b'O' => ss3(buf),
            0x1b => Step::Key(plain(KeyCode::Esc), 1),
            _ => match decode_char(&buf[1..]) {
                Step::Key(key, used) => Step::Key(
                    KeyEvent::new(key.code, key.modifiers | KeyModifiers::ALT),
                    used + 1,
                ),
                Step::Skip(used) => Step::Skip(used + 1),
                Step::Incomplete => Step::Incomplete,
            },
        }
    }
}

enum Step {
    Key(KeyEvent, usize),
    Skip(usize),
    Incomplete,
}

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// `ESC [ ... final`
fn csi(buf: &[u8]) -> Step {
    let mut end = 2;
    while end < buf.len() && !(0x40..=0x7e).contains(&buf[end]) {
        end += 1;
    }
    if end >= buf.len() {
        return Step::Incomplete;
    }
    let final_byte = buf[end];
    let params = &buf[2..end];
    let used = end + 1;

    // Parameters look like `1;5` -- the second one carries the modifiers.
    let parts: Vec<&[u8]> = params.split(|b| *b == b';').collect();
    let first = parse_num(parts.first().copied().unwrap_or(b""));
    let modifiers = decode_modifiers(parse_num(parts.get(1).copied().unwrap_or(b"")));

    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Step::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), used),
        b'~' => match first {
            Some(1) | Some(7) => KeyCode::Home,
            Some(2) => KeyCode::Insert,
            Some(3) => KeyCode::Delete,
            Some(4) | Some(8) => KeyCode::End,
            Some(5) => KeyCode::PageUp,
            Some(6) => KeyCode::PageDown,
            _ => return Step::Skip(used),
        },
        _ => return Step::Skip(used),
    };
    Step::Key(KeyEvent::new(code, modifiers), used)
}

/// `ESC O x` -- what a terminal in application cursor mode sends for arrows.
fn ss3(buf: &[u8]) -> Step {
    if buf.len() < 3 {
        return Step::Incomplete;
    }
    let code = match buf[2] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return Step::Skip(3),
    };
    Step::Key(plain(code), 3)
}

fn parse_num(bytes: &[u8]) -> Option<u8> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// xterm encodes modifiers as a bitmask offset by one.
fn decode_modifiers(param: Option<u8>) -> KeyModifiers {
    let Some(param) = param else {
        return KeyModifiers::NONE;
    };
    let bits = param.saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    modifiers
}

/// One UTF-8 character, however many bytes that takes.
fn decode_char(buf: &[u8]) -> Step {
    let width = match buf[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A stray continuation byte: drop it rather than stall the stream.
        _ => return Step::Skip(1),
    };
    if buf.len() < width {
        return Step::Incomplete;
    }
    match std::str::from_utf8(&buf[..width]) {
        Ok(text) => match text.chars().next() {
            Some(c) => {
                // An uppercase letter arrives as itself; crossterm reports the
                // shift that produced it.
                let modifiers = if c.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                Step::Key(KeyEvent::new(KeyCode::Char(c), modifiers), width)
            }
            None => Step::Skip(width),
        },
        Err(_) => Step::Skip(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
        InputParser::new().feed(bytes)
    }

    fn codes(bytes: &[u8]) -> Vec<KeyCode> {
        keys(bytes).into_iter().map(|k| k.code).collect()
    }

    #[test]
    fn plain_typing() {
        assert_eq!(
            codes(b"hi"),
            vec![KeyCode::Char('h'), KeyCode::Char('i')]
        );
    }

    #[test]
    fn the_keys_the_keymap_binds() {
        assert_eq!(codes(b"\r"), vec![KeyCode::Enter]);
        assert_eq!(codes(b"\x1b"), vec![KeyCode::Esc]);
        assert_eq!(codes(b"\t"), vec![KeyCode::Tab]);
        assert_eq!(codes(b"\x7f"), vec![KeyCode::Backspace]);
        assert_eq!(codes(b"\x1b[A"), vec![KeyCode::Up]);
        assert_eq!(codes(b"\x1b[B"), vec![KeyCode::Down]);
        assert_eq!(codes(b"\x1b[5~"), vec![KeyCode::PageUp]);
        assert_eq!(codes(b"\x1b[6~"), vec![KeyCode::PageDown]);
        assert_eq!(codes(b"\x1b[3~"), vec![KeyCode::Delete]);
        assert_eq!(codes(b"\x1b[H"), vec![KeyCode::Home]);
        assert_eq!(codes(b"\x1bOD"), vec![KeyCode::Left]);
    }

    #[test]
    fn control_chords() {
        let ctrl_c = keys(b"\x03");
        assert_eq!(ctrl_c[0].code, KeyCode::Char('c'));
        assert!(ctrl_c[0].modifiers.contains(KeyModifiers::CONTROL));

        let ctrl_w = keys(b"\x17");
        assert_eq!(ctrl_w[0].code, KeyCode::Char('w'));
        assert!(ctrl_w[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn modified_arrows() {
        let ctrl_up = keys(b"\x1b[1;5A");
        assert_eq!(ctrl_up[0].code, KeyCode::Up);
        assert!(ctrl_up[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    /// A packet boundary can land anywhere, including the middle of a
    /// sequence or of a character.
    #[test]
    fn sequences_split_across_packets() {
        let mut parser = InputParser::new();
        assert!(parser.feed(b"\x1b[").is_empty());
        assert_eq!(
            parser.feed(b"A").into_iter().map(|k| k.code).collect::<Vec<_>>(),
            vec![KeyCode::Up]
        );

        let mut parser = InputParser::new();
        assert!(parser.feed(&[0xc3]).is_empty());
        assert_eq!(
            parser.feed(&[0xa9]).into_iter().map(|k| k.code).collect::<Vec<_>>(),
            vec![KeyCode::Char('é')]
        );
    }

    #[test]
    fn a_whole_command_typed_at_once() {
        assert_eq!(
            codes(b":q\r"),
            vec![KeyCode::Char(':'), KeyCode::Char('q'), KeyCode::Enter]
        );
    }

    /// Anything unrecognised is dropped, never turned into a stray keypress.
    #[test]
    fn unknown_sequences_are_ignored() {
        assert!(codes(b"\x1b[200~").is_empty());
        assert_eq!(codes(b"\x1b[200~x"), vec![KeyCode::Char('x')]);
    }
}
