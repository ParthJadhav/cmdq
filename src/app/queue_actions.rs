//! The user's own actions on the whole queue: pause/resume, clear, and
//! restoring the last removed batch.

use std::time::Instant;

use super::lifecycle::{ShellInput, dispatch_next_eligible};
use super::queue_sync::{current_shell_cwd, resume_cwd_warning_for_current_shell};
use super::status::RESUME_CWD_CONFIRM_WINDOW;
use super::{AppState, ShellState};
use crate::panel::StatusKind;

pub(super) fn toggle_queue_pause(state: &mut AppState, shell: &mut ShellInput<'_>) {
    let starting_queue = state.force_queue && matches!(state.shell_state, ShellState::AtPrompt);
    let was_paused = state.queue.paused || starting_queue;
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
            .is_some_and(|t| t.elapsed() <= RESUME_CWD_CONFIRM_WINDOW);
        if !confirmed {
            let warning = state.resume_cwd_warning.clone().unwrap_or_default();
            state.resume_cwd_confirmation_started_at = Some(Instant::now());
            state.set_status(warning);
            return;
        }
        if let Some(cwd) = current_shell_cwd(state).cloned() {
            state.queue.retarget_origin_cwd(cwd);
        }
        state.forget_resume_cwd_warning();
    }
    state.queue.paused = !was_paused;
    if starting_queue {
        state.force_queue = false;
    }
    state.activity = None;
    state.restored_queue_paused_locally = false;
    state.queue_dirty = true;
    if state.queue.paused {
        state.set_status_as(StatusKind::Paused, "queue paused");
        return;
    }
    state.set_status_as(StatusKind::Routine, "queue resumed");
    if was_paused && matches!(state.shell_state, ShellState::AtPrompt) && !state.queue.is_empty() {
        dispatch_next_eligible(state, None, shell);
    }
}

pub(super) fn clear_queue(state: &mut AppState) {
    if state.queue.is_empty() {
        state.pending_clear_at = None;
        state.set_status("queue is already empty");
        return;
    }
    if !state.pending_clear_active() {
        state.pending_clear_at = Some(Instant::now());
        let n = state.queue.len();
        let noun = if n == 1 { "command" } else { "commands" };
        state.set_status_as(
            StatusKind::Confirmation,
            format!("clear {n} queued {noun}? press ^K again to confirm"),
        );
        return;
    }
    state.pending_clear_at = None;
    state.recovery = state.queue.items().iter().cloned().enumerate().collect();
    state.activity = Some(format!(
        "Cleared {} commands — Alt-U restores paused",
        state.recovery.len()
    ));
    state.queue.clear();
    state.queue.paused = false;
    state.restored_queue_paused_locally = false;
    if let Some(cwd) = current_shell_cwd(state).cloned() {
        state.queue.set_origin_cwd(cwd);
    }
    state.forget_resume_cwd_warning();
    if state.editor.editing_index.is_some() {
        state.editor.finish_edit();
    }
    state.queue_dirty = true;
    state.set_status_as(StatusKind::Outcome, "queue cleared");
}

/// Restore only the latest removal batch. Fresh IDs avoid resurrecting an old
/// cross-session identity, and pausing prevents recovery from executing anything.
pub(super) fn restore_removed_commands(state: &mut AppState) {
    if state.editor.editing_index.is_some() {
        state.set_status("Save or cancel the edit before restoring commands");
        return;
    }
    if state.recovery.is_empty() {
        state.set_status("No removed commands to restore");
        return;
    }
    let recovered = std::mem::take(&mut state.recovery);
    let count = recovered.len();
    for (position, item) in recovered {
        let id = state
            .queue
            .push_with_origin(item.command, item.conditional, item.origin_cwd);
        let last = state.queue.len() - 1;
        for _ in position.min(last)..last {
            state.queue.move_up(id);
        }
    }
    state.queue.paused = true;
    state.queue_dirty = true;
    state.restored_queue_paused_locally = false;
    state.recheck_resume_cwd_warning();
    state.activity = Some(format!(
        "Restored {count} commands — queue paused; review before Ctrl-X resumes"
    ));
    state.set_status_as(StatusKind::Outcome, "restored to paused queue");
}

/// Drop a pending Ctrl-K confirmation, and the header prompt that asked for
/// it, so the panel doesn't keep asking after the user moved on.
pub(super) fn withdraw_pending_clear(state: &mut AppState) {
    state.pending_clear_at = None;
    if state.status_kind == StatusKind::Confirmation {
        state.clear_status();
    }
}
