use std::cell::RefCell;

use crate::tools::ToolOutput;

use super::tool_failures::{ToolFailures, TurnStatus, report_standing_failures, settle_turn};

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
    calls: RefCell<Vec<String>>,
}

impl TurnStatus for Recorded {
    fn tool_error(&self, topic: &str) {
        self.calls.borrow_mut().push(format!("tool_error({topic})"));
    }

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

#[test]
fn a_standing_failure_is_reported_before_the_turn_ends() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));

    let status = Recorded::default();
    settle_turn(Some(&status), "t", &failures, true);

    assert_eq!(
        status.calls(),
        vec![
            "tool_error(t)".to_string(),
            "turn_ended(t, true)".to_string()
        ],
        "the failure is reported first, then the turn ends"
    );
}

#[test]
fn the_failure_is_reported_even_when_the_turn_dies() {
    let mut failures = ToolFailures::default();
    failures.record("edit", &ToolOutput::error("oldString not found"));

    let status = Recorded::default();
    report_standing_failures(Some(&status), "t", &failures);

    assert_eq!(
        status.calls(),
        vec!["tool_error(t)".to_string()],
        "the report stands on its own when the turn's end is not this path's to write"
    );
}
