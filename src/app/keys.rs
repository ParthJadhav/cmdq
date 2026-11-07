//! Keys and pastes from the user's terminal: global shortcuts, routing to
//! the queue editor or to the shell/running program, and the editor's
//! actions on the queue.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::key_encoding::encode_key_for_pty;
use super::lifecycle::{ShellInput, dispatch_next_eligible, mark_command_started};
use super::queue_actions::{
    clear_queue, restore_removed_commands, toggle_queue_pause, withdraw_pending_clear,
};
use super::queue_sync::current_shell_cwd;
use super::shell_syntax::is_likely_complete_shell_command;
use super::status::truncate_for_status;
use super::{AppState, ShellState};
use crate::input::InputAction;
use crate::panel::StatusKind;

/// Two Esc presses within this window toggle raw-input passthrough. SSH-safe
/// alternative to Ctrl-\, which terminals/SSH often eat or remap to SIGQUIT.
pub(super) const ESC_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

const ETX: u8 = 0x03;
const FS: u8 = 0x1c;
const SUB: u8 = 0x1a;

#[derive(Debug, Clone, Copy)]
pub(super) enum KeyOutcome {
    Continue,
    Quit,
}

/// Handle one key press. `original` is the byte encoding the terminal sent,
/// forwarded as-is when the key goes to the shell or running program.
pub(super) fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    shell: &mut ShellInput<'_>,
    original: Option<&[u8]>,
) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Help overlay: dismiss only on a small set of "I'm done reading" keys,
    // so a stray Ctrl-C or paste while help is open isn't lost or misrouted.
    if state.show_help {
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            state.show_help = false;
            if matches!(state.shell_state, ShellState::Running) {
                state.last_sigint_at = Some(Instant::now());
                shell.send(&[ETX]);
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

    if state.pending_clear_at.is_some() && !is_ctrl_k(&key) {
        withdraw_pending_clear(state);
    }

    if matches!(key.code, KeyCode::F(1)) && !state.effective_passthrough() {
        state.show_help = true;
        return KeyOutcome::Continue;
    }

    if matches!(key.code, KeyCode::F(2))
        && !state.auto_passthrough
        && state.queue_supported
        && (state.panel_should_be_visible() || state.manual_passthrough)
    {
        state.last_esc_at = None;
        // A detected prompt already owns input. F2 explicitly opens the queue.
        if state.child_input_prompt_active() {
            state.force_queue = true;
            state.manual_passthrough = false;
        } else {
            state.toggle_manual_passthrough();
        }
        return KeyOutcome::Continue;
    }
    if state.panel_should_be_visible()
        && !state.effective_passthrough()
        && !state.child_input_prompt_active()
    {
        if matches!(key.code, KeyCode::F(3)) {
            state.activity = None;
            state.clear_status();
            return KeyOutcome::Continue;
        }
        if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('u' | 'U'))
        {
            restore_removed_commands(state);
            return KeyOutcome::Continue;
        }
    }

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
        let child_owns_input = !state.editor_owns_input();
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
        if child_owns_input {
            // Hold a child-bound Escape briefly so Esc Esc can toggle raw
            // mode without leaking the first byte into the running app.
            return KeyOutcome::Continue;
        }
    } else if state.last_esc_at.is_some() {
        if !state.editor_owns_input() {
            shell.send(b"\x1b");
        }
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
        shell.send(&[FS]);
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
        shell.send(&[ETX]);
        state.set_status("sent Ctrl-C to running command");
        return KeyOutcome::Continue;
    }

    if ctrl
        && matches!(key.code, KeyCode::Char('z' | 'Z'))
        && matches!(state.shell_state, ShellState::Running)
        && !state.effective_passthrough()
    {
        shell.send(&[SUB]);
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
            state.forget_resume_cwd_warning();
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
            toggle_queue_pause(state, shell);
            return KeyOutcome::Continue;
        }
        if matches!(key.code, KeyCode::Char('k' | 'K')) {
            clear_queue(state);
            return KeyOutcome::Continue;
        }
    }

    // At an empty prompt Ctrl-X is almost always a reflex to start or resume
    // the queue. Forwarding it makes readline wait for a chord and silently
    // consumes the next character. Preserve readline chords once the user has
    // begun typing, and allow the legacy behavior to be configured explicitly.
    if !editor_owns
        && is_ctrl_x(&key)
        && matches!(state.shell_state, ShellState::AtPrompt)
        && state.queue.is_empty()
        && state.prompt.is_empty()
        && !state.config.keys.forward_ctrl_x
    {
        state.set_status("Nothing queued · Ctrl-Q opens the queue");
        return KeyOutcome::Continue;
    }

    if !editor_owns {
        let was_child_input = state.child_input_prompt_active();
        let was_any_key_prompt = matches!(state.shell_state, ShellState::Running)
            && !state.child.alt_screen
            && !state.child.mouse_capture
            && state.running_output_tail.looks_like_any_key_prompt();
        let at_prompt = state.shell_state == ShellState::AtPrompt && !state.effective_passthrough();
        let submitted_prompt_line = if at_prompt {
            state.prompt.on_key(&key)
        } else {
            None
        };
        // Preserve application-cursor keys (SS3), modified function keys,
        // legacy encodings, and protocols negotiated by the child itself.
        let bytes = original
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| encode_key_for_pty(&key));
        if bytes.contains(&ETX) {
            state.last_sigint_at = Some(Instant::now());
        }
        if !bytes.is_empty() {
            shell.send(&bytes);
        }
        if (was_child_input || was_any_key_prompt)
            && (matches!(key.code, KeyCode::Enter)
                || bytes.contains(&ETX)
                || (was_any_key_prompt && !bytes.is_empty()))
        {
            state.child_input_active = false;
            state.running_output_tail.clear();
        }
        if bytes.contains(&ETX) {
            state.prompt.cancel_continuation();
        }
        if at_prompt
            && state
                .prompt
                .starts_command(&key, submitted_prompt_line.as_deref())
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
        InputAction::CompletionUnavailable => {
            state.set_status("Completion is not available in the queue")
        }
        InputAction::FinishEditFirst => {
            state.set_status("Unsaved changes — Enter saves, Esc cancels before switching items")
        }
        InputAction::ForwardToChild(bytes) => {
            if bytes.contains(&ETX) {
                state.last_sigint_at = Some(Instant::now());
            }
            shell.send(&bytes);
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
                state.forget_resume_cwd_warning();
            }
            state.queue.push_with_origin(&command, conditional, origin);
            state.queue_dirty = true;
            state.set_status_as(
                StatusKind::Routine,
                format!("added: {}", truncate_for_status(&command)),
            );
            // A draft kept the panel open after its command finished. The
            // shell is idle and nothing holds the queue, so run it now rather
            // than parking it until some unrelated command ends.
            if matches!(state.shell_state, ShellState::AtPrompt)
                && !state.force_queue
                && !state.queue.paused
            {
                let prev_exit = state.last_exit_code;
                dispatch_next_eligible(state, prev_exit, shell);
            }
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
                state.set_status_as(
                    StatusKind::Routine,
                    format!("saved: {}", truncate_for_status(&command)),
                );
            }
        }
        InputAction::CancelEdit => {
            state.set_status_as(StatusKind::Routine, "edit cancelled");
        }
        InputAction::DeleteEdited => {
            if let Some(idx) = state.editor.editing_index
                && let Some(id) = state.queue.items().get(idx).map(|it| it.id)
                && let Some(removed) = state.queue.remove(id)
            {
                state.queue_dirty = true;
                state.recovery = vec![(idx, removed.clone())];
                state.activity = Some(format!(
                    "Removed {} — Alt-U restores paused",
                    truncate_for_status(&removed.command)
                ));
                state.set_status_as(
                    StatusKind::Outcome,
                    format!("removed: {}", truncate_for_status(&removed.command)),
                );
            }
            state.editor.finish_edit();
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
            toggle_queue_pause(state, shell);
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
            let msg = if now_on {
                "Only if previous succeeds — Alt-S changes to Always"
            } else {
                "Always run after previous command finishes"
            };
            state.set_status(msg);
        }
    }
    KeyOutcome::Continue
}

/// A lone Esc held back for a possible double-tap goes to the program once
/// the double-tap window has passed.
pub(super) fn flush_pending_escape_if_due(state: &mut AppState, shell: &mut ShellInput<'_>) {
    let expired = state
        .last_esc_at
        .is_some_and(|started| started.elapsed() > ESC_DOUBLE_TAP_WINDOW);
    if !expired {
        return;
    }
    if !state.editor_owns_input() {
        shell.send(b"\x1b");
    }
    state.last_esc_at = None;
}

fn is_ctrl_x(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('x' | 'X'))
}

fn is_ctrl_k(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('k' | 'K'))
}

pub(super) fn handle_paste(text: String, state: &mut AppState, shell: &mut ShellInput<'_>) {
    state.show_help = false;
    state.pending_quit_at = None;
    if state.pending_clear_at.is_some() {
        withdraw_pending_clear(state);
    }
    state.resume_cwd_confirmation_started_at = None;
    if !state.editor_owns_input() && should_open_queue_for_running_paste(state) {
        state.activate_queue_for_running_command();
    }
    if !state.editor_owns_input() {
        // Pass through to the running program. Only wrap with bracketed-paste
        // markers when the child explicitly enabled paste mode; otherwise a
        // plain `read`, password prompt, or simple REPL would receive the
        // markers as literal input.
        let submitted_prompt_line =
            if state.shell_state == ShellState::AtPrompt && !state.effective_passthrough() {
                state.prompt.on_paste(&text, state.child.bracketed_paste)
            } else {
                None
            };
        if state.child.bracketed_paste {
            shell.send(&[b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat());
        } else {
            shell.send(text.as_bytes());
        }
        if state.child_input_active && (text.contains('\n') || text.contains('\r')) {
            state.child_input_active = false;
            state.running_output_tail.clear();
        }
        if submitted_prompt_line
            .as_deref()
            .is_some_and(is_likely_complete_shell_command)
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
