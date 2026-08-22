//! `context_browse` — page through the agent's in-memory conversation
//! transcript (user/assistant text pairs), including turns that fell out of
//! the model's sliding context window.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Hard cap on pairs returned per call. The result is appended to the
/// transcript and resent on the next request, so an unbounded page would
/// blow the window; 50 pairs is plenty for any "what happened earlier"
/// question.
const MAX_PAGE: usize = 50;

/// Page through the in-memory conversation transcript.
pub struct ContextBrowseTool;

#[async_trait]
impl Tool for ContextBrowseTool {
    fn name(&self) -> &str {
        "context_browse"
    }

    fn description(&self) -> &str {
        "Browse the current conversation transcript — user/assistant text \
         pairs, including turns outside your sliding context window. Pair \
         numbers count from the oldest pair (1 = first). `offset` is the \
         number of pairs to skip from the newest end (0 = the most recent \
         pairs); `limit` is the max number of pairs to return (default 10, \
         capped at 50). Returns a page of oldest→newest pairs."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "offset": {
                    "type": "integer",
                    "description": "Pairs to skip from the newest end. 0 = most recent pairs.",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Max pairs to return (capped at 50).",
                    "default": 10
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let offset = input
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as usize;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(10)
            .clamp(1, MAX_PAGE as i64) as usize;

        let pairs = crate::session::extract_pairs(&ctx.raw_context);
        let total = pairs.len();
        // Page [start..end) in oldest→newest order, with `offset` counted
        // from the newest end. Saturating math clamps out-of-range offsets
        // to an empty page instead of panicking.
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);

        if start >= end {
            return Ok(ToolOutput::success(format!(
                "No pairs in range: transcript has {total} pairs, offset {offset}."
            )));
        }

        let mut out = format!(
            "=== Transcript pairs {}-{} of {} (oldest→newest, offset {offset}) ===\n",
            start + 1,
            end,
            total
        );
        for (i, pair) in pairs[start..end].iter().enumerate() {
            let n = start + i + 1;
            out.push_str(&format!(
                "\n[{n}] USER: {}\n",
                crate::session::extract_message_text(&pair.user),
            ));
            if let Some(assistant) = &pair.assistant {
                out.push_str(&format!(
                    "[{n}] ASSISTANT: {}\n",
                    crate::session::extract_message_text(assistant),
                ));
            }
            if let Some(note) = &pair.note {
                out.push_str(&format!(
                    "[{n}] TOOLS: {}\n",
                    crate::session::extract_message_text(note),
                ));
            }
        }
        Ok(ToolOutput::success(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// Build a transcript of `n` user+assistant pairs (u1/a1 … un/an).
    fn pairs(n: usize) -> Vec<Value> {
        let mut v = Vec::new();
        for i in 1..=n {
            v.push(json!({"role": "user", "content": format!("u{i}")}));
            v.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        v
    }

    /// A ToolContext with the given raw_context snapshot, borrowing the
    /// test's tempdir as the working dir (dropped when the test ends, so
    /// nothing leaks). `context_browse` never touches the working dir.
    fn tool_ctx(working_dir: &Path, raw: Vec<Value>) -> ToolContext<'_> {
        let mut ctx = ToolContext::new(working_dir);
        ctx.raw_context = raw;
        ctx
    }

    #[tokio::test]
    async fn offset_zero_shows_newest_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(
                json!({"offset": 0, "limit": 10}),
                &tool_ctx(dir.path(), pairs(25)),
            )
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 16-25 of 25"), "header: {text}");
        assert!(text.contains("[16] USER: u16"), "first: {text}");
        assert!(text.contains("[25] ASSISTANT: a25"), "last: {text}");
        assert!(!text.contains("u15"), "older pair leaked: {text}");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn offset_steps_toward_older_pairs() {
        // 25 pairs, offset 10 → skip newest 10 (16–25) → show 6–15.
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"offset": 10}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 6-15 of 25"), "header: {text}");
        assert!(text.contains("[6] USER: u6"), "text: {text}");
        assert!(text.contains("[15] ASSISTANT: a15"), "text: {text}");
    }

    #[tokio::test]
    async fn offset_beyond_total_returns_empty_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"offset": 30}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        assert!(
            out.content
                .contains("No pairs in range: transcript has 25 pairs, offset 30."),
            "{}",
            out.content
        );
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn empty_transcript_returns_empty_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), Vec::new()))
            .await
            .unwrap();
        assert!(out.content.contains("0 pairs"), "{}", out.content);
    }

    /// Default limit is 10; an oversized limit is capped (50) rather than
    /// echoing an unbounded page back into the transcript.
    #[tokio::test]
    async fn limit_defaults_and_clamps() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        // Defaults: limit 10 → newest 10 of 25 = pairs 16–25.
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        assert!(out.content.contains("pairs 16-25 of 25"), "{}", out.content);
        // Huge limit is capped at 50, which covers all 25.
        let out = tool
            .execute(
                json!({"offset": 0, "limit": 999}),
                &tool_ctx(dir.path(), pairs(25)),
            )
            .await
            .unwrap();
        assert!(out.content.contains("pairs 1-25 of 25"), "{}", out.content);
    }

    /// Bare tool calls an assistant issued are summarized in the pair's
    /// history note, matching the windowed-view rendering.
    #[tokio::test]
    async fn renders_tool_call_annotation() {
        let raw = vec![
            json!({"role": "user", "content": "u"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}}
            ]}),
        ];
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        assert!(
            out.content.contains("[1] ASSISTANT: running\n"),
            "{}",
            out.content
        );
        assert!(
            out.content
                .contains(r#"[1] TOOLS: [History note] assistant tool calls: bash(command="ls")"#),
            "{}",
            out.content
        );
    }

    /// The built-in registry exposes context_browse.
    #[test]
    fn registered_in_builtin_registry() {
        let registry = crate::tools::builtin::create_builtin_registry();
        assert!(registry.has_tool("context_browse"));
    }
}
