//! End-to-end test: start a bash shell with cmdq's OSC 133 integration, then
//! simulate cmdq's main-loop dispatch logic — when CommandEnd is seen, write
//! the next queued command to the PTY. Verify the queued command runs and
//! conditional-on-success chaining is respected.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use cmdq::osc133::{Detector, Event};
use cmdq::queue::Queue;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const SHELL_INTEGRATION_BASH: &str = include_str!("../shell/integration.bash");

fn write_temp_files(suffix: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("cmdq-e2e-{}-{}", std::process::id(), suffix));
    std::fs::create_dir_all(&dir).unwrap();
    let int_path = dir.join("integration.bash");
    std::fs::write(&int_path, SHELL_INTEGRATION_BASH).unwrap();
    let rcfile = dir.join("bashrc");
    std::fs::write(
        &rcfile,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    (int_path, rcfile)
}

fn spawn_bash(
    rcfile: &std::path::Path,
) -> (
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
    Box<dyn portable_pty::Child + Send + Sync>,
) {
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
    cmd.arg(rcfile);
    cmd.arg("-i");
    cmd.env("CMDQ_ACTIVE", "1");
    cmd.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    (reader, writer, child)
}

#[test]
fn dispatches_queued_commands_after_running_one() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let (_int, rcfile) = write_temp_files("dispatch");
    let (reader, mut writer, mut child) = spawn_bash(&rcfile);

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut reader = reader;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }

    let mut detector = Detector::new();
    let mut queue = Queue::new();
    queue.push("echo QUEUED_FIRST", false);
    queue.push("echo QUEUED_SECOND", false);

    let mut accum: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);

    // Wait for first PromptStart, then write a long-ish command. While that
    // runs, the queue is "ready"; when CommandEnd fires, we dispatch the
    // next queued command. Loop until queue is empty AND we have seen
    // QUEUED_SECOND in the output.
    let mut wrote_initial = false;

    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
            for ev in detector.feed(&b) {
                match ev {
                    Event::PromptStart => {
                        if !wrote_initial {
                            wrote_initial = true;
                            // Run a slow-ish first command. The queue should
                            // dispatch when this finishes.
                            writer.write_all(b"sleep 0.2; echo INITIAL_DONE\n").unwrap();
                            writer.flush().unwrap();
                        }
                    }
                    Event::CommandEnd { exit_code } => {
                        if !queue.is_empty()
                            && let Some(item) = queue.pop_next_eligible(exit_code)
                        {
                            writer.write_all(item.command.as_bytes()).unwrap();
                            writer.write_all(b"\n").unwrap();
                            writer.flush().unwrap();
                        }
                    }
                    _ => {}
                }
            }
        }

        if accum
            .windows(b"QUEUED_SECOND".len())
            .filter(|w| *w == b"QUEUED_SECOND")
            .count()
            > 0
            && accum
                .windows(b"QUEUED_FIRST".len())
                .filter(|w| *w == b"QUEUED_FIRST")
                .count()
                > 0
        {
            break;
        }
    }

    // Cleanup.
    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();

    let s = String::from_utf8_lossy(&accum);
    // Each output should appear at least twice — once as the typed echo'd
    // input, once as the actual `echo` output. We just check >=1.
    assert!(s.contains("INITIAL_DONE"), "missing initial command output");
    assert!(
        s.contains("QUEUED_FIRST"),
        "first queued command did not run"
    );
    assert!(
        s.contains("QUEUED_SECOND"),
        "second queued command did not run"
    );
    assert!(queue.is_empty(), "queue not drained");
    // Order: INITIAL_DONE before QUEUED_FIRST before QUEUED_SECOND in output.
    let pos_initial = s.find("INITIAL_DONE").unwrap();
    // Look for the *output* occurrence of QUEUED_FIRST — i.e. the second
    // appearance, since the first is the echoed input line.
    let echoes_only = |needle: &str, s: &str| -> Option<usize> {
        let mut indices: Vec<usize> = s.match_indices(needle).map(|(i, _)| i).collect();
        if indices.len() >= 2 {
            Some(indices.remove(1))
        } else {
            indices.first().copied()
        }
    };
    let pos_first = echoes_only("QUEUED_FIRST", &s).unwrap();
    let pos_second = echoes_only("QUEUED_SECOND", &s).unwrap();
    assert!(
        pos_initial < pos_first,
        "INITIAL should come before QUEUED_FIRST"
    );
    assert!(
        pos_first < pos_second,
        "QUEUED_FIRST should come before QUEUED_SECOND"
    );
}

#[test]
fn conditional_skips_after_failure() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let (_int, rcfile) = write_temp_files("cond");
    let (reader, mut writer, mut child) = spawn_bash(&rcfile);

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut reader = reader;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }

    let mut detector = Detector::new();
    let mut queue = Queue::new();
    // First queued is conditional — should be skipped because the initial
    // command will exit non-zero.
    queue.push("echo SHOULD_NOT_RUN", true);
    queue.push("echo ALWAYS_RUNS", false);

    let mut accum: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut wrote_initial = false;

    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
            for ev in detector.feed(&b) {
                match ev {
                    Event::PromptStart => {
                        if !wrote_initial {
                            wrote_initial = true;
                            writer.write_all(b"false\n").unwrap();
                            writer.flush().unwrap();
                        }
                    }
                    Event::CommandEnd { exit_code } => {
                        if !queue.is_empty()
                            && let Some(item) = queue.pop_next_eligible(exit_code)
                        {
                            writer.write_all(item.command.as_bytes()).unwrap();
                            writer.write_all(b"\n").unwrap();
                            writer.flush().unwrap();
                        }
                    }
                    _ => {}
                }
            }
        }
        let s = String::from_utf8_lossy(&accum);
        if s.contains("ALWAYS_RUNS\r\n") || s.matches("ALWAYS_RUNS").count() >= 2 {
            break;
        }
    }

    writer.write_all(b"exit\n").unwrap();
    let _ = child.wait();

    let s = String::from_utf8_lossy(&accum);
    assert!(
        s.contains("ALWAYS_RUNS"),
        "ALWAYS_RUNS missing in output: {}",
        s
    );
    // SHOULD_NOT_RUN should never have been written to the PTY.
    assert!(
        !s.contains("SHOULD_NOT_RUN"),
        "conditional command was not skipped"
    );
}
