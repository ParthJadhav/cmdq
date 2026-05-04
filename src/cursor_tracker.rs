//! Lightweight cursor-position tracker for direct PTY forwarding.
//!
//! This is intentionally not a terminal emulator. We track only enough cursor
//! movement to restore the shell cursor after repainting cmdq's bottom panel
//! without using terminal-global save/restore slots (`ESC 7` / `ESC 8`).

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    String,
    StringEsc,
    Utf8,
}

#[derive(Debug, Clone)]
pub struct CursorTracker {
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    saved: Option<(u16, u16)>,
    state: State,
    csi: Vec<u8>,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_needed: usize,
}

impl CursorTracker {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            col: 0,
            row: rows.saturating_sub(1),
            cols: cols.max(1),
            rows: rows.max(1),
            saved: None,
            state: State::Ground,
            csi: Vec::new(),
            utf8: [0; 4],
            utf8_len: 0,
            utf8_needed: 0,
        }
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.col = self.col.min(self.cols);
        self.row = self.row.min(self.rows.saturating_sub(1));
    }

    pub fn set_to_bottom_left(&mut self) {
        self.col = 0;
        self.row = self.rows.saturating_sub(1);
        self.state = State::Ground;
        self.csi.clear();
        self.clear_utf8();
    }

    pub fn position(&self) -> (u16, u16) {
        (
            self.col.min(self.cols.saturating_sub(1)),
            self.row.min(self.rows.saturating_sub(1)),
        )
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => match b {
                0x1b => self.state = State::Esc,
                b'\r' => self.col = 0,
                b'\n' => self.linefeed(),
                0x08 => self.col = self.col.saturating_sub(1),
                b'\t' => {
                    let next = ((self.col / 8) + 1) * 8;
                    self.col = next.min(self.cols.saturating_sub(1));
                }
                0x20..=0x7e => self.printable_width(1),
                0xc2..=0xf4 => self.start_utf8(b),
                _ => {}
            },
            State::Esc => match b {
                b'[' => {
                    self.csi.clear();
                    self.state = State::Csi;
                }
                b']' => {
                    self.state = State::Osc;
                }
                b'P' | b'_' | b'^' | b'X' => {
                    self.state = State::String;
                }
                b'7' => {
                    self.saved = Some(self.position());
                    self.state = State::Ground;
                }
                b'8' => {
                    if let Some((col, row)) = self.saved {
                        self.col = col.min(self.cols.saturating_sub(1));
                        self.row = row.min(self.rows.saturating_sub(1));
                    }
                    self.state = State::Ground;
                }
                0x1b => self.state = State::Esc,
                _ => self.state = State::Ground,
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.finish_csi(b);
                    self.csi.clear();
                    self.state = State::Ground;
                } else if self.csi.len() < 64 {
                    self.csi.push(b);
                }
            }
            State::Osc => match b {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::OscEsc,
                _ => {}
            },
            State::OscEsc => match b {
                b'\\' => self.state = State::Ground,
                0x1b => self.state = State::OscEsc,
                _ => self.state = State::Osc,
            },
            State::String => match b {
                0x1b => self.state = State::StringEsc,
                _ => {}
            },
            State::StringEsc => match b {
                b'\\' => self.state = State::Ground,
                0x1b => self.state = State::StringEsc,
                _ => self.state = State::String,
            },
            State::Utf8 => self.continue_utf8(b),
        }
    }

    fn printable_width(&mut self, width: usize) {
        if width == 0 {
            return;
        }
        let width = width.min(self.cols as usize) as u16;
        if self.col >= self.cols || self.col.saturating_add(width) > self.cols {
            self.linefeed();
            self.col = 0;
        }
        self.col = self.col.saturating_add(width).min(self.cols);
    }

    fn start_utf8(&mut self, b: u8) {
        let Some(needed) = utf8_expected_len(b) else {
            return;
        };
        self.utf8 = [0; 4];
        self.utf8[0] = b;
        self.utf8_len = 1;
        self.utf8_needed = needed;
        if needed == 1 {
            self.finish_utf8();
        } else {
            self.state = State::Utf8;
        }
    }

    fn continue_utf8(&mut self, b: u8) {
        if !is_utf8_continuation(b) {
            self.clear_utf8();
            self.state = State::Ground;
            self.step(b);
            return;
        }
        if self.utf8_len < self.utf8.len() {
            self.utf8[self.utf8_len] = b;
            self.utf8_len += 1;
        }
        if self.utf8_len == self.utf8_needed {
            self.finish_utf8();
            self.state = State::Ground;
        }
    }

    fn finish_utf8(&mut self) {
        if let Ok(text) = std::str::from_utf8(&self.utf8[..self.utf8_len])
            && let Some(ch) = text.chars().next()
        {
            self.printable_width(UnicodeWidthChar::width(ch).unwrap_or(0));
        }
        self.clear_utf8();
    }

    fn clear_utf8(&mut self) {
        self.utf8_len = 0;
        self.utf8_needed = 0;
    }

    fn linefeed(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn finish_csi(&mut self, final_byte: u8) {
        let params = parse_params(&self.csi);
        match final_byte {
            b'A' => self.row = self.row.saturating_sub(param_or(&params, 0, 1)),
            b'B' => {
                self.row = self
                    .row
                    .saturating_add(param_or(&params, 0, 1))
                    .min(self.rows - 1)
            }
            b'C' => {
                self.col = self
                    .col
                    .saturating_add(param_or(&params, 0, 1))
                    .min(self.cols - 1)
            }
            b'D' => self.col = self.col.saturating_sub(param_or(&params, 0, 1)),
            b'G' => self.col = one_based_param_or(&params, 0, 1).min(self.cols) - 1,
            b'H' | b'f' => {
                self.row = one_based_param_or(&params, 0, 1).min(self.rows) - 1;
                self.col = one_based_param_or(&params, 1, 1).min(self.cols) - 1;
            }
            b'd' => self.row = one_based_param_or(&params, 0, 1).min(self.rows) - 1,
            b's' => self.saved = Some(self.position()),
            b'u' => {
                if let Some((col, row)) = self.saved {
                    self.col = col.min(self.cols.saturating_sub(1));
                    self.row = row.min(self.rows.saturating_sub(1));
                }
            }
            _ => {}
        }
    }
}

fn parse_params(bytes: &[u8]) -> Vec<Option<u16>> {
    let text = String::from_utf8_lossy(bytes);
    text.trim_start_matches('?')
        .split(';')
        .map(|part| {
            if part.is_empty() {
                None
            } else {
                part.parse::<u16>().ok()
            }
        })
        .collect()
}

fn param_or(params: &[Option<u16>], idx: usize, default: u16) -> u16 {
    params.get(idx).and_then(|v| *v).unwrap_or(default).max(1)
}

fn one_based_param_or(params: &[Option<u16>], idx: usize, default: u16) -> u16 {
    param_or(params, idx, default)
}

fn utf8_expected_len(b: u8) -> Option<usize> {
    match b {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn is_utf8_continuation(b: u8) -> bool {
    (0x80..=0xbf).contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_basic_print_crlf_and_cursor_moves() {
        let mut t = CursorTracker::new(10, 5);
        t.feed(b"abc\r\nxy");
        assert_eq!(t.position(), (2, 4));
        t.feed(b"\x1b[2;4Hz");
        assert_eq!(t.position(), (4, 1));
        t.feed(b"\x1b[2D");
        assert_eq!(t.position(), (2, 1));
    }

    #[test]
    fn carriage_return_after_full_line_stays_on_same_row() {
        let mut t = CursorTracker::new(10, 5);
        t.feed(b"1234567890\rX");
        assert_eq!(t.position(), (1, 4));
    }

    #[test]
    fn tracks_child_save_restore_without_owning_terminal_slot() {
        let mut t = CursorTracker::new(20, 10);
        t.feed(b"\x1b[3;5H\x1b7hello\x1b[9;9H\x1b8!");
        assert_eq!(t.position(), (5, 2));
    }

    #[test]
    fn tracks_utf8_display_width_across_chunks() {
        let mut t = CursorTracker::new(10, 5);
        t.feed("a".as_bytes());
        let wide = "界".as_bytes();
        t.feed(&wide[..1]);
        assert_eq!(
            t.position(),
            (1, 4),
            "incomplete UTF-8 must not move cursor"
        );
        t.feed(&wide[1..]);
        t.feed("e\u{301}🙂".as_bytes());
        assert_eq!(t.position(), (6, 4));
    }

    #[test]
    fn osc_payloads_do_not_move_cursor() {
        let mut t = CursorTracker::new(80, 5);
        t.feed(b"before");
        t.feed(b"\x1b]8;;https://example.com/very/long/path\x07link\x1b]8;;\x07");
        assert_eq!(t.position(), (10, 4));

        t.feed(b"\x1b]52;c;SGVsbG8=\x07");
        assert_eq!(t.position(), (10, 4));
    }

    #[test]
    fn osc_st_terminated_payloads_do_not_move_cursor() {
        let mut t = CursorTracker::new(80, 5);
        t.feed(b"\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\");
        assert_eq!(t.position(), (5, 4));
    }

    #[test]
    fn terminal_string_payloads_do_not_move_cursor() {
        let mut t = CursorTracker::new(80, 5);
        t.feed(b"before");
        t.feed(b"\x1bP1;1;AAAAA\x1b\\after");
        assert_eq!(t.position(), (11, 4));

        t.feed(b"\x1b_kitty-graphics-payload\x1b\\!");
        assert_eq!(t.position(), (12, 4));

        t.feed(b"\x1b^privacy-message\x1b\\\x1bXsos-payload\x1b\\?");
        assert_eq!(t.position(), (13, 4));
    }

    #[test]
    fn wide_char_exactly_filling_line_defers_wrap_until_next_printable() {
        let mut t = CursorTracker::new(10, 5);
        t.feed(b"\x1b[1;9H");
        t.feed("界".as_bytes());
        assert_eq!(t.position(), (9, 0));
        t.feed(b"x");
        assert_eq!(t.position(), (1, 1));
    }

    #[test]
    fn wide_char_with_insufficient_room_wraps_before_printing() {
        let mut t = CursorTracker::new(10, 5);
        t.feed(b"\x1b[1;10H");
        t.feed("界".as_bytes());
        assert_eq!(t.position(), (2, 1));
    }
}
