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

Automatic queueing supports **zsh, bash, and fish** on macOS and Linux.
Other shells, including `sh`, `dash`, and `ash`, run in explicitly announced
terminal passthrough mode: they keep their normal keys and do not capture or
execute a queue.

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

No shell configuration changes are needed. Every session loads the integration
after your existing zsh, bash, or fish configuration. Your normal shell remains
available by typing `exit` or pressing Ctrl-D at an empty shell prompt.

For an optional managed source line in your shell configuration, run:

```bash
cmdq --install-integration
```

The installed hooks activate only inside cmdq. Re-running the installer updates
the managed block and preserves symlinks in your dotfiles.

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
>
> **Put the auto-start line at the very top of your rc file.** Prompt
> frameworks such as Powerlevel10k's instant prompt redirect stdin and stdout
> while the rest of the rc loads, which makes the `-t 0 -t 1` terminal checks
> fail and silently skips the `exec`. Starting `cmdq` first also means only the
> shell inside `cmdq` pays the cost of loading your full configuration.

<details>
<summary><b>zsh</b></summary>

Install the integration into `~/.zshrc`:

```zsh
cmdq --shell zsh --install-integration
```

Auto-start in every terminal — add as the **first line** of `~/.zshrc`
(above any Powerlevel10k instant-prompt block):

```zsh
[[ -o interactive && -t 0 && -t 1 && -z "$CMDQ_ACTIVE" ]] && exec cmdq --shell zsh
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
cmdq --shell bash --install-integration
```

Auto-start in every terminal — add as the first line of your bash rc:

```bash
[[ $- == *i* && -t 0 && -t 1 && -z "$CMDQ_ACTIVE" ]] && exec cmdq --shell bash
```

Reload:

```bash
source ~/.bashrc
```

</details>

<details>
<summary><b>fish</b></summary>

Install into `$XDG_CONFIG_HOME/fish/config.fish`, or
`~/.config/fish/config.fish` when `XDG_CONFIG_HOME` is unset:

```fish
cmdq --shell fish --install-integration
```

Auto-start in every terminal — add at the top of your `config.fish`:

```fish
if status is-interactive; and isatty stdin; and isatty stdout; and not set -q CMDQ_ACTIVE
    exec cmdq --shell fish
end
```

Reload:

```fish
source $__fish_config_dir/config.fish
```

</details>

<details>
<summary><b>sh / dash / ash (POSIX)</b></summary>

These shells run with normal terminal passthrough and an explicit notice that
queueing is unavailable. No queue keys are intercepted, and saved queues are
left alone. To use automatic queueing from a POSIX shell, start a supported
shell explicitly:

```sh
cmdq --shell bash
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
so quick commands (`ls`, `cd`) don't flash UI. Typing the next command opens
the panel immediately while a normal command is running. Press **F1** or **?**
while the panel is visible for a full help overlay.

> **Note:** while the queue panel is open, **↑** recalls items from the
> *queue*, not your shell history. Interactive applications receive their keys
> automatically. Double-tap `Esc`
> while the panel is visible to send input to a program that cmdq has not
> recognized, such as a plain `cat` waiting for text without a prompt.

**Add to queue**

| Key | Action |
|-----|--------|
| (any printable) | type into the input buffer |
| Enter | add the typed command to the queue |
| Tab | toggle chaining for the draft; then Enter to enqueue |
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
| Ctrl-K | clear the entire queue (press twice to confirm) |

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
| Ctrl-D | quit cmdq (twice if the queue is non-empty); with text in the input, delete the character under the cursor |
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
- **Direct input without an alternate screen.** Raw/cbreak readers, REPLs,
  `less -X`, SSH, and programs that disable echo receive input directly; the
  panel stays hidden while they own terminal input. Recognizable line-input
  prompts are also passed through. An unlabelled canonical reader can still
  require the manual Esc Esc override.
- **Terminal replies reach the child.** Capability, cursor-position, color,
  and other terminal responses pass through, including modern Fish's startup
  negotiation. Application cursor keys and mouse encodings are preserved.
- **SIGINT auto-pauses the queue.** Ctrl-C on a running command (or exit
  status 130) pauses the queue instead of dispatching the next item.
- **Bracketed paste.** Pasting a multi-line snippet keeps heredocs, loops,
  and scripts intact while still landing as one queue item.
- **Confirm before losing work.** Ctrl-D with a non-empty queue and Ctrl-K
  (clear queue) both require a second press within a few seconds.
- **Drafts survive fast commands.** If the running command finishes while you
  are still typing the next one, the panel stays open with your draft. Enter
  runs it right away on the now-idle shell; Esc discards it.
- **Long queues say so.** When more commands are queued than fit in the panel,
  the header shows `12 queued, showing 1–8`.
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

The injected markers carry a `cmdq=1` property so native Fish markers and
other terminal integrations cannot dispatch the same queue twice.

When the shell finishes a command (`\e]133;D`), `cmdq` writes the next queued
command's bytes back into the PTY master — same as if you'd typed it.

## Development

```bash
cargo build
cargo test            # unit + integration + binary smoke
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Install zsh, bash, fish, Vim, less, and Git to exercise the full shell matrix.
CI installs these dependencies and requires all three supported shells:

```bash
CMDQ_REQUIRE_SHELL_MATRIX=1 cargo test --test shell_matrix
```

The matrix uses temporary homes and actual PTYs to test conditional queueing,
Vim file edits, pagers, hidden input, terminal replies, exit statuses, and
interrupting continuous output. On Unix the input transport retains terminal
replies; its key decoder is adapted from MIT-licensed crossterm 0.29.0 (license
and provenance are included in `src/terminal_input/parse.rs`).

## Caveats

- `cmdq` forwards shell output directly to your terminal instead of emulating
  a terminal. Colors, hyperlinks, scrollback, selection, OSC 52, and image
  protocols stay owned by the terminal.
- The auto-injected ZDOTDIR shim sources your real zsh startup files before
  appending the integration. If your zsh startup mutates global terminal state,
  test a fresh `cmdq --shell /bin/zsh` session before auto-starting it.
- Automatic queueing requires zsh, bash, or fish. Other shells use transparent
  passthrough, and arbitrary unlabelled input prompts may need Esc Esc.
- cmdq requires an interactive terminal on both stdin and stdout. It preserves
  the hosted shell's exit status and restores terminal state on exit.

## License

[MIT](LICENSE)

## Author

Created by [Parth Jadhav](https://www.parthjadhav.com/).
