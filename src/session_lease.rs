use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::atomic_file::{self, Access};

const LEASE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct SessionLease {
    path: PathBuf,
    record: LeaseRecord,
    /// Serializes refreshes against removal from a signal-handling thread.
    io: Arc<Mutex<()>>,
}

/// Removes a lease's file from another thread when the process is about to
/// exit, without racing a refresh that would recreate it.
#[derive(Debug)]
pub struct LeaseRemover {
    path: PathBuf,
    io: Arc<Mutex<()>>,
}

impl LeaseRemover {
    /// Remove the lease file and block further refreshes until the process
    /// exits.
    pub fn remove_for_exit(self) {
        let guard = lock(&self.io);
        let _ = std::fs::remove_file(&self.path);
        std::mem::forget(guard);
    }
}

fn lock(io: &Mutex<()>) -> MutexGuard<'_, ()> {
    io.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
            atomic_file::unique_suffix()
        ));
        let mut lease = Self {
            path,
            record: LeaseRecord {
                pid: std::process::id(),
                queue_path: queue_path.to_path_buf(),
                cwd: cwd.map(Path::to_path_buf),
                updated_at_millis: now_millis(),
            },
            io: Arc::default(),
        };
        lease.refresh()?;
        Ok(lease)
    }

    pub fn refresh(&mut self) -> Result<()> {
        let _io = lock(&self.io);
        self.record.updated_at_millis = now_millis();
        write_record_atomic(&self.path, &self.record)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remover(&self) -> LeaseRemover {
        LeaseRemover {
            path: self.path.clone(),
            io: self.io.clone(),
        }
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
    atomic_file::write(path, &serde_json::to_vec(record)?, Access::Inherit)
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
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
