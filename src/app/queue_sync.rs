//! Keeping this session's queue and its file on disk in step: saving local
//! changes, merging changes other sessions made, and warning when queued
//! commands were saved in a different directory than the shell is in now.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::AppState;
use crate::panel::StatusKind;
use crate::queue::{self, Queue};

/// How often an idle session checks its own persisted queue file for changes.
pub(super) const QUEUE_SYNC_INTERVAL: Duration = Duration::from_millis(500);

pub(super) const DEFERRED_QUEUE_CHANGE_STATUS: &str =
    "queue changed in another session; finish or cancel edit to merge";

/// A queue restored at startup, paused for review.
pub(super) struct RestoredQueue {
    pub(super) status: Option<String>,
    pub(super) resume_cwd_warning: Option<String>,
    /// The pause is this session's own: the file on disk was not paused.
    pub(super) paused_locally: bool,
}

pub(super) fn prepare_queue_for_startup(
    queue: &mut Queue,
    session_cwd: Option<&Path>,
    active_peer_count: usize,
) -> RestoredQueue {
    let restored = queue.len();
    if restored == 0 {
        return RestoredQueue {
            status: None,
            resume_cwd_warning: None,
            paused_locally: false,
        };
    }

    let paused_locally = !queue.paused;
    queue.paused = true;
    let noun = if restored == 1 { "command" } else { "commands" };
    // Keep the actions first and terse: at 80 columns the header has room
    // for roughly 70 characters, and the tail is what gets clipped.
    let peer_note = active_peer_note(active_peer_count);
    let resume_cwd_warning =
        session_cwd.and_then(|current| resume_cwd_warning_for_queue(queue, current));
    let saved_in = match (session_cwd, &resume_cwd_warning) {
        (Some(current), Some(_)) => format!(
            " · saved in {}",
            compact_origin_summary(&queue.mismatched_origins(current))
        ),
        _ => String::new(),
    };
    RestoredQueue {
        status: Some(format!(
            "restored {restored} {noun} — ^X resume, ^K clear{saved_in}{peer_note}"
        )),
        resume_cwd_warning,
        paused_locally,
    }
}

fn active_peer_note(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " · another session active".to_string(),
        n => format!(" · {n} other sessions active"),
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

pub(super) fn current_shell_cwd(state: &AppState) -> Option<&PathBuf> {
    state.shell_cwd.as_ref().or(state.session_cwd.as_ref())
}

pub(super) fn resume_cwd_warning_for_current_shell(state: &AppState) -> Option<String> {
    resume_cwd_warning_for_queue(&state.queue, current_shell_cwd(state)?)
}

pub(super) fn dispatch_cwd_warning_for_item(
    queue: &Queue,
    item: &queue::QueueItem,
    current: &Path,
) -> Option<String> {
    let origin = item.origin_cwd.as_deref().or(queue.origin_cwd())?;
    if origin == current {
        return None;
    }
    cwd_warning(&[origin.to_path_buf()], current, "Ctrl-X to run here")
}

fn resume_cwd_warning_for_queue(queue: &Queue, current: &Path) -> Option<String> {
    cwd_warning(
        &queue.mismatched_origins(current),
        current,
        "press Ctrl-X again to run here",
    )
}

fn cwd_warning(origins: &[PathBuf], current: &Path, action: &str) -> Option<String> {
    match origins {
        [] => None,
        [origin] => Some(format!(
            "queue was saved in {}; {action} ({})",
            compact_path(origin),
            compact_path(current)
        )),
        _ => Some(format!(
            "queue includes commands from {}; {action} ({})",
            compact_origin_summary(origins),
            compact_path(current)
        )),
    }
}

/// Persist local queue changes, merging what other sessions wrote meanwhile.
pub(super) fn save_queue_if_dirty(state: &mut AppState) {
    let Some(queue_path) = state.queue_path.clone() else {
        return;
    };
    if !state.queue_dirty {
        return;
    }
    match state.queue.save_preserving_unseen(
        &queue_path,
        &mut state.queue_known_items,
        &mut state.queue_known_paused,
    ) {
        Ok(merge) => {
            state.queue_dirty = false;
            state.queue_external_change_pending = false;
            state.queue_save_error = None;
            apply_queue_merge_status(state, merge, None);
        }
        Err(e) => report_queue_error(state, "queue save failed", &e),
    }
}

/// Show a queue file error once; retries keep failing silently until the
/// error changes or the file works again.
fn report_queue_error(state: &mut AppState, context: &str, error: &anyhow::Error) {
    let msg = error.to_string();
    if state.queue_save_error.as_deref() != Some(msg.as_str()) {
        state.set_status(format!("{context}: {msg}"));
        state.queue_save_error = Some(msg);
    }
}

/// Adopt changes other sessions made to this queue's file, unless a local
/// change is unsaved or a draft/edit would be disturbed (then only say so).
pub(super) fn sync_queue_from_disk_if_due(state: &mut AppState, last_sync: &mut Instant) {
    let Some(queue_path) = state.queue_path.clone() else {
        return;
    };
    if last_sync.elapsed() < QUEUE_SYNC_INTERVAL {
        return;
    }
    if state.queue_dirty {
        return;
    }
    if state.draft_pending() {
        *last_sync = Instant::now();
        notify_deferred_queue_change_if_needed(state, &queue_path);
        return;
    }
    *last_sync = Instant::now();

    let before_items = state.queue.item_snapshot();
    let before_paused = state.queue.paused;
    let keep_local_startup_pause = state.restored_queue_paused_locally && state.queue.paused;
    let known_paused_before_sync = state.queue_known_paused;
    match state.queue.sync_from_disk(
        &queue_path,
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
                    state.forget_resume_cwd_warning();
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
        Err(e) => report_queue_error(state, "queue sync failed", &e),
    }
}

fn notify_deferred_queue_change_if_needed(state: &mut AppState, queue_path: &Path) {
    match Queue::disk_differs_from_known(
        queue_path,
        &state.queue_known_items,
        state.queue_known_paused,
    ) {
        Ok(true) => {
            state.queue_save_error = None;
            if !state.queue_external_change_pending {
                state.queue_external_change_pending = true;
                state.set_status(DEFERRED_QUEUE_CHANGE_STATUS);
            }
        }
        Ok(false) => {
            if state.queue_external_change_pending && state.status == DEFERRED_QUEUE_CHANGE_STATUS {
                state.clear_status();
            }
            state.queue_external_change_pending = false;
            state.queue_save_error = None;
        }
        Err(e) => report_queue_error(state, "queue sync waiting for edit", &e),
    }
}

/// While a change from another session waits for the user's edit, keep
/// saying so instead of letting the status fade.
pub(super) fn deferred_queue_change_status_if_pending(state: &AppState) -> Option<&'static str> {
    state
        .queue_external_change_pending
        .then_some(DEFERRED_QUEUE_CHANGE_STATUS)
}

fn apply_queue_merge_status(
    state: &mut AppState,
    merge: queue::SaveMerge,
    fallback_status: Option<&'static str>,
) {
    if let Some(warning) = merge.warning {
        state.set_status(warning);
    } else if merge.external_pause {
        state.set_status_as(StatusKind::Paused, "queue paused by another session");
    } else if merge.external_resume {
        state.recheck_resume_cwd_warning();
        state.set_status_as(StatusKind::Routine, "queue resumed by another session");
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
            state.recheck_resume_cwd_warning();
            state.set_status(format!(
                "merged {} queued {noun} from another session; queue paused",
                merge.unseen_items
            ));
        }
    } else if let Some(status) = fallback_status {
        state.set_status(status);
    }
}
