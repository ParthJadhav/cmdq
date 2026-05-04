//! Integration test: spawn a real bash shell with cmdq's OSC 133 integration
//! sourced, drive a command through it, and verify the OSC 133 markers reach
//! the master side. This proves the wiring end-to-end without a TUI.

use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SHELL_INTEGRATION_BASH: &str = include_str!("../shell/integration.bash");

fn write_temp_integration() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cmdq-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("integration.bash");
    std::fs::write(&path, SHELL_INTEGRATION_BASH).unwrap();
    path
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn bash_integration_emits_osc133_markers() {
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        eprintln!("/bin/bash not available — skipping");
        return;
    }
    let bash_path = bash.to_string_lossy().into_owned();

    let integration_path = write_temp_integration();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let rcfile = integration_path.parent().unwrap().join("bashrc");
    std::fs::write(
        &rcfile,
        format!(
            "PS1='PROMPT> '\nsource \"{}\"\n",
            integration_path.display()
        ),
    )
    .unwrap();

    let mut cmd = CommandBuilder::new(&bash_path);
    cmd.arg("--noprofile");
    cmd.arg("--rcfile");
    cmd.arg(&rcfile);
    cmd.arg("-i");
    cmd.env("CMDQ_ACTIVE", "1");
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    // Reader thread.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Helper: collect bytes for up to a duration.
    let collect_until = |needle: &[u8], timeout: Duration, accum: &mut Vec<u8>| -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if accum.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(b) => accum.extend_from_slice(&b),
                Err(_) => continue,
            }
        }
        accum.windows(needle.len()).any(|w| w == needle)
    };

    let mut accum: Vec<u8> = Vec::new();

    // Wait for first prompt + PromptStart marker.
    assert!(
        collect_until(b"PROMPT>", Duration::from_secs(5), &mut accum),
        "did not see initial prompt"
    );
    let saw_prompt_start = accum.windows(7).any(|w| w == b"\x1b]133;A");
    assert!(
        saw_prompt_start,
        "expected 133;A marker before first prompt; got {} bytes",
        accum.len()
    );

    // Run a command.
    writer.write_all(b"echo hello-cmdq\n").unwrap();
    writer.flush().unwrap();

    // Wait for D marker (final, comes after output) — by the time we see it
    // we will also have seen C.
    assert!(
        collect_until(b"\x1b]133;D", Duration::from_secs(5), &mut accum),
        "did not see CommandEnd marker; got: {:?}",
        String::from_utf8_lossy(&accum)
    );

    let saw_start = accum.windows(7).any(|w| w == b"\x1b]133;C");
    let saw_end = accum.windows(7).any(|w| w == b"\x1b]133;D");
    assert!(saw_start, "expected 133;C marker for command start");
    assert!(saw_end, "expected 133;D marker for command end");
    assert!(
        accum
            .windows(b"hello-cmdq".len())
            .any(|w| w == b"hello-cmdq"),
        "expected command output 'hello-cmdq' in stream"
    );

    // Clean up.
    writer.write_all(b"exit\n").unwrap();
    writer.flush().unwrap();
    let _ = child.wait();
}

#[test]
fn bash_integration_emits_cwd_before_command_end_after_cd() {
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        return;
    }

    let integration_path = write_temp_integration();
    let dir = integration_path.parent().unwrap();
    let start = dir.join("cwd-before-d-start");
    let next = dir.join("cwd-before-d-next");
    std::fs::create_dir_all(&start).unwrap();
    std::fs::create_dir_all(&next).unwrap();
    let rcfile = dir.join("bashrc-cwd-before-d");
    std::fs::write(
        &rcfile,
        format!(
            "PS1='PROMPT> '\nsource \"{}\"\n",
            integration_path.display()
        ),
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.arg("--noprofile");
    cmd.arg("--rcfile");
    cmd.arg(&rcfile);
    cmd.arg("-i");
    cmd.cwd(&start);
    cmd.env("CMDQ_ACTIVE", "1");
    cmd.env("TERM", "xterm-256color");

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

    let mut accum = Vec::new();
    assert!(wait_for_bytes(
        &rx,
        &mut accum,
        b"\x1b]133;A",
        Duration::from_secs(5)
    ));

    writer
        .write_all(format!("cd {}\n", shell_quote(&next.to_string_lossy())).as_bytes())
        .unwrap();
    writer.flush().unwrap();
    assert!(wait_for_bytes(
        &rx,
        &mut accum,
        b"\x1b]133;D;0",
        Duration::from_secs(5)
    ));

    let cwd_marker = format!("\x1b]7;file://localhost{}\x07", next.to_string_lossy());
    let cwd_pos = find_bytes(&accum, cwd_marker.as_bytes()).unwrap_or_else(|| {
        panic!(
            "missing cwd marker for {}; output:\n{}",
            next.display(),
            String::from_utf8_lossy(&accum)
        )
    });
    let end_pos = find_bytes(&accum, b"\x1b]133;D;0").expect("missing command-end marker");
    assert!(
        cwd_pos < end_pos,
        "cwd marker must arrive before command end so dispatch uses fresh cwd; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();
}

#[test]
fn detector_picks_up_real_shell_markers() {
    // Same setup as above, but feed bytes through cmdq's actual Detector.
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        return;
    }

    let integration_path = write_temp_integration();
    let rcfile = integration_path.parent().unwrap().join("bashrc-detector");
    std::fs::write(
        &rcfile,
        format!("PS1='$ '\nsource \"{}\"\n", integration_path.display()),
    )
    .unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.arg("--noprofile");
    cmd.arg("--rcfile");
    cmd.arg(&rcfile);
    cmd.arg("-i");
    cmd.env("CMDQ_ACTIVE", "1");
    cmd.env("TERM", "xterm-256color");

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

    use cmdq::osc133::{Detector, Event};
    let mut detector = Detector::new();
    let mut events: Vec<Event> = Vec::new();
    let mut deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            for ev in detector.feed(&b) {
                events.push(ev);
            }
        }
        if events.iter().any(|e| matches!(e, Event::PromptStart)) {
            break;
        }
    }
    assert!(
        events.iter().any(|e| matches!(e, Event::PromptStart)),
        "no PromptStart from detector after waiting; events: {:?}",
        events
    );

    // Run a quick command.
    writer.write_all(b"true\n").unwrap();
    writer.flush().unwrap();
    deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            for ev in detector.feed(&b) {
                events.push(ev);
            }
        }
        if events.iter().any(|e| matches!(e, Event::CommandEnd { .. })) {
            break;
        }
    }

    let saw_start = events.iter().any(|e| matches!(e, Event::CommandStart));
    let saw_end_zero = events
        .iter()
        .any(|e| matches!(e, Event::CommandEnd { exit_code: Some(0) }));
    assert!(
        saw_start,
        "detector missed CommandStart; events: {:?}",
        events
    );
    assert!(
        saw_end_zero,
        "detector missed CommandEnd exit=0; events: {:?}",
        events
    );

    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();
}

#[test]
fn bash_integration_preserves_existing_debug_trap() {
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        return;
    }

    let integration_path = write_temp_integration();
    let dir = integration_path.parent().unwrap();
    let debug_log = dir.join("debug-trap.log");
    let rcfile = dir.join("bashrc-debug-trap");
    std::fs::write(
        &rcfile,
        format!(
            "PS1='$ '\ntrap 'printf \"DEBUG:%s\\n\" \"$BASH_COMMAND\" >> \"$DEBUG_LOG\"' DEBUG\nsource \"{}\"\n: > \"$DEBUG_LOG\"\n",
            integration_path.display()
        ),
    )
    .unwrap();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.arg("--noprofile");
    cmd.arg("--rcfile");
    cmd.arg(&rcfile);
    cmd.arg("-i");
    cmd.env("CMDQ_ACTIVE", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env("DEBUG_LOG", &debug_log);

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

    let mut accum = Vec::new();
    assert!(
        wait_for_bytes(&rx, &mut accum, b"\x1b]133;A", Duration::from_secs(5)),
        "no prompt marker before debug-trap command; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"echo debug-target\n").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for_bytes(&rx, &mut accum, b"debug-target", Duration::from_secs(5)),
        "no user command output after debug-trap command; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        wait_for_bytes(&rx, &mut accum, b"\x1b]133;D;0", Duration::from_secs(5)),
        "no command-end marker after user command; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let log = std::fs::read_to_string(&debug_log).unwrap_or_default();
    assert!(
        log.contains("echo debug-target"),
        "existing DEBUG trap did not see user command; log={log:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();
}

#[test]
fn bash_integration_preserves_prompt_command_array_shape() {
    let bash = std::path::Path::new("/bin/bash");
    if !bash.exists() {
        return;
    }

    let integration_path = write_temp_integration();
    let script = format!(
        "PROMPT_COMMAND=('printf user-one' 'printf user-two'); \
         CMDQ_ACTIVE=1; \
         source {}; \
         declare -p PROMPT_COMMAND",
        shell_quote(&integration_path.to_string_lossy())
    );

    let output = Command::new(bash).args(["-lc", &script]).output().unwrap();
    assert!(
        output.status.success(),
        "bash failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("declare -a PROMPT_COMMAND="),
        "PROMPT_COMMAND was flattened instead of preserved as an array: {stdout}"
    );
    assert!(
        stdout.contains("[0]=\"_CMDQ_LAST_STATUS=") && stdout.contains("_cmdq_prompt_start"),
        "cmdq prompt start hook was not prepended: {stdout}"
    );
    assert!(
        stdout.contains("[1]=\"printf user-one\""),
        "first user PROMPT_COMMAND entry was not preserved: {stdout}"
    );
    assert!(
        stdout.contains("[2]=\"printf user-two\""),
        "second user PROMPT_COMMAND entry was not preserved: {stdout}"
    );
    assert!(
        stdout.contains("[3]=\"_cmdq_prompt_end\""),
        "cmdq prompt cleanup hook was not appended: {stdout}"
    );
}

fn wait_for_bytes(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    accum: &mut Vec<u8>,
    needle: &[u8],
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if accum.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Ok(b) = rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            accum.extend_from_slice(&b);
        }
    }
    accum.windows(needle.len()).any(|w| w == needle)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
