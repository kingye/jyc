//! The tool failures of a turn, and the ones the model recovered from.
//!
//! A tool that fails leaves the turn's outcome in doubt: the model may fix
//! the call and carry on — a wrong argument, a path that was not there — and
//! then the turn did what it was asked to do. Only a failure that *stands*
//! is worth reporting, so the topic's status follows what the turn left
//! behind instead of every stumble on the way.
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
/// The agent loop settles a turn through this, so which tool failures reach
/// the status at all — and in which order the status is touched — can be
/// pinned without a whole topic manager.
pub(super) trait TurnStatus {
    /// A tool failed and the model did not recover from it.
    fn tool_error(&self, topic: &str);
    /// The turn ended.
    fn turn_ended(&self, topic: &str, success: bool);
}

/// Writes a turn's outcome to the topic manager.
pub(super) struct ManagerStatus<'a>(pub(super) &'a TopicManager);

impl TurnStatus for ManagerStatus<'_> {
    fn tool_error(&self, topic: &str) {
        self.0.set_topic_tool_error_if_known(topic);
    }

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

/// Tell the status about the tool failures that stand.
///
/// A failure the model recovered from never reaches the status: the topic
/// is left as it was, and only the activity feed keeps the stumble.
pub(super) fn report_standing_failures(
    status: Option<&impl TurnStatus>,
    topic: &str,
    failures: &ToolFailures,
) {
    let standing = failures.unrecovered();
    if standing.is_empty() {
        return;
    }
    tracing::warn!(
        topic = %topic,
        tools = ?standing,
        "tool errors the turn did not recover from"
    );
    if let Some(status) = status {
        status.tool_error(topic);
    }
}

/// Settle the turn: the failures that stand are reported, then the turn
/// ends.
///
/// The report comes first: the inspect server publishes the live status
/// when it sees the turn's end, so a failure that landed after it would
/// reach the dashboard one event too late.
pub(super) fn settle_turn(
    status: Option<&impl TurnStatus>,
    topic: &str,
    failures: &ToolFailures,
    success: bool,
) {
    report_standing_failures(status, topic, failures);
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
