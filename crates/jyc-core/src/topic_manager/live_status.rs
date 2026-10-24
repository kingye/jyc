//! Per-topic live status — the single source of truth for `TopicStatus`.
//!
//! A topic's status is written by the runtime as its turns start and end
//! (the agent loop, the queue's error path) and read by everything that
//! reports it: `/info`, the dashboard's topic table, the info pane. One
//! writer, one reader — no consumer computes a status of its own.
//!
//! The values mirror what the topic's events say: a turn in flight is
//! `Processing`, a turn that failed — or a tool that failed during it and
//! was never retried successfully — is `Error`, anything else is `Idle`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use jyc_types::TopicStatus;

use super::TopicManager;

/// The messages a queue holds: its capacity minus the room it has left.
///
/// Tokio's own numbers — the sender knows how much room the channel has
/// left, and how much room it had at most — so the count cannot drift
/// from the channel the way a counter of our own would.
///
/// A channel whose receiver is gone reports no depth: dropping the
/// receiver drains what it held and hands the permits back, so a queue
/// nobody reads counts as empty rather than as full of messages that
/// were thrown away with it.
fn queue_depth<T>(tx: &tokio::sync::mpsc::Sender<T>) -> usize {
    tx.max_capacity() - tx.capacity()
}

/// A topic's live status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveStatus {
    /// A turn is in flight.
    pub is_processing: bool,
    /// The last turn failed, or a tool failed during it and the model did
    /// not recover from it.
    ///
    /// Cleared when the next turn starts — a topic that failed once and
    /// then took a new message is working again, not still broken.
    pub has_error: bool,
    /// The messages the running turn pulled aside for a turn of their
    /// own: the ones that arrived while it was working and could not
    /// join it.
    ///
    /// The queue's own backlog — the messages still waiting in the
    /// channel — is counted by the channel itself; this half is the part
    /// the channel cannot see.
    pub queued: usize,
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
    ///
    /// The write touches the two status flags and leaves the backlog
    /// alone: a turn that starts finds the queue as it was.
    ///
    /// The backlog belongs to the running turn, and the turn's first poll
    /// can land after the worker already pulled a message aside for it —
    /// clearing the count here would hide that message for the whole
    /// first call. The count is ended with the turn instead (see
    /// [`TurnStatusGuard`]).
    pub fn set_processing(&self, topic: &str) {
        let mut statuses = self.statuses();
        let entry = statuses.entry(topic.to_string()).or_default();
        entry.is_processing = true;
        entry.has_error = false;
    }

    /// The running turn's own backlog: the messages it pulled aside for a
    /// turn of their own.
    ///
    /// The write is an overwrite, not a counter: the turn knows how many
    /// messages it holds, so it says so — a counter would have to be
    /// right about every way a message leaves the buffer, and a drift
    /// there would show a topic a backlog it does not have.
    pub fn set_queued(&self, topic: &str, queued: usize) {
        let mut statuses = self.statuses();
        let entry = statuses.entry(topic.to_string()).or_default();
        entry.queued = queued;
    }

    /// The turn died with the messages it held: they went with it.
    ///
    /// The write follows the "if known" rule: a topic closed mid-turn has
    /// no entry any more — the close clears it — and re-creating it here
    /// would report a closed topic as waiting for a turn it never gets.
    pub fn clear_queued_if_known(&self, topic: &str) {
        let mut statuses = self.statuses();
        if let Some(entry) = statuses.get_mut(topic) {
            entry.queued = 0;
        }
    }

    /// The turn ended. A turn that did not succeed leaves the topic in
    /// error; one that did keeps the error an unrecovered tool failure left
    /// behind, so the topic still reports what went wrong.
    ///
    /// The write follows the "if known" rule: a topic that was closed
    /// mid-turn has no entry any more — the close clears it — and
    /// re-creating it here would report the topic as failed until its next
    /// turn, and a name reused later would inherit that failure. A topic
    /// that never ran has no entry to end in the first place.
    pub fn end_turn_if_known(&self, topic: &str, success: bool) {
        let mut statuses = self.statuses();
        if let Some(entry) = statuses.get_mut(topic) {
            entry.is_processing = false;
            if !success {
                entry.has_error = true;
            }
        }
    }

    /// A tool failed during the turn and the model did not recover from it.
    ///
    /// The write follows the "if known" rule: a topic that was closed
    /// mid-turn has no entry any more — the close clears it — and
    /// re-creating it here would report a closed topic as failed, and a
    /// name reused later would inherit that failure.
    pub fn set_tool_error_if_known(&self, topic: &str) {
        let mut statuses = self.statuses();
        if let Some(entry) = statuses.get_mut(topic) {
            entry.has_error = true;
        }
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
    ///
    /// A topic the store does not know — one that was closed mid-turn — is
    /// left alone: the turn's end says nothing about a topic that is gone.
    pub fn end_topic_turn_if_known(&self, topic_name: &str, success: bool) {
        self.live_status.end_turn_if_known(topic_name, success);
    }

    /// A tool failed during the topic's turn and the model did not recover
    /// from it.
    ///
    /// A topic the store does not know — one that was closed mid-turn — is
    /// left alone: the report says nothing about a topic that is gone.
    pub fn set_topic_tool_error_if_known(&self, topic_name: &str) {
        self.live_status.set_tool_error_if_known(topic_name);
    }

    /// The backlog of every topic of this channel, read under one lock.
    ///
    /// The channel's queue depths for every topic at once: the messages
    /// each topic's queue holds, waiting for a turn of their own. The
    /// other half of the backlog — the messages the running turn pulled
    /// aside — is the store's `queued`, read per topic.
    ///
    /// One lock for the whole channel: the overview asks for every topic
    /// at once, and a lock per topic would serialize the poll it is
    /// answering.
    ///
    /// The two halves are read at different moments — the depths here, the
    /// store's per topic — so a poll that lands while a turn hands its
    /// messages back to the queue can count one message twice or not at
    /// all. The gap is one message and one poll wide, and the next poll
    /// has it right.
    pub(crate) async fn channel_queued(&self) -> HashMap<String, usize> {
        let queues = self.topic_queues.lock().await;
        queues
            .iter()
            .map(|(name, tx)| (name.clone(), queue_depth(tx)))
            .collect()
    }

    /// The running turn's own backlog — the messages it holds for a turn
    /// of their own.
    pub(crate) fn set_topic_queued(&self, topic_name: &str, queued: usize) {
        self.live_status.set_queued(topic_name, queued);
    }
}

/// Closes a topic's "processing" flag when a turn ends without saying so.
///
/// A turn that ends normally closes itself — the agent loop publishes the
/// event the status mirrors, and the status follows it. A turn that is cut
/// short — the worker panics, the turn dies without saying so — publishes
/// nothing, and the topic would keep reporting "Processing" forever. This
/// guard drops on every path out of the turn and closes the flag on the
/// ones the turn did not close itself.
///
/// The guard also ends the turn's backlog. A turn that ended on its own
/// gave its messages back to the queue and said so; a turn that died kept
/// them — they died with it. Either way the turn holds none when the guard
/// drops, and a count that outlived it would be a backlog the topic does
/// not have, inherited by the next turn.
///
/// The guard's writes name the topic, not the turn: a topic closed and
/// reopened under this guard can have the new turn's count cleared by this
/// one. The window is the guard's own drop — nothing awaits between the
/// two writes — and the new turn rewrites the count the next time it takes
/// a message in.
pub(crate) struct TurnStatusGuard {
    store: Arc<LiveStatusStore>,
    topic_name: String,
}

impl TurnStatusGuard {
    pub(crate) fn new(store: Arc<LiveStatusStore>, topic_name: String) -> Self {
        Self { store, topic_name }
    }
}

impl Drop for TurnStatusGuard {
    fn drop(&mut self) {
        // Only a turn that left the flag open: a turn that ended on its own
        // closed it, and a topic closed mid-turn has no entry to close. A
        // turn that died without saying so (a panic) leaves the flag open —
        // close it as failed.
        if self.store.get(&self.topic_name).is_processing {
            // Only for a topic the store still knows: a topic closed while
            // the turn was in flight has no entry any more, and writing one
            // here would report it as failed (see `end_turn_if_known`).
            self.store.end_turn_if_known(&self.topic_name, false);
        }
        // The turn holds no messages any more. The write follows the "if
        // known" rule: a topic closed mid-turn has no entry, and one made
        // here would report a closed topic as waiting for a turn it never
        // gets.
        self.store.clear_queued_if_known(&self.topic_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<LiveStatusStore> {
        Arc::new(LiveStatusStore::default())
    }

    /// The queue's depth is the channel's own count: the messages it
    /// holds, not the room it has left.
    #[test]
    fn the_queue_depth_counts_the_messages_the_channel_holds() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u8>(4);
        assert_eq!(queue_depth(&tx), 0);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(queue_depth(&tx), 2, "the two messages in the channel");
        tx.try_send(3).unwrap();
        tx.try_send(4).unwrap();
        assert_eq!(queue_depth(&tx), 4, "the channel is full");
        assert!(tx.try_send(5).is_err(), "a full channel takes no more");
    }

    /// The turn's start does not touch the backlog: the count belongs to
    /// the running turn, and the turn's first poll can land after the
    /// worker already pulled a message aside for it.
    #[test]
    fn the_turn_start_leaves_the_backlog_alone() {
        let store = store();
        store.set_queued("t", 3);
        assert_eq!(store.get("t").queued, 3);
        store.set_processing("t");
        assert_eq!(
            store.get("t").queued,
            3,
            "the turn's start wiped the backlog"
        );
        store.set_queued("t", 0);
        assert_eq!(store.get("t").queued, 0, "the turn let its backlog go");
    }

    /// A turn that dies without saying so takes its backlog with it: the
    /// messages it held went with the task, so the count must not outlive
    /// them — a topic would report a backlog it does not have, and the
    /// next turn would inherit it.
    #[test]
    fn a_turn_that_dies_leaves_no_backlog_behind() {
        let store = store();
        store.set_queued("t", 3);
        store.set_processing("t");
        {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
        }
        assert_eq!(
            store.get("t").queued,
            0,
            "the dead turn's backlog is still counted"
        );
    }

    /// The guard ends the backlog of a turn that ended on its own too: the
    /// turn gave its messages back to the queue, so it holds none, and the
    /// count must not outlive it.
    #[test]
    fn a_turn_that_ended_leaves_no_backlog_behind() {
        let store = store();
        store.set_queued("t", 3);
        store.set_processing("t");
        store.end_turn_if_known("t", true);
        {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
        }
        assert_eq!(
            store.get("t").queued,
            0,
            "the finished turn's backlog is still counted"
        );
    }

    /// A closed topic has no backlog: the close clears the entry, and a
    /// name reused later starts empty.
    #[test]
    fn a_closed_topic_has_no_backlog() {
        let store = store();
        store.set_queued("t", 3);
        store.clear("t");
        assert_eq!(store.get("t").queued, 0);
        assert!(!store.get("t").is_processing, "the close cleared it");
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
    fn a_guard_over_a_closed_turn_changes_nothing() {
        let store = store();
        store.set_processing("t");
        store.end_turn_if_known("t", true);
        {
            let _guard = TurnStatusGuard::new(store.clone(), "t".to_string());
        }
        assert_eq!(store.get("t").status(), TopicStatus::Idle);
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

    /// The turn's end follows the "if known" rule too: a topic closed
    /// mid-turn stays gone, and a failed turn does not bring it back.
    #[test]
    fn a_failed_turn_does_not_revive_a_closed_topic() {
        let store = store();
        store.set_processing("t");
        store.clear("t");
        store.end_turn_if_known("t", false);
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
