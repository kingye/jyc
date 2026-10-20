use super::tool_failures::ToolFailures;

#[test]
fn a_failed_tool_stands() {
    let mut failures = ToolFailures::default();
    failures.record("edit", true);
    assert!(
        failures.unrecovered().contains("edit"),
        "the tool stands in error"
    );
}

#[test]
fn a_retry_that_works_recovers_the_failure() {
    let mut failures = ToolFailures::default();
    failures.record("edit", true);
    failures.record("edit", false);
    assert!(
        failures.unrecovered().is_empty(),
        "the retry recovered the failure, so the turn has nothing to report"
    );
}

#[test]
fn another_tools_success_does_not_recover_a_failure() {
    let mut failures = ToolFailures::default();
    failures.record("edit", true);
    failures.record("bash", false);
    assert!(
        failures.unrecovered().contains("edit"),
        "bash working says nothing about edit"
    );
}

#[test]
fn each_tool_stands_on_its_own() {
    let mut failures = ToolFailures::default();
    failures.record("edit", true);
    failures.record("bash", true);
    failures.record("edit", false);
    assert!(failures.unrecovered().contains("bash"), "bash never worked");
    assert!(
        !failures.unrecovered().contains("edit"),
        "edit was retried successfully"
    );
}
