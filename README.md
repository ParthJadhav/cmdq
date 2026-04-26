# cmdq — type the next command while one is still running

`cmdq` is a small terminal wrapper that hosts your shell in a pseudo-terminal
and gives you a separate "queue input" region below a separator. While a
command is running you can type the *next* command — review it, edit it,
reorder, cancel, or chain it conditionally — and it will dispatch
automatically when the current one finishes.

Works with any shell (zsh, bash, fish, sh). Not a zsh plugin: a plugin can
not display anything or capture keys while a foreground command holds the
TTY, so cmdq sits one layer above the shell instead.

## Why

In a normal terminal, when a command is running:

- You can't see what you're typing for the *next* command (input bleeds
  into the running command's stdin or the running command's output).
- You can't cancel or edit a typed-but-not-yet-executed command.
- You can't queue more than one.

cmdq fixes all three.

## Quick start

```bash
cargo build --release
./target/release/cmdq        # spawns $SHELL inside cmdq
```

The first run auto-installs OSC 133 prompt-marker hooks for your shell into
a private rcfile (so detection of "is the shell at a prompt vs running a
command" is reliable). To make the integration permanent in your normal
shell sessions too:

```bash
./target/release/cmdq --install-integration
```

### Auto-starting cmdq for every terminal

If you want cmdq to run automatically whenever you open a terminal, add it to
your shell rc — but **guard it against recursion** and use `exec` so cmdq
replaces (rather than nests inside) your shell:

```bash
# in ~/.zshrc, ~/.bashrc, etc.
[ -z "$CMDQ_ACTIVE" ] && exec /path/to/cmdq
```

Without the guard, the inner shell that cmdq itself spawns will source the
same rc file and try to start cmdq again, looping forever. cmdq detects this
and refuses to start when `CMDQ_ACTIVE` is already set, but the guard above
is still the right pattern.

## Keybindings

cmdq starts in passthrough mode — keystrokes go directly to your shell, the
queue is invisible. The queue panel only appears when a command has been
running for more than **1.5 seconds**, so quick commands (`ls`, `cd`) don't
flash UI. Press **F1** or **?** any time the panel is visible for the full
help overlay; a context-aware hint line is always visible at the bottom.

> **Note:** while the queue panel is open, **↑** recalls items from the
> *queue*, not your shell history. Shell history isn't reachable from
> queue mode — exit with `Ctrl-\` (raw input) if you need it.

**add to queue**

| Key | Action |
|-----|--------|
| (any printable) | type into the input buffer |
| Enter | add the typed command to the queue |
| Tab | chain — only run if the previous command succeeded |
| Esc | clear the input buffer |

**edit a queued item**

| Key | Action |
|-----|--------|
| ↑ / ↓ | open previous / next queued item for edit |
| Enter | save the edit |
| Esc | cancel the edit (item unchanged) |
| Ctrl-D | delete the item being edited |
| Alt-↑ / Alt-↓ | reorder the item being edited |

**queue control**

| Key | Action |
|-----|--------|
| Ctrl-X | pause / resume auto-dispatch |
| Ctrl-K | clear the entire queue |

**modes**

| Key | Action |
|-----|--------|
| Ctrl-Q | force the panel open even at the shell prompt |
| Ctrl-\\ | raw input — keys go straight to the running app |

**misc**

| Key | Action |
|-----|--------|
| Ctrl-C | forward SIGINT to the running command (auto-pauses the queue) |
| Ctrl-D | quit cmdq (twice if the queue is non-empty) |
| Ctrl-A / Ctrl-E | beginning / end of input line |
| Ctrl-U | kill back to start |
| F1 / ? | show / dismiss the help overlay |

## Smart behaviors

- **Panel only appears for long commands.** Anything finishing in under
  1.5 seconds runs without UI getting in the way. `Ctrl-Q` forces the
  panel open at the shell prompt if you want to queue ahead of time.
- **Auto-passthrough on alt-screen.** When the running program enters
  alt-screen mode (vim, htop, less, fzf, btop, …), cmdq automatically
  forwards keystrokes verbatim and hides the queue panel. When the program
  exits alt-screen, queue mode resumes. No manual toggling needed for the
  common case. `Ctrl-\` is still available for non-alt-screen interactive
  programs (Python REPL, `cat`, raw `nc`).
- **SIGINT auto-pauses the queue.** If you Ctrl-C a running command — or
  the command exits with status 130 — cmdq treats that as "the user
  changed their mind" and pauses the queue instead of dispatching the
  next item. Ctrl-X to resume; Ctrl-K to clear.
- **Bracketed paste.** Pasting a multi-line snippet into the queue input
  collapses newlines into `;` so it lands as a single queue item — review,
  then Enter to commit.
- **Quit-confirm.** Ctrl-D with a non-empty queue requires a second press
  within 3 seconds; any other key cancels the confirmation.
- **Context-aware hint line.** The bottom of the panel changes its hints
  based on whether you're typing, editing, or paused. F1 / ? opens the
  full overlay.

## How it works

```
┌────────────────────────────────────────────────────┐
│   your terminal                                    │
│   ┌─────────────────────────────────────────────┐  │
│   │  cmdq                                        │  │
│   │   • crossterm for raw mode + alt-screen     │  │
│   │   • portable-pty hosts your shell          │  │
│   │   • vt100 parses shell output → ratatui    │  │
│   │   • OSC 133 detector watches PTY stream    │  │
│   │   • queue model + line editor              │  │
│   └─────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────┘
```

The shell emits `\e]133;A`, `\e]133;C`, `\e]133;D[;exitcode]` markers around
prompts and command execution. cmdq watches the PTY output stream for these
and uses them to decide whether keystrokes should pass through to the shell
(when at a prompt) or be captured into the queue (when a command is running).

When the shell finishes a command (`\e]133;D`), cmdq writes the next queued
command's bytes back into the PTY master — same as if you'd typed it.

## Quality-of-life features

- **Conditional chaining**: prefix `?` (or hit Tab) to mark a queued command
  as "only run if the previous command succeeded". Driven by the exit code
  reported in the OSC 133 D marker.
- **Reorder**: Alt-Up / Alt-Down on the item being edited.
- **Edit in place**: Up navigates into the queue, edit, Enter to commit.
- **Pause**: Ctrl-X stops auto-dispatch; queued commands stay until you
  resume.
- **Persistence**: queue lives in `~/.local/share/cmdq/queue.json` so a
  `cmdq` restart mid-session doesn't lose pending work.
- **Force-queue & passthrough escape hatches**: if integration markers
  aren't reaching cmdq for some reason (Ctrl-Q to force queue mode), or
  you need to talk to an interactive REPL (Ctrl-\\ to send keys to
  child stdin).

## Layout

```
src/
  main.rs              CLI entry, --install-integration, --print-integration
  app.rs               event loop: PTY ↔ vt100 ↔ queue ↔ ratatui
  pty.rs               portable-pty wrapper, SIGWINCH/resize, ZDOTDIR shim
  osc133.rs            streaming detector for prompt-marker escape sequences
  queue.rs             queue model + JSON persistence
  input.rs             line editor + key→action routing
  ui.rs                ratatui rendering (vt100 → top region, queue → bottom)
  shell_integration.rs install / locate / read shell snippets
shell/
  integration.zsh      precmd / preexec emitting OSC 133
  integration.bash     PROMPT_COMMAND + DEBUG trap
  integration.fish     fish_preexec / fish_postexec / fish_prompt events
tests/
  integration_pty.rs   real-bash + Detector verification
  queue_dispatch_e2e.rs proves dispatch + conditional skipping end-to-end
  binary_smoke.rs      drives the actual cmdq binary in a PTY
```

## Tests

```
cargo test            # 41 tests: unit + integration + binary smoke
cargo clippy --all-targets -- -D warnings
```

## Caveats

- `vt100` rendering is solid for normal CLI output, colors, and most TUIs,
  but may have small fidelity gaps versus your native terminal for very
  exotic sequences (sixel images, advanced kitty graphics). For interactive
  TUIs (vim, htop) you can hit Ctrl-\\ to enter passthrough mode while
  using them.
- The auto-injected ZDOTDIR shim sources your real `~/.zshrc` before
  appending the integration. If you have machinery that's strictly
  ZDOTDIR-aware, set up integration manually via `--install-integration`.

## License

MIT.
