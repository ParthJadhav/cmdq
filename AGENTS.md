# Local build and installation

- After making any changes, build and install the current checkout before finishing so the user's local setup picks them up.
- Run `cargo install --path . --force --locked` from the repository root. This builds the release binary and replaces the locally installed `cmdq`; `cargo build` alone is not sufficient.
- Verify that `command -v cmdq` resolves to the installed binary and that `cmdq --version` succeeds.
- If building or installing fails, fix the failure or clearly report the blocker. Do not claim the local setup is updated until installation succeeds.
- Existing running sessions keep their loaded binary; start a new `cmdq` session to use the update.

# Repository navigation

cmdq is a Rust PTY shell wrapper. Shell output streams to the real terminal;
the application routes input, dispatches queued commands, and paints a bottom panel.

| Task | Start with | Coverage |
| --- | --- | --- |
| CLI and configuration | `src/main.rs::Cli`, `src/config.rs::Config::load` | config unit tests, `tests/binary_smoke.rs` |
| Queue execution and recovery | `src/app.rs::dispatch_next_eligible`, `src/queue.rs::Queue` | app/queue unit tests, `tests/queue_dispatch_e2e.rs`, `tests/shell_matrix.rs` |
| Session isolation and cleanup | `src/queue.rs::new_session_path`, `remove_session_dir`, `src/session_lease.rs` | queue unit tests, binary smoke |
| Keys, drafts, and editing | `src/input.rs::LineEditor::handle_key`, app `handle_key_with_bytes` | input/app unit tests, shell matrix |
| Panel and resize | `src/panel.rs`, app `desired_layout` / `transition_layout`, `src/cursor_tracker.rs` | panel unit tests, `tests/path_a_smoke.rs` |
| Shell hooks and input ownership | `shell/integration.{bash,zsh,fish}`, `src/pty.rs`, `src/osc133.rs`, `src/mode_detect.rs`, `src/terminal_input.rs` | protocol unit tests, shell matrix, binary smoke |

- Read [the developer guide](docs/development.md) for the full map, lifecycle invariants, and test selection; follow [release testing](docs/releasing.md) for publication.
- Inventory with `rg --files src shell tests docs .github`; locate symbols with `rg -n`, then read bounded chunks. Whole-file dumps and combined batch output often exceed tool limits.
- README documents supported behavior; source/tests define implementation; CHANGELOG records shipped changes. The HTML walkthrough and UX review are historical snapshots. Check [UX implementation status](docs/ux-status.md) before treating a proposal as current work.
- Normal supported-shell launches own separate queue files. Keep atomic saves, locks, and rollback; recovery cannot undo shell effects.
- `demo/` is optional locally ignored media work. Search it explicitly for demo tasks; exclude its dependencies, captures, caches, and exports from general Rust exploration.
- Update this map when ownership changes and the UX status ledger when a proposal ships. Use symbol names rather than fixed source line numbers.
