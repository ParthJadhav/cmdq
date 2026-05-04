//! Thin wrapper around `portable-pty`: spawn the user's shell in a pseudo
//! terminal and expose the master side for read/write/resize.
//!
//! We set `CMDQ_ACTIVE=1` so the shell integration snippet (sourced from rc)
//! knows to install OSC 133 hooks. We also try to source the snippet directly
//! for zsh/bash/fish via a temporary rc/init hook so users without integration
//! in their rc still get markers automatically. (Best-effort; falls back
//! gracefully.)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::shell_integration::{
    ShellKind, data_dir, shell_single_quote, write_file_atomic, write_integration_script,
    zsh_dot_dir,
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
const STALE_SESSION_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub struct ShellPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    session_dirs: Vec<PathBuf>,
}

pub struct PtyIo {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

impl ShellPty {
    /// Spawn the configured shell. Returns the pty + io handles.
    pub fn spawn(shell: Option<&str>, cols: u16, rows: u16) -> Result<(Self, PtyIo)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let shell_path = shell
            .map(|s| s.to_string())
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let kind = ShellKind::detect_from_path(&shell_path);

        // Try to write the integration script so it can be sourced.
        let integration_script = write_integration_script(kind).ok();
        let mut session_dirs = Vec::new();

        let mut cmd = CommandBuilder::new(&shell_path);

        // Mark this session.
        cmd.env("CMDQ_ACTIVE", "1");

        // Force interactive shell so rc files / init hooks are sourced.
        match kind {
            ShellKind::Zsh => {
                cmd.arg("-i");
                if let Some(script) = &integration_script
                    && let Some(zdotdir) = prepare_zdotdir(script)
                {
                    cmd.env("ZDOTDIR", &zdotdir);
                    session_dirs.push(zdotdir);
                }
            }
            ShellKind::Bash => {
                cmd.arg("--noprofile");
                if let Some(script) = &integration_script
                    && let Some((rcfile, session_dir)) = prepare_bash_rcfile(script)
                {
                    cmd.arg("--rcfile");
                    cmd.arg(rcfile);
                    session_dirs.push(session_dir);
                }
                cmd.arg("-i");
            }
            ShellKind::Fish => {
                if let Some(script) = &integration_script {
                    cmd.arg("--init-command");
                    cmd.arg(format!("source {}", shell_single_quote(script)));
                }
                cmd.arg("-i");
            }
            ShellKind::Sh => {
                cmd.arg("-i");
            }
        }

        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;

        // Drop the slave; the child holds it open via its fd.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;

        Ok((
            Self {
                master: pair.master,
                child,
                session_dirs,
            },
            PtyIo { reader, writer },
        ))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")?;
        Ok(())
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        Ok(())
    }

    pub fn session_dirs(&self) -> &[PathBuf] {
        &self.session_dirs
    }
}

impl Drop for ShellPty {
    fn drop(&mut self) {
        for dir in self.session_dirs.drain(..) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Build a temporary bash rcfile that sources the user's real ~/.bashrc and
/// then cmdq's integration script. Interactive bash ignores BASH_ENV, so
/// --rcfile is the only reliable way to make clean bash sessions emit markers.
fn prepare_bash_rcfile(script: &std::path::Path) -> Option<(PathBuf, PathBuf)> {
    let session_dir = prepare_session_dir("bash")?;
    let rcfile = session_dir.join("bashrc");

    let real_home = dirs::home_dir()?;
    let real_bashrc_q = shell_single_quote(&real_home.join(".bashrc"));
    let real_bash_profile_q = shell_single_quote(&real_home.join(".bash_profile"));
    let real_bash_login_q = shell_single_quote(&real_home.join(".bash_login"));
    let real_profile_q = shell_single_quote(&real_home.join(".profile"));
    let script_q = shell_single_quote(script);
    let content = format!(
        "if [ -f {real_bashrc_q} ]; then\n  . {real_bashrc_q}\nelif [ -f {real_bash_profile_q} ]; then\n  . {real_bash_profile_q}\nelif [ -f {real_bash_login_q} ]; then\n  . {real_bash_login_q}\nelif [ -f {real_profile_q} ]; then\n  . {real_profile_q}\nfi\n[ -f {script_q} ] && . {script_q}\n"
    );
    write_file_atomic(&rcfile, content.as_bytes()).ok()?;
    Some((rcfile, session_dir))
}

/// Build a temporary ZDOTDIR that sources the user's real zsh startup files and then
/// our integration script. This means cmdq users get OSC 133 even without
/// running `--install-integration`.
fn prepare_zdotdir(script: &std::path::Path) -> Option<PathBuf> {
    let zdotdir = prepare_session_dir("zsh")?;

    let real_zdotdir = zsh_dot_dir()?;
    let real_zshenv = real_zdotdir.join(".zshenv");

    let zdotdir_q = shell_single_quote(&zdotdir);
    let real_zdotdir_q = shell_single_quote(&real_zdotdir);
    let real_zshenv_q = shell_single_quote(&real_zshenv);
    let zshenv = format!(
        "_CMDQ_SYNTHETIC_ZDOTDIR={zdotdir_q}\n\
         _CMDQ_USER_ZDOTDIR={real_zdotdir_q}\n\
         [ -f {real_zshenv_q} ] && source {real_zshenv_q}\n\
         if [[ -n \"${{ZDOTDIR:-}}\" && \"$ZDOTDIR\" != \"$_CMDQ_SYNTHETIC_ZDOTDIR\" ]]; then\n\
         \x20 _CMDQ_USER_ZDOTDIR=\"$ZDOTDIR\"\n\
         fi\n\
         export _CMDQ_SYNTHETIC_ZDOTDIR _CMDQ_USER_ZDOTDIR\n\
         export ZDOTDIR=\"$_CMDQ_SYNTHETIC_ZDOTDIR\"\n\
         _cmdq_source_user_zdot_file() {{\n\
         \x20 local _cmdq_file=\"$1\"\n\
         \x20 local _cmdq_saved_zdotdir=\"${{ZDOTDIR:-}}\"\n\
         \x20 export ZDOTDIR=\"$_CMDQ_USER_ZDOTDIR\"\n\
         \x20 if [[ -n \"$_CMDQ_USER_ZDOTDIR\" && -f \"$_CMDQ_USER_ZDOTDIR/$_cmdq_file\" ]]; then\n\
         \x20\x20 source \"$_CMDQ_USER_ZDOTDIR/$_cmdq_file\"\n\
         \x20 fi\n\
         \x20 export ZDOTDIR=\"$_cmdq_saved_zdotdir\"\n\
         }}\n"
    );
    write_file_atomic(&zdotdir.join(".zshenv"), zshenv.as_bytes()).ok()?;

    let mk = |dest: &Path, file: &str, also_source_integration: bool| -> Option<()> {
        let mut content = format!(
            "if (( $+functions[_cmdq_source_user_zdot_file] )); then\n  _cmdq_source_user_zdot_file {file}\nfi\n"
        );
        if also_source_integration {
            let script_q = shell_single_quote(script);
            content.push_str(&format!("[ -f {script_q} ] && source {script_q}\n"));
        }
        write_file_atomic(dest, content.as_bytes()).ok()
    };

    mk(&zdotdir.join(".zprofile"), ".zprofile", false)?;
    // Source the integration *after* user's own zshrc so our hooks add to theirs.
    mk(&zdotdir.join(".zshrc"), ".zshrc", true)?;
    mk(&zdotdir.join(".zlogin"), ".zlogin", false)?;

    Some(zdotdir)
}

fn prepare_session_dir(prefix: &str) -> Option<PathBuf> {
    let base = data_dir().ok()?.join("cmdq").join("sessions");
    std::fs::create_dir_all(&base).ok()?;
    cleanup_stale_session_dirs(&base);
    for attempt in 0..100 {
        let dir = base.join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            monotonic_session_suffix(),
            attempt
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Some(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn cleanup_stale_session_dirs(base: &Path) {
    cleanup_stale_session_dirs_at(base, unix_nanos());
}

fn cleanup_stale_session_dirs_at(base: &Path, now_nanos: u128) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let stale_age = STALE_SESSION_DIR_AGE.as_nanos();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(created_nanos) = session_dir_created_nanos(name) else {
            continue;
        };
        if now_nanos.saturating_sub(created_nanos) > stale_age {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn session_dir_created_nanos(name: &str) -> Option<u128> {
    if !(name.starts_with("bash-") || name.starts_with("zsh-")) {
        return None;
    }
    let mut parts = name.rsplitn(3, '-');
    let _attempt = parts.next()?;
    let suffix = parts.next()?.parse::<u128>().ok()?;
    let _prefix_and_pid = parts.next()?;
    Some(suffix / 1000)
}

fn monotonic_session_suffix() -> u128 {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    unix_nanos().saturating_mul(1000) + u128::from(counter)
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_stale_session_dirs_removes_only_old_synthetic_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        let now = STALE_SESSION_DIR_AGE.as_nanos() + 1_000;
        let stale_suffix = (now - STALE_SESSION_DIR_AGE.as_nanos() - 1).saturating_mul(1000);
        let fresh_suffix = (now - 10).saturating_mul(1000);
        let stale = base.join(format!("bash-123-{stale_suffix}-0"));
        let fresh = base.join(format!("zsh-123-{fresh_suffix}-0"));
        let unrelated = base.join("notes-123-0-0");
        std::fs::create_dir(&stale).unwrap();
        std::fs::create_dir(&fresh).unwrap();
        std::fs::create_dir(&unrelated).unwrap();

        cleanup_stale_session_dirs_at(base, now);

        assert!(!stale.exists(), "stale synthetic dir should be pruned");
        assert!(fresh.exists(), "fresh synthetic dir should stay");
        assert!(unrelated.exists(), "unrelated dirs must not be pruned");
    }
}
