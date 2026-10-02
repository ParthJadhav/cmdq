# UX implementation status

The [UX review](ux-improvements.html) records observations and proposals against
v0.3.3 on 13 September 2026. Its source line numbers, captures, problem statements,
and roadmap describe that snapshot, not the current release.

The entries below were checked against v0.4.1 on 3 October 2026. This ledger
covers the three items reviewed during the navigation retrospective; absence
from the table does not mean an item is unimplemented. Check source, tests and
[CHANGELOG](../CHANGELOG.md) before scheduling any remaining proposal.

| Item | Status | Shipped behavior | Remaining scope | Implementation and coverage |
| --- | --- | --- | --- | --- |
| [14: Ctrl-X at an empty prompt](ux-improvements.html#i14) | Shipped in v0.4.0 | Empty-prompt Ctrl-X is absorbed by default. Prompt chords remain available after typing; `keys.forward_ctrl_x` opts into forwarding from an empty prompt. | None for the proposed empty-prompt fix. | `src/app.rs::handle_key_with_bytes`; tests `ctrl_x_at_an_empty_prompt_is_absorbed`, `ctrl_x_forwarding_can_be_configured_or_preserved_for_a_prompt_chord` |
| [29: queue-directory cleanup](ux-improvements.html#i29) | Shipped in v0.4.0; startup protection improved in v0.4.1 | Empty session directories are removed on normal exit and SIGTERM. Startup sweeping preserves active sessions and gives new directories a grace period; recent nonempty snapshots remain, while orphans older than seven days can be removed. | Item 28's cross-session recovery offer remains a separate proposal. | queue `remove_session_dir`, `sweep_stale_queue_dirs_at`; app shutdown and `install_signal_cleanup`; tests `stale_queue_sweep_removes_empty_and_old_orphans`, `stale_queue_sweep_keeps_an_active_empty_session`, `sweep_preserves_a_session_before_its_lease_is_written`; binary smoke |
| [33: configuration](ux-improvements.html#i33) | Partially shipped in v0.4.0 | TOML configuration, `--config`, `cmdq config --print`, panel delay, visible-row limit, Ctrl-X forwarding, and their environment overrides. | Compact/startup-hint settings, queue policy, notifications, arbitrary key bindings and themes in the proposed schema are not supported. | `src/config.rs::Config::load`, `src/main.rs::Cli`; config unit tests and `cli_prints_effective_config_with_environment_overrides` |

## Supported configuration

Use the [README configuration example](../README.md#configuration) or
`cmdq config --print` for the accepted schema. Unknown fields reject the file
and cause a warning and fallback to defaults. Copying the broader proposed
schema from item 33 can therefore discard otherwise valid custom settings.

## Updating this ledger

When a proposal changes, record its item ID, shipped release, implemented
subset, remaining work, and source/test symbols. Use proposed, partially
shipped, shipped, or superseded as appropriate. Preserve the historical HTML
observations and capture versions; add status notes rather than treating their
old problem statements as current defects.
