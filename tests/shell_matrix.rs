//! Real shell and terminal-application tests with isolated homes.
#![cfg(unix)]

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

struct Session {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    query_tail: Vec<u8>,
    dir: tempfile::TempDir,
}

impl Session {
    fn new(shell: &str) -> Option<Self> {
        Self::new_with_zshrc(shell, "PROMPT='MATRIX> '\n")
    }
    fn new_with_zshrc(shell: &str, zshrc: &str) -> Option<Self> {
        let Some(shell_path) = executable(shell) else {
            assert!(
                std::env::var_os("CMDQ_REQUIRE_SHELL_MATRIX").is_none(),
                "required shell missing: {shell}"
            );
            eprintln!("skipping unavailable shell: {shell}");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config = dir.path().join("config");
        std::fs::create_dir_all(config.join("fish")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".bashrc"), "PS1='MATRIX> '\n").unwrap();
        std::fs::write(home.join(".zshrc"), zshrc).unwrap();
        std::fs::write(
            config.join("fish/config.fish"),
            "set -g fish_greeting\nfunction fish_prompt; printf 'MATRIX> '; end\n",
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
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_cmdq"));
        cmd.args(["--shell", shell_path.to_str().unwrap()]);
        cmd.cwd(dir.path());
        for key in [
            "CMDQ_ACTIVE",
            "CMDQ_INTEGRATION_LOADED",
            "BASH_ENV",
            "ENV",
            "PROMPT_COMMAND",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", &home);
        cmd.env("ZDOTDIR", &home);
        cmd.env("XDG_CONFIG_HOME", &config);
        cmd.env("XDG_DATA_HOME", dir.path().join("data"));
        cmd.env("TERM", "xterm-256color");
        cmd.env("PS1", "MATRIX> ");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 || tx.send(bytes[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut session = Self {
            child,
            _master: pair.master,
            writer,
            rx,
            output: Vec::new(),
            query_tail: Vec::new(),
            dir,
        };
        session.expect(b"MATRIX> ");
        session.drain(Duration::from_millis(150));
        Some(session)
    }
    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
    fn absorb(&mut self, chunk: Vec<u8>) {
        // Model real terminal replies, including split queries. Modern Fish
        // asks again after drawing its prompt; older Fish may not query at all.
        self.output.extend_from_slice(&chunk);
        for byte in chunk {
            self.query_tail.push(byte);
            if self.query_tail.ends_with(b"\x1b[0c") {
                self.send(b"\x1b[?1;2c");
            } else if self.query_tail.ends_with(b"\x1b[6n") {
                self.send(b"\x1b[1;9R");
            } else if self.query_tail.ends_with(b"\x1b[?u") {
                self.send(b"\x1b[?0u");
            }
            if self.query_tail.len() > 8 {
                self.query_tail.remove(0);
            }
        }
    }
    fn command(&mut self, command: &str) {
        self.output.clear();
        self.send(format!("{command}\r").as_bytes());
    }
    fn expect(&mut self, bytes: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if self.output.windows(bytes.len()).any(|w| w == bytes) {
                return;
            }
            if let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(20)) {
                self.absorb(chunk);
            }
        }
        panic!(
            "missing {:?}; output: {}",
            String::from_utf8_lossy(bytes),
            String::from_utf8_lossy(&self.output)
        );
    }
    fn drain(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(10)) {
                self.absorb(bytes);
            }
        }
    }
    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn expect_file(&mut self, name: &str, contents: &str) {
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            if std::fs::read_to_string(self.file(name)).ok().as_deref() == Some(contents) {
                return;
            }
            self.drain(Duration::from_millis(20));
        }
        panic!(
            "file {name} did not contain {contents:?}; output: {}",
            String::from_utf8_lossy(&self.output)
        );
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn queue_lifecycle(shell: &str) {
    let Some(mut s) = Session::new(shell) else {
        return;
    };
    s.command("sh -c 'sleep 2; exit 7'");
    s.expect(b"\x1b]133;C");
    s.send(b"printf skipped > skipped\t\r");
    s.send(b"printf dispatched > dispatched\r");
    s.expect_file("dispatched", "dispatched");
    assert!(
        !s.file("skipped").exists(),
        "conditional command ran after failure"
    );
    s.expect(b"\x1b]133;D;7");
    s.expect(b"\x1b]133;D;0");
    s.drain(Duration::from_millis(100));
    s.command("sh -c 'sleep 2; exit 7'");
    s.expect(b"\x1b]133;C");
    s.send(b"sh -c 'sleep 1; printf first > first'\r");
    s.send(b"sh -c 'test -f first && printf second > second'\t\r");
    s.expect_file("second", "second");
}

fn direct_input(shell: &str) {
    let Some(mut s) = Session::new(shell) else {
        return;
    };
    // No recognizable prompt and no alternate screen. Echo-off input must
    // never become a persisted queue entry (e.g. passwords).
    s.command(
        "sh -c 'stty -echo; printf READY; read answer; stty echo; printf %s \"$answer\" > answer'",
    );
    s.expect(b"READY");
    s.drain(Duration::from_millis(1750));
    s.send(b"private-answer\r");
    s.expect_file("answer", "private-answer");
    s.expect(b"\x1b]133;D;0");
    let queue = std::fs::read_to_string(s.file("data/cmdq/queue.json")).unwrap_or_default();
    assert!(!queue.contains("private-answer"));
    // A raw reader with no output also needs keys, including Ctrl-Q and Esc.
    s.command("sh -c 'stty raw -echo; head -c 2 > raw; stty sane'");
    s.expect(b"\x1b]133;C");
    s.drain(Duration::from_millis(1750));
    s.send(b"\x11\x1b");
    s.expect_file("raw", "\x11\x1b");
    s.expect(b"\x1b]133;D;0");
    s.command("sh -c 'stty raw -echo; head -c 10 > keys; stty sane'");
    s.expect(b"\x1b]133;C");
    s.drain(Duration::from_millis(250));
    s.send(b"\x1bOA\x1b[15;2~");
    s.expect_file("keys", "\x1bOA\x1b[15;2~");
}

fn real_programs(shell: &str) {
    let Some(mut s) = Session::new(shell) else {
        return;
    };
    for program in ["vim", "less", "git"] {
        assert!(
            executable(program).is_some(),
            "missing test program: {program}"
        );
    }
    s.command("vim -Nu NONE -n -i NONE edited");
    s.expect(b"\x1b[?1049h");
    s.drain(Duration::from_millis(1750));
    s.send(b"ihello from vim\x1b");
    s.drain(Duration::from_millis(100));
    s.send(b":wq\r");
    s.expect_file("edited", "hello from vim\n");
    s.expect(b"\x1b]133;D;0");
    std::fs::write(
        s.file("long-file"),
        (0..150).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    s.command("less -X long-file");
    s.expect(b"line 0");
    s.drain(Duration::from_millis(1750));
    s.send(b" q");
    s.expect(b"\x1b]133;D;0");
    std::fs::write(s.file("before"), "old\n".repeat(150)).unwrap();
    std::fs::write(s.file("after"), "new\n".repeat(150)).unwrap();
    s.command("env GIT_PAGER='less -+F' git --paginate diff --no-index before after");
    s.expect(b"diff --git");
    s.drain(Duration::from_millis(1750));
    s.send(b"q");
    s.expect(b"\x1b]133;D;1");
    s.command("printf recovered > recovered");
    s.expect_file("recovered", "recovered");
}

macro_rules! shell_tests {
    ($queue:ident, $input:ident, $apps:ident, $shell:literal) => {
        #[test]
        fn $queue() {
            queue_lifecycle($shell);
        }
        #[test]
        fn $input() {
            direct_input($shell);
        }
        #[test]
        fn $apps() {
            real_programs($shell);
        }
    };
}
shell_tests!(bash_queue, bash_direct_input, bash_real_programs, "bash");
shell_tests!(zsh_queue, zsh_direct_input, zsh_real_programs, "zsh");
shell_tests!(fish_queue, fish_direct_input, fish_real_programs, "fish");

#[test]
fn zsh_rc_top_level_typeset_survives_the_startup_shim() {
    // A top-level `typeset` in ~/.zshrc must stay global. If the shim sourced
    // the file from inside a function, `path`/`fpath` would become locals:
    // PATH edits would vanish and no autoloadable function (add-zsh-hook
    // included) could be found, so the integration would never hook in.
    let rc = "typeset -U path PATH fpath FPATH\n\
              path+=(/cmdq-matrix-bin)\n\
              autoload -Uz add-zsh-hook\n\
              _matrix_precmd() { :; }\n\
              add-zsh-hook precmd _matrix_precmd\n\
              PROMPT='MATRIX> '\n";
    let Some(mut s) = Session::new_with_zshrc("zsh", rc) else {
        return;
    };
    s.command("print -r -- MARK-${path[-1]}-$(( $+functions[add-zsh-hook] ))");
    s.expect(b"MARK-/cmdq-matrix-bin-1");
    s.expect(b"\x1b]133;D;0");
    assert!(
        !s.output
            .windows(b"function definition file not found".len())
            .any(|w| w == b"function definition file not found"),
        "autoload broke during startup: {}",
        String::from_utf8_lossy(&s.output)
    );
}

#[test]
fn install_fish_respects_config_home_and_symlink_chains() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("custom-config");
    std::fs::create_dir_all(config.join("fish")).unwrap();
    let real = dir.path().join("dotfiles.fish");
    std::fs::write(&real, "# user configuration\n").unwrap();
    let link = dir.path().join("link.fish");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let rc = config.join("fish/config.fish");
    std::os::unix::fs::symlink(&link, &rc).unwrap();
    for _ in 0..2 {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_cmdq"))
            .args(["--shell", "fish", "--install-integration"])
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(link.is_symlink());
    assert!(rc.is_symlink());
    let contents = std::fs::read_to_string(real).unwrap();
    assert!(contents.starts_with("# user configuration\n"));
    assert_eq!(contents.matches("# >>> cmdq").count(), 1);
}

#[test]
fn non_terminal_start_has_actionable_error_without_creating_state() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cmdq"))
        .env("XDG_DATA_HOME", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!dir.path().join("cmdq").exists());
}

#[test]
fn shell_exit_status_reaches_parent() {
    let Some(mut s) = Session::new("bash") else {
        return;
    };
    s.command("exit 7");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = s.child.try_wait().unwrap() {
            assert_eq!(status.exit_code(), 7);
            break;
        }
        assert!(Instant::now() < deadline, "cmdq did not exit");
        s.drain(Duration::from_millis(20));
    }
}

#[test]
fn continuous_output_can_be_interrupted() {
    let Some(mut s) = Session::new("bash") else {
        return;
    };
    s.command("yes");
    s.expect(b"\x1b]133;C");
    s.drain(Duration::from_millis(250));
    s.send(b"\x03");
    s.expect(b"\x1b]133;D;130");
    s.command("printf usable > usable");
    s.expect_file("usable", "usable");
}

#[test]
fn child_negotiated_keyboard_protocol_preserves_press_and_release() {
    let Some(mut s) = Session::new("bash") else {
        return;
    };
    s.command("sh -c 'stty raw -echo; printf \"\\033[>8u\"; head -c 14 > protocol; printf \"\\033[<u\"; stty sane'");
    s.expect(b"\x1b[>8u");
    s.drain(Duration::from_millis(150));
    s.send(b"\x1b[97u\x1b[97;1:3u");
    s.expect_file("protocol", "\x1b[97u\x1b[97;1:3u");
}

#[test]
fn posix_shell_is_explicit_transparent_passthrough() {
    let Some(mut s) = Session::new("sh") else {
        return;
    };
    assert!(String::from_utf8_lossy(&s.output).contains("no queue hooks"));
    s.command("sh -c 'stty raw -echo; head -c 1 > input; stty sane'");
    s.drain(Duration::from_millis(1750));
    s.send(b"\x11");
    s.expect_file("input", "\x11");
    assert!(!s.file("data/cmdq/queue.json").exists());
}
