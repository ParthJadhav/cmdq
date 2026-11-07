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

mod exit_cleanup;
mod key_encoding;
mod keys;
mod lifecycle;
mod output_tail;
mod prompt_mirror;
mod pty_output;
mod queue_actions;
mod queue_sync;
mod screen;
mod shell_syntax;
mod status;
#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        EnableBracketedPaste, Event as CtEvent, KeyEventKind, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::enable_raw_mode,
};

use crate::config::Config;
use crate::input::LineEditor;
use crate::panel::{self, PanelState, StatusKind};
use crate::pty::ShellPty;
use crate::queue::{self, Queue};
use crate::session_lease::SessionLease;
use crate::shell_integration::ShellKind;
use exit_cleanup::{SignalCleanup, TerminalGuard, install_signal_cleanup, prepare_signal_cleanup};
use keys::{KeyOutcome, flush_pending_escape_if_due, handle_key, handle_paste};
use lifecycle::{ShellInput, refresh_auto_passthrough_for_child_modes};
use output_tail::OutputTail;
use prompt_mirror::PromptMirror;
use pty_output::OutputSniffer;
use queue_sync::{
    prepare_queue_for_startup, resume_cwd_warning_for_current_shell, save_queue_if_dirty,
    sync_queue_from_disk_if_due,
};
use screen::{PanelLayout, Screen, shared_restore_state, terminal_allows_panel};
use status::{CLEAR_CONFIRM_WINDOW, QUIT_CONFIRM_WINDOW};

pub struct AppConfig {
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellState {
    Unknown,
    AtPrompt,
    Running,
}

/// How long to wait for the terminal's cursor-position reply at startup.
/// Every real terminal answers well within this; the fallback is the old
/// bottom-row assumption.
#[cfg(unix)]
const CURSOR_QUERY_TIMEOUT: Duration = Duration::from_millis(250);

/// Input poll timeout while a partial key sequence or a rate-limited repaint
/// is pending.
const POLL_INTERVAL: Duration = Duration::from_millis(16);

/// Poll timeout when nothing is pending. PTY output wakes the loop through a
/// pipe, so this only bounds the latency of time-driven transitions.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How often a running cmdq refreshes its lightweight "I'm alive" lease.
const SESSION_LEASE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Terminal modes the running program asked for. They end with the command.
#[derive(Debug, Clone, Copy, Default)]
struct ChildModes {
    alt_screen: bool,
    mouse_capture: bool,
    focus_events: bool,
    /// The program reads the tty directly (raw or no-echo termios).
    tty_input: bool,
    /// The program enabled bracketed paste (`CSI ? 2004 h`). cmdq keeps the
    /// outer terminal's paste reporting on for itself; this decides whether
    /// forwarded pastes are wrapped.
    bracketed_paste: bool,
}

struct AppState {
    config: Config,
    shell_kind: ShellKind,
    queue_supported: bool,
    /// Readline can write back a stale PTY size while preparing Bash's prompt.
    wait_for_bash_prompt: bool,
    pending_bash_exit: Option<Option<i32>>,
    queue: Queue,
    editor: LineEditor,
    shell_state: ShellState,
    manual_passthrough: bool,
    /// The running program owns the whole terminal (alt-screen, mouse
    /// capture, direct tty input), so the panel and queue step aside.
    auto_passthrough: bool,
    child: ChildModes,
    force_queue: bool,
    show_help: bool,
    terminal_allows_panel: bool,
    /// Last time we forwarded SIGINT (0x03) to the inner shell.
    last_sigint_at: Option<Instant>,
    command_started_at: Option<Instant>,
    command_tty_check_after: Option<Instant>,
    prompt: PromptMirror,
    /// Recent visible output from the child. If a non-alt-screen program
    /// prints an input prompt (`Password:`, `>>>`, `read -p`, etc.), cmdq
    /// leaves keystrokes with that program instead of silently queueing them.
    running_output_tail: OutputTail,
    /// Sticky form of `running_output_tail.looks_like_input_prompt()`. Once
    /// a child prompt is detected, stay in child-input mode while the user
    /// types the answer; echoed characters should not make us recapture the
    /// rest of the line.
    child_input_active: bool,
    pending_quit_at: Option<Instant>,
    pending_clear_at: Option<Instant>,
    /// Exit status of the most recently finished command; lets a chained
    /// draft submitted at the prompt honour the real previous result.
    last_exit_code: Option<i32>,
    last_esc_at: Option<Instant>,
    /// Persistent outcome, separate from transient confirmations.
    activity: Option<String>,
    /// Most recent removal batch, with original positions and working directories.
    recovery: Vec<(usize, queue::QueueItem)>,
    status: String,
    status_kind: StatusKind,
    status_set_at: Option<Instant>,
    queue_dirty: bool,
    /// Last queue-save error already surfaced to the user. This lets us keep
    /// retrying without repainting a fresh status every event-loop tick.
    queue_save_error: Option<String>,
    /// `None` keeps the queue in memory only.
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
    fn tty_input_requested(&self, pty: &ShellPty) -> bool {
        matches!(self.shell_state, ShellState::Running)
            && self
                .command_tty_check_after
                .is_some_and(|after| Instant::now() >= after)
            && pty.needs_direct_input()
    }

    fn effective_passthrough(&self) -> bool {
        !self.queue_supported || self.manual_passthrough || self.auto_passthrough
    }

    fn command_long_running(&self) -> bool {
        self.command_started_at
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(self.config.panel.delay_ms))
    }

    /// Something the user is doing with the queue needs the editor: a long
    /// command to type behind, a forced queue, help, a paused queue with
    /// items (so they can see *why* nothing runs and discover ^X), or an
    /// uncommitted draft/edit (so a command finishing mid-keystroke doesn't
    /// swallow it).
    fn wants_queue_input(&self) -> bool {
        self.command_long_running()
            || self.force_queue
            || self.show_help
            || (self.queue.paused && !self.queue.is_empty())
            || self.draft_pending()
    }

    /// Whether the panel should be reserved on screen (regardless of who
    /// owns keystrokes). A recent outcome (`activity`) also keeps it, but a
    /// fresh status message does not: that would steal keystrokes from the
    /// shell after every status update.
    fn panel_should_be_visible(&self) -> bool {
        self.queue_supported
            && !self.auto_passthrough
            && self.terminal_allows_panel
            && (self.wants_queue_input() || self.activity.is_some())
    }

    /// Whether cmdq's editor owns keystrokes (rather than forwarding to PTY).
    /// The panel can be visible while keystrokes still go to the shell — that's
    /// what `manual_passthrough` does (raw input mode).
    fn editor_owns_input(&self) -> bool {
        self.panel_should_be_visible()
            && self.wants_queue_input()
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
            .is_some_and(|t| t.elapsed() <= QUIT_CONFIRM_WINDOW)
    }

    fn pending_clear_active(&self) -> bool {
        self.pending_clear_at
            .is_some_and(|t| t.elapsed() <= CLEAR_CONFIRM_WINDOW)
    }

    /// The user has typed (or is editing) something in the queue input that
    /// has not been committed yet.
    fn draft_pending(&self) -> bool {
        self.editor.editing_index.is_some() || !self.editor.buffer.is_empty()
    }

    fn panel_view(&self) -> PanelState<'_> {
        let running = matches!(self.shell_state, ShellState::Running);
        PanelState {
            queue: &self.queue,
            running,
            force_queue: self.force_queue,
            passthrough_to_child: self.effective_passthrough() || self.child_input_prompt_active(),
            child_input_prompt: self.child_input_prompt_active(),
            input_buffer: &self.editor.buffer,
            input_cursor: self.editor.cursor,
            editing_index: self.editor.editing_index,
            conditional: self.editor.conditional,
            shell_input: !self.editor_owns_input() && !running,
            activity: self.activity.as_deref(),
            can_recover: !self.recovery.is_empty(),
            status: &self.status,
            status_kind: self.status_kind,
            pending_quit: self.pending_quit_active(),
            pending_clear: self.pending_clear_active(),
            show_help: self.show_help,
            max_queue_visible: self.config.panel.max_rows,
        }
    }

    fn desired_layout(&self, term_rows: u16) -> PanelLayout {
        if !self.panel_should_be_visible() {
            return PanelLayout::Hidden;
        }
        let height =
            panel::panel_height(&self.panel_view(), term_rows).min(term_rows.saturating_sub(2));
        if height == 0 {
            PanelLayout::Hidden
        } else {
            PanelLayout::Reserved { height }
        }
    }

    /// The running program's terminal modes end with it.
    fn reset_child_modes(&mut self) {
        self.child = ChildModes::default();
        self.auto_passthrough = false;
    }

    fn forget_resume_cwd_warning(&mut self) {
        self.resume_cwd_warning = None;
        self.resume_cwd_confirmation_started_at = None;
    }

    fn forget_resume_cwd_warning_if_empty(&mut self) {
        if self.queue.is_empty() {
            self.forget_resume_cwd_warning();
        }
    }

    /// Re-derive the resume warning after the queue or its pause changed
    /// behind the user's back; any confirmation in progress starts over.
    fn recheck_resume_cwd_warning(&mut self) {
        self.resume_cwd_warning = resume_cwd_warning_for_current_shell(self);
        self.resume_cwd_confirmation_started_at = None;
    }

    fn toggle_force_queue(&mut self) {
        if !self.terminal_allows_panel && !self.force_queue {
            self.set_status("terminal too small for queue panel");
            return;
        }
        self.force_queue = !self.force_queue;
        let msg = if self.force_queue {
            "Building queue — Ctrl-X starts it, Ctrl-Q returns to shell"
        } else {
            "Typing into shell"
        };
        self.set_status_as(StatusKind::Routine, msg);
    }

    fn activate_queue_for_running_command(&mut self) {
        if matches!(self.shell_state, ShellState::Running) {
            self.command_started_at =
                Some(Instant::now() - Duration::from_millis(self.config.panel.delay_ms));
        }
    }

    fn toggle_manual_passthrough(&mut self) {
        self.manual_passthrough = !self.manual_passthrough;
        let msg = if self.manual_passthrough {
            "Typing into running program — F2 returns to queue"
        } else {
            "Typing into queue"
        };
        self.set_status_as(StatusKind::Routine, msg);
    }
}

pub fn run_with_config_exit_status(cfg: AppConfig, config: Config) -> Result<u32> {
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "cmdq needs an interactive terminal on stdin and stdout; run cmdq directly in your terminal"
    );
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
        return Ok(0);
    }

    let shell = cfg
        .shell
        .clone()
        .or_else(crate::shell_integration::current_shell)
        .unwrap_or_else(|| "/bin/sh".into());
    let shell_kind = ShellKind::detect_from_path(&shell);
    let queue_supported = shell_kind != ShellKind::Sh;
    if !queue_supported {
        eprintln!(
            "cmdq: {shell} has no queue hooks; using terminal passthrough. For automatic queueing, use --shell zsh, --shell bash, or --shell fish."
        );
    }

    // Register handlers before creating any persistent session state. A
    // SIGTERM can arrive as soon as another process observes the lease; the
    // signal iterator queues it until the cleanup thread is ready.
    let pending_signals = prepare_signal_cleanup().context("prepare signal cleanup")?;

    let session_cwd = std::env::current_dir().ok();
    let queue_path = if queue_supported {
        queue::new_session_path()?
    } else {
        queue::try_default_path()?
    };
    let (mut queue, queue_load_warning) = if queue_supported {
        Queue::load_or_default_with_warning(&queue_path)
    } else {
        (Queue::new(), None)
    };
    if let Some(warning) = &queue_load_warning {
        eprintln!("cmdq: {warning}");
    }
    let active_peer_count = crate::session_lease::active_peer_count(&queue_path).unwrap_or(0);
    let queue_known_paused = queue.paused;
    let restored = prepare_queue_for_startup(&mut queue, session_cwd.as_deref(), active_peer_count);
    let startup_status = restored.status.or(queue_load_warning);
    let queue_known_items = queue.item_snapshot();
    let mut session_lease = if queue_supported {
        SessionLease::start(&queue_path, session_cwd.as_deref()).ok()
    } else {
        None
    };

    let (cols, rows) = {
        let (c, r) = crossterm::terminal::size().unwrap_or((80, 24));
        (c.max(1), r.max(1))
    };
    let (pty, io_pair) = ShellPty::spawn(cfg.shell.as_deref(), cols, rows)?;
    let pty_output = pty_output::spawn_reader(io_pair.reader);
    let mut shell_writer: Box<dyn Write + Send> = io_pair.writer;

    let restore_state = shared_restore_state(cols, rows);
    enable_raw_mode().context("enable_raw_mode")?;
    let _terminal_guard = TerminalGuard::install(restore_state.clone());
    let queued_count = Arc::new(AtomicUsize::new(queue.len()));
    install_signal_cleanup(
        pending_signals,
        SignalCleanup {
            restore: restore_state.clone(),
            lease: session_lease.as_ref().map(SessionLease::remover),
            session_dirs: pty.session_dirs().to_vec(),
            queue_path: queue_path.clone(),
            queued_count: queued_count.clone(),
        },
    );
    let mut screen = Screen::new(pty, restore_state, cols, rows);
    // Bracketed paste lets us distinguish typed text from pasted text.
    // Save the outer keyboard mode with a neutral base. Children can negotiate
    // their own keyboard protocol; requesting cmdq-only CSI-u flags here would
    // make an ordinary shell receive encodings it never asked for.
    // We do NOT enter alt-screen and we do NOT enable mouse capture: that
    // would cost the user their terminal's native selection, scrollback,
    // hyperlinks, OSC 52 clipboard, and image protocols.
    let _ = execute!(screen.out, EnableBracketedPaste);
    let _ = execute!(
        screen.out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::empty())
    );

    let mut state = AppState {
        config,
        shell_kind,
        queue_supported,
        wait_for_bash_prompt: shell_kind == ShellKind::Bash,
        pending_bash_exit: None,
        queue,
        editor: LineEditor::new(),
        shell_state: ShellState::Unknown,
        manual_passthrough: false,
        auto_passthrough: false,
        child: ChildModes::default(),
        force_queue: false,
        show_help: false,
        terminal_allows_panel: terminal_allows_panel(cols, rows),
        last_sigint_at: None,
        command_started_at: None,
        command_tty_check_after: None,
        prompt: PromptMirror::default(),
        running_output_tail: OutputTail::default(),
        child_input_active: false,
        pending_quit_at: None,
        pending_clear_at: None,
        last_exit_code: None,
        last_esc_at: None,
        activity: None,
        recovery: Vec::new(),
        status: String::new(),
        status_kind: StatusKind::Notice,
        status_set_at: None,
        queue_dirty: false,
        queue_save_error: None,
        queue_path: Some(queue_path.clone()),
        queue_known_items,
        queue_known_paused,
        queue_external_change_pending: false,
        restored_queue_paused_locally: restored.paused_locally,
        shell_cwd: session_cwd.clone(),
        session_cwd,
        resume_cwd_warning: restored.resume_cwd_warning,
        resume_cwd_confirmation_started_at: None,
    };
    if let Some(status) = startup_status {
        state.set_status(status);
    }

    let mut sniffer = OutputSniffer::new();
    let mut paint_pending = false;
    let mut output_generation: u64 = 0;
    let mut last_queue_sync = Instant::now();
    let mut last_session_lease_refresh = Instant::now();
    #[cfg(unix)]
    let mut terminal_input = crate::terminal_input::Reader::new((cols, rows));
    #[cfg(unix)]
    if let Some(wakeup) = pty_output.wakeup {
        terminal_input.set_output_wakeup(wakeup, pty_output.wakeup_armed.clone());
    }

    // The shell starts wherever the terminal's cursor already is — usually
    // mid-screen, right under the line that launched cmdq. Ask the terminal
    // instead of assuming the bottom row, so the first panel reservation
    // neither scrolls history away nor leaves a blank gap above the output.
    #[cfg(unix)]
    let initial_cursor = terminal_input
        .query_cursor_position(&mut screen.out, CURSOR_QUERY_TIMEOUT)
        .ok()
        .flatten();
    #[cfg(not(unix))]
    let initial_cursor = crossterm::cursor::position().ok();
    if let Some((col, row)) = initial_cursor {
        screen.set_shell_cursor(col, row);
    }

    let result = loop {
        // Readline/ZLE briefly retain their raw mode while submitting a
        // command. Allow that handoff to settle before treating it as a child
        // requesting direct input. Recheck even when there is no output.
        state.child.tty_input = state.tty_input_requested(&screen.pty);
        refresh_auto_passthrough_for_child_modes(&mut state, &mut screen, None)?;

        // Give input, repainting, and child-exit checks a turn even when the
        // producer continuously fills the channel (e.g. yes or a busy log).
        let mut had_bytes = false;
        for _ in 0..32 {
            match pty_output.chunks.try_recv() {
                Ok(chunk) => {
                    had_bytes = true;
                    sniffer.forward(chunk, &mut state, &mut screen, &mut *shell_writer)?;
                }
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if had_bytes {
            output_generation = output_generation.wrapping_add(1);
        }
        let _ = screen.out.flush();

        // An unfinished escape sequence has no printable content. Keep it
        // buffered until complete (or child exit): timing it out lets a later
        // cursor move or panel repaint splice bytes into the child's sequence.
        // Slow programs, network links, and busy runners can pause arbitrarily
        // between fragments of the same terminal control sequence.
        state.tick_status();
        flush_pending_escape_if_due(&mut state, &mut ShellInput::new(&mut *shell_writer, None));

        if let Some(status) = screen.pty.try_wait().context("wait for shell")? {
            sniffer.flush_pending(&mut state, &mut screen)?;
            // Tear down our reservation before exiting so the user's
            // terminal returns to a clean state.
            let _ = screen.hide_panel();
            break Ok(status.exit_code());
        }

        let desired = state.desired_layout(screen.rows());
        if desired != screen.layout() {
            screen.set_layout(desired)?;
        }

        // PTY output wakes the poll through a pipe, so idle iterations can
        // sleep longer; keep the short timeout only while a partial key
        // sequence or a deferred repaint is pending.
        #[cfg(unix)]
        let input_pending = terminal_input.has_pending_input();
        #[cfg(not(unix))]
        let input_pending = false;
        let timeout = if had_bytes {
            Duration::from_millis(0)
        } else if paint_pending || input_pending {
            POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        #[cfg(unix)]
        let event = match terminal_input
            .read(timeout)
            .context("read terminal input")?
        {
            Some(crate::terminal_input::Input::Event(event)) => Some(event),
            Some(crate::terminal_input::Input::Reply(bytes)) => {
                shell_writer
                    .write_all(&bytes)
                    .context("forward terminal reply")?;
                shell_writer.flush()?;
                None
            }
            None => None,
        };
        #[cfg(not(unix))]
        let event = if crossterm::event::poll(timeout).context("poll terminal input")? {
            Some(crossterm::event::read().context("event::read")?)
        } else {
            None
        };
        if let Some(event) = event {
            // A reader can restore canonical mode while poll waits for the
            // next key. Route against the current discipline, not the state
            // sampled before the last output batch.
            let direct_input = state.tty_input_requested(&screen.pty);
            if direct_input != state.child.tty_input {
                state.child.tty_input = direct_input;
                refresh_auto_passthrough_for_child_modes(&mut state, &mut screen, None)?;
            }
            match event {
                CtEvent::Resize(new_cols, new_rows) => {
                    let reflowed_panel_rows = match screen.layout() {
                        PanelLayout::Reserved { height } => panel::reflowed_panel_height(
                            &state.panel_view(),
                            height,
                            screen.cols(),
                            new_cols.max(1),
                        ),
                        PanelLayout::Hidden => 0,
                    };
                    screen.resize(new_cols, new_rows, reflowed_panel_rows)?;
                    state.terminal_allows_panel =
                        terminal_allows_panel(screen.cols(), screen.rows());
                    screen.set_layout(state.desired_layout(screen.rows()))?;
                    screen.resize_pty();
                }
                CtEvent::Key(key) => {
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        #[cfg(unix)]
                        if !state.editor_owns_input() && !state.show_help {
                            shell_writer.write_all(&terminal_input.event_bytes)?;
                            shell_writer.flush()?;
                        }
                        continue;
                    }
                    #[cfg(unix)]
                    let original = Some(terminal_input.event_bytes.as_slice());
                    #[cfg(not(unix))]
                    let original = None;
                    let mut shell = ShellInput::new(&mut *shell_writer, Some(&mut screen));
                    if let KeyOutcome::Quit = handle_key(key, &mut state, &mut shell, original) {
                        screen.hide_panel()?;
                        break Ok(0);
                    }
                }
                CtEvent::Paste(text) => handle_paste(
                    text,
                    &mut state,
                    &mut ShellInput::new(&mut *shell_writer, None),
                ),
                CtEvent::Mouse(mouse) => {
                    if state.child.mouse_capture || state.auto_passthrough {
                        #[cfg(unix)]
                        let bytes = terminal_input.event_bytes.clone();
                        #[cfg(not(unix))]
                        let bytes = key_encoding::encode_mouse_event_for_pty(mouse);
                        #[cfg(unix)]
                        let _ = mouse;
                        if !bytes.is_empty() {
                            ShellInput::new(&mut *shell_writer, None).send(&bytes);
                        }
                    }
                }
                CtEvent::FocusGained if state.child.focus_events => {
                    ShellInput::new(&mut *shell_writer, None).send(b"\x1b[I");
                }
                CtEvent::FocusLost if state.child.focus_events => {
                    ShellInput::new(&mut *shell_writer, None).send(b"\x1b[O");
                }
                CtEvent::FocusGained | CtEvent::FocusLost => {}
            }
        }

        if state.queue_supported {
            save_queue_if_dirty(&mut state);
            queued_count.store(state.queue.len(), Ordering::Release);
            sync_queue_from_disk_if_due(&mut state, &mut last_queue_sync);
        }
        if last_session_lease_refresh.elapsed() >= SESSION_LEASE_REFRESH_INTERVAL {
            last_session_lease_refresh = Instant::now();
            if let Some(lease) = &mut session_lease {
                let _ = lease.refresh();
            }
        }

        // Repaint the panel when what it shows has changed. Output counts as
        // a change: it leaves the terminal cursor away from the panel's input
        // line. Rate-limited so a flood of output cannot turn into a flood of
        // repaints.
        paint_pending = screen.paint_panel(
            &state.panel_view(),
            state.editor_owns_input(),
            output_generation,
        )?;
    };

    save_queue_if_dirty(&mut state);
    queued_count.store(state.queue.len(), Ordering::Release);
    let _ = screen.pty.kill();
    drop(session_lease);
    if state.queue_supported && state.queue.is_empty() {
        queue::remove_session_dir(&queue_path);
    }
    result
}
