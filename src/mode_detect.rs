//! Streaming detector for the few CSI private-mode sequences cmdq cares about.
//!
//! Path A architecture (cmdq-as-byte-forwarder, not terminal emulator) means
//! we never parse the full output stream — we just pass shell bytes through
//! to the user's real terminal and only watch for a tiny set of mode flips
//! that change cmdq's own behavior.
//!
//! We detect **alt-screen entry/exit**, **child bracketed-paste mode**,
//! **mouse tracking**, and **focus reporting**.
//! When the inner program (vim, htop, less, fzf, btop, …) flips on
//! `\x1b[?1049h` (or the legacy `\x1b[?47h` / `\x1b[?1047h`), it expects to
//! own the whole screen — so cmdq must tear down its bottom panel and stop
//! reserving rows until the program flips it back off.
//!
//! Bracketed paste is tracked separately because cmdq keeps the outer terminal
//! in paste-reporting mode for itself, then emulates the child's requested
//! paste mode when forwarding paste payloads.
//!
//! The detector is permissive: any non-CSI byte passes through untouched.
//! Sequences split across read chunks are handled (state is preserved).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    AltScreenEnter,
    AltScreenExit,
    BracketedPasteEnable,
    BracketedPasteDisable,
    MouseCaptureEnable,
    MouseCaptureDisable,
    FocusEventsEnable,
    FocusEventsDisable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocatedEvent {
    pub kind: Event,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Normal,
    AfterEsc,
    InCsi,
    /// `?` seen as the first intermediate byte after `\x1b[`. We're now
    /// collecting digits up to the final byte (`h` or `l`).
    InCsiPrivate,
}

#[derive(Debug, Clone)]
pub struct Detector {
    state: State,
    sequence_start: Option<usize>,
    /// Accumulated digits between `?` and the final `h`/`l`. Bounded so a
    /// pathological stream can't grow this without limit.
    params: heapless_digits::Buf,
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
            sequence_start: None,
            params: heapless_digits::Buf::new(),
        }
    }

    pub fn reset(&mut self) {
        self.state = State::Normal;
        self.sequence_start = None;
        self.params.clear();
    }

    /// Feed a chunk of bytes; returns events detected within them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.feed_with_offsets(bytes)
            .into_iter()
            .map(|ev| ev.kind)
            .collect()
    }

    /// Feed a chunk of bytes; returns detected events with byte offsets into
    /// the current chunk. `start` points to the ESC byte when the whole mode
    /// sequence appeared in this chunk; for sequences split across chunks it
    /// is clamped to 0 because earlier bytes have already been streamed.
    pub fn feed_with_offsets(&mut self, bytes: &[u8]) -> Vec<LocatedEvent> {
        let mut out = Vec::new();
        if !matches!(self.state, State::Normal) {
            self.sequence_start = None;
        }
        for (idx, &b) in bytes.iter().enumerate() {
            self.step(idx, b, &mut out);
        }
        out
    }

    /// Number of trailing bytes in the most recently fed chunk that are part
    /// of an incomplete escape sequence.
    pub fn pending_len(&self, chunk_len: usize) -> usize {
        if matches!(self.state, State::Normal) {
            0
        } else if let Some(start) = self.sequence_start {
            chunk_len.saturating_sub(start)
        } else {
            chunk_len
        }
    }

    fn step(&mut self, idx: usize, b: u8, out: &mut Vec<LocatedEvent>) {
        const ESC: u8 = 0x1B;
        self.state = match self.state {
            State::Normal => {
                if b == ESC {
                    self.sequence_start = Some(idx);
                    State::AfterEsc
                } else {
                    State::Normal
                }
            }
            State::AfterEsc => match b {
                b'[' => State::InCsi,
                ESC => {
                    self.sequence_start = Some(idx);
                    State::AfterEsc
                }
                _ => {
                    self.sequence_start = None;
                    State::Normal
                }
            },
            State::InCsi => match b {
                b'?' => {
                    self.params.clear();
                    State::InCsiPrivate
                }
                ESC => {
                    self.sequence_start = Some(idx);
                    State::AfterEsc
                }
                // Any other byte means this CSI isn't a private mode set/reset
                // (or the parser would need to track parameters / intermediates).
                // We don't care — bail back to ground.
                _ => {
                    self.sequence_start = None;
                    State::Normal
                }
            },
            State::InCsiPrivate => match b {
                b'0'..=b'9' | b';' => {
                    self.params.push(b);
                    State::InCsiPrivate
                }
                b'h' => {
                    if self.is_alt_screen_mode() {
                        out.push(LocatedEvent {
                            kind: Event::AltScreenEnter,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_bracketed_paste_mode() {
                        out.push(LocatedEvent {
                            kind: Event::BracketedPasteEnable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_mouse_capture_mode() {
                        out.push(LocatedEvent {
                            kind: Event::MouseCaptureEnable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_focus_events_mode() {
                        out.push(LocatedEvent {
                            kind: Event::FocusEventsEnable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    self.params.clear();
                    self.sequence_start = None;
                    State::Normal
                }
                b'l' => {
                    if self.is_alt_screen_mode() {
                        out.push(LocatedEvent {
                            kind: Event::AltScreenExit,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_bracketed_paste_mode() {
                        out.push(LocatedEvent {
                            kind: Event::BracketedPasteDisable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_mouse_capture_mode() {
                        out.push(LocatedEvent {
                            kind: Event::MouseCaptureDisable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    if self.is_focus_events_mode() {
                        out.push(LocatedEvent {
                            kind: Event::FocusEventsDisable,
                            start: self.sequence_start.unwrap_or(0),
                            end: idx + 1,
                        });
                    }
                    self.params.clear();
                    self.sequence_start = None;
                    State::Normal
                }
                ESC => {
                    self.params.clear();
                    self.sequence_start = Some(idx);
                    State::AfterEsc
                }
                // Any other byte → abandon: this isn't a mode set/reset we
                // recognize. (e.g. `\x1b[?25h` for cursor visibility — same
                // shape, not interesting.)
                _ => {
                    self.params.clear();
                    self.sequence_start = None;
                    State::Normal
                }
            },
        };
    }

    /// True if the accumulated params name an alt-screen mode: 47, 1047, or 1049.
    /// Multi-mode sequences like `\x1b[?1049;25h` are handled — any of the
    /// listed numbers anywhere in the param list counts.
    fn is_alt_screen_mode(&self) -> bool {
        self.params
            .as_slice()
            .split(|&b| b == b';')
            .any(|n| matches!(n, b"47" | b"1047" | b"1049"))
    }

    fn is_bracketed_paste_mode(&self) -> bool {
        self.params
            .as_slice()
            .split(|&b| b == b';')
            .any(|n| n == b"2004")
    }

    fn is_mouse_capture_mode(&self) -> bool {
        self.params
            .as_slice()
            .split(|&b| b == b';')
            .any(|n| matches!(n, b"1000" | b"1002" | b"1003" | b"1006" | b"1015"))
    }

    fn is_focus_events_mode(&self) -> bool {
        self.params
            .as_slice()
            .split(|&b| b == b';')
            .any(|n| n == b"1004")
    }
}

mod heapless_digits {
    /// Tiny fixed-size byte buffer for accumulating CSI parameter digits.
    /// 32 bytes is plenty for the worst legitimate `?<num>;<num>;…` we care
    /// about; longer inputs are silently truncated, which is fine because
    /// the detector only checks for short specific tokens.
    const CAP: usize = 32;

    #[derive(Debug, Clone)]
    pub struct Buf {
        data: [u8; CAP],
        len: usize,
    }

    impl Buf {
        pub const fn new() -> Self {
            Self {
                data: [0; CAP],
                len: 0,
            }
        }
        pub fn clear(&mut self) {
            self.len = 0;
        }
        pub fn push(&mut self, b: u8) {
            if self.len < CAP {
                self.data[self.len] = b;
                self.len += 1;
            }
        }
        pub fn as_slice(&self) -> &[u8] {
            &self.data[..self.len]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bytes: &[u8]) -> Vec<Event> {
        let mut d = Detector::new();
        d.feed(bytes)
    }

    #[test]
    fn detects_alt_screen_enter_1049() {
        assert_eq!(ev(b"\x1b[?1049h"), vec![Event::AltScreenEnter]);
    }

    #[test]
    fn detects_alt_screen_exit_1049() {
        assert_eq!(ev(b"\x1b[?1049l"), vec![Event::AltScreenExit]);
    }

    #[test]
    fn detects_legacy_47() {
        assert_eq!(ev(b"\x1b[?47h"), vec![Event::AltScreenEnter]);
        assert_eq!(ev(b"\x1b[?47l"), vec![Event::AltScreenExit]);
    }

    #[test]
    fn detects_legacy_1047() {
        assert_eq!(ev(b"\x1b[?1047h"), vec![Event::AltScreenEnter]);
        assert_eq!(ev(b"\x1b[?1047l"), vec![Event::AltScreenExit]);
    }

    #[test]
    fn ignores_unrelated_private_modes() {
        assert!(ev(b"\x1b[?25h").is_empty(), "cursor show is not alt-screen");
        assert!(ev(b"\x1b[?25l").is_empty(), "cursor hide");
    }

    #[test]
    fn detects_bracketed_paste_mode() {
        assert_eq!(ev(b"\x1b[?2004h"), vec![Event::BracketedPasteEnable]);
        assert_eq!(ev(b"\x1b[?2004l"), vec![Event::BracketedPasteDisable]);
    }

    #[test]
    fn detects_bracketed_paste_inside_combined_mode_set() {
        assert_eq!(ev(b"\x1b[?25;2004h"), vec![Event::BracketedPasteEnable]);
        assert_eq!(ev(b"\x1b[?2004;25l"), vec![Event::BracketedPasteDisable]);
    }

    #[test]
    fn detects_mouse_capture_modes() {
        assert_eq!(ev(b"\x1b[?1000h"), vec![Event::MouseCaptureEnable]);
        assert_eq!(ev(b"\x1b[?1002h"), vec![Event::MouseCaptureEnable]);
        assert_eq!(ev(b"\x1b[?1003h"), vec![Event::MouseCaptureEnable]);
        assert_eq!(ev(b"\x1b[?1006h"), vec![Event::MouseCaptureEnable]);
        assert_eq!(ev(b"\x1b[?1015h"), vec![Event::MouseCaptureEnable]);
        assert_eq!(ev(b"\x1b[?1006l"), vec![Event::MouseCaptureDisable]);
    }

    #[test]
    fn detects_focus_event_mode() {
        assert_eq!(ev(b"\x1b[?1004h"), vec![Event::FocusEventsEnable]);
        assert_eq!(ev(b"\x1b[?1004l"), vec![Event::FocusEventsDisable]);
    }

    #[test]
    fn detects_multiple_interesting_modes_in_one_sequence() {
        assert_eq!(
            ev(b"\x1b[?1049;2004;1006;1004h"),
            vec![
                Event::AltScreenEnter,
                Event::BracketedPasteEnable,
                Event::MouseCaptureEnable,
                Event::FocusEventsEnable
            ]
        );
        assert_eq!(
            ev(b"\x1b[?1049;2004;1006;1004l"),
            vec![
                Event::AltScreenExit,
                Event::BracketedPasteDisable,
                Event::MouseCaptureDisable,
                Event::FocusEventsDisable
            ]
        );
    }

    #[test]
    fn ignores_unrelated_csi() {
        assert!(ev(b"\x1b[31m").is_empty(), "SGR red");
        assert!(ev(b"\x1b[2J").is_empty(), "clear screen");
        assert!(ev(b"\x1b[H").is_empty(), "cursor home");
    }

    #[test]
    fn handles_split_chunks() {
        let mut d = Detector::new();
        let mut all = vec![];
        all.extend(d.feed(b"\x1b"));
        all.extend(d.feed(b"["));
        all.extend(d.feed(b"?"));
        all.extend(d.feed(b"10"));
        all.extend(d.feed(b"49"));
        all.extend(d.feed(b"h"));
        assert_eq!(all, vec![Event::AltScreenEnter]);
    }

    #[test]
    fn reports_pending_incomplete_sequence_len() {
        let mut d = Detector::new();
        assert!(d.feed_with_offsets(b"abc\x1b[?10").is_empty());
        assert_eq!(d.pending_len(8), 5);

        let events = d.feed_with_offsets(b"49hxyz");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Event::AltScreenEnter);
        assert_eq!(d.pending_len(6), 0);
    }

    #[test]
    fn detects_in_combined_mode_set() {
        // vim sometimes flips multiple modes in one CSI: `?1049;25h`.
        assert_eq!(ev(b"\x1b[?1049;25h"), vec![Event::AltScreenEnter]);
        assert_eq!(ev(b"\x1b[?25;1049l"), vec![Event::AltScreenExit]);
    }

    #[test]
    fn handles_real_world_vim_sequence() {
        // Approximate vim-like opening burst: clear screen + alt screen on +
        // hide cursor + a bit of SGR. Only the alt-screen flip should fire.
        let bytes = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[1;1H\x1b[31m";
        assert_eq!(ev(bytes), vec![Event::AltScreenEnter]);
    }

    #[test]
    fn back_to_back_enter_exit() {
        // Cycle through alt screen — useful for fzf which enters and exits
        // quickly. Each transition should be reported.
        let bytes = b"\x1b[?1049h some output \x1b[?1049l";
        assert_eq!(ev(bytes), vec![Event::AltScreenEnter, Event::AltScreenExit]);
    }

    #[test]
    fn reports_offsets_for_same_chunk_events() {
        let mut d = Detector::new();
        let bytes = b"before\x1b[?1049hinside\x1b[?1049lafter";
        let events = d.feed_with_offsets(bytes);
        assert_eq!(
            events,
            vec![
                LocatedEvent {
                    kind: Event::AltScreenEnter,
                    start: 6,
                    end: 14,
                },
                LocatedEvent {
                    kind: Event::AltScreenExit,
                    start: 20,
                    end: 28,
                },
            ]
        );
    }

    #[test]
    fn malformed_sequences_do_not_panic() {
        let _ = ev(b"\x1b[?");
        let _ = ev(b"\x1b[?;;;;;");
        let _ = ev(b"\x1b[?abc");
        let _ = ev(b"\x1b\x1b\x1b[?1049h");
        let _ = ev(&[0xFF; 1024]);
    }

    #[test]
    fn esc_resyncs_in_csi_private() {
        // A bare ESC mid-CSI should reset to AfterEsc and let the next
        // sequence be detected.
        let bytes = b"\x1b[?123\x1b[?1049h";
        assert_eq!(ev(bytes), vec![Event::AltScreenEnter]);
    }
}
