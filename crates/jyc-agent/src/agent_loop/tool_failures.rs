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

use std::collections::HashSet;

use jyc_core::topic_manager::TopicManager;

/// The tools whose last call in this turn failed and was not retried
/// successfully.
#[derive(Debug, Default)]
pub(super) struct ToolFailures {
    /// The tools that stand in error.
    unrecovered: HashSet<String>,
}

impl ToolFailures {
    /// A tool call's outcome: a failure stands until that tool works again.
    pub(super) fn record(&mut self, tool: &str, is_error: bool) {
        if is_error {
            self.unrecovered.insert(tool.to_string());
        } else {
            self.unrecovered.remove(tool);
        }
    }

    /// The tools that stand in error.
    pub(super) fn unrecovered(&self) -> &HashSet<String> {
        &self.unrecovered
    }

    /// Tell the topic's status about the failures that stand: the topic is
    /// left in error when a tool never worked, and says nothing when the
    /// model recovered from it.
    ///
    /// The report happens once, at the end of the turn — a failure the model
    /// recovered from in between never reaches the status at all.
    pub(super) fn report(&self, topic: &str, tm: Option<&TopicManager>) {
        let unrecovered = self.unrecovered();
        if unrecovered.is_empty() {
            return;
        }
        tracing::warn!(
            topic = %topic,
            tools = ?unrecovered,
            "tool errors the turn did not recover from"
        );
        if let Some(tm) = tm {
            tm.set_topic_tool_error(topic);
        }
    }
}
