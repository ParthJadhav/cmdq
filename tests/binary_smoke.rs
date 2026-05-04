//! Smoke test: spawn the actual `cmdq` binary inside a PTY, drive it with
//! some keystrokes, then exit cleanly. Verifies the whole binary starts
//! (raw mode + keyboard enhancement + bracketed paste set up cleanly) and
//! shuts down without panicking when the inner shell exits.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
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
fn cli_help_and_version_surface_public_flags() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        eprintln!("cmdq binary not built; skipping ({})", bin.display());
        return;
    }

    let help = Command::new(&bin).arg("--help").output().unwrap();
    assert!(
        help.status.success(),
        "--help failed: stdout={} stderr={}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    let help_stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help_stdout.contains("--shell"));
    assert!(help_stdout.contains("--install-integration"));
    assert!(help_stdout.contains("--print-integration"));

    let version = Command::new(&bin).arg("--version").output().unwrap();
    assert!(
        version.status.success(),
        "--version failed: stdout={} stderr={}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
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
    cmd.env(
        "HOME",
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
    );

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

    // Wait for cmdq to start up and produce *some* bytes (terminal-setup
    // sequences and the inner shell's first prompt). Then send `exit\n` to
    // the child shell — once the shell exits, cmdq should exit too.
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
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();

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
fn binary_normal_exit_removes_session_lease() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-lease-normal-exit-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();
    let queue_path = xdg.join("cmdq").join("queue.json");

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    let lease_created = wait_until(Duration::from_secs(5), || {
        cmdq::session_lease::active_peer_count(&queue_path).unwrap_or(0) == 1
    });
    assert!(lease_created, "cmdq did not create a session lease");

    writer.write_all(b"exit\r").unwrap();
    writer.flush().unwrap();

    let exited = wait_until(Duration::from_secs(5), || {
        child.try_wait().ok().flatten().is_some()
    });
    let lease_removed = wait_until(Duration::from_secs(1), || {
        cmdq::session_lease::active_peer_count(&queue_path).unwrap_or(0) == 0
    });
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    let session_dirs_clean = wait_until(Duration::from_secs(1), || session_dirs_clean(&xdg));

    assert!(exited, "cmdq did not exit after shell `exit`");
    assert!(
        lease_removed,
        "cmdq session lease remained after normal exit"
    );
    assert!(
        session_dirs_clean,
        "cmdq synthetic shell session dirs remained after normal exit"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn binary_sigterm_removes_session_lease() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-lease-sigterm-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();
    let queue_path = xdg.join("cmdq").join("queue.json");

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let _writer = pair.master.take_writer().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    let lease_created = wait_until(Duration::from_secs(5), || {
        cmdq::session_lease::active_peer_count(&queue_path).unwrap_or(0) == 1
    });
    assert!(lease_created, "cmdq did not create a session lease");

    let pid = child.process_id().expect("cmdq child pid");
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to send SIGTERM to cmdq");

    let exited = wait_until(Duration::from_secs(5), || {
        child.try_wait().ok().flatten().is_some()
    });
    let lease_removed = wait_until(Duration::from_secs(1), || {
        cmdq::session_lease::active_peer_count(&queue_path).unwrap_or(0) == 0
    });
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    let session_dirs_clean = wait_until(Duration::from_secs(1), || session_dirs_clean(&xdg));

    assert!(exited, "cmdq did not exit after SIGTERM");
    assert!(
        lease_removed,
        "cmdq session lease remained after SIGTERM cleanup"
    );
    assert!(
        session_dirs_clean,
        "cmdq synthetic shell session dirs remained after SIGTERM cleanup"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cli_install_integration_uses_temp_home_and_xdg_data_home() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/bash")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg)
        .env_remove("SHELL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install integration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let bashrc = std::fs::read_to_string(home.join(".bashrc")).unwrap();
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(bashrc.contains("cmdq shell integration"));
    assert!(bashrc.contains("integration.bash"));
    assert!(report.contains("integration.bash"));
    assert!(xdg.join("cmdq").join("integration.bash").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cli_install_integration_uses_absolute_zdotdir_for_zsh() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-zdotdir-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let zdotdir = tmp.join("zdotdir");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&zdotdir).unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/zsh")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg)
        .env("ZDOTDIR", &zdotdir)
        .env_remove("SHELL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "zsh install integration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !home.join(".zshrc").exists(),
        "install should not write HOME/.zshrc when ZDOTDIR is set"
    );
    let zshrc = std::fs::read_to_string(zdotdir.join(".zshrc")).unwrap();
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(zshrc.contains("cmdq shell integration"));
    assert!(zshrc.contains("integration.zsh"));
    assert!(report.contains(&format!("{}", zdotdir.join(".zshrc").display())));
    assert!(xdg.join("cmdq").join("integration.zsh").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn zsh_auto_integration_sources_absolute_zdotdir_zshrc() {
    if !std::path::Path::new("/bin/zsh").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-zdotdir-smoke-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let zdotdir = tmp.join("zdotdir");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&zdotdir).unwrap();
    let side_effect = tmp.join("zdotdir-loaded.txt");
    std::fs::write(
        zdotdir.join(".zshrc"),
        format!(
            "print -r -- ZDOTDIR_RC_LOADED > {}\nPS1='ZDOT> '\n",
            shell_quote(&side_effect)
        ),
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg("/bin/zsh");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);
    cmd.env("ZDOTDIR", &zdotdir);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            contains_bytes(s, b"\x1b]133;A")
                && contains_bytes(s, b"ZDOT>")
                && file_contains(&side_effect, "ZDOTDIR_RC_LOADED")
        }),
        "zsh auto integration did not source custom ZDOTDIR rc; output:\n{} side_effect={:?}",
        String::from_utf8_lossy(&accum),
        std::fs::read_to_string(&side_effect).ok()
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn zsh_auto_integration_survives_zshenv_repointing_zdotdir() {
    let Some(zsh) = find_shell(&[
        "/opt/homebrew/bin/zsh",
        "/usr/local/bin/zsh",
        "/bin/zsh",
        "/usr/bin/zsh",
    ]) else {
        return;
    };
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-zshenv-repoint-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let repointed_zdotdir = home.join(".config").join("zsh");
    std::fs::create_dir_all(&repointed_zdotdir).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let side_effect = tmp.join("repointed-zshrc-loaded.txt");
    std::fs::write(
        home.join(".zshenv"),
        "export ZDOTDIR=\"$HOME/.config/zsh\"\n",
    )
    .unwrap();
    std::fs::write(
        repointed_zdotdir.join(".zshrc"),
        format!(
            "print -r -- REPOINTED_ZSHRC_LOADED > {}\nPS1='REPOINT> '\n",
            shell_quote(&side_effect)
        ),
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg(zsh);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
    let loaded = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        contains_bytes(s, b"\x1b]133;A")
            && contains_bytes(s, b"REPOINT>")
            && file_contains(&side_effect, "REPOINTED_ZSHRC_LOADED")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let marker = std::fs::read_to_string(&side_effect).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        loaded,
        "zsh auto integration did not survive .zshenv ZDOTDIR repoint; marker={marker:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn cli_install_integration_ignores_relative_xdg_data_home() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-relative-xdg-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/bash")
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", "relative-data")
        .env_remove("SHELL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install integration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let bashrc = std::fs::read_to_string(home.join(".bashrc")).unwrap();
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(bashrc.contains("cmdq shell integration"));
    assert!(bashrc.contains("integration.bash"));
    assert!(report.contains("integration.bash"));
    assert!(!cwd.join("relative-data").join("cmdq").exists());
    assert!(!report.contains("relative-data"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cli_install_integration_preserves_invalid_utf8_rc() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-invalid-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let rc = home.join(".bashrc");
    let original = vec![b'a', 0xff, b'b', b'\n'];
    std::fs::write(&rc, &original).unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/bash")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg)
        .env_remove("SHELL")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "invalid UTF-8 rc should fail instead of being rewritten"
    );
    assert_eq!(std::fs::read(&rc).unwrap(), original);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cli_install_integration_refreshes_existing_managed_block() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-refresh-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let rc = home.join(".bashrc");
    std::fs::write(
        &rc,
        "before\n# >>> cmdq shell integration >>>\n[ -f '/old/integration.bash' ] && . '/old/integration.bash'\n# <<< cmdq shell integration <<<\nafter\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/bash")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg)
        .env_remove("SHELL")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "install integration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bashrc = std::fs::read_to_string(&rc).unwrap();
    assert!(bashrc.contains("before\n"));
    assert!(bashrc.contains("\nafter\n"));
    assert!(!bashrc.contains("/old/integration.bash"));
    assert_eq!(
        bashrc.matches("# >>> cmdq shell integration >>>").count(),
        1
    );
    assert!(bashrc.contains(&format!(
        "{}",
        xdg.join("cmdq").join("integration.bash").display()
    )));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[cfg(unix)]
fn cli_install_integration_preserves_symlinked_rc_file() {
    use std::os::unix::fs::symlink;

    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cli-install-symlink-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let dotfiles = tmp.join("dotfiles");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("bashrc");
    std::fs::write(&target, "alias ll='ls -la'\n").unwrap();
    symlink(&target, home.join(".bashrc")).unwrap();

    let output = Command::new(&bin)
        .arg("--install-integration")
        .arg("--shell")
        .arg("/bin/bash")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg)
        .env_remove("SHELL")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "install integration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::symlink_metadata(home.join(".bashrc"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let target_contents = std::fs::read_to_string(&target).unwrap();
    assert!(target_contents.contains("alias ll"));
    assert!(target_contents.contains("cmdq shell integration"));
    assert!(target_contents.contains("integration.bash"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cli_print_integration_sh_fails_loudly() {
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let output = Command::new(bin)
        .args(["--print-integration", "sh"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "sh integration should fail instead of printing bash syntax"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("POSIX sh has no reliable preexec hook"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    let tmp = std::env::temp_dir().join(format!(
        "cmdq-smoke-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
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

    // Type a queued command and Enter. The assertion below checks the file
    // side effect, not just terminal text, because the panel can echo queued
    // command strings before they run.
    let queued_side_effect = tmp.join("queued-side-effect.txt");
    let queued_cmd = format!(
        "printf 'SMOKE_QUEUED_SIDE_EFFECT\\n' > {}\r",
        shell_quote(&queued_side_effect)
    );
    writer.write_all(queued_cmd.as_bytes()).unwrap();
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

    // Wait until the trigger appears and the queued command actually runs.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        let s = String::from_utf8_lossy(&accum);
        if s.contains("SMOKE_TRIGGER")
            && file_contains(&queued_side_effect, "SMOKE_QUEUED_SIDE_EFFECT")
        {
            break;
        }
    }

    let s = String::from_utf8_lossy(&accum);
    let side_effect = std::fs::read_to_string(&queued_side_effect).unwrap_or_default();

    // Cleanup.
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        s.contains("SMOKE_TRIGGER"),
        "trigger command did not run; output:\n{}",
        s
    );
    assert!(
        side_effect.contains("SMOKE_QUEUED_SIDE_EFFECT"),
        "queued command did not create side effect; output:\n{}",
        s
    );
}

#[test]
fn binary_prompt_ctrl_w_edit_runs_edited_command() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-prompt-edit-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let output_file = tmp.join("prompt-edit.txt");
    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "prompt-edit smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let edited = format!("printf 'NOPE'\x17'OK\\n' > {}\r", shell_quote(&output_file));
    writer.write_all(edited.as_bytes()).unwrap();
    writer.flush().unwrap();

    let ran = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&output_file, "OK")
    });

    let contents = std::fs::read_to_string(&output_file).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        ran,
        "edited prompt command did not create expected file; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(contents, "OK\n");
}

#[test]
fn binary_panel_ctrl_line_editing_runs_edited_queue_item() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-panel-edit-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let output_file = tmp.join("panel-edit.txt");
    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "panel-edit smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q: force queue mode.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer.write_all(b"garbage ").unwrap();
    writer.write_all(&[0x17]).unwrap(); // Ctrl-W: delete previous word.
    writer.write_all(b"printf 'OKX").unwrap();
    writer.write_all(b"\x1bb").unwrap(); // Alt-B: jump to start of word.
    writer.write_all(b"\x1bf").unwrap(); // Alt-F: jump back to word end.
    writer.write_all(&[0x02]).unwrap(); // Ctrl-B: move before X.
    writer.write_all(&[0x06]).unwrap(); // Ctrl-F: move after X.
    writer.write_all(&[0x08]).unwrap(); // Ctrl-H/backspace: delete X.
    let rest = format!("\\n' > {}\r", shell_quote(&output_file));
    writer.write_all(rest.as_bytes()).unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer.write_all(&[0x11]).unwrap(); // Leave force queue mode.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer.write_all(b"echo PANEL_EDIT_TRIGGER\r").unwrap();
    writer.flush().unwrap();

    let ran = wait_for(&rx, &mut accum, Duration::from_secs(8), |s| {
        String::from_utf8_lossy(s).contains("PANEL_EDIT_TRIGGER")
            && file_contains(&output_file, "OK")
    });

    let contents = std::fs::read_to_string(&output_file).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        ran,
        "edited panel queue item did not run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(contents, "OK\n");
}

#[test]
fn binary_prompt_alt_edit_fast_followup_queues_next_command() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-alt-fast-followup-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = home.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let first_file = tmp.join("alt-first.txt");
    let second_file = tmp.join("alt-second.txt");
    let queue_path = xdg.join("cmdq").join("queue.json");
    let pair = native_pty_system()
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
    cmd.cwd(&tmp);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "alt fast-followup smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let first = format!(
        "sleep 2; printf 'FIRST_DONE\\n' > {}",
        shell_quote(&first_file)
    );
    let second = format!("printf 'SECOND_DONE\\n' > {}\r", shell_quote(&second_file));
    writer.write_all(first.as_bytes()).unwrap();
    writer.write_all(b"\x1bb\x1bf\r").unwrap(); // Alt-B/F before Enter.
    writer.write_all(second.as_bytes()).unwrap();
    writer.flush().unwrap();

    let queued_before_first_finished = wait_until(Duration::from_secs(1), || {
        file_contains(&queue_path, "SECOND_DONE") && !first_file.exists()
    });
    let ran = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&first_file, "FIRST_DONE") && file_contains(&second_file, "SECOND_DONE")
    });

    let first_contents = std::fs::read_to_string(&first_file).unwrap_or_default();
    let second_contents = std::fs::read_to_string(&second_file).unwrap_or_default();
    let queue_snapshot = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        queued_before_first_finished,
        "fast follow-up command was not persisted in cmdq queue while first command was running; queue={queue_snapshot:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran,
        "commands did not both run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(first_contents, "FIRST_DONE\n");
    assert_eq!(second_contents, "SECOND_DONE\n");
}

#[test]
fn binary_prompt_newline_paste_fast_followup_queues_next_command() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-paste-fast-followup-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let first_file = home.join("paste-first.txt");
    let second_file = home.join("paste-second.txt");
    let queue_path = xdg.join("cmdq").join("queue.json");
    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "paste fast-followup smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let first = "sleep 2; printf 'PASTE_FIRST_DONE\\n' > paste-first.txt\n";
    let second = "printf 'PASTE_SECOND_DONE\\n' > paste-second.txt\r";
    writer.write_all(b"\x1b[200~").unwrap();
    writer.write_all(first.as_bytes()).unwrap();
    writer.write_all(b"\x1b[201~").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    writer.write_all(second.as_bytes()).unwrap();
    writer.flush().unwrap();

    let queued_before_first_finished = wait_until(Duration::from_secs(1), || {
        file_contains(&queue_path, "PASTE_SECOND_DONE") && !first_file.exists()
    });
    let ran = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&first_file, "PASTE_FIRST_DONE")
            && file_contains(&second_file, "PASTE_SECOND_DONE")
    });

    let first_contents = std::fs::read_to_string(&first_file).unwrap_or_default();
    let second_contents = std::fs::read_to_string(&second_file).unwrap_or_default();
    let queue_snapshot = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        queued_before_first_finished,
        "fast follow-up after newline paste was not persisted while first command was running; queue={queue_snapshot:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran,
        "pasted command and queued follow-up did not both run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(first_contents, "PASTE_FIRST_DONE\n");
    assert_eq!(second_contents, "PASTE_SECOND_DONE\n");
}

#[test]
fn binary_leading_noop_paste_fast_followup_queues_next_command() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-leading-noop-paste-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let first_file = home.join("leading-noop-first.txt");
    let second_file = home.join("leading-noop-second.txt");
    let queue_path = xdg.join("cmdq").join("queue.json");
    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "leading-noop paste smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let first = "\n# copied note before command\nsleep 2; printf 'LEADING_NOOP_FIRST_DONE\\n' > leading-noop-first.txt\n";
    let second = "printf 'LEADING_NOOP_SECOND_DONE\\n' > leading-noop-second.txt\r";
    writer.write_all(b"\x1b[200~").unwrap();
    writer.write_all(first.as_bytes()).unwrap();
    writer.write_all(b"\x1b[201~").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    writer.write_all(second.as_bytes()).unwrap();
    writer.flush().unwrap();

    let queued_before_first_finished = wait_until(Duration::from_secs(1), || {
        file_contains(&queue_path, "LEADING_NOOP_SECOND_DONE") && !first_file.exists()
    });
    let ran = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&first_file, "LEADING_NOOP_FIRST_DONE")
            && file_contains(&second_file, "LEADING_NOOP_SECOND_DONE")
    });

    let first_contents = std::fs::read_to_string(&first_file).unwrap_or_default();
    let second_contents = std::fs::read_to_string(&second_file).unwrap_or_default();
    let queue_snapshot = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        queued_before_first_finished,
        "fast follow-up after leading-noop paste was not persisted while first command was running; queue={queue_snapshot:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran,
        "leading-noop pasted command and queued follow-up did not both run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(first_contents, "LEADING_NOOP_FIRST_DONE\n");
    assert_eq!(second_contents, "LEADING_NOOP_SECOND_DONE\n");
}

#[test]
fn binary_esc_clear_then_esc_stays_in_queue_editor() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-esc-clear-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = home.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let side_effect = tmp.join("esc-clear.txt");
    let queue_path = xdg.join("cmdq").join("queue.json");
    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "esc clear smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q: force queue mode.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer.write_all(b"draft").unwrap();
    writer.write_all(b"\x1b\x1b").unwrap(); // clear draft, then press Esc again.
    let queued = format!("printf 'ESC_CLEAR_OK\\n' > {}\r", shell_quote(&side_effect));
    writer.write_all(queued.as_bytes()).unwrap();
    writer.flush().unwrap();

    let queued_not_leaked = wait_until(Duration::from_secs(1), || {
        file_contains(&queue_path, "ESC_CLEAR_OK") && !side_effect.exists()
    });

    writer.write_all(&[0x11]).unwrap(); // Leave force queue mode.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo ESC_CLEAR_TRIGGER\r").unwrap();
    writer.flush().unwrap();

    let ran = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        String::from_utf8_lossy(s).contains("ESC_CLEAR_TRIGGER")
            && file_contains(&side_effect, "ESC_CLEAR_OK")
    });

    let contents = std::fs::read_to_string(&side_effect).unwrap_or_default();
    let queue_snapshot = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        queued_not_leaked,
        "Esc clear + Esc should leave next command in cmdq queue, not raw shell input; queue={queue_snapshot:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran,
        "queued command did not run after trigger; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(contents, "ESC_CLEAR_OK\n");
}

#[test]
fn binary_queue_does_not_dispatch_item_being_edited() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-edit-freeze-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = home.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let side_effect = tmp.join("edit-freeze.txt");
    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "edit-freeze smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q: force queue mode.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let queued = format!(
        "printf 'EDIT_FREEZE_OK\\n' > {}\r",
        shell_quote(&side_effect)
    );
    writer.write_all(queued.as_bytes()).unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x11]).unwrap(); // Return keys to the shell.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer
        .write_all(b"sleep 2; echo EDIT_FREEZE_TRIGGER\r")
        .unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(1700));
    writer.write_all(b"\x1b[A").unwrap(); // Start editing queued item while command is running.
    writer.flush().unwrap();

    let stayed_queued = wait_for(&rx, &mut accum, Duration::from_secs(2), |s| {
        let output = String::from_utf8_lossy(s);
        output.contains("EDIT_FREEZE_TRIGGER")
            && output.contains("queue paused")
            && !side_effect.exists()
    });

    writer.write_all(b"\r").unwrap(); // Save unchanged edit.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    writer.write_all(&[0x18]).unwrap(); // Ctrl-X: resume paused queue.
    writer.flush().unwrap();

    let ran_after_resume = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&side_effect, "EDIT_FREEZE_OK")
    });

    let contents = std::fs::read_to_string(&side_effect).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        stayed_queued,
        "queued item dispatched while it was being edited; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran_after_resume,
        "queued item did not run after save/resume; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(contents, "EDIT_FREEZE_OK\n");
}

#[test]
fn binary_multiline_paste_preserves_heredoc_queue_item() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-heredoc-paste-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();

    let heredoc_file = tmp.join("heredoc-output.txt");
    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "heredoc paste smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let block = format!(
        "cat > {} <<'EOF'\nhello from heredoc\nEOF\n",
        shell_quote(&heredoc_file)
    );
    writer.write_all(b"\x1b[200~").unwrap();
    writer.write_all(block.as_bytes()).unwrap();
    writer.write_all(b"\x1b[201~").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    writer.write_all(b"\r").unwrap(); // enqueue pasted block
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x11]).unwrap(); // return to shell
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo HEREDOC_TRIGGER\r").unwrap();
    writer.flush().unwrap();

    let ran = wait_for(&rx, &mut accum, Duration::from_secs(8), |_| {
        file_contains(&heredoc_file, "hello from heredoc")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        ran,
        "queued heredoc block did not run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn running_read_prompt_receives_input_instead_of_queue_capture() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-read-prompt-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    let answer_file = tmp.join("read-answer.txt");

    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "read-prompt smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let read_cmd = format!(
        "read -p 'name? ' name; printf '%s\\n' \"$name\" > {}\r",
        shell_quote(&answer_file)
    );
    writer.write_all(read_cmd.as_bytes()).unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("name? ")
        }),
        "read prompt did not appear; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    // Wait past the queue-panel delay. Before the child-input prompt
    // heuristic, this plaintext answer was captured into cmdq's queue.
    std::thread::sleep(Duration::from_millis(1800));
    writer.write_all(b"alice\r").unwrap();
    writer.flush().unwrap();

    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
            file_contains(&answer_file, "alice")
        }),
        "read prompt input was not delivered to child; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn running_press_enter_prompt_receives_enter_instead_of_queue_capture() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-press-enter-prompt-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    let done_file = tmp.join("press-enter-done.txt");

    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "press-enter smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let read_cmd = format!(
        "printf 'Press ENTER to continue'; read _; printf DONE > {}\r",
        shell_quote(&done_file)
    );
    writer.write_all(read_cmd.as_bytes()).unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("Press ENTER to continue")
        }),
        "press-enter prompt did not appear; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    std::thread::sleep(Duration::from_millis(1800));
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();

    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
            file_contains(&done_file, "DONE")
        }),
        "Enter was not delivered to child prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn running_press_any_key_prompt_releases_after_single_key_answer() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-press-any-key-prompt-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let int_path = home.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = home.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    let done_file = tmp.join("press-any-key-done.txt");
    let queued_file = tmp.join("press-any-key-queued.txt");
    let queue_path = xdg.join("cmdq").join("queue.json");

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "press-any-key smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let read_cmd = format!(
        "printf 'Press any key to continue'; IFS= read -r -n 1 _; printf 'AFTER_ANY_KEY'; sleep 3; printf DONE > {}\r",
        shell_quote(&done_file)
    );
    writer.write_all(read_cmd.as_bytes()).unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("Press any key to continue")
        }),
        "press-any-key prompt did not appear; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    std::thread::sleep(Duration::from_millis(1800));
    writer.write_all(b"x").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("AFTER_ANY_KEY")
        }),
        "single-key answer did not reach child; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let queued_cmd = format!("printf 'ANY_KEY_QUEUED' > {}\r", shell_quote(&queued_file));
    writer.write_all(queued_cmd.as_bytes()).unwrap();
    writer.flush().unwrap();

    let queued_before_child_done = wait_until(Duration::from_secs(1), || {
        std::fs::read_to_string(&queue_path)
            .map(|s| s.contains("ANY_KEY_QUEUED"))
            .unwrap_or(false)
    });
    let ran = wait_for(&rx, &mut accum, Duration::from_secs(6), |_| {
        file_contains(&done_file, "DONE") && file_contains(&queued_file, "ANY_KEY_QUEUED")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let queue_snapshot = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        queued_before_child_done,
        "follow-up after any-key prompt was not captured into cmdq queue; queue={queue_snapshot:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        ran,
        "queued follow-up after any-key prompt did not run; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn running_read_prompt_receives_paste_instead_of_queue_capture() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-read-paste-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    let answer_file = tmp.join("read-paste-answer.txt");

    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "read-paste smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let read_cmd = format!(
        "read -p 'name? ' name; printf '%s\\n' \"$name\" > {}\r",
        shell_quote(&answer_file)
    );
    writer.write_all(read_cmd.as_bytes()).unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("name? ")
        }),
        "read prompt did not appear; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    // Wait past the queue-panel delay, then send an outer bracketed-paste
    // event. It should answer the child prompt, not become a queued command;
    // because `read` did not request bracketed paste, the child should receive
    // plain text without marker bytes.
    std::thread::sleep(Duration::from_millis(1800));
    writer.write_all(b"\x1b[200~alice\n\x1b[201~").unwrap();
    writer.flush().unwrap();

    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
            std::fs::read_to_string(&answer_file)
                .map(|s| s == "alice\n")
                .unwrap_or(false)
        }),
        "read prompt paste was not delivered cleanly to child; file={:?}; output:\n{}",
        std::fs::read_to_string(&answer_file).ok(),
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn multiline_if_continuation_stays_with_shell_after_panel_delay() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-if-continuation-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let int_path = tmp.join("integration.bash");
    std::fs::write(&int_path, include_str!("../shell/integration.bash")).unwrap();
    let bashrc = tmp.join(".bashrc");
    std::fs::write(
        &bashrc,
        format!("PS1='$ '\nPS2='> '\nsource \"{}\"\n", int_path.display()),
    )
    .unwrap();
    let side_effect = tmp.join("if-body-ran.txt");

    let pair = native_pty_system()
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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "if-continuation smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"if true\r").unwrap();
    writer.flush().unwrap();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            String::from_utf8_lossy(s).contains("> ")
        }),
        "bash did not enter PS2 continuation prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    // Wait long enough that a false "command is running" guess would open the
    // queue panel and capture these continuation lines.
    std::thread::sleep(Duration::from_millis(1800));
    let body = format!("printf 'IF_BODY_OK\\n' > {}", shell_quote(&side_effect));
    for line in ["then".to_string(), body, "fi".to_string()] {
        writer.write_all(line.as_bytes()).unwrap();
        writer.write_all(b"\r").unwrap();
        writer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
            file_contains(&side_effect, "IF_BODY_OK")
        }),
        "multiline if body did not execute; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn binary_auto_installs_bash_integration_for_clean_home() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    // No .bashrc and no pre-installed cmdq block. cmdq should still launch
    // bash with its generated rcfile so OSC 133 markers drive dispatch.
    let tmp = std::env::temp_dir().join(format!(
        "cmdq-clean-home-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

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
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        if accum
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")
        {
            break;
        }
    }
    assert!(
        accum
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A"),
        "clean bash home did not emit prompt marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // Ctrl-Q
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer
        .write_all(b"printf 'CLEAN_HOME_QUEUED_OUTPUT\\n'\r")
        .unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x11]).unwrap(); // leave force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo CLEAN_HOME_TRIGGER\r").unwrap();
    writer.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
            accum.extend_from_slice(&b);
        }
        let s = String::from_utf8_lossy(&accum);
        if s.contains("CLEAN_HOME_TRIGGER\r\n") && s.contains("CLEAN_HOME_QUEUED_OUTPUT\r\n") {
            break;
        }
    }

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    let s = String::from_utf8_lossy(&accum);
    assert!(
        s.contains("CLEAN_HOME_TRIGGER"),
        "trigger command did not run; output:\n{}",
        s
    );
    assert!(
        s.find("CLEAN_HOME_TRIGGER\r\n")
            .zip(s.find("CLEAN_HOME_QUEUED_OUTPUT\r\n"))
            .map(|(trigger, queued)| trigger < queued)
            .unwrap_or(false),
        "queued command output did not appear after trigger output; output:\n{}",
        s
    );
}

#[test]
fn bash_auto_integration_sources_bash_profile_when_bashrc_absent() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-bash-profile-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::write(
        home.join(".bash_profile"),
        "export CMDQ_PROFILE_MARKER=profile-loaded\n",
    )
    .unwrap();
    let marker_file = tmp.join("profile-marker.txt");

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "bash-profile smoke never saw shell prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    let cmdline = format!(
        "printf '%s\\n' \"$CMDQ_PROFILE_MARKER\" > {}\r",
        shell_quote(&marker_file)
    );
    writer.write_all(cmdline.as_bytes()).unwrap();
    writer.flush().unwrap();

    let loaded = wait_for(&rx, &mut accum, Duration::from_secs(5), |_| {
        file_contains(&marker_file, "profile-loaded")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let marker = std::fs::read_to_string(&marker_file).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        loaded,
        ".bash_profile was not sourced by generated bash rcfile; marker={marker:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn binary_auto_installs_zsh_integration_for_clean_home() {
    let Some(zsh) = find_shell(&[
        "/opt/homebrew/bin/zsh",
        "/usr/local/bin/zsh",
        "/bin/zsh",
        "/usr/bin/zsh",
    ]) else {
        return;
    };
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-clean-zsh-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg(zsh);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
    let saw_prompt = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
    });
    assert!(
        saw_prompt,
        "clean zsh home did not emit prompt marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"true\r").unwrap();
    writer.flush().unwrap();
    let saw_end = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        s.windows(b"\x1b]133;D;0".len())
            .any(|w| w == b"\x1b]133;D;0")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        saw_end,
        "clean zsh home did not emit command end marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn binary_auto_installs_fish_integration_for_clean_home() {
    let Some(fish) = find_shell(&[
        "/opt/homebrew/bin/fish",
        "/usr/local/bin/fish",
        "/usr/bin/fish",
        "/bin/fish",
    ]) else {
        return;
    };
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-clean-fish-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin.as_os_str());
    cmd.arg("--shell");
    cmd.arg(fish);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
    let saw_prompt = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
    });
    assert!(
        saw_prompt,
        "clean fish home did not emit prompt marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(b"true\r").unwrap();
    writer.flush().unwrap();
    let saw_end = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        s.windows(b"\x1b]133;D;0".len())
            .any(|w| w == b"\x1b]133;D;0")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        saw_end,
        "clean fish home did not emit command end marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn restored_queue_resumes_and_dispatches_at_prompt() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-restored-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();

    let side_effect = tmp.join("restored-side-effect.txt");
    let queued_command = format!(
        "printf 'RESTORED_QUEUE_RAN\\n' > {}",
        shell_quote(&side_effect)
    );
    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = serde_json::json!({
        "items": [
            {
                "id": 0,
                "command": queued_command,
                "conditional": false
            }
        ],
        "next_id": 1,
        "paused": false
    });
    std::fs::write(&queue_path, queue_json.to_string()).unwrap();

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
    let saw_prompt = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
    });
    assert!(
        saw_prompt,
        "restored-queue test never saw prompt marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x18]).unwrap(); // Ctrl-X resumes the restored queue.
    writer.flush().unwrap();

    let dispatched = wait_for(&rx, &mut accum, Duration::from_secs(8), |_| {
        file_contains(&side_effect, "RESTORED_QUEUE_RAN") && queue_file_is_empty(&queue_path)
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        dispatched,
        "restored queue did not dispatch and persist empty queue; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn restored_queue_startup_mentions_active_peer_session() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-peer-session-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();

    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = serde_json::json!({
        "items": [
            {
                "id": 0,
                "command": "echo keep-me",
                "conditional": false
            }
        ],
        "next_id": 1,
        "paused": false
    });
    std::fs::write(&queue_path, queue_json.to_string()).unwrap();

    let first_pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut first_cmd = CommandBuilder::new(bin.as_os_str());
    first_cmd.arg("--shell");
    first_cmd.arg("/bin/bash");
    first_cmd.env("TERM", "xterm-256color");
    first_cmd.env("HOME", &home);
    first_cmd.env("XDG_DATA_HOME", &xdg);

    let mut first_child = first_pair.slave.spawn_command(first_cmd).unwrap();
    drop(first_pair.slave);
    let mut first_reader = first_pair.master.try_clone_reader().unwrap();
    let _first_writer = first_pair.master.take_writer().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = first_reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    assert!(
        wait_until(Duration::from_secs(5), || {
            cmdq::session_lease::active_peer_count(&queue_path).unwrap_or(0) >= 1
        }),
        "first cmdq session did not create a session lease"
    );

    let second_pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut second_cmd = CommandBuilder::new(bin.as_os_str());
    second_cmd.arg("--shell");
    second_cmd.arg("/bin/bash");
    second_cmd.env("TERM", "xterm-256color");
    second_cmd.env("HOME", &home);
    second_cmd.env("XDG_DATA_HOME", &xdg);

    let mut second_child = second_pair.slave.spawn_command(second_cmd).unwrap();
    drop(second_pair.slave);

    let mut second_reader = second_pair.master.try_clone_reader().unwrap();
    let _second_writer = second_pair.master.take_writer().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = second_reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut accum = Vec::new();
    let reported_peer = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        contains_bytes(s, b"another cmdq session is active")
    });

    let _ = first_child.kill();
    let _ = second_child.kill();
    let _ = first_child.wait();
    let _ = second_child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        reported_peer,
        "second restored-queue startup did not report active peer session; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn restored_queue_from_other_cwd_requires_double_resume_confirmation() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-restored-cwd-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let origin = tmp.join("original-cwd");
    let current = tmp.join("current-cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&current).unwrap();

    let side_effect = tmp.join("restored-cwd-side-effect.txt");
    let queued_command = format!(
        "printf 'RESTORED_CWD_QUEUE_RAN\\n' > {}",
        shell_quote(&side_effect)
    );
    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = serde_json::json!({
        "items": [
            {
                "id": 0,
                "command": queued_command,
                "conditional": false
            }
        ],
        "next_id": 1,
        "origin_cwd": origin,
        "paused": false
    });
    std::fs::write(&queue_path, queue_json.to_string()).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&current);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
                && contains_bytes(s, b"from ")
        }),
        "restored-cwd test never saw prompt and cwd warning; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x18]).unwrap(); // first Ctrl-X confirms cwd mismatch.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !side_effect.exists(),
        "first Ctrl-X should warn, not dispatch queue from another cwd"
    );

    writer.write_all(b"x").unwrap(); // any other interaction cancels confirmation.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x18]).unwrap(); // warns again instead of dispatching.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !side_effect.exists(),
        "Ctrl-X after another key should warn again, not dispatch"
    );

    writer.write_all(&[0x15]).unwrap(); // Ctrl-U: clear the stray editor input.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x18]).unwrap(); // first fresh Ctrl-X warns.
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x18]).unwrap(); // second Ctrl-X resumes.
    writer.flush().unwrap();
    let dispatched = wait_for(&rx, &mut accum, Duration::from_secs(8), |_| {
        file_contains(&side_effect, "RESTORED_CWD_QUEUE_RAN") && queue_file_is_empty(&queue_path)
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        dispatched,
        "second Ctrl-X did not dispatch restored queue; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn restored_queue_rechecks_shell_cwd_after_bashrc_cd() {
    let bin = cmdq_binary_path();
    if !bin.exists() || !std::path::Path::new("/bin/bash").exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-restored-rc-cd-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let origin = tmp.join("original-cwd");
    let after_rc = tmp.join("after-rc-cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&after_rc).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        format!("cd {}\n", shell_quote(&after_rc)),
    )
    .unwrap();

    let side_effect = tmp.join("restored-rc-cd-side-effect.txt");
    let queued_command = format!(
        "printf 'RESTORED_RC_CD_QUEUE_RAN\\n' > {}",
        shell_quote(&side_effect)
    );
    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = serde_json::json!({
        "items": [
            {
                "id": 0,
                "command": queued_command,
                "conditional": false
            }
        ],
        "next_id": 1,
        "origin_cwd": origin,
        "paused": false
    });
    std::fs::write(&queue_path, queue_json.to_string()).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&origin);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
                && contains_bytes(s, b"restored")
        }),
        "restored-rc-cd test never saw prompt and restored warning; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x18]).unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !side_effect.exists(),
        "first Ctrl-X should warn because bashrc changed cwd"
    );

    writer.write_all(&[0x18]).unwrap();
    writer.flush().unwrap();
    let dispatched = wait_for(&rx, &mut accum, Duration::from_secs(8), |_| {
        file_contains(&side_effect, "RESTORED_RC_CD_QUEUE_RAN") && queue_file_is_empty(&queue_path)
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        dispatched,
        "second Ctrl-X did not dispatch restored queue after cwd confirmation; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn restored_queue_clears_stale_cwd_warning_after_bashrc_cd_back_to_origin() {
    let bin = cmdq_binary_path();
    if !bin.exists() || !std::path::Path::new("/bin/bash").exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-restored-rc-cd-back-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let origin = tmp.join("original-cwd");
    let launch_cwd = tmp.join("launch-cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(xdg.join("cmdq")).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&launch_cwd).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        format!("cd {}\n", shell_quote(&origin)),
    )
    .unwrap();

    let side_effect = tmp.join("restored-rc-cd-back-side-effect.txt");
    let queued_command = format!(
        "printf 'RESTORED_RC_CD_BACK_QUEUE_RAN\\n' > {}",
        shell_quote(&side_effect)
    );
    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = serde_json::json!({
        "items": [
            {
                "id": 0,
                "command": queued_command,
                "conditional": false
            }
        ],
        "next_id": 1,
        "origin_cwd": origin,
        "paused": false
    });
    std::fs::write(&queue_path, queue_json.to_string()).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&launch_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
                && contains_bytes(s, b"restored")
        }),
        "restored-rc-cd-back test never saw prompt and restored warning; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x18]).unwrap();
    writer.flush().unwrap();
    let dispatched = wait_for(&rx, &mut accum, Duration::from_secs(8), |_| {
        file_contains(&side_effect, "RESTORED_RC_CD_BACK_QUEUE_RAN")
            && queue_file_is_empty(&queue_path)
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        dispatched,
        "single Ctrl-X should dispatch once bashrc returns to queue origin; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn queued_command_origin_tracks_inner_shell_cd() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-inner-cwd-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let start_cwd = tmp.join("start");
    let inner_cwd = tmp.join("inner cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&start_cwd).unwrap();
    std::fs::create_dir_all(&inner_cwd).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&start_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "inner-cwd smoke never saw initial prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer
        .write_all(format!("cd {}\r", shell_quote(&inner_cwd)).as_bytes())
        .unwrap();
    writer.flush().unwrap();
    let inner_cwd_bytes = inner_cwd.to_string_lossy().into_owned();
    assert!(
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            s.windows(inner_cwd_bytes.len())
                .any(|w| w == inner_cwd_bytes.as_bytes())
                && s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
        }),
        "inner-cwd smoke never saw cwd report after cd; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo INNER_CWD_TRACKED\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let saved: serde_json::Value = serde_json::from_str(&queue_json).unwrap();

    let _ = writer.write_all(&[0x11]); // leave force queue before exiting shell
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert_eq!(
        saved.get("origin_cwd").and_then(|v| v.as_str()),
        Some(inner_cwd_bytes.as_str()),
        "queue origin should track inner shell cwd after cd; saved={queue_json}"
    );
}

#[test]
fn queued_command_pauses_after_shell_cd_before_auto_dispatch() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-cwd-auto-pause-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let start_cwd = tmp.join("start");
    let next_cwd = tmp.join("next");
    let side_effect = tmp.join("queued-pwd.txt");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&start_cwd).unwrap();
    std::fs::create_dir_all(&next_cwd).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&start_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "cwd-auto-pause smoke never saw initial prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let queued = format!("pwd > {}", shell_quote(&side_effect));
    writer.write_all(queued.as_bytes()).unwrap();
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    writer.write_all(&[0x11]).unwrap(); // return to shell
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer
        .write_all(format!("cd {}\r", shell_quote(&next_cwd)).as_bytes())
        .unwrap();
    writer.flush().unwrap();

    let warned = wait_for(&rx, &mut accum, Duration::from_secs(6), |s| {
        contains_bytes(s, b"Ctrl-X to run here")
    });
    assert!(
        warned,
        "cwd change did not pause queue before auto-dispatch; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert!(
        !side_effect.exists(),
        "queued command ran automatically after cd; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x18]).unwrap(); // confirm running the queue here
    writer.flush().unwrap();
    let ran_after_confirm = wait_for(&rx, &mut accum, Duration::from_secs(6), |_| {
        file_contains(&side_effect, &next_cwd.to_string_lossy())
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let side_effect_text = std::fs::read_to_string(&side_effect).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        ran_after_confirm,
        "queued command did not run in confirmed cwd; side_effect={side_effect_text:?}; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn queued_command_origin_handles_literal_percent_in_cwd() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-percent-cwd-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let percent_cwd = tmp.join("100%done");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&percent_cwd).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&percent_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "percent-cwd smoke never saw prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo PERCENT_CWD_TRACKED\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let saved: serde_json::Value = serde_json::from_str(&queue_json).unwrap();
    let expected = std::fs::canonicalize(&percent_cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let _ = writer.write_all(&[0x11]);
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert_eq!(
        saved.get("origin_cwd").and_then(|v| v.as_str()),
        Some(expected.as_str()),
        "literal percent in cwd should survive OSC 7 parsing; saved={queue_json}"
    );
}

#[test]
fn queued_command_origin_handles_bel_in_cwd() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-bel-cwd-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let bel_cwd = tmp.join("bell\u{7}dir");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&bel_cwd).unwrap();

    let pair = native_pty_system()
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
    cmd.cwd(&bel_cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| s
            .windows(b"\x1b]133;A".len())
            .any(|w| w == b"\x1b]133;A")),
        "bel-cwd smoke never saw prompt; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"echo BEL_CWD_TRACKED\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let queue_path = xdg.join("cmdq").join("queue.json");
    let queue_json = std::fs::read_to_string(&queue_path).unwrap_or_default();
    let saved: serde_json::Value = serde_json::from_str(&queue_json).unwrap();
    let expected = std::fs::canonicalize(&bel_cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let _ = writer.write_all(&[0x11]);
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert_eq!(
        saved.get("origin_cwd").and_then(|v| v.as_str()),
        Some(expected.as_str()),
        "BEL in cwd should be percent-encoded by shell hook and restored by parser; saved={queue_json}"
    );
}

#[test]
fn corrupt_persisted_queue_is_backed_up_and_reported_on_startup() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-corrupt-queue-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    let queue_dir = xdg.join("cmdq");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&queue_dir).unwrap();
    let queue_path = queue_dir.join("queue.json");
    std::fs::write(&queue_path, b"{broken queue").unwrap();

    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
    let reported = wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
        contains_bytes(s, b"cmdq: ignored corrupt queue file")
    });
    let backups: Vec<_> = std::fs::read_dir(&queue_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("queue.json.corrupt-"))
                .unwrap_or(false)
        })
        .collect();

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        reported,
        "corrupt queue backup warning was not visible; output:\n{}",
        String::from_utf8_lossy(&accum)
    );
    assert_eq!(backups.len(), 1, "expected one corrupt queue backup");
}

#[test]
fn queued_command_after_visible_panel_sees_full_terminal_height() {
    if !std::path::Path::new("/bin/bash").exists() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-stty-{}-{}",
        std::process::id(),
        monotonic_test_suffix()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let size_file = tmp.join("queued-size.txt");
    let pair = native_pty_system()
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
    cmd.env("HOME", &home);
    cmd.env("XDG_DATA_HOME", &xdg);

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
        wait_for(&rx, &mut accum, Duration::from_secs(5), |s| {
            s.windows(b"\x1b]133;A".len()).any(|w| w == b"\x1b]133;A")
        }),
        "stty-size test never saw prompt marker; output:\n{}",
        String::from_utf8_lossy(&accum)
    );

    writer.write_all(&[0x11]).unwrap(); // force queue
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let queued = format!("stty size > {}", shell_quote(&size_file));
    writer.write_all(queued.as_bytes()).unwrap();
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x11]).unwrap(); // return to shell
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    writer.write_all(b"sleep 2\r").unwrap();
    writer.flush().unwrap();

    let saw_size = wait_for(&rx, &mut accum, Duration::from_secs(6), |_| {
        file_contains(&size_file, "30 100")
    });

    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    let size = std::fs::read_to_string(&size_file).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        saw_size,
        "queued stty saw wrong terminal size ({size:?}); output:\n{}",
        String::from_utf8_lossy(&accum)
    );
}

#[test]
fn binary_queue_flow_works_inside_tmux_when_available() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }
    let bin = cmdq_binary_path();
    if !bin.exists() {
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "cmdq-tmux-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let home = tmp.join("home");
    let xdg = tmp.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let session = format!(
        "cmdq_smoke_{}_{}",
        std::process::id(),
        monotonic_test_suffix()
    );
    let side_effect = tmp.join("tmux-queued.txt");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let start_command = format!(
        "cd {} && HOME={} XDG_DATA_HOME={} TERM=xterm-256color {} --shell /bin/bash",
        shell_quote(repo),
        shell_quote(&home),
        shell_quote(&xdg),
        shell_quote(&bin)
    );

    let new_status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "100",
            "-y",
            "30",
            &start_command,
        ])
        .status()
        .unwrap();
    assert!(new_status.success(), "failed to create tmux session");

    let mut cleanup = TmuxCleanup {
        session: session.clone(),
        tmp: tmp.clone(),
        active: true,
    };

    assert!(
        wait_until(Duration::from_secs(5), || {
            capture_tmux(&session)
                .map(|s| s.contains('$'))
                .unwrap_or(false)
        }),
        "tmux session never showed shell prompt; pane:\n{}",
        capture_tmux(&session).unwrap_or_default()
    );

    tmux_send(&session, &["C-q"]);
    std::thread::sleep(Duration::from_millis(200));
    let queued = format!("printf 'TMUX_QUEUED_OK' > {}", shell_quote(&side_effect));
    tmux_send(&session, &[&queued, "C-m"]);
    std::thread::sleep(Duration::from_millis(200));
    tmux_send(&session, &["C-q"]);
    std::thread::sleep(Duration::from_millis(200));
    tmux_send(&session, &["echo TMUX_TRIGGER", "C-m"]);

    assert!(
        wait_until(Duration::from_secs(6), || file_contains(
            &side_effect,
            "TMUX_QUEUED_OK"
        )),
        "queued command did not run inside tmux; pane:\n{}",
        capture_tmux(&session).unwrap_or_default()
    );

    tmux_send(&session, &["exit", "C-m"]);
    cleanup.finish();
}

fn wait_for(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    accum: &mut Vec<u8>,
    timeout: Duration,
    mut predicate: impl FnMut(&[u8]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let chunk =
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now()));
        if let Ok(b) = rx.recv_timeout(chunk) {
            accum.extend_from_slice(&b);
        }
        if predicate(accum) {
            return true;
        }
    }
    predicate(accum)
}

fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn file_contains(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

fn queue_file_is_empty(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains("\"items\":[]"))
        .unwrap_or(false)
}

fn session_dirs_clean(xdg: &Path) -> bool {
    match std::fs::read_dir(xdg.join("cmdq").join("sessions")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn find_shell<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|path| std::path::Path::new(path).exists())
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    predicate()
}

fn tmux_send(session: &str, keys: &[&str]) {
    let status = Command::new("tmux")
        .arg("send-keys")
        .arg("-t")
        .arg(session)
        .args(keys)
        .status()
        .unwrap();
    assert!(status.success(), "tmux send-keys failed for {keys:?}");
}

fn capture_tmux(session: &str) -> Option<String> {
    let output = Command::new("tmux")
        .arg("capture-pane")
        .arg("-pt")
        .arg(session)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn monotonic_test_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

struct TmuxCleanup {
    session: String,
    tmp: std::path::PathBuf,
    active: bool,
}

impl TmuxCleanup {
    fn finish(&mut self) {
        if !self.active {
            return;
        }
        let _ = Command::new("tmux")
            .arg("kill-session")
            .arg("-t")
            .arg(&self.session)
            .status();
        let _ = std::fs::remove_dir_all(&self.tmp);
        self.active = false;
    }
}

impl Drop for TmuxCleanup {
    fn drop(&mut self) {
        self.finish();
    }
}
