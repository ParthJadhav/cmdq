# cmdq

[![CI](https://github.com/ParthJadhav/cmdq/actions/workflows/ci.yml/badge.svg)](https://github.com/ParthJadhav/cmdq/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/cmdq.svg)](https://crates.io/crates/cmdq)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> Type the next command while one is still running.

`cmdq` is a small terminal wrapper that hosts your shell in a pseudo-terminal
and gives you a separate "queue input" region below a separator. While a
command is running you can type the *next* command — review it, edit it,
reorder, cancel, or chain it conditionally — and it dispatches automatically
when the current one finishes.

Works best with zsh, bash, and fish. Plain POSIX `sh` can run inside `cmdq`,
but shells without prompt/preexec hooks have limited queue lifecycle detection.

## Why

In a normal terminal, when a command is running:

- You can't see what you're typing for the *next* command.
- You can't cancel or edit a typed-but-not-yet-executed command.
- You can't queue more than one.

`cmdq` fixes all three.

## Installation

### Shell (macOS & Linux) — recommended

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ParthJadhav/cmdq/releases/latest/download/cmdq-installer.sh | sh
```

Detects your OS / CPU, downloads the matching pre-built binary, and drops it
in `~/.cargo/bin` (or wherever `CARGO_HOME` points). No Rust toolchain
required.

### Homebrew (macOS & Linux)

```bash
brew install ParthJadhav/tap/cmdq
```

### From crates.io

```bash
cargo install cmdq
```

Requires Rust **1.88+** (edition 2024).

### Manual download

Grab the platform tarball from the
[latest release](https://github.com/ParthJadhav/cmdq/releases/latest)
and put `cmdq` somewhere on your `PATH`.

### From source

```bash
git clone https://github.com/ParthJadhav/cmdq.git
cd cmdq
cargo build --release
./target/release/cmdq
```

## Quick start

```bash
cmdq        # spawns $SHELL inside cmdq
```

The first run auto-loads OSC 133 prompt-marker hooks for zsh, bash, and fish
inside the `cmdq` session. To make the integration permanent in your normal
shell sessions too, run:

```bash
cmdq --install-integration
```

## CLI options

```bash
cmdq --shell /bin/zsh              # run cmdq with a specific shell
cmdq --install-integration         # install shell prompt markers into your rc file
cmdq --print-integration zsh       # print the zsh, bash, or fish integration
cmdq --help                        # show all flags
cmdq --version                     # show the installed version
```

## Shell setup

Pick the section for your shell. Each one shows how to install the OSC 133
integration and (optionally) auto-start `cmdq` whenever you open a terminal.

> **Why the `CMDQ_ACTIVE` guard?** `cmdq` itself spawns a child shell which
> sources your rc. Without the guard, that child would re-launch `cmdq` and
> loop forever. `exec` then replaces (rather than nests inside) your shell.

<details>
<summary><b>zsh</b></summary>

Install the integration into `~/.zshrc`:

```zsh
cmdq --install-integration
```

Auto-start in every terminal — append to `~/.zshrc`:

```zsh
[ -z "$CMDQ_ACTIVE" ] && exec cmdq
```

Reload:

```zsh
source ~/.zshrc
```

> macOS users: custom `ZDOTDIR` setups are supported. The auto-injected shim
> sources your real zsh startup files, then appends cmdq's prompt markers.

</details>

<details>
<summary><b>bash</b></summary>

Install the integration into `~/.bashrc`:

```bash
cmdq --install-integration
```

Auto-start in every terminal — append to your bash rc:

```bash
[ -z "$CMDQ_ACTIVE" ] && exec cmdq
```

Reload:

```bash
source ~/.bashrc
```

</details>

<details>
<summary><b>fish</b></summary>

Install the integration into `~/.config/fish/config.fish`:

```fish
cmdq --install-integration
```

Auto-start in every terminal — append to `~/.config/fish/config.fish`:

```fish
if not set -q CMDQ_ACTIVE
    exec cmdq
end
```

Reload:

```fish
source ~/.config/fish/config.fish
```

</details>

<details>
<summary><b>sh / dash / ash (POSIX)</b></summary>

POSIX shells can run inside `cmdq`, but they do not expose reliable prompt
and preexec hooks. Full queue automation requires zsh, bash, or fish.

You can still auto-start `cmdq` from a POSIX shell if you want the wrapper
available:

```sh
[ -z "$CMDQ_ACTIVE" ] && exec cmdq
```

Reload your profile:

```sh
. ~/.profile
```

</details>

<details>
<summary><b>Manual integration (zsh, bash, fish)</b></summary>

If you'd rather paste the integration snippet yourself (e.g. you manage your
dotfiles via a tool that doesn't like generated edits), print it and copy
into your rc:

```bash
cmdq --print-integration zsh    # or bash, fish
```

</details>

## Keybindings

`cmdq` starts in passthrough mode — keystrokes go directly to your shell. The
queue panel only appears once a command has been running for **1.5 seconds**,
so quick commands (`ls`, `cd`) don't flash UI. Press **F1** or **?** any time
the panel is visible for a full help overlay.

> **Note:** while the queue panel is open, **↑** recalls items from the
> *queue*, not your shell history. Double-tap `Esc` to enter raw input mode
> if you need shell history or want to scroll a pager (`git diff`, `less`).

**Add to queue**

| Key | Action |
|-----|--------|
| (any printable) | type into the input buffer |
| Enter | add the typed command to the queue |
| Tab | chain — only run if the previous command succeeded |
| Esc | clear the input buffer |

**Edit a queued item**

| Key | Action |
|-----|--------|
| ↑ / ↓ | open previous / next queued item for edit |
| Enter | save the edit |
| Esc | cancel the edit (item unchanged) |
| Ctrl-D | delete the item being edited |
| Alt-↑ / Alt-↓ | reorder the item being edited |

**Queue control**

| Key | Action |
|-----|--------|
| Ctrl-X | pause / resume auto-dispatch |
| Ctrl-K | clear the entire queue |

**Modes**

| Key | Action |
|-----|--------|
| Ctrl-Q | force the panel open even at the shell prompt |
| Esc Esc | raw input — keys go straight to the running app; double-tap again to return |
| Ctrl-\\ | send SIGQUIT to a running command; exits raw input when already raw |

**Misc**

| Key | Action |
|-----|--------|
| Ctrl-C | forward SIGINT to the running command (auto-pauses the queue) |
| Ctrl-Z | suspend the running command |
| Ctrl-D | quit cmdq (twice if the queue is non-empty) |
| Ctrl-A / Ctrl-E | beginning / end of input line |
| Ctrl-B / Ctrl-F | move one character left / right |
| Alt-B / Alt-F | move one word left / right |
| Alt-Left / Alt-Right | move one word left / right |
| Ctrl-H | backspace |
| Ctrl-U | kill back to start |
| Ctrl-W | delete previous word |
| F1 / ? | show / dismiss the help overlay |

## Smart behaviors

- **Panel only appears for long commands.** Anything finishing in under 1.5s
  runs without UI getting in the way. `Ctrl-Q` forces the panel open at the
  shell prompt if you want to queue ahead of time.
- **Auto-passthrough on alt-screen.** When the running program enters
  alt-screen mode (vim, htop, less, fzf, btop, …), `cmdq` automatically
  forwards keystrokes verbatim and hides the queue panel.
- **SIGINT auto-pauses the queue.** Ctrl-C on a running command (or exit
  status 130) pauses the queue instead of dispatching the next item.
- **Bracketed paste.** Pasting a multi-line snippet keeps heredocs, loops,
  and scripts intact while still landing as one queue item.
- **Quit-confirm.** Ctrl-D with a non-empty queue requires a second press.
- **Persistence.** The queue lives in `$XDG_DATA_HOME/cmdq/queue.json` when
  `$XDG_DATA_HOME` is set to an absolute path, otherwise your platform data
  directory, so a restart mid-session doesn't lose pending work. If a restored
  queue was saved from another working directory, `cmdq` keeps it paused and
  asks for an extra Ctrl-X before running it in the current shell. Corrupt
  queue files are backed up as `queue.json.corrupt-*` instead of overwritten.

## How it works

The shell emits `\e]133;A`, `\e]133;C`, `\e]133;D[;exitcode]` markers around
prompts and command execution. `cmdq` watches the PTY output stream for these
to decide whether keystrokes should pass through to the shell (at a prompt)
or be captured into the queue (when a command is running).

When the shell finishes a command (`\e]133;D`), `cmdq` writes the next queued
command's bytes back into the PTY master — same as if you'd typed it.

## Development

```bash
cargo build
cargo test            # unit + integration + binary smoke
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

## Caveats

- `cmdq` forwards shell output directly to your terminal instead of emulating
  a terminal. Colors, hyperlinks, scrollback, selection, OSC 52, and image
  protocols stay owned by the terminal.
- The auto-injected ZDOTDIR shim sources your real zsh startup files before
  appending the integration. If your zsh startup mutates global terminal state,
  test a fresh `cmdq --shell /bin/zsh` session before auto-starting it.
- POSIX `sh` support is best-effort because portable shells do not expose a
  reliable preexec hook. Use zsh, bash, or fish for full queue automation.

## License

[MIT](LICENSE)
