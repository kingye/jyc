//! Cursor store for WeCom KF (Customer Service) incremental message sync.
//!
//! The KF `sync_msg` API uses cursor-based pagination. Cursors must persist
//! across restarts to avoid re-syncing all historical messages. This module
//! provides a thread-safe cursor store with optional file-based persistence.
//!
//! If no `persist_path` is configured, cursors are kept in memory only
//! (lost on restart, but dedup prevents double-processing).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::Result;

/// Thread-safe cursor store for KF sync cursors.
///
/// Maps `open_kfid` → cursor string. Supports optional file persistence
/// via JSON file for durability across restarts.
pub struct KfCursorStore {
    cursors: RwLock<HashMap<String, String>>,
    persist_path: Option<PathBuf>,
}

impl KfCursorStore {
    /// Create a new cursor store.
    ///
    /// If `persist_path` is `Some`, the store will load existing cursors
    /// from the file (if it exists).
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let store = Self {
            cursors: RwLock::new(HashMap::new()),
            persist_path,
        };

        // Load existing cursors from disk (sync, called during construction)
        if let Err(e) = store.load_from_disk() {
            tracing::warn!(
                path = ?store.persist_path,
                error = %e,
                "KfCursorStore: failed to load cursors from disk"
            );
        }

        store
    }

    /// Get the cursor for a given `open_kfid`.
    pub fn get_cursor(&self, open_kfid: &str) -> Option<String> {
        self.cursors
            .read()
            .ok()
            .and_then(|guard| guard.get(open_kfid).cloned())
    }

    /// Set the cursor for a given `open_kfid` and optionally persist to disk.
    pub fn set_cursor(&self, open_kfid: &str, cursor: &str) {
        if let Ok(mut guard) = self.cursors.write() {
            guard.insert(open_kfid.to_string(), cursor.to_string());
        }
        if let Err(e) = self.save_to_disk() {
            tracing::warn!(
                path = ?self.persist_path,
                error = %e,
                "KfCursorStore: failed to persist cursors"
            );
        }
    }

    /// Load cursors from the JSON file.
    fn load_from_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p,
            None => return Ok(()),
        };

        if !path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read cursor file: {}", e))?;

        let data: HashMap<String, String> = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse cursor file: {}", e))?;

        if let Ok(mut guard) = self.cursors.write() {
            guard.extend(data);
        }

        tracing::debug!(
            path = %path.display(),
            "KfCursorStore: loaded cursors from disk"
        );

        Ok(())
    }

    /// Save cursors to disk as JSON.
    fn save_to_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Ok(()),
        };

        let data = {
            let guard = self
                .cursors
                .read()
                .map_err(|e| anyhow::anyhow!("cursor lock poisoned: {}", e))?;
            serde_json::to_string_pretty(&*guard)
                .map_err(|e| anyhow::anyhow!("failed to serialize cursors: {}", e))?
        };

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("failed to create cursor directory: {}", e))?;
        }

        std::fs::write(&path, &data)
            .map_err(|e| anyhow::anyhow!("failed to write cursor file: {}", e))?;

        Ok(())
    }

    #[cfg(test)]
    fn cursors_count(&self) -> usize {
        self.cursors.read().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cursor_get_set() {
        let store = KfCursorStore::new(None);
        assert!(store.get_cursor("kf001").is_none());

        store.set_cursor("kf001", "cursor_abc");
        assert_eq!(
            store.get_cursor("kf001"),
            Some("cursor_abc".to_string())
        );
    }

    #[test]
    fn test_cursor_multiple_kf_accounts() {
        let store = KfCursorStore::new(None);

        store.set_cursor("kf001", "cursor_001");
        store.set_cursor("kf002", "cursor_002");

        assert_eq!(
            store.get_cursor("kf001"),
            Some("cursor_001".to_string())
        );
        assert_eq!(
            store.get_cursor("kf002"),
            Some("cursor_002".to_string())
        );
    }

    #[test]
    fn test_cursor_overwrite() {
        let store = KfCursorStore::new(None);

        store.set_cursor("kf001", "cursor_old");
        assert_eq!(
            store.get_cursor("kf001"),
            Some("cursor_old".to_string())
        );

        store.set_cursor("kf001", "cursor_new");
        assert_eq!(
            store.get_cursor("kf001"),
            Some("cursor_new".to_string())
        );
    }

    #[test]
    fn test_persist_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kf_cursors.json");

        // Create store with persist path and set a cursor
        {
            let store = KfCursorStore::new(Some(path.clone()));
            store.set_cursor("kf001", "cursor_abc");
            assert_eq!(store.cursors_count(), 1);
        }

        // Create a new store with the same path — should load from disk
        {
            let store = KfCursorStore::new(Some(path.clone()));
            assert_eq!(store.cursors_count(), 1);
            assert_eq!(
                store.get_cursor("kf001"),
                Some("cursor_abc".to_string())
            );
        }
    }

    #[test]
    fn test_persist_empty_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kf_cursors.json");

        // Create store with persist path but no cursors set
        {
            let store = KfCursorStore::new(Some(path.clone()));
            assert_eq!(store.cursors_count(), 0);
        }

        // Create a new store with the same path — should load empty state
        {
            let store = KfCursorStore::new(Some(path.clone()));
            assert_eq!(store.cursors_count(), 0);
        }
    }

    #[test]
    fn test_persist_path_none() {
        let store = KfCursorStore::new(None);
        store.set_cursor("kf001", "cursor_abc");
        assert_eq!(
            store.get_cursor("kf001"),
            Some("cursor_abc".to_string())
        );
    }

    #[test]
    fn test_cursor_for_different_open_kfid() {
        let store = KfCursorStore::new(None);
        store.set_cursor("kf001", "cursor_001");

        // Non-existent key should return None
        assert!(store.get_cursor("kf999").is_none());
    }
}
