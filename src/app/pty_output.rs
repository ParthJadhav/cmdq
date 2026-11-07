//! Shell output: read on a thread, passed straight through to the terminal,
//! and sniffed on the way for shell-integration markers and child mode
//! switches, which are applied in byte order with the output around them.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use crossterm::{event::EnableBracketedPaste, execute};

use super::AppState;
use super::lifecycle::{
    handle_osc_event, refresh_auto_passthrough_for_child_modes, update_child_input_detection,
};
use super::screen::Screen;
use crate::mode_detect;
use crate::osc133;

#[cfg(unix)]
pub(super) type OutputWakeup = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
pub(super) type OutputWakeup = ();

/// Chunks read from the PTY. `wakeup` is readable after new output, so the
/// main loop's input poll returns immediately instead of at its timeout.
pub(super) struct PtyOutput {
    pub(super) chunks: mpsc::Receiver<Vec<u8>>,
    pub(super) wakeup: Option<OutputWakeup>,
    /// Set while a wakeup byte is unread, so a burst of reads costs one write.
    pub(super) wakeup_armed: Arc<AtomicBool>,
}

pub(super) fn spawn_reader(mut reader: Box<dyn Read + Send>) -> PtyOutput {
    // Backpressure keeps a noisy command from growing memory without bound.
    let (tx, chunks) = mpsc::sync_channel::<Vec<u8>>(32);
    let (wake_tx, wakeup) = output_wakeup_pair();
    let wakeup_armed = Arc::new(AtomicBool::new(false));
    let armed = wakeup_armed.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    signal_output_wakeup(&wake_tx, &armed);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    PtyOutput {
        chunks,
        wakeup,
        wakeup_armed,
    }
}

#[cfg(unix)]
fn output_wakeup_pair() -> (Option<OutputWakeup>, Option<OutputWakeup>) {
    match std::os::unix::net::UnixStream::pair() {
        Ok((tx, rx)) if tx.set_nonblocking(true).is_ok() && rx.set_nonblocking(true).is_ok() => {
            (Some(tx), Some(rx))
        }
        _ => (None, None),
    }
}

#[cfg(not(unix))]
fn output_wakeup_pair() -> (Option<OutputWakeup>, Option<OutputWakeup>) {
    (None, None)
}

fn signal_output_wakeup(tx: &Option<OutputWakeup>, armed: &AtomicBool) {
    #[cfg(unix)]
    if let Some(tx) = tx
        && !armed.swap(true, Ordering::AcqRel)
    {
        // A full socket buffer means a wakeup is already pending.
        let _ = (&*tx).write(&[1u8]);
    }
    #[cfg(not(unix))]
    let _ = (tx, armed);
}

/// The marker and mode detectors that watch shell output, plus the tail of a
/// mode sequence split across reads. That tail is held until it completes
/// (or the shell exits), never timed out: programs and network links can
/// pause anywhere inside a sequence, and a panel repaint written in the gap
/// would splice into it.
pub(super) struct OutputSniffer {
    osc: osc133::Detector,
    mode: mode_detect::Detector,
    pending: Vec<u8>,
}

impl OutputSniffer {
    pub(super) fn new() -> Self {
        Self {
            osc: osc133::Detector::for_cmdq(),
            mode: mode_detect::Detector::new(),
            pending: Vec::new(),
        }
    }

    /// Forward one PTY read to the terminal, applying markers and mode
    /// switches at their position in the stream.
    pub(super) fn forward(
        &mut self,
        chunk: Vec<u8>,
        state: &mut AppState,
        screen: &mut Screen,
        shell: &mut dyn Write,
    ) -> Result<()> {
        let mut bytes = if self.pending.is_empty() {
            chunk
        } else {
            let mut combined = std::mem::take(&mut self.pending);
            combined.extend_from_slice(&chunk);
            combined
        };
        let osc_events = self.osc.feed_with_offsets(&bytes);
        let mode_events = self.mode.feed_with_offsets(&bytes);
        let pending_len = self.mode.pending_len(bytes.len());
        let process_len = bytes.len().saturating_sub(pending_len);
        if pending_len > 0 {
            self.pending.extend_from_slice(&bytes[process_len..]);
            bytes.truncate(process_len);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        screen.begin_shell_output()?;
        let mut write_pos = 0usize;
        let mut osc_events = osc_events.into_iter().peekable();
        for ev in &mode_events {
            while let Some(osc_ev) = osc_events.next_if(|osc_ev| osc_ev.end <= ev.start) {
                handle_osc_event(osc_ev.event, state, screen, shell)?;
            }
            let start = ev.start.min(bytes.len()).max(write_pos);
            let end = ev.end.min(bytes.len()).max(start);
            apply_mode_event(
                ev.kind,
                &bytes[write_pos..start],
                &bytes[start..end],
                state,
                screen,
            )?;
            write_pos = end;
        }
        screen.write_shell_output(&bytes[write_pos..], !state.auto_passthrough);

        // OSC 133 markers drive cmdq's command lifecycle. Apply them in byte
        // order relative to mode flips in the same PTY read, so
        // `CommandStart` followed by child bracketed-paste enable leaves
        // paste forwarding enabled.
        for osc_ev in osc_events {
            handle_osc_event(osc_ev.event, state, screen, shell)?;
        }
        update_child_input_detection(state, &bytes);
        Ok(())
    }

    /// The shell exited: write out a mode sequence it never finished.
    pub(super) fn flush_pending(
        &mut self,
        state: &mut AppState,
        screen: &mut Screen,
    ) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        screen.begin_shell_output()?;
        screen.write_shell_output(&self.pending, !state.auto_passthrough);
        state.running_output_tail.feed(&self.pending);
        self.pending.clear();
        self.mode.reset();
        let _ = screen.out.flush();
        Ok(())
    }
}

/// Write `before` (ordinary output) and `sequence` (the mode switch itself)
/// and record what the child asked for.
fn apply_mode_event(
    kind: mode_detect::Event,
    before: &[u8],
    sequence: &[u8],
    state: &mut AppState,
    screen: &mut Screen,
) -> Result<()> {
    use mode_detect::Event;

    let track_cursor = !state.auto_passthrough;
    match kind {
        // Leaving the shell's screen: the panel goes before the child's
        // sequence reaches the terminal.
        Event::AltScreenEnter | Event::MouseCaptureEnable => {
            screen.write_shell_output(before, track_cursor);
            let status = if kind == Event::AltScreenEnter {
                state.child.alt_screen = true;
                "alt-screen detected — keys go to the running app"
            } else {
                state.child.mouse_capture = true;
                "mouse tracking detected — keys and clicks go to the running app"
            };
            refresh_auto_passthrough_for_child_modes(state, screen, Some(status))?;
            screen.write_shell_output(sequence, false);
        }
        Event::AltScreenExit => {
            screen.write_shell_output(before, false);
            screen.write_shell_output(sequence, false);
            state.child.alt_screen = false;
            refresh_auto_passthrough_for_child_modes(
                state,
                screen,
                Some("alt-screen exit — queue mode restored"),
            )?;
        }
        Event::MouseCaptureDisable => {
            screen.write_shell_output(before, track_cursor);
            screen.write_shell_output(sequence, track_cursor);
            state.child.mouse_capture = false;
            refresh_auto_passthrough_for_child_modes(
                state,
                screen,
                Some("mouse tracking off — queue mode restored"),
            )?;
        }
        Event::BracketedPasteEnable
        | Event::BracketedPasteDisable
        | Event::FocusEventsEnable
        | Event::FocusEventsDisable => {
            screen.write_shell_output(before, track_cursor);
            screen.write_shell_output(sequence, track_cursor);
            match kind {
                Event::BracketedPasteEnable => state.child.bracketed_paste = true,
                Event::BracketedPasteDisable => {
                    state.child.bracketed_paste = false;
                    // The child disabled its paste mode, but cmdq still needs
                    // the outer terminal to report paste events so it can
                    // route them correctly.
                    let _ = execute!(screen.out, EnableBracketedPaste);
                }
                Event::FocusEventsEnable => state.child.focus_events = true,
                _ => state.child.focus_events = false,
            }
        }
    }
    Ok(())
}
