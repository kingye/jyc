use super::*;

fn tc(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
    }
}

#[test]
fn empty_string_args_detected() {
    assert!(all_tool_calls_empty(&[tc("1", "bash", "")]));
}

#[test]
fn empty_object_args_detected() {
    assert!(all_tool_calls_empty(&[tc("1", "bash", "{}")]));
}

#[test]
fn whitespace_only_args_detected() {
    assert!(all_tool_calls_empty(&[tc("1", "bash", "  ")]));
}

#[test]
fn non_empty_args_not_detected() {
    assert!(!all_tool_calls_empty(&[tc(
        "1",
        "bash",
        r#"{"command":"ls"}"#
    )]));
}

#[test]
fn mixed_args_not_all_empty() {
    let calls = [tc("1", "bash", ""), tc("2", "read", r#"{"file_path":"x"}"#)];
    assert!(!all_tool_calls_empty(&calls));
}

#[test]
fn empty_slice_not_detected() {
    assert!(!all_tool_calls_empty(&[]));
}
