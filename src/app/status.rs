//! The one-line status message in the panel header, and the confirmation
//! windows it explains.

use std::time::{Duration, Instant};

use super::AppState;
use super::queue_sync::DEFERRED_QUEUE_CHANGE_STATUS;
use crate::panel::StatusKind;

/// Status messages ("added: ls", "queue cleared", …) fade after this so the
/// header isn't a permanent log of the last action. Long enough to read a
/// full sentence, and never shorter than any confirmation window it explains.
pub(super) const STATUS_TTL: Duration = Duration::from_secs(4);

/// A second Ctrl-D within this window discards a non-empty queue and quits.
pub(super) const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// Ctrl-K throws away every queued command with no undo, and it doubles as a
/// readline kill-line reflex. Ask for a second press within this window.
pub(super) const CLEAR_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// Running a restored queue from a different directory is high-risk enough to
/// require two close Ctrl-X presses, like a small "are you sure?" gesture.
pub(super) const RESUME_CWD_CONFIRM_WINDOW: Duration = Duration::from_secs(4);

const _: () = assert!(
    STATUS_TTL.as_millis() >= QUIT_CONFIRM_WINDOW.as_millis()
        && STATUS_TTL.as_millis() >= CLEAR_CONFIRM_WINDOW.as_millis()
        && STATUS_TTL.as_millis() >= RESUME_CWD_CONFIRM_WINDOW.as_millis()
);

impl AppState {
    pub(super) fn set_status(&mut self, text: impl Into<String>) {
        self.set_status_as(StatusKind::Notice, text);
    }

    pub(super) fn set_status_as(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status = text.into();
        self.status_kind = kind;
        self.status_set_at = Some(Instant::now());
    }

    pub(super) fn clear_status(&mut self) {
        self.status.clear();
        self.status_kind = StatusKind::Notice;
        self.status_set_at = None;
    }

    pub(super) fn tick_status(&mut self) {
        if !self
            .status_set_at
            .is_some_and(|set_at| set_at.elapsed() >= STATUS_TTL)
        {
            return;
        }
        // While a change from another session waits for the user's edit,
        // keep saying so instead of letting the status fade.
        if self.queue_external_change_pending {
            self.set_status(DEFERRED_QUEUE_CHANGE_STATUS);
        } else {
            self.clear_status();
        }
    }
}

/// One line of a command for the status header: line breaks shown as ⏎,
/// long commands cut with an ellipsis.
pub(super) fn truncate_for_status(s: &str) -> String {
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
