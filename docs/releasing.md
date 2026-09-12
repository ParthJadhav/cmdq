# Release testing

A tag is a release candidate until its CI and artifact tests pass. The Release
workflow calls CI on the same commit before building or publishing. Crates.io
publishing has no separate tag or manual trigger. GitHub release creation and
Homebrew publishing require the artifact checks; crates.io runs after that gate.

## Required matrix

| Platform | Native runner | Shells |
| --- | --- | --- |
| Linux x64 | Ubuntu 24.04 | bash, zsh, fish |
| Linux ARM64 | Ubuntu 24.04 ARM | bash, zsh, fish |
| macOS Apple Silicon | macOS 15 | bash, zsh, fish |
| macOS Intel | macOS 15 Intel | bash, zsh, fish |
| Minimum Rust 1.88 | Ubuntu 24.04 | bash, zsh, fish |

CI tests optimized builds, formatting, Clippy, documentation, and the packaged
crate. `CMDQ_REQUIRE_SHELL_MATRIX=1` makes missing supported shells a failure.

The PTY suite checks actual file effects and command exit statuses, including:

- Queue ordering, conditional execution, editing, reordering, deletion, and undo.
- Draft preservation after editing, running a command, direct input, and resizing.
- Clear confirmation, paused recovery, interruption, and independent sessions.
- Multiline paste, heredocs, Unicode input, and shell continuation prompts.
- Vim edits, less, Git's pager, hidden input, raw readers, and tmux.
- Terminal queries/replies, keyboard and mouse protocols, alternate screens,
  resize storms, tiny terminals, signal cleanup, and shell exit status.
- Clean shell homes, startup customization, symlinked dotfiles, and POSIX passthrough.
- Bash prompt-readiness synchronization and a deterministic stale terminal-size regression.

Tests create temporary homes and files; they do not change personal shell configs.
An ignored throughput benchmark is diagnostic, not a correctness gate.

## Before publication

1. Update Cargo.toml, Cargo.lock, and CHANGELOG.md; commit all release changes.
2. Run local checks and install the checkout:

   ```sh
   CMDQ_REQUIRE_SHELL_MATRIX=1 cargo test --locked --release
   cargo clippy --locked --all-targets --all-features -- -D warnings
   cargo fmt --all -- --check
   cargo package --locked
   cargo install --path . --force --locked
   command -v cmdq
   cmdq --version
   ```

3. Open a release PR and wait for CI and the archive/installer checks to pass.
   Pull requests build and test artifacts without publishing them. Merge only
   after these checks pass.
4. Tag the same commit, e.g. `git tag v0.3.3`, and push that tag.
5. The release workflow reruns CI, builds all four archives, verifies their
   checksums, and runs the PTY suites against each **extracted release binary**.
   It also tests the generated shell installer and repeat installation using a
   local artifact server. Any failure blocks publication.

`CMDQ_TEST_BINARY=/absolute/path/to/cmdq` selects a packaged or installed binary
for `shell_matrix`, `binary_smoke`, and `path_a_smoke`. This prevents tests from
silently exercising a source build when checking a release archive.

## After publication

The workflow verifies shell installation, upgrading from v0.2.1, repeat
installation, crates.io installation, and Homebrew installation on all four
native platforms. Each installation must report the tagged version and pass
the real-shell matrix. The final announcement job requires these checks.

If a source defect appears, add a reproducing regression test, fix it, rerun the
matrix, and issue a patch version. Never replace a published tag or crate.
Infrastructure failures should be corrected and rerun without disguising them
as product fixes. Preserve custom gates when updating the cargo-dist workflow.

The supported release targets are macOS and glibc Linux. These automated tests
exercise terminal protocols with real PTYs; they cannot guarantee identical
rendering in every terminal application or compatibility with every dotfile
configuration. New reported failures should become reproducible tests here.
