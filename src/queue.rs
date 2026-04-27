//! Queue data model + persistence.
//!
//! The queue is an ordered list of pending commands. Each item has a unique id
//! (monotonic), a command string, and a flag for "only run if previous
//! succeeded" (conditional). Persistence uses a JSON file at
//! `~/.cmdq/queue.json` so the queue survives across cmdq restarts within the
//! user's environment.
//!
//! Operations are intentionally simple — push/edit/remove/move/clear — and
//! independent of any UI concerns.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueItem {
    pub id: u64,
    pub command: String,
    /// If true, only run when the previous command's exit code was 0.
    pub conditional: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    items: Vec<QueueItem>,
    next_id: u64,
    /// When true, the runtime will not auto-dispatch on CommandEnd.
    pub paused: bool,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn push(&mut self, command: impl Into<String>, conditional: bool) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(QueueItem {
            id,
            command: command.into(),
            conditional,
        });
        id
    }

    pub fn front(&self) -> Option<&QueueItem> {
        self.items.first()
    }

    pub fn remove(&mut self, id: u64) -> Option<QueueItem> {
        let idx = self.items.iter().position(|it| it.id == id)?;
        Some(self.items.remove(idx))
    }

    pub fn edit(&mut self, id: u64, new_command: impl Into<String>) -> bool {
        if let Some(it) = self.items.iter_mut().find(|it| it.id == id) {
            it.command = new_command.into();
            true
        } else {
            false
        }
    }

    pub fn set_conditional(&mut self, id: u64, conditional: bool) -> bool {
        if let Some(it) = self.items.iter_mut().find(|it| it.id == id) {
            it.conditional = conditional;
            true
        } else {
            false
        }
    }

    pub fn move_up(&mut self, id: u64) -> bool {
        let idx = self.items.iter().position(|it| it.id == id);
        match idx {
            Some(i) if i > 0 => {
                self.items.swap(i, i - 1);
                true
            }
            _ => false,
        }
    }

    pub fn move_down(&mut self, id: u64) -> bool {
        let idx = self.items.iter().position(|it| it.id == id);
        match idx {
            Some(i) if i + 1 < self.items.len() => {
                self.items.swap(i, i + 1);
                true
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Find next item to dispatch given the previous command's exit code.
    /// Items with `conditional = true` are skipped (and removed) when
    /// `prev_exit != Some(0)`. Returns the first item that is allowed to run.
    pub fn pop_next_eligible(&mut self, prev_exit: Option<i32>) -> Option<QueueItem> {
        while let Some(front) = self.items.first() {
            if front.conditional && prev_exit != Some(0) {
                self.items.remove(0);
                continue;
            }
            return Some(self.items.remove(0));
        }
        None
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating queue parent dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec(self)?;
        std::fs::write(&tmp, data)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(q) => q,
                Err(e) => {
                    log::warn!("ignoring corrupt queue file {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}

/// Default persistence path: `$XDG_DATA_HOME/cmdq/queue.json` or
/// `~/.cmdq/queue.json`.
pub fn default_path() -> PathBuf {
    if let Some(dir) = dirs::data_dir() {
        return dir.join("cmdq").join("queue.json");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".cmdq").join("queue.json");
    }
    PathBuf::from(".cmdq-queue.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn push_and_len() {
        let mut q = Queue::new();
        assert_eq!(q.len(), 0);
        q.push("ls", false);
        q.push("pwd", false);
        assert_eq!(q.len(), 2);
        assert_eq!(q.front().unwrap().command, "ls");
    }

    #[test]
    fn ids_are_monotonic_and_unique() {
        let mut q = Queue::new();
        let a = q.push("a", false);
        let b = q.push("b", false);
        let c = q.push("c", false);
        assert!(a < b && b < c);
        q.remove(b);
        let d = q.push("d", false);
        assert!(d > c, "ids should not be reused after remove");
    }

    #[test]
    fn edit_changes_command() {
        let mut q = Queue::new();
        let id = q.push("ls", false);
        assert!(q.edit(id, "ls -la"));
        assert_eq!(q.items()[0].command, "ls -la");
        assert!(!q.edit(9999, "nope"));
    }

    #[test]
    fn move_up_and_down() {
        let mut q = Queue::new();
        let a = q.push("a", false);
        let b = q.push("b", false);
        let c = q.push("c", false);
        assert!(q.move_up(c));
        // order: a, c, b
        assert_eq!(q.items()[1].id, c);
        assert_eq!(q.items()[2].id, b);
        assert!(q.move_down(a));
        // order: c, a, b
        assert_eq!(q.items()[0].id, c);
        assert_eq!(q.items()[1].id, a);
        assert!(!q.move_up(c)); // already at top
    }

    #[test]
    fn pop_next_eligible_skips_conditional_after_failure() {
        let mut q = Queue::new();
        q.push("foo", true); // conditional, should be skipped if prev failed
        q.push("bar", false);
        let next = q.pop_next_eligible(Some(1));
        assert_eq!(next.unwrap().command, "bar");
        assert!(q.is_empty());
    }

    #[test]
    fn pop_next_eligible_runs_conditional_after_success() {
        let mut q = Queue::new();
        q.push("foo", true);
        q.push("bar", false);
        let next = q.pop_next_eligible(Some(0));
        assert_eq!(next.unwrap().command, "foo");
    }

    #[test]
    fn pop_next_eligible_unknown_prev_skips_conditional() {
        // Initial dispatch with no prev exit: conditional commands should skip.
        let mut q = Queue::new();
        q.push("foo", true);
        q.push("bar", false);
        let next = q.pop_next_eligible(None);
        assert_eq!(next.unwrap().command, "bar");
    }

    #[test]
    fn persist_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut q = Queue::new();
        q.push("first", false);
        q.push("second", true);
        q.paused = true;
        q.save(&path).unwrap();
        let loaded = Queue::load_or_default(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.paused);
        assert_eq!(loaded.items()[1].command, "second");
        assert!(loaded.items()[1].conditional);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let q = Queue::load_or_default(&path);
        assert!(q.is_empty());
    }
}
