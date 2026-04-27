//! Top-level application: wires PTY + vt100 + queue + ui + input together.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CtEvent, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::input::{InputAction, LineEditor};
use crate::osc133::{self, Detector};
use crate::pty::ShellPty;
use crate::queue::{self, Queue};
use crate::ui::{self, QueueViewState};

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

/// Two Esc presses within this window toggle raw-input passthrough. SSH-safe
/// alternative to Ctrl-\, which terminals/SSH often eat or remap to SIGQUIT.
const ESC_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

const RENDER_INTERVAL: Duration = Duration::from_millis(16);
const ETX: u8 = 0x03;

struct AppState {
    queue: Queue,
    editor: LineEditor,
    shell_state: ShellState,
    /// User explicitly toggled passthrough.
    manual_passthrough: bool,
    /// Inner program is in alt-screen → we passthrough automatically.
    auto_passthrough: bool,
    force_queue: bool,
    show_help: bool,
    /// Last time we forwarded SIGINT (0x03) to the inner shell.
    last_sigint_at: Option<Instant>,
    /// When the current command started (set on CommandStart, cleared on End).
    command_started_at: Option<Instant>,
    /// Timestamp of first Ctrl-D when queue was non-empty; second within
    /// QUIT_CONFIRM_WINDOW actually quits.
    pending_quit_at: Option<Instant>,
    /// Timestamp of the most recent bare-Esc press; a second Esc within
    /// ESC_DOUBLE_TAP_WINDOW toggles passthrough.
    last_esc_at: Option<Instant>,
    /// Has the user pressed Tab (chain toggle) at least once this session?
    /// First press shows an explanatory status; subsequent presses are terse.
    chain_seen: bool,
    status: String,
    /// Time the current status was set; used to fade it after STATUS_TTL.
    status_set_at: Option<Instant>,
    /// Set when the queue mutates so the loop knows to persist it.
    queue_dirty: bool,
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

    fn in_queue_mode(&self) -> bool {
        !self.effective_passthrough() && (self.command_long_running() || self.force_queue)
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
            self.status.clear();
            self.status_set_at = None;
        }
    }

    fn toggle_force_queue(&mut self) {
        self.force_queue = !self.force_queue;
        let msg = if self.force_queue {
            "force-queue ON (Ctrl-Q to disable)"
        } else {
            "force-queue OFF"
        };
        self.set_status(msg);
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

    let queue_path = queue::default_path();
    let mut queue = Queue::load_or_default(&queue_path);
    queue.clear();
    queue.paused = false;

    let (cols, rows) = {
        let (c, r) = crossterm::terminal::size().unwrap_or((80, 24));
        // A zero-sized grid trips up vt100; clamp to a sane minimum.
        (c.max(20), r.max(5))
    };
    let (mut pty, io) = ShellPty::spawn(cfg.shell.as_deref(), cols, rows)?;

    let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();
    {
        let mut reader: Box<dyn Read + Send> = io.reader;
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

    let mut writer: Box<dyn Write + Send> = io.writer;

    let mut parser = vt100::Parser::new(rows, cols, 0);
    let mut osc = Detector::new();

    enable_raw_mode().context("enable_raw_mode")?;
    let mut stdout = io::stdout();
    // Alt-screen is required; mouse / bracketed-paste / enhanced keys are
    // best-effort so a terminal that rejects them still gets a working cmdq.
    execute!(stdout, EnterAlternateScreen).context("EnterAlternateScreen")?;
    let _ = execute!(stdout, EnableMouseCapture);
    let _ = execute!(stdout, EnableBracketedPaste);
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend).context("ratatui Terminal::new")?;

    let mut state = AppState {
        queue,
        editor: LineEditor::new(),
        shell_state: ShellState::Unknown,
        manual_passthrough: false,
        auto_passthrough: false,
        force_queue: false,
        show_help: false,
        last_sigint_at: None,
        command_started_at: None,
        pending_quit_at: None,
        last_esc_at: None,
        chain_seen: false,
        status: String::new(),
        status_set_at: None,
        queue_dirty: true,
    };

    let mut last_render = Instant::now();

    let result = loop {
        let mut had_bytes = false;
        loop {
            match pty_rx.try_recv() {
                Ok(bytes) => {
                    had_bytes = true;
                    // vt100 0.16 has a few unwrap()s in its escape-sequence
                    // handling that can panic on unusual byte streams. We'd
                    // rather drop a malformed chunk than take down cmdq.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        parser.process(&bytes)
                    }));
                    let evs = osc.feed(&bytes);
                    for ev in evs {
                        match ev {
                            osc133::Event::PromptStart | osc133::Event::PromptEnd => {
                                state.shell_state = ShellState::AtPrompt;
                            }
                            osc133::Event::CommandStart => {
                                state.shell_state = ShellState::Running;
                                state.command_started_at = Some(Instant::now());
                            }
                            osc133::Event::CommandEnd { exit_code } => {
                                state.shell_state = ShellState::AtPrompt;
                                state.command_started_at = None;
                                handle_command_end(&mut state, exit_code, &mut writer);
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // Auto-passthrough: when the inner program is in alt-screen
        // (vim/htop/less/fzf/btop), forward keystrokes verbatim and hide
        // the queue panel so we don't fight with their UI.
        let on_alt = parser.screen().alternate_screen();
        if on_alt && !state.auto_passthrough {
            state.auto_passthrough = true;
            state.set_status("alt-screen detected — keys go to the running app");
        } else if !on_alt && state.auto_passthrough {
            state.auto_passthrough = false;
            state.set_status("alt-screen exit — queue mode restored");
        }

        state.tick_status();

        if let Ok(Some(_)) = pty.try_wait() {
            break Ok(());
        }

        let timeout = if had_bytes {
            Duration::from_millis(0)
        } else {
            RENDER_INTERVAL
        };
        if crossterm::event::poll(timeout).unwrap_or(false) {
            let event = crossterm::event::read().context("event::read")?;
            match event {
                CtEvent::Resize(cw, rh) => {
                    let cw = cw.max(20);
                    let rh = rh.max(5);
                    parser.screen_mut().set_size(rh, cw);
                    let _ = pty.resize(cw, rh);
                }
                CtEvent::Key(key) => {
                    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                        continue;
                    }
                    match handle_key(key, &mut state, &mut writer) {
                        KeyOutcome::Quit => break Ok(()),
                        KeyOutcome::Continue => {}
                    }
                }
                CtEvent::Paste(text) => handle_paste(text, &mut state, &mut writer),
                _ => {}
            }
        }

        if state.queue_dirty {
            let _ = state.queue.save(&queue_path);
            state.queue_dirty = false;
        }

        if last_render.elapsed() >= RENDER_INTERVAL {
            let show_panel = state.in_queue_mode();
            let qview = QueueViewState {
                queue: &state.queue,
                running: matches!(state.shell_state, ShellState::Running),
                passthrough_to_child: state.effective_passthrough(),
                force_queue: state.force_queue,
                input_buffer: &state.editor.buffer,
                input_cursor: state.editor.cursor,
                editing_index: state.editor.editing_index,
                max_queue_visible: 8,
                status: &state.status,
                pending_quit: state.pending_quit_active(),
            };
            let show_help = state.show_help;
            term.draw(|f| {
                ui::render(f, &parser, &qview, show_panel);
                if show_help {
                    ui::render_help(f);
                }
            })
            .context("draw")?;
            last_render = Instant::now();
        }
    };

    let _ = pty.kill();
    let _ = restore_terminal();
    result
}

/// On CommandEnd, decide whether to dispatch the next queued command. If we
/// recently sent SIGINT (or the exit code looks like one), auto-pause instead.
fn handle_command_end(
    state: &mut AppState,
    exit_code: Option<i32>,
    writer: &mut Box<dyn Write + Send>,
) {
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
        return;
    }
    state.last_sigint_at = None;

    if state.queue.paused || state.queue.is_empty() {
        return;
    }
    if let Some(item) = state.queue.pop_next_eligible(exit_code) {
        let _ = writer.write_all(item.command.as_bytes());
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
        state.queue_dirty = true;
        state.set_status(format!(
            "dispatched: {}",
            truncate_for_status(&item.command)
        ));
    }
}

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

    // Help overlay: dismiss only on a small set of "I'm done reading" keys,
    // so a stray Ctrl-C or paste while help is open isn't lost or misrouted.
    if state.show_help {
        let dismiss = matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('?') | KeyCode::F(1)
        );
        if dismiss {
            state.show_help = false;
        }
        return KeyOutcome::Continue;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if matches!(key.code, KeyCode::F(1)) {
        state.show_help = true;
        return KeyOutcome::Continue;
    }

    // Double-Esc toggles passthrough — SSH-safe alternative to Ctrl-\, which
    // some terminals/SSH paths swallow or remap to SIGQUIT. Only Press counts:
    // key-repeat from a held Esc shouldn't fire a toggle every frame. The
    // first Esc still falls through to its existing behavior (cancel edit /
    // forward to child); the second within the window swallows itself and
    // flips the mode.
    if matches!(key.code, KeyCode::Esc)
        && !ctrl
        && !key.modifiers.contains(KeyModifiers::ALT)
        && key.kind == KeyEventKind::Press
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
    }

    let in_queue = state.in_queue_mode();

    if ctrl && matches!(key.code, KeyCode::Char('q' | 'Q')) && !in_queue {
        state.toggle_force_queue();
        return KeyOutcome::Continue;
    }

    if ctrl && matches!(key.code, KeyCode::Char('\\')) && in_queue {
        state.toggle_manual_passthrough();
        return KeyOutcome::Continue;
    }

    // Ctrl-D quit flow:
    //   - empty buffer + no edit: quit, but require a second press within
    //     QUIT_CONFIRM_WINDOW if the queue still has items.
    //   - the editor binds Ctrl-D to DeleteEdited only, so we have to handle
    //     the quit case here.
    if ctrl
        && matches!(key.code, KeyCode::Char('d' | 'D'))
        && in_queue
        && state.editor.editing_index.is_none()
        && state.editor.buffer.is_empty()
    {
        if state.queue.is_empty() || state.pending_quit_active() {
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

    if !in_queue {
        let bytes = encode_key_for_pty(&key);
        if bytes.contains(&ETX) {
            state.last_sigint_at = Some(Instant::now());
        }
        if !bytes.is_empty() {
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
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
            state.queue.push(&command, conditional);
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
            state.queue.paused = !state.queue.paused;
            state.queue_dirty = true;
            state.set_status(if state.queue.paused {
                "queue paused"
            } else {
                "queue resumed"
            });
        }
        InputAction::ClearQueue => {
            state.queue.clear();
            state.queue_dirty = true;
            state.set_status("queue cleared");
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

fn truncate_for_status(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

fn handle_paste(text: String, state: &mut AppState, writer: &mut Box<dyn Write + Send>) {
    if !state.in_queue_mode() {
        // Pass through to the running program; it can decide how to interpret
        // the bracketed paste content.
        let _ = writer.write_all(text.as_bytes());
        let _ = writer.flush();
        return;
    }
    // Treat the whole paste as one queue item: collapse newlines so the
    // single-line buffer makes sense, then drop into the input buffer where
    // the user can review and Enter to commit.
    let normalized: String = text
        .split(['\n', '\r'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    state.editor.insert_str(&normalized);
}

fn encode_key_for_pty(key: &KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
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
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Left => out.extend_from_slice(b"\x1b[D"),
        Right => out.extend_from_slice(b"\x1b[C"),
        Up => out.extend_from_slice(b"\x1b[A"),
        Down => out.extend_from_slice(b"\x1b[B"),
        Home => out.extend_from_slice(b"\x1b[H"),
        End => out.extend_from_slice(b"\x1b[F"),
        PageUp => out.extend_from_slice(b"\x1b[5~"),
        PageDown => out.extend_from_slice(b"\x1b[6~"),
        Insert => out.extend_from_slice(b"\x1b[2~"),
        Delete => out.extend_from_slice(b"\x1b[3~"),
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

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    execute!(
        stdout,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()?;
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

    fn make_state() -> AppState {
        AppState {
            queue: Queue::new(),
            editor: LineEditor::new(),
            shell_state: ShellState::Running,
            manual_passthrough: false,
            auto_passthrough: false,
            force_queue: false,
            show_help: false,
            last_sigint_at: None,
            command_started_at: None,
            pending_quit_at: None,
            last_esc_at: None,
            chain_seen: false,
            status: String::new(),
            status_set_at: None,
            queue_dirty: false,
        }
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
        handle_command_end(&mut state, Some(0), &mut w);
        assert!(!state.queue.paused);
        assert!(state.queue.is_empty(), "front item dispatched");
        let written = buf.lock().unwrap();
        let s = String::from_utf8_lossy(&written);
        assert!(s.contains("echo a"));
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
    fn quick_command_does_not_enter_queue_mode() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now());
        assert!(!s.in_queue_mode(), "fresh command must not flash the panel");
    }

    #[test]
    fn long_running_command_enters_queue_mode() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now() - QUEUE_PANEL_DELAY - Duration::from_millis(50));
        assert!(s.in_queue_mode());
    }

    #[test]
    fn force_queue_bypasses_delay() {
        let mut s = make_state();
        s.command_started_at = Some(Instant::now()); // just started
        s.force_queue = true;
        assert!(s.in_queue_mode());
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
        // Force queue mode so the first Esc is handled by the editor path
        // (its existing reset behavior is fine; we only care about the toggle).
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

        // And a fresh double-tap toggles back to queue mode.
        let _ = handle_key(esc_press(), &mut s, &mut w);
        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(!s.manual_passthrough, "double-Esc again returns to queue mode");
    }

    #[test]
    fn esc_outside_window_does_not_toggle() {
        let mut s = make_state();
        s.force_queue = true;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut w: Box<dyn Write + Send> = Box::new(VecWriter(buf.clone()));

        let _ = handle_key(esc_press(), &mut s, &mut w);
        // Backdate the first Esc beyond the window.
        s.last_esc_at = Some(Instant::now() - ESC_DOUBLE_TAP_WINDOW - Duration::from_millis(50));
        let _ = handle_key(esc_press(), &mut s, &mut w);
        assert!(
            !s.manual_passthrough,
            "second Esc outside window must not toggle"
        );
    }
}
