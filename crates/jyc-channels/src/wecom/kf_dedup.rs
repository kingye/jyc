//! Deduplication store for WeCom KF (Customer Service) messages.
//!
//! The KF `sync_msg` API may return overlapping messages across syncs.
//! Dedup by `msgid` prevents duplicate processing of the same message.
//!
//! Uses an in-memory `HashSet` with a maximum of 10,000 entries.
//! Older entries are evicted when the limit is exceeded (FIFO).

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

/// Maximum number of seen message IDs to keep in memory.
const MAX_ENTRIES: usize = 10_000;

type DedupState = (HashSet<String>, VecDeque<String>);

/// In-memory deduplication store for KF message IDs.
///
/// Tracks seen `msgid` values in a `HashSet` with a FIFO eviction policy
/// when the number of entries exceeds `MAX_ENTRIES`. Both the set and the
/// insertion-order queue are held under a single `Mutex` for atomicity.
///
/// Memory-only: persistent dedup is not needed since the cursor store
/// prevents most re-syncs. The dedup store catches any overlap within
/// a single batch or across edge cases.
pub struct KfDedupStore {
    state: Mutex<DedupState>,
}

impl KfDedupStore {
    /// Create a new empty dedup store.
    pub fn new() -> Self {
        Self {
            state: Mutex::new((HashSet::new(), VecDeque::new())),
        }
    }

    /// Check if a message ID has been seen before.
    ///
    /// Returns `true` if the message ID is a duplicate (already seen).
    pub fn is_duplicate(&self, msgid: &str) -> bool {
        self.state
            .lock()
            .map(|guard| guard.0.contains(msgid))
            .unwrap_or(false)
    }

    /// Mark a message ID as seen.
    ///
    /// If the store has reached `MAX_ENTRIES`, the oldest entry is evicted.
    pub fn mark_seen(&self, msgid: &str) {
        if let Ok(mut guard) = self.state.lock() {
            let (ref mut seen, ref mut order) = *guard;

            if seen.len() >= MAX_ENTRIES
                && let Some(oldest) = order.pop_front()
            {
                seen.remove(&oldest);
            }

            if seen.insert(msgid.to_string()) {
                order.push_back(msgid.to_string());
            }
        }
    }

    /// Get the current number of entries (for testing).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().map(|g| g.0.len()).unwrap_or(0)
    }
}

impl Default for KfDedupStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_store_empty() {
        let store = KfDedupStore::new();
        assert!(!store.is_duplicate("msg_001"));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_mark_and_check_duplicate() {
        let store = KfDedupStore::new();
        assert!(!store.is_duplicate("msg_001"));

        store.mark_seen("msg_001");
        assert!(store.is_duplicate("msg_001"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_multiple_messages() {
        let store = KfDedupStore::new();
        store.mark_seen("msg_001");
        store.mark_seen("msg_002");
        store.mark_seen("msg_003");

        assert!(store.is_duplicate("msg_001"));
        assert!(store.is_duplicate("msg_002"));
        assert!(store.is_duplicate("msg_003"));
        assert!(!store.is_duplicate("msg_004"));
        assert!(store.len() == 3);
    }

    #[test]
    fn test_duplicate_mark_is_idempotent() {
        let store = KfDedupStore::new();
        store.mark_seen("msg_001");
        store.mark_seen("msg_001"); // mark again
        assert_eq!(store.len(), 1); // still 1
    }

    #[test]
    fn test_eviction_when_max_entries_exceeded() {
        let store = KfDedupStore::new();

        // Add MAX_ENTRIES + 1 items
        for i in 0..=MAX_ENTRIES {
            store.mark_seen(&format!("msg_{:05}", i));
        }

        // The first item should be evicted
        assert!(!store.is_duplicate("msg_00000"));
        // But later items should still be present
        assert!(store.is_duplicate("msg_00001"));
        assert!(store.is_duplicate(&format!("msg_{:05}", MAX_ENTRIES)));

        // Size should be capped at MAX_ENTRIES
        assert_eq!(store.len(), MAX_ENTRIES);
    }

    #[test]
    fn test_eviction_keeps_most_recent() {
        let store = KfDedupStore::new();

        // Fill up to MAX_ENTRIES
        for i in 0..MAX_ENTRIES {
            store.mark_seen(&format!("msg_{:05}", i));
        }
        assert_eq!(store.len(), MAX_ENTRIES);

        // Add one more — evicts the first one
        store.mark_seen("msg_final");
        assert_eq!(store.len(), MAX_ENTRIES);
        assert!(!store.is_duplicate("msg_00000"));
        assert!(store.is_duplicate("msg_final"));
    }

    #[test]
    fn test_new_instance_does_not_share_state() {
        let store1 = KfDedupStore::new();
        store1.mark_seen("msg_001");

        let store2 = KfDedupStore::new();
        assert!(!store2.is_duplicate("msg_001")); // separate instance
    }
}
