# Developer guide

cmdq is a Rust PTY shell wrapper with a bottom panel. Shell output streams to
the real terminal. The application coordinates input ownership, shell markers,
queue dispatch and layout.

## Code map

| Task | Start with | Relevant coverage |
| --- | --- | --- |
| CLI flags and configuration | `src/main.rs::Cli`, `src/config.rs::Config::load` | config unit tests; `tests/binary_smoke.rs` |
| Main loop and session startup | `src/app.rs::run_with_config_exit_status`, `AppState` | app unit tests; binary smoke |
| Queue dispatch, conditionals, rollback | app `dispatch_next_eligible`, `src/queue.rs::Queue`, shell integration scripts | `tests/queue_dispatch_e2e.rs`, shell matrix, app/queue unit tests |
| Session isolation and queue cleanup | queue `new_session_path`, `remove_session_dir`, `sweep_stale_queue_dirs_at`, `src/session_lease.rs` | queue unit tests; binary smoke |
| Keys, drafts, edit/reorder/delete | `src/input.rs::LineEditor::handle_key`, app `handle_key_with_bytes` | input/app unit tests; shell matrix |
| Panel strings, clipping, help | panel `paint`, `paint_header`, `paint_hints`, `input_line`, `paint_help` | panel unit tests; binary smoke |
| Resize and scroll regions | app `desired_layout`, `transition_layout`, `resize_pty_for_layout`; panel `reserve`, `release`; `src/cursor_tracker.rs` | `tests/path_a_smoke.rs`, binary smoke |
| Child input and terminal protocol routing | `src/terminal_input.rs::Reader`, `src/terminal_input/parse.rs`, `src/mode_detect.rs`, app `refresh_auto_passthrough_for_child_modes` | protocol unit tests; path-A smoke; shell matrix |
| Shell hooks and shell startup | `shell/integration.{bash,zsh,fish}`, `src/shell_integration.rs`, `src/pty.rs::ShellPty::spawn`, `src/osc133.rs::Detector` | shell matrix; binary smoke; integration PTY tests |
| Release/platform changes | [release testing](releasing.md), `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `dist-workspace.toml` | release gates; installer verification script |

Paths in the map are relative to the repository root. Implementation unit
tests live in each module's `#[cfg(test)]` block. Find the production symbol
first, then search the test names rather than reading the entire file.

## Document authority

- [README](../README.md): supported user behavior and configuration.
- Source and regression tests: implementation details.
- [CHANGELOG](../CHANGELOG.md): behavior shipped in each version.
- [Release testing](releasing.md): release checks and installation process.
- [Walkthrough](walkthrough.html): historical architecture narrative; persistence, rendering and key details may have changed.
- [UX review](ux-improvements.html): September 2026 proposals against v0.3.3. Check [implementation status](ux-status.md) before implementing; the full config example is a proposal, not an accepted schema.
- [September 29 bugbash](bugbash-2026-09-29.md): dated findings and regression-test pointers.
- `demo/`: optional locally ignored media projects, not required for Rust development or necessarily present in another clone. Local variants include the original Remotion project and Diffusion Studio projects in `promo/`, `departures/`, and `upbeat/`; inspect each project's brief, package scripts and capture version before reuse.

## Search narrowly

```sh
rg --files src shell tests docs .github
rg -n 'dispatch_next_eligible|handle_osc_event' src/app.rs
rg -n 'conditional|rollback' tests src/queue.rs
```

Read the relevant symbol and its tests in bounded chunks. Whole-file reads of
app, queue, panel and binary smoke frequently exceed tool output limits. Keep
combined batch output bounded too. Prefer symbols to fixed line ranges in
maintained docs. Search demo explicitly for media tasks; exclude its dependencies,
generated captures, caches and exports from ordinary Rust exploration.

## Test selection

| Change | Start with | Broaden when relevant |
| --- | --- | --- |
| Pure config/parser/model/render helpers | `cargo test --locked --lib <filter>` | Affected binary/PTY regression |
| Queue dispatch | `cargo test --locked --test queue_dispatch_e2e` | Required real-shell matrix |
| Shell hooks, startup, interactive programs | `CMDQ_REQUIRE_SHELL_MATRIX=1 cargo test --locked --release --test shell_matrix` | Binary smoke and release matrix |
| Scroll regions, resize, terminal replies | `cargo test --locked --test path_a_smoke` | Binary smoke and shell matrix |
| Binary behavior/lifecycle | `cargo test --locked --test binary_smoke <filter>` | Related module tests |
| Publication | Follow [release testing](releasing.md) | All required native platforms and extracted release artifacts |

Replace `<filter>` with the relevant test-name substring. These commands are
starting points for focused development; the release checklist defines the full gate.

The shell matrix uses isolated homes and temporary files, and models terminal
replies. Missing shells can be skipped unless `CMDQ_REQUIRE_SHELL_MATRIX=1` is
set. Install dependencies from `.github/actions/test-dependencies/action.yml`;
the suite uses bash, zsh, fish and interactive programs including Vim, less and
tmux. Binary smoke, path-A smoke and shell matrix honor
`CMDQ_TEST_BINARY=/absolute/path/to/cmdq` when validating an installed or packaged
binary. `tests/common/mod.rs` selects that binary; local tests otherwise use
Cargo's freshly built executable.

Run the appropriate checks after implementation. Install every changed checkout
with `cargo install --path . --force --locked`, verify `command -v cmdq` and
`cmdq --version`, and use a new session to load the updated binary.

## Session lifecycle invariants

- Supported-shell launches allocate independent queue files; do not derive session identity from inherited environment, cwd or parent PID.
- The legacy `try_default_path` helper is not the queue identity for normal supported-shell launches.
- Queue saves/claims use locks, atomic writes and rollback; retain them even though shell sessions are independent.
- Input ownership combines shell lifecycle markers, child modes, termios and explicit switching. Queue-editor keys and child input must be routed deliberately.
- Recovery restores pending commands paused for review; it cannot reverse or prove absence of shell side effects.
- Empty session directories are cleaned on exit, and startup sweeping protects active/new sessions. Cross-session orphan reattachment remains a proposal.

## Keep navigation current

When changing an ownership boundary, update the maps here and in
[AGENTS.md](../AGENTS.md). When shipping a UX proposal, update its item ID in
[UX implementation status](ux-status.md) with the implemented subset, release
and regression-test reference. Historical captured states should retain their
original version labels.
