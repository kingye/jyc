//! The tool failures of a turn, and the ones the model recovered from.
//!
//! A tool call that fails is a step on the way, not the turn's outcome: the
//! model may fix the call and carry on — a wrong argument, a path that was
//! not there — or give up on that tool and reach the answer another way.
//! Either way the turn did what it was asked to do, so a tool failure never
//! writes the topic's status: the status follows the turn's own outcome, and
//! the failures that stand are a log line and the activity feed the tool
//! already left behind.
//!
//! A failure is recovered when the same tool is called again and works: the
//! model adjusted the call and the retry went through. Another tool's
//! success says nothing about it.
//!
//! The recovery is counted per tool, not per call: the arguments are not
//! part of the key, so another call of the same tool that works counts as a
//! recovery too — the model may have given up on the call that failed and
//! carried on with another one. Keying on the arguments instead would miss
//! the common case, where the model fixes the wrong argument and the retry
//! (a different call) works.

use std::collections::BTreeMap;
use std::future::Future;

use jyc_core::topic_manager::TopicManager;

use crate::tools::ToolOutput;

/// Where a turn's outcome is written.
///
/// The agent loop settles a turn through this, so what the status is told —
/// and in which order it is told — can be pinned without a whole topic
/// manager. The turn's own outcome is the only thing it carries: a tool that
/// failed along the way is not the turn failing.
pub(super) trait TurnStatus {
    /// The turn ended.
    fn turn_ended(&self, topic: &str, success: bool);
}

/// Writes a turn's outcome to the topic manager.
pub(super) struct ManagerStatus<'a>(pub(super) &'a TopicManager);

impl TurnStatus for ManagerStatus<'_> {
    fn turn_ended(&self, topic: &str, success: bool) {
        self.0.end_topic_turn_if_known(topic, success);
    }
}

/// The tools whose last call in this turn failed and was not retried
/// successfully.
#[derive(Debug, Default)]
pub(super) struct ToolFailures {
    /// The tools that stand in error, with the message their last failure
    /// carried.
    unrecovered: BTreeMap<String, String>,
}

impl ToolFailures {
    /// A tool call's outcome: a failure stands until that tool works again.
    pub(super) fn record(&mut self, tool: &str, output: &ToolOutput) {
        if output.is_error {
            self.unrecovered
                .insert(tool.to_string(), trim(&output.content));
        } else {
            self.unrecovered.remove(tool);
        }
    }

    /// The tools that stand in error.
    pub(super) fn unrecovered(&self) -> &BTreeMap<String, String> {
        &self.unrecovered
    }
}

/// Log the tool failures that stand.
///
/// The failures the model recovered from are already gone; the ones that
/// stand say a call never worked, which is worth a line in the log — the
/// tool's own failure is in the activity feed once, where it happened. The
/// topic's status is not told: it follows the turn, not the tools the turn
/// went through.
pub(super) fn log_standing_failures(topic: &str, failures: &ToolFailures) {
    let standing = failures.unrecovered();
    if standing.is_empty() {
        return;
    }
    tracing::warn!(
        topic = %topic,
        tools = ?standing,
        "tool calls failed and were not retried successfully"
    );
}

/// Settle the turn: the failures that stand are logged, then the turn ends.
///
/// The log comes first so the line lands before the turn's own end: reading
/// the log, the stumble is above the outcome it belongs to.
pub(super) fn settle_turn(
    status: Option<&impl TurnStatus>,
    topic: &str,
    failures: &ToolFailures,
    success: bool,
) {
    log_standing_failures(topic, failures);
    if let Some(status) = status {
        status.turn_ended(topic, success);
    }
}

/// Settle the turn and announce its end.
///
/// The two belong together: every path that ends a turn settles the status
/// and says so, and the order between them is the one
/// [`settle_turn`] documents. The announcement is a closure so that the
/// order can be pinned without a whole event bus.
pub(super) async fn finish_turn<F, Fut>(
    status: Option<&impl TurnStatus>,
    topic: &str,
    failures: &ToolFailures,
    success: bool,
    announce: F,
) where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    settle_turn(status, topic, failures, success);
    announce().await;
}

/// The failure's message, trimmed — the log line wants to name the call,
/// not carry its whole output.
fn trim(text: &str) -> String {
    const MAX: usize = 200;
    let text = text.trim();
    match text.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}
