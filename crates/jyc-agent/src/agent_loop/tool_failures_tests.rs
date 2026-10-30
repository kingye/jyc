use std::cell::RefCell;
use std::rc::Rc;

use crate::tools::ToolOutput;

use super::tool_failures::{ToolFailures, TurnStatus, finish_turn, settle_turn};

#[test]
fn a_failed_tool_stands() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));
    assert!(
        failures.unrecovered().contains_key("edit"),
        "the tool stands in error"
    );
}

#[test]
fn a_retry_that_works_recovers_the_failure() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));
    failures.record("edit", &ToolOutput::success("edited"));
    assert!(
        failures.unrecovered().is_empty(),
        "the retry recovered the failure, so the turn has nothing to report"
    );
}

#[test]
fn another_tools_success_does_not_recover_a_failure() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));
    failures.record("bash", &ToolOutput::success("ok"));
    assert!(
        failures.unrecovered().contains_key("edit"),
        "bash working says nothing about edit"
    );
}

#[test]
fn each_tool_stands_on_its_own() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));
    failures.record("bash", &ToolOutput::error("exit 1"));
    failures.record("edit", &ToolOutput::success("edited"));
    assert!(
        failures.unrecovered().contains_key("bash"),
        "bash never worked"
    );
    assert!(
        !failures.unrecovered().contains_key("edit"),
        "edit was retried successfully"
    );
}

/// The status the turn's outcome was written to.
#[derive(Default)]
struct Recorded {
    calls: Rc<RefCell<Vec<String>>>,
}

impl TurnStatus for Recorded {
    fn turn_ended(&self, topic: &str, success: bool) {
        self.calls
            .borrow_mut()
            .push(format!("turn_ended({topic}, {success})"));
    }
}

impl Recorded {
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

#[test]
fn a_recovered_failure_never_reaches_the_status() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));
    failures.record("edit", &ToolOutput::success("edited"));

    let status = Recorded::default();
    settle_turn(Some(&status), "t", &failures, true);

    assert_eq!(
        status.calls(),
        vec!["turn_ended(t, true)".to_string()],
        "a failure the model recovered from says nothing to the status"
    );
}

/// A tool that failed and was never retried is not the turn failing: the
/// model may have reached the answer another way, so the status hears the
/// turn's own outcome and nothing else.
#[test]
fn a_standing_failure_writes_only_the_turns_end() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));

    let status = Recorded::default();
    settle_turn(Some(&status), "t", &failures, true);

    assert_eq!(
        status.calls(),
        vec!["turn_ended(t, true)".to_string()],
        "a tool that failed along the way does not put the topic in error"
    );
}

/// The turn is settled before it is announced.
///
/// The inspect server publishes the live status when it sees the turn's
/// end, so the status has to be written before that event leaves.
#[tokio::test]
async fn the_turn_is_settled_before_it_is_announced() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));

    let status = Recorded::default();
    let log = status.calls.clone();
    finish_turn(Some(&status), "t", &failures, true, move || async move {
        log.borrow_mut().push("announced".to_string());
    })
    .await;

    assert_eq!(
        status.calls(),
        vec!["turn_ended(t, true)".to_string(), "announced".to_string()],
        "the status is settled first, then the turn's end is announced"
    );
}
