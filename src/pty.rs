//! Thin wrapper around `portable-pty`: spawn the user's shell in a pseudo
//! terminal and expose the master side for read/write/resize.
//!
//! We set `CMDQ_ACTIVE=1` so the shell integration snippet (sourced from rc)
//! knows to install OSC 133 hooks. We also try to source the snippet directly
//! for zsh/bash via `ZDOTDIR`/`BASH_ENV` so users without integration in their
//! rc still get markers automatically. (Best-effort; falls back gracefully.)

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::shell_integration::{ShellKind, write_integration_script};

pub struct ShellPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
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

        let mut cmd = CommandBuilder::new(&shell_path);

        // Mark this session.
        cmd.env("CMDQ_ACTIVE", "1");

        // Force interactive shell so rc files are sourced (zsh/bash).
        match kind {
            ShellKind::Zsh => {
                cmd.arg("-i");
                if let Some(script) = &integration_script
                    && let Some(zdotdir) = prepare_zdotdir(script) {
                        cmd.env("ZDOTDIR", zdotdir);
                    }
            }
            ShellKind::Bash => {
                cmd.arg("-i");
                // BASH_ENV is sourced for non-interactive shells; for
                // interactive ones we rely on the user's .bashrc to source the
                // file (or we drop a sourcing line later via auto-install).
                if let Some(script) = &integration_script {
                    cmd.env("BASH_ENV", script);
                }
            }
            ShellKind::Fish | ShellKind::Sh => {
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
}

/// Build a temporary ZDOTDIR that sources the user's real ~/.zshrc and then
/// our integration script. This means cmdq users get OSC 133 even without
/// running `--install-integration`.
fn prepare_zdotdir(script: &std::path::Path) -> Option<PathBuf> {
    let data = dirs::data_dir().or_else(dirs::home_dir)?.join("cmdq");
    std::fs::create_dir_all(&data).ok()?;
    let zdotdir = data.join("zdotdir");
    std::fs::create_dir_all(&zdotdir).ok()?;

    let real_home = dirs::home_dir()?;
    let real_zshrc = real_home.join(".zshrc");
    let real_zshenv = real_home.join(".zshenv");
    let real_zprofile = real_home.join(".zprofile");
    let real_zlogin = real_home.join(".zlogin");

    let mk = |dest: &std::path::Path, real: &std::path::Path, also_source_integration: bool| {
        let mut content = format!(
            "[ -f \"{}\" ] && source \"{}\"\n",
            real.display(),
            real.display()
        );
        if also_source_integration {
            content.push_str(&format!(
                "[ -f \"{}\" ] && source \"{}\"\n",
                script.display(),
                script.display()
            ));
        }
        std::fs::write(dest, content).ok();
    };

    mk(&zdotdir.join(".zshenv"), &real_zshenv, false);
    mk(&zdotdir.join(".zprofile"), &real_zprofile, false);
    // Source the integration *after* user's own zshrc so our hooks add to theirs.
    mk(&zdotdir.join(".zshrc"), &real_zshrc, true);
    mk(&zdotdir.join(".zlogin"), &real_zlogin, false);

    Some(zdotdir)
}
