//! Path A architecture tests: cmdq is *not* a terminal emulator.
//!
//! These tests boot the real `cmdq` binary inside a portable-pty and assert
//! on its output bytes — the only way to verify the architecture's actual
//! invariants (DECSTBM scrolling region, no alt-screen, escape-sequence
//! passthrough). Each test owns its full PTY setup so they can run in
//! parallel without sharing state.

use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const SHELL_INTEGRATION_BASH: &str = include_str!("../shell/integration.bash");

fn cmdq_binary_path() -> std::path::PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_cmdq") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("cmdq")
}

/// Spawn `cmdq` under bash with the project's OSC 133 integration sourced
/// from a custom HOME (so the inner shell emits prompt markers reliably).
/// Returns master PTY, child handle, and a recv stream of bytes.
struct Harness {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl Harness {
    fn spawn(suffix: &str) -> Option<Self> {
        Self::spawn_with_size(suffix, 30, 100)
    }

    fn spawn_with_size(suffix: &str, rows: u16, cols: u16) -> Option<Self> {
        if !std::path::Path::new("/bin/bash").exists() {
            return None;
        }
        let bin = cmdq_binary_path();
        if !bin.exists() {
            eprintln!("cmdq binary not built; skipping ({})", bin.display());
            return None;
        }

        let tmp =
            std::env::temp_dir().join(format!("cmdq-pathA-{}-{}", std::process::id(), suffix));
        std::fs::create_dir_all(&tmp).unwrap();
        let int_path = tmp.join("integration.bash");
        std::fs::write(&int_path, SHELL_INTEGRATION_BASH).unwrap();
        let bashrc = tmp.join(".bashrc");
        std::fs::write(
            &bashrc,
            format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
        )
        .unwrap();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .ok()?;

        let mut cmd = CommandBuilder::new(bin.as_os_str());
        cmd.arg("--shell");
        cmd.arg("/bin/bash");
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", &tmp);

        let child = pair.slave.spawn_command(cmd).ok()?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().ok()?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        Some(Self {
            master: pair.master,
            child,
            rx,
        })
    }

    fn writer(&self) -> Box<dyn Write + Send> {
        self.master.take_writer().unwrap()
    }

    /// Drain the byte stream until either `predicate(accum)` returns true or
    /// the deadline passes.
    fn wait_for(
        &self,
        accum: &mut Vec<u8>,
        timeout: Duration,
        mut predicate: impl FnMut(&[u8]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(b) => accum.extend_from_slice(&b),
                Err(_) => break,
            }
            if predicate(accum) {
                return true;
            }
        }
        predicate(accum)
    }

    fn drain_for(&self, accum: &mut Vec<u8>, dur: Duration) {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            let chunk =
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now()));
            if let Ok(b) = self.rx.recv_timeout(chunk) {
                accum.extend_from_slice(&b);
            }
        }
    }

    fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// **Invariant 1: cmdq must not switch the outer terminal into alt-screen.**
/// If it did, native scrollback / selection would be gone — the whole point
/// of Path A.
#[test]
fn does_not_enter_alt_screen() {
    let Some(h) = Harness::spawn("noalt") else {
        return;
    };
    let mut accum = Vec::new();
    h.drain_for(&mut accum, Duration::from_millis(800));

    // Forbidden: the three sequences that switch the outer terminal into
    // alt-screen — `?1049h`, `?1047h`, `?47h`. cmdq passes the *inner*
    // shell's bytes through, but the inner shell here (bash --rcfile)
    // never enters alt-screen on its own.
    let bad: &[&[u8]] = &[b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
    for needle in bad {
        assert!(
            !contains(&accum, needle),
            "cmdq output contains alt-screen-enter sequence {:?}\nfull output: {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&accum)
        );
    }

    h.shutdown();
}

/// **Invariant 2: bracketed paste is enabled, keyboard enhancement is pushed.**
/// These are the only "terminal setup" sequences cmdq is allowed to emit on
/// startup. They are what makes the editor work cleanly without making us a
/// terminal emulator.
#[test]
fn enables_bracketed_paste_and_keyboard_enhancement() {
    let Some(h) = Harness::spawn("setup") else {
        return;
    };
    let mut accum = Vec::new();
    h.drain_for(&mut accum, Duration::from_millis(800));

    assert!(
        contains(&accum, b"\x1b[?2004h"),
        "bracketed-paste enable not seen; output: {:?}",
        String::from_utf8_lossy(&accum)
    );
    // Kitty keyboard protocol push: `CSI > <flags> u`. Crossterm uses flags
    // 1 (DISAMBIGUATE) | 4 (REPORT_ALTERNATE_KEYS) = 5.
    assert!(
        contains(&accum, b"\x1b[>5u") || contains(&accum, b"\x1b[>1u"),
        "keyboard-enhancement push not seen; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    h.shutdown();
}

/// **Invariant 3: shell escape sequences pass through verbatim.**
/// We type `printf` into the shell so the inner bash actually emits SGR red
/// text. cmdq must forward those bytes byte-for-byte — that's what makes
/// colors / hyperlinks / images / OSC 52 "just work" under Path A.
#[test]
fn shell_sgr_passes_through_verbatim() {
    let Some(h) = Harness::spawn("sgr") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    // Wait for the shell prompt.
    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));

    // Send a printf that emits SGR red text. Single quotes keep `\033`
    // literal; bash's printf then converts octal escapes to bytes. Use
    // `\r` to submit. (cmdq is in raw mode + key passthrough at the prompt,
    // so per-character keystrokes flow into the shell.)
    let cmd = b"printf 'CMDQ_SGR_TEST_\\033[31mRED\\033[0m_END\\n'\r";
    writer.write_all(cmd).unwrap();
    writer.flush().unwrap();

    // Wait for the *executed* printf output, not the typed echo. The typed
    // echo contains literal `\033` bytes; the executed output contains a
    // real ESC (0x1B). We require both the leading marker AND a real ESC
    // byte adjacent to `[31m` to be sure we're past the echo.
    let ok = h.wait_for(&mut accum, Duration::from_secs(5), |s| {
        contains(s, b"CMDQ_SGR_TEST_\x1b[31m")
    });
    assert!(
        ok,
        "executed printf output never appeared; full output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    // The interesting assertion: the SGR escape bytes themselves are in the
    // output. (vt100 would have absorbed them into its grid model.)
    assert!(
        contains(&accum, b"\x1b[31m") || contains(&accum, b"\x1b[31;"),
        "expected SGR red `\\x1b[31m` to pass through; output: {:?}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        contains(&accum, b"\x1b[0m") || contains(&accum, b"\x1b[m"),
        "expected SGR reset `\\x1b[0m` to pass through; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

/// **Invariant 4: OSC 8 hyperlinks pass through verbatim.**
/// vt100 0.16 didn't support OSC 8 at all; previously `gh`/`cargo`/`eza
/// --hyperlink` produced uncliсkable text in cmdq. Under Path A, the bytes
/// reach the user's terminal unmodified.
#[test]
fn osc8_hyperlink_passes_through_verbatim() {
    let Some(h) = Harness::spawn("osc8") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));

    // Emit an OSC 8 hyperlink: `ESC ] 8 ; ; https://example.com ESC \ link
    // ESC ] 8 ; ; ESC \`. Use `\033` (ESC) and `\033\\` (ST = ESC + literal
    // backslash). Single-quoted printf preserves the backslashes for printf
    // to interpret as octal escapes.
    let cmd =
        b"printf 'CMDQ_OSC8_\\033]8;;https://example.com\\033\\\\link\\033]8;;\\033\\\\_END\\n'\r";
    writer.write_all(cmd).unwrap();
    writer.flush().unwrap();

    // Wait for the *executed* output (contains a real ESC byte adjacent to
    // `]8;;`), not the typed echo (which has literal backslash-zero-three-three).
    let ok = h.wait_for(&mut accum, Duration::from_secs(5), |s| {
        contains(s, b"\x1b]8;;https://example.com")
    });
    assert!(
        ok,
        "executed printf output never appeared; full output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    // The OSC 8 prefix must be there byte-for-byte.
    assert!(
        contains(&accum, b"\x1b]8;;https://example.com"),
        "OSC 8 prefix missing; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

/// **Invariant 5: OSC 52 clipboard sequences pass through verbatim.**
/// Terminal emulators use OSC 52 for clipboard operations; cmdq should not
/// parse, strip, or reinterpret them.
#[test]
fn osc52_clipboard_passes_through_verbatim() {
    let Some(h) = Harness::spawn("osc52") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));

    writer
        .write_all(b"printf 'CMDQ_OSC52_\\033]52;c;SGVsbG8=\\007_END\\n'\r")
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(5), |s| {
        contains(s, b"\x1b]52;c;SGVsbG8=\x07")
    });
    assert!(
        ok,
        "OSC 52 bytes did not pass through; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

/// **Invariant 6: DECSTBM scrolling region is set when the panel reserves
/// rows, and reset when cmdq exits.** This is the architectural mechanism
/// that lets the shell scroll within the upper region while the panel sits
/// untouched at the bottom.
#[test]
fn decstbm_set_when_panel_reserves_and_reset_on_exit() {
    let Some(h) = Harness::spawn("decstbm") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));

    // Press Ctrl-Q to force the queue panel open even at the prompt.
    writer.write_all(&[0x11]).unwrap();
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));

    // Scrolling region prefix: `\x1b[1;` followed by a number followed by `r`.
    // We don't want to be brittle about the exact row count (it depends on
    // queue length), so just check the prefix is there.
    assert!(
        contains(&accum, b"\x1b[1;"),
        "expected DECSTBM `\\x1b[1;Nr` after Ctrl-Q; output: {:?}",
        String::from_utf8_lossy(&accum)
    );
    // Sanity: the matched prefix is followed by `r` somewhere in the next
    // few bytes (the scrolling-region terminator).
    assert!(
        find_decstbm(&accum),
        "DECSTBM prefix found but not terminated with `r`; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    // Toggle off — cmdq should reset the scrolling region. The reset form
    // is either the empty-parameter `\x1b[r` or the explicit-bounds
    // `\x1b[1;<rows>r` covering the full screen. (cmdq uses the explicit
    // form for portability across terminal emulators.)
    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q again
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));

    assert!(
        decstbm_was_reset(&accum, 30),
        "expected DECSTBM reset (`\\x1b[r` or `\\x1b[1;<rows>r`) after panel hidden; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.drain_for(&mut accum, Duration::from_millis(500));
    h.shutdown();
}

#[test]
#[cfg(unix)]
fn sigterm_restores_terminal_state_when_panel_reserved() {
    let Some(mut h) = Harness::spawn("sigterm-cleanup") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer.write_all(&[0x11]).unwrap();
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));
    assert!(find_decstbm(&accum), "panel never reserved before SIGTERM");

    let before_sigterm = accum.len();
    let pid = h.child.process_id().expect("cmdq child pid");
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM failed: {status}");
    h.drain_for(&mut accum, Duration::from_secs(1));
    drop(writer);

    let mut exited = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if h.child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    assert!(exited, "cmdq did not exit after SIGTERM");

    let after_sigterm = &accum[before_sigterm..];
    assert!(
        decstbm_was_reset(after_sigterm, 30),
        "SIGTERM cleanup should reset DECSTBM; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
    assert!(
        contains(after_sigterm, b"\x1b[?2004l"),
        "SIGTERM cleanup should disable bracketed paste; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
}

#[test]
fn child_exit_while_in_alt_screen_restores_outer_terminal() {
    let Some(mut h) = Harness::spawn("alt-exit-cleanup") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();
    writer.write_all(b"printf '\\033[?1049h'; exit\r").unwrap();
    writer.flush().unwrap();
    drop(writer);

    let mut exited = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        h.drain_for(&mut accum, Duration::from_millis(100));
        if h.child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
    }
    if !exited {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    assert!(exited, "cmdq did not exit after inner shell exit");

    let tail = &accum[start..];
    assert!(
        contains(tail, b"\x1b[?1049h"),
        "test command never entered alt-screen; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert!(
        contains(tail, b"\x1b[?1049l"),
        "cleanup should leave alt-screen after child exits; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert!(
        decstbm_was_reset(tail, 30),
        "cleanup should reset DECSTBM after child exits; output: {:?}",
        String::from_utf8_lossy(tail)
    );
}

#[test]
#[cfg(unix)]
fn sigterm_while_in_alt_screen_restores_outer_terminal() {
    let Some(mut h) = Harness::spawn("sigterm-alt-cleanup") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer
        .write_all(b"printf '\\033[?1049h'; sleep 10\r")
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(5), |s| {
            contains(s, b"\x1b[?1049h")
        }),
        "test command never entered alt-screen; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let before_sigterm = accum.len();
    let pid = h.child.process_id().expect("cmdq child pid");
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM failed: {status}");
    h.drain_for(&mut accum, Duration::from_secs(1));
    drop(writer);

    let mut exited = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if h.child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    assert!(exited, "cmdq did not exit after SIGTERM");

    let after_sigterm = &accum[before_sigterm..];
    assert!(
        contains(after_sigterm, b"\x1b[?1049l"),
        "SIGTERM cleanup should leave alt-screen; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
    assert!(
        contains(after_sigterm, b"\x1b[?2004l"),
        "SIGTERM cleanup should disable bracketed paste; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
}

#[test]
#[cfg(unix)]
fn sigterm_while_mouse_and_focus_modes_enabled_restores_outer_terminal() {
    let Some(mut h) = Harness::spawn("sigterm-mouse-focus-cleanup") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer
        .write_all(b"printf '\\033[?1006h\\033[?1004h'; sleep 10\r")
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(5), |s| {
            contains(s, b"\x1b[?1006h") && contains(s, b"\x1b[?1004h")
        }),
        "test command never enabled mouse/focus modes; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let before_sigterm = accum.len();
    let pid = h.child.process_id().expect("cmdq child pid");
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM failed: {status}");
    h.drain_for(&mut accum, Duration::from_secs(1));
    drop(writer);

    let mut exited = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if h.child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    assert!(exited, "cmdq did not exit after SIGTERM");

    let after_sigterm = &accum[before_sigterm..];
    assert!(
        contains(after_sigterm, b"\x1b[?1006l") && contains(after_sigterm, b"\x1b[?1000l"),
        "SIGTERM cleanup should disable mouse capture modes; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
    assert!(
        contains(after_sigterm, b"\x1b[?1004l"),
        "SIGTERM cleanup should disable focus events; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
    assert!(
        contains(after_sigterm, b"\x1b[?2004l"),
        "SIGTERM cleanup should disable bracketed paste; output: {:?}",
        String::from_utf8_lossy(after_sigterm)
    );
}

#[test]
fn tiny_terminal_does_not_reserve_invisible_panel() {
    let Some(h) = Harness::spawn_with_size("tiny", 4, 10) else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let before = accum.len();
    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q would normally force the panel.
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));
    let after_ctrl_q = &accum[before..];

    assert!(
        !find_decstbm(after_ctrl_q),
        "tiny terminal should not reserve a panel; output after Ctrl-Q: {:?}",
        String::from_utf8_lossy(after_ctrl_q)
    );

    let before_resize = accum.len();
    h.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));
    let after_resize = &accum[before_resize..];
    assert!(
        !find_decstbm(after_resize),
        "Ctrl-Q on a tiny terminal should not arm a sticky panel after resize: {:?}",
        String::from_utf8_lossy(after_resize)
    );

    let before_second_ctrl_q = accum.len();
    writer.write_all(&[0x11]).unwrap();
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));
    let after_second_ctrl_q = &accum[before_second_ctrl_q..];
    assert!(
        find_decstbm(after_second_ctrl_q),
        "Ctrl-Q after resizing to usable size should reserve the panel: {:?}",
        String::from_utf8_lossy(after_second_ctrl_q)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn resize_hidden_layout_updates_child_pty_size() {
    let Some(h) = Harness::spawn("resize-hidden") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    h.master
        .resize(PtySize {
            rows: 16,
            cols: 70,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));

    writer
        .write_all(b"printf 'SIZE:'; stty size; echo RESIZE_HIDDEN_DONE\r")
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(5), |s| {
            contains(s, b"SIZE:16 70") && contains(s, b"RESIZE_HIDDEN_DONE")
        }),
        "child PTY did not see hidden-layout resize; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn resize_storm_keeps_shell_usable() {
    let Some(h) = Harness::spawn("resize") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer.write_all(&[0x11]).unwrap(); // force panel
    writer.flush().unwrap();
    assert!(
        h.wait_for(&mut accum, Duration::from_secs(2), find_decstbm),
        "panel did not reserve before resize storm; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    for (rows, cols) in [(40, 120), (12, 50), (4, 10), (30, 100), (18, 70), (30, 100)] {
        h.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        h.drain_for(&mut accum, Duration::from_millis(120));
    }

    writer.write_all(&[0x11]).unwrap(); // release force panel
    writer.flush().unwrap();
    h.drain_for(&mut accum, Duration::from_millis(300));
    writer.write_all(b"echo RESIZE_STORM_OK\r").unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(5), |s| {
            contains(s, b"RESIZE_STORM_OK")
        }),
        "shell was not usable after resize storm; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn resize_from_reserved_panel_resets_scroll_region_to_new_height() {
    let Some(h) = Harness::spawn("resize-reset-new-height") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer.write_all(&[0x11]).unwrap(); // force panel
    writer.flush().unwrap();
    assert!(
        h.wait_for(&mut accum, Duration::from_secs(2), |s| {
            contains(s, b"\x1b[1;27r")
        }),
        "panel did not reserve before resize; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let start = accum.len();
    h.master
        .resize(PtySize {
            rows: 4,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    h.drain_for(&mut accum, Duration::from_millis(500));

    let tail = &accum[start..];
    assert!(
        contains(tail, b"\x1b[1;4r"),
        "resize should reset DECSTBM to the new terminal height, not only the stale old height; tail: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

/// **Invariant 7: alt-screen sniffer detects an inner-program flip, releases
/// cmdq's panel *before* forwarding `?1049h`, and restores the panel after
/// `?1049l` if the command is still running.**
///
/// This matters for real apps like vim/fzf: their first full-screen draw
/// must see the full terminal height, not cmdq's shrunken shell PTY.
#[test]
fn alt_screen_releases_panel_before_forwarding_and_restores_after_exit() {
    let Some(h) = Harness::spawn("altorder") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    // Initial sleep gives cmdq time to reserve the panel for a long-running
    // command. The sleep after `?1049l` keeps the command alive long enough
    // for cmdq to re-reserve the panel after the alt-screen app exits.
    writer
        .write_all(b"sleep 2; printf '\\033[?1049h'; sleep 0.2; printf '\\033[?1049l'; sleep 2\r")
        .unwrap();
    writer.flush().unwrap();
    let ok = h.wait_for(&mut accum, Duration::from_secs(7), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1b[?1049h")
            && contains(tail, b"\x1b[?1049l")
            && count_occurrences(tail, b"\x1b[1;27r") >= 2
            && contains(tail, b"\x1b[1;30r")
    });
    assert!(
        ok,
        "alt-screen lifecycle sequences not observed; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let first_reserve = find_bytes(tail, b"\x1b[1;27r").expect("panel reserve");
    let reset = find_bytes(tail, b"\x1b[1;30r").expect("full-height reset");
    let enter = find_bytes(tail, b"\x1b[?1049h").expect("inner alt-screen enter");
    let exit = find_bytes(tail, b"\x1b[?1049l").expect("inner alt-screen exit");
    let restore = find_bytes_after(tail, b"\x1b[1;27r", exit).expect("panel restore");

    assert!(
        first_reserve < reset,
        "panel should be reserved before alt-screen app starts; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert!(
        reset < enter,
        "panel must be released before forwarding `?1049h`; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert!(
        exit < restore,
        "panel should be restored after `?1049l` while command is still running; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert_eq!(
        count_occurrences(tail, b"\x1b[?1049h"),
        1,
        "cmdq must not amplify alt-screen enter; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert_eq!(
        count_occurrences(tail, b"\x1b[?1049l"),
        1,
        "cmdq must not amplify alt-screen exit; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn combined_alt_screen_and_bracketed_paste_mode_wraps_child_paste() {
    let Some(h) = Harness::spawn("altpaste-combined") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();
    let paste_file =
        std::env::temp_dir().join(format!("cmdq-combined-paste-{}", std::process::id()));
    let _ = std::fs::remove_file(&paste_file);

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    writer
        .write_all(
            format!(
                "printf '\\033[?1049;2004h'; cat > {}\r",
                shell_quote(&paste_file.to_string_lossy())
            )
            .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(5), |s| {
            contains(s, b"\x1b[?1049;2004h")
        }),
        "combined alt-screen/bracketed-paste mode was not observed; output: {:?}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"\x1b[200~hello\x1b[201~").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x04, 0x04]).unwrap();
    writer.flush().unwrap();

    let wrapped = h.wait_for(&mut accum, Duration::from_secs(5), |_| {
        std::fs::read(&paste_file)
            .map(|bytes| contains(&bytes, b"\x1b[200~hello\x1b[201~"))
            .unwrap_or(false)
    });

    let _ = writer.write_all(&[0x03]);
    let _ = writer.flush();
    h.shutdown();
    let file_bytes = std::fs::read(&paste_file).unwrap_or_default();
    let _ = std::fs::remove_file(&paste_file);

    assert!(
        wrapped,
        "child paste should be wrapped after combined mode set; file={:?}; output={:?}",
        String::from_utf8_lossy(&file_bytes),
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn alt_screen_exit_restores_panel_before_trailing_same_chunk_output() {
    let Some(h) = Harness::spawn("alttrail") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(b"sleep 2; printf '\\033[?1049hALT\\033[?1049lAFTER_EXIT'; sleep 2\r")
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(7), |s| {
        let tail = &s[start..];
        let Some(exit) = find_bytes(tail, b"\x1b[?1049l") else {
            return false;
        };
        contains(tail, b"\x1b[?1049h")
            && find_bytes_after(tail, b"AFTER_EXIT", exit).is_some()
            && count_occurrences(tail, b"\x1b[1;27r") >= 2
    });
    assert!(
        ok,
        "alt-screen trailing-output lifecycle not observed; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let enter = find_bytes(tail, b"\x1b[?1049h").expect("inner alt-screen enter");
    let exit = find_bytes(tail, b"\x1b[?1049l").expect("inner alt-screen exit");
    let restore = find_bytes_after(tail, b"\x1b[1;27r", exit).expect("panel restore");
    let trailing = find_bytes_after(tail, b"AFTER_EXIT", exit).expect("trailing output");

    assert!(
        enter < exit && exit < restore && restore < trailing,
        "panel must restore before bytes after `?1049l`; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn split_alt_screen_enter_is_not_interleaved_with_panel_release() {
    let Some(h) = Harness::spawn("altsplit") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(
            b"sleep 2; printf '\\033'; sleep 0.2; printf '[?1049hSPLIT_ALT\\033[?1049l'; sleep 2\r",
        )
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(7), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1b[?1049hSPLIT_ALT")
            && contains(tail, b"\x1b[?1049l")
            && contains(tail, b"\x1b[1;30r")
    });
    assert!(
        ok,
        "split alt-screen enter was not forwarded contiguously; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let reset = find_bytes(tail, b"\x1b[1;30r").expect("full-height reset");
    let enter = find_bytes(tail, b"\x1b[?1049h").expect("inner alt-screen enter");
    assert!(
        reset < enter,
        "panel release must happen before the complete alt-screen enter sequence, not between ESC and suffix; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert_eq!(
        count_occurrences(tail, b"\x1b[?1049h"),
        1,
        "split alt-screen enter should remain one contiguous sequence"
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn mouse_capture_releases_panel_and_forwards_mouse_events() {
    let Some(h) = Harness::spawn("mouse-capture") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();
    writer
        .write_all(
            b"sleep 2; printf '\\033[?1006h'; IFS= read -r -s -n 11 mouse; printf '\\nMOUSE_HEX:'; printf '%s' \"$mouse\" | od -An -tx1 | tr -d ' \\n'; printf '\\n'; printf '\\033[?1006l'; sleep 2\r",
        )
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(6), |s| {
            contains(&s[start..], b"\x1b[?1006h")
        }),
        "mouse-capture command never enabled SGR mouse mode; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );
    writer.write_all(b"\x1b[<20;10;5M").unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(5), |s| {
        let tail = &s[start..];
        contains(tail, b"MOUSE_HEX:1b5b3c32303b31303b354d")
            && contains(tail, b"\x1b[?1006l")
            && count_occurrences(tail, b"\x1b[1;27r") >= 2
            && contains(tail, b"\x1b[1;30r")
    });
    assert!(
        ok,
        "mouse event was not forwarded through cmdq; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let first_reserve = find_bytes(tail, b"\x1b[1;27r").expect("panel reserve");
    let reset = find_bytes(tail, b"\x1b[1;30r").expect("full-height reset");
    let mouse_on = find_bytes(tail, b"\x1b[?1006h").expect("mouse enable");
    let mouse_off = find_bytes(tail, b"\x1b[?1006l").expect("mouse disable");
    let restore = find_bytes_after(tail, b"\x1b[1;27r", mouse_off).expect("panel restore");

    assert!(
        first_reserve < reset && reset < mouse_on,
        "panel must be released before forwarding mouse enable; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert!(
        mouse_off < restore,
        "panel should be restored after mouse mode exits; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn focus_events_are_forwarded_when_child_enables_focus_reporting() {
    let Some(h) = Harness::spawn("focus-events") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();
    writer
        .write_all(
            b"printf '\\033[?1004h'; IFS= read -r -s -n 3 focus; printf '\\nFOCUS_HEX:'; printf '%s' \"$focus\" | od -An -tx1 | tr -d ' \\n'; printf '\\n'; printf '\\033[?1004l'\r",
        )
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(3), |s| {
            contains(&s[start..], b"\x1b[?1004h")
        }),
        "focus-reporting command never enabled focus events; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );
    writer.write_all(b"\x1b[I").unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(3), |s| {
            let tail = &s[start..];
            contains(tail, b"FOCUS_HEX:1b5b49") && contains(tail, b"\x1b[?1004l")
        }),
        "focus event was not forwarded through cmdq; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn command_end_repairs_child_left_mouse_and_focus_modes() {
    let Some(h) = Harness::spawn("mode-leak-command-end") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();
    writer
        .write_all(b"printf '\\033[?1006h\\033[?1004hMODE_LEAK_DONE\\n'\r")
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(3), |s| {
            let tail = &s[start..];
            contains(tail, b"MODE_LEAK_DONE")
                && contains(tail, b"\x1b[?1006h")
                && contains(tail, b"\x1b[?1004h")
                && contains(tail, b"\x1b[?1006l")
                && contains(tail, b"\x1b[?1004l")
        }),
        "cmdq did not repair child-left mouse/focus modes after command end; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let done = find_bytes(tail, b"MODE_LEAK_DONE").expect("command output");
    let mouse_off = find_bytes_after(tail, b"\x1b[?1006l", done).expect("mouse disable");
    let focus_off = find_bytes_after(tail, b"\x1b[?1004l", done).expect("focus disable");

    assert!(
        done < mouse_off && done < focus_off,
        "terminal mode repair should happen after command output and before the next prompt; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn command_end_repairs_child_left_alt_screen() {
    let Some(h) = Harness::spawn("alt-leak-command-end") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();
    writer
        .write_all(b"printf '\\033[?1049hALT_LEAK_DONE\\n'\r")
        .unwrap();
    writer.flush().unwrap();

    assert!(
        h.wait_for(&mut accum, Duration::from_secs(3), |s| {
            let tail = &s[start..];
            contains(tail, b"ALT_LEAK_DONE")
                && contains(tail, b"\x1b[?1049h")
                && contains(tail, b"\x1b[?1049l")
        }),
        "cmdq did not leave child-left alt-screen after command end; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let done = find_bytes(tail, b"ALT_LEAK_DONE").expect("command output");
    let alt_off = find_bytes_after(tail, b"\x1b[?1049l", done).expect("alt-screen disable");
    assert!(
        done < alt_off,
        "alt-screen repair should happen after command output; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn delayed_split_alt_screen_enter_stays_contiguous() {
    let Some(h) = Harness::spawn("altdelayed") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(
            b"sleep 2; printf '\\033'; sleep 0.45; printf '[?1049hDELAYED_ALT\\033[?1049l'; sleep 2\r",
        )
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(7), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1b[?1049hDELAYED_ALT")
            && contains(tail, b"\x1b[?1049l")
            && contains(tail, b"\x1b[1;30r")
    });
    assert!(
        ok,
        "delayed split alt-screen enter was not forwarded contiguously; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let reset = find_bytes(tail, b"\x1b[1;30r").expect("full-height reset");
    let enter = find_bytes(tail, b"\x1b[?1049h").expect("inner alt-screen enter");
    assert!(
        reset < enter,
        "panel release must happen before the complete delayed alt-screen enter sequence; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn slow_progress_split_alt_screen_enter_stays_contiguous() {
    let Some(h) = Harness::spawn("altslowprogress") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(
            b"sleep 2; printf '\\033'; sleep 0.45; printf '[?104'; sleep 0.3; printf '9hSLOW_ALT\\033[?1049l'; sleep 2\r",
        )
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(8), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1b[?1049hSLOW_ALT")
            && contains(tail, b"\x1b[?1049l")
            && contains(tail, b"\x1b[1;30r")
    });
    assert!(
        ok,
        "slow-progress alt-screen enter was not forwarded contiguously; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let reset = find_bytes(tail, b"\x1b[1;30r").expect("full-height reset");
    let enter = find_bytes(tail, b"\x1b[?1049h").expect("inner alt-screen enter");
    assert!(
        reset < enter,
        "panel release must happen before the complete slow-progress alt-screen enter sequence; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert_eq!(
        count_occurrences(tail, b"\x1b[?1049h"),
        1,
        "slow-progress alt-screen enter should remain one contiguous sequence"
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn incomplete_escape_fragment_flushes_before_child_outputs_more() {
    let Some(h) = Harness::spawn("escpending") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(b"printf 'PENDING_ESC:\\033'; sleep 1; printf ':AFTER\\n'\r")
        .unwrap();
    writer.flush().unwrap();

    let flushed = h.wait_for(&mut accum, Duration::from_millis(800), |s| {
        let tail = &s[start..];
        contains(tail, b"PENDING_ESC:\x1b")
    });
    assert!(
        flushed,
        "trailing ESC fragment should be forwarded after a short timeout; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    h.shutdown();
}

#[test]
fn panel_paint_does_not_clobber_child_cursor_save_restore_slot() {
    let Some(h) = Harness::spawn("cursorsave") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(b"sleep 2; printf '\\0337SAVED\\0338RESTORED'; sleep 1\r")
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(6), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1b7SAVED") && contains(tail, b"\x1b8RESTORED") && find_decstbm(tail)
    });
    assert!(
        ok,
        "cursor save/restore smoke did not observe expected output; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    assert_eq!(
        count_occurrences(tail, b"\x1b7"),
        1,
        "cmdq panel paint must not emit ESC 7 and clobber child cursor save slot; output: {:?}",
        String::from_utf8_lossy(tail)
    );
    assert_eq!(
        count_occurrences(tail, b"\x1b8"),
        1,
        "cmdq panel paint must not emit ESC 8 and clobber child cursor save slot; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn panel_restore_tracks_wide_child_output_cursor_width() {
    let Some(h) = Harness::spawn("widecursor") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(b"sleep 2; printf '\\347\\225\\214\\360\\237\\231\\202'; sleep 2\r")
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(6), |s| {
        let tail = &s[start..];
        contains(tail, "界🙂".as_bytes()) && contains(tail, b"\x1b[27;5H")
    });
    assert!(
        ok,
        "panel did not restore cursor after wide child output; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let wide = find_bytes(tail, "界🙂".as_bytes()).expect("wide child output");
    let restore = find_bytes_after(tail, b"\x1b[27;5H", wide)
        .expect("cursor restore after wide child output");
    assert!(
        wide < restore,
        "cursor restore should happen after child wide output; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn panel_restore_ignores_osc_payload_width_for_child_cursor() {
    let Some(h) = Harness::spawn("osccursor") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(
            b"sleep 2; printf '\\033]8;;https://example.com/a/very/long/url\\033\\\\link\\033]8;;\\033\\\\'; sleep 2\r",
        )
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(6), |s| {
        let tail = &s[start..];
        contains(
            tail,
            b"\x1b]8;;https://example.com/a/very/long/url\x1b\\link",
        ) && contains(tail, b"\x1b[27;5H")
    });
    assert!(
        ok,
        "panel did not restore cursor after OSC hyperlink by visible width; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let link = find_bytes(tail, b"link").expect("visible OSC 8 label");
    let restore =
        find_bytes_after(tail, b"\x1b[27;5H", link).expect("cursor restore after OSC label");
    assert!(
        link < restore,
        "cursor restore should follow visible OSC label at width 4; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

#[test]
fn panel_restore_ignores_dcs_payload_width_for_child_cursor() {
    let Some(h) = Harness::spawn("dcscursor") else {
        return;
    };
    let mut writer = h.writer();
    let mut accum = Vec::new();

    h.wait_for(&mut accum, Duration::from_secs(3), |s| s.contains(&b'$'));
    let start = accum.len();

    writer
        .write_all(b"sleep 2; printf '\\033P1;1;AAAAA\\033\\\\ok'; sleep 2\r")
        .unwrap();
    writer.flush().unwrap();

    let ok = h.wait_for(&mut accum, Duration::from_secs(6), |s| {
        let tail = &s[start..];
        contains(tail, b"\x1bP1;1;AAAAA\x1b\\ok") && contains(tail, b"\x1b[27;3H")
    });
    assert!(
        ok,
        "panel did not restore cursor after DCS payload by visible width; output: {:?}",
        String::from_utf8_lossy(&accum[start..])
    );

    let tail = &accum[start..];
    let visible = find_bytes(tail, b"ok").expect("visible text after DCS");
    let restore =
        find_bytes_after(tail, b"\x1b[27;3H", visible).expect("cursor restore after visible text");
    assert!(
        visible < restore,
        "cursor restore should ignore DCS payload and follow only visible text; output: {:?}",
        String::from_utf8_lossy(tail)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    h.shutdown();
}

// ---- helpers ----

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_bytes_after(haystack: &[u8], needle: &[u8], offset: usize) -> Option<usize> {
    let start = offset.saturating_add(1).min(haystack.len());
    find_bytes(&haystack[start..], needle).map(|pos| start + pos)
}

/// Look for a complete DECSTBM `CSI 1 ; <digits> r` sequence somewhere in
/// the stream. (We don't pin the row count, just the shape.)
fn find_decstbm(haystack: &[u8]) -> bool {
    let prefix = b"\x1b[1;";
    let mut i = 0;
    while i + prefix.len() <= haystack.len() {
        if &haystack[i..i + prefix.len()] == prefix {
            // Skip digits.
            let mut j = i + prefix.len();
            while j < haystack.len() && haystack[j].is_ascii_digit() {
                j += 1;
            }
            if j < haystack.len() && haystack[j] == b'r' {
                return true;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

/// Detect a DECSTBM *reset* — either the empty-params `\x1b[r` form, or the
/// explicit `\x1b[1;<rows>r` form covering the full screen. The latter is
/// what cmdq emits today (more portable), but accepting both keeps the test
/// resilient if the implementation switches back to the bare form.
fn decstbm_was_reset(haystack: &[u8], rows: u16) -> bool {
    contains(haystack, b"\x1b[r") || contains(haystack, format!("\x1b[1;{rows}r").as_bytes())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
