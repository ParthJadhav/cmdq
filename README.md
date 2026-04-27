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

Works with any shell (zsh, bash, fish, sh).

## Why

In a normal terminal, when a command is running:

- You can't see what you're typing for the *next* command.
- You can't cancel or edit a typed-but-not-yet-executed command.
- You can't queue more than one.

`cmdq` fixes all three.

## Installation

### From crates.io (recommended)

```bash
cargo install cmdq
```

Requires Rust **1.88+** (edition 2024).

### Pre-built binaries

Download the latest binary for your platform from the
[releases page](https://github.com/ParthJadhav/cmdq/releases).

```bash
# macOS / Linux example
curl -L https://github.com/ParthJadhav/cmdq/releases/latest/download/cmdq-$(uname -s)-$(uname -m).tar.gz | tar xz
sudo mv cmdq /usr/local/bin/
```

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

The first run auto-installs OSC 133 prompt-marker hooks for your shell into
a private rcfile. To make the integration permanent in your normal shell
sessions too, run:

```bash
cmdq --install-integration
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

> macOS users: if you use a custom `ZDOTDIR`, install integration manually
> with `--install-integration` rather than relying on the auto-injected shim.

</details>

<details>
<summary><b>bash</b></summary>

Install the integration into `~/.bashrc` (Linux) or `~/.bash_profile` (macOS):

```bash
cmdq --install-integration
```

Auto-start in every terminal — append to your bash rc:

```bash
[ -z "$CMDQ_ACTIVE" ] && exec cmdq
```

Reload:

```bash
source ~/.bashrc   # or ~/.bash_profile on macOS
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

Install the integration into `~/.profile`:

```sh
cmdq --install-integration
```

Auto-start in every terminal — append to `~/.profile`:

```sh
[ -z "$CMDQ_ACTIVE" ] && exec cmdq
```

Reload:

```sh
. ~/.profile
```

</details>

<details>
<summary><b>Manual integration (any shell)</b></summary>

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
> *queue*, not your shell history. Exit with `Ctrl-\` if you need shell history.

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
| Ctrl-\\ | raw input — keys go straight to the running app |

**Misc**

| Key | Action |
|-----|--------|
| Ctrl-C | forward SIGINT to the running command (auto-pauses the queue) |
| Ctrl-D | quit cmdq (twice if the queue is non-empty) |
| Ctrl-A / Ctrl-E | beginning / end of input line |
| Ctrl-U | kill back to start |
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
- **Bracketed paste.** Pasting a multi-line snippet collapses newlines into
  `;` so it lands as a single queue item.
- **Quit-confirm.** Ctrl-D with a non-empty queue requires a second press.
- **Persistence.** The queue lives in `~/.local/share/cmdq/queue.json`, so a
  restart mid-session doesn't lose pending work.

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

- `vt100` rendering is solid for normal CLI output, colors, and most TUIs,
  but may have small fidelity gaps for exotic sequences (sixel, kitty
  graphics). Use `Ctrl-\` to passthrough to the running program.
- The auto-injected ZDOTDIR shim sources your real `~/.zshrc` before
  appending the integration. If you have machinery that's strictly
  ZDOTDIR-aware, install integration manually via `--install-integration`.

## License

[MIT](LICENSE)
