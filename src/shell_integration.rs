//! Shell integration: emits OSC 133 prompt markers so cmdq can detect when
//! the inner shell is between commands vs. running one.
//!
//! Two concerns:
//!  1. Provide the integration snippet for a given shell (zsh / bash / fish).
//!  2. On `--install-integration`, append a `source` line to the user's rc file
//!     guarded by the `CMDQ_ACTIVE` env var so the snippet is only active
//!     under cmdq.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::atomic_file::{self, Access};

const ZSH_SNIPPET: &str = include_str!("../shell/integration.zsh");
const BASH_SNIPPET: &str = include_str!("../shell/integration.bash");
const FISH_SNIPPET: &str = include_str!("../shell/integration.fish");

const RC_BLOCK_BEGIN: &str = "# >>> cmdq shell integration >>>";
const RC_BLOCK_END: &str = "# <<< cmdq shell integration <<<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
    Sh,
}

impl ShellKind {
    pub fn detect_from_path(shell: &str) -> Self {
        let name = Path::new(shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(shell);
        match name {
            "zsh" => Self::Zsh,
            "bash" => Self::Bash,
            "fish" => Self::Fish,
            _ => Self::Sh,
        }
    }

    pub fn snippet(self) -> Result<&'static str> {
        match self {
            Self::Zsh => Ok(ZSH_SNIPPET),
            Self::Bash => Ok(BASH_SNIPPET),
            Self::Fish => Ok(FISH_SNIPPET),
            Self::Sh => Err(anyhow!(
                "POSIX sh has no reliable preexec hook; use zsh, bash, or fish for cmdq integration"
            )),
        }
    }
}

pub fn snippet_for(shell: &str) -> Result<&'static str> {
    let kind = ShellKind::detect_from_path(shell);
    if kind == ShellKind::Sh {
        return Err(unsupported_shell_error(shell));
    }
    kind.snippet()
}

/// Explain *why* a shell has no integration. Blaming "POSIX sh" for a typo
/// like `--shell zhs` or an unknown shell like `nu` sends people hunting for
/// the wrong problem.
fn unsupported_shell_error(shell: &str) -> anyhow::Error {
    let name = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    match name {
        "sh" | "dash" | "ash" => anyhow!(
            "POSIX {name} has no reliable preexec hook; use zsh, bash, or fish for cmdq integration"
        ),
        _ => anyhow!(
            "unsupported shell `{name}`; cmdq integration is available for zsh, bash, and fish"
        ),
    }
}

pub fn current_shell() -> Option<String> {
    std::env::var("SHELL").ok()
}

fn rc_file_for(shell: ShellKind) -> Option<PathBuf> {
    Some(match shell {
        ShellKind::Zsh => zsh_dot_dir()?.join(".zshrc"),
        ShellKind::Bash => dirs::home_dir()?.join(".bashrc"),
        ShellKind::Fish => crate::paths::config_dir()?.join("fish").join("config.fish"),
        ShellKind::Sh => return None,
    })
}

pub(crate) fn zsh_dot_dir() -> Option<PathBuf> {
    std::env::var_os("ZDOTDIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .and_then(crate::paths::absolute_path)
        .or_else(dirs::home_dir)
}

/// Path under the user's data dir where we drop the integration script
/// for `source`-ing from rc files.
fn integration_script_path(shell: ShellKind) -> Result<PathBuf> {
    let dir = data_dir()?.join("cmdq");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(match shell {
        ShellKind::Zsh => "integration.zsh",
        ShellKind::Bash => "integration.bash",
        ShellKind::Fish => "integration.fish",
        ShellKind::Sh => "integration.sh",
    }))
}

fn data_dir() -> Result<PathBuf> {
    crate::paths::data_dir().ok_or_else(|| anyhow!("could not locate home/data dir"))
}

/// Write the integration snippet to `~/.local/share/cmdq/integration.<shell>`.
pub fn write_integration_script(shell: ShellKind) -> Result<PathBuf> {
    let path = integration_script_path(shell)?;
    atomic_file::write(&path, shell.snippet()?.as_bytes(), Access::Inherit)?;
    Ok(path)
}

/// Install the integration into the user's rc file (idempotent).
pub fn install_for_current_shell() -> Result<String> {
    let shell_path =
        current_shell().ok_or_else(|| anyhow!("$SHELL is not set; pass --shell explicitly"))?;
    install_for_shell(&shell_path)
}

pub fn install_for_shell(shell_path: &str) -> Result<String> {
    let kind = ShellKind::detect_from_path(shell_path);
    if kind == ShellKind::Sh {
        return Err(unsupported_shell_error(shell_path));
    }
    let script = write_integration_script(kind)?;
    let rc = rc_file_for(kind).ok_or_else(|| anyhow!("could not determine rc file location"))?;

    let source_line = match kind {
        ShellKind::Fish => format!("source {}", shell_single_quote(&script)),
        _ => format!(
            "[ -f {} ] && . {}",
            shell_single_quote(&script),
            shell_single_quote(&script)
        ),
    };

    let block = format!(
        "{RC_BLOCK_BEGIN}\n# Activated only when running under cmdq.\n{source_line}\n{RC_BLOCK_END}\n"
    );

    let existing = read_rc_utf8_or_empty(&rc)?;
    let (new, updated_existing) = upsert_managed_block(&existing, &block)?;
    if updated_existing && new == existing {
        return Ok(format!(
            "cmdq integration already present in {}.\nIntegration script: {}",
            rc.display(),
            script.display()
        ));
    }

    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    write_rc_atomic(&rc, &new)?;

    Ok(format!(
        "{} cmdq integration in {}.\n\
         Integration script: {}\n\
         Restart your shell (or open a new cmdq session) to activate.",
        if updated_existing {
            "Updated"
        } else {
            "Installed"
        },
        rc.display(),
        script.display()
    ))
}

fn upsert_managed_block(existing: &str, block: &str) -> Result<(String, bool)> {
    let Some(start) = existing.find(RC_BLOCK_BEGIN) else {
        let mut new = existing.to_string();
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push('\n');
        new.push_str(block);
        return Ok((new, false));
    };
    let rest = &existing[start..];
    let Some(end_in_rest) = rest.find(RC_BLOCK_END) else {
        return Err(anyhow!(
            "found start of cmdq integration block but no end marker; please fix the rc file manually"
        ));
    };
    let end = start + end_in_rest + RC_BLOCK_END.len();
    let mut new = String::with_capacity(existing.len() + block.len());
    new.push_str(&existing[..start]);
    new.push_str(block);
    if !existing[end..].starts_with('\n') && !existing[end..].is_empty() {
        new.push('\n');
    }
    new.push_str(existing[end..].trim_start_matches('\n'));
    Ok((new, true))
}

fn read_rc_utf8_or_empty(path: &Path) -> Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).with_context(|| {
            format!(
                "{} is not valid UTF-8; refusing to rewrite it",
                path.display()
            )
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_rc_atomic(path: &Path, contents: &str) -> Result<()> {
    let write_path = writable_rc_path(path)?;
    atomic_file::write(&write_path, contents.as_bytes(), Access::Inherit)
}

fn writable_rc_path(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    // Follow the entire chain even when its final target does not exist yet.
    // canonicalize cannot resolve that case; renaming onto an intermediate
    // symlink would silently break a dotfiles manager's links.
    for _ in 0..40 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", current.display()));
            }
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let target = std::fs::read_link(&current)
            .with_context(|| format!("reading symlink {}", current.display()))?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }
    Err(anyhow!("too many symlinks resolving {}", path.display()))
}

pub(crate) fn shell_single_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    // Escape backslashes outside quotes too: fish interprets \\\\ inside
    // single quotes, unlike POSIX shells.
    let mut quoted = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => quoted.push_str("'\\\\'"),
            '\'' => quoted.push_str("'\\''"),
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// Send a block through the shell's parser as one command. Literal control
/// bytes would otherwise become readline/ZLE edits, and separate lines would
/// emit independent command-end markers with incorrect chaining statuses.
pub(crate) fn encode_queued_command(command: &str, shell: ShellKind) -> std::io::Result<String> {
    if command.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell commands cannot contain NUL bytes",
        ));
    }
    if !command.chars().any(|c| c.is_ascii_control()) {
        return Ok(command.to_string());
    }
    use std::fmt::Write;
    let fish = shell == ShellKind::Fish;
    let mut encoded = String::from(if fish {
        "builtin eval '"
    } else {
        "builtin eval $'"
    });
    for c in command.chars() {
        match c {
            '\\' => encoded.push_str("\\\\"),
            '\'' => encoded.push_str("\\'"),
            c if c.is_ascii_control() => {
                if fish {
                    write!(encoded, "'\\x{:02x}'", c as u32).unwrap();
                } else {
                    write!(encoded, "\\x{:02x}", c as u32).unwrap();
                }
            }
            _ => encoded.push(c),
        }
    }
    encoded.push('\'');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_shell_error_names_the_shell_instead_of_blaming_sh() {
        let err = snippet_for("/opt/homebrew/bin/nu").unwrap_err().to_string();
        assert!(err.contains("unsupported shell `nu`"), "{err}");
        assert!(!err.contains("POSIX"), "{err}");

        let err = snippet_for("zhs").unwrap_err().to_string();
        assert!(err.contains("`zhs`"), "{err}");

        for posix in ["sh", "/bin/dash", "/bin/ash"] {
            let err = snippet_for(posix).unwrap_err().to_string();
            assert!(err.contains("POSIX"), "{posix}: {err}");
        }
    }

    #[test]
    fn detect_shell_kind() {
        assert_eq!(ShellKind::detect_from_path("/bin/zsh"), ShellKind::Zsh);
        assert_eq!(
            ShellKind::detect_from_path("/usr/local/bin/bash"),
            ShellKind::Bash
        );
        assert_eq!(
            ShellKind::detect_from_path("/opt/homebrew/bin/fish"),
            ShellKind::Fish
        );
        assert_eq!(ShellKind::detect_from_path("/bin/sh"), ShellKind::Sh);
    }

    #[test]
    fn snippets_are_non_empty_and_have_osc_133() {
        for s in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let snip = s.snippet().unwrap();
            assert!(!snip.is_empty(), "{:?}", s);
            assert!(snip.contains("133;C"), "{:?} missing C marker", s);
            assert!(snip.contains("133;D"), "{:?} missing D marker", s);
            assert!(snip.contains("133;A"), "{:?} missing A marker", s);
            assert!(
                snip.contains("file://localhost"),
                "{:?} missing OSC 7 cwd marker",
                s
            );
            assert!(snip.contains("%25"), "{:?} missing cwd percent escape", s);
            assert!(snip.contains("%1B"), "{:?} missing cwd ESC escape", s);
        }
    }

    #[test]
    fn sh_integration_fails_loudly() {
        assert!(ShellKind::Sh.snippet().is_err());
        assert!(rc_file_for(ShellKind::Sh).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rc_write_preserves_a_chain_with_a_missing_final_target() {
        let temp = tempfile::tempdir().unwrap();
        let rc = temp.path().join("rc");
        let middle = temp.path().join("middle");
        let target = temp.path().join("dotfiles/rc");
        std::os::unix::fs::symlink("middle", &rc).unwrap();
        std::os::unix::fs::symlink("dotfiles/rc", &middle).unwrap();
        write_rc_atomic(&rc, "# config\n").unwrap();
        assert!(rc.is_symlink());
        assert!(middle.is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "# config\n");
    }
}
