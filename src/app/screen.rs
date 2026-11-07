//! The user's terminal as cmdq sees it: the buffered output stream, the rows
//! reserved for the panel, the shell's PTY size that follows them, and the
//! record the exit paths use to put the terminal back.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    QueueableCommand,
    cursor::{MoveTo, Show},
    event::{DisableBracketedPaste, PopKeyboardEnhancementFlags},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode},
};

use crate::cursor_tracker::CursorTracker;
use crate::panel::{self, PanelState};
use crate::pty::ShellPty;

/// Maximum interval between repaints when the panel is visible. We also
/// repaint immediately when state changes, so this is just a backstop for
/// the status-message fade timer.
const PANEL_MIN_REPAINT_INTERVAL: Duration = Duration::from_millis(33);

const MIN_PANEL_COLS: u16 = 20;
const MIN_PANEL_ROWS: u16 = 5;

const ALT_SCREEN_EXIT: &str = "\x1b[?1049l\x1b[?1047l\x1b[?47l";
const MOUSE_TRACKING_OFF: &str = "\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const FOCUS_EVENTS_OFF: &str = "\x1b[?1004l";

/// Whether cmdq is currently reserving panel rows on the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelLayout {
    Hidden,
    /// Bottom `height` rows reserved for cmdq. Shell scrolling confined above.
    Reserved {
        height: u16,
    },
}

/// What the signal, panic and drop paths need to put the terminal back.
#[derive(Debug, Clone, Copy)]
pub(super) struct TerminalRestoreState {
    layout: PanelLayout,
    rows: u16,
    alt_screen: bool,
}

pub(super) type SharedRestoreState = Arc<Mutex<TerminalRestoreState>>;

pub(super) fn shared_restore_state(rows: u16) -> SharedRestoreState {
    Arc::new(Mutex::new(TerminalRestoreState {
        layout: PanelLayout::Hidden,
        rows,
        alt_screen: false,
    }))
}

pub(super) fn terminal_allows_panel(cols: u16, rows: u16) -> bool {
    cols >= MIN_PANEL_COLS && rows >= MIN_PANEL_ROWS
}

/// The terminal, the panel rows on it, and the shell PTY sized to the rest.
/// Every layout change goes through [`Screen::set_layout`], which keeps the
/// PTY size, the restore record, the shell-cursor tracker and the paint
/// cache in step with the rows actually reserved.
pub(super) struct Screen {
    pub(super) out: Terminal,
    pub(super) pty: ShellPty,
    layout: PanelLayout,
    cols: u16,
    rows: u16,
    restore: SharedRestoreState,
    cursor: CursorTracker,
    paint: PaintClock,
    /// Reflowed panel rows a shrink pushed above the viewport; they come
    /// back into view when the terminal grows again.
    deferred_panel_reflow_rows: Option<u16>,
}

impl Screen {
    pub(super) fn new(pty: ShellPty, restore: SharedRestoreState, cols: u16, rows: u16) -> Self {
        Self {
            out: Terminal::new(),
            pty,
            layout: PanelLayout::Hidden,
            cols,
            rows,
            restore,
            cursor: CursorTracker::new(cols, rows),
            paint: PaintClock::forced(),
            deferred_panel_reflow_rows: None,
        }
    }

    pub(super) fn layout(&self) -> PanelLayout {
        self.layout
    }

    pub(super) fn cols(&self) -> u16 {
        self.cols
    }

    pub(super) fn rows(&self) -> u16 {
        self.rows
    }

    pub(super) fn set_shell_cursor(&mut self, col: u16, row: u16) {
        self.cursor.set_position(col, row);
    }

    /// Move to `desired`: release/reserve rows, size the PTY to the rows the
    /// shell owns, record the layout for the exit paths, re-sync the cursor
    /// tracker and repaint. Re-sizing to an unchanged size is free: the
    /// kernel only signals the shell when the size differs.
    pub(super) fn set_layout(&mut self, desired: PanelLayout) -> Result<()> {
        if self.layout != desired {
            self.transition(desired)?;
        }
        self.resize_pty();
        self.record_layout();
        self.sync_cursor_tracker();
        self.paint.force();
        Ok(())
    }

    pub(super) fn hide_panel(&mut self) -> Result<()> {
        self.set_layout(PanelLayout::Hidden)
    }

    fn transition(&mut self, desired: PanelLayout) -> Result<()> {
        let (rows, cols) = (self.rows, self.cols);
        let shell_cursor = self.cursor.position();
        if let PanelLayout::Reserved { height } = self.layout {
            panel::release(&mut self.out, height, rows)?;
            // Resetting DECSTBM moves the real terminal cursor. Put it back on
            // the shell cursor that we tracked from PTY output before resizing
            // the child; otherwise a SIGWINCH prompt redraw can leave an orphan
            // prompt at the old row and a second prompt at the screen bottom.
            self.out.queue(MoveTo(
                shell_cursor.0.min(cols.saturating_sub(1)),
                shell_cursor.1.min(rows.saturating_sub(1)),
            ))?;
            self.out.flush()?;
        }
        if let PanelLayout::Reserved { height } = desired {
            panel::reserve(&mut self.out, height, rows, cols, shell_cursor)?;
        }
        self.layout = desired;
        Ok(())
    }

    fn shell_rows(&self) -> u16 {
        match self.layout {
            PanelLayout::Hidden => self.rows,
            PanelLayout::Reserved { height } => self.rows.saturating_sub(height).max(1),
        }
    }

    pub(super) fn resize_pty(&mut self) {
        let _ = self.pty.resize(self.cols, self.shell_rows());
    }

    fn sync_cursor_tracker(&mut self) {
        let (col, row) = self.cursor.position();
        self.cursor.set_size(self.cols, self.shell_rows());
        match self.layout {
            PanelLayout::Hidden => self.cursor.set_position(col, row),
            PanelLayout::Reserved { height } => {
                let scroll_bottom = self.rows.saturating_sub(height);
                let scrolled_rows = row.saturating_add(1).saturating_sub(scroll_bottom);
                self.cursor
                    .set_position(col, row.saturating_sub(scrolled_rows));
            }
        }
    }

    fn record_layout(&self) {
        if let Ok(mut restore) = self.restore.lock() {
            restore.layout = self.layout;
            restore.rows = self.rows;
        }
    }

    /// Turn off terminal modes a finished program left on.
    pub(super) fn end_child_modes(
        &mut self,
        alt_screen: bool,
        mouse_capture: bool,
        focus_events: bool,
    ) -> Result<()> {
        if alt_screen {
            write!(self.out, "{ALT_SCREEN_EXIT}")?;
        }
        if mouse_capture {
            write!(self.out, "{MOUSE_TRACKING_OFF}")?;
        }
        if focus_events {
            write!(self.out, "{FOCUS_EVENTS_OFF}")?;
        }
        if alt_screen || mouse_capture || focus_events {
            self.out.flush()?;
        }
        self.record_alt_screen(false);
        Ok(())
    }

    pub(super) fn record_alt_screen(&self, alt_screen: bool) {
        if let Ok(mut restore) = self.restore.lock() {
            restore.alt_screen = alt_screen;
        }
    }

    /// Put the terminal cursor back where the shell left it before writing
    /// shell output: painting the panel moved it into the panel rows.
    pub(super) fn begin_shell_output(&mut self) -> Result<()> {
        if matches!(self.layout, PanelLayout::Reserved { .. }) {
            let (col, row) = self.cursor.position();
            self.out.queue(MoveTo(col, row))?;
        }
        Ok(())
    }

    /// Write shell output through. The cursor tracker follows it unless a
    /// full-screen child owns the terminal and its cursor.
    pub(super) fn write_shell_output(&mut self, bytes: &[u8], track_cursor: bool) {
        let _ = self.out.write_all(bytes);
        if track_cursor {
            self.cursor.feed(bytes);
        }
    }

    /// Apply a terminal resize. `reflowed_panel_rows` is how many rows the
    /// painted panel occupies after the terminal reflowed it to the new width.
    /// The caller then chooses the layout for the new size.
    pub(super) fn resize(&mut self, cols: u16, rows: u16, reflowed_panel_rows: u16) -> Result<()> {
        let (old_cols, old_rows) = (self.cols, self.rows);
        let old_reserved = matches!(self.layout, PanelLayout::Reserved { .. });
        let (new_cols, new_rows) = (cols.max(1), rows.max(1));
        let shrinking = new_cols < old_cols || new_rows < old_rows;
        if old_reserved && shrinking {
            self.layout = PanelLayout::Hidden;
        } else if old_reserved {
            self.transition(PanelLayout::Hidden)?;
            self.resize_pty();
        }
        self.cols = new_cols;
        self.rows = new_rows;
        self.record_layout();
        if old_reserved {
            panel::restore_full_screen(&mut self.out, new_rows)?;
            if shrinking {
                let (col, row) = clear_reflowed_panel_after_shrink(
                    &mut self.out,
                    reflowed_panel_rows,
                    new_cols,
                    new_rows,
                    self.cursor.position(),
                )?;
                self.cursor.set_size(new_cols, new_rows);
                self.cursor.set_position(col, row);
                self.deferred_panel_reflow_rows =
                    Some(reflowed_panel_rows.saturating_sub(new_rows)).filter(|rows| *rows > 0);
            }
        }
        if new_rows > old_rows
            && let Some(reflow_rows) = self.deferred_panel_reflow_rows.take()
        {
            panel::restore_full_screen(&mut self.out, new_rows)?;
            clear_reappeared_panel_rows(&mut self.out, reflow_rows, old_rows, new_rows)?;
            let (col, row) = self.cursor.position();
            self.cursor.set_size(new_cols, new_rows);
            self.cursor
                .set_position(col, row.saturating_add(new_rows - old_rows));
        }
        Ok(())
    }

    /// Paint the panel if what it shows changed since the last paint.
    /// Returns `true` when a change is waiting out the repaint rate limit.
    pub(super) fn paint_panel(
        &mut self,
        view: &PanelState<'_>,
        cursor_in_input: bool,
        output_generation: u64,
    ) -> Result<bool> {
        let PanelLayout::Reserved { height } = self.layout else {
            return Ok(false);
        };
        let key = panel_render_key(
            view,
            height,
            self.rows,
            self.cols,
            cursor_in_input,
            output_generation,
        );
        if self.paint.key == Some(key) {
            return Ok(false);
        }
        if self.paint.at.elapsed() < PANEL_MIN_REPAINT_INTERVAL {
            return Ok(true);
        }
        panel::paint(
            &mut self.out,
            view,
            height,
            self.rows,
            self.cols,
            cursor_in_input,
            self.cursor.position(),
        )?;
        self.paint.painted(key);
        Ok(false)
    }
}

/// Buffered stdout. Shell output is forwarded chunk by chunk, and the line
/// buffering of `io::Stdout` turns every chunk with a newline into its own
/// write syscall; batching them per loop iteration is a large part of the
/// wrapper's cost under heavy output. The main loop flushes every iteration.
pub(super) struct Terminal(Option<io::BufWriter<io::Stdout>>);

impl Terminal {
    fn new() -> Self {
        Self(Some(io::BufWriter::with_capacity(64 * 1024, io::stdout())))
    }

    fn inner(&mut self) -> &mut io::BufWriter<io::Stdout> {
        self.0
            .as_mut()
            .expect("terminal writer is only taken on drop")
    }
}

impl Write for Terminal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner().write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner().write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner().flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(writer) = self.0.take() {
            if std::thread::panicking() {
                // The panic hook has already restored the terminal; flushing
                // stale panel bytes on top of that would only corrupt it.
                let _ = writer.into_parts();
            } else {
                drop(writer);
            }
        }
    }
}

/// When the panel was last painted and what it showed, so idle iterations
/// don't rewrite identical rows (and blink the cursor) every few dozen ms.
struct PaintClock {
    at: Instant,
    key: Option<u64>,
}

impl PaintClock {
    fn forced() -> Self {
        Self {
            at: Instant::now() - PANEL_MIN_REPAINT_INTERVAL,
            key: None,
        }
    }

    fn force(&mut self) {
        *self = Self::forced();
    }

    fn painted(&mut self, key: u64) {
        self.at = Instant::now();
        self.key = Some(key);
    }
}

/// Everything `panel::paint` reads, folded into one value. Output is folded
/// in through `output_generation` because writing shell output moves the
/// terminal cursor away from wherever the panel last parked it.
fn panel_render_key(
    view: &PanelState<'_>,
    height: u16,
    term_rows: u16,
    term_cols: u16,
    cursor_in_input: bool,
    output_generation: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut h = std::collections::hash_map::DefaultHasher::new();
    view.queue.len().hash(&mut h);
    for item in view.queue.items() {
        item.id.hash(&mut h);
        item.conditional.hash(&mut h);
        item.command.hash(&mut h);
    }
    view.queue.paused.hash(&mut h);
    (
        view.running,
        view.force_queue,
        view.passthrough_to_child,
        view.child_input_prompt,
    )
        .hash(&mut h);
    view.input_buffer.hash(&mut h);
    view.input_cursor.hash(&mut h);
    view.editing_index.hash(&mut h);
    view.status.hash(&mut h);
    view.status_kind.hash(&mut h);
    view.conditional.hash(&mut h);
    view.shell_input.hash(&mut h);
    view.activity.hash(&mut h);
    view.can_recover.hash(&mut h);
    (view.pending_quit, view.pending_clear, view.show_help).hash(&mut h);
    (
        height,
        term_rows,
        term_cols,
        cursor_in_input,
        output_generation,
    )
        .hash(&mut h);
    h.finish()
}

fn clear_reflowed_panel_after_shrink(
    out: &mut Terminal,
    panel_rows: u16,
    term_cols: u16,
    term_rows: u16,
    shell_cursor: (u16, u16),
) -> Result<(u16, u16)> {
    // Terminals reflow already-painted panel rows before delivering Resize,
    // so clear the measured new footprint at the bottom instead of using the
    // stale old coordinates (or erasing unrelated shell rows).
    let rows_to_clear = panel_rows.min(term_rows);
    let first_panel_row = term_rows.saturating_sub(rows_to_clear);
    for row in first_panel_row..term_rows {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
    }
    let safe_row = if first_panel_row == 0 {
        0
    } else {
        shell_cursor.1.min(first_panel_row - 1)
    };
    let safe_col = shell_cursor.0.min(term_cols.saturating_sub(1));
    out.queue(MoveTo(safe_col, safe_row))?;
    out.flush()?;
    Ok((safe_col, safe_row))
}

fn clear_reappeared_panel_rows(
    out: &mut Terminal,
    panel_rows: u16,
    old_rows: u16,
    new_rows: u16,
) -> Result<()> {
    // When a tiny viewport grows, tmux pulls the old viewport down and fills
    // the newly exposed band from scrollback. Reflowed panel overflow sits
    // immediately above the preserved old viewport, so erase only that band.
    let old_viewport_top = new_rows.saturating_sub(old_rows);
    let first_panel_row = old_viewport_top.saturating_sub(panel_rows);
    for row in first_panel_row..old_viewport_top {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
    }
    out.flush()?;
    Ok(())
}

pub(super) fn restore_terminal(state: &SharedRestoreState) -> Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, Show);
    let restore = state.lock().map(|s| *s).unwrap_or(TerminalRestoreState {
        layout: PanelLayout::Hidden,
        rows: crossterm::terminal::size()
            .map(|(_, rows)| rows.max(1))
            .unwrap_or(24),
        alt_screen: false,
    });
    let _ = restore_terminal_display(&mut stdout, restore);
    let _ = write!(stdout, "{MOUSE_TRACKING_OFF}{FOCUS_EVENTS_OFF}");
    execute!(stdout, DisableBracketedPaste)?;
    disable_raw_mode()?;
    Ok(())
}

fn restore_terminal_display(out: &mut impl Write, state: TerminalRestoreState) -> io::Result<()> {
    let rows = state.rows.max(1);
    if state.alt_screen {
        write!(out, "{ALT_SCREEN_EXIT}")?;
    }
    let height = match state.layout {
        PanelLayout::Reserved { height } => height,
        PanelLayout::Hidden => 0,
    };
    panel::release(out, height, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_terminal_display_releases_reserved_panel_rows() {
        let mut out = Vec::new();
        restore_terminal_display(
            &mut out,
            TerminalRestoreState {
                layout: PanelLayout::Reserved { height: 3 },
                rows: 10,
                alt_screen: false,
            },
        )
        .unwrap();

        let s = String::from_utf8_lossy(&out);
        assert_eq!(
            s.matches("\x1b[2K").count(),
            3,
            "reserved panel rows should be cleared"
        );
        assert!(s.contains("\x1b[1;10r"), "scroll region reset: {s:?}");
        assert!(
            s.contains("\x1b[10;1H"),
            "cursor restored to bottom row: {s:?}"
        );
    }

    #[test]
    fn restore_terminal_display_exits_alt_screen_when_child_left_it_active() {
        let mut out = Vec::new();
        restore_terminal_display(
            &mut out,
            TerminalRestoreState {
                layout: PanelLayout::Hidden,
                rows: 10,
                alt_screen: true,
            },
        )
        .unwrap();

        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?1049l"), "alt-screen exit missing: {s:?}");
        assert!(s.contains("\x1b[1;10r"), "scroll region reset: {s:?}");
    }
}
