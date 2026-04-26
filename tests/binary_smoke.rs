//! Smoke test: spawn the actual `cmdq` binary inside a PTY, drive it with
//! some keystrokes, then exit cleanly. Verifies the whole binary starts,
//! enters/leaves alt-screen, and shuts down without panicking when the
//! inner shell exits.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn cmdq_binary_path() -> std::path::PathBuf {
    // Tests are run from the crate root; CARGO_BIN_EXE_cmdq is the canonical
    // way to find the built test artifact. If unavailable, fall back to the
    // debug build path.
    if let Some(p) = option_env!("CARGO_BIN_EXE_cmdq") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("cmdq")
}

#[test]
fn binary_starts_and_exits_cleanly_with_bash() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        eprintln!("cmdq binary not built; skipping ({})", bin.display());
        return;
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg("/bin/bash");
    cmd.env("TERM", "xterm-256color");
    // Don't inherit broken/test-runner env that might confuse bash.
    cmd.env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    // Wait for cmdq to start up and produce *some* bytes (alt-screen + first
    // ratatui frame). Then send `exit\n` to the child shell — once the shell
    // exits, cmdq should exit too.
    let mut accum = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        if accum.len() > 32 {
            break;
        }
    }
    assert!(
        !accum.is_empty(),
        "cmdq produced no output — it didn't start"
    );

    // Send some text to be queued or run. With OSC 133 not active in this
    // bash (because we --noprofile/--norc would be needed), shell_state may
    // remain Unknown — typing should pass through to the shell (bash will
    // run "exit" verbatim).
    writer.write_all(b"exit\r").unwrap();
    writer.flush().unwrap();

    // Wait for cmdq to terminate.
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "cmdq did not exit after shell `exit`");
}

#[test]
fn binary_queues_command_via_force_queue_then_dispatches() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    // Set up a custom HOME with our integration sourced from .bashrc, so the
    // shell inside cmdq emits OSC 133 markers reliably regardless of the
    // user's actual rc files.
    let tmp = std::env::temp_dir().join(format!("cmdq-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(
        &int_path,
        include_str!("../shell/integration.bash"),
    )
    .unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg("/bin/bash");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &tmp);
    // Make bash interactive use our bashrc.
    // (cmdq's pty.rs sets BASH_ENV; for an interactive bash, --rcfile or
    // HOME-based .bashrc is what gets sourced.)

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    // Wait for startup.
    let mut accum = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        if accum.len() > 64 {
            break;
        }
    }
    // Give the inner shell a moment to source rc.
    std::thread::sleep(Duration::from_millis(500));

    // Press Ctrl-Q to enter force-queue mode (so we capture even at prompt).
    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Type a queued command and Enter.
    writer.write_all(b"echo SMOKE_QUEUED\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Press Ctrl-Q to leave force-queue mode (so the next "echo" we send
    // becomes typed-into-shell rather than queued).
    writer.write_all(&[0x11]).unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // Now type a command directly into the shell so it runs and triggers
    // CommandEnd; cmdq should then dispatch our queued command.
    writer.write_all(b"echo SMOKE_TRIGGER\r").unwrap();
    writer.flush().unwrap();

    // Wait until both strings appear.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        let s = String::from_utf8_lossy(&accum);
        if s.contains("SMOKE_QUEUED") && s.contains("SMOKE_TRIGGER") {
            break;
        }
    }

    let s = String::from_utf8_lossy(&accum);

    // Cleanup.
    writer.write_all(b"exit\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        s.contains("SMOKE_TRIGGER"),
        "trigger command did not run; output:\n{}",
        s
    );
    assert!(
        s.contains("SMOKE_QUEUED"),
        "queued command did not get dispatched; output:\n{}",
        s
    );
}
