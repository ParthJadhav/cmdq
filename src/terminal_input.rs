//! Decode keys for the queue without discarding terminal replies intended for
//! the hosted shell. Crossterm's public event reader consumes DA/CPR responses
//! internally and cannot serve as a transparent PTY input transport.

mod parse;

use crossterm::event::Event;
use nix::poll::{PollFd, PollFlags, poll};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

pub enum Input {
    Event(Event),
    Reply(Vec<u8>),
}

pub struct Reader {
    buffer: Vec<u8>,
    pending_since: Instant,
    size: (u16, u16),
    second_escape: bool,
    /// Original bytes for the last decoded event, retained for PTY forwarding.
    pub event_bytes: Vec<u8>,
    /// Poked by the PTY reader thread whenever shell output arrives, so a
    /// `read` blocked on the terminal returns early.
    output_wakeup: Option<std::os::unix::net::UnixStream>,
    /// Cleared after each drain so the reader thread re-arms the wakeup.
    output_wakeup_armed: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Reader {
    pub fn new(size: (u16, u16)) -> Self {
        Self {
            buffer: Vec::new(),
            pending_since: Instant::now(),
            size,
            second_escape: false,
            event_bytes: Vec::new(),
            output_wakeup: None,
            output_wakeup_armed: None,
        }
    }

    pub fn set_output_wakeup(
        &mut self,
        wakeup: std::os::unix::net::UnixStream,
        armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.output_wakeup = Some(wakeup);
        self.output_wakeup_armed = Some(armed);
    }

    /// Bytes of a partially received key sequence are waiting on a timer.
    pub fn has_pending_input(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn drain_output_wakeup(&mut self) {
        if let Some(wakeup) = &mut self.output_wakeup {
            let mut sink = [0u8; 64];
            while matches!(wakeup.read(&mut sink), Ok(n) if n > 0) {}
        }
        // Disarm only after the socket is empty: a chunk that lands in
        // between is already queued for the caller's next channel drain, and
        // anything after this point signals afresh.
        if let Some(armed) = &self.output_wakeup_armed {
            armed.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        let size = crossterm::terminal::size()?;
        if size != self.size {
            self.size = size;
            self.event_bytes.clear();
            return Ok(Some(Input::Event(Event::Resize(size.0, size.1))));
        }
        if let Some(input) = self.take() {
            return Ok(Some(input));
        }
        let stdin = io::stdin();
        let (stdin_ready, output_ready) = {
            let mut fds = Vec::with_capacity(2);
            fds.push(PollFd::new(stdin.as_fd(), PollFlags::POLLIN));
            if let Some(wakeup) = &self.output_wakeup {
                fds.push(PollFd::new(wakeup.as_fd(), PollFlags::POLLIN));
            }
            match poll(&mut fds, timeout.as_millis().min(u16::MAX as u128) as u16) {
                Ok(0) => (false, false),
                Err(nix::errno::Errno::EINTR) => return Ok(None),
                Err(error) => return Err(error.into()),
                Ok(_) => (
                    fds[0].revents().is_some_and(|r| !r.is_empty()),
                    fds.get(1)
                        .and_then(|fd| fd.revents())
                        .is_some_and(|r| !r.is_empty()),
                ),
            }
        };
        if output_ready {
            self.drain_output_wakeup();
        }
        if !stdin_ready {
            return Ok(self.take());
        }
        let mut bytes = [0; 8192];
        let count = stdin.lock().read(&mut bytes)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            ));
        }
        self.buffer.extend_from_slice(&bytes[..count]);
        self.pending_since = Instant::now();
        Ok(self.take())
    }

    /// Ask the terminal where its cursor is (DSR 6) and wait up to `timeout`
    /// for the CPR reply. Any keys or other replies that arrive in the
    /// meantime stay buffered for the normal [`Reader::read`] flow. Returns
    /// zero-based `(col, row)`, or `None` when the terminal did not answer.
    pub fn query_cursor_position(
        &mut self,
        out: &mut impl Write,
        timeout: Duration,
    ) -> io::Result<Option<(u16, u16)>> {
        out.write_all(b"\x1b[6n")?;
        out.flush()?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(position) = self.take_cursor_report() {
                return Ok(Some(position));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let stdin = io::stdin();
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, remaining.as_millis().min(u16::MAX as u128) as u16) {
                Ok(0) => return Ok(self.take_cursor_report()),
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            let mut bytes = [0; 8192];
            let count = stdin.lock().read(&mut bytes)?;
            if count == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&bytes[..count]);
            self.pending_since = Instant::now();
        }
    }

    /// Remove the first complete `ESC [ row ; col R` report from the buffer
    /// and return it as zero-based `(col, row)`.
    fn take_cursor_report(&mut self) -> Option<(u16, u16)> {
        let (range, position) = find_cursor_report(&self.buffer)?;
        self.buffer.drain(range);
        Some(position)
    }

    fn take(&mut self) -> Option<Input> {
        if self.buffer.is_empty() {
            return None;
        }
        // Esc Esc must remain two Esc keys; interpreting the second as an
        // Alt prefix would steal the next typed character from the editor.
        if self.second_escape || self.buffer.starts_with(b"\x1b\x1b") {
            self.second_escape = !self.second_escape;
            self.buffer.remove(0);
            self.event_bytes = vec![0x1b];
            return Some(Input::Event(Event::Key(
                crossterm::event::KeyCode::Esc.into(),
            )));
        }
        let length = token_length(&self.buffer).or_else(|| {
            let paste = self.buffer.starts_with(b"\x1b[200~");
            let delay = if self.buffer == b"\x1b" { 40 } else { 600 };
            (!paste && self.pending_since.elapsed() >= Duration::from_millis(delay))
                .then_some(self.buffer.len())
        })?;
        let bytes: Vec<_> = self.buffer.drain(..length).collect();
        self.event_bytes.clone_from(&bytes);
        let parsed = if bytes.starts_with(b"\x1b]")
            || bytes.starts_with(b"\x1bP")
            || bytes.starts_with(b"\x1b_")
            || bytes.starts_with(b"\x1b^")
        {
            None
        } else {
            parse::parse_event(&bytes, false).ok().flatten()
        };
        Some(match parsed {
            Some(parse::InternalEvent::Event(event)) => Input::Event(event),
            _ => Input::Reply(bytes),
        })
    }
}

fn find_cursor_report(bytes: &[u8]) -> Option<(std::ops::Range<usize>, (u16, u16))> {
    let mut start = 0;
    while start + 1 < bytes.len() {
        if bytes[start] != 0x1b || bytes[start + 1] != b'[' {
            start += 1;
            continue;
        }
        let body_start = start + 2;
        let mut end = body_start;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
            end += 1;
        }
        if end > body_start && bytes.get(end) == Some(&b'R') {
            let body = std::str::from_utf8(&bytes[body_start..end]).ok()?;
            let mut parts = body.split(';');
            let row = parts.next().and_then(|s| s.parse::<u16>().ok());
            let col = parts.next().and_then(|s| s.parse::<u16>().ok());
            if let (Some(row), Some(col), None) = (row, col, parts.next())
                && row >= 1
                && col >= 1
            {
                return Some((start..end + 1, (col - 1, row - 1)));
            }
        }
        start += 1;
    }
    None
}

fn token_length(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    if first != 0x1b {
        let width = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 1,
        };
        return (bytes.len() >= width).then_some(width);
    }
    match *bytes.get(1)? {
        b'[' => {
            // Linux console function keys use ESC [ [ A through E.
            if bytes.starts_with(b"\x1b[[") {
                return (bytes.len() >= 4).then_some(4);
            }
            if bytes.starts_with(b"\x1b[200~") {
                return bytes[6..]
                    .windows(6)
                    .position(|w| w == b"\x1b[201~")
                    .map(|end| end + 12);
            }
            if bytes.starts_with(b"\x1b[M") {
                return (bytes.len() >= 6).then_some(6);
            }
            bytes[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map(|end| end + 3)
        }
        b'O' => (bytes.len() >= 3).then_some(3),
        b']' | b'P' | b'_' | b'^' => {
            for index in 2..bytes.len() {
                if (bytes[1] == b']' && bytes[index] == 7)
                    || (bytes[index - 1] == 0x1b && bytes[index] == b'\\')
                {
                    return Some(index + 1);
                }
            }
            // Bound malformed/unterminated reply buffers.
            (bytes.len() >= 1024 * 1024).then_some(bytes.len())
        }
        0x1b => Some(1),
        _ => token_length(&bytes[1..]).map(|length| length + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn replies_survive_every_chunk_boundary() {
        for reply in [
            b"\x1b[?1;2c".as_slice(),
            b"\x1b[12;40R",
            b"\x1b[?0u",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1bP>|terminal 1.0\x1b\\",
        ] {
            for split in 1..reply.len() {
                let mut reader = Reader::new((80, 24));
                reader.buffer.extend_from_slice(&reply[..split]);
                assert!(
                    reader.take().is_none(),
                    "premature reply at {split}: {reply:?}"
                );
                reader.buffer.extend_from_slice(&reply[split..]);
                assert!(matches!(reader.take(), Some(Input::Reply(bytes)) if bytes == reply));
            }
        }
    }

    #[test]
    fn paste_unicode_modified_keys_and_controls_stay_events() {
        let mut reader = Reader::new((80, 24));
        reader
            .buffer
            .extend_from_slice("\x1b[200~hello\n世界\x1b[201~\x1b[1;3D\x1c".as_bytes());
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Paste(text))) if text == "hello\n世界")
        );
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Left && key.modifiers == KeyModifiers::ALT)
        );
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Char('\\') && key.modifiers == KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn bare_escape_expires_but_fragmented_unicode_waits() {
        let mut reader = Reader::new((80, 24));
        reader.buffer.push(0x1b);
        assert!(reader.take().is_none());
        reader.pending_since = Instant::now() - Duration::from_millis(50);
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Esc)
        );
        reader.buffer.extend_from_slice(&[0xe4, 0xb8]);
        assert!(reader.take().is_none());
        reader.buffer.push(0x96);
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Char('世'))
        );
    }

    #[test]
    fn double_escape_does_not_consume_next_letter_as_alt_key() {
        let mut reader = Reader::new((80, 24));
        reader.buffer.extend_from_slice(b"\x1b\x1bprintf");
        for _ in 0..2 {
            assert!(
                matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Esc)
            );
        }
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Char('p') && key.modifiers.is_empty())
        );
    }

    #[test]
    fn malformed_coordinates_do_not_panic_or_corrupt_following_keys() {
        for bytes in [b"\x1b[0;0R".as_slice(), b"\x1b[<0;0;0M", b"\x1b[M   "] {
            let mut reader = Reader::new((80, 24));
            reader.buffer.extend_from_slice(bytes);
            reader.buffer.push(b'x');
            assert!(matches!(reader.take(), Some(Input::Reply(raw)) if raw == bytes));
            assert!(
                matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Char('x'))
            );
        }
    }

    #[test]
    fn cursor_report_is_extracted_and_other_input_is_kept() {
        let mut reader = Reader::new((80, 24));
        reader.buffer.extend_from_slice(b"ls\x1b[12;40R\x1b[?1;2c");
        assert_eq!(reader.take_cursor_report(), Some((39, 11)));
        assert_eq!(reader.buffer, b"ls\x1b[?1;2c");
        assert!(reader.take_cursor_report().is_none());
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::Char('l'))
        );
    }

    #[test]
    fn partial_or_malformed_cursor_reports_are_not_extracted() {
        for bytes in [
            b"\x1b[12;".as_slice(),
            b"\x1b[0;5R",
            b"\x1b[12R",
            b"\x1b[1;2;3R",
        ] {
            let mut reader = Reader::new((80, 24));
            reader.buffer.extend_from_slice(bytes);
            assert!(reader.take_cursor_report().is_none(), "{bytes:?}");
            assert_eq!(reader.buffer, bytes);
        }
    }

    #[test]
    fn linux_console_function_key_is_one_event() {
        let mut reader = Reader::new((80, 24));
        reader.buffer.extend_from_slice(b"\x1b[[A");
        assert!(
            matches!(reader.take(), Some(Input::Event(Event::Key(key))) if key.code == KeyCode::F(1))
        );
        assert!(reader.buffer.is_empty());
    }
}
