# Changelog

## 0.3.2

Includes the unreleased 0.3.0 and 0.3.1 changes. Neither candidate was published.

### Added

- Independent queues for each terminal session. New sessions start empty and
  leave previous snapshots and the legacy shared queue untouched.
- Recover the last deleted, cleared, or skipped batch with Alt-U. Recovery keeps
  command order, working directories, and conditions, and pauses for review.
- F2 switches between typing into a running program and the queue while keeping
  the draft and cursor. F3 dismisses persistent outcome notices.
- Clearer panel states, contextual shortcuts, overflow indicators, and visible
  execution conditions.

### Changed

- Alt-S controls conditional execution. Tab shows a completion notice instead
  of silently changing whether a command runs.
- Editing, canceling, deleting, and navigating queued items preserve the draft.
  Unsaved edits must be saved or canceled before switching items.
- Ctrl-X starts a queue built at the shell prompt with one press.
- Condensed installation and shell-setup documentation.
- Updated Rust dependencies and GitHub Actions, retaining Rust 1.88 support.

### Fixed

- Wait for Bash to finish drawing its prompt before dispatching queued commands,
  preventing readline from restoring a stale, reduced terminal height.

- Split terminal control sequences remain intact even when a program or network
  pauses between fragments; panel rendering no longer corrupts delayed sequences.

- Starting an idle draft captures immediately typed follow-up characters before
  the shell's command-start marker arrives.
- Terminal mode checks remain compatible with the updated nix dependency and
  portable-pty's separately versioned terminal flags.

### Release verification

- Bound PTY test capture and apply backpressure so continuous-output stress
  tests check interruption without overwhelming their own output parser.

- Publishing requires CI and native archive tests on macOS and Linux, both
  x64/Intel and ARM64. Rust 1.88 is tested separately.
- Real-PTY regression coverage includes editing, recovery, paste, interruption,
  resizing, direct input, interactive programs, and independent sessions.
- Generated installers are checked before publication; published shell,
  Homebrew, and crates.io installations are tested afterward on all four targets.
