//! Streaming detector for OSC 133 prompt-marker escape sequences.
//!
//! The shell emits these (when integration is installed):
//!   ESC ] 133 ; A ST   -- prompt start
//!   ESC ] 133 ; B ST   -- prompt end / command input start
//!   ESC ] 133 ; C ST   -- pre-execution (user pressed Enter)
//!   ESC ] 133 ; D\[;ec\] ST -- command finished, optional exit code
//!
//! ST may be either BEL (0x07) or ESC \ (0x1B 0x5C).
//!
//! We feed every byte coming back from the PTY through `Detector::feed` and
//! emit `Event`s the app uses to flip between passthrough and queue-capture mode.
//!
//! The detector is permissive: any non-OSC bytes pass through untouched and are
//! never consumed (we only *observe*; the app still writes the bytes to the
//! user's terminal verbatim).

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;
const OSC_BODY_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandEnd { exit_code: Option<i32> },
}

#[derive(Debug, Clone, Copy)]
enum State {
    Normal,
    AfterEsc,
    InOsc,
    InOscAfterEsc,
}

#[derive(Debug, Clone)]
pub struct Detector {
    state: State,
    body: Vec<u8>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            body: Vec::new(),
        }
    }

    /// Feed a chunk of bytes. Returns events detected within them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for &b in bytes {
            self.step(b, &mut out);
        }
        out
    }

    fn finish_osc(&mut self, out: &mut Vec<Event>) {
        if let Some(ev) = parse_133(&self.body) {
            out.push(ev);
        }
        self.body.clear();
        self.state = State::Normal;
    }

    fn step(&mut self, b: u8, out: &mut Vec<Event>) {
        self.state = match self.state {
            State::Normal => {
                if b == ESC {
                    State::AfterEsc
                } else {
                    State::Normal
                }
            }
            State::AfterEsc => match b {
                b']' => {
                    self.body.clear();
                    State::InOsc
                }
                ESC => State::AfterEsc,
                _ => State::Normal,
            },
            State::InOsc => {
                if b == BEL {
                    self.finish_osc(out);
                    return;
                }
                if b == ESC {
                    State::InOscAfterEsc
                } else {
                    if self.body.len() < OSC_BODY_LIMIT {
                        self.body.push(b);
                    }
                    State::InOsc
                }
            }
            State::InOscAfterEsc => match b {
                b'\\' => {
                    self.finish_osc(out);
                    return;
                }
                ESC => State::InOscAfterEsc,
                _ => {
                    self.body.clear();
                    State::Normal
                }
            },
        };
    }
}

fn parse_133(body: &[u8]) -> Option<Event> {
    let s = std::str::from_utf8(body).ok()?.strip_prefix("133;")?;
    let mut parts = s.split(';');
    match parts.next()? {
        "A" => Some(Event::PromptStart),
        "B" => Some(Event::PromptEnd),
        "C" => Some(Event::CommandStart),
        "D" => {
            let exit_code = parts.next().and_then(|s| s.parse::<i32>().ok());
            Some(Event::CommandEnd { exit_code })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut d = Detector::new();
        d.feed(bytes)
    }

    #[test]
    fn detects_command_start_bel() {
        assert_eq!(events(b"\x1b]133;C\x07"), vec![Event::CommandStart]);
    }

    #[test]
    fn detects_command_end_with_exit() {
        assert_eq!(
            events(b"\x1b]133;D;0\x07"),
            vec![Event::CommandEnd { exit_code: Some(0) }]
        );
    }

    #[test]
    fn detects_command_end_with_nonzero_exit() {
        assert_eq!(
            events(b"\x1b]133;D;127\x07"),
            vec![Event::CommandEnd {
                exit_code: Some(127)
            }]
        );
    }

    #[test]
    fn detects_command_end_without_exit() {
        assert_eq!(
            events(b"\x1b]133;D\x07"),
            vec![Event::CommandEnd { exit_code: None }]
        );
    }

    #[test]
    fn detects_prompt_start_and_end() {
        assert_eq!(
            events(b"\x1b]133;A\x07hi\x1b]133;B\x07"),
            vec![Event::PromptStart, Event::PromptEnd]
        );
    }

    #[test]
    fn st_terminator_string() {
        // ESC \ form
        assert_eq!(events(b"\x1b]133;C\x1b\\"), vec![Event::CommandStart]);
    }

    #[test]
    fn ignores_unrelated_osc() {
        assert!(events(b"\x1b]0;window title\x07").is_empty());
        assert!(events(b"\x1b]2;another title\x07").is_empty());
        assert!(events(b"\x1b]52;c;abcd\x07").is_empty());
    }

    #[test]
    fn handles_byte_split_input() {
        let mut d = Detector::new();
        let mut all = vec![];
        all.extend(d.feed(b"\x1b"));
        all.extend(d.feed(b"]"));
        all.extend(d.feed(b"1"));
        all.extend(d.feed(b"3"));
        all.extend(d.feed(b"3"));
        all.extend(d.feed(b";"));
        all.extend(d.feed(b"C"));
        all.extend(d.feed(b"\x07"));
        assert_eq!(all, vec![Event::CommandStart]);
    }

    #[test]
    fn full_cycle_sequence() {
        let bytes =
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\n\x1b]133;C\x07file1 file2\n\x1b]133;D;0\x07";
        assert_eq!(
            events(bytes),
            vec![
                Event::PromptStart,
                Event::PromptEnd,
                Event::CommandStart,
                Event::CommandEnd { exit_code: Some(0) },
            ]
        );
    }

    #[test]
    fn embedded_in_random_output_with_colors() {
        let bytes = b"random output\x1b[1;31mcolor\x1b[0m text \x1b]133;C\x07more";
        assert_eq!(events(bytes), vec![Event::CommandStart]);
    }

    #[test]
    fn multiple_in_one_buffer() {
        let bytes = b"\x1b]133;C\x07stuff\x1b]133;D;1\x07\x1b]133;A\x07";
        assert_eq!(
            events(bytes),
            vec![
                Event::CommandStart,
                Event::CommandEnd { exit_code: Some(1) },
                Event::PromptStart,
            ]
        );
    }

    #[test]
    fn malformed_osc_does_not_panic() {
        // Truncated, garbage payloads.
        assert!(events(b"\x1b]133;").is_empty());
        assert!(events(b"\x1b]133;Z\x07").is_empty());
        assert!(events(b"\x1b]\x07").is_empty());
    }

    #[test]
    fn body_length_cap() {
        let mut huge = b"\x1b]133;".to_vec();
        huge.extend(std::iter::repeat_n(b'X', 10_000));
        huge.push(b'\x07');
        // Should not panic / OOM; body cap caps memory.
        let _ = events(&huge);
    }

    #[test]
    fn esc_inside_normal_text_does_not_break_detection() {
        // A bare ESC followed by a non-bracket byte should not consume the
        // following 133 sequence.
        let bytes = b"\x1bX\x1b]133;C\x07";
        assert_eq!(events(bytes), vec![Event::CommandStart]);
    }
}
