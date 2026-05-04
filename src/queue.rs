//! Queue data model + persistence.
//!
//! The queue is an ordered list of pending commands. Each item has a unique id
//! (monotonic), a command string, and a flag for "only run if previous
//! succeeded" (conditional). Persistence uses a JSON file under the user's
//! data dir so the queue survives across cmdq restarts within the user's
//! environment.
//!
//! Operations are intentionally simple — push/edit/remove/move/clear — and
//! independent of any UI concerns. Writes are atomic, use unique temporary
//! files, and move corrupt JSON aside before returning an empty queue so a
//! later save does not destroy the only copy of the user's pending commands.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueItem {
    pub id: u64,
    pub command: String,
    /// If true, only run when the previous command's exit code was 0.
    pub conditional: bool,
    #[serde(default)]
    pub origin_cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    items: Vec<QueueItem>,
    next_id: u64,
    #[serde(default)]
    origin_cwd: Option<PathBuf>,
    /// When true, the runtime will not auto-dispatch on CommandEnd.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveMerge {
    pub unseen_items: usize,
    pub warning: Option<String>,
    pub external_pause: bool,
    pub external_resume: bool,
    pub item_conflicts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueClaim {
    Claimed(QueueItem),
    BlockedByCwd(QueueItem),
    Stale,
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

    pub fn item_ids(&self) -> HashSet<u64> {
        self.items.iter().map(|it| it.id).collect()
    }

    pub fn item_snapshot(&self) -> Vec<QueueItem> {
        self.items.clone()
    }

    pub fn origin_cwd(&self) -> Option<&Path> {
        self.origin_cwd.as_deref()
    }

    pub fn set_origin_cwd(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        for item in &mut self.items {
            if item.origin_cwd.is_none() {
                item.origin_cwd = Some(path.clone());
            }
        }
        self.origin_cwd = Some(path);
    }

    pub fn retarget_origin_cwd(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        for item in &mut self.items {
            item.origin_cwd = Some(path.clone());
        }
        self.origin_cwd = Some(path);
    }

    pub fn push(&mut self, command: impl Into<String>, conditional: bool) -> u64 {
        self.push_with_origin(command, conditional, self.origin_cwd.clone())
    }

    pub fn push_with_origin(
        &mut self,
        command: impl Into<String>,
        conditional: bool,
        origin_cwd: Option<PathBuf>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(QueueItem {
            id,
            command: command.into(),
            conditional,
            origin_cwd,
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

    pub fn mismatched_origins(&self, current: &Path) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for item in &self.items {
            let Some(origin) = item.origin_cwd.as_ref().or(self.origin_cwd.as_ref()) else {
                continue;
            };
            if origin != current && seen.insert(origin.clone()) {
                out.push(origin.clone());
            }
        }
        if out.is_empty()
            && let Some(origin) = self.origin_cwd.as_ref()
            && origin != current
            && seen.insert(origin.clone())
        {
            out.push(origin.clone());
        }
        out
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
        with_queue_file_lock(path, || self.save_snapshot_unlocked(path))
    }

    pub fn save_preserving_unseen(
        &mut self,
        path: &Path,
        known_items: &mut Vec<QueueItem>,
        known_paused: &mut bool,
    ) -> Result<SaveMerge> {
        with_queue_file_lock(path, || {
            let merge = self.merge_disk_snapshot_unlocked(path, known_items, known_paused)?;
            self.save_snapshot_unlocked(path)?;
            *known_items = self.item_snapshot();
            *known_paused = self.paused;
            Ok(merge)
        })
    }

    pub fn sync_from_disk(
        &mut self,
        path: &Path,
        known_items: &mut Vec<QueueItem>,
        known_paused: &mut bool,
    ) -> Result<SaveMerge> {
        with_queue_file_lock(path, || {
            let merge = self.merge_disk_snapshot_unlocked(path, known_items, known_paused)?;
            if merge.warning.is_some() {
                self.save_snapshot_unlocked(path)?;
            }
            *known_items = self.item_snapshot();
            *known_paused = self.paused;
            Ok(merge)
        })
    }

    pub fn disk_differs_from_known(
        path: &Path,
        known_items: &[QueueItem],
        known_paused: bool,
    ) -> Result<bool> {
        with_queue_file_lock(path, || {
            let disk = match std::fs::read(path) {
                Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                    .with_context(|| format!("parsing queue file {}", path.display()))?,
                Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading queue file {}", path.display()));
                }
            };
            let mut disk = disk;
            disk.backfill_item_origins();
            Ok(disk.paused != known_paused || disk.item_snapshot() != known_items)
        })
    }

    pub fn claim_next_eligible_if_current(
        &mut self,
        path: &Path,
        expected_id: u64,
        prev_exit: Option<i32>,
        current_cwd: Option<&Path>,
        known_items: &mut Vec<QueueItem>,
        known_paused: &mut bool,
    ) -> Result<QueueClaim> {
        with_queue_file_lock(path, || {
            let mut merge = SaveMerge::default();
            let mut disk = self.read_disk_snapshot_for_merge(path, &mut merge)?;
            if merge.warning.is_some() || disk.paused {
                *self = disk;
                *known_items = self.item_snapshot();
                *known_paused = self.paused;
                return Ok(QueueClaim::Stale);
            }

            let mut changed = false;
            while disk
                .items
                .first()
                .map(|item| item.conditional && prev_exit != Some(0))
                .unwrap_or(false)
            {
                disk.items.remove(0);
                changed = true;
            }

            let Some(item) = disk.items.first().cloned() else {
                if changed {
                    disk.save_snapshot_unlocked(path)?;
                }
                *self = disk;
                *known_items = self.item_snapshot();
                *known_paused = self.paused;
                return Ok(QueueClaim::Stale);
            };

            if item.id != expected_id {
                if changed {
                    disk.save_snapshot_unlocked(path)?;
                }
                *self = disk;
                *known_items = self.item_snapshot();
                *known_paused = self.paused;
                return Ok(QueueClaim::Stale);
            }

            if let Some(current) = current_cwd
                && let Some(origin) = item.origin_cwd.as_deref().or(disk.origin_cwd())
                && origin != current
            {
                disk.paused = true;
                disk.save_snapshot_unlocked(path)?;
                *self = disk;
                *known_items = self.item_snapshot();
                *known_paused = self.paused;
                return Ok(QueueClaim::BlockedByCwd(item));
            }

            let item = disk.items.remove(0);
            disk.save_snapshot_unlocked(path)?;
            *self = disk;
            *known_items = self.item_snapshot();
            *known_paused = self.paused;
            Ok(QueueClaim::Claimed(item))
        })
    }

    pub fn restore_claimed_front(
        &mut self,
        path: &Path,
        item: QueueItem,
        known_items: &mut Vec<QueueItem>,
        known_paused: &mut bool,
    ) -> Result<()> {
        with_queue_file_lock(path, || {
            let mut merge = SaveMerge::default();
            let mut disk = self.read_disk_snapshot_for_merge(path, &mut merge)?;
            if !disk.items.iter().any(|existing| existing.id == item.id) {
                disk.items.insert(0, item);
            }
            disk.paused = true;
            disk.next_id = disk.next_id.max(
                disk.max_item_id()
                    .map(|id| id.saturating_add(1))
                    .unwrap_or(0),
            );
            disk.save_snapshot_unlocked(path)?;
            *self = disk;
            *known_items = self.item_snapshot();
            *known_paused = self.paused;
            Ok(())
        })
    }

    fn save_snapshot_unlocked(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating queue parent dir {}", parent.display()))?;
        }
        let tmp = unique_temp_path(path);
        let data = serde_json::to_vec(self)?;
        std::fs::write(&tmp, data)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e).with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
            }
        }
    }

    fn merge_disk_snapshot_unlocked(
        &mut self,
        path: &Path,
        known_items: &[QueueItem],
        known_paused: &mut bool,
    ) -> Result<SaveMerge> {
        let mut merge = SaveMerge::default();
        let disk = self.read_disk_snapshot_for_merge(path, &mut merge)?;
        if merge.warning.is_some() {
            return Ok(merge);
        }
        if disk.paused != *known_paused && self.paused == *known_paused {
            self.paused = disk.paused;
            if disk.paused {
                merge.external_pause = true;
            } else {
                merge.external_resume = true;
            }
        }
        let item_merge = self.merge_disk_items(disk, known_items);
        merge.unseen_items = item_merge.unseen_items;
        merge.item_conflicts = item_merge.conflicts;
        Ok(merge)
    }

    fn read_disk_snapshot_for_merge(&self, path: &Path, merge: &mut SaveMerge) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(mut q) => {
                    q.backfill_item_origins();
                    Ok(q)
                }
                Err(e) => {
                    let backup = preserve_corrupt_file(path).with_context(|| {
                        format!("preserving corrupt queue file {}", path.display())
                    })?;
                    log::warn!(
                        "moved corrupt queue file {} to {} before merge: {e}",
                        path.display(),
                        backup.display()
                    );
                    merge.warning = Some(format!(
                        "ignored corrupt queue file — backup: {}",
                        backup.display()
                    ));
                    Ok(Self::default())
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading queue file {}", path.display())),
        }
    }

    fn merge_disk_items(&mut self, disk: Self, known_items: &[QueueItem]) -> ItemMerge {
        self.backfill_item_origins();
        let known_ids: HashSet<u64> = known_items.iter().map(|item| item.id).collect();
        let known_by_id: std::collections::HashMap<u64, QueueItem> = known_items
            .iter()
            .map(|item| (item.id, item.clone()))
            .collect();
        let known_order: Vec<u64> = known_items.iter().map(|item| item.id).collect();
        let local_ids: HashSet<u64> = self.items.iter().map(|item| item.id).collect();
        let disk_ids: HashSet<u64> = disk.items.iter().map(|item| item.id).collect();
        let disk_by_id: std::collections::HashMap<u64, QueueItem> = disk
            .items
            .iter()
            .map(|item| (item.id, item.clone()))
            .collect();
        let mut used = HashSet::new();
        let mut unseen = Vec::new();
        let mut next_id = self.next_id.max(disk.next_id);

        for item in &disk.items {
            if !known_ids.contains(&item.id) && used.insert(item.id) {
                next_id = next_id.max(item.id.saturating_add(1));
                unseen.push(item.clone());
            }
        }

        let prefer_disk_order = should_preserve_disk_known_order(
            &self.items,
            &disk.items,
            &known_order,
            &known_ids,
            &local_ids,
            &disk_ids,
        );
        let base_items: Vec<QueueItem> = if prefer_disk_order {
            disk.items
                .iter()
                .filter(|item| known_ids.contains(&item.id) && local_ids.contains(&item.id))
                .filter_map(|disk_item| self.items.iter().find(|local| local.id == disk_item.id))
                .cloned()
                .chain(
                    self.items
                        .iter()
                        .filter(|item| {
                            if !known_ids.contains(&item.id) {
                                return true;
                            }
                            !disk_ids.contains(&item.id)
                                && known_by_id
                                    .get(&item.id)
                                    .map(|known_item| *item != known_item)
                                    .unwrap_or(false)
                        })
                        .cloned(),
                )
                .collect()
        } else {
            self.items.clone()
        };

        let mut local = Vec::new();
        let mut conflicts = 0usize;
        for mut item in base_items {
            if let Some(known_item) = known_by_id.get(&item.id) {
                let local_changed = &item != known_item;
                if let Some(disk_item) = disk_by_id.get(&item.id) {
                    let disk_changed = disk_item != known_item;
                    if !local_changed && disk_changed {
                        item = disk_item.clone();
                    } else if local_changed && disk_changed && &item != disk_item {
                        conflicts += 1;
                    }
                } else if local_changed {
                    conflicts += 1;
                } else {
                    continue;
                }
            }
            local.push_with_unique_id(item, &mut used, &mut next_id);
        }

        let unseen_count = unseen.len();
        let merged = if known_ids.is_empty() {
            unseen.into_iter().chain(local).collect()
        } else {
            local.into_iter().chain(unseen).collect()
        };

        self.items = merged;
        self.next_id = next_id.max(
            self.max_item_id()
                .map(|id| id.saturating_add(1))
                .unwrap_or(0),
        );
        if self.origin_cwd.is_none() {
            self.origin_cwd = disk.origin_cwd;
        }
        ItemMerge {
            unseen_items: unseen_count,
            conflicts,
        }
    }

    fn max_item_id(&self) -> Option<u64> {
        self.items.iter().map(|it| it.id).max()
    }

    fn backfill_item_origins(&mut self) {
        if let Some(origin) = self.origin_cwd.clone() {
            for item in &mut self.items {
                if item.origin_cwd.is_none() {
                    item.origin_cwd = Some(origin.clone());
                }
            }
        }
        self.next_id = self.next_id.max(
            self.max_item_id()
                .map(|id| id.saturating_add(1))
                .unwrap_or(0),
        );
    }

    pub fn load_or_default(path: &Path) -> Self {
        Self::load_or_default_with_warning(path).0
    }

    pub fn load_or_default_with_warning(path: &Path) -> (Self, Option<String>) {
        match with_queue_file_lock(path, || {
            Ok(Self::load_or_default_with_warning_unlocked(path))
        }) {
            Ok(result) => result,
            Err(e) => (
                Self::default(),
                Some(format!("could not lock queue file {}: {e}", path.display())),
            ),
        }
    }

    fn load_or_default_with_warning_unlocked(path: &Path) -> (Self, Option<String>) {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(mut q) => {
                    q.backfill_item_origins();
                    (q, None)
                }
                Err(e) => {
                    let warning = match preserve_corrupt_file(path) {
                        Ok(backup) => {
                            log::warn!(
                                "moved corrupt queue file {} to {}: {e}",
                                path.display(),
                                backup.display()
                            );
                            format!("ignored corrupt queue file — backup: {}", backup.display())
                        }
                        Err(backup_error) => {
                            log::warn!(
                                "ignoring corrupt queue file {}; backup failed: {backup_error}; parse error: {e}",
                                path.display()
                            );
                            format!(
                                "ignored corrupt queue file {} (backup failed: {backup_error})",
                                path.display()
                            )
                        }
                    };
                    (Self::default(), Some(warning))
                }
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (
                Self::default(),
                Some(format!("could not read queue file {}: {e}", path.display())),
            ),
        }
    }
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("queue");
    let tmp_name = format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        monotonic_suffix()
    );
    path.with_file_name(tmp_name)
}

fn next_available_id(used: &mut HashSet<u64>, next_id: &mut u64) -> u64 {
    loop {
        let candidate = *next_id;
        *next_id = next_id.saturating_add(1);
        if used.insert(candidate) {
            return candidate;
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ItemMerge {
    unseen_items: usize,
    conflicts: usize,
}

fn should_preserve_disk_known_order(
    local_items: &[QueueItem],
    disk_items: &[QueueItem],
    known_order: &[u64],
    known_ids: &HashSet<u64>,
    local_ids: &HashSet<u64>,
    disk_ids: &HashSet<u64>,
) -> bool {
    if known_order.is_empty() {
        return false;
    }
    let shared_known: Vec<u64> = known_order
        .iter()
        .copied()
        .filter(|id| local_ids.contains(id) && disk_ids.contains(id))
        .collect();
    let local_known_order: Vec<u64> = local_items
        .iter()
        .map(|item| item.id)
        .filter(|id| known_ids.contains(id) && disk_ids.contains(id))
        .collect();
    let disk_known_order: Vec<u64> = disk_items
        .iter()
        .map(|item| item.id)
        .filter(|id| known_ids.contains(id) && local_ids.contains(id))
        .collect();
    local_known_order == shared_known && disk_known_order != shared_known
}

trait PushQueueItemUnique {
    fn push_with_unique_id(&mut self, item: QueueItem, used: &mut HashSet<u64>, next_id: &mut u64);
}

impl PushQueueItemUnique for Vec<QueueItem> {
    fn push_with_unique_id(
        &mut self,
        mut item: QueueItem,
        used: &mut HashSet<u64>,
        next_id: &mut u64,
    ) {
        if !used.insert(item.id) {
            item.id = next_available_id(used, next_id);
        }
        *next_id = (*next_id).max(item.id.saturating_add(1));
        self.push(item);
    }
}

fn monotonic_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn preserve_corrupt_file(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("queue");
    let backup_name = format!("{file_name}.corrupt-{}", monotonic_suffix());
    let backup = path.with_file_name(backup_name);
    std::fs::rename(path, &backup)?;
    Ok(backup)
}

fn with_queue_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating queue parent dir {}", parent.display()))?;
    }
    let lock_path = lock_path_for(path);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening queue lock {}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("locking queue file {}", lock_path.display()))?;
    f()
}

fn lock_path_for(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("queue");
    path.with_file_name(format!("{file_name}.lock"))
}

/// Fallible default persistence path: `$XDG_DATA_HOME/cmdq/queue.json`,
/// the platform data directory, or `~/.cmdq/queue.json`.
pub fn try_default_path() -> Result<PathBuf> {
    if let Some(dir) = crate::paths::xdg_data_home() {
        return Ok(dir.join("cmdq").join("queue.json"));
    }
    if let Some(dir) = dirs::data_dir().and_then(crate::paths::absolute_path) {
        return Ok(dir.join("cmdq").join("queue.json"));
    }
    if let Some(home) = dirs::home_dir().and_then(crate::paths::absolute_path) {
        return Ok(home.join(".cmdq").join("queue.json"));
    }
    Err(anyhow!(
        "could not locate an absolute home/data dir; set XDG_DATA_HOME or HOME"
    ))
}

/// Default persistence path. Prefer `try_default_path` when startup can report
/// a useful error; this infallible form avoids ever falling back to the cwd.
pub fn default_path() -> PathBuf {
    try_default_path().unwrap_or_else(|_| fallback_temp_path())
}

fn fallback_temp_path() -> PathBuf {
    let mut dir = std::env::temp_dir();
    if !dir.is_absolute() {
        dir = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };
    }
    dir.join("cmdq").join("queue.json")
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
        q.set_origin_cwd(dir.path());
        q.push("first", false);
        q.push("second", true);
        q.paused = true;
        q.save(&path).unwrap();
        let loaded = Queue::load_or_default(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.paused);
        assert_eq!(loaded.origin_cwd(), Some(dir.path()));
        assert_eq!(loaded.items()[1].command, "second");
        assert!(loaded.items()[1].conditional);
    }

    #[test]
    fn legacy_queue_without_paused_field_still_loads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(
            &path,
            r#"{"items":[{"id":0,"command":"echo old","conditional":false}],"next_id":1}"#,
        )
        .unwrap();

        let (loaded, warning) = Queue::load_or_default_with_warning(&path);

        assert!(warning.is_none());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "echo old");
        assert!(!loaded.paused);
    }

    #[test]
    fn legacy_queue_origin_backfills_item_origins() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(
            &path,
            r#"{"items":[{"id":0,"command":"echo old","conditional":false}],"next_id":1,"origin_cwd":"/tmp/legacy"}"#,
        )
        .unwrap();

        let loaded = Queue::load_or_default(&path);

        assert_eq!(loaded.origin_cwd(), Some(Path::new("/tmp/legacy")));
        assert_eq!(
            loaded.items()[0].origin_cwd.as_deref(),
            Some(Path::new("/tmp/legacy"))
        );
    }

    #[test]
    fn save_ignores_stale_fixed_temp_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::create_dir(dir.path().join("q.json.tmp")).unwrap();

        let mut q = Queue::new();
        q.push("durable", false);
        q.save(&path).unwrap();

        let loaded = Queue::load_or_default(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "durable");
    }

    #[test]
    fn merge_save_preserves_concurrent_additions_with_id_collision() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut first = Queue::new();
        let mut second = Queue::new();
        let mut first_known = Vec::new();
        let mut second_known = Vec::new();
        let mut first_paused = false;
        let mut second_paused = false;

        first.set_origin_cwd("/tmp/first");
        first.push("echo from first terminal", false);
        first
            .save_preserving_unseen(&path, &mut first_known, &mut first_paused)
            .unwrap();
        second.set_origin_cwd("/tmp/second");
        second.push("echo from second terminal", false);
        let merge = second
            .save_preserving_unseen(&path, &mut second_known, &mut second_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        let ids: HashSet<_> = loaded.items().iter().map(|it| it.id).collect();
        assert_eq!(loaded.len(), 2);
        assert_eq!(ids.len(), 2, "merged items should have distinct ids");
        assert_eq!(
            commands,
            vec!["echo from first terminal", "echo from second terminal"]
        );
        assert_eq!(merge.unseen_items, 1);
        assert!(loaded.items().iter().any(|it| {
            it.command == "echo from first terminal"
                && it.origin_cwd.as_deref() == Some(Path::new("/tmp/first"))
        }));
        assert!(loaded.items().iter().any(|it| {
            it.command == "echo from second terminal"
                && it.origin_cwd.as_deref() == Some(Path::new("/tmp/second"))
        }));
    }

    #[test]
    fn save_preserving_unseen_serializes_concurrent_writers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();

        for command in ["echo from first process", "echo from second process"] {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let mut q = Queue::new();
                q.push(command, false);
                let mut known = Vec::new();
                let mut known_paused = false;
                barrier.wait();
                q.save_preserving_unseen(&path, &mut known, &mut known_paused)
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let loaded = Queue::load_or_default(&path);
        let commands: HashSet<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(loaded.len(), 2);
        assert!(commands.contains("echo from first process"));
        assert!(commands.contains("echo from second process"));
    }

    #[test]
    fn merge_save_appends_unseen_items_after_known_local_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut session = Queue::new();
        session.set_origin_cwd("/tmp/session");
        let known_id = session.push("echo known", false);
        session.save(&path).unwrap();
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        external.set_origin_cwd("/tmp/external");
        external.push("echo external", false);
        external.save(&path).unwrap();

        session.push("echo local new", false);
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(
            commands,
            vec!["echo known", "echo local new", "echo external"]
        );
        assert_eq!(merge.unseen_items, 1);
        assert_eq!(loaded.items()[0].id, known_id);
    }

    #[test]
    fn merge_save_preserves_external_edit_when_local_item_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let shared_id = base.push("echo original", false);
        base.push("echo second", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.edit(shared_id, "echo external edit"));
        external.save(&path).unwrap();

        session.push("echo local add", false);
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(
            commands,
            vec!["echo external edit", "echo second", "echo local add"]
        );
        assert_eq!(merge.item_conflicts, 0);
    }

    #[test]
    fn merge_save_preserves_external_reorder_when_local_order_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let first = base.push("echo first", false);
        base.push("echo second", false);
        base.push("echo third", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.move_down(first));
        external.save(&path).unwrap();

        session.push("echo local add", false);
        session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|it| it.command.as_str())
            .collect();
        assert_eq!(
            commands,
            vec!["echo second", "echo first", "echo third", "echo local add"]
        );
    }

    #[test]
    fn merge_save_reports_conflict_when_local_and_external_edit_same_item() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let shared_id = base.push("echo original", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.edit(shared_id, "echo external edit"));
        external.save(&path).unwrap();

        assert!(session.edit(shared_id, "echo local edit"));
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert_eq!(merge.item_conflicts, 1);
        assert_eq!(loaded.items()[0].command, "echo local edit");
    }

    #[test]
    fn sync_from_disk_preserves_external_edit_when_local_item_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let shared_id = base.push("echo original", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.edit(shared_id, "echo external edit"));
        external.save(&path).unwrap();

        let merge = session
            .sync_from_disk(&path, &mut known, &mut known_paused)
            .unwrap();

        assert_eq!(merge, SaveMerge::default());
        assert_eq!(session.items()[0].command, "echo external edit");
        assert_eq!(known, session.item_snapshot());
    }

    #[test]
    fn sync_from_disk_treats_missing_queue_file_as_external_clear() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        base.push("echo clear me", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        std::fs::remove_file(&path).unwrap();

        let merge = session
            .sync_from_disk(&path, &mut known, &mut known_paused)
            .unwrap();

        assert_eq!(merge, SaveMerge::default());
        assert!(session.is_empty());
        assert!(known.is_empty());
        assert!(
            !path.exists(),
            "sync should not recreate a deliberate clear"
        );
    }

    #[test]
    fn merge_save_does_not_resurrect_known_removed_item() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let old_id = base.push("echo remove me", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        assert!(session.remove(old_id).is_some());
        session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn merge_save_does_not_resurrect_item_removed_by_another_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut first = Queue::new();
        first.push("echo first", false);
        first.save(&path).unwrap();
        let mut first_known = first.item_snapshot();
        let mut first_paused = first.paused;

        let mut second = Queue::load_or_default(&path);
        let mut second_known = second.item_snapshot();
        let mut second_paused = second.paused;
        let external_id = second.push("echo external", false);
        second
            .save_preserving_unseen(&path, &mut second_known, &mut second_paused)
            .unwrap();

        first
            .save_preserving_unseen(&path, &mut first_known, &mut first_paused)
            .unwrap();
        assert!(
            first.remove(first.items()[0].id).is_some(),
            "unrelated local mutation"
        );

        let mut second = Queue::load_or_default(&path);
        let mut second_known = second.item_snapshot();
        let mut second_paused = second.paused;
        assert!(second.remove(external_id).is_some());
        second
            .save_preserving_unseen(&path, &mut second_known, &mut second_paused)
            .unwrap();

        first
            .save_preserving_unseen(&path, &mut first_known, &mut first_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(
            loaded
                .items()
                .iter()
                .all(|item| item.command != "echo external"),
            "stale live session must not resurrect externally removed item"
        );
    }

    #[test]
    fn merge_save_reports_conflict_when_local_edit_races_external_delete() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let shared_id = base.push("echo original", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.remove(shared_id).is_some());
        external.save(&path).unwrap();

        assert!(session.edit(shared_id, "echo local edit"));
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert_eq!(merge.item_conflicts, 1);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "echo local edit");
    }

    #[test]
    fn merge_save_keeps_local_edit_when_external_delete_also_reorders() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        let first = base.push("echo first", false);
        let deleted = base.push("echo deleted elsewhere", false);
        base.push("echo third", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;

        let mut external = Queue::load_or_default(&path);
        assert!(external.remove(deleted).is_some());
        assert!(external.move_down(first));
        external.save(&path).unwrap();

        assert!(session.edit(deleted, "echo local edit"));
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        let commands: Vec<_> = loaded
            .items()
            .iter()
            .map(|item| item.command.as_str())
            .collect();
        assert_eq!(merge.item_conflicts, 1);
        assert_eq!(
            commands,
            vec!["echo third", "echo first", "echo local edit"]
        );
    }

    #[test]
    fn merge_save_does_not_resurrect_items_after_queue_file_deleted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        base.push("echo clear me", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        std::fs::remove_file(&path).unwrap();

        session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn merge_save_preserves_new_external_pause_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut first = Queue::new();
        first.push("echo shared", false);
        first.save(&path).unwrap();
        let mut first_known = first.item_snapshot();
        let mut first_paused = first.paused;

        let mut second = Queue::load_or_default(&path);
        second.paused = true;
        second.save(&path).unwrap();

        let merge = first
            .save_preserving_unseen(&path, &mut first_known, &mut first_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(merge.external_pause);
        assert!(first.paused);
        assert!(loaded.paused);
    }

    #[test]
    fn merge_save_allows_local_resume_of_known_pause_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut q = Queue::new();
        q.push("echo shared", false);
        q.paused = true;
        q.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        session.paused = false;
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(!merge.external_pause);
        assert!(!session.paused);
        assert!(!loaded.paused);
    }

    #[test]
    fn merge_save_preserves_new_external_resume_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut first = Queue::new();
        first.push("echo shared", false);
        first.paused = true;
        first.save(&path).unwrap();
        let mut first_known = first.item_snapshot();
        let mut first_paused = first.paused;

        let mut second = Queue::load_or_default(&path);
        second.paused = false;
        second.save(&path).unwrap();

        let merge = first
            .save_preserving_unseen(&path, &mut first_known, &mut first_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(merge.external_resume);
        assert!(!merge.external_pause);
        assert!(!first.paused);
        assert!(!loaded.paused);
    }

    #[test]
    fn merge_save_allows_local_pause_of_known_running_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut q = Queue::new();
        q.push("echo shared", false);
        q.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        session.paused = true;
        let merge = session
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert!(!merge.external_pause);
        assert!(!merge.external_resume);
        assert!(session.paused);
        assert!(loaded.paused);
    }

    #[test]
    fn save_preserving_unseen_backs_up_corrupt_disk_file_before_overwrite() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let mut q = Queue::new();
        q.push("echo durable", false);
        let mut known = Vec::new();
        let mut known_paused = false;

        let merge = q
            .save_preserving_unseen(&path, &mut known, &mut known_paused)
            .unwrap();

        let loaded = Queue::load_or_default(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.items()[0].command, "echo durable");
        assert!(
            merge
                .warning
                .unwrap()
                .contains("ignored corrupt queue file")
        );

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("q.json.corrupt-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1, "expected one corrupt queue backup");
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{not valid json");
    }

    #[test]
    fn sync_from_disk_backs_up_corrupt_file_and_preserves_live_queue() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        let mut base = Queue::new();
        base.push("echo keep me", false);
        base.save(&path).unwrap();

        let mut session = Queue::load_or_default(&path);
        let mut known = session.item_snapshot();
        let mut known_paused = session.paused;
        std::fs::write(&path, b"{not valid json").unwrap();

        let merge = session
            .sync_from_disk(&path, &mut known, &mut known_paused)
            .unwrap();

        assert_eq!(session.len(), 1);
        assert_eq!(session.items()[0].command, "echo keep me");
        assert_eq!(
            Queue::load_or_default(&path).items()[0].command,
            "echo keep me"
        );
        assert!(
            merge
                .warning
                .unwrap()
                .contains("ignored corrupt queue file")
        );

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("q.json.corrupt-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{not valid json");
    }

    #[test]
    fn corrupt_queue_file_is_moved_aside_with_warning() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(&path, b"{not valid json").unwrap();

        let (loaded, warning) = Queue::load_or_default_with_warning(&path);

        assert!(loaded.is_empty());
        assert!(warning.unwrap().contains("ignored corrupt queue file"));
        assert!(!path.exists(), "corrupt live queue should be moved aside");

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("q.json.corrupt-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{not valid json");
    }

    #[test]
    fn concurrent_corrupt_loads_create_one_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Queue::load_or_default_with_warning(&path)
                })
            })
            .collect();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(results.iter().all(|(queue, _)| queue.is_empty()));
        assert!(
            results
                .iter()
                .any(|(_, warning)| warning.as_deref().unwrap_or("").contains("ignored corrupt"))
        );

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("q.json.corrupt-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{not valid json");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let (q, warning) = Queue::load_or_default_with_warning(&path);
        assert!(q.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn load_queue_path_read_error_returns_warning() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("q.json");
        std::fs::create_dir(&path).unwrap();

        let (q, warning) = Queue::load_or_default_with_warning(&path);

        assert!(q.is_empty());
        assert!(warning.unwrap().contains("could not read queue file"));
    }
}
