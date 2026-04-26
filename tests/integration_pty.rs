//! Integration test: spawn a real bash shell with cmdq's OSC 133 integration
//! sourced, drive a command through it, and verify the OSC 133 markers reach
//! the master side. This proves the wiring end-to-end without a TUI.

use std::io::{Read, Write};
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
    assert!(saw_prompt_start, "expected 133;A marker before first prompt; got {} bytes", accum.len());

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
        accum.windows(b"hello-cmdq".len()).any(|w| w == b"hello-cmdq"),
        "expected command output 'hello-cmdq' in stream"
    );

    // Clean up.
    writer.write_all(b"exit\n").unwrap();
    writer.flush().unwrap();
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
        format!(
            "PS1='$ '\nsource \"{}\"\n",
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
        if events
            .iter()
            .any(|e| matches!(e, Event::CommandEnd { .. }))
        {
            break;
        }
    }

    let saw_start = events
        .iter()
        .any(|e| matches!(e, Event::CommandStart));
    let saw_end_zero = events
        .iter()
        .any(|e| matches!(e, Event::CommandEnd { exit_code: Some(0) }));
    assert!(saw_start, "detector missed CommandStart; events: {:?}", events);
    assert!(
        saw_end_zero,
        "detector missed CommandEnd exit=0; events: {:?}",
        events
    );

    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();
}
