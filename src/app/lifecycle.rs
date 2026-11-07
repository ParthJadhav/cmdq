//! The shell's command lifecycle as reported by OSC 133 markers, and running
//! the next queued command when one ends.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::queue_sync::{
    current_shell_cwd, dispatch_cwd_warning_for_item, resume_cwd_warning_for_current_shell,
    save_queue_if_dirty,
};
use super::screen::{ALT_SCREEN_EXIT, FOCUS_EVENTS_OFF, MOUSE_TRACKING_OFF, Screen};
use super::status::truncate_for_status;
use super::{AppState, ShellState};
use crate::osc133;
use crate::queue;
use crate::shell_integration::{ShellKind, encode_queued_command};

/// Ctrl-C means the user changed their mind, so a command ending within this
/// window of a forwarded `0x03` (or with exit code 130) pauses the queue.
const SIGINT_AUTO_PAUSE_WINDOW: Duration = Duration::from_secs(3);
const SIGINT_EXIT_CODE: i32 = 130;

/// Bytes for the shell. Queued commands go through
/// [`ShellInput::send_command`], which first gives the panel's rows back: the
/// shell must read a command at the size it runs at, not have readline
/// redraw it mid-read.
pub(super) struct ShellInput<'a> {
    writer: &'a mut dyn Write,
    screen: Option<&'a mut Screen>,
}

impl<'a> ShellInput<'a> {
    /// `screen` is `None` only where no terminal exists (unit tests).
    pub(super) fn new(writer: &'a mut dyn Write, screen: Option<&'a mut Screen>) -> Self {
        Self { writer, screen }
    }

    /// Forward input typed for the shell or the running program. A closed
    /// PTY shows up as the child exiting, so write errors are not reported.
    pub(super) fn send(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn send_command(&mut self, command: &str, shell: ShellKind) -> io::Result<()> {
        let encoded = encode_queued_command(command, shell)?;
        if let Some(screen) = self.screen.as_deref_mut() {
            screen.hide_panel().map_err(io::Error::other)?;
        }
        self.writer.write_all(encoded.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
}

pub(super) fn handle_osc_event(
    event: osc133::Event,
    state: &mut AppState,
    screen: &mut Screen,
    shell: &mut dyn Write,
) -> Result<()> {
    if !state.queue_supported {
        return Ok(());
    }
    let bash_prompt_ready = state.wait_for_bash_prompt && matches!(event, osc133::Event::PromptEnd);
    let event = if bash_prompt_ready {
        let Some(exit_code) = state.pending_bash_exit.take() else {
            // Resize redraws can repeat B; only a preceding D may dispatch.
            return Ok(());
        };
        osc133::Event::CommandEnd { exit_code }
    } else {
        event
    };
    match event {
        osc133::Event::PromptStart | osc133::Event::PromptEnd => {
            state.shell_state = ShellState::AtPrompt;
            state.prompt.new_prompt();
        }
        osc133::Event::CommandStart => {
            // Keep the short settling delay set by `mark_command_started`.
            // Some shells (fish 4.x) fire their preexec hook while their line
            // editor still owns the tty in a raw/no-echo mode, then restore a
            // cooked mode a moment later when they exec the command. Checking
            // the tty discipline immediately would misread that transient raw
            // mode as the child requesting direct input and route the next
            // queued keystrokes to the shell instead of the panel.
            mark_command_started(state);
        }
        osc133::Event::CommandEnd { exit_code } => {
            state.shell_state = ShellState::AtPrompt;
            state.command_started_at = None;
            state.last_exit_code = exit_code;
            state.prompt.new_prompt();
            state.running_output_tail.clear();
            state.child_input_active = false;
            restore_child_terminal_modes_after_command(state, screen)?;
            if state.wait_for_bash_prompt && !bash_prompt_ready {
                state.pending_bash_exit = Some(exit_code);
                return Ok(());
            }
            if bash_prompt_ready {
                // Reassert even if the layout is unchanged: readline's
                // get/set-winsize pair can overwrite an earlier resize. B
                // follows that pair.
                screen.resize_pty();
            }
            let dispatched =
                handle_command_end(state, exit_code, &mut ShellInput::new(shell, Some(screen)));
            if !dispatched
                && !state.queue.paused
                && !state.force_queue
                && state.editor.editing_index.is_none()
                && !state.editor.buffer.is_empty()
            {
                state.set_status("draft kept — Enter runs it now, Esc discards it");
            }
        }
        osc133::Event::CurrentDir(path) => {
            state.shell_cwd = Some(path);
        }
    }
    Ok(())
}

fn restore_child_terminal_modes_after_command(
    state: &mut AppState,
    screen: &mut Screen,
) -> Result<()> {
    let child = state.child;
    if child.alt_screen {
        write!(screen.out, "{ALT_SCREEN_EXIT}")?;
    }
    if child.mouse_capture {
        write!(screen.out, "{MOUSE_TRACKING_OFF}")?;
    }
    if child.focus_events {
        write!(screen.out, "{FOCUS_EVENTS_OFF}")?;
    }
    if child.alt_screen || child.mouse_capture || child.focus_events {
        screen.out.flush()?;
    }
    state.reset_child_modes();
    screen.record_alt_screen(false);
    Ok(())
}

pub(super) fn update_child_input_detection(state: &mut AppState, bytes: &[u8]) {
    // The tail only matters while a command runs; it is cleared on every
    // command start, so prompt-time output never needs to be scanned.
    if !matches!(state.shell_state, ShellState::Running) {
        return;
    }
    let saw_line_break = bytes.iter().any(|b| matches!(b, b'\r' | b'\n'));
    state.running_output_tail.feed(bytes);
    let looks_like_prompt = state.running_output_tail.looks_like_input_prompt();
    if state.child_input_active && saw_line_break && !looks_like_prompt {
        state.child_input_active = false;
    }
    if looks_like_prompt && !state.force_queue && !state.effective_passthrough() {
        state.child_input_active = true;
    }
}

/// Recompute whether the running program owns the whole terminal, and give
/// it the panel rows when that changes. `status` announces the change.
pub(super) fn refresh_auto_passthrough_for_child_modes(
    state: &mut AppState,
    screen: &mut Screen,
    status: Option<&'static str>,
) -> Result<()> {
    let was_passthrough = state.auto_passthrough;
    state.auto_passthrough = !state.queue_supported
        || state.child.alt_screen
        || state.child.mouse_capture
        || state.child.tty_input;
    screen.record_alt_screen(state.child.alt_screen);
    if was_passthrough == state.auto_passthrough {
        return Ok(());
    }
    if state.auto_passthrough {
        state.show_help = false;
    }
    if let Some(status) = status {
        state.set_status_as(crate::panel::StatusKind::Routine, status);
    }
    let desired = state.desired_layout(screen.rows());
    if desired != screen.layout() {
        screen.set_layout(desired)?;
    }
    Ok(())
}

pub(super) fn mark_command_started(state: &mut AppState) {
    state.shell_state = ShellState::Running;
    state.command_started_at = Some(Instant::now());
    state.command_tty_check_after = Some(Instant::now() + Duration::from_millis(100));
    state.prompt.new_prompt();
    state.running_output_tail.clear();
    state.child_input_active = false;
    state.reset_child_modes();
}

/// Run the next queued command, unless the one that just ended was
/// interrupted (pause instead) or an item is being edited.
pub(super) fn handle_command_end(
    state: &mut AppState,
    exit_code: Option<i32>,
    shell: &mut ShellInput<'_>,
) -> bool {
    state.manual_passthrough = false;
    state.reset_child_modes();
    let recent_sigint = state
        .last_sigint_at
        .is_some_and(|t| t.elapsed() <= SIGINT_AUTO_PAUSE_WINDOW);
    state.last_sigint_at = None;

    if (recent_sigint || exit_code == Some(SIGINT_EXIT_CODE)) && !state.queue.is_empty() {
        state.queue.paused = true;
        state.queue_dirty = true;
        state.activity = Some("Paused after interruption — Ctrl-X resumes the queue".into());
        state.set_status("Ctrl-C detected — queue paused. Ctrl-X to resume, Ctrl-K to clear.");
        return false;
    }
    if state.queue.paused || state.queue.is_empty() {
        return false;
    }
    if state.editor.editing_index.is_some() {
        state.queue.paused = true;
        state.queue_dirty = true;
        state.activity =
            Some("Queue paused while editing — save or cancel, then Ctrl-X resumes".into());
        state.set_status("queue paused while editing — Enter save, Esc cancel, Ctrl-X resume");
        return false;
    }
    dispatch_next_eligible(state, exit_code, shell)
}

/// Send the next runnable queued command to the shell and mark it running.
/// Chained items whose predecessor failed are skipped into the recovery
/// batch. Returns whether a command was sent.
pub(super) fn dispatch_next_eligible(
    state: &mut AppState,
    prev_exit: Option<i32>,
    shell: &mut ShellInput<'_>,
) -> bool {
    if state.pending_bash_exit.is_some() {
        return false;
    }
    if state.queue_dirty && state.queue_path.is_some() {
        save_queue_if_dirty(state);
        if state.queue_dirty || state.queue.paused {
            return false;
        }
    }

    let mut skipped_conditional = false;
    while let Some(item) = state.queue.front().cloned() {
        if item.conditional && prev_exit != Some(0) {
            let _ = state.queue.remove(item.id);
            if !skipped_conditional {
                state.recovery.clear();
            }
            state.recovery.push((state.recovery.len(), item));
            state.activity = Some(format!(
                "{} skipped — previous command did not succeed",
                state.recovery.len()
            ));
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
                    if let Err(e) = shell.send_command(&item.command, state.shell_kind) {
                        let rollback = state.queue.restore_claimed_front(
                            &queue_path,
                            item,
                            &mut state.queue_known_items,
                            &mut state.queue_known_paused,
                        );
                        state.queue_dirty = rollback.is_err();
                        report_dispatch_failure(state, &e);
                        return false;
                    }
                    state.queue_dirty = false;
                    command_dispatched(state, &item.command);
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
                    state.queue.paused = true;
                    state.queue_dirty = true;
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
            return false;
        }

        if let Err(e) = shell.send_command(&item.command, state.shell_kind) {
            state.queue.paused = true;
            state.queue_dirty = true;
            report_dispatch_failure(state, &e);
            return false;
        }

        let _ = state.queue.remove(item.id);
        state.queue_dirty = true;
        command_dispatched(state, &item.command);
        return true;
    }

    if skipped_conditional {
        state.queue_dirty = true;
        state.forget_resume_cwd_warning_if_empty();
        state.set_status("skipped chained item: previous command did not succeed");
        save_queue_if_dirty(state);
    }
    false
}

fn command_dispatched(state: &mut AppState, command: &str) {
    state.forget_resume_cwd_warning_if_empty();
    state.set_status_as(
        crate::panel::StatusKind::Routine,
        format!("dispatched: {}", truncate_for_status(command)),
    );
    mark_command_started(state);
}

fn report_dispatch_failure(state: &mut AppState, error: &io::Error) {
    state.set_status(format!(
        "dispatch failed; queue paused: {}",
        truncate_for_status(&error.to_string())
    ));
}
