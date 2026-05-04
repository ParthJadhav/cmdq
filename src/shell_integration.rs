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
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};

const ZSH_SNIPPET: &str = include_str!("../shell/integration.zsh");
const BASH_SNIPPET: &str = include_str!("../shell/integration.bash");
const FISH_SNIPPET: &str = include_str!("../shell/integration.fish");

const RC_BLOCK_BEGIN: &str = "# >>> cmdq shell integration >>>";
const RC_BLOCK_END: &str = "# <<< cmdq shell integration <<<";
static FILE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    ShellKind::detect_from_path(shell).snippet()
}

/// Determine the user's current shell from $SHELL.
pub fn current_shell() -> Option<String> {
    std::env::var("SHELL").ok()
}

/// Path to the rc file we should install into for the given shell.
pub fn rc_file_for(shell: ShellKind) -> Option<PathBuf> {
    Some(match shell {
        ShellKind::Zsh => zsh_dot_dir()?.join(".zshrc"),
        ShellKind::Bash => dirs::home_dir()?.join(".bashrc"),
        ShellKind::Fish => dirs::home_dir()?
            .join(".config")
            .join("fish")
            .join("config.fish"),
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
pub fn integration_script_path(shell: ShellKind) -> Result<PathBuf> {
    let dir = data_dir()?.join("cmdq");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(match shell {
        ShellKind::Zsh => "integration.zsh",
        ShellKind::Bash => "integration.bash",
        ShellKind::Fish => "integration.fish",
        ShellKind::Sh => "integration.sh",
    }))
}

pub(crate) fn data_dir() -> Result<PathBuf> {
    crate::paths::data_dir().ok_or_else(|| anyhow!("could not locate home/data dir"))
}

/// Write the integration snippet to `~/.local/share/cmdq/integration.<shell>`.
pub fn write_integration_script(shell: ShellKind) -> Result<PathBuf> {
    let path = integration_script_path(shell)?;
    write_file_atomic(&path, shell.snippet()?.as_bytes())?;
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
    write_file_atomic(&write_path, contents.as_bytes())
}

pub(crate) fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp_name = format!(
        ".{file_name}.cmdq-tmp-{}-{}",
        std::process::id(),
        monotonic_suffix()
    );
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing temp file {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("renaming temp file into {}", path.display()))
        }
    }
}

fn writable_rc_path(path: &Path) -> Result<PathBuf> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(path.to_path_buf());
    };
    if !meta.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    let target =
        std::fs::read_link(path).with_context(|| format!("reading symlink {}", path.display()))?;
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(path.parent().unwrap_or_else(|| Path::new(".")).join(target))
    }
}

fn monotonic_suffix() -> u128 {
    let counter = FILE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nanos.saturating_mul(1000) + u128::from(counter)
}

pub(crate) fn shell_single_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn integration_loaded_guard_is_not_exported_to_nested_shells() {
        assert!(!BASH_SNIPPET.contains("export CMDQ_INTEGRATION_LOADED"));
        assert!(!ZSH_SNIPPET.contains("export CMDQ_INTEGRATION_LOADED"));
        assert!(!FISH_SNIPPET.contains("set -gx CMDQ_INTEGRATION_LOADED"));
    }

    #[test]
    fn snippets_emit_cwd_before_command_end() {
        assert!(BASH_SNIPPET.contains("_cmdq_emit_cwd\n        if [[ -n \"$_CMDQ_IN_CMD\" ]]"));
        assert!(ZSH_SNIPPET.contains("_cmdq_emit_cwd\n        if [[ -n \"$_CMDQ_IN_CMD\" ]]"));
        assert!(FISH_SNIPPET.contains(
            "set -l exit $status\n        _cmdq_emit_cwd\n        printf '\\e]133;D;%s\\a' $exit"
        ));
    }

    #[test]
    fn sh_integration_fails_loudly() {
        assert!(ShellKind::Sh.snippet().is_err());
        assert!(rc_file_for(ShellKind::Sh).is_none());
    }

    #[test]
    fn write_file_atomic_creates_parent_and_leaves_no_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("integration.bash");

        write_file_atomic(&path, b"source me\n").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"source me\n");
        let parent = path.parent().unwrap();
        let temp_files = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".cmdq-tmp-"))
            .count();
        assert_eq!(temp_files, 0);
    }

    #[test]
    fn concurrent_atomic_writes_leave_one_complete_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("integration.bash");
        let first = b"first\n".repeat(16_384);
        let second = b"second\n".repeat(16_384);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = path.clone();
                let first = first.clone();
                let second = second.clone();
                scope.spawn(move || {
                    write_file_atomic(&path, &first).unwrap();
                    write_file_atomic(&path, &second).unwrap();
                });
            }
        });

        let final_contents = std::fs::read(&path).unwrap();
        assert!(
            final_contents == first || final_contents == second,
            "final file contained a partial or mixed payload"
        );
    }
}
