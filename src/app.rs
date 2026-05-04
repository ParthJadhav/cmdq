//! Top-level application: PTY + queue + bottom-panel painter.
//!
//! ## Architecture (Path A: not a terminal emulator)
//!
//! cmdq is **not** a terminal emulator. The shell's PTY output streams
//! straight through to the user's real terminal — colors, hyperlinks,
//! images, OSC 52 clipboard, scrollback, mouse selection, all of it.
//! Anything the user's terminal supports just works.
//!
//! cmdq owns only the bottom few rows. We use the standard DEC scrolling
//! region (DECSTBM) to confine shell output to the rows above, then paint
//! our queue panel into the rows below using direct crossterm calls.
//!
//! When the inner program goes alt-screen (vim, htop, less, fzf, btop) we
//! detect that by sniffing `\x1b[?1049h` in the byte stream, tear down the
//! scrolling region, resize the PTY back to the full terminal height, and
//! get out of the program's way until it flips alt-screen back off.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    QueueableCommand,
    cursor::{MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
#[cfg(unix)]
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};

use crate::cursor_tracker::CursorTracker;
use crate::input::{InputAction, LineEditor};
use crate::mode_detect;
use crate::osc133::{self, Detector};
use crate::panel::{self, PanelState};
use crate::pty::ShellPty;
use crate::queue::{self, Queue};

pub struct AppConfig {
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellState {
    Unknown,
    AtPrompt,
    Running,
}

/// SIGINT to the running command should pause the queue: when the user hits
/// Ctrl-C they've changed their mind, so don't auto-fire what's queued.
/// We check both:
///  - whether we forwarded `0x03` to the PTY in the last few seconds, and
///  - whether the CommandEnd marker reports exit code 130 (= 128 + SIGINT).
const SIGINT_AUTO_PAUSE_WINDOW: Duration = Duration::from_secs(3);

/// Don't surface the queue panel until a command has been running for at
/// least this long — short-lived commands (`ls`, `cd`) shouldn't flash UI.
/// Keys typed within this window pass through to the shell as normal.
const QUEUE_PANEL_DELAY: Duration = Duration::from_millis(1500);

/// How long after the first Ctrl-D do we still treat a second Ctrl-D as
/// confirmation? After this, the prompt resets and the user has to press it
/// twice fresh.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// Status messages ("added: ls", "queue cleared", …) fade after this so the
/// header isn't a permanent log of the last action.
const STATUS_TTL: Duration = Duration::from_secs(2);

/// Running a restored queue from a different directory is high-risk enough to
/// require two close Ctrl-X presses, like a small "are you sure?" gesture.
const RESUME_CWD_CONFIRM_WINDOW: Duration = Duration::from_secs(4);

/// Two Esc presses within this window toggle raw-input passthrough. SSH-safe
/// alternative to Ctrl-\, which terminals/SSH often eat or remap to SIGQUIT.
const ESC_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// Maximum interval between repaints when the panel is visible. We also
/// repaint immediately when state changes, so this is just a backstop for
/// the status-message fade timer.
const PANEL_REPAINT_INTERVAL: Duration = Duration::from_millis(80);

/// Maximum time to hold a trailing escape-sequence fragment while waiting to
/// see if it becomes an alt-screen or bracketed-paste mode switch.
const MODE_PENDING_TIMEOUT: Duration = Duration::from_millis(600);

/// PTY reader idle timeout. We don't poll for keys longer than this so the
/// status fade and time-driven panel state transitions stay snappy.
const POLL_INTERVAL: Duration = Duration::from_millis(16);

/// How often an idle session checks whether another cmdq session edited the
/// shared queue file.
const QUEUE_SYNC_INTERVAL: Duration = Duration::from_millis(500);

/// How often a running cmdq refreshes its lightweight "I'm alive" lease.
const SESSION_LEASE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

const ETX: u8 = 0x03;
const FS: u8 = 0x1c;
const MAX_QUEUE_VISIBLE: u16 = 8;
const MIN_PANEL_COLS: u16 = 20;
const MIN_PANEL_ROWS: u16 = 5;

/// Whether cmdq is currently reserving panel rows on the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelLayout {
    /// Full terminal belongs to the shell. No scrolling region set.
    Hidden,
    /// Bottom `height` rows reserved for cmdq. Shell scrolling confined above.
    Reserved { height: u16 },
}

#[derive(Debug, Clone, Copy)]
struct TerminalRestoreState {
    layout: PanelLayout,
    rows: u16,
    cols: u16,
    alt_screen: bool,
}

struct AppState {
    queue: Queue,
    editor: LineEditor,
    shell_state: ShellState,
    /// User explicitly toggled passthrough.
    manual_passthrough: bool,
    /// Inner program is in alt-screen or mouse-capture mode → we let it own the screen.
    auto_passthrough: bool,
    child_alt_screen: bool,
    child_mouse_capture: bool,
    child_focus_events: bool,
    force_queue: bool,
    show_help: bool,
    terminal_allows_panel: bool,
    /// Last time we forwarded SIGINT (0x03) to the inner shell.
    last_sigint_at: Option<Instant>,
    /// When the current command started (set on CommandStart, cleared on End).
    command_started_at: Option<Instant>,
    /// Approximation of what the user has typed at the shell prompt. We only
    /// use this to decide whether an Enter key plausibly started a command
    /// before the shell integration's preexec marker arrives.
    prompt_buffer: String,
    prompt_cursor: usize,
    prompt_buffer_reliable: bool,
    /// True after the user submits an incomplete shell construct (`if true`,
    /// `cat <<EOF`, `echo one |`, etc.). While the shell is showing a
    /// continuation prompt, cmdq must not optimistically capture follow-up
    /// lines as queued commands; it waits for the real OSC 133 command-start
    /// marker instead.
    prompt_continuation_active: bool,
    /// Recent visible output from the child. If a non-alt-screen program
    /// prints an input prompt (`Password:`, `>>>`, `read -p`, etc.), cmdq
    /// leaves keystrokes with that program instead of silently queueing them.
    running_output_tail: OutputTail,
    /// Sticky form of `running_output_tail.looks_like_input_prompt()`. Once
    /// a child prompt is detected, stay in child-input mode while the user
    /// types the answer; echoed characters should not make us recapture the
    /// rest of the line.
    child_input_active: bool,
    /// Whether the child explicitly enabled bracketed paste (`CSI ? 2004 h`).
    /// cmdq keeps the outer terminal's paste reporting enabled for itself;
    /// this flag lets forwarded paste match what the child requested.
    child_bracketed_paste: bool,
    /// Timestamp of first Ctrl-D when queue was non-empty; second within
    /// QUIT_CONFIRM_WINDOW actually quits.
    pending_quit_at: Option<Instant>,
    /// Timestamp of the most recent bare-Esc press; a second Esc within
    /// ESC_DOUBLE_TAP_WINDOW toggles passthrough.
    last_esc_at: Option<Instant>,
    /// Has the user pressed Tab (chain toggle) at least once this session?
    chain_seen: bool,
    status: String,
    /// Time the current status was set; used to fade it after STATUS_TTL.
    status_set_at: Option<Instant>,
    /// Set when the queue mutates so the loop knows to persist it.
    queue_dirty: bool,
    /// Last queue-save error already surfaced to the user. This lets us keep
    /// retrying without repainting a fresh status every event-loop tick.
    queue_save_error: Option<String>,
    queue_path: Option<PathBuf>,
    queue_known_items: Vec<queue::QueueItem>,
    queue_known_paused: bool,
    queue_external_change_pending: bool,
    restored_queue_paused_locally: bool,
    session_cwd: Option<PathBuf>,
    shell_cwd: Option<PathBuf>,
    resume_cwd_warning: Option<String>,
    resume_cwd_confirmation_started_at: Option<Instant>,
}

impl AppState {
    fn effective_passthrough(&self) -> bool {
        self.manual_passthrough || self.auto_passthrough
    }

    fn command_long_running(&self) -> bool {
        self.command_started_at
            .map(|t| t.elapsed() >= QUEUE_PANEL_DELAY)
            .unwrap_or(false)
    }

    /// Whether the panel should be reserved on screen (regardless of who
    /// owns keystrokes). When the inner program is in alt-screen we always
    /// hide the panel — vim/htop expect the whole screen.
    ///
    /// Show the panel when:
    ///   * a long-running command is in flight, OR
    ///   * force_queue is on, OR
    ///   * help is open, OR
    ///   * the queue is paused with items pending (so the user can see *why*
    ///     nothing is auto-dispatching after a ^C and discover ^X to resume).
    ///
    /// Note: we deliberately do NOT keep the panel visible just because a
    /// status message is fresh. Doing so would steal keystrokes from the
    /// shell for ~2 s after every status update — a worse footgun than the
    /// momentary status-not-seen issue it would fix.
    fn panel_should_be_visible(&self) -> bool {
        if self.auto_passthrough {
            return false;
        }
        if !self.terminal_allows_panel {
            return false;
        }
        self.command_long_running()
            || self.force_queue
            || self.show_help
            || (self.queue.paused && !self.queue.is_empty())
    }

    /// Whether cmdq's editor owns keystrokes (rather than forwarding to PTY).
    /// The panel can be visible while keystrokes still go to the shell — that's
    /// what `manual_passthrough` does (raw input mode).
    fn editor_owns_input(&self) -> bool {
        self.panel_should_be_visible()
            && !self.manual_passthrough
            && !self.child_input_prompt_active()
    }

    fn child_input_prompt_active(&self) -> bool {
        matches!(self.shell_state, ShellState::Running)
            && !self.force_queue
            && !self.show_help
            && !self.effective_passthrough()
            && (self.child_input_active || self.running_output_tail.looks_like_input_prompt())
    }

    fn pending_quit_active(&self) -> bool {
        self.pending_quit_at
            .map(|t| t.elapsed() <= QUIT_CONFIRM_WINDOW)
            .unwrap_or(false)
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_set_at = Some(Instant::now());
    }

    fn tick_status(&mut self) {
        if let Some(t) = self.status_set_at
            && t.elapsed() >= STATUS_TTL
        {
            if self.queue_external_change_pending {
                self.status = deferred_queue_change_status().to_string();
                self.status_set_at = Some(Instant::now());
            } else {
                self.status.clear();
                self.status_set_at = None;
            }
        }
    }

    fn toggle_force_queue(&mut self) {
        if !self.terminal_allows_panel && !self.force_queue {
            self.set_status("terminal too small for queue panel");
            return;
        }
        self.force_queue = !self.force_queue;
        let msg = if self.force_queue {
            "force-queue ON (Ctrl-Q to disable)"
        } else {
            "force-queue OFF"
        };
        self.set_status(msg);
    }

    fn activate_queue_for_running_command(&mut self) {
        if matches!(self.shell_state, ShellState::Running) {
            self.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY);
        }
    }

    fn toggle_manual_passthrough(&mut self) {
        self.manual_passthrough = !self.manual_passthrough;
        let msg = if self.manual_passthrough {
            "raw input: keys go to the running app (Esc Esc / Ctrl-\\ to exit)"
        } else {
            "raw input off"
        };
        self.set_status(msg);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailParser {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Debug, Clone)]
struct OutputTail {
    text: String,
    parser: TailParser,
}

impl Default for OutputTail {
    fn default() -> Self {
        Self {
            text: String::new(),
            parser: TailParser::Ground,
        }
    }
}

impl OutputTail {
    fn clear(&mut self) {
        self.text.clear();
        self.parser = TailParser::Ground;
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.parser {
                TailParser::Ground => match b {
                    0x1b => self.parser = TailParser::Esc,
                    b'\r' | b'\n' => self.text.clear(),
                    0x08 | 0x7f => {
                        self.text.pop();
                    }
                    b'\t' => self.push(' '),
                    0x20..=0x7e => self.push(b as char),
                    _ => {}
                },
                TailParser::Esc => match b {
                    b'[' => self.parser = TailParser::Csi,
                    b']' => self.parser = TailParser::Osc,
                    _ => self.parser = TailParser::Ground,
                },
                TailParser::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.parser = TailParser::Ground;
                    }
                }
                TailParser::Osc => match b {
                    0x07 => self.parser = TailParser::Ground,
                    0x1b => self.parser = TailParser::OscEsc,
                    _ => {}
                },
                TailParser::OscEsc => {
                    self.parser = TailParser::Ground;
                }
            }
        }
    }

    fn push(&mut self, c: char) {
        self.text.push(c);
        const MAX_TAIL_CHARS: usize = 200;
        let extra = self.text.chars().count().saturating_sub(MAX_TAIL_CHARS);
        if extra > 0 {
            let drain_to = self
                .text
                .char_indices()
                .nth(extra)
                .map(|(i, _)| i)
                .unwrap_or(self.text.len());
            self.text.drain(..drain_to);
        }
    }

    fn looks_like_input_prompt(&self) -> bool {
        let trimmed = self.text.trim_end();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("password")
            || lower.contains("passphrase")
            || lower.contains("(yes/no)")
            || lower.contains("[y/n]")
            || lower.contains("press enter")
            || lower.contains("press return")
            || lower.contains("press any key")
            || lower.contains("hit enter")
            || lower.contains("hit return")
        {
            return true;
        }
        if looks_like_status_tail(&lower) {
            return false;
        }
        if trimmed.ends_with('?') {
            return trimmed.chars().count() <= 80;
        }
        if trimmed.ends_with(':') {
            return contains_prompt_word(&lower)
                || (trimmed.chars().count() <= 24
                    && lower.split_whitespace().count() <= 3
                    && lower.chars().any(|c| c.is_ascii_alphabetic()));
        }
        looks_like_repl_prompt(trimmed)
    }

    fn looks_like_any_key_prompt(&self) -> bool {
        let lower = self.text.trim_end().to_ascii_lowercase();
        lower.contains("press any key") || lower.contains("hit any key")
    }
}

fn contains_prompt_word(lower: &str) -> bool {
    [
        "answer",
        "choice",
        "code",
        "confirm",
        "continue",
        "directory",
        "email",
        "enter",
        "file",
        "host",
        "input",
        "name",
        "otp",
        "passcode",
        "path",
        "port",
        "select",
        "token",
        "user",
        "username",
        "value",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn looks_like_status_tail(lower: &str) -> bool {
    let first_word = lower
        .trim_start()
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '.' | '-' | '>'))
        .find(|part| !part.is_empty())
        .unwrap_or("");
    matches!(
        first_word,
        "building"
            | "checking"
            | "compiling"
            | "connected"
            | "connecting"
            | "debug"
            | "downloaded"
            | "downloading"
            | "error"
            | "extracting"
            | "failed"
            | "fetching"
            | "finished"
            | "info"
            | "installed"
            | "installing"
            | "loaded"
            | "loading"
            | "progress"
            | "running"
            | "saved"
            | "saving"
            | "started"
            | "starting"
            | "stopped"
            | "stopping"
            | "trace"
            | "uploading"
            | "warning"
    )
}

fn looks_like_repl_prompt(trimmed: &str) -> bool {
    if matches!(trimmed, ">" | ">>>" | "..." | "=>" | "->") {
        return true;
    }
    let len = trimmed.chars().count();
    if len > 40 || trimmed.split_whitespace().count() > 1 {
        return false;
    }
    trimmed.ends_with('>') || trimmed.ends_with("=>") || trimmed.ends_with("->")
}

pub fn run(cfg: AppConfig) -> Result<()> {
    // We export CMDQ_ACTIVE=1 to the inner shell so its rc-file integration
    // knows when it's running under us. If we *already* see it set, that means
    // this cmdq was launched from inside another cmdq's shell — almost always
    // because the user put `cmdq` (no `exec`, no guard) in their rc file.
    // Refuse to recurse: it would otherwise spawn nested PTYs forever.
    if std::env::var_os("CMDQ_ACTIVE").is_some() {
        eprintln!(
            "cmdq: refusing to start — CMDQ_ACTIVE is already set, which means \n\
             this cmdq was launched from inside another cmdq session.\n\
             \n\
             If you put `cmdq` in your shell rc file to auto-start it, guard it:\n\
             \n\
             \x20   [ -z \"$CMDQ_ACTIVE\" ] && exec cmdq\n\
             \n\
             (the `exec` replaces your shell, and the guard prevents recursion.)"
        );
        return Ok(());
    }

    let session_cwd = std::env::current_dir().ok();
    let queue_path = queue::try_default_path()?;
    let (mut queue, queue_load_warning) = Queue::load_or_default_with_warning(&queue_path);
    if let Some(warning) = &queue_load_warning {
        eprintln!("cmdq: {warning}");
    }
    let active_peer_count = crate::session_lease::active_peer_count(&queue_path).unwrap_or(0);
    let queue_known_paused = queue.paused;
    let (startup_status, queue_dirty_on_startup, resume_cwd_warning, restored_queue_paused_locally) =
        prepare_queue_for_startup(&mut queue, session_cwd.as_deref(), active_peer_count);
    let startup_status = startup_status.or(queue_load_warning);
    let queue_known_items = queue.item_snapshot();
    let mut session_lease =
        crate::session_lease::SessionLease::start(&queue_path, session_cwd.as_deref()).ok();

    let (cols, rows) = {
        let (c, r) = crossterm::terminal::size().unwrap_or((80, 24));
        (c.max(1), r.max(1))
    };
    let (mut pty, io_pair) = ShellPty::spawn(cfg.shell.as_deref(), cols, rows)?;

    let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();
    {
        let mut reader: Box<dyn Read + Send> = io_pair.reader;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if pty_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
    }

    let mut writer: Box<dyn Write + Send> = io_pair.writer;

    let mut osc = Detector::new();
    let mut mode = mode_detect::Detector::new();
    let cleanup_state = Arc::new(Mutex::new(TerminalRestoreState {
        layout: PanelLayout::Hidden,
        rows,
        cols,
        alt_screen: false,
    }));

    enable_raw_mode().context("enable_raw_mode")?;
    let cleanup_for_guard = cleanup_state.clone();
    let _terminal_guard = CleanupGuard::new(move || {
        let _ = restore_terminal(&cleanup_for_guard);
    });
    install_signal_cleanup(
        cleanup_state.clone(),
        session_lease
            .as_ref()
            .map(|lease| lease.path().to_path_buf()),
        pty.session_dirs().to_vec(),
    )
    .context("install signal cleanup")?;
    let mut stdout = io::stdout();
    // Bracketed paste lets us distinguish typed text from pasted text.
    // Keyboard enhancement flags give us reliable Esc / modifier reporting.
    // We do NOT enter alt-screen and we do NOT enable mouse capture: that
    // would cost the user their terminal's native selection, scrollback,
    // hyperlinks, OSC 52 clipboard, and image protocols.
    let _ = execute!(stdout, EnableBracketedPaste);
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );

    let original_hook = std::panic::take_hook();
    let cleanup_for_panic = cleanup_state.clone();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(&cleanup_for_panic);
        original_hook(info);
    }));

    let has_startup_status = startup_status.is_some();
    let mut state = AppState {
        queue,
        editor: LineEditor::new(),
        shell_state: ShellState::Unknown,
        manual_passthrough: false,
        auto_passthrough: false,
        child_alt_screen: false,
        child_mouse_capture: false,
        child_focus_events: false,
        force_queue: false,
        show_help: false,
        terminal_allows_panel: terminal_allows_panel(cols, rows),
        last_sigint_at: None,
        command_started_at: None,
        prompt_buffer: String::new(),
        prompt_cursor: 0,
        prompt_buffer_reliable: true,
        prompt_continuation_active: false,
        running_output_tail: OutputTail::default(),
        child_input_active: false,
        child_bracketed_paste: false,
        pending_quit_at: None,
        last_esc_at: None,
        chain_seen: false,
        status: startup_status.unwrap_or_default(),
        status_set_at: if has_startup_status {
            Some(Instant::now())
        } else {
            None
        },
        queue_dirty: queue_dirty_on_startup,
        queue_save_error: None,
        queue_path: Some(queue_path.clone()),
        queue_known_items,
        queue_known_paused,
        queue_external_change_pending: false,
        restored_queue_paused_locally,
        shell_cwd: session_cwd.clone(),
        session_cwd,
        resume_cwd_warning,
        resume_cwd_confirmation_started_at: None,
    };

    let mut layout = PanelLayout::Hidden;
    let (mut term_cols, mut term_rows) = (cols, rows);
    let mut shell_cursor = CursorTracker::new(term_cols, term_rows);
    let mut mode_pending = Vec::new();
    let mut mode_pending_since: Option<Instant> = None;
    let mut last_paint = Instant::now() - PANEL_REPAINT_INTERVAL;
    let mut last_queue_sync = Instant::now();
    let mut last_session_lease_refresh = Instant::now();

    let result = loop {
        // 1. Drain any PTY output, pass it straight through to the user's
        //    terminal, and feed the byte sniffers.
        let mut had_bytes = false;
        loop {
            match pty_rx.try_recv() {
                Ok(bytes) => {
                    had_bytes = true;
                    let mut bytes = if mode_pending.is_empty() {
                        bytes
                    } else {
                        let mut combined = Vec::with_capacity(mode_pending.len() + bytes.len());
                        combined.extend_from_slice(&mode_pending);
                        combined.extend_from_slice(&bytes);
                        mode_pending.clear();
                        combined
                    };
                    let osc_events = osc.feed_with_offsets(&bytes);
                    let mode_events = mode.feed_with_offsets(&bytes);
                    let pending_len = mode.pending_len(bytes.len());
                    let process_len = bytes.len().saturating_sub(pending_len);
                    if pending_len > 0 {
                        mode_pending.extend_from_slice(&bytes[process_len..]);
                        mode_pending_since = Some(Instant::now());
                        bytes.truncate(process_len);
                    } else {
                        mode_pending_since = None;
                    }
                    if bytes.is_empty() {
                        continue;
                    }
                    restore_shell_cursor_if_reserved(&mut stdout, layout, &shell_cursor)?;
                    let mut write_pos = 0usize;
                    let mut osc_event_idx = 0usize;
                    for ev in &mode_events {
                        while let Some(osc_ev) = osc_events.get(osc_event_idx)
                            && osc_ev.end <= ev.start
                        {
                            handle_osc_event(
                                osc_ev.event.clone(),
                                &mut state,
                                &mut stdout,
                                &mut layout,
                                term_rows,
                                term_cols,
                                &mut pty,
                                &cleanup_state,
                                &mut shell_cursor,
                                &mut last_paint,
                                &mut writer,
                            )?;
                            osc_event_idx += 1;
                        }
                        let start = ev.start.min(bytes.len()).max(write_pos);
                        let end = ev.end.min(bytes.len()).max(start);
                        match ev.kind {
                            mode_detect::Event::AltScreenEnter => {
                                let pre = &bytes[write_pos..start];
                                let _ = stdout.write_all(pre);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(pre);
                                }
                                state.child_alt_screen = true;
                                refresh_auto_passthrough_for_child_modes(
                                    &mut state,
                                    &mut stdout,
                                    &mut layout,
                                    term_rows,
                                    term_cols,
                                    &mut pty,
                                    &cleanup_state,
                                    &mut shell_cursor,
                                    &mut last_paint,
                                    Some("alt-screen detected — keys go to the running app"),
                                )?;
                                let _ = stdout.write_all(&bytes[start..end]);
                            }
                            mode_detect::Event::AltScreenExit => {
                                let _ = stdout.write_all(&bytes[write_pos..end]);
                                state.child_alt_screen = false;
                                refresh_auto_passthrough_for_child_modes(
                                    &mut state,
                                    &mut stdout,
                                    &mut layout,
                                    term_rows,
                                    term_cols,
                                    &mut pty,
                                    &cleanup_state,
                                    &mut shell_cursor,
                                    &mut last_paint,
                                    Some("alt-screen exit — queue mode restored"),
                                )?;
                            }
                            mode_detect::Event::BracketedPasteEnable => {
                                let event_bytes = &bytes[write_pos..end];
                                let _ = stdout.write_all(event_bytes);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(event_bytes);
                                }
                                state.child_bracketed_paste = true;
                            }
                            mode_detect::Event::BracketedPasteDisable => {
                                let event_bytes = &bytes[write_pos..end];
                                let _ = stdout.write_all(event_bytes);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(event_bytes);
                                }
                                state.child_bracketed_paste = false;
                                // The child disabled its paste mode, but cmdq
                                // still needs the outer terminal to report
                                // paste events so it can route them correctly.
                                let _ = execute!(stdout, EnableBracketedPaste);
                            }
                            mode_detect::Event::MouseCaptureEnable => {
                                let pre = &bytes[write_pos..start];
                                let _ = stdout.write_all(pre);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(pre);
                                }
                                state.child_mouse_capture = true;
                                refresh_auto_passthrough_for_child_modes(
                                    &mut state,
                                    &mut stdout,
                                    &mut layout,
                                    term_rows,
                                    term_cols,
                                    &mut pty,
                                    &cleanup_state,
                                    &mut shell_cursor,
                                    &mut last_paint,
                                    Some(
                                        "mouse tracking detected — keys and clicks go to the running app",
                                    ),
                                )?;
                                let _ = stdout.write_all(&bytes[start..end]);
                            }
                            mode_detect::Event::MouseCaptureDisable => {
                                let event_bytes = &bytes[write_pos..end];
                                let _ = stdout.write_all(event_bytes);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(event_bytes);
                                }
                                state.child_mouse_capture = false;
                                refresh_auto_passthrough_for_child_modes(
                                    &mut state,
                                    &mut stdout,
                                    &mut layout,
                                    term_rows,
                                    term_cols,
                                    &mut pty,
                                    &cleanup_state,
                                    &mut shell_cursor,
                                    &mut last_paint,
                                    Some("mouse tracking off — queue mode restored"),
                                )?;
                            }
                            mode_detect::Event::FocusEventsEnable => {
                                let event_bytes = &bytes[write_pos..end];
                                let _ = stdout.write_all(event_bytes);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(event_bytes);
                                }
                                state.child_focus_events = true;
                            }
                            mode_detect::Event::FocusEventsDisable => {
                                let event_bytes = &bytes[write_pos..end];
                                let _ = stdout.write_all(event_bytes);
                                if !state.auto_passthrough {
                                    shell_cursor.feed(event_bytes);
                                }
                                state.child_focus_events = false;
                            }
                        }
                        write_pos = end;
                    }
                    let tail = &bytes[write_pos..];
                    let _ = stdout.write_all(tail);
                    if !state.auto_passthrough {
                        shell_cursor.feed(tail);
                    }

                    // OSC 133 markers drive cmdq's command lifecycle. Apply
                    // them in byte order relative to mode flips in the same
                    // PTY read, so `CommandStart` followed by child
                    // bracketed-paste enable leaves paste forwarding enabled.
                    while let Some(osc_ev) = osc_events.get(osc_event_idx) {
                        handle_osc_event(
                            osc_ev.event.clone(),
                            &mut state,
                            &mut stdout,
                            &mut layout,
                            term_rows,
                            term_cols,
                            &mut pty,
                            &cleanup_state,
                            &mut shell_cursor,
                            &mut last_paint,
                            &mut writer,
                        )?;
                        osc_event_idx += 1;
                    }
                    update_child_input_detection(&mut state, &bytes);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if had_bytes {
            let _ = stdout.flush();
        }

        if !mode_pending.is_empty()
            && mode_pending_since
                .map(|since| since.elapsed() >= MODE_PENDING_TIMEOUT)
                .unwrap_or(false)
        {
            flush_mode_pending(
                &mut stdout,
                layout,
                &mut shell_cursor,
                &mut state,
                &mut mode_pending,
            )?;
            mode.reset();
            mode_pending_since = None;
            let _ = stdout.flush();
        }

        state.tick_status();

        if let Ok(Some(_)) = pty.try_wait() {
            if !mode_pending.is_empty() {
                flush_mode_pending(
                    &mut stdout,
                    layout,
                    &mut shell_cursor,
                    &mut state,
                    &mut mode_pending,
                )?;
                mode.reset();
                let _ = stdout.flush();
            }
            // Tear down our reservation before exiting so the user's
            // terminal returns to a clean state.
            let _ = transition_layout_recorded(
                &mut stdout,
                &mut layout,
                PanelLayout::Hidden,
                term_rows,
                term_cols,
                &mut pty,
                &cleanup_state,
            );
            sync_cursor_tracker_for_layout(&mut shell_cursor, layout, term_cols, term_rows);
            break Ok(());
        }

        // 2. Update panel layout to match desired state.
        let desired = desired_layout(&state, term_rows);
        if desired != layout {
            transition_layout_recorded(
                &mut stdout,
                &mut layout,
                desired,
                term_rows,
                term_cols,
                &mut pty,
                &cleanup_state,
            )?;
            sync_cursor_tracker_for_layout(&mut shell_cursor, layout, term_cols, term_rows);
            last_paint = Instant::now() - PANEL_REPAINT_INTERVAL;
        }

        // 3. Read one input event (with a short timeout so the loop ticks).
        let timeout = if had_bytes {
            Duration::from_millis(0)
        } else {
            POLL_INTERVAL
        };
        if crossterm::event::poll(timeout).unwrap_or(false) {
            let event = crossterm::event::read().context("event::read")?;
            match event {
                CtEvent::Resize(cw, rh) => {
                    let old_cols = term_cols;
                    let old_rows = term_rows;
                    let old_reserved = matches!(layout, PanelLayout::Reserved { .. });
                    let new_cols = cw.max(1);
                    let new_rows = rh.max(1);
                    // The desired panel height may change with the new size;
                    // release first, then recompute and apply. This keeps the
                    // outer terminal's scroll region sane even if the resized
                    // state no longer wants a panel.
                    transition_layout_recorded(
                        &mut stdout,
                        &mut layout,
                        PanelLayout::Hidden,
                        old_rows,
                        old_cols,
                        &mut pty,
                        &cleanup_state,
                    )?;
                    term_cols = new_cols;
                    term_rows = new_rows;
                    record_terminal_restore_state(&cleanup_state, layout, term_rows, term_cols);
                    if old_reserved {
                        panel::release(&mut stdout, 0, term_rows, term_cols)?;
                        record_terminal_restore_state(&cleanup_state, layout, term_rows, term_cols);
                    }
                    state.terminal_allows_panel = terminal_allows_panel(term_cols, term_rows);
                    let desired = desired_layout(&state, term_rows);
                    transition_layout_recorded(
                        &mut stdout,
                        &mut layout,
                        desired,
                        term_rows,
                        term_cols,
                        &mut pty,
                        &cleanup_state,
                    )?;
                    resize_pty_for_layout(&mut pty, layout, term_cols, term_rows);
                    sync_cursor_tracker_for_layout(&mut shell_cursor, layout, term_cols, term_rows);
                    last_paint = Instant::now() - PANEL_REPAINT_INTERVAL;
                }
                CtEvent::Key(key) => {
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        continue;
                    }
                    match handle_key(key, &mut state, &mut writer) {
                        KeyOutcome::Quit => {
                            transition_layout_recorded(
                                &mut stdout,
                                &mut layout,
                                PanelLayout::Hidden,
                                term_rows,
                                term_cols,
                                &mut pty,
                                &cleanup_state,
                            )?;
                            sync_cursor_tracker_for_layout(
                                &mut shell_cursor,
                                layout,
                                term_cols,
                                term_rows,
                            );
                            break Ok(());
                        }
                        KeyOutcome::Continue => {}
                    }
                }
                CtEvent::Paste(text) => handle_paste(text, &mut state, &mut writer),
                CtEvent::Mouse(mouse) => {
                    if state.child_mouse_capture || state.auto_passthrough {
                        let bytes = encode_mouse_event_for_pty(mouse);
                        if !bytes.is_empty() {
                            let _ = writer.write_all(&bytes);
                            let _ = writer.flush();
                        }
                    }
                }
                CtEvent::FocusGained => {
                    if state.child_focus_events {
                        let _ = writer.write_all(b"\x1b[I");
                        let _ = writer.flush();
                    }
                }
                CtEvent::FocusLost => {
                    if state.child_focus_events {
                        let _ = writer.write_all(b"\x1b[O");
                        let _ = writer.flush();
                    }
                }
            }
        }

        save_queue_if_dirty(&mut state, &queue_path);
        sync_queue_from_disk_if_due(&mut state, &queue_path, &mut last_queue_sync);
        refresh_session_lease_if_due(&mut session_lease, &mut last_session_lease_refresh);

        // 4. Repaint the panel if it should be visible.
        if let PanelLayout::Reserved { height } = layout
            && last_paint.elapsed() >= PANEL_REPAINT_INTERVAL
        {
            let view = PanelState {
                queue: &state.queue,
                running: matches!(state.shell_state, ShellState::Running),
                force_queue: state.force_queue,
                passthrough_to_child: state.effective_passthrough()
                    || state.child_input_prompt_active(),
                child_input_prompt: state.child_input_prompt_active(),
                input_buffer: &state.editor.buffer,
                input_cursor: state.editor.cursor,
                editing_index: state.editor.editing_index,
                status: &state.status,
                pending_quit: state.pending_quit_active(),
                show_help: state.show_help,
                max_queue_visible: MAX_QUEUE_VISIBLE,
            };
            panel::paint(
                &mut stdout,
                &view,
                height,
                term_rows,
                term_cols,
                state.editor_owns_input(),
                shell_cursor.position(),
            )?;
            last_paint = Instant::now();
        }
    };

    save_queue_if_dirty(&mut state, &queue_path);
    let _ = pty.kill();
    result
}

struct CleanupGuard<F: FnOnce()> {
    cleanup: Option<F>,
}

impl<F: FnOnce()> CleanupGuard<F> {
    fn new(cleanup: F) -> Self {
        Self {
            cleanup: Some(cleanup),
        }
    }
}

impl<F: FnOnce()> Drop for CleanupGuard<F> {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[cfg(unix)]
fn install_signal_cleanup(
    state: Arc<Mutex<TerminalRestoreState>>,
    session_lease_path: Option<PathBuf>,
    session_dirs: Vec<PathBuf>,
) -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])?;
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _ = restore_terminal(&state);
            if let Some(path) = session_lease_path.as_deref() {
                let _ = std::fs::remove_file(path);
            }
            for dir in &session_dirs {
                let _ = std::fs::remove_dir_all(dir);
            }
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn install_signal_cleanup(
    _state: Arc<Mutex<TerminalRestoreState>>,
    _session_lease_path: Option<PathBuf>,
    _session_dirs: Vec<PathBuf>,
) -> Result<()> {
    Ok(())
}

fn terminal_allows_panel(cols: u16, rows: u16) -> bool {
    cols >= MIN_PANEL_COLS && rows >= MIN_PANEL_ROWS
}

fn prepare_queue_for_startup(
    queue: &mut Queue,
    session_cwd: Option<&Path>,
    active_peer_count: usize,
) -> (Option<String>, bool, Option<String>, bool) {
    let restored = queue.len();
    if restored == 0 {
        return (None, false, None, false);
    }

    let local_pause = !queue.paused;
    queue.paused = true;
    let noun = if restored == 1 { "command" } else { "commands" };
    let peer_note = active_peer_note(active_peer_count);
    if let Some(current) = session_cwd
        && let Some(warning) = resume_cwd_warning_for_queue(queue, current)
    {
        let origins = queue.mismatched_origins(current);
        return (
            Some(format!(
                "restored {restored} queued {noun} from {}{peer_note} — Ctrl-X to confirm, Ctrl-K to clear",
                compact_origin_summary(&origins)
            )),
            false,
            Some(warning),
            local_pause,
        );
    }

    (
        Some(format!(
            "restored {restored} queued {noun}{peer_note} — Ctrl-X to resume, Ctrl-K to clear"
        )),
        false,
        None,
        local_pause,
    )
}

fn active_peer_note(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " (another cmdq session is active)".to_string(),
        n => format!(" ({n} other cmdq sessions are active)"),
    }
}

fn compact_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    const MAX: usize = 48;
    if s.chars().count() <= MAX {
        return s.into_owned();
    }
    let tail: String = s
        .chars()
        .rev()
        .take(MAX.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

fn compact_origin_summary(origins: &[PathBuf]) -> String {
    match origins {
        [] => "unknown cwd".to_string(),
        [origin] => compact_path(origin),
        [origin, rest @ ..] => format!("{} and {} other dirs", compact_path(origin), rest.len()),
    }
}

fn current_shell_cwd(state: &AppState) -> Option<&PathBuf> {
    state.shell_cwd.as_ref().or(state.session_cwd.as_ref())
}

fn set_queue_origin_to_session(state: &mut AppState) {
    if let Some(cwd) = current_shell_cwd(state) {
        state.queue.set_origin_cwd(cwd.clone());
    }
}

fn resume_cwd_warning_for_current_shell(state: &AppState) -> Option<String> {
    resume_cwd_warning_for_queue(&state.queue, current_shell_cwd(state)?)
}

fn resume_cwd_warning_for_queue(queue: &Queue, current: &Path) -> Option<String> {
    cwd_warning_for_queue(queue, current, "press Ctrl-X again to run here")
}

fn dispatch_cwd_warning_for_item(
    queue: &Queue,
    item: &queue::QueueItem,
    current: &Path,
) -> Option<String> {
    let origin = item.origin_cwd.as_deref().or(queue.origin_cwd())?;
    if origin == current {
        return None;
    }
    Some(format!(
        "queue was saved in {}; Ctrl-X to run here ({})",
        compact_path(origin),
        compact_path(current)
    ))
}

fn cwd_warning_for_queue(queue: &Queue, current: &Path, action: &str) -> Option<String> {
    let origins = queue.mismatched_origins(current);
    match origins.as_slice() {
        [] => None,
        [origin] => Some(format!(
            "queue was saved in {}; {action} ({})",
            compact_path(origin),
            compact_path(current)
        )),
        _ => Some(format!(
            "queue includes commands from {}; {action} ({})",
            compact_origin_summary(&origins),
            compact_path(current)
        )),
    }
}

fn save_queue_if_dirty(state: &mut AppState, queue_path: &std::path::Path) {
    if !state.queue_dirty {
        return;
    }
    match state.queue.save_preserving_unseen(
        queue_path,
        &mut state.queue_known_items,
        &mut state.queue_known_paused,
    ) {
        Ok(merge) => {
            state.queue_dirty = false;
            state.queue_external_change_pending = false;
            state.queue_save_error = None;
            apply_queue_merge_status(state, merge, None);
        }
        Err(e) => {
            let msg = e.to_string();
            if state.queue_save_error.as_deref() != Some(msg.as_str()) {
                state.set_status(format!("queue save failed: {msg}"));
                state.queue_save_error = Some(msg);
            }
        }
    }
}

fn sync_queue_from_disk_if_due(
    state: &mut AppState,
    queue_path: &std::path::Path,
    last_sync: &mut Instant,
) {
    if last_sync.elapsed() < QUEUE_SYNC_INTERVAL {
        return;
    }
    if state.queue_dirty {
        return;
    }
    if state.editor.editing_index.is_some() || !state.editor.buffer.is_empty() {
        *last_sync = Instant::now();
        notify_deferred_queue_change_if_needed(state, queue_path);
        return;
    }
    *last_sync = Instant::now();

    let before_items = state.queue.item_snapshot();
    let before_paused = state.queue.paused;
    let keep_local_startup_pause = state.restored_queue_paused_locally && state.queue.paused;
    let known_paused_before_sync = state.queue_known_paused;
    match state.queue.sync_from_disk(
        queue_path,
        &mut state.queue_known_items,
        &mut state.queue_known_paused,
    ) {
        Ok(merge) => {
            state.queue_external_change_pending = false;
            state.queue_save_error = None;
            let fallback = if merge == queue::SaveMerge::default()
                && (before_items != state.queue.item_snapshot()
                    || before_paused != state.queue.paused)
            {
                if !before_items.is_empty() && state.queue.is_empty() {
                    state.resume_cwd_warning = None;
                    state.resume_cwd_confirmation_started_at = None;
                    Some("queue cleared by another session")
                } else {
                    Some("queue updated by another session")
                }
            } else {
                None
            };
            apply_queue_merge_status(state, merge, fallback);
            if keep_local_startup_pause && !state.queue.is_empty() {
                state.queue.paused = true;
                state.queue_known_paused = known_paused_before_sync;
            } else if state.queue.is_empty() || !state.queue.paused {
                state.restored_queue_paused_locally = false;
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if state.queue_save_error.as_deref() != Some(msg.as_str()) {
                state.set_status(format!("queue sync failed: {msg}"));
                state.queue_save_error = Some(msg);
            }
        }
    }
}

fn notify_deferred_queue_change_if_needed(state: &mut AppState, queue_path: &std::path::Path) {
    match Queue::disk_differs_from_known(
        queue_path,
        &state.queue_known_items,
        state.queue_known_paused,
    ) {
        Ok(true) => {
            state.queue_save_error = None;
            if !state.queue_external_change_pending {
                state.queue_external_change_pending = true;
                state.set_status(deferred_queue_change_status());
            }
        }
        Ok(false) => {
            if state.queue_external_change_pending && state.status == deferred_queue_change_status()
            {
                state.status.clear();
                state.status_set_at = None;
            }
            state.queue_external_change_pending = false;
            state.queue_save_error = None;
        }
        Err(e) => {
            let msg = e.to_string();
            if state.queue_save_error.as_deref() != Some(msg.as_str()) {
                state.set_status(format!("queue sync waiting for edit: {msg}"));
                state.queue_save_error = Some(msg);
            }
        }
    }
}

fn deferred_queue_change_status() -> &'static str {
    "queue changed in another session; finish or cancel edit to merge"
}

fn refresh_session_lease_if_due(
    lease: &mut Option<crate::session_lease::SessionLease>,
    last_refresh: &mut Instant,
) {
    if last_refresh.elapsed() < SESSION_LEASE_REFRESH_INTERVAL {
        return;
    }
    *last_refresh = Instant::now();
    if let Some(lease) = lease {
        let _ = lease.refresh();
    }
}

fn apply_queue_merge_status(
    state: &mut AppState,
    merge: queue::SaveMerge,
    fallback_status: Option<&'static str>,
) {
    if let Some(warning) = merge.warning {
        state.set_status(warning);
    } else if merge.external_pause {
        state.set_status("queue paused by another session");
    } else if merge.external_resume {
        state.resume_cwd_warning = resume_cwd_warning_for_current_shell(state);
        state.resume_cwd_confirmation_started_at = None;
        state.set_status("queue resumed by another session");
    } else if merge.item_conflicts > 0 {
        let noun = if merge.item_conflicts == 1 {
            "item"
        } else {
            "items"
        };
        state.set_status(format!(
            "queue changed in another session; kept local edits for {} {noun}",
            merge.item_conflicts
        ));
    } else if merge.unseen_items > 0 {
        let noun = if merge.unseen_items == 1 {
            "item"
        } else {
            "items"
        };
        if state.queue.paused {
            state.set_status(format!(
                "merged {} queued {noun} from another session",
                merge.unseen_items
            ));
        } else {
            state.queue.paused = true;
            state.queue_dirty = true;
            state.resume_cwd_warning = resume_cwd_warning_for_current_shell(state);
            state.resume_cwd_confirmation_started_at = None;
            state.set_status(format!(
                "merged {} queued {noun} from another session; queue paused",
                merge.unseen_items
            ));
        }
    } else if let Some(status) = fallback_status {
        state.set_status(status);
    }
}

fn desired_layout(state: &AppState, term_rows: u16) -> PanelLayout {
    if !state.panel_should_be_visible() {
        return PanelLayout::Hidden;
    }
    let view = PanelState {
        queue: &state.queue,
        running: matches!(state.shell_state, ShellState::Running),
        force_queue: state.force_queue,
        passthrough_to_child: state.effective_passthrough() || state.child_input_prompt_active(),
        child_input_prompt: state.child_input_prompt_active(),
        input_buffer: &state.editor.buffer,
        input_cursor: state.editor.cursor,
        editing_index: state.editor.editing_index,
        status: &state.status,
        pending_quit: state.pending_quit_active(),
        show_help: state.show_help,
        max_queue_visible: MAX_QUEUE_VISIBLE,
    };
    let h = panel::panel_height(&view, term_rows).min(term_rows.saturating_sub(2));
    if h == 0 {
        PanelLayout::Hidden
    } else {
        PanelLayout::Reserved { height: h }
    }
}

fn record_terminal_restore_state(
    state: &Arc<Mutex<TerminalRestoreState>>,
    layout: PanelLayout,
    rows: u16,
    cols: u16,
) {
    if let Ok(mut restore) = state.lock() {
        restore.layout = layout;
        restore.rows = rows;
        restore.cols = cols;
    }
}

fn record_alt_screen_state(state: &Arc<Mutex<TerminalRestoreState>>, alt_screen: bool) {
    if let Ok(mut restore) = state.lock() {
        restore.alt_screen = alt_screen;
    }
}

fn transition_layout_recorded(
    out: &mut io::Stdout,
    current: &mut PanelLayout,
    desired: PanelLayout,
    term_rows: u16,
    term_cols: u16,
    pty: &mut ShellPty,
    cleanup_state: &Arc<Mutex<TerminalRestoreState>>,
) -> Result<()> {
    transition_layout(out, current, desired, term_rows, term_cols, pty)?;
    record_terminal_restore_state(cleanup_state, *current, term_rows, term_cols);
    Ok(())
}

/// Move from `*current` to `desired`, applying scrolling-region and PTY-size
/// changes so the inner shell sees a sensible window.
fn transition_layout(
    out: &mut io::Stdout,
    current: &mut PanelLayout,
    desired: PanelLayout,
    term_rows: u16,
    term_cols: u16,
    pty: &mut ShellPty,
) -> Result<()> {
    if *current == desired {
        return Ok(());
    }
    // Always release whatever is currently reserved before applying the new
    // state — keeps the state machine simple even on resize.
    if let PanelLayout::Reserved { height } = *current {
        panel::release(out, height, term_rows, term_cols)?;
    }
    match desired {
        PanelLayout::Hidden => {}
        PanelLayout::Reserved { height } => panel::reserve(out, height, term_rows, term_cols)?,
    }
    resize_pty_for_layout(pty, desired, term_cols, term_rows);
    *current = desired;
    Ok(())
}

fn resize_pty_for_layout(pty: &mut ShellPty, layout: PanelLayout, term_cols: u16, term_rows: u16) {
    let shell_rows = match layout {
        PanelLayout::Hidden => term_rows,
        PanelLayout::Reserved { height } => term_rows.saturating_sub(height).max(1),
    };
    let _ = pty.resize(term_cols, shell_rows);
}

fn sync_cursor_tracker_for_layout(
    cursor: &mut CursorTracker,
    layout: PanelLayout,
    term_cols: u16,
    term_rows: u16,
) {
    let shell_rows = match layout {
        PanelLayout::Hidden => term_rows,
        PanelLayout::Reserved { height } => term_rows.saturating_sub(height).max(1),
    };
    cursor.set_size(term_cols, shell_rows);
    cursor.set_to_bottom_left();
}

fn restore_shell_cursor_if_reserved(
    out: &mut io::Stdout,
    layout: PanelLayout,
    cursor: &CursorTracker,
) -> Result<()> {
    if matches!(layout, PanelLayout::Reserved { .. }) {
        let (col, row) = cursor.position();
        out.queue(MoveTo(col, row))?;
    }
    Ok(())
}

fn flush_mode_pending(
    out: &mut io::Stdout,
    layout: PanelLayout,
    cursor: &mut CursorTracker,
    state: &mut AppState,
    mode_pending: &mut Vec<u8>,
) -> Result<()> {
    restore_shell_cursor_if_reserved(out, layout, cursor)?;
    out.write_all(mode_pending)?;
    if !state.auto_passthrough {
        cursor.feed(mode_pending);
    }
    state.running_output_tail.feed(mode_pending);
    mode_pending.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_osc_event(
    event: osc133::Event,
    state: &mut AppState,
    stdout: &mut io::Stdout,
    layout: &mut PanelLayout,
    term_rows: u16,
    term_cols: u16,
    pty: &mut ShellPty,
    cleanup_state: &Arc<Mutex<TerminalRestoreState>>,
    shell_cursor: &mut CursorTracker,
    last_paint: &mut Instant,
    writer: &mut Box<dyn Write + Send>,
) -> Result<()> {
    match event {
        osc133::Event::PromptStart | osc133::Event::PromptEnd => {
            state.shell_state = ShellState::AtPrompt;
            reset_prompt_tracking(state);
            state.prompt_continuation_active = false;
        }
        osc133::Event::CommandStart => {
            mark_command_started(state);
        }
        osc133::Event::CommandEnd { exit_code } => {
            state.shell_state = ShellState::AtPrompt;
            state.command_started_at = None;
            reset_prompt_tracking(state);
            state.prompt_continuation_active = false;
            state.running_output_tail.clear();
            state.child_input_active = false;
            restore_child_terminal_modes_after_command(state, stdout, cleanup_state)?;
            if command_end_may_touch_queue(state, exit_code) {
                transition_layout_recorded(
                    stdout,
                    layout,
                    PanelLayout::Hidden,
                    term_rows,
                    term_cols,
                    pty,
                    cleanup_state,
                )?;
                sync_cursor_tracker_for_layout(shell_cursor, *layout, term_cols, term_rows);
                *last_paint = Instant::now() - PANEL_REPAINT_INTERVAL;
            }
            handle_command_end(state, exit_code, writer);
        }
        osc133::Event::CurrentDir(path) => {
            state.shell_cwd = Some(path);
        }
    }
    Ok(())
}

fn restore_child_terminal_modes_after_command(
    state: &mut AppState,
    stdout: &mut io::Stdout,
    cleanup_state: &Arc<Mutex<TerminalRestoreState>>,
) -> Result<()> {
    if state.child_alt_screen {
        write!(stdout, "\x1b[?1049l\x1b[?1047l\x1b[?47l")?;
    }
    if state.child_mouse_capture {
        write!(
            stdout,
            "\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        )?;
    }
    if state.child_focus_events {
        write!(stdout, "\x1b[?1004l")?;
    }
    if state.child_alt_screen || state.child_mouse_capture || state.child_focus_events {
        stdout.flush()?;
    }
    state.child_bracketed_paste = false;
    state.child_alt_screen = false;
    state.child_mouse_capture = false;
    state.child_focus_events = false;
    state.auto_passthrough = false;
    record_alt_screen_state(cleanup_state, false);
    Ok(())
}

fn update_child_input_detection(state: &mut AppState, bytes: &[u8]) {
    let saw_line_break = bytes.iter().any(|b| matches!(b, b'\r' | b'\n'));
    state.running_output_tail.feed(bytes);
    if state.child_input_active
        && saw_line_break
        && !state.running_output_tail.looks_like_input_prompt()
    {
        state.child_input_active = false;
    }
    if state.running_output_tail.looks_like_input_prompt()
        && matches!(state.shell_state, ShellState::Running)
        && !state.force_queue
        && !state.effective_passthrough()
    {
        state.child_input_active = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh_auto_passthrough_for_child_modes(
    state: &mut AppState,
    stdout: &mut io::Stdout,
    layout: &mut PanelLayout,
    term_rows: u16,
    term_cols: u16,
    pty: &mut ShellPty,
    cleanup_state: &Arc<Mutex<TerminalRestoreState>>,
    shell_cursor: &mut CursorTracker,
    last_paint: &mut Instant,
    status: Option<&'static str>,
) -> Result<()> {
    let was_passthrough = state.auto_passthrough;
    state.auto_passthrough = state.child_alt_screen || state.child_mouse_capture;
    record_alt_screen_state(cleanup_state, state.child_alt_screen);

    if let Some(status) = status
        && was_passthrough != state.auto_passthrough
    {
        state.set_status(status);
    }
    if was_passthrough == state.auto_passthrough {
        return Ok(());
    }

    let desired = desired_layout(state, term_rows);
    if desired != *layout {
        transition_layout_recorded(
            stdout,
            layout,
            desired,
            term_rows,
            term_cols,
            pty,
            cleanup_state,
        )?;
        sync_cursor_tracker_for_layout(shell_cursor, *layout, term_cols, term_rows);
        *last_paint = Instant::now() - PANEL_REPAINT_INTERVAL;
    }
    Ok(())
}

fn mark_command_started(state: &mut AppState) {
    state.shell_state = ShellState::Running;
    state.command_started_at = Some(Instant::now());
    reset_prompt_tracking(state);
    state.prompt_continuation_active = false;
    state.running_output_tail.clear();
    state.child_input_active = false;
    state.child_bracketed_paste = false;
    state.child_alt_screen = false;
    state.child_mouse_capture = false;
    state.child_focus_events = false;
    state.auto_passthrough = false;
}

/// On CommandEnd, decide whether to dispatch the next queued command. If we
/// recently sent SIGINT (or the exit code looks like one), auto-pause instead.
fn handle_command_end(
    state: &mut AppState,
    exit_code: Option<i32>,
    writer: &mut Box<dyn Write + Send>,
) -> bool {
    state.manual_passthrough = false;
    state.child_bracketed_paste = false;
    state.child_alt_screen = false;
    state.child_mouse_capture = false;
    state.child_focus_events = false;
    state.auto_passthrough = false;
    let recent_sigint = state
        .last_sigint_at
        .map(|t| t.elapsed() <= SIGINT_AUTO_PAUSE_WINDOW)
        .unwrap_or(false);
    let exit_was_sigint = exit_code == Some(130);

    if (recent_sigint || exit_was_sigint) && !state.queue.is_empty() {
        state.queue.paused = true;
        state.last_sigint_at = None;
        state.queue_dirty = true;
        state.set_status("Ctrl-C detected — queue paused. Ctrl-X to resume, Ctrl-K to clear.");
        return false;
    }
    state.last_sigint_at = None;

    if state.queue.paused || state.queue.is_empty() {
        return false;
    }
    if state.editor.editing_index.is_some() {
        state.queue.paused = true;
        state.queue_dirty = true;
        state.set_status("queue paused while editing — Enter save, Esc cancel, Ctrl-X resume");
        return false;
    }
    if dispatch_next_eligible(state, exit_code, writer) {
        mark_command_started(state);
        true
    } else {
        false
    }
}

fn command_end_may_touch_queue(state: &AppState, exit_code: Option<i32>) -> bool {
    let recent_sigint = state
        .last_sigint_at
        .map(|t| t.elapsed() <= SIGINT_AUTO_PAUSE_WINDOW)
        .unwrap_or(false);
    let exit_was_sigint = exit_code == Some(130);
    !(recent_sigint || exit_was_sigint || state.queue.paused || state.queue.is_empty())
}

fn dispatch_next_eligible(
    state: &mut AppState,
    prev_exit: Option<i32>,
    writer: &mut Box<dyn Write + Send>,
) -> bool {
    if let Some(queue_path) = state.queue_path.clone()
        && state.queue_dirty
    {
        save_queue_if_dirty(state, &queue_path);
        if state.queue_dirty || state.queue.paused {
            return false;
        }
    }

    let mut skipped_conditional = false;
    while let Some(item) = state.queue.front().cloned() {
        if item.conditional && prev_exit != Some(0) {
            let _ = state.queue.remove(item.id);
            skipped_conditional = true;
            continue;
        }

        if let Some(queue_path) = state.queue_path.clone() {
            let current_cwd = current_shell_cwd(state).cloned();
            let claim = state.queue.claim_next_eligible_if_current(
                &queue_path,
                item.id,
                prev_exit,
                current_cwd.as_deref(),
                &mut state.queue_known_items,
                &mut state.queue_known_paused,
            );
            match claim {
                Ok(queue::QueueClaim::Claimed(item)) => {
                    if let Err(e) = write_command_to_child(writer, &item.command) {
                        let rollback = state.queue.restore_claimed_front(
                            &queue_path,
                            item,
                            &mut state.queue_known_items,
                            &mut state.queue_known_paused,
                        );
                        state.queue_dirty = rollback.is_err();
                        state.set_status(format!(
                            "dispatch failed; queue paused: {}",
                            truncate_for_status(&e.to_string())
                        ));
                        return false;
                    }
                    state.queue_dirty = false;
                    if state.queue.is_empty() {
                        state.resume_cwd_warning = None;
                        state.resume_cwd_confirmation_started_at = None;
                    }
                    state.set_status(format!(
                        "dispatched: {}",
                        truncate_for_status(&item.command)
                    ));
                    return true;
                }
                Ok(queue::QueueClaim::BlockedByCwd(item)) => {
                    state.queue_dirty = false;
                    state.resume_cwd_warning = resume_cwd_warning_for_current_shell(state);
                    state.resume_cwd_confirmation_started_at = Some(Instant::now());
                    if let Some(current) = current_shell_cwd(state)
                        && let Some(warning) =
                            dispatch_cwd_warning_for_item(&state.queue, &item, current)
                    {
                        state.set_status(warning);
                    }
                    return false;
                }
                Ok(queue::QueueClaim::Stale) => {
                    state.queue_dirty = false;
                    state.set_status("queue changed in another session");
                    return false;
                }
                Err(e) => {
                    state.set_status(format!("queue dispatch sync failed: {e}"));
                    return false;
                }
            }
        }

        if let Some(current) = current_shell_cwd(state)
            && let Some(warning) = dispatch_cwd_warning_for_item(&state.queue, &item, current)
        {
            state.queue.paused = true;
            state.queue_dirty = true;
            state.resume_cwd_warning = resume_cwd_warning_for_current_shell(state);
            state.resume_cwd_confirmation_started_at = Some(Instant::now());
            state.set_status(warning);
            if skipped_conditional {
                save_queue_immediately_after_dispatch(state);
            }
            return false;
        }

        if let Err(e) = write_command_to_child(writer, &item.command) {
            state.queue.paused = true;
            state.queue_dirty = true;
            state.set_status(format!(
                "dispatch failed; queue paused: {}",
                truncate_for_status(&e.to_string())
            ));
            return false;
        }

        let _ = state.queue.remove(item.id);
        state.queue_dirty = true;
        if state.queue.is_empty() {
            state.resume_cwd_warning = None;
            state.resume_cwd_confirmation_started_at = None;
        }
        state.set_status(format!(
            "dispatched: {}",
            truncate_for_status(&item.command)
        ));
        save_queue_immediately_after_dispatch(state);
        return true;
    }

    if skipped_conditional {
        state.queue_dirty = true;
        if state.queue.is_empty() {
            state.resume_cwd_warning = None;
            state.resume_cwd_confirmation_started_at = None;
        }
        state.set_status("skipped chained item: previous command did not succeed");
        save_queue_immediately_after_dispatch(state);
    }
    false
}

fn save_queue_immediately_after_dispatch(state: &mut AppState) {
    if let Some(path) = state.queue_path.clone() {
        save_queue_if_dirty(state, &path);
    }
}

fn write_command_to_child(writer: &mut Box<dyn Write + Send>, command: &str) -> io::Result<()> {
    writer.write_all(command.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Debug, Clone, Copy)]
enum KeyOutcome {
    Continue,
    Quit,
}

fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    writer: &mut Box<dyn Write + Send>,
) -> KeyOutcome {
    use crossterm::event::KeyCode;

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Help overlay: dismiss only on a small set of "I'm done reading" keys,
    // so a stray Ctrl-C or paste while help is open isn't lost or misrouted.
    if state.show_help {
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            state.show_help = false;
            if matches!(state.shell_state, ShellState::Running) {
                state.last_sigint_at = Some(Instant::now());
                let _ = writer.write_all(&[ETX]);
                let _ = writer.flush();
            }
            return KeyOutcome::Continue;
        }
        let dismiss = matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('?') | KeyCode::F(1)
        );
        if dismiss {
            state.show_help = false;
        }
        return KeyOutcome::Continue;
    }

    if state.resume_cwd_confirmation_started_at.is_some() && !is_ctrl_x(&key) {
        state.resume_cwd_confirmation_started_at = None;
    }

    if matches!(key.code, KeyCode::F(1)) && !state.effective_passthrough() {
        state.show_help = true;
        return KeyOutcome::Continue;
    }

    // Double-Esc toggles passthrough — SSH-safe alternative to Ctrl-\.
    let esc_clears_editor = state.editor_owns_input()
        && (!state.editor.buffer.is_empty() || state.editor.editing_index.is_some());
    if matches!(key.code, KeyCode::Esc)
        && !ctrl
        && !key.modifiers.contains(KeyModifiers::ALT)
        && key.kind == KeyEventKind::Press
        && !state.auto_passthrough
        && !esc_clears_editor
        && (state.panel_should_be_visible() || state.manual_passthrough)
    {
        let now = Instant::now();
        let double_tap = state
            .last_esc_at
            .map(|t| now.duration_since(t) <= ESC_DOUBLE_TAP_WINDOW)
            .unwrap_or(false);
        if double_tap {
            state.last_esc_at = None;
            state.toggle_manual_passthrough();
            return KeyOutcome::Continue;
        }
        state.last_esc_at = Some(now);
        // fall through so the first Esc behaves normally
    } else if state.last_esc_at.is_some() {
        state.last_esc_at = None;
    }

    if !state.editor_owns_input() && should_open_queue_for_running_key(&key, state) {
        state.activate_queue_for_running_command();
    }
    let editor_owns = state.editor_owns_input();

    if ctrl
        && matches!(key.code, KeyCode::Char('\\'))
        && matches!(state.shell_state, ShellState::Running)
        && !state.effective_passthrough()
    {
        let _ = writer.write_all(&[FS]);
        let _ = writer.flush();
        state.set_status("sent Ctrl-\\ to running command");
        return KeyOutcome::Continue;
    }

    if ctrl && matches!(key.code, KeyCode::Char('\\')) && (editor_owns || state.manual_passthrough)
    {
        state.toggle_manual_passthrough();
        return KeyOutcome::Continue;
    }

    if ctrl
        && matches!(key.code, KeyCode::Char('c' | 'C'))
        && matches!(state.shell_state, ShellState::Running)
        && !state.effective_passthrough()
    {
        state.last_sigint_at = Some(Instant::now());
        state.child_input_active = false;
        state.running_output_tail.clear();
        let _ = writer.write_all(&[ETX]);
        let _ = writer.flush();
        state.set_status("sent Ctrl-C to running command");
        return KeyOutcome::Continue;
    }

    if ctrl
        && matches!(key.code, KeyCode::Char('z' | 'Z'))
        && matches!(state.shell_state, ShellState::Running)
        && !state.effective_passthrough()
    {
        let _ = writer.write_all(&[0x1a]);
        let _ = writer.flush();
        state.set_status("sent Ctrl-Z to running command");
        return KeyOutcome::Continue;
    }

    if ctrl
        && matches!(key.code, KeyCode::Char('q' | 'Q'))
        && !editor_owns
        && !state.effective_passthrough()
    {
        state.toggle_force_queue();
        return KeyOutcome::Continue;
    }

    // Ctrl-D quit flow:
    //   - empty buffer + no edit: quit, but require a second press within
    //     QUIT_CONFIRM_WINDOW if the queue still has items.
    //   - the editor binds Ctrl-D to DeleteEdited only, so we have to handle
    //     the quit case here.
    if ctrl
        && matches!(key.code, KeyCode::Char('d' | 'D'))
        && editor_owns
        && state.editor.editing_index.is_none()
        && state.editor.buffer.is_empty()
    {
        if state.queue.is_empty() {
            return KeyOutcome::Quit;
        }
        if state.pending_quit_active() {
            state.queue.clear();
            state.queue.paused = false;
            state.resume_cwd_warning = None;
            state.resume_cwd_confirmation_started_at = None;
            state.queue_dirty = true;
            return KeyOutcome::Quit;
        }
        state.pending_quit_at = Some(Instant::now());
        state.set_status(format!(
            "queue has {} pending — press ^D again to discard and quit",
            state.queue.len()
        ));
        return KeyOutcome::Continue;
    }

    // Any other key cancels a pending quit confirmation.
    if state.pending_quit_at.is_some() {
        state.pending_quit_at = None;
    }

    if !editor_owns
        && !state.terminal_allows_panel
        && !state.effective_passthrough()
        && state.queue.paused
        && !state.queue.is_empty()
        && ctrl
    {
        if matches!(key.code, KeyCode::Char('x' | 'X')) {
            toggle_queue_pause(state, writer);
            return KeyOutcome::Continue;
        }
        if matches!(key.code, KeyCode::Char('k' | 'K')) {
            clear_queue(state);
            return KeyOutcome::Continue;
        }
    }

    if !editor_owns {
        let was_child_input = state.child_input_prompt_active();
        let was_any_key_prompt =
            was_child_input && state.running_output_tail.looks_like_any_key_prompt();
        let submitted_prompt_line = update_prompt_buffer_for_forwarded_key(&key, state);
        let bytes = encode_key_for_pty(&key);
        if bytes.contains(&ETX) {
            state.last_sigint_at = Some(Instant::now());
        }
        if !bytes.is_empty() {
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
        }
        if was_child_input
            && (matches!(key.code, KeyCode::Enter)
                || bytes.contains(&ETX)
                || (was_any_key_prompt && !bytes.is_empty()))
        {
            state.child_input_active = false;
            state.running_output_tail.clear();
        }
        if bytes.contains(&ETX) {
            state.prompt_continuation_active = false;
        }
        if let Some(line) = submitted_prompt_line.as_deref()
            && line_may_start_shell_continuation(line)
        {
            state.prompt_continuation_active = true;
        }
        if should_optimistically_mark_command_started(&key, state, submitted_prompt_line.as_deref())
        {
            mark_command_started(state);
        }
        return KeyOutcome::Continue;
    }

    if matches!(key.code, KeyCode::Char('?'))
        && !ctrl
        && state.editor.buffer.is_empty()
        && state.editor.editing_index.is_none()
    {
        state.show_help = true;
        return KeyOutcome::Continue;
    }

    let action = state.editor.handle_key(key, &state.queue);
    match action {
        InputAction::Nothing => {}
        InputAction::ForwardToChild(bytes) => {
            if bytes.contains(&ETX) {
                state.last_sigint_at = Some(Instant::now());
            }
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
        }
        InputAction::EnqueueCurrent {
            command,
            conditional,
        } => {
            let origin = current_shell_cwd(state).cloned();
            if state.queue.is_empty() {
                if let Some(cwd) = origin.clone() {
                    state.queue.set_origin_cwd(cwd);
                }
                state.resume_cwd_warning = None;
                state.resume_cwd_confirmation_started_at = None;
            }
            state.queue.push_with_origin(&command, conditional, origin);
            state.queue_dirty = true;
            state.set_status(format!("added: {}", truncate_for_status(&command)));
        }
        InputAction::CommitEdit {
            index,
            command,
            conditional,
        } => {
            if let Some(id) = state.queue.items().get(index).map(|it| it.id) {
                state.queue.edit(id, &command);
                state.queue.set_conditional(id, conditional);
                state.queue_dirty = true;
                state.set_status(format!("saved: {}", truncate_for_status(&command)));
            }
        }
        InputAction::CancelEdit => {
            state.set_status("edit cancelled");
        }
        InputAction::DeleteEdited => {
            if let Some(idx) = state.editor.editing_index
                && let Some(id) = state.queue.items().get(idx).map(|it| it.id)
                && let Some(removed) = state.queue.remove(id)
            {
                state.queue_dirty = true;
                state.set_status(format!(
                    "removed: {}",
                    truncate_for_status(&removed.command)
                ));
            }
            state.editor.reset();
        }
        InputAction::MoveEditedUp => {
            if let Some(idx) = state.editor.editing_index
                && let Some(id) = state.queue.items().get(idx).map(|it| it.id)
                && state.queue.move_up(id)
            {
                state.editor.editing_index = Some(idx.saturating_sub(1));
                state.queue_dirty = true;
            }
        }
        InputAction::MoveEditedDown => {
            if let Some(idx) = state.editor.editing_index
                && let Some(id) = state.queue.items().get(idx).map(|it| it.id)
                && state.queue.move_down(id)
            {
                state.editor.editing_index = Some(idx + 1);
                state.queue_dirty = true;
            }
        }
        InputAction::TogglePause => {
            toggle_queue_pause(state, writer);
        }
        InputAction::ClearQueue => {
            clear_queue(state);
        }
        InputAction::ToggleForceQueue => {
            state.toggle_force_queue();
        }
        InputAction::ToggleHelp => {
            state.show_help = true;
        }
        InputAction::ToggleChain { now_on } => {
            let msg: String = if !state.chain_seen {
                state.chain_seen = true;
                if now_on {
                    "chain ON — runs only if the previous command succeeds (Tab to undo)".into()
                } else {
                    "chain OFF".into()
                }
            } else if now_on {
                "chain ON".into()
            } else {
                "chain OFF".into()
            };
            state.set_status(msg);
        }
    }
    KeyOutcome::Continue
}

fn toggle_queue_pause(state: &mut AppState, writer: &mut Box<dyn Write + Send>) {
    let was_paused = state.queue.paused;
    if was_paused && state.editor.editing_index.is_some() {
        state.set_status("finish or cancel the edit before resuming the queue");
        return;
    }
    if was_paused && !state.queue.is_empty() {
        state.resume_cwd_warning = resume_cwd_warning_for_current_shell(state);
        if state.resume_cwd_warning.is_none() {
            state.resume_cwd_confirmation_started_at = None;
        }
    }
    if was_paused && !state.queue.is_empty() && state.resume_cwd_warning.is_some() {
        let confirmed = state
            .resume_cwd_confirmation_started_at
            .map(|t| t.elapsed() <= RESUME_CWD_CONFIRM_WINDOW)
            .unwrap_or(false);
        if !confirmed {
            let warning = state.resume_cwd_warning.clone().unwrap_or_default();
            state.resume_cwd_confirmation_started_at = Some(Instant::now());
            state.set_status(warning);
            return;
        }
        if let Some(cwd) = current_shell_cwd(state).cloned() {
            state.queue.retarget_origin_cwd(cwd);
        }
        state.resume_cwd_warning = None;
        state.resume_cwd_confirmation_started_at = None;
    }
    state.queue.paused = !state.queue.paused;
    state.restored_queue_paused_locally = false;
    state.queue_dirty = true;
    if state.queue.paused {
        state.set_status("queue paused");
    } else if was_paused
        && matches!(state.shell_state, ShellState::AtPrompt)
        && !state.queue.is_empty()
    {
        let before = state.queue.len();
        if !dispatch_next_eligible(state, None, writer) && state.queue.len() == before {
            state.set_status("queue resumed");
        }
    } else {
        state.set_status("queue resumed");
    }
}

fn clear_queue(state: &mut AppState) {
    state.queue.clear();
    state.queue.paused = false;
    state.restored_queue_paused_locally = false;
    set_queue_origin_to_session(state);
    state.resume_cwd_warning = None;
    state.resume_cwd_confirmation_started_at = None;
    if state.editor.editing_index.is_some() {
        state.editor.reset();
    }
    state.queue_dirty = true;
    state.set_status("queue cleared");
}

fn truncate_for_status(s: &str) -> String {
    const MAX: usize = 60;
    let display = s
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', " ⏎ ");
    if display.chars().count() <= MAX {
        display
    } else {
        let mut out: String = display.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

fn is_ctrl_x(key: &KeyEvent) -> bool {
    use crossterm::event::KeyCode;

    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('x' | 'X'))
}

fn handle_paste(text: String, state: &mut AppState, writer: &mut Box<dyn Write + Send>) {
    if state.show_help {
        state.show_help = false;
    }
    if state.pending_quit_at.is_some() {
        state.pending_quit_at = None;
    }
    if state.resume_cwd_confirmation_started_at.is_some() {
        state.resume_cwd_confirmation_started_at = None;
    }
    if !state.editor_owns_input() && should_open_queue_for_running_paste(state) {
        state.activate_queue_for_running_command();
    }
    if !state.editor_owns_input() {
        // Pass through to the running program. Only wrap with bracketed-paste
        // markers when the child explicitly enabled paste mode; otherwise a
        // plain `read`, password prompt, or simple REPL would receive the
        // markers as literal input.
        let submitted_prompt_line = update_prompt_buffer_for_forwarded_paste(&text, state);
        if state.child_bracketed_paste {
            let _ = writer.write_all(b"\x1b[200~");
        }
        let _ = writer.write_all(text.as_bytes());
        if state.child_bracketed_paste {
            let _ = writer.write_all(b"\x1b[201~");
        }
        let _ = writer.flush();
        if state.child_input_active && (text.contains('\n') || text.contains('\r')) {
            state.child_input_active = false;
            state.running_output_tail.clear();
        }
        if let Some(line) = submitted_prompt_line.as_deref()
            && line_may_start_shell_continuation(line)
        {
            state.prompt_continuation_active = true;
        }
        if submitted_prompt_line
            .as_deref()
            .map(is_likely_complete_shell_command)
            .unwrap_or(false)
        {
            mark_command_started(state);
        }
        return;
    }
    // Treat the whole paste as one queue item and preserve internal newlines.
    // This keeps heredocs, loops, functions, and copied install scripts intact;
    // the panel renders newlines as markers instead of printing them literally.
    let normalized = normalize_queue_paste(&text);
    state.editor.insert_str(&normalized);
}

fn update_prompt_buffer_for_forwarded_paste(text: &str, state: &mut AppState) -> Option<String> {
    if !matches!(state.shell_state, ShellState::AtPrompt)
        || state.effective_passthrough()
        || !state.prompt_buffer_reliable
    {
        return None;
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.contains('\n') {
        let submitted = first_meaningful_submitted_paste_line(
            &normalized,
            &state.prompt_buffer,
            state.prompt_cursor,
        );
        reset_prompt_tracking(state);
        return submitted;
    }
    state
        .prompt_buffer
        .insert_str(state.prompt_cursor, &normalized);
    state.prompt_cursor += normalized.len();
    None
}

fn first_meaningful_submitted_paste_line(
    normalized: &str,
    prompt_buffer: &str,
    prompt_cursor: usize,
) -> Option<String> {
    let mut parts = normalized.split('\n');
    let head = parts.next().unwrap_or_default();
    let before = &prompt_buffer[..prompt_cursor];
    let after = &prompt_buffer[prompt_cursor..];
    let first = format!("{before}{head}{after}");
    if !is_shell_noop_line(&first) {
        return Some(first);
    }

    let rest: Vec<&str> = parts.collect();
    let submitted_rest = if normalized.ends_with('\n') {
        rest.len()
    } else {
        rest.len().saturating_sub(1)
    };
    rest.into_iter()
        .take(submitted_rest)
        .find(|line| !is_shell_noop_line(line))
        .map(str::to_string)
}

fn is_shell_noop_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

fn normalize_queue_paste(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    normalized.trim_end_matches('\n').to_string()
}

fn should_open_queue_for_running_paste(state: &AppState) -> bool {
    matches!(state.shell_state, ShellState::Running)
        && !state.effective_passthrough()
        && !state.child_input_prompt_active()
}

fn should_open_queue_for_running_key(key: &KeyEvent, state: &AppState) -> bool {
    use crossterm::event::KeyCode;

    if !matches!(state.shell_state, ShellState::Running)
        || state.effective_passthrough()
        || state.child_input_prompt_active()
    {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return false;
    }
    matches!(
        key.code,
        KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::Enter
    )
}

fn should_optimistically_mark_command_started(
    key: &KeyEvent,
    state: &AppState,
    submitted_prompt_line: Option<&str>,
) -> bool {
    use crossterm::event::KeyCode;

    matches!(state.shell_state, ShellState::AtPrompt)
        && matches!(key.code, KeyCode::Enter)
        && !state.effective_passthrough()
        && !state.prompt_continuation_active
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && submitted_prompt_line
            .map(is_likely_complete_shell_command)
            .unwrap_or(false)
}

fn update_prompt_buffer_for_forwarded_key(key: &KeyEvent, state: &mut AppState) -> Option<String> {
    use crossterm::event::KeyCode;

    if !matches!(state.shell_state, ShellState::AtPrompt) || state.effective_passthrough() {
        return None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => {
            prompt_insert_char(state, c);
            None
        }
        KeyCode::Backspace => {
            prompt_backspace(state);
            None
        }
        KeyCode::Delete => {
            prompt_delete_at_cursor(state);
            None
        }
        KeyCode::Left if alt => {
            prompt_move_word_left(state);
            None
        }
        KeyCode::Right if alt => {
            prompt_move_word_right(state);
            None
        }
        KeyCode::Left => {
            prompt_move_left(state);
            None
        }
        KeyCode::Right => {
            prompt_move_right(state);
            None
        }
        KeyCode::Home => {
            prompt_move_home(state);
            None
        }
        KeyCode::End => {
            prompt_move_end(state);
            None
        }
        KeyCode::Char('a' | 'A') if ctrl => {
            prompt_move_home(state);
            None
        }
        KeyCode::Char('e' | 'E') if ctrl => {
            prompt_move_end(state);
            None
        }
        KeyCode::Char('b' | 'B') if ctrl => {
            prompt_move_left(state);
            None
        }
        KeyCode::Char('f' | 'F') if ctrl => {
            prompt_move_right(state);
            None
        }
        KeyCode::Char('h' | 'H') if ctrl => {
            prompt_backspace(state);
            None
        }
        KeyCode::Char('d' | 'D') if ctrl => {
            prompt_delete_at_cursor(state);
            None
        }
        KeyCode::Char('k' | 'K') if ctrl => {
            prompt_kill_to_end(state);
            None
        }
        KeyCode::Char('u' | 'U') if ctrl => {
            prompt_kill_to_start(state);
            None
        }
        KeyCode::Char('w' | 'W') if ctrl => {
            prompt_delete_previous_word(state);
            None
        }
        KeyCode::Char('b' | 'B') if alt => {
            prompt_move_word_left(state);
            None
        }
        KeyCode::Char('f' | 'F') if alt => {
            prompt_move_word_right(state);
            None
        }
        KeyCode::Char('l' | 'L') if ctrl => None,
        KeyCode::Char('c' | 'C') if ctrl => {
            reset_prompt_tracking(state);
            state.prompt_continuation_active = false;
            None
        }
        KeyCode::Enter => {
            let submitted = state
                .prompt_buffer_reliable
                .then(|| state.prompt_buffer.clone());
            reset_prompt_tracking(state);
            submitted
        }
        _ => {
            invalidate_prompt_tracking(state);
            None
        }
    }
}

fn reset_prompt_tracking(state: &mut AppState) {
    state.prompt_buffer.clear();
    state.prompt_cursor = 0;
    state.prompt_buffer_reliable = true;
}

fn invalidate_prompt_tracking(state: &mut AppState) {
    state.prompt_buffer.clear();
    state.prompt_cursor = 0;
    state.prompt_buffer_reliable = false;
}

fn prompt_insert_char(state: &mut AppState, c: char) {
    if !state.prompt_buffer_reliable {
        return;
    }
    state.prompt_buffer.insert(state.prompt_cursor, c);
    state.prompt_cursor += c.len_utf8();
}

fn prompt_backspace(state: &mut AppState) {
    if !state.prompt_buffer_reliable || state.prompt_cursor == 0 {
        return;
    }
    let prev = prompt_prev_boundary(&state.prompt_buffer, state.prompt_cursor);
    state
        .prompt_buffer
        .replace_range(prev..state.prompt_cursor, "");
    state.prompt_cursor = prev;
}

fn prompt_delete_at_cursor(state: &mut AppState) {
    if !state.prompt_buffer_reliable || state.prompt_cursor >= state.prompt_buffer.len() {
        return;
    }
    let next = prompt_next_boundary(&state.prompt_buffer, state.prompt_cursor);
    state
        .prompt_buffer
        .replace_range(state.prompt_cursor..next, "");
}

fn prompt_move_left(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = prompt_prev_boundary(&state.prompt_buffer, state.prompt_cursor);
    }
}

fn prompt_move_right(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = prompt_next_boundary(&state.prompt_buffer, state.prompt_cursor);
    }
}

fn prompt_move_home(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = 0;
    }
}

fn prompt_move_end(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = state.prompt_buffer.len();
    }
}

fn prompt_move_word_left(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = prompt_previous_word_start(&state.prompt_buffer, state.prompt_cursor);
    }
}

fn prompt_move_word_right(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_cursor = prompt_next_word_end(&state.prompt_buffer, state.prompt_cursor);
    }
}

fn prompt_kill_to_start(state: &mut AppState) {
    if !state.prompt_buffer_reliable {
        reset_prompt_tracking(state);
        return;
    }
    state.prompt_buffer.replace_range(..state.prompt_cursor, "");
    state.prompt_cursor = 0;
}

fn prompt_kill_to_end(state: &mut AppState) {
    if state.prompt_buffer_reliable {
        state.prompt_buffer.replace_range(state.prompt_cursor.., "");
    }
}

fn prompt_delete_previous_word(state: &mut AppState) {
    if !state.prompt_buffer_reliable || state.prompt_cursor == 0 {
        return;
    }
    let start = prompt_previous_word_start(&state.prompt_buffer, state.prompt_cursor);
    state
        .prompt_buffer
        .replace_range(start..state.prompt_cursor, "");
    state.prompt_cursor = start;
}

fn prompt_previous_word_start(s: &str, cursor: usize) -> usize {
    let mut start = cursor;
    while start > 0 {
        let prev = prompt_prev_boundary(s, start);
        let ch = s[prev..start].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        start = prev;
    }
    while start > 0 {
        let prev = prompt_prev_boundary(s, start);
        let ch = s[prev..start].chars().next().unwrap_or(' ');
        if ch.is_whitespace() {
            break;
        }
        start = prev;
    }
    start
}

fn prompt_next_word_end(s: &str, cursor: usize) -> usize {
    let mut end = cursor;
    while end < s.len() {
        let next = prompt_next_boundary(s, end);
        let ch = s[end..next].chars().next().unwrap_or(' ');
        if !ch.is_whitespace() {
            break;
        }
        end = next;
    }
    while end < s.len() {
        let next = prompt_next_boundary(s, end);
        let ch = s[end..next].chars().next().unwrap_or(' ');
        if ch.is_whitespace() {
            break;
        }
        end = next;
    }
    end
}

fn prompt_prev_boundary(s: &str, cursor: usize) -> usize {
    s[..cursor]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn prompt_next_boundary(s: &str, cursor: usize) -> usize {
    s[cursor..]
        .char_indices()
        .nth(1)
        .map(|(idx, _)| cursor + idx)
        .unwrap_or(s.len())
}

fn is_likely_complete_shell_command(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.ends_with('\\')
        || trimmed.ends_with('|')
        || trimmed.ends_with("&&")
        || trimmed.ends_with("||")
        || has_unclosed_quote(trimmed)
        || contains_heredoc_operator(trimmed)
        || has_unclosed_compound_construct(trimmed)
        || has_unclosed_shell_grouping(trimmed)
    {
        return false;
    }

    let last_word = trimmed
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or("");
    !matches!(last_word, "do" | "then" | "else" | "elif" | "case")
}

fn line_may_start_shell_continuation(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !is_likely_complete_shell_command(trimmed)
}

fn has_unclosed_compound_construct(line: &str) -> bool {
    match first_shell_word(line) {
        Some("if") => !shell_keyword_present(line, "fi"),
        Some("for" | "while" | "until" | "select") => !shell_keyword_present(line, "done"),
        Some("case") => !shell_keyword_present(line, "esac"),
        _ => has_unclosed_brace_block(line),
    }
}

fn first_shell_word(line: &str) -> Option<&str> {
    shell_words(line).into_iter().next()
}

fn shell_keyword_present(line: &str, needle: &str) -> bool {
    shell_words(line).into_iter().any(|word| word == needle)
}

fn shell_words(line: &str) -> Vec<&str> {
    line.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '{' | '}'))
        .filter(|s| !s.is_empty())
        .collect()
}

fn has_unclosed_brace_block(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;

    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '{' if !in_single && !in_double => depth += 1,
            '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    depth > 0
}

fn has_unclosed_shell_grouping(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '$' if !in_single && chars.peek() == Some(&'(') => {
                chars.next();
                depth += 1;
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && depth > 0 => depth -= 1,
            _ => {}
        }
    }

    depth > 0
}

fn has_unclosed_quote(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }

    in_single || in_double || escaped
}

fn contains_heredoc_operator(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'\\' && !in_single {
            escaped = true;
            i += 1;
            continue;
        }
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'<' if !in_single && !in_double && bytes.get(i + 1) == Some(&b'<') => {
                return bytes.get(i + 2) != Some(&b'<');
            }
            _ => {}
        }
        i += 1;
    }
    false
}

fn encode_mouse_event_for_pty(mouse: MouseEvent) -> Vec<u8> {
    let (mut cb, final_byte) = sgr_mouse_code(mouse.kind);
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    format!(
        "\x1b[<{};{};{}{}",
        cb,
        mouse.column.saturating_add(1),
        mouse.row.saturating_add(1),
        final_byte
    )
    .into_bytes()
}

fn sgr_mouse_code(kind: MouseEventKind) -> (u16, char) {
    let button_code = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    match kind {
        MouseEventKind::Down(button) => (button_code(button), 'M'),
        MouseEventKind::Up(button) => (button_code(button), 'm'),
        MouseEventKind::Drag(button) => (button_code(button) + 32, 'M'),
        MouseEventKind::Moved => (35, 'M'),
        MouseEventKind::ScrollUp => (64, 'M'),
        MouseEventKind::ScrollDown => (65, 'M'),
        MouseEventKind::ScrollLeft => (66, 'M'),
        MouseEventKind::ScrollRight => (67, 'M'),
    }
}

fn encode_key_for_pty(key: &KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let nav_modifier = navigation_modifier_param(key);
    let mut out = Vec::new();
    let push_alt = |out: &mut Vec<u8>| out.push(0x1B);

    match key.code {
        Char(c) => {
            if alt {
                push_alt(&mut out);
            }
            if ctrl {
                let lc = c.to_ascii_lowercase();
                let code = match lc {
                    'a'..='z' => Some((lc as u8) - b'a' + 1),
                    '@' => Some(0),
                    '[' | '\\' | ']' | '^' | '_' => Some((lc as u8) - 0x40),
                    _ => None,
                };
                if let Some(b) = code {
                    out.push(b);
                } else {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Backspace if alt => out.extend_from_slice(b"\x1b\x7f"),
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Left => push_navigation_csi(&mut out, nav_modifier, b'D', b"\x1b[D"),
        Right => push_navigation_csi(&mut out, nav_modifier, b'C', b"\x1b[C"),
        Up => push_navigation_csi(&mut out, nav_modifier, b'A', b"\x1b[A"),
        Down => push_navigation_csi(&mut out, nav_modifier, b'B', b"\x1b[B"),
        Home => push_navigation_csi(&mut out, nav_modifier, b'H', b"\x1b[H"),
        End => push_navigation_csi(&mut out, nav_modifier, b'F', b"\x1b[F"),
        PageUp => push_navigation_tilde_csi(&mut out, nav_modifier, 5, b"\x1b[5~"),
        PageDown => push_navigation_tilde_csi(&mut out, nav_modifier, 6, b"\x1b[6~"),
        Insert => push_navigation_tilde_csi(&mut out, nav_modifier, 2, b"\x1b[2~"),
        Delete => push_navigation_tilde_csi(&mut out, nav_modifier, 3, b"\x1b[3~"),
        F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}

fn navigation_modifier_param(key: &KeyEvent) -> Option<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (shift, alt, ctrl) {
        (false, false, false) => None,
        (true, false, false) => Some(2),
        (false, true, false) => Some(3),
        (true, true, false) => Some(4),
        (false, false, true) => Some(5),
        (true, false, true) => Some(6),
        (false, true, true) => Some(7),
        (true, true, true) => Some(8),
    }
}

fn push_modified_csi(out: &mut Vec<u8>, final_byte: u8, modifier: u8) {
    out.extend_from_slice(format!("\x1b[1;{modifier}").as_bytes());
    out.push(final_byte);
}

fn push_modified_tilde_csi(out: &mut Vec<u8>, code: u8, modifier: u8) {
    out.extend_from_slice(format!("\x1b[{code};{modifier}~").as_bytes());
}

fn push_navigation_csi(out: &mut Vec<u8>, modifier: Option<u8>, final_byte: u8, plain: &[u8]) {
    if let Some(modifier) = modifier {
        push_modified_csi(out, final_byte, modifier);
    } else {
        out.extend_from_slice(plain);
    }
}

fn push_navigation_tilde_csi(out: &mut Vec<u8>, modifier: Option<u8>, code: u8, plain: &[u8]) {
    if let Some(modifier) = modifier {
        push_modified_tilde_csi(out, code, modifier);
    } else {
        out.extend_from_slice(plain);
    }
}

fn restore_terminal(state: &Arc<Mutex<TerminalRestoreState>>) -> Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, Show);
    let restore = state.lock().map(|s| *s).unwrap_or(TerminalRestoreState {
        layout: PanelLayout::Hidden,
        rows: crossterm::terminal::size()
            .map(|(_, rows)| rows.max(1))
            .unwrap_or(24),
        cols: 80,
        alt_screen: false,
    });
    let _ = restore_terminal_display(&mut stdout, restore);
    let _ = write!(
        stdout,
        "\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1004l"
    );
    execute!(stdout, DisableBracketedPaste)?;
    disable_raw_mode()?;
    Ok(())
}

fn restore_terminal_display(out: &mut impl Write, state: TerminalRestoreState) -> io::Result<()> {
    let rows = state.rows.max(1);
    let cols = state.cols.max(1);
    if state.alt_screen {
        write!(out, "\x1b[?1049l\x1b[?1047l\x1b[?47l")?;
    }
    match state.layout {
        PanelLayout::Reserved { height } if height > 0 && height < rows => {
            panel::release(out, height, rows, cols)?;
        }
        _ => {
            // Reset scrolling region and put the cursor at the normal shell
            // prompt location even if no panel was reserved.
            write!(out, "\x1b[1;{}r", rows)?;
            out.queue(MoveTo(0, rows.saturating_sub(1)))?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Capture-into-Vec writer for unit tests of dispatch logic.
    pub struct VecWriter(pub Arc<Mutex<Vec<u8>>>);
    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "child closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn make_state() -> AppState {
        AppState {
            queue: Queue::new(),
            editor: LineEditor::new(),
            shell_state: ShellState::Running,
            manual_passthrough: false,
            auto_passthrough: false,
            child_alt_screen: false,
            child_mouse_capture: false,
            child_focus_events: false,
            force_queue: false,
            show_help: false,
            terminal_allows_panel: true,
            last_sigint_at: None,
            command_started_at: None,
            prompt_buffer: String::new(),
            prompt_cursor: 0,
            prompt_buffer_reliable: true,
            prompt_continuation_active: false,
            running_output_tail: OutputTail::default(),
            child_input_active: false,
            child_bracketed_paste: false,
            pending_quit_at: None,
            last_esc_at: None,
            chain_seen: false,
            status: String::new(),
            status_set_at: None,
            queue_dirty: false,
            queue_save_error: None,
            queue_path: None,
            queue_known_items: Vec::new(),
            queue_known_paused: false,
            queue_external_change_pending: false,
            restored_queue_paused_locally: false,
            session_cwd: Some(PathBuf::from("/tmp/cmdq-test")),
            shell_cwd: Some(PathBuf::from("/tmp/cmdq-test")),
            resume_cwd_warning: None,
            resume_cwd_confirmation_started_at: None,
        }
    }

    #[test]
    fn encode_mouse_event_for_pty_uses_sgr_mouse_coordinates() {
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 9,
            row: 4,
            modifiers: KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<20;10;5M");

        let mouse = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<2;1;1m");

        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 3,
            modifiers: KeyModifiers::ALT,
        };
        assert_eq!(encode_mouse_event_for_pty(mouse), b"\x1b[<73;3;4M");
    }

    #[test]
    fn mouse_capture_keeps_running_app_in_passthrough() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY);
        assert!(s.panel_should_be_visible());
        assert!(s.editor_owns_input());

        s.child_mouse_capture = true;
        s.auto_passthrough = true;

        assert!(!s.panel_should_be_visible());
        assert!(!s.editor_owns_input());
    }

    #[test]
    fn encode_key_for_pty_preserves_alt_arrows() {
        use crossterm::event::KeyCode;

        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            b"\x1b[1;3D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            b"\x1b[1;3C"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            b"\x1b[1;3A"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            b"\x1b[1;3B"
        );
    }

    #[test]
    fn encode_key_for_pty_preserves_modified_navigation_keys() {
        use crossterm::event::KeyCode;

        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
            b"\x1b[1;5H"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
            b"\x1b[1;5F"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
            b"\x1b[3;5~"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
            b"\x1b[1;2D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::CONTROL
            )),
            b"\x1b[1;7D"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
            b"\x1b\x7f"
        );
        assert_eq!(
            encode_key_for_pty(&KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL)),
            &[FS]
        );
    }

    #[test]
    fn cleanup_guard_runs_on_scope_exit() {
        let called = Arc::new(Mutex::new(false));
        {
            let called = called.clone();
            let _guard = CleanupGuard::new(move || {
                *called.lock().unwrap() = true;
            });
        }
        assert!(*called.lock().unwrap());
    }

    #[test]
    fn restore_terminal_display_releases_reserved_panel_rows() {
        let mut out = Vec::new();
        restore_terminal_display(
            &mut out,
            TerminalRestoreState {
                layout: PanelLayout::Reserved { height: 3 },
                rows: 10,
                cols: 40,
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
                cols: 40,
                alt_screen: true,
            },
        )
        .unwrap();

        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?1049l"), "alt-screen exit missing: {s:?}");
        assert!(s.contains("\x1b[1;10r"), "scroll region reset: {s:?}");
    }

    #[test]
    fn output_tail_detects_child_input_prompts() {
        let mut tail = OutputTail::default();
        tail.feed(b"\x1b]133;C\x07name? ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"\x1b[31mPassword: \x1b[0m");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"name: ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"mysql> ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Press ENTER to continue");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Press any key to continue");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"downloaded 42%");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Downloading: ");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Error: ");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"next -> ");
        assert!(!tail.looks_like_input_prompt());
    }

    #[test]
    fn child_input_prompt_keeps_plain_keys_with_running_child() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        s.running_output_tail.feed(b"name? ");
        s.child_input_active = true;
        assert!(s.panel_should_be_visible());
        assert!(!s.editor_owns_input());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        for c in "alice".chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(s.editor.buffer.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"alice\r");
        assert!(!s.child_input_active);
    }

    #[test]
    fn any_key_prompt_releases_after_one_key_answer() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        s.running_output_tail.feed(b"Press any key to continue");
        assert!(s.child_input_prompt_active());
        assert!(!s.editor_owns_input());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"x");
        assert!(!s.child_input_active);
        assert!(!s.child_input_prompt_active());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"x");
        assert_eq!(s.editor.buffer, "q");
    }

    #[test]
    fn child_input_prompt_clears_after_fresh_output_line() {
        let mut s = make_state();
        s.child_input_active = true;
        s.running_output_tail.feed(b"continue? ");

        update_child_input_detection(&mut s, b"y\r\ncontinuing\r\n");

        assert!(
            !s.child_input_active,
            "single-key prompts should release key routing once the child prints a fresh non-prompt line"
        );
    }

    #[test]
    fn same_chunk_command_start_then_bracketed_paste_enable_keeps_child_paste_mode() {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.child_bracketed_paste = true;
        let bytes = b"\x1b]133;C\x07\x1b[?2004h";
        let mut osc = Detector::new();
        let mut mode = mode_detect::Detector::new();
        let osc_events = osc.feed_with_offsets(bytes);
        let mode_events = mode.feed_with_offsets(bytes);
        let mut osc_idx = 0usize;

        for mode_event in &mode_events {
            while let Some(osc_event) = osc_events.get(osc_idx)
                && osc_event.end <= mode_event.start
            {
                if matches!(osc_event.event, osc133::Event::CommandStart) {
                    mark_command_started(&mut s);
                }
                osc_idx += 1;
            }
            if matches!(mode_event.kind, mode_detect::Event::BracketedPasteEnable) {
                s.child_bracketed_paste = true;
            }
        }
        while let Some(osc_event) = osc_events.get(osc_idx) {
            if matches!(osc_event.event, osc133::Event::CommandStart) {
                mark_command_started(&mut s);
            }
            osc_idx += 1;
        }

        assert!(
            s.child_bracketed_paste,
            "a child paste-mode enable after command start must not be cleared by stale prompt state"
        );
    }

    #[test]
    fn ctrl_c_interrupts_running_child_even_with_dirty_queue_input() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.force_queue = true;
        s.editor.insert_str("echo draft");
        assert!(s.editor_owns_input());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), &[ETX]);
        assert_eq!(s.editor.buffer, "echo draft");
        assert!(s.last_sigint_at.is_some());
    }

    #[test]
    fn ctrl_z_reaches_running_child_even_when_panel_owns_input() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.force_queue = true;
        s.editor.insert_str("echo draft");
        assert!(s.editor_owns_input());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), &[0x1a]);
        assert_eq!(s.editor.buffer, "echo draft");
        assert!(s.status.contains("Ctrl-Z"));
    }

    #[test]
    fn sigint_recently_forwarded_pauses_queue_on_command_end() {
        let mut state = make_state();
        state.queue.push("echo a", false);
        state.last_sigint_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_command_end(&mut state, Some(130), &mut w);
        assert!(state.queue.paused);
        assert_eq!(state.queue.len(), 1, "queued items kept, just paused");
        assert!(buf.lock().unwrap().is_empty(), "no command dispatched");
        assert!(state.status.contains("Ctrl-C"));
    }

    #[test]
    fn exit_130_pauses_even_without_recent_sigint() {
        let mut state = make_state();
        state.queue.push("echo a", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_command_end(&mut state, Some(130), &mut w);
        assert!(state.queue.paused);
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn normal_exit_dispatches_next() {
        let mut state = make_state();
        state.queue.push("echo a", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        assert!(handle_command_end(&mut state, Some(0), &mut w));
        assert!(!state.queue.paused);
        assert!(state.queue.is_empty(), "front item dispatched");
        assert!(matches!(state.shell_state, ShellState::Running));
        assert!(state.command_started_at.is_some());
        let written = buf.lock().unwrap();
        let s = String::from_utf8_lossy(&written);
        assert!(s.contains("echo a"));
    }

    #[test]
    fn concurrent_command_end_only_one_session_claims_item() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut base = Queue::new();
        base.push("echo once", false);
        base.save(&queue_path).unwrap();

        let mut first = make_state();
        first.queue_path = Some(queue_path.clone());
        first.queue = Queue::load_or_default(&queue_path);
        first.queue_known_items = first.queue.item_snapshot();
        first.queue_known_paused = first.queue.paused;

        let mut second = make_state();
        second.queue_path = Some(queue_path.clone());
        second.queue = Queue::load_or_default(&queue_path);
        second.queue_known_items = second.queue.item_snapshot();
        second.queue_known_paused = second.queue.paused;

        let first_buf = Arc::new(Mutex::new(Vec::new()));
        let second_buf = Arc::new(Mutex::new(Vec::new()));
        let mut first_writer: Box<dyn Write + Send> = Box::new(VecWriter(first_buf.clone()));
        let mut second_writer: Box<dyn Write + Send> = Box::new(VecWriter(second_buf.clone()));

        assert!(handle_command_end(&mut first, Some(0), &mut first_writer));
        assert!(!handle_command_end(
            &mut second,
            Some(0),
            &mut second_writer
        ));

        assert_eq!(&*first_buf.lock().unwrap(), b"echo once\n");
        assert!(second_buf.lock().unwrap().is_empty());
        assert!(Queue::load_or_default(&queue_path).is_empty());
        assert!(second.status.contains("another session"));
    }

    #[test]
    fn command_end_does_not_dispatch_item_cleared_by_another_session() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut base = Queue::new();
        base.push("echo stale", false);
        base.save(&queue_path).unwrap();

        let mut state = make_state();
        state.queue_path = Some(queue_path.clone());
        state.queue = Queue::load_or_default(&queue_path);
        state.queue_known_items = state.queue.item_snapshot();
        state.queue_known_paused = state.queue.paused;

        let mut external = Queue::load_or_default(&queue_path);
        external.clear();
        external.save(&queue_path).unwrap();

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        assert!(!handle_command_end(&mut state, Some(0), &mut w));

        assert!(buf.lock().unwrap().is_empty());
        assert!(state.queue.is_empty());
        assert!(state.status.contains("another session"));
    }

    #[test]
    fn dispatch_failure_rolls_claim_back_to_disk_paused() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut base = Queue::new();
        base.push("echo keep-me", false);
        base.save(&queue_path).unwrap();

        let mut state = make_state();
        state.queue_path = Some(queue_path.clone());
        state.queue = Queue::load_or_default(&queue_path);
        state.queue_known_items = state.queue.item_snapshot();
        state.queue_known_paused = state.queue.paused;
        let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

        assert!(!handle_command_end(&mut state, Some(0), &mut w));

        let loaded = Queue::load_or_default(&queue_path);
        assert!(loaded.paused);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "echo keep-me");
        assert!(state.status.contains("dispatch failed"));
    }

    #[test]
    fn command_end_pauses_queue_while_editing() {
        let mut state = make_state();
        state.queue.push("echo old", false);
        state.queue.push("echo next", false);
        state.editor.load_for_edit(0, "echo edited", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        assert!(!handle_command_end(&mut state, Some(0), &mut w));

        assert!(state.queue.paused);
        assert!(state.queue_dirty);
        assert_eq!(state.queue.len(), 2);
        assert_eq!(state.queue.items()[0].command, "echo old");
        assert_eq!(state.editor.editing_index, Some(0));
        assert!(buf.lock().unwrap().is_empty());
        assert!(state.status.contains("paused while editing"));
    }

    #[test]
    fn command_end_pauses_before_dispatch_when_shell_cwd_changed() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.shell_cwd = Some(PathBuf::from("/tmp/current"));
        state.queue.set_origin_cwd("/tmp/original");
        state.queue.push("echo keep-me", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        assert!(!handle_command_end(&mut state, Some(0), &mut w));

        assert!(state.queue.paused);
        assert!(state.queue_dirty);
        assert!(state.status.contains("Ctrl-X to run here"));
        assert!(state.resume_cwd_confirmation_started_at.is_some());
        assert!(buf.lock().unwrap().is_empty());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(!state.queue.paused);
        assert!(state.queue.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
    }

    #[test]
    fn failed_command_skips_foreign_conditional_before_cwd_warning() {
        let mut state = make_state();
        state.shell_cwd = Some(PathBuf::from("/tmp/current"));
        state
            .queue
            .push_with_origin("echo should-skip", true, Some(PathBuf::from("/tmp/other")));
        state.queue.push_with_origin(
            "echo should-run",
            false,
            Some(PathBuf::from("/tmp/current")),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        assert!(handle_command_end(&mut state, Some(1), &mut w));

        assert!(!state.queue.paused);
        assert!(state.queue.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"echo should-run\n");
        assert!(
            !state.status.contains("/tmp/other"),
            "skipped chained item should not trigger cwd warning"
        );
    }

    #[test]
    fn dispatch_failure_keeps_command_and_pauses_queue() {
        let mut state = make_state();
        state.queue.push("echo keep-me", false);
        let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

        assert!(!dispatch_next_eligible(&mut state, Some(0), &mut w));

        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue.front().unwrap().command, "echo keep-me");
        assert!(state.queue.paused);
        assert!(state.queue_dirty);
        assert!(state.status.contains("dispatch failed"));
    }

    #[test]
    fn dispatch_failure_after_conditional_skip_preserves_next_command() {
        let mut state = make_state();
        state.queue.push("echo skipped", true);
        state.queue.push("echo keep-me", false);
        let mut w: Box<dyn Write + Send> = Box::new(FailingWriter);

        assert!(!dispatch_next_eligible(&mut state, Some(1), &mut w));

        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue.front().unwrap().command, "echo keep-me");
        assert!(state.queue.paused);
        assert!(state.queue_dirty);
    }

    #[test]
    fn dispatch_persists_removed_item_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut state = make_state();
        state.queue.push("echo once", false);
        state.queue.save(&queue_path).unwrap();
        state.queue_known_items = state.queue.item_snapshot();
        state.queue_path = Some(queue_path.clone());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        assert!(dispatch_next_eligible(&mut state, Some(0), &mut w));

        assert_eq!(&*buf.lock().unwrap(), b"echo once\n");
        assert!(state.queue.is_empty());
        assert!(!state.queue_dirty);
        assert!(Queue::load_or_default(&queue_path).is_empty());
    }

    #[test]
    fn resume_paused_queue_at_prompt_dispatches_immediately() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.queue.paused = true;
        state.force_queue = true;
        state.queue.push("echo resumed", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );

        assert!(!state.queue.paused);
        assert!(state.queue.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"echo resumed\n");
        assert!(state.queue_dirty);
    }

    #[test]
    fn resume_paused_queue_at_prompt_waits_for_edit_to_finish() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.queue.paused = true;
        state.force_queue = true;
        state.queue.push("echo old", false);
        state.editor.load_for_edit(0, "echo edited", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );

        assert!(state.queue.paused);
        assert_eq!(state.queue.len(), 1);
        assert!(buf.lock().unwrap().is_empty());
        assert!(state.status.contains("finish or cancel"));
    }

    #[test]
    fn resume_paused_queue_at_prompt_does_not_fake_success_for_conditional() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.queue.paused = true;
        state.force_queue = true;
        state.queue.push("echo should-skip", true);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );

        assert!(!state.queue.paused);
        assert!(
            state.queue.is_empty(),
            "unknown previous exit skips conditional"
        );
        assert!(buf.lock().unwrap().is_empty());
        assert!(state.status.contains("skipped chained item"));
        assert!(state.queue_dirty);
    }

    #[test]
    fn old_sigint_does_not_pause() {
        let mut state = make_state();
        state.queue.push("echo a", false);
        state.last_sigint_at = Some(Instant::now() - Duration::from_secs(10));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_command_end(&mut state, Some(0), &mut w);
        assert!(!state.queue.paused);
        assert!(!buf.lock().unwrap().is_empty(), "stale sigint ignored");
    }

    #[test]
    fn empty_queue_with_sigint_does_not_pause() {
        let mut state = make_state();
        state.last_sigint_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_command_end(&mut state, Some(130), &mut w);
        assert!(
            !state.queue.paused,
            "no queued items → no need to auto-pause"
        );
    }

    #[test]
    fn effective_passthrough_combines_manual_and_auto() {
        let mut s = make_state();
        assert!(!s.effective_passthrough());
        s.manual_passthrough = true;
        assert!(s.effective_passthrough());
        s.manual_passthrough = false;
        s.auto_passthrough = true;
        assert!(s.effective_passthrough());
    }

    #[test]
    fn command_start_clears_stale_prompt_bracketed_paste_mode() {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.prompt_buffer = "echo hi".to_string();
        s.prompt_cursor = s.prompt_buffer.len();
        s.child_bracketed_paste = true;
        s.child_input_active = true;

        mark_command_started(&mut s);

        assert!(matches!(s.shell_state, ShellState::Running));
        assert!(s.command_started_at.is_some());
        assert!(s.prompt_buffer.is_empty());
        assert!(!s.prompt_continuation_active);
        assert!(!s.child_input_active);
        assert!(
            !s.child_bracketed_paste,
            "paste mode requested by the shell prompt must not leak into the child command"
        );
    }

    #[test]
    fn command_end_clears_child_bracketed_paste_mode() {
        let mut s = make_state();
        s.child_bracketed_paste = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf));

        handle_command_end(&mut s, Some(0), &mut w);

        assert!(
            !s.child_bracketed_paste,
            "paste mode requested by a child command must not leak back to the shell prompt"
        );
    }

    #[test]
    fn command_end_clears_manual_passthrough_so_prompt_controls_return() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.manual_passthrough = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_command_end(&mut s, Some(0), &mut w);
        s.shell_state = ShellState::AtPrompt;
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.manual_passthrough);
        assert!(s.force_queue, "Ctrl-Q should work again at the next prompt");
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn quick_command_does_not_show_panel() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now());
        assert!(!s.panel_should_be_visible());
    }

    #[test]
    fn first_typed_key_during_delay_opens_queue_instead_of_forwarding() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now());
        assert!(!s.editor_owns_input());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(s.editor_owns_input(), "queue editor should take focus");
        assert_eq!(s.editor.buffer, "e");
        assert!(
            buf.lock().unwrap().is_empty(),
            "first queue key must not leak to running command"
        );
    }

    #[test]
    fn enter_at_prompt_optimistically_starts_command_for_fast_followup_typing() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        for c in "sleep 5".chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        assert!(matches!(s.shell_state, ShellState::Running));
        assert!(!s.editor_owns_input(), "delay should still suppress flash");

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(s.editor.buffer, "e");
        assert_eq!(&*buf.lock().unwrap(), b"sleep 5\r");
    }

    #[test]
    fn prompt_tracker_handles_midline_readline_edits() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;

        for c in "echo wrld".chars() {
            assert!(
                update_prompt_buffer_for_forwarded_key(
                    &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                    &mut s,
                )
                .is_none()
            );
        }
        for _ in 0..3 {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                &mut s,
            );
        }
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            &mut s,
        );
        let submitted = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
        );

        assert_eq!(submitted.as_deref(), Some("echo world"));
    }

    #[test]
    fn prompt_tracker_handles_ctrl_w_word_kill() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;

        for c in "sleep 5".chars() {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
            );
        }
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &mut s,
        );
        for c in "10".chars() {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
            );
        }
        let submitted = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
        );

        assert_eq!(submitted.as_deref(), Some("sleep 10"));
    }

    #[test]
    fn prompt_tracker_handles_alt_word_movement() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;

        for c in "echo one three".chars() {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
            );
        }
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            &mut s,
        );
        for c in "two ".chars() {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
            );
        }
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            &mut s,
        );
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
            &mut s,
        );
        let submitted = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
        );

        assert_eq!(submitted.as_deref(), Some("echo one two three!"));
    }

    #[test]
    fn prompt_tracker_handles_alt_arrow_word_movement() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;

        for c in "echo alph beta".chars() {
            let _ = update_prompt_buffer_for_forwarded_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
            );
        }
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            &mut s,
        );
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut s,
        );
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut s,
        );
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            &mut s,
        );
        let _ = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
            &mut s,
        );
        let submitted = update_prompt_buffer_for_forwarded_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
        );

        assert_eq!(submitted.as_deref(), Some("echo alpha beta!"));
    }

    #[test]
    fn history_navigation_invalidates_prompt_tracker_for_fast_followup_capture() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        for c in "echo stale".chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(matches!(s.shell_state, ShellState::AtPrompt));
        assert_eq!(&*buf.lock().unwrap(), b"echo stale\x1b[A\r");
    }

    #[test]
    fn heredoc_prompt_line_does_not_trigger_fast_followup_capture() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        for c in "cat <<EOF".chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        assert!(matches!(s.shell_state, ShellState::AtPrompt));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(s.editor.buffer.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"cat <<EOF\rh");
    }

    #[test]
    fn multiline_if_continuation_lines_do_not_trigger_fast_followup_capture() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        for line in ["if true", "then", "echo body", "fi"] {
            for c in line.chars() {
                let _ = handle_key(
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                    &mut s,
                    &mut w,
                );
            }
            let _ = handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut s,
                &mut w,
            );
            assert!(
                s.editor.buffer.is_empty(),
                "{line:?} leaked into queue editor"
            );
        }

        assert!(matches!(s.shell_state, ShellState::AtPrompt));
        assert_eq!(&*buf.lock().unwrap(), b"if true\rthen\recho body\rfi\r");
    }

    #[test]
    fn incomplete_shell_lines_are_not_optimistically_marked_running() {
        assert!(!is_likely_complete_shell_command("echo 'unterminated"));
        assert!(!is_likely_complete_shell_command("for f in *; do"));
        assert!(!is_likely_complete_shell_command("for f in"));
        assert!(!is_likely_complete_shell_command("if true"));
        assert!(!is_likely_complete_shell_command("if true; then"));
        assert!(!is_likely_complete_shell_command("while true"));
        assert!(!is_likely_complete_shell_command("case \"$x\" in"));
        assert!(!is_likely_complete_shell_command("foo() {"));
        assert!(!is_likely_complete_shell_command("echo one |"));
        assert!(!is_likely_complete_shell_command("echo $(printf hi"));
        assert!(!is_likely_complete_shell_command("echo $((1 +"));
        assert!(!is_likely_complete_shell_command("(echo hi"));
        assert!(is_likely_complete_shell_command("sleep 5"));
        assert!(is_likely_complete_shell_command("printf 'ok\\n'"));
        assert!(is_likely_complete_shell_command("echo $(printf hi)"));
        assert!(is_likely_complete_shell_command("echo $((1 + 2))"));
        assert!(is_likely_complete_shell_command("(echo hi)"));
        assert!(is_likely_complete_shell_command(
            "if true; then echo ok; fi"
        ));
        assert!(is_likely_complete_shell_command(
            "for f in *; do echo \"$f\"; done"
        ));
    }

    #[test]
    fn ctrl_c_during_delay_still_forwards_sigint() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.editor_owns_input());
        assert_eq!(&*buf.lock().unwrap(), &[ETX]);
        assert!(s.last_sigint_at.is_some());
    }

    #[test]
    fn help_ctrl_c_still_interrupts_running_child() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.show_help = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.show_help);
        assert_eq!(&*buf.lock().unwrap(), &[ETX]);
        assert!(s.last_sigint_at.is_some());
    }

    #[test]
    fn help_ctrl_c_at_prompt_only_dismisses_help() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.show_help = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.show_help);
        assert!(buf.lock().unwrap().is_empty());
        assert!(s.last_sigint_at.is_none());
    }

    #[test]
    fn paste_dismisses_help_then_queues_normally() {
        let mut s = make_state();
        s.force_queue = true;
        s.show_help = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("echo after-help".to_string(), &mut s, &mut w);

        assert!(!s.show_help);
        assert_eq!(s.editor.buffer, "echo after-help");
    }

    #[test]
    fn paste_cancels_pending_quit_confirmation() {
        let mut s = make_state();
        s.force_queue = true;
        s.pending_quit_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("echo not-discarding".to_string(), &mut s, &mut w);

        assert!(s.pending_quit_at.is_none());
        assert_eq!(s.editor.buffer, "echo not-discarding");
    }

    #[test]
    fn paste_at_prompt_updates_tracker_for_fast_followup_capture() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("sleep 5".to_string(), &mut s, &mut w);
        assert_eq!(s.prompt_buffer, "sleep 5");
        assert_eq!(&*buf.lock().unwrap(), b"sleep 5");

        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        assert!(matches!(s.shell_state, ShellState::Running));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"sleep 5\r");
        assert_eq!(s.editor.buffer, "e");
    }

    #[test]
    fn newline_paste_at_prompt_optimistically_starts_command_for_fast_followup() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("sleep 5\n".to_string(), &mut s, &mut w);
        assert!(matches!(s.shell_state, ShellState::Running));
        assert!(s.command_started_at.is_some());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"sleep 5\n");
        assert_eq!(s.editor.buffer, "e");
    }

    #[test]
    fn leading_blank_paste_line_still_detects_later_submitted_command() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("\nsleep 5\n".to_string(), &mut s, &mut w);
        assert!(matches!(s.shell_state, ShellState::Running));
        assert!(s.command_started_at.is_some());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"\nsleep 5\n");
        assert_eq!(s.editor.buffer, "e");
    }

    #[test]
    fn leading_comment_paste_line_still_detects_later_submitted_command() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("# setup\nsleep 5\n".to_string(), &mut s, &mut w);
        assert!(matches!(s.shell_state, ShellState::Running));
        assert!(s.command_started_at.is_some());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), b"# setup\nsleep 5\n");
        assert_eq!(s.editor.buffer, "e");
    }

    #[test]
    fn leading_noop_paste_before_heredoc_keeps_continuation_passthrough() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("\n# setup\ncat <<EOF\n".to_string(), &mut s, &mut w);
        assert!(matches!(s.shell_state, ShellState::AtPrompt));
        assert!(s.prompt_continuation_active);

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(s.editor.buffer.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"\n# setup\ncat <<EOF\nh");
    }

    #[test]
    fn incomplete_newline_paste_at_prompt_does_not_capture_continuation() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("cat <<EOF\n".to_string(), &mut s, &mut w);
        assert!(matches!(s.shell_state, ShellState::AtPrompt));
        assert!(s.prompt_continuation_active);

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(s.editor.buffer.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"cat <<EOF\nh");
    }

    #[test]
    fn paste_during_delay_opens_queue_instead_of_forwarding() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("echo pasted\n".to_string(), &mut s, &mut w);

        assert!(s.editor_owns_input(), "paste should reveal queue editor");
        assert_eq!(s.editor.buffer, "echo pasted");
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn long_running_command_shows_panel() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        assert!(s.panel_should_be_visible());
    }

    #[test]
    fn force_queue_bypasses_delay() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now()); // just started
        s.force_queue = true;
        assert!(s.panel_should_be_visible());
    }

    #[test]
    fn tiny_terminal_disables_panel_capture() {
        let mut s = make_state();
        s.force_queue = true;
        s.terminal_allows_panel = false;
        assert!(!s.panel_should_be_visible());
        assert!(!s.editor_owns_input());
    }

    #[test]
    fn ctrl_q_does_not_arm_invisible_force_queue_on_tiny_terminal() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.terminal_allows_panel = false;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.force_queue);
        assert!(s.status.contains("too small"));
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn tiny_terminal_ctrl_x_can_resume_paused_queue() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.terminal_allows_panel = false;
        s.shell_state = ShellState::AtPrompt;
        s.queue.push("echo tiny", false);
        s.queue.paused = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.queue.paused);
        assert!(s.queue.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"echo tiny\n");
    }

    #[test]
    fn tiny_terminal_ctrl_k_can_clear_paused_queue() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.terminal_allows_panel = false;
        s.queue.push("echo tiny", false);
        s.queue.paused = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(s.queue.is_empty());
        assert!(!s.queue.paused);
        assert!(s.status.contains("queue cleared"));
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_passthrough_hides_panel_even_when_force_queue() {
        // alt-screen always wins — vim/htop expect the whole screen.
        let mut s = make_state();
        s.force_queue = true;
        s.auto_passthrough = true;
        assert!(!s.panel_should_be_visible());
    }

    #[test]
    fn manual_passthrough_keeps_panel_but_releases_keys() {
        // raw input mode: panel still drawn, but keystrokes go to the PTY.
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        s.manual_passthrough = true;
        assert!(s.panel_should_be_visible());
        assert!(!s.editor_owns_input());
    }

    #[test]
    fn ctrl_backslash_exits_manual_passthrough() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        s.manual_passthrough = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(!s.manual_passthrough);
        assert!(
            buf.lock().unwrap().is_empty(),
            "exit chord should not leak to the child process"
        );
    }

    #[test]
    fn ctrl_backslash_reaches_running_child_when_panel_owns_input() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        assert!(s.editor_owns_input());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert_eq!(&*buf.lock().unwrap(), &[FS]);
        assert!(!s.manual_passthrough);
        assert!(s.status.contains("Ctrl-\\"));
    }

    #[test]
    fn raw_passthrough_sends_ctrl_q_to_child() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.force_queue = true;
        s.manual_passthrough = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(s.force_queue, "Ctrl-Q must not toggle cmdq while raw");
        assert_eq!(&*buf.lock().unwrap(), &[0x11]);
    }

    #[test]
    fn auto_passthrough_sends_f1_to_child() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.auto_passthrough = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );

        assert!(!s.show_help, "F1 belongs to the alt-screen app");
        assert_eq!(&*buf.lock().unwrap(), b"\x1bOP");
    }

    #[test]
    fn pending_quit_active_within_window() {
        let mut s = make_state();
        assert!(!s.pending_quit_active());
        s.pending_quit_at = Some(Instant::now());
        assert!(s.pending_quit_active());
        s.pending_quit_at = Some(Instant::now() - QUIT_CONFIRM_WINDOW - Duration::from_secs(1));
        assert!(!s.pending_quit_active());
    }

    #[test]
    fn ctrl_k_clear_resets_pause_and_edit_state() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        let id = s.queue.push("echo doomed", false);
        s.queue.paused = true;
        s.force_queue = true;
        s.editor.load_for_edit(0, "echo doomed", false);
        assert_eq!(s.queue.items()[0].id, id);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(s.queue.is_empty());
        assert!(!s.queue.paused);
        assert!(s.editor.editing_index.is_none());
        assert!(s.editor.buffer.is_empty());
    }

    #[test]
    fn truncate_for_status_long_command() {
        let long: String = "x".repeat(200);
        let t = truncate_for_status(&long);
        assert!(t.chars().count() <= 61);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_for_status_short_command() {
        assert_eq!(truncate_for_status("ls -la"), "ls -la");
    }

    #[test]
    fn command_end_clears_long_running_flag() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        assert!(s.command_long_running());
        s.command_started_at = None;
        assert!(!s.command_long_running());
    }

    fn esc_press() -> KeyEvent {
        use crossterm::event::KeyCode;
        let mut k = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        k.kind = KeyEventKind::Press;
        k
    }

    #[test]
    fn double_esc_toggles_passthrough() {
        let mut s = make_state();
        // Force queue mode so the editor has focus.
        s.force_queue = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        assert!(!s.manual_passthrough);
        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(!s.manual_passthrough, "single Esc must not toggle");
        assert!(s.last_esc_at.is_some());

        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(s.manual_passthrough, "second Esc within window toggles");
        assert!(
            s.last_esc_at.is_none(),
            "timestamp cleared so a 3rd quick Esc starts a fresh window"
        );

        let _ = handle_key(esc_press(), &mut s, &mut w);
        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(
            !s.manual_passthrough,
            "double-Esc again returns to queue mode"
        );
    }

    #[test]
    fn double_esc_at_plain_prompt_does_not_enter_invisible_passthrough() {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        let _ = handle_key(esc_press(), &mut s, &mut w);

        assert!(!s.manual_passthrough);
        assert_eq!(&*buf.lock().unwrap(), b"\x1b\x1b");
    }

    #[test]
    fn double_esc_passes_through_in_auto_passthrough() {
        let mut s = make_state();
        s.auto_passthrough = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        let _ = handle_key(esc_press(), &mut s, &mut w);

        assert!(!s.manual_passthrough);
        assert_eq!(&*buf.lock().unwrap(), b"\x1b\x1b");
    }

    #[test]
    fn esc_outside_window_does_not_toggle() {
        let mut s = make_state();
        s.force_queue = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        s.last_esc_at = Some(Instant::now() - ESC_DOUBLE_TAP_WINDOW - Duration::from_millis(50));
        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(
            !s.manual_passthrough,
            "second Esc outside window must not toggle"
        );
    }

    #[test]
    fn non_esc_key_disarms_double_esc_passthrough() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.force_queue = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        let _ = handle_key(esc_press(), &mut s, &mut w);

        assert!(
            !s.manual_passthrough,
            "Esc, another key, Esc should not count as double-Esc raw input"
        );
        assert!(
            s.editor.buffer.is_empty(),
            "final Esc should clear editor input"
        );
        assert!(
            buf.lock().unwrap().is_empty(),
            "editor-owned keys should not leak"
        );
    }

    #[test]
    fn esc_that_clears_input_does_not_arm_double_esc_passthrough() {
        let mut s = make_state();
        s.force_queue = true;
        s.editor.insert_str("draft");
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(s.editor.buffer.is_empty());
        assert!(s.last_esc_at.is_none());

        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(
            !s.manual_passthrough,
            "Esc after clearing input should start a fresh double-Esc window"
        );
        assert!(s.last_esc_at.is_some());
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn esc_that_cancels_edit_does_not_arm_double_esc_passthrough() {
        let mut s = make_state();
        s.force_queue = true;
        s.queue.push("echo queued", false);
        s.editor.load_for_edit(0, "echo queued", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(s.editor.editing_index.is_none());
        assert!(s.last_esc_at.is_none());

        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(
            !s.manual_passthrough,
            "Esc after canceling edit should start a fresh double-Esc window"
        );
        assert!(s.last_esc_at.is_some());
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_restores_non_empty_queue_paused_and_visible() {
        let mut q = Queue::new();
        q.push("echo keep-me", false);

        let (status, dirty, warning, local_pause) =
            prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/cmdq-test")), 0);

        assert!(q.paused);
        assert!(
            !dirty,
            "startup pause is session-local so a second window does not pause another live queue"
        );
        assert!(local_pause);
        assert!(status.unwrap().contains("restored 1 queued command"));
        assert!(warning.is_none());

        let mut s = make_state();
        s.queue = q;
        assert!(
            s.panel_should_be_visible(),
            "restored queues should be visible instead of silently waiting"
        );
    }

    #[test]
    fn startup_status_mentions_active_peer_session() {
        let mut q = Queue::new();
        q.push("echo keep-me", false);

        let (status, dirty, warning, local_pause) =
            prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/cmdq-test")), 1);

        assert!(q.paused);
        assert!(!dirty);
        assert!(local_pause);
        assert!(warning.is_none());
        assert!(status.unwrap().contains("another cmdq session is active"));
    }

    #[test]
    fn startup_local_pause_does_not_persist_over_live_unpaused_queue() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut live = Queue::new();
        live.push("echo live", false);
        live.save(&queue_path).unwrap();

        let mut restored = Queue::load_or_default(&queue_path);
        let known_paused = restored.paused;
        let (_status, dirty, _warning, local_pause) =
            prepare_queue_for_startup(&mut restored, Some(Path::new("/tmp/cmdq-test")), 0);

        let mut state = make_state();
        state.queue_path = Some(queue_path.clone());
        state.queue = restored;
        state.queue_known_items = state.queue.item_snapshot();
        state.queue_known_paused = known_paused;
        state.restored_queue_paused_locally = local_pause;
        state.queue_dirty = dirty;

        assert!(state.queue.paused);
        assert!(!state.queue_dirty);
        save_queue_if_dirty(&mut state, &queue_path);
        assert!(
            !Queue::load_or_default(&queue_path).paused,
            "opening a second cmdq should not persist a pause over an active unpaused queue"
        );

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut state, &queue_path, &mut last_sync);
        assert!(
            state.queue.paused,
            "the restored session still keeps its own safety pause"
        );
        assert_eq!(state.queue_known_paused, known_paused);

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut state, &queue_path, &mut last_sync);
        assert!(
            state.queue.paused,
            "repeated syncs should not turn a session-local restore pause into a resume"
        );
    }

    #[test]
    fn startup_restored_queue_from_different_cwd_requires_resume_confirmation() {
        use crossterm::event::KeyCode;

        let mut q = Queue::new();
        q.set_origin_cwd("/tmp/original");
        q.push("echo keep-me", false);

        let (status, dirty, warning, local_pause) =
            prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

        assert!(q.paused);
        assert!(!dirty);
        assert!(local_pause);
        assert!(status.unwrap().contains("from /tmp/original"));
        assert!(warning.unwrap().contains("press Ctrl-X again"));

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.shell_cwd = Some(PathBuf::from("/tmp/current"));
        state.session_cwd = Some(PathBuf::from("/tmp/current"));
        state.queue = q;
        state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(state.queue.paused);
        assert!(buf.lock().unwrap().is_empty());
        assert!(state.status.contains("/tmp/original"));
        assert!(state.status.contains("/tmp/current"));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(!state.queue.paused);
        assert!(state.queue.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
    }

    #[test]
    fn startup_restored_mixed_cwd_queue_requires_resume_confirmation() {
        let mut q = Queue::new();
        q.push_with_origin("echo local", false, Some(PathBuf::from("/tmp/current")));
        q.push_with_origin("echo remote", false, Some(PathBuf::from("/tmp/remote")));

        let (status, dirty, warning, local_pause) =
            prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

        assert!(q.paused);
        assert!(!dirty);
        assert!(local_pause);
        assert!(status.unwrap().contains("/tmp/remote"));
        assert!(warning.unwrap().contains("/tmp/remote"));
    }

    #[test]
    fn startup_restored_multiple_foreign_origins_mentions_other_dirs() {
        let mut q = Queue::new();
        q.push_with_origin("echo one", false, Some(PathBuf::from("/tmp/one")));
        q.push_with_origin("echo two", false, Some(PathBuf::from("/tmp/two")));

        let (status, _, warning, _) =
            prepare_queue_for_startup(&mut q, Some(Path::new("/tmp/current")), 0);

        assert!(status.unwrap().contains("1 other dirs"));
        assert!(warning.unwrap().contains("1 other dirs"));
    }

    #[test]
    fn resume_cwd_confirmation_cancels_on_other_key() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.shell_cwd = Some(PathBuf::from("/tmp/current"));
        state.queue.paused = true;
        state.queue.set_origin_cwd("/tmp/original");
        state.queue.push("echo keep-me", false);
        state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(state.queue.paused);
        assert!(state.resume_cwd_confirmation_started_at.is_some());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut state,
            &mut w,
        );
        assert!(state.resume_cwd_confirmation_started_at.is_none());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(state.queue.paused);
        assert!(
            state.resume_cwd_confirmation_started_at.is_some(),
            "Ctrl-X after another key should warn again, not resume"
        );
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn resume_cwd_confirmation_expires() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.shell_cwd = Some(PathBuf::from("/tmp/current"));
        state.queue.paused = true;
        state.queue.set_origin_cwd("/tmp/original");
        state.queue.push("echo keep-me", false);
        state.resume_cwd_warning = Some("cwd mismatch; press Ctrl-X again".to_string());
        state.resume_cwd_confirmation_started_at =
            Some(Instant::now() - RESUME_CWD_CONFIRM_WINDOW - Duration::from_millis(1));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );

        assert!(state.queue.paused);
        assert!(state.resume_cwd_confirmation_started_at.is_some());
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn resume_rechecks_latest_shell_cwd_before_dispatch() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.session_cwd = Some(PathBuf::from("/tmp/original"));
        state.shell_cwd = Some(PathBuf::from("/tmp/after-rc"));
        state.queue.paused = true;
        state.queue.set_origin_cwd("/tmp/original");
        state.queue.push("echo keep-me", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(state.queue.paused);
        assert!(state.status.contains("/tmp/original"));
        assert!(state.status.contains("/tmp/after-rc"));
        assert!(buf.lock().unwrap().is_empty());

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );
        assert!(!state.queue.paused);
        assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
    }

    #[test]
    fn stale_resume_cwd_warning_clears_when_shell_returns_to_origin() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.shell_state = ShellState::AtPrompt;
        state.session_cwd = Some(PathBuf::from("/tmp/current"));
        state.shell_cwd = Some(PathBuf::from("/tmp/original"));
        state.queue.paused = true;
        state.queue.set_origin_cwd("/tmp/original");
        state.queue.push("echo keep-me", false);
        state.resume_cwd_warning = Some(
            "queue was saved in /tmp/original; press Ctrl-X again to run here (/tmp/current)"
                .to_string(),
        );
        state.resume_cwd_confirmation_started_at = Some(Instant::now());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &mut state,
            &mut w,
        );

        assert!(!state.queue.paused);
        assert!(state.resume_cwd_warning.is_none());
        assert!(state.resume_cwd_confirmation_started_at.is_none());
        assert_eq!(&*buf.lock().unwrap(), b"echo keep-me\n");
    }

    #[test]
    fn queued_item_origin_uses_latest_shell_cwd() {
        use crossterm::event::KeyCode;

        let mut state = make_state();
        state.force_queue = true;
        state.shell_cwd = Some(PathBuf::from("/tmp/inner-shell"));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        for c in "echo here".chars() {
            let _ = handle_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut state,
                &mut w,
            );
        }
        let _ = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &mut w,
        );

        assert_eq!(
            state.queue.origin_cwd(),
            Some(Path::new("/tmp/inner-shell"))
        );
        assert_eq!(state.queue.front().unwrap().command, "echo here");
        assert_eq!(
            state.queue.front().unwrap().origin_cwd.as_deref(),
            Some(Path::new("/tmp/inner-shell"))
        );
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn queue_save_failure_keeps_dirty_and_surfaces_status() {
        let temp = tempfile::tempdir().unwrap();
        let not_a_dir = temp.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"file").unwrap();
        let queue_path = not_a_dir.join("queue.json");

        let mut s = make_state();
        s.queue.push("echo persist-me", false);
        s.queue_dirty = true;

        save_queue_if_dirty(&mut s, &queue_path);

        assert!(s.queue_dirty);
        assert!(s.status.contains("queue save failed"));
        assert!(s.queue_save_error.is_some());
    }

    #[test]
    fn queue_save_merges_unseen_external_items() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut external = Queue::new();
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo local", false);
        s.queue_dirty = true;

        save_queue_if_dirty(&mut s, &queue_path);

        let loaded = Queue::load_or_default(&queue_path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert!(
            s.queue_dirty,
            "external merge should schedule a follow-up save for the paused state"
        );
        assert!(s.status.contains("merged 1 queued item"));
        assert!(s.status.contains("queue paused"));
        assert_eq!(loaded.len(), 2);
        assert!(commands.contains(&"echo external"));
        assert!(commands.contains(&"echo local"));

        save_queue_if_dirty(&mut s, &queue_path);
        assert!(!s.queue_dirty);
    }

    #[test]
    fn queue_save_reports_corrupt_disk_backup() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        std::fs::write(&queue_path, b"{broken queue").unwrap();

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo local", false);
        s.queue_dirty = true;

        save_queue_if_dirty(&mut s, &queue_path);

        assert!(!s.queue_dirty);
        assert!(s.status.contains("ignored corrupt queue file"));
        assert_eq!(
            Queue::load_or_default(&queue_path).front().unwrap().command,
            "echo local"
        );
    }

    #[test]
    fn queue_save_pauses_after_merging_unseen_external_items() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut external = Queue::new();
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo local", false);
        s.queue_dirty = true;

        save_queue_if_dirty(&mut s, &queue_path);

        assert!(s.queue.paused);
        assert!(
            s.queue_dirty,
            "pause state should be persisted on the next save"
        );
        assert!(s.status.contains("queue paused"));

        save_queue_if_dirty(&mut s, &queue_path);
        assert!(!s.queue_dirty);
        assert!(Queue::load_or_default(&queue_path).paused);
    }

    #[test]
    fn saved_edit_is_not_lost_when_another_session_deletes_item() {
        use crossterm::event::KeyCode;

        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let mut base = Queue::new();
        let shared_id = base.push("echo original", false);
        base.save(&queue_path).unwrap();

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue = Queue::load_or_default(&queue_path);
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;
        s.editor
            .load_for_edit(0, s.queue.items()[0].command.as_str(), false);

        let mut external = Queue::load_or_default(&queue_path);
        assert!(external.remove(shared_id).is_some());
        external.save(&queue_path).unwrap();

        s.editor.buffer = "echo local edit".to_string();
        s.editor.cursor = s.editor.buffer.len();
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf));

        let outcome = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut s,
            &mut w,
        );
        assert!(matches!(outcome, KeyOutcome::Continue));
        save_queue_if_dirty(&mut s, &queue_path);

        let loaded = Queue::load_or_default(&queue_path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "echo local edit");
        assert!(s.status.contains("kept local edits"));
    }

    #[test]
    fn idle_queue_sync_merges_external_items_and_pauses() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo local", false);
        s.queue.save(&queue_path).unwrap();
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;

        let mut external = Queue::load_or_default(&queue_path);
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        let commands: Vec<_> = s
            .queue
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(commands, vec!["echo local", "echo external"]);
        assert!(s.queue.paused);
        assert!(s.queue_dirty, "auto-pause should be persisted");
        assert!(s.status.contains("merged 1 queued item"));
        assert!(s.status.contains("queue paused"));

        save_queue_if_dirty(&mut s, &queue_path);
        assert!(!s.queue_dirty);
        assert!(Queue::load_or_default(&queue_path).paused);
    }

    #[test]
    fn idle_queue_sync_reports_external_change_during_draft_without_clobbering_input() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.force_queue = true;
        s.queue.push("echo local", false);
        s.queue.save(&queue_path).unwrap();
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;
        s.editor.insert_str("echo draft");

        let mut external = Queue::load_or_default(&queue_path);
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        assert_eq!(s.editor.buffer, "echo draft");
        assert_eq!(s.queue.len(), 1);
        assert!(!s.queue_dirty);
        assert!(s.queue_external_change_pending);
        assert!(s.status.contains("finish or cancel edit to merge"));

        s.editor.reset();
        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        let commands: Vec<_> = s
            .queue
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(commands, vec!["echo local", "echo external"]);
        assert!(!s.queue_external_change_pending);
    }

    #[test]
    fn deferred_external_change_warning_persists_until_edit_can_merge() {
        let mut s = make_state();
        s.queue_external_change_pending = true;
        s.set_status(deferred_queue_change_status());
        s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));

        s.tick_status();

        assert_eq!(s.status, deferred_queue_change_status());
        assert!(s.status_set_at.is_some());

        s.set_status("temporary note");
        s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
        s.tick_status();

        assert_eq!(s.status, deferred_queue_change_status());

        s.queue_external_change_pending = false;
        s.status_set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
        s.tick_status();

        assert!(s.status.is_empty());
        assert!(s.status_set_at.is_none());
    }

    #[test]
    fn deferred_external_change_warning_clears_when_disk_returns_to_known_state() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.force_queue = true;
        s.queue.push("echo local", false);
        s.queue.save(&queue_path).unwrap();
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;
        s.editor.insert_str("echo draft");

        let mut external = Queue::load_or_default(&queue_path);
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);
        assert!(s.queue_external_change_pending);
        assert_eq!(s.status, deferred_queue_change_status());

        s.queue.save(&queue_path).unwrap();
        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        assert!(!s.queue_external_change_pending);
        assert!(s.status.is_empty());
        assert!(s.status_set_at.is_none());
        assert_eq!(s.editor.buffer, "echo draft");
    }

    #[test]
    fn idle_queue_sync_reports_external_change_during_item_edit_without_clobbering_edit() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo local", false);
        s.queue.save(&queue_path).unwrap();
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;
        s.editor.load_for_edit(0, "echo local changed", false);

        let mut external = Queue::load_or_default(&queue_path);
        external.push("echo external", false);
        external.save(&queue_path).unwrap();

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        assert_eq!(s.editor.buffer, "echo local changed");
        assert_eq!(s.editor.editing_index, Some(0));
        assert_eq!(s.queue.len(), 1);
        assert!(s.queue_external_change_pending);
        assert!(s.status.contains("finish or cancel edit to merge"));
    }

    #[test]
    fn idle_queue_sync_adopts_external_clear() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let mut s = make_state();
        s.queue_path = Some(queue_path.clone());
        s.queue.push("echo clear me", false);
        s.queue.save(&queue_path).unwrap();
        s.queue_known_items = s.queue.item_snapshot();
        s.queue_known_paused = s.queue.paused;
        std::fs::remove_file(&queue_path).unwrap();

        let mut last_sync = Instant::now() - QUEUE_SYNC_INTERVAL - Duration::from_millis(1);
        sync_queue_from_disk_if_due(&mut s, &queue_path, &mut last_sync);

        assert!(s.queue.is_empty());
        assert!(!s.queue_dirty);
        assert_eq!(s.status, "queue cleared by another session");
        assert!(s.queue_known_items.is_empty());
    }

    #[test]
    fn confirmed_ctrl_d_discards_queue_before_quit() {
        use crossterm::event::KeyCode;

        let mut s = make_state();
        s.force_queue = true;
        s.queue.push("echo discard", false);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let first = handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );
        assert!(matches!(first, KeyOutcome::Continue));
        assert_eq!(s.queue.len(), 1);

        let second = handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut s,
            &mut w,
        );

        assert!(matches!(second, KeyOutcome::Quit));
        assert!(s.queue.is_empty());
        assert!(!s.queue.paused);
        assert!(s.queue_dirty);
    }

    #[test]
    fn paste_in_passthrough_is_plain_until_child_enables_bracketed_paste() {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        // Ensure editor doesn't own input → paste goes to PTY.
        assert!(!s.editor_owns_input());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_paste("hello".to_string(), &mut s, &mut w);
        assert_eq!(&*buf.lock().unwrap(), b"hello");
    }

    #[test]
    fn paste_in_passthrough_wraps_when_child_enabled_bracketed_paste() {
        let mut s = make_state();
        s.shell_state = ShellState::AtPrompt;
        s.child_bracketed_paste = true;
        assert!(!s.editor_owns_input());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_paste("hello".to_string(), &mut s, &mut w);
        let written = buf.lock().unwrap().clone();
        assert!(written.starts_with(b"\x1b[200~"));
        assert!(written.ends_with(b"\x1b[201~"));
        assert!(
            String::from_utf8_lossy(&written).contains("hello"),
            "payload preserved"
        );
    }

    #[test]
    fn paste_to_child_input_prompt_does_not_open_queue() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        s.running_output_tail.feed(b"name? ");
        s.child_input_active = true;
        assert!(s.panel_should_be_visible());
        assert!(!s.editor_owns_input());

        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_paste("alice\n".to_string(), &mut s, &mut w);

        assert!(s.editor.buffer.is_empty());
        assert_eq!(&*buf.lock().unwrap(), b"alice\n");
        assert!(!s.child_input_active);
    }

    #[test]
    fn paste_in_queue_mode_inserts_into_editor() {
        let mut s = make_state();
        s.force_queue = true;
        // editor owns input now (force_queue + not auto_passthrough +
        // not manual_passthrough)
        assert!(s.editor_owns_input());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));
        handle_paste("ls -la".to_string(), &mut s, &mut w);
        assert_eq!(s.editor.buffer, "ls -la");
        assert!(buf.lock().unwrap().is_empty(), "PTY must not see paste");
    }

    #[test]
    fn multiline_paste_in_queue_mode_preserves_block_shape() {
        let mut s = make_state();
        s.force_queue = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        handle_paste("cat <<'EOF'\r\nhello\nEOF\n".to_string(), &mut s, &mut w);

        assert_eq!(s.editor.buffer, "cat <<'EOF'\nhello\nEOF");
        assert!(buf.lock().unwrap().is_empty(), "PTY must not see paste");
    }
}
