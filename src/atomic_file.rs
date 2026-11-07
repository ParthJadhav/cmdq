//! Replacing a file atomically: write a uniquely named temporary file beside
//! it, then rename it over the target, so a reader (another cmdq session, a
//! shell sourcing its rc file) never sees a partial file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Who may read the written file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Owner only (queued commands can contain credentials).
    Private,
    /// Keep the permissions of the file being replaced, if any.
    Inherit,
}

pub(crate) fn write(path: &Path, contents: &[u8], access: Access) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(
        ".{file_name}.cmdq-tmp-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if access == Access::Private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("creating temp file {}", tmp.display()))?;
    if let Err(error) = file.write_all(contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("writing temp file {}", tmp.display()));
    }
    drop(file);
    if access == Access::Inherit
        && let Ok(meta) = std::fs::metadata(path)
    {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).or_else(|error| {
        let _ = std::fs::remove_file(&tmp);
        Err(error).with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
    })
}

/// Create a new directory `<base>/<prefix><pid>-<unique suffix>-<attempt>`
/// that no other process or earlier call owns.
pub(crate) fn create_unique_dir(base: &Path, prefix: &str) -> Result<PathBuf> {
    for attempt in 0..100 {
        let dir = base.join(format!(
            "{prefix}{}-{}-{attempt}",
            std::process::id(),
            unique_suffix()
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    anyhow::bail!("could not create a unique directory in {}", base.display())
}

/// A number no earlier call in this process returned, and that later
/// processes are unlikely to reuse: wall-clock nanoseconds × 1000 plus a
/// per-process counter. Dividing by 1000 recovers the creation time.
pub(crate) fn unique_suffix() -> u128 {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    unix_nanos().saturating_mul(1000) + u128::from(counter % 1000)
}

/// The wall-clock nanoseconds a [`unique_suffix`] was taken at.
pub(crate) fn unique_suffix_nanos(suffix: u128) -> u128 {
    suffix / 1000
}

pub(crate) fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_parent_and_leaves_no_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("integration.bash");

        write(&path, b"source me\n", Access::Inherit).unwrap();
        write(&path, b"source me again\n", Access::Inherit).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"source me again\n");
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1, "temp file left behind");
    }

    #[test]
    fn concurrent_writes_leave_one_complete_payload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("integration.bash");
        let first = b"first\n".repeat(16_384);
        let second = b"second\n".repeat(16_384);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let (path, first, second) = (&path, &first, &second);
                scope.spawn(move || {
                    write(path, first, Access::Inherit).unwrap();
                    write(path, second, Access::Inherit).unwrap();
                });
            }
        });

        let final_contents = std::fs::read(&path).unwrap();
        assert!(
            final_contents == first || final_contents == second,
            "final file contained a partial or mixed payload"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only_and_inherit_keeps_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("queue.json");
        write(&private, b"{}", Access::Private).unwrap();
        let mode = std::fs::metadata(&private).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let rc = temp.path().join(".bashrc");
        std::fs::write(&rc, b"old").unwrap();
        std::fs::set_permissions(&rc, std::fs::Permissions::from_mode(0o640)).unwrap();
        write(&rc, b"new", Access::Inherit).unwrap();
        let mode = std::fs::metadata(&rc).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }
}
