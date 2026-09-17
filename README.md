# cmdq

[![CI](https://github.com/ParthJadhav/cmdq/actions/workflows/ci.yml/badge.svg)](https://github.com/ParthJadhav/cmdq/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/cmdq.svg)](https://crates.io/crates/cmdq)

**Type the next command while one is still running.**

`cmdq` wraps your shell with a small command queue. While a build or another
command runs, write what comes next. Edit, reorder, pause, or make commands
conditional; they run automatically when the current command finishes.

## Install

**macOS and Linux** — no Rust required:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ParthJadhav/cmdq/releases/latest/download/cmdq-installer.sh | sh
```

Installs to `~/.cargo/bin` (or `$CARGO_HOME/bin`). Alternatively:

```sh
brew install ParthJadhav/tap/cmdq
```

With Rust **1.88+**:

```sh
cargo install cmdq --locked
```

[Prebuilt downloads](https://github.com/ParthJadhav/cmdq/releases/latest)
cover Apple Silicon and Intel Macs, and ARM64 and x64 Linux with glibc.
Windows and Alpine/musl are not supported.

## Use

```sh
cmdq                 # use $SHELL
cmdq --shell zsh     # or bash / fish
```

**zsh, bash, and fish support automatic queueing.** Integration loads after your
existing shell configuration; no dotfile changes are required. `sh`, `dash`,
and `ash` run in terminal passthrough mode with queueing disabled.

Run a command, then type the next one and press Enter. The queue panel appears
when you type or after a command has run for 1.5 seconds. Quick commands stay
out of the way. At an idle shell, Ctrl-Q opens a queue; Ctrl-X starts it.

| Key | Action in the queue panel |
| --- | --- |
| Enter | Add/run a command, or save an edit |
| ↑ / ↓ | Edit queued commands; preserve your draft |
| Alt-↑ / Alt-↓ | Move the command being edited |
| Esc | Clear the draft or cancel an edit |
| Ctrl-D | Delete the edited item; on an empty draft, quit (twice if queued) |
| Alt-S | Toggle “only if previous succeeds” |
| Ctrl-X | Pause, resume, or start the queue |
| Ctrl-K twice | Clear the queue |
| Alt-U | Restore the last removed/skipped batch, paused |
| F2 | Type into the running program / return to queue |
| Ctrl-C | Interrupt the running command and pause the queue |
| F1 | Show all shortcuts |

Interactive programs such as Vim and pagers receive their keys automatically.
Use F2 for an unrecognized input reader. Tab completion is available in your
shell; the queue editor does not provide completion.

Each session starts with an independent empty queue. Recovery restores commands
for review, not effects of commands already run, and lasts only for that session.
Exit the hosted shell with `exit` or Ctrl-D at an empty shell prompt.

## Configuration

cmdq reads `~/.config/cmdq/config.toml` (or
`$XDG_CONFIG_HOME/cmdq/config.toml`). The currently supported settings are:

```toml
[panel]
delay_ms = 1500
max_rows = 8

[keys]
forward_ctrl_x = false
```

`forward_ctrl_x = false` prevents an accidental Ctrl-X at an empty prompt from
arming a readline chord and consuming the next character. Ctrl-X is still
forwarded after prompt text has been entered. Set it to `true` if you rely on
readline's Ctrl-X chords from an empty prompt.

`CMDQ_PANEL_DELAY_MS`, `CMDQ_PANEL_MAX_ROWS`, and `CMDQ_FORWARD_CTRL_X`
override the matching file settings. Use `cmdq config --print` to inspect the
effective values and `cmdq --config /path/to/config.toml config --print` to
inspect another file.

## Shell setup (optional)

To start cmdq whenever you open a terminal, add the matching block **at the top**
of your shell configuration, before prompt frameworks:

**zsh** — `~/.zshrc` (or `$ZDOTDIR/.zshrc`):

```zsh
[[ -o interactive && -t 0 && -t 1 && -z "$CMDQ_ACTIVE" ]] && exec cmdq --shell zsh
```

**bash** — `~/.bashrc` (ensure your login profile sources it):

```bash
[[ $- == *i* && -t 0 && -t 1 && -z "$CMDQ_ACTIVE" ]] && exec cmdq --shell bash
```

**fish** — `~/.config/fish/config.fish` (or `$XDG_CONFIG_HOME/fish/config.fish`):

```fish
if status is-interactive; and isatty stdin; and isatty stdout; and not set -q CMDQ_ACTIVE
    exec cmdq --shell fish
end
```

The `CMDQ_ACTIVE` guard prevents recursive launches. Open a new terminal to use it.
For an optional persistent integration block, run
`cmdq --shell zsh --install-integration` (replace `zsh` with `bash` or `fish`).
Use `cmdq --print-integration zsh` to inspect the hooks.

## How it works

cmdq hosts your shell in a pseudo-terminal. Small shell hooks mark command
start and completion; cmdq uses those markers to route typing and dispatch the
next queued command. Your terminal still handles output, colors, scrollback,
links, and selection.

## Troubleshooting and removal

- **Command not found:** add `~/.cargo/bin` to your shell's PATH or open a new terminal after installing.
- **A program needs input:** press F2 while the panel is visible.
- **Queue paused:** save/cancel any edit, then press Ctrl-X. Changing directories may require confirmation.
- **Alt shortcuts on macOS:** configure your terminal's Option key to send Esc/Meta.
- **After upgrading:** start a new cmdq session; existing sessions keep their loaded binary.

To uninstall, remove any auto-start lines first. Run `brew uninstall cmdq` or
`cargo uninstall cmdq`, or remove the shell installer's `cmdq` binary from
`~/.cargo/bin`. If you installed persistent hooks, remove the block between
`# >>> cmdq shell integration >>>` and `# <<< cmdq shell integration <<<` in your shell config.

## Development

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
cargo install --path . --force --locked
```

See [release testing](docs/releasing.md) for the platform matrix and release process.

MIT licensed. Created by [Parth Jadhav](https://www.parthjadhav.com/).
