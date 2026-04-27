//! Shell integration: emits OSC 133 prompt markers so cmdq can detect when
//! the inner shell is between commands vs. running one.
//!
//! Two concerns:
//!  1. Provide the integration snippet for a given shell (zsh / bash / fish).
//!  2. On `--install-integration`, append a `source` line to the user's rc file
//!     guarded by the `CMDQ_ACTIVE` env var so the snippet is only active
//!     under cmdq.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

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

    pub fn snippet(self) -> &'static str {
        match self {
            Self::Zsh => ZSH_SNIPPET,
            Self::Bash | Self::Sh => BASH_SNIPPET,
            Self::Fish => FISH_SNIPPET,
        }
    }
}

pub fn snippet_for(shell: &str) -> Result<&'static str> {
    Ok(ShellKind::detect_from_path(shell).snippet())
}

/// Determine the user's current shell from $SHELL.
pub fn current_shell() -> Option<String> {
    std::env::var("SHELL").ok()
}

/// Path to the rc file we should install into for the given shell.
pub fn rc_file_for(shell: ShellKind) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(match shell {
        ShellKind::Zsh => home.join(".zshrc"),
        ShellKind::Bash | ShellKind::Sh => home.join(".bashrc"),
        ShellKind::Fish => home.join(".config").join("fish").join("config.fish"),
    })
}

/// Path under the user's data dir where we drop the integration script
/// for `source`-ing from rc files.
pub fn integration_script_path(shell: ShellKind) -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("could not locate home/data dir"))?;
    let dir = dir.join("cmdq");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(match shell {
        ShellKind::Zsh => "integration.zsh",
        ShellKind::Bash | ShellKind::Sh => "integration.bash",
        ShellKind::Fish => "integration.fish",
    }))
}

/// Write the integration snippet to `~/.local/share/cmdq/integration.<shell>`.
pub fn write_integration_script(shell: ShellKind) -> Result<PathBuf> {
    let path = integration_script_path(shell)?;
    std::fs::write(&path, shell.snippet())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Install the integration into the user's rc file (idempotent).
pub fn install_for_current_shell() -> Result<String> {
    let shell_path =
        current_shell().ok_or_else(|| anyhow!("$SHELL is not set; pass --shell explicitly"))?;
    let kind = ShellKind::detect_from_path(&shell_path);
    let script = write_integration_script(kind)?;
    let rc = rc_file_for(kind).ok_or_else(|| anyhow!("could not determine rc file location"))?;

    let source_line = match kind {
        ShellKind::Fish => format!("source \"{}\"", script.display()),
        _ => format!(
            "[ -f \"{}\" ] && . \"{}\"",
            script.display(),
            script.display()
        ),
    };

    let block = format!(
        "{RC_BLOCK_BEGIN}\n# Activated only when running under cmdq.\n{source_line}\n{RC_BLOCK_END}\n"
    );

    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(RC_BLOCK_BEGIN) {
        return Ok(format!(
            "cmdq integration already present in {}.\nIntegration script: {}",
            rc.display(),
            script.display()
        ));
    }

    let mut new = existing;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push('\n');
    new.push_str(&block);
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&rc, new).with_context(|| format!("writing {}", rc.display()))?;

    Ok(format!(
        "Installed cmdq integration into {}.\n\
         Integration script: {}\n\
         Restart your shell (or open a new cmdq session) to activate.",
        rc.display(),
        script.display()
    ))
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
            let snip = s.snippet();
            assert!(!snip.is_empty(), "{:?}", s);
            assert!(snip.contains("133;C"), "{:?} missing C marker", s);
            assert!(snip.contains("133;D"), "{:?} missing D marker", s);
            assert!(snip.contains("133;A"), "{:?} missing A marker", s);
        }
    }
}
