use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const LEASE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SessionLease {
    path: PathBuf,
    record: LeaseRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    pid: u32,
    queue_path: PathBuf,
    cwd: Option<PathBuf>,
    updated_at_millis: u128,
}

impl SessionLease {
    pub fn start(queue_path: &Path, cwd: Option<&Path>) -> Result<Self> {
        let dir = lease_dir(queue_path)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session lease dir {}", dir.display()))?;
        let path = dir.join(format!(
            "session-{}-{}.json",
            std::process::id(),
            monotonic_suffix()
        ));
        let mut lease = Self {
            path,
            record: LeaseRecord {
                pid: std::process::id(),
                queue_path: queue_path.to_path_buf(),
                cwd: cwd.map(Path::to_path_buf),
                updated_at_millis: now_millis(),
            },
        };
        lease.refresh()?;
        Ok(lease)
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.record.updated_at_millis = now_millis();
        write_record_atomic(&self.path, &self.record)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn active_peer_count(queue_path: &Path) -> Result<usize> {
    let dir = lease_dir(queue_path)?;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let now = now_millis();
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<LeaseRecord>(&bytes) else {
            continue;
        };
        if record.queue_path != queue_path {
            continue;
        }
        if now.saturating_sub(record.updated_at_millis) > LEASE_TTL.as_millis() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        count += 1;
    }
    Ok(count)
}

fn lease_dir(queue_path: &Path) -> Result<PathBuf> {
    let parent = queue_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("queue path has no parent: {}", queue_path.display()))?;
    Ok(parent.join("session-leases"))
}

fn write_record_atomic(path: &Path, record: &LeaseRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating session lease dir {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("lease"),
        std::process::id(),
        monotonic_suffix()
    ));
    let data = serde_json::to_vec(record)?;
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e).with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
        }
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn monotonic_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_peer_count_tracks_live_lease_and_drop_removes_it() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");

        let lease = SessionLease::start(&queue_path, Some(temp.path())).unwrap();
        assert_eq!(active_peer_count(&queue_path).unwrap(), 1);

        drop(lease);
        assert_eq!(active_peer_count(&queue_path).unwrap(), 0);
    }

    #[test]
    fn active_peer_count_ignores_stale_leases() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.json");
        let dir = lease_dir(&queue_path).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("session-stale.json");
        let record = LeaseRecord {
            pid: 123,
            queue_path: queue_path.clone(),
            cwd: None,
            updated_at_millis: now_millis() - LEASE_TTL.as_millis() - 1,
        };
        write_record_atomic(&stale, &record).unwrap();

        assert_eq!(active_peer_count(&queue_path).unwrap(), 0);
        assert!(!stale.exists());
    }
}
