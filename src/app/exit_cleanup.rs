//! Leave the user's terminal and cmdq's session files tidy however cmdq
//! exits: normal return, panic, or a terminating signal.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use anyhow::Result;

use super::screen::{SharedRestoreState, restore_terminal};
use crate::session_lease::LeaseRemover;

/// Restores the terminal when dropped (normal exit and unwinding) and from a
/// panic hook, which runs before stdout's buffered panel bytes would be
/// flushed on top of the restored terminal.
pub(super) struct TerminalGuard(SharedRestoreState);

impl TerminalGuard {
    pub(super) fn install(restore: SharedRestoreState) -> Self {
        let original_hook = std::panic::take_hook();
        let for_panic = restore.clone();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal(&for_panic);
            original_hook(info);
        }));
        Self(restore)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal(&self.0);
    }
}

/// What a terminating signal must tidy up before the process exits.
pub(super) struct SignalCleanup {
    pub(super) restore: SharedRestoreState,
    pub(super) lease: Option<LeaseRemover>,
    pub(super) session_dirs: Vec<PathBuf>,
    pub(super) queue_path: PathBuf,
    /// Commands still queued; the session directory is kept unless this is 0.
    pub(super) queued_count: Arc<AtomicUsize>,
}

#[cfg(unix)]
pub(super) type PendingSignals = signal_hook::iterator::Signals;
#[cfg(not(unix))]
pub(super) type PendingSignals = ();

#[cfg(unix)]
pub(super) fn prepare_signal_cleanup() -> Result<PendingSignals> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    PendingSignals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT]).map_err(Into::into)
}

#[cfg(not(unix))]
pub(super) fn prepare_signal_cleanup() -> Result<PendingSignals> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn install_signal_cleanup(mut signals: PendingSignals, cleanup: SignalCleanup) {
    use std::sync::atomic::Ordering;

    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _ = restore_terminal(&cleanup.restore);
            if let Some(lease) = cleanup.lease {
                lease.remove_for_exit();
            }
            for dir in &cleanup.session_dirs {
                let _ = std::fs::remove_dir_all(dir);
            }
            if cleanup.queued_count.load(Ordering::Acquire) == 0 {
                crate::queue::remove_session_dir(&cleanup.queue_path);
            }
            std::process::exit(128 + signal);
        }
    });
}

#[cfg(not(unix))]
pub(super) fn install_signal_cleanup(_signals: PendingSignals, _cleanup: SignalCleanup) {}
