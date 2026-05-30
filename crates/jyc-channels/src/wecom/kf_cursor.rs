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
use tokio::sync::Mutex as TokioMutex;

/// Thread-safe cursor store for KF sync cursors.
///
/// Maps `open_kfid` → cursor string. Supports optional file persistence
/// via JSON file for durability across restarts. File writes use `tokio::fs`
/// for non-blocking I/O. A dirty flag coalesces multiple writes into a
/// single disk flush to avoid excessive I/O under rapid sync activity.
pub struct KfCursorStore {
    cursors: RwLock<HashMap<String, String>>,
    persist_path: Option<PathBuf>,
    /// Dirty flag and flush lock: only one flush at a time.
    dirty: std::sync::Mutex<bool>,
    flush_lock: TokioMutex<()>,
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
            dirty: std::sync::Mutex::new(false),
            flush_lock: TokioMutex::new(()),
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

    /// Set the cursor for a given `open_kfid` and mark the store as dirty.
    ///
    /// The actual disk write is deferred to the next `flush_to_disk()` call.
    /// Multiple `set_cursor` calls between flushes are coalesced into one write.
    pub fn set_cursor(&self, open_kfid: &str, cursor: &str) {
        if let Ok(mut guard) = self.cursors.write() {
            guard.insert(open_kfid.to_string(), cursor.to_string());
        }
        // Mark dirty — disk write happens on flush
        if let Ok(mut d) = self.dirty.lock() {
            *d = true;
        }
    }

    /// Flush cursors to disk if dirty.
    ///
    /// Uses `tokio::fs` for non-blocking I/O. Only one flush executes at a
    /// time (serialized by `flush_lock`). Call this periodically or on shutdown.
    pub async fn flush_to_disk(&self) -> Result<()> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Ok(()),
        };

        // Check dirty flag without holding the lock
        {
            let d = self.dirty.lock().unwrap();
            if !*d {
                return Ok(());
            }
        }

        // Serialize flushes — only one write at a time
        let _guard = self.flush_lock.lock().await;

        // Re-check dirty flag (another thread may have flushed)
        {
            let d = self.dirty.lock().unwrap();
            if !*d {
                return Ok(());
            }
        }

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
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("failed to create cursor directory: {}", e))?;
        }

        // Write atomically: write to temp file then rename
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| anyhow::anyhow!("failed to write cursor temp file: {}", e))?;
        tokio::fs::rename(&tmp_path, &path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to rename cursor file: {}", e))?;

        // Clear dirty flag
        if let Ok(mut d) = self.dirty.lock() {
            *d = false;
        }

        Ok(())
    }

    /// Load cursors from the JSON file (sync, called during construction).
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
        assert_eq!(store.get_cursor("kf001"), Some("cursor_abc".to_string()));
    }

    #[test]
    fn test_cursor_multiple_kf_accounts() {
        let store = KfCursorStore::new(None);

        store.set_cursor("kf001", "cursor_001");
        store.set_cursor("kf002", "cursor_002");

        assert_eq!(store.get_cursor("kf001"), Some("cursor_001".to_string()));
        assert_eq!(store.get_cursor("kf002"), Some("cursor_002".to_string()));
    }

    #[test]
    fn test_cursor_overwrite() {
        let store = KfCursorStore::new(None);

        store.set_cursor("kf001", "cursor_old");
        assert_eq!(store.get_cursor("kf001"), Some("cursor_old".to_string()));

        store.set_cursor("kf001", "cursor_new");
        assert_eq!(store.get_cursor("kf001"), Some("cursor_new".to_string()));
    }

    #[tokio::test]
    async fn test_persist_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kf_cursors.json");

        // Create store with persist path and set a cursor
        {
            let store = KfCursorStore::new(Some(path.clone()));
            store.set_cursor("kf001", "cursor_abc");
            assert_eq!(store.cursors_count(), 1);
            // Flush to disk
            store.flush_to_disk().await.unwrap();
        }

        // Create a new store with the same path — should load from disk
        {
            let store = KfCursorStore::new(Some(path.clone()));
            assert_eq!(store.cursors_count(), 1);
            assert_eq!(store.get_cursor("kf001"), Some("cursor_abc".to_string()));
        }
    }

    #[tokio::test]
    async fn test_persist_empty_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kf_cursors.json");

        // Create store with persist path but no cursors set
        {
            let store = KfCursorStore::new(Some(path.clone()));
            assert_eq!(store.cursors_count(), 0);
            store.flush_to_disk().await.unwrap();
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
        assert_eq!(store.get_cursor("kf001"), Some("cursor_abc".to_string()));
    }

    #[test]
    fn test_cursor_for_different_open_kfid() {
        let store = KfCursorStore::new(None);
        store.set_cursor("kf001", "cursor_001");
        assert!(store.get_cursor("kf999").is_none());
    }

    #[tokio::test]
    async fn test_flush_to_disk_noop_without_persist_path() {
        let store = KfCursorStore::new(None);
        store.set_cursor("kf001", "cursor_abc");
        assert!(store.flush_to_disk().await.is_ok());
    }

    #[tokio::test]
    async fn test_flush_to_disk_not_dirty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kf_cursors.json");
        let store = KfCursorStore::new(Some(path.clone()));
        assert!(store.flush_to_disk().await.is_ok());
    }
}
