//! In-process registry connecting the `ask_user` tool with channel answers.
//!
//! [`QuestionHub`] tracks questions currently awaiting a user answer: the
//! `ask_user` tool registers a oneshot receiver and blocks on it, while the
//! channel that rendered the question submits answers via
//! [`QuestionHub::respond`] — the websocket client's question box sends one
//! `question_response` frame per question. Giving up on a whole set is a
//! different event and not an answer: [`QuestionHub::abort_topic`] settles
//! every pending question of that topic as
//! [`QuestionReply::Discarded`](jyc_types::channel::QuestionReply::Discarded)
//! so the blocked call can stop the run at once. Channels whose outbound
//! cannot render a question register none at all (the tool reports that to the
//! model). Entries are keyed by question id (UUID), so a single hub serves all
//! channels and topics.

use std::collections::HashMap;
use std::sync::Mutex;

use jyc_types::channel::{QuestionAnswer, QuestionReply};

/// One question waiting for its answer.
struct PendingEntry {
    /// Topic that asked the question, so discarding a batch can find it.
    topic: String,
    /// Channel back to the blocked `ask_user` tool call.
    sender: tokio::sync::oneshot::Sender<QuestionReply>,
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
        sender: tokio::sync::oneshot::Sender<QuestionReply>,
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

    /// Submit the user's answer to one question. Returns `false` when the id
    /// is unknown or the asker is already gone (timed out / cancelled) — late
    /// answers are logged and ignored by callers.
    pub fn respond(&self, id: &str, answer: QuestionAnswer) -> bool {
        let Some(entry) = self.pending.lock().unwrap().remove(id) else {
            return false;
        };
        entry.sender.send(QuestionReply::Answer(answer)).is_ok()
    }

    /// Discard every pending question of `topic` — a client giving up on the
    /// set (Esc on the TUI's question box), which says nothing about any
    /// single question and stops the run instead of answering one.
    ///
    /// Only one `ask_user` call can be in flight per topic, so the questions
    /// found here are exactly the batch the user is looking at. Returns how
    /// many were settled.
    pub fn abort_topic(&self, topic: &str) -> usize {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, entry)| entry.topic == topic)
            .map(|(id, _)| id.clone())
            .collect();
        let mut settled = 0;
        for id in ids {
            if let Some(entry) = pending.remove(&id)
                && entry.sender.send(QuestionReply::Discarded).is_ok()
            {
                settled += 1;
            }
        }
        settled
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

        assert!(hub.respond("q1", QuestionAnswer::Choice(vec!["yes".into()])));
        assert_eq!(
            rx.blocking_recv(),
            Ok(QuestionReply::Answer(QuestionAnswer::Choice(vec![
                "yes".into()
            ])))
        );
    }

    #[test]
    fn respond_delivers_decline() {
        let hub = QuestionHub::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _guard = hub.register("q1", "topic-a", tx);

        assert!(hub.respond("q1", QuestionAnswer::Declined));
        assert_eq!(
            rx.blocking_recv(),
            Ok(QuestionReply::Answer(QuestionAnswer::Declined))
        );
    }

    #[test]
    fn respond_unknown_id_returns_false() {
        let hub = QuestionHub::new();
        assert!(!hub.respond("nope", QuestionAnswer::Declined));
    }

    #[test]
    fn respond_twice_second_is_false() {
        let hub = QuestionHub::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _guard = hub.register("q1", "topic-a", tx);
        assert!(hub.respond("q1", QuestionAnswer::Declined));
        assert!(!hub.respond("q1", QuestionAnswer::Declined));
    }

    #[test]
    fn guard_drop_removes_entry() {
        let hub = QuestionHub::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let guard = hub.register("q1", "topic-a", tx);
        drop(guard);
        // Gone: a late answer finds nothing, and an abort settles zero.
        assert!(!hub.respond("q1", QuestionAnswer::Declined));
        assert_eq!(hub.abort_topic("topic-a"), 0);
    }

    #[test]
    fn dropped_receiver_makes_respond_false() {
        let hub = QuestionHub::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let guard = hub.register("q1", "topic-a", tx);
        drop(rx); // asker gone (e.g. tool future dropped by cancellation)
        drop(guard);
        // Entry removed by guard; even without it, send would fail.
        assert!(!hub.respond("q1", QuestionAnswer::Declined));
    }

    /// One `ask_user` call leaves several questions pending for the same
    /// topic. Esc discards all of them at once — answering none, and leaving
    /// a question of another topic untouched.
    #[test]
    fn abort_topic_discards_every_question_of_that_topic() {
        let hub = QuestionHub::new();
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        let (tx3, mut rx3) = tokio::sync::oneshot::channel();
        let _g1 = hub.register("q1", "topic-a", tx1);
        let _g2 = hub.register("q2", "topic-a", tx2);
        let _g3 = hub.register("q3", "topic-b", tx3);

        assert_eq!(hub.abort_topic("topic-a"), 2);
        assert_eq!(rx1.blocking_recv(), Ok(QuestionReply::Discarded));
        assert_eq!(rx2.blocking_recv(), Ok(QuestionReply::Discarded));

        // Consumed: a late answer to a discarded question goes nowhere.
        assert!(!hub.respond("q1", QuestionAnswer::Choice(vec!["1".into()])));
        // The other topic's question still waits for its own answer.
        assert_eq!(
            rx3.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        assert_eq!(hub.abort_topic("topic-a"), 0);
    }
}
