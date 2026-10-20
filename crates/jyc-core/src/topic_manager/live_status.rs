//! Per-topic live status — the single source of truth for `TopicStatus`.
//!
//! A topic's status is written by the runtime as its turns start and end
//! (the agent loop, the queue's error path) and read by everything that
//! reports it: `/info`, the dashboard's topic table, the info pane. One
//! writer, one reader — no consumer computes a status of its own.
//!
//! The values mirror what the topic's events say: a turn in flight is
//! `Processing`, a turn that failed — or a tool that failed during it — is
//! `Error`, anything else is `Idle`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use jyc_types::TopicStatus;

use super::TopicManager;

/// A topic's live status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// A turn is in flight.
    pub is_processing: bool,
    /// The last turn failed, or a tool failed during it.
    ///
    /// Cleared when the next turn starts — a topic that failed once and
    /// then took a new message is working again, not still broken.
    pub has_error: bool,
}

impl LiveStatus {
    /// The status every consumer sees.
    pub fn status(&self) -> TopicStatus {
        if self.is_processing {
            TopicStatus::Processing
        } else if self.has_error {
            TopicStatus::Error
        } else {
            TopicStatus::Idle
        }
    }
}

/// The live statuses of one channel's topics.
///
/// Shared by the workers that write it and the readers that report it
/// (see [`TopicManager::topic_live_status`]).
#[derive(Debug, Default)]
pub struct LiveStatusStore {
    statuses: Mutex<HashMap<String, LiveStatus>>,
}

impl LiveStatusStore {
    /// The topic's status; `Idle` for a topic that has never run.
    pub fn get(&self, topic: &str) -> LiveStatus {
        self.statuses().get(topic).copied().unwrap_or_default()
    }

    /// A turn started: the topic is processing, and whatever went wrong
    /// before it is over.
    pub fn set_processing(&self, topic: &str) {
        self.statuses().insert(
            topic.to_string(),
            LiveStatus {
                is_processing: true,
                has_error: false,
            },
        );
    }

    /// The turn ended. A turn that did not succeed leaves the topic in
    /// error; one that did keeps the error a failed tool left behind, so
    /// the topic still reports what went wrong.
    pub fn set_turn_ended(&self, topic: &str, success: bool) {
        let mut statuses = self.statuses();
        let entry = statuses.entry(topic.to_string()).or_default();
        entry.is_processing = false;
        if !success {
            entry.has_error = true;
        }
    }

    /// The turn ended, for a topic the store still knows.
    ///
    /// A topic that was closed mid-turn has no entry any more — the close
    /// clears it — and re-creating it here would report the topic as failed
    /// until its next turn, and a name reused later would inherit that
    /// failure. A topic that never ran has no entry to fail in the first
    /// place.
    pub fn end_turn_if_known(&self, topic: &str, success: bool) {
        let mut statuses = self.statuses();
        if let Some(entry) = statuses.get_mut(topic) {
            entry.is_processing = false;
            if !success {
                entry.has_error = true;
            }
        }
    }

    /// A tool failed during the turn.
    pub fn set_tool_error(&self, topic: &str) {
        let mut statuses = self.statuses();
        statuses.entry(topic.to_string()).or_default().has_error = true;
    }

    /// Forget the topic's status — it is gone, or its turn ended without
    /// a word.
    pub fn clear(&self, topic: &str) {
        self.statuses().remove(topic);
    }

    fn statuses(&self) -> MutexGuard<'_, HashMap<String, LiveStatus>> {
        // A poisoned lock means a panic while the map was held; the map
        // itself is still consistent, so take the data anyway.
        self.statuses.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TopicManager {
    /// The topic's live status — the value every consumer of `TopicStatus`
    /// reads.
    pub fn topic_live_status(&self, topic_name: &str) -> LiveStatus {
        self.live_status.get(topic_name)
    }

    /// The topic's status, as the consumers report it.
    pub fn topic_status(&self, topic_name: &str) -> TopicStatus {
        self.topic_live_status(topic_name).status()
    }

    /// A turn started on this topic.
    pub fn set_topic_processing(&self, topic_name: &str) {
        self.live_status.set_processing(topic_name);
    }

    /// The topic's turn ended — `success` says whether it went through.
    pub fn set_topic_turn_ended(&self, topic_name: &str, success: bool) {
        self.live_status.set_turn_ended(topic_name, success);
    }

    /// A tool failed during the topic's turn.
    pub fn set_topic_tool_error(&self, topic_name: &str) {
        self.live_status.set_tool_error(topic_name);
    }
}

/// Closes a topic's "processing" flag when a turn ends without saying so.
///
/// A turn that ends normally closes itself — the agent loop publishes the
/// event the status mirrors, and the status follows it. A turn that is cut
/// short (the worker panics, the topic is closed mid-turn) publishes
/// nothing, and the topic would keep reporting "Processing" forever. This
/// guard drops on every path out of the turn and closes the flag on the
/// ones the turn did not close itself.
pub(crate) struct TurnStatusGuard {
    store: Arc<LiveStatusStore>,
    topic_name: String,
    closed: bool,
}

impl TurnStatusGuard {
    pub(crate) fn new(store: Arc<LiveStatusStore>, topic_name: String) -> Self {
        Self {
            store,
            topic_name,
            closed: false,
        }
    }

    /// The turn closed itself — nothing left to clean up.
    pub(crate) fn disarm(&mut self) {
        self.closed = true;
    }
}

impl Drop for TurnStatusGuard {
    fn drop(&mut self) {
        if !self.closed {
            // Only for a topic the store still knows: a topic closed while
            // the turn was in flight has no entry any more, and writing one
            // here would report it as failed (see `end_turn_if_known`).
            self.store.end_turn_if_known(&self.topic_name, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<LiveStatusStore> {
        Arc::new(LiveStatusStore::default())
    }

    /// The guard is what keeps "Processing" from outliving the turn: a turn
    /// that ends without saying so still closes its own flag.
    #[test]
    fn the_guard_closes_the_flag_when_the_turn_does_not() {
        let store = store();
        store.set_processing("t");
        {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
        }
        assert!(!store.get("t").is_processing, "the flag stayed open");
        assert_eq!(store.get("t").status(), TopicStatus::Error);
    }

    /// A turn that closed its own status leaves the guard nothing to do.
    #[test]
    fn a_disarmed_guard_changes_nothing() {
        let store = store();
        store.set_processing("t");
        {
            let mut guard = TurnStatusGuard::new(store.clone(), "t".to_string());
            guard.disarm();
        }
        assert_eq!(store.get("t").status(), TopicStatus::Processing);
    }

    /// A topic closed mid-turn has no entry any more. The guard must not
    /// bring it back as a failed topic — a name reused later would report
    /// the failure of the topic that was closed.
    #[test]
    fn the_guard_does_not_revive_a_closed_topic() {
        let store = store();
        store.set_processing("t");
        store.clear("t");
        {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
        }
        let statuses = store.statuses.lock().unwrap();
        assert!(
            !statuses.contains_key("t"),
            "the closed topic came back as a failed one"
        );
    }

    /// A turn that panics never closes its own status — the guard's drop on
    /// the way out is the only thing that closes it.
    #[test]
    fn a_panicking_turn_still_closes_the_flag() {
        let store = store();
        store.set_processing("t");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
            panic!("the turn blew up");
        }));
        assert!(result.is_err(), "the panic must not be swallowed");
        assert!(!store.get("t").is_processing, "the flag stayed open");
    }
}
