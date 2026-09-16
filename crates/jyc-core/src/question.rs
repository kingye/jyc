//! In-process registry connecting the `ask_user` tool with channel answers.
//!
//! [`QuestionHub`] tracks questions currently awaiting a user answer: the
//! `ask_user` tool registers a oneshot receiver and blocks on it, while
//! channel inbound adapters (websocket today; feishu/wecom cards later)
//! submit answers via [`QuestionHub::respond`]. Entries are keyed by question
//! id (UUID), so a single hub serves all channels and topics.

use std::collections::HashMap;
use std::sync::Mutex;

use jyc_types::channel::QuestionAnswer;

/// One question waiting for its answer.
struct PendingEntry {
    /// Topic that asked the question (for text-fallback routing).
    topic: String,
    /// Channel back to the blocked `ask_user` tool call.
    sender: tokio::sync::oneshot::Sender<QuestionAnswer>,
}

/// Registry of questions currently awaiting a user answer.
#[derive(Default)]
pub struct QuestionHub {
    pending: Mutex<HashMap<String, PendingEntry>>,
}

impl QuestionHub {
    /// Create an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending question and return a guard for it.
    ///
    /// The guard removes the entry when dropped, covering tool timeout,
    /// agent cancellation, and send failures, so stale entries never
    /// accumulate. Dropping the guard after [`QuestionHub::respond`] has
    /// already consumed the entry is a no-op.
    pub fn register(
        &self,
        id: &str,
        topic: &str,
        sender: tokio::sync::oneshot::Sender<QuestionAnswer>,
    ) -> PendingGuard<'_> {
        self.pending.lock().unwrap().insert(
            id.to_string(),
            PendingEntry {
                topic: topic.to_string(),
                sender,
            },
        );
        PendingGuard {
            hub: self,
            id: id.to_string(),
        }
    }

    /// Submit the user's answer. Returns `false` when the id is unknown or
    /// the asker is already gone (timed out / cancelled) — late answers are
    /// logged and ignored by callers.
    pub fn respond(&self, id: &str, answer: QuestionAnswer) -> bool {
        let Some(entry) = self.pending.lock().unwrap().remove(id) else {
            return false;
        };
        entry.sender.send(answer).is_ok()
    }

    /// Id of a pending question for `topic`, if any.
    ///
    /// Used by text-fallback channels (email, github) to route a plain user
    /// reply to the question while one is outstanding for that topic.
    pub fn pending_for(&self, topic: &str) -> Option<String> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .find(|(_, e)| e.topic == topic)
            .map(|(id, _)| id.clone())
    }
}

/// Removes the hub entry for a registered question on drop.
pub struct PendingGuard<'a> {
    hub: &'a QuestionHub,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.hub.pending.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respond_delivers_choice() {
        let hub = QuestionHub::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _guard = hub.register("q1", "topic-a", tx);

        assert!(hub.respond("q1", QuestionAnswer::Choice("yes".into())));
        assert_eq!(rx.blocking_recv(), Ok(QuestionAnswer::Choice("yes".into())));
    }

    #[test]
    fn respond_unknown_id_returns_false() {
        let hub = QuestionHub::new();
        assert!(!hub.respond("nope", QuestionAnswer::Cancelled));
    }

    #[test]
    fn respond_twice_second_is_false() {
        let hub = QuestionHub::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _guard = hub.register("q1", "topic-a", tx);
        assert!(hub.respond("q1", QuestionAnswer::Cancelled));
        assert!(!hub.respond("q1", QuestionAnswer::Cancelled));
    }

    #[test]
    fn guard_drop_removes_entry() {
        let hub = QuestionHub::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let guard = hub.register("q1", "topic-a", tx);
        drop(guard);
        // Gone: respond finds nothing, pending_for too.
        assert!(!hub.respond("q1", QuestionAnswer::Cancelled));
        assert_eq!(hub.pending_for("topic-a"), None);
    }

    #[test]
    fn pending_for_finds_by_topic() {
        let hub = QuestionHub::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _guard = hub.register("q1", "topic-a", tx);
        assert_eq!(hub.pending_for("topic-a"), Some("q1".to_string()));
        assert_eq!(hub.pending_for("topic-b"), None);
    }

    #[test]
    fn dropped_receiver_makes_respond_false() {
        let hub = QuestionHub::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let guard = hub.register("q1", "topic-a", tx);
        drop(rx); // asker gone (e.g. tool future dropped by cancellation)
        drop(guard);
        // Entry removed by guard; even without it, send would fail.
        assert!(!hub.respond("q1", QuestionAnswer::Cancelled));
    }
}
